#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
//
// Web demo: a dependency-light HTTP bridge in front of grpc-calamine.
//
// The browser uploads a workbook, then subscribes to a Server-Sent Events
// stream; every gRPC event (header, row, in-band error) is forwarded to the
// page the moment the Rust server emits it, so the sheet renders while it is
// being parsed.
//
//   node server.js            # http://127.0.0.1:8080
//
// Environment: CALAMINE_ADDR (default 127.0.0.1:50051), PORT (default 8080).

import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { CalamineClient, renderCell } from "./lib/calamine.js";

const PORT = Number(process.env.PORT ?? 8080);
const client = new CalamineClient(process.env.CALAMINE_ADDR ?? "127.0.0.1:50051");
const publicDir = path.join(path.dirname(fileURLToPath(import.meta.url)), "public");

function sendJson(res, status, body) {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(body));
}

/**
 * Rows coalesced into a single socket write.
 *
 * One `res.write` per row is what actually throttles this bridge: on a 1M-row
 * sheet it caps the SSE stream near 139 MB/s, while batching lifts it to about
 * 246 MB/s. The gain saturates by 16 rows, so 32 leaves headroom without
 * making the grid visibly chunky as it fills.
 */
const SSE_BATCH_ROWS = 32;

/** Write the SSE response head. */
function startSse(res) {
  res.writeHead(200, {
    "content-type": "text/event-stream",
    "cache-control": "no-cache",
    connection: "keep-alive",
  });
}

/** Format one SSE event frame. */
function frame(event, data) {
  return `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`;
}

/** Pipe a range/formula gRPC stream into an SSE response. */
function pipeStream(stream, res, mapRow) {
  startSse(res);
  let pending = [];
  let sinceFlush = 0;

  // Returns false when the socket has taken all it wants for now.
  const flush = () => {
    if (pending.length === 0) return true;
    const chunk = pending.join("");
    pending = [];
    sinceFlush = 0;
    return res.write(chunk);
  };

  // Queue one row and flush on the batch boundary, honouring backpressure.
  const pushRow = (row) => {
    pending.push(frame("row", mapRow(row)));
    if (++sinceFlush >= SSE_BATCH_ROWS && !flush()) {
      // Backpressure: stop pulling rows from the server until the browser has
      // drained, instead of queueing the whole sheet in memory here.
      stream.pause();
      res.once("drain", () => stream.resume());
    }
  };

  stream.on("data", (message) => {
    switch (message.event) {
      case "started":
        pending.push(frame("started", message.started));
        flush();
        break;
      // The server batches rows by default and sends `row` only when the
      // client asks for `maxRowsPerMessage: 1`. Both carriers are handled so
      // this bridge cannot silently drop a sheet.
      case "row":
        pushRow(message.row);
        break;
      case "rows":
        message.rows.rows.forEach(pushRow);
        break;
      // A run of rows holding no values, forwarded as its own SSE event so the
      // page can draw one elided line instead of a million blank ones. It is
      // queued through `pending` like a row so it cannot overtake the rows
      // around it, but it does not count toward the flush batch, because it is
      // one small frame however many rows it stands for.
      case "rowGap":
        pending.push(frame("row-gap", message.rowGap));
        break;
      case "error":
        pending.push(frame("calamine-error", message.error));
        flush();
        break;
    }
  });
  stream.on("end", () => {
    pending.push(frame("done", {}));
    flush();
    res.end();
  });
  stream.on("error", (err) => {
    pending.push(frame("grpc-error", { message: err.message }));
    flush();
    res.end();
  });
  res.on("close", () => stream.cancel());
}

const server = createServer(async (req, res) => {
  const url = new URL(req.url, `http://${req.headers.host}`);
  try {
    // POST /api/workbooks — body is the raw workbook bytes, piped straight
    // into the gRPC upload as it arrives (never buffered in the bridge).
    if (req.method === "POST" && url.pathname === "/api/workbooks") {
      const opened = await client.openWorkbookStream(req);
      return sendJson(res, 200, opened);
    }

    // DELETE /api/workbooks/:id
    let m = url.pathname.match(/^\/api\/workbooks\/([^/]+)$/);
    if (req.method === "DELETE" && m) {
      return sendJson(res, 200, await client.closeWorkbook(m[1]));
    }

    // GET /api/workbooks/:id/sheets/:index/rows — SSE of parsed rows.
    m = url.pathname.match(/^\/api\/workbooks\/([^/]+)\/sheets\/(\d+)\/rows$/);
    if (req.method === "GET" && m) {
      const stream = client.streamWorksheetRange(m[1], { sheetIndex: Number(m[2]) });
      return pipeStream(stream, res, (row) => ({
        rowIndex: row.rowIndex,
        cells: row.values.map(renderCell),
      }));
    }

    // GET /api/workbooks/:id/sheets/:index/formulas — SSE of formula rows.
    m = url.pathname.match(/^\/api\/workbooks\/([^/]+)\/sheets\/(\d+)\/formulas$/);
    if (req.method === "GET" && m) {
      const stream = client.streamWorksheetFormula(m[1], { sheetIndex: Number(m[2]) });
      return pipeStream(stream, res, (row) => ({
        rowIndex: row.rowIndex,
        cells: row.formulas.map((f) => ({ text: f, type: f ? "formula" : "empty" })),
      }));
    }

    // Static front end. no-store: this is a live demo page, never let the
    // browser run a stale copy of it.
    if (req.method === "GET" && (url.pathname === "/" || url.pathname === "/index.html")) {
      const html = await readFile(path.join(publicDir, "index.html"));
      res.writeHead(200, {
        "content-type": "text/html; charset=utf-8",
        "cache-control": "no-store",
      });
      return res.end(html);
    }
    if (req.method === "GET" && url.pathname === "/favicon.ico") {
      res.writeHead(204);
      return res.end();
    }

    sendJson(res, 404, { error: "not found" });
  } catch (err) {
    // gRPC errors carry .code/.details; surface them as JSON.
    sendJson(res, 502, { error: err.details ?? err.message });
  }
});

server.on("error", (err) => {
  if (err.code === "EADDRINUSE") {
    console.error(`port ${PORT} is already in use — another bridge instance?`);
    console.error(`stop it, or run with a different port: PORT=8081 npm start`);
    process.exit(1);
  }
  throw err;
});

server.listen(PORT, () => {
  console.log(`calamine web demo on http://127.0.0.1:${PORT}`);
  console.log(`forwarding to grpc-calamine at ${process.env.CALAMINE_ADDR ?? "127.0.0.1:50051"}`);
});
