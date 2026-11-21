//! Privileges on what is not a table: sequences, functions and procedures,
//! schemas, databases. Also `GRANT … ON ALL TABLES IN SCHEMA`, the
//! `has_*_privilege` functions, and the statements that are refused outright.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::many_single_char_names)]

use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::{EmbeddedDatabase, Error, Value};

fn login(db: &EmbeddedDatabase, role: &str) -> SessionId {
    db.create_session(role, IsolationLevel::ReadCommitted).unwrap()
}

fn run(db: &EmbeddedDatabase, session: SessionId, params: bool, sql: &str) -> Result<u64, Error> {
    if params {
        db.execute_params_for_session(session, sql, &[])
    } else {
        db.execute_for_session(session, sql)
    }
}

fn count(db: &EmbeddedDatabase, session: SessionId, params: bool, sql: &str) -> Result<usize, Error> {
    if params {
        db.query_params_for_session(session, sql, &[]).map(|rows| rows.len())
    } else {
        db.query_with_columns_for_session(session, sql).map(|(rows, _)| rows.len())
    }
}

fn assert_error<T: std::fmt::Debug>(result: Result<T, Error>, expected: &str, context: &str) {
    match result {
        Ok(value) => panic!("{context}: must be refused with `{expected}`, but returned {value:?}"),
        Err(error) => {
            let text = error.to_string();
            assert!(text.contains(expected), "{context}: expected `{expected}`, got: {text}");
        }
    }
}

fn answer(db: &EmbeddedDatabase, sql: &str) -> Value {
    db.query(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"))[0].values[0].clone()
}

fn setup() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE ROLE app LOGIN").unwrap();
    db.execute("CREATE ROLE other LOGIN").unwrap();
    db
}

// ---------------------------------------------------------------------------
// Sequences
// ---------------------------------------------------------------------------

#[test]
fn sequence_functions_need_the_sequence_privileges() {
    for params in [false, true] {
        let db = setup();
        db.execute("CREATE SEQUENCE ticket_no").unwrap();
        let app = login(&db, "app");
        assert_error(
            count(&db, app, params, "SELECT nextval('ticket_no')"),
            "permission denied for sequence ticket_no",
            "nextval without USAGE",
        );
        assert_error(
            count(&db, app, params, "SELECT setval('ticket_no', 100)"),
            "permission denied for sequence ticket_no",
            "setval without UPDATE",
        );
        db.execute("GRANT USAGE ON SEQUENCE ticket_no TO app").unwrap();
        assert_eq!(count(&db, app, params, "SELECT nextval('ticket_no')").unwrap(), 1);
        assert_eq!(count(&db, app, params, "SELECT currval('ticket_no')").unwrap(), 1);
        assert_error(
            count(&db, app, params, "SELECT setval('ticket_no', 100)"),
            "permission denied for sequence ticket_no",
            "USAGE is not UPDATE",
        );
        db.execute("GRANT UPDATE ON SEQUENCE ticket_no TO app").unwrap();
        assert_eq!(count(&db, app, params, "SELECT setval('ticket_no', 100)").unwrap(), 1);
        db.execute("REVOKE ALL ON SEQUENCE ticket_no FROM app").unwrap();
        assert_error(
            count(&db, app, params, "SELECT nextval('ticket_no')"),
            "permission denied for sequence ticket_no",
            "after REVOKE ALL",
        );
        // Sequence DDL belongs to the owner.
        assert_error(
            run(&db, app, params, "DROP SEQUENCE ticket_no"),
            "must be owner of sequence ticket_no",
            "a non-owner dropping a sequence",
        );
        assert_error(
            run(&db, app, params, "ALTER SEQUENCE ticket_no RESTART WITH 5"),
            "must be owner of sequence ticket_no",
            "a non-owner altering a sequence",
        );
    }
}

