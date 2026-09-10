//! GH #27 (residual) — the two `ALTER TABLE` spellings of a `REFERENCES`
//! clause that `tests/gh_issue_27.rs` found still open after v4.31.0:
//!
//!   1. `ALTER TABLE t ADD COLUMN c T REFERENCES p[(cols)]` — the planner
//!      discarded `ColumnOption::ForeignKey` on ADD COLUMN, so a dangling
//!      parent was ACCEPTED and a real one was silently NOT enforced;
//!   2. `ALTER TABLE t ADD [CONSTRAINT n] FOREIGN KEY (c) REFERENCES p` with
//!      NO referenced-column list — a hard parse error (sqlparser 0.53 makes
//!      the list mandatory on the table-level production only).
//!
//! Underneath both sits the default itself: a list-less `REFERENCES p` binds
//! to `p`'s PRIMARY KEY, and `resolve_fk_referenced_columns` used to persist
//! an EMPTY list when `p` had none — a constraint enforced by nothing, #27's
//! defect through another door. PostgreSQL rejects that with 42704 undefined_object
//! `there is no primary key for referenced table "p"`, and rejects a
//! referencing/referenced column-count mismatch with 42830 `number of
//! referencing and referenced columns for foreign key disagree`.
//!
//! This file pins only the shapes `tests/gh_issue_27.rs` does NOT: the
//! key-less parent in every spelling, the composite-PK default, arity, the
//! self-reference through ADD COLUMN, multi-operation ALTERs (a nested
//! `AlterTableMulti` would otherwise be an internal error), atomicity of the
//! rejected ADD COLUMN, and both executor families wherever a statement
//! reaches both.
//!
//! Implementation under test:
//!   * `Parser::rewrite_fk_default_referenced_columns` /
//!     `strip_fk_default_pk_sentinel`               — src/sql/parser.rs
//!   * `Planner::inline_reference_constraint` + the `AddColumn` desugar to
//!     `AlterTableMulti { [AddColumn, AddForeignKey] }` — src/sql/planner.rs
//!   * `EmbeddedDatabase::validate_fk_reference` (no-PK / arity) and the
//!     validate-all-FKs-first pass in the `AlterTableMulti` arm — src/lib.rs

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{EmbeddedDatabase, Value};

fn fresh_db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory database")
}

/// `false` → TEXT family (`db.execute`), `true` → PARAMS family
/// (`db.execute_params`, the PostgreSQL extended protocol + REST).
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

/// Probe the CATALOG, not a projection: `SELECT nosuch FROM t` succeeds with
/// zero rows when `t` is empty (the projection is never evaluated — a
/// separate lenient-resolution defect, filed), so it cannot tell whether a
/// column exists. `information_schema.columns` can.
fn column_exists(db: &EmbeddedDatabase, table: &str, column: &str) -> bool {
    db.query(
        &format!(
            "SELECT column_name FROM information_schema.columns WHERE table_name = '{table}' AND column_name = '{column}'"
        ),
        &[],
    )
    .map(|rows| !rows.is_empty())
    .unwrap_or(false)
}

fn must_reject(db: &EmbeddedDatabase, sql: &str, params: bool) -> String {
    match run(db, sql, params) {
        Ok(_) => panic!(
            "[{}] *** UNENFORCEABLE CONSTRAINT ACCEPTED *** `{sql}` must be rejected at DDL time",
            family(params)
        ),
        Err(e) => e.to_string(),
    }
}

fn assert_undefined_table(err: &str, name: &str) {
    let lower = err.to_ascii_lowercase();
    assert!(
        lower.contains("relation") && lower.contains(&name.to_ascii_lowercase()) && lower.contains("does not exist"),
        "expected PostgreSQL's 42P01 wording (`relation \"{name}\" does not exist`), got: {err}"
    );
}

fn assert_undefined_column(err: &str, col: &str) {
    let lower = err.to_ascii_lowercase();
    assert!(
        lower.contains(&format!("column \"{}\"", col.to_ascii_lowercase())) && lower.contains("does not exist"),
        "expected PostgreSQL's 42703 wording (`column \"{col}\" … does not exist`), got: {err}"
    );
}

