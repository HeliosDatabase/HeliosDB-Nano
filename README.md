# HeliosDB Nano

[![Crates.io](https://img.shields.io/crates/v/heliosdb-nano.svg)](https://crates.io/crates/heliosdb-nano)
[![Documentation](https://docs.rs/heliosdb-nano/badge.svg)](https://docs.rs/heliosdb-nano)
[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)](LICENSE)

**The first embedded database with native PostgreSQL, MySQL, and SQLite compatibility.** Single 47 MB binary. HNSW vector search, git-like branching, time-travel queries, AES-256-GCM encryption, built-in BaaS layer (Auth, REST API, Realtime).

Use your existing clients (`psql`, `mysql`, `curl`) and ORMs — zero migration required.

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
```

Three servers start on one process:

| Protocol | Port | Connect |
|----------|-----:|---------|
| PostgreSQL wire | 5432 | `psql`, psycopg2, pgx, JDBC, Npgsql, node-postgres |
| MySQL wire | 3306 | `mysql`, PyMySQL, SQLAlchemy, JDBC, mysql2 |
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
psql (16.0, server HeliosDB Nano 3.10.0)
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

## Features at a Glance

- **Full SQL**: JOINs, CTEs, window functions, subqueries, set operations, aggregates, CASE
- **PL/pgSQL**: Stored procedures and functions
- **JSONB**: `->`, `->>`, `@>`, `?` operators
- **Foreign keys**: CASCADE, SET NULL, RESTRICT
- **Triggers**: BEFORE/AFTER INSERT/UPDATE/DELETE
- **Row-Level Security**: Per-tenant data isolation via policies
- **EXPLAIN**: Cost-based optimizer, ANALYZE, JSON/XML/YAML output
- **Backup/Restore**: Compressed dumps (zstd/gzip/brotli)
- **Import/Export**: CSV, JSON, JSONL, Parquet, Arrow, SQL
- **Audit logging**: Tamper-proof trail (SHA-256 checksums)
- **Encryption**: AES-256-GCM TDE, FIPS 140-3 mode

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
heliosdb-nano = "3.10"
```

See **[the Rust API guide](https://docs.rs/heliosdb-nano)** for embedded usage and the [examples/](examples/) directory for working code.

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

## Documentation

- [Getting Started](https://heliosdb.com/nano.html)
- [API Explorer (Swagger UI)](http://localhost:8080/docs) — when running locally
- [vs Supabase](https://heliosdb.com/vs-supabase.html)
- [vs Firebase](https://heliosdb.com/vs-firebase.html)
- [vs PostgreSQL](https://heliosdb.com/vs-postgresql.html)
- [vs SQLite](https://heliosdb.com/vs-sqlite.html)
- [Migrate from MySQL](https://heliosdb.com/migrate-mysql.html)

## License

[AGPL-3.0-only](LICENSE) — GNU Affero General Public License v3
