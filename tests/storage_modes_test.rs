//! Integration tests for per-column storage modes
//!
//! Tests dictionary encoding, content-addressed storage, and columnar storage.

use heliosdb_lite::{EmbeddedDatabase, Config, Value};

fn test_db() -> EmbeddedDatabase {
    let mut config = Config::default();
    config.storage.memory_only = true;
    config.storage.wal_enabled = false; // Faster tests
    EmbeddedDatabase::with_config(config).expect("Failed to create database")
}

#[test]
fn test_dictionary_encoding() {
    let db = test_db();

    // Create table with default storage
    db.execute("CREATE TABLE orders (id INT PRIMARY KEY, status TEXT)").unwrap();

    // Insert data with repetitive values
    db.execute("INSERT INTO orders VALUES (1, 'pending')").unwrap();
    db.execute("INSERT INTO orders VALUES (2, 'pending')").unwrap();
    db.execute("INSERT INTO orders VALUES (3, 'shipped')").unwrap();
    db.execute("INSERT INTO orders VALUES (4, 'pending')").unwrap();
    db.execute("INSERT INTO orders VALUES (5, 'delivered')").unwrap();

    // Verify data before migration
    let results = db.query("SELECT * FROM orders ORDER BY id", &[]).unwrap();
    assert_eq!(results.len(), 5);

    // Migrate to dictionary encoding
    let migrated = db.execute("ALTER TABLE orders ALTER COLUMN status SET STORAGE DICTIONARY").unwrap();
    assert_eq!(migrated, 5); // 5 rows migrated

    // Verify data after migration
    let results = db.query("SELECT * FROM orders ORDER BY id", &[]).unwrap();
    assert_eq!(results.len(), 5);
    assert_eq!(results[0].values[1], Value::String("pending".to_string()));
    assert_eq!(results[2].values[1], Value::String("shipped".to_string()));
    assert_eq!(results[4].values[1], Value::String("delivered".to_string()));

    // Insert new data (should use dictionary encoding)
    db.execute("INSERT INTO orders VALUES (6, 'pending')").unwrap();
    db.execute("INSERT INTO orders VALUES (7, 'returned')").unwrap(); // New value

    let results = db.query("SELECT * FROM orders WHERE id >= 6 ORDER BY id", &[]).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].values[1], Value::String("pending".to_string()));
    assert_eq!(results[1].values[1], Value::String("returned".to_string()));
}

#[test]
fn test_content_addressed_storage() {
    let db = test_db();

    // Create table
    db.execute("CREATE TABLE documents (id INT PRIMARY KEY, content TEXT)").unwrap();

    // Create large duplicate content (> 1KB)
    let large_content = "x".repeat(2000);

    // Insert duplicate content
    db.execute(&format!("INSERT INTO documents VALUES (1, '{}')", large_content)).unwrap();
    db.execute(&format!("INSERT INTO documents VALUES (2, '{}')", large_content)).unwrap();
    db.execute("INSERT INTO documents VALUES (3, 'small')").unwrap();

    // Migrate to content-addressed storage
    let migrated = db.execute("ALTER TABLE documents ALTER COLUMN content SET STORAGE CONTENT_ADDRESSED").unwrap();
    assert_eq!(migrated, 3);

    // Verify data is correctly retrieved
    let results = db.query("SELECT * FROM documents ORDER BY id", &[]).unwrap();
    assert_eq!(results.len(), 3);

    if let Value::String(s) = &results[0].values[1] {
        assert_eq!(s.len(), 2000);
        assert!(s.chars().all(|c| c == 'x'));
    } else {
        panic!("Expected String value");
    }

    // Both rows should have the same content
    assert_eq!(results[0].values[1], results[1].values[1]);
    assert_eq!(results[2].values[1], Value::String("small".to_string()));
}

