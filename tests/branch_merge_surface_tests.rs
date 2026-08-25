//! #72: the user-reachable branch-merge surfaces.
//!
//! These drive SQL and the public `EmbeddedDatabase` API — the paths a user or the
//! MCP tool actually takes. The pre-existing `branch_merge_conflict_tests` suite
//! drives `begin_branch_transaction`/`BranchTransaction`, which has zero production
//! callers and a different on-disk key encoding than the merge implementation the
//! SQL path uses; that suite could never pass and was not evidence about this
//! feature. Assertions here are on observable row content after the merge.

use heliosdb_nano::EmbeddedDatabase;

fn seeded() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, val TEXT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'main_row')").unwrap();
    db
}

fn ids_on_current_branch(db: &EmbeddedDatabase) -> Vec<i32> {
    db.query("SELECT id FROM t ORDER BY id", &[])
        .unwrap()
        .iter()
        .filter_map(|row| match row.get(0) {
            Some(heliosdb_nano::Value::Int4(i)) => Some(*i),
            Some(heliosdb_nano::Value::Int8(i)) => Some(*i as i32),
            _ => None,
        })
        .collect()
}

/// The headline behaviour: a row written on a branch is visible on the target
/// after MERGE BRANCH. Asserts CONTENT, not that the statement returned Ok.
#[test]
fn merge_branch_into_main_moves_rows_that_a_select_can_see() {
    let db = seeded();
    db.execute("CREATE BRANCH dev AS OF NOW").unwrap();
    db.execute("USE BRANCH dev").unwrap();
    db.execute("INSERT INTO t VALUES (2, 'dev_row')").unwrap();
    assert_eq!(ids_on_current_branch(&db), vec![1, 2], "branch sees its own write");

    db.execute("USE BRANCH main").unwrap();
    assert_eq!(ids_on_current_branch(&db), vec![1], "main is isolated pre-merge");

    db.execute("MERGE BRANCH dev INTO main").unwrap();
    assert_eq!(
        ids_on_current_branch(&db),
        vec![1, 2],
        "the branch row must be readable on main after the merge"
    );
}

/// `conflict_resolution` parses (the paren bug is fixed) but must NOT silently
/// succeed: `StorageEngine::merge_branch` ignores the strategy and hard-codes
/// `conflicts: Vec::new()`, so honouring the option is impossible today. Silent
/// acceptance would tell a caller who asked for 'target_wins' that a
/// last-writer-wins merge completed with 0 conflicts.
#[test]
fn merge_branch_rejects_conflict_resolution_as_unimplemented() {
    let db = seeded();
    db.execute("CREATE BRANCH dev AS OF NOW").unwrap();
    db.execute("USE BRANCH dev").unwrap();
    db.execute("INSERT INTO t VALUES (2, 'dev_row')").unwrap();
    db.execute("USE BRANCH main").unwrap();

    let err = db
        .execute("MERGE BRANCH dev INTO main WITH (conflict_resolution = 'branch_wins')")
        .expect_err("conflict_resolution must not be silently ignored");
    let msg = err.to_string();
    assert!(
        msg.contains("not implemented"),
        "the error must say the option is unimplemented, not that the key is unknown: {msg}"
    );
    assert!(
        !msg.contains("(conflict_resolution"),
        "and it must not be the old paren parse error: {msg}"
    );

    // The plain merge still works — only the unhonourable option is refused.
    db.execute("MERGE BRANCH dev INTO main").unwrap();
    assert_eq!(ids_on_current_branch(&db), vec![1, 2]);
}

#[test]
fn merge_branch_accepts_delete_branch_after_option() {
    let db = seeded();
    db.execute("CREATE BRANCH gone AS OF NOW").unwrap();
    db.execute("MERGE BRANCH gone INTO main WITH (delete_branch_after = true)")
        .expect("WITH (delete_branch_after = ...) must parse");
}

