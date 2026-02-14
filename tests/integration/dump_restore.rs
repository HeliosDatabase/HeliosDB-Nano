//! Dump and Restore Integration Tests
//!
//! Tests for memory-to-disk persistence.

#![cfg(test)]

use heliosdb_nano::{EmbeddedDatabase, Result};
use crate::test_helpers::*;
use std::fs;
use tempfile::TempDir;

#[test]
fn test_full_dump_basic() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let temp_dir = TempDir::new().unwrap();
    let dump_path = temp_dir.path().join("test.heliodump");

    // Create test data
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)")?;
    db.execute("INSERT INTO users VALUES (1, 'Alice', 30)")?;
    db.execute("INSERT INTO users VALUES (2, 'Bob', 25)")?;
    db.execute("INSERT INTO users VALUES (3, 'Charlie', 35)")?;

    // Perform full dump
    let report = db.dump_full(&dump_path)?;

    // Verify dump file was created
    assert!(dump_path.exists());
    assert!(report.compressed_size > 0);
    assert_eq!(report.table_count, 1);
    assert_eq!(report.total_rows, 3);

    Ok(())
}

#[test]
fn test_full_restore_basic() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let dump_path = temp_dir.path().join("test.heliodump");

    // Create and dump database
    let db1 = EmbeddedDatabase::new_in_memory()?;
    db1.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)")?;
    db1.execute("INSERT INTO users VALUES (1, 'Alice', 30)")?;
    db1.execute("INSERT INTO users VALUES (2, 'Bob', 25)")?;

    db1.dump_full(&dump_path)?;

    // Create new database and restore
    let mut db2 = EmbeddedDatabase::new_in_memory()?;
    db2.restore_from_dump(&dump_path)?;

    // Verify data
    let results = db2.query("SELECT * FROM users ORDER BY id", &[])?;
    assert_eq!(results.len(), 2);
    assert_eq!(get_string_value(&results[0], 1), Some("Alice".to_string()));
    assert_eq!(get_int_value(&results[0], 2), Some(30));

    Ok(())
}

#[test]
fn test_dump_compression_zstd() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let temp_dir = TempDir::new().unwrap();
    let uncompressed_path = temp_dir.path().join("uncompressed.heliodump");
    let compressed_path = temp_dir.path().join("compressed.heliodump");

    // Create large dataset
    db.execute("CREATE TABLE large_data (id INT PRIMARY KEY, data TEXT)")?;
    for i in 0..100 {
        db.execute(&format!("INSERT INTO large_data VALUES ({}, '{}')", i, "x".repeat(1000)))?;
    }

    // Dump without compression
    db.dump_full_uncompressed(&uncompressed_path)?;
    let uncompressed_size = fs::metadata(&uncompressed_path)?.len();

    // Dump with zstd compression
    db.dump_full_compressed(&compressed_path, heliosdb_nano::storage::DumpCompressionType::Zstd)?;
    let compressed_size = fs::metadata(&compressed_path)?.len();

    // Compressed should be significantly smaller
    assert!(compressed_size < uncompressed_size);

    Ok(())
}

#[test]
fn test_dump_checksum_validation() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let temp_dir = TempDir::new().unwrap();
    let dump_path = temp_dir.path().join("test.heliodump");

    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)")?;
    db.execute("INSERT INTO users VALUES (1, 'Alice')")?;

    db.dump_full(&dump_path)?;

    // Corrupt the dump file
    let mut content = fs::read(&dump_path)?;
    let len = content.len();
    if len > 100 {
        content[len - 10] ^= 0xFF; // Flip a byte near the end to avoid metadata header
    }
    fs::write(&dump_path, content)?;

    // Restore should fail
    let mut db2 = EmbeddedDatabase::new_in_memory()?;
    let result = db2.restore_from_dump(&dump_path);
    assert!(result.is_err(), "Restore should fail for corrupted dump file");

    Ok(())
}

