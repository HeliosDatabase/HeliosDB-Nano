//! Comprehensive tests for branch data isolation
//!
//! Tests branch data isolation across both persistent and in-memory storage modes.
//! Ensures metadata properties and data isolation work correctly regardless of storage mode.

use heliosdb_lite::{Config, storage::{StorageEngine, BranchOptions}};
use std::path::PathBuf;
use tempfile::TempDir;

// Helper function to create a temporary database directory
fn create_temp_db() -> (TempDir, PathBuf) {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let db_path = temp_dir.path().join("test_db");
    (temp_dir, db_path)
}

// Helper function to create persistent config
fn persistent_config(db_path: &PathBuf) -> Config {
    let mut config = Config::default();
    config.storage.path = Some(db_path.clone());
    config.storage.memory_only = false;
    config
}

// ============================================================================
// PERSISTENT MODE TESTS
// ============================================================================

#[test]
fn test_persistent_branch_metadata_persistence() {
    let (_temp_dir, db_path) = create_temp_db();

    // Create database and branches
    {
        let config = persistent_config(&db_path);
        let engine = StorageEngine::open(&db_path, &config).expect("Failed to open engine");

        // Insert data in main
        engine.put(&b"key1".to_vec(), b"main_value").expect("Failed to put in main");

        // Create branch
        let branch_id = engine.create_branch(
            "dev",
            Some("main"),
            BranchOptions::default(),
        ).expect("Failed to create branch");

        assert!(branch_id > 1, "Branch ID should be > 1");

        // List branches to verify creation
        let branches = engine.list_branches().expect("Failed to list branches");
        assert_eq!(branches.len(), 2, "Should have main and dev branches");

        // Verify branch names
        let names: Vec<_> = branches.iter().map(|b| b.name.as_str()).collect();
        assert!(names.contains(&"main"), "Should contain main branch");
        assert!(names.contains(&"dev"), "Should contain dev branch");

        // Write data in the dev branch
        let mut tx = engine.begin_branch_transaction("dev").expect("Failed to start transaction");
        tx.put(b"key1".to_vec(), b"dev_value".to_vec()).expect("Failed to put in dev");
        tx.commit().expect("Failed to commit transaction");
    }

    // Reopen database and verify metadata persistence
    {
        let config = persistent_config(&db_path);
        let engine = StorageEngine::open(&db_path, &config).expect("Failed to reopen engine");

        // Verify branches still exist
        let branches = engine.list_branches().expect("Failed to list branches after restart");
        assert_eq!(branches.len(), 2, "Branches should persist after restart");

        let names: Vec<_> = branches.iter().map(|b| b.name.as_str()).collect();
        assert!(names.contains(&"dev"), "Dev branch should persist");

        // Verify data isolation persists
        let main_value = engine.get(&b"key1".to_vec()).expect("Failed to get from main");
        assert_eq!(main_value, Some(b"main_value".to_vec()), "Main branch data should be unchanged");

        let tx = engine.begin_branch_transaction("dev").expect("Failed to start transaction");
        let dev_value = tx.get(&b"key1".to_vec()).expect("Failed to get from dev");
        assert_eq!(dev_value, Some(b"dev_value".to_vec()), "Dev branch data should persist");
    }
}

#[test]
fn test_persistent_complex_branch_hierarchy() {
    let (_temp_dir, db_path) = create_temp_db();

    // Create complex hierarchy
    {
        let config = persistent_config(&db_path);
        let engine = StorageEngine::open(&db_path, &config).expect("Failed to open engine");

        // Create hierarchy: main -> feature -> feature-sub
        engine.put(&b"data".to_vec(), b"main_data").expect("Failed to put in main");

        engine.create_branch("feature", Some("main"), BranchOptions::default())
            .expect("Failed to create feature branch");

        engine.create_branch("feature-sub", Some("feature"), BranchOptions::default())
            .expect("Failed to create feature-sub branch");

        // Modify at each level
        let mut tx_feature = engine.begin_branch_transaction("feature")
            .expect("Failed to start feature transaction");
        tx_feature.put(b"data".to_vec(), b"feature_data".to_vec())
            .expect("Failed to put in feature");
        tx_feature.commit().expect("Failed to commit feature transaction");

        let mut tx_sub = engine.begin_branch_transaction("feature-sub")
            .expect("Failed to start sub transaction");
        tx_sub.put(b"data".to_vec(), b"sub_data".to_vec())
            .expect("Failed to put in sub");
        tx_sub.commit().expect("Failed to commit sub transaction");
    }

    // Verify hierarchy persists after restart
    {
        let config = persistent_config(&db_path);
        let engine = StorageEngine::open(&db_path, &config).expect("Failed to reopen engine");

        let branches = engine.list_branches().expect("Failed to list branches");
        assert_eq!(branches.len(), 3, "Should have 3 branches after restart");

        // Verify data isolation at each level
        let main_val = engine.get(&b"data".to_vec()).expect("Failed to get main");
        assert_eq!(main_val, Some(b"main_data".to_vec()), "Main data should persist");

        let tx_feature = engine.begin_branch_transaction("feature")
            .expect("Failed to start feature transaction");
        let feature_val = tx_feature.get(&b"data".to_vec()).expect("Failed to get feature");
        assert_eq!(feature_val, Some(b"feature_data".to_vec()), "Feature data should persist");

        let tx_sub = engine.begin_branch_transaction("feature-sub")
            .expect("Failed to start sub transaction");
        let sub_val = tx_sub.get(&b"data".to_vec()).expect("Failed to get sub");
        assert_eq!(sub_val, Some(b"sub_data".to_vec()), "Sub data should persist");
    }
}

