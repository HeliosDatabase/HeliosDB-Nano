//! Placeholder interpolation in stored-routine bodies.
//!
//! `CALL p(args)` substitutes the call's arguments into the procedure body before the
//! body is executed. Until this suite existed, that substitution was four sequential
//! `String::replace` passes, and it corrupted bodies four ways — three of them SILENTLY,
//! which is why they survived a shipped test suite:
//!
//!   1. **Literal blindness.** `INSERT INTO t VALUES ('price is $1 dollars')` with
//!      `CALL p(5)` stored `price is 5 dollars`. No error; the wrong string just landed
//!      in the table.
//!   2. **Positional prefix capture.** With ten parameters, `$1` matched the prefix of
//!      `$10`: `VALUES ($10)` with `CALL p(1,…,9,99)` left the text `10` behind (the
//!      substituted `1` plus the orphaned `0`), which is a perfectly valid integer, so
//!      the row stored `10` instead of `99`.
//!   3. **Name prefix capture.** Parameters `p` and `p_id` with a body naming both:
//!      `$p` ate the prefix of `$p_id`, leaving `7_id` — a loud parse error outside a
//!      literal, a silently wrong string inside one. It was also declaration-order
//!      dependent: declaring the longer name first happened to work.
//!   4. **Substituted values were re-scanned.** The positional pass ran before the named
//!      pass, so a VALUE inserted by the first pass was interpolated by the second:
//!      `CALL p('$name', 'INJECTED')` blew up with `Expected: ), found: INJECTED`.
//!      Argument DATA could change the interpolation of other placeholders.
//!
//! All four are now structurally impossible: one left-to-right scanner
//! (`src/sql/interpolate.rs`) that knows every region in which a `$` is not a
//! placeholder, uses maximal munch for names, and never re-scans the text it emits.
//!
//! The same scanner now also runs for `LANGUAGE plpgsql` bodies, which previously
//! substituted nothing at all (`$p_id` → `Invalid parameter placeholder`, `$1` →
//! `Parameter $1 not provided`).
//!
//! **The `$` sigil is still mandatory, in both languages** — a bare parameter name is a
//! column reference and fails with `Column 'n' not found in schema`. That is deliberate:
//! PostgreSQL resolves bare PL/pgSQL variable names, and a variable that can shadow a
//! column is exactly the silent-wrong-data class this change exists to remove.
//! `tests/function_unimplemented_tests.rs` pins the same rule from the other side.
//!
//! HOW TO MAINTAIN THIS FILE. Every test asserts unconditionally — never introduce an
//! `is_ok()` guard, and never assert `rows.len() > 0` against `SELECT COUNT(*)` (a count
//! query returns one row whether the count is 0 or 10,000). Use `rows_in()`.

use heliosdb_nano::{EmbeddedDatabase, Value};

fn db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory db")
}

