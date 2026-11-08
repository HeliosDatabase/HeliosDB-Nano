//! Real public sequence operations cannot publish an old runtime's reservation
//! after another session completed RESTART. Requires the excluded cfg seams.
#![cfg(feature = "internal-tests")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::sql::sequences::{debug_next_before_refill_owner, debug_next_before_setval_owner};
use heliosdb_nano::{Config, EmbeddedDatabase, Result, Value};
use rocksdb::WriteBatchIterator;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
enum Family {
    Text,
    Params,
}
#[derive(Clone, Copy, Debug)]
enum Change {
    Refill,
    Setval,
}

fn open(path: &Path, history: bool) -> EmbeddedDatabase {
    let mut config = Config::default();
    config.storage.path = Some(path.into());
    config.storage.memory_only = false;
    config.storage.wal_enabled = true;
    config.storage.time_travel_enabled = history;
    config.storage.version_gc_interval_secs = Some(0);
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    config.storage.wal_checkpoint_on_close = false;
    config.storage.flush_on_close = false;
    let db = EmbeddedDatabase::with_config(config).unwrap();
    db.storage.set_index_snapshots_on_close(false);
    db.storage.set_row_counter_flush_on_close(false);
    db
}

fn exec(db: &EmbeddedDatabase, session: SessionId, family: Family, sql: &str) -> Result<u64> {
    match family {
        Family::Text => db.execute_for_session(session, sql),
        Family::Params => db.execute_params_for_session(session, sql, &[]),
    }
}

fn scalar(db: &EmbeddedDatabase, session: SessionId, family: Family, sql: &str) -> Result<i64> {
    let rows = match family {
        Family::Text => db.query_with_columns_for_session(session, sql)?.0,
        Family::Params => db.query_params_for_session(session, sql, &[])?,
    };
    assert_eq!(rows.len(), 1);
    Ok(match rows[0].values.first().unwrap() {
        Value::Int4(value) => i64::from(*value),
        Value::Int8(value) => *value,
        other => panic!("expected sequence integer, got {other:?}"),
    })
}

#[derive(Default)]
struct Keys(Vec<Vec<u8>>);
impl WriteBatchIterator for Keys {
    fn put(&mut self, key: Box<[u8]>, _: Box<[u8]>) {
        self.0.push(key.into_vec());
    }
    fn delete(&mut self, key: Box<[u8]>) {
        self.0.push(key.into_vec());
    }
}

fn trace(db: &EmbeddedDatabase, before: u64, after_ddl: u64, _history: bool) -> (usize, usize) {
    let raw = db.storage.db();
    let after = raw.latest_sequence_number();
    raw.flush_wal(true).unwrap();
    let mut next = before + 1;
    let mut owners = 0;
    let mut reservations = 0;
    let mut snapshots = 0;
    let mut cursor = raw.get_updates_since(before).unwrap();
    for item in cursor.by_ref() {
        let (first, batch) = item.unwrap();
        assert_eq!(first, next, "complete native cursor without skipped groups");
        next += batch.len() as u64;
        let mut keys = Keys::default();
        batch.iterate(&mut keys);
        let wall = |key: &[u8]| {
            key.strip_prefix(b"scn_map:@")
                .and_then(|tail| tail.first())
                .is_some_and(u8::is_ascii_digit)
        };
        let snapshot = keys.0.len() == 4
            && keys.0.iter().filter(|key| key.starts_with(b"snapshot:")).count() == 1
            && keys.0.iter().filter(|key| wall(key)).count() == 1
            && keys.0.iter().filter(|key| key.starts_with(b"scn_map:@s:")).count() == 1
            && keys.0.iter().filter(|key| key.starts_with(b"scn_map:@t:")).count() == 1;
        if snapshot {
            // Established successful text DDL empty-txn bookkeeping only;
            // never admit arbitrary metadata, errors or late reservation here.
            assert_eq!(owners, 1);
            assert!(first <= after_ddl && next - 1 <= after_ddl);
            snapshots += 1;
        } else if keys.0.iter().any(|key| key == b"meta:sequence:target") {
            assert!(keys.0.iter().any(|key| key == b"meta:seqstate:target"));
            assert!(keys.0.iter().any(|key| key.starts_with(b"wal:entries:")));
            assert!(next - 1 <= after_ddl);
            owners += 1;
        } else {
            assert_eq!(
                keys.0,
                vec![b"meta:seqstate:target".to_vec()],
                "only actual public reservation writes after DDL"
            );
            assert!(
                first > after_ddl,
                "the parked old Arc resumes after the completed restart"
            );
            reservations += 1;
        }
        eprintln!(
            "SEQUENCE_INFLIGHT_GROUP first={first} end={} keys={:?}",
            next - 1,
            keys.0
        );
    }
    cursor.status().unwrap();
    assert_eq!(next, after + 1);
    assert_eq!(owners, 1);
    assert!(snapshots <= 1, "at most the exact established DDL bookkeeping group");
    (owners, reservations)
}

