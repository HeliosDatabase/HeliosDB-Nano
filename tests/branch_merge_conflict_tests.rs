//! Comprehensive tests for database branch merging with conflict resolution
//!
//! Tests all merge strategies:
//! - Auto (prefer source on conflict)
//! - Manual (fail on conflict)
//! - Theirs (always prefer source)
//! - Ours (always prefer target)
//!
//! NOTE: These tests are disabled because they use internal APIs that are not
//! publicly exposed. They need to be rewritten to use the public API.
//! Enable with: cargo test --features internal-tests

#![cfg(feature = "internal-tests")]

use heliosdb_nano::{
    Config,
    storage::{StorageEngine, BranchOptions, MergeStrategy},
};

#[test]
fn test_merge_no_conflicts() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Setup: main has key1, dev adds key2
    engine.put(b"key1", b"value1_main").unwrap();

    // Create dev branch
    engine.create_branch("dev", Some("main"), BranchOptions::default()).unwrap();

    // Add key2 in dev
    let mut tx = engine.begin_branch_transaction("dev").unwrap();
    tx.put(b"key2".to_vec(), b"value2_dev".to_vec()).unwrap();
    tx.commit().unwrap();

    // Merge dev into main (no conflicts)
    let result = engine.merge_branch("dev", "main", MergeStrategy::Auto).unwrap();

    assert!(result.completed);
    assert_eq!(result.conflicts.len(), 0);
    assert_eq!(result.merged_keys, 1); // Only key2 was merged

    // Verify main now has both keys
    assert_eq!(engine.get(b"key1").unwrap(), Some(b"value1_main".to_vec()));
    assert_eq!(engine.get(b"key2").unwrap(), Some(b"value2_dev".to_vec()));
}

#[test]
fn test_merge_with_conflict_auto_strategy() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Setup: both branches modify same key
    engine.put(b"key1", b"original").unwrap();

    // Create dev branch
    engine.create_branch("dev", Some("main"), BranchOptions::default()).unwrap();

    // Modify key1 in dev
    let mut tx_dev = engine.begin_branch_transaction("dev").unwrap();
    tx_dev.put(b"key1".to_vec(), b"dev_value".to_vec()).unwrap();
    tx_dev.commit().unwrap();

    // Modify key1 in main
    engine.put(b"key1", b"main_value").unwrap();

    // Merge with Auto strategy (should prefer dev/source)
    let result = engine.merge_branch("dev", "main", MergeStrategy::Auto).unwrap();

    assert!(result.completed);
    assert_eq!(result.conflicts.len(), 1); // Conflict detected but resolved

    // Auto strategy should have used dev's value
    assert_eq!(engine.get(b"key1").unwrap(), Some(b"dev_value".to_vec()));
}

#[test]
fn test_merge_with_conflict_manual_strategy() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Setup: both branches modify same key
    engine.put(b"key1", b"original").unwrap();

    engine.create_branch("dev", Some("main"), BranchOptions::default()).unwrap();

    // Modify key1 in dev
    let mut tx_dev = engine.begin_branch_transaction("dev").unwrap();
    tx_dev.put(b"key1".to_vec(), b"dev_value".to_vec()).unwrap();
    tx_dev.commit().unwrap();

    // Modify key1 in main
    engine.put(b"key1", b"main_value").unwrap();

    // Merge with Manual strategy (should fail on conflict)
    let result = engine.merge_branch("dev", "main", MergeStrategy::Manual).unwrap();

    assert!(!result.completed); // Merge not completed due to conflict
    assert_eq!(result.conflicts.len(), 1);

    // Main should be unchanged
    assert_eq!(engine.get(b"key1").unwrap(), Some(b"main_value".to_vec()));
}

#[test]
fn test_merge_with_conflict_theirs_strategy() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Setup: both branches modify same key
    engine.put(b"key1", b"original").unwrap();

    engine.create_branch("dev", Some("main"), BranchOptions::default()).unwrap();

    // Modify key1 in dev
    let mut tx_dev = engine.begin_branch_transaction("dev").unwrap();
    tx_dev.put(b"key1".to_vec(), b"dev_value".to_vec()).unwrap();
    tx_dev.commit().unwrap();

    // Modify key1 in main
    engine.put(b"key1", b"main_value").unwrap();

    // Merge with Theirs strategy (always prefer source/dev)
    let result = engine.merge_branch("dev", "main", MergeStrategy::Theirs).unwrap();

    assert!(result.completed);
    assert_eq!(result.merged_keys, 1);

    // Should use dev's value
    assert_eq!(engine.get(b"key1").unwrap(), Some(b"dev_value".to_vec()));
}

