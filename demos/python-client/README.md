# python-client

CLI demo client. `run.sh` is self-contained: it creates a virtualenv,
installs `grpcio`, generates stubs from `../../proto` with
`grpc_tools.protoc` (regenerating whenever the protos change), and runs
the client.

```bash
./run.sh ../sample-data/date.xlsx                      # stream first sheet
./run.sh ../sample-data/errors.xlsx --sheet Feuil1     # by name
./run.sh ../sample-data/formula.issue.xlsx --formulas  # values + formulas
./run.sh ../sample-data/vba.xlsm --vba                 # + VBA modules
./run.sh file.ods --addr host:port                     # remote server
```

Worth reading in `client.py`:

- `upload_frames` — the client-streaming upload as a plain generator.
- `format_excel_datetime` — serial-to-datetime conversion honoring the
  per-workbook 1904 epoch flag carried by the contract.
- `stream_vba` — VBA modules arrive as raw MBCS bytes (exactly what
  calamine's `get_module_raw` returns); decoding is the client's choice.
