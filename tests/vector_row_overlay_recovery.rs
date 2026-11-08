//! Persistent row images survive a heap write that stops before graph publication.
#![cfg(feature = "vector-persist")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
use heliosdb_nano::storage::{StorageEngine, VectorIndexType};
#[cfg(feature = "internal-tests")]
use heliosdb_nano::storage::{WalEntry, WalOperation};
use heliosdb_nano::{Config, EmbeddedDatabase, Tuple, Value};
#[cfg(feature = "internal-tests")]
use rocksdb::WriteBatchIterator;
use rocksdb::DB;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const TABLE: &str = "overlay_rows";
const INDEX: &str = "overlay_graph";
fn config(path: &Path, history: bool) -> Config {
    let mut cfg = Config::default();
    cfg.storage.path = Some(path.to_path_buf());
    cfg.storage.memory_only = false;
    cfg.storage.time_travel_enabled = history;
    cfg.storage.wal_enabled = true;
    cfg.storage.logical_wal_per_statement = false;
    cfg.storage.wal_checkpoint_interval_entries = 0;
    cfg.storage.wal_checkpoint_interval_secs = 0;
    cfg.storage.wal_checkpoint_on_close = false;
    cfg.storage.flush_on_close = false;
    cfg.storage.version_gc_interval_secs = Some(0);
    cfg.replication.physical.enabled = true;
    cfg
}
fn open(path: &Path, history: bool) -> EmbeddedDatabase {
    EmbeddedDatabase::with_config(config(path, history)).unwrap()
}
#[cfg(feature = "internal-tests")]
fn tuple(id: i32, vector: Vec<f32>) -> Tuple {
    Tuple::new(vec![Value::Int4(id), Value::Vector(vector)])
}
fn seed(db: &EmbeddedDatabase, precision: &str, pq: bool) {
    db.execute("CREATE TABLE overlay_rows (id INT PRIMARY KEY, embedding VECTOR(4))")
        .unwrap();
    let values = (1..=32)
        .map(|id| format!("({id}, '[{},0,0,1]')", 1000 + id * 100))
        .collect::<Vec<_>>()
        .join(",");
    db.execute(&format!("INSERT INTO overlay_rows VALUES {values}"))
        .unwrap();
    db.execute(&format!("CREATE INDEX overlay_graph ON overlay_rows USING hnsw (embedding) WITH (persistent=true, rerank_precision='{precision}'{})", if pq { ", quantization='product', pq_subquantizers=2, pq_centroids=4" } else { "" })).unwrap();
    assert_mode(&db.storage, precision, pq);
    assert_graph(&db.storage, 32, 1);
}
fn assert_mode(engine: &StorageEngine, precision: &str, pq: bool) {
    let VectorIndexType::Persistent(mode) = engine.vector_indexes().get_metadata(INDEX).unwrap().index_type else {
        panic!("persistent backend must be retained")
    };
    assert_eq!(mode.rerank_precision, precision);
    assert_eq!(mode.pq_enabled, pq);
}
fn image(db: &DB) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut opts = rocksdb::ReadOptions::default();
    opts.set_total_order_seek(true);
    db.iterator_opt(rocksdb::IteratorMode::Start, opts)
        .map(|item| {
            let (key, value) = item.unwrap();
            (key.to_vec(), value.to_vec())
        })
        .collect()
}
#[cfg(feature = "internal-tests")]
fn base_graph(db: &DB) -> Vec<(Vec<u8>, Vec<u8>)> {
    image(db)
        .into_iter()
        .filter(|(key, _)| {
            key.starts_with(b"__vidx:") && !key.windows(b":row_image:".len()).any(|part| part == b":row_image:")
        })
        .collect()
}
fn assert_graph(engine: &StorageEngine, count: usize, nearest: i32) {
    let schema = engine.catalog().get_table_schema(TABLE).unwrap();
    let rows: BTreeMap<_, _> = engine
        .scan_table_with_schema(TABLE, &schema)
        .unwrap()
        .into_iter()
        .map(|row| {
            let Value::Int4(id) = row.values[0] else {
                panic!("unexpected row {row:?}")
            };
            (row.row_id.unwrap(), id)
        })
        .collect();
    assert_eq!(rows.len(), count);
    let graph = engine.vector_indexes();
    assert!(
        graph
            .get_index_stats(INDEX)
            .unwrap()
            .quantization
            .contains("Persistent"),
        "same persistent backend retained"
    );
    assert_eq!(graph.index_live_size(INDEX), Some(count));
    let found = graph.search(INDEX, &vec![0.0; 4], count + 1).unwrap();
    assert_eq!(found.len(), count, "observe every live graph member");
    assert_eq!(
        found.iter().map(|(row, _)| *row).collect::<BTreeSet<_>>(),
        rows.keys().copied().collect()
    );
    assert_eq!(rows[&found[0].0], nearest);
}
fn read_only(path: &Path, history: bool) -> StorageEngine {
    let engine = StorageEngine::open_read_only(path, &config(path, history)).unwrap();
    engine.catalog().rebuild_all_indexes().unwrap();
    engine
}
fn run_sql(db: &EmbeddedDatabase, params: bool, sql: &str) {
    if params {
        db.execute_params(sql, &[]).unwrap();
    } else {
        db.execute(sql).unwrap();
    }
}

