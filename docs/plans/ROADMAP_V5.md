# HeliosDB-Nano — Roadmap to v5.0

**Status:** draft, 2026-07-27. **Current release:** v4.6.3 (`208611d`, crates.io + `gh release
list` confirmed latest). **Scope:** every known outstanding item as of this date, sequenced into
milestones. **v5.0 ships when this roadmap is empty** — no item deferred without an explicit
decision recorded in this file.

## How this document was built

Every item below was checked against source at `208611d` (`git log -1`), not carried over
from memory or prior reports unverified. Where a claim from the originating inventory matched
the code, it's marked **Verified** with file:line anchors. Where the code told a different or
more nuanced story, it's marked **Correction** and the actual finding replaces the original
claim. Two items (§1.2, §1.3) are already being fixed in a parallel session as this document is
written; they're marked **In-flight** and sequenced as if landing imminently, not as open
investigation.

Quality gates referenced throughout (`cargo test --lib`, the full integration suite, doc tests,
HA suite where applicable, fmt/clippy/deny, the perf-gate battery, interface-coverage) are the
ones defined in `CLAUDE.md` § Quality Gates and explained in `docs/GATES.md` — this document
does not restate them, only calls out which apply per item.

---

## Section 1 — Data correctness: items that can silently corrupt or leak user data

Ranked by **user impact**: can this happen under ordinary, correct SQL, with no error message,
against data the user has every reason to trust? Effort is a secondary sort key.

### 1.1 Row-Level Security is not enforced on any write path a real client uses

**Status:** open, unfixed. **Severity: highest in this document.** **Effort: substantial.**

