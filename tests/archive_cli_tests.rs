//! Spec P3.1 §5, §6: `heliosdb-nano archive list | verify | gaps | prune`,
//! offline, on a prepared archive directory.
//!
//! The directory is written with the library's own segment writer from a
//! real RocksDB store's write groups (`poll_source`), then damaged the way an
//! operator would find it:
//!
//! * one segment with a flipped header byte (corrupt: `header hash`);
//! * one segment of ANOTHER history filed under this history's directory
//!   (corrupt: `other history`);
//! * one segment removed (a hole: `no segment holds it`);
//! * in a second directory, one segment with a flipped body byte: the cheap
//!   `verify` passes it, `verify --deep` names it (`file hash`).
//!
//! Every refusal names the file; `verify` exits 1 at the first failure (or
//! lists all with `--keep-going`); `prune --dry-run` changes nothing; a real
//! `prune` is refused while a running node archives into the directory.
//!
//! Red on the base of this change (f0ec2d58): there was no `archive`
//! subcommand, no segment format and no archiver.

#![cfg(all(feature = "ha-tier1", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/physical_pair.rs"]
mod physical_pair;

use heliosdb_nano::replication::archive_segment::{segment_dir, SegmentInfo, HEADER_LEN};
use heliosdb_nano::replication::archiver::{SegmentLimits, SegmentWriter};
use heliosdb_nano::replication::physical::{poll_source, FlushMemo, PhysicalBatch, MAX_BATCH_BYTES};
use physical_pair::*;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;
use uuid::Uuid;

/// `count` one-put write groups of a fresh store, as an archiver sees them.
fn groups(root: &Path, name: &str, count: u32) -> (Uuid, Vec<PhysicalBatch>) {
    let mut options = rocksdb::Options::default();
    options.create_if_missing(true);
    options.set_wal_ttl_seconds(3600);
    let store = rocksdb::DB::open(&options, root.join(name)).unwrap();
    for index in 0..count {
        store.put(format!("data:t:{index:04}"), [7_u8; 48]).unwrap();
    }
    let history = Uuid::new_v4();
    let polled = poll_source(&store, history, 0, MAX_BATCH_BYTES, &FlushMemo::new()).unwrap();
    assert_eq!(polled.batches.len(), count as usize);
    (history, polled.batches)
}

/// Archive `batches` under `root`, sealing a segment every `per` groups.
fn archive(root: &Path, history: Uuid, batches: &[PhysicalBatch], per: usize) -> Vec<SegmentInfo> {
    let limits = SegmentLimits {
        max_bytes: 1 << 20,
        max_secs: 3600,
        fsync_bytes: 4096,
    };
    let mut writer = SegmentWriter::new(root, history, 1, Uuid::new_v4(), None, [0; 32], limits, None);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let mut sealed = Vec::new();
    for chunk in batches.chunks(per) {
        for batch in chunk {
            sealed.extend(writer.append(batch, now).unwrap());
        }
        sealed.extend(writer.seal().unwrap());
    }
    sealed
}

fn flip(path: &Path, offset: usize) {
    let mut bytes = std::fs::read(path).unwrap();
    bytes[offset] ^= 0x41;
    std::fs::write(path, bytes).unwrap();
}

fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"))
        .arg("archive")
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

/// The prepared archive: history A in 3-group segments 1..3, 4..6 (header
/// flipped), 7..9 (removed), 10..12; a segment of history B filed under A.
struct Prepared {
    _root: tempfile::TempDir,
    dir: PathBuf,
    flipped: PathBuf,
    misfiled: PathBuf,
    removed: PathBuf,
}