#[test]
fn savepoint_rollback_and_commit_publish_only_final_persistent_membership() {
    let mut cells = 0;
    for precision in ["f32", "f16", "i8"] {
        for pq in [false, true] {
            for history in [false, true] {
                for params in [false, true] {
                    let root = tempfile::tempdir().unwrap();
                    let db = open(root.path(), history);
                    seed(&db, precision, pq);
                    for sql in [
                        "BEGIN",
                        "SAVEPOINT discarded",
                        "DELETE FROM overlay_rows WHERE id=1",
                        "INSERT INTO overlay_rows VALUES (99,'[0,0,0,0]')",
                        "ROLLBACK TO discarded",
                        "UPDATE overlay_rows SET embedding='[60000,0,0,0]' WHERE id=1",
                        "COMMIT",
                    ] {
                        run_sql(&db, params, sql);
                    }
                    assert_graph(&db.storage, 32, 2);
                    assert!(db
                        .query("SELECT id FROM overlay_rows WHERE id=99", &[])
                        .unwrap()
                        .is_empty());
                    let before = image(&db.storage.db());
                    let sequence = db.storage.db().latest_sequence_number();
                    let ro = read_only(root.path(), history);
                    assert_mode(&ro, precision, pq);
                    assert_graph(&ro, 32, 2);
                    assert_eq!(ro.write_gate().refusals(), 0);
                    drop(ro);
                    assert_eq!(image(&db.storage.db()), before);
                    assert_eq!(db.storage.db().latest_sequence_number(), sequence);
                    drop(db);
                    let reopened = open(root.path(), history);
                    assert_mode(&reopened.storage, precision, pq);
                    assert_graph(&reopened.storage, 32, 2);
                    cells += 1;
                }
            }
        }
    }
    assert_eq!(cells, 24, "all precision/PQ/history/route cells executed");
    eprintln!("vector-overlay-receipt savepoint={cells}");
}

#[test]
fn unversioned_columnar_vector_transaction_uses_pending_sidecars_in_its_authoritative_image() {
    let mut cells = 0;
    for precision in ["f32", "f16", "i8"] {
        for pq in [false, true] {
            for params in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let db = open(root.path(), false);
                seed(&db, precision, pq);
                run_sql(
                    &db,
                    params,
                    "ALTER TABLE overlay_rows ALTER COLUMN embedding SET STORAGE COLUMNAR",
                );
                for sql in [
                    "BEGIN",
                    "UPDATE overlay_rows SET embedding='[60000,0,0,0]' WHERE id=1",
                    "COMMIT",
                ] {
                    run_sql(&db, params, sql);
                }
                assert_graph(&db.storage, 32, 2);
                drop(db);
                let ro = read_only(root.path(), false);
                assert_mode(&ro, precision, pq);
                let before = image(&ro.db());
                assert_graph(&ro, 32, 2);
                assert_eq!(image(&ro.db()), before);
                assert_eq!(ro.write_gate().refusals(), 0);
                drop(ro);
                let reopened = open(root.path(), false);
                assert_mode(&reopened.storage, precision, pq);
                assert_graph(&reopened.storage, 32, 2);
                cells += 1;
            }
        }
    }
    assert_eq!(cells, 12);
    eprintln!("vector-overlay-receipt unversioned_columnar={cells}");
}

#[cfg(feature = "internal-tests")]
#[derive(Default, Debug)]
struct Group {
    keys: Vec<Vec<u8>>,
    operations: Vec<WalOperation>,
}
#[cfg(feature = "internal-tests")]
impl WriteBatchIterator for Group {
    fn put(&mut self, key: Box<[u8]>, value: Box<[u8]>) {
        if key.starts_with(b"wal:entries:") {
            self.operations.push(WalEntry::deserialize(&value).unwrap().operation);
        }
        self.keys.push(key.into_vec());
    }
    fn delete(&mut self, key: Box<[u8]>) {
        self.keys.push(key.into_vec());
    }
}
#[cfg(feature = "internal-tests")]
fn assert_owning_group(db: &DB, after: u64) {
    let mut updates = db.get_updates_since(after).unwrap();
    let mut groups = Vec::new();
    for item in updates.by_ref() {
        let (_, batch) = item.unwrap();
        let mut group = Group::default();
        batch.iterate(&mut group);
        groups.push(group);
    }
    updates.status().unwrap();
    let data: Vec<_> = groups
        .iter()
        .filter(|group| group.keys.iter().any(|key| key.starts_with(b"data:overlay_rows:")))
        .collect();
    assert_eq!(data.len(), 1, "one native owning row group: {groups:#?}");
    assert!(
        data[0]
            .keys
            .iter()
            .any(|key| key.starts_with(b"__vidx:")
                && key.windows(b":row_image:".len()).any(|part| part == b":row_image:")),
        "authoritative image is in the heap group: {groups:#?}"
    );
    for group in &groups {
        if group.operations.iter().any(|op| matches!(op, WalOperation::Insert{table,..}|WalOperation::Update{table,..}|WalOperation::Delete{table,..} if table==TABLE)) {
            assert!(group.keys.iter().any(|key| key.starts_with(b"data:overlay_rows:")), "logical row history never escapes its data group");
        }
    }
}

#[cfg(feature = "internal-tests")]
mod internal {
    use super::*;
    const CHILD: &str = "NANO_VECTOR_OVERLAY_CHILD_PATH";
    const ACTION: &str = "NANO_VECTOR_OVERLAY_CHILD_ACTION";
    const HISTORY: &str = "NANO_VECTOR_OVERLAY_CHILD_HISTORY";
    const KEY_FILE: &str = "NANO_VECTOR_OVERLAY_CHILD_KEY_FILE";
    const NAME: &str =
        "internal::every_heap_before_graph_crash_reopens_same_persistent_backend_read_only_without_writes";
    pub(super) fn crash(path: &Path, history: bool, action: &str, key: Option<&Path>) -> u64 {
        let tag = match action {
            "insert" | "fast_insert" => "row_insert",
            "update" | "update_no_index" => "row_update",
            "delete" | "delete_pk" => "row_delete",
            "batch" => "row_batch",
            "replication" => "row_replication",
            _ => "row_transaction",
        };
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", NAME, "--nocapture"])
            .env(CHILD, path)
            .env(ACTION, action)
            .env(HISTORY, if history { "1" } else { "0" })
            .env_remove(KEY_FILE)
            .env("HELIOSDB_NANO_TEST_EXIT_AFTER_DDL_WRITE", format!("{tag}:{TABLE}"));
        if let Some(key) = key {
            child.env(KEY_FILE, key);
        }
        let status = child.status().unwrap();
        assert_eq!(status.code(), Some(0), "actual {action} crash: {status}");
        std::fs::read_to_string(path.join("overlay-child-ready"))
            .unwrap()
            .parse()
            .unwrap()
    }
    pub(super) fn assert_quarantined(db: &EmbeddedDatabase) {
        assert_table_quarantined(db, TABLE);
    }
    pub(super) fn assert_table_quarantined(db: &EmbeddedDatabase, table: &str) {
        let error = db.query(&format!("SELECT * FROM {table}"), &[]).unwrap_err();
        match error {
            heliosdb_nano::Error::Storage(message) => assert_eq!(
                message,
                format!(
                    "table '{table}' is unavailable after materialized view reconciliation failed; reopen to rebuild"
                )
            ),
            other => panic!("expected the table quarantine error, got {other}"),
        }
    }

