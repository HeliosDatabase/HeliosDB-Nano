//! Spec P2.1 §2, §4: synchronous replication on the PHYSICAL sequence, over
//! the one replication connection (`docs/plans/specs/P2_1_single_connection_control_plane.md`).
//!
//! Real managed daemons, real wire (`tests/support/physical_pair.rs`):
//!
//! * `sync`: a commit on the primary waits until its standby has APPLIED the
//!   commit's physical sequence. With the standby stopped (SIGSTOP — its
//!   socket stays open, it just never acknowledges) an INSERT blocks, and it
//!   returns once the standby runs again (SIGCONT);
//! * `semi-sync` with `min_acks = 2` and one standby: under
//!   `sync_timeout_action = "warn"` the commit returns after the timeout with
//!   a WARN; under `"fail"` the client gets the "synchronous replication not
//!   confirmed" error — and the row is durable on the primary all the same;
//! * 15 s of heartbeat-only traffic never moves the primary's recorded
//!   sequences back, and the primary's `/health` lists the standby with
//!   applied/durable positions that advance with the primary.
//!
//! Red on the base of this change (fa533dbc): commits waited on the native v3
//! channel's LOGICAL acknowledgement, sent before apply, so a sync INSERT
//! returned at once with the standby stopped; `min_acks` and
//! `sync_timeout_action` did not exist (the timeout only warned); the native
//! heartbeat reset the recorded position to 0 every few seconds; and a
//! primary's `/health` listed no standby.

#![cfg(all(feature = "ha-tier1", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/physical_pair.rs"]
mod physical_pair;

use physical_pair::*;
use std::time::{Duration, Instant};

const NOT_CONFIRMED: &str = "synchronous replication not confirmed";

/// Start the pair, standby first in line: a sync primary waits for its
/// standby from its first commit, so the standby must stream before the test
/// writes anything.
async fn start(pair: &Pair, primary_config: &str) -> (Daemon, Daemon) {
    let primary = Daemon::spawn(
        pair.dir.path(),
        "primary",
        &pair.dir.path().join("primary"),
        &pair.token,
        &pair.primary_args,
        Some(primary_config),
    );
    let standby = Daemon::spawn(
        pair.dir.path(),
        "standby",
        &pair.dir.path().join("standby"),
        &pair.token,
        &pair.standby_args,
        None,
    );
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(90), &standby).await;
    (primary, standby)
}

