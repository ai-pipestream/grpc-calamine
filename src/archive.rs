// SPDX-License-Identifier: Apache-2.0

//! What calamine inflates while it opens a zip-packaged workbook, measured
//! before calamine is allowed to.
//!
//! calamine's xlsx, xlsb and ods readers read some parts of the package whole
//! while they are being constructed, before any sheet is asked for. With the
//! `picture` feature this server is built with, that is every embedded image,
//! each one a `read_to_end` (xlsx/mod.rs:711-733, xlsb/mod.rs:449-474,
//! ods.rs:811-840), and for xlsx and xlsb it is the shared-string table,
//! parsed into owned strings (xlsx/mod.rs:346, xlsb/mod.rs:269). Nothing
//! bounds either. Deflate reaches about 1,000:1, and the zip reader does not
//! hold a deflated entry to the size the archive records for it, so a 5 MB
//! upload can inflate to 5 GB inside `OpenWorkbook`. An allocation that size
//! fails by aborting the process, which no panic supervisor can catch, and
//! every reader of the workbook would repeat it.
//!
//! So those parts are inflated here first, into nothing, and counted, and the
//! workbook is refused as soon as a count passes its limit. The count comes
//! from inflating: every size written in the archive came from the uploader.
//! The work this costs is bounded by the limits, never by the file.
//!
//! The match is deliberately wider than calamine's own. A picture is any entry
//! with one of calamine's image extensions, wherever it sits, because the
//! media directory calamine reads depends on the package's relationships; a
//! shared-string table is matched by file name, compared the way calamine
//! looks it up (ignoring case and slash direction). Counting an entry calamine
//! would skip costs a little work; missing one it reads would cost the process.

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, ErrorKind, Read, Seek};

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::store::LimitExceeded;

/// Extensions calamine's `picture` feature reads into memory
/// (xlsx/mod.rs:719-722), restated because calamine does not export them.
const PICTURE_EXTENSIONS: [&str; 12] = [
    "emf", "wmf", "pict", "jpeg", "jpg", "png", "dib", "gif", "tiff", "eps", "bmp", "wpg",
];

/// Lowercased file names of the shared-string tables calamine reads whole:
/// `xl/sharedStrings.xml` for xlsx and `xl/sharedStrings.bin` for xlsb.
const SHARED_STRINGS_FILES: [&str; 2] = ["sharedstrings.xml", "sharedstrings.bin"];

/// Longest entry name quoted back in a refusal, so a crafted 64 KiB name
/// cannot bloat the status message.
const MAX_QUOTED_NAME: usize = 200;

/// Default for [`InflateLimits::max_picture_bytes`]: 64 MiB.
const DEFAULT_MAX_PICTURE_BYTES: u64 = 64 * 1024 * 1024;

/// Default for [`InflateLimits::max_picture_total_bytes`]: 256 MiB.
const DEFAULT_MAX_PICTURE_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

/// Default for [`InflateLimits::max_shared_strings_bytes`]: 1 GiB.
const DEFAULT_MAX_SHARED_STRINGS_BYTES: u64 = 1024 * 1024 * 1024;

/// Smallest bytes a shared-string `<si>` entry can occupy in the table. Used
/// only to turn the inflated table size into a ceiling on how many strings it
/// could hold, so a declared count far above that is caught as a lie. A real
/// entry is larger, so this never rejects an honest table.
const MIN_SI_BYTES: u64 = 8;

/// How much of a shared-string table is scanned for its declared count. The
/// `<sst>` element is the table's root, so its attributes sit at the very
/// start; a megabyte is far more than any real one needs.
const SST_HEAD_SCAN: usize = 1024 * 1024;

/// Largest drawing part read to count its anchors. Real drawings are tiny; a
/// part past this is refused rather than scanned.
const MAX_DRAWING_BYTES: usize = 16 * 1024 * 1024;

/// Largest relationships part read to resolve its image targets.
const MAX_RELS_BYTES: usize = 4 * 1024 * 1024;

/// Bounds on what opening one workbook may inflate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InflateLimits {
    /// Largest single embedded picture, in inflated bytes.
    pub max_picture_bytes: u64,
    /// Most inflated bytes one workbook's embedded pictures may add up to.
    pub max_picture_total_bytes: u64,
    /// Largest shared-string table, in inflated bytes.
    pub max_shared_strings_bytes: u64,
}

