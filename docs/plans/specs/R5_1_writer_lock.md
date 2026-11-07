# R5.1 — per-table writer lock (first R5 implementation step)

Parent: docs/plans/specs/R5_statement_atomicity.md. Part 1 decisions 1, 13, 15 win. Why first: every
concurrency finding of the B3 reviews (g/b3_review5.txt #2, #3, #4; the ADD COLUMN backfill race; R5
design H1) is one defect: nothing keeps a writer out of a table while something rewrites it.

## The primitive
`src/storage/table_locks.rs` (new): per engine, table name -> a fair shared/exclusive lock, entries
created on demand and removed when idle. Shared = a statement or transaction that may write the table.
Exclusive = DDL or MERGE that reads or rewrites the table's rows or catalog records. Writer preference:
a waiting exclusive holds new shared requests back, so DDL cannot starve (the shared holders it waits
for are bounded by the timeout). Reentrant for the owner: an owner set per transaction or statement, so
a statement that already holds a table never waits for it again, and an upgrade (shared held, exclusive
wanted by the same owner, e.g. DDL in an explicit transaction that wrote the table) succeeds when it is
the only holder, else 55P03 at once with the reason. Waits happen off the async workers: statements run
under offload_statement already; assert it in test builds. `[session] lock_timeout_ms` (default 30000)
bounds every wait: 55P03 "could not obtain lock on table t" naming the holder kind. Readers never touch
it. When no exclusive is held or wanted anywhere, a shared acquire costs one atomic load plus a per-owner
set insert: measure it; the perf smoke must stay green. Temp tables (`pg_temp_*`) still take it, cheaply.
Physical standby: no writers, nothing to do. Branch sessions: the lock key is (level, table), level = the
statement's branch level (main = 1), so branches do not block main; B3's exclusive drain section stays.