#[test]
fn test_merge_with_conflict_ours_strategy() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Setup: both branches modify same key
    engine.put(b"key1", b"original").unwrap();

    engine.create_branch("dev", Some("main"), BranchOptions::default()).unwrap();

    // Modify key1 in dev
    let mut tx_dev = engine.begin_branch_transaction("dev").unwrap();
    tx_dev.put(b"key1".to_vec(), b"dev_value".to_vec()).unwrap();
    tx_dev.commit().unwrap();

    // Modify key1 in main
    engine.put(b"key1", b"main_value").unwrap();

    // Merge with Ours strategy (always prefer target/main)
    let result = engine.merge_branch("dev", "main", MergeStrategy::Ours).unwrap();

    assert!(result.completed);

    // Should keep main's value
    assert_eq!(engine.get(b"key1").unwrap(), Some(b"main_value".to_vec()));
}

#[test]
fn test_merge_multiple_conflicts() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Setup: multiple conflicting keys
    for i in 0..10 {
        let key = format!("key{}", i).into_bytes();
        engine.put(&key, b"original").unwrap();
    }

    engine.create_branch("dev", Some("main"), BranchOptions::default()).unwrap();

    // Modify all keys in dev
    let mut tx_dev = engine.begin_branch_transaction("dev").unwrap();
    for i in 0..10 {
        let key = format!("key{}", i).into_bytes();
        tx_dev.put(key, b"dev_modified".to_vec()).unwrap();
    }
    tx_dev.commit().unwrap();

    // Modify all keys in main
    for i in 0..10 {
        let key = format!("key{}", i).into_bytes();
        engine.put(&key, b"main_modified").unwrap();
    }

    // Merge with Manual strategy (should detect all 10 conflicts)
    let result = engine.merge_branch("dev", "main", MergeStrategy::Manual).unwrap();

    assert!(!result.completed);
    assert_eq!(result.conflicts.len(), 10);

    // All keys should still have main's values
    for i in 0..10 {
        let key = format!("key{}", i).into_bytes();
        assert_eq!(engine.get(&key).unwrap(), Some(b"main_modified".to_vec()));
    }
}

#[test]
fn test_merge_with_deletions() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Setup: main has key1 and key2
    engine.put(b"key1", b"value1").unwrap();
    engine.put(b"key2", b"value2").unwrap();

    engine.create_branch("dev", Some("main"), BranchOptions::default()).unwrap();

    // Delete key1 in dev
    let mut tx_dev = engine.begin_branch_transaction("dev").unwrap();
    tx_dev.delete(b"key1".to_vec()).unwrap();
    tx_dev.commit().unwrap();

    // Merge dev into main
    let result = engine.merge_branch("dev", "main", MergeStrategy::Auto).unwrap();

    assert!(result.completed);

    // key1 should be deleted, key2 should remain
    assert_eq!(engine.get(b"key1").unwrap(), None);
    assert_eq!(engine.get(b"key2").unwrap(), Some(b"value2".to_vec()));
}

#[test]
fn test_merge_branch_state_update() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    engine.create_branch("dev", Some("main"), BranchOptions::default()).unwrap();

    // Verify dev is Active
    let dev_before = engine.get_branch("dev").unwrap();
    assert!(matches!(dev_before.state, heliosdb_nano::storage::BranchState::Active));

    // Merge dev into main
    engine.merge_branch("dev", "main", MergeStrategy::Auto).unwrap();

    // Verify dev is now Merged
    let dev_after = engine.get_branch("dev").unwrap();
    assert!(matches!(
        dev_after.state,
        heliosdb_nano::storage::BranchState::Merged { .. }
    ));
}

#[test]
fn test_cannot_merge_inactive_branch() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Create and drop a branch
    engine.create_branch("temp", Some("main"), BranchOptions::default()).unwrap();
    engine.drop_branch("temp", false).unwrap();

    // Try to merge dropped branch
    let result = engine.merge_branch("temp", "main", MergeStrategy::Auto);

    assert!(result.is_err());
}

