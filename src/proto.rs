//! Generated protobuf code for the `calamine.v1` package.
//!
//! The files under `src/gen` are produced by `buf generate` (see
//! `buf.gen.yaml`; never edit them by hand). They are re-generated from
//! `proto/calamine/v1/*.proto`.

/// Messages, enums, client, and server for the `calamine.v1` protobuf
/// package.
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
pub mod v1 {
    // The prost output already ends with `include!("calamine.v1.tonic.rs")`,
    // pulling in the client and server modules.
    include!("gen/calamine/v1/calamine.v1.rs");
}
