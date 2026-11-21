#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names,
    clippy::large_futures
)]
//! Triggers over the PostgreSQL wire: they fire on the simple and the
//! extended protocol, `RAISE NOTICE` arrives as a NoticeResponse before the
//! statement's CommandComplete, and the errors carry their SQLSTATE.

use super::handler::PgConnectionHandler;
use super::messages::FrontendMessage;
use crate::EmbeddedDatabase;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, DuplexStream};

fn connect(db: &Arc<EmbeddedDatabase>) -> (PgConnectionHandler<DuplexStream>, DuplexStream) {
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

/// The backend messages in `bytes` as (type, payload).
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

/// One field of an ErrorResponse or NoticeResponse payload.
fn field(payload: &[u8], code: u8) -> String {
    payload
        .split(|&b| b == 0)
        .find(|f| f.first() == Some(&code))
        .map(|f| String::from_utf8_lossy(&f[1..]).to_string())
        .unwrap_or_default()
}

/// (SQLSTATE, message) of the first ErrorResponse.
fn error(bytes: &[u8]) -> Option<(String, String)> {
    messages(bytes)
        .into_iter()
        .find(|(t, _)| *t == b'E')
        .map(|(_, p)| (field(&p, b'C'), field(&p, b'M')))
}

async fn query(handler: &mut PgConnectionHandler<DuplexStream>, client: &mut DuplexStream, sql: &str) -> Vec<u8> {
    handler
        .dispatch_message(FrontendMessage::Query { query: sql.into() })
        .await
        .unwrap_or_else(|e| panic!("dispatch `{sql}`: {e}"));
    drain(client).await
}

async fn setup(handler: &mut PgConnectionHandler<DuplexStream>, client: &mut DuplexStream, sql: &str) {
    let out = query(handler, client, sql).await;
    assert!(error(&out).is_none(), "`{sql}` failed over the wire: {:?}", error(&out));
}

fn count(db: &EmbeddedDatabase, table: &str) -> usize {
    db.query(&format!("SELECT * FROM {table}"), &[]).unwrap().len()
}

async fn audited(handler: &mut PgConnectionHandler<DuplexStream>, client: &mut DuplexStream) {
    setup(handler, client, "CREATE TABLE items (id INT PRIMARY KEY, qty INT)").await;
    setup(handler, client, "CREATE TABLE log (line TEXT)").await;
    setup(
        handler,
        client,
        "CREATE FUNCTION audit() RETURNS TRIGGER AS $$ BEGIN \
           IF NEW.qty < 0 THEN RAISE EXCEPTION 'qty % is negative', NEW.qty; END IF; \
           IF NEW.qty = 0 THEN RAISE EXCEPTION SQLSTATE '23514' 'qty is zero'; END IF; \
           RAISE NOTICE 'item % stored', NEW.id; \
           INSERT INTO log VALUES ('item ' || NEW.id); RETURN NEW; END $$ LANGUAGE plpgsql",
    )
    .await;
    setup(
        handler,
        client,
        "CREATE TRIGGER items_audit AFTER INSERT ON items FOR EACH ROW EXECUTE FUNCTION audit()",
    )
    .await;
}

#[tokio::test]
async fn the_simple_protocol_fires_triggers_and_delivers_raise_notice() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = connect(&db);
    audited(&mut handler, &mut client).await;

    let out = query(&mut handler, &mut client, "INSERT INTO items VALUES (1, 5)").await;
    assert!(error(&out).is_none(), "{:?}", error(&out));
    let types: Vec<u8> = messages(&out).into_iter().map(|(t, _)| t).collect();
    let notice = types.iter().position(|t| *t == b'N').expect("a NoticeResponse");
    let complete = types.iter().position(|t| *t == b'C').expect("a CommandComplete");
    assert!(notice < complete, "the notice comes before the command tag: {types:?}");
    let (_, payload) = messages(&out).into_iter().find(|(t, _)| *t == b'N').unwrap();
    assert_eq!(field(&payload, b'S'), "NOTICE");
    assert_eq!(field(&payload, b'M'), "item 1 stored");
    assert_eq!(count(&db, "log"), 1);
}

