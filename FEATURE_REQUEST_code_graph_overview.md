---
requested-by: Claude Code code-graph initiative — danimoya
requested-against: HeliosDB-Nano v3.13.x
priority: high
status: proposed
date-filed: 2026-04-23
track: code-graph
---

# Feature Track: HeliosDB-Nano as a native code-graph / AI-agent retrieval engine

## TL;DR

Turn HeliosDB-Nano into the single-tool answer for AI coding assistants
(Claude Code, Cursor, Continue, Aider, Codex) and AI research agents
working over large, multi-repo corpora — by promoting its existing
vector / FTS / graph / git-integration primitives into a first-class
**code-graph layer** with AST-awareness, LSP-shaped operations, and a
**native MCP endpoint**, so the database *is* the retrieval backend
rather than the storage behind one.

This track is a set of five feature requests (this document + four
siblings) that together deliver that capability. They are designed so
they can be shipped independently and in parallel, and so that each one
ships user-visible value on its own.

## Motivation

AI coding agents today combine three pieces that are almost always
separate: (1) a language server or AST indexer (tree-sitter / LSP /
Serena / Sourcegraph SCIP), (2) a vector database for semantic search
over code and docs (pgvector / LanceDB / Qdrant / Chroma), and (3) a
custom MCP/HTTP wrapper that exposes both to the agent. The glue layer
is Python or TypeScript, maintained per-project, and is a common source
of slowness, staleness, and cost.

HeliosDB-Nano already has every primitive required to collapse that
three-layer stack into a single embedded binary:

| Need | Already present in HeliosDB-Nano |
|---|---|
| Vector search | `src/vector/hnsw_index.rs`, `src/vector/quantized_hnsw.rs`, `src/vector/quantization/` (HNSW + Product Quantization) |
| Keyword / FTS / BM25 | `src/search/{bm25.rs, hybrid.rs, reranker.rs}`, `src/storage/gin_index.rs`, `tsvector`/`ts_rank_cd` |
| Hybrid search | `src/search/hybrid.rs` (blended vector + FTS) |
| Graph engine | `src/graph/{storage.rs, traverse.rs, sql.rs}` |
| Git awareness | `src/git_integration/{commit_tracker.rs, hooks/, diff/, ddl_versioning/}` |
| Time-travel | `src/storage/engine_timetravel_extension.rs` |
| Branching | `src/storage/branch.rs` |
| Triggers / CDC | `src/sql/triggers.rs` |
| HTTP server | `src/api/server.rs` (Axum, port 8080) |
| MCP tool handlers | `src/mcp_extensions/{tools.rs, resources.rs}` (hybrid search, graph edges, graph traversal, embed-and-store) |

What is **missing** to make HeliosDB-Nano a drop-in answer for AI code
agents is:

1. A language-aware **AST / symbol layer** that parses source code and
   materialises it as rows the rest of the engine can index and join
   on. (FR: `ast_index_and_lsp`)
2. **LSP-shaped** stored functions (go-to-def, find-refs,
   call-hierarchy, hover, rename-preview) that AI agents can call as
   normal SQL, so no per-project glue is needed. (FR:
   `ast_index_and_lsp`)
3. **Temporal and branch-aware** variants of those LSP operations —
   something LSP itself cannot do. (FR: `temporal_branch_lsp`)
4. A **cross-modal graph schema** (code + docs + tickets + emails) and
   a `WITH CONTEXT` SQL clause that lets one query retrieve a
   semantically-relevant *subgraph* rather than a flat list of chunks.
   (FR: `graphrag_with_context`)
5. A **native MCP endpoint** on the existing Axum server so agents
   connect to HeliosDB-Nano directly, with no Python middleware. (FR:
   `native_mcp_endpoint`)

## Concrete pilot workload