    pub(super) fn mutate(db: &EmbeddedDatabase, action: &str) -> heliosdb_nano::Result<()> {
        let schema = db.storage.catalog().get_table_schema(TABLE).unwrap();
        let pk = Value::Int4(1);
        let pk_key = heliosdb_nano::storage::ArtIndexManager::encode_key(std::slice::from_ref(&pk));
        let row = db.storage.art_indexes().pk_index_lookup(TABLE, &pk_key).unwrap();
        let _row_guard = if matches!(action, "update" | "update_no_index" | "delete" | "delete_pk") {
            Some(db.storage.lock_row_for_fast_write(TABLE, row)?)
        } else {
            None
        };
        // The direct fast API requires this guard to precede the old-row read.
        let old = db.storage.get_row_by_id(TABLE, row, &schema)?.unwrap();
        match action {
            "insert" => {
                db.storage
                    .insert_tuple_versioned_with_schema(TABLE, tuple(99, vec![0.0; 4]), &schema)?;
            }
            "fast_insert" => {
                db.storage.insert_tuple_fast(TABLE, tuple(99, vec![0.0; 4]), &schema)?;
            }
            "update" => {
                db.storage
                    .update_tuple_fast(TABLE, row, tuple(1, vec![60000.0, 0.0, 0.0, 0.0]), &old, &schema)?;
            }
            "update_no_index" => {
                db.storage.update_tuple_fast_no_index(
                    TABLE,
                    row,
                    tuple(1, vec![60000.0, 0.0, 0.0, 0.0]),
                    &schema,
                    Some(&old),
                )?;
            }
            "delete" => {
                db.storage.delete_tuple_fast(TABLE, row, &old, &schema)?;
            }
            "delete_pk" => {
                let pk = Value::Int4(1);
                let key = heliosdb_nano::storage::ArtIndexManager::encode_key(std::slice::from_ref(&pk));
                db.storage.delete_tuple_fast_pk_only(TABLE, row, &key, &pk)?;
            }
            "batch" => {
                let id = db.storage.next_row_id(TABLE)?;
                db.storage
                    .insert_prepared_tuples_fast_batch(TABLE, vec![(id, tuple(99, vec![0.0; 4]))], &schema)?;
            }
            "replication" => {
                db.storage.apply_replicated_operation_at(
                    100,
                    WalOperation::Update {
                        table: TABLE.into(),
                        key: format!("data:{TABLE}:{row}").into_bytes(),
                        tuple: bincode::serialize(&tuple(1, vec![60000.0, 0.0, 0.0, 0.0])).unwrap(),
                    },
                )?;
            }
            "transaction" => {
                for sql in [
                    "BEGIN",
                    "UPDATE overlay_rows SET embedding='[60000,0,0,0]' WHERE id=1",
                    "COMMIT",
                ] {
                    db.execute(sql)?;
                }
            }
            _ => panic!("unknown action {action}"),
        }
        Ok(())
    }
    fn expected(action: &str) -> (usize, i32) {
        match action {
            "insert" | "fast_insert" | "batch" => (33, 99),
            "delete" | "delete_pk" => (31, 2),
            _ => (32, 2),
        }
    }
    #[test]
    fn every_heap_before_graph_crash_reopens_same_persistent_backend_read_only_without_writes() {
        if let Ok(path) = std::env::var(CHILD) {
            let history = std::env::var(HISTORY).unwrap() == "1";
            #[allow(unused_mut)]
            let mut cfg = config(Path::new(&path), history);
            if let Ok(key) = std::env::var(KEY_FILE) {
                #[cfg(feature = "encryption")]
                {
                    cfg.encryption.enabled = true;
                    cfg.encryption.key_source = heliosdb_nano::KeySource::File(key.into());
                }
                #[cfg(not(feature = "encryption"))]
                panic!("encrypted child requested without encryption feature: {key}");
            }
            let db = EmbeddedDatabase::with_config(cfg).unwrap();
            let action = std::env::var(ACTION).unwrap();
            std::fs::write(
                Path::new(&path).join("overlay-child-ready"),
                db.storage.db().latest_sequence_number().to_string(),
            )
            .unwrap();
            mutate(&db, &action).unwrap();
            panic!("native row pre-publication crash hook did not fire");
        }
        let mut cells = 0;
        for precision in ["f32", "f16", "i8"] {
            for pq in [false, true] {
                for history in [false, true] {
                    for action in [
                        "insert",
                        "fast_insert",
                        "update",
                        "update_no_index",
                        "delete",
                        "delete_pk",
                        "batch",
                        "transaction",
                    ] {
                        let root = tempfile::tempdir().unwrap();
                        let old_graph = {
                            let db = open(root.path(), history);
                            seed(&db, precision, pq);
                            base_graph(&db.storage.db())
                        };
                        let before = crash(root.path(), history, action, None);
                        let ro = read_only(root.path(), history);
                        assert_mode(&ro, precision, pq);
                        assert!(
                            ro.db().latest_sequence_number() > before,
                            "native owning group really applied"
                        );
                        assert_owning_group(&ro.db(), before);
                        assert_eq!(
                            base_graph(&ro.db()),
                            old_graph,
                            "crash truly preceded graph-key publication"
                        );
                        let bytes = image(&ro.db());
                        let sequence = ro.db().latest_sequence_number();
                        let (count, nearest) = expected(action);
                        assert_graph(&ro, count, nearest);
                        assert_eq!(ro.write_gate().refusals(), 0);
                        assert_eq!(ro.db().latest_sequence_number(), sequence);
                        assert_eq!(image(&ro.db()), bytes);
                        drop(ro);
                        let db = open(root.path(), history);
                        assert_mode(&db.storage, precision, pq);
                        assert_graph(&db.storage, count, nearest);
                        cells += 1;
                    }
                }
            }
        }
        assert_eq!(cells, 96, "all actual child precision/PQ/history/owner cells executed");
        eprintln!("vector-overlay-receipt crash={cells}");
    }

