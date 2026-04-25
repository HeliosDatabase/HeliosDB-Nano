# Follow-up plan — closing the FR backlog

Twelve tasks land the remaining items from the original code-graph /
graph-rag / MCP track plus the post-merge expansions the user
requested: docling-based content conversion, local inference,
schema namespacing, type-aware resolution, lsp_rename_apply,
acceptance benchmarks, FR 6 pilot.

| # | Task | Effort | Depends on | Doc |
|--|--|--|--|--|
| 181 | `pg_catalog.list_languages` system view | XS | — | [`181-list-languages-view.md`](181-list-languages-view.md) |
| 182 | `body_vec` VECTOR column on `_hdb_code_symbols` | S | — | [`182-body-vec-column.md`](182-body-vec-column.md) |
| 183 | Symbol-extractor pluggability | S | — | [`183-extractor-registry.md`](183-extractor-registry.md) |
| 184 | HTTP POST + SSE progress pairing | M | — | [`184-http-sse-progress.md`](184-http-sse-progress.md) |
| 185 | `helios_lsp_rename_apply` write-back | S | — | [`185-lsp-rename-apply.md`](185-lsp-rename-apply.md) |
| 186 | Docling content-conversion ingestion | M | — | [`186-docling-ingest.md`](186-docling-ingest.md) |
| 187 | `code-embed` feature flag — local inference | M | 182 | [`187-code-embed.md`](187-code-embed.md) |
| 188 | `_hdb_code.schema` dotted namespacing | L | — | [`188-schema-namespacing.md`](188-schema-namespacing.md) |
| 189 | Type-aware resolution scaffold | L | — | [`189-type-aware-resolution.md`](189-type-aware-resolution.md) |
| 190 | HNSW navigation-bias centrality + in-descent prefilter | L | 182 | [`190-hnsw-bias.md`](190-hnsw-bias.md) |
| 191 | Acceptance benchmarks + binary size guard | M | most | [`191-acceptance-benchmarks.md`](191-acceptance-benchmarks.md) |
| 192 | FR 6 pilot deployment | M | parallel | [`192-fr6-pilot.md`](192-fr6-pilot.md) |

## Execution order

Dependency layers — each layer can run in parallel internally,
serial across layers:

* **Layer 1 (foundational, parallel)**: 181, 182, 183, 184, 185, 188
* **Layer 2 (depends on 182)**: 187, 190
* **Layer 3 (depends on 187)**: 186 (when local inference available)
* **Layer 4 (independent, run in parallel)**: 189, 192
* **Layer 5 (last)**: 191

In practice the implementation will batch related tasks per commit —
e.g. 181 + 183 together, 188 in its own commit, etc.

## Cross-cutting design principles

1. **Default-feature build stays slim.** Heavy deps (fastembed-rs,
   tokenizers, ORT) are gated under `code-embed` and pulled in only
   when the user opts in.
2. **Pluggable interfaces.** Embedders (HTTP / fastembed / noop),
   content converters (docling-serve / mock), and symbol extractors
   all use trait objects so callers swap implementations without
   forking the indexer.
3. **Backwards compatibility.** Schema namespacing keeps flat-prefix
   names working as aliases for the dotted form. Existing code
   using `_hdb_code_symbols` continues to compile.
4. **Acceptance criteria drive #191.** The benchmark suite is the
   one place the FR success criteria are measured end-to-end —
   under-500ms WITH CONTEXT, ≥80% linker precision, +5% binary
   size, MCP conformance.

## Out of scope (still)

- GPU inference. fastembed-rs runs on CPU; ONNX Runtime supports
  CUDA but we don't pull the GPU EP by default.
- Beyond-Rust language type inference (Pyright-grade, etc.). The
  type-aware resolver in #189 is scope-chain + import tracking
  only.
- Docling internals reimplementation. We integrate via HTTP against
  `docling-serve`; bringing the layout / OCR / VLM pipeline into
  Rust is a separate multi-quarter effort.