/// PostgreSQL's 42704 (undefined_object) wording for a list-less reference to a key-less table.
/// The wire classifier anchors on this text, so the WORDING is the SQLSTATE.
fn assert_no_primary_key(err: &str, parent: &str) {
    let lower = err.to_ascii_lowercase();
    assert!(
        lower.contains("there is no primary key for referenced table") && lower.contains(&parent.to_ascii_lowercase()),
        "expected PostgreSQL's 42704 wording (`there is no primary key for referenced table \"{parent}\"`), got: {err}"
    );
}

/// PostgreSQL's 42830 wording for a column-count mismatch.
fn assert_arity_mismatch(err: &str) {
    assert!(
        err.contains("number of referencing and referenced columns for foreign key disagree"),
        "expected PostgreSQL's 42830 arity wording, got: {err}"
    );
}

// ===========================================================================
// 1. ADD COLUMN … REFERENCES — the shapes gh_issue_27.rs does not pin.
// ===========================================================================

/// A rejected `ADD COLUMN … REFERENCES` must be ATOMIC: DDL is not
/// transactional, and the statement is planned as `[AddColumn, AddForeignKey]`,
/// so without the validate-first pass the column would be added and THEN the
/// foreign key rejected.
#[test]
fn a_rejected_add_column_reference_leaves_no_column_behind() {
    let db = fresh_db();
    db.execute("CREATE TABLE ac_atomic (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE ac_par (id INT PRIMARY KEY, name TEXT)")
        .unwrap();
    db.execute("CREATE TABLE ac_nopk (id INT)").unwrap();

    for (label, ddl, check) in [
        (
            "missing parent",
            "ALTER TABLE ac_atomic ADD COLUMN p INT REFERENCES ac_missing(id)",
            (|e: &str| assert_undefined_table(e, "ac_missing")) as fn(&str),
        ),
        (
            "missing parent column",
            "ALTER TABLE ac_atomic ADD COLUMN p INT REFERENCES ac_par(nocol)",
            (|e: &str| assert_undefined_column(e, "nocol")) as fn(&str),
        ),
        (
            "key-less parent, no column list",
            "ALTER TABLE ac_atomic ADD COLUMN p INT REFERENCES ac_nopk",
            (|e: &str| assert_no_primary_key(e, "ac_nopk")) as fn(&str),
        ),
    ] {
        let err = must_reject(&db, ddl, false);
        check(&err);
        assert!(
            !column_exists(&db, "ac_atomic", "p"),
            "[{label}] *** the rejected ADD COLUMN … REFERENCES left column `p` behind ***"
        );
    }

    // The table is still fully usable and a later, legal ADD COLUMN works.
    db.execute("ALTER TABLE ac_atomic ADD COLUMN p INT REFERENCES ac_par(id)")
        .expect("the corrected statement must succeed");
    assert!(column_exists(&db, "ac_atomic", "p"));
}

/// `REFERENCES p` with no list on ADD COLUMN binds to `p`'s PRIMARY KEY and is
/// enforced (the same default CREATE TABLE already applies).
#[test]
fn add_column_reference_without_a_column_list_binds_to_the_parent_primary_key() {
    let db = fresh_db();
    db.execute("CREATE TABLE acl_par (id INT PRIMARY KEY, name TEXT)")
        .unwrap();
    db.execute("CREATE TABLE acl_c (id INT PRIMARY KEY)").unwrap();
    db.execute("ALTER TABLE acl_c ADD COLUMN p INT REFERENCES acl_par")
        .expect("a legal list-less ADD COLUMN … REFERENCES must be accepted");

    db.execute("INSERT INTO acl_par VALUES (1, 'a')").unwrap();
    db.execute("INSERT INTO acl_c VALUES (1, 1)")
        .expect("a satisfied FK must insert");
    db.execute("INSERT INTO acl_c VALUES (2, NULL)")
        .expect("a NULL referencing value is always allowed (MATCH SIMPLE)");
    assert!(
        db.execute("INSERT INTO acl_c VALUES (3, 99)").is_err(),
        "*** the list-less ADD COLUMN … REFERENCES was not enforced against the parent PK ***"
    );
}

