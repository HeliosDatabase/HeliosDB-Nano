//! A valid Standard graph dump must become invalid in its owning heap group.
#![cfg(feature = "vector-search")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
use heliosdb_nano::storage::{index_snapshot, StorageEngine, VectorIndexType};
use heliosdb_nano::{Config, EmbeddedDatabase, Value};
#[cfg(feature = "internal-tests")]
use rocksdb::DB;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const TABLE: &str = "standard_rows";
const INDEX: &str = "standard_graph";
fn config(path: &Path, history: bool) -> Config {
    let mut cfg = Config::default();
    cfg.storage.path = Some(path.to_path_buf());
    cfg.storage.memory_only = false;
    cfg.storage.wal_enabled = true;
    cfg.storage.logical_wal_per_statement = false;
    cfg.storage.time_travel_enabled = history;
    cfg.storage.wal_checkpoint_interval_entries = 0;
    cfg.storage.wal_checkpoint_interval_secs = 0;
    cfg.storage.wal_checkpoint_on_close = false;
    cfg.storage.flush_on_close = false;
    cfg.storage.version_gc_interval_secs = Some(0);
    cfg.replication.physical.enabled = true;
    cfg
}
fn open(path: &Path, history: bool) -> EmbeddedDatabase {
    let db = EmbeddedDatabase::with_config(config(path, history)).unwrap();
    db.storage.set_index_snapshots_on_close(false);
    db.storage.set_row_counter_flush_on_close(false);
    db
}
#[cfg(feature = "internal-tests")]
fn image(db: &DB) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut opts = rocksdb::ReadOptions::default();
    opts.set_total_order_seek(true);
    db.iterator_opt(rocksdb::IteratorMode::Start, opts)
        .map(|entry| {
            let (key, value) = entry.unwrap();
            (key.to_vec(), value.to_vec())
        })
        .collect()
}
fn seed(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE standard_rows (id INT PRIMARY KEY, embedding VECTOR(2))")
        .unwrap();
    let values = (1..=32)
        .map(|id| format!("({id},'[{},0]')", id * 100))
        .collect::<Vec<_>>()
        .join(",");
    db.execute(&format!("INSERT INTO standard_rows VALUES {values}"))
        .unwrap();
    db.execute("CREATE INDEX standard_graph ON standard_rows USING hnsw (embedding)")
        .unwrap();
    assert_graph(&db.storage, 32, 1);
}
fn assert_graph(engine: &StorageEngine, count: usize, nearest: i32) {
    assert!(matches!(
        engine.vector_indexes().get_metadata(INDEX).unwrap().index_type,
        VectorIndexType::Standard(_)
    ));
    let schema = engine.catalog().get_table_schema(TABLE).unwrap();
    let rows: BTreeMap<_, _> = engine
        .scan_table_with_schema(TABLE, &schema)
        .unwrap()
        .into_iter()
        .map(|row| {
            let Value::Int4(id) = row.values[0] else {
                panic!("unexpected row{row:?}")
            };
            (row.row_id.unwrap(), id)
        })
        .collect();
    assert_eq!(rows.len(), count);
    assert_eq!(engine.vector_indexes().index_live_size(INDEX), Some(count));
    let found = engine
        .vector_indexes()
        .search(INDEX, &vec![0.0, 0.0], count + 1)
        .unwrap();
    assert_eq!(found.len(), count);
    assert_eq!(
        found.iter().map(|(row, _)| *row).collect::<BTreeSet<_>>(),
        rows.keys().copied().collect()
    );
    assert_eq!(rows[&found[0].0], nearest);
}
fn checkpoint(db: &EmbeddedDatabase) {
    let report = db.storage.persist_index_snapshots().unwrap();
    assert_eq!(report.vector_dump_failures, 0, "{report:?}");
    assert_eq!(
        report.vector_graphs + report.vector_unchanged,
        1,
        "one real graph dumped or valid previous dump retained: {report:?}"
    );
    let marker = db
        .storage
        .db()
        .get(index_snapshot::vec_snapshot_marker_key(INDEX))
        .unwrap()
        .unwrap();
    let marker: index_snapshot::SnapshotMarker = bincode::deserialize(&marker).unwrap();
    assert!(marker.matches_current_format(), "real stored graph marker is valid");
    assert_eq!(marker.key_encoding_version, index_snapshot::ART_KEY_ENCODING_VERSION);
    assert!(marker.checkpointed_at_secs > 0);
    // The diagnostic wall-clock second can advance after the checkpoint;
    // validity is proved by the stored format and actual reload below.

    assert!(db
        .storage
        .db()
        .get(index_snapshot::vec_snapshot_key(INDEX))
        .unwrap()
        .is_some());
    assert!(db.storage.vector_indexes().snapshot_is_clean());
}
#[test]
fn a_checkpointed_standard_graph_is_really_reloadable_without_persistent_feature() {
    let mut cells = 0;
    for history in [false, true] {
        let root = tempfile::tempdir().unwrap();
        {
            let db = open(root.path(), history);
            seed(&db);
            checkpoint(&db);
        }
        let reopened = open(root.path(), history);
        let report = reopened.storage.last_index_open_report().unwrap();
        assert_eq!(report.vector_reloaded, 1, "{report:?}");
        assert_eq!(report.vector_rebuilt, 0, "{report:?}");
        assert_graph(&reopened.storage, 32, 1);
        cells += 1;
    }
    assert_eq!(cells, 2);
    eprintln!("standard-row-receipt checkpoint={cells}");
}

