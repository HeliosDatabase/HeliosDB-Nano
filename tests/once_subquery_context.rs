//! Once-evaluated nested reads retain the owning public session's row view.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};

#[derive(Clone, Copy, Debug)]
enum Family {
    Text,
    Params,
}
const FAMILIES: [Family; 2] = [Family::Text, Family::Params];
const QUERIES: [&str; 5] = [
    "SELECT id FROM outer_rows WHERE id IN (SELECT chosen FROM inner_rows)",
    "SELECT id FROM outer_rows WHERE id=(SELECT chosen FROM inner_rows WHERE id=10)",
    "SELECT id FROM outer_rows WHERE id=(SELECT chosen FROM inner_rows WHERE id=30)",
    "SELECT id FROM outer_rows WHERE EXISTS(SELECT 1 FROM inner_rows WHERE chosen=3)",
    "SELECT id FROM outer_rows WHERE NOT EXISTS(SELECT 1 FROM inner_rows WHERE chosen=3)",
];
fn original() -> Vec<Vec<i64>> {
    vec![vec![1, 2], vec![1], vec![], vec![], vec![1, 2, 3]]
}
fn changed() -> Vec<Vec<i64>> {
    vec![vec![2, 3], vec![], vec![3], vec![1, 2, 3], vec![]]
}
fn seed() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    for sql in [
        "CREATE TABLE outer_rows(id INT PRIMARY KEY)",
        "CREATE TABLE inner_rows(id INT PRIMARY KEY, chosen INT)",
        "INSERT INTO outer_rows VALUES(1),(2),(3)",
        "INSERT INTO inner_rows VALUES(10,1),(20,2)",
    ] {
        db.execute(sql).unwrap();
    }
    db
}
fn ids(rows: Vec<Tuple>) -> Vec<i64> {
    let mut result: Vec<_> = rows
        .into_iter()
        .map(|row| {
            assert_eq!(row.values.len(), 1);
            match row.values[0] {
                Value::Int2(value) => i64::from(value),
                Value::Int4(value) => i64::from(value),
                Value::Int8(value) => value,
                ref other => panic!("expected integer id: {other:?}"),
            }
        })
        .collect();
    result.sort_unstable();
    result
}
fn seen(db: &EmbeddedDatabase, family: Family, session: Option<SessionId>) -> Vec<Vec<i64>> {
    QUERIES
        .into_iter()
        .map(|sql| {
            let rows = match (family, session) {
                (Family::Text, Some(session)) => db.query_with_columns_for_session(session, sql).map(|result| result.0),
                (Family::Params, Some(session)) => db.query_params_for_session(session, sql, &[]),
                (Family::Text, None) => db.query_with_columns(sql).map(|result| result.0),
                (Family::Params, None) => db.query_params(sql, &[]),
            }
            .unwrap_or_else(|error| panic!("{family:?}: {sql}: {error}"));
            ids(rows)
        })
        .collect()
}
fn execute(db: &EmbeddedDatabase, family: Family, session: SessionId, sql: &str) {
    match family {
        Family::Text => db.execute_for_session(session, sql),
        Family::Params => db.execute_params_for_session(session, sql, &[]),
    }
    .unwrap_or_else(|error| panic!("{family:?}: {sql}: {error}"));
}

#[test]
fn once_in_scalar_and_exists_see_staged_rows_and_savepoint_restore() {
    for family in FAMILIES {
        let db = seed();
        let session = db.create_session("once_staged", IsolationLevel::ReadCommitted).unwrap();
        db.begin_transaction_for_session(session).unwrap();
        assert_eq!(seen(&db, family, Some(session)), original());
        execute(&db, family, session, "SAVEPOINT before_inner");
        execute(&db, family, session, "DELETE FROM inner_rows WHERE id=10");
        execute(&db, family, session, "INSERT INTO inner_rows VALUES(30,3)");
        assert_eq!(
            seen(&db, family, Some(session)),
            changed(),
            "{family:?}: owning transaction"
        );
        assert_eq!(
            seen(&db, family, None),
            original(),
            "{family:?}: uncommitted rows stay private"
        );
        execute(&db, family, session, "ROLLBACK TO SAVEPOINT before_inner");
        assert_eq!(
            seen(&db, family, Some(session)),
            original(),
            "{family:?}: restored staging"
        );
        db.commit_transaction_for_session(session).unwrap();
        db.destroy_session(session).unwrap();
        assert_eq!(
            seen(&db, family, None),
            original(),
            "{family:?}: committed restored view"
        );
    }
}

#[test]
fn once_in_scalar_and_exists_keep_repeatable_read_while_fresh_reads_advance() {
    for family in FAMILIES {
        let db = seed();
        let reader = db.create_session("once_rr", IsolationLevel::RepeatableRead).unwrap();
        db.begin_transaction_for_session(reader).unwrap();
        assert_eq!(seen(&db, family, Some(reader)), original());
        db.execute("DELETE FROM inner_rows WHERE id=10").unwrap();
        db.execute("INSERT INTO inner_rows VALUES(30,3)").unwrap();
        assert_eq!(
            seen(&db, family, Some(reader)),
            original(),
            "{family:?}: original nested snapshot"
        );
        assert_eq!(seen(&db, family, None), changed(), "{family:?}: fresh nested snapshot");
        db.commit_transaction_for_session(reader).unwrap();
        db.destroy_session(reader).unwrap();
    }
}
