//! Public owning-row APIs: cap refusal versus native Err, with and without application.
//! Requires the separate cfg-only source18 native-outcome seam; no observer is added here.
#![cfg(feature = "internal-tests")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
use heliosdb_nano::storage::branch_catalog::MergeFailSite;
use heliosdb_nano::{Config, EmbeddedDatabase, Error, Tuple, Value};
use rocksdb::{IteratorMode, ReadOptions, WriteBatchIterator};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug)]
enum Fault {
    Cap,
    NativeUnchanged,
    NativeApplied,
    Applied,
}
#[derive(Clone, Copy, Debug)]
enum Insert {
    Common,
    Fast,
    Prepared,
}
#[derive(Clone, Copy, Debug)]
enum Family {
    Text,
    Params,
}
#[derive(Clone, Copy, Debug)]
enum Change {
    Move,
    Payload,
    Delete,
    DeletePk,
}
fn config(path: &Path, history: bool) -> Config {
    let mut c = Config::default();
    c.storage.path = Some(path.to_path_buf());
    c.storage.memory_only = false;
    c.storage.time_travel_enabled = history;
    c.storage.wal_enabled = true;
    c.storage.logical_wal_per_statement = false;
    c.storage.version_gc_interval_secs = Some(0);
    c.storage.wal_checkpoint_interval_entries = 0;
    c.storage.wal_checkpoint_interval_secs = 0;
    c.storage.wal_checkpoint_on_close = false;
    c.storage.flush_on_close = false;
    c
}
fn open(path: &Path, history: bool) -> EmbeddedDatabase {
    let db = EmbeddedDatabase::with_config(config(path, history)).unwrap();
    db.storage.set_index_snapshots_on_close(false);
    db.storage.set_row_counter_flush_on_close(false);
    db
}
fn exec(db: &EmbeddedDatabase, family: Family, sql: &str) -> heliosdb_nano::Result<u64> {
    match family {
        Family::Text => db.execute(sql),
        Family::Params => db.execute_params(sql, &[]),
    }
}
fn values(id: i32, u: i32, p: &str) -> Vec<Value> {
    vec![Value::Int4(id), Value::Int4(u), Value::String(p.into())]
}
fn original() -> Vec<Vec<Value>> {
    vec![values(1, 10, "one"), values(2, 20, "two")]
}
fn query(db: &EmbeddedDatabase, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql, &[]).unwrap().into_iter().map(|r| r.values).collect()
}
fn image(db: &EmbeddedDatabase) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let raw = db.storage.db();
    let mut ro = ReadOptions::default();
    ro.set_total_order_seek(true);
    raw.iterator_opt(IteratorMode::Start, ro)
        .map(|r| {
            let (k, v) = r.unwrap();
            (k.to_vec(), v.to_vec())
        })
        .collect()
}
fn heap(db: &EmbeddedDatabase) -> BTreeMap<u64, Vec<Value>> {
    image(db)
        .into_iter()
        .filter_map(|(k, v)| {
            let suffix = k.strip_prefix(b"data:t:")?;
            let id = std::str::from_utf8(suffix).unwrap().parse().unwrap();
            let row: Tuple = bincode::deserialize(&v).unwrap();
            Some((id, row.values))
        })
        .collect()
}
fn sorted(rows: impl IntoIterator<Item = Vec<Value>>) -> Vec<Vec<Value>> {
    let mut rows: Vec<_> = rows.into_iter().collect();
    rows.sort_by_key(|r| match r[0] {
        Value::Int4(v) => v,
        _ => panic!("integer PK"),
    });
    rows
}
fn truth(db: &EmbeddedDatabase, expected: &[Vec<Value>]) {
    let raw = heap(db);
    assert_eq!(sorted(raw.values().cloned()), expected, "raw owning heap");
    assert!(!db.storage.has_uncommitted_writes(), "no census may conceal stale ART");
    assert_eq!(query(db, "SELECT id,u,payload FROM t ORDER BY id"), expected);
    assert_eq!(
        query(db, "SELECT COUNT(*) FROM t"),
        vec![vec![Value::Int8(expected.len() as i64)]]
    );
    for (position, column, domain) in [
        (0, "id", vec![1, 2, 3, 4, 11, 999]),
        (1, "u", vec![10, 20, 30, 40, 999]),
    ] {
        let index = db.storage.art_indexes().find_column_index("t", column);
        if column == "id" {
            assert!(index.is_some(), "PK ART remains registered");
        }
        for key in domain {
            let wanted: Vec<_> = raw
                .iter()
                .filter(|(_, r)| r[position] == Value::Int4(key))
                .map(|(id, _)| *id)
                .collect();
            let encoded = heliosdb_nano::storage::ArtIndexManager::encode_key(&[Value::Int4(key)]);
            if let Some(index) = &index {
                let mut actual = db.storage.art_indexes().index_get_all(index, &encoded);
                actual.sort_unstable();
                assert_eq!(actual, wanted, "direct ART {column}={key}");
            }
            let wanted = sorted(raw.values().filter(|r| r[position] == Value::Int4(key)).cloned());
            assert_eq!(
                query(
                    db,
                    &format!("SELECT id,u,payload FROM t WHERE {column}={key} ORDER BY id")
                ),
                wanted
            );
        }
    }
}
fn setup(db: &EmbeddedDatabase) {
    setup_shape(db, false);
}
fn setup_shape(db: &EmbeddedDatabase, pk_only: bool) {
    db.execute(if pk_only {
        "CREATE TABLE t(id INT PRIMARY KEY,u INT,payload TEXT)"
    } else {
        "CREATE TABLE t(id INT PRIMARY KEY,u INT UNIQUE,payload TEXT)"
    })
    .unwrap();
    db.execute("CREATE TABLE healthy(id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO t VALUES(1,10,'one'),(2,20,'two')").unwrap();
    for _ in 0..4 {
        truth(db, &original());
    }
}
fn arm(db: &EmbeddedDatabase, fault: Fault) {
    match fault {
        Fault::Cap => db.storage.debug_set_write_group_ceiling(Some(0)),
        Fault::NativeUnchanged => db.storage.write_gate().debug_fail_next_native_write_before_apply(),
        Fault::NativeApplied => db.storage.write_gate().debug_fail_next_native_write_after_apply(),
        Fault::Applied => db.storage.debug_fail_next_applied_commit(),
    }
}
#[derive(Default)]
struct Keys(Vec<Vec<u8>>);
impl WriteBatchIterator for Keys {
    fn put(&mut self, k: Box<[u8]>, _: Box<[u8]>) {
        self.0.push(k.into_vec());
    }
    fn delete(&mut self, k: Box<[u8]>) {
        self.0.push(k.into_vec());
    }
}
fn group_proof(db: &EmbeddedDatabase, before: u64, applied: bool, wal: bool) {
    let raw = db.storage.db();
    let after = raw.latest_sequence_number();
    if !applied {
        assert_eq!(after, before, "owning group changed nothing");
        return;
    }
    assert!(after > before, "fault actually applied the target native group");
    let mut count = 0;
    let mut iter = raw.get_updates_since(before).unwrap();
    for item in iter.by_ref() {
        let (first, batch) = item.unwrap();
        if first > after {
            break;
        }
        assert_eq!(first, before + 1, "owning group starts at the exact next sequence");
        let mut keys = Keys::default();
        batch.iterate(&mut keys);
        assert!(
            keys.0.iter().any(|k| k.starts_with(b"data:t:")),
            "no incidental metadata failure accepted"
        );
        if wal {
            assert!(
                keys.0.iter().any(|k| k.starts_with(b"wal:entries:")),
                "logical entry shares owning row group"
            );
        }
        count += 1;
    }
    assert_eq!(count, 1, "one actual owning native row group, no error repair writes");
}
fn fault_error(result: heliosdb_nano::Result<()>, fault: Fault) {
    let error = result.expect_err("armed owning fault must fire");
    let text = error.to_string();
    let marker = match fault {
        Fault::Cap => "write group",
        Fault::NativeUnchanged => "injected native write error before apply; outcome is unknown",
        Fault::NativeApplied => "injected native write error after apply; outcome is unknown",
        Fault::Applied => "injected durability failure after the commit applied",
    };
    assert!(text.contains(marker), "{fault:?}: {error}");
}
fn fenced_without_writes(db: &EmbeddedDatabase) {
    let before = image(db);
    let seq = db.storage.db().latest_sequence_number();
    for result in [
        db.query("SELECT id,u,payload FROM t ORDER BY id", &[]),
        db.query("SELECT id FROM t WHERE id=1", &[]),
        db.query_params("SELECT COUNT(*) FROM t", &[]),
    ] {
        let error = result.expect_err("unknown owning outcome must fence reads");
        assert!(matches!(error, Error::Storage(_)), "typed storage fence: {error}");
        assert!(error.to_string().contains("reopen"), "{error}");
    }
    for family in [Family::Text, Family::Params] {
        assert!(exec(db, family, "INSERT INTO t VALUES(999,999,'must refuse')").is_err());
    }
    assert_eq!(
        image(db),
        before,
        "read/refused-write/error handling performs no native repair"
    );
    assert_eq!(db.storage.db().latest_sequence_number(), seq);
    db.execute("INSERT INTO healthy VALUES(7)").unwrap();
    assert_eq!(query(db, "SELECT id FROM healthy"), vec![vec![Value::Int4(7)]]);
}
fn reopen(path: &Path, history: bool, expected: &[Vec<Value>]) {
    reopen_as(path, history, expected, expected);
}
fn reopen_as(path: &Path, history: bool, ro_expected: &[Vec<Value>], expected: &[Vec<Value>]) {
    {
        let ro = EmbeddedDatabase::open_read_only(path).unwrap();
        let seq = ro.storage.db().latest_sequence_number();
        let bytes = image(&ro);
        truth(&ro, ro_expected);
        assert_eq!(image(&ro), bytes);
        assert_eq!(ro.storage.db().latest_sequence_number(), seq);
    }
    let db = open(path, history);
    truth(&db, expected);
    for row in expected {
        let Value::Int4(id) = row[0] else { panic!("int") };
        let Value::Int4(u) = row[1] else { panic!("int") };
        assert!(db
            .execute(&format!("INSERT INTO t VALUES({id},999,'duplicate PK')"))
            .is_err());
        if db.storage.art_indexes().find_column_index("t", "u").is_some() {
            assert!(db
                .execute(&format!("INSERT INTO t VALUES(999,{u},'duplicate unique')"))
                .is_err());
        }
    }
    truth(&db, expected);
}
fn insert(db: &EmbeddedDatabase, route: Insert) -> heliosdb_nano::Result<()> {
    let schema = db.storage.catalog().get_table_schema("t")?;
    match route {
        Insert::Common => {
            db.storage.insert_tuple("t", Tuple::new(values(3, 30, "new")))?;
        }
        Insert::Fast => {
            db.storage
                .insert_tuple_fast("t", Tuple::new(values(3, 30, "new")), &schema)?;
        }
        Insert::Prepared => {
            let rows = [(3, 30), (4, 40)]
                .into_iter()
                .map(|(id, u)| (db.storage.next_row_id_volatile("t"), Tuple::new(values(id, u, "new"))))
                .collect();
            assert_eq!(db.storage.insert_prepared_tuples_fast_batch("t", rows, &schema)?, 2);
        }
    }
    Ok(())
}
fn inserts(fault: Fault) {
    let mut cells = 0;
    for history in [false, true] {
        for route in [Insert::Common, Insert::Fast, Insert::Prepared] {
            let dir = tempfile::tempdir().unwrap();
            let mut expected = original();
            if matches!(fault, Fault::NativeApplied) {
                expected.push(values(3, 30, "new"));
                if matches!(route, Insert::Prepared) {
                    expected.push(values(4, 40, "new"));
                }
            }
            {
                let db = open(dir.path(), history);
                setup(&db);
                let before = db.storage.db().latest_sequence_number();
                let bytes = image(&db);
                arm(&db, fault);
                let result = insert(&db, route);
                db.storage.debug_set_write_group_ceiling(None);
                fault_error(result, fault);
                assert_eq!(sorted(heap(&db).into_values()), expected);
                group_proof(
                    &db,
                    before,
                    matches!(fault, Fault::NativeApplied),
                    matches!(route, Insert::Common),
                );
                if matches!(fault, Fault::Cap) {
                    assert_eq!(image(&db), bytes);
                    truth(&db, &expected);
                    insert(&db, route).unwrap();
                    expected.push(values(3, 30, "new"));
                    if matches!(route, Insert::Prepared) {
                        expected.push(values(4, 40, "new"));
                    }
                    truth(&db, &expected);
                } else {
                    fenced_without_writes(&db);
                }
            }
            reopen(dir.path(), history, &expected);
            cells += 1;
        }
    }
    eprintln!("direct-row-outcome insert_{fault:?}={cells}");
}
fn changed(change: Change) -> Vec<Vec<Value>> {
    match change {
        Change::Move => vec![values(2, 20, "two"), values(11, 10, "one")],
        Change::Payload => vec![values(1, 10, "changed"), values(2, 20, "two")],
        Change::Delete | Change::DeletePk => vec![values(2, 20, "two")],
    }
}
fn changes(fault: Fault) {
    let mut cells = 0;
    for history in [false, true] {
        for family in [Family::Text, Family::Params] {
            for change in [Change::Move, Change::Payload, Change::Delete, Change::DeletePk] {
                let dir = tempfile::tempdir().unwrap();
                let mut expected = if matches!(fault, Fault::NativeApplied) {
                    changed(change)
                } else {
                    original()
                };
                let sql = match change {
                    Change::Move => "UPDATE t SET id=11 WHERE id=1",
                    Change::Payload => "UPDATE t SET payload='changed' WHERE id=1",
                    Change::Delete | Change::DeletePk => "DELETE FROM t WHERE id=1",
                };
                {
                    let db = open(dir.path(), history);
                    setup_shape(&db, matches!(change, Change::DeletePk));
                    let before = db.storage.db().latest_sequence_number();
                    let bytes = image(&db);
                    arm(&db, fault);
                    let result = exec(&db, family, sql).map(|n| assert_eq!(n, 1));
                    db.storage.debug_set_write_group_ceiling(None);
                    fault_error(result, fault);
                    assert_eq!(sorted(heap(&db).into_values()), expected);
                    group_proof(&db, before, matches!(fault, Fault::NativeApplied), false);
                    if matches!(fault, Fault::Cap) {
                        assert_eq!(image(&db), bytes);
                        truth(&db, &expected);
                        assert_eq!(exec(&db, family, sql).unwrap(), 1);
                        expected = changed(change);
                        truth(&db, &expected);
                    } else {
                        fenced_without_writes(&db);
                    }
                }
                reopen(dir.path(), history, &expected);
                cells += 1;
            }
        }
    }
    eprintln!("direct-row-outcome fast_sql_{fault:?}={cells}");
}
fn merge(fault: Fault) {
    let mut cells = 0;
    for history in [false, true] {
        for family in [Family::Text, Family::Params] {
            let dir = tempfile::tempdir().unwrap();
            let expected = if matches!(fault, Fault::NativeApplied | Fault::Applied) {
                changed(Change::Move)
            } else {
                original()
            };
            {
                let db = open(dir.path(), history);
                setup(&db);
                db.storage
                    .create_branch("work", Some("main"), Default::default())
                    .unwrap();
                exec(&db, family, "USE BRANCH work").unwrap();
                exec(&db, family, "UPDATE t SET id=11 WHERE id=1").unwrap();
                exec(&db, family, "USE BRANCH main").unwrap();
                truth(&db, &original());
                let gate = Arc::clone(db.storage.write_gate());
                let engine = Arc::clone(&db.storage);
                let captured = Arc::new(Mutex::new(None));
                let record = Arc::clone(&captured);
                db.storage.debug_on_merge_site(
                    MergeFailSite::TableClaimed,
                    Some(Arc::new(move || {
                        *record.lock().unwrap() = Some(engine.db().latest_sequence_number());
                        match fault {
                            Fault::Cap => engine.debug_set_write_group_ceiling(Some(0)),
                            Fault::NativeUnchanged => gate.debug_fail_next_native_write_before_apply(),
                            Fault::NativeApplied => gate.debug_fail_next_native_write_after_apply(),
                            Fault::Applied => engine.debug_fail_next_applied_commit(),
                        }
                    })),
                );
                let result = exec(&db, family, "MERGE BRANCH work INTO main").map(|_| ());
                db.storage.debug_on_merge_site(MergeFailSite::TableClaimed, None);
                db.storage.debug_set_write_group_ceiling(None);
                fault_error(result, fault);
                let before = captured.lock().unwrap().expect("actual owning table boundary reached");
                assert_eq!(sorted(heap(&db).into_values()), expected);
                group_proof(
                    &db,
                    before,
                    matches!(fault, Fault::NativeApplied | Fault::Applied),
                    true,
                );
                if matches!(fault, Fault::NativeApplied | Fault::NativeUnchanged) {
                    fenced_without_writes(&db);
                } else {
                    truth(&db, &expected);
                }
            }
            // A writable reopen resumes the durable merge intent, even when
            // its first table landing never applied. Read-only open is write-free.
            reopen_as(dir.path(), history, &expected, &changed(Change::Move));
            cells += 1;
        }
    }
    eprintln!("direct-row-outcome merge_{fault:?}={cells}");
}
#[test]
fn direct_insert_cap_refusal_keeps_keys_and_retries() {
    inserts(Fault::Cap)
}
#[test]
fn direct_insert_native_error_without_apply_fences() {
    inserts(Fault::NativeUnchanged)
}
#[test]
fn direct_insert_native_error_after_apply_fences() {
    inserts(Fault::NativeApplied)
}
#[test]
fn fast_sql_row_cap_refusal_keeps_keys_and_retries() {
    changes(Fault::Cap)
}
#[test]
fn fast_sql_row_native_error_without_apply_fences() {
    changes(Fault::NativeUnchanged)
}
#[test]
fn fast_sql_row_native_error_after_apply_fences() {
    changes(Fault::NativeApplied)
}
#[test]
fn main_merge_cap_refusal_keeps_old_claims() {
    merge(Fault::Cap)
}
#[test]
fn main_merge_known_applied_error_keeps_new_claims() {
    merge(Fault::Applied)
}
#[test]
fn main_merge_native_error_without_apply_fences() {
    merge(Fault::NativeUnchanged)
}
#[test]
fn main_merge_native_error_after_apply_fences() {
    merge(Fault::NativeApplied)
}
