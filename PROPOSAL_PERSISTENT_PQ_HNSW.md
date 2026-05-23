---
title: Persistent PQ-HNSW — unified, durable, deletable vector index — design proposal
authors: Claude (Opus 4.7, max effort)
sponsor: gpc-enterprise admin (requested 2026-05-23)
status: PROPOSAL — design-first, awaiting triage before implementation
related:
  - src/vector/hnsw_index.rs        (current graph path — wraps the hnsw_rs crate, in-RAM)
  - src/vector/quantized_hnsw.rs    (current PQ path — brute-force ADC scan, no graph)
  - src/vector/quantization/         (Product Quantizer: codebook/encoder/decoder/distance/training)
  - src/vector/simd/                 (AVX2 L2/cosine/dot kernels)
  - src/vector/in_descent.rs         (in-house graph-descent primitive: Adjacency/Positions)
  - src/vector/biased_descent.rs     (centrality-biased descent helper)
  - src/storage/vector_index.rs      (RocksDB-backed vector storage — persistence integration point)
  - src/storage/mvcc.rs              (MVCC primitives)
  - src/storage/lock_manager.rs      (lock manager)
  - src/storage/predicate_pushdown.rs (predicate pushdown — filtered-search integration point)
  - FEATURE_REQUEST_adaptive_topk.md
priority: P1 (closes a credibility gap in the vector subsystem) + P1 (enables RAG-at-scale)
feature_flag: vector-persist (new, opt-in; default path unchanged during transition)
---

# Proposal: Persistent PQ-HNSW — a unified, durable, deletable, filterable vector index

## TL;DR

Nano's vector subsystem is split across two implementations that each solve half the
problem and cannot be combined:

1. **`src/vector/hnsw_index.rs`** — a thin wrapper over the third-party `hnsw_rs` crate.
   It gives sub-linear graph search but is **RAM-only** (rebuilt from scratch on restart),
   uses **tombstone-only deletes** with no graph repair (the code itself notes the index
   must be "rebuilt periodically", `hnsw_index.rs:218`), holds a **single global lock**
   over the whole structure, and is **`f32`-only**.
2. **`src/vector/quantized_hnsw.rs`** — despite its name, this is **not a graph**. Its
   `graph` field is never populated; `insert()` only appends a Product-Quantization (PQ)
   code, and `search()` is a **brute-force linear ADC scan** over all codes
   (`quantized_hnsw.rs:247-262`). It gives 8–16× memory savings but **O(N) query time**.

So today a user must choose **either** sub-linear latency (full `f32`, in-RAM, no durable
deletes) **or** small memory (linear scan). There is no index that is simultaneously
**graph-fast, memory-efficient, durable, and safely mutable**.

This proposal specifies **Persistent PQ-HNSW**: one index that is

