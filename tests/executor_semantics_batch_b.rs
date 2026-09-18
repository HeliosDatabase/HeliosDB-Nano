//! Executor / constraint semantics batch B — four defects that were all
//! SILENT: every one of them reported success while doing the wrong thing.
//!
//!   1. sprinter `a3a6cc7c59d6` — `HAVING <agg> > $1` returned ZERO ROWS on the
//!      params family. Two stacked defects in three lines of
//!      `AggregateOperator::new`: the HAVING evaluator was built with an EMPTY
//!      bind vector, so `$1` evaluated to `Err("Parameter $1 not provided")`
//!      for every group — and the `_ => false` arm then swallowed that error
//!      and reported it as "this group does not qualify". A successful, empty
//!      result set instead of either the rows or the error. The same pair lived
//!      in the aggregate-PUSHDOWN twin, `Executor::apply_having_post_filter`.
//!   2. sprinter `fb9aec923da8` — `REFERENCES p(any_col)` was accepted against
//!      a column with no uniqueness. The constraint that leaves behind has no
//!      defined semantics: a child value can match several parent rows, so
//!      "does the parent exist" is ambiguous and `ON DELETE CASCADE` has no
//!      single victim. PostgreSQL refuses it at DDL time (42830).
//!   3. sprinter `b9aa53f0e6ca` — a list-less `FOREIGN KEY (x, y) REFERENCES t`
//!      bound to `t`'s primary key in SCHEMA order, where PostgreSQL binds in
//!      CONSTRAINT order. `PRIMARY KEY (b, a)` on a table declared `(a, b)`
//!      therefore bound `x→a, y→b` instead of `x→b, y→a`: a composite key
//!      silently checked against transposed columns.
//!   4. sprinter `6d501be6013f` — `ALTER TABLE parent RENAME TO parent2` left
//!      every OTHER table's foreign keys naming `parent`. `rename_table` only
//!      ever rewrote the RENAMED table's own constraint record (the
//!      self-reference case), and a foreign key lives on the CHILD.
//!
//! # SQLSTATE
//!
//! The wire classifier is not reachable from an integration test
//! (`sqlstate_for_error` is `pub(crate)`), so — as in `tests/gh_issue_27*.rs`
//! and `tests/ddl_validation_batch_a.rs` — item 2 pins the MESSAGE SHAPE the
//! classifier keys on, which is what makes the SQLSTATE:
//!
//! * `there is no unique constraint matching given keys for referenced table
//!   "p"` → 42830 invalid_foreign_key
//!
//! # The three read pipelines
//!
//! Item 1 is a correctness fix that had to land where EVERY path reaches it,
//! because the three families do not share an optimizer:
//!
//! * TEXT — `query()` / `execute()`, the full optimizer rule pipeline.
//! * PARAMS — `query_params()` / `query_params_for_session()`, which run NO
//!   optimizer passes and go through `parameterized_plan_cached`. This is also
//!   exactly what the PostgreSQL EXTENDED protocol reaches: `handler_extended.rs`
//!   Execute → `query_params_for_session*`. `query_params_for_session` is
//!   therefore called directly below — it is the wire path, minus the socket.
//! * SCHEMA-AWARE PARAMS — `query_params_with_columns()`, which DOES run the
//!   full pipeline and is what the MySQL prepared-statement path reaches
//!   (`protocol/mysql/handler.rs` → `query_params_with_columns_for_session`)
//!   along with the PyO3 binding.
//!
//! Each of the three is asserted, and asserted to AGREE — a future divergence
//! fails loudly rather than being discovered by a user.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};

fn fresh_db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory database")
}

fn session(db: &EmbeddedDatabase) -> SessionId {
    db.create_session("batch_b", IsolationLevel::ReadCommitted)
        .expect("session")
}

/// Rows as comparable text, order-insensitive. GROUP BY output order is not
/// specified, so every cross-family comparison below sorts first rather than
/// leaning on an ORDER BY that would itself be under test.
fn normalize(rows: &[Tuple]) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = rows
        .iter()
        .map(|t| t.values.iter().map(|v| format!("{v}")).collect())
        .collect();
    out.sort();
    out
}

/// `false` → TEXT family, `true` → PARAMS family.
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

fn ok(db: &EmbeddedDatabase, sql: &str, params: bool) {
    if let Err(e) = run(db, sql, params) {
        panic!("[{}] `{sql}` must succeed: {e}", family(params));
    }
}

