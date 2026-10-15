//! A clean close flushes RocksDB's memtables (`storage.flush_on_close`), so the
//! next open has no RocksDB WAL to replay.
//!
//! Opening an existing store reads it twice: the read-only encryption preflight,
//! then the writable handle. Each replays whatever is still only in the RocksDB
//! WAL. Measured on the 4.41.1 candidate before this setting existed, a store
//! whose rows were all still in the WAL started ~22-25 ms slower than on 4.41.0
//! (2 MB of WAL, tmpfs), because the preflight replayed it a second time.

use heliosdb_nano::{Config, EmbeddedDatabase};
use std::path::Path;

const ROWS: usize = 2000;

fn config(dir: &Path, flush_on_close: bool) -> Config {
    let mut config = Config::default();
    config.storage.memory_only = false;
    config.storage.path = Some(dir.to_path_buf());
    config.storage.flush_on_close = flush_on_close;
    config
}

/// Total size of the files directly under `dir` with extension `ext`.
fn bytes_with_extension(dir: &Path, ext: &str) -> u64 {
    std::fs::read_dir(dir)
        .expect("read data dir")
        .map(|entry| entry.expect("dir entry"))
        .filter(|entry| entry.path().extension().is_some_and(|e| e == ext))
        .map(|entry| entry.metadata().expect("metadata").len())
        .sum()
}

fn write_and_close(dir: &Path, flush_on_close: bool) {
    let db = EmbeddedDatabase::with_config(config(dir, flush_on_close)).expect("open");
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, payload TEXT)")
        .expect("create");
    for chunk in (0..ROWS).collect::<Vec<_>>().chunks(250) {
        let values: Vec<String> = chunk.iter().map(|i| format!("({i}, '{}')", "x".repeat(100))).collect();
        db.execute(&format!("INSERT INTO t VALUES {}", values.join(", ")))
            .expect("insert");
    }
    drop(db);
}

fn reopen_count(dir: &Path, flush_on_close: bool) -> i64 {
    let db = EmbeddedDatabase::with_config(config(dir, flush_on_close)).expect("reopen");
    let rows = db.query("SELECT count(*) FROM t", &[]).expect("count");
    let count = rows
        .first()
        .and_then(|row| row.values.first())
        .cloned()
        .expect("one value");
    match count {
        heliosdb_nano::Value::Int8(n) => n,
        heliosdb_nano::Value::Int4(n) => i64::from(n),
        other => panic!("unexpected count value {other:?}"),
    }
}

#[test]
fn clean_close_leaves_no_wal_to_replay() {
    let on = tempfile::tempdir().expect("tempdir");
    let off = tempfile::tempdir().expect("tempdir");
    write_and_close(on.path(), true);
    write_and_close(off.path(), false);

    let (wal_on, sst_on) = (
        bytes_with_extension(on.path(), "log"),
        bytes_with_extension(on.path(), "sst"),
    );
    let (wal_off, sst_off) = (
        bytes_with_extension(off.path(), "log"),
        bytes_with_extension(off.path(), "sst"),
    );

    // Control: without the flush the rows are still only in the WAL, otherwise
    // the comparison below proves nothing.
    assert!(
        wal_off > (ROWS * 100) as u64,
        "control: with flush_on_close = false the rows must still be in the RocksDB WAL \
         (wal {wal_off} bytes, sst {sst_off} bytes)"
    );
    assert!(sst_on > 0, "the clean close must have written the rows to a table file");
    assert!(
        wal_on * 10 < wal_off,
        "after a clean close the RocksDB WAL must be (almost) empty: {wal_on} bytes with the flush, \
         {wal_off} without"
    );

    assert_eq!(reopen_count(on.path(), true), ROWS as i64);
    assert_eq!(reopen_count(off.path(), false), ROWS as i64);
}

#[test]
fn read_only_open_is_not_flushed_or_mutated() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_and_close(dir.path(), false);
    let wal_before = bytes_with_extension(dir.path(), "log");
    let sst_before = bytes_with_extension(dir.path(), "sst");

    let db = EmbeddedDatabase::open_read_only(dir.path()).expect("read-only open");
    let rows = db.query("SELECT count(*) FROM t", &[]).expect("count");
    assert_eq!(rows.len(), 1);
    drop(db);

    assert_eq!(bytes_with_extension(dir.path(), "log"), wal_before);
    assert_eq!(bytes_with_extension(dir.path(), "sst"), sst_before);
}
