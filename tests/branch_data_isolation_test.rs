//! Comprehensive tests for branch data isolation
//!
//! Tests branch data isolation across both persistent and in-memory storage modes.
//! Ensures metadata properties and data isolation work correctly regardless of storage mode.

use heliosdb_nano::{
    storage::{ArtIndexManager, BranchOptions, StorageEngine},
    Config, EmbeddedDatabase, Tuple, Value,
};
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
        engine
            .put(&b"key1".to_vec(), b"main_value")
            .expect("Failed to put in main");

        // Create branch
        let branch_id = engine
            .create_branch("dev", Some("main"), BranchOptions::default())
            .expect("Failed to create branch");

        assert!(branch_id > 1, "Branch ID should be > 1");

        // List branches to verify creation
        let branches = engine.list_branches().expect("Failed to list branches");
        assert_eq!(branches.len(), 2, "Should have main and dev branches");

        // Verify branch names
        let names: Vec<_> = branches.iter().map(|b| b.name.as_str()).collect();
        assert!(names.contains(&"main"), "Should contain main branch");
        assert!(names.contains(&"dev"), "Should contain dev branch");

        // Write data in the dev branch
        let mut tx = engine
            .begin_branch_transaction("dev")
            .expect("Failed to start transaction");
        tx.put(b"key1".to_vec(), b"dev_value".to_vec())
            .expect("Failed to put in dev");
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
        assert_eq!(
            main_value,
            Some(b"main_value".to_vec()),
            "Main branch data should be unchanged"
        );

        let tx = engine
            .begin_branch_transaction("dev")
            .expect("Failed to start transaction");
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
        engine
            .put(&b"data".to_vec(), b"main_data")
            .expect("Failed to put in main");

        engine
            .create_branch("feature", Some("main"), BranchOptions::default())
            .expect("Failed to create feature branch");

        engine
            .create_branch("feature-sub", Some("feature"), BranchOptions::default())
            .expect("Failed to create feature-sub branch");

        // Modify at each level
        let mut tx_feature = engine
            .begin_branch_transaction("feature")
            .expect("Failed to start feature transaction");
        tx_feature
            .put(b"data".to_vec(), b"feature_data".to_vec())
            .expect("Failed to put in feature");
        tx_feature.commit().expect("Failed to commit feature transaction");

        let mut tx_sub = engine
            .begin_branch_transaction("feature-sub")
            .expect("Failed to start sub transaction");
        tx_sub
            .put(b"data".to_vec(), b"sub_data".to_vec())
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

        let tx_feature = engine
            .begin_branch_transaction("feature")
            .expect("Failed to start feature transaction");
        let feature_val = tx_feature.get(&b"data".to_vec()).expect("Failed to get feature");
        assert_eq!(
            feature_val,
            Some(b"feature_data".to_vec()),
            "Feature data should persist"
        );

        let tx_sub = engine
            .begin_branch_transaction("feature-sub")
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
    engine
        .put(&b"key1".to_vec(), b"main_value")
        .expect("Failed to put in main");

    // Create multiple branches
    let branch_ids: Vec<_> = (0..5)
        .map(|i| {
            let branch_name = format!("branch_{}", i);
            engine
                .create_branch(&branch_name, Some("main"), BranchOptions::default())
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
        let mut tx = engine
            .begin_branch_transaction(&branch_name)
            .expect(&format!("Failed to start transaction for {}", branch_name));
        let value = format!("branch_{}_value", i);
        tx.put(b"key1".to_vec(), value.into_bytes())
            .expect(&format!("Failed to put in {}", branch_name));
        tx.commit().expect(&format!("Failed to commit {}", branch_name));
    }

    // Verify isolation
    let main_val = engine.get(&b"key1".to_vec()).expect("Failed to get from main");
    assert_eq!(
        main_val,
        Some(b"main_value".to_vec()),
        "Main branch should be unchanged"
    );

    for i in 0..5 {
        let branch_name = format!("branch_{}", i);
        let tx = engine
            .begin_branch_transaction(&branch_name)
            .expect(&format!("Failed to start transaction for {}", branch_name));
        let value = tx
            .get(&b"key1".to_vec())
            .expect(&format!("Failed to get from {}", branch_name));
        let expected = format!("branch_{}_value", i);
        assert_eq!(
            value,
            Some(expected.into_bytes()),
            "Branch {} should have isolated value",
            i
        );
    }
}

