// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests: a real tonic server on an ephemeral port, the generated
//! protobuf client, and real workbook files from the calamine test suite.
//!
//! Every streamed cell is compared against calamine's own `worksheet_range`
//! output, so the tests assert that the wire stream is a faithful rendering
//! of what calamine parsed.

use std::path::PathBuf;
use std::time::Duration;

use calamine::{Data, HeaderRow, Reader, Sheets, open_workbook_auto};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::transport::{Endpoint, Server};
use tonic::{Code, Status};

use grpc_calamine::archive::InflateLimits;
use grpc_calamine::proto::v1 as pb;
use grpc_calamine::proto::v1::calamine_service_client::CalamineServiceClient;
use grpc_calamine::store::StoreLimits;
use grpc_calamine::{CalamineGrpc, WorkbookStore, convert};

/// Directory holding the workbook fixtures (originally from the calamine
/// test suite, MIT licensed).
fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("demos/sample-data")
}

/// Start the server on an ephemeral localhost port and return a connected
/// client.
async fn start_server() -> CalamineServiceClient<tonic::transport::Channel> {
    start_server_with(CalamineGrpc::new(WorkbookStore::new())).await
}

/// Start `grpc`, and its idle reaper, on an ephemeral localhost port and
/// return a connected client.
async fn start_server_with(grpc: CalamineGrpc) -> CalamineServiceClient<tonic::transport::Channel> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().unwrap();
    let _reaper = grpc.spawn_reaper();
    let service = grpc.into_service();
    tokio::spawn(async move {
        Server::builder()
            .add_service(service)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .expect("server failed");
    });
    // The listener is already bound, so connect cannot race the serve call.
    let channel = Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .expect("connect to server");
    CalamineServiceClient::new(channel)
}

/// Upload a workbook file in 64 KiB chunks and return the open response.
async fn upload(
    client: &CalamineServiceClient<tonic::transport::Channel>,
    file: &str,
) -> pb::OpenWorkbookResponse {
    upload_with_options(client, file, default_options()).await
}

/// Upload a workbook file with explicit open-time options.
async fn upload_with_options(
    client: &CalamineServiceClient<tonic::transport::Channel>,
    file: &str,
    options: pb::WorkbookOptions,
) -> pb::OpenWorkbookResponse {
    let bytes = std::fs::read(fixtures().join(file)).expect("read fixture");
    try_upload_bytes(client, bytes, options)
        .await
        .expect("open workbook")
}

/// Upload a workbook file and return whatever the server answers.
async fn try_upload(
    client: &CalamineServiceClient<tonic::transport::Channel>,
    file: &str,
) -> Result<pb::OpenWorkbookResponse, Status> {
    let bytes = std::fs::read(fixtures().join(file)).expect("read fixture");
    try_upload_bytes(client, bytes, default_options()).await
}

/// The open-time options every test uses unless it says otherwise.
fn default_options() -> pb::WorkbookOptions {
    pb::WorkbookOptions {
        format_hint: pb::WorkbookFormat::Unspecified as i32,
        header_row: None,
    }
}

/// Upload workbook bytes in 64 KiB chunks and return whatever the server
/// answers.
async fn try_upload_bytes(
    client: &CalamineServiceClient<tonic::transport::Channel>,
    bytes: Vec<u8>,
    options: pb::WorkbookOptions,
) -> Result<pb::OpenWorkbookResponse, Status> {
    let mut client = client.clone();
    let mut frames = vec![pb::OpenWorkbookRequest {
        payload: Some(pb::open_workbook_request::Payload::Options(options)),
    }];
    frames.extend(
        bytes
            .chunks(64 * 1024)
            .map(|chunk| pb::OpenWorkbookRequest {
                payload: Some(pb::open_workbook_request::Payload::Chunk(chunk.to_vec())),
            }),
    );
    client
        .open_workbook(tokio_stream::iter(frames))
        .await
        .map(tonic::Response::into_inner)
}

/// Stream a whole worksheet by index and return (header, rows).
async fn stream_range(
    client: &CalamineServiceClient<tonic::transport::Channel>,
    workbook_id: &str,
    sheet_index: u32,
) -> (pb::RangeStarted, Vec<pb::WorksheetRow>) {
    stream_range_batched(client, workbook_id, sheet_index, 0).await
}

/// Stream a worksheet with an explicit `max_rows_per_message`, flattening
/// whichever carrier the server chooses so callers see one row list either way.
async fn stream_range_batched(
    client: &CalamineServiceClient<tonic::transport::Channel>,
    workbook_id: &str,
    sheet_index: u32,
    max_rows_per_message: u32,
) -> (pb::RangeStarted, Vec<pb::WorksheetRow>) {
    let mut client = client.clone();
    let mut stream = client
        .stream_worksheet_range(pb::StreamWorksheetRangeRequest {
            workbook_id: workbook_id.to_string(),
            sheet: Some(pb::SheetSelector {
                selector: Some(pb::sheet_selector::Selector::SheetIndex(sheet_index)),
            }),
            max_rows_per_message,
            use_string_table: false,
        })
        .await
        .expect("stream worksheet range")
        .into_inner();

    let mut header = None;
    let mut rows = Vec::new();
    while let Some(event) = stream.message().await.expect("stream event") {
        match event.event.expect("event kind") {
            pb::stream_worksheet_range_response::Event::Started(started) => {
                assert!(header.is_none(), "header must be sent exactly once, first");
                assert!(rows.is_empty(), "header must precede all rows");
                header = Some(started);
            }
            pb::stream_worksheet_range_response::Event::Row(row) => rows.push(row),
            pb::stream_worksheet_range_response::Event::Rows(batch) => {
                assert!(!batch.rows.is_empty(), "a rows batch is never empty");
                rows.extend(batch.rows);
            }
            pb::stream_worksheet_range_response::Event::RowGap(gap) => expand_gap(&mut rows, gap),
            pb::stream_worksheet_range_response::Event::StringTable(_) => {
                panic!("string table events must only appear when requested")
            }
            pb::stream_worksheet_range_response::Event::Error(err) => {
                panic!("unexpected in-band error: {:?}", err.error)
            }
        }
    }
    (header.expect("stream must start with a header"), rows)
}

/// Expand a `row_gap` back into the blank rows it stands for.
///
/// This is what a client wanting a dense grid does, and doing it here lets
/// every assertion in this file keep comparing against calamine's own dense
/// output: a gap carries no cells by definition, so expanding one can only
/// ever produce blank rows. Their width is taken from the preceding row,
/// which is what a densifying walk would have padded them to.
///
/// A client collecting populated cells instead just skips the event, and one
/// that ignores it entirely still places every row correctly, because
/// `row_index` is absolute.
fn expand_gap(rows: &mut Vec<pb::WorksheetRow>, gap: pb::WorksheetRowGap) {
    assert!(gap.row_count > 0, "a gap always covers at least one row");
    // Width comes from the row before, which a gap normally has. The one case
    // with nothing before it is a `HeaderRow::Row(n)` selection whose header
    // row is blank: the sheet starts at `n`, so the gap leads. Zero width is
    // the honest answer there, since no cell has arrived to imply one, and the
    // contract allows a row to omit trailing empty cells anyway.
    let width = match rows.last() {
        Some(last) => {
            assert_eq!(
                gap.first_row_index,
                last.row_index + 1,
                "a gap starts immediately after the row before it"
            );
            last.values.len()
        }
        None => 0,
    };
    for offset in 0..gap.row_count {
        rows.push(pb::WorksheetRow {
            row_index: gap.first_row_index + offset,
            values: vec![convert::empty_cell_data(); width],
        });
    }
}

/// Ground truth for a worksheet, parsed locally with calamine: the range and
/// the expected dense rows exactly as the server should stream them.
fn expected_rows(file: &str, sheet_index: usize) -> (String, Vec<pb::WorksheetRow>) {
    let mut workbook: Sheets<_> = open_workbook_auto(fixtures().join(file)).expect("open fixture");
    let is_1904 = convert::has_1904_epoch(&workbook);
    let name = workbook.sheet_names()[sheet_index].clone();
    let range = workbook.worksheet_range(&name).expect("worksheet range");
    let start = range.start().expect("non-empty range");
    // Streamed rows are anchored at column 0, so a range starting right of
    // column A carries explicit leading empties.
    let pad = start.1 as usize;
    let rows = range
        .rows()
        .enumerate()
        .map(|(offset, row)| {
            let mut values = vec![convert::empty_cell_data(); pad];
            values.extend(
                row.iter()
                    .map(|d| convert::cell_data(convert::data_value(d, is_1904))),
            );
            pb::WorksheetRow {
                row_index: start.0 + offset as u32,
                values,
            }
        })
        .collect();
    (name, rows)
}

/// Assert that a streamed sheet matches calamine's own range output exactly.
async fn assert_sheet_matches_calamine(file: &str, sheet_index: u32) {
    let client = start_server().await;
    let opened = upload(&client, file).await;
    let (expected_name, expected_rows) = expected_rows(file, sheet_index as usize);

    let (header, rows) = stream_range(&client, &opened.workbook_id, sheet_index).await;

    assert_eq!(header.sheet_name, expected_name);
    assert_eq!(
        rows.len(),
        expected_rows.len(),
        "row count mismatch for {file} sheet {sheet_index}"
    );
    for (got, want) in rows.iter().zip(&expected_rows) {
        assert_eq!(got, want, "row {} mismatch", want.row_index);
    }
}

#[tokio::test]
async fn xlsx_streams_incrementally_and_matches_calamine() {
    assert_sheet_matches_calamine("date.xlsx", 0).await;
}

#[tokio::test]
async fn xlsb_streams_incrementally_and_matches_calamine() {
    assert_sheet_matches_calamine("date.xlsb", 0).await;
}

#[tokio::test]
async fn xls_streams_buffered_and_matches_calamine() {
    assert_sheet_matches_calamine("date.xls", 0).await;
}

#[tokio::test]
async fn ods_streams_buffered_and_matches_calamine() {
    assert_sheet_matches_calamine("date.ods", 0).await;
}

#[tokio::test]
async fn batched_and_single_row_modes_deliver_identical_rows() {
    // `max_rows_per_message` changes only how rows are packed into messages,
    // never what a row contains or the order they arrive in. A caller that
    // asks for 1 gets the `row` carrier; anything else gets `rows` batches.
    let client = start_server().await;
    let opened = upload(&client, "date.xlsx").await;

    let (batched_header, batched) = stream_range_batched(&client, &opened.workbook_id, 0, 0).await;
    let (single_header, single) = stream_range_batched(&client, &opened.workbook_id, 0, 1).await;
    let (paired_header, paired) = stream_range_batched(&client, &opened.workbook_id, 0, 2).await;

    assert_eq!(batched_header, single_header);
    assert_eq!(batched_header, paired_header);
    assert_eq!(batched, single, "batching must not change row content");
    assert_eq!(paired, single, "a cap of 2 must not change row content");
}

