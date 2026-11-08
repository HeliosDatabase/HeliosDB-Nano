//! Actual nontransactional setval native outcomes settle its cached window.
#![cfg(feature = "internal-tests")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
use heliosdb_nano::{Config, EmbeddedDatabase, Error, Value};
use rocksdb::{IteratorMode, ReadOptions, WriteBatchIterator};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
#[derive(Clone, Copy, Debug)]
enum Fault {
    Cap,
    Before,
    After,
    Late,
}
impl Fault {
    fn applied(self) -> bool {
        matches!(self, Self::After | Self::Late)
    }
    fn arm(self, db: &EmbeddedDatabase) {
        match self {
            Self::Cap => db.storage.debug_set_write_group_ceiling(Some(0)),
            Self::Before => db.storage.write_gate().debug_fail_next_native_write_before_apply(),
            Self::After => db.storage.write_gate().debug_fail_next_native_write_after_apply(),
            Self::Late => db.storage.debug_fail_next_sequence_flush_after_apply(),
        }
    }
    fn check(self, result: heliosdb_nano::Result<i64>) {
        let error = result.expect_err("actual setval owning state group must report armed error");
        let marker = match self {
            Self::Cap => "write group",
            Self::Before => "injected native write error before apply; outcome is unknown",
            Self::After => "injected native write error after apply; outcome is unknown",
            Self::Late => "injected sequence durability failure after state applied",
        };
        assert!(error.to_string().contains(marker), "{self:?}: {error}");
        if matches!(self, Self::Cap) {
            assert!(
                matches!(&error, heliosdb_nano::Error::QueryExecution(message)
                if message.contains(heliosdb_nano::BRANCH_MATERIALIZE_TOO_LARGE_MARKER)),
                "cap refusal must preserve its original typed cause: {error:?}"
            );
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
fn exec(db: &EmbeddedDatabase, params: bool, sql: &str) {
    if params {
        db.execute_params(sql, &[]).unwrap();
    } else {
        db.execute(sql).unwrap();
    }
}
fn scalar(db: &EmbeddedDatabase, params: bool, sql: &str) -> heliosdb_nano::Result<i64> {
    let rows = if params {
        db.query_params(sql, &[])
    } else {
        db.query(sql, &[])
    }?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values.len(), 1);
    Ok(match rows[0].values[0] {
        Value::Int8(v) => v,
        ref v => panic!("sequence integer:{v:?}"),
    })
}
fn next(db: &EmbeddedDatabase, params: bool, name: &str) -> i64 {
    scalar(db, params, &format!("SELECT nextval('{name}')")).unwrap()
}
fn set(db: &EmbeddedDatabase, params: bool, called: bool) -> heliosdb_nano::Result<i64> {
    scalar(db, params, &format!("SELECT setval('target',500,{called})"))
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
    exec(
        db,
        params,
        "CREATE SEQUENCE target START WITH 100 INCREMENT BY 7 CACHE 8",
    );
    exec(db, params, "CREATE SEQUENCE healthy START WITH 900 CACHE 8");
    assert_eq!(next(db, params, "target"), 100);
    assert_eq!(next(db, params, "healthy"), 900);
    let state = db.storage.catalog().get_sequence_state("target").unwrap().unwrap();
    assert_eq!((state.last_reserved, state.is_called), (149, true));
}
#[derive(Default)]
struct Keys(Vec<Vec<u8>>);
impl WriteBatchIterator for Keys {
    fn put(&mut self, k: Box<[u8]>, _: Box<[u8]>) {
        self.0.push(k.into_vec());
    }
    fn delete(&mut self, k: Box<[u8]>) {
        panic!("setval owning group deletes nothing:{k:?}");
    }
}
fn one_state_group(db: &EmbeddedDatabase, before: u64, applied: bool) {
    let raw = db.storage.db();
    let after = raw.latest_sequence_number();
    if !applied {
        assert_eq!(after, before);
        return;
    }
    let mut count = 0;
    let mut it = raw.get_updates_since(before).unwrap();
    for e in it.by_ref() {
        let (first, batch) = e.unwrap();
        assert_eq!(first, before + 1);
        assert_eq!(first + batch.len() as u64 - 1, after);
        let mut keys = Keys::default();
        batch.iterate(&mut keys);
        assert_eq!(
            keys.0,
            vec![b"meta:seqstate:target".to_vec()],
            "only actual owning state key; no incidental metadata fault or native repair"
        );
        count += 1;
    }
    it.status().unwrap();
    assert_eq!(count, 1);
}
fn exercise(fault: Fault, called_values: &[bool]) {
    let mut cells = 0;
    for history in [false, true] {
        for params in [false, true] {
            for &called in called_values {
                let path = tempfile::tempdir().unwrap();
                let expected_reopen;
                {
                    let db = open(path.path(), history);
                    setup(&db, params);
                    let other_path = tempfile::tempdir().unwrap();
                    let other = open(other_path.path(), history);
                    exec(
                        &other,
                        params,
                        "CREATE SEQUENCE target START WITH 700 INCREMENT BY 11 CACHE 8",
                    );
                    assert_eq!(next(&other, params, "target"), 700);
                    let bytes = image(&db);
                    let before = db.storage.db().latest_sequence_number();
                    fault.arm(&db);
                    let result = set(&db, params, called);
                    db.storage.debug_set_write_group_ceiling(None);
                    fault.check(result);
                    one_state_group(&db, before, fault.applied());
                    if !fault.applied() {
                        assert_eq!(image(&db), bytes);
                    }
                    let state = db.storage.catalog().get_sequence_state("target").unwrap().unwrap();
                    assert_eq!(
                        (state.last_reserved, state.is_called),
                        if fault.applied() { (500, called) } else { (149, true) }
                    );
                    let after = image(&db);
                    let seq = db.storage.db().latest_sequence_number();
                    assert_eq!(
                        scalar(&db, params, "SELECT currval('target')").unwrap(),
                        100,
                        "error does not falsely record an acknowledged session value"
                    );
                    assert_eq!(next(&db, params, "healthy"), 901);
                    assert_eq!(next(&other, params, "target"), 711);
                    assert_eq!(image(&db), after);
                    assert_eq!(db.storage.db().latest_sequence_number(), seq);
                    let expected = if fault.applied() {
                        if called {
                            507
                        } else {
                            500
                        }
                    } else {
                        107
                    };
                    assert_eq!(next(&db,params,"target"),expected,"{fault:?}/called={called}/history={history}/params={params}: native truth must replace stale window");
                    if !fault.applied() {
                        assert_eq!(image(&db), after);
                        assert_eq!(
                            db.storage.db().latest_sequence_number(),
                            seq,
                            "old reserved107 requires no native write"
                        );
                    }
                    if matches!(fault, Fault::Cap) {
                        assert_eq!(set(&db, params, called).unwrap(), 500);
                        assert_eq!(
                            next(&db, params, "target"),
                            if called { 507 } else { 500 },
                            "known refusal retries correctly"
                        );
                    }
                    let state = db.storage.catalog().get_sequence_state("target").unwrap().unwrap();
                    expected_reopen = state.last_reserved + 7;
                }
                {
                    let ro = EmbeddedDatabase::open_read_only(path.path()).unwrap();
                    let bytes = image(&ro);
                    let seq = ro.storage.db().latest_sequence_number();
                    assert!(ro.storage.catalog().get_sequence_state("target").unwrap().is_some());
                    assert_eq!(image(&ro), bytes);
                    assert_eq!(ro.storage.db().latest_sequence_number(), seq);
                }
                let reopened = open(path.path(), history);
                assert_eq!(
                    next(&reopened, params, "target"),
                    expected_reopen,
                    "resume past actual durable reservation"
                );
                cells += 1;
            }
        }
    }
    assert_eq!(cells, 4 * called_values.len());
    eprintln!("sequence-setval-outcome {fault:?}/{called_values:?}={cells}");
}
#[test]
fn known_cap_refusal_preserves_old_window_and_retry() {
    exercise(Fault::Cap, &[false, true]);
}
#[test]
fn native_before_state_apply_preserves_old_window() {
    exercise(Fault::Before, &[false, true]);
}
#[test]
fn native_after_state_apply_honors_not_called_target() {
    exercise(Fault::After, &[false]);
}
#[test]
fn native_after_state_apply_honors_called_target() {
    exercise(Fault::After, &[true]);
}
#[test]
fn late_applied_flush_error_honors_not_called_target() {
    exercise(Fault::Late, &[false]);
}
#[test]
fn late_applied_flush_error_honors_called_target() {
    exercise(Fault::Late, &[true]);
}
#[test]
fn unreadable_applied_state_fences_own_namespace_without_native_repair() {
    let mut cells = 0;
    for history in [false, true] {
        for params in [false, true] {
            let path = tempfile::tempdir().unwrap();
            let db = open(path.path(), history);
            setup(&db, params);
            let other_path = tempfile::tempdir().unwrap();
            let other = open(other_path.path(), history);
            exec(&other, params, "CREATE SEQUENCE target START WITH 700 CACHE 8");
            assert_eq!(next(&other, params, "target"), 700);
            let raw = db.storage.db();
            let capture = Arc::new(Mutex::new(None));
            let record = Arc::clone(&capture);
            let before = raw.latest_sequence_number();
            db.storage.debug_fail_next_sequence_flush_after_apply_with(move || {
                raw.put(b"meta:seqstate:target", b"unreadable actual state at settlement")
                    .unwrap();
                *record.lock().unwrap() = Some(raw.latest_sequence_number());
            });
            Fault::Late.check(set(&db, params, false));
            let finalseq = capture.lock().unwrap().expect("actual own state nativeOK hook fired");
            assert_eq!(db.storage.db().latest_sequence_number(), finalseq);
            // EXACTLY the owning state write plus explicit fixture corruption, no
            // further native write from error settlement. The original cause survives.
            assert_eq!(finalseq, before + 2);
            let mut groups = 0;
            let mut it = db.storage.db().get_updates_since(before).unwrap();
            for e in it.by_ref() {
                let (first, batch) = e.unwrap();
                assert_eq!(first, before + 1 + groups);
                let mut keys = Keys::default();
                batch.iterate(&mut keys);
                assert_eq!(keys.0, vec![b"meta:seqstate:target".to_vec()]);
                groups += 1;
            }
            it.status().unwrap();
            assert_eq!(groups, 2);
            let bytes = image(&db);
            for sql in ["SELECT nextval('target')", "SELECT setval('target',123)"] {
                let r = if params {
                    db.query_params(sql, &[])
                } else {
                    db.query(sql, &[])
                };
                let error = r.expect_err("unreadable actual state cannot reset/reserve from old runtime");
                assert!(
                    matches!(&error,Error::Storage(s)if s.contains("sequence")&&s.contains("unavailable")),
                    "{error}"
                );
            }
            assert_eq!(scalar(&db, params, "SELECT currval('target')").unwrap(), 100);
            assert_eq!(next(&db, params, "healthy"), 901);
            assert_eq!(next(&other, params, "target"), 701);
            assert_eq!(image(&db), bytes);
            assert_eq!(db.storage.db().latest_sequence_number(), finalseq);
            cells += 1;
        }
    }
    assert_eq!(cells, 4);
    eprintln!("sequence-setval-outcome unresolved=4");
}
