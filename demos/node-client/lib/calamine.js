// SPDX-License-Identifier: Apache-2.0
//
// Thin promise/async wrapper around the calamine.v1 gRPC contract.
//
// The protos are loaded dynamically from ../../proto (the single source of
// truth in this repository) — no generated code is checked in.

import { fileURLToPath } from "node:url";
import path from "node:path";
import grpc from "@grpc/grpc-js";
import protoLoader from "@grpc/proto-loader";

const PROTO_ROOT = path.join(
  path.dirname(fileURLToPath(import.meta.url)),
  "..", "..", "..", "proto",
);

const packageDefinition = protoLoader.loadSync(
  path.join(PROTO_ROOT, "calamine", "v1", "calamine_service.proto"),
  {
    includeDirs: [PROTO_ROOT],
    keepCase: false,
    longs: Number,
    enums: String,
    defaults: true,
    oneofs: true,
  },
);

const { calamine } = grpc.loadPackageDefinition(packageDefinition);

/** One-MiB upload chunks: far below the server's 32 MiB frame limit. */
const CHUNK_BYTES = 1024 * 1024;

/** A connected calamine client with promise-friendly helpers. */
export class CalamineClient {
  /** @param {string} address host:port of the grpc-calamine server. */
  constructor(address = "127.0.0.1:50051") {
    this.stub = new calamine.v1.CalamineService(
      address,
      grpc.credentials.createInsecure(),
      { "grpc.max_receive_message_length": 32 * 1024 * 1024 },
    );
  }

  /**
   * Upload workbook bytes (options frame first, then 1 MiB chunks) and
   * resolve with the OpenWorkbookResponse.
   *
   * @param {Buffer} bytes complete workbook file content.
   * @param {object} [options] WorkbookOptions overrides (formatHint, headerRow).
   */
  openWorkbook(bytes, options = {}) {
    return new Promise((resolve, reject) => {
      const call = this.stub.openWorkbook((err, response) =>
        err ? reject(err) : resolve(response),
      );
      call.write({ options: { formatHint: "WORKBOOK_FORMAT_UNSPECIFIED", ...options } });
      for (let offset = 0; offset < bytes.length; offset += CHUNK_BYTES) {
        call.write({ chunk: bytes.subarray(offset, offset + CHUNK_BYTES) });
      }
      call.end();
    });
  }

  /**
   * Upload from any Readable (file stream, HTTP request body) without ever
   * holding the whole workbook in this process: chunks are forwarded the
   * moment they arrive, and gRPC write backpressure pauses the source.
   *
   * The server still needs the complete file before parsing — xlsx/xlsb/ods
   * are zip containers and xls is a CFB file, so their indexes live at the
   * END of the byte stream — but nothing buffers on the way there.
   *
   * @param {import("node:stream").Readable} source workbook byte stream.
   * @param {object} [options] WorkbookOptions overrides.
   */
  openWorkbookStream(source, options = {}) {
    return new Promise((resolve, reject) => {
      const call = this.stub.openWorkbook((err, response) =>
        err ? reject(err) : resolve(response),
      );
      call.write({ options: { formatHint: "WORKBOOK_FORMAT_UNSPECIFIED", ...options } });
      source.on("data", (chunk) => {
        if (!call.write({ chunk })) {
          source.pause();
          call.once("drain", () => source.resume());
        }
      });
      source.on("end", () => call.end());
      source.on("error", (err) => {
        call.cancel();
        reject(err);
      });
    });
  }

  /** Release a workbook handle. Resolves with { closed }. */
  closeWorkbook(workbookId) {
    return new Promise((resolve, reject) => {
      this.stub.closeWorkbook({ workbookId }, (err, response) =>
        err ? reject(err) : resolve(response),
      );
    });
  }

  /** Fetch the metadata snapshot for an open handle. */
  getMetadata(workbookId) {
    return new Promise((resolve, reject) => {
      this.stub.getMetadata({ workbookId }, (err, response) =>
        err ? reject(err) : resolve(response),
      );
    });
  }

  /** Fetch all defined names for an open handle. */
  getDefinedNames(workbookId) {
    return new Promise((resolve, reject) => {
      this.stub.getDefinedNames({ workbookId }, (err, response) =>
        err ? reject(err) : resolve(response),
      );
    });
  }

