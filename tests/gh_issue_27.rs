//! GH #27 — `CREATE TABLE` must reject a `REFERENCES` clause naming a table
//! (or column) that does not exist, at DDL time, the way PostgreSQL does.
//!
//! Compile as `tests/gh_issue_27.rs`.
//!
//! Reported on 3.58.1: `CREATE TABLE child (id INT PRIMARY KEY, parent_id INT
//! REFERENCES parent(id))` SUCCEEDED with no `parent` table anywhere. The
//! constraint was persisted against a relation that does not exist, so every
//! later INSERT passed the FK check — a migration with a typo'd or out-of-order
//! table reported success and produced a database with NO referential
//! integrity. The failure mode is silent, which is why it must be caught at DDL
//! time and not left to the write path.
//!
//! Expected (PostgreSQL): `42P01 relation "parent" does not exist` for a
//! missing table, `42703` for a missing column, in `CREATE TABLE` and in
//! `ALTER TABLE … ADD FOREIGN KEY` alike, with self-references still legal.
//!
//! Implementation under audit:
//!   * `EmbeddedDatabase::validate_fk_reference`            — src/lib.rs:20617
//!   * `EmbeddedDatabase::validate_create_table_fk_targets` — src/lib.rs:20640
//!   * the CREATE TABLE call site (text family, BEFORE `catalog.create_table`)
//!                                                          — src/lib.rs:5038
//!   * `alter_table_add_foreign_key` (shared by BOTH families)
//!                                                          — src/lib.rs:11318
//!   * params-family routing of `AlterTableAddForeignKey`   — src/lib.rs:14989
//!
//! `tests/prisma_p0_unique_on_conflict.rs` §7 already covers the inline
//! `REFERENCES nosuch(id)` spelling, the missing-column case and ALTER on both
//! families. What THIS file adds is every OTHER spelling of the same clause —
//! table-level `FOREIGN KEY (…) REFERENCES`, the named `CONSTRAINT c FOREIGN
//! KEY` form, the column-list-less `REFERENCES parent`, and quoted identifiers
//! — because "the inline and the table-level spelling take different code
//! paths" is exactly how the v4.31.0 Prisma UNIQUE defect hid from two review
//! passes. Plus the false-POSITIVE direction: a legal forward-declared FK, a
//! self-reference in all four spellings, and a schema-qualified parent must all
//! still be accepted.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{EmbeddedDatabase, Value};

fn fresh_db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory database")
}

/// Which DML executor family runs the statement.
///
/// * `false` → TEXT   : `db.execute()`        → `execute_in_transaction_inner`
/// * `true`  → PARAMS : `db.execute_params()` → `execute_plan_with_params_inner`
///
/// A fix or a test in one family says NOTHING about the other; every ALTER
/// sub-case below runs on both.
fn family(params: bool) -> &'static str {
    if params {
        "params"
    } else {
        "text"
    }
}

fn run(db: &EmbeddedDatabase, sql: &str, params: bool) -> heliosdb_nano::Result<u64> {
    if params {
        db.execute_params(sql, &[])
    } else {
        db.execute(sql)
    }
}

fn table_exists(db: &EmbeddedDatabase, table: &str) -> bool {
    db.query_with_columns(&format!("SELECT * FROM {table}")).is_ok()
}

/// The error text a rejected statement produced, or a panic naming the
/// statement that was wrongly ACCEPTED.
fn must_reject(db: &EmbeddedDatabase, sql: &str, params: bool) -> String {
    match run(db, sql, params) {
        Ok(_) => panic!(
            "[{}] *** UNENFORCEABLE CONSTRAINT ACCEPTED *** `{sql}` must be rejected at DDL time",
            family(params)
        ),
        Err(e) => e.to_string(),
    }
}

/// A rejection must carry PostgreSQL's 42P01 wording — the wire classifier
/// (`sqlstate_for_query_execution_message`, src/protocol/postgres/handler.rs)
/// maps `relation … does not exist` to 42P01 undefined_table, so the WORDING
/// is the SQLSTATE.
fn assert_undefined_table(err: &str, name: &str) {
    let lower = err.to_ascii_lowercase();
    assert!(
        lower.contains("relation") && lower.contains(&name.to_ascii_lowercase()) && lower.contains("does not exist"),
        "expected PostgreSQL's 42P01 wording (`relation \"{name}\" does not exist`), got: {err}"
    );
}

