//! HDB-009 — the SQL identity functions report the session's real login, not a
//! hard-coded principal.
//!
//! WHAT WAS BROKEN. `current_user` and `session_user` were the literal
//! `"heliosdb"` for every connection, and `current_setting('session_authorization')`
//! was a *different* literal, `"postgres"` — so the two contradicted each other
//! and neither had anything to do with who was connected. The engine already
//! stored the login name on the session (`Session.login_user`, used to expand a
//! `"$user"` search_path), but the expression evaluator has no session handle,
//! so the value never reached SQL. `current_role` was not answered at all.
//!
//! Anything that audits or filters on identity was therefore wrong in the unsafe
//! direction: `DEFAULT current_user` stamped one name on every row whoever wrote
//! it, and `WHERE owner = current_user` either matched everything or nothing.
//!
//! THE FIX, and what each case below pins:
//!   * the identity travels to the storage-less evaluator through the same kind
//!     of per-statement thread-local as `current_schema()` and the advisory-lock
//!     context, installed by every session entry point (cases 1-4);
//!   * the PG wire publishes the login name only AFTER authentication succeeds,
//!     and refuses a startup packet with no `user` at all (cases 5-7);
//!   * the MySQL wire publishes its handshake user (case 8 — trust-only
//!     listener: the name is asserted by the client, never proved);
//!   * the shared result cache is keyed by principal, so an answer computed for
//!     alice is never served to bob — including when the identity is hidden
//!     inside a view or a SQL UDF and the two SQL texts are byte-identical
//!     (cases 3 and 6 repeat their queries to cross the cache's admission
//!     threshold; that is the whole point of the repetition).
//!
//!   * the COPY FAST path stamps the session identity into a
//!     `DEFAULT current_user` column exactly like the generic path (case 9 —
//!     it evaluates the DEFAULT itself, outside every `_for_session` entry
//!     point, so it needed its own identity wrapper);
//!   * a client-asserted login is truncated at PostgreSQL's 63-byte identifier
//!     limit before it is published (case 10);
//!   * the PG simple-query handler's own top-level result-cache probe runs
//!     under the session's identity, so rows computed session-less on a SHARED
//!     handle (REST / MCP / an embedder's `db.query()`) are never served to an
//!     authenticated wire client (case 11 — that probe sits outside every
//!     `_for_session` entry point, so it had no principal installed).
//!
//! This is identity REPORTING. `SET ROLE` / `SET SESSION AUTHORIZATION` stay
//! refused (with 0A000 on the PG wire in the default configuration) and no
//! privilege decision is made from these functions; see
//! `docs/guides/authentication.md`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::protocol::mysql::MySqlHandler;
use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::{EmbeddedDatabase, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

const IO_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);

/// The three spellings a client can ask "who am I" with, in one statement.
const IDENTITY_QUERY: &str = "SELECT current_user, session_user, current_setting('session_authorization')";

// ===========================================================================
// Embedded helpers
// ===========================================================================

fn db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory db")
}

fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => panic!("expected a text value, got {other:?}"),
    }
}