fn must_reject(db: &EmbeddedDatabase, sql: &str, params: bool) -> String {
    match run(db, sql, params) {
        Ok(_) => panic!(
            "[{}] *** UNENFORCEABLE DECLARATION ACCEPTED *** `{sql}` must be rejected at DDL time",
            family(params)
        ),
        Err(e) => e.to_string(),
    }
}

fn table_exists(db: &EmbeddedDatabase, table: &str) -> bool {
    db.query_with_columns(&format!("SELECT * FROM {table}")).is_ok()
}

/// PostgreSQL's 42830 wording for a referenced column set that is not a key
/// (`tablecmds.c transformFkeyCheckAttrs`). Deliberately NOT the ON CONFLICT
/// wording ("no unique OR EXCLUSION constraint matching …", 42P10).
fn assert_no_unique_constraint(err: &str, parent: &str) {
    let lower = err.to_ascii_lowercase();
    assert!(
        lower.contains("there is no unique constraint matching given keys for referenced table")
            && lower.contains(&parent.to_ascii_lowercase()),
        "expected PostgreSQL's 42830 wording naming \"{parent}\", got: {err}"
    );
}

// ===========================================================================
// 1. sprinter a3a6cc7c59d6 — HAVING with a bound parameter
// ===========================================================================

/// The headline repro, on all three read pipelines plus the text family.
///
/// FAILS on the pre-fix tree: every params-family spelling below returned
/// `Ok(vec![])`. `AggregateOperator::new` built the HAVING evaluator as
/// `Evaluator::new(schema)` — no bind vector — so `$1` raised
/// `Parameter $1 not provided`, and the `_ => false` arm turned that error into
/// "the group does not qualify" for every group.
#[test]
fn having_with_a_parameter_returns_the_same_rows_as_the_literal() {
    let db = fresh_db();
    db.execute("CREATE TABLE na (id INT PRIMARY KEY, a INT)").unwrap();
    db.execute("INSERT INTO na VALUES (1, 10)").unwrap();
    db.execute("INSERT INTO na VALUES (2, 20)").unwrap();

    const LITERAL: &str = "SELECT a, count(*) FROM na GROUP BY a HAVING count(*) > 0";
    const PARAM: &str = "SELECT a, count(*) FROM na GROUP BY a HAVING count(*) > $1";
    // An int4 bind, which is what a real driver sends for a small integer —
    // the comparison against COUNT's int8 is a cross-width one on purpose.
    let bind = [Value::Int4(0)];

    // The reference answer: the literal spelling on the TEXT family.
    let expected = normalize(&db.query(LITERAL, &[]).expect("text/literal"));
    assert_eq!(expected.len(), 2, "both groups qualify: {expected:?}");

    // The literal spelling on the PARAMS family — no parameter involved, so
    // this was already correct and is the control for the comparison.
    assert_eq!(
        normalize(&db.query_params(LITERAL, &[]).expect("params/literal")),
        expected,
        "the literal spelling must agree across families"
    );

    // 1. Embedded params API.
    assert_eq!(
        normalize(&db.query_params(PARAM, &bind).expect("params/$1")),
        expected,
        "*** SILENTLY WRONG *** `HAVING count(*) > $1` must return the rows the literal returns"
    );

    // 2. The PostgreSQL EXTENDED protocol: `handler_extended.rs` Execute calls
    //    exactly this function with the Bind values.
    let sid = session(&db);
    assert_eq!(
        normalize(&db.query_params_for_session(sid, PARAM, &bind).expect("extended/$1")),
        expected,
        "the PG extended protocol path must agree with the literal spelling"
    );

    // 3. The schema-aware params path — the MySQL prepared path and the PyO3
    //    binding — which runs the FULL optimizer pipeline, unlike (1) and (2).
    let (rows, _cols) = db.query_params_with_columns(PARAM, &bind).expect("with_columns/$1");
    assert_eq!(
        normalize(&rows),
        expected,
        "the schema-aware params path must agree with the literal spelling"
    );
}

