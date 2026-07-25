// SPDX-License-Identifier: Apache-2.0

//! The `CalamineService` gRPC implementation.
//!
//! All calamine work is blocking, CPU-bound parsing; every read runs inside
//! `tokio::task::spawn_blocking` and pushes events into a bounded channel, so
//! slow consumers apply backpressure and many workbooks can stream
//! concurrently. Workbook bytes are kept in memory only.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Display;
use std::rc::Rc;
use std::sync::Arc;

use calamine::{CellType, Data, HeaderRow, Range, Reader, Sheets};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::convert;
use crate::proto::v1 as pb;
use crate::proto::v1::calamine_service_server::{CalamineService, CalamineServiceServer};
use crate::store::{WorkbookEntry, WorkbookStore};

/// Default upper bound on the uploaded workbook size: 512 MiB.
const DEFAULT_MAX_WORKBOOK_BYTES: usize = 512 * 1024 * 1024;

/// Channel capacity between a blocking parser task and the gRPC stream.
const STREAM_CHANNEL_CAPACITY: usize = 64;

/// Rows the server will pack into one `rows` event when the caller does not
/// choose. Only reached while the consumer is behind; a consumer that keeps up
/// receives smaller batches sooner. Sized so a batch of ordinary rows stays
/// far below the frame limit.
const DEFAULT_MAX_ROWS_PER_MESSAGE: usize = 256;

/// How long the first row of a batch may wait for company.
///
/// Bounds the latency batching adds. Small enough to be invisible in a live
/// view, large enough that a fast parser fills the row cap long before it
/// expires.
const DEFAULT_LINGER: std::time::Duration = std::time::Duration::from_millis(5);

/// Rows between linger-deadline checks, so the clock is not read per row.
const LINGER_CHECK_EVERY: usize = 32;

/// Per-message gRPC frame limit: 32 MiB. Upload clients should chunk well
/// below this (the reference chunking is 64 KiB–1 MiB); the encoding side
/// covers very wide rows.
const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;

/// Upper bound on the string bytes packed into one `StringTableChunk`, kept
/// far under `MAX_FRAME_BYTES` so a burst of fresh strings (a wide sheet's
/// first rows) can never build an unencodable event.
const MAX_STRING_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// gRPC implementation of `calamine.v1.CalamineService`.
pub struct CalamineGrpc {
    store: Arc<WorkbookStore>,
    max_workbook_bytes: usize,
}

impl CalamineGrpc {
    /// Create the service around an empty workbook store.
    #[must_use]
    pub fn new(store: WorkbookStore) -> Self {
        Self {
            store: Arc::new(store),
            max_workbook_bytes: DEFAULT_MAX_WORKBOOK_BYTES,
        }
    }

    /// Override the maximum accepted workbook size in bytes.
    #[must_use]
    pub fn with_max_workbook_bytes(mut self, max: usize) -> Self {
        self.max_workbook_bytes = max;
        self
    }

    /// Wrap into the generated tonic service, with frame limits sized for
    /// chunked uploads and wide rows.
    ///
    /// Compression is advertised but never imposed: the server accepts
    /// gzip- or zstd-compressed requests and will compress responses only
    /// for a client that asks (`grpc-accept-encoding`). The row stream is
    /// mostly repeated strings, so zstd trades CPU for a large wire
    /// reduction; the bench harness measures the exchange rate rather than
    /// assuming it.
    #[must_use]
    pub fn into_service(self) -> CalamineServiceServer<Self> {
        use tonic::codec::CompressionEncoding;
        CalamineServiceServer::new(self)
            .max_decoding_message_size(MAX_FRAME_BYTES)
            .max_encoding_message_size(MAX_FRAME_BYTES)
            .accept_compressed(CompressionEncoding::Gzip)
            .accept_compressed(CompressionEncoding::Zstd)
            .send_compressed(CompressionEncoding::Gzip)
            .send_compressed(CompressionEncoding::Zstd)
    }
}

/// A server-streamed response message that can carry an in-band
/// [`pb::StreamError`] event.
///
/// Implementing this lets [`send_stream_error`] and [`abort_with`] wrap an
/// error into the right response type without per-call-site plumbing.
trait StreamResponse: Send + 'static {
    /// Wrap an in-band error event into this response type.
    fn from_stream_error(error: pb::StreamError) -> Self;
}

impl StreamResponse for pb::StreamWorksheetRangeResponse {
    fn from_stream_error(error: pb::StreamError) -> Self {
        Self {
            event: Some(pb::stream_worksheet_range_response::Event::Error(error)),
        }
    }
}

impl StreamResponse for pb::StreamWorksheetFormulaResponse {
    fn from_stream_error(error: pb::StreamError) -> Self {
        Self {
            event: Some(pb::stream_worksheet_formula_response::Event::Error(error)),
        }
    }
}

impl StreamResponse for pb::StreamVbaProjectResponse {
    fn from_stream_error(error: pb::StreamError) -> Self {
        Self {
            event: Some(pb::stream_vba_project_response::Event::Error(error)),
        }
    }
}

