//! Durable MV allocator evidence survives delayed commits and an empty delta log.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use chrono::{Duration, Utc};
use heliosdb_nano::storage::{MvDelta, MvDeltaOperation, MvDeltaTracker, WriteGate};
use heliosdb_nano::{Config, EmbeddedDatabase, Tuple, Value};
use rocksdb::{IteratorMode, Options, ReadOptions, WriteBatch, WriteBatchIterator, DB};
use std::path::Path;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

fn raw(path: &Path) -> Arc<DB> {
    let mut options = Options::default();
    options.create_if_missing(true);
    Arc::new(DB::open(&options, path).unwrap())
}
fn tuple(id: i32) -> Tuple {
    Tuple::new(vec![Value::Int4(id)])
}
fn keys(db: &DB, prefix: &[u8]) -> Vec<Vec<u8>> {
    let mut options = ReadOptions::default();
    options.set_total_order_seek(true);
    db.iterator_opt(
        rocksdb::IteratorMode::From(prefix, rocksdb::Direction::Forward),
        options,
    )
    .map(|item| item.unwrap().0.into_vec())
    .take_while(|key| key.starts_with(prefix))
    .collect()
}

#[derive(Default)]
struct Group(Vec<Vec<u8>>);
impl WriteBatchIterator for Group {
    fn put(&mut self, key: Box<[u8]>, _: Box<[u8]>) {
        self.0.push(key.into_vec());
    }
    fn delete(&mut self, key: Box<[u8]>) {
        self.0.push(key.into_vec());
    }
}
fn groups_after(db: &DB, after: u64) -> Vec<Group> {
    let mut groups = Vec::new();
    let mut updates = db.get_updates_since(after).unwrap();
    for item in updates.by_ref() {
        let (first, batch) = item.unwrap();
        if first <= after {
            continue;
        }
        let mut group = Group::default();
        batch.iterate(&mut group);
        groups.push(group);
    }
    updates.status().unwrap();
    groups
}
fn assert_floor_in_one_group(db: &DB, before: u64) {
    let groups = groups_after(db, before);
    assert_eq!(groups.len(), 1, "allocator metadata has no separate write group");
    assert!(groups[0].0.iter().any(|key| key.starts_with(b"delta:")));
    assert!(groups[0].0.iter().any(|key| key.starts_with(b"meta:delta:floor:")));
}

/// Compiles against the pre-fix public API: ID 1 commits after IDs 2..65 were
/// committed AND purged. IDs 1 and 65 deliberately share an allocator lane.
#[test]
fn delayed_producer_after_gc_cannot_lower_the_reopened_allocator() {
    let temp = tempfile::tempdir().unwrap();
    {
        let db = raw(temp.path());
        let gate = WriteGate::open();
        let tracker = MvDeltaTracker::new(Arc::clone(&db), Arc::clone(&gate)).unwrap();
        let mut delayed = WriteBatch::default();
        tracker
            .stage_insert(&mut delayed, "table:with:colons", 1, tuple(1))
            .unwrap();
        let mut overtaking = WriteBatch::default();
        for id in 2..=65 {
            tracker
                .stage_insert(&mut overtaking, "t", id, tuple(id as i32))
                .unwrap();
        }
        gate.write(&db, overtaking, None).unwrap();
        assert_eq!(tracker.purge_deltas_before(Utc::now() + Duration::days(1)).unwrap(), 64);
        assert!(keys(&db, b"delta:").is_empty());
        gate.write(&db, delayed, None).unwrap();
    }
    let db = raw(temp.path());
    let gate = WriteGate::open();
    let tracker = MvDeltaTracker::new(Arc::clone(&db), gate).unwrap();
    assert_eq!(tracker.current_delta_id(), 65);
    tracker.record_insert("t", 66, tuple(66)).unwrap();
    assert!(db.get(b"delta:t:00000000000000000066").unwrap().is_some());
    assert!(db
        .get(b"delta:table:with:colons:00000000000000000001")
        .unwrap()
        .is_some());
}

