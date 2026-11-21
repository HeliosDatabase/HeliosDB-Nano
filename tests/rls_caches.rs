//! No cache serves one identity another identity's rows: the same statement
//! text, run by different roles, in every order.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::{EmbeddedDatabase, Value};

fn int(v: &Value) -> i64 {
    match v {
        Value::Int2(v) => i64::from(*v),
        Value::Int4(v) => i64::from(*v),
        Value::Int8(v) => *v,
        other => panic!("not an integer: {other:?}"),
    }
}

fn session(db: &EmbeddedDatabase, user: &str) -> SessionId {
    db.create_session(user, IsolationLevel::ReadCommitted).unwrap()
}

fn text_ids(db: &EmbeddedDatabase, s: SessionId, sql: &str) -> Vec<i64> {
    let (rows, _) = db.query_with_columns_for_session(s, sql).unwrap();
    rows.iter().map(|r| int(&r.values[0])).collect()
}

fn param_ids(db: &EmbeddedDatabase, s: SessionId, sql: &str, params: &[Value]) -> Vec<i64> {
    db.query_params_for_session(s, sql, params)
        .unwrap()
        .iter()
        .map(|r| int(&r.values[0]))
        .collect()
}

fn embedded_ids(db: &EmbeddedDatabase, sql: &str) -> Vec<i64> {
    db.query(sql, &[]).unwrap().iter().map(|r| int(&r.values[0])).collect()
}

fn db() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE ROLE alice LOGIN").unwrap();
    db.execute("CREATE ROLE bob LOGIN").unwrap();
    db.execute("CREATE ROLE auditor BYPASSRLS LOGIN").unwrap();
    db.execute("CREATE TABLE docs (id INT PRIMARY KEY, owner TEXT)").unwrap();
    db.execute("INSERT INTO docs VALUES (1, 'alice'), (2, 'alice'), (3, 'bob')").unwrap();
    db.execute("ALTER TABLE docs ENABLE ROW LEVEL SECURITY").unwrap();
    db.execute("CREATE POLICY own ON docs USING (owner = current_user)").unwrap();
    db
}

const STATEMENTS: [&str; 5] = [
    "SELECT id FROM docs ORDER BY id",
    "SELECT id FROM docs",
    "SELECT count(*) FROM docs",
    "SELECT id FROM docs WHERE id = 3",
    "SELECT id FROM docs WHERE id IN (1, 3)",
];

fn expected(role: &str, statement: &str) -> Vec<i64> {
    let visible: &[i64] = match role {
        "alice" => &[1, 2],
        "bob" => &[3],
        _ => &[1, 2, 3],
    };
    match statement {
        "SELECT count(*) FROM docs" => vec![visible.len() as i64],
        "SELECT id FROM docs WHERE id = 3" => visible.iter().copied().filter(|i| *i == 3).collect(),
        "SELECT id FROM docs WHERE id IN (1, 3)" => visible.iter().copied().filter(|i| *i == 1 || *i == 3).collect(),
        _ => visible.to_vec(),
    }
}

#[test]
fn the_same_text_gives_each_role_its_own_rows_in_every_order() {
    let db = db();
    let alice = session(&db, "alice");
    let bob = session(&db, "bob");
    let auditor = session(&db, "auditor");
    let orders: [[(&str, SessionId); 3]; 3] = [
        [("auditor", auditor), ("alice", alice), ("bob", bob)],
        [("bob", bob), ("auditor", auditor), ("alice", alice)],
        [("alice", alice), ("bob", bob), ("auditor", auditor)],
    ];
    // Three rounds, so every statement is served warm (plan cache, result
    // cache, the direct paths) to every role after every other role.
    for round in 0..3 {
        for order in &orders {
            for statement in STATEMENTS {
                for (role, s) in order {
                    let mut text = text_ids(&db, *s, statement);
                    text.sort_unstable();
                    assert_eq!(text, expected(role, statement), "round {round}, {role}: {statement}");
                    let mut params = param_ids(&db, *s, statement, &[]);
                    params.sort_unstable();
                    assert_eq!(params, expected(role, statement), "round {round}, {role} (params): {statement}");
                }
                // The embedded handle (no login) in between: the bootstrap superuser.
                let mut embedded = embedded_ids(&db, statement);
                embedded.sort_unstable();
                assert_eq!(embedded, expected("embedded", statement), "round {round}: {statement}");
            }
        }
    }
}

#[test]
fn a_bound_parameter_statement_is_not_shared_between_roles() {
    let db = db();
    let alice = session(&db, "alice");
    let bob = session(&db, "bob");
    let sql = "SELECT id FROM docs WHERE id = $1";
    for _ in 0..3 {
        assert_eq!(param_ids(&db, alice, sql, &[Value::Int4(1)]), vec![1]);
        assert_eq!(param_ids(&db, bob, sql, &[Value::Int4(1)]), Vec::<i64>::new());
        assert_eq!(param_ids(&db, bob, sql, &[Value::Int4(3)]), vec![3]);
        assert_eq!(param_ids(&db, alice, sql, &[Value::Int4(3)]), Vec::<i64>::new());
    }
}

#[test]
fn a_policy_change_takes_effect_on_the_next_statement() {
    let db = db();
    let alice = session(&db, "alice");
    let sql = "SELECT id FROM docs ORDER BY id";
    assert_eq!(text_ids(&db, alice, sql), vec![1, 2]);
    assert_eq!(param_ids(&db, alice, sql, &[]), vec![1, 2]);
    db.execute("CREATE POLICY also_three ON docs FOR SELECT USING (id = 3)").unwrap();
    assert_eq!(text_ids(&db, alice, sql), vec![1, 2, 3]);
    assert_eq!(param_ids(&db, alice, sql, &[]), vec![1, 2, 3]);
    db.execute("ALTER POLICY also_three ON docs USING (false)").unwrap();
    assert_eq!(text_ids(&db, alice, sql), vec![1, 2]);
    db.execute("DROP POLICY own ON docs").unwrap();
    assert_eq!(text_ids(&db, alice, sql), Vec::<i64>::new());
    assert_eq!(param_ids(&db, alice, sql, &[]), Vec::<i64>::new());
    db.execute("ALTER TABLE docs DISABLE ROW LEVEL SECURITY").unwrap();
    assert_eq!(text_ids(&db, alice, sql), vec![1, 2, 3]);
    assert_eq!(param_ids(&db, alice, sql, &[]), vec![1, 2, 3]);
    db.execute("ALTER TABLE docs ENABLE ROW LEVEL SECURITY").unwrap();
    assert_eq!(text_ids(&db, alice, sql), Vec::<i64>::new());
}

#[test]
fn a_setting_a_policy_reads_is_part_of_the_identity() {
    let db = db();
    db.execute("DROP POLICY own ON docs").unwrap();
    db.execute("CREATE POLICY by_setting ON docs USING (owner = current_setting('application_name'))")
        .unwrap();
    let alice = session(&db, "alice");
    let sql = "SELECT id FROM docs ORDER BY id";
    for _ in 0..2 {
        db.execute_for_session(alice, "SET application_name = 'bob'").unwrap();
        assert_eq!(text_ids(&db, alice, sql), vec![3]);
        assert_eq!(param_ids(&db, alice, sql, &[]), vec![3]);
        db.execute_for_session(alice, "SET application_name = 'alice'").unwrap();
        assert_eq!(text_ids(&db, alice, sql), vec![1, 2]);
        assert_eq!(param_ids(&db, alice, sql, &[]), vec![1, 2]);
    }
}
