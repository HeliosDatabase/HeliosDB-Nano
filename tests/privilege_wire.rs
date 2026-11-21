//! Privileges over the wires: the PostgreSQL simple and extended protocols,
//! prepared statements, `COPY`, `SET ROLE`, and the MySQL listener's own codes
//! and wording.
//!
//! The listeners authenticate with `trust` here: the role is the name the
//! client sent, as in PostgreSQL. A name that is a role in the catalog is
//! checked as that role.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::many_single_char_names)]

use heliosdb_nano::protocol::mysql::MySqlHandler;
use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::EmbeddedDatabase;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::time::timeout;
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

const TIMEOUT: Duration = Duration::from_secs(15);

fn database() -> Arc<EmbeddedDatabase> {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE orders (id INT PRIMARY KEY, customer TEXT, total INT)")
        .unwrap();
    db.execute("INSERT INTO orders VALUES (1, 'ann', 10), (2, 'bob', 20)").unwrap();
    db.execute("CREATE TABLE secrets (id INT PRIMARY KEY, body TEXT)").unwrap();
    db.execute("INSERT INTO secrets VALUES (1, 'classified')").unwrap();
    db.execute("CREATE ROLE reader LOGIN").unwrap();
    db.execute("GRANT SELECT ON orders TO reader").unwrap();
    Arc::new(db)
}

async fn serve(db: Arc<EmbeddedDatabase>) -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("address");
    drop(listener);
    let server = PgServer::new(PgServerConfig::with_address(addr), db).expect("server");
    tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    addr
}

