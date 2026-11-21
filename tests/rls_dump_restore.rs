//! A dump carries row-level security: the policies and the table switches are
//! there after a restore, in the SQL format and in the binary format.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use heliosdb_nano::session::IsolationLevel;
use heliosdb_nano::{EmbeddedDatabase, Value};
use tempfile::TempDir;

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "NULL".to_string(),
        Value::Boolean(b) => b.to_string(),
        other => format!("{other:?}"),
    }
}

fn rows(db: &EmbeddedDatabase, sql: &str) -> Vec<Vec<String>> {
    db.query(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .iter()
        .map(|r| r.values.iter().map(text).collect())
        .collect()
}

fn visible(db: &EmbeddedDatabase, user: &str, table: &str) -> usize {
    let s = db.create_session(user, IsolationLevel::ReadCommitted).unwrap();
    db.query_params_for_session(s, &format!("SELECT id FROM {table}"), &[])
        .unwrap()
        .len()
}

fn roles(db: &EmbeddedDatabase) {
    db.execute("CREATE ROLE alice LOGIN").unwrap();
    db.execute("CREATE ROLE bob LOGIN").unwrap();
}

fn source() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    roles(&db);
    db.execute("CREATE TABLE docs (id INT PRIMARY KEY, owner TEXT, dept TEXT)").unwrap();
    db.execute("INSERT INTO docs VALUES (1, 'alice', 'eng'), (2, 'bob', 'eng'), (3, 'bob', 'ops')")
        .unwrap();
    db.execute("ALTER TABLE docs ENABLE ROW LEVEL SECURITY").unwrap();
    db.execute("ALTER TABLE docs FORCE ROW LEVEL SECURITY").unwrap();
    db.execute("CREATE POLICY own ON docs USING (owner = current_user) WITH CHECK (owner = current_user AND dept <> 'x''y')")
        .unwrap();
    db.execute("CREATE POLICY no_ops ON docs AS RESTRICTIVE FOR SELECT TO bob USING (dept <> 'ops')")
        .unwrap();
    db.execute("CREATE TABLE plain (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO plain VALUES (1)").unwrap();
    // Enabled with no policy at all: default deny must survive too.
    db.execute("CREATE TABLE locked (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO locked VALUES (1)").unwrap();
    db.execute("ALTER TABLE locked ENABLE ROW LEVEL SECURITY").unwrap();
    db
}

const POLICIES: &str =
    "SELECT tablename, policyname, permissive, roles, cmd, qual, with_check FROM pg_policies ORDER BY tablename, policyname";
const SWITCHES: &str = "SELECT relname, relrowsecurity, relforcerowsecurity FROM pg_class \
     WHERE relname IN ('docs', 'plain', 'locked') AND relkind = 'r' ORDER BY relname";

fn check_restored(src: &EmbeddedDatabase, db: &EmbeddedDatabase) {
    assert_eq!(rows(db, POLICIES), rows(src, POLICIES));
    assert_eq!(rows(db, POLICIES).len(), 2);
    assert_eq!(
        rows(db, SWITCHES),
        vec![
            vec!["docs", "true", "true"],
            vec!["locked", "true", "false"],
            vec!["plain", "false", "false"]
        ]
    );
    // All rows were loaded, and the policies apply to them.
    assert_eq!(rows(db, "SELECT count(*) FROM docs").len(), 1);
    assert_eq!(db.query("SELECT id FROM docs", &[]).unwrap().len(), 3);
    assert_eq!(visible(db, "alice", "docs"), 1);
    assert_eq!(visible(db, "bob", "docs"), 1);
    assert_eq!(visible(db, "alice", "locked"), 0);
    assert_eq!(visible(db, "alice", "plain"), 1);
}

#[test]
fn a_sql_dump_carries_policies_and_switches() {
    let src = source();
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("dump.sql");
    src.dump_sql(&path).unwrap();
    let script = std::fs::read_to_string(&path).unwrap();
    assert!(script.contains("CREATE POLICY \"own\" ON \"docs\""), "{script}");
    assert!(script.contains("ALTER TABLE \"docs\" FORCE ROW LEVEL SECURITY;"), "{script}");
    // After the rows of the table they protect.
    assert!(script.find("INSERT INTO").unwrap() < script.find("CREATE POLICY").unwrap(), "{script}");
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    roles(&db);
    db.execute_sql_script(&script)
        .unwrap_or_else(|e| panic!("sql restore failed: {e}\n----\n{script}"));
    check_restored(&src, &db);
}

#[test]
fn a_binary_dump_carries_policies_and_switches() {
    let src = source();
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("dump.heliodump");
    src.dump_full(&path).unwrap();
    let mut db = EmbeddedDatabase::new_in_memory().unwrap();
    roles(&db);
    db.restore_from_dump(&path).unwrap();
    check_restored(&src, &db);
}

#[test]
fn a_restore_without_a_role_a_policy_names_fails_loudly() {
    let src = source();
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("dump.heliodump");
    src.dump_full(&path).unwrap();
    let mut db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE ROLE alice LOGIN").unwrap();
    // `bob` is missing: the restrictive policy that names it cannot be
    // recreated, and a table restored without it would show more rows.
    let e = db.restore_from_dump(&path).unwrap_err().to_string();
    assert!(e.contains("role \"bob\" does not exist"), "{e}");
    assert!(e.contains("row-level security of table \"docs\""), "{e}");
}
