//! Source operation boundary regressions. Does not certify complete daemon coverage.
#![cfg(all(feature = "ha-tier1", target_os = "linux"))]
use heliosdb_nano::{replication::history::load_or_create_primary_history, Config, EmbeddedDatabase, Tuple, Value};
use std::{
    sync::{mpsc, Arc},
    thread,
    time::Duration,
};

fn fixture() -> (tempfile::TempDir, Arc<EmbeddedDatabase>, uuid::Uuid) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source");
    let mut config = Config::default();
    config.storage.path = Some(path.clone());
    config.storage.memory_only = false;
    config.storage.version_gc_interval_secs = Some(0);
    config.replication.physical.enabled = true;
    let db = Arc::new(EmbeddedDatabase::with_config(config).unwrap());
    let history = load_or_create_primary_history(&path).unwrap();
    db.execute("CREATE TABLE admission_rows (id INT PRIMARY KEY, label TEXT)")
        .unwrap();
    (root, db, history)
}

#[test]
fn ordinary_duplicate_key_error_does_not_disable_source_certification() {
    let (_root, db, history) = fixture();
    let coordinator = db.storage.physical_source_coordinator().unwrap();
    let schema = db.storage.catalog().get_table_schema("admission_rows").unwrap();
    let tuple = Tuple::new(vec![Value::Int4(1), Value::String("first".into())]);
    db.storage
        .insert_tuple_fast("admission_rows", tuple.clone(), &schema)
        .unwrap();
    let before = coordinator.capture_barrier(history, Duration::from_secs(2)).unwrap();
    assert!(db.storage.insert_tuple_fast("admission_rows", tuple, &schema).is_err());
    let after = coordinator.capture_barrier(history, Duration::from_secs(2)).unwrap();
    assert_eq!(before.source_sequence(), after.source_sequence());
    assert_eq!(db.query("SELECT id, label FROM admission_rows", &[]).unwrap().len(), 1);
}

#[test]
fn direct_fast_storage_writer_cannot_cross_source_capture_fence() {
    let (_root, db, history) = fixture();
    let coordinator = db.storage.physical_source_coordinator().unwrap();
    let schema = db.storage.catalog().get_table_schema("admission_rows").unwrap();
    let fence = coordinator.fence(Duration::from_secs(2)).unwrap();
    let before = fence.capture_barrier(history).unwrap().source_sequence();
    let writer = db.clone();
    let (started_tx, started_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let _ = started_tx.send(());
        let result = writer.storage.insert_tuple_fast(
            "admission_rows",
            Tuple::new(vec![Value::Int4(7), Value::String("after-fence".into())]),
            &schema,
        );
        let _ = done_tx.send(result);
    });
    let started = started_rx.recv_timeout(Duration::from_secs(2));
    let early = done_rx.recv_timeout(Duration::from_millis(30));
    let during = db.storage.db().latest_sequence_number();
    drop(fence);
    let completed = done_rx.recv_timeout(Duration::from_secs(3));
    let joined = worker.join();
    assert!(started.is_ok() && early.is_err());
    assert_eq!(before, during);
    completed.unwrap().unwrap();
    joined.unwrap();
    assert!(
        coordinator
            .capture_barrier(history, Duration::from_secs(2))
            .unwrap()
            .source_sequence()
            > before
    );
    assert_eq!(
        db.query("SELECT id FROM admission_rows", &[]).unwrap()[0].values,
        vec![Value::Int4(7)]
    );
}
