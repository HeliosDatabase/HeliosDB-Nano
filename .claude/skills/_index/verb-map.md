---
name: heliosdb-nano-verb-map
description: Full A→Z catalogue of every verb (CLI flag, REPL meta-command, public Rust API method, MCP tool, SQL statement) exposed by HeliosDB-Nano. Use this as a lookup index when you remember the operation name but not which surface or skill exposes it.
type: reference
---

# HeliosDB-Nano — Full Verb Catalogue

Source of truth: `src/main.rs`, `src/repl/commands.rs`, `src/lib.rs`, `src/mcp/tools.rs`, `Cargo.toml`. Each entry below names the surface + the skill where the recipe lives.

## CLI subcommands (`heliosdb-nano <subcmd>`)

| Subcommand | Purpose | Skill |
|------------|---------|-------|
| `start` | Launch server (PG/MySQL wires + HTTP health) | `heliosdb-nano-server` |
| `stop` | Graceful shutdown via PID file | `heliosdb-nano-server` |
| `status` | Check daemon status | `heliosdb-nano-server` |
| `init` | Create empty data directory | `heliosdb-nano-install` |
| `repl` | Interactive shell (embedded mode) | `heliosdb-nano-connect` |
| `dump` | Backup to file (zstd/gzip/brotli/none) | `heliosdb-nano-backup` |
| `restore` | Restore from backup file | `heliosdb-nano-backup` |
| `code-graph hook` | Git-hook source indexer (`code-graph` feature) | `heliosdb-nano-code-graph` |

## CLI flags (`heliosdb-nano start …`)

| Flag | Default | Purpose | Skill |
|------|---------|---------|-------|
| `--data-dir <p>` | — | File-backed data directory | `heliosdb-nano-install`, `-server` |
| `--memory` | false | In-memory mode (no persistence) | `heliosdb-nano-install`, `-server` |
| `--port <n>` | `5432` | PG-wire TCP port | `heliosdb-nano-server` |
| `--listen <addr>` | `127.0.0.1` | TCP bind address | `heliosdb-nano-server` |
| `--config <f>` | — | TOML config file | `heliosdb-nano-server` |
| `--daemon` | false | Run in background | `heliosdb-nano-server` |
| `--pid-file <p>` | `./heliosdb.pid` | Daemon PID file | `heliosdb-nano-server` |
| `--dump-on-shutdown` | false | Persist `--memory` DB on shutdown | `heliosdb-nano-backup` |
| `--dump-schedule <cron>` | — | Periodic background dump | `heliosdb-nano-backup` |
| `--tls-cert <f>` / `--tls-key <f>` | — | TLS in transit (PEM) | `heliosdb-nano-server` |
| `--auth <mode>` | `trust` | `trust\|password\|md5\|scram-sha-256` | `heliosdb-nano-server` |
| `--password <s>` | — | Admin password seed (non-trust) | `heliosdb-nano-server` |
| `--replication-role <r>` | `standalone` | `standalone\|primary\|standby\|observer` | `heliosdb-nano-server` |
| `--replication-port <n>` | `5433` | WAL streaming port | `heliosdb-nano-server` |
| `--primary-host <hp>` | — | Standby's primary target | `heliosdb-nano-server` |
| `--standby-hosts <list>` | — | Primary's standby list | `heliosdb-nano-server` |
| `--observer-hosts <list>` | — | Split-brain protection | `heliosdb-nano-server` |
| `--sync-mode <m>` | `async` | `async\|semi-sync\|sync` | `heliosdb-nano-server` |
| `--http-port <n>` | `8080` | `/health` endpoint port | `heliosdb-nano-observability` |
| `--node-id <uuid>` | auto | Cluster node identifier | `heliosdb-nano-server` |
| `--mysql` | false | Enable MySQL-wire listener | `heliosdb-nano-connect`, `-server` |
| `--mysql-listen <addr>` | `127.0.0.1:3306` | MySQL TCP bind | `heliosdb-nano-connect`, `-server` |
| `--mysql-socket <p>` | — | MySQL Unix socket (PHP/WP) | `heliosdb-nano-connect` |
| `--pg-socket-dir <d>` | — | PG Unix socket dir (`<d>/.s.PGSQL.<port>`) | `heliosdb-nano-connect` |

