# Build arguments:
#
# - `git_commit` is the hash of the current git commit — used for
#   versioning information inside the binaries.
# - `build_date` is the date and time of when the docker image was
#   built
# - `binaries` is the path to the directory containing the Linera
#   binaries. Leave unset to build the binaries from scratch.
# - `target` is a Rust target quadruple. Currently known to be
#   supported are `x86_64-unknown-linux-gnu` and
#   `aarch64-unknown-linux-gnu`.
# - `build_flag` is either "--release" or an empty string.
# - `build_folder` is either "release" or "debug".
# - `build_features` is a comma-separated list of features to build the binaries with.

# Stage 1 - Generate recipe file for dependencies

# Modern Docker provides TARGETPLATFORM and friends, but these do not
# provide enough information to reconstruct the whole Rust target and
# switching on them to map to a subset of targets seems unwise
ARG git_commit
ARG build_date
ARG target=x86_64-unknown-linux-gnu
ARG binaries=
ARG copy=${binaries:+_copy}
ARG build_flag=--release
ARG build_folder=release
ARG build_features=scylladb,metrics
ARG rustflags="-C force-frame-pointers=yes"

FROM rust:1.74-slim-bookworm AS builder
ARG git_commit
ARG target
ARG build_flag
ARG build_folder
ARG build_features
ARG rustflags

RUN apt-get update && apt-get install -y \
    pkg-config \
    protobuf-compiler \
    clang

COPY examples examples
COPY linera-base linera-base
COPY linera-chain linera-chain
COPY linera-client linera-client
COPY linera-core linera-core
COPY linera-ethereum linera-ethereum
COPY linera-execution linera-execution
COPY linera-explorer linera-explorer
COPY linera-faucet linera-faucet
COPY linera-indexer linera-indexer
COPY linera-metrics linera-metrics
COPY linera-rpc linera-rpc
COPY linera-sdk linera-sdk
COPY linera-sdk-derive linera-sdk-derive
COPY linera-service linera-service
COPY linera-service-graphql-client linera-service-graphql-client
COPY linera-storage linera-storage
COPY linera-storage-service linera-storage-service
COPY linera-summary linera-summary
COPY linera-persistent linera-persistent
COPY linera-version linera-version
COPY linera-views linera-views
COPY linera-views-derive linera-views-derive
COPY linera-web linera-web
COPY linera-witty linera-witty
COPY linera-witty-macros linera-witty-macros
COPY scripts scripts
COPY rust-toolchain* Cargo.* ./

ENV GIT_COMMIT=${git_commit}
ENV RUSTFLAGS=${rustflags}

RUN cargo build ${build_flag:+"$build_flag"} \
    --target "$target" \
    --bin linera \
    --bin linera-proxy \
    --bin linera-server \
    --features $build_features

RUN mv \
    target/"$target"/"$build_folder"/linera \
    target/"$target"/"$build_folder"/linera-proxy \
    target/"$target"/"$build_folder"/linera-server \
    ./

# Optionally copy binaries instead of using the build images above
FROM scratch AS builder_copy
ARG binaries
COPY \
    "$binaries"/linera \
    "$binaries"/linera-server \
    "$binaries"/linera-proxy \
    ./

FROM builder$copy AS binaries

# Setup running environment for container
FROM debian:latest

ARG git_commit
LABEL git_commit=$git_commit

ARG build_date
LABEL build_date=$build_date

RUN apt-get update && apt-get install -y \
    ca-certificates \
    openssl
RUN update-ca-certificates

ARG target

COPY --from=binaries \
    linera \
    linera-server \
    linera-proxy \
    ./

COPY --chmod=755 \
    docker/server-entrypoint.sh \
    docker/server-init.sh \
    docker/proxy-entrypoint.sh \
    docker/proxy-init.sh \
    docker/compose-server-entrypoint.sh \
    docker/compose-proxy-entrypoint.sh \
    docker/compose-server-init.sh \
    ./
