//! `INSERT … SELECT` and `CREATE TABLE … AS` with bound parameters.
//!
//! WHAT WAS BROKEN. The params family's `InsertSelect` arm ran its source through an
//! `Executor` it never gave the statement's parameters to, so a `$n` anywhere in the source
//! failed `Parameter $1 not provided. Expected 1 parameters, got 0`, on every driver that
//! binds, in autocommit and in a transaction. Both `CREATE TABLE … AS` paths populate
//! through that arm and handed it `&[]`, and `EXPLAIN ANALYZE` built its own `Executor` the
//! same way. The text family has no parameters, so nothing that used `psql` could see it.
//!
//! Every test asserts the VALUES that landed: a source run with the wrong parameter, or
//! with none and a NULL in its place, would succeed too.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::io::Write as _;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::session::IsolationLevel;
use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};
use tokio_postgres::types::Type;
use tokio_postgres::NoTls;

/// `$1` and `$2` in the select list, `$3` in the source's predicate.
const INSERT_SELECT: &str = "INSERT INTO d (id, code) SELECT id + $1, $2 || CAST(id AS TEXT) FROM src WHERE id > $3";

fn bound() -> [Value; 3] {
    [Value::Int4(100), Value::String("p".to_string()), Value::Int4(1)]
}

/// What [`INSERT_SELECT`] with [`bound`] puts in `d`: not `src` row 1, by `$3`.
fn expected() -> Vec<(i64, String)> {
    vec![(102, "p2".to_string()), (103, "p3".to_string())]
}

fn seeded() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE src (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO src VALUES (1), (2), (3)").unwrap();
    db.execute("CREATE TABLE d (id INT PRIMARY KEY, code TEXT)").unwrap();
    db
}

fn int(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Int2(v)) => i64::from(*v),
        Some(Value::Int4(v)) => i64::from(*v),
        Some(Value::Int8(v)) => *v,
        // A CTAS column whose type could not be inferred from a parameter is TEXT.
        Some(Value::String(v)) => v.parse::<i64>().unwrap_or_else(|e| panic!("not an integer: {v}: {e}")),
        other => panic!("not an integer: {other:?}"),
    }
}

fn id_and_code(rows: &[Tuple]) -> Vec<(i64, String)> {
    rows.iter()
        .map(|row| match row.values.get(1) {
            Some(Value::String(code)) => (int(row.values.first()), code.clone()),
            other => panic!("not text: {other:?}"),
        })
        .collect()
}

fn in_d(db: &EmbeddedDatabase) -> Vec<(i64, String)> {
    id_and_code(&db.query("SELECT id, code FROM d ORDER BY id", &[]).unwrap())
}

