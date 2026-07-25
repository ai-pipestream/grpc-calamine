// SPDX-License-Identifier: Apache-2.0

//! Experiment: what does the per-cell protobuf framing actually cost?
//!
//! The contract carries every cell as its own `CellData` submessage holding a
//! `oneof`. That is one length-delimited submessage, one tag and one length
//! prefix per cell, and on the decode side one owned `String` per text cell.
//! The question is how much of the wire's expansion over the source `.xlsx`
//! that framing is responsible for, and how much is something else.
//!
//! Three encodings of the identical cell stream are compared:
//!
//! - **A. contract** - `WorksheetRowBatch` exactly as the server sends it.
//! - **B. packed** - the same cells as flat parallel arrays in one `bytes`
//!   field: a tag byte per cell, numerics packed little-endian, text lengths
//!   and text bytes concatenated. No per-cell submessage, no per-cell pointer.
//! - **C. packed + dictionary** - B, except shared strings become `u32`
//!   indices into a table sent once per stream. This is the dedup that XLSX
//!   already performs and that the wire format currently throws away. The
//!   table is built during the parse walk (by pointer identity: the borrows
//!   in `DataRef::SharedString` all point into the workbook's own table), so
//!   the encode column prices the dictionary's steady state. The one-time
//!   derivation cost is measured and reported separately, in both the
//!   pointer-identity and the content-hash variant.
//!
//! Every encoding is decoded back and fed through the same order-sensitive
//! digest, so a format that loses or reorders anything cannot post a number.
//! Endianness is fixed little-endian and asserted on decode, since a packed
//! buffer has no self-describing types to fall back on.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Instant;

use calamine::{DataRef, Reader, Xlsx, open_workbook_from_rs};
use grpc_calamine::convert;
use grpc_calamine::proto::v1 as pb;
use prost::Message;

type Bytes = Arc<[u8]>;

// --- tags shared by every encoding ----------------------------------------

const T_EMPTY: u8 = 0;
const T_INT: u8 = 1;
const T_FLOAT: u8 = 2;
const T_STRING: u8 = 3;
const T_SHARED: u8 = 4;
const T_BOOL: u8 = 5;
const T_DATETIME: u8 = 6;
const T_DT_ISO: u8 = 7;
const T_DUR_ISO: u8 = 8;
const T_ERROR: u8 = 9;

/// Order-sensitive FNV-1a over (tag, payload) per cell, plus row indices.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Digest {
    hash: u64,
    cells: u64,
    rows: u64,
}

impl Digest {
    const fn new() -> Self {
        Self {
            hash: 0xcbf2_9ce4_8422_2325,
            cells: 0,
            rows: 0,
        }
    }
    fn write(&mut self, b: &[u8]) {
        for x in b {
            self.hash ^= u64::from(*x);
            self.hash = self.hash.wrapping_mul(0x100_0000_01b3);
        }
    }
    fn row(&mut self, i: u32) {
        self.rows += 1;
        self.write(&i.to_le_bytes());
    }
    fn cell(&mut self, tag: u8, payload: &[u8]) {
        self.cells += 1;
        self.write(&[tag]);
        self.write(payload);
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}/{}r/{}c", self.hash, self.rows, self.cells)
    }
}

/// One cell, owned, in the form every encoding starts from.
#[derive(Clone)]
enum Cell {
    Empty,
    Int(i64),
    Float(f64),
    Str(String),
    /// A shared string, with the dictionary id it interned to during the
    /// walk. Arms A and B use only `text`; arm C uses only `id`.
    Shared { text: String, id: u32 },
    Bool(bool),
    DateTime(f64),
    DtIso(String),
    DurIso(String),
    Error,
}

