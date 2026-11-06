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

## The per-table writer lock (R5.1)

Separate from the row-level `LockManager` above, and what writers see during DDL. Spec:
`docs/plans/specs/R5_1_writer_lock.md`.

- A statement or transaction that writes a table holds it **shared**, from before it reads the rows it will
  change until the statement ends (autocommit) or the transaction does (`BEGIN`). Writers do not block each other.
- `ALTER TABLE` (every form), `DROP TABLE`, `RENAME`, `TRUNCATE`, `CREATE INDEX`, `DROP INDEX`, `CREATE`/`DROP TRIGGER`,
  `REFRESH MATERIALIZED VIEW` of the view's data table, and `MERGE BRANCH` on every table it changes hold it
  **exclusively**. Foreign-key DDL also holds the table on the other end.
- **Readers never wait.** `SELECT` takes nothing.
- **Writers wait.** During a long `ALTER TABLE … ADD COLUMN … DEFAULT` or a merge, a writer of that table waits and
  then runs against what the statement left, with no error. Other tables, and the same table on another branch, are not
  affected.
- **DDL waits for the writers already there**, including a transaction left open, and new writers wait behind it, so a
  statement cannot starve under a write load. An idle transaction that wrote the table blocks an `ALTER TABLE` until it ends.
- **`[session] lock_timeout_ms`** (default 30000) bounds every wait, and **`SET lock_timeout`** does for a session (`SET`,
  `SET LOCAL`, `RESET`, `SHOW`; a bare number is milliseconds, or `'2s'`). After it the statement fails with SQLSTATE
  **55P03** (MySQL wire: 1205 `ER_LOCK_WAIT_TIMEOUT`, REST: 409) naming what holds the table. It changed nothing; retry it.
  In a transaction it aborts the transaction like any other failed statement. **`SET lock_timeout = 0` is not PostgreSQL's
  unbounded wait**: it means the server's `lock_timeout_ms`, so a `pg_dump` script, which starts with it, still ends. A wait
  here is always bounded.
- **A wait also ends at `statement_timeout`, and at a cancel** (PostgreSQL's CancelRequest, which psql's Ctrl-C and every
  driver's cancel send): SQLSTATE **57014**, and the statement does not run when the lock comes free. It looks every 50 ms.
  Each wait counts its `statement_timeout` from where it starts, so a statement that waits for several tables can overrun
  it by that. A cancel needs a free connection slot to arrive on; there is no cancel for a REST request or on the MySQL
  wire. **A client that vanishes during a wait is not noticed**: nothing reads its socket while its statement runs, so the
  statement waits on and runs when the lock comes free, unless one of these timeouts ends it first. Set a `statement_timeout`
  or a short `lock_timeout` where that matters.
- **At most 256 statements wait at once**, across the server. The next is refused with 55P03 at once, saying so. Each waiter
  pins a thread of the server's blocking pool (512), and when the pool is full nothing answers, the `COMMIT` that would free
  them included. REST runs at most 384 requests at once for the same reason and queues the rest, holding no thread. With
  `max_connections` raised, or many long statements, the sum can still reach the pool; keep them below it.
- **Deadlocks end at the timeout.** Multi-table DDL takes its tables in sorted order, so two of those cannot deadlock.
  Transactions locking tables in opposite orders behind waiting DDL can, and the timeout ends it.
- **DDL inside a transaction keeps the table exclusively until the transaction ends**, as in PostgreSQL. The next statement
  of a migration finds it its own, and writers wait for the whole migration, so keep such transactions short. If the
  transaction wrote the table first, the DDL upgrades its lock: it waits for the other holders, bounded by the same timeout,
  ahead of any DDL already queued (which is waiting for this transaction). It is refused with 55P03 at once only where waiting
  is certain to deadlock: another transaction is already waiting to upgrade the same table. A `MERGE BRANCH` inside a
  transaction that wrote one of its tables is refused at once too: it lands in commits of its own, and would be waiting
  for the transaction it runs in.
- **A DELETE takes every table its foreign keys make it write** (`ON DELETE CASCADE` and `SET NULL`) up front, in sorted
  order, so its 55P03 means no child has lost a row.
- **Getting out.** Whoever holds a table is named by kind in the 55P03, not by session: end the transaction that holds it
  (`pg_stat_activity` lists the sessions), or let `idle_in_transaction_session_timeout` end it, which releases the lock.
- **No lock is held through a standby's acknowledgement** on `COMMIT` or an autocommit statement: a table is free again when
  the write is durable here. Two exceptions: a `MERGE BRANCH` landing, and a schema statement inside an explicit transaction,
  which keeps its table until the transaction ends.
- A READ COMMITTED or autocommit statement that waited reads what the DDL left. A REPEATABLE READ transaction keeps its
  snapshot, and if a merge changed a row it wrote, its COMMIT gets 40001.

With no DDL or merge running, a write pays one atomic load and the bookkeeping of its own lock set, and allocates nothing.
