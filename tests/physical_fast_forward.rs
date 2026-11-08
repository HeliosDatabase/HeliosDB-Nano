//! Spec P4.1 §5: `heliosdb-nano recover --from-primary` on real managed
//! daemons (`tests/support/physical_pair.rs`): the recovery ladder and the
//! incremental fast-forward.
//!
//! * (a) a standby away past its primary's WAL retention, the archive off,
//!   takes route 2: only the leaves that differ travel (well under 10% of
//!   the bytes for 1% of the rows rewritten), its default column family is
//!   the primary's export at the cut, its cursor is `{T, T}`, the stage
//!   reopens, and `start` resumes at `T` — no bootstrap — with the rows the
//!   primary took during the operation; (g) the same for an encrypted
//!   dataset, without the key;
//! * (b) a standby the primary still serves needs nothing copied (route 1);
//! * (c) rows deleted on the primary while the standby was away vanish;
//! * (d) a fast-forward killed at each crash point (`internal-tests`) leaves
//!   a directory `start` refuses with the dirty reason (`/ready` 503), and a
//!   re-run completes it to the same digest;
//! * (e) above `--full-copy-threshold` the ladder takes route 3:
//!   `--no-full-copy` exits 3 with the estimate, without `--replace` the copy
//!   is completed beside the directory and the command exits 3 with the two
//!   `mv` commands, and `--replace` swaps it in, keeping the old directory;
//! * (f) a directory of another history takes route 3 the same way;
//! * (h) an absent data directory gets the primary's snapshot (route 3, as
//!   `resync` would);
//! * (i) a cascading standby serves the fast-forward to its downstream;
//! * (sprinter 6146b39a5beb) a completed copy beside the directory is adopted
//!   only while the primary still serves it: a stale one is discarded and
//!   copied again, and an adopted one is reported as "nothing copied".
//!
//! Red on the base of this change (P3.2, `ddfe1708`): `recover` knew only
//! `--from-archive`, the protocol had no fast-forward session, and a standby
//! past the primary's retention could only be rebuilt by a full copy into a
//! new directory.

#![cfg(all(feature = "ha-tier1", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/physical_pair.rs"]
mod physical_pair;

use heliosdb_nano::replication::checkpoint_transfer::{physical_stage_history, ReopenedPhysicalStage, TransferLimits};
use heliosdb_nano::replication::physical::{decode_cursor, range_digest, CURSOR_KEY, LOCAL_CF};
use heliosdb_nano::replication::recover_primary::format_count;
use heliosdb_nano::Config;
use physical_pair::*;
use rocksdb::{Options, DB};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// Small leaves, so a test-sized dataset has hundreds of them.
const LEAF: &str = "64KiB";

fn recover(args: Vec<String>, env: Vec<(String, String)>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"));
    command.arg("recover").args(&args).env("RUST_LOG", "warn");
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().unwrap()
}

