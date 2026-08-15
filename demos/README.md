# grpc-calamine demos

**These are examples, not a client library.** Nothing here is packaged,
versioned, or published to npm, PyPI, or Maven Central, and nothing depends
on it. Each demo is a short readable file meant to be *copied from*: the
deliverable is the contract in [`../proto`](../proto), and these show what
talking to it looks like in three languages. Every one generates (or
dynamically loads) its stubs from the repository's proto files at build/run
time, so no generated client code is checked in.

Each client README ends with a from-scratch tutorial for that language:
the dependency, the stub generation, and a ~40 line program that uploads a
workbook and prints its rows.

Start the server first, from the repository root:

```bash
cargo run --release
# grpc-calamine listening on 0.0.0.0:50051
```

| Demo | Stack | What it shows |
|---|---|---|
| [`node-client`](node-client) | Node 20+, `@grpc/grpc-js` | CLI streamer **and a live web viewer** that renders sheets in the browser as the server parses them (gRPC → SSE → DOM) |
| [`python-client`](python-client) | Python 3.11+, `grpcio` | CLI streamer with formula and VBA-project streaming |
| [`java-client`](java-client) | Java 17+, Maven, `grpc-java` | Client-streaming upload + blocking server-stream consumption |

## Quick start

```bash
# Node web viewer (the visual one), then open http://127.0.0.1:8080
cd node-client && npm install && npm start

# Node CLI
cd node-client && node cli.js ../sample-data/date.xlsx

# Python
cd python-client && ./run.sh ../sample-data/vba.xlsm --vba --formulas

# Java
cd java-client && mvn -q compile exec:java -Dexec.args="../sample-data/date.xlsx"
```

All demos accept a sheet name or zero-based index as the second argument
and honor `CALAMINE_ADDR` (default `127.0.0.1:50051`). All three print the
same rows for the same file; that agreement is the point.

## The one thing every client gets wrong first

Rows arrive **batched**. The default `StreamWorksheetRange` carrier is
`rows` (a `WorksheetRowBatch`, up to 256 rows), not `row`. A client that
switches on the response `oneof` and handles only `row` connects, gets its
`RangeStarted` header, prints nothing, and exits 0, which looks like an
empty sheet rather than a bug. Handle both:

```
started      -> the header, once
rows         -> batch.rows, the default carrier   <-- do not skip this one
row          -> a single row, only if you asked for max_rows_per_message = 1
row_gap      -> a run of rows holding nothing
string_table -> only in use_string_table mode
error        -> in-band StreamError
```

Set `max_rows_per_message = 1` if you would rather have the simpler
single-row carrier and can afford one message per row.

## Empty rows do not arrive as rows

Only rows holding at least one value arrive as a row. A run of empty ones
arrives as a single `row_gap`, a first index and a count, however long the
run is.

This exists because a sheet's populated cells can sit arbitrarily far apart.
`sample-data/corners.xlsx` is 2,341 bytes and holds two cells, one at `A1`
and one at `XFD1048576`. The rows between them are 1,048,574 of nothing, and
spelling them out costs a message and a client-side object each: 17.2 billion
cells, which is what OOM-killed the Node viewer before this event existed.
One gap says the same thing in constant space. Try it:

```bash
node cli.js ../sample-data/corners.xlsx
```

```
     1 │ 1
     ⋮ │ (1,048,574 empty rows, 2-1,048,575)
1048576 │ · │ · │ ...
```

Handling it is a one-liner in whichever direction you need:

- **Building a dense grid?** Expand it into `row_count` blank rows.
- **Collecting populated cells?** Skip it. It covers none by definition.
- **Only care about values?** Ignore the event entirely. `row_index` is
  absolute, so every populated row still lands where it belongs whether or
  not you ever look at a gap.

The one thing you cannot do is treat a gap as data loss. Nothing is lost:
a gap covers no cells, which is exactly why it can be a gap.

## The API in one workflow

The service is **handle-based**: you upload a workbook once and then issue
any number of concurrent reads against the returned `workbook_id`. Every
demo follows the same five steps; the file:line references point at where
each one is implemented.

1. **Open (client-streaming upload).** Send an `OpenWorkbook` stream whose
   **first** frame is a `WorkbookOptions` (format hint + optional header
   row) and whose remaining frames are file `chunk`s in order. The server
   replies once with `{ workbook_id, detected_format, metadata }`.
   → `node-client/lib/calamine.js` `openWorkbookStream`,
   `python-client/client.py` `upload_frames`,
   `java-client/…/CalamineDemo.java` `openWorkbook`.

2. **Inspect metadata.** The open response already carries `Metadata`
   (every `Sheet` with its name / type / visibility, plus defined names).
   `GetMetadata` and `GetDefinedNames` return the same snapshot later.

