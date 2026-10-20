//! Spec P0.3: a physical standby keeps its node-local state in its own column
//! family, so its default column family is a verbatim copy of the primary's,
//! and every RocksDB open of a Nano store uses one set of options.
//!
//! # Why
//!
//! Every later phase (serving, archive, fast-forward) rests on two facts: a
//! standby at source sequence `T` holds exactly the primary's keyspace at `T`,
//! and the standby reads that keyspace with the primary's RocksDB options.
//! Before P0.3 neither held: the receiver appended its cursor to every applied
//! batch IN the default column family, so no standby was ever byte-identical
//! to its primary, and it opened RocksDB with `Options::default()` — no prefix
//! extractor, bloom filter, block cache or compression of the engine's.
//!
//! # What
//!
//! 1. Byte identity: the SHA-256 range digest of the entire default column
//!    family on a standby caught up to `T` equals the primary's at `T`, after
//!    a corpus of real SQL writes. Red before P0.3 (the cursor key).
//! 2. Owned writes keep the cursor invariant
//!    `latest_sequence_number == cursor.local_sequence`, never change the
//!    copied keyspace, survive close/reopen, and a raw write around the writer
//!    poisons it.
//! 3. A staging directory with the pre-P0.3 layout (cursor in the default
//!    family) is refused with a typed full-resync error, untouched.
//! 4. Every engine open mode refuses a staged directory, untouched.
//! 5. The receiver writes the same `OPTIONS-*` as the engine.
#![cfg(all(feature = "ha-tier1", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::replication::checkpoint_transfer::{
    OnlineCheckpointExport, PhysicalStage, ReceiverStaging, ReopenedPhysicalStage, TransferError, TransferLimits,
};
use heliosdb_nano::replication::history::load_or_create_primary_history;
use heliosdb_nano::replication::physical::{
    poll_source, range_digest, LocalBatch, PhysicalError, StagedPhysicalReplica, CURSOR_KEY, LOCAL_CF, MAX_BATCH_BYTES,
};
use heliosdb_nano::storage::StorageEngine;
use heliosdb_nano::{Config, EmbeddedDatabase};
use rocksdb::{checkpoint::Checkpoint, Options, DB};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

/// Where a receiver kept its cursor before the node-local namespace.
const LEGACY_CURSOR_KEY: &[u8] = b"__nano_physical_replica_cursor_v1";

fn config(path: &Path) -> Config {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    // The primary keeps archived RocksDB WAL, so a stage can continue from its
    // checkpoint cut.
    config.replication.physical.enabled = true;
    config
}

/// Open `path` read-only with the engine's options and every column family.
fn open_read_only(path: &Path, config: &Config) -> DB {
    let (options, families) = StorageEngine::build_db_options(config, path).unwrap();
    DB::open_cf_descriptors_read_only(&options, path, families, false).unwrap()
}

fn has_local_family(path: &Path) -> bool {
    DB::list_cf(&Options::default(), path)
        .unwrap()
        .iter()
        .any(|family| family == LOCAL_CF)
}

/// A plain-RocksDB primary with retained WAL and its history marker.
fn raw_primary(root: &Path) -> (Arc<DB>, Uuid) {
    let mut options = Options::default();
    options.create_if_missing(true);
    options.set_wal_ttl_seconds(3600);
    let db = Arc::new(DB::open(&options, root.join("primary")).unwrap());
    db.put(b"seed", b"original").unwrap();
    let history = load_or_create_primary_history(db.path()).unwrap();
    (db, history)
}

/// Export a checkpoint of the live primary `source`, download it into `root`
/// and open it as a stage: the production path up to the first cursor.
fn stage(source: &DB, history: Uuid, root: &Path, target: &str, config: &Config) -> PhysicalStage {
    let limits = TransferLimits::default();
    let export = OnlineCheckpointExport::create(source, history, root, limits).unwrap();
    let mut receiver =
        ReceiverStaging::begin(&root.join(target), &export.manifest_bytes().unwrap(), history, limits).unwrap();
    for (index, file) in export.manifest().files.iter().enumerate() {
        let mut offset = 0;
        while offset < file.size {
            let bytes = export.read_chunk(index as u32, offset, 64 * 1024).unwrap();
            receiver.append_chunk(index as u32, offset, &bytes).unwrap();
            offset += bytes.len() as u64;
        }
    }
    receiver.finish_download(config).unwrap().open(config).unwrap()
}

