//! `INSERT … SELECT` row assembly and constraint enforcement, pinned on BOTH
//! DML executor families (tasks #101, #102, #84).
//!
//! HeliosDB Nano has two parallel DML executors:
//!   * "text family"   — `db.execute()`        → `execute_in_transaction_inner`
//!                        (embedded, psql simple-query, the whole MySQL wire, the REPL)
//!   * "params family" — `db.execute_params()` → `execute_params_inner`
//!                        → `execute_plan_with_params_inner`
//!                        (PG EXTENDED protocol — psycopg3 server-side bind, JDBC, sqlx,
//!                         Drizzle, node-postgres — plus every REST/BaaS write, and
//!                         `CREATE TABLE … AS`, which re-enters that arm to populate)
//!
//! WHAT WAS BROKEN. The two `InsertSelect` arms were independent implementations of one
//! rule and had decayed apart:
//!
//!   * #101 — the params arm built the row with `Vec::new()` + `push`, using the INSERT
//!     column list ONLY to choose a cast type. `INSERT INTO t (b, a) SELECT x, y FROM s`
//!     therefore stored `x` in the FIRST column. With type-compatible columns there was no
//!     error at all: the values were simply swapped. Silent data corruption, on the family
//!     every real driver uses.
//!   * #102 — that same arm's entire constraint surface was one `CHECK` call. No NOT NULL,
//!     no FOREIGN KEY, no table-level UNIQUE, no DEFAULT fill, no missing-non-nullable
//!     error. Over the extended protocol `INSERT INTO child SELECT …` created orphan rows
//!     past a FOREIGN KEY and wrote NULL into NOT NULL columns.
//!   * #84 — NEITHER arm applied the BEFORE-row rewrite recipe. Not a family divergence:
//!     both were wrong the same way, which is exactly why the both-families parity suites
//!     missed it.
//!
//! THE FIX is ONE shared gate (`InsertSelectGate` / `build_insert_select_row`, `src/lib.rs`)
//! called from both arms. It assembles and validates; it does NOT write — each arm keeps its
//! own `insert_tuple_branch_aware_with_schema` call.
//!
//! HOW TO MAINTAIN THIS FILE. Every test asserts unconditionally — never wrap an assertion
//! in `if result.is_ok()`, and never assert on `SELECT COUNT(*)` row counts (a count query
//! returns exactly one row whether the count is 0 or 10,000): use `rows_in`. Each violating
//! statement below inserts exactly ONE row, so "the table is unchanged afterwards" is a real
//! assertion and does not depend on statement-level atomicity (which INSERT … SELECT does
//! not have — see the pinned #100 test at the bottom).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

fn mem_db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory db")
}

/// Rows physically present in `table`. Deliberately NOT `SELECT COUNT(*)`.
fn rows_in(db: &EmbeddedDatabase, table: &str) -> usize {
    let sql = format!("SELECT * FROM {table}");
    db.query(&sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
        .len()
}

/// Every value of one column, in the order the scan returns them.
fn column(db: &EmbeddedDatabase, sql: &str) -> Vec<Value> {
    db.query(sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
        .iter()
        .map(|row| row.values.first().cloned().unwrap_or(Value::Null))
        .collect()
}

fn text(s: &str) -> Value {
    Value::String(s.to_string())
}

// ===========================================================================
// #101 — a column list must place values in the NAMED columns, not positionally
// ===========================================================================

/// The regression test for #101. Both columns are TEXT, so the pre-fix params arm
/// produced NO error — it just swapped the two values.
#[test]
fn column_list_permutation_lands_in_the_named_columns_on_both_families() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute("CREATE TABLE perm_src (x TEXT, y TEXT)").unwrap();
        db.execute("INSERT INTO perm_src (x, y) VALUES ('X-value', 'Y-value')")
            .unwrap();
        db.execute("CREATE TABLE perm_dst (a TEXT, b TEXT)").unwrap();
    }

    // --- text family ---
    let db = mem_db();
    setup(&db);
    db.execute("INSERT INTO perm_dst (b, a) SELECT x, y FROM perm_src")
        .expect("text family INSERT … SELECT with a permuted column list");
    let text_a = column(&db, "SELECT a FROM perm_dst");
    let text_b = column(&db, "SELECT b FROM perm_dst");

    // --- params family: the SAME statement over the extended protocol ---
    let db2 = mem_db();
    setup(&db2);
    db2.execute_params("INSERT INTO perm_dst (b, a) SELECT x, y FROM perm_src", &[])
        .expect("params family INSERT … SELECT with a permuted column list");
    let params_a = column(&db2, "SELECT a FROM perm_dst");
    let params_b = column(&db2, "SELECT b FROM perm_dst");

    assert_eq!(
        params_a, text_a,
        "DIVERGENCE in column `a`: text family stored {text_a:?}, params family {params_a:?}"
    );
    assert_eq!(
        params_b, text_b,
        "DIVERGENCE in column `b`: text family stored {text_b:?}, params family {params_b:?}"
    );
    assert_eq!(
        text_a,
        vec![text("Y-value")],
        "`INSERT INTO perm_dst (b, a) SELECT x, y` must put y in a"
    );
    assert_eq!(
        text_b,
        vec![text("X-value")],
        "`INSERT INTO perm_dst (b, a) SELECT x, y` must put x in b"
    );
}