- **graph-navigable** (HNSW, native to Nano — no third-party graph dependency on the durable path),
- **PQ-compressed** with an **exact two-stage rerank** (ADC candidate scan → precise re-scoring),
- **persisted in RocksDB** and crash-recoverable, with a bounded in-memory hot cache,
- **transactional** (index mutations commit atomically with the row mutation, via Nano's MVCC),
- **truly deletable online** (neighbor repair + compaction, not just tombstones),
- **filterable during traversal** (predicate pushdown into the graph walk), and
- **multi-precision** (`f32` / `f16` / `i8` vector storage).

It is delivered behind a new opt-in `vector-persist` feature flag, in phases, each gated by
the repo's standard merge-validation methodology. The existing in-RAM path remains the
default until the new path meets or beats it on recall, latency, and memory.

---

## 1. Motivation and goals

### 1.1 Problems with the current state (all internal)

| Limitation | Where | Consequence |
|---|---|---|
| Graph index is RAM-only | `hnsw_index.rs` (`Arc<RwLock<Hnsw<..>>>`) | Cold-start rebuild cost; index must fit in RAM; no durability |
| Deletes are tombstones only | `hnsw_index.rs:211-229` | Recall degrades under churn; periodic full rebuild required |
| Single global lock | `hnsw_index.rs` (`parking_lot::RwLock`) | Writers serialize against the whole index |
| `QuantizedHnswIndex` has no graph | `quantized_hnsw.rs:212-217` | "Quantized HNSW" is actually a brute-force scan — O(N) |
| PQ and graph are mutually exclusive | two separate types | Can't get small memory **and** sub-linear latency together |
| `f32` only on the graph path | `hnsw_index.rs` | No memory/accuracy dial below 32 bits/dim |
| No filtered KNN in the index | — | `WHERE … ORDER BY v <-> $q` filters *around* the index, hurting top-k quality |
| Third-party graph dependency | `hnsw_rs = 0.3.3` | We don't control deletes/persistence/filtering hooks |

### 1.2 Goals

- **G1 — Durability:** index survives restart with no rebuild; recoverable after crash.
- **G2 — Unified PQ + graph:** sub-linear search **and** compressed storage in one index.
- **G3 — Correct online deletes:** delete repairs the graph; recall stays stable under churn.
- **G4 — Transactional integrity:** index mutations are atomic with the owning row write.
- **G5 — Filtered KNN:** apply row predicates during traversal, preserving top-k quality.
- **G6 — Memory dial:** `f32`/`f16`/`i8` storage types plus PQ; tunable accuracy vs footprint.
- **G7 — No regression:** equal-or-better recall and latency vs the current in-RAM path before it becomes default.
- **G8 — Self-owned graph:** remove the durable path's reliance on a third-party graph crate.

### 1.3 Non-goals (explicitly out of scope here)

- Distributed/sharded vector indexes across nodes (separate future track).
- GPU-accelerated index build.
- Learned indexes / IVF clustering (PQ here is flat-residual over the graph, not IVF).
- Changing the public SQL distance operators (`<->`, `<#>`, `<=>`) or DDL grammar shape.

---

## 2. Background (public research and prior art)

This design is built from **published algorithms** and **permissively-licensed public
reference implementations**, implemented independently against Nano's own architecture.
Key references:

- **HNSW.** Malkov, Yu. A., & Yashunin, D. A., *"Efficient and robust approximate nearest
  neighbor search using Hierarchical Navigable Small World graphs"*, arXiv:1603.09320
  (IEEE TPAMI, 2020). Source of: hierarchical layers, level assignment
  `l = floor(-ln(U)·mL)`, greedy descent through upper layers, the `ef`-bounded layer-0
  search, and the **neighbor-selection heuristic** with its optional *extend-candidates* and
  *keep-pruned-connections* variants (paper Algorithm 4). The reference implementation
  **hnswlib** is Apache-2.0.
- **Product Quantization.** Jégou, H., Douze, M., & Schmid, C., *"Product Quantization for
  Nearest Neighbor Search"*, IEEE TPAMI 33(1), 2011. Source of: subspace codebooks,
  **Asymmetric Distance Computation (ADC)**, and the precomputed distance-table scan. Nano
  already implements this in `src/vector/quantization/`.
- **Graph + PQ + rerank.** The pattern of navigating a graph using compressed (ADC) distances
  and re-ranking a small candidate set with exact vectors is well established in the public,
  **MIT-licensed FAISS** library (`IndexHNSWPQ`, `IndexIVFPQ`/`IndexIVFPQR`). We adopt the
  *pattern*, not the code.
- **Filtered ANN.** Published predicate-aware graph-search approaches (e.g. Filtered-DiskANN,
  WWW 2023; ACORN, 2024) motivate evaluating the predicate *during* traversal rather than
  post-filtering. We integrate this with Nano's existing `predicate_pushdown` machinery.

See §11 (IP posture) for the diligence statement.

---

## 3. Architecture overview

```
                         ┌─────────────────────────────────────────────┐
   SQL / embedded API    │            PqHnswIndex  (new)                │
   CREATE INDEX … hnsw   │                                              │
   v <-> $q  /  filtered │   ┌───────────────┐     ┌──────────────────┐ │
        │                │   │  Graph layer  │     │  Vector store    │ │
        ▼                │   │  (in-house    │     │  PQ codes  +     │ │
   ┌──────────┐  insert  │   │  HNSW: levels,│ ADC │  rerank vectors  │ │
   │ Planner/ │ ───────▶ │   │  adjacency,   │◀───▶│  (f32/f16/i8)    │ │
   │ executor │  search  │   │  heuristic)   │     └──────────────────┘ │
   └──────────┘ ◀─────── │   └──────┬────────┘              │           │
        ▲   filtered KNN │          │  hot cache (LRU, capped)          │
        │                │   ┌──────▼───────────────────────▼────────┐  │
        │                │   │   Persistence adapter (RocksDB CF)     │  │
        │                │   │   meta / nodes / codes / vecs / tomb   │  │
        │                │   └──────┬─────────────────────────────────┘  │
        │                └──────────┼──────────────────────────────────┘
        │                           ▼
        │                   ┌──────────────┐   atomic with row write
        └─ predicate ─────▶ │ MVCC + locks │◀── (same transaction)
                            └──────────────┘
```

The index is four cooperating layers:

1. **Graph layer** — an **in-house HNSW** (levels, adjacency lists, entry point,
   level assignment, neighbor heuristic). Built on Nano's existing descent primitives
   (`in_descent.rs`, `biased_descent.rs`) plus the published HNSW algorithm. Replaces
   `hnsw_rs` on the durable path.
2. **Vector store** — per element: a PQ code (for ADC during traversal) and a *rerank
   vector* in the configured precision (`f32`/`f16`/`i8`) for exact final scoring.
3. **Persistence adapter** — maps graph + vectors to RocksDB keys; provides a bounded
   in-memory hot cache; handles recovery.
4. **Transaction integration** — index mutations participate in the owning row's MVCC
   transaction via `src/storage/mvcc.rs` and `lock_manager.rs`.

---

## 4. Storage model (RocksDB)

All keys live under a per-index prefix `__vidx:<index_id>`. Proposed logical keyspace
(exact CF layout finalized in Phase 1):

| Key | Value | Notes |
|---|---|---|
| `__vidx:<id>:meta` | index params + runtime state | dim, metric, M, M0, ef_construction, mL, PQ config, storage precision, entry-point id, element counter, layer count, schema version |
| `__vidx:<id>:lvl:<elem>` | top level of element | small; enables entry-point/level lookups |
| `__vidx:<id>:adj:<layer>:<elem>` | neighbor id list | adjacency for `elem` at `layer`; bounded by M (M0 at layer 0) |
| `__vidx:<id>:code:<elem>` | PQ code bytes | compact; used for ADC traversal |
| `__vidx:<id>:vec:<elem>` | rerank vector (f32/f16/i8) | exact-ish re-scoring of the candidate set |
| `__vidx:<id>:map:<elem>` | external row id | element→row mapping |
| `__vidx:<id>:rmap:<row>` | element id | row→element reverse mapping |
| `__vidx:<id>:tomb` | compressed deleted-id bitmap | soft-deletes pending compaction |

Design points:

- **Write-through within the host transaction.** On row insert/update/delete, the
  corresponding index mutations are written to RocksDB in the **same** transaction
  (G4). No separate, lossy index journal.
- **Bounded hot cache.** A size-capped (configurable, e.g. `vector.index_cache_mb`)
  in-memory LRU holds hot nodes/codes/vectors. A cache miss reads from RocksDB. This
  decouples index size from RAM (G1) while keeping hot traversals fast.
- **Recovery = open + warm.** On startup, read `meta`, set the entry point and counters,
  and lazily fault nodes/vectors into the cache on demand. No full rebuild (G1).
- **Rerank vector precision** is independent of PQ: a user can store `i8` rerank vectors
  for extra savings, or `f32` for exact rerank (G6).

---

## 5. Algorithms

### 5.1 Insert

1. Assign a level `l = floor(-ln(U)·mL)` (U ~ Uniform(0,1)); `mL = 1/ln(M)` by default.
2. PQ-encode the vector (reuse `src/vector/quantization`); store code + rerank vector.
3. Greedy-descend from the entry point through layers `> l` (ef = 1) to find the local entry.
4. For each layer `min(l, top)…0`, run the `ef_construction` search, then select up to
   `M` (`M0` at layer 0) neighbors via the **paper heuristic** (with optional
   *extend-candidates* / *keep-pruned-connections*, configurable per index).
5. Add bidirectional edges; prune over-full neighbor lists with the same heuristic.
6. Promote the entry point if `l` exceeds the current top level.

Distance during construction uses **ADC over PQ codes** for candidate ranking, matching the
query path so that graph topology is consistent with how it will be searched. SIMD kernels
(`src/vector/simd`) accelerate exact distances where used.

### 5.2 Search (filtered KNN)

1. Greedy-descend upper layers (ef = 1) to the layer-0 entry.
2. Run the `ef`-bounded best-first search at layer 0 using **ADC** for candidate distances.
3. **Filter during traversal (G5):** evaluate the row predicate (via `predicate_pushdown`)
   on each visited candidate. Non-matching nodes are **traversed but not collected**
   (so connectivity is preserved), matching nodes enter the result heap. A selectivity
   estimate chooses between *filter-during-traversal* (low selectivity) and
   *search-then-filter with widened `ef`* (high selectivity).
4. **Two-stage rerank:** take the top `ef` ADC candidates, re-score them with the exact/`f16`
   rerank vectors, and return the true top-`k`. This recovers the accuracy PQ alone loses.
5. `ef` is chosen by the existing dynamic-`ef` logic (`hnsw_index.rs:calculate_ef_search`),
   extended to account for filter selectivity.

### 5.3 Delete (true online, G3)

1. Resolve `row → elem`; load the element's neighbors at every layer.
2. Remove the element's edges; for each affected neighbor, **re-select** its neighbor set
   from the union of its remaining neighbors and the deleted node's other neighbors (local
   repair), so connectivity and degree are preserved.
