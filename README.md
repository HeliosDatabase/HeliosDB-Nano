# HeliosDB Nano

**PostgreSQL-compatible embedded database with vector search, encryption, git-like branching, time-travel queries, and 50+ enterprise features.**

[![License](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2021-orange.svg)](https://www.rust-lang.org)
[![Version](https://img.shields.io/badge/version-3.7.0-green.svg)](Cargo.toml)

HeliosDB Nano is a single-binary, zero-dependency database engine that speaks the PostgreSQL wire protocol. Connect with `psql`, any PostgreSQL driver, or use it as an embedded Rust library. It combines the simplicity of SQLite with enterprise features typically found only in distributed systems.

---

## Table of Contents

- [Quick Start](#quick-start)
- [Connection Protocols](#connection-protocols)
- [SQL Reference](#sql-reference)
- [Vector Search](#vector-search)
- [Git-Like Branching](#git-like-branching)
- [Time-Travel Queries](#time-travel-queries)
- [Materialized Views](#materialized-views)
- [Encryption & Security](#encryption--security)
- [EXPLAIN & Query Optimizer](#explain--query-optimizer)
- [Import / Export & Backup](#import--export--backup)
- [CLI & REPL](#cli--repl)
- [REST API](#rest-api)
- [Embedded Library Usage](#embedded-library-usage)
- [High Availability](#high-availability)
- [Configuration Reference](#configuration-reference)
- [Building from Source](#building-from-source)

---

## Quick Start

### Start the server

```bash
# Persistent storage
heliosdb-nano start --data-dir ./mydata

# In-memory (no disk)
heliosdb-nano start --memory

# With authentication
heliosdb-nano start --data-dir ./mydata --auth scram-sha-256 --password s3cret
```

### Connect with any PostgreSQL client

```bash
psql -h 127.0.0.1 -p 5432
```

```sql
CREATE TABLE users (
    id    INT PRIMARY KEY,
    name  TEXT NOT NULL,
    email VARCHAR(255) UNIQUE
);

INSERT INTO users VALUES (1, 'Alice', 'alice@example.com');
SELECT * FROM users;
```

### Interactive REPL (embedded, no server)

```bash
heliosdb-nano repl --data-dir ./mydata
heliosdb-nano repl --memory          # ephemeral in-memory session
```

---

## Connection Protocols

### PostgreSQL Wire Protocol (primary)

Full PostgreSQL v3 wire protocol. Works with every PostgreSQL client, driver, and ORM.

| Setting | Default |
|---------|---------|
| Host | `127.0.0.1` |
| Port | `5432` |
| Auth methods | `trust`, `password`, `md5`, `scram-sha-256` |
| TLS | Optional (rustls, TLS 1.2+) |

**Connection strings:**

```
psql -h 127.0.0.1 -p 5432
postgresql://127.0.0.1:5432/heliosdb          # libpq / psycopg2
jdbc:postgresql://127.0.0.1:5432/heliosdb      # JDBC
Host=127.0.0.1;Port=5432;                      # Npgsql (.NET)
```

**TLS:**

```bash
heliosdb-nano start --data-dir ./mydata \
    --tls-cert /path/to/cert.pem \
    --tls-key  /path/to/key.pem
```

### REST / HTTP API

Axum-based async HTTP server on a configurable port (default `8080`). Endpoints include `/api/query`, `/api/data`, `/api/vectors`, `/api/branches`, `/api/schema`, and more. See [REST API](#rest-api).

### Oracle TNS (experimental)

Optional Oracle-compatible listener on port `1521`. Translates Oracle SQL dialect to HeliosDB SQL internally. Enable with `oracle_port` in the config file.

### Supabase-Compatible API

PostgREST-compatible endpoints, real-time subscriptions, storage API, and JWT authentication for drop-in Supabase replacement.

---

## SQL Reference

### Data Types

| Type | Aliases | Description |
|------|---------|-------------|
| `BOOLEAN` | `BOOL` | `true` / `false` |
| `SMALLINT` | `INT2` | 16-bit signed integer |
| `INTEGER` | `INT`, `INT4` | 32-bit signed integer |
| `BIGINT` | `INT8` | 64-bit signed integer |
| `REAL` | `FLOAT4` | 32-bit IEEE 754 |
| `DOUBLE PRECISION` | `FLOAT8` | 64-bit IEEE 754 |
| `NUMERIC` | `DECIMAL(p,s)` | Arbitrary-precision decimal |
| `TEXT` | | Variable-length string |
| `VARCHAR(n)` | `CHARACTER VARYING` | Bounded string |
| `BYTEA` | `BLOB` | Binary data |
| `DATE` | | Calendar date |
| `TIME` | | Time of day |
| `TIMESTAMP` | `TIMESTAMPTZ` | Date + time (UTC) |
| `UUID` | | 128-bit UUID |
| `JSON` | `JSONB` | JSON document with indexing |
| `VECTOR(n)` | | n-dimensional float vector |
| `ARRAY` | | Typed arrays: `INT[]`, `TEXT[]` |

### DDL

```sql
CREATE TABLE products (
    id          INT PRIMARY KEY,
    name        VARCHAR(255) NOT NULL,
    price       DECIMAL(10,2) DEFAULT 0.00,
    tags        TEXT[],
    metadata    JSONB,
    embedding   VECTOR(1536),
    created_at  TIMESTAMP DEFAULT NOW()
);

CREATE INDEX idx_name ON products (name);
CREATE UNIQUE INDEX idx_sku ON products (sku);

ALTER TABLE products ADD COLUMN category TEXT;
ALTER TABLE products DROP COLUMN tags;

DROP TABLE products;
DROP TABLE IF EXISTS products;
```

**Foreign keys:**

```sql
CREATE TABLE orders (
    id         INT PRIMARY KEY,
    user_id    INT REFERENCES users(id) ON DELETE CASCADE,
    product_id INT REFERENCES products(id) ON DELETE SET NULL
);
```

**Triggers:**

```sql
CREATE TRIGGER audit_trigger
    AFTER INSERT OR UPDATE OR DELETE ON users
    FOR EACH ROW EXECUTE FUNCTION log_change();
```

### DML

```sql
INSERT INTO users (id, name) VALUES (1, 'Alice'), (2, 'Bob');

INSERT INTO users (id, name) VALUES (1, 'Alice')
    ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name;    -- UPSERT

UPDATE users SET name = 'Charlie' WHERE id = 1;

DELETE FROM users WHERE id = 2;
```

### Queries

```sql
-- Joins
SELECT u.name, o.total
FROM users u
INNER JOIN orders o ON u.id = o.user_id
LEFT JOIN products p ON o.product_id = p.id;

-- Subqueries
SELECT * FROM users WHERE id IN (SELECT user_id FROM orders WHERE total > 100);

-- CTEs
WITH top_buyers AS (
    SELECT user_id, SUM(total) as spend FROM orders GROUP BY user_id
)
SELECT u.name, t.spend FROM users u JOIN top_buyers t ON u.id = t.user_id;

-- Set operations
SELECT name FROM employees UNION ALL SELECT name FROM contractors;
SELECT id FROM table_a INTERSECT SELECT id FROM table_b;
SELECT id FROM table_a EXCEPT SELECT id FROM table_b;

-- Aggregates
SELECT department, COUNT(*), AVG(salary), MIN(salary), MAX(salary)
FROM employees GROUP BY department HAVING AVG(salary) > 50000;

-- Window functions
SELECT
    name, salary,
    ROW_NUMBER() OVER (ORDER BY salary DESC)               AS rank,
    RANK()       OVER (PARTITION BY dept ORDER BY salary)   AS dept_rank,
    LAG(salary)  OVER (ORDER BY hire_date)                  AS prev_salary,
    LEAD(salary) OVER (ORDER BY hire_date)                  AS next_salary
FROM employees;

-- CASE
SELECT name, CASE WHEN salary > 100000 THEN 'Senior' ELSE 'Junior' END AS level
FROM employees;

-- DISTINCT, ORDER BY, LIMIT/OFFSET
SELECT DISTINCT department FROM employees ORDER BY department LIMIT 10 OFFSET 5;
```

### Procedural SQL

Multi-dialect procedural language support with auto-detection (PL/pgSQL, T-SQL, PL/SQL, DB2 SQL PL):

```sql
CREATE FUNCTION factorial(n INT) RETURNS INT AS $$
DECLARE
    result INT := 1;
BEGIN
    FOR i IN 1..n LOOP
        result := result * i;
    END LOOP;
    RETURN result;
END;
$$ LANGUAGE plpgsql;

CREATE PROCEDURE reset_scores(threshold INT) AS $$
BEGIN
    UPDATE players SET score = 0 WHERE score < threshold;
END;
$$ LANGUAGE plpgsql;

CALL reset_scores(10);
```

### JSONB Operators

```sql
SELECT metadata->'address'->>'city' FROM users;              -- field access
SELECT * FROM users WHERE metadata @> '{"role":"admin"}';     -- containment
SELECT * FROM users WHERE metadata ? 'email';                 -- key exists
```

---

## Vector Search

HNSW (Hierarchical Navigable Small World) indexes with optional Product Quantization for 8-16x memory reduction.

```sql
-- Create a table with a vector column
CREATE TABLE documents (
    id        INT PRIMARY KEY,
    title     TEXT,
    embedding VECTOR(1536)
);

-- Insert vectors
INSERT INTO documents VALUES (1, 'Hello world', '[0.1, 0.2, ...]');

-- K-nearest-neighbor search
SELECT title, embedding <-> '[0.15, 0.25, ...]' AS distance
FROM documents
ORDER BY embedding <-> '[0.15, 0.25, ...]'
LIMIT 10;
```

**Distance operators:**

| Operator | Metric |
|----------|--------|
| `<->` | Cosine distance |
| `<~>` | L2 (Euclidean) distance |
| `<#>` | Inner product distance |

**Configuration** (in `config.toml`):

```toml
[vector]
default_index_type = "hnsw"
hnsw_ef_construction = 200   # Higher = better recall, slower build
hnsw_m = 16                  # Connections per layer
enable_pq = true             # Product Quantization (8-16x compression)
pq_subvectors = 8
pq_bits = 8
```

---

## Git-Like Branching

Create isolated database branches for development, testing, or A/B experiments. Copy-on-write storage keeps branches lightweight.

```sql
CREATE BRANCH staging;                 -- branch from current state
CREATE BRANCH feature FROM staging;    -- branch from another branch

USE BRANCH staging;                    -- switch to branch

-- All reads/writes are now isolated to 'staging'
INSERT INTO users VALUES (99, 'Test User');

-- Merge back
MERGE BRANCH staging INTO main;

-- Clean up
DROP BRANCH staging;
```

**REPL meta-commands:**

```
\branches          List all branches
\use staging       Switch to branch
```

---

## Time-Travel Queries

Query any table at a previous point in time. Three addressing modes:

```sql
-- By wall-clock timestamp
SELECT * FROM orders AS OF TIMESTAMP '2024-06-15 10:30:00';

-- By transaction ID
SELECT * FROM orders AS OF TRANSACTION 12345;

-- By System Change Number (Oracle-compatible)
SELECT * FROM orders AS OF SCN 999999;
```

Configure retention in `config.toml`:

```toml
[storage]
time_travel_enabled = true
# snapshot_retention_days = 7   (default)
```

---

## Materialized Views

Pre-computed query results with manual, automatic, or incremental refresh strategies.

```sql
CREATE MATERIALIZED VIEW sales_summary AS
    SELECT product_id, SUM(amount) AS total, COUNT(*) AS orders
    FROM orders GROUP BY product_id;

-- Manual refresh
REFRESH MATERIALIZED VIEW sales_summary;

-- Auto-refresh on base table changes (config-level)
-- Set auto_refresh_default = true in [materialized_views]
```

**System views:**

```sql
SELECT * FROM pg_materialized_views;   -- list all MVs
SELECT * FROM pg_mv_stats;             -- refresh stats
```

**Configuration:**

```toml
[materialized_views]
auto_refresh_default = false
default_max_cpu_percent = 15
refresh_check_interval_secs = 60
max_concurrent_refreshes = 2
```

---

## Encryption & Security

### Transparent Data Encryption (TDE)

AES-256-GCM at-rest encryption with automatic key rotation.

```toml
[encryption]
enabled = true
algorithm = "Aes256Gcm"
rotation_interval_days = 90

[encryption.key_source]
Environment = "HELIOSDB_ENCRYPTION_KEY"
# File = "/secure/path/to/encryption.key"
# Kms = { provider = "aws", key_id = "arn:aws:kms:..." }
```

### Zero-Knowledge Encryption (ZKE)

Client-side encryption where the server never sees plaintext data or keys.

| Mode | Description |
|------|-------------|
| `Full` | All data encrypted client-side; server only stores ciphertext |
| `Hybrid` | Metadata unencrypted, row data encrypted |
| `PerRequest` | Key provided per-request, server decrypts temporarily |

### FIPS 140-3 Compliance

Build with the FIPS-certified AWS-LC provider (Certificate #4816):

```bash
cargo build --no-default-features --features fips,encryption,vector-search
```

Switches: BLAKE3 to SHA-256, Argon2id to PBKDF2, ring to aws-lc-rs.

### Row-Level Security (RLS)

Column-level masking and row filtering based on tenant context.

### Multi-Tenancy

Schema-level tenant isolation with per-tenant encryption keys and resource quotas:

```toml
[resource_quotas]
memory_limit_per_user_mb = 1024
max_concurrent_queries = 100
query_timeout_secs = 300
```

### Authentication

```bash
# SCRAM-SHA-256 (recommended)
heliosdb-nano start --auth scram-sha-256 --password s3cret

# MD5
heliosdb-nano start --auth md5 --password s3cret

# Trust (development only)
heliosdb-nano start --auth trust
```

JWT token-based auth is also supported for API access.

### Audit Logging

Tamper-proof audit trail with SHA-256 checksums:

```toml
[audit]
enabled = true
```

```sql
SELECT * FROM pg_audit_log
WHERE operation = 'DELETE' AND timestamp > NOW() - INTERVAL '24 hours';
```

---

## EXPLAIN & Query Optimizer

Cost-based query optimizer with predicate pushdown, join reordering, and index selection.

```sql
EXPLAIN SELECT * FROM orders WHERE user_id = 42;
EXPLAIN (ANALYZE, BUFFERS) SELECT * FROM orders JOIN users ON orders.user_id = users.id;
EXPLAIN (FORMAT JSON) SELECT * FROM products WHERE price > 100;
```

**Output formats:** `TEXT` (default), `JSON`, `XML`, `YAML`

**Options:** `ANALYZE`, `VERBOSE`, `COSTS`, `BUFFERS`, `TIMING`

**Optimizer tuning:**

```toml
[optimizer]
enabled = true
seq_page_cost = 1.0
random_page_cost = 1.1    # Use 1.1 for SSD, 4.0 for HDD
cpu_tuple_cost = 0.01
```

---

## Import / Export & Backup

### Dump & Restore

```bash
# Full backup with zstd compression
heliosdb-nano dump --data-dir ./mydata --output backup.heliodump --compression zstd

# Incremental
heliosdb-nano dump --data-dir ./mydata --output backup.heliodump --append

# Restore with integrity verification
heliosdb-nano restore --input backup.heliodump --target ./restored --verify
```

Supported compression: `zstd` (default), `gzip`, `brotli`, `none`.

### Automatic Scheduled Dumps

```toml
[dump]
auto_dump_enabled = true
schedule = "0 */6 * * *"    # Every 6 hours (cron syntax)
compression = "zstd"
max_dump_size_mb = 10000
keep_dumps = 10
```

### REPL Import/Export

```
\import csv /path/to/data.csv INTO my_table
\export my_table TO /path/to/output.parquet
```

Formats: CSV, JSON, JSONL, Parquet, Arrow, SQL.

---

## CLI & REPL

### Commands

```
heliosdb-nano start    [options]     Start PostgreSQL-compatible server
heliosdb-nano stop     --pid-file    Stop a running server
heliosdb-nano status   --pid-file    Check server status
heliosdb-nano init     <data-dir>    Initialize new database directory
heliosdb-nano repl     [options]     Interactive embedded SQL shell
heliosdb-nano dump     [options]     Backup database to file
heliosdb-nano restore  [options]     Restore from backup
```

### REPL Meta-Commands

| Command | Description |
|---------|-------------|
| `\q`, `\exit` | Quit |
| `\h`, `\help` | Show help |
| `\d` | List tables |
| `\d <table>` | Describe table (columns, types, constraints) |
| `\dt` | Tables with row counts and sizes |
| `\dS` | List system views |
| `\timing` | Toggle query execution timing |
| `\branches` | List branches |
| `\use <branch>` | Switch branch |
| `\snapshots` | List time-travel snapshots |
| `\dmv` | List materialized views |
| `\stats` | Database statistics |
| `\compression` | Compression statistics |
| `\optimize <table>` | Optimization recommendations |
| `\show lsn` | Current LSN / transaction |
| `\show branch` | Current branch |
| `\ai templates` | List AI schema templates |
| `\ai generate <desc>` | Generate schema from description |

---

## REST API

Base URL: `http://localhost:8080`

| Method | Endpoint | Description |
|--------|----------|-------------|
| `POST` | `/api/query` | Execute SQL, returns JSON rows |
| `POST` | `/api/query/nl` | Natural language to SQL |
| `POST` | `/api/data/:table` | Insert rows |
| `GET` | `/api/data/:table` | Query with filters |
| `PUT` | `/api/data/:table/:id` | Update row |
| `DELETE` | `/api/data/:table/:id` | Delete row |
| `POST` | `/api/vectors/search` | K-NN vector search |
| `GET` | `/api/branches` | List branches |
| `POST` | `/api/branches` | Create branch |
| `POST` | `/api/branches/:name/merge` | Merge branch |
| `GET` | `/api/schema` | Schema introspection |
| `POST` | `/api/chat` | OpenAI-compatible chat completions |
| `POST` | `/api/webhooks` | Register event webhooks |
| `GET` | `/health` | Health check |

**Example — execute SQL:**

```bash
curl -X POST http://localhost:8080/api/query \
  -H 'Content-Type: application/json' \
  -d '{"sql": "SELECT * FROM users LIMIT 10"}'
```

**Example — vector search:**

```bash
curl -X POST http://localhost:8080/api/vectors/search \
  -H 'Content-Type: application/json' \
  -d '{"collection": "documents", "query": [0.1, 0.2], "k": 5, "metric": "cosine"}'
```

---

## Embedded Library Usage

Use HeliosDB as a Rust library (like SQLite) with zero network overhead.

**Add to `Cargo.toml`:**

```toml
[dependencies]
heliosdb-nano = "3.7"
```

**Example:**

```rust
use heliosdb_nano::EmbeddedDatabase;

fn main() -> heliosdb_nano::Result<()> {
    // Persistent on disk
    let db = EmbeddedDatabase::new("./mydb.helio")?;

    // Or fully in-memory
    // let db = EmbeddedDatabase::new_in_memory()?;

    // DDL
    db.execute("CREATE TABLE items (id INT PRIMARY KEY, name TEXT, price DECIMAL(10,2))")?;

    // DML
    db.execute("INSERT INTO items VALUES (1, 'Widget', 9.99)")?;

    // Queries
    let rows = db.query("SELECT * FROM items WHERE price < 20", &[])?;
    for row in &rows {
        println!("{:?}", row);
    }

    // Parameterized queries
    use heliosdb_nano::Value;
    let rows = db.query_params(
        "SELECT * FROM items WHERE id = $1",
        &[Value::Int4(1)],
    )?;

    // Time-travel
    let history = db.query(
        "SELECT * FROM items AS OF TIMESTAMP '2024-06-01 00:00:00'",
        &[],
    )?;

    // Branching
    db.execute("CREATE BRANCH staging")?;
    db.execute("USE BRANCH staging")?;
    db.execute("UPDATE items SET price = 12.99 WHERE id = 1")?;
    db.execute("MERGE BRANCH staging INTO main")?;

    Ok(())
}
```

---

## High Availability

HA features are compiled via Cargo feature flags. All are optional.

| Feature Flag | Description |
|---|---|
| `ha-tier1` | Warm standby: WAL streaming, automatic failover, read replicas |
| `ha-tier2` | Multi-primary: active-active, branch-based conflict resolution |
| `ha-tier3` | Sharding: consistent hash ring, cross-shard queries |
| `ha-proxy` | Connection router with load balancing |
| `ha-tr` | Transaction Replay: journaling, cursor restore, session migration |
| `ha-dedup` | Content-addressed deduplication across nodes |
| `ha-ab-testing` | Branch-based A/B experiment routing |
| `ha-branch-replication` | Selective branch sync to remote servers |
| `ha-standard` | Bundle: tier1 + tier2 + proxy + tr |
| `ha-full` | Bundle: all HA features |

```bash
# Build with standard HA
cargo build --release --features ha-standard

# Start as primary
heliosdb-nano start --data-dir ./primary \
    --replication-role primary \
    --replication-port 5433

# Start as standby
heliosdb-nano start --data-dir ./standby \
    --port 5434 \
    --replication-role standby \
    --primary-host localhost:5433 \
    --sync-mode semi-sync
```

---

## Configuration Reference

HeliosDB loads configuration from a TOML file specified with `--config`:

```bash
heliosdb-nano start --config heliosdb.toml --data-dir ./mydata
```

### Minimal `heliosdb.toml`

```toml
[server]
listen_addr = "0.0.0.0"
port = 5432
max_connections = 100

[storage]
wal_enabled = true
wal_sync_mode = "sync"           # sync | async | group_commit
compression = "Zstd"
time_travel_enabled = true

[encryption]
enabled = false

[authentication]
enabled = false
method = "trust"

[optimizer]
enabled = true
random_page_cost = 1.1           # SSD-optimized

[vector]
default_index_type = "hnsw"
hnsw_ef_construction = 200
hnsw_m = 16

[materialized_views]
auto_refresh_default = false
max_concurrent_refreshes = 2
```

Full configuration options cover: server, storage, encryption, authentication, optimizer, vector search, materialized views, AI/LLM integration, RAG, dump scheduling, resource quotas, session management, locks, audit logging, and WASM extensions.

---

## Building from Source

### Prerequisites

- Rust 1.75+ (2021 edition)
- C/C++ compiler (for RocksDB)
- clang + LLVM (for FIPS builds with aws-lc-rs)

### Build

```bash
# Default (ring crypto, encryption, vector search)
cargo build --release

# FIPS 140-3 compliant build
cargo build --release --no-default-features --features fips,encryption,vector-search

# With SIMD acceleration
cargo build --release --features simd

# With all HA features
cargo build --release --features ha-full
```

### Run Tests

```bash
cargo test --lib               # Unit tests (~989 tests)
cargo test --test '*'           # Integration tests
```

### Benchmarks

```bash
cargo bench --bench vector_search_bench
cargo bench --bench encryption_benchmark
cargo bench --bench art_index_bench
cargo bench --bench time_travel_optimization
cargo bench --bench branch_performance
```

---

## License

AGPL-3.0-only (GNU Affero General Public License v3)