/// A column list naming a SUBSET must leave the unlisted column at its DEFAULT — the
/// pre-fix params arm produced a short tuple with no fill at all.
#[test]
fn default_fills_an_unlisted_column_on_both_families() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute("CREATE TABLE def_src (id INT, name TEXT)").unwrap();
        db.execute("INSERT INTO def_src (id, name) VALUES (1, 'Alice')")
            .unwrap();
        db.execute("CREATE TABLE def_dst (id INT PRIMARY KEY, name TEXT, tag TEXT DEFAULT 'from-default')")
            .unwrap();
    }

    let db = mem_db();
    setup(&db);
    db.execute("INSERT INTO def_dst (id, name) SELECT id, name FROM def_src")
        .expect("text family");
    let text_tag = column(&db, "SELECT tag FROM def_dst");

    let db2 = mem_db();
    setup(&db2);
    db2.execute_params("INSERT INTO def_dst (id, name) SELECT id, name FROM def_src", &[])
        .expect("params family");
    let params_tag = column(&db2, "SELECT tag FROM def_dst");

    assert_eq!(
        params_tag, text_tag,
        "DIVERGENCE: text family stored tag={text_tag:?}, params family {params_tag:?}"
    );
    assert_eq!(
        text_tag,
        vec![text("from-default")],
        "an unlisted column with a DEFAULT must receive it"
    );
}

/// An unknown column name must ERROR. `filter_map(get_column_index)` used to drop it
/// silently and shift every later value one column left.
#[test]
fn an_unknown_column_name_errors_on_both_families() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute("CREATE TABLE unk_src (id INT, name TEXT)").unwrap();
        db.execute("INSERT INTO unk_src (id, name) VALUES (1, 'Alice')")
            .unwrap();
        db.execute("CREATE TABLE unk_dst (id INT, name TEXT)").unwrap();
    }

    let db = mem_db();
    setup(&db);
    let text_err = db
        .execute("INSERT INTO unk_dst (id, nosuchcol) SELECT id, name FROM unk_src")
        .expect_err("an unknown target column must be rejected (text family)")
        .to_string();
    assert_eq!(
        rows_in(&db, "unk_dst"),
        0,
        "nothing may be written for a rejected INSERT"
    );

    let db2 = mem_db();
    setup(&db2);
    let params_err = db2
        .execute_params("INSERT INTO unk_dst (id, nosuchcol) SELECT id, name FROM unk_src", &[])
        .expect_err("an unknown target column must be rejected (params family)")
        .to_string();
    assert_eq!(
        rows_in(&db2, "unk_dst"),
        0,
        "nothing may be written for a rejected INSERT"
    );

    assert!(
        text_err.contains("nosuchcol"),
        "the error must name the offending column, got: {text_err}"
    );
    assert!(
        params_err.contains("nosuchcol"),
        "the error must name the offending column, got: {params_err}"
    );
}

// ===========================================================================
// #102 — the constraint surface, on both families
// ===========================================================================

#[test]
fn not_null_is_enforced_on_both_families() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute("CREATE TABLE nn_src (id INT, name TEXT)").unwrap();
        db.execute("INSERT INTO nn_src (id, name) VALUES (1, NULL)").unwrap();
        db.execute("CREATE TABLE nn_dst (id INT PRIMARY KEY, name TEXT NOT NULL)")
            .unwrap();
    }

    let db = mem_db();
    setup(&db);
    let text_rejected = db
        .execute("INSERT INTO nn_dst (id, name) SELECT id, name FROM nn_src")
        .is_err();
    let text_rows = rows_in(&db, "nn_dst");

    let db2 = mem_db();
    setup(&db2);
    let params_rejected = db2
        .execute_params("INSERT INTO nn_dst (id, name) SELECT id, name FROM nn_src", &[])
        .is_err();
    let params_rows = rows_in(&db2, "nn_dst");

    assert_eq!(
        params_rejected, text_rejected,
        "DIVERGENCE: text rejected={text_rejected}, params rejected={params_rejected}"
    );
    assert!(text_rejected, "NULL into a NOT NULL column must be rejected");
    assert_eq!(params_rows, text_rows, "DIVERGENCE in rows left behind");
    assert_eq!(text_rows, 0, "the rejected row must not be written");
}

#[test]
fn a_missing_non_nullable_column_errors_on_both_families() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute("CREATE TABLE miss_src (id INT, note TEXT)").unwrap();
        db.execute("INSERT INTO miss_src (id, note) VALUES (1, 'n')").unwrap();
        // `name` is NOT NULL, has no DEFAULT, and is not named by the INSERT.
        db.execute("CREATE TABLE miss_dst (id INT PRIMARY KEY, name TEXT NOT NULL, note TEXT)")
            .unwrap();
    }

    let db = mem_db();
    setup(&db);
    let text_rejected = db
        .execute("INSERT INTO miss_dst (id, note) SELECT id, note FROM miss_src")
        .is_err();
    let text_rows = rows_in(&db, "miss_dst");

    let db2 = mem_db();
    setup(&db2);
    let params_rejected = db2
        .execute_params("INSERT INTO miss_dst (id, note) SELECT id, note FROM miss_src", &[])
        .is_err();
    let params_rows = rows_in(&db2, "miss_dst");

    assert_eq!(
        params_rejected, text_rejected,
        "DIVERGENCE: text rejected={text_rejected}, params rejected={params_rejected}"
    );
    assert!(
        text_rejected,
        "an omitted NOT NULL column with no DEFAULT must be rejected"
    );
    assert_eq!(params_rows, text_rows, "DIVERGENCE in rows left behind");
    assert_eq!(text_rows, 0, "the rejected row must not be written");
}

