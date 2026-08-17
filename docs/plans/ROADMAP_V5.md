# HeliosDB-Nano — Roadmap to v5.0

**Status:** living document. Created 2026-07-27, last updated 2026-07-28. **Current release:**
v4.7.0 (`7649a39`, crates.io verified live). **Scope:** every known outstanding item, sequenced
into milestones. **v5.0 ships when this roadmap is empty** — no item deferred without an explicit
decision recorded in this file.

**Changes since creation:** §1.2, §1.3 and §2.4 shipped in v4.7.0 (plus an unlisted fourth fix —
the detached HTTP task pinning `Arc<EmbeddedDatabase>`, without which the other two were inert
under the default `--http-port 8080`). §1.8 was promoted from unverified lead to **confirmed**
and now leads v4.8 as a hard blocker on the v5.0 tag. §2.7 and §2.8 were found while gating
v4.7.0 and added. One claim in §1.8 was **corrected** — see the correction notice in that
section.

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

### 1.1 Row-Level Security is not enforced on any write path — **SHIPPED in v4.9.0 (`bfe9115`)**

**Status:** FIXED and shipped in v4.9.0 (`bfe9115`). Enforcement lives in `RlsWriteGuard` plus
three shared helpers called from the generic Insert/InsertSelect/Update/Delete arms of BOTH executor
families; violations map to SQLSTATE 42501. Two colocated `get_rls_conditions` bugs fixed alongside
(first-policy-only instead of OR-combination; missing WITH CHECK -> USING fallback), both of which
affected reads too. Verified by a 15-test both-families parity suite AND by an independent
coordinator-authored probe: UPDATE 0 rows, INSERT Err, DELETE 0 rows, hidden row intact.

**Residual CLOSED in v4.10.0 (`399124a`).** All three listed holes fixed, plus two more found
while fixing them: `execute_params()` had the same catch-all defect as `execute()`, and reads
inside an explicit transaction applied no RLS at all (eight call sites, one root cause — the
choke-point fix closed an eighth the design had not found). The result cache leaked in BOTH
directions, measured, not just the no-context→RLS-active direction recorded here; under an active
context it is now bypassed entirely rather than tenant-keyed, because policies can reference
`current_setting()` and roles, so any hand-maintained key would be a second driftable copy of what
determines visibility. Poisoned pre-existing entries are handled by gating reads, not only writes.

**Still open, and these now gate wire exposure** (filed 2026-08-03): RLS does not walk into CTEs,
unions or table functions; scalar/correlated subqueries execute via a fresh RLS-blind `Executor`;
`protocols::adapters::executor::LiteQueryExecutorAdapter` and `protocols::oracle::handler` plan and
execute holding only `Arc<StorageEngine>` with no `TenantManager`, so they cannot apply RLS even in
principle (public library surface, not wired into `main.rs`). Also found: a latent re-entrant
deadlock in DML trigger bodies (`current_transaction` locked unconditionally with no
`global_txn_active` fast-out), and `max_qps` is a lifetime quota rather than a rate — nothing ever
resets the window, so a free-plan tenant gets 10 statements for the life of the process.

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

**CORRECTION (2026-07-29) — the blast radius above is wrong, and the direction matters.** The
claim that "every INSERT/UPDATE/DELETE through psql, MySQL, the embedded API, the extended protocol,
or `/rest/v1/` bypasses RLS" overstates the *wire* exposure and understates a different problem.
Verified at `f2c0e29`:

- **`set_current_context` has exactly ONE production caller tree-wide** — `src/repl/commands.rs:3522`.
  Every other call site is a test. No protocol handler, no HTTP handler, no wire path ever sets a
  tenant context.
- Therefore over psql / MySQL / `/rest/v1/`, `get_current_context()` is always `None`,
  `should_apply_rls` always returns false, and **RLS does nothing at all — reads included.** There is
  no cross-tenant write vulnerability over the network, because there is no RLS over the network.
- The write bypass is real and live, but reachable via the **embedded API and the REPL** — which is
  how it was demonstrated (see below).
- Separately: `current_context` is a single **process-wide** `Arc<RwLock<Option<TenantContext>>>`
  (`src/tenant/mod.rs:758`), not session-scoped. So naively wiring it to the wire would make one
  session's tenant context apply to every concurrent session — a worse bug than the one being fixed.
  **Wiring RLS to the wire is therefore blocked on session-scoping this global** (related: §2.3).

So the honest statement is two defects, not one: *the write paths do not enforce RLS* (this item), and
*the multi-tenancy feature is inert over every network protocol* (new, file separately). Fixing the
first does not make RLS safe for concurrent wire serving.

**Demonstrated live** (embedded API, policy `owner = 'alice'` with `RLSCommand::All`, tenant context
set, `should_apply_rls` true for all four commands):

```
SELECT sees 1 row(s)                    (policy-correct = 1)   correct
UPDATE bob's row -> 1 row affected      (policy-correct = 0)   BYPASS
INSERT violating WITH CHECK -> Ok(1)    (policy-correct = Err) BYPASS
DELETE bob's row -> 1 row affected      (policy-correct = 0)   BYPASS
final table: alice's row + mallory's row; bob's row deleted
```

A session that could *see* exactly one row deleted a row it could not see. The read path being
correct is what makes this dangerous — an operator verifying isolation with `SELECT` gets a clean
result.

**WHY IT SHIPPED — the RLS test suite cannot fail on this bug.** `tests/multi_tenancy_integration.rs`
has tests named for exactly this scenario, and they assert nothing:

```rust
// Try to update Tenant B's data (should fail - RLS blocks it)
let result = db.execute("UPDATE sales SET amount = 1600 WHERE id = 2");
println!("✓ UPDATE affected 0 rows for Tenant B's data (RLS protected)");
```

`result` is bound, never asserted on, and shadowed by the next `let result` so no unused-variable
warning fires — then the test prints a success checkmark unconditionally. The DELETE case
(`:354`-`355`) is identical. `tests/multi_tenancy_tests.rs` barely touches `EmbeddedDatabase` at all.
**A test that prints a checkmark instead of asserting is worse than no test: it manufactures
documented confidence in a property nobody verified.** Hardening these two files is part of this
item's scope, not optional cleanup.

**Also found, filed separately:** with a policy on `owner`, `SELECT id FROM docs` fails with
"Column 'owner' not found in schema" — the injected RLS `Filter` references a column the projection
excludes (`SELECT *` works). Fails closed, so availability not security, but RLS-enabled tables
reject ordinary projections, which is plausibly why this surface saw so little real use.

**CORRECTION (2026-08-16) — the stated cause above is wrong, and it under-states the scope.**
"The injected `Filter` references a column the projection excludes" is refuted by the `WHERE`
case: `SELECT id FROM docs WHERE id > 0` projects exactly the same single column and **works**.
The *user's* `Project` node always sits ABOVE the injected `Filter` and drops nothing the filter
needs. The real defect needs **two** mechanisms plus their ordering:

1. **`ProjectionPruningRule`** (`src/optimizer/rules.rs:494-691`) pushes a projection INTO a bare
   `Scan` — but only when a `Project{distinct:false, distinct_on:None}` is its **direct** input,
   and it leaves `Scan.schema` at full width while the scan emits pruned rows.
