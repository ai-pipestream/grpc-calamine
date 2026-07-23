# node-client

Two demos on one small library (`lib/calamine.js`, which loads the protos
dynamically from `../../proto` via `@grpc/proto-loader` — no codegen).

## Web viewer

```bash
npm install
npm start          # http://127.0.0.1:8080  (PORT=8081 npm start to change)
```

Drop any workbook onto the page (or click to choose one). The bridge
(`server.js`, dependency-free Node `http`) streams the upload straight into
the gRPC call — the file is never buffered whole in the bridge — then
forwards every response event to the browser as Server-Sent Events. The
table fills in row by row exactly as the Rust server parses the sheet, with
a live progress bar fed by the `RangeStarted` header's `total_cells`.

What each contract feature looks like in the UI:

- **Sheet tabs** come from the workbook metadata; hidden and very-hidden
  sheets are labeled.
- **Cell types** are colored — numbers, dates (rendered from the Excel
  serial + the workbook's 1904 flag), booleans, and `#DIV/0!`-style errors.
- **"Show formulas"** re-streams the same sheet through
  `StreamWorksheetFormula`. A sheet with no formulas shows an explicit
  empty-state note rather than a blank grid.
- **In-band `StreamError` events** surface as a banner without killing the
  stream.

To actually see it stream (rather than finish instantly), use a large
workbook — see [../README.md](../README.md#see-the-streaming-not-just-the-result).

## CLI

```bash
node cli.js ../sample-data/date.xlsx            # first sheet
node cli.js ../sample-data/errors.xlsx Feuil1   # by name
CALAMINE_ADDR=host:port node cli.js file.xlsb 2 # by index, remote server
```

The CLI also streams its upload from disk (`createReadStream`), so it never
holds the workbook whole in the client process either.
