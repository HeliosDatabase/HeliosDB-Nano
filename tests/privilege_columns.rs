//! Column grants: `GRANT SELECT (a, b) ON t TO r` allows those columns and
//! nothing else of the table.
//!
//! A statement needs the privilege on the table, or on every column it reads
//! or writes. `SELECT *` reads every column. A column the gate cannot
//! attribute to a table is refused, never waved through.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::many_single_char_names)]

use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::{EmbeddedDatabase, Error, Value};

fn login(db: &EmbeddedDatabase, role: &str) -> SessionId {
    db.create_session(role, IsolationLevel::ReadCommitted).unwrap()
}

/// Rows returned by `sql`, on the text route when `params` is false and on the
/// params route when it is true.
fn rows(db: &EmbeddedDatabase, session: SessionId, params: bool, sql: &str) -> Result<usize, Error> {
    if params {
        db.query_params_for_session(session, sql, &[]).map(|rows| rows.len())
    } else {
        db.query_with_columns_for_session(session, sql).map(|(rows, _)| rows.len())
    }
}

fn run(db: &EmbeddedDatabase, session: SessionId, params: bool, sql: &str) -> Result<u64, Error> {
    if params {
        db.execute_params_for_session(session, sql, &[])
    } else {
        db.execute_for_session(session, sql)
    }
}

fn assert_denied<T: std::fmt::Debug>(result: Result<T, Error>, context: &str) {
    match result {
        Ok(value) => panic!("{context}: must be refused, but returned {value:?}"),
        Err(error) => {
            let text = error.to_string();
            assert!(
                text.contains("permission denied for table people"),
                "{context}: expected a refusal for table people, got: {text}"
            );
        }
    }
}

fn setup() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE people (id INT PRIMARY KEY, name TEXT, salary INT, note TEXT)")
        .unwrap();
    db.execute("INSERT INTO people VALUES (1, 'ann', 100, 'a'), (2, 'bob', 200, 'b')")
        .unwrap();
    db.execute("CREATE TABLE teams (id INT PRIMARY KEY, lead INT)").unwrap();
    db.execute("INSERT INTO teams VALUES (1, 1)").unwrap();
    db.execute("CREATE ROLE clerk LOGIN").unwrap();
    db
}

#[test]
fn a_column_grant_allows_only_that_column() {
    let db = setup();
    db.execute("GRANT SELECT (id, name) ON people TO clerk").unwrap();
    let clerk = login(&db, "clerk");
    for params in [false, true] {
        assert_eq!(rows(&db, clerk, params, "SELECT id, name FROM people").unwrap(), 2);
        assert_eq!(rows(&db, clerk, params, "SELECT name FROM people WHERE id = 1").unwrap(), 1);
        assert_eq!(
            rows(&db, clerk, params, "SELECT p.name FROM people AS p ORDER BY p.id").unwrap(),
            2
        );
        assert_eq!(rows(&db, clerk, params, "SELECT count(*) FROM people").unwrap(), 1);
        assert_eq!(
            rows(&db, clerk, params, "SELECT name, count(*) FROM people GROUP BY name").unwrap(),
            2
        );

        assert_denied(rows(&db, clerk, params, "SELECT * FROM people"), "SELECT *");
        assert_denied(rows(&db, clerk, params, "SELECT salary FROM people"), "an ungranted column");
        assert_denied(
            rows(&db, clerk, params, "SELECT name FROM people WHERE salary > 150"),
            "an ungranted column in WHERE",
        );
        assert_denied(
            rows(&db, clerk, params, "SELECT name FROM people ORDER BY salary"),
            "an ungranted column in ORDER BY",
        );
        assert_denied(
            rows(&db, clerk, params, "SELECT name, sum(salary) FROM people GROUP BY name"),
            "an ungranted column in an aggregate",
        );
        assert_denied(
            rows(&db, clerk, params, "SELECT name FROM people GROUP BY name HAVING max(salary) > 1"),
            "an ungranted column in HAVING",
        );
        assert_denied(
            rows(
                &db,
                clerk,
                params,
                "SELECT name, rank() OVER (ORDER BY salary) FROM people",
            ),
            "an ungranted column in a window",
        );
        assert_denied(
            rows(
                &db,
                clerk,
                params,
                "SELECT name, CASE WHEN salary > 1 THEN 'x' ELSE 'y' END FROM people",
            ),
            "an ungranted column in CASE",
        );
        assert_denied(
            rows(&db, clerk, params, "SELECT d.name FROM (SELECT name, salary FROM people) AS d"),
            "an ungranted column inside a derived table",
        );
        assert_denied(
            rows(
                &db,
                clerk,
                params,
                "WITH c AS (SELECT name, note FROM people) SELECT name FROM c",
            ),
            "an ungranted column inside a CTE",
        );
        assert_denied(
            rows(&db, clerk, params, "SELECT 1 WHERE EXISTS (SELECT 1 FROM people WHERE salary > 0)"),
            "an ungranted column inside a subquery",
        );
    }
}

