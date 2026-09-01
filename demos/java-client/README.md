# java-client

An **example**, not a library. One file, `CalamineDemo.java`, that uploads a
workbook to grpc-calamine and prints a worksheet as the rows arrive. Copy
from it; do not depend on it.

The `protobuf-maven-plugin` generates `grpc-java` stubs from
[`../../proto`](../../proto) at build time. Nothing generated is checked in.

## Run it

Needs a JDK 17+ and Maven. Start the server first, from the repository root:

```bash
cargo run --release          # listening on 0.0.0.0:50062
```

Then, in this directory:

```bash
mvn -q compile exec:java -Dexec.args="../sample-data/date.xlsx"
mvn -q compile exec:java -Dexec.args="../sample-data/errors.xlsx Feuil1"
CALAMINE_ADDR=host:port mvn -q compile exec:java -Dexec.args="file.xlsb 2"
```

The second argument is a sheet name or a zero-based index. Expected output:

```
opened date.xlsx as WORKBOOK_FORMAT_XLSX — handle 42206e42-…
sheets: Sheet1

streaming "Sheet1" — 6 cells

     1 │ 2021-01-01 │ 15
     2 │ 2021-01-02 │ 16
     3 │ 255:10:10 │ 17
```

The first `mvn` run downloads a `protoc` binary for your platform plus the
grpc-java codegen plugin, so it takes a minute; later runs are instant.

Worth reading in `CalamineDemo.java`:

- `openWorkbook`: the client-streaming upload with an async
  `StreamObserver`, bridged to a `CompletableFuture` (the request stream
  and the single response are independent in gRPC Java).
- `streamRows`: the server stream consumed as a plain blocking
  `Iterator`, switching on the response `oneof`.
- `formatExcelDateTime`: serial-to-datetime conversion honoring the
  per-workbook 1904 epoch flag.

## Tutorial: talk to it from your own Java project

### 1. Take the contract

Copy the two proto files into the standard Maven location. They are the
whole API surface; there is no client jar to depend on.

```bash
mkdir -p src/main/proto/calamine/v1
cp path/to/grpc-calamine/proto/calamine/v1/*.proto src/main/proto/calamine/v1/
```

### 2. Wire up codegen

In `pom.xml`. `os-maven-plugin` picks the right `protoc` binary for your
platform, and `compile-custom` is what emits the gRPC service stubs
(`compile` alone gives you only the message classes).

```xml
<properties>
  <maven.compiler.release>17</maven.compiler.release>
  <grpc.version>1.68.1</grpc.version>
  <protobuf.version>4.28.3</protobuf.version>
</properties>

<dependencies>
  <dependency><groupId>com.google.protobuf</groupId><artifactId>protobuf-java</artifactId><version>${protobuf.version}</version></dependency>
  <dependency><groupId>io.grpc</groupId><artifactId>grpc-netty-shaded</artifactId><version>${grpc.version}</version></dependency>
  <dependency><groupId>io.grpc</groupId><artifactId>grpc-protobuf</artifactId><version>${grpc.version}</version></dependency>
  <dependency><groupId>io.grpc</groupId><artifactId>grpc-stub</artifactId><version>${grpc.version}</version></dependency>
  <!-- supplies @Generated for the generated sources -->
  <dependency><groupId>org.apache.tomcat</groupId><artifactId>annotations-api</artifactId><version>6.0.53</version><scope>provided</scope></dependency>
</dependencies>

<build>
  <extensions>
    <extension><groupId>kr.motd.maven</groupId><artifactId>os-maven-plugin</artifactId><version>1.7.1</version></extension>
  </extensions>
  <plugins>
    <plugin>
      <groupId>org.xolstice.maven.plugins</groupId>
      <artifactId>protobuf-maven-plugin</artifactId>
      <version>0.6.1</version>
      <configuration>
        <protocArtifact>com.google.protobuf:protoc:${protobuf.version}:exe:${os.detected.classifier}</protocArtifact>
        <pluginId>grpc-java</pluginId>
        <pluginArtifact>io.grpc:protoc-gen-grpc-java:${grpc.version}:exe:${os.detected.classifier}</pluginArtifact>
      </configuration>
      <executions>
        <execution><goals><goal>compile</goal><goal>compile-custom</goal></goals></execution>
      </executions>
    </plugin>
  </plugins>
</build>
```

`mvn compile` now generates everything into the `calamine.v1` package.

### 3. Upload, stream, close

The API is handle-based: upload once, then run any number of reads against
the returned `workbook_id`.