    #[test]
    fn native_refusals_preserve_old_graph_and_unknown_applied_errors_refuse_until_reopen() {
        let mut cells = 0;
        for history in [false, true] {
            for action in [
                "insert",
                "fast_insert",
                "update",
                "update_no_index",
                "delete",
                "delete_pk",
            ] {
                for unknown in [false, true] {
                    let root = tempfile::tempdir().unwrap();
                    let db = open(root.path(), history);
                    seed(&db, "f32", false);
                    let before = db.storage.db().latest_sequence_number();
                    if unknown {
                        db.storage.write_gate().debug_fail_next_native_write_after_apply();
                    } else {
                        db.storage.debug_set_write_group_ceiling(Some(64));
                    }
                    // The same real direct API as the crash cells, preserving its error.
                    let error = mutate(&db, action).unwrap_err();
                    db.storage.debug_set_write_group_ceiling(None);
                    if unknown {
                        assert!(
                            error
                                .to_string()
                                .contains("injected native write error after apply; outcome is unknown"),
                            "{error}"
                        );
                    } else {
                        assert!(
                            error
                                .to_string()
                                .contains(heliosdb_nano::BRANCH_MATERIALIZE_TOO_LARGE_MARKER),
                            "{error}"
                        );
                    }
                    if unknown {
                        assert!(db.storage.db().latest_sequence_number() > before);
                        assert_quarantined(&db);
                        drop(db);
                        let reopened = open(root.path(), history);
                        let (count, nearest) = expected(action);
                        assert_graph(&reopened.storage, count, nearest);
                    } else {
                        assert_graph(&db.storage, 32, 1);
                        db.storage
                            .insert_tuple_fast(
                                TABLE,
                                tuple(99, vec![0.0; 4]),
                                &db.storage.catalog().get_table_schema(TABLE).unwrap(),
                            )
                            .unwrap();
                        assert_graph(&db.storage, 33, 99);
                    }
                    cells += 1;
                }
            }
        }
        assert_eq!(cells, 24);
        eprintln!("vector-overlay-receipt native_outcome={cells}");
    }
}

#[cfg(all(feature = "internal-tests", feature = "encryption"))]
#[test]
fn encrypted_authoritative_images_stay_sealed_after_crash_and_wrong_key_open_writes_nothing() {
    let mut cells = 0;
    for precision in ["f32", "f16", "i8"] {
        for pq in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("db");
            let key = root.path().join("right.key");
            let wrong = root.path().join("wrong.key");
            std::fs::write(&key, "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef").unwrap();
            std::fs::write(
                &wrong,
                "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210",
            )
            .unwrap();
            let mut cfg = config(&path, true);
            cfg.encryption.enabled = true;
            cfg.encryption.key_source = heliosdb_nano::KeySource::File(key.clone());
            {
                let db = EmbeddedDatabase::with_config(cfg.clone()).unwrap();
                seed(&db, precision, pq);
            }
            let before = internal::crash(&path, true, "update", Some(&key));
            let ro = StorageEngine::open_read_only(&path, &cfg).unwrap();
            ro.catalog().rebuild_all_indexes().unwrap();
            assert_mode(&ro, precision, pq);
            assert!(ro.db().latest_sequence_number() > before);
            assert_graph(&ro, 32, 2);
            let bytes = image(&ro.db());
            let sequence = ro.db().latest_sequence_number();
            let images: Vec<_> = bytes
                .iter()
                .filter(|(key, _)| {
                    key.starts_with(b"__vidx:") && key.windows(b":row_image:".len()).any(|part| part == b":row_image:")
                })
                .collect();
            assert!(
                !images.is_empty(),
                "actual owning row group staged authoritative images"
            );
            let plaintext_vector = bincode::serialize(&vec![60000.0f32, 0.0, 0.0, 0.0]).unwrap();
            for (key, raw) in images {
                let opened = ro.get(key).unwrap().unwrap();
                assert_ne!(*raw, opened, "new authoritative values are sealed");
                assert!(
                    !raw.windows(plaintext_vector.len())
                        .any(|part| part == plaintext_vector.as_slice()),
                    "no full vector plaintext in row-image value"
                );
            }
            let mut bad = cfg.clone();
            bad.encryption.key_source = heliosdb_nano::KeySource::File(wrong);
            let error = StorageEngine::open_read_only(&path, &bad)
                .err()
                .expect("wrong key must refuse");
            assert!(
                error
                    .to_string()
                    .contains("configured encryption key does not match this database"),
                "{error}"
            );
            assert_eq!(ro.db().latest_sequence_number(), sequence);
            assert_eq!(image(&ro.db()), bytes);
            assert_eq!(ro.write_gate().refusals(), 0);
            drop(ro);
            let reopened = EmbeddedDatabase::with_config(cfg).unwrap();
            assert_mode(&reopened.storage, precision, pq);
            assert_graph(&reopened.storage, 32, 2);
            cells += 1;
        }
    }
    assert_eq!(cells, 6);
    eprintln!("vector-overlay-receipt sealed={cells}");
}

