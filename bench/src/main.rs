// SPDX-License-Identifier: Apache-2.0

//! What does reaching calamine over gRPC actually cost?
//!
//! The comparison is between in-process calamine and the same work delivered
//! by `grpc-calamine` over a socket. It is built to be hard to accuse of
//! stacking the deck:
//!
//! - **Every arm proves it did the same work.** Arms 1 to 3 feed an
//!   order-sensitive digest over the identical canonical cell stream (dense
//!   rows, explicit empties, gap rows filled, the exact grid the server
//!   emits). If two digests disagree the run aborts instead of printing
//!   numbers. Arm 0 is deliberately *not* gated: it walks only populated cells,
//!   which is strictly less work, and is labelled as such.
//! - **The ladder isolates each cost.** Arm 0 is calamine alone; arm 1 adds
//!   densification; arm 2 adds the protobuf conversion and encode; arm 3 adds
//!   the socket. Each step's marginal cost is the honest answer to "where does
//!   the time go".
//! - **CPU-seconds are reported next to wall clock.** gRPC spreads work over a
//!   blocking parser thread, tokio workers and a separate client process. Wall
//!   clock alone would credit it for using more cores, so total CPU across both
//!   processes is printed beside it.
//! - **Arms are interleaved** within each iteration and reported as min /
//!   median / p95, never a bare mean, so clock drift cannot masquerade as an
//!   effect.
//! - **The upload leg is reported separately**, because always counting it
//!   flatters native and never counting it flatters gRPC.
//!
//! Run it with the server binary already built:
//!
//! ```bash
//! cargo build --release            # from the repository root
//! cd bench && cargo run --release -- <workbook.xlsx> [sheet] [iterations]
//! ```

use std::io::{Cursor, IoSlice};
use std::pin::Pin;
use std::process::{Child, Command};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Instant;

use tokio::io::ReadBuf;

use calamine::{DataRef, Reader, Xlsx, open_workbook_from_rs};
use grpc_calamine::convert;
use grpc_calamine::proto::v1 as pb;
use pb::calamine_service_client::CalamineServiceClient;
use prost::Message;

/// Upload frame size. Matches the reference chunking used by the demos.
const CHUNK: usize = 1024 * 1024;

/// Port the benchmark's own server instance listens on.
const PORT: u16 = 50077;

type Bytes = Arc<[u8]>;

// ---------------------------------------------------------------------------
// socket byte counting
// ---------------------------------------------------------------------------

/// A `TcpStream` that counts every byte crossing it.
///
/// Wire size used to be inferred from the encoded protobuf payload, which is
/// exact only while nothing between prost and the socket changes the bytes.
/// Compression breaks that equivalence, so the honest number is counted here,
/// at the socket, HTTP/2 framing included.
struct CountedTcp {
    inner: tokio::net::TcpStream,
    read: Arc<AtomicU64>,
    written: Arc<AtomicU64>,
}

impl tokio::io::AsyncRead for CountedTcp {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &poll {
            self.read
                .fetch_add((buf.filled().len() - before) as u64, Ordering::Relaxed);
        }
        poll
    }
}

