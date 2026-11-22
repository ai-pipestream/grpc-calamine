// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests of `ParseWorkbook`, the handle-free bidirectional stream.
//!
//! The reference for every part is the per-part RPC over an `OpenWorkbook`
//! handle: the combined stream must carry exactly what those RPCs carry, in
//! the order the contract fixes, whatever the chunking of the upload.

use std::io::{Cursor, Write};
use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::transport::{Channel, Endpoint, Server};
use tonic::{Code, Status, Streaming};

use grpc_calamine::proto::v1 as pb;
use grpc_calamine::proto::v1::calamine_service_client::CalamineServiceClient;
use grpc_calamine::store::StoreLimits;
use grpc_calamine::{CalamineGrpc, WorkbookStore};

use pb::parse_workbook_request::Frame;
use pb::parse_workbook_response::Event;

type Client = CalamineServiceClient<Channel>;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("demos/sample-data")
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(fixtures().join(name)).expect("read fixture")
}

async fn start_server_with(grpc: CalamineGrpc) -> Client {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().unwrap();
    let service = grpc.into_service();
    tokio::spawn(async move {
        Server::builder()
            .add_service(service)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .expect("server failed");
    });
    let channel = Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .expect("connect to server");
    CalamineServiceClient::new(channel)
}

async fn start_server() -> Client {
    start_server_with(CalamineGrpc::new(WorkbookStore::new())).await
}

/// Every part, one row per message so the batching cannot differ between two
/// streams of the same sheet.
fn every_part() -> pb::ParseWorkbookOpen {
    pb::ParseWorkbookOpen {
        emit_ranges: true,
        emit_formulas: true,
        emit_defined_names: true,
        emit_pictures: true,
        emit_vba: true,
        range_options: Some(pb::ParseWorkbookRangeOptions {
            max_rows_per_message: 1,
            use_string_table: false,
            include_formulas: false,
        }),
        ..Default::default()
    }
}

fn open_frame(open: pb::ParseWorkbookOpen) -> pb::ParseWorkbookRequest {
    pb::ParseWorkbookRequest {
        frame: Some(Frame::Open(open)),
    }
}

fn chunk_frame(data: &[u8], last: bool) -> pb::ParseWorkbookRequest {
    pb::ParseWorkbookRequest {
        frame: Some(Frame::Chunk(pb::WorkbookChunk {
            data: data.to_vec(),
            last,
        })),
    }
}

fn cancel_frame() -> pb::ParseWorkbookRequest {
    pb::ParseWorkbookRequest {
        frame: Some(Frame::Cancel(pb::ParseWorkbookCancel {
            reason: "test".into(),
        })),
    }
}

/// The upload frames for `bytes` in `chunk`-byte pieces. With `mark_last`
/// the final chunk carries the marker; without it the half-close ends the
/// upload.
fn upload_frames(
    open: pb::ParseWorkbookOpen,
    bytes: &[u8],
    chunk: usize,
    mark_last: bool,
) -> Vec<pb::ParseWorkbookRequest> {
    let mut frames = vec![open_frame(open)];
    let pieces: Vec<&[u8]> = bytes.chunks(chunk).collect();
    for (i, piece) in pieces.iter().enumerate() {
        frames.push(chunk_frame(piece, mark_last && i + 1 == pieces.len()));
    }
    frames
}

/// Send `frames`, half-close, and collect the stream to its end.
async fn parse_frames(
    client: &Client,
    frames: Vec<pb::ParseWorkbookRequest>,
) -> Result<Vec<Event>, Status> {
    let mut client = client.clone();
    let mut stream = client
        .parse_workbook(tokio_stream::iter(frames))
        .await?
        .into_inner();
    let mut events = Vec::new();
    while let Some(message) = stream.message().await? {
        events.push(message.event.expect("event kind"));
    }
    Ok(events)
}

async fn parse(client: &Client, open: pb::ParseWorkbookOpen, bytes: &[u8]) -> Vec<Event> {
    parse_frames(client, upload_frames(open, bytes, 64 * 1024, false))
        .await
        .expect("parse workbook")
}

