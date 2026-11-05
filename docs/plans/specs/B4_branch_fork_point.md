# B4 — a branch is a fork; dropping reclaims; docs

Plan: `docs/plans/BRANCH_ISOLATION.md` (D5, D6, D7). Builds on B1-B3. Sprinter 6bc4a4040eb7
items 4, 5, 6 and the doc drift.

## Promise being delivered

Skill Recipe 2: `CREATE DATABASE BRANCH rewind FROM main AS OF TIMESTAMP '…09:00'` — "the
branch sees data as it was at 09:00; live writes to main don't appear here". A branch is a
fork: what its parent does after the fork point does not show through, whether the fork
point is an `AS OF` anchor in the past or the creation time (`AS OF NOW`, or no `AS OF`).
"Drop branches you no longer need" must reclaim space.

## Design: copy-on-write at the parent's write, not history reads

Reads stay what B2 defined (main's current rows ⊕ the chain's overlays, nearest level
wins). Isolation from the parent comes from the WRITE side: before a parent level `P`
(main, or a branch) changes something a live child `C` of `P` sees, the pre-image is
frozen into `C`'s own overlay, unless `C` already has its own version of it. This keeps
branch reads cheap, keeps B2's index-backed constraint checks valid, and does not depend on
version history after the fork. Facts that make it possible (verified 2026-10-30): every
engine write goes through `WriteGate::{write, put, delete}` (storage/write_gate.rs; no
direct `self.db` writes remain in engine.rs); DROP TABLE stages one delete per row
(`stage_prefix_delete`, catalog.rs ~2117) — no range deletes on the SQL path; branch
inserts take row ids from the table's shared counter (`next_row_id_volatile`), so ids
never collide across levels; stored values are sealed per storage key
(`tde::open_ref(km, storage_key, raw)`), so a frozen copy must be decoded with its source
key and re-sealed for its destination key — never copied as raw bytes.

## What to build

1. **Live-children registry.** In memory: parent id (main included) → the ACTIVE child
   branch ids, plus one global atomic "some level has live children". Rebuilt at open from
   the branch registry; updated by create, drop and merge.
2. **The freeze hook in the write gate.** A hook the engine registers with its
   `WriteGate`, run on every `write`/`put`/`delete` before the batch is applied when the
   global atomic is set (otherwise one atomic load and nothing else). For each key in the
   batch (use RocksDB's WriteBatch iteration; turn single put/delete into a batch while the
   hook is active):
   - a row of level `P`: `data:{t}:{r}` (P = main) or `bdata:{P}:{t}:{r}` /
     `bdel:{P}:{t}:{r}`;
   - a table definition of level `P`: any main catalog key of table `t` (`meta:table:{t}`,
     `table_constraints:{t}`, `constraint:{t}:…`, and the other per-table catalog keys) or
     `bmeta:{P}:table:{t}`.
   For each live child `C` of `P` that has no entry of its own for that row / table (no
   `bdata`/`bdel`/`bfrz` for the row; no `bmeta` for the table; `t` not isolated or
   tombstoned at `C`), add to the SAME batch:
   - a row: `P`'s view of `(t, r)` read BEFORE the batch applies (main: the current
     `data:` value; a branch: the nearest level in `P`'s chain) → `bdata:{C}:{t}:{r}` =
     that tuple re-sealed for the new key; no visible row → `bdel:{C}:{t}:{r}`; and in both
     cases the marker `bfrz:{C}:{t}:{r}` ("frozen copy, not a change made on C");
   - a table definition: `P`'s current definition of `t` → `bmeta:{C}:table:{t}` marked
     frozen; `t` not visible on `P` before the batch (a table `P` is creating) → a frozen
     tombstone.
   Ordering rule (concurrent writers): read `P`'s pre-image first, then `C`'s own entries;
   the frozen copy is written in the same atomic batch as `P`'s change. Keys the hook
   writes (`bdata`/`bdel`/`bfrz`/`bmeta` of `C`) are themselves writes of level `C`, so
   `C`'s live children are frozen in turn (recursion bounded by the chain depth).
   Keys the hook ignores: indexes, versions, counters, sequences, everything else.
3. **A child's own write clears the marker.** When `C` writes `(t, r)` (or its table
   definition), delete `bfrz:{C}:{t}:{r}` (or the frozen flag) in the same batch: it is
   now a change made on `C`.
