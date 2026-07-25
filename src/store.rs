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

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, Mutex, RwLock};

use calamine::{
    HeaderRow, Ods, Reader, Sheets, Xls, Xlsb, Xlsx, open_workbook_auto_from_rs,
    open_workbook_from_rs,
};

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
}

impl WorkbookEntry {
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
/// The lock is only held long enough to clone or remove an `Arc`; it is
/// never held while parsing, so it never throttles read concurrency.
#[derive(Default)]
pub struct WorkbookStore {
    inner: RwLock<HashMap<String, Arc<WorkbookEntry>>>,
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
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse `bytes` as a workbook, register it, and return its id and entry.
    ///
    /// This is blocking CPU work; callers must run it inside
    /// `tokio::task::spawn_blocking`.
    ///
    /// # Errors
    ///
    /// Returns [`OpenError`] when the bytes cannot be parsed as a workbook
    /// (or as the specific format given by `format_hint`).
    ///
    /// # Panics
    ///
    /// Panics if the store lock was poisoned by a panic on another thread.
    pub fn open(
        &self,
        bytes: Vec<u8>,
        format_hint: pb::WorkbookFormat,
        header_row: Option<HeaderRow>,
    ) -> Result<(String, Arc<WorkbookEntry>), OpenError> {
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
        });

        let id = uuid::Uuid::new_v4().to_string();
        self.inner
            .write()
            .expect("workbook store lock poisoned")
            .insert(id.clone(), Arc::clone(&entry));
        Ok((id, entry))
    }

    /// Look up an open workbook by id.
    ///
    /// # Panics
    ///
    /// Panics if the store lock was poisoned by a panic on another thread.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<WorkbookEntry>> {
        self.inner
            .read()
            .expect("workbook store lock poisoned")
            .get(id)
            .cloned()
    }

    /// Remove a workbook from the store. Returns true if it existed.
    ///
    /// # Panics
    ///
    /// Panics if the store lock was poisoned by a panic on another thread.
    pub fn close(&self, id: &str) -> bool {
        self.inner
            .write()
            .expect("workbook store lock poisoned")
            .remove(id)
            .is_some()
    }

    /// Number of currently open workbooks.
    ///
    /// # Panics
    ///
    /// Panics if the store lock was poisoned by a panic on another thread.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .read()
            .expect("workbook store lock poisoned")
            .len()
    }

    /// Whether the store currently holds no workbooks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
