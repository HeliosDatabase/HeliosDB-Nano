//! Triggers on BOTH executor families: the cases this file has always held.
//!
//! Until 5.0 this file pinned that trigger bodies did NOT run, with one exception (a
//! `BEFORE INSERT` row trigger made of `NEW.<col> = <expr>` assignments was turned into a
//! row rewrite at CREATE TRIGGER). Triggers run their function now, so the cases that
//! pinned "nothing happens" assert what happens instead (each says what it used to pin),
//! and the row-rewrite cases are unchanged: the same rows result, now from running the
//! function. The suites written for the new behaviour are `tests/trigger_row_events.rs`,
//! `trigger_raise_and_nesting.rs`, `trigger_ddl_lifecycle.rs`, `trigger_write_routes.rs`,
//! `trigger_dialect_forms.rs` and `trigger_catalog_views.rs`.
//!
//! HOW TO MAINTAIN THIS FILE. Every test asserts unconditionally. Never wrap an assertion
//! in `if result.is_ok()`, and never assert `rows.len() > 0` against `SELECT COUNT(*)` — a
//! count query returns exactly one row whether the count is 0 or 10,000. Use `rows_in()`.

use heliosdb_nano::{EmbeddedDatabase, Value};

// ===========================================================================
// Harness
// ===========================================================================

fn mem_db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory db")
}

