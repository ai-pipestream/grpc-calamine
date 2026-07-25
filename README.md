# grpc-calamine

A gRPC server around the Rust [calamine](https://github.com/tafia/calamine)
crate. Upload a workbook once, then stream its cells, formulas, VBA modules,
and embedded pictures from any language with a gRPC client. Rows go out
while the sheet is still being parsed, uploaded bytes stay in memory, and
nothing is written to disk.

Formats: `.xls`/`.xla`, `.xlsx`/`.xlsm`/`.xlam`, `.xlsb`, `.ods`.

## Speed

Numbers from a 105.7 MB, 985,351-row `.xlsx` (7.9 million cells) on a Ryzen
9 9950X3D, captured 2026-07-24. Full runs and methodology live in
[bench/RESULTS.md](bench/RESULTS.md) and [bench/](bench).

|                                             | wall clock |
|---------------------------------------------|------------|
| calamine as a library, in your own process  | 2.10 s     |
| the same sheet over grpc-calamine, loopback | 1.86 s     |
| time to the first row                       | 1.5 ms     |
| python-calamine, in process                 | 5.5 s      |
| the same rows into Python over gRPC         | 4.8 s      |

The stream finishes before the in-process library call because the server
parses while the client decodes, on different cores. What it costs is CPU
(about 1.6x the single-threaded work) and wire size: protobuf re-expands
the strings that XLSX stores once, 789 MB for this file. Both fixes are
built in, and both are measured rather than assumed:

| row stream         | on the socket | vs the 105.7 MB file |
|--------------------|---------------|----------------------|
| plain              | 789 MB        | 7.5x                 |
| zstd compression   | 290 MB        | 2.7x                 |
| `use_string_table` | 173 MB        | 1.6x                 |
| both               | 62 MB         | 0.6x                 |

`use_string_table` is dictionary encoding. Each distinct string crosses the
wire once and cells carry u32 ids, which also costs less CPU than the plain
stream since neither side copies string bodies per cell. Every mode feeds
the same digest check, so none of them can win by dropping data.

Your workbook and your hardware will move these numbers. The bench reruns
everything with one command.

## How it works

- **Nothing touches disk.** Uploaded bytes live in memory (`Arc<[u8]>`) and
  calamine parses them through its `open_workbook_*_from_rs` readers.
- **Rows stream during the parse.** XLSX and XLSB use calamine's incremental
  cell readers. XLS and ODS only support whole-range parsing, so those parse
  first and then stream. The wire contract is identical either way, and the
  streamed grid matches calamine's own `worksheet_range`: trailing blank
  rows trimmed, interior gaps sent as explicit empty rows. The test suite
  asserts that parity against calamine for every sheet of every fixture.
- **Reads don't block each other.** Each read builds its own calamine reader
  over the shared bytes, so many clients can stream one workbook at once.
  Parsing runs on tokio's blocking pool behind a bounded channel, so a slow
  client slows its own parse instead of filling server memory.
- **The protobuf model mirrors calamine's types one to one** (`Data`,
  `Range`, `Cell`, `ExcelDateTime`, `CellErrorType`, ...). Conversions in
  `src/convert.rs` are total; nothing is guessed or flattened.

## Run

```bash
cargo run --release
# grpc-calamine listening on 0.0.0.0:50051
```

Use `--release`; a debug build parses more than an order of magnitude
slower.

| Variable                         | Default        | Meaning                                     |
|----------------------------------|----------------|---------------------------------------------|
| `GRPC_CALAMINE_ADDR`             | `0.0.0.0:50051`| Listen address                              |
| `GRPC_CALAMINE_WORKERS`          | CPU count      | tokio worker threads                        |
| `GRPC_CALAMINE_BLOCKING_THREADS` | `512`          | max threads for calamine parsing tasks      |
| `GRPC_CALAMINE_WINDOW_BYTES`     | `52428800`     | HTTP/2 initial stream and connection window |

The window default is 50 MiB because window size over round-trip time caps
upload throughput; hyper's 1 MiB default holds a 10 ms link near 100 MB/s.
It governs the upload only. A client that wants a wide window for the row
stream sets its own.

