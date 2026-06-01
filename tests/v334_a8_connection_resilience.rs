//! Regression for checklist item **A8** — "Connection wedges after a
//! constraint error; daemon stops accepting new connections after ~100 errors."
//!
//! Verdict (v3.34.0 / 37238cc): PARTIALLY pre-fixed. The acceptor was already
//! sound — the per-connection semaphore permit is held in the spawned task
//! (`_permit`) and released on connection close (RAII), see
//! `src/protocol/postgres/server.rs:189-215`. BUT the EXTENDED-query path had a
//! real recovery bug: an Execute-time constraint error sent `ReadyForQuery`
//! before the client's `Sync`, which closes/wedges the driver connection. Fixed
//! in `src/protocol/postgres/handler.rs` by deferring `ReadyForQuery` until the
//! client `Sync` (discarding messages in between), the PostgreSQL-shaped
//! extended-query error recovery.
//!
//! Acceptance criteria covered:
//!  1. A connection that receives a constraint error accepts the next query —
//!     proven on BOTH the simple-query and the extended/prepared paths.
//!  2. The daemon keeps accepting new connections after repeated constraint
//!     errors (a leaked permit would wedge the acceptor after ~`max_connections`).
//!
//! Harness note: the simple-query tests use `batch_execute` / `simple_query`;
//! the extended test drives the error via `execute` (the path real drivers use)
//! and reads back via `simple_query` to avoid the unrelated param-`SELECT`
//! RowDescription stack-overflow latent in this in-process `PgServer` harness
//! (see the `#[ignore]`'d tests in `tests/extended_query_param_select.rs`).

use std::sync::Arc;
use std::time::Duration;

use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::EmbeddedDatabase;
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

async fn setup(max_connections: usize) -> (String, tokio::task::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener);

    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let config = PgServerConfig::with_address(addr).with_max_connections(max_connections);
    let server = PgServer::new(config, db).expect("server");
    let handle = tokio::spawn(async move {
        if let Err(e) = server.serve().await {
            eprintln!("server: {e}");
        }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    (
        format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port()),
        handle,
    )
}

async fn connect(cs: &str) -> Client {
    let (client, conn) = tokio_postgres::connect(cs, NoTls).await.expect("connect");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
}

/// Read `SELECT COUNT(*)` over the simple-query protocol.
async fn count_t(client: &Client) -> i64 {
    let msgs = client
        .simple_query("SELECT COUNT(*) FROM t")
        .await
        .expect("count query (connection wedged?)");
    for m in msgs {
        if let SimpleQueryMessage::Row(row) = m {
            return row.get(0).expect("count column").parse().expect("integer count");
        }
    }
    panic!("COUNT(*) returned no row");
}

/// Acceptance #1: a single connection survives repeated constraint errors and
/// keeps serving subsequent queries.
#[tokio::test]
async fn a8_connection_accepts_next_query_after_constraint_error() {
    let (cs, _h) = setup(100).await;
    let client = connect(&cs).await;

    client
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
        .await
        .expect("create table");
    client
        .batch_execute("INSERT INTO t (id, v) VALUES (1, 'a')")
        .await
        .expect("seed row");

    // Far more than the historical ~100-error wedge threshold, all on ONE
    // long-lived connection.
    for i in 0..150 {
        let dup = client.batch_execute("INSERT INTO t (id, v) VALUES (1, 'dup')").await;
        assert!(
            dup.is_err(),
            "iteration {i}: duplicate PK insert must raise a constraint error"
        );

        // The connection must still answer the very next query.
        assert_eq!(
            count_t(&client).await,
            1,
            "iteration {i}: connection wedged after constraint error"
        );
    }
}

/// Acceptance #2: the acceptor keeps taking new connections after many
/// constraint errors across short-lived connections. `max_connections` is set
/// to 5, so a per-connection permit leak would wedge the acceptor well before
/// the loop completes.
#[tokio::test]
async fn a8_daemon_keeps_accepting_after_many_constraint_errors() {
    let (cs, _h) = setup(5).await;

    // Bootstrap the table + a seed row, then close that connection.
    {
        let client = connect(&cs).await;
        client
            .batch_execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
            .await
            .expect("create table");
        client
            .batch_execute("INSERT INTO t (id, v) VALUES (1, 'a')")
            .await
            .expect("seed row");
    }
    tokio::time::sleep(Duration::from_millis(40)).await;

    // 25 cycles = 5x the connection limit. Each opens a fresh connection,
    // triggers a constraint error, then drops it. A leaked permit would make
    // `connect` fail once 5 permits are exhausted.
    for i in 0..25 {
        let client = connect(&cs).await;
        let dup = client.batch_execute("INSERT INTO t (id, v) VALUES (1, 'dup')").await;
        assert!(
            dup.is_err(),
            "cycle {i}: duplicate PK insert must raise a constraint error"
        );
        drop(client);
        // Give the server task time to observe the closed socket and release
        // the RAII permit before the next cycle.
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // The acceptor must still be healthy and serving.
    let client = connect(&cs).await;
    assert_eq!(
        count_t(&client).await,
        1,
        "acceptor stopped serving after repeated constraint errors"
    );
}

/// Acceptance #1 over the EXTENDED (prepared) query protocol — the path real
/// drivers (`tokio_postgres::execute`, asyncpg, psycopg) actually use, and
/// where the recovery gap lives: an Execute-time error must be followed by
/// `ErrorResponse` → discard-until-`Sync` → `ReadyForQuery`. Sending
/// `ReadyForQuery` early wedges/closes the driver connection.
///
/// Reads are issued over the simple-query protocol to avoid the unrelated
/// param-`SELECT` RowDescription stack-overflow in this in-process harness.
#[tokio::test]
async fn a8_extended_protocol_recovers_after_constraint_error() {
    let (cs, _h) = setup(100).await;
    let client = connect(&cs).await;

    client
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
        .await
        .expect("create table");
    client
        .batch_execute("INSERT INTO t (id, v) VALUES (1, 'a')")
        .await
        .expect("seed row");

    // Extended/prepared duplicate insert must error WITHOUT closing the conn.
    let dup = client.execute("INSERT INTO t (id, v) VALUES (1, 'dup')", &[]).await;
    assert!(
        dup.is_err(),
        "extended-protocol duplicate PK insert must raise a constraint error"
    );

    // The connection must still answer the next query.
    assert_eq!(
        count_t(&client).await,
        1,
        "connection wedged/closed after extended-protocol constraint error"
    );

    // ...and the extended/prepared path itself must be usable again.
    client
        .execute("INSERT INTO t (id, v) VALUES (2, 'b')", &[])
        .await
        .expect("extended-protocol insert must succeed after error recovery");
    assert_eq!(
        count_t(&client).await,
        2,
        "extended path did not recover after the error"
    );
}
