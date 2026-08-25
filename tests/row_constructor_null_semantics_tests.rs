//! PostgreSQL NULL semantics for row-constructor (tuple) comparison.
//!
//! PostgreSQL 9.24.5 splits these into two families with DIFFERENT NULL rules,
//! and getting them uniform is itself a bug:
//!
//!   ordering (`<` `<=` `>` `>=`) — compare left-to-right, stopping at the first
//!     pair that is unequal OR null; if that stopping pair holds a null the
//!     result is unknown. So a null EARLY beats a decisive pair later.
//!   equality (`=` `<>`) — rows are unequal if ANY corresponding members are
//!     non-null and unequal; only if no decisive pair exists is it unknown. So a
//!     decisive pair later beats a null early — the opposite precedence.
//!
//! Before the fix, ordering ops skipped past a null pair and let a later pair
//! decide, so `(NULL,1) < (2,3)` answered TRUE where PostgreSQL answers NULL —
//! a silently wrong answer on the keyset-pagination shape. Reported by the Lite
//! team while evaluating Nano's row-constructor support.

use heliosdb_nano::{EmbeddedDatabase, Value};

/// A WHERE predicate keeps a row only when it evaluates to TRUE; NULL and FALSE
/// both filter it out. So `WHERE <pred>` cannot tell NULL from FALSE — these
/// tests select the expression instead and inspect the value directly.
fn eval(db: &EmbeddedDatabase, expr: &str) -> Value {
    let rows = db
        .query(&format!("SELECT {expr} AS r"), &[])
        .unwrap_or_else(|e| panic!("query failed for `{expr}`: {e}"));
    assert_eq!(rows.len(), 1, "expected exactly one row for `{expr}`");
    rows[0].get(0).cloned().unwrap_or(Value::Null)
}

fn db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().unwrap()
}

// --- ordering operators: a NULL pair stops the walk -------------------------

#[test]
fn ordering_null_in_first_pair_is_unknown_even_when_a_later_pair_decides() {
    let db = db();
    // The regression: pair 2 (1 < 3) is decisive, but pair 1 is null, and
    // PostgreSQL stops there. Returned Boolean(true) before the fix.
    assert_eq!(eval(&db, "(NULL, 1) < (2, 3)"), Value::Null);
    assert_eq!(eval(&db, "(NULL, 1) > (2, 3)"), Value::Null);
    assert_eq!(eval(&db, "(NULL, 1) <= (2, 3)"), Value::Null);
    assert_eq!(eval(&db, "(NULL, 1) >= (2, 3)"), Value::Null);
}

#[test]
fn ordering_null_on_the_right_side_is_also_unknown() {
    let db = db();
    assert_eq!(eval(&db, "(1, 2) < (NULL, 3)"), Value::Null);
}

#[test]
fn ordering_null_after_an_equal_pair_is_unknown() {
    let db = db();
    // First pair equal → walk continues → second pair null → stop → unknown.
    // This case was already correct; it guards the `saw_null` fall-through.
    assert_eq!(eval(&db, "(2, NULL) < (2, 3)"), Value::Null);
}

#[test]
fn ordering_decisive_pair_before_any_null_still_answers() {
    let db = db();
    // The null never gets reached, so the result is definite. Guards against
    // over-applying the fix by returning NULL whenever a null appears anywhere.
    assert_eq!(eval(&db, "(1, NULL) < (2, 3)"), Value::Boolean(true));
    assert_eq!(eval(&db, "(9, NULL) < (2, 3)"), Value::Boolean(false));
}

#[test]
fn ordering_without_nulls_is_lexicographic() {
    let db = db();
    assert_eq!(eval(&db, "(1, 2) < (1, 3)"), Value::Boolean(true));
    assert_eq!(eval(&db, "(1, 3) < (1, 2)"), Value::Boolean(false));
    assert_eq!(eval(&db, "(1, 2) <= (1, 2)"), Value::Boolean(true));
    assert_eq!(eval(&db, "(2, 0) > (1, 9)"), Value::Boolean(true));
}

// --- equality operators: a decisive pair beats an earlier NULL --------------

#[test]
fn equality_keeps_scanning_past_a_null_for_a_decisive_pair() {
    let db = db();
    // Opposite precedence to the ordering ops: 1 <> 3 is decisive and non-null,
    // so the rows are unequal despite the leading null. If the NULL fix were
    // applied uniformly these would wrongly become NULL.
    assert_eq!(eval(&db, "(NULL, 1) = (2, 3)"), Value::Boolean(false));
    assert_eq!(eval(&db, "(NULL, 1) <> (2, 3)"), Value::Boolean(true));
}

#[test]
fn equality_is_unknown_when_no_pair_is_decisive() {
    let db = db();
    assert_eq!(eval(&db, "(NULL, 2) = (1, 2)"), Value::Null);
    assert_eq!(eval(&db, "(NULL, 2) <> (1, 2)"), Value::Null);
}

#[test]
fn equality_without_nulls_is_elementwise() {
    let db = db();
    assert_eq!(eval(&db, "(1, 2) = (1, 2)"), Value::Boolean(true));
    assert_eq!(eval(&db, "(1, 2) = (1, 3)"), Value::Boolean(false));
    assert_eq!(eval(&db, "(1, 2) <> (1, 3)"), Value::Boolean(true));
}

// --- the shape this actually matters for ------------------------------------

#[test]
fn keyset_pagination_predicate_excludes_rows_with_a_null_key() {
    let db = db();
    db.execute("CREATE TABLE k (created_at INT, id INT, tag TEXT)").unwrap();
    db.execute("INSERT INTO k VALUES (1020, 21, 'solid')").unwrap();
    db.execute("INSERT INTO k VALUES (NULL, 99, 'null_key')").unwrap();
    db.execute("INSERT INTO k VALUES (1019, 5, 'older')").unwrap();

    // A NULL sort key makes the comparison unknown, so the row is filtered out
    // rather than silently paginated in. Before the fix `(NULL, 99)` compared
    // TRUE via its id and leaked into every keyset page.
    let rows = db
        .query(
            "SELECT tag FROM k WHERE (created_at, id) < (1020, 25) ORDER BY created_at DESC, id DESC",
            &[],
        )
        .unwrap();
    let tags: Vec<String> = rows
        .iter()
        .filter_map(|r| match r.get(0) {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert!(
        !tags.contains(&"null_key".to_string()),
        "NULL-keyed row must not match: {tags:?}"
    );
    assert_eq!(tags, vec!["solid".to_string(), "older".to_string()]);
}

#[test]
fn arity_mismatch_is_still_a_clean_error() {
    let db = db();
    let err = db
        .query("SELECT (1, 2) < (1, 2, 3) AS r", &[])
        .expect_err("row constructors of different arity must error");
    assert!(
        err.to_string().to_lowercase().contains("size mismatch") || err.to_string().to_lowercase().contains("arity"),
        "unexpected message: {err}"
    );
}
