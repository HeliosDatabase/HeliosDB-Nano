//! Spec P2.1 §1, §4: a primary that will not serve a standby says why — a
//! typed `Refuse { code, reason }` on the replication connection — and the
//! standby turns it into posture `needs_recovery`, staying up (field finding
//! J2) instead of reconnecting forever (the P1.3 residual: a refused session
//! was dropped silently and read as a transport failure).
//!
//! Real managed daemons, real wire (`tests/support/physical_pair.rs`):
//!
//! * another dataset on the primary's endpoints → `HistoryMismatch`;
//! * the WAL the standby needs pruned away → `WalPruned`;
//! * the wrong token → `AuthFailed` (after a few attempts);
//! * an `NPHYS001` client → `Refuse { Version }`, in this protocol's framing.
//!
//! In every case the standby process stays up, `/ready` answers 503 with
//! `needs_recovery`, and `heliosdb_require_ready()` refuses with SQLSTATE
//! 55000 carrying the reason.
//!
//! Red on the base of this change (fa533dbc): the physical source dropped a
//! refused session without a word, so a pruned WAL or a wrong token left the
//! standby `reconnecting` forever; only the native v3 handshake could report
//! a history mismatch; and no `Refuse` frame existed.

#![cfg(all(feature = "ha-tier1", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/physical_pair.rs"]
mod physical_pair;

use physical_pair::*;
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::Path,
    time::Duration,
};

/// The standby is up, not ready, in `needs_recovery`, and says why on the
/// PostgreSQL wire (55000) and on `/ready`.
async fn assert_needs_recovery(pair: &Pair, standby: &mut Daemon, reason: &str) {
    let block = wait_http(pair.http_s, "/ready", 503, Duration::from_secs(60), standby).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut block = block;
    while block["state"] != "needs_recovery" {
        assert!(
            std::time::Instant::now() < deadline,
            "never needs_recovery: {block}; log tail: {}",
            standby.tail(12)
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
        block = http(pair.http_s, "/ready").await.map(|(_, body)| body).unwrap_or(block);
    }
    assert_eq!(block["ready"], false, "{block}");
    assert!(standby.alive(), "a refused standby stays up: {}", standby.tail(12));
    let client = connect(pair.pg_s).await;
    let refused = client
        .simple_query("SELECT heliosdb_require_ready()")
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&refused), "55000", "{refused}");
    assert!(
        message(&refused).starts_with("standby is not ready: needs_recovery") && message(&refused).contains(reason),
        "{refused}"
    );
    assert_eq!(value(&client, "SELECT pg_is_in_recovery()").await.as_deref(), Some("t"));
    assert!(standby.log().contains(reason), "{}", standby.tail(12));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_mismatch_is_refused_and_the_standby_stays_up_needing_recovery() {
    let p = pair();
    let standby_data = p.dir.path().join("standby");
    let mut primary = Daemon::spawn(
        p.dir.path(),
        "primary-a",
        &p.dir.path().join("primary-a"),
        &p.token,
        &p.primary_args,
        None,
    );
    let writer = connect(p.pg_p).await;
    writer
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY)")
        .await
        .unwrap();
    writer.batch_execute("INSERT INTO t VALUES (1)").await.unwrap();
    let mut standby = Daemon::spawn(
        p.dir.path(),
        "standby-a",
        &standby_data,
        &p.token,
        &p.standby_args,
        None,
    );
    wait_http(p.http_s, "/ready", 200, Duration::from_secs(60), &standby).await;
    standby.stop();
    drop(writer);
    primary.stop();

    // Another dataset answers on the same endpoints.
    let other = Daemon::spawn(
        p.dir.path(),
        "primary-b",
        &p.dir.path().join("primary-b"),
        &p.token,
        &p.primary_args,
        None,
    );
    let _ = connect(p.pg_p).await;
    let mut standby = Daemon::spawn(
        p.dir.path(),
        "standby-b",
        &standby_data,
        &p.token,
        &p.standby_args,
        None,
    );
    assert_needs_recovery(&p, &mut standby, "history mismatch").await;
    let refused = client_refusal(&standby);
    assert!(refused.contains("physical replication refused"), "{refused}");
    assert!(
        other.log_contains_within("history mismatch", Duration::from_secs(10)),
        "the primary logs the refusal it sent: {}",
        other.tail(12)
    );
    // The standby's data is untouched and readable again after an operator
    // points it back at its own primary: it was never rewritten.
    let marker = standby_data.join("NANO-PHYSICAL-REPLICA.json");
    assert!(marker.is_file(), "the refused standby keeps its copy");
}

/// The line of the standby log that names the refusal it got.
fn client_refusal(standby: &Daemon) -> String {
    standby
        .log()
        .lines()
        .find(|line| line.contains("physical standby needs recovery"))
        .unwrap_or_default()
        .to_string()
}