fn prepared() -> Prepared {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("archive");
    let (history, batches) = groups(root.path(), "a", 12);
    let sealed = archive(&dir, history, &batches, 3);
    assert_eq!(sealed.len(), 4);
    let flipped = sealed[1].path.clone();
    flip(&flipped, 40);
    let removed = sealed[2].path.clone();
    std::fs::remove_file(&removed).unwrap();
    let (other, theirs) = groups(root.path(), "b", 2);
    let stray = archive(&root.path().join("elsewhere"), other, &theirs, 2);
    let misfiled = segment_dir(&dir, history, 1).join(stray[0].path.file_name().unwrap());
    assert!(!misfiled.exists());
    std::fs::rename(&stray[0].path, &misfiled).unwrap();
    Prepared {
        _root: root,
        dir,
        flipped,
        misfiled,
        removed,
    }
}

#[test]
fn list_names_every_segment_and_every_refusal() {
    let archive = prepared();
    let output = cli(&["list", "--archive-dir", text(&archive.dir).as_str()]);
    assert!(output.status.success(), "{}", stderr(&output));
    let listed = stdout(&output);
    assert_eq!(
        listed.lines().filter(|line| line.starts_with("available")).count(),
        2,
        "{listed}"
    );
    assert_eq!(
        listed.lines().filter(|line| line.starts_with("corrupt")).count(),
        2,
        "{listed}"
    );
    let flipped = listed
        .lines()
        .find(|line| line.contains(&text(&archive.flipped)))
        .unwrap();
    assert!(
        flipped.starts_with("corrupt") && flipped.contains("header hash"),
        "{flipped}"
    );
    let misfiled = listed
        .lines()
        .find(|line| line.contains(&text(&archive.misfiled)))
        .unwrap();
    assert!(misfiled.contains("other history"), "{misfiled}");
    assert!(!listed.contains(&text(&archive.removed)));

    let output = cli(&["list", "--archive-dir", text(&archive.dir).as_str(), "--json"]);
    assert!(output.status.success());
    let rows: Vec<serde_json::Value> = stdout(&output)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 4);
    let available: Vec<&serde_json::Value> = rows.iter().filter(|row| row["state"] == "available").collect();
    assert_eq!(available.len(), 2);
    for row in available {
        assert_eq!(row["batches"], 3);
        assert_eq!(row["sha256"].as_str().unwrap().len(), 64);
    }
}

#[test]
fn verify_stops_at_the_first_failure_and_keep_going_names_them_all() {
    let archive = prepared();
    let output = cli(&["verify", "--archive-dir", text(&archive.dir).as_str()]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    let report = stdout(&output);
    let failures: Vec<&str> = report.lines().filter(|line| line.starts_with("FAILED")).collect();
    assert_eq!(failures.len(), 1, "{report}");
    assert!(
        failures[0].contains("segment ") && failures[0].contains(".hseg"),
        "{report}"
    );
    assert!(report.contains("--keep-going"), "{report}");

    let output = cli(&["verify", "--archive-dir", text(&archive.dir).as_str(), "--keep-going"]);
    assert_eq!(output.status.code(), Some(1));
    let report = stdout(&output);
    let failures: Vec<&str> = report.lines().filter(|line| line.starts_with("FAILED")).collect();
    assert_eq!(failures.len(), 2, "{report}");
    assert!(failures
        .iter()
        .any(|line| line.contains(&text(&archive.flipped)) && line.contains("header hash")));
    assert!(failures
        .iter()
        .any(|line| line.contains(&text(&archive.misfiled)) && line.contains("other history")));
    assert!(report.contains("4 segment(s) verified"), "{report}");
}

/// A flipped body byte passes the cheap check (header and footer verify)
/// and fails `--deep`, which re-hashes the file.
#[test]
fn only_a_deep_verify_reads_the_body() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("archive");
    let (history, batches) = groups(root.path(), "a", 4);
    let sealed = archive(&dir, history, &batches, 4);
    flip(&sealed[0].path, HEADER_LEN + 20);
    let output = cli(&["verify", "--archive-dir", text(&dir).as_str()]);
    assert!(output.status.success(), "{}", stdout(&output));
    let output = cli(&["verify", "--deep", "--archive-dir", text(&dir).as_str()]);
    assert_eq!(output.status.code(), Some(1));
    let report = stdout(&output);
    assert!(
        report.contains(&text(&sealed[0].path)) && report.contains("file hash"),
        "{report}"
    );
}