async fn connect(addr: SocketAddr, user: &str) -> Client {
    let conn = format!("host=127.0.0.1 port={} user={user} dbname=postgres", addr.port());
    let (client, connection) = timeout(TIMEOUT, tokio_postgres::connect(&conn, NoTls))
        .await
        .expect("connect timeout")
        .expect("connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// SQLSTATE and message of a refused statement.
fn refusal(error: &tokio_postgres::Error) -> (String, String) {
    let db_error = error
        .as_db_error()
        .unwrap_or_else(|| panic!("expected a server error, got: {error}"));
    (db_error.code().code().to_string(), db_error.message().to_string())
}

fn assert_42501<T: std::fmt::Debug>(result: Result<T, tokio_postgres::Error>, expected: &str, context: &str) {
    match result {
        Ok(value) => panic!("{context}: must be refused, but returned {value:?}"),
        Err(error) => {
            let (code, message) = refusal(&error);
            assert_eq!(code, "42501", "{context}: SQLSTATE (message: {message})");
            assert!(message.contains(expected), "{context}: expected `{expected}`, got: {message}");
        }
    }
}

async fn simple_rows(client: &Client, sql: &str) -> Result<usize, tokio_postgres::Error> {
    let messages = timeout(TIMEOUT, client.simple_query(sql)).await.expect("query timeout")?;
    Ok(messages
        .iter()
        .filter(|message| matches!(message, SimpleQueryMessage::Row(_)))
        .count())
}

async fn simple_scalar(client: &Client, sql: &str) -> String {
    let messages = timeout(TIMEOUT, client.simple_query(sql))
        .await
        .expect("query timeout")
        .unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    for message in messages {
        if let SimpleQueryMessage::Row(row) = message {
            return row.get(0).unwrap_or_default().to_string();
        }
    }
    panic!("`{sql}` returned no row");
}

// ---------------------------------------------------------------------------
// PostgreSQL wire
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn simple_protocol_refuses_with_42501_and_postgres_wording() {
    let addr = serve(database()).await;
    let reader = connect(addr, "reader").await;
    assert_eq!(simple_rows(&reader, "SELECT * FROM orders").await.unwrap(), 2);
    assert_eq!(simple_rows(&reader, "SELECT total FROM orders WHERE id = 1").await.unwrap(), 1);
    for sql in [
        "SELECT * FROM secrets",
        "SELECT body FROM secrets WHERE id = 1",
        "SELECT * FROM orders WHERE id IN (SELECT id FROM secrets)",
    ] {
        assert_42501(simple_rows(&reader, sql).await, "permission denied for table secrets", sql);
    }
    for sql in [
        "INSERT INTO orders VALUES (3, 'cy', 30)",
        "UPDATE orders SET total = 0 WHERE id = 1",
        "DELETE FROM orders WHERE id = 1",
        "TRUNCATE orders",
    ] {
        assert_42501(simple_rows(&reader, sql).await, "permission denied for table orders", sql);
    }
    assert_42501(
        simple_rows(&reader, "DROP TABLE orders").await,
        "must be owner of table orders",
        "DROP TABLE",
    );
    assert_42501(
        simple_rows(&reader, "CREATE TABLE mine (id INT)").await,
        "permission denied for schema public",
        "CREATE TABLE",
    );
    // Several statements in one message: each is checked.
    assert_42501(
        simple_rows(&reader, "SELECT 1; SELECT * FROM secrets").await,
        "permission denied for table secrets",
        "the second statement of a multi-statement message",
    );
    // The connection is still usable, and nothing was changed.
    assert_eq!(simple_rows(&reader, "SELECT * FROM orders").await.unwrap(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn extended_protocol_is_checked_like_the_simple_one() {
    let addr = serve(database()).await;
    let reader = connect(addr, "reader").await;
    let rows = reader
        .query("SELECT total FROM orders WHERE id = $1", &[&1i32])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_42501(
        reader
            .query("SELECT body FROM secrets WHERE id = $1", &[&1i32])
            .await
            .map(|rows| rows.len()),
        "permission denied for table secrets",
        "an extended-protocol SELECT",
    );
    assert_42501(
        reader
            .execute("INSERT INTO orders VALUES ($1, $2, $3)", &[&9i32, &"x", &1i32])
            .await,
        "permission denied for table orders",
        "an extended-protocol INSERT",
    );
    assert_42501(
        reader.execute("UPDATE orders SET total = $1 WHERE id = $2", &[&1i32, &1i32]).await,
        "permission denied for table orders",
        "an extended-protocol UPDATE",
    );
    assert_42501(
        reader.execute("DELETE FROM orders WHERE id = $1", &[&1i32]).await,
        "permission denied for table orders",
        "an extended-protocol DELETE",
    );
    assert_eq!(reader.query("SELECT * FROM orders", &[]).await.unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prepared_statement_loses_access_after_revoke() {
    let db = database();
    let addr = serve(Arc::clone(&db)).await;
    let reader = connect(addr, "reader").await;
    let admin = connect(addr, "postgres").await;
    let by_id = reader.prepare("SELECT total FROM orders WHERE id = $1").await.unwrap();
    let all = reader.prepare("SELECT * FROM orders").await.unwrap();
    for _ in 0..4 {
        assert_eq!(reader.query(&by_id, &[&1i32]).await.unwrap().len(), 1);
        assert_eq!(reader.query(&all, &[]).await.unwrap().len(), 2);
    }
    admin.simple_query("REVOKE SELECT ON orders FROM reader").await.unwrap();
    assert_42501(
        reader.query(&by_id, &[&1i32]).await.map(|rows| rows.len()),
        "permission denied for table orders",
        "the prepared primary-key read after REVOKE",
    );
    assert_42501(
        reader.query(&all, &[]).await.map(|rows| rows.len()),
        "permission denied for table orders",
        "the prepared scan after REVOKE",
    );
    // And gets it back with the grant, on the same prepared statements.
    admin.simple_query("GRANT SELECT ON orders TO reader").await.unwrap();
    assert_eq!(reader.query(&by_id, &[&1i32]).await.unwrap().len(), 1);
    assert_eq!(reader.query(&all, &[]).await.unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn revoke_on_one_connection_reaches_another_at_its_next_statement() {
    let addr = serve(database()).await;
    let reader = connect(addr, "reader").await;
    let admin = connect(addr, "postgres").await;
    for _ in 0..6 {
        assert_eq!(simple_rows(&reader, "SELECT * FROM orders").await.unwrap(), 2);
        assert_eq!(simple_rows(&reader, "SELECT total FROM orders WHERE id = 2").await.unwrap(), 1);
    }
    admin.simple_query("REVOKE SELECT ON orders FROM reader").await.unwrap();
    assert_42501(
        simple_rows(&reader, "SELECT * FROM orders").await,
        "permission denied for table orders",
        "the cached scan",
    );
    assert_42501(
        simple_rows(&reader, "SELECT total FROM orders WHERE id = 2").await,
        "permission denied for table orders",
        "the cached primary-key read",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cached_answer_is_not_served_across_roles_on_the_wire() {
    let addr = serve(database()).await;
    let admin = connect(addr, "postgres").await;
    let reader = connect(addr, "reader").await;
    for sql in ["SELECT * FROM secrets", "SELECT body FROM secrets WHERE id = 1"] {
        for _ in 0..6 {
            assert_eq!(simple_rows(&admin, sql).await.unwrap(), 1);
        }
        assert_42501(simple_rows(&reader, sql).await, "permission denied for table secrets", sql);
        assert_42501(
            reader.query(sql, &[]).await.map(|rows| rows.len()),
            "permission denied for table secrets",
            sql,
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn grants_can_be_issued_through_the_extended_protocol() {
    let db = database();
    db.execute("CREATE ROLE auditors").unwrap();
    db.execute("CREATE SEQUENCE ticket_no").unwrap();
    let addr = serve(db).await;
    let admin = connect(addr, "postgres").await;
    let reader = connect(addr, "reader").await;
    // Statements the SQL parser of the server has no grammar for arrive as
    // Parse / Bind / Execute from every driver that prepares server-side.
    for sql in [
        "GRANT SELECT (id) ON secrets TO reader",
        "GRANT auditors TO reader",
        "GRANT USAGE ON SEQUENCE ticket_no TO reader GRANTED BY postgres",
        "GRANT USAGE ON SCHEMA public TO auditors",
        "ALTER TABLE secrets OWNER TO auditors",
    ] {
        admin.execute(sql, &[]).await.unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    }
    // `reader` inherits ownership of `secrets` through `auditors`.
    assert_eq!(reader.query("SELECT * FROM secrets", &[]).await.unwrap().len(), 1);
    assert_eq!(simple_rows(&reader, "SELECT nextval('ticket_no')").await.unwrap(), 1);
    admin.execute("REVOKE auditors FROM reader", &[]).await.unwrap();
    assert_eq!(reader.query("SELECT id FROM secrets", &[]).await.unwrap().len(), 1, "the column grant");
    assert_42501(
        reader.query("SELECT body FROM secrets", &[]).await.map(|rows| rows.len()),
        "permission denied for table secrets",
        "a column that was not granted",
    );
    let refused = admin
        .execute("ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO reader", &[])
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused).0, "0A000");
}

#[tokio::test(flavor = "multi_thread")]
async fn trust_keeps_working_for_postgres_and_for_names_that_are_not_roles() {
    let addr = serve(database()).await;
    for user in ["postgres", "some_application"] {
        let client = connect(addr, user).await;
        assert_eq!(simple_rows(&client, "SELECT * FROM secrets").await.unwrap(), 1, "{user}");
        client
            .simple_query("CREATE TABLE IF NOT EXISTS scratch (id INT PRIMARY KEY)")
            .await
            .unwrap_or_else(|e| panic!("{user}: {e}"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn set_role_over_the_wire_switches_and_is_never_a_bare_acknowledgement() {
    let db = database();
    db.execute("CREATE ROLE auditors").unwrap();
    db.execute("GRANT SELECT ON secrets TO auditors").unwrap();
    db.execute("GRANT auditors TO reader").unwrap();
    db.execute("ALTER ROLE reader NOINHERIT").unwrap();
    db.execute("CREATE ROLE stranger").unwrap();
    let addr = serve(db).await;
    let reader = connect(addr, "reader").await;

    assert_42501(
        simple_rows(&reader, "SELECT * FROM secrets").await,
        "permission denied for table secrets",
        "before SET ROLE",
    );
    reader.simple_query("SET ROLE auditors").await.unwrap();
    assert_eq!(simple_scalar(&reader, "SELECT current_user").await, "auditors");
    assert_eq!(simple_scalar(&reader, "SELECT session_user").await, "reader");
    assert_eq!(simple_rows(&reader, "SELECT * FROM secrets").await.unwrap(), 1);
    assert_eq!(reader.query("SELECT * FROM secrets", &[]).await.unwrap().len(), 1);

    assert_42501(
        reader.simple_query("SET ROLE stranger").await.map(|m| m.len()),
        "permission denied to set role \"stranger\"",
        "a role the session may not become",
    );
    let missing = reader.simple_query("SET ROLE nobody_here").await.map(|m| m.len()).unwrap_err();
    assert_eq!(refusal(&missing).0, "42704", "an unknown role is undefined_object");
    assert_42501(
        reader.simple_query("SET SESSION AUTHORIZATION postgres").await.map(|m| m.len()),
        "permission denied to set session authorization",
        "SET SESSION AUTHORIZATION by an ordinary role",
    );
    // The refusals changed nothing.
    assert_eq!(simple_scalar(&reader, "SELECT current_user").await, "auditors");

    reader.simple_query("RESET ROLE").await.unwrap();
    assert_eq!(simple_scalar(&reader, "SELECT current_user").await, "reader");
    assert_42501(
        simple_rows(&reader, "SELECT * FROM secrets").await,
        "permission denied for table secrets",
        "after RESET ROLE",
    );
    // Through the extended protocol as well.
    reader.execute("SET ROLE auditors", &[]).await.unwrap();
    assert_eq!(simple_rows(&reader, "SELECT * FROM secrets").await.unwrap(), 1);
    reader.simple_query("SET ROLE NONE").await.unwrap();
    assert_eq!(simple_scalar(&reader, "SELECT current_user").await, "reader");
}

#[tokio::test(flavor = "multi_thread")]
async fn copy_needs_insert_to_load_and_select_to_unload() {
    use futures::SinkExt;
    let db = database();
    db.execute("CREATE ROLE loader LOGIN").unwrap();
    let addr = serve(Arc::clone(&db)).await;
    let reader = connect(addr, "reader").await;

    // COPY … FROM STDIN: `reader` holds SELECT only.
    let outcome = async {
        let sink = reader.copy_in::<_, bytes::Bytes>("COPY orders (id, customer, total) FROM STDIN").await?;
        futures::pin_mut!(sink);
        sink.send(bytes::Bytes::from_static(b"7\tzed\t70\n")).await?;
        sink.finish().await
    }
    .await;
    assert_42501(outcome, "permission denied for table orders", "COPY FROM without INSERT");
    assert_eq!(
        db.query("SELECT count(*) FROM orders", &[]).unwrap()[0].values[0],
        heliosdb_nano::Value::Int8(2),
        "the refused COPY loaded nothing"
    );

    // COPY … TO STDOUT of a table the role may not read.
    let unload = reader.copy_out("COPY secrets TO STDOUT").await;
    let refused = match unload {
        Err(error) => error,
        Ok(stream) => {
            use futures::StreamExt;
            futures::pin_mut!(stream);
            let mut failure = None;
            while let Some(chunk) = stream.next().await {
                if let Err(error) = chunk {
                    failure = Some(error);
                    break;
                }
            }
            failure.expect("COPY secrets TO STDOUT must be refused")
        }
    };
    let (code, message) = refusal(&refused);
    assert_eq!(code, "42501", "{message}");
    assert!(message.contains("permission denied for table secrets"), "{message}");

    // With INSERT, the load goes through.
    db.execute("GRANT INSERT ON orders TO loader").unwrap();
    let loader = connect(addr, "loader").await;
    let sink = loader
        .copy_in::<_, bytes::Bytes>("COPY orders (id, customer, total) FROM STDIN")
        .await
        .unwrap();
    futures::pin_mut!(sink);
    sink.send(bytes::Bytes::from_static(b"7\tzed\t70\n")).await.unwrap();
    assert_eq!(sink.finish().await.unwrap(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_can_be_revoked_from_a_role() {
    let db = database();
    db.execute("REVOKE CONNECT ON DATABASE postgres FROM PUBLIC").unwrap();
    db.execute("CREATE ROLE welcome LOGIN").unwrap();
    db.execute("GRANT CONNECT ON DATABASE postgres TO welcome").unwrap();
    let addr = serve(db).await;
    let conn = format!("host=127.0.0.1 port={} user=reader dbname=postgres", addr.port());
    let refused = timeout(TIMEOUT, tokio_postgres::connect(&conn, NoTls))
        .await
        .expect("connect timeout")
        .err()
        .expect("a role without CONNECT must not get a session");
    let (code, message) = refusal(&refused);
    assert_eq!(code, "42501", "{message}");
    assert!(message.contains("permission denied for database postgres"), "{message}");
    // A role that holds CONNECT, and the bootstrap superuser, still connect.
    let welcome = connect(addr, "welcome").await;
    assert_eq!(simple_rows(&welcome, "SELECT 1").await.unwrap(), 1);
    let admin = connect(addr, "postgres").await;
    assert_eq!(simple_rows(&admin, "SELECT 1").await.unwrap(), 1);
}

// ---------------------------------------------------------------------------
// MySQL wire
// ---------------------------------------------------------------------------

const COM_QUERY: u8 = 0x03;
const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;

struct MySqlClient {
    stream: DuplexStream,
}

/// What the server answered a COM_QUERY with.
#[derive(Debug, PartialEq, Eq)]
enum MySqlAnswer {
    Ok,
    Rows(usize),
    Error { code: u16, state: String, message: String },
}

impl MySqlClient {
    async fn login(db: Arc<EmbeddedDatabase>, user: &str) -> Self {
        let (client, server) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move {
            let _ = MySqlHandler::handle_connection(db, server, 1).await;
        });
        let mut this = Self { stream: client };
        let _greeting = this.read_packet().await;
        let mut p = Vec::new();
        p.extend_from_slice(&(CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION).to_le_bytes());
        p.extend_from_slice(&(1u32 << 24).to_le_bytes());
        p.push(45);
        p.extend_from_slice(&[0u8; 23]);
        p.extend_from_slice(user.as_bytes());
        p.push(0);
        p.push(0);
        this.write_packet(1, &p).await;
        let ok = this.read_packet().await;
        assert_eq!(ok[0], 0x00, "expected auth OK");
        this
    }

    async fn write_packet(&mut self, seq: u8, payload: &[u8]) {
        let len = payload.len();
        let header = [(len & 0xFF) as u8, ((len >> 8) & 0xFF) as u8, ((len >> 16) & 0xFF) as u8, seq];
        self.stream.write_all(&header).await.unwrap();
        self.stream.write_all(payload).await.unwrap();
        self.stream.flush().await.unwrap();
    }

    async fn read_packet(&mut self) -> Vec<u8> {
        let mut header = [0u8; 4];
        timeout(TIMEOUT, self.stream.read_exact(&mut header))
            .await
            .expect("read timeout")
            .unwrap();
        let len = (header[0] as usize) | ((header[1] as usize) << 8) | ((header[2] as usize) << 16);
        let mut payload = vec![0u8; len];
        self.stream.read_exact(&mut payload).await.unwrap();
        payload
    }

    async fn query(&mut self, sql: &str) -> MySqlAnswer {
        let mut p = vec![COM_QUERY];
        p.extend_from_slice(sql.as_bytes());
        self.write_packet(0, &p).await;
        let first = self.read_packet().await;
        match first[0] {
            0x00 => MySqlAnswer::Ok,
            0xFF => {
                let code = u16::from_le_bytes([first[1], first[2]]);
                // '#' + five bytes of SQLSTATE, then the message.
                let state = String::from_utf8_lossy(&first[4..9]).into_owned();
                let message = String::from_utf8_lossy(&first[9..]).into_owned();
                MySqlAnswer::Error { code, state, message }
            }
            columns => {
                for _ in 0..columns {
                    let _definition = self.read_packet().await;
                }
                let eof = self.read_packet().await;
                assert_eq!(eof[0], 0xFE, "expected EOF after the column definitions");
                let mut rows = 0;
                loop {
                    let packet = self.read_packet().await;
                    if packet[0] == 0xFE && packet.len() < 9 {
                        return MySqlAnswer::Rows(rows);
                    }
                    rows += 1;
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_refuses_with_its_own_codes_and_wording() {
    let db = database();
    db.execute("CREATE SCHEMA vault").unwrap();
    db.execute("CREATE TABLE vault.keys (id INT PRIMARY KEY)").unwrap();
    db.execute("GRANT SELECT ON vault.keys TO reader").unwrap();
    let mut reader = MySqlClient::login(Arc::clone(&db), "reader").await;

    assert_eq!(reader.query("SELECT * FROM orders").await, MySqlAnswer::Rows(2));
    assert_eq!(
        reader.query("SELECT * FROM secrets").await,
        MySqlAnswer::Error {
            code: 1142,
            state: "42000".to_string(),
            message: "SELECT command denied to user 'reader'@'%' for table 'secrets'".to_string(),
        }
    );
    assert_eq!(
        reader.query("INSERT INTO orders VALUES (3, 'cy', 30)").await,
        MySqlAnswer::Error {
            code: 1142,
            state: "42000".to_string(),
            message: "INSERT command denied to user 'reader'@'%' for table 'orders'".to_string(),
        }
    );
    assert_eq!(
        reader.query("UPDATE orders SET total = 0 WHERE id = 1").await,
        MySqlAnswer::Error {
            code: 1142,
            state: "42000".to_string(),
            message: "UPDATE command denied to user 'reader'@'%' for table 'orders'".to_string(),
        }
    );
    assert_eq!(
        reader.query("DELETE FROM orders WHERE id = 1").await,
        MySqlAnswer::Error {
            code: 1142,
            state: "42000".to_string(),
            message: "DELETE command denied to user 'reader'@'%' for table 'orders'".to_string(),
        }
    );
    assert_eq!(
        reader.query("DROP TABLE orders").await,
        MySqlAnswer::Error {
            code: 1142,
            state: "42000".to_string(),
            message: "DROP command denied to user 'reader'@'%' for table 'orders'".to_string(),
        }
    );
    // A schema is a database to MySQL.
    assert_eq!(
        reader.query("SELECT * FROM vault.keys").await,
        MySqlAnswer::Error {
            code: 1044,
            state: "42000".to_string(),
            message: "Access denied for user 'reader'@'%' to database 'vault'".to_string(),
        }
    );
    match reader.query("CREATE ROLE helper").await {
        MySqlAnswer::Error { code, state, .. } => assert_eq!((code, state.as_str()), (1227, "42000")),
        other => panic!("CREATE ROLE by an ordinary role must be refused, got {other:?}"),
    }
    // Nothing was written, and the grant takes effect on the open connection.
    assert_eq!(
        db.query("SELECT count(*) FROM orders", &[]).unwrap()[0].values[0],
        heliosdb_nano::Value::Int8(2)
    );
    db.execute("GRANT SELECT ON secrets TO reader").unwrap();
    assert_eq!(reader.query("SELECT * FROM secrets").await, MySqlAnswer::Rows(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_root_and_other_names_that_are_not_roles_keep_full_access() {
    let db = database();
    let mut root = MySqlClient::login(Arc::clone(&db), "root").await;
    assert_eq!(root.query("SELECT * FROM secrets").await, MySqlAnswer::Rows(1));
    assert_eq!(root.query("INSERT INTO orders VALUES (3, 'cy', 30)").await, MySqlAnswer::Ok);
}
