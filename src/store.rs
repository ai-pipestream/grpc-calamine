// SPDX-License-Identifier: Apache-2.0

//! In-memory workbook store.
//!
//! Uploaded workbooks live only in process memory: the raw bytes are held
//! once per workbook in an `Arc<[u8]>` and parsed with calamine's
//! `open_workbook_*_from_rs` family. Nothing is ever written to disk, by
//! design.
//!
//! Concurrency model: an entry is immutable once opened. Every read
//! request works from its own calamine reader over a cheap `Cursor` clone of
//! the shared bytes, so reads of the same workbook run fully in parallel:
//! there is no per-workbook lock held while parsing.
//!
//! Readers are pooled rather than rebuilt from scratch every time. Opening one
//! is not cheap (calamine parses the zip directory, the shared-string table
//! and the workbook structure up front, measured at ~400 ms for a 105 MB
//! workbook), and that cost was previously paid on every read request and then
//! thrown away, including for the reader built at open time just to snapshot
//! metadata. A checked-out reader is returned to a small free list on drop and
//! reused, which is sound because calamine's readers seek per call: the same
//! reader yields identical cells across repeated and interleaved sheet reads.
//! The pool is bounded because each pooled reader retains its own
//! shared-string table; past the cap, readers are dropped instead of kept, and
//! a read that finds the list empty simply opens a fresh reader as before. The
//! lock is only ever held to pop or push, never while parsing.
//!
//! The store is bounded. Nothing guarantees a client closes what it opens: a
//! process killed between `OpenWorkbook` and `CloseWorkbook`, a partition, or
//! a close that times out each leave a handle behind, and every one holds an
//! upload of up to 512 MiB plus a parsed reader. So [`StoreLimits`] caps how
//! many workbooks are open and how many bytes they hold, refusing an open past
//! either, and closes a workbook nobody has used for the idle TTL. A workbook
//! an RPC is still reading is never idle, however long the read takes.
//!
//! What calamine inflates while opening a workbook is bounded too, before
//! calamine is asked to: see [`crate::archive`]. What that leaves each reader
//! holding (the pictures and the parsed shared-string table) is charged to
//! the same byte budget: once in the workbook's own charge, for the reader it
//! keeps parked, and again for every further reader while it lives.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use calamine::{
    HeaderRow, Ods, Reader, Sheets, Xls, Xlsb, Xlsx, open_workbook_auto_from_rs,
    open_workbook_from_rs,
};

use crate::archive::{self, InflateLimits};
use crate::convert;
use crate::proto::v1 as pb;

/// Shared, reference-counted workbook bytes.
pub type WorkbookBytes = Arc<[u8]>;

/// The reader type every request works with.
pub type WorkbookReader = Sheets<Cursor<WorkbookBytes>>;

/// How many readers one workbook keeps parked for reuse.
///
/// One, chosen from measurement rather than taste. Parking a single reader
/// already captures the whole win on a 105 MB text-heavy workbook: sequential
/// reads drop from ~2.6 s to ~2.15 s and time-to-first-row from ~400 ms to
/// ~0.2 ms, because the open is no longer redone per request. A larger cap only
/// helps *concurrent* readers of the same workbook, and aggregate throughput at
/// 4 and 16 concurrent streams did not move outside run-to-run noise when the
/// cap was raised to 4. Concurrency is bounded by memory rather than by this
/// cap: each in-flight reader materializes its own shared-string table, which
/// is what drives peak RSS (16 concurrent streams pushed it past 2 GiB at
/// either setting), so parking more readers buys throughput that the machine
/// cannot spend. Readers beyond the parked one open their own, as before.
const MAX_POOLED_READERS: usize = 1;

/// A reader and what it is charged against the store's byte budget.
///
/// `None` is the workbook's first reader, which the workbook's own charge
/// covers; every reader opened after it carries its own [`Charge`].
type ParkedReader = (WorkbookReader, Option<Charge>);

/// Free list of readers parked for reuse by one workbook.
#[derive(Default)]
struct ReaderPool {
    free: Mutex<Vec<ParkedReader>>,
}

impl ReaderPool {
    /// Take a parked reader, if one is available.
    fn take(&self) -> Option<ParkedReader> {
        self.free.lock().expect("reader pool lock poisoned").pop()
    }

    /// Park a reader for reuse, dropping it if the pool is already full.
    ///
    /// The first reader is never the one dropped: it takes the place of a
    /// charged one if it must. So it lives as long as the workbook, the
    /// workbook's own charge always has a reader to cover, and every other
    /// live reader carries a charge of its own.
    fn park(&self, reader: ParkedReader) {
        let mut free = self.free.lock().expect("reader pool lock poisoned");
        if free.len() < MAX_POOLED_READERS {
            free.push(reader);
        } else if reader.1.is_none()
            && let Some(charged) = free.iter_mut().find(|parked| parked.1.is_some())
        {
            *charged = reader;
        }
    }
}

/// The byte budget the open workbooks and their extra readers share.
#[derive(Debug)]
struct Budget {
    held: Mutex<u64>,
    max: u64,
}

impl Budget {
    fn held(&self) -> u64 {
        *self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Take `bytes` if they fit, or return what is already held.
    fn take(&self, bytes: u64) -> Result<(), u64> {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        if held.saturating_add(bytes) > self.max {
            return Err(*held);
        }
        *held += bytes;
        Ok(())
    }

    fn give(&self, bytes: u64) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        *held = held.saturating_sub(bytes);
    }
}