## Who takes it
- Shared, from BEFORE the statement reads the rows it will change, until the end of the statement
  (autocommit) or the end of the transaction (explicit). Acquire at the statement level in both DML
  families (`execute_in_transaction_inner`, `execute_plan_with_params_inner`), from the plan's target
  table; and in every fast path that bypasses plans (`try_fast_insert*`, fast UPDATE/DELETE by PK, COPY,
  batch inserts, REST writes, MCP, vector-store and code-graph writes, MV refresh's own writes). FK cascades
  and triggers: acquire on the child or target table when reached (reentrant owner), in sorted order when
  known up front. A held shared lock survives a savepoint rollback, like PostgreSQL's row-table locks.
- Exclusive: every DDL arm that reads or rewrites rows or catalog records of the table: ALTER (all forms),
  DROP TABLE (partition children, sorted), RENAME, TRUNCATE, CREATE INDEX (backfill), DROP INDEX, CREATE/DROP
  TRIGGER, REFRESH of the MV's backing table; FK DDL also on the referenced or inbound table. Multi-table: sorted order.
  Released before R1's DdlWrite::wait_replicated.
- MERGE BRANCH: exclusive on every table of its plan at the target level, from before key validation through
  the last table's landing; a resumed merge takes it again and re-validates. A merge's budget (30 s) includes it.

## What this removes
`merged_writes`, `hold_merge_landing`, `validate_and_record_with_merge_floor`, `merge_conflict_floor`,
`exempt_from_merged_writes` (conflict.rs, transaction.rs): delete them; a writer now waits. The census re-checks
in the merge added for the same race (`refuse_open_writers`, the second check under the tree locks): an open
transaction holding the shared lock is now what the merge waits for, then 55P03 after the timeout, naming that a
transaction holds the table. The `READ COMMITTED` upgrade note goes. Keep what is still needed for other reasons, and say why.

## Detector
In test builds (debug_assertions or internal-tests) `WriteGate::write/put/delete`: a write of a `data:{t}:`, `v:{t}:`,
`bdata:{L}:{t}:` key on a thread whose statement holds no lock on (level, t) panics, naming the table and the
caller; release: a counter + WARN once per table. Allow-list only what cannot race: open-time paths, WAL replay,
the standby applier, version GC, background MV maintenance if proven. Run the whole default tier with it on: every hit is a missed path.

## Tests
New tests/table_write_lock.rs + the review scripts as tests (g/b3rev5_scripts/t_generic.py params, t_generic2.py del,
t_generic3.py, t_trunc.py, t_trunc2.py; the ignored w16, w17 in tests/branch_schema_isolation.rs lose their ignore):
during a merge and during an ALTER ADD COLUMN DEFAULT over 20k rows, each writer kind runs concurrently (both families:
single-row and multi-row INSERT, UPDATE by PK, UPDATE by non-key predicate, DELETE both ways, ON CONFLICT, INSERT...SELECT,
COPY, TRUNCATE, explicit transaction before and during) and afterwards rows, every index lookup, COUNT(*) fast path and a
restart agree, and every statement either succeeded or got a clean 55P03. Timeout: a held transaction makes ALTER and MERGE
fail with 55P03 after lock_timeout_ms (set small). No deadlock: transactions locking two tables in opposite orders with DDL
waiting: the timeout ends it. Starvation: DDL goes through under a continuous writer load. Reentrancy: DDL in an explicit
transaction that wrote the table. Unrelated tables never wait. Deterministic handshakes, no sleeps.

## Rules
No cargo, no git. Both executor families. No new unwrap/expect on non-test paths. STOP and report if statement-level
acquisition cannot see a write target on some path (say which) or if the lock would have to be held across an await.
Report: acquisition sites, what was removed, the detector's allow-list with the reason per entry, anything in this spec that turned out wrong.

## As built (what turned out different)

Code: `src/storage/table_locks.rs`, the funnels in `src/branch_schema.rs`, `StorageEngine::merge_branch`.

**The hot path.** A shared hold is published in the owner's own set and an exclusive request scans the owner sets, a Dekker
pairing with a fence each side, instead of a per-table count: `publish; fence; load pressure` against `raise pressure; fence;
scan`. So with nothing exclusive around a statement's cost is its owner-set bookkeeping (one uncontended mutex, twice) and
one load, with no allocation up to 4 tables. The statement owner is one per thread, moved in and out of a thread-local. Table
identity in a lock key is a 64-bit FNV-1a of the name: a collision over-locks, it never under-locks. A transaction's owner
is created at its first write.

**A snapshot taken before the lock is a stale snapshot (not in the spec, and the largest thing it missed).** A transaction's
snapshot, autocommit's included, is taken before the statement parses, so a statement that waited for a merge read the rows
from BEFORE it and wrote them back: exactly the race, with the lock held correctly. It is not only a wait: a DDL can start and
finish between the snapshot and the lock. Fixed by comparing the schema generation, read before the snapshot
(`Transaction::snapshot_generation`), once the statement holds its locks: moved, an autocommit or READ COMMITTED transaction
takes a new snapshot (`refresh_snapshot_after_locks`). One atomic load when it did not. `snapshot_ts` became atomic for it,
since the statement only has `&Transaction`. A validating transaction keeps its snapshot and gets 40001 at COMMIT. Every exclusive
hold moves the generation when it ends, so that this and the next item see it.

**A spec cached before the lock is a stale spec.** The fast paths hold a cached schema, so they lock BEFORE they fetch it, where
the table name is known without it (literal paths from the text, parameterized ones from the plan). Where the spec is what names
the table (`try_autocommit_fast_update_delete_params_cached`, the batch and global-slot INSERT arms) they read the generation
before the spec and decline to the general path if it moved. A schema statement clears the SQL caches again before it releases
(`PlanCachesClearedOnDrop`): the first clear runs before it, and another session can refill them from the old schema meanwhile.

**Who is the owner.** The spec's "an owner set per transaction or statement" needed one more rule: a transaction binds itself to
the thread for each writing statement (`Transaction::bind_table_locks`, `bind_statement_transaction`), because statements hop
threads and the owner cannot live in a thread-local. An exclusive hold in a transaction stays until the transaction ends, as in
PostgreSQL (the review's finding 1: going back to shared made the next DDL of a migration an upgrade, which was refused beside any
writer, with the first DDL already persisted).

**A failed commit.** The hold must outlive a commit that fails, because the caller replays its eager index undo afterwards:
`Transaction::table_locks_hold`, beside `write_census_hold`. Rollback paths already undo before they drop the transaction.

**DDL ordering against an exclusion.** Check, lock, check again. The first check keeps B3's behaviour of refusing DDL on a table a
merge holds at once (55006) rather than making it wait; the second catches a merge that registered while it waited and did not
finish. A merge registers first and then locks, so a statement past its check is what it waits for. The exclusion outlives a
crash; `MAIN_STATEMENT`, `MAIN_DDL_IN_FLIGHT` and `wait_for_main_ddl` are gone, since the lock is what they counted.

**A merge into a branch** does not take main's lock on the tables its target inherits, which would block main's writers for a
branch's merge. It waits, taking nothing, for exclusive holders already past their check (`wait_for_exclusive_holders`).

**A merge has an owner of its own** (`separate_owner`): it lands in commits of its own, so inside a transaction that wrote the
table it must not count as that transaction. That case is refused at once.

**A transaction's first staged write also takes the lock** (`TransactionLocks::ensure_shared`, decision 1's wording), for a
transaction driven on the storage API with no statement (`delta_applicator`, the protocol adapter, a test). It is counted
(`debug_table_lock_late_acquisitions`) and `tests/table_write_lock.rs` asserts every SQL writer kind leaves it at 0, so it cannot hide a
missed statement-level path. An autocommit transaction has no such backstop: the detector sees it.

