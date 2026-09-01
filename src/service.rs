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

/// How long a parse may wait on a consumer that is not draining before the
/// stream is abandoned.
///
/// `blocking_send` parks its thread until capacity appears, with no bound. A
/// client that opens streams and never reads them therefore pins one blocking
/// thread each, and enough of those exhaust the pool and hang every later RPC
/// on the server, including `OpenWorkbook` from unrelated clients. A consumer
/// that has not taken a single message in this long is not slow, it is gone.
const CONSUMER_STALL: std::time::Duration = std::time::Duration::from_secs(30);

/// Default cap on streaming reads running at once.
///
/// Bounds the blast radius rather than the defect: streams may still starve
/// each other, but they can never take the whole blocking pool and with it the
/// unary RPCs.
const DEFAULT_MAX_CONCURRENT_STREAMS: usize = 128;

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

/// Rows between linger-deadline checks once a batch is large enough for the
/// clock read to be worth amortizing. Below this the deadline is checked every
/// row, because a batch that never gets this big would otherwise never flush
/// on its deadline at all, which is exactly the slow-parser case the linger
/// exists for.
const LINGER_CHECK_EVERY: usize = 32;

/// Hard ceiling on `max_rows_per_message`, whatever the caller asks for.
///
/// The byte cap below is the real bound; this only stops an absurd request
/// from parking millions of `WorksheetRow` structs in the batcher on a sheet
/// whose rows are individually tiny.
const MAX_ROWS_PER_MESSAGE_CEILING: usize = 65_536;

/// Per-message gRPC frame limit: 32 MiB. Upload clients should chunk well
/// below this (the reference chunking is 64 KiB–1 MiB); the encoding side
/// covers very wide rows.
const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;

/// Upper bound on the estimated payload of one `rows` batch.
///
/// `max_rows_per_message` bounds rows, not bytes: 256 rows of a
/// 16,384-column sheet holding even short strings runs past the frame limit,
/// and exceeding it fails the encode and kills the stream on a legal
/// workbook. A quarter of the frame limit leaves room for the estimate to be
/// wrong.
const MAX_BATCH_BYTES: usize = MAX_FRAME_BYTES / 4;

/// Upper bound on the string bytes packed into one `StringTableChunk`, kept
/// far under `MAX_FRAME_BYTES` so a burst of fresh strings (a wide sheet's
/// first rows) can never build an unencodable event.
const MAX_STRING_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// Widest row the *declared* extent may pre-allocate, matching Excel's own
/// column limit and calamine's `xlsx::MAX_COLUMNS` (not re-exported, so it is
/// restated here).
///
/// This bounds a hint, never the data: rows still grow to hold any cell that
/// actually arrives. It exists because the declared end column comes straight
/// out of the uploaded file and calamine only *warns* past this limit rather
/// than clamping (`get_dimension`, xlsx/mod.rs:2794), so `A1:ZZZZZZ1` parses
/// to column 321,272,405 and would otherwise commit ~10 GiB before the first
/// cell is read.
const MAX_DECLARED_COLUMNS: usize = 16_384;

/// Rows between client-liveness checks while skipping an empty gap.
///
/// A gap costs no allocation and sends nothing, so without this a crafted
/// `r="4000000000"` would spin the parse thread for hours after the caller
/// had already hung up.
const GAP_CHECK_EVERY: u32 = 4096;

/// Rough encoded size of one row, used only to decide when to close a batch.
///
/// Deliberately not `prost::Message::encoded_len`, which walks the same tree
/// the encoder is about to walk and would price every row twice. Over-
/// estimating is safe (it flushes sooner) and under-estimating by a wide
/// margin still lands inside the frame limit, because the cap is a quarter of
/// it.
fn approx_row_bytes(values: &[pb::CellData]) -> usize {
    /// Field tag, length prefix, and the `CellData` submessage header.
    const PER_CELL: usize = 6;
    values
        .iter()
        .map(|cell| {
            PER_CELL
                + match &cell.value {
                    Some(
                        pb::cell_data::Value::StringValue(s)
                        | pb::cell_data::Value::SharedStringValue(s)
                        | pb::cell_data::Value::DateTimeIso(s)
                        | pb::cell_data::Value::DurationIso(s),
                    ) => s.len(),
                    _ => 8,
                }
        })
        .sum()
}