#[test]
fn test_in_memory_branch_metadata_properties() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).expect("Failed to open in-memory engine");

    // Create a branch with options
    let options = BranchOptions::default();
    let branch_id = engine
        .create_branch("test_branch", Some("main"), options)
        .expect("Failed to create branch");

    // Verify branch metadata is retrievable
    let branches = engine.list_branches().expect("Failed to list branches");
    let branch = branches
        .iter()
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
    let engine = Arc::new(StorageEngine::open_in_memory(&config).expect("Failed to open in-memory engine"));

    // Create branches for each thread
    for i in 0..3 {
        engine
            .create_branch(&format!("branch_{}", i), Some("main"), BranchOptions::default())
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
                let mut tx = engine_clone
                    .begin_branch_transaction(&branch_name)
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
        let tx = engine
            .begin_branch_transaction(&branch_name)
            .expect(&format!("Failed to start transaction"));

        for j in 0..50 {
            let key = format!("key{}", j).into_bytes();
            let expected = format!("thread{}_value{}", i, j).into_bytes();
            let value = tx.get(&key).expect(&format!("Failed to get key"));
            assert_eq!(
                value,
                Some(expected),
                "Branch {} key {} should have correct isolated value",
                i,
                j
            );
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
    engine
        .create_branch("feature", Some("main"), BranchOptions::default())
        .expect("Failed to create branch");

    // Modify a subset of keys in branch
    {
        let mut tx = engine
            .begin_branch_transaction("feature")
            .expect("Failed to start transaction");

        for i in 0..100 {
            let key = format!("key{}", i).into_bytes();
            let value = format!("feature_value{}", i).into_bytes();
            tx.put(key, value).expect("Failed to put in branch");
        }

        tx.commit().expect("Failed to commit transaction");
    }

    // Verify isolation at scale
    let tx = engine
        .begin_branch_transaction("feature")
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

        engine
            .put(&b"test_key".to_vec(), b"persistent_value")
            .expect("Failed to put");

        engine
            .create_branch("test", Some("main"), BranchOptions::default())
            .expect("Failed to create branch");

        let mut tx = engine.begin_branch_transaction("test").expect("Failed to start tx");
        tx.put(b"test_key".to_vec(), b"branch_value".to_vec())
            .expect("Failed to put in branch");
        tx.commit().expect("Failed to commit");
    }

    // Test in-memory mode with same operations
    {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).expect("Failed to open in-memory engine");

        engine
            .put(&b"test_key".to_vec(), b"memory_value")
            .expect("Failed to put");

        engine
            .create_branch("test", Some("main"), BranchOptions::default())
            .expect("Failed to create branch");

        let mut tx = engine.begin_branch_transaction("test").expect("Failed to start tx");
        tx.put(b"test_key".to_vec(), b"branch_value".to_vec())
            .expect("Failed to put in branch");
        tx.commit().expect("Failed to commit");

        // Both modes should show same branch isolation behavior
        let main_val = engine.get(&b"test_key".to_vec()).expect("Failed to get");
        assert_eq!(
            main_val,
            Some(b"memory_value".to_vec()),
            "Main branch isolation should match"
        );

        let tx = engine.begin_branch_transaction("test").expect("Failed to start tx");
        let branch_val = tx.get(&b"test_key".to_vec()).expect("Failed to get");
        assert_eq!(
            branch_val,
            Some(b"branch_value".to_vec()),
            "Branch isolation should match"
        );
    }
}