/// Bytes a reader beyond a workbook's first holds against the budget, given
/// back when the reader is dropped.
struct Charge {
    budget: Arc<Budget>,
    bytes: u64,
}

impl Drop for Charge {
    fn drop(&mut self) {
        self.budget.give(self.bytes);
    }
}

/// A calamine reader borrowed from a workbook's pool.
///
/// Derefs to the underlying [`WorkbookReader`] and returns it to the pool when
/// dropped, so the next read of the same workbook skips the open cost.
pub struct PooledReader {
    /// Always `Some` until `Drop` takes it back out.
    reader: Option<ParkedReader>,
    pool: Arc<ReaderPool>,
}

impl std::ops::Deref for PooledReader {
    type Target = WorkbookReader;

    fn deref(&self) -> &Self::Target {
        &self.reader.as_ref().expect("reader taken only on drop").0
    }
}

impl std::ops::DerefMut for PooledReader {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.reader.as_mut().expect("reader taken only on drop").0
    }
}

impl Drop for PooledReader {
    fn drop(&mut self) {
        if let Some(reader) = self.reader.take() {
            self.pool.park(reader);
        }
    }
}

/// Default for [`StoreLimits::max_open_workbooks`].
const DEFAULT_MAX_OPEN_WORKBOOKS: usize = 256;

/// Default for [`StoreLimits::max_store_bytes`]: 2 GiB.
const DEFAULT_MAX_STORE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Default for [`StoreLimits::idle_ttl`].
const DEFAULT_IDLE_TTL: Duration = Duration::from_secs(300);

/// Bounds on what the store holds, and for how long.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreLimits {
    /// Most workbooks open at once. Opening one more is refused.
    pub max_open_workbooks: usize,
    /// Most bytes the open workbooks may hold together, counting each one's
    /// uploaded bytes and what one reader of it keeps (the pictures inflated
    /// from it and an estimate of its parsed shared-string table), plus that
    /// again for every further reader while it reads. Opening a workbook, or
    /// a further reader, that would pass it is refused.
    pub max_store_bytes: u64,
    /// How long a workbook may go unused before it is closed on its client's
    /// behalf. Zero keeps every workbook until it is closed.
    pub idle_ttl: Duration,
    /// What opening one workbook may inflate.
    pub inflate: InflateLimits,
}

impl Default for StoreLimits {
    fn default() -> Self {
        Self {
            max_open_workbooks: DEFAULT_MAX_OPEN_WORKBOOKS,
            max_store_bytes: DEFAULT_MAX_STORE_BYTES,
            idle_ttl: DEFAULT_IDLE_TTL,
            inflate: InflateLimits::default(),
        }
    }
}

/// The workbook formats a store accepts, configured as a comma-separated
/// list such as `xlsx,xlsb` (`GRPC_CALAMINE_FORMATS`).
///
/// A refused format is never handed to calamine at all, so it costs nothing
/// that opening it would: XLS and ODS are parsed into dense ranges while they
/// open, outside every byte limit, and a server that cannot afford that for
/// its clients refuses them here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormatSet(u8);

impl FormatSet {
    /// Every format calamine reads.
    pub const ALL: Self = Self(0b1111);

    /// The order calamine's `open_workbook_auto_from_rs` tries formats in.
    const DETECTION_ORDER: [pb::WorkbookFormat; 4] = [
        pb::WorkbookFormat::Xls,
        pb::WorkbookFormat::Xlsx,
        pb::WorkbookFormat::Xlsb,
        pb::WorkbookFormat::Ods,
    ];

    /// The order formats are listed in.
    const LISTED_ORDER: [pb::WorkbookFormat; 4] = [
        pb::WorkbookFormat::Xlsx,
        pb::WorkbookFormat::Xlsb,
        pb::WorkbookFormat::Xls,
        pb::WorkbookFormat::Ods,
    ];

    fn bit(format: pb::WorkbookFormat) -> u8 {
        match format {
            pb::WorkbookFormat::Unspecified => 0,
            pb::WorkbookFormat::Xls => 1,
            pb::WorkbookFormat::Xlsx => 2,
            pb::WorkbookFormat::Xlsb => 4,
            pb::WorkbookFormat::Ods => 8,
        }
    }

    /// The name a format is configured by.
    fn name(format: pb::WorkbookFormat) -> &'static str {
        match format {
            pb::WorkbookFormat::Unspecified => "unspecified",
            pb::WorkbookFormat::Xls => "xls",
            pb::WorkbookFormat::Xlsx => "xlsx",
            pb::WorkbookFormat::Xlsb => "xlsb",
            pb::WorkbookFormat::Ods => "ods",
        }
    }

    /// Whether workbooks of `format` are accepted.
    #[must_use]
    pub fn accepts(self, format: pb::WorkbookFormat) -> bool {
        self.0 & Self::bit(format) != 0
    }
}

impl std::str::FromStr for FormatSet {
    type Err = String;

