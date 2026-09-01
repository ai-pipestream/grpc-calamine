# syntax=docker/dockerfile:1
# SPDX-License-Identifier: Apache-2.0

# ---------------------------------------------------------------------------
# Build stage.
#
# The tests run here, before the binary is built, so a red suite fails the
# image rather than shipping. That is the whole reason this is a multi-stage
# build and not a `COPY` of something built on a laptop: the artifact and the
# evidence for it come out of the same command. The suite streams real
# workbook fixtures (demos/sample-data) and compares every cell against
# calamine itself, so `.dockerignore` keeps that directory in the context.
#
# No protoc and no buf. Code generation happens at development time (see
# buf.gen.yaml) and the generated Rust plus the descriptor set are checked in
# under src/gen/, so the image build needs a Rust toolchain and nothing else.
# ---------------------------------------------------------------------------
FROM dhi.io/rust:1 AS builder

# The hardened toolchain image runs as a nonroot user; the build needs to
# write only under /src and the cargo home, so give it a writable workspace.
USER root
WORKDIR /src
COPY . .

# `--locked` makes the build reproducible and fails loudly if Cargo.lock is out
# of date, rather than quietly resolving to something never tested.
RUN cargo test --release --locked
RUN cargo build --release --locked

# ---------------------------------------------------------------------------
# Runtime stage.
#
# Docker Hardened Images debian-base: glibc and libgcc, no package manager,
# pulls from the docker.io ecosystem (dhi.io) with signed provenance, and
# runs as uid 65532 out of the box. Uploaded workbooks live in memory and
# nothing is written to disk, so the container can and should run with
# `--read-only`.
#
#   docker run --rm --read-only --cap-drop ALL --security-opt no-new-privileges \
#     -p 50062:50062 grpc-calamine
#
# The in-code default listen address predates the fleet port registry and is
# still 50051; the image pins the registered port (50062, see the workspace
# AGENTS.md) through the same env var the binary reads.
#
# There is no health service registered, so checking is the orchestrator's
# job over gRPC reflection or a real RPC rather than a Dockerfile
# HEALTHCHECK, because there is no shell here to run one with.
# ---------------------------------------------------------------------------
FROM dhi.io/debian-base:trixie-debian13

COPY --from=builder /src/target/release/grpc-calamine /usr/local/bin/grpc-calamine

ENV GRPC_CALAMINE_ADDR=0.0.0.0:50062
EXPOSE 50062
USER nonroot
ENTRYPOINT ["/usr/local/bin/grpc-calamine"]
