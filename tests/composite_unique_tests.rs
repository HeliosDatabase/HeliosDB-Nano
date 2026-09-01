//! Table-level composite `UNIQUE (a, b)` enforcement (task #107).
//!
//! WHAT WAS BROKEN. `Catalog::create_table` derives unique ART indexes from
//! `schema.columns` — the COLUMN-level `unique` flag. A table-level constraint lives in
//! `TableConstraints`, which that function never sees. So a composite `UNIQUE (a, b)` had
//! no index; with no index there was nothing to probe, and the constraint was enforced by
//! NOTHING on any write path, on either executor family. Duplicates were silently accepted.
//!
//! This was the ninth "machinery exists, no caller" defect of the campaign: everything
//! below the creation call was ALREADY composite-capable and is unchanged by the fix —
//! `ArtIndexManager::insert_row_indexes` resolves `entry.columns` at any arity, and
//! `check_unique_constraints_tuple` encodes the multi-value key and skips the constraint
//! when any column is NULL. Only the `create_unique_index` call was missing.
//!
//! SCOPE NOTE. Single-column table-level `UNIQUE (a)` was ALREADY enforced before this fix
//! (that shape also sets the column-level flag). `unique_shapes_that_already_worked_still_work`
//! pins that, so the fix cannot be credited with something it did not change and cannot
//! silently regress it either.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

fn mem_db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory db")
}

/// Rows physically present. Deliberately NOT `SELECT COUNT(*)` — a count query returns one
/// row whether the count is 0 or 10,000.
fn rows_in(db: &EmbeddedDatabase, table: &str) -> usize {
    let sql = format!("SELECT * FROM {table}");
    db.query(&sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
        .len()
}

const DDL: &str = "CREATE TABLE cu (id INT PRIMARY KEY, a INT, b INT, CONSTRAINT cu_ab UNIQUE (a, b))";

// ===========================================================================
// The regression test for #107 — must fail against v4.24.0
// ===========================================================================

#[test]
fn a_duplicate_composite_unique_is_rejected_on_both_families() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute(DDL).unwrap();
        db.execute("INSERT INTO cu (id, a, b) VALUES (1, 7, 8)").unwrap();
    }

    let db = mem_db();
    setup(&db);
    let text_rejected = db.execute("INSERT INTO cu (id, a, b) VALUES (2, 7, 8)").is_err();
    let text_rows = rows_in(&db, "cu");

    let db2 = mem_db();
    setup(&db2);
    let params_rejected = db2
        .execute_params("INSERT INTO cu (id, a, b) VALUES (2, 7, 8)", &[])
        .is_err();
    let params_rows = rows_in(&db2, "cu");

    assert_eq!(
        params_rejected, text_rejected,
        "DIVERGENCE: text rejected={text_rejected}, params rejected={params_rejected}"
    );
    assert!(
        text_rejected,
        "a duplicate (a, b) pair must violate the composite UNIQUE constraint"
    );
    assert_eq!(params_rows, text_rows, "DIVERGENCE in rows left behind");
    assert_eq!(text_rows, 1, "only the pre-existing row may remain");
}

/// The constraint is on the PAIR, not on either column. Guards against the fix being
/// implemented as "index each column separately", which would reject far too much.
#[test]
fn a_partial_overlap_is_accepted_on_both_families() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute(DDL).unwrap();
        db.execute("INSERT INTO cu (id, a, b) VALUES (1, 7, 8)").unwrap();
    }

    for (label, run) in [("text", 0), ("params", 1)] {
        let db = mem_db();
        setup(&db);
        // Same a, different b.
        let sql1 = "INSERT INTO cu (id, a, b) VALUES (2, 7, 99)";
        // Different a, same b.
        let sql2 = "INSERT INTO cu (id, a, b) VALUES (3, 99, 8)";
        let (r1, r2) = if run == 0 {
            (db.execute(sql1), db.execute(sql2))
        } else {
            (db.execute_params(sql1, &[]), db.execute_params(sql2, &[]))
        };
        r1.unwrap_or_else(|e| panic!("{label}: same a / different b must be accepted: {e}"));
        r2.unwrap_or_else(|e| panic!("{label}: different a / same b must be accepted: {e}"));
        assert_eq!(rows_in(&db, "cu"), 3, "{label}: all three rows must be present");
    }
}