2. **RLS injection** (`src/lib.rs`, `apply_rls_to_plan_recursive`) wrapped the scan in a `Filter`
   **above** it, whereas its own `FilteredScan` arm merged the policy INTO the scan's predicate —
   one rule, two implementations — and the text-family pipelines apply RLS **after** the optimizer.

The bug fires exactly when (i) the plan contains `Project{distinct:false}` **directly** over
`Scan`, (ii) the pruned column set excludes a policy column, and (iii) the entry point optimizes
before applying RLS. Every escaping shape escapes structurally, not coincidentally: `WHERE`
interposes a `Filter`/`FilteredScan` so the `Project`'s input is no longer a bare `Scan` and
pruning never fires at all; `ORDER BY` puts the `Sort` between `Project` and `Scan`; `DISTINCT`
fails the rule's `distinct: false` match; aggregates interpose an `Aggregate`; and
`SELECT id, owner` *does* prune but keeps the policy column inside the projection — which is why
`tests/rls_read_parity_tests.rs`, whose workhorse read is `SELECT id, owner FROM orders`, is green
against a live bug. The params family never optimizes, so it could not exhibit this at all.

**Scope was wider than "embedded API".** The PG **simple-query** protocol diverged *with itself*:
`query_with_columns_for_session` delegates to the optimized text path in autocommit but plans
without the optimizer inside `BEGIN`, so the same statement on the same connection errored before
`BEGIN` and returned rows after it.

**Fixed (Unreleased).** The `Scan` arm now emits `FilteredScan` with the projection preserved and
the policy as the scan's own predicate — evaluated against the full base-table row before
projection — with both scan-leaf arms taking their policy from one shared `rls_read_predicate`
helper. Pinned by `tests/rls_projection_shapes_tests.rs` (shapes × both executor families ×
autocommit/`BEGIN`, asserting row counts *and* contents). Closing this also required teaching
`handle_filtered_scan` the `VERSIONS BETWEEN` branch `handle_scan` already had, so that
`FilteredScan` is a genuine drop-in superset of `Scan`.

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

### 1.8 Writes inside an explicit transaction never reach the logical WAL — **SHIPPED in v4.8.0 (`563a084`)**

**Status:** FIXED and shipped in v4.8.0 (`563a084`). `commit_with_timestamp` now collects the
transaction's operations from `write_set`/`insert_log` and emits them through a new
`WriteAheadLog::append_batch` before the RocksDB batch write — one batch, one broadcast, one
sync-wait. Verified by 13 unit tests including autocommit-vs-transaction parity and an
end-to-end convergence test, plus `ha_integration` 33/0 and a dedicated sync-ACK target.

**No longer a v5.0 blocker.** Retained here for the mechanism write-up and because the
follow-ups below are still open.

Filed on 2026-07-27 as an unverified lead; verified 2026-07-28. The original disproof condition
was "the replication `WalEntry` stream might be a physically separate log fed from RocksDB." It
is not. Both halves of the chain are now established.

**The mechanism, end to end:**

1. Every logical-WAL append in the DML paths is gated behind `!skip_fast_paths` —
   `src/lib.rs:3915` (insert), `:4060` (insert), `:4815` (update), `:5060` (delete).
   `skip_fast_paths == true` means "inside an explicit or session transaction", per the comment
   at `src/lib.rs:3433`-`3437`.
2. Transaction commit does not compensate. `Transaction::commit_with_timestamp`
   (`src/storage/transaction.rs:637`) builds a `rocksdb::WriteBatch` and writes it *directly*
   via `self.db.write_opt(batch, ...)` / `self.db.write(batch)` (`:886`-`:902`), bypassing
   `StorageEngine::put`/`delete` — and therefore bypassing `wal.append()` — entirely.
   `transaction.rs` contains no reference to `log_data_*`, `logical_wal`, or `wal()`.
3. `wal.append()` is the **sole replication broadcast point**: `WalLog::append`
   (`src/storage/wal.rs:475`) calls `broadcast_after_append` (`:486`), which calls
   `ha_state().broadcast_wal_operation(lsn, operation)` (`:498`) and then honours the
   sync/semi-sync ACK wait (`:510`). `append_nosync` deliberately broadcasts too (`:541`-`:543`,
   "the nosync path only skips the local fsync — it must NOT skip replication"), which shows the
   invariant was consciously maintained on that path and simply never considered for the
   explicit-transaction path.
4. Therefore a row written inside `BEGIN … COMMIT` produces no `WalOperation`, no LSN, and no
   broadcast. The standby never hears about it.

**Measured, not just reasoned.** A probe using the existing `wal_entries_for_tests()` harness,
counting `WalOperation` entries for one table with `wal_enabled = true` and
`logical_wal_per_statement = true`:

```
baseline (after DDL only):        insert=0 update=0 delete=0
after AUTOCOMMIT insert/update/delete:   delta 1 / 1 / 1
after EXPLICIT TXN insert/update/delete: delta 0 / 0 / 0
committed-txn row visible in table:      true
explicit-txn INSERT alone:               insert delta = 0
```

The committed transaction's data is present and queryable locally, and emits zero logical-WAL
records. (The probe was temporary and has been reverted; re-create it from this snippet.)

**Blast radius.** Local durability is NOT affected — that comes from the RocksDB WriteBatch,
synced per `sync_commit`/group-commit config. What is affected is everything fed by the logical
WAL: Tier-1 warm-standby replication, logical replication / CDC (`ChangeEvent` decoding at
`src/replication/logical_replication.rs:521`-`526`), and any consumer of the WAL entry stream.
A primary and its standby silently diverge for **every write issued inside an explicit
transaction** — which is most writes from any ORM, any `BEGIN`-wrapping client, and any
multi-statement unit of work. Nothing errors; the standby simply never receives the data. Worse,
sync/semi-sync mode gives false assurance: `wait_for_sync` is only reached from `append`, so a
transaction that broadcasts nothing also waits for nothing and returns "acknowledged".

**Why this was not caught:** autocommit single statements — what most tests and every benchmark
issue — take the `!skip_fast_paths` branch and replicate correctly. The bug is invisible unless a
test wraps writes in `BEGIN`/`COMMIT` *and* asserts on standby or WAL state. No such test exists.

**Fix shape (needs design, do not start blind):** the logical WAL needs transaction awareness —
emit the transaction's operations at commit, either by buffering them per-statement or by
replaying `write_set`/`insert_log` into the WAL at commit time. Ordering against the RocksDB
batch write matters for crash-vs-replica consistency and must be reasoned about explicitly, not
assumed.

**CORRECTION (2026-07-28):** an earlier revision of this entry claimed no `Begin`/`Commit`/`Abort`
marker mechanism exists and that adding one was likely part of this work. **That was wrong.**
`WalOperation::Begin { tx_id }` / `Commit { tx_id }` / `Abort { tx_id }` exist at
`src/storage/wal.rs:124`-`128`, with replay handling in `src/storage/engine.rs` (`:9757`, `:10189`,
`:10372`) and HA classification in `src/replication/ha_state.rs:574`-`576` mapping them to
`WalEntryType::TxBegin`/`TxCommit`/`TxRollback`. What is missing is any **producer**: a
tree-wide search finds zero production constructors — the only site that ever builds one is
`tests/wal_crash_recovery_tests.rs:298`. The markers are fully specified, fully handled on the
consuming side, and never emitted.