/// The single row `sql` must produce on `session`, as text.
fn session_row(db: &EmbeddedDatabase, session: SessionId, sql: &str) -> Vec<String> {
    let rows = db
        .query_in_session(session, sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
    assert_eq!(rows.len(), 1, "`{sql}` must return exactly one row");
    rows[0].values.iter().map(text).collect()
}

/// The single row `sql` must produce on the session-less embedded handle.
fn embedded_row(db: &EmbeddedDatabase, sql: &str) -> Vec<String> {
    let rows = db.query(sql, &[]).unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
    assert_eq!(rows.len(), 1, "`{sql}` must return exactly one row");
    rows[0].values.iter().map(text).collect()
}

// ===========================================================================
// 1. Control — the embedded API has no login identity
// ===========================================================================

#[test]
fn hdb009_embedded_default_identity_is_the_service_user() {
    let db = db();
    assert_eq!(
        embedded_row(&db, IDENTITY_QUERY),
        vec!["heliosdb".to_string(); 3],
        "a session-less embedded call has no login identity and must report the \
         documented service user — identically for all three spellings (before \
         the fix `session_authorization` answered `postgres`)"
    );
}

// ===========================================================================
// 2. Every named session sees its own identity
// ===========================================================================

#[test]
fn hdb009_named_sessions_see_their_own_identity() {
    let db = db();
    let alice = db.create_session("alice", IsolationLevel::ReadCommitted).unwrap();
    let bob = db.create_session("bob", IsolationLevel::ReadCommitted).unwrap();

    // Interleaved, and each query repeated, so a cached first answer would be
    // served to the other principal on the next round.
    for _ in 0..2 {
        for (session, expected) in [(alice, "alice"), (bob, "bob")] {
            assert_eq!(
                session_row(&db, session, IDENTITY_QUERY),
                vec![expected.to_string(); 3],
                "{expected}'s session must see {expected} in all three spellings"
            );
            assert_eq!(
                session_row(&db, session, "SELECT current_role()"),
                vec![expected.to_string()],
                "current_role must answer the same identity"
            );
            // The BARE spelling is the one PostgreSQL's docs (and ours) show,
            // and the one spelling sqlparser does not lower to a function — so
            // it is lowered in the planner and has to be pinned separately.
            assert_eq!(
                session_row(&db, session, "SELECT current_role"),
                vec![expected.to_string()],
                "bare `current_role` (no parentheses) must answer the identity, not `column not found`"
            );
        }
    }

    // The session-less handle on the same database is unaffected.
    assert_eq!(embedded_row(&db, IDENTITY_QUERY), vec!["heliosdb".to_string(); 3]);

    db.destroy_session(alice).unwrap();
    db.destroy_session(bob).unwrap();
}

// ===========================================================================
// 3. Identity hidden inside a UDF and a view is never served across sessions
// ===========================================================================

#[test]
fn hdb009_identity_hidden_in_a_udf_and_a_view_is_not_served_across_sessions() {
    let db = db();
    // Unique names: UDF resolution is process-global (see
    // tests/udf_invocation_tests.rs) and `cargo test` runs these in parallel.
    db.execute("CREATE TABLE hdb009_one (id INT)").unwrap();
    db.execute("INSERT INTO hdb009_one VALUES (1)").unwrap();
    db.execute("CREATE FUNCTION hdb009_whoami() RETURNS TEXT AS $$ SELECT current_user $$ LANGUAGE sql")
        .unwrap();
    db.execute("CREATE VIEW hdb009_who AS SELECT current_user AS u FROM hdb009_one")
        .unwrap();

    let alice = db.create_session("alice", IsolationLevel::ReadCommitted).unwrap();
    let bob = db.create_session("bob", IsolationLevel::ReadCommitted).unwrap();

    // Neither statement contains an identity token, so the SQL text alone
    // cannot tell the two principals apart — which is exactly what the shared
    // result cache used to key on. Four rounds each: rounds 2+ are the ones a
    // text-keyed cache would answer from another principal's entry.
    for _ in 0..4 {
        for (session, expected) in [(alice, "alice"), (bob, "bob")] {
            assert_eq!(
                session_row(&db, session, "SELECT hdb009_whoami()"),
                vec![expected.to_string()],
                "a UDF body hiding current_user must still answer the CALLING session"
            );
            assert_eq!(
                session_row(&db, session, "SELECT u FROM hdb009_who"),
                vec![expected.to_string()],
                "a view hiding current_user must still answer the READING session"
            );
        }
    }

    db.destroy_session(alice).unwrap();
    db.destroy_session(bob).unwrap();
}

// ===========================================================================
// 4. A column DEFAULT stamps the writing session's identity
// ===========================================================================

#[test]
fn hdb009_column_default_uses_the_session_identity() {
    let db = db();
    db.execute("CREATE TABLE hdb009_audit (id INT, who TEXT DEFAULT current_user)")
        .unwrap();

    let alice = db.create_session("alice", IsolationLevel::ReadCommitted).unwrap();
    let bob = db.create_session("bob", IsolationLevel::ReadCommitted).unwrap();
    db.execute_in_session(alice, "INSERT INTO hdb009_audit (id) VALUES (1)")
        .unwrap();
    db.execute_in_session(bob, "INSERT INTO hdb009_audit (id) VALUES (2)")
        .unwrap();

    let rows = db.query("SELECT who FROM hdb009_audit ORDER BY id", &[]).unwrap();
    let who: Vec<String> = rows.iter().map(|r| text(&r.values[0])).collect();
    assert_eq!(
        who,
        vec!["alice".to_string(), "bob".to_string()],
        "DEFAULT current_user must record WHO wrote each row, not one hard-coded name"
    );

    db.destroy_session(alice).unwrap();
    db.destroy_session(bob).unwrap();
}

// ===========================================================================
// PG wire harness
// ===========================================================================

async fn setup_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    setup_server_on(Arc::new(EmbeddedDatabase::new_in_memory().expect("db"))).await
}