/// A HAVING predicate that genuinely fails must report the FAILURE. This is
/// the half that matters most: threading the parameters fixes one cause of an
/// error, while swallowing errors hides every cause of one.
///
/// FAILS on the pre-fix tree: `Ok(vec![])` on both families, because
/// `Err("Division by zero")` was matched by the `_ => false` arm and every
/// group was dropped.
#[test]
fn a_having_expression_that_errors_reports_the_error_not_an_empty_result() {
    for params in [false, true] {
        let db = fresh_db();
        let fam = family(params);
        ok(&db, "CREATE TABLE ne (id INT PRIMARY KEY, a INT)", params);
        ok(&db, "INSERT INTO ne VALUES (1, 10)", params);
        ok(&db, "INSERT INTO ne VALUES (2, 20)", params);

        let sql = "SELECT a, count(*) FROM ne GROUP BY a HAVING count(*) / 0 > 0";
        let result = if params {
            db.query_params(sql, &[])
        } else {
            db.query(sql, &[])
        };
        match result {
            Ok(rows) => panic!(
                "[{fam}] *** ERROR SWALLOWED *** a failing HAVING predicate returned {} row(s) \
                 instead of an error",
                rows.len()
            ),
            Err(e) => assert!(
                e.to_string().to_ascii_lowercase().contains("division by zero"),
                "[{fam}] expected the division-by-zero error to reach the caller, got: {e}"
            ),
        }
    }
}

/// SQL three-valued logic is NOT an error: a HAVING predicate that evaluates
/// to NULL drops its group silently, exactly as before. This is the boundary
/// the error-propagation change must not cross — `Ok(Null)` still drops.
#[test]
fn a_having_predicate_that_is_null_drops_the_group_without_erroring() {
    let db = fresh_db();
    db.execute("CREATE TABLE nn (id INT PRIMARY KEY, g INT, v INT)")
        .unwrap();
    // g=1 has only a NULL v, so MAX(v) is NULL and `NULL > 0` is NULL.
    db.execute("INSERT INTO nn VALUES (1, 1, NULL)").unwrap();
    db.execute("INSERT INTO nn VALUES (2, 2, 5)").unwrap();

    let rows = db
        .query_params(
            "SELECT g, max(v) FROM nn GROUP BY g HAVING max(v) > $1",
            &[Value::Int4(0)],
        )
        .expect("a NULL HAVING predicate must not be an error");
    let rows = normalize(&rows);
    assert_eq!(rows.len(), 1, "only the non-NULL group qualifies: {rows:?}");
    assert_eq!(rows[0][0], "2", "the surviving group must be g = 2: {rows:?}");
}

/// `Evaluator::new` audit, site `src/sql/executor/window.rs:64`: a `$n` DOES
/// reach the window operator's evaluator, because window-function ARGUMENTS
/// are lowered by the generic `Planner::expr_to_logical`.
///
/// DIFFERENTIAL, with a guard: every call site in `window.rs` swallows
/// evaluation failures as `.unwrap_or(Value::Null)`, so the pre-fix symptom was
/// a silent NULL rather than an error. The guard exists because the literal
/// spelling is the premise — if this engine does not support a constant
/// argument to `FIRST_VALUE` at all there is no divergence to detect, and this
/// test must not fail for that unrelated reason. When the literal spelling DOES
/// work, the parameter spelling must match it.
#[test]
fn a_window_function_argument_sees_its_bound_parameter() {
    let db = fresh_db();
    db.execute("CREATE TABLE wp (id INT PRIMARY KEY, a INT)").unwrap();
    db.execute("INSERT INTO wp VALUES (1, 10)").unwrap();
    db.execute("INSERT INTO wp VALUES (2, 20)").unwrap();

    let Ok(literal) = db.query("SELECT id, FIRST_VALUE(7) OVER (ORDER BY id) FROM wp", &[]) else {
        eprintln!("FIRST_VALUE(<constant>) is not supported here; nothing to compare against");
        return;
    };
    let literal = normalize(&literal);
    if literal.len() != 2 || literal.iter().any(|r| r.get(1).map(String::as_str) != Some("7")) {
        eprintln!("FIRST_VALUE(<constant>) did not yield the constant ({literal:?}); nothing to compare against");
        return;
    }

    let bound = db
        .query_params(
            "SELECT id, FIRST_VALUE($1) OVER (ORDER BY id) FROM wp",
            &[Value::Int4(7)],
        )
        .expect("a parameter in a window-function argument must not fail the query");
    assert_eq!(
        normalize(&bound),
        literal,
        "*** SILENTLY WRONG *** a bound parameter inside a window-function argument \
         must produce what the same value spelled as a literal produces"
    );
}

// ===========================================================================
// 2. sprinter fb9aec923da8 — an FK target must be a key
// ===========================================================================

