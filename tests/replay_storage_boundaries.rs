//! PROPOSED regression source: syntax/API reviewed only, not compiled or run.
//! Uses private TempDir/key files; no environment mutation, network or live paths.
use heliosdb_nano::{
    storage::{WalOperation, WalSyncMode, WriteAheadLog},
    Config, EmbeddedDatabase, Tuple, Value,
};

type Rows = Vec<Vec<String>>;
const TABLE: &str = "replay_bounds";
const SELECT: &str = "SELECT id, v FROM replay_bounds ORDER BY id";

fn primary() -> EmbeddedDatabase {
    let mut config = Config::in_memory();
    config.storage.wal_enabled = true;
    config.storage.logical_wal_per_statement = true;
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    EmbeddedDatabase::with_config(config).unwrap()
}

fn schema(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE replay_bounds (id INT PRIMARY KEY, v TEXT UNIQUE)")
        .unwrap();
}

fn rows(db: &EmbeddedDatabase, sql: &str) -> Rows {
    db.query(sql, &[])
        .unwrap()
        .into_iter()
        .map(|row| row.values.iter().map(ToString::to_string).collect())
        .collect()
}

// Inspect RocksDB bytes directly so a stale SQL or row-cache hit cannot conceal
// an accidental mutation. Keep table data and its allocator in one snapshot.
fn raw_snapshot(db: &EmbeddedDatabase) -> Vec<(Vec<u8>, Vec<u8>)> {
    let prefix = format!("data:{TABLE}:").into_bytes();
    let raw = db.storage.db();
    let mut snapshot = Vec::new();
    for item in raw.prefix_iterator(&prefix) {
        let (key, value) = item.unwrap();
        if !key.starts_with(&prefix) {
            break;
        }
        snapshot.push((key.to_vec(), value.to_vec()));
    }
    let counter = format!("counter:{TABLE}").into_bytes();
    if let Some(value) = raw.get(&counter).unwrap() {
        snapshot.push((counter, value));
    }
    snapshot.sort();
    snapshot
}

fn capture(db: &EmbeddedDatabase, sql: &str) -> Vec<WalOperation> {
    let before = db.storage.wal_lsn().unwrap();
    db.execute(sql).unwrap();
    let reader = WriteAheadLog::open(db.storage.db(), WalSyncMode::Async).unwrap();
    let entries = reader.replay_from(before).unwrap();
    assert!(!entries.is_empty());
    entries.into_iter().map(|entry| entry.operation).collect()
}

fn one_row(operations: &[WalOperation]) -> WalOperation {
    let rows: Vec<_> = operations
        .iter()
        .filter(|op| matches!(op, WalOperation::Insert { .. } | WalOperation::Update { .. }))
        .cloned()
        .collect();
    assert_eq!(rows.len(), 1);
    rows.into_iter().next().unwrap()
}

fn apply(db: &EmbeddedDatabase, operations: &[WalOperation]) {
    for operation in operations {
        db.storage.apply_replicated_operation(operation.clone()).unwrap();
    }
}

fn change_tuple(operation: &WalOperation, change: impl FnOnce(&mut Tuple)) -> WalOperation {
    let mut operation = operation.clone();
    match &mut operation {
        WalOperation::Insert { tuple, .. } | WalOperation::Update { tuple, .. } => {
            let mut decoded: Tuple = bincode::deserialize(tuple).unwrap();
            change(&mut decoded);
            *tuple = bincode::serialize(&decoded).unwrap();
        }
        _ => panic!("fixture requires a row operation"),
    }
    operation
}

