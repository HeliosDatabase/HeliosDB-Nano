# Nano post-3.57 — opt-in HTAP / ingest feature spec

**Author:** Opus (Nano release owner) · **Date:** 2026-06-14
**Inputs synthesized:**
- Proxy: `/home/gpc/HDB/Proxy/docs/audit-2026-06/NANO-v3.57-PROXY-OPTIMIZATION-RECOMMENDATIONS.md` (+ `SCALABILITY-MATRIX.md`, live 2×2 proxy×Nano matrix)
- CodeKB-MCP: `/home/gpc/HDB/heliosdb-codekb-mcp/NANO_3.57.0_INGEST_PERF_RECOMMENDATIONS.md` (6-agent workflow vs published 3.57.0 crate source)

## Prime directive (non-negotiable)
Every item is **additive + opt-in**: a `SET helios.*` session param, a `StorageConfig`/profile field, a new wire sub-protocol only entered on client request, or a gated cargo feature. **Default = today's behavior.** None may change the single-row simple-`Query` OLTP path that `pg35_benchmark` measures. **Each item ships only after a quiet-machine pg35 A/B proves neutrality** (delegated to the release Codex). Where the win is on a path pg35 never exercises (embeddings, COPY, extended pipeline), neutrality is by construction.

---

## Ingest time-budget (CodeKB, measured on the one end-to-end run = 3.36.1, 3h19m)

| Nano-side cost | % of total ingest | note |
|---|---:|---|
| Embedding compute (serial FastEmbedder, one text/call) | **35–50%** | largest; inside `code_index` → item 1 |
| Symbol + ref bulk-row INSERT (1.93M rows) | 10–20% | refs are 1.58M → `--skip-code-refs` / item 1b |
| Cross-file-resolve post-loop UPDATE | 3–8% | already batched 1000-at-a-time in 3.57.0 |
| **HNSW / vector index build** | **~0%** | **no HNSW built at ingest** — `body_vec` is a plain `VECTOR(384)` column; parallel-HNSW / build-time m·ef are **query-track, not ingest** |
| Commit + fsync / WAL | ~0% | `durable_commit=false` default + async WAL already off the hot path |
| DDL / columnar encoding | <0.1% / low background | one-time / off critical path |
| *(plugin, not Nano ingest)* linker candidate point-lookups | 42.6% separately | 599,964 SELECTs, **0 committed = plugin bug** (§ correctness) |

**Implication:** per-row-vs-batched, commit/fsync, DDL, and HNSW are NOT the ingest cost — it's **embedding compute first, raw symbol/ref volume second.** That is exactly what items 1 / 1b / 4 target.

## Master table (deduped across both consumers, prioritized)

| Pr | Feature | Surface | New/Enhance/Exists | Serves | Est. win | pg35-neutral because |
|----|---------|---------|--------------------|--------|----------|----------------------|
| **1** | `Embedder::embed_batch` (collapse 344k serial 1-elem ORT calls → one batched call/chunk) | engine API (internal) | **NEW** (no `embed_batch` in src) | CodeKB (unblocks `--with-embeddings`, currently KILLED @5h45m) | **2–5× on `code_index`, −40–55% total ingest** (single highest-value item) | embeddings path not in pg35; strictly faster, no behavior change |
| **1b** | Make `bulk_load_mode` **actually suspend** inline vector-DML / secondary-index maintenance on the `bulk_insert_tuples` path (today it only spares the ~12k single-row file inserts; near-no-op for the 1.9M-row bulk) | `SET bulk_load_mode=on` (exists) | **ENHANCE** (`lib.rs:14465-14473` inline `vector_dml_gate`/`on_row_insert`) | CodeKB (also the 3.57.0 write-path regression suspect) | **~10–15%** | gated behind existing `bulk_load_mode` SET (default off); OLTP path never sets it |
| **2** | COPY wire sub-protocol (`CopyInResponse`/`CopyData`/`CopyDone`/`CopyOutResponse`) | PG wire | **NEW** (no Copy* in `protocol/postgres`) | Proxy P3 + PG→Nano migration mirror (Batch G2) | **2–10× bulk ingest** | only entered on client `COPY`; pg35 uses INSERT via simple Query |
| **3** | Server-side plan cache for **unnamed/text-keyed** extended statements | `SET helios.plan_cache=on` (default off) | **ENHANCE** (`prepared.rs` already caches *named* `cached_plan`+epoch) | Proxy P1 | **+15–25%** extended-heavy | default off; pg35 default path unchanged |
| **4** | "Fast-ingest (regenerable)" `ProfileConfig` bundle: `time_travel_enabled=false` + `wal_sync_mode=Async/GroupCommit` + `durable_commit=false` + `compression=Lz4` + larger `cache_size` + `skip_symbol_refs`/`skip_cross_file_resolve` | profile name / `with_config` | **ENHANCE** (profile bundle system exists in `config.rs`; one profile already sets `time_travel_enabled=false`) | CodeKB (regenerable KB) | **medium** (trims version-key + DELETE/UPDATE traffic) | opt-in profile; default profile unchanged |
| **5** | Cheap session reset (`DISCARD ALL`-equivalent / `helios.reset_session()`) | wire / `SET` | **NEW** (only txn-level discard today) | Proxy P4 (unlocks cross-client conn pooling) | unlock (enables pooling) | new command; not issued on the pg35 path |
| **6** | Pipelined extended exec: N `Bind/Execute` before one `Sync`, ordered, single `ReadyForQuery` | PG wire | **NEW/verify** | Proxy P2 | **+15–40%** batch/ORM | only multi-Execute pipelines; pg35 is one stmt/Sync |
| **7** | Binary result-format per portal (int/bigint/float/timestamp/uuid + correct OIDs) | extended protocol (negotiated) | **NEW/verify** | Proxy P5 | **+5–15%** wide numeric/temporal (HTAP) | pg35 uses text format |
| **8** | Expose hard-coded RocksDB knobs (`write_buffer_size`, `max_write_buffer_number`, `max_background_jobs`, L0 trigger, `bytes_per_sync`) as `StorageConfig` | config fields | **NEW** (literals at `engine.rs:2019-2024`) | CodeKB (+ all bulk) | low–medium | current literals become defaults → identical |
| **9** | Per-session autocommit/implicit-txn fast path | `SET helios.fast_autocommit=on` (default off) | **NEW** | Proxy P6 | +3–8% point reads | OLTP-sensitive → strictly default-off |
| **10** | Faster/cacheable connection setup; `ParameterStatus` advertising active `helios.*` GUCs (capability probe) | wire | **NEW** | Proxy P7 + auto-enable probe | +2–5% short conns | startup path; pg35 reuses connections |

