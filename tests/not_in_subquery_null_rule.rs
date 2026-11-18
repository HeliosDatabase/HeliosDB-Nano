//! `x NOT IN (subquery)` beside a NULL keeps no row, whatever the subquery's size.
//!
//! SQL: `x IN (set)` is true on a match, unknown when nothing matched and the
//! set holds a NULL, false otherwise; `NOT IN` is its negation, and a `WHERE`
//! keeps only rows whose predicate is true. PostgreSQL, MySQL and SQLite agree.
//!
//! A subquery result of up to 16 rows was evaluated as a list, which followed
//! the rule. Above 16 rows it became a set whose NULL member was ignored:
//! `id NOT IN (SELECT x FROM big)` returned every row without a match where it
//! must return none. The boundary is pinned on both sides.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

fn int(value: &Value) -> i64 {
    match value {
        Value::Int2(n) => i64::from(*n),
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        other => panic!("not an integer: {other:?}"),
    }
}

/// `a(id)` = 1, 2, 3, 100. `members(x)` = 1 and the numbers 1001 … up to
/// `rows` rows in all, the last of them NULL when `with_null`.
fn database(rows: usize, with_null: bool) -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE a (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO a VALUES (1), (2), (3), (100)").unwrap();
    db.execute("CREATE TABLE members (k INT PRIMARY KEY, x INT)").unwrap();
    for k in 0..rows {
        let x = if k == 0 {
            "1".to_string()
        } else if with_null && k == rows - 1 {
            "NULL".to_string()
        } else {
            (1000 + k).to_string()
        };
        db.execute(&format!("INSERT INTO members VALUES ({k}, {x})")).unwrap();
    }
    db
}

fn ids(db: &EmbeddedDatabase, sql: &str) -> Vec<i64> {
    let plain: Vec<i64> = db
        .query(sql, &[])
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
        .iter()
        .map(|row| int(&row.values[0]))
        .collect();
    // The same statement with a bound parameter takes the other statement family.
    let bound_sql = sql.replace("FROM a WHERE", "FROM a WHERE id <> $1 AND");
    let bound: Vec<i64> = db
        .query_params(&bound_sql, &[Value::Int4(-1)])
        .unwrap_or_else(|error| panic!("{bound_sql}: {error}"))
        .iter()
        .map(|row| int(&row.values[0]))
        .collect();
    assert_eq!(plain, bound, "{sql}: text and bound forms differ");
    plain
}

#[test]
fn not_in_beside_a_null_keeps_no_row_at_every_size() {
    for rows in [2, 16, 17, 18, 40] {
        let db = database(rows, true);
        assert_eq!(
            ids(
                &db,
                "SELECT id FROM a WHERE id NOT IN (SELECT x FROM members) ORDER BY id"
            ),
            Vec::<i64>::new(),
            "{rows} rows with a NULL"
        );
    }
}

#[test]
fn not_in_without_a_null_keeps_the_rows_without_a_match() {
    for rows in [2, 16, 17, 18, 40] {
        let db = database(rows, false);
        assert_eq!(
            ids(
                &db,
                "SELECT id FROM a WHERE id NOT IN (SELECT x FROM members) ORDER BY id"
            ),
            [2, 3, 100],
            "{rows} rows without a NULL"
        );
    }
}

#[test]
fn in_beside_a_null_is_true_on_a_match_and_unknown_otherwise() {
    for rows in [2, 16, 17, 18, 40] {
        let db = database(rows, true);
        assert_eq!(
            ids(&db, "SELECT id FROM a WHERE id IN (SELECT x FROM members) ORDER BY id"),
            [1],
            "{rows} rows"
        );
        // Unknown, not false: `IS NULL` sees it.
        assert_eq!(
            ids(
                &db,
                "SELECT id FROM a WHERE (id IN (SELECT x FROM members)) IS NULL ORDER BY id"
            ),
            [2, 3, 100],
            "{rows} rows"
        );
        assert_eq!(
            ids(
                &db,
                "SELECT id FROM a WHERE (id NOT IN (SELECT x FROM members)) IS NULL ORDER BY id"
            ),
            [2, 3, 100],
            "{rows} rows"
        );
    }
}

#[test]
fn a_null_on_the_left_is_unknown_and_an_empty_subquery_decides_alone() {
    let db = database(40, false);
    db.execute("CREATE TABLE b (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO b VALUES (1, 1), (2, NULL), (3, 7)").unwrap();
    let rows = |sql: &str| -> Vec<i64> {
        db.query(sql, &[])
            .unwrap_or_else(|error| panic!("{sql}: {error}"))
            .iter()
            .map(|row| int(&row.values[0]))
            .collect()
    };
    assert_eq!(
        rows("SELECT id FROM b WHERE v NOT IN (SELECT x FROM members) ORDER BY id"),
        [3]
    );
    assert_eq!(
        rows("SELECT id FROM b WHERE v IN (SELECT x FROM members) ORDER BY id"),
        [1]
    );
    // Nothing to compare with: NOT IN is true for every row, a NULL one too.
    assert_eq!(
        rows("SELECT id FROM b WHERE v NOT IN (SELECT x FROM members WHERE x < 0) ORDER BY id"),
        [1, 2, 3]
    );
}
