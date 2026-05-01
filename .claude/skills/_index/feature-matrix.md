---
name: heliosdb-nano-feature-matrix
description: Cross-reference between cargo features and the skills that need them. Use this to decide which `--features <list>` to pass to `cargo install heliosdb-nano`, and to verify that a given recipe will work on a given build.
type: reference
---

# Cargo Feature → Skill Matrix

Authoritative source: `Cargo.toml:189-299`. Re-check with `cargo info heliosdb-nano --features` for the published-on-crates-io flags.

## Default features (always on)

| Feature | Provides | Skills using it |
|---------|----------|------------------|
| `encryption` | TDE at-rest (AES-256-GCM) | `heliosdb-nano-server` (encryption config) |
| `vector-search` | HNSW indexing, PQ codebook | `heliosdb-nano-vector`, `heliosdb-nano-mcp` (`hybrid_search`), `heliosdb-nano-graph-rag` |
| `ring-crypto` | `ring` + BLAKE3 crypto provider | `heliosdb-nano-server` (TLS, password hashing) |
| `ha-tier1` | WAL streaming, automatic failover | `heliosdb-nano-server` (HA recipes), `heliosdb-nano-deploy` |

## Optional features

| Feature | Adds | Implies | Required for skills |
|---------|------|---------|----------------------|
| `code-graph` | tree-sitter (Rust/Python/TS/Go/MD/SQL), `_hdb_code_*` tables, LSP API | — | `heliosdb-nano-code-graph` |
| `graph-rag` | `_hdb_graph_*` schema, seed/expand/rerank | `code-graph` | `heliosdb-nano-graph-rag`, MCP graph tools |
| `code-embed` | fastembed-rs (ONNX) in-process embedder | `code-graph` | `heliosdb_embed_and_store` MCP tool |
| `mcp-endpoint` | JSON-RPC 2.0, stdio/HTTP/WS, 16-tool catalog | — | `heliosdb-nano-mcp` |
| `compression` | MySQL wire-protocol packet compression (zlib) | — | `heliosdb-nano-connect` (high-bandwidth MySQL) |
| `server` | Server-mode async runtime knobs | — | (advanced server builds) |

## Crypto providers (mutually exclusive)

Pick exactly one. `ring-crypto` is in `default` — drop it explicitly when enabling `fips`.

| Feature | Use case | Skills |
|---------|----------|--------|
| `ring-crypto` (default) | Standard production, BLAKE3 | `heliosdb-nano-server` |
| `fips` | FIPS 140-3 (cert #4816), regulated environments | `heliosdb-nano-server`, `heliosdb-nano-deploy` |

```bash
# FIPS build — note --no-default-features
cargo install heliosdb-nano \
    --no-default-features \
    --features fips,encryption,vector-search,ha-tier1
```

## HA tiers

| Feature | Adds | Implies | Skills |
|---------|------|---------|--------|
| `ha-tier1` (default) | Warm standby (active-passive), WAL streaming | — | `heliosdb-nano-server` |
| `ha-tier2` | Multi-primary (active-active), branch-based replication, vector-clock conflicts | `ha-tier1` | `heliosdb-nano-server`, `heliosdb-nano-branches` |
| `ha-tier3` | Sharding (consistent hash ring, cross-shard routing) | — | `heliosdb-nano-server`, `heliosdb-nano-deploy` |
| `ha-dedup` | Content-addressed dedup across nodes | — | `heliosdb-nano-server` |
| `ha-ab-testing` | Branch-to-experiment routing | — | `heliosdb-nano-branches`, `heliosdb-nano-server` |
| `ha-branch-replication` | Selective branch sync to remote replicas | `ha-tier2` | `heliosdb-nano-branches`, `heliosdb-nano-server` |
| `ha-full` | Bundle: all `ha-*` features | (all of the above) | `heliosdb-nano-server`, `heliosdb-nano-deploy` |

## Build presets (common combinations)

| Preset | Command | Use case |
|--------|---------|----------|
| Default | `cargo install heliosdb-nano` | Most users |
| AI-coding | `cargo install heliosdb-nano --features code-graph,mcp-endpoint` | Claude Code / Codex agents over a codebase |
| AI-coding + local embed | `cargo install heliosdb-nano --features code-embed,mcp-endpoint` | `code-graph` + in-process embeddings |
| Full RAG | `cargo install heliosdb-nano --features graph-rag,code-embed,mcp-endpoint` | End-to-end RAG (no external embedder) |
| FIPS-compliant server | `cargo install heliosdb-nano --no-default-features --features fips,encryption,vector-search,ha-tier1` | Regulated environments |
| HA bundle | `cargo install heliosdb-nano --features ha-full` | Multi-region clusters |
| MySQL-only deploy | `cargo install heliosdb-nano --features compression` | PHP/WordPress/MariaDB drop-in |

## Skill ↔ feature reverse lookup

| Skill | Required features | Optional features |
|-------|-------------------|-------------------|
| `heliosdb-nano-overview` | (none) | — |
| `heliosdb-nano-install` | (none) | (this is the meta-skill) |
| `heliosdb-nano-connect` | (none) | `compression` for MySQL |
| `heliosdb-nano-schema` | (none) | `vector-search` for HNSW DDL (default) |
| `heliosdb-nano-query` | (none) | — |
| `heliosdb-nano-transactions` | (none) | — |
| `heliosdb-nano-branches` | (none) | `ha-tier2`, `ha-branch-replication`, `ha-ab-testing` |
| `heliosdb-nano-time-travel` | (none) | — |
| `heliosdb-nano-backup` | (none) | — |
| `heliosdb-nano-vector` | `vector-search` (default) | `code-embed` for in-process embedder |
| `heliosdb-nano-code-graph` | `code-graph` | `code-embed` for embeddings |
| `heliosdb-nano-graph-rag` | `graph-rag` (implies `code-graph`) | `code-embed`, `mcp-endpoint` |
| `heliosdb-nano-mcp` | `mcp-endpoint` | `vector-search`, `graph-rag`, `code-embed` |
| `heliosdb-nano-server` | (none) | `fips`, `ha-tier1+`, TLS deps via `ring-crypto`/`fips` |
| `heliosdb-nano-deploy` | (none) | matches whatever the deployed build needs |
| `heliosdb-nano-observability` | (none) | — |
| `heliosdb-nano-migrate` | (none) | `compression` for MySQL drop-in |

## How to verify a build's feature set at runtime

```bash
heliosdb-nano --version                    # version banner

# Subcommand presence test (each line on / off):
heliosdb-nano code-graph --help 2>/dev/null && echo "code-graph: ON" || echo "code-graph: OFF"
heliosdb-nano mcp        --help 2>/dev/null && echo "mcp:        ON" || echo "mcp:        OFF"

# Cargo-side metadata:
cargo info heliosdb-nano --features
```
