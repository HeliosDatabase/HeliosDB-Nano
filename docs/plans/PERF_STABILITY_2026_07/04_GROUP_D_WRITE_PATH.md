# Group D — Write-path concurrency & durability

**Target:** T4 — durable-write concurrency + write-path stability liabilities found by analysis.
**Branch:** `perf/write-path-2026-07` (D1-D3) · **Risk:** low-medium per item · **Effort:** S-M each

**Headline analysis verdict (HEAD @ 68e814a):** group commit is ALREADY implemented and
measured (leader/follower, `src/storage/group_commit.rs:93-147`, ~5-6× at 16T durable) —
T4's original premise is stale. The real findings, ranked:

## D1 — Sequence refill: fsync outside the per-sequence mutex + default CACHE > 1
**Gain: ~15-30× on `nextval`-bound inserts (currently ~90 rows/s TOTAL at any concurrency).**

Diagnosis: serve fast path is lock-free CAS in a reserved block (`sequences.rs:425-457`),
but refill holds the per-sequence `Mutex` **across the group-commit fsync**
(`sequences.rs:462` → `persist_high_water:534` → `engine.rs:3363-3373`). Default `CACHE 1`
on every creation path (`catalog.rs:89`, `executor/mod.rs:4211`) ⇒ every nextval is a
refill ⇒ one *serialized* fsync per value — the mutex admits one caller at a time into
`wait_durable`, so the group committer can never batch a hot sequence. Bites even with
`durable_commit=false` (sequence fsync is correctly unconditional — the v3.60.0
no-duplicate-on-crash invariant, `engine.rs:3350-3362`).

Fix: under refill — reserve block + stage high-water record, **release mutex before**
`group_committer.wait_durable(...)`, publish `next/block_end` only after own fsync
returns, in reservation order (per-sequence publish queue or seqno check). Concurrent
refillers coalesce into cohort fsyncs. Independently: default CACHE→32 for
auto-vivified/CREATE SEQUENCE (PG precedent: WAL-logs every 32).
**Invariant preserved:** value served only after its high-water is durable.
**Gate:** `sequences_durable_tests.rs`, `sequence_durability_tests.rs`, wire-path psycopg
sequence tests (v3.60.0 memory rule), crash-recovery suites.

## D2 — Group-commit window default: 200 µs → 1000 µs (measured +10-25% at 24-32T)
`config.rs:483`, sleep at `group_commit.rs:116-120`. Measured (perf/r1_3_p2_runs):
w=1000 → 984-1718 txn/s vs w=200 → 890-1393 at 32T. Idle-WAL 1T cost: 108→90/s
(acceptable; consider adaptive EMA-of-fsync-latency cap later — not in this PR).
No durability semantics change. **Gate:** durable-commit bench A/B
(`HELIOS_DURABLE=1 … run_durable_commit_bench`, windows 200 vs 1000), crash suites.

## D3 — Shard the row cache + batch commit-time invalidation
Single global `RwLock<LruCache>` (`row_cache.rs:155-157`) write-locked per written row by
every committer (`transaction.rs:817-831`) — named lever for the R0.2 −31…−46%
write-cycle tax (`perf/R0_2_conflict_detection.md`). Reuse `src/sharded_lru.rs` pattern.
**Invariant:** per-shard invalidate-before-`end_commit` ordering (lost-update fence,
`transaction.rs:807-816`). **Gate:** row_cache tests, tps_workloads update-cycle A/B,
16-32T sweep.

## D4 — UPDATE versioning unification (CORRECTNESS — coordinate with Group C)
Three-way split today: fast autocommit UPDATE (`engine.rs:10797-10888`) writes **only
`data:`** — no version history, not gated on `time_travel_enabled` ⇒ **`AS OF` reads
after a fast UPDATE silently return the stale insert-time row**. Branch-aware UPDATE
(`engine.rs:12242-12335`): 4 un-batched puts (crash window), writes `v:` without `v_idx:`
(invisible to indexed snapshot reads; forces GC slow path `version_gc.rs:419-443`).
Fix: route both through `write_data_version_and_register_snapshot`
(`time_travel.rs:780-805`); drop the 2 redundant point-gets (`engine.rs:12286,12301`).
Expect small fast-UPDATE TPS cost; sanctioned fast mode = `time_travel_enabled=false`.
**Gate:** time-travel suites + crash-recovery + tps UPDATE A/B (document the delta).

## D5 — Bounded default for version retention (STABILITY — coordinate with Group C)
Factory default `version_retention: None` (`config.rs:480`) ⇒ GC worker never starts
(`engine.rs:3441`), `VACUUM VERSIONS` silently no-ops (`version_gc.rs:218-223`) ⇒
**unbounded on-disk `v:`/`v_idx:` growth by default** (every insert permanently ~doubles
row footprint; no compaction filter exists). Decision: ship default retention (e.g. 7d)
or at minimum make `VACUUM VERSIONS` error loudly + document. Raise
`max_versions_per_cycle`/interval so reclaim keeps up when enabled (~167 versions/s today).
Product-visible — changelog + docs.

## D6 — Same-row conflict handling (STABILITY — likely Group C ownership)
Pessimistic row lock in `Transaction::put` (`transaction.rs:448-454`) is redundant with
the optimistic commit-time registry (`transaction.rs:653-677`) and documented harmful
(`docs/NANO_CONCURRENCY_LOCKING.md:55-81`): waits are futile (holder can't commit until
waiter times out), 1 ms sleep-poll spin pins a tokio worker up to 1 s
(`lock_manager.rs:225-274`), no fairness (`:285-311`), DFS deadlock check per failed poll
(`:239,357-388`), O(all-locks) timeout cleanup (`:442-461`). Fix = doc's Option 2: drop
pessimistic acquisition for session-txn writes; keep for SELECT FOR UPDATE. Converts 1 s
stalls into immediate retriable serialization errors.
**Gate:** doc's prescribed no-stall test, one-winner/one-loser test, pg35, protocol suite.

## Deferred (not this campaign)
- R6 version-key format changes (`v_idx:` fold-in, `data:`/`v:` dedup) — on-disk format
  migration risk; prototype behind a flag later.
- R8 timestamp `RwLock`→atomic split — small win, subtle ordering contract with the
  conflict ring (`conflict.rs:114-119`).
- Adaptive group-commit window; `spawn_blocking` for durable waiters (small-host ceiling).

## Sequencing within the campaign
D1+D2+D3 = one perf PR (`perf/write-path-2026-07`). D4/D5/D6 are correctness/stability —
fold into Group C's hardening PR(s) after the stability audit lands, so all
behavior-visible changes ship together with their tests.

## Gate (campaign §Milestone gate) — additions specific to this group
- `run_durable_commit_bench` A/B at windows {200,1000}, 16/32T — expect ≥+10% at 32T.
- nextval-bound insert microbench (new, add to tps_workloads or a dedicated test):
  c=32 INSERTs with `DEFAULT nextval` — expect ≥15×.
- Update-cycle tps A/B for D3 — expect measurable recovery of the R0.2 tax; no read regression.
