// SPDX-License-Identifier: Apache-2.0

//! Binary entry point for the calamine gRPC server.
//!
//! Runtime sizing (all optional environment overrides):
//! - `GRPC_CALAMINE_ADDR`: listen address (default `0.0.0.0:50062`).
//! - `GRPC_CALAMINE_WORKERS`: tokio worker threads (default: CPU count).
//! - `GRPC_CALAMINE_BLOCKING_THREADS`: cap of the blocking pool that runs
//!   calamine parsing (default: 512, tokio's own default).
//! - `GRPC_CALAMINE_WINDOW_BYTES`: HTTP/2 initial stream and connection
//!   window (default: 50 MiB).
//! - `GRPC_CALAMINE_MAX_CONCURRENT_STREAMS`: streaming reads admitted at
//!   once (default: 128). Past the cap a read is refused with
//!   `RESOURCE_EXHAUSTED` rather than queued.
//! - `GRPC_CALAMINE_MAX_CONCURRENT_UPLOADS`: `OpenWorkbook` uploads admitted
//!   at once (default: 16), refused past the cap the same way.
//! - `GRPC_CALAMINE_MAX_OPEN_WORKBOOKS`: workbooks open at once (default:
//!   256). Opening one more is refused with `RESOURCE_EXHAUSTED`.
//! - `GRPC_CALAMINE_MAX_STORE_BYTES`: bytes the open workbooks may hold
//!   together (default: 2 GiB), refused past it the same way.
//! - `GRPC_CALAMINE_UPLOAD_DEADLINE_SECS`: seconds one whole `OpenWorkbook`
//!   upload may take before it is abandoned with `DEADLINE_EXCEEDED`
//!   (default: 600; 0 never).
//! - `GRPC_CALAMINE_HANDLE_TTL_SECS`: seconds a workbook may go unused
//!   before it is closed for its client (default: 300; 0 never expires).
//! - `GRPC_CALAMINE_MAX_PICTURE_BYTES`: largest embedded picture, inflated,
//!   that opening a workbook may read (default: 64 MiB).
//! - `GRPC_CALAMINE_MAX_PICTURE_TOTAL_BYTES`: most inflated bytes of one
//!   workbook's pictures together (default: 256 MiB).
//! - `GRPC_CALAMINE_MAX_SHARED_STRINGS_BYTES`: largest shared-string table,
//!   inflated (default: 256 MiB). A workbook past any of the three is refused
//!   at `OpenWorkbook` with `RESOURCE_EXHAUSTED`.
//! - `GRPC_CALAMINE_FORMATS`: comma-separated workbook formats accepted, any
//!   of `xlsx`, `xlsb`, `xls` and `ods` (default: all four). A refused format
//!   is never opened: named in the options it fails `OpenWorkbook` with
//!   `FAILED_PRECONDITION`, and auto-detection never tries it.
//! - `GRPC_CALAMINE_MAX_DENSE_CELLS`: most cells, counted from column A, that
//!   one formula stream or XLS/ODS stream may densify (default: 33554432).
//!   A larger range is refused with `RESOURCE_EXHAUSTED` before its first
//!   event.
//! - `GRPC_CALAMINE_MAX_FORMULA_BYTES`: most bytes of formulas, as calamine
//!   expands them, that one xlsx or xlsb formula stream may collect before it
//!   sends (default: 536870912). Past it the stream is refused with
//!   `RESOURCE_EXHAUSTED` before its first event.

use std::time::Duration;

use tonic::transport::Server;

use grpc_calamine::archive::InflateLimits;
use grpc_calamine::store::{FormatSet, StoreLimits};
use grpc_calamine::{CalamineGrpc, WorkbookStore, proto};

/// Default listen address when `GRPC_CALAMINE_ADDR` is not set.
const DEFAULT_ADDR: &str = "0.0.0.0:50062";