#[tokio::test]
async fn single_row_mode_uses_the_row_carrier() {
    // A client that only understands `row` must be able to ask for it.
    let mut client = start_server().await;
    let opened = upload(&client, "date.xlsx").await;
    let mut stream = client
        .stream_worksheet_range(pb::StreamWorksheetRangeRequest {
            workbook_id: opened.workbook_id.clone(),
            sheet: Some(pb::SheetSelector {
                selector: Some(pb::sheet_selector::Selector::SheetIndex(0)),
            }),
            max_rows_per_message: 1,
            use_string_table: false,
        })
        .await
        .expect("stream worksheet range")
        .into_inner();

    let mut saw_row = false;
    while let Some(event) = stream.message().await.expect("stream event") {
        match event.event.expect("event kind") {
            pb::stream_worksheet_range_response::Event::Row(_) => saw_row = true,
            pb::stream_worksheet_range_response::Event::Rows(_) => {
                panic!("max_rows_per_message = 1 must not use the batch carrier")
            }
            _ => {}
        }
    }
    assert!(saw_row, "the sheet has rows, so at least one must arrive");
}

#[tokio::test]
async fn sheet_without_declared_dimension_streams_every_cell() {
    // `<dimension>` is optional in ECMA-376 and temperature.xlsx omits it, so
    // calamine's cell reader reports the 1x1 default extent while the sheet
    // really holds 3 rows of 2 cells. Treating that extent as a filter dropped
    // 5 of the 6 cells with no error event, so the shape is asserted here
    // rather than through `assert_sheet_matches_calamine`: the incremental
    // reader reports these as `shared_string_value`, while `worksheet_range`
    // resolves them to `string_value`.
    let client = start_server().await;
    let opened = upload(&client, "temperature.xlsx").await;
    let (_header, rows) = stream_range(&client, &opened.workbook_id, 0).await;

    assert_eq!(rows.len(), 3, "every populated row must be streamed");
    for row in &rows {
        assert_eq!(
            row.values.len(),
            2,
            "row {} was truncated to the declared extent",
            row.row_index
        );
    }
    let first = &rows[0].values[0].value;
    assert_eq!(
        first.as_ref(),
        Some(&pb::cell_data::Value::SharedStringValue("label".into())),
        "first cell should survive intact"
    );
    let last = &rows[2].values[1].value;
    assert_eq!(
        last.as_ref(),
        Some(&pb::cell_data::Value::FloatValue(72.0)),
        "the far corner cell is the one most likely to be dropped"
    );
}

#[tokio::test]
async fn open_workbook_reports_format_and_metadata() {
    let mut client = start_server().await;
    let opened = upload(&client, "date.xlsx").await;
    assert_eq!(opened.detected_format, pb::WorkbookFormat::Xlsx as i32);
    let metadata = opened.metadata.clone().expect("metadata");
    assert!(!metadata.sheets.is_empty());
    assert_eq!(metadata.sheets[0].typ, pb::SheetType::Worksheet as i32);
    assert_eq!(metadata.sheets[0].visible, pb::SheetVisible::Visible as i32);

    // GetMetadata returns the same snapshot for the handle.
    let again = client
        .get_metadata(pb::GetMetadataRequest {
            workbook_id: opened.workbook_id.clone(),
        })
        .await
        .expect("get metadata")
        .into_inner();
    assert_eq!(again.metadata, opened.metadata);

    // The response carries the frontend advertisement.
    let ui = again.ui.expect("ui info");
    assert_eq!(ui.title, "Calamine");
    assert_eq!(ui.path, "/ui/calamine");
    assert_eq!(
        ui.description,
        "Spreadsheet parsing via calamine (xls, xlsx, xlsb, ods)"
    );

    // Close is idempotent-safe: first close true, second false.
    let closed = client
        .close_workbook(pb::CloseWorkbookRequest {
            workbook_id: opened.workbook_id.clone(),
        })
        .await
        .expect("close")
        .into_inner();
    assert!(closed.closed);
    let closed_again = client
        .close_workbook(pb::CloseWorkbookRequest {
            workbook_id: opened.workbook_id,
        })
        .await
        .expect("close again")
        .into_inner();
    assert!(!closed_again.closed);
}

#[tokio::test]
async fn empty_workbook_id_is_the_service_probe() {
    let mut client = start_server().await;
    let probe = client
        .get_metadata(pb::GetMetadataRequest::default())
        .await
        .expect("service probe")
        .into_inner();
    assert!(probe.metadata.is_none());
    let ui = probe.ui.expect("ui info on the probe response");
    assert_eq!(ui.title, "Calamine");
    assert_eq!(ui.path, "/ui/calamine");
}

#[tokio::test]
async fn unknown_workbook_id_is_not_found() {
    let mut client = start_server().await;
    let err = client
        .stream_worksheet_range(pb::StreamWorksheetRangeRequest {
            workbook_id: "does-not-exist".to_string(),
            sheet: Some(pb::SheetSelector {
                selector: Some(pb::sheet_selector::Selector::SheetIndex(0)),
            }),
            max_rows_per_message: 0,
            use_string_table: false,
        })
        .await
        .expect_err("must fail");
    assert_eq!(err.code(), Code::NotFound);
}

#[tokio::test]
async fn unknown_sheet_fails_in_band() {
    let mut client = start_server().await;
    let opened = upload(&client, "date.xlsx").await;
    let mut stream = client
        .stream_worksheet_range(pb::StreamWorksheetRangeRequest {
            workbook_id: opened.workbook_id,
            sheet: Some(pb::SheetSelector {
                selector: Some(pb::sheet_selector::Selector::SheetName(
                    "NoSuchSheet".to_string(),
                )),
            }),
            max_rows_per_message: 0,
            use_string_table: false,
        })
        .await
        .expect("rpc itself succeeds")
        .into_inner();
    let event = stream
        .message()
        .await
        .expect("stream open")
        .expect("one event");
    match event.event.expect("event kind") {
        pb::stream_worksheet_range_response::Event::Error(err) => {
            assert!(err.terminal);
            assert_eq!(
                err.error.expect("error detail").kind,
                pb::CalamineErrorKind::Xlsx as i32
            );
        }
        other => panic!("expected in-band error, got {other:?}"),
    }
}

#[tokio::test]
async fn vba_project_streams_modules() {
    let mut client = start_server().await;
    let opened = upload(&client, "vba.xlsm").await;
    let mut stream = client
        .stream_vba_project(pb::StreamVbaProjectRequest {
            workbook_id: opened.workbook_id,
        })
        .await
        .expect("stream vba project")
        .into_inner();

    let mut info = None;
    let mut modules = Vec::new();
    while let Some(event) = stream.message().await.expect("stream event") {
        match event.event.expect("event kind") {
            pb::stream_vba_project_response::Event::Info(i) => info = Some(i),
            pb::stream_vba_project_response::Event::Module(m) => modules.push(m),
            pb::stream_vba_project_response::Event::Error(err) => {
                panic!("unexpected in-band error: {:?}", err.error)
            }
        }
    }

    let info = info.expect("info header");
    assert!(info.present, "vba.xlsm must have a VBA project");
    assert!(!info.module_names.is_empty(), "module names in header");
    assert_eq!(info.module_names.len(), modules.len());
    for module in &modules {
        assert!(info.module_names.contains(&module.name));
        assert!(!module.raw_content.is_empty());
    }
}

#[tokio::test]
async fn no_vba_project_reports_absent() {
    let mut client = start_server().await;
    let opened = upload(&client, "date.xlsx").await;
    let mut stream = client
        .stream_vba_project(pb::StreamVbaProjectRequest {
            workbook_id: opened.workbook_id,
        })
        .await
        .expect("stream vba project")
        .into_inner();
    let event = stream
        .message()
        .await
        .expect("stream open")
        .expect("exactly one event");
    match event.event.expect("event kind") {
        pb::stream_vba_project_response::Event::Info(info) => assert!(!info.present),
        other => panic!("expected info header, got {other:?}"),
    }
    assert!(stream.message().await.expect("stream end").is_none());
}

#[tokio::test]
async fn formulas_stream_matches_calamine() {
    let mut client = start_server().await;
    let opened = upload(&client, "formula.issue.xlsx").await;

    // Ground truth.
    let mut workbook: Sheets<_> =
        open_workbook_auto(fixtures().join("formula.issue.xlsx")).expect("open fixture");
    let sheet_name = workbook.sheet_names()[0].clone();
    let range = workbook.worksheet_formula(&sheet_name).expect("formulas");
    let expected: Vec<Vec<String>> = range.rows().map(<[String]>::to_vec).collect();

    let mut stream = client
        .stream_worksheet_formula(pb::StreamWorksheetFormulaRequest {
            workbook_id: opened.workbook_id,
            sheet: Some(pb::SheetSelector {
                selector: Some(pb::sheet_selector::Selector::SheetIndex(0)),
            }),
        })
        .await
        .expect("stream formulas")
        .into_inner();

    let mut header_seen = false;
    let mut rows = Vec::new();
    while let Some(event) = stream.message().await.expect("stream event") {
        match event.event.expect("event kind") {
            pb::stream_worksheet_formula_response::Event::Started(h) => {
                header_seen = true;
                assert_eq!(h.sheet_name, sheet_name);
            }
            pb::stream_worksheet_formula_response::Event::Row(r) => rows.push(r.formulas),
            pb::stream_worksheet_formula_response::Event::Error(err) => {
                panic!("unexpected in-band error: {:?}", err.error)
            }
        }
    }
    assert!(header_seen);
    assert_eq!(rows, expected);
}

#[tokio::test]
async fn concurrent_uploads_and_streams() {
    let client = start_server().await;

    // Upload three workbooks of different formats concurrently.
    let (a, b, c) = tokio::join!(
        upload(&client, "date.xlsx"),
        upload(&client, "date.xlsb"),
        upload(&client, "date.ods"),
    );

    // Stream all three concurrently.
    let ((ha, ra), (hb, rb), (hc, rc)) = tokio::join!(
        stream_range(&client, &a.workbook_id, 0),
        stream_range(&client, &b.workbook_id, 0),
        stream_range(&client, &c.workbook_id, 0),
    );
    assert!(!ra.is_empty() && !rb.is_empty() && !rc.is_empty());
    assert!(ha.total_cells > 0 && hb.total_cells > 0 && hc.total_cells > 0);
}

#[tokio::test]
async fn same_workbook_streams_in_parallel() {
    // The read path is lock-free: many concurrent streams against one
    // workbook handle must all complete with identical results.
    let client = start_server().await;
    let opened = upload(&client, "date.xlsx").await;

    let (expected_name, expected_rows) = expected_rows("date.xlsx", 0);

    let (r0, r1, r2, r3) = tokio::join!(
        stream_range(&client, &opened.workbook_id, 0),
        stream_range(&client, &opened.workbook_id, 0),
        stream_range(&client, &opened.workbook_id, 0),
        stream_range(&client, &opened.workbook_id, 0),
    );
    for (header, rows) in [&r0, &r1, &r2, &r3] {
        assert_eq!(header.sheet_name, expected_name);
        assert_eq!(rows, &expected_rows);
    }
}

#[tokio::test]
async fn datetime_cells_round_trip_exactly() {
    // date.xlsx contains real Excel serial datetimes; make sure the proto
    // carries the raw serial value and epoch flag untouched.
    let client = start_server().await;
    let opened = upload(&client, "date.xlsx").await;
    let (_, rows) = stream_range(&client, &opened.workbook_id, 0).await;

    let mut saw_datetime = false;
    for row in &rows {
        for cell in &row.values {
            if let Some(pb::cell_data::Value::DateTime(dt)) = &cell.value {
                saw_datetime = true;
                assert!(dt.value > 0.0, "serial value must be preserved");
                assert_ne!(dt.datetime_type, pb::ExcelDateTimeType::Unspecified as i32);
                assert!(!dt.is_1904, "date.xlsx uses the 1900 date system");
            }
        }
    }
    assert!(saw_datetime, "date.xlsx must contain datetime cells");
}

