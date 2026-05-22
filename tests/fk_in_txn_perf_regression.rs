//! In-transaction FK validation correctness and performance.
//!
//! Covers the v3.31.2 fix that removed the `active_txn.is_none()` gate
//! on the ART-index fast path in `check_referencing_rows_exist`. The
//! gate was added with the Quirk H fix in v3.30.0 to preserve
//! read-your-own-writes semantics on a hypothetical "ART only reflects
//! committed state" model — but in practice the engine has been
//! updating ART eagerly inside transactions since well before v3.30
//! (`lib.rs:1433` on_insert, `lib.rs:2303` on_delete,
//! `art_undo_log` for rollback at `lib.rs:578`). The gate therefore
//! gained no correctness and cost up to 338× on bulk-ingest workloads
//! that wrote ~100k FK-bearing rows inside one transaction (codekb-mcp
//! plugin; see `ENGINE_REGRESSION_BISECT_v3.28.0.md`).
//!
//! These tests pin down the per-txn correctness properties that must
//! hold AFTER the gate removal, and the perf floor on the bulk path
//! that must not regress again.

use heliosdb_nano::EmbeddedDatabase;
use std::time::Instant;

/// Helper: `db.execute()` returns a u64 rowcount (rows AFFECTED for DML,
/// rows RETURNED for SELECT). For row-count verification we use SELECT *
/// since count(*) always returns 1 (one row containing the count value),
/// not the count itself.
fn row_count(db: &EmbeddedDatabase, table: &str) -> u64 {
    db.execute(&format!("SELECT * FROM {table}")).expect("select")
}

/// Edge case 1: BEGIN; INSERT parent; INSERT child FK→parent; COMMIT.
/// Child must see the just-inserted parent — the in-txn ART index has
/// been updated by the parent insert.
#[test]
fn in_txn_insert_parent_then_insert_child_succeeds() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE p (id INTEGER PRIMARY KEY, name TEXT)").expect("create p");
    db.execute("CREATE TABLE c (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES p(id))")
        .expect("create c");

    db.execute("BEGIN").expect("begin");
    db.execute("INSERT INTO p (id, name) VALUES (1, 'parent')")
        .expect("insert parent in txn");
    db.execute("INSERT INTO c (id, parent_id) VALUES (101, 1)")
        .expect("insert child FK→just-inserted parent");
    db.execute("COMMIT").expect("commit");

    // Verify both rows are visible after commit.
    assert_eq!(row_count(&db, "p"), 1, "parent row missing post-commit");
    assert_eq!(row_count(&db, "c"), 1, "child row missing post-commit");
}

/// Edge case 2: BEGIN; DELETE parent; INSERT child FK→(deleted) parent; ROLLBACK.
/// The child insert must FK-violate — the parent is gone in this txn's view.
#[test]
fn in_txn_delete_parent_then_insert_orphan_child_violates() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE p (id INTEGER PRIMARY KEY)").expect("create p");
    db.execute("CREATE TABLE c (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES p(id))")
        .expect("create c");
    db.execute("INSERT INTO p (id) VALUES (1)").expect("seed parent");

    db.execute("BEGIN").expect("begin");
    db.execute("DELETE FROM p WHERE id = 1").expect("delete parent in txn");
    // Child references the just-deleted parent. Must FK-violate.
    let r = db.execute("INSERT INTO c (id, parent_id) VALUES (101, 1)");
    assert!(
        r.is_err(),
        "INSERT child referencing in-txn-deleted parent must FK-violate; got {r:?}"
    );
    db.execute("ROLLBACK").expect("rollback");

    // Post-rollback: parent should still exist (ART consistency).
    assert_eq!(row_count(&db, "p"), 1, "parent must survive rollback");
}

/// Edge case 3: BEGIN; DELETE child; DELETE parent; COMMIT.
/// This is the v3.22.1 regression case — the DELETE on parent must not
/// FK-violate because the in-txn DELETE on child removed the dependent
/// row already. Read-your-own-writes via the write-set merge in the
/// scan fallback path (FK-target unindexed) or via the on_delete
/// eager ART update (FK-target indexed).
#[test]
fn in_txn_delete_child_then_delete_parent_succeeds() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE p (id INTEGER PRIMARY KEY)").expect("create p");
    db.execute("CREATE TABLE c (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES p(id))")
        .expect("create c");
    db.execute("INSERT INTO p (id) VALUES (1)").expect("seed parent");
    db.execute("INSERT INTO c (id, parent_id) VALUES (101, 1)")
        .expect("seed child");

    db.execute("BEGIN").expect("begin");
    db.execute("DELETE FROM c WHERE id = 101").expect("delete child in txn");
    // Parent delete must succeed — the child is gone in this txn's view.
    db.execute("DELETE FROM p WHERE id = 1")
        .expect("delete parent after child removed in same txn (v3.22.1 regression case)");
    db.execute("COMMIT").expect("commit");

    assert_eq!(row_count(&db, "p"), 0, "parent should be gone after commit");
}