#[test]
fn foreign_keys_are_enforced_on_both_families() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute("CREATE TABLE fk_parent (id INT PRIMARY KEY)").unwrap();
        db.execute("CREATE TABLE fk_child (id INT PRIMARY KEY, p_id INT REFERENCES fk_parent(id))")
            .unwrap();
        db.execute("CREATE TABLE fk_stage (id INT, p_id INT)").unwrap();
        // 999 has no parent row.
        db.execute("INSERT INTO fk_stage (id, p_id) VALUES (10, 999)").unwrap();
    }

    let db = mem_db();
    setup(&db);
    let text_rejected = db
        .execute("INSERT INTO fk_child (id, p_id) SELECT id, p_id FROM fk_stage")
        .is_err();
    let text_rows = rows_in(&db, "fk_child");

    let db2 = mem_db();
    setup(&db2);
    let params_rejected = db2
        .execute_params("INSERT INTO fk_child (id, p_id) SELECT id, p_id FROM fk_stage", &[])
        .is_err();
    let params_rows = rows_in(&db2, "fk_child");

    assert_eq!(
        params_rejected, text_rejected,
        "DIVERGENCE: text rejected={text_rejected}, params rejected={params_rejected} \
         (a false on the params side is an ORPHAN row past a FOREIGN KEY)"
    );
    assert!(text_rejected, "a row with no parent must be rejected");
    assert_eq!(params_rows, text_rows, "DIVERGENCE in rows left behind");
    assert_eq!(text_rows, 0, "the orphan row must not be written");
}

/// A FOREIGN KEY that IS satisfied must still be accepted — the enforcement added for
/// #102 must not turn into a phantom rejection.
#[test]
fn a_satisfied_foreign_key_is_accepted_on_both_families() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute("CREATE TABLE fkok_parent (id INT PRIMARY KEY)").unwrap();
        db.execute("CREATE TABLE fkok_child (id INT PRIMARY KEY, p_id INT REFERENCES fkok_parent(id))")
            .unwrap();
        db.execute("CREATE TABLE fkok_stage (id INT, p_id INT)").unwrap();
        db.execute("INSERT INTO fkok_parent (id) VALUES (1)").unwrap();
        db.execute("INSERT INTO fkok_stage (id, p_id) VALUES (10, 1)").unwrap();
    }

    let db = mem_db();
    setup(&db);
    db.execute("INSERT INTO fkok_child (id, p_id) SELECT id, p_id FROM fkok_stage")
        .expect("text family: the parent exists");
    let text_rows = rows_in(&db, "fkok_child");

    let db2 = mem_db();
    setup(&db2);
    db2.execute_params("INSERT INTO fkok_child (id, p_id) SELECT id, p_id FROM fkok_stage", &[])
        .expect("params family: the parent exists");
    let params_rows = rows_in(&db2, "fkok_child");

    assert_eq!(params_rows, text_rows, "DIVERGENCE in rows written");
    assert_eq!(text_rows, 1, "a satisfied FK must not block the row");
}

#[test]
fn check_constraints_are_enforced_on_both_families() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute("CREATE TABLE chk_src (id INT, qty INT)").unwrap();
        db.execute("INSERT INTO chk_src (id, qty) VALUES (1, -5)").unwrap();
        db.execute("CREATE TABLE chk_dst (id INT PRIMARY KEY, qty INT CHECK (qty > 0))")
            .unwrap();
    }

    let db = mem_db();
    setup(&db);
    let text_rejected = db
        .execute("INSERT INTO chk_dst (id, qty) SELECT id, qty FROM chk_src")
        .is_err();
    let text_rows = rows_in(&db, "chk_dst");

    let db2 = mem_db();
    setup(&db2);
    let params_rejected = db2
        .execute_params("INSERT INTO chk_dst (id, qty) SELECT id, qty FROM chk_src", &[])
        .is_err();
    let params_rows = rows_in(&db2, "chk_dst");

    assert_eq!(
        params_rejected, text_rejected,
        "DIVERGENCE: text rejected={text_rejected}, params rejected={params_rejected}"
    );
    assert!(text_rejected, "a row violating CHECK must be rejected");
    assert_eq!(params_rows, text_rows, "DIVERGENCE in rows left behind");
    assert_eq!(text_rows, 0, "the rejected row must not be written");
}

/// A CHECK that PASSES must still be accepted — the per-statement compile of the CHECK
/// expressions must keep the same three-valued semantics as the per-row path.
#[test]
fn a_satisfied_check_is_accepted_on_both_families() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute("CREATE TABLE chkok_src (id INT, qty INT)").unwrap();
        db.execute("INSERT INTO chkok_src (id, qty) VALUES (1, 7)").unwrap();
        db.execute("CREATE TABLE chkok_dst (id INT PRIMARY KEY, qty INT CHECK (qty > 0))")
            .unwrap();
    }

    let db = mem_db();
    setup(&db);
    db.execute("INSERT INTO chkok_dst (id, qty) SELECT id, qty FROM chkok_src")
        .expect("text family: CHECK is satisfied");
    let text_qty = column(&db, "SELECT qty FROM chkok_dst");

    let db2 = mem_db();
    setup(&db2);
    db2.execute_params("INSERT INTO chkok_dst (id, qty) SELECT id, qty FROM chkok_src", &[])
        .expect("params family: CHECK is satisfied");
    let params_qty = column(&db2, "SELECT qty FROM chkok_dst");

    assert_eq!(params_qty, text_qty, "DIVERGENCE in stored values");
    assert_eq!(text_qty, vec![Value::Int4(7)], "the satisfying row must be stored");
}