/// Rows physically present in `table`. Deliberately NOT `SELECT COUNT(*)`: a count query
/// always returns exactly one row, so `rows.len()` on it can never detect an empty table.
fn rows_in(db: &EmbeddedDatabase, table: &str) -> usize {
    let sql = format!("SELECT * FROM {table}");
    db.query(&sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
        .len()
}

/// First column of the first row, as text.
fn first_text(db: &EmbeddedDatabase, sql: &str) -> String {
    let rows = db.query(sql, &[]).unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
    match rows.first().and_then(|r| r.values.first()) {
        Some(Value::String(s)) => s.clone(),
        other => panic!("expected text from `{sql}`, got {other:?}"),
    }
}

/// Subject table `trg_t(id, tag)` plus audit table `trg_audit(note)`.
fn create_tables(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE trg_t (id INT, tag TEXT)")
        .expect("subject table");
    db.execute("CREATE TABLE trg_audit (note TEXT)").expect("audit table");
}

/// Register a trigger function whose PL/pgSQL body is `body`.
fn create_fn(db: &EmbeddedDatabase, name: &str, body: &str) {
    let sql = format!("CREATE FUNCTION {name}() RETURNS TRIGGER AS $$ {body} $$ LANGUAGE plpgsql");
    db.execute(&sql)
        .unwrap_or_else(|e| panic!("CREATE FUNCTION {name} failed: {e}"));
}

/// The one body shape with an effect: rewrite `NEW.tag`.
const REWRITE_BODY: &str = "BEGIN NEW.tag = 'set-by-trigger'; RETURN NEW; END";
/// The other: drop the row entirely.
const SKIP_BODY: &str = "BEGIN RETURN NULL; END";
/// A body with a side effect: an audit row per firing.
const SIDE_EFFECT_BODY: &str = "BEGIN INSERT INTO trg_audit (note) VALUES ('fired'); RETURN NEW; END";

fn create_trigger_sql(name: &str, clause: &str, func: &str) -> String {
    format!("CREATE TRIGGER {name} {clause} EXECUTE FUNCTION {func}()")
}

/// `CREATE TRIGGER` through the TEXT family (`execute()`): embedded API, PG simple query,
/// MySQL wire, REPL.
fn create_trigger_text(db: &EmbeddedDatabase, name: &str, clause: &str, func: &str) -> heliosdb_nano::Result<u64> {
    db.execute(&create_trigger_sql(name, clause, func))
}

/// `CREATE TRIGGER` through the PARAMS family (`execute_params()`): the PostgreSQL
/// EXTENDED query protocol and the REST layer. This is the path that used to hard-error.
fn create_trigger_params(db: &EmbeddedDatabase, name: &str, clause: &str, func: &str) -> heliosdb_nano::Result<u64> {
    db.execute_params(&create_trigger_sql(name, clause, func), &[])
}

const BEFORE_INSERT_ROW: &str = "BEFORE INSERT ON trg_t FOR EACH ROW";

// ===========================================================================
// 1. DDL matrix — CREATE / DROP TRIGGER on BOTH executor families
// ===========================================================================

#[test]
fn create_trigger_succeeds_on_the_text_family() {
    let db = mem_db();
    create_tables(&db);
    create_fn(&db, "trg_fn", SIDE_EFFECT_BODY);

    create_trigger_text(&db, "trg", "AFTER INSERT ON trg_t FOR EACH ROW", "trg_fn")
        .expect("text family CREATE TRIGGER");

    assert!(db.trigger_registry.has_triggers_for_table("trg_t"), "must register");
    let triggers = db.trigger_registry.get_triggers_for_table("trg_t").expect("lookup");
    assert_eq!(triggers.len(), 1);
    assert_eq!(triggers[0].name, "trg");
    // The registered body is STILL always empty — this is why bodies never run.
    assert!(triggers[0].body.is_empty(), "registered trigger body is always empty");
}

#[test]
fn create_trigger_succeeds_on_the_params_family() {
    // THE headline fix. Over the PostgreSQL extended query protocol this used to fail with
    // `Operator not yet implemented: CreateTrigger { … }`, so every ORM and migration tool
    // that binds parameters could not create a trigger at all.
    let db = mem_db();
    create_tables(&db);
    create_fn(&db, "trg_fn", SIDE_EFFECT_BODY);

    let res = create_trigger_params(&db, "trg", "AFTER INSERT ON trg_t FOR EACH ROW", "trg_fn");
    let err = match &res {
        Ok(_) => String::new(),
        Err(e) => e.to_string(),
    };
    assert!(res.is_ok(), "params-family CREATE TRIGGER must succeed, got: {err}");
    assert!(
        !err.contains("not yet implemented"),
        "the extended-protocol hard error must be gone: {err}"
    );
    assert!(
        db.trigger_registry.has_triggers_for_table("trg_t"),
        "the params family must register the trigger in the same live registry"
    );
}

#[test]
fn duplicate_create_trigger_errors_on_both_families() {
    let db = mem_db();
    create_tables(&db);
    create_fn(&db, "trg_fn", SIDE_EFFECT_BODY);
    let clause = "AFTER INSERT ON trg_t FOR EACH ROW";

    create_trigger_text(&db, "trg", clause, "trg_fn").expect("first create");

    let text_err = create_trigger_text(&db, "trg", clause, "trg_fn")
        .expect_err("text family must reject a duplicate")
        .to_string();
    assert!(text_err.contains("already exists"), "text family: {text_err}");

    let params_err = create_trigger_params(&db, "trg", clause, "trg_fn")
        .expect_err("params family must reject a duplicate")
        .to_string();
    assert!(params_err.contains("already exists"), "params family: {params_err}");

    assert_eq!(
        db.trigger_registry
            .get_triggers_for_table("trg_t")
            .expect("lookup")
            .len(),
        1,
        "a rejected create must not add a registration"
    );
}

#[test]
fn create_or_replace_trigger_is_idempotent_on_both_families() {
    let db = mem_db();
    create_tables(&db);
    create_fn(&db, "trg_fn", SIDE_EFFECT_BODY);
    let sql = "CREATE OR REPLACE TRIGGER trg AFTER INSERT ON trg_t FOR EACH ROW EXECUTE FUNCTION trg_fn()";

    db.execute(sql).expect("first create, text family");
    db.execute(sql).expect("OR REPLACE must not error, text family");
    db.execute_params(sql, &[])
        .expect("OR REPLACE must not error, params family");

    assert_eq!(
        db.trigger_registry
            .get_triggers_for_table("trg_t")
            .expect("lookup")
            .len(),
        1,
        "OR REPLACE must leave exactly one registration"
    );
}

#[test]
fn drop_trigger_works_on_both_families() {
    let db = mem_db();
    create_tables(&db);
    create_fn(&db, "trg_fn", SIDE_EFFECT_BODY);
    let clause = "AFTER INSERT ON trg_t FOR EACH ROW";

    create_trigger_text(&db, "a", clause, "trg_fn").expect("create a");
    db.execute("DROP TRIGGER a ON trg_t").expect("text family DROP TRIGGER");
    assert!(!db.trigger_registry.has_triggers_for_table("trg_t"), "a must be gone");

    create_trigger_text(&db, "b", clause, "trg_fn").expect("create b");
    db.execute_params("DROP TRIGGER b ON trg_t", &[])
        .expect("params family DROP TRIGGER");
    assert!(!db.trigger_registry.has_triggers_for_table("trg_t"), "b must be gone");
}

#[test]
fn drop_trigger_missing_errors_on_both_families_unless_if_exists() {
    let db = mem_db();
    create_tables(&db);

    let text_err = db
        .execute("DROP TRIGGER nope ON trg_t")
        .expect_err("text family must reject a missing trigger")
        .to_string();
    assert!(text_err.contains("does not exist"), "text family: {text_err}");

    let params_err = db
        .execute_params("DROP TRIGGER nope ON trg_t", &[])
        .expect_err("params family must reject a missing trigger")
        .to_string();
    assert!(params_err.contains("does not exist"), "params family: {params_err}");

    db.execute("DROP TRIGGER IF EXISTS nope ON trg_t")
        .expect("IF EXISTS must succeed, text family");
    db.execute_params("DROP TRIGGER IF EXISTS nope ON trg_t", &[])
        .expect("IF EXISTS must succeed, params family");
}

#[test]
fn drop_trigger_without_on_table_finds_the_trigger_by_name() {
    // Was: a parse error. Triggers run now, and the MySQL / SQLite spelling is taken.
    let db = mem_db();
    create_tables(&db);
    db.execute("DROP TRIGGER IF EXISTS whatever")
        .expect("IF EXISTS tolerates a missing trigger, text family");
    db.execute_params("DROP TRIGGER IF EXISTS whatever", &[])
        .expect("IF EXISTS tolerates a missing trigger, params family");
    assert!(
        db.execute("DROP TRIGGER whatever").is_err(),
        "a missing trigger is an error"
    );
    create_fn(&db, "trg_fn", SIDE_EFFECT_BODY);
    create_trigger_text(&db, "trg", "AFTER INSERT ON trg_t FOR EACH ROW", "trg_fn").expect("create");
    db.execute_params("DROP TRIGGER trg", &[]).expect("found by name");
    assert!(!db.trigger_registry.has_triggers_for_table("trg_t"));
}

#[test]
fn sqlite_style_inline_body_runs() {
    // Was: a parse error ("Expected EXECUTE"). Inline bodies run now.
    for params_family in [false, true] {
        let db = mem_db();
        create_tables(&db);
        let sql = "CREATE TRIGGER trg AFTER INSERT ON trg_t FOR EACH ROW
                   BEGIN INSERT INTO trg_audit (note) VALUES ('fired'); END";
        if params_family {
            db.execute_params(sql, &[]).expect("inline body, params family");
        } else {
            db.execute(sql).expect("inline body, text family");
        }
        assert!(db.trigger_registry.has_triggers_for_table("trg_t"));
        db.execute("INSERT INTO trg_t (id, tag) VALUES (1, 'a')")
            .expect("insert");
        assert_eq!(rows_in(&db, "trg_audit"), 1);
    }
}

#[test]
fn truncate_trigger_for_each_row_is_still_rejected() {
    let db = mem_db();
    create_tables(&db);
    create_fn(&db, "trg_fn", SIDE_EFFECT_BODY);

    let err = create_trigger_text(&db, "trg", "AFTER TRUNCATE ON trg_t FOR EACH ROW", "trg_fn")
        .expect_err("TRUNCATE + FOR EACH ROW must be rejected")
        .to_string();
    assert!(
        err.to_uppercase().contains("TRUNCATE"),
        "expected a TRUNCATE diagnostic: {err}"
    );
}

// ===========================================================================
// 2. BEFORE-INSERT row rewrite — parity across BOTH executor families
// ===========================================================================

#[test]
fn before_insert_rewrite_applies_on_the_text_family() {
    let db = mem_db();
    create_tables(&db);
    create_fn(&db, "mut_fn", REWRITE_BODY);
    create_trigger_text(&db, "trg", BEFORE_INSERT_ROW, "mut_fn").expect("create trigger");

    db.execute("INSERT INTO trg_t (id, tag) VALUES (1, 'original')")
        .expect("insert");

    assert_eq!(first_text(&db, "SELECT tag FROM trg_t WHERE id = 1"), "set-by-trigger");
}

#[test]
fn before_insert_rewrite_applies_on_the_params_family() {
    // This is the divergence that made a REST insert and a psql insert produce DIFFERENT
    // rows in the same table. The params family had no hook at all.
    let db = mem_db();
    create_tables(&db);
    create_fn(&db, "mut_fn", REWRITE_BODY);
    create_trigger_text(&db, "trg", BEFORE_INSERT_ROW, "mut_fn").expect("create trigger");

    db.execute_params("INSERT INTO trg_t (id, tag) VALUES (1, 'original')", &[])
        .expect("params insert");

    assert_eq!(first_text(&db, "SELECT tag FROM trg_t WHERE id = 1"), "set-by-trigger");
}

#[test]
fn before_insert_rewrite_applies_to_bound_parameters_including_null() {
    let db = mem_db();
    create_tables(&db);
    create_fn(&db, "mut_fn", REWRITE_BODY);
    create_trigger_text(&db, "trg", BEFORE_INSERT_ROW, "mut_fn").expect("create trigger");

    db.execute_params(
        "INSERT INTO trg_t (id, tag) VALUES ($1, $2)",
        &[Value::Int4(1), Value::String("original".to_string())],
    )
    .expect("bound insert");
    // A NULL bound parameter must be rewritten just the same.
    db.execute_params(
        "INSERT INTO trg_t (id, tag) VALUES ($1, $2)",
        &[Value::Int4(2), Value::Null],
    )
    .expect("bound insert with NULL");

    assert_eq!(first_text(&db, "SELECT tag FROM trg_t WHERE id = 1"), "set-by-trigger");
    assert_eq!(first_text(&db, "SELECT tag FROM trg_t WHERE id = 2"), "set-by-trigger");
}

#[test]
fn before_insert_rewrite_applies_to_every_row_of_a_multi_row_values_list() {
    let db = mem_db();
    create_tables(&db);
    create_fn(&db, "mut_fn", REWRITE_BODY);
    create_trigger_text(&db, "trg", BEFORE_INSERT_ROW, "mut_fn").expect("create trigger");

    db.execute_params("INSERT INTO trg_t (id, tag) VALUES (1, 'a'), (2, 'b'), (3, 'c')", &[])
        .expect("multi-row insert");

    assert_eq!(rows_in(&db, "trg_t"), 3, "all three rows written");
    for id in 1..=3 {
        assert_eq!(
            first_text(&db, &format!("SELECT tag FROM trg_t WHERE id = {id}")),
            "set-by-trigger",
            "row {id} must be rewritten"
        );
    }
}

#[test]
fn before_insert_rewrite_is_reflected_in_returning() {
    // `INSERT … RETURNING` routes through the params family on EVERY interface, so before
    // this fix even an embedded `execute_returning` skipped the rewrite the plain
    // `execute()` applied.
    let db = mem_db();
    create_tables(&db);
    create_fn(&db, "mut_fn", REWRITE_BODY);
    create_trigger_text(&db, "trg", BEFORE_INSERT_ROW, "mut_fn").expect("create trigger");

    let (count, rows) = db
        .execute_returning("INSERT INTO trg_t (id, tag) VALUES (1, 'original') RETURNING tag")
        .expect("insert returning");

    assert_eq!(count, 1);
    assert_eq!(rows.len(), 1, "one RETURNING tuple");
    assert_eq!(
        rows[0].values.first(),
        Some(&Value::String("set-by-trigger".to_string())),
        "RETURNING must reflect the rewritten row"
    );
    assert_eq!(first_text(&db, "SELECT tag FROM trg_t WHERE id = 1"), "set-by-trigger");
}

#[test]
fn rewrite_rhs_can_reference_another_new_column() {
    let db = mem_db();
    db.execute("CREATE TABLE ref_t (id INT, tag TEXT, copy TEXT)")
        .expect("table");
    create_fn(&db, "copy_fn", "BEGIN NEW.copy = NEW.tag; RETURN NEW; END");
    db.execute("CREATE TRIGGER trg BEFORE INSERT ON ref_t FOR EACH ROW EXECUTE FUNCTION copy_fn()")
        .expect("create trigger");

    db.execute("INSERT INTO ref_t (id, tag, copy) VALUES (1, 'hello', 'x')")
        .expect("text insert");
    db.execute_params("INSERT INTO ref_t (id, tag, copy) VALUES (2, 'world', 'x')", &[])
        .expect("params insert");

    assert_eq!(first_text(&db, "SELECT copy FROM ref_t WHERE id = 1"), "hello");
    assert_eq!(
        first_text(&db, "SELECT copy FROM ref_t WHERE id = 2"),
        "world",
        "the params family must resolve NEW.<col> exactly like the text family"
    );
}

#[test]
fn serial_pk_rows_are_written_the_same_way_by_both_families() {
    // The two families fill a SERIAL/IDENTITY primary key at DIFFERENT points (the text
    // family in `execute_in_transaction_inner`, the params family inside the storage
    // write). The binding invariant is that the BEFORE-row rewrite is applied on both and
    // that neither lets the trigger smuggle a primary-key value.
    let db = mem_db();
    db.execute("CREATE TABLE ser_t (id SERIAL PRIMARY KEY, tag TEXT)")
        .expect("table");
    create_fn(&db, "mut_fn", REWRITE_BODY);
    db.execute("CREATE TRIGGER trg BEFORE INSERT ON ser_t FOR EACH ROW EXECUTE FUNCTION mut_fn()")
        .expect("create trigger");

    db.execute("INSERT INTO ser_t (tag) VALUES ('original')")
        .expect("text insert");
    db.execute_params("INSERT INTO ser_t (tag) VALUES ('original')", &[])
        .expect("params insert");

    let rows = db.query("SELECT id, tag FROM ser_t", &[]).expect("select");
    assert_eq!(rows.len(), 2, "both inserts landed");

    let mut ids = Vec::new();
    for row in &rows {
        assert_eq!(
            row.values.get(1),
            Some(&Value::String("set-by-trigger".to_string())),
            "both families must apply the rewrite"
        );
        let id = row.values.first().cloned().expect("id column");
        assert!(
            !matches!(id, Value::Null),
            "the SERIAL primary key must be auto-filled on both families, got NULL"
        );
        ids.push(format!("{id:?}"));
    }
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 2, "the two generated primary keys must differ: {ids:?}");
}

