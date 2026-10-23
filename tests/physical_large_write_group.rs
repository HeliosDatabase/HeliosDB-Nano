//! Spec P4.1c §3: write groups of any size replicate, on real managed daemons
//! (`tests/support/physical_pair.rs`).
//!
//! * one `INSERT INTO t SELECT i, repeat('x', 900) || i FROM
//!   generate_series(1, 20000) i` — ONE write group of ~80 MiB, ten times a
//!   serving standby's 8 MiB poll limit — reaches a standby (A) whole: it
//!   settles at the primary's sequence with the primary's default column
//!   family, and its `/health` never reports the old refusal ("exceeds poll
//!   byte limit"); the small writes after it follow;
//! * the primary's archive seals the group into a segment of its own (one
//!   write group, larger than `segment_max_bytes`), contiguous with the rest,
//!   and it deep-verifies;
//! * a standby (B) stopped before the group is advanced over it from copied
//!   segments by `recover --from-archive`, to the primary's exact default
//!   column family;
//! * a standby (C) cascaded from A follows it too, to the same state;
//! * (`internal-tests`: the kill hook) a standby killed (SIGKILL) with the
//!   group's `Batch` frame half received has applied nothing of it, resumes
//!   from its cursor and settles at the primary's state.
//!
//! The 100 000-row variant (~400 MiB) is left to the live probe: it costs
//! more memory and time than the default tier affords.
//!
//! Red on the base of this change (P4.1b): the standby stopped at the group,
//! `reconnecting` forever ("single source batch exceeds poll byte limit"),
//! and the archiver failed at it on every poll, sealing nothing past it.

#![cfg(all(feature = "ha-tier1", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/physical_pair.rs"]
mod physical_pair;

use heliosdb_nano::replication::archive_segment::SegmentInfo;
use heliosdb_nano::replication::physical::{decode_cursor, range_digest, CURSOR_KEY, LOCAL_CF};
use physical_pair::*;
use rocksdb::{Options, DB};
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// The statement of the defect: one write group of 20 000 rows of ~900 bytes.
const BIG_INSERT: &str = "INSERT INTO t SELECT i, repeat('x', 900) || i FROM generate_series(1, 20000) i";
/// A serving standby's poll limit (`STANDBY_POLL_BYTES` in `main.rs`).
const STANDBY_POLL_BYTES: u64 = 8 * 1024 * 1024;
/// Small segments, sealed every second: the group is far larger than one.
const ARCHIVE: &str = "[replication.archive]\nsegment_max_bytes = \"4MiB\"\nsegment_max_secs = 1\n";
/// What a standby may take to apply the group.
const APPLY_BUDGET: Duration = Duration::from_secs(240);

/// A stopped standby's durable cursor (its source sequence), read offline.
fn standby_cursor(path: &Path) -> u64 {
    let db = DB::open_cf_for_read_only(&Options::default(), path, ["default", LOCAL_CF], false).unwrap();
    let bytes = db.get_cf(db.cf_handle(LOCAL_CF).unwrap(), CURSOR_KEY).unwrap().unwrap();
    decode_cursor(&bytes).unwrap().source_sequence()
}

/// The default column family's digest of the store at `path` (read-only,
/// even under a running node), with the store's sequence.
fn default_digest(path: &Path) -> (u64, [u8; 32]) {
    let db = DB::open_for_read_only(&Options::default(), path, false).unwrap();
    (db.latest_sequence_number(), range_digest(&db, None, b"").unwrap())
}

fn recover(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"))
        .arg("recover")
        .args(args)
        .env("RUST_LOG", "warn")
        .output()
        .unwrap()
}

fn text(path: &Path) -> String {
    path.display().to_string()
}

/// Copy `segments` (loose, as an operator would) into `into`.
fn copy_segments(segments: &[SegmentInfo], into: &Path) {
    std::fs::create_dir_all(into).unwrap();
    for segment in segments {
        std::fs::copy(&segment.path, into.join(segment.path.file_name().unwrap())).unwrap();
    }
}

