//! Actual Nano/RocksDB contract for the offline physical snapshot helper.
//! This deliberately makes no online replication continuation claim.
#![cfg(target_os = "linux")]

use heliosdb_nano::{EmbeddedDatabase, Value};
use std::path::Path;
use std::process::{Command, Output};

fn assert_seed_rows(db: &EmbeddedDatabase, expected: &[(i64, &str)]) {
    let rows = db
        .query("SELECT id, label FROM seed_data ORDER BY id", &[])
        .expect("read seed rows");
    assert_eq!(rows.len(), expected.len(), "snapshot row cardinality");
    for (row, (id, label)) in rows.iter().zip(expected) {
        let actual_id = match row.get(0) {
            Some(Value::Int4(value)) => i64::from(*value),
            Some(Value::Int8(value)) => *value,
            other => panic!("unexpected seed ID: {other:?}"),
        };
        assert_eq!(actual_id, *id);
        assert_eq!(row.get(1), Some(&Value::String((*label).to_string())));
    }
}

fn copy_offline(source: &Path, target: &Path) -> Output {
    Command::new("python3")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/nano-offline-reseed.py"))
        .arg("--source")
        .arg(source)
        .arg("--target")
        .arg(target)
        .arg("--confirm-offline")
        .output()
        .expect("run offline reseed helper")
}

#[test]
fn offline_reseed_refuses_a_live_nano_directory() {
    let temp = tempfile::tempdir().expect("fixture");
    let source = temp.path().join("primary");
    let target = temp.path().join("snapshot");
    let db = EmbeddedDatabase::new(&source).expect("open primary");
    db.execute("CREATE TABLE seed_lock (id INT PRIMARY KEY)")
        .expect("schema");
    let output = copy_offline(&source, &target);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("source is open by another process"));
    assert!(!target.exists());
    db.execute("INSERT INTO seed_lock VALUES (1)")
        .expect("source stays usable");
}

#[test]
fn offline_reseed_preserves_rows_indexes_and_independent_storage() {
    let temp = tempfile::tempdir().expect("fixture");
    let source = temp.path().join("primary");
    let target = temp.path().join("snapshot");
    {
        let db = EmbeddedDatabase::new(&source).expect("open primary");
        db.execute("CREATE TABLE seed_data (id INT PRIMARY KEY, label TEXT UNIQUE)")
            .expect("schema");
        db.execute("INSERT INTO seed_data VALUES (1, 'first'), (2, 'second')")
            .expect("rows");
        db.flush().expect("flush");
    }
    let output = copy_offline(&source, &target);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    {
        let copy = EmbeddedDatabase::new(&target).expect("snapshot opens");
        assert_seed_rows(&copy, &[(1, "first"), (2, "second")]);
        assert!(copy.execute("INSERT INTO seed_data VALUES (3, 'first')").is_err());
        copy.execute("INSERT INTO seed_data VALUES (3, 'third')")
            .expect("row counter/index state supports new writes");
        assert_seed_rows(&copy, &[(1, "first"), (2, "second"), (3, "third")]);
    }
    let original = EmbeddedDatabase::new(&source).expect("source reopens");
    assert_seed_rows(&original, &[(1, "first"), (2, "second")]);
    let rejected = copy_offline(&source, &target);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("target already exists"));
}
