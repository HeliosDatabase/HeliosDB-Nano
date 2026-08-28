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
- **Prepared / extended-protocol statements** — was the one shape where PostgreSQL led.
  v4.2.0 removed the session-mutex serialization on parameterized autocommit reads, and
  measured 2026-08-28 at v4.19.0 Nano is ahead on both (extended 1.33×–1.57×, prepared
  1.64×–2.39×). It remains Nano's *weakest* read shape, and since most drivers default to
  the extended protocol, a typical application sees less of the advantage than a
  `simple`-only benchmark implies.

None of these are regressions — they're the frontier of an already-winning suite.

## Known limits — where PostgreSQL is ahead

Measured, not estimated. Sources: `docs/benchmarks/heliosdb-nano-vs-postgresql-2026-08-28.md`
and `perf/pagination_depth_curve.json`.

- **Concurrency above ~32 clients.** Nano plateaus across the c=32→64 step (`SELECT 1`
  −1.8%, indexed simple +0.4%, prepared −3.2%) while PostgreSQL gains **+32% to +41%**.
  Nano still leads in absolute terms at c=64, but the lead is shrinking, and extrapolating
  it past c=64 is not supported by any data we have.
- **Bulk load above ~50k rows.** COPY 100k: 171 ms vs PostgreSQL 134 ms (1.28×). Nano is
  faster at 10k (50 ms vs 92 ms); the crossover sits between 10k and 50k.
- **`DROP TABLE` on a large table.** 100k rows: 133 ms vs PostgreSQL 65 ms (2.05×).
- **Sequential-scan analytics** — filter/aggregate/join/top-k over *unindexed* columns.
  Our own SQLite mirror puts Nano at 0.35×–0.58× on those shapes at 10k rows.
- **`LIMIT … OFFSET` at depth** — cost is linear in the offset; use keyset pagination.

Note on durability when reading the write-side numbers: Nano defaults to
`durable_commit = false` while the PostgreSQL side ran `synchronous_commit = on`, so the
COPY and DROP comparisons are not like-for-like on durability — if anything they flatter Nano.
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
