//! Spec P3.1 §6: archived WAL segments and the recovery window, on real
//! managed daemons (`tests/support/physical_pair.rs`).
//!
//! * a loaded primary seals several segments whose frames cover its
//!   sequences contiguously; its standby archives what it applied, frame for
//!   frame the bytes the primary archived; `heliosdb_archived_wal` lists
//!   exactly the files; `/health` carries the `archive` object beside (never
//!   inside) the frozen `replication` block;
//! * a primary killed (SIGKILL) with an open segment repairs and seals it at
//!   its next start, and archives on with no gap;
//! * with `recovery_window = "2s"` old segments expire while the
//!   `heliosdb_recovery_window` row tracks the effective window, state `ok`;
//! * a tiny `max_bytes` prunes inside the window: one WARN, `shortened`,
//!   `pruned_inside_window_total > 0`, and every write of the load succeeds;
//! * the disk reserve prunes too (`internal-tests` builds: free space is
//!   injected through `HELIOSDB_NANO_TEST_ARCHIVE_FREE_BYTES`);
//! * an encrypted dataset's segments are sealed with the dataset key:
//!   `archive verify` passes without the key, `--deep` needs it.
//!
//! Red on the base of this change (f0ec2d58): nothing archived anything — no
//! segment, no view, no `archive` object, no `archive` command.

#![cfg(all(feature = "ha-tier1", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/physical_pair.rs"]
mod physical_pair;

use heliosdb_nano::replication::archive_catalog::{ArchiveCatalog, PartialPolicy};
use heliosdb_nano::replication::archive_segment::{
    encode_batch_payload, parse_partial_name, SegmentInfo, HEADER_LEN, PARTIAL_SUFFIX,
};
use physical_pair::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio_postgres::SimpleQueryMessage;

/// Sealed, available segments of an archive directory, in sequence order.
fn sealed(dir: &Path) -> Vec<SegmentInfo> {
    let Ok((catalog, _)) = ArchiveCatalog::scan(dir, PartialPolicy::Leave) else {
        return Vec::new();
    };
    let mut segments: Vec<SegmentInfo> = catalog.entries().filter_map(|entry| entry.info.clone()).collect();
    segments.sort_by_key(SegmentInfo::first_sequence);
    segments
}

fn assert_contiguous(segments: &[SegmentInfo]) {
    for pair in segments.windows(2) {
        assert_eq!(
            pair[0].last_sequence() + 1,
            pair[1].first_sequence(),
            "{} then {}",
            pair[0].path.display(),
            pair[1].path.display()
        );
    }
}

/// Every frame of `segments` as its wire payload, by first sequence.
fn frames(segments: &[SegmentInfo]) -> BTreeMap<u64, Vec<u8>> {
    let mut frames = BTreeMap::new();
    for segment in segments {
        for batch in segment.frames(None).unwrap() {
            let batch = batch.unwrap();
            frames.insert(batch.first_sequence(), encode_batch_payload(&batch));
        }
    }
    frames
}

/// Open segments under `dir`.
fn partials(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(partials(&path));
        } else if path.to_string_lossy().ends_with(PARTIAL_SUFFIX) {
            found.push(path);
        }
    }
    found
}

