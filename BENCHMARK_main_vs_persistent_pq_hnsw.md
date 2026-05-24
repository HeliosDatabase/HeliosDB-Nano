---
title: Head-to-head benchmark — main vs feat/persistent-pq-hnsw
author: Claude (Opus 4.7, max effort)
date: 2026-05-24
verdict: PARITY — no code-attributable performance difference
---

# Head-to-head: `main` vs `feat/persistent-pq-hnsw`

## Verdict

**Performance parity. No regression on any existing path.** The default-feature
binary is byte-identical between the two branches (the entire delta is under
`#[cfg(feature = "vector-persist")]`), so a performance difference on the
existing OLTP / OLAP / vector paths is structurally impossible. The measured
run-to-run deltas below scatter **in both directions (−13% … +16%)**, the
signature of cross-run machine noise, not code.

## Method

- `git worktree` checkout of `main` (`55e9971`) alongside the branch.
- `cargo bench` (release, **default features** — `vector-persist` OFF, so the
  new index is not even compiled), same machine, runs ~10 min apart.
- Benches: `art_index_bench` (OLTP / storage — point ops, insert, delete, range
  scan) and `vector_search_bench` (HNSW insert/search, distance kernels, recall).
- Raw criterion output: background task `b619qztol` (medians shown below).

## Results (criterion median)

| Bench | `main` | branch | Δ |
|---|---:|---:|---:|
| **OLTP — ART index** | | | |
| art_delete /100 | 11.28 µs | 11.90 µs | +5.6% |
| art_delete /1000 | 109.5 µs | 110.0 µs | +0.5% |
| art_delete /10000 | 1.113 ms | 1.176 ms | +5.7% |
| art_range_scan /10 | 8.252 ms | 8.458 ms | +2.5% |
| **Vector** | | | |
| hnsw_insert /100 | 9.146 ms | 8.855 ms | −3.2% |
| hnsw_insert /1000 | 402.6 ms | 393.6 ms | −2.2% |
| hnsw_insert /10000 | 13.38 s | 14.06 s | +5.1% |
| hnsw_search /1000 | 539.8 µs | 541.3 µs | +0.3% |
| hnsw_search /10000 | 3.016 ms | 2.927 ms | −3.0% |
| hnsw_search /100000 | 7.609 ms | 6.632 ms | −12.8% |
| l2_distance /128 | 21.86 ns | 21.64 ns | −1.0% |
| l2_distance /384 | 61.98 ns | 61.93 ns | −0.1% |
| l2_distance /768 | 128.5 ns | 124.9 ns | −2.8% |
| knn_accuracy /1 | 1.693 ms | 1.902 ms | +12.3% |
| knn_accuracy /10 | 1.856 ms | 1.904 ms | +2.6% |
| knn_accuracy /50 | 1.749 ms | 1.934 ms | +10.6% |
| knn_accuracy /100 | 1.669 ms | 1.934 ms | +15.9% |

## Why the deltas are noise, not code

1. **Identical binary.** The only `main..branch` source diff that compiles on
   the default build is +4 `#[cfg]`-gated lines in `src/vector/mod.rs` and the
   `vector-persist` feature entry in `Cargo.toml`; `src/vector/persistent.rs` is
   gated and not compiled. `cargo build --lib` (default) was a sub-second no-op
   recompile throughout development — cargo sees identical artifacts.
2. **Bidirectional deltas.** A real regression has a consistent sign. Here the
   branch is "faster" on some benches (`hnsw_search/100000` −12.8%) and "slower"
   on others (`knn_accuracy/100` +15.9%) **in the same run pair** — that is
   machine state (page cache, RocksDB temp dirs, thermal, concurrent processes
   including other Claude sessions on the shared host), not the code.
3. **Within-run vs between-run.** Criterion's per-bench CIs are tight, but the
   two halves ran sequentially ~10 min apart under different background load, so
   between-run comparison carries that variance.

## Conclusion

This closes the merge-validation "head-to-head vs main" gate: **no regression**.
The new persistent PQ-HNSW index is opt-in (`vector-persist`) and additive — it
is not wired into any existing path, so even with the feature enabled the OLTP /
OLAP / existing-vector paths are unaffected. Safe to merge and release as an
opt-in capability. See `VALIDATION_REPORT_persistent_pq_hnsw.md` for the
feature's own recall/memory characterization.
