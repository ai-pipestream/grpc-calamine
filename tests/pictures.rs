// SPDX-License-Identifier: Apache-2.0

//! One image anchored many times costs one copy.
//!
//! Its own test binary, because it measures the process's peak memory and
//! nothing else may run beside it while it does.

use std::io::{Cursor, Write};

use calamine::Reader;
use grpc_calamine::WorkbookStore;
use grpc_calamine::archive::InflateLimits;
use grpc_calamine::proto::v1 as pb;
use grpc_calamine::store::StoreLimits;

const IMAGE_BYTES: usize = 64 * 1024;
const ANCHORS: usize = 20_000;

/// Peak resident memory of this process so far, in KiB.
fn peak_rss_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kib| kib.parse().ok())
        .expect("VmHWM in /proc/self/status")
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

/// One 64 KiB image anchored 20,000 times by a drawing outside
/// `xl/drawings/`, which is where the sheet's relationships say it is and so
/// where calamine follows them: 1.25 GB if every anchor held its own copy.
fn workbook() -> Vec<u8> {
    let mut drawing = String::from(
        r#"<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
    );
    for _ in 0..ANCHORS {
        drawing.push_str(
            r#"<xdr:oneCellAnchor><xdr:pic><xdr:blipFill><a:blip r:embed="rId1"/></xdr:blipFill></xdr:pic></xdr:oneCellAnchor>"#,
        );
    }
    drawing.push_str("</xdr:wsDr>");
    let parts: [(&str, Vec<u8>); 9] = [
        (
            "[Content_Types].xml",
            br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Default Extension="png" ContentType="image/png"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
</Types>"#
                .to_vec(),
        ),
        ("_rels/.rels", rels("officeDocument", "xl/workbook.xml")),
        (
            "xl/workbook.xml",
            br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#
                .to_vec(),
        ),
        (
            "xl/_rels/workbook.xml.rels",
            rels("worksheet", "worksheets/sheet1.xml"),
        ),
        (
            "xl/worksheets/sheet1.xml",
            br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData>
</worksheet>"#
                .to_vec(),
        ),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            rels("drawing", "../art/drawing1.xml"),
        ),
        ("xl/art/drawing1.xml", drawing.into_bytes()),
        ("xl/art/_rels/drawing1.xml.rels", rels("image", "../media/image1.png")),
        ("xl/media/image1.png", vec![0u8; IMAGE_BYTES]),
    ];
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let deflated = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, body) in parts {
        zip.start_file(name, deflated).expect("start part");
        zip.write_all(&body).expect("write part");
    }
    zip.finish().expect("finish package").into_inner()
}

/// The workbook opens against a picture budget of 1 MiB, is charged its one
/// distinct image, and holds about that: peak memory grows by megabytes, not
/// by the 1.25 GB that a copy per anchor would be. Every anchor is still
/// there, each handed out as its own copy, one at a time.
#[test]
fn one_image_anchored_many_times_is_charged_and_held_once() {
    let package = workbook();
    assert!(package.len() < 64 * 1024, "{} bytes", package.len());
    let store = WorkbookStore::with_limits(StoreLimits {
        inflate: InflateLimits {
            max_picture_total_bytes: 1024 * 1024,
            ..InflateLimits::default()
        },
        ..StoreLimits::default()
    });

    let before = peak_rss_kib();
    let (_id, entry) = store
        .open(package.clone(), pb::WorkbookFormat::Unspecified, None)
        .expect("one distinct image fits a 1 MiB picture budget");
    assert_eq!(
        store.held_bytes(),
        package.len() as u64 + IMAGE_BYTES as u64,
        "charged the upload and its one image"
    );

    let reader = entry.reader().expect("reader");
    let mut anchors = 0;
    for picture in reader.pictures_iter() {
        assert_eq!(picture.data.len(), IMAGE_BYTES);
        assert_eq!(picture.sheet_name, "Sheet1");
        anchors += 1;
    }
    assert_eq!(anchors, ANCHORS, "every anchor is still reported");

    let grew_mib = (peak_rss_kib() - before) / 1024;
    assert!(
        grew_mib < 64,
        "peak memory grew {grew_mib} MiB; one copy per anchor would be 1250"
    );
}
