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
//! calamine is asked to: see [`crate::archive`].

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

/// Free list of readers parked for reuse by one workbook.
#[derive(Default)]
struct ReaderPool {
    free: Mutex<Vec<WorkbookReader>>,
}

impl ReaderPool {
    /// Take a parked reader, if one is available.
    fn take(&self) -> Option<WorkbookReader> {
        self.free.lock().expect("reader pool lock poisoned").pop()
    }

    /// Park a reader for reuse, dropping it if the pool is already full.
    fn park(&self, reader: WorkbookReader) {
        let mut free = self.free.lock().expect("reader pool lock poisoned");
        if free.len() < MAX_POOLED_READERS {
            free.push(reader);
        }
    }
}

/// A calamine reader borrowed from a workbook's pool.
///
/// Derefs to the underlying [`WorkbookReader`] and returns it to the pool when
/// dropped, so the next read of the same workbook skips the open cost.
pub struct PooledReader {
    /// Always `Some` until `Drop` takes it back out.
    reader: Option<WorkbookReader>,
    pool: Arc<ReaderPool>,
}

impl std::ops::Deref for PooledReader {
    type Target = WorkbookReader;

    fn deref(&self) -> &Self::Target {
        self.reader.as_ref().expect("reader taken only on drop")
    }
}

impl std::ops::DerefMut for PooledReader {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.reader.as_mut().expect("reader taken only on drop")
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
    /// uploaded bytes and the pictures inflated from them, which its parked
    /// reader keeps. Opening a workbook that would pass it is refused.
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
    /// # Errors
    ///
    /// Returns [`OpenError`] when calamine cannot re-open the bytes in the
    /// format recorded at open time.
    pub fn reader(&self) -> Result<PooledReader, OpenError> {
        let mut workbook = match self.pool.take() {
            Some(parked) => parked,
            None => open_as(Cursor::new(Arc::clone(&self.bytes)), self.format)?,
        };
        // Re-applied on every checkout: a parked reader carries whatever the
        // previous borrower set.
        if let Some(header_row) = self.header_row {
            workbook.with_header_row(header_row);
        }
        Ok(PooledReader {
            reader: Some(workbook),
            pool: Arc::clone(&self.pool),
        })
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
    limits: StoreLimits,
}

/// The workbooks and what they are charged, behind the store's lock.
#[derive(Default)]
struct Registry {
    entries: HashMap<String, Arc<WorkbookEntry>>,
    /// Bytes held by `entries` and by opens admitted but not yet registered.
    held_bytes: u64,
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
        registry.held_bytes = registry.held_bytes.saturating_sub(self.bytes);
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
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(e) => e.fmt(f),
            Self::Limit(e) => e.fmt(f),
            Self::Malformed { part, detail } => write!(f, "{part}: {detail}"),
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
            limits,
        }
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
        if registry.held_bytes.saturating_add(bytes) > self.limits.max_store_bytes {
            return Err(LimitExceeded::StoreBytes {
                needed: bytes,
                held: registry.held_bytes,
                max: self.limits.max_store_bytes,
            });
        }
        Ok(())
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
        registry.pending += 1;
        registry.held_bytes += bytes;
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
    /// anything, and [`StoreError::Unreadable`] when the bytes cannot be
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
        // What calamine would inflate is measured before calamine is allowed
        // to, and the pictures every reader keeps are charged to the workbook.
        let inflated = archive::inspect(&bytes, &self.limits.inflate)?;
        let held_bytes = (bytes.len() as u64).saturating_add(inflated.picture_bytes);
        let reservation = self.reserve(held_bytes)?;
        let bytes: WorkbookBytes = bytes.into();

        // One probing reader to detect the format and snapshot metadata.
        let probe = open_as(Cursor::new(Arc::clone(&bytes)), format_hint)?;
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
        pool.park(probe);

        let entry = Arc::new(WorkbookEntry {
            bytes,
            format,
            header_row,
            metadata,
            is_1904,
            pool,
            held_bytes,
            last_used: Mutex::new(Instant::now()),
        });

        let id = reservation.register(Arc::clone(&entry));
        Ok((id, entry))
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
        registry.held_bytes = registry.held_bytes.saturating_sub(entry.held_bytes);
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
        registry.held_bytes = registry.held_bytes.saturating_sub(freed);
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

    /// Bytes the open workbooks hold, as charged against
    /// [`StoreLimits::max_store_bytes`].
    ///
    /// # Panics
    ///
    /// Panics if the store lock was poisoned by a panic on another thread.
    #[must_use]
    pub fn held_bytes(&self) -> u64 {
        self.read().held_bytes
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