// ===========================================================================
// 2b. ORDERING — the rewrite runs BEFORE constraint checking, on both families
// ===========================================================================
//
// PostgreSQL runs BEFORE ROW triggers BEFORE constraint checking, so a CHECK
// constraint must see the REWRITTEN row. The text family used to apply the
// rewrite AFTER the CHECK and UNIQUE gates while the params family applied it
// before, so the SAME `INSERT` was accepted over psql and rejected over
// psycopg/JDBC/sqlx — and, in the first case below, a CHECK-VIOLATING ROW WAS
// PERSISTED on the text family. These tests exist to keep the two orderings
// from drifting apart again; never assert one family alone here.

/// `trg_chk(id, tag CHECK (<check>))` with a BEFORE INSERT rewrite that always
/// sets `tag = 'set-by-trigger'`.
fn check_case_db(check: &str) -> EmbeddedDatabase {
    let db = mem_db();
    db.execute(&format!("CREATE TABLE trg_chk (id INT, tag TEXT CHECK ({check}))"))
        .unwrap_or_else(|e| panic!("subject table with CHECK ({check}): {e}"));
    create_fn(&db, "chk_fn", REWRITE_BODY);
    db.execute("CREATE TRIGGER trg BEFORE INSERT ON trg_chk FOR EACH ROW EXECUTE FUNCTION chk_fn()")
        .expect("create trigger");
    db
}