#[test]
fn simultaneous_same_lane_producers_and_cleanup_keep_the_empty_log_floor_after_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let maximum;
    {
        let db = raw(temp.path());
        let gate = WriteGate::open();
        let tracker = Arc::new(MvDeltaTracker::new(Arc::clone(&db), Arc::clone(&gate)).unwrap());
        let mut delayed = WriteBatch::default();
        tracker.stage_insert(&mut delayed, "t", 1, tuple(1)).unwrap();
        let mut overtaking = WriteBatch::default();
        for id in 2..=65 {
            tracker
                .stage_insert(&mut overtaking, "t", id, tuple(id as i32))
                .unwrap();
        }
        gate.write(&db, overtaking, None).unwrap();
        let rounds = Arc::new(std::sync::Barrier::new(2));
        let producer_tracker = Arc::clone(&tracker);
        let producer_db = Arc::clone(&db);
        let producer_gate = Arc::clone(&gate);
        let producer_rounds = Arc::clone(&rounds);
        let producer = std::thread::spawn(move || {
            let mut errors = Vec::new();
            for round in 0..16 {
                // Every batch includes all lanes, including lane1 whose ID65
                // can simultaneously be removed by the cleanup worker.
                let mut batch = WriteBatch::default();
                for row in 0..64 {
                    if let Err(error) =
                        producer_tracker.stage_insert(&mut batch, "t", 100 + round * 64 + row, tuple(row as i32))
                    {
                        errors.push(error.to_string());
                    }
                }
                producer_rounds.wait();
                if round == 0 {
                    if let Err(error) = producer_gate.write(&producer_db, std::mem::take(&mut delayed), None) {
                        errors.push(error.to_string());
                    }
                }
                if let Err(error) = producer_gate.write(&producer_db, batch, None) {
                    errors.push(error.to_string());
                }
                producer_rounds.wait();
            }
            errors
        });
        let cleanup_tracker = Arc::clone(&tracker);
        let cleanup = std::thread::spawn(move || {
            let mut errors = Vec::new();
            for _ in 0..16 {
                rounds.wait();
                if let Err(error) = cleanup_tracker.purge_deltas_before(Utc::now() + Duration::days(1)) {
                    errors.push(error.to_string());
                }
                rounds.wait();
            }
            errors
        });
        assert!(producer.join().unwrap().is_empty());
        assert!(cleanup.join().unwrap().is_empty());
        maximum = tracker.current_delta_id();
        assert_eq!(maximum, 65 + 16 * 64);
        tracker.purge_deltas_before(Utc::now() + Duration::days(1)).unwrap();
        assert!(keys(&db, b"delta:").is_empty());
    }
    let db = raw(temp.path());
    let tracker = MvDeltaTracker::new(Arc::clone(&db), WriteGate::open()).unwrap();
    assert_eq!(tracker.current_delta_id(), maximum);
    tracker.record_insert("t", 9999, tuple(1)).unwrap();
    assert!(db.get(format!("delta:t:{:020}", maximum + 1)).unwrap().is_some());
}

#[test]
fn legacy_regressed_marker_recovers_retained_key_ids_without_writing_the_image() {
    let temp = tempfile::tempdir().unwrap();
    {
        // Simulate bytes produced by an older binary, before permanent floors.
        let db = raw(temp.path());
        let delta = MvDelta::new(
            "legacy".into(),
            9,
            MvDeltaOperation::Insert { tuple: tuple(9) },
            UNIX_EPOCH,
            0,
        );
        db.put(
            b"delta:legacy:00000000000000000999",
            bincode::serialize(&delta).unwrap(),
        )
        .unwrap();
        db.put(b"meta:delta:last_id", 2u64.to_le_bytes()).unwrap();
    }
    {
        let db = Arc::new(DB::open_for_read_only(&Options::default(), temp.path(), false).unwrap());
        let before = db.latest_sequence_number();
        let gate = WriteGate::closed("physical snapshot");
        let tracker = MvDeltaTracker::new(Arc::clone(&db), Arc::clone(&gate)).unwrap();
        assert_eq!(tracker.current_delta_id(), 999);
        assert_eq!(db.latest_sequence_number(), before);
        assert_eq!(gate.refusals(), 0);
        assert!(keys(&db, b"meta:delta:floor:").is_empty());
    }
    let db = raw(temp.path());
    let tracker = MvDeltaTracker::new(Arc::clone(&db), WriteGate::open()).unwrap();
    tracker.record_insert("legacy", 10, tuple(10)).unwrap();
    assert!(db.get(b"delta:legacy:00000000000000001000").unwrap().is_some());
}

