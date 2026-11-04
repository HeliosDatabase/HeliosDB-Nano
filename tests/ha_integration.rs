//! High Availability integration tests: a primary and its PHYSICAL standby
//! (spec P2.1, `docs/guides/physical-standby.md`).
//!
//! Real managed daemons, real wire (`tests/support/physical_pair.rs`), the
//! standby following the primary's one replication port. The scenarios the
//! native v3 suite covered, on the pair that replaced it:
//!
//! * start order — a standby started before its primary waits for it (up,
//!   not ready) and bootstraps once it appears;
//! * restart of both sides — SIGKILL and restart of the primary, stop and
//!   restart of the standby, then both at once: the pair converges on every
//!   committed row, the standby resuming from its durable cursor;
//! * sync and semi-sync waits — a commit that returns is already on the
//!   standby, readable there at once;
//! * lag reporting — a standby that falls behind shows on the primary's
//!   `/health`, and catches up;
//! * (sprinter ffe805264aac) a standby's HTTP endpoint answers before its main
//!   loop runs: a SIGTERM in between is neither lost nor fatal (`internal-tests`,
//!   red when the gate and the main loop each had a listener of their own), and
//!   an HTTP port that is also its cascading port costs only the HTTP endpoint.
//!
//! ```bash
//! cargo test --test ha_integration
//! ```

#![cfg(all(feature = "ha-tier1", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/physical_pair.rs"]
mod physical_pair;