#[test]
fn a_duplicate_unique_value_is_rejected_on_both_families() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute("CREATE TABLE uq_dst (id INT PRIMARY KEY, email TEXT UNIQUE)")
            .unwrap();
        db.execute("INSERT INTO uq_dst (id, email) VALUES (1, 'a@x')").unwrap();
        db.execute("CREATE TABLE uq_src (id INT, email TEXT)").unwrap();
        // Same email, different PK — only the UNIQUE column collides.
        db.execute("INSERT INTO uq_src (id, email) VALUES (2, 'a@x')").unwrap();
    }

    let db = mem_db();
    setup(&db);
    let text_rejected = db
        .execute("INSERT INTO uq_dst (id, email) SELECT id, email FROM uq_src")
        .is_err();
    let text_rows = rows_in(&db, "uq_dst");

    let db2 = mem_db();
    setup(&db2);
    let params_rejected = db2
        .execute_params("INSERT INTO uq_dst (id, email) SELECT id, email FROM uq_src", &[])
        .is_err();
    let params_rows = rows_in(&db2, "uq_dst");

    assert_eq!(
        params_rejected, text_rejected,
        "DIVERGENCE: text rejected={text_rejected}, params rejected={params_rejected}"
    );
    assert!(text_rejected, "a duplicate UNIQUE value must be rejected");
    assert_eq!(params_rows, text_rows, "DIVERGENCE in rows left behind");
    assert_eq!(text_rows, 1, "only the pre-existing row may remain");
}

#[test]
fn a_duplicate_primary_key_is_rejected_on_both_families() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute("CREATE TABLE pk_dst (id INT PRIMARY KEY, name TEXT)")
            .unwrap();
        db.execute("INSERT INTO pk_dst (id, name) VALUES (1, 'first')").unwrap();
        db.execute("CREATE TABLE pk_src (id INT, name TEXT)").unwrap();
        db.execute("INSERT INTO pk_src (id, name) VALUES (1, 'second')")
            .unwrap();
    }

    let db = mem_db();
    setup(&db);
    let text_rejected = db
        .execute("INSERT INTO pk_dst (id, name) SELECT id, name FROM pk_src")
        .is_err();
    let text_rows = rows_in(&db, "pk_dst");

    let db2 = mem_db();
    setup(&db2);
    let params_rejected = db2
        .execute_params("INSERT INTO pk_dst (id, name) SELECT id, name FROM pk_src", &[])
        .is_err();
    let params_rows = rows_in(&db2, "pk_dst");

    assert_eq!(
        params_rejected, text_rejected,
        "DIVERGENCE: text rejected={text_rejected}, params rejected={params_rejected}"
    );
    assert!(text_rejected, "a duplicate PRIMARY KEY must be rejected");
    assert_eq!(params_rows, text_rows, "DIVERGENCE in rows left behind");
    assert_eq!(text_rows, 1, "only the pre-existing row may remain");
}

// ===========================================================================
// #84 — the BEFORE-row rewrite recipe, which NEITHER arm used to apply
// ===========================================================================

const REWRITE_BODY: &str = "BEGIN NEW.tag = 'set-by-trigger'; RETURN NEW; END";
const SKIP_BODY: &str = "BEGIN RETURN NULL; END";

/// `trg_dst(id, tag)`, a two-row source, and a BEFORE INSERT … FOR EACH ROW trigger
/// whose function body is `body`. `when_clause` is inserted verbatim (may be empty).
fn setup_rewrite(db: &EmbeddedDatabase, body: &str, when_clause: &str) {
    db.execute("CREATE TABLE trg_dst (id INT, tag TEXT)").unwrap();
    db.execute("CREATE TABLE trg_src (id INT, tag TEXT)").unwrap();
    db.execute("INSERT INTO trg_src (id, tag) VALUES (1, 'original')")
        .unwrap();
    db.execute("INSERT INTO trg_src (id, tag) VALUES (99, 'original')")
        .unwrap();
    db.execute(&format!(
        "CREATE FUNCTION rewrite_fn() RETURNS TRIGGER AS $$ {body} $$ LANGUAGE plpgsql"
    ))
    .expect("CREATE FUNCTION");
    db.execute(&format!(
        "CREATE TRIGGER trg BEFORE INSERT ON trg_dst FOR EACH ROW {when_clause} EXECUTE FUNCTION rewrite_fn()"
    ))
    .expect("CREATE TRIGGER");
}

