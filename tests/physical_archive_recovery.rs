//! Spec P3.2 §5: a physical standby recovered from archived WAL, on real
//! managed daemons (`tests/support/physical_pair.rs`).
//!
//! * gap fill: a standby away while its primary's RocksDB WAL is pruned
//!   under it (the primary keeps running; its archived WAL files are removed
//!   as retention removes them) reconnects and catches up from the primary's
//!   archive on its replication connection — no bootstrap, every row — and
//!   the primary's `/health` lists it served from the WAL again; with the
//!   archive off the same standby is refused `WalPruned`, both floors named;
//! * `heliosdb-nano recover`: a stopped standby advanced from segments an
//!   operator copied to a third directory reaches exactly the primary's
//!   default column family at that sequence; a corrupt segment is named and
//!   nothing is applied; a missing one exits 2 naming the range; a re-run is a
//!   no-op; the standby then resumes from its primary; an encrypted dataset's
//!   sealed segments need the dataset key (`--config`);
//! * an archive-only standby (`--source archive`) follows a directory the
//!   test feeds with the primary's segments: `/ready` 200 (`recovering`, mode
//!   `archive`) while current, 503 `catching_up` past `max_standby_lag`, 503
//!   `needs_recovery` naming a missing segment's range, and back once it
//!   appears;
//! * cascading: primary → standby A (`cascade = true`) → standby B; B holds
//!   the primary's rows and default column family at the same sequence, and
//!   is ready; A says `cascade: true` and lists B.
//!
//! Red on the base of this change (2093d6b1): a standby past the WAL's
//! retention was always refused `WalPruned`, there was no `recover` command,
//! no `--source archive`, and a standby could not be a source.

#![cfg(all(feature = "ha-tier1", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/physical_pair.rs"]
mod physical_pair;

use heliosdb_nano::replication::archive_segment::{SegmentInfo, HEADER_LEN};
use heliosdb_nano::replication::physical::{decode_cursor, range_digest, CURSOR_KEY, LOCAL_CF};
use physical_pair::*;
use rocksdb::{Options, DB};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

// `sealed`, `wait_sealed_through`, `insert`, `create_table`,
// `prune_archived_wal`, `pruning_primary_config` and
// `standby_away_past_retention` live in `support/physical_pair.rs`, shared
// with the fast-forward tests (spec P4.1).

/// Copy `segments` (loose, as an operator would) into `into`.
fn copy_segments<'a>(segments: impl IntoIterator<Item = &'a SegmentInfo>, into: &Path) {
    std::fs::create_dir_all(into).unwrap();
    for segment in segments {
        std::fs::copy(&segment.path, into.join(segment.path.file_name().unwrap())).unwrap();
    }
}

fn flip(path: &Path, offset: usize) {
    let mut bytes = std::fs::read(path).unwrap();
    bytes[offset] ^= 0x5A;
    std::fs::write(path, bytes).unwrap();
}

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

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn text(path: &Path) -> String {
    path.display().to_string()
}

