---
title: Persistent PQ-HNSW vector index — validation report
author: Claude (Opus 4.7, max effort)
date: 2026-05-23
branch: feat/persistent-pq-hnsw
feature: vector-persist (opt-in; off by default)
status: VALIDATION — feature complete (P1–P6), opt-in; awaiting maintainer rollout decision (P7)
design: PROPOSAL_PERSISTENT_PQ_HNSW.md
---

# Validation Report: Persistent PQ-HNSW vector index

## 1. Summary

A new vector index, `PersistentVectorIndex` in `src/vector/persistent.rs`, behind the
opt-in `vector-persist` Cargo feature. It unifies — in a single index — the four
properties the existing vector code could only provide separately:

- **Graph navigation** — an in-house HNSW (no third-party graph crate on this path).
- **PQ compression** — Product-Quantization codes resident in RAM; full vectors on disk.
- **Durability** — RocksDB-backed, crash-recoverable; no rebuild on restart.
- **Safe mutation** — true online deletes with neighbor repair, plus compaction.

Plus **filtered KNN** (predicate-during-traversal) and a **multi-precision** (F32/F16/I8)
rerank-vector dial. The default vector path (`hnsw_index`, `quantized_hnsw`) is unchanged —
every change lives under the feature gate, and a default-feature build is a no-op recompile.

Delivered in seven phases, one commit each:

| Phase | Commit | Deliverable |
|---|---|---|
| P0 | `87b0497` | Design proposal |
| P1 | `478500c` | RocksDB persistence + crash recovery + coarse locking |
| P2 | `154cbdf` | In-house HNSW graph (build + search) |
| P3 | `58f8955` | Online deletes (neighbor repair) + compaction |
| P4 | `f291251` | PQ unified with the graph: ADC traversal + two-stage rerank |
| P5 | `afe16bc` | Filtered KNN (predicate-during-traversal) |
| P6 | `df4c197` | Multi-precision rerank vectors (f16/i8), zero-dependency |

## 2. Methodology

Follows the repository's merge-validation methodology, adapted to an **additive,
feature-gated** change (which has no regression surface on the default build):

| Merge-validation phase | Status |
|---|---|
| 1. Branch + implement | ✅ `feat/persistent-pq-hnsw`, 7 commits |
| 2. Targeted unit tests | ✅ 26 tests in `vector::persistent` |
| 3. Integration regression | ✅ see §4 (full lib suite with the feature on) |
| 4. Targeted benchmark | ✅ see §5 |
| 5. Cross-feature regression | ✅ see §4 |
| 6. Head-to-head OLTP vs `main` | ➖ not applicable — default build is byte-identical (feature gated); no main behavior changes |
| 7. Validation report | ✅ this document |
| 8. Release | ⏸ deferred — opt-in feature; rollout recommendation in §7 |

## 3. Test matrix (26 unit tests, all passing)

| Area | Tests | Gate |
|---|---|---|
| Persistence / recovery (P1) | create/open round-trip, crash-recovery reopen, keyspace isolation, drop, duplicate/missing/dimension errors | exact state restored from disk |
| HNSW graph (P2) | search-finds-self, **recall vs brute-force** | recall@10 ≥ 0.90 vs exact ground truth |
| | graph-survives-reopen | identical results after recover |
| Online deletes (P3) | remove excludes + keeps-searchable, remove-persists-reopen, **churn**, compaction | post-churn recall@10 ≥ 0.80, **zero stale results** |
| PQ (P4) | **memory + recall**, pq-survives-reopen, pq-remove | ≥ 8× RAM reduction AND recall@10 ≥ 0.75 |
| Filtered KNN (P5) | **correctness**, pq-filtered, **beats post-filter**, no-match | filtered recall@10 ≥ 0.90, full k, all match |
| Precision (P6) | f16 roundtrip, encode-size order, **pq f16/i8 recall**, i8 reopen | f16 ≥ 0.70 / i8 ≥ 0.60, footprint i8 < f16 < f32 |

## 4. Cross-feature regression

```
cargo test --lib --features vector-persist
→ 1796 passed; 0 failed; 1 ignored
```