/// `recover` on a blocking thread, so the test can write meanwhile.
async fn recover_async(args: Vec<String>, env: Vec<(String, String)>) -> Output {
    tokio::task::spawn_blocking(move || recover(args, env)).await.unwrap()
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

/// `recover --from-primary` of `pair`'s standby directory, with `extra` flags.
fn from_primary(pair: &Pair, data: &Path, extra: &[&str]) -> Vec<String> {
    let mut args = vec![
        "--data-dir".to_string(),
        text(data),
        "--from-primary".to_string(),
        "--primary-host".to_string(),
        format!("127.0.0.1:{}", pair.repl_p),
        "--replication-token-file".to_string(),
        text(&pair.token),
        "--leaf-bytes".to_string(),
        LEAF.to_string(),
        "--then".to_string(),
        "stop".to_string(),
    ];
    args.extend(extra.iter().map(|arg| arg.to_string()));
    args
}

/// A stopped standby's cursor: (snapshot sequence, source sequence).
fn cursor_of(path: &Path) -> (u64, u64) {
    let db = DB::open_cf_for_read_only(&Options::default(), path, ["default", LOCAL_CF], false).unwrap();
    let bytes = db.get_cf(db.cf_handle(LOCAL_CF).unwrap(), CURSOR_KEY).unwrap().unwrap();
    let cursor = decode_cursor(&bytes).unwrap();
    (cursor.snapshot_sequence(), cursor.source_sequence())
}

/// The default column family's digest of the store at `path`, read-only.
fn digest_of(path: &Path) -> [u8; 32] {
    let db = DB::open_for_read_only(&Options::default(), path, false).unwrap();
    range_digest(&db, None, b"").unwrap()
}

/// The digest of the export checkpoint of `source_data` at `sequence`: the
/// cut a fast-forward served (its lease keeps it after the session).
fn export_digest_at(source_data: &Path, sequence: u64) -> [u8; 32] {
    let mut exports = source_data.as_os_str().to_os_string();
    exports.push(".physical-export");
    let exports = PathBuf::from(exports);
    let mut seen = Vec::new();
    for entry in std::fs::read_dir(&exports).unwrap() {
        let checkpoint = entry.unwrap().path().join("checkpoint");
        if !checkpoint.is_dir() {
            continue;
        }
        let db = DB::open_for_read_only(&Options::default(), &checkpoint, false).unwrap();
        seen.push(db.latest_sequence_number());
        if db.latest_sequence_number() == sequence {
            return range_digest(&db, None, b"").unwrap();
        }
    }
    panic!("no export of {} at {sequence}: {seen:?}", source_data.display());
}

/// The standby's directory reopens as the consistent stage every `start`
/// proves.
fn stage_reopens(path: &Path) {
    let history = physical_stage_history(path).unwrap();
    let stage = ReopenedPhysicalStage::open(path, history, TransferLimits::default(), &Config::default()).unwrap();
    stage.close().unwrap();
}

/// The percentage of the primary's bytes the report says differ.
fn differing_percent(report: &str) -> f64 {
    let line = report
        .lines()
        .find(|line| line.contains(" differ ("))
        .unwrap_or_else(|| panic!("no digest line: {report}"));
    line.split(" differ (")
        .nth(1)
        .and_then(|rest| rest.split('%').next())
        .and_then(|number| number.parse().ok())
        .unwrap_or_else(|| panic!("no percentage: {line}"))
}

/// Insert `ids` in multi-row statements of 500.
async fn bulk_insert(client: &tokio_postgres::Client, ids: std::ops::RangeInclusive<i32>, filler: &str) {
    let ids: Vec<i32> = ids.collect();
    for chunk in ids.chunks(500) {
        let values: Vec<String> = chunk.iter().map(|id| format!("({id}, '{filler}')")).collect();
        client
            .batch_execute(&format!("INSERT INTO t VALUES {}", values.join(", ")))
            .await
            .unwrap();
    }
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
            "/ready never answered as wanted; last {seen:?}{}",
            daemon.diagnose(port)
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// An owner-only dataset key and the `[encryption]` section that uses it.
fn encryption(dir: &Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let key = dir.join("dataset.key");
    std::fs::write(&key, "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    format!(
        "[encryption]\nenabled = true\nkey_source = {{ File = \"{}\" }}\n",
        key.display()
    )
}

/// The standby of `pair` (configuration `extra` on both nodes) follows its
/// primary to `base` rows and stops; the primary runs `changes`, then writes
/// on until its RocksDB WAL no longer holds the standby's next group — its
/// archive off, so nothing serves it: the fast-forward's precondition.
/// Returns the primary, its client and the last row id.
async fn away_past_retention(
    pair: &Pair,
    extra: Option<&str>,
    base: i32,
    changes: &str,
) -> (Daemon, tokio_postgres::Client, i32) {
    let dir = pair.dir.path();
    let config = format!("{}{}", pruning_primary_config(false), extra.unwrap_or_default());
    let primary_data = dir.join("primary");
    let primary = Daemon::spawn(
        dir,
        "primary",
        &primary_data,
        &pair.token,
        &pair.primary_args,
        Some(config.as_str()),
    );
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    bulk_insert(&writer, 1..=base, &"b".repeat(200)).await;
    let mut standby = Daemon::spawn(
        dir,
        "standby-1",
        &dir.join("standby"),
        &pair.token,
        &pair.standby_args,
        extra,
    );
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(120), &standby).await;
    let through = primary_sequence(&writer).await;
    wait_listed(pair.http_p, through, Duration::from_secs(90), &primary).await;
    standby.stop();

    writer.batch_execute(changes).await.unwrap();
    let filler = "w".repeat(400);
    let mut last = base;
    let mut pruned = 0;
    let deadline = Instant::now() + Duration::from_secs(120);
    while pruned == 0 {
        assert!(
            Instant::now() < deadline,
            "precondition: the primary never archived a WAL file; log tail: {}",
            primary.tail(12)
        );
        assert_eq!(insert(&writer, last + 1..=last + 150, &filler).await, 0);
        last += 150;
        pruned += prune_archived_wal(&primary_data);
    }
    // Three more rounds past the first pruned file, so the standby's next
    // group is well behind what the WAL still holds.
    for _ in 0..3 {
        assert_eq!(insert(&writer, last + 1..=last + 150, &filler).await, 0);
        last += 150;
        prune_archived_wal(&primary_data);
    }
    (primary, writer, last)
}

/// (a), (g): route 2, its invariants, and the tail after it.
async fn fast_forward_past_retention(encrypted: bool) {
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let extra = encrypted.then(|| encryption(&dir));
    let base = 40_000;
    let (primary, writer, last) = away_past_retention(
        &pair,
        extra.as_deref(),
        base,
        // 1% of the rows, rewritten.
        "UPDATE t SET v = 'changed' WHERE id BETWEEN 20001 AND 20400",
    )
    .await;
    let standby_dir = dir.join("standby");
    let before = cursor_of(&standby_dir);

    // The primary keeps serving writes during the operation. The recovery
    // runs without the dataset key: it moves raw bytes.
    let running = recover_async(from_primary(&pair, &standby_dir, &[]), Vec::new());
    let during = insert(&writer, last + 1..=last + 30, "during");
    let (output, failed) = tokio::join!(running, during);
    assert_eq!(failed, 0);
    let report = stdout(&output);
    assert!(output.status.success(), "{report}{}", stderr(&output));
    assert!(report.contains("is behind the primary"), "{report}");
    assert!(report.contains("fast-forward from the primary"), "{report}");
    assert!(!report.contains("full copy"), "{report}");
    let percent = differing_percent(&report);
    assert!(
        percent < 10.0,
        "{percent}% of the bytes differ for 1% of the rows: {report}"
    );

    let (snapshot, source) = cursor_of(&standby_dir);
    assert_eq!(snapshot, source, "the cursor is re-cut at the cut");
    assert!(source > before.1);
    assert!(
        report.contains(&format!("the standby is at sequence {}", format_count(source))),
        "{report}"
    );
    assert_eq!(
        digest_of(&standby_dir),
        export_digest_at(&dir.join("primary"), source),
        "the primary's default column family at the cut, byte for byte"
    );
    stage_reopens(&standby_dir);
    assert!(
        !dir.join("standby.recover-stage").exists(),
        "the stage directory is removed when done"
    );
    // Written after the recovery returned: past the cut for certain.
    assert_eq!(insert(&writer, last + 31..=last + 60, "after").await, 0);

    // `start` resumes at the cut and tails the rest, with no bootstrap.
    let standby = Daemon::spawn(
        &dir,
        "standby-2",
        &standby_dir,
        &pair.token,
        &pair.standby_args,
        extra.as_deref(),
    );
    let want: Vec<i32> = (1..=last + 60).collect();
    assert_eq!(settle(pair.pg_s, &want, Duration::from_secs(120)).await, Ok(want));
    let log = standby.log();
    assert!(!log.contains("Bootstrapping physical standby"), "{}", standby.tail(20));
    assert!(log.contains("Resuming physical standby"), "{}", standby.tail(20));
    let block = wait_ready_body(pair.http_s, Duration::from_secs(60), &standby, |code, _| code == 200).await;
    assert_eq!(block["state"], "streaming", "{block}");
    drop(writer);
    drop(primary);
}

/// (a) Spec P4.1 §3.4 (1), (2), (6).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_standby_away_past_retention_is_fast_forwarded_shipping_only_what_differs() {
    fast_forward_past_retention(false).await;
}

