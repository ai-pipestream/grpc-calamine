// SPDX-License-Identifier: Apache-2.0

//! The `CalamineService` gRPC implementation.
//!
//! All calamine work is blocking, CPU-bound parsing; every read runs inside
//! `tokio::task::spawn_blocking` and pushes events into a bounded channel, so
//! slow consumers apply backpressure and many workbooks can stream
//! concurrently. Workbook bytes are kept in memory only.

use std::sync::Arc;

use calamine::{Data, HeaderRow, Range, Reader, Sheets};
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

/// Per-message gRPC frame limit: 32 MiB. Upload clients should chunk well
/// below this (the reference chunking is 64 KiB–1 MiB); the encoding side
/// covers very wide rows.
const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;

/// gRPC implementation of `calamine.v1.CalamineService`.
pub struct CalamineGrpc {
    store: Arc<WorkbookStore>,
    max_workbook_bytes: usize,
}

impl CalamineGrpc {
    /// Create the service around an empty workbook store.
    pub fn new(store: WorkbookStore) -> Self {
        Self {
            store: Arc::new(store),
            max_workbook_bytes: DEFAULT_MAX_WORKBOOK_BYTES,
        }
    }

    /// Override the maximum accepted workbook size in bytes.
    pub fn with_max_workbook_bytes(mut self, max: usize) -> Self {
        self.max_workbook_bytes = max;
        self
    }

    /// Wrap into the generated tonic service, with frame limits sized for
    /// chunked uploads and wide rows.
    pub fn into_service(self) -> CalamineServiceServer<Self> {
        CalamineServiceServer::new(self)
            .max_decoding_message_size(MAX_FRAME_BYTES)
            .max_encoding_message_size(MAX_FRAME_BYTES)
    }
}