Enabling `vector-persist` and running the **entire** library test suite shows no
regressions in any other subsystem. The default build (`cargo build --lib`) is a no-op
recompile, confirming all changes are isolated to the gated module.

## 5. Benchmark

`cargo test --lib --features vector-persist bench_persistent_pq_summary -- --ignored --nocapture`
(`dim=128, n=2000, k=10, ef=100, 200 queries`). **Debug build** — read the *ratios* and
recall, not absolute timings (release would be substantially faster):

| Mode | recall@10 | RAM (vectors) | query | build |
|---|---:|---:|---:|---:|
| exact f32 | 0.989 | 1000 KB | 4.5 ms | 16.5 s |
| PQ + f32 rerank | 0.987 | **62 KB (16.1×)** | 12.5 ms | 29.8 s |
| PQ + i8 rerank | 0.972 | 62 KB (16.1×) | 12.3 ms | 29.4 s |

### Analysis

- **Memory (the headline):** PQ resides codes in RAM — **16× smaller** (62 KB vs 1000 KB)
  — while recall is essentially unchanged (0.987 vs 0.989). This is the central design
  claim, measured. RAM is identical for f32- and i8-rerank because the rerank precision
  affects the **on-disk** full-vector footprint, not the resident codes; the f16/i8 dial
  cuts disk/IO cost and rerank-load size (and trades a little recall: i8 0.972).
- **Latency:** PQ query is ~3× the exact path here because the two-stage rerank performs a
  RocksDB point-get per candidate to fetch each full vector. This is the expected cost of
  keeping full vectors off-RAM and is the clear next optimization (see §6) — a bounded
  in-memory rerank-vector cache would remove most of it. Build is slower under PQ (encode +
  ADC during construction).

## 6. Known limitations / future work

- **Rerank-vector cache (latency):** the biggest lever — a bounded LRU over recently-reranked
  full vectors would cut PQ query latency toward the exact path. Not yet implemented.
- **PQ is L2-only**, matching the existing `quantization` module (cosine/inner-product would
  need residual handling or normalization).
- **PQ training is upfront** (`create_with_pq` trains on a caller-supplied set, the standard
  train-then-add model). Incremental "train-on-threshold" while streaming inserts is future
  work.
- **Locking is coarse** (one `RwLock` per index) — correctness-first. Region/striped locking
  is a later optimization if contention shows up.
- **Not wired into SQL DDL.** This is the library-level index; exposing it through
  `CREATE INDEX … USING hnsw WITH (persist=true, pq=true, …)` is a separate, larger engine
  integration (parser + planner + `VectorIndexManager`) and is intentionally out of scope
  for this branch.

## 7. Rollout recommendation

- **Keep `vector-persist` opt-in (off by default) for now.** It is additive and isolated;
  shipping it gated lets early adopters use the library API without any risk to existing
  builds.
- **Enable it explicitly:** `cargo build --release --features vector-persist`; construct via
  `PersistentVectorIndex::create(...)` (exact) or `::create_with_pq(...)` (compressed).
- **Suggested next steps, in order:** (1) rerank-vector cache; (2) SQL DDL surface
  (`WITH (persist=true, pq=…, rerank_precision=…)`); (3) consider making it the default
  vector index only after the cache lands and a release-build OLTP comparison is run.
- **Do not flip any default in this branch** — that is a maintainer release decision and
  should follow a release-build benchmark.

## 8. IP posture

All work is derived from published research (HNSW — Malkov & Yashunin, arXiv:1603.09320;
Product Quantization — Jégou et al., 2011) and this repository's own modules
(`quantization`, the SIMD distance kernels, `in_descent`'s descent pattern, RocksDB/bincode).
Level assignment uses the public-domain SplitMix64 mixer; the f16 codec is a hand-rolled
IEEE-754 conversion. No third-party graph/database source is copied, vendored, or
paraphrased. Output remains under the repository's Apache-2.0 license.

## 9. Conclusion

P1–P6 are feature-complete and validated: a single vector index that is graph-fast,
PQ-compressed (16× less RAM at equal recall), durable, online-deletable, filterable, and
precision-tunable — behind an opt-in flag with the default path untouched. Recommended for
merge as an **opt-in** capability; the latency optimization and SQL surface are the natural
follow-ups before considering it the default.
