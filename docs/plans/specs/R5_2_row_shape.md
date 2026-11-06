# R5.2 — one row-shape funnel; ADD COLUMN by missing value; DataRows that cannot be malformed

Parent: docs/plans/specs/R5_statement_atomicity.md (Part 1 decisions 4, 8 and 11's neighbours win over Part 3; read
Part 3 §1.5, §4 M3, F4, F5; Part 4's H5 and the "ADD COLUMN can be done without rewriting rows" passage).

Why: a row narrower than its schema today gives `XX000 Column index N out of bounds` on the general path and a malformed
DataRow on the PK fast path (the client cannot parse it and the connection dies); ADD COLUMN DEFAULT rewrites every row one
write group at a time, so readers see it half applied (tests/physical_ddl_window_repro.rs `a_...`), a crash leaves the column
NULL or the row unreadable, and an UPDATE racing the backfill leaves a row narrow for good; a non-literal DEFAULT is stored as NULL.

## Deliver
1. Full-row funnel. Every full-row decode (Part 3 census: 22 sites, 5 shape today) goes through one function that shapes the
   tuple to the reader's schema: fewer values than columns -> append each missing column's value from step 3, or NULL when it has
   none; more values than columns -> truncate. It replaces the `snapshot_schema_evolution` "null_pad" arms where they do the same.
   The row cache stores shaped tuples keyed so that a schema change cannot serve an old shape (say how; the generation bump is
   there). The partial-decode functions (`prefix_decode`, 13 functions) keep working on a narrow row: a column past the row's end
   answers its missing value. List every site in your report.
2. DataRow guard. Every DataRow writer (PG, both protocols; MySQL) refuses a row whose field count differs from the row
   description: an error to the client, never a malformed message.
3. Missing values: `meta:attmissing:{table}:{column}`, new key family classified in src/storage/keyspace.rs (replicates), value
   the column's sealed bincode `Value`. ADD COLUMN with a stable default (a literal, or an expression with only stable functions,
   evaluated once at the ALTER through the same evaluator and casts INSERT's default uses) writes the schema and the key in ONE
   group, through `write_ddl_batch`, rewrites no row, at any table size. It is removed by DROP TABLE's purge and DROP COLUMN of the
   column, carried by RENAME TABLE and RENAME COLUMN, honoured by branch views and AS OF reads (a row version older than the column
   reads the missing value, as PostgreSQL does), loaded into the schema cache's entry so the funnel pays no extra read per row, and
   published by the standby's applier when the key arrives. SET DEFAULT never changes it. Where the catalog's other per-table
   families are listed (P1.1b purge, catalog orphans test, dump and restore) add this one.
4. Volatile defaults (random(), nextval, clock_timestamp...: check how the codebase classifies functions; unknown = volatile):
   every row is rewritten in ONE group through `write_ddl_batch` up to `[storage] max_atomic_rewrite_bytes` (new, default 256 MiB,
   documented), refused 54000 above it, measured before anything is written, naming the alternative (add without a default, then
   UPDATE in batches). NOT NULL with no usable default on a non-empty table: 23502 before any write.
5. F5: in one explicit transaction, rows inserted before ADD COLUMN read through the funnel, so they are right at once; check the
   transaction's own staged rows on every read path and at COMMIT; DROP COLUMN after own writes re-shapes the staged rows or is
   refused 0A000 as Part 3 F5 says.
6. B3's MERGE: `AddColumnsOnMain` writes schema + attmissing in one group, no backfill: remove `backfill_added_columns`; rows land
   by their own commit. A branch's ADD COLUMN keeps materializing copies for now.
7. Compatibility: rows already on disk that are narrower than their schema (Part 4 H5: they exist today) read as NULL for columns
   with no missing value. No schema or tuple format changes.

## Tests
tests/row_shape.rs + the repros in tests/physical_ddl_window_repro.rs (`a_` and `c_` lose their ignore): every read path over a
table after ADD COLUMN: full scan, PK point read (both families, the fast path), index scan, LIMIT, ORDER BY top-k, COUNT, aggregates,
JOIN, RETURNING, COPY TO, REST, MySQL wire, a branch view, AS OF before and after the ALTER, the result cache and plan cache, restart,
physical standby. ADD COLUMN with literal, expression, NULL and no default; NOT NULL refused; volatile below and above the cap; a
concurrent reader during ADD COLUMN sees before or after, never a mix; crash right after the ALTER; DROP COLUMN then ADD COLUMN of the
same name does not resurrect the old missing value; RENAME carries it; dump and restore keep it. pg35 must not regress on full scans
and point reads: the shape check is one length comparison on the path where the row already fits.

## Rules
No cargo, no git. Both executor families. No new unwrap/expect on non-test paths. STOP and report if a decode site cannot reach the
schema. Report the site list, what the row cache does, and anything in this spec that turned out wrong.

## What turned out different (implementation notes)

- **Site count.** The census's order holds: 21 full-row decodes through the funnel, one rewrite shaped first, one new rewrite, and 13 partial-decode functions, listed below. `add_column_to_rows` and `backfill_added_columns` are gone.
- **Row cache: stored shape, not shaped.** The cache holds a row as stored and shapes it on the way out (`shaped_row_arc`; one length comparison when it fits), and a row wider than the reader's schema is not cached. Keying shaped tuples cannot be made safe with the generation: a reader planned under the previous schema captures the *current* generation, shapes with its own schema, and fills an entry the next reader would trust. A stored-shape entry is a function of the row's bytes alone. Cost: a narrow row's hit clones and pads.
- **Missing values live beside the schema cache, not in its entry.** `StorageEngine::missing_values_cache`, same lifecycle (`invalidate_schema_cache` / `clear_schema_cache` clear it), loaded on the first narrow row or partial scan of a table, a table with none cached too. Loading at schema-cache fill would add a seek to every fill and to `prewarm_schema_cache`. The funnel is by POSITION in main's durable schema at load, which every schema change invalidates, so `shape_row` takes `(table, width)` and `BranchView` needs no schema.
- **A logical WAL op was needed.** `WalOperation::SetColumnMissing { table, column, value }` rides the group. Without it a replayed DROP in the redo window purges the key and nothing brings it back.
- **F5's `DROP TABLE` refusal contradicts GH#62.** Shipped behaviour discards the staged rows (`gh62_rows_staged_before_drop_table_do_not_reappear_in_recreated_table`), so `DROP TABLE` stays. `RENAME TO` and `SET STORAGE` are refused 0A000 as F5 says. `DROP COLUMN` and a volatile `ADD COLUMN` over own rows re-stage them; under a savepoint they are refused 0A000, because a `ROLLBACK TO` restores images the statement cannot reach.
- **"SET DEFAULT never changes it" is vacuous today.** `ALTER COLUMN … SET DEFAULT` has no plan (`Unsupported ALTER TABLE operation`).
- **Dump needs no new record.** A dump reads rows through the funnel, so it materializes the value into each row; the restored table has wide rows and no key.
- **A branch's stable default** is stored evaluated in `BranchAddedColumn.default`, so MERGE gives main's rows the value the branch's copies hold, with no evaluator at a resume after a crash. A volatile default on a merge rewrites main's rows in the step's group. What such a column can refuse is asked in the merge's preflight, before the intent is written (`preflight_merged_columns`): NOT NULL is judged against what main will hold (its rows minus what the branch changed or deleted), and a volatile rewrite runs into a scratch batch for the cap. The step judges NOT NULL again against main's rows then, so a row main gained after the preflight stops the merge pending rather than taking NULL in the column, and so does a cap lowered before a resume (`MERGE BRANCH … ABORT`, or remove the row, or raise the cap).
- **Existing tests pinned the opposite for `AS OF`** (`pre-ADD snapshot projects NULL, never the backfill`): w3_5 (three), w3_2 (one), w2_5 (one). Flipped per decision: a version older than the column reads the missing value. `strict` still keeps a version's stored arity; current rows are shaped under `strict` too.
- **DROP COLUMN stays several groups here**, as the Deliver list has it (one-group DROP is decision 5's later step): only the schema and the key removal are one group.
- **The DataRow refusal is 0A000, PostgreSQL's code for `cached plan must not change result type`,** not `XX000`, which proxies read as a broken backend. It is the last resort: decision 2(h) is asked at Execute, before anything runs, by comparing the plan's output (names and types) with what was last Described (`revalidate_result_shape`), because a DML with RETURNING refused at its rows has already committed. A statement never Described has nothing to contradict and runs under the new shape; a Describe adopts the new shape. PostgreSQL refuses the never-Described case too; this is deliberate. A portal is bound by its own Describe, else by its statement's (PostgreSQL's rule), and a never-Described portal whose Bind fixed one result format per column of the old width is refused. The refusal carries PostgreSQL's `M` exactly, the detail in `D`, and `F = plancache.c`, `R = RevalidateCachedQuery`, which asyncpg and pgjdbc key their re-prepare on; the last-resort refusal carries no `R`, since a driver retrying it could apply it twice. The derived shape is keyed by the plan epoch, the schema generation and the session's schema, `search_path` and branch, so a scoped session re-derives only when one of them moves.
- **The rewrite writes versions.** The spec had `data:` only, so every snapshot-resolved read shaped the old narrow version and read NULL, and `INSERT … SELECT` in a transaction copied those NULLs for good. It writes a version of each row at one timestamp, a `snapshot:` record, and for a row with no version at all a pre-image older than any snapshot, all in the group. So the cap, which measures the group, fits a third to a half of the table it did. With `time_travel_enabled = false` it writes none, as commits do. The timestamp is taken when the rewrite starts: a snapshot taken during it, above that timestamp, reads the old image until the group lands, then the new one. Only a writer in that window can lose anything, and R5.1's table lock is what stops it.
- **Own staged rows: constraints refused, indexes built.** Item 5 asked for constraints and index maintenance over staged rows at COMMIT. A constraint-bearing `ADD` (any spelling, and `ADD CONSTRAINT`, FOREIGN KEY included) over a staged change of the table is refused 0A000 before any clause writes. `CREATE INDEX` is not refused, because `BEGIN; CREATE TABLE; INSERT; CREATE INDEX; COMMIT` is the shape of a real migration: it brings the new index up to the transaction's view through the funnel (`create_index_over_staged_rows`), with one undo entry at the FRONT of the log, so it replays last and converges on the committed keys whichever earlier entries could see the index's columns. A plain `ADD COLUMN` re-stages the rows at any default, not only a volatile one, so what commits and what is logged is as wide as the table.
- **A multi-clause ALTER is validated, not atomic.** `prevalidate_added_columns` asks each `ADD COLUMN` clause's refusals before the first write, including a dry run of the volatile clauses over the committed rows and the transaction's own (so a volatile default runs twice per row: `nextval` consumes twice, a side effect happens twice, and one that fails only the second time still fails in its clause). A single clause evaluates its own rows' values before `add_columns` and re-stages with them, once. One group for the statement is a later step.
- **The cap's bound is the standby's.** 2 GiB is below `MAX_BATCH_BYTES` (about 4 GiB), and a const assert keeps it there. `from_toml_str` validates it, as `start --config` takes no other check.
- **`apply_live_replicated_row` kept its strict arity for the row applied.** The first pass let a narrow one through, but it cannot be told from a row with a value lost. The log is in order, so a row logged before the ALTER is applied under its own schema, and own staged rows are re-staged by the ALTER. The row already STORED is shaped: it may be older than a column. This path has no production caller now that the logical standby is gone.

## Decode sites

Full row, through the funnel: `decode_stored_row`, `scan_table_with_schema_opt` (Full), `scan_table_with_offset_limit`, `scan_table_with_limit`, `get_row_by_id_arc`, `get_row_by_pk_inner`, `apply_live_replicated_row`, `update_tuples_branch_aware`, `delete_tuples_branch_aware`, `merge_with_write_set_inner` (3 decodes, `RowShape`), `BranchView::{decode_committed, decode_main_row, own_entry via decode_staged, apply_merged_row, padded}`, ON CONFLICT (text, params). Version rule (`shape_version_row`, or the `null_pad` gate): `scan_table_at_snapshot`, VERSIONS BETWEEN (2). Rewrites, shaped first: `migrate_column_storage`, `stage_added_column_rows`; `drop_column_from_rows` removes by position only. Left alone: dump restore (its rows were read through the funnel), the TDE probe, `src/sync` and `engine_timetravel_extension.rs` (not compiled).

Partial, `Option<&MissingValues>` through `prefix_decode`: `scan_table_with_schema_opt` (Prefix, Columns), `scan_table_with_schema_columns_filtered`, `…_columns_integer_filtered`, `…_projected_filtered`, `…_projected_string_eq_filtered`, `…_mixed_projected_filtered`, `…_projected_integer_filters`, `scan_table_topk_single_int_projected_columns`, `scan_table_topk_projected_columns`, `try_aggregate_row_text_group_count_sum_int`, `try_aggregate_primitive_row_columns`, `try_aggregate_primitive_count_sum_avg`, `try_aggregate_row_columns`.