// ============================================================================
// IN-MEMORY MODE TESTS
// ============================================================================

#[test]
fn test_in_memory_branch_metadata_isolation() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).expect("Failed to open in-memory engine");

    // Insert data in main
    engine.put(&b"key1".to_vec(), b"main_value").expect("Failed to put in main");

    // Create multiple branches
    let branch_ids: Vec<_> = (0..5)
        .map(|i| {
            let branch_name = format!("branch_{}", i);
            engine.create_branch(&branch_name, Some("main"), BranchOptions::default())
                .expect(&format!("Failed to create {}", branch_name))
        })
        .collect();

    assert_eq!(branch_ids.len(), 5, "Should have created 5 branches");

    // Verify all branches exist
    let branches = engine.list_branches().expect("Failed to list branches");
    assert_eq!(branches.len(), 6, "Should have main + 5 branches");

    // Write different values to each branch
    for i in 0..5 {
        let branch_name = format!("branch_{}", i);
        let mut tx = engine.begin_branch_transaction(&branch_name)
            .expect(&format!("Failed to start transaction for {}", branch_name));
        let value = format!("branch_{}_value", i);
        tx.put(b"key1".to_vec(), value.into_bytes())
            .expect(&format!("Failed to put in {}", branch_name));
        tx.commit().expect(&format!("Failed to commit {}", branch_name));
    }

    // Verify isolation
    let main_val = engine.get(&b"key1".to_vec()).expect("Failed to get from main");
    assert_eq!(main_val, Some(b"main_value".to_vec()), "Main branch should be unchanged");

    for i in 0..5 {
        let branch_name = format!("branch_{}", i);
        let tx = engine.begin_branch_transaction(&branch_name)
            .expect(&format!("Failed to start transaction for {}", branch_name));
        let value = tx.get(&b"key1".to_vec())
            .expect(&format!("Failed to get from {}", branch_name));
        let expected = format!("branch_{}_value", i);
        assert_eq!(value, Some(expected.into_bytes()),
                   "Branch {} should have isolated value", i);
    }
}

#[test]
fn test_in_memory_branch_metadata_properties() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).expect("Failed to open in-memory engine");

    // Create a branch with options
    let options = BranchOptions::default();
    let branch_id = engine.create_branch("test_branch", Some("main"), options)
        .expect("Failed to create branch");

    // Verify branch metadata is retrievable
    let branches = engine.list_branches().expect("Failed to list branches");
    let branch = branches.iter()
        .find(|b| b.name == "test_branch")
        .expect("Branch should exist in list");

    assert_eq!(branch.branch_id, branch_id, "Branch ID should match");
    assert_eq!(branch.name, "test_branch", "Branch name should match");
    assert!(branch.parent_id.is_some(), "Branch should have parent_id");
    assert_eq!(branch.parent_id.unwrap(), 1, "Parent should be main (ID 1)");
}