The server accepts gzip- and zstd-compressed requests and compresses
responses for any client that negotiates it. No configuration needed on
either side beyond the client asking.

Hard limits (compile-time, `src/service.rs`): 512 MiB max workbook upload,
32 MiB max gRPC frame, 64-event stream backpressure channel.

## API

Handle-based: upload once, then run any number of concurrent reads against
the returned `workbook_id`.

- `OpenWorkbook` — client-streaming upload: one options frame, then file
  bytes. Returns `workbook_id`, the detected format, and full metadata.
- `StreamWorksheetRange` — a `RangeStarted` header, then dense rows anchored
  at column A, so a value's index is its absolute column. Rows arrive
  batched (`WorksheetRowBatch`, up to 256 rows, 5 ms linger); set
  `max_rows_per_message = 1` for one row per message. Set
  `use_string_table = true` for dictionary encoding: `string_table` events
  define each distinct string once, cells carry `shared_string_id`, and
  every id is defined before the first row that references it. XLSX/XLSB
  only; other formats accept the flag unchanged.
- `StreamWorksheetFormula` — same shape, formula strings instead of values.
- `StreamVbaProject` — project info, then one event per module (raw MBCS
  bytes; decoding is the client's choice, matching calamine).
- `GetPictures` — one event per embedded image.
- `GetMetadata`, `GetDefinedNames`, `CloseWorkbook`.

Terminal failures use gRPC status codes (`NOT_FOUND`, `INVALID_ARGUMENT`,
`RESOURCE_EXHAUSTED`). Recoverable per-item failures arrive as in-band
`StreamError` events with a typed `CalamineError`, so one bad sheet doesn't
kill the stream.

### Client sketch (Rust)

```rust
// 1. Upload: first frame = options, then file bytes in <= 1 MiB chunks.
let opened = client.open_workbook(frames).await?.into_inner();

// 2. Stream a sheet by name or index.
let mut rows = client
    .stream_worksheet_range(StreamWorksheetRangeRequest {
        workbook_id: opened.workbook_id.clone(),
        sheet: Some(SheetSelector {
            selector: Some(sheet_selector::Selector::SheetName("Sheet1".into())),
        }),
        max_rows_per_message: 0, // 0 = server default (batched)
        use_string_table: false,
    })
    .await?
    .into_inner();
while let Some(event) = rows.message().await? {
    // RangeStarted | WorksheetRowBatch | WorksheetRow | StringTableChunk | StreamError
}

// 3. Release the handle.
client.close_workbook(CloseWorkbookRequest {
    workbook_id: opened.workbook_id,
}).await?;
```

## Building from source

Rust stable, the [buf](https://buf.build/docs/installation) CLI, and the
codegen plugins (`cargo install protoc-gen-prost protoc-gen-tonic`). Stock
calamine 0.36 from crates.io; no fork, no patches.

```bash
buf lint && buf generate   # after editing anything under proto/
cargo build
cargo test                 # unit + end-to-end streaming tests
```

```
proto/calamine/v1/     protobuf contract (source of truth)
src/
  convert.rs           calamine <-> protobuf conversions
  store.rs             in-memory workbook store
  service.rs           the CalamineService implementation
  gen/                 generated by `buf generate`, never edited by hand
tests/streaming.rs     end-to-end tests against real workbook fixtures
bench/                 the measurement harness behind the numbers above
demos/                 Java, Python, and Node clients, plus a web viewer
```

## Tests

`cargo test` starts a real tonic server on an ephemeral port and streams
real workbook files. Every streamed cell is compared against calamine's own
`worksheet_range` output, across every sheet of every fixture, including
synthetic workbooks whose declared `<dimension>` lies (inflated,
under-declared, shifted). Dictionary mode, compression, batching modes,
concurrency, in-band errors, VBA, formulas, and datetime fidelity all have
dedicated tests.

## Demos

Self-contained clients in Java, Python, and Node live under
[`demos/`](demos), including a web viewer that renders sheets in the browser
as the server parses them. See [demos/README.md](demos/README.md).

## License

Apache-2.0. See [LICENSE](LICENSE).
