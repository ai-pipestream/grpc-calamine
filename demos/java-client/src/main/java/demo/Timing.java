package demo;

import calamine.v1.CalamineServiceGrpc;
import calamine.v1.CellData;
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
import io.grpc.ManagedChannelBuilder;
import io.grpc.stub.StreamObserver;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Iterator;
import java.util.concurrent.CompletableFuture;

/**
 * Timing-only client: consume every cell, print nothing per row.
 *
 * Environment:
 *   MODE=blocking|async     stub style (default blocking)
 *   EXECUTOR=default|direct directExecutor keeps callbacks on the netty event
 *                           loop instead of handing each message to a pool
 *   WINDOW=&lt;bytes&gt;    flow control window (0 = grpc-java default)
 *   PASSES=&lt;n&gt;        repeat the read so JIT warmup is visible
 */
public final class Timing {
    private static final int CHUNK = 1024 * 1024;

    /** Protobuf bytes this sheet puts on the wire, measured. */
    private static final double WIRE_MB = 791.1;

    /** Touch a cell so the decode cannot be optimized away. */
    private static long touch(CellData c) {
        return switch (c.getValueCase()) {
            case STRING_VALUE -> c.getStringValue().length();
            case SHARED_STRING_VALUE -> c.getSharedStringValue().length();
            case FLOAT_VALUE -> Double.doubleToLongBits(c.getFloatValue());
            case INT_VALUE -> c.getIntValue();
            default -> 1L;
        };
    }