#[test]
fn a_rewrite_into_a_check_violation_is_rejected_by_both_families() {
    for params in [false, true] {
        let db = check_case_db("tag <> 'set-by-trigger'");
        let sql = "INSERT INTO trg_chk (id, tag) VALUES (1, 'ok')";
        let result = if params {
            db.execute_params(sql, &[])
        } else {
            db.execute(sql)
        };

        assert!(
            result.is_err(),
            "the CHECK must be evaluated against the REWRITTEN row and reject it (params = {params})"
        );
        assert_eq!(
            rows_in(&db, "trg_chk"),
            0,
            "a CHECK-violating row must never be persisted (params = {params})"
        );
    }
}

#[test]
fn a_rewrite_that_repairs_a_check_violation_is_accepted_by_both_families() {
    for params in [false, true] {
        let db = check_case_db("tag = 'set-by-trigger'");
        let sql = "INSERT INTO trg_chk (id, tag) VALUES (1, 'bad')";
        let result = if params {
            db.execute_params(sql, &[])
        } else {
            db.execute(sql)
        };

        result.unwrap_or_else(|e| panic!("the rewrite runs first, so the CHECK must pass (params = {params}): {e}"));
        assert_eq!(
            first_text(&db, "SELECT tag FROM trg_chk WHERE id = 1"),
            "set-by-trigger",
            "the repaired value must be the one stored (params = {params})"
        );
    }
}

/// The UNIQUE pre-check is built from the same row as the CHECK gate, so a
/// rewrite onto an already-taken value must be judged IDENTICALLY by the two
/// families. This asserts AGREEMENT rather than a specific verdict: the point
/// is parity, and pinning the verdict here would also pin UNIQUE-on-TEXT
/// enforcement, which is not what this suite is about.
#[test]
fn a_rewrite_onto_a_duplicate_unique_value_is_judged_the_same_by_both_families() {
    let outcome = |params: bool| -> (bool, usize) {
        let db = mem_db();
        db.execute("CREATE TABLE trg_uq (id INT PRIMARY KEY, tag TEXT UNIQUE)")
            .expect("subject table");
        create_fn(&db, "chk_fn", REWRITE_BODY);
        db.execute("CREATE TRIGGER trg BEFORE INSERT ON trg_uq FOR EACH ROW EXECUTE FUNCTION chk_fn()")
            .expect("create trigger");
        // Row 1 is itself rewritten to 'set-by-trigger', so row 2 collides only
        // if the UNIQUE gate sees the post-rewrite value.
        let first = if params {
            db.execute_params("INSERT INTO trg_uq (id, tag) VALUES (1, 'first')", &[])
        } else {
            db.execute("INSERT INTO trg_uq (id, tag) VALUES (1, 'first')")
        };
        first.expect("first row");

        let sql = "INSERT INTO trg_uq (id, tag) VALUES (2, 'second')";
        let result = if params {
            db.execute_params(sql, &[])
        } else {
            db.execute(sql)
        };
        (result.is_err(), rows_in(&db, "trg_uq"))
    };

    assert_eq!(
        outcome(false),
        outcome(true),
        "text family and params family must agree on a rewrite that collides with a UNIQUE value"
    );
}

// ===========================================================================
// 3. RETURN NULL suppression on BOTH families
// ===========================================================================

#[test]
fn return_null_suppresses_the_row_on_both_families() {
    let db = mem_db();
    create_tables(&db);
    create_fn(&db, "skip_fn", SKIP_BODY);
    create_trigger_text(&db, "trg", BEFORE_INSERT_ROW, "skip_fn").expect("create trigger");

    let text_count = db
        .execute("INSERT INTO trg_t (id, tag) VALUES (1, 'a')")
        .expect("text insert still succeeds");
    let params_count = db
        .execute_params("INSERT INTO trg_t (id, tag) VALUES (2, 'b')", &[])
        .expect("params insert still succeeds");

    assert_eq!(text_count, 0, "a suppressed row must not be counted, text family");
    assert_eq!(params_count, 0, "a suppressed row must not be counted, params family");
    assert_eq!(rows_in(&db, "trg_t"), 0, "no row may be persisted by either family");
}

