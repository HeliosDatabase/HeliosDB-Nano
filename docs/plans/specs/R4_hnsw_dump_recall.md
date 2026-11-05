# R4 — HNSW dump cache defects and the recall flake family (Sprinter 746342a0bfbb, 66aeb488f312)

Plan: `docs/plans/STANDBY_BACKLOG_4_42.md`. Context: the HNSW graph dump (`hnsw_snapshots/`
files + a `vecsnap:` sidecar) stays a PRIMARY-LOCAL cache (decided on 8f0721af1a70: standbys
never read it and rebuild indexes from rows). The survey of 2026-10-24 (code reading, not yet
reproduced — verify each first) found:

1. After a RELOAD from the dump, hnsw_rs 0.3.3 `load_hnsw` sets `datamap_opt=true`
   (hnswio.rs ~517), so the next `file_dump` refuses to overwrite (api.rs ~73) and
   `DumpInit::new` writes `{basename}-{rand}.hnsw.*` (hnswio.rs ~156-187); our
   `dump_graph_impl` then errors (hnsw_index.rs ~360-367) → every clean close after a reload
   counts a `vector_dump_failures`, leaves a full-size orphan pair that
   `remove_vector_index_snapshot` never deletes (engine.rs ~10489), and the stale
   sidecar+files reload next open, leaking per restart. No test does reload→close→reopen;
   tests/physical_standby_fireproof.rs ~103-151 may pass for the wrong reason.
2. `EmbeddedDatabase::drop` ran the row-counter flush and `persist_index_snapshots` before
   the last-owner gate — CHECK whether the trigger-clone Drop fix (4517cd09, Sprinter
   96e5a8cfa254) already gates them; if not, gate them.
3. `HnswIndex::new` calls `Hnsw::new(M, dimension, ef_construction, 100, Dist)` but the crate
   signature is `(max_nb_connection, max_elements, max_layer, ef_construction, f)`
   (hnsw.rs ~766-772): effective ef_construction is always 100, `max_layer = min(16,
   configured ef_construction)`, and a configured ef_construction < 16 makes every dump fail
   (hnswio.rs ~893-896 needs nb_layer == 16). Also check `max_elements` is sized sensibly.
4. Dump files are not fsynced and not atomic; under at-rest encryption the plaintext vectors
   sit in `hnsw_snapshots/` next to sealed rows; the basename is a lossy sanitized index name
   (collisions possible); sidecar and marker are two separate puts.
5. Recall flake family (66aeb488f312): exact-match rows missing from top-k on indexes REBUILT
   from rows or built under load — three tests: physical_standby_fireproof
   `a_graph_dump_the_sidecar_does_not_describe_is_rebuilt_from_rows`,
   vector_sql_integration_test `test_parallel_backfill_indexes_all_rows` and
   `test_create_hnsw_index_on_populated_table_backfills_existing_rows`, plus
   `test_knn_planner_matches_bruteforce_top1` once. Item 3 (wrong max_layer / ef) is a strong
   suspect; also check entry point and level assignment on rebuild, ef_search for small N,
   parallel insert ordering.

## What to build

- Dump into a fresh temp directory, fsync files and directory, rename into `hnsw_snapshots/`
  atomically, then write sidecar+marker in ONE batch; at open, sweep dump files no sidecar
  references. A reload→close→reopen cycle reloads, leaves zero stray files and counts no dump
  failure.
- Fix the constructor argument order; derive `max_layer` correctly (the crate's constant 16
  if the dump format requires it); unit test that a built graph reports the configured M and
  ef_construction.
- Per-index dirty flag so an unchanged index is not re-dumped at close.
- Basename from a stable index id, not the sanitized name.
- Under at-rest encryption: do not write plaintext vectors — skip the dump (rebuild at open)
  unless the files can be sealed with the dataset key; document which.
- Recall: reproduce with a seeded loop (400 random 4-dim vectors; build incrementally, rebuild
  from rows, parallel backfill; query each row's own vector, top-1) and measure the miss
  rate before and after; fix in the index (parameters, level assignment, entry point,
  insertion order, or an exact fallback below N rows); the three tests' asserts stay strict
  (restore LIMIT 5 in the fireproof test once the miss rate is 0 over 50 seeded runs).
- Tests for every item above.

## Rules

Do not run cargo, git or any build. No new `unwrap`/`expect` on non-test paths. Report which
survey claims reproduced (and which did not), the fixes, the measured miss rates, and anything
in this spec that turned out wrong.