/// (g) Spec P4.1 §3.4 (5): an encrypted dataset, without the key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_encrypted_standby_is_fast_forwarded_without_the_dataset_key() {
    fast_forward_past_retention(true).await;
}

/// (b) Route 1: the primary still serves the standby's next group — nothing
/// is copied, nothing is created, and the standby follows as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_standby_the_primary_still_serves_needs_nothing_copied() {
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let _primary = Daemon::spawn(
        &dir,
        "primary",
        &dir.join("primary"),
        &pair.token,
        &pair.primary_args,
        None,
    );
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    assert_eq!(insert(&writer, 1..=10, "a").await, 0);
    let standby_dir = dir.join("standby");
    let mut standby = Daemon::spawn(&dir, "standby", &standby_dir, &pair.token, &pair.standby_args, None);
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(90), &standby).await;
    assert_eq!(
        settle(pair.pg_s, &(1..=10).collect::<Vec<_>>(), Duration::from_secs(30)).await,
        Ok((1..=10).collect())
    );
    standby.stop();
    assert_eq!(insert(&writer, 11..=20, "b").await, 0);
    let before = cursor_of(&standby_dir);
    let markers = |path: &Path| {
        (
            std::fs::read(path.join("NANO-PHYSICAL-REPLICA.json")).unwrap(),
            std::fs::read(path.join("NANO-PHYSICAL-MANIFEST.json")).unwrap(),
        )
    };
    let markers_before = markers(&standby_dir);
    let digest_before = digest_of(&standby_dir);

    let output = recover(from_primary(&pair, &standby_dir, &[]), Vec::new());
    let report = stdout(&output);
    assert!(output.status.success(), "{report}{}", stderr(&output));
    assert!(report.contains("nothing to copy"), "{report}");
    assert!(report.contains("nothing copied"), "{report}");
    assert!(!report.contains("fast-forward from the primary"), "{report}");
    assert_eq!(cursor_of(&standby_dir), before, "nothing was applied");
    assert_eq!(digest_of(&standby_dir), digest_before);
    assert_eq!(markers(&standby_dir), markers_before, "the stage was not re-cut");
    assert!(!dir.join("standby.recover-stage").exists());

    let resumed = Daemon::spawn(&dir, "standby-2", &standby_dir, &pair.token, &pair.standby_args, None);
    let want: Vec<i32> = (1..=20).collect();
    assert_eq!(settle(pair.pg_s, &want, Duration::from_secs(60)).await, Ok(want));
    assert!(!resumed.log().contains("Bootstrapping physical standby"));
}

/// (c) Rows deleted on the primary while the standby was away vanish: their
/// ranges are emptied before the primary's leaves are ingested.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rows_deleted_on_the_primary_while_away_vanish() {
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let (_primary, writer, last) =
        away_past_retention(&pair, None, 6_000, "DELETE FROM t WHERE id BETWEEN 1001 AND 1300").await;
    let standby_dir = dir.join("standby");
    let output = recover(from_primary(&pair, &standby_dir, &[]), Vec::new());
    let report = stdout(&output);
    assert!(output.status.success(), "{report}{}", stderr(&output));
    assert!(report.contains("fast-forward from the primary"), "{report}");
    let (_, source) = cursor_of(&standby_dir);
    assert_eq!(digest_of(&standby_dir), export_digest_at(&dir.join("primary"), source));

    let _standby = Daemon::spawn(&dir, "standby-2", &standby_dir, &pair.token, &pair.standby_args, None);
    let want: Vec<i32> = (1..=1000).chain(1301..=last).collect();
    assert_eq!(settle(pair.pg_s, &want, Duration::from_secs(120)).await, Ok(want));
    drop(writer);
}

/// (d) Spec P4.1 §3.4 (3): a fast-forward killed at each crash point leaves
/// a directory `start` refuses — staying up, `/ready` 503, the reason naming
/// the interrupted fast-forward and the command that finishes it — and the
/// next run continues it; the last one completes to the cut's exact digest.
#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fast_forward_killed_at_any_point_is_refused_by_start_and_finished_by_a_rerun() {
    use std::os::unix::process::ExitStatusExt;
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let (_primary, writer, last) = away_past_retention(
        &pair,
        None,
        8_000,
        "UPDATE t SET v = 'changed' WHERE id BETWEEN 4001 AND 4100",
    )
    .await;
    let standby_dir = dir.join("standby");
    for (round, phase) in ["after-delete:0", "after-ingest:0", "before-recut:0", "after-manifest:0"]
        .into_iter()
        .enumerate()
    {
        let output = recover(
            from_primary(&pair, &standby_dir, &[]),
            vec![("HELIOSDB_NANO_TEST_KILL_FAST_FORWARD_AT".to_string(), phase.to_string())],
        );
        assert_eq!(
            output.status.signal(),
            Some(libc::SIGKILL),
            "{phase}: {}{}",
            stdout(&output),
            stderr(&output)
        );
        if round > 0 {
            assert!(
                stdout(&output).contains("carries an interrupted fast-forward"),
                "{phase}: {}",
                stdout(&output)
            );
        }
        let mut refused = Daemon::spawn(
            &dir,
            &format!("refused-{round}"),
            &standby_dir,
            &pair.token,
            &pair.standby_args,
            None,
        );
        let block = wait_ready_body(pair.http_s, Duration::from_secs(60), &refused, |code, body| {
            code == 503 && body["state"] == "needs_recovery"
        })
        .await;
        let reason = block["reason"].as_str().unwrap_or_default().to_string();
        assert!(
            reason.contains("fast-forward from the primary interrupted"),
            "{phase}: {reason}"
        );
        assert!(
            reason.contains(&format!("--data-dir {}", standby_dir.display())),
            "{phase}: {reason}"
        );
        assert!(refused.alive(), "{phase}: a refused standby stays up");
        refused.stop();
    }
    let output = recover(from_primary(&pair, &standby_dir, &[]), Vec::new());
    let report = stdout(&output);
    assert!(output.status.success(), "{report}{}", stderr(&output));
    assert!(report.contains("carries an interrupted fast-forward"), "{report}");
    let (snapshot, source) = cursor_of(&standby_dir);
    assert_eq!(snapshot, source);
    assert_eq!(digest_of(&standby_dir), export_digest_at(&dir.join("primary"), source));
    stage_reopens(&standby_dir);

    let standby = Daemon::spawn(&dir, "standby-2", &standby_dir, &pair.token, &pair.standby_args, None);
    let want: Vec<i32> = (1..=last).collect();
    assert_eq!(settle(pair.pg_s, &want, Duration::from_secs(120)).await, Ok(want));
    assert!(!standby.log().contains("Bootstrapping physical standby"));
    drop(writer);
}

