//! Torn WAL Segment Recovery Tests
//!
//! `WalStore::init()` against a real segment left behind by a primary that was killed
//! mid-write. A torn trailing record is the normal, expected state of a write-ahead
//! log after any unclean shutdown, so a WAL reader that cannot survive its own torn
//! tail has the failure mode inverted: it is strictest exactly when recovery matters
//! most. See `docs/plans/ROADMAP_V5.md` §2.8.
//!
//! Counterpart to `wal_crash_recovery_tests.rs`, which covers the other (logical,
//! RocksDB-backed) WAL in `src/storage/wal.rs`.
//!
//! ```bash
//! cargo test --test wal_segment_recovery_tests
//! ```

#![cfg(feature = "ha-tier1")]

use heliosdb_nano::replication::wal_store::{WalStore, WalStoreConfig};
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;

/// The genuine article: `data/wal/segment_000032.wal` exactly as a SIGTERMed run left
/// it. Header reports `segment_id = 32`, `start_lsn = 1`, `entry_count = 0` — only
/// `close_segment` ever backpatches that count, so a segment killed before close
/// always reports zero. Records for LSN 1-3 verify against CRC-32; LSN 4-7 are
/// structurally intact but their payloads no longer hash to the checksums their
/// headers carry; past those, a run of `0x78` filler parses as a record header
/// claiming 2,021,161,080 bytes at LSN 8,680,820,740,569,200,760.
const TORN_SEGMENT: &[u8] = include_bytes!("fixtures/wal_segments/segment_000032_torn.wal");

/// Wall-clock deadline for `init()`. Generous against an expected sub-second run,
/// tight against "hangs forever".
const INIT_DEADLINE: Duration = Duration::from_secs(10);

/// Recovery keeps the checksum-verified prefix and stops at the first failure.
///
/// **Exactly three records, not seven.** LSN 4-7 look well-formed — right length,
/// sequential LSN, intact header — but their payloads do not hash to the checksums
/// their headers promise. A scan cannot tell payload corruption (length honest,
/// stream still aligned) from a lying length (stream now misaligned) after the fact,
/// so the first mismatch ends the scan and everything after it is discarded. An
/// assertion of 7 here would mean the reader had gone back to skipping past checksum
/// failures, which is the bug (ROADMAP_V5 §2.8).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_torn_segment_recovers_exactly_three_valid_records() {
    let dir = tempdir().expect("tempdir");
    std::fs::write(dir.path().join("segment_000032.wal"), TORN_SEGMENT).expect("write fixture");

    let config = WalStoreConfig {
        // Always explicit. `WalStoreConfig::default()`'s `wal_dir` is the CWD-relative
        // `./data/wal` (ROADMAP_V5 §2.7, still open), which would make this test read
        // and write the repository's own data directory.
        wal_dir: dir.path().to_path_buf(),
        fsync_on_write: false,
        ..Default::default()
    };
    let store = Arc::new(WalStore::new(config));

    // `init()` runs as its own task on a multi-threaded runtime deliberately. The
    // regression this deadline guards against is a synchronous loop inside a single
    // `poll()` — an `lsn_index` backfill across ~8.7e18 LSNs, fed by a garbage
    // `end_lsn` — which never yields, so a timer future sharing its thread would never
    // be polled and the test would hang right along with it. This bug hung the
    // repository's test suite for two days; a reintroduction must fail loudly instead.
    let init_store = Arc::clone(&store);
    let init = tokio::spawn(async move { init_store.init().await });
    tokio::time::timeout(INIT_DEADLINE, init)
        .await
        .expect("WalStore::init() exceeded its deadline - the torn-segment scan is unbounded again")
        .expect("WalStore::init() task panicked")
        .expect("WalStore::init() failed");

    // Every valid payload in this segment is `entry_` followed by 1000 'a' bytes.
    let mut expected = b"entry_".to_vec();
    expected.resize(1006, b'a');

    for lsn in 1..=3u64 {
        let entry = store
            .get(lsn)
            .await
            .expect("LSN 1-3 pass checksum and must be recovered");
        assert_eq!(entry.lsn, lsn);
        assert_eq!(entry.data, expected);
        assert_eq!(entry.checksum, crc32fast::hash(&entry.data));
    }

    // LSN 4-7 fail checksum. LSN 8 never existed at all: the trailing filler merely
    // parses as a header claiming it.
    for lsn in 4..=8u64 {
        assert!(store.get(lsn).await.is_none(), "LSN {} must not be recovered", lsn);
    }

    assert_eq!(
        store.current_lsn(),
        3,
        "the store must not advertise an LSN it cannot serve"
    );

    let segments = store.list_segments().await;
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].segment_id, 32);
    assert_eq!(segments[0].start_lsn, 1);
    assert_eq!(
        segments[0].end_lsn, 3,
        "end_lsn must be a validated LSN, not a parsed one"
    );
    assert_eq!(segments[0].entry_count, 3);

    // The torn file is preserved, not truncated: it is the only forensic record of how
    // the tear happened, and this artifact exists precisely because one was kept.
    let on_disk = std::fs::read(dir.path().join("segment_000032.wal")).expect("segment still readable");
    assert_eq!(
        on_disk.len(),
        TORN_SEGMENT.len(),
        "recovery must not rewrite the segment"
    );
}
