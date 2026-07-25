// SPDX-License-Identifier: Apache-2.0

//! The NDJSON arm, in Rust: fetch the same rows as newline-delimited JSON over
//! plain HTTP and parse them, feeding the same digest the gRPC arms use.
//!
//! With a Python client, protobuf and JSON land within a few percent of each
//! other because the interpreter is the bottleneck. This exists to answer the
//! separate question: once the client is fast enough for the format to matter,
//! how far apart are they?

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Instant;

fn crc32(mut h: u32, bytes: &[u8]) -> u32 {
    // Same polynomial and chaining as zlib.crc32, so the Python and Rust arms
    // produce identical digests.
    for b in bytes {
        h ^= u32::from(*b);
        for _ in 0..8 {
            h = if h & 1 != 0 {
                (h >> 1) ^ 0xEDB8_8320
            } else {
                h >> 1
            };
        }
    }
    h
}

struct Digest {
    h: u32,
    rows: u64,
    cells: u64,
}

impl Digest {
    /// Seeded to match the Python harness, which starts from the FNV offset
    /// basis truncated to 32 bits, so the two produce comparable digests.
    fn new() -> Self {
        Self {
            h: 0x8422_2325,
            rows: 0,
            cells: 0,
        }
    }
    fn w(&mut self, b: &[u8]) {
        self.h = !crc32(!self.h, b);
    }
    fn cell_text(&mut self, s: &str) {
        self.cells += 1;
        let mut v = Vec::with_capacity(s.len() + 1);
        v.push(3u8);
        v.extend_from_slice(s.as_bytes());
        self.w(&v);
    }
    fn cell_num(&mut self, f: f64) {
        self.cells += 1;
        let mut v = Vec::with_capacity(9);
        if f.fract() == 0.0 {
            v.push(1u8);
            v.extend_from_slice(&(f as i64).to_le_bytes());
        } else {
            v.push(2u8);
            v.extend_from_slice(&f.to_bits().to_le_bytes());
        }
        self.w(&v);
    }
    fn cell_empty(&mut self) {
        self.cells += 1;
        self.w(&[0u8]);
    }
}

fn main() {
    let host = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1".into());
    let port: u16 = std::env::args()
        .nth(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(8099);

    let t = Instant::now();
    let mut sock = TcpStream::connect((host.as_str(), port)).expect("connect");
    sock.set_nodelay(true).ok();
    write!(
        sock,
        "GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .expect("send");

    // Skip the response head.
    let mut reader = BufReader::with_capacity(1 << 20, sock);
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line).expect("head");
        if line == "\r\n" || line.is_empty() {
            break;
        }
    }

    let mut digest = Digest::new();
    let mut bytes = 0u64;
    let mut buf = Vec::with_capacity(1 << 16);
    loop {
        buf.clear();
        let n = read_until_nl(&mut reader, &mut buf);
        if n == 0 {
            break;
        }
        bytes += n as u64;
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf) else {
            continue;
        };
        let Some(cells) = v.get("cells").and_then(|c| c.as_array()) else {
            continue;
        };
        digest.rows += 1;
        for c in cells {
            let text = c.get("text").and_then(|t| t.as_str()).unwrap_or("");
            let ty = c.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if ty == "number" && !text.is_empty() {
                digest.cell_num(text.parse::<f64>().unwrap_or(0.0));
            } else if ty == "empty" || text.is_empty() {
                digest.cell_empty();
            } else {
                digest.cell_text(text);
            }
        }
    }
    let ms = t.elapsed().as_secs_f64() * 1e3;
    println!(
        "rust NDJSON/HTTP  {ms:9.0} ms  {:9.0} rows/s  {:016x}/{}r/{}c  {:.0} MB",
        digest.rows as f64 / (ms / 1e3),
        digest.h,
        digest.rows,
        digest.cells,
        bytes as f64 / 1e6
    );
}

/// Read one newline-terminated record, returning bytes consumed.
fn read_until_nl(r: &mut impl BufRead, out: &mut Vec<u8>) -> usize {
    let mut total = 0;
    loop {
        let available = match r.fill_buf() {
            Ok([]) => return total,
            Ok(b) => b,
            Err(_) => return total,
        };
        if let Some(i) = available.iter().position(|b| *b == b'\n') {
            out.extend_from_slice(&available[..i]);
            r.consume(i + 1);
            return total + i + 1;
        }
        let n = available.len();
        out.extend_from_slice(available);
        r.consume(n);
        total += n;
    }
}
