# SPDX-License-Identifier: Apache-2.0
"""Python demo client for the grpc-calamine server.

Uploads a workbook (client-streaming), prints its metadata, then streams a
worksheet row by row exactly as the Rust server parses it.

Usage (via ./run.sh, which handles the venv and stub generation)::

    ./run.sh <workbook-file> [--sheet NAME_OR_INDEX] [--formulas] [--vba]
             [--addr HOST:PORT]
"""

from __future__ import annotations

import argparse
import datetime
import sys
from collections.abc import Iterator
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent / "gen"))

import grpc  # noqa: E402
from calamine.v1 import calamine_service_pb2 as svc  # noqa: E402
from calamine.v1 import calamine_service_pb2_grpc as rpc  # noqa: E402
from calamine.v1 import types_pb2 as types  # noqa: E402

CHUNK_BYTES = 1024 * 1024

# The exact display strings Excel uses for cell errors.
EXCEL_ERRORS = {
    types.CELL_ERROR_TYPE_DIV0: "#DIV/0!",
    types.CELL_ERROR_TYPE_NA: "#N/A",
    types.CELL_ERROR_TYPE_NAME: "#NAME?",
    types.CELL_ERROR_TYPE_NULL: "#NULL!",
    types.CELL_ERROR_TYPE_NUM: "#NUM!",
    types.CELL_ERROR_TYPE_REF: "#REF!",
    types.CELL_ERROR_TYPE_VALUE: "#VALUE!",
    types.CELL_ERROR_TYPE_GETTING_DATA: "#DATA!",
}


def upload_frames(path: Path) -> Iterator[svc.OpenWorkbookRequest]:
    """Yield the options frame, then the file bytes in 1 MiB chunks."""
    yield svc.OpenWorkbookRequest(
        options=svc.WorkbookOptions(format_hint=types.WORKBOOK_FORMAT_UNSPECIFIED)
    )
    with path.open("rb") as file:
        while chunk := file.read(CHUNK_BYTES):
            yield svc.OpenWorkbookRequest(chunk=chunk)


def format_excel_datetime(dt: types.ExcelDateTime) -> str:
    """Render an Excel serial datetime, honoring the workbook's epoch."""
    if dt.datetime_type == types.EXCEL_DATE_TIME_TYPE_TIME_DELTA:
        total = round(dt.value * 86400)
        return f"{total // 3600}:{total % 3600 // 60:02}:{total % 60:02}"
    epoch = (
        datetime.datetime(1904, 1, 1, tzinfo=datetime.UTC)
        if dt.is_1904
        # 1899-12-30 absorbs Excel's fictitious 1900-02-29.
        else datetime.datetime(1899, 12, 30, tzinfo=datetime.UTC)
    )
    moment = epoch + datetime.timedelta(days=dt.value)
    if dt.value == int(dt.value):
        return moment.date().isoformat()
    return moment.strftime("%Y-%m-%d %H:%M:%S")


def render_cell(cell: types.CellData) -> str:
    """Render one CellData oneof to display text."""
    kind = cell.WhichOneof("value")
    match kind:
        case "int_value":
            return str(cell.int_value)
        case "float_value":
            return f"{cell.float_value:g}"
        case "string_value":
            return cell.string_value
        case "shared_string_value":
            return cell.shared_string_value
        case "bool_value":
            return "TRUE" if cell.bool_value else "FALSE"
        case "date_time":
            return format_excel_datetime(cell.date_time)
        case "date_time_iso":
            return cell.date_time_iso
        case "duration_iso":
            return cell.duration_iso
        case "error":
            return EXCEL_ERRORS.get(cell.error, "#ERR?")
        case _:
            return "·"


def sheet_selector(value: str) -> svc.SheetSelector:
    """Interpret a numeric argument as an index, anything else as a name."""
    if value.isdigit():
        return svc.SheetSelector(sheet_index=int(value))
    return svc.SheetSelector(sheet_name=value)