Two consequences. First, emitting them is **mixed-version safe** — the variants already occupy
fixed bincode indices, so an older standby deserializes them and routes them through existing
handling rather than failing. Second, emitting them is nonetheless of limited value in this
change: the standby's apply loop treats them as inert, so without also teaching the consumer to
buffer-and-apply atomically they are cosmetic. Recommendation: do not emit markers here; treat
atomic standby apply as a separate follow-up that would use them.

This is the third instance in this codebase of *machinery that exists, is wired on the consuming
side, and has no producer or caller* — alongside RLS write enforcement (§1.1, `execute_internal`
has zero callers) and trigger loading (§2.2, `load_triggers()` has zero callers). Worth treating
as a review heuristic: when a feature looks implemented, grep for who **calls** it, not just
whether it exists. (A fourth instance landed later and is the most complete example: §2.2 turned
out to be the whole trigger subsystem, not just its loader — registry, DML hooks, cascade guard and
row context all present and wired, with a planner that never populates a body.)

**Gate:** a new test asserting WAL/broadcast parity between an autocommit statement and the same
statement inside a transaction; `ha_integration` under `--features ha-tier1`; and a
primary/standby convergence test that writes inside `BEGIN`. Per `CLAUDE.md`, HA-touching changes
additionally require `cargo test --features ha-tier1 --test ha_integration`.

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

### 2.2 Triggers are not implemented at all

**Status:** documented as unimplemented in v4.10.1 (`e0a3d31`). **Effort to actually implement:
large**, and not currently scheduled.

**CORRECTION (2026-08-07).** This section previously read "Triggers do not survive a restart" and
proposed wiring `load_triggers()` at startup as a small fix. That framing was wrong in a way that
would have wasted the work: it treats persistence as the defect, which presumes triggers otherwise
*run*. They do not. Wiring `load_triggers()` would restore **registration** only, and would make
triggers look more correct while still executing nothing. Do not do it as a standalone fix.

