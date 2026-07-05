# HeliosDB-Nano vs PostgreSQL — Scalability & Performance

> **⚠️ Superseded by [`heliosdb-nano-vs-postgresql-2026-07-05.md`](heliosdb-nano-vs-postgresql-2026-07-05.md) (v4.0.0).**
> The indexed-read result below (PostgreSQL winning) was **reversed** by the
> 2026-07 campaign: v4.0.0 now leads PostgreSQL 1.7×–2.3× on indexed reads, and
> COPY is ~6× faster than the 3.60.x numbers here. Kept for historical reference.

**Date:** 2026-06-28
**Engines:** PostgreSQL 18.4 vs HeliosDB-Nano `3.31.0-dev, 3.58.1, 3.60.4, 3.60.5, 3.60.6, 3.60.7`
**Client:** `pgbench` (from the PostgreSQL 18.4 image), concurrency sweep c ∈ {1, 8, 16, 32, 64}, 8 s per cell.

Raw data: [`results/`](results/). Reproduce: [`bench-engines.sh`](bench-engines.sh).

---

## TL;DR

| Dimension | Winner | Margin |
|---|---|---|
| `SELECT 1` throughput (protocol + simple query) | **HeliosDB-Nano** | ~2–3× over PostgreSQL |
| Indexed point-read (storage path) | **PostgreSQL** | ~1.3× @ c=1 → ~2.1× @ c=64 |
| Bulk-load (COPY) | **PostgreSQL** | ~20× faster, but Nano is now viable |
| `DROP TABLE` (100k rows) | **PostgreSQL ≈ Nano 3.60.7** | Nano **3.60.6 = >60 s stall → 3.60.7 = 219 ms** |

**Two headline findings:**
1. **Nano's advantage is in the query/protocol layer, not storage.** It crushes PostgreSQL on `SELECT 1` (2–3×) but PostgreSQL's mature storage engine wins on real indexed reads and bulk-load.
2. **3.60.7 fixes a severe write-path stall.** Dropping a 100k-row table took **>60 s on 3.60.6** (O(rows) fsync) and **219 ms on 3.60.7** — a ~270× fix. Bulk-load via `COPY` also became viable on the 3.60.x line (a >90 s hang on 3.31.0-dev → ~2.3 s for 100k rows).

---

## 1. `SELECT 1` — protocol & connection scalability (TPS, higher is better)

| clients | PostgreSQL | 3.31.0-dev | 3.58.1 | 3.60.4 | 3.60.5 | 3.60.6 | 3.60.7 |
|--------:|-----------:|-----------:|-------:|-------:|-------:|-------:|-------:|
|  1 |   9,779 |  20,308 |  25,933 |  25,783 |  26,066 |  25,403 | **26,374** |
|  8 |  51,133 |  95,537 | **121,635** | 117,697 | 121,112 | 119,292 | 117,852 |
| 16 |  67,792 | 169,258 | 187,685 | 193,345 | 194,174 | 192,654 | **201,620** |
| 32 |  84,575 | 209,838 | **241,103** | 218,510 | 230,281 | 237,636 | 216,450 |
| 64 | 118,435 | 223,857 | 217,974 | 237,038 | 243,561 | 227,433 | **245,346** |

- All Nano builds beat PostgreSQL **~2–3×** at every concurrency (latency ~half: e.g. 0.038 ms vs 0.102 ms @ c=1).
- The **step-change was 3.31.0-dev → 3.58.1**; **3.58.1 → 3.60.x is one tier** (within ~5 % run-to-run noise — rankings flip cell-to-cell).
- **3.60.6/3.60.7 are bug-fix releases** — equal `SELECT 1` numbers confirm **no throughput regression**; 3.60.7 is at/near the top with the best high-concurrency latency.
- PostgreSQL scales smoothly but caps ~118 k vs Nano's ~245 k on this workload.

## 2. Bulk-load — `COPY` from CSV (lower ms is better)

| rows | PostgreSQL | Nano 3.60.6 | Nano 3.60.7 |
|-----:|-----------:|------------:|------------:|
|  10,000 |  91 ms |   309 ms |   248 ms |
|  50,000 |  91 ms | 1,193 ms | 1,198 ms |
| 100,000 | 115 ms | 2,307 ms | 2,361 ms |