#[test]
fn a_sequence_used_as_an_insert_value_is_checked_for_the_inserting_role() {
    let db = setup();
    db.execute("CREATE SEQUENCE ticket_no").unwrap();
    db.execute("CREATE TABLE tickets (no INT PRIMARY KEY, what TEXT)").unwrap();
    db.execute("GRANT INSERT ON tickets TO app").unwrap();
    let app = login(&db, "app");
    for params in [false, true] {
        assert_error(
            run(&db, app, params, "INSERT INTO tickets VALUES (nextval('ticket_no'), 'x')"),
            "permission denied for sequence ticket_no",
            "INSERT … nextval without USAGE",
        );
    }
    assert_eq!(answer(&db, "SELECT count(*) FROM tickets"), Value::Int8(0));
    db.execute("GRANT USAGE ON SEQUENCE ticket_no TO app").unwrap();
    for params in [false, true] {
        run(&db, app, params, "INSERT INTO tickets VALUES (nextval('ticket_no'), 'x')").unwrap();
    }
    assert_eq!(answer(&db, "SELECT count(*) FROM tickets"), Value::Int8(2));
}

#[test]
fn the_creator_owns_the_sequence() {
    let db = setup();
    db.execute("GRANT CREATE ON SCHEMA public TO app").unwrap();
    let app = login(&db, "app");
    let other = login(&db, "other");
    run(&db, app, false, "CREATE SEQUENCE mine").unwrap();
    assert_eq!(count(&db, app, false, "SELECT nextval('mine')").unwrap(), 1);
    assert_error(
        count(&db, other, false, "SELECT nextval('mine')"),
        "permission denied for sequence mine",
        "another role",
    );
    run(&db, app, false, "GRANT USAGE ON SEQUENCE mine TO other").unwrap();
    assert_eq!(count(&db, other, false, "SELECT nextval('mine')").unwrap(), 1);
    run(&db, app, false, "DROP SEQUENCE mine").unwrap();
}

// ---------------------------------------------------------------------------
// Functions and procedures
// ---------------------------------------------------------------------------

#[test]
fn execute_is_publics_by_default_and_can_be_taken_away() {
    for params in [false, true] {
        let db = setup();
        db.execute("CREATE FUNCTION dbl(x INT) RETURNS INT AS $$ SELECT $1 * 2 $$ LANGUAGE sql")
            .unwrap();
        let app = login(&db, "app");
        let other = login(&db, "other");
        // PostgreSQL's default: every role may call a function.
        assert_eq!(count(&db, app, params, "SELECT dbl(2)").unwrap(), 1);
        db.execute("REVOKE EXECUTE ON FUNCTION dbl FROM PUBLIC").unwrap();
        for session in [app, other] {
            assert_error(
                count(&db, session, params, "SELECT dbl(2)"),
                "permission denied for function dbl",
                "after REVOKE … FROM PUBLIC",
            );
        }
        db.execute("GRANT EXECUTE ON FUNCTION dbl(INT) TO app").unwrap();
        assert_eq!(count(&db, app, params, "SELECT dbl(2)").unwrap(), 1);
        assert_error(
            count(&db, other, params, "SELECT dbl(2)"),
            "permission denied for function dbl",
            "a role EXECUTE was not granted to",
        );
        db.execute("GRANT EXECUTE ON FUNCTION dbl TO PUBLIC").unwrap();
        assert_eq!(count(&db, other, params, "SELECT dbl(2)").unwrap(), 1);
        assert_error(
            run(&db, app, params, "DROP FUNCTION dbl"),
            "must be owner of function dbl",
            "a non-owner dropping a function",
        );
        assert_error(
            run(
                &db,
                app,
                params,
                "CREATE FUNCTION sneaky() RETURNS INT AS $$ SELECT 1 $$ LANGUAGE sql",
            ),
            "permission denied for schema public",
            "creating a function without CREATE on the schema",
        );
    }
}

