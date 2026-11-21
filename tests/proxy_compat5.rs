//! What HeliosProxy needs from Nano on the PostgreSQL wire (5.0 pre-GA list, 2026-11-21).
//!
//! The Proxy runs one integration suite against PostgreSQL 18 and Nano and skips the parts Nano
//! cannot serve. Each item it reported is pinned here: a test that runs is behaviour Nano has and
//! must keep; an `#[ignore]`d test is a defect still open, with the 5.0 package that owns it in
//! the reason. Remove the `#[ignore]` in the commit that fixes it.
//!
//! 1. `SHOW ALL` and `row_security` (on by default, PostgreSQL semantics), with PostgreSQL's
//!    spelling of the names (`DateStyle`, `TimeZone`) and the command tag `SHOW` (both fixed in
//!    4.42.1): runs.
//! 2. A statement that starts with a block comment returns its rows: runs.
//! 3. The type of an array parameter in `ParameterDescription`: open (`pgwire-rest`).
//! 4. Session restore: `set_config` and `DISCARD ALL` run; SQL-level `PREPARE` / `EXECUTE`,
//!    `pg_prepared_statements` (`sql-std`) and the `RESET` command tag (`pgwire-rest`) are open.
//! 5. Fenced promotion: `pg_promote()` and `SWITCHOVER TO` exist and fence (tests in
//!    `tests/physical_*.rs`); a stand-alone fence or demote of a running node is owned by
//!    `ha-online` and gets its tests there.
//! 6. SQL text read as PostgreSQL reads it: the statement splitter cases run; a nested block
//!    comment after the first token is open (`sql-correct`); catalog reads answered by matching
//!    raw text are removed by `dict-alias`.
//! 7. Every 5.0 change to `SHOW`, Describe or `ParameterStatus` output is checked against the
//!    Proxy's suite before release (a GA exit gate, not a test in this file).

use heliosdb_nano::{
    protocol::postgres::server::{PgServer, PgServerConfig},
    EmbeddedDatabase,
};
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;
use tokio_postgres::{types::Type, NoTls, SimpleQueryMessage};

async fn server() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener);
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let server = PgServer::new(PgServerConfig::with_address(addr), db).expect("server");
    tokio::spawn(async move {
        if let Err(e) = server.serve().await {
            eprintln!("server: {e}");
        }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    addr.port()
}

async fn connect(port: u16) -> tokio_postgres::Client {
    let (client, conn) = tokio_postgres::connect(
        &format!("host=127.0.0.1 port={port} user=postgres dbname=postgres"),
        NoTls,
    )
    .await
    .expect("connect");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
}

fn rows(messages: &[SimpleQueryMessage]) -> Vec<Vec<Option<String>>> {
    messages
        .iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(r) => Some((0..r.len()).map(|i| r.get(i).map(str::to_string)).collect()),
            _ => None,
        })
        .collect()
}

/// The command tags of one simple query, read off the wire (the drivers do not expose them).
fn command_tags(port: u16, sql: &str) -> Vec<String> {
    fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
        out.extend_from_slice(body);
        out
    }
    fn read_message(stream: &mut std::net::TcpStream) -> (u8, Vec<u8>) {
        let mut head = [0u8; 5];
        stream.read_exact(&mut head).expect("header");
        let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
        let mut body = vec![0u8; len - 4];
        stream.read_exact(&mut body).expect("body");
        (head[0], body)
    }
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(10))).expect("timeout");
    let mut startup = 196_608u32.to_be_bytes().to_vec();
    startup.extend_from_slice(b"user\0postgres\0database\0postgres\0\0");
    let mut packet = ((startup.len() + 4) as u32).to_be_bytes().to_vec();
    packet.extend_from_slice(&startup);
    stream.write_all(&packet).expect("startup");
    while read_message(&mut stream).0 != b'Z' {}
    let mut query = sql.as_bytes().to_vec();
    query.push(0);
    stream.write_all(&frame(b'Q', &query)).expect("query");
    let mut tags = Vec::new();
    loop {
        let (tag, body) = read_message(&mut stream);
        match tag {
            b'C' => tags.push(String::from_utf8_lossy(&body[..body.len() - 1]).into_owned()),
            b'E' => tags.push("ERROR".to_string()),
            b'Z' => return tags,
            _ => {}
        }
    }
}

// ── 1. SHOW ALL and row_security ────────────────────────────────────────────

#[tokio::test]
async fn item1_show_all_lists_row_security_on() {
    let client = connect(server().await).await;
    let all = rows(&client.simple_query("SHOW ALL").await.expect("SHOW ALL"));
    let row_security = all
        .iter()
        .find(|r| r.first().and_then(|n| n.as_deref()) == Some("row_security"))
        .expect("SHOW ALL lists row_security");
    assert_eq!(row_security.get(1).and_then(|v| v.as_deref()), Some("on"));
}