/// Poll `/ready` until its body satisfies `want`; panics with the last answer.
async fn wait_ready_body(
    port: u16,
    budget: Duration,
    daemon: &Daemon,
    want: impl Fn(u16, &serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = Instant::now() + budget;
    loop {
        let seen = http(port, "/ready").await;
        if let Some((code, body)) = &seen {
            if want(*code, body) {
                return body.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "/ready never answered as wanted; last {seen:?}; log tail: {}",
            daemon.tail(12)
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Spec P3.2 §1, §5: the standby reconnects and catches up from the
/// primary's archive — served from it on the replication connection, then
/// from the WAL — with every row and no bootstrap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_standby_away_past_wal_retention_catches_up_from_the_primarys_archive() {
    let pair = pair();
    let dir = pair.dir.path();
    let (primary, writer, last) = standby_away_past_retention(&pair, true).await;
    let standby = Daemon::spawn(
        dir,
        "standby-2",
        &dir.join("standby"),
        &pair.token,
        &pair.standby_args,
        None,
    );
    let want: Vec<i32> = (1..=last).collect();
    assert_eq!(settle(pair.pg_s, &want, Duration::from_secs(120)).await, Ok(want));
    assert!(
        primary.log_contains_within("from the WAL archive", Duration::from_secs(10)),
        "the gap was served from the archive: {}",
        primary.tail(20)
    );
    assert!(
        primary.log_contains_within("RocksDB WAL again", Duration::from_secs(10)),
        "then from the WAL: {}",
        primary.tail(20)
    );
    assert!(
        !standby.log().contains("Bootstrapping physical standby"),
        "{}",
        standby.tail(20)
    );
    assert!(
        standby.log().contains("Resuming physical standby"),
        "{}",
        standby.tail(20)
    );
    // The primary lists it, served from its WAL again, durable at its sequence.
    let through = primary_sequence(&writer).await;
    let listed = wait_listed(pair.http_p, through, Duration::from_secs(60), &primary).await;
    assert_eq!(listed["source"], "wal", "{listed}");
    let (_, health) = http(pair.http_p, "/health").await.unwrap();
    assert!(
        health["replication"].get("source").is_none(),
        "the frozen block is unchanged: {health}"
    );
}

/// Spec P3.2 §1, §5: with the primary's archive off, the same standby is
/// refused `WalPruned`, the refusal naming the missing range and both floors.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_the_archive_off_a_pruned_wal_is_refused_naming_both_floors() {
    let pair = pair();
    let dir = pair.dir.path();
    let (_primary, _writer, _) = standby_away_past_retention(&pair, false).await;
    let mut standby = Daemon::spawn(
        dir,
        "standby-2",
        &dir.join("standby"),
        &pair.token,
        &pair.standby_args,
        None,
    );
    let block = wait_ready_body(pair.http_s, Duration::from_secs(90), &standby, |code, body| {
        code == 503 && body["state"] == "needs_recovery"
    })
    .await;
    let reason = block["reason"].as_str().unwrap_or_default().to_string();
    assert!(reason.contains("WAL pruned"), "{reason}");
    assert!(reason.contains("RocksDB holds"), "{reason}");
    assert!(reason.contains("the WAL archive is off"), "{reason}");
    assert!(reason.contains("recover --from-archive"), "{reason}");
    assert!(standby.alive(), "a refused standby stays up");
}

/// The primary/standby pair of the `recover` tests: the standby followed to
/// row 1 and stopped; the primary then wrote rows 2..=`rows` and sealed them
/// into small segments. Returns the primary, its client, the standby's data
/// directory, its cursor, the primary's sequence and its sealed segments.
struct Recoverable {
    pair: Pair,
    primary: Daemon,
    writer: tokio_postgres::Client,
    standby: PathBuf,
    cursor: u64,
    through: u64,
    segments: Vec<SegmentInfo>,
}

async fn recoverable(config: Option<&str>, rows: i32) -> Recoverable {
    const SMALL: &str = "[replication.archive]\nsegment_max_bytes = \"16KiB\"\nsegment_max_secs = 1\n";
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let config = format!("{}{SMALL}", config.unwrap_or_default());
    let primary = Daemon::spawn(
        &dir,
        "primary",
        &dir.join("primary"),
        &pair.token,
        &pair.primary_args,
        Some(config.as_str()),
    );
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    assert_eq!(insert(&writer, 1..=1, "first").await, 0);
    let standby_data = dir.join("standby");
    let mut standby = Daemon::spawn(
        &dir,
        "standby",
        &standby_data,
        &pair.token,
        &pair.standby_args,
        Some(config.as_str()),
    );
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(90), &standby).await;
    assert_eq!(settle(pair.pg_s, &[1], Duration::from_secs(30)).await, Ok(vec![1]));
    standby.stop();
    let cursor = standby_cursor(&standby_data);
    assert_eq!(insert(&writer, 2..=rows, &"r".repeat(200)).await, 0);
    let through = primary_sequence(&writer).await;
    let segments = wait_sealed_through(&dir.join("primary.archive"), through, &primary).await;
    Recoverable {
        pair,
        primary,
        writer,
        standby: standby_data,
        cursor,
        through,
        segments,
    }
}

/// Spec P3.2 §2, §5: `recover` from segments an operator copied to a third
/// directory: a corrupt one applies nothing, a missing one exits 2 with the
/// range, all of them reach exactly the primary's default column family at
/// that sequence, a re-run is a no-op, and the standby then resumes from its
/// primary without a copy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recover_advances_a_stopped_standby_from_copied_segments_exactly() {
    let fixture = recoverable(None, 200).await;
    let dir = fixture.pair.dir.path().to_path_buf();
    let needed: Vec<&SegmentInfo> = fixture
        .segments
        .iter()
        .filter(|segment| segment.last_sequence() > fixture.cursor && segment.first_sequence() <= fixture.through)
        .collect();
    assert!(needed.len() >= 3, "precondition: {} segments to apply", needed.len());
    let standby = text(&fixture.standby);
    let through = fixture.through.to_string();

    // A corrupt copy anywhere in the chain: named, nothing applied.
    let corrupt = dir.join("copies-corrupt");
    copy_segments(&fixture.segments, &corrupt);
    let damaged = corrupt.join(needed[1].path.file_name().unwrap());
    flip(&damaged, HEADER_LEN + 20);
    let output = recover(&["--data-dir", &standby, "--from-archive", &text(&corrupt)]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    let refusal = stderr(&output);
    assert!(
        refusal.contains(&text(&damaged)) && refusal.contains("nothing was applied"),
        "{refusal}"
    );
    assert_eq!(standby_cursor(&fixture.standby), fixture.cursor, "nothing was applied");

    // A missing segment: applied up to it, then exit 2 naming the range.
    let gapped = dir.join("copies-gapped");
    copy_segments(
        fixture
            .segments
            .iter()
            .filter(|segment| segment.first_sequence() != needed[1].first_sequence()),
        &gapped,
    );
    let output = recover(&[
        "--data-dir",
        &standby,
        "--from-archive",
        &text(&gapped),
        "--through",
        &through,
    ]);
    assert_eq!(output.status.code(), Some(2), "{}{}", stdout(&output), stderr(&output));
    let report = stdout(&output);
    assert!(report.contains("needs recovery: archive gap"), "{report}");
    assert!(
        report.contains(&format!(
            "{}..{}",
            needed[1].first_sequence(),
            needed[1].last_sequence()
        )),
        "{report}"
    );
    assert_eq!(standby_cursor(&fixture.standby), needed[0].last_sequence());

    // Every segment, copied to a third directory: exactly `through`.
    let copies = dir.join("copies");
    copy_segments(&fixture.segments, &copies);
    let copies_text = text(&copies);
    let args = [
        "--data-dir",
        standby.as_str(),
        "--from-archive",
        copies_text.as_str(),
        "--through",
        through.as_str(),
    ];
    let output = recover(&args);
    assert!(output.status.success(), "{}{}", stdout(&output), stderr(&output));
    assert!(
        stdout(&output).contains(&format!("the standby is at sequence {}", fixture.through)),
        "{}",
        stdout(&output)
    );
    assert_eq!(standby_cursor(&fixture.standby), fixture.through);
    let (primary_sequence_now, primary_digest) = default_digest(&dir.join("primary"));
    assert_eq!(
        primary_sequence_now, fixture.through,
        "precondition: the primary wrote nothing since"
    );
    let (_, standby_digest) = default_digest(&fixture.standby);
    assert_eq!(
        standby_digest, primary_digest,
        "the primary's default column family at the same sequence"
    );

    // A re-run is a no-op.
    let output = recover(&args);
    assert!(output.status.success(), "{}{}", stdout(&output), stderr(&output));
    assert!(
        stdout(&output).contains("0 write group(s) applied"),
        "{}",
        stdout(&output)
    );
    assert_eq!(standby_cursor(&fixture.standby), fixture.through);
    let (_, again) = default_digest(&fixture.standby);
    assert_eq!(again, primary_digest);

    // The recovered standby resumes from its primary: no copy, every row.
    let resumed = Daemon::spawn(
        &dir,
        "standby-resumed",
        &fixture.standby,
        &fixture.pair.token,
        &fixture.pair.standby_args,
        None,
    );
    let want: Vec<i32> = (1..=200).collect();
    assert_eq!(
        settle(fixture.pair.pg_s, &want, Duration::from_secs(90)).await,
        Ok(want)
    );
    assert!(
        !resumed.log().contains("Bootstrapping physical standby"),
        "{}",
        resumed.tail(20)
    );
    drop(fixture.writer);
    drop(fixture.primary);
}

/// Spec P3.2 §5: an encrypted dataset's segments are sealed with the dataset
/// key: `recover` refuses them without it (nothing applied) and recovers
/// with it (`--config`, the standby's own configuration).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recover_needs_the_dataset_key_of_an_encrypted_dataset() {
    use std::os::unix::fs::PermissionsExt;
    let key_dir = tempfile::tempdir().unwrap();
    let key = key_dir.path().join("dataset.key");
    std::fs::write(&key, "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let encryption = format!(
        "[encryption]\nenabled = true\nkey_source = {{ File = \"{}\" }}\n",
        key.display()
    );
    let fixture = recoverable(Some(encryption.as_str()), 60).await;
    let dir = fixture.pair.dir.path().to_path_buf();
    assert!(fixture.segments.iter().all(SegmentInfo::sealed_frames));
    let copies = dir.join("copies");
    copy_segments(&fixture.segments, &copies);
    let standby = text(&fixture.standby);
    let through = fixture.through.to_string();

    let keyless = recover(&[
        "--data-dir",
        &standby,
        "--from-archive",
        &text(&copies),
        "--through",
        &through,
    ]);
    assert_eq!(keyless.status.code(), Some(1), "{}", stdout(&keyless));
    assert!(
        stderr(&keyless).contains("dataset key required"),
        "{}",
        stderr(&keyless)
    );
    assert_eq!(standby_cursor(&fixture.standby), fixture.cursor);

    let config = text(&dir.join("standby.toml"));
    let keyed = recover(&[
        "--data-dir",
        &standby,
        "--from-archive",
        &text(&copies),
        "--through",
        &through,
        "--config",
        &config,
    ]);
    assert!(keyed.status.success(), "{}{}", stdout(&keyed), stderr(&keyed));
    assert_eq!(standby_cursor(&fixture.standby), fixture.through);

    let resumed = Daemon::spawn(
        &dir,
        "standby-resumed",
        &fixture.standby,
        &fixture.pair.token,
        &fixture.pair.standby_args,
        Some(encryption.as_str()),
    );
    let want: Vec<i32> = (1..=60).collect();
    assert_eq!(
        settle(fixture.pair.pg_s, &want, Duration::from_secs(90)).await,
        Ok(want)
    );
    assert!(
        !resumed.log().contains("Bootstrapping physical standby"),
        "{}",
        resumed.tail(20)
    );
}

/// Spec P3.2 §3, §5: an archive-only standby follows a directory the test
/// feeds with the primary's segments.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archive_only_standby_follows_a_fed_directory_and_resumes_after_a_gap() {
    const SMALL: &str = "[replication.archive]\nsegment_max_bytes = \"8KiB\"\nsegment_max_secs = 1\n";
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let archive = dir.join("primary.archive");
    let primary = Daemon::spawn(
        &dir,
        "primary",
        &dir.join("primary"),
        &pair.token,
        &pair.primary_args,
        Some(SMALL),
    );
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    assert_eq!(insert(&writer, 1..=1, "first").await, 0);
    let standby_data = dir.join("standby");
    let mut standby = Daemon::spawn(&dir, "standby", &standby_data, &pair.token, &pair.standby_args, None);
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(90), &standby).await;
    assert_eq!(settle(pair.pg_s, &[1], Duration::from_secs(30)).await, Ok(vec![1]));
    standby.stop();

    // The same standby, following a directory of segments only.
    let feed = dir.join("feed");
    std::fs::create_dir_all(&feed).unwrap();
    let args: Vec<String> = vec![
        "--port".into(),
        pair.pg_s.to_string(),
        "--replication-role".into(),
        "standby".into(),
        "--source".into(),
        "archive".into(),
        "--archive-source".into(),
        text(&feed),
        "--http-port".into(),
        pair.http_s.to_string(),
        "--replication-startup-timeout-secs".into(),
        "5".into(),
    ];
    let lag = "[replication]\nmax_standby_lag = \"10s\"\n";
    let mut follower = Daemon::spawn(&dir, "follower", &standby_data, &pair.token, &args, Some(lag));
    // Nothing to apply yet: not ready, and its source is the directory.
    let block = wait_ready_body(pair.http_s, Duration::from_secs(60), &follower, |code, body| {
        code == 503 && body["state"] == "catching_up"
    })
    .await;
    assert_eq!(block["primary"], format!("archive:{}", feed.display()), "{block}");

    // Fed the primary's segments: it applies them and is ready, recovering.
    assert_eq!(insert(&writer, 2..=60, &"a".repeat(200)).await, 0);
    let through = primary_sequence(&writer).await;
    let segments = wait_sealed_through(&archive, through, &primary).await;
    copy_segments(&segments, &feed);
    let mut copied: Vec<String> = segments.iter().map(|segment| text(&segment.path)).collect();
    let block = wait_ready_body(pair.http_s, Duration::from_secs(60), &follower, |code, _| code == 200).await;
    assert_eq!(block["state"], "recovering", "{block}");
    assert!(
        block["progress"]
            .as_str()
            .is_some_and(|progress| progress.contains(" of ")),
        "{block}"
    );
    let want: Vec<i32> = (1..=60).collect();
    assert_eq!(settle(pair.pg_s, &want, Duration::from_secs(60)).await, Ok(want));

    // No newer segment for longer than max_standby_lag: catching up, 503.
    wait_ready_body(pair.http_s, Duration::from_secs(60), &follower, |code, body| {
        code == 503 && body["state"] == "catching_up"
    })
    .await;

    // A segment missing from the directory: needs_recovery naming its range.
    assert_eq!(insert(&writer, 61..=200, &"b".repeat(200)).await, 0);
    let through = primary_sequence(&writer).await;
    let segments = wait_sealed_through(&archive, through, &primary).await;
    let newer: Vec<&SegmentInfo> = segments
        .iter()
        .filter(|segment| !copied.contains(&text(&segment.path)))
        .collect();
    assert!(newer.len() >= 3, "precondition: {} newer segments", newer.len());
    let missing = newer[1];
    copy_segments(
        newer.iter().copied().filter(|segment| segment.path != missing.path),
        &feed,
    );
    let range = format!("{}..{}", missing.first_sequence(), missing.last_sequence());
    // (A file caught half-copied is refused too, until its copy completes:
    // wait for the gap itself.)
    let block = wait_ready_body(pair.http_s, Duration::from_secs(60), &follower, |code, body| {
        code == 503
            && body["state"] == "needs_recovery"
            && body["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("archive gap"))
    })
    .await;
    let reason = block["reason"].as_str().unwrap_or_default().to_string();
    assert!(reason.contains(&range), "{reason}");
    assert!(follower.alive(), "a gap leaves it up");

    // The segment appears (and fresh ones after it): it resumes by itself.
    copy_segments([missing], &feed);
    copied.extend(newer.iter().map(|segment| text(&segment.path)));
    assert_eq!(insert(&writer, 201..=210, "c").await, 0);
    let through = primary_sequence(&writer).await;
    let segments = wait_sealed_through(&archive, through, &primary).await;
    copy_segments(
        segments.iter().filter(|segment| !copied.contains(&text(&segment.path))),
        &feed,
    );
    let block = wait_ready_body(pair.http_s, Duration::from_secs(60), &follower, |code, _| code == 200).await;
    assert_eq!(block["state"], "recovering", "{block}");
    let want: Vec<i32> = (1..=210).collect();
    assert_eq!(settle(pair.pg_s, &want, Duration::from_secs(60)).await, Ok(want));
    assert!(follower.alive());
    assert!(!follower.log().contains("Bootstrapping physical standby"));
    assert!(
        follower.log().contains("Archive-only standby following"),
        "{}",
        follower.tail(12)
    );
}