#[tokio::test]
async fn date_1904_workbook_sets_epoch_flag_on_every_datetime() {
    // The 1904 flag is a workbook-level property, read once per workbook via
    // calamine's `has_1904_epoch` and stamped onto each streamed datetime.
    let client = start_server().await;
    let opened = upload(&client, "date_1904.xlsx").await;
    let (_, rows) = stream_range(&client, &opened.workbook_id, 0).await;

    let mut saw_datetime = false;
    for row in &rows {
        for cell in &row.values {
            if let Some(pb::cell_data::Value::DateTime(dt)) = &cell.value {
                saw_datetime = true;
                assert!(dt.is_1904, "date_1904.xlsx uses the 1904 date system");
            }
        }
    }
    assert!(saw_datetime, "date_1904.xlsx must contain datetime cells");
}

#[tokio::test]
async fn error_cells_map_to_typed_enum() {
    let client = start_server().await;
    let opened = upload(&client, "errors.xlsx").await;
    let (_, rows) = stream_range(&client, &opened.workbook_id, 0).await;

    let mut saw_error = false;
    for row in &rows {
        for cell in &row.values {
            if let Some(pb::cell_data::Value::Error(kind)) = &cell.value {
                saw_error = true;
                assert_ne!(*kind, pb::CellErrorType::Unspecified as i32);
            }
        }
    }
    assert!(saw_error, "errors.xlsx must contain error cells");
}

/// Sanity check on the ground-truth helper itself: `Data::Empty` cells must
/// appear as explicit empty variants, never as missing oneofs.
#[test]
fn empty_cells_are_explicit() {
    let mut workbook: Sheets<_> =
        open_workbook_auto(fixtures().join("date.xlsx")).expect("open fixture");
    let range = workbook
        .worksheet_range_at(0)
        .expect("sheet")
        .expect("range");
    let has_data = range.cells().any(|(_, _, d)| !matches!(d, Data::Empty));
    assert!(has_data);
}

// ---------------------------------------------------------------------------
// Count parity: the stream vs calamine's own API.
//
// Both real incidents so far were count mismatches rooted in the declared
// `<dimension>`: a 105 MB workbook whose declaration ended in 58,577 rows of
// styled blanks (the server streamed them, `Range::from_sparse` trims them),
// and temperature.xlsx, which omits the declaration entirely (treating the
// default 1x1 extent as a filter dropped 5 of its 6 cells). Both were found
// by accident, from the outside. These tests make the parity a stated
// invariant: for every sheet the server can stream, the populated cells and
// the row extent must equal what `worksheet_range` reports, and a sheet
// calamine refuses locally must fail in-band rather than half-stream.
// ---------------------------------------------------------------------------

/// Stream a worksheet and report exactly what happened, asserting nothing:
/// the header if one arrived, every row from either carrier, any in-band
/// error.
async fn stream_range_outcome(
    client: &CalamineServiceClient<tonic::transport::Channel>,
    workbook_id: &str,
    sheet_index: u32,
) -> (
    Option<pb::RangeStarted>,
    Vec<pb::WorksheetRow>,
    Option<pb::StreamError>,
) {
    let mut client = client.clone();
    let mut stream = client
        .stream_worksheet_range(pb::StreamWorksheetRangeRequest {
            workbook_id: workbook_id.to_string(),
            sheet: Some(pb::SheetSelector {
                selector: Some(pb::sheet_selector::Selector::SheetIndex(sheet_index)),
            }),
            max_rows_per_message: 0,
            use_string_table: false,
        })
        .await
        .expect("stream worksheet range")
        .into_inner();

    let mut header = None;
    let mut rows = Vec::new();
    let mut error = None;
    while let Some(event) = stream.message().await.expect("stream event") {
        match event.event.expect("event kind") {
            pb::stream_worksheet_range_response::Event::Started(started) => header = Some(started),
            pb::stream_worksheet_range_response::Event::Row(row) => rows.push(row),
            pb::stream_worksheet_range_response::Event::Rows(batch) => rows.extend(batch.rows),
            pb::stream_worksheet_range_response::Event::RowGap(gap) => expand_gap(&mut rows, gap),
            pb::stream_worksheet_range_response::Event::StringTable(_) => {
                panic!("string table events must only appear when requested")
            }
            pb::stream_worksheet_range_response::Event::Error(err) => error = Some(err),
        }
    }
    (header, rows, error)
}

/// A sheet's population as `(row, col, value)` for every non-empty cell, in
/// stream order, with shared strings resolved to plain strings so the
/// incremental reader and `worksheet_range` compare equal. Rows are anchored
/// at column 0, so a value's index is its absolute column.
fn populated_cells(rows: &[pb::WorksheetRow]) -> Vec<(u32, u32, pb::cell_data::Value)> {
    let mut cells = Vec::new();
    for row in rows {
        for (col, cell) in row.values.iter().enumerate() {
            let value = match &cell.value {
                None | Some(pb::cell_data::Value::Empty(())) => continue,
                Some(pb::cell_data::Value::SharedStringValue(s)) => {
                    pb::cell_data::Value::StringValue(s.clone())
                }
                Some(v) => v.clone(),
            };
            cells.push((row.row_index, col as u32, value));
        }
    }
    cells
}

/// Every sheet of every fixture: the streamed row extent and populated cells
/// must equal calamine's `worksheet_range`, and a sheet calamine refuses
/// must produce an in-band error, not a silent partial stream.
#[tokio::test]
async fn every_sheet_of_every_fixture_matches_calamine_counts() {
    let files = [
        "any_sheets.xlsx",
        "date.ods",
        "date.xls",
        "date.xlsb",
        "date.xlsx",
        "date_1904.xlsx",
        "errors.xlsx",
        "formula.issue.xlsx",
        "temperature.xlsx",
        "vba.xlsm",
        "dimension_inflated.xlsx",
        "dimension_underdeclared.xlsx",
        "dimension_shifted.xlsx",
        "dimension_offset.xlsx",
        "dimension_wide.xlsx",
        "rows_out_of_order.xlsx",
        "rows_descending.xlsx",
        "gap.ods",
        "rows_with_gaps.xlsx",
        // Deliberately absent: `dimension_reversed.xlsx`, whose declaration
        // underflows calamine's own unchecked corner subtraction, so building
        // the ground truth here would panic before the server was asked
        // anything (it has its own test); `rows_late_backwards.xlsx`, which
        // calamine reads and a one-pass stream cannot (likewise); and the
        // `corners*` fixtures, whose dense range is 17.2 billion cells, so
        // asking calamine for the ground truth is itself the thing that cannot
        // be afforded. They have their own tests, which assert against the
        // cells in the file rather than against a range nobody can allocate.
    ];
    for file in files {
        let client = start_server().await;
        let opened = upload(&client, file).await;
        let mut workbook: Sheets<_> =
            open_workbook_auto(fixtures().join(file)).expect("open fixture");
        let is_1904 = convert::has_1904_epoch(&workbook);
        let names = workbook.sheet_names().to_vec();

        for (index, name) in names.iter().enumerate() {
            let local = workbook.worksheet_range(name);
            let (header, rows, error) =
                stream_range_outcome(&client, &opened.workbook_id, index as u32).await;

            let Ok(range) = local else {
                assert!(
                    error.is_some(),
                    "{file}/{name}: calamine refuses this sheet locally, \
                     so the stream must carry an in-band error"
                );
                continue;
            };

            assert!(
                error.is_none(),
                "{file}/{name}: calamine parses this sheet locally, but the \
                 stream errored: {error:?}"
            );
            let header = header.expect("stream must start with a header");
            assert_eq!(header.sheet_name, *name, "{file}: wrong sheet resolved");

            assert_eq!(
                rows.len(),
                range.height(),
                "{file}/{name}: row count differs from worksheet_range"
            );
            if let (Some(first), Some(start)) = (rows.first(), range.start()) {
                assert_eq!(
                    first.row_index, start.0,
                    "{file}/{name}: first streamed row is not the range start"
                );
            }
            if let (Some(last), Some(end)) = (rows.last(), range.end()) {
                assert_eq!(
                    last.row_index, end.0,
                    "{file}/{name}: last streamed row is not the range end"
                );
            }

            let range_start = range.start().unwrap_or((0, 0));
            let mut expected = Vec::new();
            for (row_offset, row) in range.rows().enumerate() {
                for (col_offset, data) in row.iter().enumerate() {
                    if matches!(data, Data::Empty) {
                        continue;
                    }
                    let value = match convert::data_value(data, is_1904) {
                        pb::cell_data::Value::SharedStringValue(s) => {
                            pb::cell_data::Value::StringValue(s)
                        }
                        v => v,
                    };
                    expected.push((
                        range_start.0 + row_offset as u32,
                        range_start.1 + col_offset as u32,
                        value,
                    ));
                }
            }
            assert_eq!(
                populated_cells(&rows),
                expected,
                "{file}/{name}: populated cells differ from worksheet_range \
                 in position, count or value"
            );
        }
    }
}

/// The miniature of the 58,577-row incident: a declaration of A1:C50 whose
/// content stops at row 4, followed by styled-blank rows the incremental
/// reader still yields cells for. The trailing blanks must be trimmed, and
/// the interior gap row must survive as an explicit empty row.
#[tokio::test]
async fn trailing_styled_blank_rows_are_trimmed() {
    let client = start_server().await;
    let opened = upload(&client, "dimension_inflated.xlsx").await;
    let (header, rows) = stream_range(&client, &opened.workbook_id, 0).await;

    // The header reports the declared extent; it is a pre-allocation hint,
    // not a promise of what will stream.
    assert_eq!(header.total_cells, 150, "declared A1:C50 is 50x3");

    let indices: Vec<u32> = rows.iter().map(|r| r.row_index).collect();
    assert_eq!(
        indices,
        vec![0, 1, 2, 3],
        "rows 10-11 are trailing styled blanks and must not stream"
    );
    assert!(
        rows[2]
            .values
            .iter()
            .all(|c| matches!(c.value, Some(pb::cell_data::Value::Empty(())))),
        "row 3 (index 2) is an interior gap and must stream as an empty row"
    );
}

/// A declaration of A1:A1 over content reaching D5: everything past the
/// declared extent must stream. Treating the declaration as a filter is the
/// bug that silently dropped 5 of temperature.xlsx's 6 cells.
#[tokio::test]
async fn cells_past_an_underdeclared_dimension_all_stream() {
    let client = start_server().await;
    let opened = upload(&client, "dimension_underdeclared.xlsx").await;
    let (_header, rows) = stream_range(&client, &opened.workbook_id, 0).await;

    assert_eq!(rows.len(), 5, "rows 1-5 with interior gaps as empty rows");
    assert_eq!(
        populated_cells(&rows),
        vec![
            (0, 0, pb::cell_data::Value::FloatValue(1.0)),
            (4, 3, pb::cell_data::Value::FloatValue(9.0)),
        ],
        "the cell at D5 must survive despite the A1:A1 declaration"
    );
}

