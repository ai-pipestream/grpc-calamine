// SPDX-License-Identifier: Apache-2.0

//! Binary entry point for the calamine gRPC server.
//!
//! Runtime sizing (all optional environment overrides):
//! - `GRPC_CALAMINE_ADDR` — listen address (default `0.0.0.0:50051`).
//! - `GRPC_CALAMINE_WORKERS` — tokio worker threads (default: CPU count).
//! - `GRPC_CALAMINE_BLOCKING_THREADS` — cap of the blocking pool that runs
//!   calamine parsing (default: 512, tokio's own default).

use std::time::Duration;

use tonic::transport::Server;

use grpc_calamine::{CalamineGrpc, WorkbookStore};

/// Default listen address when `GRPC_CALAMINE_ADDR` is not set.
const DEFAULT_ADDR: &str = "0.0.0.0:50051";

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

    let service = CalamineGrpc::new(WorkbookStore::new()).into_service();

    eprintln!("grpc-calamine listening on {addr}");
    Server::builder()
        // Latency/throughput tuning for many concurrent streaming clients.
        .tcp_nodelay(true)
        .tcp_keepalive(Some(Duration::from_secs(60)))
        .http2_keepalive_interval(Some(Duration::from_secs(30)))
        .http2_keepalive_timeout(Some(Duration::from_secs(10)))
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
