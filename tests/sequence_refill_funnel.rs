//! Managed nextval refill uses the same bounded native owner as setval.
#![cfg(feature = "internal-tests")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
use heliosdb_nano::{Config, EmbeddedDatabase, Value};
use rocksdb::{IteratorMode, ReadOptions, WriteBatchIterator};
use std::collections::BTreeMap;
use std::path::Path;
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
        let error = result.expect_err("actual refill owning state group must report armed error");
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
fn exercise(fault: Fault) {
    let mut cells = 0;
    for history in [false, true] {
        for params in [false, true] {
            let path = tempfile::tempdir().unwrap();
            let expected_reopen;
            {
                let db = open(path.path(), history);
                exec(
                    &db,
                    params,
                    "CREATE SEQUENCE target START WITH 100 INCREMENT BY 7 CACHE 2",
                );
                exec(&db, params, "CREATE SEQUENCE healthy START WITH 900 CACHE 8");
                assert_eq!(next(&db, params, "target"), 100);
                assert_eq!(next(&db, params, "target"), 107);
                assert_eq!(next(&db, params, "healthy"), 900);
                let before_bytes = image(&db);
                let before = db.storage.db().latest_sequence_number();
                fault.arm(&db);
                let result = scalar(&db, params, "SELECT nextval('target')");
                db.storage.debug_set_write_group_ceiling(None);
                fault.check(result);
                one_state_group(&db, before, fault.applied());
                if !fault.applied() {
                    assert_eq!(image(&db), before_bytes);
                }
                let state = db.storage.catalog().get_sequence_state("target").unwrap().unwrap();
                assert_eq!(
                    (state.last_reserved, state.is_called),
                    (if fault.applied() { 121 } else { 107 }, true)
                );
                let after = image(&db);
                let after_seq = db.storage.db().latest_sequence_number();
                assert_eq!(
                    scalar(&db, params, "SELECT currval('target')").unwrap(),
                    107,
                    "an unacknowledged refill must not serve a value"
                );
                assert_eq!(next(&db, params, "healthy"), 901);
                assert_eq!(image(&db), after);
                assert_eq!(db.storage.db().latest_sequence_number(), after_seq);
                assert_eq!(
                    next(&db, params, "target"),
                    if fault.applied() { 128 } else { 114 },
                    "retry reads actual high-water; an unserved applied block may be skipped"
                );
                one_state_group(&db, after_seq, true);
                let state = db.storage.catalog().get_sequence_state("target").unwrap().unwrap();
                expected_reopen = state.last_reserved + 7;
            }
            {
                let ro = EmbeddedDatabase::open_read_only(path.path()).unwrap();
                let before = image(&ro);
                let before_seq = ro.storage.db().latest_sequence_number();
                assert!(ro.storage.catalog().get_sequence_state("target").unwrap().is_some());
                assert_eq!(image(&ro), before);
                assert_eq!(ro.storage.db().latest_sequence_number(), before_seq);
            }
            let reopened = open(path.path(), history);
            assert_eq!(next(&reopened, params, "target"), expected_reopen);
            cells += 1;
        }
    }
    assert_eq!(cells, 4);
    eprintln!("sequence-refill-funnel {fault:?}={cells}");
}
#[test]
fn managed_refill_cap_is_refused_before_native_and_retries() {
    exercise(Fault::Cap);
}
#[test]
fn managed_refill_native_before_error_preserves_state_and_retries() {
    exercise(Fault::Before);
}
#[test]
fn managed_refill_native_after_error_skips_unserved_block_and_reopens() {
    exercise(Fault::After);
}
#[test]
fn managed_refill_late_error_skips_unserved_block_and_reopens() {
    exercise(Fault::Late);
}