impl StreamResponse for pb::GetPicturesResponse {
    fn from_stream_error(error: pb::StreamError) -> Self {
        Self {
            event: Some(pb::get_pictures_response::Event::Error(error)),
        }
    }
}

/// Resolve a `SheetSelector` to a concrete sheet name.
fn resolve_sheet_name(
    entry: &WorkbookEntry,
    selector: Option<&pb::SheetSelector>,
) -> Result<String, Status> {
    let selector =
        selector.ok_or_else(|| Status::invalid_argument("sheet selector is required"))?;
    match &selector.selector {
        Some(pb::sheet_selector::Selector::SheetName(name)) => Ok(name.clone()),
        Some(pb::sheet_selector::Selector::SheetIndex(i)) => entry
            .metadata
            .sheets
            .get(*i as usize)
            .map(|s| s.name.clone())
            .ok_or_else(|| Status::not_found(format!("no sheet at index {i}"))),
        None => Err(Status::invalid_argument("sheet selector is empty")),
    }
}

/// Look up a workbook or fail the RPC.
fn get_entry(store: &WorkbookStore, id: &str) -> Result<Arc<WorkbookEntry>, Status> {
    store
        .get(id)
        .ok_or_else(|| Status::not_found(format!("unknown workbook_id: {id}")))
}

/// Spawn `body` on the blocking pool and return its receiving stream.
///
/// The bounded channel is the backpressure boundary: when the client reads
/// slowly, the parser blocks on `send` instead of buffering the sheet.
fn spawn_blocking_stream<T, F>(body: F) -> Response<ReceiverStream<Result<T, Status>>>
where
    T: Send + 'static,
    F: FnOnce(mpsc::Sender<Result<T, Status>>) + Send + 'static,
{
    let (tx, rx) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
    tokio::task::spawn_blocking(move || body(tx));
    Response::new(ReceiverStream::new(rx))
}

/// Send one event; returns false when the client has gone away.
fn send_event<T>(tx: &mpsc::Sender<Result<T, Status>>, event: T) -> bool {
    tx.blocking_send(Ok(event)).is_ok()
}

/// Send an in-band error event; returns false when the client has gone away.
fn send_stream_error<T: StreamResponse>(
    tx: &mpsc::Sender<Result<T, Status>>,
    error: pb::CalamineError,
    terminal: bool,
) -> bool {
    send_event(
        tx,
        T::from_stream_error(pb::StreamError {
            error: Some(error),
            terminal,
        }),
    )
}

/// Convert a calamine parse failure into an in-band terminal stream error.
fn abort_with<T: StreamResponse>(
    tx: &mpsc::Sender<Result<T, Status>>,
    kind: pb::CalamineErrorKind,
    err: impl Display,
) {
    let _ = send_stream_error(tx, convert::calamine_error(kind, err), true);
}

/// Build the `RangeStarted` header for a parsed range. An empty range gets
/// no dimensions and zero cells.
fn range_header<T: CellType>(sheet_name: &str, range: &Range<T>) -> pb::RangeStarted {
    match (range.start(), range.end()) {
        (Some(start), Some(end)) => {
            let (height, width) = range.get_size();
            pb::RangeStarted {
                sheet_name: sheet_name.to_string(),
                dimensions: Some(pb::Dimensions {
                    start: Some(convert::cell_position(start)),
                    end: Some(convert::cell_position(end)),
                }),
                total_cells: (height * width) as u64,
            }
        }
        _ => pb::RangeStarted {
            sheet_name: sheet_name.to_string(),
            dimensions: None,
            total_cells: 0,
        },
    }
}

/// Wrap a header into a range-stream response.
fn range_started(header: pb::RangeStarted) -> pb::StreamWorksheetRangeResponse {
    pb::StreamWorksheetRangeResponse {
        event: Some(pb::stream_worksheet_range_response::Event::Started(header)),
    }
}

/// Wrap one dense row into a range-stream response.
fn range_row(row_index: u32, values: Vec<pb::CellData>) -> pb::StreamWorksheetRangeResponse {
    pb::StreamWorksheetRangeResponse {
        event: Some(pb::stream_worksheet_range_response::Event::Row(
            pb::WorksheetRow { row_index, values },
        )),
    }
}

/// Wrap a batch of rows into a range-stream response.
fn range_rows(rows: Vec<pb::WorksheetRow>) -> pb::StreamWorksheetRangeResponse {
    pb::StreamWorksheetRangeResponse {
        event: Some(pb::stream_worksheet_range_response::Event::Rows(
            pb::WorksheetRowBatch { rows },
        )),
    }
}

fn range_string_table(chunk: pb::StringTableChunk) -> pb::StreamWorksheetRangeResponse {
    pb::StreamWorksheetRangeResponse {
        event: Some(pb::stream_worksheet_range_response::Event::StringTable(
            chunk,
        )),
    }
}

