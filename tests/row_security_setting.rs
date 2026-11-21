//! `row_security` as a per-session setting, and the one parameter registry
//! behind `SHOW`, `SHOW ALL`, `pg_settings` and `current_setting()`.
//!
//! Before: `SET row_security = off` (the first line of every pg_dump script)
//! was refused on the embedded / shell path ("SetVariable is not supported"),
//! acknowledged with no effect on the wire, and `SHOW row_security`,
//! `current_setting('row_security')`, `SHOW ALL` and the embedded `pg_settings`
//! did not exist.

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

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

fn scalar(db: &EmbeddedDatabase, sql: &str) -> String {
    let rows = db.query(sql, &[]).unwrap();
    assert_eq!(rows.len(), 1, "{sql}");
    text(&rows[0].values[0])
}

/// `orders` with alice's and bob's rows, a SharedSchema tenant in context, and
/// a SELECT + DELETE policy showing alice only.
fn seeded() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE orders (id INT PRIMARY KEY, owner TEXT NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO orders VALUES (1, 'alice')").unwrap();
    db.execute("INSERT INTO orders VALUES (2, 'bob')").unwrap();
    let tenant = db
        .tenant_manager
        .register_tenant("rs".to_string(), IsolationMode::SharedSchema);
    db.tenant_manager.set_current_context(TenantContext {
        tenant_id: tenant.id,
        user_id: "alice".to_string(),
        roles: vec![],
        isolation_mode: IsolationMode::SharedSchema,
    });
    for (name, cmd) in [("read_own", RLSCommand::Select), ("delete_own", RLSCommand::Delete)] {
        db.tenant_manager.create_rls_policy(
            "orders".to_string(),
            name.to_string(),
            String::new(),
            cmd,
            "owner = 'alice'".to_string(),
            None,
        );
    }
    db
}

fn session(db: &EmbeddedDatabase, user: &str) -> SessionId {
    db.create_session(user, IsolationLevel::ReadCommitted).unwrap()
}

const RLS_REFUSAL: &str = "query would be affected by row-level security policy for table \"orders\"";

#[test]
fn set_show_reset_row_security_on_the_embedded_path() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    assert_eq!(scalar(&db, "SHOW row_security"), "on");
    db.execute("SET row_security = off;").unwrap();
    assert_eq!(scalar(&db, "SHOW row_security"), "off");
    assert_eq!(scalar(&db, "SELECT current_setting('row_security')"), "off");
    let rows = db
        .query_params("SELECT current_setting($1)", &[Value::String("row_security".into())])
        .unwrap();
    assert_eq!(text(&rows[0].values[0]), "off");
    db.execute("RESET row_security").unwrap();
    assert_eq!(scalar(&db, "SHOW row_security"), "on");
    db.execute_params("SET row_security = off", &[]).unwrap();
    assert_eq!(scalar(&db, "SELECT current_setting('row_security')"), "off");
    let err = db.execute("SET row_security = maybe").unwrap_err().to_string();
    assert!(err.contains("requires a Boolean value"), "{err}");
}

#[test]
fn row_security_is_per_session() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let a = session(&db, "a");
    let b = session(&db, "b");
    db.execute_for_session(a, "SET row_security = off").unwrap();
    let (rows, _) = db.query_with_columns_for_session(a, "SHOW row_security").unwrap();
    assert_eq!(text(&rows[0].values[0]), "off");
    let (rows, _) = db.query_with_columns_for_session(b, "SHOW row_security").unwrap();
    assert_eq!(text(&rows[0].values[0]), "on");
}

#[test]
fn row_security_off_refuses_a_filtered_query_for_a_role_without_bypassrls() {
    let db = seeded();
    let app = session(&db, "app_user");
    // On: filtered, as always.
    let rows = db.query_params_for_session(app, "SELECT id FROM orders", &[]).unwrap();
    assert_eq!(rows.len(), 1);
    db.execute_for_session(app, "SET row_security = off").unwrap();
    // Off: an error, never filtered rows and never unfiltered ones — on both
    // statement families.
    let err = db
        .query_params_for_session(app, "SELECT id FROM orders", &[])
        .unwrap_err();
    assert!(err.to_string().contains(RLS_REFUSAL), "{err}");
    let err = db
        .query_with_columns_for_session(app, "SELECT id FROM orders")
        .unwrap_err();
    assert!(err.to_string().contains(RLS_REFUSAL), "{err}");
    let err = db.execute_for_session(app, "DELETE FROM orders").unwrap_err();
    assert!(err.to_string().contains(RLS_REFUSAL), "{err}");
    let err = db
        .execute_params_for_session(app, "DELETE FROM orders WHERE id = $1", &[Value::Int4(2)])
        .unwrap_err();
    assert!(err.to_string().contains(RLS_REFUSAL), "{err}");
    // Nothing was deleted.
    db.tenant_manager.clear_current_context();
    assert_eq!(db.query("SELECT id FROM orders", &[]).unwrap().len(), 2);
}