#[test]
fn a_join_charges_each_column_to_its_own_table() {
    let db = setup();
    db.execute("GRANT SELECT (id, name) ON people TO clerk").unwrap();
    db.execute("GRANT SELECT ON teams TO clerk").unwrap();
    let clerk = login(&db, "clerk");
    for params in [false, true] {
        assert_eq!(
            rows(
                &db,
                clerk,
                params,
                "SELECT p.name, t.id FROM people p JOIN teams t ON t.lead = p.id"
            )
            .unwrap(),
            1
        );
        assert_denied(
            rows(
                &db,
                clerk,
                params,
                "SELECT p.name FROM people p JOIN teams t ON t.lead = p.salary",
            ),
            "an ungranted column in a join condition",
        );
        assert_denied(
            rows(
                &db,
                clerk,
                params,
                "SELECT t.id, (SELECT salary FROM people WHERE people.id = t.lead) FROM teams t",
            ),
            "an ungranted column in a correlated subquery",
        );
    }
}

#[test]
fn insert_and_update_column_grants_cover_the_columns_written() {
    let db = setup();
    db.execute("GRANT INSERT (id, name) ON people TO clerk").unwrap();
    db.execute("GRANT UPDATE (note) ON people TO clerk").unwrap();
    let clerk = login(&db, "clerk");
    for (n, params) in [false, true].into_iter().enumerate() {
        run(&db, clerk, params, &format!("INSERT INTO people (id, name) VALUES ({}, 'x')", 10 + n)).unwrap();
        assert_denied(
            run(&db, clerk, params, "INSERT INTO people VALUES (20, 'y', 1, 'n')"),
            "INSERT with no column list writes every column",
        );
        assert_denied(
            run(&db, clerk, params, "INSERT INTO people (id, salary) VALUES (21, 5)"),
            "INSERT into an ungranted column",
        );
        run(&db, clerk, params, "UPDATE people SET note = 'z'").unwrap();
        assert_denied(
            run(&db, clerk, params, "UPDATE people SET salary = 1"),
            "UPDATE of an ungranted column",
        );
        assert_denied(
            run(&db, clerk, params, "UPDATE people SET note = 'q' WHERE salary > 0"),
            "UPDATE whose WHERE reads a column without SELECT",
        );
        assert_denied(
            run(&db, clerk, params, "UPDATE people SET note = name"),
            "UPDATE whose value reads a column without SELECT",
        );
        assert_denied(
            run(&db, clerk, params, "DELETE FROM people"),
            "DELETE has no column form",
        );
    }
    assert_eq!(
        db.query("SELECT count(*) FROM people WHERE salary = 1", &[]).unwrap()[0].values[0],
        Value::Int8(0)
    );
}

#[test]
fn returning_and_on_conflict_need_select_on_their_columns() {
    let db = setup();
    db.execute("GRANT INSERT ON people TO clerk").unwrap();
    db.execute("GRANT SELECT (id) ON people TO clerk").unwrap();
    db.execute("GRANT UPDATE (name) ON people TO clerk").unwrap();
    let clerk = login(&db, "clerk");
    let (_, returned) = db
        .execute_returning_for_session(clerk, "INSERT INTO people VALUES (30, 'r', 1, 'n') RETURNING id")
        .unwrap();
    assert_eq!(returned.len(), 1);
    assert_denied(
        db.execute_returning_for_session(clerk, "INSERT INTO people VALUES (31, 'r', 1, 'n') RETURNING salary"),
        "RETURNING an ungranted column",
    );
    assert_denied(
        db.execute_returning_for_session(clerk, "INSERT INTO people VALUES (32, 'r', 1, 'n') RETURNING *"),
        "RETURNING *",
    );
    db.execute_for_session(
        clerk,
        "INSERT INTO people VALUES (1, 'again', 1, 'n') ON CONFLICT (id) DO UPDATE SET name = 'upserted'",
    )
    .unwrap();
    assert_denied(
        db.execute_for_session(
            clerk,
            "INSERT INTO people VALUES (1, 'again', 1, 'n') ON CONFLICT (id) DO UPDATE SET salary = 0",
        ),
        "ON CONFLICT DO UPDATE of an ungranted column",
    );
    assert_eq!(
        db.query("SELECT name, salary FROM people WHERE id = 1", &[]).unwrap()[0].values,
        vec![Value::String("upserted".into()), Value::Int4(100)]
    );
}

