//! A deferred pre-write refusal restores heap/index agreement and retires the
//! commit announcement before another reader takes a snapshot.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::{EmbeddedDatabase, Error, Value};
use std::sync::mpsc;
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
enum Family {
    Text,
    Params,
}
const FAMILIES: [Family; 2] = [Family::Text, Family::Params];
fn run(db: &EmbeddedDatabase, family: Family, session: Option<SessionId>, sql: &str) -> heliosdb_nano::Result<u64> {
    match (family, session) {
        (Family::Text, None) => db.execute(sql),
        (Family::Params, None) => db.execute_params(sql, &[]),
        (Family::Text, Some(session)) => db.execute_for_session(session, sql),
        (Family::Params, Some(session)) => db.execute_params_for_session(session, sql, &[]),
    }
}
fn setup(delete: bool) -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE p(id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO p VALUES(1),(2)").unwrap();
    db.execute(&format!(
        "CREATE TABLE c(id INT PRIMARY KEY,pid INT REFERENCES p(id) ON {} NO ACTION DEFERRABLE INITIALLY DEFERRED)",
        if delete { "DELETE" } else { "UPDATE" }
    ))
    .unwrap();
    db.execute("INSERT INTO c VALUES(10,1)").unwrap();
    db
}
fn fk_error(error: Error) {
    assert!(matches!(error, Error::ConstraintViolation(_)), "{error}");
    assert!(
        error.to_string().to_ascii_lowercase().contains("foreign key"),
        "{error}"
    );
}
fn assert_original(db: &EmbeddedDatabase) {
    // Point reads use the PK ART after the transaction's census has gone away.
    assert_eq!(
        db.query("SELECT id FROM p WHERE id=1", &[]).unwrap()[0].values,
        vec![Value::Int4(1)]
    );
    assert!(db.query("SELECT id FROM p WHERE id=99", &[]).unwrap().is_empty());
    let mut heap: Vec<_> = db
        .storage
        .scan_table("p")
        .unwrap()
        .into_iter()
        .map(|row| row.values)
        .collect();
    heap.sort_by_key(|row| match &row[0] {
        Value::Int4(id) => *id,
        _ => panic!("id"),
    });
    assert_eq!(heap, vec![vec![Value::Int4(1)], vec![Value::Int4(2)]]);
    assert_eq!(
        db.query("SELECT id,pid FROM c", &[]).unwrap()[0].values,
        vec![Value::Int4(10), Value::Int4(1)]
    );
}
fn barrier_progress(db: &EmbeddedDatabase) {
    let registry = db.storage.conflict_registry();
    let (done, received) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        registry.snapshot_barrier(u64::MAX);
        done.send(()).unwrap();
    });
    received
        .recv_timeout(Duration::from_secs(5))
        .expect("a refused commit must retire its snapshot announcement");
    waiter.join().unwrap();
    let reader = db
        .create_session("after-refusal", IsolationLevel::RepeatableRead)
        .unwrap();
    db.begin_transaction_for_session(reader).unwrap();
    assert_eq!(
        db.query_in_session(reader, "SELECT id FROM p WHERE id=1", &[])
            .unwrap()
            .len(),
        1
    );
    db.commit_transaction_for_session(reader).unwrap();
    db.destroy_session(reader).unwrap();
}
fn change(delete: bool) -> &'static str {
    if delete {
        "DELETE FROM p WHERE id=1"
    } else {
        "UPDATE p SET id=99 WHERE id=1"
    }
}
fn repair(delete: bool) -> &'static str {
    if delete {
        "UPDATE c SET pid=2 WHERE pid=1"
    } else {
        "UPDATE c SET pid=99 WHERE pid=1"
    }
}
#[test]
fn deferred_commit_refusal_restores_global_and_session_indexes_and_allows_exact_retry() {
    for family in FAMILIES {
        for session_owned in [false, true] {
            for delete in [false, true] {
                let db = setup(delete);
                let session =
                    session_owned.then(|| db.create_session("writer", IsolationLevel::ReadCommitted).unwrap());
                if let Some(session) = session {
                    db.begin_transaction_for_session(session).unwrap();
                } else {
                    run(&db, family, None, "BEGIN").unwrap();
                }
                assert_eq!(run(&db, family, session, change(delete)).unwrap(), 1);
                let sequence = db.storage.db().latest_sequence_number();
                let refused = match session {
                    Some(session) => db.commit_transaction_for_session(session),
                    None => run(&db, family, None, "COMMIT").map(|_| ()),
                };
                fk_error(refused.unwrap_err());
                assert_eq!(
                    db.storage.db().latest_sequence_number(),
                    sequence,
                    "pre-write refusal has no group"
                );
                assert_original(&db);
                barrier_progress(&db);
                if let Some(session) = session {
                    db.begin_transaction_for_session(session).unwrap();
                } else {
                    run(&db, family, None, "BEGIN").unwrap();
                }
                run(&db, family, session, change(delete)).unwrap();
                run(&db, family, session, repair(delete)).unwrap();
                match session {
                    Some(session) => db.commit_transaction_for_session(session).unwrap(),
                    None => {
                        run(&db, family, None, "COMMIT").unwrap();
                    }
                }
                assert!(db.query("SELECT id FROM p WHERE id=1", &[]).unwrap().is_empty());
                assert_eq!(
                    db.query("SELECT id FROM p WHERE id=99", &[]).unwrap().len(),
                    usize::from(!delete)
                );
                assert_eq!(
                    db.query("SELECT pid FROM c WHERE id=10", &[]).unwrap()[0].values,
                    vec![Value::Int4(if delete { 2 } else { 99 })]
                );
                if let Some(session) = session {
                    db.destroy_session(session).unwrap();
                }
            }
        }
    }
}
#[test]
#[allow(deprecated)]
fn deferred_commit_refusal_restores_raii_indexes_before_a_new_snapshot() {
    for delete in [false, true] {
        let db = setup(delete);
        let txn = db.begin_transaction().unwrap();
        txn.execute(change(delete)).unwrap();
        let sequence = db.storage.db().latest_sequence_number();
        fk_error(txn.commit().unwrap_err());
        assert_eq!(db.storage.db().latest_sequence_number(), sequence);
        assert_original(&db);
        barrier_progress(&db);
        let txn = db.begin_transaction().unwrap();
        txn.execute(change(delete)).unwrap();
        txn.execute(repair(delete)).unwrap();
        txn.commit().unwrap();
        assert_eq!(
            db.query("SELECT pid FROM c WHERE id=10", &[]).unwrap()[0].values,
            vec![Value::Int4(if delete { 2 } else { 99 })]
        );
    }
}
#[test]
fn a_cold_presence_intent_disappears_on_savepoint_rewind_and_rollback() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE p(id INT PRIMARY KEY)").unwrap();
        db.execute("CREATE TABLE c(id INT PRIMARY KEY,pid INT REFERENCES p(id) ON DELETE CASCADE)")
            .unwrap();
        db.execute("INSERT INTO p VALUES(1)").unwrap();
        db.execute("INSERT INTO c VALUES(10,1),(11,1)").unwrap();
        db.execute("ALTER TABLE c ALTER COLUMN pid SET STORAGE COLUMNAR")
            .unwrap();
        let raw = db.storage.db();
        assert!(raw.get(b"colpm:c").unwrap().is_none(), "cold manifest precondition");
        run(&db, family, None, "BEGIN").unwrap();
        run(&db, family, None, "SAVEPOINT s").unwrap();
        let before = raw.latest_sequence_number();
        run(&db, family, None, "DELETE FROM p WHERE id=1").unwrap();
        assert_eq!(
            raw.latest_sequence_number(),
            before,
            "even cold presence must remain staged"
        );
        assert!(raw.get(b"colpm:c").unwrap().is_none());
        run(&db, family, None, "ROLLBACK TO SAVEPOINT s").unwrap();
        run(&db, family, None, "COMMIT").unwrap();
        assert!(raw.get(b"colpm:c").unwrap().is_none());
        assert_eq!(db.query("SELECT pid FROM c ORDER BY id", &[]).unwrap().len(), 2);
        // Reading can lazily populate the manifest; remove only derived state
        // to independently exercise full transaction rollback from cold state.
        // Recreate the cold state using a supported storage-mode round trip.
        db.execute("ALTER TABLE c ALTER COLUMN pid SET STORAGE DEFAULT")
            .unwrap();
        db.execute("ALTER TABLE c ALTER COLUMN pid SET STORAGE COLUMNAR")
            .unwrap();
        assert!(raw.get(b"colpm:c").unwrap().is_none());
        run(&db, family, None, "BEGIN").unwrap();
        let before = raw.latest_sequence_number();
        run(&db, family, None, "DELETE FROM p WHERE id=1").unwrap();
        assert_eq!(raw.latest_sequence_number(), before);
        run(&db, family, None, "ROLLBACK").unwrap();
        assert!(raw.get(b"colpm:c").unwrap().is_none());
        assert_eq!(db.query("SELECT id FROM p WHERE id=1", &[]).unwrap().len(), 1);
        assert_eq!(db.query("SELECT pid FROM c ORDER BY id", &[]).unwrap().len(), 2);
    }
}

