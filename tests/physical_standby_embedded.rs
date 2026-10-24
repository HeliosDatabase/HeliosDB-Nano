//! Spec P1.4: an `EmbeddedDatabase` serves SQL from a physical standby, never
//! writes, and follows the catalog its applier applies.
//!
//! # Why
//!
//! P1.1's applier keeps the ENGINE's derived state right as the primary's
//! batches arrive. The SQL layer holds state of its own that no batch touches
//! — the function / procedure and trigger registries, the plan, parse and
//! result caches — and every write entry point of the embedded API is one
//! more way to write to a store whose only writer must be the applier.
//!
//! # What
//!
//! Each case stages a physical standby from a primary as P1.1's harness does
//! (`OnlineCheckpointExport` → `ReceiverStaging` → `LocalWriter` →
//! `StorageEngine::open_physical_standby`), serves it with
//! `EmbeddedDatabase::with_physical_standby`, and hands that database to a
//! `PhysicalApplier` as its `DerivedStateListener`.
//!
//! 1. Reads are served, including rows applied after the open, and a clean
//!    open → read → drop makes no write attempt (`WriteGate::refusals() == 0`)
//!    and leaves the directory byte-identical (a file census as in P0.3's
//!    tests).
//! 2. Every write entry point of the embedded API returns the one typed error
//!    and the gate counts no refusal: the SQL layer refused BEFORE the engine.
//! 3. `CREATE FUNCTION` / `CREATE PROCEDURE` / `CREATE VIEW` /
//!    `CREATE TRIGGER` run on the primary — and their replacement and removal —
//!    reach the standby's registries without a restart, and the standby calls
//!    the function through its own registry.
//! 4. Hydration has one owner: a second one is refused, even over a store
//!    with no index for the rebuild's own fresh-registry check to notice.
//!
//! # Red before P1.4
//!
//! `with_physical_standby` did not exist. Behind it: the close path flushed
//! the standby's memtable into new files (1), vector-store writes and
//! lock-free ingestion went straight through (2), the registries kept their
//! open-time contents (3), and a second hydration of a store without an
//! index ran in silence (4).
//!
//! The harness is the minimum copied from `tests/physical_standby_derived_state.rs`
//! (staging) and `tests/physical_standby_local_namespace.rs` (the census):
//! that one serves no SQL, so its pair hydrates through the applier instead.
#![cfg(all(feature = "ha-tier1", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::replication::checkpoint_transfer::{
    OnlineCheckpointExport, PhysicalStage, ReceiverStaging, TransferLimits,
};
use heliosdb_nano::replication::history::load_or_create_primary_history;
use heliosdb_nano::replication::physical::{poll_source, FlushMemo, MAX_BATCH_BYTES};
use heliosdb_nano::replication::physical_applier::{DerivedStateListener, NoopDerivedStateListener, PhysicalApplier};
use heliosdb_nano::storage::lockfree::LockFreeIngestionConfig;
use heliosdb_nano::storage::{StorageEngine, WriteGate};
use heliosdb_nano::{Config, EmbeddedDatabase, Tuple, Value, PHYSICAL_STANDBY_WRITE_REFUSED};
use rocksdb::DB;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A primary's configuration: persistent, with retained RocksDB WAL so a
/// stage can continue from its checkpoint cut. The standby opens with the
/// same configuration over its own path.
fn primary_config(path: &Path) -> Config {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    config.replication.physical.enabled = true;
    config
}

/// Export a checkpoint of the live primary `source`, download it into `root`
/// and open it as a stage. (As in `tests/physical_standby_derived_state.rs`.)
fn stage(source: &DB, history: Uuid, root: &Path, config: &Config) -> PhysicalStage {
    let limits = TransferLimits::default();
    let export = OnlineCheckpointExport::create(source, history, root, limits).unwrap();
    let mut receiver = ReceiverStaging::begin(
        &root.join("standby"),
        &export.manifest_bytes().unwrap(),
        history,
        limits,
    )
    .unwrap();
    for (index, file) in export.manifest().files.iter().enumerate() {
        let mut offset = 0;
        while offset < file.size {
            let bytes = export.read_chunk(index as u32, offset, 64 * 1024).unwrap();
            receiver.append_chunk(index as u32, offset, &bytes).unwrap();
            offset += bytes.len() as u64;
        }
    }
    receiver.finish_download(config).unwrap().open(config).unwrap()
}

