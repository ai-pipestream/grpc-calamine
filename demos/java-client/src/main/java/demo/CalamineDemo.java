// SPDX-License-Identifier: Apache-2.0
package demo;

import calamine.v1.CalamineServiceGrpc;
import calamine.v1.CellData;
import calamine.v1.CloseWorkbookRequest;
import calamine.v1.ExcelDateTime;
import calamine.v1.ExcelDateTimeType;
import calamine.v1.OpenWorkbookRequest;
import calamine.v1.OpenWorkbookResponse;
import calamine.v1.SheetSelector;
import calamine.v1.StreamWorksheetRangeRequest;
import calamine.v1.StreamWorksheetRangeResponse;
import calamine.v1.WorkbookFormat;
import calamine.v1.WorkbookOptions;
import calamine.v1.WorksheetRow;
import com.google.protobuf.ByteString;
import io.grpc.Grpc;
import io.grpc.InsecureChannelCredentials;
import io.grpc.ManagedChannel;
import io.grpc.stub.StreamObserver;
import java.io.IOException;
import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.time.LocalDateTime;
import java.time.format.DateTimeFormatter;
import java.util.Iterator;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import java.util.stream.Collectors;

/**
 * Java demo client for the grpc-calamine server.
 *
 * <p>Uploads a workbook (client-streaming), prints its metadata, then streams
 * a worksheet row by row exactly as the Rust server parses it.
 *
 * <pre>
 *   mvn -q compile exec:java -Dexec.args="../sample-data/date.xlsx [sheet]"
 * </pre>
 */
public final class CalamineDemo {

    private static final int CHUNK_BYTES = 1024 * 1024;

    /** The exact display strings Excel uses for cell errors. */
    private static final Map<calamine.v1.CellErrorType, String> EXCEL_ERRORS = Map.of(
            calamine.v1.CellErrorType.CELL_ERROR_TYPE_DIV0, "#DIV/0!",
            calamine.v1.CellErrorType.CELL_ERROR_TYPE_NA, "#N/A",
            calamine.v1.CellErrorType.CELL_ERROR_TYPE_NAME, "#NAME?",
            calamine.v1.CellErrorType.CELL_ERROR_TYPE_NULL, "#NULL!",
            calamine.v1.CellErrorType.CELL_ERROR_TYPE_NUM, "#NUM!",
            calamine.v1.CellErrorType.CELL_ERROR_TYPE_REF, "#REF!",
            calamine.v1.CellErrorType.CELL_ERROR_TYPE_VALUE, "#VALUE!",
            calamine.v1.CellErrorType.CELL_ERROR_TYPE_GETTING_DATA, "#DATA!");

    private CalamineDemo() {}

    public static void main(String[] args) throws Exception {
        if (args.length < 1) {
            System.err.println("usage: CalamineDemo <workbook-file> [sheet-name-or-index]");
            System.exit(2);
        }
        Path file = Path.of(args[0]);
        String sheetArg = args.length > 1 ? args[1] : "0";
        String addr = System.getenv().getOrDefault("CALAMINE_ADDR", "127.0.0.1:50051");

        ManagedChannel channel =
                Grpc.newChannelBuilder(addr, InsecureChannelCredentials.create()).build();
        try {
            OpenWorkbookResponse opened = openWorkbook(channel, file);
            System.out.printf(
                    "opened %s as %s — handle %s%n",
                    file.getFileName(), opened.getDetectedFormat(), opened.getWorkbookId());
            System.out.println(
                    "sheets: "
                            + opened.getMetadata().getSheetsList().stream()
                                    .map(calamine.v1.Sheet::getName)
                                    .collect(Collectors.joining(", ")));

            streamRows(channel, opened.getWorkbookId(), sheetArg);

            CalamineServiceGrpc.newBlockingStub(channel)
                    .closeWorkbook(
                            CloseWorkbookRequest.newBuilder()
                                    .setWorkbookId(opened.getWorkbookId())
                                    .build());
        } finally {
            channel.shutdownNow().awaitTermination(5, TimeUnit.SECONDS);
        }
    }

    /** Client-streaming upload: options frame first, then 1 MiB chunks. */
    private static OpenWorkbookResponse openWorkbook(ManagedChannel channel, Path file)
            throws IOException, InterruptedException {
        CompletableFuture<OpenWorkbookResponse> response = new CompletableFuture<>();
        StreamObserver<OpenWorkbookRequest> upload =
                CalamineServiceGrpc.newStub(channel)
                        .openWorkbook(
                                new StreamObserver<>() {
                                    @Override
                                    public void onNext(OpenWorkbookResponse value) {
                                        response.complete(value);
                                    }

                                    @Override
                                    public void onError(Throwable t) {
                                        response.completeExceptionally(t);
                                    }

                                    @Override
                                    public void onCompleted() {}
                                });

        upload.onNext(
                OpenWorkbookRequest.newBuilder()
                        .setOptions(
                                WorkbookOptions.newBuilder()
                                        .setFormatHint(
                                                WorkbookFormat.WORKBOOK_FORMAT_UNSPECIFIED))
                        .build());
        try (InputStream in = Files.newInputStream(file)) {
            byte[] buffer = new byte[CHUNK_BYTES];
            for (int read = in.read(buffer); read > 0; read = in.read(buffer)) {
                upload.onNext(
                        OpenWorkbookRequest.newBuilder()
                                .setChunk(ByteString.copyFrom(buffer, 0, read))
                                .build());
            }
        }
        upload.onCompleted();
        return response.join();
    }