#[test]
fn test_merge_preserves_non_conflicting_changes() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Setup: shared key and branch-specific keys
    engine.put(b"shared", b"original").unwrap();

    engine.create_branch("dev", Some("main"), BranchOptions::default()).unwrap();

    // Dev: modify shared + add dev_key
    let mut tx_dev = engine.begin_branch_transaction("dev").unwrap();
    tx_dev.put(b"shared".to_vec(), b"dev_value".to_vec()).unwrap();
    tx_dev.put(b"dev_key".to_vec(), b"dev_only".to_vec()).unwrap();
    tx_dev.commit().unwrap();

    // Main: modify shared + add main_key
    engine.put(b"shared", b"main_value").unwrap();
    engine.put(b"main_key", b"main_only").unwrap();

    // Merge with Theirs strategy
    let result = engine.merge_branch("dev", "main", MergeStrategy::Theirs).unwrap();

    assert!(result.completed);

    // Verify all keys present with correct values
    assert_eq!(engine.get(b"shared").unwrap(), Some(b"dev_value".to_vec())); // Conflict resolved with dev's value
    assert_eq!(engine.get(b"dev_key").unwrap(), Some(b"dev_only".to_vec())); // Dev's unique key merged
    assert_eq!(engine.get(b"main_key").unwrap(), Some(b"main_only".to_vec())); // Main's unique key preserved
}

#[test]
fn test_merge_same_change_no_conflict() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Setup
    engine.put(b"key1", b"original").unwrap();

    engine.create_branch("dev", Some("main"), BranchOptions::default()).unwrap();

    // Both branches make same change
    let mut tx_dev = engine.begin_branch_transaction("dev").unwrap();
    tx_dev.put(b"key1".to_vec(), b"same_value".to_vec()).unwrap();
    tx_dev.commit().unwrap();

    engine.put(b"key1", b"same_value").unwrap();

    // Merge should succeed with no conflicts (same change)
    let result = engine.merge_branch("dev", "main", MergeStrategy::Manual).unwrap();

    assert!(result.completed);
    assert_eq!(result.conflicts.len(), 0); // No conflict because values are identical
}

#[test]
fn test_merge_conflict_metadata() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Setup conflict
    engine.put(b"key1", b"base").unwrap();

    engine.create_branch("dev", Some("main"), BranchOptions::default()).unwrap();

    let mut tx_dev = engine.begin_branch_transaction("dev").unwrap();
    tx_dev.put(b"key1".to_vec(), b"dev_value".to_vec()).unwrap();
    tx_dev.commit().unwrap();

    engine.put(b"key1", b"main_value").unwrap();

    // Merge with Manual to inspect conflict metadata
    let result = engine.merge_branch("dev", "main", MergeStrategy::Manual).unwrap();

    assert_eq!(result.conflicts.len(), 1);

    let conflict = &result.conflicts[0];
    assert_eq!(conflict.key, "key1");
    assert_eq!(conflict.source_value, Some(b"dev_value".to_vec()));
    assert_eq!(conflict.target_value, Some(b"main_value".to_vec()));
    assert!(conflict.source_timestamp > 0);
    assert!(conflict.target_timestamp > 0);
}

#[test]
fn test_merge_large_dataset() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Create 1000 keys in main
    for i in 0..1000 {
        let key = format!("key{}", i).into_bytes();
        let value = format!("main_value_{}", i).into_bytes();
        engine.put(&key, &value).unwrap();
    }

    engine.create_branch("dev", Some("main"), BranchOptions::default()).unwrap();

    // Modify 100 keys in dev, add 100 new keys
    let mut tx_dev = engine.begin_branch_transaction("dev").unwrap();
    for i in 0..100 {
        let key = format!("key{}", i).into_bytes();
        let value = format!("dev_modified_{}", i).into_bytes();
        tx_dev.put(key, value).unwrap();
    }
    for i in 1000..1100 {
        let key = format!("key{}", i).into_bytes();
        let value = format!("dev_new_{}", i).into_bytes();
        tx_dev.put(key, value).unwrap();
    }
    tx_dev.commit().unwrap();

    // Merge with Theirs strategy
    let start = std::time::Instant::now();
    let result = engine.merge_branch("dev", "main", MergeStrategy::Theirs).unwrap();
    let duration = start.elapsed();

    assert!(result.completed);
    assert_eq!(result.merged_keys, 200); // 100 modified + 100 new

    // Merge should complete in reasonable time (< 1 second for 200 keys)
    assert!(duration.as_secs() < 1, "Merge took too long: {:?}", duration);

    // Verify merged data
    for i in 0..100 {
        let key = format!("key{}", i).into_bytes();
        let expected = format!("dev_modified_{}", i).into_bytes();
        assert_eq!(engine.get(&key).unwrap(), Some(expected));
    }

    for i in 1000..1100 {
        let key = format!("key{}", i).into_bytes();
        let expected = format!("dev_new_{}", i).into_bytes();
        assert_eq!(engine.get(&key).unwrap(), Some(expected));
    }
}