#[test]
fn point_and_range_cleanup_preserve_bounded_floors_even_when_every_delta_is_gone() {
    let temp = tempfile::tempdir().unwrap();
    {
        let db = raw(temp.path());
        let gate = WriteGate::open();
        let tracker = MvDeltaTracker::new(Arc::clone(&db), Arc::clone(&gate)).unwrap();
        let mut batch = WriteBatch::default();
        for id in 1..=130 {
            tracker.stage_insert(&mut batch, "t", id, tuple(id as i32)).unwrap();
        }
        gate.write(&db, batch, None).unwrap();
        assert_eq!(keys(&db, b"meta:delta:floor:").len(), 64);
        // Normal GC is point deletion; also exercise raw range cleanup that
        // spans BOTH the delta records and their supposedly permanent metadata.
        gate.delete(&db, b"meta:delta:floor:02", None).unwrap();
        gate.put(&db, b"meta:delta:last_id", 1u64.to_le_bytes(), None).unwrap();
        let mut cleanup = WriteBatch::default();
        cleanup.delete_range(b"delta:".as_slice(), b"meta:delta;".as_slice());
        gate.write(&db, cleanup, None).unwrap();
        assert!(keys(&db, b"delta:").is_empty());
        assert_eq!(keys(&db, b"meta:delta:floor:").len(), 64);
    }
    let db = raw(temp.path());
    let tracker = MvDeltaTracker::new(Arc::clone(&db), WriteGate::open()).unwrap();
    assert_eq!(tracker.current_delta_id(), 130);
    tracker.record_insert("t", 131, tuple(131)).unwrap();
    assert!(db.get(b"delta:t:00000000000000000131").unwrap().is_some());
}

#[test]
fn every_delta_producer_and_its_gc_floor_share_one_write_group() {
    let temp = tempfile::tempdir().unwrap();
    let db = raw(temp.path());
    let gate = WriteGate::open();
    let tracker = MvDeltaTracker::new(Arc::clone(&db), Arc::clone(&gate)).unwrap();
    for operation in 0..4 {
        let before = db.latest_sequence_number();
        match operation {
            0 => tracker
                .record_delta(MvDelta::new(
                    "t".into(),
                    1,
                    MvDeltaOperation::Insert { tuple: tuple(1) },
                    UNIX_EPOCH,
                    0,
                ))
                .unwrap(),
            1 => tracker.record_insert("t", 2, tuple(2)).unwrap(),
            2 => tracker.record_update("t", 2, tuple(2), tuple(3)).unwrap(),
            _ => tracker.record_delete("t", 2, tuple(3)).unwrap(),
        }
        assert_floor_in_one_group(&db, before);
    }
    let before = db.latest_sequence_number();
    let purged = tracker.purge_deltas_before(Utc::now() + Duration::days(1)).unwrap();
    let groups = groups_after(&db, before);
    assert_eq!(groups.len() as u64, purged);
    assert!(groups
        .iter()
        .all(|group| group.0.iter().any(|key| key.starts_with(b"meta:delta:floor:"))));
}

#[test]
fn separate_tracker_handles_share_ids_and_refused_final_metadata_writes_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let db = raw(temp.path());
    let gate = WriteGate::open();
    let one = MvDeltaTracker::new(Arc::clone(&db), Arc::clone(&gate)).unwrap();
    let two = MvDeltaTracker::new(Arc::clone(&db), Arc::clone(&gate)).unwrap();
    let mut first = WriteBatch::default();
    let mut second = WriteBatch::default();
    one.stage_insert(&mut first, "t", 1, tuple(1)).unwrap();
    two.stage_insert(&mut second, "t", 2, tuple(2)).unwrap();
    gate.write(&db, second, None).unwrap();
    gate.write(&db, first, None).unwrap();
    assert_eq!(keys(&db, b"delta:t:").len(), 2);
    let mut refused = WriteBatch::default();
    one.stage_insert(&mut refused, "t", 3, tuple(3)).unwrap();
    gate.debug_set_write_group_ceiling(Some(refused.size_in_bytes()));
    let before: Vec<_> = db.iterator(IteratorMode::Start).map(|item| item.unwrap()).collect();
    let sequence = db.latest_sequence_number();
    assert!(
        gate.write(&db, refused, None).is_err(),
        "the final floor also counts toward the cap"
    );
    assert_eq!(db.latest_sequence_number(), sequence);
    assert_eq!(
        db.iterator(IteratorMode::Start)
            .map(|item| item.unwrap())
            .collect::<Vec<_>>(),
        before
    );
}

