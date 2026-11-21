//! Row-level security survives a restart: policies, the per-table switches and
//! the tenant registry are stored with the database and loaded at open.

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
use tempfile::TempDir;

fn session(db: &EmbeddedDatabase, user: &str) -> SessionId {
    db.create_session(user, IsolationLevel::ReadCommitted).unwrap()
}

fn ids(db: &EmbeddedDatabase, s: SessionId, sql: &str) -> Vec<i64> {
    let rows = db.query_params_for_session(s, sql, &[]).unwrap();
    rows.iter()
        .map(|r| match &r.values[0] {
            Value::Int4(v) => i64::from(*v),
            Value::Int8(v) => *v,
            other => panic!("unexpected {other:?}"),
        })
        .collect()
}

/// The reproduction the brief asks for first: a policy and a tenant made
/// through the library API are still there after the handle is dropped and the
/// directory is opened again.
#[test]
fn a_library_policy_and_a_tenant_survive_a_restart() {
    let dir = TempDir::new().unwrap();
    let tenant_id;
    {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        db.execute("CREATE TABLE docs (id INT PRIMARY KEY, owner TEXT)").unwrap();
        db.execute("INSERT INTO docs VALUES (1, 'alice'), (2, 'bob')").unwrap();
        let tenant = db
            .tenant_manager
            .register_tenant("acme".to_string(), IsolationMode::SharedSchema);
        tenant_id = tenant.id;
        db.tenant_manager.create_rls_policy(
            "docs".to_string(),
            "own".to_string(),
            String::new(),
            RLSCommand::All,
            "owner = 'alice'".to_string(),
            None,
        );
        assert_eq!(db.tenant_manager.get_rls_policies("docs").len(), 1);
    }
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    let tenants = db.tenant_manager.list_tenants();
    assert_eq!(tenants.len(), 1, "the tenant registry is lost on restart");
    assert_eq!(tenants[0].id, tenant_id);
    assert_eq!(tenants[0].name, "acme");
    assert!(tenants[0].rls_enabled);
    let policies = db.tenant_manager.get_rls_policies("docs");
    assert_eq!(policies.len(), 1, "the policy is lost on restart");
    assert_eq!(policies[0].name, "own");
    assert_eq!(policies[0].using_expr, "owner = 'alice'");

    // And it still filters for that tenant.
    db.tenant_manager.set_current_context(TenantContext {
        tenant_id,
        user_id: "alice".to_string(),
        roles: vec![],
        isolation_mode: IsolationMode::SharedSchema,
    });
    let rows = db.query("SELECT id FROM docs", &[]).unwrap();
    assert_eq!(rows.len(), 1);
    db.tenant_manager.clear_current_context();
}

#[test]
fn a_changed_tenant_setting_and_a_deleted_tenant_survive_a_restart() {
    let dir = TempDir::new().unwrap();
    let (keep, gone);
    {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        keep = db
            .tenant_manager
            .register_tenant("keep".to_string(), IsolationMode::SharedSchema)
            .id;
        gone = db
            .tenant_manager
            .register_tenant("gone".to_string(), IsolationMode::SchemaPerTenant)
            .id;
        let mut limits = db.tenant_manager.get_tenant(keep).unwrap().limits;
        limits.max_qps = 4321;
        limits.max_connections = 7;
        db.tenant_manager.update_resource_limits(keep, limits).unwrap();
        db.tenant_manager.delete_tenant(gone).unwrap();
    }
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    assert!(db.tenant_manager.get_tenant(gone).is_none());
    let tenant = db.tenant_manager.get_tenant(keep).unwrap();
    assert_eq!(tenant.limits.max_qps, 4321);
    assert_eq!(tenant.limits.max_connections, 7);
    assert_eq!(tenant.isolation_mode, IsolationMode::SharedSchema);
}

#[test]
fn sql_policies_and_the_table_switches_survive_a_restart() {
    let dir = TempDir::new().unwrap();
    {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        db.execute("CREATE ROLE alice LOGIN").unwrap();
        db.execute("CREATE ROLE bob LOGIN").unwrap();
        db.execute("CREATE TABLE notes (id INT PRIMARY KEY, owner TEXT, body TEXT)")
            .unwrap();
        db.execute("INSERT INTO notes VALUES (1, 'alice', 'a'), (2, 'bob', 'b'), (3, 'bob', 'c')")
            .unwrap();
        db.execute("ALTER TABLE notes ENABLE ROW LEVEL SECURITY").unwrap();
        db.execute("ALTER TABLE notes FORCE ROW LEVEL SECURITY").unwrap();
        db.execute("CREATE POLICY own ON notes FOR ALL TO PUBLIC USING (owner = current_user) WITH CHECK (owner = current_user)")
            .unwrap();
        db.execute("CREATE POLICY no_c ON notes AS RESTRICTIVE FOR SELECT TO bob USING (body <> 'c')")
            .unwrap();
        let bob = session(&db, "bob");
        assert_eq!(ids(&db, bob, "SELECT id FROM notes ORDER BY id"), vec![2]);
    }
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    let alice = session(&db, "alice");
    let bob = session(&db, "bob");
    assert_eq!(ids(&db, alice, "SELECT id FROM notes ORDER BY id"), vec![1]);
    assert_eq!(ids(&db, bob, "SELECT id FROM notes ORDER BY id"), vec![2]);
    let err = db
        .execute_for_session(bob, "INSERT INTO notes VALUES (9, 'alice', 'x')")
        .unwrap_err()
        .to_string();
    assert!(err.contains("new row violates row-level security policy for table \"notes\""), "{err}");

    let rows = db
        .query(
            "SELECT policyname, permissive, cmd FROM pg_policies WHERE tablename = 'notes' ORDER BY policyname",
            &[],
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    let flags = db
        .query(
            "SELECT relrowsecurity, relforcerowsecurity FROM pg_class WHERE relname = 'notes'",
            &[],
        )
        .unwrap();
    assert_eq!(flags[0].values, vec![Value::Boolean(true), Value::Boolean(true)]);

    // Dropping and disabling are durable too.
    db.execute("DROP POLICY no_c ON notes").unwrap();
    db.execute("ALTER TABLE notes NO FORCE ROW LEVEL SECURITY").unwrap();
    drop(db);
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    let bob = session(&db, "bob");
    assert_eq!(ids(&db, bob, "SELECT id FROM notes ORDER BY id"), vec![2, 3]);
    let flags = db
        .query(
            "SELECT relrowsecurity, relforcerowsecurity FROM pg_class WHERE relname = 'notes'",
            &[],
        )
        .unwrap();
    assert_eq!(flags[0].values, vec![Value::Boolean(true), Value::Boolean(false)]);
}

#[test]
fn dropping_a_table_drops_its_policies() {
    let dir = TempDir::new().unwrap();
    {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, owner TEXT)").unwrap();
        db.execute("ALTER TABLE t ENABLE ROW LEVEL SECURITY").unwrap();
        db.execute("CREATE POLICY p ON t USING (owner = current_user)").unwrap();
        db.execute("DROP TABLE t").unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, owner TEXT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 'x')").unwrap();
    }
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    let s = session(&db, "someone");
    assert_eq!(ids(&db, s, "SELECT id FROM t"), vec![1]);
    assert_eq!(db.query("SELECT * FROM pg_policies", &[]).unwrap().len(), 0);
}
