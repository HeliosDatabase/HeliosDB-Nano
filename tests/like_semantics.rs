#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
//! v01.7: one LIKE matcher on every route. The same pattern returns the same
//! rows whether the literal is normalized to a parameter, kept as a literal and
//! pushed to storage, run inside a transaction, or bound as `$1`; ESCAPE is
//! honoured; a pattern with many `%` cannot pin a core.

use heliosdb_nano::{EmbeddedDatabase, Value};

fn db() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE lk (id INT PRIMARY KEY, s TEXT)")
        .expect("create");
    for (id, s) in [
        (1, "a\nb"),
        (2, "100%"),
        (3, "100x"),
        (4, "a_b"),
        (5, "axb"),
        (6, "ab"),
        (7, "100\\%"),
        (8, "é"),
        (9, "ÉB"),
    ] {
        db.execute_params(
            "INSERT INTO lk VALUES ($1, $2)",
            &[Value::Int4(id), Value::String(s.into())],
        )
        .expect("insert");
    }
    db.execute("INSERT INTO lk VALUES (10, NULL)").expect("insert null");
    db
}

fn ids(rows: &[heliosdb_nano::Tuple], col: usize) -> Vec<i64> {
    let mut out: Vec<i64> = rows
        .iter()
        .map(|r| match &r.values[col] {
            Value::Int4(v) => i64::from(*v),
            Value::Int8(v) => *v,
            other => panic!("id {other:?}"),
        })
        .collect();
    out.sort_unstable();
    out
}

/// SQL literal for `pattern`, standard-conforming (backslash is not special).
fn lit(pattern: &str) -> String {
    format!("'{}'", pattern.replace('\'', "''"))
}

/// `s <op> pattern` through four routes; all must equal `want`.
fn routes(db: &EmbeddedDatabase, op: &str, pattern: &str, want: &[i64]) {
    let where_lit = format!("s {op} {}", lit(pattern));
    // 1. autocommit literal (the normalizer may turn it into a parameter)
    let a = db
        .query(&format!("SELECT id FROM lk WHERE {where_lit}"), &[])
        .unwrap_or_else(|e| panic!("{where_lit}: {e}"));
    assert_eq!(ids(&a, 0), want, "autocommit: {where_lit}");
    // 2. a `$$x$$` select-list item makes the normalizer decline: the literal
    //    stays a literal and the storage pushdown takes it
    let b = db
        .query(&format!("SELECT $$r$$, id FROM lk WHERE {where_lit}"), &[])
        .unwrap_or_else(|e| panic!("$$ route {where_lit}: {e}"));
    assert_eq!(ids(&b, 1), want, "literal route: {where_lit}");
    // 3. inside a transaction
    db.execute("BEGIN").expect("begin");
    let c = db
        .query(&format!("SELECT id FROM lk WHERE {where_lit}"), &[])
        .unwrap_or_else(|e| panic!("txn {where_lit}: {e}"));
    db.execute("COMMIT").expect("commit");
    assert_eq!(ids(&c, 0), want, "transaction: {where_lit}");
    // 4. the pattern bound as $1
    let d = db
        .query_params(
            &format!("SELECT id FROM lk WHERE s {op} $1"),
            &[Value::String(pattern.into())],
        )
        .unwrap_or_else(|e| panic!("param {where_lit}: {e}"));
    assert_eq!(ids(&d, 0), want, "parameter: {where_lit}");
}

#[test]
fn every_route_agrees() {
    let db = db();
    routes(&db, "LIKE", "a%b", &[1, 4, 5, 6]);
    routes(&db, "LIKE", "100\\%", &[2]);
    routes(&db, "LIKE", "100%", &[2, 3, 7]);
    routes(&db, "LIKE", "a\\_b", &[4]);
    routes(&db, "LIKE", "a_b", &[1, 4, 5]);
    routes(&db, "LIKE", "_", &[8]);
    routes(&db, "LIKE", "%b", &[1, 4, 5, 6]);
    routes(&db, "LIKE", "%x%", &[3, 5]);
    routes(&db, "LIKE", "", &[]);
    routes(&db, "NOT LIKE", "a%b", &[2, 3, 7, 8, 9]);
    routes(&db, "ILIKE", "éb", &[9]);
    routes(&db, "ILIKE", "A%B", &[1, 4, 5, 6]);
}

#[test]
fn null_operands_are_unknown() {
    let db = db();
    let rows = db
        .query(
            "SELECT NULL LIKE 'a', 'a' LIKE NULL, s LIKE 'a' FROM lk WHERE id = 10",
            &[],
        )
        .expect("q");
    assert_eq!(rows[0].values, vec![Value::Null, Value::Null, Value::Null]);
}

#[test]
fn escape_is_honoured() {
    let db = db();
    let one = |sql: &str| -> Value { db.query(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"))[0].values[0].clone() };
    assert_eq!(one("SELECT '100%' LIKE '100!%' ESCAPE '!'"), Value::Boolean(true));
    assert_eq!(one("SELECT '100x' LIKE '100!%' ESCAPE '!'"), Value::Boolean(false));
    assert_eq!(one("SELECT '100x' NOT LIKE '100!%' ESCAPE '!'"), Value::Boolean(true));
    assert_eq!(one("SELECT 'A%' ILIKE 'a!%' ESCAPE '!'"), Value::Boolean(true));
    // ESCAPE '' disables escaping: the backslash is an ordinary character.
    assert_eq!(one("SELECT 'a\\b' LIKE 'a\\b' ESCAPE ''"), Value::Boolean(true));
    let err = db
        .query("SELECT 'a' LIKE 'a' ESCAPE 'ab'", &[])
        .expect_err("two-char escape");
    assert!(err.to_string().contains("invalid escape string"), "{err}");
    let err = db.query("SELECT 'a' LIKE 'a\\'", &[]).expect_err("trailing escape");
    assert!(err.to_string().contains("must not end with escape character"), "{err}");

    for sql in [
        "SELECT id FROM lk WHERE s LIKE '100!%' ESCAPE '!'",
        "SELECT $$r$$, id FROM lk WHERE s LIKE '100!%' ESCAPE '!'",
    ] {
        let rows = db.query(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let col = usize::from(sql.contains("$$r$$"));
        assert_eq!(ids(&rows, col), vec![2], "{sql}");
    }
    db.execute("BEGIN").expect("begin");
    let rows = db
        .query("SELECT id FROM lk WHERE s LIKE '100!%' ESCAPE '!'", &[])
        .expect("txn");
    db.execute("COMMIT").expect("commit");
    assert_eq!(ids(&rows, 0), vec![2]);
}

#[test]
fn like_any_is_refused_not_misread() {
    let db = db();
    assert!(db.query("SELECT id FROM lk WHERE s LIKE ANY ('a%')", &[]).is_err());
}

#[test]
fn an_adversarial_pattern_is_fast_on_the_pushed_route() {
    let db = db();
    db.execute(&format!("INSERT INTO lk VALUES (100, '{}')", "a".repeat(100)))
        .expect("insert");
    let start = std::time::Instant::now();
    let rows = db
        .query("SELECT $$r$$, id FROM lk WHERE s LIKE '%a%a%a%a%a%a%b'", &[])
        .expect("query");
    let elapsed = start.elapsed();
    assert!(ids(&rows, 1).is_empty());
    assert!(elapsed < std::time::Duration::from_millis(500), "took {elapsed:?}");
}