#[test]
fn the_before_row_rewrite_applies_to_insert_select_on_both_families() {
    let db = mem_db();
    setup_rewrite(&db, REWRITE_BODY, "");
    db.execute("INSERT INTO trg_dst (id, tag) SELECT id, tag FROM trg_src WHERE id = 1")
        .expect("text family");
    let text_tag = column(&db, "SELECT tag FROM trg_dst WHERE id = 1");

    let db2 = mem_db();
    setup_rewrite(&db2, REWRITE_BODY, "");
    db2.execute_params(
        "INSERT INTO trg_dst (id, tag) SELECT id, tag FROM trg_src WHERE id = 1",
        &[],
    )
    .expect("params family");
    let params_tag = column(&db2, "SELECT tag FROM trg_dst WHERE id = 1");

    assert_eq!(
        params_tag, text_tag,
        "DIVERGENCE: text stored {text_tag:?}, params stored {params_tag:?}"
    );
    assert_eq!(
        text_tag,
        vec![text("set-by-trigger")],
        "the BEFORE-row rewrite must apply to INSERT … SELECT (#84)"
    );
}

#[test]
fn a_return_null_rewrite_skips_the_row_on_both_families() {
    let db = mem_db();
    setup_rewrite(&db, SKIP_BODY, "");
    let text_count = db
        .execute("INSERT INTO trg_dst (id, tag) SELECT id, tag FROM trg_src")
        .expect("text family");
    let text_rows = rows_in(&db, "trg_dst");

    let db2 = mem_db();
    setup_rewrite(&db2, SKIP_BODY, "");
    let params_count = db2
        .execute_params("INSERT INTO trg_dst (id, tag) SELECT id, tag FROM trg_src", &[])
        .expect("params family");
    let params_rows = rows_in(&db2, "trg_dst");

    assert_eq!(params_count, text_count, "DIVERGENCE in the reported row count");
    assert_eq!(params_rows, text_rows, "DIVERGENCE in rows written");
    assert_eq!(text_rows, 0, "`RETURN NULL` must suppress every row");
    assert_eq!(text_count, 0, "a suppressed row must not be counted");
}

#[test]
fn the_rewrites_when_clause_gates_insert_select_on_both_families() {
    let db = mem_db();
    setup_rewrite(&db, REWRITE_BODY, "WHEN (NEW.id > 10)");
    db.execute("INSERT INTO trg_dst (id, tag) SELECT id, tag FROM trg_src")
        .expect("text family");
    let text_gated = column(&db, "SELECT tag FROM trg_dst WHERE id = 1");
    let text_fired = column(&db, "SELECT tag FROM trg_dst WHERE id = 99");

    let db2 = mem_db();
    setup_rewrite(&db2, REWRITE_BODY, "WHEN (NEW.id > 10)");
    db2.execute_params("INSERT INTO trg_dst (id, tag) SELECT id, tag FROM trg_src", &[])
        .expect("params family");
    let params_gated = column(&db2, "SELECT tag FROM trg_dst WHERE id = 1");
    let params_fired = column(&db2, "SELECT tag FROM trg_dst WHERE id = 99");

    assert_eq!(params_gated, text_gated, "DIVERGENCE on the WHEN-false row");
    assert_eq!(params_fired, text_fired, "DIVERGENCE on the WHEN-true row");
    assert_eq!(
        text_gated,
        vec![text("original")],
        "a row failing WHEN must NOT be rewritten"
    );
    assert_eq!(
        text_fired,
        vec![text("set-by-trigger")],
        "a row satisfying WHEN must be rewritten"
    );
}

/// The rewrite runs BEFORE the constraint gates (PostgreSQL's order), so a rewrite that
/// REPAIRS a CHECK violation is accepted, on both families.
#[test]
fn the_rewrite_runs_before_the_check_gate_on_both_families() {
    fn setup(db: &EmbeddedDatabase) {
        // Only the REWRITTEN value satisfies this CHECK, so the row can only be
        // stored if the rewrite ran first.
        db.execute("CREATE TABLE ord_dst (id INT, tag TEXT CHECK (tag = 'rewritten'))")
            .unwrap();
        db.execute("CREATE TABLE ord_src (id INT, tag TEXT)").unwrap();
        db.execute("INSERT INTO ord_src (id, tag) VALUES (1, 'original')")
            .unwrap();
        db.execute("CREATE FUNCTION ord_fn() RETURNS TRIGGER AS $$ BEGIN NEW.tag = 'rewritten'; RETURN NEW; END $$ LANGUAGE plpgsql")
            .expect("CREATE FUNCTION");
        db.execute("CREATE TRIGGER ord_trg BEFORE INSERT ON ord_dst FOR EACH ROW EXECUTE FUNCTION ord_fn()")
            .expect("CREATE TRIGGER");
    }

    let db = mem_db();
    setup(&db);
    db.execute("INSERT INTO ord_dst (id, tag) SELECT id, tag FROM ord_src")
        .expect("text family: the rewrite repairs the CHECK violation");
    let text_tag = column(&db, "SELECT tag FROM ord_dst");

    let db2 = mem_db();
    setup(&db2);
    db2.execute_params("INSERT INTO ord_dst (id, tag) SELECT id, tag FROM ord_src", &[])
        .expect("params family: the rewrite repairs the CHECK violation");
    let params_tag = column(&db2, "SELECT tag FROM ord_dst");

    assert_eq!(params_tag, text_tag, "DIVERGENCE in stored values");
    assert_eq!(
        text_tag,
        vec![text("rewritten")],
        "the rewrite must run BEFORE the CHECK gate"
    );
}

// ===========================================================================
// CTAS — it populates by re-entering the params arm, so it inherits the gate
// ===========================================================================

