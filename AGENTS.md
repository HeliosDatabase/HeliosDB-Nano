# HeliosDB-Nano — Agentic Operations Reference

This file is the **vendor-neutral aggregated reference** for any LLM-driven coding agent (OpenAI Codex CLI, generic AI tools, anything that reads `AGENTS.md` at the project root). For Claude Code, the same content lives as separately-discoverable skills under `.claude/skills/heliosdb-nano-*/`.

## What is HeliosDB-Nano

Single-binary embedded database (Rust). PostgreSQL- and MySQL-wire compatible. Ships from one crate published to crates.io: `heliosdb-nano`. Default features: at-rest encryption, vector search (HNSW), HA tier 1 (warm standby), `ring` crypto. Opt-in: code-graph indexing, MCP server, FIPS, multi-active-active replication, sharding.

```
Binary:  heliosdb-nano               # the CLI you run
Library: use heliosdb_nano::{…}      # the Rust crate you depend on
```

## Skill catalogue

When you (the agent) need to perform an operation in this database, find the right entry below, then read the corresponding skill file in `.claude/skills/<name>/SKILL.md` for full recipes. The verb tables below are not exhaustive — they are quick references; the skill files are the source of truth.

### Foundational

#### `heliosdb-nano-overview`
Top-level navigation. Describes which skill covers which area. Read this first if unsure where to look.

#### `heliosdb-nano-install`
Install via crates.io, build from source, init a data directory, list compiled features. Default: `cargo install heliosdb-nano`. Add features via `--features <list>` (e.g., `code-graph,mcp-endpoint`). FIPS requires `--no-default-features --features fips,encryption,vector-search`.

#### `heliosdb-nano-connect`
Open a connection in any of five modes:
- **Embedded (Rust)**: `EmbeddedDatabase::new(path)` / `::new_in_memory()` / `::with_config(cfg)`
- **REPL**: `heliosdb-nano repl --data-dir <p> | --memory`
- **PG wire**: `psql -h 127.0.0.1 -p 5432 -U heliosdb` (server: `heliosdb-nano start --data-dir <p>`)
- **MySQL wire**: `mysql -h 127.0.0.1 -P 3306` (server: `start --mysql`)
- **Python sqlite3 drop-in**: `from heliosdb_sqlite import connect`

### Schema & data

#### `heliosdb-nano-schema`
DDL: `CREATE / ALTER / DROP` for tables, indexes (B-tree + HNSW), views, materialized views, triggers, PL/pgSQL functions. Multi-op `ALTER TABLE` is atomic per statement. Introspection in 3 dialects: Postgres (`information_schema`), SQLite (`sqlite_master`, `PRAGMA table_info`), Nano (`\d`, `\dt`, `\dS`, `\dmv`).