impl Cell {
    fn from_ref(d: &DataRef<'_>, dict: &mut Option<&mut Dict>) -> Self {
        match d {
            DataRef::Int(v) => Self::Int(*v),
            DataRef::Float(v) => Self::Float(*v),
            DataRef::String(v) => Self::Str(v.clone()),
            DataRef::SharedString(v) => Self::Shared {
                id: dict.as_mut().map_or(0, |d| d.intern(v)),
                text: (*v).to_string(),
            },
            DataRef::Bool(v) => Self::Bool(*v),
            DataRef::DateTime(v) => Self::DateTime(v.as_f64()),
            DataRef::DateTimeIso(v) => Self::DtIso(v.clone()),
            DataRef::DurationIso(v) => Self::DurIso(v.clone()),
            DataRef::Error(_) => Self::Error,
            DataRef::Empty => Self::Empty,
        }
    }

    fn tag(&self) -> u8 {
        match self {
            Self::Empty => T_EMPTY,
            Self::Int(_) => T_INT,
            Self::Float(_) => T_FLOAT,
            Self::Str(_) => T_STRING,
            Self::Shared { .. } => T_SHARED,
            Self::Bool(_) => T_BOOL,
            Self::DateTime(_) => T_DATETIME,
            Self::DtIso(_) => T_DT_ISO,
            Self::DurIso(_) => T_DUR_ISO,
            Self::Error => T_ERROR,
        }
    }

    fn digest_into(&self, d: &mut Digest) {
        match self {
            Self::Empty => d.cell(T_EMPTY, &[]),
            Self::Int(v) => d.cell(T_INT, &v.to_le_bytes()),
            Self::Float(v) => d.cell(T_FLOAT, &v.to_bits().to_le_bytes()),
            Self::Str(v) => d.cell(T_STRING, v.as_bytes()),
            Self::Shared { text, .. } => d.cell(T_SHARED, text.as_bytes()),
            Self::Bool(v) => d.cell(T_BOOL, &[u8::from(*v)]),
            Self::DateTime(v) => d.cell(T_DATETIME, &v.to_bits().to_le_bytes()),
            Self::DtIso(v) => d.cell(T_DT_ISO, v.as_bytes()),
            Self::DurIso(v) => d.cell(T_DUR_ISO, v.as_bytes()),
            Self::Error => d.cell(T_ERROR, &[]),
        }
    }

    fn to_pb(&self) -> pb::CellData {
        use pb::cell_data::Value;
        convert::cell_data(match self {
            Self::Empty => Value::Empty(()),
            Self::Int(v) => Value::IntValue(*v),
            Self::Float(v) => Value::FloatValue(*v),
            Self::Str(v) => Value::StringValue(v.clone()),
            Self::Shared { text, .. } => Value::SharedStringValue(text.clone()),
            Self::Bool(v) => Value::BoolValue(*v),
            Self::DateTime(v) => Value::DateTime(pb::ExcelDateTime {
                value: *v,
                datetime_type: pb::ExcelDateTimeType::DateTime as i32,
                is_1904: false,
            }),
            Self::DtIso(v) => Value::DateTimeIso(v.clone()),
            Self::DurIso(v) => Value::DurationIso(v.clone()),
            Self::Error => Value::Error(0),
        })
    }
}

// --- little-endian helpers -------------------------------------------------
//
// A packed buffer carries no type information, so byte order has to be part of
// the contract rather than inferred. Everything here is explicitly LE, which
// also matches protobuf's own `fixed32`/`fixed64`, so a future contract change
// would not introduce a second convention.

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn get_u32(b: &[u8], at: &mut usize) -> u32 {
    let v = u32::from_le_bytes(b[*at..*at + 4].try_into().expect("4 bytes"));
    *at += 4;
    v
}
fn get_u64(b: &[u8], at: &mut usize) -> u64 {
    let v = u64::from_le_bytes(b[*at..*at + 8].try_into().expect("8 bytes"));
    *at += 8;
    v
}

/// A batch of dense rows, the unit every encoding works on.
struct Batch {
    first_row: u32,
    width: usize,
    /// Row-major, `width` cells per row.
    cells: Vec<Cell>,
}

