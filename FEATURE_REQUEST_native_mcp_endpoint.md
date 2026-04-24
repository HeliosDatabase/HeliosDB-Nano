---
requested-by: Claude Code code-graph initiative — danimoya
requested-against: HeliosDB-Nano v3.13.x
priority: medium
status: proposed
date-filed: 2026-04-23
track: code-graph
doc: 5/5
depends-on: FEATURE_REQUEST_ast_index_and_lsp.md, BLOCKER_mcp_legacy.md
---

# Feature Request: Native MCP endpoint on the Axum server (`/mcp`, `/mcp/ws`)

## TL;DR

Expose HeliosDB-Nano's code-graph, GraphRAG, LSP, and hybrid-search
surface as a **first-class Model Context Protocol endpoint** on the
existing Axum HTTP server (`src/api/server.rs`, port 8080). AI
coding agents (Claude Code, Cursor, Continue, Codex, Aider) connect
to HeliosDB-Nano directly over MCP — no Python / TypeScript
middleware, no extra process, no extra port. This is the feature
that makes HeliosDB-Nano the only embedded database that *is* an
MCP server.

## Motivation

Every AI agent today talks to data stores via the same pattern: a
bespoke server that translates MCP tool calls to backend-specific
APIs. That server has to be built, hosted, authenticated, and kept
in sync. If HeliosDB-Nano ships the MCP endpoint itself, the
deployment story for any user becomes:

```bash
heliosdb-nano start --mcp
# .mcp.json on the client:
# { "mcpServers": { "helios": { "url": "http://localhost:8080/mcp" } } }
```

…and the agent has code-graph, LSP, GraphRAG, and hybrid search.
Done. No Python, no Docker Compose, no glue.

This is also the feature that turns the track from "nice extension"
into a **category move**: no other embedded / SQL database on the
market speaks MCP natively. Ship this and HeliosDB-Nano defines that
category.

## Current state in HeliosDB-Nano

- Axum HTTP server already running on 8080: `src/api/server.rs`,
  routes under `src/api/routes/`, handlers under
  `src/api/handlers/`.
- REST executor: `src/api/rest_executor.rs`.
- Auth already in place: `src/api/jwt.rs`, `src/api/auth_bridge.rs`,
  `src/api/oauth.rs`.
- OpenAPI generation: `src/api/openapi/`.
- MCP tool *handlers* exist in two places:
  - `src/mcp/{mod.rs, protocol.rs, server.rs, tools.rs}` — legacy,
    currently **disabled** in `src/lib.rs`, per
    `BLOCKER_mcp_legacy.md`.
  - `src/mcp_extensions/{mod.rs, resources.rs, tools.rs}` — active,
    holds the six "idea 5" tools (bm25_index, hybrid_search,
    graph_add_edge, graph_traverse, graph_path, embed_and_store)
    plus two resource resolvers (`heliosdb://schema/{table}`,
    `heliosdb://stats/{table}`).

The MCP handlers exist; **they are not currently wired to a
transport**. This FR both fixes that and adds a transport that
agents can actually reach.

## Proposed design

### 5.1 Transport: HTTP + WebSocket on the existing Axum server

Two new route groups on the existing server:

- `POST /mcp` — JSON-RPC 2.0 over HTTP, one request per body, one
  response per body. Suitable for every MCP client that supports
  HTTP transport.
- `GET  /mcp/ws` — the same JSON-RPC framed over a WebSocket, for
  clients that want streaming / server-push tool progress.
- `GET  /mcp/sse` — Server-Sent Events variant, for clients that
  prefer SSE (some MCP implementations do).

All three dispatch to the same handler core; only framing differs.

Request shape conforms to the MCP spec (`initialize`,
`tools/list`, `tools/call`, `resources/list`, `resources/read`,
`prompts/list`, `prompts/get`, `ping`, `notifications/initialized`,
etc.). Version: current MCP spec as of filing; bumped as the spec
evolves.

