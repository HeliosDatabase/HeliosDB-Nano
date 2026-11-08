//! Public caller-transaction scope coverage beyond the native baseline.
//! Each cell is process-bounded; no private scope API or shipping hook is used.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::{Config, EmbeddedDatabase, Result, Tuple, Value};
use rocksdb::{IteratorMode, ReadOptions};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CHILD_CELL: &str = "HELIOSDB_ROUTINE_TRANSACTION_SCOPE_CELL";
const CHILD_ROOT: &str = "HELIOSDB_ROUTINE_TRANSACTION_SCOPE_ROOT";
const CHILD_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Clone, Copy, Debug)]
enum Family {
    Text,
    Params,
}
impl Family {
    fn label(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Params => "params",
        }
    }
    fn exec(self, db: &EmbeddedDatabase, sid: SessionId, sql: &str) -> Result<u64> {
        match self {
            Self::Text => db.execute_for_session(sid, sql),
            Self::Params => db.execute_params_for_session(sid, sql, &[]),
        }
    }
    fn query(self, db: &EmbeddedDatabase, sid: SessionId, sql: &str) -> Result<Vec<Tuple>> {
        match self {
            Self::Text => db.query_in_session(sid, sql, &[]),
            Self::Params => db.query_params_for_session(sid, sql, &[]),
        }
    }
}