#[test]
fn test_multiple_sequential_operations_in_memory() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).expect("Failed to open engine");

    // Perform multiple sequential operations
    for iteration in 0..10 {
        let branch_name = format!("branch_{}", iteration);

        engine
            .create_branch(&branch_name, Some("main"), BranchOptions::default())
            .expect(&format!("Failed to create {}", branch_name));

        let mut tx = engine
            .begin_branch_transaction(&branch_name)
            .expect(&format!("Failed to start tx for {}", branch_name));

        tx.put(
            format!("iter_{}", iteration).into_bytes(),
            format!("value_{}", iteration).into_bytes(),
        )
        .expect(&format!("Failed to put in {}", branch_name));

        tx.commit().expect(&format!("Failed to commit {}", branch_name));
    }

    // Verify all branches still exist and have correct data
    let branches = engine.list_branches().expect("Failed to list branches");
    assert_eq!(branches.len(), 11, "Should have main + 10 branches");

    for iteration in 0..10 {
        let branch_name = format!("branch_{}", iteration);
        let tx = engine
            .begin_branch_transaction(&branch_name)
            .expect(&format!("Failed to start read tx for {}", branch_name));

        let value = tx
            .get(&format!("iter_{}", iteration).into_bytes())
            .expect(&format!("Failed to get from {}", branch_name));

        assert_eq!(
            value,
            Some(format!("value_{}", iteration).into_bytes()),
            "Iteration {} should have correct value",
            iteration
        );
    }
}

#[test]
fn test_branch_write_does_not_leak_to_main_in_memory() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).expect("Failed to open in-memory engine");

    // Write to main
    engine
        .put(&b"shared_key".to_vec(), b"main_value")
        .expect("Failed to write to main");

    // Create a branch
    engine
        .create_branch("test_branch", Some("main"), BranchOptions::default())
        .expect("Failed to create branch");

    // Write to the branch using BranchTransaction
    {
        let mut tx = engine
            .begin_branch_transaction("test_branch")
            .expect("Failed to start branch transaction");

        tx.put(b"shared_key".to_vec(), b"branch_value".to_vec())
            .expect("Failed to write to branch");

        tx.commit().expect("Failed to commit branch transaction");
    }

    // CRITICAL TEST: Verify main branch data is NOT changed
    let main_value = engine.get(&b"shared_key".to_vec()).expect("Failed to read from main");
    assert_eq!(
        main_value,
        Some(b"main_value".to_vec()),
        "ISOLATION VIOLATION: Branch write modified main branch data!"
    );

    // Verify branch has the new value
    {
        let tx = engine
            .begin_branch_transaction("test_branch")
            .expect("Failed to start read transaction");
        let branch_value = tx.get(&b"shared_key".to_vec()).expect("Failed to read from branch");
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
    engine
        .create_branch("branch_a", Some("main"), BranchOptions::default())
        .expect("Failed to create branch_a");
    engine
        .create_branch("branch_b", Some("main"), BranchOptions::default())
        .expect("Failed to create branch_b");

    // Write different values to each branch for the same key
    {
        let mut tx_a = engine
            .begin_branch_transaction("branch_a")
            .expect("Failed to start transaction for branch_a");
        tx_a.put(b"test_key".to_vec(), b"value_from_a".to_vec())
            .expect("Failed to write to branch_a");
        tx_a.commit().expect("Failed to commit branch_a");
    }

    {
        let mut tx_b = engine
            .begin_branch_transaction("branch_b")
            .expect("Failed to start transaction for branch_b");
        tx_b.put(b"test_key".to_vec(), b"value_from_b".to_vec())
            .expect("Failed to write to branch_b");
        tx_b.commit().expect("Failed to commit branch_b");
    }

    // Verify each branch has its own isolated value
    {
        let tx_a = engine
            .begin_branch_transaction("branch_a")
            .expect("Failed to read from branch_a");
        let value_a = tx_a
            .get(&b"test_key".to_vec())
            .expect("Failed to get test_key from branch_a");
        assert_eq!(
            value_a,
            Some(b"value_from_a".to_vec()),
            "branch_a should have its own value"
        );
    }

    {
        let tx_b = engine
            .begin_branch_transaction("branch_b")
            .expect("Failed to read from branch_b");
        let value_b = tx_b
            .get(&b"test_key".to_vec())
            .expect("Failed to get test_key from branch_b");
        assert_eq!(
            value_b,
            Some(b"value_from_b".to_vec()),
            "branch_b should have its own value"
        );
    }

    // Verify main is unchanged
    let main_value = engine.get(&b"test_key".to_vec()).expect("Failed to read from main");
    assert_eq!(
        main_value, None,
        "main should not have test_key (it was never written to main)"
    );
}