impl tokio::io::AsyncWrite for CountedTcp {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &poll {
            self.written.fetch_add(*n as u64, Ordering::Relaxed);
        }
        poll
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        if let Poll::Ready(Ok(n)) = &poll {
            self.written.fetch_add(*n as u64, Ordering::Relaxed);
        }
        poll
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

/// Connect to `endpoint` through a [`CountedTcp`], retrying until the server
/// listens. Mirrors what `Endpoint::connect` would do (TCP_NODELAY on, the
/// same window settings), with the counters as the only addition.
async fn connect_counted(
    endpoint: &str,
    window: u32,
    read: &Arc<AtomicU64>,
    written: &Arc<AtomicU64>,
) -> tonic::transport::Channel {
    loop {
        let mut ep = tonic::transport::Endpoint::from_shared(endpoint.to_string())
            .expect("valid endpoint");
        if window > 0 {
            ep = ep
                .initial_stream_window_size(window)
                .initial_connection_window_size(window);
        }
        let (read, written) = (Arc::clone(read), Arc::clone(written));
        let connector = tower::service_fn(move |uri: tonic::transport::Uri| {
            let (read, written) = (Arc::clone(&read), Arc::clone(&written));
            async move {
                let authority = uri
                    .authority()
                    .expect("endpoint uri has an authority")
                    .as_str()
                    .to_string();
                let inner = tokio::net::TcpStream::connect(authority).await?;
                inner.set_nodelay(true)?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(CountedTcp {
                    inner,
                    read,
                    written,
                }))
            }
        });
        match ep.connect_with_connector(connector).await {
            Ok(ch) => break ch,
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
}

// ---------------------------------------------------------------------------
// digest: the proof that two arms did the same work
// ---------------------------------------------------------------------------

/// Order-sensitive FNV-1a over the canonical cell stream.
///
/// Both the calamine-side and protobuf-side feeders map a value to the same
/// tag and bytes, so an arm that skips a cell, reorders rows, drops an empty
/// or truncates the grid produces a different digest.
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

    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.hash ^= u64::from(*b);
            self.hash = self.hash.wrapping_mul(0x100_0000_01b3);
        }
    }

    fn row(&mut self, row_index: u32) {
        self.rows += 1;
        self.write(&row_index.to_le_bytes());
    }

    /// Feed one cell as (tag, payload). The tag numbering is shared by both
    /// feeders below and must not be reordered.
    fn cell(&mut self, tag: u8, payload: &[u8]) {
        self.cells += 1;
        self.write(&[tag]);
        self.write(payload);
    }

    fn push_data_ref(&mut self, d: &DataRef<'_>) {
        match d {
            DataRef::Int(v) => self.cell(1, &v.to_le_bytes()),
            DataRef::Float(v) => self.cell(2, &v.to_bits().to_le_bytes()),
            DataRef::String(v) => self.cell(3, v.as_bytes()),
            DataRef::SharedString(v) => self.cell(4, v.as_bytes()),
            DataRef::Bool(v) => self.cell(5, &[u8::from(*v)]),
            DataRef::DateTime(v) => self.cell(6, &v.as_f64().to_bits().to_le_bytes()),
            DataRef::DateTimeIso(v) => self.cell(7, v.as_bytes()),
            DataRef::DurationIso(v) => self.cell(8, v.as_bytes()),
            DataRef::Error(_) => self.cell(9, &[]),
            DataRef::Empty => self.cell(0, &[]),
        }
    }

    fn push_pb(&mut self, v: Option<&pb::cell_data::Value>) {
        use pb::cell_data::Value;
        match v {
            Some(Value::IntValue(v)) => self.cell(1, &v.to_le_bytes()),
            Some(Value::FloatValue(v)) => self.cell(2, &v.to_bits().to_le_bytes()),
            Some(Value::StringValue(v)) => self.cell(3, v.as_bytes()),
            Some(Value::SharedStringValue(v)) => self.cell(4, v.as_bytes()),
            Some(Value::BoolValue(v)) => self.cell(5, &[u8::from(*v)]),
            Some(Value::DateTime(d)) => self.cell(6, &d.value.to_bits().to_le_bytes()),
            Some(Value::DateTimeIso(v)) => self.cell(7, v.as_bytes()),
            Some(Value::DurationIso(v)) => self.cell(8, v.as_bytes()),
            Some(Value::Error(_)) => self.cell(9, &[]),
            Some(Value::Empty(())) | None => self.cell(0, &[]),
            // Ids must be resolved against the streamed table before
            // digesting, so dictionary mode is held to the same digest as
            // every other arm. See `arm_grpc`.
            Some(Value::SharedStringId(_)) => {
                unreachable!("resolve shared_string_id before digesting")
            }
        }
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}/{}r/{}c", self.hash, self.rows, self.cells)
    }
}

// ---------------------------------------------------------------------------
// process accounting
// ---------------------------------------------------------------------------

/// Total CPU seconds (user + sys) charged to a pid, from `/proc/<pid>/stat`.
///
/// The split is on the last `)` because the comm field is parenthesised and
/// may itself contain spaces or parentheses.
fn cpu_seconds(pid: u32) -> f64 {
    let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return 0.0;
    };
    let Some((_, rest)) = text.rsplit_once(')') else {
        return 0.0;
    };
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let tick = |i: usize| -> f64 {
        fields
            .get(i)
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    // After the comm field, `state` is index 0, so utime/stime (fields 14 and
    // 15 of the raw record) land at 11 and 12.
    (tick(11) + tick(12)) / sysconf_clock_ticks()
}

/// Kernel tick rate. 100 Hz on every mainstream Linux build; read from the
/// environment when something exotic overrides it.
fn sysconf_clock_ticks() -> f64 {
    std::env::var("CLK_TCK")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100.0)
}

