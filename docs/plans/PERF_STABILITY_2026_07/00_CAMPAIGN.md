# Perf & Stability Campaign — 2026-07

**Base:** `main` @ `68e814a` (v3.60.9) · **Host:** 32-core Linux, docker, native Nano binaries
**Driver docs:** `docs/benchmarks/heliosdb-nano-vs-postgresql-2026-06-28.md` (targets), `perf/SUMMARY.md` (prior work), `ISSUE-index-persistence-and-uuid-pointlookup.md` (open stability issues)

## Targets (from the 2026-06-28 head-to-head vs PostgreSQL 18.4)

| # | Dimension | Nano today | PG 18.4 | Goal |
|---|---|---|---|---|
| T1 | Indexed point-read, simple protocol | 5.3k @ c=1, **saturates ~48k @ c=32** | 6.8k @ c=1, 100k @ c=64 | Close the c=1 gap; raise the saturation ceiling toward PG |
| T2 | COPY bulk-load 100k rows | ~2.3 s (23 µs/row, linear) | 115 ms (flat) | ≥5× faster (≤ ~5 µs/row) without losing atomicity |
| T3 | Stability | open: index persistence across upgrades, panic surfaces, release-gate flakes | — | No known silent-degradation bugs; panic-free serving path |
| T4 | Durable-write concurrency | fsync per autocommit statement (TBC by analysis) | group commit | Group commit if analysis confirms the gap |

Groups are finalized after the 4-agent analysis fan-out (read-path, COPY, stability audit, write-path). One plan doc per group in this directory; groups land **sequentially**, each through the full gate below.

## Milestones (final, post-analysis 2026-07-04)