/// A declaration of C3:D4 over content starting at A1, left of and above the
/// declared start. `worksheet_range` rebuilds the extent from the cells it
/// sees, so A1 and B2 are part of the sheet and must stream.
#[tokio::test]
async fn cells_left_of_a_shifted_dimension_still_stream() {
    let client = start_server().await;
    let opened = upload(&client, "dimension_shifted.xlsx").await;
    let (_header, rows) = stream_range(&client, &opened.workbook_id, 0).await;

    assert_eq!(
        populated_cells(&rows),
        vec![
            (0, 0, pb::cell_data::Value::FloatValue(1.0)),
            (1, 1, pb::cell_data::Value::FloatValue(2.0)),
            (2, 2, pb::cell_data::Value::FloatValue(3.0)),
            (2, 3, pb::cell_data::Value::FloatValue(4.0)),
        ],
        "cells left of the declared start column must not be dropped"
    );
}

/// An honest C3:D4 range: rows are anchored at column A regardless, so the
/// C-column values sit at index 2 behind two explicit empties, in both the
/// incremental and the buffered representation of the same contract.
#[tokio::test]
async fn rows_are_anchored_at_column_a() {
    let client = start_server().await;
    let opened = upload(&client, "dimension_offset.xlsx").await;
    let (_header, rows) = stream_range(&client, &opened.workbook_id, 0).await;

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].row_index, 2, "range starts at spreadsheet row 3");
    assert!(
        rows[0].values.len() >= 4,
        "the C3 value must sit at its absolute column index"
    );
    assert!(
        rows[0].values[..2]
            .iter()
            .all(|c| matches!(c.value, Some(pb::cell_data::Value::Empty(())))),
        "columns A and B are explicit empties"
    );
    assert_eq!(
        rows[0].values[2].value,
        Some(pb::cell_data::Value::FloatValue(3.0))
    );
    assert_eq!(
        rows[1].values[3].value,
        Some(pb::cell_data::Value::FloatValue(6.0))
    );
}

/// A `<dimension>` whose end is before its start must not break the stream.
///
/// ECMA-376 does not require `ref` to be ordered, and calamine subtracts the
/// two corners with unchecked `u32` arithmetic (`get_dimension`,
/// xlsx/mod.rs:2789, and `Dimensions::len`, lib.rs:181). Both underflow on a
/// reversed range, and the server hands the result straight to the client as
/// `RangeStarted.total_cells`. Two observable failures follow from the same
/// input:
///
/// - **release** (overflow checks off): the extent wraps and `total_cells`
///   comes back as 18,446,744,056,529,682,000, which is 10^9 times the whole
///   Excel grid. Anything sizing a progress bar or preallocating from it is
///   handed nonsense.
/// - **debug** (overflow checks on): calamine panics inside the blocking
///   parse task. The panic kills the task, which drops the channel sender,
///   which ends the stream *successfully* with zero events. The caller sees
///   no header, no in-band error, and an OK status: an empty sheet.
///
/// The second is the worse one, and it is not specific to this input. Any
/// panic below `spawn_blocking_stream` (service.rs:175) is reported to the
/// client as a clean, empty, successful stream.
#[tokio::test]
async fn a_reversed_declared_dimension_does_not_break_the_stream() {
    // Excel's own grid: 1,048,576 rows x 16,384 columns. Nothing calamine can
    // legitimately report may exceed it.
    const MAX_GRID_CELLS: u64 = 1_048_576 * 16_384;

    let mut client = start_server().await;
    let opened = upload(&client, "dimension_reversed.xlsx").await;
    let mut stream = client
        .stream_worksheet_range(pb::StreamWorksheetRangeRequest {
            workbook_id: opened.workbook_id,
            sheet: Some(pb::SheetSelector {
                selector: Some(pb::sheet_selector::Selector::SheetIndex(0)),
            }),
            max_rows_per_message: 0,
            use_string_table: false,
        })
        .await
        .expect("rpc itself succeeds")
        .into_inner();

    let mut header = None;
    let mut rows = 0usize;
    let mut in_band = None;
    // A terminal failure is allowed to arrive as a gRPC status; what is not
    // allowed is a clean, empty, successful stream.
    let mut status = None;
    loop {
        match stream.message().await {
            Ok(None) => break,
            Ok(Some(event)) => match event.event.expect("event kind") {
                pb::stream_worksheet_range_response::Event::Started(s) => header = Some(s),
                pb::stream_worksheet_range_response::Event::Row(_) => rows += 1,
                pb::stream_worksheet_range_response::Event::Rows(b) => rows += b.rows.len(),
                pb::stream_worksheet_range_response::Event::RowGap(g) => {
                    rows += g.row_count as usize
                }
                pb::stream_worksheet_range_response::Event::Error(e) => in_band = Some(e),
                pb::stream_worksheet_range_response::Event::StringTable(_) => {}
            },
            Err(s) => {
                status = Some(s);
                break;
            }
        }
    }

    assert!(
        header.is_some() || in_band.is_some() || status.is_some(),
        "the stream ended with no header, no rows ({rows}) and no error of any \
         kind: a caller cannot tell this from an empty sheet"
    );

    if let Some(header) = header {
        assert!(
            header.total_cells <= MAX_GRID_CELLS,
            "total_cells is {}, larger than the entire Excel grid ({MAX_GRID_CELLS}); \
             the declared extent underflowed",
            header.total_cells
        );
    }
}

/// Rows that arrive out of order must land at their own row index.
///
/// Nothing in ECMA-376 requires `<row>` elements to be sorted; the `r`
/// attribute on each `<c>` is what fixes the position, and calamine reads
/// such a sheet correctly because `Range::from_sparse` sorts the cells it
/// collected. `emit_incremental` (service.rs:502) instead walks the cell
/// stream in arrival order and only ever advances: `while current_row < row`
/// (service.rs:584) has no branch for a row index that moves backwards, so a
/// late-arriving earlier row is written into the row already under
/// construction, at that row's index.
///
/// For `rows_out_of_order.xlsx` (`A3=1` written before `A1=2`) calamine
/// reports three rows -- A1=2, A2 empty, A3=1 -- and the server emits one row
/// at index 2 holding the value 2. The value 1 is dropped and the value 2 is
/// reported at the wrong row. Both are silent: no error event, no gRPC
/// status.
#[tokio::test]
async fn rows_arriving_out_of_order_keep_their_own_row_index() {
    let client = start_server().await;
    let opened = upload(&client, "rows_out_of_order.xlsx").await;
    let (_header, rows) = stream_range(&client, &opened.workbook_id, 0).await;

    assert_eq!(
        populated_cells(&rows),
        vec![
            (0, 0, pb::cell_data::Value::FloatValue(2.0)),
            (2, 0, pb::cell_data::Value::FloatValue(1.0)),
        ],
        "a row written after a later row must still land at its own index, \
         and no value may be dropped"
    );
    assert_eq!(
        rows.len(),
        3,
        "A1..A3 spans three rows, the middle one empty"
    );
}

/// A fully reversed sheet still streams correctly, in ascending order.
///
/// 40 rows written last-to-first. Nothing is committed while the whole sheet
/// fits in the batcher's unsent queue, so every late row is repaired in place
/// rather than lost. This is the reach the one-pass densifier actually has.
#[tokio::test]
async fn a_fully_reversed_sheet_streams_in_ascending_order() {
    let client = start_server().await;
    let opened = upload(&client, "rows_descending.xlsx").await;
    let (_header, rows) = stream_range(&client, &opened.workbook_id, 0).await;

    let expected: Vec<(u32, u32, pb::cell_data::Value)> = (0..40)
        .map(|r| (r, 0, pb::cell_data::Value::FloatValue(f64::from(r + 1))))
        .collect();
    assert_eq!(populated_cells(&rows), expected);
    assert_eq!(
        rows.iter().map(|r| r.row_index).collect::<Vec<_>>(),
        (0..40).collect::<Vec<_>>(),
        "rows must arrive in ascending order whatever order the file used"
    );
}

/// Repair reach must not depend on which carrier the caller asked for.
///
/// `max_rows_per_message = 1` changes how rows are packed into messages, and
/// nothing else. If it also shrank the window in which an out-of-order row can
/// be repaired, the same file would succeed batched and fail row-per-message.
#[tokio::test]
async fn unsorted_rows_repair_identically_in_both_carriers() {
    let client = start_server().await;
    let opened = upload(&client, "rows_descending.xlsx").await;

    let (batched_header, batched) = stream_range_batched(&client, &opened.workbook_id, 0, 0).await;
    let (single_header, single) = stream_range_batched(&client, &opened.workbook_id, 0, 1).await;

    assert_eq!(batched_header, single_header);
    assert_eq!(
        batched, single,
        "an unsorted sheet must resolve to the same rows in either carrier"
    );
}

/// Out of order too late to repair must fail loudly, never silently.
///
/// 600 ascending rows force several batches onto the wire, and only then does
/// a cell arrive back in row 1. gRPC cannot retract a sent message, so the row
/// cannot be placed. The contract's terminal in-band error is the only honest
/// outcome; folding the cell into the row under construction, which is what
/// the server used to do, is silent data loss.
///
/// calamine's own buffered API reads this file without complaint. That gap is
/// the documented price of streaming in one pass, not a parity bug.
#[tokio::test]
async fn out_of_order_beyond_repair_fails_in_band() {
    let client = start_server().await;
    let opened = upload(&client, "rows_late_backwards.xlsx").await;
    let (header, rows, error) = stream_range_outcome(&client, &opened.workbook_id, 0).await;

    assert!(header.is_some(), "the header still arrives first");
    let error = error.expect("an unrepairable row must produce an in-band error");
    assert!(error.terminal, "the stream cannot continue past this");
    let detail = error.error.expect("error detail");
    assert!(
        detail.message.contains("ascending order"),
        "the message must say what is wrong with the file: {}",
        detail.message
    );
    // Whatever did stream must still be correct and in order.
    let indices: Vec<u32> = rows.iter().map(|r| r.row_index).collect();
    assert!(
        indices.windows(2).all(|w| w[0] < w[1]),
        "rows delivered before the failure must still be ascending"
    );
}

/// Every event of a range stream, in arrival order, with gaps left intact.
///
/// The other collectors expand a gap so their assertions can compare against
/// calamine's dense output. These tests are about the gaps themselves, so they
/// need to see them.
#[derive(Debug, PartialEq)]
enum Event {
    Row(u32),
    Gap(u32, u32),
}

async fn stream_range_events(
    client: &CalamineServiceClient<tonic::transport::Channel>,
    workbook_id: &str,
    sheet_index: u32,
) -> (Vec<Event>, Vec<(u32, u32, pb::cell_data::Value)>) {
    let mut client = client.clone();
    let mut stream = client
        .stream_worksheet_range(pb::StreamWorksheetRangeRequest {
            workbook_id: workbook_id.to_string(),
            sheet: Some(pb::SheetSelector {
                selector: Some(pb::sheet_selector::Selector::SheetIndex(sheet_index)),
            }),
            max_rows_per_message: 0,
            use_string_table: false,
        })
        .await
        .expect("stream worksheet range")
        .into_inner();

    let mut events = Vec::new();
    let mut cells = Vec::new();
    fn collect(
        events: &mut Vec<Event>,
        cells: &mut Vec<(u32, u32, pb::cell_data::Value)>,
        row: pb::WorksheetRow,
    ) {
        for (col, cell) in row.values.iter().enumerate() {
            match &cell.value {
                Some(pb::cell_data::Value::Empty(())) | None => {}
                Some(value) => cells.push((row.row_index, col as u32, value.clone())),
            }
        }
        events.push(Event::Row(row.row_index));
    }
    while let Some(event) = stream.message().await.expect("stream event") {
        match event.event.expect("event kind") {
            pb::stream_worksheet_range_response::Event::Started(_) => {}
            pb::stream_worksheet_range_response::Event::Row(row) => {
                collect(&mut events, &mut cells, row)
            }
            pb::stream_worksheet_range_response::Event::Rows(batch) => {
                for row in batch.rows {
                    collect(&mut events, &mut cells, row);
                }
            }
            pb::stream_worksheet_range_response::Event::RowGap(gap) => {
                events.push(Event::Gap(gap.first_row_index, gap.row_count))
            }
            pb::stream_worksheet_range_response::Event::StringTable(_) => {}
            pb::stream_worksheet_range_response::Event::Error(err) => {
                panic!("unexpected in-band error: {:?}", err.error)
            }
        }
    }
    (events, cells)
}

