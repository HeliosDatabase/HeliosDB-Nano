//! GH #36 — the logical WAL must represent EVERY schema-changing statement.
//!
//! # What this file pins
//!
//! Anything built on the `wal:entries:` log — HA replication
//! (`ha_state::broadcast_wal_operation`), a CDC consumer, point-in-time
//! recovery, and the checkpoint-bounded open-time replay added in 57c808b —
//! can only reconstruct the right end state if every DDL statement leaves an
//! entry behind. A statement that changes the catalog and logs NOTHING makes a
//! replica silently diverge: no error, no gap in the LSN sequence, just a
//! different schema on the other side.
//!
//! The issue's headline (`ALTER TABLE … RENAME TO` is unrepresented) is FIXED
//! on this tree, ON THE TEXT FAMILY: `Catalog::rename_table_inner`
//! (src/storage/catalog.rs:2085) calls `StorageEngine::log_rename_table`
//! (src/storage/engine.rs:9944), which appends `WalOperation::RenameTable`
//! (src/storage/wal.rs:143); both replay drivers apply it
//! (src/storage/engine.rs:10486 and :10965) and
//! `ha_state::broadcast_wal_operation` classifies it as a SchemaChange
//! (src/replication/ha_state.rs:545). Those tests are the regression guard.
//!
//! It is FIXED ONLY ON THE TEXT FAMILY, and that is not a nitpick: the params
//! family (PG extended protocol — psycopg / JDBC / sqlx / node-postgres — and
//! every REST write) cannot execute `ALTER TABLE … RENAME TO` at all. It is
//! rejected by `Executor::plan_to_operator`'s catch-all
//! (src/sql/executor/mod.rs:4953); see the `Family` doc comment below and
//! `params_family_alter_table_column_ddl_is_rejected_loudly_not_silently_unlogged`.
//! That rejection is fail-CLOSED (loud error, nothing appended, nothing applied),
//! which is why the headline verdict is "fixed where the statement can run"
//! rather than "unfixed".
//!
//! The rest of the DDL surface is NOT represented, and those tests FAIL on the
//! current tree. Each one is marked ***UNFIXED*** with what the current tree
//! produces.
//!
//! # Decoding, not grepping
//!
//! `bincode` encodes an enum variant as a NUMERIC discriminant, so text-searching
//! a WAL payload for "RenameTable" is a guaranteed false negative. Every
//! assertion here goes through `WalEntry::deserialize` and matches on the real
//! `WalOperation` variant.
//!
//! # How one statement's entries are isolated
//!
//! `StorageEngine::recover_wal_at_open` (src/storage/engine.rs:2463) adopts and
//! TRUNCATES an un-checkpointed log at open without replaying it. So:
//!
//!   1. open the store, run the setup DDL, close   -> log holds the setup entries
//!   2. reopen                                     -> the setup log is adopted + truncated
//!   3. run the statement under test, close        -> log holds ONLY its entries
//!   4. open the store RAW (never through the engine again — a third open would
//!      truncate what we came to read) and decode `wal:entries:`
//!
//! `the_reopen_truncates_the_setup_log` is the positive control for step 2.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::storage::{WalEntry, WalOperation};
use heliosdb_nano::{Config, EmbeddedDatabase, StorageEngine, Value};
use rocksdb::{Options, DB};
use std::path::Path;

/// `WriteAheadLog::ENTRY_PREFIX`, restated so this file fails loudly if the
/// on-disk keyspace is ever renamed without the tests being revisited.
const ENTRY_PREFIX: &[u8] = b"wal:entries:";

/// Which DML/DDL executor family a statement is driven through.
///
/// The two are genuinely different code paths, and for `ALTER TABLE` they do
/// not merely differ — they DIVERGE:
///
///   * text family: the ALTER arms are inlined in `execute_in_transaction_inner`
///     (src/lib.rs:4781; the RENAME arm at :7395), and `execute_alter_table_op`
///     (src/lib.rs:11418) is called from exactly ONE site, src/lib.rs:7461 — the
///     text family's own `AlterTableMulti` arm. It is not "the params copy".
///   * params family: `execute_plan_with_params_inner` (src/lib.rs:14903) routes
///     ONLY three ALTER plans — `AlterTableAddUnique` (:14973),
///     `AlterTableDropConstraint` (:14981) and `AlterTableAddForeignKey` (:14989).
///     Every other ALTER form falls to the catch-all at src/lib.rs:16384 →
///     `Executor::plan_to_operator` (src/sql/executor/mod.rs:3394), which has NO
///     `AlterTable*` arm at all and errors "Operator not yet implemented"
///     (src/sql/executor/mod.rs:4953). The in-tree comment at src/lib.rs:14964
///     says so, and `tests/rename_table_trigger_tests.rs:161` pins it.
///
/// So the column-DDL and RENAME forms are exercised on the text family plus a
/// dedicated params-family PARITY control; the constraint forms are exercised on
/// both, because those three really do share one body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    /// `db.execute()` -> `execute_in_transaction_inner`
    Text,
    /// `db.execute_params()` -> `execute_plan_with_params_inner`
    /// (what the PostgreSQL extended protocol and the REST layer use)
    Params,
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn config_for(dir: &Path) -> Config {
    let mut c = Config::default();
    c.storage.path = Some(dir.to_path_buf());
    c.storage.memory_only = false;
    c.storage.wal_enabled = true;
    // DDL logging is NOT gated on this flag (`log_create_table` and friends
    // check only `is_replaying` and `wal.is_some()`); it is left at its default
    // so these tests describe the DEFAULT configuration, not an opt-in one.
    c.storage.logical_wal_per_statement = false;
    // This harness READS the retained entries of a closed store. GH#35's
    // close-time logical-WAL checkpoint (default on) advances the checkpoint
    // and reclaims exactly those entries, and its periodic triggers can do the
    // same mid-test, so both are disabled here. The close-time behaviour itself
    // is covered by tests/gh_issue_35.rs.
    c.storage.wal_checkpoint_on_close = false;
    c.storage.wal_checkpoint_interval_entries = 0;
    c.storage.wal_checkpoint_interval_secs = 0;
    c
}