#[cfg(feature = "encryption")]
#[test]
fn encrypted_live_replay_survives_reopen_and_preserves_allocator() {
    let fixture = tempfile::tempdir().unwrap();
    let key_path = fixture.path().join("test-only.key");
    std::fs::write(&key_path, [0x71u8; 32]).unwrap();
    let mut config = Config::default();
    config.storage.memory_only = false;
    config.storage.path = Some(fixture.path().join("standby"));
    // No local logical WAL: reopen must inspect the applied durable rows, not
    // reexecute an independent target-side history created by fixture setup.
    config.storage.wal_enabled = false;
    config.encryption.enabled = true;
    config.encryption.key_source = heliosdb_nano::KeySource::File(key_path);
    let source = primary();
    schema(&source);
    let target = EmbeddedDatabase::with_config(config.clone()).unwrap();
    schema(&target);
    assert!(target.storage.is_encrypted());
    let mut operations = Vec::new();
    for sql in [
        "INSERT INTO replay_bounds VALUES (1, 'secret-one')",
        "INSERT INTO replay_bounds VALUES (2, 'secret-two')",
        "INSERT INTO replay_bounds VALUES (3, 'secret-three')",
        "UPDATE replay_bounds SET v = 'secret-updated' WHERE id = 2",
        "DELETE FROM replay_bounds WHERE id = 1",
    ] {
        let step = capture(&source, sql);
        apply(&target, &step);
        apply(&target, &step); // adjacent duplicate delivery, never reorder history
        operations.extend(step);
    }
    assert!(operations.iter().any(|op| matches!(op, WalOperation::Insert { .. })));
    assert!(operations.iter().any(|op| matches!(op, WalOperation::Update { .. })));
    assert!(operations.iter().any(|op| matches!(op, WalOperation::Delete { .. })));
    assert_eq!(rows(&target, SELECT), rows(&source, SELECT));
    let persisted = target.storage.scan_table(TABLE).unwrap();
    let high = persisted.iter().map(|tuple| tuple.row_id.unwrap()).max().unwrap();
    for tuple in &persisted {
        let key = format!("data:{TABLE}:{}", tuple.row_id.unwrap()).into_bytes();
        let decoded = target.storage.get(&key).unwrap().unwrap();
        let raw = target.storage.db().get(&key).unwrap().unwrap();
        assert_ne!(raw, decoded, "replayed data must pass through TDE sealing");
        assert_eq!(bincode::deserialize::<Tuple>(&decoded).unwrap().values, tuple.values);
    }
    assert!(high > 1, "fixture must exceed the stale counter value");
    target
        .storage
        .apply_replicated_operation(WalOperation::UpdateCounter {
            table_name: TABLE.into(),
            new_value: 1,
        })
        .unwrap();
    let counter = format!("counter:{TABLE}").into_bytes();
    let decoded = target.storage.get(&counter).unwrap().unwrap();
    assert!(bincode::deserialize::<u64>(&decoded).unwrap() >= high);
    assert_ne!(target.storage.db().get(&counter).unwrap().unwrap(), decoded);
    target.flush().unwrap();
    drop(target);
    let reopened = EmbeddedDatabase::with_config(config).unwrap();
    assert_eq!(rows(&reopened, SELECT), rows(&source, SELECT));
    reopened
        .execute("INSERT INTO replay_bounds VALUES (4, 'secret-four')")
        .unwrap();
    let persisted = reopened.storage.scan_table(TABLE).unwrap();
    let next = persisted
        .iter()
        .find(|tuple| tuple.values[0] == Value::Int4(4))
        .unwrap();
    assert!(
        next.row_id.unwrap() > high,
        "reopen allocation must not overwrite replayed rows"
    );
    assert_eq!(persisted.len(), 3);
    assert_eq!(
        rows(&reopened, "SELECT id FROM replay_bounds WHERE v = 'secret-updated'"),
        vec![vec!["2".to_string()]]
    );
}

