//! NaN / Infinity / -Infinity support on the NUMERIC cast + literal path.
//!
//! Root cause of the ×62 corpus cascade: `Value::Numeric` is String-backed
//! (canonical decimal string) and `rust_decimal::Decimal` cannot represent
//! NaN/±Infinity, so `INSERT ... VALUES (...,'NaN')` errored
//! "Cannot cast 'NaN' to NUMERIC" — and because the corpus loads inside a
//! `BEGIN`, that one failure aborted the whole transaction and ~397 later
//! statements died "current transaction is aborted".
//!
//! Every test below fails on pre-change code: the `'NaN'` insert/cast errors,
//! so the rows never exist and the follow-on assertions cannot hold (the
//! `NotANumber`-rejection test is the deliberate exception — a guard that
//! must keep rejecting genuine garbage).
//!
//! PG contract exercised (numeric.c): `NaN = NaN` TRUE, `NaN` sorts greatest,
//! `-Infinity < finite < +Infinity < NaN`, arithmetic propagates NaN and
//! `Inf - Inf = NaN`, and text output is exactly `NaN`/`Infinity`/`-Infinity`
//! (the embedded, PG-wire and COPY paths all emit the stored payload string
//! verbatim, so asserting the payload proves all three output surfaces).

use heliosdb_nano::{EmbeddedDatabase, Value};

fn num(v: &Value) -> &str {
    match v {
        Value::Numeric(s) => s.as_str(),
        other => panic!("expected Value::Numeric, got {:?}", other),
    }
}

/// The exact statement from the lease that aborted the corpus load.
#[test]
fn insert_nan_literal_into_numeric_no_longer_errors() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE num_exp_div (id1 INT, id2 INT, exp_val NUMERIC)")
        .unwrap();

    let r = db.execute("INSERT INTO num_exp_div VALUES (0, 0, 'NaN')");
    assert!(r.is_ok(), "INSERT of 'NaN' into NUMERIC must succeed: {:?}", r.err());

    let rows = db.query("SELECT exp_val FROM num_exp_div", &[]).unwrap();
    assert_eq!(rows.len(), 1);
    // Round-trips as the exact PG text token (this is what wire/COPY emit).
    assert_eq!(num(rows[0].get(0).unwrap()), "NaN");
}

/// All three special literals load, in canonical PG text form, case- and
/// sign-insensitively (`nan`, `inf`, `+Inf`, `-infinity`, …).
#[test]
fn all_special_literals_canonicalize() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT, v NUMERIC)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'NaN')").unwrap();
    db.execute("INSERT INTO t VALUES (2, 'nan')").unwrap();
    db.execute("INSERT INTO t VALUES (3, 'Infinity')").unwrap();
    db.execute("INSERT INTO t VALUES (4, 'inf')").unwrap();
    db.execute("INSERT INTO t VALUES (5, '+Infinity')").unwrap();
    db.execute("INSERT INTO t VALUES (6, '-Infinity')").unwrap();
    db.execute("INSERT INTO t VALUES (7, '-inf')").unwrap();

    let rows = db.query("SELECT v FROM t ORDER BY id", &[]).unwrap();
    let got: Vec<&str> = rows.iter().map(|r| num(r.get(0).unwrap())).collect();
    assert_eq!(
        got,
        vec![
            "NaN",
            "NaN",
            "Infinity",
            "Infinity",
            "Infinity",
            "-Infinity",
            "-Infinity"
        ]
    );
}

/// A full BEGIN block that used to cascade: the middle `'NaN'` insert failed,
/// aborting the transaction, so the trailing inserts died. All must commit.
#[test]
fn begin_block_does_not_cascade_on_special_row() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE num_data (id INT, v NUMERIC)").unwrap();

    db.execute("BEGIN").unwrap();
    assert!(db.execute("INSERT INTO num_data VALUES (1, '1.5')").is_ok());
    // Previously errored here and poisoned the txn.
    assert!(db.execute("INSERT INTO num_data VALUES (2, 'NaN')").is_ok());
    // Previously: "current transaction is aborted".
    assert!(db.execute("INSERT INTO num_data VALUES (3, 'Infinity')").is_ok());
    assert!(db.execute("INSERT INTO num_data VALUES (4, '-Infinity')").is_ok());
    db.execute("COMMIT").unwrap();

    let rows = db.query("SELECT v FROM num_data ORDER BY id", &[]).unwrap();
    assert_eq!(rows.len(), 4, "all four rows must survive the transaction");
    assert_eq!(num(rows[1].get(0).unwrap()), "NaN");
}