The forcing function is a real one: ~200 investor diligence questions
against `~/Helios`, a multi-repo corpus containing all HeliosDB-Nano
editions (Nano / Lite / Full / Cloud), docs (public + internal),
website, SDKs, proxy, and the investor data-room. Each question
requires tracing through source, design docs, and prior
correspondence. Today that means Claude Code reading dozens of files
per question; with this track it becomes 1–3 SQL/MCP calls per
question.

## Reading order

| Order | Document | Scope | Priority | Depends on |
|---|---|---|---|---|
| 1 | `FEATURE_REQUEST_code_graph_overview.md` (this file) | Track charter | — | — |
| 2 | `FEATURE_REQUEST_ast_index_and_lsp.md` | `CREATE AST INDEX` DDL, `hdb_code` extension, `ast_nodes` / `symbols` schema, `lsp_*` stored functions, CDC trigger | high | — |
| 3 | `FEATURE_REQUEST_temporal_branch_lsp.md` | `AS OF COMMIT` / `AS OF TIMESTAMP` for LSP calls, branch-scoped AST views, diff helpers | medium | 2 |
| 4 | `FEATURE_REQUEST_graphrag_with_context.md` | Unified cross-modal node/edge schema, `WITH CONTEXT` clause, graph-weighted HNSW, semantic Merkle tree | high | 2 |
| 5 | `FEATURE_REQUEST_native_mcp_endpoint.md` | `/mcp` route on Axum, MCP tool catalogue, JWT auth, WS streaming | medium | 2, 4 (logical); `BLOCKER_mcp_legacy.md` (for module repair) |

## Release sequencing (suggested)

- **v3.14** — FR 2 (AST index + LSP SQL functions) as an experimental
  extension. Ship tree-sitter grammars for Rust, Python, TypeScript,
  Go, SQL, and Markdown. Gate behind `CREATE EXTENSION hdb_code`.
- **v3.15** — FR 3 (temporal + branch LSP) once FR 2 is stable. Small
  addition because it reuses existing time-travel / branch machinery.
- **v3.16** — FR 5 (native MCP endpoint). Includes repairing the
  legacy `src/mcp/` module per `BLOCKER_mcp_legacy.md` and promoting
  `src/mcp_extensions/` into it, plus the new `lsp_*` tools.
- **v3.17** — FR 4 (cross-modal GraphRAG + `WITH CONTEXT`). Biggest
  piece of engine work; benefits from everything above being live so
  the semantics can be validated on the pilot workload.

## Non-goals for the track

- Replacing a full editor-grade LSP server for *interactive* IDE use
  (rename-across-project with conflict UI, live diagnostics, etc.).
  The LSP surface we expose is query-shaped, not edit-shaped.
- Competing with Sourcegraph on global precomputed code graphs across
  the open-source universe. Scope is per-install, per-corpus.
- Shipping language-specific static analyses (dataflow, taint, borrow
  checking). Symbol / call / reference resolution only.

## Why all five together (and not just one)

Each FR is useful alone, but the **flagship moment** — where
HeliosDB-Nano becomes uniquely positioned vs. the
pgvector + Serena + wrapper stack — is when (2) and (4) land and an
agent can write:

```sql
SELECT q.author, fn.path, m.source_doc
FROM investor_questions q
JOIN edges  e1 ON e1.from_node = q.node_id AND e1.kind = 'ASKS_ABOUT'
JOIN chunks m   ON m.node_id = e1.to_node
JOIN edges  e2 ON e2.from_node = m.node_id AND e2.kind = 'MENTIONS'
JOIN symbols fn ON fn.node_id = e2.to_node
WHERE fn.embedding <-> $query_vec < 0.25
  AND m.embedding  <-> $query_vec < 0.30
WITH CONTEXT (HOPS 2, EDGES CALLS|IMPLEMENTS|CITES, RERANK BY $query_vec, LIMIT 30);
```

…as one call, with no Python, no extra services, and with time-travel
and branch-awareness included. No other embedded database on the
market can do that.

## Contact

Follow-ups against this file or the per-FR files. Pilot corpus lives
at `~/Helios/` on the danielmoya.cv host.
