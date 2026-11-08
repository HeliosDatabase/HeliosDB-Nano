//! Public owning-row APIs: cap refusal versus native Err, with and without application.
//! Requires the separate cfg-only source18 native-outcome seam; no observer is added here.
#![cfg(feature = "internal-tests")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
use crate::{Config, EmbeddedDatabase, Error, Tuple, Value};
use rocksdb::{IteratorMode, ReadOptions, WriteBatchIterator};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Copy, Debug)]
enum Fault {
    Cap,
    NativeUnchanged,
    NativeApplied,
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
            let encoded = crate::storage::ArtIndexManager::encode_key(&[Value::Int4(key)]);
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
fn fault_error(result: crate::Result<()>, fault: Fault) {
    let error = result.expect_err("armed owning fault must fire");
    let text = error.to_string();
    let marker = match fault {
        Fault::Cap => "write group",
        Fault::NativeUnchanged => "injected native write error before apply; outcome is unknown",
        Fault::NativeApplied => "injected native write error after apply; outcome is unknown",
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
    assert!(db.execute("INSERT INTO t VALUES(999,999,'must refuse')").is_err());
    assert!(db
        .execute_params("INSERT INTO t VALUES(999,999,'must refuse')", &[])
        .is_err());
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
fn copy_rows(db: &EmbeddedDatabase, session: Option<crate::session::SessionId>) -> crate::Result<()> {
    let rows = vec![
        vec![Some("3".into()), Some("30".into()), Some("new".into())],
        vec![Some("4".into()), Some("40".into()), Some("new".into())],
    ];
    let columns = ["id".into(), "u".into(), "payload".into()];
    let result = match session {
        Some(id) => db.copy_bulk_insert_for_session(id, "t", &columns, &rows),
        None => db.copy_bulk_insert("t", &columns, &rows),
    };
    assert_eq!(result.expect("actual COPY fast-batch funnel must be selected")?, 2);
    Ok(())
}
fn copies(fault: Fault) {
    let mut cells = 0;
    for history in [false, true] {
        for session_bound in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut expected = original();
            if matches!(fault, Fault::NativeApplied) {
                expected.extend([values(3, 30, "new"), values(4, 40, "new")]);
            }
            {
                let db = open(dir.path(), history);
                setup(&db);
                let session = session_bound.then(|| {
                    db.create_session("copy_owner", crate::session::IsolationLevel::ReadCommitted)
                        .unwrap()
                });
                let before = db.storage.db().latest_sequence_number();
                let bytes = image(&db);
                arm(&db, fault);
                let result = copy_rows(&db, session);
                db.storage.debug_set_write_group_ceiling(None);
                fault_error(result, fault);
                assert_eq!(sorted(heap(&db).into_values()), expected);
                group_proof(&db, before, matches!(fault, Fault::NativeApplied), false);
                if matches!(fault, Fault::Cap) {
                    assert_eq!(image(&db), bytes);
                    truth(&db, &expected);
                    copy_rows(&db, session).unwrap();
                    expected.extend([values(3, 30, "new"), values(4, 40, "new")]);
                    truth(&db, &expected);
                } else {
                    fenced_without_writes(&db);
                }
            }
            reopen(dir.path(), history, &expected);
            cells += 1;
        }
    }
    eprintln!("direct-row-outcome copy_{fault:?}={cells}");
}
#[test]
fn actual_copy_cap_refusal_preserves_keys_and_retries() {
    copies(Fault::Cap);
}
#[test]
fn actual_copy_native_error_without_apply_fences() {
    copies(Fault::NativeUnchanged);
}
#[test]
fn actual_copy_native_error_after_apply_fences() {
    copies(Fault::NativeApplied);
}