// ============================================================================
// W2.0 — ART secondary-index cross-branch isolation (wrong-data class)
//
// Audit of the ART twin of the Wave-1 row-cache cross-branch poisoning: the
// `art_index_manager` trees are process-wide and branch-blind (keyed by
// (table, encoded-value) -> row_id with NO branch dimension). Branch reads
// bail on `is_branch_active` (scan.rs) and full-scan the `bdata:` overlay, so
// the shared ART must never gain OR lose entries for a branch's rows —
// otherwise main-branch indexed probes / unique checks observe wrong-branch
// data (a main probe treats `index_get_all` as authoritative).
//
// The dedicated branch DML engine helpers
// (`insert_tuple_branch_aware_with_schema`, `update_tuples_branch_aware`,
// `delete_tuples_branch_aware`) never touch the ART, and the autocommit /
// literal / parameterized *fast* paths bail on branches. But the SLOW /
// in-transaction plan arms — reached for every branch DML the fast paths hand
// off (autocommit-implicit-txn AND explicit BEGIN) — wrote each row
// branch-aware yet maintained the shared ART UNCONDITIONALLY. This is a whole
// class, not one leak: the INSERT, ON CONFLICT DO UPDATE, INSERT..SELECT,
// UPDATE (delete-old + insert-new) and DELETE plan arms on BOTH the text
// (`execute_in_transaction`) and parameterized (`execute_plan_with_params_inner`)
// engines, plus the embedded bulk-insert path and the FK CASCADE-delete helper.
// The INSERT direction adds a phantom; the UPDATE/DELETE directions REMOVE
// main's own entry for an inherited row (the worse direction — a main point
// probe then returns authoritative-empty and silently loses a real row, and
// unique/PK enforcement stops protecting the removed value). Every such site is
// now gated on `get_current_branch_id().is_none()` (the same predicate that
// routes `branch_aware_data_key`), so the shared ART reflects main data only.
//
// Out of scope (documented, NOT ART-poisoning): the parameterized
// `INSERT ... ON CONFLICT DO UPDATE` path calls `update_tuple_fast`, which
// writes the row to the MAIN `data:` key even on a branch — a deeper
// data-isolation issue, not an ART-index one; and the dead `execute_internal`
// DELETE arm (verified zero callers).
// ============================================================================

// Extract the first (integer) projected column of each result row as i64.
fn first_col_ints(rows: &[Tuple]) -> Vec<i64> {
    rows.iter()
        .map(|row| match row.values.first() {
            Some(Value::Int2(v)) => i64::from(*v),
            Some(Value::Int4(v)) => i64::from(*v),
            Some(Value::Int8(v)) => *v,
            other => panic!("expected integer first column, got {other:?}"),
        })
        .collect()
}

