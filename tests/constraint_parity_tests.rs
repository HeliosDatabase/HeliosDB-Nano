//! Constraint-enforcement parity between the two DML executor families.
//!
//! HeliosDB Nano has two parallel DML executors:
//!   * "text family"   — `db.execute()`        → `execute_in_transaction_inner`
//!                        (psql simple-query, MySQL wire)
//!   * "params family" — `db.execute_params()` → `execute_params_inner`
//!                        → `execute_plan_with_params_inner`
//!                        (PG extended protocol: psycopg server-side bind,
//!                         JDBC, sqlx, Drizzle, node-postgres; plus REST/BaaS)
//!
//! The params family had drifted and lost enforcement the text family has:
//!   * parameterized DELETE ran no referencing-FK logic at all, so
//!     `ON DELETE CASCADE` never fired (orphans), `ON DELETE SET NULL` never
//!     nulled, and `ON DELETE RESTRICT` silently permitted the delete;
//!   * parameterized UPDATE ran no UNIQUE / PRIMARY KEY enforcement.
//! And in the opposite direction, `ON CONFLICT DO UPDATE` re-validated almost
//! nothing on the TEXT side (a `CHECK (qty > 0)` column accepted
//! `DO UPDATE SET qty = -99`) while the params side had only CHECK.
//!
//! Every test below runs the SAME logical statement through BOTH families and
//! asserts they agree — and that the agreed-on answer is the correct one.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

fn count(db: &EmbeddedDatabase, sql: &str) -> usize {
    db.query(sql, &[]).unwrap().len()
}

/// Fetch a single scalar (row 0, column 0), or `None` when no row matched.
fn scalar(db: &EmbeddedDatabase, sql: &str) -> Option<Value> {
    db.query(sql, &[]).unwrap().first().and_then(|row| row.get(0).cloned())
}

// ---------------------------------------------------------------------------
// FIX A — parameterized DELETE must enforce referencing foreign keys
// ---------------------------------------------------------------------------

/// ON DELETE CASCADE must remove the child row on BOTH families.
#[test]
fn f2_params_delete_honors_referencing_fk_cascade() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
        db.execute("CREATE TABLE c (id INT PRIMARY KEY, p_id INT REFERENCES p(id) ON DELETE CASCADE)")
            .unwrap();
        db.execute("INSERT INTO p VALUES (1)").unwrap();
        db.execute("INSERT INTO c VALUES (10, 1)").unwrap();
    }

    // --- text family control ---
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup(&db);
    db.execute("DELETE FROM p WHERE id = 1").unwrap();
    let text_children = count(&db, "SELECT id FROM c");

    // --- params family: identical statement, bound parameter ---
    let db2 = EmbeddedDatabase::new_in_memory().unwrap();
    setup(&db2);
    db2.execute_params("DELETE FROM p WHERE id = $1", &[Value::Int4(1)])
        .unwrap();
    let params_children = count(&db2, "SELECT id FROM c");

    assert_eq!(
        params_children, text_children,
        "DIVERGENCE: text family left {text_children} child row(s), params family left \
         {params_children}. Non-zero on the params side = orphaned row, CASCADE never fired."
    );
    assert_eq!(text_children, 0, "ON DELETE CASCADE must remove the child row");
}

/// ON DELETE RESTRICT must reject the parent delete on BOTH families.
#[test]
fn f2_params_delete_honors_referencing_fk_restrict() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
        db.execute("CREATE TABLE c (id INT PRIMARY KEY, p_id INT REFERENCES p(id) ON DELETE RESTRICT)")
            .unwrap();
        db.execute("INSERT INTO p VALUES (1)").unwrap();
        db.execute("INSERT INTO c VALUES (10, 1)").unwrap();
    }

    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup(&db);
    let text_err = db.execute("DELETE FROM p WHERE id = 1").is_err();
    let text_parents = count(&db, "SELECT id FROM p");

    let db2 = EmbeddedDatabase::new_in_memory().unwrap();
    setup(&db2);
    let params_err = db2
        .execute_params("DELETE FROM p WHERE id = $1", &[Value::Int4(1)])
        .is_err();
    let params_parents = count(&db2, "SELECT id FROM p");

    assert_eq!(
        params_err, text_err,
        "DIVERGENCE: text family rejected={text_err}, params family rejected={params_err}. \
         params=false = RESTRICT silently permitted, leaving an orphan."
    );
    assert!(text_err, "ON DELETE RESTRICT must reject the parent delete");
    assert_eq!(
        params_parents, text_parents,
        "both families must leave the parent row in place after a rejected DELETE"
    );
    assert_eq!(text_parents, 1, "the rejected DELETE must not have removed the parent");
}

