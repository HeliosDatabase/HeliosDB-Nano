//! Regressions from the PGConf.Brasil 2026 demo capture (brief: nano-pgconf-demo-2026-09-03/ISSUES.md),
//! each verified STILL PRESENT on v4.29.0 by direct repro before the fix, and each of these tests
//! observed FAILING on that tree before the patch was applied.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use heliosdb_nano::{EmbeddedDatabase, Value};

fn db_with_docs() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE docs (id INT PRIMARY KEY, body TEXT, embedding VECTOR(3))")
        .unwrap();
    for i in 1..=60 {
        let (a, b, c) = (
            (i as f32 * 0.37).sin(),
            (i as f32 * 0.11).cos(),
            (i as f32 * 0.05).sin(),
        );
        db.execute(&format!(
            "INSERT INTO docs VALUES ({i}, 'doc {i}', '[{a:.4}, {b:.4}, {c:.4}]')"
        ))
        .unwrap();
    }
    db
}
fn ids(db: &EmbeddedDatabase, sql: &str) -> Vec<i32> {
    db.query(sql, &[])
        .unwrap()
        .iter()
        .map(|r| match r.values[0] {
            Value::Int4(i) => i,
            ref o => panic!("{o:?}"),
        })
        .collect()
}

/// Issue 1 (silent wrong order): an ORDER BY key that cannot be evaluated must be an ERROR under
/// LIMIT, exactly as it already is without LIMIT and in the select list — never a silent sort on NULL.
#[test]
fn unevaluable_order_by_key_under_limit_is_an_error_not_silent_order() {
    let db = db_with_docs();
    let r = db.query(
        "SELECT id FROM docs ORDER BY embedding <=> (SELECT embedding FROM docs WHERE id = 42) LIMIT 5",
        &[],
    );
    assert!(
        r.is_err(),
        "v4.29.0 returned ids 10,11,12,13,1 here with no error; got {r:?}"
    );
    // The same statement without LIMIT already errored — the two paths must agree.
    let r2 = db.query(
        "SELECT id FROM docs ORDER BY embedding <=> (SELECT embedding FROM docs WHERE id = 42)",
        &[],
    );
    assert_eq!(
        r.is_err(),
        r2.is_err(),
        "LIMIT and non-LIMIT paths must agree on an unevaluable key"
    );
}

/// Issue 2 (brace literal): `{…}` — the format the PG wire printed — must be accepted on every
/// operator and give the SAME KNN order as `[…]`; on v4.29.0 both operators silently returned id order.
#[test]
fn brace_vector_literal_gives_the_same_knn_order_as_brackets() {
    let db = db_with_docs();
    for op in ["<=>", "<->"] {
        let bracket = ids(
            &db,
            &format!("SELECT id FROM docs ORDER BY embedding {op} '[0.1, 0.2, 0.3]' LIMIT 5"),
        );
        let brace = ids(
            &db,
            &format!("SELECT id FROM docs ORDER BY embedding {op} '{{0.1, 0.2, 0.3}}' LIMIT 5"),
        );
        assert_eq!(
            brace, bracket,
            "{op}: brace literal must order like the bracket literal"
        );
        assert_ne!(
            bracket,
            vec![1, 2, 3, 4, 5],
            "{op}: control — a real KNN order is not id order"
        );
    }
    // …and in the select list / WHERE too (cast path).
    let d = db
        .query("SELECT embedding <=> '{0.1,0.2,0.3}' FROM docs WHERE id = 1", &[])
        .unwrap();
    assert!(
        matches!(d[0].values[0], Value::Float4(_)),
        "brace literal must evaluate in the select list"
    );
}

/// A genuinely invalid literal must ERROR on every operator, under LIMIT and without.
#[test]
fn an_invalid_vector_literal_is_an_error_on_every_operator() {
    let db = db_with_docs();
    for op in ["<=>", "<->", "<#>"] {
        for tail in ["LIMIT 3", ""] {
            let r = db.query(
                &format!("SELECT id FROM docs ORDER BY embedding {op} 'not-a-vector' {tail}"),
                &[],
            );
            assert!(r.is_err(), "{op} {tail}: an invalid literal must error, got {r:?}");
        }
    }
}