/// Peak RSS in MiB for a pid, from `/proc/<pid>/status`.
fn peak_rss_mib(pid: u32) -> f64 {
    let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return 0.0;
    };
    for line in text.lines() {
        if let Some(kb) = line
            .strip_prefix("VmHWM:")
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse::<f64>().ok())
        {
            return kb / 1024.0;
        }
    }
    0.0
}

// ---------------------------------------------------------------------------
// arms
// ---------------------------------------------------------------------------

fn open_xlsx(bytes: &Bytes) -> Xlsx<Cursor<Bytes>> {
    open_workbook_from_rs::<Xlsx<_>, _>(Cursor::new(Arc::clone(bytes))).expect("open xlsx")
}

/// Arm 0: what a calamine user writes. Walks only populated cells, so it does
/// strictly less work than the others and is never digest-compared to them.
fn arm_native_sparse(bytes: &Bytes, sheet: &str) -> (f64, Digest) {
    let t = Instant::now();
    let mut wb = open_xlsx(bytes);
    let mut reader = wb.worksheet_cells_reader(sheet).expect("cells reader");
    let mut digest = Digest::new();
    while let Some(cell) = reader.next_cell().expect("next_cell") {
        let (row, _col) = cell.get_position();
        digest.row(row);
        digest.push_data_ref(cell.get_value());
    }
    let ms = t.elapsed().as_secs_f64() * 1e3;
    (ms, digest)
}

/// Arm 1: calamine plus densification into the canonical grid, values kept as
/// calamine's own `DataRef`. Isolates the cost of building the dense row shape
/// the contract requires, without paying for protobuf.
///
/// The densification mirrors `service::emit_incremental` exactly: the declared
/// `<dimension>` only pre-sizes the row, a cell past it grows the row, and the
/// first populated cell fixes the starting row.
/// Walk a sheet into the canonical dense grid, mirroring
/// `service::emit_incremental` including how it treats blank rows.
///
/// Duplicating that rule here is deliberate: the digest gate then compares two
/// independent implementations of the contract instead of comparing the server
/// against itself. A row counts only if it holds a non-empty cell, matching
/// `Range::from_sparse`, so leading and trailing rows of blanks are dropped and
/// interior gaps are released once a later non-empty row proves they were gaps.
fn walk_dense<T>(
    dims: calamine::Dimensions,
    mut next: impl FnMut() -> Option<(u32, u32, T, bool)>,
    empty: impl Fn() -> T,
    mut on_row: impl FnMut(u32, Vec<T>),
) {
    // Anchored at column 0, like the server: a value's index is its absolute
    // column, and a cell left of a wrong declared start cannot be dropped.
    let mut width = dims.end.1 as usize + 1;
    let mut current_row = dims.start.0;
    let mut values: Vec<T> = (0..width).map(|_| empty()).collect();
    let mut open = false;
    let mut row_has_value = false;
    let mut started = false;
    let mut pending_empty: u32 = 0;

    while let Some((row, col, value, is_empty)) = next() {
        let idx = col as usize;
        if open {
            while current_row < row {
                let fresh: Vec<T> = (0..width).map(|_| empty()).collect();
                let done = std::mem::replace(&mut values, fresh);
                if row_has_value {
                    for back in (1..=pending_empty).rev() {
                        on_row(current_row - back, (0..width).map(|_| empty()).collect());
                    }
                    pending_empty = 0;
                    started = true;
                    on_row(current_row, done);
                } else if started {
                    pending_empty += 1;
                }
                row_has_value = false;
                current_row += 1;
            }
        } else {
            current_row = row;
            open = true;
        }
        while values.len() <= idx {
            values.push(empty());
        }
        width = width.max(values.len());
        if !is_empty {
            row_has_value = true;
        }
        values[idx] = value;
    }

    if open && row_has_value {
        for back in (1..=pending_empty).rev() {
            on_row(current_row - back, (0..width).map(|_| empty()).collect());
        }
        on_row(current_row, values);
    }
}

