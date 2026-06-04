# syntax=docker/dockerfile:1.6
#
# Multi-stage Dockerfile for the policast Compose stack.
#
# Build targets:
#   - sidecar: minimal runtime image containing policast-uc-sidecar
#   - demo:    runtime image containing the run_datafusion_uc_http example
#   - shell:   rust toolchain + workspace source, for interactive dev use
#
# Build each via:
#   docker build --target <name> -t policast-<name> -f docker/Dockerfile .

########################
# 1. Common Rust builder
########################
ARG RUST_VERSION=1.90
FROM rust:${RUST_VERSION}-slim-bookworm AS builder

ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_TERM_COLOR=always \
    CARGO_INCREMENTAL=0

RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential \
      pkg-config \
      libssl-dev \
      ca-certificates \
      cmake \
      git \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /workspace

# Optional cargo config from the host. If the repo ships a
# `.cargo/config.toml` (for example a corporate crates.io mirror),
# it is honored inside the image without any URLs being baked into
# this Dockerfile. Users without a custom config can drop the file
# locally; cargo falls back to crates.io. `.cargo/registry` and
# `.cargo/git` are excluded via .dockerignore so we never ship
# cached archives.
COPY .cargo ./.cargo

COPY Cargo.toml Cargo.lock ./
COPY crates/policast-core/Cargo.toml crates/policast-core/Cargo.toml
COPY crates/policast-datafusion/Cargo.toml crates/policast-datafusion/Cargo.toml
COPY crates/policast-uc/Cargo.toml crates/policast-uc/Cargo.toml

COPY crates/policast-core/src crates/policast-core/src
COPY crates/policast-datafusion/src crates/policast-datafusion/src
COPY crates/policast-datafusion/tests crates/policast-datafusion/tests
COPY crates/policast-uc/src crates/policast-uc/src
COPY examples examples
COPY scripts scripts

########################
# 2. Sidecar build stage
########################
FROM builder AS sidecar-build

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/workspace/target \
    cargo build --release \
        -p policast-uc \
        --bin policast-uc-sidecar \
        --features sidecar,uc-bootstrap \
    && cp target/release/policast-uc-sidecar /usr/local/bin/policast-uc-sidecar

#####################
# 3. Sidecar runtime
#####################
FROM debian:bookworm-slim AS sidecar

RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates \
      libssl3 \
      curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system policast \
    && useradd --system --gid policast --home /home/policast --create-home policast

COPY --from=sidecar-build /usr/local/bin/policast-uc-sidecar /usr/local/bin/policast-uc-sidecar

USER policast
WORKDIR /home/policast

EXPOSE 8765

ENTRYPOINT ["/usr/local/bin/policast-uc-sidecar"]
CMD ["--listen", "0.0.0.0:8765", "--store-root", "/data/store", "--signing-secret-env", "POLICAST_UC_SECRET"]

#####################
# 4. Demo build stage
#####################
FROM builder AS demo-build

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/workspace/target \
    cargo build --release \
        -p policast-datafusion \
        --example run_datafusion_uc_http \
        --features "uc delta" \
    && cp target/release/examples/run_datafusion_uc_http /usr/local/bin/run_datafusion_uc_http

##################
# 5. Demo runtime
##################
FROM debian:bookworm-slim AS demo

RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates \
      libssl3 \
      curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=demo-build /usr/local/bin/run_datafusion_uc_http /usr/local/bin/run_datafusion_uc_http
COPY docker/entrypoint-demo.sh /usr/local/bin/entrypoint-demo.sh
RUN chmod +x /usr/local/bin/entrypoint-demo.sh

ENV POLICAST_UC_ENDPOINT=http://sidecar:8765 \
    POLICAST_PRINCIPAL_ID=alice@hospital.com \
    POLICAST_PRINCIPAL_ROLE=analyst \
    POLICAST_PRINCIPAL_REGION=us-east \
    POLICAST_TABLE=hospital.clinical.patients

ENTRYPOINT ["/usr/local/bin/entrypoint-demo.sh"]

#################################
# 6. Interactive dev shell image
#################################
FROM rust:${RUST_VERSION}-bookworm AS shell

ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_TERM_COLOR=always

RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential \
      pkg-config \
      libssl-dev \
      ca-certificates \
      cmake \
      git \
      curl \
      jq \
      less \
      vim-tiny \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /workspace

CMD ["/bin/bash"]