/// Drop every WAL file a stopped primary archived: what its retention does
/// to the WAL a standby that is away too long still needs. A clean stop
/// flushes the memtables (`storage.flush_on_close`), so the archived files
/// hold nothing the primary itself still needs. Returns how many went.
fn prune_archived_wal(primary_data: &Path) -> usize {
    let archive = primary_data.join("archive");
    let Ok(entries) = std::fs::read_dir(&archive) else {
        return 0;
    };
    let mut pruned = 0;
    for entry in entries {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|extension| extension == "log") {
            std::fs::remove_file(path).unwrap();
            pruned += 1;
        }
    }
    pruned
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pruned_wal_is_refused_as_wal_pruned() {
    let p = pair();
    // The smallest retention there is; the purge itself is simulated below,
    // because RocksDB purges archived WAL on its own clock (every ten minutes
    // with a size limit set), far longer than a test. Spec P3.2: with its WAL
    // archive on, the primary would serve the gap from it instead
    // (`tests/physical_archive_recovery.rs`); off, the gap is refused.
    let retention = "[replication.physical]\nwal_retention_secs = 60\nwal_retention_size_mb = 64\n\
                     [replication.archive]\nenabled = false\n";
    let primary_data = p.dir.path().join("primary");
    let standby_data = p.dir.path().join("standby");
    let mut primary = Daemon::spawn(
        p.dir.path(),
        "primary-1",
        &primary_data,
        &p.token,
        &p.primary_args,
        Some(retention),
    );
    let writer = connect(p.pg_p).await;
    writer
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY)")
        .await
        .unwrap();
    writer.batch_execute("INSERT INTO t VALUES (1)").await.unwrap();
    let mut standby = Daemon::spawn(
        p.dir.path(),
        "standby-1",
        &standby_data,
        &p.token,
        &p.standby_args,
        None,
    );
    wait_http(p.http_s, "/ready", 200, Duration::from_secs(60), &standby).await;
    assert_eq!(settle(p.pg_s, &[1], Duration::from_secs(30)).await, Ok(vec![1]));
    standby.stop();

    // While the standby is away the primary writes on, then retention drops
    // the WAL that held it.
    for id in 2..=40 {
        writer
            .batch_execute(&format!("INSERT INTO t VALUES ({id})"))
            .await
            .unwrap();
    }
    drop(writer);
    primary.stop();
    let pruned = prune_archived_wal(&primary_data);
    assert!(
        pruned > 0,
        "precondition: the clean stop archived the WAL holding rows 2..=40 ({})",
        primary_data.join("archive").display()
    );

    let _primary = Daemon::spawn(
        p.dir.path(),
        "primary-2",
        &primary_data,
        &p.token,
        &p.primary_args,
        Some(retention),
    );
    let reader = connect(p.pg_p).await;
    assert_eq!(ids(&reader).await.unwrap().len(), 40, "the primary itself lost nothing");
    let mut standby = Daemon::spawn(
        p.dir.path(),
        "standby-2",
        &standby_data,
        &p.token,
        &p.standby_args,
        None,
    );
    assert_needs_recovery(&p, &mut standby, "WAL pruned").await;
    assert!(
        client_refusal(&standby).contains("rebuild the standby"),
        "the refusal says what to do: {}",
        client_refusal(&standby)
    );
    // Spec P3.2 §1: it names both floors.
    let refusal = client_refusal(&standby);
    assert!(
        refusal.contains("RocksDB holds") && refusal.contains("the WAL archive is off"),
        "{refusal}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_token_is_refused_with_auth_failed() {
    let p = pair();
    let primary = Daemon::spawn(
        p.dir.path(),
        "primary",
        &p.dir.path().join("primary"),
        &p.token,
        &p.primary_args,
        None,
    );
    let _ = connect(p.pg_p).await;
    let wrong = token_file(p.dir.path(), "wrong.token", 0x24);
    let standby_data = p.dir.path().join("standby");
    let mut standby = Daemon::spawn(p.dir.path(), "standby", &standby_data, &wrong, &p.standby_args, None);
    assert_needs_recovery(&p, &mut standby, "authentication failed").await;
    assert!(!standby_data.exists(), "nothing was installed for a refused standby");
    assert!(
        primary.log_contains_within("authentication failed", Duration::from_secs(10)),
        "{}",
        primary.tail(12)
    );
}

/// Read one frame header and payload from a raw replication socket.
fn read_raw_frame(stream: &mut TcpStream) -> ([u8; 16], Vec<u8>) {
    let mut header = [0u8; 16];
    stream.read_exact(&mut header).unwrap();
    let length = u32::from_be_bytes(header[12..16].try_into().unwrap()) as usize;
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).unwrap();
    (header, payload)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_nphys001_client_is_refused_with_version() {
    let p = pair();
    let primary = Daemon::spawn(
        p.dir.path(),
        "primary",
        &p.dir.path().join("primary"),
        &p.token,
        &p.primary_args,
        None,
    );
    let _ = connect(p.pg_p).await;
    let token = std::fs::read(&p.token).unwrap();
    let repl_p = p.repl_p;
    let (header, payload) = tokio::task::spawn_blocking(move || {
        let mut stream =
            TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], repl_p)), Duration::from_secs(5)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        // NPHYS001's Auth frame: magic, version 1, kind 1, length 32, the token.
        let mut auth = b"NPHYS001".to_vec();
        auth.extend(1_u16.to_be_bytes());
        auth.extend(1_u16.to_be_bytes());
        auth.extend(32_u32.to_be_bytes());
        auth.extend(&token);
        stream.write_all(&auth).unwrap();
        read_raw_frame(&mut stream)
    })
    .await
    .unwrap();
    assert_eq!(&header[..8], b"NPHYS002", "answered in this protocol's framing");
    assert_eq!(u16::from_be_bytes([header[8], header[9]]), 2);
    assert_eq!(u16::from_be_bytes([header[10], header[11]]), 17, "a Refuse frame");
    assert_eq!(u16::from_be_bytes([payload[0], payload[1]]), 8, "code Version");
    let reason = String::from_utf8_lossy(&payload[4..]);
    assert!(reason.contains("NPHYS001") && reason.contains("NPHYS002"), "{reason}");
    assert!(
        primary.log_contains_within("protocol version", Duration::from_secs(10)),
        "{}",
        primary.tail(12)
    );
}