3. If the deleted node was the entry point, promote the highest-level surviving neighbor.
4. Mark the id in the deleted-bitmap; free its keys.
5. **Compaction:** a background pass (and an explicit `REINDEX`/maintenance hook) rebuilds
   regions whose tombstone density crosses a threshold, bounding long-term drift. This is a
   safety net; steady-state deletes are handled by local repair, not bulk rebuild.

---

## 6. Concurrency & transactions

- **Atomicity (G4).** Index writes are emitted into the same MVCC transaction as the row
  write (`src/storage/mvcc.rs`). Commit/rollback of the row commits/rolls back the index
  mutation. No partial index state on abort.
- **Isolation.** Reads see a consistent index snapshot for their transaction; structural
  mutations take an index-scoped lock via `src/storage/lock_manager.rs`. **Phase 1 ships
  coarse (per-index) structural locking for correctness first**; a later phase refines to
  region/lock-striped granularity if benchmarks show contention.
- **Bulk build fast-path.** Initial index construction over an existing table uses a
  single-writer bulk path (no per-row transaction overhead), consistent with Nano's existing
  bulk-load patterns, then flips to transactional incremental maintenance.

> Note: this uses Nano's *own* transaction and locking primitives end-to-end; no external
> concurrency scheme is adopted.

---