/// Resolve a `SheetSelector` to a concrete sheet name.
fn resolve_sheet_name(
    entry: &WorkbookEntry,
    selector: Option<&pb::SheetSelector>,
) -> Result<String, Status> {
    let selector = selector.ok_or_else(|| Status::invalid_argument("sheet selector is required"))?;
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

/// Send one event; returns false when the client has gone away.
fn send_event<T>(tx: &mpsc::Sender<Result<T, Status>>, event: T) -> bool {
    tx.blocking_send(Ok(event)).is_ok()
}

/// Send an in-band error event; returns false when the client has gone away.
fn send_stream_error<E>(
    tx: &mpsc::Sender<Result<E, Status>>,
    wrap: impl FnOnce(pb::StreamError) -> E,
    error: pb::CalamineError,
    terminal: bool,
) -> bool {
    send_event(
        tx,
        wrap(pb::StreamError {
            error: Some(error),
            terminal,
        }),
    )
}

/// Convert a calamine parse failure into an in-band terminal stream error.
fn abort_with<E>(
    tx: &mpsc::Sender<Result<E, Status>>,
    wrap: impl FnOnce(pb::StreamError) -> E,
    kind: pb::CalamineErrorKind,
    err: impl std::fmt::Display,
) {
    let _ = send_stream_error(tx, wrap, convert::calamine_error(kind, err), true);
}

/// Emit the rows of a dense `Range<Data>` (buffered path for XLS and ODS,
/// whose calamine readers do not expose an incremental cell iterator).
fn emit_range(
    sheet_name: &str,
    range: &Range<Data>,
    tx: &mpsc::Sender<Result<pb::StreamWorksheetRangeResponse, Status>>,
) -> bool {
    let (start, end) = match (range.start(), range.end()) {
        (Some(s), Some(e)) => (s, e),
        _ => {
            // Empty range: header without dimensions, then end of stream.
            return send_event(
                tx,
                pb::StreamWorksheetRangeResponse {
                    event: Some(pb::stream_worksheet_range_response::Event::Started(
                        pb::RangeStarted {
                            sheet_name: sheet_name.to_string(),
                            dimensions: None,
                            total_cells: 0,
                        },
                    )),
                },
            );
        }
    };

    let (height, width) = range.get_size();
    let started = pb::RangeStarted {
        sheet_name: sheet_name.to_string(),
        dimensions: Some(pb::Dimensions {
            start: Some(convert::cell_position(start)),
            end: Some(convert::cell_position(end)),
        }),
        total_cells: (height * width) as u64,
    };
    if !send_event(
        tx,
        pb::StreamWorksheetRangeResponse {
            event: Some(pb::stream_worksheet_range_response::Event::Started(started)),
        },
    ) {
        return false;
    }

    for (offset, row) in range.rows().enumerate() {
        let event = pb::WorksheetRow {
            row_index: start.0 + offset as u32,
            values: row
                .iter()
                .map(|d| convert::cell_data(convert::data_value(d)))
                .collect(),
        };
        if !send_event(
            tx,
            pb::StreamWorksheetRangeResponse {
                event: Some(pb::stream_worksheet_range_response::Event::Row(event)),
            },
        ) {
            return false;
        }
    }
    true
}

/// Stream cells from an incremental calamine cell reader (XLSX and XLSB),
/// densifying the sparse cell stream into full rows as they arrive.
///
/// Rows are emitted densely from the declared start row up to the last row
/// that actually contains a cell: gaps between populated rows are filled
/// with empty rows, and trailing declared-but-absent rows are not emitted.
/// `next_cell` yields `Ok(None)` at end of sheet.
fn emit_incremental<E>(
    sheet_name: &str,
    dims: calamine::Dimensions,
    mut next_cell: impl FnMut() -> Result<Option<(u32, u32, pb::cell_data::Value)>, E>,
    kind: pb::CalamineErrorKind,
    tx: &mpsc::Sender<Result<pb::StreamWorksheetRangeResponse, Status>>,
) where
    E: std::fmt::Display,
{
    let started = pb::RangeStarted {
        sheet_name: sheet_name.to_string(),
        dimensions: Some(convert::dimensions(dims)),
        total_cells: dims.len(),
    };
    if !send_event(
        tx,
        pb::StreamWorksheetRangeResponse {
            event: Some(pb::stream_worksheet_range_response::Event::Started(started)),
        },
    ) {
        return;
    }

    // Degenerate or inverted dimensions: nothing to stream.
    if dims.end.0 < dims.start.0 || dims.end.1 < dims.start.1 {
        return;
    }
    let width = (dims.end.1 - dims.start.1 + 1) as usize;
    let mut current_row = dims.start.0;
    let mut values = vec![convert::empty_cell_data(); width];
    let mut row_touched = false;

    loop {
        let cell = match next_cell() {
            Ok(Some(cell)) => cell,
            Ok(None) => break,
            Err(e) => {
                abort_with(
                    tx,
                    |se| pb::StreamWorksheetRangeResponse {
                        event: Some(pb::stream_worksheet_range_response::Event::Error(se)),
                    },
                    kind,
                    e,
                );
                return;
            }
        };
        let (row, col, value) = cell;

        // Defensive: ignore cells outside the declared dimensions.
        if row < dims.start.0 || row > dims.end.0 || col < dims.start.1 || col > dims.end.1 {
            continue;
        }

        // Flush the current row and any fully-empty gap rows before it.
        while current_row < row {
            if !send_event(
                tx,
                pb::StreamWorksheetRangeResponse {
                    event: Some(pb::stream_worksheet_range_response::Event::Row(
                        pb::WorksheetRow {
                            row_index: current_row,
                            values: std::mem::replace(
                                &mut values,
                                vec![convert::empty_cell_data(); width],
                            ),
                        },
                    )),
                },
            ) {
                return;
            }
            current_row += 1;
        }

        values[(col - dims.start.1) as usize] = convert::cell_data(value);
        row_touched = true;
    }

    // Flush the final row (only if any cell was ever seen, so an empty sheet
    // emits just the header).
    if row_touched && !send_event(
        tx,
        pb::StreamWorksheetRangeResponse {
            event: Some(pb::stream_worksheet_range_response::Event::Row(
                pb::WorksheetRow {
                    row_index: current_row,
                    values,
                },
            )),
        },
    ) {
    }
}

/// Convert one `Cell<DataRef>` from an incremental reader into the tuple
/// form used by `emit_incremental`.
fn cell_tuple(cell: calamine::Cell<calamine::DataRef<'_>>) -> (u32, u32, pb::cell_data::Value) {
    let (row, col) = cell.get_position();
    (row, col, convert::data_ref_value(cell.get_value()))
}

/// The blocking body of `StreamWorksheetRange`.
///
/// XLSX and XLSB use calamine's incremental cell readers, so rows are sent
/// as the parser walks the sheet. XLS and ODS only offer whole-range
/// parsing, so the range is parsed and then streamed row by row.
fn run_stream_worksheet_range(
    entry: Arc<WorkbookEntry>,
    selector: Option<pb::SheetSelector>,
    tx: mpsc::Sender<Result<pb::StreamWorksheetRangeResponse, Status>>,
) {
    let sheet_name = match resolve_sheet_name(&entry, selector.as_ref()) {
        Ok(name) => name,
        Err(status) => {
            let _ = tx.blocking_send(Err(status));
            return;
        }
    };
    let kind = convert::error_kind_for_format(entry.format);
    let wrap = |se: pb::StreamError| pb::StreamWorksheetRangeResponse {
        event: Some(pb::stream_worksheet_range_response::Event::Error(se)),
    };
    // Fresh independent reader: no locks, fully parallel with other reads.
    let mut workbook = match entry.reader() {
        Ok(workbook) => workbook,
        Err(e) => return abort_with(&tx, wrap, kind, e),
    };

    match &mut workbook {
        Sheets::Xlsx(x) => {
            let mut reader = match x.worksheet_cells_reader(&sheet_name) {
                Ok(r) => r,
                Err(e) => return abort_with(&tx, wrap, kind, e),
            };
            let dims = reader.dimensions();
            emit_incremental(
                &sheet_name,
                dims,
                || reader.next_cell().map(|opt| opt.map(cell_tuple)),
                kind,
                &tx,
            );
        }
        Sheets::Xlsb(x) => {
            let mut reader = match x.worksheet_cells_reader(&sheet_name) {
                Ok(r) => r,
                Err(e) => return abort_with(&tx, wrap, kind, e),
            };
            let dims = reader.dimensions();
            emit_incremental(
                &sheet_name,
                dims,
                || reader.next_cell().map(|opt| opt.map(cell_tuple)),
                kind,
                &tx,
            );
        }
        other => {
            let range = match other.worksheet_range(&sheet_name) {
                Ok(range) => range,
                Err(e) => return abort_with(&tx, wrap, kind, e),
            };
            emit_range(&sheet_name, &range, &tx);
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
        let options = match first.payload {
            Some(pb::open_workbook_request::Payload::Options(options)) => options,
            _ => {
                return Err(Status::invalid_argument(
                    "first upload frame must carry `options`",
                ))
            }
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
                    ))
                }
            }
        }
        if bytes.is_empty() {
            return Err(Status::invalid_argument("upload contained no file bytes"));
        }

        let store = Arc::clone(&self.store);
        let (id, entry) = tokio::task::spawn_blocking(move || {
            store.open(bytes, format_hint, header_row)
        })
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

    type StreamWorksheetRangeStream =
        ReceiverStream<Result<pb::StreamWorksheetRangeResponse, Status>>;

    async fn stream_worksheet_range(
        &self,
        request: Request<pb::StreamWorksheetRangeRequest>,
    ) -> Result<Response<Self::StreamWorksheetRangeStream>, Status> {
        let req = request.into_inner();
        let entry = get_entry(&self.store, &req.workbook_id)?;
        let (tx, rx) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
        tokio::task::spawn_blocking(move || run_stream_worksheet_range(entry, req.sheet, tx));
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    type StreamWorksheetFormulaStream =
        ReceiverStream<Result<pb::StreamWorksheetFormulaResponse, Status>>;

    async fn stream_worksheet_formula(
        &self,
        request: Request<pb::StreamWorksheetFormulaRequest>,
    ) -> Result<Response<Self::StreamWorksheetFormulaStream>, Status> {
        let req = request.into_inner();
        let entry = get_entry(&self.store, &req.workbook_id)?;
        let (tx, rx) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
        tokio::task::spawn_blocking(move || {
            let sheet_name = match resolve_sheet_name(&entry, req.sheet.as_ref()) {
                Ok(name) => name,
                Err(status) => {
                    let _ = tx.blocking_send(Err(status));
                    return;
                }
            };
            let kind = convert::error_kind_for_format(entry.format);
            let wrap = |se: pb::StreamError| pb::StreamWorksheetFormulaResponse {
                event: Some(pb::stream_worksheet_formula_response::Event::Error(se)),
            };
            let mut workbook = match entry.reader() {
                Ok(workbook) => workbook,
                Err(e) => return abort_with(&tx, wrap, kind, e),
            };
            let range = match workbook.worksheet_formula(&sheet_name) {
                Ok(range) => range,
                Err(e) => return abort_with(&tx, wrap, kind, e),
            };
            drop(workbook);

            let (start, end) = match (range.start(), range.end()) {
                (Some(s), Some(e)) => (s, e),
                _ => {
                    let _ = send_event(
                        &tx,
                        pb::StreamWorksheetFormulaResponse {
                            event: Some(
                                pb::stream_worksheet_formula_response::Event::Started(
                                    pb::RangeStarted {
                                        sheet_name,
                                        dimensions: None,
                                        total_cells: 0,
                                    },
                                ),
                            ),
                        },
                    );
                    return;
                }
            };
            let (height, width) = range.get_size();
            if !send_event(
                &tx,
                pb::StreamWorksheetFormulaResponse {
                    event: Some(pb::stream_worksheet_formula_response::Event::Started(
                        pb::RangeStarted {
                            sheet_name: sheet_name.clone(),
                            dimensions: Some(pb::Dimensions {
                                start: Some(convert::cell_position(start)),
                                end: Some(convert::cell_position(end)),
                            }),
                            total_cells: (height * width) as u64,
                        },
                    )),
                },
            ) {
                return;
            }
            for (offset, row) in range.rows().enumerate() {
                if !send_event(
                    &tx,
                    pb::StreamWorksheetFormulaResponse {
                        event: Some(pb::stream_worksheet_formula_response::Event::Row(
                            pb::FormulaRow {
                                row_index: start.0 + offset as u32,
                                formulas: row.to_vec(),
                            },
                        )),
                    },
                ) {
                    return;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
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

    type StreamVbaProjectStream = ReceiverStream<Result<pb::StreamVbaProjectResponse, Status>>;

    async fn stream_vba_project(
        &self,
        request: Request<pb::StreamVbaProjectRequest>,
    ) -> Result<Response<Self::StreamVbaProjectStream>, Status> {
        let req = request.into_inner();
        let entry = get_entry(&self.store, &req.workbook_id)?;
        let (tx, rx) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
        tokio::task::spawn_blocking(move || {
            let kind = convert::error_kind_for_format(entry.format);
            let wrap = |se: pb::StreamError| pb::StreamVbaProjectResponse {
                event: Some(pb::stream_vba_project_response::Event::Error(se)),
            };
            let mut workbook = match entry.reader() {
                Ok(workbook) => workbook,
                Err(e) => return abort_with(&tx, wrap, kind, e),
            };
            let project = match workbook.vba_project() {
                Ok(Some(project)) => project,
                Ok(None) => {
                    let _ = send_event(
                        &tx,
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
                Err(e) => return abort_with(&tx, wrap, kind, e),
            };
            drop(workbook);

            let module_names: Vec<String> = project
                .get_module_names()
                .into_iter()
                .map(str::to_string)
                .collect();
            if !send_event(
                &tx,
                pb::StreamVbaProjectResponse {
                    event: Some(pb::stream_vba_project_response::Event::Info(
                        pb::VbaProjectInfo {
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
                        },
                    )),
                },
            ) {
                return;
            }

            for name in module_names {
                match project.get_module_raw(&name) {
                    Ok(raw) => {
                        if !send_event(
                            &tx,
                            pb::StreamVbaProjectResponse {
                                event: Some(pb::stream_vba_project_response::Event::Module(
                                    pb::VbaModule {
                                        name,
                                        raw_content: raw.to_vec(),
                                    },
                                )),
                            },
                        ) {
                            return;
                        }
                    }
                    Err(e) => {
                        // Per-module failures are non-terminal: remaining
                        // modules can still be delivered.
                        if !send_stream_error(
                            &tx,
                            wrap,
                            convert::calamine_error(pb::CalamineErrorKind::Vba, e),
                            false,
                        ) {
                            return;
                        }
                    }
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    type GetPicturesStream = ReceiverStream<Result<pb::GetPicturesResponse, Status>>;

    async fn get_pictures(
        &self,
        request: Request<pb::GetPicturesRequest>,
    ) -> Result<Response<Self::GetPicturesStream>, Status> {
        let req = request.into_inner();
        let entry = get_entry(&self.store, &req.workbook_id)?;
        let (tx, rx) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
        tokio::task::spawn_blocking(move || {
            let wrap = |se: pb::StreamError| pb::GetPicturesResponse {
                event: Some(pb::get_pictures_response::Event::Error(se)),
            };
            let kind = convert::error_kind_for_format(entry.format);
            let workbook = match entry.reader() {
                Ok(workbook) => workbook,
                Err(e) => return abort_with(&tx, wrap, kind, e),
            };
            for pic in workbook.pictures_with_metadata() {
                if !send_event(
                    &tx,
                    pb::GetPicturesResponse {
                        event: Some(pb::get_pictures_response::Event::Picture(pb::Picture {
                            row: pic.row,
                            col: pic.col,
                            sheet_name: pic.sheet_name,
                            extension: pic.extension,
                            data: pic.data,
                            name: pic.name,
                        })),
                    },
                ) {
                    return;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}
