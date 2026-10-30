# B2 — writes and constraints on a branch

Plan: `docs/plans/BRANCH_ISOLATION.md` (D2, D3). Builds on B1 (the session's branch binding
and its one accessor). Sprinter 6bc4a4040eb7 items 2, 4 and the TRUNCATE data loss.

## The model this phase fixes in code

A branch `b` with ancestor chain `main → … → b` sees table `t` as: main's rows, then each
level's overlay applied in order — `bdata:{level}:{t}:{row}` replaces a row,
`bdel:{level}:{t}:{row}` hides it — where for every row the NEAREST level that has an entry
for it decides (a child's own `bdata` beats an ancestor's `bdel`; today deletes are applied
after all overlays, which hides a child's own forked row). B4 will freeze the parent's
contribution at the fork point by copying a parent's pre-image (or a tombstone) into each
live child's overlay at the moment the parent writes — so after B4 "the rows the branch
sees" is still exactly this merge, and nothing in B2 may depend on reading history.

## Facts (survey 2026-10-30; approximate lines)

- No branch point-get exists; every branch read is the full merge
  `scan_table_branch_aware_with_schema` (engine.rs ~16711-16855). Writes:
  `insert_tuple_branch_aware_with_schema` (~16871, no index claim),
  `update_tuples_branch_aware` (~16982), `delete_tuples_branch_aware` (~17162),
  in-transaction `branch_aware_data_key` (~16622). Branch rows are in no index (ART is
  process-wide by table; W2.0 comments lib.rs ~8447, ~19549; HNSW main-only).
- Constraints on a branch: the text generic INSERT checks only main's ART
  (`check_unique_constraints`, lib.rs ~8650); the text literal fast path `try_fast_insert`
  on a branch checks NOTHING (lib.rs ~15701-15712); the params family pre-checks main's ART
  (~19465) and skips the reservation on a branch (~19556). Result: duplicates of main's
  values and duplicates among branch rows insert; rows deleted/updated on the branch still
  collide falsely with main's ART entries.
- ON CONFLICT DO UPDATE on a branch: params family reads the existing row with
  `branch_aware_data_key` (~19711; errors "existing row not found" for an inherited main
  row) and writes with `update_tuple_fast` (~19818) = MAIN's `data:` key and main's ART;
  text family writes `txn.put(bdata)` (~8879) with the same read failure.
- Explicit transactions on a branch read main (`txn_base_tuples` →
  `scan_table_at_snapshot`, scan.rs ~179-205; `merge_with_write_set` matches only `data:`,
  storage/transaction.rs ~2536), so a transaction does not see its own branch writes.
- Deletes without tombstones: params in-transaction DELETE does `txn.delete(bdata)` only
  (lib.rs ~20532-20536); FK cascade likewise (~26766, ~26791). The text family writes
  `bdel` (~10229).
- TRUNCATE on a branch deletes MAIN's rows (`handle_truncate` ddl.rs ~867-930; in-txn arm
  lib.rs ~10571-10582).
- Unverified, check each: COPY FROM on a branch (`copy_bulk_insert_for_session`), the
  autocommit `INSERT … SELECT` streaming path (lib.rs ~9367-9464), bulk-load mode and
  lock-free ingestion on a branch, PK/indexed UPDATE and DELETE fast paths, vector top-k
  (`<->` ORDER BY … LIMIT) on a branch, FK existence checks on a branch, and how row ids
  are allocated for branch inserts (they must never collide with a row id main allocates
  later — confirm the counter is shared and persistent).

## What to build

1. **One branch view** (a small type in the storage layer, built from the session's binding):
   - `get(t, row)` — a point read through the chain (nearest level wins; then main).
   - `scan(t)` — the existing merge, with the nearest-level rule above.
   - `rows_with_key(t, columns, key)` — the visible rows whose `columns` equal `key`:
     candidates from main's ART (or main's rows when no ART covers the constraint),
     each kept only if visible through the chain with that key; plus every overlay row in
     the chain (prefix scans of `bdata:{level}:{t}:`) that is visible and matches. Cost
     O(overlay size) + an index probe, never a full scan of main.
   - `put(t, row, tuple)` writes `bdata:{b}:{t}:{row}` and clears `bdel:{b}:{t}:{row}`;
     `delete(t, row)` clears `bdata:{b}:{t}:{row}` and writes `bdel:{b}:{t}:{row}` (always
     a tombstone — it is what hides an inherited row).
   - Transaction-aware variants: the same operations on the transaction's write set, and
     reads that merge the write set over the view (read-your-writes).
