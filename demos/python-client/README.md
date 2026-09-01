# python-client

An **example**, not a library. One file, `client.py`, that uploads a workbook
to grpc-calamine and prints a worksheet as the rows arrive, plus formulas and
VBA modules on request. Copy from it; do not depend on it.

`run.sh` is self-contained: it creates a virtualenv, installs `grpcio`,
generates stubs from [`../../proto`](../../proto) with `grpc_tools.protoc`
(regenerating whenever the protos change), and runs the client. Nothing
generated is checked in.

## Run it

Needs Python 3.11+. Start the server first, from the repository root:

```bash
cargo run --release          # listening on 0.0.0.0:50062
```

Then, in this directory:

```bash
./run.sh ../sample-data/date.xlsx                      # stream first sheet
./run.sh ../sample-data/errors.xlsx --sheet Feuil1     # by name
./run.sh ../sample-data/formula.issue.xlsx --formulas  # values + formulas
./run.sh ../sample-data/vba.xlsm --vba                 # + VBA modules
./run.sh file.ods --addr host:port                     # remote server
```

Expected output:

```
opened date.xlsx as WORKBOOK_FORMAT_XLSX — handle f9e625db-…
sheets: Sheet1

streaming "Sheet1" — 6 cells

     1 │ 2021-01-01 │ 15
     2 │ 2021-01-02 │ 16
     3 │ 255:10:10 │ 17
```

The first run builds the venv and generates stubs, so it takes a few
seconds; later runs are instant. Delete `.venv/` and `gen/` to start over.

Worth reading in `client.py`:

- `upload_frames`: the client-streaming upload as a plain generator.
- `format_excel_datetime`: serial-to-datetime conversion honoring the
  per-workbook 1904 epoch flag carried by the contract.
- `stream_vba`: VBA modules arrive as raw MBCS bytes (exactly what
  calamine's `get_module_raw` returns); decoding is the client's choice.

## Tutorial: talk to it from your own Python project

### 1. Install and generate stubs

There is no client package to `pip install`. The protos are the API surface.

```bash
pip install grpcio grpcio-tools

python -m grpc_tools.protoc \
    -I path/to/grpc-calamine/proto \
    --python_out=gen --grpc_python_out=gen --pyi_out=gen \
    calamine/v1/types.proto calamine/v1/calamine_service.proto

touch gen/__init__.py gen/calamine/__init__.py gen/calamine/v1/__init__.py
```

The generated `calamine_service_pb2_grpc.py` imports its sibling by the
proto path, so `gen/` has to be on `sys.path` (that is the `sys.path.insert`
at the top of `client.py`) rather than imported as a nested package.

### 2. Upload, stream, close

The API is handle-based: upload once, then run any number of reads against
the returned `workbook_id`.

```python
import sys
from pathlib import Path

sys.path.insert(0, "gen")

import grpc
from calamine.v1 import calamine_service_pb2 as svc
from calamine.v1 import calamine_service_pb2_grpc as rpc
from calamine.v1 import types_pb2 as types


def upload_frames(path: Path):
    """An options frame, then the file bytes in 1 MiB chunks."""
    yield svc.OpenWorkbookRequest(
        options=svc.WorkbookOptions(format_hint=types.WORKBOOK_FORMAT_UNSPECIFIED)
    )
    with path.open("rb") as file:
        while chunk := file.read(1024 * 1024):
            yield svc.OpenWorkbookRequest(chunk=chunk)


def render(cell: types.CellData) -> str:
    """CellData is a oneof mirroring calamine's Data enum exactly."""
    match cell.WhichOneof("value"):
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
        # An Excel serial plus the workbook's 1904 flag. Convert it
        # yourself, as client.py's format_excel_datetime does.
        case "date_time":
            return f"serial:{cell.date_time.value}"
        case "date_time_iso":
            return cell.date_time_iso
        case "duration_iso":
            return cell.duration_iso
        case "error":
            return types.CellErrorType.Name(cell.error)
        case _:
            return ""  # empty


def print_row(row: svc.WorksheetRow) -> None:
    print(f"{row.row_index + 1:>6} │ " + " │ ".join(render(c) for c in row.values))


with grpc.insecure_channel("127.0.0.1:50062") as channel:
    stub = rpc.CalamineServiceStub(channel)

    # 1. Upload. The generator IS the client-streaming request.
    opened = stub.OpenWorkbook(upload_frames(Path(sys.argv[1])))
    for sheet in opened.metadata.sheets:
        print("sheet:", sheet.name)

    # 2. Stream sheet 0. Rows arrive while the server is still parsing.
    request = svc.StreamWorksheetRangeRequest(
        workbook_id=opened.workbook_id,
        sheet=svc.SheetSelector(sheet_index=0),
    )
    try:
        for event in stub.StreamWorksheetRange(request):
            match event.WhichOneof("event"):
                case "started":
                    print("streaming", event.started.sheet_name)
                # "rows" is the DEFAULT carrier. Handle only "row" and you
                # will print nothing at all, and exit 0.
                case "rows":
                    for row in event.rows.rows:
                        print_row(row)
                case "row":
                    print_row(event.row)
                case "error":
                    print("in-band error:", event.error.error.message, file=sys.stderr)
    finally:
        # 3. Release the handle. Nothing else frees the server's memory.
        stub.CloseWorkbook(svc.CloseWorkbookRequest(workbook_id=opened.workbook_id))
```

```bash
python main.py book.xlsx
```

### Things that bite

- **`WhichOneof("event")` has five arms**, not three: `started`, `rows`,
  `row`, `string_table`, `error`. Missing `rows` is the failure that looks
  like success. Pass `max_rows_per_message=1` on the request if you want
  only the single-row carrier.
- **Chunk the upload.** The server's frame limit is 32 MiB. Yielding a
  100 MB workbook in one `chunk=` fails; 1 MiB chunks are what the demos use.
- **Rows are anchored at column A.** A value's index in `row.values` is its
  absolute zero-based column, so no header arithmetic is needed. Empty cells
  are explicit, never a gap.
- **In-band errors are usually not fatal.** Check `event.error.terminal`;
  a non-terminal one means the stream continues with the remaining items.
- **Close the handle.** The server holds the workbook bytes in memory until
  `CloseWorkbook`; dropping the channel alone does not free them. Hence the
  `try/finally`.
- **This will not beat `python-calamine` on a local file**, and it is not
  meant to. Reaching a server pays off when the file is not on your machine,
  when many clients share one parse, or when you want the same contract from
  twelve languages. The measured numbers, including the cases where
  python-calamine wins, are in [`../../bench/RESULTS.md`](../../bench/RESULTS.md).