## REPL meta-commands

| Command | Purpose | Skill |
|---------|---------|-------|
| `\q`, `\quit`, `\exit` | Exit REPL | `heliosdb-nano-connect` |
| `\h`, `\help`, `\?` | Help | `heliosdb-nano-connect` |
| `\d` | List tables | `heliosdb-nano-schema` |
| `\d <table>` | Describe table | `heliosdb-nano-schema` |
| `\dt` | List tables (detail) | `heliosdb-nano-schema` |
| `\dS [view]` | List/describe system views | `heliosdb-nano-schema` |
| `\dmv [view]` | List/describe materialized views | `heliosdb-nano-schema` |
| `\timing` | Toggle query timing | `heliosdb-nano-query` |
| `\branches` | List database branches | `heliosdb-nano-branches` |
| `\use <branch>` | Switch active branch | `heliosdb-nano-branches` |
| `\show branch` | Show current branch ID | `heliosdb-nano-branches` |
| `\snapshots` | List time-travel snapshots | `heliosdb-nano-time-travel` |
| `\show lsn` | Toggle LSN display | `heliosdb-nano-time-travel` |
| `\compression [t]` | Compression statistics | `heliosdb-nano-observability` |
| `\set [var] [val]` | Show/set REPL variables | `heliosdb-nano-server` |
| `\server [start\|stop\|status]` | Server-mode control | `heliosdb-nano-server` |
| `\ssl [status]` | TLS status | `heliosdb-nano-server` |
| `\user [list\|add\|remove]` | User management | `heliosdb-nano-server` |
| `\password <user>` | Change password | `heliosdb-nano-server` |
| `\config [reload]` | Show/reload config | `heliosdb-nano-server` |
| `\optimize <t>` | Optimization hints | `heliosdb-nano-observability` |
| `\indexes <t>` | Index recommendations | `heliosdb-nano-observability` |
| `\stats` | Database statistics | `heliosdb-nano-observability` |
| `\dump [file]` | SQL-level export | `heliosdb-nano-backup` |
| `\ai templates\|template <n>\|infer\|generate <d>\|optimize <t>` | AI schema inference | `heliosdb-nano-schema` |
| `\tenants`, `\tenant …` | Multi-tenancy | (advanced — `\help` in REPL) |

## Public library API (`EmbeddedDatabase`)

| Method | Purpose | Skill |
|--------|---------|-------|
| `new(path)` | Open file-backed DB | `heliosdb-nano-connect` |
| `new_in_memory()` | Open in-memory DB | `heliosdb-nano-connect` |
| `with_config(cfg)` | Open with custom config | `heliosdb-nano-connect` |
| `execute(sql)` | Execute, return row count | `heliosdb-nano-query` |
| `execute_params(sql, &[…])` | Parameterized execute | `heliosdb-nano-query` |
| `execute_returning(sql)` | Execute + result set | `heliosdb-nano-query` |
| `execute_params_returning(sql, &[…])` | Parameterized + result set | `heliosdb-nano-query` |
| `execute_batch(sqls)` | Multi-statement batch | `heliosdb-nano-query` |
| `query(sql, &[…])` | Read tuples | `heliosdb-nano-query` |
| `query_with_columns(sql)` | Tuples + column names | `heliosdb-nano-query` |
| `begin_transaction()` | Start RAII transaction | `heliosdb-nano-transactions` |
| `begin()` / `commit()` / `rollback()` | Simple txn API | `heliosdb-nano-transactions` |
| `in_transaction()` | Check active txn | `heliosdb-nano-transactions` |
| `create_branch(name)` | New branch | `heliosdb-nano-branches` |
| `switch_branch(name)` | Activate branch | `heliosdb-nano-branches` |
| `merge_branch(src)` | Merge source into current | `heliosdb-nano-branches` |
| `drop_branch(name)` | Delete branch | `heliosdb-nano-branches` |
| `list_branches()` | Enumerate branches | `heliosdb-nano-branches` |
| `dump_full(path)` | Full backup | `heliosdb-nano-backup` |
| `restore_from_dump(path)` | Restore | `heliosdb-nano-backup` |
| `restore_tables(path, [t…])` | Partial restore | `heliosdb-nano-backup` |
| `register_grammar(name, lang)` | Register tree-sitter grammar | `heliosdb-nano-code-graph` |
| `code_index()` | Project-wide AST ingest | `heliosdb-nano-code-graph` |
| `lsp_definition/_references/_call_hierarchy/_hover` | LSP queries | `heliosdb-nano-code-graph` |
| `code_graph_merkle_refresh()` | Recompute content hashes | `heliosdb-nano-code-graph` |
| `graph_rag_search(q)` | Seed/expand/rerank search | `heliosdb-nano-graph-rag` |
| `graph_rag_link_exact/_vector` | Add typed graph links | `heliosdb-nano-graph-rag` |
| `graph_rag_ingest_{docs,pdf,office,audio,image,email,issues,qa}` | Domain ingest | `heliosdb-nano-graph-rag` |
| `insert_vectors(store, vecs)` | Bulk vector insert | `heliosdb-nano-vector` |
| `delete_vectors(store, ids)` | Vector delete | `heliosdb-nano-vector` |
| `delete_vector_store(name)` | Drop a vector store | `heliosdb-nano-vector` |