def stream_rows(stub: rpc.CalamineServiceStub, workbook_id: str, sheet: str) -> None:
    """Stream a worksheet's values and print them as they arrive."""
    request = svc.StreamWorksheetRangeRequest(
        workbook_id=workbook_id, sheet=sheet_selector(sheet)
    )
    for event in stub.StreamWorksheetRange(request):
        match event.WhichOneof("event"):
            case "started":
                started = event.started
                print(f'\nstreaming "{started.sheet_name}" — {started.total_cells} cells\n')
            case "row":
                cells = " │ ".join(render_cell(c) for c in event.row.values)
                print(f"{event.row.row_index + 1:>6} │ {cells}")
            case "error":
                detail = event.error.error
                print(f"in-band error: {detail.message}", file=sys.stderr)
                if event.error.terminal:
                    sys.exit(1)


def stream_formulas(stub: rpc.CalamineServiceStub, workbook_id: str, sheet: str) -> None:
    """Stream a worksheet's formulas and print the non-empty ones."""
    request = svc.StreamWorksheetFormulaRequest(
        workbook_id=workbook_id, sheet=sheet_selector(sheet)
    )
    for event in stub.StreamWorksheetFormula(request):
        match event.WhichOneof("event"):
            case "started":
                print(f'\nformulas of "{event.started.sheet_name}"\n')
            case "row":
                for col, formula in enumerate(event.row.formulas):
                    if formula:
                        print(f"  r{event.row.row_index + 1} c{col + 1}: ={formula}")
            case "error":
                print(f"in-band error: {event.error.error.message}", file=sys.stderr)


def stream_vba(stub: rpc.CalamineServiceStub, workbook_id: str) -> None:
    """Stream the VBA project, decoding modules for display."""
    for event in stub.StreamVbaProject(svc.StreamVbaProjectRequest(workbook_id=workbook_id)):
        match event.WhichOneof("event"):
            case "info":
                info = event.info
                if not info.present:
                    print("no VBA project in this workbook")
                    return
                print(f"VBA project: modules {list(info.module_names)}")
                for ref in info.references:
                    print(f"  reference: {ref.name} — {ref.description}")
            case "module":
                # Raw content is MBCS; latin-1 preserves bytes for display.
                source = event.module.raw_content.decode("latin-1")
                lines = source.count("\n") + 1
                print(f"\n--- {event.module.name} ({lines} lines) ---")
                print("\n".join(source.splitlines()[:8]))
            case "error":
                print(f"in-band error: {event.error.error.message}", file=sys.stderr)


def main() -> None:
    parser = argparse.ArgumentParser(description="grpc-calamine Python demo client")
    parser.add_argument("workbook", type=Path, help="workbook file to upload")
    parser.add_argument("--sheet", default="0", help="sheet name or zero-based index")
    parser.add_argument("--formulas", action="store_true", help="stream formulas too")
    parser.add_argument("--vba", action="store_true", help="stream the VBA project too")
    parser.add_argument("--addr", default="127.0.0.1:50051", help="server address")
    args = parser.parse_args()

    with grpc.insecure_channel(args.addr) as channel:
        stub = rpc.CalamineServiceStub(channel)

        opened = stub.OpenWorkbook(upload_frames(args.workbook))
        format_name = types.WorkbookFormat.Name(opened.detected_format)
        print(f"opened {args.workbook.name} as {format_name} — handle {opened.workbook_id}")
        print("sheets:", ", ".join(s.name for s in opened.metadata.sheets))
        for defined in opened.metadata.defined_names:
            print(f"defined name: {defined.name} = {defined.definition}")

        try:
            stream_rows(stub, opened.workbook_id, args.sheet)
            if args.formulas:
                stream_formulas(stub, opened.workbook_id, args.sheet)
            if args.vba:
                stream_vba(stub, opened.workbook_id)
        finally:
            stub.CloseWorkbook(svc.CloseWorkbookRequest(workbook_id=opened.workbook_id))


if __name__ == "__main__":
    main()
