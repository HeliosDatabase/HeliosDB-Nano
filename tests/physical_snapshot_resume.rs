//! Spec P2.2 §2, §4: a bootstrap copy that is interrupted resumes.
//!
//! Real managed daemons (`tests/support/physical_pair.rs`). The primary is
//! loaded until its store holds several SST files (1 MiB memtables, no
//! compression, no compaction, so the files stay apart). A standby
//! bootstrapping from it is killed with SIGKILL at ~30 % and again at ~70 % of
//! the download — by the receiver's `internal-tests` hook
//! (`HELIOSDB_NANO_TEST_KILL_DOWNLOAD_AT`), at an exact byte count rather
//! than a timing race — and restarted each time:
//!
//! * each restart resumes the SAME snapshot (the primary's export lease) from
//!   the progress record the killed run left in its staging directory;
//! * the bytes a restart fetches are exactly what that record did not hold —
//!   never more than what was left at the kill, plus the record's lag inside
//!   the file in progress (at most `PARTIAL_RECORD_INTERVAL`) and one chunk;
//! * the installed standby holds exactly the primary's rows, its directory
//!   carries no progress record, and no staging sibling is left behind;
//! * with a 1 s lease, a standby killed mid-download that comes back after
//!   the lease expired gets a new snapshot and starts over, saying so.
//!
//! Red on the base of this change (b46cdd44): every download staged into a
//! new random sibling and started from zero; nothing recorded progress and
//! the export died with its session.

#![cfg(all(feature = "ha-tier1", feature = "internal-tests", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/physical_pair.rs"]
mod physical_pair;

use heliosdb_nano::replication::checkpoint_transfer::{PARTIAL_RECORD_INTERVAL, PHYSICAL_PARTIAL_FILE, STAGING_PREFIX};
use heliosdb_nano::replication::physical_receiver::KILL_DOWNLOAD_AT_ENV;
use physical_pair::*;
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// One chunk of the download (the default `max_chunk_bytes`).
const CHUNK: u64 = 1024 * 1024;

/// Small memtables, no compression and no compaction: many SST files of a
/// predictable size (the standby gets the same storage options).
const STORAGE: &str = "[storage]\ncompression = \"None\"\nrocksdb_write_buffer_size = 1048576\n\
                       rocksdb_level0_file_num_compaction_trigger = 1000\n";

/// Deterministic, incompressible text for row `id`.
fn payload(id: u32) -> String {
    use std::fmt::Write as _;
    let mut state = 0x9E37_79B9_7F4A_7C15_u64 ^ u64::from(id).wrapping_mul(0x2545_F491_4F6C_DD1D);
    let mut text = String::with_capacity(2048);
    while text.len() < 2048 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let _ = write!(text, "{state:016x}");
    }
    text
}

async fn load(client: &tokio_postgres::Client, ids: std::ops::Range<u32>) {
    let ids: Vec<u32> = ids.collect();
    for batch in ids.chunks(100) {
        let values: Vec<String> = batch.iter().map(|id| format!("({id}, '{}')", payload(*id))).collect();
        client
            .batch_execute(&format!("INSERT INTO t VALUES {}", values.join(", ")))
            .await
            .unwrap();
    }
}

/// SHA-256 over every `(id, v)` of `t` in id order, and the row count.
async fn table_digest(host_port: u16) -> ([u8; 32], usize) {
    let client = connect(host_port).await;
    let rows = client.query("SELECT id, v FROM t ORDER BY id", &[]).await.unwrap();
    let mut hash = Sha256::new();
    for row in &rows {
        let id: i32 = row.get(0);
        let v: String = row.get(1);
        hash.update(id.to_be_bytes());
        hash.update((v.len() as u64).to_be_bytes());
        hash.update(v.as_bytes());
    }
    (hash.finalize().into(), rows.len())
}

fn sst_bytes(dir: &Path) -> (usize, u64) {
    std::fs::read_dir(dir)
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| entry.path().extension().is_some_and(|extension| extension == "sst"))
        .fold((0, 0), |(count, bytes), entry| {
            (count + 1, bytes + entry.metadata().unwrap().len())
        })
}

