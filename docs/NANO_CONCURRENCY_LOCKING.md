# Nano concurrency & write-locking — findings, Option 1 (shipped), Option 2 (planned)

Historical investigation: the sections through BUG B describe v3.60.x and its then-proposed
options. They are not instructions to remove the row/key ownership used by 4.42.0. The R5 sections
below describe the current release-candidate implementation; final correctness, physical and
performance acceptance remains pending.

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
  change until the statement ends (autocommit) or the transaction does (`BEGIN`). Shared table
  ownership allows ordinary writers to coexist; row/key locks still order conflicting writes.
- `ALTER TABLE` (every form), `DROP TABLE`, `RENAME`, `TRUNCATE`, `CREATE INDEX`, `DROP INDEX`, `CREATE`/`DROP TRIGGER`,
  `REFRESH MATERIALIZED VIEW` of the view's data table, and `MERGE BRANCH` on every table it changes hold it
  **exclusively**. Foreign-key DDL also holds the table on the other end.
- **Ordinary readers take no table writer lock.** `SELECT` does not acquire these table locks.
  An index-snapshot checkpoint is a separate maintenance operation: it takes sorted exclusive MAIN
  table owners before exporting ART/HNSW caches, and skips before any cache write if it already
  has a statement table owner or an active write census. After waiting it checks publication and
  schema state again. Locks cover export and marker publication; vector-manager publication checks
  also detect callers which mutate a graph without a table owner. Raw KV/ART callers retain their
  own synchronization/index-maintenance contract.
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
  here is always bounded: a session may set at most 2147483647 ms (about 24 days), PostgreSQL's limit. MySQL's
  `innodb_lock_wait_timeout` and `lock_wait_timeout` (in seconds) both set this one timeout, clamped to that limit
  rather than refused, since MySQL's own default for `lock_wait_timeout` is a year. Unlike PostgreSQL, a plain `SET`
  inside a transaction that rolls back is not undone (no session setting here is); `SET LOCAL` is.
