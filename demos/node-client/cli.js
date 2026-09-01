#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
//
// CLI demo: upload a workbook to grpc-calamine and stream a worksheet to
// the terminal as rows arrive.
//
//   node cli.js <workbook-file> [sheet-name-or-index]
//
// Environment: CALAMINE_ADDR overrides the server address (default
// 127.0.0.1:50062).

import { createReadStream } from "node:fs";
import { CalamineClient, renderCell, columnLabel } from "./lib/calamine.js";

const [file, sheetArg] = process.argv.slice(2);
if (!file) {
  console.error("usage: node cli.js <workbook-file> [sheet-name-or-index]");
  process.exit(2);
}

const sheet = sheetArg === undefined
  ? { sheetIndex: 0 }
  : /^\d+$/.test(sheetArg)
    ? { sheetIndex: Number(sheetArg) }
    : { sheetName: sheetArg };

const client = new CalamineClient(process.env.CALAMINE_ADDR ?? "127.0.0.1:50062");

// Piped from disk chunk by chunk; the file is never whole in this process.
const opened = await client.openWorkbookStream(createReadStream(file));
console.log(`opened ${file} as ${opened.detectedFormat} — handle ${opened.workbookId}`);
console.log("sheets:", opened.metadata.sheets.map((s) => s.name).join(", "));
if (opened.metadata.definedNames.length > 0) {
  console.log("defined names:", opened.metadata.definedNames.map((n) => `${n.name}=${n.definition}`).join(", "));
}

const started = process.hrtime.bigint();
let rowCount = 0;

/** Print one streamed row, whichever carrier delivered it. */
function printRow(row) {
  rowCount += 1;
  const cells = row.values.map((cell) => renderCell(cell).text || "·");
  console.log(`${String(row.rowIndex + 1).padStart(6)} │ ${cells.join(" │ ")}`);
}

const stream = client.streamWorksheetRange(opened.workbookId, sheet);
stream.on("data", (message) => {
  switch (message.event) {
    case "started": {
      const { sheetName, dimensions, totalCells } = message.started;
      const where = dimensions
        ? `${columnLabel(dimensions.start.col)}${dimensions.start.row + 1}:` +
          `${columnLabel(dimensions.end.col)}${dimensions.end.row + 1}`
        : "(empty)";
      console.log(`\nstreaming "${sheetName}" ${where} — ${totalCells} cells\n`);
      break;
    }
    // Rows arrive batched by default and one at a time only when the client
    // asks for `maxRowsPerMessage: 1`, so both carriers are handled.
    case "row":
      printRow(message.row);
      break;
    case "rows":
      message.rows.rows.forEach(printRow);
      break;
    case "error":
      console.error("in-band error:", message.error.error?.kind, message.error.error?.message);
      if (message.error.terminal) process.exitCode = 1;
      break;
  }
});

stream.on("error", (err) => {
  console.error("stream failed:", err.message);
  process.exitCode = 1;
});

stream.on("end", async () => {
  const elapsedMs = Number(process.hrtime.bigint() - started) / 1e6;
  console.log(`\n${rowCount} rows in ${elapsedMs.toFixed(1)} ms`);
  await client.closeWorkbook(opened.workbookId);
  client.close();
});
