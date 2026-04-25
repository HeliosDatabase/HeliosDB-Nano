# Task 190 — HNSW navigation-bias centrality + in-descent prefilter

## Goal

Two HNSW improvements bundled because both touch the same hot path
(`hnsw_rs` greedy descent):

1. **Centrality bias** — bias the greedy descent so high-centrality
   nodes are explored first when distances are close. The
   post-rerank centrality from phase 3 helps the final ranking;
   biasing descent yields the same lift earlier and more cheaply.
2. **In-descent prefilter** — apply `VectorScanOperator`'s pre-
   filter predicate inside the candidate walk. Today we filter
   *after* the walk, which means the scan returns up to `ef`
   candidates and discards filtered ones, costing us recall.

## Acceptance

* `CREATE INDEX … USING hnsw WITH (centrality_col = 'cent')` builds
  an HNSW where descent prefers high-centrality neighbours within
  ε of the closest distance.
* Vector scan with a predicate applies it inside the candidate walk;
  scanned-but-filtered count drops vs. post-walk filter.
* Benchmark (#191): flagship `WITH CONTEXT` query under 500 ms on
  10k-node fixture.

## Design

### Wrapping `hnsw_rs`

`hnsw_rs` is 5600 LOC; forking is heavy. The plan ratifies the
"wrapper" approach: build a thin re-implementation of the greedy-
descent loop that:

1. Holds the same level-0 graph as `hnsw_rs`.
2. Reads it via the public iterator API.
3. Runs our own descent that consults a side-table of centrality
   weights and predicate-evaluator closures.

### `src/vector/biased_descent.rs` (new)

```rust
pub struct BiasedHnsw {
    inner: hnsw_rs::Hnsw<'static, f32, DistL2>,
    centrality: Vec<f32>,           // index by hnsw point id
    epsilon: f32,                    // distance tie-break window
}

impl BiasedHnsw {
    pub fn search(&self, q: &[f32], k: usize, ef: usize,
                  prefilter: Option<&dyn Fn(usize) -> bool>) -> Vec<(usize, f32)>;
}
```

Inside `search`:
1. Standard descent until level 0.
2. At level 0, maintain a candidate priority queue. When two
   candidates are within `epsilon * dist_min`, prefer the higher
   centrality.
3. If `prefilter` is `Some(f)`, drop neighbours where `f(neighbour)
   == false` *before* adding them to the candidate queue. The
   candidate queue stays at ≤ ef regardless of how many neighbours
   were filtered, so recall is maintained even under aggressive
   filters.

### Wiring

* `VectorScanOperator` (`src/sql/executor/scan.rs`) gets an
  `Option<BiasedHnsw>` path it falls into when the index has a
  centrality column.
* The existing `prefilter: Option<LogicalExpr>` (already there
  from earlier task #174) gets evaluated through a closure passed
  into `BiasedHnsw::search`.
* DDL: `CREATE INDEX i ON t USING hnsw (col) WITH (centrality_col
  = 'cent')` parsed in `src/sql/planner.rs` → builds a `BiasedHnsw`
  populating centrality from `cent`.

## Files to touch

* `src/vector/biased_descent.rs` — new.
* `src/vector/mod.rs` — re-exports.
* `src/vector/hnsw_index.rs` — pass-through path that delegates to
  `BiasedHnsw` when the index has a centrality column.
* `src/sql/executor/scan.rs` — VectorScanOperator dispatch.
* `src/sql/planner.rs` — `WITH (centrality_col = ...)` handling.
* `tests/vector_biased_hnsw.rs` — new.

## Tests

1. Build a 1000-vector fixture with skewed centrality. Query the
   regular HNSW and `BiasedHnsw` for the same k. Confirm the
   biased one returns the high-centrality neighbours first when
   distances are within ε.
2. Pre-filter test: 1000 vectors with 10% matching predicate.
   `BiasedHnsw::search(... prefilter=Some(p))` returns ≥ k matches
   when k=10 (regular post-filter would drop below k).
3. `epsilon = 0` falls back to plain HNSW behaviour.

## Out of scope

- Higher-level (level > 0) bias. Concentrating on level-0 yields
  most of the lift.
- Building the centrality column. That's caller-supplied (matches
  the existing `centrality::Centrality::from_edges` Rust API).