/// Per-stream shared-string table for `use_string_table` mode.
///
/// `DataRef::SharedString` borrows into the workbook's own shared-strings
/// table, which is stable for the lifetime of the read, so entries are
/// interned by pointer identity: a hash of two machine words per cell, never
/// of the string body. Ids are dense from zero in order of first appearance,
/// and the batcher sends every pending entry as a `StringTableChunk` before
/// the first row event that could reference it. The table lives and dies
/// with one stream; ids carry no meaning outside it.
struct StringTable {
    index: HashMap<(usize, usize), u32>,
    /// Entries interned since the last chunk was taken, in id order.
    pending: Vec<String>,
    next_id: u32,
}

impl StringTable {
    fn new() -> Self {
        Self {
            index: HashMap::new(),
            pending: Vec::new(),
            next_id: 0,
        }
    }

    /// Intern one shared-string borrow, returning its stream-scoped id.
    fn intern(&mut self, s: &str) -> u32 {
        let key = (s.as_ptr() as usize, s.len());
        if let Some(id) = self.index.get(&key) {
            return *id;
        }
        let id = self.next_id;
        self.index.insert(key, id);
        self.pending.push(s.to_string());
        self.next_id += 1;
        id
    }

    /// Take the next chunk of not-yet-sent entries, bounded by
    /// `MAX_STRING_CHUNK_BYTES` (always at least one entry). `None` when
    /// everything interned so far is already on the wire.
    fn take_chunk(&mut self) -> Option<pb::StringTableChunk> {
        if self.pending.is_empty() {
            return None;
        }
        let first_id = self.next_id - self.pending.len() as u32;
        let mut bytes = 0usize;
        let mut n = 0usize;
        for s in &self.pending {
            if n > 0 && bytes + s.len() > MAX_STRING_CHUNK_BYTES {
                break;
            }
            bytes += s.len();
            n += 1;
        }
        let rest = self.pending.split_off(n);
        let entries = std::mem::replace(&mut self.pending, rest);
        Some(pb::StringTableChunk { first_id, entries })
    }
}

/// Accumulates rows and decides when to hand them to the channel.
///
/// This is Kafka's producer policy: fill up to `batch.size` rows, but never
/// hold the first row of a batch longer than `linger`. Waiting is what buys
/// the throughput, because the cost being amortized is per *message*, not per
/// row, in every client stack. An earlier version flushed as soon as the
/// channel had capacity (`linger.ms = 0`); that never batched at all here,
/// because calamine is the slow side of this pipeline and the consumer has
/// always drained by the time the next row is parsed.
///
/// The linger bounds the added latency, so a sheet that parses slowly still
/// streams rather than stalling until `cap` rows exist.
struct RowBatcher {
    rows: Vec<pb::WorksheetRow>,
    cap: usize,
    linger: std::time::Duration,
    /// When the batch's first row was queued.
    opened_at: Option<std::time::Instant>,
    /// When true, emit a bare `row` event per row, as the contract's
    /// `max_rows_per_message = 1` mode requires.
    single: bool,
    /// The stream's shared-string table in `use_string_table` mode. Shared
    /// with the cell conversion that interns into it; the batcher's job is
    /// to put pending entries on the wire before the rows that need them.
    strings: Option<Rc<RefCell<StringTable>>>,
}

impl RowBatcher {
    fn new(max_rows_per_message: u32, strings: Option<Rc<RefCell<StringTable>>>) -> Self {
        let cap = match max_rows_per_message {
            0 => DEFAULT_MAX_ROWS_PER_MESSAGE,
            n => n as usize,
        };
        Self {
            rows: Vec::new(),
            cap,
            linger: DEFAULT_LINGER,
            opened_at: None,
            single: cap == 1,
            strings,
        }
    }

    /// Put every string-table entry interned since the last row event on the
    /// wire. Called before sending rows, which is what upholds the
    /// contract's "defined before first referenced" guarantee.
    fn send_new_strings(
        &mut self,
        tx: &mpsc::Sender<Result<pb::StreamWorksheetRangeResponse, Status>>,
    ) -> bool {
        let Some(table) = &self.strings else {
            return true;
        };
        loop {
            let chunk = table.borrow_mut().take_chunk();
            match chunk {
                Some(chunk) => {
                    if !send_event(tx, range_string_table(chunk)) {
                        return false;
                    }
                }
                None => return true,
            }
        }
    }

    /// Queue one row, flushing on the row cap or the linger deadline.
    /// Returns false once the client has gone away.
    fn push(
        &mut self,
        tx: &mpsc::Sender<Result<pb::StreamWorksheetRangeResponse, Status>>,
        row_index: u32,
        values: Vec<pb::CellData>,
    ) -> bool {
        if self.single {
            return self.send_new_strings(tx) && send_event(tx, range_row(row_index, values));
        }
        if self.rows.is_empty() {
            self.opened_at = Some(std::time::Instant::now());
        }
        self.rows.push(pb::WorksheetRow { row_index, values });

        if self.rows.len() >= self.cap {
            return self.flush(tx);
        }
        // Checking the clock per row would cost more than it saves on a wide
        // sheet, and the cap already bounds the batch, so sample it instead.
        if self.rows.len().is_multiple_of(LINGER_CHECK_EVERY)
            && self.opened_at.is_some_and(|t| t.elapsed() >= self.linger)
        {
            return self.flush(tx);
        }
        true
    }