/// Spec P3.2 §4, §5: primary → standby A (`cascade = true`) → standby B. B
/// bootstraps from A and follows it in the primary's history and sequence
/// numbers: its rows and default column family are the primary's, it is
/// ready, and A says it cascades and lists B.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cascading_standby_serves_a_downstream_standby_the_primarys_history() {
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let (repl_a, pg_b, http_b) = (free_port(), free_port(), free_port());
    let primary = Daemon::spawn(
        &dir,
        "primary",
        &dir.join("primary"),
        &pair.token,
        &pair.primary_args,
        None,
    );
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    assert_eq!(insert(&writer, 1..=10, "before").await, 0);

    let mut a_args = pair.standby_args.clone();
    a_args.extend(["--replication-port".to_string(), repl_a.to_string()]);
    let cascade = "[replication.physical]\ncascade = true\n[replication.archive]\nsegment_max_secs = 1\n";
    let standby_a = Daemon::spawn(
        &dir,
        "standby-a",
        &dir.join("standby-a"),
        &pair.token,
        &a_args,
        Some(cascade),
    );
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(90), &standby_a).await;
    assert!(
        standby_a.log_contains_within("Cascading replication port listening", Duration::from_secs(30)),
        "{}",
        standby_a.tail(12)
    );

    let b_data = dir.join("standby-b");
    let mut standby_b = Daemon::spawn(
        &dir,
        "standby-b",
        &b_data,
        &pair.token,
        &standby_args(pg_b, repl_a, http_b),
        None,
    );
    wait_http(http_b, "/ready", 200, Duration::from_secs(120), &standby_b).await;
    assert_eq!(insert(&writer, 11..=50, "after").await, 0);
    let through = primary_sequence(&writer).await;
    let want: Vec<i32> = (1..=50).collect();
    assert_eq!(settle(pg_b, &want, Duration::from_secs(90)).await, Ok(want));

    // B follows A at the primary's sequence, ready.
    let deadline = Instant::now() + Duration::from_secs(60);
    let block = loop {
        let (code, body) = http(http_b, "/ready").await.unwrap_or_default();
        if code == 200 && body["cursor"].as_str().is_some_and(|cursor| lsn(cursor) == through) {
            break body;
        }
        assert!(Instant::now() < deadline, "B never reached {through}: {body}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(block["primary"], format!("127.0.0.1:{repl_a}"), "{block}");
    assert_eq!(block["state"], "streaming", "{block}");

    // A says it cascades and lists B; its replication block is unchanged.
    let (_, health) = http(pair.http_s, "/health").await.unwrap();
    assert_eq!(health["cascade"], true, "{health}");
    assert!(health["replication"].get("cascade").is_none(), "{health}");
    let downstreams = health["standbys"].as_array().cloned().unwrap_or_default();
    assert_eq!(downstreams.len(), 1, "{health}");
    assert_eq!(downstreams[0]["pg_port"], pg_b, "{health}");

    // B's default column family is the primary's at the same sequence.
    standby_b.stop();
    assert_eq!(standby_cursor(&b_data), through);
    let (primary_now, primary_digest) = default_digest(&dir.join("primary"));
    assert_eq!(primary_now, through, "precondition: the primary wrote nothing since");
    let (_, b_digest) = default_digest(&b_data);
    assert_eq!(b_digest, primary_digest, "B holds the primary's default column family");
    drop(writer);
    drop(primary);
}