## 7. Public API & SQL surface

**No grammar changes to the operators.** DDL gains opt-in `WITH` options:

```sql
CREATE INDEX docs_embed_idx ON docs USING hnsw (embedding vector_cosine_ops)
WITH (
  persist = true,            -- new: durable RocksDB-backed index (default false initially)
  m = 16, ef_construction = 200,
  pq = true, pq_subvectors = 96, pq_bits = 8,   -- compression
  rerank_precision = 'f16',  -- f32 | f16 | i8
  index_cache_mb = 256
);

-- search is unchanged:
SELECT id, embedding <=> $1 AS dist
FROM docs
WHERE lang = 'en'            -- filtered during traversal
ORDER BY dist LIMIT 10;
```

**Embedded Rust API.** Introduce a unified `PqHnswIndex` (+ `PqHnswConfig`) in a new
`src/vector/persistent/` module, mirroring the ergonomics of the existing `HnswIndex` /
`QuantizedHnswIndex` (`new`, `insert`, `search`, `search_with_ef`, `delete`, `len`). The two
legacy types remain for the in-RAM/back-compat path during transition.

---

## 8. Phased implementation plan

Each phase is a separate change gated by the repo's merge-validation methodology
(branch → unit → integration regression → targeted bench → cross-feature regression →
head-to-head OLTP vs `main` → report → release).