/// The referential action and deferrability on an ADD COLUMN reference are
/// carried through to the created constraint (`ON DELETE CASCADE` cascades).
#[test]
fn add_column_reference_carries_its_referential_action() {
    let db = fresh_db();
    db.execute("CREATE TABLE acc_par (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE acc_c (id INT PRIMARY KEY)").unwrap();
    db.execute("ALTER TABLE acc_c ADD COLUMN p INT REFERENCES acc_par(id) ON DELETE CASCADE")
        .expect("ON DELETE CASCADE on an ADD COLUMN reference must be accepted");

    db.execute("INSERT INTO acc_par VALUES (1)").unwrap();
    db.execute("INSERT INTO acc_c VALUES (1, 1)").unwrap();
    db.execute("DELETE FROM acc_par WHERE id = 1")
        .expect("the parent delete must cascade, not be blocked");
    let rows = db.query_with_columns("SELECT id FROM acc_c").expect("select").0;
    assert!(
        rows.is_empty(),
        "ON DELETE CASCADE was dropped on the way from ADD COLUMN to the constraint: {rows:?}"
    );
}

/// A self-reference through ADD COLUMN: the table exists, the referenced
/// column exists, and the constraint must be enforced.
#[test]
fn add_column_can_reference_its_own_table() {
    let db = fresh_db();
    db.execute("CREATE TABLE ac_tree (id INT PRIMARY KEY)").unwrap();
    db.execute("ALTER TABLE ac_tree ADD COLUMN parent INT REFERENCES ac_tree(id)")
        .expect("*** a legal self-reference through ADD COLUMN was rejected ***");
    db.execute("INSERT INTO ac_tree (id, parent) VALUES (1, NULL)").unwrap();
    db.execute("INSERT INTO ac_tree (id, parent) VALUES (2, 1)").unwrap();
    assert!(
        db.execute("INSERT INTO ac_tree (id, parent) VALUES (3, 99)").is_err(),
        "the self-referencing FK added by ADD COLUMN must be enforced"
    );

    // And the list-less spelling binds to the table's own PK.
    let db = fresh_db();
    db.execute("CREATE TABLE ac_tree2 (id INT PRIMARY KEY)").unwrap();
    db.execute("ALTER TABLE ac_tree2 ADD COLUMN parent INT REFERENCES ac_tree2")
        .expect("list-less self-reference through ADD COLUMN");
    db.execute("INSERT INTO ac_tree2 (id, parent) VALUES (1, NULL)")
        .unwrap();
    assert!(db.execute("INSERT INTO ac_tree2 (id, parent) VALUES (2, 99)").is_err());

    // A self-reference to a column the table does not have is still 42703.
    let err = must_reject(
        &db,
        "ALTER TABLE ac_tree2 ADD COLUMN up INT REFERENCES ac_tree2(nocol)",
        false,
    );
    assert_undefined_column(&err, "nocol");
    assert!(!column_exists(&db, "ac_tree2", "up"));
}

/// `ADD COLUMN c INT REFERENCES p(id), ADD COLUMN d INT` — the desugared
/// `AlterTableMulti` must be FLATTENED into the outer one (a nested Multi is
/// an internal error in `execute_alter_table_op`), both columns must exist
/// and the FK must be enforced.
#[test]
fn multi_operation_alter_with_an_add_column_reference_is_flattened_and_enforced() {
    let db = fresh_db();
    db.execute("CREATE TABLE mo_par (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE mo_c (id INT PRIMARY KEY)").unwrap();
    db.execute("ALTER TABLE mo_c ADD COLUMN p INT REFERENCES mo_par(id), ADD COLUMN d INT")
        .expect("*** a multi-operation ALTER with an inline reference was rejected ***");
    assert!(column_exists(&db, "mo_c", "p"));
    assert!(column_exists(&db, "mo_c", "d"));

    db.execute("INSERT INTO mo_par VALUES (1)").unwrap();
    db.execute("INSERT INTO mo_c VALUES (1, 1, 10)").unwrap();
    assert!(
        db.execute("INSERT INTO mo_c VALUES (2, 99, 10)").is_err(),
        "the FK from the first sub-operation must be enforced"
    );

    // The reverse order, and two references in one statement.
    db.execute("CREATE TABLE mo_par2 (id INT PRIMARY KEY)").unwrap();
    db.execute(
        "ALTER TABLE mo_c ADD COLUMN e INT, ADD COLUMN q INT REFERENCES mo_par2(id), \
         ADD COLUMN r INT REFERENCES mo_par",
    )
    .expect("three operations, two with references");
    db.execute("INSERT INTO mo_par2 VALUES (5)").unwrap();
    db.execute("INSERT INTO mo_c VALUES (3, 1, 0, 0, 5, 1)").unwrap();
    assert!(
        db.execute("INSERT INTO mo_c VALUES (4, 1, 0, 0, 99, 1)").is_err(),
        "q → mo_par2"
    );
    assert!(
        db.execute("INSERT INTO mo_c VALUES (5, 1, 0, 0, 5, 99)").is_err(),
        "r → mo_par"
    );
}