fn run(change: Change) {
    let mut cells = 0;
    for history in [false, true] {
        for family in [Family::Text, Family::Params] {
            let path = tempfile::tempdir().unwrap();
            let other_path = tempfile::tempdir().unwrap();
            {
                let db = open(path.path(), history);
                let other = open(other_path.path(), history);
                let a = db
                    .create_session("old-sequence-runtime", IsolationLevel::ReadCommitted)
                    .unwrap();
                let b = db
                    .create_session("restart-sequence", IsolationLevel::ReadCommitted)
                    .unwrap();
                let healthy = other
                    .create_session("other-database", IsolationLevel::ReadCommitted)
                    .unwrap();
                exec(
                    &db,
                    b,
                    family,
                    "CREATE SEQUENCE target START WITH 100 INCREMENT BY 7 CACHE 2",
                )
                .unwrap();
                exec(
                    &other,
                    healthy,
                    family,
                    "CREATE SEQUENCE target START WITH 900 INCREMENT BY 11 CACHE 2",
                )
                .unwrap();
                assert_eq!(scalar(&db, a, family, "SELECT nextval('target')").unwrap(), 100);
                assert_eq!(scalar(&db, a, family, "SELECT nextval('target')").unwrap(), 107);
                assert_eq!(
                    scalar(&other, healthy, family, "SELECT nextval('target')").unwrap(),
                    900
                );
                // The read session gets its own transaction slot, so the public
                // SQL call never holds EmbeddedDatabase's global txn mutex.
                // Sequence effects remain nontransactional on rollback.
                db.begin_transaction_for_session(a).unwrap();
                let before = db.storage.db().latest_sequence_number();
                let (ready_tx, ready_rx) = mpsc::channel();
                let (release_tx, release_rx) = mpsc::channel();
                let (done_tx, done_rx) = mpsc::channel();
                let (reached, completed_before_release, old, ddl) = std::thread::scope(|scope| {
                    let old = scope.spawn(|| {
                        let pause = move || {
                            ready_tx.send(()).unwrap();
                            release_rx
                                .recv_timeout(Duration::from_secs(20))
                                .expect("bounded old-Arc release");
                        };
                        match change {
                            Change::Refill => debug_next_before_refill_owner(pause),
                            Change::Setval => debug_next_before_setval_owner(pause),
                        }
                        scalar(
                            &db,
                            a,
                            family,
                            match change {
                                Change::Refill => "SELECT nextval('target')",
                                Change::Setval => "SELECT setval('target',200,false)",
                            },
                        )
                    });
                    let reached = ready_rx.recv_timeout(Duration::from_secs(5)).is_ok();
                    let ddl = scope.spawn(|| {
                        let result = exec(
                            &db,
                            b,
                            family,
                            match change {
                                Change::Refill => "ALTER SEQUENCE target RESTART WITH 500",
                                //200was legal before; after this definition it is
                                //invalid. Either legal serialization must preserve
                                //restart100 or reject stale setval, never expose200.
                                Change::Setval => "ALTER SEQUENCE target MAXVALUE 150 RESTART WITH 100",
                            },
                        );
                        let _ = done_tx.send(db.storage.db().latest_sequence_number());
                        result
                    });
                    let completed = done_rx.recv_timeout(Duration::from_secs(5)).ok();
                    // Always release/reap both threads before any assertion.
                    let _ = release_tx.send(());
                    (reached, completed, old.join(), ddl.join())
                });
                assert!(reached, "actual captured old-Arc owner boundary reached");
                let after_ddl =
                    completed_before_release.expect("DDL completes while old Arc is parked BEFORE its mutation owner");
                assert_eq!(ddl.unwrap().unwrap(), 0);
                let old = old.unwrap();
                let state = db.storage.catalog().get_sequence_state("target").unwrap().unwrap();
                let counts = trace(&db, before, after_ddl, history);
                eprintln!("SEQUENCE_INFLIGHT_TRUTH {change:?} {history} {family:?} old={old:?} state={state:?}");
                match change {
                    Change::Refill => {
                        assert_eq!(
                            old.unwrap(),
                            500,
                            "captured old Arc must re-fetch the completed restart's not-called state"
                        );
                        assert_eq!((state.last_reserved, state.is_called), (507, true));
                        assert_eq!(scalar(&db, b, family, "SELECT nextval('target')").unwrap(), 507);
                        assert_eq!(counts, (1, 1), "one DDL and one real public refill, no repair");
                    }
                    Change::Setval => {
                        // Judge actual public/current wrong-value truth before
                        //the additional error/ownership invariant.
                        assert_eq!(
                            scalar(&db, b, family, "SELECT nextval('target')").unwrap(),
                            100,
                            "stale setval must not overwrite completed bounded restart"
                        );
                        let error = old.unwrap_err();
                        assert!(error.to_string().contains("out of bounds"), "{error}");
                        assert_eq!((state.last_reserved, state.is_called), (100, false));
                        assert_eq!(counts, (1, 0), "rejected stale setval writes no reservation or repair");
                        assert_eq!(
                            db.storage.catalog().get_sequence("target").unwrap().unwrap().max_value,
                            150
                        );
                    }
                }
                assert_eq!(
                    scalar(&other, healthy, family, "SELECT nextval('target')").unwrap(),
                    911
                );
                db.rollback_transaction_for_session(a).unwrap();
                db.destroy_session(a).unwrap();
                db.destroy_session(b).unwrap();
                other.destroy_session(healthy).unwrap();
            }
            let db = open(path.path(), history);
            let session = db
                .create_session("reopened-sequence", IsolationLevel::ReadCommitted)
                .unwrap();
            assert_eq!(
                scalar(&db, session, family, "SELECT nextval('target')").unwrap(),
                match change {
                    Change::Refill => 514,
                    Change::Setval => 114,
                }
            );
            db.destroy_session(session).unwrap();
            cells += 1;
            eprintln!("SEQUENCE_INFLIGHT_CELL {change:?} {history} {family:?}");
        }
    }
    assert_eq!(cells, 4);
}

#[test]
fn exhausted_old_runtime_refill_observes_an_already_completed_restart() {
    run(Change::Refill);
}

#[test]
fn old_runtime_setval_cannot_bypass_completed_bounds_and_restart() {
    run(Change::Setval);
}