impl Default for InflateLimits {
    fn default() -> Self {
        Self {
            max_picture_bytes: DEFAULT_MAX_PICTURE_BYTES,
            max_picture_total_bytes: DEFAULT_MAX_PICTURE_TOTAL_BYTES,
            max_shared_strings_bytes: DEFAULT_MAX_SHARED_STRINGS_BYTES,
        }
    }
}

/// What [`inspect`] measured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Inflated {
    /// Inflated bytes of all embedded pictures together, counted once per
    /// drawing anchor that embeds each one, as calamine clones them. Every
    /// reader of the workbook holds this for as long as it lives.
    pub picture_bytes: u64,
}

/// Why [`inspect`] refused a workbook.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rejected {
    /// A configured inflation limit was passed. A resource problem: the same
    /// bytes open on a server with more headroom.
    Limit(LimitExceeded),
    /// The package is internally inconsistent in a way that would otherwise
    /// drive calamine to allocate from a number it never checks: a declared
    /// count the inflated part cannot hold, or a structural part too large to
    /// validate. A fault in the file, reported as `INVALID_ARGUMENT`.
    Malformed {
        /// The entry at fault.
        part: String,
        /// What is wrong with it.
        detail: String,
    },
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Limit(e) => e.fmt(f),
            Self::Malformed { part, detail } => write!(f, "{part}: {detail}"),
        }
    }
}

impl std::error::Error for Rejected {}

impl From<LimitExceeded> for Rejected {
    fn from(e: LimitExceeded) -> Self {
        Self::Limit(e)
    }
}

/// A part of the package calamine reads whole when it opens it.
enum Part {
    Picture,
    SharedStrings,
}

/// An entry name with backslashes folded to slashes and lowercased, matching
/// how calamine looks parts up (`build_zip_path_cache`, utils.rs:172).
fn normalize(name: &str) -> String {
    name.replace('\\', "/").to_ascii_lowercase()
}

/// Which part, if any, an entry name is.
fn classify(name: &str) -> Option<Part> {
    let name = normalize(name);
    let file = name.rsplit('/').next().unwrap_or_default();
    if SHARED_STRINGS_FILES.contains(&file) {
        return Some(Part::SharedStrings);
    }
    let (_, extension) = file.rsplit_once('.')?;
    PICTURE_EXTENSIONS
        .contains(&extension)
        .then_some(Part::Picture)
}

/// An entry name fit to quote in a refusal.
fn quoted(name: &str) -> String {
    match name.char_indices().nth(MAX_QUOTED_NAME) {
        Some((cut, _)) => format!("{}...", &name[..cut]),
        None => name.to_owned(),
    }
}

/// Inflate, into nothing, every part calamine would read whole when opening
/// `bytes`, refuse the workbook once one passes its limit, and charge each
/// embedded picture once per drawing anchor that embeds it, as calamine
/// clones them.
///
/// Bytes that are not a zip archive (an xls file is a compound file instead)
/// pass untouched, as do entries the zip reader cannot open or inflate:
/// calamine opens the same entries, meets the same failure and reports it
/// in its own words.
///
/// # Errors
///
/// [`Rejected::Limit`] for a picture, picture total, or shared-string table
/// past its byte limit; [`Rejected::Malformed`] for a shared-string table
/// whose declared count the bytes cannot hold, or a structural part too
/// large to validate.
pub fn inspect(bytes: &[u8], limits: &InflateLimits) -> Result<Inflated, Rejected> {
    let Ok(mut zip) = zip::ZipArchive::new(Cursor::new(bytes)) else {
        return Ok(Inflated::default());
    };

    // Pass 1: measure each distinct media entry once, validate the
    // shared-string table, and index the archive so pass 2 can read the
    // drawing parts by name.
    let mut media: HashMap<String, u64> = HashMap::new();
    let mut by_lc: HashMap<String, usize> = HashMap::new();
    for index in 0..zip.len() {
        let Ok(mut entry) = zip.by_index(index) else {
            continue;
        };
        let lc = normalize(entry.name());
        match classify(entry.name()) {
            Some(Part::Picture) => {
                let name = quoted(entry.name());
                let size = inflated_size(&mut entry, limits.max_picture_bytes).ok_or(
                    Rejected::Limit(LimitExceeded::Picture {
                        name,
                        max: limits.max_picture_bytes,
                    }),
                )?;
                media.insert(lc.clone(), size);
            }
            Some(Part::SharedStrings) => {
                let name = quoted(entry.name());
                scan_shared_strings(&mut entry, name, limits)?;
            }
            None => {}
        }
        by_lc.insert(lc, index);
    }

    let picture_bytes = charge_pictures(&mut zip, &by_lc, &media, limits.max_picture_total_bytes)?;
    Ok(Inflated { picture_bytes })
}

