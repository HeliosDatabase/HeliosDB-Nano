//! Branch-merge tests against the `BranchTransaction` storage API.
//!
//! WHAT WAS REMOVED, AND WHY (#72). This file used to hold 13 tests, 9 of which
//! could never pass. They were reported as evidence that `MERGE BRANCH` was a
//! silent no-op; it is not — the SQL path merges correctly. Two separate causes:
//!
//! 1. They write via `begin_branch_transaction` / `BranchTransaction`, which has
//!    zero production callers and encodes keys as `bdata:` + 8 binary bytes,
//!    while the implementation the SQL path uses (`StorageEngine::merge_branch`)
//!    scans the ASCII `bdata:{id}:{table}:{row}` that `branch_aware_data_key`
//!    writes. The merge could not see anything they wrote.
//! 2. Six of them asserted conflict DETECTION or strategy semantics that do not
//!    exist: `StorageEngine::merge_branch` takes `_strategy` (unused) and returns
//!    a hard-coded `conflicts: Vec::new()`. Merging is last-writer-wins.
//!    `MERGE BRANCH ... WITH (conflict_resolution = ...)` now fails with an
//!    explicit "not implemented" error rather than being silently ignored.
//!
//! Three tested mechanics the real merge does support (deletions propagating,
//! rows unique to each side surviving, a large diff) and were rewritten against
//! SQL in `tests/branch_merge_surface_tests.rs`. The rest were deleted.
//!
//! The tests left here pass and cover branch state validation, not merged data.
//! Do not add merge-behaviour tests to this file — add them to
//! `branch_merge_surface_tests.rs`, which drives the paths users actually reach.
//!
//! Enable with: cargo test --features internal-tests

#![cfg(feature = "internal-tests")]

use heliosdb_nano::{
    storage::{BranchOptions, MergeStrategy, StorageEngine},
    Config,
};

#[test]
fn test_merge_with_conflict_ours_strategy() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Setup: both branches modify same key
    engine.put(&b"key1".to_vec(), b"original").unwrap();

    engine
        .create_branch("dev", Some("main"), BranchOptions::default())
        .unwrap();

    // Modify key1 in dev
    let mut tx_dev = engine.begin_branch_transaction("dev").unwrap();
    tx_dev.put(b"key1".to_vec(), b"dev_value".to_vec()).unwrap();
    tx_dev.commit().unwrap();

    // Modify key1 in main
    engine.put(&b"key1".to_vec(), b"main_value").unwrap();

    // Merge with Ours strategy (always prefer target/main)
    let result = engine.merge_branch("dev", "main", MergeStrategy::Ours).unwrap();

    assert!(result.completed);

    // Should keep main's value
    assert_eq!(engine.get(&b"key1".to_vec()).unwrap(), Some(b"main_value".to_vec()));
}

#[test]
fn test_merge_branch_state_update() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    engine
        .create_branch("dev", Some("main"), BranchOptions::default())
        .unwrap();

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
    engine
        .create_branch("temp", Some("main"), BranchOptions::default())
        .unwrap();
    engine.drop_branch("temp", false).unwrap();

    // Try to merge dropped branch
    let result = engine.merge_branch("temp", "main", MergeStrategy::Auto);

    assert!(result.is_err());
}

#[test]
fn test_merge_same_change_no_conflict() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Setup
    engine.put(&b"key1".to_vec(), b"original").unwrap();

    engine
        .create_branch("dev", Some("main"), BranchOptions::default())
        .unwrap();

    // Both branches make same change
    let mut tx_dev = engine.begin_branch_transaction("dev").unwrap();
    tx_dev.put(b"key1".to_vec(), b"same_value".to_vec()).unwrap();
    tx_dev.commit().unwrap();

    engine.put(&b"key1".to_vec(), b"same_value").unwrap();

    // Merge should succeed with no conflicts (same change)
    let result = engine.merge_branch("dev", "main", MergeStrategy::Manual).unwrap();

    assert!(result.completed);
    assert_eq!(result.conflicts.len(), 0); // No conflict because values are identical
}