/// FLIPPING (fails pre-fix): a branch INSERT performed inside an explicit
/// transaction must not leak a phantom entry into the process-wide ART
/// secondary index that a main-branch probe then reads. Pre-fix
/// `insert_prepared_tuple_in_transaction` called
/// `on_insert_tuple_collect_index_values` unconditionally, so after switching
/// back to main the shared `idx_t_code` index still held the branch row.
#[test]
fn w2_branch_txn_insert_does_not_poison_shared_secondary_art() {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory db");
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, code INT)")
        .expect("create table");
    db.execute("CREATE INDEX idx_t_code ON t(code)")
        .expect("create secondary index");
    db.execute("INSERT INTO t (id, code) VALUES (1, 100)")
        .expect("seed main row");
    // Prime the main-branch indexed probe.
    assert_eq!(
        first_col_ints(&db.query("SELECT id FROM t WHERE code = 100", &[]).expect("probe main")),
        vec![1]
    );

    db.execute("CREATE BRANCH b1 AS OF NOW").expect("create branch");
    db.execute("USE BRANCH b1").expect("switch to branch");

    // Branch INSERT *inside an explicit transaction*: the only path that
    // reached `try_fast_insert_literal_in_transaction`, which pre-fix poisoned
    // the shared ART.
    db.execute("BEGIN").expect("begin on branch");
    db.execute("INSERT INTO t (id, code) VALUES (2, 200)")
        .expect("branch insert");
    db.execute("COMMIT").expect("commit on branch");

    // The branch sees its own overlay row (full-scan path — must stay correct).
    assert_eq!(
        first_col_ints(&db.query("SELECT id FROM t WHERE code = 200", &[]).expect("branch probe")),
        vec![2],
        "branch must see its own overlay row"
    );

    db.execute("USE BRANCH main").expect("switch back to main");

    // (a) FLIP: the shared ART must contain NO entry for the branch-only value.
    let idx = db
        .storage
        .art_indexes()
        .find_column_index("t", "code")
        .expect("code index exists");
    let phantom = db
        .storage
        .art_indexes()
        .index_get_all(&idx, &ArtIndexManager::encode_key(&[Value::Int4(200)]));
    assert!(
        phantom.is_empty(),
        "branch INSERT leaked a shared-ART entry visible on main: {phantom:?}"
    );

    // (b) Main indexed probes resolve only main data.
    assert!(
        db.query("SELECT id FROM t WHERE code = 200", &[])
            .expect("main probe 200")
            .is_empty(),
        "main must not see the branch's row via the secondary index"
    );
    assert_eq!(
        first_col_ints(&db.query("SELECT id FROM t WHERE code = 100", &[]).expect("main probe 100")),
        vec![1],
        "main must still see its own row"
    );
}

/// FLIPPING (fails pre-fix): the leaked shared-ART entry made a main INSERT of
/// a value held ONLY on a branch fail a phantom UNIQUE violation, because
/// `insert_tuple_fast` -> `check_unique_constraints_tuple` reads the shared ART
/// unconditionally. This is the user-visible face of the same bug.
#[test]
fn w2_branch_txn_insert_no_phantom_unique_violation_on_main() {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory db");
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, code INT UNIQUE)")
        .expect("create table");
    db.execute("INSERT INTO t (id, code) VALUES (1, 100)")
        .expect("seed main row");

    db.execute("CREATE BRANCH b1 AS OF NOW").expect("create branch");
    db.execute("USE BRANCH b1").expect("switch to branch");
    db.execute("BEGIN").expect("begin on branch");
    db.execute("INSERT INTO t (id, code) VALUES (2, 200)")
        .expect("branch insert");
    db.execute("COMMIT").expect("commit on branch");

    db.execute("USE BRANCH main").expect("switch back to main");

    // Pre-fix this failed: the shared UNIQUE ART already held code=200 from the
    // branch, so main saw a duplicate for a value it never held.
    let res = db.execute("INSERT INTO t (id, code) VALUES (3, 200)");
    assert!(
        res.is_ok(),
        "main INSERT of a branch-only unique value must not hit a phantom violation: {res:?}"
    );
    assert_eq!(
        first_col_ints(&db.query("SELECT id FROM t WHERE code = 200", &[]).expect("main probe")),
        vec![3],
        "main must resolve code=200 to its own row, not the branch's"
    );
}