#[test]
fn gaps_name_the_hole_and_the_corrupt_interval() {
    let archive = prepared();
    let output = cli(&["gaps", "--archive-dir", text(&archive.dir).as_str()]);
    assert!(output.status.success(), "{}", stderr(&output));
    let report = stdout(&output);
    let hole = report
        .lines()
        .find(|line| line.contains(" 7..9 "))
        .unwrap_or_else(|| panic!("{report}"));
    assert!(hole.contains("no segment holds it"), "{hole}");
    let corrupt = report
        .lines()
        .find(|line| line.contains(" 4..6 "))
        .unwrap_or_else(|| panic!("{report}"));
    assert!(corrupt.contains("corrupt"), "{corrupt}");
    assert!(report.contains("2 gap(s)"), "{report}");
    // The misfiled segment's interval is held by an available segment: no gap.
    assert!(!report.contains(" 1..2 "), "{report}");

    let output = cli(&["gaps", "--archive-dir", text(&archive.dir).as_str(), "--from", "10"]);
    assert!(stdout(&output).contains("0 gap(s)"), "{}", stdout(&output));
}

#[test]
fn prune_dry_run_changes_nothing() {
    let archive = prepared();
    let before: Vec<PathBuf> = walk(&archive.dir);
    let output = cli(&[
        "prune",
        "--dry-run",
        "--archive-dir",
        text(&archive.dir).as_str(),
        "--max-bytes",
        "1",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let report = stdout(&output);
    // Two available segments: the older would go, the newest never does;
    // corrupt files are never pruned.
    assert_eq!(
        report.lines().filter(|line| line.starts_with("would prune")).count(),
        1,
        "{report}"
    );
    assert!(report.contains("over max_bytes"), "{report}");
    assert_eq!(walk(&archive.dir), before, "a dry run removes nothing");
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files.sort();
    files
}

/// Spec P3.1 §5: a real prune is refused while a running node archives into
/// the directory (its archiver holds the directory's lock).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prune_is_refused_while_a_node_archives_into_the_directory() {
    let pair = pair();
    let data = pair.dir.path().join("primary");
    let primary = Daemon::spawn(pair.dir.path(), "primary", &data, &pair.token, &pair.primary_args, None);
    let _client = connect(pair.pg_p).await;
    assert!(
        primary.log_contains_within("archive: primary archiving history", Duration::from_secs(30)),
        "{}",
        primary.tail(12)
    );
    let output = cli(&["prune", "--data-dir", text(&data).as_str()]);
    assert!(!output.status.success(), "{}", stdout(&output));
    let refusal = stderr(&output);
    assert!(refusal.contains("archive prune refused"), "{refusal}");
    assert!(refusal.contains("primary.archive"), "names the directory: {refusal}");
    // A dry run takes no lock: it previews under a running node.
    let output = cli(&["prune", "--dry-run", "--data-dir", text(&data).as_str()]);
    assert!(output.status.success(), "{}", stderr(&output));
}

// ---------------------------------------------------------------------------
// `heliosdb-nano recover` (spec P3.2 §2, §5): argument validation, the
// refusals before anything is opened, the running-node refusal. Its exit
// codes on real segments (0, 2 with the missing range, 1 on a corrupt one)
// are in `tests/physical_archive_recovery.rs`.
// ---------------------------------------------------------------------------

fn recover_cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"))
        .arg("recover")
        .args(args)
        .env("RUST_LOG", "warn")
        .output()
        .unwrap()
}

