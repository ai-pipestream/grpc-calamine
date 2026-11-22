// SPDX-License-Identifier: Apache-2.0

//! The `ParseWorkbook` bidirectional stream: chunks in, every requested part
//! of the workbook out, and no handle.
//!
//! The upload is received whole first, because a workbook is a ZIP (or an OLE
//! compound file) whose directory sits at the end, so nothing can be read
//! until the last byte is in. The workbook is then opened without being
//! registered ([`WorkbookStore::with_transient`]), and each part runs the
//! very body its own RPC runs, on the same blocking thread, into a channel of
//! that RPC's response type. A forwarder wraps each event into the combined
//! response, so every part streams byte for byte what the per-part RPC would
//! have streamed and the two can never drift apart. The workbook goes when
//! the stream does, however it ends.

use std::sync::{Arc, OnceLock};

use calamine::HeaderRow;
use tokio::sync::mpsc;
use tonic::{Status, Streaming};

use super::{
    CONSUMER_STALL, STALL_REASON, STREAM_CHANNEL_CAPACITY, accept_chunk, next_frame, open_options,
    run_get_pictures, run_stream_vba_project, run_stream_worksheet_formula,
    run_stream_worksheet_range, send_event, store_error_status,
};
use crate::proto::v1 as pb;
use crate::store::{WorkbookEntry, WorkbookStore};

use pb::parse_workbook_request::Frame;
use pb::parse_workbook_response::Event;

/// The sending half of a `ParseWorkbook` response stream.
type Tx = mpsc::Sender<Result<pb::ParseWorkbookResponse, Status>>;

/// Why a stream stops early. Set once, by whichever side notices first.
enum Stop {
    /// The client sent `cancel`.
    Cancelled,
    /// The client broke the frame order after the upload.
    Refused(Status),
}

/// Shared between the parse and the task that watches the request stream.
type StopSignal = Arc<OnceLock<Stop>>;

/// What one `ParseWorkbook` call needs from the service, owned so the call
/// can run on its own task.
pub(super) struct ParseJob {
    pub(super) store: Arc<WorkbookStore>,
    pub(super) upload_stall: std::time::Duration,
    pub(super) upload_deadline: std::time::Duration,
    pub(super) max_workbook_bytes: usize,
    pub(super) max_dense_cells: u64,
    pub(super) max_formula_bytes: u64,
}

/// A whole upload, ready to open.
struct Upload {
    open: pb::ParseWorkbookOpen,
    format_hint: pb::WorkbookFormat,
    header_row: Option<HeaderRow>,
    bytes: Vec<u8>,
    /// The request stream, when the upload ended on `last` rather than on a
    /// half-close, so a `cancel` can still arrive on it.
    rest: Option<Streaming<pb::ParseWorkbookRequest>>,
}

/// A caller's limit tightened against the server's: zero means the server's
/// own, and nothing can raise it.
fn tighten(server: u64, asked: u64) -> u64 {
    if asked == 0 {
        server
    } else {
        asked.min(server)
    }
}