### 5.2 Wire: unify the two MCP modules, then mount

Per `BLOCKER_mcp_legacy.md`, `src/mcp/` has been off since
`EmbeddedDatabase` API drift. Repair path the blocker already lays
out (summarised):

1. Re-enable `pub mod mcp;` in `src/lib.rs`.
2. Fix the ~15 call sites to the new `db.query(sql, params)` shape,
   the new `Vec<Tuple>` result, and removed branch/timestamp
   variants (branch/timestamp now live as session settings and `AS
   OF`).
3. Move the six handlers + two resources from `src/mcp_extensions/`
   into `src/mcp/tools.rs` / `src/mcp/server.rs`, preserving the
   shared `BM25_INDEXES` / `GRAPH_STORE` `once_cell::sync::Lazy`
   state.
4. Delete `src/mcp_extensions/` and its BLOCKER file.

Then mount the MCP server behind the Axum routes in 5.1. The MCP
server itself is now a library call from the Axum handler.

### 5.3 Tool catalogue

The MCP tool surface is the union of:

**Already implemented (post-repair):**
- `heliosdb_bm25_index` — create/refresh BM25 index on a column.
- `heliosdb_hybrid_search` — vector + FTS + optional rerank.
- `heliosdb_graph_add_edge` — write a typed edge.
- `heliosdb_graph_traverse` — BFS/DFS from a seed, filtered by edge kinds.
- `heliosdb_graph_path` — shortest path between two nodes.
- `heliosdb_embed_and_store` — embed text, store into a vector column.

**Added by this track:**
- `helios_lsp_definition(name, hint_file?, hint_kind?, at?)` — FR 2.
- `helios_lsp_references(symbol_id, include_tests?, at?)` — FR 2.
- `helios_lsp_call_hierarchy(symbol_id, direction?, depth?, at?)` — FR 2.
- `helios_lsp_hover(symbol_id, at?)` — FR 2.
- `helios_lsp_document_symbols(file_id, at?)` — FR 2.
- `helios_lsp_rename_preview(symbol_id, new_name, at?)` — FR 2.
- `helios_lsp_diff(symbol_id, at_a, at_b, kind)` — FR 3.
- `helios_ast_diff(file_path, at_a, at_b)` — FR 3.
- `helios_graphrag_search(query, node_kinds?, hops?, edges?, rerank_limit?)` — FR 4 (wraps `WITH CONTEXT`).

**Resources (MCP resources):**
- `heliosdb://schema/{table}` — table DDL.
- `heliosdb://stats/{table}` — row/page stats.
- `heliosdb://symbol/{node_id}` — rendered symbol (signature + hover).
- `heliosdb://graph/node/{node_id}` — node + 1-hop neighbourhood.
- `heliosdb://file/{path}@{ref?}` — file content at ref (git SHA,
  timestamp, or HEAD).

Tool descriptors (JSON schema) are autogenerated from the
`lsp_*` / `ast_*` stored-function catalog via a small macro in
`src/mcp/tools.rs`. This keeps SQL-level and MCP-level signatures
in lockstep — every time someone adds an `lsp_*` function, its MCP
wrapper is a one-line registration.

### 5.4 Authentication and authorisation

- **Default**: `Authorization: Bearer <JWT>` via existing
  `src/api/jwt.rs`. A dedicated MCP scope (`mcp:read`, `mcp:write`)
  gates tool calls. Resources inherit the same scopes.
- **Local/embedded mode**: Unix-domain-socket transport can be
  enabled (`--mcp-socket /tmp/helios-mcp.sock`) with peer-cred
  auth, no JWT required — same pattern the README already
  documents for PG/MySQL sockets (`README.md:41–45`, `:280`).
- **Public exposure**: strictly opt-in — `--mcp-bind 0.0.0.0` is
  refused unless a non-default JWT secret is configured, mirroring
  the rest of the server's hardening.

### 5.5 Streaming / progress