/// ON DELETE SET NULL must null the child's FK column on BOTH families
/// (child row survives, parent goes away).
#[test]
fn f2_params_delete_set_null() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
        db.execute("CREATE TABLE c (id INT PRIMARY KEY, p_id INT REFERENCES p(id) ON DELETE SET NULL)")
            .unwrap();
        db.execute("INSERT INTO p VALUES (1)").unwrap();
        db.execute("INSERT INTO c VALUES (10, 1)").unwrap();
    }

    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup(&db);
    db.execute("DELETE FROM p WHERE id = 1").unwrap();
    let text_children = count(&db, "SELECT id FROM c");
    let text_fk = scalar(&db, "SELECT p_id FROM c WHERE id = 10");

    let db2 = EmbeddedDatabase::new_in_memory().unwrap();
    setup(&db2);
    db2.execute_params("DELETE FROM p WHERE id = $1", &[Value::Int4(1)])
        .unwrap();
    let params_children = count(&db2, "SELECT id FROM c");
    let params_fk = scalar(&db2, "SELECT p_id FROM c WHERE id = 10");

    assert_eq!(
        params_children, text_children,
        "DIVERGENCE: text family kept {text_children} child row(s), params family kept \
         {params_children}. SET NULL must keep the child row, not delete it."
    );
    assert_eq!(text_children, 1, "ON DELETE SET NULL must keep the child row");
    assert_eq!(
        format!("{params_fk:?}"),
        format!("{text_fk:?}"),
        "DIVERGENCE: text family left c.p_id = {text_fk:?}, params family left {params_fk:?}"
    );
    assert!(
        matches!(text_fk.as_ref(), Some(Value::Null)),
        "ON DELETE SET NULL must null the child FK column; got {text_fk:?}"
    );
}

// ---------------------------------------------------------------------------
// FIX B — parameterized UPDATE must enforce UNIQUE / PRIMARY KEY
// ---------------------------------------------------------------------------

/// An UPDATE that moves a row onto another row's UNIQUE value must be rejected
/// by BOTH families.
#[test]
fn f5_params_update_enforces_unique() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute("CREATE TABLE users (id INT PRIMARY KEY, email TEXT UNIQUE)")
            .unwrap();
        db.execute("INSERT INTO users VALUES (1, 'a@x')").unwrap();
        db.execute("INSERT INTO users VALUES (2, 'b@x')").unwrap();
    }

    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup(&db);
    let text_err = db.execute("UPDATE users SET email = 'a@x' WHERE id = 2").is_err();
    let text_dupes = count(&db, "SELECT id FROM users WHERE email = 'a@x'");

    let db2 = EmbeddedDatabase::new_in_memory().unwrap();
    setup(&db2);
    let params_err = db2
        .execute_params(
            "UPDATE users SET email = $1 WHERE id = $2",
            &[Value::String("a@x".into()), Value::Int4(2)],
        )
        .is_err();
    let params_dupes = count(&db2, "SELECT id FROM users WHERE email = 'a@x'");

    assert_eq!(
        params_err, text_err,
        "DIVERGENCE: text rejected={text_err}, params rejected={params_err} \
         ({params_dupes} rows now share the UNIQUE email on the params side)."
    );
    assert!(text_err, "UPDATE onto an existing UNIQUE value must be rejected");
    assert_eq!(
        params_dupes, text_dupes,
        "DIVERGENCE: text left {text_dupes} row(s) with email='a@x', params left {params_dupes}"
    );
    assert_eq!(text_dupes, 1, "the rejected UPDATE must not have created a duplicate");
}

/// The self-collision guard: `unique_key_exists` probes an index that still
/// contains the row being updated, so a no-op `SET email = <the value it
/// already holds>` must NOT be reported as a duplicate. Both families.
///
/// This is the most likely way the FIX B enforcement could break real users,
/// so it is asserted explicitly.
#[test]
fn f5_update_same_value_is_not_a_false_positive() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute("CREATE TABLE users (id INT PRIMARY KEY, email TEXT UNIQUE)")
            .unwrap();
        db.execute("INSERT INTO users VALUES (1, 'a@x')").unwrap();
        db.execute("INSERT INTO users VALUES (2, 'b@x')").unwrap();
    }

    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup(&db);
    let text_res = db.execute("UPDATE users SET email = 'a@x' WHERE id = 1");
    let text_ok = text_res.is_ok();
    let text_email = scalar(&db, "SELECT email FROM users WHERE id = 1");
    let text_dupes = count(&db, "SELECT id FROM users WHERE email = 'a@x'");

    let db2 = EmbeddedDatabase::new_in_memory().unwrap();
    setup(&db2);
    let params_res = db2.execute_params(
        "UPDATE users SET email = $1 WHERE id = $2",
        &[Value::String("a@x".into()), Value::Int4(1)],
    );
    let params_ok = params_res.is_ok();
    let params_email = scalar(&db2, "SELECT email FROM users WHERE id = 1");
    let params_dupes = count(&db2, "SELECT id FROM users WHERE email = 'a@x'");

    assert_eq!(
        params_ok, text_ok,
        "DIVERGENCE: text ok={text_ok} ({text_res:?}), params ok={params_ok} ({params_res:?})"
    );
    assert!(
        text_ok,
        "re-assigning a UNIQUE column its OWN current value must succeed, not self-collide; \
         got {text_res:?}"
    );
    assert_eq!(
        format!("{params_email:?}"),
        format!("{text_email:?}"),
        "DIVERGENCE: text left email={text_email:?}, params left {params_email:?}"
    );
    assert_eq!(text_dupes, 1, "the no-op UPDATE must not have duplicated the row");
    assert_eq!(params_dupes, 1, "the no-op UPDATE must not have duplicated the row");
}

