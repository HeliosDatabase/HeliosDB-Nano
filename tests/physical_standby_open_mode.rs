//! Spec P0.2 behaviour: a database opened without write access writes nothing,
//! and a physical standby's directory opens only as a standby.
//!
//! The assertions that matter are on `storage::WriteGate`'s refusal counter,
//! not only on RocksDB's sequence number: a read-only RocksDB handle already
//! rejects writes, so an unchanged sequence alone cannot tell "nothing tried to
//! write" from "something tried and RocksDB said no". Before the gate existed,
//! the read-only and physical-snapshot opens still ATTEMPTED counter reseeds,
//! snapshot registrations, hook deletes and close-time flushes. A clean read
//! path must leave the counter at zero; the one statement here that needs a
//! write (`nextval`) must be refused exactly once, by the gate.
//!
//! `open_physical_snapshot` stands in for the physical standby open (P0.3 wires
//! the standby's own writer); both run the same physical hydration behind a
//! closed gate.
use heliosdb_nano::storage::keyspace::LOCAL_CF;
use heliosdb_nano::storage::StorageEngine;
use heliosdb_nano::{Config, EmbeddedDatabase, Value};
use rocksdb::{Options, DB};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn config(path: &Path) -> Config {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    config
}

/// A closed physical checkpoint of a primary holding a table, a sequence whose
/// high-water is already durable, and committed rows.
fn populated_checkpoint() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("primary");
    let checkpoint = root.path().join("checkpoint");
    let db = EmbeddedDatabase::with_config(config(&source)).unwrap();
    db.execute("CREATE TABLE standby_rows (id INT PRIMARY KEY, tag TEXT)")
        .unwrap();
    db.execute("INSERT INTO standby_rows VALUES (1, 'one'), (2, 'two'), (3, 'three')")
        .unwrap();
    db.execute("CREATE SEQUENCE standby_seq START 10").unwrap();
    db.query("SELECT nextval('standby_seq')", &[]).unwrap();
    let raw = db.storage.db();
    rocksdb::checkpoint::Checkpoint::new(&raw)
        .unwrap()
        .create_checkpoint(&checkpoint)
        .unwrap();
    drop(raw);
    drop(db);
    (root, checkpoint)
}

/// The store's sequence number, read through an independent read-only handle.
fn stored_sequence(path: &Path) -> u64 {
    DB::open_for_read_only(&Options::default(), path, false)
        .unwrap()
        .latest_sequence_number()
}

#[test]
fn a_physical_open_reads_commits_and_closes_without_a_single_write_attempt() {
    let (_root, path) = populated_checkpoint();
    let before = stored_sequence(&path);

    let hydrated = EmbeddedDatabase::open_physical_snapshot(config(&path)).unwrap();
    let db = &hydrated.database;
    let gate = Arc::clone(db.storage.write_gate());
    let sequence = || db.storage.db().latest_sequence_number();
    assert!(!gate.is_open(), "a physical open must close its write gate");
    assert_eq!(sequence(), before, "the open moved the sequence");
    assert_eq!(gate.refusals(), 0, "the open attempted a write");

    let rows = db.query("SELECT count(*) FROM standby_rows", &[]).unwrap();
    let count = match rows.first().and_then(|row| row.values.first()) {
        Some(Value::Int8(n)) => *n,
        Some(Value::Int4(n)) => i64::from(*n),
        other => panic!("unexpected count {other:?}"),
    };
    assert_eq!(count, 3);
    assert_eq!(sequence(), before, "SELECT count(*) moved the sequence");
    assert_eq!(gate.refusals(), 0, "SELECT count(*) attempted a write");

    db.execute("BEGIN").unwrap();
    db.query("SELECT 1", &[]).unwrap();
    db.execute("COMMIT").unwrap();
    assert_eq!(sequence(), before, "a read-only transaction moved the sequence");
    assert_eq!(gate.refusals(), 0, "a read-only COMMIT attempted a write");

    let error = match db.query("SELECT nextval('standby_seq')", &[]) {
        Ok(rows) => panic!("nextval served {} rows from a database that may not write", rows.len()),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("write refused on a physical snapshot"), "{error}");
    assert_eq!(sequence(), before, "nextval moved the sequence");
    assert_eq!(gate.refusals(), 1, "nextval must be refused exactly once, by the gate");

    drop(hydrated);
    assert_eq!(stored_sequence(&path), before, "close wrote to the store");
    assert_eq!(gate.refusals(), 1, "close attempted a write");
}

#[test]
fn a_physical_standby_directory_opens_only_as_a_standby() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("standby");
    let mut cfg = Config::default();
    cfg.storage.wal_enabled = false;
    cfg.storage.elide_latest_version = false;
    // A store every mode could open, until it carries a standby's node-local
    // column family (P0.3's writer creates it; plain RocksDB stands in here).
    drop(StorageEngine::open(&path, &cfg).unwrap());
    let mut raw = DB::open(&Options::default(), &path).unwrap();
    raw.create_cf(LOCAL_CF, &Options::default()).unwrap();
    drop(raw);

    let sequence = || {
        DB::open_cf_for_read_only(&Options::default(), &path, ["default", LOCAL_CF], false)
            .unwrap()
            .latest_sequence_number()
    };
    let files = |dir: &Path| {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    // Read the sequence first: this plain read-only open rotates RocksDB's own
    // LOG, which the file census below must not mistake for the refused opens.
    let before_sequence = sequence();
    let before = files(&path);

    for (mode, refused) in [
        ("open", StorageEngine::open(&path, &cfg).err()),
        ("open_read_only", StorageEngine::open_read_only(&path, &cfg).err()),
        (
            "open_physical_snapshot",
            StorageEngine::open_physical_snapshot(&path, &cfg).err(),
        ),
    ] {
        let error = refused
            .unwrap_or_else(|| panic!("{mode} opened a physical standby's directory"))
            .to_string();
        assert!(
            error.contains(LOCAL_CF) && error.contains("physical standby start path"),
            "{mode} must refuse by naming the column family and the standby start path: {error}"
        );
    }

    // Refused before any RocksDB handle existed: no LOG/OPTIONS/MANIFEST churn,
    // no sequence movement.
    assert_eq!(files(&path), before, "a refused open touched the directory");
    assert_eq!(sequence(), before_sequence);
}
