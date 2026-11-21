//! What a policy does: default deny, permissive and restrictive policies,
//! `USING` and `WITH CHECK`, who is exempt, `row_security = off`, and what a
//! policy expression may contain.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::tenant::{IsolationMode, RLSCommand, TenantContext};
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

/// The ids a session sees, through both statement families (which must agree).
fn ids(db: &EmbeddedDatabase, s: SessionId, sql: &str) -> Vec<i64> {
    let (text_rows, _) = db
        .query_with_columns_for_session(s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    let text: Vec<i64> = text_rows.iter().map(|r| int(&r.values[0])).collect();
    let params: Vec<i64> = db
        .query_params_for_session(s, sql, &[])
        .unwrap_or_else(|e| panic!("{sql} (params): {e}"))
        .iter()
        .map(|r| int(&r.values[0]))
        .collect();
    assert_eq!(text, params, "the two statement families disagree on `{sql}`");
    text
}

fn all_ids(db: &EmbeddedDatabase, table: &str) -> Vec<i64> {
    db.query(&format!("SELECT id FROM {table} ORDER BY id"), &[])
        .unwrap()
        .iter()
        .map(|r| int(&r.values[0]))
        .collect()
}

const VIOLATION: &str = "new row violates row-level security policy for table \"docs\"";
const SELECT: &str = "SELECT id FROM docs ORDER BY id";

/// docs: 1 alice/eng, 2 alice/ops, 3 bob/eng, 4 bob/ops, 5 carol/eng. Row
/// security is enabled; no policy yet.
fn db() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    for role in ["alice", "bob", "carol"] {
        db.execute(&format!("CREATE ROLE {role} LOGIN")).unwrap();
    }
    db.execute("CREATE TABLE docs (id INT PRIMARY KEY, owner TEXT, dept TEXT)").unwrap();
    db.execute(
        "INSERT INTO docs VALUES (1, 'alice', 'eng'), (2, 'alice', 'ops'), (3, 'bob', 'eng'), \
         (4, 'bob', 'ops'), (5, 'carol', 'eng')",
    )
    .unwrap();
    db.execute("ALTER TABLE docs ENABLE ROW LEVEL SECURITY").unwrap();
    db
}

#[test]
fn enabled_with_no_policy_denies_everything_to_a_subject_role() {
    let db = db();
    let alice = session(&db, "alice");
    assert_eq!(ids(&db, alice, SELECT), Vec::<i64>::new());
    assert_eq!(db.execute_for_session(alice, "UPDATE docs SET dept = 'x'").unwrap(), 0);
    assert_eq!(db.execute_for_session(alice, "DELETE FROM docs").unwrap(), 0);
    let e = db
        .execute_for_session(alice, "INSERT INTO docs VALUES (9, 'alice', 'eng')")
        .unwrap_err()
        .to_string();
    assert!(e.contains(VIOLATION), "{e}");
    let e = db
        .execute_params_for_session(alice, "INSERT INTO docs VALUES (9, 'alice', 'eng')", &[])
        .unwrap_err()
        .to_string();
    assert!(e.contains(VIOLATION), "{e}");
    // Nothing changed, and the embedded handle (the bootstrap superuser) sees all.
    assert_eq!(all_ids(&db, "docs"), vec![1, 2, 3, 4, 5]);
    // Not enabled: the same role sees everything.
    db.execute("ALTER TABLE docs DISABLE ROW LEVEL SECURITY").unwrap();
    assert_eq!(ids(&db, alice, SELECT), vec![1, 2, 3, 4, 5]);
}

#[test]
fn a_policy_on_a_table_that_is_not_enabled_does_nothing() {
    let db = db();
    db.execute("ALTER TABLE docs DISABLE ROW LEVEL SECURITY").unwrap();
    db.execute("CREATE POLICY own ON docs USING (owner = current_user)").unwrap();
    let alice = session(&db, "alice");
    assert_eq!(ids(&db, alice, SELECT), vec![1, 2, 3, 4, 5]);
    db.execute("ALTER TABLE docs ENABLE ROW LEVEL SECURITY").unwrap();
    assert_eq!(ids(&db, alice, SELECT), vec![1, 2]);
}