**Measured** (`tests/trigger_unimplemented_tests.rs`, 21 unconditional tests):
`CREATE TRIGGER … EXECUTE FUNCTION f()` parses, registers, and returns Ok for every timing
(`BEFORE`/`AFTER`/`INSTEAD OF`), every event, row- and statement-level, with or without `WHEN` —
and **no trigger body ever executes**, with no error, warning, or log line. Two independent,
unconditional breaks: `Planner::create_trigger_to_plan` (`src/sql/planner.rs:5763`) hardcodes
`let body = vec![]; // will be populated in Phase 2`, so `execute_triggers`' `for stmt in
&trigger_def.body` loop is structurally unreachable; and the four DML call sites pass an
`executor_fn` that discards the `TriggerRowContext`, so `NEW`/`OLD` could not resolve even if a
body existed. The SQLite/MySQL `BEGIN … END` form does not parse at all.

Sole working mechanism: `BEFORE INSERT … FOR EACH ROW` whose function body contains literal
`NEW.<col> = <expr>` and/or `RETURN NULL` rewrites or skips the inserted row — a text scan
(`parse_trigger_assignments`), not execution. INSERT-only; no `OLD`; no side effects.
Also measured: `DROP TABLE` does **not** deregister triggers (the name stays burned for the
process), and `CALL` on a `CREATE PROCEDURE` *does* execute the body.

**QUALIFICATION (2026-08-07), second pass.** This entry ended "`CALL` on a `CREATE PROCEDURE`
*does* execute — that is the alternative to recommend", and v4.10.1 shipped that advice verbatim.
The advice stands, but it was **under-specified**, and an agent following it can easily land on a
form that errors. Two rules were stated with it: the procedure must be **`LANGUAGE sql`**
(a `LANGUAGE plpgsql` body substitutes nothing — `$p_id` errors `Invalid parameter placeholder:
$p_id. Expected format: $1, $2, etc.`, and `$1` errors `Parameter $1 not provided. Expected 1
parameters, got 0`), and the body must reference parameters with a **`$` sigil**, by name
(`$p_id`) or positionally (`$1`) — a bare parameter name fails `Column 'n' not found in schema`
in *either* language. **Rule 1 no longer applies** (§2.9 fix shape (b) landed): both languages now
bind through one shared scanner, so only the `$`-sigil rule remains. Within that rule arguments
bind correctly and a procedure is a genuine escape hatch. `CREATE FUNCTION` is not an alternative
at all; nothing can call it. See §2.9.

**If this is ever scheduled**, the work is: populate the body in the planner, thread
`TriggerRowContext` through the DML executor closures (both families — see §1.1's lesson), fix the
lock asymmetry noted in the closed trigger-deadlock item (`execute()` holds `current_transaction`
while the Insert/Update/Delete arms re-acquire it, non-reentrant `parking_lot::Mutex`), add
`pg_trigger`/`information_schema.triggers`/`relhastriggers`, and only then wire `load_triggers()`.
Rewrite `tests/trigger_unimplemented_tests.rs` rather than relaxing it — it is designed to go red
on purpose the day triggers start working.

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

### 2.7 Replication WAL directory is CWD-relative and unconfigurable — **found 2026-07-28**

**Status:** open. **Effort: small** (plumb a config/CLI parameter; the harder half is choosing a
migration-safe default). Verified.

`WalStoreConfig::default()` sets `wal_dir: PathBuf::from("./data/wal")` — a **relative** path
(`src/replication/wal_store.rs:84`). `src/main.rs:1640`, in the `"primary"` role branch, constructs
the replication WAL store with exactly that default, and a tree-wide search finds **no site that
ever sets `wal_dir` from `config.toml` or any CLI flag** — the field is unreachable from user
configuration.

Consequences on a real primary:
- The replication WAL lands wherever the server's **current working directory** happens to point.
  Started from `/` (the systemd default) it writes `/data/wal`; started from a home directory it
  writes there instead.
- `--data-dir` does **not** govern it, which is precisely where an operator would expect it to live
  and where they would look for it.
- Two primaries launched from the same working directory silently share one replication WAL
  directory.
- `init()` (`:193`) calls `create_dir_all` on that path, so the directory is created wherever the
  process happens to be — this is a write, not just a read.

This also has a test-hygiene consequence that bit this session: the HA test suite uses
`WalStoreConfig::default()`, so running `ha_integration` from the repo root writes into the repo's
own `data/` directory — the one `CLAUDE.md` forbids touching. Any fix here should also give the
tests an isolated directory.

**Fix shape:** add `wal_dir` to the replication config section and a matching CLI flag, defaulting
to a path derived from `--data-dir` rather than the process CWD. Preserve the old location as a
fallback, or document the move loudly — an existing deployment's replication WAL would otherwise
appear to vanish on upgrade. **Interface coverage (CLAUDE.md gate 5) currently fails for this
parameter**, which is itself the reason to fix it.

### 2.8 `WalStore::init()` hangs forever on a torn WAL segment — **SHIPPED in v4.9.1 (`b568e95`)**

**Status:** FIXED and shipped in v4.9.1 (`b568e95`). `load_segment_metadata` and
`load_segment_entries` now share one checksum-aware bounded scan, so the two readers can no longer
disagree about where a segment ends. Allocations are bounded by remaining file bytes and
`max_segment_size`; record count by `max_entries_per_segment`; the scan stops at the first bad
record rather than continuing into a misaligned stream. Torn segments are left on disk, not
truncated. The real artifact ships as `tests/fixtures/wal_segments/segment_000032_torn.wal` and is
asserted to recover exactly its 3 CRC-valid records promptly.

**Residual (filed, not fixed):** `WalSegmentInfo::is_complete` is still hardcoded `true` even when
the scan found the segment torn (zero readers today, so inert); a segment with zero recoverable
records still reports `end_lsn = start_lsn`, advertising one LSN it cannot serve; and
`ha_tests::streaming_tests` still writes into the repo's `data/` dir via §2.7's CWD-relative
default — this fix stops it hanging there, not writing there.

**This entry originally read "`ha_integration` hangs on this class of host" and was wrong.** It was
filed from single-target runs and generalised into a property of the host. The real cause is a
single malformed WAL segment, and the real defect is in the engine, not the test harness. The
correction matters because the original framing invited exactly the wrong response — adding a skip.

**Isolation, by bisection on the same test binary:**

| `./data/wal` contents | Result |
|---|---|
| absent (clean working directory) | **33 passed, 0 failed, 1.48s** |
| all 29 segments from the repo's `data/wal` | hangs indefinitely, killed at timeout |
| the same 29 **minus `segment_000032.wal`** | **33 passed, 0 failed, 1.74s** |

`segment_000032.wal` is 129,073 bytes; every other segment in that directory is ~3 KB. It was
produced by a test run that was SIGTERMed mid-write, so its trailing record is torn.

**Why this is a production defect and not test detritus.** `WalStore::init()`
(`src/replication/wal_store.rs:193`) scans `wal_dir` and calls `load_segment_metadata` /
`load_segment_entries` on every `.wal` file it finds. Given a segment whose final record is
truncated, that scan does not error and does not stop — it hangs. **A torn trailing record is the
normal, expected state of a write-ahead log after any unclean shutdown.** That is the entire premise
of a WAL. So:

- A primary killed by SIGKILL, OOM, or power loss mid-segment-write **will hang on startup**, with
  no error and no log line, forever.
- v4.7.0's SIGTERM handler reduces how often this is reached but cannot eliminate it — SIGKILL, OOM
  and power loss still tear segments.
- Combined with §2.7 (the directory is CWD-relative and unconfigurable), an operator would have a
  primary that will not start and no documented place to look for the cause.

**CORRECTION (2026-08-02) — the mechanism above is wrong, and the wrong fix follows from it.**
I attributed the hang to `load_segment_entries` allocating from an untrusted length
(`vec![0u8; length]` with `length` read from the file). Verified against the artifact, that is not
what happens:

- The header is `magic(4) version(4) segment_id(8) start_lsn(8) entry_count(8)`. I had read
  offset 8 as `entry_count`; it is `segment_id`. The artifact's real `entry_count` is **0**, because
  `close_segment` is the only code that ever backpatches that field — so **every** segment killed
  before close has zero there, by construction.
- `load_segment_entries` loops `for _ in 0..entry_count`, so on a torn segment **it never iterates
  at all**. The 1.88 GiB allocation I described never occurs on this path, and would terminate in
  under a second if it did.

**The real mechanism is `load_segment_metadata` (`src/replication/wal_store.rs:280`) plus the
`lsn_index` backfill in `init()` (`:236`).** `load_segment_metadata` runs its own independent,
unbounded `loop` that validates no checksums and skips each payload with
`reader.seek(SeekFrom::Current(length as i64))`. **Seeking past EOF succeeds**, so a garbage length
does not error — it simply advances the cursor. The loop therefore keeps consuming, reaches a run of
`0x78` bytes, parses them as a record header, and sets `end_lsn = 0x7878787878787878 =
8,680,820,740,569,200,760`. `init()` then executes:

```rust
for lsn in segment_info.start_lsn..=segment_info.end_lsn { index.insert(lsn, segment_info.segment_id); }
```

— **8.68 quintillion `BTreeMap` inserts.** That is the hang, and it is unbounded in wall-clock terms,
not merely slow.

Measured on the artifact (`segment_id=32`, `start_lsn=1`, `entry_count=0`): records at LSN 1–3 pass
CRC-32; **LSN 4 at offset 3101 fails checksum**; the oversized-length record lies beyond it. A
correct scan stops at LSN 4 and recovers **3** records — not the 7 an earlier revision of this entry
claimed, which counted structural well-formedness without verifying checksums.

**Consequence for the fix:** scoping it to `load_segment_entries` — which the earlier text implied —
would have left the primary unable to restart. Both readers need the same checksum-aware, bounded
scan, and they must share one implementation so they cannot disagree about where a segment ends.

**Fix shape:** `load_segment_entries` must treat a truncated or checksum-failing trailing record as
end-of-segment — recover the records before it, log a warning naming the file and offset, and
continue. It must also bound its read loop so a malformed length prefix cannot spin. A WAL reader
that cannot survive its own torn tail has the failure mode inverted: it is strictest exactly when
recovery matters most. Consider truncating the segment to the last valid record on open, as
mainstream WAL implementations do.

**Gate:** a unit test that writes a segment, truncates it mid-record, and asserts `init()` returns
promptly having recovered the intact prefix — plus the same for a corrupted checksum and for a
zero-length file.

**Operational note for this repo, 2026-07-28:** `data/wal/segment_000032.wal` in the working tree is
currently torn, which makes `cargo test --tests` from the repo root hang for everyone until it is
removed. `CLAUDE.md` forbids this session from touching `data/`, so it has been left in place and is
flagged here instead. Removing that one file restores the suite; the directory is gitignored and
holds no production data. Until then, the HA tests can be run non-destructively by executing the
test binary from any other working directory, since §2.7 makes `wal_dir` CWD-relative.

### 2.9 User-defined functions are registered but callable by nothing — **found 2026-08-07**

**Status:** open for FUNCTIONS, documented (not fixed). **Effort to make functions callable:
medium** — the executor already exists and passes its own unit test; what is missing is the wiring.
Not currently scheduled. **Fix shape (b) — `LANGUAGE plpgsql` procedure argument binding — is DONE**
(see the procedure sub-entry below); everything else in this section stands.

This is the same class of defect as §2.2 and was found by checking v4.10.1's own remediation advice.
That release told users to replace triggers with "a `CREATE PROCEDURE` invoked with `CALL`
(procedures do execute)". That advice held up — procedures execute and bind their arguments — but
verifying it meant probing the neighbouring `CREATE FUNCTION` surface, which did not. Two lessons
worth keeping, both earned here: remediation advice deserves the same verification as the defect it
remediates, and a claim confirmed on one variant is not confirmed (the procedure rule was measured
first on `LANGUAGE plpgsql` alone and written up wrongly as a result — see the correction below).

**Measured** (`tests/function_unimplemented_tests.rs`; both executor families and both protocols).
All three `CREATE FUNCTION` forms register and return Ok — `LANGUAGE plpgsql`, `LANGUAGE sql`, and
`RETURNS <t> RETURN <expr>`. Then **every invocation route fails**:

| Route | Result |
|---|---|
| `SELECT post_count(7)` | `Unknown scalar function: post_count` |
| `SELECT dbl(21)` / `SELECT public.dbl(21)` | `Unknown scalar function: dbl` / `… public.dbl` |
| `SELECT id, dbl(id) FROM posts` | `Unknown scalar function: dbl` |
| `SELECT id FROM posts WHERE dbl(id) = 2` | `Unknown scalar function: dbl` |
| `SELECT * FROM dbl(21)` | `Table 'dbl' does not exist` |
| `CALL dbl(21)` | `Procedure 'dbl' does not exist` |
| `PERFORM dbl(21)` | SQL parse error — `PERFORM` is not a statement |
| `execute_params("SELECT dbl($1)")` | `Unknown scalar function: dbl` |

Identical on the embedded API and the PostgreSQL wire. There is no silent-wrong-answer here — every
route errors loudly — but there is also no route at all, and `CREATE FUNCTION` returning Ok is the
only signal a user gets.

**Procedures, measured separately — they WORK, and are not part of this gap.** An earlier revision
of this entry claimed "arguments are never bound"; that was measured only on `LANGUAGE plpgsql` and
was wrong as a general statement. Re-probed, embedded path, in-memory, all via the custom-parser
form:

| Language | Body references param as | Result |
|---|---|---|
| `sql` | `$p_id` / `$p_name` | **Ok** — `(42, 'hello')` inserted |
| `sql` | `$1` / `$2` | **Ok** — `(7, 'seven')` inserted |
| `sql` | bare `n` | `Column 'n' not found in schema` |
| ~~`plpgsql`~~ | ~~`$p_id`~~ | ~~`Invalid parameter placeholder: $p_id. Expected format: $1, $2, etc.`~~ **FIXED — now Ok, `(7, 'x')` inserted** |
| ~~`plpgsql`~~ | ~~`$1`~~ | ~~`Parameter $1 not provided. Expected 1 parameters, got 0`~~ **FIXED — now Ok** |
| `plpgsql` | bare `n` | `Column 'n' not found in schema` (by design, unchanged) |

So the rule is now: **arguments bind in BOTH languages, referenced with a `$` sigil (by name or
positionally). A bare name never works in either language.** A zero-parameter body works, and a
body that never mentions its parameter succeeds while silently discarding the argument.

**Residual procedure gap — RESOLVED.** `LANGUAGE plpgsql` procedure bodies now interpolate
parameters. Both languages share one scanner, `src/sql/interpolate.rs`: `execute_sql_procedure`
(`src/sql/functions.rs`) resolves against the declared parameters and call arguments, and
`ExecutionContext::interpolate` (`src/sql/procedural/runtime.rs`) resolves `$<name>` against the
procedural scope and `$1..$N` against `ExecutionContext::positional_params`, immediately before each
body statement is executed (`Execute`, `SelectInto`, `ForQuery`; `ExecuteDynamic` stays verbatim,
matching PostgreSQL). The same change fixed three silent corruption modes in the `LANGUAGE sql`
path that the sequential `String::replace` implementation had — see CHANGELOG `[Unreleased]`.
`tests/procedure_interpolation_tests.rs` is the matrix; `tests/function_unimplemented_tests.rs`
flipped its two plpgsql pins to assert the new behaviour.

**Still open here:** the sigil requirement is permanent by design (a bare name must stay a column
reference); `SELECT … INTO <var>` inside a plpgsql body is still not populated
(`ProceduralStatement::SelectInto` is never constructed — the procedural parser has no `INTO`
detection, so the statement goes to the engine verbatim and the variable is never filled); the
procedural expression parser still stores raw expression TEXT rather than evaluating it, so `:=`
locals hold strings and `IF`/`WHILE` conditions never evaluate to Boolean; `execute_procedure`
still does not validate argument count; and `parse_until_semicolon` is still quote-blind, so a `;`
inside a string literal splits a plpgsql body statement.

**Introspection, measured.** `information_schema.routines`, `information_schema.parameters` and
`pg_proc` all return zero rows on the wire *with a function registered*; on the embedded path
`information_schema.routines` does not resolve at all and `pg_proc` returns no rows.

**Root cause — four independent breaks, none of them subtle:**

1. `src/sql/evaluator.rs` contains **zero references** to any function registry. Its scalar-function
   match ends in `_ => Err("Unknown scalar function: {}")` (`src/sql/evaluator.rs:1154`), so no
   expression on any path — select list, `WHERE`, projection, params family — can resolve a user
   function.
2. `FunctionRegistry::execute_function` (`src/sql/functions.rs:190`) has **exactly one call site in
   `src/`, and it is inside `mod tests`** (`src/sql/functions.rs:603`; `#[cfg(test)]` begins at
   line 462). The executor works in its unit test and is never reached in production. This is the
   §1.1 "grep for CALLERS not definitions" lesson again: the feature has an implementation, a
   registry, a WAL op, and a green unit test, and is not connected to anything.