/// The staging siblings next to the standby's data directory.
fn staging_dirs(parent: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(parent)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(STAGING_PREFIX))
        })
        .collect()
}

/// The progress record a killed download left: its snapshot and the bytes it holds.
fn recorded(parent: &Path) -> (String, u64) {
    let staging = staging_dirs(parent);
    assert_eq!(staging.len(), 1, "one staging directory: {staging:?}");
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(staging[0].join(PHYSICAL_PARTIAL_FILE)).unwrap()).unwrap();
    let held: u64 = record["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["bytes_done"].as_u64().unwrap())
        .sum();
    (record["snapshot_id"].as_str().unwrap().to_string(), held)
}

/// The numbers of the log line that contains `marker`, in order.
fn numbers_after(log: &str, marker: &str) -> Vec<u64> {
    let line = log
        .lines()
        .rev()
        .find(|line| line.contains(marker))
        .unwrap_or_else(|| panic!("no log line with {marker:?}"));
    let rest = &line[line.find(marker).unwrap() + marker.len()..];
    rest.split(|c: char| !c.is_ascii_digit())
        .filter(|token| !token.is_empty())
        .filter_map(|token| token.parse().ok())
        .collect()
}

/// Field order matters: the daemon stops before its directory is removed.
struct Primary {
    daemon: Daemon,
    pair: Pair,
}