/// Rows physically present in `table`. Deliberately NOT `SELECT COUNT(*)`.
fn rows_in(db: &EmbeddedDatabase, table: &str) -> usize {
    let sql = format!("SELECT * FROM {table}");
    db.query(&sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
        .len()
}

/// The values of the single row in `table`, asserting there is exactly one.
fn only_row(db: &EmbeddedDatabase, table: &str) -> Vec<Value> {
    let sql = format!("SELECT * FROM {table}");
    let rows = db.query(&sql, &[]).unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
    assert_eq!(rows.len(), 1, "expected exactly one row in {table}, got {}", rows.len());
    rows.first().expect("one row").values.clone()
}

fn ints(db: &EmbeddedDatabase, table: &str) -> (i32, i32) {
    match only_row(db, table).as_slice() {
        [Value::Int4(a), Value::Int4(b)] => (*a, *b),
        other => panic!("expected two INTEGER columns in {table}, got {other:?}"),
    }
}

fn int_text(db: &EmbeddedDatabase, table: &str) -> (i32, String) {
    match only_row(db, table).as_slice() {
        [Value::Int4(a), Value::String(b)] => (*a, b.clone()),
        other => panic!("expected (INTEGER, TEXT) in {table}, got {other:?}"),
    }
}

fn text(db: &EmbeddedDatabase, table: &str) -> String {
    match only_row(db, table).as_slice() {
        [Value::String(s)] => s.clone(),
        other => panic!("expected a single TEXT column in {table}, got {other:?}"),
    }
}

fn two_texts(db: &EmbeddedDatabase, table: &str) -> (String, String) {
    match only_row(db, table).as_slice() {
        [Value::String(a), Value::String(b)] => (a.clone(), b.clone()),
        other => panic!("expected two TEXT columns in {table}, got {other:?}"),
    }
}

// ===========================================================================
// A. `LANGUAGE sql` — the four corruption modes, as regressions.
// ===========================================================================

#[test]
fn positional_placeholders_do_not_prefix_capture() {
    // Defect 2. Before: stored Int4(10) — `$1` matched inside `$10`, the leftover `0`
    // made `10`, and `10` parses, so this was silent data corruption.
    let db = db();
    db.execute("CREATE TABLE pi_ten (hi INTEGER, lo INTEGER)").unwrap();

    db.execute(
        "CREATE PROCEDURE pi_ten_proc(p1 INTEGER, p2 INTEGER, p3 INTEGER, p4 INTEGER, p5 INTEGER, \
         p6 INTEGER, p7 INTEGER, p8 INTEGER, p9 INTEGER, p10 INTEGER) LANGUAGE sql \
         AS $$INSERT INTO pi_ten VALUES ($10, $1)$$",
    )
    .expect("CREATE PROCEDURE must be accepted");

    db.execute("CALL pi_ten_proc(1, 2, 3, 4, 5, 6, 7, 8, 9, 99)")
        .expect("CALL must execute the body");

    assert_eq!(
        ints(&db, "pi_ten"),
        (99, 1),
        "$10 must resolve to the tenth argument, not to $1 followed by a stray 0"
    );
}

#[test]
fn named_placeholders_do_not_prefix_capture_in_either_declaration_order() {
    // Defect 3, plus its declaration-order dependence: with the old sequential
    // `replace`, `$p` ate the prefix of `$p_id` only when `p` was declared first.
    let db = db();
    db.execute("CREATE TABLE pi_pref (a INTEGER, b INTEGER)").unwrap();

    // Short name declared FIRST — the order that used to corrupt.
    db.execute(
        "CREATE PROCEDURE pi_pref_short_first(p INTEGER, p_id INTEGER) LANGUAGE sql \
         AS $$INSERT INTO pi_pref VALUES ($p, $p_id)$$",
    )
    .expect("CREATE PROCEDURE must be accepted");
    db.execute("CALL pi_pref_short_first(7, 42)")
        .expect("CALL must execute the body");
    assert_eq!(ints(&db, "pi_pref"), (7, 42), "$p=7, $p_id=42");

    db.execute("DELETE FROM pi_pref").unwrap();

    // Long name declared FIRST — same body, so the columns mirror the swap. What matters
    // is that each name resolves to ITS OWN argument, whatever the declaration order.
    db.execute(
        "CREATE PROCEDURE pi_pref_long_first(p_id INTEGER, p INTEGER) LANGUAGE sql \
         AS $$INSERT INTO pi_pref VALUES ($p, $p_id)$$",
    )
    .expect("CREATE PROCEDURE must be accepted");
    db.execute("CALL pi_pref_long_first(7, 42)")
        .expect("CALL must execute the body");
    assert_eq!(ints(&db, "pi_pref"), (42, 7), "$p=42, $p_id=7");
}

#[test]
fn no_substitution_inside_string_literals() {
    // Defect 1. Before: stored "price is 5 dollars".
    let db = db();
    db.execute("CREATE TABLE pi_lit (n INTEGER, note TEXT)").unwrap();

    db.execute(
        "CREATE PROCEDURE pi_lit_proc(p_n INTEGER) LANGUAGE sql \
         AS $$INSERT INTO pi_lit VALUES ($1, 'price is $1 dollars')$$",
    )
    .expect("CREATE PROCEDURE must be accepted");

    db.execute("CALL pi_lit_proc(5)").expect("CALL must execute the body");

    assert_eq!(
        int_text(&db, "pi_lit"),
        (5, "price is $1 dollars".to_string()),
        "a `$1` inside a string literal is data, not a placeholder"
    );
}

#[test]
fn argument_data_cannot_influence_other_placeholders() {
    // Defect 4. Before: the positional pass inserted the text `$name`, then the NAMED
    // pass substituted inside that inserted value — `Expected: ), found: INJECTED`.
    let db = db();
    db.execute("CREATE TABLE pi_second (a TEXT, b TEXT)").unwrap();

    db.execute(
        "CREATE PROCEDURE pi_second_proc(s TEXT, name TEXT) LANGUAGE sql \
         AS $$INSERT INTO pi_second VALUES ($1, $2)$$",
    )
    .expect("CREATE PROCEDURE must be accepted");

    db.execute("CALL pi_second_proc('$name', 'INJECTED')")
        .expect("an argument that looks like a placeholder must not break the call");

    assert_eq!(
        two_texts(&db, "pi_second"),
        ("$name".to_string(), "INJECTED".to_string()),
        "a substituted value is never re-scanned"
    );
}

#[test]
fn argument_text_that_looks_like_a_placeholder_is_stored_verbatim() {
    let db = db();
    db.execute("CREATE TABLE pi_verbatim (a TEXT, b TEXT)").unwrap();

    db.execute(
        "CREATE PROCEDURE pi_verbatim_proc(p_a TEXT, p_b TEXT) LANGUAGE sql \
         AS $$INSERT INTO pi_verbatim VALUES ($p_a, $p_b)$$",
    )
    .expect("CREATE PROCEDURE must be accepted");

    db.execute("CALL pi_verbatim_proc('costs $1', 'and $p_b')")
        .expect("CALL must execute the body");

    assert_eq!(
        two_texts(&db, "pi_verbatim"),
        ("costs $1".to_string(), "and $p_b".to_string()),
        "placeholder-shaped argument text must survive intact"
    );
}

#[test]
fn text_argument_containing_a_quote_round_trips() {
    // Not a regression — `value_to_sql_literal` has always doubled `'`. Pinned because
    // the whole design now rests on it: it is what keeps interpolation safe.
    let db = db();
    db.execute("CREATE TABLE pi_quote (note TEXT)").unwrap();

    db.execute(
        "CREATE PROCEDURE pi_quote_proc(p_note TEXT) LANGUAGE sql \
         AS $$INSERT INTO pi_quote VALUES ($p_note)$$",
    )
    .expect("CREATE PROCEDURE must be accepted");

    db.execute("CALL pi_quote_proc('O''Brien')")
        .expect("CALL must execute the body");

    assert_eq!(text(&db, "pi_quote"), "O'Brien");
}

#[test]
fn null_arguments_interpolate_as_sql_null() {
    let db = db();
    db.execute("CREATE TABLE pi_null (n INTEGER, note TEXT)").unwrap();

    db.execute(
        "CREATE PROCEDURE pi_null_proc(p_n INTEGER, p_note TEXT) LANGUAGE sql \
         AS $$INSERT INTO pi_null VALUES ($p_n, $p_note)$$",
    )
    .expect("CREATE PROCEDURE must be accepted");

    db.execute("CALL pi_null_proc(NULL, NULL)")
        .expect("CALL must execute the body");

    assert_eq!(
        only_row(&db, "pi_null"),
        vec![Value::Null, Value::Null],
        "a NULL argument renders as the SQL keyword NULL, in both an INT and a TEXT column"
    );
}

#[test]
fn unknown_placeholder_still_fails_loudly() {
    // Unresolvable placeholders are left VERBATIM by design, so the ordinary planner
    // raises its existing error. Typos must not be silently swallowed.
    let db = db();
    db.execute("CREATE TABLE pi_oops (n INTEGER)").unwrap();

    db.execute(
        "CREATE PROCEDURE pi_oops_proc(p_n INTEGER) LANGUAGE sql \
         AS $$INSERT INTO pi_oops VALUES ($oops)$$",
    )
    .expect("CREATE PROCEDURE must be accepted");

    let err = db
        .execute("CALL pi_oops_proc(5)")
        .expect_err("an undeclared placeholder must not be swallowed")
        .to_string();
    assert!(
        err.contains("Invalid parameter placeholder"),
        "expected 'Invalid parameter placeholder', got: {err}"
    );
    assert_eq!(rows_in(&db, "pi_oops"), 0, "nothing may have been written");
}

#[test]
fn dollar_quoted_literal_in_body_is_not_a_placeholder() {
    // A `$body$`-delimited body may legally contain `$q$ … $q$`. A scanner unaware of
    // dollar quoting would read `$q` as a named placeholder and corrupt the delimiter.
    let db = db();
    db.execute("CREATE TABLE pi_dq (note TEXT, n INTEGER)").unwrap();

    db.execute(
        "CREATE PROCEDURE pi_dq_proc(p_n INTEGER) LANGUAGE sql \
         AS $body$INSERT INTO pi_dq VALUES ($q$has $1 and $p_n$q$, $1)$body$",
    )
    .expect("CREATE PROCEDURE must be accepted");

    db.execute("CALL pi_dq_proc(5)").expect("CALL must execute the body");

    match only_row(&db, "pi_dq").as_slice() {
        [Value::String(note), Value::Int4(n)] => {
            assert_eq!(note, "has $1 and $p_n", "the dollar-quoted region is data");
            assert_eq!(*n, 5, "the placeholder outside it still binds");
        }
        other => panic!("unexpected row shape: {other:?}"),
    }
}

#[test]
fn placeholder_inside_a_comment_is_not_substituted() {
    let db = db();
    db.execute("CREATE TABLE pi_cmt (n INTEGER)").unwrap();

    db.execute(
        "CREATE PROCEDURE pi_cmt_proc(p_n INTEGER) LANGUAGE sql \
         AS $$INSERT INTO pi_cmt VALUES ($1) -- see $2$$",
    )
    .expect("CREATE PROCEDURE must be accepted");

    // `$2` has no argument. If the scanner looked inside the comment it would be left
    // verbatim there and reach the planner as an unbound placeholder; it does not, and
    // the lexer drops the comment.
    db.execute("CALL pi_cmt_proc(5)").expect("CALL must execute the body");

    assert_eq!(only_row(&db, "pi_cmt"), vec![Value::Int4(5)]);
}

// ===========================================================================
// B. `LANGUAGE plpgsql` — parameters now bind. Before this change the plpgsql path
//    declared parameters into the procedural scope and then passed every body
//    statement to the executor verbatim, so NOTHING was substituted, by any spelling.
//    The two pins in tests/function_unimplemented_tests.rs assert the new behaviour.
// ===========================================================================

#[test]
fn plpgsql_body_binds_named_parameters() {
    let db = db();
    db.execute("CREATE TABLE pi_pg (id INTEGER, note TEXT)").unwrap();

    db.execute(
        "CREATE PROCEDURE pi_pg_named(p_id INTEGER) LANGUAGE plpgsql \
         AS $$BEGIN INSERT INTO pi_pg VALUES ($p_id, 'x'); END$$",
    )
    .expect("CREATE PROCEDURE must be accepted");

    db.execute("CALL pi_pg_named(7)")
        .expect("a plpgsql body must substitute $<paramname>");

    assert_eq!(int_text(&db, "pi_pg"), (7, "x".to_string()));
}

#[test]
fn plpgsql_body_binds_positional_parameters() {
    let db = db();
    db.execute("CREATE TABLE pi_pg_pos (id INTEGER, note TEXT)").unwrap();

    db.execute(
        "CREATE PROCEDURE pi_pg_pos_proc(p_id INTEGER) LANGUAGE plpgsql \
         AS $$BEGIN INSERT INTO pi_pg_pos VALUES ($1, 'x'); END$$",
    )
    .expect("CREATE PROCEDURE must be accepted");

    db.execute("CALL pi_pg_pos_proc(7)")
        .expect("a plpgsql body must substitute $1 from the call's arguments");

    assert_eq!(int_text(&db, "pi_pg_pos"), (7, "x".to_string()));
}

#[test]
fn plpgsql_parameter_is_reusable_across_statements() {
    let db = db();
    db.execute("CREATE TABLE pi_pg_multi (id INTEGER, note TEXT)").unwrap();

    db.execute(
        "CREATE PROCEDURE pi_pg_multi_proc(p_id INTEGER) LANGUAGE plpgsql \
         AS $$BEGIN INSERT INTO pi_pg_multi VALUES ($p_id, 'first'); \
         INSERT INTO pi_pg_multi VALUES ($p_id, 'second'); END$$",
    )
    .expect("CREATE PROCEDURE must be accepted");

    db.execute("CALL pi_pg_multi_proc(7)")
        .expect("CALL must execute the body");

    let rows = db
        .query("SELECT id, note FROM pi_pg_multi ORDER BY note", &[])
        .expect("read");
    assert_eq!(rows.len(), 2, "both statements must have run");
    let notes: Vec<String> = rows
        .iter()
        .map(|r| match r.values.as_slice() {
            [Value::Int4(id), Value::String(note)] => {
                assert_eq!(*id, 7, "every statement sees the same bound parameter");
                note.clone()
            }
            other => panic!("unexpected row shape: {other:?}"),
        })
        .collect();
    assert_eq!(notes, vec!["first".to_string(), "second".to_string()]);
}

#[test]
fn plpgsql_quote_and_null_arguments() {
    let db = db();
    db.execute("CREATE TABLE pi_pg_quote (note TEXT)").unwrap();
    db.execute("CREATE TABLE pi_pg_null (note TEXT)").unwrap();

    db.execute(
        "CREATE PROCEDURE pi_pg_quote_proc(p_note TEXT) LANGUAGE plpgsql \
         AS $$BEGIN INSERT INTO pi_pg_quote VALUES ($p_note); END$$",
    )
    .expect("CREATE PROCEDURE must be accepted");
    db.execute("CALL pi_pg_quote_proc('O''Brien')")
        .expect("CALL must execute the body");
    assert_eq!(text(&db, "pi_pg_quote"), "O'Brien");

    db.execute(
        "CREATE PROCEDURE pi_pg_null_proc(p_note TEXT) LANGUAGE plpgsql \
         AS $$BEGIN INSERT INTO pi_pg_null VALUES ($p_note); END$$",
    )
    .expect("CREATE PROCEDURE must be accepted");
    db.execute("CALL pi_pg_null_proc(NULL)")
        .expect("CALL must execute the body");
    assert_eq!(only_row(&db, "pi_pg_null"), vec![Value::Null]);
}

#[test]
fn plpgsql_does_not_substitute_inside_string_literals_either() {
    let db = db();
    db.execute("CREATE TABLE pi_pg_lit (id INTEGER, note TEXT)").unwrap();

    db.execute(
        "CREATE PROCEDURE pi_pg_lit_proc(p_id INTEGER) LANGUAGE plpgsql \
         AS $$BEGIN INSERT INTO pi_pg_lit VALUES ($p_id, 'id is $p_id'); END$$",
    )
    .expect("CREATE PROCEDURE must be accepted");

    db.execute("CALL pi_pg_lit_proc(7)")
        .expect("CALL must execute the body");

    assert_eq!(int_text(&db, "pi_pg_lit"), (7, "id is $p_id".to_string()));
}

#[test]
fn plpgsql_unknown_placeholder_still_fails_loudly() {
    let db = db();
    db.execute("CREATE TABLE pi_pg_oops (id INTEGER)").unwrap();

    db.execute(
        "CREATE PROCEDURE pi_pg_oops_proc(p_id INTEGER) LANGUAGE plpgsql \
         AS $$BEGIN INSERT INTO pi_pg_oops VALUES ($oops); END$$",
    )
    .expect("CREATE PROCEDURE must be accepted");

    let err = db
        .execute("CALL pi_pg_oops_proc(7)")
        .expect_err("an undeclared placeholder must not be swallowed")
        .to_string();
    assert!(
        err.contains("Invalid parameter placeholder"),
        "expected 'Invalid parameter placeholder', got: {err}"
    );
    assert_eq!(rows_in(&db, "pi_pg_oops"), 0, "nothing may have been written");
}

// ===========================================================================
// C. The sigil policy, from the interpolation side. A bare name is a COLUMN.
// ===========================================================================

#[test]
fn a_bare_parameter_name_is_still_a_column_reference_in_both_languages() {
    let db = db();
    db.execute("CREATE TABLE pi_bare (n INTEGER, note TEXT)").unwrap();

    db.execute("CREATE PROCEDURE pi_bare_sql(n INTEGER) LANGUAGE sql AS $$INSERT INTO pi_bare VALUES (n, 'x')$$")
        .expect("CREATE PROCEDURE must be accepted");
    db.execute(
        "CREATE PROCEDURE pi_bare_pg(n INTEGER) LANGUAGE plpgsql \
         AS $$BEGIN INSERT INTO pi_bare VALUES (n, 'x'); END$$",
    )
    .expect("CREATE PROCEDURE must be accepted");

    for call in ["CALL pi_bare_sql(7)", "CALL pi_bare_pg(7)"] {
        let err = match db.execute(call) {
            Ok(_) => panic!("`{call}` must not resolve a bare parameter name"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("not found in schema"),
            "`{call}` must fail as a column reference, got: {err}"
        );
    }

    assert_eq!(rows_in(&db, "pi_bare"), 0, "nothing may have been written");
}