#[test]
fn test_in_memory_concurrent_branch_isolation() {
    use std::sync::Arc;
    use std::thread;

    let config = Config::in_memory();
    let engine = Arc::new(StorageEngine::open_in_memory(&config)
        .expect("Failed to open in-memory engine"));

    // Create branches for each thread
    for i in 0..3 {
        engine.create_branch(&format!("branch_{}", i), Some("main"), BranchOptions::default())
            .expect(&format!("Failed to create branch_{}", i));
    }

    let mut handles = vec![];

    // Spawn threads that write to different branches
    for i in 0..3 {
        let engine_clone = Arc::clone(&engine);
        let handle = thread::spawn(move || {
            let branch_name = format!("branch_{}", i);

            // Write multiple times to ensure metadata consistency
            for j in 0..50 {
                let mut tx = engine_clone.begin_branch_transaction(&branch_name)
                    .expect(&format!("Failed to start transaction"));
                let key = format!("key{}", j).into_bytes();
                let value = format!("thread{}_value{}", i, j).into_bytes();
                tx.put(key, value).expect(&format!("Failed to put in transaction"));
                tx.commit().expect(&format!("Failed to commit transaction"));
            }
        });
        handles.push(handle);
    }

    // Wait for all threads to complete
    for handle in handles {
        handle.join().expect("Thread panicked");
    }

    // Verify data isolation
    for i in 0..3 {
        let branch_name = format!("branch_{}", i);
        let tx = engine.begin_branch_transaction(&branch_name)
            .expect(&format!("Failed to start transaction"));

        for j in 0..50 {
            let key = format!("key{}", j).into_bytes();
            let expected = format!("thread{}_value{}", i, j).into_bytes();
            let value = tx.get(&key).expect(&format!("Failed to get key"));
            assert_eq!(value, Some(expected),
                       "Branch {} key {} should have correct isolated value", i, j);
        }
    }
}

#[test]
fn test_in_memory_large_dataset_isolation() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).expect("Failed to open in-memory engine");

    // Insert large dataset in main
    const DATA_SIZE: usize = 1000;
    for i in 0..DATA_SIZE {
        let key = format!("key{}", i).into_bytes();
        let value = format!("main_value{}", i).into_bytes();
        engine.put(&key, &value).expect("Failed to put in main");
    }

    // Create branch with large dataset
    engine.create_branch("feature", Some("main"), BranchOptions::default())
        .expect("Failed to create branch");

    // Modify a subset of keys in branch
    {
        let mut tx = engine.begin_branch_transaction("feature")
            .expect("Failed to start transaction");

        for i in 0..100 {
            let key = format!("key{}", i).into_bytes();
            let value = format!("feature_value{}", i).into_bytes();
            tx.put(key, value).expect("Failed to put in branch");
        }

        tx.commit().expect("Failed to commit transaction");
    }

    // Verify isolation at scale
    let tx = engine.begin_branch_transaction("feature")
        .expect("Failed to start read transaction");

    // Modified keys should have branch values
    for i in 0..100 {
        let key = format!("key{}", i).into_bytes();
        let expected = format!("feature_value{}", i).into_bytes();
        let value = tx.get(&key).expect("Failed to get key");
        assert_eq!(value, Some(expected), "Modified key {} should have branch value", i);
    }

    // Unmodified keys should have main values
    for i in 100..DATA_SIZE {
        let key = format!("key{}", i).into_bytes();
        let expected = format!("main_value{}", i).into_bytes();
        let value = tx.get(&key).expect("Failed to get key");
        assert_eq!(value, Some(expected), "Unmodified key {} should have main value", i);
    }
}

// ============================================================================
// CROSS-MODE COMPARISON TESTS
// ============================================================================

#[test]
fn test_branch_isolation_behavior_consistency() {
    let (_temp_dir, db_path) = create_temp_db();

    // Test persistent mode
    {
        let config = persistent_config(&db_path);
        let engine = StorageEngine::open(&db_path, &config).expect("Failed to open persistent engine");

        engine.put(&b"test_key".to_vec(), b"persistent_value").expect("Failed to put");

        engine.create_branch("test", Some("main"), BranchOptions::default())
            .expect("Failed to create branch");

        let mut tx = engine.begin_branch_transaction("test").expect("Failed to start tx");
        tx.put(b"test_key".to_vec(), b"branch_value".to_vec()).expect("Failed to put in branch");
        tx.commit().expect("Failed to commit");
    }

    // Test in-memory mode with same operations
    {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).expect("Failed to open in-memory engine");

        engine.put(&b"test_key".to_vec(), b"memory_value").expect("Failed to put");

        engine.create_branch("test", Some("main"), BranchOptions::default())
            .expect("Failed to create branch");

        let mut tx = engine.begin_branch_transaction("test").expect("Failed to start tx");
        tx.put(b"test_key".to_vec(), b"branch_value".to_vec()).expect("Failed to put in branch");
        tx.commit().expect("Failed to commit");

        // Both modes should show same branch isolation behavior
        let main_val = engine.get(&b"test_key".to_vec()).expect("Failed to get");
        assert_eq!(main_val, Some(b"memory_value".to_vec()), "Main branch isolation should match");

        let tx = engine.begin_branch_transaction("test").expect("Failed to start tx");
        let branch_val = tx.get(&b"test_key".to_vec()).expect("Failed to get");
        assert_eq!(branch_val, Some(b"branch_value".to_vec()), "Branch isolation should match");
    }
}

