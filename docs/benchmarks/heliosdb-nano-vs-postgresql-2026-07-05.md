# HeliosDB-Nano vs PostgreSQL — Scalability & Performance (v4.0.0)

**Date:** 2026-07-05
**Engines:** PostgreSQL 18.4 vs HeliosDB-Nano **v4.0.0** (baseline reference: `3.60.9`)
**Client:** `pgbench` (PostgreSQL 18.4 image), concurrency sweep c ∈ {1, 8, 16, 32, 64}.
**Method:** paired, order-swapped runs on the same 32-core host; ratios within a run are the signal (see *Methodology*).

Supersedes [`heliosdb-nano-vs-postgresql-2026-06-28.md`](heliosdb-nano-vs-postgresql-2026-06-28.md). The numbers below reflect the 2026-07 perf+stability campaign (`docs/plans/PERF_STABILITY_2026_07/`), shipped in **v4.0.0**.

---

## TL;DR — what changed since 3.60.x

| Dimension | 3.60.x (2026-06-28) | **v4.0.0** | vs PostgreSQL 18.4 |
|---|---|---|---|
| Indexed point-read | **PostgreSQL won** ~1.3–2.1× (Nano saturated ~48k @ c=32) | **HeliosDB-Nano wins** — reversed at every concurrency | **1.7×–2.3× faster** |
| `SELECT 1` (protocol) | Nano 2–3× | unchanged | **~2.5× faster** |
| COPY bulk-load, 100k rows | ~2.3 s (PG ~20× faster) | **423 ms** (5.4× faster than 3.60.x) | ~3× (10k: parity/faster) |
| `nextval`-bound INSERT | ~60 TPS (fsync-serialized) | **~2,000 TPS** (32×) | — |
| Durable-write TPS @16-32T | baseline | **+11–63%** | — |

**The headline:** the indexed-read workload — the one PostgreSQL used to win — is **reversed**. On the pgbench point-read (`SELECT abalance FROM t WHERE aid = :rand` over a 50k-row indexed table), 3.60.x *lost* to PostgreSQL 0.80–0.92× at c≥8 and saturated near 48k TPS. **v4.0.0 leads PostgreSQL 1.73×–2.26× at every concurrency**, up to ~172k TPS. The lever was literal normalization: repeated point reads that differ only in their literals now share one cached parameterized plan instead of re-parsing and re-planning every statement.

---

## 1. Indexed point-read — storage + query path (TPS, higher is better)

`SELECT abalance FROM t WHERE aid = :rand` over a 50k-row table, btree index on `aid`, pgbench simple protocol (fresh literal every statement). Paired, order-swapped, 2 rounds.

| clients | PostgreSQL 18.4 | Nano 3.60.9 | **Nano v4.0.0** | v4.0.0 / PG | v4.0.0 / 3.60.9 |
|--------:|----------------:|------------:|----------------:|:-----------:|:---------------:|
|  1 |   6,775 |  7,090 |  **14,496** | **2.14×** | +104% |
|  8 |  40,609 | 32,339 |  **77,597** | 1.91× | +140% |
| 16 |  61,154 | 56,549 | **136,976** | 2.24× | +142% |
| 32 |  74,518 | 65,604 | **168,643** | **2.26×** | +157% |
| 64 |  99,631 | 66,276 | **172,075** | 1.73× | +160% |

- **This reverses the previous result.** In the 2026-06-28 report PostgreSQL won this workload (~1.3× @ c=1 widening to ~2.1× @ c=64) and Nano saturated ~48k TPS. v4.0.0 now leads at every concurrency and scales past PostgreSQL.
- **A2 literal normalization is the dominant lever.** A kill-switch A/B (`NANO_DISABLE_QUERY_NORMALIZATION=1`) isolates it: normalization roughly **doubles** point-read throughput on its own; the read-path churn-stop (M2a) contributes the remaining ~13%.
- Effect confirmed across order-swapped rounds within ~2–3 percentage points — not a host-noise artifact.

## 2. `SELECT 1` — protocol & connection scalability (TPS, higher is better)

Unchanged by the campaign (normalization does not touch this path); reproduced here as no-erosion context.

| clients | PostgreSQL 18.4 | **Nano v4.0.0** | v4.0.0 / PG |
|--------:|----------------:|----------------:|:-----------:|
|  1 |   ~9,600 |  ~26,000 | ~2.7× |
| 64 | ~100,000 | ~245,000 | ~2.5× |

Nano remains **~2.5× PostgreSQL** across the sweep — its protocol/connection path was already ahead and is untouched.

## 3. Bulk-load — `COPY` from CSV (lower ms is better)

| rows | PostgreSQL 18.4 | Nano 3.60.9 | **Nano v4.0.0** | v4.0.0 / 3.60.9 |
|-----:|----------------:|------------:|----------------:|:---------------:|
|  10,000 |  82 ms |   ~260 ms |  **76 ms** | 3.4× faster |
|  50,000 | 106 ms | ~1,250 ms |  **227 ms** | 5.5× faster |
| 100,000 | 133 ms | ~2,550 ms |  **423 ms** | 6.0× faster |