impl ParseJob {
    /// Run the whole call: receive, open, emit, release.
    ///
    /// `upload_permit` is released once the workbook is parsed, as on
    /// `OpenWorkbook`; `stream_permit` once the last event has gone.
    pub(super) async fn run(
        self,
        stream: Streaming<pb::ParseWorkbookRequest>,
        upload_permit: tokio::sync::OwnedSemaphorePermit,
        stream_permit: tokio::sync::OwnedSemaphorePermit,
        tx: Tx,
    ) {
        let receive = self.receive(stream);
        let received = if self.upload_deadline.is_zero() {
            receive.await
        } else {
            tokio::time::timeout(self.upload_deadline, receive)
                .await
                .unwrap_or_else(|_| {
                    Err(Status::deadline_exceeded(format!(
                        "the upload did not finish within {:?}; abandoning it so its \
                         upload slot can be reused (GRPC_CALAMINE_UPLOAD_DEADLINE_SECS)",
                        self.upload_deadline
                    )))
                })
        };
        let upload = match received {
            Ok(Some(upload)) => upload,
            Ok(None) => {
                let done = pb::ParseWorkbookDone {
                    cancelled: true,
                    ..Default::default()
                };
                let _ = tx.send(Ok(done_event(done))).await;
                return;
            }
            Err(status) => {
                let _ = tx.send(Err(status)).await;
                return;
            }
        };

        let stop = StopSignal::default();
        let Upload {
            open,
            format_hint,
            header_row,
            bytes,
            rest,
        } = upload;
        let watcher = rest.map(|rest| tokio::spawn(watch_for_cancel(rest, Arc::clone(&stop))));
        let max_dense_cells = tighten(
            self.max_dense_cells,
            open.limits.as_ref().map_or(0, |l| l.max_dense_cells),
        );
        let max_formula_bytes = tighten(
            self.max_formula_bytes,
            open.limits.as_ref().map_or(0, |l| l.max_formula_bytes),
        );
        let store = self.store;
        let supervisor = super::spawn_blocking_onto(stream_permit, tx, move |tx| {
            let opened = store.with_transient(bytes, format_hint, header_row, |entry| {
                // Parsed: the upload no longer holds anything an upload slot
                // accounts for.
                drop(upload_permit);
                emit_workbook(entry, &open, max_dense_cells, max_formula_bytes, &stop, &tx);
            });
            if let Err(e) = opened {
                let _ = tx.blocking_send(Err(store_error_status(e)));
            }
        });
        let _ = supervisor.await;
        if let Some(watcher) = watcher {
            watcher.abort();
        }
    }

    /// Read the `open` frame and the file bytes. `None` when the client
    /// cancelled before the upload ended.
    async fn receive(
        &self,
        mut stream: Streaming<pb::ParseWorkbookRequest>,
    ) -> Result<Option<Upload>, Status> {
        let first = next_frame(&mut stream, self.upload_stall)
            .await?
            .ok_or_else(|| Status::invalid_argument("empty upload: no frames received"))?;
        let Some(Frame::Open(open)) = first.frame else {
            return Err(Status::invalid_argument(
                "first ParseWorkbook frame must carry `open`",
            ));
        };
        let (format_hint, header_row) = open_options(&open.options.unwrap_or_default())?;
        let max = tighten(
            self.max_workbook_bytes as u64,
            open.limits.as_ref().map_or(0, |l| l.max_workbook_bytes),
        );
        let max = usize::try_from(max).unwrap_or(usize::MAX);

        let mut bytes = Vec::new();
        let mut rest = None;
        while let Some(frame) = next_frame(&mut stream, self.upload_stall).await? {
            match frame.frame {
                Some(Frame::Chunk(chunk)) => {
                    // A frame that carries nothing would let a client keep an
                    // upload alive without ever sending the workbook; only the
                    // closing marker may be empty.
                    if chunk.data.is_empty() && !chunk.last {
                        return Err(Status::invalid_argument(
                            "an upload chunk must carry at least one byte unless it is `last`",
                        ));
                    }
                    accept_chunk(&self.store, max, &mut bytes, &chunk.data)?;
                    if chunk.last {
                        rest = Some(stream);
                        break;
                    }
                }
                Some(Frame::Cancel(_)) => return Ok(None),
                _ => {
                    return Err(Status::invalid_argument(
                        "`open` must be the first and only open frame",
                    ));
                }
            }
        }
        if bytes.is_empty() {
            return Err(Status::invalid_argument("upload contained no file bytes"));
        }
        Ok(Some(Upload {
            open,
            format_hint,
            header_row,
            bytes,
            rest,
        }))
    }
}

/// Watch what the client sends after its last chunk: a `cancel` stops the
/// parse, anything else refuses it. A client that goes away needs nothing
/// here: its response stream closes too, and that stops the parse.
async fn watch_for_cancel(mut rest: Streaming<pb::ParseWorkbookRequest>, stop: StopSignal) {
    let stopped = match rest.message().await {
        Ok(Some(pb::ParseWorkbookRequest {
            frame: Some(Frame::Cancel(_)),
        })) => Stop::Cancelled,
        Ok(Some(_)) => Stop::Refused(Status::invalid_argument(
            "only `cancel` may follow the chunk marked `last`",
        )),
        Ok(None) | Err(_) => return,
    };
    let _ = stop.set(stopped);
}

