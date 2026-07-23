// SPDX-License-Identifier: Apache-2.0

//! In-memory workbook store.
//!
//! Uploaded workbooks live only in process memory: the raw bytes are held
//! once per workbook in an `Arc<[u8]>` and parsed with calamine's
//! `open_workbook_*_from_rs` family. Nothing is ever written to disk, by
//! design.
//!
//! Concurrency model: an entry is immutable once opened. Every read
//! request builds its own calamine reader over a cheap `Cursor` clone of the
//! shared bytes, so reads of the same workbook run fully in parallel — there
//! is no per-workbook lock on the read path. The trade-off is that each
//! concurrent reader re-parses workbook-level structures (zip directory,
//! shared strings), which is the price of lock-free reads with calamine's
//! `&mut self` reader API.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, RwLock};

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
}

impl WorkbookEntry {
    /// Build a fresh, independent calamine reader over the shared bytes.
    ///
    /// This is blocking CPU work; callers must run it inside
    /// `tokio::task::spawn_blocking`.
    ///
    /// # Errors
    ///
    /// Returns [`OpenError`] when calamine cannot re-open the bytes in the
    /// format recorded at open time.
    pub fn reader(&self) -> Result<WorkbookReader, OpenError> {
        let mut workbook = open_as(Cursor::new(Arc::clone(&self.bytes)), self.format)?;
        if let Some(header_row) = self.header_row {
            workbook.with_header_row(header_row);
        }
        Ok(workbook)
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
        drop(probe);

        let entry = Arc::new(WorkbookEntry {
            bytes,
            format,
            header_row,
            metadata,
            is_1904,
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
