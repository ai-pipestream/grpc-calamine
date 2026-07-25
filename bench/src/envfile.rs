// SPDX-License-Identifier: Apache-2.0

//! Machine-specific settings from `bench/.env`.
//!
//! Addresses and workbook paths are the operator's business, not the
//! repository's. `.env` is gitignored and `.env.example` documents the keys
//! with dummy values. The process environment always wins over the file, so
//! `BENCH_ADDR=... cargo run` behaves exactly as the README documents.

use std::collections::HashMap;
use std::sync::OnceLock;

/// A setting by name: the process environment first, then `bench/.env`.
pub fn var(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .or_else(|| file_values().get(key).cloned())
}

fn file_values() -> &'static HashMap<String, String> {
    static VALUES: OnceLock<HashMap<String, String>> = OnceLock::new();
    VALUES.get_or_init(|| {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".env");
        std::fs::read_to_string(path)
            .map(|text| parse(&text))
            .unwrap_or_default()
    })
}

/// Parse `KEY=VALUE` lines. Blank lines and `#` comments are skipped and
/// whitespace around key and value is trimmed; everything after the first
/// `=` belongs to the value.
fn parse(text: &str) -> HashMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn parses_key_values_and_skips_noise() {
        let map = parse(
            "# comment\n\
             \n\
             BENCH_ADDR = 192.0.2.10:50055 \n\
             BENCH_WORKBOOK=/data/big.xlsx\n\
             WINDOW_BYTES=0\n\
             #BENCH_DICT=1\n\
             =nokey\n\
             NOEQUALS\n",
        );
        assert_eq!(map.get("BENCH_ADDR").unwrap(), "192.0.2.10:50055");
        assert_eq!(map.get("BENCH_WORKBOOK").unwrap(), "/data/big.xlsx");
        assert_eq!(map.get("WINDOW_BYTES").unwrap(), "0");
        assert!(!map.contains_key("BENCH_DICT"), "commented lines are skipped");
        assert_eq!(map.len(), 3, "empty keys and non-assignments are dropped");
    }

    #[test]
    fn value_keeps_everything_after_the_first_equals() {
        let map = parse("KEY=a=b=c");
        assert_eq!(map.get("KEY").unwrap(), "a=b=c");
    }
}