**Correction, not an echo.** The original characterization ("RLS is enforced on read paths
only, not on writes") undersells this. There *is* write-enforcement code — `LogicalPlan::Insert
/ Update / Delete` arms inside `execute_internal` (`src/lib.rs:12033`) call
`self.tenant_manager.should_apply_rls(table_name, "INSERT"/"UPDATE"/"DELETE")` and evaluate
`WITH CHECK` / `USING` expressions (`src/lib.rs:12129`, `12243`, `12387`, `12504`). But
**`execute_internal` has zero callers anywhere in the crate** (`grep -rn "execute_internal("`
matches only its own definition). It is dead code.

The two DML paths every real client actually hits never check RLS at all:
- `execute_in_transaction_inner` (`src/lib.rs:2951`–~5836; PostgreSQL simple-query, all MySQL
  wire, the REPL, the embedded `execute()` API — the "text family") — zero occurrences of
  `should_apply_rls` or `RLS` in its ~2,900 lines.
- `execute_plan_with_params_inner` (`src/lib.rs:13518`–~14597; PostgreSQL extended protocol —
  psycopg server-side cursors, JDBC, sqlx, Drizzle, node-postgres — plus every REST/BaaS write,
  the "params family") — same, zero occurrences.

The fast-path spec builders inside those two functions (`build_fast_param_insert_spec` at
`7882`, `try_fast_insert` at `9461`, `try_fast_update_params`/`try_fast_delete_params` at
`8305`/`8475`, `fast_literal_update_spec`/`fast_literal_delete_spec` at `10031`/`10118`) *do*
call `should_apply_rls` and correctly bail out of the fast path when it's true — but the "slow"
path they fall through to is the same RLS-blind generic INSERT/UPDATE/DELETE handling inside
the same two functions. The RLS check exists only to skip an optimization that was never the
problem; it does not route to anything that enforces the policy.

**Net effect:** for any table with `rls_enabled` and one or more policies (the SharedSchema +
RLS multi-tenancy mode documented in `heliosdb-nano-tenant`), **every INSERT, UPDATE, and DELETE
through psql, MySQL, the embedded API, the extended protocol, or `/rest/v1/` bypasses RLS
entirely, with no error.** `should_apply_rls` for `"SELECT"` is checked in the read/scan path
(`src/lib.rs:10640`, `10863`, `11185`, `19560`) and does work — so a session sees only its own
tenant's rows, which is exactly the false confidence that makes this dangerous: reads look
correctly isolated while writes are not. A tenant's application bug (or a malicious actor with
any write access) can create, modify, or delete another tenant's rows outright.

**Fix shape:** move (or duplicate, verified identical) the `should_apply_rls` /
`get_rls_conditions` / `WITH CHECK` / `USING` evaluation blocks from the dead `execute_internal`
arms into the live generic INSERT/UPDATE/DELETE handling inside both
`execute_in_transaction_inner` and `execute_plan_with_params_inner` — the same "shared helper,
called from both families" pattern the v4.6.3 constraint-parity fix (`8352b91`) just used for
FK/UNIQUE/CHECK. Given how directly that fix maps onto this bug, it is the template to follow.
New test surface: a `tests/rls_write_parity_tests.rs` mirroring `tests/constraint_parity_tests.rs`
— every INSERT/UPDATE/DELETE against an RLS-enabled table, run through both executor families,
asserting the policy is enforced identically. Gate: full suite + `tests/protocol_tests` (psycopg;
embedded tests cannot see this class of bug, same lesson as `docs/GATES.md` gate 2) +
`multi_tenancy_integration`.

### 1.2 `ON DELETE CASCADE` / `SET NULL` escape the enclosing transaction — **In-flight**

**Status:** confirmed still present at `208611d`; being fixed in a parallel session as of this
writing. Sequenced as landing before v4.7 ships. **Effort: small–substantial** (touches shared
DML infra; needs the same both-executor-family care as 1.1).

**Verified.** `cascade_delete_referencing_rows` (`src/lib.rs:18150`) and
`set_null_referencing_rows` (`src/lib.rs:18235`) each call
`self.storage.begin_autocommit_transaction()` (lines `18189` and `18285` respectively) and
`txn.commit()` unconditionally before returning — their own private autocommit transaction,
independent of any enclosing explicit transaction the caller may be inside. A
`BEGIN; DELETE FROM parent WHERE id=1; ROLLBACK;` where `parent` has a referencing child with
`ON DELETE CASCADE` rolls back the parent delete but the cascade's child deletes/nulls are
already durably committed. Silent, no error, exactly the "ordinary correct SQL" class this
section is ranking by.

**Gate:** `transaction_integration_tests`, a new CASCADE-inside-ROLLBACK regression case, plus
the full suite (this touches `src/lib.rs` DML machinery shared with 1.1's fix surface — land
them in the same reviewed batch if the parallel session's branch and this roadmap's owner agree
on sequencing, since both touch the identical two-executor-family pattern).

### 1.3 `Drop for EmbeddedDatabase` persists index snapshots before flushing row counters — **In-flight**

**Status:** confirmed still present at `208611d`; being fixed in parallel. Sequenced as landing
before v4.7. **Effort: one-line** (reorder two calls) **+ small** (crash-window test).

**Verified.** `impl Drop for EmbeddedDatabase` (`src/lib.rs:649`) calls
`self.storage.persist_index_snapshots()` at line `659`, then
`self.storage.flush_all_row_counters()` at line `671`. A crash between the two (process killed
mid-`Drop`, OOM, `kill -9`) leaves a durable index snapshot that assumes the counter state *at
that point*, but the counter itself was never flushed — reopen loads the stale counter, and per
the mechanism the v4.6.1 fix (`CHANGELOG` "hard crash... could leave a table's row-id counter
stale") already described for a related case, the next INSERT can silently overwrite a live row.
§2.4 (no SIGTERM handler) is the reason this crash window is wider than it looks: today `Drop`
only runs on `ctrl_c`/normal exit, so `heliosdb-nano stop`, `systemctl stop`, and container
shutdown all skip this code path entirely and hit the *worse* case (no snapshot persist and no
counter flush at all) rather than merely the ordering bug. Fixing 1.3 without also fixing 2.4
closes the crash-only exposure but leaves the SIGTERM exposure open.

**Fix shape:** flush counters before persisting snapshots (trivial reorder). Gate: a crash-window
test that kills the process between the two operations (or an injectable fault point) and
asserts reopen never reuses a row id — pattern from `wal_crash_recovery`/`crash_recovery_e2e`.

### 1.4 UNIQUE constraint checks read a branch-blind shared index

**Status:** open. **Effort: substantial** (needs a real design decision, not a patch — see
below).

**Verified, and the finding is worse and more specific than the one-line summary.**
`ArtManager::unique_key_exists` (`src/storage/art_manager.rs:1591`) filters candidate ART
entries by `entry.table == table` only — no branch parameter exists in its signature at all.
`enforce_unique_on_update` (`src/lib.rs:18403`) calls it unconditionally for every UPDATE that
changes a PK/UNIQUE column, regardless of whether a branch is active.

But the other half of the picture is that `insert_tuple_branch_aware_with_schema`
(`src/storage/engine.rs:13201`), the function that actually writes a row on a non-main branch,
**never calls into `art_indexes()` at all** (verified by reading the full function body,
`13201`–`13295`: it writes `bdata:{branch_id}:{table}:{row_id}` and a `bv:` version key, updates
the MV delta tracker and speculative filters, and returns — no ART maintenance). This is by
design, not an oversight: v4.2.0 (`CHANGELOG` "Fix (branch isolation, wrong-data class)")
deliberately stopped ~11 DML sites from maintaining "the process-wide value index for
branch-routed writes" after it caused phantom UNIQUE violations on main and branch DELETEs
stripping main's inherited-row index entries. That fix corrected the *write* side (branches no
longer corrupt main's ART). It did not touch the *check* side — `enforce_unique_on_update` still
unconditionally probes the same all-main ART.

Net effect for an UPDATE on a branch that changes a UNIQUE column: the check consults an index
that (a) contains only main's committed values, never this branch's own rows, and (b) is
consulted regardless of branch. Two independent failure modes follow, both real: a value already
used by *main* (or by sibling data the branch inherited but hasn't itself touched) can wrongly
block a legitimate branch UPDATE; and a genuine duplicate created entirely by two branch-local
rows is never caught, because neither row is in the index being checked. The originating
characterization ("uniqueness may consult rows from other branches") is directionally right but
undersells the second failure mode — silent duplicate admission — which is the data-corruption
case, not the false-positive-rejection case.

**Fix shape:** this needs a decision, not a patch: either (a) give ART indexes real per-branch
scoping (bigger: touches index maintenance, GC, merge), or (b) fall back to a branch-aware table
scan for the UNIQUE check specifically when a branch is active (smaller, slower, correct — mirror
how `scan_table_branch_aware` already merges main + ancestor + current-branch data for reads).
Option (b) is the pragmatic v4.x fix; option (a) is a v5.0-or-later branching-engine project.
Gate: `branch_data_isolation_test`, `branch_storage_test`, a new UNIQUE-across-branch-and-main
regression matrix.

### 1.5 `ON CONFLICT DO UPDATE` does not re-check `NOT NULL`

**Status:** open, and — per the code's own comment — deliberately consistent with a second,
equally-open bug. **Effort: small** (once 1.1/1.2's shared-helper pattern exists, this is a
one-arm addition).

**Verified exactly as described**, with the code self-documenting the tradeoff:
`validate_on_conflict_updated_row` (`src/lib.rs:18527`) checks FK and CHECK constraints and calls
`enforce_unique_on_update`, but its doc comment (`18524`–`18528`) states: *"NOT NULL is
deliberately not checked here: neither general UPDATE arm checks it either, and this helper's
contract is parity with UPDATE, not a new rule for one statement type."* So `ON CONFLICT DO
UPDATE SET col = NULL` on a `NOT NULL` column silently succeeds — and so does a plain
`UPDATE t SET col = NULL` on a `NOT NULL` column, which is the actually-scoped bug: **the general
UPDATE path itself does not enforce NOT NULL.** This is a schema-constraint bypass reachable by
the most ordinary SQL there is. Confirmed present in the v4.6.3 CHANGELOG's own "Known
limitations left unchanged by this release" list.

**Fix shape:** add a NOT NULL check to the shared UPDATE value-assignment path (wherever the
column's new value is finalized, before the write), not just the ON CONFLICT arm — fixing only
the upsert arm would leave the wider bug and create a new UPDATE/upsert behavioral split. Gate:
`constraint_parity_tests` extension + full suite (touches the hottest DML path — watch for perf
regression on the UPDATE fast paths, which don't currently pay this cost).

### 1.6 `purge_table_data` is not called from `create_table`

**Status:** open, but **narrower blast radius than the original framing** — see correction.
**Effort: one-line** (defense-in-depth; the common path is already safe).

**Correction.** The original framing ("stale data can survive a drop/recreate of the same table
name") reads as if `DROP TABLE t; CREATE TABLE t (...)` routinely leaks old rows. It doesn't:
`Catalog::drop_table` (`src/storage/catalog.rs:357`) already does a full `data:{table}:` prefix
delete (`397`–`433`) *and* calls `ColumnarStore::purge_table_sidecars` (`437`) to clear columnar
side-storage — a deliberate R3.3 hardening pass, per its own comment, specifically "so a
re-created table with the same name never reads stale batches." `purge_table_data`
(`src/storage/engine.rs:3836`) is a near-duplicate of that same logic (same prefix delete + same
sidecar purge), but its only caller is materialized-view refresh (`src/storage/materialized_view.rs:294`)
— it exists for a different purpose (cleaning up a table that's being repopulated without going
through `drop_table`/`create_table` at all).

The real residual gap: `Catalog::create_table` (`src/storage/catalog.rs:186`) only checks
`table_exists()` before writing fresh metadata + a zeroed counter — it never defensively purges.
If a table's *metadata* is gone (so `table_exists()` is false, `create_table` proceeds) but stray
physical `data:`/columnar keys survive for some other reason — a crash mid-`drop_table` between
the metadata-delete batch (`~421`) and the data-delete batch (`~432`), or a WAL-replay ordering
edge case — the recreated table would silently inherit those orphaned rows. This is real but
requires an *interrupted* drop, not an ordinary one.

**Fix shape:** add a `purge_table_data`-equivalent call at the top of `create_table`, after the
`table_exists` check, as defense-in-depth against exactly the interrupted-drop case. Genuinely
~3 lines. Gate: a crash-mid-drop-table-then-recreate regression test (new); cheap enough to land
alongside 1.1–1.5's batch.

### 1.7 UNIQUE self-collision guard tests "value changed", not "different row"

**Status:** open. Not data loss — a false-positive availability bug. **Effort: one-line.**

**Verified.** `enforce_unique_on_update` (`src/lib.rs:18403`), lines `18418`–`18420`: for each
PK/UNIQUE column it compares `old_value == new_value` and `continue`s (skips the check) only
when unchanged. Confirmed by its own doc comment: *"Self-collision: `unique_key_exists` probes an
ART index that still contains the row being updated, so every check is gated on the value having
actually changed."* This means a same-statement key **swap** (`UPDATE t SET email = other.email,
… WHERE id IN (a, b)` intending a two-row cycle) or a genuine cycle is wrongly rejected as a
duplicate, because the check has no way to tell "this value belongs to the row I'm updating" from
"this value belongs to some other row that happens to equal the pre-image." Confirmed present in
the v4.6.3 CHANGELOG's known-limitations list, matching the original inventory exactly.

**Fix shape:** the check needs to exclude the *row being updated* by `row_id`, not by
value-equality — i.e., `unique_key_exists` (or a sibling) needs to accept an "excluding this row"
parameter and the ART lookup needs to resolve to a row id it can compare against, not just a
boolean "does this value exist." Gate: `constraint_parity_tests` + a same-statement key-swap /
2-cycle regression case.

### 1.8 Writes inside an explicit transaction may never reach the logical WAL — **UNVERIFIED LEAD, investigate before anything else in Section 1**

**Status:** open, **unverified**. Potentially the highest-severity item in this document if it
confirms; a non-issue if the two WAL stores turn out to be distinct. **Effort: investigation
first (small), then unknown.** Do not schedule a fix until the question below is answered.

This surfaced while designing the 1.2 cascade fix and has *not* been run to ground. Recording the
evidence and the disproof condition rather than the conclusion.

**What is established:**

- Every logical-WAL append in the DML paths is gated behind `!skip_fast_paths` — `src/lib.rs:3915`
  (insert), `:4060` (insert), `:4815` (update), `:5060` (delete).
- `skip_fast_paths == true` means "inside an explicit or session transaction". Confirmed by the
  comment at `src/lib.rs:3433`–`3437`: *"Only on the autocommit-implicit path
  (`!skip_fast_paths`): explicit/session transactions commit outside any scope."*
- `src/storage/transaction.rs` contains **zero** references to `log_data_*`, `logical_wal`, or
  `wal()`. `commit()` (`:1105`) delegates to `commit_with_timestamp` (`:637`); neither emits a
  logical-WAL entry.
- Therefore: a row written inside `BEGIN … COMMIT` appears to produce **no logical-WAL record at
  all**, at any point.
- Physical durability on the primary is unaffected — that comes from the RocksDB WriteBatch at
  commit, per the comment at `src/lib.rs:4808`–`4814`.
- Replication consumes logical entries: `src/replication/logical_replication.rs:521`–`526`
  decodes `WalEntryType::Insert|Update|Delete` into `ChangeEvent`s for the standby.

**The open question that resolves this:** is the `WalEntry` stream consumed by
`src/replication/wal_replicator.rs` / `wal_store.rs` the *same* store that `log_data_*` writes to
in `src/storage/wal.rs`, or a physically separate replication log fed from RocksDB? If the same:
**a standby silently diverges from its primary for every write made inside an explicit
transaction** — which is most writes from any ORM or any client using `BEGIN`. If separate, this
item is void and should be deleted.

**How to answer it:** trace the write side of `wal_store.rs` back to its producer and compare
against `storage::wal`'s file/keyspace. This is a half-day read, no benchmarking required.

**Why it is ranked here despite being unverified:** the failure mode is silent, affects HA
correctness rather than a single query, and the cost of *checking* is trivial compared to the cost
of shipping more HA work on a false premise. If it confirms, it outranks 1.1.

---

## Section 2 — Correctness, smaller blast radius

### 2.1 `DROP INDEX` falls through to `LogicalPlan::DropTable`

**Status:** open. **Effort: small.**

**Verified**, and worse in the worst case than "wrong error." `src/sql/planner.rs`, the
`Statement::Drop` handler's `to_plan` closure (`~730`–`769`): explicit arms exist for `View`,
`Table`, `Database`, `Type`, `Sequence`; everything else — including `ObjectType::Index` — falls
into `_ => LogicalPlan::DropTable { name: self.resolve_table_ref(name), if_exists }` (line
`768`). There is no `LogicalPlan::DropIndex` variant in the planner at all. The machinery to
actually drop an index exists and still works — `Catalog::drop_index_definition`
(`src/storage/catalog.rs:623`), `ArtManager::drop_index` (`src/storage/art_manager.rs:708`), and
a `WalOperation::DropIndex` replay path (`src/storage/engine.rs:10039`–`10041`) — but it is
orphaned: `drop_index_definition`'s only caller anywhere in the crate is that WAL-replay arm, and
nothing ever *logs* a `DropIndex` WAL op from live SQL, because live SQL never produces that
plan. Ordinary case: `DROP INDEX idx_foo` looks for a *table* named `idx_foo`, doesn't find one,
errors "table does not exist" — confusing but not destructive. Worst case: a table happens to
share a name with the index the user meant to drop (plausible — naming conventions sometimes put
the base name on both), and `DROP INDEX` silently drops that table's data instead.

**Fix shape:** add `LogicalPlan::DropIndex { name, if_exists }`, wire it to the existing
`Catalog::drop_index_definition` / `ArtManager::drop_index` / WAL-log path (all still functional,
just disconnected from the parser). Gate: new DDL test, `information_schema_completion`.

### 2.2 Triggers do not survive a restart

**Status:** open. **Effort: small** (one call-site wiring) **+ verification that the persisted
format round-trips.**

**Verified.** `StorageEngine::load_triggers` (`src/storage/engine.rs:8459`) has zero callers
anywhere in the crate (`grep -rn "load_triggers("` matches only the definition). Whatever writes
trigger definitions durably, nothing reads them back on open — every trigger a user created is
silently gone after any restart (clean or crash), with no error at trigger-creation time and no
error at restart. For anyone using triggers to enforce derived-data consistency, this is a
silent, wholesale loss of enforcement, not a one-off row.

**Fix shape:** call `load_triggers()` during the same startup sequence that rebuilds ART/vector
index snapshots. Gate: `trigger`-related integration tests (verify a create-trigger-restart-fire
round trip; none currently exist per the "zero callers" finding, since nothing exercises the
reload path).

### 2.3 Five process-wide globals are not session-keyed

**Status:** open. **Effort: substantial** (each needs to migrate to the `DashMap<SessionId, _>`
pattern the codebase already uses elsewhere for this exact problem).

**Verified, five identified.** All five are bare `Arc<RwLock<_>>` / `Arc<Mutex<_>>` /
`Arc<AtomicBool>` fields on `EmbeddedDatabase`, with no session key anywhere in their type or
access pattern — contrasted directly against fields that solve the identical problem correctly
elsewhere on the same struct: `session_transactions: Arc<DashMap<SessionId, SessionTxnSlot>>`
(`515`) and `session_art_undo: Arc<DashMap<SessionId, Vec<ArtUndoOp>>>` (`622`).

1. **`savepoints: Arc<RwLock<Vec<SavepointState>>>`** (`558`) — the example given in the
   originating report. `RollbackToSavepoint` resolves by
   `savepoints.iter().rposition(|s| &s.name == name)` (`5726`–`5732`, and the same pattern at
   `12984`, `14493`) with no session filter — under concurrent sessions, `ROLLBACK TO SAVEPOINT
   x` can resolve to a different session's identically-named savepoint.
2. **`deferred_fk_checks: Arc<Mutex<Vec<PendingFkCheck>>>`** (`632`) — the queue of FK checks
   deferred by `SET CONSTRAINTS ... DEFERRED`. Cleared unconditionally
   (`self.deferred_fk_checks.lock().clear()`) at ~13 call sites across transaction
   begin/commit/rollback; one session's transaction boundary clears every session's queued
   deferred checks.
3. **`constraints_all_deferred: Arc<AtomicBool>`** (`638`) — backs `SET CONSTRAINTS ALL
   DEFERRED`. Its own doc comment says it's "reset to `false` at every transaction
   begin/commit/rollback" — for the whole process, not the session that set it.
4. **`fk_validation_mode: Arc<RwLock<FkValidationMode>>`** (`628`) — doc comment literally reads
   "Session-level FK validation mode," but the field is a single process-wide value.
5. **`fk_validation_source: Arc<RwLock<FkValidationSource>>`** (`630`) — same pattern, same
   contradiction between doc comment and implementation.

Blast radius is narrower than 1.1–1.5 (needs concurrent sessions actively using savepoints or
deferred constraints against each other), but the failure mode — one session's rollback silently
undoing a different session's work, or one session's `SET CONSTRAINTS` silently changing another
session's enforcement — is exactly the kind of thing that passes every single-connection test
suite and only shows up in production concurrency.

**Fix shape:** migrate all five to `DashMap<SessionId, _>` following the `session_transactions`
/ `session_art_undo` precedent already in the file. Gate: a new concurrent-session isolation test
exercising savepoints + `SET CONSTRAINTS DEFERRED` from two sessions simultaneously;
`transaction_integration_tests`, `savepoint_hardening_tests`.

### 2.4 No SIGTERM handler

**Status:** open. **Effort: small** (Unix-only; `tokio::signal::unix`).

**Verified.** The only shutdown signal awaited anywhere is `tokio::signal::ctrl_c()` — in
`src/main.rs:990` and `src/protocols/server_manager.rs:194`. No
`tokio::signal::unix::signal(SignalKind::terminate())` anywhere in the crate. Default Unix
signal disposition for SIGTERM with no handler installed is immediate process termination — no
`Drop` runs, no clean-shutdown path fires, none of §1.3's fix does any good. This is not
hypothetical: `heliosdb-nano stop --pid-file <f>` (`src/main.rs:1422`–`1428`) *sends SIGTERM to
the running server itself*, meaning the documented, supported way to stop a Nano server already
exercises this exact gap today. `systemctl stop` and container orchestrators (`docker stop`,
Kubernetes pod termination) all default to SIGTERM before SIGKILL — every one of those skips
clean shutdown entirely on this build.

**Fix shape:** on Unix, select over both `ctrl_c()` and `signal(SignalKind::terminate())` at both
call sites; route SIGTERM to the same shutdown path SIGINT uses. Gate: a smoke test sending
SIGTERM to a running instance and asserting the on-disk state matches what SIGINT produces
(index snapshot + counter flush both present, post-1.3-fix ordering).

### 2.5 Parameterized `INSERT ... SELECT` drops all bound parameters

**Status:** open. Loud failure, **not silent corruption** — corrects an earlier (wrong) claim
that this was a silent-corruption bug. **Effort: one-line.**

**Verified precisely.** Inside `execute_plan_with_params_inner`, the `InsertSelect` arm
(`src/lib.rs:14003`) builds its source-query executor as
`sql::Executor::with_storage(&self.storage).with_timeout(...)` (`14010`–`14011`) — no
`.with_parameters(...)` call. Immediately visible by contrast: the same function's generic
fallback arm (`14565`–`14570`) builds its executor as
`sql::Executor::with_storage(&self.storage).with_timeout(...).with_parameters(params.to_vec())`.
Any `$1` inside the `SELECT` half of `INSERT INTO t SELECT * FROM u WHERE u.col = $1` has zero
parameters bound against it and fails with "Parameter $1 not provided" — a hard error, not a
silently-wrong result. v4.1.0 already fixed a sibling gap in the same family (`$n` inside a
scalar subquery in `UPDATE ... SET`, PR #14, per `CHANGELOG.md`) but not this one.

**Fix shape:** add `.with_parameters(params.to_vec())` to the `InsertSelect` arm's executor
construction — same one-line shape as the fix already applied elsewhere in this function. Gate:
a parameterized `INSERT ... SELECT ... WHERE $1` regression test over both executor families
(the text family should already handle this — verify, don't assume).

### 2.6 Documentation defect: "most DDL is transactional"

**Status:** open. **Effort: one-line** (doc only).

**Verified as stated, plus the mechanism confirmed.** `.claude/skills/heliosdb-nano-transactions/SKILL.md:120`
reads: *"DDL inside a transaction: most DDL is transactional, but some forms commit implicitly
(server-version dependent)."* This is wrong in the direction that matters: **no DDL is
transactional.** `Catalog<'a>` (`src/storage/catalog.rs:170`–`172`) holds only `storage: &'a
StorageEngine` — no `Transaction` reference exists anywhere in the struct or any of its ~38
`self.storage.put(...)` / `self.storage.db.write(batch)` call sites (confirmed by grep across the
file). Every catalog write — `create_table`, `drop_table`, index/constraint registration — goes
straight to RocksDB, bypassing the `Transaction` staged-write-set entirely. `BEGIN; CREATE TABLE
t (...); ROLLBACK;` leaves `t` created. No test in the repo exercises this
(`tests/transaction_integration_tests.rs` has rollback tests for INSERT/UPDATE, none for DDL).

Two adjacent claims are a **different, narrower assertion and not verified false**: AGENTS.md:37
and `SKILL.md:127` both say "Multi-op `ALTER TABLE` is atomic per statement" — this is about a
single multi-clause `ALTER TABLE` statement being all-or-nothing internally, a different
mechanism from participating in an enclosing `BEGIN/COMMIT/ROLLBACK`, and nothing found while
verifying this item contradicts it. Don't conflate the two when fixing the doc.

**Fix shape:** rewrite `SKILL.md:120` to state plainly that DDL is never transactional (commits
immediately, independent of any enclosing `BEGIN`), and that schema changes inside an explicit
transaction should be treated as already-applied even on `ROLLBACK`. This doc fix has no code
dependency and should ship immediately, independent of milestone sequencing — it's actively
telling users something false about data-loss-adjacent behavior today.

---

## Section 3 — Do not touch

### 3.1 Branch GC must stay disarmed

**Status:** intentionally inert. **Do not wire this up without fixing the encoding disagreement
first.**

**Verified, and the mechanism is a three-way encoding mismatch, not the two-way one originally
described** (worth restating precisely so nobody "fixes" the wrong half). Three different
functions build or consume `bdata:` keys with three different branch-id encodings:

- `encode_branch_data_key` (`src/storage/branch.rs:1324`) and `gc_branch_data`'s deletion prefix
  (`src/storage/branch.rs:597`) both use `branch_id.to_be_bytes()` — raw big-endian bytes.
- `branch_aware_data_key` (`src/storage/engine.rs:12952`) and the actual branch-INSERT write path
  (`src/storage/engine.rs:13256`, `format!("bdata:{}:{}:{}", branch_id, table_name, row_id)`) —
  both use the branch id rendered as an **ASCII decimal string**.

These are not "the same bytes in the opposite order" (the originally-described big-endian vs.
little-endian framing) — they are two structurally different encodings (raw binary vs. text).
`branch_id = 1` as `to_be_bytes()` is 8 mostly-null bytes; as the real write path renders it, it's
the single ASCII byte `'1'`. A prefix scan built from `gc_branch_data`'s encoding will never match
a single key written by the real INSERT path.

It gets worse than a silent no-op, though: `gc_branch_data` (`src/storage/branch.rs:594`–`625`)
builds its prefix, calls `self.db.prefix_iterator(&prefix)`, and pushes **every item the iterator
yields** into `keys_to_delete` with **no `key.starts_with(&prefix)` guard** — contrast this
directly with `Catalog::drop_table`'s data-purge loop (`src/storage/catalog.rs:~415`), which
explicitly checks `if !key.starts_with(prefix_bytes) { break; }` before ever deleting. Because the
GC prefix (null bytes) sorts before the real `bdata:<ascii-digit>...` keyspace, and nothing bounds
the scan to the intended prefix, an armed GC run is not guaranteed to stay confined to garbage —
it depends entirely on how far RocksDB's `prefix_iterator` walks past the seek point, which this
code never checks.

**Currently:** confirmed harmless in production. `run_gc()` / `gc_eligible_branches()` have zero
callers anywhere outside `branch.rs` (and its own tests) — no scheduler, no CLI subcommand, no
config wiring invokes it. `auto_gc_enabled: true` in `BranchGcConfig::default()` only controls
whether a DROP enqueues a *pending*-GC entry; nothing ever drains that queue. This is disarmed by
omission, not by an explicit kill switch — a future engineer who wires a scheduler to `run_gc()`
without first fixing the encoding mismatch (and adding the missing `starts_with` guard) will hit
this immediately.

**If ever revisited:** fix the encoding disagreement first (pick one encoding, migrate both
sides, or better, have `gc_branch_data` call the identical key-construction helper the live write
path uses), add the missing prefix-bound guard, and add a test that arms GC against a
multi-branch fixture and asserts only the targeted branch's rows are gone. Not scheduled in any
milestone below; flagged here so it stays flagged.

---

## Section 4 — Performance

**Correction up front, load-bearing for how this section is sequenced:** two of the three
items in the originating inventory describe a state the shipped code has already moved past.
Treat this section as "re-baseline before allocating engineering effort," not "these are open
gaps."

### 4.1 Indexed reads vs. PostgreSQL — likely already resolved, tracking issue stale

**Status:** the described gap (Nano ~48k/s saturating vs. PostgreSQL ~100k/s) matches the
*pre-v4.0.0* state exactly, and appears to have shipped a fix in v4.0.0 (2026-07-05), 22
releases before the current v4.6.3. **Recommended effort before v5.0: none beyond re-measurement**
— see below.

**Verified as a correction.** `src/sql/normalize.rs` (1,102 lines, header comment: *"A2:
token-level literal normalization for the plan cache"*) implements exactly the proposal in
GitHub issue #5 ("token-level statement normalization for simple-query point-read plan caching")
— a byte-lexer that rewrites WHERE-predicate literals to `$1, $2, …` *before* parsing, so
repeated point-reads that differ only by literal share one cached parameterized plan. It's wired
via `try_normalized_query_with_columns` (`src/lib.rs:15049`, calling
`normalize_select_literals` at `15066`), gated by a runtime kill switch
(`NANO_DISABLE_QUERY_NORMALIZATION`, `query_normalization_enabled` at `~15038`), and its own
module comment documents a differential-oracle proof of correctness
(`sql::normalize::differential`, raw-SQL execution == normalized+parameterized execution,
row-for-row) plus `tests/query_normalization.rs` for the wired end-to-end path.

`CHANGELOG.md`'s `[4.0.0]` entry documents the result directly:
*"Indexed point-read: reversed to 1.7×–2.3× PostgreSQL (was PostgreSQL-won ~2×; Nano saturated
~48k TPS, now scales to ~172k)."* `docs/benchmarks/heliosdb-nano-vs-postgresql-2026-07-05.md`
has the full concurrency sweep: Nano v4.0.0 beats PostgreSQL 18.4 at every measured concurrency
(c=1: 14,496 vs 6,775; c=32: 168,643 vs 74,518; c=64: 172,075 vs 99,631).

**The open item is process hygiene, not engineering:** GitHub issue #5 is still `state: OPEN`
(confirmed via `gh issue view 5` at the time of writing) despite describing work that shipped.
Either close it with a pointer to `f786646`/the A2 work, or — if there's a specific residual gap
it still describes (its body calls out a "session-txn branch" plan-fresh path distinct from the
"autocommit `query_with_columns` fast-select path" it says A2 targets) — determine whether
`try_normalized_query_with_columns`'s bail conditions (`self.in_transaction() ||
self.any_session_txns() || …`, `src/lib.rs:~15055`) still leave that specific case
un-normalized, and file a narrower, accurate follow-up issue if so. **First action for v5.0:**
a fresh `bench-engines.sh` point-read sweep against current `main` (no benchmark newer than
2026-07-05 exists in `docs/benchmarks/` despite ~20 releases and 3 weeks of subsequent changes)
to confirm the win is durable before either closing the issue or committing further engineering
here.

### 4.2 COPY bulk-load — gap already narrowed substantially, residual unverified against current head

**Status:** `CHANGELOG.md`'s `[4.1.0]` entry (2026-07-06) documents COPY 100k rows at "~397 ms →
~160 ms (2.5×), near PostgreSQL parity (~115–133 ms)" via the `vmeta:` range-marker work — a
~1.2–1.4× residual gap, consistent with (if not identical to) the "~1.3–1.5×" figure in the
originating inventory. **Effort if still real: small–medium**, but re-measure first.

Same caveat as 4.1: no benchmark data more recent than 2026-07-05 exists in the repo, and COPY
touched further work after that (v4.2.0's "COPY fast path for FK/CHECK tables," "streaming COPY
decode"). **Do not schedule further COPY engineering before a fresh measurement** confirms the
gap is still where the inventory describes it, and against unconstrained (non-FK/CHECK) tables
specifically, since the FK/CHECK-table path measured separately in v4.2.0.

### 4.3 Concurrency knee at c=32→64 — unverified against current repository state; re-measure before committing effort

**Status:** open investigation, prime suspect plausible but **not corroborated by any benchmark
file in this repository**. **Effort: unknown until re-measured; if confirmed, likely substantial**
(runtime/threading model change).

The theory — `#[tokio::main]`'s worker-thread pool contending with a synchronous blocking call
inside an async task — is architecturally plausible and worth investigating, and the "ruled out
as a read-path issue because `SELECT 1` flattens identically" framing is a sound diagnostic
(§4.1's fix touches only the query/plan path, not connection/protocol handling, so it's a valid
control). But `docs/benchmarks/heliosdb-nano-vs-postgresql-2026-07-05.md`'s own `SELECT 1` sweep
(the only recent `SELECT 1` data in the repo) shows Nano scaling from ~26,000 TPS at c=1 to
~245,000 TPS at c=64 with no reported knee — the opposite of "flattens at c=32→64." That doc is
abbreviated (only c=1 and c=64 rows shown, not the full sweep), so it neither confirms nor
refutes a c=32→64 plateau specifically, but it does not corroborate the claim either.

**Verified, and a real, independent finding worth folding into this investigation regardless of
outcome:** `#[tokio::main]` (`src/main.rs:293`) uses the default multi-thread runtime with no
explicit `Builder::new_multi_thread().worker_threads(N)` override anywhere in the crate — so
whatever worker-count theory is tested, it cannot currently be tuned or A/B'd without a code
change, *despite `config.example.toml` appearing to offer exactly that knob*:
`[performance] worker_threads = 0  # Use all cores` (`config.example.toml:207`–`209`, also
documented at `451`–`452`) and `src/config.rs:988`/`1020` (`pub worker_threads: usize`, default
`num_cpus::get()`). **This config field is never read anywhere in `src/`** (`grep -rn
".worker_threads\b"` matches only its declaration and default) — it is inert. This matters
independent of whether the c=32→64 knee is real: it's a documented, user-facing config knob that
silently does nothing, which is its own small interface-coverage defect (`CLAUDE.md` gate 5) and
also means nobody can currently test the leading hypothesis for the knee without patching main.rs
directly.

**Fix shape (regardless of the knee's current reality):** wire `performance.worker_threads`
through an explicit `tokio::runtime::Builder` in `main()` (requires restructuring away from the
`#[tokio::main]` attribute macro to a hand-built runtime + `block_on`) so the knob is real and the
hypothesis becomes testable in production, not just via a source patch. Then re-run the
concurrency sweep with the knob varied before deciding whether a deeper runtime-model change is
warranted.

---

## Section 5 — Blocked on the maintainer

### 5.1 PyPI Trusted Publisher not registered — wheel queue 7 releases deep

**Status:** confirmed via `gh run list --workflow=python-wheel.yml`: `py-v4.3.0`, `py-v4.4.0`,
`py-v4.5.0`, `py-v4.6.0`, `py-v4.6.1`, `py-v4.6.2`, `py-v4.6.3` — all `completed / failure`.
**Not an engineering task.** Requires a pending Trusted Publisher registered on PyPI: project
`heliosdb-nano-embedded`, owner `HeliosDatabase`, repo `HeliosDB-Nano`, workflow
`python-wheel.yml`, environment blank. Once registered, one `gh run rerun <id> --failed` per
queued release clears the backlog. **Never re-tag** — the source and wheel content for each
tagged version is already correct; only the publish step is failing.

### 5.2 Open GitHub issues

- **#5** — "perf(reads): token-level statement normalization for simple-query point-read plan
  caching." See §4.1: appears to already be resolved by shipped work; recommend closing or
  re-scoping after the re-measurement recommended there.
- **#1** — "Feature request: PyO3 binding for `EmbeddedDatabase` (Token-Dashboard cutover)."
  Phase 1 offered per prior coordination; awaiting the requester. No roadmap action until they
  respond.

---

## Milestones

### v4.7 — Transaction- and session-boundary correctness

**Contents:** 1.2 (CASCADE/SET NULL escape transaction) · 1.3 (Drop ordering) · **1.8
(logical-WAL-in-explicit-transaction investigation — do this FIRST)** · 2.4 (SIGTERM handler) ·
2.3 (five session-unscoped globals) · 2.6 (doc fix, ships immediately regardless).

**1.8 is sequenced ahead of the rest of this milestone** even though it is unverified: it asks
whether an explicit transaction boundary is *also* a replication blind spot, which is the same
question as 1.2 and 2.3 one layer down. It is a half-day read with no benchmarking, and if it
confirms it re-ranks the entire document — so it is cheaper to answer now than to schedule more
transaction work on an unknown premise.

**Rationale:** every item here is about the same question — *what does a transaction or session
boundary actually isolate?* — and they share failure surface: 1.2 and 1.3 are already in flight
in a parallel session; 2.4 is a structural prerequisite for 1.3 to matter in any real deployment
(daemon/systemd/container shutdown all use SIGTERM, which 1.3's fix alone does nothing for); 2.3
is the same "boundary that should isolate a session but doesn't" bug class as 1.2, just for
savepoints/deferred-constraints instead of cascade actions. Landing them together means one
`transaction_integration_tests` + `savepoint_hardening_tests` + full-suite gate run covers all
four, instead of four separate gate cycles each re-touching adjacent code. 2.6 has no code
dependency and should ship the moment it's written, not wait for a milestone — listed here only
to record it's in scope.

**Gate:** full suite + `docs/GATES.md` gates 1–6 + `transaction_integration_tests` +
`savepoint_hardening_tests` + a new SIGTERM-vs-SIGINT parity smoke test + a new
concurrent-session savepoint/deferred-constraint isolation test.

### v4.8 — Write-path constraint and security parity

**Contents:** 1.1 (RLS not enforced on writes) · 1.5 (NOT NULL bypass on UPDATE/upsert) · 1.7
(UNIQUE self-collision false positive) · 2.5 (parameterized INSERT...SELECT) · 1.6
(`purge_table_data` defense-in-depth).

**Rationale:** 1.1 is the single highest-impact item in this document and deserves its own
focused milestone with dedicated adversarial review (per the merge-validation skill's two
independent reviewer passes) rather than being bundled with unrelated work — this is a security
boundary, not just a correctness bug. Its fix pattern (shared validation helper called from both
`execute_in_transaction_inner` and `execute_plan_with_params_inner`) is *exactly* the pattern the
v4.6.3 release just used for FK/UNIQUE/CHECK parity (`8352b91`) — 1.5 and 1.7 are corrections to
that same constraint-enforcement surface that v4.6.3 didn't close, so extending the same helpers
in the same release is the natural continuation, not a new investigation. 2.5 and 1.6 are
low-effort, same-file-neighborhood cleanups that ride along on the same gate run at near-zero
marginal cost.

**Gate:** full suite + `tests/constraint_parity_tests.rs` (extended) + a new
`tests/rls_write_parity_tests.rs` + `tests/protocol_tests` (psycopg — mandatory per
`docs/GATES.md` gate 2 for anything touching catalog/planner/both DML families) +
`multi_tenancy_integration`.

### v4.9 — Index/branch correctness + performance re-baseline

**Contents:** 1.4 (branch-blind UNIQUE) · 2.1 (`DROP INDEX` → `DropTable`) · 2.2 (triggers don't
survive restart) · §4's re-measurement pass (4.1, 4.2, 4.3) and, if confirmed, whatever
follow-up each produces.

**Rationale:** 1.4 needs a real design decision (branch-scoped ART vs. branch-aware scan
fallback) that shouldn't be rushed into the same milestone as the mechanical fixes in v4.8; 2.1
and 2.2 are both "a piece of real, working machinery got disconnected from its SQL entry point"
bugs — cheap, unrelated to each other in code, but natural to batch as a DDL-hygiene pass. The
performance re-baseline is sequenced last among engineering work specifically so it measures
*post*-v4.7/v4.8 changes (both touch hot DML paths; a stale baseline measured before them would
be invalid), and because two of its three items may turn out to need zero further engineering
once measured, in which case this milestone is lighter than it looks on paper.

**Gate:** full suite + `branch_data_isolation_test` + `branch_storage_test` +
`information_schema_completion` + trigger-restart regression + the full
`benches/public/ci_perf_smoke.sh` / `bench-engines.sh` / pg35 battery from `docs/GATES.md` gates
7–9, run fresh against v4.9 head before deciding v5.0's content.

### v5.0 — Release

**Contents:** whatever v4.9's performance re-baseline determines is still open (best case:
nothing, given 4.1/4.2 both appear largely resolved already), plus §5 (PyPI publishing — blocked
on the maintainer, not scheduled engineering, but its resolution gates whether v5.0's Python
wheel actually reaches users on release day).

**Release criteria (explicit):**
1. Every item in Sections 1 and 2 of this document is either shipped and gate-verified, or has
   an explicit, written decision in this file to defer it past v5.0 with a stated reason — no
   silent drops. Item 1.8 additionally must have been *resolved as a question* — confirmed and
   fixed, or disproved and deleted from this file. An unverified lead of that severity may not
   still be open at a v5.0 tag.
2. Section 3 (branch GC) remains disarmed, unchanged, and re-verified inert (`run_gc` /
   `gc_eligible_branches` still have zero non-test callers) at v5.0 tag time, unless a dedicated
   design (not part of this roadmap) fixes the encoding disagreement first.
3. Section 4's performance claims are backed by a `docs/benchmarks/` file dated within the v5.0
   release cycle — not the 2026-07-05 snapshot this document had to rely on for verification.
4. Every fix passes the full `CLAUDE.md` Quality Gates battery (test suite, no regression, perf
   gate + cumulative <3% budget, lint gates, interface coverage) — referenced, not restated, per
   this document's scope.
5. `tests/protocol_tests` (psycopg) is green for every item that touched `protocol/`, catalog, or
   either DML executor family — non-negotiable per `docs/GATES.md` gate 2's own capture (embedded
   tests structurally cannot see this class of bug).
6. PyPI publishing (§5.1) is either resolved (Trusted Publisher registered, queue cleared) or
   explicitly called out in the v5.0 release notes as a known-broken distribution channel with a
   workaround (`cargo install heliosdb-nano` / source build) — v5.0 does not ship silently
   pretending the Python wheel is current when it hasn't published since `py-v4.2.0`.
7. GitHub issue #5 is closed (with a pointer to the A2 work) or re-scoped to an accurate residual
   gap; issue #1 status is whatever the requester's response dictates, not blocking.

---

## Appendix — known flakes, explicitly not bugs (do not re-investigate)

Per `CLAUDE.md` and `docs/GATES.md`:
- `ha_tests::streaming_tests` and `lock_management` — documented pre-existing suite skips on
  constrained runners. Never add new skips without written commit-message justification; these
  two are already justified.
- The vector-index / `test_hnsw_basic` test and the dependency-download step in the release gate
  — flake; `gh run rerun --failed`, never re-tag.