4. **Creation is linearized.** `CREATE BRANCH` registers the child (set the atomic,
   SeqCst), then waits for every write that was in flight before the registration (the
   gate's in-flight drain used by P5.1 fencing), so no write that commits after creation
   escapes the hook. The fork point recorded in `BranchMetadata` is taken after the drain.
5. **`AS OF TIMESTAMP` in the past.** After registering (so later writes are frozen),
   materialize the difference between the anchor and the fork into the new branch's
   overlay, table by table, from version history: a row whose value at the anchor differs
   from its current value → the anchor value (frozen); a row created after the anchor →
   frozen tombstone; a row that existed at the anchor and was deleted since → the anchor
   value (frozen). This needs to know WHEN a row was deleted: from now on, a main DELETE
   records a delete event in the version index at its timestamp (a tombstone version),
   and the snapshot read path treats it as "absent from this time"; rows deleted before
   this release have no event and are treated as deleted before any anchor (today's
   behaviour). `time_travel_enabled = false`, or an anchor below the version-GC watermark:
   refuse `AS OF TIMESTAMP` (keep the existing error); `AS OF NOW` / no `AS OF` never needs
   history. Table definitions: the branch gets the parent's CURRENT definition (as `AS OF`
   queries are shaped today, W3.5); document that schema changes between the anchor and
   the fork are visible. Also make `SELECT … AS OF TIMESTAMP` on main use the same row
   universe, so it returns rows deleted after the timestamp (today it never does) — add a
   test.
6. **Version GC no longer pinned by branches.** A branch needs no history after creation,
   so drop the per-branch anchor pin of the version-GC horizon (storage/version_gc.rs
   ~343-356), except while an `AS OF` materialization runs.
7. **MERGE skips frozen copies.** `MERGE BRANCH c INTO p` copies only `c`'s own changes:
   `bdata`/`bdel` without a `bfrz` marker, and `bmeta` that is not frozen. Frozen entries
   are not changes and must never revert the target. Rows stay last-writer-wins
   (documented). After a successful merge the source's overlay and catalog are reclaimed
   (8) — it can no longer be bound (B1).
8. **Reclaim.** `DROP BRANCH` (and a completed merge): mark the branch `Dropped`/`Merged`
   first, wait for in-flight writes (the drain), then delete every key of the branch —
   prefixes `bdata:{id}:`, `bdel:{id}:`, `bfrz:{id}:`, `bv:{id}:`, `bmeta:{id}:` (decimal
   id followed by `:` — match exactly, `1:` must not match `12:`) — in bounded batches. A
   crash mid-reclaim: at open, finish the reclaim of every `Dropped`/`Merged` branch that
   still has keys. Replace the never-called binary-format GC (`BranchManager::run_gc`,
   `gc_branch_data`, `branch:pending_gc`) with this; remove or redirect the dead binary
   merge called from `git_integration/webhooks/mod.rs` ~664 to the real merge.
9. **Docs** (D7): README branch section, `.claude/skills/heliosdb-nano-branches/SKILL.md`,
   AGENTS.md's branch line, `.claude/skills/_index/verb-map.md`, `docs/llms.txt` if it
   covers branches, and CHANGELOG `[Unreleased]`: per-session branches (`USE BRANCH`,
   `SET heliosdb.branch`, startup `dbname=<db>@<branch>` and `options=-c
   heliosdb.branch=…`; note that HeliosProxy's `hba` and tenant rules do not yet match
   `db@branch`, so do not rely on it through the proxy yet); `AS OF` optional (default
   `NOW`) — remove "mandatory"; the fork rule; constraints enforced on a branch; schema
   DDL allowed (CREATE/DROP TABLE, ADD COLUMN) and refused (everything else); TRUNCATE on
   a branch empties it on the branch only; MERGE carries schema changes, refuses when the
   target's definition moved, is last-writer-wins for rows and never reverts what the
   target changed after the fork unless the branch changed the same row; sequences are
   shared; dropping reclaims; REST `/v1/branches/:name/…` and MCP branch tools are scoped,
   `heliosdb_branch_drop` exists. Remove the stale pitfalls (TRUNCATE "rows reappear",
   "AS OF is mandatory", "drop reclaims nothing" implications).

## Tests (write them first)

New `tests/branch_fork_point.rs` (embedded + params paths; each ends by checking main):
- NOW branch: after the fork, main INSERT / UPDATE / DELETE of rows the branch never
  touched are NOT visible on the branch; the branch's own changes still are; main sees its
  own changes; `MERGE` of a branch that changed nothing changes nothing on main (frozen
  copies are not changes); MERGE of a branch that changed row A while main changed row B
  keeps main's B.