    /// Parse a comma-separated list of `xlsx`, `xlsb`, `xls` and `ods`, in
    /// any case and order. A list naming none of them is an error: a server
    /// that accepts no workbook is a mistake in its configuration.
    fn from_str(list: &str) -> Result<Self, Self::Err> {
        let mut set = 0u8;
        for word in list.split(',').map(str::trim).filter(|w| !w.is_empty()) {
            let format = Self::LISTED_ORDER
                .into_iter()
                .find(|format| Self::name(*format).eq_ignore_ascii_case(word))
                .ok_or_else(|| {
                    format!("unknown format {word:?}; list any of xlsx, xlsb, xls and ods")
                })?;
            set |= Self::bit(format);
        }
        if set == 0 {
            return Err("no format is listed; list any of xlsx, xlsb, xls and ods".to_string());
        }
        Ok(Self(set))
    }
}

impl std::fmt::Display for FormatSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = Self::LISTED_ORDER
            .into_iter()
            .filter(|format| self.accepts(*format))
            .map(Self::name)
            .collect();
        f.write_str(&names.join(","))
    }
}

/// A configured limit that opening a workbook would pass.
///
/// A refusal, not a fault in the workbook: the same bytes open once there is
/// room, or on a server configured with more of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LimitExceeded {
    /// As many workbooks as the store holds are already open.
    OpenWorkbooks {
        /// [`StoreLimits::max_open_workbooks`].
        max: usize,
    },
    /// A further reader of a workbook does not fit beside what the open
    /// workbooks and their readers already hold.
    ReaderBytes {
        /// What the reader would hold.
        needed: u64,
        /// What is already held.
        held: u64,
        /// [`StoreLimits::max_store_bytes`].
        max: u64,
    },
    /// The open workbooks hold too much for this one to fit beside them.
    StoreBytes {
        /// What this workbook would hold.
        needed: u64,
        /// What the open workbooks already hold.
        held: u64,
        /// [`StoreLimits::max_store_bytes`].
        max: u64,
    },
    /// One embedded picture inflates past the per-picture limit.
    Picture {
        /// The picture's entry in the package.
        name: String,
        /// [`InflateLimits::max_picture_bytes`].
        max: u64,
    },
    /// The embedded pictures together inflate past the per-workbook limit.
    PictureTotal {
        /// [`InflateLimits::max_picture_total_bytes`].
        max: u64,
    },
    /// The shared-string table inflates past its limit.
    SharedStrings {
        /// The table's entry in the package.
        name: String,
        /// [`InflateLimits::max_shared_strings_bytes`].
        max: u64,
    },
}

impl std::fmt::Display for LimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenWorkbooks { max } => write!(
                f,
                "{max} workbooks are already open, the most this server holds at once; \
                 close handles that are no longer needed, retry shortly, or raise \
                 GRPC_CALAMINE_MAX_OPEN_WORKBOOKS"
            ),
            Self::StoreBytes { needed, held, max } => write!(
                f,
                "this workbook needs {needed} bytes and open workbooks already hold \
                 {held} of the {max} this server keeps; close handles that are no \
                 longer needed, retry shortly, or raise GRPC_CALAMINE_MAX_STORE_BYTES"
            ),
            Self::ReaderBytes { needed, held, max } => write!(
                f,
                "another read of this workbook already holds its reader, and a second \
                 one needs {needed} bytes for its pictures and shared strings while \
                 open workbooks and their readers already hold {held} of the {max} \
                 this server keeps; retry once the other read ends, or raise \
                 GRPC_CALAMINE_MAX_STORE_BYTES"
            ),
            Self::Picture { name, max } => write!(
                f,
                "embedded picture {name:?} inflates past {max} bytes, the most one \
                 picture may (GRPC_CALAMINE_MAX_PICTURE_BYTES)"
            ),
            Self::PictureTotal { max } => write!(
                f,
                "the embedded pictures inflate past {max} bytes together, the most \
                 one workbook's pictures may (GRPC_CALAMINE_MAX_PICTURE_TOTAL_BYTES)"
            ),
            Self::SharedStrings { name, max } => write!(
                f,
                "shared-string table {name:?} inflates past {max} bytes, the most \
                 this server reads (GRPC_CALAMINE_MAX_SHARED_STRINGS_BYTES)"
            ),
        }
    }
}

impl std::error::Error for LimitExceeded {}

/// A workbook opened for reading: shared bytes plus an open-time snapshot.
pub struct WorkbookEntry {
    /// The raw uploaded bytes, shared by every reader of this workbook.
    pub bytes: WorkbookBytes,
    /// The format the workbook was opened as.
    pub format: pb::WorkbookFormat,
    /// Header row selection applied to every reader built from this entry.
    pub header_row: Option<HeaderRow>,
    /// Metadata snapshot taken at open time (sheets and defined names).
    pub metadata: pb::Metadata,
    /// Workbook-level 1904 date-system flag, read once at open time via
    /// `has_1904_epoch` and stamped onto every streamed datetime cell.
    pub is_1904: bool,
    /// Readers parked for reuse. Never held across a parse.
    pool: Arc<ReaderPool>,
    /// What this workbook counts against [`StoreLimits::max_store_bytes`].
    held_bytes: u64,
    /// What each reader beyond the first is charged: its pictures and its
    /// parsed shared-string table.
    reader_bytes: u64,
    /// The store's byte budget, which further readers are charged against.
    budget: Arc<Budget>,
    /// When an RPC last used this workbook, for the idle TTL.
    last_used: Mutex<Instant>,
}

impl WorkbookEntry {
    /// Record a use at `now`.
    fn touch(&self, now: Instant) {
        *self
            .last_used
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = now;
    }

