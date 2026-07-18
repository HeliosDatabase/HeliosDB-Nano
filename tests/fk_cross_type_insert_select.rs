//! I-FK increment 0 — the cross-type FK "twin" on the INSERT..SELECT path.
//!
//! Before the fix, the INSERT..SELECT write arm in
//! `execute_in_transaction_inner` ran its OWN inline FK-membership probe: it
//! built an ART key from the RAW child values and called `pk_index_contains`
//! (and its `check_foreign_key_exists` fallback) WITHOUT coercing the probe to
//! the referenced PK's declared type. `ArtIndexManager::encode_key` is
//! type-width-sensitive, so a cross-type FK (int child -> int8 parent, or the
//! reverse) encoded a width-mismatched key that could never match the parent's
//! stored key -> a PHANTOM 23503 "references non-existent" on rows that DO
//! satisfy the FK. The exact-`==` fallback failed identically, because
//! `Value::Int4(10) != Value::Int8(10)`.
//!
//! The fix routes this arm through the shared, type-aware validator
//! `check_fk_constraints_on_write` (the same one the direct-INSERT sibling
//! already uses), which coerces the probe via `coerce_fk_probe_values`, queues
//! deferred checks, and audits. These tests fail on the pre-change binary and
//! pass after the delete-and-delegate.

use heliosdb_nano::EmbeddedDatabase;

/// int8 (BIGINT) parent PK referenced by an int4 (INTEGER) child FK column.
/// An INSERT..SELECT whose values genuinely exist in the parent must SUCCEED
/// (no phantom 23503).
#[test]
fn insert_select_cross_type_int8_parent_int4_child_no_phantom() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE parent8 (id BIGINT PRIMARY KEY)")
        .expect("create parent8");
    db.execute("INSERT INTO parent8 (id) VALUES (10), (20), (30)")
        .expect("seed parent8");

    // Child FK column is INTEGER (int4) referencing a BIGINT (int8) PK.
    db.execute("CREATE TABLE child4 (cid INTEGER PRIMARY KEY, pid INTEGER REFERENCES parent8(id))")
        .expect("create child4");

    // Staging holds values that all EXIST in parent8.
    db.execute("CREATE TABLE staging4 (cid INTEGER, pid INTEGER)")
        .expect("create staging4");
    db.execute("INSERT INTO staging4 (cid, pid) VALUES (1, 10), (2, 20), (3, 30)")
        .expect("seed staging4");

    // Pre-fix: this phantom-errored 23503 on every row (int4 probe key vs int8
    // parent key). Post-fix: the coerced probe matches, so it SUCCEEDS.
    let n = db
        .execute("INSERT INTO child4 (cid, pid) SELECT cid, pid FROM staging4")
        .expect("cross-type INSERT..SELECT with existing parents must not phantom-error");
    assert_eq!(n, 3, "all three matching rows should insert");

    let rows = db.query("SELECT cid FROM child4", &[]).expect("read back child4");
    assert_eq!(rows.len(), 3, "child4 should contain the 3 inserted rows");
}

/// The reverse width mismatch: int4 (INTEGER) parent PK referenced by an int8
/// (BIGINT) child FK column. Also must not phantom-error when values match.
#[test]
fn insert_select_cross_type_int4_parent_int8_child_no_phantom() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE parent4 (id INTEGER PRIMARY KEY)")
        .expect("create parent4");
    db.execute("INSERT INTO parent4 (id) VALUES (100), (200)")
        .expect("seed parent4");

    db.execute("CREATE TABLE child8 (cid INTEGER PRIMARY KEY, pid BIGINT REFERENCES parent4(id))")
        .expect("create child8");
    db.execute("CREATE TABLE staging8 (cid INTEGER, pid BIGINT)")
        .expect("create staging8");
    db.execute("INSERT INTO staging8 (cid, pid) VALUES (1, 100), (2, 200)")
        .expect("seed staging8");

    let n = db
        .execute("INSERT INTO child8 (cid, pid) SELECT cid, pid FROM staging8")
        .expect("reverse cross-type INSERT..SELECT must not phantom-error");
    assert_eq!(n, 2, "both matching rows should insert");
}

