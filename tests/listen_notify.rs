//! GH#40 — `LISTEN` / `UNLISTEN` / `NOTIFY` / `pg_notify()`.
//!
//! Every statement used to be refused ("LISTEN … is not supported", and
//! `Unknown scalar function: pg_notify`), so clients had to poll. These tests
//! drive a REAL in-process `PgServer` with `tokio-postgres`, whose connection
//! surfaces `NotificationResponse` messages as `AsyncMessage::Notification`:
//!
//! * an idle listener is told at once, without issuing another query;
//! * notifications sent inside a block arrive at COMMIT, are dropped on
//!   ROLLBACK (and by `ROLLBACK TO SAVEPOINT` for the ones after it), and
//!   identical (channel, payload) pairs of one transaction are folded;
//! * the simple and extended protocol families both send;
//! * the embedded API (`execute` / `query` / `execute_params` /
//!   `query_params`) sends to the wire listeners of the same database, and
//!   refuses `LISTEN` with 0A000 (it has no connection to deliver to);
//! * PostgreSQL's limits: payload shorter than 8000 bytes, else 22023.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use futures::StreamExt;
use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::{EmbeddedDatabase, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_postgres::{AsyncMessage, Client, NoTls, SimpleQueryMessage};

const WAIT: Duration = Duration::from_secs(5);
/// How long "nothing arrives" is watched for.
const QUIET: Duration = Duration::from_millis(300);

type Inbox = mpsc::UnboundedReceiver<(i32, String, String)>;

async fn setup_server() -> (Arc<EmbeddedDatabase>, String, tokio::task::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test port");
    let addr = listener.local_addr().expect("test addr");
    drop(listener);
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let server = PgServer::new(PgServerConfig::with_address(addr), Arc::clone(&db)).expect("server");
    let handle = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    let conn = format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port());
    (db, conn, handle)
}

/// A client whose connection task forwards every notification to the inbox.
async fn connect(conn: &str) -> (Client, Inbox, tokio::task::JoinHandle<()>) {
    let (client, mut connection) = timeout(WAIT, tokio_postgres::connect(conn, NoTls))
        .await
        .expect("connect timeout")
        .expect("connect");
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let mut stream = futures::stream::poll_fn(move |cx| connection.poll_message(cx));
        while let Some(message) = stream.next().await {
            match message {
                Ok(AsyncMessage::Notification(n)) => {
                    let _ = tx.send((n.process_id(), n.channel().to_string(), n.payload().to_string()));
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });
    (client, rx, task)
}

async fn run(client: &Client, sql: &str) {
    timeout(WAIT, client.batch_execute(sql))
        .await
        .expect("query timeout")
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e:?}"));
}

async fn next(inbox: &mut Inbox) -> (i32, String, String) {
    timeout(WAIT, inbox.recv())
        .await
        .expect("no notification arrived")
        .expect("connection task ended")
}

async fn assert_quiet(inbox: &mut Inbox) {
    if let Ok(Some(n)) = timeout(QUIET, inbox.recv()).await {
        panic!("unexpected notification {n:?}");
    }
}

async fn backend_pid(client: &Client) -> i32 {
    let rows = client.simple_query("SELECT pg_backend_pid()").await.expect("pid");
    for row in rows {
        if let SimpleQueryMessage::Row(row) = row {
            return row.get(0).expect("pid value").parse().expect("pid int");
        }
    }
    panic!("no pid row");
}