/// FLIPPING (fails pre-fix): a branch UPDATE of an indexed column is NOT served
/// by the ART-free `update_tuples_branch_aware` helper on the live literal path
/// — an autocommit branch UPDATE hands off to the in-transaction UPDATE plan arm
/// (`execute_in_transaction`), which pre-fix ran `on_delete(old)+on_insert(new)`
/// on the process-wide ART whenever an indexed column changed. That removed
/// main's `100 -> row1` mapping and inserted a phantom `200 -> row1`, so after
/// switching back to main the point probe for `code = 100` returned
/// authoritative-empty (0 rows, flipping the `vec![1]` assertion) and the shared
/// index held `200`. The W2.0 gate makes the UPDATE plan arm skip shared-ART
/// maintenance on a branch.
#[test]
fn w2_branch_update_does_not_poison_secondary_index_on_main() {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory db");
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, code INT)")
        .expect("create table");
    db.execute("CREATE INDEX idx_t_code ON t(code)")
        .expect("create index");
    db.execute("INSERT INTO t (id, code) VALUES (1, 100)")
        .expect("seed main row");
    assert_eq!(
        first_col_ints(&db.query("SELECT id FROM t WHERE code = 100", &[]).unwrap()),
        vec![1]
    );

    db.execute("CREATE BRANCH b1 AS OF NOW").expect("create branch");
    db.execute("USE BRANCH b1").expect("switch to branch");
    db.execute("UPDATE t SET code = 200 WHERE id = 1")
        .expect("branch update");

    // Branch observes its overlay value.
    assert_eq!(
        first_col_ints(&db.query("SELECT id FROM t WHERE code = 200", &[]).unwrap()),
        vec![1],
        "branch sees the updated value"
    );
    assert!(
        db.query("SELECT id FROM t WHERE code = 100", &[]).unwrap().is_empty(),
        "branch no longer sees the pre-update value"
    );

    db.execute("USE BRANCH main").expect("switch back to main");

    // Main is untouched: the shared ART still maps 100 -> row 1 and has no 200.
    assert_eq!(
        first_col_ints(&db.query("SELECT id FROM t WHERE code = 100", &[]).unwrap()),
        vec![1],
        "main keeps its own value after a branch update"
    );
    assert!(
        db.query("SELECT id FROM t WHERE code = 200", &[]).unwrap().is_empty(),
        "main never sees the branch update via the index"
    );
    let idx = db
        .storage
        .art_indexes()
        .find_column_index("t", "code")
        .expect("code index");
    assert!(
        db.storage
            .art_indexes()
            .index_get_all(&idx, &ArtIndexManager::encode_key(&[Value::Int4(200)]))
            .is_empty(),
        "branch UPDATE must not add a shared-ART entry for the new value"
    );
}

/// FLIPPING (fails pre-fix): a branch INSERT that misses the fast in-transaction
/// literal path hands off to the INSERT *plan arm* (`execute_in_transaction`),
/// which pre-fix ran `on_insert` on the process-wide ART unconditionally. This
/// pins the plan-arm INSERT site — distinct from the W2.0 in-transaction
/// fast-INSERT path that tests 1/2 exercise. The table carries a CHECK
/// constraint so `fast_literal_insert_spec` bails (FK/CHECK tables return the
/// slow path), guaranteeing the plan arm is the route under test.
#[test]
fn w2_branch_autocommit_insert_does_not_poison_shared_secondary_art() {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory db");
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, code INT, CHECK (code >= 0))")
        .expect("create table");
    db.execute("CREATE INDEX idx_t_code ON t(code)")
        .expect("create secondary index");
    db.execute("INSERT INTO t (id, code) VALUES (1, 100)")
        .expect("seed main row");
    assert_eq!(
        first_col_ints(&db.query("SELECT id FROM t WHERE code = 100", &[]).expect("probe main")),
        vec![1]
    );

    db.execute("CREATE BRANCH b1 AS OF NOW").expect("create branch");
    db.execute("USE BRANCH b1").expect("switch to branch");

    // Branch INSERT into a CHECK table (fast path bails) — routes through the
    // INSERT plan arm, the site this test pins.
    db.execute("INSERT INTO t (id, code) VALUES (2, 200)")
        .expect("branch plan-arm insert");

    // The branch sees its own overlay row (full-scan path — must stay correct).
    assert_eq!(
        first_col_ints(&db.query("SELECT id FROM t WHERE code = 200", &[]).expect("branch probe")),
        vec![2],
        "branch must see its own overlay row"
    );

    db.execute("USE BRANCH main").expect("switch back to main");

    // (a) FLIP: the shared ART holds NO entry for the branch-only value.
    let idx = db
        .storage
        .art_indexes()
        .find_column_index("t", "code")
        .expect("code index exists");
    assert!(
        db.storage
            .art_indexes()
            .index_get_all(&idx, &ArtIndexManager::encode_key(&[Value::Int4(200)]))
            .is_empty(),
        "autocommit branch INSERT leaked a shared-ART entry visible on main"
    );

    // (b) Main indexed probes resolve only main data.
    assert!(
        db.query("SELECT id FROM t WHERE code = 200", &[])
            .expect("main probe 200")
            .is_empty(),
        "main must not see the branch's row via the secondary index"
    );
    assert_eq!(
        first_col_ints(&db.query("SELECT id FROM t WHERE code = 100", &[]).expect("main probe 100")),
        vec![1],
        "main must still see its own row"
    );
}