- **Bulk-load now works on the 3.60.x line** — a 100k-row `COPY` lands in ~2.3 s. On **3.31.0-dev the same load hung past 90 s** (synchronous-WAL per-row path), so this is a categorical fix, not just a speedup.
- **PostgreSQL is still ~20× faster** at bulk-load (115 ms for 100k) — its `COPY` path is essentially flat across these sizes. Nano scales roughly linearly (~23 µs/row).
- 3.60.6 and 3.60.7 `COPY` throughput are equal (within noise) — see §4 for where they actually differ.

## 3. Indexed point-read — storage path (TPS, higher is better)

`SELECT abalance FROM t WHERE aid = :rand` over a 50k-row table with a btree index on `aid`.

| clients | PostgreSQL | Nano 3.60.6 | Nano 3.60.7 |
|--------:|-----------:|------------:|------------:|
|  1 |  6,784 |  5,349 |  4,930 |
|  8 | 40,282 | 25,915 | 25,698 |
| 16 | 59,778 | 44,073 | 42,449 |
| 32 | 71,802 | 48,234 | 49,158 |
| 64 | **99,604** | 47,594 | 47,477 |

- **PostgreSQL wins the storage read path** — ~1.3× @ c=1, widening to **~2.1× @ c=64**.
- **Nano saturates ~48 k TPS around c=32** and does not scale past it; PostgreSQL keeps climbing to ~100 k.
- 3.60.6 ≈ 3.60.7 on reads (the 3.60.7 fixes are on the write/drop path, not reads).
- This is the mirror image of `SELECT 1`: once a real index/buffer lookup is involved, PostgreSQL's mature storage engine leads.

## 4. `DROP TABLE` on a 100k-row table — the 3.60.7 write-path fix

| engine | DROP time |
|---|---|
| PostgreSQL | 61 ms |
| **Nano 3.60.6** | **>60,000 ms (timed out)** |
| **Nano 3.60.7** | **219 ms** |

- 3.60.6 spends O(rows) fsync work on `DROP`, stalling >60 s on 100k rows; **3.60.7 fixes it (~270× faster)**, matching PostgreSQL's order of magnitude.
- This is the practical difference between the two latest releases — it does not show up in steady-state `SELECT 1` or `COPY`, but it dominates any workload that drops/recreates large tables (test harnesses, ETL, migrations). It is why benchmark loops that `DROP TABLE` between iterations time out on 3.60.6 but not 3.60.7.

---

## Conclusions

- **Pick Nano for connection-dense, simple-query / point-lookup-light workloads** (agent/edge/embedded, high QPS on trivial queries): 2–3× PostgreSQL on `SELECT 1`.
- **Pick PostgreSQL for storage-heavy workloads** (indexed scans at high concurrency, bulk ingest): ~2× on indexed reads, ~20× on `COPY`.
- **Use Nano ≥ 3.60.7**, not 3.60.6, for anything that drops/recreates large tables — the `DROP`-fsync stall is fixed.

## Environment & method

- **Host:** 32-core Linux; load average ~4 (~13 %) during runs; no compiler jobs active.
- **PostgreSQL:** `postgres:18.4-bookworm` container, `--network host`, `shared_buffers=256MB`, `max_connections=200`, port 25433.
- **HeliosDB-Nano:** each version a **native host binary** — `heliosdb-nano start --auth trust --http-port 0 --data-dir <fresh>` on a dedicated port; release builds from `git tag v3.58.1 … v3.60.7` of the Nano source (3.31.0-dev from the prebuilt sprint image).
- **Workloads:** `SELECT 1`; indexed point-read over 50k rows; `COPY` of 10k/50k/100k-row CSVs; `DROP TABLE` of a 100k-row table. `pgbench -n -f <script> -c <c> -j min(c,8) -T 8`.

## Caveats

- Nano runs as a native binary; PostgreSQL in a container with host networking (negligible network overhead, identical across all comparisons). pgbench always runs containerized.
- Single 8 s run per cell; run-to-run noise ≈5 % — differences within that band (notably across the 3.60.x line) are not significant.
- `SELECT 1` and the point-read isolate the protocol/query and indexed-read paths respectively; they are not a full OLTP/OLAP mix.
- Shared host with other low/idle agent sessions; transient contention could nudge individual cells.

---

*Mirrored from the HeliosDB-Proxy repository (`benchmarks/`). Canonical copy + reproducible harness: https://github.com/HeliosDatabase/HeliosDB-Proxy/tree/main/benchmarks*
