# Opus INLJ Correctness Fixes + Perf Wins — Validation Handoff

Date: 2026-06-13 UTC. Author: Opus 4.8 (orchestrator). Workspace: `/home/gpc/HDB/Nano-r01` ONLY.
Do NOT touch `/home/gpc/HDB/Nano` (protected ISSUE-08 stash workflow).

## Why this exists
Opus code review of the uncommitted perf-tuning diff found **2 HIGH-severity
correctness regressions** in the Codex-added indexed-nested-loop join (INLJ),
plus implemented 3 safe perf wins. The current source was NOT shippable before
these fixes. After applying them, the source must be rebuilt + revalidated +
re-benchmarked (the pre-fix PG35 / A/B results are stale).

## Source changes applied by Opus (uncommitted, in working tree)

### Correctness (MUST verify)
1. **BUG-1 type-equivalence guard** — `src/sql/executor/join.rs`,
   `try_index_nested_loop_join`. INLJ encodes the LEFT key value and probes the
   RIGHT column index; `encode_key` is type-width sensitive (Int2=2B/Int4=4B/
   Int8=8B/...). A cross-type equi-join (`int4_fk = int8_pk`, `text = int`,
   `text = uuid`) built a mismatched-width key and **silently dropped all
   matches**. Fix: capture the right join column's declared type in Phase 1
   (`right_join_col_type`); in Phase 2, after resolving `left_key_idx`, bail to
   the hash/NLJ path unless `left_col.data_type == right_col.data_type`.
2. **BUG-2 visibility guards** — same function. INLJ read committed ART/storage
   directly with no branch/txn guard. Fixes: (a) `if storage.is_branch_active()
   { return Ok(None) }` in Phase 1; (b) `if
   executor.txn_forces_slow_reads_for_table(&right_table) { return Ok(None) }`
   after Phase 1 (bails on active-txn staged writes / stale snapshot).

### Perf wins — REVERTED (per user directive: do not remove code, it may be Fable 5)
Two perf micro-opts were briefly applied then REVERTED:
- `HELIOS_INLJ_OFF` → `OnceLock<bool>` in `should_try_index_nested_loop_join`:
  reverted to the original `env::var` read. (Negligible benefit; not worth
  touching code right before ship.)
- IN-list COUNT fast path (`src/lib.rs` `fast_count_pk_in_predicate`): the
  "improvement" of routing through `pk_index_count_keys` is the SAME batched-
  helper experiment Fable 5 already tried and **rejected** — it REGRESSED small
  `count_pk(id IN)` probes ~-3.9% (`perf/v337_vs_latest/focused_scan_art_batch_*`),
  and the active code was deliberately changed back to per-key
  `pk_index_contains`. Reverted to the original Fable 5 loop; added an inline
  NOTE comment so this is not re-attempted without an A/B. **Net change in
  lib.rs = one comment, zero logic change.**

So the ONLY behavioral changes vs the pre-review source are the 3 additive INLJ
correctness guards above. They delete nothing — they gate INLJ to fall back to
the (already-present, correct) hash/NLJ path in the unsafe cases.

Deferred ideas (NOT applied — would require A/B, post-ship): engine.rs
`tuple.clone()` → `Arc<Tuple>`; integer-filter scan-method dedup (drift hazard,
not a bug); INLJ streaming operator instead of full materialization.

Big rocks verified UNTOUCHED (rustfmt only): R3.4 typed_kernels/typed_batch
(SIMD codegen intact), R4.3 version_gc, R1.3-p2 group_commit/conflict (conflict.rs
has zero uncommitted change), R4.x art_manager/art_index/index_snapshot.

## VALIDATION STEPS (Codex — run in /home/gpc/HDB/Nano-r01, quiet machine)

1. Build: `cargo build --release --tests 2>&1 | tail -20` — must be clean
   (pre-existing warnings: phase3.rs noop clones, main.rs child_dead are OK).
2. Targeted correctness suites (these exercise joins + txn visibility):
   `cargo test --release --test transaction_integration_tests -- --test-threads=1`
   `cargo test --release --test crud_tests -- --test-threads=1`
   plus any join suite: `ls tests/ | grep -iE 'join|inlj'` then run them.
3. **Cross-type INLJ correctness smoke** (the exact BUG-1 scenario). Create two
   tables where the FK type differs from the PK type, e.g. left `fk INT4`, right
   `id INT8 PRIMARY KEY` with an index on the join column; insert matching rows;
   confirm `SELECT ... FROM left JOIN right ON left.fk = right.id` returns the
   SAME rows with INLJ default-on as with `HELIOS_INLJ_OFF=1`. Row counts MUST
   match. (If no quick test exists, a short inline test or psql check is fine.)
4. Lib regression: `cargo test --release --lib -- --test-threads=8 2>&1 | tail -3`
   (expect ~1896 passed, 0 failed).
5. Rebuild bench bins and rerun ONE A/B + PG35 round on the FIXED source:
   - `cargo test --release --test tps_workloads --no-run`
   - `ORDER=AB bash /tmp/v337_compare.sh opusfix_r1`
   - `ORDER=BA bash /tmp/v337_compare.sh opusfix_r2`
   - PG35 (container codex-pg184-bench on port 25433): rebuild
     `cargo test --release --test pg35_benchmark --no-run`, run 2 rounds.
   NOTE: the 4-table-join PG35 number may move toward the hash-join time IF that
   benchmark's join keys are cross-type (then INLJ now correctly falls back).
   Same-type joins keep the INLJ speedup. Either way correctness wins; report the
   delta so Opus can make the ship call.
6. Report back to Opus: build status, suite pass/fail, the cross-type row-count
   match result, and the A/B + PG35 scoreboards. Do NOT tag/push until Opus
   confirms ship.