/// Wait until the archive's sealed segments reach `through`.
async fn wait_sealed_through(dir: &Path, through: u64, daemon: &Daemon) -> Vec<SegmentInfo> {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let segments = sealed(dir);
        if segments.last().is_some_and(|last| last.last_sequence() >= through) {
            return segments;
        }
        assert!(
            Instant::now() < deadline,
            "{} never sealed through {through}: {:?}; log tail: {}",
            dir.display(),
            segments.last().map(SegmentInfo::last_sequence),
            daemon.tail(12)
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The rows of `sql` (simple protocol), column name to text.
async fn rows(client: &tokio_postgres::Client, sql: &str) -> Vec<BTreeMap<String, Option<String>>> {
    client
        .simple_query(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .iter()
        .filter_map(|message| match message {
            SimpleQueryMessage::Row(row) => Some(
                row.columns()
                    .iter()
                    .enumerate()
                    .map(|(index, column)| (column.name().to_string(), row.get(index).map(str::to_string)))
                    .collect(),
            ),
            _ => None,
        })
        .collect()
}

async fn recovery_window(client: &tokio_postgres::Client) -> BTreeMap<String, Option<String>> {
    let mut window = rows(client, "SELECT * FROM heliosdb_recovery_window").await;
    assert_eq!(window.len(), 1);
    window.remove(0)
}

fn field<'a>(row: &'a BTreeMap<String, Option<String>>, name: &str) -> &'a str {
    row.get(name)
        .and_then(Option::as_deref)
        .unwrap_or_else(|| panic!("{name} is NULL or missing: {row:?}"))
}

fn number(row: &BTreeMap<String, Option<String>>, name: &str) -> u64 {
    field(row, name).parse().unwrap_or_else(|_| panic!("{name}: {row:?}"))
}

/// Insert `ids` one statement each; returns how many failed.
async fn insert(client: &tokio_postgres::Client, ids: std::ops::Range<i32>, filler: &str) -> usize {
    let mut failed = 0;
    for id in ids {
        if client
            .batch_execute(&format!("INSERT INTO t VALUES ({id}, '{filler}')"))
            .await
            .is_err()
        {
            failed += 1;
        }
    }
    failed
}

async fn primary_with(pair: &Pair, name: &str, config: &str) -> (Daemon, tokio_postgres::Client) {
    let dir = pair.dir.path();
    let primary = Daemon::spawn(
        dir,
        name,
        &dir.join("primary"),
        &pair.token,
        &pair.primary_args,
        Some(config),
    );
    let client = connect(pair.pg_p).await;
    (primary, client)
}

/// Spec P3.1 §6: segments under load, the standby's frames, the view, `/health`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_loaded_primary_seals_contiguous_segments_and_its_standby_archives_the_same_frames() {
    const SMALL: &str = "[replication.archive]\nsegment_max_bytes = \"16KiB\"\nsegment_max_secs = 1\n";
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let (primary, client) = primary_with(&pair, "primary", SMALL).await;
    client
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
        .await
        .unwrap();
    let standby = Daemon::spawn(
        &dir,
        "standby",
        &dir.join("standby"),
        &pair.token,
        &pair.standby_args,
        Some(SMALL),
    );
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(120), &standby).await;
    assert_eq!(insert(&client, 0..300, &"x".repeat(200)).await, 0);
    let want: Vec<i32> = (0..300).collect();
    assert_eq!(settle(pair.pg_s, &want, Duration::from_secs(60)).await, Ok(want));
    let through = primary_sequence(&client).await;

    let primary_segments = wait_sealed_through(&dir.join("primary.archive"), through, &primary).await;
    let standby_segments = wait_sealed_through(&dir.join("standby.archive"), through, &standby).await;
    assert!(primary_segments.len() >= 3, "{} segments", primary_segments.len());
    assert_contiguous(&primary_segments);
    assert_contiguous(&standby_segments);
    for segment in primary_segments.iter().chain(&standby_segments) {
        assert!(
            segment.file_bytes <= 16 * 1024 || segment.batch_count() == 1,
            "{segment:?}"
        );
        segment.verify_deep(None).unwrap();
    }
    // The standby archived what it applied: the primary's frames, byte for
    // byte (history, first sequence, digest, raw write group).
    let primary_frames = frames(&primary_segments);
    let standby_frames = frames(&standby_segments);
    assert!(standby_frames.len() >= 100, "{} standby frames", standby_frames.len());
    for (first, payload) in &standby_frames {
        assert_eq!(
            primary_frames.get(first),
            Some(payload),
            "the standby's frame at {first} is not the primary's"
        );
    }

    // `heliosdb_archived_wal` lists exactly the sealed files (retried while
    // a segment is sealed in between).
    let archive = dir.join("primary.archive");
    let files = || {
        sealed(&archive)
            .into_iter()
            .map(|segment| {
                (
                    segment.first_sequence(),
                    segment.last_sequence(),
                    segment.path.display().to_string(),
                )
            })
            .collect::<Vec<_>>()
    };
    let mut matched = None;
    for _ in 0..20 {
        let before = files();
        let mut listed: Vec<(u64, u64, String)> = rows(
            &client,
            "SELECT first_sequence, last_sequence, path, state, sha256 FROM heliosdb_archived_wal",
        )
        .await
        .iter()
        .map(|row| {
            assert_eq!(field(row, "state"), "available");
            assert_eq!(field(row, "sha256").len(), 64);
            (
                number(row, "first_sequence"),
                number(row, "last_sequence"),
                field(row, "path").to_string(),
            )
        })
        .collect();
        listed.sort();
        if before == files() && listed == before {
            matched = Some(listed);
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert!(
        matched.is_some(),
        "heliosdb_archived_wal never matched the files: {:?}",
        files()
    );
    assert!(rows(&client, "SELECT from_sequence FROM heliosdb_archive_gaps")
        .await
        .is_empty());
    let window = recovery_window(&client).await;
    assert_eq!(field(&window, "enabled"), "t");
    assert_eq!(field(&window, "state"), "ok");
    assert_eq!(number(&window, "configured_window_secs"), 7 * 86_400);

    // `/health`: the archive is a sibling of the frozen replication block.
    for port in [pair.http_p, pair.http_s] {
        let (code, body) = http(port, "/health").await.unwrap();
        assert_eq!(code, 200);
        assert_eq!(body["archive"]["enabled"], true, "{body}");
        assert!(body["archive"]["segments"].as_u64().unwrap() >= 1, "{body}");
        assert_eq!(body["archive"]["gaps"], 0, "{body}");
        assert!(body["replication"].get("archive").is_none(), "{body}");
    }
}

/// Spec P3.1 §6: SIGKILL mid-segment; the restart repairs and seals the open
/// segment and archives on, contiguously.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_killed_primary_repairs_its_open_segment_and_archives_on_without_a_gap() {
    const OPEN: &str = "[replication.archive]\nsegment_max_secs = 3600\n";
    const SEALING: &str = "[replication.archive]\nsegment_max_secs = 1\n";
    let pair = pair();
    let archive = pair.dir.path().join("primary.archive");
    let (mut primary, client) = primary_with(&pair, "primary", OPEN).await;
    client
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
        .await
        .unwrap();
    assert_eq!(insert(&client, 0..50, "before the kill").await, 0);
    let deadline = Instant::now() + Duration::from_secs(30);
    let partial = loop {
        if let Some(partial) = partials(&archive)
            .into_iter()
            .find(|path| std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > HEADER_LEN as u64 + 1024))
        {
            break partial;
        }
        assert!(
            Instant::now() < deadline,
            "no open segment with frames: {}",
            primary.tail(12)
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    // Let the archiver read the last inserts (it polls every second).
    tokio::time::sleep(Duration::from_millis(2500)).await;
    drop(client);
    primary.kill();
    assert!(partial.exists(), "SIGKILL leaves the open segment");
    let first = parse_partial_name(&partial.file_name().unwrap().to_string_lossy()).unwrap();

    let (primary, client) = primary_with(&pair, "primary-2", SEALING).await;
    assert!(
        primary.log_contains_within("archive: repaired the open segment", Duration::from_secs(30)),
        "{}",
        primary.tail(12)
    );
    assert!(!partial.exists(), "the open segment was repaired and sealed");
    let repaired = sealed(&archive)
        .into_iter()
        .find(|segment| segment.first_sequence() == first)
        .expect("a sealed segment starts where the open one did");
    assert!(repaired.batch_count() >= 50, "{repaired:?}");
    repaired.verify_deep(None).unwrap();

    assert_eq!(insert(&client, 50..100, "after the restart").await, 0);
    let through = primary_sequence(&client).await;
    let segments = wait_sealed_through(&archive, through, &primary).await;
    assert_contiguous(&segments);
    assert!(rows(&client, "SELECT from_sequence FROM heliosdb_archive_gaps")
        .await
        .is_empty());
    assert!(!primary.log().contains("archive gap"), "{}", primary.tail(20));
}

/// Spec P3.1 §6: a 2-second window under a steady load: old segments expire,
/// the effective window stays near the configured one, state `ok`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expiring_window_prunes_old_segments_and_tracks_the_effective_window() {
    const EXPIRING: &str =
        "[replication.archive]\nsegment_max_secs = 1\nrecovery_window = \"2s\"\nprune_interval_secs = 1\n";
    let pair = pair();
    let archive = pair.dir.path().join("primary.archive");
    let (primary, client) = primary_with(&pair, "primary", EXPIRING).await;
    client
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
        .await
        .unwrap();
    let writer = connect(pair.pg_p).await;
    let load = tokio::spawn(async move {
        let started = Instant::now();
        let (mut id, mut failed) = (0, 0);
        while started.elapsed() < Duration::from_secs(14) {
            failed += insert(&writer, id..id + 1, "steady").await;
            id += 1;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        failed
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let oldest = loop {
        if let Some(first) = sealed(&archive).first() {
            break first.path.clone();
        }
        assert!(Instant::now() < deadline, "nothing sealed: {}", primary.tail(12));
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    tokio::time::sleep(Duration::from_secs(6)).await;
    let window = recovery_window(&client).await;
    assert_eq!(field(&window, "state"), "ok", "{window:?}");
    assert_eq!(field(&window, "shortened"), "f");
    assert_eq!(number(&window, "configured_window_secs"), 2);
    assert_eq!(number(&window, "pruned_inside_window_total"), 0);
    let effective = number(&window, "effective_window_secs");
    assert!(
        (1..=8).contains(&effective),
        "the effective window follows the configured 2 s: {window:?}"
    );
    assert!(!oldest.exists(), "the first segment expired");
    assert_eq!(load.await.unwrap(), 0, "every write of the load succeeded");
    assert!(!primary.log().contains("recovery window shortened"));
}

/// Spec P3.1 §4, §6: `max_bytes` far below the load prunes inside the window
/// — one WARN for the episode — and never holds a write back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tiny_max_bytes_shortens_the_window_once_and_never_slows_writes() {
    const CAPPED: &str = "[replication.archive]\nsegment_max_bytes = \"4KiB\"\nsegment_max_secs = 1\n\
                          max_bytes = \"8KiB\"\nprune_interval_secs = 1\n";
    let pair = pair();
    let (primary, client) = primary_with(&pair, "primary", CAPPED).await;
    client
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
        .await
        .unwrap();
    let started = Instant::now();
    assert_eq!(insert(&client, 0..400, &"y".repeat(100)).await, 0, "no write failed");
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(60), "400 inserts took {elapsed:?}");
    let deadline = Instant::now() + Duration::from_secs(30);
    let window = loop {
        let window = recovery_window(&client).await;
        if field(&window, "shortened") == "t" {
            break window;
        }
        assert!(
            Instant::now() < deadline,
            "never shortened: {window:?}; {}",
            primary.tail(12)
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    };
    assert_eq!(field(&window, "state"), "shortened");
    assert!(number(&window, "pruned_inside_window_total") > 0, "{window:?}");
    assert_eq!(number(&window, "max_bytes"), 8192);
    let (_, health) = http(pair.http_p, "/health").await.unwrap();
    assert_eq!(health["archive"]["shortened"], true, "{health}");
    // More prune passes, the same episode: still one WARN.
    assert_eq!(insert(&client, 400..450, "more").await, 0);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        primary.log().matches("recovery window shortened to").count(),
        1,
        "{}",
        primary.tail(20)
    );
}

/// Spec P3.1 §4, §6: free space below the disk reserve prunes inside the
/// window (free space injected: `internal-tests` builds only).
#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn free_space_below_the_disk_reserve_prunes_inside_the_window() {
    use heliosdb_nano::replication::archive_catalog::TEST_FREE_BYTES_ENV;
    const RESERVED: &str = "[replication.archive]\nsegment_max_bytes = \"4KiB\"\nsegment_max_secs = 1\n\
                            prune_interval_secs = 1\ndisk_reserve = \"1MiB\"\n";
    let pair = pair();
    let dir = pair.dir.path();
    let primary = Daemon::spawn_with(
        dir,
        "primary",
        &dir.join("primary"),
        &pair.token,
        &pair.primary_args,
        Some(RESERVED),
        &SpawnOptions {
            listen: None,
            env: vec![(TEST_FREE_BYTES_ENV, "0".to_string())],
        },
    );
    let client = connect(pair.pg_p).await;
    client
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
        .await
        .unwrap();
    assert_eq!(insert(&client, 0..200, &"z".repeat(100)).await, 0);
    let deadline = Instant::now() + Duration::from_secs(30);
    let window = loop {
        let window = recovery_window(&client).await;
        if field(&window, "shortened") == "t" {
            break window;
        }
        assert!(
            Instant::now() < deadline,
            "never shortened: {window:?}; {}",
            primary.tail(12)
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    };
    assert_eq!(number(&window, "free_bytes"), 0);
    assert_eq!(number(&window, "disk_reserve_bytes"), 1024 * 1024);
    assert!(number(&window, "pruned_inside_window_total") > 0);
    assert!(
        primary.log_contains_within("below the disk reserve", Duration::from_secs(5)),
        "{}",
        primary.tail(20)
    );
}

/// Spec P3.1 §2, §6: an encrypted dataset's frames are sealed with the dataset
/// key; `archive verify` checks header and footer without it, `--deep` needs
/// it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_encrypted_dataset_seals_its_segments_and_deep_verify_needs_the_key() {
    use std::os::unix::fs::PermissionsExt;
    let pair = pair();
    let dir = pair.dir.path();
    let key = dir.join("dataset.key");
    std::fs::write(&key, "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let config = format!(
        "[encryption]\nenabled = true\nkey_source = {{ File = \"{}\" }}\n[replication.archive]\nsegment_max_secs = 1\n",
        key.display()
    );
    let (primary, client) = primary_with(&pair, "primary", &config).await;
    client
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
        .await
        .unwrap();
    assert_eq!(insert(&client, 0..20, "secret").await, 0);
    let through = primary_sequence(&client).await;
    let archive = dir.join("primary.archive");
    let segments = wait_sealed_through(&archive, through, &primary).await;
    for segment in &segments {
        assert!(segment.sealed_frames(), "{segment:?}");
        assert_ne!(segment.header.key_check, [0; 32]);
        assert!(segment.frames(None).is_err());
    }
    let manager =
        heliosdb_nano::crypto::KeyManager::from_source(&heliosdb_nano::config::KeySource::File(key.clone())).unwrap();
    for segment in &segments {
        segment.verify_deep(Some(&manager)).unwrap();
    }

    let cli = |args: &[String]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"))
            .arg("archive")
            .arg("verify")
            .arg("--archive-dir")
            .arg(&archive)
            .args(args)
            .output()
            .unwrap()
    };
    let shallow = cli(&[]);
    assert!(shallow.status.success(), "{}", String::from_utf8_lossy(&shallow.stdout));
    let keyless = cli(&["--deep".to_string()]);
    assert_eq!(keyless.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&keyless.stdout).contains("dataset key required"),
        "{}",
        String::from_utf8_lossy(&keyless.stdout)
    );
    let keyed = cli(&[
        "--deep".to_string(),
        "--key-file".to_string(),
        key.display().to_string(),
    ]);
    assert!(keyed.status.success(), "{}", String::from_utf8_lossy(&keyed.stdout));
}
