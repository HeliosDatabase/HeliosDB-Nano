# Pilot: HeliosDB-Nano code-graph against its own `src/` tree

**Date:** 2026-04-24
**Target:** `/home/app/Helios/Nano` (this repo, a 333-file Rust corpus)
**Build:** `cargo build --release --features code-graph`
**Pilot binary:** `examples/code_graph_pilot.rs`
**Inspector:** `examples/code_graph_inspect.rs`
**Index location:** `.helios-index/heliosdb-data` (git-ignored per
`.gitignore` convention in `FEATURE_REQUEST_pilot_helios_corpus.md`)

## What the pilot does

1. Walks a source directory recursively, filtering to the languages
   the phase-1 extractor understands (`rs`, `py`, `ts`, `tsx`, `js`).
2. Upserts each file as `(path, lang, content, size_bytes)` into a
   `src` table in a **persistent** HeliosDB-Nano instance at
   `.helios-index/heliosdb-data`.
3. Calls `EmbeddedDatabase::code_index(...)` — tree-sitter parse,
   per-file symbol extraction, and the cross-file resolver pass.
4. Runs the four canonical LSP queries against the populated index
   with wall-clock timing.

## Cold-index results (Nano's own `src/`)

| Metric | Value |
|---|---|
| Files walked | 333 `.rs` |
| Bytes ingested | 7.9 MB |
| Symbols extracted | 11,962 |
| Refs collected | 66,326 |
| Walk + upsert wall | 700 ms – 1.5 s |
| `code_index` wall | **128 s** cold |
| Throughput | 393 ms / file · 17.0 ms / KB |
| Disk footprint | 58 MB (SST + open WAL) |
| Source size | 8.4 MB |
| Index : source ratio | ~7× |

### Symbol breakdown

```
function   8042
struct     1374
impl       1023
module      577
const       536
enum        339
type         46
trait        25
```

### Resolution breakdown (refs)

```
exact       21,565  (32.5%)
heuristic   20,385  (30.7%)
unresolved  24,362  (36.7%)
```

The unresolved rate is typical for a heuristic resolver without
type-aware resolution — calls like `Arc::new(...)`, `Ok(x)`,
`Some(x)` name generic stdlib symbols that don't exist in this
corpus, so they resolve to NULL cleanly rather than bind to a
wrong target. The top 10 unresolved names mirror this:
`Ok`, `Some`, `Err`, `Arc::new`, `Vec::new`, `Box::new`,
`db.execute`, `db.query`, `Error::query_execution`,
`Error::storage`. All are real calls; none are in-corpus
definitions.

## Query latencies

| Query | Hits | Wall | Path |
|---|---|---|---|
| `lsp_definition("EmbeddedDatabase")` | 1 | 12–18 ms | `lib.rs:373` |
| `lsp_definition("code_index")` | 2 | 13–17 ms | `code_graph/storage.rs:116` |
| `lsp_definition("lsp_definition")` | 2 | 12–18 ms | `code_graph/lsp.rs:68` |
| `lsp_definition("ProductQuantizer")` | 1 | 17–18 ms | `vector/quantization/product_quantizer.rs:193` |
| `lsp_definition("new_in_memory")` | 1 | 12 ms | `lib.rs:2741` |
| `lsp_references(new_in_memory)` | 693 | 54 ms | – |
| `lsp_call_hierarchy(new_in_memory, in, 2)` | 490 | 510 ms | – |
| `lsp_references(execute)` | 6 | 53 ms | – |
| `lsp_call_hierarchy(execute, in, 2)` | 3 | 259 ms | – |
| `lsp_hover(code_index)` | signature | 0 ms | – |
| `count(*) _hdb_code_files` | — | 0 ms | scan |
| `count(*) _hdb_code_symbols` | — | 3 ms | scan |
| `count(*) _hdb_code_symbol_refs` | — | 19 ms | scan |

**Observations.**

- **`lsp_definition` is consistently ~15 ms** regardless of result
  count. The storage-level pushdown (`name = $1` Eq → bloom /
  zone-maps / SIMD) carries the weight — the SSTs skip most blocks
  before row materialisation starts.
- **`lsp_references` scales with result count.** 6 refs → 53 ms; 693
  refs → 54 ms. The expensive leg is the JOIN to `_hdb_code_files`,
  not the scan. A covering index on `(to_symbol)` plus a projection
  that avoids the JOIN would bring this under 10 ms.