/// `REFERENCES p(v)` where `v` is neither the primary key nor unique is
/// refused with PostgreSQL's 42830, through CREATE TABLE and through
/// ALTER TABLE, on both executor families — and the refused CREATE TABLE
/// leaves no table behind.
///
/// FAILS on the pre-fix tree: both statements returned `Ok`.
#[test]
fn a_foreign_key_target_must_be_a_key() {
    for params in [false, true] {
        let db = fresh_db();
        let fam = family(params);
        ok(&db, "CREATE TABLE p (id INT, v INT)", params);

        let err = must_reject(&db, "CREATE TABLE c (x INT REFERENCES p(v))", params);
        assert_no_unique_constraint(&err, "p");
        assert!(
            !table_exists(&db, "c"),
            "[{fam}] the rejected CREATE TABLE left table c behind"
        );

        // The table-level spelling, and the ALTER path, take the same rule.
        let err = must_reject(&db, "CREATE TABLE c (x INT, FOREIGN KEY (x) REFERENCES p(v))", params);
        assert_no_unique_constraint(&err, "p");

        ok(&db, "CREATE TABLE c (x INT)", params);
        let err = must_reject(&db, "ALTER TABLE c ADD FOREIGN KEY (x) REFERENCES p(v)", params);
        assert_no_unique_constraint(&err, "p");
    }
}

/// Every spelling that DOES make the referenced set a key is still accepted:
/// the primary key, a column-level `UNIQUE`, and a composite table-level
/// `UNIQUE (a, b)`. (`CREATE UNIQUE INDEX` has its own test below.)
///
/// The composite case also pins the SET-vs-sequence decision: PostgreSQL
/// matches the referenced list against the constraint's column SET, so
/// `UNIQUE (a, b)` satisfies a reference to `(b, a)` as well as to `(a, b)`.
/// The key really is unique under either permutation; the permutation still
/// decides the positional BINDING, which is a separate question (item 3).
#[test]
fn every_real_key_is_still_a_legal_foreign_key_target() {
    for params in [false, true] {
        let db = fresh_db();

        // PRIMARY KEY.
        ok(&db, "CREATE TABLE kp (id INT PRIMARY KEY, v INT)", params);
        ok(&db, "CREATE TABLE kc1 (x INT REFERENCES kp(id))", params);

        // Column-level UNIQUE.
        ok(&db, "CREATE TABLE ku (id INT PRIMARY KEY, v INT UNIQUE)", params);
        ok(&db, "CREATE TABLE kc2 (x INT REFERENCES ku(v))", params);

        // Composite table-level UNIQUE, referenced in both permutations.
        ok(&db, "CREATE TABLE kk (a INT, b INT, UNIQUE (a, b))", params);
        ok(
            &db,
            "CREATE TABLE kc4 (x INT, y INT, FOREIGN KEY (x, y) REFERENCES kk(a, b))",
            params,
        );
        ok(
            &db,
            "CREATE TABLE kc5 (x INT, y INT, FOREIGN KEY (x, y) REFERENCES kk(b, a))",
            params,
        );
    }
}

/// A user's `CREATE UNIQUE INDEX` is a key too — the source that neither the
/// column flags nor the `TableConstraints` record knows about, so it is the
/// one that proves `unique_key_covers` asks the live index registry as well.
///
/// TEXT family only, on purpose: the point under test is the index as a SOURCE
/// of uniqueness, and routing `CREATE INDEX` through the params funnel is a
/// separate question this test has no business pinning.
#[test]
fn a_user_unique_index_is_a_legal_foreign_key_target() {
    let db = fresh_db();
    db.execute("CREATE TABLE ki (id INT, v INT)").unwrap();
    db.execute("CREATE UNIQUE INDEX ki_v_ux ON ki (v)").unwrap();
    db.execute("CREATE TABLE kc3 (x INT REFERENCES ki(v))")
        .expect("a column covered by a UNIQUE INDEX is a legal foreign-key target");

    // …and the column NEXT to it, covered by nothing, still is not.
    let err = must_reject(&db, "CREATE TABLE kc3b (x INT REFERENCES ki(id))", false);
    assert_no_unique_constraint(&err, "ki");
}