#[test]
fn row_security_off_unfiltered_for_bypassrls_and_superusers() {
    let db = seeded();
    db.execute("CREATE ROLE dumper BYPASSRLS LOGIN").unwrap();
    db.execute("CREATE ROLE plain LOGIN").unwrap();
    // 5.0: privileges are enforced, so the roles are granted what the test reads.
    db.execute("GRANT SELECT ON orders TO dumper, plain").unwrap();
    for (user, bypass) in [("dumper", true), ("postgres", true), ("plain", false)] {
        let sid = session(&db, user);
        db.execute_for_session(sid, "SET row_security = off").unwrap();
        let result = db.query_params_for_session(sid, "SELECT id FROM orders", &[]);
        if bypass {
            assert_eq!(result.unwrap().len(), 2, "{user} bypasses RLS");
        } else {
            assert!(result.unwrap_err().to_string().contains(RLS_REFUSAL), "{user}");
        }
    }
    // The embedded API (no login: the process owner, the bootstrap superuser).
    db.execute("SET row_security = off").unwrap();
    assert_eq!(db.query("SELECT id FROM orders", &[]).unwrap().len(), 2);
    db.execute("SET row_security = on").unwrap();
    assert_eq!(db.query("SELECT id FROM orders", &[]).unwrap().len(), 1);
}

#[test]
fn show_all_lists_every_setting_with_its_description() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let (rows, columns) = db.query_with_columns("SHOW ALL").unwrap();
    assert_eq!(columns, vec!["name", "setting", "description"]);
    assert!(rows.len() >= 40, "{}", rows.len());
    let names: Vec<String> = rows.iter().map(|r| text(&r.values[0])).collect();
    for expected in [
        "row_security",
        "server_version",
        "statement_timeout",
        "shared_buffers",
        "work_mem",
    ] {
        assert!(names.iter().any(|n| n == expected), "SHOW ALL lacks {expected}");
    }
    for row in &rows {
        assert!(
            !text(&row.values[2]).is_empty(),
            "{} has no description",
            text(&row.values[0])
        );
    }
    // The params family too.
    assert_eq!(db.query_params("SHOW ALL", &[]).unwrap().len(), rows.len());
}

#[test]
fn show_current_setting_and_pg_settings_agree_for_every_parameter() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("SET row_security = off").unwrap();
    db.execute("SET work_mem = 8192").unwrap();
    let all = db.query("SHOW ALL", &[]).unwrap();
    let (settings, columns) = db
        .query_with_columns("SELECT name, setting, short_desc, context, vartype, source FROM pg_settings")
        .unwrap();
    assert_eq!(
        columns,
        vec!["name", "setting", "short_desc", "context", "vartype", "source"]
    );
    assert_eq!(
        settings.len(),
        all.len(),
        "pg_settings and SHOW ALL list the same parameters"
    );
    for row in &all {
        let name = text(&row.values[0]);
        let setting = text(&row.values[1]);
        assert_eq!(scalar(&db, &format!("SHOW {name}")), setting, "SHOW {name}");
        assert_eq!(
            scalar(&db, &format!("SELECT current_setting('{name}')")),
            setting,
            "current_setting('{name}')"
        );
        let from_view = db
            .query_params(
                "SELECT setting FROM pg_settings WHERE name = $1",
                &[Value::String(name.clone())],
            )
            .unwrap();
        assert_eq!(from_view.len(), 1, "pg_settings row for {name}");
        assert_eq!(text(&from_view[0].values[0]), setting, "pg_settings.setting for {name}");
    }
    let rows = db
        .query(
            "SELECT setting, source FROM pg_settings WHERE name = 'row_security'",
            &[],
        )
        .unwrap();
    assert_eq!(text(&rows[0].values[0]), "off");
    assert_eq!(text(&rows[0].values[1]), "session");
    // Qualified spelling.
    assert_eq!(
        db.query("SELECT name FROM pg_catalog.pg_settings", &[]).unwrap().len(),
        all.len()
    );
}

#[test]
fn server_version_is_one_value_everywhere() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let shown = scalar(&db, "SHOW server_version");
    assert!(shown.starts_with("16.0 (HeliosDB Nano "), "{shown}");
    assert_eq!(scalar(&db, "SELECT current_setting('server_version')"), shown);
    assert_eq!(
        scalar(&db, "SELECT setting FROM pg_settings WHERE name = 'server_version'"),
        shown
    );
    assert!(scalar(&db, "SELECT version()").contains(&shown));
    assert_eq!(scalar(&db, "SHOW server_version_num"), "160000");
    assert_eq!(scalar(&db, "SELECT current_setting('server_version_num')"), "160000");
}

#[test]
fn server_level_names_read_the_same_everywhere() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("SET shared_buffers = 5").unwrap();
    assert_eq!(scalar(&db, "SHOW shared_buffers"), "5");
    assert_eq!(scalar(&db, "SELECT current_setting('shared_buffers')"), "5");
    assert_eq!(
        scalar(&db, "SELECT setting FROM pg_settings WHERE name = 'shared_buffers'"),
        "5"
    );
    let desc = scalar(&db, "SELECT short_desc FROM pg_settings WHERE name = 'shared_buffers'");
    assert!(desc.contains("nothing in this engine reads it"), "{desc}");
    // max_connections: postmaster-scoped, PostgreSQL's refusal.
    let err = db.execute("SET max_connections = 5").unwrap_err().to_string();
    assert!(err.contains("cannot be changed now"), "{err}");
    assert_eq!(
        scalar(&db, "SHOW max_connections"),
        scalar(&db, "SELECT current_setting('max_connections')")
    );
    // Unknown names still fail.
    assert!(db.query("SELECT current_setting('no_such_parameter')", &[]).is_err());
}
