//! Integration tests for per-column storage modes
//!
//! Tests dictionary encoding, content-addressed storage, and columnar storage.

use heliosdb_nano::{
    storage::{ColumnarStore, WalEntry, WalOperation},
    Config, EmbeddedDatabase, Tuple, Value,
};

fn test_db() -> EmbeddedDatabase {
    let mut config = Config::default();
    config.storage.memory_only = true;
    config.storage.wal_enabled = false; // Faster tests
    EmbeddedDatabase::with_config(config).expect("Failed to create database")
}

fn wal_test_db(time_travel_enabled: bool) -> EmbeddedDatabase {
    let mut config = Config::default();
    config.storage.memory_only = true;
    config.storage.wal_enabled = true;
    config.storage.logical_wal_per_statement = true;
    config.storage.time_travel_enabled = time_travel_enabled;
    EmbeddedDatabase::with_config(config).expect("Failed to create database")
}

fn find_insert_wal_tuple(db: &EmbeddedDatabase, table_name: &str) -> Tuple {
    let last_lsn_key = b"wal:last_lsn".to_vec();
    let last_lsn_bytes = db
        .storage
        .get(&last_lsn_key)
        .unwrap()
        .expect("wal:last_lsn should exist");
    let last_lsn = u64::from_le_bytes(last_lsn_bytes.try_into().unwrap());

    for lsn in 1..=last_lsn {
        let key = format!("wal:entries:{:020}", lsn).into_bytes();
        let Some(raw) = db.storage.get(&key).unwrap() else {
            continue;
        };
        let entry = WalEntry::deserialize(&raw).unwrap();
        if let WalOperation::Insert { table, tuple, .. } = entry.operation {
            if table == table_name {
                return bincode::deserialize(&tuple).unwrap();
            }
        }
    }

    panic!("missing INSERT WAL entry for table {table_name}");
}

#[test]
fn test_dictionary_encoding() {
    let db = test_db();

    // Create table with default storage
    db.execute("CREATE TABLE orders (id INT PRIMARY KEY, status TEXT)")
        .unwrap();

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
    let migrated = db
        .execute("ALTER TABLE orders ALTER COLUMN status SET STORAGE DICTIONARY")
        .unwrap();
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
    db.execute("CREATE TABLE documents (id INT PRIMARY KEY, content TEXT)")
        .unwrap();

    // Create large duplicate content (> 1KB)
    let large_content = "x".repeat(2000);

    // Insert duplicate content
    db.execute(&format!("INSERT INTO documents VALUES (1, '{}')", large_content))
        .unwrap();
    db.execute(&format!("INSERT INTO documents VALUES (2, '{}')", large_content))
        .unwrap();
    db.execute("INSERT INTO documents VALUES (3, 'small')").unwrap();

    // Migrate to content-addressed storage
    let migrated = db
        .execute("ALTER TABLE documents ALTER COLUMN content SET STORAGE CONTENT_ADDRESSED")
        .unwrap();
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
    db.execute("CREATE TABLE metrics (id INT PRIMARY KEY, timestamp INT8, value FLOAT8)")
        .unwrap();

    // Insert data
    db.execute("INSERT INTO metrics VALUES (1, 1000, 1.5)").unwrap();
    db.execute("INSERT INTO metrics VALUES (2, 2000, 2.5)").unwrap();
    db.execute("INSERT INTO metrics VALUES (3, 3000, 3.5)").unwrap();

    // Migrate value column to columnar storage
    let migrated = db
        .execute("ALTER TABLE metrics ALTER COLUMN value SET STORAGE COLUMNAR")
        .unwrap();
    assert_eq!(migrated, 3);
    assert_eq!(
        db.storage
            .columnar_column_stats("metrics", "value")
            .unwrap()
            .non_null_values,
        3
    );

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
    assert_eq!(
        db.storage
            .columnar_column_stats("metrics", "value")
            .unwrap()
            .non_null_values,
        4
    );
}

