//! SQL prepared utilities share both executor families without binding a
//! PREPARE body to the outer call, leaking names, or retrying durable effects.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::storage::layout_epochs::is_layout_changed;
use heliosdb_nano::{EmbeddedDatabase, Error, Tuple, Value};

#[derive(Clone, Copy, Debug)]
enum Family {
    Text,
    Params,
}
const FAMILIES: [Family; 2] = [Family::Text, Family::Params];
fn run(db: &EmbeddedDatabase, family: Family, sql: &str) -> heliosdb_nano::Result<u64> {
    match family {
        Family::Text => db.execute(sql),
        Family::Params => db.execute_params(sql, &[]),
    }
}
fn session_run(db: &EmbeddedDatabase, family: Family, session: SessionId, sql: &str) -> heliosdb_nano::Result<u64> {
    match family {
        Family::Text => db.execute_for_session(session, sql),
        Family::Params => db.execute_params_for_session(session, sql, &[]),
    }
}
fn values(rows: Vec<Tuple>) -> Vec<Vec<Value>> {
    rows.into_iter().map(|tuple| tuple.values).collect()
}
fn integer(value: &Value) -> i64 {
    match value {
        Value::Int2(value) => i64::from(*value),
        Value::Int4(value) => i64::from(*value),
        Value::Int8(value) => *value,
        other => panic!("expected an integer, got {other:?}"),
    }
}
fn missing(error: Error, name: &str) {
    assert!(
        matches!(&error, Error::QueryExecution(message) if message == &format!("Prepared statement '{name}' does not exist")),
        "{error}"
    );
}

#[test]
fn prepare_keeps_inner_placeholders_and_execute_binds_outer_arguments_once() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE pp (id INT PRIMARY KEY, a INT)").unwrap();
        db.execute("INSERT INTO pp VALUES (1,10),(2,20)").unwrap();
        assert_eq!(
            run(
                &db,
                family,
                "PREPARE selected AS SELECT id,a FROM pp WHERE a > $1 ORDER BY id"
            )
            .unwrap(),
            0
        );
        let (rows, columns) = db
            .query_params_with_columns("EXECUTE selected($1)", &[Value::Int4(15)])
            .unwrap();
        assert_eq!(columns, vec!["id".to_string(), "a".to_string()]);
        assert_eq!(values(rows), vec![vec![Value::Int4(2), Value::Int4(20)]]);
        assert_eq!(db.execute_params("EXECUTE selected($1)", &[Value::Int4(0)]).unwrap(), 2);

        run(&db, family, "PREPARE ins AS INSERT INTO pp VALUES ($1,$2)").unwrap();
        assert_eq!(
            db.execute_params("EXECUTE ins($2,$1)", &[Value::Int4(30), Value::Int4(3)])
                .unwrap(),
            1
        );
        assert_eq!(
            values(db.query_params("EXECUTE selected($1)", &[Value::Int4(25)]).unwrap()),
            vec![vec![Value::Int4(3), Value::Int4(30)]]
        );
        run(&db, family, "BEGIN").unwrap();
        assert_eq!(
            db.execute_params("EXECUTE ins($2,$1)", &[Value::Int4(40), Value::Int4(4)])
                .unwrap(),
            1
        );
        assert_eq!(
            values(db.query_params("EXECUTE selected($1)", &[Value::Int4(35)]).unwrap()),
            vec![vec![Value::Int4(4), Value::Int4(40)]]
        );
        run(&db, family, "ROLLBACK").unwrap();
        assert!(db
            .query_params("EXECUTE selected($1)", &[Value::Int4(35)])
            .unwrap()
            .is_empty());
        let error = db
            .execute_params("PREPARE unbound AS SELECT $1", &[Value::Int4(999)])
            .unwrap_err();
        assert!(
            matches!(&error, Error::QueryExecution(message) if message == "PREPARE does not accept outer bind parameters"),
            "{error}"
        );
        missing(db.query_params("EXECUTE unbound(1)", &[]).unwrap_err(), "unbound");
        run(&db, family, "DEALLOCATE selected").unwrap();
        missing(db.query_params("EXECUTE selected(0)", &[]).unwrap_err(), "selected");
    }
}