/// The Proxy compares `SHOW ALL` names case-sensitively, as PostgreSQL prints them (fixed in
/// 4.42.1: 4.42.0 listed `datestyle` and `timezone`).
#[tokio::test]
async fn item1_show_all_uses_postgresql_names() {
    let client = connect(server().await).await;
    let all = rows(&client.simple_query("SHOW ALL").await.expect("SHOW ALL"));
    let names: Vec<&str> = all
        .iter()
        .filter_map(|r| r.first().and_then(|n| n.as_deref()))
        .collect();
    for name in [
        "DateStyle",
        "TimeZone",
        "search_path",
        "client_encoding",
        "standard_conforming_strings",
    ] {
        assert!(names.contains(&name), "SHOW ALL lacks {name}: {names:?}");
    }
    for name in ["datestyle", "timezone"] {
        assert!(!names.contains(&name), "SHOW ALL lists {name} in lower case");
    }
}

/// The Proxy trusts a `SHOW ALL` reply only when its CommandComplete tag is exactly `SHOW`
/// (fixed in 4.42.1: 4.42.0 sent `SELECT 56` on the simple protocol).
#[tokio::test]
async fn item1_show_command_tag_is_show() {
    let port = server().await;
    for sql in ["SHOW ALL", "SHOW TimeZone", "SHOW search_path"] {
        let tags = tokio::task::spawn_blocking(move || command_tags(port, sql))
            .await
            .expect("blocking");
        assert_eq!(tags, vec!["SHOW".to_string()], "{sql}");
    }
}

// ── 2. A leading block comment ──────────────────────────────────────────────

#[tokio::test]
async fn item2_leading_block_comment_returns_rows_simple_and_extended() {
    let client = connect(server().await).await;
    client
        .batch_execute("CREATE TABLE c2 (id BIGINT PRIMARY KEY); INSERT INTO c2 VALUES (1), (2)")
        .await
        .expect("setup");
    for sql in [
        "/* leading */ SELECT id FROM c2 ORDER BY id",
        "/* a */ /* b */\nSELECT id FROM c2 ORDER BY id",
        "/* outer /* nested */ still */ SELECT id FROM c2 ORDER BY id",
    ] {
        let simple = rows(&client.simple_query(sql).await.expect(sql));
        assert_eq!(simple.len(), 2, "simple protocol: {sql}");
        let extended = client.query(sql, &[]).await.expect(sql);
        assert_eq!(extended.len(), 2, "extended protocol: {sql}");
    }
}

// ── 3. Parameter types in ParameterDescription ──────────────────────────────

#[tokio::test]
async fn item3_scalar_parameter_types_are_described() {
    let client = connect(server().await).await;
    client
        .batch_execute("CREATE TABLE c3 (id BIGINT PRIMARY KEY, v TEXT)")
        .await
        .expect("setup");
    let by_key = client.prepare("SELECT v FROM c3 WHERE id = $1").await.expect("prepare");
    assert_eq!(by_key.params(), &[Type::INT8]);
    let insert = client.prepare("INSERT INTO c3 VALUES ($1, $2)").await.expect("prepare");
    assert_eq!(insert.params(), &[Type::INT8, Type::TEXT]);
}

#[tokio::test]
#[ignore = "5.0 package pgwire-rest: array parameters are described as 705 (unknown)"]
async fn item3_array_parameter_types_are_described() {
    let client = connect(server().await).await;
    client
        .batch_execute("CREATE TABLE c3a (id BIGINT PRIMARY KEY)")
        .await
        .expect("setup");
    for (sql, want) in [
        ("SELECT $1::bigint[]", Type::INT8_ARRAY),
        ("SELECT $1::int[]", Type::INT4_ARRAY),
        ("SELECT $1::text[]", Type::TEXT_ARRAY),
        ("SELECT id FROM c3a WHERE id = ANY($1)", Type::INT8_ARRAY),
    ] {
        let statement = client.prepare(sql).await.expect(sql);
        assert_eq!(statement.params(), &[want], "{sql}");
    }
    // What the Proxy's transaction replay does with it: bind a bigint[] without declaring it.
    client
        .batch_execute("INSERT INTO c3a VALUES (1), (2), (3)")
        .await
        .expect("rows");
    let ids: Vec<i64> = vec![1, 3];
    let found = client
        .query("SELECT id FROM c3a WHERE id = ANY($1) ORDER BY id", &[&ids])
        .await
        .expect("bind bigint[]");
    assert_eq!(found.len(), 2);
}

// ── 4. Session restore ──────────────────────────────────────────────────────

