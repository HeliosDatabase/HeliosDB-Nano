//! `DELETE FROM t RETURNING …` — the bare form (no `WHERE`, no alias).
//!
//! ## The defect
//!
//! sqlparser 0.53's DELETE grammar allows a BARE table alias, and `RETURNING`
//! is not reserved in that position — so `DELETE FROM t RETURNING id` bound
//! `RETURNING` as the table's alias and then rejected the leftover column list
//! with `Expected: end of statement, found: id at Line: 1, Column: 25`, the
//! error column landing immediately after `RETURNING `. `RETURNING` is
//! advertised for DELETE in the README and in the `heliosdb-nano-query` skill,
//! so this was a documented surface reachable only with a redundant `WHERE`.
//!
//! The measured matrix, and what pins the mechanism — every escape works by
//! filling or ending the alias slot:
//!
//! | statement                                    | before | why |
//! |----------------------------------------------|--------|-----|
//! | `DELETE FROM t RETURNING id`                 | ERROR  | alias slot eats `RETURNING` |
//! | `DELETE FROM t RETURNING *`                  | ERROR  | same |
//! | `DELETE FROM t RETURNING id, amount`         | ERROR  | same |
//! | `DELETE FROM public.t RETURNING id`          | ERROR  | same |
//! | `DELETE FROM t AS x RETURNING id`            | ok     | slot filled explicitly |
//! | `DELETE FROM t WHERE amount > 0 RETURNING id`| ok     | `WHERE` ends alias lookahead |
//! | `DELETE FROM t WHERE TRUE RETURNING id`      | ok     | same |
//! | `UPDATE t SET amount = 1 RETURNING id`       | ok     | UPDATE has no bare-alias slot |
//! | `INSERT INTO t VALUES (…) RETURNING id`      | ok     | INSERT has no bare-alias slot |
//!
//! It failed identically through `db.query` and `db.query_params` because it
//! failed at PARSE time, before either executor family was reached.
//!
//! ## The fix, and what these tests must prove
//!
//! `Parser::rewrite_delete_returning` (`src/sql/parser.rs`) inserts `WHERE TRUE`
//! between the table reference and `RETURNING`, and is chained into the
//! parse-failure fallback that already carries the Stage-0 partitioning
//! rewrites — so it runs ONLY on SQL that fails to parse today, and a rewrite
//! that still fails reports the ORIGINAL diagnostic.
//!
//! Every assertion below therefore checks BOTH halves — the rows RETURNED and
//! the rows actually DELETED. A rewrite that parses but deletes nothing, or one
//! that deletes rows a `WHERE` should have spared, passes an `is_ok()` check and
//! fails here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};

const DDL: &str = "CREATE TABLE t (id INT PRIMARY KEY, amount INT, note TEXT)";

/// Three rows: ids 1/2/3, amounts 10/20/30. Row 2's `note` is literally the
/// word `RETURNING`, so every fixture doubles as a check that a keyword sitting
/// in DATA never influences the rewrite.
fn seeded() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute(DDL).unwrap();
    db.execute("INSERT INTO t VALUES (1, 10, 'alpha')").unwrap();
    db.execute("INSERT INTO t VALUES (2, 20, 'RETURNING')").unwrap();
    db.execute("INSERT INTO t VALUES (3, 30, 'gamma')").unwrap();
    db
}

/// Integer column `col` of every row, sorted. Width-agnostic so the assertions
/// do not depend on whether `INT` materializes as `Int4` or `Int8`.
fn ints(rows: &[Tuple], col: usize) -> Vec<i64> {
    let mut out: Vec<i64> = rows
        .iter()
        .map(|t| match t.values.get(col) {
            Some(Value::Int2(v)) => i64::from(*v),
            Some(Value::Int4(v)) => i64::from(*v),
            Some(Value::Int8(v)) => *v,
            other => panic!("expected an integer at column {col}, got {other:?} in {t:?}"),
        })
        .collect();
    out.sort_unstable();
    out
}

fn texts(rows: &[Tuple], col: usize) -> Vec<String> {
    let mut out: Vec<String> = rows
        .iter()
        .map(|t| match t.values.get(col) {
            Some(Value::String(s)) => s.clone(),
            other => panic!("expected a string at column {col}, got {other:?} in {t:?}"),
        })
        .collect();
    out.sort();
    out
}