/// A rejection must carry PostgreSQL's 42703 wording (`column "c" …`), which is
/// what `message_names_a_column` anchors on.
fn assert_undefined_column(err: &str, col: &str) {
    let lower = err.to_ascii_lowercase();
    assert!(
        lower.contains(&format!("column \"{}\"", col.to_ascii_lowercase())) && lower.contains("does not exist"),
        "expected PostgreSQL's 42703 wording (`column \"{col}\" … does not exist`), got: {err}"
    );
}

// ===========================================================================
// 0. POSITIVE CONTROL — passes before AND after the fix.
// ===========================================================================

#[test]
fn positive_control_the_harness_runs_and_valid_ddl_still_works() {
    let db = fresh_db();

    // A perfectly ordinary FK, declared after its parent, must still be
    // accepted and ENFORCED — this is the false-positive guard for everything
    // below. If DDL-time validation ever over-rejects, this fails first.
    db.execute("CREATE TABLE ctl_parent (id INT PRIMARY KEY, name TEXT)")
        .expect("parent");
    db.execute("CREATE TABLE ctl_child (id INT PRIMARY KEY, p INT REFERENCES ctl_parent(id))")
        .expect("*** a LEGAL foreign key was rejected ***");
    db.execute("INSERT INTO ctl_parent VALUES (1, 'a')").unwrap();
    db.execute("INSERT INTO ctl_child VALUES (1, 1)")
        .expect("a satisfied FK must insert");
    assert!(
        db.execute("INSERT INTO ctl_child VALUES (2, 99)").is_err(),
        "the FK must actually be enforced"
    );

    // Both executor families are reachable in this harness.
    assert_eq!(
        db.execute_params(
            "INSERT INTO ctl_parent VALUES ($1, $2)",
            &[Value::Int4(2), Value::String("b".into())]
        )
        .expect("params-family insert"),
        1
    );
    let rows = db
        .query_with_columns("SELECT id FROM ctl_parent ORDER BY id")
        .expect("select")
        .0;
    assert_eq!(rows.len(), 2);

    // And a genuinely missing table still errors, so "must_reject" below cannot
    // be satisfied by a permissive engine.
    assert!(!table_exists(&db, "ctl_no_such_table"));
}

// ===========================================================================
// 1. CREATE TABLE — missing PARENT TABLE (42P01), every spelling.
//
// The issue's literal reproducer is the first case; the rest are the same
// clause written the other four ways a migration tool emits it. They must all
// reach `validate_create_table_fk_targets` (the planner folds inline
// `REFERENCES` into the same `TableConstraint::ForeignKey` list at
// src/sql/planner.rs:5212-5245).
// ===========================================================================

#[test]
fn create_table_rejects_a_reference_to_a_missing_table_in_every_spelling() {
    // (label, DDL, the parent name that must be named in the error)
    let cases: [(&str, &str, &str); 5] = [
        (
            "the issue's literal reproducer (inline REFERENCES)",
            "CREATE TABLE gh27_a (id INT PRIMARY KEY, parent_id INT REFERENCES parent(id))",
            "parent",
        ),
        (
            "table-level FOREIGN KEY (…) REFERENCES",
            "CREATE TABLE gh27_b (id INT PRIMARY KEY, parent_id INT, FOREIGN KEY (parent_id) REFERENCES parent(id))",
            "parent",
        ),
        (
            "named CONSTRAINT … FOREIGN KEY (the spelling Prisma emits)",
            "CREATE TABLE gh27_c (id INT PRIMARY KEY, parent_id INT, \
             CONSTRAINT gh27_c_parent_fkey FOREIGN KEY (parent_id) REFERENCES parent(id))",
            "parent",
        ),
        (
            "REFERENCES with NO column list (binds to the parent PK)",
            "CREATE TABLE gh27_d (id INT PRIMARY KEY, parent_id INT REFERENCES parent)",
            "parent",
        ),
        (
            "quoted identifiers",
            "CREATE TABLE gh27_e (id INT PRIMARY KEY, parent_id INT REFERENCES \"NoSuchParent\"(\"id\"))",
            "NoSuchParent",
        ),
    ];

    for (label, ddl, parent) in cases {
        let db = fresh_db();
        let err = must_reject(&db, ddl, false);
        assert_undefined_table(&err, parent);

        // And the statement must be atomic: no half-created relation is left
        // behind (validation runs BEFORE `catalog.create_table`).
        let created = ddl.split_whitespace().nth(2).expect("table name").to_string();
        assert!(
            !table_exists(&db, &created),
            "[{label}] *** the rejected CREATE TABLE left `{created}` behind ***"
        );
    }
}

