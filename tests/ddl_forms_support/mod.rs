//! Shared helpers of the DDL-form suites: both statement families, a persistent
//! database that can be reopened, and a PostgreSQL wire listener on an ephemeral
//! port.

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::{EmbeddedDatabase, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

/// The two statement families of the embedded API.
#[derive(Clone, Copy, Debug)]
pub enum Family {
    Text,
    Params,
}

pub const FAMILIES: [Family; 2] = [Family::Text, Family::Params];

pub fn run(db: &EmbeddedDatabase, family: Family, sql: &str) -> heliosdb_nano::Result<u64> {
    match family {
        Family::Text => db.execute(sql),
        Family::Params => db.execute_params(sql, &[]),
    }
}

pub fn ok(db: &EmbeddedDatabase, family: Family, sql: &str) -> u64 {
    run(db, family, sql).unwrap_or_else(|e| panic!("{family:?}: {sql}\n  {e}"))
}

/// The statement must fail; its message.
pub fn refused(db: &EmbeddedDatabase, family: Family, sql: &str) -> String {
    match run(db, family, sql) {
        Ok(rows) => panic!("{family:?}: {sql}\n  succeeded ({rows} rows) and should have been refused"),
        Err(e) => e.to_string(),
    }
}

pub fn setup(statements: &[&str]) -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
    for statement in statements {
        db.execute(statement)
            .unwrap_or_else(|e| panic!("setup failed: {statement}\n  {e}"));
    }
    db
}

pub fn rows(db: &EmbeddedDatabase, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}\n  {e}"))
        .into_iter()
        .map(|row| row.values)
        .collect()
}

/// Every value as text, NULL as `NULL`: a comparison that does not depend on
/// which integer or text type a value has.
pub fn text_rows(db: &EmbeddedDatabase, sql: &str) -> Vec<Vec<String>> {
    rows(db, sql)
        .into_iter()
        .map(|row| row.iter().map(text).collect())
        .collect()
}

pub fn text(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Open (or reopen) a persistent database. Retried: a handle just dropped
/// releases its lock in the background.
pub fn open(dir: &std::path::Path) -> EmbeddedDatabase {
    let mut last = None;
    for _ in 0..50 {
        match EmbeddedDatabase::new(dir) {
            Ok(db) => return db,
            Err(e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    panic!("could not open {}: {:?}", dir.display(), last);
}

pub struct Server {
    pub db: Arc<EmbeddedDatabase>,
    pub port: u16,
    listener: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.listener.abort();
    }
}

/// Serve `db` on the PostgreSQL wire, on a port the system picked.
pub async fn serve(db: EmbeddedDatabase) -> Server {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let db = Arc::new(db);
    let server = PgServer::new(
        PgServerConfig::with_address(addr).with_max_connections(16),
        Arc::clone(&db),
    )
    .unwrap();
    let listener = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    Server {
        db,
        port: addr.port(),
        listener,
    }
}

pub async fn connect(server: &Server) -> (Client, tokio::task::JoinHandle<()>) {
    let cs = format!("host=127.0.0.1 port={} user=postgres dbname=postgres", server.port);
    let (client, conn) = tokio_postgres::connect(&cs, NoTls)
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"));
    let task = tokio::spawn(async move { conn.await.map(|_| ()).unwrap_or(()) });
    (client, task)
}

/// Every row of a simple-protocol query, each value as text (NULL as `NULL`).
pub async fn wire_rows(client: &Client, sql: &str) -> Vec<Vec<String>> {
    client
        .simple_query(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .into_iter()
        .filter_map(|message| match message {
            SimpleQueryMessage::Row(row) => Some(
                (0..row.len())
                    .map(|i| row.get(i).unwrap_or("NULL").to_string())
                    .collect(),
            ),
            _ => None,
        })
        .collect()
}

pub async fn wire_ok(client: &Client, sql: &str) {
    client.simple_query(sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// The SQLSTATE and message of a statement the server must refuse.
pub async fn wire_refused(client: &Client, sql: &str) -> (String, String) {
    match client.simple_query(sql).await {
        Ok(_) => panic!("{sql}\n  succeeded over the wire and should have been refused"),
        Err(e) => match e.as_db_error() {
            Some(db) => (db.code().code().to_string(), db.message().to_string()),
            None => panic!("{sql}: not a database error: {e}"),
        },
    }
}

pub fn sqlstate(e: &tokio_postgres::Error) -> String {
    e.as_db_error()
        .map(|d| d.code().code().to_string())
        .unwrap_or_else(|| format!("<not a DbError: {e}>"))
}
