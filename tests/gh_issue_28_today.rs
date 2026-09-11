//! GH #28 — PROOF-ON-THE-CURRENT-TREE repro.
//!
//! Install as `tests/gh_issue_28_today.rs`.
//!
//! This file uses ONLY the API that exists on main today (v4.31.1). It compiles
//! against the unfixed tree and FIVE of its tests FAIL there. It is the
//! evidence half of the deliverable; the permanent regression suite for the
//! feature is `tests/gh_issue_28.rs` (see `repro_28.rs`), which necessarily
//! references the new configuration surface and therefore cannot compile until
//! the fix lands.
//!
//! # What #28 reports
//!
//! Internet scanners open a TCP socket to the PostgreSQL port and never send a
//! startup packet. Each such socket consumes one of the `--max-connections`
//! slots forever, because:
//!
//!   * `PgServer::serve` takes the semaphore permit BEFORE reading a single
//!     byte and hands it to the spawned task
//!     (`src/protocol/postgres/server.rs:189-214`);
//!   * `PgServer::handle_connection` then does an unbounded
//!     `stream.read_exact(&mut len_buf)` with no timeout
//!     (`src/protocol/postgres/server.rs:233-247`);
//!   * there is no `authentication_timeout`, no `idle_session_timeout` and no
//!     TCP keepalive anywhere on the accepted socket — `set_nodelay` at
//!     `src/protocol/postgres/server.rs:184` is the only socket option applied
//!     (repo-wide grep for `set_keepalive|TcpKeepalive|SO_KEEPALIVE|socket2`
//!     over `src/` returns nothing).
//!
//! So the permit is released only when the peer closes. 99 dead scanner sockets
//! + 1 pooled application connection = a server that rejects everything with
//! `Connection limit reached (100), rejecting …`.
//!
//! And the reporting/observability half of #28 (ask 4) is broken independently
//! of any socket, on BOTH executor families:
//!
//!   * `PgConnectionHandler::resolve_show_parameter`
//!     (`src/protocol/postgres/handler.rs:2620-2641`) is a hardcoded lookup
//!     table that answers `SHOW max_connections` with the literal string
//!     `"100"` (`handler.rs:2632`) no matter what `--max-connections` says —
//!     the same lie as the startup banner's literal
//!     `println!("      - Max connections: 100")` at `src/main.rs:657`;
//!   * that table answers every unknown parameter with `String::new()`
//!     (`handler.rs:2638`), so `SHOW idle_session_timeout` returns a row
//!     containing an EMPTY STRING rather than erroring or answering;
//!   * the generic wire `SET` arm acks anything it does not recognise with a
//!     bare `CommandComplete("SET")` (`handler.rs:1122-1124`), so
//!     `SET authentication_timeout = …` is silently swallowed — exactly the
//!     "false security claim" failure mode the HC4 `SET ROLE` fix at
//!     `handler.rs:1075-1090` already ruled unacceptable in this repo;
//!   * and the EXTENDED (params) family does not even reach that code:
//!     `handler_extended` only delegates transaction control and `SET ROLE`
//!     back to `handle_single_query`
//!     (`src/protocol/postgres/handler_extended.rs:219-238`), so psycopg3 /
//!     JDBC / sqlx / node-postgres send `SHOW idle_session_timeout` into
//!     `EmbeddedDatabase::query_params` — which never calls
//!     `try_handle_db_setting_statement_with_columns` (its only callers are
//!     `execute` at `src/lib.rs:8766`, `query` at `src/lib.rs:16494` and
//!     `query_with_columns` at `src/lib.rs:17056`) and has no `LogicalPlan`
//!     variant for a generic `SHOW`, so it fails to parse.
//!
//! # Expected outcome of THIS file on the CURRENT tree
//!
//! PASS (positive controls — the harness itself is sound):
//!   * `positive_control_server_serves_a_query`
//!   * `positive_control_connection_limit_rejects_when_full`
//!   * `positive_control_slots_are_released_when_the_peer_closes`
//!   * `positive_control_wire_show_answers_a_known_parameter`
//!   * `positive_control_text_family_show_reads_the_settings_registry`
//!   * `server_config_carries_a_dead_idle_timeout_knob`
//!
//! FAIL (the bug):
//!   * `silent_scanner_sockets_never_release_connection_slots`   <- the outage
//!   * `wire_show_max_connections_reports_the_configured_limit`  <- ask 4
//!   * `wire_show_idle_session_timeout_is_answered`              <- ask 1
//!   * `wire_set_authentication_timeout_is_not_silently_swallowed` <- ask 2
//!   * `extended_family_show_idle_session_timeout_is_answered`   <- ask 1, params family
//!
//! After the fix all eleven PASS. Only the first failing test is slow (~62 s on
//! a fixed tree, 75 s on an unfixed one); the other four are sub-second. Do NOT
//! `#[ignore]` any of them.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::EmbeddedDatabase;