#### `heliosdb-nano-query`
DML: `INSERT [OR REPLACE/IGNORE]`, `… ON CONFLICT … DO UPDATE/NOTHING`, `… RETURNING`, `INSERT … SELECT`, `UPDATE`, `DELETE`, `MERGE`. `EXPLAIN [ANALYZE]`. Window functions, recursive CTEs, set operations. Parameter styles: `?` / `$1` / `:name` / `@name` (don't mix `?` and `$N` in one statement).

#### `heliosdb-nano-transactions`
`BEGIN / COMMIT / ROLLBACK`, `SAVEPOINT … RELEASE / ROLLBACK TO`, RAII `Transaction<'_>` from the embedded API. Bulk-load patterns: chunk into ~1000-row multi-row INSERTs inside one transaction; commit every ~10k rows for very large loads.

### Storage features

#### `heliosdb-nano-branches`
Branching DDL: `CREATE DATABASE BRANCH <n> [PARENT <p>] [AS OF <ts>]`, `USE BRANCH <n>`, `MERGE`, `DROP`. REPL: `\branches`, `\use <branch>`, `\show branch`. Per-branch isolation; merge into parent on completion. Used for migrations, A/B tests, transient experiments.

#### `heliosdb-nano-time-travel`
`SELECT … FROM t AS OF TIMESTAMP '<iso>';`. REPL: `\snapshots`, `\show lsn`. Retention configurable in `[storage]`.

#### `heliosdb-nano-backup`
`heliosdb-nano dump --output <f> [--compression zstd|gzip|brotli|none] [--append]`. `heliosdb-nano restore --input <f> --target <p> [--verify]`. Library: `db.dump_full(path)`, `db.restore_from_dump(path)`, `db.restore_tables(path, [t…])`. Periodic dump via `--dump-schedule "0 */6 * * *"`.

### Search & RAG (feature-gated)

#### `heliosdb-nano-vector` (default `vector-search` feature)
`CREATE INDEX … USING HNSW (col) WITH (dim = N, metric = '…')`. Operators: `<->` (L2), `<#>` (negative inner product), `<=>` (cosine). Library: `insert_vectors`, `delete_vectors`, `delete_vector_store`. Hybrid search via `heliosdb_hybrid_search` MCP tool combines BM25 + vector.

#### `heliosdb-nano-code-graph` (`code-graph` feature)
Index a repository's source code as AST symbols + references. Languages: Rust, Python, TypeScript, Go, Markdown, SQL. Library: `register_grammar`, `code_index()`, `lsp_definition/_references/_call_hierarchy/_hover`, `code_graph_merkle_refresh()`. CLI: `heliosdb-nano code-graph hook --data-dir … --source-table src` (reads `git diff-tree` from stdin). Tables: `_hdb_code_symbols`, `_hdb_code_symbol_refs`.

#### `heliosdb-nano-graph-rag` (`graph-rag` feature, implies `code-graph`)
Knowledge-graph + RAG ingest pipeline. Library: `graph_rag_search(q)`, `graph_rag_link_exact/_vector`, `graph_rag_ingest_{docs,pdf,office,audio,image,email,issues,qa}`. Tables: `_hdb_graph_*`. Implements seed-and-expand graph traversal with vector reranking.

#### `heliosdb-nano-mcp` (`mcp-endpoint` feature)
JSON-RPC 2.0 dispatcher with stdio / HTTP / WebSocket transports. 16-tool catalog: `heliosdb_query`, `_schema`, `_list_tables`, `_create_table`, `_insert`, `_branch_{create,list,merge}`, `_search`, `_time_travel`, `_bm25_index`, `_hybrid_search`, `_graph_{add_edge,traverse,path}`, `_embed_and_store`. Wire into Claude Code via `claude mcp add heliosdb -- heliosdb-nano mcp serve` (see skill for the full client-side recipe).

### Operations

#### `heliosdb-nano-server`
Daemon mode, TLS, auth (trust|password|md5|scram-sha-256), HA replication tiers, user management. CLI: `heliosdb-nano start [--data-dir <p>|--memory] [--port 5432] [--listen 127.0.0.1] [--daemon] [--pid-file <f>] [--tls-cert <c> --tls-key <k>] [--auth <m>] [--password <s>] [--mysql] [--replication-role <r>] [--sync-mode <m>]`. Stop: `heliosdb-nano stop --pid-file <f>`. Status: `heliosdb-nano status --pid-file <f>`.

#### `heliosdb-nano-deploy`
Pre-baked deployment recipes:
- **Docker**: `Dockerfile.binary`, `deployment/docker/{Dockerfile,docker-compose.yml}`
- **Fly.io**: `deployment/flyio/fly.toml`
- **Railway**: `deployment/railway/railway.toml`
- **Render**: `deployment/render/render.yaml`
- **Bare-metal install**: `scripts/install-nano-pilot.sh`
- **systemd / k8s**: not pre-baked; templates documented in the skill.

#### `heliosdb-nano-observability`
Tracing: `RUST_LOG=info,heliosdb=debug heliosdb-nano start …`. Slow-query log: 1 s default threshold, WARN level. HTTP `/health` endpoint: `GET http://<host>:<http-port>/health`. REPL: `\stats`, `\compression [t]`, `\optimize <t>`, `\indexes <t>`. Prometheus scraping where wired.

### Compatibility

#### `heliosdb-nano-migrate`
- **From sqlite3 (Python)**: install `heliosdb_sqlite` SDK; change `import sqlite3` → `from heliosdb_sqlite import dbapi as sqlite3`. Most apps work unchanged.
- **From PostgreSQL**: any PG-wire client (psycopg2, sqlx, pg8000) connects unchanged.
- **From MySQL**: start with `--mysql`, `--mysql-listen`, optional `--mysql-socket`. PHP/WordPress works against the socket.
- **Dialect autodetect**: `--dialect=auto` is default; `--dialect=sqlite|postgres|mysql` to force.

## Verb catalogue

The full A→Z catalogue (every CLI flag, REPL meta-command, public Rust API method, MCP tool, SQL surface) lives at `.claude/skills/_index/verb-map.md`. The cargo-feature → skill mapping lives at `.claude/skills/_index/feature-matrix.md`.

## Installing these skills globally

By default, `git clone` makes the skills available to Claude Code in this project only. To install them into `~/.claude/skills/` so Claude Code can use them in any project:

```bash
./scripts/install-agent-skills.sh                 # copy (default)
./scripts/install-agent-skills.sh --symlink       # symlink (live updates)
```

Existing `~/.claude/skills/heliosdb-nano-*` directories are backed up to `*.bak.<unix-ts>` before being overwritten.

## Tips for agents

1. **Check the feature gate before recommending a recipe**. If the user runs `heliosdb-nano code-graph --help` and gets "unknown subcommand", the binary was built without `--features code-graph`. Tell them to reinstall.
2. **Default port 5432 collides with PostgreSQL**. If both run on the host, use `--port 5433` or stop PG.
3. **Single-process embedded mode**. Multiple OS processes opening the same data dir → lock errors. For multi-process, run `start` and use PG wire.
4. **Cross-process `INSERT … ON CONFLICT (path) DO UPDATE`** has a known regression when re-attaching to a populated DB from a different process — see `FEATURE_REQUEST_cross_process_on_conflict.md`. Single-process workflows are unaffected.
5. **Always show the user `heliosdb-nano --version`** before recommending version-gated recipes. Skills assume `≥ 3.22.x`.

---

Source of truth: `src/main.rs`, `src/repl/commands.rs`, `src/lib.rs`, `Cargo.toml`. When in doubt, prefer the skill file over the inline summary above.