/// A multi-operation ALTER whose LATER sub-operation dangles must reject the
/// WHOLE statement before the earlier one mutates anything — this is what the
/// validate-first pass in the `AlterTableMulti` arm buys, and it applies to
/// the hand-written `ADD COLUMN a INT, ADD FOREIGN KEY (a) REFERENCES …`
/// spelling as well.
#[test]
fn a_multi_operation_alter_is_rejected_whole_when_any_foreign_key_dangles() {
    let db = fresh_db();
    db.execute("CREATE TABLE mw_par (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE mw_c (id INT PRIMARY KEY)").unwrap();

    let err = must_reject(
        &db,
        "ALTER TABLE mw_c ADD COLUMN a INT, ADD COLUMN b INT REFERENCES mw_nosuch(id)",
        false,
    );
    assert_undefined_table(&err, "mw_nosuch");
    assert!(
        !column_exists(&db, "mw_c", "a"),
        "*** the earlier ADD COLUMN was applied before the rejection ***"
    );
    assert!(!column_exists(&db, "mw_c", "b"));

    let err = must_reject(
        &db,
        "ALTER TABLE mw_c ADD COLUMN a INT, ADD FOREIGN KEY (a) REFERENCES mw_nosuch(id)",
        false,
    );
    assert_undefined_table(&err, "mw_nosuch");
    assert!(!column_exists(&db, "mw_c", "a"));

    // Legal version of the same statement works and is enforced.
    db.execute("ALTER TABLE mw_c ADD COLUMN a INT, ADD FOREIGN KEY (a) REFERENCES mw_par(id)")
        .expect("legal multi-op");
    db.execute("INSERT INTO mw_par VALUES (1)").unwrap();
    db.execute("INSERT INTO mw_c VALUES (1, 1)").unwrap();
    assert!(db.execute("INSERT INTO mw_c VALUES (2, 99)").is_err());
}

/// The plain `ADD COLUMN` (no REFERENCES) must be planned and executed exactly
/// as before: no foreign key, no constraint, a nullable usable column.
#[test]
fn a_plain_add_column_is_unchanged() {
    let db = fresh_db();
    db.execute("CREATE TABLE plain_c (id INT PRIMARY KEY)").unwrap();
    db.execute("ALTER TABLE plain_c ADD COLUMN v INT").unwrap();
    db.execute("ALTER TABLE plain_c ADD COLUMN w TEXT DEFAULT 'x' NOT NULL")
        .unwrap();
    db.execute("INSERT INTO plain_c (id, v) VALUES (1, 99)").unwrap();
    let rows = db
        .query_with_columns("SELECT v, w FROM plain_c WHERE id = 1")
        .expect("select")
        .0;
    assert_eq!(rows[0].values[0], Value::Int4(99));
    assert_eq!(rows[0].values[1], Value::String("x".into()));
}

// ===========================================================================
// 2. The list-less table-level FOREIGN KEY — parse + default + fail-closed,
//    on BOTH executor families (ALTER … ADD FOREIGN KEY reaches both).
// ===========================================================================

