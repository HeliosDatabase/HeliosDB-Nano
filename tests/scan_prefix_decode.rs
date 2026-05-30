//! End-to-end: the projection-aware prefix decode (issue #1 follow-up) must never
//! change query results. A query that reads only early columns must skip the wide
//! tail; a query that references a tail column must still read it correctly.

use heliosdb_nano::{EmbeddedDatabase, Value};

fn int(v: &Value) -> i64 {
    match v {
        Value::Int8(n) => *n,
        Value::Int4(n) => i64::from(*n),
        Value::Int2(n) => i64::from(*n),
        other => panic!("expected int, got {other:?}"),
    }
}

fn float(v: &Value) -> f64 {
    match v {
        Value::Float8(n) => *n,
        Value::Float4(n) => f64::from(*n),
        other => panic!("expected float, got {other:?}"),
    }
}

#[test]
fn prefix_decode_preserves_results() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE w (id INT, k TEXT, payload TEXT, note TEXT)")
        .unwrap();
    for n in 0..300 {
        db.execute(&format!(
            "INSERT INTO w VALUES ({n}, 'k{}', 'payload-{n}-xxxxxxxxxxxxxxxx', 'note-{n}')",
            n % 3
        ))
        .unwrap();
    }

    // Needs only k (idx 1) — tail (payload idx2, note idx3) is skipped by the decode.
    let (r, _) = db.query_with_columns("SELECT COUNT(DISTINCT k) AS n FROM w").unwrap();
    assert_eq!(int(&r[0].values[0]), 3, "COUNT(DISTINCT k)");

    // Needs no columns at all (prefix 0).
    let (r, _) = db.query_with_columns("SELECT COUNT(*) AS n FROM w").unwrap();
    assert_eq!(int(&r[0].values[0]), 300, "COUNT(*)");

    // Filter on id (idx0), project k (idx1): tail ignored, result correct.
    let (r, _) = db.query_with_columns("SELECT k FROM w WHERE id = 7").unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].values[0], Value::String("k1".into())); // 7 % 3 == 1

    // Referencing a TAIL column must still read it (prefix must cover it).
    let (r, _) = db.query_with_columns("SELECT payload FROM w WHERE id = 7").unwrap();
    assert_eq!(r[0].values[0], Value::String("payload-7-xxxxxxxxxxxxxxxx".into()));
    let (r, _) = db.query_with_columns("SELECT note FROM w WHERE id = 299").unwrap();
    assert_eq!(r[0].values[0], Value::String("note-299".into()));

    // GROUP BY early column, aggregate present.
    let (r, _) = db
        .query_with_columns("SELECT k, COUNT(*) AS c FROM w GROUP BY k ORDER BY k")
        .unwrap();
    assert_eq!(r.len(), 3);
    assert_eq!(int(&r[0].values[1]), 100);

    // SELECT * must read all columns (no wildcard optimization → still correct).
    let (r, _) = db.query_with_columns("SELECT * FROM w WHERE id = 100").unwrap();
    assert_eq!(r[0].values.len(), 4);
    assert_eq!(r[0].values[3], Value::String("note-100".into()));
}

#[test]
fn count_star_pk_range_handles_negative_keys() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE n (id INTEGER PRIMARY KEY, v TEXT)").unwrap();
    for id in [-3, -1, 0, 1, 4] {
        db.execute(&format!("INSERT INTO n VALUES ({id}, 'v{id}')"))
            .unwrap();
    }

    let rows = db.query("SELECT COUNT(*) FROM n WHERE id >= 0", &[]).unwrap();
    assert_eq!(int(&rows[0].values[0]), 3);

    let rows = db.query("SELECT COUNT(*) FROM n WHERE id < 0", &[]).unwrap();
    assert_eq!(int(&rows[0].values[0]), 2);

    let rows = db.query("SELECT COUNT(*) FROM n WHERE 0 <= id", &[]).unwrap();
    assert_eq!(int(&rows[0].values[0]), 3);

    let rows = db
        .query("SELECT COUNT(*) FROM n WHERE id >= -1 AND id <= 1", &[])
        .unwrap();
    assert_eq!(int(&rows[0].values[0]), 3);

    let rows = db
        .query("SELECT COUNT(*) FROM n WHERE id BETWEEN -1 AND 1", &[])
        .unwrap();
    assert_eq!(int(&rows[0].values[0]), 3);

    let rows = db
        .query_params(
            "SELECT COUNT(*) FROM n WHERE id >= $1 AND id <= $2",
            &[Value::Int4(-1), Value::Int4(1)],
        )
        .unwrap();
    assert_eq!(int(&rows[0].values[0]), 3);

    let rows = db.query("SELECT COUNT(*) FROM n WHERE id > 1 AND id < 1", &[]).unwrap();
    assert_eq!(int(&rows[0].values[0]), 0);
}

#[test]
fn count_star_uses_live_pk_index_size_after_mutations() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE n (id INTEGER PRIMARY KEY, v TEXT)").unwrap();
    for id in 1..=5 {
        db.execute(&format!("INSERT INTO n VALUES ({id}, 'v{id}')")).unwrap();
    }

    assert_eq!(db.storage.art_indexes().pk_index_len("n"), Some(5));
    let rows = db.query("SELECT COUNT(*) FROM n", &[]).unwrap();
    assert_eq!(int(&rows[0].values[0]), 5);

    db.execute("DELETE FROM n WHERE id = 3").unwrap();
    assert_eq!(db.storage.art_indexes().pk_index_len("n"), Some(4));
    let rows = db.query("SELECT COUNT(*) FROM n", &[]).unwrap();
    assert_eq!(int(&rows[0].values[0]), 4);

    db.execute("TRUNCATE TABLE n").unwrap();
    assert_eq!(db.storage.art_indexes().pk_index_len("n"), Some(0));
    let rows = db.query("SELECT COUNT(*) FROM n", &[]).unwrap();
    assert_eq!(int(&rows[0].values[0]), 0);
}

