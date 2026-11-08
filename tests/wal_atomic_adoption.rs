//! Current groups never redo; ambiguous legacy recovery needs explicit offline adoption.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{Config, EmbeddedDatabase, StorageEngine, Value};
use rocksdb::{IteratorMode, Options, ReadOptions, DB};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Output};

const FORMAT: &[u8] = b"wal:atomic_groups";
const CHECKPOINT: &[u8] = b"wal:checkpoint";
const FLOOR: &[u8] = b"wal:pruned_through";
const LAST: &[u8] = b"wal:last_lsn";
fn config(path: &Path) -> Config {
    let mut c = Config::default();
    c.storage.path = Some(path.to_owned());
    c.storage.memory_only = false;
    c.storage.wal_enabled = true;
    c.storage.logical_wal_per_statement = true;
    c.storage.wal_checkpoint_on_close = false;
    c.storage.wal_checkpoint_interval_entries = 0;
    c.storage.wal_checkpoint_interval_secs = 0;
    c.replication.logical_wal_retention_entries = 1000;
    c.replication.physical.enabled = true;
    c
}
fn open(c: Config) -> heliosdb_nano::Result<EmbeddedDatabase> {
    let mut last = None;
    for _ in 0..100 {
        match EmbeddedDatabase::with_config(c.clone()) {
            Ok(db) => return Ok(db),
            Err(e) if e.to_string().to_ascii_lowercase().contains("lock") => last = Some(e),
            Err(e) => return Err(e),
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Err(last.unwrap())
}
fn raw(path: &Path) -> DB {
    let mut options = Options::default();
    options.create_if_missing(false);
    options.set_prefix_extractor(rocksdb::SliceTransform::create_fixed_prefix(5));
    for _ in 0..100 {
        match DB::open(&options, path) {
            Ok(db) => return db,
            Err(e) if e.to_string().to_ascii_lowercase().contains("lock") => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => panic!("raw open: {e}"),
        }
    }
    panic!("raw DB lock did not release");
}
fn fixture(path: &Path) -> Config {
    let c = config(path);
    let db = open(c.clone()).unwrap();
    assert_eq!(db.storage.db().get(FORMAT).unwrap().as_deref(), Some([1_u8].as_slice()));
    db.execute("CREATE TABLE kept (id INT PRIMARY KEY, body TEXT)").unwrap();
    db.execute("INSERT INTO kept VALUES (1, 'old')").unwrap();
    db.execute("UPDATE kept SET body = 'new' WHERE id = 1").unwrap();
    db.execute("CREATE BRANCH retained_branch AS OF NOW").unwrap();
    db.execute("USE BRANCH retained_branch").unwrap();
    db.execute("INSERT INTO kept VALUES (2, 'branch')").unwrap();
    db.execute("USE BRANCH main").unwrap();
    drop(db);
    c
}
fn legacy(path: &Path, checkpoint: Option<u64>) {
    let db = raw(path);
    db.delete(FORMAT).unwrap();
    db.delete(FLOOR).unwrap();
    match checkpoint {
        Some(value) => db.put(CHECKPOINT, value.to_le_bytes()).unwrap(),
        None => db.delete(CHECKPOINT).unwrap(),
    }
    assert!(
        all(&db).keys().any(|key| key.starts_with(b"wal:entries:")),
        "nonempty retained history is required"
    );
}
fn all(db: &DB) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut read = ReadOptions::default();
    read.set_total_order_seek(true);
    db.iterator_opt(IteratorMode::Start, read)
        .map(|item| {
            let (key, value) = item.unwrap();
            (key.into_vec(), value.into_vec())
        })
        .collect()
}
fn payload(mut keys: BTreeMap<Vec<u8>, Vec<u8>>) -> BTreeMap<Vec<u8>, Vec<u8>> {
    for key in [FORMAT, CHECKPOINT, FLOOR, LAST] {
        keys.remove(key);
    }
    keys
}
fn cli(path: &Path, explicit: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"));
    command.args(["recover-wal", "--data-dir"]).arg(path);
    if explicit {
        command.arg("--adopt-local-state");
    }
    command.output().unwrap()
}
fn main_rows(db: &EmbeddedDatabase) {
    let rows = db.query("SELECT body FROM kept ORDER BY id", &[]).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values, vec![Value::String("new".into())]);
}
#[test]
fn explicit_cli_adoption_preserves_all_data_catalog_branch_history_and_retained_entries() {
    let temp = tempfile::tempdir().unwrap();
    let c = fixture(temp.path());
    legacy(temp.path(), Some(0));
    let (before, sequence) = {
        let db = raw(temp.path());
        (all(&db), db.latest_sequence_number())
    };
    let refused = open(c.clone()).err().expect("legacy redo must refuse writable startup");
    assert!(refused.to_string().contains("recover-wal"), "{refused}");
    let readonly = EmbeddedDatabase::open_read_only(temp.path()).unwrap();
    main_rows(&readonly);
    drop(readonly);
    let output = cli(temp.path(), true);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    {
        let db = raw(temp.path());
        let after = all(&db);
        assert_eq!(
            payload(after.clone()),
            payload(before),
            "only four recovery metadata keys may change"
        );
        assert_eq!(after.get(FORMAT), Some(&vec![1]));
        assert_eq!(after.get(FLOOR), Some(&0_u64.to_le_bytes().to_vec()));
        assert_eq!(db.latest_sequence_number(), sequence + 4);
        // Closing the CLI's raw DB may flush/delete its native WAL files.
        // Post-close data/sequence checks stay here; the same adopt() writer's
        // one-batch proof runs while its handle is live in wal_recovery::tests::
        // legacy_without_checkpoint_adopts_once_in_one_metadata_group.
    }
    let db = open(c).unwrap();
    main_rows(&db);
    db.execute("USE BRANCH retained_branch").unwrap();
    assert_eq!(db.query("SELECT id FROM kept", &[]).unwrap().len(), 2);
}