    /// When an RPC last used this workbook.
    fn last_used(&self) -> Instant {
        *self
            .last_used
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Borrow a calamine reader over the shared bytes, reusing a parked one
    /// when the pool has it and opening a fresh one otherwise.
    ///
    /// This is blocking CPU work; callers must run it inside
    /// `tokio::task::spawn_blocking`.
    ///
    /// A reader opened because the parked one is busy holds its own pictures
    /// and shared-string table, so it is charged against the store's byte
    /// budget until it is dropped.
    ///
    /// # Errors
    ///
    /// [`ReaderError::Limit`] when a further reader does not fit in the
    /// store's byte budget, and [`ReaderError::Open`] when calamine cannot
    /// re-open the bytes in the format recorded at open time.
    pub fn reader(&self) -> Result<PooledReader, ReaderError> {
        let (mut workbook, charge) = match self.pool.take() {
            Some(parked) => parked,
            None => {
                self.budget
                    .take(self.reader_bytes)
                    .map_err(|held| LimitExceeded::ReaderBytes {
                        needed: self.reader_bytes,
                        held,
                        max: self.budget.max,
                    })?;
                let charge = Charge {
                    budget: Arc::clone(&self.budget),
                    bytes: self.reader_bytes,
                };
                (
                    open_as(Cursor::new(Arc::clone(&self.bytes)), self.format)?,
                    Some(charge),
                )
            }
        };
        // Re-applied on every checkout: a parked reader carries whatever the
        // previous borrower set.
        if let Some(header_row) = self.header_row {
            workbook.with_header_row(header_row);
        }
        Ok(PooledReader {
            reader: Some((workbook, charge)),
            pool: Arc::clone(&self.pool),
        })
    }
}

/// Why [`WorkbookEntry::reader`] did not give a reader.
#[derive(Debug)]
pub enum ReaderError {
    /// calamine cannot re-open the bytes.
    Open(OpenError),
    /// A further reader does not fit in the store's byte budget.
    Limit(LimitExceeded),
}

impl std::fmt::Display for ReaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open(e) => e.fmt(f),
            Self::Limit(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ReaderError {}

impl From<OpenError> for ReaderError {
    fn from(e: OpenError) -> Self {
        Self::Open(e)
    }
}

impl From<LimitExceeded> for ReaderError {
    fn from(e: LimitExceeded) -> Self {
        Self::Limit(e)
    }
}

/// Open a cursor as the given format, auto-detecting when `Unspecified`.
fn open_as(
    cursor: Cursor<WorkbookBytes>,
    format: pb::WorkbookFormat,
) -> Result<WorkbookReader, OpenError> {
    match format {
        pb::WorkbookFormat::Unspecified => {
            open_workbook_auto_from_rs(cursor).map_err(|e| OpenError {
                error: convert::calamine_error(convert::error_kind(&e), &e),
            })
        }
        pb::WorkbookFormat::Xls => open_workbook_from_rs::<Xls<_>, _>(cursor)
            .map(Sheets::Xls)
            .map_err(|e| OpenError {
                error: convert::calamine_error(pb::CalamineErrorKind::Xls, &e),
            }),
        pb::WorkbookFormat::Xlsx => open_workbook_from_rs::<Xlsx<_>, _>(cursor)
            .map(Sheets::Xlsx)
            .map_err(|e| OpenError {
                error: convert::calamine_error(pb::CalamineErrorKind::Xlsx, &e),
            }),
        pb::WorkbookFormat::Xlsb => open_workbook_from_rs::<Xlsb<_>, _>(cursor)
            .map(Sheets::Xlsb)
            .map_err(|e| OpenError {
                error: convert::calamine_error(pb::CalamineErrorKind::Xlsb, &e),
            }),
        pb::WorkbookFormat::Ods => open_workbook_from_rs::<Ods<_>, _>(cursor)
            .map(Sheets::Ods)
            .map_err(|e| OpenError {
                error: convert::calamine_error(pb::CalamineErrorKind::Ods, &e),
            }),
    }
}

/// Thread-safe registry of open workbooks, keyed by workbook id.
///
/// The lock is only held long enough to clone or remove an `Arc`, or to check
/// and charge the limits; it is never held while parsing, so it never
/// throttles read concurrency.
pub struct WorkbookStore {
    inner: RwLock<Registry>,
    /// Bytes held by the open workbooks, by opens admitted but not yet
    /// registered, and by every reader beyond a workbook's first.
    budget: Arc<Budget>,
    limits: StoreLimits,
    formats: FormatSet,
}

/// The workbooks, behind the store's lock.
#[derive(Default)]
struct Registry {
    entries: HashMap<String, Arc<WorkbookEntry>>,
    /// Opens admitted but not yet registered. They count against the
    /// workbook cap too, so concurrent opens cannot overshoot it together.
    pending: usize,
}

/// Room taken for a workbook that is still being parsed.
///
/// Dropped without [`Reservation::register`] (the parse failed or panicked),
/// it gives the room back.
struct Reservation<'a> {
    store: &'a WorkbookStore,
    bytes: u64,
    registered: bool,
}

impl Reservation<'_> {
    /// Register the parsed workbook under a fresh id.
    fn register(mut self, entry: Arc<WorkbookEntry>) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let mut registry = self.store.write();
        registry.pending -= 1;
        registry.entries.insert(id.clone(), entry);
        self.registered = true;
        id
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if self.registered {
            return;
        }
        // Reached while unwinding from a parser panic too, so a poisoned lock
        // is recovered rather than turned into a second panic.
        let mut registry = self
            .store
            .inner
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        registry.pending -= 1;
        self.store.budget.give(self.bytes);
    }
}