/// The first column of `sql`'s rows.
fn ints(db: &EmbeddedDatabase, sql: &str) -> Vec<i64> {
    db.query(sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}`: {e}"))
        .iter()
        .map(|row| int(row.values.first()))
        .collect()
}

#[test]
fn autocommit_binds_the_select_list_and_the_predicate() {
    let db = seeded();
    assert_eq!(db.execute_params(INSERT_SELECT, &bound()).unwrap(), 2);
    assert_eq!(in_d(&db), expected());
}

#[test]
fn the_same_statement_on_the_text_family_agrees() {
    let db = seeded();
    db.execute("INSERT INTO d (id, code) SELECT id + 100, 'p' || CAST(id AS TEXT) FROM src WHERE id > 1")
        .unwrap();
    assert_eq!(in_d(&db), expected());
}

#[test]
fn another_set_of_values_lands_other_values() {
    let db = seeded();
    db.execute_params(INSERT_SELECT, &bound()).unwrap();
    db.execute_params(
        INSERT_SELECT,
        &[Value::Int4(200), Value::String("q".to_string()), Value::Int4(2)],
    )
    .unwrap();
    let mut wanted = expected();
    wanted.push((203, "q3".to_string()));
    assert_eq!(in_d(&db), wanted);
}

#[test]
fn returning_gives_back_what_the_parameters_made() {
    let db = seeded();
    let (count, returned) = db
        .execute_params_returning(&format!("{INSERT_SELECT} RETURNING id, code"), &bound())
        .unwrap();
    assert_eq!(count, 2);
    let mut returned = id_and_code(&returned);
    returned.sort();
    assert_eq!(returned, expected());
    assert_eq!(in_d(&db), expected());
}

#[test]
fn the_query_route_binds_too() {
    let db = seeded();
    db.query_params(INSERT_SELECT, &bound()).unwrap();
    assert_eq!(in_d(&db), expected());
}

#[test]
fn a_session_transaction_commits_the_bound_values() {
    let db = seeded();
    let session = db.create_session("binder", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(session).unwrap();
    assert_eq!(
        db.execute_params_for_session(session, INSERT_SELECT, &bound()).unwrap(),
        2
    );
    assert!(in_d(&db).is_empty(), "not visible outside the transaction yet");
    db.commit_transaction_for_session(session).unwrap();
    assert_eq!(in_d(&db), expected());
}

#[test]
fn a_session_transaction_rolls_the_bound_values_back() {
    let db = seeded();
    let session = db.create_session("binder", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(session).unwrap();
    db.execute_params_for_session(session, INSERT_SELECT, &bound()).unwrap();
    db.rollback_transaction_for_session(session).unwrap();
    assert!(in_d(&db).is_empty());
}

/// The parameters and #100 together: the source sees what the transaction staged, and the
/// predicate on `$3` applies to it.
#[test]
fn a_session_transaction_binds_over_rows_it_staged_itself() {
    let db = seeded();
    let session = db.create_session("binder", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(session).unwrap();
    db.execute_params_for_session(session, "INSERT INTO src VALUES ($1)", &[Value::Int4(4)])
        .unwrap();
    assert_eq!(
        db.execute_params_for_session(session, INSERT_SELECT, &bound()).unwrap(),
        3
    );
    db.commit_transaction_for_session(session).unwrap();
    let mut wanted = expected();
    wanted.push((104, "p4".to_string()));
    assert_eq!(in_d(&db), wanted);
}

#[test]
fn the_handles_own_transaction_binds_commits_and_rolls_back() {
    let db = seeded();
    db.begin().unwrap();
    db.execute_params(INSERT_SELECT, &bound()).unwrap();
    db.rollback().unwrap();
    assert!(in_d(&db).is_empty(), "rolled back");

    db.begin().unwrap();
    assert_eq!(db.execute_params(INSERT_SELECT, &bound()).unwrap(), 2);
    db.commit().unwrap();
    assert_eq!(in_d(&db), expected());
}

/// `PREPARE` and `EXECUTE` are SQL text and still bind: `EXECUTE` hands the arm its values.
#[test]
fn a_prepared_statement_binds_too() {
    let db = seeded();
    db.query(&format!("PREPARE ins AS {INSERT_SELECT}"), &[]).unwrap();
    db.query("EXECUTE ins(100, 'p', 1)", &[]).unwrap();
    assert_eq!(in_d(&db), expected());
}

/// It fails closed: a parameter nobody bound is an error, not a NULL, and nothing lands.
#[test]
fn a_missing_parameter_is_still_refused() {
    let db = seeded();
    let err = db
        .execute_params(INSERT_SELECT, &[Value::Int4(100)])
        .expect_err("$2 and $3 are not bound");
    assert!(err.to_string().to_lowercase().contains("parameter"), "{err}");
    assert!(in_d(&db).is_empty());
}

/// `CREATE TABLE … AS` populates through the same arm: the predicate decides which rows,
/// the select list what is in them.
#[test]
fn create_table_as_binds_its_source() {
    let db = seeded();
    db.execute_params(
        "CREATE TABLE c AS SELECT id, id + $1 AS x FROM src WHERE id >= $2",
        &[Value::Int4(100), Value::Int4(2)],
    )
    .unwrap();
    assert_eq!(ints(&db, "SELECT id FROM c ORDER BY id"), vec![2, 3]);
    let mut x = ints(&db, "SELECT x FROM c");
    x.sort_unstable();
    assert_eq!(x, vec![102, 103]);
}

#[test]
fn create_table_as_on_the_query_route_binds_its_source() {
    let db = seeded();
    db.query_params("CREATE TABLE c AS SELECT id FROM src WHERE id >= $1", &[Value::Int4(3)])
        .unwrap();
    assert_eq!(ints(&db, "SELECT id FROM c"), vec![3]);
}

#[test]
fn create_table_as_on_a_session_binds_its_source() {
    let db = seeded();
    let session = db.create_session("binder", IsolationLevel::ReadCommitted).unwrap();
    db.execute_params_for_session(
        session,
        "CREATE TABLE c AS SELECT id FROM src WHERE id >= $1",
        &[Value::Int4(3)],
    )
    .unwrap();
    assert_eq!(ints(&db, "SELECT id FROM c"), vec![3]);
}

/// On a branch the statement takes its own path to the same arm, and that path has its own
/// `CREATE TABLE … AS`.
#[test]
fn create_table_as_on_a_branch_binds_its_source() {
    let db = seeded();
    db.execute("CREATE BRANCH r AS OF NOW").unwrap();
    db.execute("USE BRANCH r").unwrap();
    db.execute_params(
        "CREATE TABLE c AS SELECT id, id + $1 AS x FROM src WHERE id >= $2",
        &[Value::Int4(100), Value::Int4(2)],
    )
    .unwrap();
    assert_eq!(ints(&db, "SELECT id FROM c ORDER BY id"), vec![2, 3]);
    let mut x = ints(&db, "SELECT x FROM c");
    x.sort_unstable();
    assert_eq!(x, vec![102, 103]);
    db.execute("USE BRANCH main").unwrap();
    assert!(db.query("SELECT id FROM c", &[]).is_err(), "main has no such table");
}

/// `EXPLAIN ANALYZE` runs the query. Its own `Executor` had no parameters, and was given
/// the time elapsed so far as its timeout, so it reported one failure or the other in
/// place of the actual rows, on both families.
#[test]
fn explain_analyze_runs_with_the_parameters() {
    let db = seeded();
    let output = format!(
        "{:?}",
        db.query_params("EXPLAIN ANALYZE SELECT id FROM src WHERE id > $1", &[Value::Int4(1)])
            .unwrap()
    );
    assert!(!output.contains("not provided"), "{output}");
    assert!(!output.contains("Execution Error"), "{output}");
    assert!(output.contains("Actual Rows    : 2"), "{output}");

    let output = format!(
        "{:?}",
        db.query("EXPLAIN ANALYZE SELECT id FROM src WHERE id > 1", &[])
            .unwrap()
    );
    assert!(output.contains("Actual Rows    : 2"), "text family: {output}");
}

/// A driver's round trip: Parse, Bind, Describe, Execute, with the values in binary, in
/// autocommit and in a transaction, and a rollback. `prepare_typed` states the types on the
/// wire, so this does not depend on parameter-type inference.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_extended_protocol_lands_the_bound_values() {
    // A hung server thread would keep the runtime from shutting down: no timeout on an
    // await stops that. Past this the process ends, with a message libtest cannot capture.
    let (_disarm, disarmed) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        if let Err(mpsc::RecvTimeoutError::Timeout) = disarmed.recv_timeout(Duration::from_secs(300)) {
            let _ = writeln!(
                std::io::stderr(),
                "\nwatchdog: the_extended_protocol_lands_the_bound_values still running after 300 s"
            );
            std::process::abort();
        }
    });

    let db = Arc::new(seeded());
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let server = PgServer::new(
        PgServerConfig::with_address(addr).with_max_connections(16),
        Arc::clone(&db),
    )
    .unwrap();
    let listener = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (client, connection) = tokio_postgres::connect(
        &format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port()),
        NoTls,
    )
    .await
    .unwrap();
    let connection = tokio::spawn(async move {
        let _ = connection.await;
    });

    let statement = client
        .prepare_typed(INSERT_SELECT, &[Type::INT4, Type::TEXT, Type::INT4])
        .await
        .unwrap();
    let wire_rows = |rows: Vec<tokio_postgres::Row>| -> Vec<(i64, String)> {
        rows.iter()
            .map(|row| (i64::from(row.get::<_, i32>(0)), row.get::<_, String>(1)))
            .collect()
    };
    const IN_D: &str = "SELECT id, code FROM d ORDER BY id";

    assert_eq!(client.execute(&statement, &[&100_i32, &"p", &1_i32]).await.unwrap(), 2);
    assert_eq!(wire_rows(client.query(IN_D, &[]).await.unwrap()), expected());

    client.simple_query("BEGIN").await.unwrap();
    assert_eq!(client.execute(&statement, &[&200_i32, &"q", &2_i32]).await.unwrap(), 1);
    client.simple_query("ROLLBACK").await.unwrap();
    assert_eq!(wire_rows(client.query(IN_D, &[]).await.unwrap()), expected());

    client.simple_query("BEGIN").await.unwrap();
    assert_eq!(client.execute(&statement, &[&200_i32, &"q", &2_i32]).await.unwrap(), 1);
    client.simple_query("COMMIT").await.unwrap();
    let mut wanted = expected();
    wanted.push((203, "q3".to_string()));
    assert_eq!(wire_rows(client.query(IN_D, &[]).await.unwrap()), wanted);

    drop(client);
    connection.abort();
    listener.abort();
}