/// The same server, on a handle the CALLER also keeps — the shipped layout
/// (`src/main.rs` hands one `Arc<EmbeddedDatabase>` to `PgServer` and to
/// `ApiServer::from_config`), so a test can run SQL session-less against the
/// very database the wire is serving.
async fn setup_server_on(db: Arc<EmbeddedDatabase>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test port");
    let addr = listener.local_addr().expect("test addr");
    drop(listener);

    let config = PgServerConfig::with_address(addr);
    let server = PgServer::new(config, db).expect("server");
    let handle = tokio::spawn(async move {
        let _ = server.serve().await;
    });

    tokio::time::sleep(Duration::from_millis(150)).await;
    (addr, handle)
}

/// Connect as `user`. `dbname` is always the reserved `postgres`: with no
/// `database` parameter the startup check falls back to the user name, which is
/// not a database.
async fn connect_as(addr: SocketAddr, user: &str) -> Client {
    let conn_string = format!("host=127.0.0.1 port={} user={user} dbname=postgres", addr.port());
    let (client, connection) = timeout(CONNECT_TIMEOUT, tokio_postgres::connect(&conn_string, NoTls))
        .await
        .expect("connect timeout")
        .expect("connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// Every column of the first row `sql` returns, as text.
async fn simple_row(client: &Client, sql: &str) -> Vec<String> {
    let messages = timeout(QUERY_TIMEOUT, client.simple_query(sql))
        .await
        .expect("query timeout")
        .unwrap_or_else(|e| panic!("`{sql}` must succeed: {e}"));
    messages
        .into_iter()
        .find_map(|message| match message {
            SimpleQueryMessage::Row(row) => Some(
                (0..row.columns().len())
                    .map(|i| row.get(i).unwrap_or("").to_string())
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_else(|| panic!("`{sql}` returned no row"))
}

/// The FIRST column of every row `sql` returns, as text.
async fn simple_column(client: &Client, sql: &str) -> Vec<String> {
    let messages = timeout(QUERY_TIMEOUT, client.simple_query(sql))
        .await
        .expect("query timeout")
        .unwrap_or_else(|e| panic!("`{sql}` must succeed: {e}"));
    messages
        .into_iter()
        .filter_map(|message| match message {
            SimpleQueryMessage::Row(row) => Some(row.get(0).unwrap_or("").to_string()),
            _ => None,
        })
        .collect()
}

/// How many data rows `sql` returns.
async fn simple_row_count(client: &Client, sql: &str) -> usize {
    let messages = timeout(QUERY_TIMEOUT, client.simple_query(sql))
        .await
        .expect("query timeout")
        .unwrap_or_else(|e| panic!("`{sql}` must succeed: {e}"));
    messages
        .into_iter()
        .filter(|message| matches!(message, SimpleQueryMessage::Row(_)))
        .count()
}

// ===========================================================================
// 5. A wire session reports the name it authenticated with
// ===========================================================================

#[tokio::test]
async fn hdb009_wire_sessions_report_the_authenticated_login() {
    let (addr, server) = setup_server().await;
    let alice = connect_as(addr, "alice").await;
    let bob = connect_as(addr, "bob").await;

    for _ in 0..4 {
        for (client, expected) in [(&alice, "alice"), (&bob, "bob")] {
            assert_eq!(
                simple_row(client, IDENTITY_QUERY).await,
                vec![expected.to_string(); 3],
                "the wire session must report its own login in all three spellings"
            );
        }
    }

    for (client, expected) in [(&alice, "alice"), (&bob, "bob")] {
        assert_eq!(
            simple_row(client, "SHOW session_authorization").await,
            vec![expected.to_string()],
            "SHOW session_authorization must agree with current_user"
        );
    }

    server.abort();
}

// ===========================================================================
// 6. A predicate on the identity stays per-session across repeated reads
// ===========================================================================

#[tokio::test]
async fn hdb009_wire_where_clause_and_repeated_reads_stay_per_session() {
    let (addr, server) = setup_server().await;
    let alice = connect_as(addr, "alice").await;
    let bob = connect_as(addr, "bob").await;

    // Byte-identical SQL on both connections: alice matches, bob does not. The
    // repetition crosses the result cache's admission threshold, so a
    // text-keyed cache would hand bob alice's row (or alice bob's emptiness).
    for _ in 0..3 {
        assert_eq!(
            simple_row_count(&alice, "SELECT 1 WHERE current_user = 'alice'").await,
            1,
            "alice's session must match the predicate"
        );
        assert_eq!(
            simple_row_count(&bob, "SELECT 1 WHERE current_user = 'alice'").await,
            0,
            "bob's session must NOT match alice's predicate, however often it was asked before"
        );
    }

    server.abort();
}

// ===========================================================================
// 7. A startup packet with no `user` is refused
// ===========================================================================

fn put_cstr(buf: &mut Vec<u8>, value: &str) {
    buf.extend_from_slice(value.as_bytes());
    buf.push(0);
}

/// A StartupMessage carrying ONLY `database` — no `user` parameter at all.
fn startup_without_user() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_i32.to_be_bytes());
    put_cstr(&mut body, "database");
    put_cstr(&mut body, "postgres");
    body.push(0);

    let mut msg = Vec::new();
    msg.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    msg.extend_from_slice(&body);
    msg
}

/// A StartupMessage for `user=<user> dbname=postgres`.
fn startup_as(user: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_i32.to_be_bytes());
    put_cstr(&mut body, "user");
    put_cstr(&mut body, user);
    put_cstr(&mut body, "database");
    put_cstr(&mut body, "postgres");
    body.push(0);

    let mut msg = Vec::new();
    msg.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    msg.extend_from_slice(&body);
    msg
}

fn frontend_message(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut msg = vec![tag];
    msg.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    msg.extend_from_slice(body);
    msg
}

/// Connect and finish startup as `user`, returning the stream at ReadyForQuery.
async fn connect_wire_as(addr: SocketAddr, user: &str) -> TcpStream {
    let mut stream = timeout(IO_TIMEOUT, TcpStream::connect(addr))
        .await
        .expect("connect timeout")
        .expect("connect");
    timeout(IO_TIMEOUT, stream.write_all(&startup_as(user)))
        .await
        .expect("startup write timeout")
        .expect("startup write");
    loop {
        let (tag, _) = read_frame(&mut stream).await.expect("startup frame");
        if tag == b'Z' {
            return stream;
        }
    }
}

/// Send `sql` as one Query message, answering a CopyInResponse (`G`) with
/// `copy_rows` as CopyData frames plus CopyDone, and collect every backend
/// frame up to and including ReadyForQuery.
async fn wire_copy(stream: &mut TcpStream, sql: &str, copy_rows: &[String]) -> Vec<(u8, Vec<u8>)> {
    let mut body = Vec::new();
    put_cstr(&mut body, sql);
    timeout(IO_TIMEOUT, stream.write_all(&frontend_message(b'Q', &body)))
        .await
        .expect("query write timeout")
        .expect("query write");

    let mut frames = Vec::new();
    loop {
        let frame = read_frame(stream).await.expect("backend frame");
        let tag = frame.0;
        frames.push(frame);
        if tag == b'G' {
            let mut out = Vec::new();
            for row in copy_rows {
                out.extend_from_slice(&frontend_message(b'd', row.as_bytes()));
            }
            out.extend_from_slice(&frontend_message(b'c', &[]));
            timeout(IO_TIMEOUT, stream.write_all(&out))
                .await
                .expect("copy write timeout")
                .expect("copy write");
        }
        if tag == b'Z' {
            return frames;
        }
    }
}

async fn read_frame(stream: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    timeout(IO_TIMEOUT, stream.read_exact(&mut header)).await.ok()?.ok()?;
    let len = i32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut body = vec![0u8; len.saturating_sub(4)];
    timeout(IO_TIMEOUT, stream.read_exact(&mut body)).await.ok()?.ok()?;
    Some((header[0], body))
}

/// ErrorResponse fields, keyed by their one-byte field code (`S`, `C`, `M`, …).
fn error_fields(payload: &[u8]) -> Vec<(char, String)> {
    payload
        .split(|b| *b == 0)
        .filter(|f| !f.is_empty())
        .map(|f| (char::from(f[0]), String::from_utf8_lossy(&f[1..]).into_owned()))
        .collect()
}

#[tokio::test]
async fn hdb009_wire_startup_without_a_user_is_refused() {
    let (addr, server) = setup_server().await;
    let mut stream = timeout(IO_TIMEOUT, TcpStream::connect(addr))
        .await
        .expect("connect timeout")
        .expect("connect");
    timeout(IO_TIMEOUT, stream.write_all(&startup_without_user()))
        .await
        .expect("startup write timeout")
        .expect("startup write");

    let mut tags = Vec::new();
    let mut fields = Vec::new();
    while let Some((tag, payload)) = read_frame(&mut stream).await {
        tags.push(tag);
        if tag == b'E' {
            fields = error_fields(&payload);
            break;
        }
    }

    assert!(
        !tags.contains(&b'R'),
        "an anonymous startup must never reach an Authentication message, got frames {:?}",
        tags.iter().map(|t| char::from(*t)).collect::<Vec<_>>()
    );
    assert_eq!(
        tags.first().copied(),
        Some(b'E'),
        "the first frame must be ErrorResponse"
    );
    assert!(
        fields.iter().any(|(code, value)| *code == 'C' && value == "08P01"),
        "PostgreSQL answers 08P01 protocol_violation for a missing user: {fields:?}"
    );
    assert!(
        fields.iter().any(|(code, value)| *code == 'S' && value == "FATAL"),
        "startup failures are FATAL: {fields:?}"
    );

    server.abort();
}

// ===========================================================================
// 8. MySQL wire — the handshake user becomes the SQL identity
// ===========================================================================
//
// Minimal text-protocol client, the same shape as
// tests/mysql_stmt_execute_tests.rs (handshake + COM_QUERY), trimmed to the one
// question this case asks.

const COM_QUERY: u8 = 0x03;
const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;

struct MySqlTestClient {
    stream: DuplexStream,
}

impl MySqlTestClient {
    /// Spawn the handler over a duplex stream and log in as `user`.
    async fn login(db: Arc<EmbeddedDatabase>, user: &str) -> Self {
        let (client, server) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move {
            let _ = MySqlHandler::handle_connection(db, server, 1).await;
        });
        let mut this = Self { stream: client };

        let (_seq, _greeting) = this.read_packet().await;
        let mut p = Vec::new();
        p.extend_from_slice(&(CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION).to_le_bytes());
        p.extend_from_slice(&(1u32 << 24).to_le_bytes());
        p.push(45); // utf8mb4_general_ci
        p.extend_from_slice(&[0u8; 23]);
        p.extend_from_slice(user.as_bytes());
        p.push(0);
        p.push(0); // empty auth response (SECURE_CONNECTION framing)
        this.write_packet(1, &p).await;

        let (_seq, ok) = this.read_packet().await;
        assert_eq!(ok[0], 0x00, "expected auth OK, got {:?}", &ok[..ok.len().min(16)]);
        this
    }

    async fn write_packet(&mut self, seq: u8, payload: &[u8]) {
        let len = payload.len();
        let hdr = [
            (len & 0xFF) as u8,
            ((len >> 8) & 0xFF) as u8,
            ((len >> 16) & 0xFF) as u8,
            seq,
        ];
        self.stream.write_all(&hdr).await.unwrap();
        self.stream.write_all(payload).await.unwrap();
        self.stream.flush().await.unwrap();
    }

    async fn read_packet(&mut self) -> (u8, Vec<u8>) {
        let mut hdr = [0u8; 4];
        self.stream.read_exact(&mut hdr).await.unwrap();
        let len = (hdr[0] as usize) | ((hdr[1] as usize) << 8) | ((hdr[2] as usize) << 16);
        let mut payload = vec![0u8; len];
        self.stream.read_exact(&mut payload).await.unwrap();
        (hdr[3], payload)
    }

    /// COM_QUERY for a one-column, one-row result set; returns that value.
    async fn query_scalar(&mut self, sql: &str) -> String {
        let mut p = vec![COM_QUERY];
        p.extend_from_slice(sql.as_bytes());
        self.write_packet(0, &p).await;

        let (_seq, first) = self.read_packet().await;
        assert_ne!(
            first[0],
            0xFF,
            "server error for `{sql}`: {}",
            String::from_utf8_lossy(&first)
        );
        assert_eq!(first[0], 1, "expected a one-column result set for `{sql}`");
        let (_seq, _column_def) = self.read_packet().await;
        let (_seq, eof) = self.read_packet().await;
        assert_eq!(eof[0], 0xFE, "expected EOF after the column definition");

        let (_seq, row) = self.read_packet().await;
        assert_ne!(row[0], 0xFE, "expected a data row for `{sql}`");
        let len = row[0] as usize;
        assert!(len < 0xFB, "test helper only decodes short strings");
        let value = String::from_utf8_lossy(&row[1..1 + len]).into_owned();
        let (_seq, trailer) = self.read_packet().await;
        assert_eq!(trailer[0], 0xFE, "expected EOF after the row");
        value
    }
}

#[tokio::test]
async fn hdb009_mysql_login_name_becomes_the_sql_identity() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let mut carol = MySqlTestClient::login(Arc::clone(&db), "carol").await;
    assert_eq!(
        carol.query_scalar("SELECT current_user").await,
        "carol",
        "the MySQL handshake user is this session's SQL identity (trust listener: \
         asserted by the client, never proved)"
    );

    let mut dave = MySqlTestClient::login(Arc::clone(&db), "dave").await;
    assert_eq!(
        dave.query_scalar("SELECT current_user").await,
        "dave",
        "a second connection to the same database must not inherit carol's identity"
    );
    assert_eq!(
        carol.query_scalar("SELECT current_user").await,
        "carol",
        "and carol's connection must not pick up dave's"
    );
}

