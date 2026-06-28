# Nano concurrency & write-locking — findings, Option 1 (shipped), Option 2 (planned)

Status: Option 1 shipped in v3.60.7. Option 2 is the recommended follow-up.

Context: found via Any2HeliosDB (a2h) Pagila/Oracle → Nano on 3.60.4. Reported as
BUG A (concurrent write txns block instead of erroring) and BUG B (DROP of an
FK-linked table hangs and wedges the whole server).

## What actually happens

Nano runs **two** write-concurrency mechanisms at once:

1. **Pessimistic** `lock_manager` (`src/storage/lock_manager.rs`): row-level
   Read/Write locks acquired in `Transaction::put()` / read path. `acquire_lock`
   is a **synchronous spin** (1 ms `clock_nanosleep` retry loop) bounded by a
   single timeout (was 60 s).
2. **Optimistic** first-committer-wins `conflict_registry`
   (`WriteConflictRegistry::validate_and_record`, called from
   `Transaction::commit_with_timestamp`): detects write-write conflicts at
   COMMIT and returns a clean `serialization failure: … retry` error — **no
   spin, no blocking.**

The pessimistic layer is **redundant** (the optimistic layer already provides
correct conflict detection) and **actively harmful**:

- A same-row write conflict makes the second writer spin in `acquire_lock`.
  Because the spin is synchronous and holds engine resources, **the whole server
  stalls** for the duration: independent statements and even new-connection
  startup block. Verified empirically (gdb thread dump: the only active engine
  thread is the one spinning in `acquire_lock`; 15 idle tokio workers; new
  connections time out).
- The lock *holder's* own COMMIT cannot make progress until the waiter gives up.
  So for a write-write conflict **waiting is futile**: the wait can only ever end
  in a timeout, never in the lock being granted. (Repro: holder's `COMMIT`
  returned in 59.0 s — exactly when the waiter hit the 60 s timeout.)
- Non-conflicting concurrent writes (different rows) are unaffected — they never
  contend a lock, so this never triggers under a well-sharded parallel load.

BUG B (DROP wedge) is the same mechanism surfacing on the DDL/large-delete path
plus a separate correctness gap (see below).

## Option 1 — bound the spin (SHIPPED, v3.60.7)

`LockManager::with_default_timeout()` now honors `NANO_LOCK_TIMEOUT_MS` and
defaults to **1000 ms** (was 60 000 ms). A write-write conflict now fails fast
with a retriable lock-timeout/serialization error instead of stalling the server
for up to a minute. This is a mitigation, not a cure: the server can still stall
for up to `NANO_LOCK_TIMEOUT_MS` per conflict, because the underlying spin and
holder-commit interaction are unchanged.

Tuning: set `NANO_LOCK_TIMEOUT_MS` lower for latency-sensitive deployments,
higher for batch jobs that tolerate longer waits (at the cost of a longer
worst-case stall per conflict).

## Option 2 — drop the pessimistic write lock (RECOMMENDED, planned)

Remove the redundant pessimistic row-lock from the session-transaction write
path and rely solely on the optimistic `conflict_registry`:

- In `Transaction::put()` (`src/storage/transaction.rs`), stop calling
  `lock_manager.acquire_lock(.., Write)` for session transactions (gate behind a
  flag, or make `lock_manager` `None` on that path). Reads already MVCC-snapshot;
  writes already stage into the `write_set` and are validated at commit.
- `commit_with_timestamp` already calls `validate_and_record`, which aborts the
  *second* committer with `serialization failure` — first-committer-wins, no
  spin, no holder-commit stall, no server-wide wedge.
- Net effect: concurrent same-row writers both proceed optimistically; whoever
  commits first wins; the loser gets an immediate, retriable serialization error
  at its own COMMIT. This is standard SI/SSI behavior and is exactly what a2h
  asked for ("no serialize fallback needed" for non-conflicting parallel load;
  clean retriable error on genuine conflict).

Risks / to verify before shipping Option 2:
- Confirm nothing depends on read-lock blocking semantics (e.g. SELECT FOR
  UPDATE — currently also via `acquire_lock`). FOR UPDATE may still want a
  bounded lock; keep the pessimistic path available behind an explicit request.
- Deadlock detection (`detect_deadlock`) becomes unnecessary on the optimistic
  path; keep it only for any retained pessimistic uses.
- Re-run the full pg35 + tps + protocol suites; add a same-row-conflict test
  asserting (a) no server-wide stall and (b) one committer wins, the other gets
  a serialization error immediately.

## BUG B — additional correctness gap (separate from the lock wedge)

Independently of the wedge, Nano currently **allows `DROP TABLE` of a table that
is still referenced by another table's FOREIGN KEY** (PostgreSQL rejects this
without `CASCADE`). Dropping the referenced parent leaves the child with a
dangling FK; a subsequent `DROP` of the child then misbehaves. Fixes to scope:

- Reject `DROP TABLE parent` when an FK references it, unless `CASCADE` (then
  drop/Disable the dependent constraints first).
- Ensure `catalog.drop_table()` cleans up FK constraint metadata
  (`table_constraints:` key) so no dangling constraint survives a drop.

Once Option 2 (or the bounded Option 1) removes the 60 s stall, the DROP path
should fail fast / behave correctly rather than wedging the server.