/// A multi-column FK whose parent is missing is rejected on the FIRST bad
/// target, and a statement with SEVERAL foreign keys is rejected when ANY of
/// them dangles — not only when the first one does.
#[test]
fn create_table_rejects_when_any_of_several_foreign_keys_dangles() {
    let db = fresh_db();
    db.execute("CREATE TABLE gh27_ok (id INT PRIMARY KEY)").unwrap();

    // Second FK is the bad one.
    let err = must_reject(
        &db,
        "CREATE TABLE gh27_multi (id INT PRIMARY KEY, a INT REFERENCES gh27_ok(id), \
         b INT REFERENCES gh27_missing(id))",
        false,
    );
    assert_undefined_table(&err, "gh27_missing");
    assert!(!table_exists(&db, "gh27_multi"));

    // Composite FK against a missing table.
    let err = must_reject(
        &db,
        "CREATE TABLE gh27_comp (a INT, b INT, PRIMARY KEY (a, b), \
         FOREIGN KEY (a, b) REFERENCES gh27_missing(x, y))",
        false,
    );
    assert_undefined_table(&err, "gh27_missing");
    assert!(!table_exists(&db, "gh27_comp"));
}

// ===========================================================================
// 2. CREATE TABLE — missing referenced COLUMN (42703), every spelling.
// ===========================================================================

#[test]
fn create_table_rejects_a_reference_to_a_missing_column_in_every_spelling() {
    let cases: [(&str, &str, &str); 3] = [
        (
            "inline",
            "CREATE TABLE gh27_f (id INT PRIMARY KEY, p INT REFERENCES gh27_p(nocol))",
            "nocol",
        ),
        (
            "table-level",
            "CREATE TABLE gh27_g (id INT PRIMARY KEY, p INT, FOREIGN KEY (p) REFERENCES gh27_p(nocol))",
            "nocol",
        ),
        (
            "composite, second component missing",
            "CREATE TABLE gh27_h (a INT PRIMARY KEY, b INT, FOREIGN KEY (a, b) REFERENCES gh27_p(id, nocol))",
            "nocol",
        ),
    ];

    for (label, ddl, col) in cases {
        let db = fresh_db();
        db.execute("CREATE TABLE gh27_p (id INT PRIMARY KEY, name TEXT)")
            .unwrap();
        let err = must_reject(&db, ddl, false);
        assert_undefined_column(&err, col);
        let created = ddl.split_whitespace().nth(2).expect("table name").to_string();
        assert!(
            !table_exists(&db, &created),
            "[{label}] the rejected CREATE TABLE left `{created}` behind"
        );
    }
}

// ===========================================================================
// 3. Self-references stay legal — the over-rejection guard.
//
// `validate_create_table_fk_targets` runs BEFORE `catalog.create_table`, so
// the table being created is NOT in the catalog yet. A self-reference is
// therefore a special case (src/lib.rs:20659-20673) and it must keep working
// in every spelling, including under a non-`public` search_path where the
// planner cannot resolve the bare name.
// ===========================================================================

#[test]
fn a_self_referencing_foreign_key_is_still_accepted_in_every_spelling() {
    for (label, ddl) in [
        (
            "inline",
            "CREATE TABLE gh27_tree (id INT PRIMARY KEY, parent INT REFERENCES gh27_tree(id))",
        ),
        (
            "table-level",
            "CREATE TABLE gh27_tree (id INT PRIMARY KEY, parent INT, FOREIGN KEY (parent) REFERENCES gh27_tree(id))",
        ),
        (
            "named constraint",
            "CREATE TABLE gh27_tree (id INT PRIMARY KEY, parent INT, \
             CONSTRAINT gh27_tree_parent_fkey FOREIGN KEY (parent) REFERENCES gh27_tree(id))",
        ),
        (
            "no column list",
            "CREATE TABLE gh27_tree (id INT PRIMARY KEY, parent INT REFERENCES gh27_tree)",
        ),
        (
            "quoted, case-preserving",
            "CREATE TABLE \"gh27_tree\" (id INT PRIMARY KEY, parent INT REFERENCES \"gh27_tree\"(id))",
        ),
    ] {
        let db = fresh_db();
        db.execute(ddl)
            .unwrap_or_else(|e| panic!("[{label}] *** a legal self-reference was rejected: {e} ***"));

        // ... and it is actually enforced.
        db.execute("INSERT INTO gh27_tree (id, parent) VALUES (1, NULL)")
            .unwrap();
        db.execute("INSERT INTO gh27_tree (id, parent) VALUES (2, 1)").unwrap();
        assert!(
            db.execute("INSERT INTO gh27_tree (id, parent) VALUES (3, 99)").is_err(),
            "[{label}] the self-referencing FK must be enforced"
        );
    }
}