/// (e) Spec P4.1 §3.4 (7): most of the data rewritten — the full copy is
/// cheaper. `--no-full-copy` exits 3 with the estimate, touching nothing;
/// without `--replace` the copy is completed beside the directory and the
/// command exits 3 naming both `mv`s; `--replace` uses that copy, keeping
/// the old directory.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn above_the_threshold_a_full_copy_is_made_and_swapped_in_only_with_replace() {
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let (_primary, writer, last) =
        away_past_retention(&pair, None, 3_000, "UPDATE t SET v = 'every row rewritten'").await;
    let standby_dir = dir.join("standby");
    let before = cursor_of(&standby_dir);

    let output = recover(from_primary(&pair, &standby_dir, &["--no-full-copy"]), Vec::new());
    let report = stdout(&output);
    assert_eq!(output.status.code(), Some(3), "{report}{}", stderr(&output));
    assert!(report.contains("--full-copy-threshold 50%"), "{report}");
    assert!(report.contains("--no-full-copy"), "{report}");
    assert_eq!(cursor_of(&standby_dir), before, "nothing was applied");

    let output = recover(from_primary(&pair, &standby_dir, &[]), Vec::new());
    let report = stdout(&output);
    assert_eq!(output.status.code(), Some(3), "{report}{}", stderr(&output));
    let copy = dir.join("standby.recover-full");
    assert!(report.contains("a full copy is cheaper"), "{report}");
    // The paths the command prints are canonical.
    let data = std::fs::canonicalize(&standby_dir).unwrap();
    let copy_named = data.with_file_name("standby.recover-full");
    let old_prefix = format!("mv {} {}.replaced-", data.display(), data.display());
    let swap_old = report
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with(&old_prefix))
        .unwrap_or_else(|| panic!("no `{old_prefix}<UTC>` line: {report}"));
    let stamp = &swap_old[old_prefix.len()..];
    assert!(
        stamp.len() == 16
            && stamp.ends_with('Z')
            && stamp.as_bytes()[8] == b'T'
            && stamp.bytes().filter(u8::is_ascii_digit).count() == 14,
        "the whole line `mv <D> <D>.replaced-<YYYYmmddTHHMMSSZ>`: {swap_old}"
    );
    assert!(
        report
            .lines()
            .map(str::trim)
            .any(|line| line == format!("mv {} {}", copy_named.display(), data.display())),
        "{report}"
    );
    assert_eq!(cursor_of(&standby_dir), before, "the occupied directory is untouched");
    let (_, copied) = cursor_of(&copy);
    assert!(copied > before.1);

    let output = recover(from_primary(&pair, &standby_dir, &["--replace"]), Vec::new());
    let report = stdout(&output);
    assert!(output.status.success(), "{report}{}", stderr(&output));
    assert!(report.contains("already holds a completed copy"), "{report}");
    // Sprinter 6146b39a5beb: adopted because the primary still serves it,
    // and the summary says nothing was copied (it used to say "0 B").
    assert!(report.contains("which the primary still serves"), "{report}");
    assert!(report.contains("nothing copied in this run"), "{report}");
    assert!(!report.contains("a full copy of the primary, 0 B"), "{report}");
    assert!(report.contains("it is kept for the operator"), "{report}");
    assert!(!copy.exists());
    let replaced: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("standby.replaced-"))
        })
        .collect();
    assert_eq!(replaced.len(), 1, "{replaced:?}");
    assert_eq!(cursor_of(&replaced[0]), before, "the old directory, kept as it was");
    assert_eq!(cursor_of(&standby_dir).1, copied, "the copy is the standby now");

    let standby = Daemon::spawn(&dir, "standby-2", &standby_dir, &pair.token, &pair.standby_args, None);
    let want: Vec<i32> = (1..=last).collect();
    assert_eq!(settle(pair.pg_s, &want, Duration::from_secs(120)).await, Ok(want));
    assert!(!standby.log().contains("Bootstrapping physical standby"));
    drop(writer);
}

/// (f) A standby of another dataset history: the primary refuses it
/// (`HistoryMismatch`), so only a full copy can help — beside the directory,
/// swapped in with `--replace`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_standby_of_another_history_needs_a_full_copy_and_replace() {
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let standby_dir = dir.join("standby");
    {
        let _old = Daemon::spawn(
            &dir,
            "primary-old",
            &dir.join("primary-old"),
            &pair.token,
            &pair.primary_args,
            None,
        );
        let writer = connect(pair.pg_p).await;
        create_table(&writer).await;
        assert_eq!(insert(&writer, 1..=5, "old").await, 0);
        let mut standby = Daemon::spawn(&dir, "standby", &standby_dir, &pair.token, &pair.standby_args, None);
        wait_http(pair.http_s, "/ready", 200, Duration::from_secs(90), &standby).await;
        standby.stop();
    }
    // Another dataset on the same replication port.
    let _new = Daemon::spawn(
        &dir,
        "primary-new",
        &dir.join("primary-new"),
        &pair.token,
        &pair.primary_args,
        None,
    );
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    assert_eq!(insert(&writer, 100..=120, "new").await, 0);
    let old_history = physical_stage_history(&standby_dir).unwrap();

    let output = recover(from_primary(&pair, &standby_dir, &[]), Vec::new());
    let report = stdout(&output);
    assert_eq!(output.status.code(), Some(3), "{report}{}", stderr(&output));
    assert!(report.contains("history mismatch"), "{report}");
    assert!(report.contains("--replace"), "{report}");
    assert_eq!(physical_stage_history(&standby_dir).unwrap(), old_history);

    let output = recover(from_primary(&pair, &standby_dir, &["--replace"]), Vec::new());
    let report = stdout(&output);
    assert!(output.status.success(), "{report}{}", stderr(&output));
    assert_ne!(physical_stage_history(&standby_dir).unwrap(), old_history);
    let standby = Daemon::spawn(&dir, "standby-2", &standby_dir, &pair.token, &pair.standby_args, None);
    let want: Vec<i32> = (100..=120).collect();
    assert_eq!(settle(pair.pg_s, &want, Duration::from_secs(90)).await, Ok(want));
    assert!(!standby.log().contains("Bootstrapping physical standby"));
}

