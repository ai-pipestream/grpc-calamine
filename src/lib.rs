// SPDX-License-Identifier: Apache-2.0

//! A gRPC server that reads Excel and `OpenDocument` workbooks with the Rust
//! `calamine` crate and streams the results back to callers as they are
//! parsed.
//!
//! Design rules:
//! - **No disk I/O for workbook content.** Uploaded bytes live in memory
//!   (`Cursor<Vec<u8>>`) and are parsed with
//!   `calamine::open_workbook_auto_from_rs`.
//! - **Streaming reads.** XLSX and XLSB sheets are parsed incrementally via
//!   calamine's cell readers; XLS and ODS are range-parsed and then streamed
//!   row by row. Every RPC is safe to run concurrently against many open
//!   workbooks.
//! - **One-to-one contract.** The protobuf model in `proto/calamine/v1`
//!   mirrors calamine's public types exactly; conversions in [`convert`] are
//!   total and lossless.

pub mod convert;
pub mod proto;
pub mod service;
pub mod store;

pub use service::CalamineGrpc;
pub use store::WorkbookStore;
