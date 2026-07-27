// SPDX-License-Identifier: Apache-2.0

//! Binary entry point for the calamine gRPC server.
//!
//! Runtime sizing (all optional environment overrides):
//! - `GRPC_CALAMINE_ADDR` — listen address (default `0.0.0.0:50051`).
//! - `GRPC_CALAMINE_WORKERS` — tokio worker threads (default: CPU count).
//! - `GRPC_CALAMINE_BLOCKING_THREADS` — cap of the blocking pool that runs
//!   calamine parsing (default: 512, tokio's own default).
//! - `GRPC_CALAMINE_WINDOW_BYTES` — HTTP/2 initial stream and connection
//!   window (default: 50 MiB).
//! - `GRPC_CALAMINE_MAX_CONCURRENT_STREAMS` — streaming reads admitted at
//!   once (default: 128). Past the cap a read is refused with
//!   `RESOURCE_EXHAUSTED` rather than queued.

use std::time::Duration;

use tonic::transport::Server;

use grpc_calamine::{CalamineGrpc, WorkbookStore};

/// Default listen address when `GRPC_CALAMINE_ADDR` is not set.
const DEFAULT_ADDR: &str = "0.0.0.0:50051";

/// Default HTTP/2 initial window, for both the stream and the connection.
///
/// hyper's own default is 1 MiB. Workbook uploads are bulk transfers of tens
/// or hundreds of megabytes, so a wide window keeps them from being paced at
/// one window per round trip over any link with real latency.
const DEFAULT_WINDOW_BYTES: u32 = 50 * 1024 * 1024;

/// Read a `usize` environment variable, falling back to `default`.
fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workers = env_usize(
        "GRPC_CALAMINE_WORKERS",
        std::thread::available_parallelism().map_or(4, usize::from),
    );
    let blocking = env_usize("GRPC_CALAMINE_BLOCKING_THREADS", 512);

    // Explicit multi-threaded runtime: every request and every parse task is
    // spread across all worker threads; calamine's CPU-bound parsing runs in
    // the blocking pool so it never stalls the async workers.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(workers)
        .max_blocking_threads(blocking)
        .build()?;

    runtime.block_on(serve())
}

async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::var("GRPC_CALAMINE_ADDR")
        .unwrap_or_else(|_| DEFAULT_ADDR.to_string())
        .parse()?;

    // Streaming reads are capped well below the blocking pool so they can
    // never take every thread and leave uploads with none.
    let mut grpc = CalamineGrpc::new(WorkbookStore::new());
    if let Some(max) = std::env::var("GRPC_CALAMINE_MAX_CONCURRENT_STREAMS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        grpc = grpc.with_max_concurrent_streams(max);
    }
    let service = grpc.into_service();

    // HTTP/2 flow control is directional: this governs what the server
    // *receives*, so it sizes the `OpenWorkbook` upload, not the row stream.
    // A client that wants a wide download window has to set its own; hyper
    // defaults both to 1 MiB, which throttles a bulk transfer to one window
    // per round trip once there is real latency in the path.
    let window = u32::try_from(env_usize(
        "GRPC_CALAMINE_WINDOW_BYTES",
        DEFAULT_WINDOW_BYTES as usize,
    ))
    .unwrap_or(DEFAULT_WINDOW_BYTES);

    eprintln!("grpc-calamine listening on {addr} (http2 window {window} bytes)");
    Server::builder()
        // Latency/throughput tuning for many concurrent streaming clients.
        .tcp_nodelay(true)
        .tcp_keepalive(Some(Duration::from_secs(60)))
        .http2_keepalive_interval(Some(Duration::from_secs(30)))
        .http2_keepalive_timeout(Some(Duration::from_secs(10)))
        .initial_stream_window_size(window)
        .initial_connection_window_size(window)
        .max_concurrent_streams(1024)
        .add_service(service)
        .serve_with_shutdown(addr, shutdown_signal())
        .await?;
    eprintln!("grpc-calamine shut down");
    Ok(())
}

/// Resolve when the process receives SIGINT (Ctrl-C) or SIGTERM, so open
/// streams can drain instead of being cut mid-row.
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! {
        _ = ctrl_c => {}
        _ = sigterm.recv() => {}
    }
}
