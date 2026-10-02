//! Draft strict storage hydration regressions; requires strict-storage-draft.patch.
//! Closed private databases only. These tests do not prove live publication.
use heliosdb_nano::{storage::StorageEngine, Config};
use rocksdb::{Options, DB};
use tempfile::TempDir;

fn config() -> Config {
    let mut config = Config::default();
    config.storage.wal_enabled = false;
    config.storage.elide_latest_version = false;
    config
}

fn initialized() -> (TempDir, Config) {
    let dir = TempDir::new().unwrap();
    let cfg = config();
    let storage = StorageEngine::open(dir.path(), &cfg).unwrap();
    drop(storage);
    (dir, cfg)
}

#[test]
fn strict_snapshot_does_not_replay_wal_or_install_elision_or_write_on_close() {
    let (dir, mut cfg) = initialized();
    let raw = DB::open(&Options::default(), dir.path()).unwrap();
    raw.put(
        b"wal:entries:00000000000000000001",
        b"deliberately invalid logical intent",
    )
    .unwrap();
    raw.put(b"physical-test", b"authoritative physical bytes").unwrap();
    let cut = raw.latest_sequence_number();
    drop(raw);
    cfg.storage.wal_enabled = true;
    cfg.storage.elide_latest_version = true;
    let storage = StorageEngine::open_physical_snapshot(dir.path(), &cfg).unwrap();
    assert!(!storage.index_snapshots_on_close());
    assert!(!storage.row_counter_flush_on_close());
    assert!(!storage.wal_checkpoint_on_close());
    assert_eq!(storage.db().latest_sequence_number(), cut);
    assert_eq!(
        storage.db().get(b"physical-test").unwrap().as_deref(),
        Some(b"authoritative physical bytes".as_slice())
    );
    assert!(storage.db().get(b"w3_2_elide_used").unwrap().is_none());
    assert!(storage.db().put(b"cannot-write", b"x").is_err());
    drop(storage);
    let raw = DB::open_for_read_only(&Options::default(), dir.path(), false).unwrap();
    assert_eq!(raw.latest_sequence_number(), cut);
    assert!(raw.get(b"wal:entries:00000000000000000001").unwrap().is_some());
    assert!(raw.get(b"cannot-write").unwrap().is_none());
}

#[test]
fn malformed_visibility_and_counter_records_refuse_strict_hydration() {
    let records: &[(&[u8], &[u8])] = &[
        (b"snapshot:1", b"broken"),
        (b"vmeta:t:00000000000000000009:00000000000000000001", &[0; 8]),
        (b"vgc:low_watermark", b"short"),
        (b"w3_2_elide_used", &[2]),
        (b"counter:\xff", &[0; 8]),
    ];
    for (key, value) in records {
        let (dir, cfg) = initialized();
        let raw = DB::open(&Options::default(), dir.path()).unwrap();
        raw.put(key, value).unwrap();
        let cut = raw.latest_sequence_number();
        drop(raw);
        assert!(
            StorageEngine::open_physical_snapshot(dir.path(), &cfg).is_err(),
            "accepted corrupt key {key:?}"
        );
        let raw = DB::open_for_read_only(&Options::default(), dir.path(), false).unwrap();
        assert_eq!(raw.latest_sequence_number(), cut);
        assert_eq!(raw.get(key).unwrap().as_deref(), Some(*value));
    }
}

#[test]
fn strict_metadata_scan_rejects_lossy_names() {
    let (dir, cfg) = initialized();
    let raw = DB::open(&Options::default(), dir.path()).unwrap();
    raw.put(b"meta:test:\xff", b"value").unwrap();
    drop(raw);
    let storage = StorageEngine::open_physical_snapshot(dir.path(), &cfg).unwrap();
    assert!(storage.meta_blobs_with_prefix_strict("meta:test:").is_err());
}

#[test]
fn strict_branch_loader_never_creates_missing_registry() {
    let dir = TempDir::new().unwrap();
    let mut options = Options::default();
    options.create_if_missing(true);
    let raw = DB::open(&options, dir.path()).unwrap();
    assert_eq!(raw.latest_sequence_number(), 0);
    drop(raw);
    assert!(StorageEngine::open_physical_snapshot(dir.path(), &config()).is_err());
    let raw = DB::open_for_read_only(&Options::default(), dir.path(), false).unwrap();
    assert_eq!(raw.latest_sequence_number(), 0);
}

#[test]
fn ordinary_open_refuses_physical_receiver_cursor_before_logical_recovery() {
    let (dir, cfg) = initialized();
    let raw = DB::open(&Options::default(), dir.path()).unwrap();
    raw.put(b"__nano_physical_replica_cursor_v1", b"reserved receiver metadata")
        .unwrap();
    let cut = raw.latest_sequence_number();
    drop(raw);
    assert!(StorageEngine::open(dir.path(), &cfg).is_err());
    assert!(StorageEngine::open_read_only(dir.path(), &cfg).is_err());
    let raw = DB::open_for_read_only(&Options::default(), dir.path(), false).unwrap();
    assert_eq!(raw.latest_sequence_number(), cut);
}

#[test]
fn physical_snapshot_requires_persistent_existing_storage() {
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("absent");
    assert!(StorageEngine::open_physical_snapshot(&missing, &config()).is_err());
    assert!(!missing.exists());
    assert!(StorageEngine::open_physical_snapshot(dir.path(), &Config::in_memory()).is_err());
}

#[test]
fn strict_open_on_an_existing_empty_directory_is_refused_without_mutation() {
    // An existing-but-invalid (empty) checkpoint directory must be refused AND
    // left untouched: strict hydration never initializes storage or writes files.
    let dir = TempDir::new().unwrap();
    let empty = dir.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    fn names(p: &std::path::Path) -> Vec<std::ffi::OsString> {
        let mut v: Vec<_> = std::fs::read_dir(p).unwrap().map(|e| e.unwrap().file_name()).collect();
        v.sort();
        v
    }
    let before = names(&empty);
    assert!(StorageEngine::open_physical_snapshot(&empty, &config()).is_err());
    let after = names(&empty);
    assert!(after.is_empty(), "strict open created files in an empty dir: {after:?}");
    assert_eq!(before, after, "strict open mutated the directory");
}

#[cfg(unix)]
#[test]
fn physical_marker_symlink_refuses_before_creating_database_files() {
    let dir = TempDir::new().unwrap();
    std::os::unix::fs::symlink("missing", dir.path().join("NANO-PHYSICAL-REPLICA.json")).unwrap();
    assert!(StorageEngine::open(dir.path(), &config()).is_err());
    assert!(StorageEngine::open_read_only(dir.path(), &config()).is_err());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    assert!(!dir.path().join("CURRENT").exists());
}
