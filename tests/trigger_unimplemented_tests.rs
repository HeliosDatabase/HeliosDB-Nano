//! Triggers are NOT implemented — this suite pins the behaviour that ships today.
//!
//! HeliosDB Nano accepts and registers PostgreSQL-style `CREATE TRIGGER`, but it never
//! executes a trigger body. Two independent, unconditional breaks:
//!
//!   1. `Planner::create_trigger_to_plan` (`src/sql/planner.rs`) hardcodes
//!      `let body = vec![]; // Empty body for now - will be populated in Phase 2`, so every
//!      `TriggerDefinition` has an empty body and `execute_triggers`' `for stmt in
//!      &trigger_def.body` loop is structurally unreachable.
//!   2. The four DML call sites pass an `executor_fn` that discards the `TriggerRowContext`
//!      (`_ctx`) and calls `execute_plan_internal`, which takes no row context — so `NEW.col`
//!      / `OLD.col` could not resolve even if a body existed.
//!
//! WHY THIS SUITE EXISTS. It replaces ~2,600 lines across five deleted files
//! (`trigger_tests.rs`, `decimal_trigger_integration_tests.rs`, `trigger_hardening_tests.rs`,
//! `test_trigger_errors.rs`, `test_trigger_manual.rs`) that read as comprehensive, passing
//! coverage while asserting nothing: 46+ tests wrapped every assertion in `if result.is_ok()`
//! or `match res { Ok => …, Err => eprintln! }` guards that were always taken, 93 of their
//! trigger definitions used the SQLite `BEGIN … END` form this parser cannot parse, and two
//! of the files were `fn main()` scripts with zero `#[test]` and zero assertions.
//!
//! HOW TO MAINTAIN THIS FILE. Every test asserts unconditionally. Never reintroduce an
//! `is_ok()` guard, and never assert `rows.len() > 0` against `SELECT COUNT(*)` — a count
//! query returns exactly one row whether the count is 0 or 10,000, so that assertion is
//! always true and proves nothing. Use `rows_in()` below instead.
//!
//! IF A "MUST NOT FIRE" TEST FAILS BECAUSE ROWS APPEARED IN `trg_audit`: triggers have been
//! implemented. Do NOT "fix" the test. Delete this suite, write real trigger tests (fires
//! once per row, BEFORE vs AFTER ordering, NEW/OLD, WHEN, statement-level, cascade depth,
//! rollback behaviour), and update the docs that currently document triggers as
//! unimplemented: `README.md`, `AGENTS.md`, `docs/llms.txt`, `docs/guides/upgrade.md`,
//! `.claude/skills/heliosdb-nano-schema/SKILL.md` (Recipe 5),
//! `.claude/skills/heliosdb-nano-overview/SKILL.md`, `.claude/skills/_index/verb-map.md`,
//! and the three `examples/trigger_*.rs` demos.

use heliosdb_nano::{EmbeddedDatabase, Value};