/// Rows still in the table, by id. `SELECT id` (not `SELECT *`, and no `WHERE`)
/// deliberately misses every read fast path, so this reads the real table.
fn surviving_ids(db: &EmbeddedDatabase, table: &str) -> Vec<i64> {
    let rows = db.query(&format!("SELECT id FROM {table}"), &[]).unwrap();
    ints(&rows, 0)
}

// ---------------------------------------------------------------------------
// The headline defect, on every surface that measured it
// ---------------------------------------------------------------------------

/// `DELETE FROM t RETURNING id` on all four public entry points. Each gets its
/// own database because each one deletes the whole table.
#[test]
fn bare_delete_returning_returns_every_row_and_deletes_every_row() {
    const SQL: &str = "DELETE FROM t RETURNING id";

    // `db.query` — the measured surface.
    let db = seeded();
    let rows = db.query(SQL, &[]).unwrap();
    assert_eq!(ints(&rows, 0), vec![1, 2, 3], "db.query: wrong RETURNING rows");
    assert_eq!(surviving_ids(&db, "t"), Vec::<i64>::new(), "db.query: rows survived");

    // `db.query_params` — the other measured surface.
    let db = seeded();
    let rows = db.query_params(SQL, &[]).unwrap();
    assert_eq!(ints(&rows, 0), vec![1, 2, 3], "db.query_params: wrong RETURNING rows");
    assert_eq!(
        surviving_ids(&db, "t"),
        Vec::<i64>::new(),
        "db.query_params: rows survived"
    );

    // `db.execute_returning` — count AND rows from one call.
    let db = seeded();
    let (count, rows) = db.execute_returning(SQL).unwrap();
    assert_eq!(count, 3, "execute_returning reported the wrong delete count");
    assert_eq!(ints(&rows, 0), vec![1, 2, 3]);
    assert_eq!(surviving_ids(&db, "t"), Vec::<i64>::new());

    // `db.execute` — the TEXT executor family (`execute_in_transaction_inner`),
    // which discards RETURNING rows but must still parse the statement and
    // delete exactly the right rows.
    let db = seeded();
    assert_eq!(db.execute(SQL).unwrap(), 3, "db.execute: wrong delete count");
    assert_eq!(surviving_ids(&db, "t"), Vec::<i64>::new(), "db.execute: rows survived");
}

/// Both families must agree on the same statement — the property that made the
/// defect family-independent in the first place.
#[test]
fn both_executor_families_agree_on_the_same_statement() {
    const SQL: &str = "DELETE FROM t RETURNING id, amount";

    let text_db = seeded();
    let text_rows = text_db.query(SQL, &[]).unwrap();

    let params_db = seeded();
    let params_rows = params_db.query_params(SQL, &[]).unwrap();

    assert_eq!(ints(&text_rows, 0), ints(&params_rows, 0), "families disagree on ids");
    assert_eq!(
        ints(&text_rows, 1),
        ints(&params_rows, 1),
        "families disagree on amounts"
    );
    assert_eq!(surviving_ids(&text_db, "t"), surviving_ids(&params_db, "t"));
    assert_eq!(surviving_ids(&text_db, "t"), Vec::<i64>::new());
}

// ---------------------------------------------------------------------------
// Projection shapes
// ---------------------------------------------------------------------------

#[test]
fn returning_star_projects_the_whole_row() {
    let db = seeded();
    let (count, rows) = db.execute_returning("DELETE FROM t RETURNING *").unwrap();
    assert_eq!(count, 3);
    assert_eq!(rows.len(), 3);
    for row in &rows {
        assert_eq!(row.values.len(), 3, "RETURNING * must project all 3 columns: {row:?}");
    }
    assert_eq!(ints(&rows, 0), vec![1, 2, 3]);
    assert_eq!(ints(&rows, 1), vec![10, 20, 30]);
    assert_eq!(texts(&rows, 2), vec!["RETURNING", "alpha", "gamma"]);
    assert_eq!(surviving_ids(&db, "t"), Vec::<i64>::new());
}

#[test]
fn returning_an_explicit_column_list_projects_exactly_those_columns() {
    let db = seeded();
    let (count, rows) = db.execute_returning("DELETE FROM t RETURNING id, amount").unwrap();
    assert_eq!(count, 3);
    for row in &rows {
        assert_eq!(row.values.len(), 2, "expected a 2-column projection: {row:?}");
    }
    assert_eq!(ints(&rows, 0), vec![1, 2, 3]);
    assert_eq!(ints(&rows, 1), vec![10, 20, 30]);
    assert_eq!(surviving_ids(&db, "t"), Vec::<i64>::new());

    let db = seeded();
    let (count, rows) = db.execute_returning("DELETE FROM t RETURNING note").unwrap();
    assert_eq!(count, 3);
    assert_eq!(texts(&rows, 0), vec!["RETURNING", "alpha", "gamma"]);
    assert_eq!(surviving_ids(&db, "t"), Vec::<i64>::new());
}