#[test]
fn test_columnar_storage() {
    let db = test_db();

    // Create table
    db.execute("CREATE TABLE metrics (id INT PRIMARY KEY, timestamp INT8, value FLOAT8)").unwrap();

    // Insert data
    db.execute("INSERT INTO metrics VALUES (1, 1000, 1.5)").unwrap();
    db.execute("INSERT INTO metrics VALUES (2, 2000, 2.5)").unwrap();
    db.execute("INSERT INTO metrics VALUES (3, 3000, 3.5)").unwrap();

    // Migrate value column to columnar storage
    let migrated = db.execute("ALTER TABLE metrics ALTER COLUMN value SET STORAGE COLUMNAR").unwrap();
    assert_eq!(migrated, 3);

    // Verify data
    let results = db.query("SELECT * FROM metrics ORDER BY id", &[]).unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].values[2], Value::Float8(1.5));
    assert_eq!(results[1].values[2], Value::Float8(2.5));
    assert_eq!(results[2].values[2], Value::Float8(3.5));

    // Insert more data (should use columnar storage)
    db.execute("INSERT INTO metrics VALUES (4, 4000, 4.5)").unwrap();

    let results = db.query("SELECT * FROM metrics WHERE id = 4", &[]).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].values[2], Value::Float8(4.5));
}

#[test]
fn test_migrate_back_to_default() {
    let db = test_db();

    // Create table and migrate to dictionary
    db.execute("CREATE TABLE test (id INT PRIMARY KEY, val TEXT)").unwrap();
    db.execute("INSERT INTO test VALUES (1, 'foo'), (2, 'bar')").unwrap();

    db.execute("ALTER TABLE test ALTER COLUMN val SET STORAGE DICTIONARY").unwrap();

    // Migrate back to default
    let migrated = db.execute("ALTER TABLE test ALTER COLUMN val SET STORAGE DEFAULT").unwrap();
    assert_eq!(migrated, 2);

    // Verify data
    let results = db.query("SELECT * FROM test ORDER BY id", &[]).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].values[1], Value::String("foo".to_string()));
    assert_eq!(results[1].values[1], Value::String("bar".to_string()));
}

#[test]
fn test_multiple_storage_modes_same_table() {
    let db = test_db();

    // Create table with multiple columns
    db.execute("CREATE TABLE combined (
        id INT PRIMARY KEY,
        status TEXT,
        description TEXT,
        score FLOAT8
    )").unwrap();

    // Insert data
    let desc = "x".repeat(2000); // Large content
    db.execute(&format!("INSERT INTO combined VALUES (1, 'active', '{}', 95.5)", desc)).unwrap();
    db.execute(&format!("INSERT INTO combined VALUES (2, 'active', '{}', 87.3)", desc)).unwrap();
    db.execute("INSERT INTO combined VALUES (3, 'inactive', 'small', 75.0)").unwrap();

    // Set different storage modes for different columns
    db.execute("ALTER TABLE combined ALTER COLUMN status SET STORAGE DICTIONARY").unwrap();
    db.execute("ALTER TABLE combined ALTER COLUMN description SET STORAGE CONTENT_ADDRESSED").unwrap();
    db.execute("ALTER TABLE combined ALTER COLUMN score SET STORAGE COLUMNAR").unwrap();

    // Verify all data is correctly retrieved
    let results = db.query("SELECT * FROM combined ORDER BY id", &[]).unwrap();
    assert_eq!(results.len(), 3);

    // Check status (dictionary encoded)
    assert_eq!(results[0].values[1], Value::String("active".to_string()));
    assert_eq!(results[1].values[1], Value::String("active".to_string()));
    assert_eq!(results[2].values[1], Value::String("inactive".to_string()));

    // Check description (content addressed - duplicates should work)
    if let Value::String(s) = &results[0].values[2] {
        assert_eq!(s.len(), 2000);
    }
    assert_eq!(results[0].values[2], results[1].values[2]); // Same content
    assert_eq!(results[2].values[2], Value::String("small".to_string()));

    // Check score (columnar)
    assert_eq!(results[0].values[3], Value::Float8(95.5));
    assert_eq!(results[1].values[3], Value::Float8(87.3));
    assert_eq!(results[2].values[3], Value::Float8(75.0));
}

#[test]
fn test_no_change_when_same_mode() {
    let db = test_db();

    db.execute("CREATE TABLE test (id INT, val TEXT)").unwrap();
    db.execute("INSERT INTO test VALUES (1, 'test')").unwrap();

    // Setting same mode should return 0 (no rows migrated)
    let result = db.execute("ALTER TABLE test ALTER COLUMN val SET STORAGE DEFAULT").unwrap();
    assert_eq!(result, 0);
}

#[test]
fn test_invalid_column_error() {
    let db = test_db();

    db.execute("CREATE TABLE test (id INT, val TEXT)").unwrap();

    // Nonexistent column should error
    let result = db.execute("ALTER TABLE test ALTER COLUMN nonexistent SET STORAGE DICTIONARY");
    assert!(result.is_err());
}