/// A primary with several SST files and `rows` rows in `t`.
async fn loaded_primary(config: &str, rows: u32) -> Primary {
    let pair = pair();
    let daemon = Daemon::spawn(
        pair.dir.path(),
        "primary",
        &pair.dir.path().join("primary"),
        &pair.token,
        &pair.primary_args,
        Some(config),
    );
    let client = connect(pair.pg_p).await;
    client
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
        .await
        .unwrap();
    load(&client, 0..rows).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    while sst_bytes(&pair.dir.path().join("primary")).0 < 3 {
        assert!(Instant::now() < deadline, "the primary never flushed 3 SST files");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Primary { daemon, pair }
}

impl Primary {
    fn spawn_standby(&self, name: &str, config: &str, kill_at: Option<u64>) -> Daemon {
        Daemon::spawn_with(
            self.pair.dir.path(),
            name,
            &self.pair.dir.path().join("standby"),
            &self.pair.token,
            &self.pair.standby_args,
            Some(config),
            &SpawnOptions {
                listen: None,
                env: kill_at
                    .map(|bytes| (KILL_DOWNLOAD_AT_ENV, bytes.to_string()))
                    .into_iter()
                    .collect(),
            },
        )
    }
}

/// Wait for a standby that kills itself mid-download.
fn killed(standby: &mut Daemon) {
    let status = standby
        .wait_exit(Duration::from_secs(120))
        .unwrap_or_else(|| panic!("the standby was not killed: {}", standby.tail(12)));
    assert!(!status.success(), "{status:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_killed_bootstrap_resumes_the_same_snapshot_from_its_progress_record() {
    let primary = loaded_primary(STORAGE, 4000).await;
    let parent = primary.pair.dir.path().to_path_buf();
    let (files, sst) = sst_bytes(&parent.join("primary"));
    assert!(files >= 3, "{files} SST files");
    let kill_early = sst * 3 / 10;
    let kill_late = sst * 7 / 10;

    // ~30 %: killed; its staging directory keeps the progress record.
    let mut standby = primary.spawn_standby("standby-1", STORAGE, Some(kill_early));
    killed(&mut standby);
    assert!(standby.log().contains(KILL_DOWNLOAD_AT_ENV), "{}", standby.tail(12));
    assert!(!parent.join("standby").exists(), "nothing installed");
    let (snapshot, held_early) = recorded(&parent);
    assert!(held_early > 0, "the first completed files were recorded");
    assert!(
        held_early <= kill_early + CHUNK && held_early + PARTIAL_RECORD_INTERVAL + CHUNK >= kill_early,
        "the record lags the kill by at most one interval: {held_early} vs {kill_early}"
    );

    // ~70 %: the restart resumes the same snapshot from exactly those bytes.
    let mut standby = primary.spawn_standby("standby-2", STORAGE, Some(kill_late));
    killed(&mut standby);
    let log = standby.log();
    assert!(
        log.contains(&format!("resuming the download of snapshot {snapshot}")),
        "{log}"
    );
    let resumed = numbers_after(&log, &format!("resuming the download of snapshot {snapshot}: "));
    assert_eq!(resumed[0], held_early, "it holds what the record said: {resumed:?}");
    let total = resumed[1];
    let (again, held_late) = recorded(&parent);
    assert_eq!(again, snapshot, "the primary leased the same export");
    assert!(held_late > held_early && held_late + PARTIAL_RECORD_INTERVAL + CHUNK >= kill_late);

    // The last run completes, fetching only what the second did not hold.
    let standby = primary.spawn_standby("standby-3", STORAGE, None);
    wait_http(primary.pair.http_s, "/ready", 200, Duration::from_secs(120), &standby).await;
    let log = standby.log();
    assert_eq!(
        numbers_after(&log, &format!("resuming the download of snapshot {snapshot}: "))[0],
        held_late
    );
    let complete = numbers_after(&log, &format!("the download of snapshot {snapshot} is complete: "));
    let (fetched, resumed_bytes) = (complete[1], complete[2]);
    assert_eq!(complete[0], total);
    assert_eq!(fetched, total - held_late, "no byte the record held was fetched again");
    assert_eq!(resumed_bytes, held_late);
    assert!(
        fetched <= total - kill_late.min(total) + PARTIAL_RECORD_INTERVAL + CHUNK,
        "bytes after the restart: {fetched}; left at the kill: {}",
        total - kill_late.min(total)
    );
    assert!(fetched < total, "a resumed download is not a full copy");

    // The copy is exactly the primary's, with no extra file and no sibling.
    assert_eq!(
        table_digest(primary.pair.pg_s).await,
        table_digest(primary.pair.pg_p).await
    );
    assert!(!parent.join("standby").join(PHYSICAL_PARTIAL_FILE).exists());
    assert!(
        staging_dirs(&parent).is_empty(),
        "the staging directory became the standby"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_snapshot_after_the_lease_expired_starts_the_download_over() {
    let config = format!("{STORAGE}[replication]\nsnapshot_export_lease_secs = 1\n");
    let primary = loaded_primary(&config, 2500).await;
    let parent = primary.pair.dir.path().to_path_buf();
    let (_, sst) = sst_bytes(&parent.join("primary"));

    let mut standby = primary.spawn_standby("standby-1", STORAGE, Some(sst * 3 / 10));
    killed(&mut standby);
    let (snapshot, held) = recorded(&parent);
    assert!(held > 0);
    // The killed session ended; its export's 1 s lease runs out and is swept.
    tokio::time::sleep(Duration::from_secs(4)).await;

    let standby = primary.spawn_standby("standby-2", STORAGE, None);
    wait_http(primary.pair.http_s, "/ready", 200, Duration::from_secs(120), &standby).await;
    let log = standby.log();
    assert!(
        log.contains("the snapshot download is starting over")
            && log.contains(&format!("the partial download belongs to snapshot {snapshot}")),
        "{}",
        standby.tail(20)
    );
    assert!(
        !log.contains(&format!("snapshot {snapshot} is complete")),
        "the old snapshot was not installed"
    );
    let complete = numbers_after(&log, " is complete: ");
    assert_eq!(complete[1], complete[0], "a full copy: everything fetched again");
    assert_eq!(
        table_digest(primary.pair.pg_s).await,
        table_digest(primary.pair.pg_p).await
    );
    assert!(
        primary.daemon.log().contains("expired and were removed") || primary.daemon.log().contains("no longer leased")
    );
    assert!(staging_dirs(&parent).is_empty());
}
