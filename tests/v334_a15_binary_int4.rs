//! Regression for checklist item A15.
//!
//! Binary-first PostgreSQL clients request binary result formats in Bind. Before
//! A15, extended Execute returned all DataRow values as text bytes, so an int4
//! value such as `1` was sent as one byte (`"1"`) and client-side binary int4
//! decoders failed with the asyncpg-shaped "requested 4 remaining 1" error.
//!
//! This uses tokio-postgres as the client analogue because it also requests and
//! decodes binary int4 results. Parameterized extended queries still hit the
//! pre-existing in-process PgServer stack-overflow harness artifact, so this
//! test uses a no-parameter SELECT inside a transaction to isolate the binary
//! result path.

use heliosdb_nano::{
    protocol::postgres::server::{PgServer, PgServerConfig},
    EmbeddedDatabase,
};
use std::{future::Future, sync::Arc, time::Duration};
use tokio_postgres::{Client, NoTls};

fn run_with_large_stack<F: Future>(body: F) -> F::Output {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_stack_size(32 * 1024 * 1024)
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(body)
}

async fn setup() -> (String, tokio::task::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
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
    let (client, connection) = tokio_postgres::connect(conn_string, NoTls).await.expect("connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

#[test]
fn a15_binary_int4_result_in_transaction() {
    run_with_large_stack(async {
        let (conn_string, server_handle) = setup().await;
        let client = connect(&conn_string).await;

        client
            .batch_execute(
                "CREATE TABLE a15_tokio (id INT PRIMARY KEY, n INT);
                 INSERT INTO a15_tokio VALUES (1, 42), (2, 7);",
            )
            .await
            .expect("seed");

        client.batch_execute("BEGIN").await.expect("begin");

        let rows = client
            .query("SELECT n FROM a15_tokio WHERE id = 1", &[])
            .await
            .expect("binary int4 result query inside transaction");
        assert_eq!(rows.len(), 1);
        let n: i32 = rows[0].get(0);
        assert_eq!(n, 42);

        let rows = client
            .query("SELECT id, n FROM a15_tokio WHERE id = 2", &[])
            .await
            .expect("binary multi-column int4 result query");
        assert_eq!(rows.len(), 1);
        let id: i32 = rows[0].get(0);
        let n: i32 = rows[0].get(1);
        assert_eq!((id, n), (2, 7));

        client.batch_execute("COMMIT").await.expect("commit");
        server_handle.abort();
    });
}
