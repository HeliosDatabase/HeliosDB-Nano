//! Scalar routine bodies must use the calling branch transaction, like CALL.
//!
//! Each family/history cell runs in a bounded native child. The body table and
//! audit table are distinct so an INSERT body cannot merely wait on a row that
//! its caller already owns. These are public session APIs, not storage hooks.
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

const CHILD_CELL: &str = "HELIOSDB_ROUTINE_BRANCH_CONTEXT_CELL";
const CHILD_ROOT: &str = "HELIOSDB_ROUTINE_BRANCH_CONTEXT_ROOT";
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

fn child_case(kind: &str, family: Family, history: bool, path: &Path) {
    let db = open(path, history);
    seed(&db);
    committed_truth(&db, family);
    let sid = session(&db, family, "routine_caller", "rb_child");
    assert_eq!(scalar(&family.query(&db, sid, "SELECT rb_count()").unwrap()), 2);
    let before = image(&db);
    let sequence = db.storage.db().latest_sequence_number();
    db.begin_transaction_for_session(sid).unwrap();
    family.exec(&db, sid, "INSERT INTO rb_source VALUES (3)").unwrap();
    assert_eq!(
        ids(&db, family, sid, "rb_source"),
        [1, 2, 3],
        "ordinary caller sees its staged row"
    );
    assert_image(&db, &before, sequence);
    println!("RBC_READY kind={kind} family={} history={history}", family.label());
    std::io::stdout().flush().unwrap();
    let result = match kind {
        "read" => family.query(&db, sid, "SELECT rb_count()"),
        "write" => family.query(&db, sid, "SELECT rb_log(303)"),
        "call" => family.exec(&db, sid, "CALL rb_copy()").map(|_| Vec::new()),
        _ => panic!("unknown child kind"),
    };
    // Record the exact result before cleanup/assertion so a refusal is never
    // silently accepted as transaction participation.
    println!(
        "RBC_RESULT kind={kind} family={} history={history} result={result:?}",
        family.label()
    );
    let audit_during = if result.is_ok() {
        ids(&db, family, sid, "rb_audit")
    } else {
        Vec::new()
    };
    let after_body_sequence = db.storage.db().latest_sequence_number();
    db.rollback_transaction_for_session(sid).unwrap();
    db.destroy_session(sid).unwrap();
    println!("RBC_ROLLED_BACK kind={kind} family={} history={history} audit_during={audit_during:?} native_before={sequence} native_after_body={after_body_sequence}", family.label());
    // These are fresh public sessions, distinct from the caller's transaction.
    committed_truth(&db, family);
    assert_image(&db, &before, sequence);
    let rows = result.expect("already-supported session routine invocation must succeed, not refuse");
    match kind {
        "read" => {
            assert_eq!(
                scalar(&rows),
                3,
                "SQL function body must see caller's staged branch row"
            );
            assert_eq!(audit_during, [101, 202]);
        }
        "write" => {
            assert_eq!(scalar(&rows), 303);
            assert_eq!(
                audit_during,
                [101, 202, 303],
                "scalar body participates before rollback"
            );
        }
        "call" => assert_eq!(
            audit_during,
            [101, 202, 1001, 1002, 1003],
            "CALL reads and writes the same caller transaction"
        ),
        _ => unreachable!(),
    }
    println!("RBC_CELL_OK kind={kind} family={} history={history}", family.label());
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
            let marker = format!("RBC_CELL_OK kind={kind} family={} history={history}", family.label());
            let passed = status.success() && !timed_out && text.lines().filter(|line| *line == marker).count() == 1;
            println!(
                "RBC_CHILD kind={kind} family={} history={history} exit={status} timed_out={timed_out} passed={passed}",
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
fn scalar_body_reads_callers_staged_branch_rows() {
    run("read", "scalar_body_reads_callers_staged_branch_rows");
}
#[test]
fn scalar_body_insert_is_discarded_by_callers_branch_rollback() {
    run("write", "scalar_body_insert_is_discarded_by_callers_branch_rollback");
}
#[test]
fn procedure_body_joins_the_same_branch_transaction_control() {
    run("call", "procedure_body_joins_the_same_branch_transaction_control");
}
