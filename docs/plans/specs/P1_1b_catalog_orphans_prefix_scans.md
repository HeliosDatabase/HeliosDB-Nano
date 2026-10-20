# Spec P1.1b — catalog orphans and prefix-mode scans (primary bugs that block standby hydration)

Parent: `PHYSICAL_STANDBY_REPLICATION.md` P1. Sprinter `3b10bcb2a804` and `e142fe16ef6d`. After P1.1.

Value: the P1.1 tests compare a standby with a fresh hydration of a primary checkpoint. They
found that the primary leaves catalog orphans and that several open-time scans skip records
after a range tombstone. Strict hydration refuses the orphans, so no standby can be built from
a primary that ever dropped a PK table; the scans can lose counters and snapshots on any
restart. Fix at the source, on the primary, with strict hydration unchanged.

## 1. Catalog orphans (`3b10bcb2a804`)
- **DROP TABLE** must delete `table_constraints:{t}` and every other per-table catalog sidecar
  (`statistics:table:{t}`, `compression:*:{t}`, `trigger:`/`trigger_rowmut:` entries for `t`,
  `meta:index:*` definitions on `t`, `meta:identity:`/`meta:partchild:` entries, `counter:{t}`,
  `dict:{t}:*`, `col*:{t}:*` sidecars if not already) in the SAME write group as the data range
  delete (one `WriteBatch`), so a standby applying that group sees a consistent catalog. Audit
  `Catalog::drop_table` and `EmbeddedDatabase::drop_table` (lib.rs) for the full list; cite each.
  `create_table` must never merge a stale constraints record (lib.rs ~7583): with the orphan
  gone this cannot happen, but add the refusal/assert so a future orphan fails loudly.
- **RENAME COLUMN** must rewrite the affected `meta:index:*` definitions (column name) in the
  same write group as the schema change; `rename_column_in_indexes` stays for the live ART.
- **MERGE BRANCH** must maintain the primary's in-memory ART/vector indexes for the rows it
  writes (reuse the `stage_replicated_row`/`on_insert` paths or invalidate + re-derive the
  table via the P1.1 catalog helpers `register_table_indexes` + `backfill_table_indexes`).
- Tests (proof-first, red today): after DROP TABLE of a PK table → reopen (and
  `open_physical_snapshot`) succeeds and the keyspace has no `table_constraints:{t}`;
  re-create same name → fresh constraints only; RENAME COLUMN with a secondary index → reopen
  registers the index on the new column, lookups work; MERGE BRANCH → PK lookups and COUNT(*)
  on the live primary equal a reopened copy. Un-skip/extend the P1.1 cases that worked around
  these (`drop_table_retires_its_indexes` direct-assert, the rename test's avoided index, the
  merge test's reference-only compare) so they compare against the primary again.

## 2. Prefix-mode scans (`e142fe16ef6d`)
- Audit every full-keyspace or cross-prefix scan: `grep -n 'iterator(IteratorMode::Start)\|iterator(rocksdb::IteratorMode::Start)' src/` and any `prefix_iterator` whose prefix is shorter than 5 bytes or that spans families. For each: either set `ReadOptions::set_total_order_seek(true)` (pattern ddl.rs ~894, engine.rs ~9414/9598/9713, and P1.1's `recover_snapshots_with_policy`) or justify in a comment why a prefix-mode scan is correct (prefix ≥ 5 bytes, one family). Known: `load_counters` (engine.rs ~14410), the logical-WAL TRUNCATE replay (~12328).
- `gc_old_snapshots` (time_travel.rs): remove a transaction-id / SCN mapping only when it names
  the snapshot being dropped; re-point to the newest survivor (mirror P1.1's `forget_snapshots`).
- Tests: three flushed SST files + a range tombstone (the P1.1 unit-test pattern in
  time_travel.rs) for counters: TRUNCATE + reopen keeps the row counter (next row id > old max);
  TRUNCATE replay from the logical WAL after a crash; GC with shared LSN ids keeps the survivor's
  mapping. Add a `tests/` tripwire listing every `IteratorMode::Start` site with its justification
  (fails on a new unjustified site), same style as the keyspace/funnel tripwires.

## Constraints
No cargo/git; read before edit; match style; primary behaviour changes are exactly the fixes
above. Report: files; the per-table sidecar list deleted by DROP TABLE with citations; the
scan audit table (site → total-order or justification); tests red/green; commands
(`physical_standby_derived_state`, `physical_snapshot_hydration`, `gh_issue_35`, `gh_issue_36`,
lib `storage::`, the new tripwire, `cargo check --all-targets`).