/// `ADD [CONSTRAINT n] FOREIGN KEY (c) REFERENCES p` with no list now parses,
/// binds to `p`'s PRIMARY KEY, and is enforced — on both families.
#[test]
fn list_less_add_foreign_key_binds_to_the_parent_primary_key_on_both_families() {
    for params in [false, true] {
        let db = fresh_db();
        let fam = family(params);
        db.execute("CREATE TABLE ll_par (id INT PRIMARY KEY, name TEXT)")
            .unwrap();
        db.execute("CREATE TABLE ll_c (id INT PRIMARY KEY, p INT, q INT)")
            .unwrap();

        run(&db, "ALTER TABLE ll_c ADD FOREIGN KEY (p) REFERENCES ll_par", params)
            .unwrap_or_else(|e| panic!("[{fam}] *** the list-less ADD FOREIGN KEY was rejected: {e} ***"));
        run(
            &db,
            "ALTER TABLE ll_c ADD CONSTRAINT ll_c_q_fkey FOREIGN KEY (q) REFERENCES ll_par ON DELETE CASCADE",
            params,
        )
        .unwrap_or_else(|e| panic!("[{fam}] *** the named list-less ADD FOREIGN KEY was rejected: {e} ***"));

        db.execute("INSERT INTO ll_par VALUES (1, 'a')").unwrap();
        db.execute("INSERT INTO ll_c VALUES (1, 1, 1)")
            .expect("a satisfied FK must insert");
        assert!(
            db.execute("INSERT INTO ll_c VALUES (2, 99, 1)").is_err(),
            "[{fam}] the unnamed list-less FK must be enforced"
        );
        assert!(
            db.execute("INSERT INTO ll_c VALUES (3, 1, 99)").is_err(),
            "[{fam}] the named list-less FK must be enforced"
        );

        // The named constraint can be dropped by its name (the name survived).
        db.execute("ALTER TABLE ll_c DROP CONSTRAINT ll_c_q_fkey")
            .unwrap_or_else(|e| panic!("[{fam}] the constraint name was lost by the rewrite: {e}"));
    }
}

/// The default resolves to a COMPOSITE primary key when the referencing list
/// has the same width, and reports the arity mismatch when it does not.
#[test]
fn list_less_foreign_key_resolves_to_a_composite_primary_key() {
    for params in [false, true] {
        let db = fresh_db();
        let fam = family(params);
        db.execute("CREATE TABLE cp_par (a INT, b INT, v TEXT, PRIMARY KEY (a, b))")
            .unwrap();
        db.execute("CREATE TABLE cp_c (id INT PRIMARY KEY, x INT, y INT)")
            .unwrap();

        // Width 1 against a two-column key: 42830 arity, nothing recorded.
        let err = must_reject(&db, "ALTER TABLE cp_c ADD FOREIGN KEY (x) REFERENCES cp_par", params);
        assert_arity_mismatch(&err);

        run(&db, "ALTER TABLE cp_c ADD FOREIGN KEY (x, y) REFERENCES cp_par", params)
            .unwrap_or_else(|e| panic!("[{fam}] *** a list-less composite FK was rejected: {e} ***"));

        db.execute("INSERT INTO cp_par VALUES (1, 2, 'v')").unwrap();
        db.execute("INSERT INTO cp_c VALUES (1, 1, 2)")
            .expect("a satisfied composite FK must insert");
        assert!(
            db.execute("INSERT INTO cp_c VALUES (2, 1, 3)").is_err(),
            "[{fam}] the composite FK must be enforced on the FULL key, not the first column"
        );
    }

    // The same through CREATE TABLE, table-level spelling.
    let db = fresh_db();
    db.execute("CREATE TABLE cpc_par (a INT, b INT, PRIMARY KEY (a, b))")
        .unwrap();
    let err = must_reject(
        &db,
        "CREATE TABLE cpc_bad (id INT PRIMARY KEY, x INT, FOREIGN KEY (x) REFERENCES cpc_par)",
        false,
    );
    assert_arity_mismatch(&err);
    assert!(!table_exists(&db, "cpc_bad"));
    db.execute("CREATE TABLE cpc_c (id INT PRIMARY KEY, x INT, y INT, FOREIGN KEY (x, y) REFERENCES cpc_par)")
        .expect("list-less composite FK in CREATE TABLE");
    db.execute("INSERT INTO cpc_par VALUES (1, 2)").unwrap();
    db.execute("INSERT INTO cpc_c VALUES (1, 1, 2)").unwrap();
    assert!(db.execute("INSERT INTO cpc_c VALUES (2, 1, 3)").is_err());
}