/// Every entry of `dir`: a file's SHA-256, or a marker for anything else.
/// (As in `tests/physical_standby_local_namespace.rs`.)
fn census(dir: &Path) -> BTreeMap<String, String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            let content = if kind.is_file() {
                format!("{:x}", Sha256::digest(fs::read(entry.path()).unwrap()))
            } else {
                format!("{kind:?}")
            };
            (entry.file_name().to_string_lossy().into_owned(), content)
        })
        .collect()
}

/// A primary and its physical standby, served by an `EmbeddedDatabase`.
/// Fields drop in declaration order: the applier (which holds the database
/// as its listener) first, then the database, the engine, the stage that
/// owns their RocksDB handle, the primary, and the directory last.
struct Standby {
    applier: Option<PhysicalApplier>,
    database: Option<Arc<EmbeddedDatabase>>,
    engine: Arc<StorageEngine>,
    stage: PhysicalStage,
    raw: Arc<DB>,
    primary: Option<EmbeddedDatabase>,
    history: Uuid,
    root: tempfile::TempDir,
}

impl Standby {
    /// A primary that ran `setup`, a standby staged from it right after, and
    /// the standby's engine — not yet hydrated, with no SQL layer.
    fn engine_only(setup: &[&str]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let config = primary_config(&root.path().join("primary"));
        let primary = EmbeddedDatabase::with_config(config.clone()).unwrap();
        for sql in setup {
            primary
                .execute(sql)
                .unwrap_or_else(|e| panic!("primary setup `{sql}`: {e}"));
        }
        let raw = primary.storage.db();
        let history = load_or_create_primary_history(raw.path()).unwrap();
        let stage = stage(&raw, history, root.path(), &config);
        let path = stage.staging_path().to_path_buf();
        let engine = Arc::new(
            StorageEngine::open_physical_standby(
                &path,
                &primary_config(&path),
                Arc::clone(stage.writer()),
                WriteGate::closed("physical standby"),
            )
            .unwrap(),
        );
        Self {
            applier: None,
            database: None,
            engine,
            stage,
            raw,
            primary: Some(primary),
            history,
            root,
        }
    }

    /// The SQL layer over the standby's engine (it hydrates the engine).
    fn open_database(&self) -> EmbeddedDatabase {
        EmbeddedDatabase::with_physical_standby(primary_config(self.stage.staging_path()), Arc::clone(&self.engine))
            .unwrap_or_else(|e| panic!("with_physical_standby: {e}"))
    }

    /// Serve `database` and start an applier that reports to it.
    fn serve(&mut self, database: EmbeddedDatabase) {
        let database = Arc::new(database);
        let listener: Arc<dyn DerivedStateListener> = database.clone();
        let applier =
            PhysicalApplier::new(Arc::clone(&self.engine), Arc::clone(self.stage.writer()), listener).unwrap();
        self.applier = Some(applier);
        self.database = Some(database);
    }

    /// `engine_only` + `open_database` + `serve`: the production order.
    fn new(setup: &[&str]) -> Self {
        let mut standby = Self::engine_only(setup);
        let database = standby.open_database();
        standby.serve(database);
        standby
    }

    fn db(&self) -> &EmbeddedDatabase {
        self.database.as_deref().expect("the standby is served")
    }

    fn primary(&self) -> &EmbeddedDatabase {
        self.primary.as_ref().expect("the primary is open")
    }

