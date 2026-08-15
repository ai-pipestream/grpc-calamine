# node-client

**Examples, not a library.** Two demos over one small wrapper
(`lib/calamine.js`), which loads the protos dynamically from
[`../../proto`](../../proto) via `@grpc/proto-loader`, so there is no codegen
step and nothing generated is checked in. Copy from these; do not depend on
them.

Needs Node 20+. Start the server first, from the repository root:

```bash
cargo run --release          # listening on 0.0.0.0:50051
```

## Web viewer

```bash
npm install
npm start          # http://127.0.0.1:8080
PORT=8081 CALAMINE_ADDR=127.0.0.1:50055 npm start    # both are overridable
```

Drop any workbook onto the page (or click to choose one). The bridge
(`server.js`, dependency-free Node `http`) streams the upload straight into
the gRPC call (the file is never buffered whole in the bridge), then
forwards every response event to the browser as Server-Sent Events. The
table fills in row by row exactly as the Rust server parses the sheet, with
a live progress bar fed by the `RangeStarted` header's `total_cells`.

What each contract feature looks like in the UI:

- **Sheet tabs** come from the workbook metadata; hidden and very-hidden
  sheets are labeled.
- **Cell types** are colored: numbers, dates (rendered from the Excel
  serial + the workbook's 1904 flag), booleans, and `#DIV/0!`-style errors.
- **"Show formulas"** re-streams the same sheet through
  `StreamWorksheetFormula`. A sheet with no formulas shows an explicit
  empty-state note rather than a blank grid.
- **In-band `StreamError` events** surface as a banner without killing the
  stream.

To actually see it stream (rather than finish instantly), use a large
workbook; see [../README.md](../README.md#see-the-streaming-not-just-the-result).

The SSE bridge batches rows into frames of 32 before writing to the browser
and respects `res.write` backpressure; both matter a great deal on a
million-row sheet. See the comment at the top of `server.js`.

## CLI

```bash
node cli.js ../sample-data/date.xlsx            # first sheet
node cli.js ../sample-data/errors.xlsx Feuil1   # by name
CALAMINE_ADDR=host:port node cli.js file.xlsb 2 # by index, remote server
```

Expected output:

```
opened ../sample-data/date.xlsx as WORKBOOK_FORMAT_XLSX — handle 99ff5256-…
sheets: Sheet1

streaming "Sheet1" A1:B3 — 6 cells

     1 │ 2021-01-01 │ 15
     2 │ 2021-01-02 │ 16
     3 │ 255:10:10 │ 17

3 rows in 4.4 ms
```

The CLI streams its upload from disk (`createReadStream`), so it never holds
the workbook whole in the client process either.

## Tutorial: talk to it from your own Node project

### 1. Install and load the contract

No codegen, and no client package to install. `@grpc/proto-loader` reads the
`.proto` files at startup.

```bash
npm install @grpc/grpc-js @grpc/proto-loader
cp -r path/to/grpc-calamine/proto ./proto
```

The loader options are not cosmetic; they decide what your code looks like:

```js
import grpc from "@grpc/grpc-js";
import protoLoader from "@grpc/proto-loader";

// The filename is relative to includeDirs, not to the process cwd.
const definition = protoLoader.loadSync("calamine/v1/calamine_service.proto", {
  includeDirs: ["proto"],  // types.proto is imported by path
  keepCase: false,         // snake_case -> camelCase: row_index -> rowIndex
  longs: Number,           // uint64 total_cells as a JS number, not a Long
  enums: String,           // "WORKBOOK_FORMAT_XLSX", not 2
  defaults: true,
  oneofs: true,            // adds `event` / `value` naming the set field
});

const { calamine } = grpc.loadPackageDefinition(definition);
```

`oneofs: true` is the one you cannot skip: it is what gives each message a
discriminator field (`message.event === "rows"`) instead of making you probe
for which key is present.

### 2. Upload, stream, close

The API is handle-based: upload once, then run any number of reads against
the returned `workbookId`.

```js
import { createReadStream } from "node:fs";

const client = new calamine.v1.CalamineService(
  "127.0.0.1:50051",
  grpc.credentials.createInsecure(),
  { "grpc.max_receive_message_length": 32 * 1024 * 1024 },
);

// 1. Upload. Client-streaming: an options frame, then the file bytes.
//    Piping from a read stream never holds the workbook whole in-process.
const opened = await new Promise((resolve, reject) => {
  const call = client.openWorkbook((err, res) => (err ? reject(err) : resolve(res)));
  call.write({ options: { formatHint: "WORKBOOK_FORMAT_UNSPECIFIED" } });
  const source = createReadStream(process.argv[2]);
  source.on("data", (chunk) => {
    // Honour gRPC write backpressure or a big file will balloon in memory.
    if (!call.write({ chunk })) {
      source.pause();
      call.once("drain", () => source.resume());
    }
  });
  source.on("end", () => call.end());
  source.on("error", reject);
});

console.log("sheets:", opened.metadata.sheets.map((s) => s.name).join(", "));

// 2. Stream sheet 0. Rows arrive while the server is still parsing.
const stream = client.streamWorksheetRange({
  workbookId: opened.workbookId,
  sheet: { sheetIndex: 0 },
});

const printRow = (row) =>
  console.log(`${row.rowIndex + 1}: ` + row.values.map(render).join(" | "));

stream.on("data", (message) => {
  switch (message.event) {
    case "started":
      console.log("streaming", message.started.sheetName);
      break;
    // "rows" is the DEFAULT carrier. Handle only "row" and you will print
    // nothing at all, and exit 0.
    case "rows":
      message.rows.rows.forEach(printRow);
      break;
    case "row":
      printRow(message.row);
      break;
    case "error":
      console.error("in-band error:", message.error.error.message);
      break;
  }
});

stream.on("end", () => {
  // 3. Release the handle. Nothing else frees the server's memory.
  client.closeWorkbook({ workbookId: opened.workbookId }, () => client.close());
});

/** CellData is a oneof mirroring calamine's Data enum exactly. */
function render(cell) {
  switch (cell.value) {
    case "intValue": return String(cell.intValue);
    case "floatValue": return String(cell.floatValue);
    case "stringValue": return cell.stringValue;
    case "sharedStringValue": return cell.sharedStringValue;
    case "boolValue": return cell.boolValue ? "TRUE" : "FALSE";
    // An Excel serial plus the workbook's 1904 flag. Convert it yourself,
    // as lib/calamine.js's formatExcelDateTime does.
    case "dateTime": return `serial:${cell.dateTime.value}`;
    case "dateTimeIso": return cell.dateTimeIso;
    case "durationIso": return cell.durationIso;
    case "error": return cell.error;
    default: return "";   // empty
  }
}
```

```bash
node main.js book.xlsx
```

### Things that bite

- **`message.event` has five values**, not three: `started`, `rows`, `row`,
  `stringTable`, `error`. Missing `rows` is the failure that looks
  like success. Pass `maxRowsPerMessage: 1` on the request if you want only
  the single-row carrier.
- **Raise `grpc.max_receive_message_length`.** The default is 4 MB and a
  256-row batch of a wide sheet can exceed it. The server's own frame limit
  is 32 MiB, so match it.
- **Honour write backpressure on upload** (`call.write` returning `false`).
  Without it, a 100 MB workbook is buffered in the Node process, which is
  exactly what streaming was supposed to avoid.
- **Rows are anchored at column A.** `row.values[i]` is column `i`, absolute
  and zero-based, so no header arithmetic is needed. Empty *cells* within a
  row are always explicit.
- **In-band errors are usually not fatal.** Check `message.error.terminal`;
  a non-terminal one means the stream continues with the remaining items.
- **Close the handle.** The server holds the workbook bytes in memory until
  `closeWorkbook`; dropping the channel alone does not free them.
- **If you are bridging to a browser, batch your writes.** Forwarding one
  SSE frame per row is the single biggest cost in a bridge like `server.js`;
  frames of 32 took a million-row sheet from 8.4 s to 2.4 s there.
