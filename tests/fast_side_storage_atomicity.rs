//! Direct row writers keep physical sidecars, logical history and branch copies
//! in the same RocksDB group, including refusal and competing dictionary IDs.
use heliosdb_nano::storage::{dictionary::ColumnDictionary, Catalog};
use heliosdb_nano::{Config, EmbeddedDatabase, Result, Tuple, Value};
use rocksdb::{IteratorMode, WriteBatchIterator};
use std::sync::{Arc, Barrier};

fn config(path: &std::path::Path, history: bool) -> Config {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    config.storage.time_travel_enabled = history;
    config.storage.version_gc_interval_secs = Some(0);
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    config.replication.physical.enabled = true;
    config
}

fn row(id: i32, c: i64, tag: &str) -> Tuple {
    Tuple::new(vec![
        Value::Int4(id),
        Value::Int8(c),
        Value::String(tag.to_string()),
        Value::String(tag.repeat(1200)),
    ])
}

fn setup(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, c BIGINT STORAGE COLUMNAR, d TEXT STORAGE DICTIONARY, x TEXT STORAGE CONTENT_ADDRESSED)").unwrap();
}

struct Keys(Vec<Vec<u8>>);
impl WriteBatchIterator for Keys {
    fn put(&mut self, key: Box<[u8]>, _: Box<[u8]>) {
        self.0.push(key.into_vec());
    }
    fn delete(&mut self, key: Box<[u8]>) {
        self.0.push(key.into_vec());
    }
}
fn groups(db: &EmbeddedDatabase, after: u64) -> Vec<Vec<Vec<u8>>> {
    let raw = db.storage.db();
    let latest = raw.latest_sequence_number();
    raw.flush_wal(true).unwrap();
    let mut next = after + 1;
    let mut groups = Vec::new();
    let mut updates = raw.get_updates_since(after).unwrap();
    for update in updates.by_ref() {
        let (sequence, batch) = update.unwrap();
        if batch.is_empty() || sequence > latest {
            continue;
        }
        assert_eq!(sequence, next, "physical WAL must be gap-free");
        next = sequence + batch.len() as u64;
        let mut keys = Keys(Vec::new());
        batch.iterate(&mut keys);
        groups.push(keys.0);
    }
    updates.status().unwrap();
    assert_eq!(next, latest + 1);
    groups
}
fn assert_group(keys: &[Vec<u8>], history: bool) {
    for prefix in [b"data:t:".as_slice(), b"dict:t:d", b"cas:", b"col:t:c:", b"colz:t:c:"] {
        assert!(
            keys.iter().any(|key| key.starts_with(prefix)),
            "missing {}",
            String::from_utf8_lossy(prefix)
        );
    }
    if history {
        for prefix in [b"v:t:".as_slice(), b"v_idx:t:", b"snapshot:"] {
            assert!(keys.iter().any(|key| key.starts_with(prefix)), "missing history");
        }
    }
}
fn raw_state(db: &EmbeddedDatabase) -> Vec<(Vec<u8>, Vec<u8>)> {
    db.storage
        .db()
        .iterator(IteratorMode::Start)
        .map(|item| {
            let (key, value) = item.unwrap();
            (key.into_vec(), value.into_vec())
        })
        .collect()
}
fn fast_update(db: &EmbeddedDatabase, row_id: u64, tuple: Tuple) -> Result<u64> {
    let schema = Catalog::new(&db.storage).get_table_schema("t")?;
    let _row = db.storage.lock_row_for_fast_write("t", row_id)?;
    db.storage.update_tuple_fast_no_index("t", row_id, tuple, &schema, None)
}

#[test]
fn both_direct_insert_routes_commit_sidecars_with_the_row() {
    for history in [false, true] {
        for fast in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let db = EmbeddedDatabase::with_config(config(temp.path(), history)).unwrap();
            setup(&db);
            let schema = Catalog::new(&db.storage).get_table_schema("t").unwrap();
            let before = db.storage.db().latest_sequence_number();
            if fast {
                db.storage.insert_tuple_fast("t", row(1, 10, "old"), &schema).unwrap();
            } else {
                db.storage.insert_tuple_versioned("t", row(1, 10, "old")).unwrap();
            }
            let groups = groups(&db, before);
            assert_eq!(groups.len(), 1, "fast={fast}, history={history}: {groups:?}");
            assert_group(&groups[0], history);
            assert!(groups[0].iter().any(|key| key == b"counter:t"));
            assert_eq!(db.storage.scan_table("t").unwrap()[0].values, row(1, 10, "old").values);
        }
    }
}