// ===========================================================================
// 9. COPY's fast path stamps the session identity, like every other write path
// ===========================================================================
//
// `COPY … FROM STDIN` outside a transaction is served by `copy_bulk_insert`,
// which the wire handler calls DIRECTLY — not through any `_for_session` entry
// point — and which evaluates column DEFAULTs itself. So a
// `DEFAULT current_user` column used to be stamped with the service user on the
// fast path while the very same table, written by `INSERT` (case 4) or by the
// generic COPY fallback (add a trigger and it is taken), recorded the real
// login. An audit column that is right most of the time and silently wrong when
// an internal fast-path decision flips is worse than one that is uniformly
// wrong, which is why this is pinned on the wire and not just in-crate.

#[tokio::test]
async fn hdb009_copy_fast_path_stamps_the_session_identity() {
    let (addr, server) = setup_server().await;
    let alice = connect_as(addr, "alice").await;
    alice
        .batch_execute("CREATE TABLE hdb009_copy_audit (id INT, who TEXT DEFAULT current_user)")
        .await
        .expect("create the audit table");

    // A plain table: no triggers, no constraints, no open transaction, so
    // `copy_bulk_insert` takes the batch and evaluates the DEFAULT itself.
    let mut wire = connect_wire_as(addr, "alice").await;
    let rows: Vec<String> = (1..=3).map(|i| format!("{i}\n")).collect();
    let frames = wire_copy(&mut wire, "COPY hdb009_copy_audit (id) FROM STDIN", &rows).await;
    let tags: Vec<char> = frames.iter().map(|(tag, _)| char::from(*tag)).collect();
    assert!(tags.contains(&'G'), "the server must invite the COPY stream: {tags:?}");
    assert!(!tags.contains(&'E'), "the COPY must succeed: {tags:?}");

    let who = simple_column(&alice, "SELECT who, id FROM hdb009_copy_audit ORDER BY id").await;
    assert_eq!(
        who,
        vec!["alice".to_string(); 3],
        "every COPY-inserted row must record the copying session's login, exactly as \
         `INSERT` does — the fast path evaluates the DEFAULT outside the session entry \
         points, so it needs the identity installed explicitly"
    );

    server.abort();
}