fn arm_native_dense(bytes: &Bytes, sheet: &str) -> (f64, Digest) {
    let t = Instant::now();
    let mut wb = open_xlsx(bytes);
    let mut reader = wb.worksheet_cells_reader(sheet).expect("cells reader");
    let dims = reader.dimensions();
    let mut digest = Digest::new();
    walk_dense(
        dims,
        || {
            reader.next_cell().expect("next_cell").map(|c| {
                let (r, col) = c.get_position();
                let v = c.get_value();
                (r, col, Owned::from_ref(v), matches!(v, DataRef::Empty))
            })
        },
        || Owned::Empty,
        |row, values| {
            digest.row(row);
            for v in &values {
                v.digest_into(&mut digest);
            }
        },
    );
    (t.elapsed().as_secs_f64() * 1e3, digest)
}

/// An owned cell value for arm 1.
///
/// `DataRef` borrows the reader, so a dense grid has to own its values. This
/// mirrors `DataRef`'s variants one for one, and critically keeps
/// `SharedString` distinct from `String`: collapsing the two would make arm 1
/// disagree with the protobuf arms, which carry `shared_string_value` as its
/// own field. Owning the strings here is not an artefact of the benchmark, it
/// is exactly what the server's own densification does.
enum Owned {
    Int(i64),
    Float(f64),
    Str(String),
    Shared(String),
    Bool(bool),
    DateTime(f64),
    DateTimeIso(String),
    DurationIso(String),
    Error,
    Empty,
}

impl Owned {
    fn from_ref(d: &DataRef<'_>) -> Self {
        match d {
            DataRef::Int(v) => Self::Int(*v),
            DataRef::Float(v) => Self::Float(*v),
            DataRef::String(v) => Self::Str(v.clone()),
            DataRef::SharedString(v) => Self::Shared((*v).to_string()),
            DataRef::Bool(v) => Self::Bool(*v),
            DataRef::DateTime(v) => Self::DateTime(v.as_f64()),
            DataRef::DateTimeIso(v) => Self::DateTimeIso(v.clone()),
            DataRef::DurationIso(v) => Self::DurationIso(v.clone()),
            DataRef::Error(_) => Self::Error,
            DataRef::Empty => Self::Empty,
        }
    }

    /// Same tag numbering as the other two feeders.
    fn digest_into(&self, d: &mut Digest) {
        match self {
            Self::Int(v) => d.cell(1, &v.to_le_bytes()),
            Self::Float(v) => d.cell(2, &v.to_bits().to_le_bytes()),
            Self::Str(v) => d.cell(3, v.as_bytes()),
            Self::Shared(v) => d.cell(4, v.as_bytes()),
            Self::Bool(v) => d.cell(5, &[u8::from(*v)]),
            Self::DateTime(v) => d.cell(6, &v.to_bits().to_le_bytes()),
            Self::DateTimeIso(v) => d.cell(7, v.as_bytes()),
            Self::DurationIso(v) => d.cell(8, v.as_bytes()),
            Self::Error => d.cell(9, &[]),
            Self::Empty => d.cell(0, &[]),
        }
    }
}

/// Arm 2: everything the server does, in process. Densify, convert to
/// protobuf, and prost-encode each row exactly as it would go on the wire.
/// Returns the encoded byte total so the wire expansion can be reported.
fn arm_native_encode(bytes: &Bytes, sheet: &str, is_1904: bool) -> (f64, Digest, u64) {
    let t = Instant::now();
    let mut wb = open_xlsx(bytes);
    let mut reader = wb.worksheet_cells_reader(sheet).expect("cells reader");
    let dims = reader.dimensions();
    let mut digest = Digest::new();
    let mut wire = 0u64;
    let mut buf = Vec::with_capacity(64 * 1024);
    walk_dense(
        dims,
        || {
            reader.next_cell().expect("next_cell").map(|c| {
                let (r, col) = c.get_position();
                let v = c.get_value();
                let empty = matches!(v, DataRef::Empty);
                (
                    r,
                    col,
                    convert::cell_data(convert::data_ref_value(v, is_1904)),
                    empty,
                )
            })
        },
        convert::empty_cell_data,
        |row_index, values| {
            digest.row(row_index);
            for c in &values {
                digest.push_pb(c.value.as_ref());
            }
            let msg = pb::StreamWorksheetRangeResponse {
                event: Some(pb::stream_worksheet_range_response::Event::Row(
                    pb::WorksheetRow { row_index, values },
                )),
            };
            buf.clear();
            msg.encode(&mut buf).expect("encode");
            wire += buf.len() as u64;
        },
    );
    (t.elapsed().as_secs_f64() * 1e3, digest, wire)
}