#[test]
fn fast_update_captures_preimage_before_sidecars_and_freezes_child_in_one_group() {
    let temp = tempfile::tempdir().unwrap();
    {
        let db = EmbeddedDatabase::with_config(config(temp.path(), false)).unwrap();
        setup(&db);
        db.storage.insert_tuple_versioned("t", row(1, 10, "old")).unwrap();
    }
    let anchor = {
        let db = EmbeddedDatabase::with_config(config(temp.path(), true)).unwrap();
        db.execute("CREATE TABLE tick (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO tick VALUES (1)").unwrap();
        let anchor = db.storage.snapshot_manager().current_scn() - 1;
        db.execute("CREATE BRANCH child FROM main").unwrap();
        let before = db.storage.db().latest_sequence_number();
        fast_update(&db, 1, row(1, 20, "new")).unwrap();
        let groups = groups(&db, before);
        assert_eq!(groups.len(), 1, "{groups:?}");
        assert_group(&groups[0], true);
        assert!(
            groups[0].iter().any(|key| key.starts_with(b"bdata:")),
            "child freezes in the same group"
        );
        let old = db.query(&format!("SELECT * FROM t AS OF SCN {anchor}"), &[]).unwrap();
        assert_eq!(old[0].values, row(1, 10, "old").values);
        let child = db
            .with_request_branch_scope("child", || db.query("SELECT * FROM t", &[]))
            .unwrap();
        assert_eq!(child[0].values, old[0].values);
        anchor
    };
    let db = EmbeddedDatabase::with_config(config(temp.path(), true)).unwrap();
    assert_eq!(
        db.query(&format!("SELECT * FROM t AS OF SCN {anchor}"), &[]).unwrap()[0].values,
        row(1, 10, "old").values
    );
    assert_eq!(db.storage.scan_table("t").unwrap()[0].values, row(1, 20, "new").values);
}

#[test]
fn refused_direct_side_storage_writes_leave_rows_payloads_and_dictionary_unchanged() {
    for fast_insert in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let db = EmbeddedDatabase::with_config(config(temp.path(), true)).unwrap();
        setup(&db);
        db.storage.insert_tuple_versioned("t", row(1, 10, "old")).unwrap();
        db.execute("CREATE BRANCH child FROM main").unwrap();
        let schema = Catalog::new(&db.storage).get_table_schema("t").unwrap();
        for insert in [false, true] {
            let before = raw_state(&db);
            db.storage.debug_set_write_group_ceiling(Some(512));
            let result = if !insert {
                fast_update(&db, 1, row(1, 20, "rejected"))
            } else if fast_insert {
                db.storage.insert_tuple_fast("t", row(2, 20, "rejected"), &schema)
            } else {
                db.storage.insert_tuple_versioned("t", row(2, 20, "rejected"))
            };
            db.storage.debug_set_write_group_ceiling(None);
            assert!(result.is_err(), "the full physical group exceeds the ceiling");
            assert_eq!(raw_state(&db), before, "no prebatch sidecar or frozen copy may land");
        }
        fast_update(&db, 1, row(1, 30, "accepted")).unwrap();
        let dict: ColumnDictionary = bincode::deserialize(&db.storage.db().get(b"dict:t:d").unwrap().unwrap()).unwrap();
        assert!(!dict.contains("rejected"), "a refused plan must restore its cache IDs");
        assert!(dict.contains("accepted"));
        let child = db
            .with_request_branch_scope("child", || db.query("SELECT * FROM t", &[]))
            .unwrap();
        assert_eq!(child[0].values, row(1, 10, "old").values);
    }
}

