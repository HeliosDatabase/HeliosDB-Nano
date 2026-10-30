# B3 — schema DDL on a branch

Plan: `docs/plans/BRANCH_ISOLATION.md` (D4). Builds on B1 (session binding) and B2 (the
branch view). Sprinter 6bc4a4040eb7 item 1 (DROP TABLE on a branch drops main's table).

## Promise being delivered

The branches skill, Recipe 1 (migration rehearsal): `USE BRANCH r; ALTER TABLE orders ADD
COLUMN currency TEXT NOT NULL DEFAULT 'USD'; UPDATE …; SELECT …` on the branch, then either
4a — drop the branch and run the same `ADD COLUMN` on main (today it fails: the column is
already on main) — or 4b — `MERGE BRANCH r INTO main` and see the column and the values on
main. "A/B test a new schema." README: "main is never touched."

## Facts (survey 2026-10-30; approximate lines)

- The catalog is global: `meta:table:{t}` (storage/catalog.rs ~4371), `meta:index:`,
  `table_constraints:{t}`, `constraint:{t}:{n}`, identity, partition and trigger keys;
  in-memory `schema_cache: DashMap<String, Schema>` keyed by table name (engine.rs ~2102);
  `Catalog::get_table_schema` (~1586) is the single resolver — its temp-table check (~1609)
  is a precedent for session-private relations.
- Rows are positional bincode `Tuple { values }` (types.rs ~553) shaped to the schema width
  by `values.resize` (engine.rs ~8572); there are no column ids.
- DDL on a branch today: CREATE TABLE and DROP TABLE are global (DROP purges main's
  `data:{t}:`, catalog.rs ~1885, and orphans the branch's rows); ADD COLUMN writes the
  global schema and rewrites main's rows (`add_column_to_rows`, engine.rs ~9737); DROP
  COLUMN rewrites `data:` only (~9863). Refused on a branch today: CREATE INDEX (ddl.rs
  ~66), DROP INDEX (~685), ADD PRIMARY KEY (lib.rs ~7693, ~15055), ADD UNIQUE (~7710,
  ~14734). The shared DDL funnel for both executor families is
  `plan_invalidates_sql_caches` (lib.rs ~2324; called ~8333 and ~19203); pre-plan DDL
  entry points also exist (`execute_create_schema_with_elements` ~8200, MV pre-parse, the
  `execute_alter_table_multi` pre-pass ~7595).
- `tests/w3_5_alter_snapshot_characterization.rs` ~167-250 PINS that a branch sees main's
  ADD COLUMN backfill; this phase (and B4) deliberately changes that — update the test to
  the new rule and say so in its comment.

## What to build

1. **Branch catalog overlay.** Keys `bmeta:{bid}:table:{t}` holding either a table
   definition (the `Schema` plus its constraints, in whatever shape the main catalog keeps
   them — keep it one record per table if possible) and an `isolated` flag, or a
   tombstone. Resolution through the session's chain: the NEAREST level with a `bmeta`
   entry for `t` decides (definition → that definition; tombstone → no such table); none →
   main's catalog. Every catalog reader goes through it: `get_table_schema`, `table_exists`,
   `list_tables`, `information_schema` / `pg_catalog` / `sqlite_master` / `\d` / PRAGMA
   introspection, the planner, and both executor families. Bypass `schema_cache` (and any
   other name-keyed schema cache) when the session is on a branch.
2. **`isolated` tables.** When `t` is isolated on level `L`, the branch view (B2) reads
   `t`'s rows from `L` and its descendants only — never from `L`'s ancestors or main.
3. **DDL on a branch** (the session is bound to branch `b`):
   - `CREATE TABLE t` (t not visible on `b`): writes `bmeta:{b}:table:t` (isolated); rows
     go to `b`'s overlay through the B2 view. Constraints are enforced by the B2 view
     (`rows_with_key` falls back to overlay scans — there is no ART for a branch-created
     table, which is fine at sandbox scale). `CREATE TABLE IF NOT EXISTS` and `CREATE
     TABLE … AS SELECT` likewise.
   - `DROP TABLE t` (visible on `b`): writes a tombstone `bmeta:{b}:table:t` and deletes
     `b`'s own `bdata`/`bdel` for `t`. Main and other branches untouched.
     `DROP TABLE IF EXISTS` likewise.
   - `ALTER TABLE t ADD COLUMN …` (single or multi-op where every op is ADD COLUMN):
     compute the new definition exactly as main would (defaults, NOT NULL, generated
     values), materialize every row `b` sees into `bdata:{b}:t:*` under the new definition
     (applying the column default/backfill rule main uses), and write `bmeta:{b}:table:t`
     with `isolated = true`. O(rows). Constraint and default semantics identical to main.
   - Every OTHER schema statement on a branch — DROP COLUMN, RENAME (table or column),
     ALTER COLUMN TYPE / SET DEFAULT / NOT NULL, constraints, indexes, views, materialized
     views, sequences, functions/procedures, triggers, schemas, `TRUNCATE` is NOT schema
     (B2 owns it) — is REFUSED before it changes anything: SQLSTATE `0A000`, message
     "<statement> is not supported on a branch (on branch b): run it on the parent, or
     rehearse it in a copy of the database (`heliosdb-nano dump` / `restore`)". Both
     executor families and every pre-plan DDL entry point.
4. **MERGE BRANCH with schema changes** (skill Recipe 4b). For each table the source
   branch defines in its own `bmeta` (relative to the target):
   - the TARGET's definition of `t` must be unchanged since the source forked (record
     enough in the branch's `bmeta` at fork/first-DDL time — e.g. the target's definition
     fingerprint — to tell); if any affected table changed on the target, refuse the
     whole merge before writing anything: SQLSTATE `55000`, naming the tables;
   - created on the branch → create it on the target (same definition), then the rows;
   - dropped on the branch → drop it on the target (its rows go with it);
   - ADD COLUMN on the branch → apply the same ADD COLUMN on the target (same backfill
     rule), then the rows (the branch's materialized rows win, as rows do today).
   Rows of tables without schema changes merge exactly as today. The merge stays
   last-writer-wins for rows (documented). All of it in one atomic write if the existing
   merge is atomic; otherwise refuse-or-complete must not leave a half-merged schema.
5. **Reclaim.** `DROP BRANCH` deletes `bmeta:{bid}:*` along with everything else (B4 wires
   the reclaim; here just make sure nothing else references a dropped branch's `bmeta`).
6. **Main's DDL while a branch exists** is B4's job (freezing the parent's definition into
   live children); in B3 a branch keeps reading main's CURRENT definition for tables it
   has not changed itself — do not try to freeze here.

## Tests (write them first)

New `tests/branch_schema_isolation.rs`, embedded text path and params/extended protocol:
- Recipe 1 verbatim on a seeded `orders` table: on the branch the column exists and the
  UPDATE applies; main has no such column (information_schema + SELECT *); 4a — after
  `DROP BRANCH`, the same `ADD COLUMN` on main succeeds; 4b — after `MERGE … INTO main`,
  main has the column and the branch's values.
- CREATE TABLE on a branch: invisible on main and on a sibling branch (catalog AND
  introspection); PK/UNIQUE enforced on it; dropped with the branch; if main later creates
  a table with the same name, the branch still sees only its own rows (isolated).
- DROP TABLE on a branch: main's table and rows intact; the branch no longer sees it;
  `SELECT` on the branch → `42P01`; recreating it on the branch starts empty.
- ADD COLUMN with a DEFAULT and NOT NULL on a populated table: every inherited row shows
  the default on the branch; main unchanged; inserts on the branch must supply/receive the
  new column.
- Refusals: DROP COLUMN, RENAME, CREATE INDEX, ADD CONSTRAINT, CREATE VIEW, CREATE SEQUENCE,
  CREATE FUNCTION on a branch → `0A000`, and the catalog is unchanged afterwards.
- MERGE refused (`55000`) when main changed an affected table's definition after the fork;
  nothing written.
- Two branches each `ADD COLUMN` different columns to the same table: each sees only its
  own.
- Branch-of-branch: a child of `b` sees `b`'s new column and table; a DROP TABLE in the
  child does not affect `b`.
- `w3_5_alter_snapshot_characterization.rs`: updated to the new rule.

## Rules

Do not run cargo, git or any build. Both executor families. No new `unwrap`/`expect` on
non-test paths. Report: files touched, the `bmeta` record format, every catalog reader
routed through the resolver, every DDL entry point and what it now does on a branch, the
merge algorithm, and anything in this spec that turned out wrong.
