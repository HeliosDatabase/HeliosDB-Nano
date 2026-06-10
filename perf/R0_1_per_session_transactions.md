# R0.1 — Per-session transactions for wire handlers

**Branch:** `perf/r0-1-session-txns` (worktree `/home/gpc/HDB/Nano-r01`, off `98a868a` v3.37.3)
**Date:** 2026-06-10 · 32-core EPYC · `--profile perf` · Baseline: `perf/BASELINE_2026_06_10.md`
**Raw runs:** `perf/baseline_runs/` (before) vs `perf/r0_1_runs/` (after; runs 3/4 are final)

## What changed

1. **PG and MySQL wire handlers now run on per-session transactions.** Every
   connection creates a session (`create_wire_session`, quota-exempt — the
   server's `max_connections` semaphore bounds connections); BEGIN/COMMIT/
   ROLLBACK and all in-transaction statements route through the session API;
   autocommit statements keep the full `execute()`/`query*()` fast paths. New
   entry points: `execute_for_session`, `query_with_columns_for_session`,
   `execute_returning_for_session`, `execute_params_for_session`,
   `query_params_for_session`, `handle_transaction_control_for_session`.
2. **Cross-connection transaction bleed is fixed**: wire statements no longer
   join the process-global `current_transaction`. The global slot remains only
   for the embedded `begin()/commit()` API (unchanged semantics).
3. **Wire in-transaction SELECT gains read-your-writes** — both simple-query
   (`query_with_columns` never attached the transaction to the executor; the
   session variant does) and extended-protocol params paths
   (`session_txn` threaded through `execute_plan_with_params` /
   `query_plan_with_params`).
4. **Per-session ART undo logs.** Session transactions mutate ART indexes
   eagerly but logged undo entries into the *global* `art_undo_log`
   (pre-existing latent bug): a session rollback left phantom index entries,
   and an unrelated global ROLLBACK could un-index committed session rows.
   Undo entries are now routed by `Transaction::session_id()`; session
   COMMIT/ROLLBACK/destroy clear or replay exactly their own log.
   `destroy_session` also rolls back any open transaction (dropped
   connections leak nothing); both handlers do this from `Drop`.
5. **TT-aware INSERT fast-path gate.** Autocommit INSERT fast paths (and the
   literal in-transaction fast insert) now stay enabled while session
   transactions are open whenever time-travel versioning is on — fast inserts
   write MVCC versions, so versioned snapshot reads filter them correctly
   (regression-tested). UPDATE/DELETE fast paths stay strictly gated (they
   write no version history — roadmap C15).
6. **Atomic session-transaction counter.** The fast-path gates called
   `DashMap::is_empty()` per statement — a sweep of ~128 shard locks (~1.5µs)
   on this host, *already present at baseline*. All gates now read one
   `AtomicUsize`. This is why several baseline numbers improved well beyond
   the R0.1 targets.

## Direct categories (session-transaction bench)

| Workload | Baseline | After | Δ |
|---|---:|---:|---|
| global_txn_cycle(1T) | 48,335 txn/s | 50,324 txn/s | +4% |
| session_txn_cycle(1T) | 18,364 txn/s | **45,601 txn/s** | **+148%** (target ≥40k ✓) |
| session_txn_cycle(4T) | 36,155 txn/s | **90,261 txn/s** | **+150%** |
| session_txn_cycle(16T) | 49,390 txn/s | **161,770 txn/s** | **+228%** (3.5× its 1T rate; target ≥2× ✓) |
| session_autocommit_insert(1T) | 42,904 ops/s | 49,627 ops/s | +16% |
| autocommit_insert_with_open_session_txn | 12,908 ops/s | **123,352 ops/s** | **+856%** (target ≥80k ✓) |

Wire transactions previously could not run concurrently at all (second BEGIN
errored or folded into the foreign transaction); 16 isolated concurrent wire
transactions at 161k cycle/s is a new capability, not just a speedup.

## Indirect categories (regression gate)

Embedded mem TPS (mean of runs 3-4 vs baseline mean):

| Workload | Baseline | After | Δ |
|---|---:|---:|---|
| bulk_insert_users(txn) | 132,185/s | **170,661/s** | **+29%** |
| autocommit_insert | 99,407/s | **128,865/s** | **+30%** |
| update_by_pk | 111,049/s | **185,763/s** | **+67%** |
| delete_by_pk | 134,766/s | **285,530/s** | **+112%** |
| point_lookup_pk | 295,861/s | 293,220/s | noise |
| point_lookup_hot | 1,367,295/s | 1,365,695/s | noise |
| filter/agg/group/join/topN | 189/453/240/90/164 | 183/408/218/88/161 | within ±10% noise |

Disk: bulk_insert 126k→160k (+27%), autocommit_insert 46.4k→49.5k (+7%),
update 61.8k→80.7k (+31%), delete 94.7k→139.5k (+47%), reads/analytics noise.
Cache-concurrency bench: within noise (1T 929k / 4T 906k / 16T 964k vs
baseline 989k/757k/901k).

The DML gains are the atomic-counter fix (item 6): the gates on every
autocommit INSERT/UPDATE/DELETE and every in-transaction statement paid one
or more DashMap shard sweeps per statement at baseline. An intermediate
version of this branch that added a *second* sweep measured bulk_insert at
−21% — recorded here as evidence the protocol catches what it should
(`perf/r0_1_runs/mem_run1.txt`/`mem_run2.txt`).

## Correctness validation

- New `tests/session_wire_transactions.rs` (7 tests): cross-session bleed,
  read-your-writes (simple + extended params), per-session ART undo isolation
  (incl. committed-rows-survive-unrelated-global-rollback), 16-thread
  concurrent commit correctness, orphaned-connection rollback,
  RepeatableRead snapshot vs concurrent autocommit fast insert (guards the
  TT-aware gate).
- Full `--lib` suite: **1,822 passed / 0 failed**. Integration: transaction
  (28), transaction_integration (35), savepoint_hardening (60), crud (30),
  integration_v3 (33), drizzle_compat (47), PG wire a14/a15 (9),
  postgres_extended_protocol (16), protocol (15+0), network_protocol (2) —
  all green.

## Known limitations / follow-ups

- The embedded global slot still exists (`begin()`/`commit()`); deleting it
  (R0.1b) follows once embedded users have a migration path. In server mode
  it is now unused — REST/MCP/REPL are autocommit-only or single-session.
- SAVEPOINT inside a *session* transaction is rejected (was global-only
  before; PG handler surfaces the error). Scope for a follow-up.
- The extended-protocol params INSERT plan arm writes directly to storage
  (not through the transaction write set) for complex shapes — pre-existing
  for the global path, inherited by sessions (simple shapes use the staged
  fast path). Tracked as a C2-adjacent discovery for R0.2.
- Default-path write-write conflict detection (R0.2) is unchanged by this
  item: session transactions use the LockManager via the session API's
  isolation levels, the embedded global path still does last-writer-wins.
- MySQL COM_STMT_EXECUTE still discards bound parameters (roadmap C6, R5.W4).