/// Arm 3: the full gRPC path. Returns (total ms, time-to-first-row ms,
/// digest, row messages, string-table entries).
///
/// With `dict` set the request opts into `use_string_table`; ids are
/// resolved against the streamed table and digested as the shared strings
/// they stand for, so dictionary mode must reproduce the identical digest
/// or the run aborts.
async fn arm_grpc(
    client: &mut CalamineServiceClient<tonic::transport::Channel>,
    workbook_id: &str,
    sheet: &str,
    batch: u32,
    dict: bool,
) -> (f64, f64, Digest, u64, u64) {
    let t = Instant::now();
    let mut stream = client
        .stream_worksheet_range(pb::StreamWorksheetRangeRequest {
            workbook_id: workbook_id.to_string(),
            sheet: Some(pb::SheetSelector {
                selector: Some(pb::sheet_selector::Selector::SheetName(sheet.to_string())),
            }),
            max_rows_per_message: batch,
            use_string_table: dict,
        })
        .await
        .expect("stream request")
        .into_inner();

    let mut digest = Digest::new();
    let mut ttfr = 0.0;
    let mut messages = 0u64;
    let mut table: Vec<String> = Vec::new();
    let feed = |digest: &mut Digest, table: &[String], r: pb::WorksheetRow| {
        digest.row(r.row_index);
        for c in &r.values {
            match c.value.as_ref() {
                Some(pb::cell_data::Value::SharedStringId(id)) => {
                    digest.cell(4, table[*id as usize].as_bytes());
                }
                v => digest.push_pb(v),
            }
        }
    };
    while let Some(msg) = stream.message().await.expect("stream event") {
        match msg.event {
            // Rows arrive batched unless `max_rows_per_message` asks for one
            // per message; both carriers produce the same digest.
            Some(pb::stream_worksheet_range_response::Event::Rows(batch)) => {
                if digest.rows == 0 {
                    ttfr = t.elapsed().as_secs_f64() * 1e3;
                }
                messages += 1;
                for r in batch.rows {
                    feed(&mut digest, &table, r);
                }
            }
            Some(pb::stream_worksheet_range_response::Event::Row(r)) => {
                if digest.rows == 0 {
                    ttfr = t.elapsed().as_secs_f64() * 1e3;
                }
                messages += 1;
                feed(&mut digest, &table, r);
            }
            Some(pb::stream_worksheet_range_response::Event::StringTable(chunk)) => {
                assert_eq!(
                    chunk.first_id as usize,
                    table.len(),
                    "string table chunks must arrive dense and in order"
                );
                table.extend(chunk.entries);
            }
            _ => {}
        }
    }
    (
        t.elapsed().as_secs_f64() * 1e3,
        ttfr,
        digest,
        messages,
        table.len() as u64,
    )
}

// ---------------------------------------------------------------------------
// statistics and reporting
// ---------------------------------------------------------------------------

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i]
}

struct Stat {
    min: f64,
    median: f64,
    p95: f64,
}

fn stat(mut v: Vec<f64>) -> Stat {
    v.sort_by(|a, b| a.partial_cmp(b).expect("no NaN timings"));
    Stat {
        min: v[0],
        median: pct(&v, 0.5),
        p95: pct(&v, 0.95),
    }
}

// ---------------------------------------------------------------------------