    /// Writes the standby engine's gate refused: each one is a write the SQL
    /// layer let through.
    fn refusals(&self) -> u64 {
        self.engine.write_gate().refusals()
    }

    fn run(&self, statements: &[&str]) {
        for sql in statements {
            self.primary()
                .execute(sql)
                .unwrap_or_else(|e| panic!("primary `{sql}`: {e}"));
        }
    }

    /// Apply everything the primary has. Returns the batches applied.
    fn sync(&self) -> usize {
        let applier = self.applier.as_ref().expect("the standby is served");
        let mut applied = 0;
        loop {
            let after = applier.writer().cursor().source_sequence();
            let poll = poll_source(&self.raw, self.history, after, MAX_BATCH_BYTES, &FlushMemo::new()).unwrap();
            if poll.batches.is_empty() {
                return applied;
            }
            for batch in &poll.batches {
                applier
                    .apply(batch)
                    .unwrap_or_else(|e| panic!("applying {}..={}: {e}", batch.first_sequence(), batch.last_sequence()));
                applied += 1;
            }
        }
    }
}

fn count(rows: &[Tuple]) -> i64 {
    match rows.first().and_then(|row| row.values.first()) {
        Some(Value::Int8(n)) => *n,
        Some(Value::Int4(n)) => i64::from(*n),
        other => panic!("unexpected count {other:?}"),
    }
}

fn values(rows: Vec<Tuple>) -> Vec<Vec<Value>> {
    rows.into_iter().map(|row| row.values).collect()
}

/// `result` is the standby's typed refusal, and the engine never saw it.
fn assert_refused(standby: &Standby, entry: &str, result: heliosdb_nano::Result<()>) {
    match result {
        Ok(()) => panic!("{entry} was served by a physical standby"),
        Err(e) => assert!(
            e.is_physical_standby_write_refusal() && e.to_string().contains(PHYSICAL_STANDBY_WRITE_REFUSED),
            "{entry} must be refused with the standby's typed error, not: {e}"
        ),
    }
    assert_eq!(
        standby.refusals(),
        0,
        "{entry} reached the storage engine: its write gate had to refuse it"
    );
}

// ---------------------------------------------------------------------------
// 1. Reads, and a write-free close
// ---------------------------------------------------------------------------

#[test]
fn a_standby_database_serves_reads_and_writes_nothing_from_open_to_drop() {
    let mut standby = Standby::new(&[
        "CREATE TABLE items (id INT PRIMARY KEY, name TEXT, qty INT)",
        "INSERT INTO items VALUES (1, 'one', 10), (2, 'two', 20), (3, 'three', 30)",
    ]);
    assert!(standby.db().is_physical_standby());
    assert!(!standby.primary().is_physical_standby());
    assert_eq!(standby.refusals(), 0, "the open attempted a write");

    let db = standby.db();
    let total = "SELECT count(*) FROM items";
    for _ in 0..3 {
        // Repeated, so the plan and result caches are warm for the check below.
        assert_eq!(count(&db.query(total, &[]).unwrap()), 3);
    }
    assert_eq!(
        values(db.query("SELECT name FROM items WHERE id = 2", &[]).unwrap()),
        vec![vec![Value::String("two".into())]]
    );
    assert_eq!(
        values(
            db.query_params("SELECT qty FROM items WHERE id = $1", &[Value::Int4(3)])
                .unwrap()
        ),
        vec![vec![Value::Int4(30)]]
    );
    let (_, columns) = db.query_with_columns("SELECT id, name FROM items ORDER BY id").unwrap();
    assert_eq!(columns, vec!["id".to_string(), "name".to_string()]);
    db.execute("SELECT * FROM items").unwrap();
    db.explain("SELECT * FROM items WHERE id = 1").unwrap();
    let session = db.create_wire_session("standby_reader").unwrap();
    assert_eq!(count(&db.query_in_session(session, total, &[]).unwrap()), 3);
    let transaction = db.begin_transaction().unwrap();
    assert_eq!(count(&transaction.query(total, &[]).unwrap()), 3);
    transaction.commit().unwrap();
    db.dump_full(&standby.root.path().join("standby.dump")).unwrap();
    assert_eq!(standby.refusals(), 0, "a read attempted a write");

    // Rows the applier publishes after the open are read through the same
    // warm caches: their publication moves the storage generation the
    // result cache reconciles against.
    standby.run(&[
        "INSERT INTO items VALUES (4, 'four', 40)",
        "UPDATE items SET qty = 21 WHERE id = 2",
    ]);
    assert!(standby.sync() > 0);
    let db = standby.db();
    assert_eq!(count(&db.query(total, &[]).unwrap()), 4);
    assert_eq!(
        values(db.query("SELECT qty FROM items WHERE id = 2", &[]).unwrap()),
        vec![vec![Value::Int4(21)]]
    );
    assert_eq!(standby.refusals(), 0);

    // Drop: no refusal, and not a byte changed in the directory. The applied
    // batches sit in the memtable, so a close-time flush would show.
    let path = standby.stage.staging_path().to_path_buf();
    let before = census(&path);
    drop(standby.applier.take());
    let database = standby.database.take().unwrap();
    assert_eq!(Arc::strong_count(&database), 1, "the applier released its listener");
    drop(database);
    assert_eq!(standby.refusals(), 0, "the close attempted a write");
    assert_eq!(census(&path), before, "the close changed the standby's directory");
    let writer = standby.stage.writer();
    assert_eq!(
        writer.db().latest_sequence_number(),
        writer.cursor().local_sequence(),
        "something wrote to the store around its writer"
    );
}