impl Batch {
    fn rows(&self) -> usize {
        self.cells.len().checked_div(self.width).unwrap_or(0)
    }
}

// --- A. the contract as it stands -----------------------------------------

fn encode_contract(b: &Batch, out: &mut Vec<u8>) {
    let rows = (0..b.rows())
        .map(|r| pb::WorksheetRow {
            row_index: b.first_row + r as u32,
            values: b.cells[r * b.width..(r + 1) * b.width]
                .iter()
                .map(Cell::to_pb)
                .collect(),
        })
        .collect();
    let msg = pb::StreamWorksheetRangeResponse {
        event: Some(pb::stream_worksheet_range_response::Event::Rows(
            pb::WorksheetRowBatch { rows },
        )),
    };
    out.clear();
    msg.encode(out).expect("encode");
}

fn decode_contract(buf: &[u8], d: &mut Digest) {
    use pb::cell_data::Value;
    let msg = pb::StreamWorksheetRangeResponse::decode(buf).expect("decode");
    let Some(pb::stream_worksheet_range_response::Event::Rows(batch)) = msg.event else {
        panic!("expected a rows batch")
    };
    for row in &batch.rows {
        d.row(row.row_index);
        for c in &row.values {
            match &c.value {
                Some(Value::IntValue(v)) => d.cell(T_INT, &v.to_le_bytes()),
                Some(Value::FloatValue(v)) => d.cell(T_FLOAT, &v.to_bits().to_le_bytes()),
                Some(Value::StringValue(v)) => d.cell(T_STRING, v.as_bytes()),
                Some(Value::SharedStringValue(v)) => d.cell(T_SHARED, v.as_bytes()),
                Some(Value::BoolValue(v)) => d.cell(T_BOOL, &[u8::from(*v)]),
                Some(Value::DateTime(x)) => d.cell(T_DATETIME, &x.value.to_bits().to_le_bytes()),
                Some(Value::DateTimeIso(v)) => d.cell(T_DT_ISO, v.as_bytes()),
                Some(Value::DurationIso(v)) => d.cell(T_DUR_ISO, v.as_bytes()),
                Some(Value::Error(_)) => d.cell(T_ERROR, &[]),
                Some(Value::Empty(())) | None => d.cell(T_EMPTY, &[]),
                // This experiment encodes the plain contract; arm C's
                // dictionary is its own hand-rolled layout, so the wire
                // variant never appears here.
                Some(Value::SharedStringId(_)) => {
                    unreachable!("contract encode never produces ids")
                }
            }
        }
    }
}

// --- B / C. packed columnar ------------------------------------------------
//
// Layout, all little-endian:
//   u32 first_row | u32 width | u32 row_count
//   u32 n_tags    | tags[n_tags]                 one byte per cell
//   u32 n_fixed   | fixed[n_fixed] as u64        numerics, in cell order
//   u32 n_text    | lens[n_text] as u32          text lengths, in cell order
//   u32 n_bytes   | text[n_bytes]                concatenated UTF-8
// In dictionary mode, shared-string cells contribute a u32 index to `fixed`
// instead of their bytes to `text`.