- **A wait also ends at `statement_timeout`, and at a cancel** (PostgreSQL's CancelRequest, which psql's Ctrl-C and every
  driver's cancel send): SQLSTATE **57014**, and the statement does not run when the lock comes free. It looks every 50 ms.
  Row and table waits count `statement_timeout` from the start of the statement, including time
  already spent before a later wait. A cancel request is served even with every connection slot taken, also one that asks for TLS first, as
  `sslmode=prefer` does; there is no cancel for a REST request or on the MySQL wire. **A client that vanishes during a
  wait is not noticed**: nothing reads its socket while its statement runs, so the statement waits on and runs when the
  lock comes free, unless one of these timeouts ends it first. Set a `statement_timeout` or a short `lock_timeout` where
  that matters.
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
  for the transaction it runs in. A function called in the transaction runs its statements for the transaction, so a
  writing function after the transaction's `ALTER` does not wait for it (its writes still commit by themselves).
- **On the MySQL wire, DDL commits the open transaction first** and runs as a statement of its own, as MySQL does
  (`CREATE`, `ALTER`, `DROP`, `RENAME`, `TRUNCATE`, but not on a `TEMPORARY` table): nothing is held after it. The
  PostgreSQL wire and the embedded API keep their explicit transaction's lock ownership. This
  does not make DDL rollbackable: a completed schema statement commits separately from the block's
  staged DML and can survive its ROLLBACK.
- **A parent key UPDATE or DELETE with inbound foreign keys prelocks its relationship graph**, in
  sorted order, before scanning child references. This includes recursive CASCADE, SET NULL and
  SET DEFAULT targets and excludes a concurrent child INSERT while the parent key changes. It
  then locks changed child rows and reads their staged/current images. Ordinary tables without
  inbound foreign keys keep the shared-table path. Constraint timing and Off/Proxy/Audit policy
  remain separate from lock acquisition (`src/fk_update.rs`).
- **Getting out.** Whoever holds a table is named by kind in the 55P03, not by session: end the transaction that holds it
  (`pg_stat_activity` lists the sessions), or let `idle_in_transaction_session_timeout` end it, which releases the lock.
- **Table ownership is released before a COMMIT or autocommit ACK wait.** Local data/index
  publication finishes first. A transaction releases its table owners explicitly; statement owners
  and explicit exclusive guards defer the wait until their table owners drop, including direct
  TRUNCATE and a MERGE landing. This statement concerns table ownership, not every index-key or
  row guard in the commit stack. A schema statement inside an explicit transaction still retains
  that transaction's table ownership until the transaction ends; its statement cannot defer an
  ACK wait to a future COMMIT.
- A READ COMMITTED or autocommit statement that waited reads what the DDL left. A REPEATABLE READ transaction keeps its
  snapshot, and if a merge changed a row it wrote, its COMMIT gets 40001.

With no DDL or merge running, a write pays one atomic load and the bookkeeping of its own lock set, and allocates nothing.

## One write group per schema statement (R5.3)

Spec: `docs/plans/specs/R5_3_ddl_batch.md`. A schema statement stages everything it makes durable in one batch
(`storage::DdlBatch`) and writes it with ONE RocksDB write: the catalog records, the rows it moves or deletes, its
logical-WAL entries, and the copies a live child branch takes of what it changes. So a reader, a physical standby (which
applies the primary's write groups one by one), a branch fork and a crash all see the statement entirely or not at all.

- **Converted** (one group, both executor families where the statement parses on both): `CREATE TABLE` (schema, counter,
  incarnation, constraints, identity, partition link), `DROP TABLE` with the partition children it cascades to,
  `ALTER TABLE … RENAME TO` (with its index definitions, triggers and column side storage), `RENAME COLUMN` (with the
  constraint records that name the column, other tables' foreign keys included, and its side storage),
  `CREATE [UNIQUE] INDEX` (every kind), `DROP INDEX`, `ADD CONSTRAINT …
  UNIQUE | FOREIGN KEY`, `DROP CONSTRAINT`, `ALTER COLUMN … DROP NOT NULL`, `CREATE`/`DROP VIEW`, `CREATE`/`DROP FUNCTION`, `CREATE`/`DROP PROCEDURE`, `CREATE`/`DROP TRIGGER`,
  `CREATE`/`ALTER`/`DROP SEQUENCE`, `CREATE MATERIALIZED VIEW`, `REFRESH MATERIALIZED VIEW` (plain,
  `CONCURRENTLY`, `INCREMENTALLY` and the auto-refresh worker), `DROP MATERIALIZED VIEW`.
- **Publication follows the owning write.** Schema/constraint caches and function/trigger/index
  registries publish in the batch's resource/commit phases. A proven pre-write refusal undoes eager
  registration. Native errors can have applied: actual-state reconciliation or table quarantine
  occurs before ownership drops. Known-applied work is never undone as if its group were refused.
  See the outcome/recovery section below.
- **A new index is hidden while it is built.** `CREATE INDEX` and `ADD CONSTRAINT … UNIQUE` register the index hidden:
  writers maintain it (they are kept out by the writer lock anyway), the read path's index choice skips it, and it becomes
  visible after its group. A reader used to take it while its backfill ran and get no rows.
- **TRUNCATE** stages data deletes, columnar presence, history and its logical operation in the
  owning transaction. Eager index key holds and inverse actions survive until settlement and are
  savepoint-aware; it does not defer a full-tree clear that would erase keys inserted later in the
  same transaction. Rollback reconstructs actual durable rows using the current schema while its
  index/census/table owners remain alive. Vector reconciliation is RAM-only; it issues no graph
  repair writes from a rollback/error callback. Direct borrowed-Executor TRUNCATE shares this path.
- **Side storage keyed by name moves with a rename.** A `DICTIONARY` column's dictionary (`dict:`) and a `COLUMNAR`
  column's batches, zone statistics and live-row presence (`col:`, `colz:`, `colzm:`, `colp:`, `colpm:`) are keyed by
  the table's and the column's name: `RENAME TO` and `RENAME COLUMN` move them in their group, under the zone-stats
  lock from the scan to the write (a columnar read's lazy backfill writes under it), and the dictionary cache lets go of
  both names after the write (the next use loads the moved dictionary). A dropped column's dictionary is not deleted, so `RENAME COLUMN` deletes whatever is stored under
  the target name first. The live trigger registry follows a `RENAME TO` after its write too.
- **`pg_indexes`** reads the user indexes from their durable definitions (which change in the same group as the
  table), not from the in-memory registry a `RENAME TO` or `DROP TABLE` changes after its group, and is built again
  until two builds agree: a reader sees a table's index set whole or not at all.
- **Materialized-view creation/population** uses one DdlBatch for metadata, backing definition,
  replacement rows/side storage, logical operations and its one `snapshot:` record. Indexed and
  non-default-storage backing tables use this path too. A compatible refresh keeps existing index
  and storage definitions and retains history; a true shape replacement stages drop/purge/create
  and a Refreshed layout cut in that same group. Persistent graph replacement is an owned DDL
  resource, settled before ordinary publication callbacks. A refused group preserves the old view;
  an ambiguous native result is reconciled or quarantined instead of acknowledged as unchanged.
  `INCREMENTALLY` can rematerialize the full query; its syntax is not proof of delta application.
- **The ceiling.** A write group above what a standby can receive (`MAX_BATCH_BYTES`, about 4 GiB) is refused before it
  is written, SQLSTATE 54000, whatever made it: a schema statement, a transaction's commit, a branch freeze.
- **Still more than one group** (and why): `CREATE TABLE … AS`, `CREATE SCHEMA` with elements (each
  element is its own statement), `DROP TABLE a, b` (one group per table), `CREATE`/`DROP ROLE`,
  `GRANT`/`REVOKE` over several objects, `CREATE DATABASE BRANCH`. Every `ALTER TABLE` is one group
  since R5.5 (below). Persistent graph construction can precede publication of its hidden index
  definition; actual retirement stages the graph-key purge with the owning DDL group and settles
  its lease before registry publication. MV replacement no longer relies on a later graph purge.
- **The detector.** While a batch is open on a thread, a write of the same store that is not its commit is a finding:
  test builds panic naming the writer, release builds count it and warn once per site. What may write apart, and why: the
  index-snapshot invalidation (derived markers), a dropped name's version-history purge (a name that is no table), a
  branch's frozen copy of a definition it already reads, and a columnar read's lazy backfill (derived
  side data). Persistent row graph hooks may still update derived graph records separately; their
  authoritative latest-row images and dump-marker invalidation ride the owning row group, not that
  later hook. Data/catalog mutations and their logical operations cannot use a derived-write
  exception to split their commit.

## One write group per ALTER TABLE (R5.5)

Spec: `docs/plans/specs/R5_5_drop_column_and_multi_alter.md`. `AGENTS.md` promises "Multi-op `ALTER TABLE` is atomic per
statement"; it is.

- **Every clause of one statement in one group** (`src/alter_table.rs`, both executor families): the statement is the
  table as its clauses leave it, in memory; each clause is asked against that (a `CHECK` sees the column an earlier clause
  added, with its value in every row) and refuses before anything is written. Then one `DdlBatch`: the rows' new images
  when a clause rewrites rows, the schema, the missing values, the index definitions a rename names, the constraint,
  identity and columnar records, the shape record (B4) and the layout record (below), with their logical-WAL entries. A
  refusal by any clause leaves nothing; a crash leaves the statement whole or not at all. Indexes a clause registers are
  hidden until the write and taken back if it fails. `RENAME TO` combined with other clauses is refused `0A000`.
- **`DROP COLUMN` and `SET STORAGE` rewrite every row in that group**, each with a version at the statement's one
  timestamp (as spec R5.2's volatile `ADD COLUMN` does), up to `[storage] max_atomic_rewrite_bytes` with the live branches'
  copies counted; above it `54000`, nothing written. They used to write the schema, then a group per row, then the
  constraints and the column-storage side data: a crash or a reader in between saw a schema without the column over rows
  that still had it, every later value under the wrong column.
- **Reads older than a layout change.** A row version keeps the layout it was written with. `meta:table_layout:{t}`
  (`storage::table_layout`) records, in the change's group, every `DROP COLUMN` (width and position) and every cut of the
  table's history (`RENAME TO`, a refresh that swapped a view's data table). `SET STORAGE` preserves logical versions;
  its legacy layout record remains readable and is not a table-wide history cut. `AS OF`, a transaction's snapshot
  scan and its point reads, and `VERSIONS BETWEEN` take a version
  through the drops made after it, so each value reads under its own column; a read from before a cut is refused `0A000`
  (the marker of a branch anchor refused for the same reason), except by the transaction whose own `RENAME TO` made it.
  Past a rename, a row it moved with no version at the snapshot and a change since is refused too (B4's rule). A rename
  and a refresh that replaced a view's data table write their own `snapshot:` record, so `AS OF` a time after them
  resolves after them. `CREATE BRANCH … AS OF` reads a table that has the record by it (`materialize_branch_anchor`), so
  an anchor and `AS OF` give the same answer for the same time; B4's `meta:table_shape:` rule is the fallback for a
  table without one.
  Every new version and every writer-materialized preimage stores logical values, including dictionary, CAS and
  columnar columns. Legacy versions are mapped into today's layout first; retained dictionary IDs and CAS hashes use
  their immutable payloads when present. A remaining ColumnarRef or a missing immutable payload is refused with the
  same typed history marker; a historical ColumnarRef never reads today's mutable slot. Other logical versions remain
  readable. Raw `SnapshotManager` byte APIs preserve the original stored layout; the decoded SQL, transaction, scan
  and past-branch funnels apply these rules before exposing row values.
  `RENAME COLUMN` and `ADD COLUMN` need no record. The loaded records are forgotten with a table's missing values, on the
  primary and through a standby's applier.
- **Layout epochs (R5.6)** validate the schema used to plan against the rows exposed, including
  prepared plans, warm result-cache hits and vector/index scans on a primary or standby. A safe
  read can replan within the bounded retry policy. Writes and executed sequence/procedure/advisory
  effects are not replayed. A stale layout is `40001`; a prepared result-shape change is `0A000`
  before execution. An already-applied publication error retains its system-error classification,
  even when its nested derived cause mentions a layout race.

## The row write lock (R5.4)

Spec: `docs/plans/specs/R5_4_dml_one_group.md`. What keeps two writers of one row from losing each other's work.
Before 4.42.0 three clients running `UPDATE cnt SET n = n + 1 WHERE id = 1` over the extended protocol were told 576
times that their increment committed and the counter ended at 426: each read the row, computed, and wrote it back over
the other's commit. R5.1's table lock does not order them, since every writer holds a table shared.

- **Every UPDATE, DELETE and `ON CONFLICT DO UPDATE` locks each row it changes**, before it computes the new image, on
  both executor families, on the primary-key fast paths, in transactions, on a branch (a branch's rows are locked by
  level and row) and in the children an `ON DELETE CASCADE` / `SET NULL` reaches. A transaction holds its row locks
  until its commit is applied (or it ends); an autocommit statement until its one write group is applied; a
  `ROLLBACK TO SAVEPOINT` lets go of the rows locked after the savepoint. Inserts lock nothing: their rows are new.
- **A writer reads the row from storage, never from the row cache**, and every write fences the cache's fills: a
  reader that read a row's old image before a write can no longer put it back afterwards. (A point reader refilling
  the cache with the old image made the next PK increment compute from it: 7-15 lost in ~25k.)
- **A key an open transaction took out is not free.** A transaction's DELETE (or its UPDATE that moves a PRIMARY KEY /
  UNIQUE value) takes the old key out of the index at once, so the transaction itself sees it gone; until it ends the
  key is **held** for it. Another writer of that key waits for the transaction, as in PostgreSQL: an INSERT then
  succeeds after a commit and fails 23505 after a rollback (it used to go ahead, and the rollback left two rows with one
  primary key), and a PK UPDATE / DELETE finds the row and waits for its lock (it used to find nothing and lose its
  change). Readers see the key gone, as before.
- **`INSERT … ON CONFLICT` is PostgreSQL's speculative insertion.** It holds the value it arbitrates on until its
  transaction ends, so a second upsert of the same new key waits for the first and then takes its DO UPDATE (or inserts
  after the first rolled back) instead of failing 23505 or 40001; a conflicting row deleted meanwhile makes it insert.
  A plain INSERT does not take that hold, so an upsert racing a plain INSERT of the same new key can still get 23505.
- **A second writer of the row waits** and then, at READ COMMITTED and in autocommit, changes the row **as the first one
  committed it**: it evaluates its `WHERE` clause again over the committed row and computes its assignments from it,
  as PostgreSQL's READ COMMITTED does. A row that no longer matches is not changed; a row deleted meanwhile is not
  changed either.
- **REPEATABLE READ and SERIALIZABLE do not re-evaluate**: a row changed by a commit after the transaction's snapshot is
  a serialization failure, SQLSTATE **40001**, at the statement or at COMMIT. Never a silent overwrite.
- **A wait is bounded like a table-lock wait**: `lock_timeout` (55P03, "could not obtain lock on row in relation"),
  `statement_timeout`, counted from the start of the statement (a row or table lock wait that begins late in a statement
  has only what is left), and a cancel (57014). A wait that would close a cycle of waiters fails at once with **40P01**
  (deadlock detected) in the waiter that closes it. A cycle through a table lock and a row lock (a transaction upgrading
  its table lock for DDL, e.g. `TRUNCATE`, while another statement holding the table shared waits for one of its rows)
  is not detected: the waiter's `lock_timeout` ends it (the review measured 8 s at `lock_timeout_ms = 8000`), and the
  DDL then proceeds. Detecting it would mean one wait graph over table locks and row locks, whose owners are different
  objects today.
- **A wait that could never end is refused at once.** The embedded handle's own transaction (`BEGIN` on the handle) can
  only be committed by the thread that opened it; a statement on that thread (a session's, say) that wants one of its
  rows fails at once with 40001 instead of waiting out `lock_timeout`. A session's or a `begin_transaction()` handle's
  transaction can be ended from another thread, so a wait for it is bounded by the timeouts only.
- **Cost**: re-reading every row under its lock would cost a point read per row of a bulk UPDATE, so a statement loads
  its table's change epoch before it reads and re-reads a row only if some writer of that table committed since. With
  no concurrent writer of the table, a row lock is one uncontended mutex and a map insert, and a release the same.
- A commit that holds row locks takes a fresh commit timestamp (an autocommit statement otherwise commits at its
  snapshot's), so its version sorts after the commit it waited for.
- **The ceiling, early.** A statement or transaction whose staged rows can no longer fit one write group is refused
  (54000) while it stages them, not after its whole batch was built: a group holds each row up to three times (the row,
  its version with time travel on, its logical-WAL operation), so the effective limit is about 1.33 GiB of row data
  (keys and serialized rows) per statement or transaction with the defaults, 2 GiB with time travel off. An autocommit
  `INSERT … SELECT` past it fails in seconds and frees what it staged; v4.41.1 wrote such a statement row by row.
- **Foreign-key actions** share `src/fk_update.rs` across ordinary UPDATE/upsert and recursive
  DELETE. NO ACTION/RESTRICT/CASCADE/SET NULL/SET DEFAULT check the proposed/final relationship,
  including composites and self-references. Only deferrable NO ACTION waits until SET IMMEDIATE
  or COMMIT; immutable touched-row/old-parent-key obligations and timing overrides roll back with
  savepoints. Named timing is restricted to supported unqualified public FK names; unsupported
  schema selections refuse before overrides change. MERGE validates the final target schema and
  rows before intent and on resume with its durable policy; a pending intent does not itself
  exclude valid target DML between attempts.


## Commit outcome and local startup recovery (R5.7)

`storage::write_outcome::WriteAttempt` distinguishes NotWritten, Applied and Unknown without a
new durable transaction record. The gate marks Unknown immediately before the owning native write
and Applied on native success, before later fsync/ACK/publication failures. High-level settlement
runs with transaction/index/census owners alive: undo only NotWritten; keep applied index ownership;
fence all affected tables before waiting for retirement on Unknown. No error callback writes a
persistent graph repair. The original native error remains the operation's error.

A successful native row group whose derived vector publication fails is a different case: required
committed callbacks still run, then the table is fenced before the snapshot barrier drops and the
client receives PostgreSQL 58000 / MySQL 1105 (HY000), with “row changes were already applied”. The
nested cause may describe a refused derived group. Reopen and inspect durable rows before retrying;
this error must not become a generic layout retry or a “nothing was written” promise.

Current-format primary startup adopts already-committed data groups and advances/prunes logical
history; it never redoes old logical row images over them. Physical standby/archive application
uses its own complete-group/cursor recovery path. Ambiguous legacy retained log above its existing
checkpoint refuses writable open and names the stopped-directory, exclusive-lock command
`heliosdb-nano recover-wal --data-dir /path/to/data --adopt-local-state`. That command changes only
recovery metadata and preserves local rows/catalogs/branches/history/log bytes; it does not
reconstruct missing older writes. The parseable wal_replay_destructive_ddl setting cannot enable
automatic startup redo. See `docs/plans/specs/R5_7_crash_replay.md` and the migration note in CHANGELOG.

Managed persistent row-image overlays are part of the owning heap group; read-only open/application
can reconcile unfinished graph publication in RAM without changing database bytes or sequence.
Default HNSW dump validity is invalidated in the row group too. Full graph encryption is separate:
new row_image values use the enabled engine key, while legacy opt-in persistent graph values remain
unsealed. These source contracts do not constitute final runtime or release acceptance.