3. **Stream a worksheet (server-streaming).** Call `StreamWorksheetRange`
   with the handle and a `SheetSelector` (`sheet_name` **or**
   `sheet_index`). The first event is always a `RangeStarted` header
   (resolved sheet name, dimensions, total cell count); then one
   `WorksheetRow` per row arrives as it is parsed. `StreamWorksheetFormula`
   has the identical shape but carries formula strings.

4. **Handle errors two ways.** Unknown handle or bad request → the RPC
   fails with a gRPC status (`NOT_FOUND`, `INVALID_ARGUMENT`,
   `RESOURCE_EXHAUSTED`). A recoverable per-item failure (e.g. one bad
   sheet) arrives **in-band** as a `StreamError` event carrying a typed
   `CalamineError`, so the rest of the stream survives.

5. **Close.** `CloseWorkbook` releases the handle (safe to call on an
   unknown id). Handles otherwise live until the server stops.

Other reads follow the same streaming shape: `StreamVbaProject`
(`VbaProjectInfo` header, then one `VbaModule` per module) and
`GetPictures` (one `Picture` per embedded image).

The one value worth understanding is `CellData`: a `oneof` mirroring
calamine's `Data`/`DataRef` exactly: `int`, `float`, `string`,
`shared_string`, `bool`, `date_time` (an Excel serial + the workbook's
1904 flag), `date_time_iso`, `duration_iso`, `error` (a typed enum), and an
explicit `empty`. Each demo has a `renderCell` / `render_cell` function
that turns one `CellData` into display text. That is the whole client-side
mapping you need.

The contract itself is the source of truth: see
[`../proto/calamine/v1`](../proto/calamine/v1), whose comments document
every field.

## See the streaming, not just the result

The point of this service is that rows are emitted **as the parser walks the
sheet**, never buffered whole. The small fixtures here finish in about a
millisecond, so to actually watch it stream, feed it a big workbook.

A good one is the ~100 MB sample (≈1M rows) from
<https://examplefile.com/document/xlsx/100-mb-xlsx>. Download it from that
page in your browser (the site gates direct `curl`), then:

- **Web viewer**: drop the file onto the page and watch rows fill the grid
  live while the progress bar advances. The bridge forwards each row the
  instant the Rust server parses it and never holds the whole file (see
  `openWorkbookStream` in `node-client/lib/calamine.js`).
- **CLI**: `node cli.js path/to/100mb.xlsx | head` prints the first rows
  before the sheet has finished parsing.

Nothing in the pipeline touches disk: the browser upload streams straight
into the gRPC call, and the server keeps only one shared `Arc<[u8]>` of the
bytes in memory for as long as the handle is open.

## Sample data

[`sample-data/`](sample-data) holds small workbooks originally from the
[calamine](https://github.com/tafia/calamine) test suite (MIT licensed),
each chosen to exercise a specific corner of the contract:

| File | Shows off |
|---|---|
| `date.xlsx`, `date.xlsb`, `date.xls`, `date.ods` | the same dates across all four formats (1900 epoch) |
| `date_1904.xlsx` | the workbook-level 1904 date system, carried per cell |
| `errors.xlsx` | typed error cells (`#DIV/0!`, `#N/A`, …) |
| `any_sheets.xlsx` | hidden / very-hidden sheets and a chart sheet |
| `formula.issue.xlsx` | formula streaming (`--formulas` / the "show formulas" toggle) |
| `vba.xlsm` | a VBA project with modules (`--vba`) |
| `temperature.xlsx` | a plain numeric sheet, and the one that omits `<dimension>` entirely |

Alongside them, `dimension_*.xlsx` are synthetic: minimal workbooks whose
declared `<dimension>` deliberately lies (inflated, under-declared, shifted,
offset, reversed). `<dimension>` is optional in ECMA-376 and real writers get
it wrong, so these pin the cases that have actually bitten this server. They
are generated by [`make_synthetic_fixtures.py`](sample-data/make_synthetic_fixtures.py),
which explains each one.

The Rust integration tests stream all of these and compare every cell
against calamine's own output.

## Also in here, but not demos

Two files under these directories belong to the benchmark harness in
[`../bench`](../bench), not to the examples. They are measurement tools:
noisier, less readable, and not something to copy from.

| File | What it is |
|---|---|
| `java-client/src/main/java/demo/Timing.java` | Measures grpc-java client dispatch (blocking vs async stub, executor, HTTP/2 window, batch size). Lives here because it needs this project's generated stubs. |
| `python-client/pybench.py` | Compares openpyxl and python-calamine on a local file against grpc-calamine over the wire and against the same rows as NDJSON. |

Both are documented in [`../bench/README.md`](../bench/README.md) and run
from there. If you are learning the API, read `CalamineDemo.java` and
`client.py` instead.