#[test]
fn prepared_layout_replanning_preserves_result_shape_and_never_repeats_sequence_effects() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE pp (id INT PRIMARY KEY,a INT)").unwrap();
        db.execute("INSERT INTO pp VALUES (1,10),(2,20)").unwrap();
        run(
            &db,
            family,
            "PREPARE summary AS SELECT count(*),sum(a) FROM pp WHERE a > $1",
        )
        .unwrap();
        run(&db, family, "PREPARE every_column AS SELECT * FROM pp WHERE a > $1").unwrap();
        db.execute("ALTER TABLE pp ADD COLUMN unused INT DEFAULT 7").unwrap();
        let summary = db.query_params("EXECUTE summary($1)", &[Value::Int4(0)]).unwrap();
        let actual: Vec<_> = summary.iter().flat_map(|row| row.values.iter().map(integer)).collect();
        assert_eq!(actual, vec![2, 30]);
        let error = db
            .query_params("EXECUTE every_column($1)", &[Value::Int4(0)])
            .unwrap_err();
        assert!(
            error.to_string().contains("cached plan must not change result type"),
            "{error}"
        );

        db.execute("CREATE SEQUENCE utility_effect").unwrap();
        db.execute("CREATE TABLE effect_rows (id BIGINT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO effect_rows VALUES (10)").unwrap();
        run(&db, family, "PREPARE effect AS SELECT nextval('utility_effect') AS n UNION ALL SELECT 0 UNION ALL SELECT id FROM effect_rows").unwrap();
        db.execute("ALTER TABLE effect_rows ADD COLUMN unused INT").unwrap();
        let error = db.query_params("EXECUTE effect", &[]).unwrap_err();
        assert!(is_layout_changed(&error), "{error}");
        let next = db.query("SELECT nextval('utility_effect')", &[]).unwrap();
        assert_eq!(
            next.iter()
                .flat_map(|row| row.values.iter().map(integer))
                .collect::<Vec<_>>(),
            vec![2]
        );
    }
}

#[test]
fn sql_names_and_deallocate_all_are_scoped_to_the_connection() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let a = db.create_session("a", IsolationLevel::ReadCommitted).unwrap();
    let b = db.create_session("b", IsolationLevel::ReadCommitted).unwrap();
    db.execute_params_for_session(a, "PREPARE same AS SELECT 11 AS answer", &[])
        .unwrap();
    db.execute_for_session(b, "PREPARE same AS SELECT 22 AS answer")
        .unwrap();
    db.execute("PREPARE same AS SELECT 33 AS answer").unwrap();
    assert_eq!(
        values(db.query_params_for_session(a, "EXECUTE same", &[]).unwrap()),
        vec![vec![Value::Int4(11)]]
    );
    assert_eq!(
        values(db.query_params_for_session(b, "EXECUTE same", &[]).unwrap()),
        vec![vec![Value::Int4(22)]]
    );
    assert_eq!(
        values(db.query_params("EXECUTE same", &[]).unwrap()),
        vec![vec![Value::Int4(33)]]
    );
    db.execute_params_for_session(a, "DEALLOCATE ALL", &[]).unwrap();
    missing(db.query_params_for_session(a, "EXECUTE same", &[]).unwrap_err(), "same");
    assert_eq!(
        values(db.query_params_for_session(b, "EXECUTE same", &[]).unwrap()),
        vec![vec![Value::Int4(22)]]
    );
    assert_eq!(
        values(db.query_params("EXECUTE same", &[]).unwrap()),
        vec![vec![Value::Int4(33)]]
    );
    db.destroy_session(a).unwrap();
    assert_eq!(
        values(db.query_params_for_session(b, "EXECUTE same", &[]).unwrap()),
        vec![vec![Value::Int4(22)]]
    );
    db.execute("DEALLOCATE ALL").unwrap();
    assert_eq!(
        values(db.query_params_for_session(b, "EXECUTE same", &[]).unwrap()),
        vec![vec![Value::Int4(22)]]
    );
    db.destroy_session(b).unwrap();
}