    /// Send whatever has accumulated. Returns false once the client is gone.
    fn flush(
        &mut self,
        tx: &mpsc::Sender<Result<pb::StreamWorksheetRangeResponse, Status>>,
    ) -> bool {
        if self.rows.is_empty() {
            return true;
        }
        self.opened_at = None;
        self.send_new_strings(tx) && send_event(tx, range_rows(std::mem::take(&mut self.rows)))
    }
}

/// Wrap a header into a formula-stream response.
fn formula_started(header: pb::RangeStarted) -> pb::StreamWorksheetFormulaResponse {
    pb::StreamWorksheetFormulaResponse {
        event: Some(pb::stream_worksheet_formula_response::Event::Started(
            header,
        )),
    }
}

/// Wrap one row of formula strings into a formula-stream response.
fn formula_row(row_index: u32, formulas: Vec<String>) -> pb::StreamWorksheetFormulaResponse {
    pb::StreamWorksheetFormulaResponse {
        event: Some(pb::stream_worksheet_formula_response::Event::Row(
            pb::FormulaRow {
                row_index,
                formulas,
            },
        )),
    }
}

/// Emit the rows of a dense `Range<Data>` (buffered path for XLS and ODS,
/// whose calamine readers do not expose an incremental cell iterator).
fn emit_range(
    sheet_name: &str,
    range: &Range<Data>,
    is_1904: bool,
    batcher: &mut RowBatcher,
    tx: &mpsc::Sender<Result<pb::StreamWorksheetRangeResponse, Status>>,
) {
    if !send_event(tx, range_started(range_header(sheet_name, range))) {
        return;
    }
    // Empty range: the header is the whole stream.
    let Some(start) = range.start() else { return };
    // Rows are anchored at column 0 to match the incremental path: a value's
    // index is its absolute column, so a client never needs the header to
    // place a cell.
    let pad = start.1 as usize;
    for (offset, row) in range.rows().enumerate() {
        let mut values = Vec::with_capacity(pad + row.len());
        values.resize(pad, convert::empty_cell_data());
        values.extend(
            row.iter()
                .map(|d| convert::cell_data(convert::data_value(d, is_1904))),
        );
        if !batcher.push(tx, start.0 + offset as u32, values) {
            return;
        }
    }
    batcher.flush(tx);
}

/// Stream cells from an incremental calamine cell reader (XLSX and XLSB),
/// densifying the sparse cell stream into full rows as they arrive.
///
/// The emitted grid matches what calamine's own `worksheet_range` reports in
/// row extent and populated cells: it spans the first to the last row holding
/// a non-empty cell, interior gaps are filled with empty rows, and leading or
/// trailing rows of blanks are not emitted even when the reader yields cells
/// for them. Rows are dense from column 0, so a value's index is its absolute
/// column.
/// `next_cell` yields `Ok(None)` at end of sheet.
fn emit_incremental<E: Display>(
    sheet_name: &str,
    dims: calamine::Dimensions,
    mut next_cell: impl FnMut() -> Result<Option<(u32, u32, pb::cell_data::Value)>, E>,
    kind: pb::CalamineErrorKind,
    batcher: &mut RowBatcher,
    tx: &mpsc::Sender<Result<pb::StreamWorksheetRangeResponse, Status>>,
) {
    let header = pb::RangeStarted {
        sheet_name: sheet_name.to_string(),
        dimensions: Some(convert::dimensions(dims)),
        total_cells: dims.len(),
    };
    if !send_event(tx, range_started(header)) {
        return;
    }

    // The declared extent is a hint, not a guarantee. `<dimension>` is
    // optional in ECMA-376 and writers get it wrong, which is why calamine's
    // own `worksheet_range` ignores it and rebuilds the extent from the cells
    // it actually sees (`Range::from_sparse`). Treating it as a filter would
    // silently drop every cell outside a wrong or absent declaration, so it is
    // used only to pre-size the row: a cell past it grows the row instead.
    //
    // Rows are anchored at column 0, never at the declared start column. A
    // one-pass stream that anchors at the declaration cannot re-base rows it
    // has already sent when a cell arrives left of a wrong declared start, so
    // anchoring there forces a choice between dropping cells and aborting;
    // anchoring at zero makes a value's index its absolute column and the
    // problem impossible.
    let mut width = dims.end.1 as usize + 1;
    let mut current_row = dims.start.0;
    let mut values = vec![convert::empty_cell_data(); width];

    // `values`/`current_row` describe a real row.
    let mut open = false;
    // The row under construction holds at least one non-empty cell.
    let mut row_has_value = false;
    // A non-empty row has already been emitted.
    let mut started = false;
    // Completed all-empty rows held back, waiting to learn whether they are an
    // interior gap or the sheet's trailing padding.
    let mut pending_empty: u32 = 0;

    // A row is only real to calamine if it holds a non-empty cell:
    // `Range::from_sparse` builds the extent from non-empty cells alone, so
    // leading and trailing rows of blanks are not part of the sheet even when
    // the reader emits cells for them. A worksheet can carry thousands of such
    // rows (styled but blank), so they are held back rather than streamed: an
    // interior gap is released once a later non-empty row proves it was a gap,
    // and trailing padding is simply dropped at end of sheet.
    macro_rules! complete_row {
        ($index:expr, $row:expr) => {{
            let index: u32 = $index;
            if row_has_value {
                for back in (1..=pending_empty).rev() {
                    if !batcher.push(tx, index - back, vec![convert::empty_cell_data(); width]) {
                        return;
                    }
                }
                pending_empty = 0;
                started = true;
                if !batcher.push(tx, index, $row) {
                    return;
                }
            } else if started {
                pending_empty += 1;
            }
            row_has_value = false;
        }};
    }

    loop {
        let cell = match next_cell() {
            Ok(Some(cell)) => cell,
            Ok(None) => break,
            Err(e) => return abort_with(tx, kind, e),
        };
        let (row, col, value) = cell;
        let idx = col as usize;

        if open {
            while current_row < row {
                let flushed =
                    std::mem::replace(&mut values, vec![convert::empty_cell_data(); width]);
                complete_row!(current_row, flushed);
                current_row += 1;
            }
        } else {
            // Cells arrive in row-major order, so the first one fixes the
            // starting row. Snapping to it keeps an under-declared start from
            // manufacturing leading empty rows that calamine would not report.
            current_row = row;
            open = true;
        }

        if idx >= width {
            width = idx + 1;
            values.resize(width, convert::empty_cell_data());
        }
        if !matches!(value, pb::cell_data::Value::Empty(())) {
            row_has_value = true;
        }
        values[idx] = convert::cell_data(value);
    }

    // Complete the final row. Written out rather than reusing the macro
    // because none of its bookkeeping is read again. Anything still pending is
    // trailing padding and is deliberately not emitted.
    if open && row_has_value {
        for back in (1..=pending_empty).rev() {
            if !batcher.push(
                tx,
                current_row - back,
                vec![convert::empty_cell_data(); width],
            ) {
                return;
            }
        }
        if !batcher.push(tx, current_row, values) {
            return;
        }
    }
    batcher.flush(tx);
}