#[test]
fn concurrent_fast_dictionary_updates_keep_every_committed_id() {
    let temp = tempfile::tempdir().unwrap();
    let db = EmbeddedDatabase::with_config(config(temp.path(), true)).unwrap();
    setup(&db);
    for id in 1..=8 {
        db.storage.insert_tuple_versioned("t", row(id, 0, "old")).unwrap();
    }
    let barrier = Arc::new(Barrier::new(8));
    let mut writers = Vec::new();
    for id in 1..=8 {
        let storage = Arc::clone(&db.storage);
        let barrier = Arc::clone(&barrier);
        writers.push(std::thread::spawn(move || {
            let schema = Catalog::new(&storage).get_table_schema("t").unwrap();
            barrier.wait();
            for version in 1..=5 {
                let _row = storage.lock_row_for_fast_write("t", id as u64).unwrap();
                storage
                    .update_tuple_fast_no_index(
                        "t",
                        id as u64,
                        row(id, version, &format!("row-{id}-{version}")),
                        &schema,
                        None,
                    )
                    .unwrap();
            }
        }));
    }
    for writer in writers {
        writer.join().unwrap();
    }
    drop(db);
    let db = EmbeddedDatabase::with_config(config(temp.path(), true)).unwrap();
    for actual in db.storage.scan_table("t").unwrap() {
        let Value::Int4(id) = actual.values[0] else {
            panic!("id")
        };
        assert_eq!(actual.values, row(id, 5, &format!("row-{id}-5")).values);
    }
}

#[test]
fn stale_raw_dictionary_transaction_refuses_id_collision_with_a_fast_writer() {
    let temp = tempfile::tempdir().unwrap();
    let db = EmbeddedDatabase::with_config(config(temp.path(), true)).unwrap();
    setup(&db);
    db.storage.insert_tuple_versioned("t", row(1, 10, "old")).unwrap();
    let mut dict: ColumnDictionary = bincode::deserialize(&db.storage.db().get(b"dict:t:d").unwrap().unwrap()).unwrap();
    dict.encode("pending");
    let txn = db.storage.begin_transaction().unwrap();
    txn.put(b"dict:t:d".to_vec(), bincode::serialize(&dict).unwrap())
        .unwrap();
    fast_update(&db, 1, row(1, 20, "committed")).unwrap();
    let before = raw_state(&db);
    let error = txn.commit().unwrap_err().to_string();
    assert!(
        error.contains("serialization failure") && error.contains("dictionary"),
        "{error}"
    );
    assert_eq!(raw_state(&db), before);
    assert_eq!(
        db.storage.scan_table("t").unwrap()[0].values,
        row(1, 20, "committed").values
    );
}