fn payloads(items: &[(i32, String, String)]) -> Vec<&str> {
    items.iter().map(|(_, _, p)| p.as_str()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_listener_is_told_at_once_with_the_sender_pid() {
    let (_db, conn, server) = setup_server().await;
    let (a, mut a_inbox, _ta) = connect(&conn).await;
    let (b, _b_inbox, _tb) = connect(&conn).await;
    run(&a, "LISTEN jobs").await;
    let b_pid = backend_pid(&b).await;

    run(&b, "NOTIFY jobs, 'hello'").await;
    // A issues nothing: the server must push the notification to an idle session.
    let (pid, channel, payload) = next(&mut a_inbox).await;
    assert_eq!((pid, channel.as_str(), payload.as_str()), (b_pid, "jobs", "hello"));

    // No payload is an empty payload; a channel nobody listens on reaches nobody.
    run(&b, "NOTIFY jobs; NOTIFY other_channel, 'x'").await;
    let (_, channel, payload) = next(&mut a_inbox).await;
    assert_eq!((channel.as_str(), payload.as_str()), ("jobs", ""));
    assert_quiet(&mut a_inbox).await;

    // Unquoted channel names fold to lower case, as every identifier does.
    run(&b, "NOTIFY JOBS, 'upper'").await;
    assert_eq!(next(&mut a_inbox).await.2, "upper");
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delivered_at_commit_dropped_at_rollback_and_folded() {
    let (_db, conn, server) = setup_server().await;
    let (a, mut a_inbox, _ta) = connect(&conn).await;
    let (b, _b_inbox, _tb) = connect(&conn).await;
    run(&a, "LISTEN q").await;

    run(&b, "BEGIN").await;
    run(&b, "NOTIFY q, 'x'").await;
    run(&b, "NOTIFY q, 'x'").await;
    run(&b, "SELECT pg_notify('q', 'y')").await;
    run(&b, "NOTIFY q, 'x'").await;
    assert_quiet(&mut a_inbox).await;
    run(&b, "COMMIT").await;
    let got = vec![next(&mut a_inbox).await, next(&mut a_inbox).await];
    assert_eq!(payloads(&got), vec!["x", "y"], "folded, in send order");
    assert_quiet(&mut a_inbox).await;

    run(&b, "BEGIN; NOTIFY q, 'rolled back'").await;
    run(&b, "ROLLBACK").await;
    assert_quiet(&mut a_inbox).await;

    // ROLLBACK TO SAVEPOINT drops only what was sent after the savepoint.
    run(
        &b,
        "BEGIN; NOTIFY q, 'keep'; SAVEPOINT s; NOTIFY q, 'drop'; ROLLBACK TO SAVEPOINT s",
    )
    .await;
    run(&b, "COMMIT").await;
    assert_eq!(next(&mut a_inbox).await.2, "keep");
    assert_quiet(&mut a_inbox).await;

    // A multi-statement batch is one implicit transaction: a failing later
    // statement drops the batch's notification.
    let failed = timeout(WAIT, b.batch_execute("NOTIFY q, 'batch'; SELECT * FROM no_such_table"))
        .await
        .expect("timeout");
    assert!(failed.is_err());
    assert_quiet(&mut a_inbox).await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn listener_inside_a_block_hears_after_its_block_ends() {
    let (_db, conn, server) = setup_server().await;
    let (a, mut a_inbox, _ta) = connect(&conn).await;
    let (b, _b_inbox, _tb) = connect(&conn).await;
    run(&a, "LISTEN w").await;
    run(&a, "BEGIN").await;
    run(&b, "NOTIFY w, 'later'").await;
    assert_quiet(&mut a_inbox).await;
    run(&a, "COMMIT").await;
    assert_eq!(next(&mut a_inbox).await.2, "later");
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn self_notification_and_unlisten() {
    let (_db, conn, server) = setup_server().await;
    let (a, mut a_inbox, _ta) = connect(&conn).await;
    run(&a, "LISTEN c1; LISTEN c2").await;
    run(&a, "NOTIFY c1, 'self'").await;
    assert_eq!(next(&mut a_inbox).await.2, "self");

    run(&a, "UNLISTEN c1").await;
    run(&a, "NOTIFY c1, 'gone'; NOTIFY c2, 'still'").await;
    assert_eq!(next(&mut a_inbox).await.2, "still");
    run(&a, "UNLISTEN *").await;
    run(&a, "NOTIFY c2, 'gone too'").await;
    assert_quiet(&mut a_inbox).await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extended_protocol_sends_and_listens() {
    let (_db, conn, server) = setup_server().await;
    let (a, mut a_inbox, _ta) = connect(&conn).await;
    let (b, _b_inbox, _tb) = connect(&conn).await;
    a.execute("LISTEN ext", &[]).await.expect("extended LISTEN");
    b.execute("SELECT pg_notify($1, $2)", &[&"ext", &"bound"])
        .await
        .expect("extended pg_notify");
    assert_eq!(next(&mut a_inbox).await.2, "bound");
    b.execute("NOTIFY ext, 'plain'", &[]).await.expect("extended NOTIFY");
    assert_eq!(next(&mut a_inbox).await.2, "plain");
    a.execute("UNLISTEN ext", &[]).await.expect("extended UNLISTEN");
    b.execute("NOTIFY ext, 'unheard'", &[]).await.expect("extended NOTIFY");
    assert_quiet(&mut a_inbox).await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgresql_limits_and_error_codes() {
    let (_db, conn, server) = setup_server().await;
    let (a, mut a_inbox, _ta) = connect(&conn).await;
    run(&a, "LISTEN lim").await;
    let ok = "p".repeat(7999);
    run(&a, &format!("SELECT pg_notify('lim', '{ok}')")).await;
    assert_eq!(next(&mut a_inbox).await.2.len(), 7999);

    let too_long = "p".repeat(8000);
    for sql in [
        format!("NOTIFY lim, '{too_long}'"),
        format!("SELECT pg_notify('lim', '{too_long}')"),
    ] {
        let err = a.simple_query(&sql).await.expect_err("8000 bytes is too long");
        let db = err.as_db_error().expect("db error");
        assert_eq!(db.code().code(), "22023", "{db:?}");
        assert!(db.message().ends_with("payload string too long"), "{db:?}");
    }
    let err = a
        .simple_query("SELECT pg_notify(NULL, 'x')")
        .await
        .expect_err("empty channel");
    assert_eq!(err.as_db_error().expect("db error").code().code(), "22023");
    assert_quiet(&mut a_inbox).await;

    // Command tags are PostgreSQL's.
    for (sql, tag) in [
        ("LISTEN t1", "LISTEN"),
        ("NOTIFY t1", "NOTIFY"),
        ("UNLISTEN t1", "UNLISTEN"),
    ] {
        let messages = a.simple_query(sql).await.expect(sql);
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, SimpleQueryMessage::CommandComplete(_))),
            "{sql}: {tag}"
        );
    }
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn embedded_api_sends_to_wire_listeners_and_refuses_listen() {
    let (db, conn, server) = setup_server().await;
    let (a, mut a_inbox, _ta) = connect(&conn).await;
    run(&a, "LISTEN emb").await;

    let db2 = Arc::clone(&db);
    tokio::task::spawn_blocking(move || {
        db2.execute("NOTIFY emb, 'execute'").expect("execute NOTIFY");
        db2.query("SELECT pg_notify('emb', 'query')", &[])
            .expect("query pg_notify");
        db2.execute_params(
            "SELECT pg_notify($1, $2)",
            &[Value::String("emb".into()), Value::String("execute_params".into())],
        )
        .expect("execute_params pg_notify");
        db2.query_params(
            "SELECT pg_notify($1, $2)",
            &[Value::String("emb".into()), Value::String("query_params".into())],
        )
        .expect("query_params pg_notify");
    })
    .await
    .expect("blocking");
    let mut got = Vec::new();
    for _ in 0..4 {
        got.push(next(&mut a_inbox).await);
    }
    assert_eq!(
        payloads(&got),
        vec!["execute", "query", "execute_params", "query_params"]
    );

    // The embedded global transaction: delivered at commit, dropped at rollback.
    // `begin()` binds the transaction to the calling thread (only that thread may
    // commit it), and two `spawn_blocking` calls may land on different pool
    // threads, so one dedicated thread runs the whole transaction and waits for
    // the go-ahead to commit while the listener is checked for silence.
    let db2 = Arc::clone(&db);
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let (began_tx, began_rx) = tokio::sync::oneshot::channel::<()>();
    let txn_thread = std::thread::spawn(move || {
        db2.begin().expect("begin");
        db2.execute("NOTIFY emb, 'txn'").expect("notify");
        db2.execute("NOTIFY emb, 'txn'").expect("notify");
        began_tx.send(()).expect("signal");
        go_rx.recv().expect("go-ahead");
        db2.commit().expect("commit");
    });
    began_rx.await.expect("transaction open");
    assert_quiet(&mut a_inbox).await;
    go_tx.send(()).expect("go-ahead");
    tokio::task::spawn_blocking(move || txn_thread.join())
        .await
        .expect("blocking")
        .expect("transaction thread");
    assert_eq!(next(&mut a_inbox).await.2, "txn");
    assert_quiet(&mut a_inbox).await;

    let db2 = Arc::clone(&db);
    tokio::task::spawn_blocking(move || {
        db2.begin().expect("begin");
        db2.execute("NOTIFY emb, 'never'").expect("notify");
        db2.rollback().expect("rollback");
    })
    .await
    .expect("blocking");
    assert_quiet(&mut a_inbox).await;

    // LISTEN has no connection to deliver to outside the PostgreSQL wire.
    let db2 = Arc::clone(&db);
    let errors = tokio::task::spawn_blocking(move || {
        vec![
            db2.execute("LISTEN emb").expect_err("execute LISTEN").to_string(),
            db2.query("LISTEN emb", &[]).expect_err("query LISTEN").to_string(),
            db2.execute_params("LISTEN emb", &[])
                .expect_err("execute_params LISTEN")
                .to_string(),
            db2.query_params("LISTEN emb", &[])
                .expect_err("query_params LISTEN")
                .to_string(),
        ]
    })
    .await
    .expect("blocking");
    for message in errors {
        assert!(
            message.contains("is not supported by HeliosDB Nano") && message.contains("LISTEN"),
            "{message}"
        );
        assert!(!message.contains("Ident {"), "{message}");
    }
    // UNLISTEN is harmless there, as in PostgreSQL for a session that never listened.
    let db2 = Arc::clone(&db);
    tokio::task::spawn_blocking(move || {
        db2.execute("UNLISTEN *").expect("UNLISTEN *");
        db2.execute_params("UNLISTEN emb", &[]).expect("UNLISTEN");
    })
    .await
    .expect("blocking");
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn listens_end_when_the_listener_disconnects() {
    let (_db, conn, server) = setup_server().await;
    let (a, mut a_inbox, a_task) = connect(&conn).await;
    let (b, _b_inbox, _tb) = connect(&conn).await;
    run(&a, "LISTEN bye").await;
    drop(a);
    let _ = timeout(WAIT, a_task).await;
    // Sending to a channel whose only listener left is fine.
    run(&b, "NOTIFY bye, 'nobody'").await;
    assert!(a_inbox.recv().await.is_none());
    // And a new connection does not inherit anything.
    let (c, mut c_inbox, _tc) = connect(&conn).await;
    run(&b, "NOTIFY bye, 'still nobody'").await;
    assert_quiet(&mut c_inbox).await;
    drop(c);
    server.abort();
}