/// The primary lists its standby as a streaming one that counts toward
/// `min_acks`, durable at least at the primary's current sequence.
async fn wait_following(pair: &Pair, primary: &Daemon, writer: &tokio_postgres::Client) -> serde_json::Value {
    let at = primary_sequence(writer).await;
    let listed = wait_listed(pair.http_p, at, Duration::from_secs(60), primary).await;
    assert_eq!(listed["state"], "streaming", "{listed}");
    assert_eq!(listed["counts_for_sync"], true, "{listed}");
    assert_eq!(listed["pg_port"], pair.pg_s, "{listed}");
    listed
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_commit_blocks_while_the_standby_is_paused_and_returns_when_it_resumes() {
    let p = pair();
    let (primary, standby) = start(&p, "[replication]\nsync_mode = \"sync\"\nsync_timeout_ms = 60000\n").await;
    let writer = connect(p.pg_p).await;
    wait_following(&p, &primary, &writer).await;
    let started = Instant::now();
    writer
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY)")
        .await
        .unwrap();
    writer.batch_execute("INSERT INTO t VALUES (1)").await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "a running standby acknowledges promptly: {:?}",
        started.elapsed()
    );
    assert_eq!(settle(p.pg_s, &[1], Duration::from_secs(30)).await, Ok(vec![1]));

    // The standby stops: its session stays open, but nothing is acknowledged.
    standby.pause();
    let blocked = connect(p.pg_p).await;
    let insert = tokio::spawn(async move {
        let started = Instant::now();
        let outcome = blocked.batch_execute("INSERT INTO t VALUES (2)").await;
        (outcome.map_err(|e| e.to_string()), started.elapsed())
    });
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(
        !insert.is_finished(),
        "a sync INSERT returned while its standby could not have applied it; primary log: {}",
        primary.tail(12)
    );
    // The primary has the row already: the commit is local and durable, only
    // its acknowledgement is outstanding.
    assert_eq!(ids(&writer).await, Ok(vec![1, 2]));

    standby.resume();
    let (outcome, elapsed) = tokio::time::timeout(Duration::from_secs(40), insert)
        .await
        .expect("the INSERT returns once the standby runs again")
        .unwrap();
    assert_eq!(outcome, Ok(()), "primary log: {}", primary.tail(12));
    assert!(elapsed >= Duration::from_secs(4), "{elapsed:?}");
    assert_eq!(settle(p.pg_s, &[1, 2], Duration::from_secs(30)).await, Ok(vec![1, 2]));
    assert!(
        !primary.log().contains(NOT_CONFIRMED),
        "nothing timed out: {}",
        primary.tail(12)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn semi_sync_with_min_acks_two_and_one_standby_warns_or_fails_but_the_commit_stands() {
    let p = pair();
    let warn = "[replication]\nsync_mode = \"semi-sync\"\nmin_acks = 2\nsync_timeout_ms = 1500\n\
                sync_timeout_action = \"warn\"\n";
    let (mut primary, _standby) = start(&p, warn).await;
    let writer = connect(p.pg_p).await;
    wait_following(&p, &primary, &writer).await;

    // `warn`: one standby can never make two acknowledgements. A commit
    // returns after the timeout, with a WARN, and the primary then commits
    // asynchronously instead of paying the timeout on every commit — so the
    // commit that pays it may be the primary's own first write (its HTTP
    // auth schema), not this client's; the WARN is what proves the wait.
    writer
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY)")
        .await
        .unwrap();
    writer.batch_execute("INSERT INTO t VALUES (1)").await.unwrap();
    assert_eq!(ids(&writer).await, Ok(vec![1]), "warn commits");
    let log = primary.log();
    assert!(
        log.contains(NOT_CONFIRMED)
            && log.contains("of 2 required standbys acknowledged")
            && log.contains("sync_timeout_action = \"warn\""),
        "the shortfall is logged: {}",
        primary.tail(12)
    );
    assert_eq!(settle(p.pg_s, &[1], Duration::from_secs(30)).await, Ok(vec![1]));
    drop(writer);
    primary.stop();

    // `fail`: the client is told — and the commit is durable on the primary,
    // like PostgreSQL's cancelled synchronous commit.
    let fail = "[replication]\nsync_mode = \"semi-sync\"\nmin_acks = 2\nsync_timeout_ms = 1500\n\
                sync_timeout_action = \"fail\"\n";
    let primary = Daemon::spawn(
        p.dir.path(),
        "primary-fail",
        &p.dir.path().join("primary"),
        &p.token,
        &p.primary_args,
        Some(fail),
    );
    let writer = connect(p.pg_p).await;
    wait_following(&p, &primary, &writer).await;
    let refused = writer.batch_execute("INSERT INTO t VALUES (2)").await.unwrap_err();
    assert!(
        message(&refused).contains(NOT_CONFIRMED) && message(&refused).contains("durable on this primary"),
        "{refused}"
    );
    let reader = connect(p.pg_p).await;
    assert_eq!(ids(&reader).await, Ok(vec![1, 2]), "the commit stands on the primary");
    assert_eq!(settle(p.pg_s, &[1, 2], Duration::from_secs(30)).await, Ok(vec![1, 2]));
    // `fail` never degrades: the next short commit fails too.
    assert!(reader.batch_execute("INSERT INTO t VALUES (3)").await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heartbeats_never_regress_the_recorded_sequences_and_health_lists_the_standby() {
    let p = pair();
    let (primary, _standby) = start(&p, "[replication]\nsync_mode = \"async\"\n").await;
    let writer = connect(p.pg_p).await;
    writer
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY)")
        .await
        .unwrap();
    writer.batch_execute("INSERT INTO t VALUES (1), (2)").await.unwrap();
    let first = wait_following(&p, &primary, &writer).await;
    let node = first["node_id"].as_str().unwrap().to_string();
    for field in ["applied", "durable"] {
        lsn(first[field].as_str().unwrap());
    }
    assert!(first["lag_bytes"].is_u64() && first["last_seen"].is_i64(), "{first}");

    // 15 s in which the standby only heartbeats (an idle primary) — save one
    // row in the middle, so the positions must also ADVANCE.
    let mut seen = (
        first["applied_sequence"].as_u64().unwrap(),
        first["durable_sequence"].as_u64().unwrap(),
        first["last_seen"].as_i64().unwrap(),
    );
    let initial = seen;
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut wrote = false;
    while Instant::now() < deadline {
        if !wrote && deadline - Instant::now() < Duration::from_millis(7_500) {
            writer.batch_execute("INSERT INTO t VALUES (3)").await.unwrap();
            wrote = true;
        }
        let listed = listed_standbys(p.http_p).await;
        let standby = listed
            .iter()
            .find(|standby| standby["node_id"] == node.as_str())
            .unwrap_or_else(|| panic!("the standby left the registry: {listed:?}"));
        let now = (
            standby["applied_sequence"].as_u64().unwrap(),
            standby["durable_sequence"].as_u64().unwrap(),
            standby["last_seen"].as_i64().unwrap(),
        );
        assert!(
            now.0 >= seen.0 && now.1 >= seen.1 && now.2 >= seen.2,
            "a recorded position moved back: {seen:?} -> {now:?}"
        );
        assert!(now.1 <= now.0, "durable above applied: {now:?}");
        seen = now;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let target = primary_sequence(&writer).await;
    let last = wait_listed(p.http_p, target, Duration::from_secs(30), &primary).await;
    assert!(
        last["applied_sequence"].as_u64().unwrap() > initial.0,
        "the standby's position advanced with the primary: {initial:?} -> {last}"
    );
    assert!(
        last["last_seen"].as_i64().unwrap() > initial.2,
        "heartbeats refresh last_seen: {initial:?} -> {last}"
    );
    assert_eq!(
        settle(p.pg_s, &[1, 2, 3], Duration::from_secs(30)).await,
        Ok(vec![1, 2, 3])
    );
}