#[test]
fn unsafe_legacy_refusal_also_applies_when_logical_wal_is_disabled() {
    let temp = tempfile::tempdir().unwrap();
    let mut c = fixture(temp.path());
    legacy(temp.path(), Some(0));
    c.storage.wal_enabled = false;
    let before = {
        let db = raw(temp.path());
        all(&db)
    };
    let error = open(c).err().expect("disabling WAL cannot bypass migration");
    assert!(error.to_string().contains("legacy WAL"), "{error}");
    assert_eq!(all(&raw(temp.path())), before, "refusal precedes open-time backfills");
}

#[test]
fn current_format_and_uncheckpointed_legacy_open_without_replaying_local_state() {
    for current in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let c = fixture(temp.path());
        if !current {
            legacy(temp.path(), None);
        }
        let db = open(c).expect("safe current writer/legacy-history case must open successfully");
        main_rows(&db);
        assert_eq!(db.storage.db().get(FORMAT).unwrap().as_deref(), Some([1_u8].as_slice()));
    }
}

#[test]
fn unknown_or_corrupt_format_refuses_normal_readonly_and_offline_migration() {
    for marker in [vec![], vec![2], vec![1, 0]] {
        let temp = tempfile::tempdir().unwrap();
        let c = fixture(temp.path());
        {
            raw(temp.path()).put(FORMAT, &marker).unwrap();
        }
        let before = { all(&raw(temp.path())) };
        assert!(open(c).is_err());
        assert!(EmbeddedDatabase::open_read_only(temp.path()).is_err());
        let output = cli(temp.path(), true);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("format marker"));
        assert_eq!(all(&raw(temp.path())), before);
    }
}

#[test]
fn explicit_cli_requires_a_stopped_existing_directory_and_the_adoption_flag() {
    let temp = tempfile::tempdir().unwrap();
    let missing = temp.path().join("missing");
    assert!(!cli(&missing, true).status.success());
    assert!(!missing.exists());
    assert!(!cli(&missing, false).status.success());
    assert!(!missing.exists());
    let dir = temp.path().join("db");
    let c = fixture(&dir);
    let db = open(c).unwrap();
    let sequence = db.storage.db().latest_sequence_number();
    let output = cli(&dir, true);
    assert!(!output.status.success(), "a live database must retain its lock");
    assert_eq!(db.storage.db().latest_sequence_number(), sequence);
    main_rows(&db);
}