#[test]
fn malformed_or_exhausted_allocator_evidence_cannot_wrap_into_an_existing_id() {
    for value in [vec![7], u64::MAX.to_le_bytes().to_vec()] {
        let temp = tempfile::tempdir().unwrap();
        let db = raw(temp.path());
        db.put(b"meta:delta:floor:63", &value).unwrap();
        let before = db.latest_sequence_number();
        let tracker = MvDeltaTracker::new(Arc::clone(&db), WriteGate::open());
        if value.len() != 8 {
            assert!(tracker.is_err());
        } else {
            let tracker = tracker.unwrap();
            assert_eq!(tracker.current_delta_id(), u64::MAX);
            assert!(tracker
                .record_insert("t", 1, tuple(1))
                .unwrap_err()
                .to_string()
                .contains("exhausted"));
        }
        assert_eq!(db.latest_sequence_number(), before);
        assert!(keys(&db, b"delta:").is_empty());
    }
}

#[test]
fn a_common_insert_refuses_delta_preparation_failure_before_any_row_effect() {
    for versioning in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.storage.path = Some(temp.path().to_path_buf());
        config.storage.memory_only = false;
        config.storage.wal_enabled = false;
        config.storage.time_travel_enabled = versioning;
        config.storage.version_gc_interval_secs = Some(0);
        let db = EmbeddedDatabase::with_config(config).unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
        let raw = db.storage.db();
        db.storage
            .write_gate()
            .put(&raw, b"meta:delta:floor:63", u64::MAX.to_le_bytes(), None)
            .unwrap();
        let before: Vec<_> = raw.iterator(IteratorMode::Start).map(|item| item.unwrap()).collect();
        let sequence = raw.latest_sequence_number();
        let error = db.storage.insert_tuple("t", tuple(1)).unwrap_err();
        assert!(error.to_string().contains("exhausted"), "{error}");
        assert_eq!(raw.latest_sequence_number(), sequence);
        assert_eq!(
            raw.iterator(IteratorMode::Start)
                .map(|item| item.unwrap())
                .collect::<Vec<_>>(),
            before
        );
        assert!(db.storage.scan_table("t").unwrap().is_empty());
        assert!(matches!(
            db.query("SELECT COUNT(*) FROM t", &[]).unwrap()[0].values[0],
            Value::Int8(0) | Value::Int4(0)
        ));
    }
}

#[test]
fn materialized_refresh_cleanup_retains_applied_ids_after_the_log_becomes_empty() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.storage.path = Some(temp.path().to_path_buf());
    config.storage.memory_only = false;
    config.storage.version_gc_interval_secs = Some(0);
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    let maximum;
    {
        let db = EmbeddedDatabase::with_config(config.clone()).unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO t VALUES (1)").unwrap();
        db.execute("CREATE MATERIALIZED VIEW mv AS SELECT * FROM t").unwrap();
        // A timestamped old delta is certain to fall before this refresh cutoff.
        db.storage
            .mv_delta_tracker()
            .record_delta(MvDelta::new(
                "t".into(),
                7,
                MvDeltaOperation::Insert { tuple: tuple(7) },
                UNIX_EPOCH,
                0,
            ))
            .unwrap();
        maximum = db.storage.mv_delta_tracker().current_delta_id();
        db.execute("REFRESH MATERIALIZED VIEW mv").unwrap();
        assert!(keys(&db.storage.db(), b"delta:").is_empty());
    }
    let db = EmbeddedDatabase::with_config(config).unwrap();
    assert!(db.storage.mv_delta_tracker().current_delta_id() >= maximum);
    db.storage.mv_delta_tracker().record_insert("t", 8, tuple(8)).unwrap();
    assert!(db.storage.mv_delta_tracker().current_delta_id() > maximum);
}
