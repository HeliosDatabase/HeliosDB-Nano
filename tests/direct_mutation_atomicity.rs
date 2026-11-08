//! Public generic mutations preserve one durable statement and historical images.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{Config, EmbeddedDatabase, Tuple, Value};

fn rows(db: &EmbeddedDatabase, at: Option<u64>) -> Vec<Vec<Value>> {
    let mut rows: Vec<_> = match at {
        Some(ts) => db.storage.scan_table_at_snapshot("mutations", ts).unwrap(),
        None => db.storage.scan_table("mutations").unwrap(),
    }
    .into_iter()
    .map(|row| row.values)
    .collect();
    rows.sort_by_key(|row| match row.first() {
        Some(Value::Int4(id)) => *id,
        _ => panic!("id"),
    });
    rows
}

fn seed(db: &EmbeddedDatabase) -> Vec<Tuple> {
    db.execute("CREATE TABLE mutations (id INT PRIMARY KEY, body TEXT STORAGE DICTIONARY, payload TEXT STORAGE CONTENT_ADDRESSED, amount BIGINT STORAGE COLUMNAR)").unwrap();
    db.execute("INSERT INTO mutations VALUES (1, 'first', 'a', 10), (2, 'second', 'b', 20)")
        .unwrap();
    db.storage.scan_table("mutations").unwrap()
}

#[test]
fn generic_mutations_preserve_logical_history_across_cache_eviction_and_reopen() {
    for logical_sync in [false, true] {
        let temp = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.storage.path = Some(temp.path().to_path_buf());
        config.storage.logical_wal_per_statement = logical_sync;
        let db = EmbeddedDatabase::with_config(config.clone()).unwrap();
        let original = seed(&db);
        let before_update = db.storage.current_timestamp();
        let original_values = rows(&db, None);
        let updates: Vec<_> = original
            .iter()
            .map(|row| {
                let mut new = row.clone();
                new.values[1] = Value::String("replacement".into());
                new.values[2] = Value::String("new payload".into());
                new.values[3] = Value::Int8(30);
                (row.row_id.unwrap(), new)
            })
            .collect();
        assert_eq!(db.storage.update_tuples_branch_aware("mutations", updates).unwrap(), 2);
        let after_update = db.storage.current_timestamp();
        let updated = rows(&db, None);
        assert_ne!(updated, original_values);
        db.storage.snapshot_manager().clear_cache();
        assert_eq!(rows(&db, Some(before_update)), original_values);
        assert_eq!(
            db.storage
                .delete_tuples_branch_aware("mutations", vec![original[0].row_id.unwrap()])
                .unwrap(),
            1
        );
        assert_eq!(rows(&db, None).len(), 1);
        db.storage.snapshot_manager().clear_cache();
        assert_eq!(rows(&db, Some(before_update)), original_values);
        assert_eq!(rows(&db, Some(after_update)), updated);
        drop(db);
        let reopened = EmbeddedDatabase::with_config(config).unwrap();
        assert_eq!(rows(&reopened, None).len(), 1);
        assert_eq!(rows(&reopened, Some(before_update)), original_values);
        assert_eq!(rows(&reopened, Some(after_update)), updated);
    }
}

#[cfg(feature = "internal-tests")]
#[test]
fn a_refused_generic_update_changes_no_row_or_side_storage() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let original = seed(&db);
    let before = rows(&db, None);
    let seq = db.storage.db().latest_sequence_number();
    let updates = original
        .into_iter()
        .map(|mut row| {
            row.values[1] = Value::String("large".repeat(2000));
            row.values[2] = Value::String("large content".repeat(2000));
            row.values[3] = Value::Int8(999);
            (row.row_id.unwrap(), row)
        })
        .collect();
    db.storage.debug_set_write_group_ceiling(Some(1024));
    let result = db.storage.update_tuples_branch_aware("mutations", updates);
    db.storage.debug_set_write_group_ceiling(None);
    assert!(result.is_err(), "oversized statement must actually be refused");
    assert_eq!(
        db.storage.db().latest_sequence_number(),
        seq,
        "no side write escaped the failed group"
    );
    db.storage.row_cache().clear();
    assert_eq!(rows(&db, None), before);
}

#[cfg(all(feature = "internal-tests", feature = "ha-tier1"))]
mod physical {
    use super::*;
    use heliosdb_nano::storage::{WalEntry, WalOperation};
    use rocksdb::WriteBatchIterator;

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
    fn groups(db: &EmbeddedDatabase, after: u64) -> Vec<Group> {
        let raw = db.storage.db();
        let mut updates = raw.get_updates_since(after).unwrap();
        let mut result = Vec::new();
        let latest = raw.latest_sequence_number();
        let mut next = after + 1;
        for update in updates.by_ref() {
            let (first, batch) = update.unwrap();
            if first > latest {
                break;
            }
            assert_eq!(first, next);
            next = first + batch.len() as u64;
            let mut group = Group::default();
            batch.iterate(&mut group);
            result.push(group);
        }
        updates.status().unwrap();
        assert_eq!(next, latest + 1, "inspect all retained groups");
        result
    }

    #[test]
    fn rows_side_data_and_logical_entries_of_generic_mutations_share_one_group() {
        for logical_sync in [false, true] {
            let temp = tempfile::TempDir::new().unwrap();
            let mut config = Config::default();
            config.storage.path = Some(temp.path().to_path_buf());
            config.storage.logical_wal_per_statement = logical_sync;
            config.replication.physical.enabled = true;
            let db = EmbeddedDatabase::with_config(config).unwrap();
            let original = seed(&db);
            for delete in [false, true] {
                let before = db.storage.db().latest_sequence_number();
                let changed = if delete {
                    db.storage
                        .delete_tuples_branch_aware("mutations", original.iter().map(|r| r.row_id.unwrap()).collect())
                        .unwrap()
                } else {
                    db.storage
                        .update_tuples_branch_aware(
                            "mutations",
                            original
                                .iter()
                                .map(|r| {
                                    let mut row = r.clone();
                                    row.values[3] = Value::Int8(99);
                                    (r.row_id.unwrap(), row)
                                })
                                .collect(),
                        )
                        .unwrap()
                };
                assert_eq!(changed, 2);
                let found = groups(&db, before);
                let owners: Vec<_> = found
                    .iter()
                    .filter(|g| g.0.iter().any(|(k, _)| k.starts_with("data:mutations:")))
                    .collect();
                assert_eq!(owners.len(), 1, "one physical group for every changed row: {found:?}");
                let group = owners[0];
                assert_eq!(
                    group.0.iter().filter(|(k, _)| k.starts_with("data:mutations:")).count(),
                    2
                );
                assert!(
                    group.0.iter().any(|(k, _)| k.starts_with("col:")),
                    "side storage shares the group: {found:?}"
                );
                assert!(
                    group.0.iter().any(|(k, _)| k.starts_with("snapshot:")),
                    "snapshot shares the group: {found:?}"
                );
                let entries: Vec<WalEntry> = group
                    .0
                    .iter()
                    .filter_map(|(k, v)| {
                        k.starts_with("wal:entries:")
                            .then(|| bincode::deserialize(v.as_ref().unwrap()).unwrap())
                    })
                    .collect();
                assert_eq!(entries.len(), 2, "exactly one entry per changed row: {found:?}");
                for entry in entries {
                    assert!(match entry.operation {
                        WalOperation::Delete { table, .. } => delete && table == "mutations",
                        WalOperation::Update { table, .. } => !delete && table == "mutations",
                        _ => false,
                    });
                }
            }
        }
    }
}
