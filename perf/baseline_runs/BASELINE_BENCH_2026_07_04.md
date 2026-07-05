# HeliosDB-Nano Baseline Bench — 2026-07-04

**Host:** 32-core Linux (shared host, several other agent sessions active — see Environment notes)
**Load average (before run, 02:27:37 UTC):** 5.67, 5.91, 5.89
**Load average (after run, 02:52:25 UTC):** 7.42, 14.40, 13.94
**Binary under test:** `heliosdb-nano 3.60.9` — commit `68e814a` ("fix(v3.60.9): Oracle-export compat — trailing comma, INTERVAL YEAR/MONTH, auto-recursive CTE")
**Binary path:** `/home/gpc/HDB/Nano/perf/baseline_runs/bins/heliosdb-nano-baseline-main-68e814a`
**PostgreSQL:** `PostgreSQL 18.4 (Debian 18.4-1.pgdg12+1)`, `postgres:18.4-bookworm` container, shared instance `codex-pg184-bench`, port 25433
**Harness:** `docs/benchmarks/bench-engines.sh`, defaults `DUR=8` s/cell, `CLIENTS="1 8 16 32 64"`
**Reference for comparison:** `docs/benchmarks/heliosdb-nano-vs-postgresql-2026-06-28.md` (Nano 3.60.7 column)

---

## Critical finding: `ALTER TABLE … RENAME TO …` hangs the server (3.60.9)

While building the indexed-read fixture (the harness renames a freshly-COPY'd `t50000` table to `t50` before indexing it), the statement

```sql
DROP TABLE IF EXISTS t50; ALTER TABLE t50000 RENAME TO t50;   -- t50000 has 50,000 rows
```

**did not complete after >15 minutes** and was still writing to disk (confirmed via `/proc/<pid>/io`, growing ~100-130 KB/s) when I gave up and killed the server. Notably:

- **Killing the client connection did not cancel the server-side work.** A subsequent trivial query (`\dt`) on a fresh connection also hung (2 min timeout), and a kernel thread-state check showed a Nano worker thread parked in `folio_wait_bit_common` (blocked on a page/IO wait) — i.e. the RENAME kept running server-side, holding what looks like a broad lock, well after its own client was gone. The server had to be `kill -9`'d; there was no clean recovery path.
- **This is isolated to `RENAME`, not DDL in general.** On a fresh server, building the same 50k-row table directly (skipping the rename) was fast and unremarkable: `CREATE TABLE` instant, `COPY 50,000 rows` = 1,317 ms, `CREATE INDEX i50 ON t50(aid)` = **390 ms**. `DROP TABLE` on a 100k-row table (below) is also fast (161 ms). Only the rename-of-an-existing-table path is broken.
- This looks like the same family of bug as the pre-3.60.7 `DROP TABLE` O(rows)-fsync stall (60,004 ms timeout on 3.60.6, fixed to 219 ms in 3.60.7) — except worse (no termination within 15+ min vs. a 60 s timeout) and apparently not cancellable. Recommend filing this as a release blocker / P0 for investigation; not something this benchmark run should attempt to fix.
- **Methodology impact:** because of this, Table 2 and Table 4's Nano numbers below were captured on a **second, fresh Nano server instance** with the `t50` fixture built directly via `CREATE TABLE` + `COPY` (bypassing `RENAME` entirely), not via the harness's default path. Everything else (SELECT 1, COPY timings, and all PostgreSQL numbers) came from the single unattended harness run.

---

## Table 1 — `SELECT 1` TPS by client count

| clients | PostgreSQL (TPS) | Nano-baseline 3.60.9 (TPS) |
|--------:|------------------:|---------------------------:|
|  1 |  7,877.52 |  21,613.78 |
|  8 | 42,237.57 |  95,490.45 |
| 16 | 55,730.29 | 132,472.07 |
| 32 | 72,590.91 | 156,054.76 |
| 64 | 98,267.88 | 146,092.94 |

## Table 2 — Indexed point-read TPS by client count (50k-row table, btree on `aid`)

| clients | PostgreSQL (TPS) | Nano-baseline 3.60.9 (TPS) |
|--------:|------------------:|---------------------------:|
|  1 |  5,935.11 |  7,494.78 |
|  8 | 33,265.74 | 24,544.98 |
| 16 | 49,093.19 | 36,665.13 |
| 32 | 60,311.59 | 33,783.63 |
| 64 | 76,681.39 | 51,080.46 |

*Nano column captured on the fresh (post-incident) server — see methodology note above. This sweep also landed partly inside a contention spike from a concurrent HeliosProxy `cargo build --release` on this shared host (load average briefly hit 33.96/32 cores at 02:49:47, right after this sweep) — see Environment notes.*

## Table 3 — Bulk-load `COPY` (ms, lower is better)

| rows | PostgreSQL (ms) | Nano-baseline 3.60.9 (ms) |
|-----:|------------------:|---------------------------:|
|  10,000 | 106 |   359 |
|  50,000 | 103 | 1,383 |
| 100,000 | 119 | 3,210 |

## Table 4 — `DROP TABLE` on a 100k-row table (ms)

| engine | DROP time (ms) |
|---|---:|
| PostgreSQL | 82 |
| Nano-baseline 3.60.9 | 161 |

*Nano number captured on the fresh (post-incident) server, immediately after the load spike above had started subsiding (load average 12.11 at test start) — see methodology note.*

---

## Reference-expectation deviation flags

Reference (2026-06-28 report, Nano 3.60.7 column / PostgreSQL column): SELECT-1 ≈26k/201k/245k TPS at c=1/16/64; indexed read ≈5k/44k/48k TPS at c=1/16/32; COPY@100k ≈2,361 ms; DROP@100k ≈219 ms.

| Metric | Cell | Expected (ref) | Observed | Δ | Flag (>15%)? |
|---|---|---:|---:|---:|:--:|
| SELECT 1 | c=1 | 26,374 | 21,614 | −18.1% | **yes** |
| SELECT 1 | c=8 | 117,852 | 95,490 | −19.0% | **yes** |
| SELECT 1 | c=16 | 201,620 | 132,472 | −34.3% | **yes** |
| SELECT 1 | c=32 | 216,450 | 156,055 | −27.9% | **yes** |
| SELECT 1 | c=64 | 245,346 | 146,093 | −40.5% | **yes** |
| Indexed read | c=1 | 4,931 | 7,495 | +52.0% | **yes** (higher, not a regression) |
| Indexed read | c=8 | 25,698 | 24,545 | −4.5% | no |
| Indexed read | c=16 | 42,449 | 36,665 | −13.6% | no (borderline) |
| Indexed read | c=32 | 49,158 | 33,784 | −31.3% | **yes** |
| Indexed read | c=64 | 47,477 | 51,080 | +7.6% | no |
| COPY @100k | — | 2,361 ms | 3,210 ms | +36.0% | **yes** |
| DROP @100k | — | 219 ms | 161 ms | −26.5% | **yes** (faster, not a regression) |

**Reading the flags — environmental vs. real:**

- **SELECT 1 is uniformly ~18–40% below reference at every client count, and this is very likely environmental, not a Nano regression.** PostgreSQL's own SELECT-1 numbers (same unchanged container image, measured in the same run) are *also* uniformly ~14–19% below **its own** 2026-06-28 numbers (e.g. c=1: 7,878 vs. 9,779 = −19.4%; c=64: 98,268 vs. 118,435 = −17.0%). Since PostgreSQL's engine didn't change between the two reports, a matched-direction, similar-magnitude drop on both engines points to a noisier host this session (5 tmux sessions with 8 total agent windows were active at the start, vs. the reference run's reported "no compiler jobs active"), not a code-level regression. The same logic applies to most of the indexed-read cells (PG is also down 12–24% across the board).
- **The indexed-read c=32 cell (−31.3%) is the one cell where Nano's drop (−31%) clearly exceeds PostgreSQL's drop at the same client count (−16.0%)** — this lines up with the contention spike (rustc build) that started right around/after this sweep; treat as noise from that specific incident rather than a durable finding, but a clean re-run of this one cell would be worth doing in isolation.
- **COPY@100k (+36% slower) is the one throughput number that does *not* fit the "environmental" explanation** — PostgreSQL's own COPY@100k barely moved (115 ms ref → 119 ms now, +3.5%), so the contention story doesn't account for Nano's COPY getting slower. This is a real (if second-order, ms-scale) signal worth keeping an eye on across the next few releases, separate from the RENAME finding above.
- **DROP TABLE (−26.5%, i.e. faster) and indexed-read c=1 (+52%, faster)** are "good" deviations — no action needed, just noting for completeness since both cross the 15% band.

