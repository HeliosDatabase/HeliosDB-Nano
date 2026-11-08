//! Directly durable storage calls retain their data/counter/history group even
//! when invoked inside a different transaction's statement-capture frame.
#![cfg(feature = "internal-tests")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::config::WalSyncModeConfig;
use heliosdb_nano::storage::{LogicalCatchup, WalEntry, WalOperation};
use heliosdb_nano::{Config, EmbeddedDatabase, Tuple, Value};
use rocksdb::WriteBatchIterator;
use std::path::Path;
use std::sync::{Arc, Barrier};

fn config(dir: &Path, time_travel: bool, mode: WalSyncModeConfig) -> Config {
    let mut config = Config::default();
    config.storage.path = Some(dir.to_path_buf());
    config.storage.memory_only = false;
    config.storage.time_travel_enabled = time_travel;
    config.storage.wal_enabled = true;
    config.storage.logical_wal_per_statement = true;
    config.storage.wal_sync_mode = mode;
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    config.replication.physical.enabled = true;
    config
}

fn open(config: Config) -> EmbeddedDatabase {
    let mut last = None;
    for _ in 0..100 {
        match EmbeddedDatabase::with_config(config.clone()) {
            Ok(db) => return db,
            Err(error) => {
                last = Some(error);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
    panic!("open failed: {last:?}");
}

#[derive(Default, Debug)]
struct Group(Vec<(String, Option<Vec<u8>>)>);
impl WriteBatchIterator for Group {
    fn put(&mut self, key: Box<[u8]>, value: Box<[u8]>) {
        self.0
            .push((String::from_utf8(key.into_vec()).unwrap(), Some(value.into_vec())));
    }
    fn delete(&mut self, key: Box<[u8]>) {
        self.0.push((String::from_utf8(key.into_vec()).unwrap(), None));
    }
}
impl Group {
    fn has(&self, key: &str) -> bool {
        self.0.iter().any(|(found, _)| found == key)
    }
    fn has_prefix(&self, prefix: &str) -> bool {
        self.0.iter().any(|(found, _)| found.starts_with(prefix))
    }
    fn entries(&self) -> Vec<WalEntry> {
        self.0
            .iter()
            .filter_map(|(key, value)| {
                key.starts_with("wal:entries:")
                    .then(|| bincode::deserialize(value.as_ref().expect("entry put")).unwrap())
            })
            .collect()
    }
}

fn groups_since(db: &EmbeddedDatabase, after: u64) -> Vec<Group> {
    let raw = db.storage.db();
    let latest = raw.latest_sequence_number();
    let mut next = after + 1;
    let mut groups = Vec::new();
    let mut updates = raw.get_updates_since(after).unwrap();
    for update in updates.by_ref() {
        let (first, batch) = update.unwrap();
        if first > latest {
            break;
        }
        assert_eq!(first, next, "no gaps in retained physical WAL");
        next = first + batch.len() as u64;
        let mut group = Group::default();
        batch.iterate(&mut group);
        groups.push(group);
    }
    updates.status().unwrap();
    assert_eq!(next, latest + 1, "all groups inspected");
    groups
}

#[test]
fn direct_inserts_keep_row_counter_and_log_together_inside_capture_and_in_both_tt_modes() {
    for time_travel in [false, true] {
        for capture in [false, true] {
            for path in ["ordinary", "versioned", "fast"] {
                let temp = tempfile::TempDir::new().unwrap();
                let db = open(config(temp.path(), time_travel, WalSyncModeConfig::Sync));
                db.execute("CREATE TABLE direct_row (id INT PRIMARY KEY, body TEXT)")
                    .unwrap();
                let schema = db.storage.catalog().get_table_schema("direct_row").unwrap();
                let before = db.storage.db().latest_sequence_number();
                let insert = || {
                    let row = Tuple::new(vec![Value::Int4(7), Value::String("payload".into())]);
                    match path {
                        "ordinary" => db.storage.insert_tuple("direct_row", row),
                        "versioned" => db
                            .storage
                            .insert_tuple_versioned_with_schema("direct_row", row, &schema),
                        _ => db.storage.insert_tuple_fast("direct_row", row, &schema),
                    }
                    .unwrap()
                };
                let row_id = if capture {
                    db.storage.debug_with_statement_wal_capture(insert)
                } else {
                    insert()
                };
                let row_key = format!("data:direct_row:{row_id}");
                let groups = groups_since(&db, before);
                let row_groups: Vec<_> = groups.iter().filter(|group| group.has(&row_key)).collect();
                assert_eq!(
                    row_groups.len(),
                    1,
                    "{path}, TT={time_travel}, capture={capture}: {groups:?}"
                );
                let group = row_groups[0];
                assert!(group.has("counter:direct_row"), "counter in row group: {groups:?}");
                let entries = group.entries();
                assert_eq!(entries.len(), 1, "one logical row entry: {groups:?}");
                assert!(matches!(&entries[0].operation, WalOperation::Insert { table, key, .. }
                    if table == "direct_row" && key.as_slice() == row_key.as_bytes()));
                assert_eq!(groups.iter().filter(|group| group.has("counter:direct_row")).count(), 1);
                assert_eq!(group.has_prefix("snapshot:"), time_travel);
                assert_eq!(group.has_prefix("v_idx:direct_row:"), time_travel);
                assert!(db.storage.db().get(row_key.as_bytes()).unwrap().is_some());
                // The counter is an allocator high-water mark. Identity-aware
                // APIs also reserve the explicitly supplied integer PK (7),
                // although this row itself was allocated the physical id1.
                let durable: u64 =
                    bincode::deserialize(&db.storage.db().get(b"counter:direct_row").unwrap().unwrap()).unwrap();
                let expected = if path == "ordinary" && !time_travel {
                    row_id
                } else {
                    row_id.max(7)
                };
                assert_eq!(durable, expected, "{path}, TT={time_travel}, capture={capture}");
                assert_eq!(db.storage.row_counter_high_water("direct_row"), Some(durable));
            }
        }
    }
}

#[test]
fn direct_delete_keeps_its_log_with_data_removal_inside_capture() {
    let temp = tempfile::TempDir::new().unwrap();
    let db = open(config(temp.path(), true, WalSyncModeConfig::Sync));
    db.execute("CREATE TABLE direct_delete (id INT PRIMARY KEY)").unwrap();
    let row_id = db
        .storage
        .insert_tuple("direct_delete", Tuple::new(vec![Value::Int4(1)]))
        .unwrap();
    let key = format!("data:direct_delete:{row_id}").into_bytes();
    let before = db.storage.db().latest_sequence_number();
    db.storage
        .debug_with_statement_wal_capture(|| db.storage.delete(&key).unwrap());
    let groups = groups_since(&db, before);
    assert_eq!(groups.len(), 1, "{groups:?}");
    let entries = groups[0].entries();
    assert_eq!(entries.len(), 1);
    assert!(
        matches!(&entries[0].operation, WalOperation::Delete { table, key: found }
        if table == "direct_delete" && found == &key)
    );
    assert!(groups[0].has(std::str::from_utf8(&key).unwrap()));
    assert!(db.storage.db().get(&key).unwrap().is_none());
}

#[test]
fn durable_counter_and_log_share_a_group_in_every_sync_mode_even_inside_capture() {
    for mode in [
        WalSyncModeConfig::Sync,
        WalSyncModeConfig::Async,
        WalSyncModeConfig::GroupCommit,
    ] {
        let temp = tempfile::TempDir::new().unwrap();
        let db = open(config(temp.path(), false, mode));
        db.execute("CREATE TABLE allocate (id INT)").unwrap();
        let before = db.storage.db().latest_sequence_number();
        let next = db
            .storage
            .debug_with_statement_wal_capture(|| db.storage.next_row_id("allocate").unwrap());
        let groups = groups_since(&db, before);
        let group = groups.iter().find(|group| group.has("counter:allocate")).unwrap();
        let entries = group.entries();
        assert_eq!(entries.len(), 1, "{groups:?}");
        assert!(
            matches!(&entries[0].operation, WalOperation::UpdateCounter { table_name, new_value }
            if table_name == "allocate" && *new_value == next)
        );
        assert_eq!(groups.iter().filter(|group| group.has("counter:allocate")).count(), 1);
    }
}

#[test]
fn concurrent_durable_allocators_never_regress_the_counter_before_or_after_reopen() {
    const THREADS: usize = 4;
    const PER_THREAD: usize = 12;
    for mode in [
        WalSyncModeConfig::Sync,
        WalSyncModeConfig::Async,
        WalSyncModeConfig::GroupCommit,
    ] {
        let temp = tempfile::TempDir::new().unwrap();
        let cfg = config(temp.path(), false, mode);
        let db = Arc::new(open(cfg.clone()));
        db.execute("CREATE TABLE allocate (id INT)").unwrap();
        let before = db.storage.db().latest_sequence_number();
        let start = Arc::new(Barrier::new(THREADS));
        let threads: Vec<_> = (0..THREADS)
            .map(|_| {
                let db = Arc::clone(&db);
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    (0..PER_THREAD)
                        .map(|_| {
                            db.storage
                                .debug_with_statement_wal_capture(|| db.storage.next_row_id("allocate").unwrap())
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut values: Vec<_> = threads.into_iter().flat_map(|thread| thread.join().unwrap()).collect();
        values.sort_unstable();
        assert_eq!(values, (1..=(THREADS * PER_THREAD) as u64).collect::<Vec<_>>());
        let durable: u64 = bincode::deserialize(&db.storage.db().get(b"counter:allocate").unwrap().unwrap()).unwrap();
        assert_eq!(durable, (THREADS * PER_THREAD) as u64);
        let groups = groups_since(&db, before);
        let counter_groups: Vec<_> = groups.iter().filter(|group| group.has("counter:allocate")).collect();
        assert_eq!(counter_groups.len(), THREADS * PER_THREAD);
        let mut previous = 0;
        for group in counter_groups {
            let bytes = group
                .0
                .iter()
                .find(|(key, _)| key == "counter:allocate")
                .unwrap()
                .1
                .as_ref()
                .unwrap();
            let value: u64 = bincode::deserialize(bytes).unwrap();
            assert!(
                value > previous,
                "durable counter never regresses: {previous} -> {value}"
            );
            previous = value;
            assert!(group.entries().iter().any(|entry| {
                matches!(&entry.operation, WalOperation::UpdateCounter { table_name, new_value }
                    if table_name == "allocate" && *new_value == value)
            }));
        }
        assert_eq!(previous, durable);
        drop(db);
        let reopened = open(cfg);
        assert_eq!(reopened.storage.next_row_id("allocate").unwrap(), durable + 1);
    }
}

#[test]
fn refused_direct_groups_publish_neither_rows_nor_counters_nor_log_entries() {
    for time_travel in [false, true] {
        let temp = tempfile::TempDir::new().unwrap();
        let db = open(config(temp.path(), time_travel, WalSyncModeConfig::Sync));
        db.execute("CREATE TABLE refused (id INT)").unwrap();
        let sequence = db.storage.db().latest_sequence_number();
        let counter_before = db.storage.db().get(b"counter:refused").unwrap();
        let lsn = db.storage.wal_lsn();
        db.storage.debug_set_write_group_ceiling(Some(64));
        let result = db
            .storage
            .debug_with_statement_wal_capture(|| db.storage.insert_tuple("refused", Tuple::new(vec![Value::Int4(1)])));
        db.storage.debug_set_write_group_ceiling(None);
        assert!(result.is_err());
        assert_eq!(db.storage.db().latest_sequence_number(), sequence);
        assert!(db.storage.db().get(b"data:refused:1").unwrap().is_none());
        assert_eq!(db.storage.db().get(b"counter:refused").unwrap(), counter_before);
        // The abandoned reservation may raise the allocated LSN but must never
        // create an entry. A later durable write still publishes across the hole.
        assert!(groups_since(&db, sequence).is_empty());
        let next = db
            .storage
            .insert_tuple("refused", Tuple::new(vec![Value::Int4(2)]))
            .unwrap();
        assert_eq!(next, 2);
        assert!(db.storage.wal_lsn() > lsn);
        match db.storage.logical_wal_catchup(lsn.unwrap()).unwrap() {
            LogicalCatchup::Entries { entries, .. } => assert!(
                entries.iter().any(|entry| {
                    matches!(&entry.operation, WalOperation::Insert { table, key, .. }
                    if table == "refused" && key.as_slice() == b"data:refused:2")
                }),
                "the later entry publishes across the abandoned reservation"
            ),
            other => panic!("unexpected catchup result: {other:?}"),
        }
        assert!(db.storage.db().get(b"data:refused:2").unwrap().is_some());
    }
}

#[test]
fn refused_counter_groups_do_not_persist_the_attached_put_or_the_log() {
    for mode in [
        WalSyncModeConfig::Sync,
        WalSyncModeConfig::Async,
        WalSyncModeConfig::GroupCommit,
    ] {
        let temp = tempfile::TempDir::new().unwrap();
        let db = open(config(temp.path(), false, mode));
        db.execute("CREATE TABLE refused_counter (id INT)").unwrap();
        let raw = db.storage.db();
        let sequence = raw.latest_sequence_number();
        let original = raw.get(b"counter:refused_counter").unwrap();
        db.storage.debug_set_write_group_ceiling(Some(64));
        let result = db
            .storage
            .debug_with_statement_wal_capture(|| db.storage.next_row_id("refused_counter"));
        db.storage.debug_set_write_group_ceiling(None);
        assert!(result.is_err());
        assert_eq!(raw.latest_sequence_number(), sequence);
        assert_eq!(raw.get(b"counter:refused_counter").unwrap(), original);
        assert_eq!(db.storage.next_row_id("refused_counter").unwrap(), 2);
        let groups = groups_since(&db, sequence);
        let counter_groups: Vec<_> = groups
            .iter()
            .filter(|group| group.has("counter:refused_counter"))
            .collect();
        assert_eq!(counter_groups.len(), 1);
        let entries = counter_groups[0].entries();
        assert_eq!(entries.len(), 1);
        assert!(
            matches!(&entries[0].operation, WalOperation::UpdateCounter { table_name, new_value }
            if table_name == "refused_counter" && *new_value == 2)
        );
    }
}