#[cfg(all(feature = "internal-tests", feature = "ha-tier1", target_os = "linux"))]
#[test]
fn physical_standby_derives_unpublished_persistent_images_without_changing_source_bytes() {
    use heliosdb_nano::replication::checkpoint_transfer::{OnlineCheckpointExport, ReceiverStaging, TransferLimits};
    use heliosdb_nano::replication::history::load_or_create_primary_history;
    use heliosdb_nano::replication::physical::{poll_source, FlushMemo, MAX_BATCH_BYTES};
    use heliosdb_nano::replication::physical_applier::{
        decode_raw_batch, DerivedStateListener, PhysicalApplier, RawOp,
    };
    use heliosdb_nano::storage::WriteGate;
    use std::sync::Arc;
    let mut cells = 0;
    for precision in ["f32", "f16", "i8"] {
        for pq in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let primary_path = root.path().join("primary");
            let cfg = config(&primary_path, true);
            let primary = EmbeddedDatabase::with_config(cfg.clone()).unwrap();
            seed(&primary, precision, pq);
            let raw = primary.storage.db();
            let history = load_or_create_primary_history(raw.path()).unwrap();
            let limits = TransferLimits::default();
            let export = OnlineCheckpointExport::create(&raw, history, root.path(), limits).unwrap();
            let mut receiver = ReceiverStaging::begin(
                &root.path().join("standby"),
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
            let stage = receiver.finish_download(&cfg).unwrap().open(&cfg).unwrap();
            let path = stage.staging_path().to_path_buf();
            let standby_cfg = config(&path, true);
            let engine = Arc::new(
                StorageEngine::open_physical_standby(
                    &path,
                    &standby_cfg,
                    Arc::clone(stage.writer()),
                    WriteGate::closed("physical standby"),
                )
                .unwrap(),
            );
            let served = Arc::new(EmbeddedDatabase::with_physical_standby(standby_cfg, Arc::clone(&engine)).unwrap());
            let listener: Arc<dyn DerivedStateListener> = served.clone();
            let applier = PhysicalApplier::new(Arc::clone(&engine), Arc::clone(stage.writer()), listener).unwrap();
            assert_mode(&engine, precision, pq);
            assert_graph(&engine, 32, 1);
            // Release every source owner before the real crash child opens it.
            drop(export);
            drop(raw);
            drop(primary);
            let before = internal::crash(&primary_path, true, "update", None);
            // Prove read-only recovery from the crashed owning row group before
            // any writable reopen is allowed to perform normal bookkeeping.
            let recovered_ro = read_only(&primary_path, true);
            assert_mode(&recovered_ro, precision, pq);
            let recovered_bytes = image(&recovered_ro.db());
            let recovered_sequence = recovered_ro.db().latest_sequence_number();
            assert!(recovered_sequence > before);
            assert_graph(&recovered_ro, 32, 2);
            assert_eq!(image(&recovered_ro.db()), recovered_bytes);
            assert_eq!(recovered_ro.db().latest_sequence_number(), recovered_sequence);
            assert_eq!(recovered_ro.write_gate().refusals(), 0);
            drop(recovered_ro);

            // A live physical source must be writable: poll_source flushes its
            // native WAL to make the advertised source barrier durable.
            let source = EmbeddedDatabase::with_config(cfg.clone()).unwrap();
            assert_mode(&source.storage, precision, pq);
            assert_graph(&source.storage, 32, 2);
            let source_raw = source.storage.db();
            assert!(source_raw.latest_sequence_number() >= recovered_sequence);
            assert_eq!(
                image(&source_raw)
                    .into_iter()
                    .filter(|(key, _)| key.starts_with(b"__vidx:"))
                    .collect::<Vec<_>>(),
                recovered_bytes
                    .into_iter()
                    .filter(|(key, _)| key.starts_with(b"__vidx:"))
                    .collect::<Vec<_>>(),
                "writable reopen derives RAM without rewriting persistent graph truth"
            );
            let source_bytes = image(&source_raw);
            let source_sequence = source_raw.latest_sequence_number();
            let flushed = FlushMemo::new();
            let mut applied = 0;
            let mut row_groups = 0;
            let mut expected = BTreeMap::new();
            loop {
                let poll = poll_source(
                    &source_raw,
                    history,
                    applier.writer().cursor().source_sequence(),
                    MAX_BATCH_BYTES,
                    &flushed,
                )
                .unwrap();
                if poll.batches.is_empty() {
                    break;
                }
                for batch in &poll.batches {
                    let records = decode_raw_batch(batch.data()).unwrap();
                    let mut rows = false;
                    let mut overlay = false;
                    for record in &records {
                        match record {
                            RawOp::Put { key, value } => {
                                rows |= key.starts_with(b"data:overlay_rows:");
                                overlay |= key.starts_with(b"__vidx:")
                                    && key.windows(b":row_image:".len()).any(|part| part == b":row_image:");
                                expected.insert(key.to_vec(), Some(value.to_vec()));
                            }
                            RawOp::Delete { key } => {
                                expected.insert(key.to_vec(), None);
                            }
                            RawOp::DeleteRange { .. } => {
                                panic!("this bounded single-row fixture must not require a range tombstone")
                            }
                        }
                    }
                    if rows {
                        row_groups += 1;
                        assert!(overlay, "row-image obligation shares actual row frame");
                    }
                    applier.apply(batch).unwrap();
                    applied += 1;
                    if rows {
                        assert_graph(&engine, 32, 2);
                    }
                }
            }
            assert!(applied > 0);
            assert_eq!(row_groups, 1);
            assert_eq!(
                image(&source_raw),
                source_bytes,
                "live polling preserves source KV bytes"
            );
            assert_eq!(source_raw.latest_sequence_number(), source_sequence);
            assert_eq!(
                applier.writer().cursor().source_sequence(),
                source_raw.latest_sequence_number()
            );
            for (key, value) in expected {
                assert_eq!(
                    applier.writer().db().get(&key).unwrap(),
                    value,
                    "applied record identity for {}",
                    String::from_utf8_lossy(&key)
                );
            }
            assert_eq!(
                image(applier.writer().db())
                    .into_iter()
                    .filter(|(key, _)| key.starts_with(b"__vidx:"))
                    .collect::<Vec<_>>(),
                image(&source_raw)
                    .into_iter()
                    .filter(|(key, _)| key.starts_with(b"__vidx:"))
                    .collect::<Vec<_>>()
            );
            let bytes = image(applier.writer().db());
            let sequence = applier.writer().db().latest_sequence_number();
            assert_graph(&engine, 32, 2);
            assert_eq!(image(applier.writer().db()), bytes);
            assert_eq!(applier.writer().db().latest_sequence_number(), sequence);
            assert_eq!(engine.write_gate().refusals(), 0);
            drop(applier);
            drop(served);
            drop(engine);
            drop(stage);
            cells += 1;
        }
    }
    assert_eq!(cells, 6);
    eprintln!("vector-overlay-receipt physical={cells}");
}

