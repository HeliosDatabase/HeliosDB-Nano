//! End-to-end SQL integration tests for database branching
//!
//! Tests complete SQL workflows:
//! - CREATE DATABASE BRANCH
//! - DROP DATABASE BRANCH
//! - MERGE DATABASE BRANCH
//! - System views for branch management
//!
//! NOTE: These tests are disabled because they use internal APIs that are not
//! publicly exposed. They need to be rewritten to use the public API.
//! Enable with: cargo test --features internal-tests

#![cfg(feature = "internal-tests")]

use heliosdb_nano::{Config, EmbeddedDatabase, Tuple, Value};

/// `Tuple::get_string` was removed; the convention in this repo is to match on
/// `Value` directly. Panics like the old `.get_string(i).unwrap()` did.
fn get_string(row: &Tuple, index: usize) -> String {
    match row.values.get(index) {
        Some(Value::String(s)) => s.clone(),
        other => panic!("expected a text value at index {index}, got {other:?}"),
    }
}

/// `Tuple::get_i64` was removed. Returns `None` where the old API returned
/// `Err` (NULL or a non-integer value).
fn get_i64(row: &Tuple, index: usize) -> Option<i64> {
    match row.values.get(index) {
        Some(Value::Int8(v)) => Some(*v),
        Some(Value::Int4(v)) => Some(*v as i64),
        Some(Value::Int2(v)) => Some(*v as i64),
        _ => None,
    }
}

#[test]
fn test_create_branch_sql() {
    let config = Config::in_memory();
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create a branch using SQL
    let result = db.execute("CREATE DATABASE BRANCH dev FROM main AS OF NOW").unwrap();
    assert_eq!(result, 0); // DDL affects no rows

    // Verify branch exists via system view
    let branches = db.query("SELECT * FROM pg_database_branches()", &[]).unwrap();
    assert!(branches.len() >= 2); // main + dev

    let branch_names: Vec<String> = branches.iter().map(|row| get_string(row, 0)).collect();
    assert!(branch_names.contains(&"main".to_string()));
    assert!(branch_names.contains(&"dev".to_string()));
}

#[test]
fn test_create_branch_with_parent() {
    let config = Config::in_memory();
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create staging from main
    db.execute("CREATE DATABASE BRANCH staging FROM main AS OF NOW")
        .unwrap();

    // Create feature from staging
    db.execute("CREATE DATABASE BRANCH feature FROM staging AS OF NOW")
        .unwrap();

    // Verify hierarchy
    let branches = db
        .query("SELECT branch_name, parent_id FROM pg_database_branches()", &[])
        .unwrap();
    assert_eq!(branches.len(), 3); // main, staging, feature
}

#[test]
fn test_create_branch_if_not_exists() {
    let config = Config::in_memory();
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create branch
    db.execute("CREATE DATABASE BRANCH dev FROM main AS OF NOW").unwrap();

    // Try to create again without IF NOT EXISTS (should error)
    let result = db.execute("CREATE DATABASE BRANCH dev FROM main AS OF NOW");
    assert!(result.is_err());

    // Try with IF NOT EXISTS (should succeed)
    let result = db.execute("CREATE DATABASE BRANCH IF NOT EXISTS dev FROM main AS OF NOW");
    assert!(result.is_ok());
}

#[test]
fn test_drop_branch_sql() {
    let config = Config::in_memory();
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create and drop branch
    db.execute("CREATE DATABASE BRANCH temp FROM main AS OF NOW").unwrap();
    db.execute("DROP DATABASE BRANCH temp").unwrap();

    // Verify branch no longer exists
    let branches = db.query("SELECT branch_name FROM pg_database_branches()", &[]).unwrap();
    let branch_names: Vec<String> = branches.iter().map(|row| get_string(row, 0)).collect();
    assert!(!branch_names.contains(&"temp".to_string()));
}

#[test]
fn test_drop_branch_if_exists() {
    let config = Config::in_memory();
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Try to drop non-existent branch without IF EXISTS (should error)
    let result = db.execute("DROP DATABASE BRANCH nonexistent");
    assert!(result.is_err());

    // Try with IF EXISTS (should succeed)
    let result = db.execute("DROP DATABASE BRANCH IF EXISTS nonexistent");
    assert!(result.is_ok());
}