#[test]
fn return_null_yields_no_returning_tuple() {
    let db = mem_db();
    create_tables(&db);
    create_fn(&db, "skip_fn", SKIP_BODY);
    create_trigger_text(&db, "trg", BEFORE_INSERT_ROW, "skip_fn").expect("create trigger");

    let (count, rows) = db
        .execute_returning("INSERT INTO trg_t (id, tag) VALUES (1, 'a') RETURNING tag")
        .expect("insert returning");

    assert_eq!(count, 0, "suppressed rows are not counted");
    assert!(rows.is_empty(), "a suppressed row contributes no RETURNING tuple");
    assert_eq!(rows_in(&db, "trg_t"), 0);
}

#[test]
fn a_when_gated_return_null_suppresses_only_the_matching_rows() {
    let db = mem_db();
    create_tables(&db);
    create_fn(&db, "skip_fn", SKIP_BODY);
    db.execute("CREATE TRIGGER trg BEFORE INSERT ON trg_t FOR EACH ROW WHEN (NEW.id > 10) EXECUTE FUNCTION skip_fn()")
        .expect("create trigger");

    let count = db
        .execute_params("INSERT INTO trg_t (id, tag) VALUES (1, 'a'), (99, 'b'), (2, 'c')", &[])
        .expect("mixed batch");

    assert_eq!(count, 2, "only the two rows failing WHEN survive");
    assert_eq!(rows_in(&db, "trg_t"), 2);
    assert_eq!(
        db.query("SELECT id FROM trg_t WHERE id = 99", &[])
            .expect("select")
            .len(),
        0,
        "the row satisfying WHEN must have been suppressed"
    );
}

// ===========================================================================
// 4. WHEN clause — honoured for the first time
// ===========================================================================

#[test]
fn when_clause_gates_the_rewrite_on_both_families() {
    // Before this change `apply_before_row_mutations` never looked at `when_condition`, so
    // the rewrite hit EVERY row regardless of the trigger's predicate.
    let db = mem_db();
    create_tables(&db);
    create_fn(&db, "mut_fn", REWRITE_BODY);
    db.execute("CREATE TRIGGER trg BEFORE INSERT ON trg_t FOR EACH ROW WHEN (NEW.id > 10) EXECUTE FUNCTION mut_fn()")
        .expect("create trigger");

    // Text family: one row satisfies WHEN, one does not.
    db.execute("INSERT INTO trg_t (id, tag) VALUES (99, 'original')")
        .expect("text insert, WHEN true");
    db.execute("INSERT INTO trg_t (id, tag) VALUES (1, 'original')")
        .expect("text insert, WHEN false");
    // Params family: the same two cases.
    db.execute_params("INSERT INTO trg_t (id, tag) VALUES (98, 'original')", &[])
        .expect("params insert, WHEN true");
    db.execute_params("INSERT INTO trg_t (id, tag) VALUES (2, 'original')", &[])
        .expect("params insert, WHEN false");

    assert_eq!(first_text(&db, "SELECT tag FROM trg_t WHERE id = 99"), "set-by-trigger");
    assert_eq!(first_text(&db, "SELECT tag FROM trg_t WHERE id = 98"), "set-by-trigger");
    assert_eq!(
        first_text(&db, "SELECT tag FROM trg_t WHERE id = 1"),
        "original",
        "text family: a row failing WHEN must be untouched"
    );
    assert_eq!(
        first_text(&db, "SELECT tag FROM trg_t WHERE id = 2"),
        "original",
        "params family: a row failing WHEN must be untouched"
    );
}

#[test]
fn a_when_clause_evaluating_to_null_does_not_fire() {
    // `NULL > 10` is NULL, not TRUE — PostgreSQL does not fire the trigger.
    let db = mem_db();
    create_tables(&db);
    create_fn(&db, "mut_fn", REWRITE_BODY);
    db.execute("CREATE TRIGGER trg BEFORE INSERT ON trg_t FOR EACH ROW WHEN (NEW.id > 10) EXECUTE FUNCTION mut_fn()")
        .expect("create trigger");

    db.execute_params(
        "INSERT INTO trg_t (id, tag) VALUES ($1, $2)",
        &[Value::Null, Value::String("original".to_string())],
    )
    .expect("insert with a NULL predicate column");

    assert_eq!(rows_in(&db, "trg_t"), 1, "the row is still inserted");
    assert_eq!(
        first_text(&db, "SELECT tag FROM trg_t"),
        "original",
        "a NULL WHEN result must not fire the rewrite"
    );
}

// ===========================================================================
// 5. STILL UNIMPLEMENTED — trigger bodies do not execute. These are the honest core.
// ===========================================================================

#[test]
fn a_side_effecting_body_runs_on_either_family() {
    // Was: no audit row (bodies did not run). The function runs once per inserted row now.
    let db = mem_db();
    create_tables(&db);
    create_fn(&db, "trg_fn", SIDE_EFFECT_BODY);
    create_trigger_text(&db, "trg", BEFORE_INSERT_ROW, "trg_fn").expect("create trigger");

    db.execute("INSERT INTO trg_t (id, tag) VALUES (1, 'a')")
        .expect("text insert");
    db.execute_params("INSERT INTO trg_t (id, tag) VALUES (2, 'b')", &[])
        .expect("params insert");

    assert_eq!(rows_in(&db, "trg_t"), 2, "the inserts themselves happen");
    assert_eq!(
        rows_in(&db, "trg_audit"),
        2,
        "one audit row per inserted row, on either family"
    );
}