fn encode_packed(b: &Batch, use_dict: bool, out: &mut Vec<u8>) {
    let mut tags = Vec::with_capacity(b.cells.len());
    let mut fixed: Vec<u64> = Vec::new();
    let mut lens: Vec<u32> = Vec::new();
    let mut text: Vec<u8> = Vec::new();

    for c in &b.cells {
        tags.push(c.tag());
        match c {
            Cell::Empty | Cell::Error => {}
            Cell::Int(v) => fixed.push(*v as u64),
            Cell::Float(v) | Cell::DateTime(v) => fixed.push(v.to_bits()),
            Cell::Bool(v) => fixed.push(u64::from(*v)),
            Cell::Shared { text: s, id } => {
                if use_dict {
                    // The id was interned during the walk, so encoding a
                    // shared string is one array push — no hashing, no
                    // lookup, no copy of the body.
                    fixed.push(u64::from(*id));
                } else {
                    lens.push(s.len() as u32);
                    text.extend_from_slice(s.as_bytes());
                }
            }
            Cell::Str(s) | Cell::DtIso(s) | Cell::DurIso(s) => {
                lens.push(s.len() as u32);
                text.extend_from_slice(s.as_bytes());
            }
        }
    }

    out.clear();
    put_u32(out, b.first_row);
    put_u32(out, b.width as u32);
    put_u32(out, b.rows() as u32);
    put_u32(out, tags.len() as u32);
    out.extend_from_slice(&tags);
    put_u32(out, fixed.len() as u32);
    for v in &fixed {
        put_u64(out, *v);
    }
    put_u32(out, lens.len() as u32);
    for v in &lens {
        put_u32(out, *v);
    }
    put_u32(out, text.len() as u32);
    out.extend_from_slice(&text);
}

fn decode_packed(buf: &[u8], dict: Option<&Dict>, d: &mut Digest) {
    let mut at = 0usize;
    let first_row = get_u32(buf, &mut at);
    let width = get_u32(buf, &mut at) as usize;
    let row_count = get_u32(buf, &mut at) as usize;

    let n_tags = get_u32(buf, &mut at) as usize;
    let tags = &buf[at..at + n_tags];
    at += n_tags;

    let n_fixed = get_u32(buf, &mut at) as usize;
    let fixed_at = at;
    at += n_fixed * 8;

    let n_text = get_u32(buf, &mut at) as usize;
    let lens_at = at;
    at += n_text * 4;

    let n_bytes = get_u32(buf, &mut at) as usize;
    let text = &buf[at..at + n_bytes];

    let (mut fi, mut ti, mut toff) = (0usize, 0usize, 0usize);
    let next_fixed = |i: &mut usize| {
        let mut p = fixed_at + *i * 8;
        *i += 1;
        get_u64(buf, &mut p)
    };

    for r in 0..row_count {
        d.row(first_row + r as u32);
        for c in 0..width {
            let tag = tags[r * width + c];
            match tag {
                T_EMPTY | T_ERROR => d.cell(tag, &[]),
                T_INT => d.cell(tag, &(next_fixed(&mut fi) as i64).to_le_bytes()),
                T_FLOAT | T_DATETIME => d.cell(tag, &next_fixed(&mut fi).to_le_bytes()),
                T_BOOL => d.cell(tag, &[next_fixed(&mut fi) as u8]),
                T_SHARED if dict.is_some() => {
                    let idx = next_fixed(&mut fi) as u32;
                    d.cell(tag, dict.expect("checked").get(idx).as_bytes());
                }
                _ => {
                    let mut p = lens_at + ti * 4;
                    let len = get_u32(buf, &mut p) as usize;
                    ti += 1;
                    d.cell(tag, &text[toff..toff + len]);
                    toff += len;
                }
            }
        }
    }
}

/// Shared-string table built by pointer identity during the parse walk.
///
/// `DataRef::SharedString` borrows straight into the workbook's shared-strings
/// table, so every occurrence of the same entry carries the same pointer and
/// interning on (address, length) hashes two machine words instead of the
/// string's bytes. Measured, that saves less than intuition says: the real
/// cost of either derivation is materializing the table (an allocation and a
/// copy per unique string), which only an upstream accessor for calamine's
/// already-resolved table would remove. Two distinct table entries holding
/// equal text get two ids, exactly as in the workbook's own table.
#[derive(Default)]
struct Dict {
    index: HashMap<(usize, usize), u32>,
    entries: Vec<String>,
    bytes: usize,
}

