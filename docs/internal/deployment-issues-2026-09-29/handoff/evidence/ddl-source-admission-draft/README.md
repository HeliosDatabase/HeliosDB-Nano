# CREATE TABLE completion admission draft

External only; no frozen source, builds, or running processes changed. Base hashes are in base-source.sha256. Prerequisites are the source-fence storage admission API and storage-durable-phase-helper.patch (fb909db1 prefix, physical_durable_phase / complete).

Catalog::create_table keeps its public signature and delegates to crate-private create_table_with_finalize. Operation admission precedes preflight; the durable phase begins after existing duplicate/unique-index preflight and immediately before logical WAL/schema persistence. The SQL wrapper supplies its existing identity, partition and constraints/index work as the completion callback. Any propagated failure after phase entry poisons certification before admission is released. Ordinary SQL remains available; this does not invent DDL rollback or erase partial state. Successful IF NOT EXISTS exits before this callback, as before. The redundant warning-only outer logical CreateTable WAL emission was removed; Catalog retains the single existing fallible WAL emission inside the phase.

Four authored unit tests, not executed: exact callback failure after schema and constraints persisted plus sticky poison/ordinary DDL continuation; duplicate refusal with callback uncalled and unchanged sequence; real SQL partition registry decode failure after child schema persistence; successful SQL constraint metadata plus duplicate/IF NOT EXISTS/missing-FK prewrite controls. The injected callback test is deliberate fault injection through the exact semantic boundary, not a test-only production branch.

Validation: rustfmt parsing/formatting using repository settings, Python generator syntax, git apply --check against frozen base. No compile or runtime claim. Source reviews pending.

Remaining bounded audit scopes, expressly NOT repaired here:

- src/storage/catalog.rs::create_table existing warning-only PK ART registration errors (around line 879) and lib finalizer's existing load_table_constraints if-let-Ok behavior. This patch tracks propagated failures; swallowed failure semantics require their own explicit poison or exact harmless-error classification.
- src/lib.rs::execute_alter_table_multi (7057), alter_table_add_unique (14069), add_foreign_key (14190), drop_constraint (14308), add_primary_key (14390), add_check (14491), add/drop/rename_column (14627 onward): related row/schema/constraint/ART writes need a whole semantic boundary, with ordinary validation completed before arming. Multi-action ALTER currently executes steps separately.
- src/storage/catalog.rs::drop_table (1573), drop_table_index_definitions (5085), drop_unique_constraint_indexes (1258): cleanup warning-and-continue paths need certification-only poison; partition cascade bookkeeping may precede the core drop.
- src/sql/executor/ddl.rs CREATE/DROP INDEX and lib CTAS: durable definition, structures and row-loading phases require their own complete operation boundary, including vector persistence and ignored cleanup failures.
- Other catalog namespace mutations (views/MV, routines/triggers, enums, sequences, table rename/schema move) are not certified by this scoped change.

No full source-mutator coverage, full resync serving readiness, or performance acceptance is claimed.