#[test]
fn test_merge_branch_sql() {
    let config = Config::in_memory();
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Setup: create table and insert data
    db.execute("CREATE TABLE users (id SERIAL PRIMARY KEY, name TEXT)")
        .unwrap();
    db.execute("INSERT INTO users (name) VALUES ('Alice')").unwrap();

    // Create dev branch
    db.execute("CREATE DATABASE BRANCH dev FROM main AS OF NOW").unwrap();

    // Switch to dev and add more data (would need SET BRANCH support)
    // For now, test basic merge

    // Merge dev into main
    let result = db.execute("MERGE DATABASE BRANCH dev INTO main");
    assert!(result.is_ok());

    // Verify dev branch is marked as merged
    let branches = db
        .query("SELECT branch_name, status FROM pg_database_branches()", &[])
        .unwrap();
    let dev_status = branches
        .iter()
        .find(|row| get_string(row, 0) == "dev")
        .map(|row| get_string(row, 6)); // status column
    assert!(dev_status.is_some());
}

#[test]
fn test_merge_with_conflict_resolution() {
    let config = Config::in_memory();
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create branches
    db.execute("CREATE DATABASE BRANCH dev FROM main AS OF NOW").unwrap();
    db.execute("CREATE DATABASE BRANCH staging FROM main AS OF NOW")
        .unwrap();

    // Merge strategies are NOT implemented. This test previously failed to parse
    // (the option list kept its parentheses, so the key read as
    // "(conflict_resolution"); fixing that briefly made it pass, which was worse —
    // `StorageEngine::merge_branch` takes `_strategy` (unused) and hard-codes
    // `conflicts: Vec::new()`, so 'branch_wins' and 'target_wins' would both have
    // silently produced the same last-writer-wins merge. Both are now refused.
    for strategy in ["branch_wins", "target_wins"] {
        let err = db
            .execute(&format!(
                "MERGE DATABASE BRANCH dev INTO main WITH (conflict_resolution = '{strategy}')"
            ))
            .expect_err("conflict_resolution must not be silently ignored");
        assert!(
            err.to_string().contains("not implemented"),
            "expected an explicit unimplemented error for {strategy}, got: {err}"
        );
    }

    // Without the option both merges succeed.
    db.execute("MERGE DATABASE BRANCH dev INTO main").unwrap();
    db.execute("MERGE DATABASE BRANCH staging INTO main").unwrap();
}

#[test]
fn test_create_branch_as_of_timestamp() {
    let config = Config::in_memory();
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create table and insert data
    db.execute("CREATE TABLE logs (id SERIAL PRIMARY KEY, message TEXT, ts TIMESTAMP)")
        .unwrap();
    db.execute("INSERT INTO logs (message, ts) VALUES ('Event 1', CURRENT_TIMESTAMP)")
        .unwrap();

    // Create branch at current time
    let result = db.execute("CREATE DATABASE BRANCH snapshot1 FROM main AS OF NOW");
    assert!(result.is_ok());

    // Note: AS OF TIMESTAMP with specific timestamp would require
    // snapshot registration in actual usage
}

#[test]
fn test_system_view_pg_database_branches() {
    let config = Config::in_memory();
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create multiple branches
    db.execute("CREATE DATABASE BRANCH dev FROM main AS OF NOW").unwrap();
    db.execute("CREATE DATABASE BRANCH staging FROM main AS OF NOW")
        .unwrap();
    db.execute("CREATE DATABASE BRANCH feature FROM dev AS OF NOW").unwrap();

    // Query system view
    let branches = db.query("SELECT * FROM pg_database_branches()", &[]).unwrap();
    assert_eq!(branches.len(), 4); // main + 3 created

    // Verify schema (7 columns)
    assert_eq!(branches[0].values.len(), 7);

    // Verify main branch exists and has no parent
    let main_branch = branches.iter().find(|row| get_string(row, 0) == "main").unwrap();
    assert!(get_i64(main_branch, 2).is_none() || get_i64(main_branch, 2).unwrap() == 0);
    // parent_id null or 0
}