  /**
   * Server-streaming worksheet data. Returns the raw gRPC stream; each
   * message is a StreamWorksheetRangeResponse with `event` naming the set
   * oneof field ("started" | "rows" | "row" | "stringTable" | "error").
   *
   * Rows arrive in `rows` batches by default. Pass
   * `{ maxRowsPerMessage: 1 }` for one row per message, or
   * `{ useStringTable: true }` for dictionary-encoded shared strings.
   *
   * @param {string} workbookId handle from openWorkbook.
   * @param {object} sheet SheetSelector ({ sheetIndex } or { sheetName }).
   * @param {object} [options] StreamWorksheetRangeRequest overrides.
   */
  streamWorksheetRange(workbookId, sheet, options = {}) {
    return this.stub.streamWorksheetRange({ workbookId, sheet, ...options });
  }

  /** Server-streaming worksheet formulas; same event shape as ranges. */
  streamWorksheetFormula(workbookId, sheet) {
    return this.stub.streamWorksheetFormula({ workbookId, sheet });
  }

  /** Server-streaming VBA project (info header, then modules). */
  streamVbaProject(workbookId) {
    return this.stub.streamVbaProject({ workbookId });
  }

  /** Server-streaming embedded pictures. */
  getPictures(workbookId) {
    return this.stub.getPictures({ workbookId });
  }

  /** Close the underlying channel. */
  close() {
    this.stub.close();
  }
}

/**
 * Render one CellData message to a display string.
 *
 * @param {object} cell CellData with `value` naming the set oneof field.
 * @returns {{text: string, type: string}} display text plus a coarse type
 *   tag (number | text | bool | date | error | empty) for styling.
 */
export function renderCell(cell) {
  switch (cell?.value) {
    case "intValue":
      return { text: String(cell.intValue), type: "number" };
    case "floatValue":
      return { text: String(cell.floatValue), type: "number" };
    case "stringValue":
      return { text: cell.stringValue, type: "text" };
    case "sharedStringValue":
      return { text: cell.sharedStringValue, type: "text" };
    case "boolValue":
      return { text: cell.boolValue ? "TRUE" : "FALSE", type: "bool" };
    case "dateTime":
      return { text: formatExcelDateTime(cell.dateTime), type: "date" };
    case "dateTimeIso":
      return { text: cell.dateTimeIso, type: "date" };
    case "durationIso":
      return { text: cell.durationIso, type: "date" };
    case "error":
      return { text: EXCEL_ERROR_DISPLAY[cell.error] ?? cell.error, type: "error" };
    default:
      return { text: "", type: "empty" };
  }
}

/** The exact display strings Excel uses for cell errors. */
const EXCEL_ERROR_DISPLAY = {
  CELL_ERROR_TYPE_DIV0: "#DIV/0!",
  CELL_ERROR_TYPE_NA: "#N/A",
  CELL_ERROR_TYPE_NAME: "#NAME?",
  CELL_ERROR_TYPE_NULL: "#NULL!",
  CELL_ERROR_TYPE_NUM: "#NUM!",
  CELL_ERROR_TYPE_REF: "#REF!",
  CELL_ERROR_TYPE_VALUE: "#VALUE!",
  CELL_ERROR_TYPE_GETTING_DATA: "#DATA!",
};

/**
 * Format an ExcelDateTime message as ISO-ish text.
 *
 * Excel serials count days from 1899-12-30 (1900 system, which absorbs the
 * fictitious 1900-02-29) or 1904-01-01 (1904 system); the fraction is the
 * time of day. Durations are rendered as [h]:mm:ss like Excel.
 */
export function formatExcelDateTime({ value, datetimeType, is1904 }) {
  if (datetimeType === "EXCEL_DATE_TIME_TYPE_TIME_DELTA") {
    const totalSeconds = Math.round(value * 86400);
    const h = Math.floor(totalSeconds / 3600);
    const m = String(Math.floor((totalSeconds % 3600) / 60)).padStart(2, "0");
    const s = String(totalSeconds % 60).padStart(2, "0");
    return `${h}:${m}:${s}`;
  }
  const epoch = is1904 ? Date.UTC(1904, 0, 1) : Date.UTC(1899, 11, 30);
  const date = new Date(epoch + value * 86400 * 1000);
  const iso = date.toISOString();
  // Pure dates (no time fraction) read better without the midnight suffix.
  return value % 1 === 0 ? iso.slice(0, 10) : `${iso.slice(0, 10)} ${iso.slice(11, 19)}`;
}

/** Spreadsheet-style column label: 0 -> A, 25 -> Z, 26 -> AA, ... */
export function columnLabel(index) {
  let label = "";
  for (let i = index; i >= 0; i = Math.floor(i / 26) - 1) {
    label = String.fromCharCode(65 + (i % 26)) + label;
  }
  return label;
}
