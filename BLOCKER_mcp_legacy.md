# Idea 5 (MCP enhancements) -- partial blocker

## What's delivered

- New module `src/mcp_extensions/` with all six idea-5 tool handlers:
  - `heliosdb_bm25_index`
  - `heliosdb_hybrid_search`
  - `heliosdb_graph_add_edge`
  - `heliosdb_graph_traverse`
  - `heliosdb_graph_path`
  - `heliosdb_embed_and_store`
- Two new resource resolvers: `heliosdb://schema/{table}`,
  `heliosdb://stats/{table}` (in `mcp_extensions::resources`).
- Tests: 6 unit tests + integration tests in `tests/mcp_new_tools.rs`.

## What's blocked

The plan asks for the new tools/resources to be wired directly into
`src/mcp/server.rs` and `src/mcp/tools.rs`. That module is currently
**not enabled in `src/lib.rs`** because its existing handlers reference
`EmbeddedDatabase` methods that have since been removed/renamed:

- `db.query(branch, sql, params)`            -- signature changed to `(sql, params)`
- `db.query_branch(branch, sql, params)`     -- removed
- `db.execute_branch(branch, sql, params)`   -- removed
- `db.merge_branches(src, dst)`              -- removed
- `db.query_at_timestamp(...)`               -- removed
- `result.rows`, `result.columns`            -- result type is now `Vec<Tuple>`

Re-enabling `mcp` requires reconciling ~15 call sites against the
current `EmbeddedDatabase` API. That refactor is out of scope for
this drop -- it isn't a external project integration concern, it's pre-existing
API drift in the legacy MCP wrapper.

## Migration path

When the legacy `mcp` module is repaired:

1. Re-enable `pub mod mcp;` in `src/lib.rs`.
2. Move handlers from `src/mcp_extensions/tools.rs` into
   `src/mcp/tools.rs` (the standalone module is already shaped to
   drop in -- same signatures, same JSON-schema descriptors).
3. Register the resources in `src/mcp/server.rs`'s
   `handle_resources_list` / `handle_resources_read`.
4. Delete `src/mcp_extensions/` and the BLOCKER file.

The standalone module shares storage (`BM25_INDEXES`, `GRAPH_STORE`)
via `once_cell::sync::Lazy` -- a future merge can keep those as the
backing state for the unified `mcp` module without losing process-wide
warmth.
