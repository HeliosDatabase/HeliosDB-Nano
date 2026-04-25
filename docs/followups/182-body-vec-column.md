# Task 182 — `body_vec VECTOR(n)` on `_hdb_code_symbols`

## Goal

Land the column the FR planned but never shipped, and wire the
indexer to populate it when an embedder is configured. Without it,
the vector-similar linker (#7 from feedback) and the in-descent
HNSW prefilter (#4) can't operate against the indexer's own
symbols.

## Acceptance

* `_hdb_code_symbols` carries a `body_vec VECTOR(n)` column, where
  `n` is negotiated with the embedder on first index pass. `n =
  NULL` (column omitted) when no embedder is configured.
* `db.code_index(opts.with_embedder(HttpEmbedder::new(url)))`
  populates `body_vec` for every symbol.
* `code_index_is_idempotent` regression test still green.

## Design

* `code_graph::storage::ensure_tables` adds the column lazily —
  on the first call where the indexer has an embedder, we ALTER
  the table to add the column. Subsequent calls no-op (column
  exists).
* Negotiation: the first batch sent to the embedder establishes the
  dimension. Subsequent batches must match, else error.
* `body_vec` is set via the existing batched INSERT path; new SQL
  template parameterises the column list.
* `Embedder` trait already exists; the indexer just calls
  `embedder.embed(body)` per symbol when configured.

## Files to touch

* `src/code_graph/storage.rs` — schema ensure, negotiated dimension,
  insert template extension.
* `src/code_graph/embed.rs` — already has `Embedder` trait + `Http`
  / `Noop` impls; no API change.
* `tests/code_graph_body_vec.rs` — 4 cases: no embedder → column
  absent, embedder with d=384 → column populated, mismatched
  dimensions error, second pass overwrites first.

## Tests

1. `body_vec_absent_when_no_embedder` — ensure_tables produces no
   `body_vec` column.
2. `body_vec_populated_when_embedder_configured` — uses a mock
   embedder that returns `[1.0; 8]`, confirms the column shows up
   and rows have non-null vectors.
3. `dimension_mismatch_errors` — first pass d=8, second pass d=4,
   indexer fails cleanly.
4. `re_embedding_overwrites_in_place` — same row, two passes,
   final row carries the second pass's vector.

## Out of scope

- Real embedder integration → that's #187 (code-embed flag).
- HNSW index over the new column → falls out of #190.