/// Wrap the terminal event.
fn done_event(done: pb::ParseWorkbookDone) -> pb::ParseWorkbookResponse {
    pb::ParseWorkbookResponse {
        event: Some(Event::Done(done)),
    }
}

/// The selected sheets as positions in workbook order. Empty selects every
/// sheet.
fn resolve_selection(
    entry: &WorkbookEntry,
    selectors: &[pb::SheetSelector],
) -> Result<Vec<u32>, Status> {
    let sheets = &entry.metadata.sheets;
    if selectors.is_empty() {
        return Ok((0..sheets.len() as u32).collect());
    }
    selectors
        .iter()
        .map(|selector| match &selector.selector {
            Some(pb::sheet_selector::Selector::SheetName(name)) => sheets
                .iter()
                .position(|sheet| &sheet.name == name)
                .map(|i| i as u32)
                .ok_or_else(|| Status::not_found(format!("no sheet named {name:?}"))),
            Some(pb::sheet_selector::Selector::SheetIndex(i)) if (*i as usize) < sheets.len() => {
                Ok(*i)
            }
            Some(pb::sheet_selector::Selector::SheetIndex(i)) => {
                Err(Status::not_found(format!("no sheet at index {i}")))
            }
            None => Err(Status::invalid_argument("sheet selector is empty")),
        })
        .collect()
}

/// What a part's events add to the `done` counts.
trait PartEvent: Send + 'static {
    /// Whether the event is an in-band error, and whether that error ends
    /// the part.
    fn error(&self) -> Option<bool>;
    /// Whether the event carries one counted item: a picture or a module.
    fn is_item(&self) -> bool {
        false
    }
}

impl PartEvent for pb::StreamWorksheetRangeResponse {
    fn error(&self) -> Option<bool> {
        match &self.event {
            Some(pb::stream_worksheet_range_response::Event::Error(e)) => Some(e.terminal),
            _ => None,
        }
    }
}

impl PartEvent for pb::StreamWorksheetFormulaResponse {
    fn error(&self) -> Option<bool> {
        match &self.event {
            Some(pb::stream_worksheet_formula_response::Event::Error(e)) => Some(e.terminal),
            _ => None,
        }
    }
}

impl PartEvent for pb::GetPicturesResponse {
    fn error(&self) -> Option<bool> {
        match &self.event {
            Some(pb::get_pictures_response::Event::Error(e)) => Some(e.terminal),
            _ => None,
        }
    }
    fn is_item(&self) -> bool {
        matches!(
            self.event,
            Some(pb::get_pictures_response::Event::Picture(_))
        )
    }
}

impl PartEvent for pb::StreamVbaProjectResponse {
    fn error(&self) -> Option<bool> {
        match &self.event {
            Some(pb::stream_vba_project_response::Event::Error(e)) => Some(e.terminal),
            _ => None,
        }
    }
    fn is_item(&self) -> bool {
        matches!(
            self.event,
            Some(pb::stream_vba_project_response::Event::Module(_))
        )
    }
}

/// What one part sent.
#[derive(Default)]
struct Tally {
    errors: u32,
    terminal: bool,
    items: u32,
}

/// How a part ended, when it did not simply run to its end.
enum Halt {
    /// A stop was requested; the stream itself is still open.
    Stopped,
    /// The stream is over: a terminal status has gone out, or the client
    /// left or stopped reading.
    Ended,
    /// The client read nothing for [`CONSUMER_STALL`]. Only the forwarder
    /// reports it; [`forward_part`] turns it into [`Halt::Ended`] once the
    /// reason is recorded for the supervisor.
    Stalled,
}