#[test]
#[allow(deprecated)]
fn nonfatal_deferred_audit_is_published_once_by_each_commit_owner() {
    for family in FAMILIES {
        for owner in 0..3 {
            if owner == 2 && matches!(family, Family::Params) {
                continue;
            }
            for delete in [false, true] {
                let db = setup(delete);
                db.execute("SET helios.fk_validation = 'audit'").unwrap();
                assert!(
                    !db.storage.catalog().table_exists("pg_log_violations").unwrap(),
                    "the audit relation is created lazily by the first violation"
                );
                match owner {
                    0 => {
                        run(&db, family, None, "BEGIN").unwrap();
                        run(&db, family, None, change(delete)).unwrap();
                        run(&db, family, None, "COMMIT").unwrap();
                    }
                    1 => {
                        let session = db
                            .create_session("audit_writer", IsolationLevel::ReadCommitted)
                            .unwrap();
                        db.begin_transaction_for_session(session).unwrap();
                        run(&db, family, Some(session), change(delete)).unwrap();
                        db.commit_transaction_for_session(session).unwrap();
                        db.destroy_session(session).unwrap();
                    }
                    _ => {
                        let txn = db.begin_transaction().unwrap();
                        txn.execute(change(delete)).unwrap();
                        txn.commit().unwrap();
                    }
                }
                let log = db
                    .query("SELECT referenced_values FROM pg_log_violations", &[])
                    .unwrap();
                assert_eq!(log.len(), 1, "{family:?} owner={owner} delete={delete}");
                assert_eq!(log[0].values, vec![Value::String("1".into())]);
                assert!(db.query("SELECT id FROM p WHERE id=1", &[]).unwrap().is_empty());
                assert_eq!(
                    db.query("SELECT pid FROM c WHERE id=10", &[]).unwrap()[0].values,
                    vec![Value::Int4(1)],
                    "nonfatal audit preserves the child's value"
                );
                assert_eq!(
                    db.query("SELECT id FROM p WHERE id=99", &[]).unwrap().len(),
                    usize::from(!delete)
                );
            }
        }
    }
}
