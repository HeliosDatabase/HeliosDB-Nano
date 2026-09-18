//! Batch C — PostgreSQL wire-protocol hygiene (four sprinter items).
//!
//! Every test here drives a REAL listener: the in-process `PgServer` for the
//! three protocol items, and the actual `heliosdb-nano` binary for the
//! Unix-socket connection cap, which lives in `src/main.rs` and has no
//! in-process entry point.
//!
//! What each one pins, and what it looked like before the fix:
//!
//! 1. sprinter 59b989cf7d7e — `SHOW <unknown parameter>` answered a DataRow
//!    containing an EMPTY STRING (`resolve_show_parameter`'s
//!    `_ => String::new()` catch-all), so a client could not tell "this server
//!    has no such setting" from "the setting is set to nothing". PostgreSQL
//!    answers `ERROR 42704 unrecognized configuration parameter "x"`.
//!
//! 2. sprinter e143ae12ea2d — a statement kind the planner has no arm for came
//!    back as `XX000 internal_error` (which PgBouncer, pgpool and HA proxies
//!    read as a broken backend) carrying the whole sqlparser AST — `Ident {
//!    value: "…", quote_style: None, span: Span(Location(..)..) }` — in the
//!    user-facing message. It is `0A000 feature_not_supported` naming the
//!    statement KIND and nothing else.
//!
//! 3. sprinter dfc3d6f2c341 — the PostgreSQL Unix-socket accept loop acquired
//!    no `max_connections` permit, so local clients could exceed the limit the
//!    TCP listener enforces and the server still reported.
//!
//! 4. sprinter 263befdf85ca — the idle-session deadline was armed between
//!    EVERY message, including between a `Parse` and its `Sync`, so a
//!    pipelining client that paused mid-batch was disconnected. PostgreSQL
//!    arms that timer only while the backend is idle at a ReadyForQuery. The
//!    control test below proves the timer still fires there — the fix must not
//!    be "the timeout no longer works".

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use bytes::{BufMut, BytesMut};
use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::protocol::postgres::timeouts::ConnectionTimeouts;
use heliosdb_nano::EmbeddedDatabase;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

/// Read budget for one backend frame. Generous: it only has to be longer than
/// the deliberately tiny `idle_session_timeout` the timing tests configure.
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);
/// The idle budget the two timing tests run under.
const TINY_IDLE: Duration = Duration::from_millis(200);

// ===========================================================================
// In-process server harness (same shape as tests/security_hdb_004.rs)
// ===========================================================================

async fn setup_server(timeouts: ConnectionTimeouts) -> (SocketAddr, String, tokio::task::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test port");
    let addr = listener.local_addr().expect("test addr");
    drop(listener);

    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let config = PgServerConfig::with_address(addr).with_timeouts(timeouts);
    let server = PgServer::new(config, db).expect("server");
    let handle = tokio::spawn(async move {
        let _ = server.serve().await;
    });

    tokio::time::sleep(Duration::from_millis(150)).await;
    let conn_string = format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port());
    (addr, conn_string, handle)
}

async fn connect_client(conn_string: &str) -> (Client, tokio::task::JoinHandle<()>) {
    let (client, connection) = timeout(FRAME_TIMEOUT, tokio_postgres::connect(conn_string, NoTls))
        .await
        .expect("connect timeout")
        .expect("connect");
    let task = tokio::spawn(async move {
        let _ = connection.await;
    });
    (client, task)
}

/// Run `sql` as one simple query and return the `DbError` it must fail with.
async fn expect_db_error(client: &Client, sql: &str) -> tokio_postgres::error::DbError {
    let err = timeout(FRAME_TIMEOUT, client.simple_query(sql))
        .await
        .expect("query timeout")
        .expect_err("the statement must fail");
    err.as_db_error()
        .cloned()
        .unwrap_or_else(|| panic!("expected a DbError for `{sql}`, got: {err:?}"))
}

// ===========================================================================
// Raw frontend/backend frames — the extended protocol has to be driven by
// hand, because no driver will send a `Parse` and then simply stop.
// ===========================================================================

fn put_cstr(buf: &mut BytesMut, value: &str) {
    buf.extend_from_slice(value.as_bytes());
    buf.put_u8(0);
}