/// Total cells in a declared extent, tolerating a degenerate declaration.
///
/// `calamine::Dimensions::len` subtracts the corners with unchecked `u32`
/// arithmetic (lib.rs:181). ECMA-376 does not require `ref` to be ordered, so
/// `<dimension ref="C5:A1"/>` underflows it: a release build reports
/// 18,446,744,056,529,682,435 cells and a build with overflow checks panics
/// outright. The declaration is only ever a pre-allocation hint here, so a
/// reversed one is reported as an empty extent rather than propagated.
fn declared_total_cells(dims: calamine::Dimensions) -> u64 {
    if dims.end.0 < dims.start.0 || dims.end.1 < dims.start.1 {
        return 0;
    }
    let rows = u64::from(dims.end.0 - dims.start.0) + 1;
    let cols = u64::from(dims.end.1 - dims.start.1) + 1;
    rows * cols
}

/// gRPC implementation of `calamine.v1.CalamineService`.
pub struct CalamineGrpc {
    store: Arc<WorkbookStore>,
    max_workbook_bytes: usize,
    /// Admission control for streaming reads. Every stream holds a permit for
    /// its whole life, so the number of blocking-pool threads this service can
    /// occupy is bounded and the unary RPCs always have threads left.
    stream_slots: Arc<tokio::sync::Semaphore>,
}

impl CalamineGrpc {
    /// Create the service around an empty workbook store.
    #[must_use]
    pub fn new(store: WorkbookStore) -> Self {
        Self {
            store: Arc::new(store),
            max_workbook_bytes: DEFAULT_MAX_WORKBOOK_BYTES,
            stream_slots: Arc::new(tokio::sync::Semaphore::new(DEFAULT_MAX_CONCURRENT_STREAMS)),
        }
    }

    /// Override the maximum accepted workbook size in bytes.
    #[must_use]
    pub fn with_max_workbook_bytes(mut self, max: usize) -> Self {
        self.max_workbook_bytes = max;
        self
    }

    /// Override how many streaming reads may run at once.
    ///
    /// Requests past the cap are refused immediately with
    /// `RESOURCE_EXHAUSTED` rather than queued, so a caller learns to back off
    /// instead of waiting behind a stream that may itself be stuck.
    #[must_use]
    pub fn with_max_concurrent_streams(mut self, max: usize) -> Self {
        self.stream_slots = Arc::new(tokio::sync::Semaphore::new(max));
        self
    }

