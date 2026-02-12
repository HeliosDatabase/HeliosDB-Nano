//! End-to-end crash recovery integration test
//!
//! Tests that data survives a simulated crash (unclean shutdown) by:
//! 1. Opening a disk-backed database
//! 2. Inserting data
//! 3. Dropping the database without clean shutdown (simulating crash)
//! 4. Reopening the database
//! 5. Verifying data is recoverable via WAL auto-replay

use heliosdb_nano::EmbeddedDatabase;
use tempfile::TempDir;

/// Helper to create a disk-backed database in a temp directory
fn open_db(dir: &std::path::Path) -> EmbeddedDatabase {
    // Default config has WAL enabled with Sync mode
    EmbeddedDatabase::new(dir).expect("Failed to open database")
}

#[test]
fn test_crash_recovery_insert_reopen() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().to_path_buf();

    // Phase 1: Insert data and "crash" (drop without clean shutdown)
    {
        let db = open_db(&db_path);
        db.execute("CREATE TABLE crash_test (id INT, name TEXT)").unwrap();
        db.execute("INSERT INTO crash_test VALUES (1, 'Alice')").unwrap();
        db.execute("INSERT INTO crash_test VALUES (2, 'Bob')").unwrap();
        db.execute("INSERT INTO crash_test VALUES (3, 'Charlie')").unwrap();

        // Verify data is readable before crash
        let rows = db.query("SELECT id, name FROM crash_test", &[]).unwrap();
        assert_eq!(rows.len(), 3, "Should have 3 rows before crash");

        // Drop without calling any shutdown method = simulated crash
        drop(db);
    }

    // Phase 2: Reopen database — WAL auto-replay should recover data
    {
        let db = open_db(&db_path);

        // Schema should survive (stored in RocksDB metadata)
        let rows = db.query("SELECT id, name FROM crash_test", &[]).unwrap();
        assert!(
            !rows.is_empty(),
            "Table should exist and have data after crash recovery"
        );
        // Data should survive via RocksDB + WAL
        assert_eq!(rows.len(), 3, "Should have 3 rows after crash recovery");
    }
}

#[test]
fn test_crash_recovery_transaction_committed() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().to_path_buf();

    // Phase 1: Use explicit transaction, commit, then crash
    {
        let db = open_db(&db_path);
        db.execute("CREATE TABLE txn_test (id INT, val TEXT)").unwrap();

        // Committed transaction — should survive
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO txn_test VALUES (1, 'committed')").unwrap();
        db.execute("COMMIT").unwrap();

        drop(db);
    }

    // Phase 2: Verify committed data survived
    {
        let db = open_db(&db_path);
        let rows = db.query("SELECT val FROM txn_test WHERE id = 1", &[]).unwrap();
        assert_eq!(rows.len(), 1, "Committed transaction data should survive crash");
    }
}

#[test]
fn test_crash_recovery_multiple_tables() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().to_path_buf();

    // Phase 1: Create multiple tables with data
    {
        let db = open_db(&db_path);
        db.execute("CREATE TABLE users (id INT, name TEXT)").unwrap();
        db.execute("CREATE TABLE orders (id INT, user_id INT, amount INT)").unwrap();

        for i in 1..=10 {
            db.execute(&format!("INSERT INTO users VALUES ({}, 'user_{}')", i, i)).unwrap();
        }
        for i in 1..=20 {
            db.execute(&format!("INSERT INTO orders VALUES ({}, {}, {})", i, (i % 10) + 1, i * 100)).unwrap();
        }

        drop(db);
    }

    // Phase 2: Verify all tables recovered
    {
        let db = open_db(&db_path);

        let users = db.query("SELECT id FROM users", &[]).unwrap();
        assert_eq!(users.len(), 10, "All 10 users should survive crash");

        let orders = db.query("SELECT id FROM orders", &[]).unwrap();
        assert_eq!(orders.len(), 20, "All 20 orders should survive crash");
    }
}

#[test]
fn test_crash_recovery_update_delete() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().to_path_buf();

    // Phase 1: Insert, update, delete, then crash
    {
        let db = open_db(&db_path);
        db.execute("CREATE TABLE crud_test (id INT, status TEXT)").unwrap();
        db.execute("INSERT INTO crud_test VALUES (1, 'initial')").unwrap();
        db.execute("INSERT INTO crud_test VALUES (2, 'to_delete')").unwrap();
        db.execute("INSERT INTO crud_test VALUES (3, 'unchanged')").unwrap();

        db.execute("UPDATE crud_test SET status = 'updated' WHERE id = 1").unwrap();
        db.execute("DELETE FROM crud_test WHERE id = 2").unwrap();

        drop(db);
    }

    // Phase 2: Verify final state after recovery
    {
        let db = open_db(&db_path);
        let rows = db.query("SELECT id, status FROM crud_test", &[]).unwrap();

        // Should have 2 rows (id=1 updated, id=2 deleted, id=3 unchanged)
        assert_eq!(rows.len(), 2, "Should have 2 rows after recovery (1 deleted)");
    }
}