/// Convert one `Cell<DataRef>` from an incremental reader into the tuple
/// form used by `emit_incremental`. With a string table, shared strings
/// become ids interned against the borrow instead of copies of the body.
fn cell_tuple(
    cell: calamine::Cell<calamine::DataRef<'_>>,
    is_1904: bool,
    table: Option<&Rc<RefCell<StringTable>>>,
) -> (u32, u32, pb::cell_data::Value) {
    let (row, col) = cell.get_position();
    let value = match (cell.get_value(), table) {
        (calamine::DataRef::SharedString(s), Some(table)) => {
            pb::cell_data::Value::SharedStringId(table.borrow_mut().intern(s))
        }
        (value, _) => convert::data_ref_value(value, is_1904),
    };
    (row, col, value)
}

/// The blocking body of `StreamWorksheetRange`.
///
/// XLSX and XLSB use calamine's incremental cell readers, so rows are sent
/// as the parser walks the sheet. XLS and ODS only offer whole-range
/// parsing, so the range is parsed and then streamed row by row.
fn run_stream_worksheet_range(
    entry: &WorkbookEntry,
    selector: Option<&pb::SheetSelector>,
    max_rows_per_message: u32,
    use_string_table: bool,
    tx: &mpsc::Sender<Result<pb::StreamWorksheetRangeResponse, Status>>,
) {
    // Only the incremental readers produce shared strings, so only they can
    // fill the table; for other formats the flag is an accepted no-op.
    let table = use_string_table.then(|| Rc::new(RefCell::new(StringTable::new())));
    let mut batcher = RowBatcher::new(max_rows_per_message, table.clone());
    let sheet_name = match resolve_sheet_name(entry, selector) {
        Ok(name) => name,
        Err(status) => {
            let _ = tx.blocking_send(Err(status));
            return;
        }
    };
    let kind = convert::error_kind_for_format(entry.format);
    // Fresh independent reader: no locks, fully parallel with other reads.
    let mut workbook = match entry.reader() {
        Ok(workbook) => workbook,
        Err(e) => return abort_with(tx, kind, e),
    };

    let is_1904 = entry.is_1904;
    match &mut *workbook {
        Sheets::Xlsx(xlsx) => {
            // The cells reader refuses non-worksheets (e.g. chartsheets)
            // that `worksheet_range` still answers for with an empty range.
            // The contract is parity with calamine's own API, so fall back
            // to the buffered path and only fail if calamine itself does.
            // The scope ends the reader's borrow before the fallback
            // re-borrows the workbook.
            let streamed = match xlsx.worksheet_cells_reader(&sheet_name) {
                Ok(mut reader) => {
                    let dims = reader.dimensions();
                    emit_incremental(
                        &sheet_name,
                        dims,
                        || {
                            reader
                                .next_cell()
                                .map(|opt| opt.map(|cell| cell_tuple(cell, is_1904, table.as_ref())))
                        },
                        kind,
                        &mut batcher,
                        tx,
                    );
                    true
                }
                Err(_) => false,
            };
            if !streamed {
                match xlsx.worksheet_range(&sheet_name) {
                    Ok(range) => emit_range(&sheet_name, &range, is_1904, &mut batcher, tx),
                    Err(e) => abort_with(tx, kind, e),
                }
            }
        }
        Sheets::Xlsb(xlsb) => {
            // Same chartsheet fallback as the XLSX arm.
            let streamed = match xlsb.worksheet_cells_reader(&sheet_name) {
                Ok(mut reader) => {
                    let dims = reader.dimensions();
                    emit_incremental(
                        &sheet_name,
                        dims,
                        || {
                            reader
                                .next_cell()
                                .map(|opt| opt.map(|cell| cell_tuple(cell, is_1904, table.as_ref())))
                        },
                        kind,
                        &mut batcher,
                        tx,
                    );
                    true
                }
                Err(_) => false,
            };
            if !streamed {
                match xlsb.worksheet_range(&sheet_name) {
                    Ok(range) => emit_range(&sheet_name, &range, is_1904, &mut batcher, tx),
                    Err(e) => abort_with(tx, kind, e),
                }
            }
        }
        other => {
            let range = match other.worksheet_range(&sheet_name) {
                Ok(range) => range,
                Err(e) => return abort_with(tx, kind, e),
            };
            emit_range(&sheet_name, &range, is_1904, &mut batcher, tx);
        }
    }
}