#[test]
fn test_sql_dump_format() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let temp_dir = TempDir::new().unwrap();
    let dump_path = temp_dir.path().join("dump.sql");

    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)")?;
    db.execute("INSERT INTO users VALUES (1, 'Alice')")?;
    db.execute("INSERT INTO users VALUES (2, 'Bob')")?;

    // Create SQL dump
    let report = db.dump_sql(&dump_path)?;
    assert_eq!(report.table_count, 1);
    assert_eq!(report.total_rows, 2);

    // Verify content
    let content = fs::read_to_string(&dump_path)?;
    assert!(content.contains("CREATE TABLE IF NOT EXISTS users"));
    assert!(content.contains("INSERT INTO users VALUES"));
    assert!(content.contains("(1, 'Alice')"));
    assert!(content.contains("(2, 'Bob')"));

    Ok(())
}

#[test]
fn test_dump_version_compatibility() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let temp_dir = TempDir::new().unwrap();
    let dump_path = temp_dir.path().join("test.heliodump");

    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)")?;
    db.dump_full(&dump_path)?;

    // Verify metadata is readable
    let metadata = db.read_dump_metadata(&dump_path)?;
    assert!(metadata.dump_id > 0);

    Ok(())
}

#[test]
fn test_dump_restore_multi_table_roundtrip() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let dump_path = temp_dir.path().join("multi.heliodump");

    // Create database with multiple tables and data types
    let db1 = EmbeddedDatabase::new_in_memory()?;
    db1.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, active BOOLEAN)")?;
    db1.execute("CREATE TABLE orders (id INT PRIMARY KEY, user_id INT, amount FLOAT, note TEXT)")?;

    db1.execute("INSERT INTO users VALUES (1, 'Alice', true)")?;
    db1.execute("INSERT INTO users VALUES (2, 'Bob', false)")?;
    db1.execute("INSERT INTO users VALUES (3, 'Charlie', true)")?;

    db1.execute("INSERT INTO orders VALUES (100, 1, 29.99, 'First order')")?;
    db1.execute("INSERT INTO orders VALUES (101, 1, 49.50, 'Second order')")?;
    db1.execute("INSERT INTO orders VALUES (102, 2, 15.00, 'Bobs order')")?;

    // Dump
    let metadata = db1.dump_full(&dump_path)?;
    assert_eq!(metadata.table_count, 2);
    assert_eq!(metadata.total_rows, 6);

    // Restore into fresh database
    let mut db2 = EmbeddedDatabase::new_in_memory()?;
    db2.restore_from_dump(&dump_path)?;

    // Verify users table
    let users = db2.query("SELECT * FROM users ORDER BY id", &[])?;
    assert_eq!(users.len(), 3);
    assert_eq!(get_string_value(&users[0], 1), Some("Alice".to_string()));
    assert_eq!(get_string_value(&users[2], 1), Some("Charlie".to_string()));

    // Verify orders table
    let orders = db2.query("SELECT * FROM orders ORDER BY id", &[])?;
    assert_eq!(orders.len(), 3);
    assert_eq!(get_int_value(&orders[0], 1), Some(1)); // user_id
    assert_eq!(get_string_value(&orders[2], 3), Some("Bobs order".to_string()));

    // Verify queries work on restored data
    let count = db2.query("SELECT COUNT(*) FROM orders WHERE user_id = 1", &[])?;
    assert_eq!(count.len(), 1);

    Ok(())
}

#[test]
fn test_dump_restore_incremental() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let dump_path = temp_dir.path().join("incremental.heliodump");

    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE items (id INT PRIMARY KEY, name TEXT)")?;
    db.execute("INSERT INTO items VALUES (1, 'Widget')")?;

    // Mark table as dirty for incremental dump tracking
    db.dump_manager.dirty_tracker().mark_table_dirty("items");

    // Create incremental dump
    let metadata = db.dump_incremental(&dump_path)?;
    assert!(dump_path.exists());
    assert!(metadata.total_rows >= 1);

    Ok(())
}