/// PG ordering: `-Infinity < finite < +Infinity < NaN`.
#[test]
fn order_by_places_neg_inf_first_and_nan_last() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE ord (id INT, v NUMERIC)").unwrap();
    // Finite values chosen so their (canonical-string) order is unambiguous.
    for (id, v) in [
        (1, "-Infinity"),
        (2, "-5"),
        (3, "0"),
        (4, "5"),
        (5, "Infinity"),
        (6, "NaN"),
    ] {
        db.execute(&format!("INSERT INTO ord VALUES ({id}, '{v}')")).unwrap();
    }

    let rows = db.query("SELECT v FROM ord ORDER BY v ASC", &[]).unwrap();
    let got: Vec<&str> = rows.iter().map(|r| num(r.get(0).unwrap())).collect();
    assert_eq!(got.first().copied(), Some("-Infinity"));
    assert_eq!(got[got.len() - 2], "Infinity");
    assert_eq!(got.last().copied(), Some("NaN"));
}

/// MIN / MAX over a numeric column containing specials.
#[test]
fn min_max_with_specials() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE agg (v NUMERIC)").unwrap();
    for v in ["-Infinity", "-1", "0", "42", "Infinity", "NaN"] {
        db.execute(&format!("INSERT INTO agg VALUES ('{v}')")).unwrap();
    }

    let rows = db.query("SELECT MIN(v), MAX(v) FROM agg", &[]).unwrap();
    assert_eq!(num(rows[0].get(0).unwrap()), "-Infinity", "MIN is -Infinity");
    assert_eq!(num(rows[0].get(1).unwrap()), "NaN", "MAX is NaN (greatest)");
}

/// SUM over a numeric column: a NaN operand makes the whole SUM NaN (PG),
/// rather than being silently coalesced to 0.
#[test]
fn sum_propagates_nan() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE s (v NUMERIC)").unwrap();
    for v in ["1", "2", "NaN", "3"] {
        db.execute(&format!("INSERT INTO s VALUES ('{v}')")).unwrap();
    }
    let rows = db.query("SELECT SUM(v) FROM s", &[]).unwrap();
    assert_eq!(num(rows[0].get(0).unwrap()), "NaN");
}

/// `NaN = NaN` is TRUE and `NaN <> NaN` is FALSE (PG-specific, unlike IEEE).
#[test]
fn nan_equals_itself() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let eq = db.query("SELECT 'NaN'::numeric = 'NaN'::numeric", &[]).unwrap();
    assert_eq!(eq[0].get(0).unwrap(), &Value::Boolean(true));
    let ne = db.query("SELECT 'NaN'::numeric <> 'NaN'::numeric", &[]).unwrap();
    assert_eq!(ne[0].get(0).unwrap(), &Value::Boolean(false));
}

/// Ordering operators: NaN greatest, ±Infinity vs finite.
#[test]
fn special_comparison_operators() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let t = |sql: &str| {
        let rows = db.query(sql, &[]).unwrap();
        assert_eq!(rows[0].get(0).unwrap(), &Value::Boolean(true), "expected TRUE: {sql}");
    };
    t("SELECT 'NaN'::numeric > 'Infinity'::numeric");
    t("SELECT 'NaN'::numeric > '999999999'::numeric");
    t("SELECT 'Infinity'::numeric > '999999999'::numeric");
    t("SELECT '-Infinity'::numeric < '0'::numeric");
    t("SELECT '-Infinity'::numeric < 'Infinity'::numeric");
}