## SQL surface (highlights)

| Statement | Skill |
|-----------|-------|
| `CREATE / ALTER / DROP TABLE / INDEX / VIEW / MATERIALIZED VIEW / TRIGGER / FUNCTION` | `heliosdb-nano-schema` |
| `INSERT [OR REPLACE/IGNORE] / … ON CONFLICT … / … RETURNING` | `heliosdb-nano-query` |
| `UPDATE … RETURNING` / `DELETE … RETURNING` / `MERGE` | `heliosdb-nano-query` |
| `SELECT` (windows, CTEs, set ops) / `EXPLAIN [ANALYZE]` | `heliosdb-nano-query` |
| `BEGIN / COMMIT / ROLLBACK / SAVEPOINT … RELEASE / ROLLBACK TO` | `heliosdb-nano-transactions` |
| `CREATE DATABASE BRANCH … [PARENT … AS OF …]` / `USE BRANCH` / `MERGE` / `DROP` | `heliosdb-nano-branches` |
| `SELECT … FROM t AS OF TIMESTAMP '…'` | `heliosdb-nano-time-travel` |
| `CREATE INDEX … USING HNSW` / `<->`, `<#>`, `<=>` operators | `heliosdb-nano-vector` |
| `PRAGMA <foreign_keys\|table_info\|journal_mode\|synchronous\|busy_timeout>` | `heliosdb-nano-migrate` |
| `SHOW ALL` / `SET <var> = …` | `heliosdb-nano-server` |

## MCP tools (`mcp-endpoint` feature, 16 tools)

| Tool | Skill |
|------|-------|
| `heliosdb_query`, `_schema`, `_list_tables`, `_create_table`, `_insert` | `heliosdb-nano-mcp` |
| `heliosdb_branch_{create,list,merge}` | `heliosdb-nano-mcp` (+ `heliosdb-nano-branches`) |
| `heliosdb_search`, `_time_travel` | `heliosdb-nano-mcp` |
| `heliosdb_bm25_index`, `_hybrid_search` | `heliosdb-nano-mcp` (+ `heliosdb-nano-vector`) |
| `heliosdb_graph_{add_edge,traverse,path}` | `heliosdb-nano-mcp` (+ `heliosdb-nano-graph-rag`) |
| `heliosdb_embed_and_store` | `heliosdb-nano-mcp` (+ `heliosdb-nano-vector`) |

## Cargo features

See `_index/feature-matrix.md` (or `Cargo.toml:189-299`) for the full table mapping each feature to the skill that uses it.

## Configuration sources

| Source | Purpose | Skill |
|--------|---------|-------|
| `config.toml` (`[storage] [server] [encryption] [performance] [session] [locks] [materialized_views] [vector] [audit] [optimizer] [compression] [sync]`) | Runtime knobs | `heliosdb-nano-server` |
| Env `HELIOSDB_PRIMARY_PG_PORT` | Standby's forwarded write target | `heliosdb-nano-server` |
| Env `RUST_LOG` | Tracing level | `heliosdb-nano-observability` |