#[test]
fn test_columnar_storage_fast_insert_side_data() {
    let db = test_db();

    db.execute("CREATE TABLE metrics (id INT PRIMARY KEY, value FLOAT8)")
        .unwrap();
    db.execute("ALTER TABLE metrics ALTER COLUMN value SET STORAGE COLUMNAR")
        .unwrap();

    // Simple literal INSERT uses the autocommit fast path. It must still
    // populate the physical columnar side store, otherwise a future columnar
    // scan would miss the row even though row scans read the inline value.
    db.execute("INSERT INTO metrics (id, value) VALUES (1, 10.5)").unwrap();

    let stats = db.storage.columnar_column_stats("metrics", "value").unwrap();
    assert_eq!(stats.non_null_values, 1);
    let rows = db.query("SELECT value FROM metrics WHERE id = 1", &[]).unwrap();
    assert_eq!(rows[0].values[0], Value::Float8(10.5));
}

#[test]
fn test_columnar_scan_reads_requested_side_data() {
    let db = test_db();

    db.execute("CREATE TABLE metrics (id INT PRIMARY KEY, value FLOAT8, bucket INT)")
        .unwrap();
    db.execute("ALTER TABLE metrics ALTER COLUMN value SET STORAGE COLUMNAR")
        .unwrap();
    db.execute("ALTER TABLE metrics ALTER COLUMN bucket SET STORAGE COLUMNAR")
        .unwrap();

    db.execute("INSERT INTO metrics (id, value, bucket) VALUES (1, 10.5, 1)")
        .unwrap();
    db.execute("INSERT INTO metrics (id, value, bucket) VALUES (2, NULL, 1)")
        .unwrap();
    db.execute("INSERT INTO metrics (id, value, bucket) VALUES (3, 30.5, 2)")
        .unwrap();
    db.execute("DELETE FROM metrics WHERE id = 3").unwrap();

    let schema = db.storage.catalog().get_table_schema("metrics").unwrap();
    let rows = db
        .storage
        .scan_table_with_schema_columnar_columns("metrics", &schema, &[1, 2])
        .unwrap();

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].row_id, Some(1));
    assert_eq!(rows[0].values[0], Value::Null);
    assert_eq!(rows[0].values[1], Value::Float8(10.5));
    assert_eq!(rows[0].values[2], Value::Int4(1));
    assert_eq!(rows[1].row_id, Some(2));
    assert_eq!(rows[1].values[1], Value::Null);
    assert_eq!(rows[1].values[2], Value::Int4(1));

    let projected = db.query("SELECT value FROM metrics WHERE bucket = 1", &[]).unwrap();
    assert_eq!(projected.len(), 2);
    assert_eq!(projected[0].values[0], Value::Float8(10.5));
    assert_eq!(projected[1].values[0], Value::Null);
}

#[test]
fn test_columnar_native_aggregate_respects_live_rows() {
    let db = test_db();

    db.execute("CREATE TABLE metrics (id INT PRIMARY KEY, a INT STORAGE COLUMNAR, b INT STORAGE COLUMNAR, e INT STORAGE COLUMNAR)")
        .unwrap();
    db.execute("INSERT INTO metrics (id, a, b, e) VALUES (1, 10, 10, 1)")
        .unwrap();
    db.execute("INSERT INTO metrics (id, a, b, e) VALUES (2, 20, 20, 1)")
        .unwrap();
    db.execute("INSERT INTO metrics (id, a, b, e) VALUES (3, 30, 30, 2)")
        .unwrap();
    db.execute("INSERT INTO metrics (id, a, b, e) VALUES (4, 40, 40, 2)")
        .unwrap();
    db.execute("DELETE FROM metrics WHERE id = 4").unwrap();

    let rows = db
        .query("SELECT SUM(a), AVG(a), MAX(b) FROM metrics WHERE b >= 20", &[])
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int8(50));
    assert_eq!(rows[0].values[1], Value::Float8(25.0));
    assert_eq!(rows[0].values[2], Value::Int4(30));

    let empty = db
        .query("SELECT COUNT(*), SUM(a), AVG(a), MAX(b) FROM metrics WHERE b > 100", &[])
        .unwrap();
    assert_eq!(empty.len(), 1);
    assert_eq!(
        empty[0].values,
        vec![Value::Int8(0), Value::Null, Value::Null, Value::Null]
    );

    let grouped = db
        .query(
            "SELECT e, COUNT(*), SUM(a) FROM metrics WHERE b >= 10 GROUP BY e",
            &[],
        )
        .unwrap();
    assert_eq!(grouped.len(), 2);
    assert_eq!(grouped[0].values, vec![Value::Int4(1), Value::Int8(2), Value::Int8(30)]);
    assert_eq!(grouped[1].values, vec![Value::Int4(2), Value::Int8(1), Value::Int8(30)]);
}