#[cfg(feature = "internal-tests")]
mod internal {
    use super::*;
    use heliosdb_nano::Tuple;
    use rocksdb::WriteBatchIterator;
    const CHILD: &str = "NANO_STANDARD_ROW_CRASH_PATH";
    const ACTION: &str = "NANO_STANDARD_ROW_CRASH_ACTION";
    const HISTORY: &str = "NANO_STANDARD_ROW_CRASH_HISTORY";
    const NAME: &str =
        "internal::valid_standard_dump_marker_is_deleted_in_heap_group_before_insert_update_delete_hooks";
    #[derive(Default)]
    struct Group {
        records: Vec<(bool, Vec<u8>)>,
    }
    impl WriteBatchIterator for Group {
        fn put(&mut self, key: Box<[u8]>, _: Box<[u8]>) {
            self.records.push((false, key.into_vec()));
        }
        fn delete(&mut self, key: Box<[u8]>) {
            self.records.push((true, key.into_vec()));
        }
    }
    #[test]
    fn valid_standard_dump_marker_is_deleted_in_heap_group_before_insert_update_delete_hooks() {
        if let Ok(path) = std::env::var(CHILD) {
            let history = std::env::var(HISTORY).unwrap() == "1";
            let db = open(Path::new(&path), history);
            let loaded = db.storage.last_index_open_report().unwrap();
            assert_eq!(loaded.vector_reloaded, 1, "real old dump loaded: {loaded:?}");
            // Opening consumes validity. Re-arm the real, already-loaded dump
            // immediately before the owning native write, while RAM is clean.
            checkpoint(&db);
            let action = std::env::var(ACTION).unwrap();
            let schema = db.storage.catalog().get_table_schema(TABLE).unwrap();
            let pk = heliosdb_nano::storage::ArtIndexManager::encode_key(&[Value::Int4(1)]);
            let row = db.storage.art_indexes().pk_index_lookup(TABLE, &pk).unwrap();
            let _row_guard = if action != "insert" {
                Some(db.storage.lock_row_for_fast_write(TABLE, row).unwrap())
            } else {
                None
            };
            let old = db.storage.get_row_by_id(TABLE, row, &schema).unwrap().unwrap();
            let ready = serde_json::json!({"action":action,"sequence":db.storage.db().latest_sequence_number(),"dump_reloaded":loaded.vector_reloaded});
            std::fs::write(
                Path::new(&path).join("standard-row-child-ready"),
                serde_json::to_vec(&ready).unwrap(),
            )
            .unwrap();
            match action.as_str() {
                "insert" => {
                    db.storage
                        .insert_tuple_versioned_with_schema(
                            TABLE,
                            Tuple::new(vec![Value::Int4(99), Value::Vector(vec![0.0, 0.0])]),
                            &schema,
                        )
                        .unwrap();
                }
                "update" => {
                    db.storage
                        .update_tuple_fast(
                            TABLE,
                            row,
                            Tuple::new(vec![Value::Int4(1), Value::Vector(vec![60000.0, 0.0])]),
                            &old,
                            &schema,
                        )
                        .unwrap();
                }
                "delete" => {
                    db.storage.delete_tuple_fast(TABLE, row, &old, &schema).unwrap();
                }
                _ => panic!("unknown action {action}"),
            }
            panic!("actual native heap-before-graph hook did not fire");
        }
        let mut cells = 0;
        let mut actions = BTreeSet::new();
        for history in [false, true] {
            for action in ["insert", "update", "delete"] {
                let root = tempfile::tempdir().unwrap();
                {
                    let db = open(root.path(), history);
                    seed(&db);
                    checkpoint(&db);
                }
                let tag = match action {
                    "insert" => "row_insert",
                    "update" => "row_update",
                    _ => "row_delete",
                };
                let status = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", NAME, "--nocapture"])
                    .env(CHILD, root.path())
                    .env(ACTION, action)
                    .env(HISTORY, if history { "1" } else { "0" })
                    .env("HELIOSDB_NANO_TEST_EXIT_AFTER_DDL_WRITE", format!("{tag}:{TABLE}"))
                    .status()
                    .unwrap();
                assert_eq!(status.code(), Some(0), "native {action} child: {status}");
                let ready: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(root.path().join("standard-row-child-ready")).unwrap())
                        .unwrap();
                assert_eq!(ready["action"], action);
                assert_eq!(ready["dump_reloaded"], 1);
                let sequence = ready["sequence"].as_u64().unwrap();
                let ro = StorageEngine::open_read_only(root.path(), &config(root.path(), history)).unwrap();
                let raw = ro.db();
                assert!(raw.latest_sequence_number() > sequence);
                let marker = index_snapshot::vec_snapshot_marker_key(INDEX);
                assert!(
                    raw.get(&marker).unwrap().is_none(),
                    "stale dump must be ineligible at crash"
                );
                assert!(
                    raw.get(index_snapshot::vec_snapshot_key(INDEX)).unwrap().is_some(),
                    "old blob still exists, so missing marker is decisive"
                );
                let mut updates = raw.get_updates_since(sequence).unwrap();
                let mut owning = 0;
                let mut marker_groups = 0;
                for entry in updates.by_ref() {
                    let (_, batch) = entry.unwrap();
                    let mut group = Group::default();
                    batch.iterate(&mut group);
                    let rows = group
                        .records
                        .iter()
                        .any(|(_, key)| key.starts_with(b"data:standard_rows:"));
                    let invalidates = group.records.iter().any(|(deleted, key)| *deleted && key == &marker);
                    if rows {
                        owning += 1;
                        assert!(invalidates, "the REAL row group invalidates dump, no later hook needed");
                    }
                    if invalidates {
                        marker_groups += 1;
                        assert!(rows, "no premature/late marker-only invalidation group");
                    }
                }
                updates.status().unwrap();
                assert_eq!(owning, 1);
                assert_eq!(marker_groups, 1);
                drop(updates);
                let before = image(&raw);
                let before_sequence = raw.latest_sequence_number();
                ro.catalog().rebuild_all_indexes().unwrap();
                let (count, nearest) = match action {
                    "insert" => (33, 99),
                    "delete" => (31, 2),
                    _ => (32, 2),
                };
                assert_graph(&ro, count, nearest);
                assert_eq!(ro.write_gate().refusals(), 0);
                assert_eq!(raw.latest_sequence_number(), before_sequence);
                assert_eq!(image(&raw), before);
                drop(raw);
                drop(ro);
                let reopened = open(root.path(), history);
                let report = reopened.storage.last_index_open_report().unwrap();
                assert_eq!(
                    report.vector_reloaded, 0,
                    "old dumped graph must never be loaded: {report:?}"
                );
                assert_eq!(report.vector_rebuilt, 1, "actual heap reconstructs graph: {report:?}");
                assert_graph(&reopened.storage, count, nearest);
                cells += 1;
                actions.insert(action);
            }
        }
        assert_eq!(cells, 6);
        assert_eq!(actions.len(), 3);
        eprintln!("standard-row-receipt native_crash={cells}");
    }
}