/// The blocking body of `StreamWorksheetFormula`.
///
/// Calamine only exposes formulas as a whole `Range<String>`, so the range
/// is parsed first and then streamed row by row.
fn run_stream_worksheet_formula(
    entry: &WorkbookEntry,
    selector: Option<&pb::SheetSelector>,
    tx: &mpsc::Sender<Result<pb::StreamWorksheetFormulaResponse, Status>>,
) {
    let sheet_name = match resolve_sheet_name(entry, selector) {
        Ok(name) => name,
        Err(status) => {
            let _ = tx.blocking_send(Err(status));
            return;
        }
    };
    let kind = convert::error_kind_for_format(entry.format);
    let mut workbook = match entry.reader() {
        Ok(workbook) => workbook,
        Err(e) => return abort_with(tx, kind, e),
    };
    let range = match workbook.worksheet_formula(&sheet_name) {
        Ok(range) => range,
        Err(e) => return abort_with(tx, kind, e),
    };
    drop(workbook);

    if !send_event(tx, formula_started(range_header(&sheet_name, &range))) {
        return;
    }
    // Empty range: the header is the whole stream.
    let Some(start) = range.start() else { return };
    // Anchored at column 0 like the value stream: a formula's index is its
    // absolute column. Cells without formulas are empty strings either way.
    let pad = start.1 as usize;
    for (offset, row) in range.rows().enumerate() {
        let mut formulas = vec![String::new(); pad];
        formulas.extend_from_slice(row);
        if !send_event(tx, formula_row(start.0 + offset as u32, formulas)) {
            return;
        }
    }
}

/// The blocking body of `StreamVbaProject`.
///
/// One `VbaProjectInfo` header (present flag, references, module names),
/// then one `VbaModule` event per module. Per-module read failures are
/// delivered as non-terminal in-band errors so the remaining modules can
/// still be streamed.
fn run_stream_vba_project(
    entry: &WorkbookEntry,
    tx: &mpsc::Sender<Result<pb::StreamVbaProjectResponse, Status>>,
) {
    let kind = convert::error_kind_for_format(entry.format);
    let mut workbook = match entry.reader() {
        Ok(workbook) => workbook,
        Err(e) => return abort_with(tx, kind, e),
    };
    let project = match workbook.vba_project() {
        Ok(Some(project)) => project,
        Ok(None) => {
            let _ = send_event(
                tx,
                pb::StreamVbaProjectResponse {
                    event: Some(pb::stream_vba_project_response::Event::Info(
                        pb::VbaProjectInfo {
                            present: false,
                            references: Vec::new(),
                            module_names: Vec::new(),
                        },
                    )),
                },
            );
            return;
        }
        Err(e) => return abort_with(tx, kind, e),
    };

    let module_names: Vec<String> = project
        .get_module_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    let info = pb::VbaProjectInfo {
        present: true,
        references: project
            .get_references()
            .iter()
            .map(|r| pb::VbaReference {
                name: r.name.clone(),
                description: r.description.clone(),
                path: r.path.display().to_string(),
            })
            .collect(),
        module_names: module_names.clone(),
    };
    if !send_event(
        tx,
        pb::StreamVbaProjectResponse {
            event: Some(pb::stream_vba_project_response::Event::Info(info)),
        },
    ) {
        return;
    }

    for name in module_names {
        let event = match project.get_module_raw(&name) {
            Ok(raw) => pb::StreamVbaProjectResponse {
                event: Some(pb::stream_vba_project_response::Event::Module(
                    pb::VbaModule {
                        name,
                        raw_content: raw.to_vec(),
                    },
                )),
            },
            // Per-module failures are non-terminal: remaining modules can
            // still be delivered.
            Err(e) => pb::StreamVbaProjectResponse::from_stream_error(pb::StreamError {
                error: Some(convert::calamine_error(pb::CalamineErrorKind::Vba, e)),
                terminal: false,
            }),
        };
        if !send_event(tx, event) {
            return;
        }
    }
}