/// Apply source batches until the stage's cursor is exactly `through`.
fn catch_up(stage: &mut PhysicalStage, source: &DB, history: Uuid, through: u64) -> usize {
    let mut applied = 0;
    'poll: while stage.cursor().source_sequence() < through {
        let poll = poll_source(source, history, stage.cursor().source_sequence(), MAX_BATCH_BYTES).unwrap();
        assert!(!poll.batches.is_empty(), "the source has nothing up to {through}");
        for batch in &poll.batches {
            assert!(
                batch.last_sequence() <= through,
                "source batch {}..={} crosses the cut {through}",
                batch.first_sequence(),
                batch.last_sequence()
            );
            assert!(stage.apply(batch).unwrap());
            applied += 1;
            if batch.last_sequence() == through {
                break 'poll;
            }
        }
    }
    assert_eq!(stage.cursor().source_sequence(), through);
    applied
}

/// Every entry of `dir`: a file's SHA-256, or a marker for anything else.
fn census(dir: &Path) -> BTreeMap<String, String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            let content = if kind.is_file() {
                format!("{:x}", Sha256::digest(fs::read(entry.path()).unwrap()))
            } else {
                format!("{kind:?}")
            };
            (entry.file_name().to_string_lossy().into_owned(), content)
        })
        .collect()
}

#[test]
fn a_standby_default_family_is_byte_identical_to_its_primary_at_the_same_sequence() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(&root.path().join("primary"));
    let primary = EmbeddedDatabase::with_config(cfg.clone()).unwrap();
    for sql in [
        "CREATE TABLE items (id INT PRIMARY KEY, name TEXT, qty INT)",
        "INSERT INTO items VALUES (1, 'one', 10), (2, 'two', 20), (3, 'three', 30)",
        "CREATE INDEX items_qty ON items (qty)",
    ] {
        primary.execute(sql).unwrap();
    }
    let raw = primary.storage.db();
    let history = load_or_create_primary_history(raw.path()).unwrap();
    let mut standby = stage(&raw, history, root.path(), "standby", &cfg);
    let snapshot_cut = standby.cursor().source_sequence();

    // Row, schema, index, transaction and sequence writes after the cut.
    for sql in [
        "INSERT INTO items VALUES (4, 'four', 40), (5, 'five', 50)",
        "UPDATE items SET qty = qty + 1 WHERE id <= 2",
        "DELETE FROM items WHERE id = 3",
        "CREATE TABLE notes (id INT PRIMARY KEY, body TEXT)",
        "INSERT INTO notes VALUES (1, 'kept'), (2, 'deleted')",
        "ALTER TABLE items ADD COLUMN tag TEXT",
        "BEGIN",
        "INSERT INTO notes VALUES (3, 'in a transaction')",
        "DELETE FROM notes WHERE id = 2",
        "COMMIT",
        "CREATE SEQUENCE ticket START 7",
    ] {
        primary.execute(sql).unwrap();
    }
    primary.query("SELECT nextval('ticket')", &[]).unwrap();

    // The primary at T: a checkpoint is a consistent cut on a batch boundary.
    let at_t_path = root.path().join("primary-at-t");
    Checkpoint::new(&raw).unwrap().create_checkpoint(&at_t_path).unwrap();
    let at_t = open_read_only(&at_t_path, &cfg);
    let t = at_t.latest_sequence_number();
    assert!(t > snapshot_cut, "the corpus wrote nothing after the checkpoint");
    let primary_digest = range_digest(&at_t, None, b"").unwrap();

    assert!(catch_up(&mut standby, &raw, history, t) > 1);
    let writer = Arc::clone(standby.writer());
    assert_eq!(
        range_digest(writer.db(), None, b"").unwrap(),
        primary_digest,
        "the standby's default column family differs from the primary's at source sequence {t}"
    );
    assert_eq!(writer.db().latest_sequence_number(), writer.cursor().local_sequence());
    // The cursor is node-local and nowhere in the copied keyspace.
    let local = writer.db().cf_handle(LOCAL_CF).unwrap();
    assert!(writer.db().get_cf(local, CURSOR_KEY).unwrap().is_some());
    assert!(writer.db().get(LEGACY_CURSOR_KEY).unwrap().is_none());
    drop(writer);

    // And durably so: the installed directory still agrees once closed.
    let installed = standby.publish().unwrap();
    assert_eq!(installed.cursor.source_sequence(), t);
    let reopened = open_read_only(&installed.path, &cfg);
    assert_eq!(range_digest(&reopened, None, b"").unwrap(), primary_digest);
    assert_eq!(reopened.latest_sequence_number(), installed.cursor.local_sequence());
}