/// Sprinter 6146b39a5beb: a completed copy left beside the directory by an
/// earlier run is adopted only while the primary still serves its next write
/// group. Here the primary's WAL moved past it (its archive off) before the
/// re-run, which says so, removes it, copies again, and installs a standby
/// that follows the primary — where the stale copy was adopted as it was, and
/// its first `start` could only be refused `WalPruned`. (`internal-tests`:)
/// a transient refusal of the probe keeps the copy and fails the run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_completed_copy_is_discarded_and_copied_again() {
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let standby_dir = dir.join("standby");
    {
        // A standby of another dataset: only a full copy beside it helps.
        let _old = Daemon::spawn(
            &dir,
            "primary-old",
            &dir.join("primary-old"),
            &pair.token,
            &pair.primary_args,
            None,
        );
        let writer = connect(pair.pg_p).await;
        create_table(&writer).await;
        assert_eq!(insert(&writer, 1..=5, "old").await, 0);
        let mut standby = Daemon::spawn(&dir, "standby", &standby_dir, &pair.token, &pair.standby_args, None);
        wait_http(pair.http_s, "/ready", 200, Duration::from_secs(90), &standby).await;
        standby.stop();
    }
    let primary_data = dir.join("primary-new");
    let config = pruning_primary_config(false);
    let primary = Daemon::spawn(
        &dir,
        "primary-new",
        &primary_data,
        &pair.token,
        &pair.primary_args,
        Some(config.as_str()),
    );
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    assert_eq!(insert(&writer, 1..=20, "new").await, 0);

    // Without --replace the copy is completed beside the directory.
    let output = recover(from_primary(&pair, &standby_dir, &[]), Vec::new());
    assert_eq!(output.status.code(), Some(3), "{}{}", stdout(&output), stderr(&output));
    let copy = dir.join("standby.recover-full");
    let (_, copied_at) = cursor_of(&copy);

    // The primary writes on until its WAL no longer holds the copy's next
    // write group (nothing archives it).
    let filler = "w".repeat(400);
    let mut last = 20;
    let mut pruned = 0;
    let deadline = Instant::now() + Duration::from_secs(120);
    while pruned == 0 {
        assert!(
            Instant::now() < deadline,
            "precondition: the primary never archived a WAL file; log tail: {}",
            primary.tail(12)
        );
        assert_eq!(insert(&writer, last + 1..=last + 150, &filler).await, 0);
        last += 150;
        pruned += prune_archived_wal(&primary_data);
    }
    for _ in 0..3 {
        assert_eq!(insert(&writer, last + 1..=last + 150, &filler).await, 0);
        last += 150;
        prune_archived_wal(&primary_data);
    }

    // A primary that cannot answer for the copy right now — here refused
    // `TooManySessions` through every retry (the `internal-tests` hook) —
    // says nothing about the copy: it is kept, and the command fails.
    #[cfg(feature = "internal-tests")]
    {
        let output = recover(
            from_primary(&pair, &standby_dir, &["--replace"]),
            vec![(
                "HELIOSDB_NANO_TEST_COPY_PROBE_REFUSE".to_string(),
                "TooManySessions".to_string(),
            )],
        );
        let said = format!("{}{}", stdout(&output), stderr(&output));
        assert_eq!(output.status.code(), Some(1), "{said}");
        assert!(said.contains("the copy is kept"), "{said}");
        assert!(!said.contains("copying again"), "{said}");
        assert_eq!(cursor_of(&copy).1, copied_at, "the completed copy is kept as it was");
    }

    let output = recover(from_primary(&pair, &standby_dir, &["--replace"]), Vec::new());
    let report = stdout(&output);
    assert!(output.status.success(), "{report}{}", stderr(&output));
    assert!(
        report.contains("is older than what the primary retains"),
        "the stale copy must not be adopted: {report}"
    );
    assert!(report.contains("copying again"), "{report}");
    assert!(!report.contains("nothing copied in this run"), "{report}");
    let (_, installed_at) = cursor_of(&standby_dir);
    assert!(
        installed_at > copied_at,
        "a new copy, past the stale one: {installed_at} vs {copied_at}"
    );
    assert!(!copy.exists(), "the copy was swapped in");

    let standby = Daemon::spawn(&dir, "standby-2", &standby_dir, &pair.token, &pair.standby_args, None);
    let want: Vec<i32> = (1..=last).collect();
    assert_eq!(settle(pair.pg_s, &want, Duration::from_secs(120)).await, Ok(want));
    assert!(!standby.log().contains("Bootstrapping physical standby"));
    drop(writer);
    drop(primary);
}

/// (h) An absent data directory gets the primary's snapshot, as `resync`
/// would install it, and `start` resumes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_absent_data_directory_gets_the_primarys_snapshot() {
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let _primary = Daemon::spawn(
        &dir,
        "primary",
        &dir.join("primary"),
        &pair.token,
        &pair.primary_args,
        None,
    );
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    assert_eq!(insert(&writer, 1..=25, "row").await, 0);
    let standby_dir = dir.join("fresh");
    let output = recover(from_primary(&pair, &standby_dir, &[]), Vec::new());
    let report = stdout(&output);
    assert!(output.status.success(), "{report}{}", stderr(&output));
    assert!(report.contains("holds no standby yet"), "{report}");
    assert!(report.contains("a full copy of the primary"), "{report}");
    let (snapshot, source) = cursor_of(&standby_dir);
    assert!(source >= snapshot && source > 0);
    stage_reopens(&standby_dir);

    let standby = Daemon::spawn(&dir, "standby", &standby_dir, &pair.token, &pair.standby_args, None);
    let want: Vec<i32> = (1..=25).collect();
    assert_eq!(settle(pair.pg_s, &want, Duration::from_secs(90)).await, Ok(want));
    assert!(!standby.log().contains("Bootstrapping physical standby"));
    assert!(standby.log().contains("Resuming physical standby"));
}