// ---------------------------------------------------------------------------
// Table-reference shapes
// ---------------------------------------------------------------------------

#[test]
fn schema_qualified_table_reference() {
    let db = seeded();
    let (count, rows) = db.execute_returning("DELETE FROM public.t RETURNING id").unwrap();
    assert_eq!(count, 3);
    assert_eq!(ints(&rows, 0), vec![1, 2, 3]);
    assert_eq!(surviving_ids(&db, "t"), Vec::<i64>::new());

    // …and through the params family too.
    let db = seeded();
    let rows = db.query_params("DELETE FROM public.t RETURNING *", &[]).unwrap();
    assert_eq!(ints(&rows, 0), vec![1, 2, 3]);
    assert_eq!(surviving_ids(&db, "public.t"), Vec::<i64>::new());
}

#[test]
fn quoted_table_reference_including_one_with_a_space() {
    // Mixed-case quoted identifier: the classic quoted case.
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute(r#"CREATE TABLE "Orders" (id INT PRIMARY KEY, amount INT)"#)
        .unwrap();
    db.execute(r#"INSERT INTO "Orders" VALUES (1, 10)"#).unwrap();
    db.execute(r#"INSERT INTO "Orders" VALUES (2, 20)"#).unwrap();
    let (count, rows) = db.execute_returning(r#"DELETE FROM "Orders" RETURNING id"#).unwrap();
    assert_eq!(count, 2);
    assert_eq!(ints(&rows, 0), vec![1, 2]);
    assert_eq!(surviving_ids(&db, r#""Orders""#), Vec::<i64>::new());

    // A quoted name containing whitespace: the case that would break any
    // rewrite scanning for the table reference without honouring quotes.
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute(r#"CREATE TABLE "my table" (id INT PRIMARY KEY, amount INT)"#)
        .unwrap();
    db.execute(r#"INSERT INTO "my table" VALUES (1, 10)"#).unwrap();
    db.execute(r#"INSERT INTO "my table" VALUES (2, 20)"#).unwrap();
    let (count, rows) = db.execute_returning(r#"DELETE FROM "my table" RETURNING *"#).unwrap();
    assert_eq!(count, 2, "quoted table name with a space");
    assert_eq!(ints(&rows, 0), vec![1, 2]);
    assert_eq!(ints(&rows, 1), vec![10, 20]);
    assert_eq!(surviving_ids(&db, r#""my table""#), Vec::<i64>::new());
}

// ---------------------------------------------------------------------------
// Lexical shapes
// ---------------------------------------------------------------------------

#[test]
fn mixed_case_extra_whitespace_newlines_and_a_trailing_semicolon() {
    for sql in [
        "delete from t returning id",
        "Delete From t Returning id",
        "DELETE FROM t RETURNING id;",
        "DELETE   FROM    t    RETURNING    id",
        "DELETE\nFROM\n  t\nRETURNING\n  id",
        "  \n DELETE FROM t RETURNING id ;  ",
    ] {
        let db = seeded();
        let (count, rows) = db
            .execute_returning(sql)
            .unwrap_or_else(|e| panic!("{sql:?} failed: {e}"));
        assert_eq!(count, 3, "{sql:?} deleted the wrong number of rows");
        assert_eq!(ints(&rows, 0), vec![1, 2, 3], "{sql:?} returned the wrong rows");
        assert_eq!(surviving_ids(&db, "t"), Vec::<i64>::new(), "{sql:?} left rows behind");
    }
}

// ---------------------------------------------------------------------------
// Regression pins: shapes that ALREADY worked and must keep working
// ---------------------------------------------------------------------------

/// The rewrite runs only after a parse failure, so none of these should ever
/// reach it. They are pinned because they are exactly the statements a broken
/// rewrite would corrupt — an over-eager `WHERE TRUE` injection into any of
/// them turns a filtered delete into a whole-table delete.
#[test]
fn shapes_that_already_worked_are_unchanged() {
    // Explicit alias — fills the slot the defect stole.
    let db = seeded();
    let (count, rows) = db.execute_returning("DELETE FROM t AS x RETURNING id").unwrap();
    assert_eq!(count, 3);
    assert_eq!(ints(&rows, 0), vec![1, 2, 3]);
    assert_eq!(surviving_ids(&db, "t"), Vec::<i64>::new());

    // `WHERE` that matches everything.
    let db = seeded();
    let (count, rows) = db
        .execute_returning("DELETE FROM t WHERE amount > 0 RETURNING id")
        .unwrap();
    assert_eq!(count, 3);
    assert_eq!(ints(&rows, 0), vec![1, 2, 3]);
    assert_eq!(surviving_ids(&db, "t"), Vec::<i64>::new());

    // The documented workaround, which must still mean the same thing.
    let db = seeded();
    let (count, rows) = db.execute_returning("DELETE FROM t WHERE TRUE RETURNING id").unwrap();
    assert_eq!(count, 3);
    assert_eq!(ints(&rows, 0), vec![1, 2, 3]);
    assert_eq!(surviving_ids(&db, "t"), Vec::<i64>::new());

    // A SELECTIVE `WHERE`: the one that proves no blanket `WHERE TRUE` was
    // injected. One row out, two rows left.
    let db = seeded();
    let (count, rows) = db
        .execute_returning("DELETE FROM t WHERE amount = 20 RETURNING id, note")
        .unwrap();
    assert_eq!(count, 1, "a selective DELETE must not become a whole-table DELETE");
    assert_eq!(ints(&rows, 0), vec![2]);
    assert_eq!(texts(&rows, 1), vec!["RETURNING"]);
    assert_eq!(surviving_ids(&db, "t"), vec![1, 3]);

    // Same, through the params family with a bound value.
    let db = seeded();
    let (count, rows) = db
        .execute_params_returning("DELETE FROM t WHERE amount = $1 RETURNING id", &[Value::Int4(20)])
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(ints(&rows, 0), vec![2]);
    assert_eq!(surviving_ids(&db, "t"), vec![1, 3]);

    // DELETE with no RETURNING at all is untouched.
    let db = seeded();
    assert_eq!(db.execute("DELETE FROM t").unwrap(), 3);
    assert_eq!(surviving_ids(&db, "t"), Vec::<i64>::new());
    let db = seeded();
    assert_eq!(db.execute("DELETE FROM t WHERE id = 1").unwrap(), 1);
    assert_eq!(surviving_ids(&db, "t"), vec![2, 3]);
}

/// UPDATE and INSERT never had the defect (neither grammar has a bare-alias
/// slot there) and must be entirely unaffected by the DELETE rewrite.
#[test]
fn update_and_insert_returning_are_unchanged_controls() {
    let db = seeded();
    let (count, rows) = db.execute_returning("UPDATE t SET amount = 1 RETURNING id").unwrap();
    assert_eq!(count, 3, "UPDATE ... RETURNING must still update every row");
    assert_eq!(ints(&rows, 0), vec![1, 2, 3]);
    assert_eq!(surviving_ids(&db, "t"), vec![1, 2, 3], "UPDATE must not delete rows");
    let amounts = db.query("SELECT amount FROM t", &[]).unwrap();
    assert_eq!(ints(&amounts, 0), vec![1, 1, 1]);

    let db = seeded();
    let (count, rows) = db
        .execute_returning("INSERT INTO t VALUES (4, 40, 'delta') RETURNING id")
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(ints(&rows, 0), vec![4]);
    assert_eq!(surviving_ids(&db, "t"), vec![1, 2, 3, 4]);

    let db = seeded();
    let (count, rows) = db
        .execute_returning("INSERT INTO t VALUES (4, 40, 'delta') RETURNING *")
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(rows.len(), 1);
    assert_eq!(ints(&rows, 0), vec![4]);
}

// ---------------------------------------------------------------------------
// Literal boundaries — the v4.12.1 lesson
// ---------------------------------------------------------------------------

/// The word `RETURNING` living in DATA, in a `WHERE` literal, or in a comment
/// must never be mistaken for the clause. The rewrite never searches the text
/// for the word — it requires `RETURNING` to be the token immediately after the
/// table reference — so all of these are structurally out of reach; these tests
/// pin that.
#[test]
fn the_word_returning_in_data_or_a_literal_is_never_the_clause() {
    // Row 2's `note` IS the word. A bare DELETE ... RETURNING must hand it back
    // intact, not treat it as syntax.
    let db = seeded();
    let (count, rows) = db.execute_returning("DELETE FROM t RETURNING id, note").unwrap();
    assert_eq!(count, 3);
    assert_eq!(texts(&rows, 1), vec!["RETURNING", "alpha", "gamma"]);
    assert_eq!(surviving_ids(&db, "t"), Vec::<i64>::new());

    // A `WHERE` predicate whose literal contains the word: this parses today,
    // so the rewrite is never reached — and it must delete exactly one row.
    let db = seeded();
    let (count, rows) = db
        .execute_returning("DELETE FROM t WHERE note = 'RETURNING' RETURNING id")
        .unwrap();
    assert_eq!(count, 1, "the literal must not turn this into a whole-table DELETE");
    assert_eq!(ints(&rows, 0), vec![2]);
    assert_eq!(surviving_ids(&db, "t"), vec![1, 3]);

    // A literal that contains the word AND a table-looking prefix.
    let db = seeded();
    let (count, rows) = db
        .execute_returning("DELETE FROM t WHERE note = 'x RETURNING y' RETURNING id")
        .unwrap();
    assert_eq!(count, 0, "no row has that note, so nothing may be deleted");
    assert_eq!(rows.len(), 0);
    assert_eq!(surviving_ids(&db, "t"), vec![1, 2, 3]);

    // An inline comment mentioning the word, on a statement that is otherwise
    // selective.
    let db = seeded();
    let (count, rows) = db
        .execute_returning("DELETE FROM t /* RETURNING everything */ WHERE id = 1 RETURNING id")
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(ints(&rows, 0), vec![1]);
    assert_eq!(surviving_ids(&db, "t"), vec![2, 3]);

    // And the same comment on the bare form, which DOES need the rewrite.
    let db = seeded();
    let (count, rows) = db
        .execute_returning("DELETE FROM t -- delete them all\nRETURNING id")
        .unwrap();
    assert_eq!(count, 3);
    assert_eq!(ints(&rows, 0), vec![1, 2, 3]);
    assert_eq!(surviving_ids(&db, "t"), Vec::<i64>::new());
}

/// A row whose TEXT column contains a quote as well as the keyword — the
/// combination the v4.12.1 naive-scan corruption was made of.
#[test]
fn a_quote_bearing_row_survives_the_round_trip() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE q (id INT PRIMARY KEY, note TEXT)").unwrap();
    db.execute("INSERT INTO q VALUES (1, 'O''Brien RETURNING *')").unwrap();
    db.execute("INSERT INTO q VALUES (2, '-- RETURNING id')").unwrap();

    let (count, rows) = db.execute_returning("DELETE FROM q RETURNING id, note").unwrap();
    assert_eq!(count, 2);
    assert_eq!(texts(&rows, 1), vec!["-- RETURNING id", "O'Brien RETURNING *"]);
    assert_eq!(surviving_ids(&db, "q"), Vec::<i64>::new());
}

// ---------------------------------------------------------------------------
// The strictly-additive guarantee
// ---------------------------------------------------------------------------

/// A DELETE that is malformed for its OWN reason must report the ORIGINAL
/// diagnostic — never one whose offsets refer to the injected `WHERE TRUE`.
#[test]
fn a_malformed_delete_reports_its_original_diagnostic() {
    let db = seeded();

    // The rewrite CLAIMS this one (`RETURNING` is the next token) but the
    // rewritten text still won't parse: sqlparser rejects the trailing `FROM`
    // at a different offset. The message must still be about `id` — the
    // original complaint — at its original position.
    let err = db
        .query("DELETE FROM t RETURNING id FROM", &[])
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("found: id"),
        "expected the ORIGINAL diagnostic (about `id`), got: {err}"
    );
    assert!(
        !err.contains("found: FROM"),
        "the rewritten statement's diagnostic leaked out: {err}"
    );
    assert!(!err.contains("TRUE"), "the injected predicate leaked into: {err}");

    // The rewrite DECLINES this one (a `WHERE` is present), so the original
    // error passes straight through.
    let err = db.query("DELETE FROM t WHERE amount >", &[]).unwrap_err().to_string();
    assert!(!err.contains("TRUE"), "the injected predicate leaked into: {err}");

    // A DELETE against a table that does not exist must keep failing, and must
    // not be turned into a parse error by the rewrite.
    let err = db
        .query("DELETE FROM no_such_table RETURNING id", &[])
        .unwrap_err()
        .to_string();
    assert!(
        err.to_lowercase().contains("no_such_table"),
        "expected a table-resolution error naming the table, got: {err}"
    );

    // Nothing above may have touched the data.
    assert_eq!(surviving_ids(&db, "t"), vec![1, 2, 3]);
}