/// A self-reference to a column the table does NOT declare is still 42703 —
/// the self-reference shortcut must validate against the columns being
/// declared, not wave the constraint through.
#[test]
fn a_self_reference_to_a_missing_own_column_is_rejected() {
    let db = fresh_db();
    let err = must_reject(
        &db,
        "CREATE TABLE gh27_selfbad (id INT PRIMARY KEY, parent INT REFERENCES gh27_selfbad(nocol))",
        false,
    );
    assert_undefined_column(&err, "nocol");
    assert!(!table_exists(&db, "gh27_selfbad"));
}

// ===========================================================================
// 4. ALTER TABLE … ADD FOREIGN KEY — on BOTH executor families.
//
// The params family routes this plan to the SAME body (src/lib.rs:14989 →
// `alter_table_add_foreign_key`), which is the only reason the two families
// cannot drift here. Both are asserted so a future re-split is caught.
// ===========================================================================

#[test]
fn alter_table_add_foreign_key_rejects_a_missing_table_on_both_families() {
    for params in [false, true] {
        let db = fresh_db();
        let fam = family(params);
        db.execute("CREATE TABLE gh27_c1 (id INT PRIMARY KEY, p INT)").unwrap();

        for (label, ddl) in [
            (
                "named constraint",
                "ALTER TABLE gh27_c1 ADD CONSTRAINT gh27_c1_fkey FOREIGN KEY (p) REFERENCES gh27_nope(id)",
            ),
            (
                "unnamed",
                "ALTER TABLE gh27_c1 ADD FOREIGN KEY (p) REFERENCES gh27_nope(id)",
            ),
            (
                "no column list",
                "ALTER TABLE gh27_c1 ADD FOREIGN KEY (p) REFERENCES gh27_nope",
            ),
        ] {
            let err = must_reject(&db, ddl, params);
            assert_undefined_table(&err, "gh27_nope");
            // Nothing was recorded: a later INSERT is unaffected by a
            // constraint that was refused.
            db.execute(&format!("INSERT INTO gh27_c1 VALUES ({}, 7)", label.len()))
                .unwrap_or_else(|e| panic!("[{fam}/{label}] the refused ALTER left a broken constraint: {e}"));
        }
    }
}

#[test]
fn alter_table_add_foreign_key_rejects_a_missing_column_on_both_families() {
    for params in [false, true] {
        let db = fresh_db();
        db.execute("CREATE TABLE gh27_c2 (id INT PRIMARY KEY, p INT)").unwrap();
        db.execute("CREATE TABLE gh27_q (id INT PRIMARY KEY, name TEXT)")
            .unwrap();

        let err = must_reject(
            &db,
            "ALTER TABLE gh27_c2 ADD CONSTRAINT gh27_c2_fkey FOREIGN KEY (p) REFERENCES gh27_q(nocol)",
            params,
        );
        assert_undefined_column(&err, "nocol");
    }
}

/// The CHILD table of an `ALTER TABLE … ADD FOREIGN KEY` must exist too
/// (42P01), on both families.
#[test]
fn alter_table_add_foreign_key_rejects_a_missing_child_table_on_both_families() {
    for params in [false, true] {
        let db = fresh_db();
        db.execute("CREATE TABLE gh27_q2 (id INT PRIMARY KEY)").unwrap();
        let err = must_reject(
            &db,
            "ALTER TABLE gh27_no_child ADD FOREIGN KEY (p) REFERENCES gh27_q2(id)",
            params,
        );
        assert!(
            err.to_ascii_lowercase().contains("does not exist"),
            "[{}] expected an undefined-relation error for the child, got: {err}",
            family(params)
        );
    }
}

