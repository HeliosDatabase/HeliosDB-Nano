#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names,
    clippy::large_futures,
    clippy::assert_is_empty
)]
//! The dictionary foundation over the PostgreSQL wire: routine rows in
//! `pg_proc`, reserved names and their SQLSTATE, one shape of `pg_tables`,
//! object identifiers that agree between psql's `\d` lookup and `pg_class`,
//! and an alias read through its canonical view.

use super::handler::PgConnectionHandler;
use super::messages::FrontendMessage;
use crate::EmbeddedDatabase;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, DuplexStream};

type Handler = PgConnectionHandler<DuplexStream>;

fn connect(db: &Arc<EmbeddedDatabase>) -> (Handler, DuplexStream) {
    let (server, client) = tokio::io::duplex(8 << 20);
    (PgConnectionHandler::new_for_tests(Arc::clone(db), server), client)
}

async fn drain(client: &mut DuplexStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 65536];
    while let Ok(Ok(n)) = tokio::time::timeout(std::time::Duration::from_millis(50), client.read(&mut buf)).await {
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

fn messages(bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 5 <= bytes.len() {
        let len = i32::from_be_bytes([bytes[pos + 1], bytes[pos + 2], bytes[pos + 3], bytes[pos + 4]]) as usize;
        let end = pos + 1 + len;
        assert!(end <= bytes.len(), "truncated message");
        out.push((bytes[pos], bytes[pos + 5..end].to_vec()));
        pos = end;
    }
    out
}

/// Every DataRow as text cells (`NULL` for a wire NULL).
fn data_rows(bytes: &[u8]) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    for (ty, payload) in messages(bytes) {
        if ty != b'D' {
            continue;
        }
        let ncols = i16::from_be_bytes([payload[0], payload[1]]) as usize;
        let mut pos = 2;
        let mut row = Vec::with_capacity(ncols);
        for _ in 0..ncols {
            let len = i32::from_be_bytes([payload[pos], payload[pos + 1], payload[pos + 2], payload[pos + 3]]);
            pos += 4;
            if len < 0 {
                row.push("NULL".to_string());
            } else {
                let len = len as usize;
                row.push(String::from_utf8_lossy(&payload[pos..pos + len]).to_string());
                pos += len;
            }
        }
        rows.push(row);
    }
    rows
}

fn column_names(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    for (ty, payload) in messages(bytes) {
        if ty != b'T' {
            continue;
        }
        let nfields = i16::from_be_bytes([payload[0], payload[1]]) as usize;
        let mut pos = 2;
        for _ in 0..nfields {
            let end = pos + payload[pos..].iter().position(|&b| b == 0).expect("field name");
            out.push(String::from_utf8_lossy(&payload[pos..end]).to_string());
            pos = end + 1 + 18;
        }
    }
    out
}

fn sqlstates(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    for (ty, payload) in messages(bytes) {
        if ty != b'E' {
            continue;
        }
        let mut pos = 0;
        while pos < payload.len() && payload[pos] != 0 {
            let field = payload[pos];
            pos += 1;
            let start = pos;
            while pos < payload.len() && payload[pos] != 0 {
                pos += 1;
            }
            if field == b'C' {
                out.push(String::from_utf8_lossy(&payload[start..pos]).to_string());
            }
            pos += 1;
        }
    }
    out
}

/// One simple-protocol statement; the backend bytes it produced.
async fn simple(handler: &mut Handler, client: &mut DuplexStream, sql: &str) -> Vec<u8> {
    handler
        .dispatch_message(FrontendMessage::Query { query: sql.into() })
        .await
        .unwrap_or_else(|e| panic!("dispatch `{sql}`: {e}"));
    drain(client).await
}

async fn setup(handler: &mut Handler, client: &mut DuplexStream, sql: &str) {
    let out = simple(handler, client, sql).await;
    assert!(
        sqlstates(&out).is_empty(),
        "setup `{sql}` failed: {:?}",
        sqlstates(&out)
    );
}

/// The same statement through Parse / Bind / Describe / Execute / Sync.
async fn extended(handler: &mut Handler, client: &mut DuplexStream, sql: &str) -> Vec<u8> {
    for message in [
        FrontendMessage::Parse {
            statement_name: String::new(),
            query: sql.into(),
            param_types: vec![],
        },
        FrontendMessage::Bind {
            portal_name: String::new(),
            statement_name: String::new(),
            param_formats: vec![],
            params: vec![],
            result_formats: vec![],
        },
        FrontendMessage::Describe {
            target: super::messages::DescribeTarget::Portal,
            name: String::new(),
        },
        FrontendMessage::Execute {
            portal_name: String::new(),
            max_rows: 0,
        },
        FrontendMessage::Sync,
    ] {
        // A message after a failed one is skipped by the handler until Sync.
        let _ = handler.dispatch_message(message).await;
    }
    drain(client).await
}