#[test]
fn test_system_view_pg_branch_stats() {
    let config = Config::in_memory();
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create branches
    db.execute("CREATE DATABASE BRANCH dev FROM main AS OF NOW").unwrap();
    db.execute("CREATE DATABASE BRANCH staging FROM main AS OF NOW")
        .unwrap();

    // Query branch statistics
    let stats = db.query("SELECT * FROM pg_branch_stats()", &[]).unwrap();
    assert!(stats.len() >= 2); // At least main + dev

    // Verify schema (6 columns)
    assert_eq!(stats[0].values.len(), 6);

    // Verify columns: branch_name, modified_keys, storage_bytes, commit_count, last_modified, compression_ratio
    let main_stats = stats.iter().find(|row| get_string(row, 0) == "main").unwrap();

    assert!(get_i64(main_stats, 1).is_some()); // modified_keys
    assert!(get_i64(main_stats, 2).is_some()); // storage_bytes
    assert!(get_i64(main_stats, 3).is_some()); // commit_count
}

#[test]
fn test_branch_lifecycle_complete() {
    let config = Config::in_memory();
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // 1. Create table and insert initial data
    db.execute("CREATE TABLE products (id SERIAL PRIMARY KEY, name TEXT, price REAL)")
        .unwrap();
    db.execute("INSERT INTO products (name, price) VALUES ('Widget', 9.99)")
        .unwrap();

    // 2. Create development branch
    db.execute("CREATE DATABASE BRANCH development FROM main AS OF NOW")
        .unwrap();

    // 3. Verify branch creation
    let branches = db.query("SELECT branch_name FROM pg_database_branches()", &[]).unwrap();
    assert_eq!(branches.len(), 2);

    // 4. Merge branch back
    db.execute("MERGE DATABASE BRANCH development INTO main").unwrap();

    // 5. Check statistics
    let stats = db.query("SELECT * FROM pg_branch_stats()", &[]).unwrap();
    assert!(stats.len() >= 1); // At least main exists

    // 6. Drop merged branch
    db.execute("DROP DATABASE BRANCH development").unwrap();

    // 7. Verify cleanup
    let final_branches = db.query("SELECT branch_name FROM pg_database_branches()", &[]).unwrap();
    assert_eq!(final_branches.len(), 1); // Only main remains
}

#[test]
fn test_branch_with_transactions() {
    let config = Config::in_memory();
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create table
    db.execute("CREATE TABLE accounts (id SERIAL PRIMARY KEY, balance REAL)")
        .unwrap();
    db.execute("INSERT INTO accounts (balance) VALUES (1000.0)").unwrap();

    // Create branch
    db.execute("CREATE DATABASE BRANCH test_txn FROM main AS OF NOW")
        .unwrap();

    // Note: Full transaction support within branches would require
    // SET BRANCH context switching, which is a future enhancement

    // Verify branch exists
    let branches = db.query("SELECT branch_name FROM pg_database_branches()", &[]).unwrap();
    let branch_names: Vec<String> = branches.iter().map(|row| get_string(row, 0)).collect();
    assert!(branch_names.contains(&"test_txn".to_string()));
}

#[test]
fn test_concurrent_branch_creation() {
    use std::sync::Arc;
    use std::thread;

    let config = Config::in_memory();
    let db = Arc::new(EmbeddedDatabase::with_config(config).unwrap());

    // Create branches concurrently
    let handles: Vec<_> = (0..5)
        .map(|i| {
            let db_clone = Arc::clone(&db);
            thread::spawn(move || {
                let branch_name = format!("branch_{}", i);
                let sql = format!("CREATE DATABASE BRANCH {} FROM main AS OF NOW", branch_name);
                db_clone.execute(&sql)
            })
        })
        .collect();

    // Wait for all threads
    for handle in handles {
        let result = handle.join().unwrap();
        assert!(result.is_ok());
    }

    // Verify all branches created
    let branches = db.query("SELECT branch_name FROM pg_database_branches()", &[]).unwrap();
    assert_eq!(branches.len(), 6); // main + 5 created
}

#[test]
fn test_merge_with_delete_after() {
    let config = Config::in_memory();
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create temporary branch
    db.execute("CREATE DATABASE BRANCH temp_feature FROM main AS OF NOW")
        .unwrap();

    // Merge and delete in one operation
    db.execute("MERGE DATABASE BRANCH temp_feature INTO main WITH (delete_branch_after = true)")
        .unwrap();

    // Verify branch no longer exists
    let branches = db.query("SELECT branch_name FROM pg_database_branches()", &[]).unwrap();
    let branch_names: Vec<String> = branches.iter().map(|row| get_string(row, 0)).collect();
    assert!(!branch_names.contains(&"temp_feature".to_string()));
}

