//! Generated protobuf code for the `calamine.v1` package.
//!
//! The files under `src/gen` are produced by `buf generate` (see
//! `buf.gen.yaml`; never edit them by hand). They are re-generated from
//! `proto/calamine/v1/*.proto`.

/// Messages, enums, client, and server for the `calamine.v1` protobuf
/// package.
///
/// Wire-level documentation lives in the `.proto` files (buf enforces
/// comments on every item there); the generated Rust carries it over where
/// prost supports it.
#[allow(clippy::all, clippy::pedantic, clippy::nursery, missing_docs)]
pub mod v1 {
    // The prost output already ends with `include!("calamine.v1.tonic.rs")`,
    // pulling in the client and server modules.
    include!("gen/calamine/v1/calamine.v1.rs");
}

/// Serialized `FileDescriptorSet` for the `calamine.v1` package, served by
/// the gRPC reflection service.
///
/// Produced by `buf build -o src/gen/calamine/v1/calamine.v1.binpb`; refresh
/// it whenever the protos under `proto/` change, alongside `buf generate`.
pub const FILE_DESCRIPTOR_SET: &[u8] = include_bytes!("gen/calamine/v1/calamine.v1.binpb");