3. `Planner::is_table_function` (`src/sql/planner.rs:2078`) is
   `matches!(name, "generate_series" | "unnest")` — a fixed whitelist — so `SELECT * FROM my_udf()`
   cannot resolve a user function regardless of (1) and (2).
4. ~~(Procedures only, and narrow.) `LANGUAGE plpgsql` procedure parameters are declared into the
   procedural scope but never interpolated into the body's SQL.~~ **FIXED** — `Execute` now calls
   `ExecutionContext::interpolate` before handing the statement to the executor closure. Note the
   contrast that defines this whole entry: `CALL` works because `execute_procedure` *does* have a
   real call site (`execute_call_plan`, `src/lib.rs:3184` — see §2.11, which is where that call
   site had to be *shared* before both executor families could reach it) — `execute_function` does
   not, which is the entire difference
   between a procedural surface that ships working and one that ships dead.

`query_information_schema_routines` (`src/protocol/postgres/catalog.rs:2398`) returns
`(schema, vec![])` by construction; its own doc comment concedes Nano "does not currently expose its
runtime function catalog through this view".

**Fix shape**, in increasing order of cost — do not do (c) before (a):

  a. **Scalar calls.** Give `Evaluator` a handle to the `FunctionRegistry` and, in the
     `_ =>` arm at `evaluator.rs:1154`, look the name up before erroring. `execute_function` already
     handles arity validation, `LANGUAGE sql` vs `LANGUAGE plpgsql`, and defaults/`OUT` modes. The
     hard part is the `sql_executor` closure — the evaluator has no database handle today, and
     `clone_for_trigger` + closure is how the `CALL` path solves it (`src/lib.rs:5556`). Volatility
     and the result cache interact: a `VOLATILE` function must not be cached.
  b. ~~**`LANGUAGE plpgsql` procedure argument binding**~~ — **DONE.** Unified as directed: one
     scanner in `src/sql/interpolate.rs`, called from `execute_sql_procedure` /
     `execute_sql_function` and from `ExecutionContext::interpolate`
     (`src/sql/procedural/runtime.rs`). No third mechanism. Binding stayed TEXTUAL (values render
     through `value_to_sql_literal`); moving to real bind parameters through `sql_executor` is a
     follow-up that would reuse the same scanner, emitting `$1..$k` plus a value vector instead of
     literals.
  c. **Table functions** (`SELECT * FROM f()`) and `RETURNS TABLE`, which need a planner change at
     `planner.rs:2078` beyond the whitelist.
  d. **Introspection**: populate `information_schema.routines` / `parameters` and `pg_proc` from the
     registry. Cheap, independent of (a)–(c), and worth doing even if the rest is deferred — today a
     registered routine is undiscoverable.