// ===========================================================================
// 10. A client-asserted login is bounded before it is published
// ===========================================================================
//
// Trust is the default on both listeners, so the name is whatever the client
// puts in the startup packet — bounded only by the 1 MiB startup-message cap.
// Since the identity is now cloned into a thread-local per statement and
// compared on every result-cache probe, an unbounded name would be a
// per-statement cost the client chooses. PostgreSQL truncates identifiers at
// NAMEDATALEN-1 = 63 bytes; so do we, and the truncated name is what SQL
// reports (no error, no silent full-length identity behind a short display).

#[tokio::test]
async fn hdb009_wire_login_name_is_truncated_to_the_identifier_limit() {
    let (addr, server) = setup_server().await;
    let long = "u".repeat(200);
    let client = connect_as(addr, &long).await;

    assert_eq!(
        simple_row(&client, IDENTITY_QUERY).await,
        vec!["u".repeat(63); 3],
        "a 200-byte client-asserted login must be published truncated to PostgreSQL's \
         63-byte identifier limit, identically for all three spellings"
    );

    server.abort();
}

// ===========================================================================
// 11. The wire's own read-cache probe runs with the session's identity
// ===========================================================================
//
// The shared result cache is tagged with the principal that computed each
// entry, and the tag is compared against a per-statement thread-local that only
// the `_for_session` entry points install. The PostgreSQL simple-query
// handler's FIRST act on a `SELECT` is its own top-level probe of that cache —
// and it sat outside every one of those entry points, so it probed with no
// principal installed and matched exactly the `None`-tagged entries: the ones
// computed session-less.
//
// On the shipped server layout that is a real leak. `src/main.rs` hands ONE
// `Arc<EmbeddedDatabase>` to both `PgServer` and `ApiServer::from_config`, so
// the REST data API, MCP and an embedder's own `db.query()` publish `None`-
// tagged rows into the very cache the wire probes; an authenticated client's
// first read of the same SQL was then answered with rows computed as
// `heliosdb`. (The mirror image is the perf half: on a wire-only server every
// WRITE is tagged `Some(login)`, so the untagged probe could never hit at all.)
//
// This case pins the leak direction, which is the one that changes an answer.
// `SELECT u FROM hdb009_cache_who` carries no identity token, so the SQL text
// alone cannot tell the two callers apart — the cache tag is the only thing
// that can, and only if the probe reads it under the right principal.