// ---------------------------------------------------------------------------
// 2. Every write entry point refuses before the engine
// ---------------------------------------------------------------------------

#[test]
fn every_write_entry_point_is_refused_before_it_reaches_the_engine() {
    let mut standby = Standby::engine_only(&[
        "CREATE TABLE items (id INT PRIMARY KEY, name TEXT, qty INT)",
        "INSERT INTO items VALUES (1, 'one', 10), (2, 'two', 20), (3, 'three', 30)",
        "CREATE SEQUENCE ticket START 7",
        "CREATE PROCEDURE add_item() LANGUAGE sql AS $$INSERT INTO items VALUES (99, 'proc', 9)$$",
        "CREATE MATERIALIZED VIEW item_names AS SELECT id, name FROM items",
    ]);
    let sequence = standby.stage.writer().db().latest_sequence_number();

    // `restore_from_dump` takes `&mut self`: before the database is shared.
    // The path does not exist — the refusal comes before the file is opened.
    let mut database = standby.open_database();
    let absent = standby.root.path().join("absent.dump");
    assert_refused(&standby, "restore_from_dump", database.restore_from_dump(&absent));
    assert_refused(
        &standby,
        "restore_tables",
        database.restore_tables(&absent, vec!["items"]),
    );
    standby.serve(database);
    let db = standby.db();

    for sql in [
        "INSERT INTO items VALUES (9, 'nine', 90)",
        "UPDATE items SET qty = 0",
        "DELETE FROM items WHERE id = 1",
        "INSERT INTO items VALUES (10, 'ten', 100) ON CONFLICT (id) DO NOTHING",
        "MERGE INTO items USING items AS s ON items.id = s.id WHEN MATCHED THEN UPDATE SET qty = 1",
        "TRUNCATE TABLE items",
        "CREATE TABLE more (id INT PRIMARY KEY)",
        "ALTER TABLE items ADD COLUMN tag TEXT",
        "CREATE INDEX items_qty_idx ON items (qty)",
        "DROP TABLE items",
        "CREATE VIEW v AS SELECT id FROM items",
        "CREATE FUNCTION f(x INT) RETURNS INT AS $$ SELECT $1 $$ LANGUAGE sql",
        "SELECT id INTO items_copy FROM items",
        "SELECT * FROM items FOR UPDATE",
        "CALL add_item()",
        "REFRESH MATERIALIZED VIEW item_names",
        "VACUUM VERSIONS",
        "ANALYZE items",
        "CREATE BRANCH dev",
        "SELECT 1; INSERT INTO items VALUES (11, 'eleven', 110)",
    ] {
        assert_refused(&standby, &format!("execute `{sql}`"), db.execute(sql).map(drop));
    }
    let error = db
        .execute("INSERT INTO items VALUES (12, 'x', 1)")
        .unwrap_err()
        .to_string();
    assert!(error.contains("INSERT"), "the refusal names the statement: {error}");

    // One call at a time, so a gate refusal is blamed on the call that caused it.
    let insert = "INSERT INTO items VALUES ($1, 'p', 1)";
    let returning = "UPDATE items SET qty = $1 RETURNING id";
    let refused = |entry: &str, result: heliosdb_nano::Result<()>| assert_refused(&standby, entry, result);
    refused("query nextval", db.query("SELECT nextval('ticket')", &[]).map(drop));
    refused(
        "query INSERT … RETURNING",
        db.query("INSERT INTO items VALUES (13, 'r', 1) RETURNING id", &[])
            .map(drop),
    );
    refused(
        "query_with_columns DELETE … RETURNING",
        db.query_with_columns("DELETE FROM items RETURNING id").map(drop),
    );
    refused(
        "execute_params",
        db.execute_params(insert, &[Value::Int4(14)]).map(drop),
    );
    refused(
        "execute_many_params",
        db.execute_many_params(insert, &[vec![Value::Int4(15)], vec![Value::Int4(16)]])
            .map(drop),
    );
    refused(
        "execute_returning",
        db.execute_returning("DELETE FROM items WHERE id = 1 RETURNING id")
            .map(drop),
    );
    refused(
        "execute_params_returning",
        db.execute_params_returning(returning, &[Value::Int4(5)]).map(drop),
    );
    refused(
        "query_params UPDATE",
        db.query_params(returning, &[Value::Int4(6)]).map(drop),
    );
    refused(
        "query_params_with_columns UPDATE",
        db.query_params_with_columns(returning, &[Value::Int4(7)]).map(drop),
    );
    refused(
        "execute_batch",
        db.execute_batch(&["SELECT 1", "INSERT INTO items VALUES (17, 'b', 1)"])
            .map(drop),
    );
    refused(
        "execute_sql_script",
        db.execute_sql_script("SELECT 1; INSERT INTO items VALUES (18, 's', 1);")
            .map(drop),
    );
    refused(
        "insert_vectors",
        db.insert_vectors("embeddings", vec![vec![0.1, 0.2]]).map(drop),
    );
    refused("create_vector_store", db.create_vector_store("embeddings", 2).map(drop));
    refused(
        "upsert_vectors",
        db.upsert_vectors("embeddings", vec![("vec_1".to_string(), vec![0.3, 0.4])]),
    );
    refused(
        "delete_vectors",
        db.delete_vectors("embeddings", vec!["vec_1".to_string()]),
    );
    refused("delete_vector_store", db.delete_vector_store("embeddings"));
    refused(
        "refresh_materialized_view",
        db.refresh_materialized_view("item_names").map(drop),
    );
    refused("create_branch", db.create_branch("dev").map(drop));
    refused("switch_branch", db.switch_branch("dev").map(drop));
    refused("merge_branch", db.merge_branch("dev").map(drop));
    refused("drop_branch", db.drop_branch("dev").map(drop));
    refused("vacuum_versions", db.vacuum_versions().map(drop));
    refused(
        "enable_lockfree_ingestion",
        db.storage
            .enable_lockfree_ingestion(LockFreeIngestionConfig::for_bulk_load()),
    );

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert_refused(
        &standby,
        "start_auto_refresh",
        runtime.block_on(db.start_auto_refresh(None)),
    );
    assert!(!db.is_auto_refresh_running());

    #[cfg(feature = "code-graph")]
    assert_refused(
        &standby,
        "lsp_rename_apply",
        db.lsp_rename_apply(1, "renamed", &heliosdb_nano::code_graph::RenameApplyOptions::default())
            .map(drop),
    );

    // The session and transaction families.
    let session = db.create_wire_session("standby_writer").unwrap();
    assert_refused(
        &standby,
        "execute_for_session",
        db.execute_for_session(session, "INSERT INTO items VALUES (19, 'w', 1)")
            .map(drop),
    );
    assert_refused(
        &standby,
        "execute_in_session",
        db.execute_in_session(session, "DELETE FROM items").map(drop),
    );
    assert_refused(
        &standby,
        "execute_params_for_session",
        db.execute_params_for_session(session, insert, &[Value::Int4(20)])
            .map(drop),
    );
    let transaction = db.begin_transaction().unwrap();
    assert_refused(
        &standby,
        "Transaction::execute",
        transaction.execute("INSERT INTO items VALUES (21, 't', 1)").map(drop),
    );
    assert_refused(
        &standby,
        "Transaction::query",
        transaction.query("SELECT nextval('ticket')", &[]).map(drop),
    );
    // A refusal is raised before the statement enters the transaction, so
    // the transaction is still usable.
    assert_eq!(count(&transaction.query("SELECT count(*) FROM items", &[]).unwrap()), 3);
    transaction.rollback().unwrap();
    db.execute("BEGIN").unwrap();
    assert_refused(
        &standby,
        "execute inside BEGIN",
        db.execute("INSERT INTO items VALUES (22, 'g', 1)").map(drop),
    );
    assert_eq!(count(&db.query("SELECT count(*) FROM items", &[]).unwrap()), 3);
    db.execute("COMMIT").unwrap();

    // Nothing changed, nothing was attempted.
    assert_eq!(count(&db.query("SELECT count(*) FROM items", &[]).unwrap()), 3);
    assert!(db.query("SELECT * FROM items_copy", &[]).is_err());
    assert_eq!(standby.stage.writer().db().latest_sequence_number(), sequence);
    assert_eq!(standby.refusals(), 0);
}