#[test]
fn ctas_still_copies_rows_correctly_on_both_families() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute("CREATE TABLE ctas_c_src (id INT, name TEXT)").unwrap();
        db.execute("INSERT INTO ctas_c_src (id, name) VALUES (1, 'Alice')")
            .unwrap();
        db.execute("INSERT INTO ctas_c_src (id, name) VALUES (2, 'Bob')")
            .unwrap();
    }

    let db = mem_db();
    setup(&db);
    let text_count = db
        .execute("CREATE TABLE ctas_c_dst AS SELECT id, name FROM ctas_c_src")
        .expect("text family CTAS");
    let text_names = column(&db, "SELECT name FROM ctas_c_dst ORDER BY id");

    let db2 = mem_db();
    setup(&db2);
    let params_count = db2
        .execute_params("CREATE TABLE ctas_c_dst AS SELECT id, name FROM ctas_c_src", &[])
        .expect("params family CTAS");
    let params_names = column(&db2, "SELECT name FROM ctas_c_dst ORDER BY id");

    assert_eq!(params_count, text_count, "DIVERGENCE in the reported row count");
    assert_eq!(params_names, text_names, "DIVERGENCE in copied values");
    assert_eq!(text_count, 2, "CTAS must report the rows it populated");
    assert_eq!(text_names, vec![text("Alice"), text("Bob")], "CTAS must copy the rows");
}

/// The compensating drop still runs when population fails: the half-built table must be
/// gone and its name free again.
#[test]
fn ctas_failed_population_still_leaves_no_table() {
    let db = mem_db();
    db.execute("CREATE TABLE ctas_f_src (n INT)").unwrap();
    db.execute("INSERT INTO ctas_f_src (n) VALUES (5)").unwrap();
    db.execute("INSERT INTO ctas_f_src (n) VALUES (0)").unwrap();

    let err = db
        .execute("CREATE TABLE ctas_f_dst AS SELECT 10 / n AS q FROM ctas_f_src")
        .expect_err("dividing by the n=0 row must fail the statement")
        .to_string();
    assert!(
        err.contains("Division by zero"),
        "the ORIGINAL population error must surface, got: {err}"
    );
    assert!(
        db.query("SELECT * FROM ctas_f_dst", &[]).is_err(),
        "the compensating drop must remove the half-built table"
    );
    db.execute("CREATE TABLE ctas_f_dst (q INT)")
        .expect("the name must be free again");
}

// ===========================================================================
// KNOWN GAPS — pinned, not fixed. Read before "fixing" a failure here.
// ===========================================================================

/// PINS TASK #100: `INSERT … SELECT` writes rows STRAIGHT to storage
/// (`insert_tuple_branch_aware_with_schema`, which takes no transaction), so they survive
/// the enclosing `ROLLBACK`. That is WRONG and it is filed as #100; it is deliberately NOT
/// fixed by the #101/#102/#84 change, which alters row assembly and validation only and
/// leaves transaction participation exactly as it was.
///
/// WHEN #100 LANDS THIS TEST MUST FAIL. Do not relax it — REPLACE it with the inverse
/// assertion (`rows_in(&db, "pin_dst") == 0` after ROLLBACK, and the rows visible before
/// COMMIT only to the writing session), on BOTH executor families.
#[test]
fn pinned_gap_100_insert_select_rows_survive_rollback() {
    let db = mem_db();
    db.execute("CREATE TABLE pin_src (id INT, name TEXT)").unwrap();
    db.execute("INSERT INTO pin_src (id, name) VALUES (1, 'Alice')")
        .unwrap();
    db.execute("CREATE TABLE pin_dst (id INT, name TEXT)").unwrap();

    db.execute("BEGIN").expect("BEGIN");
    db.execute("INSERT INTO pin_dst (id, name) SELECT id, name FROM pin_src")
        .expect("INSERT … SELECT inside the transaction");
    db.execute("ROLLBACK").expect("ROLLBACK");

    assert_eq!(
        rows_in(&db, "pin_dst"),
        1,
        "KNOWN GAP #100: INSERT … SELECT writes around the transaction, so the row survives \
         ROLLBACK. If this now reports 0, #100 has been fixed — replace this test with the \
         inverse assertion rather than deleting it."
    );
}

/// PINS the composite-UNIQUE hole: `Catalog::create_table` only builds ART indexes for
/// PRIMARY KEY and COLUMN-level UNIQUE (`src/storage/catalog.rs`), so a TABLE-level
/// `UNIQUE (a, b)` has no index to probe and nothing enforces it — on any write path, on
/// either family. The shared INSERT … SELECT gate carries the pre-existing probe forward
/// unchanged rather than inventing a second enforcement mechanism.
///
/// WHEN composite UNIQUE indexes land THIS TEST MUST FAIL. Replace it with the rejection
/// assertion (both families, table unchanged) rather than relaxing it.
#[test]
fn pinned_gap_composite_unique_is_enforced_on_neither_family() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute("CREATE TABLE cu_dst (id INT PRIMARY KEY, a INT, b INT, UNIQUE (a, b))")
            .unwrap();
        db.execute("INSERT INTO cu_dst (id, a, b) VALUES (1, 7, 8)").unwrap();
        db.execute("CREATE TABLE cu_src (id INT, a INT, b INT)").unwrap();
        db.execute("INSERT INTO cu_src (id, a, b) VALUES (2, 7, 8)").unwrap();
    }

    let db = mem_db();
    setup(&db);
    db.execute("INSERT INTO cu_dst (id, a, b) SELECT id, a, b FROM cu_src")
        .expect("text family: composite UNIQUE is not enforced today");
    let text_rows = rows_in(&db, "cu_dst");

    let db2 = mem_db();
    setup(&db2);
    db2.execute_params("INSERT INTO cu_dst (id, a, b) SELECT id, a, b FROM cu_src", &[])
        .expect("params family: composite UNIQUE is not enforced today");
    let params_rows = rows_in(&db2, "cu_dst");

    assert_eq!(
        params_rows, text_rows,
        "the two families must agree even about a gap: text left {text_rows} row(s), params {params_rows}"
    );
    assert_eq!(
        text_rows, 2,
        "KNOWN GAP: a table-level UNIQUE (a, b) has no ART index, so the duplicate is \
         accepted. If this now reports 1, composite UNIQUE is enforced — replace this test \
         with the rejection assertion rather than deleting it."
    );
}