#[test]
fn after_triggers_fire_for_insert_update_and_delete_on_either_family() {
    // Was: no audit row. AFTER row triggers run their function now.
    let db = mem_db();
    create_tables(&db);
    create_fn(
        &db,
        "trg_fn",
        "BEGIN INSERT INTO trg_audit (note) VALUES (TG_OP); RETURN NULL; END",
    );
    create_trigger_text(&db, "ai", "AFTER INSERT ON trg_t FOR EACH ROW", "trg_fn").expect("after insert");
    create_trigger_text(&db, "au", "AFTER UPDATE ON trg_t FOR EACH ROW", "trg_fn").expect("after update");
    create_trigger_text(&db, "ad", "AFTER DELETE ON trg_t FOR EACH ROW", "trg_fn").expect("after delete");

    db.execute("INSERT INTO trg_t (id, tag) VALUES (1, 'a')")
        .expect("insert");
    db.execute_params("UPDATE trg_t SET tag = 'b' WHERE id = 1", &[])
        .expect("update");
    db.execute("DELETE FROM trg_t WHERE id = 1").expect("delete");

    assert_eq!(rows_in(&db, "trg_audit"), 3, "one audit row per event");
    assert_eq!(
        rows_in(&db, "trg_t"),
        0,
        "an AFTER trigger's NULL return changes nothing"
    );
}

#[test]
fn statement_level_triggers_fire_once_per_statement() {
    // Was: no audit row. A statement-level trigger runs once, whatever the row count.
    let db = mem_db();
    create_tables(&db);
    create_fn(
        &db,
        "trg_fn",
        "BEGIN INSERT INTO trg_audit (note) VALUES ('fired'); RETURN NULL; END",
    );
    create_trigger_text(&db, "trg", "AFTER INSERT ON trg_t FOR EACH STATEMENT", "trg_fn").expect("create trigger");

    db.execute("INSERT INTO trg_t (id, tag) VALUES (1, 'a'), (2, 'b')")
        .expect("insert");

    assert_eq!(rows_in(&db, "trg_t"), 2);
    assert_eq!(rows_in(&db, "trg_audit"), 1);
}

#[test]
fn a_before_update_trigger_changes_the_row_on_both_families() {
    // Was: BEFORE UPDATE did nothing. It runs its function now.
    let db = mem_db();
    create_tables(&db);
    db.execute("INSERT INTO trg_t (id, tag) VALUES (1, 'original')")
        .expect("seed");
    db.execute("INSERT INTO trg_t (id, tag) VALUES (2, 'original')")
        .expect("seed");
    create_fn(&db, "mut_fn", REWRITE_BODY);
    create_trigger_text(&db, "trg", "BEFORE UPDATE ON trg_t FOR EACH ROW", "mut_fn").expect("create trigger");

    db.execute("UPDATE trg_t SET id = 11 WHERE id = 1")
        .expect("text update");
    db.execute_params("UPDATE trg_t SET id = 12 WHERE id = 2", &[])
        .expect("params update");

    assert_eq!(
        first_text(&db, "SELECT tag FROM trg_t WHERE id = 11"),
        "set-by-trigger",
        "text family"
    );
    assert_eq!(
        first_text(&db, "SELECT tag FROM trg_t WHERE id = 12"),
        "set-by-trigger",
        "params family"
    );
}

#[test]
fn an_update_only_trigger_does_not_fire_on_insert() {
    let db = mem_db();
    create_tables(&db);
    create_fn(&db, "mut_fn", REWRITE_BODY);
    create_trigger_text(&db, "trg", "BEFORE UPDATE ON trg_t FOR EACH ROW", "mut_fn").expect("create trigger");

    db.execute("INSERT INTO trg_t (id, tag) VALUES (1, 'original')")
        .expect("text insert");
    db.execute_params("INSERT INTO trg_t (id, tag) VALUES (2, 'original')", &[])
        .expect("params insert");

    assert_eq!(first_text(&db, "SELECT tag FROM trg_t WHERE id = 1"), "original");
    assert_eq!(first_text(&db, "SELECT tag FROM trg_t WHERE id = 2"), "original");
}

#[test]
fn pg_trigger_lists_the_trigger() {
    // Was: no such view. See tests/trigger_catalog_views.rs for the columns.
    let db = mem_db();
    create_tables(&db);
    create_fn(&db, "trg_fn", SIDE_EFFECT_BODY);
    create_trigger_text(&db, "trg", BEFORE_INSERT_ROW, "trg_fn").expect("create trigger");

    assert_eq!(db.query("SELECT * FROM pg_trigger", &[]).expect("pg_trigger").len(), 1);
}

// ===========================================================================
// 6. DROP TABLE lifecycle
// ===========================================================================

#[test]
fn drop_table_deregisters_triggers_on_both_families() {
    let db = mem_db();
    create_tables(&db);
    create_fn(&db, "mut_fn", REWRITE_BODY);
    create_trigger_text(&db, "trg", BEFORE_INSERT_ROW, "mut_fn").expect("create trigger");

    db.execute("DROP TABLE trg_t").expect("text family DROP TABLE");
    assert!(
        !db.trigger_registry.has_triggers_for_table("trg_t"),
        "DROP TABLE must deregister the table's triggers"
    );

    // The name is reusable, and the OLD rewrite must not follow the new table.
    db.execute("CREATE TABLE trg_t (id INT, tag TEXT)").expect("recreate");
    db.execute("INSERT INTO trg_t (id, tag) VALUES (1, 'original')")
        .expect("insert");
    assert_eq!(
        first_text(&db, "SELECT tag FROM trg_t WHERE id = 1"),
        "original",
        "the dropped trigger's rewrite recipe must be gone too"
    );

    create_trigger_text(&db, "trg", BEFORE_INSERT_ROW, "mut_fn").expect("the trigger name is reusable");
    db.execute_params("DROP TABLE trg_t", &[])
        .expect("params family DROP TABLE");
    assert!(
        !db.trigger_registry.has_triggers_for_table("trg_t"),
        "the params family must deregister too"
    );
}