// ---------------------------------------------------------------------------
// 3. Catalog changes reach the SQL layer
// ---------------------------------------------------------------------------

#[test]
fn routines_views_and_triggers_from_the_primary_are_usable_without_a_restart() {
    let mut standby = Standby::new(&[
        "CREATE TABLE items (id INT PRIMARY KEY, name TEXT, qty INT)",
        "INSERT INTO items VALUES (1, 'one', 10), (2, 'two', 20), (3, 'three', 30), (4, 'four', 40)",
    ]);
    assert!(!standby.db().function_registry.function_exists("dbl"));

    standby.run(&[
        "CREATE FUNCTION dbl(x INT) RETURNS INT AS $$ SELECT $1 * 2 $$ LANGUAGE sql",
        "CREATE FUNCTION trpl(x INT) RETURNS INT AS $$ SELECT $1 * 3 $$ LANGUAGE sql",
        "CREATE PROCEDURE add_item() LANGUAGE sql AS $$INSERT INTO items VALUES (99, 'proc', 9)$$",
        "CREATE VIEW small_items AS SELECT id, name FROM items WHERE id < 3",
        "CREATE FUNCTION tag_rewrite() RETURNS TRIGGER AS $$ BEGIN NEW.name = 'tagged'; RETURN NEW; END $$ LANGUAGE plpgsql",
        "CREATE TRIGGER items_tag BEFORE INSERT ON items FOR EACH ROW EXECUTE FUNCTION tag_rewrite()",
    ]);
    assert!(standby.sync() > 0);
    let db = standby.db();
    let functions = &db.function_registry;
    assert!(
        functions.function_exists("dbl"),
        "CREATE FUNCTION did not reach the registry"
    );
    assert!(functions.function_exists("trpl"));
    assert!(
        functions.procedure_exists("add_item"),
        "CREATE PROCEDURE did not reach the registry"
    );
    assert!(
        db.trigger_registry.trigger_exists("items", "items_tag").unwrap(),
        "CREATE TRIGGER did not reach the registry"
    );
    assert!(
        db.trigger_registry.has_row_mutations("items"),
        "the trigger's rewrite recipe did not follow its definition"
    );
    let view = "SELECT id FROM small_items ORDER BY id";
    for _ in 0..3 {
        // Repeated: the plan cache holds this statement from here on.
        assert_eq!(
            values(db.query(view, &[]).unwrap()),
            vec![vec![Value::Int4(1)], vec![Value::Int4(2)]]
        );
    }

    // Replace one function, drop the other routines and the trigger, and
    // redefine the view under the same name.
    standby.run(&[
        "CREATE OR REPLACE FUNCTION dbl(x INT) RETURNS INT AS $$ SELECT $1 * 5 $$ LANGUAGE sql",
        "DROP FUNCTION trpl",
        "DROP PROCEDURE add_item",
        "DROP TRIGGER items_tag ON items",
        "DROP VIEW small_items",
        "CREATE VIEW small_items AS SELECT id, name FROM items WHERE id < 5",
    ]);
    assert!(standby.sync() > 0);
    let db = standby.db();
    let functions = &db.function_registry;
    let body = functions
        .get_function("dbl")
        .expect("dbl survives its replacement")
        .body;
    assert!(body.contains("* 5"), "CREATE OR REPLACE kept the old body: {body}");
    assert!(
        !functions.function_exists("trpl"),
        "DROP FUNCTION did not reach the registry"
    );
    assert!(
        !functions.procedure_exists("add_item"),
        "DROP PROCEDURE did not reach the registry"
    );
    assert!(!db.trigger_registry.trigger_exists("items", "items_tag").unwrap());
    assert!(
        !db.trigger_registry.has_row_mutations("items"),
        "a dropped trigger's recipe stayed attached"
    );
    let redefined = values(db.query(view, &[]).unwrap());
    assert_eq!(redefined, values(standby.primary().query(view, &[]).unwrap()));
    assert_eq!(redefined.len(), 4, "the standby answered from the old view definition");

    // The standby calls the function through its OWN registry: the
    // primary's (whose registry also defines `dbl`) is gone.
    let expected = values(standby.primary().query("SELECT dbl(21)", &[]).unwrap());
    drop(standby.primary.take());
    assert_eq!(values(standby.db().query("SELECT dbl(21)", &[]).unwrap()), expected);
    assert_eq!(standby.refusals(), 0);
}