// ===========================================================================
// Regressions the shared gate itself introduced, caught in adversarial review
// ===========================================================================

/// The UNIQUE probe the gate inherited resolves the table's PRIMARY KEY index and answers
/// about THAT INDEX ALONE (`pk_index_contains`, `src/storage/art_manager.rs`). Feeding it a
/// NON-PK unique constraint's values therefore asks the wrong index: on
/// `(name TEXT PRIMARY KEY, alias TEXT UNIQUE)`, inserting `alias = 'x'` while a row NAMED
/// 'x' exists reported a phantom duplicate.
///
/// That false positive is pre-existing on the text family. The shared gate would have newly
/// exposed the extended protocol and REST to it, so the gate now probes for the PRIMARY KEY
/// constraint only. Non-PK single-column UNIQUE is still enforced by the ART unique index at
/// the storage layer — asserted below by the second half of this test.
#[test]
fn a_non_pk_unique_value_colliding_with_a_primary_key_is_accepted_on_both_families() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute("CREATE TABLE ph_dst (name TEXT PRIMARY KEY, alias TEXT UNIQUE)")
            .unwrap();
        db.execute("INSERT INTO ph_dst (name, alias) VALUES ('x', 'other')")
            .unwrap();
        db.execute("CREATE TABLE ph_src (name TEXT, alias TEXT)").unwrap();
        // alias 'x' collides with an existing NAME, not with an existing alias.
        db.execute("INSERT INTO ph_src (name, alias) VALUES ('y', 'x')")
            .unwrap();
    }

    let db = mem_db();
    setup(&db);
    db.execute("INSERT INTO ph_dst (name, alias) SELECT name, alias FROM ph_src")
        .expect("text family: an alias colliding only with a PRIMARY KEY must be accepted");
    let text_rows = rows_in(&db, "ph_dst");

    let db2 = mem_db();
    setup(&db2);
    db2.execute_params("INSERT INTO ph_dst (name, alias) SELECT name, alias FROM ph_src", &[])
        .expect("params family: an alias colliding only with a PRIMARY KEY must be accepted");
    let params_rows = rows_in(&db2, "ph_dst");

    assert_eq!(
        params_rows, text_rows,
        "DIVERGENCE: text left {text_rows} row(s), params {params_rows}"
    );
    assert_eq!(text_rows, 2, "both rows must be present — no phantom 23505");

    // …and a GENUINE duplicate alias is still rejected, by the storage-layer ART unique
    // index. Without this half, the fix above could not be told apart from deleting the
    // non-PK UNIQUE check entirely.
    let db3 = mem_db();
    setup(&db3);
    db3.execute("INSERT INTO ph_src (name, alias) VALUES ('z', 'other')")
        .unwrap();
    let rejected = db3
        .execute("INSERT INTO ph_dst (name, alias) SELECT name, alias FROM ph_src WHERE name = 'z'")
        .is_err();
    assert!(
        rejected,
        "a genuine duplicate in a non-PK UNIQUE column must still be rejected"
    );
    assert_eq!(rows_in(&db3, "ph_dst"), 1, "the rejected row must not be stored");
}