- Main's ADD COLUMN / DROP TABLE / CREATE TABLE after the fork: the branch keeps the old
  definition and rows / still sees the dropped table / does not see the new table.
- Branch-of-branch: `b` changes after `c` forked from `b` are not visible on `c`; main's
  changes are visible on neither.
- AS OF past: rows inserted, updated and deleted on main after the anchor appear on the
  branch as they were at the anchor; refused with time travel off; `SELECT … AS OF
  TIMESTAMP` on main returns a row deleted after the timestamp.
- Creation race: concurrent main writers during `CREATE BRANCH` — every write that
  committed after `CREATE BRANCH` returned is invisible on the branch (loop it).
- Encryption at rest on (`[encryption] enabled = true` test config): frozen copies decode
  on the branch.
- Reclaim: after `DROP BRANCH`, no key with the branch's prefixes remains (scan the
  keyspace), including after a simulated crash mid-reclaim and reopen; branch ids `1` vs
  `12` prefixes do not collide; version GC can advance past a long-lived branch's anchor.
- The cost when no branch exists is one atomic load per write: keep the existing perf
  smoke green (the dispatching session runs it).

## Rules

Do not run cargo, git or any build. Both executor families. No new `unwrap`/`expect` on
non-test paths. Report: files touched, the hook's placement and fast path, the marker and
record formats, the creation linearization, the delete-event format and every reader
changed, the reclaim algorithm, the doc changes, and anything in this spec that turned
out wrong.

## Inherited from B3's fix pass

`docs/plans/specs/B3_branch_schema.md`, section "What B4 inherits", is part of this spec:
`bfrz:` is already written by a branch `ADD COLUMN` and read by MERGE; the reclaim prefixes
gain `bkey:{id}:`; reclaim runs after `finish_pending_branch_merges`, skips sources with an
intent, and re-checks the state (`MERGE BRANCH x ABORT` returns a source to Active); the frozen
record's `parent_fingerprint` and `parent_incarnation` rules; the live-descendants DDL refusal
that this phase lifts, walked through merged branches; `bmeta:{id}:fork`, whose
`ForkEntry { incarnation, fingerprint, columns }` is what MERGE compares a table's target
definition against.

From B3's third fix pass, which changed what MERGE does and what this phase may rely on:

- **Lift one refusal.** MERGE now refuses 55000 when a branch changed rows of a table with no
  fork entry ("created on the target after the fork"). Fork-point reads make such a table
  invisible to the branch, so it cannot have changed rows of it: drop that branch of the check in
  `plan_branch_merge` (the records loop and the rows-only loop) and its test
  `merge_of_rows_of_a_table_created_on_the_target_after_the_fork_is_refused`, replacing it with
  the visibility test above.
- **Keep the other.** A branch with no fork record at all (created by an earlier release) stays
  refused for any table it changed rows of: it has no fork point to read from either. Decide here
  how such a branch reads, and say so in the upgrade notes.
- **Frozen copies and the row phase.** MERGE into main lands each table as one main transaction
  (`land_merged_table_on_main`), with its keys claimed in one step per tree, for the rows the branch
  changed and no others: what is marked `bfrz:` never gets there. A frozen pre-image must stay marked, or
  it reaches the claim as a change and can stop a merge on a key main moved since the fork, and it would
  commit over main's row.
- **Deletes.** That transaction deletes as any main transaction does. `AS OF` on main does not see a row
  deleted after the timestamp today, merged or not; this phase's delete events change both, through the
  commit and `scan_table_at_snapshot`, so no merge-specific path is needed.
- **Materialization's last batch** holds only the rows a level changed, and it is refused 54000 when
  over `MAX_BATCH_BYTES`; marked copies stay chunked. The freeze hook must not grow that batch, and a
  frozen copy written in a final batch would count against it.
- **The statement guard** (`enter_main_statement`, `MAIN_STATEMENT`) counts main's schema statements
  per table, and a merge waits only on overlap. The freeze hook runs under it, in the statement's
  thread: never move it off the thread, or the funnels under it refuse a statement a merge waits for.
- **`CREATE BRANCH` with either end of an unfinished merge as its parent is refused** (55006), so
  the fork-point snapshot never lands on a level part way through a merge. Keep that order in the
  creation linearization.