/// Rows of `sql`, required identical on the simple and the extended protocol.
async fn rows(handler: &mut Handler, client: &mut DuplexStream, sql: &str) -> Vec<Vec<String>> {
    let simple_out = simple(handler, client, sql).await;
    assert!(
        sqlstates(&simple_out).is_empty(),
        "`{sql}` failed: {:?}",
        sqlstates(&simple_out)
    );
    let extended_out = extended(handler, client, sql).await;
    assert!(
        sqlstates(&extended_out).is_empty(),
        "`{sql}` failed on the extended protocol: {:?}",
        sqlstates(&extended_out)
    );
    let got = data_rows(&simple_out);
    assert_eq!(data_rows(&extended_out), got, "`{sql}`: the two protocols must agree");
    assert_eq!(
        column_names(&extended_out),
        column_names(&simple_out),
        "`{sql}`: the two protocols must describe the same columns"
    );
    got
}

fn new_db() -> Arc<EmbeddedDatabase> {
    Arc::new(EmbeddedDatabase::new_in_memory().expect("database"))
}

// ---------------------------------------------------------------------------
// Item 1: producers reach the routine registry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wire_pg_proc_lists_user_routines() {
    let db = new_db();
    let (mut handler, mut client) = connect(&db);
    setup(&mut handler, &mut client, "CREATE TABLE dw_audit (id INTEGER)").await;
    setup(
        &mut handler,
        &mut client,
        "CREATE FUNCTION dw_add(a INTEGER, b TEXT) RETURNS INTEGER AS $$ SELECT $1 $$ LANGUAGE sql",
    )
    .await;
    setup(
        &mut handler,
        &mut client,
        "CREATE PROCEDURE dw_ins(p INTEGER) LANGUAGE sql AS $$INSERT INTO dw_audit VALUES ($p)$$",
    )
    .await;
    let got = rows(
        &mut handler,
        &mut client,
        "SELECT p.proname, p.prokind, p.prorettype, p.proargtypes, n.nspname \
         FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace ORDER BY p.proname",
    )
    .await;
    assert_eq!(
        got,
        vec![
            vec!["dw_add", "f", "23", "23 25", "public"],
            vec!["dw_ins", "p", "2278", "23", "public"]
        ]
    );
}

// ---------------------------------------------------------------------------
// Item 3: reserved names and engine-owned tables
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wire_reserved_names_are_42939() {
    let db = new_db();
    let (mut handler, mut client) = connect(&db);
    setup(&mut handler, &mut client, "CREATE TABLE dw_ok (id INTEGER PRIMARY KEY)").await;
    for sql in [
        "CREATE TABLE hdb_mine (id INTEGER)",
        "CREATE TABLE pg_class (id INTEGER)",
        "CREATE TABLE hdb_ctas AS SELECT 1 AS x",
        "CREATE VIEW hdb_v AS SELECT id FROM dw_ok",
        "CREATE VIEW pg_tables AS SELECT id FROM dw_ok",
        "ALTER TABLE dw_ok RENAME TO hdb_renamed",
        "ALTER TABLE dw_ok RENAME TO pg_settings",
    ] {
        let out = simple(&mut handler, &mut client, sql).await;
        assert_eq!(sqlstates(&out), vec!["42939".to_string()], "simple protocol: `{sql}`");
        let out = extended(&mut handler, &mut client, sql).await;
        assert_eq!(
            sqlstates(&out).first().map(String::as_str),
            Some("42939"),
            "extended protocol: `{sql}`"
        );
    }
    // The embedded error carries the same class.
    let error = db.execute("CREATE TABLE hdb_mine (id INTEGER)").expect_err("refused");
    assert_eq!(super::handler::sqlstate_for_error(&error), "42939");
    // The session is usable and nothing was created or renamed.
    let got = rows(
        &mut handler,
        &mut client,
        "SELECT table_name FROM information_schema.tables WHERE table_name LIKE 'hdb\\_%' OR table_name = 'dw_ok'",
    )
    .await;
    assert_eq!(got, vec![vec!["dw_ok"]]);
}

#[tokio::test]
async fn wire_engine_tables_refuse_user_ddl_with_42501() {
    let db = new_db();
    let (mut handler, mut client) = connect(&db);
    setup(
        &mut handler,
        &mut client,
        "CREATE TABLE IF NOT EXISTS _hdb_code_dw_probe (id INTEGER)",
    )
    .await;
    for sql in [
        "ALTER TABLE _hdb_code_dw_probe ADD COLUMN extra INTEGER",
        "DROP TABLE _hdb_code_dw_probe",
    ] {
        let out = simple(&mut handler, &mut client, sql).await;
        if cfg!(feature = "code-graph") {
            assert_eq!(sqlstates(&out), vec!["42501".to_string()], "`{sql}`");
        } else {
            assert!(
                sqlstates(&out).is_empty(),
                "`{sql}`: nothing owns the table in this binary"
            );
        }
    }
    // The decision itself, whatever this binary was compiled with.
    let refusal = crate::sql::dict::reserved::engine_table_refusal("_hdb_graph_nodes", "DROP TABLE", true)
        .expect("refused while the owning feature is compiled in");
    assert_eq!(super::handler::sqlstate_for_error(&refusal), "42501");
    assert!(crate::sql::dict::reserved::engine_table_refusal("_hdb_graph_nodes", "DROP TABLE", false).is_none());
}
