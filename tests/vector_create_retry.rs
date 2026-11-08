//! Explicit retry may reclaim unreferenced scratch, including a missing meta key.
#![cfg(all(feature = "internal-tests", feature = "vector-persist"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::sql::executor::ddl::debug_next_before_vector_backfill;
use heliosdb_nano::{Config, EmbeddedDatabase, Error, Value};
use rocksdb::{IteratorMode, ReadOptions};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

fn open(path: &Path) -> EmbeddedDatabase {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    config.storage.wal_enabled = true;
    config.storage.time_travel_enabled = false;
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    config.storage.version_gc_interval_secs = Some(0);
    let db = EmbeddedDatabase::with_config(config).unwrap();
    db.storage.set_index_snapshots_on_close(false);
    db.storage.set_row_counter_flush_on_close(false);
    db
}

fn execute(db: &EmbeddedDatabase, params: bool, sql: &str) -> heliosdb_nano::Result<()> {
    if params {
        db.execute_params(sql, &[]).map(|_| ())
    } else {
        db.execute(sql).map(|_| ())
    }
}

fn image(db: &EmbeddedDatabase) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut options = ReadOptions::default();
    options.set_total_order_seek(true);
    db.storage
        .db()
        .iterator_opt(IteratorMode::Start, options)
        .map(|entry| {
            let (key, value) = entry.unwrap();
            (key.to_vec(), value.to_vec())
        })
        .collect()
}

fn seed(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE items (id INT PRIMARY KEY, embedding VECTOR(3))")
        .unwrap();
    let rows = (1..=32)
        .map(|id| format!("({id}, '[{id},0,0]')"))
        .collect::<Vec<_>>()
        .join(",");
    db.execute(&format!("INSERT INTO items VALUES {rows}")).unwrap();
    db.execute("CREATE TABLE healthy (id INT PRIMARY KEY, embedding VECTOR(3))")
        .unwrap();
    db.execute("INSERT INTO healthy VALUES (7, '[7,0,0]')").unwrap();
    db.execute("CREATE INDEX healthy_graph ON healthy USING hnsw (embedding)")
        .unwrap();
}

fn create_orphan(db: &EmbeddedDatabase, params: bool) -> Vec<u8> {
    let prefix = Arc::new(Mutex::new(None));
    let saved = prefix.clone();
    let raw = db.storage.db();
    let gate = db.storage.write_gate().clone();
    debug_next_before_vector_backfill(move |manager, name, vectors| {
        assert_eq!(name, "scratch");
        let partial: Vec<_> = vectors
            .iter()
            .filter(|(_, vector)| vector[0] <= 16.0)
            .cloned()
            .collect();
        assert_eq!(partial.len(), 16);
        manager.insert_vectors_batch(name, &partial).unwrap();
        let mut options = ReadOptions::default();
        options.set_total_order_seek(true);
        let meta = raw
            .iterator_opt(IteratorMode::Start, options)
            .map(|entry| entry.unwrap().0.to_vec())
            .find(|key| key.starts_with(b"__vidx:") && key.ends_with(b":meta"))
            .unwrap();
        *saved.lock().unwrap() = Some(meta[..meta.len() - b"meta".len()].to_vec());
        gate.debug_set_write_group_ceiling(Some(0));
    });
    let error = execute(
        db,
        params,
        "CREATE INDEX scratch ON items USING hnsw (embedding) WITH (persistent=true)",
    )
    .unwrap_err()
    .to_string();
    db.storage.debug_set_write_group_ceiling(None);
    assert!(
        error.contains("Failed to backfill HNSW index") && error.contains("write group"),
        "{error}"
    );
    assert!(!db.storage.vector_indexes().index_exists("scratch"));
    assert!(db.storage.get(&b"meta:index:scratch".to_vec()).unwrap().is_none());
    let result = prefix
        .lock()
        .unwrap()
        .take()
        .expect("actual partial graph was registered");
    result
}