/// The terminal `done` of a successful stream, checked to be last.
fn done(events: &[Event]) -> pb::ParseWorkbookDone {
    match events.last() {
        Some(Event::Done(done)) => *done,
        other => panic!("expected `done` last, got {other:?}"),
    }
}

/// What the per-part RPCs answer for `bytes`, as the events `ParseWorkbook`
/// must carry for `every_part()`.
async fn reference_events(client: &Client, bytes: &[u8]) -> Vec<Event> {
    let mut client = client.clone();
    let mut frames = vec![pb::OpenWorkbookRequest {
        payload: Some(pb::open_workbook_request::Payload::Options(
            pb::WorkbookOptions::default(),
        )),
    }];
    frames.extend(bytes.chunks(64 * 1024).map(|c| pb::OpenWorkbookRequest {
        payload: Some(pb::open_workbook_request::Payload::Chunk(c.to_vec())),
    }));
    let opened = client
        .open_workbook(tokio_stream::iter(frames))
        .await
        .expect("open workbook")
        .into_inner();
    let id = opened.workbook_id.clone();
    let metadata = opened.metadata.clone().expect("metadata");

    let mut events = vec![
        Event::Metadata(pb::WorkbookMetadata {
            detected_format: opened.detected_format,
            metadata: Some(metadata.clone()),
        }),
        Event::DefinedNames(pb::WorkbookDefinedNames {
            defined_names: client
                .get_defined_names(pb::GetDefinedNamesRequest {
                    workbook_id: id.clone(),
                })
                .await
                .expect("defined names")
                .into_inner()
                .defined_names,
        }),
    ];
    for sheet_index in 0..metadata.sheets.len() as u32 {
        let sheet = Some(pb::SheetSelector {
            selector: Some(pb::sheet_selector::Selector::SheetIndex(sheet_index)),
        });
        let mut range = client
            .stream_worksheet_range(pb::StreamWorksheetRangeRequest {
                workbook_id: id.clone(),
                sheet: sheet.clone(),
                max_rows_per_message: 1,
                ..Default::default()
            })
            .await
            .expect("range")
            .into_inner();
        while let Some(event) = range.message().await.expect("range event") {
            events.push(Event::Range(pb::SheetRangeEvent {
                sheet_index,
                event: Some(event),
            }));
        }
        let mut formula = client
            .stream_worksheet_formula(pb::StreamWorksheetFormulaRequest {
                workbook_id: id.clone(),
                sheet,
            })
            .await
            .expect("formula")
            .into_inner();
        while let Some(event) = formula.message().await.expect("formula event") {
            events.push(Event::Formula(pb::SheetFormulaEvent {
                sheet_index,
                event: Some(event),
            }));
        }
    }
    let mut pictures = client
        .get_pictures(pb::GetPicturesRequest {
            workbook_id: id.clone(),
        })
        .await
        .expect("pictures")
        .into_inner();
    while let Some(event) = pictures.message().await.expect("picture event") {
        events.push(Event::Picture(event));
    }
    let mut vba = client
        .stream_vba_project(pb::StreamVbaProjectRequest {
            workbook_id: id.clone(),
        })
        .await
        .expect("vba")
        .into_inner();
    while let Some(event) = vba.message().await.expect("vba event") {
        events.push(Event::Vba(event));
    }
    client
        .close_workbook(pb::CloseWorkbookRequest { workbook_id: id })
        .await
        .expect("close");
    events
}

/// A relationships part with one relationship of `kind` to `target`.
fn rels(kind: &str, target: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/{kind}" Target="{target}"/>
</Relationships>"#
    )
    .into_bytes()
}

fn zip_parts(parts: Vec<(&str, Vec<u8>)>) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let deflated = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, body) in parts {
        zip.start_file(name, deflated).expect("start part");
        zip.write_all(&body).expect("write part");
    }
    zip.finish().expect("finish package").into_inner()
}

const CONTENT_TYPES: &[u8] = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Default Extension="png" ContentType="image/png"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
</Types>"#;