#[tokio::test]
async fn item4_set_config_and_discard_all() {
    let port = server().await;
    let client = connect(port).await;
    let set = rows(
        &client
            .simple_query("SELECT pg_catalog.set_config('application_name', 'restored', false)")
            .await
            .expect("set_config"),
    );
    assert_eq!(set[0][0].as_deref(), Some("restored"));
    let shown = rows(&client.simple_query("SHOW application_name").await.expect("SHOW"));
    assert_eq!(shown[0][0].as_deref(), Some("restored"));
    let local = rows(
        &client
            .simple_query("SELECT set_config('search_path', 'public', true)")
            .await
            .expect("set_config local"),
    );
    assert_eq!(local[0][0].as_deref(), Some("public"));
    let tags = tokio::task::spawn_blocking(move || command_tags(port, "DISCARD ALL"))
        .await
        .expect("join");
    assert_eq!(tags, ["DISCARD ALL"]);
}

#[tokio::test]
#[ignore = "5.0 package sql-std: EXECUTE returns no rows; PREPARE answers OK 0; no pg_prepared_statements"]
async fn item4_sql_prepare_execute() {
    let port = server().await;
    let client = connect(port).await;
    client
        .batch_execute("CREATE TABLE c4 (id BIGINT PRIMARY KEY, v TEXT); INSERT INTO c4 VALUES (1, 'a'), (2, 'b')")
        .await
        .expect("setup");
    client
        .batch_execute("PREPARE p4(bigint) AS SELECT v FROM c4 WHERE id = $1")
        .await
        .expect("PREPARE");
    let executed = rows(&client.simple_query("EXECUTE p4(2)").await.expect("EXECUTE"));
    assert_eq!(executed, vec![vec![Some("b".to_string())]]);
    let listed = rows(
        &client
            .simple_query("SELECT name FROM pg_prepared_statements")
            .await
            .expect("pg_prepared_statements"),
    );
    assert_eq!(listed, vec![vec![Some("p4".to_string())]]);
    let tags = tokio::task::spawn_blocking(move || {
        command_tags(
            port,
            "PREPARE p5(bigint) AS SELECT 1 FROM c4 WHERE id = $1; EXECUTE p5(1); DEALLOCATE p5",
        )
    })
    .await
    .expect("join");
    assert_eq!(tags, ["PREPARE", "SELECT 1", "DEALLOCATE"]);
}

#[tokio::test]
#[ignore = "5.0 package pgwire-rest: RESET ALL answers the command tag OK 0"]
async fn item4_reset_all_command_tag() {
    let port = server().await;
    let tags = tokio::task::spawn_blocking(move || command_tags(port, "RESET ALL"))
        .await
        .expect("join");
    assert_eq!(tags, ["RESET"]);
}

// ── 6. SQL text read as PostgreSQL reads it ─────────────────────────────────

#[tokio::test]
async fn item6_statement_splitter_knows_quotes_and_comments() {
    let client = connect(server().await).await;
    let cases: [(&str, Vec<Vec<Option<String>>>); 6] = [
        (
            "SELECT $$a;b$$ AS x; SELECT 2 AS y",
            vec![vec![Some("a;b".into())], vec![Some("2".into())]],
        ),
        (
            "SELECT $t$ it's ; $t$ AS x; SELECT 3",
            vec![vec![Some(" it's ; ".into())], vec![Some("3".into())]],
        ),
        (
            "SELECT 'a;b' AS x; SELECT 4",
            vec![vec![Some("a;b".into())], vec![Some("4".into())]],
        ),
        ("/* x; y */ SELECT 5", vec![vec![Some("5".into())]]),
        (
            "SELECT E'it\\'s;' AS x; SELECT 6",
            vec![vec![Some("it's;".into())], vec![Some("6".into())]],
        ),
        (
            "SELECT $$--$$ AS a, '/*' AS b, '*/' AS c",
            vec![vec![Some("--".into()), Some("/*".into()), Some("*/".into())]],
        ),
    ];
    for (sql, want) in cases {
        assert_eq!(rows(&client.simple_query(sql).await.expect(sql)), want, "{sql}");
    }
    let literal = rows(&client.simple_query("SELECT 'a\\nb'").await.expect("literal"));
    assert_eq!(
        literal[0][0].as_deref(),
        Some("a\\nb"),
        "standard_conforming_strings = on"
    );
}

#[tokio::test]
#[ignore = "5.0 package sql-correct: a nested block comment after the first token ends at the first */"]
async fn item6_nested_block_comment_inside_a_statement() {
    let client = connect(server().await).await;
    for sql in ["SELECT 1 /* a /* b */ ; c */ AS z", "SELECT 1 AS z /* x /* y */ z */"] {
        let got = rows(&client.simple_query(sql).await.expect(sql));
        assert_eq!(got, vec![vec![Some("1".to_string())]], "{sql}");
    }
}