/// A run of empty rows is one `row_gap`, whatever its length.
///
/// The fixture has a single blank row, a run of thirteen, two populated rows
/// back to back, leading blanks and trailing blanks, which is every shape the
/// rule has to cover.
#[tokio::test]
async fn interior_empty_rows_collapse_into_one_gap_each() {
    let client = start_server().await;
    let opened = upload(&client, "rows_with_gaps.xlsx").await;
    let (events, cells) = stream_range_events(&client, &opened.workbook_id, 0).await;

    // Rows are 0-based on the wire, so the file's rows 3, 4, 6 and 20 are 2, 3,
    // 5 and 19 here.
    assert_eq!(
        events,
        vec![
            Event::Row(2),
            Event::Row(3),
            Event::Gap(4, 1),
            Event::Row(5),
            Event::Gap(6, 13),
            Event::Row(19),
        ],
        "one gap per run, no gap between adjacent rows, and none before the \
         first row or after the last"
    );
    assert_eq!(
        cells,
        vec![
            (2, 0, pb::cell_data::Value::FloatValue(3.0)),
            (3, 0, pb::cell_data::Value::FloatValue(4.0)),
            (5, 0, pb::cell_data::Value::FloatValue(6.0)),
            (19, 0, pb::cell_data::Value::FloatValue(20.0)),
        ],
        "collapsing empty rows must not move or lose a single value"
    );
}

/// Two cells at opposite corners of the grid must cost two rows, not a million.
///
/// This is the file that OOM-killed a Node client. `corners.xlsx` is about
/// 2 KB and declares `A1:XFD1048576`; the rows between its two cells hold
/// nothing, and at the sheet's final width that dense grid is 17.2 billion
/// cells. calamine itself allocates it, which is why the ground truth here is
/// the two cells in the file rather than `worksheet_range`.
///
/// A gap is constant size however long the run, so the whole sheet is two rows
/// and one gap. Without one this test does not merely fail, it never finishes.
#[tokio::test]
async fn a_sheet_of_two_far_apart_cells_streams_in_constant_space() {
    let client = start_server().await;
    let opened = upload(&client, "corners.xlsx").await;
    let (events, cells) = stream_range_events(&client, &opened.workbook_id, 0).await;

    assert_eq!(
        events,
        vec![
            Event::Row(0),
            Event::Gap(1, 1_048_574),
            Event::Row(1_048_575)
        ],
        "1,048,574 empty rows must ride one gap"
    );
    assert_eq!(
        cells,
        vec![
            (0, 0, pb::cell_data::Value::FloatValue(1.0)),
            (1_048_575, 16_383, pb::cell_data::Value::FloatValue(2.0)),
        ],
        "both corners must arrive at their absolute positions"
    );
}

/// A blank `header_row` selection starts the sheet there, and the gap says so.
///
/// `HeaderRow::Row(n)` makes `n` the start of the sheet whatever `n` holds:
/// calamine inserts a synthetic empty cell there before building the extent,
/// so `Row(2)` over a sheet populated from row 5 reports `start = (2, 0)` and
/// height 5, not `start = (5, 0)` and height 2. The rows below the header are
/// therefore interior blanks, not leading padding to trim, and this is the one
/// case where a gap is the first row event of a stream.
///
/// Getting this wrong is silent: the values are all correct and only the
/// sheet's reported extent shifts, which no cell-level assertion would catch.
#[tokio::test]
async fn a_blank_header_row_starts_the_sheet_and_leads_with_a_gap() {
    let client = start_server().await;
    let opened = upload_with_options(
        &client,
        "blank_header_row.xlsx",
        pb::WorkbookOptions {
            format_hint: pb::WorkbookFormat::Unspecified as i32,
            header_row: Some(pb::HeaderRow {
                selection: Some(pb::header_row::Selection::RowIndex(2)),
            }),
        },
    )
    .await;
    let (events, cells) = stream_range_events(&client, &opened.workbook_id, 0).await;

    assert_eq!(
        events,
        vec![Event::Gap(2, 3), Event::Row(5), Event::Row(6)],
        "rows 2-4 are below the selected header row, so they are interior \
         blanks the stream must announce rather than trim"
    );
    assert_eq!(
        cells,
        vec![
            (5, 0, pb::cell_data::Value::FloatValue(6.0)),
            (6, 0, pb::cell_data::Value::FloatValue(7.0)),
        ]
    );

    // The same sheet with no selection starts at the first populated row, so
    // there is nothing above it to describe and no gap leads.
    let plain = upload(&client, "blank_header_row.xlsx").await;
    let (events, _) = stream_range_events(&client, &plain.workbook_id, 0).await;
    assert_eq!(
        events,
        vec![Event::Row(5), Event::Row(6)],
        "without a header row selection, leading blanks are trimmed"
    );
}

/// A gap changes the wire, never the content.
///
/// Expanding every gap back into blank rows has to reproduce exactly what a
/// row-per-row stream of the same sheet produces, or the gap is not a lossless
/// encoding of the rows it replaced. Both carriers are checked, because
/// `max_rows_per_message = 1` sends gaps too.
#[tokio::test]
async fn expanding_gaps_reproduces_the_dense_stream() {
    let client = start_server().await;
    let opened = upload(&client, "rows_with_gaps.xlsx").await;

    let (batched_header, batched) = stream_range_batched(&client, &opened.workbook_id, 0, 0).await;
    let (single_header, single) = stream_range_batched(&client, &opened.workbook_id, 0, 1).await;

    assert_eq!(batched_header, single_header);
    assert_eq!(
        batched, single,
        "the carrier must not change what a gap means"
    );
    assert_eq!(
        batched.iter().map(|r| r.row_index).collect::<Vec<_>>(),
        (2..=19).collect::<Vec<_>>(),
        "expanded, the sheet is rows 2 through 19 with nothing missing"
    );
    assert!(
        batched.iter().all(|r| r.values.len() == 1),
        "an expanded blank row is as wide as the rows around it"
    );
}

/// The declared `<dimension>` must not size the server's row buffer.
///
/// `emit_incremental` pre-sizes each row from the declaration
/// (`width = dims.end.1 as usize + 1`, service.rs:532) and then allocates
/// `vec![empty_cell_data(); width]` (service.rs:534) before reading a single
/// cell. That end column comes straight out of the uploaded file. calamine
/// once only warned past the 16,384 column grid limit, so a declared
/// `A1:ZZZZZZ1` (column 321,272,405) committed roughly 10 GB of buffer
/// before any work happened; since tafia/calamine#696 a reference past the
/// grid is a hard `ColumnNumberOverflow`, which closes that route upstream.
///
/// This fixture declares the widest in-grid extent, `A1:XFD1` (16,384
/// columns), over a single cell at A1. calamine reports a 1x1 range for it,
/// so the emitted row should be one cell wide: the declaration still must
/// not control how much the server allocates.
#[tokio::test]
async fn a_wide_declared_dimension_does_not_size_the_row_buffer() {
    let client = start_server().await;
    let opened = upload(&client, "dimension_wide.xlsx").await;
    let (_header, rows) = stream_range(&client, &opened.workbook_id, 0).await;

    assert_eq!(rows.len(), 1, "the sheet holds exactly one populated row");
    assert_eq!(
        rows[0].values.len(),
        1,
        "the row is sized from the declared full-grid extent rather than \
         from the single cell present, so the declaration controls how much \
         the server allocates"
    );
}

/// The buffered path (XLS and ODS) gets its rows from a dense range, where a
/// gap row is as wide as the widest row. It must still leave as one `row_gap`,
/// exactly as on the incremental path, not as rows of empty cells.
#[tokio::test]
async fn a_buffered_sheet_collapses_its_gap_into_one_event() {
    let client = start_server().await;
    let opened = upload(&client, "gap.ods").await;
    let (events, cells) = stream_range_events(&client, &opened.workbook_id, 0).await;

    assert_eq!(
        events,
        vec![Event::Row(0), Event::Gap(1, 3), Event::Row(4)],
        "the three empty rows of the dense range are one gap"
    );
    assert_eq!(
        cells,
        vec![
            (0, 0, pb::cell_data::Value::FloatValue(1.0)),
            (4, 3, pb::cell_data::Value::FloatValue(9.0)),
        ]
    );
}

/// Compression must change bytes on the wire, never content: a client that
/// negotiates zstd (or gzip) gets exactly the rows a plain client gets.
#[tokio::test]
async fn compressed_streams_deliver_identical_rows() {
    let client = start_server().await;
    let opened = upload(&client, "date.xlsx").await;
    let (plain_header, plain) = stream_range(&client, &opened.workbook_id, 0).await;

    for encoding in [
        tonic::codec::CompressionEncoding::Zstd,
        tonic::codec::CompressionEncoding::Gzip,
    ] {
        let compressed_client = client
            .clone()
            .accept_compressed(encoding)
            .send_compressed(encoding);
        let (header, rows) =
            stream_range_batched(&compressed_client, &opened.workbook_id, 0, 0).await;
        assert_eq!(header, plain_header, "{encoding:?} changed the header");
        assert_eq!(rows, plain, "{encoding:?} changed row content");
    }
}

// ---------------------------------------------------------------------------
// Dictionary mode: `use_string_table`.
// ---------------------------------------------------------------------------

/// Resolve one row's `shared_string_id` cells back into
/// `shared_string_value` against the table collected so far. Panics if a row
/// references an id no chunk has defined, which is the contract's ordering
/// guarantee.
fn resolve_row(mut row: pb::WorksheetRow, table: &[String]) -> pb::WorksheetRow {
    for cell in &mut row.values {
        if let Some(pb::cell_data::Value::SharedStringId(id)) = cell.value {
            let text = table
                .get(id as usize)
                .unwrap_or_else(|| panic!("id {id} referenced before its defining chunk"));
            cell.value = Some(pb::cell_data::Value::SharedStringValue(text.clone()));
        }
    }
    row
}