use physical_pair::*;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_standby_started_before_its_primary_waits_and_then_bootstraps() {
    let p = pair();
    let standby_data = p.dir.path().join("standby");
    let mut standby = Daemon::spawn(p.dir.path(), "standby", &standby_data, &p.token, &p.standby_args, None);
    // Its listeners open after `replication.startup_timeout_secs` (30 s here)
    // at the latest, not ready: there is nobody to copy from yet.
    let waiting = wait_http(p.http_s, "/ready", 503, Duration::from_secs(60), &standby).await;
    assert_ne!(
        waiting["state"], "needs_recovery",
        "an absent primary is not a refusal: {waiting}"
    );
    assert!(standby.alive());

    let _primary = Daemon::spawn(
        p.dir.path(),
        "primary",
        &p.dir.path().join("primary"),
        &p.token,
        &p.primary_args,
        None,
    );
    let writer = connect(p.pg_p).await;
    writer
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY)")
        .await
        .unwrap();
    writer.batch_execute("INSERT INTO t VALUES (1), (2)").await.unwrap();
    wait_http(p.http_s, "/ready", 200, Duration::from_secs(90), &standby).await;
    assert_eq!(settle(p.pg_s, &[1, 2], Duration::from_secs(30)).await, Ok(vec![1, 2]));
    assert!(standby.log().contains("Bootstrapping physical standby"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_sides_restart_and_the_pair_converges() {
    let p = pair();
    let primary_data = p.dir.path().join("primary");
    let standby_data = p.dir.path().join("standby");
    let mut primary = Daemon::spawn(
        p.dir.path(),
        "primary-1",
        &primary_data,
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
        "standby-1",
        &standby_data,
        &p.token,
        &p.standby_args,
        None,
    );
    wait_http(p.http_s, "/ready", 200, Duration::from_secs(60), &standby).await;
    assert_eq!(settle(p.pg_s, &[1], Duration::from_secs(30)).await, Ok(vec![1]));

    // The primary dies without a shutdown, and comes back.
    primary.kill();
    drop(writer);
    let mut primary = Daemon::spawn(
        p.dir.path(),
        "primary-2",
        &primary_data,
        &p.token,
        &p.primary_args,
        None,
    );
    let writer = connect(p.pg_p).await;
    writer.batch_execute("INSERT INTO t VALUES (2)").await.unwrap();
    assert_eq!(settle(p.pg_s, &[1, 2], Duration::from_secs(60)).await, Ok(vec![1, 2]));

    // The standby stops; the primary writes on; the standby resumes.
    standby.stop();
    writer.batch_execute("INSERT INTO t VALUES (3)").await.unwrap();
    let mut standby = Daemon::spawn(
        p.dir.path(),
        "standby-2",
        &standby_data,
        &p.token,
        &p.standby_args,
        None,
    );
    assert_eq!(
        settle(p.pg_s, &[1, 2, 3], Duration::from_secs(60)).await,
        Ok(vec![1, 2, 3])
    );
    let log = standby.log();
    assert!(log.contains("Resuming physical standby"), "{log}");
    assert!(!log.contains("Bootstrapping physical standby"), "no second copy: {log}");

    // Both at once; the standby first in line this time.
    drop(writer);
    standby.stop();
    primary.stop();
    let standby = Daemon::spawn(
        p.dir.path(),
        "standby-3",
        &standby_data,
        &p.token,
        &p.standby_args,
        None,
    );
    let _primary = Daemon::spawn(
        p.dir.path(),
        "primary-3",
        &primary_data,
        &p.token,
        &p.primary_args,
        None,
    );
    let writer = connect(p.pg_p).await;
    writer.batch_execute("INSERT INTO t VALUES (4)").await.unwrap();
    wait_http(p.http_s, "/ready", 200, Duration::from_secs(90), &standby).await;
    assert_eq!(
        settle(p.pg_s, &[1, 2, 3, 4], Duration::from_secs(60)).await,
        Ok(vec![1, 2, 3, 4])
    );
}

/// A sync or semi-sync commit that has returned is on the standby already —
/// applied, so a reader there sees it with no wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_and_semi_sync_commits_are_on_the_standby_when_they_return() {
    for mode in ["semi-sync", "sync"] {
        let p = pair();
        let config = format!("[replication]\nsync_mode = \"{mode}\"\nsync_timeout_ms = 60000\n");
        let primary = Daemon::spawn(
            p.dir.path(),
            "primary",
            &p.dir.path().join("primary"),
            &p.token,
            &p.primary_args,
            Some(&config),
        );
        let standby = Daemon::spawn(
            p.dir.path(),
            "standby",
            &p.dir.path().join("standby"),
            &p.token,
            &p.standby_args,
            None,
        );
        wait_http(p.http_s, "/ready", 200, Duration::from_secs(90), &standby).await;
        let writer = connect(p.pg_p).await;
        let at = primary_sequence(&writer).await;
        wait_listed(p.http_p, at, Duration::from_secs(60), &primary).await;
        writer
            .batch_execute("CREATE TABLE t (id INT PRIMARY KEY)")
            .await
            .unwrap();
        let reader = connect(p.pg_s).await;
        for id in 1..=5 {
            writer
                .batch_execute(&format!("INSERT INTO t VALUES ({id})"))
                .await
                .unwrap();
            let expected: Vec<i32> = (1..=id).collect();
            assert_eq!(
                ids(&reader).await,
                Ok(expected),
                "{mode}: a returned commit is readable on the standby at once"
            );
        }
        let (_, health) = http(p.http_p, "/health").await.unwrap();
        assert_eq!(health["replication"]["sync_mode"], mode, "{health}");
        assert_eq!(health["standbys"].as_array().unwrap().len(), 1, "{health}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lag_shows_on_the_primary_and_the_standby_catches_up() {
    let p = pair();
    let primary = Daemon::spawn(
        p.dir.path(),
        "primary",
        &p.dir.path().join("primary"),
        &p.token,
        &p.primary_args,
        None,
    );
    let writer = connect(p.pg_p).await;
    writer
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY)")
        .await
        .unwrap();
    let standby = Daemon::spawn(
        p.dir.path(),
        "standby",
        &p.dir.path().join("standby"),
        &p.token,
        &p.standby_args,
        None,
    );
    wait_http(p.http_s, "/ready", 200, Duration::from_secs(60), &standby).await;
    let at = primary_sequence(&writer).await;
    wait_listed(p.http_p, at, Duration::from_secs(60), &primary).await;

    // An async primary writes on while its standby cannot apply anything.
    standby.pause();
    for id in 1..=20 {
        writer
            .batch_execute(&format!("INSERT INTO t VALUES ({id})"))
            .await
            .unwrap();
    }
    let behind = listed_standbys(p.http_p).await;
    assert_eq!(behind.len(), 1, "{behind:?}");
    assert!(
        behind[0]["lag_bytes"].as_u64().unwrap() >= 20,
        "20 writes behind at least: {behind:?}"
    );
    standby.resume();
    let target = primary_sequence(&writer).await;
    let caught_up = wait_listed(p.http_p, target, Duration::from_secs(60), &primary).await;
    assert!(caught_up["applied_sequence"].as_u64().unwrap() >= target, "{caught_up}");
    let all: Vec<i32> = (1..=20).collect();
    assert_eq!(settle(p.pg_s, &all, Duration::from_secs(30)).await, Ok(all));
    let (_, health) = http(p.http_s, "/health").await.unwrap();
    let block = &health["replication"];
    assert_eq!(block["role"], "standby", "{health}");
    assert!(
        lsn(block["cursor"].as_str().unwrap()) >= target,
        "the standby reports the position it caught up to: {health}"
    );
}

/// `internal-tests`: past its readiness gate and before its main loop, a
/// standby's start path holds while the file this names exists (`main.rs`).
#[cfg(feature = "internal-tests")]
const HOLD_AFTER_GATE_ENV: &str = "HELIOSDB_NANO_TEST_HOLD_AFTER_GATE_FILE";
/// The log line that hook writes once it holds.
#[cfg(feature = "internal-tests")]
const HOLDING_AFTER_GATE: &str = "holds after its readiness gate";

/// A standby of `data` whose start path will hold past its gate while `hold`
/// exists.
#[cfg(feature = "internal-tests")]
fn spawn_held(p: &Pair, data: &std::path::Path, hold: &std::path::Path) -> Daemon {
    std::fs::write(hold, b"").unwrap();
    let options = SpawnOptions {
        listen: None,
        env: vec![(HOLD_AFTER_GATE_ENV, hold.display().to_string())],
    };
    Daemon::spawn_with(p.dir.path(), "standby", data, &p.token, &p.standby_args, None, &options)
}

/// SIGTERM while `standby` holds past its gate: it stays up, and once its main
/// loop runs it shuts down cleanly.
#[cfg(feature = "internal-tests")]
async fn sigterm_while_held_is_a_clean_shutdown(standby: &mut Daemon, hold: &std::path::Path) {
    standby.terminate();
    // Long enough for the default action, had no handler been installed.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(standby.alive(), "killed by the default action: {}", standby.tail(12));
    std::fs::remove_file(hold).unwrap();
    let status = standby.wait_exit(Duration::from_secs(60));
    assert!(
        status.is_some_and(|status| status.success()),
        "the signal was lost, or the shutdown failed: {status:?}; {}",
        standby.tail(20)
    );
    assert!(standby.log().contains("SIGTERM received"), "{}", standby.tail(20));
}

/// `/ready` says 200 before the main loop runs, and a supervisor may act on it
/// at once.
#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sigterm_between_the_readiness_gate_and_the_main_loop_is_honoured() {
    let p = pair();
    let dir = p.dir.path();
    let _primary = Daemon::spawn(dir, "primary", &dir.join("primary"), &p.token, &p.primary_args, None);
    let writer = connect(p.pg_p).await;
    create_table(&writer).await;
    let hold = dir.join("hold-after-gate");
    let mut standby = spawn_held(&p, &dir.join("standby"), &hold);
    assert!(
        standby.log_contains_within(HOLDING_AFTER_GATE, Duration::from_secs(90)),
        "{}",
        standby.tail(12)
    );
    wait_http(p.http_s, "/ready", 200, Duration::from_secs(60), &standby).await;
    sigterm_while_held_is_a_clean_shutdown(&mut standby, &hold).await;
}

/// A directory refused at start: its gate returns without ever polling for a
/// signal, while `/ready` already answers.
#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sigterm_right_after_a_refused_directory_is_not_fatal() {
    let p = pair();
    let dir = p.dir.path();
    let legacy = dir.join("legacy");
    std::fs::create_dir(&legacy).unwrap();
    std::fs::write(legacy.join("NANO-STANDBY-BINDING.json"), b"{}").unwrap();
    std::fs::write(legacy.join("CURRENT"), b"MANIFEST-000001\n").unwrap();
    let hold = dir.join("hold-after-gate");
    let mut standby = spawn_held(&p, &legacy, &hold);
    assert!(
        standby.log_contains_within(HOLDING_AFTER_GATE, Duration::from_secs(30)),
        "{}",
        standby.tail(12)
    );
    let refused = wait_http(p.http_s, "/ready", 503, Duration::from_secs(30), &standby).await;
    assert_eq!(refused["state"], "needs_recovery", "{refused}");
    sigterm_while_held_is_a_clean_shutdown(&mut standby, &hold).await;
}