/// FLIPPING (fails pre-fix): deleting branch-inherited rows on a branch must not
/// strip main's shared-ART entries. Pre-fix both the autocommit DELETE plan arm
/// and the in-transaction DELETE arm ran `on_delete` on the process-wide ART
/// unconditionally, so after switching back to main the indexed probe for a
/// deleted value returned authoritative-empty and main silently lost the row —
/// the worse "remove-main" direction the original audit declared clean.
#[test]
fn w2_branch_delete_does_not_strip_secondary_index_on_main() {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory db");
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, code INT)")
        .expect("create table");
    db.execute("CREATE INDEX idx_t_code ON t(code)")
        .expect("create secondary index");
    db.execute("INSERT INTO t (id, code) VALUES (1, 100)")
        .expect("seed main row");
    db.execute("INSERT INTO t (id, code) VALUES (2, 200)")
        .expect("seed second main row");
    assert_eq!(
        first_col_ints(&db.query("SELECT id FROM t WHERE code = 100", &[]).unwrap()),
        vec![1]
    );

    db.execute("CREATE BRANCH b1 AS OF NOW").expect("create branch");
    db.execute("USE BRANCH b1").expect("switch to branch");

    // Autocommit branch DELETE of an inherited row (DELETE plan arm).
    db.execute("DELETE FROM t WHERE id = 1")
        .expect("branch autocommit delete");
    // In-transaction branch DELETE of a second inherited row (txn DELETE arm).
    db.execute("BEGIN").expect("begin on branch");
    db.execute("DELETE FROM t WHERE id = 2")
        .expect("branch delete in txn");
    db.execute("COMMIT").expect("commit on branch");

    // The branch no longer sees either row (overlay tombstone — full-scan path).
    assert!(
        db.query("SELECT id FROM t WHERE code = 100", &[]).unwrap().is_empty(),
        "branch must not see the row it deleted"
    );
    assert!(
        db.query("SELECT id FROM t WHERE code = 200", &[]).unwrap().is_empty(),
        "branch must not see the row it deleted in a txn"
    );

    db.execute("USE BRANCH main").expect("switch back to main");

    // (a) FLIP: main's shared-ART entries for the deleted values survive.
    let idx = db
        .storage
        .art_indexes()
        .find_column_index("t", "code")
        .expect("code index");
    assert!(
        !db.storage
            .art_indexes()
            .index_get_all(&idx, &ArtIndexManager::encode_key(&[Value::Int4(100)]))
            .is_empty(),
        "autocommit branch DELETE stripped main's shared-ART entry for code=100"
    );
    assert!(
        !db.storage
            .art_indexes()
            .index_get_all(&idx, &ArtIndexManager::encode_key(&[Value::Int4(200)]))
            .is_empty(),
        "branch txn DELETE stripped main's shared-ART entry for code=200"
    );

    // (b) Main indexed probes still resolve both of its own rows.
    assert_eq!(
        first_col_ints(&db.query("SELECT id FROM t WHERE code = 100", &[]).unwrap()),
        vec![1],
        "main lost its own row via the secondary index after a branch delete"
    );
    assert_eq!(
        first_col_ints(&db.query("SELECT id FROM t WHERE code = 200", &[]).unwrap()),
        vec![2],
        "main lost its own row via the secondary index after a branch txn delete"
    );
}