```java
import calamine.v1.*;
import com.google.protobuf.ByteString;
import io.grpc.*;
import io.grpc.stub.StreamObserver;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.CompletableFuture;

public final class Main {
    public static void main(String[] args) throws Exception {
        ManagedChannel channel = Grpc.newChannelBuilder(
                "127.0.0.1:50062", InsecureChannelCredentials.create()).build();

        // 1. Upload. Client-streaming: an options frame, then the file bytes.
        //    The single response arrives on its own observer, so bridge it.
        CompletableFuture<OpenWorkbookResponse> opened = new CompletableFuture<>();
        StreamObserver<OpenWorkbookRequest> upload = CalamineServiceGrpc.newStub(channel)
                .openWorkbook(new StreamObserver<>() {
                    public void onNext(OpenWorkbookResponse r) { opened.complete(r); }
                    public void onError(Throwable t) { opened.completeExceptionally(t); }
                    public void onCompleted() { }
                });
        upload.onNext(OpenWorkbookRequest.newBuilder()
                .setOptions(WorkbookOptions.getDefaultInstance())   // format auto-detected
                .build());
        byte[] file = Files.readAllBytes(Path.of(args[0]));
        for (int off = 0; off < file.length; off += 1 << 20) {      // 1 MiB chunks
            upload.onNext(OpenWorkbookRequest.newBuilder()
                    .setChunk(ByteString.copyFrom(
                            file, off, Math.min(1 << 20, file.length - off)))
                    .build());
        }
        upload.onCompleted();

        OpenWorkbookResponse workbook = opened.join();
        String id = workbook.getWorkbookId();
        for (Sheet sheet : workbook.getMetadata().getSheetsList()) {
            System.out.println("sheet: " + sheet.getName());
        }

        // 2. Stream sheet 0. Rows arrive while the server is still parsing.
        var blocking = CalamineServiceGrpc.newBlockingStub(channel);
        Iterator<StreamWorksheetRangeResponse> events = blocking.streamWorksheetRange(
                StreamWorksheetRangeRequest.newBuilder()
                        .setWorkbookId(id)
                        .setSheet(SheetSelector.newBuilder().setSheetIndex(0))
                        .build());

        while (events.hasNext()) {
            StreamWorksheetRangeResponse event = events.next();
            switch (event.getEventCase()) {
                case STARTED -> System.out.println("streaming " + event.getStarted().getSheetName());
                // ROWS is the DEFAULT carrier. Handle only ROW and you will
                // print nothing at all, and exit successfully.
                case ROWS -> event.getRows().getRowsList().forEach(Main::print);
                case ROW  -> print(event.getRow());
                case ERROR -> System.err.println(event.getError().getError().getMessage());
                default -> { }
            }
        }

        // 3. Release the handle. Nothing else frees the server's memory.
        blocking.closeWorkbook(CloseWorkbookRequest.newBuilder().setWorkbookId(id).build());
        channel.shutdownNow();
    }

    /** One CellData is a oneof mirroring calamine's Data enum exactly. */
    static void print(WorksheetRow row) {
        StringBuilder line = new StringBuilder().append(row.getRowIndex() + 1).append(": ");
        for (CellData cell : row.getValuesList()) {
            line.append(switch (cell.getValueCase()) {
                case INT_VALUE -> String.valueOf(cell.getIntValue());
                case FLOAT_VALUE -> String.valueOf(cell.getFloatValue());
                case STRING_VALUE -> cell.getStringValue();
                case SHARED_STRING_VALUE -> cell.getSharedStringValue();
                case BOOL_VALUE -> String.valueOf(cell.getBoolValue());
                // An Excel serial plus the workbook's 1904 flag. Convert it
                // yourself, as CalamineDemo.formatExcelDateTime does.
                case DATE_TIME -> "serial:" + cell.getDateTime().getValue();
                case DATE_TIME_ISO -> cell.getDateTimeIso();
                case DURATION_ISO -> cell.getDurationIso();
                case ERROR -> cell.getError().name();
                default -> "";                                      // EMPTY
            }).append(" | ");
        }
        System.out.println(line);
    }
}
```

```bash
mvn -q compile exec:java -Dexec.mainClass=Main -Dexec.args=book.xlsx
```

Note that `-Dexec.mainClass` on the command line is ignored if your `pom.xml`
already sets `<mainClass>` in the `exec-maven-plugin` configuration; the POM
wins. Either drop it from the POM or run the class directly with `java -cp`.

### Things that bite

- **Chunk the upload.** The server's frame limit is 32 MiB. A single
  `setChunk` of a 100 MB workbook fails; 1 MiB chunks are what the demos use.
- **`getEventCase()` has five arms**, not three: `STARTED`, `ROWS`, `ROW`,
  `STRING_TABLE`, `ERROR`. Missing `ROWS` is the failure that looks like
  success.
- **Rows are anchored at column A.** A value's index in `getValuesList()`
  is its absolute zero-based column, so no header arithmetic is needed.
  Empty cells are explicit, never a gap.
- **`ERROR` events are usually not fatal.** Check
  `event.getError().getTerminal()`; a non-terminal one means the stream
  continues with the remaining items.
- **Close the handle.** The server holds the workbook bytes in memory until
  `CloseWorkbook`; dropping the connection alone does not free them.
- **For throughput, not just correctness**, an async stub with
  `directExecutor()` beats the blocking iterator by roughly 2.7x on a
  million-row sheet, because the blocking iterator hands every message
  across a thread boundary. `Timing.java` in this directory measures the
  configurations; see [`../../bench/README.md`](../../bench/README.md).