/// The blocking body of `GetPictures`: one `Picture` event per embedded
/// image.
fn run_get_pictures(
    entry: &WorkbookEntry,
    tx: &mpsc::Sender<Result<pb::GetPicturesResponse, Status>>,
) {
    let kind = convert::error_kind_for_format(entry.format);
    let workbook = match entry.reader() {
        Ok(workbook) => workbook,
        Err(e) => return abort_with(tx, kind, e),
    };
    for pic in workbook.pictures_with_metadata() {
        let event = pb::GetPicturesResponse {
            event: Some(pb::get_pictures_response::Event::Picture(pb::Picture {
                row: pic.row,
                col: pic.col,
                sheet_name: pic.sheet_name,
                extension: pic.extension,
                data: pic.data,
                name: pic.name,
            })),
        };
        if !send_event(tx, event) {
            return;
        }
    }
}

#[tonic::async_trait]
impl CalamineService for CalamineGrpc {
    async fn open_workbook(
        &self,
        request: Request<Streaming<pb::OpenWorkbookRequest>>,
    ) -> Result<Response<pb::OpenWorkbookResponse>, Status> {
        let mut stream = request.into_inner();

        // First frame must carry the options.
        let first = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("empty upload: no frames received"))?;
        let Some(pb::open_workbook_request::Payload::Options(options)) = first.payload else {
            return Err(Status::invalid_argument(
                "first upload frame must carry `options`",
            ));
        };
        let format_hint = pb::WorkbookFormat::try_from(options.format_hint)
            .map_err(|_| Status::invalid_argument("unknown format_hint value"))?;
        let header_row = options.header_row.map(|hr| match hr.selection {
            Some(pb::header_row::Selection::RowIndex(i)) => HeaderRow::Row(i),
            _ => HeaderRow::FirstNonEmptyRow,
        });