    public static void main(String[] args) throws Exception {
        String file = args[0];
        String sheet = args[1];
        String addr = System.getenv().getOrDefault("CALAMINE_ADDR", "127.0.0.1:50055");
        String mode = System.getenv().getOrDefault("MODE", "blocking");
        String executor = System.getenv().getOrDefault("EXECUTOR", "default");
        int window = Integer.parseInt(System.getenv().getOrDefault("WINDOW", "0"));
        int passes = Integer.parseInt(System.getenv().getOrDefault("PASSES", "3"));

        ManagedChannelBuilder<?> builder =
                Grpc.newChannelBuilder(addr, InsecureChannelCredentials.create())
                        .maxInboundMessageSize(32 * 1024 * 1024);
        if (executor.equals("direct")) {
            builder = builder.directExecutor();
        }
        if (window > 0) {
            builder = ((io.grpc.netty.shaded.io.grpc.netty.NettyChannelBuilder) builder)
                    .flowControlWindow(window);
        }
        ManagedChannel channel = builder.build();

        var async = CalamineServiceGrpc.newStub(channel);
        var blocking = CalamineServiceGrpc.newBlockingStub(channel)
                .withMaxInboundMessageSize(32 * 1024 * 1024);

        byte[] bytes = Files.readAllBytes(Path.of(file));

        long t0 = System.nanoTime();
        CompletableFuture<OpenWorkbookResponse> opened = new CompletableFuture<>();
        StreamObserver<OpenWorkbookRequest> up = async.openWorkbook(new StreamObserver<>() {
            public void onNext(OpenWorkbookResponse r) { opened.complete(r); }
            public void onError(Throwable t) { opened.completeExceptionally(t); }
            public void onCompleted() {}
        });
        up.onNext(OpenWorkbookRequest.newBuilder()
                .setOptions(WorkbookOptions.newBuilder()
                        .setFormatHint(WorkbookFormat.WORKBOOK_FORMAT_UNSPECIFIED))
                .build());
        for (int i = 0; i < bytes.length; i += CHUNK) {
            int n = Math.min(CHUNK, bytes.length - i);
            up.onNext(OpenWorkbookRequest.newBuilder()
                    .setChunk(ByteString.copyFrom(bytes, i, n)).build());
        }
        up.onCompleted();
        String id = opened.get().getWorkbookId();
        long uploadMs = (System.nanoTime() - t0) / 1_000_000;

        String label = String.format("%s/%s/win=%s", mode, executor,
                window > 0 ? String.valueOf(window / (1024 * 1024)) + "M" : "def");

        for (int pass = 1; pass <= passes; pass++) {
            StreamWorksheetRangeRequest req = StreamWorksheetRangeRequest.newBuilder()
                    .setWorkbookId(id)
                    .setSheet(SheetSelector.newBuilder().setSheetName(sheet))
                    .setMaxRowsPerMessage(Integer.parseInt(System.getenv().getOrDefault("BATCH", "0")))
                    .build();
            long rows;
            long cells;
            long ttfr;
            long ms;
            long asyncMsgs = 0;
            long blockingMsgs = 0;
            final long t1 = System.nanoTime();

            if (mode.equals("async")) {
                long[] st = new long[5];
                CompletableFuture<Void> done = new CompletableFuture<>();
                async.streamWorksheetRange(req, new StreamObserver<StreamWorksheetRangeResponse>() {
                    public void onNext(StreamWorksheetRangeResponse ev) {
                        if (st[0] == 0 && ev.getEventCase() != StreamWorksheetRangeResponse.EventCase.STARTED) {
                            st[2] = (System.nanoTime() - t1) / 1_000_000;
                        }
                        switch (ev.getEventCase()) {
                            case ROW -> { st[0]++; for (CellData c : ev.getRow().getValuesList()) { st[1]++; st[3] += touch(c); } }
                            case ROWS -> { for (WorksheetRow r : ev.getRows().getRowsList()) { st[0]++; for (CellData c : r.getValuesList()) { st[1]++; st[3] += touch(c); } } }
                            default -> { }
                        }
                        st[4]++;
                    }
                    public void onError(Throwable t) { done.completeExceptionally(t); }
                    public void onCompleted() { done.complete(null); }
                });
                done.get();
                ms = (System.nanoTime() - t1) / 1_000_000;
                rows = st[0];
                cells = st[1];
                ttfr = st[2];
                asyncMsgs = st[4];
            } else {
                Iterator<StreamWorksheetRangeResponse> it = blocking.streamWorksheetRange(req);
                long r = 0;
                long c = 0;
                long f = 0;
                long sink = 0;
                long msgs = 0;
                while (it.hasNext()) {
                    StreamWorksheetRangeResponse ev = it.next();
                    if (r == 0 && ev.getEventCase() != StreamWorksheetRangeResponse.EventCase.STARTED) {
                        f = (System.nanoTime() - t1) / 1_000_000;
                    }
                    if (ev.getEventCase() == StreamWorksheetRangeResponse.EventCase.ROW) {
                        r++; msgs++;
                        for (CellData cd : ev.getRow().getValuesList()) { c++; sink += touch(cd); }
                    } else if (ev.getEventCase() == StreamWorksheetRangeResponse.EventCase.ROWS) {
                        msgs++;
                        for (WorksheetRow wr : ev.getRows().getRowsList()) {
                            r++;
                            for (CellData cd : wr.getValuesList()) { c++; sink += touch(cd); }
                        }
                    }
                }
                ms = (System.nanoTime() - t1) / 1_000_000;
                rows = r;
                cells = c;
                ttfr = f;
                blockingMsgs = msgs;
                if (sink == Long.MIN_VALUE) System.out.print("");
            }

            long messages = mode.equals("async") ? asyncMsgs : blockingMsgs;
            System.out.printf(
                "java %-22s batch=%-4s pass %d  stream=%5d ms  %7d rows/s  %6.1f MB/s  %8d msgs  %5.2f us/msg%n",
                label, System.getenv().getOrDefault("BATCH", "0"), pass, ms,
                Math.round(rows / (ms / 1000.0)),
                WIRE_MB / (ms / 1000.0),
                messages,
                (ms * 1000.0) / Math.max(messages, 1));
        }
        channel.shutdownNow();
    }
}