#[test]
fn owned_writes_keep_the_cursor_invariant_and_never_touch_the_copied_keyspace() {
    let root = tempfile::tempdir().unwrap();
    let (source, history) = raw_primary(root.path());
    let cfg = config(&root.path().join("standby"));
    let standby = stage(&source, history, root.path(), "standby", &cfg);
    let writer = Arc::clone(standby.writer());
    let accounted = |step: &str| {
        assert_eq!(
            writer.db().latest_sequence_number(),
            writer.cursor().local_sequence(),
            "after {step}"
        );
    };
    let copied = || range_digest(writer.db(), None, b"").unwrap();
    accounted("the initial cursor");
    let initial = copied();

    for n in 0..3 {
        let before = writer.cursor().local_sequence();
        let mut batch = LocalBatch::new();
        batch.put(format!("posture-{n}").as_bytes(), b"streaming");
        batch.delete(b"absent");
        writer.write_local(batch).unwrap();
        assert_eq!(
            writer.cursor().local_sequence(),
            before + 3,
            "two operations and the cursor"
        );
        accounted("write_local");
        assert_eq!(copied(), initial, "a node-local write changed the copied keyspace");
    }

    source.put(b"after-checkpoint", b"source write").unwrap();
    let poll = poll_source(&source, history, writer.cursor().source_sequence(), MAX_BATCH_BYTES).unwrap();
    assert_eq!(poll.batches.len(), 1);
    assert!(writer.apply_source(&poll.batches[0]).unwrap());
    accounted("apply_source");
    let after_source = copied();
    assert_ne!(after_source, initial, "apply_source must change the copied keyspace");

    let mut batch = LocalBatch::new();
    batch.put(b"stats", b"1");
    writer.write_local(batch).unwrap();
    accounted("write_local after apply_source");
    assert_eq!(copied(), after_source);
    let cursor = writer.cursor();
    drop(writer);

    // Close and reopen: the cursor is found in the node-local family again.
    let closed = standby.close().unwrap();
    assert_eq!(closed.cursor, cursor);
    let reopened = ReopenedPhysicalStage::open(&closed.path, history, TransferLimits::default(), &cfg).unwrap();
    assert_eq!(reopened.cursor(), cursor);
    let writer = Arc::clone(reopened.writer());
    assert_eq!(writer.db().latest_sequence_number(), cursor.local_sequence());
    let local = writer.db().cf_handle(LOCAL_CF).unwrap();
    assert_eq!(
        writer.db().get_cf(local, b"stats").unwrap().as_deref(),
        Some(b"1".as_slice())
    );

    // A raw default-family write between two steps poisons the next one.
    writer.db().put(b"stray", b"not through the writer").unwrap();
    source.put(b"after-stray", b"source write").unwrap();
    let poll = poll_source(&source, history, cursor.source_sequence(), MAX_BATCH_BYTES).unwrap();
    let error = writer.apply_source(&poll.batches[0]).unwrap_err();
    assert!(
        error.to_string().contains("modified outside physical replication"),
        "{error}"
    );
    assert!(writer.is_poisoned());
    assert!(writer.db().get(b"after-stray").unwrap().is_none());
    let mut batch = LocalBatch::new();
    batch.put(b"stats", b"2");
    assert!(matches!(writer.write_local(batch), Err(PhysicalError::Poisoned)));
    drop(writer);
    assert!(
        reopened.close().is_err(),
        "a poisoned stage must not close as consistent"
    );
}