#[test]
fn permissive_policies_are_ored_and_restrictive_policies_are_anded() {
    let db = db();
    db.execute("CREATE POLICY own ON docs USING (owner = current_user)").unwrap();
    let alice = session(&db, "alice");
    let bob = session(&db, "bob");
    assert_eq!(ids(&db, alice, SELECT), vec![1, 2]);
    assert_eq!(ids(&db, bob, SELECT), vec![3, 4]);

    db.execute("CREATE POLICY eng ON docs FOR SELECT USING (dept = 'eng')").unwrap();
    assert_eq!(ids(&db, alice, SELECT), vec![1, 2, 3, 5]);
    assert_eq!(ids(&db, bob, SELECT), vec![1, 3, 4, 5]);

    db.execute("CREATE POLICY not_carol ON docs AS RESTRICTIVE USING (owner <> 'carol')")
        .unwrap();
    assert_eq!(ids(&db, alice, SELECT), vec![1, 2, 3]);

    db.execute("CREATE POLICY low ON docs AS RESTRICTIVE FOR SELECT USING (id < 3)")
        .unwrap();
    assert_eq!(ids(&db, alice, SELECT), vec![1, 2]);
    assert_eq!(ids(&db, bob, SELECT), vec![1]);
}

#[test]
fn restrictive_policies_alone_let_nothing_through() {
    let db = db();
    db.execute("CREATE POLICY r ON docs AS RESTRICTIVE USING (true)").unwrap();
    let alice = session(&db, "alice");
    assert_eq!(ids(&db, alice, SELECT), Vec::<i64>::new());
    db.execute("CREATE POLICY p ON docs USING (dept = 'ops')").unwrap();
    assert_eq!(ids(&db, alice, SELECT), vec![2, 4]);
}

#[test]
fn a_policy_applies_to_the_roles_it_names() {
    let db = db();
    db.execute("CREATE POLICY for_alice ON docs TO alice USING (true)").unwrap();
    db.execute("CREATE POLICY for_bob ON docs TO bob USING (owner = 'bob')").unwrap();
    let alice = session(&db, "alice");
    let bob = session(&db, "bob");
    let carol = session(&db, "carol");
    assert_eq!(ids(&db, alice, SELECT), vec![1, 2, 3, 4, 5]);
    assert_eq!(ids(&db, bob, SELECT), vec![3, 4]);
    // No policy names carol: default deny.
    assert_eq!(ids(&db, carol, SELECT), Vec::<i64>::new());
    db.execute("ALTER POLICY for_bob ON docs TO bob, carol").unwrap();
    assert_eq!(ids(&db, carol, SELECT), vec![3, 4]);
}

#[test]
fn a_policy_applies_to_the_command_it_is_for() {
    let db = db();
    db.execute("CREATE POLICY read_all ON docs FOR SELECT USING (true)").unwrap();
    db.execute("CREATE POLICY change_own ON docs FOR UPDATE USING (owner = current_user)")
        .unwrap();
    let alice = session(&db, "alice");
    assert_eq!(ids(&db, alice, SELECT), vec![1, 2, 3, 4, 5]);
    // UPDATE: only alice's rows are in reach.
    assert_eq!(db.execute_for_session(alice, "UPDATE docs SET dept = 'new'").unwrap(), 2);
    assert_eq!(
        db.execute_params_for_session(alice, "UPDATE docs SET dept = 'newer' WHERE id >= 1", &[])
            .unwrap(),
        2
    );
    // DELETE and INSERT have no policy: denied.
    assert_eq!(db.execute_for_session(alice, "DELETE FROM docs").unwrap(), 0);
    assert!(db
        .execute_for_session(alice, "INSERT INTO docs VALUES (9, 'alice', 'eng')")
        .unwrap_err()
        .to_string()
        .contains(VIOLATION));
    let changed = db
        .query("SELECT id FROM docs WHERE dept = 'newer' ORDER BY id", &[])
        .unwrap();
    assert_eq!(changed.iter().map(|r| int(&r.values[0])).collect::<Vec<_>>(), vec![1, 2]);
}

