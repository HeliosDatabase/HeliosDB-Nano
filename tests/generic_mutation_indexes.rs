//! Generic storage mutations own their ART memberships and one final MV delta.
use heliosdb_nano::storage::{ArtIndexManager, Catalog, MvDelta, MvDeltaOperation};
use heliosdb_nano::{Config, EmbeddedDatabase, Tuple, Value};
use std::collections::BTreeMap;
use std::path::Path;

fn open(path: &Path) -> EmbeddedDatabase {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    config.storage.version_gc_interval_secs = Some(0);
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    EmbeddedDatabase::with_config(config).unwrap()
}
fn tuple(id: i32, unique: i32, manual: i32, payload: &str) -> Tuple {
    Tuple::new(vec![
        Value::Int4(id),
        Value::Int4(unique),
        Value::Int4(manual),
        Value::String(payload.into()),
    ])
}
fn seed(db: &EmbeddedDatabase) -> Vec<Tuple> {
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, u INT UNIQUE, m INT, payload TEXT)")
        .unwrap();
    db.execute("CREATE INDEX t_m ON t (m)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 10, 100, 'one'), (2, 20, 200, 'two')")
        .unwrap();
    let mut rows = db.storage.scan_table("t").unwrap();
    rows.sort_by_key(|row| int(&row.values[0]));
    rows
}
fn int(value: &Value) -> i64 {
    match value {
        Value::Int2(value) => i64::from(*value),
        Value::Int4(value) => i64::from(*value),
        Value::Int8(value) => *value,
        other => panic!("integer: {other:?}"),
    }
}
fn count(db: &EmbeddedDatabase, predicate: &str) -> i64 {
    int(&db.query(&format!("SELECT COUNT(*) FROM t {predicate}"), &[]).unwrap()[0].values[0])
}
fn assert_key(db: &EmbeddedDatabase, column: &str, value: i32, expected: i64) {
    assert_eq!(
        count(db, &format!("WHERE {column} = {value}")),
        expected,
        "{column}={value}"
    );
}
fn deltas(db: &EmbeddedDatabase) -> BTreeMap<Vec<u8>, MvDelta> {
    let prefix = b"delta:t:";
    db.storage
        .db()
        .prefix_iterator(prefix)
        .map(|item| {
            let (key, value) = item.unwrap();
            assert!(key.starts_with(prefix));
            (key.into_vec(), bincode::deserialize(&value).unwrap())
        })
        .collect()
}
fn new_deltas(db: &EmbeddedDatabase, before: &BTreeMap<Vec<u8>, MvDelta>) -> Vec<MvDelta> {
    deltas(db)
        .into_iter()
        .filter(|(key, _)| !before.contains_key(key))
        .map(|(_, delta)| delta)
        .collect()
}

#[test]
fn direct_update_maintains_old_new_pk_unique_manual_keys_and_delete_reuse_after_reopen() {
    let temp = tempfile::tempdir().unwrap();
    {
        let db = open(temp.path());
        let original = seed(&db);
        // Exercise both ART and result-cache answers before the storage API write.
        for _ in 0..12 {
            assert_eq!(count(&db, ""), 2);
            assert_key(&db, "id", 1, 1);
        }
        assert_eq!(
            db.storage
                .update_tuples_branch_aware("t", vec![(original[0].row_id.unwrap(), tuple(11, 110, 101, "changed"))])
                .unwrap(),
            1
        );
        assert_eq!(count(&db, ""), 2);
        for (column, old, new) in [("id", 1, 11), ("u", 10, 110), ("m", 100, 101)] {
            assert_key(&db, column, old, 0);
            assert_key(&db, column, new, 1);
        }
        assert_eq!(
            db.storage
                .delete_tuples_branch_aware("t", vec![original[0].row_id.unwrap(), original[0].row_id.unwrap()])
                .unwrap(),
            1
        );
        assert_eq!(count(&db, ""), 1);
        for (column, value) in [("id", 11), ("u", 110), ("m", 101)] {
            assert_key(&db, column, value, 0);
        }
        db.execute("INSERT INTO t VALUES (11, 110, 101, 'reused')").unwrap();
        assert_eq!(count(&db, ""), 2);
        assert_key(&db, "id", 11, 1);
    }
    let db = open(temp.path());
    assert_eq!(count(&db, ""), 2);
    assert_key(&db, "id", 11, 1);
    assert_key(&db, "u", 110, 1);
    assert_key(&db, "m", 101, 1);
}