---

## Environment notes

- Preflight (02:16:49 UTC) found a **live, unrelated benchmark already in progress**: a concurrent Claude Code session (HeliosDB-Proxy repo, `/tmp/perf-2026-07/...`) was running its own baseline regression + scalability sweep against the *same* shared PostgreSQL container (`codex-pg184-bench`, port 25433) — load average was 14.10 at that point. I did **not** stop/remove that container (it was actively serving another team's in-flight benchmark, not stale); instead I waited (~9 minutes) for it to finish before starting this run, to avoid contaminating both benchmarks with cross-traffic on the same PG instance. This is why "load average before" above is timestamped 02:27:37 rather than the initial preflight time.
- Mid-run, a second, separate contention event occurred: the same Proxy-repo session kicked off a `cargo build --release` (LTO, codegen-units=1) immediately after its benchmark finished, spiking host load average to 33.96/32 cores at 02:49:47 — right around when I was capturing the manual indexed-read sweep for Nano (see Table 2 note). This had subsided by the time of the DROP TABLE measurement.
- I did not start, stop, or modify anything belonging to the concurrent Proxy session or its containers; `codex-pg184-bench` was left running for shared reuse, as found.

## Cleanup performed

- Killed both Nano server instances I started (original harness-managed instance, PID wedged by the RENAME bug, and the fresh replacement instance used for Table 2/4) via `kill -9`; confirmed no `heliosdb-nano-baseline*` processes remain.
- Confirmed port 5460 free.
- Removed the harness's temp workdir (`/tmp/bench-engines.*`), which was left behind because the harness's own `EXIT` trap didn't fire (the script was force-killed after the RENAME hang).
- Left `codex-pg184-bench` (shared PostgreSQL container) running, untouched, as found.
- Raw captured stdout from the unattended portion of the run: `/tmp/claude-1001/-home-gpc-HDB-Nano/7a75c977-e8f1-4819-abdb-955443482aad/scratchpad/baseline-bench-stdout.log`.
