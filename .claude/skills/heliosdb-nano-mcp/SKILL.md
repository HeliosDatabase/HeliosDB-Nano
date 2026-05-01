---
name: heliosdb-nano-mcp
description: Run HeliosDB-Nano as an MCP (Model Context Protocol) server so AI agents (Claude Code, Codex CLI, MCP-aware tools) can query, write, branch, time-travel, and search via JSON-RPC 2.0. Covers stdio, HTTP, and WebSocket transports; the 16-tool catalog (10 DB-backed + 6 in-process RAG); and wiring into Claude Code via `claude mcp add`. Use this when the user wants an LLM agent to operate the database directly through tool calls instead of writing SQL.
allowed-tools: Bash(heliosdb-nano *), Bash(claude *), Read
---

# MCP Server (Model Context Protocol)

## When to use
- Expose Nano to an AI agent (Claude Code, Codex CLI) as a callable toolset.
- Replace ad-hoc shell-out-and-parse glue with structured JSON-RPC.
- Add Nano-backed retrieval to an MCP-orchestrated workflow.

## Prerequisites
- Cargo feature: **`mcp-endpoint`** required.
  ```bash
  cargo install heliosdb-nano --features mcp-endpoint
  ```
- For embedding tools: add `code-embed`. For graph traversal: add `graph-rag` (implies `code-graph`).

Verify:
```bash
heliosdb-nano mcp --help \
  && echo "mcp-endpoint: ENABLED" \
  || echo "rebuild with --features mcp-endpoint"
```
*(If `mcp` isn't a recognised subcommand on your build, the feature is off — see `Cargo.toml:189-299`.)*

## Tool catalog (16 tools)

### DB-backed (require an `EmbeddedDatabase` mounted in the server)
| Tool | Purpose |
|------|---------|
| `heliosdb_query` | Run arbitrary SQL; return rows as JSON |
| `heliosdb_schema` | Introspect table schema |
| `heliosdb_list_tables` | List user tables |
| `heliosdb_create_table` | DDL via JSON shape |
| `heliosdb_insert` | INSERT rows from JSON |
| `heliosdb_branch_create` | `CREATE DATABASE BRANCH` |
| `heliosdb_branch_list` | List branches |
| `heliosdb_branch_merge` | `MERGE BRANCH` |
| `heliosdb_search` | Full-text search |
| `heliosdb_time_travel` | `SELECT … AS OF …` shorthand |

### In-process (process-static state — RAG/index utilities)
| Tool | Purpose | Extra feature |
|------|---------|---------------|
| `heliosdb_bm25_index` | Build BM25 index over a text column | — |
| `heliosdb_hybrid_search` | BM25 + vector reranked search | `vector-search` |
| `heliosdb_graph_add_edge` | Add a typed edge between graph nodes | `graph-rag` |
| `heliosdb_graph_traverse` | Walk the graph from a seed node | `graph-rag` |
| `heliosdb_graph_path` | Shortest-path between two nodes | `graph-rag` |
| `heliosdb_embed_and_store` | Embed text in-process, store in vector store | `code-embed` |

## Recipes

### Recipe 1: Stand up an MCP server (stdio transport)
```bash
heliosdb-nano start \
    --data-dir ./mydata \
    --mcp-stdio                    # exposes the JSON-RPC server on stdin/stdout
```
*(Exact flag varies by `mcp-endpoint` build — check `heliosdb-nano start --help` after enabling the feature; in some builds the MCP server is a separate `heliosdb-nano mcp serve` subcommand.)*

### Recipe 2: Wire into Claude Code
```bash
# from the directory where you want Claude Code to find the server:
claude mcp add heliosdb -- heliosdb-nano mcp serve --data-dir ./mydata

# verify
claude mcp list
# Expect: heliosdb (running)
```
Inside any Claude Code session in this project, the 16 tools become available to the model directly.

### Recipe 3: HTTP transport (for non-stdio agents)
```bash
heliosdb-nano mcp serve \
    --data-dir ./mydata \
    --transport http \
    --listen 127.0.0.1:9000
```
Client (any HTTP-capable agent):
```bash
curl -s http://127.0.0.1:9000 -d '{
  "jsonrpc":"2.0","id":1,"method":"tools/list"
}' | jq '.result.tools[].name'
```

### Recipe 4: WebSocket transport
```bash
heliosdb-nano mcp serve \
    --data-dir ./mydata \
    --transport ws \
    --listen 127.0.0.1:9001
```
Useful for browser-side agents.

### Recipe 5: Tool call — run an arbitrary SQL query
```jsonrpc
{
  "jsonrpc":"2.0","method":"tools/call","id":42,
  "params":{
    "name":"heliosdb_query",
    "arguments":{
      "sql":"SELECT id, email FROM users WHERE created > $1 LIMIT 5",
      "params":["2026-04-01"]
    }
  }
}
```
Result: JSON shape with `columns: [...]` + `rows: [[...]]` (or `error: {…}`).

### Recipe 6: Tool call — hybrid search
```jsonrpc
{
  "jsonrpc":"2.0","method":"tools/call","id":7,
  "params":{
    "name":"heliosdb_hybrid_search",
    "arguments":{
      "table":"docs","text_col":"body","vec_col":"embedding",
      "query":"how do branches handle conflicts",
      "alpha":0.5,"top_k":10
    }
  }
}
```

### Recipe 7: Tool call — branch ops
```jsonrpc
{ "name":"heliosdb_branch_create",
  "arguments":{"name":"agent_run_42","parent":"main"} }

// … agent does work …

{ "name":"heliosdb_branch_merge",
  "arguments":{"source":"agent_run_42","target":"main"} }
```

### Recipe 8: Resources (cached query results, session state)
The MCP server also exposes Resources via `resources/list` and `resources/read`. Common patterns:
- Last query result cached as `helios://cache/last`.
- Session state under `helios://session/<id>`.
- Code-graph symbol shards under `helios://code-graph/symbols/<shard>` (when `code-graph` is built in).

## Pitfalls
- **Feature gating**: every tool above requires the right cargo feature(s). `tools/list` returns only what's compiled in.
- **Subcommand naming differs across builds**. Some builds expose MCP through `heliosdb-nano start --mcp-stdio`, others through a distinct `heliosdb-nano mcp serve …`. Run `--help` to confirm.
- **Stdio transport conflicts with stdout logging.** When using stdio MCP, redirect tracing to stderr (`RUST_LOG=warn` and never log to stdout).
- **`heliosdb_query` returns errors as JSON, not exceptions.** Agents must check the `error` field.
- **Branches created via MCP persist** — agents that don't clean up `agent_run_*` branches leak state. Wrap agent runs with `branch_create … branch_merge` (or drop the branch in a `finally` block).
- **`heliosdb_embed_and_store` downloads a model on first call** (`code-embed`). Provision the model into `./.fastembed_cache/` ahead of time in CI.

## See also
- `heliosdb-nano-code-graph` / `heliosdb-nano-graph-rag` — the engines behind the graph tools.
- `heliosdb-nano-vector` — vector tools and HNSW operators.
- `heliosdb-nano-branches` — branch-tool semantics from the SQL side.
- `src/mcp/tools.rs:82-93` — authoritative tool catalog list.