/// PostgreSQL's comma list is planned as `DropMulti`, NOT `DropTable`. The
/// cleanup used to match `DropTable` only, so `DROP TABLE a, b` left the
/// dropped tables' triggers registered — in memory AND, now that triggers
/// persist, on disk forever.
#[test]
fn drop_table_comma_list_deregisters_triggers_on_both_families() {
    for params in [false, true] {
        let db = mem_db();
        create_tables(&db);
        db.execute("CREATE TABLE trg_t2 (id INT, tag TEXT)")
            .expect("second table");
        create_fn(&db, "mut_fn", REWRITE_BODY);
        create_trigger_text(&db, "trg", BEFORE_INSERT_ROW, "mut_fn").expect("create trigger");

        if params {
            db.execute_params("DROP TABLE trg_t, trg_t2", &[])
                .expect("params family DROP TABLE a, b");
        } else {
            db.execute("DROP TABLE trg_t, trg_t2")
                .expect("text family DROP TABLE a, b");
        }

        assert!(
            !db.trigger_registry.has_triggers_for_table("trg_t"),
            "DROP TABLE a, b must deregister a's triggers (params = {params})"
        );

        // The trigger name is reusable, and the old recipe must not follow the
        // recreated table.
        db.execute("CREATE TABLE trg_t (id INT, tag TEXT)").expect("recreate");
        create_trigger_text(&db, "trg", BEFORE_INSERT_ROW, "mut_fn")
            .unwrap_or_else(|e| panic!("the trigger name must be reusable (params = {params}): {e}"));
    }
}

// ===========================================================================
// 7. Restart durability
// ===========================================================================

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("nano_trigger_{tag}_{id}"));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