/// Poll a standby's `/health` until its cursor reaches `through`. Fails at
/// once on the defect's refusal, or on `needs_recovery`.
async fn follow(http_port: u16, through: u64, daemon: &Daemon) {
    let deadline = Instant::now() + APPLY_BUDGET;
    loop {
        if let Some((_, health)) = http(http_port, "/health").await {
            let block = &health["replication"];
            let reason = block["reason"].as_str().unwrap_or_default();
            assert!(!reason.contains("exceeds poll byte limit"), "the defect: {block}");
            assert_ne!(block["state"], "needs_recovery", "{block}; {}", daemon.tail(12));
            if block["cursor"].as_str().is_some_and(|cursor| lsn(cursor) >= through) {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "never reached {through}; log tail: {}",
            daemon.tail(12)
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Neither the old refusal nor the archiver's old failure is in `daemon`'s log.
fn assert_never_refused(daemon: &Daemon) {
    let log = daemon.log();
    assert!(!log.contains("exceeds poll byte limit"), "{}", daemon.tail(20));
    assert!(!log.contains("cannot read the store's WAL"), "{}", daemon.tail(20));
}

/// Spec P4.1c §3: one ~80 MiB write group reaches a standby, a cascaded
/// standby, the archive (a segment of its own) and `recover --from-archive`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_large_write_group_reaches_every_standby_the_archive_and_recovery() {
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let primary_data = dir.join("primary");
    let primary = Daemon::spawn(
        &dir,
        "primary",
        &primary_data,
        &pair.token,
        &pair.primary_args,
        Some(ARCHIVE),
    );
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    assert_eq!(insert(&writer, 20_001..=20_010, "before").await, 0);
    let before: Vec<i32> = (20_001..=20_010).collect();

    // A follows the primary and cascades, on a replication port of its own.
    let repl_a = free_port();
    let mut a_args = pair.standby_args.clone();
    a_args.extend(["--replication-port".to_string(), repl_a.to_string()]);
    let cascade = format!("[replication.physical]\ncascade = true\n{ARCHIVE}");
    let a_data = dir.join("standby-a");
    let mut standby_a = Daemon::spawn(&dir, "standby-a", &a_data, &pair.token, &a_args, Some(cascade.as_str()));
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(90), &standby_a).await;
    assert!(
        standby_a.log_contains_within("Cascading replication port listening", Duration::from_secs(30)),
        "{}",
        standby_a.tail(12)
    );

    // B follows the primary this far and stops: the base that
    // `recover --from-archive` advances over the group.
    let (pg_b, http_b) = (free_port(), free_port());
    let b_data = dir.join("standby-b");
    let mut standby_b = Daemon::spawn(
        &dir,
        "standby-b",
        &b_data,
        &pair.token,
        &standby_args(pg_b, pair.repl_p, http_b),
        None,
    );
    wait_http(http_b, "/ready", 200, Duration::from_secs(90), &standby_b).await;
    assert_eq!(settle(pg_b, &before, Duration::from_secs(30)).await, Ok(before.clone()));
    standby_b.stop();

    // C follows A.
    let (pg_c, http_c) = (free_port(), free_port());
    let c_data = dir.join("standby-c");
    let mut standby_c = Daemon::spawn(
        &dir,
        "standby-c",
        &c_data,
        &pair.token,
        &standby_args(pg_c, repl_a, http_c),
        None,
    );
    wait_http(http_c, "/ready", 200, Duration::from_secs(120), &standby_c).await;
    assert_eq!(settle(pg_c, &before, Duration::from_secs(60)).await, Ok(before.clone()));

    // The write group, then small ones after it.
    let start = primary_sequence(&writer).await;
    writer.batch_execute(BIG_INSERT).await.unwrap();
    assert_eq!(insert(&writer, 20_011..=20_020, "after").await, 0);
    let through = primary_sequence(&writer).await;
    let want: Vec<i32> = (1..=20_020).collect();

    follow(pair.http_s, through, &standby_a).await;
    follow(http_c, through, &standby_c).await;
    assert_eq!(
        settle(pair.pg_s, &want, Duration::from_secs(60)).await,
        Ok(want.clone())
    );
    assert_eq!(settle(pg_c, &want, Duration::from_secs(60)).await, Ok(want.clone()));

    // The archive holds the group in a segment of its own.
    let archive = dir.join("primary.archive");
    let segments = wait_sealed_through(&archive, through, &primary).await;
    for adjacent in segments.windows(2) {
        assert_eq!(
            adjacent[0].last_sequence() + 1,
            adjacent[1].first_sequence(),
            "{} then {}",
            adjacent[0].path.display(),
            adjacent[1].path.display()
        );
    }
    let own = segments
        .iter()
        .filter(|segment| segment.first_sequence() > start && segment.last_sequence() <= through)
        .max_by_key(|segment| segment.file_bytes)
        .unwrap_or_else(|| panic!("no segment past {start}: {segments:?}"));
    assert!(
        own.file_bytes > STANDBY_POLL_BYTES,
        "precondition: the statement's write group is larger than the standby's poll limit: {own:?}"
    );
    assert_eq!(
        own.batch_count(),
        1,
        "one write group, in a segment of its own: {own:?}"
    );
    own.verify_deep(None).unwrap();

    // B, advanced over the group from copied segments.
    let copies = dir.join("copies");
    copy_segments(&segments, &copies);
    let through_text = through.to_string();
    let output = recover(&[
        "--data-dir",
        &text(&b_data),
        "--from-archive",
        &text(&copies),
        "--through",
        &through_text,
    ]);
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(standby_cursor(&b_data), through);

    // Every copy is the primary's default column family at `through`.
    standby_c.stop();
    standby_a.stop();
    let (primary_now, primary_digest) = default_digest(&primary_data);
    assert_eq!(primary_now, through, "precondition: the primary wrote nothing since");
    for (name, data) in [("A", &a_data), ("B", &b_data), ("C (cascaded)", &c_data)] {
        assert_eq!(standby_cursor(data), through, "{name}");
        assert_eq!(
            default_digest(data).1,
            primary_digest,
            "{name}: the primary's default column family"
        );
    }
    for daemon in [&primary, &standby_a, &standby_c] {
        assert_never_refused(daemon);
    }
    drop(writer);
}

/// Spec P4.1c §3 (`internal-tests`: the kill hook): a standby killed
/// (SIGKILL) with the group's `Batch` frame half received has applied
/// nothing of it — its default column family is the one it had before — and
/// its restart resumes from its cursor (no bootstrap), receives the group
/// whole and settles at the primary's state.
#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_standby_killed_with_the_group_in_flight_resumes_from_its_cursor() {
    use heliosdb_nano::replication::physical_wire::KILL_MID_BATCH_ENV;
    use std::os::unix::process::ExitStatusExt;

    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let primary_data = dir.join("primary");
    let _primary = Daemon::spawn(
        &dir,
        "primary",
        &primary_data,
        &pair.token,
        &pair.primary_args,
        Some("[replication.archive]\nenabled = false\n"),
    );
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    assert_eq!(insert(&writer, 20_001..=20_010, "before").await, 0);
    let standby_data = dir.join("standby");
    // Killed once a Batch frame of at least the poll limit is half received.
    let hooked = SpawnOptions {
        listen: None,
        env: vec![(KILL_MID_BATCH_ENV, STANDBY_POLL_BYTES.to_string())],
    };
    let mut standby = Daemon::spawn_with(
        &dir,
        "standby",
        &standby_data,
        &pair.token,
        &pair.standby_args,
        None,
        &hooked,
    );
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(90), &standby).await;
    let start = primary_sequence(&writer).await;
    follow(pair.http_s, start, &standby).await;
    let (_, applied_before) = default_digest(&standby_data);

    writer.batch_execute(BIG_INSERT).await.unwrap();
    let through = primary_sequence(&writer).await;
    let status = standby
        .wait_exit(APPLY_BUDGET)
        .unwrap_or_else(|| panic!("the hook never killed the standby: {}", standby.tail(12)));
    assert_eq!(status.signal(), Some(libc::SIGKILL), "{}", standby.tail(12));
    assert!(standby.log().contains(KILL_MID_BATCH_ENV), "{}", standby.tail(12));

    // Nothing of the group was applied.
    assert_eq!(
        standby_cursor(&standby_data),
        start,
        "the durable cursor is where it was"
    );
    assert_eq!(
        default_digest(&standby_data).1,
        applied_before,
        "the default column family holds nothing of the group"
    );

    // The restart resumes from the cursor and settles.
    let mut restarted = Daemon::spawn(&dir, "standby-2", &standby_data, &pair.token, &pair.standby_args, None);
    follow(pair.http_s, through, &restarted).await;
    let want: Vec<i32> = (1..=20_010).collect();
    assert_eq!(settle(pair.pg_s, &want, Duration::from_secs(60)).await, Ok(want));
    let log = restarted.log();
    assert!(
        !log.contains("Bootstrapping physical standby"),
        "{}",
        restarted.tail(20)
    );
    assert!(log.contains("Resuming physical standby"), "{}", restarted.tail(20));
    restarted.stop();
    let (primary_now, primary_digest) = default_digest(&primary_data);
    assert_eq!(primary_now, through, "precondition: the primary wrote nothing since");
    assert_eq!(standby_cursor(&standby_data), through);
    assert_eq!(default_digest(&standby_data).1, primary_digest);
    drop(writer);
}