/// (i) A cascading standby serves the fast-forward to its downstream: B,
/// away while A's WAL moved past it (A keeps no archive), is fast-forwarded
/// from A's export, in the primary's history and sequence numbers, and then
/// follows A again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cascading_standby_fast_forwards_its_downstream() {
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let (repl_a, pg_b, http_b) = (free_port(), free_port(), free_port());
    let _primary = Daemon::spawn(
        &dir,
        "primary",
        &dir.join("primary"),
        &pair.token,
        &pair.primary_args,
        None,
    );
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    bulk_insert(&writer, 1..=2_000, &"b".repeat(200)).await;

    let mut a_args = pair.standby_args.clone();
    a_args.extend(["--replication-port".to_string(), repl_a.to_string()]);
    // No archive of its own: once A's WAL no longer holds B's next group,
    // nothing on A serves it.
    let cascade = "[replication.physical]\ncascade = true\n[replication.archive]\nenabled = false\n";
    let a_data = dir.join("standby-a");
    let mut standby_a = Daemon::spawn(&dir, "standby-a", &a_data, &pair.token, &a_args, Some(cascade));
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(90), &standby_a).await;
    assert!(
        standby_a.log_contains_within("Cascading replication port listening", Duration::from_secs(30)),
        "{}",
        standby_a.tail(12)
    );
    let b_data = dir.join("standby-b");
    let b_args = standby_args(pg_b, repl_a, http_b);
    let mut standby_b = Daemon::spawn(&dir, "standby-b", &b_data, &pair.token, &b_args, None);
    wait_http(http_b, "/ready", 200, Duration::from_secs(120), &standby_b).await;
    standby_b.stop();
    let before = cursor_of(&b_data);

    // A applies more while B is away, then restarts: its open flushes its
    // memtables, and a receiver keeps no obsolete WAL, so A's WAL no longer
    // holds B's next group.
    // A small change set (a tenth of the rows): the fast-forward is the
    // cheaper route. A change touching half the bytes would, correctly, be
    // a full copy.
    assert_eq!(insert(&writer, 2_001..=2_200, &"w".repeat(200)).await, 0);
    let through = primary_sequence(&writer).await;
    let caught_up = |daemon: &Daemon| {
        let deadline = Instant::now() + Duration::from_secs(90);
        let port = pair.http_s;
        let tail = daemon.tail(12);
        async move {
            loop {
                let (code, body) = http(port, "/ready").await.unwrap_or_default();
                if code == 200 && body["cursor"].as_str().is_some_and(|cursor| lsn(cursor) >= through) {
                    break;
                }
                assert!(Instant::now() < deadline, "A never reached {through}: {body}; {tail}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    };
    caught_up(&standby_a).await;
    standby_a.stop();
    let standby_a = Daemon::spawn(&dir, "standby-a-2", &a_data, &pair.token, &a_args, Some(cascade));
    caught_up(&standby_a).await;
    assert!(
        standby_a.log_contains_within("Cascading replication port listening", Duration::from_secs(30)),
        "{}",
        standby_a.tail(12)
    );

    let args = vec![
        "--data-dir".to_string(),
        text(&b_data),
        "--from-primary".to_string(),
        "--primary-host".to_string(),
        format!("127.0.0.1:{repl_a}"),
        "--replication-token-file".to_string(),
        text(&pair.token),
        "--leaf-bytes".to_string(),
        LEAF.to_string(),
        "--then".to_string(),
        "stop".to_string(),
    ];
    let output = recover(args, Vec::new());
    let report = stdout(&output);
    assert!(output.status.success(), "{report}{}", stderr(&output));
    assert!(report.contains("fast-forward from the primary"), "{report}");
    let (snapshot, source) = cursor_of(&b_data);
    assert_eq!(snapshot, source);
    assert!(source > before.1);
    // A's export holds the primary's default column family at its source
    // sequence, which is the primary's last (it wrote nothing since).
    let primary_db = DB::open_for_read_only(&Options::default(), dir.join("primary"), false).unwrap();
    assert_eq!(
        primary_db.latest_sequence_number(),
        source,
        "precondition: A held every write of the primary when it exported"
    );
    assert_eq!(digest_of(&b_data), range_digest(&primary_db, None, b"").unwrap());
    drop(primary_db);

    let _standby_b = Daemon::spawn(&dir, "standby-b-2", &b_data, &pair.token, &b_args, None);
    let want: Vec<i32> = (1..=2_200).collect();
    assert_eq!(settle(pg_b, &want, Duration::from_secs(120)).await, Ok(want));
    drop(standby_a);
}

/// `recover --from-primary` with its default `--then primary`, on a small
/// store: the token given to `recover` itself reaches the `start` it goes on
/// to, and the command ends as a node streaming from the primary, on the port
/// given after `--`. (Through the review it did its work and then refused to
/// follow: "a physical standby requires --replication-token-file".)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recover_that_goes_on_to_follow_hands_its_token_to_the_start() {
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let s_data = dir.join("standby");
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
    let following = Daemon::spawn_cli(
        &dir,
        "recover-follows",
        &[
            "recover".to_string(),
            "--data-dir".to_string(),
            text(&s_data),
            "--from-primary".to_string(),
            "--primary-host".to_string(),
            format!("127.0.0.1:{}", pair.repl_p),
            "--replication-token-file".to_string(),
            text(&pair.token),
            "--".to_string(),
            "--listen".to_string(),
            "127.0.0.1".to_string(),
            "--auth".to_string(),
            "trust".to_string(),
            "--port".to_string(),
            pair.pg_s.to_string(),
            "--http-port".to_string(),
            pair.http_s.to_string(),
            "--replication-startup-timeout-secs".to_string(),
            "30".to_string(),
        ],
    );
    let ready = wait_http(pair.http_s, "/ready", 200, Duration::from_secs(120), &following).await;
    assert_eq!(ready["state"], "streaming", "{ready}");
    assert!(
        following.log().contains("as `start --replication-role standby` would"),
        "{}",
        following.tail(20)
    );
    let mut want: Vec<i32> = (1..=10).collect();
    assert_eq!(
        settle(pair.pg_s, &want, Duration::from_secs(60)).await,
        Ok(want.clone())
    );
    assert_eq!(insert(&writer, 11..=15, "after").await, 0);
    want.extend(11..=15);
    assert_eq!(settle(pair.pg_s, &want, Duration::from_secs(60)).await, Ok(want));
    drop(following);
    drop(primary);
}

/// Mandatory scale cell from STANDBY_RECOVERY_PLAN §6: an actual >=1GiB
/// logical dataset, exactly1% clustered row changes, native child SIGKILL,
/// dirty-start refusal, resumed byte-digest equality and writes through tail.
/// Kept outside the ordinary bounded tier; the release scale stage names it.
#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "mandatory P4 scale acceptance; run explicitly under the fleet24GiB scope"]
async fn a_gib_dataset_with_one_percent_changed_resumes_after_kill_and_tails_new_writes() {
    use heliosdb_nano::replication::physical_tls::ReplicationTransport;
    use heliosdb_nano::replication::physical_wire::WireLimits;
    use heliosdb_nano::replication::recover_primary::{
        recover_from_primary, DirectoryState, PrimaryRecoverOptions, RecoverRoute,
    };
    use std::os::unix::process::ExitStatusExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    const BASE: i32 = 20_000;
    const PAYLOAD: usize = 55 * 1024;
    const CHANGED: i32 = BASE / 100;
    fn payload(id: i32, version: u64) -> String {
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        let mut state = (id as u64 + 1).wrapping_mul(0x9e3779b97f4a7c15) ^ version.wrapping_mul(0xd6e8feb86659fd93);
        let mut bytes = Vec::with_capacity(PAYLOAD);
        for _ in 0..PAYLOAD {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            bytes.push(ALPHABET[(state % ALPHABET.len() as u64) as usize]);
        }
        String::from_utf8(bytes).unwrap()
    }
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let data = dir.join("primary");
    let config = pruning_primary_config(false).replace(
        "wal_retention_secs = 60",
        "operation_timeout_secs = 900\nwal_retention_secs = 60",
    );
    // Seed with ordinary memtables, so >=1GiB does not manufacture tens of
    // thousands of tiny SSTs. Reopen with tiny buffers only for retention churn.
    let seed_config = config.replace(
        "rocksdb_write_buffer_size = 65536",
        "rocksdb_write_buffer_size = 16777216",
    );
    let receiver_config = "[replication.physical]\noperation_timeout_secs = 900\n";
    let recover_config = dir.join("scale-recover.toml");
    std::fs::write(&recover_config, receiver_config).unwrap();
    let mut primary = Daemon::spawn(
        &dir,
        "primary-scale",
        &data,
        &pair.token,
        &pair.primary_args,
        Some(&seed_config),
    );
    let mut writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    let statement = writer.prepare("INSERT INTO t VALUES ($1,$2)").await.unwrap();
    for id in 1..=BASE {
        assert_eq!(writer.execute(&statement, &[&id, &payload(id, 0)]).await.unwrap(), 1);
    }
    let logical_bytes: i64 = writer
        .query_one("SELECT CAST(SUM(LENGTH(v)) AS BIGINT) FROM t", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(logical_bytes, i64::from(BASE) * PAYLOAD as i64);
    assert!(logical_bytes >= 1 << 30, "the actual SQL dataset must be at least1GiB");
    let standby_dir = dir.join("standby");
    let mut standby = Daemon::spawn(
        &dir,
        "standby-scale",
        &standby_dir,
        &pair.token,
        &pair.standby_args,
        Some(receiver_config),
    );
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(900), &standby).await;
    let through = primary_sequence(&writer).await;
    wait_listed(pair.http_p, through, Duration::from_secs(600), &primary).await;
    standby.stop();
    let before = cursor_of(&standby_dir);
    drop(writer);
    primary.stop();
    primary = Daemon::spawn(
        &dir,
        "primary-scale-pruning",
        &data,
        &pair.token,
        &pair.primary_args,
        Some(&config),
    );
    writer = connect(pair.pg_p).await;
    let update = writer.prepare("UPDATE t SET v=$1 WHERE id=$2").await.unwrap();
    // A contiguous range makes the chosen1% locality explicit. A uniformly
    // random mutation can touch most leaves and properly select a full copy.
    for id in BASE / 2 + 1..=BASE / 2 + CHANGED {
        assert_eq!(writer.execute(&update, &[&payload(id, 1), &id]).await.unwrap(), 1);
    }
    let last = BASE;
    let mut version = 1_u64;
    let mut pruned = prune_archived_wal(&data);
    let deadline = Instant::now() + Duration::from_secs(180);
    while pruned == 0 {
        assert!(
            Instant::now() < deadline,
            "retention precondition: {}",
            primary.tail(20)
        );
        version += 1;
        for id in BASE / 2 + 1..=BASE / 2 + CHANGED {
            assert_eq!(writer.execute(&update, &[&payload(id, version), &id]).await.unwrap(), 1);
        }
        pruned += prune_archived_wal(&data);
    }
    // Retention churn stays within the SAME1% rows: no hidden filler rows or
    // logical changes outside the independently declared mutation interval.
    for _ in 0..3 {
        version += 1;
        for id in BASE / 2 + 1..=BASE / 2 + CHANGED {
            assert_eq!(writer.execute(&update, &[&payload(id, version), &id]).await.unwrap(), 1);
        }
        prune_archived_wal(&data);
    }
    let killed = recover_async(
        from_primary(&pair, &standby_dir, &["--config", &text(&recover_config)]),
        vec![(
            "HELIOSDB_NANO_TEST_KILL_FAST_FORWARD_AT".into(),
            "after-ingest:0".into(),
        )],
    )
    .await;
    assert_eq!(
        killed.status.signal(),
        Some(libc::SIGKILL),
        "{}{}",
        stdout(&killed),
        stderr(&killed)
    );
    let mut refused = Daemon::spawn(
        &dir,
        "scale-dirty-refused",
        &standby_dir,
        &pair.token,
        &pair.standby_args,
        Some(receiver_config),
    );
    let body = wait_ready_body(pair.http_s, Duration::from_secs(60), &refused, |code, body| {
        code == 503 && body["state"] == "needs_recovery"
    })
    .await;
    assert!(body["reason"]
        .as_str()
        .unwrap()
        .contains("fast-forward from the primary interrupted"));
    assert!(refused.alive());
    refused.stop();
    let mut wire = WireLimits::default();
    wire.operation_timeout = Duration::from_secs(900);
    let options = PrimaryRecoverOptions {
        data_dir: standby_dir.clone(),
        layout: DirectoryState::Physical {
            history: physical_stage_history(&standby_dir).unwrap(),
        },
        config: Config::default(),
        primary_host: format!("127.0.0.1:{}", pair.repl_p),
        sources: vec![format!("127.0.0.1:{}", pair.repl_p).parse().unwrap()],
        transport: ReplicationTransport {
            tls: None,
            allow_insecure_remote: false,
        },
        token: heliosdb_nano::replication::read_replication_token(&pair.token).unwrap(),
        wire,
        full: false,
        no_full_copy: true,
        full_copy_threshold: 50,
        leaf_bytes: 64 * 1024,
        replace: false,
        stage_dir: dir.join("standby.recover-stage"),
    };
    let (start_tx, start_rx) = tokio::sync::oneshot::channel::<()>();
    let (committed_tx, committed_rx) = std::sync::mpsc::channel::<()>();
    // FnMut+Send borrows this receiver; Mutex makes the shared capture Sync.
    let committed_rx = std::sync::Mutex::new(committed_rx);
    let finished = Arc::new(AtomicBool::new(false));
    let seen_finished = Arc::clone(&finished);
    let during_id = last + 1;
    let pg = pair.pg_p;
    let during = tokio::spawn(async move {
        start_rx.await.unwrap();
        let client = connect(pg).await;
        assert_eq!(
            client
                .execute("INSERT INTO t VALUES ($1,'during recovery')", &[&during_id])
                .await
                .unwrap(),
            1
        );
        assert!(
            !seen_finished.load(Ordering::SeqCst),
            "the acknowledged write must occur during recovery"
        );
        committed_tx.send(()).unwrap();
    });
    let mut start_tx = Some(start_tx);
    let mut lines = Vec::new();
    let report = recover_from_primary(&options, &mut |line| {
        eprintln!("P4_SCALE_PROGRESS {line}");
        lines.push(line.to_string());
        if line.contains(" to ship") {
            if let Some(start) = start_tx.take() {
                start.send(()).unwrap();
                // Deterministic handoff: writer is a separate task. This is test
                // coordination after the source cut, never a storage lock wait.
                tokio::task::block_in_place(|| {
                    committed_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(60))
                        .unwrap()
                });
            }
        }
    })
    .await
    .unwrap();
    finished.store(true, Ordering::SeqCst);
    during.await.unwrap();
    assert!(start_tx.is_none(), "the real differing-leaf plan must be observed");
    assert!(matches!(report.route, RecoverRoute::FastForward));
    assert!(report.bytes_total >= 1 << 30);
    assert!(
        report.bytes_shipped > 0 && report.bytes_shipped * 10 < report.bytes_total,
        "{report:?}"
    );
    assert!(report.to > before.1);
    let cursor = cursor_of(&standby_dir);
    assert_eq!(cursor, (report.to, report.to));
    let actual = digest_of(&standby_dir);
    let expected = export_digest_at(&data, report.to);
    assert_eq!(actual, expected);
    stage_reopens(&standby_dir);
    let resumed = Daemon::spawn(
        &dir,
        "standby-scale-resumed",
        &standby_dir,
        &pair.token,
        &pair.standby_args,
        Some(receiver_config),
    );
    let want: Vec<i32> = (1..=during_id).collect();
    assert_eq!(settle(pair.pg_s, &want, Duration::from_secs(900)).await, Ok(want));
    assert!(resumed.log().contains("Resuming physical standby"));
    assert!(!resumed.log().contains("Bootstrapping physical standby"));
    let reader = connect(pair.pg_s).await;
    for id in [BASE / 2, BASE / 2 + 1, BASE / 2 + CHANGED, BASE / 2 + CHANGED + 1] {
        let value: String = reader
            .query_one("SELECT v FROM t WHERE id=$1", &[&id])
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            value,
            payload(
                id,
                if id > BASE / 2 && id <= BASE / 2 + CHANGED {
                    version
                } else {
                    0
                }
            )
        );
    }
    let digest = actual.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    let receipt = serde_json::json!({"dataset_rows":BASE,"payload_bytes_per_row":PAYLOAD,"logical_bytes":logical_bytes,
        "changed_rows":CHANGED,"changed_fraction":0.01,"retention_pruning_rows_added":0,"changed_row_versions":version,"mutation_locality":"one contiguous row-id interval",
        "killed_phase":"after-ingest:0","dirty_start_status":503,"route":report.route.as_str(),
        "from_sequence":before.1,"cut_sequence":report.to,"default_cf_digest":digest,
        "leaves_total":report.leaves_total,"leaves_differing":report.leaves_differing,
        "bytes_total":report.bytes_total,"bytes_shipped":report.bytes_shipped,
        "shipped_fraction":report.bytes_shipped as f64/report.bytes_total as f64,
        "acknowledged_during_recovery":during_id,"tail_resumed_without_bootstrap":true,
        "elapsed_seconds":report.duration.as_secs_f64()});
    eprintln!("P4_SCALE_CASE {receipt}");
}