/// Arithmetic follows PG infinity rules; NaN propagates.
#[test]
fn special_arithmetic() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let n = |sql: &str| -> String {
        let rows = db.query(sql, &[]).unwrap();
        num(rows[0].get(0).unwrap()).to_string()
    };
    assert_eq!(n("SELECT 'NaN'::numeric + '5'::numeric"), "NaN");
    assert_eq!(n("SELECT 'Infinity'::numeric + 'Infinity'::numeric"), "Infinity");
    assert_eq!(n("SELECT 'Infinity'::numeric - 'Infinity'::numeric"), "NaN");
    assert_eq!(n("SELECT 'Infinity'::numeric * '-2'::numeric"), "-Infinity");
    assert_eq!(n("SELECT '5'::numeric / 'Infinity'::numeric"), "0");
    // Unary minus: -Infinity flips, NaN stays NaN.
    assert_eq!(n("SELECT -('Infinity'::numeric)"), "-Infinity");
    assert_eq!(n("SELECT -('NaN'::numeric)"), "NaN");
}

/// A special NUMERIC casts to float natively (PG allows `'NaN'::numeric` to
/// become `'NaN'::float8`). Exercises the numeric→float8 cast path directly.
#[test]
fn special_numeric_casts_to_float() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE f (v NUMERIC)").unwrap();
    db.execute("INSERT INTO f VALUES ('Infinity')").unwrap();
    db.execute("INSERT INTO f VALUES ('NaN')").unwrap();

    // ORDER BY v puts Infinity before NaN (NaN greatest).
    let rows = db.query("SELECT CAST(v AS float8) FROM f ORDER BY v", &[]).unwrap();
    assert_eq!(rows.len(), 2);
    match rows[0].get(0).unwrap() {
        Value::Float8(f) => assert!(f.is_infinite() && f.is_sign_positive(), "got {f}"),
        other => panic!("expected Float8(inf), got {:?}", other),
    }
    match rows[1].get(0).unwrap() {
        Value::Float8(f) => assert!(f.is_nan(), "got {f}"),
        other => panic!("expected Float8(NaN), got {:?}", other),
    }
}

/// Genuine garbage must still be rejected — the fix must not over-accept.
#[test]
fn non_numeric_text_still_rejected() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (v NUMERIC)").unwrap();
    assert!(db.execute("INSERT INTO t VALUES ('NotANumber')").is_err());
    assert!(db.execute("INSERT INTO t VALUES ('infi')").is_err());
    assert!(db.query("SELECT 'NotANumber'::numeric", &[]).is_err());
}

/// Window ORDER BY over a NUMERIC column must order finite values numerically
/// (2 < 10 < 100), never lexicographically ("10" < "100" < "2"), and place a
/// NaN token last (greatest) per PG rank. The window comparator is a distinct
/// path from the main-query sort; a lexicographic finite rule there regresses
/// ROW_NUMBER / RANK / LAG-LEAD offsets / MIN-MAX OVER frames.
#[test]
fn window_order_by_numeric_is_numeric_not_lexicographic() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE w (v NUMERIC)").unwrap();
    // Insertion order is deliberately unsorted; the window ORDER BY imposes rank.
    for v in ["100", "2", "NaN", "10"] {
        db.execute(&format!("INSERT INTO w VALUES ('{v}')")).unwrap();
    }
    let rows = db
        .query("SELECT v, ROW_NUMBER() OVER (ORDER BY v) FROM w", &[])
        .unwrap();
    let mut rank: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for r in &rows {
        let v = num(r.get(0).unwrap()).to_string();
        let rn = match r.get(1).unwrap() {
            Value::Int8(n) => *n,
            other => panic!("expected Int8 ROW_NUMBER, got {:?}", other),
        };
        rank.insert(v, rn);
    }
    assert_eq!(rank.get("2"), Some(&1), "2 ranks first");
    assert_eq!(rank.get("10"), Some(&2), "10 ranks second (numeric, not lexicographic)");
    assert_eq!(rank.get("100"), Some(&3), "100 ranks third");
    assert_eq!(rank.get("NaN"), Some(&4), "NaN ranks last (greatest)");
}

