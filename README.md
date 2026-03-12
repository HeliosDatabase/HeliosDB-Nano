# HeliosDB Nano

[![Crates.io](https://img.shields.io/crates/v/heliosdb-nano.svg)](https://crates.io/crates/heliosdb-nano)
[![Documentation](https://docs.rs/heliosdb-nano/badge.svg)](https://docs.rs/heliosdb-nano)
[![License](https://img.shields.io/crates/l/heliosdb-nano.svg)](LICENSE)

**PostgreSQL-compatible embedded database for Rust** with HNSW vector search, AES-256-GCM encryption, git-like branching, time-travel queries, and 50+ enterprise features. Single binary, zero external dependencies.

HeliosDB Nano combines the simplicity of SQLite (embed it in your app) with PostgreSQL compatibility (connect with `psql`, any PG driver, or any ORM). Built on RocksDB + Apache Arrow.

## Installation

```toml
[dependencies]
heliosdb-nano = "3.7"
```

## Quick Start

```rust
use heliosdb_nano::EmbeddedDatabase;

fn main() -> heliosdb_nano::Result<()> {
    // Persistent on disk
    let db = EmbeddedDatabase::new("./mydb.helio")?;
    // Or fully in-memory: EmbeddedDatabase::new_in_memory()?

    db.execute("CREATE TABLE items (id INT PRIMARY KEY, name TEXT, price DECIMAL(10,2))")?;
    db.execute("INSERT INTO items VALUES (1, 'Widget', 9.99)")?;

    let rows = db.query("SELECT * FROM items WHERE price < 20", &[])?;
    for row in &rows {
        println!("{:?}", row);
    }

    // Parameterized queries
    use heliosdb_nano::Value;
    let rows = db.query_params("SELECT * FROM items WHERE id = $1", &[Value::Int4(1)])?;

    Ok(())
}
```

## Feature Highlights

### Vector Search (HNSW + Product Quantization)

Native HNSW indexes with cosine, L2, and inner product distance. Optional Product Quantization for 8-16x memory reduction.

```rust
db.execute("CREATE TABLE docs (id INT PRIMARY KEY, title TEXT, embedding VECTOR(1536))")?;
db.execute("INSERT INTO docs VALUES (1, 'Hello', '[0.1, 0.2, ...]')")?;

// K-nearest-neighbor search
let results = db.query(
    "SELECT title, embedding <-> '[0.15, 0.25, ...]' AS distance
     FROM docs ORDER BY embedding <-> '[0.15, 0.25, ...]' LIMIT 10",
    &[],
)?;
```

Distance operators: `<->` (cosine), `<~>` (L2/Euclidean), `<#>` (inner product).

### Git-Like Branching

Create isolated database branches for dev/test/A/B experiments. Copy-on-write keeps branches lightweight.

```rust
db.execute("CREATE BRANCH staging")?;
db.execute("USE BRANCH staging")?;
db.execute("INSERT INTO items VALUES (99, 'Test', 0.01)")?;  // isolated to staging
db.execute("MERGE BRANCH staging INTO main")?;
db.execute("DROP BRANCH staging")?;
```

### Time-Travel Queries

Query any table at a previous point in time:

```rust
let history = db.query(
    "SELECT * FROM items AS OF TIMESTAMP '2024-06-01 00:00:00'", &[],
)?;
// Also: AS OF TRANSACTION 12345, AS OF SCN 999999
```

### Encryption

- **TDE**: AES-256-GCM at-rest encryption with automatic key rotation
- **ZKE**: Zero-Knowledge Encryption (client-side, server never sees plaintext)
- **FIPS 140-3**: Build with `--features fips` for AWS-LC certified cryptography

### Materialized Views

```rust
db.execute(
    "CREATE MATERIALIZED VIEW sales_summary AS
     SELECT product_id, SUM(amount) AS total FROM orders GROUP BY product_id"
)?;
db.execute("REFRESH MATERIALIZED VIEW sales_summary")?;
```

### Additional Features

- **Full SQL**: JOINs, CTEs, window functions, subqueries, set operations, aggregates, CASE, DISTINCT
- **PL/pgSQL**: Stored procedures and functions with multi-dialect support
- **JSONB**: Field access (`->`/`->>`), containment (`@>`), key existence (`?`)
- **Foreign keys**: CASCADE, SET NULL, RESTRICT
- **Triggers**: BEFORE/AFTER INSERT/UPDATE/DELETE
- **Row-Level Security**: Column masking and row filtering per tenant
- **EXPLAIN**: Cost-based optimizer with ANALYZE, BUFFERS, JSON/XML/YAML output
- **Backup/Restore**: Compressed dumps (zstd/gzip/brotli), incremental backups
- **Import/Export**: CSV, JSON, JSONL, Parquet, Arrow, SQL formats
- **Audit logging**: Tamper-proof trail with SHA-256 checksums

## Data Types

| Type | Aliases |
|------|---------|
| `BOOLEAN` | `BOOL` |
| `SMALLINT` / `INTEGER` / `BIGINT` | `INT2` / `INT4` / `INT8` |
| `REAL` / `DOUBLE PRECISION` | `FLOAT4` / `FLOAT8` |
| `NUMERIC` | `DECIMAL(p,s)` |
| `TEXT` / `VARCHAR(n)` | `CHARACTER VARYING` |
| `BYTEA` | `BLOB` |
| `DATE` / `TIME` / `TIMESTAMP` | `TIMESTAMPTZ` |
| `UUID` | |
| `JSON` / `JSONB` | |
| `VECTOR(n)` | |
| `ARRAY` | `INT[]`, `TEXT[]` |

## Server Mode

HeliosDB Nano also runs as a standalone PostgreSQL-compatible server:

```bash
# Start server (persistent)
heliosdb-nano start --data-dir ./mydata

# Start server (in-memory)
heliosdb-nano start --memory

# With SCRAM-SHA-256 authentication
heliosdb-nano start --data-dir ./mydata --auth scram-sha-256 --password s3cret

# With TLS
heliosdb-nano start --data-dir ./mydata --tls-cert cert.pem --tls-key key.pem
```

Connect with any PostgreSQL client:

```bash
psql -h 127.0.0.1 -p 5432
```

Works with every PostgreSQL driver and ORM: libpq, psycopg2, JDBC, Npgsql, node-postgres, etc.

## REST API

When running in server mode, an HTTP API is available on port 8080:

```bash
# Execute SQL
curl -X POST http://localhost:8080/api/query \
  -H 'Content-Type: application/json' \
  -d '{"sql": "SELECT * FROM users LIMIT 10"}'

# Vector search
curl -X POST http://localhost:8080/api/vectors/search \
  -H 'Content-Type: application/json' \
  -d '{"collection": "docs", "query": [0.1, 0.2], "k": 5, "metric": "cosine"}'
```

Endpoints: `/api/query`, `/api/data/:table`, `/api/vectors/search`, `/api/branches`, `/api/schema`, `/health`, and more.

## High Availability (Feature Flags)

All HA features are optional and compiled via Cargo feature flags:

```bash
cargo build --release --features ha-standard   # tier1 + tier2 + proxy + transaction replay
cargo build --release --features ha-full        # all HA features
```

| Flag | Description |
|------|-------------|
| `ha-tier1` | Warm standby: WAL streaming, automatic failover, read replicas |
| `ha-tier2` | Multi-primary: active-active with branch-based conflict resolution |
| `ha-tier3` | Sharding: consistent hash ring, cross-shard queries |
| `ha-proxy` | Connection router with load balancing |
| `ha-tr` | Transaction Replay: journaling, cursor restore, session migration |
| `ha-standard` | Bundle: tier1 + tier2 + proxy + tr |
| `ha-full` | All HA features |

## Building from Source

**Prerequisites:** Rust 1.75+, C/C++ compiler (for RocksDB). Add clang + LLVM for FIPS builds.

```bash
# Default (ring crypto + encryption + vector search)
cargo build --release

# FIPS 140-3 compliant
cargo build --release --no-default-features --features fips,encryption,vector-search

# Run tests
cargo test --lib               # ~1400 unit tests
cargo test --test '*'           # ~800 integration tests
```

## Architecture

| Layer | Technology |
|-------|-----------|
| Storage engine | RocksDB (LSM-tree) |
| Columnar format | Apache Arrow |
| SQL parser | sqlparser-rs |
| Vector index | HNSW (Hierarchical Navigable Small World) |
| Encryption | AES-256-GCM / AWS-LC (FIPS) |
| Wire protocol | PostgreSQL v3 |
| HTTP server | Axum |

## License

[AGPL-3.0-only](LICENSE) (GNU Affero General Public License v3)
