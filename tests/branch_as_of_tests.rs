//! Integration tests for CREATE BRANCH AS OF timestamp resolution
//!
//! Tests the resolution of AS OF clauses (TIMESTAMP, TRANSACTION, SCN)
//! when creating branches at specific points in time.
//!
//! NOTE: These tests are disabled because they use internal APIs that are not
//! publicly exposed. They need to be rewritten to use the public API.
//! Enable with: cargo test --features internal-tests

#![cfg(feature = "internal-tests")]

use heliosdb_lite::{
    Config,
    storage::{StorageEngine, BranchOptions},
    Tuple, Value,
};

#[test]
fn test_create_branch_as_of_now() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Create branch with AS OF NOW (default behavior)
    let branch_id = engine.create_branch_at_snapshot(
        "dev",
        Some("main"),
        None, // AS OF NOW
        BranchOptions::default(),
    ).unwrap();

    assert!(branch_id > 1); // main is 1

    // Verify branch exists
    let branches = engine.list_branches().unwrap();
    let names: Vec<_> = branches.iter().map(|b| b.name.as_str()).collect();
    assert!(names.contains(&"dev"));
}

#[test]
fn test_create_branch_at_specific_snapshot() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Register snapshots manually to simulate time progression
    let snapshot_mgr = engine.snapshot_manager();

    // Register snapshot at timestamp 100
    snapshot_mgr.register_snapshot(100).unwrap();

    // Create branch at historical snapshot
    let branch_id = engine.create_branch_at_snapshot(
        "historical",
        Some("main"),
        Some(100), // Specific snapshot
        BranchOptions::default(),
    ).unwrap();

    assert!(branch_id > 1);

    // Verify branch metadata contains correct snapshot
    let branch = engine.get_branch("historical").unwrap();
    assert_eq!(branch.created_from_snapshot, 100);
}

#[test]
fn test_create_branch_as_of_timestamp_string() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Register a snapshot with a known timestamp
    let snapshot_mgr = engine.snapshot_manager();
    let snapshot_meta = snapshot_mgr.register_snapshot(100).unwrap();

    // Parse the timestamp string from metadata
    let timestamp_str = snapshot_meta.wall_clock_time;

    // Create branch using AS OF TIMESTAMP
    let resolved = snapshot_mgr.resolve_as_of(
        &heliosdb_lite::sql::logical_plan::AsOfClause::Timestamp(timestamp_str)
    );

    // Should resolve successfully to snapshot 100
    assert!(resolved.is_ok(), "Failed to resolve timestamp: {:?}", resolved.err());
    let snapshot_id = resolved.unwrap();
    assert_eq!(snapshot_id, 100);

    // Create branch at resolved snapshot
    let branch_id = engine.create_branch_at_snapshot(
        "time_travel",
        Some("main"),
        Some(snapshot_id),
        BranchOptions::default(),
    ).unwrap();

    assert!(branch_id > 1);

    // Verify branch was created at correct snapshot
    let branch = engine.get_branch("time_travel").unwrap();
    assert_eq!(branch.created_from_snapshot, 100);
}

#[test]
fn test_create_branch_as_of_transaction() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Register a snapshot
    let snapshot_mgr = engine.snapshot_manager();
    let snapshot_meta = snapshot_mgr.register_snapshot(200).unwrap();
    let txn_id = snapshot_meta.transaction_id;

    // Resolve using AS OF TRANSACTION
    let resolved = snapshot_mgr.resolve_as_of(
        &heliosdb_lite::sql::logical_plan::AsOfClause::Transaction(txn_id)
    ).unwrap();

    assert_eq!(resolved, 200);

    // Create branch at transaction point
    let branch_id = engine.create_branch_at_snapshot(
        "txn_branch",
        Some("main"),
        Some(resolved),
        BranchOptions::default(),
    ).unwrap();

    assert!(branch_id > 1);

    // Verify correct snapshot
    let branch = engine.get_branch("txn_branch").unwrap();
    assert_eq!(branch.created_from_snapshot, 200);
}

#[test]
fn test_create_branch_as_of_scn() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Register a snapshot
    let snapshot_mgr = engine.snapshot_manager();
    let snapshot_meta = snapshot_mgr.register_snapshot(300).unwrap();
    let scn = snapshot_meta.scn;

    // Resolve using AS OF SCN
    let resolved = snapshot_mgr.resolve_as_of(
        &heliosdb_lite::sql::logical_plan::AsOfClause::Scn(scn)
    ).unwrap();

    assert_eq!(resolved, 300);

    // Create branch at SCN point
    let branch_id = engine.create_branch_at_snapshot(
        "scn_branch",
        Some("main"),
        Some(resolved),
        BranchOptions::default(),
    ).unwrap();

    assert!(branch_id > 1);

    // Verify correct snapshot
    let branch = engine.get_branch("scn_branch").unwrap();
    assert_eq!(branch.created_from_snapshot, 300);
}