#[cfg(feature = "internal-tests")]
#[test]
#[allow(deprecated)]
fn applied_heap_commit_reports_graph_publication_error_and_keeps_committed_primary_keys() {
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    };
    const UPDATE: &str = "UPDATE overlay_rows SET id=101,embedding='[60000,0,0,0]' WHERE id=1";
    let mut cells = 0;
    for history in [false, true] {
        for pq in [false, true] {
            for owner in ["text", "params", "session", "raii"] {
                let root = tempfile::tempdir().unwrap();
                let db = open(root.path(), history);
                seed(&db, "f32", pq);
                let key = |id| heliosdb_nano::storage::ArtIndexManager::encode_key(&[Value::Int4(id)]);
                let physical = db.storage.art_indexes().pk_index_lookup(TABLE, &key(1)).unwrap();
                let mut session = None;
                let mut raii = None;
                match owner {
                    "text" => {
                        db.execute("BEGIN").unwrap();
                        db.execute(UPDATE).unwrap();
                    }
                    "params" => {
                        db.execute_params("BEGIN", &[]).unwrap();
                        db.execute_params(UPDATE, &[]).unwrap();
                    }
                    "session" => {
                        let id = db.create_wire_session("vector-publication").unwrap();
                        db.begin_transaction_for_session(id).unwrap();
                        db.execute_for_session(id, UPDATE).unwrap();
                        session = Some(id);
                    }
                    "raii" => {
                        let txn = db.begin_transaction().unwrap();
                        txn.execute(UPDATE).unwrap();
                        raii = Some(txn);
                    }
                    _ => unreachable!(),
                }
                let raw = db.storage.db();
                let hook_raw = Arc::clone(&raw);
                let gate = Arc::clone(db.storage.write_gate());
                let applied = Arc::new(AtomicU64::new(0));
                let seen = Arc::clone(&applied);
                db.storage.debug_next_after_native_transaction_write(move || {
                    seen.store(hook_raw.latest_sequence_number(), Ordering::SeqCst);
                    gate.debug_set_write_group_ceiling(Some(64));
                });
                let result = match owner {
                    "text" => db.execute("COMMIT").map(|_| ()),
                    "params" => db.execute_params("COMMIT", &[]).map(|_| ()),
                    "session" => db.commit_transaction_for_session(session.unwrap()),
                    "raii" => raii.take().unwrap().commit(),
                    _ => unreachable!(),
                };
                db.storage.debug_set_write_group_ceiling(None);
                let error = result.unwrap_err();
                let message = error.to_string();
                assert!(
                    error.is_applied_publication_failure(),
                    "known-applied error context: {error}"
                );
                assert!(message.contains("row changes were already applied"), "{message}");
                assert!(message.contains("Reopen the database before continuing"), "{message}");
                assert!(message.contains("do not retry these row changes"), "{message}");
                assert!(message.contains("Cause for the derived vector group only"), "{message}");
                assert!(
                    message.contains(heliosdb_nano::BRANCH_MATERIALIZE_TOO_LARGE_MARKER),
                    "retain original graph cap error: {error}"
                );
                assert!(
                    message.contains("nothing was written"),
                    "retain original derived cause: {message}"
                );
                let native = applied.load(Ordering::SeqCst);
                assert!(native > 0, "actual own native-success hook ran");
                assert_eq!(raw.latest_sequence_number(), native, "no error-path repair writes");
                let bytes = db
                    .storage
                    .get(&format!("data:{TABLE}:{physical}").into_bytes())
                    .unwrap()
                    .unwrap();
                let actual: Tuple = bincode::deserialize(&bytes).unwrap();
                assert_eq!(actual.values, tuple(101, vec![60000.0, 0.0, 0.0, 0.0]).values);
                assert_eq!(
                    db.storage.art_indexes().pk_index_lookup(TABLE, &key(101)),
                    Some(physical),
                    "known applied commit retains new key"
                );
                assert_eq!(
                    db.storage.art_indexes().pk_index_lookup(TABLE, &key(1)),
                    None,
                    "applied error never restores old key"
                );
                internal::assert_quarantined(&db);
                drop(raii);
                drop(raw);
                drop(db);
                let ro = read_only(root.path(), history);
                assert_graph(&ro, 32, 2);
                let before = image(&ro.db());
                assert_graph(&ro, 32, 2);
                assert_eq!(image(&ro.db()), before);
                assert_eq!(ro.write_gate().refusals(), 0);
                drop(ro);
                let reopened = open(root.path(), history);
                assert_graph(&reopened.storage, 32, 2);
                assert!(reopened
                    .query("SELECT id FROM overlay_rows WHERE id=1", &[])
                    .unwrap()
                    .is_empty());
                assert_eq!(
                    reopened
                        .query("SELECT id FROM overlay_rows WHERE id=101", &[])
                        .unwrap()
                        .len(),
                    1
                );
                cells += 1;
            }
        }
    }
    assert_eq!(cells, 16);
    eprintln!("vector-overlay-receipt publication_error={cells}");
}