use tokio::net::TcpStream;
use tokio_postgres::{NoTls, SimpleQueryMessage};

// ---------------------------------------------------------------------------
// Harness (same shape as tests/v334_a8_connection_resilience.rs)
// ---------------------------------------------------------------------------

/// Start an in-process `PgServer` on an ephemeral loopback port.
/// Returns (addr, connection string, server task handle).
async fn setup(max_connections: usize) -> (std::net::SocketAddr, String, tokio::task::JoinHandle<()>) {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
    let addr = probe.local_addr().expect("local_addr");
    drop(probe);

    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let config = PgServerConfig::with_address(addr).with_max_connections(max_connections);
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

/// Try one full PostgreSQL handshake + query. `true` when the server served us.
async fn try_full_session(cs: &str) -> bool {
    match tokio::time::timeout(Duration::from_secs(5), tokio_postgres::connect(cs, NoTls)).await {
        Ok(Ok((client, conn))) => {
            let conn_task = tokio::spawn(async move {
                let _ = conn.await;
            });
            let ok = matches!(
                tokio::time::timeout(Duration::from_secs(5), client.simple_query("SELECT 1")).await,
                Ok(Ok(_))
            );
            drop(client);
            conn_task.abort();
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

/// Open a raw TCP socket and send NOTHING — an internet scanner / half-open
/// probe. The returned stream is kept alive by the caller; dropping it is what
/// a well-behaved client would do and is exactly what a scanner does not do.
async fn silent_scanner_socket(addr: std::net::SocketAddr) -> TcpStream {
    let stream = TcpStream::connect(addr).await.expect("scanner connect");
    // Prove the server accepted it and is not sending us anything either: the
    // server is blocked in read_exact() waiting for a startup packet. `peek`
    // takes &self and does not consume bytes, so this observation is free.
    let mut probe = [0u8; 1];
    let peeked = tokio::time::timeout(Duration::from_millis(300), stream.peek(&mut probe)).await;
    assert!(
        peeked.is_err(),
        "server unexpectedly wrote to (or closed) a socket that has sent no startup packet"
    );
    stream
}

// ---------------------------------------------------------------------------
// Positive controls — these pass BOTH before and after the fix. If any of them
// fails, the harness itself is broken and the bug tests below mean nothing.
// ---------------------------------------------------------------------------

/// Control 1: the in-process listener really serves PostgreSQL clients.
#[tokio::test]
async fn positive_control_server_serves_a_query() {
    let (_addr, cs, _h) = setup(8).await;
    assert!(
        try_full_session(&cs).await,
        "positive control failed: the harness cannot even run SELECT 1"
    );
}

/// Control 2: `max_connections` is really enforced — this is the mechanism the
/// issue's `Connection limit reached (N), rejecting …` line comes from
/// (`src/protocol/postgres/server.rs:189-200`).
#[tokio::test]
async fn positive_control_connection_limit_rejects_when_full() {
    let (_addr, cs, _h) = setup(1).await;

    // Hold the single slot with a live, fully authenticated session.
    let (holder, holder_conn) = tokio_postgres::connect(&cs, NoTls).await.expect("holder connect");
    let holder_task = tokio::spawn(async move {
        let _ = holder_conn.await;
    });
    holder.simple_query("SELECT 1").await.expect("holder query");

    assert!(
        !try_full_session(&cs).await,
        "the server accepted a second connection with max_connections = 1"
    );

    drop(holder);
    holder_task.abort();
}

/// Control 3: a slot IS released when the peer closes the socket. This isolates
/// the defect: permit accounting is sound; what is missing is any server-side
/// deadline that closes a peer which never speaks.
#[tokio::test]
async fn positive_control_slots_are_released_when_the_peer_closes() {
    let (addr, cs, _h) = setup(1).await;

    // Take the only slot with a silent socket, then close it ourselves.
    let scanner = silent_scanner_socket(addr).await;
    assert!(
        !try_full_session(&cs).await,
        "a silent socket did not consume the single connection slot"
    );
    drop(scanner);

    // The slot must come back promptly once the peer really goes away.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut recovered = false;
    while Instant::now() < deadline {
        if try_full_session(&cs).await {
            recovered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        recovered,
        "connection slot was not released after the client closed the socket"
    );
}

/// Control 4: the wire `SHOW` arm answers a parameter it knows about. This
/// proves the assertions below are testing the VALUE, not a dead code path.
#[tokio::test]
async fn positive_control_wire_show_answers_a_known_parameter() {
    let (_addr, cs, _h) = setup(8).await;
    let (client, conn) = tokio_postgres::connect(&cs, NoTls).await.expect("connect");
    let task = tokio::spawn(async move {
        let _ = conn.await;
    });

    let msgs = client.simple_query("SHOW server_version").await.expect("SHOW");
    let v = first_simple_value(&msgs).expect("SHOW server_version returned no row");
    assert!(
        !v.is_empty(),
        "positive control failed: SHOW server_version returned an empty value"
    );

    drop(client);
    task.abort();
}

/// Control 5: the TEXT executor family really does serve `SHOW` from the
/// session-settings registry (`src/lib.rs:1802-1815`). This proves that
/// registering a GUC there is a real mechanism, and that the
/// `SHOW idle_session_timeout` assertion below fails for the right reason
/// (the setting is not registered) rather than because SHOW is broken.
#[test]
fn positive_control_text_family_show_reads_the_settings_registry() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    let rows = db.query("SHOW statement_timeout", &[]).expect("SHOW statement_timeout");
    assert_eq!(rows.len(), 1, "SHOW statement_timeout must return exactly one row");
}

/// Control 6: the `[server]` section already carries an `idle_timeout_secs`
/// knob with a 300 s default, and NOTHING on the serving path reads it. This
/// asserts only the part that is true today (the knob exists and defaults to
/// 300) so it keeps passing after the fix.
#[test]
fn server_config_carries_a_dead_idle_timeout_knob() {
    let cfg = heliosdb_nano::Config::default();
    assert_eq!(
        cfg.server.idle_timeout_secs, 300,
        "[server] idle_timeout_secs default changed; \
         src/config.rs:636-638 / src/config.rs:1113-1114"
    );
    // NOTE for the implementer: grep for CALLERS of this field. On v4.31.1 the
    // only consumers are src/network/server.rs:61/73/134/203 and
    // src/network/session.rs:92-120 — the legacy PG stack behind the
    // non-default `legacy-network` cargo feature (Cargo.toml:231-237). The
    // production listener, src/protocol/postgres/server.rs, never reads it, and
    // neither does `[server] max_connections`: a repo-wide grep for
    // `server\.max_connections` over src/ returns NOTHING, so the key
    // documented at config.example.toml:257 is inert and only the clap flag
    // (src/main.rs:163-167 -> :337 -> :642) has any effect.
}

// ---------------------------------------------------------------------------
// The bug (#28), part 1: the outage itself
// ---------------------------------------------------------------------------

/// A socket that connects and never sends a startup packet must not hold a
/// connection slot forever. PostgreSQL closes it after `authentication_timeout`
/// (default 60 s). HeliosDB Nano on main has no such deadline, so the slot is
/// held until the peer closes — which a scanner never does.
///
/// Fails on the current tree; passes once `authentication_timeout` is enforced
/// (with the PostgreSQL-compatible default of 60 s, or anything shorter).
#[tokio::test]
async fn silent_scanner_sockets_never_release_connection_slots() {
    const SLOTS: usize = 2;
    // 60 s (PostgreSQL's default authentication_timeout) + generous slack for a
    // loaded CI box. The loop polls, so a shorter configured timeout wins early.
    // If the default is ever changed, this budget must stay above it.
    const DEADLINE: Duration = Duration::from_secs(75);

    let (addr, cs, _h) = setup(SLOTS).await;

    // Wedge every slot with scanner sockets that never speak.
    let mut scanners = Vec::new();
    for _ in 0..SLOTS {
        scanners.push(silent_scanner_socket(addr).await);
    }

    assert!(
        !try_full_session(&cs).await,
        "sanity: {SLOTS} silent sockets should have consumed all {SLOTS} slots"
    );

    let started = Instant::now();
    let mut recovered_after = None;
    while started.elapsed() < DEADLINE {
        if try_full_session(&cs).await {
            recovered_after = Some(started.elapsed());
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // Keep the scanner sockets open for the whole window: the server, not the
    // client, must be the one that gives up.
    drop(scanners);

    assert!(
        recovered_after.is_some(),
        "GH #28: {SLOTS} sockets that never sent a startup packet held every \
         connection slot for {DEADLINE:?}. There is no authentication_timeout on \
         the accept path (src/protocol/postgres/server.rs:189-214 takes the \
         permit before src/protocol/postgres/server.rs:233-247 blocks forever in \
         read_exact), so a port scanner permanently exhausts --max-connections."
    );
}

// ---------------------------------------------------------------------------
// The bug (#28), part 2: ask 4 — the reported limit is a hardcoded lie
// ---------------------------------------------------------------------------

/// `SHOW max_connections` must report the limit this server is actually
/// enforcing. `PgConnectionHandler::resolve_show_parameter` hardcodes `"100"`
/// (src/protocol/postgres/handler.rs:2632), so an operator who set
/// `--max-connections 7` is told 7 slots are 100 — and a pooler that sizes
/// itself from `SHOW max_connections` will happily open 100 and be rejected.
/// This is the same literal-100 defect as the startup banner (src/main.rs:657).
#[tokio::test]
async fn wire_show_max_connections_reports_the_configured_limit() {
    let (_addr, cs, _h) = setup(7).await;
    let (client, conn) = tokio_postgres::connect(&cs, NoTls).await.expect("connect");
    let task = tokio::spawn(async move {
        let _ = conn.await;
    });

    let msgs = client.simple_query("SHOW max_connections").await.expect("SHOW");
    let v = first_simple_value(&msgs).expect("SHOW max_connections returned no row");

    drop(client);
    task.abort();

    assert_eq!(
        v, "7",
        "GH #28 ask 4: the server was started with max_connections = 7 but \
         SHOW max_connections answered {v:?} — resolve_show_parameter hardcodes \
         \"100\" at src/protocol/postgres/handler.rs:2632 and never consults the \
         effective PgServerConfig::max_connections."
    );
}

// ---------------------------------------------------------------------------
// The bug (#28), part 3: the timeout GUCs do not exist on either family
// ---------------------------------------------------------------------------

/// PostgreSQL exposes `idle_session_timeout` as a GUC. On main the wire `SHOW`
/// arm falls through to `resolve_show_parameter`'s `_ => String::new()` catch-
/// all (src/protocol/postgres/handler.rs:2638), so the client is handed a row
/// containing an EMPTY STRING — worse than an error, because a client cannot
/// tell "not supported" from "set to nothing".
#[tokio::test]
async fn wire_show_idle_session_timeout_is_answered() {
    let (_addr, cs, _h) = setup(8).await;
    let (client, conn) = tokio_postgres::connect(&cs, NoTls).await.expect("connect");
    let task = tokio::spawn(async move {
        let _ = conn.await;
    });

    let msgs = client.simple_query("SHOW idle_session_timeout").await.expect("SHOW");
    let v = first_simple_value(&msgs).expect("SHOW idle_session_timeout returned no row");

    drop(client);
    task.abort();

    assert!(
        !v.trim().is_empty(),
        "GH #28 ask 1: SHOW idle_session_timeout returned {v:?} (the empty-string \
         catch-all at src/protocol/postgres/handler.rs:2638). The setting is not \
         registered in SessionSettings::new (src/sql/settings.rs:96-167) and the \
         wire SHOW arm does not consult that registry at all."
    );
}

/// `authentication_timeout` is postmaster-scoped in PostgreSQL: a session must
/// NOT be able to change it. On main the generic wire `SET` arm acks anything
/// it does not recognise (`send_command_complete("SET")`,
/// src/protocol/postgres/handler.rs:1122-1124), so the statement succeeds and
/// has zero effect — the identical "silently acked a security-relevant SET"
/// defect the HC4 `SET ROLE` fix (handler.rs:1075-1090) already ruled out.
///
/// Fail-closed requirement: this must be an ERROR, not a silent ack.
#[tokio::test]
async fn wire_set_authentication_timeout_is_not_silently_swallowed() {
    let (_addr, cs, _h) = setup(8).await;
    let (client, conn) = tokio_postgres::connect(&cs, NoTls).await.expect("connect");
    let task = tokio::spawn(async move {
        let _ = conn.await;
    });

    let res = client.simple_query("SET authentication_timeout = '5s'").await;

    drop(client);
    task.abort();

    assert!(
        res.is_err(),
        "GH #28 ask 2: `SET authentication_timeout = '5s'` was acked with a bare \
         CommandComplete(\"SET\") and had no effect. A postmaster-scoped, \
         security-relevant setting must be REFUSED, not silently swallowed."
    );
}

/// CROSS-FAMILY. `tokio_postgres::Client::query` uses the EXTENDED protocol
/// (Parse/Bind/Execute) — the path psycopg3, JDBC, sqlx and node-postgres all
/// take, and the path the REST layer takes through
/// `execute_plan_with_params_inner`. `handler_extended` only delegates
/// transaction control and `SET ROLE` back to `handle_single_query`
/// (src/protocol/postgres/handler_extended.rs:219-238), so `SHOW` lands in
/// `EmbeddedDatabase::query_params` — which never calls
/// `try_handle_db_setting_statement_with_columns` and has no generic `SHOW`
/// LogicalPlan variant. A fix that only touches the text family leaves every
/// real driver broken.
#[tokio::test]
async fn extended_family_show_idle_session_timeout_is_answered() {
    let (_addr, cs, _h) = setup(8).await;
    let (client, conn) = tokio_postgres::connect(&cs, NoTls).await.expect("connect");
    let task = tokio::spawn(async move {
        let _ = conn.await;
    });

    let res = client.query("SHOW idle_session_timeout", &[]).await;

    let outcome = match &res {
        Ok(rows) => format!("Ok({} rows)", rows.len()),
        Err(e) => format!("Err({e})"),
    };
    let one_row = matches!(&res, Ok(rows) if rows.len() == 1);

    drop(client);
    task.abort();

    assert!(
        one_row,
        "GH #28 ask 1 (params family): `SHOW idle_session_timeout` over the \
         EXTENDED protocol returned {outcome}, expected exactly one row. \
         handler_extended.rs:219-238 delegates only transaction control and \
         SET ROLE to handle_single_query, so every server-side-binding driver \
         (psycopg3, JDBC, sqlx, node-postgres) misses the SHOW arm entirely."
    );
}