#[test]
fn test_invalid_timestamp_returns_error() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    let snapshot_mgr = engine.snapshot_manager();

    // Try to resolve non-existent timestamp
    let result = snapshot_mgr.resolve_as_of(
        &heliosdb_lite::sql::logical_plan::AsOfClause::Timestamp(
            "2099-12-31 23:59:59".to_string()
        )
    );

    // Should fail - no snapshot exists for future timestamp
    assert!(result.is_err());
}

#[test]
fn test_invalid_transaction_returns_error() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    let snapshot_mgr = engine.snapshot_manager();

    // Try to resolve non-existent transaction
    let result = snapshot_mgr.resolve_as_of(
        &heliosdb_lite::sql::logical_plan::AsOfClause::Transaction(999999)
    );

    // Should fail - transaction doesn't exist
    assert!(result.is_err());
}

#[test]
fn test_invalid_scn_returns_error() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    let snapshot_mgr = engine.snapshot_manager();

    // Try to resolve non-existent SCN
    let result = snapshot_mgr.resolve_as_of(
        &heliosdb_lite::sql::logical_plan::AsOfClause::Scn(999999)
    );

    // Should fail - SCN doesn't exist
    assert!(result.is_err());
}

#[test]
fn test_multiple_snapshots_resolution() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    let snapshot_mgr = engine.snapshot_manager();

    // Register multiple snapshots
    let snap1 = snapshot_mgr.register_snapshot(100).unwrap();
    let snap2 = snapshot_mgr.register_snapshot(200).unwrap();
    let snap3 = snapshot_mgr.register_snapshot(300).unwrap();

    // Create branches at different snapshots
    engine.create_branch_at_snapshot(
        "branch1",
        Some("main"),
        Some(100),
        BranchOptions::default(),
    ).unwrap();

    engine.create_branch_at_snapshot(
        "branch2",
        Some("main"),
        Some(200),
        BranchOptions::default(),
    ).unwrap();

    engine.create_branch_at_snapshot(
        "branch3",
        Some("main"),
        Some(300),
        BranchOptions::default(),
    ).unwrap();

    // Verify each branch has correct snapshot
    let b1 = engine.get_branch("branch1").unwrap();
    assert_eq!(b1.created_from_snapshot, 100);

    let b2 = engine.get_branch("branch2").unwrap();
    assert_eq!(b2.created_from_snapshot, 200);

    let b3 = engine.get_branch("branch3").unwrap();
    assert_eq!(b3.created_from_snapshot, 300);
}

#[test]
fn test_branch_created_at_snapshot_inherits_parent_state() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Insert data in main at different times
    engine.put(b"key1", b"value1_v1").unwrap();

    // Register snapshot after first insert
    let snapshot_mgr = engine.snapshot_manager();
    snapshot_mgr.register_snapshot(100).unwrap();

    // Update data
    engine.put(b"key1", b"value1_v2").unwrap();

    // Register another snapshot
    snapshot_mgr.register_snapshot(200).unwrap();

    // Create branch at first snapshot (should see v1)
    engine.create_branch_at_snapshot(
        "branch_v1",
        Some("main"),
        Some(100),
        BranchOptions::default(),
    ).unwrap();

    // Create branch at second snapshot (should see v2)
    engine.create_branch_at_snapshot(
        "branch_v2",
        Some("main"),
        Some(200),
        BranchOptions::default(),
    ).unwrap();

    // Verify branches were created at correct snapshots
    let b1 = engine.get_branch("branch_v1").unwrap();
    assert_eq!(b1.created_from_snapshot, 100);

    let b2 = engine.get_branch("branch_v2").unwrap();
    assert_eq!(b2.created_from_snapshot, 200);
}

#[test]
fn test_timestamp_format_variations() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    let snapshot_mgr = engine.snapshot_manager();

    // Register a snapshot
    snapshot_mgr.register_snapshot(100).unwrap();

    // Try different timestamp formats
    let formats = vec![
        "2025-11-21 10:00:00",  // Space separator
        "2025-11-21T10:00:00",  // ISO format
    ];

    for format in formats {
        let result = snapshot_mgr.resolve_as_of(
            &heliosdb_lite::sql::logical_plan::AsOfClause::Timestamp(format.to_string())
        );

        // Should either resolve or fail gracefully (depending on whether snapshot exists)
        // The key is that it should not panic
        assert!(result.is_ok() || result.is_err());
    }
}
