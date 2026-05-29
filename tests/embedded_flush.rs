//! Tests for `EmbeddedDatabase::flush()` — forces the memtable→SST split so reads,
//! aggregates, and MV materialization can be exercised across the full LSM tree.
//! Added while investigating Quirk J / issue #2 (MV aggregates reportedly wrong at
//! scale); flush() lets a future test reproduce the multi-SST condition deterministically.

use heliosdb_nano::{EmbeddedDatabase, Value};

fn count(db: &EmbeddedDatabase, sql: &str) -> i64 {
    let (rows, _) = db.query_with_columns(sql).expect(sql);
    match rows[0].values.first() {
        Some(Value::Int8(n)) => *n,
        Some(Value::Int4(n)) => i64::from(*n),
        other => panic!("expected integer, got {other:?}"),
    }
}

#[test]
fn flush_then_reads_span_sst_and_memtable() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE t (id INT, k TEXT)").expect("create");

    // Batch 1 → flushed to SST.
    db.execute("INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'a')")
        .expect("insert b1");
    db.flush().expect("flush");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM t"), 3, "rows visible after flush");

    // Batch 2 → memtable. Reads + aggregates must span both.
    db.execute("INSERT INTO t VALUES (4,'b'),(5,'a')").expect("insert b2");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM t"), 5, "COUNT spans SST + memtable");
    assert_eq!(
        count(&db, "SELECT COUNT(DISTINCT k) FROM t"),
        2,
        "COUNT(DISTINCT) spans SST + memtable"
    );
}
