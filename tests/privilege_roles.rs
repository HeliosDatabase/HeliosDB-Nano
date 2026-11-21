//! Role membership and role switching: `GRANT role TO role`, `INHERIT` /
//! `NOINHERIT`, `WITH ADMIN OPTION`, `SET ROLE`, `RESET ROLE`,
//! `SET SESSION AUTHORIZATION`, `ALTER ROLE … RENAME TO`, and who may create,
//! alter and drop roles.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::many_single_char_names)]

use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::{EmbeddedDatabase, Error, Value};
use tempfile::TempDir;

fn login(db: &EmbeddedDatabase, role: &str) -> SessionId {
    db.create_session(role, IsolationLevel::ReadCommitted).unwrap()
}

fn run(db: &EmbeddedDatabase, session: SessionId, params: bool, sql: &str) -> Result<u64, Error> {
    if params {
        db.execute_params_for_session(session, sql, &[])
    } else {
        db.execute_for_session(session, sql)
    }
}

fn count(db: &EmbeddedDatabase, session: SessionId, params: bool, sql: &str) -> Result<usize, Error> {
    if params {
        db.query_params_for_session(session, sql, &[]).map(|rows| rows.len())
    } else {
        db.query_with_columns_for_session(session, sql).map(|(rows, _)| rows.len())
    }
}