/// Default HTTP/2 initial window, for both the stream and the connection.
///
/// hyper's own default is 1 MiB. Workbook uploads are bulk transfers of tens
/// or hundreds of megabytes, so a wide window keeps them from being paced at
/// one window per round trip over any link with real latency.
const DEFAULT_WINDOW_BYTES: u32 = 50 * 1024 * 1024;

/// An environment variable that is set but cannot be used.
///
/// Every variable here is a limit or a size, and one that does not parse is a
/// mistake in the deployment: falling back to the default would quietly run
/// the server with a bound its operator did not choose, so startup fails
/// instead, naming the variable.
#[derive(Debug)]
struct BadEnv {
    name: &'static str,
    value: String,
    reason: String,
}

impl std::fmt::Display for BadEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is set to {:?}, which is not valid: {}",
            self.name, self.value, self.reason
        )
    }
}

impl std::error::Error for BadEnv {}

/// Parse `raw`, the value of environment variable `name`: `None` when it is
/// unset, the parsed value when it parses, and an error otherwise.
fn parse_env<T>(name: &'static str, raw: Option<std::ffi::OsString>) -> Result<Option<T>, BadEnv>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let Some(raw) = raw else {
        return Ok(None);
    };
    let value = raw.into_string().map_err(|raw| BadEnv {
        name,
        value: raw.to_string_lossy().into_owned(),
        reason: "not valid UTF-8".to_string(),
    })?;
    value.trim().parse().map(Some).map_err(|e: T::Err| BadEnv {
        name,
        reason: e.to_string(),
        value,
    })
}

/// Read environment variable `name`; see [`parse_env`].
fn env<T>(name: &'static str) -> Result<Option<T>, BadEnv>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    parse_env(name, std::env::var_os(name))
}

/// Read environment variable `name`, or `default` when it is unset.
fn env_or<T>(name: &'static str, default: T) -> Result<T, BadEnv>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    Ok(env(name)?.unwrap_or(default))
}

