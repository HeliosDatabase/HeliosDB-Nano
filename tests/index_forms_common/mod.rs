//! Helpers shared by the index-form suites (`index_composite`, `index_partial`,
//! `index_expression`): both statement families, EXPLAIN text, a dump/restore
//! round trip in both formats, and a PostgreSQL-wire client on an ephemeral port.

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use std::sync::Arc;
use std::time::Duration;

use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::{EmbeddedDatabase, Value};
use tempfile::TempDir;

/// The two statement families: `execute`/`query`, and `execute_params`/`query_params`.
pub const FAMILIES: [bool; 2] = [false, true];

pub fn run(db: &EmbeddedDatabase, sql: &str, params: bool) -> heliosdb_nano::Result<u64> {
    if params {
        db.execute_params(sql, &[])
    } else {
        db.execute(sql)
    }
}

pub fn must(db: &EmbeddedDatabase, sql: &str, params: bool) {
    run(db, sql, params).unwrap_or_else(|e| panic!("`{sql}` failed (params={params}): {e}"));
}

pub fn rows(db: &EmbeddedDatabase, sql: &str, params: bool) -> Vec<Vec<Value>> {
    let tuples = if params {
        db.query_params(sql, &[])
    } else {
        db.query(sql, &[])
    };
    tuples
        .unwrap_or_else(|e| panic!("`{sql}` failed (params={params}): {e}"))
        .into_iter()
        .map(|t| t.values)
        .collect()
}

pub fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int2(n) => i64::from(*n),
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        other => panic!("not an integer: {other:?}"),
    }
}

pub fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => format!("{other}"),
    }
}

/// The first column of every row, as integers, in the order returned.
pub fn ids(db: &EmbeddedDatabase, sql: &str, params: bool) -> Vec<i64> {
    rows(db, sql, params).iter().map(|r| as_i64(&r[0])).collect()
}

/// [`ids`], sorted: for a query that promises no order.
pub fn id_set(db: &EmbeddedDatabase, sql: &str, params: bool) -> Vec<i64> {
    let mut got = ids(db, sql, params);
    got.sort_unstable();
    got
}

/// The first column of every row of a statement with bound parameters.
pub fn id_set_bound(db: &EmbeddedDatabase, sql: &str, bound: &[Value]) -> Vec<i64> {
    let mut got: Vec<i64> = db
        .query_params(sql, bound)
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
        .iter()
        .map(|t| as_i64(&t.values[0]))
        .collect();
    got.sort_unstable();
    got
}

/// The plan `EXPLAIN` prints for `sql`.
pub fn plan(db: &EmbeddedDatabase, sql: &str, params: bool) -> String {
    rows(db, &format!("EXPLAIN {sql}"), params)
        .iter()
        .map(|r| text(&r[0]))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The plan of a statement with bound parameters.
pub fn plan_bound(db: &EmbeddedDatabase, sql: &str, bound: &[Value]) -> String {
    db.query_params(&format!("EXPLAIN {sql}"), bound)
        .unwrap_or_else(|e| panic!("EXPLAIN `{sql}` failed: {e}"))
        .iter()
        .map(|t| text(&t.values[0]))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `indexdef` of one index in `pg_indexes`.
pub fn indexdef(db: &EmbeddedDatabase, index: &str, params: bool) -> String {
    let found = rows(
        db,
        &format!("SELECT indexdef FROM pg_indexes WHERE indexname = '{index}'"),
        params,
    );
    assert_eq!(found.len(), 1, "{index} in pg_indexes (params={params}): {found:?}");
    text(&found[0][0])
}

#[derive(Clone, Copy, Debug)]
pub enum Format {
    Sql,
    Binary,
}

pub const FORMATS: [Format; 2] = [Format::Sql, Format::Binary];

/// Dump `src` and restore it into a fresh database.
pub fn round_trip(src: &EmbeddedDatabase, format: Format) -> (EmbeddedDatabase, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let mut db = EmbeddedDatabase::new_in_memory().expect("target");
    match format {
        Format::Sql => {
            let path = dir.path().join("dump.sql");
            src.dump_sql(&path).expect("dump_sql");
            let script = std::fs::read_to_string(&path).expect("read");
            db.execute_sql_script(&script)
                .unwrap_or_else(|e| panic!("sql restore failed: {e}\n----\n{script}"));
        }
        Format::Binary => {
            let path = dir.path().join("dump.heliodump");
            src.dump_full(&path).expect("dump_full");
            db.restore_from_dump(&path).expect("binary restore");
        }
    }
    (db, dir)
}

/// Serve `db` over the PostgreSQL wire on an ephemeral loopback port.
pub async fn serve(db: Arc<EmbeddedDatabase>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test port");
    let addr = listener.local_addr().expect("test addr");
    drop(listener);
    let server = PgServer::new(PgServerConfig::with_address(addr), db).expect("server");
    let handle = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    (
        format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port()),
        handle,
    )
}

pub async fn connect(conn: &str) -> tokio_postgres::Client {
    let (client, connection) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_postgres::connect(conn, tokio_postgres::NoTls),
    )
    .await
    .expect("connect timeout")
    .expect("connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// Every row of a simple-protocol query, each as its column texts.
pub async fn wire_rows(client: &tokio_postgres::Client, sql: &str) -> Vec<Vec<String>> {
    client
        .simple_query(sql)
        .await
        .unwrap_or_else(|e| panic!("`{sql}` failed over the wire: {e}"))
        .into_iter()
        .filter_map(|m| match m {
            tokio_postgres::SimpleQueryMessage::Row(row) => Some(
                (0..row.columns().len())
                    .map(|i| row.get(i).unwrap_or("").to_string())
                    .collect(),
            ),
            _ => None,
        })
        .collect()
}

/// The SQLSTATE a statement fails with over the wire.
pub async fn wire_sqlstate(client: &tokio_postgres::Client, sql: &str) -> String {
    match client.simple_query(sql).await {
        Ok(_) => panic!("`{sql}` succeeded over the wire; an error was expected"),
        Err(e) => e
            .code()
            .map(|c| c.code().to_string())
            .unwrap_or_else(|| panic!("`{sql}`: no SQLSTATE in {e}")),
    }
}
