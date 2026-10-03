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
//!
//! What is counted is each distinct image, once. calamine attaches an image to
//! every drawing anchor and rich-data cell that embeds it, and those are free
//! to multiply in the XML, but the pinned fork shares one copy of the bytes
//! between them (ai-pipestream/calamine 23ae1f1), so the reader holds the
//! distinct images and nothing per reference. The server hands pictures out
//! through `pictures_iter`, one copy at a time, for the same reason.

use std::io::{Cursor, ErrorKind, Read};

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
    /// Inflated bytes of the embedded pictures together, each distinct image
    /// once, however many anchors embed it. Every reader of the workbook
    /// holds this for as long as it lives.
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
    /// count the inflated part cannot hold. A fault in the file, reported as
    /// `INVALID_ARGUMENT`.
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
/// `bytes`, refuse the workbook once one passes its limit, and return what
/// the embedded pictures add up to.
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
/// whose declared count the bytes cannot hold.
pub fn inspect(bytes: &[u8], limits: &InflateLimits) -> Result<Inflated, Rejected> {
    let Ok(mut zip) = zip::ZipArchive::new(Cursor::new(bytes)) else {
        return Ok(Inflated::default());
    };

    // Each image is inflated only as far as the picture total still has room
    // for, so the work here is bounded by that total however many images the
    // archive holds.
    let mut media_total = 0u64;
    for index in 0..zip.len() {
        let Ok(mut entry) = zip.by_index(index) else {
            continue;
        };
        match classify(entry.name()) {
            Some(Part::Picture) => {
                let name = quoted(entry.name());
                let room = limits.max_picture_total_bytes.saturating_sub(media_total);
                let limit = limits.max_picture_bytes.min(room);
                let size = match inflated_size(&mut entry, limit) {
                    Some(size) => size,
                    None if limit == limits.max_picture_bytes => {
                        return Err(Rejected::Limit(LimitExceeded::Picture {
                            name,
                            max: limits.max_picture_bytes,
                        }));
                    }
                    None => {
                        return Err(Rejected::Limit(LimitExceeded::PictureTotal {
                            max: limits.max_picture_total_bytes,
                        }));
                    }
                };
                media_total = media_total.saturating_add(size);
            }
            Some(Part::SharedStrings) => {
                let name = quoted(entry.name());
                scan_shared_strings(&mut entry, name, limits)?;
            }
            None => {}
        }
    }
    Ok(Inflated {
        picture_bytes: media_total,
    })
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
}
