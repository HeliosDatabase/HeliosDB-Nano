//! Integration tests for branch storage
//!
//! Tests the copy-on-write branch storage backend.

use heliosdb_nano::{Config, storage::{StorageEngine, BranchOptions}};

#[test]
fn test_create_and_list_branches() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Create a branch
    let branch_id = engine.create_branch(
        "dev",
        Some("main"),
        BranchOptions::default(),
    ).unwrap();

    assert!(branch_id > 1); // main is 1

    // List branches
    let branches = engine.list_branches().unwrap();
    assert_eq!(branches.len(), 2); // main + dev

    let names: Vec<_> = branches.iter().map(|b| b.name.as_str()).collect();
    assert!(names.contains(&"main"));
    assert!(names.contains(&"dev"));
}

#[test]
fn test_branch_isolation() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Insert data in main
    engine.put(&b"key1".to_vec(), b"value_main").unwrap();

    // Create branch
    engine.create_branch("dev", Some("main"), BranchOptions::default()).unwrap();

    // Read from dev (should see main's value)
    let mut tx = engine.begin_branch_transaction("dev").unwrap();
    assert_eq!(tx.get(&b"key1".to_vec()).unwrap(), Some(b"value_main".to_vec()));

    // Write in dev
    tx.put(b"key1".to_vec(), b"value_dev".to_vec()).unwrap();
    tx.commit().unwrap();

    // Verify main is unchanged
    let value = engine.get(&b"key1".to_vec()).unwrap();
    assert_eq!(value, Some(b"value_main".to_vec()));

    // Verify dev has new value
    let tx = engine.begin_branch_transaction("dev").unwrap();
    assert_eq!(tx.get(&b"key1".to_vec()).unwrap(), Some(b"value_dev".to_vec()));
}

#[test]
fn test_copy_on_write() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Insert 1000 keys in main
    for i in 0..1000 {
        let key = format!("key{}", i).into_bytes();
        let value = format!("value{}", i).into_bytes();
        engine.put(&key, &value).unwrap();
    }

    // Create branch (should be instant)
    let start = std::time::Instant::now();
    engine.create_branch("feature", Some("main"), BranchOptions::default()).unwrap();
    let elapsed = start.elapsed();

    assert!(elapsed.as_millis() < 100, "Branch creation took too long: {:?}", elapsed);

    // Read all keys from branch (should see main's values)
    let mut tx = engine.begin_branch_transaction("feature").unwrap();
    for i in 0..1000 {
        let key = format!("key{}", i).into_bytes();
        let value = tx.get(&key).unwrap();
        assert_eq!(value, Some(format!("value{}", i).into_bytes()));
    }

    // Modify only 10 keys in branch
    for i in 0..10 {
        let key = format!("key{}", i).into_bytes();
        let value = format!("modified{}", i).into_bytes();
        tx.put(key, value).unwrap();
    }
    tx.commit().unwrap();

    // Verify branch has modified values
    let tx = engine.begin_branch_transaction("feature").unwrap();
    for i in 0..10 {
        let key = format!("key{}", i).into_bytes();
        let value = tx.get(&key).unwrap();
        assert_eq!(value, Some(format!("modified{}", i).into_bytes()));
    }

    // Verify remaining keys still come from main
    for i in 10..1000 {
        let key = format!("key{}", i).into_bytes();
        let value = tx.get(&key).unwrap();
        assert_eq!(value, Some(format!("value{}", i).into_bytes()));
    }
}

#[test]
fn test_drop_branch() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Create and drop branch
    engine.create_branch("temp", Some("main"), BranchOptions::default()).unwrap();
    engine.drop_branch("temp", false).unwrap();

    // Verify branch no longer appears in list
    let branches = engine.list_branches().unwrap();
    let names: Vec<_> = branches.iter().map(|b| b.name.as_str()).collect();
    assert!(!names.contains(&"temp"));
}