#[test]
fn test_error_handling_invalid_branch_name() {
    let config = Config::in_memory();
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Try to create branch from non-existent parent
    let result = db.execute("CREATE DATABASE BRANCH child FROM nonexistent AS OF NOW");
    assert!(result.is_err());

    // Try to drop non-existent branch
    let result = db.execute("DROP DATABASE BRANCH nonexistent");
    assert!(result.is_err());

    // Try to merge non-existent branches
    let result = db.execute("MERGE DATABASE BRANCH fake1 INTO fake2");
    assert!(result.is_err());
}

#[test]
fn test_cannot_drop_main_branch() {
    let config = Config::in_memory();
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Try to drop main branch (should fail)
    let result = db.execute("DROP DATABASE BRANCH main");
    assert!(result.is_err());
}

#[test]
fn test_cannot_drop_branch_with_children() {
    let config = Config::in_memory();
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create parent and child
    db.execute("CREATE DATABASE BRANCH parent FROM main AS OF NOW").unwrap();
    db.execute("CREATE DATABASE BRANCH child FROM parent AS OF NOW")
        .unwrap();

    // Try to drop parent (should fail)
    let result = db.execute("DROP DATABASE BRANCH parent");
    assert!(result.is_err());

    // Drop child first
    db.execute("DROP DATABASE BRANCH child").unwrap();

    // Now can drop parent
    let result = db.execute("DROP DATABASE BRANCH parent");
    assert!(result.is_ok());
}

#[test]
fn test_branch_isolation_with_tables() {
    let config = Config::in_memory();
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create table in main
    db.execute("CREATE TABLE test_table (id SERIAL PRIMARY KEY, value TEXT)")
        .unwrap();
    db.execute("INSERT INTO test_table (value) VALUES ('main_data')")
        .unwrap();

    // Create branch
    db.execute("CREATE DATABASE BRANCH isolated FROM main AS OF NOW")
        .unwrap();

    // Note: Full branch context switching would be needed to verify
    // complete isolation. This test verifies branch creation succeeds.

    let branches = db.query("SELECT branch_name FROM pg_database_branches()", &[]).unwrap();
    assert_eq!(branches.len(), 2);
}

#[test]
fn test_multiple_sequential_merges() {
    let config = Config::in_memory();
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create and merge multiple branches sequentially
    for i in 0..5 {
        let branch_name = format!("iteration_{}", i);
        let create_sql = format!("CREATE DATABASE BRANCH {} FROM main AS OF NOW", branch_name);
        db.execute(&create_sql).unwrap();

        let merge_sql = format!("MERGE DATABASE BRANCH {} INTO main", branch_name);
        db.execute(&merge_sql).unwrap();
    }

    // Verify all branches marked as merged (still in metadata)
    let branches = db
        .query("SELECT branch_name, status FROM pg_database_branches()", &[])
        .unwrap();

    // Filter for iteration branches
    let merged_count = branches
        .iter()
        .filter(|row| {
            let name = get_string(row, 0);
            name.starts_with("iteration_")
        })
        .count();

    assert_eq!(merged_count, 5);
}

#[test]
fn test_pg_branch_stats_accuracy() {
    let config = Config::in_memory();
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create branch
    db.execute("CREATE DATABASE BRANCH stats_test FROM main AS OF NOW")
        .unwrap();

    // Query initial stats
    let stats = db
        .query(
            "SELECT modified_keys, storage_bytes, commit_count FROM pg_branch_stats() WHERE branch_name = 'stats_test'",
            &[],
        )
        .unwrap();

    assert_eq!(stats.len(), 1);

    // Initial values should be 0
    let row = &stats[0];
    assert_eq!(get_i64(row, 0).unwrap(), 0); // modified_keys
    assert_eq!(get_i64(row, 1).unwrap(), 0); // storage_bytes
    assert_eq!(get_i64(row, 2).unwrap(), 0); // commit_count
}