#[cfg(feature = "internal-tests")]
#[test]
fn live_logical_applier_pairs_overlay_and_cursor_for_success_refusal_unknown_and_crash() {
    let mut cells = 0;
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path(), true);
    seed(&db, "f32", false);
    let row = db.storage.next_row_id(TABLE).unwrap();
    let key = format!("data:{TABLE}:{row}").into_bytes();
    for (lsn, operation, count, nearest) in [
        (
            100,
            WalOperation::Insert {
                table: TABLE.into(),
                key: key.clone(),
                tuple: bincode::serialize(&tuple(99, vec![0.0; 4])).unwrap(),
            },
            33,
            99,
        ),
        (
            101,
            WalOperation::Update {
                table: TABLE.into(),
                key: key.clone(),
                tuple: bincode::serialize(&tuple(99, vec![60000.0, 0.0, 0.0, 0.0])).unwrap(),
            },
            33,
            1,
        ),
        (
            102,
            WalOperation::Delete {
                table: TABLE.into(),
                key: key.clone(),
            },
            32,
            1,
        ),
    ] {
        let before = db.storage.db().latest_sequence_number();
        db.storage.apply_replicated_operation_at(lsn, operation).unwrap();
        assert_eq!(db.storage.replication_applied_lsn().unwrap(), lsn);
        assert_graph(&db.storage, count, nearest);
        let mut updates = db.storage.db().get_updates_since(before).unwrap();
        let mut owning = 0;
        for item in updates.by_ref() {
            let (_, batch) = item.unwrap();
            let mut group = Group::default();
            batch.iterate(&mut group);
            if group.keys.iter().any(|key| key.starts_with(b"data:overlay_rows:")) {
                owning += 1;
                assert!(group.keys.iter().any(|key| key == b"repl:applied_lsn"));
                assert!(group
                    .keys
                    .iter()
                    .any(|key| key.windows(b":row_image:".len()).any(|part| part == b":row_image:")));
                assert!(
                    group.operations.is_empty(),
                    "explicit live replay does not append local logical history"
                );
            }
        }
        updates.status().unwrap();
        assert_eq!(owning, 1);
        cells += 1;
    }
    drop(db);
    for unknown in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path(), true);
        seed(&db, "f32", false);
        let row = db
            .storage
            .art_indexes()
            .pk_index_lookup(
                TABLE,
                &heliosdb_nano::storage::ArtIndexManager::encode_key(&[Value::Int4(1)]),
            )
            .unwrap();
        if unknown {
            db.storage.write_gate().debug_fail_next_native_write_after_apply();
        } else {
            db.storage.debug_set_write_group_ceiling(Some(64));
        }
        let error = db
            .storage
            .apply_replicated_operation_at(
                100,
                WalOperation::Update {
                    table: TABLE.into(),
                    key: format!("data:{TABLE}:{row}").into_bytes(),
                    tuple: bincode::serialize(&tuple(1, vec![60000.0, 0.0, 0.0, 0.0])).unwrap(),
                },
            )
            .unwrap_err();
        db.storage.debug_set_write_group_ceiling(None);
        if unknown {
            assert!(
                error
                    .to_string()
                    .contains("injected native write error after apply; outcome is unknown"),
                "{error}"
            );
            assert_eq!(db.storage.replication_applied_lsn().unwrap(), 100);
            internal::assert_quarantined(&db);
            drop(db);
            let reopened = open(root.path(), true);
            assert_graph(&reopened.storage, 32, 2);
            assert_eq!(reopened.storage.replication_applied_lsn().unwrap(), 100);
        } else {
            assert!(
                error
                    .to_string()
                    .contains(heliosdb_nano::BRANCH_MATERIALIZE_TOO_LARGE_MARKER),
                "{error}"
            );
            assert_eq!(db.storage.replication_applied_lsn().unwrap(), 0);
            assert_graph(&db.storage, 32, 1);
        }
        cells += 1;
    }
    let root = tempfile::tempdir().unwrap();
    {
        let db = open(root.path(), true);
        seed(&db, "f32", false);
    }
    let before = internal::crash(root.path(), true, "replication", None);
    let ro = read_only(root.path(), true);
    assert!(ro.db().latest_sequence_number() > before);
    assert_eq!(ro.replication_applied_lsn().unwrap(), 100);
    assert_graph(&ro, 32, 2);
    let bytes = image(&ro.db());
    assert_graph(&ro, 32, 2);
    assert_eq!(image(&ro.db()), bytes);
    assert_eq!(ro.write_gate().refusals(), 0);
    cells += 1;
    assert_eq!(cells, 6);
    eprintln!("vector-overlay-receipt live_logical={cells}");
}

#[cfg(feature = "internal-tests")]
#[test]
fn live_logical_no_vector_unknown_fences_applied_keys_and_refusal_restores_old_keys() {
    let mut cells = 0;
    for unknown in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path(), true);
        db.execute("CREATE TABLE plain_rows (id INT PRIMARY KEY, label TEXT UNIQUE)")
            .unwrap();
        db.execute("INSERT INTO plain_rows VALUES (1,'old')").unwrap();
        let key = |id| heliosdb_nano::storage::ArtIndexManager::encode_key(&[Value::Int4(id)]);
        let row = db.storage.art_indexes().pk_index_lookup("plain_rows", &key(1)).unwrap();
        let raw_key = format!("data:plain_rows:{row}").into_bytes();
        let original = db.storage.get(&raw_key).unwrap().unwrap();
        let before = db.storage.db().latest_sequence_number();
        let new = Tuple::new(vec![Value::Int4(2), Value::String("new".into())]);
        if unknown {
            db.storage.write_gate().debug_fail_next_native_write_after_apply();
        } else {
            db.storage.debug_set_write_group_ceiling(Some(64));
        }
        let error = db
            .storage
            .apply_replicated_operation_at(
                100,
                WalOperation::Update {
                    table: "plain_rows".into(),
                    key: raw_key.clone(),
                    tuple: bincode::serialize(&new).unwrap(),
                },
            )
            .unwrap_err();
        db.storage.debug_set_write_group_ceiling(None);
        if unknown {
            assert!(
                error
                    .to_string()
                    .contains("injected native write error after apply; outcome is unknown"),
                "{error}"
            );
            assert_eq!(db.storage.replication_applied_lsn().unwrap(), 100);
            let actual: Tuple = bincode::deserialize(&db.storage.get(&raw_key).unwrap().unwrap()).unwrap();
            assert_eq!(actual.values, new.values);
            internal::assert_table_quarantined(&db, "plain_rows");
            assert!(
                db.storage.art_indexes().list_table_indexes("plain_rows").is_empty(),
                "uncertain live graph is retired before readers pass"
            );
            let mut updates = db.storage.db().get_updates_since(before).unwrap();
            let mut groups = 0;
            for item in updates.by_ref() {
                let (_, batch) = item.unwrap();
                let mut group = Group::default();
                batch.iterate(&mut group);
                assert!(group.keys.iter().any(|key| key == &raw_key));
                assert!(group.keys.iter().any(|key| key == b"repl:applied_lsn"));
                groups += 1;
            }
            updates.status().unwrap();
            assert_eq!(groups, 1, "fencing/retirement performs no extra native group");
            drop(db);
            let reopened = open(root.path(), true);
            assert!(reopened
                .query("SELECT id FROM plain_rows WHERE id=1", &[])
                .unwrap()
                .is_empty());
            assert_eq!(
                reopened
                    .query("SELECT id,label FROM plain_rows WHERE id=2", &[])
                    .unwrap()[0]
                    .values,
                new.values
            );
            assert_eq!(reopened.storage.replication_applied_lsn().unwrap(), 100);
        } else {
            assert!(
                error
                    .to_string()
                    .contains(heliosdb_nano::BRANCH_MATERIALIZE_TOO_LARGE_MARKER),
                "{error}"
            );
            assert_eq!(db.storage.replication_applied_lsn().unwrap(), 0);
            assert_eq!(db.storage.get(&raw_key).unwrap().unwrap(), original);
            assert_eq!(db.storage.db().latest_sequence_number(), before);
            assert_eq!(
                db.storage.art_indexes().pk_index_lookup("plain_rows", &key(1)),
                Some(row)
            );
            assert_eq!(db.storage.art_indexes().pk_index_lookup("plain_rows", &key(2)), None);
            assert_eq!(
                db.query("SELECT id,label FROM plain_rows", &[]).unwrap()[0].values,
                vec![Value::Int4(1), Value::String("old".into())]
            );
        }
        cells += 1;
    }
    assert_eq!(cells, 2);
    eprintln!("vector-overlay-receipt live_logical_no_vector={cells}");
}