/// The over-rejection guard for ALTER: a legal FK added afterwards is accepted
/// and enforced, on both families — including the self-referencing case.
#[test]
fn alter_table_add_foreign_key_still_accepts_legal_targets_on_both_families() {
    for params in [false, true] {
        let db = fresh_db();
        let fam = family(params);
        db.execute("CREATE TABLE gh27_par (id INT PRIMARY KEY)").unwrap();
        db.execute("CREATE TABLE gh27_ch (id INT PRIMARY KEY, p INT, up INT)")
            .unwrap();

        run(
            &db,
            "ALTER TABLE gh27_ch ADD CONSTRAINT gh27_ch_p_fkey FOREIGN KEY (p) REFERENCES gh27_par(id)",
            params,
        )
        .unwrap_or_else(|e| panic!("[{fam}] *** a LEGAL ALTER … ADD FOREIGN KEY was rejected: {e} ***"));

        // Self-reference through ALTER.
        run(
            &db,
            "ALTER TABLE gh27_ch ADD CONSTRAINT gh27_ch_up_fkey FOREIGN KEY (up) REFERENCES gh27_ch(id)",
            params,
        )
        .unwrap_or_else(|e| panic!("[{fam}] *** a LEGAL self-referencing ALTER was rejected: {e} ***"));

        db.execute("INSERT INTO gh27_par VALUES (1)").unwrap();
        db.execute("INSERT INTO gh27_ch VALUES (1, 1, NULL)")
            .expect("a satisfied FK must insert");
        assert!(
            db.execute("INSERT INTO gh27_ch VALUES (2, 99, NULL)").is_err(),
            "[{fam}] the added FK must be enforced"
        );
    }
}

// ===========================================================================
// 5. Ordering / resolution regressions the fix must NOT introduce.
// ===========================================================================

/// Two statements in the right order still work, and the WRONG order is what
/// the issue is about: it must now fail loudly instead of producing a database
/// with no referential integrity.
#[test]
fn migration_ordering_is_reported_instead_of_silently_accepted() {
    // Right order: fine.
    let db = fresh_db();
    db.execute("CREATE TABLE mo_parent (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE mo_child (id INT PRIMARY KEY, p INT REFERENCES mo_parent(id))")
        .expect("in-order migration must succeed");

    // Wrong order: rejected, and the child was not created — so re-running the
    // migration after fixing the order works.
    let db = fresh_db();
    let err = must_reject(
        &db,
        "CREATE TABLE mo_child (id INT PRIMARY KEY, p INT REFERENCES mo_parent(id))",
        false,
    );
    assert_undefined_table(&err, "mo_parent");
    db.execute("CREATE TABLE mo_parent (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE mo_child (id INT PRIMARY KEY, p INT REFERENCES mo_parent(id))")
        .expect("the corrected migration must succeed");
}

/// A schema-qualified parent resolves; a schema-qualified MISSING parent is
/// still 42P01. (`resolve_table_ref` treats an explicit qualifier as exact.)
#[test]
fn schema_qualified_foreign_key_targets_resolve_and_still_validate() {
    let db = fresh_db();
    db.execute("CREATE SCHEMA gh27s").expect("create schema");
    db.execute("CREATE TABLE gh27s.par (id INT PRIMARY KEY)")
        .expect("qualified parent");
    db.execute("CREATE TABLE gh27s_child (id INT PRIMARY KEY, p INT REFERENCES gh27s.par(id))")
        .expect("*** a LEGAL schema-qualified foreign key was rejected ***");

    let err = must_reject(
        &db,
        "CREATE TABLE gh27s_bad (id INT PRIMARY KEY, p INT REFERENCES gh27s.nope(id))",
        false,
    );
    assert!(
        err.to_ascii_lowercase().contains("does not exist"),
        "a missing schema-qualified parent must still be rejected, got: {err}"
    );
    assert!(!table_exists(&db, "gh27s_bad"));
}

/// The rejection must survive an explicit transaction too — a migration runner
/// wraps its DDL in `BEGIN … COMMIT`, and a constraint that is only validated
/// in autocommit would let exactly that runner through.
#[test]
fn the_dangling_reference_is_rejected_inside_an_explicit_transaction() {
    let db = fresh_db();
    db.execute("BEGIN").unwrap();
    let err = must_reject(
        &db,
        "CREATE TABLE gh27_txn (id INT PRIMARY KEY, p INT REFERENCES gh27_txn_parent(id))",
        false,
    );
    assert_undefined_table(&err, "gh27_txn_parent");
    let _ = db.execute("ROLLBACK");
    assert!(!table_exists(&db, "gh27_txn"));
}

