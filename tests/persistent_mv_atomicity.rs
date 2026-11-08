//! Persistent HNSW must share an MV replacement/drop's native write group.
#![cfg(feature = "vector-persist")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::storage::{WalEntry, WalOperation};
use heliosdb_nano::{Column, Config, DataType, EmbeddedDatabase, Schema, Tuple, Value};
use rocksdb::{WriteBatchIterator, DB};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const INDEX: &str = "mv_vec";
const REFRESH: &str = "REFRESH MATERIALIZED VIEW CONCURRENTLY mv";

fn config(path: &Path, history: bool, strict: bool) -> Config {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    config.storage.wal_enabled = true;
    config.storage.logical_wal_per_statement = strict;
    config.storage.time_travel_enabled = history;
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    config.replication.physical.enabled = true;
    config
}
fn open(path: &Path, history: bool, strict: bool) -> EmbeddedDatabase {
    EmbeddedDatabase::with_config(config(path, history, strict)).unwrap()
}
fn execute(db: &EmbeddedDatabase, params: bool, sql: &str) -> heliosdb_nano::Result<()> {
    if params {
        db.execute_params(sql, &[]).map(|_| ())
    } else {
        db.execute(sql).map(|_| ())
    }
}
fn seed(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE src (id INT PRIMARY KEY, embedding VECTOR(2))")
        .unwrap();
    db.execute("INSERT INTO src VALUES (1, CAST('[9.0, 0.0]' AS VECTOR(2))), (2, CAST('[8.0, 0.0]' AS VECTOR(2)))")
        .unwrap();
    db.execute("CREATE MATERIALIZED VIEW mv AS SELECT * FROM src").unwrap();
    db.execute("CREATE INDEX mv_vec ON __mv_mv USING hnsw (embedding) WITH (persistent = true)")
        .unwrap();
    assert_graph(db, "__mv_mv", INDEX, &[1, 2], 2);
}
fn change_source(db: &EmbeddedDatabase) {
    db.execute("DELETE FROM src").unwrap();
    db.execute("INSERT INTO src VALUES (3, CAST('[0.0, 0.0]' AS VECTOR(2))), (4, CAST('[2.0, 0.0]' AS VECTOR(2)))")
        .unwrap();
}
fn image(db: &DB) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut opts = rocksdb::ReadOptions::default();
    opts.set_total_order_seek(true);
    db.iterator_opt(rocksdb::IteratorMode::Start, opts)
        .map(|entry| {
            let (key, value) = entry.unwrap();
            (key.to_vec(), value.to_vec())
        })
        .collect()
}
fn index_prefix(db: &EmbeddedDatabase, name: &str) -> Vec<u8> {
    for (key, _) in image(&db.storage.db()) {
        if let Some((id, suffix)) = heliosdb_nano::vector::persistent::split_record_key(&key) {
            if db.storage.vector_indexes().persistent_index_with_id(id).as_deref() == Some(name) {
                return key[..key.len() - suffix.len()].to_vec();
            }
        }
    }
    panic!("no persistent records for {name}")
}
fn prefix_image(db: &DB, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    image(db)
        .into_iter()
        .filter(|(key, _)| key.starts_with(prefix))
        .collect()
}
fn assert_graph(db: &EmbeddedDatabase, table: &str, name: &str, expected: &[i32], nearest: i32) {
    let schema = db.storage.catalog().get_table_schema(table).unwrap();
    let rows: BTreeMap<_, _> = db
        .storage
        .scan_table_with_schema(table, &schema)
        .unwrap()
        .into_iter()
        .map(|row| {
            let Value::Int4(id) = row.values[0] else {
                panic!("unexpected logical row: {row:?}")
            };
            (row.row_id.unwrap(), id)
        })
        .collect();
    let manager = db.storage.vector_indexes();
    assert_eq!(manager.get_index_stats(name).unwrap().quantization, "Persistent");
    assert_eq!(manager.index_live_size(name), Some(expected.len()));
    let found = manager.search(name, &vec![0.0, 0.0], expected.len() + 1).unwrap();
    assert_eq!(found.len(), expected.len(), "all live graph members are observed");
    let found_ids: BTreeSet<_> = found.iter().map(|(row, _)| *rows.get(row).unwrap()).collect();
    assert_eq!(found_ids, expected.iter().copied().collect());
    assert_eq!(rows[&found[0].0], nearest);
}

