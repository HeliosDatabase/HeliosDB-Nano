//! A failed sequence DDL must settle the owning database's already-warm cache.
#![cfg(feature = "internal-tests")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
use heliosdb_nano::storage::ddl_batch::{
    debug_fail_next_applied_commit, debug_fail_next_applied_commit_with, debug_fail_next_commit,
    debug_next_before_native_commit,
};
use heliosdb_nano::{Config, EmbeddedDatabase, Error, Value};
use rocksdb::{IteratorMode, ReadOptions, WriteBatchIterator};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug)]
enum Fault {
    Refused,
    Cap,
    NativeBefore,
    NativeAfter,
    Applied,
}
impl Fault {
    fn applied(self) -> bool {
        matches!(self, Self::NativeAfter | Self::Applied)
    }
    fn arm(self, db: &EmbeddedDatabase) {
        match self {
            Self::Refused => debug_fail_next_commit(),
            Self::Cap => db.storage.debug_set_write_group_ceiling(Some(0)),
            Self::Applied => debug_fail_next_applied_commit(),
            Self::NativeBefore | Self::NativeAfter => {
                let gate = Arc::clone(db.storage.write_gate());
                debug_next_before_native_commit(move || match self {
                    Self::NativeBefore => gate.debug_fail_next_native_write_before_apply(),
                    Self::NativeAfter => gate.debug_fail_next_native_write_after_apply(),
                    _ => unreachable!(),
                });
            }
        }
    }
    fn assert_error(self, result: heliosdb_nano::Result<()>) {
        let error = result.expect_err("fault at actual sequence owning native group");
        let expected = match self {
            Self::Refused => "write group was not written",
            Self::Cap => "write group",
            Self::Applied => "injected schema durability failure after the group applied",
            Self::NativeBefore => "injected native write error before apply; outcome is unknown",
            Self::NativeAfter => "injected native write error after apply; outcome is unknown",
        };
        assert!(error.to_string().contains(expected), "{self:?}: {error}");
    }
}
#[derive(Clone, Copy, Debug)]
enum Change {
    Restart,
    Increment,
    Drop,
}
impl Change {
    fn sql(self) -> &'static str {
        match self {
            Self::Restart => "ALTER SEQUENCE target RESTART WITH 500",
            Self::Increment => "ALTER SEQUENCE target INCREMENT BY 3",
            Self::Drop => "DROP SEQUENCE target",
        }
    }
}
fn open(path: &Path, history: bool) -> EmbeddedDatabase {
    let mut c = Config::default();
    c.storage.path = Some(path.into());
    c.storage.memory_only = false;
    c.storage.time_travel_enabled = history;
    c.storage.version_gc_interval_secs = Some(0);
    c.storage.wal_checkpoint_interval_entries = 0;
    c.storage.wal_checkpoint_interval_secs = 0;
    c.storage.wal_checkpoint_on_close = false;
    c.storage.flush_on_close = false;
    let db = EmbeddedDatabase::with_config(c).unwrap();
    db.storage.set_index_snapshots_on_close(false);
    db.storage.set_row_counter_flush_on_close(false);
    db
}
fn execute(db: &EmbeddedDatabase, params: bool, sql: &str) -> heliosdb_nano::Result<()> {
    if params {
        db.execute_params(sql, &[]).map(|_| ())
    } else {
        db.execute(sql).map(|_| ())
    }
}
fn value(db: &EmbeddedDatabase, params: bool, sql: &str) -> i64 {
    let rows = if params {
        db.query_params(sql, &[])
    } else {
        db.query(sql, &[])
    }
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values.len(), 1);
    match rows[0].values[0] {
        Value::Int8(v) => v,
        ref other => panic!("integer sequence value: {other:?}"),
    }
}
fn next(db: &EmbeddedDatabase, params: bool, name: &str) -> i64 {
    value(db, params, &format!("SELECT nextval('{name}')"))
}
fn image(db: &EmbeddedDatabase) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut ro = ReadOptions::default();
    ro.set_total_order_seek(true);
    db.storage
        .db()
        .iterator_opt(IteratorMode::Start, ro)
        .map(|e| {
            let (k, v) = e.unwrap();
            (k.to_vec(), v.to_vec())
        })
        .collect()
}
fn setup(db: &EmbeddedDatabase, params: bool) {
    execute(
        db,
        params,
        "CREATE SEQUENCE target START WITH 100 INCREMENT BY 7 CACHE 8",
    )
    .unwrap();
    execute(db, params, "CREATE SEQUENCE healthy START WITH 900 CACHE 8").unwrap();
    assert_eq!(next(db, params, "target"), 100);
    assert_eq!(next(db, params, "healthy"), 900);
    let state = db.storage.catalog().get_sequence_state("target").unwrap().unwrap();
    assert_eq!(state.last_reserved, 149);
    assert!(state.is_called);
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
fn owning_group(db: &EmbeddedDatabase, before: u64, applied: bool) {
    let raw = db.storage.db();
    let latest = raw.latest_sequence_number();
    if !applied {
        assert_eq!(latest, before);
        return;
    }
    assert!(latest > before);
    let mut groups = 0;
    let mut it = raw.get_updates_since(before).unwrap();
    for entry in it.by_ref() {
        let (first, batch) = entry.unwrap();
        assert_eq!(first, before + 1);
        assert_eq!(first + batch.len() as u64 - 1, latest);
        let mut keys = Keys::default();
        batch.iterate(&mut keys);
        assert!(keys
            .0
            .iter()
            .any(|k| k == b"meta:sequence:target" || k == b"meta:seqstate:target"));
        assert!(keys.0.iter().any(|k| k.starts_with(b"wal:entries:")));
        groups += 1;
    }
    it.status().unwrap();
    assert_eq!(groups, 1, "one actual owning DDL group; no error-path repair writes");
}
fn exercise(fault: Fault, changes: &[Change]) {
    let mut cells = 0;
    for history in [false, true] {
        for params in [false, true] {
            for &change in changes {
                let path = tempfile::tempdir().unwrap();
                let reopened_next;
                {
                    let db = open(path.path(), history);
                    setup(&db, params);
                    // Another live database has the SAME sequence name. Cache eviction
                    // must address the owning engine, not the latest-open registry entry.
                    let other_path = tempfile::tempdir().unwrap();
                    let other = open(other_path.path(), history);
                    execute(
                        &other,
                        params,
                        "CREATE SEQUENCE target START WITH 700 INCREMENT BY 5 CACHE 8",
                    )
                    .unwrap();
                    assert_eq!(next(&other, params, "target"), 700);
                    let before = image(&db);
                    let seq = db.storage.db().latest_sequence_number();
                    fault.arm(&db);
                    let result = execute(&db, params, change.sql());
                    db.storage.debug_set_write_group_ceiling(None);
                    fault.assert_error(result);
                    owning_group(&db, seq, fault.applied());
                    if !fault.applied() {
                        assert_eq!(image(&db), before);
                    }
                    let catalog = db.storage.catalog();
                    if fault.applied() && matches!(change, Change::Drop) {
                        assert!(catalog.get_sequence("target").unwrap().is_none());
                        assert!(catalog.get_sequence_state("target").unwrap().is_none());
                    } else {
                        let def = catalog.get_sequence("target").unwrap().unwrap();
                        let state = catalog.get_sequence_state("target").unwrap().unwrap();
                        assert_eq!(
                            def.increment_by,
                            if fault.applied() && matches!(change, Change::Increment) {
                                3
                            } else {
                                7
                            }
                        );
                        assert_eq!(
                            state.last_reserved,
                            if fault.applied() && matches!(change, Change::Restart) {
                                500
                            } else {
                                149
                            }
                        );
                        assert_eq!(state.is_called, !(fault.applied() && matches!(change, Change::Restart)));
                    }
                    let after_ddl = image(&db);
                    let after_seq = db.storage.db().latest_sequence_number();
                    // currval is session history, not the new definition. It remains
                    // the last value actually served even after a successful DROP.
                    assert_eq!(value(&db, params, "SELECT currval('target')"), 100);
                    assert_eq!(next(&db, params, "healthy"), 901);
                    assert_eq!(next(&other, params, "target"), 705);
                    assert_eq!(image(&db), after_ddl);
                    assert_eq!(db.storage.db().latest_sequence_number(), after_seq);
                    let expected = if !fault.applied() {
                        107
                    } else {
                        match change {
                            Change::Restart => 500,
                            Change::Increment => 152,
                            Change::Drop => 1,
                        }
                    };
                    assert_eq!(
                        next(&db, params, "target"),
                        expected,
                        "{fault:?}/{change:?}/history={history}/params={params}"
                    );
                    // D7 deliberately auto-vivifies a missing sequence. The DROP control
                    // preserves that contract; it must use fresh default1, not stale107.
                    let def = catalog.get_sequence("target").unwrap().unwrap();
                    let state = catalog.get_sequence_state("target").unwrap().unwrap();
                    if fault.applied() && matches!(change, Change::Drop) {
                        assert_eq!(def.start_value, 1);
                        assert_eq!(def.increment_by, 1);
                    }
                    reopened_next = state.last_reserved + def.increment_by;
                }
                {
                    let ro = EmbeddedDatabase::open_read_only(path.path()).unwrap();
                    let bytes = image(&ro);
                    let seq = ro.storage.db().latest_sequence_number();
                    assert!(ro.storage.catalog().get_sequence("target").unwrap().is_some());
                    assert_eq!(image(&ro), bytes);
                    assert_eq!(ro.storage.db().latest_sequence_number(), seq);
                }
                let reopened = open(path.path(), history);
                assert_eq!(
                    next(&reopened, params, "target"),
                    reopened_next,
                    "resume strictly after reserved high-water"
                );
                cells += 1;
            }
        }
    }
    assert_eq!(cells, 4 * changes.len());
    eprintln!("sequence-ddl-outcome {fault:?}/{changes:?}={cells}");
}
#[test]
fn known_refusal_preserves_warm_sequence_runtime() {
    exercise(Fault::Refused, &[Change::Restart, Change::Increment, Change::Drop]);
}
#[test]
fn cap_refusal_preserves_warm_sequence_runtime() {
    exercise(Fault::Cap, &[Change::Restart, Change::Increment, Change::Drop]);
}
#[test]
fn native_error_without_apply_preserves_actual_old_sequence() {
    exercise(Fault::NativeBefore, &[Change::Restart, Change::Increment, Change::Drop]);
}
#[test]
fn native_error_after_apply_reloads_restart() {
    exercise(Fault::NativeAfter, &[Change::Restart]);
}
#[test]
fn native_error_after_apply_reloads_increment() {
    exercise(Fault::NativeAfter, &[Change::Increment]);
}
#[test]
fn native_error_after_apply_forgets_dropped_sequence() {
    exercise(Fault::NativeAfter, &[Change::Drop]);
}
#[test]
fn late_applied_error_reloads_restart() {
    exercise(Fault::Applied, &[Change::Restart]);
}
#[test]
fn late_applied_error_reloads_increment() {
    exercise(Fault::Applied, &[Change::Increment]);
}
#[test]
fn late_applied_error_forgets_dropped_sequence() {
    exercise(Fault::Applied, &[Change::Drop]);
}

#[test]
fn unreadable_sequence_outcome_fences_only_its_namespace_without_repair_writes() {
    let mut cells = 0;
    for history in [false, true] {
        for params in [false, true] {
            let path = tempfile::tempdir().unwrap();
            let db = open(path.path(), history);
            setup(&db, params);
            let other_path = tempfile::tempdir().unwrap();
            let other = open(other_path.path(), history);
            execute(&other, params, "CREATE SEQUENCE target START WITH 700 CACHE 8").unwrap();
            assert_eq!(next(&other, params, "target"), 700);
            let captured = Arc::new(Mutex::new(None));
            let record = Arc::clone(&captured);
            let raw = db.storage.db();
            debug_fail_next_applied_commit_with(move || {
                // Explicit fixture corruption after the actual owner, before its
                // outcome witness. Later settlement is forbidden from repairing it.
                raw.put(b"meta:sequence:target", b"unreadable sequence definition")
                    .unwrap();
                *record.lock().unwrap() = Some(raw.latest_sequence_number());
            });
            Fault::Applied.assert_error(execute(&db, params, "ALTER SEQUENCE target RESTART WITH 500"));
            let seq = captured
                .lock()
                .unwrap()
                .expect("actual post-owner corruption hook fired");
            assert_eq!(db.storage.db().latest_sequence_number(), seq);
            let bytes = image(&db);
            for sql in ["SELECT nextval('target')", "SELECT setval('target',123)"] {
                let result = if params {
                    db.query_params(sql, &[])
                } else {
                    db.query(sql, &[])
                };
                let error =
                    result.expect_err("unresolved sequence must not use old cache or autovivify over corruption");
                assert!(
                    matches!(&error,Error::Storage(message) if message.contains("sequence")&&message.contains("unavailable")),
                    "{error}"
                );
            }
            assert_eq!(next(&db, params, "healthy"), 901);
            assert_eq!(next(&other, params, "target"), 701);
            assert_eq!(image(&db), bytes);
            assert_eq!(db.storage.db().latest_sequence_number(), seq);
            cells += 1;
        }
    }
    assert_eq!(cells, 4);
    eprintln!("sequence-ddl-outcome unresolved=4");
}