#[tokio::test]
async fn hdb009_wire_read_cache_is_probed_with_the_session_identity() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    db.execute("CREATE TABLE hdb009_cache_one (id INT)")
        .expect("create table");
    db.execute("INSERT INTO hdb009_cache_one VALUES (1)").expect("insert");
    db.execute("CREATE VIEW hdb009_cache_who AS SELECT current_user AS u FROM hdb009_cache_one")
        .expect("create view");

    const SQL: &str = "SELECT u FROM hdb009_cache_who";

    // Session-less, twice: the second sighting crosses the cache's admission
    // filter, so the rows are published tagged `login = None` — exactly what a
    // REST / MCP / embedded caller leaves behind on a shared handle.
    assert_eq!(embedded_row(&db, SQL), vec!["heliosdb".to_string()]);
    assert_eq!(
        embedded_row(&db, SQL),
        vec!["heliosdb".to_string()],
        "the session-less caller must keep seeing the service user (and this \
         repetition is what publishes the cache entry the wire then probes)"
    );

    let (addr, server) = setup_server_on(Arc::clone(&db)).await;
    let alice = connect_as(addr, "alice").await;

    // The FIRST wire read is the one that used to be served from the
    // session-less entry; the second proves the wire caches and re-reads its
    // OWN answer rather than falling back onto the untagged one.
    for round in 1..=2 {
        assert_eq!(
            simple_row(&alice, SQL).await,
            vec!["alice".to_string()],
            "round {round}: an authenticated wire client must never be served rows \
             computed session-less on the same handle — the handler's top-level \
             result-cache probe has to run under the session's identity"
        );
    }

    // ...and nothing leaks back the other way either: alice's now-cached rows
    // are tagged `Some("alice")` and must be a miss for the session-less caller.
    assert_eq!(
        embedded_row(&db, SQL),
        vec!["heliosdb".to_string()],
        "a session-less read must not pick up the wire session's cached identity"
    );

    server.abort();
}