#[test]
fn merge_branch_rejects_a_genuinely_unknown_option() {
    let db = seeded();
    db.execute("CREATE BRANCH dev AS OF NOW").unwrap();
    let err = db
        .execute("MERGE BRANCH dev INTO main WITH (no_such_option = 1)")
        .expect_err("an unknown option must still be an error");
    let msg = err.to_string();
    assert!(msg.contains("no_such_option"), "error should name the option: {msg}");
    assert!(
        !msg.contains("(no_such_option"),
        "the paren must be stripped before the key is reported: {msg}"
    );
}

/// The same paren bug applied to CREATE BRANCH.
#[test]
fn create_branch_accepts_a_with_option_list() {
    let db = seeded();
    db.execute("CREATE BRANCH regional AS OF NOW WITH (region = 'eu')")
        .expect("CREATE BRANCH ... WITH (region = ...) must parse");
}

#[test]
fn create_branch_rejects_a_genuinely_unknown_option() {
    let db = seeded();
    let err = db
        .execute("CREATE BRANCH bad AS OF NOW WITH (no_such_option = 1)")
        .expect_err("an unknown branch option must still be an error");
    assert!(err.to_string().contains("no_such_option"));
}

/// The public helper emitted `MERGE BRANCH <src>` with no `INTO`, which its own
/// parser rejects — so it always failed. The MCP `branch_merge` tool is its only
/// production caller and switches to the target first, hence "into current".
#[test]
fn public_merge_branch_helper_merges_into_the_current_branch() {
    let db = seeded();
    db.execute("CREATE BRANCH dev AS OF NOW").unwrap();
    db.execute("USE BRANCH dev").unwrap();
    db.execute("INSERT INTO t VALUES (2, 'dev_row')").unwrap();
    db.execute("USE BRANCH main").unwrap();

    db.merge_branch("dev")
        .expect("the public helper must not emit invalid SQL");
    assert_eq!(
        ids_on_current_branch(&db),
        vec![1, 2],
        "helper must merge into the branch the handle is on"
    );
}

/// Guards the MCP tool's actual shape: switch to target, then merge.
#[test]
fn public_helper_targets_a_non_main_branch_when_switched_to_it() {
    let db = seeded();
    db.execute("CREATE BRANCH feature AS OF NOW").unwrap();
    db.execute("CREATE BRANCH donor AS OF NOW").unwrap();
    db.execute("USE BRANCH donor").unwrap();
    db.execute("INSERT INTO t VALUES (3, 'donor_row')").unwrap();

    db.execute("USE BRANCH feature").unwrap();
    db.merge_branch("donor").expect("merge into a non-main current branch");
    assert!(
        ids_on_current_branch(&db).contains(&3),
        "donor's row must land on the branch we were on, not main"
    );
}

// ---------------------------------------------------------------------------
// The helper family: every public method that builds SQL by hand.
//
// `merge_branch` emitted `MERGE BRANCH <src>` (no INTO) and `list_branches`
// emitted `LIST BRANCHES` (not in the grammar) — both parse errors on every
// call, in neighbouring methods, neither caught because nothing executed them.
// These tests run each one so a hand-built statement cannot silently rot again.
// ---------------------------------------------------------------------------

#[test]
fn list_branches_helper_emits_parseable_sql() {
    let db = seeded();
    db.execute("CREATE BRANCH listed AS OF NOW").unwrap();
    let rows = db.list_branches().expect("list_branches must not emit invalid SQL");
    let names = format!("{rows:?}");
    assert!(names.contains("listed"), "the created branch should be listed: {names}");
    assert!(names.contains("main"), "main should be listed: {names}");
}

#[test]
fn drop_branch_helper_emits_parseable_sql() {
    let db = seeded();
    db.execute("CREATE BRANCH doomed AS OF NOW").unwrap();
    db.drop_branch("doomed").expect("drop_branch must not emit invalid SQL");
    let names = format!("{:?}", db.list_branches().unwrap());
    assert!(!names.contains("doomed"), "dropped branch should be gone: {names}");
}

#[test]
fn explain_helpers_emit_parseable_sql() {
    let db = seeded();
    db.explain("SELECT id FROM t")
        .expect("explain must not emit invalid SQL");
    db.explain_analyze("SELECT id FROM t")
        .expect("explain_analyze must not emit invalid SQL");
}