#[test]
fn test_columnar_transaction_insert_stages_side_data() {
    let db = test_db();

    db.execute("CREATE TABLE metrics (id INT PRIMARY KEY, value FLOAT8, bucket INT)")
        .unwrap();
    db.execute("ALTER TABLE metrics ALTER COLUMN value SET STORAGE COLUMNAR")
        .unwrap();
    db.execute("ALTER TABLE metrics ALTER COLUMN bucket SET STORAGE COLUMNAR")
        .unwrap();

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO metrics (id, value, bucket) VALUES (1, 10.5, 1)")
        .unwrap();
    db.execute("INSERT INTO metrics (id, value, bucket) VALUES (2, 20.5, 2)")
        .unwrap();
    let in_txn = db.query("SELECT value FROM metrics WHERE bucket = 2", &[]).unwrap();
    assert_eq!(in_txn.len(), 1);
    assert_eq!(in_txn[0].values[0], Value::Float8(20.5));
    db.execute("COMMIT").unwrap();

    let schema = db.storage.catalog().get_table_schema("metrics").unwrap();
    let rows = db
        .storage
        .scan_table_with_schema_columnar_columns("metrics", &schema, &[1, 2])
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values[1], Value::Float8(10.5));
    assert_eq!(rows[0].values[2], Value::Int4(1));
    assert_eq!(rows[1].values[1], Value::Float8(20.5));
    assert_eq!(rows[1].values[2], Value::Int4(2));

    let projected = db.query("SELECT value FROM metrics WHERE bucket = 2", &[]).unwrap();
    assert_eq!(projected.len(), 1);
    assert_eq!(projected[0].values[0], Value::Float8(20.5));
}

#[test]
fn test_columnar_fast_insert_wal_logs_logical_tuple() {
    let db = wal_test_db(true);

    db.execute("CREATE TABLE metrics (id INT PRIMARY KEY, value FLOAT8)")
        .unwrap();
    db.execute("ALTER TABLE metrics ALTER COLUMN value SET STORAGE COLUMNAR")
        .unwrap();
    db.execute("INSERT INTO metrics (id, value) VALUES (1, 10.5)").unwrap();

    let wal_tuple = find_insert_wal_tuple(&db, "metrics");
    assert_eq!(wal_tuple.values[1], Value::Float8(10.5));
}

#[test]
fn test_columnar_direct_insert_tt_off_wal_logs_logical_tuple() {
    let db = wal_test_db(false);

    db.execute("CREATE TABLE metrics (id INT PRIMARY KEY, value FLOAT8)")
        .unwrap();
    db.execute("ALTER TABLE metrics ALTER COLUMN value SET STORAGE COLUMNAR")
        .unwrap();
    db.storage
        .insert_tuple("metrics", Tuple::new(vec![Value::Int4(1), Value::Float8(10.5)]))
        .unwrap();

    let wal_tuple = find_insert_wal_tuple(&db, "metrics");
    assert_eq!(wal_tuple.values[1], Value::Float8(10.5));
}

#[test]
fn test_columnar_fast_update_refreshes_side_data() {
    let db = test_db();

    db.execute("CREATE TABLE metrics (id INT PRIMARY KEY, value FLOAT8)")
        .unwrap();
    db.execute("ALTER TABLE metrics ALTER COLUMN value SET STORAGE COLUMNAR")
        .unwrap();
    db.execute("INSERT INTO metrics (id, value) VALUES (1, 10.5)").unwrap();

    db.execute("UPDATE metrics SET value = 20.5 WHERE id = 1").unwrap();

    let rocks = db.storage.db();
    assert_eq!(
        ColumnarStore::get(rocks.as_ref(), "metrics", "value", 1).unwrap(),
        Some(Value::Float8(20.5))
    );
    let rows = db.query("SELECT value FROM metrics WHERE id = 1", &[]).unwrap();
    assert_eq!(rows[0].values[0], Value::Float8(20.5));
}

