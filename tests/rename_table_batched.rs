//! ALTER TABLE … RENAME TO regression: renaming a populated table must move
//! every row in a single batched RocksDB write — no per-row WAL fsync, no
//! per-row re-encryption — and be correct: rows visible under the new name,
//! old name gone, row-counter carried over so new inserts don't collide.
//!
//! Root cause of the original hang: the move looped `storage.put()` +
//! `storage.delete()` per row, and every delete appends a logical-WAL entry
//! with a synchronous fdatasync — a 50k-row RENAME was ~50k fsyncs (15+ min,
//! non-cancellable, monopolized the WAL writer). Found by the 2026-07-04
//! baseline bench run; same family as the c478286 DROP-TABLE stall.

use heliosdb_nano::{EmbeddedDatabase, Value};

fn count(db: &EmbeddedDatabase, sql: &str) -> i64 {
    let rows = db.query(sql, &[]).unwrap();
    match &rows[0].values[0] {
        Value::Int2(n) => i64::from(*n),
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        other => panic!("expected integer count, got {other:?}"),
    }
}

#[test]
fn rename_populated_table_is_correct() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE ren_src (id int primary key, v text)").unwrap();
    let vals: String = (1..=500).map(|i| format!("({i},'v{i}')")).collect::<Vec<_>>().join(",");
    db.execute(&format!("INSERT INTO ren_src VALUES {vals}")).unwrap();
    db.execute("CREATE INDEX ren_src_v ON ren_src(v)").unwrap();
    assert_eq!(count(&db, "SELECT count(*) FROM ren_src"), 500);

    db.execute("ALTER TABLE ren_src RENAME TO ren_dst").unwrap();

    // All rows visible under the new name; point reads work.
    assert_eq!(count(&db, "SELECT count(*) FROM ren_dst"), 500);
    assert_eq!(count(&db, "SELECT count(*) FROM ren_dst WHERE id = 250"), 1);
    assert_eq!(count(&db, "SELECT count(*) FROM ren_dst WHERE v = 'v499'"), 1);

    // Old name is gone.
    assert!(db.query("SELECT count(*) FROM ren_src", &[]).is_err());

    // Row counter moved with the table: fresh inserts must not collide.
    db.execute("INSERT INTO ren_dst VALUES (501, 'fresh')").unwrap();
    assert_eq!(count(&db, "SELECT count(*) FROM ren_dst"), 501);

    // Old name reusable.
    db.execute("CREATE TABLE ren_src (id int primary key)").unwrap();
    assert_eq!(count(&db, "SELECT count(*) FROM ren_src"), 0);
}

#[test]
fn rename_populated_table_on_disk_is_not_per_row_fsync() {
    // Timing canary on a real (disk-backed) data dir. The batched rename is
    // O(ms); the per-row-fsync path was ≥2 WAL appends × 3k rows — minutes on
    // any disk. The generous bound keeps slow CI honest while still failing
    // decisively if per-row fsyncs come back.
    let temp = tempfile::tempdir().unwrap();
    let db = EmbeddedDatabase::new(temp.path()).unwrap();
    db.execute("CREATE TABLE ren_disk (id int primary key, v text)")
        .unwrap();
    for chunk in (1..=3000).collect::<Vec<i64>>().chunks(500) {
        let vals: String = chunk
            .iter()
            .map(|i| format!("({i},'v{i}')"))
            .collect::<Vec<_>>()
            .join(",");
        db.execute(&format!("INSERT INTO ren_disk VALUES {vals}")).unwrap();
    }
    assert_eq!(count(&db, "SELECT count(*) FROM ren_disk"), 3000);

    let start = std::time::Instant::now();
    db.execute("ALTER TABLE ren_disk RENAME TO ren_disk2").unwrap();
    let elapsed = start.elapsed();

    assert_eq!(count(&db, "SELECT count(*) FROM ren_disk2"), 3000);
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "RENAME of 3k rows took {elapsed:?} — per-row WAL fsync is back"
    );
}

#[test]
fn rename_survives_reopen() {
    // The rename batch must be durable in the data dir: reopen and verify.
    let temp = tempfile::tempdir().unwrap();
    {
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        db.execute("CREATE TABLE ren_p (id int primary key, v text)").unwrap();
        db.execute("INSERT INTO ren_p VALUES (1,'a'),(2,'b'),(3,'c')").unwrap();
        db.execute("ALTER TABLE ren_p RENAME TO ren_q").unwrap();
        assert_eq!(count(&db, "SELECT count(*) FROM ren_q"), 3);
    }
    {
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        assert_eq!(count(&db, "SELECT count(*) FROM ren_q"), 3);
        assert!(db.query("SELECT count(*) FROM ren_p", &[]).is_err());
        db.execute("INSERT INTO ren_q VALUES (4,'d')").unwrap();
        assert_eq!(count(&db, "SELECT count(*) FROM ren_q"), 4);
    }
}