/// A list-less reference to a table with NO primary key is rejected cleanly
/// (42704 wording) in every spelling, on both families, and never persists
/// the empty referenced list that used to be enforced by nothing.
#[test]
fn a_reference_to_a_table_without_a_primary_key_is_rejected_in_every_spelling() {
    // CREATE TABLE, inline and table-level (text family only: CREATE TABLE is
    // unreachable on the params family, pinned in gh_issue_27.rs §6).
    for (label, ddl) in [
        (
            "inline",
            "CREATE TABLE np_c (id INT PRIMARY KEY, p INT REFERENCES np_par)",
        ),
        (
            "table-level",
            "CREATE TABLE np_c (id INT PRIMARY KEY, p INT, FOREIGN KEY (p) REFERENCES np_par)",
        ),
        (
            "named",
            "CREATE TABLE np_c (id INT PRIMARY KEY, p INT, CONSTRAINT np_c_fk FOREIGN KEY (p) REFERENCES np_par)",
        ),
    ] {
        let db = fresh_db();
        db.execute("CREATE TABLE np_par (id INT, name TEXT)").unwrap();
        let err = must_reject(&db, ddl, false);
        assert_no_primary_key(&err, "np_par");
        assert!(
            !table_exists(&db, "np_c"),
            "[{label}] the rejected CREATE TABLE left np_c behind"
        );
    }

    // ALTER … ADD FOREIGN KEY on both families; ADD COLUMN on the text family.
    for params in [false, true] {
        let db = fresh_db();
        let fam = family(params);
        db.execute("CREATE TABLE np_par (id INT, name TEXT)").unwrap();
        db.execute("CREATE TABLE np_c (id INT PRIMARY KEY, p INT)").unwrap();
        for ddl in [
            "ALTER TABLE np_c ADD FOREIGN KEY (p) REFERENCES np_par",
            "ALTER TABLE np_c ADD CONSTRAINT np_c_fk FOREIGN KEY (p) REFERENCES np_par",
        ] {
            let err = must_reject(&db, ddl, params);
            assert_no_primary_key(&err, "np_par");
        }
        if !params {
            let err = must_reject(&db, "ALTER TABLE np_c ADD COLUMN q INT REFERENCES np_par", false);
            assert_no_primary_key(&err, "np_par");
            assert!(!column_exists(&db, "np_c", "q"));
        }
        // Nothing was recorded: any value inserts.
        db.execute("INSERT INTO np_c VALUES (1, 12345)")
            .unwrap_or_else(|e| panic!("[{fam}] a refused FK left a constraint behind: {e}"));
    }

    // An EXPLICIT column list against a key-less table is still fine — the
    // 42704 rule is about the DEFAULT, not about the parent's constraints.
    let db = fresh_db();
    db.execute("CREATE TABLE npx_par (id INT, name TEXT)").unwrap();
    db.execute("CREATE TABLE npx_c (id INT PRIMARY KEY, p INT REFERENCES npx_par(id))")
        .expect("an explicit referenced column needs no primary key");
}

/// A self-referencing CREATE TABLE with NO primary key and a list-less
/// reference: the self-reference shortcut must apply the same 42704 rule
/// against the DECLARED columns, and leave no table behind.
#[test]
fn a_list_less_self_reference_on_a_key_less_table_is_rejected() {
    let db = fresh_db();
    for ddl in [
        "CREATE TABLE selfnp (id INT, parent INT REFERENCES selfnp)",
        "CREATE TABLE selfnp (id INT, parent INT, FOREIGN KEY (parent) REFERENCES selfnp)",
    ] {
        let err = must_reject(&db, ddl, false);
        assert_no_primary_key(&err, "selfnp");
        assert!(
            !table_exists(&db, "selfnp"),
            "the rejected CREATE TABLE left selfnp behind"
        );
    }

    // With a table-level PRIMARY KEY the same list-less self-reference is
    // legal (the planner propagates the table-level key onto the columns).
    db.execute("CREATE TABLE selfpk (id INT, parent INT REFERENCES selfpk, PRIMARY KEY (id))")
        .expect("list-less self-reference against a table-level PRIMARY KEY");
    db.execute("INSERT INTO selfpk VALUES (1, NULL)").unwrap();
    db.execute("INSERT INTO selfpk VALUES (2, 1)").unwrap();
    assert!(db.execute("INSERT INTO selfpk VALUES (3, 99)").is_err());
}