#[derive(Default, Debug)]
struct Group {
    keys: Vec<Vec<u8>>,
    operations: Vec<WalOperation>,
}
impl WriteBatchIterator for Group {
    fn put(&mut self, key: Box<[u8]>, value: Box<[u8]>) {
        if key.starts_with(b"wal:entries:") {
            self.operations.push(WalEntry::deserialize(&value).unwrap().operation);
        }
        self.keys.push(key.into_vec());
    }
    fn delete(&mut self, key: Box<[u8]>) {
        self.keys.push(key.into_vec());
    }
}
impl Group {
    fn has(&self, prefix: &[u8]) -> bool {
        self.keys.iter().any(|key| key.starts_with(prefix))
    }
}
fn groups(db: &EmbeddedDatabase, after: u64) -> Vec<Group> {
    let raw = db.storage.db();
    let end = raw.latest_sequence_number();
    let mut next = after + 1;
    let mut result = Vec::new();
    let mut updates = raw.get_updates_since(after).unwrap();
    for item in updates.by_ref() {
        let (first, batch) = item.unwrap();
        if first > end {
            break;
        }
        assert_eq!(first, next, "no omitted native write groups");
        next = first + batch.len() as u64;
        let mut group = Group::default();
        batch.iterate(&mut group);
        result.push(group);
    }
    updates.status().unwrap();
    assert_eq!(next, end + 1);
    result
}
fn assert_refresh_group(db: &EmbeddedDatabase, after: u64, history: bool) {
    let groups = groups(db, after);
    let rows: Vec<_> = groups.iter().filter(|group| group.has(b"data:__mv_mv:")).collect();
    assert_eq!(rows.len(), 1, "one row replacement: {groups:#?}");
    let group = rows[0];
    assert!(group.has(b"meta:mv:mv"), "{group:#?}");
    assert!(group.has(b"__vidx:"), "the graph rides the row group: {group:#?}");
    assert_eq!(groups.iter().filter(|group| group.has(b"__vidx:")).count(), 1);
    assert_eq!(
        group
            .operations
            .iter()
            .filter(|op| matches!(op, WalOperation::RefreshMaterializedView { name, .. } if name == "mv"))
            .count(),
        1,
        "refresh WAL operation shares the native row+graph group"
    );
    if history {
        assert!(group.has(b"snapshot:"), "historical cut shares the row+graph group");
    }
}

#[test]
fn persistent_refresh_rows_graph_wal_and_snapshot_share_one_group_in_both_routes() {
    for history in [false, true] {
        for strict in [false, true] {
            for params in [false, true] {
                let path = tempfile::tempdir().unwrap();
                let db = open(path.path(), history, strict);
                seed(&db);
                change_source(&db);
                let before = db.storage.db().latest_sequence_number();
                execute(&db, params, REFRESH).unwrap();
                assert_refresh_group(&db, before, history);
                assert_graph(&db, "__mv_mv", INDEX, &[3, 4], 3);
                let raw = db.storage.db();
                let expected = image(&raw);
                let sequence = raw.latest_sequence_number();
                let readonly = EmbeddedDatabase::open_read_only(path.path()).unwrap();
                assert_graph(&readonly, "__mv_mv", INDEX, &[3, 4], 3);
                assert_eq!(
                    readonly.storage.write_gate().refusals(),
                    0,
                    "read-only hydration never attempts a write"
                );
                drop(readonly);
                assert_eq!(raw.latest_sequence_number(), sequence);
                assert_eq!(image(&raw), expected, "read-only graph hydration preserves all bytes");
                drop(raw);
                drop(db);
                let reopened = open(path.path(), history, strict);
                assert_graph(&reopened, "__mv_mv", INDEX, &[3, 4], 3);
            }
        }
    }
}

