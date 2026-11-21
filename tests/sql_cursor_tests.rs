#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::many_single_char_names)]
//! SQL cursors on the engine: `DECLARE … CURSOR`, every `FETCH` direction,
//! `MOVE`, `CLOSE`, the transaction-block rule, `WITH HOLD`, the snapshot of
//! `DECLARE`, the size limit, `pg_cursors`, and the per-session namespace —
//! on both statement families and on engine-level sessions.

use heliosdb_nano::session::IsolationLevel;
use heliosdb_nano::{Config, EmbeddedDatabase, Error, StatementOutcome, Tuple, Value};

fn int(v: &Value) -> i64 {
    match v {
        Value::Int2(n) => i64::from(*n),
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        other => panic!("not an integer: {other:?}"),
    }
}

fn ids(rows: &[Tuple]) -> Vec<i64> {
    rows.iter().map(|r| int(&r.values[0])).collect()
}

fn sqlstate(e: &Error) -> String {
    e.raised_sqlstate().unwrap_or("-").to_string()
}

/// A table `t(id, val)` with ids 1..=n.
fn db_with_rows(n: i64) -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, val TEXT)").unwrap();
    for i in 1..=n {
        db.execute(&format!("INSERT INTO t VALUES ({i}, 'v{i}')")).unwrap();
    }
    db
}