#[test]
fn multirow_unique_and_pk_swap_is_claimed_as_one_change() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let old = seed(&db);
    assert_eq!(
        db.storage
            .update_tuples_branch_aware(
                "t",
                vec![
                    (old[0].row_id.unwrap(), tuple(2, 20, 201, "from-one")),
                    (old[1].row_id.unwrap(), tuple(1, 10, 101, "from-two")),
                ]
            )
            .unwrap(),
        2
    );
    assert_eq!(count(&db, ""), 2);
    for column in ["id", "u"] {
        for value in if column == "id" { [1, 2] } else { [10, 20] } {
            assert_key(&db, column, value, 1);
        }
    }
    assert_key(&db, "m", 100, 0);
    assert_key(&db, "m", 200, 0);
    assert_key(&db, "m", 101, 1);
    assert_key(&db, "m", 201, 1);
    assert_eq!(
        db.query("SELECT payload FROM t WHERE id = 1", &[]).unwrap()[0].values,
        vec![Value::String("from-two".into())]
    );
    let before = db.storage.scan_table("t").unwrap();
    let error = db
        .storage
        .update_tuples_branch_aware("t", vec![(old[0].row_id.unwrap(), tuple(1, 10, 999, "duplicate"))])
        .unwrap_err();
    assert!(error.to_string().contains("constraint"), "{error}");
    assert_eq!(db.storage.scan_table("t").unwrap(), before);
    assert_eq!(count(&db, ""), 2);
}

#[test]
fn repeated_ids_are_last_wins_before_claims_and_produce_one_old_to_final_delta() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let old = seed(&db);
    let before = deltas(&db);
    let row = old[0].row_id.unwrap();
    assert_eq!(
        db.storage
            .update_tuples_branch_aware(
                "t",
                vec![
                    (row, tuple(2, 20, 200, "discarded duplicate key")),
                    (row, tuple(3, 30, 300, "last")),
                ]
            )
            .unwrap(),
        1
    );
    assert_eq!(count(&db, ""), 2);
    assert_key(&db, "id", 1, 0);
    assert_key(&db, "id", 3, 1);
    let changes = new_deltas(&db, &before);
    assert_eq!(changes.len(), 1);
    match &changes[0].operation {
        MvDeltaOperation::Update { old_tuple, new_tuple } => {
            assert_eq!(old_tuple.values, old[0].values);
            assert_eq!(new_tuple.values, tuple(3, 30, 300, "last").values);
        }
        other => panic!("one update delta: {other:?}"),
    }
}

#[test]
fn writer_preimage_bypasses_a_stale_or_poisoned_row_cache() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let old = seed(&db);
    let row = old[0].row_id.unwrap();
    db.storage
        .row_cache()
        .put("t", row, tuple(999, 999, 999, "stale cache"));
    let before = deltas(&db);
    db.storage
        .update_tuples_branch_aware("t", vec![(row, tuple(11, 110, 101, "actual update"))])
        .unwrap();
    assert_key(&db, "id", 1, 0);
    assert_key(&db, "id", 11, 1);
    assert_key(&db, "u", 10, 0);
    assert_key(&db, "u", 110, 1);
    let changes = new_deltas(&db, &before);
    assert!(
        matches!(&changes[0].operation, MvDeltaOperation::Update { old_tuple, .. } if old_tuple.values == old[0].values)
    );
    db.storage
        .row_cache()
        .put("t", row, tuple(888, 888, 888, "wrong delete preimage"));
    db.storage.delete_tuples_branch_aware("t", vec![row]).unwrap();
    assert_eq!(count(&db, ""), 1);
    assert_key(&db, "id", 11, 0);
    assert_key(&db, "u", 110, 0);
    db.execute("INSERT INTO t VALUES (11, 110, 101, 'reuse')").unwrap();
}