**The detector** is armed at the end of an `EmbeddedDatabase` open (`install_trigger_runtime`), which is what allow-lists open-time
paths and every storage-level unit test on a bare engine or gate. In a release build it looks at keys only while an exclusive lock
is held or wanted in the process, so the write path pays one load the rest of the time; the spec's "a counter + WARN" on every
write would have cost a batch iteration per write. Allow-list, by `allow_unlocked_row_writes`: version GC (deletes history below every
pin, never a row, and locking would make GC and DDL wait for each other for nothing); WAL replay and its apply (at open, and on a
logical replica's resync, whose primary writes for it). The standby applier does not pass the gate at all.

**Off the workers.** Every wait goes through `runtime_offload::off_worker` inside the lock, so "assert it in test builds" became "it
cannot be otherwise": a path that did not offload (`copy_bulk_insert` from a test or an embedder) does not pin a worker either.

**What the lock does not close**: a statement PLANNED before a DDL and reading another table (`INSERT … SELECT`'s source, a join) reads
with its old plan: that is M5 (decision 2). REPEATABLE READ spanning a row-rewriting ALTER reads the old width, and the ALTER's
rewrite records nothing in the conflict registry, so its COMMIT is not refused: M5 and attmissing (decision 4) close it. A merge's
landing commit waits for synchronous standbys holding its lock; only DDL's catalog writes defer that wait.

**Item 2 (R5 decision 11), done in the same pass.** Schema, constraint and reverse-FK cache fills carry the generation read before their
durable read (`cache_schema_if_current` and its two siblings) and are refused when it moved; every DDL publishes in the order write, bump,
then insert or remove (`publish_table_schema`, `rename_table`, `save_table_constraints`, `drop_view`, and DROP TABLE, which invalidates after
its write as well as before). RENAME's pre-check reads the durable record. Not in the decision: the plan cache had the same hole (a reader
that planned before a DROP and admitted after it kept serving the dropped table), so it is guarded the same way
(`plan_cache_put_if_current`). The fast paths' spec caches fill under the shared lock, so a schema statement's last clear removes what they filled. The two INSERT paths that fill before the lock, because the spec names the table, clear the cache if the generation moved during the fill (`fast_param_insert_spec`).

**What the gate's first run found.** Not in the lock: the writer matrix's `INSERT … SELECT … $1` failed `Parameter $1 not provided`, because the
params family's `InsertSelect` arm has never given its source's `Executor` the statement's parameters (the shape predates this pass: `b18edab2`,
and before it `c3f77c68`). Fixed, with both `CREATE TABLE … AS` paths and `EXPLAIN ANALYZE`, in `tests/params_insert_select_binds.rs`, and it goes
in the changelog as its own fix. The same failure left the matrix hanging for four hours: the writers' flag was set only after a statement that a
panicking `thread::scope` body never reached, and own keys collided after 100,000 rounds. `beside_writers` now stops the writers on every way
out, ends the warm-up as soon as one fails, bounds them by round and by time, and fails naming the bound; a per-test `watchdog` ends the process
if a statement itself never returns.