#[test]
fn test_cannot_drop_main() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    let result = engine.drop_branch("main", false);
    assert!(result.is_err());
}

#[test]
fn test_cannot_drop_with_children() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Create parent and child
    engine.create_branch("parent", Some("main"), BranchOptions::default()).unwrap();
    engine.create_branch("child", Some("parent"), BranchOptions::default()).unwrap();

    // Try to drop parent
    let result = engine.drop_branch("parent", false);
    assert!(result.is_err());
}

#[test]
fn test_branch_hierarchy() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Insert in main
    engine.put(&b"key".to_vec(), b"main_value").unwrap();

    // Create hierarchy: main -> level1 -> level2
    engine.create_branch("level1", Some("main"), BranchOptions::default()).unwrap();
    engine.create_branch("level2", Some("level1"), BranchOptions::default()).unwrap();

    // Read from level2 (should traverse to main)
    let tx = engine.begin_branch_transaction("level2").unwrap();
    assert_eq!(tx.get(&b"key".to_vec()).unwrap(), Some(b"main_value".to_vec()));

    // Modify in level1
    let mut tx1 = engine.begin_branch_transaction("level1").unwrap();
    tx1.put(b"key".to_vec(), b"level1_value".to_vec()).unwrap();
    tx1.commit().unwrap();

    // level2 should now see level1's value
    let tx2 = engine.begin_branch_transaction("level2").unwrap();
    assert_eq!(tx2.get(&b"key".to_vec()).unwrap(), Some(b"level1_value".to_vec()));

    // Modify in level2
    let mut tx2 = engine.begin_branch_transaction("level2").unwrap();
    tx2.put(b"key".to_vec(), b"level2_value".to_vec()).unwrap();
    tx2.commit().unwrap();

    // Verify isolation
    assert_eq!(engine.get(&b"key".to_vec()).unwrap(), Some(b"main_value".to_vec()));

    let tx1 = engine.begin_branch_transaction("level1").unwrap();
    assert_eq!(tx1.get(&b"key".to_vec()).unwrap(), Some(b"level1_value".to_vec()));

    let tx2 = engine.begin_branch_transaction("level2").unwrap();
    assert_eq!(tx2.get(&b"key".to_vec()).unwrap(), Some(b"level2_value".to_vec()));
}

#[test]
fn test_concurrent_branch_writes() {
    use std::sync::Arc;
    use std::thread;

    let config = Config::in_memory();
    let engine = Arc::new(StorageEngine::open_in_memory(&config).unwrap());

    // Create two branches
    engine.create_branch("branch_a", Some("main"), BranchOptions::default()).unwrap();
    engine.create_branch("branch_b", Some("main"), BranchOptions::default()).unwrap();

    let engine_a = Arc::clone(&engine);
    let engine_b = Arc::clone(&engine);

    // Concurrent writes to different branches
    let t1 = thread::spawn(move || {
        for i in 0..100 {
            let mut tx = engine_a.begin_branch_transaction("branch_a").unwrap();
            tx.put(format!("key{}", i).into_bytes(), b"value_a".to_vec()).unwrap();
            tx.commit().unwrap();
        }
    });

    let t2 = thread::spawn(move || {
        for i in 0..100 {
            let mut tx = engine_b.begin_branch_transaction("branch_b").unwrap();
            tx.put(format!("key{}", i).into_bytes(), b"value_b".to_vec()).unwrap();
            tx.commit().unwrap();
        }
    });

    t1.join().unwrap();
    t2.join().unwrap();

    // Verify isolation
    let tx_a = engine.begin_branch_transaction("branch_a").unwrap();
    assert_eq!(tx_a.get(&b"key50".to_vec()).unwrap(), Some(b"value_a".to_vec()));

    let tx_b = engine.begin_branch_transaction("branch_b").unwrap();
    assert_eq!(tx_b.get(&b"key50".to_vec()).unwrap(), Some(b"value_b".to_vec()));
}