2. **Every write path on a branch goes through it**, both DML executor families (text
   `execute_in_transaction_inner`, params `execute_plan_with_params_inner`) and their fast
   paths: INSERT (generic, literal fast path, multi-row, `INSERT … SELECT` streaming,
   RETURNING), UPDATE and DELETE (including PK/indexed fast paths), FK cascades (RESTRICT
   checks and CASCADE/SET NULL actions over the branch's view), `ON CONFLICT DO NOTHING /
   DO UPDATE` (arbitration and the read of the existing row through `rows_with_key` /
   `get`; the write through `put`), COPY FROM, and explicit transactions (both families).
   Any path you cannot route is REFUSED on a branch with a clear error before it writes
   (fail closed) — never written to main. Lock-free ingestion on a branch: refuse.
3. **TRUNCATE on a branch** empties `t` on that branch only: tombstone every row the branch
   sees (and drop the branch's own overlay rows for `t`); main and other branches
   untouched; works inside an explicit transaction (rollback restores).
4. **Constraints over the branch's view.** PRIMARY KEY and every UNIQUE constraint / unique
   index of `t`, on INSERT, UPDATE that changes a key column, ON CONFLICT arbitration and
   COPY; single- and multi-row statements, including duplicates within the statement;
   PostgreSQL semantics for NULL in UNIQUE (no conflict) and the same SQLSTATE `23505` and
   message shape as on main. After a branch deletes (or updates away) a key it inherited,
   that key is free on the branch. FOREIGN KEY on a branch: an insert/update must reference
   a row visible on the branch; RESTRICT and CASCADE act on the branch's visible rows only.
   NOT NULL / CHECK / defaults / BEFORE INSERT trigger rewrite: verify they apply on every
   branch path (including the literal fast path).
5. **Reads on a branch never use an index built from main alone** unless the result is
   filtered through the view: confirm every index/fast read path bails or filters on a
   branch, including vector top-k (exact search over the branch's rows is correct).
6. **Sequences / SERIAL / IDENTITY** stay shared with the parent (PostgreSQL sequences are
   not transactional either); state it in the docs later (B4).

## Tests (write them first; they must fail on the current tree)

New `tests/branch_write_isolation.rs`; every scenario runs on BOTH executor families
(embedded `execute` text path and `execute_params`/extended protocol) and ends by dropping
the branch and asserting main's rows are byte-for-byte what they were (count + an ordered
checksum), so no test can pass while writing main:
- PK duplicate of an inherited main row → 23505; duplicate among branch rows → 23505;
  duplicate within one multi-row INSERT → 23505; UPDATE to a duplicate key → 23505; after a
  branch DELETE of an inherited row, inserting its key on the branch succeeds; NULLs in a
  UNIQUE column do not conflict; composite UNIQUE; a TEXT primary key (prefix-key ART).
- ON CONFLICT DO UPDATE on a branch for an inherited row updates the branch only; DO
  NOTHING skips; RETURNING returns the branch values.
- Explicit transaction on a branch: INSERT then SELECT sees it; UPDATE then SELECT; ROLLBACK
  discards; COMMIT then SELECT on the branch sees it and main does not.
- DELETE inside a transaction on the params family hides the inherited row on the branch;
  an FK cascade on a branch hides the children on the branch only; RESTRICT on a branch
  refuses based on the branch's children.
- TRUNCATE on a branch: the branch's `t` is empty, main's is intact; inside a transaction,
  ROLLBACK restores the branch's rows.
- COPY FROM and `INSERT … SELECT` on a branch land on the branch only.
- Vector top-k on a branch returns the branch's rows (insert a nearer vector on the
  branch, delete an inherited one).
- A child branch's own row is not hidden by a tombstone of the same row in its parent.
Existing branch suites keep passing (list in B1), plus tests/branch_session_isolation.rs.

## Rules

Do not run cargo, git or any build. Keep both executor families consistent. No new
`unwrap`/`expect` on non-test paths. Report: files touched, the branch view's final API,
every write path and what it now does on a branch (or why it is refused), the tests and
what each proves, and anything in this spec that turned out wrong.