#[test]
fn test_columnar_slow_update_refreshes_side_data() {
    let db = test_db();

    db.execute("CREATE TABLE metrics (id INT PRIMARY KEY, value FLOAT8, bucket INT)")
        .unwrap();
    db.execute("ALTER TABLE metrics ALTER COLUMN value SET STORAGE COLUMNAR")
        .unwrap();
    db.execute("ALTER TABLE metrics ALTER COLUMN bucket SET STORAGE COLUMNAR")
        .unwrap();
    db.execute("INSERT INTO metrics (id, value, bucket) VALUES (1, 10.5, 1)")
        .unwrap();
    db.execute("INSERT INTO metrics (id, value, bucket) VALUES (2, 30.5, 2)")
        .unwrap();

    assert_eq!(
        db.execute("UPDATE metrics SET value = 20.5 WHERE bucket = 1").unwrap(),
        1
    );

    let rocks = db.storage.db();
    assert_eq!(
        ColumnarStore::get(rocks.as_ref(), "metrics", "value", 1).unwrap(),
        Some(Value::Float8(20.5))
    );
    let rows = db.query("SELECT value FROM metrics WHERE bucket = 1", &[]).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Float8(20.5));
}

#[test]
fn test_columnar_fast_delete_clears_side_data() {
    let db = test_db();

    db.execute("CREATE TABLE metrics (id INT PRIMARY KEY, value FLOAT8)")
        .unwrap();
    db.execute("ALTER TABLE metrics ALTER COLUMN value SET STORAGE COLUMNAR")
        .unwrap();
    db.execute("INSERT INTO metrics (id, value) VALUES (1, 10.5)").unwrap();

    db.execute("DELETE FROM metrics WHERE id = 1").unwrap();

    let rocks = db.storage.db();
    assert_eq!(
        ColumnarStore::get(rocks.as_ref(), "metrics", "value", 1).unwrap(),
        Some(Value::Null)
    );
    assert_eq!(
        db.storage
            .columnar_column_stats("metrics", "value")
            .unwrap()
            .non_null_values,
        0
    );
}

#[test]
fn test_columnar_execute_batch_delete_clears_side_data() {
    let db = test_db();

    db.execute("CREATE TABLE metrics (id INT PRIMARY KEY, value FLOAT8)")
        .unwrap();
    db.execute("ALTER TABLE metrics ALTER COLUMN value SET STORAGE COLUMNAR")
        .unwrap();
    db.execute("INSERT INTO metrics (id, value) VALUES (1, 10.5)").unwrap();

    db.execute_batch(&["DELETE FROM metrics WHERE id = 1"]).unwrap();

    let rocks = db.storage.db();
    assert_eq!(
        ColumnarStore::get(rocks.as_ref(), "metrics", "value", 1).unwrap(),
        Some(Value::Null)
    );
    assert_eq!(
        db.storage
            .columnar_column_stats("metrics", "value")
            .unwrap()
            .non_null_values,
        0
    );
}

#[test]
fn test_migrate_back_to_default() {
    let db = test_db();

    // Create table and migrate to dictionary
    db.execute("CREATE TABLE test (id INT PRIMARY KEY, val TEXT)").unwrap();
    db.execute("INSERT INTO test VALUES (1, 'foo'), (2, 'bar')").unwrap();

    db.execute("ALTER TABLE test ALTER COLUMN val SET STORAGE DICTIONARY")
        .unwrap();

    // Migrate back to default
    let migrated = db
        .execute("ALTER TABLE test ALTER COLUMN val SET STORAGE DEFAULT")
        .unwrap();
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
    db.execute(
        "CREATE TABLE combined (
        id INT PRIMARY KEY,
        status TEXT,
        description TEXT,
        score FLOAT8
    )",
    )
    .unwrap();

    // Insert data
    let desc = "x".repeat(2000); // Large content
    db.execute(&format!("INSERT INTO combined VALUES (1, 'active', '{}', 95.5)", desc))
        .unwrap();
    db.execute(&format!("INSERT INTO combined VALUES (2, 'active', '{}', 87.3)", desc))
        .unwrap();
    db.execute("INSERT INTO combined VALUES (3, 'inactive', 'small', 75.0)")
        .unwrap();

    // Set different storage modes for different columns
    db.execute("ALTER TABLE combined ALTER COLUMN status SET STORAGE DICTIONARY")
        .unwrap();
    db.execute("ALTER TABLE combined ALTER COLUMN description SET STORAGE CONTENT_ADDRESSED")
        .unwrap();
    db.execute("ALTER TABLE combined ALTER COLUMN score SET STORAGE COLUMNAR")
        .unwrap();

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
    let result = db
        .execute("ALTER TABLE test ALTER COLUMN val SET STORAGE DEFAULT")
        .unwrap();
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
