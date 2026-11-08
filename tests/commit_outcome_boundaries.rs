//! Independent heap/key oracles at owning commit boundaries. Faults are armed
//! through public test hooks; no private outcome classification is inspected.
#![cfg(feature = "internal-tests")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    deprecated
)]

use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::{Config, EmbeddedDatabase, Tuple, Value};
use rocksdb::{Direction, IteratorMode, ReadOptions};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
enum Family {
    Text,
    Params,
}

#[derive(Clone, Copy, Debug)]
enum Owner {
    Global,
    Session,
    Raii,
    Implicit,
    Batch,
}

#[derive(Clone, Copy, Debug)]
struct Route {
    owner: Owner,
    family: Family,
    session: Option<SessionId>,
}

const EXPLICIT: [(Owner, Family); 5] = [
    (Owner::Global, Family::Text),
    (Owner::Global, Family::Params),
    (Owner::Session, Family::Text),
    (Owner::Session, Family::Params),
    (Owner::Raii, Family::Text),
];
const IMPLICIT: [(Owner, Family); 3] = [
    (Owner::Implicit, Family::Text),
    (Owner::Implicit, Family::Params),
    (Owner::Batch, Family::Text),
];

impl Route {
    fn new(db: &EmbeddedDatabase, owner: Owner, family: Family) -> Self {
        Self {
            owner,
            family,
            session: matches!(owner, Owner::Session).then(|| {
                db.create_session("commit-owner", IsolationLevel::ReadCommitted)
                    .unwrap()
            }),
        }
    }
    fn finish(self, db: &EmbeddedDatabase) {
        if let Some(session) = self.session {
            db.destroy_session(session).unwrap();
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Change {
    Insert,
    Move,
    Delete,
}
const CHANGES: [Change; 3] = [Change::Insert, Change::Move, Change::Delete];

impl Change {
    fn sql(self) -> &'static str {
        match self {
            Self::Insert => "INSERT INTO t VALUES (3,30,300,'new')",
            Self::Move => "UPDATE t SET id=11,u=110,m=1100,payload='moved' WHERE id=1",
            Self::Delete => "DELETE FROM t WHERE id=1",
        }
    }
    // These shapes deliberately require the implicit owning transaction rather
    // than a directly durable single-row fast path. A missed hook is a failure.
    fn implicit_sql(self) -> &'static str {
        match self {
            Self::Insert => "INSERT INTO t SELECT 3,30,300,'new'",
            Self::Move => "UPDATE t SET id=11,u=110,m=1100,payload='moved' WHERE LENGTH(payload)=3 AND m=100",
            Self::Delete => "DELETE FROM t WHERE LENGTH(payload)=3 AND m=100",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Fault {
    None,
    Cap,
    Applied,
    Unknown,
}

fn arm(db: &EmbeddedDatabase, fault: Fault) {
    match fault {
        Fault::None => {}
        Fault::Cap => db.storage.debug_set_write_group_ceiling(Some(64)),
        Fault::Applied => db.storage.debug_fail_next_applied_commit(),
        Fault::Unknown => db.storage.write_gate().debug_fail_next_native_write_after_apply(),
    }
}

fn execute(db: &EmbeddedDatabase, route: Route, sql: &str) -> heliosdb_nano::Result<u64> {
    match (route.family, route.session) {
        (Family::Text, None) => db.execute(sql),
        (Family::Params, None) => db.execute_params(sql, &[]),
        (Family::Text, Some(session)) => db.execute_for_session(session, sql),
        (Family::Params, Some(session)) => db.execute_params_for_session(session, sql, &[]),
    }
}

/// Explicit owners arm the cap/fault AFTER DML succeeds and BEFORE COMMIT.
/// Implicit/batch owners have no public pause there, so arm before their call.
fn attempt(db: &EmbeddedDatabase, route: Route, change: Change, fault: Fault) -> (heliosdb_nano::Result<()>, u64) {
    let result;
    let before;
    match route.owner {
        Owner::Global | Owner::Session => {
            if let Some(session) = route.session {
                db.begin_transaction_for_session(session).unwrap();
            } else {
                execute(db, route, "BEGIN").unwrap();
            }
            assert_eq!(
                execute(db, route, change.sql()).unwrap(),
                1,
                "{route:?} {change:?}: DML staged before fault"
            );
            before = db.storage.db().latest_sequence_number();
            arm(db, fault);
            result = if let Some(session) = route.session {
                db.commit_transaction_for_session(session)
            } else {
                execute(db, route, "COMMIT").map(|_| ())
            };
        }
        Owner::Raii => {
            let txn = db.begin_transaction().unwrap();
            assert_eq!(txn.execute(change.sql()).unwrap(), 1, "RAII DML staged before fault");
            before = db.storage.db().latest_sequence_number();
            arm(db, fault);
            result = txn.commit();
        }
        Owner::Implicit => {
            before = db.storage.db().latest_sequence_number();
            arm(db, fault);
            result = execute(db, route, change.implicit_sql()).map(|rows| {
                assert_eq!(rows, 1, "{route:?} {change:?}: implicit DML count");
            });
        }
        Owner::Batch => {
            before = db.storage.db().latest_sequence_number();
            arm(db, fault);
            result = db
                .execute_batch(&[change.sql(), "UPDATE t SET payload='batch-second' WHERE id=2"])
                .map(|rows| {
                    assert_eq!(rows, 2, "both batch statements succeed together");
                });
        }
    }
    // Always remove the per-engine cap before assertions/retry/probes.
    db.storage.debug_set_write_group_ceiling(None);
    (result, before)
}

fn open(path: &Path, history: bool) -> EmbeddedDatabase {
    let mut last = None;
    for _ in 0..100 {
        let mut config = Config::default();
        config.storage.path = Some(path.to_path_buf());
        config.storage.memory_only = false;
        config.storage.time_travel_enabled = history;
        config.storage.wal_enabled = true;
        config.storage.version_gc_interval_secs = Some(0);
        config.storage.wal_checkpoint_interval_entries = 0;
        config.storage.wal_checkpoint_interval_secs = 0;
        match EmbeddedDatabase::with_config(config) {
            Ok(db) => return db,
            Err(error) => last = Some(error),
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("open {}: {last:?}", path.display());
}

fn row(id: i32, unique: i32, manual: i32, payload: &str) -> Vec<Value> {
    vec![
        Value::Int4(id),
        Value::Int4(unique),
        Value::Int4(manual),
        Value::String(payload.into()),
    ]
}
fn original() -> Vec<Vec<Value>> {
    vec![row(1, 10, 100, "one"), row(2, 20, 200, "two")]
}

fn expected(change: Change, batch: bool) -> Vec<Vec<Value>> {
    let mut rows = match change {
        Change::Insert => vec![row(1, 10, 100, "one"), row(2, 20, 200, "two"), row(3, 30, 300, "new")],
        Change::Move => vec![row(2, 20, 200, "two"), row(11, 110, 1100, "moved")],
        Change::Delete => vec![row(2, 20, 200, "two")],
    };
    if batch {
        rows.iter_mut().find(|row| row[0] == Value::Int4(2)).unwrap()[3] = Value::String("batch-second".into());
    }
    rows
}

fn integer(value: &Value) -> i64 {
    match value {
        Value::Int2(v) => i64::from(*v),
        Value::Int4(v) => i64::from(*v),
        Value::Int8(v) => *v,
        other => panic!("integer {other:?}"),
    }
}
fn query(db: &EmbeddedDatabase, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql, &[])
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
        .into_iter()
        .map(|row| row.values)
        .collect()
}

/// Independent byte-level truth, also usable while SQL access is quarantined.
/// This test config leaves encryption disabled; the row format is plain bincode.
fn raw_heap(db: &EmbeddedDatabase) -> Vec<Vec<Value>> {
    let raw = db.storage.db();
    let prefix = b"data:t:";
    let mut options = ReadOptions::default();
    options.set_total_order_seek(true);
    let mut rows = Vec::new();
    for item in raw.iterator_opt(IteratorMode::From(prefix, Direction::Forward), options) {
        let (key, value) = item.unwrap();
        if !key.starts_with(prefix) {
            break;
        }
        let row: Tuple = bincode::deserialize(&value).unwrap();
        rows.push(row.values);
    }
    rows.sort_by_key(|row| integer(&row[0]));
    rows
}

fn setup(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE t (id INT PRIMARY KEY,u INT UNIQUE,m INT,payload TEXT)")
        .unwrap();
    db.execute("CREATE INDEX t_m ON t(m)").unwrap();
    db.execute("CREATE TABLE healthy (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO t VALUES(1,10,100,'one'),(2,20,200,'two')")
        .unwrap();
    for _ in 0..12 {
        assert_eq!(integer(&query(db, "SELECT COUNT(*) FROM t")[0][0]), 2);
        for predicate in ["id=1", "id=2", "u=10", "m=100"] {
            assert_eq!(
                query(db, &format!("SELECT id,u,m,payload FROM t WHERE {predicate}")).len(),
                1
            );
        }
    }
    assert_truth(db, &original());
}

fn assert_truth(db: &EmbeddedDatabase, rows: &[Vec<Value>]) {
    assert!(
        !db.storage.has_uncommitted_writes(),
        "no census may conceal stale authoritative indexes"
    );
    assert_eq!(raw_heap(db), rows, "raw heap");
    assert_eq!(
        query(db, "SELECT id,u,m,payload FROM t ORDER BY id"),
        rows,
        "SQL full row/cache"
    );
    let count = query(db, "SELECT COUNT(*) FROM t");
    assert_eq!(integer(&count[0][0]), rows.len() as i64, "unfiltered ART cardinality");
    // Exhaust old/new/missing keys in every tree, including exact SQL values.
    for (position, column, domain) in [
        (0, "id", vec![1, 2, 3, 11, 50, 90, 999]),
        (1, "u", vec![10, 20, 30, 110, 500, 900, 999]),
        (2, "m", vec![100, 200, 300, 1100, 5000, 9000, 999]),
    ] {
        assert!(
            db.storage.art_indexes().find_column_index("t", column).is_some(),
            "{column} tree is live"
        );
        for key in domain {
            let matching: Vec<_> = rows
                .iter()
                .filter(|row| integer(&row[position]) == key)
                .cloned()
                .collect();
            assert_eq!(
                query(
                    db,
                    &format!("SELECT id,u,m,payload FROM t WHERE {column}={key} ORDER BY id")
                ),
                matching,
                "tree {column}={key}"
            );
            let counts = query(db, &format!("SELECT COUNT(*) FROM t WHERE {column}={key}"));
            assert_eq!(integer(&counts[0][0]), matching.len() as i64, "COUNT {column}={key}");
        }
    }
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
        .expect("failed COMMIT must retire its snapshot announcement");
    waiter.join().unwrap();
}

fn assert_enforced(db: &EmbeddedDatabase, rows: &[Vec<Value>]) {
    for existing in rows {
        assert!(db
            .execute(&format!(
                "INSERT INTO t VALUES({},999,999,'duplicate pk')",
                integer(&existing[0])
            ))
            .is_err());
        assert!(db
            .execute(&format!(
                "INSERT INTO t VALUES(999,{},999,'duplicate unique')",
                integer(&existing[1])
            ))
            .is_err());
    }
    assert_truth(db, rows);
}

fn commit_new_transaction(db: &EmbeddedDatabase, route: Route) {
    let sql = "INSERT INTO t VALUES(50,500,5000,'new transaction')";
    match route.owner {
        Owner::Session => {
            let session = route.session.unwrap();
            db.begin_transaction_for_session(session).unwrap();
            execute(db, route, sql).unwrap();
            db.commit_transaction_for_session(session).unwrap();
        }
        Owner::Raii => {
            let txn = db.begin_transaction().unwrap();
            txn.execute(sql).unwrap();
            txn.commit().unwrap();
        }
        Owner::Global => {
            execute(db, route, "BEGIN").unwrap();
            execute(db, route, sql).unwrap();
            execute(db, route, "COMMIT").unwrap();
        }
        Owner::Implicit => {
            execute(db, route, sql).unwrap();
        }
        Owner::Batch => {
            db.execute_batch(&[sql]).unwrap();
        }
    }
}

fn new_transaction_roundtrip(db: &EmbeddedDatabase, route: Route, rows: &[Vec<Value>]) {
    // Reuse the same global/session owner when available, so a stale undo log
    // from the failed COMMIT cannot remove previously committed keys later.
    match route.owner {
        Owner::Session => {
            let session = route.session.unwrap();
            db.begin_transaction_for_session(session).unwrap();
            execute(db, route, "INSERT INTO t VALUES(90,900,9000,'rollback')").unwrap();
            db.rollback_transaction_for_session(session).unwrap();
        }
        Owner::Raii => {
            let txn = db.begin_transaction().unwrap();
            txn.execute("INSERT INTO t VALUES(90,900,9000,'rollback')").unwrap();
            txn.rollback().unwrap();
        }
        _ => {
            execute(db, route, "BEGIN").unwrap();
            execute(db, route, "INSERT INTO t VALUES(90,900,9000,'rollback')").unwrap();
            execute(db, route, "ROLLBACK").unwrap();
        }
    }
    assert_truth(db, rows);
}

fn case_pass(outcome: &str, owner: Owner, family: Family, change: Change, history: bool) {
    let owner = match owner {
        Owner::Global => "global",
        Owner::Session => "session",
        Owner::Raii => "raii",
        Owner::Implicit => "implicit",
        Owner::Batch => "batch",
    };
    let family = match family {
        Family::Text => "text",
        Family::Params => "params",
    };
    let change = match change {
        Change::Insert => "insert",
        Change::Move => "pk_unique_manual_move",
        Change::Delete => "delete",
    };
    eprintln!(
        "COMMIT_OUTCOME_CASE {}",
        serde_json::json!({"outcome":outcome,"owner":owner,"family":family,"time_travel":history,"change":change})
    );
}

#[test]
fn cap_lowered_after_staged_dml_restores_global_session_raii_heap_and_all_keys() {
    for (owner, family) in EXPLICIT {
        for change in CHANGES {
            for history in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let final_rows;
                {
                    let db = open(dir.path(), history);
                    setup(&db);
                    let route = Route::new(&db, owner, family);
                    let (result, before) = attempt(&db, route, change, Fault::Cap);
                    assert!(result.is_err(), "{route:?} {change:?}: post-staging cap must refuse");
                    assert_eq!(
                        db.storage.db().latest_sequence_number(),
                        before,
                        "no native group after true refusal"
                    );
                    assert_truth(&db, &original());
                    barrier_progress(&db);
                    new_transaction_roundtrip(&db, route, &original());
                    attempt(&db, route, change, Fault::None).0.unwrap();
                    final_rows = expected(change, false);
                    assert_truth(&db, &final_rows);
                    assert_enforced(&db, &final_rows);
                    route.finish(&db);
                }
                let db = open(dir.path(), history);
                assert_truth(&db, &final_rows);
                assert_enforced(&db, &final_rows);
                case_pass("post_staging_cap", owner, family, change, history);
            }
        }
    }
}

#[test]
fn implicit_and_batch_cap_refusal_restore_heap_and_keys_and_allow_exact_retry() {
    for (owner, family) in IMPLICIT {
        for change in CHANGES {
            for history in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let final_rows;
                {
                    let db = open(dir.path(), history);
                    setup(&db);
                    let route = Route::new(&db, owner, family);
                    let (result, before) = attempt(&db, route, change, Fault::Cap);
                    assert!(result.is_err(), "{route:?} {change:?}: cap must refuse");
                    assert_eq!(db.storage.db().latest_sequence_number(), before);
                    assert_truth(&db, &original());
                    barrier_progress(&db);
                    attempt(&db, route, change, Fault::None).0.unwrap();
                    final_rows = expected(change, matches!(owner, Owner::Batch));
                    assert_truth(&db, &final_rows);
                    assert_enforced(&db, &final_rows);
                    route.finish(&db);
                }
                let db = open(dir.path(), history);
                assert_truth(&db, &final_rows);
                case_pass("implicit_or_batch_cap_before_execute", owner, family, change, history);
            }
        }
    }
}

#[test]
fn applied_then_error_retains_every_owners_committed_heap_keys_and_enforcement() {
    for (owner, family) in EXPLICIT.into_iter().chain(IMPLICIT) {
        for change in CHANGES {
            for history in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let mut final_rows = expected(change, matches!(owner, Owner::Batch));
                {
                    let db = open(dir.path(), history);
                    setup(&db);
                    let route = Route::new(&db, owner, family);
                    let (result, before) = attempt(&db, route, change, Fault::Applied);
                    let error = result.expect_err("late applied error must reach its owner");
                    assert!(
                        error.to_string().contains("after the commit applied"),
                        "{route:?} {change:?}: {error}"
                    );
                    assert!(db.storage.db().latest_sequence_number() > before, "real group applied");
                    assert_truth(&db, &final_rows);
                    assert_enforced(&db, &final_rows);
                    barrier_progress(&db);
                    // A retry observes committed truth: INSERT conflicts; old-ID
                    // UPDATE/DELETE affect zero rows and cannot undo the first commit.
                    let retry = execute(&db, route, change.sql());
                    match change {
                        Change::Insert => assert!(retry.is_err()),
                        _ => assert_eq!(retry.unwrap(), 0),
                    }
                    assert_truth(&db, &final_rows);
                    new_transaction_roundtrip(&db, route, &final_rows);
                    commit_new_transaction(&db, route);
                    final_rows.push(row(50, 500, 5000, "new transaction"));
                    final_rows.sort_by_key(|row| integer(&row[0]));
                    assert_truth(&db, &final_rows);
                    route.finish(&db);
                }
                let db = open(dir.path(), history);
                assert_truth(&db, &final_rows);
                assert_enforced(&db, &final_rows);
                case_pass("known_applied_error", owner, family, change, history);
            }
        }
    }
}

#[test]
fn ambiguous_native_error_fences_every_owners_table_until_reopen_recovers_actual_truth() {
    for (owner, family) in EXPLICIT.into_iter().chain(IMPLICIT) {
        for change in CHANGES {
            for history in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let final_rows = expected(change, matches!(owner, Owner::Batch));
                {
                    let db = open(dir.path(), history);
                    setup(&db);
                    let route = Route::new(&db, owner, family);
                    let (result, before) = attempt(&db, route, change, Fault::Unknown);
                    let error = result.expect_err("ambiguous native error must remain an error");
                    assert!(
                        error.to_string().contains("injected native write error after apply"),
                        "{route:?} {change:?}: {error}"
                    );
                    assert!(db.storage.db().latest_sequence_number() > before);
                    assert_eq!(raw_heap(&db), final_rows, "the injected native write actually applied");
                    assert!(!db.storage.has_uncommitted_writes());
                    barrier_progress(&db);
                    assert!(
                        db.query("SELECT id,u,m,payload FROM t", &[]).is_err(),
                        "full read fence"
                    );
                    assert!(
                        db.query("SELECT id FROM t WHERE id=1", &[]).is_err(),
                        "warmed PK/cache read fence"
                    );
                    assert!(
                        db.query_params("SELECT COUNT(*) FROM t", &[]).is_err(),
                        "params cardinality read fence"
                    );
                    assert!(
                        db.storage.art_indexes().list_table_indexes("t").is_empty(),
                        "retire unreliable enforcement trees"
                    );
                    assert!(execute(&db, route, "INSERT INTO t VALUES(50,500,5000,'must refuse')").is_err());
                    assert!(db
                        .storage
                        .insert_tuple_versioned("t", Tuple::new(row(50, 500, 5000, "direct must refuse")))
                        .is_err());
                    assert_eq!(
                        raw_heap(&db),
                        final_rows,
                        "retired trees must not enable duplicate-accepting writes"
                    );
                    db.execute("INSERT INTO healthy VALUES(7)").unwrap();
                    assert_eq!(query(&db, "SELECT id FROM healthy"), vec![vec![Value::Int4(7)]]);
                    route.finish(&db);
                }
                let db = open(dir.path(), history);
                assert_truth(&db, &final_rows);
                assert_enforced(&db, &final_rows);
                // Reopen clears only RAM quarantine, rebuilding actual durable
                // trees. A subsequent new commit proves normal service resumed.
                db.execute("INSERT INTO t VALUES(50,500,5000,'after reopen')").unwrap();
                let mut with_new = final_rows.clone();
                with_new.push(row(50, 500, 5000, "after reopen"));
                with_new.sort_by_key(|row| integer(&row[0]));
                assert_truth(&db, &with_new);
                case_pass("unknown_native_error", owner, family, change, history);
            }
        }
    }
}