**Gate:** rewrite `tests/function_unimplemented_tests.rs` rather than relaxing it — like
`trigger_unimplemented_tests.rs`, it is designed to go red on purpose the day functions start
working. Also update `README.md`, `AGENTS.md`, `docs/llms.txt`,
`docs/compatibility/plpgsql.md`, `docs/compatibility/information_schema.md`, and the
`heliosdb-nano-schema` (Recipe 6) / `heliosdb-nano-migrate` skills, all of which now document this
as unimplemented.

*Gate status for fix shape (b), done:* the two plpgsql pins in `tests/function_unimplemented_tests.rs`
were rewritten to assert binding (not relaxed), its header prose was corrected, and `README.md`,
`AGENTS.md`, `docs/llms.txt`, `docs/compatibility/plpgsql.md` and the `heliosdb-nano-schema` skill
now state the one-rule (sigil) form. `docs/compatibility/information_schema.md` and the
`heliosdb-nano-migrate` skill were NOT touched — they document the function/introspection half,
which is unchanged. The rest of this gate applies to fix shapes (a)/(c)/(d) and is still pending.

**Pre-existing test-suite note:** `tests/plpgsql_hardening_tests.rs` covers this area in the
`is_ok()`-guarded style that §2.2's cleanup removed for triggers — `test_function_in_select_scalar`
(`:255`) and `test_plpgsql_return_from_function` (`:493`) both wrap the assertion in a
`match { Ok => assert…, Err =>
eprintln!("[KNOWN LIMITATION] …") }` and always take the `Err` arm. They are not wrong (the file
header states "User-defined functions are NOT callable from SELECT expressions"), but they provide
no regression protection. Left in place deliberately: unlike the deleted trigger files they do not
*claim* the feature works, and several of their procedure tests do assert unconditionally.

---

### 2.10 `information_schema` / `pg_catalog` introspection is largely unpopulated — **found 2026-08-12**

**Status:** open, documented (not fixed) in v4.11.0+. **Effort: medium**, and separable — each
view is independent, so this can be taken in slices rather than as one project.

Documented honestly in `docs/compatibility/information_schema.md`; this entry is the *functional*
follow-up. The doc previously marked most of these "Complete".

**Measured over the PG wire** against a database that had base tables, a view, a view-on-a-view,
a `CHECK` constraint, a foreign key, a registered function and an executed `GRANT` — i.e. data
that should have populated every one. Of 20 documented views, 6 return rows, 13 are always empty,
and 1 does not exist:

| Result | Views |
|---|---|
| Populated | `tables`, `columns`, `schemata`, `key_column_usage`, `table_constraints`, `referential_constraints` |
| **Always empty** | `views`, `view_table_usage`, `view_column_usage`, `check_constraints`, `constraint_column_usage`, `routines`, `parameters`, `character_sets`, `collations`, `table_privileges`, `column_privileges`, `role_table_grants`, `role_column_grants` |
| **Not implemented** | `catalog_name` (raises the unknown-view error, like a typo) |

`pg_catalog` is not a fallback: `pg_views` returns zero rows with a view defined and `pg_proc`
zero with a function registered. `information_schema.tables` lists base tables only — no
`table_type = 'VIEW'` row — so a client enumerating relations through it misses every view.

**Why it matters:** an empty result is indistinguishable from "you have no views / constraints /
routines". ORMs and migration tools that introspect before acting will conclude the schema is
emptier than it is. This is the failure mode the strict-unknown-view behaviour (§ doc) was
introduced to prevent, reintroduced one layer down.

**Suggested slices, cheapest first and independently shippable:**
  a. **`views` + `pg_views`** — the view definitions already exist in the catalog; this is the
     most-requested of the set and the only way to recover a view's SQL text (today `\d` in the
     REPL is the sole surface).
  b. **`check_constraints` + `constraint_column_usage`** — `table_constraints` already reports a
     `CHECK` row, so existence is known; only `check_clause` and the column mapping are missing.
  c. **`routines` / `parameters` / `pg_proc`** — same item as §2.9(d); the registry has the data.
  d. **`character_sets` / `collations`** — constant single-row content, effectively free.
  e. **`catalog_name`** — a single-row view returning the current database name.
  f. **The four privilege views** — the largest, and arguably should stay empty until `CREATE ROLE`
     is supported at all (it currently returns "Statement not yet supported: CreateRole" while
     `GRANT` returns Ok, which is its own inconsistency worth resolving first).

**Gate:** `tests/information_schema_completion.rs` pins the current state deliberately —
`always_empty_views_stay_empty_even_with_the_objects_they_describe_present` will go red on purpose
when any slice lands. Move the view from that list to `populated_views_do_return_rows`, and update
both `docs/compatibility/information_schema.md`'s Status column and the unknown-view error text in
`src/protocol/postgres/catalog.rs`, which enumerates the always-empty set.

### 2.11 `CALL` was a silent no-op in the params executor family — **found 2026-08-15, FIXED; transaction semantics still open**

**Status:** the silent no-op is FIXED (see below). What remains open is the *transaction* half:
a procedure body does not join its caller's transaction, and `CALL` inside an embedded/REPL
`BEGIN` is refused rather than run. **Effort for the residual: medium** — it means changing how
body statements are executed, not where `CALL` is dispatched. Not currently scheduled.

Same class as §2.9 and §2.2, and found the same way: by checking v4.10.1 / v4.10.2 / v4.11.0's own
remediation advice, which tells users to replace the unimplemented triggers with "a
`CREATE PROCEDURE` invoked with `CALL`". §2.9 verified that advice — but only on the embedded
`db.execute()` path. It was inert for exactly the clients most likely to follow it. The lesson
from §1.1 recurs verbatim: **this codebase has two DML executor families, and a feature verified
in one is not verified.** That is now five instances (constraint checks, RLS writes, the
`execute_params` RLS leak, WAL readers, and this).

**Measured** (`tests/call_parity_tests.rs`), embedded API, in-memory, before the fix:

| statement | text family (`db.execute()`) | params family (`db.execute_params()`) |
|---|---|---|
| `CALL p0()` (no args) | Ok(0), row inserted | **Ok(1), NO row inserted** |
| `CALL p1($1)` | n/a (no bind values) | **Ok(1), NO row inserted** |
| `CALL nonexistent_proc()` | `Err: Procedure 'nonexistent_proc' does not exist` | **Ok(1)** |

Not merely a no-op: `rows_affected = 1` actively claimed work was done, and the existence of the
procedure was never checked. The affected population is the params family — the PostgreSQL
**extended** protocol (psycopg with server-side bind, JDBC, sqlx, Drizzle, node-postgres), every
REST/BaaS write, and trigger bodies via `execute_plan_internal`.

**Root cause — one missing match arm and one lying stub:**

1. `execute_plan_with_params_inner` (`src/lib.rs:13894`) had **no `LogicalPlan::Call` arm**. `CALL`
   fell to its catch-all (`src/lib.rs:~15040`), which builds a `sql::Executor`.
2. `sql::Executor` holds **no `FunctionRegistry` handle**, so its `Call` arm
   (`src/sql/executor/mod.rs:4223`) could only do what it did:
   `StatusMessageOperator::new(format!("Procedure '{}' called with {} arguments", …))` — a
   one-row success. Its own comment read *"For now, return a status message. Full procedure
   execution will be implemented later."* The `results.len()` of that one status row is where the
   `rows_affected = 1` came from.

**Fix shape (SHIPPED).** Extract the text family's arm into ONE shared private helper,
`EmbeddedDatabase::execute_call_plan` (`src/lib.rs:3184`), and dispatch to it from both families'
`Call` arms (`src/lib.rs:5684` text, `src/lib.rs:15022` params) — a single choke point, not a
copy. `params` are threaded in so `CALL p($1)` binds a server-side bound argument; the text family
passes an empty slice, which reproduces its previous evaluator exactly. The `Executor` stub now
returns an `Err` naming the procedure and stating the body was NOT run, so any remaining route
into it (notably `query("CALL …")`) fails loudly. **`rows_affected` is 0 in both families** —
PostgreSQL's `CALL` command tag carries no row count, and 0 is what the text family always
returned.

**Residual, still open — the transaction half.** `execute_call_plan` runs body statements on a
`clone_for_trigger()` handle through `execute()` / `query()`, ignoring the `&storage::Transaction`
its caller holds (`txn` in the text family, `session_txn` in the params family). Two consequences,
both pinned in `tests/call_parity_tests.rs`:

- **A procedure body does not join its caller's transaction.** Under a wire `BEGIN` (per-session
  transaction) or the embedded RAII `db.begin_transaction()` handle, body writes autocommit and
  survive a `ROLLBACK` of the enclosing transaction. Long-standing; not introduced here.
- **`CALL` inside an embedded/REPL `BEGIN` is refused, not run.** Re-entering `execute()` re-takes
  the process-wide `current_transaction` `parking_lot::Mutex`, which is not reentrant — this
  combination previously **hung the calling thread**, and a trigger body containing `CALL` would
  have become a second hang once the params arm went live. A `GLOBAL_TXN_LOCK_HELD` thread-local
  (`src/lib.rs:492`, set at `src/lib.rs:7351`) marks the window in which `execute()` holds that
  guard across a statement, and `execute_call_plan` returns an error instead. A loud error beats a
  hang. The PG/MySQL wire is unaffected: `execute_for_session` routes `BEGIN` to
  `handle_transaction_control_for_session`, which never populates the global slot.

The real fix for both is to execute body statements against the caller's transaction — thread the
`&Transaction` into `execute_call_plan` and use `execute_in_transaction_no_fast_path` instead of
re-entering `execute()`. That removes the re-entrancy entirely (making the thread-local gate
deletable) and gives `CALL` PostgreSQL's atomicity. It is a *visibility* change, so it needs the
same care as the identical latent issue the params catch-all's comment documents at
`src/lib.rs:~15040`, and it must be measured for the wire-session path, whose in-session arm holds
a `RwLockReadGuard` on the session-transaction slot across the call (`src/lib.rs:17043`).

**Gate:** `tests/call_parity_tests.rs` — every case runs the same logical statement through BOTH
families and asserts they agree, unconditionally. The two `known_gap_*` tests pin the transaction
behaviour above; **delete them** when this residual lands, and replace them with assertions that a
body runs and joins its caller's transaction in both families. `tests/function_unimplemented_tests.rs`
covers the procedure-argument-binding rules and still passes unchanged.

---

### 2.12 Two copies of the `Value` → SQL-literal renderer, each broken for the other's types — **found 2026-08-16, FIXED**

**Status:** FIXED — both defects, plus the merge. What remains open are three smaller gaps in
the same area, listed at the end; none is on a measured path. **Effort for the residual: small.**

Same class as §2.9 and §2.11, and it is now the third instance in three releases: **one rule,
several implementations.** v4.11.0 shipped the interpolation bug; v4.12.0 shipped the `CALL`
no-op; this is the renderer those two features both depend on. The lesson recurs verbatim —
*a rule verified in one implementation is not verified.*

**Measured** (embedded API, in-memory), before the fix:

| path | value | rendered literal | result |
|---|---|---|---|
| `CALL p($1)` → `LANGUAGE sql` body | `Timestamp` | `'2026-08-16 01:11:00 UTC'` | `Err: Cannot cast … to TIMESTAMP: trailing input` |
| `substitute_parameters` → `execute` | `Date` | `'''2026-08-16'''` | `Err: Cannot cast ''2026-08-16'' to DATE` |
| `substitute_parameters` → `execute` | `Time` | `'''01:11:00'''` | `Err: Cannot cast ''01:11:00'' to TIME` |
| `substitute_parameters` → `execute` | `Uuid` | `'''0000…'''` | `Err: Cannot cast ''0000…'' to UUID` |
| either | `Vector` | `[1.5,2.5]` (routine) / `ARRAY[1.5,2.5]` (wire) | not SQL / re-parses as an ARRAY |

Everything else round-tripped: int4, text (quote doubling preserved), bool, float8, numeric,
date, time, uuid, json, null through the routine-body copy; int4, text, bool, json, timestamp
through the wire copy.

**Root cause — two functions, two different mistakes:**

1. `src/sql/interpolate.rs:313` (pre-fix) — `Value::Timestamp(ts) => format!("'{}'", ts)`. Rust's
   `Display for DateTime<Utc>` appends a timezone **name** (`… 01:11:00 UTC`). Nano's TIMESTAMP
   cast (`src/sql/evaluator.rs:5624`) accepts an offset but not a name, so it errored with
   "trailing input". Reachable from every client, on exactly the routine paths v4.11.0 and
   v4.12.0 enabled.
2. `src/protocol/postgres/prepared.rs:537` (pre-fix) — a private FORK of the same function whose
   catch-all was `_ => format!("'{}'", value.to_string().replace('\'', "''"))`. That routes
   through `impl Display for Value` (`src/types.rs:293-355`), which **already emits its own
   quotes** for `String` / `Json` / `Uuid` / `Timestamp` / `Date` / `Time`. Every variant that
   reached the catch-all — Date, Time, Uuid, Numeric, Bytes, Interval, Array and the three
   storage-ref markers — came out double-wrapped.