/// Run one part's body into a channel of its own response type, forwarding
/// each event wrapped into the combined stream.
fn forward_part<T: PartEvent>(
    tx: &Tx,
    stop: &StopSignal,
    tally: &mut Tally,
    wrap: impl Fn(T) -> pb::ParseWorkbookResponse + Send + 'static,
    body: impl FnOnce(&mpsc::Sender<Result<T, Status>>),
) -> Result<(), Halt> {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return Err(Halt::Ended);
    };
    let (inner_tx, mut inner_rx) = mpsc::channel::<Result<T, Status>>(STREAM_CHANNEL_CAPACITY);
    let outer = tx.clone();
    let signal = Arc::clone(stop);
    // Dropping the inner receiver is what stops the body: its next send
    // fails, exactly as when a client leaves a per-part RPC.
    let forwarder = runtime.spawn(async move {
        let mut tally = Tally::default();
        while let Some(item) = inner_rx.recv().await {
            if signal.get().is_some() {
                return (tally, Err(Halt::Stopped));
            }
            let event = match item {
                Ok(event) => event,
                Err(status) => {
                    let _ = outer.send(Err(status)).await;
                    return (tally, Err(Halt::Ended));
                }
            };
            if let Some(terminal) = event.error() {
                tally.errors += 1;
                tally.terminal |= terminal;
            }
            if event.is_item() {
                tally.items += 1;
            }
            match outer.send_timeout(Ok(wrap(event)), CONSUMER_STALL).await {
                Ok(()) => {}
                Err(mpsc::error::SendTimeoutError::Closed(_)) => return (tally, Err(Halt::Ended)),
                Err(mpsc::error::SendTimeoutError::Timeout(_)) => {
                    return (tally, Err(Halt::Stalled));
                }
            }
        }
        (tally, Ok(()))
    });
    body(&inner_tx);
    drop(inner_tx);
    let (part, outcome) = runtime
        .block_on(forwarder)
        .unwrap_or_else(|_| (Tally::default(), Err(Halt::Ended)));
    tally.errors += part.errors;
    tally.terminal = part.terminal;
    tally.items += part.items;
    // A consumer that stopped reading ends the stream the way it ends a
    // per-part RPC: the reason goes in this thread's stall slot, and the
    // supervisor delivers it.
    if matches!(outcome, Err(Halt::Stalled)) {
        STALL_REASON.with(|reason| {
            reason.borrow_mut().get_or_insert_with(|| {
                Status::deadline_exceeded(format!(
                    "the client read nothing for {} seconds; abandoning the stream \
                     so its parser thread can be reused",
                    CONSUMER_STALL.as_secs()
                ))
            });
        });
    }
    // The body gives up on a stalled consumer itself too, and records why.
    if STALL_REASON.with(|reason| reason.borrow().is_some()) {
        return Err(Halt::Ended);
    }
    outcome
}

/// End the stream for a requested stop: a `done` marked cancelled for a
/// cancel, the refusal's status for a frame out of order. True when a stop
/// was requested.
fn halt_if_stopped(stop: &StopSignal, done: &mut pb::ParseWorkbookDone, tx: &Tx) -> bool {
    match stop.get() {
        None => false,
        Some(Stop::Cancelled) => {
            done.cancelled = true;
            let _ = send_event(tx, done_event(std::mem::take(done)));
            true
        }
        Some(Stop::Refused(status)) => {
            let _ = tx.blocking_send(Err(status.clone()));
            true
        }
    }
}

