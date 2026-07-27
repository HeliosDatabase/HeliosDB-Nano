//! SIGTERM must run the same clean-shutdown path as SIGINT.
//!
//! Every server entry point used to await `tokio::signal::ctrl_c()` and nothing
//! else, so SIGTERM kept its default Unix disposition: immediate termination,
//! no unwinding, no `Drop`. `EmbeddedDatabase::drop`'s ordered close-time work
//! — flush the durable row counters, THEN checkpoint the R4.2 index snapshots
//! — therefore never ran under any service manager, nor under this binary's own
//! `stop` subcommand, which sends `kill -TERM` and then waits two seconds for a
//! graceful shutdown that could not happen.
//!
//! This spawns the real `heliosdb-nano` binary, signals the real process, and
//! inspects the real on-disk state afterwards. The observable proof that `Drop`
//! ran is the index snapshot: the data directory is deliberately seeded WITHOUT
//! one, so a snapshot existing at the next open can only have been written by
//! the close path.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::EmbeddedDatabase;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// How long to wait for the server to accept connections after spawn.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
/// How long to wait for the process to exit after being signalled.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
/// Settle time after the port opens, so the probe connection's task finishes
/// and releases its `Arc<EmbeddedDatabase>` clone before we signal.
const SETTLE: Duration = Duration::from_secs(1);
/// Rows seeded before the server starts.
const SEEDED_ROWS: i32 = 10;

/// Reserve a free localhost port by binding and immediately releasing it.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn count_rows(db: &EmbeddedDatabase, sql: &str) -> i64 {
    let rows = db.query(sql, &[]).unwrap();
    match rows[0].values.first() {
        Some(heliosdb_nano::Value::Int8(n)) => *n,
        Some(heliosdb_nano::Value::Int4(n)) => i64::from(*n),
        other => panic!("expected integer count, got {other:?}"),
    }
}

/// Seed the data directory and leave it in the state an unclean stop leaves:
/// no index snapshot, unflushed row counter. Whatever the server writes on its
/// way out is then unambiguously attributable to its own close path.
fn seed_without_snapshot(dir: &Path) {
    let db = EmbeddedDatabase::new(dir).unwrap();
    db.execute("CREATE TABLE sig_t (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("CREATE INDEX idx_sig_t_v ON sig_t(v)").unwrap();
    for i in 0..SEEDED_ROWS {
        let dml = format!("INSERT INTO sig_t VALUES ({i}, {i})");
        db.execute(&dml).unwrap();
    }
    db.storage.set_index_snapshots_on_close(false);
    db.storage.set_row_counter_flush_on_close(false);
}

fn wait_until_listening(child: &mut Child, port: u16) {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            panic!("server exited during startup with {status}");
        }
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            thread::sleep(SETTLE);
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("server never listened on port {port}");
}

/// Send `sig` (e.g. `TERM`) the same way the `stop` subcommand does, then wait
/// for the process to exit on its own.
fn signal_and_wait(child: &mut Child, sig: &str) {
    let pid = child.id().to_string();
    let sent = Command::new("kill")
        .args([&format!("-{sig}"), &pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("kill must be available");
    assert!(sent.success(), "failed to send SIG{sig} to the server");

    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("server ignored SIG{sig} and had to be killed");
}

/// Spawn the server on `dir`, wait for it to serve, signal it, wait for exit.
fn run_server_then_signal(dir: &Path, sig: &str) {
    let port = free_port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"))
        .arg("start")
        .arg("--data-dir")
        .arg(dir)
        .arg("--port")
        .arg(port.to_string())
        // Disable the HTTP listener: it runs as a detached task holding its own
        // `Arc<EmbeddedDatabase>` clone, which would keep the database alive
        // past the shutdown path and defeat the very Drop under test.
        .args(["--http-port", "0"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the heliosdb-nano binary must be runnable");

    wait_until_listening(&mut child, port);
    signal_and_wait(&mut child, sig);
}

/// Reopen `dir` and return whether the close path left a usable index snapshot,
/// asserting along the way that no seeded row was lost to a reused row id.
fn inspect_after_shutdown(dir: &Path, sig: &str) -> u64 {
    let db = EmbeddedDatabase::new(dir).unwrap();
    let report = db.storage.last_index_open_report().unwrap();

    // A correct row counter is the other half of the close path. Prove it the
    // only way that is observable: the next INSERT must land on a fresh row id
    // instead of overwriting a live row in place.
    db.execute("INSERT INTO sig_t VALUES (999, 999)").unwrap();
    let total = count_rows(&db, "SELECT COUNT(*) FROM sig_t");
    assert_eq!(total, 11, "SIG{sig}: new row overwrote an existing one");
    for id in 0..SEEDED_ROWS {
        let sql = format!("SELECT COUNT(*) FROM sig_t WHERE id = {id}");
        let hits = count_rows(&db, &sql);
        assert_eq!(hits, 1, "SIG{sig}: row id={id} was overwritten");
    }

    report.tables_from_snapshot
}

/// The headline test: SIGTERM must reach the shutdown path at all, and land the
/// data directory in exactly the state SIGINT does.
#[test]
fn sigterm_shutdown_matches_sigint_shutdown() {
    let term_dir = TempDir::new().unwrap();
    seed_without_snapshot(term_dir.path());
    run_server_then_signal(term_dir.path(), "TERM");
    let term_snapshots = inspect_after_shutdown(term_dir.path(), "TERM");

    let int_dir = TempDir::new().unwrap();
    seed_without_snapshot(int_dir.path());
    run_server_then_signal(int_dir.path(), "INT");
    let int_snapshots = inspect_after_shutdown(int_dir.path(), "INT");

    // The data dirs were seeded WITHOUT a snapshot, so a snapshot at reopen can
    // only have come from the close path — i.e. `Drop` actually ran. `>= 1`
    // rather than `== 1` so an internal table gaining an index later does not
    // make this a false failure.
    assert!(int_snapshots >= 1, "SIGINT must checkpoint at shutdown");
    assert!(term_snapshots >= 1, "SIGTERM must checkpoint too");
    assert_eq!(term_snapshots, int_snapshots, "signals must agree");
}