/// PostgreSQL: a UNIQUE constraint is not violated when any constrained column is NULL —
/// two rows of `(NULL, 8)` may coexist. `check_unique_constraints_tuple` implements this
/// by skipping the constraint on any NULL; pinned here so the new index cannot change it.
#[test]
fn nulls_do_not_collide_under_composite_unique() {
    let db = mem_db();
    db.execute(DDL).unwrap();
    db.execute("INSERT INTO cu (id, a, b) VALUES (1, NULL, 8)").unwrap();
    db.execute("INSERT INTO cu (id, a, b) VALUES (2, NULL, 8)")
        .expect("two NULL-bearing rows must not collide under a UNIQUE constraint");
    assert_eq!(rows_in(&db, "cu"), 2, "both NULL-bearing rows must be stored");
}

/// INSERT … SELECT reaches the storage-layer unique probe through the shared gate added in
/// v4.24.0. Before #107 this was the `pinned_gap_composite_unique_is_enforced_on_neither_family`
/// case in `insert_select_consistency_tests` — that pin has been replaced by this assertion.
#[test]
fn insert_select_also_rejects_a_composite_duplicate_on_both_families() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute(DDL).unwrap();
        db.execute("INSERT INTO cu (id, a, b) VALUES (1, 7, 8)").unwrap();
        db.execute("CREATE TABLE cu_src (id INT, a INT, b INT)").unwrap();
        db.execute("INSERT INTO cu_src (id, a, b) VALUES (2, 7, 8)").unwrap();
    }

    let db = mem_db();
    setup(&db);
    let text_rejected = db
        .execute("INSERT INTO cu (id, a, b) SELECT id, a, b FROM cu_src")
        .is_err();
    let text_rows = rows_in(&db, "cu");

    let db2 = mem_db();
    setup(&db2);
    let params_rejected = db2
        .execute_params("INSERT INTO cu (id, a, b) SELECT id, a, b FROM cu_src", &[])
        .is_err();
    let params_rows = rows_in(&db2, "cu");

    assert_eq!(
        params_rejected, text_rejected,
        "DIVERGENCE: text rejected={text_rejected}, params rejected={params_rejected}"
    );
    assert!(text_rejected, "INSERT … SELECT must honour composite UNIQUE");
    assert_eq!(params_rows, text_rows, "DIVERGENCE in rows left behind");
    assert_eq!(text_rows, 1, "the duplicate row must not be stored");
}

/// An UPDATE that moves a row onto an existing (a, b) pair must be rejected too — the
/// constraint is not insert-only.
#[test]
fn an_update_into_a_duplicate_pair_is_rejected() {
    let db = mem_db();
    db.execute(DDL).unwrap();
    db.execute("INSERT INTO cu (id, a, b) VALUES (1, 7, 8)").unwrap();
    db.execute("INSERT INTO cu (id, a, b) VALUES (2, 1, 2)").unwrap();

    let rejected = db.execute("UPDATE cu SET a = 7, b = 8 WHERE id = 2").is_err();
    let surviving: Vec<Value> = db
        .query("SELECT a FROM cu WHERE id = 2", &[])
        .unwrap()
        .iter()
        .map(|r| r.values.first().cloned().unwrap_or(Value::Null))
        .collect();

    assert!(
        rejected,
        "UPDATE onto an existing (a, b) pair must violate the composite UNIQUE constraint; \
         row 2 now reads a={surviving:?}"
    );
}

// ===========================================================================
// Durability: the index must survive a reopen, backfilled
// ===========================================================================

/// The dangerous half-fix is registering the composite index only at CREATE TABLE: after a
/// restart the index is gone (or empty), and duplicates against pre-restart rows are
/// accepted again. `rebuild_all_indexes` therefore registers it too, BEFORE the snapshot
/// load / row replay so the tree is backfilled with existing rows.
#[test]
fn composite_unique_survives_a_reopen_and_is_backfilled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_str().unwrap().to_string();

    {
        let db = EmbeddedDatabase::new(&path).expect("open");
        db.execute(DDL).unwrap();
        db.execute("INSERT INTO cu (id, a, b) VALUES (1, 7, 8)").unwrap();
        assert_eq!(rows_in(&db, "cu"), 1);
    }

    let db = EmbeddedDatabase::new(&path).expect("reopen");
    assert_eq!(rows_in(&db, "cu"), 1, "the pre-restart row must still be there");

    let rejected = db.execute("INSERT INTO cu (id, a, b) VALUES (2, 7, 8)").is_err();
    assert!(
        rejected,
        "after reopen, a duplicate of a PRE-RESTART row must still be rejected — if this \
         passes, the composite index was registered but never backfilled"
    );
    assert_eq!(rows_in(&db, "cu"), 1, "the duplicate must not be stored");

    // …and a genuinely new pair still inserts, so the reopened index is not rejecting
    // everything (which would also make the assertion above pass, for the wrong reason).
    db.execute("INSERT INTO cu (id, a, b) VALUES (3, 7, 9)")
        .expect("a new (a, b) pair must still be accepted after reopen");
    assert_eq!(rows_in(&db, "cu"), 2);
}