// ---------------------------------------------------------------------------
// FIX C — ON CONFLICT DO UPDATE must re-validate the post-update row
// ---------------------------------------------------------------------------

/// `ON CONFLICT DO UPDATE` producing a row that violates a CHECK constraint
/// must be rejected by BOTH families. (The TEXT family was the broken one
/// here — it accepted `SET qty = -99` against `CHECK (qty > 0)`.)
#[test]
fn f8_on_conflict_do_update_revalidates_check() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, qty INT CHECK (qty > 0))")
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 5)").unwrap();
    }

    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup(&db);
    let qty_before = format!("{:?}", scalar(&db, "SELECT qty FROM t WHERE id = 1"));
    let text_err = db
        .execute("INSERT INTO t VALUES (1, 5) ON CONFLICT (id) DO UPDATE SET qty = -99")
        .is_err();
    let text_qty = scalar(&db, "SELECT qty FROM t WHERE id = 1");

    let db2 = EmbeddedDatabase::new_in_memory().unwrap();
    setup(&db2);
    let params_err = db2
        .execute_params(
            "INSERT INTO t VALUES (1, $1) ON CONFLICT (id) DO UPDATE SET qty = -99",
            &[Value::Int4(5)],
        )
        .is_err();
    let params_qty = scalar(&db2, "SELECT qty FROM t WHERE id = 1");

    assert_eq!(
        params_err, text_err,
        "DIVERGENCE: text rejected={text_err} (qty now {text_qty:?}), \
         params rejected={params_err} (qty now {params_qty:?})"
    );
    assert!(
        text_err,
        "CHECK (qty > 0) must reject a DO UPDATE SET qty = -99; \
         it was accepted and the row now holds qty={text_qty:?}."
    );
    assert_eq!(
        format!("{params_qty:?}"),
        format!("{text_qty:?}"),
        "DIVERGENCE: after the rejected upsert text holds qty={text_qty:?}, params holds {params_qty:?}"
    );
    assert_eq!(
        format!("{text_qty:?}"),
        qty_before,
        "the rejected upsert must have left the original qty untouched; got {text_qty:?}"
    );
}

/// `ON CONFLICT DO UPDATE` that points an FK column at a non-existent parent
/// must be rejected by BOTH families.
#[test]
fn f8_on_conflict_do_update_revalidates_fk() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
        db.execute("CREATE TABLE c (id INT PRIMARY KEY, p_id INT REFERENCES p(id))")
            .unwrap();
        db.execute("INSERT INTO p VALUES (1)").unwrap();
        db.execute("INSERT INTO c VALUES (10, 1)").unwrap();
    }

    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup(&db);
    let fk_before = format!("{:?}", scalar(&db, "SELECT p_id FROM c WHERE id = 10"));
    let text_err = db
        .execute("INSERT INTO c VALUES (10, 1) ON CONFLICT (id) DO UPDATE SET p_id = 999")
        .is_err();
    let text_fk = scalar(&db, "SELECT p_id FROM c WHERE id = 10");

    let db2 = EmbeddedDatabase::new_in_memory().unwrap();
    setup(&db2);
    let params_err = db2
        .execute_params(
            "INSERT INTO c VALUES (10, $1) ON CONFLICT (id) DO UPDATE SET p_id = 999",
            &[Value::Int4(1)],
        )
        .is_err();
    let params_fk = scalar(&db2, "SELECT p_id FROM c WHERE id = 10");

    assert_eq!(
        params_err, text_err,
        "DIVERGENCE: text rejected={text_err} (p_id now {text_fk:?}), \
         params rejected={params_err} (p_id now {params_fk:?})"
    );
    assert!(
        text_err,
        "a DO UPDATE that points an FK column at a non-existent parent must be rejected; \
         the row now holds p_id={text_fk:?}"
    );
    assert_eq!(
        format!("{params_fk:?}"),
        format!("{text_fk:?}"),
        "DIVERGENCE: after the rejected upsert text holds p_id={text_fk:?}, params holds {params_fk:?}"
    );
    assert_eq!(
        format!("{text_fk:?}"),
        fk_before,
        "the rejected upsert must have left the original p_id untouched; got {text_fk:?}"
    );
}
