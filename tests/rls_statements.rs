//! `CREATE POLICY`, `ALTER POLICY`, `DROP POLICY` and the `ALTER TABLE … ROW
//! LEVEL SECURITY` switches: what they accept, what they store, what they
//! refuse and who may run them.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::{EmbeddedDatabase, Value};

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

fn session(db: &EmbeddedDatabase, user: &str) -> SessionId {
    db.create_session(user, IsolationLevel::ReadCommitted).unwrap()
}

fn err(db: &EmbeddedDatabase, sql: &str) -> String {
    db.execute(sql).expect_err(sql).to_string()
}

fn db() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE ROLE alice LOGIN").unwrap();
    db.execute("CREATE ROLE bob LOGIN").unwrap();
    db.execute("CREATE TABLE docs (id INT PRIMARY KEY, owner TEXT, dept TEXT)").unwrap();
    db
}

const POLICIES: &str =
    "SELECT policyname, permissive, roles, cmd, qual, with_check FROM pg_policies WHERE tablename = 'docs' ORDER BY policyname";

#[test]
fn create_policy_with_every_clause_is_listed_in_pg_policies() {
    let db = db();
    db.execute(
        "CREATE POLICY p_full ON docs AS RESTRICTIVE FOR UPDATE TO alice, bob \
         USING (owner = current_user) WITH CHECK (dept = 'eng')",
    )
    .unwrap();
    db.execute("CREATE POLICY p_min ON docs").unwrap();
    db.execute("CREATE POLICY p_ins ON docs FOR INSERT TO PUBLIC WITH CHECK (owner = current_user)")
        .unwrap();
    assert_eq!(
        rows(&db, POLICIES),
        vec![
            vec![
                "p_full",
                "RESTRICTIVE",
                "{alice,bob}",
                "UPDATE",
                "owner = current_user",
                "dept = 'eng'"
            ],
            vec!["p_ins", "PERMISSIVE", "{public}", "INSERT", "NULL", "owner = current_user"],
            vec!["p_min", "PERMISSIVE", "{public}", "ALL", "NULL", "NULL"],
        ]
    );
    // pg_policy joins to pg_class on the table's oid.
    let joined = rows(
        &db,
        "SELECT p.polname, p.polcmd, p.polpermissive FROM pg_policy p \
         JOIN pg_class c ON c.oid = p.polrelid WHERE c.relname = 'docs' ORDER BY p.polname",
    );
    assert_eq!(
        joined,
        vec![
            vec!["p_full", "w", "false"],
            vec!["p_ins", "a", "true"],
            vec!["p_min", "*", "true"]
        ]
    );
    assert_eq!(rows(&db, "SELECT * FROM pg_policies WHERE schemaname <> 'public'").len(), 0);
}

#[test]
fn the_statements_work_on_the_parameter_family_too() {
    let db = db();
    db.execute_params("ALTER TABLE docs ENABLE ROW LEVEL SECURITY", &[]).unwrap();
    db.execute_params("ALTER TABLE docs FORCE ROW LEVEL SECURITY", &[]).unwrap();
    db.execute_params("CREATE POLICY p ON docs USING (owner = current_user)", &[])
        .unwrap();
    db.execute_params("ALTER POLICY p ON docs RENAME TO q", &[]).unwrap();
    assert_eq!(rows(&db, "SELECT policyname FROM pg_policies"), vec![vec!["q"]]);
    db.execute_params("DROP POLICY q ON docs", &[]).unwrap();
    assert_eq!(rows(&db, "SELECT policyname FROM pg_policies").len(), 0);
    assert_eq!(
        rows(
            &db,
            "SELECT relrowsecurity, relforcerowsecurity FROM pg_class WHERE relname = 'docs'"
        ),
        vec![vec!["true", "true"]]
    );
}