/// Why [`WorkbookStore::open`] did not open a workbook.
#[derive(Debug)]
pub enum StoreError {
    /// calamine cannot read the bytes as a workbook.
    Unreadable(OpenError),
    /// Opening the workbook would pass a configured limit.
    Limit(LimitExceeded),
    /// The package is internally inconsistent in a way the pre-open scan
    /// refuses (see [`archive::Rejected::Malformed`]).
    Malformed {
        /// The entry at fault.
        part: String,
        /// What is wrong with it.
        detail: String,
    },
    /// The client asked for a format this server does not accept.
    FormatRefused {
        /// The format asked for.
        format: pb::WorkbookFormat,
        /// The formats this server accepts.
        accepted: FormatSet,
    },
    /// Auto-detection found no format this server accepts that the bytes
    /// open as. They may be a workbook of a format it refuses.
    NoAcceptedFormat {
        /// The formats this server accepts.
        accepted: FormatSet,
    },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(e) => e.fmt(f),
            Self::Limit(e) => e.fmt(f),
            Self::Malformed { part, detail } => write!(f, "{part}: {detail}"),
            Self::FormatRefused { format, accepted } => write!(
                f,
                "this server does not accept {} workbooks; it accepts {accepted} \
                 (GRPC_CALAMINE_FORMATS)",
                FormatSet::name(*format)
            ),
            Self::NoAcceptedFormat { accepted } => write!(
                f,
                "the upload does not open as any format this server accepts \
                 ({accepted}; GRPC_CALAMINE_FORMATS)"
            ),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<OpenError> for StoreError {
    fn from(e: OpenError) -> Self {
        Self::Unreadable(e)
    }
}

impl From<LimitExceeded> for StoreError {
    fn from(e: LimitExceeded) -> Self {
        Self::Limit(e)
    }
}

impl From<archive::Rejected> for StoreError {
    fn from(e: archive::Rejected) -> Self {
        match e {
            archive::Rejected::Limit(limit) => Self::Limit(limit),
            archive::Rejected::Malformed { part, detail } => Self::Malformed { part, detail },
        }
    }
}

impl Default for WorkbookStore {
    fn default() -> Self {
        Self::with_limits(StoreLimits::default())
    }
}

/// Error returned when a workbook cannot be opened.
#[derive(Debug)]
pub struct OpenError {
    /// Structured error for the contract's `CalamineError`.
    pub error: pb::CalamineError,
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error.message)
    }
}

impl std::error::Error for OpenError {}

impl WorkbookStore {
    /// Create an empty store with the default limits.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an empty store bounded by `limits`.
    #[must_use]
    pub fn with_limits(limits: StoreLimits) -> Self {
        Self {
            inner: RwLock::new(Registry::default()),
            budget: Arc::new(Budget {
                held: Mutex::new(0),
                max: limits.max_store_bytes,
            }),
            limits,
            formats: FormatSet::ALL,
        }
    }

    /// Accept only workbooks of `formats`. Every format is accepted unless
    /// this narrows it.
    #[must_use]
    pub fn with_formats(mut self, formats: FormatSet) -> Self {
        self.formats = formats;
        self
    }

