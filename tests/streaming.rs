// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests: a real tonic server on an ephemeral port, the generated
//! protobuf client, and real workbook files from the calamine test suite.
//!
//! Every streamed cell is compared against calamine's own `worksheet_range`
//! output, so the tests assert that the wire stream is a faithful rendering
//! of what calamine parsed.

use std::path::PathBuf;

use calamine::{Data, HeaderRow, Reader, Sheets, open_workbook_auto};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::Code;
use tonic::transport::{Endpoint, Server};

use grpc_calamine::proto::v1 as pb;
use grpc_calamine::proto::v1::calamine_service_client::CalamineServiceClient;
use grpc_calamine::{CalamineGrpc, WorkbookStore, convert};

/// Directory holding the workbook fixtures (originally from the calamine
/// test suite, MIT licensed).
fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("demos/sample-data")
}

/// Start the server on an ephemeral localhost port and return a connected
/// client.
async fn start_server() -> CalamineServiceClient<tonic::transport::Channel> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().unwrap();
    let service = CalamineGrpc::new(WorkbookStore::new()).into_service();
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
    upload_with_options(
        client,
        file,
        pb::WorkbookOptions {
            format_hint: pb::WorkbookFormat::Unspecified as i32,
            header_row: None,
        },
    )
    .await
}

/// Upload a workbook file with explicit open-time options.
async fn upload_with_options(
    client: &CalamineServiceClient<tonic::transport::Channel>,
    file: &str,
    options: pb::WorkbookOptions,
) -> pb::OpenWorkbookResponse {
    let mut client = client.clone();
    let bytes = std::fs::read(fixtures().join(file)).expect("read fixture");
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
        .expect("open workbook")
        .into_inner()
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

    // The response carries the shared-shell frontend advertisement.
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
        // Deliberately absent: `dimension_reversed.xlsx`, whose declaration
        // underflows calamine's own unchecked corner subtraction, so building
        // the ground truth here would panic before the server was asked
        // anything (it has its own test); and `rows_late_backwards.xlsx`,
        // which calamine reads and a one-pass stream cannot (likewise).
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
                 stream errored: {:?}",
                error
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

/// The declared `<dimension>` must not size the server's row buffer.
///
/// `emit_incremental` pre-sizes each row from the declaration
/// (`width = dims.end.1 as usize + 1`, service.rs:532) and then allocates
/// `vec![empty_cell_data(); width]` (service.rs:534) before reading a single
/// cell. That end column comes straight out of the uploaded file, and calamine
/// does not clamp it: `get_dimension` only logs a warning past its 16,384
/// column limit (xlsx/mod.rs:2794). The base-26 parse accepts far more, so
/// `<dimension ref="A1:ZZZZZZ1"/>` in a 2 KB upload is column 321,272,405 and
/// commits roughly 10 GB before any work happens.
///
/// This fixture uses 200,000 columns over a single cell at A1: enough to
/// demonstrate the same path, small enough that the suite stays safe. calamine
/// reports a 1x1 range for it, so the emitted row should be one cell wide.
#[tokio::test]
async fn a_wide_declared_dimension_does_not_size_the_row_buffer() {
    let client = start_server().await;
    let opened = upload(&client, "dimension_wide.xlsx").await;
    let (_header, rows) = stream_range(&client, &opened.workbook_id, 0).await;

    assert_eq!(rows.len(), 1, "the sheet holds exactly one populated row");
    assert_eq!(
        rows[0].values.len(),
        1,
        "the row is sized from the declared 200,000 column extent rather than \
         from the single cell present, so the declaration controls how much \
         the server allocates"
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