/// Stream a worksheet in `use_string_table` mode, asserting the table
/// contract as events arrive: chunks dense from zero and in order, never
/// empty, every id defined before first referenced. Returns the resolved
/// rows and the final table size.
async fn stream_range_resolved(
    client: &CalamineServiceClient<tonic::transport::Channel>,
    workbook_id: &str,
    sheet_index: u32,
    max_rows_per_message: u32,
) -> (Vec<pb::WorksheetRow>, usize) {
    let mut client = client.clone();
    let mut stream = client
        .stream_worksheet_range(pb::StreamWorksheetRangeRequest {
            workbook_id: workbook_id.to_string(),
            sheet: Some(pb::SheetSelector {
                selector: Some(pb::sheet_selector::Selector::SheetIndex(sheet_index)),
            }),
            max_rows_per_message,
            use_string_table: true,
        })
        .await
        .expect("stream worksheet range")
        .into_inner();

    let mut table: Vec<String> = Vec::new();
    let mut rows = Vec::new();
    while let Some(event) = stream.message().await.expect("stream event") {
        match event.event.expect("event kind") {
            pb::stream_worksheet_range_response::Event::Started(_) => {}
            pb::stream_worksheet_range_response::Event::StringTable(chunk) => {
                assert_eq!(
                    chunk.first_id as usize,
                    table.len(),
                    "chunks must arrive dense and in id order"
                );
                assert!(!chunk.entries.is_empty(), "a chunk is never empty");
                table.extend(chunk.entries);
            }
            pb::stream_worksheet_range_response::Event::Rows(batch) => {
                rows.extend(batch.rows.into_iter().map(|r| resolve_row(r, &table)));
            }
            pb::stream_worksheet_range_response::Event::Row(row) => {
                rows.push(resolve_row(row, &table));
            }
            pb::stream_worksheet_range_response::Event::RowGap(gap) => expand_gap(&mut rows, gap),
            pb::stream_worksheet_range_response::Event::Error(err) => {
                panic!("unexpected in-band error: {:?}", err.error)
            }
        }
    }
    (rows, table.len())
}

/// Dictionary mode must change the wire, never the content: resolving every
/// id against the streamed table reproduces the plain stream exactly, for
/// formats with shared strings and formats without them alike.
#[tokio::test]
async fn string_table_mode_resolves_to_the_plain_stream() {
    // (file, expects shared strings on sheet 0)
    let files = [
        ("temperature.xlsx", true),
        ("date.xlsx", false),
        ("date.xlsb", false),
        ("date.xls", false),
        ("date.ods", false),
        ("vba.xlsm", false),
    ];
    for (file, has_shared) in files {
        let client = start_server().await;
        let opened = upload(&client, file).await;
        let (_, plain) = stream_range(&client, &opened.workbook_id, 0).await;
        let (resolved, table_len) = stream_range_resolved(&client, &opened.workbook_id, 0, 0).await;

        assert_eq!(
            resolved, plain,
            "{file}: dictionary mode changed row content"
        );
        if has_shared {
            assert!(table_len > 0, "{file}: expected a non-empty string table");
        }
    }
}

/// The table works in single-row mode too: chunks still precede the `row`
/// events that reference them.
#[tokio::test]
async fn string_table_mode_works_row_per_message() {
    let client = start_server().await;
    let opened = upload(&client, "temperature.xlsx").await;
    let (_, plain) = stream_range(&client, &opened.workbook_id, 0).await;
    let (resolved, table_len) = stream_range_resolved(&client, &opened.workbook_id, 0, 1).await;

    assert_eq!(resolved, plain);
    assert!(table_len > 0);
}

/// Repeated shared strings must reference one table entry, not re-define it:
/// the table never holds more entries than the sheet has distinct strings.
#[tokio::test]
async fn string_table_deduplicates() {
    let client = start_server().await;
    let opened = upload(&client, "temperature.xlsx").await;
    let (_, plain) = stream_range(&client, &opened.workbook_id, 0).await;

    let mut distinct = std::collections::HashSet::new();
    let mut occurrences = 0usize;
    for row in &plain {
        for cell in &row.values {
            if let Some(pb::cell_data::Value::SharedStringValue(s)) = &cell.value {
                distinct.insert(s.clone());
                occurrences += 1;
            }
        }
    }
    assert!(occurrences > 0, "fixture must contain shared strings");

    let (_, table_len) = stream_range_resolved(&client, &opened.workbook_id, 0, 0).await;
    assert_eq!(
        table_len,
        distinct.len(),
        "table size must equal the distinct shared strings on the sheet"
    );
}

// ---------------------------------------------------------------------------
// Header row selection: `WorkbookOptions.header_row`.
//
// The contract (calamine_service.proto:86-89) says the setting is "applied to
// subsequent worksheet reads on this handle, mirroring
// `Reader::with_header_row`". calamine applies it where the range is
// assembled, which differs per format:
//
//   XLS  -> Xls::worksheet_range            (xls.rs:430)   applied
//   ODS  -> Ods::worksheet_range            (ods.rs:245)   applied
//   XLSX -> Xlsx::worksheet_range_ref       (xlsx/mod.rs:2652) applied
//   XLSB -> Xlsb::worksheet_range_ref       (xlsb/mod.rs:562)  applied
//
// but NOT in the incremental readers the server streams through:
// `worksheet_cells_reader` (xlsx/mod.rs:2517, xlsb/mod.rs:418) never reads
// `options.header_row`. So the XLSX/XLSB streaming path has to apply the
// selection itself, and these tests hold every format to one meaning.
// ---------------------------------------------------------------------------

/// The populated cells calamine itself reports for a sheet read with an
/// explicit header row, normalized exactly the way [`populated_cells`]
/// normalizes the streamed side.
fn expected_populated_with_header_row(
    file: &str,
    sheet_index: usize,
    header_row: HeaderRow,
) -> Vec<(u32, u32, pb::cell_data::Value)> {
    let mut workbook: Sheets<_> = open_workbook_auto(fixtures().join(file)).expect("open fixture");
    let is_1904 = convert::has_1904_epoch(&workbook);
    workbook.with_header_row(header_row);
    let name = workbook.sheet_names()[sheet_index].clone();
    let range = workbook.worksheet_range(&name).expect("worksheet range");
    let start = range.start().unwrap_or((0, 0));

    let mut cells = Vec::new();
    for (row_offset, row) in range.rows().enumerate() {
        for (col_offset, data) in row.iter().enumerate() {
            if matches!(data, Data::Empty) {
                continue;
            }
            let value = match convert::data_value(data, is_1904) {
                pb::cell_data::Value::SharedStringValue(s) => pb::cell_data::Value::StringValue(s),
                v => v,
            };
            cells.push((
                start.0 + row_offset as u32,
                start.1 + col_offset as u32,
                value,
            ));
        }
    }
    cells
}

/// `header_row = Row(n)` must drop everything above row `n`, on every format.
///
/// The `date.*` fixtures are the same three-row sheet in all four formats, so
/// asking for row 1 as the header must drop row 0 four times over. XLS and ODS
/// pass because the server streams them through `worksheet_range`, which
/// applies the setting; XLSX and XLSB stream through `worksheet_cells_reader`,
/// which does not, so the server silently returns the whole sheet.
#[tokio::test]
async fn header_row_selection_is_honoured_on_every_format() {
    const HEADER: u32 = 1;

    for file in ["date.xls", "date.ods", "date.xlsx", "date.xlsb"] {
        let client = start_server().await;
        let opened = upload_with_options(
            &client,
            file,
            pb::WorkbookOptions {
                format_hint: pb::WorkbookFormat::Unspecified as i32,
                header_row: Some(pb::HeaderRow {
                    selection: Some(pb::header_row::Selection::RowIndex(HEADER)),
                }),
            },
        )
        .await;

        let (_header, rows) = stream_range(&client, &opened.workbook_id, 0).await;
        let expected = expected_populated_with_header_row(file, 0, HeaderRow::Row(HEADER));

        assert_eq!(
            populated_cells(&rows),
            expected,
            "{file}: header_row = Row({HEADER}) was not applied to the stream"
        );
    }
}

/// The same selection, stated as the observable a caller actually reads: no
/// row above the requested header row may appear in the stream.
#[tokio::test]
async fn header_row_selection_suppresses_rows_above_it() {
    const HEADER: u32 = 1;

    for file in ["date.xls", "date.ods", "date.xlsx", "date.xlsb"] {
        let client = start_server().await;
        let opened = upload_with_options(
            &client,
            file,
            pb::WorkbookOptions {
                format_hint: pb::WorkbookFormat::Unspecified as i32,
                header_row: Some(pb::HeaderRow {
                    selection: Some(pb::header_row::Selection::RowIndex(HEADER)),
                }),
            },
        )
        .await;

        let (_header, rows) = stream_range(&client, &opened.workbook_id, 0).await;
        let above: Vec<u32> = rows
            .iter()
            .map(|r| r.row_index)
            .filter(|i| *i < HEADER)
            .collect();

        assert!(
            above.is_empty(),
            "{file}: rows {above:?} are above the requested header row {HEADER} \
             and must not stream"
        );
    }
}

/// The dictionary composes with wire compression: a zstd-negotiating client
/// in `use_string_table` mode still resolves to the identical rows.
#[tokio::test]
async fn string_table_mode_composes_with_compression() {
    let client = start_server().await;
    let opened = upload(&client, "temperature.xlsx").await;
    let (_, plain) = stream_range(&client, &opened.workbook_id, 0).await;

    let compressed_client = client
        .clone()
        .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
    let (resolved, table_len) =
        stream_range_resolved(&compressed_client, &opened.workbook_id, 0, 0).await;

    assert_eq!(resolved, plain);
    assert!(table_len > 0);
}

// ---------------------------------------------------------------------------
// Handle lifetime and admission.
//
// Nothing guarantees a client closes what it opens: a killed process, a
// partition or a timed-out CloseWorkbook each leave a handle behind, holding
// its upload and a parsed reader. Handles therefore expire when idle, the
// store refuses past its caps, and uploads are admitted against their own
// slots, which a client that stops sending cannot hold forever.
// ---------------------------------------------------------------------------

/// A handle nobody uses for the idle TTL is closed as if its client had
/// called CloseWorkbook.
#[tokio::test]
async fn an_idle_handle_is_closed_by_the_reaper() {
    let store = WorkbookStore::with_limits(StoreLimits {
        idle_ttl: Duration::from_millis(200),
        ..StoreLimits::default()
    });
    let mut client = start_server_with(CalamineGrpc::new(store)).await;
    let opened = upload(&client, "date.xlsx").await;

    // The reaper runs every 50 ms here, so this leaves over a second of slack.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let gone = client
        .get_metadata(pb::GetMetadataRequest {
            workbook_id: opened.workbook_id.clone(),
        })
        .await
        .expect_err("the handle expired");
    assert_eq!(gone.code(), Code::NotFound);
    let closed = client
        .close_workbook(pb::CloseWorkbookRequest {
            workbook_id: opened.workbook_id,
        })
        .await
        .expect("close")
        .into_inner();
    assert!(!closed.closed, "nothing is left to close");
}

/// Past the workbook cap OpenWorkbook is refused with RESOURCE_EXHAUSTED,
/// naming the setting, and closing a handle makes room again.
#[tokio::test]
async fn opens_past_the_workbook_cap_are_refused() {
    let store = WorkbookStore::with_limits(StoreLimits {
        max_open_workbooks: 1,
        ..StoreLimits::default()
    });
    let mut client = start_server_with(CalamineGrpc::new(store)).await;
    let first = upload(&client, "date.xlsx").await;

    let refused = try_upload(&client, "date.xlsx")
        .await
        .expect_err("the cap is 1");
    assert_eq!(refused.code(), Code::ResourceExhausted);
    assert!(
        refused
            .message()
            .contains("GRPC_CALAMINE_MAX_OPEN_WORKBOOKS"),
        "{}",
        refused.message()
    );

    client
        .close_workbook(pb::CloseWorkbookRequest {
            workbook_id: first.workbook_id,
        })
        .await
        .expect("close");
    upload(&client, "date.xlsx").await;
}

