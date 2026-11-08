//! Mixed row-ID writers persist a monotonic counter in their own data/WAL group.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::config::WalSyncModeConfig;
use heliosdb_nano::storage::{WalEntry, WalOperation};
use heliosdb_nano::{Config, EmbeddedDatabase, Tuple, Value};
use rocksdb::WriteBatchIterator;
use std::path::Path;

fn config(path: &Path, tt: bool, mode: WalSyncModeConfig) -> Config {
    let mut c = Config::default();
    c.storage.path = Some(path.to_owned());
    c.storage.memory_only = false;
    c.storage.time_travel_enabled = tt;
    c.storage.wal_enabled = true;
    c.storage.logical_wal_per_statement = true;
    c.storage.wal_sync_mode = mode;
    c.storage.wal_checkpoint_interval_entries = 0;
    c.storage.wal_checkpoint_interval_secs = 0;
    c.replication.physical.enabled = true;
    c
}
fn open(c: Config) -> EmbeddedDatabase {
    let mut last = None;
    for _ in 0..100 {
        match EmbeddedDatabase::with_config(c.clone()) {
            Ok(db) => return db,
            Err(e) => last = Some(e),
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("open failed: {last:?}");
}
fn durable(db: &EmbeddedDatabase, table: &str) -> u64 {
    let bytes = db
        .storage
        .get(&format!("counter:{table}").into_bytes())
        .unwrap()
        .unwrap();
    bincode::deserialize(&bytes).unwrap()
}
#[derive(Default, Debug)]
struct Group {
    keys: Vec<String>,
    counters: Vec<(String, u64)>,
    operations: Vec<WalOperation>,
}
impl WriteBatchIterator for Group {
    fn put(&mut self, key: Box<[u8]>, value: Box<[u8]>) {
        if key.starts_with(b"wal:entries:") {
            let entry: WalEntry = bincode::deserialize(&value).unwrap();
            self.operations.push(entry.operation);
        }
        if key.starts_with(b"counter:") {
            self.counters.push((
                String::from_utf8(key.to_vec()).unwrap(),
                bincode::deserialize(&value).unwrap(),
            ));
        }
        self.keys.push(String::from_utf8(key.into_vec()).unwrap());
    }
    fn delete(&mut self, key: Box<[u8]>) {
        self.keys.push(String::from_utf8(key.into_vec()).unwrap());
    }
}
fn groups(db: &EmbeddedDatabase, after: u64) -> Vec<Group> {
    let raw = db.storage.db();
    let end = raw.latest_sequence_number();
    let mut next = after + 1;
    let mut out = Vec::new();
    let mut updates = raw.get_updates_since(after).unwrap();
    for update in updates.by_ref() {
        let (first, batch) = update.unwrap();
        if first > end {
            break;
        }
        assert_eq!(first, next, "every physical group is inspected");
        next = first + batch.len() as u64;
        let mut group = Group::default();
        batch.iterate(&mut group);
        out.push(group);
    }
    updates.status().unwrap();
    assert_eq!(next, end + 1);
    out
}
fn assert_counter_group(group: &Group, table: &str, expected: u64) {
    assert!(
        group.counters.contains(&(format!("counter:{table}"), expected)),
        "{group:?}"
    );
    let values: Vec<_> = group
        .operations
        .iter()
        .filter_map(|op| match op {
            WalOperation::UpdateCounter { table_name, new_value } if table_name == table => Some(*new_value),
            _ => None,
        })
        .collect();
    assert_eq!(
        values,
        vec![expected],
        "the typed WAL entry agrees with the stored counter"
    );
}
fn no_close_repair(db: &EmbeddedDatabase) {
    db.storage.set_row_counter_flush_on_close(false);
    db.storage.set_index_snapshots_on_close(false);
}

#[test]
fn a_late_transaction_preserves_newer_raw_and_volatile_counter_groups() {
    for tt in [false, true] {
        for mode in [
            WalSyncModeConfig::Sync,
            WalSyncModeConfig::Async,
            WalSyncModeConfig::GroupCommit,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let cfg = config(temp.path(), tt, mode);
            let db = open(cfg.clone());
            db.execute("CREATE TABLE mixed (body TEXT)").unwrap();
            let schema = db.storage.catalog().get_table_schema("mixed").unwrap();
            let txn = db.storage.begin_transaction().unwrap();
            let first = db.storage.next_row_id_volatile("mixed");
            txn.stage_row_counter("mixed", first).unwrap();
            let row = Tuple::with_row_id(vec![Value::String("transaction".into())], first);
            txn.put(
                format!("data:mixed:{first}").into_bytes(),
                bincode::serialize(&row).unwrap(),
            )
            .unwrap();
            let start = db.storage.db().latest_sequence_number();
            assert_eq!(db.storage.next_row_id("mixed").unwrap(), first + 1);
            let fast = db
                .storage
                .insert_tuple_fast("mixed", Tuple::new(vec![Value::String("fast".into())]), &schema)
                .unwrap();
            let ordinary = db
                .storage
                .insert_tuple("mixed", Tuple::new(vec![Value::String("ordinary".into())]))
                .unwrap();
            assert!(first < fast && fast < ordinary);
            db.storage.flush_row_counter("mixed").unwrap();
            let before_commit = db.storage.db().latest_sequence_number();
            txn.commit_with_timestamp(db.storage.next_timestamp()).unwrap();
            assert_eq!(
                durable(&db, "mixed"),
                ordinary,
                "older transaction never lowers the durable mark"
            );
            let committed = groups(&db, before_commit);
            let row_group = committed
                .iter()
                .find(|g| g.keys.contains(&format!("data:mixed:{first}")))
                .unwrap();
            assert_counter_group(row_group, "mixed", ordinary);
            let mut previous = 0;
            let mut observed = 0;
            for group in groups(&db, start) {
                for (_, value) in group.counters.iter().filter(|(key, _)| key == "counter:mixed") {
                    assert!(*value >= previous, "counter regressed {previous} -> {value}");
                    previous = *value;
                    observed += 1;
                }
            }
            assert!(
                observed >= 5,
                "raw, fast, ordinary, flush and transaction were observed"
            );
            no_close_repair(&db);
            drop(db);
            let db = open(cfg);
            let next = db
                .storage
                .insert_tuple("mixed", Tuple::new(vec![Value::String("after reopen".into())]))
                .unwrap();
            assert!(next > ordinary, "no allocated ID is reused across reopen");
            let bytes = db
                .storage
                .get(&format!("data:mixed:{first}").into_bytes())
                .unwrap()
                .unwrap();
            let original: Tuple = bincode::deserialize(&bytes).unwrap();
            assert_eq!(original.values, vec![Value::String("transaction".into())]);
        }
    }
}

#[test]
fn raw_transaction_counter_puts_normalize_to_the_shared_allocation_high_water() {
    let temp = tempfile::tempdir().unwrap();
    let db = open(config(temp.path(), false, WalSyncModeConfig::Sync));
    db.execute("CREATE TABLE raw_counter (n INT)").unwrap();
    db.storage.raise_row_counter_to("raw_counter", 90);
    db.storage.flush_row_counter("raw_counter").unwrap();
    let txn = db.storage.begin_transaction().unwrap();
    txn.put(b"counter:raw_counter".to_vec(), bincode::serialize(&3_u64).unwrap())
        .unwrap();
    let volatile = db.storage.next_row_id_volatile("raw_counter");
    assert_eq!(volatile, 91);
    let before = db.storage.db().latest_sequence_number();
    txn.commit_with_timestamp(db.storage.next_timestamp()).unwrap();
    assert_eq!(durable(&db, "raw_counter"), volatile);
    let writes = groups(&db, before);
    assert_eq!(writes.iter().filter(|g| !g.counters.is_empty()).count(), 1);
    assert_counter_group(
        writes.iter().find(|g| !g.counters.is_empty()).unwrap(),
        "raw_counter",
        volatile,
    );
    assert_eq!(db.storage.next_row_id_volatile("raw_counter"), 92);
}

#[test]
fn prepared_batch_preserves_a_newer_raw_counter_even_with_unsorted_row_ids() {
    let temp = tempfile::tempdir().unwrap();
    let mut cfg = config(temp.path(), true, WalSyncModeConfig::Sync);
    cfg.storage.logical_wal_per_statement = false;
    let db = open(cfg);
    db.execute("CREATE TABLE prepared (body TEXT)").unwrap();
    let schema = db.storage.catalog().get_table_schema("prepared").unwrap();
    let first = db.storage.next_row_id_volatile("prepared");
    let second = db.storage.next_row_id_volatile("prepared");
    let newer = db.storage.next_row_id("prepared").unwrap();
    let before = db.storage.db().latest_sequence_number();
    db.storage
        .insert_prepared_tuples_fast_batch(
            "prepared",
            vec![
                (second, Tuple::new(vec![Value::String("second".into())])),
                (first, Tuple::new(vec![Value::String("first".into())])),
            ],
            &schema,
        )
        .unwrap();
    assert_eq!(durable(&db, "prepared"), newer);
    let writes = groups(&db, before);
    let row_group = writes
        .iter()
        .find(|g| g.keys.contains(&format!("data:prepared:{first}")))
        .unwrap();
    assert!(row_group.keys.contains(&format!("data:prepared:{second}")));
    assert!(row_group.counters.contains(&("counter:prepared".into(), newer)));
}

#[test]
fn concurrent_multi_table_counter_commits_use_one_shared_lock_order() {
    let temp = tempfile::tempdir().unwrap();
    let db = std::sync::Arc::new(open(config(temp.path(), false, WalSyncModeConfig::GroupCommit)));
    db.execute("CREATE TABLE counter_a (n INT)").unwrap();
    db.execute("CREATE TABLE counter_b (n INT)").unwrap();
    let start = std::sync::Arc::new(std::sync::Barrier::new(2));
    let (send, receive) = std::sync::mpsc::sync_channel(2);
    let mut threads = Vec::new();
    for reverse in [false, true] {
        let db = std::sync::Arc::clone(&db);
        let start = std::sync::Arc::clone(&start);
        let send = send.clone();
        threads.push(std::thread::spawn(move || {
            let txn = db.storage.begin_transaction().unwrap();
            if reverse {
                txn.stage_row_counter("counter_b", 200).unwrap();
                txn.stage_row_counter("counter_a", 20).unwrap();
            } else {
                txn.stage_row_counter("counter_a", 10).unwrap();
                txn.stage_row_counter("counter_b", 100).unwrap();
            }
            start.wait();
            send.send(txn.commit_with_timestamp(db.storage.next_timestamp()))
                .unwrap();
        }));
    }
    for _ in 0..2 {
        receive
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("multi-table counter locks do not deadlock")
            .unwrap();
    }
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(durable(&db, "counter_a"), 20);
    assert_eq!(durable(&db, "counter_b"), 200);
}

#[cfg(feature = "internal-tests")]
#[test]
fn failed_counter_commits_release_ordering_locks_before_the_next_allocator() {
    for mode in [
        WalSyncModeConfig::Sync,
        WalSyncModeConfig::Async,
        WalSyncModeConfig::GroupCommit,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let db = std::sync::Arc::new(open(config(temp.path(), false, mode)));
        db.execute("CREATE TABLE refused_mixed (n INT)").unwrap();
        let txn = db.storage.begin_transaction().unwrap();
        txn.stage_row_counter("refused_mixed", 50).unwrap();
        let sequence = db.storage.db().latest_sequence_number();
        db.storage.debug_set_write_group_ceiling(Some(64));
        let result = txn.commit_with_timestamp(db.storage.next_timestamp());
        db.storage.debug_set_write_group_ceiling(None);
        assert!(result.is_err());
        assert_eq!(db.storage.db().latest_sequence_number(), sequence);
        assert_eq!(durable(&db, "refused_mixed"), 0);
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let next = {
            let db = std::sync::Arc::clone(&db);
            std::thread::spawn(move || send.send(db.storage.next_row_id("refused_mixed")).unwrap())
        };
        let value = receive
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the failed commit released its counter/table locks")
            .unwrap();
        next.join().unwrap();
        assert_eq!(value, 51, "failed allocation leaves a gap and cannot be reused");
        assert_eq!(durable(&db, "refused_mixed"), value);
    }
}
