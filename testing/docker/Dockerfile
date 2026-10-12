# syntax=docker/dockerfile:1
# One build of the TollGate binaries, reused by every topology.
#
# Stage 1 compiles the workspace; stage 2 is a slim runtime carrying the two
# binaries and the tools a node needs at run time. All compose roles run this
# same image with different configs — nothing is rebuilt per role.
#
# Build context is the repo root (see scripts/build.sh).
#
# Caching: BuildKit cache mounts persist the cargo registry, the git checkouts
# and target/ across builds, so a source edit recompiles only what changed. The
# `cp` out of target/ happens in the same RUN because cache-mount contents do
# not survive into the image layer.

# ---- Stage 1: build ----
FROM rust:1.90-slim-trixie AS builder

WORKDIR /src

# `git` because the Cashu dependencies are git dependencies, and
# `protobuf-compiler` because cdk's signatory reaches gRPC through prost, whose
# build script shells out to protoc.
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        pkg-config git ca-certificates protobuf-compiler && \
    rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --bin tollgated --bin tolltop && \
    cp target/release/tollgated /tollgated && \
    cp target/release/tolltop /tolltop

# ---- Stage 2: runtime ----
FROM debian:trixie-slim

# nftables and iproute2 are what the IP adapter gates and shapes with. curl
# reaches the mint and market over HTTP; the control socket speaks raw JSON
# rather than HTTP, so netcat reads that one. jq does the assertions.
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        nftables iproute2 iputils-ping curl ca-certificates jq netcat-openbsd && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /tollgated /usr/local/bin/tollgated
COPY --from=builder /tolltop /usr/local/bin/tolltop

ENTRYPOINT ["tollgated"]
