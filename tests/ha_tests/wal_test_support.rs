//! Shared support for the HA WAL suites: a per-test, self-cleaning `wal_dir`.
//!
//! # Why this exists
//!
//! `WalStoreConfig::default()` sets `wal_dir: PathBuf::from("./data/wal")` — a path
//! resolved against the *process's* working directory. For the server that is the
//! intended behaviour (a `data/wal` under the daemon's own directory) and it is
//! deliberately left alone. For tests it means something else entirely: every
//! `#[tokio::test]` in this suite runs in the same process, with the same CWD (the repo
//! root), so every store built from the default shared ONE directory.
//!
//! `WalStore::init()` scans that directory and reads the header of every `*.wal` file it
//! finds. Run in parallel, one test's `init()` could observe another test's segment
//! *mid-write* and derive `start_lsn > end_lsn` from it, which panics inside
//! `BTreeMap::range` with `range start is greater than range end`. Worse, the residue
//! outlived the run: a second `cargo test` saw the first one's segments, so results
//! depended on what was left on disk. That is why `--skip ha_tests::streaming_tests` is
//! a documented gate skip.
//!
//! Handing each test its own empty temp directory removes the sharing, which is the
//! whole race — nothing else about `WalStore` had to change.

#![allow(dead_code)]

use heliosdb_nano::replication::wal_store::WalStoreConfig;
use tempfile::TempDir;

/// A `WalStoreConfig` identical to `default()` except that `wal_dir` points at a private,
/// empty temporary directory.
///
/// The returned [`TempDir`] OWNS that directory and deletes it when dropped, so the
/// caller must keep it alive for as long as the store is used:
///
/// ```ignore
/// let (_wal_tmp, config) = wal_test_support::isolated_wal_config();
/// let store = WalStore::new(config);
/// // `_wal_tmp` stays in scope until the end of the test — do NOT bind it to `_`,
/// // which would drop it immediately and delete the directory out from under `store`.
/// ```
///
/// To vary another field, bind the config `mut` and set it: the directory is the only
/// thing this helper is opinionated about.
pub fn isolated_wal_config() -> (TempDir, WalStoreConfig) {
    let dir = tempfile::tempdir().expect("create temp dir for WAL store");
    let config = WalStoreConfig {
        wal_dir: dir.path().to_path_buf(),
        ..Default::default()
    };
    (dir, config)
}