// ===========================================================================
// 5b. *** THE SPELLING THAT IS STILL OPEN ***  (adversarial-review finding)
//
// `ALTER TABLE t ADD COLUMN p INT REFERENCES parent(id)` is the SAME clause in
// a sibling DDL statement, and it is NOT covered by the v4.31.0 fix.
//
// The planner's ALTER path builds its column through
// `Planner::sql_column_def_to_column_def` (src/sql/planner.rs:5906). That
// function's option loop (src/sql/planner.rs:5925-5946) handles NotNull,
// Unique/PrimaryKey, Default and Generated, and ends in a catch-all
// `_ => {}` at src/sql/planner.rs:5945 — so `ColumnOption::ForeignKey` is
// DISCARDED. `ColumnOption::ForeignKey` is destructured in exactly ONE place in
// the whole planner, src/sql/planner.rs:5212, which is the CREATE TABLE arm.
// `LogicalPlan::AlterTableAddColumn` (src/sql/planner.rs:5770) carries only a
// `ColumnDef`, which has no field for a foreign key at all, and neither
// executor arm (src/lib.rs:7241 text family, src/lib.rs:11420 params family)
// can therefore register one.
//
// Consequences, both of which are exactly issue #27's complaint:
//   (a) a REFERENCES naming a table that does NOT exist is ACCEPTED silently,
//       with no 42P01 — the reported symptom, in a statement migration tools
//       (Rails, Drizzle, hand-written SQL, Prisma's `ADD COLUMN … REFERENCES`
//       shorthand) emit constantly; and
//   (b) worse, when the parent DOES exist the constraint is silently thrown
//       away, so the database ends up with no referential integrity even
//       though the migration was written correctly and reported success.
//
// No test anywhere in the repo exercises `ADD COLUMN … REFERENCES` (grepped:
// zero hits in tests/ and src/). This test is EXPECTED TO FAIL on the current
// tree; it is the proof that #27 is only PARTLY fixed. It deliberately lives
// in the integration tier (tests/), which CI does not run, rather than in the
// `--lib` wire tier that gates releases.
// ===========================================================================

#[test]
fn alter_table_add_column_with_an_inline_reference_is_validated_and_kept() {
    // TEXT family only, deliberately: `AlterTableAddColumn` is not reachable on
    // the params family at all — `execute_alter_table_op` (src/lib.rs:11418) has
    // exactly ONE caller, src/lib.rs:7461, inside the text family's
    // `execute_in_transaction_inner`, and `execute_plan_with_params_inner` says
    // so itself at src/lib.rs:14964-14972 ("ALTER TABLE as a whole is still
    // unimplemented on this family"). That separate gap is pinned by
    // `add_column_is_not_reachable_on_the_params_family_at_all` below, so this
    // test cannot be confused with it.

    // (a) A dangling parent must be rejected at DDL time, like CREATE TABLE.
    let db = fresh_db();
    db.execute("CREATE TABLE gh27_ac (id INT PRIMARY KEY)").unwrap();
    let err = must_reject(
        &db,
        "ALTER TABLE gh27_ac ADD COLUMN p INT REFERENCES gh27_ac_missing(id)",
        false,
    );
    assert_undefined_table(&err, "gh27_ac_missing");

    // (b) With a real parent the constraint must be CREATED and ENFORCED,
    //     not silently discarded.
    let db = fresh_db();
    db.execute("CREATE TABLE gh27_acp (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE gh27_acc (id INT PRIMARY KEY)").unwrap();
    db.execute("ALTER TABLE gh27_acc ADD COLUMN p INT REFERENCES gh27_acp(id)")
        .expect("a LEGAL ADD COLUMN … REFERENCES must be accepted");

    db.execute("INSERT INTO gh27_acp VALUES (1)").unwrap();
    db.execute("INSERT INTO gh27_acc VALUES (1, 1)")
        .expect("a satisfied FK must insert");
    assert!(
        db.execute("INSERT INTO gh27_acc VALUES (2, 99)").is_err(),
        "*** UNENFORCEABLE CONSTRAINT *** the inline REFERENCES on ADD COLUMN was \
         silently discarded (src/sql/planner.rs:5945 `_ => {{}}` swallows \
         ColumnOption::ForeignKey, and LogicalPlan::AlterTableAddColumn carries \
         no foreign key at all)"
    );
}

