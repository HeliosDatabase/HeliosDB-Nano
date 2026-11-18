//! Bug 1 + Bug 5 — `CREATE DATABASE` and StartupMessage DB-name validation.
//!
//! Bug 1 was filed because `psql -c "CREATE DATABASE testdb"` hit the
//! planner's catch-all error at `src/sql/planner.rs:741`. v3.25.0 wires
//! `Statement::CreateDatabase` and `Statement::Drop { object_type: Database }`
//! through to the existing `TenantManager` API as a thin metadata-only
//! wrapper.
//!
//! Bug 5 was filed because the PG-wire StartupMessage path at
//! `src/protocol/postgres/handler.rs:236-239` accepted any `database`
//! parameter without validating it. v3.25.0 validates the requested
//! database against the tenant list and the reserved names (`heliosdb`,
//! `postgres`); unknown names are rejected.

use heliosdb_nano::{tenant::IsolationMode, EmbeddedDatabase, Value};

#[test]
fn create_database_via_sql_succeeds() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE DATABASE testdb").expect("CREATE DATABASE testdb");

    let tenants = db.tenant_manager.list_tenants();
    assert!(
        tenants.iter().any(|t| t.name == "testdb"),
        "tenant 'testdb' should be registered after CREATE DATABASE; tenants={:?}",
        tenants.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    // Default isolation mode for ORM-compat path is DatabasePerTenant.
    let testdb = tenants.iter().find(|t| t.name == "testdb").unwrap();
    assert_eq!(testdb.isolation_mode, IsolationMode::DatabasePerTenant);
}

#[test]
fn create_database_if_not_exists_is_idempotent() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE DATABASE foo").expect("first create");
    // Second call without IF NOT EXISTS should error.
    let dup = db.execute("CREATE DATABASE foo");
    assert!(dup.is_err(), "duplicate CREATE DATABASE should error");
    let msg = dup.unwrap_err().to_string().to_lowercase();
    assert!(
        msg.contains("already exists") || msg.contains("foo"),
        "error should mention duplicate or name; got {msg}"
    );

    // IF NOT EXISTS path: no error.
    db.execute("CREATE DATABASE IF NOT EXISTS foo").expect("idempotent");

    // Still exactly one 'foo' tenant.
    let count = db
        .tenant_manager
        .list_tenants()
        .iter()
        .filter(|t| t.name == "foo")
        .count();
    assert_eq!(count, 1, "duplicate IF NOT EXISTS must not create a second tenant");
}

#[test]
fn drop_database_removes_tenant() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE DATABASE removeme").expect("create");
    assert!(db.tenant_manager.list_tenants().iter().any(|t| t.name == "removeme"));

    db.execute("DROP DATABASE removeme").expect("drop");
    assert!(
        !db.tenant_manager.list_tenants().iter().any(|t| t.name == "removeme"),
        "tenant should be gone after DROP DATABASE"
    );
}

#[test]
fn drop_database_if_exists_is_idempotent() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    // No error on a name that doesn't exist.
    db.execute("DROP DATABASE IF EXISTS never_existed")
        .expect("if-exists no-op");

    // Without IF EXISTS, should error.
    let err = db.execute("DROP DATABASE never_existed");
    assert!(err.is_err(), "DROP without IF EXISTS on missing db should error");
}

#[test]
fn drop_reserved_database_names_is_refused() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    for reserved in &["heliosdb", "postgres"] {
        let q = format!("DROP DATABASE {reserved}");
        let result = db.execute(&q);
        assert!(
            result.is_err(),
            "DROP DATABASE {reserved} should be refused; got {result:?}"
        );
        let msg = result.unwrap_err().to_string().to_lowercase();
        assert!(
            msg.contains("reserved") || msg.contains("cannot") || msg.contains("system"),
            "error should explain why {reserved} can't be dropped; got {msg}"
        );
    }
}

#[test]
fn create_reserved_database_names_is_refused() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    for reserved in &["heliosdb", "postgres"] {
        let q = format!("CREATE DATABASE {reserved}");
        let result = db.execute(&q);
        assert!(
            result.is_err(),
            "CREATE DATABASE {reserved} should be refused; got {result:?}"
        );
    }
    // IF NOT EXISTS must succeed silently for reserved names (idempotent shape).
    db.execute("CREATE DATABASE IF NOT EXISTS heliosdb")
        .expect("reserved IF NOT EXISTS");
    db.execute("CREATE DATABASE IF NOT EXISTS postgres")
        .expect("reserved IF NOT EXISTS");
}

