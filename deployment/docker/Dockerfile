# HeliosDB Nano Production Dockerfile
# Multi-stage build for minimal image size

# Build stage
FROM rust:latest AS builder

WORKDIR /build

# Install build dependencies
RUN apt-get update && apt-get install -y \
    cmake \
    clang \
    libclang-dev \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy all source files needed to build the root package.
#   Cargo.lock        — reproducible dependency versions (--locked)
#   bindings          — workspace member (bindings/python); cargo refuses to
#                       load the workspace if a member manifest is missing
#   benches, examples — [[bench]]/[[example]] targets are declared in
#                       Cargo.toml, so the manifest fails to resolve if the
#                       files are absent
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY benches ./benches
COPY examples ./examples
COPY bindings ./bindings

# Build final binary with needed features
RUN cargo build --release --locked --features "encryption,vector-search,ha-tier1"

# Runtime stage — same layout as Dockerfile.binary (the published GHCR image)
FROM debian:trixie-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    libssl3t64 \
    openssl \
    && rm -rf /var/lib/apt/lists/*

# Non-root user with a fixed uid/gid (999) and the data directory
RUN groupadd -r -g 999 heliosdb \
    && useradd -r -u 999 -g heliosdb -d /data -s /usr/sbin/nologin heliosdb \
    && mkdir -p /data && chown heliosdb:heliosdb /data

COPY --from=builder /build/target/release/heliosdb-nano /usr/local/bin/heliosdb-nano
COPY deployment/docker/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod 755 /usr/local/bin/heliosdb-nano /usr/local/bin/docker-entrypoint.sh

USER heliosdb
WORKDIR /data
VOLUME ["/data"]

ENV HELIOSDB_DATA_DIR=/data
ENV HELIOSDB_LOG_LEVEL=info
ENV RUST_BACKTRACE=1

# 5432 PostgreSQL, 8080 HTTP API (/health), 3306 MySQL (with --mysql)
EXPOSE 5432 8080 3306

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/health >/dev/null || exit 1

# The entrypoint wires HELIOSDB_PASSWORD(_FILE) into scram-sha-256 auth and
# generates a self-signed TLS certificate on first start — see the script.
ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["start", "--data-dir", "/data", "--listen", "0.0.0.0", "--port", "5432"]
