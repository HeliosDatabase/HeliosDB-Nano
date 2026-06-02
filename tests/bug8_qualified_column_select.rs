//! bug-8 reproduction: table-qualified column references in single-table SELECT.
//! Reported by the markon team against v3.35.0 (psycopg3/SQLAlchemy): every
//! ORM `select(Model)` emits `SELECT t.col, ...` which fails over the wire.
#![allow(clippy::unwrap_used)]

use heliosdb_nano::{
    protocol::postgres::server::{PgServer, PgServerConfig},
    EmbeddedDatabase,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use tokio_postgres::NoTls;

// embedded baseline: the general engine path stamps source tables, so these PASS.
#[test]
fn bug8_embedded_qualified_ok() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE leads (id TEXT, email TEXT, first_name TEXT)").unwrap();
    db.execute("INSERT INTO leads (id,email,first_name) VALUES ('x','e@x.com','n')").unwrap();
    db.query("SELECT leads.id, leads.email FROM leads", &[]).unwrap();
    db.query_with_columns("SELECT leads.id, leads.email FROM leads").unwrap();
    db.query_params_with_columns("SELECT leads.id, leads.email FROM leads", &[]).unwrap();
}

async fn setup() -> (String, tokio::task::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener);
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let server = PgServer::new(PgServerConfig::with_address(addr), db).expect("server");
    let handle = tokio::spawn(async move {
        if let Err(e) = server.serve().await {
            eprintln!("server: {e}");
        }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    (format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port()), handle)
}

async fn connect(s: &str) -> tokio_postgres::Client {
    let (client, conn) = tokio_postgres::connect(s, NoTls).await.expect("connect");
    tokio::spawn(async move { let _ = conn.await; });
    client
}

// wire repro: extended protocol (what psycopg3/SQLAlchemy use).
#[test]
fn bug8_qualified_columns_over_wire() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_stack_size(32 * 1024 * 1024)
        .enable_all()
        .build()
        .expect("rt");
    rt.block_on(async {
        let (cs, _h) = setup().await;
        let client = connect(&cs).await;
        // DDL/seed via simple-query protocol (batch_execute); the extended path
        // doesn't implement CreateTable in the in-process harness.
        client
            .batch_execute(
                "CREATE TABLE leads (id TEXT, email TEXT, first_name TEXT); \
                 INSERT INTO leads (id,email,first_name) VALUES ('x','e@x.com','n');",
            )
            .await
            .unwrap();

        // baseline: unqualified works (extended)
        let r = timeout(Duration::from_secs(5), client.query("SELECT id, email FROM leads", &[])).await.expect("hang");
        assert!(r.is_ok(), "unqualified failed: {:?}", r.err());

        // bug-8: table-qualified projection (extended)
        let r = timeout(Duration::from_secs(5), client.query("SELECT leads.id, leads.email FROM leads", &[])).await.expect("hang");
        assert!(r.is_ok(), "QUALIFIED over wire FAILED (bug-8): {:?}", r.err());
        assert_eq!(r.unwrap().len(), 1);
    });
}

// extra axes: simple-query protocol + quoted identifiers (what psql/SQLAlchemy may emit).
#[test]
fn bug8_qualified_columns_axes() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_stack_size(32 * 1024 * 1024)
        .enable_all()
        .build()
        .expect("rt");
    rt.block_on(async {
        let (cs, _h) = setup().await;
        let client = connect(&cs).await;
        client
            .batch_execute(
                "CREATE TABLE leads (id TEXT, email TEXT, first_name TEXT); \
                 INSERT INTO leads (id,email,first_name) VALUES ('x','e@x.com','n');",
            )
            .await
            .unwrap();

        // simple-query protocol, qualified
        let r = timeout(Duration::from_secs(5), client.simple_query("SELECT leads.id, leads.email FROM leads")).await.expect("hang");
        assert!(r.is_ok(), "SIMPLE qualified FAILED: {:?}", r.err());

        // extended, quoted-qualified
        let r = timeout(Duration::from_secs(5), client.query("SELECT \"leads\".\"id\", \"leads\".\"email\" FROM leads", &[])).await.expect("hang");
        assert!(r.is_ok(), "QUOTED qualified FAILED: {:?}", r.err());

        // extended, qualified with table alias (SQLAlchemy ORM uses anon aliases sometimes)
        let r = timeout(Duration::from_secs(5), client.query("SELECT l.id, l.email FROM leads AS l", &[])).await.expect("hang");
        assert!(r.is_ok(), "ALIAS qualified FAILED: {:?}", r.err());
    });
}

// persistent (RocksDB-backed) server — matches markon's real `heliosdb-nano start` daemon.
#[test]
fn bug8_qualified_columns_persistent_wire() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_stack_size(32 * 1024 * 1024)
        .enable_all()
        .build()
        .expect("rt");
    rt.block_on(async {
        let dir = std::env::temp_dir().join(format!("bug8_helios_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        let db = Arc::new(EmbeddedDatabase::new(&dir).expect("persistent db"));
        let server = PgServer::new(PgServerConfig::with_address(addr), db).expect("server");
        let _h = tokio::spawn(async move { let _ = server.serve().await; });
        tokio::time::sleep(Duration::from_millis(200)).await;
        let cs = format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port());
        let client = connect(&cs).await;
        client
            .batch_execute(
                "CREATE TABLE leads (id TEXT, email TEXT, first_name TEXT); \
                 INSERT INTO leads (id,email,first_name) VALUES ('x','e@x.com','n');",
            )
            .await
            .unwrap();

        let r = timeout(Duration::from_secs(5), client.query("SELECT id, email FROM leads", &[])).await.expect("hang");
        assert!(r.is_ok(), "persistent unqualified failed: {:?}", r.err());

        let r = timeout(Duration::from_secs(5), client.query("SELECT leads.id, leads.email FROM leads", &[])).await.expect("hang");
        let ok = r.is_ok();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ok, "PERSISTENT qualified over wire FAILED (bug-8): {:?}", r.err());
    });
}