#[test]
fn current_database_returns_default_when_no_active_tenant() {
    // Embedded path: current_database() returns the default keyspace name.
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    let rows = db.query("SELECT current_database()", &[]).expect("current_database");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get(0), Some(&Value::String("heliosdb".into())));
}

#[test]
fn pg_wire_database_validation_via_catalog_api() {
    use heliosdb_nano::protocol::postgres::catalog::PgCatalog;
    use std::sync::Arc;

    // Build a DB with a known tenant.
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    db.execute("CREATE DATABASE myapp").expect("create");

    // The validator helper used by the StartupMessage handler must accept:
    //   - 'heliosdb' (default keyspace)
    //   - 'postgres' (PG client compat)
    //   - any registered tenant name
    // and reject anything else.
    assert!(PgCatalog::is_valid_database_name(&db, "heliosdb"));
    assert!(PgCatalog::is_valid_database_name(&db, "postgres"));
    assert!(PgCatalog::is_valid_database_name(&db, "myapp"));
    assert!(!PgCatalog::is_valid_database_name(&db, "totally_made_up_db_12345"));
    assert!(!PgCatalog::is_valid_database_name(&db, ""));
}

// ---------------------------------------------------------------------------
// GitHub #48: a database is still there after a restart
// ---------------------------------------------------------------------------
//
// The name was kept in memory only. After a restart a connection by it was
// refused with 3D000, and `pg_database` never listed it.

fn database_names(db: &EmbeddedDatabase) -> Vec<String> {
    db.query("SELECT datname FROM pg_database ORDER BY oid", &[])
        .expect("pg_database")
        .iter()
        .map(|row| match &row.values[0] {
            Value::String(name) => name.clone(),
            other => panic!("datname: {other:?}"),
        })
        .collect()
}

#[test]
fn a_created_database_is_listed_and_survives_a_reopen() {
    let dir = tempfile::tempdir().expect("dir");
    {
        let db = EmbeddedDatabase::new(dir.path()).expect("db");
        db.execute("CREATE DATABASE compat_db").expect("create");
        db.execute("CREATE DATABASE \"Mixed_Db\"").expect("create");
        assert!(db.database_name_is_valid("compat_db"));
        assert_eq!(database_names(&db), ["heliosdb", "compat_db", "Mixed_Db"]);
    }
    let db = EmbeddedDatabase::new(dir.path()).expect("reopen");
    // Nothing in memory knows the names yet; the store does.
    assert!(db.tenant_manager.list_tenants().is_empty());
    assert_eq!(database_names(&db), ["heliosdb", "compat_db", "Mixed_Db"]);
    assert!(db.database_name_is_valid("compat_db"));
    assert!(
        db.database_name_is_valid("COMPAT_DB"),
        "a database name is matched in any case"
    );
    assert!(db.database_name_is_valid("mixed_db"));
    assert!(!db.database_name_is_valid("never_created"));
    // The name is bound to a tenant again on first use, once.
    assert!(db.database_name_is_valid("compat_db"));
    let tenants = db.tenant_manager.list_tenants();
    assert_eq!(tenants.iter().filter(|t| t.name == "compat_db").count(), 1);
    // It still cannot be created twice.
    assert!(db.execute("CREATE DATABASE compat_db").is_err());
    db.execute("CREATE DATABASE IF NOT EXISTS compat_db")
        .expect("if not exists");
}

#[test]
fn a_dropped_database_does_not_come_back() {
    let dir = tempfile::tempdir().expect("dir");
    {
        let db = EmbeddedDatabase::new(dir.path()).expect("db");
        db.execute("CREATE DATABASE keep_db").expect("create");
        db.execute("CREATE DATABASE gone_db").expect("create");
        db.execute("DROP DATABASE gone_db").expect("drop");
        assert!(!db.database_name_is_valid("gone_db"));
        assert_eq!(database_names(&db), ["heliosdb", "keep_db"]);
    }
    let db = EmbeddedDatabase::new(dir.path()).expect("reopen");
    assert!(!db.database_name_is_valid("gone_db"));
    assert!(db.database_name_is_valid("keep_db"));
    // Dropped after a reopen, before anything connected by the name.
    db.execute("DROP DATABASE keep_db").expect("drop a recorded database");
    assert!(!db.database_name_is_valid("keep_db"));
    assert_eq!(database_names(&db), ["heliosdb"]);
    assert!(db.execute("DROP DATABASE keep_db").is_err());
    db.execute("DROP DATABASE IF EXISTS keep_db").expect("if exists");
}