/// A cascading standby whose HTTP port is its replication port: the
/// replication listener keeps the port and the standby stays up without an
/// HTTP endpoint (an HTTP bind failure alone is never fatal).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cascading_standbys_http_port_collision_costs_only_its_http_endpoint() {
    let p = pair();
    let dir = p.dir.path();
    let _primary = Daemon::spawn(dir, "primary", &dir.join("primary"), &p.token, &p.primary_args, None);
    let writer = connect(p.pg_p).await;
    create_table(&writer).await;
    assert_eq!(insert(&writer, 1..=1, "row").await, 0);
    let shared = free_port();
    let mut args = standby_args(p.pg_s, p.repl_p, shared);
    args.extend(["--replication-port".to_string(), shared.to_string()]);
    let cascade = "[replication.physical]\ncascade = true\n";
    let mut standby = Daemon::spawn(dir, "standby", &dir.join("standby"), &p.token, &args, Some(cascade));
    assert_eq!(settle(p.pg_s, &[1], Duration::from_secs(90)).await, Ok(vec![1]));
    assert!(
        standby.log().contains("Failed to bind HTTP endpoint"),
        "{}",
        standby.tail(20)
    );
    assert!(
        standby.log_contains_within("Cascading replication port listening", Duration::from_secs(30)),
        "{}",
        standby.tail(20)
    );
    assert!(standby.alive(), "{}", standby.tail(20));
}