#[test]
fn fast_delete_keeps_cold_columnar_manifest_history_and_wal_in_its_row_group() {
    // Existing materialized history is retained, while a legitimate older row
    // first written with history off needs its logical preimage in this group.
    for (history, seed_history) in [(false, false), (true, true), (true, false)] {
        let temp = tempfile::tempdir().unwrap();
        if history && !seed_history {
            let db = EmbeddedDatabase::with_config(config(temp.path(), false)).unwrap();
            setup(&db);
            db.storage.insert_tuple_versioned("t", row(1, 10, "old")).unwrap();
        }
        let anchor;
        {
            let mut cfg = config(temp.path(), history);
            cfg.storage.logical_wal_per_statement = true;
            let db = EmbeddedDatabase::with_config(cfg).unwrap();
            if history == seed_history {
                setup(&db);
                db.storage.insert_tuple_versioned("t", row(1, 10, "old")).unwrap();
            }
            let versions_before: Vec<_> = raw_state(&db)
                .into_iter()
                .filter(|(key, _)| key.starts_with(b"v:t:1:"))
                .collect();
            assert_eq!(!versions_before.is_empty(), seed_history);
            db.execute("CREATE TABLE tick (id INT PRIMARY KEY)").unwrap();
            db.execute("INSERT INTO tick VALUES (1)").unwrap();
            anchor = db.storage.snapshot_manager().current_scn() - 1;
            let schema = Catalog::new(&db.storage).get_table_schema("t").unwrap();
            // Model a legitimate older/cold store. The delete itself must
            // finish this manifest, without a separate lazy-backfill group.
            db.storage.db().delete(b"colpm:t").unwrap();
            db.storage.db().delete(b"colp:t:0").unwrap();
            let before = db.storage.db().latest_sequence_number();
            {
                let _row = db.storage.lock_row_for_fast_write("t", 1).unwrap();
                assert_eq!(
                    db.storage
                        .delete_tuple_fast("t", 1, &row(1, 10, "old"), &schema)
                        .unwrap(),
                    1
                );
            }
            let groups = groups(&db, before);
            assert_eq!(
                groups.len(),
                1,
                "history={history}, seed_history={seed_history}: {groups:?}"
            );
            for prefix in [b"data:t:".as_slice(), b"col:t:c:", b"colz:t:c:", b"colpm:t", b"wal:"] {
                assert!(
                    groups[0].iter().any(|key| key.starts_with(prefix)),
                    "missing {} from history={history}, seed_history={seed_history}: {:?}",
                    String::from_utf8_lossy(prefix),
                    groups[0]
                );
            }
            let versions_after: Vec<_> = raw_state(&db)
                .into_iter()
                .filter(|(key, _)| key.starts_with(b"v:t:1:"))
                .collect();
            if history {
                for prefix in [b"v_idx:t:".as_slice(), b"snapshot:"] {
                    assert!(
                        groups[0].iter().any(|key| key.starts_with(prefix)),
                        "missing {} from {:?}",
                        String::from_utf8_lossy(prefix),
                        groups[0]
                    );
                }
                assert_eq!(
                    groups[0].iter().any(|key| key.starts_with(b"v:t:1:")),
                    !seed_history,
                    "only an unmaterialized prior image needs a new v: in the delete group"
                );
                assert!(
                    !versions_after.is_empty(),
                    "the old logical image must remain reconstructible"
                );
                if seed_history {
                    assert_eq!(
                        versions_after, versions_before,
                        "the retained old version must remain byte-identical"
                    );
                }
                for (_, bytes) in &versions_after {
                    let logical: Tuple = bincode::deserialize(bytes).unwrap();
                    assert_eq!(logical.values, row(1, 10, "old").values);
                }
                assert_eq!(
                    db.query(&format!("SELECT * FROM t AS OF SCN {anchor}"), &[]).unwrap()[0].values,
                    row(1, 10, "old").values
                );
                let deleted_at = db.storage.snapshot_manager().current_scn() - 1;
                assert!(
                    db.query(&format!("SELECT * FROM t AS OF SCN {deleted_at}"), &[])
                        .unwrap()
                        .is_empty(),
                    "the same-group history event must make the row absent at the delete snapshot"
                );
            } else {
                assert!(versions_after.is_empty());
            }
            assert!(db.query("SELECT * FROM t WHERE id=1", &[]).unwrap().is_empty());
            assert!(db.storage.scan_table("t").unwrap().is_empty());
            assert!(db.storage.db().get(b"colpm:t").unwrap().is_some());
        }
        let db = EmbeddedDatabase::with_config(config(temp.path(), history)).unwrap();
        assert!(db.storage.scan_table("t").unwrap().is_empty());
        if history {
            assert_eq!(
                db.query(&format!("SELECT * FROM t AS OF SCN {anchor}"), &[]).unwrap()[0].values,
                row(1, 10, "old").values
            );
            let deleted_at = db.storage.snapshot_manager().current_scn() - 1;
            assert!(db
                .query(&format!("SELECT * FROM t AS OF SCN {deleted_at}"), &[])
                .unwrap()
                .is_empty());
        }
    }
}

#[test]
fn a_refused_fast_side_storage_delete_keeps_heap_indexes_and_sidecars() {
    let temp = tempfile::tempdir().unwrap();
    let db = EmbeddedDatabase::with_config(config(temp.path(), true)).unwrap();
    setup(&db);
    db.storage.insert_tuple_versioned("t", row(1, 10, "old")).unwrap();
    let schema = Catalog::new(&db.storage).get_table_schema("t").unwrap();
    let before = raw_state(&db);
    let sequence = db.storage.db().latest_sequence_number();
    db.storage.debug_set_write_group_ceiling(Some(64));
    let result = {
        let _row = db.storage.lock_row_for_fast_write("t", 1).unwrap();
        db.storage.delete_tuple_fast("t", 1, &row(1, 10, "old"), &schema)
    };
    db.storage.debug_set_write_group_ceiling(None);
    let error = result.unwrap_err().to_string();
    // Embedded errors carry the emitter marker; the wire maps this marker to
    // SQLSTATE54000, including through the fast-delete Storage wrappers.
    assert!(
        error.contains(heliosdb_nano::BRANCH_MATERIALIZE_TOO_LARGE_MARKER),
        "{error}"
    );
    assert!(error.contains("nothing was written"), "{error}");
    assert_eq!(db.storage.db().latest_sequence_number(), sequence);
    assert_eq!(raw_state(&db), before);
    assert_eq!(
        db.query("SELECT * FROM t WHERE id=1", &[]).unwrap()[0].values,
        row(1, 10, "old").values
    );
    assert_eq!(db.storage.scan_table("t").unwrap()[0].values, row(1, 10, "old").values);
}