fn fetch(db: &EmbeddedDatabase, sql: &str) -> Vec<i64> {
    ids(&db.query(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")))
}

#[test]
fn every_fetch_direction_returns_the_rows_postgresql_returns() {
    let db = db_with_rows(5);
    db.begin().unwrap();
    db.execute("DECLARE c SCROLL CURSOR FOR SELECT id, val FROM t ORDER BY id")
        .unwrap();
    assert_eq!(fetch(&db, "FETCH c"), [1]);
    assert_eq!(fetch(&db, "FETCH NEXT FROM c"), [2]);
    assert_eq!(fetch(&db, "FETCH 2 FROM c"), [3, 4]);
    assert_eq!(fetch(&db, "FETCH PRIOR FROM c"), [3]);
    assert_eq!(fetch(&db, "FETCH FIRST FROM c"), [1]);
    assert_eq!(fetch(&db, "FETCH LAST IN c"), [5]);
    assert_eq!(fetch(&db, "FETCH NEXT FROM c"), Vec::<i64>::new());
    assert_eq!(fetch(&db, "FETCH ABSOLUTE 2 FROM c"), [2]);
    assert_eq!(fetch(&db, "FETCH ABSOLUTE -1 FROM c"), [5]);
    assert_eq!(fetch(&db, "FETCH RELATIVE -2 FROM c"), [3]);
    assert_eq!(fetch(&db, "FETCH RELATIVE 0 FROM c"), [3]);
    assert_eq!(fetch(&db, "FETCH FORWARD FROM c"), [4]);
    assert_eq!(fetch(&db, "FETCH BACKWARD 2 FROM c"), [3, 2]);
    assert_eq!(fetch(&db, "FETCH BACKWARD ALL FROM c"), [1]);
    assert_eq!(fetch(&db, "FETCH FORWARD 2 FROM c"), [1, 2]);
    assert_eq!(fetch(&db, "FETCH FORWARD ALL FROM c"), [3, 4, 5]);
    assert_eq!(fetch(&db, "FETCH ALL FROM c"), Vec::<i64>::new());
    assert_eq!(fetch(&db, "FETCH -2 FROM c"), [5, 4]);
    // The other column travels with the row.
    let row = db.query("FETCH ABSOLUTE 1 FROM c", &[]).unwrap();
    assert_eq!(row[0].values[1], Value::String("v1".to_string()));
    db.commit().unwrap();
}

#[test]
fn move_repositions_without_returning_rows_and_reports_its_count() {
    let db = db_with_rows(5);
    db.begin().unwrap();
    db.execute("DECLARE c CURSOR FOR SELECT id FROM t ORDER BY id").unwrap();
    assert_eq!(db.execute("MOVE 2 FROM c").unwrap(), 2);
    assert_eq!(db.query("MOVE NEXT FROM c", &[]).unwrap().len(), 0);
    assert_eq!(fetch(&db, "FETCH c"), [4]);
    assert_eq!(db.execute("MOVE FORWARD ALL IN c").unwrap(), 1);
    assert_eq!(db.execute("MOVE c").unwrap(), 0);
    // `execute` of a FETCH reports how many rows it fetched.
    assert_eq!(db.execute("FETCH BACKWARD 3 FROM c").unwrap(), 3);
    db.rollback().unwrap();
}

#[test]
fn a_cursor_without_hold_needs_a_transaction_block_and_ends_with_it() {
    let db = db_with_rows(3);
    let err = db.execute("DECLARE c CURSOR FOR SELECT id FROM t").unwrap_err();
    assert_eq!(sqlstate(&err), "25P01", "{err}");

    db.begin().unwrap();
    db.execute("DECLARE c CURSOR FOR SELECT id FROM t ORDER BY id").unwrap();
    assert_eq!(fetch(&db, "FETCH c"), [1]);
    db.commit().unwrap();
    let err = db.query("FETCH c", &[]).unwrap_err();
    assert_eq!(sqlstate(&err), "34000", "{err}");

    db.begin().unwrap();
    db.execute("DECLARE c CURSOR FOR SELECT id FROM t ORDER BY id").unwrap();
    db.rollback().unwrap();
    assert_eq!(sqlstate(&db.query("FETCH c", &[]).unwrap_err()), "34000");
}

#[test]
fn with_hold_survives_commit_and_goes_with_the_rollback_of_its_transaction() {
    let db = db_with_rows(4);
    // Outside a block a held cursor may be declared.
    db.execute("DECLARE outside CURSOR WITH HOLD FOR SELECT id FROM t ORDER BY id")
        .unwrap();
    assert_eq!(fetch(&db, "FETCH 2 FROM outside"), [1, 2]);

    db.begin().unwrap();
    db.execute("DECLARE held CURSOR WITH HOLD FOR SELECT id FROM t ORDER BY id")
        .unwrap();
    db.execute("DECLARE plain CURSOR WITHOUT HOLD FOR SELECT id FROM t ORDER BY id")
        .unwrap();
    assert_eq!(fetch(&db, "FETCH held"), [1]);
    db.commit().unwrap();
    assert_eq!(fetch(&db, "FETCH held"), [2], "a held cursor keeps its position over COMMIT");
    assert_eq!(sqlstate(&db.query("FETCH plain", &[]).unwrap_err()), "34000");

    db.begin().unwrap();
    db.execute("DECLARE doomed CURSOR WITH HOLD FOR SELECT id FROM t").unwrap();
    db.rollback().unwrap();
    assert_eq!(sqlstate(&db.query("FETCH doomed", &[]).unwrap_err()), "34000");
    // Cursors committed earlier are not touched by a later rollback.
    assert_eq!(fetch(&db, "FETCH held"), [3]);
    assert_eq!(fetch(&db, "FETCH outside"), [3]);
    db.execute("CLOSE held").unwrap();
    db.execute("CLOSE outside").unwrap();
    assert_eq!(db.open_cursor_count(), 0);
}

#[test]
fn a_cursor_reads_the_rows_of_its_declare() {
    let db = db_with_rows(3);
    db.execute("DECLARE c CURSOR WITH HOLD FOR SELECT id FROM t ORDER BY id")
        .unwrap();
    db.execute("INSERT INTO t VALUES (4, 'late')").unwrap();
    db.execute("DELETE FROM t WHERE id = 1").unwrap();
    db.execute("UPDATE t SET id = 20 WHERE id = 2").unwrap();
    assert_eq!(fetch(&db, "FETCH ALL FROM c"), [1, 2, 3]);
    db.execute("CLOSE c").unwrap();
}

#[test]
fn the_result_of_a_cursor_is_bounded() {
    let mut config = Config::in_memory();
    config.session.cursor_max_rows = 3;
    let db = EmbeddedDatabase::with_config(config).unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO t VALUES (1), (2), (3), (4)").unwrap();
    let err = db
        .execute("DECLARE big CURSOR WITH HOLD FOR SELECT id FROM t")
        .unwrap_err();
    assert_eq!(sqlstate(&err), "54000", "{err}");
    assert!(err.to_string().contains("limit is 3"), "{err}");
    assert_eq!(db.open_cursor_count(), 0, "a refused cursor is not left open");
    // At the limit it is accepted.
    db.execute("DECLARE fits CURSOR WITH HOLD FOR SELECT id FROM t WHERE id <= 3")
        .unwrap();
    assert_eq!(db.query("FETCH ALL FROM fits", &[]).unwrap().len(), 3);
}

#[test]
fn names_errors_and_close() {
    let db = db_with_rows(3);
    db.execute("DECLARE c CURSOR WITH HOLD FOR SELECT id FROM t ORDER BY id")
        .unwrap();
    let err = db
        .execute("DECLARE c CURSOR WITH HOLD FOR SELECT id FROM t")
        .unwrap_err();
    assert_eq!(sqlstate(&err), "42P03", "{err}");
    assert_eq!(sqlstate(&db.execute("CLOSE nope").unwrap_err()), "34000");
    assert_eq!(sqlstate(&db.execute("MOVE nope").unwrap_err()), "34000");
    assert_eq!(sqlstate(&db.execute("FETCH sideways FROM c").unwrap_err()), "42601");
    // Only a query can be a cursor's statement.
    let err = db
        .execute("DECLARE w CURSOR WITH HOLD FOR DELETE FROM t")
        .unwrap_err();
    assert_eq!(sqlstate(&err), "42601", "{err}");
    assert_eq!(db.query("SELECT id FROM t", &[]).unwrap().len(), 3, "nothing was deleted");
    // A failing query leaves no cursor behind.
    assert!(db
        .execute("DECLARE bad CURSOR WITH HOLD FOR SELECT nope FROM t")
        .is_err());
    assert_eq!(sqlstate(&db.execute("CLOSE bad").unwrap_err()), "34000");

    db.execute("DECLARE d CURSOR WITH HOLD FOR SELECT id FROM t").unwrap();
    assert_eq!(db.open_cursor_count(), 2);
    db.execute("CLOSE c").unwrap();
    assert_eq!(sqlstate(&db.query("FETCH c", &[]).unwrap_err()), "34000");
    db.execute("CLOSE ALL").unwrap();
    assert_eq!(db.open_cursor_count(), 0);
    // A quoted name keeps its case; a bare one is folded.
    db.execute("DECLARE \"Mixed\" CURSOR WITH HOLD FOR SELECT id FROM t ORDER BY id")
        .unwrap();
    assert_eq!(fetch(&db, "FETCH \"Mixed\""), [1]);
    assert_eq!(sqlstate(&db.query("FETCH Mixed", &[]).unwrap_err()), "34000");
    db.execute("CLOSE ALL").unwrap();
}

#[test]
fn a_no_scroll_cursor_cannot_go_back() {
    let db = db_with_rows(3);
    db.execute("DECLARE c NO SCROLL CURSOR WITH HOLD FOR SELECT id FROM t ORDER BY id")
        .unwrap();
    assert_eq!(fetch(&db, "FETCH 2 FROM c"), [1, 2]);
    for sql in ["FETCH PRIOR FROM c", "FETCH FIRST FROM c", "FETCH BACKWARD ALL FROM c", "MOVE -1 FROM c"] {
        let err = db.query(sql, &[]).unwrap_err();
        assert_eq!(sqlstate(&err), "55000", "{sql}: {err}");
    }
    assert_eq!(fetch(&db, "FETCH c"), [3], "a refused move leaves the position");
    db.execute("CLOSE c").unwrap();
}

#[test]
fn bound_parameters_are_values_of_the_cursor_s_query() {
    let db = db_with_rows(5);
    db.execute_params(
        "DECLARE c CURSOR WITH HOLD FOR SELECT id FROM t WHERE id > $1 AND val <> $2 ORDER BY id",
        &[Value::Int4(2), Value::String("v4'; DROP TABLE t; --".to_string())],
    )
    .unwrap();
    let rows = db.query_params("FETCH ALL FROM c", &[]).unwrap();
    assert_eq!(ids(&rows), [3, 4, 5]);
    let (count, rows) = db.execute_params_returning("FETCH BACKWARD 2 FROM c", &[]).unwrap();
    assert_eq!(count, 2);
    assert_eq!(ids(&rows), [5, 4]);
    assert_eq!(db.execute_params("MOVE FIRST FROM c", &[]).unwrap(), 1);
    db.execute_params("CLOSE c", &[]).unwrap();
    assert_eq!(db.query("SELECT id FROM t", &[]).unwrap().len(), 5);
}

#[test]
fn run_statement_answers_a_fetch_with_the_cursor_s_columns() {
    let db = db_with_rows(2);
    match db
        .run_statement("DECLARE c CURSOR WITH HOLD FOR SELECT id AS k, val FROM t ORDER BY id")
        .unwrap()
    {
        StatementOutcome::Affected(0) => {}
        other => panic!("{other:?}"),
    }
    match db.run_statement("FETCH ALL FROM c").unwrap() {
        StatementOutcome::Rows { columns, rows } => {
            assert_eq!(columns, ["k", "val"]);
            assert_eq!(rows.len(), 2);
        }
        other => panic!("{other:?}"),
    }
    let (rows, columns) = db.query_with_columns("FETCH FIRST FROM c").unwrap();
    assert_eq!(columns, ["k", "val"]);
    assert_eq!(ids(&rows), [1]);
    match db.run_statement("MOVE LAST FROM c").unwrap() {
        StatementOutcome::Affected(1) => {}
        other => panic!("{other:?}"),
    }
    db.run_statement("CLOSE c").unwrap();
}

#[test]
fn pg_cursors_lists_the_session_s_open_cursors() {
    let db = db_with_rows(2);
    assert!(db.query("SELECT name FROM pg_cursors", &[]).unwrap().is_empty());
    db.execute("DECLARE b BINARY NO SCROLL CURSOR WITH HOLD FOR SELECT id FROM t")
        .unwrap();
    db.begin().unwrap();
    db.execute("DECLARE a CURSOR FOR SELECT val FROM t").unwrap();
    let rows = db
        .query(
            "SELECT name, statement, is_holdable, is_binary, is_scrollable FROM pg_cursors ORDER BY name",
            &[],
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].values,
        [
            Value::String("a".to_string()),
            Value::String("SELECT val FROM t".to_string()),
            Value::Boolean(false),
            Value::Boolean(false),
            Value::Boolean(true),
        ]
    );
    assert_eq!(
        rows[1].values,
        [
            Value::String("b".to_string()),
            Value::String("SELECT id FROM t".to_string()),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(false),
        ]
    );
    db.commit().unwrap();
    // The same text again: the list is read each time, not remembered.
    let rows = db
        .query(
            "SELECT name, statement, is_holdable, is_binary, is_scrollable FROM pg_cursors ORDER BY name",
            &[],
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    db.execute("CLOSE ALL").unwrap();
    assert!(db.query("SELECT name FROM pg_cursors", &[]).unwrap().is_empty());
}

#[test]
fn a_failed_cursor_statement_aborts_its_transaction_block() {
    let db = db_with_rows(2);
    db.begin().unwrap();
    db.execute("INSERT INTO t VALUES (9, 'nine')").unwrap();
    assert!(db.query("FETCH nope", &[]).is_err());
    // The block is failed: the next statement is refused, COMMIT rolls back.
    assert!(db.execute("INSERT INTO t VALUES (10, 'ten')").is_err());
    assert!(db.commit().is_err());
    assert_eq!(db.query("SELECT id FROM t", &[]).unwrap().len(), 2);
}

#[test]
fn two_sessions_each_have_their_own_cursor_of_one_name() {
    let db = db_with_rows(4);
    let a = db.create_session("alice", IsolationLevel::ReadCommitted).unwrap();
    let b = db.create_session("bob", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(a).unwrap();
    db.begin_transaction_for_session(b).unwrap();
    db.execute_for_session(a, "DECLARE c CURSOR FOR SELECT id FROM t WHERE id <= 2 ORDER BY id")
        .unwrap();
    db.execute_for_session(b, "DECLARE c CURSOR FOR SELECT id FROM t WHERE id > 2 ORDER BY id")
        .unwrap();
    assert_eq!(db.open_cursor_count(), 2);

    let (rows, columns) = db.query_with_columns_for_session(a, "FETCH ALL FROM c").unwrap();
    assert_eq!(columns, ["id"]);
    assert_eq!(ids(&rows), [1, 2]);
    let rows = db.query_params_for_session(b, "FETCH 1 FROM c", &[]).unwrap();
    assert_eq!(ids(&rows), [3]);
    // The handle's own session has no cursor of that name.
    assert_eq!(sqlstate(&db.query("FETCH c", &[]).unwrap_err()), "34000");

    // Each session lists only its own.
    let (listed, _) = db
        .query_with_columns_for_session(a, "SELECT name, statement FROM pg_cursors")
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert!(matches!(&listed[0].values[1], Value::String(s) if s.contains("id <= 2")));

    // Closing one session's cursor leaves the other's.
    assert_eq!(db.execute_for_session(a, "CLOSE c").unwrap(), 0);
    let (count, rows) = db.execute_returning_for_session(b, "FETCH c").unwrap();
    assert_eq!((count, ids(&rows)), (1, vec![4]));

    // The end of a session's block ends that session's cursor only.
    db.execute_for_session(a, "DECLARE c2 CURSOR FOR SELECT id FROM t").unwrap();
    db.commit_transaction_for_session(a).unwrap();
    assert_eq!(
        sqlstate(&db.execute_for_session(a, "FETCH c2").unwrap_err()),
        "34000"
    );
    assert_eq!(db.execute_params_for_session(b, "MOVE BACKWARD ALL IN c", &[]).unwrap(), 1);

    // Outside a block the rule is the same for a session.
    let err = db
        .execute_for_session(a, "DECLARE c3 CURSOR FOR SELECT id FROM t")
        .unwrap_err();
    assert_eq!(sqlstate(&err), "25P01", "{err}");
    db.execute_params_returning_for_session(a, "DECLARE c3 CURSOR WITH HOLD FOR SELECT id FROM t WHERE id = $1", &[Value::Int4(4)])
        .unwrap();
    assert_eq!(ids(&db.query_params_for_session(a, "FETCH c3", &[]).unwrap()), [4]);

    // A session that ends takes its cursors with it.
    db.rollback_transaction_for_session(b).unwrap();
    assert_eq!(db.open_cursor_count(), 1);
    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
    assert_eq!(db.open_cursor_count(), 0);
}

#[test]
fn a_session_cursor_reads_the_snapshot_of_its_transaction() {
    let db = db_with_rows(2);
    let a = db.create_session("alice", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(a).unwrap();
    db.execute_for_session(a, "INSERT INTO t VALUES (3, 'mine')").unwrap();
    db.execute_for_session(a, "DECLARE c CURSOR FOR SELECT id FROM t ORDER BY id")
        .unwrap();
    // Its own uncommitted row is in; a row committed elsewhere afterwards is not.
    db.execute("INSERT INTO t VALUES (4, 'later')").unwrap();
    let (rows, _) = db.query_with_columns_for_session(a, "FETCH ALL FROM c").unwrap();
    assert_eq!(ids(&rows), [1, 2, 3]);
    db.rollback_transaction_for_session(a).unwrap();
    db.destroy_session(a).unwrap();
}