/// Issue 7: AS OF is optional and defaults to NOW (the documented grammar).
#[test]
fn create_database_branch_without_as_of_defaults_to_now() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
    db.execute("CREATE DATABASE BRANCH nb FROM main")
        .expect("AS OF must default to NOW");
    let names: Vec<String> = db
        .query("SELECT branch_name FROM pg_database_branches()", &[])
        .unwrap()
        .iter()
        .map(|r| format!("{:?}", r.values[0]))
        .collect();
    assert!(names.iter().any(|n| n.contains("nb")), "branch must exist: {names:?}");
    // A present-but-empty clause is still an error.
    assert!(db.execute("CREATE DATABASE BRANCH nb2 FROM main AS OF").is_err());
}

/// Issue 5: the DECIMAL→NUMERIC preprocessor was O(n²) (a full `to_uppercase()` of the remaining
/// statement at every position) — ~60 s for a 1.45 MB multi-row vector INSERT. Linear now.
#[test]
fn decimal_preprocessor_is_linear_and_still_correct() {
    use heliosdb_nano::sql::Parser;
    // Correctness: word-boundary rewrite, case-insensitive, never inside quotes.
    assert_eq!(
        Parser::preprocess_decimal_to_numeric("CREATE TABLE t (a DECIMAL(10,2), b decimal)"),
        "CREATE TABLE t (a NUMERIC(10,2), b NUMERIC)"
    );
    assert_eq!(
        Parser::preprocess_decimal_to_numeric("SELECT 'decimal', \"DECIMAL\", mydecimal, decimals FROM t"),
        "SELECT 'decimal', \"DECIMAL\", mydecimal, decimals FROM t"
    );
    assert_eq!(
        Parser::preprocess_decimal_to_numeric("INSERT INTO t VALUES ('it''s DECIMAL')"),
        "INSERT INTO t VALUES ('it''s DECIMAL')"
    );
    // Speed: 1.5 MB with no DECIMAL at all — the common INSERT case — must be effectively free.
    let big = format!(
        "INSERT INTO d VALUES {}",
        (0..500)
            .map(|i| format!(
                "({i}, '[{}]')",
                (0..384)
                    .map(|k| format!("{:.4}", ((i * 31 + k) as f32).sin()))
                    .collect::<Vec<_>>()
                    .join(",")
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    assert!(big.len() > 1_000_000, "fixture must be > 1 MB, got {}", big.len());
    let t = std::time::Instant::now();
    let out = Parser::preprocess_decimal_to_numeric(&big);
    let el = t.elapsed();
    assert_eq!(out, big);
    assert!(
        el.as_millis() < 2_000,
        "1.5 MB took {el:?}; the quadratic version takes ~60 s — this must be linear"
    );
}

/// Cosmetic: an unaliased aggregate is named after the function, as in PostgreSQL.
#[test]
fn unaliased_count_column_is_named_count() {
    let db = db_with_docs();
    let (_, cols) = db.query_with_columns("SELECT count(*) FROM docs").unwrap();
    assert_eq!(cols, vec!["count".to_string()], "v4.29.0 named it count(...)");
}

/// Issue 12: a timestamp with no snapshot at or before it must say so, with the interpreted instant
/// and the available range — not the bare "No snapshot found for timestamp".
#[test]
fn as_of_timestamp_miss_is_diagnosable() {
    let dir = tempfile::tempdir().unwrap();
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
    let r = db.execute("CREATE DATABASE BRANCH audit FROM main AS OF TIMESTAMP '2001-01-01 00:00:00'");
    let msg = format!("{:?}", r.err().expect("a timestamp before any snapshot must miss"));
    assert!(msg.contains("at or before"), "message must state the semantics: {msg}");
    assert!(
        msg.contains("2001-01-01T00:00:00Z"),
        "message must show the interpreted instant: {msg}"
    );
    // Fractional seconds and an explicit offset must parse (they errored with 'Invalid timestamp format').
    let r2 = db.execute("CREATE DATABASE BRANCH audit2 FROM main AS OF TIMESTAMP '2001-01-01 00:00:00.500 -03:00'");
    let m2 = format!("{:?}", r2.err().unwrap());
    assert!(
        !m2.contains("Invalid timestamp format"),
        "offset/fraction must parse: {m2}"
    );
}