**Blast radius, stated precisely** (the fork's is narrower than the error messages suggest, and
the correction matters for anyone triaging from this entry): the fork's only in-crate caller is
`substitute_parameters` (`prepared.rs:493`), called from `handler_extended.rs:302`, and that
call site feeds the **regex-driven catalog dispatcher only** — real execution threads the
`Value`s through `query_params` / `execute_params` (see the comment at
`handler_extended.rs:256-272`, which is where the substituted-then-executed path was removed).
Additionally, `decode_parameter` (`prepared.rs:303`) never produces `Date`, `Time`, `Numeric`,
`Interval`, `Array` or `Vector` from the wire. So the wire-reachable case was a **binary UUID
(OID 2950) parameter in a `pg_catalog` / `information_schema` probe**, which mis-filtered
silently rather than erroring. The full breakage was reachable through the public library API
`heliosdb_nano::protocol::postgres::prepared::substitute_parameters`. Defect 1 (Timestamp) has
no such narrowing: it is on a real user path for every client.

**Fix shape (SHIPPED).** ONE renderer, in `src/sql/interpolate.rs`; the fork is deleted and
`prepared.rs` imports it. The shared function is **exhaustive over `Value` with no `_` arm**, so
a new variant is a compile error instead of a silently-wrong rendering — that is what stops this
recurring, not the individual arm fixes. Its documented contract is *a rendering must be valid
SQL that re-parses to an equal `Value`*, which is strictly stronger than what the catalog
dispatcher needs, which is why one function can serve both consumers rather than needing a
mode parameter. Per-type decisions: `Timestamp` → `to_rfc3339()`, no cast; `Json` → bare
quoted (the fork's `::jsonb` existed for a substituted-then-executed path that no longer
exists); `Vector` → pgvector's `'[1,2]'::vector` (the only form that round-trips); `Numeric` →
bare when it is a plain decimal, `'NaN'::numeric` otherwise (it is String-backed and could
splice a bare identifier); `Bytes` → `E'\xdead'`.

**Residual, still open — three same-class gaps, all newly documented, none measured:**

- a non-finite `Float4`/`Float8` renders as Rust's `NaN` / `inf` / `-inf`, which SQL reads as an
  identifier. Fails loudly ("Column 'NaN' not found"), does not corrupt. Fixing it needs a
  verified cast spelling for the float types, which is why it was left separable;
- `cast_value` (`src/sql/evaluator.rs:5262`) has **no `DataType::Interval` arm**, and INSERT
  casts every value to its column type, so an `INTERVAL` column cannot be written at all —
  independent of rendering;
- a plain-decimal `Value::Numeric` re-parses through `f64` (`Planner::number_literal_to_value`,
  `src/sql/planner.rs:4150`), losing precision beyond ~17 significant digits. Quoting every
  `Numeric` would fix it but would stop `ARRAY[<numeric>…]` elements parsing as numbers.

**Gate:** `tests/value_rendering_tests.rs` — a pinned per-variant rendering table, EVERY `Value`
variant round-tripped through BOTH consumers (`CALL p($1)` into a `LANGUAGE sql` body via
`execute_params`, and `substitute_parameters` then `execute`), the measured failures above as
named regression tests, `the_two_renderers_are_one_function` (byte-identical output from both
entry points — it fails if a fork is reintroduced), and
`no_rendering_ever_produces_a_doubled_leading_quote` (the *shape* of the fork's bug, asserted
over every variant). Every assertion is unconditional. `tests/procedure_interpolation_tests.rs`,
`tests/postgres_extended_protocol_tests.rs` and `tests/drizzle_compat_tests.rs` cover the
unchanged int/text/null renderings and pass without modification.

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

**Contents:** ~~1.2 (CASCADE/SET NULL escape transaction)~~ · ~~1.3 (Drop ordering)~~ ·
~~2.4 (SIGTERM handler)~~ — **all three shipped in v4.7.0 (`1c0eaf5`)**, along with an unlisted
fourth fix (the detached HTTP task pinning `Arc<EmbeddedDatabase>`, without which 1.3 and 2.4
were inert under the default `--http-port 8080`).

**Still open in this milestone:** 2.3 (five session-unscoped globals) · 2.6 (doc fix, ships
immediately regardless).

**1.8 has been promoted OUT of this milestone.** It was listed here as an investigation to run
first; it was run on 2026-07-28 and **confirmed**. It is no longer a half-day read — it is a
substantial design task (transaction-aware logical WAL), and it is now the highest-severity open
item in this document. It
should lead v4.8 rather than trail v4.7, and it must not be bundled with unrelated work. See
§1.8 for the measured evidence.

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

### v4.8 — Replication integrity, then write-path constraint and security parity

**Contents:** **1.8 (explicit transactions emit no logical-WAL records — leads this milestone)** ·
1.1 (RLS not enforced on writes) · 1.5 (NOT NULL bypass on UPDATE/upsert) · 1.7
(UNIQUE self-collision false positive) · 2.5 (parameterized INSERT...SELECT) · 1.6
(`purge_table_data` defense-in-depth).

**On sequencing 1.8 first:** it is now the highest-severity confirmed item in this document, and
unlike 1.1 it has no workaround available to a user — an operator cannot avoid it by configuration
or by careful SQL, because wrapping writes in a transaction is the *correct* thing for a client to
do. It also has a false-assurance property that 1.1 lacks: synchronous replication mode reports
success for transactions it never shipped, because `wait_for_sync` is only reachable from the
`append` path that a transaction never takes. An operator running sync replication today believes
they have a guarantee they do not have. If capacity forces a split, 1.8 should ship alone and the
constraint items slide to v4.9.

**Rationale for the rest:** 1.1 is the highest-impact item on the *query* surface and deserves its own
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

**Contents:** 1.4 (branch-blind UNIQUE) · 2.1 (`DROP INDEX` → `DropTable`) · ~~2.2~~ · §4's
re-measurement pass (4.1, 4.2, 4.3) and, if confirmed, whatever follow-up each produces.

**2.2 was dropped from this milestone.** It was scoped here as a cheap one-call-site fix on the
premise that only trigger *persistence* was broken. Investigation found the entire trigger
subsystem never executes, so the small fix would have been wasted work on a dead feature. v4.10.1
documents triggers as unimplemented and replaces the test suite; actually implementing them is a
large, unscheduled item. See §2.2.

**Rationale:** 1.4 needs a real design decision (branch-scoped ART vs. branch-aware scan
fallback) that shouldn't be rushed into the same milestone as the mechanical fixes in v4.8; 2.1
was a "a piece of real, working machinery got disconnected from its SQL entry point" bug — cheap,
and natural to fold into a DDL-hygiene pass. The
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
   silent drops. Item 1.8 is confirmed as of 2026-07-28 and is a **hard blocker**: v5.0 must not
   be tagged while explicit transactions are invisible to the logical WAL, because every HA and
   CDC claim the project makes is false for transactional write traffic until it is fixed.
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