#[test]
fn alter_policy_changes_the_name_the_roles_and_the_expressions() {
    let db = db();
    db.execute("CREATE POLICY p ON docs FOR UPDATE USING (owner = 'a') WITH CHECK (owner = 'a')")
        .unwrap();
    db.execute("ALTER POLICY p ON docs RENAME TO renamed").unwrap();
    db.execute("ALTER POLICY renamed ON docs TO bob").unwrap();
    db.execute("ALTER POLICY renamed ON docs USING (dept = 'x')").unwrap();
    assert_eq!(
        rows(&db, POLICIES),
        vec![vec![
            "renamed",
            "PERMISSIVE",
            "{bob}",
            "UPDATE",
            "dept = 'x'",
            "owner = 'a'"
        ]]
    );
    db.execute("ALTER POLICY renamed ON docs TO PUBLIC WITH CHECK (dept = 'y')")
        .unwrap();
    assert_eq!(
        rows(&db, POLICIES),
        vec![vec![
            "renamed",
            "PERMISSIVE",
            "{public}",
            "UPDATE",
            "dept = 'x'",
            "dept = 'y'"
        ]]
    );
    // The policy keeps its catalog number through an ALTER.
    let before = rows(&db, "SELECT oid FROM pg_policy");
    db.execute("ALTER POLICY renamed ON docs USING (dept = 'z')").unwrap();
    assert_eq!(rows(&db, "SELECT oid FROM pg_policy"), before);
}

#[test]
fn the_table_switches_show_in_pg_class() {
    let db = db();
    let flags = |db: &EmbeddedDatabase| {
        rows(
            db,
            "SELECT relrowsecurity, relforcerowsecurity FROM pg_class WHERE relname = 'docs'",
        )
    };
    assert_eq!(flags(&db), vec![vec!["false", "false"]]);
    db.execute("ALTER TABLE docs ENABLE ROW LEVEL SECURITY").unwrap();
    assert_eq!(flags(&db), vec![vec!["true", "false"]]);
    db.execute("ALTER TABLE docs FORCE ROW LEVEL SECURITY").unwrap();
    assert_eq!(flags(&db), vec![vec!["true", "true"]]);
    db.execute("alter table docs no force row level security").unwrap();
    assert_eq!(flags(&db), vec![vec!["true", "false"]]);
    db.execute("ALTER TABLE docs DISABLE ROW LEVEL SECURITY").unwrap();
    assert_eq!(flags(&db), vec![vec!["false", "false"]]);
    // Two switches in one statement.
    db.execute("ALTER TABLE docs ENABLE ROW LEVEL SECURITY, FORCE ROW LEVEL SECURITY")
        .unwrap();
    assert_eq!(flags(&db), vec![vec!["true", "true"]]);
}

#[test]
fn what_is_refused_and_how() {
    let db = db();
    db.execute("CREATE POLICY p ON docs USING (true)").unwrap();
    assert_eq!(
        err(&db, "CREATE POLICY p ON docs USING (true)"),
        "Query execution error: policy \"p\" for table \"docs\" already exists"
    );
    assert_eq!(
        err(&db, "DROP POLICY nope ON docs"),
        "Query execution error: policy \"nope\" for table \"docs\" does not exist"
    );
    assert_eq!(
        err(&db, "ALTER POLICY nope ON docs USING (true)"),
        "Query execution error: policy \"nope\" for table \"docs\" does not exist"
    );
    assert_eq!(
        err(&db, "ALTER POLICY nope ON docs RENAME TO other"),
        "Query execution error: policy \"nope\" for table \"docs\" does not exist"
    );
    db.execute("CREATE POLICY q ON docs USING (true)").unwrap();
    assert_eq!(
        err(&db, "ALTER POLICY q ON docs RENAME TO p"),
        "Query execution error: policy \"p\" for table \"docs\" already exists"
    );
    assert!(err(&db, "CREATE POLICY x ON missing USING (true)").contains("relation \"missing\" does not exist"));
    assert!(err(&db, "ALTER TABLE missing ENABLE ROW LEVEL SECURITY").contains("does not exist"));
    assert!(err(&db, "CREATE POLICY x ON docs TO nobody USING (true)").contains("role \"nobody\" does not exist"));
    assert!(
        err(&db, "CREATE POLICY x ON docs FOR SELECT WITH CHECK (true)")
            .contains("WITH CHECK cannot be applied to SELECT or DELETE")
    );
    assert!(
        err(&db, "CREATE POLICY x ON docs FOR DELETE USING (true) WITH CHECK (true)")
            .contains("WITH CHECK cannot be applied to SELECT or DELETE")
    );
    assert!(
        err(&db, "CREATE POLICY x ON docs FOR INSERT USING (true)")
            .contains("only WITH CHECK expression allowed for INSERT")
    );
    // An expression over a column the table does not have is refused when the
    // policy is written, not when the table is next read.
    let unknown = err(&db, "CREATE POLICY x ON docs USING (no_such_column = 1)");
    assert!(unknown.contains("no_such_column"), "{unknown}");
    // A switch and another action in one statement.
    let mixed = err(&db, "ALTER TABLE docs ENABLE ROW LEVEL SECURITY, ADD COLUMN extra INT");
    assert!(mixed.contains("is not supported with row-level security"), "{mixed}");
    // None of the refused statements left anything behind.
    assert_eq!(rows(&db, "SELECT policyname FROM pg_policies ORDER BY 1"), vec![vec!["p"], vec!["q"]]);
}