| Phase | Deliverable | Gate |
|---|---|---|
| **P0** | This design doc | triage sign-off |
| **P1** | RocksDB persistence + crash-recovery for an index structure behind `vector-persist`; coarse locking; serialize/restore round-trip tests | recovery integrity tests pass; no default-path change |
| **P2** | In-house HNSW graph (levels, heuristic, greedy search) using `in_descent`/`biased_descent`; **recall parity** with the `hnsw_rs` path | recall@k ≥ current within tolerance on bench datasets |
| **P3** | True online deletes + neighbor repair + tombstone compaction | churn test: recall stable after N delete/insert cycles |
| **P4** | Unify PQ with the graph: ADC traversal + two-stage rerank | memory ↓ 8–16× at target recall; latency within budget |
| **P5** | Filtered KNN (predicate-during-traversal + selectivity switch) | filtered-query correctness + speed vs post-filter |
| **P6** | Multi-precision rerank vectors (`f32`/`f16`/`i8`) | accuracy/footprint dial validated |
| **P7** | Make `persist=true` selectable as default; docs; full validation report | head-to-head OLTP vs `main` shows no regression |

---

## 9. Testing & benchmarking

- **Recall:** recall@{1,10,100} vs `ef` on fixed embedding datasets; assert thresholds
  (mirrors the spirit of an in-tree recall test so regressions are caught in CI).
- **Latency:** p50/p99 query latency vs N and vs `ef`; insert throughput.
- **Memory:** bytes/vector at target recall (PQ on/off; rerank precision sweep);
  `MemoryStats`-style reporting (extend `quantized_hnsw.rs::memory_stats`).
- **Churn:** interleaved insert/delete cycles; assert recall stability and bounded tombstone
  growth (the property the current tombstone-only path fails).
- **Crash recovery:** kill mid-write; reopen; assert index == expected, atomic with rows.
- **Filtered correctness:** filtered KNN result equals brute-force filtered top-k.
- Harness lives under `benches/`; datasets are synthetic + standard public ANN vectors.

---

## 10. Risks & mitigations

| Risk | Likelihood | Mitigation |
|---|---|---|
| Recall regression vs `hnsw_rs` | Med | P2 gate requires parity before progressing; keep heuristic knobs tunable |
| Storage/MVCC blast radius | Med | Feature-flagged; default path untouched until P7; coarse locking first |
| RocksDB write amplification | Med | Batch adjacency writes; cache hot nodes; tune CF options |
| Recovery latency on large indexes | Low-Med | Lazy fault-in + warm cache rather than eager full load |
| Lock contention on hot index | Med | Start coarse, measure, refine to striped/region locks only if needed |
| Delete repair complexity/bugs | Med | Property tests + churn fuzzing; compaction as correctness backstop |

## 11. IP posture & diligence

- The design is derived **only** from (a) Nano's existing, owned modules (cited in the
  frontmatter), and (b) **published, citable research** (§2: the HNSW paper, the Product
  Quantization paper, public filtered-ANN papers).
- Reference implementations consulted for the well-known **HNSW** and **HNSW+PQ+rerank**
  patterns are **permissively licensed** public projects (**hnswlib** — Apache-2.0;
  **FAISS** — MIT). We adopt **documented algorithms and patterns**, implemented
  independently against Nano's architecture; **no third-party source is copied, vendored,
  or paraphrased**, and no source-available/copyleft database code is referenced.
- All naming, key schemas, module layout, and the transaction/persistence integration are
  **original to Nano** and consistent with its existing conventions.
- Output stays under the repository's existing **Apache-2.0** license; the algorithms used
  are unencumbered prior art with broad public implementation.

## 12. Open questions for triage

1. PQ defaults: subvector count / bits per dimension class (768 vs 1536) — pick conservative
   defaults or auto-tune from `dimension`?
2. Should `persist=true` eventually become the default, or remain opt-in long-term?
3. Compaction trigger policy: tombstone-density threshold vs scheduled vs manual `REINDEX`.
4. Lock granularity target for P1 vs later — is coarse acceptable for the first GA?
5. Keep the `hnsw_rs` in-RAM path indefinitely for tiny/ephemeral indexes, or deprecate post-P7?

---

*Status: design-first. No implementation code is included in this proposal. Implementation
begins only after triage sign-off, one phase per branch, each behind the `vector-persist`
flag and gated by full merge-validation.*