#[cfg(feature = "internal-tests")]
#[test]
fn refusal_after_claim_repairs_swapped_keys_and_preserves_deltas() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let old = seed(&db);
    let before = deltas(&db);
    let storage = db.storage.clone();
    db.storage.debug_generic_after_claim(move || {
        assert!(storage.has_uncommitted_writes_for_table("t"));
        assert!(storage.has_uncommitted_index_removals_for_table("t"));
        storage.debug_set_write_group_ceiling(Some(64));
    });
    let result = db.storage.update_tuples_branch_aware(
        "t",
        vec![
            (old[0].row_id.unwrap(), tuple(2, 20, 201, "one")),
            (old[1].row_id.unwrap(), tuple(1, 10, 101, "two")),
        ],
    );
    db.storage.debug_set_write_group_ceiling(None);
    assert!(result.is_err());
    assert_eq!(
        deltas(&db).keys().collect::<Vec<_>>(),
        before.keys().collect::<Vec<_>>()
    );
    assert_eq!(count(&db, ""), 2);
    assert_key(&db, "m", 100, 1);
    assert_key(&db, "m", 200, 1);
    assert_key(&db, "m", 101, 0);
    assert!(!db.storage.has_uncommitted_writes());
    db.storage
        .update_tuples_branch_aware("t", vec![(old[0].row_id.unwrap(), tuple(3, 30, 300, "valid later"))])
        .unwrap();
    assert_key(&db, "id", 3, 1);
}

#[cfg(feature = "internal-tests")]
#[test]
fn applied_then_durability_error_keeps_committed_index_state_and_mv_delta() {
    let temp = tempfile::tempdir().unwrap();
    {
        let db = open(temp.path());
        let old = seed(&db);
        let before = deltas(&db);
        db.storage.debug_fail_next_applied_commit();
        let error = db
            .storage
            .update_tuples_branch_aware("t", vec![(old[0].row_id.unwrap(), tuple(11, 110, 101, "applied"))])
            .unwrap_err();
        assert!(error.to_string().contains("after the commit applied"));
        assert_eq!(count(&db, ""), 2);
        assert_key(&db, "id", 1, 0);
        assert_key(&db, "id", 11, 1);
        assert_key(&db, "u", 110, 1);
        assert_eq!(new_deltas(&db, &before).len(), 1);
        db.storage.debug_fail_next_applied_commit();
        assert!(db
            .storage
            .delete_tuples_branch_aware("t", vec![old[1].row_id.unwrap()])
            .is_err());
        assert_eq!(count(&db, ""), 1);
        assert_key(&db, "id", 2, 0);
        assert_key(&db, "u", 20, 0);
        db.execute("INSERT INTO t VALUES (2, 20, 200, 'reused after error')")
            .unwrap();
    }
    let db = open(temp.path());
    assert_eq!(count(&db, ""), 2);
    assert_key(&db, "id", 11, 1);
    assert_key(&db, "id", 2, 1);
}

#[cfg(feature = "internal-tests")]
#[test]
fn unreadable_reconciliation_retires_indexes_and_blocks_every_direct_write_route() {
    let temp = tempfile::tempdir().unwrap();
    {
        let db = open(temp.path());
        let old = seed(&db);
        let row = old[0].row_id.unwrap();
        let raw = db.storage.db();
        let key = format!("data:t:{row}").into_bytes();
        let saved = raw.get(&key).unwrap().unwrap();
        let corrupt_key = key.clone();
        let storage = db.storage.clone();
        let raw_hook = raw.clone();
        db.storage.debug_generic_after_claim(move || {
            // Public raw handle deliberately simulates an unreadable durable
            // row arriving after the claim; the actual commit is then refused.
            raw_hook.put(corrupt_key, b"unreadable row").unwrap();
            storage.debug_set_write_group_ceiling(Some(64));
        });
        assert!(db
            .storage
            .update_tuples_branch_aware("t", vec![(row, tuple(11, 110, 101, "rejected"))])
            .is_err());
        db.storage.debug_set_write_group_ceiling(None);
        assert_eq!(
            db.storage
                .art_indexes()
                .pk_index_contains("t", &ArtIndexManager::encode_key(&[Value::Int4(1)])),
            None,
            "unusable trees are absent, never authoritative empty"
        );
        let schema = Catalog::new(&db.storage).get_table_schema("t").unwrap();
        assert!(db.storage.insert_tuple("t", tuple(99, 99, 99, "common")).is_err());
        assert!(db
            .storage
            .insert_tuple_versioned("t", tuple(99, 99, 99, "versioned"))
            .is_err());
        assert!(db
            .storage
            .insert_tuple_fast("t", tuple(99, 99, 99, "fast"), &schema)
            .is_err());
        assert!(db
            .storage
            .insert_prepared_tuples_fast_batch("t", vec![(100, tuple(99, 99, 99, "prepared"))], &schema)
            .is_err());
        let txn = db.storage.begin_transaction().unwrap();
        txn.put(
            b"data:t:101".to_vec(),
            bincode::serialize(&tuple(99, 99, 99, "raw transaction")).unwrap(),
        )
        .unwrap();
        assert!(txn.commit().is_err());
        assert!(db
            .storage
            .delete_tuples_branch_aware("t", vec![old[1].row_id.unwrap()])
            .is_err());
        db.execute("CREATE TABLE healthy (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO healthy VALUES (1)").unwrap();
        // Simulate offline repair of the corrupt bytes. Only a fresh open may
        // rebuild the retired indexes and lift the table's write quarantine.
        raw.put(key, saved).unwrap();
    }
    let db = open(temp.path());
    assert_eq!(count(&db, ""), 2);
    db.execute("INSERT INTO t VALUES (99, 99, 99, 'rebuilt')").unwrap();
    assert_eq!(count(&db, ""), 3);
}