#[test]
fn prepared_writes_keep_the_callers_transaction_and_failed_savepoint_boundary() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE owned (id INT PRIMARY KEY)").unwrap();
        let a = db.create_session("a", IsolationLevel::ReadCommitted).unwrap();
        let b = db.create_session("b", IsolationLevel::ReadCommitted).unwrap();
        session_run(&db, family, a, "BEGIN").unwrap();
        session_run(&db, family, a, "PREPARE ins AS INSERT INTO owned VALUES ($1)").unwrap();
        assert_eq!(
            db.execute_params_for_session(a, "EXECUTE ins($1)", &[Value::Int4(1)])
                .unwrap(),
            1
        );
        assert_eq!(
            db.query_params_for_session(a, "SELECT id FROM owned", &[])
                .unwrap()
                .len(),
            1
        );
        assert!(db
            .query_params_for_session(b, "SELECT id FROM owned", &[])
            .unwrap()
            .is_empty());
        session_run(&db, family, a, "SAVEPOINT s").unwrap();
        let error = db
            .execute_params_for_session(a, "EXECUTE ins($1)", &[Value::Int4(1)])
            .unwrap_err();
        assert!(matches!(error, Error::ConstraintViolation(_)), "{error}");
        let aborted = db
            .execute_params_for_session(a, "EXECUTE ins($1)", &[Value::Int4(2)])
            .unwrap_err();
        assert!(
            aborted.to_string().contains("current transaction is aborted"),
            "{aborted}"
        );
        session_run(&db, family, a, "ROLLBACK TO SAVEPOINT s").unwrap();
        session_run(&db, family, a, "COMMIT").unwrap();
        assert_eq!(
            values(db.query_params_for_session(b, "SELECT id FROM owned", &[]).unwrap()),
            vec![vec![Value::Int4(1)]]
        );
        db.destroy_session(a).unwrap();
        db.destroy_session(b).unwrap();
    }
}

#[test]
fn prepared_utility_bodies_are_refused_before_registration_without_evaluating_them() {
    for route in 0..4 {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let utility = |sql: &str| -> heliosdb_nano::Result<()> {
            match route {
                0 => db.execute(sql).map(|_| ()),
                1 => db.execute_params(sql, &[]).map(|_| ()),
                2 => db.query(sql, &[]).map(|_| ()),
                _ => db.query_params(sql, &[]).map(|_| ()),
            }
        };
        db.execute("CREATE TABLE still_usable (id INT PRIMARY KEY)").unwrap();
        db.execute("CREATE SEQUENCE never_evaluated").unwrap();
        for (name, body) in [
            ("self_ref", "EXECUTE self_ref"),
            ("left_ref", "EXECUTE right_ref"),
            ("right_ref", "EXECUTE left_ref"),
            ("nested", "PREPARE nested_inner AS SELECT nextval('never_evaluated')"),
            ("nested_execute", "PREPARE inner_execute AS EXECUTE nested_execute"),
            ("begin_body", "BEGIN"),
            ("start_body", "START TRANSACTION"),
            ("commit_body", "COMMIT"),
            ("rollback_body", "ROLLBACK"),
            ("save_body", "SAVEPOINT forbidden"),
            ("release_body", "RELEASE SAVEPOINT forbidden"),
            ("rollback_to_body", "ROLLBACK TO SAVEPOINT forbidden"),
        ] {
            let error = utility(&format!("PREPARE {name} AS {body}")).unwrap_err();
            assert!(
                matches!(&error, Error::QueryExecution(message) if message == "PREPARE body cannot be a prepared-statement or transaction-control utility"),
                "route {route}: {name}: {error}"
            );
            // Probe absence with DEALLOCATE, never EXECUTE an untrusted body:
            // a regressed guard must fail a bounded assertion, not recurse.
            missing(utility(&format!("DEALLOCATE {name}")).unwrap_err(), name);
            assert_eq!(db.execute("INSERT INTO still_usable VALUES (1)").unwrap(), 1);
            assert_eq!(db.execute("DELETE FROM still_usable WHERE id = 1").unwrap(), 1);
        }
        for name in ["nested_inner", "inner_execute"] {
            missing(utility(&format!("DEALLOCATE {name}")).unwrap_err(), name);
        }
        utility("PREPARE kept AS SELECT 99 AS answer").unwrap();
        let error = utility("PREPARE kept AS DEALLOCATE kept").unwrap_err();
        assert!(
            matches!(&error, Error::QueryExecution(message) if message == "PREPARE body cannot be a prepared-statement or transaction-control utility"),
            "{error}"
        );
        assert_eq!(
            values(db.query_params("EXECUTE kept", &[]).unwrap()),
            vec![vec![Value::Int4(99)]]
        );
        let next = db.query("SELECT nextval('never_evaluated')", &[]).unwrap();
        assert_eq!(
            next.iter()
                .flat_map(|row| row.values.iter().map(integer))
                .collect::<Vec<_>>(),
            vec![1]
        );
    }
}
