# R5.5 — DROP COLUMN in one group, multi-clause ALTER in one group, and AS OF across a shape change

Parent: `docs/plans/specs/R5_statement_atomicity.md`, Part 1 decision 5 (DROP COLUMN and any
row-rewriting ALTER: schema, all rows and constraint/columnar cleanup in ONE group up to the cap,
54000 above it; M4 deferred) and design §8 F3 (multi-op composition). On the tree that holds
R5.1-R5.4 and B4 (R5.3's `DdlBatch` is the vehicle).

## Why

- `ALTER TABLE … DROP COLUMN` writes the schema and then rewrites rows in further groups: a crash or
  a reader between them sees a schema without the column over rows that still have it, and kill -9
  shifts values (`tests/row_shape.rs` ignored crash tests: drop_middle, readd_drop).
- `AGENTS.md` promises "Multi-op `ALTER TABLE` is atomic per statement". Today each clause is its own
  group: a crash or a later clause's refusal keeps the earlier clauses (R5.2 pre-validates its own
  refusals, but `ADD c DEFAULT 1 CHECK (c > 5)` still leaves c, and DROP COLUMN clauses aren't
  pre-validated). Deliver the promise.
- Main's own `AS OF` before a DROP COLUMN, RENAME COLUMN, RENAME TO or REFRESH mis-shapes: a dropped
  column's value shows under a later column's name (R5.2 review P3: `DROP secret; ADD pub` shows
  'secret' under pub in a transaction and in AS OF). B4 records these changes
  (`meta:table_shape:{t}`, `storage.catalog().main_table_shape_change(table)`, `describe(table)`,
  per-row `snapshot_manager().row_events_around(table, row, ts)`; see
  `StorageEngine::anchor_version_is_current_shape`).

## Items

