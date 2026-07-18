//! BUG B regression: DROP TABLE of a populated table must delete every row in
//! a single batched write (no per-row WAL fsync) — and still be *correct*: a
//! table recreated with the same name must not see any stale rows.
//!
//! Root cause was one `fdatasync` per deleted row (`catalog.drop_table` looped
//! `storage.delete()`), so DROP cost O(rows) fsyncs and a Pagila-sized table
//! appeared to hang and monopolized the WAL. Reported by Any2HeliosDB.

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
fn drop_populated_table_deletes_all_rows_and_recreate_is_empty() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE bdrop (id int primary key, v text)").unwrap();
    let vals: String = (1..=500).map(|i| format!("({i},'v{i}')")).collect::<Vec<_>>().join(",");
    db.execute(&format!("INSERT INTO bdrop VALUES {vals}")).unwrap();
    assert_eq!(count(&db, "SELECT count(*) FROM bdrop"), 500);

    // The batched delete must remove every data row.
    db.execute("DROP TABLE bdrop").unwrap();

    // Recreate with the same name: must start empty (no stale rows survived the
    // batched prefix delete).
    db.execute("CREATE TABLE bdrop (id int primary key, v text)").unwrap();
    assert_eq!(count(&db, "SELECT count(*) FROM bdrop"), 0);
    db.execute("INSERT INTO bdrop VALUES (1,'fresh')").unwrap();
    assert_eq!(count(&db, "SELECT count(*) FROM bdrop"), 1);
    assert_eq!(count(&db, "SELECT count(*) FROM bdrop WHERE id = 1"), 1);
}

#[test]
fn drop_fk_linked_child_then_parent() {
    // FK-linked tables drop cleanly (the reported scenario). Nano permits
    // dropping a referenced parent (a deliberate divergence that Any2HeliosDB's
    // drop_existing relies on); this just asserts no hang/error on the path.
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE bparent (id int primary key, n text)").unwrap();
    db.execute("CREATE TABLE bchild (id int primary key, pid int references bparent(id), v text)")
        .unwrap();
    db.execute("INSERT INTO bparent VALUES (1,'p')").unwrap();
    let vals: String = (1..=300)
        .map(|i| format!("({i},1,'c{i}')"))
        .collect::<Vec<_>>()
        .join(",");
    db.execute(&format!("INSERT INTO bchild VALUES {vals}")).unwrap();
    db.execute("DROP TABLE bchild").unwrap();
    db.execute("DROP TABLE bparent").unwrap();
    // Recreate both; child table is empty.
    db.execute("CREATE TABLE bchild (id int primary key, v text)").unwrap();
    assert_eq!(count(&db, "SELECT count(*) FROM bchild"), 0);
}