/// Real enforcement must be preserved: an INSERT..SELECT that references a
/// value genuinely ABSENT from the parent must still raise 23503. The fix only
/// removes the false positives from the width/type mismatch, not real checks.
#[test]
fn insert_select_cross_type_genuine_violation_still_errors() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE parent8 (id BIGINT PRIMARY KEY)")
        .expect("create parent8");
    db.execute("INSERT INTO parent8 (id) VALUES (10), (20)")
        .expect("seed parent8");
    db.execute("CREATE TABLE child4 (cid INTEGER PRIMARY KEY, pid INTEGER REFERENCES parent8(id))")
        .expect("create child4");

    // 999 is NOT in parent8.
    db.execute("CREATE TABLE staging_bad (cid INTEGER, pid INTEGER)")
        .expect("create staging_bad");
    db.execute("INSERT INTO staging_bad (cid, pid) VALUES (1, 999)")
        .expect("seed staging_bad");

    let res = db.execute("INSERT INTO child4 (cid, pid) SELECT cid, pid FROM staging_bad");
    assert!(
        res.is_err(),
        "INSERT..SELECT referencing an absent parent value must still error (23503)"
    );

    // And no orphan row leaked into the child.
    let rows = db.query("SELECT cid FROM child4", &[]).expect("read back child4");
    assert_eq!(rows.len(), 0, "the violating INSERT..SELECT must not persist any row");
}

/// Deferred-FK skip, fixed for free by the delegation. The old inline twin
/// honored only `enforcement == Immediate` and ignored deferral entirely, so an
/// INSERT..SELECT under deferred validation errored immediately instead of
/// queuing. After the fix it queues via `check_fk_constraints_on_write` and is
/// validated at COMMIT (by `validate_deferred_fk_checks`), so a parent that
/// arrives before COMMIT satisfies it.
#[test]
fn insert_select_deferred_fk_validated_at_commit() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE parent_d (id BIGINT PRIMARY KEY)")
        .expect("create parent_d");
    db.execute("CREATE TABLE child_d (cid INTEGER PRIMARY KEY, pid INTEGER REFERENCES parent_d(id))")
        .expect("create child_d");
    db.execute("CREATE TABLE staging_d (cid INTEGER, pid INTEGER)")
        .expect("create staging_d");
    db.execute("INSERT INTO staging_d (cid, pid) VALUES (1, 777)")
        .expect("seed staging_d");

    db.execute("SET helios.fk_validation = 'deferred'")
        .expect("enable deferred FK validation");

    db.execute("BEGIN").expect("begin");
    // pid=777 does not exist yet. Pre-fix the inline twin ran immediately and
    // errored here; post-fix the check is DEFERRED (queued), so this succeeds.
    db.execute("INSERT INTO child_d (cid, pid) SELECT cid, pid FROM staging_d")
        .expect("deferred cross-path INSERT..SELECT should not error immediately");
    // Parent arrives before COMMIT.
    db.execute("INSERT INTO parent_d (id) VALUES (777)")
        .expect("parent arrives before commit");
    db.execute("COMMIT")
        .expect("commit validates the queued FK; parent now exists");

    // A permanent orphan must fail deferred validation at COMMIT.
    db.execute("INSERT INTO staging_d (cid, pid) VALUES (2, 888)")
        .expect("seed orphan");
    db.execute("BEGIN").expect("begin 2");
    db.execute("INSERT INTO child_d (cid, pid) SELECT cid, pid FROM staging_d WHERE pid = 888")
        .expect("queued, not yet validated");
    let commit = db.execute("COMMIT");
    assert!(
        commit.is_err(),
        "a permanent orphan must fail deferred FK validation at COMMIT"
    );
    db.execute("ROLLBACK").ok();
}