---

## Engine perf fix that is NOT a knob (ship in 3.58, pg35-neutral by construction)

**`Embedder::embed_batch` (item 1).** Today `FastEmbedder` holds one `Mutex<TextEmbedding>` and calls `guard.embed(vec![text], None)` once **per symbol**, serially, inside the single-writer drain (`embed.rs:122,148-158`; loop `storage.rs:690-700`). 344,841 single-element ORT inferences under one lock = the CPU trace collapsing 1767%→579% and the `--with-embeddings` build never finishing. Fix: add `embed_batch(&[&str])` to the trait (default impl loops `embed` for Noop/Http), override on `FastEmbedder` to call `guard.embed(owned, Some(256))` once; rewrite the drain loop to collect non-empty signatures, call once per chunk, scatter back by index. fastembed already `par_chunks` over rayon + ORT `with_intra_threads(all_cpus)`. **No pg35 surface** (no embeddings in pg35). Optional follow-on: static-INT8 `BGESmallENV15Q` default model (already `pub`), GPU ORT provider (cargo-feature, hardware-gated).

## Write-path regression to bisect (CodeKB §4.2) — investigate before 3.58
3.36.1→3.57.0 bulk write slowed 113min→>340min on the same corpus. Candidate divergences to bisect (NOT yet root-caused): (a) `bulk_insert_tuples` gained inline HNSW vector maintenance `vector_dml_gate()`/`on_row_insert()` (gated by `table_has_indexes()` — verify it stays off during parse-heavy phase, `lib.rs:14465-14473`); (b) `plan_cache` → `ShardedLruCache` with a `.clear()` per bulk call (`lib.rs:425`); (c) `engine.rs` grew 9838→12607 lines. **Action:** delegate a focused bisect; if a real default-path regression is found, fix it default-on only if pg35-neutral, else gate it.

## Correctness bugs to triage (orthogonal to perf — do NOT gate this spec)
- **Linker accepted 0 / 599,964 MENTIONS** (exact-text rebind found candidates, committed none).
- **Distill built 0 / 344,841 symbol cards** (scanned=0 — query/scan-target bug).
Both independent of the perf work; bisect separately.

---

## Already available TODAY (document + tell consumers to flip — zero engine work)
`durable_commit=false` (default), `wal_sync_mode=Async`, `time_travel_enabled=false`, `compression=Lz4`, `cache_size↑` (all via `with_config`); `--skip-code-refs`/`--skip-cross-file-resolve`/`--background-quality` and `SET bulk_load_mode=true` (CodeKB plugin already wires these). **Caveat to surface:** `bulk_load_mode` is a near-no-op for the 1.9M symbol/ref volume (that goes through version-exempt `bulk_insert_tuples`); it only spares per-row work on the ~12k single-row file inserts. `simd`/`vector-persist`/`encryption` cargo features and `synchronous_commit`/`smfi_bulk_load_threshold` are no-ops/unwired for this workload — do not chase.

---

## Phasing

- **3.58 (ingest/HTAP foundations):** item 1 (`embed_batch`) + item 2 (COPY wire) + item 4 (fast-ingest profile) + item 8 (RocksDB knobs) + the regression bisect. Biggest, most-demanded wins; COPY also unblocks Proxy Batch G2 migration mirror.
- **3.59 (wire throughput):** item 3 (plan cache unnamed) + item 6 (pipelining) + item 5 (session reset) + item 10 (capability probe).
- **3.60 (polish):** item 7 (binary results) + item 9 (fast_autocommit) + connection-setup. Plus the deferred Nano core polish (engine.rs `Arc<Tuple>` cache, integer-filter scan dedup, INLJ streaming) and R4.1 on-disk layout v2 as their own track.

## Validation gate (every item)
1. Feature default-off / new-path-only.
2. Quiet-machine pg35 A/B (release Codex): the 35-category scoreboard must be unchanged vs 3.57.0 baseline with the feature compiled in but **off**.
3. Targeted A/B with the feature **on** proving the claimed downstream win (COPY ingest rate; extended tps with plan cache; embed wall-time).
4. Correctness suite green; for COPY/pipeline/binary add wire-conformance tests.

## Open co-design items (with consumers)
- `helios.*` GUC surface + `ParameterStatus` capability advertising so Proxy auto-enables each feature only when the connected Nano advertises it.
- Portal `max_rows` partial fetch + `PortalSuspended` (Proxy can stream — confirm Nano honors a row limit).
- COPY format coverage (text/CSV/binary) needed by the migration mirror.