/// Read `part` to its end into nothing and return how many bytes it gave, or
/// `None` as soon as that passes `limit`.
///
/// A read error ends the count early instead of failing it: the entry is
/// corrupt, and calamine meets the same error when it reads it.
fn inflated_size(part: &mut impl Read, limit: u64) -> Option<u64> {
    let mut sink = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        match part.read(&mut sink) {
            Ok(0) => return Some(total),
            Ok(n) => {
                total = total.saturating_add(n as u64);
                if total > limit {
                    return None;
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => return Some(total),
        }
    }
}

/// Validate a shared-string table: enforce its byte limit, and refuse one
/// whose declared `uniqueCount` is more than the inflated bytes could hold.
///
/// calamine reads `uniqueCount` and calls `self.strings.reserve(n)` with it
/// before reading a single entry (xlsx/mod.rs:360), so a 2 KB table that
/// declares a trillion strings reserves 24 TB and aborts the process. The
/// count cannot exceed one entry per [`MIN_SI_BYTES`] of the table, so a
/// declaration above that is rejected before calamine ever reserves.
///
/// `count` is not checked: it counts cell references, which legitimately far
/// exceed the number of distinct strings, and calamine does not reserve on it.
fn scan_shared_strings(
    part: &mut impl Read,
    name: String,
    limits: &InflateLimits,
) -> Result<(), Rejected> {
    let mut head: Vec<u8> = Vec::new();
    let mut total = 0u64;
    let mut sink = vec![0u8; 64 * 1024];
    loop {
        match part.read(&mut sink) {
            Ok(0) => break,
            Ok(n) => {
                total = total.saturating_add(n as u64);
                if total > limits.max_shared_strings_bytes {
                    return Err(Rejected::Limit(LimitExceeded::SharedStrings {
                        name,
                        max: limits.max_shared_strings_bytes,
                    }));
                }
                if head.len() < SST_HEAD_SCAN {
                    let room = SST_HEAD_SCAN - head.len();
                    head.extend_from_slice(&sink[..n.min(room)]);
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }

    if let Some(declared) = declared_unique_count(&head) {
        let capacity = total / MIN_SI_BYTES;
        if declared > capacity {
            return Err(Rejected::Malformed {
                part: name,
                detail: format!(
                    "declares uniqueCount={declared} shared strings, but the table \
                     inflates to {total} bytes, which can hold at most {capacity}; \
                     calamine reserves space for all {declared} before reading any"
                ),
            });
        }
    }
    Ok(())
}

/// The `uniqueCount` attribute of the `<sst>` root element, if present in the
/// scanned head of a shared-string table.
fn declared_unique_count(head: &[u8]) -> Option<u64> {
    let open = find(head, b"<sst")?;
    // The real root, not `<sstSomething>`.
    let after = *head.get(open + 4)?;
    if !(after == b'>' || after == b'/' || after.is_ascii_whitespace()) {
        return None;
    }
    let end = open + 4 + find(&head[open + 4..], b">")?;
    attr_u64(&head[open..=end], b"uniqueCount")
}

/// Parse the value of a numeric double- or single-quoted attribute from a raw
/// start tag, saturating. `None` when the attribute is absent or has no digits.
fn attr_u64(tag: &[u8], name: &[u8]) -> Option<u64> {
    let mut from = 0;
    while let Some(rel) = find(&tag[from..], name) {
        let at = from + rel;
        from = at + name.len();
        // A real attribute, not a tail of a longer name.
        if at > 0 && tag[at - 1].is_ascii_alphanumeric() {
            continue;
        }
        let mut j = from;
        while tag.get(j).is_some_and(u8::is_ascii_whitespace) {
            j += 1;
        }
        if tag.get(j) != Some(&b'=') {
            continue;
        }
        j += 1;
        while tag.get(j).is_some_and(u8::is_ascii_whitespace) {
            j += 1;
        }
        let quote = match tag.get(j) {
            Some(&q @ (b'"' | b'\'')) => q,
            _ => continue,
        };
        j += 1;
        let mut value = 0u64;
        let mut any = false;
        while let Some(&b) = tag.get(j) {
            if b == quote {
                break;
            }
            if b.is_ascii_digit() {
                value = value.saturating_mul(10).saturating_add(u64::from(b - b'0'));
                any = true;
            }
            j += 1;
        }
        if any {
            return Some(value);
        }
    }
    None
}

/// Charge every media entry against the per-workbook picture budget: once for
/// each drawing anchor that embeds it (as calamine clones it per anchor), and
/// once for a media no anchor references (calamine's fallback for unanchored
/// media). Refuses as soon as the running total passes `max_total`.
///
/// Drawing anchors in the DrawingML `blip` elements are the vector a crafted
/// file uses to multiply a small image into gigabytes. The rich-data (`vm`)
/// picture path clones per reference too and is not counted per reference
/// here; its media are still charged once, like calamine's unanchored media.
fn charge_pictures<R: Read + Seek>(
    zip: &mut zip::ZipArchive<R>,
    by_lc: &HashMap<String, usize>,
    media: &HashMap<String, u64>,
    max_total: u64,
) -> Result<u64, Rejected> {
    if media.is_empty() {
        return Ok(0);
    }
    let mut total = 0u64;
    let mut seen: HashSet<&str> = HashSet::new();
    let mut drawings: Vec<&String> = by_lc.keys().filter(|p| is_drawing(p)).collect();
    drawings.sort(); // deterministic refusal on crafted archives
    for drawing in drawings {
        let rid_to_media = match rels_sibling(drawing).and_then(|r| Some((by_lc.get(&r)?, r))) {
            Some((&idx, name)) => rels_image_map(zip, idx, parent(drawing), &name)?,
            None => HashMap::new(),
        };
        let &idx = by_lc.get(drawing).expect("drawing key came from by_lc");
        let Some(xml) = read_capped(zip, idx, MAX_DRAWING_BYTES) else {
            return Err(Rejected::Malformed {
                part: quoted(drawing),
                detail: format!(
                    "drawing part exceeds {MAX_DRAWING_BYTES} bytes, too large to validate"
                ),
            });
        };
        for rid in blip_embeds(&xml) {
            // `key` borrows `media`, which outlives the per-drawing maps, so a
            // charged media stays recorded in `seen` across drawings.
            if let Some(path) = rid_to_media.get(&rid)
                && let Some((key, &size)) = media.get_key_value(path)
            {
                total = total.saturating_add(size);
                seen.insert(key.as_str());
                if total > max_total {
                    return Err(Rejected::Limit(LimitExceeded::PictureTotal {
                        max: max_total,
                    }));
                }
            }
        }
    }
    for (path, &size) in media {
        if !seen.contains(path.as_str()) {
            total = total.saturating_add(size);
            if total > max_total {
                return Err(Rejected::Limit(LimitExceeded::PictureTotal {
                    max: max_total,
                }));
            }
        }
    }
    Ok(total)
}

/// Whether a normalized entry path is a DrawingML drawing part (not its rels).
fn is_drawing(path: &str) -> bool {
    path.starts_with("xl/drawings/") && path.ends_with(".xml") && !path.contains("/_rels/")
}

/// The directory of a normalized path.
fn parent(path: &str) -> &str {
    path.rfind('/').map_or("", |i| &path[..i])
}

/// The `_rels` sibling of a part: `a/b.xml` -> `a/_rels/b.xml.rels`.
fn rels_sibling(path: &str) -> Option<String> {
    let i = path.rfind('/')?;
    Some(format!("{}/_rels/{}.rels", &path[..i], &path[i + 1..]))
}

/// Read a zip entry fully into memory, capped at `cap` bytes. `None` when the
/// entry inflates past the cap (the caller refuses it); an empty vec when the
/// entry cannot be opened or read, so a corrupt part resolves to no mappings
/// rather than a panic.
fn read_capped<R: Read + Seek>(
    zip: &mut zip::ZipArchive<R>,
    index: usize,
    cap: usize,
) -> Option<Vec<u8>> {
    let Ok(entry) = zip.by_index(index) else {
        return Some(Vec::new());
    };
    let mut buf = Vec::new();
    let _ = entry.take(cap as u64 + 1).read_to_end(&mut buf);
    (buf.len() <= cap).then_some(buf)
}

/// Map relationship id -> resolved media path for every image relationship in
/// a `.rels` part, resolving each `Target` against `base`.
fn rels_image_map<R: Read + Seek>(
    zip: &mut zip::ZipArchive<R>,
    index: usize,
    base: &str,
    name: &str,
) -> Result<HashMap<String, String>, Rejected> {
    let Some(bytes) = read_capped(zip, index, MAX_RELS_BYTES) else {
        return Err(Rejected::Malformed {
            part: quoted(name),
            detail: format!("relationships part exceeds {MAX_RELS_BYTES} bytes"),
        });
    };
    let mut map = HashMap::new();
    let mut reader = Reader::from_reader(bytes.as_slice());
    reader.config_mut().check_end_names = false;
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_into(&mut buf);
        match event {
            Ok(Event::Eof) | Err(_) => break,
            Ok(Event::Start(e) | Event::Empty(e)) if e.local_name().as_ref() == b"Relationship" => {
                let mut id = None;
                let mut target = None;
                let mut is_image = false;
                for attr in e.attributes().flatten() {
                    match attr.key.local_name().as_ref() {
                        b"Id" => id = Some(String::from_utf8_lossy(&attr.value).into_owned()),
                        b"Target" => {
                            target = Some(String::from_utf8_lossy(&attr.value).into_owned());
                        }
                        b"Type" => is_image = attr.value.ends_with(b"/image"),
                        _ => {}
                    }
                }
                if let (true, Some(id), Some(target)) = (is_image, id, target) {
                    map.insert(id, resolve_path(base, &target));
                }
            }
            Ok(_) => {}
        }
        buf.clear();
    }
    Ok(map)
}

/// The `r:embed` relationship id of every DrawingML `blip` element, in order,
/// one per picture anchor.
fn blip_embeds(xml: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().check_end_names = false;
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_into(&mut buf);
        match event {
            Ok(Event::Eof) | Err(_) => break,
            Ok(Event::Start(e) | Event::Empty(e)) if e.local_name().as_ref() == b"blip" => {
                for attr in e.attributes().flatten() {
                    if attr.key.local_name().as_ref() == b"embed" {
                        out.push(String::from_utf8_lossy(&attr.value).into_owned());
                    }
                }
            }
            Ok(_) => {}
        }
        buf.clear();
    }
    out
}

/// Resolve a relationship `Target` against the part's directory, normalized
/// the way media keys are, mirroring calamine's `resolve_path` (xlsx/mod.rs)
/// but tolerating any number of leading `../`.
fn resolve_path(base: &str, target: &str) -> String {
    let target = target.replace('\\', "/");
    if let Some(stripped) = target.strip_prefix('/') {
        return stripped.to_ascii_lowercase();
    }
    let mut segments: Vec<&str> = if base.is_empty() {
        Vec::new()
    } else {
        base.split('/').collect()
    };
    for segment in target.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    segments.join("/").to_ascii_lowercase()
}

/// First index of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_are_matched_by_extension_anywhere() {
        assert!(matches!(
            classify("xl/media/image1.png"),
            Some(Part::Picture)
        ));
        assert!(matches!(classify("custom/IMAGE.JPEG"), Some(Part::Picture)));
        assert!(matches!(classify("Pictures/1000.emf"), Some(Part::Picture)));
        assert!(classify("xl/worksheets/sheet1.xml").is_none());
        assert!(classify("xl/media/readme").is_none());
        assert!(classify("xl/media.png/data").is_none());
    }

    #[test]
    fn shared_strings_are_matched_the_way_calamine_looks_them_up() {
        assert!(matches!(
            classify("xl/sharedStrings.xml"),
            Some(Part::SharedStrings)
        ));
        assert!(matches!(
            classify("XL\\SHAREDSTRINGS.XML"),
            Some(Part::SharedStrings)
        ));
        assert!(matches!(
            classify("elsewhere/sharedStrings.bin"),
            Some(Part::SharedStrings)
        ));
        assert!(classify("xl/sharedStrings.xml.rels").is_none());
    }

    #[test]
    fn counting_stops_just_past_the_limit() {
        let mut endless = std::io::repeat(0);
        assert_eq!(inflated_size(&mut endless, 1 << 20), None);
        let mut exact = std::io::repeat(0).take(1 << 20);
        assert_eq!(inflated_size(&mut exact, 1 << 20), Some(1 << 20));
    }

    #[test]
    fn long_names_are_cut_before_they_are_quoted() {
        let long = "x".repeat(10_000);
        assert_eq!(quoted(&long).len(), MAX_QUOTED_NAME + 3);
        assert_eq!(quoted("xl/media/image1.png"), "xl/media/image1.png");
    }

    #[test]
    fn bytes_that_are_not_a_zip_pass_untouched() {
        let limits = InflateLimits {
            max_picture_bytes: 0,
            max_picture_total_bytes: 0,
            max_shared_strings_bytes: 0,
        };
        assert_eq!(
            inspect(b"\xD0\xCF\x11\xE0 an xls header", &limits),
            Ok(Inflated::default())
        );
    }

    #[test]
    fn declared_unique_count_reads_the_sst_root_attribute() {
        let head = br#"<?xml version="1.0"?><sst xmlns="x" count="9" uniqueCount="123">"#;
        assert_eq!(declared_unique_count(head), Some(123));
        // Single quotes, and `count` is not mistaken for `uniqueCount`.
        assert_eq!(
            declared_unique_count(br#"<sst count='5' uniqueCount='7'>"#),
            Some(7)
        );
        // A huge value saturates rather than wrapping.
        assert_eq!(
            declared_unique_count(br#"<sst uniqueCount="1000000000000">"#),
            Some(1_000_000_000_000)
        );
        // No uniqueCount, and not the sst element.
        assert_eq!(declared_unique_count(br#"<sst count="5">"#), None);
        assert_eq!(declared_unique_count(br#"<sstx uniqueCount="5">"#), None);
    }

    #[test]
    fn attr_u64_requires_a_whole_attribute_name() {
        // `Count` must not satisfy a search for `count` via `uniqueCount`.
        assert_eq!(attr_u64(br#"<sst uniqueCount="7">"#, b"count"), None);
        assert_eq!(
            attr_u64(br#"<sst uniqueCount="7">"#, b"uniqueCount"),
            Some(7)
        );
    }

    #[test]
    fn resolve_path_walks_parent_references() {
        assert_eq!(
            resolve_path("xl/drawings", "../media/Image1.PNG"),
            "xl/media/image1.png"
        );
        assert_eq!(
            resolve_path("xl/drawings", "/xl/media/image1.png"),
            "xl/media/image1.png"
        );
        assert_eq!(
            resolve_path("xl/richData", "../media/i.png"),
            "xl/media/i.png"
        );
    }

    #[test]
    fn is_drawing_matches_drawing_parts_only() {
        assert!(is_drawing("xl/drawings/drawing1.xml"));
        assert!(!is_drawing("xl/drawings/_rels/drawing1.xml.rels"));
        assert!(!is_drawing("xl/worksheets/sheet1.xml"));
        assert!(!is_drawing("xl/drawings/vmldrawing1.vml"));
    }

    #[test]
    fn blip_embeds_lists_every_anchor_embed_in_order() {
        let xml = br#"<xdr:wsDr xmlns:xdr="d" xmlns:a="a" xmlns:r="r">
            <xdr:twoCellAnchor><xdr:pic><xdr:blipFill>
              <a:blip r:embed="rId1"/>
            </xdr:blipFill></xdr:pic></xdr:twoCellAnchor>
            <xdr:oneCellAnchor><xdr:pic><xdr:blipFill>
              <a:blip r:embed="rId1"/>
            </xdr:blipFill></xdr:pic></xdr:oneCellAnchor>
            <xdr:twoCellAnchor><xdr:pic><xdr:blipFill>
              <a:blip r:embed="rId2"/>
            </xdr:blipFill></xdr:pic></xdr:twoCellAnchor>
        </xdr:wsDr>"#;
        assert_eq!(blip_embeds(xml), vec!["rId1", "rId1", "rId2"]);
    }

    #[test]
    fn rels_sibling_points_at_the_rels_of_a_part() {
        assert_eq!(
            rels_sibling("xl/drawings/drawing1.xml").as_deref(),
            Some("xl/drawings/_rels/drawing1.xml.rels")
        );
    }
}