#[tokio::test]
async fn raise_exception_reaches_the_client_with_its_sqlstate_and_nothing_is_stored() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = connect(&db);
    audited(&mut handler, &mut client).await;

    let out = query(&mut handler, &mut client, "INSERT INTO items VALUES (1, 5), (2, -4)").await;
    let (code, message) = error(&out).expect("the second row is refused");
    assert_eq!(code, "P0001", "{message}");
    assert!(message.contains("qty -4 is negative"), "{message}");
    assert_eq!(count(&db, "items"), 0);
    assert_eq!(count(&db, "log"), 0);

    let out = query(&mut handler, &mut client, "INSERT INTO items VALUES (3, 0)").await;
    let (code, message) = error(&out).expect("zero is refused");
    assert_eq!(code, "23514", "{message}");
    assert_eq!(count(&db, "items"), 0);
}

#[tokio::test]
async fn the_extended_protocol_fires_triggers_with_bound_parameters() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = connect(&db);
    audited(&mut handler, &mut client).await;

    handler
        .handle_parse_extended("s1".into(), "INSERT INTO items VALUES ($1, $2)".into(), vec![])
        .await
        .expect("parse");
    handler
        .handle_bind_extended(
            "p1".into(),
            "s1".into(),
            vec![],
            vec![Some(b"7".to_vec()), Some(b"70".to_vec())],
            vec![],
        )
        .await
        .expect("bind");
    handler.handle_execute_extended("p1".into(), 0).await.expect("execute");
    let out = drain(&mut client).await;
    assert!(error(&out).is_none(), "{:?}", error(&out));
    assert_eq!(count(&db, "items"), 1);
    let log = db.query("SELECT line FROM log", &[]).unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].values[0], crate::Value::String("item 7".into()));
    assert!(
        messages(&out)
            .iter()
            .any(|(t, p)| *t == b'N' && field(p, b'M') == "item 7 stored"),
        "the notice is delivered on the extended protocol too"
    );
}

#[tokio::test]
async fn trigger_errors_carry_their_sqlstate() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = connect(&db);
    audited(&mut handler, &mut client).await;
    setup(&mut handler, &mut client, "CREATE VIEW items_v AS SELECT id FROM items").await;
    setup(
        &mut handler,
        &mut client,
        "CREATE FUNCTION again() RETURNS TRIGGER AS $$ BEGIN INSERT INTO log VALUES ('again'); RETURN NEW; END $$ \
         LANGUAGE plpgsql",
    )
    .await;
    setup(
        &mut handler,
        &mut client,
        "CREATE TRIGGER log_again AFTER INSERT ON log FOR EACH ROW EXECUTE FUNCTION again()",
    )
    .await;

    for (sql, code) in [
        // A chain that never ends.
        ("INSERT INTO log VALUES ('start')", "54001"),
        // A form that would never fire.
        (
            "CREATE TRIGGER t INSTEAD OF INSERT ON items_v FOR EACH ROW EXECUTE FUNCTION audit()",
            "0A000",
        ),
        // A function that does not exist.
        (
            "CREATE TRIGGER t AFTER INSERT ON items FOR EACH ROW EXECUTE FUNCTION nope()",
            "42883",
        ),
        // A function a trigger runs.
        ("DROP FUNCTION audit", "2BP01"),
        // An inline body with a construct that is not taken.
        (
            "CREATE TRIGGER t BEFORE INSERT ON items FOR EACH ROW BEGIN DECLARE n INT; END",
            "0A000",
        ),
    ] {
        let out = query(&mut handler, &mut client, sql).await;
        let (got, message) = error(&out).unwrap_or_else(|| panic!("`{sql}` must fail"));
        assert_eq!(got, code, "`{sql}`: {message}");
    }
}

#[tokio::test]
async fn an_inline_body_with_semicolons_is_one_statement_on_the_simple_protocol() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = connect(&db);
    setup(
        &mut handler,
        &mut client,
        "CREATE TABLE items (id INT PRIMARY KEY, qty INT)",
    )
    .await;
    setup(&mut handler, &mut client, "CREATE TABLE log (line TEXT)").await;
    setup(
        &mut handler,
        &mut client,
        "CREATE TRIGGER items_ai AFTER INSERT ON items BEGIN INSERT INTO log VALUES ('one'); INSERT INTO log VALUES \
         ('two'); END;",
    )
    .await;
    setup(&mut handler, &mut client, "INSERT INTO items VALUES (1, 1)").await;
    assert_eq!(count(&db, "log"), 2);
}