fn startup_message() -> BytesMut {
    let mut body = BytesMut::new();
    body.put_i32(196_608); // protocol 3.0
    put_cstr(&mut body, "user");
    put_cstr(&mut body, "postgres");
    put_cstr(&mut body, "database");
    put_cstr(&mut body, "postgres");
    body.put_u8(0);

    let mut msg = BytesMut::new();
    msg.put_i32((body.len() + 4) as i32);
    msg.extend_from_slice(&body);
    msg
}

fn frontend_message(tag: u8, body: BytesMut) -> BytesMut {
    let mut msg = BytesMut::new();
    msg.put_u8(tag);
    msg.put_i32((body.len() + 4) as i32);
    msg.extend_from_slice(&body);
    msg
}

fn parse_message(statement: &str, sql: &str) -> BytesMut {
    let mut body = BytesMut::new();
    put_cstr(&mut body, statement);
    put_cstr(&mut body, sql);
    body.put_i16(0); // no parameter type OIDs
    frontend_message(b'P', body)
}

fn bind_message(portal: &str, statement: &str) -> BytesMut {
    let mut body = BytesMut::new();
    put_cstr(&mut body, portal);
    put_cstr(&mut body, statement);
    body.put_i16(0); // parameter format codes
    body.put_i16(0); // parameter values
    body.put_i16(0); // result format codes
    frontend_message(b'B', body)
}

fn describe_portal_message(portal: &str) -> BytesMut {
    let mut body = BytesMut::new();
    body.put_u8(b'P');
    put_cstr(&mut body, portal);
    frontend_message(b'D', body)
}

fn execute_message(portal: &str) -> BytesMut {
    let mut body = BytesMut::new();
    put_cstr(&mut body, portal);
    body.put_i32(0); // unlimited rows
    frontend_message(b'E', body)
}

fn sync_message() -> BytesMut {
    frontend_message(b'S', BytesMut::new())
}

async fn write_frames<S: AsyncWrite + Unpin>(stream: &mut S, frames: &[BytesMut]) {
    let mut out = BytesMut::new();
    for frame in frames {
        out.extend_from_slice(frame);
    }
    timeout(FRAME_TIMEOUT, stream.write_all(&out))
        .await
        .expect("write timeout")
        .expect("write");
}

/// One backend frame, or `None` on EOF / timeout / a closed peer — every way a
/// refused or torn-down connection can end.
async fn read_frame<S: AsyncRead + Unpin>(stream: &mut S) -> Option<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    timeout(FRAME_TIMEOUT, stream.read_exact(&mut header))
        .await
        .ok()?
        .ok()?;
    let len = i32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    let body_len = usize::try_from(len - 4).ok()?;
    let mut body = vec![0u8; body_len];
    if body_len > 0 {
        timeout(FRAME_TIMEOUT, stream.read_exact(&mut body)).await.ok()?.ok()?;
    }
    Some((header[0], body))
}

/// Frames up to and including the next ReadyForQuery, or up to EOF.
async fn read_until_ready<S: AsyncRead + Unpin>(stream: &mut S) -> Vec<(u8, Vec<u8>)> {
    let mut frames = Vec::new();
    while let Some(frame) = read_frame(stream).await {
        let tag = frame.0;
        frames.push(frame);
        if tag == b'Z' {
            break;
        }
    }
    frames
}

/// Send the startup packet and read to the first ReadyForQuery. `false` when
/// the server never gets there — which is exactly what a connection refused by
/// the limiter looks like from the client side.
async fn startup<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) -> bool {
    // A write to a peer the server has already dropped fails (or is swallowed
    // by the kernel buffer and shows up as EOF on the read below); both mean
    // "not served".
    let wrote = timeout(FRAME_TIMEOUT, stream.write_all(&startup_message())).await;
    if !matches!(wrote, Ok(Ok(()))) {
        return false;
    }
    while let Some((tag, _)) = read_frame(stream).await {
        if tag == b'Z' {
            return true;
        }
        if tag == b'E' {
            return false;
        }
    }
    false
}