#[test]
fn explicit_adoption_refuses_physical_and_fenced_roles() {
    for file in ["NANO-PHYSICAL-REPLICA.json", "NANO-PROMOTION.json", "NANO-FENCED.json"] {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path());
        std::fs::write(temp.path().join(file), b"role marker").unwrap();
        let before = { all(&raw(temp.path())) };
        assert!(StorageEngine::adopt_local_wal_state(temp.path()).is_err());
        assert_eq!(all(&raw(temp.path())), before);
    }
}

#[test]
fn explicit_adoption_refuses_a_standbys_local_column_family() {
    let temp = tempfile::tempdir().unwrap();
    let mut options = Options::default();
    options.create_if_missing(true);
    options.create_missing_column_families(true);
    let names = ["default", heliosdb_nano::storage::keyspace::LOCAL_CF];
    let db = DB::open_cf(&options, temp.path(), names).unwrap();
    db.put(b"preserve-role", b"standby bytes").unwrap();
    let sequence = db.latest_sequence_number();
    drop(db);
    let output = cli(temp.path(), true);
    assert!(!output.status.success());
    let db = DB::open_cf(&options, temp.path(), names).unwrap();
    assert_eq!(db.latest_sequence_number(), sequence);
    assert_eq!(
        db.get(b"preserve-role").unwrap().as_deref(),
        Some(b"standby bytes".as_slice())
    );
    assert!(db.get(FORMAT).unwrap().is_none());
}

#[cfg(all(feature = "internal-tests", feature = "encryption"))]
#[test]
fn engine_second_key_check_failure_cannot_publish_wal_adoption() {
    const REPLACEMENT_DIR: &str = "NANO_TEST_SECOND_KEY_CHECK_REPLACEMENT_DIR";
    const KEY_FILE: &str = "NANO_TEST_SECOND_KEY_CHECK_KEY_FILE";
    let encrypted_config = |path: &Path, key: &Path| {
        let mut c = config(path);
        c.encryption.enabled = true;
        c.encryption.key_source = heliosdb_nano::KeySource::File(key.to_owned());
        c
    };
    if let Some(path) = std::env::var_os(REPLACEMENT_DIR) {
        let key = std::env::var_os(KEY_FILE).expect("child key file");
        // The configured key passes the read-only preflight. Only the actual
        // writable-handle verification substitutes a wrong key in this child.
        let error = open(encrypted_config(Path::new(&path), Path::new(&key)))
            .err()
            .expect("the second key check must refuse the substituted key");
        assert!(error.to_string().contains("does not match this database"), "{error}");
        println!("ENGINE_SECOND_KEY_CHECK_REFUSED_BEFORE_ADOPTION");
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("database");
    let key = temp.path().join("right.key");
    std::fs::write(&key, [0x7a_u8; 32]).unwrap();
    let c = encrypted_config(&path, &key);
    let db = open(c.clone()).unwrap();
    assert!(db.storage.is_encrypted());
    db.execute("CREATE TABLE kept (id INT PRIMARY KEY, body TEXT)").unwrap();
    db.execute("INSERT INTO kept VALUES (1, 'new')").unwrap();
    drop(db);
    // Missing checkpoint is eligible for adoption, so moving adoption ahead of
    // the second key check would publish its four metadata puts before failure.
    legacy(&path, None);
    let (before, sequence) = {
        let db = raw(&path);
        (all(&db), db.latest_sequence_number())
    };
    assert!(!before.contains_key(FORMAT));
    assert!(!before.contains_key(CHECKPOINT));
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "engine_second_key_check_failure_cannot_publish_wal_adoption",
            "--nocapture",
        ])
        .env(REPLACEMENT_DIR, &path)
        .env(KEY_FILE, &key)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ENGINE_SECOND_KEY_CHECK_REFUSED_BEFORE_ADOPTION"),
        "the exact child test must execute its real Engine-open refusal assertion"
    );
    {
        let db = raw(&path);
        assert_eq!(
            all(&db),
            before,
            "failed actual Engine open must publish no WAL metadata"
        );
        assert_eq!(db.latest_sequence_number(), sequence);
    }
    let db = open(c).expect("the original encryption key still opens the eligible store");
    main_rows(&db);
    assert_eq!(db.storage.db().get(FORMAT).unwrap().as_deref(), Some([1_u8].as_slice()));
}