fn scalar(db: &EmbeddedDatabase, session: SessionId, sql: &str) -> String {
    let (rows, _) = db
        .query_with_columns_for_session(session, sql)
        .unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    match &rows[0].values[0] {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn assert_error<T: std::fmt::Debug>(result: Result<T, Error>, expected: &str, context: &str) {
    match result {
        Ok(value) => panic!("{context}: must be refused with `{expected}`, but returned {value:?}"),
        Err(error) => {
            let text = error.to_string();
            assert!(text.contains(expected), "{context}: expected `{expected}`, got: {text}");
        }
    }
}

/// `readers` may read `docs`; `ann` and `ben` are logins with nothing granted.
fn setup_in(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE docs (id INT PRIMARY KEY, body TEXT)").unwrap();
    db.execute("INSERT INTO docs VALUES (1, 'one')").unwrap();
    db.execute("CREATE ROLE readers").unwrap();
    db.execute("CREATE ROLE ann LOGIN").unwrap();
    db.execute("CREATE ROLE ben LOGIN").unwrap();
    db.execute("GRANT SELECT ON docs TO readers").unwrap();
}

fn setup() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup_in(&db);
    db
}

// ---------------------------------------------------------------------------
// Membership
// ---------------------------------------------------------------------------

#[test]
fn a_member_holds_what_was_granted_to_the_role() {
    for params in [false, true] {
        let db = setup();
        let ann = login(&db, "ann");
        let ben = login(&db, "ben");
        assert_error(
            count(&db, ann, params, "SELECT * FROM docs"),
            "permission denied for table docs",
            "before the membership",
        );
        run(&db, login(&db, "postgres"), params, "GRANT readers TO ann").unwrap();
        // The open session sees it on its next statement.
        assert_eq!(count(&db, ann, params, "SELECT * FROM docs").unwrap(), 1);
        assert_error(
            count(&db, ben, params, "SELECT * FROM docs"),
            "permission denied for table docs",
            "a role that was not made a member",
        );
        db.execute("REVOKE readers FROM ann").unwrap();
        assert_error(
            count(&db, ann, params, "SELECT * FROM docs"),
            "permission denied for table docs",
            "after the membership was revoked",
        );
    }
}

#[test]
fn membership_is_transitive_and_survives_a_restart() {
    let dir = TempDir::new().unwrap();
    {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        setup_in(&db);
        db.execute("CREATE ROLE staff").unwrap();
        db.execute("GRANT readers TO staff").unwrap();
        db.execute("GRANT staff TO ann").unwrap();
        let ann = login(&db, "ann");
        assert_eq!(count(&db, ann, false, "SELECT * FROM docs").unwrap(), 1);
    }
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    let ann = login(&db, "ann");
    let ben = login(&db, "ben");
    for params in [false, true] {
        assert_eq!(count(&db, ann, params, "SELECT * FROM docs").unwrap(), 1, "ann, through staff");
        assert_error(
            count(&db, ben, params, "SELECT * FROM docs"),
            "permission denied for table docs",
            "ben is in no role",
        );
    }
    assert_eq!(
        db.query("SELECT pg_has_role('ann', 'readers', 'MEMBER')", &[]).unwrap()[0].values[0],
        Value::Boolean(true)
    );
}

#[test]
fn noinherit_holds_the_role_back_until_set_role() {
    let db = setup();
    db.execute("ALTER ROLE ann NOINHERIT").unwrap();
    db.execute("GRANT readers TO ann").unwrap();
    let ann = login(&db, "ann");
    for params in [false, true] {
        assert_error(
            count(&db, ann, params, "SELECT * FROM docs"),
            "permission denied for table docs",
            "NOINHERIT: the role's privileges are not ann's",
        );
    }
    db.execute_for_session(ann, "SET ROLE readers").unwrap();
    for params in [false, true] {
        assert_eq!(count(&db, ann, params, "SELECT * FROM docs").unwrap(), 1, "as readers");
    }
    db.execute_for_session(ann, "RESET ROLE").unwrap();
    assert_error(
        count(&db, ann, false, "SELECT * FROM docs"),
        "permission denied for table docs",
        "back as ann",
    );
}

#[test]
fn a_membership_cycle_is_refused() {
    let db = setup();
    db.execute("CREATE ROLE staff").unwrap();
    db.execute("GRANT readers TO staff").unwrap();
    assert_error(
        db.execute("GRANT staff TO readers"),
        "is a member of role",
        "a grant that closes a cycle",
    );
    assert_error(db.execute("GRANT staff TO staff"), "is a member of role", "a role granted to itself");
    assert_error(
        db.execute("GRANT nobody_here TO ann"),
        "role \"nobody_here\" does not exist",
        "an unknown role",
    );
    assert_error(
        db.execute("GRANT readers TO nobody_here"),
        "role \"nobody_here\" does not exist",
        "an unknown member",
    );
}

#[test]
fn granting_a_role_needs_the_admin_option_or_createrole() {
    for params in [false, true] {
        let db = setup();
        db.execute("CREATE ROLE hr LOGIN CREATEROLE").unwrap();
        let ann = login(&db, "ann");
        let ben = login(&db, "ben");
        let hr = login(&db, "hr");
        db.execute("GRANT readers TO ann").unwrap();
        assert_error(
            run(&db, ann, params, "GRANT readers TO ben"),
            "permission denied to grant role \"readers\"",
            "a plain member handing the role on",
        );
        assert_error(
            run(&db, ben, params, "GRANT readers TO ben"),
            "permission denied to grant role \"readers\"",
            "a role adding itself",
        );
        assert_error(
            count(&db, ben, params, "SELECT * FROM docs"),
            "permission denied for table docs",
            "the refused grants granted nothing",
        );
        db.execute("GRANT readers TO ann WITH ADMIN OPTION").unwrap();
        run(&db, ann, params, "GRANT readers TO ben").unwrap();
        assert_eq!(count(&db, ben, params, "SELECT * FROM docs").unwrap(), 1);
        run(&db, ann, params, "REVOKE readers FROM ben").unwrap();
        // CREATEROLE administers membership of ordinary roles.
        run(&db, hr, params, "GRANT readers TO ben").unwrap();
        run(&db, hr, params, "REVOKE readers FROM ben").unwrap();
        // REVOKE ADMIN OPTION FOR keeps the membership.
        db.execute("REVOKE ADMIN OPTION FOR readers FROM ann").unwrap();
        assert_eq!(count(&db, ann, params, "SELECT * FROM docs").unwrap(), 1);
        assert_error(
            run(&db, ann, params, "GRANT readers TO ben"),
            "permission denied to grant role \"readers\"",
            "after the admin option was revoked",
        );
    }
}

// ---------------------------------------------------------------------------
// SET ROLE / SET SESSION AUTHORIZATION
// ---------------------------------------------------------------------------

#[test]
fn set_role_switches_to_a_role_the_session_may_become_and_only_those() {
    let db = setup();
    db.execute("CREATE ROLE writers").unwrap();
    db.execute("GRANT INSERT ON docs TO writers").unwrap();
    db.execute("GRANT readers TO ann").unwrap();
    let ann = login(&db, "ann");

    assert_eq!(scalar(&db, ann, "SELECT current_user"), "ann");
    db.execute_for_session(ann, "SET ROLE readers").unwrap();
    assert_eq!(scalar(&db, ann, "SELECT current_user"), "readers");
    assert_eq!(scalar(&db, ann, "SELECT session_user"), "ann", "session_user is the login");
    assert_eq!(scalar(&db, ann, "SELECT current_role"), "readers");
    assert_eq!(count(&db, ann, false, "SELECT * FROM docs").unwrap(), 1);

    assert_error(
        db.execute_for_session(ann, "SET ROLE writers"),
        "permission denied to set role \"writers\"",
        "a role ann is not a member of",
    );
    assert_error(
        db.execute_for_session(ann, "SET ROLE nobody_here"),
        "role \"nobody_here\" does not exist",
        "a role that does not exist",
    );
    // The refused switches changed nothing.
    assert_eq!(scalar(&db, ann, "SELECT current_user"), "readers");

    db.execute_for_session(ann, "SET ROLE NONE").unwrap();
    assert_eq!(scalar(&db, ann, "SELECT current_user"), "ann");
    db.execute_for_session(ann, "SET ROLE 'readers'").unwrap();
    assert_eq!(scalar(&db, ann, "SELECT current_user"), "readers");
    db.execute_params_for_session(ann, "RESET ROLE", &[]).unwrap();
    assert_eq!(scalar(&db, ann, "SELECT current_user"), "ann");
}

#[test]
fn a_superuser_that_sets_a_role_is_checked_as_that_role() {
    let db = setup();
    db.execute("CREATE TABLE vault (id INT PRIMARY KEY)").unwrap();
    let admin = login(&db, "postgres");
    for params in [false, true] {
        assert_eq!(count(&db, admin, params, "SELECT * FROM vault").unwrap(), 0);
        db.execute_for_session(admin, "SET ROLE ann").unwrap();
        // Repeated statements: no cache may still answer as the superuser.
        for _ in 0..4 {
            assert_error(
                count(&db, admin, params, "SELECT * FROM vault"),
                "permission denied for table vault",
                "a superuser acting as ann",
            );
        }
        assert_error(
            run(&db, admin, params, "CREATE ROLE sneaky SUPERUSER"),
            "permission denied to create role",
            "ann may not create roles",
        );
        db.execute_for_session(admin, "RESET ROLE").unwrap();
        assert_eq!(count(&db, admin, params, "SELECT * FROM vault").unwrap(), 0);
    }
}

#[test]
fn set_role_works_on_the_embedded_api_too() {
    let db = setup();
    db.execute("CREATE TABLE vault (id INT PRIMARY KEY)").unwrap();
    assert_eq!(db.query("SELECT * FROM vault", &[]).unwrap().len(), 0);
    db.execute("SET ROLE ann").unwrap();
    for _ in 0..3 {
        assert_error(
            db.query("SELECT * FROM vault", &[]),
            "permission denied for table vault",
            "the embedded handle acting as ann (text)",
        );
        assert_error(
            db.query_params("SELECT * FROM vault", &[]),
            "permission denied for table vault",
            "the embedded handle acting as ann (params)",
        );
        assert_error(
            db.execute("INSERT INTO vault VALUES (1)"),
            "permission denied for table vault",
            "the embedded handle acting as ann (write)",
        );
    }
    assert_eq!(
        db.query("SELECT current_user", &[]).unwrap()[0].values[0],
        Value::String("ann".into())
    );
    db.execute("RESET ROLE").unwrap();
    db.execute("INSERT INTO vault VALUES (1)").unwrap();
    assert_eq!(db.query("SELECT * FROM vault", &[]).unwrap().len(), 1);
}

#[test]
fn set_session_authorization_is_for_superusers() {
    let db = setup();
    let ann = login(&db, "ann");
    assert_error(
        db.execute_for_session(ann, "SET SESSION AUTHORIZATION ben"),
        "permission denied to set session authorization \"ben\"",
        "an ordinary role",
    );
    let admin = login(&db, "postgres");
    db.execute_for_session(admin, "SET SESSION AUTHORIZATION ben").unwrap();
    assert_eq!(scalar(&db, admin, "SELECT session_user"), "ben");
    assert_eq!(scalar(&db, admin, "SELECT current_user"), "ben");
    assert_error(
        count(&db, admin, false, "SELECT * FROM docs"),
        "permission denied for table docs",
        "acting as ben",
    );
    // Still decided by who logged in: the session may go back.
    db.execute_for_session(admin, "RESET SESSION AUTHORIZATION").unwrap();
    assert_eq!(scalar(&db, admin, "SELECT session_user"), "postgres");
    assert_eq!(count(&db, admin, false, "SELECT * FROM docs").unwrap(), 1);
}

#[test]
fn set_local_role_is_refused_not_acknowledged() {
    let db = setup();
    let admin = login(&db, "postgres");
    assert_error(
        db.execute_for_session(admin, "SET LOCAL ROLE ann"),
        "not supported",
        "SET LOCAL ROLE",
    );
    assert_eq!(scalar(&db, admin, "SELECT current_user"), "postgres");
}

// ---------------------------------------------------------------------------
// Role administration
// ---------------------------------------------------------------------------

#[test]
fn creating_altering_and_dropping_roles_needs_createrole() {
    for params in [false, true] {
        let db = setup();
        db.execute("CREATE ROLE hr LOGIN CREATEROLE").unwrap();
        db.execute("CREATE ROLE boss SUPERUSER").unwrap();
        let ann = login(&db, "ann");
        let hr = login(&db, "hr");
        assert_error(
            run(&db, ann, params, "CREATE ROLE temp1"),
            "permission denied to create role",
            "without CREATEROLE",
        );
        assert_error(
            run(&db, ann, params, "ALTER ROLE ben CREATEDB"),
            "permission denied to alter role",
            "without CREATEROLE",
        );
        assert_error(
            run(&db, ann, params, "DROP ROLE ben"),
            "permission denied to drop role",
            "without CREATEROLE",
        );
        // A role may set its own password.
        run(&db, ann, params, "ALTER ROLE ann PASSWORD 'a-new-secret'").unwrap();
        assert_error(
            run(&db, ann, params, "ALTER ROLE ben PASSWORD 'not-yours'"),
            "permission denied to alter role",
            "somebody else's password",
        );

        run(&db, hr, params, "CREATE ROLE temp1 LOGIN").unwrap();
        run(&db, hr, params, "ALTER ROLE temp1 CREATEDB").unwrap();
        // CREATEROLE does not make superusers, and does not touch them.
        assert_error(
            run(&db, hr, params, "CREATE ROLE temp2 SUPERUSER"),
            "permission denied to create role",
            "CREATEROLE creating a superuser",
        );
        assert_error(
            run(&db, hr, params, "ALTER ROLE temp1 SUPERUSER"),
            "permission denied to alter role",
            "CREATEROLE granting SUPERUSER",
        );
        assert_error(
            run(&db, hr, params, "ALTER ROLE boss NOLOGIN"),
            "permission denied to alter role",
            "CREATEROLE altering a superuser",
        );
        assert_error(
            run(&db, hr, params, "DROP ROLE boss"),
            "permission denied to drop role",
            "CREATEROLE dropping a superuser",
        );
        run(&db, hr, params, "DROP ROLE temp1").unwrap();
    }
}

#[test]
fn a_renamed_role_keeps_its_grants_and_memberships() {
    let db = setup();
    db.execute("GRANT readers TO ann").unwrap();
    db.execute("GRANT INSERT ON docs TO ann").unwrap();
    assert_error(
        db.execute_for_session(login(&db, "ben"), "ALTER ROLE ann RENAME TO anna"),
        "permission denied to rename role",
        "without CREATEROLE",
    );
    db.execute("ALTER ROLE ann RENAME TO anna").unwrap();
    db.execute("ALTER ROLE readers RENAME TO viewers").unwrap();
    assert_error(
        db.execute("ALTER ROLE ben RENAME TO anna"),
        "role \"anna\" already exists",
        "a name that is taken",
    );
    assert_error(
        db.execute("ALTER ROLE postgres RENAME TO root"),
        "built-in role",
        "a built-in role",
    );
    let anna = login(&db, "anna");
    for params in [false, true] {
        assert_eq!(count(&db, anna, params, "SELECT * FROM docs").unwrap(), 1, "through viewers");
    }
    db.execute_for_session(anna, "INSERT INTO docs VALUES (2, 'two')").unwrap();
    let (rows, _) = db
        .query_with_columns("SELECT rolname FROM pg_roles WHERE rolname IN ('ann', 'anna', 'readers', 'viewers')")
        .unwrap();
    let mut names: Vec<String> = rows
        .iter()
        .map(|row| match &row.values[0] {
            Value::String(name) => name.clone(),
            other => other.to_string(),
        })
        .collect();
    names.sort();
    assert_eq!(names, vec!["anna".to_string(), "viewers".to_string()]);
}

#[test]
fn drop_role_refuses_while_the_role_owns_or_holds_something() {
    let db = setup();
    db.execute("GRANT CREATE ON SCHEMA public TO ann").unwrap();
    let ann = login(&db, "ann");
    db.execute_for_session(ann, "CREATE TABLE anns (id INT PRIMARY KEY)").unwrap();
    db.execute("REVOKE CREATE ON SCHEMA public FROM ann").unwrap();
    assert_error(db.execute("DROP ROLE ann"), "some objects depend on it", "an owner");
    db.execute("ALTER TABLE anns OWNER TO postgres").unwrap();
    // A membership alone does not block the drop: it goes with the role.
    db.execute("GRANT readers TO ann").unwrap();
    db.execute("DROP ROLE ann").unwrap();
    assert_error(db.execute("DROP ROLE readers"), "some objects depend on it", "a grantee");
    db.execute("REVOKE SELECT ON docs FROM readers").unwrap();
    db.execute("DROP ROLE readers").unwrap();
    // A role re-created under the name starts with nothing.
    db.execute("CREATE ROLE ann LOGIN").unwrap();
    let ann = login(&db, "ann");
    assert_error(
        count(&db, ann, false, "SELECT * FROM docs"),
        "permission denied for table docs",
        "the new ann",
    );
}
