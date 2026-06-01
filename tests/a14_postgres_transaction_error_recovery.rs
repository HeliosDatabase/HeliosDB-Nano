use heliosdb_nano::{
    protocol::postgres::server::{PgServer, PgServerConfig},
    EmbeddedDatabase,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::timeout;
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

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

async fn simple_scalar(client: &Client, sql: &str) -> String {
    let messages = timeout(QUERY_TIMEOUT, client.simple_query(sql))
        .await
        .expect("query timeout")
        .expect("query");
    messages
        .into_iter()
        .find_map(|message| match message {
            SimpleQueryMessage::Row(row) => row.get(0).map(str::to_string),
            _ => None,
        })
        .expect("scalar row")
}

#[tokio::test]
async fn a14_begin_succeeds_after_autocommit_error_simple() {
    let (conn_string, server_handle) = setup_server().await;
    let client = connect(&conn_string).await;

    client
        .batch_execute(
            "CREATE TABLE a14_auto_simple (id INT PRIMARY KEY, name TEXT);
             INSERT INTO a14_auto_simple VALUES (1, 'seed');",
        )
        .await
        .expect("seed");

    let duplicate = timeout(
        QUERY_TIMEOUT,
        client.batch_execute("INSERT INTO a14_auto_simple VALUES (1, 'dup')"),
    )
    .await
    .expect("duplicate timeout");
    assert!(duplicate.is_err(), "duplicate insert should fail");

    client
        .batch_execute("BEGIN")
        .await
        .expect("BEGIN after autocommit error must start a fresh transaction");
    client
        .batch_execute("INSERT INTO a14_auto_simple VALUES (2, 'fresh'); COMMIT;")
        .await
        .expect("fresh transaction after error");

    let count = simple_scalar(&client, "SELECT COUNT(*) FROM a14_auto_simple").await;
    assert_eq!(count, "2");

    server_handle.abort();
}

#[tokio::test]
async fn a14_begin_succeeds_after_simple_query_error_in_transaction() {
    let (conn_string, server_handle) = setup_server().await;
    let client = connect(&conn_string).await;

    client
        .batch_execute(
            "CREATE TABLE a14_simple (id INT PRIMARY KEY, name TEXT);
             INSERT INTO a14_simple VALUES (1, 'seed');",
        )
        .await
        .expect("seed");

    client.batch_execute("BEGIN").await.expect("begin before error");
    let duplicate = timeout(
        QUERY_TIMEOUT,
        client.batch_execute("INSERT INTO a14_simple VALUES (1, 'dup')"),
    )
    .await
    .expect("duplicate timeout");
    assert!(duplicate.is_err(), "duplicate insert should fail");

    client
        .batch_execute("BEGIN")
        .await
        .expect("BEGIN after an error response must start a fresh transaction");
    client
        .batch_execute("INSERT INTO a14_simple VALUES (2, 'fresh'); COMMIT;")
        .await
        .expect("fresh transaction after error");

    let count = simple_scalar(&client, "SELECT COUNT(*) FROM a14_simple").await;
    assert_eq!(count, "2");

    server_handle.abort();
}

#[tokio::test]
async fn a14_simple_error_keeps_transaction_aborted_until_recovery_command() {
    let (conn_string, server_handle) = setup_server().await;
    let client = connect(&conn_string).await;

    client
        .batch_execute(
            "CREATE TABLE a14_abort_simple (id INT PRIMARY KEY, name TEXT);
             INSERT INTO a14_abort_simple VALUES (1, 'seed');",
        )
        .await
        .expect("seed");

    client.batch_execute("BEGIN").await.expect("begin before error");
    client
        .batch_execute("INSERT INTO a14_abort_simple VALUES (2, 'inside')")
        .await
        .expect("insert before error");
    let duplicate = client
        .batch_execute("INSERT INTO a14_abort_simple VALUES (1, 'dup')")
        .await;
    assert!(duplicate.is_err(), "duplicate insert should fail");

    let blocked = client
        .batch_execute("INSERT INTO a14_abort_simple VALUES (3, 'blocked')")
        .await;
    let blocked_message = format!("{:?}", blocked);
    assert!(
        blocked.is_err() && blocked_message.contains("25P02"),
        "statement after failed transaction should get 25P02, got {blocked_message}"
    );

    client
        .batch_execute("ROLLBACK")
        .await
        .expect("rollback failed transaction");
    let count = simple_scalar(&client, "SELECT COUNT(*) FROM a14_abort_simple").await;
    assert_eq!(
        count, "1",
        "transaction writes before and after the error must not persist"
    );

    server_handle.abort();
}

#[tokio::test]
async fn a14_begin_succeeds_after_autocommit_error_extended() {
    let (conn_string, server_handle) = setup_server().await;
    let client = connect(&conn_string).await;

    client
        .batch_execute(
            "CREATE TABLE a14_auto_extended (id INT PRIMARY KEY, name TEXT);
             INSERT INTO a14_auto_extended VALUES (1, 'seed');",
        )
        .await
        .expect("seed");

    let duplicate = timeout(
        QUERY_TIMEOUT,
        client.execute("INSERT INTO a14_auto_extended VALUES (1, 'dup')", &[]),
    )
    .await
    .expect("duplicate timeout");
    assert!(duplicate.is_err(), "duplicate insert should fail");

    client
        .execute("BEGIN", &[])
        .await
        .expect("extended BEGIN after autocommit error must start a fresh transaction");
    client
        .execute("INSERT INTO a14_auto_extended VALUES (2, 'fresh')", &[])
        .await
        .expect("fresh insert after error");
    client.execute("COMMIT", &[]).await.expect("commit");

    let count = simple_scalar(&client, "SELECT COUNT(*) FROM a14_auto_extended").await;
    assert_eq!(count, "2");

    server_handle.abort();
}

#[tokio::test]
async fn a14_begin_succeeds_after_extended_query_error_in_transaction() {
    let (conn_string, server_handle) = setup_server().await;
    let client = connect(&conn_string).await;

    client
        .batch_execute(
            "CREATE TABLE a14_extended (id INT PRIMARY KEY, name TEXT);
             INSERT INTO a14_extended VALUES (1, 'seed');",
        )
        .await
        .expect("seed");

    client.execute("BEGIN", &[]).await.expect("begin before error");
    let duplicate = timeout(
        QUERY_TIMEOUT,
        client.execute("INSERT INTO a14_extended VALUES (1, 'dup')", &[]),
    )
    .await
    .expect("duplicate timeout");
    assert!(duplicate.is_err(), "duplicate insert should fail");

    client
        .execute("BEGIN", &[])
        .await
        .expect("extended BEGIN after an error response must start a fresh transaction");
    client
        .execute("INSERT INTO a14_extended VALUES (2, 'fresh')", &[])
        .await
        .expect("fresh insert after error");
    client.execute("COMMIT", &[]).await.expect("commit");

    let count = simple_scalar(&client, "SELECT COUNT(*) FROM a14_extended").await;
    assert_eq!(count, "2");

    server_handle.abort();
}

#[tokio::test]
async fn a14_extended_error_keeps_transaction_aborted_until_recovery_command() {
    let (conn_string, server_handle) = setup_server().await;
    let client = connect(&conn_string).await;

    client
        .batch_execute(
            "CREATE TABLE a14_abort_extended (id INT PRIMARY KEY, name TEXT);
             INSERT INTO a14_abort_extended VALUES (1, 'seed');",
        )
        .await
        .expect("seed");

    client.execute("BEGIN", &[]).await.expect("begin before error");
    client
        .execute("INSERT INTO a14_abort_extended VALUES (2, 'inside')", &[])
        .await
        .expect("insert before error");
    let duplicate = client
        .execute("INSERT INTO a14_abort_extended VALUES (1, 'dup')", &[])
        .await;
    assert!(duplicate.is_err(), "duplicate insert should fail");

    let blocked = client
        .execute("INSERT INTO a14_abort_extended VALUES (3, 'blocked')", &[])
        .await;
    let blocked_message = format!("{:?}", blocked);
    assert!(
        blocked.is_err() && blocked_message.contains("25P02"),
        "extended statement after failed transaction should get 25P02, got {blocked_message}"
    );

    client
        .execute("ROLLBACK", &[])
        .await
        .expect("rollback failed transaction");
    let count = simple_scalar(&client, "SELECT COUNT(*) FROM a14_abort_extended").await;
    assert_eq!(
        count, "1",
        "transaction writes before and after the error must not persist"
    );

    server_handle.abort();
}

#[tokio::test]
async fn a14_begin_succeeds_after_query_error_in_transaction_stress() {
    let (conn_string, server_handle) = setup_server().await;
    let client = connect(&conn_string).await;

    client
        .batch_execute(
            "CREATE TABLE a14_stress (id INT PRIMARY KEY, name TEXT);
             INSERT INTO a14_stress VALUES (1, 'seed');",
        )
        .await
        .expect("seed");

    let started = Instant::now();
    for cycle in 0..40 {
        client.batch_execute("BEGIN").await.expect("begin before error");
        let duplicate = timeout(
            QUERY_TIMEOUT,
            client.batch_execute("INSERT INTO a14_stress VALUES (1, 'dup')"),
        )
        .await
        .expect("duplicate timeout");
        assert!(duplicate.is_err(), "duplicate insert should fail on cycle {cycle}");

        client
            .batch_execute("BEGIN")
            .await
            .expect("BEGIN after an error response must start a fresh transaction");
        client
            .batch_execute("ROLLBACK")
            .await
            .expect("cleanup fresh transaction");
    }

    let count = simple_scalar(&client, "SELECT COUNT(*) FROM a14_stress").await;
    assert_eq!(count, "1");

    eprintln!(
        "A14 stress: 40 transaction error recovery cycles completed in {:?}",
        started.elapsed()
    );

    server_handle.abort();
}
