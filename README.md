# HeliosDB Nano

[![Crates.io](https://img.shields.io/crates/v/heliosdb-nano.svg)](https://crates.io/crates/heliosdb-nano)
[![Documentation](https://docs.rs/heliosdb-nano/badge.svg)](https://docs.rs/heliosdb-nano)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

**An embedded database with native PostgreSQL and MySQL wire-protocol compatibility, plus one-shot SQLite file import.** Single self-contained binary (~32 MB; ~12 MB compressed download). HNSW vector search, git-like branching, time-travel queries, AES-256-GCM encryption, built-in BaaS layer (Auth, REST API, Realtime).

Use your existing clients (`psql`, `mysql`), RESTful HTTP, drivers (`psycopg2`, `mysql-connector`, `node-postgres`, JDBC), and ORMs (SQLAlchemy, Prisma, Drizzle, Hibernate, GORM) — zero migration required. Existing `.sqlite` files import via a bundled converter.

## Ecosystem

Nano is one of four products in the HeliosDB family. SDKs and integrations are cross-edition — the same client code works against Nano, Lite, and Full.

- **[HeliosDatabase/HeliosDB-SDKs](https://github.com/HeliosDatabase/HeliosDB-SDKs)** — Official client SDKs (Python, TypeScript, Rust, Go) + integrations (VS Code, n8n, Zapier, Make, Retool, AutoGen) + cross-platform CLI. Apache 2.0.
- **[HeliosDatabase/Any2HeliosDB](https://github.com/HeliosDatabase/Any2HeliosDB)** — Apache-2.0 `a2h` migration toolkit for moving Oracle, MySQL, PostgreSQL, and SQL Server into HeliosDB Nano/Lite/Full or stock PostgreSQL, with wizard setup, resumable loads, validation, CDC, and MCP support.
- **[HeliosDatabase/HeliosDB-Lite](https://github.com/HeliosDatabase/HeliosDB-Lite)** — Production self-hosted database with HeliosProxy + HeliosCore baked in. SSPL-1.0.
- **[HeliosDatabase/HeliosDB-Full](https://github.com/HeliosDatabase/HeliosDB-Full)** — Distributed enterprise database with 14 native wire protocols. SSPL-1.0.
- **[HeliosDatabase/HeliosDB-Proxy](https://github.com/HeliosDatabase/HeliosDB-Proxy)** — Programmable Postgres data-plane (PgBouncer drop-in + WASM plugins + zero-downtime PG-12→17 upgrade). Apache 2.0.
- **[HeliosDatabase/HeliosDB-Proxy-Plugins](https://github.com/HeliosDatabase/HeliosDB-Proxy-Plugins)** · **[Operator](https://github.com/HeliosDatabase/HeliosDB-Proxy-Operator)** · **[Terraform](https://github.com/HeliosDatabase/terraform-provider-HeliosDB-Proxy)** · **[Pulumi](https://github.com/HeliosDatabase/pulumi-HeliosDB-Proxy)** — Proxy ecosystem.

**[HeliosDatabase/HeliosDB-CodeKB-MCP](https://github.com/HeliosDatabase/HeliosDB-CodeKB-MCP)** provides an MCP server that turns HeliosDB codebases and docs into queryable technical knowledge for Claude Code, Codex, and other MCP clients.

**Catalogue:** [heliosdb.com/sdks.html](https://www.heliosdb.com/sdks.html) · **Build with AI agents:** [heliosdb.com/build-with-agents.html](https://www.heliosdb.com/build-with-agents.html) · **LLM-discoverable index:** [heliosdb.com/llms.txt](https://www.heliosdb.com/llms.txt)

**Performance:** [what's fast, and where we're still improving](docs/PERFORMANCE.md) — honest strengths + transparent limits.

**Benchmark:** [HeliosDB-Nano vs PostgreSQL 18.4 — the `pg35` benchmark, its history & evolution](docs/benchmarks/PG35_BENCHMARK.md) — 35 SQL categories head-to-head; Nano wins all 35/35 at 300 iterations (30 by 10×–35,000×, the joins by 1.1×–2×). The one category that ever classified against Nano (Prepared stmts) turned out to expose a real `ROLLBACK TO SAVEPOINT` bug — fixed in v3.60.3, and now a ~157× Nano win. **Read the methodology note before quoting those ratios:** `pg35` runs Nano *embedded and in-memory* against a *networked, fsyncing* PostgreSQL, so the margins include deployment effects, not just engine speed.

For a like-for-like **wire-to-wire** comparison — Nano also as a TCP server, `pgbench` driving both — see [**vs PostgreSQL, 2026-08-28**](docs/benchmarks/heliosdb-nano-vs-postgresql-2026-08-28.md): Nano leads on indexed point-reads across all three query protocols (1.33×–2.22×) and on `SELECT 1` (1.84×–2.84×); PostgreSQL leads on `COPY` above ~50k rows (1.28× at 100k) and on `DROP TABLE` (2.05×), and **PostgreSQL keeps scaling from 32→64 clients (+32–41%) where Nano plateaus**.

## Install

### Supported now: Cargo / crates.io

Install Rust and Cargo first. On Linux, macOS, or WSL, the recommended installer
is `rustup`:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustc --version
cargo --version
```

On Windows, install Rust from [rustup.rs](https://rustup.rs/), then open a new
terminal so `cargo` is on `PATH`.

Install the `heliosdb-nano` CLI binary from crates.io:

```bash
cargo install heliosdb-nano --locked
heliosdb-nano --version
```

Use Nano as an embedded Rust library:

```bash
cargo add heliosdb-nano
```

Enable optional crate features when installing or adding the dependency:

```bash
cargo install heliosdb-nano --locked --features code-graph,mcp-endpoint
cargo add heliosdb-nano --features code-graph,mcp-endpoint
```

Build from source:

```bash
git clone https://github.com/HeliosDatabase/HeliosDB-Nano.git
cd HeliosDB-Nano
cargo build --release --locked
./target/release/heliosdb-nano --version
```

### Prebuilt Binaries

Grab a self-contained binary from the [**GitHub Releases**](https://github.com/HeliosDatabase/HeliosDB-Nano/releases/latest)
page — Linux (`x86_64` / `aarch64`), macOS (`aarch64`), and Windows (`x86_64`),
each with `SHA256SUMS`. No Rust toolchain required: download the archive for your
platform, verify it against `SHA256SUMS`, extract, and run the `heliosdb-nano`
binary.

```bash
# Example: Linux x86_64 (see Releases for macOS arm64 / Linux aarch64 / Windows)
VER=<release-tag>
curl -sSfL -O "https://github.com/HeliosDatabase/HeliosDB-Nano/releases/download/$VER/heliosdb-nano-$VER-x86_64-unknown-linux-gnu.tar.gz"
tar xzf "heliosdb-nano-$VER-x86_64-unknown-linux-gnu.tar.gz"
./heliosdb-nano --version
```

### Install Channels

Use Cargo or the GitHub release binaries for a verified install. Package-manager
channels such as npm, Homebrew, and container images should be treated as
available only when they are listed on the official releases page.

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
psql (server HeliosDB Nano)
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
Server version: HeliosDB-Nano
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

Native HNSW indexes — no extensions, no separate vector database. The default
path is in-process HNSW; `--features vector-persist` enables RocksDB-backed
persistent HNSW/PQ with `persistent = true`, `quantization = 'product'`, and
`rerank_precision = 'f32' | 'f16' | 'i8'`.

```sql
-- From any client (psql / mysql / REPL):
CREATE TABLE docs (
    id SERIAL PRIMARY KEY,
    title TEXT,
    embedding VECTOR(1536)
);

CREATE INDEX ON docs USING hnsw (embedding vector_cosine_ops);
CREATE INDEX docs_embedding_persistent ON docs USING hnsw (embedding)
WITH (persistent = true, quantization = 'product', rerank_precision = 'i8');

INSERT INTO docs (title, embedding)
VALUES ('Intro', '[0.1, 0.2, 0.3, ...]');

-- k-NN search
SELECT title, embedding <-> '[0.15, 0.25, ...]' AS distance
FROM docs
ORDER BY distance
LIMIT 10;
```

Distance operators: `<->` (L2), `<=>` (cosine), `<#>` (inner product).
Vector store IDs are namespace-scoped, and metadata/namespace filters are
applied before top-k selection in the REST/embedded vector-store API.

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

## Pagination — Flat at Depth with Keyset

**Keyset pagination on an indexed column is flat**: ~35 µs whether you are on
page 1 or page 900. `LIMIT … OFFSET` is **not** — it is linear in the offset,
because the skipped rows are still stepped over. Use keyset for deep lists.

Measured on this repo (N = 10 000, page = 10, embedded path, p50; full curve and
reproduction in [`perf/pagination_depth_curve.json`](perf/pagination_depth_curve.json),
harness in `tests/pagination_depth_curve.rs`):

| shape | depth 0 | depth 9 000 | growth |
|---|---|---|---|
| keyset, `WHERE id > $1` (indexed) | 39 µs | **35 µs** | **0.9× — flat** |
| `OFFSET`, no `ORDER BY` | 11 µs | 1 255 µs | 115× |
| `OFFSET` + `ORDER BY id` | 36 µs | 4 812 µs | 133× |
| `OFFSET` + `ORDER BY created_at DESC, id DESC` | 5 361 µs | 8 880 µs | 1.7× (flat, but ~5 ms — dominated by sorting every row) |
| keyset, row-constructor `(created_at, id)` | 5 282 µs | 5 867 µs | 1.1× (flat, but ~5 ms — see below) |

Two caveats worth reading before you design around this:

- **`OFFSET` is O(offset).** Skipping without `ORDER BY` is cheap *per row* (it
  avoids decode and decrypt), but it is still a step per skipped row. Earlier
  versions of this README described it as constant-time; that was wrong, and the
  ~30 µs figure it quoted matches the *keyset* path, not `OFFSET`.
- **Row-constructor keyset does not yet use an index seek.** `(a, b) < ($1, $2)`
  is evaluated as a post-scan filter, so it is flat in depth but pays a full scan
  (~5 ms at 10 000 rows versus ~35 µs for the single-column form). Planner-driven
  keyset pushdown onto `scan_table_pk_range` is the fix and is not implemented.
  Prefer a single indexed sort key until it is.

```sql
-- Keyset on an indexed column — flat at any depth, the recommended shape
SELECT id, created_at, subject
  FROM leads
 WHERE id < $1
 ORDER BY id DESC
 LIMIT 20;

-- LIMIT / OFFSET — correct, but cost grows with the offset
SELECT id, created_at, subject
  FROM leads
 ORDER BY created_at DESC, id DESC
 LIMIT 20 OFFSET 100000;

-- Keyset (row-constructor tuple) — preferred for high-volume lists
SELECT id, created_at, subject
  FROM leads
 WHERE (created_at, id) < ($1, $2)
 ORDER BY created_at DESC, id DESC
 LIMIT 20;

-- JOIN + pagination composes cleanly
SELECT l.id, l.subject, c.name AS company
  FROM leads l
  LEFT OUTER JOIN companies c ON l.company_id = c.id
 WHERE (l.created_at, l.id) < ($1, $2)
 ORDER BY l.created_at DESC, l.id DESC
 LIMIT 20;
```

Pitfalls: the sort key must be unique (always tail with `id` or another unique
column); avoid floating-point sort keys; do not mix `ASC`/`DESC` directions
inside the tuple. A `NULL` in a row-constructor comparison makes the whole
comparison unknown (PostgreSQL semantics), so rows with a `NULL` sort key are
excluded from every page — use `NOT NULL` sort keys.

Numbers above are reproduced by
`cargo test --release --test pagination_depth_curve -- --ignored --nocapture`,
which rewrites `perf/pagination_depth_curve.json`.

## Git-Like Branching — Fork-Test-Discard Sandboxes

Isolated copy-on-write branches: fork the database in an instant, test a
destructive change (a migration, an experiment, an AI agent's writes) against
real data, then discard the branch. `main` is never touched. This is the
recommended pattern for giving every agent run its own sandbox.

`CREATE BRANCH` **requires an `AS OF` clause** — use `AS OF NOW` to fork current
state, or `AS OF TIMESTAMP '…'` to fork from the past. Without it the statement is
rejected with `CREATE BRANCH requires AS OF clause` (`src/sql/parser.rs:3490`).

```sql
CREATE DATABASE BRANCH agent_run_42 FROM main AS OF NOW;
USE BRANCH agent_run_42;

-- Changes here are invisible to main
INSERT INTO products (name, price) VALUES ('Test', 0.01);
UPDATE products SET price = price * 1.1;   -- rehearse the scary change
SELECT COUNT(*), SUM(price) FROM products; -- validate it

USE BRANCH main;
DROP BRANCH agent_run_42;                  -- discard; main never changed
```

Branches can also fork from a past point in time:
`CREATE DATABASE BRANCH rewind FROM main AS OF TIMESTAMP '2026-08-24 09:00:00'`
(requires time-travel, which the `agent` profile keeps enabled).

> **⚠️ `MERGE BRANCH` moves rows, but does not detect conflicts.**
> Merging is **last-writer-wins**: if `main` and the branch both changed the same
> row, one value silently wins and you get no warning. Merge strategies are not
> implemented — `WITH (conflict_resolution = …)` errors with *not implemented*.
> `WITH (delete_branch_after = true)` is honoured and works.
>
> Merge is safe when the target has not been written since the fork. When you
> cannot guarantee that — and an agent never can — use fork-test-discard as
> designed: drop the branch and re-run the validated SQL against `main`.
>
> **An earlier version of this warning said `MERGE BRANCH` "merges nothing and
> reports success". That was wrong** and is retracted — see `CHANGELOG.md`
> [4.16.0]. The evidence came from tests driving `BranchTransaction`, an API with
> no production callers whose on-disk key encoding the real merge never reads.
> Measured, the SQL path merges correctly and always did; it is pinned by
> `tests/branch_merge_surface_tests.rs`.

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
- **User-defined functions**: **scalar calls work** — `CREATE FUNCTION dbl(x INT) RETURNS INT AS $$ SELECT $1 * 2 $$ LANGUAGE sql` then `SELECT dbl(21)`, `SELECT id, dbl(id) FROM t`, `SELECT id FROM t WHERE dbl(id) = 2`, `SELECT public.dbl(21)` and bound-parameter `SELECT dbl($1)` all return `42`/rows, on the embedded API and both wires, and the definition survives a restart. `LANGUAGE plpgsql` bodies work for `DECLARE` + SQL statements + `SELECT … INTO` + `RETURN`. One rule, identical to stored procedures: parameters and variables must carry the **`$` sigil** (`$1` / `$name`) — a bare name is a column reference. Recursion is bounded by `[session] udf_max_call_depth` (default 32). ⚠️ Still absent: set-returning functions (`SELECT * FROM f()` errors as a missing table, and `RETURNS TABLE(...)`'s column list is discarded), overloading (the registry keys on the name alone, so two same-name functions collide), `CALL f()` for a *function*, non-`public.` schema qualification, PL/pgSQL control flow inside a **function** body (`IF` / `CASE` / loops / `:=` are REFUSED with an explicit error rather than silently taking the wrong branch — the procedural expression parser stores expression text instead of parsing it), catalog visibility (`pg_proc`, `information_schema.routines` / `.parameters` are still empty), and the REST `/rest/v1/rpc/<fn>` endpoint (still HTTP 501). A function call inside an **embedded/REPL** explicit `BEGIN` is refused with an error (the body re-enters the executor and would deadlock on the global transaction lock)
- **Stored procedures**: `CREATE PROCEDURE p(a INT, b TEXT) LANGUAGE sql AS $$INSERT INTO t VALUES ($a, $b)$$` + `CALL p(1, 'x')` works, arguments included — in **both** `LANGUAGE sql` and `LANGUAGE plpgsql` bodies (plpgsql binding is new; it substituted nothing through 4.10.2), and now on **every client path**. One rule: the body must reference parameters and `DECLARE`d variables with a **`$` sigil**, by name (`$a`) or positionally (`$1`). A bare parameter name is always a column reference and fails with `Column 'a' not found in schema` — PostgreSQL resolves bare PL/pgSQL variable names, Nano deliberately does not, so a variable can never silently shadow a column. Substitution is literal-aware: a `$1` inside `'a string'`, a `-- comment` or a `$tag$…$tag$` block is data, not a placeholder. **Through 4.11.0, `CALL` ran the body only on the simple-query / MySQL / REPL / embedded `execute()` path**: over the PostgreSQL *extended* protocol (psycopg with server-side bind, JDBC, sqlx, Drizzle, node-postgres) and through the REST/BaaS layer it was a silent no-op that reported one affected row, and `CALL nonexistent_proc()` reported success. Both paths now share one implementation; `CALL` reports 0 rows affected and errors if the procedure does not exist. One limitation remains: on the **embedded API and the REPL**, a `CALL` inside an explicit `BEGIN` is refused with an error (the body would re-enter the executor and deadlock on the global transaction lock) — issue it outside the transaction. A `BEGIN` over the PG or MySQL wire is a per-session transaction and is unaffected
- **Materialized views**: `CREATE` / `REFRESH MATERIALIZED VIEW`, staleness tracking (`\dmv`, `pg_mv_staleness()`)
- **MVCC transactions**: **snapshot isolation**, with first-committer-wins write-write conflict detection (SQLSTATE `40001`). `READ COMMITTED` re-snapshots per statement; `REPEATABLE READ` takes one snapshot per transaction. ⚠️ **`SERIALIZABLE` is accepted as a name but served as snapshot isolation** — conflict validation sees only the transaction's *write* set, no read set is tracked anywhere in the engine (no SSI), so `SERIALIZABLE` is byte-identical to `REPEATABLE READ` and **does not prevent write skew**: two transactions can each read the rows the other is about to change and both commit. Requesting it raises a wire `WARNING` naming the anomaly; set `[storage] serializable_policy = "error"` to refuse the level outright rather than downgrade it. Real SSI is not implemented. ⚠️ **DDL is not transactional** — `BEGIN; DROP TABLE orders; ROLLBACK;` destroys the table permanently, and a `CREATE TABLE` inside a rolled-back transaction survives; only the DML in the block is undone. Run schema changes outside an explicit transaction, and take a dump or a branch before a destructive migration
- **COPY (PostgreSQL wire)**: `COPY … FROM STDIN` / `TO STDOUT` in text and CSV — works with `psql \copy` and high-throughput PG→Nano bulk migration
- **JSONB**: `->`, `->>`, `@>`, `?` operators
- **Full-text search**: `tsvector`, `tsquery`, `@@`, `ts_rank_cd`, `CREATE INDEX ... USING gin` (see [FTS scope](docs/compatibility/fts.md))
- **Keyset pagination**: row-constructor comparison `WHERE (col, id) < ($1, $2)`; top-K sort. Keyset on a single indexed column is flat at any depth (~35 µs); `LIMIT … OFFSET` is linear in the offset, and row-constructor keyset is a post-scan filter, not an index seek (see [Pagination](#pagination--flat-at-depth-with-keyset))
- **Foreign keys**: CASCADE, SET NULL, RESTRICT, deferred/audit/off validation modes, `NOT ENFORCED` constraints
- **Triggers**: ⚠️ **trigger bodies still do not execute** — `CREATE TRIGGER … EXECUTE FUNCTION f()` parses, registers and now persists, but no trigger body ever runs: no audit rows, no derived-column maintenance, no cascades, on any interface, with no error or warning. `AFTER` triggers, `FOR EACH STATEMENT`, `INSTEAD OF`, `OLD`, and deferred/CONSTRAINT triggers do nothing at all, and there is no trigger introspection (`pg_trigger` does not exist, `information_schema.triggers` is empty, and psql's `\d` reports `relhastriggers = false` — the column does not exist on `pg_class` on any other route). SQLite/MySQL-style `BEGIN … END` bodies do not parse at all. The single exception that has an effect: `BEFORE INSERT … FOR EACH ROW EXECUTE FUNCTION f()` where `f`'s body is top-level `NEW.<col> = <expr>` and/or `RETURN NULL` rewrites or skips the row being inserted. As of 4.20.0 that exception behaves IDENTICALLY on every interface — embedded `execute()`, the PostgreSQL simple *and* extended query protocols, the MySQL wire, REST/BaaS and `INSERT … RETURNING` (through 4.19.0 it applied only on the text executor family, so a REST insert and a `psql` insert into the same table produced different rows); it honours the trigger's `WHEN` clause and its enabled flag; and it survives a restart (a trigger created after the last checkpoint and lost to a crash is restored definition-only, i.e. inert). `CREATE TRIGGER`/`DROP TRIGGER` also stopped hard-erroring `Operator not yet implemented: CreateTrigger` over the extended protocol, and `DROP TABLE` now deregisters the table's triggers. Do not use triggers for audit logs or derived-data maintenance — do that work in the application, in an explicit second statement in the same transaction, or in a `CREATE PROCEDURE` invoked with `CALL`. Procedures execute and bind their arguments in either language, provided you use `$`-sigil parameters, and now do so on every client path (through 4.11.0 this advice was inert over the PostgreSQL extended protocol and the REST layer — see the **Stored procedures** bullet above, which also covers the one remaining `BEGIN` limitation).
- **Row-Level Security**: Per-tenant data isolation via policies
- **EXPLAIN**: Cost-based optimizer, ANALYZE, JSON/XML/YAML output
- **Code-graph** *(opt-in, `--features code-graph`)*: tree-sitter-backed AST index + `lsp_definition` / `lsp_references` / `lsp_call_hierarchy` / `lsp_hover` as Rust API & SQL table functions — see [code-graph overview](docs/code_graph/overview.md)
- **Agentic SQL hooks**: `predict`, `infer`, `generate`, and preview-only self-driving optimizer plans
- **Backup/Restore**: Compressed dumps (zstd/gzip/brotli)
- **Import/Export**: CSV, JSON, JSONL, Parquet, Arrow, SQL
- **Audit logging**: Tamper-proof trail (SHA-256 checksums)
- **Encryption**: AES-256-GCM at rest, FIPS 140-3 mode — see [what is and isn't encrypted](#encryption-at-rest) before relying on it
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

## Encryption at rest

Set `[encryption] enabled = true` (off by default) and supply a key via
`[encryption.key_source]`. Values are sealed with AES-256-GCM.

**Encrypted.** Every row image written through any SQL, wire-protocol or library route — the
live row, its MVCC version history, row-id counters, the logical WAL entries that carry those
rows, branch row overlays, materialized-view delta records, the catalog (table schemas, index
definitions, sequences, roles and ACLs) and durable index snapshots. This holds on every write
route: autocommit, batch/`COPY`, explicit and session transactions, WAL replay, `RENAME TABLE`
and `MERGE BRANCH`.

**Not encrypted — read this before relying on it:**

| Not sealed | Why it matters |
|---|---|
| **RocksDB keys** | Table names, column names, row ids, timestamps and branch ids are visible on disk. Only *values* are sealed |
| **Column sidecars for non-default `STORAGE` modes** (`col:`, `colz:`, `colp:`, `dict:`, `cas:`) | For a column declared `STORAGE COLUMNAR`, `DICTIONARY` or `CONTENT_ADDRESSED`, the row holds only a reference and the payload lives in a sidecar in the clear. With `time_travel` on (the default) the sealed version copy still holds the value; with time travel **off** there is no sealed copy anywhere |
| **HNSW graph snapshots** under `<data_dir>/hnsw_snapshots/` | Plain files outside RocksDB containing raw vectors |
| **Backup dumps** (`[dump]`, `--dump-on-shutdown`, `--dump-schedule`) | Written unencrypted; protect the dump directory separately |
| **Data written before enabling encryption** | Existing values are read as-is and never rewritten. Enabling encryption seals *new* writes; it does not retroactively seal an existing database |
| **The replication link** | Rows are shipped to standbys in the clear. Encryption at rest says nothing about the wire — use TLS |

**Wrong-key protection.** A sentinel is sealed at first open and verified strictly on every
subsequent open, so a wrong or rotated key is refused up front rather than silently yielding
unreadable rows. Opening an encrypted directory with encryption *disabled* is refused for the
same reason. A pre-existing database without a sentinel gains one only after the configured key
is shown to open data that is already there — an ambiguous database is refused rather than
stamped with a key that might be wrong.

**Key rotation is not implemented.** AES-256-GCM here uses a random 96-bit nonce under a single
static key; NIST SP 800-38D caps random-nonce GCM at 2^32 invocations per key. High-volume
deployments should track write volume against that bound.

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

For production deployments with multiple HeliosDB Nano instances, put **[HeliosProxy](https://github.com/HeliosDatabase/HeliosDB-Proxy)** in front — a standalone binary providing:

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
heliosdb-nano = "3.39"
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

**[heliosdb-sdks](https://github.com/HeliosDatabase/HeliosDB-SDKs)** — works with all HeliosDB editions.

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

**The canonical way for an AI agent to operate Nano is MCP**: build with
`--features mcp-endpoint`, then `claude mcp add heliosdb -- heliosdb-nano mcp
serve --data-dir ./mydata` (stdio; HTTP and WebSocket transports also
available). The agent gets 16 structured tools — query, schema, insert, branch
create/list/merge, time-travel, hybrid search — instead of building SQL
strings. See the `heliosdb-nano-mcp` skill for the full catalog and recipes.

For any LLM that ingests a single reference file, [`docs/llms.txt`](docs/llms.txt)
condenses install, connect, the SQL dialect, vector operators, branch
sandboxes, time-travel, and the skill catalogue into one agent-readable page.

For agent deployments, set `profile = "agent"` in the config (see
[config.example.toml](config.example.toml)): group-commit writes plus
time-travel kept ON, so branchable sandboxes, `AS OF` queries, and vector
search all work out of the box.

HeliosDB-Nano also ships an **agentic-operations skill catalogue** — 19 SKILL.md files that give an LLM-driven coding agent a full A→Z catalogue of "verbs" for operating the database (install, connect, schema, DML, transactions, branches, time-travel, backup, vector, code-graph, graph-rag, MCP, server, tenant, deploy, observability, migrate).

```bash
# After git clone, Claude Code automatically picks up .claude/skills/ in this project.

# To install globally (~/.claude/skills/) so they apply in any project:
bash scripts/install-agent-skills.sh                # copy (default, frozen snapshot)
bash scripts/install-agent-skills.sh --symlink      # symlink (live updates)
```

Existing `~/.claude/skills/heliosdb-nano-*` directories are backed up to `*.bak.<unix-ts>` before being overwritten in either mode.

**Installed via `cargo install` (no git checkout)?** The skill files ship *inside* the published crate, so they are already on disk in cargo's registry cache — but `scripts/install-agent-skills.sh` is **not** packaged. Deploy them straight from the cache. The catalogue is plain [Anthropic `SKILL.md`](https://agentskills.io), so the same files also work in **OpenCode** and **OpenAI Codex**, which scan their own global skill dirs:

```bash
# Skills bundled inside the crate you installed (newest cached version):
SRC=$(ls -d ~/.cargo/registry/src/*/heliosdb-nano-*/.claude/skills 2>/dev/null | sort -V | tail -1)
echo "Deploying skills from: $SRC"

# Claude Code + OpenCode read ~/.claude/skills/ ; Codex + OpenCode read ~/.agents/skills/
for DEST in ~/.claude/skills ~/.agents/skills; do
  mkdir -p "$DEST"
  cp -r "$SRC"/heliosdb-nano-* "$DEST"/
  cp -r "$SRC"/_index "$DEST"/heliosdb-nano-_index   # verb-map + feature-matrix (reference docs, not a skill)
done
```

| Agent | Global skill dir(s) it scans | Notes |
|-------|------------------------------|-------|
| **Claude Code** | `~/.claude/skills/` | Auto-discovered at next session start. |
| **OpenCode** | `~/.claude/skills/`, `~/.agents/skills/`, `~/.config/opencode/skills/` | Loaded on demand via the native `skill` tool; if your permission policy is `ask`/`deny`, allow that tool. |
| **OpenAI Codex** | `~/.agents/skills/` (personal), `.agents/skills/` (per-repo, team), `/etc/codex/skills/` (admin). The Codex CLI also scans `~/.codex/skills/`. | Auto-selected by `description`, or invoke `/skills` / `$heliosdb-nano-…`. Restart Codex if new skills don't appear. |

The cache path exists once cargo has extracted the crate (`cargo install` does this). If cargo GC'd it, run `cargo fetch heliosdb-nano` or reinstall. To pin a specific version instead of newest-cached, replace the `SRC=` glob with `…/heliosdb-nano-<version>/.claude/skills`. The `_index` folder has no `SKILL.md`, so OpenCode/Codex ignore it as a skill (it stays available as reference docs).

| Skill | What it covers |
|-------|---------------|
| `heliosdb-nano-overview` | Top-level navigation; routes to the domain skills |
| `heliosdb-nano-install` | crates.io, source, feature flags (code-graph, mcp-endpoint, fips, ha-full…) |
| `heliosdb-nano-connect` | Embedded library, REPL, PG wire, MySQL wire, Python sqlite3 drop-in, TLS |
| `heliosdb-nano-schema` | DDL: tables, indexes (B-tree + HNSW), views; why `CREATE TRIGGER` registers but trigger bodies never run, and what user-defined functions can and cannot do |
| `heliosdb-nano-query` | DML, parameter styles (`?` `$1` `:name` `@name`), `ON CONFLICT`, `RETURNING` |
| `heliosdb-nano-transactions` | BEGIN/COMMIT/ROLLBACK, savepoints, bulk-load patterns |
| `heliosdb-nano-branches` | Fork-test-discard sandboxes: `CREATE/USE/DROP DATABASE BRANCH`, `AS OF` forks |
| `heliosdb-nano-time-travel` | `SELECT … AS OF TIMESTAMP '…'`, `\snapshots` |
| `heliosdb-nano-backup` | `dump`/`restore`, compression, append, partial restore, `--dump-schedule` |
| `heliosdb-nano-vector` | HNSW indexes, `<-> <#> <=>` operators, hybrid search |
| `heliosdb-nano-code-graph` | AST symbol index, LSP queries, git hook (`code-graph` feature) |
| `heliosdb-nano-graph-rag` | Knowledge graph + RAG ingest pipeline (`graph-rag` feature) |
| `heliosdb-nano-mcp` | MCP server, 16-tool catalog, stdio/HTTP/WS (`mcp-endpoint` feature) |
| `heliosdb-nano-server` | Daemon, TLS, auth, HA tier 1/2/3, user management |
| `heliosdb-nano-tenant` | Multi-tenant isolation modes, tiered plans, RLS policies |
| `heliosdb-nano-deploy` | Docker, Fly.io, Railway, Render, systemd template |
| `heliosdb-nano-observability` | Tracing, slow-query log, `/health`, `\stats`, `\optimize`, `\indexes` |
| `heliosdb-nano-migrate` | sqlite3 / Postgres / MySQL drop-in checklists |
| `heliosdb-nano-merge-validation` | This repo's 8-phase pre-merge validation methodology (contributors) |

Lookups: [`.claude/skills/_index/verb-map.md`](.claude/skills/_index/verb-map.md) (every CLI flag / REPL meta-command / public Rust API method / MCP tool) · [`.claude/skills/_index/feature-matrix.md`](.claude/skills/_index/feature-matrix.md) (cargo feature ↔ skill).

## Documentation

In-repo guides (`docs/`):

- [Upgrade between Nano versions](docs/guides/upgrade.md)
- [Database management — `CREATE` / `DROP DATABASE`](docs/guides/database_management.md)
- [Authentication — SCRAM-SHA-256, trust, password, TLS](docs/guides/authentication.md)
- [`information_schema` compatibility](docs/compatibility/information_schema.md)
- [SQLite drop-in tutorial](docs/guides/sqlite_drop_in_tutorial.md)
- [REPL demo](docs/guides/repl_demo.md)
- [HA cluster tutorial](docs/guides/ha_cluster_tutorial.md) · [HA testing](docs/guides/ha_testing.md)
- [Audit logging](docs/guides/audit.md)
- [Tracing guide](docs/TRACING_GUIDE.md)
- [Full-text search](docs/compatibility/fts.md) · [PL/pgSQL compatibility](docs/compatibility/plpgsql.md) · [SQLite compatibility](docs/compatibility/sqlite.md)
- [Code-graph overview](docs/code_graph/overview.md)

Hosted docs:

- [Getting Started](https://heliosdb.com/nano.html)
- [API Explorer (Swagger UI)](http://localhost:8080/docs) — when running locally
- [vs Supabase](https://heliosdb.com/vs-supabase.html)
- [vs Firebase](https://heliosdb.com/vs-firebase.html)
- [vs PostgreSQL](https://heliosdb.com/vs-postgresql.html)
- [vs SQLite](https://heliosdb.com/vs-sqlite.html)
- [Migrate from MySQL](https://heliosdb.com/migrate-mysql.html)

## License

[Apache-2.0](LICENSE) — Apache License, Version 2.0
