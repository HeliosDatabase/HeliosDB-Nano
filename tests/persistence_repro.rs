//! INSERT-then-SELECT-misses-the-row repro (#205).
//!
//! Drives the same write pattern CloudV2's `admin_db::simple_execute`
//! uses against a Nano server: open ONE tokio-postgres connection,
//! `batch_execute(INSERT)` + `batch_execute("COMMIT")`, then SELECT
//! the row back on the same connection.  Asserts the row is
//! visible.
//!
//! Reference: `/home/app/Helios/CloudV2/docs/PERSISTENCE-BUG-INVESTIGATION.md`
//! step 8.4 — "Build a minimal examples/insert_visibility_repro.rs
//! in /home/app/Helios/Nano that reproduces the failure with raw
//! tokio_postgres, no admin_db wrapper."
//!
//! If this test PASSES, the bug is in the deadpool / admin_db
//! layer (Theory B).  If it FAILS the same way prod does, the bug
//! is in Nano's simple-protocol commit path (Theories A / C).

use std::sync::Arc;
use std::time::Duration;

use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::EmbeddedDatabase;
use tokio_postgres::NoTls;

async fn setup() -> Result<(String, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    drop(listener);

    let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
    let config = PgServerConfig::with_address(addr);
    let server = PgServer::new(config, db)?;
    let handle = tokio::spawn(async move {
        if let Err(e) = server.serve().await {
            eprintln!("Server error: {e}");
        }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    Ok((
        format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port()),
        handle,
    ))
}

#[tokio::test]
#[ignore = "PostgreSQL wire-protocol server requires CloudV2-specific patches; tracked in #205"]
async fn insert_then_select_visible_on_same_connection() {
    let (cs, server_handle) = setup().await.expect("server up");
    let (client, connection) = tokio_postgres::connect(&cs, NoTls)
        .await
        .expect("connect");
    let _conn_handle = tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {e}");
        }
    });

    // Bootstrap a minimal `databases` table mirroring the CloudV2
    // shape (id UUID PRIMARY KEY, name TEXT NOT NULL).
    client
        .batch_execute(
            "CREATE TABLE databases (\
               id UUID PRIMARY KEY, \
               name TEXT NOT NULL, \
               org_id TEXT, \
               created_at TIMESTAMP\
             )",
        )
        .await
        .expect("create databases table");

    // Same write pattern admin_db::simple_execute uses: simple
    // protocol via batch_execute, separate COMMIT.
    let id = uuid::Uuid::new_v4();
    let insert_sql = format!(
        "INSERT INTO databases (id, name, org_id, created_at) \
         VALUES ('{id}', 'newdb_repro', 'org-1', NOW())"
    );
    client.batch_execute(&insert_sql).await.expect("insert");
    let _ = client.batch_execute("COMMIT").await; // matches CloudV2's `let _`

    // Read it back via simple_query (same as admin_db's read path).
    let rows = client
        .simple_query(&format!("SELECT id, name FROM databases WHERE id = '{id}'"))
        .await
        .expect("select");
    let data_rows: Vec<_> = rows
        .into_iter()
        .filter(|r| matches!(r, tokio_postgres::SimpleQueryMessage::Row(_)))
        .collect();

    assert!(
        !data_rows.is_empty(),
        "INSERT-then-SELECT lost the row on the same connection — \
         the CloudV2 persistence bug reproduces against Nano alone. \
         See /home/app/Helios/CloudV2/docs/PERSISTENCE-BUG-INVESTIGATION.md \
         theories A/C."
    );

    // Also verify LIST (no WHERE on id) — the bug also masks rows
    // from `SELECT *` per the prod symptom matrix.
    let list = client
        .simple_query("SELECT id FROM databases")
        .await
        .expect("list");
    let list_rows: Vec<_> = list
        .into_iter()
        .filter(|r| matches!(r, tokio_postgres::SimpleQueryMessage::Row(_)))
        .collect();
    assert!(
        !list_rows.is_empty(),
        "LIST also missed the row — Theory A or C."
    );

    server_handle.abort();
}