#[test]
fn a_routine_runs_with_the_callers_rights() {
    let db = setup();
    db.execute("CREATE TABLE audit (id INT PRIMARY KEY, what TEXT)").unwrap();
    db.execute("CREATE PROCEDURE log_one() LANGUAGE sql AS $$INSERT INTO audit VALUES (1, 'called')$$")
        .unwrap();
    let app = login(&db, "app");
    for params in [false, true] {
        // EXECUTE is PUBLIC's, but the body writes a table `app` may not write.
        assert_error(
            run(&db, app, params, "CALL log_one()"),
            "permission denied for table audit",
            "the body's statement is the caller's statement",
        );
    }
    assert_eq!(answer(&db, "SELECT count(*) FROM audit"), Value::Int8(0));
    db.execute("GRANT INSERT ON audit TO app").unwrap();
    run(&db, app, false, "CALL log_one()").unwrap();
    assert_eq!(answer(&db, "SELECT count(*) FROM audit"), Value::Int8(1));
    db.execute("REVOKE EXECUTE ON PROCEDURE log_one FROM PUBLIC").unwrap();
    for params in [false, true] {
        assert_error(
            run(&db, app, params, "CALL log_one()"),
            "permission denied for procedure log_one",
            "CALL without EXECUTE",
        );
    }
}

// ---------------------------------------------------------------------------
// Schemas
// ---------------------------------------------------------------------------

#[test]
fn a_schema_needs_usage_to_reach_what_is_in_it_and_create_to_add_to_it() {
    for params in [false, true] {
        let db = setup();
        db.execute("CREATE SCHEMA sales").unwrap();
        db.execute("CREATE TABLE sales.deals (id INT PRIMARY KEY)").unwrap();
        db.execute("GRANT SELECT ON sales.deals TO app").unwrap();
        let app = login(&db, "app");
        assert_error(
            count(&db, app, params, "SELECT * FROM sales.deals"),
            "permission denied for schema sales",
            "a table grant without USAGE on its schema",
        );
        db.execute("GRANT USAGE ON SCHEMA sales TO app").unwrap();
        assert_eq!(count(&db, app, params, "SELECT * FROM sales.deals").unwrap(), 0);
        assert_error(
            run(&db, app, params, "CREATE TABLE sales.mine (id INT PRIMARY KEY)"),
            "permission denied for schema sales",
            "USAGE is not CREATE",
        );
        db.execute("GRANT CREATE ON SCHEMA sales TO app").unwrap();
        run(&db, app, params, "CREATE TABLE sales.mine (id INT PRIMARY KEY)").unwrap();
        run(&db, app, params, "INSERT INTO sales.mine VALUES (1)").unwrap();
        db.execute("REVOKE USAGE ON SCHEMA sales FROM app").unwrap();
        assert_error(
            count(&db, app, params, "SELECT * FROM sales.mine"),
            "permission denied for schema sales",
            "even the owner of a table needs USAGE on its schema",
        );
    }
}

#[test]
fn creating_and_dropping_a_schema() {
    for params in [false, true] {
        let db = setup();
        let app = login(&db, "app");
        let other = login(&db, "other");
        assert_error(
            run(&db, app, params, "CREATE SCHEMA mine"),
            "permission denied for database heliosdb",
            "CREATE SCHEMA without CREATE on the database",
        );
        db.execute("GRANT CREATE ON DATABASE heliosdb TO app").unwrap();
        run(&db, app, params, "CREATE SCHEMA mine").unwrap();
        // The creator owns it: it may use it, create in it, and grant on it.
        run(&db, app, params, "CREATE TABLE mine.t (id INT PRIMARY KEY)").unwrap();
        assert_error(
            count(&db, other, params, "SELECT * FROM mine.t"),
            "permission denied for schema mine",
            "another role",
        );
        run(&db, app, params, "GRANT USAGE ON SCHEMA mine TO other").unwrap();
        run(&db, app, params, "GRANT SELECT ON mine.t TO other").unwrap();
        assert_eq!(count(&db, other, params, "SELECT * FROM mine.t").unwrap(), 0);
        assert_error(
            run(&db, other, params, "DROP SCHEMA mine CASCADE"),
            "must be owner of schema mine",
            "a non-owner dropping a schema",
        );
        run(&db, app, params, "DROP SCHEMA mine CASCADE").unwrap();
        // A schema re-created under the name starts with no grants.
        db.execute("CREATE SCHEMA mine").unwrap();
        db.execute("CREATE TABLE mine.t (id INT PRIMARY KEY)").unwrap();
        assert_error(
            count(&db, other, params, "SELECT * FROM mine.t"),
            "permission denied for schema mine",
            "grants on the dropped schema are gone",
        );
    }
}

