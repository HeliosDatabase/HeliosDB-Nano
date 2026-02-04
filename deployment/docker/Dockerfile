# HeliosDB-Lite Production Dockerfile
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

# Copy all source files
COPY Cargo.toml ./
COPY src ./src
COPY benches ./benches
COPY crates ./crates

# Build final binary with needed features
RUN cargo build --release --features "encryption,vector-search"

# Runtime stage
FROM debian:trixie-slim AS runtime

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -r -s /bin/false heliosdb

# Create data directory
RUN mkdir -p /data && chown heliosdb:heliosdb /data

# Copy binary from builder
COPY --from=builder /build/target/release/heliosdb-lite /usr/local/bin/

# Set permissions
RUN chmod +x /usr/local/bin/heliosdb-lite

# Switch to non-root user
USER heliosdb

# Set environment
ENV HELIOSDB_DATA_DIR=/data
ENV HELIOSDB_LOG_LEVEL=info
ENV RUST_BACKTRACE=1

# Expose ports
EXPOSE 5432 8080

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8080/health || exit 1

# Default command - start server listening on all interfaces
ENTRYPOINT ["heliosdb-lite"]
CMD ["start", "--data-dir", "/data", "--listen", "0.0.0.0", "--port", "5432"]