/// REGRESSION (release-blocking): main-query `ORDER BY` over a NUMERIC column
/// with multi-digit finite values must order numerically (2, 9, 10, 25, 100),
/// never lexicographically ("10","100","2","25","9"). The main-query sort path
/// (SortOperator → executor `compare_values` → `cmp_sort`) is distinct from the
/// window comparator; a lexicographic finite `cmp_sort` regressed it. The
/// pre-existing `order_by_places_neg_inf_first_and_nan_last` test could not
/// catch this because it deliberately used single-token finite values
/// (`-5`,`0`,`5`) whose byte order already matches their numeric order.
#[test]
fn order_by_finite_numeric_is_numeric_not_lexicographic() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (x NUMERIC)").unwrap();
    for v in ["2", "10", "9", "100", "25"] {
        db.execute(&format!("INSERT INTO t VALUES ({v})")).unwrap();
    }
    let rows = db.query("SELECT x FROM t ORDER BY x", &[]).unwrap();
    let got: Vec<&str> = rows.iter().map(|r| num(r.get(0).unwrap())).collect();
    assert_eq!(
        got,
        vec!["2", "9", "10", "25", "100"],
        "ORDER BY numeric must be numeric, not lexicographic"
    );
    // DESC is the exact reverse.
    let rows_desc = db.query("SELECT x FROM t ORDER BY x DESC", &[]).unwrap();
    let got_desc: Vec<&str> = rows_desc.iter().map(|r| num(r.get(0).unwrap())).collect();
    assert_eq!(got_desc, vec!["100", "25", "10", "9", "2"]);
}

/// REGRESSION: MIN / MAX over multi-digit finite NUMERIC values. A lexicographic
/// comparator returned MAX="9", MIN="10"; correct numeric answers are 100 / 2.
#[test]
fn min_max_finite_numeric_is_numeric() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (x NUMERIC)").unwrap();
    for v in ["2", "10", "9", "100", "25"] {
        db.execute(&format!("INSERT INTO t VALUES ({v})")).unwrap();
    }
    let rows = db.query("SELECT MAX(x), MIN(x) FROM t", &[]).unwrap();
    assert_eq!(num(rows[0].get(0).unwrap()), "100", "MAX must be 100, not 9");
    assert_eq!(num(rows[0].get(1).unwrap()), "2", "MIN must be 2, not 10");
}

/// REGRESSION: a single `ORDER BY` mixing multi-digit finite values with all
/// three special tokens must place them `-Infinity < 2 < 10 < +Infinity < NaN`
/// — exercising both the numeric finite ordering AND the PG special rank
/// overlay in one sort.
#[test]
fn order_by_mixes_finite_and_specials() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (x NUMERIC)").unwrap();
    // Insertion order deliberately scrambled.
    for v in ["NaN", "10", "-Infinity", "2", "Infinity"] {
        db.execute(&format!("INSERT INTO t VALUES ('{v}')")).unwrap();
    }
    let rows = db.query("SELECT x FROM t ORDER BY x", &[]).unwrap();
    let got: Vec<&str> = rows.iter().map(|r| num(r.get(0).unwrap())).collect();
    assert_eq!(got, vec!["-Infinity", "2", "10", "Infinity", "NaN"]);
}

/// GREATEST / LEAST over NUMERIC specials: NaN is greatest, +Infinity beats a
/// finite value, -Infinity loses. Pre-fix the (Numeric,Numeric) comparator
/// parsed both operands as Decimal, so any special token errored instead of
/// ranking. Finite operands must stay numerically ordered (regression guard:
/// a lexicographic comparator would pick '2' as GREATEST of 2/10/100).
#[test]
fn greatest_least_with_specials() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let n = |sql: &str| -> String {
        let rows = db.query(sql, &[]).unwrap();
        num(rows[0].get(0).unwrap()).to_string()
    };
    assert_eq!(n("SELECT GREATEST('NaN'::numeric, '5'::numeric)"), "NaN");
    assert_eq!(n("SELECT LEAST('Infinity'::numeric, '5'::numeric)"), "5");
    assert_eq!(n("SELECT GREATEST('Infinity'::numeric, '5'::numeric)"), "Infinity");
    assert_eq!(n("SELECT LEAST('-Infinity'::numeric, '5'::numeric)"), "-Infinity");
    assert_eq!(n("SELECT GREATEST('2'::numeric, '10'::numeric, '100'::numeric)"), "100");
    assert_eq!(n("SELECT LEAST('2'::numeric, '10'::numeric, '100'::numeric)"), "2");
}