/// A referencing / referenced column-count mismatch with an EXPLICIT list is
/// 42704 in CREATE TABLE and in ALTER (both families), never a persisted
/// constraint with mismatched widths.
#[test]
fn a_column_count_mismatch_is_rejected_on_both_families() {
    let db = fresh_db();
    db.execute("CREATE TABLE ar_par (id INT PRIMARY KEY)").unwrap();
    let err = must_reject(
        &db,
        "CREATE TABLE ar_bad (a INT, b INT, PRIMARY KEY (a, b), FOREIGN KEY (a, b) REFERENCES ar_par(id))",
        false,
    );
    assert_arity_mismatch(&err);
    assert!(!table_exists(&db, "ar_bad"));

    for params in [false, true] {
        let db = fresh_db();
        db.execute("CREATE TABLE ar_par (id INT PRIMARY KEY)").unwrap();
        db.execute("CREATE TABLE ar_c (id INT PRIMARY KEY, x INT, y INT)")
            .unwrap();
        let err = must_reject(
            &db,
            "ALTER TABLE ar_c ADD FOREIGN KEY (x, y) REFERENCES ar_par(id)",
            params,
        );
        assert_arity_mismatch(&err);
        db.execute("INSERT INTO ar_c VALUES (1, 5, 6)")
            .unwrap_or_else(|e| panic!("[{}] a refused FK left a constraint behind: {e}", family(params)));
    }
}

/// The rewrite must fire for the list-less clause and ONLY for it: a present
/// list, the column-level spelling and a `FOREIGN KEY` phrase inside a string
/// literal are untouched, and a statement that is malformed for another
/// reason still reports a parse error (never a masked or planted one).
#[test]
fn the_parse_rewrite_is_scoped_to_the_list_less_table_level_clause() {
    let db = fresh_db();
    db.execute("CREATE TABLE sc_par (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE sc_c (id INT PRIMARY KEY, p INT, note TEXT)")
        .unwrap();

    // A literal carrying the phrase is data, not DDL.
    db.execute("INSERT INTO sc_c VALUES (1, NULL, 'FOREIGN KEY (p) REFERENCES sc_par')")
        .unwrap();
    let rows = db.query_with_columns("SELECT note FROM sc_c WHERE id = 1").unwrap().0;
    assert_eq!(
        rows[0].values[0],
        Value::String("FOREIGN KEY (p) REFERENCES sc_par".into())
    );

    // Malformed for another reason: still a parse error, and the placeholder
    // never shows up in the diagnostic.
    let err = db
        .execute("ALTER TABLE sc_c ADD FOREIGN KEY (p) REFERENCES sc_par ON DELETE")
        .expect_err("an incomplete referential action must not parse");
    let text = err.to_string().to_ascii_lowercase();
    assert!(text.contains("parse"), "expected a parse error, got: {err}");
    assert!(
        !text.contains("__hdb_fk_ref_default_pk"),
        "the placeholder leaked: {err}"
    );
}

/// The dangling list-less reference is rejected inside an explicit
/// transaction too (a migration runner wraps DDL in BEGIN … COMMIT).
#[test]
fn the_list_less_dangling_reference_is_rejected_inside_an_explicit_transaction() {
    let db = fresh_db();
    db.execute("CREATE TABLE tx_c (id INT PRIMARY KEY, p INT)").unwrap();
    db.execute("BEGIN").unwrap();
    let err = must_reject(&db, "ALTER TABLE tx_c ADD FOREIGN KEY (p) REFERENCES tx_nosuch", false);
    assert_undefined_table(&err, "tx_nosuch");
    let err = must_reject(&db, "ALTER TABLE tx_c ADD COLUMN q INT REFERENCES tx_nosuch(id)", false);
    assert_undefined_table(&err, "tx_nosuch");
    let _ = db.execute("ROLLBACK");
    assert!(!column_exists(&db, "tx_c", "q"));
    db.execute("INSERT INTO tx_c VALUES (1, 77)")
        .expect("no constraint recorded");
}