/// Two sheets, a defined name, formulas on the second sheet, and two
/// pictures anchored on the first: every part but VBA in one small package.
fn picture_workbook() -> Vec<u8> {
    let drawing = br#"<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><xdr:oneCellAnchor><xdr:from><xdr:col>1</xdr:col><xdr:row>2</xdr:row></xdr:from><xdr:pic><xdr:blipFill><a:blip r:embed="rId1"/></xdr:blipFill></xdr:pic></xdr:oneCellAnchor><xdr:oneCellAnchor><xdr:from><xdr:col>3</xdr:col><xdr:row>4</xdr:row></xdr:from><xdr:pic><xdr:blipFill><a:blip r:embed="rId1"/></xdr:blipFill></xdr:pic></xdr:oneCellAnchor></xdr:wsDr>"#;
    zip_parts(vec![
        ("[Content_Types].xml", CONTENT_TYPES.to_vec()),
        ("_rels/.rels", rels("officeDocument", "xl/workbook.xml")),
        (
            "xl/workbook.xml",
            br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Data" sheetId="1" r:id="rId1"/><sheet name="Calc" sheetId="2" r:id="rId2"/></sheets>
<definedNames><definedName name="Total">Calc!$A$1</definedName></definedNames>
</workbook>"#
                .to_vec(),
        ),
        (
            "xl/_rels/workbook.xml.rels",
            br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/>
</Relationships>"#
                .to_vec(),
        ),
        (
            "xl/worksheets/sheet1.xml",
            br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData><row r="1"><c r="A1"><v>1</v></c><c r="B1"><v>2</v></c></row><row r="3"><c r="A3"><v>3</v></c></row></sheetData>
</worksheet>"#
                .to_vec(),
        ),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            rels("drawing", "../drawings/drawing1.xml"),
        ),
        (
            "xl/worksheets/sheet2.xml",
            br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData><row r="1"><c r="A1"><f>Data!A1+Data!B1</f><v>3</v></c><c r="B1"><f>A1*2</f><v>6</v></c></row></sheetData>
</worksheet>"#
                .to_vec(),
        ),
        ("xl/drawings/drawing1.xml", drawing.to_vec()),
        (
            "xl/drawings/_rels/drawing1.xml.rels",
            rels("image", "../media/image1.png"),
        ),
        ("xl/media/image1.png", b"\x89PNG\r\n\x1a\nnot really".to_vec()),
    ])
}