#[test]
fn drop_policy_if_exists_is_quiet() {
    let db = db();
    db.execute("DROP POLICY IF EXISTS nope ON docs").unwrap();
    db.execute("DROP POLICY IF EXISTS nope ON no_such_table").unwrap();
    db.execute("CREATE POLICY p ON docs USING (true)").unwrap();
    db.execute("DROP POLICY IF EXISTS p ON docs").unwrap();
    assert_eq!(rows(&db, "SELECT policyname FROM pg_policies").len(), 0);
}

#[test]
fn a_materialized_view_takes_no_policy_of_its_own() {
    let db = db();
    db.execute("CREATE MATERIALIZED VIEW docs_mv AS SELECT id, owner FROM docs").unwrap();
    let e = err(&db, "CREATE POLICY p ON docs_mv USING (true)");
    assert!(e.contains("is a materialized view"), "{e}");
    let e = err(&db, "ALTER TABLE docs_mv ENABLE ROW LEVEL SECURITY");
    assert!(e.contains("is a materialized view"), "{e}");
}

#[test]
fn only_the_owner_or_a_superuser_may_run_them() {
    let db = db();
    db.execute("CREATE ROLE admin SUPERUSER LOGIN").unwrap();
    db.execute("CREATE ROLE bypass BYPASSRLS LOGIN").unwrap();
    let alice = session(&db, "alice");
    let bob = session(&db, "bob");
    let admin = session(&db, "admin");
    let bypass = session(&db, "bypass");

    // `docs` was made by the embedded handle: it belongs to the bootstrap
    // superuser. alice and bob own nothing of it.
    for sql in [
        "CREATE POLICY p ON docs USING (true)",
        "ALTER TABLE docs ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE docs FORCE ROW LEVEL SECURITY",
        "DROP POLICY IF EXISTS p ON docs",
    ] {
        for s in [alice, bob, bypass] {
            let e = db.execute_for_session(s, sql).expect_err(sql).to_string();
            assert!(
                e.contains("must be owner of table \"docs\" to change its row-level security"),
                "{sql}: {e}"
            );
        }
    }
    // A superuser role may.
    db.execute_for_session(admin, "CREATE POLICY p ON docs USING (true)").unwrap();
    for sql in ["ALTER POLICY p ON docs USING (false)", "ALTER POLICY p ON docs RENAME TO q", "DROP POLICY p ON docs"]
    {
        let e = db.execute_for_session(bob, sql).expect_err(sql).to_string();
        assert!(e.contains("must be owner of table"), "{sql}: {e}");
    }

    // alice's own table: alice may, bob may not.
    db.execute_for_session(alice, "CREATE TABLE notes (id INT PRIMARY KEY, owner TEXT)")
        .unwrap();
    db.execute_for_session(alice, "ALTER TABLE notes ENABLE ROW LEVEL SECURITY")
        .unwrap();
    db.execute_for_session(alice, "CREATE POLICY mine ON notes USING (owner = current_user)")
        .unwrap();
    db.execute_params_for_session(alice, "ALTER POLICY mine ON notes RENAME TO own", &[])
        .unwrap();
    let e = db
        .execute_for_session(bob, "DROP POLICY own ON notes")
        .unwrap_err()
        .to_string();
    assert!(e.contains("must be owner of table \"notes\""), "{e}");
    // Ownership follows the table through a rename.
    db.execute("ALTER TABLE notes RENAME TO memos").unwrap();
    db.execute_for_session(alice, "ALTER TABLE memos FORCE ROW LEVEL SECURITY")
        .unwrap();
    assert_eq!(
        rows(&db, "SELECT tablename, policyname FROM pg_policies WHERE policyname = 'own'"),
        vec![vec!["memos", "own"]]
    );
}

#[test]
fn to_current_user_names_the_role_that_ran_the_statement() {
    let db = db();
    db.execute("CREATE ROLE admin SUPERUSER LOGIN").unwrap();
    let admin = session(&db, "admin");
    db.execute_for_session(admin, "CREATE POLICY p ON docs TO CURRENT_USER USING (true)")
        .unwrap();
    assert_eq!(rows(&db, "SELECT roles FROM pg_policies"), vec![vec!["{admin}"]]);
}
