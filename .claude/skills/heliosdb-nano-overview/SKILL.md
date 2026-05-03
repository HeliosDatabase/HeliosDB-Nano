---
name: heliosdb-nano-overview
description: Top-level navigation for HeliosDB-Nano. Auto-loads when user mentions "heliosdb", "heliosdb-nano", or pastes its CLI/REPL output. Routes to one of 16 domain skills (install, connect, schema, query, transactions, branches, time-travel, backup, vector, code-graph, graph-rag, mcp, server, deploy, observability, migrate). Use this skill to find the right skill before going deep.
allowed-tools: Bash(heliosdb-nano *), Bash(cargo info *), Read, Grep, Glob
---

# HeliosDB-Nano — Operational Overview

## When to use
Any task involving HeliosDB-Nano. This skill is the index — it answers "which skill should I read?" and gives a one-shot orientation. After picking the relevant domain skill below, follow that skill's recipes.

## What is Nano
Single-binary embedded database (Rust). PostgreSQL- and MySQL-wire compatible. Optional: vector search, code-graph indexing, MCP server, FIPS, HA tiers. Binary name: `heliosdb-nano`. Crate: `heliosdb-nano` on crates.io.

## Pick a skill

| If the task is about… | Read |
|----------------------|------|
| Installing the binary, building from source, listing features | `heliosdb-nano-install` |
| Opening a connection (embedded, PG wire, MySQL wire, REPL, Python) | `heliosdb-nano-connect` |
| Tables / indexes / views / triggers / introspection | `heliosdb-nano-schema` |
| INSERT / UPDATE / DELETE / SELECT / EXPLAIN | `heliosdb-nano-query` |
| BEGIN / COMMIT / SAVEPOINT / bulk-load patterns | `heliosdb-nano-transactions` |
| Branching: create / switch / merge / drop / AS OF clones | `heliosdb-nano-branches` |
| Time-travel queries (`AS OF <ts>`), snapshots, LSN | `heliosdb-nano-time-travel` |
| Backup / restore (dump / restore subcommands) | `heliosdb-nano-backup` |
| HNSW vector indexes, similarity search, hybrid search | `heliosdb-nano-vector` |
| Indexing source code, LSP-style queries, git hook | `heliosdb-nano-code-graph` |
| RAG ingest (PDF/docs/email/QA), seed+expand+rerank | `heliosdb-nano-graph-rag` |
| MCP server, 16-tool catalog, stdio/HTTP/WS transports | `heliosdb-nano-mcp` |
| Daemon mode, TLS, auth, HA tiers, user management | `heliosdb-nano-server` |
| Docker, Fly.io, Railway, Render, systemd | `heliosdb-nano-deploy` |
| Tracing, slow-query log, /health, metrics | `heliosdb-nano-observability` |
| Migrating from sqlite3 / Postgres / MySQL | `heliosdb-nano-migrate` |
| Pre-merge validation methodology (required before any release) | `heliosdb-nano-merge-validation` |
| Multi-tenancy: tenants, plans, isolation modes, RLS policies | `heliosdb-nano-tenant` |

## Sanity-check the install (one-liner)
```bash
heliosdb-nano --version && heliosdb-nano --help
```
Expected: version `≥ 3.22.x`, then a usage block listing subcommands `start | stop | status | init | repl | dump | restore` (and `code-graph` if built with `--features code-graph`).

## What the binary does NOT include by default
Without explicit cargo features, these surfaces are absent — recipes that need them will fail with "unknown subcommand" or "feature not enabled":
- `code-graph` subcommand → needs `--features code-graph`
- MCP server → needs `--features mcp-endpoint`
- Local embedder → needs `--features code-embed`
- FIPS crypto → needs `--features fips` (and `--no-default-features`)
- HA tier 2/3, dedup, A/B routing → needs `--features ha-full` (or per-tier flags)

Default features that ARE on: `encryption`, `vector-search`, `ring-crypto`, `ha-tier1`.

See `heliosdb-nano-install` for build recipes and `_index/feature-matrix.md` for the full flag → skill mapping.

## Verb map at a glance
For the full A→Z verb catalog (every CLI flag, REPL meta-command, public API method, MCP tool), see [`_index/verb-map.md`](../_index/verb-map.md).

## See also
- `AGENTS.md` at repo root — the Codex-/generic-agent-readable aggregated reference.
- `README.md` "Agentic Operations" section — how to install these skills globally.