fn db() -> EmbeddedDatabase {
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

/// Subject table `trg_t(id, tag)`, audit table `trg_audit(note)`, and a PL/pgSQL trigger
/// function `trg_fn()` whose body has a side effect. The function registers successfully;
/// nothing ever calls it.
fn setup(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE trg_t (id INT, tag TEXT)").unwrap();
    db.execute("CREATE TABLE trg_audit (note TEXT)").unwrap();
    let body = "BEGIN INSERT INTO trg_audit (note) VALUES ('fired'); RETURN NEW; END";
    let sql = format!("CREATE FUNCTION trg_fn() RETURNS TRIGGER AS $$ {body} $$ LANGUAGE plpgsql");
    db.execute(&sql)
        .expect("CREATE FUNCTION ... RETURNS TRIGGER must be accepted");
}

/// `CREATE TRIGGER <name> <clause> EXECUTE FUNCTION <func>()`.
fn create_trigger(db: &EmbeddedDatabase, name: &str, clause: &str, func: &str) -> heliosdb_nano::Result<u64> {
    db.execute(&format!("CREATE TRIGGER {name} {clause} EXECUTE FUNCTION {func}()"))
}

// ===========================================================================
// Grammar: what CREATE TRIGGER accepts and rejects
// ===========================================================================

#[test]
fn create_trigger_execute_function_succeeds_and_registers() {
    let db = db();
    setup(&db);

    create_trigger(&db, "trg", "AFTER INSERT ON trg_t FOR EACH ROW", "trg_fn")
        .expect("the PostgreSQL CREATE TRIGGER form must be accepted");

    assert!(db.trigger_registry.has_triggers_for_table("trg_t"), "must register");
    let triggers = db.trigger_registry.get_triggers_for_table("trg_t").unwrap();
    assert_eq!(triggers.len(), 1, "exactly one trigger expected");
    assert_eq!(triggers[0].name, "trg");
    // The registered body is always empty — this IS the reason triggers never fire.
    // If this ever holds statements, trigger execution has been implemented; see the
    // file header before touching anything.
    assert!(triggers[0].body.is_empty(), "registered body is always empty");
}

#[test]
fn every_trigger_grammar_variant_registers() {
    let db = db();
    setup(&db);

    // Each of these parses, plans, and registers. None of them ever fires.
    create_trigger(&db, "v1", "BEFORE INSERT ON trg_t FOR EACH ROW", "trg_fn").unwrap();
    create_trigger(&db, "v2", "AFTER UPDATE ON trg_t FOR EACH ROW", "trg_fn").unwrap();
    create_trigger(&db, "v3", "INSTEAD OF DELETE ON trg_t FOR EACH ROW", "trg_fn").unwrap();
    create_trigger(&db, "v4", "AFTER INSERT ON trg_t FOR EACH STATEMENT", "trg_fn").unwrap();
    create_trigger(&db, "v5", "AFTER UPDATE OF tag ON trg_t FOR EACH ROW", "trg_fn").unwrap();
    create_trigger(
        &db,
        "v6",
        "AFTER INSERT OR UPDATE OR DELETE ON trg_t FOR EACH ROW",
        "trg_fn",
    )
    .unwrap();
    create_trigger(
        &db,
        "v7",
        "AFTER INSERT ON trg_t FOR EACH ROW WHEN (NEW.id > 0)",
        "trg_fn",
    )
    .unwrap();

    let registered = db.trigger_registry.get_triggers_for_table("trg_t").unwrap();
    assert_eq!(registered.len(), 7, "every grammar variant should register");
}

#[test]
fn sqlite_style_begin_end_body_does_not_parse() {
    let db = db();
    setup(&db);

    // sqlparser's CREATE TRIGGER grammar is PostgreSQL-shaped and requires
    // `EXECUTE {FUNCTION|PROCEDURE}`. There is no inline-body production at all, so the
    // SQLite/MySQL form that every migration reaches for is a hard parse error.
    let res = db.execute(
        "CREATE TRIGGER trg AFTER INSERT ON trg_t FOR EACH ROW
         BEGIN INSERT INTO trg_audit (note) VALUES ('fired'); END",
    );
    let err = res.expect_err("SQLite-style inline body must not parse").to_string();

    assert!(err.contains("EXECUTE"), "parser should demand EXECUTE, got: {err}");
    assert!(
        !db.trigger_registry.has_triggers_for_table("trg_t"),
        "nothing registered"
    );
}

#[test]
fn create_trigger_if_not_exists_does_not_parse() {
    let db = db();
    setup(&db);

    // `IF NOT EXISTS` is not part of the CREATE TRIGGER grammar (PostgreSQL has no such
    // clause either). `CREATE OR REPLACE TRIGGER` is the accepted idempotent form.
    let res = create_trigger(&db, "IF NOT EXISTS trg", "AFTER INSERT ON trg_t FOR EACH ROW", "trg_fn");
    assert!(res.is_err(), "CREATE TRIGGER IF NOT EXISTS must be a parse error");
    assert!(
        !db.trigger_registry.has_triggers_for_table("trg_t"),
        "nothing registered"
    );
}

#[test]
fn truncate_trigger_for_each_row_is_rejected() {
    // Preserved from the deleted `test_trigger_errors.rs`, which checked this without
    // ever asserting it (it only printed).
    let db = db();
    setup(&db);

    let res = create_trigger(&db, "trg", "AFTER TRUNCATE ON trg_t FOR EACH ROW", "trg_fn");
    let err = res.expect_err("TRUNCATE + FOR EACH ROW must be rejected").to_string();
    assert!(
        err.to_uppercase().contains("TRUNCATE"),
        "expected TRUNCATE diagnostic: {err}"
    );
}

#[test]
fn duplicate_create_trigger_is_rejected() {
    let db = db();
    setup(&db);

    let clause = "AFTER INSERT ON trg_t FOR EACH ROW";
    create_trigger(&db, "trg", clause, "trg_fn").expect("first create");
    let res = create_trigger(&db, "trg", clause, "trg_fn");

    let err = res.expect_err("re-creating the same trigger must fail").to_string();
    assert!(err.contains("already exists"), "expected 'already exists', got: {err}");
    let triggers = db.trigger_registry.get_triggers_for_table("trg_t").unwrap();
    assert_eq!(triggers.len(), 1, "the failed create must not add a registration");
}

#[test]
fn create_or_replace_trigger_is_accepted_twice() {
    let db = db();
    setup(&db);

    let sql = "CREATE OR REPLACE TRIGGER trg AFTER INSERT ON trg_t FOR EACH ROW EXECUTE FUNCTION trg_fn()";
    db.execute(sql).expect("first create");
    db.execute(sql)
        .expect("OR REPLACE must not error on an existing trigger");

    let triggers = db.trigger_registry.get_triggers_for_table("trg_t").unwrap();
    assert_eq!(triggers.len(), 1, "OR REPLACE must leave exactly one registration");
}

// ===========================================================================
// Execution: a registered trigger never fires. This is the headline pin.
//
// Every assertion below says "the audit table is still empty". If one fails because
// rows appeared, trigger execution has been implemented — rewrite this suite rather
// than relaxing the test (see the file header).
// ===========================================================================

#[test]
fn after_insert_trigger_never_fires() {
    let db = db();
    setup(&db);
    create_trigger(&db, "trg", "AFTER INSERT ON trg_t FOR EACH ROW", "trg_fn").unwrap();
    assert!(db.trigger_registry.has_triggers_for_table("trg_t"), "must register");

    db.execute("INSERT INTO trg_t (id, tag) VALUES (1, 'a')").unwrap();

    assert_eq!(rows_in(&db, "trg_t"), 1, "the INSERT itself must still happen");
    assert_eq!(rows_in(&db, "trg_audit"), 0, "AFTER INSERT body must not have run");
}

#[test]
fn after_update_trigger_never_fires() {
    let db = db();
    setup(&db);
    db.execute("INSERT INTO trg_t (id, tag) VALUES (1, 'a')").unwrap();
    create_trigger(&db, "trg", "AFTER UPDATE ON trg_t FOR EACH ROW", "trg_fn").unwrap();

    db.execute("UPDATE trg_t SET tag = 'b' WHERE id = 1").unwrap();

    assert_eq!(first_text(&db, "SELECT tag FROM trg_t WHERE id = 1"), "b");
    assert_eq!(rows_in(&db, "trg_audit"), 0, "AFTER UPDATE body must not have run");
}

#[test]
fn after_delete_trigger_never_fires() {
    let db = db();
    setup(&db);
    db.execute("INSERT INTO trg_t (id, tag) VALUES (1, 'a')").unwrap();
    create_trigger(&db, "trg", "AFTER DELETE ON trg_t FOR EACH ROW", "trg_fn").unwrap();

    db.execute("DELETE FROM trg_t WHERE id = 1").unwrap();

    assert_eq!(rows_in(&db, "trg_t"), 0, "the DELETE itself must still happen");
    assert_eq!(rows_in(&db, "trg_audit"), 0, "AFTER DELETE body must not have run");
}

#[test]
fn before_insert_side_effect_body_never_runs() {
    let db = db();
    setup(&db);
    create_trigger(&db, "trg", "BEFORE INSERT ON trg_t FOR EACH ROW", "trg_fn").unwrap();

    db.execute("INSERT INTO trg_t (id, tag) VALUES (1, 'a')").unwrap();

    // BEFORE + FOR EACH ROW + INSERT is the one combination with a live mechanism (see
    // the NEW-assignment tests below), but that mechanism only recognises
    // `NEW.<col> = <expr>` and `RETURN NULL`. A side-effecting body is silently ignored.
    assert_eq!(rows_in(&db, "trg_t"), 1, "the INSERT itself must still happen");
    assert_eq!(rows_in(&db, "trg_audit"), 0, "BEFORE INSERT body must not have run");
}

#[test]
fn statement_level_trigger_never_fires() {
    let db = db();
    setup(&db);
    create_trigger(&db, "trg", "AFTER INSERT ON trg_t FOR EACH STATEMENT", "trg_fn").unwrap();

    db.execute("INSERT INTO trg_t (id, tag) VALUES (1, 'a'), (2, 'b')")
        .unwrap();

    assert_eq!(rows_in(&db, "trg_t"), 2, "both rows must be inserted");
    assert_eq!(rows_in(&db, "trg_audit"), 0, "statement-level body must not have run");
}

// ===========================================================================
// The one narrow mechanism that DOES have an effect: BEFORE + FOR EACH ROW + INSERT
// with a `NEW.<col> = <expr>` / `RETURN NULL` body. This is a text scan of the function
// body (`parse_trigger_assignments`, `src/lib.rs`), not execution. It had zero test
// coverage anywhere in the suite before this file.
// ===========================================================================

#[test]
fn before_insert_new_column_assignment_is_applied() {
    let db = db();
    db.execute("CREATE TABLE trg_t (id INT, tag TEXT)").unwrap();
    let body = "BEGIN NEW.tag = 'set-by-trigger'; RETURN NEW; END";
    let sql = format!("CREATE FUNCTION mut_fn() RETURNS TRIGGER AS $$ {body} $$ LANGUAGE plpgsql");
    db.execute(&sql).unwrap();
    create_trigger(&db, "trg", "BEFORE INSERT ON trg_t FOR EACH ROW", "mut_fn").unwrap();

    db.execute("INSERT INTO trg_t (id, tag) VALUES (1, 'original')")
        .unwrap();

    let tag = first_text(&db, "SELECT tag FROM trg_t WHERE id = 1");
    assert_eq!(tag, "set-by-trigger", "BEFORE INSERT NEW.<col> assignment must apply");
}

#[test]
fn before_insert_return_null_skips_the_row() {
    let db = db();
    db.execute("CREATE TABLE trg_t (id INT, tag TEXT)").unwrap();
    let body = "BEGIN RETURN NULL; END";
    let sql = format!("CREATE FUNCTION skip_fn() RETURNS TRIGGER AS $$ {body} $$ LANGUAGE plpgsql");
    db.execute(&sql).unwrap();
    create_trigger(&db, "trg", "BEFORE INSERT ON trg_t FOR EACH ROW", "skip_fn").unwrap();

    db.execute("INSERT INTO trg_t (id, tag) VALUES (1, 'a')")
        .expect("statement still succeeds");

    assert_eq!(rows_in(&db, "trg_t"), 0, "RETURN NULL must skip the inserted row");
}

#[test]
fn before_update_new_column_assignment_is_not_applied() {
    let db = db();
    db.execute("CREATE TABLE trg_t (id INT, tag TEXT)").unwrap();
    db.execute("INSERT INTO trg_t (id, tag) VALUES (1, 'original')")
        .unwrap();
    let body = "BEGIN NEW.tag = 'set-by-trigger'; RETURN NEW; END";
    let sql = format!("CREATE FUNCTION mut_fn() RETURNS TRIGGER AS $$ {body} $$ LANGUAGE plpgsql");
    db.execute(&sql).unwrap();
    create_trigger(&db, "trg", "BEFORE UPDATE ON trg_t FOR EACH ROW", "mut_fn").unwrap();

    db.execute("UPDATE trg_t SET id = 2 WHERE id = 1").unwrap();

    // The row-mutation recipe is registered for any BEFORE + FOR EACH ROW trigger, but
    // `apply_before_row_mutations` has exactly one call site and it is on the INSERT path.
    let tag = first_text(&db, "SELECT tag FROM trg_t WHERE id = 2");
    assert_eq!(tag, "original", "the row-mutation mechanism is INSERT-only");
}

// ===========================================================================
// Lifecycle: DROP TRIGGER / DROP TABLE. These genuinely work.
// ===========================================================================

#[test]
fn drop_trigger_deregisters() {
    let db = db();
    setup(&db);
    create_trigger(&db, "trg", "AFTER INSERT ON trg_t FOR EACH ROW", "trg_fn").unwrap();
    assert!(db.trigger_registry.has_triggers_for_table("trg_t"), "must register");

    db.execute("DROP TRIGGER trg ON trg_t")
        .expect("DROP TRIGGER must succeed");

    assert!(!db.trigger_registry.has_triggers_for_table("trg_t"), "must deregister");
    let looked_up = db.trigger_registry.get_trigger("trg_t", "trg").unwrap();
    assert!(looked_up.is_none(), "the dropped trigger must not be retrievable");
}

#[test]
fn drop_trigger_if_exists_nonexistent() {
    // Preserved from the deleted `trigger_hardening_tests.rs`: of its 29 tests this was
    // the only one that asserted unconditionally.
    let db = db();
    db.execute("CREATE TABLE trg_t (id INT)").unwrap();
    let res = db.execute("DROP TRIGGER IF EXISTS trg_nonexistent ON trg_t");
    assert!(
        res.is_ok(),
        "DROP TRIGGER IF EXISTS must not error when missing: {res:?}"
    );
}

#[test]
fn drop_trigger_without_if_exists_errors_when_missing() {
    let db = db();
    db.execute("CREATE TABLE trg_t (id INT)").unwrap();

    let res = db.execute("DROP TRIGGER trg_nonexistent ON trg_t");
    let err = res
        .expect_err("DROP TRIGGER must fail on a missing trigger")
        .to_string();
    assert!(err.contains("does not exist"), "expected 'does not exist', got: {err}");
}

#[test]
fn drop_trigger_requires_on_table_clause() {
    let db = db();
    db.execute("CREATE TABLE trg_t (id INT)").unwrap();

    // The SQLite form (`DROP TRIGGER <name>`) omits `ON <table>`; this parser requires it.
    let res = db.execute("DROP TRIGGER IF EXISTS trg_whatever");
    assert!(res.is_err(), "DROP TRIGGER without ON <table> must not parse");
}

#[test]
fn drop_table_leaves_a_stale_trigger_registration() {
    // `DROP TABLE` does NOT cascade to the table's triggers — measured, not assumed.
    // The registration outlives the table, and the consequence is user-visible: the
    // trigger name cannot be reused after re-creating the table.
    let db = db();
    setup(&db);
    create_trigger(&db, "trg", "AFTER INSERT ON trg_t FOR EACH ROW", "trg_fn").unwrap();
    assert!(db.trigger_registry.has_triggers_for_table("trg_t"), "must register");

    db.execute("DROP TABLE trg_t").unwrap();

    assert!(
        db.trigger_registry.has_triggers_for_table("trg_t"),
        "DROP TABLE leaves the registration behind (it does not cascade)"
    );
    let stale = db.trigger_registry.get_triggers_for_table("trg_t").unwrap();
    assert_eq!(stale.len(), 1, "exactly the one stale registration remains");

    // The name is now burned for this process: re-creating the table does not clear it.
    db.execute("CREATE TABLE trg_t (id INT, tag TEXT)").unwrap();
    let err = create_trigger(&db, "trg", "AFTER INSERT ON trg_t FOR EACH ROW", "trg_fn")
        .expect_err("the stale registration must still collide")
        .to_string();
    assert!(err.contains("already exists"), "expected a name collision, got: {err}");

    // `DROP TRIGGER` is the only way out.
    db.execute("DROP TRIGGER trg ON trg_t").unwrap();
    create_trigger(&db, "trg", "AFTER INSERT ON trg_t FOR EACH ROW", "trg_fn")
        .expect("after DROP TRIGGER the name is reusable");
}

// ===========================================================================
// Introspection: there is none.
// ===========================================================================

#[test]
fn pg_trigger_catalog_view_does_not_exist() {
    let db = db();
    setup(&db);
    create_trigger(&db, "trg", "AFTER INSERT ON trg_t FOR EACH ROW", "trg_fn").unwrap();

    // No `pg_trigger` relation exists anywhere in the crate, so a registered trigger is
    // invisible to every catalog client. (`information_schema.triggers` is an explicitly
    // empty whitelisted view on the wire path, and `pg_class.relhastriggers` is hardcoded
    // `false`.) If this ever succeeds, trigger introspection was added and the docs that
    // say otherwise need updating.
    let res = db.query("SELECT * FROM pg_trigger", &[]);
    assert!(res.is_err(), "pg_trigger is not implemented");
}