**The second run: an upsert wrote back a version.** The ADD COLUMN matrix lost one row's `x`. The text family's `ON CONFLICT DO UPDATE` read the conflicting row with `Transaction::get`, which resolves a committed row through its version history, and `add_column_to_rows`, `drop_column_from_rows` and every fast UPDATE move `data:` on with no new version. So the upsert took the pre-ALTER image and wrote it over the backfill. It has nothing to do with the lock and needs no race (the read dates from at least `e2d69d52`): the matrix hit it only when the writer had gone round its ten keys before the ALTER. `Transaction::get_latest` reads what the transaction staged, else the row as it is. R5.2 note: both DO UPDATE arms ignore an assignment to a column past a narrow row's end (`if target_idx < …values.len()`), which a no-rewrite ADD COLUMN makes reachable.

**The detector and the process.** A batch's keys are looked at in RocksDB's `extern "C"` callback, and a panic there cannot unwind: one finding aborted a whole test binary. The callback only takes notes now, and the report is made once it has returned. A finding on a thread already unwinding goes in the counter and the log rather than panic again. `write_batch_through_gate` tracks its caller, so a finding names the writer. A test that plants rows no statement could write says so (`debug_plant_rows`).

**The first review's fixes.**
- *Upgrades wait* (spec: "else 55P03 at once", wrong: an autocommit statement is not waiting for this transaction, it is about to finish). They go ahead of queued exclusive requests, which wait for the upgrader's hold; a second upgrader of one table is refused at once, the one case that is certain to deadlock.
- *55P03 means nothing changed*: a DELETE takes every table its foreign keys write (`cascade_lock_tables`) up front, sorted, in both families. The params family commits each child by itself, and used to reach a held child with an earlier child's rows gone. Other multi-table writers were read: DDL and merges take theirs up front; ON UPDATE actions are parsed and never run; `CALL` is in one transaction; a writing UDF on the read path commits statement by statement, as it does for any failure, and is not changed here.
- *At most `MAX_LOCK_WAITERS` (256) wait*, half of Tokio's blocking pool, and REST runs at most 384 at once. Past that the pool can still fill: long statements on connections raised well above the default, advisory-lock waits with no `statement_timeout`, MCP.
- *Waits poll* the statement's deadline and its cancel flag (50 ms for a shared request; an exclusive one still ticks at 5 ms, since a shared release can miss it). PostgreSQL's CancelRequest was not routed at all; `session::scoped::request_cancel` finds the backend by pid and key. A vanished client is not noticed from here: nothing reads its socket during its statement.
- *Wake-ups are per table*: pressure per bucket of the table's hash, a condition variable per entry. A writer of another table takes no mutex and wakes nobody.
- *`SET lock_timeout`*: a registered, user-settable GUC read when a wait begins. 0 is the server's value, not an unbounded wait.
- *A commit lets go before its standby wait*: a transaction's owner in `commit_with_timestamp`, an autocommit TRUNCATE through `defer_commit_replication_wait`. A merge's landing still waits holding its lock, and DDL in an explicit transaction keeps the table through its wait.
- *`execute_in_session` outside a transaction* opened no statement scope, so its commit wrote with no lock: the only embedded entry point that did. Raw `begin_transaction` callers (`delta_applicator`, the protocol adapter) get theirs at the first staged write and keep it through the commit.

**The merged tree's review.**
- *A writing function waited for its own transaction*: its body runs statements of its own on the caller's thread, and a read statement bound no owner, so after the transaction's DDL they queued behind it. `run_statement_in_transaction` now binds the transaction's existing owner for every statement (`Transaction::bind_existing_table_locks`), reads included; a transaction that has written nothing has no owner and binds none. `CALL` already ran under the statement's binding, and trigger bodies run no SQL.
- *MySQL wire*: DDL inside a transaction commits it first and runs by itself, as MySQL does (`causes_implicit_commit`).
- *`innodb_lock_wait_timeout` and `lock_wait_timeout`* (seconds) are `lock_timeout`, clamped to its bound; *`lock_timeout`* is at most `INT_MAX` ms, past which PostgreSQL's spelling is refused. A plain `SET` in a rolled-back transaction is not undone, for this setting as for every other session setting here: only `SET LOCAL` has an undo (`gucs_saved`).
- *A CancelRequest past `max_connections`* is read by a bounded probe (64 at once, 2 s each) and served. An SSLRequest before it is answered as the postmaster does: the TLS handshake when TLS is configured, else 'N'.