#[test]
fn persistent_drop_funnel_purges_exact_namespace_and_allows_name_reuse() {
    for action in ["index", "table", "view", "shape", "column"] {
        let path = tempfile::tempdir().unwrap();
        let db = open(path.path(), true, false);
        seed(&db);
        let prefix = index_prefix(&db, INDEX);
        db.execute("CREATE TABLE neighbor (id INT PRIMARY KEY, embedding VECTOR(2))")
            .unwrap();
        db.execute("INSERT INTO neighbor VALUES (7, CAST('[0.0, 0.0]' AS VECTOR(2)))")
            .unwrap();
        db.execute("CREATE INDEX \"mv_vec:neighbor\" ON neighbor USING hnsw (embedding) WITH (persistent = true)")
            .unwrap();
        let neighbor_prefix = index_prefix(&db, "mv_vec:neighbor");
        assert_ne!(prefix, neighbor_prefix);
        let neighbor = prefix_image(&db.storage.db(), &neighbor_prefix);
        let before = db.storage.db().latest_sequence_number();
        match action {
            "index" => {
                db.execute("DROP INDEX mv_vec").unwrap();
            }
            "table" => {
                db.execute("DROP TABLE __mv_mv").unwrap();
            }
            "view" => {
                db.execute("DROP MATERIALIZED VIEW mv").unwrap();
            }
            "column" => {
                db.execute("ALTER TABLE __mv_mv DROP COLUMN embedding").unwrap();
            }
            "shape" => {
                db.storage
                    .mv_catalog()
                    .store_view_data(
                        "mv",
                        vec![Tuple::new(vec![Value::Int4(9)])],
                        &Schema::new(vec![Column::new("replacement", DataType::Int4)]),
                    )
                    .unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            prefix_image(&db.storage.db(), &prefix).is_empty(),
            "orphan graph after {action}"
        );
        assert_eq!(
            prefix_image(&db.storage.db(), &neighbor_prefix),
            neighbor,
            "quoted neighboring name is untouched"
        );
        assert_graph(&db, "neighbor", "mv_vec:neighbor", &[7], 7);
        let groups = groups(&db, before);
        let graph_groups: Vec<_> = groups.iter().filter(|group| group.has(&prefix)).collect();
        assert_eq!(
            graph_groups.len(),
            1,
            "persistent purge uses the DDL group: {groups:#?}"
        );
        assert!(graph_groups[0].has(b"meta:index:mv_vec"));
        assert!(!db.storage.vector_indexes().index_exists(INDEX));
        db.execute("CREATE TABLE reused (id INT PRIMARY KEY, embedding VECTOR(2))")
            .unwrap();
        db.execute("INSERT INTO reused VALUES (99, CAST('[0.0, 0.0]' AS VECTOR(2)))")
            .unwrap();
        db.execute("CREATE INDEX mv_vec ON reused USING hnsw (embedding) WITH (persistent = true)")
            .unwrap();
        assert_graph(&db, "reused", INDEX, &[99], 99);
        drop(db);
        let reopened = open(path.path(), true, false);
        assert_graph(&reopened, "reused", INDEX, &[99], 99);
        assert_graph(&reopened, "neighbor", "mv_vec:neighbor", &[7], 7);
    }
}

#[cfg(feature = "internal-tests")]
mod internal {
    use super::*;
    use heliosdb_nano::storage::ddl_batch::{debug_fail_next_applied_commit_with, debug_fail_next_commit};
    use std::sync::{mpsc, Arc};
    use std::time::{Duration, Instant};

    #[test]
    fn refused_persistent_refresh_preserves_live_rows_graph_and_writes_without_reopen() {
        for cap in [false, true] {
            let path = tempfile::tempdir().unwrap();
            let db = open(path.path(), true, true);
            seed(&db);
            change_source(&db);
            let before = db.storage.db().latest_sequence_number();
            let prefix = index_prefix(&db, INDEX);
            let old_graph = prefix_image(&db.storage.db(), &prefix);
            if cap {
                db.storage.debug_set_write_group_ceiling(Some(64));
            } else {
                debug_fail_next_commit();
            }
            let error = db.execute(REFRESH).unwrap_err();
            db.storage.debug_set_write_group_ceiling(None);
            if cap {
                assert!(
                    error
                        .to_string()
                        .contains(heliosdb_nano::BRANCH_MATERIALIZE_TOO_LARGE_MARKER),
                    "{error}"
                );
            }
            assert_eq!(db.storage.db().latest_sequence_number(), before);
            assert_eq!(prefix_image(&db.storage.db(), &prefix), old_graph);
            assert_graph(&db, "__mv_mv", INDEX, &[1, 2], 2);
            db.storage.debug_set_write_group_ceiling(Some(64));
            assert!(db.storage.vector_indexes().drop_index(INDEX).is_err());
            db.storage.debug_set_write_group_ceiling(None);
            assert_graph(&db, "__mv_mv", INDEX, &[1, 2], 2);
            assert_eq!(prefix_image(&db.storage.db(), &prefix), old_graph);
            db.execute("INSERT INTO __mv_mv VALUES (9, CAST('[0.0, 0.0]' AS VECTOR(2)))")
                .unwrap();
            assert_graph(&db, "__mv_mv", INDEX, &[1, 2, 9], 9);
            db.execute("DELETE FROM __mv_mv WHERE id = 9").unwrap();
            assert_graph(&db, "__mv_mv", INDEX, &[1, 2], 2);
        }
    }

    #[test]
    fn applied_refresh_error_publishes_no_error_path_writes_and_reopens_actual_graph() {
        for params in [false, true] {
            let path = tempfile::tempdir().unwrap();
            let db = open(path.path(), true, params);
            seed(&db);
            change_source(&db);
            let raw = db.storage.db();
            let applied = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let saved = Arc::clone(&applied);
            let hook_raw = Arc::clone(&raw);
            let before = raw.latest_sequence_number();
            debug_fail_next_applied_commit_with(move || {
                saved.store(hook_raw.latest_sequence_number(), std::sync::atomic::Ordering::SeqCst)
            });
            let error = execute(&db, params, REFRESH).unwrap_err();
            assert!(error.to_string().contains("reconciliation failed"), "{error}");
            assert!(
                applied.load(std::sync::atomic::Ordering::SeqCst) > before,
                "real native group applied"
            );
            assert_eq!(
                raw.latest_sequence_number(),
                applied.load(std::sync::atomic::Ordering::SeqCst),
                "no derived cleanup writes on error"
            );
            assert!(db.query("SELECT * FROM mv", &[]).is_err());
            assert!(!db.storage.vector_indexes().index_exists(INDEX));
            assert_refresh_group(&db, before, true);
            drop(raw);
            drop(db);
            let readonly = EmbeddedDatabase::open_read_only(path.path()).unwrap();
            let before_read = image(&readonly.storage.db());
            assert_graph(&readonly, "__mv_mv", INDEX, &[3, 4], 3);
            assert_eq!(image(&readonly.storage.db()), before_read);
            assert_eq!(readonly.storage.write_gate().refusals(), 0);
            drop(readonly);
            let reopened = open(path.path(), true, params);
            assert_graph(&reopened, "__mv_mv", INDEX, &[3, 4], 3);
        }
    }

    #[test]
    fn purge_retires_the_waiting_direct_mutator_before_releasing_its_lease() {
        let path = tempfile::tempdir().unwrap();
        let db = Arc::new(open(path.path(), true, false));
        seed(&db);
        let prefix = index_prefix(&db, INDEX);
        let (sent, received) = mpsc::channel();
        let worker_db = Arc::clone(&db);
        let raw = db.storage.db();
        let applied = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let hook_applied = Arc::clone(&applied);
        let hook_raw = Arc::clone(&raw);
        let hook_db = Arc::clone(&db);
        debug_fail_next_applied_commit_with(move || {
            hook_applied.store(hook_raw.latest_sequence_number(), std::sync::atomic::Ordering::SeqCst);
            std::thread::spawn(move || {
                let result = worker_db
                    .storage
                    .vector_indexes()
                    .insert_vector(INDEX, 999, &vec![0.0, 0.0]);
                sent.send(result).unwrap();
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while hook_db.storage.vector_indexes().debug_persistent_write_waiters(INDEX) != Some(1) {
                assert!(
                    Instant::now() < deadline,
                    "direct mutator never entered the held graph lease"
                );
                std::thread::yield_now();
            }
        });
        assert!(db.execute("DROP INDEX mv_vec").is_err());
        let error = received.recv_timeout(Duration::from_secs(5)).unwrap().unwrap_err();
        assert!(error.to_string().contains("retired"), "{error}");
        assert!(applied.load(std::sync::atomic::Ordering::SeqCst) > 0);
        assert_eq!(
            raw.latest_sequence_number(),
            applied.load(std::sync::atomic::Ordering::SeqCst),
            "neither error settlement nor waiter resurrects graph records"
        );
        assert!(prefix_image(&raw, &prefix).is_empty());
        assert!(raw.get(b"meta:index:mv_vec").unwrap().is_none());
    }

    const CHILD: &str = "NANO_PERSISTENT_MV_CRASH_PATH";
    const ACTION: &str = "NANO_PERSISTENT_MV_CRASH_ACTION";
    #[test]
    fn crash_before_any_ram_publication_keeps_complete_persistent_refresh_or_drop() {
        const NAME: &str = "internal::crash_before_any_ram_publication_keeps_complete_persistent_refresh_or_drop";
        if let Ok(path) = std::env::var(CHILD) {
            let db = open(Path::new(&path), true, true);
            let action = std::env::var(ACTION).unwrap();
            std::fs::write(Path::new(&path).join("persistent-mv-child-entered"), action.as_bytes()).unwrap();
            db.execute(if action == "refresh" {
                REFRESH
            } else {
                "DROP TABLE __mv_mv"
            })
            .unwrap();
            panic!("real pre-publication crash boundary did not fire");
        }
        for action in ["refresh", "drop"] {
            let path = tempfile::tempdir().unwrap();
            let prefix = {
                let db = open(path.path(), true, true);
                seed(&db);
                change_source(&db);
                index_prefix(&db, INDEX)
            };
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture"])
                .env(CHILD, path.path())
                .env(ACTION, action)
                .env(
                    "HELIOSDB_NANO_TEST_EXIT_AFTER_DDL_WRITE",
                    if action == "refresh" {
                        "refresh_mv:mv"
                    } else {
                        "drop:__mv_mv"
                    },
                )
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(0), "crash child {status}");
            assert_eq!(
                std::fs::read(path.path().join("persistent-mv-child-entered")).unwrap(),
                action.as_bytes(),
                "the exact child test really executed"
            );
            let readonly = EmbeddedDatabase::open_read_only(path.path()).unwrap();
            let before = image(&readonly.storage.db());
            if action == "refresh" {
                assert_graph(&readonly, "__mv_mv", INDEX, &[3, 4], 3);
            } else {
                assert!(!readonly.storage.catalog().table_exists("__mv_mv").unwrap());
                assert!(!readonly.storage.vector_indexes().index_exists(INDEX));
                assert!(prefix_image(&readonly.storage.db(), &prefix).is_empty());
            }
            assert_eq!(image(&readonly.storage.db()), before);
            assert_eq!(readonly.storage.write_gate().refusals(), 0);
            drop(readonly);
            let reopened = open(path.path(), true, true);
            if action == "refresh" {
                assert_graph(&reopened, "__mv_mv", INDEX, &[3, 4], 3);
            } else {
                reopened
                    .execute("CREATE TABLE reused (id INT PRIMARY KEY, embedding VECTOR(2))")
                    .unwrap();
                reopened
                    .execute("INSERT INTO reused VALUES (99, CAST('[0.0, 0.0]' AS VECTOR(2)))")
                    .unwrap();
                reopened
                    .execute("CREATE INDEX mv_vec ON reused USING hnsw (embedding) WITH (persistent = true)")
                    .unwrap();
                assert_graph(&reopened, "reused", INDEX, &[99], 99);
            }
        }
    }
}

#[cfg(all(feature = "ha-tier1", target_os = "linux"))]
#[test]
fn physical_standby_applies_persistent_mv_graph_in_the_original_group_without_derived_writes() {
    use heliosdb_nano::replication::checkpoint_transfer::{OnlineCheckpointExport, ReceiverStaging, TransferLimits};
    use heliosdb_nano::replication::history::load_or_create_primary_history;
    use heliosdb_nano::replication::physical::{poll_source, FlushMemo, MAX_BATCH_BYTES};
    use heliosdb_nano::replication::physical_applier::{
        decode_raw_batch, DerivedStateListener, PhysicalApplier, RawOp,
    };
    use heliosdb_nano::storage::{StorageEngine, WriteGate};
    use std::sync::Arc;

    let root = tempfile::tempdir().unwrap();
    let primary_path = root.path().join("primary");
    let cfg = config(&primary_path, true, true);
    let primary = EmbeddedDatabase::with_config(cfg.clone()).unwrap();
    seed(&primary);
    let prefix = index_prefix(&primary, INDEX);
    let raw = primary.storage.db();
    let history = load_or_create_primary_history(raw.path()).unwrap();
    let limits = TransferLimits::default();
    let export = OnlineCheckpointExport::create(&raw, history, root.path(), limits).unwrap();
    let mut receiver = ReceiverStaging::begin(
        &root.path().join("standby"),
        &export.manifest_bytes().unwrap(),
        history,
        limits,
    )
    .unwrap();
    for (index, file) in export.manifest().files.iter().enumerate() {
        let mut offset = 0;
        while offset < file.size {
            let bytes = export.read_chunk(index as u32, offset, 64 * 1024).unwrap();
            receiver.append_chunk(index as u32, offset, &bytes).unwrap();
            offset += bytes.len() as u64;
        }
    }
    let stage = receiver.finish_download(&cfg).unwrap().open(&cfg).unwrap();
    let path = stage.staging_path().to_path_buf();
    let standby_cfg = config(&path, true, true);
    let engine = Arc::new(
        StorageEngine::open_physical_standby(
            &path,
            &standby_cfg,
            Arc::clone(stage.writer()),
            WriteGate::closed("physical standby"),
        )
        .unwrap(),
    );
    let standby = Arc::new(EmbeddedDatabase::with_physical_standby(standby_cfg, Arc::clone(&engine)).unwrap());
    let listener: Arc<dyn DerivedStateListener> = standby.clone();
    let applier = PhysicalApplier::new(Arc::clone(&engine), Arc::clone(stage.writer()), listener).unwrap();
    assert_graph(&standby, "__mv_mv", INDEX, &[1, 2], 2);
    assert_eq!(engine.write_gate().refusals(), 0, "initial hydration is read-only");
    change_source(&primary);
    primary.execute(REFRESH).unwrap();
    let mut applied = 0;
    let mut replacements = 0;
    loop {
        let poll = poll_source(
            &raw,
            history,
            applier.writer().cursor().source_sequence(),
            MAX_BATCH_BYTES,
            &FlushMemo::new(),
        )
        .unwrap();
        if poll.batches.is_empty() {
            break;
        }
        for batch in &poll.batches {
            let records = decode_raw_batch(batch.data()).unwrap();
            let rows = records.iter().any(|record| match record {
                RawOp::Put { key, .. } | RawOp::Delete { key } => key.starts_with(b"data:__mv_mv:"),
                RawOp::DeleteRange { start, .. } => start.starts_with(b"data:__mv_mv:"),
            });
            if rows {
                replacements += 1;
                assert!(
                    records.iter().any(|record| match record {
                        RawOp::Put { key, .. } | RawOp::Delete { key } => key.starts_with(&prefix),
                        RawOp::DeleteRange { start, .. } => start.starts_with(&prefix),
                    }),
                    "replacement frame carries the persistent graph"
                );
            }
            applier.apply(batch).unwrap();
            applied += 1;
            if rows {
                assert_graph(&standby, "__mv_mv", INDEX, &[3, 4], 3);
                assert_eq!(
                    prefix_image(applier.writer().db(), &prefix),
                    prefix_image(&raw, &prefix)
                );
            }
        }
    }
    assert!(applied > 0, "real source groups were applied");
    assert_eq!(replacements, 1);
    assert_eq!(
        applier.writer().cursor().source_sequence(),
        raw.latest_sequence_number()
    );
    assert_eq!(
        engine.write_gate().refusals(),
        0,
        "neither hydration nor vector apply asks the standby gate to write"
    );
    assert_graph(&standby, "__mv_mv", INDEX, &[3, 4], 3);
    let source_rows = prefix_image(&raw, b"data:__mv_mv:");
    assert_eq!(prefix_image(applier.writer().db(), b"data:__mv_mv:"), source_rows);
    let before_read = image(applier.writer().db());
    let before_sequence = applier.writer().db().latest_sequence_number();
    for _ in 0..4 {
        assert_graph(&standby, "__mv_mv", INDEX, &[3, 4], 3);
    }
    assert_eq!(applier.writer().db().latest_sequence_number(), before_sequence);
    assert_eq!(
        image(applier.writer().db()),
        before_read,
        "standby reads leave every received/local key unchanged"
    );
    // Explicit drop order keeps the staging owner alive until every borrowed
    // database/applier handle is gone.
    drop(applier);
    drop(standby);
    drop(engine);
    drop(stage);
}
