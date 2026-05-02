# HeliosDB Nano

[![Crates.io](https://img.shields.io/crates/v/heliosdb-nano.svg)](https://crates.io/crates/heliosdb-nano)
[![Documentation](https://docs.rs/heliosdb-nano/badge.svg)](https://docs.rs/heliosdb-nano)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

**An embedded database with native PostgreSQL and MySQL wire-protocol compatibility, plus one-shot SQLite file import.** Single 47 MB binary. HNSW vector search, git-like branching, time-travel queries, AES-256-GCM encryption, built-in BaaS layer (Auth, REST API, Realtime).

Use your existing clients (`psql`, `mysql`), RESTful HTTP, drivers (`psycopg2`, `mysql-connector`, `node-postgres`, JDBC), and ORMs (SQLAlchemy, Prisma, Drizzle, Hibernate, GORM) — zero migration required. Existing `.sqlite` files import via a bundled converter.

## Install

```bash
# npm (cross-platform, auto-downloads binary)
npx heliosdb start

# Homebrew (macOS / Linux)
brew install dimensigon/tap/heliosdb-nano

# Docker
docker run -p 5432:5432 -p 3306:3306 -p 8080:8080 heliosdb/nano:latest

# Binary release
curl -L https://github.com/Dimensigon/HDB-HeliosDB-Nano/releases/latest/download/heliosdb-nano-$(uname -m)-$(uname -s | tr A-Z a-z).tar.gz | tar xz
```

## Start the Server

```bash
# Persistent, all three protocols
heliosdb-nano start --data-dir ./mydata --mysql

# In-memory (great for dev/test)
heliosdb-nano start --memory --mysql

# With auth and TLS
heliosdb-nano start --data-dir ./mydata --mysql \
  --auth scram-sha-256 --password s3cret \
  --tls-cert cert.pem --tls-key key.pem

# Same-host / embedded mode — Unix sockets (no TCP)
heliosdb-nano start --memory \
  --pg-socket-dir /tmp \
  --mysql --mysql-socket /tmp/heliosdb-mysql.sock
# then: psql -h /tmp  or  mysql --socket=/tmp/heliosdb-mysql.sock
```

Three servers start on one process:

| Protocol | Port | Connect |
|----------|-----:|---------|
| PostgreSQL wire | 5432 | `psql`, psycopg2, pgx, JDBC, Npgsql, node-postgres |
| PostgreSQL Unix socket | `/tmp/.s.PGSQL.5432` | `psql -h /tmp`, libpq-default apps |
| MySQL wire | 3306 | `mysql`, PyMySQL, SQLAlchemy, JDBC, mysql2 |
| MySQL Unix socket | `/tmp/heliosdb-mysql.sock` (configurable) | `mysql --socket=…`, PHP `mysqli`, WordPress |
| REST / HTTP | 8080 | `curl`, fetch, any HTTP client |

## Triple Compatibility — Same Data, Any Client

Start the server once, then connect from any of the three interfaces. They all read and write the same tables.

### Interactive REPL (zero setup)

```bash
$ heliosdb-nano repl --data-dir ./mydata
heliosdb> CREATE TABLE products (id SERIAL PRIMARY KEY, name TEXT, price DECIMAL(10,2));
OK
heliosdb> INSERT INTO products (name, price) VALUES ('Widget', 9.99), ('Gadget', 19.99);
INSERT 2
heliosdb> SELECT * FROM products WHERE price < 15;
 id |  name  | price
----+--------+-------
  1 | Widget |  9.99
(1 row)
```

### PostgreSQL Client (`psql`)

```bash
$ psql -h 127.0.0.1 -p 5432 -U postgres
psql (16.0, server HeliosDB Nano 3.13.0)
postgres=# INSERT INTO products (name, price) VALUES ('Gizmo', 29.99);
INSERT 0 1
postgres=# SELECT COUNT(*) FROM products;
 count
-------
     3
```

### MySQL Client (`mysql`)

```bash
$ mysql -h 127.0.0.1 -P 3306 -u root
Server version: 8.0.35-HeliosDB-Nano
mysql> SELECT * FROM products WHERE name LIKE 'G%';
+----+--------+-------+
| id | name   | price |
+----+--------+-------+
|  2 | Gadget | 19.99 |
|  3 | Gizmo  | 29.99 |
+----+--------+-------+
mysql> INSERT INTO products (name, price) VALUES ('Gear', 39.99);
Query OK, 1 row affected
```

### REST API (`curl`)

```bash
# Query
$ curl "http://localhost:8080/rest/v1/products?price=lt.50&select=id,name,price"
[{"id":1,"name":"Widget","price":"9.99"},{"id":2,"name":"Gadget","price":"19.99"}, ...]

# Insert
$ curl -X POST http://localhost:8080/rest/v1/products \
    -H 'Content-Type: application/json' \
    -d '{"name":"Gear 2","price":49.99}'

# Interactive API explorer (Swagger UI)
$ open http://localhost:8080/docs
```

## Vector Search

Native HNSW indexes — no extensions, no separate vector database.

```sql
-- From any client (psql / mysql / REPL):
CREATE TABLE docs (
    id SERIAL PRIMARY KEY,
    title TEXT,
    embedding VECTOR(1536)
);

CREATE INDEX ON docs USING hnsw (embedding vector_cosine_ops);

INSERT INTO docs (title, embedding)
VALUES ('Intro', '[0.1, 0.2, 0.3, ...]');

-- k-NN search
SELECT title, embedding <-> '[0.15, 0.25, ...]' AS distance
FROM docs
ORDER BY distance
LIMIT 10;
```

Distance operators: `<->` (cosine), `<~>` (L2), `<#>` (inner product).

Via REST:

```bash
curl -X POST http://localhost:8080/api/vectors/search \
    -H 'Content-Type: application/json' \
    -d '{"collection":"docs","query":[0.15,0.25],"k":5,"metric":"cosine"}'
```

## Full-Text Search

PostgreSQL-compatible FTS surface — no extensions, backed by built-in BM25:

```sql
-- Native tsvector / tsquery / @@ / ts_rank_cd:
SELECT title, ts_rank_cd(to_tsvector(body), to_tsquery('heliosdb')) AS rank
FROM articles
WHERE to_tsvector(body) @@ to_tsquery('heliosdb')
ORDER BY rank DESC
LIMIT 10;

-- Persistent tsvector column + GIN-style DDL:
CREATE TABLE articles (id SERIAL PRIMARY KEY, body TEXT, body_tsv TSVECTOR);
CREATE INDEX articles_body_fts ON articles USING gin (body_tsv);

-- Hybrid search (FTS + vector) in one query:
SELECT id, text,
       0.7 * (1.0 - (embedding <=> $1::vector))
     + 0.3 * ts_rank_cd(to_tsvector(text), plainto_tsquery($2)) AS score
FROM chunks
ORDER BY score DESC LIMIT 10;
```

Scope and honest limitations: see [docs/compatibility/fts.md](docs/compatibility/fts.md).

## Pagination — Constant-Time at Depth

Deep `LIMIT … OFFSET` runs in ~30 µs regardless of offset, up to **334× faster
than PostgreSQL 13** for 100k-row tables. Top-K over Sort, storage-level
`OFFSET` skip, and keyset (`WHERE (col, id) < ($1, $2)`) are all native.

See [pagination-performance.html](https://heliosdb.com/pagination-performance.html)
for measured numbers and reproduction recipe.

## Git-Like Branching

Isolated copy-on-write branches for dev, test, and A/B experiments.

```sql
CREATE BRANCH staging FROM main;
USE BRANCH staging;

-- Changes here are invisible to main
INSERT INTO products (name, price) VALUES ('Test', 0.01);

MERGE BRANCH staging INTO main;
DROP BRANCH staging;
```

## Time-Travel Queries

```sql
-- As of a timestamp
SELECT * FROM products AS OF TIMESTAMP '2026-04-01 12:00:00';

-- As of a transaction
SELECT * FROM products AS OF TRANSACTION 12345;
```

## Built-in Backend-as-a-Service

Self-hosted Supabase/Firebase alternative — Auth, REST, Realtime, RLS in the same binary:

```bash
# Sign up
curl -X POST http://localhost:8080/auth/v1/signup \
    -H 'Content-Type: application/json' \
    -d '{"email":"alice@example.com","password":"s3cret"}'

# Google OAuth redirect
open http://localhost:8080/auth/v1/authorize?provider=google

# Realtime subscriptions (WebSocket)
wscat -c ws://localhost:8080/realtime/v1/websocket
```

RLS is automatic on REST endpoints via JWT claims. See [vs-supabase](https://heliosdb.com/vs-supabase.html).

## ORM & Driver Compatibility

| Language | PostgreSQL driver | MySQL driver | Tested ORMs |
|----------|------------------|--------------|-------------|
| Python | `psycopg2`, `asyncpg` | `PyMySQL`, `mysql-connector-python` | SQLAlchemy, Django ORM |
| Node.js | `pg`, `node-postgres` | `mysql2` | Prisma, Drizzle, TypeORM, Sequelize |
| Java | JDBC (postgresql) | JDBC (mysql-connector-j) | Hibernate, JPA |
| Go | `lib/pq`, `pgx` | `go-sql-driver/mysql` | GORM, ent |
| Rust | `tokio-postgres`, `sqlx` | `mysql_async`, `sqlx` | SeaORM, Diesel |
| PHP | PDO pgsql | `mysqli`, PDO mysql | Laravel Eloquent, WordPress |

**WordPress runs natively** with standard `wpdb` — no drop-in required.

## Data Types

All PostgreSQL types plus MySQL type aliases (automatically translated):

| Canonical | Aliases |
|-----------|---------|
| `BOOLEAN` | `BOOL`, `TINYINT(1)` |
| `SMALLINT` / `INTEGER` / `BIGINT` | `INT2`/`INT4`/`INT8`, `TINYINT`, `MEDIUMINT` |
| `REAL` / `DOUBLE PRECISION` | `FLOAT4`/`FLOAT8`, `FLOAT(N)` |
| `NUMERIC(p,s)` | `DECIMAL(p,s)` |
| `TEXT` | `VARCHAR(n)`, `LONGTEXT`, `MEDIUMTEXT`, `TINYTEXT` |
| `BYTEA` | `BLOB`, `LONGBLOB`, `MEDIUMBLOB` |
| `TIMESTAMP` | `DATETIME` |
| `SERIAL` / `BIGSERIAL` | `INT AUTO_INCREMENT`, `BIGINT AUTO_INCREMENT` |
| `UUID`, `JSON`, `JSONB`, `VECTOR(n)`, `ARRAY` | — |
| `TSVECTOR`, `TSQUERY` | stored as JSON arrays of normalised tokens |

## Features at a Glance

- **Full SQL**: JOINs, CTEs, window functions, subqueries, set operations, aggregates, CASE
- **PL/pgSQL**: Stored procedures and functions
- **JSONB**: `->`, `->>`, `@>`, `?` operators
- **Full-text search**: `tsvector`, `tsquery`, `@@`, `ts_rank_cd`, `CREATE INDEX ... USING gin` (see [FTS scope](docs/compatibility/fts.md))
- **Keyset pagination**: row-constructor comparison `WHERE (col, id) < ($1, $2)`; top-K sort; constant-time deep OFFSET
- **Foreign keys**: CASCADE, SET NULL, RESTRICT
- **Triggers**: BEFORE/AFTER INSERT/UPDATE/DELETE
- **Row-Level Security**: Per-tenant data isolation via policies
- **EXPLAIN**: Cost-based optimizer, ANALYZE, JSON/XML/YAML output
- **Code-graph** *(opt-in, `--features code-graph`)*: tree-sitter-backed AST index + `lsp_definition` / `lsp_references` / `lsp_call_hierarchy` / `lsp_hover` as Rust API & SQL table functions — see [code-graph overview](docs/code_graph/overview.md)
- **Backup/Restore**: Compressed dumps (zstd/gzip/brotli)
- **Import/Export**: CSV, JSON, JSONL, Parquet, Arrow, SQL
- **Audit logging**: Tamper-proof trail (SHA-256 checksums)
- **Encryption**: AES-256-GCM TDE, FIPS 140-3 mode
- **Unix domain socket listeners** for both PostgreSQL (`--pg-socket-dir /tmp`) and MySQL (`--mysql-socket /tmp/heliosdb.sock`) — PHP `mysqli` / WordPress embedded-mode and libpq defaults work out of the box

## Architecture

| Layer | Technology |
|-------|-----------|
| Storage engine | RocksDB (LSM-tree) |
| Columnar format | Apache Arrow |
| SQL parser | sqlparser-rs |
| Vector index | HNSW + Product Quantization |
| Wire protocols | PostgreSQL v3, MySQL v10 |
| HTTP server | Axum |
| Encryption | AES-256-GCM, AWS-LC FIPS |

## High Availability

**Warm standby is enabled by default** — no feature flag needed. Just pass the replication flags at startup:

```bash
# Primary
heliosdb-nano start --data-dir ./data --replication-role primary \
  --standby-hosts 10.0.0.2:5433,10.0.0.3:5433

# Standby
heliosdb-nano start --data-dir ./data --replication-role standby \
  --primary-host 10.0.0.1:5433
```

Optional HA features (opt-in at compile time):

| Flag | Description |
|------|-------------|
| `ha-tier1` | Warm standby — **enabled by default** |
| `ha-tier2` | Multi-primary: branch-based active-active |
| `ha-tier3` | Sharding: consistent hash ring |
| `ha-dedup` | Content-addressed deduplication across nodes |
| `ha-ab-testing` | Branch-based experiment routing |
| `ha-branch-replication` | Selective branch sync to remote servers |
| `ha-full` | All optional HA features bundled |

```bash
cargo build --release --features ha-full    # everything
```

> **Tip — verify HA changes locally, not in CI**: the HA streaming and
> lock-management integration tests rely on tight TCP-port spin-waits
> that pass cleanly on a developer workstation (sub-second) but routinely
> hang on the 2-CPU GitHub Actions runner. The release workflow gates on
> `cargo test --lib` only; if you're modifying anything under
> `src/storage/wal/`, `src/cluster/`, or `src/storage/locks/`, run the full
> integration suite locally first:
>
> ```bash
> cargo test --features ha-tier1 --test ha_integration   # warm-standby + streaming
> cargo test --tests --skip ha_tests::streaming_tests --skip lock_management
> ```

### Connection Routing & Load Balancing

For production deployments with multiple HeliosDB Nano instances, put **[HeliosProxy](https://github.com/dimensigon/heliosdb-proxy)** in front — a standalone binary providing:

- Read/write splitting across primary + standbys
- Automatic failover with transaction replay (Oracle TAF-style)
- Connection pooling
- Health checks + circuit breakers
- TLS termination

### Recommended Production Setup

```
           ┌────────────────┐
    psql ─▶│                │──▶ HeliosDB Nano (primary, read+write)
   mysql ─▶│  HeliosProxy   │──▶ HeliosDB Nano (standby, read-only)
    curl ─▶│                │──▶ HeliosDB Nano (standby, read-only)
           └────────────────┘
              port 5432/3306/8080
```

1. Deploy 1 primary + 2 standbys (Fly.io / Render / Docker Swarm)
2. HeliosProxy in front for routing + failover
3. Automatic failover on primary death (< 5 s typical)
4. Readonly queries load-balanced across standbys

## Deploy

| Platform | Template |
|----------|----------|
| **Fly.io** | [deployment/flyio/](deployment/flyio/) |
| **Railway** | [deployment/railway/](deployment/railway/) |
| **Render** | [deployment/render/](deployment/render/) |
| **Docker** | [deployment/docker/](deployment/docker/) |

## Embedded Library (Rust)

For in-process use (no network, no daemon), add the crate as a dependency:

```toml
[dependencies]
heliosdb-nano = "3.13"
```

See **[the Rust API guide](https://docs.rs/heliosdb-nano)** for embedded usage and the [examples/](examples/) directory for working code.

## Building from Source

The `heliosdb-nano` binary builds with `cargo build --release`. Default features are `encryption + vector-search + ring-crypto + ha-tier1` — covers most embedded and single-node-server cases. Run `cargo info heliosdb-nano` (or `cargo metadata --no-deps --format-version 1 | jq '.packages[0].features'`) for the live list. Recipes for the non-obvious combinations:

```bash
# Default build — embedded + Postgres/MySQL wire + warm-standby HA.
cargo build --release

# Code-graph + MCP server (matches what heliosdb-codekb-mcp links).
# Adds tree-sitter parsers, _hdb_code_* tables, lsp_* APIs, and the
# JSON-RPC dispatcher for stdio / HTTP / WebSocket / SSE clients.
cargo build --release --features "code-graph,graph-rag,mcp-endpoint"

# In-process embedder (no external HTTP service for embeddings).
# Pulls fastembed-rs + ORT — adds ~30 MB to the binary.
cargo build --release --features "code-graph,code-embed"

# FIPS 140-3 compliant crypto (AWS-LC FIPS Cert #4816, SHA-256, PBKDF2).
# `--no-default-features` is required to swap out ring-crypto.
cargo build --release --no-default-features \
  --features "fips,encryption,vector-search,ha-tier1"

# Full HA bundle — multi-primary + sharding + dedup + branch replication.
cargo build --release --features "ha-full"
```

## SDKs & Integrations

Official client SDKs (Go, Python, TypeScript, Rust) and platform integrations (VS Code, Zapier, n8n, Retool, Make, AutoGen) live in a shared repository:

**[heliosdb-sdks](https://github.com/dimensigon/heliosdb-sdks)** — works with all HeliosDB editions.

```bash
# JavaScript / TypeScript (Supabase-compatible fluent API)
npm install @heliosdb/client
```

```javascript
import { createClient } from '@heliosdb/client'
const db = createClient('http://localhost:8080', 'anon-key')
const { data } = await db.from('products').select('*').lt('price', 50)
```

## Agentic Operations (Claude Code, Codex CLI, MCP-aware tools)

HeliosDB-Nano ships an **agentic-operations skill catalogue** — 17 SKILL.md files that give an LLM-driven coding agent a full A→Z catalogue of "verbs" for operating the database (install, connect, schema, DML, transactions, branches, time-travel, backup, vector, code-graph, graph-rag, MCP, server, deploy, observability, migrate). For Codex / generic agents the same content is aggregated at [`AGENTS.md`](AGENTS.md) at the project root.

```bash
# After git clone, Claude Code automatically picks up .claude/skills/ in this project.

# To install globally (~/.claude/skills/) so they apply in any project:
bash scripts/install-agent-skills.sh                # copy (default, frozen snapshot)
bash scripts/install-agent-skills.sh --symlink      # symlink (live updates)
```

Existing `~/.claude/skills/heliosdb-nano-*` directories are backed up to `*.bak.<unix-ts>` before being overwritten in either mode.

| Skill | What it covers |
|-------|---------------|
| `heliosdb-nano-overview` | Top-level navigation; routes to one of the 16 domain skills |
| `heliosdb-nano-install` | crates.io, source, feature flags (code-graph, mcp-endpoint, fips, ha-full…) |
| `heliosdb-nano-connect` | Embedded library, REPL, PG wire, MySQL wire, Python sqlite3 drop-in, TLS |
| `heliosdb-nano-schema` | DDL: tables, indexes (B-tree + HNSW), views, triggers, PL/pgSQL |
| `heliosdb-nano-query` | DML, parameter styles (`?` `$1` `:name` `@name`), `ON CONFLICT`, `RETURNING` |
| `heliosdb-nano-transactions` | BEGIN/COMMIT/ROLLBACK, savepoints, bulk-load patterns |
| `heliosdb-nano-branches` | `CREATE/USE/MERGE/DROP DATABASE BRANCH`, `AS OF` clones |
| `heliosdb-nano-time-travel` | `SELECT … AS OF TIMESTAMP '…'`, `\snapshots` |
| `heliosdb-nano-backup` | `dump`/`restore`, compression, append, partial restore, `--dump-schedule` |
| `heliosdb-nano-vector` | HNSW indexes, `<-> <#> <=>` operators, hybrid search |
| `heliosdb-nano-code-graph` | AST symbol index, LSP queries, git hook (`code-graph` feature) |
| `heliosdb-nano-graph-rag` | Knowledge graph + RAG ingest pipeline (`graph-rag` feature) |
| `heliosdb-nano-mcp` | MCP server, 16-tool catalog, stdio/HTTP/WS (`mcp-endpoint` feature) |
| `heliosdb-nano-server` | Daemon, TLS, auth, HA tier 1/2/3, user management |
| `heliosdb-nano-deploy` | Docker, Fly.io, Railway, Render, systemd template |
| `heliosdb-nano-observability` | Tracing, slow-query log, `/health`, `\stats`, `\optimize`, `\indexes` |
| `heliosdb-nano-migrate` | sqlite3 / Postgres / MySQL drop-in checklists |

Lookups: [`.claude/skills/_index/verb-map.md`](.claude/skills/_index/verb-map.md) (every CLI flag / REPL meta-command / public Rust API method / MCP tool) · [`.claude/skills/_index/feature-matrix.md`](.claude/skills/_index/feature-matrix.md) (cargo feature ↔ skill).

## Documentation

- [Getting Started](https://heliosdb.com/nano.html)
- [API Explorer (Swagger UI)](http://localhost:8080/docs) — when running locally
- [vs Supabase](https://heliosdb.com/vs-supabase.html)
- [vs Firebase](https://heliosdb.com/vs-firebase.html)
- [vs PostgreSQL](https://heliosdb.com/vs-postgresql.html)
- [vs SQLite](https://heliosdb.com/vs-sqlite.html)
- [Migrate from MySQL](https://heliosdb.com/migrate-mysql.html)

## License

[Apache-2.0](LICENSE) — Apache License, Version 2.0