| M | Group / branch | Content | Headline expectation |
|---|---|---|---|
| M1 | C-I `fix/stability-wire-hardening` | Wire-message caps, malformed-msg panic fixes, TO_DATE panic, MySQL conn cap + stmt LRU, accept backoff, poisonable mutex, HNSW flake fix A + recall guard, UUID-probe regression tests (03_GROUP_C §C-I) | Perf-neutral; de-flakes release gate; kills pre-auth OOM/panic vectors |
| M2a ✅MERGED | A `perf/read-path-normalization` (PR #7) | Cache-admission churn stop (A1) + shared cold optimizer (A5) + parse guard | Indexed-read +9-35% @ c≥8/16 (2 trials); no erosion; pg35 35-0-0 |
| M2b | A2+A3+A4 (own branch) | Token-level literal normalization → param plan cache (A2, see 01a), row-cache cap/stats (A3), ART registry map (A4) | Indexed read c=1 ≥+50% & >PG, c=32 ≥+75% & >PG |
| M3 | B `perf/copy-bulk-load` | COPY → typed fast-batch path (B1), implicit-txn multi-row literal fast path (B2) (02_GROUP_B) | COPY 100k 2.3s→≤0.5s + PG-atomic semantics |
| M4 | D `perf/write-path-2026-07` | Sequence refill fsync-outside-mutex + CACHE default (D1), group-commit window 1000µs (D2), row-cache sharding + batched invalidation (D3) (04_GROUP_D) | nextval inserts ~90/s→≥1.5k/s; durable TPS +10-25% @ 32T |
| M5 | C-II `fix/stability-resource-governance` | Statement timeout (C11), CTE caps (C12), WAL prefix replay (C13), portal double-copy (C14), index-def version migration (C15), UPDATE versioning unify (D4), pessimistic-lock removal (D6), session-txn COPY (B4) | Bounded resources; no silent stale AS OF; no 1s conflict stalls |

Key analysis verdicts recorded for posterity: group commit already implemented+measured
(R1.3p2); v3.33 4-Mutex convoy REFUTED at HEAD (R2.1 sharded LRUs); read-path saturation =
malloc-arena churn from guaranteed-miss cache inserts; COPY never touches the fast-batch
machinery; ISSUE A partially fixed (cross-version index-def decode still drops per-index);
ISSUE B fixed but regression-untested; HNSW flake = hnsw_rs 0.3.3 layer-0 isolation of
first-inserted node (reproduced 1/800, 4/2000) — also a REAL production recall bug.

**DECISION items for the owner (not changed unilaterally):** `durable_commit=false`
default (ACKed commits lost on power cut) · `version_retention: None` default (unbounded
version growth, `VACUUM VERSIONS` no-ops). See 03_GROUP_C §DECISION.

## Milestone gate (every group, before PR merge)

Run on a **quiet machine** (no concurrent builds/agents). Offloaded to a Sonnet sub-agent.

**Regression** (must be clean vs baseline):
1. `cargo test --lib` — baseline: 1915 pass / 0 fail (known pre-existing exceptions listed in perf/SUMMARY.md).
2. Targeted integration suites for the touched area (e.g. crud, transaction, wal_crash_recovery, crash_recovery_e2e, copy/protocol tests).
3. Wire-path rule: any change touching protocol/, catalog, planner, or caches **must** run `tests/protocol_tests/run_tests.sh` (psycopg suite) — embedded tests miss `protocol/postgres/catalog.rs` paths.
4. `benches/public/ci_perf_smoke.sh` locally (same script as the PR perf-gate CI) — no workload >2.5× slower than `benches/public/ci_baseline.json`.

**Scalability** (must not erode, target must move):
5. `docs/benchmarks/bench-engines.sh baseline:<baseline-bin> <group>:<new-bin>` — SELECT 1 sweep c∈{1,8,16,32,64}, indexed-read sweep, COPY 10k/50k/100k, DROP 100k. Baseline binary: `perf/baseline_runs/bins/heliosdb-nano-baseline-main-68e814a`.
6. Compare vs baseline: the group's target metric must improve by its plan's stated minimum; all other metrics within noise (±5%).

**Merge**: branch `perf/<group>` or `fix/<group>` → PR with gate results in the body → merge to main (repo uses merge commits, no branch protection). CI perf-gate + release-gate flakes (HNSW/vector tests, dep downloads) are known — rerun failed jobs once before treating as real.

## Campaign complete (2026-07-04) — all 5 milestones merged

| M | PR | Result |
|---|---|---|
| M1 stability wire hardening | #6 | pre-auth OOM/panic vectors closed; HNSW flake→recall fix; RENAME wedge+torn-table+WAL-resurrection fix |
| M2a read-path churn stop | #7 | indexed-read +9-35% @ c≥8/16; no erosion; pg35 35-0-0 |
| M3 COPY typed bulk path | #8 | COPY 100k 2.47s→458ms (**5.4×**), atomic; +B4 session-txn COPY |
| M4 write-path | #9 | nextval inserts **32.3×** (D1); durable +11-63% @16-32T (D2); sharded row cache (D3) |
| M5 resource governance | #10 | statement timeout enforced (C11); CTE caps + shadow fix (C12); WAL torn-tail (C13) |
| M2b A2 literal normalization | #11 | **indexed point-read +104% @c=1 (2.14× PG), +157% @c=32 (2.26× PG)** — reverses the pre-campaign vs-PG deficit at every concurrency; differential oracle + pg35 35-0-0 |

Deferred (documented, ready to pick up): C11-wire per-connection SET,
C14 portal streaming, C15 index-def version migration, D4 UPDATE versioning unify, D6
pessimistic-lock removal. Two product DECISION items unchanged: `durable_commit=false` default,
`version_retention: None` default (§Group C/D DECISION).

## Status log

- 2026-07-04: campaign started; 4 analysis agents launched; baseline binary building.
- 2026-07-04: all 4 analysis reports in; plan docs A-D written; milestones M1-M5 fixed.
- 2026-07-04: **M1 MERGED** (PR #6, f67af88). Regression gate: lib 1963/0, HNSW loop
  300/300, integration 146/0, smoke 12/12; protocol failures proven pre-existing
  (C17 filed). Scalability gate: zero cells below baseline beyond ±5%; RENAME
  verified end-to-end (baseline: 2/2 hangs + torn split-table on kill; M1: 1.3s,
  atomic, replay-correct). Bonus findings fixed in M1: RENAME torn-table hazard,
  RENAME WAL-replay resurrection, HNSW production recall bug.
- 2026-07-04: **M2a MERGED** (PR #7, eef73d7). Read-path churn stop. Regression lib
  1963/0; scalability PASS — indexed-read (target) +35% @c=16 trial 1, +9-18% @c≥8
  paired retest; all apparent SELECT1/COPY/DROP regressions reversed on order-swapped
  retest (host noise; `cache_admits` unreachable on cache hits, confirmed at source);
  pg35 35-0-0. GATE LESSON: one `bench-engines.sh` run is noise-unreliable on this
  host — paired/order-swapped retest required before calling any cell eroded.
- 2026-07-04: M1 (C-I) implemented on `fix/stability-wire-hardening`: C1-C10 complete
  (wire caps, checked parsers, TO_DATE panic, parking_lot txn mutex, MySQL conn cap +
  stmt cap, accept backoff, HNSW flake reorder + brute-force recall rescue ×3 metrics,
  EXPLAIN IndexPointLookup annotation + Bytes→Uuid coercion) + new test files
  (messages.rs malformed-frame units, tests/wire_hardening_tests.rs,
  tests/uuid_index_probe_explain.rs, HNSW rescue/tombstone units). Pending: compile+gate.
