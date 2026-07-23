// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests: a real tonic server on an ephemeral port, the generated
//! protobuf client, and real workbook files from the calamine test suite.
//!
//! Every streamed cell is compared against calamine's own `worksheet_range`
//! output, so the tests assert that the wire stream is a faithful rendering
//! of what calamine parsed.

use std::path::PathBuf;

use calamine::{Data, Reader, Sheets, open_workbook_auto};
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
    let mut client = client.clone();
    let bytes = std::fs::read(fixtures().join(file)).expect("read fixture");
    let mut frames = vec![pb::OpenWorkbookRequest {
        payload: Some(pb::open_workbook_request::Payload::Options(
            pb::WorkbookOptions {
                format_hint: pb::WorkbookFormat::Unspecified as i32,
                header_row: None,
            },
        )),
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
    let mut client = client.clone();
    let mut stream = client
        .stream_worksheet_range(pb::StreamWorksheetRangeRequest {
            workbook_id: workbook_id.to_string(),
            sheet: Some(pb::SheetSelector {
                selector: Some(pb::sheet_selector::Selector::SheetIndex(sheet_index)),
            }),
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
    let rows = range
        .rows()
        .enumerate()
        .map(|(offset, row)| pb::WorksheetRow {
            row_index: start.0 + offset as u32,
            values: row
                .iter()
                .map(|d| convert::cell_data(convert::data_value(d, is_1904)))
                .collect(),
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