#[test]
fn a_staging_directory_with_its_cursor_in_the_default_family_is_refused_untouched() {
    let root = tempfile::tempdir().unwrap();
    let (source, history) = raw_primary(root.path());
    let cfg = config(&root.path().join("legacy"));
    let installed = stage(&source, history, root.path(), "legacy", &cfg).publish().unwrap();
    let path = installed.path.clone();
    // The pre-P0.3 layout: no node-local family, and the cursor — accounting
    // for its own write — in the default family under the old key.
    {
        let mut db = DB::open_cf(&Options::default(), &path, ["default", LOCAL_CF]).unwrap();
        db.drop_cf(LOCAL_CF).unwrap();
        let local_sequence = db.latest_sequence_number() + 1;
        let mut cursor = serde_json::to_value(&installed.cursor).unwrap();
        cursor["local_sequence"] = serde_json::json!(local_sequence);
        db.put(LEGACY_CURSOR_KEY, serde_json::to_vec(&cursor).unwrap()).unwrap();
        assert_eq!(db.latest_sequence_number(), local_sequence);
    }
    assert!(!has_local_family(&path));
    let stored = |path: &Path| {
        let db = DB::open_for_read_only(&Options::default(), path, false).unwrap();
        (db.latest_sequence_number(), range_digest(&db, None, b"").unwrap())
    };
    let before_store = stored(&path);
    let before_files = census(&path);

    match ReopenedPhysicalStage::open(&path, history, TransferLimits::default(), &cfg) {
        Err(TransferError::Physical(PhysicalError::FullResyncRequired(reason))) => {
            assert!(reason.contains("predates the node-local namespace"), "{reason}");
        }
        Err(other) => panic!("a legacy staging directory must require a full resync, not: {other}"),
        Ok(_) => panic!("a legacy staging directory was reopened"),
    }
    // Refused before any RocksDB handle existed: not even a LOG rotated.
    assert_eq!(census(&path), before_files, "the refusal modified the directory");
    assert!(!has_local_family(&path), "the refusal created the node-local family");

    // The writer refuses the layout on its own as well.
    let db = DB::open_for_read_only(&Options::default(), &path, false).unwrap();
    assert!(matches!(
        StagedPhysicalReplica::resume(db, history),
        Err(PhysicalError::FullResyncRequired(_))
    ));
    assert_eq!(stored(&path), before_store, "the refusal changed the store");
}

#[test]
fn every_engine_open_mode_refuses_a_staged_directory_untouched() {
    let root = tempfile::tempdir().unwrap();
    let (source, history) = raw_primary(root.path());
    let cfg = config(&root.path().join("staged"));
    let installed = stage(&source, history, root.path(), "staged", &cfg).publish().unwrap();
    let path = installed.path;
    assert!(has_local_family(&path));
    let before = census(&path);
    for (mode, refused) in [
        ("open", StorageEngine::open(&path, &cfg).err()),
        ("open_read_only", StorageEngine::open_read_only(&path, &cfg).err()),
        (
            "open_physical_snapshot",
            StorageEngine::open_physical_snapshot(&path, &cfg).err(),
        ),
    ] {
        let error = refused
            .unwrap_or_else(|| panic!("{mode} opened a staged physical receiver"))
            .to_string();
        assert!(
            error.contains(LOCAL_CF) && error.contains("physical standby start path"),
            "{mode} must refuse by naming the column family and the standby start path: {error}"
        );
    }
    assert_eq!(census(&path), before, "a refused open touched the staged directory");
}

type OptionsFile = BTreeMap<String, BTreeMap<String, String>>;

/// The newest `OPTIONS-<n>` file of `dir`, as `section -> key -> value`.
fn options_file(dir: &Path) -> OptionsFile {
    let newest = fs::read_dir(dir)
        .unwrap()
        .filter_map(|entry| {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            let number = name.strip_prefix("OPTIONS-")?.parse::<u64>().ok()?;
            Some((number, name))
        })
        .max()
        .expect("RocksDB writes an OPTIONS file on every writable open")
        .1;
    let mut sections = OptionsFile::new();
    let mut current = None;
    for line in fs::read_to_string(dir.join(newest)).unwrap().lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|line| line.strip_suffix(']')) {
            sections.entry(name.to_string()).or_default();
            current = Some(name.to_string());
        } else if let (Some(section), Some((key, value))) = (&current, line.split_once('=')) {
            sections
                .get_mut(section)
                .unwrap()
                .insert(key.to_string(), value.to_string());
        }
    }
    sections
}