#[test]
fn test_multiple_sequential_operations_in_memory() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).expect("Failed to open engine");

    // Perform multiple sequential operations
    for iteration in 0..10 {
        let branch_name = format!("branch_{}", iteration);

        engine.create_branch(&branch_name, Some("main"), BranchOptions::default())
            .expect(&format!("Failed to create {}", branch_name));

        let mut tx = engine.begin_branch_transaction(&branch_name)
            .expect(&format!("Failed to start tx for {}", branch_name));

        tx.put(
            format!("iter_{}", iteration).into_bytes(),
            format!("value_{}", iteration).into_bytes()
        ).expect(&format!("Failed to put in {}", branch_name));

        tx.commit().expect(&format!("Failed to commit {}", branch_name));
    }

    // Verify all branches still exist and have correct data
    let branches = engine.list_branches().expect("Failed to list branches");
    assert_eq!(branches.len(), 11, "Should have main + 10 branches");

    for iteration in 0..10 {
        let branch_name = format!("branch_{}", iteration);
        let tx = engine.begin_branch_transaction(&branch_name)
            .expect(&format!("Failed to start read tx for {}", branch_name));

        let value = tx.get(&format!("iter_{}", iteration).into_bytes())
            .expect(&format!("Failed to get from {}", branch_name));

        assert_eq!(
            value,
            Some(format!("value_{}", iteration).into_bytes()),
            "Iteration {} should have correct value", iteration
        );
    }
}

#[test]
fn test_branch_write_does_not_leak_to_main_in_memory() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).expect("Failed to open in-memory engine");

    // Write to main
    engine.put(&b"shared_key".to_vec(), b"main_value").expect("Failed to write to main");

    // Create a branch
    engine.create_branch("test_branch", Some("main"), BranchOptions::default())
        .expect("Failed to create branch");

    // Write to the branch using BranchTransaction
    {
        let mut tx = engine.begin_branch_transaction("test_branch")
            .expect("Failed to start branch transaction");

        tx.put(b"shared_key".to_vec(), b"branch_value".to_vec())
            .expect("Failed to write to branch");

        tx.commit().expect("Failed to commit branch transaction");
    }

    // CRITICAL TEST: Verify main branch data is NOT changed
    let main_value = engine.get(&b"shared_key".to_vec())
        .expect("Failed to read from main");
    assert_eq!(
        main_value,
        Some(b"main_value".to_vec()),
        "ISOLATION VIOLATION: Branch write modified main branch data!"
    );

    // Verify branch has the new value
    {
        let tx = engine.begin_branch_transaction("test_branch")
            .expect("Failed to start read transaction");
        let branch_value = tx.get(&b"shared_key".to_vec())
            .expect("Failed to read from branch");
        assert_eq!(
            branch_value,
            Some(b"branch_value".to_vec()),
            "Branch should have the updated value"
        );
    }
}

#[test]
fn test_multiple_branches_no_cross_contamination_in_memory() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).expect("Failed to open in-memory engine");

    // Create two branches
    engine.create_branch("branch_a", Some("main"), BranchOptions::default())
        .expect("Failed to create branch_a");
    engine.create_branch("branch_b", Some("main"), BranchOptions::default())
        .expect("Failed to create branch_b");

    // Write different values to each branch for the same key
    {
        let mut tx_a = engine.begin_branch_transaction("branch_a")
            .expect("Failed to start transaction for branch_a");
        tx_a.put(b"test_key".to_vec(), b"value_from_a".to_vec())
            .expect("Failed to write to branch_a");
        tx_a.commit().expect("Failed to commit branch_a");
    }

    {
        let mut tx_b = engine.begin_branch_transaction("branch_b")
            .expect("Failed to start transaction for branch_b");
        tx_b.put(b"test_key".to_vec(), b"value_from_b".to_vec())
            .expect("Failed to write to branch_b");
        tx_b.commit().expect("Failed to commit branch_b");
    }

    // Verify each branch has its own isolated value
    {
        let tx_a = engine.begin_branch_transaction("branch_a")
            .expect("Failed to read from branch_a");
        let value_a = tx_a.get(&b"test_key".to_vec())
            .expect("Failed to get test_key from branch_a");
        assert_eq!(
            value_a,
            Some(b"value_from_a".to_vec()),
            "branch_a should have its own value"
        );
    }

    {
        let tx_b = engine.begin_branch_transaction("branch_b")
            .expect("Failed to read from branch_b");
        let value_b = tx_b.get(&b"test_key".to_vec())
            .expect("Failed to get test_key from branch_b");
        assert_eq!(
            value_b,
            Some(b"value_from_b".to_vec()),
            "branch_b should have its own value"
        );
    }

    // Verify main is unchanged
    let main_value = engine.get(&b"test_key".to_vec())
        .expect("Failed to read from main");
    assert_eq!(
        main_value,
        None,
        "main should not have test_key (it was never written to main)"
    );
}