#[test]
fn update_and_delete_see_only_the_rows_select_sees() {
    let db = db();
    // Every row may be updated or deleted by policy, but only eng rows are readable.
    db.execute("CREATE POLICY read_eng ON docs FOR SELECT USING (dept = 'eng')").unwrap();
    db.execute("CREATE POLICY upd ON docs FOR UPDATE USING (true)").unwrap();
    db.execute("CREATE POLICY del ON docs FOR DELETE USING (true)").unwrap();
    let alice = session(&db, "alice");
    assert_eq!(db.execute_for_session(alice, "UPDATE docs SET owner = 'x' WHERE id > 0").unwrap(), 3);
    assert_eq!(
        db.execute_params_for_session(alice, "DELETE FROM docs WHERE id > 0", &[])
            .unwrap(),
        3
    );
    assert_eq!(all_ids(&db, "docs"), vec![2, 4]);
}

#[test]
fn with_check_validates_written_rows_and_falls_back_to_using() {
    let db = db();
    db.execute("CREATE POLICY own ON docs USING (owner = current_user)").unwrap();
    let alice = session(&db, "alice");
    // No WITH CHECK: USING is the check.
    db.execute_for_session(alice, "INSERT INTO docs VALUES (10, 'alice', 'eng')")
        .unwrap();
    for sql in [
        "INSERT INTO docs VALUES (11, 'bob', 'eng')",
        "INSERT INTO docs VALUES (12, 'alice', 'eng'), (13, 'bob', 'eng')",
        "UPDATE docs SET owner = 'bob' WHERE id = 1",
    ] {
        let e = db.execute_for_session(alice, sql).expect_err(sql).to_string();
        assert!(e.contains(VIOLATION), "{sql}: {e}");
        let e = db
            .execute_params_for_session(alice, sql, &[])
            .expect_err(sql)
            .to_string();
        assert!(e.contains(VIOLATION), "{sql} (params): {e}");
    }
    assert_eq!(all_ids(&db, "docs"), vec![1, 2, 3, 4, 5, 10]);

    // An explicit WITH CHECK replaces the fallback.
    db.execute("ALTER POLICY own ON docs WITH CHECK (dept = 'eng')").unwrap();
    db.execute_for_session(alice, "UPDATE docs SET owner = 'bob' WHERE id = 10")
        .unwrap();
    let e = db
        .execute_for_session(alice, "UPDATE docs SET dept = 'ops' WHERE id = 1")
        .unwrap_err()
        .to_string();
    assert!(e.contains(VIOLATION), "{e}");
    // A parameter in the written row is checked too.
    let e = db
        .execute_params_for_session(
            alice,
            "INSERT INTO docs VALUES ($1, $2, $3)",
            &[Value::Int4(20), Value::String("alice".into()), Value::String("ops".into())],
        )
        .unwrap_err()
        .to_string();
    assert!(e.contains(VIOLATION), "{e}");
    db.execute_params_for_session(
        alice,
        "INSERT INTO docs VALUES ($1, $2, $3)",
        &[Value::Int4(21), Value::String("zed".into()), Value::String("eng".into())],
    )
    .unwrap();
}

#[test]
fn a_restrictive_check_must_hold_as_well() {
    let db = db();
    db.execute("CREATE POLICY anyone ON docs USING (true)").unwrap();
    db.execute("CREATE POLICY eng_only ON docs AS RESTRICTIVE FOR INSERT WITH CHECK (dept = 'eng')")
        .unwrap();
    let alice = session(&db, "alice");
    db.execute_for_session(alice, "INSERT INTO docs VALUES (30, 'q', 'eng')").unwrap();
    let e = db
        .execute_for_session(alice, "INSERT INTO docs VALUES (31, 'q', 'ops')")
        .unwrap_err()
        .to_string();
    assert!(e.contains(VIOLATION), "{e}");
}