#[test]
fn generic_cache_invalidation_preserves_a_repeatable_read_snapshot() {
    use heliosdb_nano::session::IsolationLevel;
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let old = seed(&db);
    let reader = db
        .create_session("generic-reader", IsolationLevel::RepeatableRead)
        .unwrap();
    db.begin_transaction_for_session(reader).unwrap();
    let before = db
        .query_in_session(reader, "SELECT payload FROM t WHERE id = 1", &[])
        .unwrap();
    for _ in 0..12 {
        assert_key(&db, "id", 1, 1);
    }
    db.storage
        .update_tuples_branch_aware("t", vec![(old[0].row_id.unwrap(), tuple(11, 110, 101, "current"))])
        .unwrap();
    assert_key(&db, "id", 1, 0);
    assert_key(&db, "id", 11, 1);
    assert_eq!(
        db.query_in_session(reader, "SELECT payload FROM t WHERE id = 1", &[])
            .unwrap(),
        before
    );
    db.rollback_transaction_for_session(reader).unwrap();
    db.destroy_session(reader).unwrap();
}

#[cfg(feature = "internal-tests")]
#[test]
fn a_warmed_sql_cache_is_fenced_before_the_generic_commit_barrier_lifts() {
    use std::sync::{mpsc, Arc};
    use std::time::Duration;
    const SQL: &str = "SELECT COUNT(*) FROM t";
    for refuse in [false, true] {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let old = seed(&db);
        for _ in 0..12 {
            assert_eq!(int(&db.query(SQL, &[]).unwrap()[0].values[0]), 2);
        }
        let (warm_tx, warm_rx) = mpsc::channel();
        db.debug_next_hot_cache_lookup(move |hit| warm_tx.send(hit).unwrap());
        db.query(SQL, &[]).unwrap();
        assert!(
            warm_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            "the pre-write SQL result must actually be cached"
        );

        let (ready_tx, ready_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let writer_db = Arc::clone(&db);
        let row = old[0].row_id.unwrap();
        let writer = std::thread::spawn(move || {
            if refuse {
                let storage = writer_db.storage.clone();
                writer_db
                    .storage
                    .debug_generic_after_claim(move || storage.debug_set_write_group_ceiling(Some(64)));
            }
            writer_db.storage.debug_next_commit_after_barrier(move || {
                ready_tx.send(()).unwrap();
                resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            });
            writer_db.storage.delete_tuples_branch_aware("t", vec![row])
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // The write was attempted and end_commit already returned, but the
        // generic caller has not resumed from commit or released its table.
        // Observe the actual SQL cache lookup now: v1 returned its old warm hit.
        let (probe_tx, probe_rx) = mpsc::channel();
        let reader_db = Arc::clone(&db);
        let reader = std::thread::spawn(move || {
            reader_db.debug_next_hot_cache_lookup(move |hit| probe_tx.send(hit).unwrap());
            reader_db.query(SQL, &[])
        });
        let probe = probe_rx.recv_timeout(Duration::from_secs(5));
        db.storage.debug_set_write_group_ceiling(None);
        resume_tx.send(()).unwrap();
        let written = writer.join().unwrap();
        let read = reader.join().unwrap().unwrap();
        assert!(
            !probe.unwrap(),
            "an old cache hit survived the lifted commit barrier (refuse={refuse})"
        );
        assert_eq!(written.is_err(), refuse);
        let expected = if refuse { 2 } else { 1 };
        assert_eq!(int(&read[0].values[0]), expected);
        assert_eq!(
            int(&db.query(SQL, &[]).unwrap()[0].values[0]),
            expected,
            "late cache fill is stamped with the attempted-write epoch"
        );
    }
}