#[test]
fn public_usage_can_be_revoked() {
    let db = setup();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    db.execute("GRANT SELECT ON t TO app").unwrap();
    let app = login(&db, "app");
    assert_eq!(count(&db, app, false, "SELECT * FROM t").unwrap(), 0);
    db.execute("REVOKE USAGE ON SCHEMA public FROM PUBLIC").unwrap();
    assert_error(
        count(&db, app, false, "SELECT * FROM t"),
        "permission denied for schema public",
        "after REVOKE USAGE … FROM PUBLIC",
    );
    db.execute("GRANT USAGE ON SCHEMA public TO app").unwrap();
    assert_eq!(count(&db, app, false, "SELECT * FROM t").unwrap(), 0);
}

#[test]
fn grant_on_all_tables_in_schema_names_every_table_there() {
    let db = setup();
    db.execute("CREATE TABLE a (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE b (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE SCHEMA sales").unwrap();
    db.execute("CREATE TABLE sales.c (id INT PRIMARY KEY)").unwrap();
    db.execute("GRANT USAGE ON SCHEMA sales TO app").unwrap();
    db.execute("GRANT SELECT ON ALL TABLES IN SCHEMA public TO app").unwrap();
    let app = login(&db, "app");
    for table in ["a", "b"] {
        assert_eq!(count(&db, app, false, &format!("SELECT * FROM {table}")).unwrap(), 0);
    }
    assert_error(
        count(&db, app, false, "SELECT * FROM sales.c"),
        "permission denied for table c",
        "a table of another schema",
    );
    // A table created afterwards is not covered: the grant named what existed.
    db.execute("CREATE TABLE later (id INT PRIMARY KEY)").unwrap();
    assert_error(
        count(&db, app, false, "SELECT * FROM later"),
        "permission denied for table later",
        "a table created after the grant",
    );
    db.execute("REVOKE SELECT ON ALL TABLES IN SCHEMA public FROM app").unwrap();
    assert_error(
        count(&db, app, false, "SELECT * FROM a"),
        "permission denied for table a",
        "after the REVOKE",
    );
    assert_error(
        db.execute("GRANT SELECT ON ALL TABLES IN SCHEMA nowhere TO app"),
        "schema \"nowhere\" does not exist",
        "an unknown schema",
    );
}

// ---------------------------------------------------------------------------
// The inquiry functions
// ---------------------------------------------------------------------------

