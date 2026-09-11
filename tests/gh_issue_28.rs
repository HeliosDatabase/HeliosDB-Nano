//! GH #28 — permanent regression suite for connection-lifetime timeouts.
//!
//! Install as `tests/gh_issue_28.rs`.
//!
//! **This file references the configuration surface the fix introduces
//! (`ConnectionTimeouts`, `PgServerConfig::with_timeouts`,
//! `heliosdb_nano::protocol::postgres::timeouts::*`). It therefore does NOT
//! compile on v4.31.1 — that is itself the proof that the feature is absent.**
//! For a failing-on-main proof that compiles against today's API, see
//! `tests/gh_issue_28_today.rs` (`repro_28_today.rs`).
//!
//! # The defect
//!
//! On v4.31.1 the PostgreSQL listener applies exactly one socket option
//! (`set_nodelay`, `src/protocol/postgres/server.rs:185`) and no deadline of any
//! kind:
//!
//!   * `PgServer::serve` acquires the `max_connections` semaphore permit before
//!     reading a byte (`server.rs:190-215`);
//!   * `PgServer::handle_connection` blocks in `read_exact` forever
//!     (`server.rs:238-249`);
//!   * `PgConnectionHandler::handle`'s message loop calls `read_message`
//!     (`handler.rs:519`), which awaits `stream.read` with no timeout
//!     (`handler.rs:796-832`);
//!   * `handle_startup` reads the startup packet and the password message with
//!     no timeout (`handler.rs:606-639`, `handler.rs:687`, `handler.rs:719`);
//!   * `[server] idle_timeout_secs` exists in `src/config.rs:1113-1114` with a
//!     300 s default and its ONLY callers are the deprecated `legacy-network`
//!     stack (`src/network/server.rs:61/73/134/203`), which is off by default.
//!
//! # Required behaviour (PostgreSQL-compatible)
//!
//!   * `authentication_timeout`               — default 60 s, 0 disables.
//!   * `idle_session_timeout`                 — default 0 (disabled), as in PG.
//!   * `idle_in_transaction_session_timeout`  — default 0 (disabled), as in PG.
//!   * TCP keepalive on every accepted socket — on by default.
//!   * A WARN when in-use connections cross `connection_warn_threshold_percent`
//!     (default 80) of `max_connections`.
//!
//! Every one of those is a `config.toml` `[server]` key AND a `start` CLI flag.
//!
//! # Requirements this file pins on the new type (read before implementing)
//!
//!   * `ConnectionTimeouts` needs a MANUAL `impl Default` carrying the
//!     PostgreSQL defaults (auth 60 s, both idle knobs ZERO, keepalive `Some`,
//!     warn 80). `#[derive(Default)]` would give ZERO/None/0 and
//!     `defaults_match_postgresql` would fail.
//!   * It must `#[derive(Debug, Clone)]`: `PgServerConfig` is
//!     `#[derive(Debug, Clone)]` (src/protocol/postgres/server.rs:18-19) and
//!     will not compile with a non-Clone field.
//!   * All five fields are `pub` and the struct must not be `#[non_exhaustive]`
//!     — the tests use struct-update syntax (`..all_disabled()`).
//!   * `TcpKeepaliveSettings` needs `Debug + Clone` for the same reason.
//!   * The read deadline must wrap ONE `stream.read()` and RESET on any byte
//!     received, NOT the whole `read_message()` loop — see
//!     `a_partially_received_message_is_not_killed_by_the_idle_deadline`.
//!
//! # Coverage note (no faked coverage)
//!
//! "A long-running query must not be killed by an idle timeout" is enforced
//! structurally: the deadline wraps the *read* of the next frontend message and
//! never the execution of a statement. HeliosDB Nano has no `pg_sleep()`, so
//! there is no cheap way to hold a statement open past a timeout from a client;
//! that property is covered here by the policy unit test
//! (`read_deadline_is_none_while_a_statement_is_running`) plus the end-to-end
//! `busy_session_is_never_closed_by_idle_session_timeout`, and NOT by a fake
//! slow-query test.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::protocol::postgres::timeouts::{
    apply_socket_options, ConnectionTimeouts, SessionActivity, TcpKeepaliveSettings,
};
use heliosdb_nano::EmbeddedDatabase;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