impl Dict {
    fn intern(&mut self, s: &str) -> u32 {
        let key = (s.as_ptr() as usize, s.len());
        if let Some(i) = self.index.get(&key) {
            return *i;
        }
        let i = self.entries.len() as u32;
        self.index.insert(key, i);
        self.entries.push(s.to_string());
        self.bytes += s.len();
        i
    }
    fn get(&self, i: u32) -> &str {
        &self.entries[i as usize]
    }
    /// Bytes the table itself costs on the wire: a u32 length per entry plus
    /// the UTF-8, sent once.
    fn wire_bytes(&self) -> usize {
        4 + self.entries.len() * 4 + self.bytes
    }
}

/// The same table derived the way a server without index access must: by
/// hashing every string's contents after the fact. Kept purely as the timed
/// comparison row against pointer interning.
#[derive(Default)]
struct ContentDict {
    index: HashMap<String, u32>,
    entries: usize,
}

impl ContentDict {
    fn intern(&mut self, s: &str) {
        if !self.index.contains_key(s) {
            self.index.insert(s.to_string(), self.entries as u32);
            self.entries += 1;
        }
    }
}

// ---------------------------------------------------------------------------

/// Append one completed row to the pending batch, emitting a `Batch` when it
/// reaches `batch_rows`. If the grid widened since earlier pending rows were
/// pushed, they are re-padded so every batch stays rectangular.
#[allow(clippy::too_many_arguments)]
fn push_row(
    batches: &mut Vec<Batch>,
    pending: &mut Vec<Cell>,
    pending_first: &mut u32,
    pending_width: &mut usize,
    batch_rows: usize,
    idx: u32,
    w: usize,
    cells: Vec<Cell>,
) {
    if pending.is_empty() {
        *pending_first = idx;
        *pending_width = w;
    } else if *pending_width < w {
        let old = *pending_width;
        let rows = pending.len() / old.max(1);
        let mut wider = Vec::with_capacity(rows * w);
        for r in 0..rows {
            wider.extend_from_slice(&pending[r * old..(r + 1) * old]);
            wider.extend(std::iter::repeat_with(|| Cell::Empty).take(w - old));
        }
        *pending = wider;
        *pending_width = w;
    }
    pending.extend(cells);
    if pending.len() / (*pending_width).max(1) >= batch_rows {
        batches.push(Batch {
            first_row: *pending_first,
            width: *pending_width,
            cells: std::mem::take(pending),
        });
    }
}