#[test]
fn count_distinct_pk_uses_index_cardinality() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE n (id INTEGER PRIMARY KEY, bucket INT, v TEXT)")
        .unwrap();
    for id in -10i32..=10 {
        db.execute(&format!("INSERT INTO n VALUES ({id}, {}, 'v{id}')", id.rem_euclid(3)))
            .unwrap();
    }

    let rows = db.query("SELECT COUNT(DISTINCT id) FROM n", &[]).unwrap();
    assert_eq!(int(&rows[0].values[0]), 21);

    let rows = db
        .query("SELECT COUNT(DISTINCT id) FROM n WHERE id >= -3 AND id < 5", &[])
        .unwrap();
    assert_eq!(int(&rows[0].values[0]), 8);

    db.execute("DELETE FROM n WHERE id = 0").unwrap();
    let rows = db.query("SELECT COUNT(DISTINCT id) FROM n", &[]).unwrap();
    assert_eq!(int(&rows[0].values[0]), 20);

    let rows = db.query("SELECT COUNT(DISTINCT bucket) FROM n", &[]).unwrap();
    assert_eq!(int(&rows[0].values[0]), 3);
}

#[test]
fn count_pk_and_count_star_pk_in_list_use_index_cardinality() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE n (id INTEGER PRIMARY KEY, bucket INT)").unwrap();
    for id in 1..=10 {
        db.execute(&format!("INSERT INTO n VALUES ({id}, {})", id % 3))
            .unwrap();
    }

    let rows = db.query("SELECT COUNT(id) FROM n", &[]).unwrap();
    assert_eq!(int(&rows[0].values[0]), 10);

    let rows = db
        .query("SELECT COUNT(*) FROM n WHERE id IN (1, 1, 3, NULL, 99)", &[])
        .unwrap();
    assert_eq!(int(&rows[0].values[0]), 2);

    let rows = db
        .query("SELECT COUNT(id) FROM n WHERE id IN (2, 2, 4, 100)", &[])
        .unwrap();
    assert_eq!(int(&rows[0].values[0]), 2);

    let rows = db
        .query_params(
            "SELECT COUNT(DISTINCT id) FROM n WHERE id IN ($1, $2, $3)",
            &[Value::Int4(5), Value::Int4(5), Value::Int4(7)],
        )
        .unwrap();
    assert_eq!(int(&rows[0].values[0]), 2);

    let rows = db.query("SELECT COUNT(bucket) FROM n WHERE id IN (1, 2, 3)", &[]).unwrap();
    assert_eq!(int(&rows[0].values[0]), 3);
}

#[test]
fn rowstore_aggregate_fast_path_preserves_filter_and_group_results() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, x INTEGER, y INTEGER, g INTEGER)")
        .unwrap();
    for id in 0..12 {
        db.execute(&format!(
            "INSERT INTO a (id, x, y, g) VALUES ({id}, {}, {}, {})",
            id % 5,
            id * 10,
            id % 3
        ))
        .unwrap();
    }
    db.execute("INSERT INTO a (id, x, y, g) VALUES (100, NULL, NULL, 1)")
        .unwrap();

    let rows = db
        .query("SELECT SUM(x), AVG(y), MAX(y) FROM a WHERE y >= 30", &[])
        .unwrap();
    assert_eq!(int(&rows[0].values[0]), 18);
    assert!((float(&rows[0].values[1]) - 70.0).abs() < 0.0001);
    assert_eq!(int(&rows[0].values[2]), 110);

    let rows = db
        .query("SELECT g, COUNT(*), SUM(x) FROM a WHERE y >= 30 GROUP BY g", &[])
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].values, vec![Value::Int4(0), Value::Int8(3), Value::Int8(8)]);
    assert_eq!(rows[1].values, vec![Value::Int4(1), Value::Int8(3), Value::Int8(6)]);
    assert_eq!(rows[2].values, vec![Value::Int4(2), Value::Int8(3), Value::Int8(4)]);

    // NOT-EQ with NULL must keep SQL three-valued logic. The row-store aggregate
    // fast path deliberately falls back here because the storage FilterPredicate
    // NotEq helper treats NULL as a positive match.
    let rows = db.query("SELECT COUNT(*) FROM a WHERE x != 1", &[]).unwrap();
    assert_eq!(int(&rows[0].values[0]), 9);
}

#[test]
fn storage_filter_pushdown_preserves_sql_null_predicates() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE f (id INTEGER PRIMARY KEY, x INTEGER, payload TEXT)")
        .unwrap();
    db.execute("INSERT INTO f VALUES (1, 1, 'a')").unwrap();
    db.execute("INSERT INTO f VALUES (2, 2, 'b')").unwrap();
    db.execute("INSERT INTO f VALUES (3, NULL, 'n')").unwrap();
    db.execute("INSERT INTO f VALUES (4, 20, 'c')").unwrap();

    let rows = db
        .query("SELECT id FROM f WHERE x > 1 ORDER BY id", &[])
        .unwrap();
    assert_eq!(rows.iter().map(|row| int(&row.values[0])).collect::<Vec<_>>(), vec![2, 4]);

    let rows = db
        .query("SELECT id FROM f WHERE x != 1 ORDER BY id", &[])
        .unwrap();
    assert_eq!(rows.iter().map(|row| int(&row.values[0])).collect::<Vec<_>>(), vec![2, 4]);
}