/// Open the embedded database, retrying briefly: the previous handle's RocksDB
/// background threads release the directory lock asynchronously.
fn open_db(dir: &Path) -> EmbeddedDatabase {
    let mut last = None;
    for _ in 0..100 {
        match EmbeddedDatabase::with_config(config_for(dir)) {
            Ok(db) => return db,
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
    panic!("embedded open failed: {last:?}");
}

/// Raw RocksDB handle on a CLOSED store. The 5-byte fixed prefix extractor
/// matches the one `StorageEngine::open` configures.
fn open_raw(dir: &Path) -> DB {
    let mut opts = Options::default();
    opts.create_if_missing(false);
    opts.set_prefix_extractor(rocksdb::SliceTransform::create_fixed_prefix(5));

    let mut last = None;
    for _ in 0..100 {
        match DB::open(&opts, dir) {
            Ok(db) => return db,
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
    panic!("raw RocksDB open failed: {last:?}");
}

fn run_try(db: &EmbeddedDatabase, sql: &str, family: Family) -> heliosdb_nano::Result<u64> {
    match family {
        Family::Text => db.execute(sql),
        Family::Params => db.execute_params(sql, &[]),
    }
}

fn run(db: &EmbeddedDatabase, sql: &str, family: Family) {
    run_try(db, sql, family).unwrap_or_else(|e| panic!("[{family:?}] `{sql}` failed: {e}"));
}

/// Decode every retained `wal:entries:` record of a CLOSED store, in LSN order
/// (the keys are `wal:entries:{lsn:020}`, so lexicographic == numeric).
fn decode_entries(dir: &Path) -> Vec<WalOperation> {
    let raw = open_raw(dir);
    raw.prefix_iterator(ENTRY_PREFIX)
        .filter_map(|item| item.ok())
        .take_while(|(key, _)| key.starts_with(ENTRY_PREFIX))
        .map(|(key, value)| {
            WalEntry::deserialize(&value)
                .unwrap_or_else(|e| {
                    panic!(
                        "a retained WAL entry did not decode ({}): {e} — the store must not be \
                         encrypted for this harness to read it",
                        String::from_utf8_lossy(&key)
                    )
                })
                .operation
        })
        .collect()
}

/// Run `setup`, discard the entries it logged, then run `under_test` and return
/// exactly the operations IT appended. See the module header for the mechanism.
fn emitted(setup: &[&str], under_test: &[&str], family: Family) -> Vec<WalOperation> {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let dir = temp.path();

    {
        let db = open_db(dir);
        for sql in setup {
            run(&db, sql, Family::Text);
        }
    }
    {
        // This open adopts + truncates the setup log without replaying it.
        let db = open_db(dir);
        for sql in under_test {
            run(&db, sql, family);
        }
    }
    decode_entries(dir)
}

/// Like [`emitted`], but the statement under test is EXPECTED to be rejected.
/// Returns the rejection error text and the entries the rejected statement left
/// behind (which must be none). Used only by the params-family parity control.
fn emitted_after_rejection(setup: &[&str], under_test: &str, family: Family) -> (String, Vec<WalOperation>) {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let dir = temp.path();

    {
        let db = open_db(dir);
        for sql in setup {
            run(&db, sql, Family::Text);
        }
    }
    let err = {
        let db = open_db(dir);
        match run_try(&db, under_test, family) {
            Ok(_) => String::new(),
            Err(e) => e.to_string(),
        }
    };
    (err, decode_entries(dir))
}

/// A compact, human-readable rendering used only in failure messages.
fn describe(ops: &[WalOperation]) -> String {
    if ops.is_empty() {
        return "<no entries at all>".to_string();
    }
    ops.iter()
        .map(|op| match op {
            WalOperation::Insert { table, .. } => format!("Insert({table})"),
            WalOperation::Update { table, .. } => format!("Update({table})"),
            WalOperation::Delete { table, .. } => format!("Delete({table})"),
            WalOperation::Truncate { table } => format!("Truncate({table})"),
            WalOperation::CreateTable { table, .. } => format!("CreateTable({table})"),
            WalOperation::DropTable { table } => format!("DropTable({table})"),
            WalOperation::RenameTable { old_table, new_table } => {
                format!("RenameTable({old_table} -> {new_table})")
            }
            WalOperation::AlterColumnStorage { table, column, .. } => {
                format!("AlterColumnStorage({table}.{column})")
            }
            WalOperation::CreateIndex { name, table, .. } => format!("CreateIndex({name} on {table})"),
            WalOperation::DropIndex { name } => format!("DropIndex({name})"),
            WalOperation::CreateTrigger { name, table, .. } => format!("CreateTrigger({name} on {table})"),
            WalOperation::DropTrigger { name, .. } => format!("DropTrigger({name})"),
            WalOperation::CreateFunction { name, .. } => format!("CreateFunction({name})"),
            WalOperation::DropFunction { name } => format!("DropFunction({name})"),
            WalOperation::CreateProcedure { name, .. } => format!("CreateProcedure({name})"),
            WalOperation::DropProcedure { name } => format!("DropProcedure({name})"),
            WalOperation::CreateMaterializedView { name, .. } => format!("CreateMaterializedView({name})"),
            WalOperation::DropMaterializedView { name } => format!("DropMaterializedView({name})"),
            WalOperation::RefreshMaterializedView { name, .. } => format!("RefreshMaterializedView({name})"),
            WalOperation::AddConstraint { table, .. } => format!("AddConstraint({table})"),
            WalOperation::DropConstraint { table, constraint_name } => {
                format!("DropConstraint({table}.{constraint_name})")
            }
            WalOperation::Begin { tx_id } => format!("Begin({tx_id})"),
            WalOperation::Commit { tx_id } => format!("Commit({tx_id})"),
            WalOperation::Abort { tx_id } => format!("Abort({tx_id})"),
            WalOperation::UpdateCounter { table_name, new_value } => {
                format!("UpdateCounter({table_name} = {new_value})")
            }
            WalOperation::AlterTableSchema { table, .. } => format!("AlterTableSchema({table})"),
            WalOperation::CreateView { name, .. } => format!("CreateView({name})"),
            WalOperation::DropView { name } => format!("DropView({name})"),
            WalOperation::CreateSequence { name, .. } => format!("CreateSequence({name})"),
            WalOperation::AlterSequence { name, .. } => format!("AlterSequence({name})"),
            WalOperation::DropSequence { name } => format!("DropSequence({name})"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Does this operation name `object` EXACTLY (never as a substring)?
///
/// Exactness matters: `CREATE MATERIALIZED VIEW mv1` does log a `CreateTable`
/// for its BACKING table `__mv_mv1`, and a substring test would let that stand
/// in for the missing `CreateMaterializedView` entry.
///
/// This match is deliberately exhaustive with no `_` arm: adding a
/// `WalOperation` variant must break this file's compilation, forcing whoever
/// adds it to say whether the coverage table in
/// `docs/guides/logical_wal_ddl_coverage.md` changes.
fn names_object(op: &WalOperation, object: &str) -> bool {
    match op {
        WalOperation::Insert { table, .. }
        | WalOperation::Update { table, .. }
        | WalOperation::Delete { table, .. }
        | WalOperation::Truncate { table }
        | WalOperation::CreateTable { table, .. }
        | WalOperation::DropTable { table }
        | WalOperation::AlterColumnStorage { table, .. }
        | WalOperation::AddConstraint { table, .. }
        | WalOperation::DropConstraint { table, .. } => table == object,
        WalOperation::RenameTable { old_table, new_table } => old_table == object || new_table == object,
        WalOperation::CreateIndex { name, table, .. } => name == object || table == object,
        WalOperation::CreateTrigger { name, table, .. } => name == object || table == object,
        WalOperation::DropIndex { name }
        | WalOperation::DropTrigger { name, .. }
        | WalOperation::CreateFunction { name, .. }
        | WalOperation::DropFunction { name }
        | WalOperation::CreateProcedure { name, .. }
        | WalOperation::DropProcedure { name }
        | WalOperation::CreateMaterializedView { name, .. }
        | WalOperation::DropMaterializedView { name }
        | WalOperation::RefreshMaterializedView { name, .. } => name == object,
        WalOperation::UpdateCounter { table_name, .. } => table_name == object,
        WalOperation::AlterTableSchema { table, .. } => table == object,
        WalOperation::CreateView { name, .. }
        | WalOperation::DropView { name }
        | WalOperation::CreateSequence { name, .. }
        | WalOperation::AlterSequence { name, .. }
        | WalOperation::DropSequence { name } => name == object,
        WalOperation::Begin { .. } | WalOperation::Commit { .. } | WalOperation::Abort { .. } => false,
    }
}

/// Is this a SCHEMA-CHANGE entry (as opposed to a row/counter/transaction one)?
///
/// ADVERSARIAL-REVIEW FIX. `assert_names` used to accept ANY entry naming the
/// object, which is vacuously satisfiable: an `UpdateCounter { table_name: "t" }`
/// from `src/storage/transaction.rs:1707`, or a row `Delete { table: "t" }` from
/// an unrelated rewrite, would have "proved" that `ALTER TABLE t ADD COLUMN`
/// is represented while the schema change stayed invisible. A DDL statement must
/// be described by a DDL entry, so DML/counter/transaction entries are excluded
/// here. The match is exhaustive with no `_` arm on purpose (see `names_object`).
fn is_schema_change(op: &WalOperation) -> bool {
    match op {
        // Row and bookkeeping traffic — never evidence that DDL was represented.
        WalOperation::Insert { .. }
        | WalOperation::Update { .. }
        | WalOperation::Delete { .. }
        | WalOperation::UpdateCounter { .. }
        | WalOperation::Begin { .. }
        | WalOperation::Commit { .. }
        | WalOperation::Abort { .. } => false,
        // Everything else describes a catalog change.
        WalOperation::Truncate { .. }
        | WalOperation::CreateTable { .. }
        | WalOperation::DropTable { .. }
        | WalOperation::RenameTable { .. }
        | WalOperation::AlterColumnStorage { .. }
        | WalOperation::CreateIndex { .. }
        | WalOperation::DropIndex { .. }
        | WalOperation::CreateTrigger { .. }
        | WalOperation::DropTrigger { .. }
        | WalOperation::CreateFunction { .. }
        | WalOperation::DropFunction { .. }
        | WalOperation::CreateProcedure { .. }
        | WalOperation::DropProcedure { .. }
        | WalOperation::CreateMaterializedView { .. }
        | WalOperation::DropMaterializedView { .. }
        | WalOperation::RefreshMaterializedView { .. }
        | WalOperation::AddConstraint { .. }
        | WalOperation::DropConstraint { .. }
        | WalOperation::AlterTableSchema { .. }
        | WalOperation::CreateView { .. }
        | WalOperation::DropView { .. }
        | WalOperation::CreateSequence { .. }
        | WalOperation::AlterSequence { .. }
        | WalOperation::DropSequence { .. } => true,
    }
}

/// The object must be named by a SCHEMA-CHANGE entry, not merely by some entry.
fn assert_names(ops: &[WalOperation], object: &str, what: &str) {
    assert!(
        ops.iter().any(|op| is_schema_change(op) && names_object(op, object)),
        "{what}: the logical WAL holds NO schema-change entry naming `{object}`, so a replica or \
         CDC consumer never learns this statement happened. Entries emitted: [{}]",
        describe(ops)
    );
}

/// No entry at all was appended. Used by the params-family parity control, where
/// the point is that the statement is REJECTED rather than silently unlogged.
fn assert_no_entries(ops: &[WalOperation], what: &str) {
    assert!(
        ops.is_empty(),
        "{what}: expected the rejected statement to leave the logical WAL untouched, but it \
         appended: [{}]",
        describe(ops)
    );
}

const T: &str = "CREATE TABLE t (id INT PRIMARY KEY, v TEXT)";

/// The fixture for `ALTER COLUMN … DROP NOT NULL`: on `T` the column is already
/// nullable, so the statement is a documented no-op and correctly emits nothing.
/// The statement only changes a schema when the column IS `NOT NULL`.
const T_NOT_NULL: &str = "CREATE TABLE t (id INT PRIMARY KEY, v TEXT NOT NULL)";

// ===========================================================================
// 0. POSITIVE CONTROLS — these pass on the unfixed tree AND after the fix.
//    Without them, an "assert an entry exists" test that vacuously found
//    nothing to read would be indistinguishable from a real defect.
// ===========================================================================

/// The harness reads real, decodable entries out of a real store.
#[test]
fn control_create_table_emits_a_create_table_entry() {
    let ops = emitted(&[], &[T], Family::Text);
    assert!(
        ops.iter()
            .any(|op| matches!(op, WalOperation::CreateTable { table, .. } if table == "t")),
        "positive control: CREATE TABLE must log CreateTable (Catalog::create_table, \
         src/storage/catalog.rs:543). Got: [{}]",
        describe(&ops)
    );
}

/// Same control on the params family, so a later "the params family logs
/// nothing at all" regression cannot hide inside the UNFIXED assertions below.
#[test]
fn control_create_table_emits_a_create_table_entry_params_family() {
    let ops = emitted(&[], &[T], Family::Params);
    assert!(
        ops.iter()
            .any(|op| matches!(op, WalOperation::CreateTable { table, .. } if table == "t")),
        "positive control (params family): CREATE TABLE must log CreateTable. Got: [{}]",
        describe(&ops)
    );
}

/// The isolation mechanism itself: the reopen in `emitted` must leave the
/// setup's entries behind, or every "no entry was emitted" assertion below
/// would be measuring the truncation rather than the statement.
#[test]
fn the_reopen_truncates_the_setup_log() {
    let ops = emitted(&[T, "INSERT INTO t VALUES (1, 'x')"], &[], Family::Text);
    assert!(
        ops.is_empty(),
        "positive control: after the adopt-and-truncate reopen the log must be empty, so the \
         entries observed in every other test belong to the statement under test alone. \
         Got: [{}]",
        describe(&ops)
    );
}

/// DROP TABLE — the other half of the create-copy-drop swap in the issue.
#[test]
fn control_drop_table_emits_a_drop_table_entry() {
    let ops = emitted(&[T], &["DROP TABLE t"], Family::Text);
    assert!(
        ops.iter()
            .any(|op| matches!(op, WalOperation::DropTable { table } if table == "t")),
        "positive control: DROP TABLE must log DropTable (Catalog::drop_table, \
         src/storage/catalog.rs:1067). Got: [{}]",
        describe(&ops)
    );
}

// ===========================================================================
// 1. THE ISSUE'S HEADLINE — ALTER TABLE … RENAME TO.
//    FIXED on this tree (57c808b). These are the regression guard.
// ===========================================================================

#[test]
fn alter_table_rename_to_emits_a_rename_table_entry() {
    let ops = emitted(&[T], &["ALTER TABLE t RENAME TO t2"], Family::Text);
    assert!(
        ops.iter().any(|op| matches!(
            op,
            WalOperation::RenameTable { old_table, new_table } if old_table == "t" && new_table == "t2"
        )),
        "ALTER TABLE … RENAME TO must log RenameTable(t -> t2), or replay/CDC re-applies the old \
         name's CreateTable and resurrects the renamed-away table (GH #36 / #35). Got: [{}]",
        describe(&ops)
    );
}

/// ADVERSARIAL-REVIEW CORRECTION. The first draft of this file asserted that
/// `ALTER TABLE … RENAME TO` on the PARAMS family also logs `RenameTable`, on
/// the belief that src/lib.rs:11418 `execute_alter_table_op` is "the params
/// copy". It is not: its only caller is src/lib.rs:7461, inside the TEXT
/// family's `AlterTableMulti` arm. The params family has no ALTER TABLE dispatch
/// beyond the three constraint arms, so the statement is REJECTED at
/// src/sql/executor/mod.rs:4953 and never reaches `Catalog::rename_table` at all.
/// `tests/rename_table_trigger_tests.rs:161` already pins the rejection.
///
/// What matters for THIS issue is the fail-closed question: does an
/// extended-protocol client's unsupported ALTER fail loudly, or does it "succeed"
/// and leave the log silently short an entry? It fails loudly and appends
/// nothing — so the params-family DDL gap is a PARITY gap, not a divergence gap.
/// PASSES today. It will start failing the moment ALTER parity lands, and the
/// implementer must then convert it into the `emitted(...)` assertion the rest of
/// this file uses, on both families.
#[test]
fn params_family_alter_table_column_ddl_is_rejected_loudly_not_silently_unlogged() {
    for stmt in [
        "ALTER TABLE t RENAME TO t2",
        "ALTER TABLE t ADD COLUMN c INT",
        "ALTER TABLE t DROP COLUMN v",
        "ALTER TABLE t RENAME COLUMN v TO w",
        "ALTER TABLE t ALTER COLUMN v DROP NOT NULL",
    ] {
        // `T_NOT_NULL`: on the nullable `T` the DROP NOT NULL spelling is a
        // documented no-op and would emit nothing on EITHER family.
        let (err, ops) = emitted_after_rejection(&[T_NOT_NULL], stmt, Family::Params);
        if err.is_empty() {
            // ALTER TABLE parity has landed on this tree (the params family
            // routes through the shared ALTER bodies). The fail-closed
            // requirement is then satisfied by REPRESENTATION: the statement
            // must have logged its schema change, on the same terms as the
            // text family.
            assert_names(
                &ops,
                "t",
                &format!("`{stmt}` on the params family (ALTER parity landed)"),
            );
        } else {
            assert_no_entries(&ops, &format!("`{stmt}` was rejected on the params family"));
        }
    }
}

/// A rename must not be re-described as drop+create: that is exactly the
/// create-copy-drop shape from the issue, and replaying it destroys the rows.
#[test]
fn alter_table_rename_to_does_not_log_a_drop_of_the_old_name() {
    let ops = emitted(
        &[T, "INSERT INTO t VALUES (1, 'keep-me')"],
        &["ALTER TABLE t RENAME TO t2"],
        Family::Text,
    );
    assert!(
        !ops.iter()
            .any(|op| matches!(op, WalOperation::DropTable { table } if table == "t")),
        "a RENAME must not be logged as a DROP of the old name — replaying that destroys the \
         renamed table's rows. Got: [{}]",
        describe(&ops)
    );
}

/// `ALTER TABLE … SET SCHEMA` is a key move and goes through the same
/// `Catalog::rename_table`, so it inherits the same entry.
#[test]
fn alter_table_set_schema_emits_a_rename_table_entry() {
    let ops = emitted(
        &[T, "CREATE SCHEMA app"],
        &["ALTER TABLE t SET SCHEMA app"],
        Family::Text,
    );
    assert!(
        ops.iter().any(|op| matches!(
            op,
            WalOperation::RenameTable { old_table, new_table } if old_table == "t" && new_table == "app.t"
        )),
        "ALTER TABLE … SET SCHEMA relocates the storage key and must log RenameTable(t -> app.t). \
         Got: [{}]",
        describe(&ops)
    );
}

// ===========================================================================
// 2. ALREADY REPRESENTED — regression guards for the rest of the covered set.
// ===========================================================================

#[test]
fn create_index_emits_a_create_index_entry() {
    let ops = emitted(&[T], &["CREATE INDEX t_v_idx ON t (v)"], Family::Text);
    assert!(
        ops.iter()
            .any(|op| matches!(op, WalOperation::CreateIndex { name, .. } if name == "t_v_idx")),
        "CREATE INDEX must log CreateIndex (src/sql/executor/ddl.rs:307). Got: [{}]",
        describe(&ops)
    );
}

#[test]
fn drop_index_emits_a_drop_index_entry() {
    let ops = emitted(
        &[T, "CREATE INDEX t_v_idx ON t (v)"],
        &["DROP INDEX t_v_idx"],
        Family::Text,
    );
    assert!(
        ops.iter()
            .any(|op| matches!(op, WalOperation::DropIndex { name } if name == "t_v_idx")),
        "DROP INDEX must log DropIndex (src/sql/executor/ddl.rs:780). Got: [{}]",
        describe(&ops)
    );
}

#[test]
fn truncate_emits_a_truncate_entry() {
    let ops = emitted(
        &[T, "INSERT INTO t VALUES (1, 'x')"],
        &["TRUNCATE TABLE t"],
        Family::Text,
    );
    assert!(
        ops.iter()
            .any(|op| matches!(op, WalOperation::Truncate { table } if table == "t")),
        "TRUNCATE must log Truncate (src/sql/executor/ddl.rs:928). Got: [{}]",
        describe(&ops)
    );
}

// ===========================================================================
// 3. THE GAPS. Every test below FAILS on the current tree.
//
//    Common mechanism: the statement changes the catalog through
//    `Catalog::update_table_schema` / `save_table_constraints` /
//    `ViewCatalog::create_view` / `Catalog::save_sequence`, all of which reach
//    storage via `StorageEngine::put` (src/storage/engine.rs:3436) — a function
//    that appends NOTHING to the logical WAL. And `StorageEngine::delete`
//    (:3476) explicitly skips any `meta:`-prefixed key, which is where the MV,
//    constraint and sequence records live.
// ===========================================================================

// --- 3a. ALTER TABLE column DDL ------------------------------------------

/// ***UNFIXED*** — current tree emits NO entries at all.
///
/// Doubly invisible: `update_table_schema` logs nothing, and
/// `add_column_to_rows` (src/storage/engine.rs:8417) rewrites every `data:` row
/// with a raw `db.put`, also unlogged. A standby keeps the old column list AND
/// the old row images.
#[test]
fn alter_table_add_column_is_represented_in_the_logical_wal() {
    let ops = emitted(&[T], &["ALTER TABLE t ADD COLUMN c INT"], Family::Text);
    assert_names(&ops, "t", "ALTER TABLE … ADD COLUMN");
}

/// ***UNFIXED*** — current tree emits NO entries at all.
#[test]
fn alter_table_drop_column_is_represented_in_the_logical_wal() {
    let ops = emitted(&[T], &["ALTER TABLE t DROP COLUMN v"], Family::Text);
    assert_names(&ops, "t", "ALTER TABLE … DROP COLUMN");
}

/// ***UNFIXED*** — current tree emits NO entries at all.
///
/// This is the column-level twin of the issue's headline: a renamed COLUMN is
/// as unrepresented as a renamed TABLE used to be.
#[test]
fn alter_table_rename_column_is_represented_in_the_logical_wal() {
    let ops = emitted(&[T], &["ALTER TABLE t RENAME COLUMN v TO w"], Family::Text);
    assert_names(&ops, "t", "ALTER TABLE … RENAME COLUMN");
}

/// ***UNFIXED*** — current tree emits NO entries at all.
/// Nullability is a constraint a replica must enforce identically.
///
/// `DROP NOT NULL` rather than `SET NOT NULL` on purpose: the planner
/// (src/sql/planner.rs:5815) implements only the `DropNotNull` spelling, so
/// `SET NOT NULL` would fail with "Unsupported ALTER TABLE operation" and this
/// test would be measuring the parser instead of the WAL.
#[test]
fn alter_table_drop_not_null_is_represented_in_the_logical_wal() {
    let ops = emitted(
        &[T_NOT_NULL, "INSERT INTO t VALUES (1, 'x')"],
        &["ALTER TABLE t ALTER COLUMN v DROP NOT NULL"],
        Family::Text,
    );
    assert_names(&ops, "t", "ALTER TABLE … ALTER COLUMN DROP NOT NULL");
}

/// POSITIVE CONTROL for this section: the ONE `ALTER TABLE ALTER COLUMN`
/// spelling that IS logged (`log_alter_column_storage`,
/// src/storage/engine.rs:9976, called from src/lib.rs:7225). It proves the
/// harness sees ALTER-shaped statements when they do log, so the failures
/// above are about the missing emission and not about the harness.
#[test]
fn control_alter_column_set_storage_emits_an_alter_column_storage_entry() {
    let ops = emitted(
        &[T],
        &["ALTER TABLE t ALTER COLUMN v SET STORAGE DICTIONARY"],
        Family::Text,
    );
    assert!(
        ops.iter().any(|op| matches!(
            op,
            WalOperation::AlterColumnStorage { table, column, .. } if table == "t" && column == "v"
        )),
        "positive control: ALTER COLUMN … SET STORAGE must log AlterColumnStorage. Got: [{}]",
        describe(&ops)
    );
}

// --- 3b. Constraints ------------------------------------------------------

/// ***UNFIXED*** — current tree emits NO entries at all.
///
/// `WalOperation::AddConstraint` and `StorageEngine::log_add_constraint`
/// (src/storage/engine.rs:10164) both exist and have ZERO callers;
/// `alter_table_add_unique` (src/lib.rs:11190) persists the constraint with
/// `save_table_constraints` -> `put` and builds the ART index directly.
/// A replica therefore does not enforce the UNIQUE rule the primary enforces.
#[test]
fn alter_table_add_unique_constraint_is_represented_in_the_logical_wal() {
    let ops = emitted(&[T], &["ALTER TABLE t ADD CONSTRAINT t_v_key UNIQUE (v)"], Family::Text);
    assert_names(&ops, "t", "ALTER TABLE … ADD CONSTRAINT … UNIQUE");
}

/// ***UNFIXED*** — params family. This one really IS routed on both families:
/// `execute_plan_with_params_inner` has an explicit `AlterTableAddUnique` arm at
/// src/lib.rs:14973 calling the SAME `alter_table_add_unique` body
/// (proven independently by tests/prisma_p0_unique_on_conflict.rs:202), so this
/// is a genuine second executor family reaching the same unlogged funnel.
#[test]
fn alter_table_add_unique_constraint_is_represented_params_family() {
    let ops = emitted(
        &[T],
        &["ALTER TABLE t ADD CONSTRAINT t_v_key UNIQUE (v)"],
        Family::Params,
    );
    assert_names(&ops, "t", "ALTER TABLE … ADD CONSTRAINT … UNIQUE (params family)");
}

/// ***UNFIXED*** — current tree emits NO entries at all. A FOREIGN KEY that a
/// replica does not know about is a referential rule silently not enforced.
#[test]
fn alter_table_add_foreign_key_is_represented_in_the_logical_wal() {
    let ops = emitted(
        &[
            "CREATE TABLE parent (id INT PRIMARY KEY)",
            "CREATE TABLE child (id INT PRIMARY KEY, pid INT)",
        ],
        &["ALTER TABLE child ADD CONSTRAINT child_pid_fk FOREIGN KEY (pid) REFERENCES parent (id)"],
        Family::Text,
    );
    assert_names(&ops, "child", "ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY");
}

/// ***UNFIXED*** — current tree emits NO entries at all.
/// `WalOperation::DropConstraint` / `log_drop_constraint` also have zero callers.
#[test]
fn alter_table_drop_constraint_is_represented_in_the_logical_wal() {
    let ops = emitted(
        &[T, "ALTER TABLE t ADD CONSTRAINT t_v_key UNIQUE (v)"],
        &["ALTER TABLE t DROP CONSTRAINT t_v_key"],
        Family::Text,
    );
    assert_names(&ops, "t", "ALTER TABLE … DROP CONSTRAINT");
}

/// ***UNFIXED*** — params family. `AlterTableDropConstraint` is routed
/// explicitly at src/lib.rs:14981 into the same `alter_table_drop_constraint`
/// body, so this covers the extended protocol / REST for the DROP half.
#[test]
fn alter_table_drop_constraint_is_represented_params_family() {
    let ops = emitted(
        &[T, "ALTER TABLE t ADD CONSTRAINT t_v_key UNIQUE (v)"],
        &["ALTER TABLE t DROP CONSTRAINT t_v_key"],
        Family::Params,
    );
    assert_names(&ops, "t", "ALTER TABLE … DROP CONSTRAINT (params family)");
}

/// ***UNFIXED*** — params family, FOREIGN KEY. Routed at src/lib.rs:14989 into
/// the same `alter_table_add_foreign_key` body.
#[test]
fn alter_table_add_foreign_key_is_represented_params_family() {
    let ops = emitted(
        &[
            "CREATE TABLE parent (id INT PRIMARY KEY)",
            "CREATE TABLE child (id INT PRIMARY KEY, pid INT)",
        ],
        &["ALTER TABLE child ADD CONSTRAINT child_pid_fk FOREIGN KEY (pid) REFERENCES parent (id)"],
        Family::Params,
    );
    assert_names(
        &ops,
        "child",
        "ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY (params family)",
    );
}

// --- 3c. Views ------------------------------------------------------------

/// ***UNFIXED*** — current tree emits NO entries at all.
/// `ViewCatalog::create_view` (src/storage/view_catalog.rs:115) stores
/// `__view_metadata__<name>` with `StorageEngine::put`, which never logs.
#[test]
fn create_view_is_represented_in_the_logical_wal() {
    let ops = emitted(&[T], &["CREATE VIEW v1 AS SELECT id FROM t"], Family::Text);
    assert_names(&ops, "v1", "CREATE VIEW");
}

/// ***UNFIXED***. Worse than silent: `ViewCatalog::drop_view` reaches
/// `StorageEngine::delete`, whose key is not `meta:`-prefixed, so the log gets a
/// `Delete { table: "unknown" }` — a bogus DML entry for a table that does not
/// exist, which every replay arm then skips. The drop is unrepresented AND the
/// CDC stream is polluted.
#[test]
fn drop_view_is_represented_in_the_logical_wal() {
    let ops = emitted(
        &[T, "CREATE VIEW v1 AS SELECT id FROM t"],
        &["DROP VIEW v1"],
        Family::Text,
    );
    assert!(
        !ops.iter()
            .any(|op| matches!(op, WalOperation::Delete { table, .. } if table == "unknown")),
        "DROP VIEW logged a bogus `Delete {{ table: \"unknown\" }}` (the view metadata key is not \
         `meta:`-prefixed, so StorageEngine::delete does not skip it). A CDC consumer sees a row \
         delete for a table that does not exist. Got: [{}]",
        describe(&ops)
    );
    assert_names(&ops, "v1", "DROP VIEW");
}

// --- 3d. Materialized views ----------------------------------------------

/// ***UNFIXED*** — the current tree logs `CreateTable(__mv_mv1)` for the
/// BACKING table but nothing that names the view, so a replayed/replicated
/// store holds an orphan `__mv_mv1` table and no materialized view.
/// `log_create_materialized_view` (src/storage/engine.rs:10121) has zero callers.
#[test]
fn create_materialized_view_is_represented_in_the_logical_wal() {
    let ops = emitted(
        &[T, "INSERT INTO t VALUES (1, 'x')"],
        &["CREATE MATERIALIZED VIEW mv1 AS SELECT id FROM t"],
        Family::Text,
    );
    assert_names(&ops, "mv1", "CREATE MATERIALIZED VIEW");
}

/// ***UNFIXED*** — `log_drop_materialized_view` (:10136) has zero callers, and
/// the `meta:mv:<name>` delete is skipped by `StorageEngine::delete`.
#[test]
fn drop_materialized_view_is_represented_in_the_logical_wal() {
    let ops = emitted(
        &[
            T,
            "INSERT INTO t VALUES (1, 'x')",
            "CREATE MATERIALIZED VIEW mv1 AS SELECT id FROM t",
        ],
        &["DROP MATERIALIZED VIEW mv1"],
        Family::Text,
    );
    assert_names(&ops, "mv1", "DROP MATERIALIZED VIEW");
}

/// ***UNFIXED*** — `log_refresh_materialized_view` (:10148) has zero callers.
#[test]
fn refresh_materialized_view_is_represented_in_the_logical_wal() {
    let ops = emitted(
        &[
            T,
            "INSERT INTO t VALUES (1, 'x')",
            "CREATE MATERIALIZED VIEW mv1 AS SELECT id FROM t",
        ],
        &["REFRESH MATERIALIZED VIEW mv1"],
        Family::Text,
    );
    assert_names(&ops, "mv1", "REFRESH MATERIALIZED VIEW");
}

// --- 3e. Sequences --------------------------------------------------------

/// ***UNFIXED*** — current tree emits NO entries at all. Both sequence records
/// (`meta:sequence:<n>`, `meta:seqstate:<n>`) are written with `put` and
/// deleted under the `meta:` skip in `StorageEngine::delete`, so a replica has
/// no sequence and `nextval()` fails there — or, worse, a failover promotes a
/// standby whose sequence high-water is absent and it re-issues served values.
#[test]
fn create_sequence_is_represented_in_the_logical_wal() {
    let ops = emitted(&[], &["CREATE SEQUENCE s1 START WITH 1"], Family::Text);
    assert_names(&ops, "s1", "CREATE SEQUENCE");
}

/// ***UNFIXED*** — current tree emits NO entries at all.
#[test]
fn drop_sequence_is_represented_in_the_logical_wal() {
    let ops = emitted(
        &["CREATE SEQUENCE s1 START WITH 1"],
        &["DROP SEQUENCE s1"],
        Family::Text,
    );
    assert_names(&ops, "s1", "DROP SEQUENCE");
}

/// ***UNFIXED*** — current tree emits NO entries at all.
#[test]
fn alter_sequence_is_represented_in_the_logical_wal() {
    let ops = emitted(
        &["CREATE SEQUENCE s1 START WITH 1"],
        &["ALTER SEQUENCE s1 RESTART WITH 100"],
        Family::Text,
    );
    assert_names(&ops, "s1", "ALTER SEQUENCE");
}

// ===========================================================================
// 4. THE CONSEQUENCE — a replica really does diverge.
//
//    These drive a second, empty engine through the exact path an HA standby
//    uses (`StorageEngine::apply_replicated_operation`, src/storage/engine.rs:
//    10359) with the primary's own entries, and compare the resulting schema.
//    They turn "an entry is missing" into "the replica has a different table".
// ===========================================================================

/// Feed every operation the primary logged into a fresh engine, the way a
/// standby applies a replicated stream, and return the replica.
fn replica_of(ops: &[WalOperation]) -> StorageEngine {
    let replica = StorageEngine::open_in_memory(&Config::in_memory()).expect("replica engine");
    for op in ops {
        replica
            .apply_replicated_operation(op.clone())
            .expect("replica applies the primary's operation");
    }
    replica
}

fn replica_columns(replica: &StorageEngine, table: &str) -> Option<Vec<String>> {
    replica
        .catalog()
        .get_table_schema(table)
        .ok()
        .map(|s| s.columns.iter().map(|c| c.name.clone()).collect())
}

/// POSITIVE CONTROL for section 4: the replication harness works, so a failure
/// below is a real divergence and not a broken replica.
#[test]
fn control_a_replica_reconstructs_a_created_table() {
    let ops = emitted(&[], &[T], Family::Text);
    let replica = replica_of(&ops);
    assert_eq!(
        replica_columns(&replica, "t"),
        Some(vec!["id".to_string(), "v".to_string()]),
        "positive control: a replica fed the primary's CreateTable entry must hold the table. \
         Entries: [{}]",
        describe(&ops)
    );
}

/// POSITIVE CONTROL: the rename really does reach the replica (the #36 fix,
/// end to end rather than by inspecting the log).
#[test]
fn a_replica_follows_alter_table_rename_to() {
    let ops = emitted(&[], &[T, "ALTER TABLE t RENAME TO t2"], Family::Text);
    let replica = replica_of(&ops);
    assert!(
        replica_columns(&replica, "t2").is_some(),
        "the replica must hold the RENAMED table `t2`. Entries: [{}]",
        describe(&ops)
    );
    assert!(
        replica_columns(&replica, "t").is_none(),
        "the replica must NOT still hold the old name `t` — that is the resurrection GH #35/#36 \
         describes. Entries: [{}]",
        describe(&ops)
    );
}

/// ***UNFIXED*** — the replica ends up with `["id", "v"]` while the primary has
/// `["id", "v", "c"]`. No error is raised anywhere; the two stores simply
/// disagree about the table's shape, which is exactly the silent divergence the
/// issue is about.
#[test]
fn a_replica_follows_alter_table_add_column() {
    let ops = emitted(&[], &[T, "ALTER TABLE t ADD COLUMN c INT"], Family::Text);
    let replica = replica_of(&ops);
    assert_eq!(
        replica_columns(&replica, "t"),
        Some(vec!["id".to_string(), "v".to_string(), "c".to_string()]),
        "the replica's column list diverged from the primary's after ALTER TABLE ADD COLUMN. \
         Entries the primary logged: [{}]",
        describe(&ops)
    );
}

/// ***UNFIXED*** — the replica keeps the dropped column.
#[test]
fn a_replica_follows_alter_table_drop_column() {
    let ops = emitted(&[], &[T, "ALTER TABLE t DROP COLUMN v"], Family::Text);
    let replica = replica_of(&ops);
    assert_eq!(
        replica_columns(&replica, "t"),
        Some(vec!["id".to_string()]),
        "the replica kept a column the primary dropped. Entries: [{}]",
        describe(&ops)
    );
}

/// ***UNFIXED*** — the replica keeps the OLD column name.
#[test]
fn a_replica_follows_alter_table_rename_column() {
    let ops = emitted(&[], &[T, "ALTER TABLE t RENAME COLUMN v TO w"], Family::Text);
    let replica = replica_of(&ops);
    assert_eq!(
        replica_columns(&replica, "t"),
        Some(vec!["id".to_string(), "w".to_string()]),
        "the replica kept the pre-rename column name. Entries: [{}]",
        describe(&ops)
    );
}

// ===========================================================================
// 5. A sanity check that the whole file is talking about a live database and
//    not just a log: the primary itself must behave correctly throughout.
//    Passes before and after the fix.
// ===========================================================================

#[test]
fn control_the_primary_itself_is_correct_after_every_ddl_form() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let db = open_db(temp.path());
    db.execute(T).expect("create");
    db.execute("INSERT INTO t VALUES (1, 'x')").expect("insert");
    db.execute("ALTER TABLE t ADD COLUMN c INT").expect("add column");
    db.execute("ALTER TABLE t RENAME COLUMN v TO w").expect("rename column");
    db.execute("ALTER TABLE t RENAME TO t2").expect("rename table");

    let rows = db.query("SELECT id, w, c FROM t2", &[]).expect("select");
    assert_eq!(rows.len(), 1, "the row must survive the DDL sequence");
    assert!(
        matches!(rows[0].values[0], Value::Int4(1) | Value::Int8(1)),
        "positive control: id must still be 1, got {:?}",
        rows[0].values[0]
    );
    assert!(
        matches!(rows[0].values[2], Value::Null),
        "positive control: the added column must read NULL, got {:?}",
        rows[0].values[2]
    );
}