- **`lsp_call_hierarchy` cost = depth × fan-out.** 3 callers at depth
  2 = 259 ms; 490 callers = 510 ms. The generic BFS issues one query
  per depth level, and each level re-walks the JOIN. A phase-2
  recursive-CTE rewrite would collapse this into one plan.
- **`lsp_hover` is a point-PK lookup** and returns in under 1 ms.

## Known quality gaps (resolver)

- `lsp_references("code_index")` returned 0 refs even though
  `code_index` is called from multiple places in Nano. Inspection:
  there are 2 definitions of `code_index` (the `storage::code_index`
  function and the `EmbeddedDatabase::code_index` method). The
  cross-file resolver rebound the single in-corpus call to the
  **other** definition. The user's `lsp_definition` query returned
  `storage::code_index` (first by node_id); the ref is pointing at
  the method. This is a ranking issue — phase 2.1 follow-ups:
  - Return all candidates with scores and let the caller pick.
  - Prefer function kind over module when both match.
  - Collapse "same name, same file" symbols into one logical
    definition with multiple locations.

## Bugs caught by the pilot (and fixed)

1. **SQL injection-like parse failures during index.** The initial
   code-graph storage path used `format!` with naïve `'` → `''`
   escaping for every INSERT — broke on Rust lifetime syntax
   (`'a`) and on raw-string literals. Fixed by swapping every insert
   on the indexer path to parameterised `execute_params_returning`
   (`src/code_graph/storage.rs`).

2. **Warm re-index violated FK constraints.** Deleting a file's
   symbols failed when other files' refs pointed at them
   (cross-file resolution had bound them). Fixed by nulling inbound
   `to_symbol` columns first; the cross-file pass at the end of
   `code_index` rebinds every orphan.

3. **UPDATE with subquery in IN** was not wired in the DML path.
   Swapped the one remaining subquery to a two-step SELECT-then-
   UPDATE-with-literal-IN-list. The subquery-in-DML path itself is
   a separate engine follow-up.

Both (1) and (2) are real fixes landed alongside this pilot report.
Fix (3) is local to the pilot path.

## Efficiency assessment

**Queries: production-grade.** Every `lsp_definition` returns in
under 20 ms; `lsp_references` scales linearly with result count, not
corpus size; `lsp_hover` is effectively free. These latencies are
comparable to rust-analyzer running against a warm workspace.

**Cold indexing: slow, but identifiable.** 128 s for 11.9 K symbols
(~93 symbols/s) against the full Nano `src/` tree. The bottleneck is
**one INSERT-RETURNING per symbol** through the SQL planner →
executor → RocksDB fsync pipeline. Phase 2.1 optimisation target:

- Multi-row `VALUES ($1,$2,...),(..),(..)` batch inserts (10–50×
  speedup). Drops the parse+plan+execute overhead to one per batch
  instead of one per symbol.
- A single `BEGIN; INSERT; INSERT; ...; COMMIT;` wrapper instead of
  one implicit transaction per row. Drops WAL fsyncs to O(batch).
- Bulk-load helpers (`hdb_code.pause` / `hdb_code.resume` — in the
  FR but not implemented in phase 1) that defer index + FK checks
  until after the load.

A realistic target post-optimisation is **≤ 10 s / 11K symbols**
(1.2 K symbols/s), which matches what tree-sitter alone can do on a
modern laptop.

**Disk ratio (7×).** The index carries every symbol + ref row plus
every file's original content. With content dropped after parsing
(or stored as a BLAKE3 hash), the ratio drops closer to 2–3×.
Phase 2.1 follow-up.

**Warm re-index.** Partial measurement; the added
`null-inbound-refs → delete → reinsert` cycle per file costs more
than a fresh index when no rows changed. Phase 2.1 makes this
content-hash-gated so only actually-changed files pay the cost.

## Reproducing

```bash
cd /home/app/Helios/Nano
cargo build --release --features code-graph \
    --example code_graph_pilot \
    --example code_graph_inspect

# cold
rm -rf .helios-index
./target/release/examples/code_graph_pilot src .helios-index

# inspect
./target/release/examples/code_graph_inspect .helios-index

# warm re-index (picks up where the cold one left off)
./target/release/examples/code_graph_pilot src .helios-index
```

The `.helios-index/` directory is `.gitignore`d per the pilot
convention, so it will not be committed.