async fn setup(
    max_connections: usize,
    timeouts: ConnectionTimeouts,
) -> (std::net::SocketAddr, String, tokio::task::JoinHandle<()>) {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
    let addr = probe.local_addr().expect("local_addr");
    drop(probe);

    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let config = PgServerConfig::with_address(addr)
        .with_max_connections(max_connections)
        .with_timeouts(timeouts);
    let server = PgServer::new(config, db).expect("server");
    let handle = tokio::spawn(async move {
        if let Err(e) = server.serve().await {
            eprintln!("server stopped: {e}");
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let cs = format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port());
    (addr, cs, handle)
}

/// Every timeout disabled — the v4.31.1 behaviour, used as the baseline for
/// tests that isolate ONE knob.
fn all_disabled() -> ConnectionTimeouts {
    ConnectionTimeouts {
        authentication_timeout: Duration::ZERO,
        idle_session_timeout: Duration::ZERO,
        idle_in_transaction_session_timeout: Duration::ZERO,
        tcp_keepalive: None,
        connection_warn_threshold_percent: 0,
    }
}

async fn connect(cs: &str) -> (Client, tokio::task::JoinHandle<()>) {
    let (client, conn) = tokio_postgres::connect(cs, NoTls).await.expect("connect");
    let task = tokio::spawn(async move {
        let _ = conn.await;
    });
    (client, task)
}

async fn try_full_session(cs: &str) -> bool {
    match tokio::time::timeout(Duration::from_secs(5), tokio_postgres::connect(cs, NoTls)).await {
        Ok(Ok((client, conn))) => {
            let task = tokio::spawn(async move {
                let _ = conn.await;
            });
            let ok = matches!(
                tokio::time::timeout(Duration::from_secs(5), client.simple_query("SELECT 1")).await,
                Ok(Ok(_))
            );
            drop(client);
            task.abort();
            ok
        }
        _ => false,
    }
}

/// First column of the first `DataRow` of a simple-protocol result.
fn first_simple_value(msgs: &[SimpleQueryMessage]) -> Option<String> {
    msgs.iter().find_map(|m| match m {
        SimpleQueryMessage::Row(r) => r.get(0).map(|s| s.to_string()),
        _ => None,
    })
}

/// Raw socket that connects and never sends a startup packet.
async fn silent_scanner_socket(addr: std::net::SocketAddr) -> TcpStream {
    TcpStream::connect(addr).await.expect("scanner connect")
}

/// Wait until the peer closes `stream` (read returns 0 / errors), or `limit`
/// elapses. Returns how long it took, or `None` if the socket stayed open.
async fn wait_for_server_close(mut stream: TcpStream, limit: Duration) -> Option<Duration> {
    let started = Instant::now();
    let mut buf = [0u8; 64];
    match tokio::time::timeout(limit, stream.read(&mut buf)).await {
        Ok(Ok(0)) => Some(started.elapsed()),
        Ok(Err(_)) => Some(started.elapsed()),
        // A byte arrived that was not a close — the server said something; not
        // a close, so keep waiting is pointless: report "not closed".
        Ok(Ok(_)) => None,
        Err(_) => None,
    }
}

// ---------------------------------------------------------------------------
// POSITIVE CONTROLS — must pass before and after the fix.
// ---------------------------------------------------------------------------

/// Control A: the harness serves queries at all.
#[tokio::test]
async fn positive_control_server_serves_a_query() {
    let (_addr, cs, _h) = setup(8, all_disabled()).await;
    assert!(
        try_full_session(&cs).await,
        "positive control failed: harness cannot run SELECT 1"
    );
}

/// Control B: `max_connections` still rejects when full (unchanged behaviour;
/// this is the `Connection limit reached (N), rejecting …` path).
#[tokio::test]
async fn positive_control_connection_limit_still_rejects_when_full() {
    let (_addr, cs, _h) = setup(1, all_disabled()).await;
    let (holder, holder_task) = connect(&cs).await;
    holder.simple_query("SELECT 1").await.expect("holder query");
    assert!(
        !try_full_session(&cs).await,
        "server accepted a second connection with max_connections = 1"
    );
    drop(holder);
    holder_task.abort();
}

/// Control C: with every timeout disabled, an idle session survives — the fix
/// must not start closing sessions that the operator did not ask to be closed.
#[tokio::test]
async fn positive_control_disabled_timeouts_never_close_an_idle_session() {
    let (_addr, cs, _h) = setup(8, all_disabled()).await;
    let (client, task) = connect(&cs).await;
    client.simple_query("SELECT 1").await.expect("first query");
    tokio::time::sleep(Duration::from_secs(3)).await;
    client
        .simple_query("SELECT 1")
        .await
        .expect("session was closed although every timeout is disabled");
    drop(client);
    task.abort();
}

// ---------------------------------------------------------------------------
// PostgreSQL-compatible DEFAULTS
// ---------------------------------------------------------------------------

/// PostgreSQL defaults: `authentication_timeout` 60 s;
/// `idle_session_timeout` and `idle_in_transaction_session_timeout` 0
/// (disabled); keepalive on; warn at 80 %.
#[test]
fn defaults_match_postgresql() {
    let d = ConnectionTimeouts::default();
    assert_eq!(
        d.authentication_timeout,
        Duration::from_secs(60),
        "authentication_timeout default must be 60s, as in PostgreSQL"
    );
    assert_eq!(
        d.idle_session_timeout,
        Duration::ZERO,
        "idle_session_timeout must default to 0 = DISABLED, as in PostgreSQL"
    );
    assert_eq!(
        d.idle_in_transaction_session_timeout,
        Duration::ZERO,
        "idle_in_transaction_session_timeout must default to 0 = DISABLED, as in PostgreSQL"
    );
    assert!(
        d.tcp_keepalive.is_some(),
        "TCP keepalive must be ON by default — this is the half-open-socket reaper #28 asks for"
    );
    assert_eq!(d.connection_warn_threshold_percent, 80);
}

/// The `[server]` config section is the source of truth; a default `Config`
/// must produce the default policy.
#[test]
fn server_config_section_maps_onto_the_policy() {
    let cfg = heliosdb_nano::Config::default();
    let t = ConnectionTimeouts::from_server_config(&cfg.server);
    assert_eq!(t.authentication_timeout, Duration::from_secs(60));
    assert_eq!(t.idle_session_timeout, Duration::ZERO);
    assert_eq!(t.idle_in_transaction_session_timeout, Duration::ZERO);
    assert_eq!(t.connection_warn_threshold_percent, 80);
}

// ---------------------------------------------------------------------------
// POLICY UNIT TESTS — the decision function, independent of any socket.
// ---------------------------------------------------------------------------

/// A statement that is executing must never be interrupted by an idle timeout:
/// the deadline applies only while waiting for the NEXT frontend message.
#[test]
fn read_deadline_is_none_while_a_statement_is_running() {
    let t = ConnectionTimeouts {
        authentication_timeout: Duration::from_secs(1),
        idle_session_timeout: Duration::from_secs(1),
        idle_in_transaction_session_timeout: Duration::from_secs(1),
        ..ConnectionTimeouts::default()
    };
    assert_eq!(
        t.read_deadline(SessionActivity::Busy),
        None,
        "a running statement must have no deadline"
    );
}

/// Full policy matrix. `0` always means "disabled", per PostgreSQL.
#[test]
fn read_deadline_policy_matrix() {
    let t = ConnectionTimeouts {
        authentication_timeout: Duration::from_secs(60),
        idle_session_timeout: Duration::from_secs(600),
        idle_in_transaction_session_timeout: Duration::from_secs(30),
        ..ConnectionTimeouts::default()
    };
    assert_eq!(
        t.read_deadline(SessionActivity::Authenticating),
        Some(Duration::from_secs(60))
    );
    assert_eq!(t.read_deadline(SessionActivity::Idle), Some(Duration::from_secs(600)));
    assert_eq!(
        t.read_deadline(SessionActivity::IdleInTransaction),
        Some(Duration::from_secs(30))
    );

    let off = all_disabled();
    assert_eq!(off.read_deadline(SessionActivity::Authenticating), None);
    assert_eq!(off.read_deadline(SessionActivity::Idle), None);
    assert_eq!(off.read_deadline(SessionActivity::IdleInTransaction), None);
    assert_eq!(off.read_deadline(SessionActivity::Busy), None);
}

/// An open transaction that has ALSO exceeded `idle_session_timeout` must be
/// closed by whichever deadline is nearer — a session sitting in a transaction
/// must never live longer than a session sitting idle.
#[test]
fn idle_in_transaction_never_outlives_idle_session_timeout() {
    let t = ConnectionTimeouts {
        idle_session_timeout: Duration::from_secs(10),
        idle_in_transaction_session_timeout: Duration::from_secs(600),
        ..all_disabled()
    };
    assert_eq!(
        t.read_deadline(SessionActivity::IdleInTransaction),
        Some(Duration::from_secs(10)),
        "the shorter of the two idle deadlines must win"
    );

    // …and a disabled idle_session_timeout must not disable the in-transaction one.
    let t = ConnectionTimeouts {
        idle_session_timeout: Duration::ZERO,
        idle_in_transaction_session_timeout: Duration::from_secs(30),
        ..all_disabled()
    };
    assert_eq!(
        t.read_deadline(SessionActivity::IdleInTransaction),
        Some(Duration::from_secs(30))
    );
}

/// The utilisation warning fires at, not after, the configured percentage, and
/// only on the crossing edge.
#[test]
fn utilisation_warning_threshold() {
    let t = ConnectionTimeouts {
        connection_warn_threshold_percent: 80,
        ..ConnectionTimeouts::default()
    };
    assert!(!t.should_warn_utilisation(79, 100));
    assert!(t.should_warn_utilisation(80, 100));
    assert!(t.should_warn_utilisation(100, 100));
    assert!(!t.should_warn_utilisation(0, 100));
    // 0 disables the warning entirely.
    let off = ConnectionTimeouts {
        connection_warn_threshold_percent: 0,
        ..ConnectionTimeouts::default()
    };
    assert!(!off.should_warn_utilisation(100, 100));
    // No division-by-zero when max_connections is 0.
    assert!(!t.should_warn_utilisation(0, 0));
}

// ---------------------------------------------------------------------------
// TCP KEEPALIVE — applied to the accepted socket.
// ---------------------------------------------------------------------------

/// `apply_socket_options` must turn SO_KEEPALIVE on for the accepted socket.
/// Read back through socket2 (a direct dependency added by the fix).
#[tokio::test]
async fn keepalive_is_enabled_on_accepted_sockets() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let client = tokio::spawn(async move { TcpStream::connect(addr).await.expect("connect") });
    let (accepted, _) = listener.accept().await.expect("accept");
    let _client = client.await.expect("client join");

    let t = ConnectionTimeouts {
        tcp_keepalive: Some(TcpKeepaliveSettings {
            idle: Duration::from_secs(60),
            interval: Duration::from_secs(10),
            retries: 3,
        }),
        ..all_disabled()
    };
    apply_socket_options(&accepted, &t).expect("apply_socket_options");

    let sock = socket2::SockRef::from(&accepted);
    assert!(
        sock.keepalive().expect("read SO_KEEPALIVE"),
        "SO_KEEPALIVE was not set on the accepted socket — half-open sockets stay forever (#28)"
    );

    // …and `None` must leave the socket alone (opt-out must really opt out).
    let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind2");
    let addr2 = listener2.local_addr().expect("addr2");
    let client2 = tokio::spawn(async move { TcpStream::connect(addr2).await.expect("connect2") });
    let (accepted2, _) = listener2.accept().await.expect("accept2");
    let _client2 = client2.await.expect("client2 join");
    apply_socket_options(&accepted2, &all_disabled()).expect("apply_socket_options (off)");
    assert!(
        !socket2::SockRef::from(&accepted2)
            .keepalive()
            .expect("read SO_KEEPALIVE"),
        "keepalive was enabled although tcp_keepalive is None"
    );
}

// ---------------------------------------------------------------------------
// authentication_timeout
// ---------------------------------------------------------------------------

/// A socket that never sends a startup packet must be closed after
/// `authentication_timeout`.
#[tokio::test]
async fn authentication_timeout_closes_a_socket_that_never_speaks() {
    let t = ConnectionTimeouts {
        authentication_timeout: Duration::from_secs(2),
        ..all_disabled()
    };
    let (addr, _cs, _h) = setup(8, t).await;

    let scanner = silent_scanner_socket(addr).await;
    let closed_after = wait_for_server_close(scanner, Duration::from_secs(15)).await;
    assert!(
        closed_after.is_some(),
        "GH #28: the server never closed a socket that sent no startup packet \
         (authentication_timeout = 2s)"
    );
    let elapsed = closed_after.expect("checked");
    assert!(
        elapsed >= Duration::from_millis(1500),
        "closed too early ({elapsed:?}) — authentication_timeout must not fire before it elapses"
    );
}

/// A socket that opens, sends a startup packet, and then never answers the
/// password challenge must also be closed: half of the scanners in #28 speak
/// PostgreSQL far enough to get an AuthenticationCleartextPassword and then go
/// quiet. (Trust auth is used by the harness, so this drives the pre-startup
/// half only; the password-message half is covered by the policy matrix
/// `SessionActivity::Authenticating` above.)
#[tokio::test]
async fn authentication_timeout_releases_the_connection_slot() {
    const SLOTS: usize = 2;
    let t = ConnectionTimeouts {
        authentication_timeout: Duration::from_secs(2),
        ..all_disabled()
    };
    let (addr, cs, _h) = setup(SLOTS, t).await;

    let mut scanners = Vec::new();
    for _ in 0..SLOTS {
        scanners.push(silent_scanner_socket(addr).await);
    }
    // All slots consumed.
    assert!(
        !try_full_session(&cs).await,
        "sanity: silent sockets should have consumed all {SLOTS} slots"
    );

    // Within authentication_timeout + slack, the slots must come back WITHOUT
    // the scanner sockets being closed by us.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut recovered = false;
    while Instant::now() < deadline {
        if try_full_session(&cs).await {
            recovered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    drop(scanners);
    assert!(
        recovered,
        "GH #28: connection slots held by silent scanner sockets were never \
         reclaimed — `Connection limit reached` forever"
    );
}

// ---------------------------------------------------------------------------
// idle_session_timeout
// ---------------------------------------------------------------------------

/// An authenticated session that goes quiet past `idle_session_timeout` is
/// closed, and its next statement fails.
#[tokio::test]
async fn idle_session_timeout_closes_an_idle_session() {
    let t = ConnectionTimeouts {
        idle_session_timeout: Duration::from_secs(2),
        ..all_disabled()
    };
    let (_addr, cs, _h) = setup(8, t).await;

    let (client, task) = connect(&cs).await;
    client.simple_query("SELECT 1").await.expect("first query");
    tokio::time::sleep(Duration::from_secs(6)).await;

    let after = tokio::time::timeout(Duration::from_secs(5), client.simple_query("SELECT 1")).await;
    assert!(
        matches!(after, Ok(Err(_)) | Err(_)),
        "GH #28: a session idle for 6s survived idle_session_timeout = 2s"
    );
    task.abort();
}

/// …and the slot it held is returned to the pool.
#[tokio::test]
async fn idle_session_timeout_returns_the_connection_slot() {
    let t = ConnectionTimeouts {
        idle_session_timeout: Duration::from_secs(2),
        ..all_disabled()
    };
    let (_addr, cs, _h) = setup(1, t).await;

    let (client, task) = connect(&cs).await;
    client.simple_query("SELECT 1").await.expect("first query");
    assert!(!try_full_session(&cs).await, "sanity: the single slot is taken");

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut recovered = false;
    while Instant::now() < deadline {
        if try_full_session(&cs).await {
            recovered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    drop(client);
    task.abort();
    assert!(recovered, "the slot of an idle-timed-out session was never released");
}

/// A session that keeps issuing statements must NEVER be closed, however small
/// `idle_session_timeout` is. This is the regression guard against a naive
/// "close after N seconds of connection age" implementation.
#[tokio::test]
async fn busy_session_is_never_closed_by_idle_session_timeout() {
    let t = ConnectionTimeouts {
        idle_session_timeout: Duration::from_secs(1),
        ..all_disabled()
    };
    let (_addr, cs, _h) = setup(8, t).await;

    let (client, task) = connect(&cs).await;
    let started = Instant::now();
    let mut n = 0u32;
    while started.elapsed() < Duration::from_secs(5) {
        client
            .simple_query("SELECT 1")
            .await
            .unwrap_or_else(|e| panic!("busy session killed after {n} queries / {:?}: {e}", started.elapsed()));
        n += 1;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert!(n >= 10, "busy loop did not actually exercise the connection (n = {n})");
    drop(client);
    task.abort();
}

// ---------------------------------------------------------------------------
// idle_in_transaction_session_timeout
// ---------------------------------------------------------------------------

/// A session that opens a transaction and goes quiet is closed after
/// `idle_in_transaction_session_timeout`, freeing its slot and its locks.
#[tokio::test]
async fn idle_in_transaction_session_timeout_closes_an_open_transaction() {
    let t = ConnectionTimeouts {
        idle_in_transaction_session_timeout: Duration::from_secs(2),
        ..all_disabled()
    };
    let (_addr, cs, _h) = setup(8, t).await;

    let (client, task) = connect(&cs).await;
    client
        .simple_query("CREATE TABLE t (id INT PRIMARY KEY, v INT)")
        .await
        .expect("ddl");
    client.simple_query("BEGIN").await.expect("begin");
    client
        .simple_query("INSERT INTO t VALUES (1, 1)")
        .await
        .expect("insert in txn");
    tokio::time::sleep(Duration::from_secs(6)).await;

    let after = tokio::time::timeout(Duration::from_secs(5), client.simple_query("SELECT 1")).await;
    assert!(
        matches!(after, Ok(Err(_)) | Err(_)),
        "GH #28: a session idle IN TRANSACTION for 6s survived \
         idle_in_transaction_session_timeout = 2s"
    );
    task.abort();

    // The aborted transaction must have rolled back, not committed.
    let (verify, vtask) = connect(&cs).await;
    let rows = verify
        .simple_query("SELECT COUNT(*) FROM t")
        .await
        .expect("count after forced disconnect");
    let count: i64 = rows
        .iter()
        .find_map(|m| match m {
            tokio_postgres::SimpleQueryMessage::Row(r) => r.get(0).map(|s| s.parse().expect("int")),
            _ => None,
        })
        .expect("a count row");
    assert_eq!(count, 0, "an idle-in-transaction disconnect must ROLL BACK, not commit");
    drop(verify);
    vtask.abort();
}

/// `idle_in_transaction_session_timeout` must NOT close a session that is
/// merely idle outside a transaction (that is `idle_session_timeout`'s job).
#[tokio::test]
async fn idle_in_transaction_timeout_does_not_touch_a_plain_idle_session() {
    let t = ConnectionTimeouts {
        idle_in_transaction_session_timeout: Duration::from_secs(1),
        idle_session_timeout: Duration::ZERO,
        ..all_disabled()
    };
    let (_addr, cs, _h) = setup(8, t).await;

    let (client, task) = connect(&cs).await;
    client.simple_query("SELECT 1").await.expect("first query");
    tokio::time::sleep(Duration::from_secs(4)).await;
    client
        .simple_query("SELECT 1")
        .await
        .expect("a plain idle session was closed by idle_in_transaction_session_timeout");
    drop(client);
    task.abort();
}

/// After COMMIT the session is no longer "idle in transaction": the in-txn
/// deadline must stop applying.
#[tokio::test]
async fn committed_session_falls_back_to_the_plain_idle_deadline() {
    let t = ConnectionTimeouts {
        idle_in_transaction_session_timeout: Duration::from_secs(1),
        idle_session_timeout: Duration::ZERO,
        ..all_disabled()
    };
    let (_addr, cs, _h) = setup(8, t).await;

    let (client, task) = connect(&cs).await;
    client.simple_query("BEGIN").await.expect("begin");
    client.simple_query("COMMIT").await.expect("commit");
    tokio::time::sleep(Duration::from_secs(4)).await;
    client
        .simple_query("SELECT 1")
        .await
        .expect("session closed after COMMIT although idle_session_timeout is 0");
    drop(client);
    task.abort();
}

// ---------------------------------------------------------------------------
// A partially-received message must never be killed by an idle deadline
// ---------------------------------------------------------------------------

/// `PgConnectionHandler::read_message` (src/protocol/postgres/handler.rs:796-832)
/// LOOPS on `stream.read()` until a complete frontend message parses. Wrapping
/// the whole call in `tokio::time::timeout(idle_deadline, …)` — the obvious
/// implementation — would therefore kill a client that is *mid-message*: a
/// large `Bind` with parameters, or a `Query` split across TCP segments on a
/// slow uplink, is indistinguishable from an idle session.
///
/// PostgreSQL's `idle_session_timeout` measures the wait for a NEW command and
/// stops the moment the command starts arriving. The required implementation is
/// therefore: apply the deadline to each individual `read()`, and RESET it on
/// any read that returns ≥ 1 byte.
///
/// This test drives the raw wire so it can split a Query message in half.
#[tokio::test]
async fn a_partially_received_message_is_not_killed_by_the_idle_deadline() {
    use tokio::io::AsyncWriteExt;

    let t = ConnectionTimeouts {
        idle_session_timeout: Duration::from_secs(1),
        ..all_disabled()
    };
    let (addr, _cs, _h) = setup(8, t).await;

    let mut sock = TcpStream::connect(addr).await.expect("connect");

    // StartupMessage: int32 len | int32 196608 | "user\0postgres\0database\0postgres\0" | \0
    let mut params = Vec::new();
    params.extend_from_slice(b"user\0postgres\0database\0postgres\0\0");
    let len = (4 + 4 + params.len()) as i32;
    let mut startup = Vec::new();
    startup.extend_from_slice(&len.to_be_bytes());
    startup.extend_from_slice(&196_608i32.to_be_bytes());
    startup.extend_from_slice(&params);
    sock.write_all(&startup).await.expect("write startup");
    sock.flush().await.expect("flush startup");

    // Drain AuthenticationOk / ParameterStatus / ReadyForQuery.
    let mut scratch = [0u8; 8192];
    let mut saw_ready = false;
    let handshake_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < handshake_deadline && !saw_ready {
        match tokio::time::timeout(Duration::from_millis(500), sock.read(&mut scratch)).await {
            Ok(Ok(n)) if n > 0 => saw_ready = scratch[..n].contains(&b'Z'),
            _ => break,
        }
    }
    assert!(
        saw_ready,
        "positive control: trust handshake did not reach ReadyForQuery"
    );

    // Query message, split in two with a pause LONGER than idle_session_timeout
    // in the middle. Body = "SELECT 1\0"; len covers the length field itself.
    let body = b"SELECT 1\0";
    let qlen = (4 + body.len()) as i32;
    let mut header = vec![b'Q'];
    header.extend_from_slice(&qlen.to_be_bytes());
    sock.write_all(&header).await.expect("write query header");
    sock.flush().await.expect("flush header");

    tokio::time::sleep(Duration::from_millis(2500)).await;

    sock.write_all(body).await.expect("write query body");
    sock.flush().await.expect("flush body");

    // The server must answer, not have closed us mid-message.
    let mut got = Vec::new();
    let reply_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < reply_deadline {
        match tokio::time::timeout(Duration::from_millis(500), sock.read(&mut scratch)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                got.extend_from_slice(&scratch[..n]);
                if got.contains(&b'Z') {
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(
        got.contains(&b'T') || got.contains(&b'D'),
        "a Query split across a 2.5s gap (idle_session_timeout = 1s) was killed \
         mid-message: the deadline must wrap ONE read() and reset on any byte \
         received, never the whole read_message() loop \
         (src/protocol/postgres/handler.rs:796-832). Got {} bytes: {:?}",
        got.len(),
        String::from_utf8_lossy(&got[..got.len().min(120)])
    );
}

// ---------------------------------------------------------------------------
// Session GUC surface (`SET`/`SHOW`) — BOTH executor families, BOTH protocols
// ---------------------------------------------------------------------------
//
// This is the half the first triage pass got wrong. Registering the GUCs in
// `SessionSettings::new` covers ONLY `EmbeddedDatabase::execute` /
// `::query` / `::query_with_columns` — the three callers of
// `try_handle_db_setting_statement_with_columns` (src/lib.rs:8766, :16494,
// :17056). It does NOT cover:
//
//   * the PG wire SHOW arm, which is a hardcoded match in
//     `resolve_show_parameter` (src/protocol/postgres/handler.rs:2620-2641)
//     and never consults `SessionSettings`;
//   * the PG wire generic SET arm, which acks unknown names with a bare
//     `CommandComplete("SET")` (handler.rs:1122-1124);
//   * the EXTENDED protocol, where `handler_extended` delegates only
//     transaction control and `SET ROLE` back to `handle_single_query`
//     (src/protocol/postgres/handler_extended.rs:219-238) and everything else
//     goes to the params family, which has no generic `SHOW` LogicalPlan.
//
// All four surfaces are asserted below.

/// Text/embedded family: `SHOW` answers from the registry, `SET` sticks, and
/// the postmaster-scoped one is refused.
#[test]
fn timeout_gucs_are_visible_to_show_text_family() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    for name in [
        "idle_session_timeout",
        "idle_in_transaction_session_timeout",
        "authentication_timeout",
    ] {
        let rows = db
            .query(&format!("SHOW {name}"), &[])
            .unwrap_or_else(|e| panic!("SHOW {name} failed: {e}"));
        assert_eq!(rows.len(), 1, "SHOW {name} returned {} rows", rows.len());
    }

    // Positive control: an already-registered GUC behaves the same way, so a
    // failure above is about the NEW names, not about SHOW being broken.
    assert_eq!(db.query("SHOW statement_timeout", &[]).expect("show control").len(), 1);

    // SET must take effect for the two session-scoped ones…
    db.execute("SET idle_session_timeout = '30s'").expect("set idle");
    let rows = db.query("SHOW idle_session_timeout", &[]).expect("show idle");
    let shown = match rows.first().and_then(|t| t.values.first()) {
        Some(heliosdb_nano::Value::String(s)) => s.clone(),
        other => panic!("unexpected SHOW result: {other:?}"),
    };
    assert!(
        shown.contains("30"),
        "SET idle_session_timeout did not stick (SHOW returned {shown:?})"
    );

    // …and authentication_timeout is server-scoped: PostgreSQL rejects SET on
    // it. `SessionSettings::set` already fails closed for read-only names
    // (src/sql/settings.rs:172-176), so the fix only has to add the name to
    // `is_read_only` (src/sql/settings.rs:203).
    assert!(
        db.execute("SET authentication_timeout = '5s'").is_err(),
        "authentication_timeout is a postmaster-scoped setting and must not be SET-able per session"
    );
}

/// PARAMS family — the one the REST layer and every server-side-binding driver
/// uses. `query_params` does NOT call
/// `try_handle_db_setting_statement_with_columns`; a fix that only edits the
/// text family leaves this failing.
#[test]
fn timeout_gucs_are_visible_to_show_params_family() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    for name in [
        "idle_session_timeout",
        "idle_in_transaction_session_timeout",
        "authentication_timeout",
    ] {
        let rows = db
            .query_params(&format!("SHOW {name}"), &[])
            .unwrap_or_else(|e| panic!("params-family SHOW {name} failed: {e}"));
        assert_eq!(
            rows.len(),
            1,
            "params-family SHOW {name} returned {} rows — the params executor \
             (EmbeddedDatabase::query_params, src/lib.rs:19194) must reach the \
             same SessionSettings registry as the text executor",
            rows.len()
        );
    }

    db.execute_params("SET idle_session_timeout = '45s'", &[])
        .expect("params-family SET idle_session_timeout");
    let rows = db
        .query_params("SHOW idle_session_timeout", &[])
        .expect("params-family SHOW after SET");
    let shown = match rows.first().and_then(|t| t.values.first()) {
        Some(heliosdb_nano::Value::String(s)) => s.clone(),
        other => panic!("unexpected params-family SHOW result: {other:?}"),
    };
    assert!(
        shown.contains("45"),
        "params-family SET idle_session_timeout did not stick (SHOW returned {shown:?})"
    );

    assert!(
        db.execute_params("SET authentication_timeout = '5s'", &[]).is_err(),
        "params family must refuse the postmaster-scoped authentication_timeout too"
    );
}

/// SIMPLE wire protocol: `SHOW` must answer from the effective policy, and
/// `SET` of the postmaster-scoped name must be an ErrorResponse rather than the
/// generic ack.
#[tokio::test]
async fn timeout_gucs_over_the_simple_wire_protocol() {
    let t = ConnectionTimeouts {
        idle_session_timeout: Duration::from_secs(600),
        ..all_disabled()
    };
    let (_addr, cs, _h) = setup(8, t).await;
    let (client, task) = connect(&cs).await;

    // Positive control: a parameter the hardcoded table already knew.
    let ctl = client.simple_query("SHOW server_version").await.expect("control SHOW");
    assert!(
        first_simple_value(&ctl).is_some_and(|v| !v.is_empty()),
        "control: SHOW server_version must answer"
    );

    let msgs = client
        .simple_query("SHOW idle_session_timeout")
        .await
        .expect("SHOW idle_session_timeout over the wire");
    let v = first_simple_value(&msgs).expect("no row");
    assert!(
        v.contains("600") || v.contains("10min"),
        "the wire SHOW arm must report the EFFECTIVE idle_session_timeout \
         (600s was configured), not the empty-string catch-all at \
         src/protocol/postgres/handler.rs:2638 — got {v:?}"
    );

    // `SET` of a session-scoped timeout must stick, not be generically acked.
    client
        .simple_query("SET idle_session_timeout = '30s'")
        .await
        .expect("SET idle_session_timeout");
    let msgs = client.simple_query("SHOW idle_session_timeout").await.expect("SHOW");
    let v = first_simple_value(&msgs).expect("no row");
    assert!(
        v.contains("30"),
        "SET idle_session_timeout over the wire did not stick (SHOW returned {v:?}) — \
         the generic CommandComplete(\"SET\") at handler.rs:1122-1124 swallowed it"
    );

    // Fail-closed: the postmaster-scoped one must be refused, not acked.
    assert!(
        client.simple_query("SET authentication_timeout = '5s'").await.is_err(),
        "SET authentication_timeout must be an ErrorResponse (the HC4 SET ROLE \
         precedent at handler.rs:1075-1090: a silently-acked security-relevant \
         SET is a false security claim)"
    );

    drop(client);
    task.abort();
}

/// EXTENDED wire protocol (`Client::query` = Parse/Bind/Execute) — psycopg3,
/// JDBC, sqlx, node-postgres and the REST layer.
#[tokio::test]
async fn timeout_gucs_over_the_extended_wire_protocol() {
    let t = ConnectionTimeouts {
        idle_session_timeout: Duration::from_secs(600),
        ..all_disabled()
    };
    let (_addr, cs, _h) = setup(8, t).await;
    let (client, task) = connect(&cs).await;

    // Positive control: the extended path works at all on this harness.
    let ctl = client.query("SELECT 1", &[]).await.expect("control extended SELECT");
    assert_eq!(ctl.len(), 1, "control: extended SELECT 1 must return one row");

    let rows = client
        .query("SHOW idle_session_timeout", &[])
        .await
        .expect("extended SHOW idle_session_timeout");
    assert_eq!(
        rows.len(),
        1,
        "the extended protocol must serve SHOW too — handler_extended.rs:219-238 \
         currently delegates only transaction control and SET ROLE back to \
         handle_single_query"
    );

    client
        .execute("SET idle_session_timeout = '30s'", &[])
        .await
        .expect("extended SET idle_session_timeout");
    let rows = client
        .query("SHOW idle_session_timeout", &[])
        .await
        .expect("extended SHOW");
    let v: String = rows
        .first()
        .and_then(|r| r.try_get::<_, String>(0).ok())
        .expect("extended SHOW value");
    assert!(
        v.contains("30"),
        "extended-protocol SET idle_session_timeout did not stick (SHOW returned {v:?})"
    );

    assert!(
        client.execute("SET authentication_timeout = '5s'", &[]).await.is_err(),
        "the extended protocol must refuse the postmaster-scoped authentication_timeout too"
    );

    drop(client);
    task.abort();
}

/// #28 ask 4: the limit the server reports must be the limit it enforces —
/// `resolve_show_parameter` hardcodes `"100"` at
/// src/protocol/postgres/handler.rs:2632, and src/main.rs:657 prints the same
/// literal in the startup banner.
#[tokio::test]
async fn show_max_connections_reports_the_configured_limit() {
    let (_addr, cs, _h) = setup(7, all_disabled()).await;
    let (client, task) = connect(&cs).await;

    let msgs = client.simple_query("SHOW max_connections").await.expect("SHOW");
    let v = first_simple_value(&msgs).expect("no row");
    assert_eq!(
        v, "7",
        "SHOW max_connections must report the effective PgServerConfig::max_connections, \
         not the literal \"100\" hardcoded at src/protocol/postgres/handler.rs:2632"
    );

    drop(client);
    task.abort();
}