#[test]
fn the_receiver_opens_its_store_with_the_engines_options() {
    let root = tempfile::tempdir().unwrap();
    let engine_dir = root.path().join("engine");
    let mut cfg = config(&engine_dir);
    // Off the defaults, so agreement cannot be two `Options::default()`s.
    cfg.storage.compression = heliosdb_nano::config::CompressionType::Lz4;
    cfg.storage.cache_size = 96 * 1024 * 1024;
    cfg.storage.rocksdb_max_background_jobs = Some(3);
    cfg.storage.rocksdb_bytes_per_sync = Some(2 * 1024 * 1024);
    cfg.storage.rocksdb_max_write_buffer_number = Some(3);
    cfg.replication.physical.wal_retention_secs = 600;
    cfg.replication.physical.wal_retention_size_mb = 256;

    let (_, fresh) = StorageEngine::build_db_options(&cfg, &engine_dir).unwrap();
    assert_eq!(
        fresh.iter().map(|family| family.name()).collect::<Vec<_>>(),
        ["default"]
    );
    drop(StorageEngine::open(&engine_dir, &cfg).unwrap());
    let engine = options_file(&engine_dir);

    let (source, history) = raw_primary(root.path());
    let closed = stage(&source, history, root.path(), "standby", &cfg).close().unwrap();
    let initial = options_file(&closed.path);
    let (_, staged) = StorageEngine::build_db_options(&cfg, &closed.path).unwrap();
    assert_eq!(
        staged.iter().map(|family| family.name()).collect::<Vec<_>>(),
        ["default", LOCAL_CF]
    );
    ReopenedPhysicalStage::open(&closed.path, history, TransferLimits::default(), &cfg)
        .unwrap()
        .close()
        .unwrap();
    let reopened = options_file(&closed.path);

    // What the engine sets, read back from RocksDB's own record of the open.
    let cf = &engine["CFOptions \"default\""];
    let table = &engine["TableOptions/BlockBasedTable \"default\""];
    assert_eq!(cf["prefix_extractor"], "rocksdb.FixedPrefix.5");
    assert_eq!(cf["compression"], "kLZ4Compression");
    assert_eq!(cf["max_write_buffer_number"], "3");
    assert_eq!(table["block_size"], "16384");
    assert_eq!(table["filter_policy"], "bloomfilter");
    assert_eq!(engine["DBOptions"]["max_background_jobs"], "3");
    assert_eq!(engine["DBOptions"]["bytes_per_sync"], "2097152");
    assert_eq!(engine["DBOptions"]["WAL_ttl_seconds"], "600");

    // Database-level differences by design: only the engine may create a
    // store, and a receiver retains no archived WAL (it is never a source,
    // and the stage's file census admits no `archive/` directory).
    let by_design = ["create_if_missing", "WAL_ttl_seconds", "WAL_size_limit_MB"];
    let db_options = |file: &OptionsFile| -> BTreeMap<String, String> {
        file["DBOptions"]
            .iter()
            .filter(|(key, _)| !by_design.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    };
    for (open, receiver) in [("initial open", &initial), ("reopen", &reopened)] {
        for section in ["CFOptions \"default\"", "TableOptions/BlockBasedTable \"default\""] {
            assert_eq!(
                receiver.get(section),
                engine.get(section),
                "{open}: [{section}] differs from the engine's"
            );
        }
        assert_eq!(db_options(receiver), db_options(&engine), "{open}: [DBOptions] differ");
        assert_eq!(receiver["DBOptions"]["WAL_ttl_seconds"], "0");
        // The node-local family is plain: no prefix extractor, no filter.
        assert_eq!(receiver["CFOptions \"__nano_local\""]["prefix_extractor"], "nullptr");
        assert_eq!(
            receiver["TableOptions/BlockBasedTable \"__nano_local\""]["filter_policy"],
            "nullptr"
        );
    }
    assert!(!closed.path.join("archive").exists());
}