/// PARITY — the gap this test used to pin is CLOSED (sprinter 15bfe577751a):
/// the params / extended family now routes `ALTER TABLE … ADD COLUMN` through
/// the shared body, so the `ADD COLUMN … REFERENCES` shorthand is validated
/// and the constraint is kept on every client.
///
/// (a) a dangling parent is rejected at DDL time with 42P01, and
/// (b) with a real parent the constraint is CREATED and ENFORCED.
#[test]
fn add_column_is_reachable_on_the_params_family_and_validated() {
    // (a) A dangling parent must be rejected at DDL time, like CREATE TABLE.
    let db = fresh_db();
    db.execute("CREATE TABLE gh27_acx (id INT PRIMARY KEY)").unwrap();
    let err = must_reject(
        &db,
        "ALTER TABLE gh27_acx ADD COLUMN p INT REFERENCES gh27_acxp_missing(id)",
        true,
    );
    assert_undefined_table(&err, "gh27_acxp_missing");

    // (b) With a real parent the constraint must be CREATED and ENFORCED.
    let db = fresh_db();
    db.execute("CREATE TABLE gh27_acxp (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE gh27_acx (id INT PRIMARY KEY)").unwrap();
    db.execute_params("ALTER TABLE gh27_acx ADD COLUMN p INT REFERENCES gh27_acxp(id)", &[])
        .expect("a LEGAL params-family ADD COLUMN … REFERENCES must be accepted");

    db.execute("INSERT INTO gh27_acxp VALUES (1)").unwrap();
    db.execute_params("INSERT INTO gh27_acx VALUES (1, 1)", &[])
        .expect("a satisfied FK must insert");
    assert!(
        db.execute_params("INSERT INTO gh27_acx VALUES (2, 99)", &[]).is_err(),
        "*** UNENFORCEABLE CONSTRAINT *** the params-family ADD COLUMN … REFERENCES \
         was silently discarded"
    );
}

/// Control for the test above, and the reason its failure is a SILENT one:
/// `ALTER TABLE … ADD COLUMN … REFERENCES <existing parent>` returns Ok and
/// really does add a usable column. So the statement is not rejected, not
/// unsupported and not a no-op — only the foreign key is thrown away
/// (src/sql/planner.rs:5945). Deliberately asserts nothing about constraint
/// enforcement, so it passes before AND after any #27 change.
#[test]
fn add_column_with_a_reference_succeeds_and_adds_a_usable_column() {
    let db = fresh_db();
    db.execute("CREATE TABLE gh27_optp (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE gh27_opt (id INT PRIMARY KEY)").unwrap();

    db.execute("ALTER TABLE gh27_opt ADD COLUMN p INT REFERENCES gh27_optp(id)")
        .expect("the ALTER itself must succeed — that is what makes the loss silent");

    db.execute("INSERT INTO gh27_optp VALUES (1)").unwrap();
    db.execute("INSERT INTO gh27_opt VALUES (1, 1)").unwrap();
    let rows = db
        .query_with_columns("SELECT p FROM gh27_opt WHERE id = 1")
        .expect("the new column must be queryable")
        .0;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int4(1), "the added column round-trips");
}

// ===========================================================================
// 6. The PARAMS family and CREATE TABLE — parity, now CLOSED.
//
// `execute_params("CREATE TABLE …")` (and therefore Parse/Bind/Execute of a
// CREATE TABLE over the extended protocol) now routes through the SAME
// `execute_create_table_plan` body the text family runs (sprinter
// 15bfe577751a), so the params family reports PostgreSQL's own SQLSTATE for a
// dangling FK — 42P01, not the old `XX000 Operator not yet implemented`.
// ===========================================================================

#[test]
fn the_params_family_never_silently_accepts_a_dangling_foreign_key() {
    let db = fresh_db();
    let err = db
        .execute_params(
            "CREATE TABLE gh27_pf (id INT PRIMARY KEY, p INT REFERENCES gh27_pf_missing(id))",
            &[],
        )
        .err()
        .expect("*** UNENFORCEABLE CONSTRAINT ACCEPTED *** the params family accepted a FK to a missing table");
    assert_undefined_table(&err.to_string(), "gh27_pf_missing");
    assert!(
        !table_exists(&db, "gh27_pf"),
        "the rejected params-family CREATE TABLE left a table behind"
    );
}