    /// Take a streaming slot, or refuse the request.
    fn admit(&self) -> Result<tokio::sync::OwnedSemaphorePermit, Status> {
        Arc::clone(&self.stream_slots)
            .try_acquire_owned()
            .map_err(|_| {
                Status::resource_exhausted(
                    "too many concurrent streaming reads; retry shortly or raise \
                     GRPC_CALAMINE_MAX_CONCURRENT_STREAMS",
                )
            })
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

/// The frontend advertisement block served on `GetMetadata`.
///
/// Same shape in every ai-pipestream gRPC service, so dashboards and
/// embedding hosts can discover and link this service's web UI.
fn ui_info() -> pb::UiInfo {
    pb::UiInfo {
        title: "Calamine".to_owned(),
        path: "/ui/calamine".to_owned(),
        description: "Spreadsheet parsing via calamine (xls, xlsx, xlsb, ods)".to_owned(),
    }
}

/// Spawn `body` on the blocking pool and return its receiving stream.
///
/// The bounded channel is the backpressure boundary: when the client reads
/// slowly, the parser blocks on `send` instead of buffering the sheet.
///
/// A supervisor holds a second sender so a panic in `body` can still be
/// reported. Without it the panic drops the only sender and the stream ends
/// *successfully* with whatever had been sent so far, which a caller cannot
/// tell from a short sheet. calamine can panic on a crafted workbook (an
/// out-of-order `<dimension ref="C5:A1"/>` underflows its unchecked corner
/// subtraction at xlsx/mod.rs:2789), so this is reachable from untrusted
/// input, not just from a bug here.
/// `permit` is released only once the body has finished and any terminal
/// status has been delivered, so a wedged stream keeps its slot until the
/// client actually goes away.
fn spawn_blocking_stream<T, F>(
    permit: tokio::sync::OwnedSemaphorePermit,
    body: F,
) -> Response<ReceiverStream<Result<T, Status>>>
where
    T: Send + 'static,
    F: FnOnce(mpsc::Sender<Result<T, Status>>) + Send + 'static,
{
    let (tx, rx) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
    let supervisor = tx.clone();
    let handle = tokio::task::spawn_blocking(move || {
        // Pool threads are reused, so never trust what the last body left.
        STALL_REASON.with(|reason| *reason.borrow_mut() = None);
        body(tx);
        STALL_REASON.with(|reason| reason.borrow_mut().take())
    });
    tokio::spawn(async move {
        let status = match handle.await {
            Err(join) => Some(Status::internal(panic_detail(join))),
            Ok(stalled) => stalled,
        };
        if let Some(status) = status {
            // Terminal failures travel as a gRPC status, per the contract;
            // in-band `StreamError` is only for failures the stream survives.
            let _ = supervisor.send(Err(status)).await;
        }
        drop(permit);
    });
    Response::new(ReceiverStream::new(rx))
}

/// Render a `JoinError` into a message worth putting on the wire.
fn panic_detail(join: tokio::task::JoinError) -> String {
    if !join.is_panic() {
        return "parser task was cancelled".to_string();
    }
    let payload = join.into_panic();
    let detail = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string());
    format!("parser panicked: {detail}")
}

thread_local! {
    /// Why the parse running on this blocking thread gave up, if it did.
    ///
    /// A pool thread runs one stream body at a time, so this is per-stream
    /// state: [`spawn_blocking_stream`] clears it on entry (threads are
    /// reused) and takes it on exit, handing it to the supervisor, which can
    /// still reach the client when the body itself no longer can. It exists so
    /// an abandoned stream ends with a status instead of looking like a sheet
    /// that simply stopped.
    static STALL_REASON: RefCell<Option<Status>> = const { RefCell::new(None) };
}

/// Send one event; returns false when the stream should stop.
///
/// The fast path is a non-blocking `try_send`. Only a consumer that is
/// genuinely behind reaches the waiting path, and that wait is bounded: see
/// [`CONSUMER_STALL`] for why an unbounded one takes the server down.
fn send_event<T>(tx: &mpsc::Sender<Result<T, Status>>, event: T) -> bool {
    let event = match tx.try_send(Ok(event)) {
        Ok(()) => return true,
        Err(mpsc::error::TrySendError::Closed(_)) => return false,
        Err(mpsc::error::TrySendError::Full(event)) => event,
    };
    // Inside `spawn_blocking` there is always a runtime; be defensive anyway,
    // because blocking without one would panic rather than wait.
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return false;
    };
    match runtime.block_on(tx.send_timeout(event, CONSUMER_STALL)) {
        Ok(()) => true,
        Err(mpsc::error::SendTimeoutError::Closed(_)) => false,
        Err(mpsc::error::SendTimeoutError::Timeout(_)) => {
            STALL_REASON.with(|reason| {
                reason.borrow_mut().get_or_insert_with(|| {
                    Status::deadline_exceeded(format!(
                        "the client read nothing for {} seconds; abandoning the stream \
                         so its parser thread can be reused",
                        CONSUMER_STALL.as_secs()
                    ))
                });
            });
            false
        }
    }
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

/// End the stream because a row arrived too far out of order to repair.
///
/// A one-pass stream cannot retract a message it has already sent, so once
/// rows are committed an earlier row has nowhere to go. Failing loudly is the
/// only honest option left: the alternative, folding the cell into whichever
/// row happens to be under construction, is silent data loss.
fn abort_unsorted(
    tx: &mpsc::Sender<Result<pb::StreamWorksheetRangeResponse, Status>>,
    kind: pb::CalamineErrorKind,
    sheet_name: &str,
    row: u32,
    col: u32,
) {
    abort_with(
        tx,
        kind,
        format!(
            "sheet {sheet_name:?}: a cell at row {row}, column {col} arrived after \
             rows past it had already been streamed. The worksheet's rows are not \
             in ascending order, and this stream had committed too far to place it. \
             Retry with a larger max_rows_per_message to widen the repair window."
        ),
    );
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
    /// Estimated encoded size of everything in `rows`, so a wide sheet closes
    /// a batch on bytes long before the row cap builds an unencodable message.
    pending_bytes: usize,
    linger: std::time::Duration,
    /// When the batch's first row was queued.
    opened_at: Option<std::time::Instant>,
    /// Carrier choice only: when true each queued row leaves as its own `row`
    /// event instead of one `rows` batch, as `max_rows_per_message = 1`
    /// requires.
    ///
    /// It deliberately does *not* mean "do not queue". The queue is also the
    /// window inside which a late, out-of-order row can still be repaired
    /// ([`RowBatcher::place`]), and how far that repair reaches must not
    /// depend on which carrier the caller happened to ask for.
    single: bool,
    /// True once any event has reached the channel. gRPC offers no way to take
    /// a message back, so this is the line past which an out-of-order row can
    /// no longer be repaired.
    sent: bool,
    /// The stream's shared-string table in `use_string_table` mode. Shared
    /// with the cell conversion that interns into it; the batcher's job is
    /// to put pending entries on the wire before the rows that need them.
    strings: Option<Rc<RefCell<StringTable>>>,
}

impl RowBatcher {
    fn new(max_rows_per_message: u32, strings: Option<Rc<RefCell<StringTable>>>) -> Self {
        let requested = match max_rows_per_message {
            0 => DEFAULT_MAX_ROWS_PER_MESSAGE,
            n => (n as usize).min(MAX_ROWS_PER_MESSAGE_CEILING),
        };
        let single = requested == 1;
        Self {
            rows: Vec::new(),
            // Single mode queues to the same depth as the default; only the
            // carrier differs, so repair reach is identical either way.
            cap: if single {
                DEFAULT_MAX_ROWS_PER_MESSAGE
            } else {
                requested
            },
            pending_bytes: 0,
            linger: DEFAULT_LINGER,
            opened_at: None,
            single,
            sent: false,
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
        if self.rows.is_empty() {
            self.opened_at = Some(std::time::Instant::now());
        }
        self.pending_bytes += approx_row_bytes(&values);
        self.rows.push(pb::WorksheetRow { row_index, values });

        // Either bound closes a batch. Bytes are the one that matters on a
        // wide sheet, where the row cap alone would build a message the
        // encoder then refuses.
        if self.rows.len() >= self.cap || self.pending_bytes >= MAX_BATCH_BYTES {
            return self.flush(tx);
        }
        // Reading the clock per row would cost more than it saves once a batch
        // is large, so it is sampled then. While the batch is small it is
        // checked every row: sampling alone meant a batch that never reached
        // `LINGER_CHECK_EVERY` rows never flushed on its deadline at all,
        // stranding a slow parser's rows until the row cap or end of sheet.
        if (self.rows.len() < LINGER_CHECK_EVERY
            || self.rows.len().is_multiple_of(LINGER_CHECK_EVERY))
            && self.opened_at.is_some_and(|t| t.elapsed() >= self.linger)
        {
            return self.flush(tx);
        }
        true
    }

    /// Place a late-arriving cell into a row that is queued but not yet sent.
    ///
    /// Returns false when the row is out of reach, which is exactly the point
    /// at which the stream can no longer be repaired.
    fn place(&mut self, row_index: u32, col: usize, value: pb::CellData, width: usize) -> bool {
        if self.sent || self.rows.is_empty() {
            return false;
        }
        debug_assert!(
            self.rows
                .windows(2)
                .all(|w| w[0].row_index < w[1].row_index),
            "the queue must stay ascending for the search below to be sound"
        );
        let front = self.rows[0].row_index;
        let at = match self.rows.binary_search_by_key(&row_index, |r| r.row_index) {
            Ok(at) => at,
            Err(at) => {
                // The queue is contiguous by construction, because every
                // interior gap row is pushed as it is passed. So an interior
                // row always exists already; only a row below the front is
                // genuinely new, and it brings its own gap fill with it.
                if row_index >= front {
                    return false;
                }
                for (n, index) in (row_index..front).enumerate() {
                    self.rows.insert(
                        at + n,
                        pb::WorksheetRow {
                            row_index: index,
                            values: vec![convert::empty_cell_data(); width],
                        },
                    );
                }
                at
            }
        };
        let values = &mut self.rows[at].values;
        if col >= values.len() {
            values.resize(col + 1, convert::empty_cell_data());
        }
        values[col] = value;
        // Only an unsorted sheet ever gets here, so the byte budget is simply
        // recomputed rather than tracked through every insert and widen.
        self.pending_bytes = self
            .rows
            .iter()
            .map(|row| approx_row_bytes(&row.values))
            .sum();
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
        self.pending_bytes = 0;
        if !self.send_new_strings(tx) {
            return false;
        }
        // Past this line nothing in the queue can be repaired any more.
        self.sent = true;
        if self.single {
            for row in std::mem::take(&mut self.rows) {
                if !send_event(tx, range_row(row.row_index, row.values)) {
                    return false;
                }
            }
            return true;
        }
        send_event(tx, range_rows(std::mem::take(&mut self.rows)))
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
///
/// `header_row` carries a `HeaderRow::Row(n)` selection, which this path has
/// to apply itself: calamine applies it in `worksheet_range_ref`
/// (xlsx/mod.rs:2652, xlsb/mod.rs:562) and never in `worksheet_cells_reader`
/// (xlsx/mod.rs:2517, xlsb/mod.rs:418), which is the reader streamed here.
fn emit_incremental<E: Display>(
    sheet_name: &str,
    dims: calamine::Dimensions,
    header_row: Option<u32>,
    mut next_cell: impl FnMut() -> Result<Option<(u32, u32, pb::cell_data::Value)>, E>,
    kind: pb::CalamineErrorKind,
    batcher: &mut RowBatcher,
    tx: &mpsc::Sender<Result<pb::StreamWorksheetRangeResponse, Status>>,
) {
    let header = pb::RangeStarted {
        sheet_name: sheet_name.to_string(),
        dimensions: Some(convert::dimensions(dims)),
        total_cells: declared_total_cells(dims),
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
    //
    // The declared end column is also clamped before it becomes an allocation
    // length: it is attacker-controlled and calamine does not bound it, so
    // only `MAX_DECLARED_COLUMNS` of hint is honored. Rows still grow to fit
    // any cell that actually arrives, so this costs at most a few reallocs on
    // a genuinely wide sheet and nothing at all on an honest one.
    // The declaration is a capacity hint and nothing more. `width` is the
    // emitted extent and grows only from cells that actually arrive, so the
    // declaration can never reach the wire or the allocator: `A1:ZZZZZZ1` in a
    // 2 KB upload reserves 16,384 slots it never fills instead of committing
    // ~10 GiB, and a sheet holding one cell streams one cell wide.
    let prealloc = (dims.end.1 as usize).min(MAX_DECLARED_COLUMNS - 1) + 1;
    let mut width = 0usize;
    let mut values: Vec<pb::CellData> = Vec::with_capacity(prealloc);

    // With `HeaderRow::Row(n)` the range begins AT the header row even when
    // that row is blank, because calamine inserts a synthetic empty cell there
    // (xlsx/mod.rs:2702-2713) before building the extent. Opening the walk at
    // `n` with `started` already true reproduces that: rows between the header
    // row and the first row holding a value are held as an interior gap
    // instead of being trimmed as leading padding.
    //
    // `values`/`current_row` describe a real row.
    // `started`: a non-empty row has already been emitted (or the header row
    // stands in for one).
    let (mut current_row, mut open, mut started) = match header_row {
        Some(n) => (n, true, true),
        None => (dims.start.0, false, false),
    };
    // The row under construction holds at least one non-empty cell.
    let mut row_has_value = false;
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
        // Everything above the selected header row is not part of the sheet,
        // matching the `cell.pos.0 >= header_row_idx` filter calamine applies
        // at xlsx/mod.rs:2691 and xlsb/mod.rs:594.
        if header_row.is_some_and(|n| row < n) {
            continue;
        }
        // Empty cells are not part of the sheet. `Range::from_sparse` builds
        // the extent from non-empty cells alone, so a blank neither widens a
        // row nor advances the walk. Dropping them here is what makes leading
        // and trailing styled-blank rows disappear with no special casing, and
        // stops one blank cell far to the right from inflating every row.
        if matches!(value, pb::cell_data::Value::Empty(())) {
            continue;
        }
        let idx = col as usize;

        // A row index that moves backwards. ECMA-376 does not require `<row>`
        // elements to be sorted and the `r` attribute is what fixes position,
        // so this is legal input, not corruption: calamine reads it correctly
        // because `Range::from_sparse` sorts (its own comment says "cells do
        // not always appear in (row, col) order", lib.rs:946). A one-pass
        // stream cannot sort, so it repairs as far back as it has not yet
        // committed, and fails loudly past that rather than dropping the cell.
        if open && row < current_row {
            // Rows in [band_lo, current_row) are completed, known all-empty
            // and still held back as `pending_empty`, so a late cell can
            // simply reopen one. While `!started` nothing has been pushed at
            // all, so the floor is 0.
            let band_lo = if started {
                current_row - pending_empty
            } else {
                0
            };
            if row >= band_lo {
                if idx >= width {
                    width = idx + 1;
                    values.resize(width, convert::empty_cell_data());
                }
                let mut reopened = vec![convert::empty_cell_data(); width];
                reopened[idx] = convert::cell_data(value);
                // Held-back rows below the reopened one are now a proven
                // interior gap. Written out rather than routed through
                // `complete_row!`, whose trailing `row_has_value = false`
                // would clobber the row still under construction at
                // `current_row` and silently drop it.
                let below = if started { row - band_lo } else { 0 };
                for back in (1..=below).rev() {
                    if !batcher.push(tx, row - back, vec![convert::empty_cell_data(); width]) {
                        return;
                    }
                }
                started = true;
                if !batcher.push(tx, row, reopened) {
                    return;
                }
                pending_empty = current_row - row - 1;
                continue;
            }
            // Older than the held-back band, but perhaps still inside the
            // batcher's unsent queue.
            if batcher.place(row, idx, convert::cell_data(value), width) {
                continue;
            }
            return abort_unsorted(tx, kind, sheet_name, row, col);
        }

        if open {
            while current_row < row {
                if row_has_value {
                    let flushed =
                        std::mem::replace(&mut values, vec![convert::empty_cell_data(); width]);
                    complete_row!(current_row, flushed);
                } else {
                    // Nothing was written into this row, so `values` is
                    // already all-empty and the macro will not read the row
                    // argument. Reusing the buffer keeps a gap free of
                    // allocation, which matters because `r=` is not bounded by
                    // calamine: `r="4000000000"` makes this loop run four
                    // billion times.
                    complete_row!(current_row, Vec::new());
                }
                current_row += 1;
                // A gap sends nothing, so `push` never reports the client
                // leaving. Check for it directly, or a caller that hung up
                // long ago still costs a parse thread for hours.
                if current_row.is_multiple_of(GAP_CHECK_EVERY) && tx.is_closed() {
                    return;
                }
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
        row_has_value = true;
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
    // `HeaderRow::FirstNonEmptyRow` is already what the densifier does (it
    // snaps to the first cell and trims leading blanks), so only an explicit
    // row index has to be carried into the incremental path. The buffered
    // path needs nothing: XLS and ODS apply the selection inside
    // `worksheet_range` themselves (xls.rs:430, ods.rs:245).
    let header_row = match entry.header_row {
        Some(HeaderRow::Row(n)) => Some(n),
        _ => None,
    };
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
                        header_row,
                        || {
                            reader.next_cell().map(|opt| {
                                opt.map(|cell| cell_tuple(cell, is_1904, table.as_ref()))
                            })
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
                        header_row,
                        || {
                            reader.next_cell().map(|opt| {
                                opt.map(|cell| cell_tuple(cell, is_1904, table.as_ref()))
                            })
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
        let workbook_id = request.into_inner().workbook_id;
        // An empty workbook_id is the service-level probe: answer with the
        // UiInfo block alone so hosts can discover the web UI without
        // opening a workbook first.
        if workbook_id.is_empty() {
            return Ok(Response::new(pb::GetMetadataResponse {
                ui: Some(ui_info()),
                ..Default::default()
            }));
        }
        let entry = get_entry(&self.store, &workbook_id)?;
        Ok(Response::new(pb::GetMetadataResponse {
            detected_format: entry.format as i32,
            metadata: Some(entry.metadata.clone()),
            ui: Some(ui_info()),
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
        let permit = self.admit()?;
        Ok(spawn_blocking_stream(permit, move |tx| {
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
        let permit = self.admit()?;
        Ok(spawn_blocking_stream(permit, move |tx| {
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
        let permit = self.admit()?;
        Ok(spawn_blocking_stream(permit, move |tx| {
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
        let permit = self.admit()?;
        Ok(spawn_blocking_stream(permit, move |tx| {
            run_get_pictures(&entry, &tx);
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Streaming reads are admitted against a fixed pool of slots, and a
    /// request past the cap is refused immediately rather than queued behind a
    /// stream that may itself be stuck.
    #[test]
    fn streams_past_the_cap_are_refused_not_queued() {
        let service = CalamineGrpc::new(WorkbookStore::new()).with_max_concurrent_streams(2);
        let first = service.admit().expect("slot 1");
        let second = service.admit().expect("slot 2");

        let refused = service.admit().expect_err("the cap is 2");
        assert_eq!(refused.code(), tonic::Code::ResourceExhausted);

        drop(second);
        let reused = service.admit().expect("a released slot is reusable");
        drop((first, reused));
    }

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