/// A workbook that does not fit the store's byte budget is refused with
/// RESOURCE_EXHAUSTED, naming the setting.
#[tokio::test]
async fn an_upload_past_the_byte_budget_is_refused() {
    let store = WorkbookStore::with_limits(StoreLimits {
        max_store_bytes: 1024,
        ..StoreLimits::default()
    });
    let client = start_server_with(CalamineGrpc::new(store)).await;

    let refused = try_upload(&client, "date.xlsx")
        .await
        .expect_err("4.6 KB does not fit in 1 KiB");
    assert_eq!(refused.code(), Code::ResourceExhausted);
    assert!(
        refused.message().contains("GRPC_CALAMINE_MAX_STORE_BYTES"),
        "{}",
        refused.message()
    );
}

/// An upload holds its slot only while it is moving: past the cap the next
/// one is refused, and an upload that stops sending is abandoned with
/// DEADLINE_EXCEEDED, which frees the slot.
#[tokio::test]
async fn a_stalled_upload_is_abandoned_and_frees_its_slot() {
    let grpc = CalamineGrpc::new(WorkbookStore::new())
        .with_max_concurrent_uploads(1)
        .with_upload_stall(Duration::from_secs(1));
    let client = start_server_with(grpc).await;

    // Sends its options frame, then nothing, while staying connected.
    let (frames, rx) = tokio::sync::mpsc::channel(1);
    frames
        .send(pb::OpenWorkbookRequest {
            payload: Some(pb::open_workbook_request::Payload::Options(
                default_options(),
            )),
        })
        .await
        .expect("queue the options frame");
    let mut stalled_client = client.clone();
    let stalled = tokio::spawn(async move {
        stalled_client
            .open_workbook(ReceiverStream::new(rx))
            .await
            .map(tonic::Response::into_inner)
    });

    tokio::time::sleep(Duration::from_millis(300)).await;
    let refused = try_upload(&client, "date.xlsx")
        .await
        .expect_err("the only upload slot is taken");
    assert_eq!(refused.code(), Code::ResourceExhausted);

    let abandoned = stalled
        .await
        .expect("join")
        .expect_err("a client that stops sending is not waited on forever");
    assert_eq!(abandoned.code(), Code::DeadlineExceeded);
    drop(frames);

    upload(&client, "date.xlsx").await;
}

// ---------------------------------------------------------------------------
// What calamine inflates at open time.
//
// calamine reads every embedded picture, and the shared-string table, whole
// while it opens a package, before any sheet is asked for. A few KB of
// deflated zeros can claim gigabytes there, so the server inflates those
// parts into nothing first and refuses the workbook past its limits. The
// packages below are built here rather than checked in, because what they
// test is their recipe: how far each part inflates.
// ---------------------------------------------------------------------------

/// The parts of the smallest package calamine opens as xlsx: one sheet
/// holding A1 = 1.
const MINIMAL_XLSX: [(&str, &str); 5] = [
    (
        "[Content_Types].xml",
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Default Extension="png" ContentType="image/png"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"#,
    ),
    (
        "_rels/.rels",
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#,
    ),
    (
        "xl/workbook.xml",
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#,
    ),
    (
        "xl/_rels/workbook.xml.rels",
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#,
    ),
    (
        "xl/worksheets/sheet1.xml",
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData>
</worksheet>"#,
    ),
];

/// The minimal xlsx plus `extra` parts, every part deflated.
fn xlsx_with_parts(extra: &[(&str, Vec<u8>)]) -> Vec<u8> {
    use std::io::Write;
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let deflated = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let base = MINIMAL_XLSX
        .iter()
        .map(|(name, body)| (*name, body.as_bytes()));
    let extra = extra.iter().map(|(name, body)| (*name, body.as_slice()));
    for (name, body) in base.chain(extra) {
        zip.start_file(name, deflated).expect("start part");
        zip.write_all(body).expect("write part");
    }
    zip.finish().expect("finish package").into_inner()
}

/// A server whose inflation limits are `inflate` and nothing else changed.
async fn start_server_inflating(
    inflate: InflateLimits,
) -> CalamineServiceClient<tonic::transport::Channel> {
    let store = WorkbookStore::with_limits(StoreLimits {
        inflate,
        ..StoreLimits::default()
    });
    start_server_with(CalamineGrpc::new(store)).await
}

const MIB: usize = 1024 * 1024;

/// One picture of 4 MiB of zeros deflates to a few KB, and calamine would
/// `read_to_end` it while opening the workbook, for every reader. Past the
/// per-picture limit it is refused before calamine sees it, and the server
/// goes on serving.
#[tokio::test]
async fn a_picture_bomb_is_refused_before_calamine_inflates_it() {
    let client = start_server_inflating(InflateLimits {
        max_picture_bytes: MIB as u64,
        ..InflateLimits::default()
    })
    .await;
    let bomb = xlsx_with_parts(&[("xl/media/image1.png", vec![0; 4 * MIB])]);
    assert!(bomb.len() < 64 * 1024, "the upload itself is small");

    let refused = try_upload_bytes(&client, bomb, default_options())
        .await
        .expect_err("the picture inflates past its limit");
    assert_eq!(refused.code(), Code::ResourceExhausted);
    assert!(
        refused.message().contains("xl/media/image1.png")
            && refused
                .message()
                .contains("GRPC_CALAMINE_MAX_PICTURE_BYTES"),
        "{}",
        refused.message()
    );

    upload(&client, "date.xlsx").await;
}

/// Pictures each under the per-picture limit can still add up past the
/// per-workbook one.
#[tokio::test]
async fn pictures_past_the_workbook_total_are_refused() {
    let client = start_server_inflating(InflateLimits {
        max_picture_bytes: MIB as u64,
        max_picture_total_bytes: MIB as u64,
        ..InflateLimits::default()
    })
    .await;
    let pictures: Vec<(&str, Vec<u8>)> = ["xl/media/image1.png", "xl/media/image2.png"]
        .into_iter()
        .map(|name| (name, vec![0; 3 * MIB / 4]))
        .collect();

    let refused = try_upload_bytes(&client, xlsx_with_parts(&pictures), default_options())
        .await
        .expect_err("two pictures of 0.75 MiB pass a 1 MiB total");
    assert_eq!(refused.code(), Code::ResourceExhausted);
    assert!(
        refused
            .message()
            .contains("GRPC_CALAMINE_MAX_PICTURE_TOTAL_BYTES"),
        "{}",
        refused.message()
    );
}

/// The shared-string table is read whole at open as well, into owned
/// strings, so it is held to its own limit.
#[tokio::test]
async fn a_shared_string_bomb_is_refused() {
    let client = start_server_inflating(InflateLimits {
        max_shared_strings_bytes: MIB as u64,
        ..InflateLimits::default()
    })
    .await;
    let mut table =
        br#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><si><t>"#
            .to_vec();
    table.resize(table.len() + 4 * MIB, b'A');
    table.extend_from_slice(b"</t></si></sst>");

    let refused = try_upload_bytes(
        &client,
        xlsx_with_parts(&[("xl/sharedStrings.xml", table)]),
        default_options(),
    )
    .await
    .expect_err("the table inflates past its limit");
    assert_eq!(refused.code(), Code::ResourceExhausted);
    assert!(
        refused
            .message()
            .contains("GRPC_CALAMINE_MAX_SHARED_STRINGS_BYTES"),
        "{}",
        refused.message()
    );
}

/// Pictures stay in memory with every reader, so the store charges them to
/// the workbook along with its upload: a few KB on the wire can be most of
/// a budget once inflated.
#[tokio::test]
async fn inflated_pictures_count_against_the_store_budget() {
    let store = WorkbookStore::with_limits(StoreLimits {
        max_store_bytes: (MIB / 2) as u64,
        ..StoreLimits::default()
    });
    let client = start_server_with(CalamineGrpc::new(store)).await;
    let package = xlsx_with_parts(&[("xl/media/image1.png", vec![0; MIB])]);
    assert!(package.len() < MIB / 2, "the upload alone fits");

    let refused = try_upload_bytes(&client, package, default_options())
        .await
        .expect_err("the inflated picture does not");
    assert_eq!(refused.code(), Code::ResourceExhausted);
    assert!(
        refused.message().contains("GRPC_CALAMINE_MAX_STORE_BYTES"),
        "{}",
        refused.message()
    );
}

/// Pictures within the limits open and come back intact from GetPictures.
#[tokio::test]
async fn pictures_within_the_limits_are_served() {
    let mut client = start_server().await;
    let image: Vec<u8> = (0..=255u8).cycle().take(64 * 1024).collect();
    let opened = try_upload_bytes(
        &client,
        xlsx_with_parts(&[("xl/media/image1.png", image.clone())]),
        default_options(),
    )
    .await
    .expect("open");

    let mut stream = client
        .get_pictures(pb::GetPicturesRequest {
            workbook_id: opened.workbook_id,
        })
        .await
        .expect("get pictures")
        .into_inner();
    let mut pictures = Vec::new();
    while let Some(event) = stream.message().await.expect("stream event") {
        match event.event.expect("event kind") {
            pb::get_pictures_response::Event::Picture(picture) => pictures.push(picture),
            pb::get_pictures_response::Event::Error(err) => {
                panic!("unexpected in-band error: {:?}", err.error)
            }
        }
    }
    assert_eq!(pictures.len(), 1);
    assert_eq!(pictures[0].extension, "png");
    assert_eq!(pictures[0].data, image);
}

// ---------------------------------------------------------------------------
// Ranges the server densifies.
//
// Formula streams, and every XLS and ODS stream, send each row of their range
// densely from column A, and the range is as large as its two furthest cells
// make it. A server-wide budget bounds the grid one stream produces.
// ---------------------------------------------------------------------------

/// Stream a sheet's formulas and return the header, the dense rows (each
/// anchored at column A) and the terminal status if the stream ended with one.
async fn stream_formulas(
    client: &CalamineServiceClient<tonic::transport::Channel>,
    workbook_id: &str,
    sheet_index: u32,
) -> (
    Option<pb::RangeStarted>,
    Vec<pb::FormulaRow>,
    Option<Status>,
) {
    let mut client = client.clone();
    let mut stream = client
        .stream_worksheet_formula(pb::StreamWorksheetFormulaRequest {
            workbook_id: workbook_id.to_string(),
            sheet: Some(pb::SheetSelector {
                selector: Some(pb::sheet_selector::Selector::SheetIndex(sheet_index)),
            }),
        })
        .await
        .expect("stream formulas")
        .into_inner();
    let mut header = None;
    let mut rows = Vec::new();
    loop {
        match stream.message().await {
            Ok(None) => return (header, rows, None),
            Ok(Some(event)) => match event.event.expect("event kind") {
                pb::stream_worksheet_formula_response::Event::Started(h) => header = Some(h),
                pb::stream_worksheet_formula_response::Event::Row(row) => rows.push(row),
                pb::stream_worksheet_formula_response::Event::Error(err) => {
                    panic!("unexpected in-band error: {:?}", err.error)
                }
            },
            Err(status) => return (header, rows, Some(status)),
        }
    }
}