#[test]
fn superusers_and_bypassrls_roles_are_exempt() {
    let db = db();
    db.execute("CREATE ROLE root_like SUPERUSER LOGIN").unwrap();
    db.execute("CREATE ROLE auditor BYPASSRLS LOGIN").unwrap();
    db.execute("CREATE POLICY own ON docs USING (owner = current_user)").unwrap();
    db.execute("ALTER TABLE docs FORCE ROW LEVEL SECURITY").unwrap();
    for name in ["root_like", "auditor", "postgres", "helios"] {
        let s = session(&db, name);
        assert_eq!(ids(&db, s, SELECT), vec![1, 2, 3, 4, 5], "{name}");
        db.execute_for_session(s, "UPDATE docs SET dept = dept WHERE id = 5").unwrap();
    }
    // The attribute is read each statement: taking it away subjects the role.
    db.execute("ALTER ROLE auditor NOBYPASSRLS").unwrap();
    let auditor = session(&db, "auditor");
    assert_eq!(ids(&db, auditor, SELECT), Vec::<i64>::new());
}

#[test]
fn the_owner_is_exempt_unless_the_table_is_forced() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE ROLE alice LOGIN").unwrap();
    db.execute("CREATE ROLE bob LOGIN").unwrap();
    let alice = session(&db, "alice");
    let bob = session(&db, "bob");
    db.execute_for_session(alice, "CREATE TABLE docs (id INT PRIMARY KEY, owner TEXT, dept TEXT)")
        .unwrap();
    db.execute_for_session(
        alice,
        "INSERT INTO docs VALUES (1, 'alice', 'eng'), (3, 'bob', 'eng'), (5, 'carol', 'eng')",
    )
    .unwrap();
    db.execute_for_session(alice, "ALTER TABLE docs ENABLE ROW LEVEL SECURITY")
        .unwrap();
    db.execute_for_session(alice, "CREATE POLICY own ON docs USING (owner = current_user)")
        .unwrap();
    // The owner: everything. Another role: its rows.
    assert_eq!(ids(&db, alice, SELECT), vec![1, 3, 5]);
    assert_eq!(ids(&db, bob, SELECT), vec![3]);
    db.execute_for_session(alice, "INSERT INTO docs VALUES (7, 'dave', 'eng')")
        .unwrap();

    db.execute_for_session(alice, "ALTER TABLE docs FORCE ROW LEVEL SECURITY")
        .unwrap();
    assert_eq!(ids(&db, alice, SELECT), vec![1]);
    let e = db
        .execute_for_session(alice, "INSERT INTO docs VALUES (8, 'dave', 'eng')")
        .unwrap_err()
        .to_string();
    assert!(e.contains(VIOLATION), "{e}");

    db.execute_for_session(alice, "ALTER TABLE docs NO FORCE ROW LEVEL SECURITY")
        .unwrap();
    assert_eq!(ids(&db, alice, SELECT), vec![1, 3, 5, 7]);
}

#[test]
fn row_security_off_fails_a_filtered_statement_instead_of_filtering() {
    let db = db();
    db.execute("CREATE ROLE auditor BYPASSRLS LOGIN").unwrap();
    db.execute("CREATE POLICY own ON docs USING (owner = current_user)").unwrap();
    let alice = session(&db, "alice");
    db.execute_for_session(alice, "SET row_security = off").unwrap();
    let refusal = "query would be affected by row-level security policy for table \"docs\"";
    let e = db.query_with_columns_for_session(alice, SELECT).unwrap_err().to_string();
    assert!(e.contains(refusal), "{e}");
    let e = db.query_params_for_session(alice, SELECT, &[]).unwrap_err().to_string();
    assert!(e.contains(refusal), "{e}");
    let e = db
        .execute_for_session(alice, "DELETE FROM docs WHERE id = 1")
        .unwrap_err()
        .to_string();
    assert!(e.contains(refusal), "{e}");
    db.execute_for_session(alice, "SET row_security = on").unwrap();
    assert_eq!(ids(&db, alice, SELECT), vec![1, 2]);
    // An exempt role is not affected, so the setting changes nothing for it.
    let auditor = session(&db, "auditor");
    db.execute_for_session(auditor, "SET row_security = off").unwrap();
    assert_eq!(ids(&db, auditor, SELECT), vec![1, 2, 3, 4, 5]);
}