// ===========================================================================
// Lifecycle: DROP TABLE and DROP CONSTRAINT must stop enforcement
// ===========================================================================

/// A recreated table must not inherit the dropped table's index — otherwise the stale ART
/// entries reject rows in a brand-new, empty table.
#[test]
fn drop_table_releases_the_composite_index() {
    let db = mem_db();
    db.execute(DDL).unwrap();
    db.execute("INSERT INTO cu (id, a, b) VALUES (1, 7, 8)").unwrap();
    db.execute("DROP TABLE cu").unwrap();

    db.execute(DDL).unwrap();
    assert_eq!(rows_in(&db, "cu"), 0, "the recreated table must be empty");
    db.execute("INSERT INTO cu (id, a, b) VALUES (1, 7, 8)")
        .expect("the recreated table must accept the pair the dropped table held");
    assert_eq!(rows_in(&db, "cu"), 1);
}

/// After `DROP CONSTRAINT` the pair is no longer constrained, so the duplicate must be
/// accepted. Guards the index-name handling in `Catalog::drop_constraint`: the index is
/// named after the CONSTRAINT here, which the original `unique_{table}_{name}` lookup did
/// not match, so without the added candidate the index would linger and keep rejecting.
#[test]
fn drop_constraint_stops_composite_unique_enforcement() {
    let db = mem_db();
    db.execute(DDL).unwrap();
    db.execute("INSERT INTO cu (id, a, b) VALUES (1, 7, 8)").unwrap();

    match db.execute("ALTER TABLE cu DROP CONSTRAINT cu_ab") {
        Ok(_) => {
            db.execute("INSERT INTO cu (id, a, b) VALUES (2, 7, 8)").expect(
                "after DROP CONSTRAINT the duplicate pair must be accepted — a lingering ART \
                 index is still enforcing a constraint that no longer exists",
            );
            assert_eq!(rows_in(&db, "cu"), 2);
        }
        Err(e) => panic!("ALTER TABLE ... DROP CONSTRAINT failed: {e}"),
    }
}

// ===========================================================================
// Anti-regression: shapes that already worked must be untouched
// ===========================================================================

/// Column-level `a INT UNIQUE` and single-column table-level `UNIQUE (a)` were BOTH already
/// enforced before #107 (verified end to end before the fix was written). The fix skips
/// single-column constraints precisely so these are not double-registered under a second
/// name. This test states that plainly so the fix is never credited with them.
#[test]
fn unique_shapes_that_already_worked_still_work() {
    for (label, ddl) in [
        (
            "column-level UNIQUE",
            "CREATE TABLE u (id INT PRIMARY KEY, a INT UNIQUE)",
        ),
        (
            "table-level single-column UNIQUE",
            "CREATE TABLE u (id INT PRIMARY KEY, a INT, UNIQUE (a))",
        ),
    ] {
        let db = mem_db();
        db.execute(ddl).unwrap();
        db.execute("INSERT INTO u (id, a) VALUES (1, 7)").unwrap();
        let rejected = db.execute("INSERT INTO u (id, a) VALUES (2, 7)").is_err();
        assert!(rejected, "{label}: a duplicate must still be rejected");
        assert_eq!(rows_in(&db, "u"), 1, "{label}: the duplicate must not be stored");
        db.execute("INSERT INTO u (id, a) VALUES (3, 8)")
            .unwrap_or_else(|e| panic!("{label}: a distinct value must still be accepted: {e}"));
    }
}

/// Three-column composite, to prove the fix is arity-generic rather than special-cased to
/// two columns.
#[test]
fn a_three_column_composite_unique_is_enforced() {
    let db = mem_db();
    db.execute("CREATE TABLE t3 (id INT PRIMARY KEY, a INT, b INT, c INT, CONSTRAINT t3_abc UNIQUE (a, b, c))")
        .unwrap();
    db.execute("INSERT INTO t3 (id, a, b, c) VALUES (1, 1, 2, 3)").unwrap();

    let rejected = db.execute("INSERT INTO t3 (id, a, b, c) VALUES (2, 1, 2, 3)").is_err();
    assert!(rejected, "an identical (a, b, c) triple must be rejected");
    assert_eq!(rows_in(&db, "t3"), 1);

    db.execute("INSERT INTO t3 (id, a, b, c) VALUES (3, 1, 2, 4)")
        .expect("a triple differing in the LAST column must be accepted");
    assert_eq!(rows_in(&db, "t3"), 2);
}