#[test]
fn column_grants_add_up_across_grants_and_a_table_grant_covers_all() {
    let db = setup();
    db.execute("GRANT SELECT (id) ON people TO clerk").unwrap();
    db.execute("GRANT SELECT (name) ON people TO PUBLIC").unwrap();
    let clerk = login(&db, "clerk");
    assert_eq!(rows(&db, clerk, false, "SELECT id, name FROM people").unwrap(), 2);
    assert_denied(rows(&db, clerk, false, "SELECT id, salary FROM people"), "still ungranted");
    db.execute("GRANT SELECT ON people TO clerk").unwrap();
    assert_eq!(rows(&db, clerk, false, "SELECT * FROM people").unwrap(), 2);
}

#[test]
fn revoking_a_column_grant_takes_the_column_away() {
    let db = setup();
    db.execute("GRANT SELECT (id, name) ON people TO clerk").unwrap();
    let clerk = login(&db, "clerk");
    for params in [false, true] {
        assert_eq!(rows(&db, clerk, params, "SELECT id, name FROM people").unwrap(), 2);
    }
    db.execute("REVOKE SELECT (name) ON people FROM clerk").unwrap();
    for params in [false, true] {
        assert_denied(rows(&db, clerk, params, "SELECT id, name FROM people"), "after the column REVOKE");
        assert_eq!(rows(&db, clerk, params, "SELECT id FROM people").unwrap(), 2);
    }
    // A table-level REVOKE removes the column grants of that privilege too.
    db.execute("REVOKE SELECT ON people FROM clerk").unwrap();
    assert_denied(rows(&db, clerk, false, "SELECT id FROM people"), "after the table REVOKE");
}

#[test]
fn a_column_grant_must_name_real_columns_and_a_column_privilege() {
    let db = setup();
    let unknown = db.execute("GRANT SELECT (nope) ON people TO clerk").unwrap_err().to_string();
    assert!(unknown.contains("nope") && unknown.contains("does not exist"), "{unknown}");
    let wrong = db.execute("GRANT DELETE (id) ON people TO clerk").unwrap_err().to_string();
    assert!(wrong.contains("cannot be granted on columns"), "{wrong}");
}

#[test]
fn dropping_or_renaming_a_column_keeps_its_grants_in_step() {
    let db = setup();
    db.execute("GRANT SELECT (id, name, note) ON people TO clerk").unwrap();
    let clerk = login(&db, "clerk");
    db.execute("ALTER TABLE people RENAME COLUMN name TO full_name").unwrap();
    assert_eq!(rows(&db, clerk, false, "SELECT id, full_name FROM people").unwrap(), 2);
    db.execute("ALTER TABLE people DROP COLUMN note").unwrap();
    db.execute("ALTER TABLE people ADD COLUMN note TEXT").unwrap();
    assert_denied(
        rows(&db, clerk, false, "SELECT note FROM people"),
        "a new column that reuses a dropped column's name starts with no grants",
    );
}

#[test]
fn a_column_grant_is_reported_by_has_column_privilege() {
    let db = setup();
    db.execute("GRANT SELECT (name) ON people TO clerk").unwrap();
    let answer = |sql: &str| db.query(sql, &[]).unwrap()[0].values[0].clone();
    assert_eq!(
        answer("SELECT has_column_privilege('clerk', 'people', 'name', 'SELECT')"),
        Value::Boolean(true)
    );
    assert_eq!(
        answer("SELECT has_column_privilege('clerk', 'people', 'salary', 'SELECT')"),
        Value::Boolean(false)
    );
    assert_eq!(
        answer("SELECT has_any_column_privilege('clerk', 'people', 'SELECT')"),
        Value::Boolean(true)
    );
    assert_eq!(
        answer("SELECT has_table_privilege('clerk', 'people', 'SELECT')"),
        Value::Boolean(false)
    );
}