Long-running tool calls (e.g. a large `heliosdb_graphrag_search`
over millions of chunks) emit incremental progress events over
`/mcp/ws` or `/mcp/sse`:

```json
{ "jsonrpc":"2.0", "method":"notifications/progress",
  "params":{ "progressToken":"<id>", "progress":0.42,
             "stage":"expand", "nodes_visited": 1813 } }
```

This matters for interactive agent UX — Claude Code renders
progress in-session when the tool emits it.

### 5.6 Discovery and introspection

- `tools/list` returns the full catalogue plus, for each tool, a
  link to `/docs` with a cross-reference to the underlying SQL
  function signature.
- `tools/list?verbose=true` includes example invocations generated
  from `_hdb_code` self-query results.
- `GET /mcp/info` (non-MCP) returns capability + version +
  extension set (`hdb_code`, `hdb_corpus`, …) for ops and
  monitoring.

## Worked example

```bash
# Server
heliosdb-nano start --mcp --jwt-secret "..." &

# Client (any MCP-capable agent)
cat > .mcp.json <<'JSON'
{
  "mcpServers": {
    "helios": {
      "url": "http://localhost:8080/mcp",
      "headers": { "Authorization": "Bearer eyJ..." }
    }
  }
}
JSON

# Agent calls, after initialize:
# tools/call helios_lsp_definition { "name": "ProductQuantizer" }
# tools/call helios_graphrag_search { "query": "revenue attribution",
#                                      "hops": 2,
#                                      "edges": ["MENTIONS","CALLS","CITES"] }
```

## Acceptance criteria

- [ ] Legacy `src/mcp/` module re-enabled per
      `BLOCKER_mcp_legacy.md`; `src/mcp_extensions/` folded in and
      deleted.
- [ ] `POST /mcp` accepts a JSON-RPC MCP `initialize` and responds
      with the expected handshake.
- [ ] `tools/list` returns every tool in §5.3 once FR 2 / FR 4
      have landed.
- [ ] `tools/call heliosdb_hybrid_search {...}` returns results
      identical to running the equivalent SQL.
- [ ] `tools/call helios_lsp_definition {...}` composes with
      `AS OF` via the optional `at` argument.
- [ ] `tools/call helios_graphrag_search {...}` emits progress
      notifications over `/mcp/ws` for operations > 250 ms.
- [ ] JWT auth enforced; anonymous calls to `/mcp` return 401.
- [ ] `--mcp-socket` Unix-domain mode works without JWT.
- [ ] `--mcp-bind 0.0.0.0` refuses to start with the default
      JWT secret.
- [ ] Adding a new `lsp_*` SQL function automatically surfaces it
      as an MCP tool via the macro in `src/mcp/tools.rs` (tested
      via a fixture function).
- [ ] MCP spec compliance verified with an upstream conformance
      tester.

## Non-goals

- A full bundled Claude / Cursor extension. That lives in the SDKs
  repo.
- Server-side prompt templates (MCP `prompts/*`). Can be added in a
  follow-up; not needed for the agent workflows in the track.

## Open questions

1. Should the MCP endpoint share the `jwt-secret` with the REST
   server, or have its own? Recommendation: share by default,
   overrideable.
2. Rate-limiting — per-token, per-tool, per-cost? Recommendation:
   per-token + per-tool counters, reuse existing middleware at
   `src/api/middleware/`.
3. SSE vs. WebSocket default — ship both, document WS as the
   recommended transport for progress-heavy tools.
4. Binary-streaming tool results (embeddings, large blobs) over
   MCP — current spec is JSON-RPC. For HeliosDB-scale results,
   offer `resources/read` URIs in place of embedding bytes
   in-band.

## Related

- Depends on `BLOCKER_mcp_legacy.md` being resolved (items 1–3).
- Depends on FR 2 / FR 3 / FR 4 for the code-graph tool surface;
  ships with whatever subset has landed.
- Once this ships, the pilot `~/Helios` index is usable from any
  MCP-capable agent with zero Python glue.