#[test]
fn retry_purges_all_orphan_keys_when_the_metadata_key_is_already_missing() {
    let mut cells = 0;
    for params in [false, true] {
        let path = tempfile::tempdir().unwrap();
        let db = open(path.path());
        seed(&db);
        let prefix = create_orphan(&db, params);
        let raw = db.storage.db();
        let mut meta = prefix.clone();
        meta.extend_from_slice(b"meta");
        let mut leftover = prefix.clone();
        leftover.extend_from_slice(b"unreachable_scratch_sentinel");
        // Explicit fixture for a previous retry interrupted after its metadata
        // chunk: this is not counted as production error-path repair evidence.
        raw.delete(&meta).unwrap();
        raw.put(&leftover, b"left after interrupted scratch cleanup").unwrap();
        assert!(raw.get(&meta).unwrap().is_none());
        assert!(image(&db).keys().any(|key| key.starts_with(&prefix)));
        drop(raw);
        drop(db);
        let reopened = open(path.path());
        assert!(reopened.storage.db().get(&meta).unwrap().is_none());
        assert!(!reopened.storage.vector_indexes().index_exists("scratch"));
        let before = image(&reopened);
        execute(
            &reopened,
            params,
            "CREATE INDEX scratch ON items USING hnsw (embedding) WITH (persistent=true)",
        )
        .unwrap();
        assert!(
            reopened.storage.db().get(&leftover).unwrap().is_none(),
            "entire prefix is purged, not only metadata"
        );
        assert!(reopened.storage.vector_indexes().index_exists("scratch"));
        assert_eq!(
            reopened
                .query("SELECT id FROM items ORDER BY embedding <-> '[32,0,0]' LIMIT 1", &[])
                .unwrap()[0]
                .values,
            vec![Value::Int4(32)]
        );
        for (key, value) in before.iter().filter(|(key, _)| key.starts_with(b"data:")) {
            assert_eq!(image(&reopened).get(key), Some(value));
        }
        // A now-referenced graph must never be reclaimed by a repeated CREATE.
        let before_duplicate = image(&reopened);
        let sequence = reopened.storage.db().latest_sequence_number();
        assert!(execute(
            &reopened,
            params,
            "CREATE INDEX scratch ON items USING hnsw (embedding) WITH (persistent=true)"
        )
        .is_err());
        assert_eq!(image(&reopened), before_duplicate);
        assert_eq!(reopened.storage.db().latest_sequence_number(), sequence);
        drop(reopened);
        let again = open(path.path());
        assert!(again.storage.vector_indexes().index_exists("scratch"));
        assert_eq!(
            again
                .query("SELECT id FROM items ORDER BY embedding <-> '[32,0,0]' LIMIT 1", &[])
                .unwrap()[0]
                .values,
            vec![Value::Int4(32)]
        );
        assert!(again.storage.vector_indexes().index_exists("healthy_graph"));
        cells += 1;
    }
    assert_eq!(cells, 2);
    eprintln!("vector-create-retry missing_meta=2");
}

#[test]
fn unreadable_reference_proof_refuses_explicit_retry_without_repair_writes() {
    let mut cells = 0;
    for params in [false, true] {
        let path = tempfile::tempdir().unwrap();
        let db = open(path.path());
        seed(&db);
        let _prefix = create_orphan(&db, params);
        db.storage
            .db()
            .put(b"meta:index:unreadable", b"cannot prove namespace references")
            .unwrap();
        let before = image(&db);
        let sequence = db.storage.db().latest_sequence_number();
        let error = execute(
            &db,
            params,
            "CREATE INDEX scratch ON items USING hnsw (embedding) WITH (persistent=true)",
        )
        .unwrap_err();
        assert!(
            matches!(&error, Error::Storage(message) if message.contains("unavailable") && message.contains("reference")),
            "{error}"
        );
        assert_eq!(image(&db), before);
        assert_eq!(db.storage.db().latest_sequence_number(), sequence);
        let error = db.query("SELECT id FROM items", &[]).unwrap_err();
        assert!(
            matches!(&error, Error::Storage(message) if message.contains("unavailable")),
            "{error}"
        );
        assert_eq!(image(&db), before);
        assert_eq!(db.storage.db().latest_sequence_number(), sequence);
        assert!(db.storage.vector_indexes().index_exists("healthy_graph"));
        assert_eq!(
            db.query("SELECT id FROM healthy", &[]).unwrap()[0].values,
            vec![Value::Int4(7)]
        );
        cells += 1;
    }
    assert_eq!(cells, 2);
    eprintln!("vector-create-retry unreadable_reference=2");
}