/// The SELF-reference arm takes the same rule. It cannot ask the catalog —
/// `validate_create_table_fk_targets` runs before `create_table`, so the table
/// is not there yet — and must answer from the columns being DECLARED.
///
/// FAILS on the pre-fix tree: the key-less self-reference returned `Ok`.
#[test]
fn a_self_referencing_foreign_key_target_must_be_a_key_too() {
    for params in [false, true] {
        let db = fresh_db();

        let err = must_reject(&db, "CREATE TABLE sk (id INT, p INT REFERENCES sk(id))", params);
        assert_no_unique_constraint(&err, "sk");
        assert!(!table_exists(&db, "sk"), "the rejected self-reference left sk behind");

        // The legal shapes are untouched: the declared PRIMARY KEY (inline and
        // table-level) and a declared UNIQUE.
        ok(
            &db,
            "CREATE TABLE sk1 (id INT PRIMARY KEY, p INT REFERENCES sk1(id))",
            params,
        );
        ok(
            &db,
            "CREATE TABLE sk2 (id INT, p INT, PRIMARY KEY (id), FOREIGN KEY (p) REFERENCES sk2(id))",
            params,
        );
        ok(
            &db,
            "CREATE TABLE sk3 (id INT PRIMARY KEY, u INT UNIQUE, p INT REFERENCES sk3(u))",
            params,
        );
    }
}

// ===========================================================================
// 3. sprinter b9aa53f0e6ca — list-less REFERENCES binds in CONSTRAINT order
// ===========================================================================

/// `PRIMARY KEY (b, a)` declared on a table whose columns are `(a, b)`: a
/// list-less `FOREIGN KEY (x, y) REFERENCES t` must bind `x→b, y→a`.
///
/// The parent holds exactly one row, `(a, b) = (1, 2)`. The two bindings are
/// therefore perfect discriminators:
///
/// * CONSTRAINT order `(b, a)` — PostgreSQL, and now Nano — accepts a child
///   `(x, y) = (2, 1)` and rejects `(1, 2)`;
/// * SCHEMA order `(a, b)` — the pre-fix behaviour — does exactly the reverse.
///
/// FAILS on the pre-fix tree: `(2, 1)` was rejected as a foreign-key violation
/// and `(1, 2)` was accepted.
#[test]
fn a_list_less_composite_foreign_key_binds_the_key_in_declared_order() {
    let db = fresh_db();
    db.execute("CREATE TABLE ord_p (a INT, b INT, PRIMARY KEY (b, a))")
        .unwrap();
    db.execute("CREATE TABLE ord_c (x INT, y INT, FOREIGN KEY (x, y) REFERENCES ord_p)")
        .unwrap();
    db.execute("INSERT INTO ord_p VALUES (1, 2)").unwrap();

    db.execute("INSERT INTO ord_c VALUES (2, 1)").unwrap_or_else(|e| {
        panic!(
            "*** TRANSPOSED KEY *** a list-less REFERENCES must bind the PK in DECLARED order \
             (b, a), so (x, y) = (2, 1) matches the parent row (a, b) = (1, 2): {e}"
        )
    });
    assert!(
        db.execute("INSERT INTO ord_c VALUES (1, 2)").is_err(),
        "*** TRANSPOSED KEY *** (x, y) = (1, 2) matches no parent row under the declared \
         key order (b, a) and must be refused"
    );
}

/// The declared order is DURABLE: it is read from the `table_constraints`
/// record, which the reference list was resolved against at DDL time, so a
/// reopened database enforces the same binding.
#[test]
fn the_declared_key_order_survives_a_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");

    {
        let db = EmbeddedDatabase::new(dir.path()).expect("open");
        db.execute("CREATE TABLE rp (a INT, b INT, PRIMARY KEY (b, a))")
            .unwrap();
        db.execute("CREATE TABLE rc (x INT, y INT, FOREIGN KEY (x, y) REFERENCES rp)")
            .unwrap();
        db.execute("INSERT INTO rp VALUES (1, 2)").unwrap();
    }

    let db = EmbeddedDatabase::new(dir.path()).expect("reopen");
    db.execute("INSERT INTO rc VALUES (2, 1)")
        .expect("the declared key order must survive the reopen");
    assert!(
        db.execute("INSERT INTO rc VALUES (1, 2)").is_err(),
        "the transposed child row must still be refused after a reopen"
    );

    // And a new list-less foreign key declared AFTER the reopen resolves the
    // same way — the record, not the schema-column order, is the source.
    db.execute("CREATE TABLE rc2 (x INT, y INT, FOREIGN KEY (x, y) REFERENCES rp)")
        .unwrap();
    db.execute("INSERT INTO rc2 VALUES (2, 1)")
        .expect("a post-reopen list-less FK must bind in declared order too");
    assert!(db.execute("INSERT INTO rc2 VALUES (1, 2)").is_err());
}

// ===========================================================================
// 4. sprinter 6d501be6013f — RENAME TABLE and INBOUND foreign keys
// ===========================================================================