#[test]
fn the_has_privilege_functions_answer_what_the_gate_would() {
    let db = setup();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("CREATE SEQUENCE s").unwrap();
    db.execute("CREATE SCHEMA sales").unwrap();
    db.execute("CREATE FUNCTION one() RETURNS INT AS $$ SELECT 1 $$ LANGUAGE sql")
        .unwrap();
    db.execute("GRANT SELECT ON t TO app WITH GRANT OPTION").unwrap();
    db.execute("GRANT USAGE ON SEQUENCE s TO app").unwrap();
    for (sql, expected) in [
        ("SELECT has_table_privilege('app', 't', 'SELECT')", true),
        ("SELECT has_table_privilege('app', 't', 'select')", true),
        ("SELECT has_table_privilege('app', 'public.t', 'SELECT')", true),
        ("SELECT has_table_privilege('app', 't', 'INSERT')", false),
        ("SELECT has_table_privilege('app', 't', 'INSERT, SELECT')", true),
        ("SELECT has_table_privilege('app', 't', 'SELECT WITH GRANT OPTION')", true),
        ("SELECT has_table_privilege('other', 't', 'SELECT')", false),
        ("SELECT has_table_privilege('postgres', 't', 'DELETE')", true),
        ("SELECT has_sequence_privilege('app', 's', 'USAGE')", true),
        ("SELECT has_sequence_privilege('app', 's', 'UPDATE')", false),
        ("SELECT has_schema_privilege('app', 'public', 'USAGE')", true),
        ("SELECT has_schema_privilege('app', 'public', 'CREATE')", false),
        ("SELECT has_schema_privilege('app', 'sales', 'USAGE')", false),
        ("SELECT has_function_privilege('app', 'one()', 'EXECUTE')", true),
        ("SELECT has_database_privilege('app', 'heliosdb', 'CONNECT')", true),
        ("SELECT has_database_privilege('app', 'heliosdb', 'CREATE')", false),
        // Asked without a role name on the embedded API: nothing restricts it.
        ("SELECT has_table_privilege('t', 'DELETE')", true),
    ] {
        assert_eq!(answer(&db, sql), Value::Boolean(expected), "{sql}");
    }
    // Asked by a role about itself.
    let app = login(&db, "app");
    let (rows, _) = db
        .query_with_columns_for_session(
            app,
            "SELECT has_table_privilege('t', 'SELECT'), has_table_privilege('t', 'UPDATE')",
        )
        .unwrap();
    assert_eq!(rows[0].values, vec![Value::Boolean(true), Value::Boolean(false)]);
    assert_error(
        db.query("SELECT has_table_privilege('nobody_here', 't', 'SELECT')", &[]),
        "role \"nobody_here\" does not exist",
        "an unknown role",
    );
    assert_error(
        db.query("SELECT has_table_privilege('app', 't', 'FLY')", &[]),
        "unrecognized privilege type",
        "an unknown privilege",
    );
}

// ---------------------------------------------------------------------------
// Refused outright
// ---------------------------------------------------------------------------

#[test]
fn what_is_not_implemented_says_so() {
    let db = setup();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    for sql in [
        "ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO app",
        "GRANT USAGE ON TYPE mood TO app",
        "GRANT USAGE ON LANGUAGE plpgsql TO app",
    ] {
        assert_error(db.execute(sql), "is not supported", sql);
        assert_error(db.execute_params(sql, &[]), "is not supported", sql);
    }
    for (sql, expected) in [
        ("GRANT USAGE ON t TO app", "privilege USAGE is not valid for a table"),
        ("GRANT SELECT ON SCHEMA public TO app", "privilege SELECT is not valid for a schema"),
        ("GRANT SELECT ON t TO nobody_here", "role \"nobody_here\" does not exist"),
        ("GRANT SELECT ON nothing_here TO app", "does not exist"),
    ] {
        assert_error(db.execute(sql), expected, sql);
        assert_error(db.execute_params(sql, &[]), expected, sql);
    }
}

#[test]
fn utilities_that_never_plan_are_for_superusers() {
    let db = setup();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    db.execute("GRANT ALL ON t TO app").unwrap();
    let app = login(&db, "app");
    for sql in ["VACUUM t", "VACUUM", "REINDEX TABLE t", "CHECKPOINT"] {
        assert_error(run(&db, app, false, sql), "permission denied to run", sql);
        assert_error(run(&db, app, true, sql), "permission denied to run", sql);
    }
    assert_error(
        run(&db, app, false, "CREATE DATABASE BRANCH scratch"),
        "permission denied to create a branch",
        "a branch",
    );
    assert_error(
        run(&db, app, false, "CREATE DATABASE elsewhere"),
        "permission denied to create database",
        "CREATE DATABASE without CREATEDB",
    );
}
