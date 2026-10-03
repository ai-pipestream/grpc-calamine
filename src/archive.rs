// SPDX-License-Identifier: Apache-2.0

//! What calamine inflates while it opens a zip-packaged workbook, measured
//! before calamine is allowed to.
//!
//! calamine's xlsx, xlsb and ods readers read some parts of the package whole
//! while they are being constructed, before any sheet is asked for. With the
//! `picture` feature this server is built with, that is every embedded image,
//! each one a `read_to_end` (xlsx/mod.rs:711-733, xlsb/mod.rs:449-471,
//! ods.rs:811-835), and for xlsx and xlsb it is the shared-string table,
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
    /// Inflated bytes of all embedded pictures together, which every reader
    /// of the workbook holds for as long as it lives.
    pub picture_bytes: u64,
}

/// A part of the package calamine reads whole when it opens it.
enum Part {
    Picture,
    SharedStrings,
}

/// Which part, if any, an entry name is.
fn classify(name: &str) -> Option<Part> {
    let name = name.replace('\\', "/").to_ascii_lowercase();
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
/// `bytes`, and refuse the workbook once one passes its limit.
///
/// Bytes that are not a zip archive (an xls file is a compound file instead)
/// pass untouched, as do entries the zip reader cannot open or inflate:
/// calamine opens the same entries, meets the same failure and reports it
/// in its own words.
///
/// # Errors
///
/// [`LimitExceeded::Picture`], [`LimitExceeded::PictureTotal`] or
/// [`LimitExceeded::SharedStrings`], for the first limit passed.
pub fn inspect(bytes: &[u8], limits: &InflateLimits) -> Result<Inflated, LimitExceeded> {
    let Ok(mut zip) = zip::ZipArchive::new(Cursor::new(bytes)) else {
        return Ok(Inflated::default());
    };
    let mut pictures = 0u64;
    for index in 0..zip.len() {
        let Ok(mut entry) = zip.by_index(index) else {
            continue;
        };
        match classify(entry.name()) {
            Some(Part::Picture) => {
                let room = limits.max_picture_total_bytes.saturating_sub(pictures);
                let limit = limits.max_picture_bytes.min(room);
                match inflated_size(&mut entry, limit) {
                    Some(size) => pictures += size,
                    None if limit == limits.max_picture_bytes => {
                        return Err(LimitExceeded::Picture {
                            name: quoted(entry.name()),
                            max: limits.max_picture_bytes,
                        });
                    }
                    None => {
                        return Err(LimitExceeded::PictureTotal {
                            max: limits.max_picture_total_bytes,
                        });
                    }
                }
            }
            Some(Part::SharedStrings)
                if inflated_size(&mut entry, limits.max_shared_strings_bytes).is_none() =>
            {
                return Err(LimitExceeded::SharedStrings {
                    name: quoted(entry.name()),
                    max: limits.max_shared_strings_bytes,
                });
            }
            // A table within its limit, or a part calamine does not read whole.
            Some(Part::SharedStrings) | None => {}
        }
    }
    Ok(Inflated {
        picture_bytes: pictures,
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
}