/// After `ALTER TABLE parent RENAME TO parent2` the CHILD's foreign key must
/// still work — both halves of it.
///
/// FAILS on the pre-fix tree at the FIRST insert, not the second: the child's
/// constraint still named `ren_p`, so `check_referencing_rows_exist` found no
/// ART index under that name (the rename moved them with the table), fell
/// through to its slow path, and failed on `get_table_schema("ren_p")`. A
/// perfectly valid child row was refused with a missing-relation diagnostic.
#[test]
fn renaming_a_parent_repoints_inbound_foreign_keys() {
    let db = fresh_db();
    db.execute("CREATE TABLE ren_p (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE ren_c (id INT PRIMARY KEY, p INT REFERENCES ren_p(id))")
        .unwrap();
    db.execute("INSERT INTO ren_p VALUES (1)").unwrap();
    db.execute("ALTER TABLE ren_p RENAME TO ren_p2").unwrap();

    db.execute("INSERT INTO ren_c VALUES (10, 1)").unwrap_or_else(|e| {
        panic!("*** INBOUND FK STRANDED *** a valid child row was refused after the parent rename: {e}")
    });
    let err = db
        .execute("INSERT INTO ren_c VALUES (11, 99)")
        .expect_err("*** INBOUND FK STRANDED *** the foreign key stopped being enforced after the rename");
    assert!(
        err.to_string().to_ascii_lowercase().contains("foreign key"),
        "expected a foreign-key violation (23503), got: {err}"
    );
}

/// The repoint is DURABLE — it is written through `save_table_constraints`,
/// not just patched in the in-memory cache.
#[test]
fn the_inbound_foreign_key_repoint_survives_a_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");

    {
        let db = EmbeddedDatabase::new(dir.path()).expect("open");
        db.execute("CREATE TABLE rrp (id INT PRIMARY KEY)").unwrap();
        db.execute("CREATE TABLE rrc (id INT PRIMARY KEY, p INT REFERENCES rrp(id))")
            .unwrap();
        db.execute("INSERT INTO rrp VALUES (1)").unwrap();
        db.execute("ALTER TABLE rrp RENAME TO rrp2").unwrap();
    }

    let db = EmbeddedDatabase::new(dir.path()).expect("reopen");
    db.execute("INSERT INTO rrc VALUES (10, 1)")
        .expect("the repointed foreign key must survive the reopen");
    assert!(
        db.execute("INSERT INTO rrc VALUES (11, 99)").is_err(),
        "the repointed foreign key must still be enforced after the reopen"
    );
}

/// `ALTER TABLE … SET SCHEMA` is implemented as a rename onto a new storage
/// key and goes through the same `move_table_side_records` funnel, so it gets
/// the inbound repoint from the same place.
#[test]
fn moving_a_parent_to_another_schema_repoints_inbound_foreign_keys() {
    let db = fresh_db();
    db.execute("CREATE SCHEMA setsch").unwrap();
    db.execute("CREATE TABLE ss_p (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE ss_c (id INT PRIMARY KEY, p INT REFERENCES ss_p(id))")
        .unwrap();
    db.execute("INSERT INTO ss_p VALUES (1)").unwrap();
    db.execute("ALTER TABLE ss_p SET SCHEMA setsch").unwrap();

    db.execute("INSERT INTO ss_c VALUES (10, 1)")
        .unwrap_or_else(|e| panic!("*** INBOUND FK STRANDED *** a valid child row was refused after SET SCHEMA: {e}"));
    assert!(
        db.execute("INSERT INTO ss_c VALUES (11, 99)").is_err(),
        "*** INBOUND FK STRANDED *** the foreign key stopped being enforced after SET SCHEMA"
    );
}

/// The SELF-reference case that `move_table_side_records` already handled must
/// keep working — the new inbound scan skips the moved table itself, so the
/// two rewrites cannot fight over the same record.
#[test]
fn a_self_referencing_foreign_key_still_follows_its_table_through_a_rename() {
    let db = fresh_db();
    db.execute("CREATE TABLE selfr (id INT PRIMARY KEY, parent INT REFERENCES selfr(id))")
        .unwrap();
    db.execute("INSERT INTO selfr VALUES (1, NULL)").unwrap();
    db.execute("ALTER TABLE selfr RENAME TO selfr2").unwrap();

    db.execute("INSERT INTO selfr2 VALUES (2, 1)")
        .expect("a self-referencing FK must still accept a valid row after the rename");
    assert!(
        db.execute("INSERT INTO selfr2 VALUES (3, 99)").is_err(),
        "a self-referencing FK must still be enforced after the rename"
    );
}