1. DROP COLUMN: schema, every row's rewrite with a version of each (as R5.2's volatile rewrite
   does), the column's constraints, indexes, columnar side data and missing value, in ONE group up to
   `[storage] max_atomic_rewrite_bytes` (with B4's frozen-copy factor), 54000 above it naming the
   alternative. Any other row-rewriting ALTER (SET STORAGE, ALTER COLUMN TYPE if supported) the same.
2. Multi-clause ALTER: one `DdlBatch` for the whole statement; every clause pre-validated before the
   first write; one group; one publication. A refusal by any clause leaves nothing.
3. Main's `AS OF` (and a transaction snapshot) older than a recorded shape change: read the row
   version under the definition it was written with, if the definition at that time can be
   reconstructed from what is recorded; otherwise refuse 0A000 naming the table and the change, the
   way B4 refuses an anchor. Never show a value under the wrong column. Rows whose newest version
   predates the change and that changed since are the hard case; follow B4's rule.
4. Un-ignore `a_crash_after_drop_column_keeps_the_other_columns_values` and
   `a_crash_after_a_drop_and_readd_keeps_the_last_definition` only if they now pass WITHOUT the
   crash-replay step; otherwise leave them for R5.7 and say why.

## Tests

One-group tests for DROP COLUMN and for multi-clause ALTER (both families), a refusal in clause 3
leaving clauses 1-2 unwritten, a reader loop during DROP COLUMN never seeing a shifted value, AS OF
before DROP COLUMN / RENAME COLUMN / RENAME TO / REFRESH either right or refused (never shifted), on
main and in a REPEATABLE READ transaction begun before the change.

## What turned out different (implementation notes)

- **One body for every ALTER TABLE, single clause or several** (`src/alter_table.rs`,
  `EmbeddedDatabase::execute_alter_statement`, reached from both families' arms and from
  `execute_alter_table_multi`). The statement is the table as its clauses leave it, in memory
  (`AlterStatement`: schema, missing values, constraint and identity records, other tables'
  records a clause touches, and what each clause does to the rows, `RowStep`). Each clause is asked
  against it, so it sees the clauses before it (a CHECK sees an added column's value in every row;
  a UNIQUE is backfilled from those rows), and nothing is staged until the last clause is through.
  Then `finish_alter` stages the rows (`StorageEngine::stage_rewritten_rows`), the catalog
  (`Catalog::stage_altered_table`), the constraints, identity and columnar records into one
  `DdlBatch`. `prevalidate_added_columns` (R5.2's dry run) is gone: a volatile default is
  evaluated once per row. The whole-statement pre-checks of `execute_alter_table_multi` stay.
- **`RENAME TO` with other clauses is refused 0A000** (PostgreSQL's grammar refuses it too). It
  rewrites the table's keys under another name; composing it with the rest is not worth it.
- **`DROP COLUMN` rewrites every row shaped to the table's width first**, so a row older than a
  column added by missing value gets that value physically (it used to remove by position from the
  stored row, which kept narrow rows narrow).
- **AS OF across a DROP is read right, not refused**, through a new key family:
  `meta:table_layout:{t}` (`storage::table_layout`, `KeyClass::MetaTableLayout`). B4's
  `meta:table_shape:` holds only the last change and no position, so it cannot reconstruct a drop.
  The layout record holds every `DROP COLUMN` (width, position) at the statement's timestamp, and
  a cut for what cannot be read across: `RENAME TO` (the history stays with the old name, which
  purges it), a refresh that swapped a view's data table, `SET STORAGE` out of a dictionary,
  content-addressed or columnar representation. Reads: `scan_snapshot` (AS OF and the
  transaction scan), `Transaction::get` on a data key (point reads at a snapshot), and `VERSIONS
  BETWEEN` take a version through the drops after it; the pre-image a rewrite writes at
  `REWRITE_PRE_IMAGE_TS` is placed just before the row's next event. A snapshot older than a cut
  is refused 0A000 (the branch-anchor marker), except for the transaction whose own `RENAME TO`
  made it (`Transaction::note_own_history_cut`). A refused read can always be answered from a
  later snapshot. Purged by DROP TABLE, replaced on the new name by RENAME (the old name's goes).
- **B4's shape record is stamped with the statement's timestamp**, the one its rewritten rows'
  versions carry, so those versions are not "older than the change" to a branch anchor.
- **Fix pass 1: branch anchors read by the layout record** (`materialize_branch_anchor`,
  `anchor_image_in_layout`), so `CREATE BRANCH … AS OF` and `SELECT … AS OF` give the same answer for
  the same time: an anchor before a `DROP COLUMN`, `RENAME COLUMN` or `SET STORAGE` into a storage
  mode is created and reads each value under its own column; before a cut it is refused. B4's
  lost-history rule (a row a rename moved, with no version at the snapshot and a change since) is
  the layout's too (`TableLayout::lost_history`, cut `through_row`), for `AS OF`, transaction reads
  and anchors alike. B4's shape-record rule remains for a table with no layout record.
- **Fix pass 1: a rename, and a refresh that replaced a view's data table, stage their own
  `snapshot:` record** at the cut's timestamp (`stage_rewrite_snapshot`, published after the
  write). `AS OF TIMESTAMP` reads at the newest snapshot at or before the time, and these wrote
  none: a time after the rename with nothing committed since read at a snapshot before it, and was
  refused as a read from before the rename.
- **Fix pass 2: the lost-history question is asked before the read**
  (`SnapshotManager::history_lost_at`, by `scan_snapshot`, `read_at_snapshot_in_layout` and
  `anchor_image_in_layout` alike), never from the read's result. The snapshot cache can still hold
  the current image such a row read while it had no version at all: a transaction's commit writes
  the row's first version without moving the table's cache generation (pre-existing). Asked only
  after a read that found nothing, the second anchor of
  `an_anchor_after_a_rename_tells_rows_it_left_behind_from_rows_created_since` hit the entry the
  first anchor filled and was created; whether a read is refused no longer depends on the cache.
- **SET STORAGE is one group** (`StorageEngine::stage_column_storage_migration`): rows, schema,
  dictionary record (`DictionaryManager::stage_column`), content-addressed payloads
  (`ContentAddressedStore::plan_store`), columnar batches (grouped, `apply_batch_values` under the
  zone-stats lock taken at the end and held to the write), presence purge, WAL entries.
- **A volatile default's evaluation is on the detector's allow-list** (`VOLATILE_DEFAULT_EFFECTS`):
  `nextval` flushes its sequence's high-water before serving (R5 design §2.3).
- **Item 4: the two crash tests stay ignored.** Each statement is one group now; what undoes them
  is the replay at open re-putting the INSERT's logged three-column images over the DROP's rows,
  which nothing in the log narrows again. That is decision 14 (R5.7). A crash test with the rows
  below the checkpoint passes (`a_crash_right_after_drop_column_keeps_each_value_in_its_column`).
- **The reader loop is ignored for R5.6.** One group removes every intermediate store state; a
  reader that resolved the schema before the group and scans after it still pairs the two.
- Codex review pass: `DROP COLUMN … CASCADE` and `RENAME COLUMN` include foreign-key neighbours in
  the plan's sorted lock set. The ALTER backstop also rechecks and holds inbound tables after the
  parent lock, before reading their constraint records or opening the batch. A child's concurrent
  constraint change must not be overwritten by a stale record loaded before that lock. The
  regression holds a child transaction, queues the parent rewrite, adds a CHECK in the holder, then
  commits it: both the CHECK and the parent's FK rewrite must survive.
- Codex review pass: dropping a column removes user index definitions (including multi-column
  keys and vector indexes) in the same batch, with `DropIndex` WAL entries, and tears down derived
  structures after publication. Renames earlier in the same statement are applied when finding
  those indexes. The regression drops a column with ordinary and multi-column unique user indexes,
  reopens, adds a fresh column of the name, and checks that no old index is reused.
- Not changed, found on the way: `AS OF` and transaction snapshot scans never resolve dictionary, content-addressed or
  columnar references (pre-existing; a SET STORAGE cut does not make it worse); a REPEATABLE READ
  transaction's scan leaves out rows deleted after its snapshot (B4's documented choice), so a
  view refreshed after it reads empty there.