    /** Server-streaming read: header, then one event per parsed row. */
    private static void streamRows(ManagedChannel channel, String workbookId, String sheetArg) {
        SheetSelector.Builder sheet = SheetSelector.newBuilder();
        if (sheetArg.chars().allMatch(Character::isDigit)) {
            sheet.setSheetIndex(Integer.parseInt(sheetArg));
        } else {
            sheet.setSheetName(sheetArg);
        }

        Iterator<StreamWorksheetRangeResponse> events =
                CalamineServiceGrpc.newBlockingStub(channel)
                        .streamWorksheetRange(
                                StreamWorksheetRangeRequest.newBuilder()
                                        .setWorkbookId(workbookId)
                                        .setSheet(sheet)
                                        .build());

        while (events.hasNext()) {
            StreamWorksheetRangeResponse event = events.next();
            switch (event.getEventCase()) {
                case STARTED ->
                        System.out.printf(
                                "%nstreaming \"%s\" — %d cells%n%n",
                                event.getStarted().getSheetName(),
                                event.getStarted().getTotalCells());
                // Rows arrive batched by default; the single-row carrier is
                // used only when the client asks for maxRowsPerMessage = 1.
                // A client that handles only ROW silently prints nothing.
                case ROWS -> event.getRows().getRowsList().forEach(CalamineDemo::printRow);
                case ROW -> printRow(event.getRow());
                case ERROR -> {
                    System.err.println(
                            "in-band error: " + event.getError().getError().getMessage());
                    if (event.getError().getTerminal()) {
                        return;
                    }
                }
                default -> { }
            }
        }
    }

    /** Print one streamed row, whichever carrier delivered it. */
    private static void printRow(WorksheetRow row) {
        String cells =
                row.getValuesList().stream()
                        .map(CalamineDemo::renderCell)
                        .collect(Collectors.joining(" │ "));
        System.out.printf("%6d │ %s%n", row.getRowIndex() + 1, cells);
    }

    /** Render one CellData oneof to display text. */
    private static String renderCell(CellData cell) {
        return switch (cell.getValueCase()) {
            case INT_VALUE -> Long.toString(cell.getIntValue());
            case FLOAT_VALUE -> trimNumber(cell.getFloatValue());
            case STRING_VALUE -> cell.getStringValue();
            case SHARED_STRING_VALUE -> cell.getSharedStringValue();
            case BOOL_VALUE -> cell.getBoolValue() ? "TRUE" : "FALSE";
            case DATE_TIME -> formatExcelDateTime(cell.getDateTime());
            case DATE_TIME_ISO -> cell.getDateTimeIso();
            case DURATION_ISO -> cell.getDurationIso();
            case ERROR -> EXCEL_ERRORS.getOrDefault(cell.getError(), "#ERR?");
            default -> "·";
        };
    }

    private static String trimNumber(double value) {
        return value == Math.rint(value) && !Double.isInfinite(value)
                ? Long.toString((long) value)
                : Double.toString(value);
    }

    /** Render an Excel serial datetime, honoring the workbook's epoch. */
    private static String formatExcelDateTime(ExcelDateTime dt) {
        if (dt.getDatetimeType() == ExcelDateTimeType.EXCEL_DATE_TIME_TYPE_TIME_DELTA) {
            Duration duration = Duration.ofSeconds(Math.round(dt.getValue() * 86400));
            return "%d:%02d:%02d"
                    .formatted(duration.toHours(), duration.toMinutesPart(),
                            duration.toSecondsPart());
        }
        // 1899-12-30 absorbs Excel's fictitious 1900-02-29.
        LocalDateTime epoch =
                dt.getIs1904()
                        ? LocalDateTime.of(1904, 1, 1, 0, 0)
                        : LocalDateTime.of(1899, 12, 30, 0, 0);
        LocalDateTime moment = epoch.plusSeconds(Math.round(dt.getValue() * 86400));
        return dt.getValue() == Math.floor(dt.getValue())
                ? moment.toLocalDate().toString()
                : moment.format(DateTimeFormatter.ofPattern("yyyy-MM-dd HH:mm:ss"));
    }
}