/// The status a value stream ends with, or `None` if it ends cleanly.
async fn range_stream_status(
    client: &CalamineServiceClient<tonic::transport::Channel>,
    workbook_id: &str,
) -> Option<Status> {
    let mut client = client.clone();
    let mut stream = client
        .stream_worksheet_range(pb::StreamWorksheetRangeRequest {
            workbook_id: workbook_id.to_string(),
            sheet: Some(pb::SheetSelector {
                selector: Some(pb::sheet_selector::Selector::SheetIndex(0)),
            }),
            max_rows_per_message: 0,
            use_string_table: false,
        })
        .await
        .expect("stream worksheet range")
        .into_inner();
    loop {
        match stream.message().await {
            Ok(None) => return None,
            Ok(Some(_)) => {}
            Err(status) => return Some(status),
        }
    }
}

/// xlsx and xlsb formulas are collected cell by cell rather than through
/// calamine's dense `worksheet_formula`, and must come out exactly as it
/// would give them, on every format, chartsheets and formula-free sheets
/// included.
#[tokio::test]
async fn formula_streams_match_calamine_on_every_format() {
    for file in [
        "formula.issue.xlsx",
        "any_sheets.xlsx",
        "date.xlsx",
        "date.xlsb",
        "date.xls",
        "date.ods",
    ] {
        let client = start_server().await;
        let opened = upload(&client, file).await;
        let mut workbook: Sheets<_> =
            open_workbook_auto(fixtures().join(file)).expect("open fixture");
        let names = workbook.sheet_names().to_vec();
        for (index, name) in names.iter().enumerate() {
            let range = workbook.worksheet_formula(name).expect("formulas");
            let (header, rows, status) =
                stream_formulas(&client, &opened.workbook_id, index as u32).await;
            assert!(status.is_none(), "{file}/{name}: {status:?}");
            let header = header.expect("a header first");
            assert_eq!(header.sheet_name, *name);

            let expected_dims = range
                .start()
                .zip(range.end())
                .map(|(start, end)| pb::Dimensions {
                    start: Some(convert::cell_position(start)),
                    end: Some(convert::cell_position(end)),
                });
            assert_eq!(header.dimensions, expected_dims, "{file}/{name}");
            let (height, width) = range.get_size();
            assert_eq!(header.total_cells, (height * width) as u64, "{file}/{name}");

            let start = range.start().unwrap_or_default();
            let expected: Vec<(u32, Vec<String>)> = range
                .rows()
                .enumerate()
                .map(|(offset, row)| {
                    let mut formulas = vec![String::new(); start.1 as usize];
                    formulas.extend_from_slice(row);
                    (start.0 + offset as u32, formulas)
                })
                .collect();
            let got: Vec<(u32, Vec<String>)> = rows
                .into_iter()
                .map(|row| (row.row_index, row.formulas))
                .collect();
            assert_eq!(got, expected, "{file}/{name}");
        }
    }
}

/// Two formulas at opposite corners make a 1,048,576 x 16,384 range. calamine
/// would densify it into 17 billion strings before the first row; the server
/// refuses it from the cells' positions alone, before any event.
#[tokio::test]
async fn formulas_at_opposite_corners_are_refused() {
    let client = start_server().await;
    let opened = upload(&client, "corners_formula.xlsx").await;
    let (header, rows, status) = stream_formulas(&client, &opened.workbook_id, 0).await;

    assert!(
        header.is_none() && rows.is_empty(),
        "refused before any event"
    );
    let status = status.expect("a terminal status");
    assert_eq!(status.code(), Code::ResourceExhausted);
    assert!(
        status.message().contains("GRPC_CALAMINE_MAX_DENSE_CELLS"),
        "{}",
        status.message()
    );
}

/// The budget holds on every path that streams a dense range: XLS and ODS
/// values, and formulas.
#[tokio::test]
async fn ranges_past_the_dense_budget_are_refused() {
    let client =
        start_server_with(CalamineGrpc::new(WorkbookStore::new()).with_max_dense_cells(5)).await;
    for file in ["date.xls", "date.ods"] {
        let opened = upload(&client, file).await;
        let status = range_stream_status(&client, &opened.workbook_id)
            .await
            .unwrap_or_else(|| panic!("{file}: a range of more than 5 cells streamed"));
        assert_eq!(status.code(), Code::ResourceExhausted, "{file}");
    }
    let opened = upload(&client, "formula.issue.xlsx").await;
    let (_, _, status) = stream_formulas(&client, &opened.workbook_id, 0).await;
    assert_eq!(
        status.expect("a 14 x 10 formula range").code(),
        Code::ResourceExhausted
    );

    // The value stream of an xlsx sheet never densifies, so it is not held to
    // the budget at all.
    let opened = upload(&client, "date.xlsx").await;
    assert!(
        range_stream_status(&client, &opened.workbook_id)
            .await
            .is_none()
    );
}

/// ODS is densified by calamine while the workbook is opened, before the
/// server sees a sheet, so its only bound there is calamine's own cell cap:
/// the corners as ODS are refused at OpenWorkbook, and the server goes on.
#[tokio::test]
async fn ods_corners_are_refused_when_opened() {
    let client = start_server().await;
    let refused = try_upload(&client, "corners.ods")
        .await
        .expect_err("past calamine's cell cap");
    assert_eq!(refused.code(), Code::InvalidArgument);
    upload(&client, "date.ods").await;
}

/// A picture too large for one gRPC message is reported in-band and skipped,
/// and the pictures around it are still delivered. It used to fail the
/// encode and end the stream, losing every picture after it.
#[tokio::test]
async fn a_picture_too_large_for_one_message_is_skipped_not_fatal() {
    use std::io::Write;
    // Stored rather than deflated: building it should cost a copy, not a
    // compression pass over 33 MiB.
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let deflated = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, body) in MINIMAL_XLSX {
        zip.start_file(name, deflated).expect("start part");
        zip.write_all(body.as_bytes()).expect("write part");
    }
    zip.start_file("xl/media/image1.png", stored)
        .expect("start picture");
    zip.write_all(&vec![7; 33 * MIB]).expect("write picture");
    zip.start_file("xl/media/image2.png", deflated)
        .expect("start picture");
    zip.write_all(&[9; 1024]).expect("write picture");
    let package = zip.finish().expect("finish package").into_inner();

    let mut client = start_server().await;
    let opened = try_upload_bytes(&client, package, default_options())
        .await
        .expect("33 MiB is within the picture limits");
    let mut stream = client
        .get_pictures(pb::GetPicturesRequest {
            workbook_id: opened.workbook_id,
        })
        .await
        .expect("get pictures")
        .into_inner();

    let mut pictures = Vec::new();
    let mut skipped = Vec::new();
    while let Some(event) = stream.message().await.expect("the stream survives") {
        match event.event.expect("event kind") {
            pb::get_pictures_response::Event::Picture(picture) => pictures.push(picture),
            pb::get_pictures_response::Event::Error(err) => skipped.push(err),
        }
    }
    assert_eq!(pictures.len(), 1, "the small picture still arrives");
    assert_eq!(pictures[0].data, vec![9; 1024]);
    assert_eq!(skipped.len(), 1, "the large one is reported");
    assert!(!skipped[0].terminal);
}

// ---------------------------------------------------------------------------
// Declared counts and anchor multiplication the pre-open scan catches, each a
// way a crafted file drives calamine to allocate from a number it never
// checks. Both are refused before calamine parses the file.
// ---------------------------------------------------------------------------

/// A 1.8 KB workbook whose `sharedStrings.xml` declares a trillion unique
/// strings. calamine would `reserve(1_000_000_000_000)` (24 TB) before reading
/// a single entry and abort the process; the scan refuses it as malformed
/// (INVALID_ARGUMENT) because the table's few inflated bytes cannot hold that
/// many strings. The server keeps serving afterward.
#[tokio::test]
async fn a_lying_shared_string_count_is_refused() {
    let table = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="1" uniqueCount="1000000000000"><si><t>a</t></si></sst>"#
        .to_vec();
    let package = xlsx_with_parts(&[("xl/sharedStrings.xml", table)]);
    assert!(package.len() < 4096, "the upload itself is tiny");

    let mut client = start_server().await;
    let refused = try_upload_bytes(&client, package, default_options())
        .await
        .expect_err("a trillion declared strings in 1.8 KB is a lie");
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert!(
        refused.message().contains("uniqueCount") && refused.message().contains("shared strings"),
        "{}",
        refused.message()
    );

    upload(&client, "date.xlsx").await;
    let probe = client
        .get_metadata(pb::GetMetadataRequest::default())
        .await
        .expect("the server still answers");
    assert!(probe.into_inner().ui.is_some());
}

/// A legitimate `uniqueCount` opens: the guard is a ceiling, not a cap on
/// real tables.
#[tokio::test]
async fn an_honest_shared_string_count_opens() {
    let table = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="2" uniqueCount="2"><si><t>alpha</t></si><si><t>beta</t></si></sst>"#
        .to_vec();
    let package = xlsx_with_parts(&[("xl/sharedStrings.xml", table)]);
    let client = start_server().await;
    try_upload_bytes(&client, package, default_options())
        .await
        .expect("an honest table opens");
}

/// Build a workbook with one media entry embedded by `anchors` drawing
/// anchors, all pointing at the same image through the drawing's rels.
fn xlsx_with_anchored_picture(image: Vec<u8>, anchors: usize) -> Vec<u8> {
    let rels = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/>
</Relationships>"#
        .to_vec();
    let mut drawing = String::from(
        r#"<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
    );
    for _ in 0..anchors {
        drawing.push_str(
            r#"<xdr:oneCellAnchor><xdr:pic><xdr:blipFill><a:blip r:embed="rId1"/></xdr:blipFill></xdr:pic></xdr:oneCellAnchor>"#,
        );
    }
    drawing.push_str("</xdr:wsDr>");
    xlsx_with_parts(&[
        ("xl/media/image1.png", image),
        ("xl/drawings/drawing1.xml", drawing.into_bytes()),
        ("xl/drawings/_rels/drawing1.xml.rels", rels),
    ])
}

/// calamine clones a picture's bytes once per anchor that embeds it, so a
/// small image referenced by many anchors is many copies in memory. The scan
/// charges each anchor against the per-workbook picture budget: one anchor
/// fits, many do not, though the distinct image is unchanged.
#[tokio::test]
async fn one_picture_multiplied_across_anchors_is_refused() {
    let client = start_server_inflating(InflateLimits {
        max_picture_bytes: (512 * 1024) as u64,
        max_picture_total_bytes: MIB as u64,
        ..InflateLimits::default()
    })
    .await;
    // 256 KiB of zeros deflates to almost nothing, so neither upload is large.
    let image = vec![0u8; 256 * 1024];

    try_upload_bytes(
        &client,
        xlsx_with_anchored_picture(image.clone(), 1),
        default_options(),
    )
    .await
    .expect("one 256 KiB copy fits a 1 MiB budget");

    let bomb = xlsx_with_anchored_picture(image, 8);
    assert!(
        bomb.len() < 64 * 1024,
        "eight anchors to one image stay tiny on the wire"
    );
    let refused = try_upload_bytes(&client, bomb, default_options())
        .await
        .expect_err("eight 256 KiB copies do not");
    assert_eq!(refused.code(), Code::ResourceExhausted);
    assert!(
        refused
            .message()
            .contains("GRPC_CALAMINE_MAX_PICTURE_TOTAL_BYTES"),
        "{}",
        refused.message()
    );
}
