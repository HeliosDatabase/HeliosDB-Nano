//! Regression tests for GH#60-#65 (filed 2026-10-07 from the SQLite-SDK PoC).
#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

fn db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("database")
}

fn ints(db: &EmbeddedDatabase, sql: &str) -> Vec<Vec<i64>> {
    db.query(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .iter()
        .map(|r| {
            r.values
                .iter()
                .map(|v| match v {
                    Value::Int2(i) => i64::from(*i),
                    Value::Int4(i) => i64::from(*i),
                    Value::Int8(i) => *i,
                    other => panic!("{sql}: unexpected {other:?}"),
                })
                .collect()
        })
        .collect()
}

#[test]
fn gh65_sum_over_no_rows_is_null_with_group_concat_or_string_agg() {
    let db = db();
    db.execute("CREATE TABLE t9 (a DOUBLE PRECISION, b BIGINT, s TEXT)")
        .expect("create");
    for sql in [
        "SELECT sum(b), group_concat(s) FROM t9",
        "SELECT sum(b), string_agg(s, ',') FROM t9",
        "SELECT sum(a), avg(a), group_concat(s), count(s) FROM t9",
    ] {
        let rows = db.query(sql, &[]).expect(sql);
        assert_eq!(rows.len(), 1, "{sql}");
        assert_eq!(rows[0].values[0], Value::Null, "sum must be NULL: {sql}");
        if sql.contains("avg") {
            assert_eq!(rows[0].values[2], Value::Null, "group_concat must be NULL: {sql}");
            assert_eq!(rows[0].values[3], Value::Int8(0));
        } else {
            assert_eq!(rows[0].values[1], Value::Null, "aggregate must be NULL: {sql}");
        }
    }
    // Non-empty input is unchanged.
    db.execute("INSERT INTO t9 VALUES (1.5, 2, 'x'), (2.5, 3, 'y')")
        .expect("insert");
    let rows = db.query("SELECT sum(b), group_concat(s) FROM t9", &[]).expect("sum");
    assert_eq!(rows[0].values[0], Value::Int8(5));
}

#[test]
fn gh61_add_column_in_transaction_pads_rows_staged_earlier_in_it() {
    let db = db();
    db.execute("CREATE TABLE t (a BIGINT)").expect("create");
    db.execute("BEGIN").expect("begin");
    db.execute("INSERT INTO t VALUES (1)").expect("insert");
    db.execute("ALTER TABLE t ADD COLUMN extra BIGINT").expect("alter");
    let rows = db.query("SELECT * FROM t", &[]).expect("select after alter");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values, vec![Value::Int8(1), Value::Null]);
    db.execute("UPDATE t SET extra = 5").expect("update");
    db.execute("COMMIT").expect("commit");
    assert_eq!(ints(&db, "SELECT a, extra FROM t"), vec![vec![1, 5]]);
}

#[test]
fn gh62_rows_staged_before_drop_table_do_not_reappear_in_recreated_table() {
    let db = db();
    db.execute("CREATE TABLE zz (a INT)").expect("create");
    db.execute("BEGIN").expect("begin");
    db.execute("INSERT INTO zz VALUES (1)").expect("insert");
    db.execute("DROP TABLE zz").expect("drop");
    db.execute("COMMIT").expect("commit");
    db.execute("CREATE TABLE zz (a INT)").expect("recreate");
    assert!(db.query("SELECT * FROM zz", &[]).expect("scan").is_empty());
}

#[test]
fn gh63_order_by_qualified_column_and_position_in_joins() {
    let db = db();
    db.execute("CREATE TABLE ob_p (id BIGINT PRIMARY KEY, name TEXT)")
        .expect("p");
    db.execute("CREATE TABLE ob_k (id BIGINT PRIMARY KEY, pid BIGINT, name TEXT, amt DOUBLE PRECISION)")
        .expect("k");
    db.execute("INSERT INTO ob_p VALUES (1, 'p'), (2, 'q')").expect("ins p");
    db.execute("INSERT INTO ob_k VALUES (10, 1, 'a', 1), (11, 1, 'b', 3), (12, 2, 'c', 2)")
        .expect("ins k");
    let want = vec![vec![2, 12], vec![1, 11], vec![1, 10]];
    for sql in [
        "SELECT ob_p.id, ob_k.id FROM ob_p JOIN ob_k ON ob_k.pid = ob_p.id ORDER BY ob_k.id DESC",
        "SELECT ob_p.id, ob_k.id FROM ob_p JOIN ob_k ON ob_k.pid = ob_p.id ORDER BY 2 DESC",
        "SELECT x.id, y.id FROM ob_p x JOIN ob_k y ON y.pid = x.id ORDER BY y.id DESC",
    ] {
        assert_eq!(ints(&db, sql), want, "{sql}");
    }
    let names: Vec<String> = db
        .query(
            "SELECT ob_k.name FROM ob_p JOIN ob_k ON ob_k.pid = ob_p.id ORDER BY ob_k.name DESC",
            &[],
        )
        .expect("names")
        .iter()
        .map(|r| r.values[0].to_string().trim_matches('\'').to_string())
        .collect();
    assert_eq!(names, vec!["c", "b", "a"]);
    // Aliased select list + a qualified key that is not projected.
    assert_eq!(
        ints(
            &db,
            "SELECT ob_k.id AS a FROM ob_k ORDER BY ob_k.pid DESC, ob_k.id DESC"
        ),
        vec![vec![12], vec![11], vec![10]]
    );
    // Previously-working forms stay working.
    assert_eq!(
        ints(
            &db,
            "SELECT ob_p.id AS a, ob_k.id AS b FROM ob_p JOIN ob_k ON ob_k.pid = ob_p.id ORDER BY b DESC"
        ),
        want
    );
}

#[test]
fn gh60_generated_key_never_duplicates_explicit_key_in_transaction() {
    let db = db();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)")
        .expect("create");
    db.execute("BEGIN").expect("begin");
    db.execute("INSERT INTO t (id, v) VALUES (2, 'explicit')")
        .expect("explicit");
    let mut failed = false;
    for v in ["a", "b"] {
        if db
            .query(&format!("INSERT INTO t (v) VALUES ('{v}') RETURNING id"), &[])
            .is_err()
        {
            failed = true;
        }
    }
    let _ = db.execute(if failed { "ROLLBACK" } else { "COMMIT" });
    let rows = ints(&db, "SELECT id FROM t ORDER BY id");
    let mut ids: Vec<i64> = rows.iter().map(|r| r[0]).collect();
    let n = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), n, "duplicate primary key stored: {rows:?}");
}

#[test]
fn gh64_sequences_are_per_database() {
    let a = db();
    a.execute("CREATE SEQUENCE ks").expect("seq a");
    a.query("SELECT nextval('ks')", &[]).expect("a1");
    a.query("SELECT nextval('ks')", &[]).expect("a2");
    let b = db();
    b.execute("CREATE SEQUENCE ks").expect("seq b");
    assert_eq!(ints(&b, "SELECT nextval('ks')"), vec![vec![1]]);
    assert_eq!(ints(&a, "SELECT nextval('ks')"), vec![vec![3]]);
}