        // Remaining frames are file bytes; held in memory only.
        let mut bytes = Vec::new();
        while let Some(frame) = stream.message().await? {
            match frame.payload {
                Some(pb::open_workbook_request::Payload::Chunk(chunk)) => {
                    if bytes.len() + chunk.len() > self.max_workbook_bytes {
                        return Err(Status::resource_exhausted(format!(
                            "workbook exceeds the {} byte limit",
                            self.max_workbook_bytes
                        )));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                _ => {
                    return Err(Status::invalid_argument(
                        "`options` frame must be the first and only options frame",
                    ));
                }
            }
        }
        if bytes.is_empty() {
            return Err(Status::invalid_argument("upload contained no file bytes"));
        }

        let store = Arc::clone(&self.store);
        let (id, entry) =
            tokio::task::spawn_blocking(move || store.open(bytes, format_hint, header_row))
                .await
                .map_err(|e| Status::internal(format!("parser task failed: {e}")))?
                .map_err(|e| Status::invalid_argument(format!("cannot open workbook: {e}")))?;

        Ok(Response::new(pb::OpenWorkbookResponse {
            workbook_id: id,
            detected_format: entry.format as i32,
            metadata: Some(entry.metadata.clone()),
        }))
    }

    async fn close_workbook(
        &self,
        request: Request<pb::CloseWorkbookRequest>,
    ) -> Result<Response<pb::CloseWorkbookResponse>, Status> {
        let closed = self.store.close(&request.into_inner().workbook_id);
        Ok(Response::new(pb::CloseWorkbookResponse { closed }))
    }

    async fn get_metadata(
        &self,
        request: Request<pb::GetMetadataRequest>,
    ) -> Result<Response<pb::GetMetadataResponse>, Status> {
        let entry = get_entry(&self.store, &request.into_inner().workbook_id)?;
        Ok(Response::new(pb::GetMetadataResponse {
            detected_format: entry.format as i32,
            metadata: Some(entry.metadata.clone()),
        }))
    }

    async fn get_defined_names(
        &self,
        request: Request<pb::GetDefinedNamesRequest>,
    ) -> Result<Response<pb::GetDefinedNamesResponse>, Status> {
        let entry = get_entry(&self.store, &request.into_inner().workbook_id)?;
        Ok(Response::new(pb::GetDefinedNamesResponse {
            defined_names: entry.metadata.defined_names.clone(),
        }))
    }

    type StreamWorksheetRangeStream =
        ReceiverStream<Result<pb::StreamWorksheetRangeResponse, Status>>;

    async fn stream_worksheet_range(
        &self,
        request: Request<pb::StreamWorksheetRangeRequest>,
    ) -> Result<Response<Self::StreamWorksheetRangeStream>, Status> {
        let req = request.into_inner();
        let entry = get_entry(&self.store, &req.workbook_id)?;
        Ok(spawn_blocking_stream(move |tx| {
            run_stream_worksheet_range(
                &entry,
                req.sheet.as_ref(),
                req.max_rows_per_message,
                req.use_string_table,
                &tx,
            );
        }))
    }

    type StreamWorksheetFormulaStream =
        ReceiverStream<Result<pb::StreamWorksheetFormulaResponse, Status>>;

    async fn stream_worksheet_formula(
        &self,
        request: Request<pb::StreamWorksheetFormulaRequest>,
    ) -> Result<Response<Self::StreamWorksheetFormulaStream>, Status> {
        let req = request.into_inner();
        let entry = get_entry(&self.store, &req.workbook_id)?;
        Ok(spawn_blocking_stream(move |tx| {
            run_stream_worksheet_formula(&entry, req.sheet.as_ref(), &tx);
        }))
    }

    type StreamVbaProjectStream = ReceiverStream<Result<pb::StreamVbaProjectResponse, Status>>;

    async fn stream_vba_project(
        &self,
        request: Request<pb::StreamVbaProjectRequest>,
    ) -> Result<Response<Self::StreamVbaProjectStream>, Status> {
        let req = request.into_inner();
        let entry = get_entry(&self.store, &req.workbook_id)?;
        Ok(spawn_blocking_stream(move |tx| {
            run_stream_vba_project(&entry, &tx);
        }))
    }

    type GetPicturesStream = ReceiverStream<Result<pb::GetPicturesResponse, Status>>;

    async fn get_pictures(
        &self,
        request: Request<pb::GetPicturesRequest>,
    ) -> Result<Response<Self::GetPicturesStream>, Status> {
        let req = request.into_inner();
        let entry = get_entry(&self.store, &req.workbook_id)?;
        Ok(spawn_blocking_stream(move |tx| {
            run_get_pictures(&entry, &tx);
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ids are dense from zero in first-appearance order, and repeats of the
    /// same borrow resolve to the same id without growing the table.
    #[test]
    fn string_table_ids_are_dense_and_pointer_deduped() {
        let backing = String::from("alpha beta alpha");
        let mut table = StringTable::new();

        let alpha = &backing[0..5];
        let beta = &backing[6..10];
        assert_eq!(table.intern(alpha), 0);
        assert_eq!(table.intern(beta), 1);
        assert_eq!(table.intern(alpha), 0, "same borrow, same id");
        assert_eq!(table.intern(beta), 1);

        let chunk = table.take_chunk().expect("two entries pending");
        assert_eq!(chunk.first_id, 0);
        assert_eq!(chunk.entries, vec!["alpha".to_string(), "beta".to_string()]);
        assert!(table.take_chunk().is_none(), "nothing left after the take");
    }

    /// Interning is by pointer identity, mirroring the workbook's own table:
    /// equal text at a different address is a different entry, exactly as
    /// two identical entries in an sst would be.
    #[test]
    fn string_table_distinguishes_equal_text_at_different_addresses() {
        let a = String::from("same");
        let b = String::from("same");
        let mut table = StringTable::new();
        assert_eq!(table.intern(&a), 0);
        assert_eq!(table.intern(&b), 1);
    }

    /// A burst of large fresh strings splits across chunks at the byte cap,
    /// preserving order and id continuity, so no chunk can outgrow the gRPC
    /// frame limit.
    #[test]
    fn string_table_chunks_split_at_the_byte_cap() {
        // Three strings of 3 MiB against a 4 MiB cap: the first chunk takes
        // one string plus whatever fits (only the first, since two exceed
        // the cap), and the remainder follows in later chunks.
        let big: Vec<String> = (0..3)
            .map(|i| {
                let mut s = String::with_capacity(3 * 1024 * 1024);
                while s.len() < 3 * 1024 * 1024 {
                    s.push(char::from(b'a' + i));
                }
                s
            })
            .collect();
        let mut table = StringTable::new();
        for s in &big {
            table.intern(s);
        }

        let mut collected: Vec<String> = Vec::new();
        let mut next_expected_id = 0u32;
        while let Some(chunk) = table.take_chunk() {
            assert_eq!(
                chunk.first_id, next_expected_id,
                "chunks are dense and in order"
            );
            let bytes: usize = chunk.entries.iter().map(String::len).sum();
            assert!(
                bytes <= MAX_STRING_CHUNK_BYTES || chunk.entries.len() == 1,
                "a chunk only exceeds the cap when a single entry does"
            );
            next_expected_id += chunk.entries.len() as u32;
            collected.extend(chunk.entries);
        }
        assert_eq!(collected, big, "every entry arrives exactly once, in order");
    }

    /// An entry bigger than the cap still ships, alone in its chunk.
    #[test]
    fn string_table_oversized_entry_ships_alone() {
        let huge = "x".repeat(MAX_STRING_CHUNK_BYTES + 1);
        let small = String::from("small");
        let mut table = StringTable::new();
        table.intern(&huge);
        table.intern(&small);

        let first = table.take_chunk().expect("first chunk");
        assert_eq!(first.entries.len(), 1, "the oversized entry ships alone");
        assert_eq!(first.entries[0].len(), MAX_STRING_CHUNK_BYTES + 1);
        let second = table.take_chunk().expect("second chunk");
        assert_eq!(second.first_id, 1);
        assert_eq!(second.entries, vec![small]);
    }
}