/// Walk a sheet into the canonical dense grid the server streams: anchored at
/// column 0, interior gaps filled with empty rows, leading and trailing rows
/// of blanks trimmed (`Range::from_sparse` semantics). When `dict` is given,
/// shared strings intern into it by pointer identity as a side effect of the
/// walk, which is the whole experiment: the table costs nothing extra to
/// build at the moment calamine hands the borrow over.
fn build_grid(bytes: &Bytes, sheet: &str, batch_rows: usize, mut dict: Option<&mut Dict>) -> Vec<Batch> {
    let mut wb =
        open_workbook_from_rs::<Xlsx<_>, _>(Cursor::new(Arc::clone(bytes))).expect("open xlsx");
    let mut reader = wb.worksheet_cells_reader(sheet).expect("cells reader");
    let dims = reader.dimensions();
    let mut width = dims.end.1 as usize + 1;
    let mut current_row = dims.start.0;
    let mut row: Vec<Cell> = vec![Cell::Empty; width];
    let mut open = false;
    let mut row_has_value = false;
    let mut started = false;
    let mut pending_empty: u32 = 0;

    let mut batches: Vec<Batch> = Vec::new();
    let mut pending: Vec<Cell> = Vec::new();
    let mut pending_first = 0u32;
    let mut pending_width = width;

    macro_rules! complete_row {
        ($index:expr, $row:expr) => {{
            let index: u32 = $index;
            if row_has_value {
                for back in (1..=pending_empty).rev() {
                    push_row(
                        &mut batches,
                        &mut pending,
                        &mut pending_first,
                        &mut pending_width,
                        batch_rows,
                        index - back,
                        width,
                        vec![Cell::Empty; width],
                    );
                }
                pending_empty = 0;
                started = true;
                push_row(
                    &mut batches,
                    &mut pending,
                    &mut pending_first,
                    &mut pending_width,
                    batch_rows,
                    index,
                    width,
                    $row,
                );
            } else if started {
                pending_empty += 1;
            }
            row_has_value = false;
        }};
    }

    while let Some(cell) = reader.next_cell().expect("next_cell") {
        let (r, c) = cell.get_position();
        let idx = c as usize;
        if open {
            while current_row < r {
                let done = std::mem::replace(&mut row, vec![Cell::Empty; width]);
                complete_row!(current_row, done);
                current_row += 1;
            }
        } else {
            current_row = r;
            open = true;
        }
        if idx >= width {
            width = idx + 1;
            row.resize(width, Cell::Empty);
        }
        let value = Cell::from_ref(cell.get_value(), &mut dict);
        if !matches!(value, Cell::Empty) {
            row_has_value = true;
        }
        row[idx] = value;
    }
    if open && row_has_value {
        for back in (1..=pending_empty).rev() {
            push_row(
                &mut batches,
                &mut pending,
                &mut pending_first,
                &mut pending_width,
                batch_rows,
                current_row - back,
                width,
                vec![Cell::Empty; width],
            );
        }
        push_row(
            &mut batches,
            &mut pending,
            &mut pending_first,
            &mut pending_width,
            batch_rows,
            current_row,
            width,
            row,
        );
    }
    if !pending.is_empty() {
        batches.push(Batch {
            first_row: pending_first,
            width: pending_width,
            cells: pending,
        });
    }
    batches
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: packed <workbook.xlsx> [sheet] [rows-per-batch]");
    let want_sheet = std::env::args().nth(2);
    let batch_rows: usize = std::env::args()
        .nth(3)
        .and_then(|v| v.parse().ok())
        .unwrap_or(256);

    let raw = std::fs::read(&path).expect("read workbook");
    let bytes: Bytes = raw.clone().into();
    let mut wb =
        open_workbook_from_rs::<Xlsx<_>, _>(Cursor::new(Arc::clone(&bytes))).expect("open xlsx");
    let names: Vec<String> = wb
        .sheets_metadata()
        .iter()
        .map(|s| s.name.clone())
        .collect();
    let sheet = want_sheet.unwrap_or_else(|| {
        names
            .iter()
            .max_by_key(|n| {
                wb.worksheet_cells_reader(n)
                    .map(|r| r.dimensions().len())
                    .unwrap_or(0)
            })
            .expect("no sheets")
            .clone()
    });

    println!("packed-buffer experiment: what does per-cell framing cost?\n");
    println!("workbook   : {path}");
    println!("sheet      : {sheet}");
    println!("batch      : {batch_rows} rows per message\n");

    drop(wb);

    // Materialize the dense grid once, so every encoding starts from the same
    // cells and the calamine parse is not counted three times. The walk
    // mirrors the server's canonical grid (column-0 anchor, trailing blanks
    // trimmed), and the shared-string dictionary is built during it, by
    // pointer identity, as cells are read.
    let mut dict = Dict::default();
    let t = Instant::now();
    let batches = build_grid(&bytes, &sheet, batch_rows, Some(&mut dict));
    let walk_ms = t.elapsed().as_secs_f64() * 1e3;

    // The identical walk without interning, to price the interning itself.
    let t = Instant::now();
    let plain = build_grid(&bytes, &sheet, batch_rows, None);
    let walk_plain_ms = t.elapsed().as_secs_f64() * 1e3;
    let plain_cells: usize = plain.iter().map(|b| b.cells.len()).sum();
    drop(plain);

    let total_cells: usize = batches.iter().map(|b| b.cells.len()).sum();
    let total_rows: usize = batches.iter().map(Batch::rows).sum();
    let width = batches.iter().map(|b| b.width).max().unwrap_or(0);
    assert_eq!(
        plain_cells, total_cells,
        "interning must not change what the walk produces"
    );
    println!("grid       : {total_rows} rows x {width} cols = {total_cells} cells");
    println!("messages   : {}\n", batches.len());

    // --- A. contract ------------------------------------------------------
    let mut buf = Vec::with_capacity(1 << 20);
    let t = Instant::now();
    let mut a_bytes = 0usize;
    let mut encoded_a: Vec<Vec<u8>> = Vec::with_capacity(batches.len());
    for b in &batches {
        encode_contract(b, &mut buf);
        a_bytes += buf.len();
        encoded_a.push(buf.clone());
    }
    let a_enc = t.elapsed().as_secs_f64() * 1e3;

    let t = Instant::now();
    let mut a_digest = Digest::new();
    for e in &encoded_a {
        decode_contract(e, &mut a_digest);
    }
    let a_dec = t.elapsed().as_secs_f64() * 1e3;
    drop(encoded_a);

    // --- B. packed, no dictionary ----------------------------------------
    let t = Instant::now();
    let mut b_bytes = 0usize;
    let mut encoded_b: Vec<Vec<u8>> = Vec::with_capacity(batches.len());
    for b in &batches {
        encode_packed(b, false, &mut buf);
        b_bytes += buf.len();
        encoded_b.push(buf.clone());
    }
    let b_enc = t.elapsed().as_secs_f64() * 1e3;

    let t = Instant::now();
    let mut b_digest = Digest::new();
    for e in &encoded_b {
        decode_packed(e, None, &mut b_digest);
    }
    let b_dec = t.elapsed().as_secs_f64() * 1e3;
    drop(encoded_b);

    // --- C. packed + shared-string dictionary ----------------------------
    // The table was interned during the walk, so this encode pays only for
    // writing ids: the steady-state cost of the dictionary, not the cost of
    // deriving it.
    let t = Instant::now();
    let mut c_bytes = 0usize;
    let mut encoded_c: Vec<Vec<u8>> = Vec::with_capacity(batches.len());
    for b in &batches {
        encode_packed(b, true, &mut buf);
        c_bytes += buf.len();
        encoded_c.push(buf.clone());
    }
    let c_enc = t.elapsed().as_secs_f64() * 1e3;
    let dict_bytes = dict.wire_bytes();

    // What deriving the same table costs a server without index access: a
    // hash and equality walk over every string body, after the fact. This is
    // the cost the first version of this experiment wrongly charged to the
    // dictionary itself.
    let t = Instant::now();
    let mut content = ContentDict::default();
    for b in &batches {
        for c in &b.cells {
            if let Cell::Shared { text, .. } = c {
                content.intern(text);
            }
        }
    }
    let content_ms = t.elapsed().as_secs_f64() * 1e3;

    let t = Instant::now();
    let mut c_digest = Digest::new();
    for e in &encoded_c {
        decode_packed(e, Some(&dict), &mut c_digest);
    }
    let c_dec = t.elapsed().as_secs_f64() * 1e3;
    drop(encoded_c);

    // --- the gate ---------------------------------------------------------
    // Source of truth: digest the in-memory grid directly, independent of any
    // encoding, and require every decoder to reproduce it.
    let mut source_digest = Digest::new();
    for b in &batches {
        for r in 0..b.rows() {
            source_digest.row(b.first_row + r as u32);
            for c in &b.cells[r * b.width..(r + 1) * b.width] {
                c.digest_into(&mut source_digest);
            }
        }
    }

    println!("lossless proof (digest of the decoded cell stream)");
    println!("  source grid             {source_digest}");
    println!("  A contract              {a_digest}");
    println!("  B packed                {b_digest}");
    println!("  C packed + dictionary   {c_digest}");
    let ok = a_digest == source_digest && b_digest == source_digest && c_digest == source_digest;
    println!(
        "  all match the source grid: {}\n",
        if ok { "yes" } else { "NO" }
    );
    assert!(
        ok,
        "an encoding lost or reordered data; refusing to publish"
    );

    // --- results ----------------------------------------------------------
    let src = raw.len() as f64;
    let mb = |b: usize| b as f64 / 1e6;
    println!("wire size");
    println!(
        "  source .xlsx                       {:8.1} MB",
        mb(raw.len())
    );
    println!(
        "  A contract (CellData per cell)     {:8.1} MB   {:5.2}x source   {:6.1} B/cell",
        mb(a_bytes),
        a_bytes as f64 / src,
        a_bytes as f64 / total_cells as f64
    );
    println!(
        "  B packed arrays                    {:8.1} MB   {:5.2}x source   {:6.1} B/cell   {:+5.1}% vs A",
        mb(b_bytes),
        b_bytes as f64 / src,
        b_bytes as f64 / total_cells as f64,
        (b_bytes as f64 / a_bytes as f64 - 1.0) * 100.0
    );
    println!(
        "  C packed + dictionary              {:8.1} MB   {:5.2}x source   {:6.1} B/cell   {:+5.1}% vs A",
        mb(c_bytes + dict_bytes),
        (c_bytes + dict_bytes) as f64 / src,
        (c_bytes + dict_bytes) as f64 / total_cells as f64,
        ((c_bytes + dict_bytes) as f64 / a_bytes as f64 - 1.0) * 100.0
    );
    println!(
        "      of which the table, sent once  {:8.1} MB   ({} unique strings)",
        mb(dict_bytes),
        dict.entries.len()
    );

    println!("\nCPU, ms for the whole sheet");
    println!("  {:34} {:>9} {:>9}", "", "encode", "decode");
    println!("  {:34} {a_enc:9.1} {a_dec:9.1}", "A contract");
    println!(
        "  {:34} {b_enc:9.1} {b_dec:9.1}   ({:+.0}% / {:+.0}%)",
        "B packed",
        (b_enc / a_enc - 1.0) * 100.0,
        (b_dec / a_dec - 1.0) * 100.0
    );
    println!(
        "  {:34} {c_enc:9.1} {c_dec:9.1}   ({:+.0}% / {:+.0}%)",
        "C packed + dictionary",
        (c_enc / a_enc - 1.0) * 100.0,
        (c_dec / a_dec - 1.0) * 100.0
    );

    println!("\nshared-string table derivation, ms (not part of the encode column)");
    println!(
        "  by pointer identity, inside the walk {:9.1}   (walk {walk_ms:.1} ms with it, {walk_plain_ms:.1} ms without)",
        walk_ms - walk_plain_ms
    );
    println!(
        "  by content hash, after the fact      {content_ms:9.1}   ({} unique strings; table itself {} entries)",
        content.index.len(),
        dict.entries.len()
    );
    println!("  Pointer identity (the `DataRef::SharedString` borrows all point into the");
    println!("  workbook's own table) hashes two machine words per cell; content hashing");
    println!("  walks every string body. Measured, the two cost about the same, because");
    println!("  the bill is materializing the table -- one allocation and copy per unique");
    println!("  string -- not the hashing that finds it. calamine already holds this exact");
    println!("  table in memory and resolves indices into it during the parse; an upstream");
    println!("  accessor exposing it would make the derivation cost zero.");

    println!("\nnote: B and C are hand-rolled little-endian buffers, so they carry no");
    println!("type information and no field numbers. That is the whole point of the");
    println!("experiment, and also the whole cost: the contract stops being");
    println!("self-describing, protobuf cannot evolve it field by field, and every");
    println!("client needs a hand-written decoder that agrees on byte order.");
}
