# syntax=docker/dockerfile:1.6
#
# Multi-stage Dockerfile for the policast Compose stack.
#
# Build targets:
#   - sidecar:    minimal runtime image containing policast-uc-sidecar
#   - demo:       runtime image containing the run_datafusion_uc_http example
#   - shell:      rust toolchain + workspace source, for interactive dev use
#   - spark-build: Scala 2.13 / sbt stage that assembles the policast-spark jar
#   - spark-demo:  Spark 4.1 (Scala 2.13, Java 17) runtime that runs RunSpark
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

###############################################
# 7. Spark plugin build stage (Scala 2.13 / sbt)
###############################################
FROM eclipse-temurin:17-jdk-jammy AS spark-build

ARG SBT_VERSION=1.12.4
# Optional corporate Maven mirror. When set, ALL sbt resolution is routed
# through it: the launcher boot (sbt + Scala themselves), plugins, and project
# dependencies. Locked-down build networks frequently block Maven Central, so
# build.sbt's resolver list is not enough — the launcher boots before build.sbt
# is read. This is the JVM analogue of .cargo/config.toml for the Rust stages.
# When empty, sbt falls back to its public default repositories.
ARG MAVEN_PROXY_URL=
ENV DEBIAN_FRONTEND=noninteractive \
    MAVEN_PROXY_URL=${MAVEN_PROXY_URL}

RUN apt-get update && apt-get install -y --no-install-recommends \
      curl ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && curl -fL "https://github.com/sbt/sbt/releases/download/v${SBT_VERSION}/sbt-${SBT_VERSION}.tgz" \
       | tar -xz -C /opt \
    && ln -s /opt/sbt/bin/sbt /usr/local/bin/sbt

WORKDIR /build
COPY policast-spark/build.sbt ./build.sbt
COPY policast-spark/project/plugins.sbt policast-spark/project/build.properties ./project/
COPY policast-spark/src ./src

# Build the assembly jar. With a proxy, write the sbt boot-repositories file and
# override build-defined repos so even the launcher boot resolves via the mirror.
RUN set -eux; \
    if [ -n "${MAVEN_PROXY_URL}" ]; then \
      mkdir -p /root/.sbt; \
      { echo "[repositories]"; \
        echo "  local"; \
        echo "  maven-central: ${MAVEN_PROXY_URL}"; \
        echo "  sbt-plugins: ${MAVEN_PROXY_URL}, [organization]/[module]/(scala_[scalaVersion]/)(sbt_[sbtVersion]/)[revision]/[type]s/[artifact](-[classifier]).[ext]"; \
      } > /root/.sbt/repositories; \
      export SBT_OPTS="-Dsbt.repository.config=/root/.sbt/repositories -Dsbt.override.build.repos=true"; \
    fi; \
    sbt assembly

###############################################
# 8. Spark demo runtime (Spark 4.1, Scala 2.13, Java 17)
###############################################
FROM eclipse-temurin:17-jre-jammy AS spark-demo

ARG SPARK_VERSION=4.1.2
ENV DEBIAN_FRONTEND=noninteractive \
    SPARK_HOME=/opt/spark \
    PATH=/opt/spark/bin:${PATH}

# Spark 4.x is Scala 2.13-only (no -scala2.13 suffix) and requires Java 17+.
RUN apt-get update && apt-get install -y --no-install-recommends \
      curl ca-certificates procps \
    && rm -rf /var/lib/apt/lists/* \
    && curl -fL "https://archive.apache.org/dist/spark/spark-${SPARK_VERSION}/spark-${SPARK_VERSION}-bin-hadoop3.tgz" \
       | tar -xz -C /opt \
    && ln -s "/opt/spark-${SPARK_VERSION}-bin-hadoop3" /opt/spark

COPY --from=spark-build /build/target/scala-2.13/policast-spark-assembly-0.1.0.jar \
     /opt/policast/policast-spark-assembly.jar
# Ship the pre-expanded manifest + sample data the plugin enforces against.
# Spark has no tag-expansion step (the resolver sidecar does that for the
# DataFusion path), so it consumes the already-expanded fixture with concrete
# table/column targets — not the tag-templated `just compile` output.
COPY policast-spark/src/main/resources/examples /opt/policast/examples
COPY docker/entrypoint-spark.sh /usr/local/bin/entrypoint-spark.sh
RUN chmod +x /usr/local/bin/entrypoint-spark.sh

ENV POLICAST_SPARK_JAR=/opt/policast/policast-spark-assembly.jar \
    POLICAST_MANIFEST_PATH=/opt/policast/examples/policies/manifest.json \
    POLICAST_DATA_PATH=/opt/policast/examples/data/patients.csv

WORKDIR /opt/policast
ENTRYPOINT ["/usr/local/bin/entrypoint-spark.sh"]