- **The COPY gap to PostgreSQL closed from ~20× to ~3×** (and at 10k rows, v4.0.0 is at parity / slightly faster). COPY now applies the whole load as one atomic batch through the fast insert-batch machinery instead of re-rendering each 500-row chunk to a ~25 KB SQL string and re-parsing it.
- COPY is now **all-or-nothing atomic** (a constraint failure or crash mid-COPY leaves zero rows) and participates in an enclosing transaction (`BEGIN; COPY; ROLLBACK` no longer leaks rows).

## 4. `nextval`-bound INSERT and durable writes

- **Sequence-driven inserts: ~60 TPS → ~2,000 TPS (32×).** A default sequence previously fsync'd its durable high-water on *every* `nextval` (a hard ceiling near one value per fsync), independent of `durable_commit`. v4.0.0 reserves a block of 32 per fsync — matching PostgreSQL's own `SEQ_LOG_VALS=32` durability granularity — so a table with a `DEFAULT nextval('seq')` column loads at storage speed instead of fsync speed.
- **Durable-write throughput +11–63% at 16–32 threads** (group-commit accumulation window tuned 200 µs → 1000 µs), with *lower* p50 commit latency — concurrent durable committers coalesce into fewer, larger fsync cohorts.

## 5. `DROP TABLE` on 100k rows (ms, lower is better)

| engine | DROP time |
|---|---|
| PostgreSQL 18.4 | ~70 ms |
| **Nano v4.0.0** | **~135 ms** |

Comparable order of magnitude (PostgreSQL ~2× faster in absolute ms). The catastrophic >60 s O(rows)-fsync stall of the 3.60.6 line was fixed in 3.60.7 and remains fixed; v4.0.0 additionally fixed a **non-cancellable `ALTER TABLE … RENAME` server-wedge** (15+ min hang that left a torn split-table on kill) — RENAME of a 50k-row table now completes in ~1.3 s, atomically.

---

## What shipped in v4.0.0 (2026-07 campaign)

- **Read path:** cache-admission filter that stops unique-literal queries from churning the plan/result caches (M2a), then **token-level literal normalization** so repeated point reads share one cached parameterized plan (M2b) — together the ~2× indexed-read win. Correctness proven by a differential oracle (raw-SQL execution == normalized+parameterized execution, row-for-row) and the pg35 benchmark-of-record (35 categories, still 35–0–0 vs PostgreSQL).
- **COPY:** typed atomic bulk path (M3) — 5.4× faster, PG-matching all-or-nothing semantics.
- **Write path:** durable sequence blocks (32×), group-commit window tuning, sharded row cache (M4).
- **Stability:** wire-message size caps (closes a pre-auth remote-OOM vector), malformed-frame panic fixes, a real HNSW recall-bug fix, `ALTER TABLE RENAME` wedge + torn-table + WAL-replay fixes, statement-timeout enforcement, recursive-CTE resource caps, WAL torn-tail tolerance (M1, M5).

## Behavior changes (why this is a major version)

- COPY is now atomic (all-or-nothing) rather than committing per 500-row chunk.
- Default sequences reserve blocks of 32 — a crash/restart may skip forward by up to 31 (never backward, never reuse), matching PostgreSQL's durability granularity. Set `CACHE 1` for the old gapless-per-value behavior.
- A deterministic SELECT warms its plan/result caches on its *second* execution, not its first (admission filter).
- `SET statement_timeout` / configured `statement_timeout_ms` is now enforced (was previously accepted but ignored).
- `group_commit_window_us` default raised 200 → 1000 (affects `durable_commit=true` only).

## Environment & method

- **Host:** 32-core Linux, 125 GiB RAM; shared/multi-tenant during runs (load average swung 2–14). PostgreSQL `18.4-bookworm` container, host networking.
- **HeliosDB-Nano:** native host binaries built from `git tag`; `heliosdb-nano start --auth trust --data-dir <fresh>` per version on dedicated ports.
- **Workloads:** `SELECT 1`; indexed point-read over 50k rows; `COPY` of 10k/50k/100k CSVs; `DROP TABLE` of 100k rows; `nextval`-bound INSERT; durable-commit microbench.

## Methodology & caveats

- **Paired, order-swapped measurement.** This host is noisy — PostgreSQL's own numbers drift 5–40 % between runs and a single `bench-engines.sh` invocation can show apparent regressions that reverse under an order-swapped retest. Every headline figure here was measured baseline-vs-v4.0.0 back-to-back and repeated with the order swapped; the ~2× indexed-read effect is far larger than the host's noise band and held within a few percent across rounds.
- **Ratios within a run, not absolute numbers across runs**, are the signal.
- The v4.0.0 indexed-read figure is the *cumulative* read-path improvement (M2a churn-stop + A2 normalization); the A2-only contribution is isolated in §1 via the kill switch.
- `pgbench` always runs containerized; Nano runs native. Network overhead is negligible and identical across comparisons.