#[test]
fn refresh_materialized_view_helper_emits_parseable_sql() {
    let db = seeded();
    db.execute("CREATE MATERIALIZED VIEW mv AS SELECT id FROM t").unwrap();
    db.refresh_materialized_view("mv")
        .expect("refresh_materialized_view must not emit invalid SQL");
}

// ---------------------------------------------------------------------------
// Ported from branch_merge_conflict_tests, which drove the dead
// `BranchTransaction` API. These three asserted mechanics the real merge does
// support, so they are rewritten against SQL rather than deleted. The six that
// asserted conflict DETECTION or strategy semantics were deleted instead:
// `StorageEngine::merge_branch` takes `_strategy` and hard-codes
// `conflicts: Vec::new()`, so there is no behaviour there to test.
// ---------------------------------------------------------------------------

/// Was `test_merge_with_deletions`: a row deleted on the branch must be deleted
/// on the target after the merge. The real implementation scans `bdel:` markers,
/// so this is reachable — unlike the conflict tests.
#[test]
fn merge_carries_a_branch_deletion_to_the_target() {
    let db = seeded();
    db.execute("INSERT INTO t VALUES (2, 'second')").unwrap();
    db.execute("CREATE BRANCH dev AS OF NOW").unwrap();
    db.execute("USE BRANCH dev").unwrap();
    db.execute("DELETE FROM t WHERE id = 1").unwrap();
    assert_eq!(ids_on_current_branch(&db), vec![2], "branch sees its own delete");

    db.execute("USE BRANCH main").unwrap();
    assert_eq!(ids_on_current_branch(&db), vec![1, 2], "main isolated pre-merge");

    db.execute("MERGE BRANCH dev INTO main").unwrap();
    assert_eq!(
        ids_on_current_branch(&db),
        vec![2],
        "the deletion must propagate: row 1 gone, row 2 kept"
    );
}

/// Was `test_merge_preserves_non_conflicting_changes`: rows unique to each side
/// both survive. The original also asserted a conflicting key resolved to the
/// branch's value under MergeStrategy::Theirs; that assertion is dropped because
/// strategy is ignored and merging is last-writer-wins.
#[test]
fn merge_preserves_rows_unique_to_each_branch() {
    let db = seeded();
    db.execute("CREATE BRANCH dev AS OF NOW").unwrap();
    db.execute("USE BRANCH dev").unwrap();
    db.execute("INSERT INTO t VALUES (2, 'dev_only')").unwrap();
    db.execute("USE BRANCH main").unwrap();
    db.execute("INSERT INTO t VALUES (3, 'main_only')").unwrap();

    db.execute("MERGE BRANCH dev INTO main").unwrap();
    assert_eq!(
        ids_on_current_branch(&db),
        vec![1, 2, 3],
        "main's own row must survive the merge alongside the branch's"
    );
}

/// Was `test_merge_large_dataset` (1000 rows). Scaled to 200: this is a
/// correctness test, not a benchmark, and this host is resource-constrained.
/// The timing assertion is dropped — a wall-clock bound in a correctness suite
/// is a flake generator on a shared machine.
#[test]
fn merge_carries_a_large_branch_diff() {
    let db = seeded();
    db.execute("CREATE BRANCH bulk AS OF NOW").unwrap();
    db.execute("USE BRANCH bulk").unwrap();
    for i in 100..300 {
        db.execute(&format!("INSERT INTO t VALUES ({i}, 'bulk_{i}')")).unwrap();
    }
    assert_eq!(ids_on_current_branch(&db).len(), 201, "1 seeded + 200 bulk");

    db.execute("USE BRANCH main").unwrap();
    db.execute("MERGE BRANCH bulk INTO main").unwrap();
    let after = ids_on_current_branch(&db);
    assert_eq!(after.len(), 201, "every bulk row must land on main");
    assert!(after.contains(&100) && after.contains(&299), "range endpoints present");
}