    fn read(&self) -> RwLockReadGuard<'_, Registry> {
        self.inner.read().expect("workbook store lock poisoned")
    }

    fn write(&self) -> RwLockWriteGuard<'_, Registry> {
        self.inner.write().expect("workbook store lock poisoned")
    }

    /// Check that a workbook of `bytes` would fit beside the open ones.
    fn fits(&self, registry: &Registry, bytes: u64) -> Result<(), LimitExceeded> {
        if registry.entries.len() + registry.pending >= self.limits.max_open_workbooks {
            return Err(LimitExceeded::OpenWorkbooks {
                max: self.limits.max_open_workbooks,
            });
        }
        let held = self.budget.held();
        if held.saturating_add(bytes) > self.limits.max_store_bytes {
            return Err(self.store_bytes(bytes, held));
        }
        Ok(())
    }

    fn store_bytes(&self, needed: u64, held: u64) -> LimitExceeded {
        LimitExceeded::StoreBytes {
            needed,
            held,
            max: self.limits.max_store_bytes,
        }
    }

    /// Whether a workbook of `bytes` could be opened now.
    ///
    /// Cheap enough to ask once per upload chunk, so an upload that can never
    /// be admitted is refused before it is buffered rather than after. Only a
    /// refusal pays for closing idle workbooks first, which may be all that
    /// stands in the way. [`WorkbookStore::open`] checks again, atomically.
    ///
    /// # Errors
    ///
    /// The limit `bytes` would pass.
    ///
    /// # Panics
    ///
    /// Panics if the store lock was poisoned by a panic on another thread.
    pub fn admits(&self, bytes: u64) -> Result<(), LimitExceeded> {
        if self.fits(&self.read(), bytes).is_ok() {
            return Ok(());
        }
        let mut registry = self.write();
        self.sweep(&mut registry, Instant::now());
        self.fits(&registry, bytes)
    }

    /// Take room for a workbook of `bytes`, closing idle workbooks first.
    fn reserve(&self, bytes: u64) -> Result<Reservation<'_>, LimitExceeded> {
        let mut registry = self.write();
        self.sweep(&mut registry, Instant::now());
        self.fits(&registry, bytes)?;
        // Readers beyond a workbook's first take from the budget without the
        // registry lock, so the bytes are taken, not just checked.
        self.budget
            .take(bytes)
            .map_err(|held| self.store_bytes(bytes, held))?;
        registry.pending += 1;
        Ok(Reservation {
            store: self,
            bytes,
            registered: false,
        })
    }

    /// Parse `bytes` as a workbook, register it, and return its id and entry.
    ///
    /// This is blocking CPU work; callers must run it inside
    /// `tokio::task::spawn_blocking`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Limit`] when the workbook would inflate too much or the
    /// store has no room for it, both checked before calamine parses
    /// anything, [`StoreError::FormatRefused`] or
    /// [`StoreError::NoAcceptedFormat`] when the format is not one this store
    /// accepts, and [`StoreError::Unreadable`] when the bytes cannot be
    /// parsed as a workbook (or as the specific format given by
    /// `format_hint`).
    ///
    /// # Panics
    ///
    /// Panics if the store lock was poisoned by a panic on another thread.
    pub fn open(
        &self,
        bytes: Vec<u8>,
        format_hint: pb::WorkbookFormat,
        header_row: Option<HeaderRow>,
    ) -> Result<(String, Arc<WorkbookEntry>), StoreError> {
        // A refused format is refused before anything is parsed: what calamine
        // builds while opening some formats is the reason to refuse them.
        if format_hint != pb::WorkbookFormat::Unspecified && !self.formats.accepts(format_hint) {
            return Err(StoreError::FormatRefused {
                format: format_hint,
                accepted: self.formats,
            });
        }

        // What calamine would inflate is measured before calamine is allowed
        // to, and what every reader keeps is charged to the workbook: once
        // here for the reader it parks, and again by each further reader.
        let inflated = archive::inspect(&bytes, &self.limits.inflate)?;
        let reader_bytes = inflated.reader_bytes();
        let held_bytes = (bytes.len() as u64).saturating_add(reader_bytes);
        let reservation = self.reserve(held_bytes)?;
        let bytes: WorkbookBytes = bytes.into();

        // One probing reader to detect the format and snapshot metadata.
        let probe =
            if format_hint == pb::WorkbookFormat::Unspecified && self.formats != FormatSet::ALL {
                self.detect_accepted(&bytes)?
            } else {
                open_as(Cursor::new(Arc::clone(&bytes)), format_hint)?
            };
        let format = match &probe {
            Sheets::Xls(_) => pb::WorkbookFormat::Xls,
            Sheets::Xlsx(_) => pb::WorkbookFormat::Xlsx,
            Sheets::Xlsb(_) => pb::WorkbookFormat::Xlsb,
            Sheets::Ods(_) => pb::WorkbookFormat::Ods,
        };
        let metadata = pb::Metadata {
            sheets: probe.sheets_metadata().iter().map(convert::sheet).collect(),
            defined_names: probe
                .defined_names()
                .iter()
                .map(|(name, definition)| pb::DefinedName {
                    name: name.clone(),
                    definition: definition.clone(),
                })
                .collect(),
        };
        let is_1904 = convert::has_1904_epoch(&probe);

        // The probe is a fully parsed reader. Park it instead of dropping it,
        // so the first read of this workbook does not repeat the open.
        let pool = Arc::new(ReaderPool::default());
        pool.park((probe, None));

        let entry = Arc::new(WorkbookEntry {
            bytes,
            format,
            header_row,
            metadata,
            is_1904,
            pool,
            held_bytes,
            reader_bytes,
            budget: Arc::clone(&self.budget),
            last_used: Mutex::new(Instant::now()),
        });

        let id = reservation.register(Arc::clone(&entry));
        Ok((id, entry))
    }

    /// Open `bytes` as the first format this store accepts that they open
    /// as, trying them in calamine's own auto-detection order and never
    /// trying a refused one.
    fn detect_accepted(&self, bytes: &WorkbookBytes) -> Result<WorkbookReader, StoreError> {
        FormatSet::DETECTION_ORDER
            .into_iter()
            .filter(|format| self.formats.accepts(*format))
            .find_map(|format| open_as(Cursor::new(Arc::clone(bytes)), format).ok())
            .ok_or(StoreError::NoAcceptedFormat {
                accepted: self.formats,
            })
    }

    /// Look up an open workbook by id, recording the use.
    ///
    /// # Panics
    ///
    /// Panics if the store lock was poisoned by a panic on another thread.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<WorkbookEntry>> {
        let entry = self.read().entries.get(id).cloned()?;
        entry.touch(Instant::now());
        Some(entry)
    }

    /// Remove a workbook from the store. Returns true if it existed.
    ///
    /// An RPC already reading the workbook finishes; its memory goes when the
    /// last reader does.
    ///
    /// # Panics
    ///
    /// Panics if the store lock was poisoned by a panic on another thread.
    pub fn close(&self, id: &str) -> bool {
        let mut registry = self.write();
        let Some(entry) = registry.entries.remove(id) else {
            return false;
        };
        self.budget.give(entry.held_bytes);
        true
    }

    /// Close every workbook unused for at least the idle TTL, as of now.
    /// Returns how many were closed.
    ///
    /// # Panics
    ///
    /// Panics if the store lock was poisoned by a panic on another thread.
    pub fn evict_idle(&self) -> usize {
        self.sweep(&mut self.write(), Instant::now())
    }

    /// Close every workbook unused for at least the idle TTL as of `now`.
    ///
    /// A workbook some RPC still holds is in use whatever its timestamp says,
    /// so a read that outlasts the TTL keeps its handle. Its clock restarts at
    /// every sweep that finds it busy, so it goes idle about when the read
    /// ends rather than when it began.
    fn sweep(&self, registry: &mut Registry, now: Instant) -> usize {
        let ttl = self.limits.idle_ttl;
        if ttl.is_zero() {
            return 0;
        }
        let before = registry.entries.len();
        let mut freed = 0u64;
        registry.entries.retain(|_, entry| {
            if Arc::strong_count(entry) > 1 {
                entry.touch(now);
                return true;
            }
            if now.saturating_duration_since(entry.last_used()) < ttl {
                return true;
            }
            freed += entry.held_bytes;
            false
        });
        self.budget.give(freed);
        before - registry.entries.len()
    }

    /// Start the task that closes idle workbooks, on the current tokio
    /// runtime. `None` when the idle TTL is zero and nothing ever expires.
    ///
    /// Without it, idle workbooks are still closed whenever an open needs
    /// their room, but one nobody needs room for stays until then. The task
    /// holds the store weakly and ends once the store is dropped.
    ///
    /// # Panics
    ///
    /// Panics when called outside a tokio runtime.
    pub fn spawn_reaper(store: &Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        let ttl = store.limits.idle_ttl;
        if ttl.is_zero() {
            return None;
        }
        // A workbook outlives its TTL by at most one period.
        let period = (ttl / 4).clamp(Duration::from_millis(50), Duration::from_secs(30));
        let store = Arc::downgrade(store);
        Some(tokio::spawn(async move {
            let mut ticks = tokio::time::interval(period);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticks.tick().await;
                let Some(store) = store.upgrade() else { break };
                let closed = store.evict_idle();
                if closed > 0 {
                    eprintln!(
                        "grpc-calamine: closed {closed} workbook(s) unused for {ttl:?}; \
                         {} open, holding {} bytes",
                        store.len(),
                        store.held_bytes()
                    );
                }
            }
        }))
    }

    /// Bytes the open workbooks and their further readers hold, as charged
    /// against [`StoreLimits::max_store_bytes`].
    #[must_use]
    pub fn held_bytes(&self) -> u64 {
        self.budget.held()
    }

    /// Number of currently open workbooks.
    ///
    /// # Panics
    ///
    /// Panics if the store lock was poisoned by a panic on another thread.
    #[must_use]
    pub fn len(&self) -> usize {
        self.read().entries.len()
    }

    /// Whether the store currently holds no workbooks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("demos/sample-data")
            .join(name);
        std::fs::read(path).expect("read fixture")
    }

    fn open(store: &WorkbookStore, name: &str) -> Result<(String, Arc<WorkbookEntry>), StoreError> {
        store.open(fixture(name), pb::WorkbookFormat::Unspecified, None)
    }

    fn limits(max_open_workbooks: usize, max_store_bytes: u64, idle_ttl: Duration) -> StoreLimits {
        StoreLimits {
            max_open_workbooks,
            max_store_bytes,
            idle_ttl,
            inflate: InflateLimits::default(),
        }
    }

    const TTL: Duration = Duration::from_secs(60);

    /// A workbook nobody uses for the TTL is closed and its bytes released;
    /// one used just inside it stays.
    #[test]
    fn idle_workbooks_are_closed_after_the_ttl() {
        let store = WorkbookStore::with_limits(limits(8, u64::MAX, TTL));
        let (id, entry) = open(&store, "date.xlsx").expect("open");
        let opened_at = entry.last_used();
        drop(entry);

        let almost = opened_at + TTL - Duration::from_millis(1);
        assert_eq!(store.sweep(&mut store.write(), almost), 0);
        assert!(store.held_bytes() > 0);

        assert_eq!(store.sweep(&mut store.write(), opened_at + TTL), 1);
        assert!(store.get(&id).is_none(), "an expired handle is gone");
        assert_eq!(store.held_bytes(), 0, "and so are its bytes");
    }

    /// A workbook an RPC still holds is in use however old its last lookup
    /// is, and its idle clock restarts from the last sweep that saw it busy.
    #[test]
    fn a_workbook_in_use_is_never_idle() {
        let store = WorkbookStore::with_limits(limits(8, u64::MAX, TTL));
        let (id, reading) = open(&store, "date.xlsx").expect("open");

        let much_later = reading.last_used() + TTL * 10;
        assert_eq!(store.sweep(&mut store.write(), much_later), 0);

        drop(reading);
        let half = much_later + TTL / 2;
        assert_eq!(
            store.sweep(&mut store.write(), half),
            0,
            "idle only since the read ended"
        );
        assert_eq!(store.sweep(&mut store.write(), much_later + TTL), 1);
        assert!(store.get(&id).is_none());
    }

    /// Every lookup is a use.
    #[test]
    fn a_lookup_restarts_the_idle_clock() {
        let store = WorkbookStore::with_limits(limits(8, u64::MAX, TTL));
        let (id, entry) = open(&store, "date.xlsx").expect("open");
        let opened_at = entry.last_used();
        std::thread::sleep(Duration::from_millis(5));
        let _ = store.get(&id).expect("open handle");
        assert!(entry.last_used() > opened_at);
    }

    /// A zero TTL keeps workbooks until they are closed.
    #[test]
    fn a_zero_ttl_never_expires() {
        let store = WorkbookStore::with_limits(limits(8, u64::MAX, Duration::ZERO));
        let (id, entry) = open(&store, "date.xlsx").expect("open");
        let far = entry.last_used() + Duration::from_secs(1_000_000);
        drop(entry);
        assert_eq!(store.sweep(&mut store.write(), far), 0);
        assert!(store.get(&id).is_some());
    }

    /// Past the workbook cap an open is refused, and closing one makes room.
    #[test]
    fn the_workbook_cap_refuses_the_next_open() {
        let store = WorkbookStore::with_limits(limits(1, u64::MAX, TTL));
        let (id, _) = open(&store, "date.xlsx").expect("first open");

        let refused = open(&store, "date.xlsx").err().expect("the cap is 1");
        assert!(matches!(
            refused,
            StoreError::Limit(LimitExceeded::OpenWorkbooks { max: 1 })
        ));

        assert!(store.close(&id));
        open(&store, "date.xlsx").expect("a closed handle's room is reusable");
    }

    /// A workbook that would take the open ones past the byte budget is
    /// refused, and the budget counts only what is open.
    #[test]
    fn the_byte_budget_refuses_a_workbook_that_does_not_fit() {
        let size = fixture("date.xlsx").len() as u64;
        let store = WorkbookStore::with_limits(limits(8, size + size / 2, TTL));
        let (id, _) = open(&store, "date.xlsx").expect("first open fits");
        assert_eq!(store.held_bytes(), size);

        let refused = open(&store, "date.xlsx").err().expect("two do not fit");
        assert!(matches!(
            refused,
            StoreError::Limit(LimitExceeded::StoreBytes { needed, held, .. })
                if needed == size && held == size
        ));
        assert!(store.admits(size).is_err(), "the early check agrees");

        assert!(store.close(&id));
        assert_eq!(store.held_bytes(), 0);
        open(&store, "date.xlsx").expect("fits again once closed");
    }

    /// An open that fails to parse gives back the room it reserved.
    #[test]
    fn a_failed_open_gives_its_room_back() {
        let store = WorkbookStore::with_limits(limits(1, u64::MAX, TTL));
        let refused = store
            .open(
                b"not a workbook".to_vec(),
                pb::WorkbookFormat::Unspecified,
                None,
            )
            .err()
            .expect("garbage does not parse");
        assert!(matches!(refused, StoreError::Unreadable(_)));
        assert_eq!(store.held_bytes(), 0);
        open(&store, "date.xlsx").expect("the only slot is free again");
    }

    /// A format list parses in any case and order, and an unknown or empty
    /// one is an error rather than a server that accepts nothing.
    #[test]
    fn format_lists_parse() {
        let set: FormatSet = " ODS, xlsx ".parse().expect("two formats");
        assert!(set.accepts(pb::WorkbookFormat::Ods));
        assert!(set.accepts(pb::WorkbookFormat::Xlsx));
        assert!(!set.accepts(pb::WorkbookFormat::Xls));
        assert!(!set.accepts(pb::WorkbookFormat::Unspecified));
        assert_eq!(set.to_string(), "xlsx,ods");
        assert_eq!("xlsx,xlsb,xls,ods".parse::<FormatSet>(), Ok(FormatSet::ALL));
        assert!("xlsx,csv".parse::<FormatSet>().is_err());
        assert!(" , ".parse::<FormatSet>().is_err());
    }

    /// A refused format is refused before anything is parsed or reserved.
    #[test]
    fn a_refused_format_holds_nothing() {
        let store = WorkbookStore::with_limits(limits(8, u64::MAX, TTL))
            .with_formats("xlsx".parse().expect("one format"));
        let named = store
            .open(fixture("date.ods"), pb::WorkbookFormat::Ods, None)
            .err()
            .expect("ods is refused");
        assert!(matches!(named, StoreError::FormatRefused { .. }));
        let detected = open(&store, "date.ods").err().expect("never detected");
        assert!(matches!(detected, StoreError::NoAcceptedFormat { .. }));
        assert_eq!(store.held_bytes(), 0);
        assert!(store.is_empty());
        open(&store, "date.xlsx").expect("xlsx is accepted");
    }

    /// Idle workbooks are closed to make room for an open that needs it, even
    /// with no reaper running.
    #[test]
    fn idle_workbooks_make_room_for_new_ones() {
        let store = WorkbookStore::with_limits(limits(1, u64::MAX, TTL));
        let (stale, entry) = open(&store, "date.xlsx").expect("first open");
        let Some(long_ago) = Instant::now().checked_sub(TTL * 2) else {
            return; // A clock this close to its epoch cannot express the case.
        };
        entry.touch(long_ago);
        drop(entry);

        assert!(
            store.admits(1).is_ok(),
            "the idle workbook is closed to admit"
        );
        open(&store, "date.xlsx").expect("second open takes its place");
        assert!(store.get(&stale).is_none());
    }
}
