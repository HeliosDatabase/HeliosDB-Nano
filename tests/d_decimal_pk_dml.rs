//! BUG D regression: DELETE/UPDATE by a DECIMAL/NUMERIC primary key matched 0
//! rows while the identical SELECT matched 1.
//!
//! Root cause: the fast-DML PK path (`fast_parse_one_value`) parsed a numeric
//! literal for a NUMERIC column as `Int8`/`Float8`, whose ART key encoding
//! (sign-flipped 8-byte int) never matched the stored `Numeric("6")` key (the
//! bytes of the decimal string), so the PK index lookup missed every time.
//! Reported by Any2HeliosDB: Oracle `NUMBER(p,0)` PKs map to `DECIMAL(p,0)`, so
//! CDC delete-reconcile silently no-oped on those tables.

use heliosdb_nano::{EmbeddedDatabase, Value};

fn count(db: &EmbeddedDatabase, sql: &str) -> i64 {
    let rows = db.query(sql, &[]).unwrap();
    assert_eq!(rows.len(), 1, "expected one count row for {sql}");
    match &rows[0].values[0] {
        Value::Int2(n) => i64::from(*n),
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        other => panic!("expected integer count, got {other:?}"),
    }
}

#[test]
fn delete_and_update_by_decimal_pk_match_the_row() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE d_dml (id DECIMAL(10,0) PRIMARY KEY, note TEXT)")
        .unwrap();
    db.execute("INSERT INTO d_dml VALUES (6,'six'),(7,'seven'),(8,'eight')")
        .unwrap();

    // SELECT has always worked (full-scan predicate coercion).
    assert_eq!(count(&db, "SELECT count(*) FROM d_dml WHERE id = 6"), 1);

    // DELETE by bare integer literal against the DECIMAL PK (fast path).
    let deleted = db.execute("DELETE FROM d_dml WHERE id = 6").unwrap();
    assert_eq!(deleted, 1, "DELETE by DECIMAL PK should remove exactly 1 row");
    assert_eq!(count(&db, "SELECT count(*) FROM d_dml"), 2);
    assert_eq!(count(&db, "SELECT count(*) FROM d_dml WHERE id = 6"), 0);

    // UPDATE by bare integer literal against the DECIMAL PK (same fast path).
    let updated = db.execute("UPDATE d_dml SET note = 'VII' WHERE id = 7").unwrap();
    assert_eq!(updated, 1, "UPDATE by DECIMAL PK should touch exactly 1 row");
    let rows = db.query("SELECT note FROM d_dml WHERE id = 7", &[]).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::String("VII".to_string()));
}
