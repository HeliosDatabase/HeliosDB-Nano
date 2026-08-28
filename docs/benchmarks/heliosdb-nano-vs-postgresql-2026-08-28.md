# HeliosDB-Nano vs PostgreSQL — 2026-08-28

**Why this run exists.** A published "honest caveats" list claimed *"PostgreSQL roughly 2× faster
on indexed reads and ~20× on COPY."* Both figures trace to a single run
(`heliosdb-nano-vs-postgresql-2026-06-28.md`, Nano 3.60.6/3.60.7) that was overtaken within a
week — but the rebuttal was itself unmeasured: **no vs-PostgreSQL run had been recorded since
2026-07-05 / v4.0.0, nineteen releases earlier.** Rather than publish either number on trust, we
re-measured. The claim as written is refuted by this run; the residual caveats below are the ones
the data actually supports.

## Setup

| | |
|---|---|
| Nano | v4.19.0 + the PG-parity ("honest caveats") change set, native host binary, sha256 `4e1431d5…` |
| PostgreSQL | 18.4 (`postgres:18.4-bookworm`), container `pg_bench_nano`, host networking |
| Client | `pgbench` from the same PostgreSQL image, over TCP to **both** engines |
| Harness | `docs/benchmarks/bench-engines.sh`, unmodified, default dataset sizes |
| Cell | 8 s, clients 1 / 8 / 16 / 32 / 64, `-j min(c,8)` |
| Host | shared build host, load ~3.5 at start; single run per cell, no repetition |

Both engines are measured **wire-to-wire** — Nano runs as a server over TCP, not embedded. This is
the comparison that matters for a drop-in claim, and it is deliberately *less* flattering to Nano
than the embedded `PG35_BENCHMARK.md` methodology.

**Protocols.** The 2026-07-05 baseline swept only `simple`. That is the protocol almost no real
client uses, so this run sweeps `simple`, `extended` and `prepared`. The difference turns out to
matter, and measuring only `simple` is part of why the stale claim went unchallenged.

## Indexed point-read, 50k-row table (TPS, higher better)

| protocol | clients | Nano | PostgreSQL | ratio |
|---|---|---|---|---|
| simple | 1 | 14,606 | 6,983 | **2.09× Nano** |
| simple | 16 | 139,033 | 62,650 | **2.22× Nano** |
| simple | 64 | 166,711 | 99,442 | **1.68× Nano** |
| extended | 1 | 9,729 | 6,731 | **1.45× Nano** |
| extended | 16 | 96,116 | 61,127 | **1.57× Nano** |
| extended | 64 | 129,323 | 97,452 | **1.33× Nano** |
| prepared | 1 | 15,215 | 8,365 | **1.82× Nano** |
| prepared | 16 | 143,258 | 68,166 | **2.10× Nano** |
| prepared | 64 | 196,990 | 120,008 | **1.64× Nano** |

**Nano leads on every protocol at every concurrency measured.** The "PostgreSQL ~2× faster on
indexed reads" claim is refuted: it describes the pre-v4.0.0 engine.

## `SELECT 1` (protocol/connection throughput, TPS)

| clients | Nano | PostgreSQL | ratio |
|---|---|---|---|
| 1 | 25,971 | 9,132 | **2.84× Nano** |
| 16 | 193,507 | 69,666 | **2.78× Nano** |
| 32 | 227,533 | 89,141 | **2.55× Nano** |
| 64 | 223,330 | 121,133 | **1.84× Nano** |

## Bulk load — `COPY` (ms, lower better)

| rows | Nano | PostgreSQL | ratio |
|---|---|---|---|
| 10,000 | **50** | 92 | **1.84× Nano** |
| 50,000 | 104 | 94 | 1.11× PostgreSQL |
| 100,000 | 171 | 134 | 1.28× PostgreSQL |

**"~20× on COPY" is refuted.** The gap at 100k rows is 1.28×, and Nano is *faster* at 10k. The
crossover is between 10k and 50k, consistent with a fixed per-batch cost that Nano amortises well
and a per-row cost PostgreSQL amortises better at volume.

## `DROP TABLE`, 100k rows (ms, lower better)

| | Nano | PostgreSQL | ratio |
|---|---|---|---|
| 100k | 133 | 65 | **2.05× PostgreSQL** |

PostgreSQL is ~2× faster here. This gap is **not mentioned in the caveats list at all**, and is
better evidenced than either figure that was.

## The residual caveats this run actually supports

1. **The extended protocol is Nano's weakest shape.** 1.33–1.57× ahead versus 1.64–2.39× on
   prepared and 1.68–2.22× on simple. Most drivers default to extended, so a typical application
   sees materially less of Nano's advantage than a `simple`-only benchmark advertises.

2. **Nano stops scaling at c=32→64; PostgreSQL does not.** Measured across the c=32→c=64 step:

   | workload | Nano | PostgreSQL |
   |---|---|---|
   | `SELECT 1` | 227,533 → 223,330 (**−1.8%**) | 89,141 → 121,133 (**+36%**) |
   | indexed, simple | 166,102 → 166,711 (**+0.4%**) | 75,337 → 99,442 (**+32%**) |
   | indexed, prepared | 203,441 → 196,990 (**−3.2%**) | 85,198 → 120,008 (**+41%**) |

   Nano is flat-to-negative where PostgreSQL gains a third. This is the concurrency knee recorded
   as **unverified** in `docs/plans/ROADMAP_V5.md` §4.3 — it is now verified, and it is the single
   most load-bearing performance caveat we have. Extrapolating Nano's lead past c=64 is not
   supported by this data.

3. **Bulk load above ~50k rows and `DROP TABLE` favour PostgreSQL**, by 1.28× and 2.05×.

## What this run does NOT cover

Stated so the numbers are not read as broader than they are: no sequential-scan analytics
(filter/aggregate/join/top-k over unindexed columns), which our own SQLite mirror puts at
0.35×–0.58×; no writes under contention; no `LIMIT … OFFSET` at depth (linear in the offset — see
`perf/pagination_depth_curve.json`); no durability matrix (Nano defaults to
`durable_commit = false`; PostgreSQL ran `synchronous_commit = on`, so the COPY and DROP numbers
above are **not** a like-for-like durability comparison and if anything flatter Nano); single run
per cell, so treat differences under ~10% as noise.

## Reproducing

```bash
cargo build --release --bin heliosdb-nano
PROTOCOLS="simple extended prepared" \
  ./docs/benchmarks/bench-engines.sh "4.19.0:$(pwd)/target/release/heliosdb-nano"
```

Requires a PostgreSQL container on `127.0.0.1:25433` (`PGHOST`/`PGPORT`/`PGDB`/`PGUSER`/`PGPASS`
override it). **Verify the binary's version and sha256 before trusting a result** — an earlier
attempt at this measurement was discarded because the build raced concurrent edits to the working
tree, and a version check alone did not catch it since the version string comes from `Cargo.toml`.