#[test]
fn a_policy_may_use_a_session_setting() {
    let db = db();
    db.execute("CREATE POLICY by_setting ON docs USING (dept = current_setting('application_name'))")
        .unwrap();
    let alice = session(&db, "alice");
    // Not set: no row has that department.
    assert_eq!(ids(&db, alice, SELECT), Vec::<i64>::new());
    db.execute_for_session(alice, "SET application_name = 'ops'").unwrap();
    assert_eq!(ids(&db, alice, SELECT), vec![2, 4]);
    let bob = session(&db, "bob");
    db.execute_for_session(bob, "SET application_name = 'eng'").unwrap();
    assert_eq!(ids(&db, bob, SELECT), vec![1, 3, 5]);
    assert_eq!(ids(&db, alice, SELECT), vec![2, 4]);
    let e = db
        .execute_for_session(alice, "INSERT INTO docs VALUES (40, 'alice', 'eng')")
        .unwrap_err()
        .to_string();
    assert!(e.contains(VIOLATION), "{e}");
    db.execute_for_session(alice, "INSERT INTO docs VALUES (41, 'alice', 'ops')")
        .unwrap();
}

#[test]
fn a_setting_that_is_not_set_reads_as_empty_with_missing_ok() {
    let db = db();
    // `current_setting(name, true)` of a name the session does not hold is the
    // empty string, never an error and never a match for a real value.
    db.execute("CREATE POLICY by_custom ON docs USING (dept = current_setting('app.dept', true))")
        .unwrap();
    let alice = session(&db, "alice");
    assert_eq!(ids(&db, alice, SELECT), Vec::<i64>::new());
    let e = db
        .execute_for_session(alice, "INSERT INTO docs VALUES (42, 'alice', 'eng')")
        .unwrap_err()
        .to_string();
    assert!(e.contains(VIOLATION), "{e}");
}

#[test]
fn a_policy_may_hold_a_subquery() {
    let db = db();
    db.execute("CREATE TABLE members (member TEXT, dept TEXT)").unwrap();
    db.execute("INSERT INTO members VALUES ('alice', 'ops'), ('bob', 'eng'), ('bob', 'ops')")
        .unwrap();
    db.execute(
        "CREATE POLICY by_membership ON docs \
         USING (dept IN (SELECT dept FROM members WHERE member = current_user))",
    )
    .unwrap();
    let alice = session(&db, "alice");
    let bob = session(&db, "bob");
    assert_eq!(ids(&db, alice, SELECT), vec![2, 4]);
    assert_eq!(ids(&db, bob, SELECT), vec![1, 2, 3, 4, 5]);
    assert_eq!(ids(&db, alice, "SELECT id FROM docs WHERE id = 1"), Vec::<i64>::new());
    // The same policy guards writes.
    assert_eq!(db.execute_for_session(alice, "UPDATE docs SET owner = owner").unwrap(), 2);
    let e = db
        .execute_for_session(alice, "INSERT INTO docs VALUES (50, 'alice', 'eng')")
        .unwrap_err()
        .to_string();
    assert!(e.contains(VIOLATION), "{e}");
    db.execute_params_for_session(alice, "INSERT INTO docs VALUES (51, 'alice', 'ops')", &[])
        .unwrap();
    assert_eq!(db.execute_for_session(alice, "DELETE FROM docs WHERE id > 50").unwrap(), 1);
}