/// Run the server, reporting a startup failure in words rather than as the
/// `Debug` form `main` would print for a returned error.
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("grpc-calamine: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let workers = env_or(
        "GRPC_CALAMINE_WORKERS",
        std::thread::available_parallelism().map_or(4, usize::from),
    )?;
    let blocking = env_or("GRPC_CALAMINE_BLOCKING_THREADS", 512_usize)?;

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
    let addr: std::net::SocketAddr = env_or("GRPC_CALAMINE_ADDR", DEFAULT_ADDR.parse()?)?;

    let defaults = StoreLimits::default();
    let limits = StoreLimits {
        max_open_workbooks: env_or(
            "GRPC_CALAMINE_MAX_OPEN_WORKBOOKS",
            defaults.max_open_workbooks,
        )?,
        max_store_bytes: env_or("GRPC_CALAMINE_MAX_STORE_BYTES", defaults.max_store_bytes)?,
        idle_ttl: Duration::from_secs(env_or(
            "GRPC_CALAMINE_HANDLE_TTL_SECS",
            defaults.idle_ttl.as_secs(),
        )?),
        inflate: InflateLimits {
            max_picture_bytes: env_or(
                "GRPC_CALAMINE_MAX_PICTURE_BYTES",
                defaults.inflate.max_picture_bytes,
            )?,
            max_picture_total_bytes: env_or(
                "GRPC_CALAMINE_MAX_PICTURE_TOTAL_BYTES",
                defaults.inflate.max_picture_total_bytes,
            )?,
            max_shared_strings_bytes: env_or(
                "GRPC_CALAMINE_MAX_SHARED_STRINGS_BYTES",
                defaults.inflate.max_shared_strings_bytes,
            )?,
        },
    };

    // Streaming reads are capped well below the blocking pool so they can
    // never take every thread and leave uploads with none.
    let formats = env_or("GRPC_CALAMINE_FORMATS", FormatSet::ALL)?;
    let mut grpc = CalamineGrpc::new(WorkbookStore::with_limits(limits).with_formats(formats));
    if let Some(max) = env("GRPC_CALAMINE_MAX_CONCURRENT_STREAMS")? {
        grpc = grpc.with_max_concurrent_streams(max);
    }
    if let Some(max) = env("GRPC_CALAMINE_MAX_CONCURRENT_UPLOADS")? {
        grpc = grpc.with_max_concurrent_uploads(max);
    }
    if let Some(max) = env("GRPC_CALAMINE_MAX_DENSE_CELLS")? {
        grpc = grpc.with_max_dense_cells(max);
    }
    if let Some(secs) = env("GRPC_CALAMINE_UPLOAD_DEADLINE_SECS")? {
        grpc = grpc.with_upload_deadline(Duration::from_secs(secs));
    }
    if let Some(max) = env("GRPC_CALAMINE_MAX_FORMULA_BYTES")? {
        grpc = grpc.with_max_formula_bytes(max);
    }
    // Detached on purpose: it ends by itself once the service is dropped.
    let _reaper = grpc.spawn_reaper();
    let service = grpc.into_service();

    // Reflection lets tooling such as grpcurl discover the service without a
    // local copy of the protos.
    let reflection = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(proto::FILE_DESCRIPTOR_SET)
        .build_v1()?;

    // HTTP/2 flow control is directional: this governs what the server
    // *receives*, so it sizes the `OpenWorkbook` upload, not the row stream.
    // A client that wants a wide download window has to set its own; hyper
    // defaults both to 1 MiB, which throttles a bulk transfer to one window
    // per round trip once there is real latency in the path.
    let window: u32 = env_or("GRPC_CALAMINE_WINDOW_BYTES", DEFAULT_WINDOW_BYTES)?;

    eprintln!("grpc-calamine listening on {addr} (http2 window {window} bytes)");
    eprintln!(
        "grpc-calamine holds at most {} workbooks and {} bytes; idle workbooks close after {}s",
        limits.max_open_workbooks,
        limits.max_store_bytes,
        limits.idle_ttl.as_secs()
    );
    eprintln!("grpc-calamine accepts {formats} workbooks");
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
        .add_service(reflection)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unset_variable_is_none() {
        assert_eq!(parse_env::<u64>("X", None).expect("unset is fine"), None);
    }

    #[test]
    fn a_set_variable_parses() {
        assert_eq!(
            parse_env::<u64>("X", Some("536870912".into())).expect("parses"),
            Some(536_870_912)
        );
        assert_eq!(
            parse_env::<usize>("X", Some(" 16 ".into()))
                .expect("surrounding space is not a mistake"),
            Some(16)
        );
    }

    /// A value that does not parse is an error naming the variable, never
    /// the default.
    #[test]
    fn a_variable_that_does_not_parse_is_refused() {
        let err = parse_env::<u64>("GRPC_CALAMINE_MAX_STORE_BYTES", Some("2GiB".into()))
            .expect_err("2GiB is not a byte count");
        let message = err.to_string();
        assert!(
            message.contains("GRPC_CALAMINE_MAX_STORE_BYTES"),
            "{message}"
        );
        assert!(message.contains("2GiB"), "{message}");

        parse_env::<usize>("X", Some("-1".into())).expect_err("negative");
        parse_env::<u32>("X", Some("4294967296".into())).expect_err("past u32");
        parse_env::<u64>("X", Some(String::new().into())).expect_err("empty");
    }

    #[test]
    fn a_format_list_parses_or_names_the_variable() {
        let formats = parse_env::<FormatSet>("GRPC_CALAMINE_FORMATS", Some("xlsx,xlsb".into()))
            .expect("parses")
            .expect("set");
        assert_eq!(formats.to_string(), "xlsx,xlsb");
        let err = parse_env::<FormatSet>("GRPC_CALAMINE_FORMATS", Some("xlsx,csv".into()))
            .expect_err("csv is not a workbook format");
        assert!(err.to_string().contains("GRPC_CALAMINE_FORMATS"), "{err}");
    }
}