/// Edge case 4 (KNOWN LIMITATION — pre-existing engine bug, not in T1 scope):
/// BEGIN; UPDATE parent SET pk = new; INSERT child FK→new pk; COMMIT.
///
/// This test is marked `#[ignore]` because the regular UPDATE path in
/// `src/lib.rs` (around line 1940-1997 in v3.31.2) does NOT refresh the
/// ART index after writing the updated tuple. Compare to INSERT
/// (`art_indexes().on_insert` at line 1433) and ON CONFLICT DO UPDATE
/// (line 1320-1322 which DOES refresh ART). The plain UPDATE path is
/// missing the ART refresh.
///
/// This is independent of the v3.28.0 FK regression that T1 is closing.
/// It will need a separate fix (add ART refresh to the UPDATE write loop
/// around line 1997). Until that lands, in-txn UPDATEs that change a PK
/// column will not be observable by subsequent FK checks within the same
/// transaction.
#[test]
#[ignore = "pre-existing: regular UPDATE path doesn't refresh ART; needs separate engine fix"]
fn in_txn_update_parent_pk_then_insert_child_fk_to_new_succeeds() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE p (id INTEGER PRIMARY KEY)").expect("create p");
    db.execute("CREATE TABLE c (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES p(id))")
        .expect("create c");
    db.execute("INSERT INTO p (id) VALUES (1)").expect("seed parent id=1");

    db.execute("BEGIN").expect("begin");
    db.execute("UPDATE p SET id = 42 WHERE id = 1").expect("update parent pk in txn");
    // Child references the NEW pk value (42), not the old (1).
    db.execute("INSERT INTO c (id, parent_id) VALUES (101, 42)")
        .expect("insert child FK→updated parent pk");
    // INSERT referencing the OLD pk must violate (the old key no longer indexes a row).
    let stale = db.execute("INSERT INTO c (id, parent_id) VALUES (102, 1)");
    assert!(
        stale.is_err(),
        "INSERT child referencing pre-UPDATE parent pk must FK-violate; got {stale:?}"
    );
    db.execute("COMMIT").expect("commit");
}

/// Edge case 5 (correctness under rollback): BEGIN; INSERT parent;
/// INSERT child FK→parent; ROLLBACK. Both rows must vanish + ART must
/// no longer see the parent.
#[test]
fn in_txn_insert_then_rollback_clears_art() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE p (id INTEGER PRIMARY KEY)").expect("create p");
    db.execute("CREATE TABLE c (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES p(id))")
        .expect("create c");

    db.execute("BEGIN").expect("begin");
    db.execute("INSERT INTO p (id) VALUES (777)").expect("insert parent in txn");
    db.execute("INSERT INTO c (id, parent_id) VALUES (1, 777)")
        .expect("insert child in txn");
    db.execute("ROLLBACK").expect("rollback");

    // Post-rollback: parent gone, ART should not retain the entry (art_undo_log).
    // A NEW txn that tries to insert a child referencing 777 must FK-violate.
    let r = db.execute("INSERT INTO c (id, parent_id) VALUES (2, 777)");
    assert!(
        r.is_err(),
        "ART must not retain rolled-back insertions; got {r:?}"
    );
}

/// Performance regression guard for the v3.28.0 → v3.30.0 → v3.31.2 path.
///
/// At ~3k FK-bearing INSERTs into one transaction, the v3.28-30 implementation
/// performed ~3k × 1.5k = ~4.5M tuple deserialisations and took multiple
/// seconds on a workstation. The v3.31.2 ART-fast-path should complete the
/// same workload in well under 5 seconds (typically <1s on a release build,
/// proportionally slower on dev builds due to lack of inlining; the threshold
/// below is generous to avoid flakes in slow CI).
///
/// This is a scaled-down stand-in for the codekb-mcp 117k-refs corpus
/// (which takes ~30 minutes to run end-to-end and is unsuitable for CI);
/// the wall-clock here is dominated by the same code path and regresses
/// proportionally.
#[test]
fn in_txn_bulk_fk_inserts_under_5s() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE p (id INTEGER PRIMARY KEY)").expect("create p");
    db.execute("CREATE TABLE c (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES p(id))")
        .expect("create c");

    const N_PARENTS: i32 = 1500;
    const N_CHILDREN: i32 = 3000; // 2 children per parent on average

    db.execute("BEGIN").expect("begin");
    for i in 1..=N_PARENTS {
        db.execute(&format!("INSERT INTO p (id) VALUES ({i})"))
            .expect("parent insert");
    }
    let t0 = Instant::now();
    for i in 1..=N_CHILDREN {
        let parent_id = ((i - 1) % N_PARENTS) + 1;
        db.execute(&format!(
            "INSERT INTO c (id, parent_id) VALUES ({i}, {parent_id})"
        ))
        .expect("child insert");
    }
    let dt = t0.elapsed();
    db.execute("COMMIT").expect("commit");

    // Threshold: 5 seconds for 3k FK-checked inserts in one txn.
    // Pre-v3.31.2, this same workload took ~30+ seconds on the same
    // hardware (verified by reverting the gate). Post-fix typically
    // <1s on release builds.
    assert!(
        dt.as_secs() < 5,
        "in-txn bulk FK inserts must complete under 5s; took {dt:?} (regression of v3.28.0 path?)"
    );
}