/// One sheet of `rows` numeric rows, enough events to cancel in the middle.
fn tall_workbook(rows: u32) -> Vec<u8> {
    let mut sheet = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>"#,
    );
    for r in 1..=rows {
        sheet.push_str(&format!(r#"<row r="{r}"><c r="A{r}"><v>{r}</v></c></row>"#));
    }
    sheet.push_str("</sheetData></worksheet>");
    zip_parts(vec![
        ("[Content_Types].xml", CONTENT_TYPES.to_vec()),
        ("_rels/.rels", rels("officeDocument", "xl/workbook.xml")),
        (
            "xl/workbook.xml",
            br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Tall" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#
                .to_vec(),
        ),
        (
            "xl/_rels/workbook.xml.rels",
            rels("worksheet", "worksheets/sheet1.xml"),
        ),
        ("xl/worksheets/sheet1.xml", sheet.into_bytes()),
    ])
}

/// Every part of every workbook matches the per-part RPCs event for event,
/// for every format, and `done` counts what was sent.
#[tokio::test]
async fn every_part_matches_the_per_part_rpcs() {
    let client = start_server().await;
    let workbooks = [
        ("vba.xlsm", fixture("vba.xlsm")),
        ("formula.issue.xlsx", fixture("formula.issue.xlsx")),
        ("date.xlsb", fixture("date.xlsb")),
        ("date.xls", fixture("date.xls")),
        ("date.ods", fixture("date.ods")),
        ("merge_cells.xlsx", fixture("merge_cells.xlsx")),
        ("pictures", picture_workbook()),
    ];
    for (name, bytes) in workbooks {
        let expected = reference_events(&client, &bytes).await;
        let mut events = parse(&client, every_part(), &bytes).await;
        let done = done(&events);
        events.pop();
        assert_eq!(
            events, expected,
            "{name}: parts differ from the per-part RPCs"
        );
        assert!(!done.cancelled, "{name}");

        let pictures = events
            .iter()
            .filter(|e| {
                matches!(e, Event::Picture(p) if matches!(p.event, Some(pb::get_pictures_response::Event::Picture(_))))
            })
            .count();
        let modules = events
            .iter()
            .filter(|e| {
                matches!(e, Event::Vba(v) if matches!(v.event, Some(pb::stream_vba_project_response::Event::Module(_))))
            })
            .count();
        assert_eq!(done.pictures as usize, pictures, "{name}");
        assert_eq!(done.vba_modules as usize, modules, "{name}");
        let sheets = match &events[0] {
            Event::Metadata(m) => m.metadata.as_ref().unwrap().sheets.len() as u32,
            other => panic!("{name}: expected metadata first, got {other:?}"),
        };
        assert_eq!(done.range_sheets, sheets, "{name}");
        assert_eq!(done.formula_sheets, sheets, "{name}");
    }
}

/// The synthetic workbook really exercises the parts it is there for.
#[tokio::test]
async fn pictures_names_and_formulas_all_arrive() {
    let client = start_server().await;
    let events = parse(&client, every_part(), &picture_workbook()).await;
    let done = done(&events);
    assert_eq!(done.pictures, 2);
    assert_eq!(done.part_errors, 0);
    assert!(events.iter().any(|e| matches!(
        e,
        Event::DefinedNames(n) if n.defined_names.iter().any(|d| d.name == "Total")
    )));
    let formulas: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            Event::Formula(pb::SheetFormulaEvent {
                sheet_index: 1,
                event:
                    Some(pb::StreamWorksheetFormulaResponse {
                        event: Some(pb::stream_worksheet_formula_response::Event::Row(row)),
                    }),
            }) => Some(row.formulas.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(formulas, ["Data!A1+Data!B1", "A1*2"]);
    // VBA was asked for: a workbook without a project still says so.
    assert!(events.iter().any(|e| matches!(
        e,
        Event::Vba(pb::StreamVbaProjectResponse {
            event: Some(pb::stream_vba_project_response::Event::Info(info)),
        }) if !info.present
    )));
}

/// Only the parts asked for are sent; metadata and `done` always are.
#[tokio::test]
async fn only_selected_parts_are_sent() {
    let client = start_server().await;
    let events = parse(
        &client,
        pb::ParseWorkbookOpen::default(),
        &picture_workbook(),
    )
    .await;
    assert_eq!(events.len(), 2, "{events:?}");
    assert!(matches!(events[0], Event::Metadata(_)));
    assert!(matches!(events[1], Event::Done(_)));

    let events = parse(
        &client,
        pb::ParseWorkbookOpen {
            emit_pictures: true,
            ..Default::default()
        },
        &picture_workbook(),
    )
    .await;
    assert!(
        events[1..events.len() - 1]
            .iter()
            .all(|e| matches!(e, Event::Picture(_)))
    );
    assert_eq!(done(&events).pictures, 2);
}

/// Where the chunk boundaries fall, and whether the upload ends on `last` or
/// on the half-close, changes nothing.
#[tokio::test]
async fn chunking_does_not_change_the_events() {
    let client = start_server().await;
    let bytes = fixture("date.xlsx");
    let reference = parse(&client, every_part(), &bytes).await;
    for (chunk, mark_last) in [(1, false), (7, true), (4096, true), (bytes.len(), true)] {
        let events = parse_frames(
            &client,
            upload_frames(every_part(), &bytes, chunk, mark_last),
        )
        .await
        .expect("parse workbook");
        assert_eq!(events, reference, "chunk {chunk}, last {mark_last}");
    }
    // A closing marker may carry no bytes of its own.
    let mut frames = upload_frames(every_part(), &bytes, 1000, false);
    frames.push(chunk_frame(&[], true));
    let events = parse_frames(&client, frames).await.expect("parse workbook");
    assert_eq!(events, reference);
}

/// Sheets are emitted in selection order, by name or by position.
#[tokio::test]
async fn sheets_are_emitted_in_selection_order() {
    let client = start_server().await;
    let by = |selector| pb::SheetSelector {
        selector: Some(selector),
    };
    let events = parse(
        &client,
        pb::ParseWorkbookOpen {
            emit_ranges: true,
            sheets: vec![
                by(pb::sheet_selector::Selector::SheetName("Calc".into())),
                by(pb::sheet_selector::Selector::SheetIndex(0)),
            ],
            ..Default::default()
        },
        &picture_workbook(),
    )
    .await;
    let started: Vec<(u32, String)> = events
        .iter()
        .filter_map(|e| match e {
            Event::Range(pb::SheetRangeEvent {
                sheet_index,
                event:
                    Some(pb::StreamWorksheetRangeResponse {
                        event: Some(pb::stream_worksheet_range_response::Event::Started(h)),
                    }),
            }) => Some((*sheet_index, h.sheet_name.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(started, [(1, "Calc".into()), (0, "Data".into())]);
    assert_eq!(done(&events).range_sheets, 2);
}

/// A selector naming no sheet fails the call before any part.
#[tokio::test]
async fn unknown_sheet_is_not_found() {
    let client = start_server().await;
    for selector in [
        pb::sheet_selector::Selector::SheetName("Nope".into()),
        pb::sheet_selector::Selector::SheetIndex(9),
    ] {
        let open = pb::ParseWorkbookOpen {
            emit_ranges: true,
            sheets: vec![pb::SheetSelector {
                selector: Some(selector),
            }],
            ..Default::default()
        };
        let err = parse_frames(
            &client,
            upload_frames(open, &picture_workbook(), 4096, false),
        )
        .await
        .expect_err("unknown sheet");
        assert_eq!(err.code(), Code::NotFound, "{err:?}");
    }
}

/// Frames out of order are refused.
#[tokio::test]
async fn frame_order_is_enforced() {
    let client = start_server().await;
    let bytes = fixture("date.xlsx");
    let cases: Vec<(&str, Vec<pb::ParseWorkbookRequest>)> = vec![
        ("no frames", vec![]),
        ("chunk first", vec![chunk_frame(&bytes, true)]),
        ("cancel first", vec![cancel_frame()]),
        (
            "open twice",
            vec![
                open_frame(every_part()),
                open_frame(every_part()),
                chunk_frame(&bytes, true),
            ],
        ),
        (
            "empty chunk",
            vec![
                open_frame(every_part()),
                chunk_frame(&[], false),
                chunk_frame(&bytes, true),
            ],
        ),
        ("no bytes", vec![open_frame(every_part())]),
        (
            "only an empty marker",
            vec![open_frame(every_part()), chunk_frame(&[], true)],
        ),
        (
            "not a workbook",
            vec![open_frame(every_part()), chunk_frame(b"plain text", true)],
        ),
    ];
    for (name, frames) in cases {
        let err = parse_frames(&client, frames).await.expect_err(name);
        assert_eq!(err.code(), Code::InvalidArgument, "{name}: {err:?}");
    }
}

/// A chunk after the one marked `last` refuses the stream rather than being
/// ignored; only `cancel` may follow it.
#[tokio::test]
async fn only_cancel_may_follow_last() {
    let client = start_server().await;
    let (tx, rx) = mpsc::channel(8);
    let mut c = client.clone();
    tx.send(open_frame(pb::ParseWorkbookOpen {
        emit_ranges: true,
        range_options: Some(pb::ParseWorkbookRangeOptions {
            max_rows_per_message: 1,
            ..Default::default()
        }),
        ..Default::default()
    }))
    .await
    .unwrap();
    tx.send(chunk_frame(&tall_workbook(50_000), true))
        .await
        .unwrap();
    let mut stream = c
        .parse_workbook(ReceiverStream::new(rx))
        .await
        .expect("call")
        .into_inner();
    let first = stream.message().await.expect("metadata").expect("event");
    assert!(matches!(first.event, Some(Event::Metadata(_))));
    tx.send(chunk_frame(b"more", false)).await.unwrap();
    let err = drain(&mut stream).await.expect_err("refused");
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
}

/// Read a stream to its end.
async fn drain(stream: &mut Streaming<pb::ParseWorkbookResponse>) -> Result<Vec<Event>, Status> {
    let mut events = Vec::new();
    while let Some(message) = stream.message().await? {
        events.push(message.event.expect("event kind"));
    }
    Ok(events)
}

/// A cancel in the middle of a part stops the stream at the next event
/// boundary and ends it with `done` marked cancelled, long before the sheet
/// is through.
#[tokio::test]
async fn cancel_mid_stream_ends_with_done() {
    const ROWS: u32 = 50_000;
    let client = start_server().await;
    let (tx, rx) = mpsc::channel(8);
    let mut c = client.clone();
    let open = pb::ParseWorkbookOpen {
        emit_ranges: true,
        emit_formulas: true,
        range_options: Some(pb::ParseWorkbookRangeOptions {
            max_rows_per_message: 1,
            ..Default::default()
        }),
        ..Default::default()
    };
    for frame in upload_frames(open, &tall_workbook(ROWS), 64 * 1024, true) {
        tx.send(frame).await.unwrap();
    }
    let mut stream = c
        .parse_workbook(ReceiverStream::new(rx))
        .await
        .expect("call")
        .into_inner();
    // Wait for the range part to be under way.
    let mut rows = 0u32;
    while rows < 10 {
        let event = stream.message().await.expect("event").expect("open").event;
        if let Some(Event::Range(pb::SheetRangeEvent {
            event:
                Some(pb::StreamWorksheetRangeResponse {
                    event: Some(pb::stream_worksheet_range_response::Event::Row(_)),
                }),
            ..
        })) = event
        {
            rows += 1;
        }
    }
    tx.send(cancel_frame()).await.unwrap();
    let events = tokio::time::timeout(Duration::from_secs(10), drain(&mut stream))
        .await
        .expect("the stream ends promptly")
        .expect("a cancel is not a failure");
    let done = done(&events);
    assert!(done.cancelled);
    assert_eq!(done.range_sheets, 0, "the range part never finished");
    let more = events
        .iter()
        .filter(|e| matches!(e, Event::Range(_)))
        .count() as u32;
    assert!(
        rows + more < ROWS,
        "cancel stopped nothing: {} rows",
        rows + more
    );
    assert!(!events.iter().any(|e| matches!(e, Event::Formula(_))));
}

/// A cancel before the upload ends stops it there: no metadata, just `done`.
#[tokio::test]
async fn cancel_during_upload_ends_with_done() {
    let client = start_server().await;
    let bytes = fixture("date.xlsx");
    let frames = vec![
        open_frame(every_part()),
        chunk_frame(&bytes[..100], false),
        cancel_frame(),
    ];
    let events = parse_frames(&client, frames).await.expect("cancelled");
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(done(&events).cancelled);
}

/// The workbook lives for the stream only: with room for one open workbook,
/// back-to-back parses and an `OpenWorkbook` after them all fit, and a client
/// that walks away mid-stream gives its slot back.
#[tokio::test]
async fn nothing_outlives_the_stream() {
    let store = WorkbookStore::with_limits(StoreLimits {
        max_open_workbooks: 1,
        ..StoreLimits::default()
    });
    let client = start_server_with(CalamineGrpc::new(store)).await;
    let bytes = fixture("date.xlsx");
    for _ in 0..3 {
        parse(&client, every_part(), &bytes).await;
    }

    // Walk away after the first event.
    {
        let (tx, rx) = mpsc::channel(8);
        let mut c = client.clone();
        let open = pb::ParseWorkbookOpen {
            emit_ranges: true,
            range_options: Some(pb::ParseWorkbookRangeOptions {
                max_rows_per_message: 1,
                ..Default::default()
            }),
            ..Default::default()
        };
        for frame in upload_frames(open, &tall_workbook(50_000), 64 * 1024, true) {
            tx.send(frame).await.unwrap();
        }
        let mut stream = c
            .parse_workbook(ReceiverStream::new(rx))
            .await
            .expect("call")
            .into_inner();
        stream.message().await.expect("event").expect("metadata");
        // While it runs, it holds the one slot.
        let err = parse_frames(
            &client,
            upload_frames(every_part(), &bytes, 64 * 1024, false),
        )
        .await
        .expect_err("store full");
        assert_eq!(err.code(), Code::ResourceExhausted, "{err:?}");
    }

    let mut c = client.clone();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let frames = vec![
            pb::OpenWorkbookRequest {
                payload: Some(pb::open_workbook_request::Payload::Options(
                    pb::WorkbookOptions::default(),
                )),
            },
            pb::OpenWorkbookRequest {
                payload: Some(pb::open_workbook_request::Payload::Chunk(bytes.clone())),
            },
        ];
        match c.open_workbook(tokio_stream::iter(frames)).await {
            Ok(_) => break,
            Err(err) if err.code() == Code::ResourceExhausted => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the abandoned stream never gave its slot back"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(err) => panic!("open workbook: {err:?}"),
        }
    }
}

/// A caller's limits tighten the server's and can never raise them.
#[tokio::test]
async fn caller_limits_only_tighten() {
    let bytes = fixture("date.xls");
    let client = start_server().await;
    let with_limits = |limits| pb::ParseWorkbookOpen {
        limits: Some(limits),
        ..every_part()
    };

    // Upload size.
    let err = parse_frames(
        &client,
        upload_frames(
            with_limits(pb::ParseWorkbookLimits {
                max_workbook_bytes: 1024,
                ..Default::default()
            }),
            &bytes,
            512,
            false,
        ),
    )
    .await
    .expect_err("too large");
    assert_eq!(err.code(), Code::ResourceExhausted, "{err:?}");

    // Dense cells: an XLS range is refused before its first event, after the
    // metadata already went out.
    let mut client2 = client.clone();
    let mut stream = client2
        .parse_workbook(tokio_stream::iter(upload_frames(
            with_limits(pb::ParseWorkbookLimits {
                max_dense_cells: 1,
                ..Default::default()
            }),
            &bytes,
            64 * 1024,
            false,
        )))
        .await
        .expect("call")
        .into_inner();
    let first = stream.message().await.expect("metadata").expect("event");
    assert!(matches!(first.event, Some(Event::Metadata(_))));
    let err = drain(&mut stream).await.expect_err("too dense");
    assert_eq!(err.code(), Code::ResourceExhausted, "{err:?}");

    // A caller asking for more than the server allows gets the server's.
    let small =
        start_server_with(CalamineGrpc::new(WorkbookStore::new()).with_max_workbook_bytes(1024))
            .await;
    let err = parse_frames(
        &small,
        upload_frames(
            with_limits(pb::ParseWorkbookLimits {
                max_workbook_bytes: u64::MAX,
                ..Default::default()
            }),
            &bytes,
            512,
            false,
        ),
    )
    .await
    .expect_err("clamped");
    assert_eq!(err.code(), Code::ResourceExhausted, "{err:?}");
}

/// Past the stream cap a call is refused before it reads a frame.
#[tokio::test]
async fn streams_past_the_cap_are_refused() {
    let client =
        start_server_with(CalamineGrpc::new(WorkbookStore::new()).with_max_concurrent_streams(0))
            .await;
    let err = parse_frames(
        &client,
        upload_frames(every_part(), &fixture("date.xlsx"), 64 * 1024, false),
    )
    .await
    .expect_err("refused");
    assert_eq!(err.code(), Code::ResourceExhausted, "{err:?}");
}

/// A client that sends nothing mid-upload is cut off with DEADLINE_EXCEEDED.
#[tokio::test]
async fn a_stalled_upload_is_abandoned() {
    let client = start_server_with(
        CalamineGrpc::new(WorkbookStore::new()).with_upload_stall(Duration::from_millis(200)),
    )
    .await;
    let (tx, rx) = mpsc::channel(8);
    tx.send(open_frame(every_part())).await.unwrap();
    let mut c = client.clone();
    let mut stream = c
        .parse_workbook(ReceiverStream::new(rx))
        .await
        .expect("call")
        .into_inner();
    let err = drain(&mut stream).await.expect_err("stalled");
    assert_eq!(err.code(), Code::DeadlineExceeded, "{err:?}");
    drop(tx);
}