fn frame_tags(frames: &[(u8, Vec<u8>)]) -> String {
    frames
        .iter()
        .map(|(tag, _)| (*tag as char).to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

/// One field of an ErrorResponse body (`C` = SQLSTATE, `M` = message).
fn error_field(body: &[u8], field: u8) -> Option<String> {
    let mut cursor = body;
    while let Some((&kind, rest)) = cursor.split_first() {
        if kind == 0 {
            return None;
        }
        let end = rest.iter().position(|b| *b == 0)?;
        let value = String::from_utf8_lossy(&rest[..end]).to_string();
        if kind == field {
            return Some(value);
        }
        cursor = &rest[end + 1..];
    }
    None
}

fn first_error(frames: &[(u8, Vec<u8>)]) -> Option<(String, String)> {
    let (_, body) = frames.iter().find(|(tag, _)| *tag == b'E')?;
    Some((
        error_field(body, b'C').unwrap_or_default(),
        error_field(body, b'M').unwrap_or_default(),
    ))
}

// ===========================================================================
// 1. sprinter 59b989cf7d7e — SHOW of a name this server does not know
// ===========================================================================

/// `SHOW definitely_not_a_real_guc` is PostgreSQL's `42704 unrecognized
/// configuration parameter "…"`, and a name the server DOES know still answers
/// its row (the control that keeps this from being "SHOW is broken now").
#[tokio::test]
async fn show_unknown_parameter_is_42704() {
    let (_addr, cs, _handle) = setup_server(ConnectionTimeouts::disabled()).await;
    let (client, task) = connect_client(&cs).await;

    // CONTROL first: a known parameter still returns exactly one row.
    let messages = timeout(FRAME_TIMEOUT, client.simple_query("SHOW server_version"))
        .await
        .expect("query timeout")
        .expect("SHOW server_version must succeed");
    let value = messages
        .into_iter()
        .find_map(|message| match message {
            SimpleQueryMessage::Row(row) => row.get(0).map(str::to_string),
            _ => None,
        })
        .expect("SHOW server_version must answer a row");
    assert!(
        !value.trim().is_empty(),
        "control: SHOW server_version answered {value:?}"
    );

    let db_error = expect_db_error(&client, "SHOW definitely_not_a_real_guc").await;

    drop(client);
    task.abort();

    assert_eq!(
        db_error.code().code(),
        "42704",
        "sprinter 59b989cf7d7e: SHOW of an unknown parameter must be 42704 \
         undefined_object, got {} / {:?}",
        db_error.code().code(),
        db_error.message()
    );
    assert!(
        db_error.message().contains("unrecognized configuration parameter")
            && db_error.message().contains("definitely_not_a_real_guc"),
        "the message must be PostgreSQL's own wording and name the parameter, got {:?}",
        db_error.message()
    );
}

/// The same name over the EXTENDED protocol (`Client::query`, the path every
/// server-side-binding driver takes) must not answer a row either.
#[tokio::test]
async fn show_unknown_parameter_over_extended_protocol_is_not_an_empty_row() {
    let (_addr, cs, _handle) = setup_server(ConnectionTimeouts::disabled()).await;
    let (client, task) = connect_client(&cs).await;

    let query = client.query("SHOW definitely_not_a_real_guc", &[]);
    let result = timeout(FRAME_TIMEOUT, query).await.expect("query timeout");

    drop(client);
    task.abort();

    let err = match result {
        Ok(rows) => panic!("SHOW of an unknown parameter answered {} row(s)", rows.len()),
        Err(e) => e,
    };
    let db_error = match err.as_db_error() {
        Some(db_error) => db_error,
        None => panic!("expected a DbError, got: {err:?}"),
    };
    assert!(
        !db_error.message().is_empty(),
        "the refusal must carry a message, got {db_error:?}"
    );
}

// ===========================================================================
// 2. sprinter e143ae12ea2d — an unimplemented statement kind
// ===========================================================================

/// `LISTEN` parses (sqlparser's PostgreSQL dialect implements it) and reaches
/// the planner's final arm, which is the code under test. The refusal must be
/// `0A000 feature_not_supported` — never `XX000`, which a pooler reads as a
/// broken backend — and must not contain one byte of the Rust AST.
#[tokio::test]
async fn unimplemented_statement_is_feature_not_supported_not_internal_error() {
    let (_addr, cs, _handle) = setup_server(ConnectionTimeouts::disabled()).await;
    let (client, task) = connect_client(&cs).await;

    let db_error = expect_db_error(&client, "LISTEN wire_hygiene_channel").await;

    // The session must still be usable afterwards: an unimplemented statement
    // is a normal error, not a broken connection.
    let alive = timeout(FRAME_TIMEOUT, client.simple_query("SELECT 1")).await.is_ok();

    drop(client);
    task.abort();

    assert_eq!(
        db_error.code().code(),
        "0A000",
        "sprinter e143ae12ea2d: an unimplemented statement kind must report \
         0A000 feature_not_supported, got {} / {:?}",
        db_error.code().code(),
        db_error.message()
    );
    let message = db_error.message().to_string();
    for leak in ["Ident {", "quote_style", "Span(", "Location("] {
        assert!(
            !message.contains(leak),
            "sprinter e143ae12ea2d: the client-facing message still carries the \
             sqlparser AST ({leak:?} in {message:?})"
        );
    }
    assert!(
        message.contains("LISTEN"),
        "the message must name the statement KIND, got {message:?}"
    );
    assert!(
        message.contains("wire_hygiene_channel"),
        "the message should name the object the user wrote, got {message:?}"
    );
    assert!(alive, "the connection must survive an unimplemented statement");
}

// ===========================================================================
// 4. sprinter 263befdf85ca — when the idle deadline is armed
// ===========================================================================

/// A client that sends `Parse` and then pauses longer than
/// `idle_session_timeout` before the rest of its pipelined batch is NOT idle:
/// PostgreSQL arms that timer only while the backend sits at a ReadyForQuery.
/// On the unfixed tree the server armed it before every read and killed the
/// connection mid-batch.
#[tokio::test]
async fn idle_deadline_is_not_armed_mid_pipeline() {
    let timeouts = ConnectionTimeouts {
        idle_session_timeout: TINY_IDLE,
        ..ConnectionTimeouts::disabled()
    };
    let (addr, _cs, _handle) = setup_server(timeouts).await;

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    assert!(startup(&mut stream).await, "startup must reach ReadyForQuery");

    // Open the extended-protocol batch and stop. No Sync yet.
    write_frames(&mut stream, &[parse_message("hygiene_stmt", "SELECT 1")]).await;
    tokio::time::sleep(TINY_IDLE * 3).await;

    // Finish the batch the way any pipelining driver would.
    write_frames(
        &mut stream,
        &[
            bind_message("", "hygiene_stmt"),
            describe_portal_message(""),
            execute_message(""),
            sync_message(),
        ],
    )
    .await;

    let frames = read_until_ready(&mut stream).await;
    let tags = frame_tags(&frames);

    assert!(
        first_error(&frames).is_none(),
        "sprinter 263befdf85ca: the mid-pipeline pause was treated as an idle \
         session and the connection was torn down ({:?}); frames: [{tags}]",
        first_error(&frames)
    );
    assert!(
        frames.iter().any(|(tag, _)| *tag == b'D'),
        "the completed batch must return its DataRow; frames: [{tags}]"
    );
    assert!(
        frames.iter().any(|(tag, _)| *tag == b'Z'),
        "the completed batch must end at ReadyForQuery; frames: [{tags}]"
    );
}

/// NON-VACUITY GUARD for the test above: a session that is genuinely idle AT a
/// ReadyForQuery is still disconnected with PostgreSQL's FATAL 57P05. "The
/// deadline is never armed any more" would pass the previous test and fail
/// this one.
#[tokio::test]
async fn idle_deadline_still_fires_when_idle_at_ready_for_query() {
    let timeouts = ConnectionTimeouts {
        idle_session_timeout: TINY_IDLE,
        ..ConnectionTimeouts::disabled()
    };
    let (addr, _cs, _handle) = setup_server(timeouts).await;

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    assert!(startup(&mut stream).await, "startup must reach ReadyForQuery");

    // Say nothing at all: idle, at a ReadyForQuery, which is the one state
    // PostgreSQL measures.
    let frames = read_until_ready(&mut stream).await;
    let tags = frame_tags(&frames);
    let (code, message) = first_error(&frames)
        .unwrap_or_else(|| panic!("an idle session must be closed with a FATAL, got frames: [{tags}]"));

    assert_eq!(
        code, "57P05",
        "an expired idle_session_timeout must report 57P05, got {code} / {message:?}"
    );
}

// ===========================================================================
// 3. sprinter dfc3d6f2c341 — the Unix-socket listener's connection cap
// ===========================================================================
//
// The accept loop under test lives in `src/main.rs`, which has no in-process
// entry point, so this spawns the real binary — the same approach
// tests/shutdown_signal_tests.rs takes for the shutdown path.

#[cfg(unix)]
mod unix_socket_connection_cap {
    use super::{startup, FRAME_TIMEOUT};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};
    use tokio::net::UnixStream;

    /// The cap the server is started with, and therefore the number of
    /// Unix-socket connections that must be accepted before the next is not.
    const MAX_CONNECTIONS: usize = 2;
    /// The server opens a RocksDB data directory; a debug build is slow.
    const STARTUP_TIMEOUT: Duration = Duration::from_secs(90);

    /// Kills the server even if an assertion unwinds past it.
    struct ServerProcess(Child);

    impl Drop for ServerProcess {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn free_tcp_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }

    /// Connect to the socket, retrying only while the listener is not there
    /// yet, and complete the startup handshake. Retries never leave a
    /// half-open connection behind — a connection that is not made cannot hold
    /// one of the permits under test.
    async fn connect_and_start_up(path: &Path, server: &mut ServerProcess) -> UnixStream {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            if let Ok(Some(status)) = server.0.try_wait() {
                panic!("server exited during startup with {status}");
            }
            match UnixStream::connect(path).await {
                Ok(mut stream) => {
                    assert!(
                        startup(&mut stream).await,
                        "a connection within max_connections must reach ReadyForQuery"
                    );
                    return stream;
                }
                Err(e) => {
                    if Instant::now() >= deadline {
                        panic!("the server never listened on {}: {e}", path.display());
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    fn spawn_server(data_dir: &Path, sock_dir: &Path, port: u16) -> ServerProcess {
        let child = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"))
            .arg("start")
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--port")
            .arg(port.to_string())
            // The HTTP listener is irrelevant here and only adds a port to
            // collide on.
            .args(["--http-port", "0"])
            .arg("--pg-socket-dir")
            .arg(sock_dir)
            .arg("--max-connections")
            .arg(MAX_CONNECTIONS.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the heliosdb-nano binary must be runnable");
        ServerProcess(child)
    }

    fn socket_path(sock_dir: &Path, port: u16) -> PathBuf {
        // libpq's own name for a Unix socket: `<dir>/.s.PGSQL.<port>`.
        sock_dir.join(format!(".s.PGSQL.{port}"))
    }

    /// The listener must enforce `max_connections`, exactly like the TCP one:
    /// the connection past the cap is closed without ever being served, not
    /// accepted unconditionally.
    #[tokio::test]
    async fn unix_socket_listener_respects_max_connections() {
        let data_dir = tempfile::TempDir::new().unwrap();
        let sock_dir = tempfile::TempDir::new().unwrap();
        let port = free_tcp_port();
        let mut server = spawn_server(data_dir.path(), sock_dir.path(), port);
        let path = socket_path(sock_dir.path(), port);

        // Fill the listener to its limit and HOLD the connections open.
        let mut held = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            held.push(connect_and_start_up(&path, &mut server).await);
        }

        // One more. The kernel may still accept it into the backlog, so the
        // proof is that it never gets served: no ReadyForQuery, ever.
        let served = match tokio::time::timeout(FRAME_TIMEOUT, UnixStream::connect(&path)).await {
            Ok(Ok(mut over_the_cap)) => startup(&mut over_the_cap).await,
            // A refused connect is the same verdict, reached sooner.
            Ok(Err(_)) | Err(_) => false,
        };

        drop(held);

        assert!(
            !served,
            "sprinter dfc3d6f2c341: connection {} of max_connections = {MAX_CONNECTIONS} was \
             served on the PostgreSQL Unix socket. The UDS accept loop in src/main.rs acquires \
             no permit from the max_connections semaphore the TCP listener uses, so local \
             clients can exhaust file descriptors and memory past the limit the server reports.",
            MAX_CONNECTIONS + 1
        );
    }
}
