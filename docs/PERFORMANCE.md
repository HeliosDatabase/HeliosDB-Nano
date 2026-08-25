# Performance — what's fast, and where we're still improving

We'd rather you trust our numbers than be dazzled by them. HeliosDB-Nano is fast
and production-useful for general OLTP/HTAP today — and there are areas we're
openly still improving. This page is the honest version of both.

The PostgreSQL comparison is reproducible from `tests/pg35_benchmark.rs`.
**Numbers vary with hardware, dataset, and config** — run them on your box; don't
take a single figure as gospel.

## Where Nano is genuinely strong

Against **PostgreSQL 18.4** on our 35-category `pg35` suite (200 customers / 500
orders / 1000 items shapes), Nano wins the large majority of categories — most by
a wide margin, not a hair:

- **Point operations** (PK lookup, INSERT/UPDATE/DELETE by key) — typically
  *orders of magnitude* faster (in-process, no network round-trip, ART index).
- **DDL, subqueries, aggregates, UPSERT, set ops** — consistently far ahead.
- **Deep pagination** — **keyset** on an indexed column is flat (~35 µs at any
  depth). `LIMIT … OFFSET` is *not* constant-time: it is linear in the offset
  (115–133× from depth 0 to 9 000). See `perf/pagination_depth_curve.json`.
- **Triple wire compatibility** (PostgreSQL + MySQL + REST) on one process, same
  data — no proxy, no second service.

## Where we're competitive, not dominant (actively improving)

A handful of categories are where Nano is *ahead but not by much* — these are our
current focus, and we'd rather name them than hide them:

- **Joins** (INNER / LEFT / 4-table) — Nano's indexed nested-loop join wins, but
  only ~1.2–1.7×, and a 4-table join can dip to roughly tied on a noisy run.
  Improving join ordering, build-side selection, and a streaming (non-materializing)
  join operator is on the roadmap.
- **`ORDER BY … LIMIT` (top-k)** — roughly tied with PostgreSQL; we're hardening the
  ordered-index top-k fast path so it wins consistently.
- **Prepared / extended-protocol statements** — the one shape where PostgreSQL is
  occasionally a touch ahead. The cost is per-`Parse` protocol overhead, not query
  planning (Nano already caches parse + plan by statement text). Reducing per-`Parse`
  allocation and pipelining round-trips is planned.

None of these are regressions — they're the frontier of an already-winning suite.
Our internal goal is to turn "wins most categories" into "wins them all."

## Known limits we're transparent about

- **Bulk-loading millions of rows into a table with several secondary indexes** is
  slower than it should be: secondary-index maintenance currently runs per row on the
  bulk path. There's an **opt-in `bulk_load_mode`** (`SET bulk_load_mode = true`) that
  already suspends some per-row work, and deferring index build to batch-end is on the
  roadmap. This is a specialized ingestion scenario — it does **not** affect the
  general OLTP/transactional path.
- **Very large embedding workloads** (the optional in-process `code-graph` / vector
  feature) can hit memory pressure at corpus scale; batching and streaming
  improvements are in progress. The embedder is opt-in (`--features code-embed`) and
  unrelated to core SQL performance.
- **Durable (power-loss-safe) autocommit** is honestly slow by design — fsync per
  commit. Use `durable_commit = false` (the default) for throughput, or batch in a
  transaction. This is physics, not a bug.

## How to reproduce

```bash
# vs PostgreSQL (point a local PG at PG35_CONNSTR)
cargo test --release --test pg35_benchmark -- --ignored --nocapture
```

Found a case where Nano is slower than you expected? **Open an issue with the
repro** — the categories above are exactly the ones we want sharpened, and a
concrete workload is the best way to get there.