#[test]
fn recover_validates_its_arguments_and_refuses_what_is_not_a_standby() {
    let root = tempfile::tempdir().unwrap();
    let segments = root.path().join("segments");
    std::fs::create_dir_all(&segments).unwrap();
    let segments = text(&segments);

    // --from-archive is required: a usage error (clap's exit code 2).
    let output = recover_cli(&["--data-dir", text(&root.path().join("standby")).as_str()]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("--from-archive"), "{}", stderr(&output));

    // --then names what follows.
    let empty = root.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let empty = text(&empty);
    let output = recover_cli(&["--data-dir", &empty, "--from-archive", &segments, "--then", "later"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("--then must be primary or stop"),
        "{}",
        stderr(&output)
    );
    let output = recover_cli(&["--data-dir", &empty, "--from-archive", &segments, "--then", "primary"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("needs --primary-host"), "{}", stderr(&output));
    let output = recover_cli(&[
        "--data-dir",
        &empty,
        "--from-archive",
        &segments,
        "--",
        "--port",
        "5555",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("for --then primary"), "{}", stderr(&output));

    // An empty or absent data directory: segments carry write groups, not a
    // base; nothing is created.
    for data in [empty.clone(), text(&root.path().join("absent"))] {
        let output = recover_cli(&["--data-dir", &data, "--from-archive", &segments]);
        assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
        assert!(
            stderr(&output).contains("a standby starts from a snapshot of the primary"),
            "{}",
            stderr(&output)
        );
    }
    assert!(!root.path().join("absent").exists());

    // A primary's directory is never recovered over.
    let primary = root.path().join("primary");
    std::fs::create_dir_all(&primary).unwrap();
    std::fs::write(
        primary.join(heliosdb_nano::replication::history::PRIMARY_HISTORY_FILE),
        "{}",
    )
    .unwrap();
    let output = recover_cli(&["--data-dir", text(&primary).as_str(), "--from-archive", &segments]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("primary's dataset identity"),
        "{}",
        stderr(&output)
    );
}

/// Spec P3.2 §2: `recover` works on a STOPPED standby; a running one holds
/// its directory and is named.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recover_is_refused_while_a_node_runs_on_the_directory() {
    let pair = pair();
    let dir = pair.dir.path();
    let _primary = Daemon::spawn(
        dir,
        "primary",
        &dir.join("primary"),
        &pair.token,
        &pair.primary_args,
        None,
    );
    let _ = connect(pair.pg_p).await;
    let standby_data = dir.join("standby");
    let mut standby = Daemon::spawn(dir, "standby", &standby_data, &pair.token, &pair.standby_args, None);
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(90), &standby).await;
    let segments = dir.join("segments");
    std::fs::create_dir_all(&segments).unwrap();
    let output = recover_cli(&[
        "--data-dir",
        text(&standby_data).as_str(),
        "--from-archive",
        text(&segments).as_str(),
    ]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    let refusal = stderr(&output);
    assert!(refusal.contains("in use by a running node"), "{refusal}");
    assert!(standby.alive(), "the running standby is untouched");
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(30), &standby).await;
}

// ---------------------------------------------------------------------------
// `heliosdb-nano recover --from-primary` (spec P4.1 §1, §5): the source group,
// the flags that belong to it, their bounds, and the refusals before any
// connection. The routes and their exit codes (0; 1; 3 when a full copy is
// needed but not applied) on real daemons are in
// `tests/physical_fast_forward.rs`.
// ---------------------------------------------------------------------------

/// Clap's usage errors (exit 2) for the source group and the flags of
/// `--from-primary`; nothing is created.
#[test]
fn recover_from_primary_validates_its_arguments() {
    let root = tempfile::tempdir().unwrap();
    let data = text(&root.path().join("standby"));
    let segments = root.path().join("segments");
    std::fs::create_dir_all(&segments).unwrap();
    let segments = text(&segments);
    let primary = ["--from-primary", "--primary-host", "127.0.0.1:1"];
    let with_primary = |extra: &[&'static str]| -> Vec<String> {
        let mut argv = vec!["--data-dir".to_string(), data.clone()];
        argv.extend(primary.iter().map(|arg| arg.to_string()));
        argv.extend(extra.iter().map(|arg| arg.to_string()));
        argv
    };
    let with_archive = |extra: &[&'static str]| -> Vec<String> {
        let mut argv = vec![
            "--data-dir".to_string(),
            data.clone(),
            "--from-archive".to_string(),
            segments.clone(),
        ];
        argv.extend(extra.iter().map(|arg| arg.to_string()));
        argv
    };
    for (argv, names) in [
        // One source, exactly.
        (with_archive(&primary), "--from-primary"),
        // --from-primary needs the primary.
        (
            vec!["--data-dir".to_string(), data.clone(), "--from-primary".to_string()],
            "--primary-host",
        ),
        // --through belongs to --from-archive.
        (with_primary(&["--through", "7"]), "--through"),
        // The ladder's flags belong to --from-primary.
        (with_archive(&["--full"]), "--from-primary"),
        (with_archive(&["--no-full-copy"]), "--from-primary"),
        (with_archive(&["--replace"]), "--from-primary"),
        (with_archive(&["--stage-dir", "/nonexistent/stage"]), "--from-primary"),
        // --full and --no-full-copy contradict each other.
        (with_primary(&["--full", "--no-full-copy"]), "--no-full-copy"),
        // Bounds.
        (with_primary(&["--leaf-bytes", "1KiB"]), "64KiB"),
        (with_primary(&["--leaf-bytes", "512MiB"]), "256MiB"),
        (with_primary(&["--full-copy-threshold", "0"]), "--full-copy-threshold"),
        (with_primary(&["--full-copy-threshold", "101"]), "--full-copy-threshold"),
    ] {
        let args: Vec<&str> = argv.iter().map(String::as_str).collect();
        let output = recover_cli(&args);
        assert_eq!(output.status.code(), Some(2), "{argv:?}: {}", stderr(&output));
        assert!(stderr(&output).contains(names), "{argv:?}: {}", stderr(&output));
    }
    assert!(!root.path().join("standby").exists(), "nothing was created");
}

/// The refusals before anything is written (exit 1): a primary's directory,
/// a stage directory inside the data directory, no token, and a primary that
/// cannot be reached — named, with a fresh directory left absent.
#[test]
fn recover_from_primary_refuses_before_writing_anything() {
    let root = tempfile::tempdir().unwrap();
    let token = text(&token_file(root.path(), "replication.token", 0x42));
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let host = closed.local_addr().unwrap().to_string();
    drop(closed);

    // A primary's directory is never recovered over, whatever the source.
    let primary = root.path().join("primary");
    std::fs::create_dir_all(&primary).unwrap();
    std::fs::write(
        primary.join(heliosdb_nano::replication::history::PRIMARY_HISTORY_FILE),
        "{}",
    )
    .unwrap();
    let primary = text(&primary);
    let output = recover_cli(&[
        "--data-dir",
        &primary,
        "--from-primary",
        "--primary-host",
        &host,
        "--replication-token-file",
        &token,
        "--then",
        "stop",
    ]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("primary's dataset identity"),
        "{}",
        stderr(&output)
    );

    // The stage directory never lies inside the data directory.
    let absent = root.path().join("absent");
    let absent_text = text(&absent);
    let inside = text(&absent.join("stage"));
    let output = recover_cli(&[
        "--data-dir",
        &absent_text,
        "--from-primary",
        "--primary-host",
        &host,
        "--replication-token-file",
        &token,
        "--stage-dir",
        &inside,
        "--then",
        "stop",
    ]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("inside the data directory"),
        "{}",
        stderr(&output)
    );

    // No token: refused before any connection.
    let output = recover_cli(&[
        "--data-dir",
        &absent_text,
        "--from-primary",
        "--primary-host",
        &host,
        "--then",
        "stop",
    ]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("--replication-token-file"),
        "{}",
        stderr(&output)
    );

    // Nothing listens there: named, and nothing is created.
    let output = recover_cli(&[
        "--data-dir",
        &absent_text,
        "--from-primary",
        "--primary-host",
        &host,
        "--replication-token-file",
        &token,
        "--then",
        "stop",
    ]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(stderr(&output).contains(&host), "{}", stderr(&output));
    assert!(!absent.exists(), "a fresh directory stays absent");
}