/// Start the server this benchmark measures, on its own port.
fn start_server() -> Child {
    let bin =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/release/grpc-calamine");
    assert!(
        bin.exists(),
        "build the server first: cargo build --release (from the repository root)"
    );
    Command::new(bin)
        .env("GRPC_CALAMINE_ADDR", format!("127.0.0.1:{PORT}"))
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn grpc-calamine")
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: grpc-calamine-bench <workbook.xlsx> [sheet] [iterations]");
    let want_sheet = args.next();
    let iterations: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(7);

    let raw = std::fs::read(&path).expect("read workbook");
    let bytes: Bytes = raw.clone().into();

    // Resolve the sheet: the largest one unless named, since sheet 0 of a big
    // workbook is often a small cover sheet.
    let mut probe = open_xlsx(&bytes);
    let names: Vec<String> = probe
        .sheets_metadata()
        .iter()
        .map(|s| s.name.clone())
        .collect();
    let sheet = want_sheet.unwrap_or_else(|| {
        names
            .iter()
            .max_by_key(|n| {
                probe
                    .worksheet_cells_reader(n)
                    .map(|r| r.dimensions().len())
                    .unwrap_or(0)
            })
            .expect("workbook has no sheets")
            .clone()
    });
    let is_1904 = probe.has_1904_epoch();
    drop(probe);

    println!("grpc-calamine: in-process calamine vs the same work over gRPC\n");
    println!("workbook    : {path}");
    println!("size        : {:.1} MB", raw.len() as f64 / 1e6);
    println!(
        "sheet       : {sheet}   (of {}: {})",
        names.len(),
        names.join(", ")
    );
    println!("iterations  : {iterations}, arms interleaved");
    println!(
        "host        : {} logical cores",
        std::thread::available_parallelism().map_or(0, usize::from)
    );
    println!("profile     : release (cargo defaults; no [profile.release] overrides)\n");

    // BENCH_ADDR points at an already-running server, which is how the
    // over-a-network runs are done: arms 0-2 stay local (they are the "just
    // parse it here" baseline) while arm 3 crosses the wire. Server CPU and RSS
    // are only observable for a local child, so they are reported as 0 remotely.
    let remote = std::env::var("BENCH_ADDR").ok();
    let server = remote.is_none().then(start_server);
    let server_pid = server.as_ref().map_or(0, Child::id);

    // Wait for the listener rather than sleeping a fixed amount.
    //
    // The client's own window is what governs the row stream: flow control is
    // directional, so the server's setting sizes the upload and this one sizes
    // the download. `WINDOW_BYTES=0` leaves hyper's 1 MiB default in place, so
    // the two can be compared.
    let window: u32 = std::env::var("WINDOW_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50 * 1024 * 1024);
    println!(
        "client http2 window: {}",
        if window == 0 {
            "hyper default (1 MiB)".to_string()
        } else {
            format!("{window} bytes")
        }
    );

    // BENCH_COMPRESSION selects the grpc-encoding the client asks the server
    // to use for responses: none (default), gzip or zstd. The upload leg
    // stays uncompressed either way; a workbook is already deflated and
    // recompressing it spends CPU on bytes that do not shrink.
    let compression_label = std::env::var("BENCH_COMPRESSION").unwrap_or_else(|_| "none".into());
    let compression = match compression_label.as_str() {
        "none" | "" => None,
        "gzip" => Some(tonic::codec::CompressionEncoding::Gzip),
        "zstd" => Some(tonic::codec::CompressionEncoding::Zstd),
        other => panic!("BENCH_COMPRESSION must be none, gzip or zstd, not {other}"),
    };
    println!("compression : {compression_label} (grpc-encoding for the row stream)");

    // BENCH_DICT=1 opts arm 3 into the contract's `use_string_table` mode:
    // shared strings arrive once in table chunks and as u32 ids per cell,
    // and the client resolves them. The digest gate holds it to the same
    // canonical cell stream as every other arm.
    let dict = std::env::var("BENCH_DICT").is_ok_and(|v| v == "1" || v == "true");
    println!(
        "string dict : {}\n",
        if dict { "on (use_string_table)" } else { "off" }
    );
    let endpoint = remote.clone().map_or_else(
        || format!("http://127.0.0.1:{PORT}"),
        |a| format!("http://{a}"),
    );
    if let Some(a) = &remote {
        println!("server     : remote at {a} (arms 0-2 remain local)\n");
    }
    let sock_read = Arc::new(AtomicU64::new(0));
    let sock_written = Arc::new(AtomicU64::new(0));
    let channel = connect_counted(&endpoint, window, &sock_read, &sock_written).await;
    let mut client =
        CalamineServiceClient::new(channel).max_decoding_message_size(32 * 1024 * 1024);
    if let Some(encoding) = compression {
        client = client.accept_compressed(encoding);
    }

    // ---- upload leg, reported on its own ---------------------------------
    let frames: Vec<pb::OpenWorkbookRequest> = std::iter::once(pb::OpenWorkbookRequest {
        payload: Some(pb::open_workbook_request::Payload::Options(
            pb::WorkbookOptions {
                format_hint: pb::WorkbookFormat::Unspecified as i32,
                header_row: None,
            },
        )),
    })
    .chain(raw.chunks(CHUNK).map(|c| pb::OpenWorkbookRequest {
        payload: Some(pb::open_workbook_request::Payload::Chunk(c.to_vec())),
    }))
    .collect();

    let t = Instant::now();
    let opened = client
        .open_workbook(tokio_stream::iter(frames))
        .await
        .expect("open workbook")
        .into_inner();
    let upload_ms = t.elapsed().as_secs_f64() * 1e3;
    let upload_socket = sock_written.load(Ordering::Relaxed);
    let workbook_id = opened.workbook_id;

    // ---- interleaved measurement -----------------------------------------
    let (mut a0, mut a1, mut a2, mut a3, mut ttfrs) = (vec![], vec![], vec![], vec![], vec![]);
    let mut digests: Vec<(&str, Digest)> = Vec::new();
    let mut wire_bytes = 0u64;
    let mut socket_bytes = 0u64;
    let mut grpc_messages = 0u64;
    let mut table_entries = 0u64;
    let batch: u32 = std::env::var("BATCH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut client_cpu = 0.0;
    let mut server_cpu = 0.0;
    let self_pid = std::process::id();

    let (mut cpu0, mut cpu1, mut cpu2) = (0.0, 0.0, 0.0);

    for i in 0..iterations {
        let c = cpu_seconds(self_pid);
        let (ms, d0) = arm_native_sparse(&bytes, &sheet);
        cpu0 += cpu_seconds(self_pid) - c;
        a0.push(ms);

        let c = cpu_seconds(self_pid);
        let (ms, d1) = arm_native_dense(&bytes, &sheet);
        cpu1 += cpu_seconds(self_pid) - c;
        a1.push(ms);

        let c = cpu_seconds(self_pid);
        let (ms, d2, wire) = arm_native_encode(&bytes, &sheet, is_1904);
        cpu2 += cpu_seconds(self_pid) - c;
        a2.push(ms);
        wire_bytes = wire;

        let c0 = cpu_seconds(self_pid);
        let s0 = cpu_seconds(server_pid);
        let sock_before = sock_read.load(Ordering::Relaxed);
        let (ms, ttfr, d3, msgs, entries) =
            arm_grpc(&mut client, &workbook_id, &sheet, batch, dict).await;
        socket_bytes = sock_read.load(Ordering::Relaxed) - sock_before;
        grpc_messages = msgs;
        table_entries = entries;
        client_cpu += cpu_seconds(self_pid) - c0;
        server_cpu += cpu_seconds(server_pid) - s0;
        a3.push(ms);
        ttfrs.push(ttfr);

        if i == 0 {
            digests.push(("0 calamine sparse walk", d0));
            digests.push(("1 native dense", d1));
            digests.push(("2 native dense + protobuf encode", d2));
            digests.push(("3 gRPC end to end", d3));
        }
        print!("\r  iteration {}/{iterations}", i + 1);
        use std::io::Write;
        std::io::stdout().flush().ok();
    }
    println!("\r                        \r");

    // ---- the gate --------------------------------------------------------
    println!("same-work proof (digest of the canonical cell stream)");
    for (name, d) in &digests {
        println!("  {name:34} {d}");
    }
    let gated = &digests[1..];
    let all_equal = gated.windows(2).all(|w| w[0].1 == w[1].1);
    println!(
        "\n  arms 1-3 identical: {}",
        if all_equal { "yes" } else { "NO" }
    );
    println!(
        "  arm 0 walks only populated cells ({} vs {}), so it is not comparable and is shown as a floor.\n",
        digests[0].1.cells, digests[1].1.cells
    );
    assert!(
        all_equal,
        "arms disagree on the cell stream; refusing to publish numbers"
    );

    // ---- results ---------------------------------------------------------
    let rows = digests[1].1.rows;
    let cells = digests[1].1.cells;
    let (s0, s1, s2, s3) = (stat(a0), stat(a1), stat(a2), stat(a3.clone()));
    let ttfr = stat(ttfrs);

    let n = iterations as f64;
    let (c0, c1, c2) = (cpu0 / n, cpu1 / n, cpu2 / n);
    let c3 = (client_cpu + server_cpu) / n;

    println!("wall clock ms (min / median / p95), and CPU seconds burned per run");
    println!(
        "                                          {:>8} {:>8} {:>8}   {:>7}",
        "min", "med", "p95", "CPU s"
    );
    println!(
        "  0  calamine alone, populated cells only  {:8.1} {:8.1} {:8.1}   {c0:7.2}",
        s0.min, s0.median, s0.p95
    );
    println!(
        "  1  + dense canonical grid                {:8.1} {:8.1} {:8.1}   {c1:7.2}",
        s1.min, s1.median, s1.p95
    );
    println!(
        "  2  + protobuf convert and encode         {:8.1} {:8.1} {:8.1}   {c2:7.2}",
        s2.min, s2.median, s2.p95
    );
    println!(
        "  3  + gRPC socket, decoded by the client  {:8.1} {:8.1} {:8.1}   {c3:7.2}",
        s3.min, s3.median, s3.p95
    );

    println!("\nwhere the in-process time goes (arms 0-2 are one serial thread)");
    println!(
        "  calamine parse, the floor                {:8.1} ms  {:5.1}%",
        s0.median,
        s0.median / s2.median * 100.0
    );
    println!(
        "  densify into the contract's grid         {:8.1} ms  {:5.1}%",
        s1.median - s0.median,
        (s1.median - s0.median) / s2.median * 100.0
    );
    println!(
        "  protobuf convert + encode                {:8.1} ms  {:5.1}%",
        s2.median - s1.median,
        (s2.median - s1.median) / s2.median * 100.0
    );
    println!(
        "  total the server must do per read        {:8.1} ms",
        s2.median
    );

    println!("\nwhat the gRPC surface actually costs");
    println!(
        "  same work, one thread, in process (2)    {:8.1} ms wall   {c2:5.2} s CPU",
        s2.median
    );
    println!(
        "  same work, over the wire (3)             {:8.1} ms wall   {c3:5.2} s CPU",
        s3.median
    );
    println!(
        "  wall clock                               {:+8.1} ms  ({:+.1}%)",
        s3.median - s2.median,
        (s3.median / s2.median - 1.0) * 100.0
    );
    println!(
        "  CPU                                      {:+8.2} s   ({:.2}x)",
        c3 - c2,
        c3 / c2.max(f64::MIN_POSITIVE)
    );
    println!(
        "  parallelism used by arm 3                {:8.2} cores (CPU / wall)",
        c3 / (s3.median / 1e3)
    );
    println!("\n  Read the two columns together. gRPC finishes sooner in wall clock because");
    println!("  the server parses while the client decodes, on different cores; it is not");
    println!("  doing less work. The honest cost of the surface is the CPU column and the");
    println!("  bytes below, not latency.");
    println!("\n  versus plain calamine in your own process (arm 0):");
    println!(
        "    wall {:+.1} ms ({:.2}x), CPU {:+.2} s ({:.2}x)",
        s3.median - s0.median,
        s3.median / s0.median,
        c3 - c0,
        c3 / c0.max(f64::MIN_POSITIVE)
    );
    println!(
        "  server peak RSS                          {:8.1} MiB",
        peak_rss_mib(server_pid)
    );

    println!("\nshape and wire");
    println!("  rows                                     {rows}");
    println!("  cells (dense)                            {cells}");
    println!(
        "  protobuf payload (arm 2 encode)          {:8.1} MB",
        wire_bytes as f64 / 1e6
    );
    println!(
        "  bytes on the socket (arm 3)              {:8.1} MB  (compression: {compression_label}, dict: {})",
        socket_bytes as f64 / 1e6,
        if dict { "on" } else { "off" }
    );
    if dict {
        println!("  string table                             {table_entries} entries, sent once in-stream");
    }
    println!(
        "  expansion over the source file           {:8.2}x  (socket / source)",
        socket_bytes as f64 / raw.len() as f64
    );
    println!(
        "  messages on the stream                   {grpc_messages}  ({:.1} rows each)",
        rows as f64 / grpc_messages.max(1) as f64
    );
    println!(
        "  throughput (arm 3, median)               {:8.0} rows/s, {:.0} cells/s",
        rows as f64 / (s3.median / 1e3),
        cells as f64 / (s3.median / 1e3)
    );

    println!("\nlatency to the first row");
    println!(
        "  gRPC stream (min / median / p95)         {:8.1} {:8.1} {:8.1} ms",
        ttfr.min, ttfr.median, ttfr.p95
    );
    println!(
        "  a batch API cannot answer before         {:8.1} ms  (arm 2 completes)",
        s2.median
    );

    println!("\nupload leg, counted separately");
    println!(
        "  OpenWorkbook for {:.1} MB               {upload_ms:8.1} ms  ({:.0} MB/s)",
        raw.len() as f64 / 1e6,
        raw.len() as f64 / 1e6 / (upload_ms / 1e3)
    );
    println!(
        "  bytes written on the socket              {:8.1} MB",
        upload_socket as f64 / 1e6
    );
    println!("  paid once per workbook, then any number of reads reuse the handle.");

    if let Some(mut child) = server {
        let _ = child.kill();
        let _ = child.wait();
    }
}
