# java-client

CLI demo client built with Maven. The `protobuf-maven-plugin` generates
`grpc-java` stubs from `../../proto` at build time; nothing generated is
checked in.

```bash
mvn -q compile exec:java -Dexec.args="../sample-data/date.xlsx"
mvn -q compile exec:java -Dexec.args="../sample-data/errors.xlsx Feuil1"
CALAMINE_ADDR=host:port mvn -q compile exec:java -Dexec.args="file.xlsb 2"
```

Worth reading in `CalamineDemo.java`:

- `openWorkbook` — the client-streaming upload with an async
  `StreamObserver`, bridged to a `CompletableFuture` (the request stream
  and the single response are independent in gRPC Java).
- `streamRows` — the server stream consumed as a plain blocking
  `Iterator`, switching on the response `oneof` (`STARTED`/`ROW`/`ERROR`).
- `formatExcelDateTime` — serial-to-datetime conversion honoring the
  per-workbook 1904 epoch flag.
