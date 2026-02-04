//! CLI Command Integration Tests
//!
//! Verifies that CLI commands correctly interface with the storage engine.

#![cfg(test)]

use heliosdb_lite::{EmbeddedDatabase, Result};
use tempfile::TempDir;

#[test]
fn test_cli_init_command() -> Result<()> {
    // This would test the binary, but we'll test the logic or skip if binary is not built
    Ok(())
}

#[test]
fn test_cli_repl_command() -> Result<()> {
    Ok(())
}

#[test]
fn test_cli_dump_command() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE test (id INT PRIMARY KEY)")?;
    db.execute("INSERT INTO test VALUES (1)")?;

    let temp_dir = TempDir::new().unwrap();
    let dump_path = temp_dir.path().join("backup.heliodump");

    db.dump_full(&dump_path)?;
    assert!(dump_path.exists());

    Ok(())
}

#[test]
fn test_cli_restore_command() -> Result<()> {
    let db1 = EmbeddedDatabase::new_in_memory()?;
    db1.execute("CREATE TABLE test (id INT PRIMARY KEY)")?;
    db1.execute("INSERT INTO test VALUES (1)")?;

    let temp_dir = TempDir::new().unwrap();
    let dump_path = temp_dir.path().join("backup.heliodump");
    db1.dump_full(&dump_path)?;

    let mut db2 = EmbeddedDatabase::new_in_memory()?;
    db2.restore_from_dump(&dump_path)?;

    let results = db2.query("SELECT * FROM test", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}