fn integer(value: &Value) -> i64 {
    match value {
        Value::Int2(v) => i64::from(*v),
        Value::Int4(v) => i64::from(*v),
        Value::Int8(v) => *v,
        other => panic!("expected integer, got {other:?}"),
    }
}
fn scalar(rows: &[Tuple]) -> i64 {
    assert_eq!(rows.len(), 1, "one scalar row");
    assert_eq!(rows[0].values.len(), 1, "one scalar column");
    integer(&rows[0].values[0])
}
fn ids(db: &EmbeddedDatabase, family: Family, sid: SessionId, table: &str) -> Vec<i64> {
    family
        .query(db, sid, &format!("SELECT id FROM {table} ORDER BY id"))
        .unwrap()
        .iter()
        .map(|row| integer(&row.values[0]))
        .collect()
}
fn image(db: &EmbeddedDatabase) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut options = ReadOptions::default();
    options.set_total_order_seek(true);
    db.storage
        .db()
        .iterator_opt(IteratorMode::Start, options)
        .map(|item| {
            let (key, value) = item.unwrap();
            (key.to_vec(), value.to_vec())
        })
        .collect()
}
fn assert_image(db: &EmbeddedDatabase, before: &BTreeMap<Vec<u8>, Vec<u8>>, sequence: u64) {
    let after = image(db);
    let changed = before
        .keys()
        .chain(after.keys())
        .filter(|key| before.get(*key) != after.get(*key))
        .map(|key| String::from_utf8_lossy(key).into_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(changed.is_empty(), "transaction/read changed durable keys: {changed:?}");
    assert_eq!(
        db.storage.db().latest_sequence_number(),
        sequence,
        "no owning native group before COMMIT"
    );
}
fn open(path: &Path, history: bool) -> EmbeddedDatabase {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    config.storage.time_travel_enabled = history;
    config.storage.wal_enabled = true;
    config.storage.logical_wal_per_statement = false;
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
fn session(db: &EmbeddedDatabase, family: Family, name: &str, branch: &str) -> SessionId {
    let sid = db.create_session(name, IsolationLevel::ReadCommitted).unwrap();
    family.exec(db, sid, &format!("USE BRANCH {branch}")).unwrap();
    sid
}
fn seed(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE rb_source (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE rb_audit (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO rb_source VALUES (1)").unwrap();
    db.execute("INSERT INTO rb_audit VALUES (101)").unwrap();
    // Shared definitions are created on main, before entering the branch.
    db.execute("CREATE FUNCTION rb_count() RETURNS BIGINT AS $$ SELECT COUNT(*) FROM rb_source $$ LANGUAGE sql")
        .unwrap();
    db.execute("CREATE FUNCTION rb_log(n INT) RETURNS INT AS $$ BEGIN INSERT INTO rb_audit VALUES ($1); RETURN $1; END $$ LANGUAGE plpgsql")
        .unwrap();
    db.execute("CREATE PROCEDURE rb_copy() LANGUAGE sql AS $$ INSERT INTO rb_audit SELECT id + 1000 FROM rb_source $$")
        .unwrap();
    db.execute("CREATE FUNCTION rb_outer() RETURNS BIGINT AS $$ SELECT rb_count() $$ LANGUAGE sql")
        .unwrap();
    db.execute("CREATE FUNCTION rb_fail() RETURNS BIGINT AS $$ SELECT id FROM rb_missing $$ LANGUAGE sql")
        .unwrap();
    db.execute("CREATE TABLE rb_expr (id INT PRIMARY KEY, v BIGINT DEFAULT rb_outer())")
        .unwrap();
    db.execute("CREATE BRANCH rb_child AS OF NOW").unwrap();
    db.with_request_branch_scope("rb_child", || {
        db.execute("INSERT INTO rb_source VALUES (2)")?;
        db.execute("INSERT INTO rb_audit VALUES (202)")?;
        Ok(())
    })
    .unwrap();
}
fn committed_truth(db: &EmbeddedDatabase, family: Family) {
    let main = session(db, family, "routine_main_observer", "main");
    let branch = session(db, family, "routine_fresh_branch_observer", "rb_child");
    assert_eq!(ids(db, family, main, "rb_source"), [1]);
    assert_eq!(ids(db, family, main, "rb_audit"), [101]);
    assert_eq!(ids(db, family, branch, "rb_source"), [1, 2]);
    assert_eq!(ids(db, family, branch, "rb_audit"), [101, 202]);
    db.destroy_session(main).unwrap();
    db.destroy_session(branch).unwrap();
}

impl Family {
    fn returning(self, db: &EmbeddedDatabase, sid: SessionId, sql: &str) -> Result<(u64, Vec<Tuple>)> {
        match self {
            Self::Text => db.execute_returning_for_session(sid, sql),
            Self::Params => db.execute_params_returning_for_session(sid, sql, &[]),
        }
    }
}
fn tuple_ids(rows: &[Tuple]) -> Vec<i64> {
    rows.iter().map(|row| integer(&row.values[0])).collect()
}
fn other_truth(db: &EmbeddedDatabase, before: &BTreeMap<Vec<u8>, Vec<u8>>, sequence: u64) {
    assert_eq!(scalar(&db.query("SELECT rb_outer()", &[]).unwrap()), 5);
    assert_eq!(
        tuple_ids(&db.query("SELECT id FROM rb_audit ORDER BY id", &[]).unwrap()),
        [101]
    );
    assert!(db.query("SELECT id FROM rb_expr", &[]).unwrap().is_empty());
    assert_image(db, before, sequence);
}
fn raii_case(db: &EmbeddedDatabase, other: &EmbeddedDatabase, history: bool) {
    db.switch_branch("rb_child").unwrap();
    let before = image(db);
    let sequence = db.storage.db().latest_sequence_number();
    let other_before = image(other);
    let other_sequence = other.storage.db().latest_sequence_number();
    {
        let tx = db.begin_transaction().unwrap();
        tx.execute("INSERT INTO rb_source VALUES (3)").unwrap();
        assert_eq!(
            tuple_ids(&tx.query("SELECT id FROM rb_source ORDER BY id", &[]).unwrap()),
            [1, 2, 3]
        );
        assert_image(db, &before, sequence);
        println!("RTS_READY kind=raii family=text history={history}");
        std::io::stdout().flush().unwrap();
        // B was opened last and defines the same names but counts5. Direct
        // DEFAULT evaluation must bind A's engine before an Executor exists.
        tx.execute("INSERT INTO rb_expr (id) VALUES (10)").unwrap();
        assert_eq!(
            scalar(&tx.query("SELECT v FROM rb_expr WHERE id = 10", &[]).unwrap()),
            3
        );
        assert_eq!(scalar(&tx.query("SELECT rb_outer()", &[]).unwrap()), 3);
        tx.execute("PREPARE rb_saved AS SELECT rb_outer()").unwrap();
        tx.execute("PREPARE rb_arg AS SELECT $1").unwrap();
        assert_eq!(scalar(&tx.query("EXECUTE rb_saved", &[]).unwrap()), 3);
        assert_eq!(scalar(&tx.query("EXECUTE rb_arg(rb_count())", &[]).unwrap()), 3);
        assert_eq!(scalar(&tx.query("SELECT rb_log(303)", &[]).unwrap()), 303);
        assert_eq!(
            tuple_ids(&tx.query("SELECT id FROM rb_audit ORDER BY id", &[]).unwrap()),
            [101, 202, 303]
        );
        tx.rollback().unwrap();
    }
    committed_truth(db, Family::Text);
    assert!(db.query("SELECT id FROM rb_expr", &[]).unwrap().is_empty());
    assert_eq!(scalar(&db.query("SELECT rb_outer()", &[]).unwrap()), 2);
    assert_image(db, &before, sequence);
    other_truth(other, &other_before, other_sequence);
    {
        let tx = db.begin_transaction().unwrap();
        tx.execute("INSERT INTO rb_source VALUES (3)").unwrap();
        tx.execute("SAVEPOINT rb_saved_point").unwrap();
        assert_eq!(scalar(&tx.query("SELECT rb_log(304)", &[]).unwrap()), 304);
        tx.execute("ROLLBACK TO SAVEPOINT rb_saved_point").unwrap();
        assert_eq!(
            tuple_ids(&tx.query("SELECT id FROM rb_audit ORDER BY id", &[]).unwrap()),
            [101, 202]
        );
        assert_eq!(scalar(&tx.query("SELECT rb_outer()", &[]).unwrap()), 3);
        assert_image(db, &before, sequence);
        tx.commit().unwrap();
    }
    assert_eq!(
        tuple_ids(&db.query("SELECT id FROM rb_source ORDER BY id", &[]).unwrap()),
        [1, 2, 3]
    );
    assert_eq!(scalar(&db.query("SELECT rb_outer()", &[]).unwrap()), 3);
    let after_commit = image(db);
    let after_commit_sequence = db.storage.db().latest_sequence_number();
    {
        let tx = db.begin_transaction().unwrap();
        let error = tx
            .query("SELECT rb_fail()", &[])
            .expect_err("the actual body relation is absent");
        assert!(error.to_string().contains("rb_missing"), "exact body failure: {error}");
        let aborted = tx
            .query("SELECT rb_outer()", &[])
            .expect_err("outer failed-transaction owner remains authoritative");
        assert!(
            aborted.to_string().contains("current transaction is aborted"),
            "{aborted}"
        );
        tx.rollback().unwrap();
    }
    assert_eq!(
        scalar(&db.query("SELECT rb_outer()", &[]).unwrap()),
        3,
        "borrowed scope restored after body error and rollback"
    );
    assert_image(db, &after_commit, after_commit_sequence);
    other_truth(other, &other_before, other_sequence);
    let main = session(db, Family::Text, "raii_main_control", "main");
    assert_eq!(ids(db, Family::Text, main, "rb_source"), [1]);
    assert_eq!(ids(db, Family::Text, main, "rb_audit"), [101]);
    db.destroy_session(main).unwrap();
}
fn session_dml_case(db: &EmbeddedDatabase, other: &EmbeddedDatabase, family: Family, history: bool) {
    let before = image(db);
    let sequence = db.storage.db().latest_sequence_number();
    let other_before = image(other);
    let other_sequence = other.storage.db().latest_sequence_number();
    let sid = session(db, family, "scope_dml_owner", "rb_child");
    db.begin_transaction_for_session(sid).unwrap();
    family.exec(db, sid, "INSERT INTO rb_source VALUES (3)").unwrap();
    assert_eq!(ids(db, family, sid, "rb_source"), [1, 2, 3]);
    assert_image(db, &before, sequence);
    println!("RTS_READY kind=dml family={} history={history}", family.label());
    std::io::stdout().flush().unwrap();
    family.exec(db, sid, "INSERT INTO rb_expr (id) VALUES (10)").unwrap();
    assert_eq!(
        scalar(&family.query(db, sid, "SELECT v FROM rb_expr WHERE id = 10").unwrap()),
        3
    );
    family.exec(db, sid, "PREPARE rb_saved AS SELECT rb_outer()").unwrap();
    family.exec(db, sid, "PREPARE rb_arg AS SELECT $1").unwrap();
    assert_eq!(scalar(&family.query(db, sid, "EXECUTE rb_saved").unwrap()), 3);
    assert_eq!(scalar(&family.query(db, sid, "EXECUTE rb_arg(rb_count())").unwrap()), 3);
    let (count, rows) = family
        .returning(
            db,
            sid,
            "UPDATE rb_expr SET v = rb_outer() + 1 WHERE id = 10 RETURNING rb_outer()",
        )
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(scalar(&rows), 3);
    assert_eq!(
        scalar(&family.query(db, sid, "SELECT v FROM rb_expr WHERE id = 10").unwrap()),
        4
    );
    let (count, rows) = family
        .returning(
            db,
            sid,
            "INSERT INTO rb_expr (id,v) VALUES (11,rb_log(303)) RETURNING v",
        )
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(scalar(&rows), 303);
    assert_eq!(ids(db, family, sid, "rb_audit"), [101, 202, 303]);
    // A different session of A and a different engine B must not inherit the
    // caller transaction after its lexical statement scope has returned.
    let main = session(db, family, "scope_other_session", "main");
    assert_eq!(scalar(&family.query(db, main, "SELECT rb_outer()").unwrap()), 1);
    other_truth(other, &other_before, other_sequence);
    assert_eq!(scalar(&family.query(db, sid, "SELECT rb_outer()").unwrap()), 3);
    assert_image(db, &before, sequence);
    db.rollback_transaction_for_session(sid).unwrap();
    db.destroy_session(sid).unwrap();
    db.destroy_session(main).unwrap();
    committed_truth(db, family);
    assert!(db
        .with_request_branch_scope("rb_child", || db.query("SELECT id FROM rb_expr", &[]))
        .unwrap()
        .is_empty());
    assert_image(db, &before, sequence);
    other_truth(other, &other_before, other_sequence);
}
fn child_case(kind: &str, family: Family, history: bool, path: &Path) {
    let db = open(path, history);
    seed(&db);
    committed_truth(&db, family);
    // Keep the newer database alive with divergent same-name function results.
    let other = open(&path.with_file_name("other"), history);
    seed(&other);
    other
        .execute("INSERT INTO rb_source VALUES (10),(11),(12),(13)")
        .unwrap();
    match kind {
        "raii" => raii_case(&db, &other, history),
        "dml" => session_dml_case(&db, &other, family, history),
        _ => panic!("unknown scope case"),
    }
    println!("RTS_CELL_OK kind={kind} family={} history={history}", family.label());
}
fn run(kind: &str, test_name: &str) {
    if let Ok(cell) = std::env::var(CHILD_CELL) {
        let parts = cell.split(':').collect::<Vec<_>>();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], kind);
        let family = match parts[1] {
            "text" => Family::Text,
            "params" => Family::Params,
            _ => panic!("invalid child family"),
        };
        let history = parts[2].parse::<bool>().unwrap();
        child_case(kind, family, history, Path::new(&std::env::var(CHILD_ROOT).unwrap()));
        return;
    }
    let mut failures = Vec::new();
    for family in [Family::Text, Family::Params] {
        if kind == "raii" && matches!(family, Family::Params) {
            continue;
        }
        for history in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let log_path = root.path().join("child.log");
            let log = File::create(&log_path).unwrap();
            let cell = format!("{kind}:{}:{history}", family.label());
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
                .env(CHILD_CELL, &cell)
                .env(CHILD_ROOT, root.path().join("data"))
                .env_remove("__RUST_TEST_INVOKE")
                .stdin(Stdio::null())
                .stdout(Stdio::from(log.try_clone().unwrap()))
                .stderr(Stdio::from(log));
            let mut child = command.spawn().unwrap();
            let started = Instant::now();
            let (status, timed_out) = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break (status, false);
                }
                if started.elapsed() >= CHILD_TIMEOUT {
                    child.kill().unwrap();
                    break (child.wait().unwrap(), true);
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            let text = std::fs::read_to_string(&log_path).unwrap();
            print!("{text}");
            let marker = format!("RTS_CELL_OK kind={kind} family={} history={history}", family.label());
            let passed = status.success() && !timed_out && text.lines().filter(|line| *line == marker).count() == 1;
            println!(
                "RTS_CHILD kind={kind} family={} history={history} exit={status} timed_out={timed_out} passed={passed}",
                family.label()
            );
            if !passed {
                failures.push(format!("{cell}: exit={status}, timed_out={timed_out}"));
            }
        }
    }
    assert!(failures.is_empty(), "routine transaction child failures: {failures:?}");
}

#[test]
fn raii_nested_routines_follow_rollback_savepoints_and_restore_after_error() {
    run(
        "raii",
        "raii_nested_routines_follow_rollback_savepoints_and_restore_after_error",
    );
}
#[test]
fn session_dml_routines_share_default_values_update_returning_and_prepared_callers() {
    run(
        "dml",
        "session_dml_routines_share_default_values_update_returning_and_prepared_callers",
    );
}
