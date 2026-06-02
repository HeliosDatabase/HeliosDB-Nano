//! Token Dashboard outstanding item #5: `CREATE SCHEMA` was rejected with
//! "Statement not yet supported: CreateSchema". It is now accepted as a
//! namespace no-op so migrations that issue it succeed.

use heliosdb_nano::{EmbeddedDatabase, Result};

#[test]
fn create_schema_is_accepted() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE SCHEMA IF NOT EXISTS dashboard")?;
    db.execute("CREATE SCHEMA analytics")?;
    // IF NOT EXISTS is idempotent.
    db.execute("CREATE SCHEMA IF NOT EXISTS dashboard")?;
    Ok(())
}

#[test]
fn create_schema_then_use_flat_namespace() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE SCHEMA IF NOT EXISTS dashboard")?;

    // HeliosDB keeps a flat namespace; a composite table name still works.
    db.execute("CREATE TABLE \"dashboard.messages\" (id INT PRIMARY KEY, n INT)")?;
    db.execute("INSERT INTO \"dashboard.messages\" VALUES (1, 42)")?;
    let rows = db.query("SELECT n FROM \"dashboard.messages\" WHERE id = 1", &[])?;
    assert_eq!(rows.len(), 1);
    Ok(())
}