// ---------------------------------------------------------------------------
// 4. One hydration
// ---------------------------------------------------------------------------

#[test]
fn a_standby_engine_is_hydrated_once_by_its_sql_layer() {
    // No table, so no index: the rebuild's own fresh-registry check cannot
    // see a second hydration here.
    let standby = Standby::engine_only(&[]);
    let database = standby.open_database();
    assert!(database.is_physical_standby());
    let applier = PhysicalApplier::new(
        Arc::clone(&standby.engine),
        Arc::clone(standby.stage.writer()),
        Arc::new(NoopDerivedStateListener),
    )
    .unwrap();
    assert!(
        applier.hydrate().is_err(),
        "the applier hydrated an engine its SQL layer had hydrated"
    );
    assert!(
        EmbeddedDatabase::with_physical_standby(
            primary_config(standby.stage.staging_path()),
            Arc::clone(&standby.engine)
        )
        .is_err(),
        "a second SQL layer hydrated the same engine"
    );

    // Only a physical standby's engine is accepted.
    let path = standby.root.path().join("plain");
    let config = primary_config(&path);
    let plain = Arc::new(StorageEngine::open(&path, &config).unwrap());
    assert!(EmbeddedDatabase::with_physical_standby(config, plain).is_err());
    assert_eq!(standby.refusals(), 0);
}