#[test]
fn triggers_and_their_rewrite_recipes_survive_a_restart() {
    let dir = scratch_dir("restart");
    {
        let db = EmbeddedDatabase::new(&dir).expect("open");
        create_tables(&db);
        create_fn(&db, "mut_fn", REWRITE_BODY);
        create_trigger_text(&db, "trg", BEFORE_INSERT_ROW, "mut_fn").expect("create trigger");
        db.execute("INSERT INTO trg_t (id, tag) VALUES (1, 'original')")
            .expect("insert before restart");
        assert_eq!(first_text(&db, "SELECT tag FROM trg_t WHERE id = 1"), "set-by-trigger");
    }

    {
        let db = EmbeddedDatabase::new(&dir).expect("reopen");
        assert!(
            db.trigger_registry.has_triggers_for_table("trg_t"),
            "the definition must be restored into the LIVE registry"
        );
        // The definition survived, so the name is taken.
        let err = create_trigger_text(&db, "trg", BEFORE_INSERT_ROW, "mut_fn")
            .expect_err("the restored trigger must still occupy its name")
            .to_string();
        assert!(err.contains("already exists"), "expected a name collision, got: {err}");

        // And the compiled rewrite recipe survived too, on BOTH families.
        db.execute("INSERT INTO trg_t (id, tag) VALUES (2, 'original')")
            .expect("text insert after restart");
        db.execute_params("INSERT INTO trg_t (id, tag) VALUES (3, 'original')", &[])
            .expect("params insert after restart");
        assert_eq!(
            first_text(&db, "SELECT tag FROM trg_t WHERE id = 2"),
            "set-by-trigger",
            "the rewrite recipe must survive the restart, text family"
        );
        assert_eq!(
            first_text(&db, "SELECT tag FROM trg_t WHERE id = 3"),
            "set-by-trigger",
            "the rewrite recipe must survive the restart, params family"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_dropped_trigger_does_not_come_back_after_a_restart() {
    let dir = scratch_dir("dropped");
    {
        let db = EmbeddedDatabase::new(&dir).expect("open");
        create_tables(&db);
        create_fn(&db, "mut_fn", REWRITE_BODY);
        create_trigger_text(&db, "trg", BEFORE_INSERT_ROW, "mut_fn").expect("create trigger");
        db.execute("DROP TRIGGER trg ON trg_t").expect("drop trigger");
    }

    {
        let db = EmbeddedDatabase::new(&dir).expect("reopen");
        assert!(
            !db.trigger_registry.has_triggers_for_table("trg_t"),
            "a dropped trigger must not be resurrected by the open-time loader"
        );
        db.execute("INSERT INTO trg_t (id, tag) VALUES (1, 'original')")
            .expect("insert");
        assert_eq!(
            first_text(&db, "SELECT tag FROM trg_t WHERE id = 1"),
            "original",
            "the dropped trigger's recipe must be gone from disk too"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn drop_table_removes_the_persisted_trigger_records() {
    let dir = scratch_dir("droptable");
    {
        let db = EmbeddedDatabase::new(&dir).expect("open");
        create_tables(&db);
        create_fn(&db, "mut_fn", REWRITE_BODY);
        create_trigger_text(&db, "trg", BEFORE_INSERT_ROW, "mut_fn").expect("create trigger");
        db.execute("DROP TABLE trg_t").expect("drop table");
    }

    {
        let db = EmbeddedDatabase::new(&dir).expect("reopen");
        assert!(
            !db.trigger_registry.has_triggers_for_table("trg_t"),
            "DROP TABLE must delete the persisted trigger records, not just the in-memory ones"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_in_memory_database_opens_with_no_triggers() {
    // The open-time loader runs for in-memory databases too; it must be a clean no-op.
    let db = mem_db();
    create_tables(&db);
    assert!(!db.trigger_registry.has_triggers_for_table("trg_t"));
    assert!(
        db.trigger_registry.list_all_triggers().expect("list").is_empty(),
        "a fresh in-memory database has no triggers"
    );
}

// ===========================================================================
// A recipe is the whole body or nothing
// ===========================================================================
//
// The rewrite recipe was read off the body one `;`-separated piece at a time,
// with no regard for the control flow around a piece: `IF … RETURN NULL; END IF;
// RETURN NEW` skipped every row, and an assignment under an `IF` was applied to
// every row. A body with control flow, a `RETURN` of anything but NEW or NULL, or
// a `RETURN` that is not its last statement now registers no recipe: the trigger
// is inert, which is what a body the engine cannot run has always been.

/// Run `check` once per executor family, on a fresh database with the trigger
/// function `body` on `trg_t`.
fn with_trigger_body(body: &str, check: impl Fn(&EmbeddedDatabase, &dyn Fn(&str))) {
    for params_family in [false, true] {
        let db = mem_db();
        create_tables(&db);
        create_fn(&db, "body_fn", body);
        create_trigger_text(&db, "trg", BEFORE_INSERT_ROW, "body_fn").expect("create trigger");
        let insert = |sql: &str| {
            let done = if params_family {
                db.execute_params(sql, &[])
            } else {
                db.execute(sql)
            };
            done.unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
        };
        check(&db, &insert);
    }
}

#[test]
fn a_conditional_return_null_does_not_drop_rows() {
    with_trigger_body(
        "BEGIN IF NEW.id < 0 THEN RAISE NOTICE 'skipping'; RETURN NULL; END IF; RETURN NEW; END",
        |db, insert| {
            insert("INSERT INTO trg_t (id, tag) VALUES (1, 'kept')");
            assert_eq!(rows_in(db, "trg_t"), 1);
            assert_eq!(first_text(db, "SELECT tag FROM trg_t WHERE id = 1"), "kept");
        },
    );
}

#[test]
fn a_conditional_assignment_is_not_applied_to_every_row() {
    with_trigger_body(
        "BEGIN IF NEW.tag IS NULL THEN NEW.tag = 'was-null'; END IF; RETURN NEW; END",
        |db, insert| {
            insert("INSERT INTO trg_t (id, tag) VALUES (1, 'original')");
            assert_eq!(first_text(db, "SELECT tag FROM trg_t WHERE id = 1"), "original");
        },
    );
}

#[test]
fn a_return_null_before_other_statements_skips_the_row() {
    // Was: the row was stored unchanged (no recipe). The function runs now: RETURN NULL
    // ends it and skips the row, as in PostgreSQL.
    with_trigger_body("BEGIN RETURN NULL; NEW.tag = 'late'; END", |db, insert| {
        insert("INSERT INTO trg_t (id, tag) VALUES (1, 'original')");
        assert_eq!(rows_in(db, "trg_t"), 0);
    });
}

#[test]
fn returning_a_whole_record_expression_is_an_error_not_a_silent_result() {
    // Was: the row was stored (the body was not run). A trigger function returns NEW, OLD
    // or NULL; any other whole-record expression fails the statement.
    for params_family in [false, true] {
        let db = mem_db();
        create_tables(&db);
        create_fn(&db, "body_fn", "BEGIN RETURN NULLIF(NEW, NEW); END");
        create_trigger_text(&db, "trg", BEFORE_INSERT_ROW, "body_fn").expect("create trigger");
        let sql = "INSERT INTO trg_t (id, tag) VALUES (1, 'original')";
        let done = if params_family {
            db.execute_params(sql, &[])
        } else {
            db.execute(sql)
        };
        assert!(done.is_err(), "the statement fails");
        assert_eq!(rows_in(&db, "trg_t"), 0);
    }
}

#[test]
fn a_case_expression_on_the_right_hand_side_still_rewrites() {
    with_trigger_body(
        "BEGIN NEW.tag = CASE WHEN NEW.id > 5 THEN 'big' ELSE 'small' END; RETURN NEW; END",
        |db, insert| {
            insert("INSERT INTO trg_t (id, tag) VALUES (1, 'x'), (9, 'y')");
            assert_eq!(first_text(db, "SELECT tag FROM trg_t WHERE id = 1"), "small");
            assert_eq!(first_text(db, "SELECT tag FROM trg_t WHERE id = 9"), "big");
        },
    );
}

#[test]
fn a_semicolon_inside_a_string_is_part_of_one_assignment() {
    with_trigger_body("BEGIN NEW.tag = 'a; RETURN NULL; b'; RETURN NEW; END", |db, insert| {
        insert("INSERT INTO trg_t (id, tag) VALUES (1, 'original')");
        assert_eq!(
            first_text(db, "SELECT tag FROM trg_t WHERE id = 1"),
            "a; RETURN NULL; b"
        );
    });
}

#[test]
fn a_function_body_that_does_not_parse_never_stores_a_half_changed_row() {
    // Was: the trigger was accepted and did nothing. Now CREATE TRIGGER refuses a function
    // whose body does not parse (or, failing that, the INSERT fails): never a silent row.
    let db = mem_db();
    create_tables(&db);
    create_fn(
        &db,
        "body_fn",
        "BEGIN NEW.tag = 'half'; NEW.id = (SELECT; RETURN NEW; END",
    );
    let created = create_trigger_text(&db, "trg", BEFORE_INSERT_ROW, "body_fn");
    let inserted = db.execute("INSERT INTO trg_t (id, tag) VALUES (1, 'original')");
    assert!(created.is_err() || inserted.is_err(), "one of the two statements fails");
    if created.is_ok() {
        assert_eq!(rows_in(&db, "trg_t"), 0);
    } else {
        assert_eq!(first_text(&db, "SELECT tag FROM trg_t WHERE id = 1"), "original");
    }
}

#[test]
fn straight_line_bodies_keep_their_recipe() {
    with_trigger_body(REWRITE_BODY, |db, insert| {
        insert("INSERT INTO trg_t (id, tag) VALUES (1, 'original')");
        assert_eq!(first_text(db, "SELECT tag FROM trg_t WHERE id = 1"), "set-by-trigger");
    });
    with_trigger_body(SKIP_BODY, |db, insert| {
        insert("INSERT INTO trg_t (id, tag) VALUES (1, 'original')");
        assert_eq!(rows_in(db, "trg_t"), 0);
    });
    // A statement without an effect on the row is ignored, as before.
    with_trigger_body(
        "BEGIN RAISE NOTICE 'seen'; NEW.tag := 'set'; RETURN NEW; END",
        |db, insert| {
            insert("INSERT INTO trg_t (id, tag) VALUES (1, 'original')");
            assert_eq!(first_text(db, "SELECT tag FROM trg_t WHERE id = 1"), "set");
        },
    );
}