/// A source-supplied NULL primary key must behave the same in `INSERT … SELECT` as in a
/// single-row `INSERT`. This codebase auto-fills it from the row id — verified on the
/// single-row arm of BOTH families before this test was written — and the gate's
/// post-rewrite NOT NULL pass already exempted primary keys for the OMITTED form. Its
/// per-value pass did not, so `INSERT … SELECT` alone rejected what every other insert
/// shape accepted. The two passes inside one function contradicted each other.
///
/// `INT PRIMARY KEY` is deliberate, not `SERIAL`: the planner sets `not_null = false` for
/// SERIAL/IDENTITY columns (`sql_column_def_to_column_def`, `src/sql/planner.rs`), so a
/// SERIAL PK never reaches the NOT NULL pass at all and would make this test vacuous.
///
/// NOTE this auto-fill diverges from PostgreSQL, which rejects an explicit NULL into a
/// plain `INT PRIMARY KEY`. That divergence is pre-existing and codebase-wide — the
/// single-row arms do it too — so it is filed separately rather than changed here, where
/// the goal is that the statement shapes AGREE.
#[test]
fn a_source_supplied_null_primary_key_matches_the_single_row_arm_on_both_families() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute("CREATE TABLE spk_dst (id INT PRIMARY KEY, label TEXT)")
            .unwrap();
        db.execute("CREATE TABLE spk_src (label TEXT)").unwrap();
        db.execute("INSERT INTO spk_src (label) VALUES ('alpha')").unwrap();
    }

    // The reference behaviour: a single-row INSERT of an explicit NULL primary key.
    let reference = mem_db();
    setup(&reference);
    let single_row_accepts = reference
        .execute("INSERT INTO spk_dst (id, label) VALUES (NULL, 'alpha')")
        .is_ok();

    let db = mem_db();
    setup(&db);
    let text_accepts = db
        .execute("INSERT INTO spk_dst (id, label) SELECT NULL, label FROM spk_src")
        .is_ok();
    let text_rows = rows_in(&db, "spk_dst");

    let db2 = mem_db();
    setup(&db2);
    let params_accepts = db2
        .execute_params("INSERT INTO spk_dst (id, label) SELECT NULL, label FROM spk_src", &[])
        .is_ok();
    let params_rows = rows_in(&db2, "spk_dst");

    assert_eq!(
        text_accepts, single_row_accepts,
        "INSERT … SELECT (text) must agree with single-row INSERT about a NULL primary key: \
         single-row accepted={single_row_accepts}, INSERT … SELECT accepted={text_accepts}"
    );
    assert_eq!(
        params_accepts, single_row_accepts,
        "INSERT … SELECT (params) must agree with single-row INSERT: \
         single-row accepted={single_row_accepts}, INSERT … SELECT accepted={params_accepts}"
    );
    assert_eq!(params_rows, text_rows, "DIVERGENCE in rows left behind");

    // Whatever the shared convention is, assert what it currently IS so a change to it
    // surfaces here rather than silently.
    assert!(
        single_row_accepts,
        "reference behaviour changed: single-row INSERT now rejects a NULL primary key. \
         Update this test AND the gate together — they must stay in agreement."
    );
    assert_eq!(text_rows, 1, "the row must be stored");
    assert_eq!(
        column(&db, "SELECT label FROM spk_dst"),
        vec![text("alpha")],
        "the non-PK column must survive intact"
    );
    assert!(
        !matches!(column(&db, "SELECT id FROM spk_dst").first(), Some(Value::Null) | None),
        "the primary key must have been filled, not left NULL"
    );
}

/// A NULL into a NOT NULL column that is NOT the primary key is still rejected. Pins the
/// PK exemption above as an exemption rather than a hole.
#[test]
fn the_primary_key_null_exemption_does_not_leak_to_other_columns() {
    fn setup(db: &EmbeddedDatabase) {
        db.execute("CREATE TABLE nnx_dst (id INT PRIMARY KEY, label TEXT NOT NULL)")
            .unwrap();
        db.execute("CREATE TABLE nnx_src (label TEXT)").unwrap();
        db.execute("INSERT INTO nnx_src (label) VALUES (NULL)").unwrap();
    }

    let db = mem_db();
    setup(&db);
    let text_rejected = db
        .execute("INSERT INTO nnx_dst (id, label) SELECT NULL, label FROM nnx_src")
        .is_err();
    let text_rows = rows_in(&db, "nnx_dst");

    let db2 = mem_db();
    setup(&db2);
    let params_rejected = db2
        .execute_params("INSERT INTO nnx_dst (id, label) SELECT NULL, label FROM nnx_src", &[])
        .is_err();
    let params_rows = rows_in(&db2, "nnx_dst");

    assert_eq!(
        params_rejected, text_rejected,
        "DIVERGENCE: text rejected={text_rejected}, params rejected={params_rejected}"
    );
    assert!(text_rejected, "a NULL into a non-PK NOT NULL column must be rejected");
    assert_eq!(params_rows, text_rows, "DIVERGENCE in rows left behind");
    assert_eq!(text_rows, 0, "the rejected row must not be stored");
}

/// CTAS re-enters the params arm to populate, so the gate's NOT NULL pass now runs over a
/// CTAS target. A CTAS target has no NOT NULL constraints in PostgreSQL — it inherits types
/// only — so an outer-join NULL must land, not be rejected. Guards the `nullable: true` fix
/// in `ctas_target_columns` (`src/sql/planner.rs`).
#[test]
fn ctas_accepts_outer_join_nulls_from_a_not_null_source_column() {
    let db = mem_db();
    db.execute("CREATE TABLE oj_left (id INT PRIMARY KEY, k INT)").unwrap();
    db.execute("CREATE TABLE oj_right (k INT PRIMARY KEY, val TEXT NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO oj_left (id, k) VALUES (1, 10)").unwrap();
    db.execute("INSERT INTO oj_left (id, k) VALUES (2, 99)").unwrap();
    db.execute("INSERT INTO oj_right (k, val) VALUES (10, 'matched')")
        .unwrap();

    db.execute(
        "CREATE TABLE oj_out AS SELECT oj_left.id, oj_right.val \
         FROM oj_left LEFT JOIN oj_right ON oj_left.k = oj_right.k",
    )
    .expect("CTAS must accept the NULL an outer join produces for a NOT NULL source column");

    assert_eq!(
        rows_in(&db, "oj_out"),
        2,
        "both left rows must land, including the unmatched one"
    );
    let vals = column(&db, "SELECT val FROM oj_out");
    assert!(
        vals.contains(&Value::Null),
        "the unmatched row's val must be NULL, got {vals:?}"
    );
    assert!(
        vals.contains(&text("matched")),
        "the matched row's val must survive, got {vals:?}"
    );
}
