# Performance — what's fast, and where we're still improving

We'd rather you trust our numbers than be dazzled by them. HeliosDB-Nano is fast
and production-useful for general OLTP/HTAP today — and there are areas we're
openly still improving. This page is the honest version of both.

The PostgreSQL comparison is reproducible from `tests/pg35_benchmark.rs` through `benches/public/pg35_run.py`.
**Numbers vary with hardware, dataset, and config** — run them on your box; don't
take a single figure as gospel.

## Where Nano is genuinely strong

Against **PostgreSQL 18.4** on our 35-category `pg35` suite (200 customers / 500
orders / 1000 items shapes), Nano 4.42.0 is faster in all 35 categories. Nano runs
embedded in the test process and in memory, PostgreSQL as a server over TCP that syncs
every commit, so these margins include the deployment, not only the engine. Server
against server, PostgreSQL is ahead in most cells (see *Known limits* below).

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

- **Joins** (INNER / LEFT / 4-table) — the margins here are the suite's smallest
  after top-k. In the 4.42.0 release run they are 9.7×, 13.7× and 8.9×.
  The 4-table join was slower than PostgreSQL up to 4.41.1 (about 0.8×): a `WHERE`
  term pushed below the joins lost the index lookups of the joins above it, which
  4.42.0 fixes. Join ordering, build-side selection, and a streaming
  (non-materializing) join operator are on the roadmap.
- **`ORDER BY … LIMIT` (top-k)** — 3.0× in the 4.42.0 release run, the smallest
  margin of the 35 categories; we're hardening the ordered-index top-k fast path.
- **One-row writes since 4.42.0** — still far ahead of a networked PostgreSQL, but
  slower than 4.41.1 was, because every one-row `UPDATE` and `DELETE` now records the
  row's history for `AS OF`. The release benchmark has the numbers and the setting
  that turns it off (`docs/benchmarks/RELEASE_4_42_0.md`).
- **Prepared / extended-protocol statements** — Nano's weakest read shape. Server against
  server in the 4.42.0 release run, PostgreSQL is ahead at every client count (see below).
  Most drivers default to the extended protocol.

## Known limits — where PostgreSQL is ahead

Measured, not estimated. Source: the pgbench section of
[`docs/benchmarks/RELEASE_4_42_0.md`](benchmarks/RELEASE_4_42_0.md) (Nano 4.42.0 and
PostgreSQL 18.4 as servers on the same host, `pgbench` from the `postgres:18.4-bookworm` image),
and `perf/pagination_depth_curve.json`.

- **Server-to-server reads at 8 clients and more.** `SELECT 1`: PostgreSQL 1.07–1.26× at 8 to
  64 clients (Nano 1.35× at 1 client). Indexed point read as text: PostgreSQL 1.06–1.35× at 8 to
  64 clients (Nano 1.26× at 1 client); extended protocol 1.16–1.93× and prepared 1.38–1.87× at
  every client count.
- **Concurrency above ~32 clients.** From 32 to 64 clients Nano's indexed point read gains
  at most 2% in any protocol while PostgreSQL's gains 9–26%.
- **Bulk load above ~10k rows.** `COPY` of 50,000 rows: 108 ms against PostgreSQL's 78 ms
  (1.38×); 100,000 rows: 180 ms against 109 ms (1.65×). Nano is faster at 10,000 (55 ms against
  76 ms).
- **`DROP TABLE` on a large table.** 100,000 rows: 148 ms against PostgreSQL's 60 ms (2.47×).
- **Sequential-scan analytics** — filter/aggregate/join/top-k over *unindexed* columns.
  Our own SQLite mirror puts Nano at 0.35×–0.58× on those shapes at 10k rows.
- **`LIMIT … OFFSET` at depth** — cost is linear in the offset; use keyset pagination.

Note on durability when reading the write-side numbers: Nano defaults to
`durable_commit = false` while the PostgreSQL side ran `synchronous_commit = on`, so the
COPY and DROP comparisons are not like-for-like on durability — if anything they flatter Nano.

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
# vs PostgreSQL 18.4 (a postgres:18.4 container on 127.0.0.1:25433; the script names it)
benches/public/pg35_run.py out candidate=.

# a release against an earlier one: pg35 plus UPDATE over the wire at 1 to 64 clients
benches/public/release_bench.sh v4.41.1
```

The 4.42.0 run of that command, with the pgbench comparison and what is slower than 4.41.1, is
[`docs/benchmarks/RELEASE_4_42_0.md`](benchmarks/RELEASE_4_42_0.md).

Found a case where Nano is slower than you expected? **Open an issue with the
repro** — the categories above are exactly the ones we want sharpened, and a
concrete workload is the best way to get there.