#[cfg(feature = "internal-tests")]
#[test]
fn native_ok_standard_hook_fault_returns_unavailable_instead_of_success() {
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    };
    let root = tempfile::tempdir().unwrap();
    let db = Arc::new(open(root.path(), true));
    db.execute("CREATE TABLE overlay_rows (id INT PRIMARY KEY, embedding VECTOR(4))")
        .unwrap();
    let values = (1..=32)
        .map(|id| format!("({id},'[{},0,0,1]')", 1000 + id * 100))
        .collect::<Vec<_>>()
        .join(",");
    db.execute(&format!("INSERT INTO overlay_rows VALUES {values}"))
        .unwrap();
    db.execute("CREATE INDEX overlay_graph ON overlay_rows USING hnsw (embedding)")
        .unwrap();
    let key = |id| heliosdb_nano::storage::ArtIndexManager::encode_key(&[Value::Int4(id)]);
    let physical = db.storage.art_indexes().pk_index_lookup(TABLE, &key(1)).unwrap();
    let schema = db.storage.catalog().get_table_schema(TABLE).unwrap();
    db.execute("BEGIN").unwrap();
    db.execute("UPDATE overlay_rows SET id=101,embedding='[60000,0,0,0]' WHERE id=1")
        .unwrap();
    let applied = Arc::new(AtomicU64::new(0));
    let seen = Arc::clone(&applied);
    let hook_db = Arc::clone(&db);
    let raw = db.storage.db();
    let hook_raw = Arc::clone(&raw);
    db.storage.debug_next_after_native_transaction_write(move || {
        seen.store(hook_raw.latest_sequence_number(), Ordering::SeqCst);
        let old = tuple(101, vec![60000.0, 0.0, 0.0, 0.0]);
        let bad = tuple(101, vec![1.0, 2.0, 3.0]);
        let _undo = hook_db
            .storage
            .vector_indexes()
            .on_row_update(TABLE, physical, &schema, Some(&old), &bad);
        assert_eq!(
            hook_db.storage.vector_indexes().index_live_size(INDEX),
            Some(31),
            "real partial hook removed old member before dimension failure"
        );
    });
    let error = db.execute("COMMIT").unwrap_err();
    assert!(
        error.is_applied_publication_failure(),
        "known-applied hook failure: {error}"
    );
    let message = error.to_string();
    assert!(message.contains("row changes were already applied"), "{message}");
    assert!(message.contains("do not retry these row changes"), "{message}");
    assert!(message.contains("Reopen the database before continuing"), "{message}");
    assert!(message.contains("Cause for the derived vector group only"), "{message}");
    assert!(
        message.contains("writes to table 'overlay_rows' are unavailable after index reconciliation failed (vector graph hook failed); reopen to rebuild its indexes"),
        "retain exact post-publication availability cause: {message}"
    );
    assert!(applied.load(Ordering::SeqCst) > 0);
    assert_eq!(raw.latest_sequence_number(), applied.load(Ordering::SeqCst));
    assert_eq!(
        db.storage.art_indexes().pk_index_lookup(TABLE, &key(101)),
        Some(physical)
    );
    assert_eq!(db.storage.art_indexes().pk_index_lookup(TABLE, &key(1)), None);
    internal::assert_quarantined(&db);
    drop(raw);
    drop(db);
    let reopened = open(root.path(), true);
    assert!(matches!(
        reopened
            .storage
            .vector_indexes()
            .get_metadata(INDEX)
            .unwrap()
            .index_type,
        VectorIndexType::Standard(_)
    ));
    assert_eq!(reopened.storage.vector_indexes().index_live_size(INDEX), Some(32));
    assert_eq!(
        reopened
            .query(
                "SELECT id FROM overlay_rows ORDER BY embedding <-> '[0,0,0,0]' LIMIT 1",
                &[]
            )
            .unwrap()[0]
            .values,
        vec![Value::Int4(2)]
    );
    assert_eq!(
        reopened
            .query("SELECT id FROM overlay_rows WHERE id=101", &[])
            .unwrap()
            .len(),
        1
    );
    eprintln!("vector-overlay-receipt standard_hook_fault=1");
}
