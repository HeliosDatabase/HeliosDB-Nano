//! D4 phase 1: end-to-end SQLSTATE mapping tests over the PostgreSQL wire.
//!
//! Each test provokes a real error through tokio-postgres against a live
//! PgServer and asserts the SQLSTATE the driver sees, plus detail/hint
//! where D4 populates them. The mapping functions themselves are
//! pub(crate) and unit-tested in-module
//! (src/protocol/postgres/handler.rs::sqlstate_mapping_unit_tests and
//! src/protocol/mysql/handler.rs tests for map_error_code).

use heliosdb_nano::{
    protocol::postgres::server::{PgServer, PgServerConfig},
    EmbeddedDatabase,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use tokio_postgres::error::SqlState;
use tokio_postgres::{Client, NoTls};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

async fn setup_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test port");
    let addr = listener.local_addr().expect("test addr");
    drop(listener);

    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let config = PgServerConfig::with_address(addr);
    let server = PgServer::new(config, db).expect("server");
    let handle = tokio::spawn(async move {
        let _ = server.serve().await;
    });

    tokio::time::sleep(Duration::from_millis(150)).await;
    (
        format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port()),
        handle,
    )
}

async fn connect(conn_string: &str) -> Client {
    let (client, connection) = timeout(CONNECT_TIMEOUT, tokio_postgres::connect(conn_string, NoTls))
        .await
        .expect("connect timeout")
        .expect("connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// Run `sql` via the simple-query path and return the resulting DbError.
async fn expect_db_error(client: &Client, sql: &str) -> tokio_postgres::error::DbError {
    let err = timeout(QUERY_TIMEOUT, client.batch_execute(sql))
        .await
        .expect("query timeout")
        .expect_err("statement must fail");
    err.as_db_error().cloned().unwrap_or_else(|| panic!("expected DbError, got: {err:?}"))
}

#[tokio::test]
async fn undefined_table_reports_42p01_on_the_wire() {
    let (conn, server) = setup_server().await;
    let client = connect(&conn).await;

    let db_err = expect_db_error(&client, "SELECT * FROM table_that_is_not_there").await;
    assert_eq!(*db_err.code(), SqlState::UNDEFINED_TABLE, "got: {db_err:?}");
    assert!(
        db_err.detail().unwrap_or_default().contains("table_that_is_not_there"),
        "detail must carry the table name; got: {:?}",
        db_err.detail()
    );
    assert!(db_err.hint().is_some(), "hint must be populated; got: {db_err:?}");

    server.abort();
}

#[tokio::test]
async fn undefined_column_reports_42703_on_the_wire() {
    let (conn, server) = setup_server().await;
    let client = connect(&conn).await;
    client
        .batch_execute(
            // Need a row: projection expressions are evaluated per-row, so
            // an empty table would return zero rows instead of the error.
            "CREATE TABLE sqlstate_cols (id INT PRIMARY KEY); INSERT INTO sqlstate_cols VALUES (1);",
        )
        .await
        .expect("create");

    let db_err = expect_db_error(&client, "SELECT column_that_is_not_there FROM sqlstate_cols").await;
    assert_eq!(*db_err.code(), SqlState::UNDEFINED_COLUMN, "got: {db_err:?}");
    assert!(
        db_err.detail().unwrap_or_default().contains("column_that_is_not_there"),
        "detail must carry the column name; got: {:?}",
        db_err.detail()
    );

    server.abort();
}

#[tokio::test]
async fn duplicate_table_reports_42p07_on_the_wire() {
    let (conn, server) = setup_server().await;
    let client = connect(&conn).await;
    client
        .batch_execute("CREATE TABLE sqlstate_dup (id INT PRIMARY KEY)")
        .await
        .expect("create");

    let db_err = expect_db_error(&client, "CREATE TABLE sqlstate_dup (id INT PRIMARY KEY)").await;
    assert_eq!(*db_err.code(), SqlState::DUPLICATE_TABLE, "got: {db_err:?}");

    server.abort();
}

#[tokio::test]
async fn undefined_function_reports_42883_on_the_wire() {
    let (conn, server) = setup_server().await;
    let client = connect(&conn).await;
    client
        .batch_execute("CREATE TABLE sqlstate_fn (id INT PRIMARY KEY); INSERT INTO sqlstate_fn VALUES (1);")
        .await
        .expect("seed");

    let db_err = expect_db_error(&client, "SELECT not_a_real_function(id) FROM sqlstate_fn").await;
    assert_eq!(*db_err.code(), SqlState::UNDEFINED_FUNCTION, "got: {db_err:?}");

    server.abort();
}

#[tokio::test]
async fn unique_violation_still_reports_23505_on_the_wire() {
    // Regression guard for the pre-D4 constraint mapping.
    let (conn, server) = setup_server().await;
    let client = connect(&conn).await;
    client
        .batch_execute("CREATE TABLE sqlstate_uniq (id INT PRIMARY KEY); INSERT INTO sqlstate_uniq VALUES (1);")
        .await
        .expect("seed");

    let db_err = expect_db_error(&client, "INSERT INTO sqlstate_uniq VALUES (1)").await;
    assert_eq!(*db_err.code(), SqlState::UNIQUE_VIOLATION, "got: {db_err:?}");

    server.abort();
}

#[tokio::test]
async fn serialization_failure_reports_40001_with_retry_hint_on_the_wire() {
    // Real R0.2 first-committer-wins conflict between two wire sessions.
    let (conn, server) = setup_server().await;
    let a = connect(&conn).await;
    let b = connect(&conn).await;

    a.batch_execute(
        "CREATE TABLE sqlstate_ser (id INT PRIMARY KEY, v INT);
         INSERT INTO sqlstate_ser VALUES (1, 100);",
    )
    .await
    .expect("seed");

    a.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ").await.expect("begin a");
    b.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ").await.expect("begin b");

    a.batch_execute("UPDATE sqlstate_ser SET v = 101 WHERE id = 1").await.expect("update a");
    a.batch_execute("COMMIT").await.expect("commit a");

    b.batch_execute("UPDATE sqlstate_ser SET v = 150 WHERE id = 1").await.expect("update b");
    let db_err = expect_db_error(&b, "COMMIT").await;

    assert_eq!(*db_err.code(), SqlState::T_R_SERIALIZATION_FAILURE, "got: {db_err:?}");
    assert!(
        db_err.hint().unwrap_or_default().to_ascii_lowercase().contains("retry"),
        "hint must suggest retrying; got: {:?}",
        db_err.hint()
    );

    server.abort();
}