#[test]
fn malformed_live_rows_fail_without_mutating_rows_or_indexes() {
    let source = primary();
    let target = primary();
    for db in [&source, &target] {
        schema(db);
        db.execute("INSERT INTO replay_bounds VALUES (1, 'kept')").unwrap();
    }
    let operation = one_row(&capture(&source, "INSERT INTO replay_bounds VALUES (2, 'new')"));
    let before = rows(&target, SELECT);
    let raw_before = raw_snapshot(&target);
    let tuples_before = target.storage.scan_table(TABLE).unwrap();
    let pk_before = target.storage.art_indexes().pk_index_len(TABLE);
    assert_eq!(pk_before, Some(tuples_before.len()));
    let mut cases = Vec::new();
    for key in [
        "data:other:2",
        "data:replay_bounds:0",
        "data:replay_bounds:02",
        "data:replay_bounds:18446744073709551615",
    ] {
        let mut bad = operation.clone();
        match &mut bad {
            WalOperation::Insert { key: target_key, .. } | WalOperation::Update { key: target_key, .. } => {
                *target_key = key.as_bytes().to_vec()
            }
            _ => panic!("fixture requires a row operation"),
        }
        cases.push(bad);
    }
    cases.push(change_tuple(&operation, |tuple| {
        tuple.values.pop();
    }));
    cases.push(change_tuple(&operation, |tuple| {
        tuple.row_id = Some(999);
    }));
    for reference in [
        Value::DictRef { dict_id: 12 },
        Value::CasRef { hash: [7; 32] },
        Value::ColumnarRef,
    ] {
        cases.push(change_tuple(&operation, |tuple| tuple.values[1] = reference));
    }
    let mut invalid_bytes = operation.clone();
    match &mut invalid_bytes {
        WalOperation::Insert { tuple, .. } | WalOperation::Update { tuple, .. } => *tuple = vec![255],
        _ => panic!("fixture requires a row operation"),
    }
    cases.push(invalid_bytes);
    for bad in cases {
        assert!(
            target.storage.apply_replicated_operation(bad.clone()).is_err(),
            "must refuse {bad:?}"
        );
        assert_eq!(rows(&target, SELECT), before);
        assert_eq!(raw_snapshot(&target), raw_before);
        assert_eq!(target.storage.scan_table(TABLE).unwrap(), tuples_before);
        assert_eq!(target.storage.art_indexes().pk_index_len(TABLE), pk_before);
        assert_eq!(target.storage.scan_table(TABLE).unwrap().len(), 1);
        assert!(rows(&target, "SELECT id FROM replay_bounds WHERE v = 'new'").is_empty());
        assert_eq!(
            rows(&target, "SELECT id FROM replay_bounds WHERE v = 'kept'"),
            vec![vec!["1".to_string()]]
        );
    }
    target.storage.apply_replicated_operation(operation).unwrap();
    assert_eq!(rows(&target, SELECT), rows(&source, SELECT));
}

#[test]
fn conflicting_replay_restores_pk_and_unique_membership() {
    let source = primary();
    let target = primary();
    for db in [&source, &target] {
        schema(db);
        db.execute("INSERT INTO replay_bounds VALUES (1, 'one')").unwrap();
        db.execute("INSERT INTO replay_bounds VALUES (2, 'two')").unwrap();
    }
    let operation = one_row(&capture(
        &source,
        "UPDATE replay_bounds SET id = 3, v = 'three' WHERE id = 1",
    ));
    let before = rows(&target, SELECT);
    let raw_before = raw_snapshot(&target);
    let tuples_before = target.storage.scan_table(TABLE).unwrap();
    let pk_before = target.storage.art_indexes().pk_index_len(TABLE);
    assert_eq!(pk_before, Some(tuples_before.len()));
    for conflict in [
        change_tuple(&operation, |tuple| tuple.values[0] = Value::Int4(2)),
        change_tuple(&operation, |tuple| tuple.values[1] = Value::String("two".into())),
    ] {
        assert!(target.storage.apply_replicated_operation(conflict).is_err());
        assert_eq!(rows(&target, SELECT), before);
        assert_eq!(raw_snapshot(&target), raw_before);
        assert_eq!(target.storage.scan_table(TABLE).unwrap(), tuples_before);
        assert_eq!(target.storage.art_indexes().pk_index_len(TABLE), pk_before);
        for (id, value) in [(1, "one"), (2, "two")] {
            assert_eq!(
                rows(&target, &format!("SELECT v FROM replay_bounds WHERE id = {id}")),
                vec![vec![format!("'{value}'")]]
            );
            assert_eq!(
                rows(&target, &format!("SELECT id FROM replay_bounds WHERE v = '{value}'")),
                vec![vec![id.to_string()]]
            );
        }
        assert!(rows(&target, "SELECT id FROM replay_bounds WHERE id = 3").is_empty());
        assert!(rows(&target, "SELECT id FROM replay_bounds WHERE v = 'three'").is_empty());
    }
    // A valid application must still claim keys tentatively held by failed attempts.
    target.storage.apply_replicated_operation(operation.clone()).unwrap();
    target.storage.apply_replicated_operation(operation).unwrap();
    assert_eq!(rows(&target, SELECT), rows(&source, SELECT));
    target.execute("INSERT INTO replay_bounds VALUES (1, 'one')").unwrap();
    assert_eq!(target.storage.scan_table(TABLE).unwrap().len(), 3);
}