#[test]
fn the_table_a_policy_subquery_reads_is_filtered_for_the_reader_too() {
    let db = db();
    db.execute("CREATE TABLE members (member TEXT, dept TEXT)").unwrap();
    db.execute("INSERT INTO members VALUES ('alice', 'ops'), ('bob', 'eng')").unwrap();
    db.execute("ALTER TABLE members ENABLE ROW LEVEL SECURITY").unwrap();
    db.execute("CREATE POLICY own_rows ON members USING (member = current_user)").unwrap();
    // Without a filter on `member`: the subquery returns only the reader's rows.
    db.execute("CREATE POLICY by_membership ON docs USING (dept IN (SELECT dept FROM members))")
        .unwrap();
    let alice = session(&db, "alice");
    let bob = session(&db, "bob");
    assert_eq!(ids(&db, alice, SELECT), vec![2, 4]);
    assert_eq!(ids(&db, bob, SELECT), vec![1, 3, 5]);
}

#[test]
fn a_policy_that_reads_its_own_table_is_an_error_not_a_loop() {
    let db = db();
    db.execute("CREATE POLICY loops ON docs USING (id IN (SELECT id FROM docs))").unwrap();
    let alice = session(&db, "alice");
    let e = db.query_with_columns_for_session(alice, SELECT).unwrap_err().to_string();
    assert!(
        e.contains("infinite recursion detected in policy for relation \"docs\""),
        "{e}"
    );
    let e = db.query_params_for_session(alice, SELECT, &[]).unwrap_err().to_string();
    assert!(e.contains("infinite recursion detected"), "{e}");
}

#[test]
fn a_policy_that_can_no_longer_be_applied_fails_the_statement() {
    let db = db();
    db.execute("CREATE POLICY by_dept ON docs USING (dept = 'eng')").unwrap();
    db.execute("ALTER TABLE docs DROP COLUMN dept").unwrap();
    let alice = session(&db, "alice");
    assert!(db.query_with_columns_for_session(alice, SELECT).is_err());
    assert!(db.query_params_for_session(alice, SELECT, &[]).is_err());
    assert!(db.execute_for_session(alice, "DELETE FROM docs").is_err());
    assert_eq!(all_ids(&db, "docs"), vec![1, 2, 3, 4, 5]);
}

#[test]
fn a_tenant_policy_and_a_table_switch_work_side_by_side() {
    let db = db();
    db.execute("CREATE TABLE items (id INT PRIMARY KEY, tenant_id TEXT)").unwrap();
    let tenant = db
        .tenant_manager
        .register_tenant("acme".to_string(), IsolationMode::SharedSchema);
    db.execute(&format!(
        "INSERT INTO items VALUES (1, '{}'), (2, 'someone-else')",
        tenant.id
    ))
    .unwrap();
    db.tenant_manager.create_rls_policy(
        "items".to_string(),
        "tenant_rows".to_string(),
        String::new(),
        RLSCommand::All,
        "tenant_id = current_tenant()".to_string(),
        None,
    );
    // The policy is a stored policy like any other.
    let listed = db
        .query("SELECT policyname, cmd FROM pg_policies WHERE tablename = 'items'", &[])
        .unwrap();
    assert_eq!(listed.len(), 1);
    // The table is not enabled: a role without a tenant is not filtered …
    let alice = session(&db, "alice");
    assert_eq!(ids(&db, alice, "SELECT id FROM items ORDER BY id"), vec![1, 2]);
    // … a tenant context is.
    db.tenant_manager.set_current_context(TenantContext {
        tenant_id: tenant.id,
        user_id: "u".to_string(),
        roles: vec![],
        isolation_mode: IsolationMode::SharedSchema,
    });
    let seen: Vec<i64> = db
        .query("SELECT id FROM items ORDER BY id", &[])
        .unwrap()
        .iter()
        .map(|r| int(&r.values[0]))
        .collect();
    assert_eq!(seen, vec![1]);
    db.tenant_manager.clear_current_context();
    // Deleting it through the tenant API removes the stored policy.
    assert!(db.tenant_manager.delete_rls_policy("items", "tenant_rows").unwrap());
    assert!(!db.tenant_manager.delete_rls_policy("items", "tenant_rows").unwrap());
    let left = db.query("SELECT 1 FROM pg_policies WHERE tablename = 'items'", &[]).unwrap();
    assert_eq!(left.len(), 0);
}
