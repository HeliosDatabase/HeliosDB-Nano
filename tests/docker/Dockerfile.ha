# HeliosDB Nano HA Test Dockerfile
# Builds with HA features for replication testing

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

# Copy Cargo files first for caching
COPY Cargo.toml Cargo.lock* ./
COPY crates ./crates

# Copy source
COPY src ./src
COPY benches ./benches
COPY tests ./tests

# Build with HA features (tier1 only for now)
RUN cargo build --release --features "ha-tier1,encryption,vector-search,ring-crypto"

# Build the proxy binary
RUN cargo build --release -p heliosdb-proxy

# Runtime stage
FROM debian:trixie-slim AS runtime

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    netcat-openbsd \
    procps \
    jq \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -r -s /bin/false heliosdb

# Create directories
RUN mkdir -p /data /config /scripts && chown -R heliosdb:heliosdb /data /config

# Copy binaries from builder
COPY --from=builder /build/target/release/heliosdb-nano /usr/local/bin/
COPY --from=builder /build/target/release/heliosdb-proxy /usr/local/bin/

# Set permissions
RUN chmod +x /usr/local/bin/heliosdb-nano /usr/local/bin/heliosdb-proxy

# Copy entrypoint script
COPY tests/docker/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

# Environment
ENV HELIOSDB_DATA_DIR=/data
ENV HELIOSDB_LOG_LEVEL=info
ENV HELIOSDB_ROLE=primary
ENV HELIOSDB_NODE_ID=
ENV HELIOSDB_PRIMARY_HOST=
ENV HELIOSDB_STANDBYS=
ENV HELIOSDB_OBSERVER_HOSTS=
ENV RUST_BACKTRACE=1

# Expose ports
# 5432: PostgreSQL-compatible protocol
# 5433: Replication streaming
# 8080: HTTP API / Admin
# 9090: Proxy admin
EXPOSE 5432 5433 8080 9090

# Health check
HEALTHCHECK --interval=10s --timeout=5s --start-period=30s --retries=3 \
    CMD curl -f http://localhost:8080/health || exit 1

# Entrypoint
ENTRYPOINT ["/entrypoint.sh"]
CMD ["heliosdb-nano"]