/// Emit every requested part of an opened workbook, in the order the
/// contract fixes, then `done`.
fn emit_workbook(
    entry: &WorkbookEntry,
    open: &pb::ParseWorkbookOpen,
    max_dense_cells: u64,
    max_formula_bytes: u64,
    stop: &StopSignal,
    tx: &Tx,
) {
    let mut done = pb::ParseWorkbookDone::default();
    if halt_if_stopped(stop, &mut done, tx) {
        return;
    }
    // Every selector is checked before the first event, so a typo fails the
    // call instead of arriving as a part that is missing at the end.
    let sheets = match resolve_selection(entry, &open.sheets) {
        Ok(sheets) => sheets,
        Err(status) => {
            let _ = tx.blocking_send(Err(status));
            return;
        }
    };
    let metadata = pb::WorkbookMetadata {
        detected_format: entry.format as i32,
        metadata: Some(entry.metadata.clone()),
    };
    if !send_event(
        tx,
        pb::ParseWorkbookResponse {
            event: Some(Event::Metadata(metadata)),
        },
    ) {
        return;
    }
    if open.emit_defined_names {
        let names = pb::WorkbookDefinedNames {
            defined_names: entry.metadata.defined_names.clone(),
        };
        if !send_event(
            tx,
            pb::ParseWorkbookResponse {
                event: Some(Event::DefinedNames(names)),
            },
        ) {
            return;
        }
    }

    // A part that halts ends the walk: a stop gets its `done`, anything else
    // has already ended the stream.
    let halted = |halt: Halt, done: &mut pb::ParseWorkbookDone| {
        if matches!(halt, Halt::Stopped) {
            halt_if_stopped(stop, done, tx);
        }
    };
    let range_options = open.range_options.unwrap_or_default();
    for sheet_index in sheets {
        let selector = pb::SheetSelector {
            selector: Some(pb::sheet_selector::Selector::SheetIndex(sheet_index)),
        };
        if open.emit_ranges {
            let mut tally = Tally::default();
            let sent = forward_part(
                tx,
                stop,
                &mut tally,
                move |event| pb::ParseWorkbookResponse {
                    event: Some(Event::Range(pb::SheetRangeEvent {
                        sheet_index,
                        event: Some(event),
                    })),
                },
                |inner| {
                    run_stream_worksheet_range(
                        entry,
                        Some(&selector),
                        range_options.max_rows_per_message,
                        range_options.use_string_table,
                        range_options.include_formulas,
                        max_dense_cells,
                        inner,
                    );
                },
            );
            done.part_errors += tally.errors;
            if let Err(halt) = sent {
                return halted(halt, &mut done);
            }
            if !tally.terminal {
                done.range_sheets += 1;
            }
        }
        if open.emit_formulas {
            let mut tally = Tally::default();
            let sent = forward_part(
                tx,
                stop,
                &mut tally,
                move |event| pb::ParseWorkbookResponse {
                    event: Some(Event::Formula(pb::SheetFormulaEvent {
                        sheet_index,
                        event: Some(event),
                    })),
                },
                |inner| {
                    run_stream_worksheet_formula(
                        entry,
                        Some(&selector),
                        max_dense_cells,
                        max_formula_bytes,
                        inner,
                    );
                },
            );
            done.part_errors += tally.errors;
            if let Err(halt) = sent {
                return halted(halt, &mut done);
            }
            if !tally.terminal {
                done.formula_sheets += 1;
            }
        }
        if halt_if_stopped(stop, &mut done, tx) {
            return;
        }
    }

    if open.emit_pictures {
        let mut tally = Tally::default();
        let sent = forward_part(
            tx,
            stop,
            &mut tally,
            |event| pb::ParseWorkbookResponse {
                event: Some(Event::Picture(event)),
            },
            |inner| run_get_pictures(entry, inner),
        );
        done.part_errors += tally.errors;
        done.pictures += tally.items;
        if let Err(halt) = sent {
            return halted(halt, &mut done);
        }
    }
    if open.emit_vba {
        let mut tally = Tally::default();
        let sent = forward_part(
            tx,
            stop,
            &mut tally,
            |event| pb::ParseWorkbookResponse {
                event: Some(Event::Vba(event)),
            },
            |inner| run_stream_vba_project(entry, inner),
        );
        done.part_errors += tally.errors;
        done.vba_modules += tally.items;
        if let Err(halt) = sent {
            return halted(halt, &mut done);
        }
    }

    if halt_if_stopped(stop, &mut done, tx) {
        return;
    }
    let _ = send_event(tx, done_event(done));
}
