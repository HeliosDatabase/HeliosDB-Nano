//! An aggregate's `FILTER (WHERE …)` (4.42.0 pre-release review, item P1).
//!
//! The planner dropped the clause without a word, so `sum(v) FILTER (WHERE id
//! <= 1)` summed every row, on every route: the review's repro returned
//! (60, 3, 60) where PostgreSQL returns (10, 2, 60). Every assertion here
//! compares a FILTERed aggregate with a value computed independently, on a
//! shape whose unFILTERed answer differs, so a path that ignores the clause
//! fails. Each storage fast path is driven with the shape that takes it when
//! the clause is absent (the unFILTERed baseline is asserted alongside), and
//! the aggregate pushdowns' rayon split is engaged at a handful of rows.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::sql::planner::{
    AGGREGATE_IN_FILTER, FILTER_ON_NON_AGGREGATE_PREFIX, FILTER_ON_NON_AGGREGATE_WINDOW, WINDOW_IN_FILTER,
};
use heliosdb_nano::{EmbeddedDatabase, Value};

static PARALLEL_THRESHOLD: std::sync::Once = std::sync::Once::new();

/// The parallel-aggregation threshold is latched process-wide on first use,
/// so it is set once, before any test here opens a database.
fn db() -> EmbeddedDatabase {
    PARALLEL_THRESHOLD.call_once(|| std::env::set_var("HELIOS_AGG_PARALLEL_THRESHOLD", "4"));
    EmbeddedDatabase::new_in_memory().expect("database")
}

/// A cell in a family-neutral spelling (an integer is its digits whatever
/// its width), for comparing the two executor families' rows.
fn canon(v: &Value) -> String {
    match v {
        Value::Int2(i) => i.to_string(),
        Value::Int4(i) => i.to_string(),
        Value::Int8(i) => i.to_string(),
        other => format!("{other:?}"),
    }
}

/// Run `sql` on the text family (`query`) and on the params family
/// (`query_params`, no binds), require the same rows from both, return them.
fn both(db: &EmbeddedDatabase, sql: &str) -> Vec<Vec<Value>> {
    let text: Vec<Vec<Value>> = db
        .query(sql, &[])
        .unwrap_or_else(|e| panic!("query {sql}: {e}"))
        .into_iter()
        .map(|t| t.values)
        .collect();
    let params: Vec<Vec<Value>> = db
        .query_params(sql, &[])
        .unwrap_or_else(|e| panic!("query_params {sql}: {e}"))
        .into_iter()
        .map(|t| t.values)
        .collect();
    let spell =
        |rows: &[Vec<Value>]| -> Vec<Vec<String>> { rows.iter().map(|r| r.iter().map(canon).collect()).collect() };
    assert_eq!(
        spell(&text),
        spell(&params),
        "{sql}: the two executor families disagree"
    );
    text
}

fn with_params(db: &EmbeddedDatabase, sql: &str, params: &[Value]) -> Vec<Vec<Value>> {
    db.query_params(sql, params)
        .unwrap_or_else(|e| panic!("query_params {sql}: {e}"))
        .into_iter()
        .map(|t| t.values)
        .collect()
}

/// An integer cell, `None` for NULL.
fn int(v: &Value) -> Option<i64> {
    match v {
        Value::Null => None,
        Value::Int2(i) => Some(i64::from(*i)),
        Value::Int4(i) => Some(i64::from(*i)),
        Value::Int8(i) => Some(*i),
        Value::Numeric(s) => Some(
            s.parse()
                .unwrap_or_else(|_| panic!("integral numeric expected, got {s}")),
        ),
        other => panic!("integer expected, got {other:?}"),
    }
}

fn float(v: &Value) -> Option<f64> {
    match v {
        Value::Null => None,
        Value::Float4(f) => Some(f64::from(*f)),
        Value::Float8(f) => Some(*f),
        Value::Numeric(s) => Some(s.parse().unwrap_or_else(|_| panic!("numeric expected, got {s}"))),
        other => int(other).map(|i| i as f64),
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) | Value::Json(s) => s.clone(),
        other => panic!("text expected, got {other:?}"),
    }
}

fn ints(row: &[Value]) -> Vec<Option<i64>> {
    row.iter().map(int).collect()
}

/// `sql` must be refused on both executor families with `needle` in the message.
fn assert_refused(db: &EmbeddedDatabase, sql: &str, needle: &str) {
    let text = db.query(sql, &[]).map(|_| ()).expect_err(sql).to_string();
    assert!(text.contains(needle), "query {sql}: {text}");
    let params = db.query_params(sql, &[]).map(|_| ()).expect_err(sql).to_string();
    assert!(params.contains(needle), "query_params {sql}: {params}");
}

/// `s`: five rows, one with a NULL `v`, three groups.
fn seed_small(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE s (id INT PRIMARY KEY, g TEXT, v INT)")
        .unwrap();
    db.execute("INSERT INTO s VALUES (1, 'a', 10), (2, 'a', 20), (3, 'b', 30), (4, 'b', NULL), (5, 'c', 50)")
        .unwrap();
}

#[test]
fn review_repro_sum_and_count_honour_filter() {
    let db = db();
    db.execute("CREATE TABLE f (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO f VALUES (1, 10), (2, 20), (3, 30)").unwrap();

    let got = both(
        &db,
        "SELECT sum(v) FILTER (WHERE id <= 1), count(*) FILTER (WHERE id > 1), sum(v) FROM f",
    );
    assert_eq!(ints(&got[0]), vec![Some(10), Some(2), Some(60)]);

    // The repro's bound form: the predicate's `$1` reads the bind vector.
    let got = with_params(
        &db,
        "SELECT sum(v) FILTER (WHERE id <= $1), count(*) FILTER (WHERE id > $1), sum(v) FROM f",
        &[Value::Int4(1)],
    );
    assert_eq!(ints(&got[0]), vec![Some(10), Some(2), Some(60)]);
    let got = with_params(&db, "SELECT sum(v) FILTER (WHERE id <= $1) FROM f", &[Value::Int4(2)]);
    assert_eq!(ints(&got[0]), vec![Some(30)]);
}

#[test]
fn every_aggregate_honours_filter_with_nulls() {
    let db = db();
    seed_small(&db);
    let got = both(
        &db,
        "SELECT count(*) FILTER (WHERE id > 1), count(v) FILTER (WHERE id > 1), \
                count(*) FILTER (WHERE v > 10), sum(v) FILTER (WHERE v > 10), \
                min(v) FILTER (WHERE id >= 2), max(v) FILTER (WHERE id < 5), \
                sum(v) FILTER (WHERE id = 4), count(*) FILTER (WHERE false), sum(v) FILTER (WHERE false) \
         FROM s",
    );
    // count(*) counts the NULL-v row the predicate accepts, count(v) does not;
    // a predicate that is NULL for a row (`v > 10` with v NULL) rejects it.
    assert_eq!(
        ints(&got[0]),
        vec![
            Some(4),
            Some(3),
            Some(3),
            Some(100),
            Some(20),
            Some(30),
            None,
            Some(0),
            None
        ]
    );

    let got = both(&db, "SELECT avg(v) FILTER (WHERE g = 'a'), avg(v) FROM s");
    assert_eq!(float(&got[0][0]), Some(15.0));
    assert_eq!(float(&got[0][1]), Some(27.5));

    // Inside an expression, and the same aggregate with and without the clause.
    let got = both(
        &db,
        "SELECT sum(v) FILTER (WHERE g = 'a') * 2 + count(*) FILTER (WHERE g = 'b'), \
                sum(v), sum(v) FILTER (WHERE id = 1) FROM s",
    );
    assert_eq!(ints(&got[0]), vec![Some(62), Some(110), Some(10)]);

    // Over an empty input: count 0, the others NULL.
    db.execute("CREATE TABLE e (id INT PRIMARY KEY, v INT)").unwrap();
    let got = both(
        &db,
        "SELECT count(*) FILTER (WHERE v > 0), sum(v) FILTER (WHERE v > 0), max(v) FILTER (WHERE v > 0) FROM e",
    );
    assert_eq!(ints(&got[0]), vec![Some(0), None, None]);
}

#[test]
fn filter_with_group_by_and_having() {
    let db = db();
    seed_small(&db);
    let got = both(
        &db,
        "SELECT g, count(*) FILTER (WHERE v >= 20), sum(v) FILTER (WHERE v >= 20), count(*) \
         FROM s GROUP BY g ORDER BY g",
    );
    let got: Vec<(String, Vec<Option<i64>>)> = got.iter().map(|r| (text(&r[0]), ints(&r[1..]))).collect();
    assert_eq!(
        got,
        vec![
            ("a".to_string(), vec![Some(1), Some(20), Some(2)]),
            ("b".to_string(), vec![Some(1), Some(30), Some(2)]),
            ("c".to_string(), vec![Some(1), Some(50), Some(1)]),
        ]
    );

    // An aggregate only HAVING names, FILTERed: `count(*) = 0` alone keeps no group.
    let got = both(
        &db,
        "SELECT g FROM s GROUP BY g HAVING count(*) FILTER (WHERE v < 20) = 0 ORDER BY g",
    );
    let got: Vec<String> = got.iter().map(|r| text(&r[0])).collect();
    assert_eq!(got, vec!["b".to_string(), "c".to_string()]);

    // Bound predicate, grouped.
    let got = with_params(
        &db,
        "SELECT g, sum(v) FILTER (WHERE v > $1) FROM s GROUP BY g ORDER BY g",
        &[Value::Int4(15)],
    );
    let got: Vec<Option<i64>> = got.iter().map(|r| int(&r[1])).collect();
    assert_eq!(got, vec![Some(20), Some(30), Some(50)]);
}

#[test]
fn filter_with_distinct_and_the_collecting_aggregates() {
    let db = db();
    db.execute("CREATE TABLE d (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO d VALUES (1, 5), (2, 5), (3, 7), (4, NULL), (5, 9), (6, 9)")
        .unwrap();
    let got = both(
        &db,
        "SELECT count(DISTINCT v) FILTER (WHERE id <= 4), sum(DISTINCT v) FILTER (WHERE id >= 3), \
                count(DISTINCT v) FROM d",
    );
    assert_eq!(ints(&got[0]), vec![Some(2), Some(16), Some(3)]);
    let got = both(
        &db,
        "SELECT avg(DISTINCT v) FILTER (WHERE id >= 3), avg(DISTINCT v) FROM d",
    );
    assert_eq!(float(&got[0][0]), Some(8.0));
    assert_eq!(float(&got[0][1]), Some(7.0));

    seed_small(&db);
    let got = both(
        &db,
        "SELECT g, count(DISTINCT v) FILTER (WHERE id <> 1) FROM s GROUP BY g ORDER BY g",
    );
    let got: Vec<Option<i64>> = got.iter().map(|r| int(&r[1])).collect();
    assert_eq!(got, vec![Some(1), Some(1), Some(1)]);

    let got = both(
        &db,
        "SELECT string_agg(DISTINCT g, ',') FILTER (WHERE v >= 30), array_agg(DISTINCT id) FILTER (WHERE v >= 30), \
                json_agg(id) FILTER (WHERE id = 5), group_concat(DISTINCT g) FILTER (WHERE id <= 2) FROM s",
    );
    assert_eq!(text(&got[0][0]), "b,c");
    assert_eq!(got[0][1], Value::Array(vec![Value::Int4(3), Value::Int4(5)]));
    assert_eq!(text(&got[0][2]), "[5]");
    assert_eq!(text(&got[0][3]), "a");
}

#[test]
fn filter_on_aggregate_window_calls() {
    let db = db();
    seed_small(&db);
    let got = both(
        &db,
        "SELECT id, sum(v) FILTER (WHERE id <> 2) OVER (ORDER BY id), \
                count(*) FILTER (WHERE id > 2) OVER (ORDER BY id), \
                count(*) FILTER (WHERE v IS NOT NULL) OVER () \
         FROM s ORDER BY id",
    );
    let sums: Vec<Option<f64>> = got.iter().map(|r| float(&r[1])).collect();
    assert_eq!(sums, vec![Some(10.0), Some(10.0), Some(40.0), Some(40.0), Some(90.0)]);
    let running: Vec<Option<i64>> = got.iter().map(|r| int(&r[2])).collect();
    assert_eq!(running, vec![Some(0), Some(0), Some(1), Some(2), Some(3)]);
    let whole: Vec<Option<i64>> = got.iter().map(|r| int(&r[3])).collect();
    assert_eq!(whole, vec![Some(4); 5]);
}

/// Two window calls with one output name (`count` both) are two columns. The
/// projection read each window result by its name, so the second read the
/// first's values — with FILTER or without (the test above failed this way).
/// A window call named like an input column read that column too.
#[test]
fn window_calls_sharing_a_name_are_distinct_columns() {
    let db = db();
    seed_small(&db);
    let got = both(
        &db,
        "SELECT id, count(*) OVER (ORDER BY id), count(*) OVER () FROM s ORDER BY id",
    );
    let running: Vec<Option<i64>> = got.iter().map(|r| int(&r[1])).collect();
    assert_eq!(running, vec![Some(1), Some(2), Some(3), Some(4), Some(5)]);
    let whole: Vec<Option<i64>> = got.iter().map(|r| int(&r[2])).collect();
    assert_eq!(whole, vec![Some(5); 5]);

    let got = both(&db, "SELECT id, sum(v) OVER () AS id FROM s");
    let mut ids: Vec<Option<i64>> = got.iter().map(|r| int(&r[0])).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![Some(1), Some(2), Some(3), Some(4), Some(5)]);
    let sums: Vec<Option<f64>> = got.iter().map(|r| float(&r[1])).collect();
    assert_eq!(sums, vec![Some(110.0); 5]);
}

#[test]
fn filter_that_cannot_be_honoured_is_refused() {
    let db = db();
    seed_small(&db);
    assert_refused(
        &db,
        "SELECT row_number() FILTER (WHERE v > 0) OVER () FROM s",
        FILTER_ON_NON_AGGREGATE_WINDOW,
    );
    assert_refused(
        &db,
        "SELECT upper(g) FILTER (WHERE v > 0) FROM s",
        FILTER_ON_NON_AGGREGATE_PREFIX,
    );
    assert_refused(
        &db,
        "SELECT sum(v) FILTER (WHERE count(*) > 1) FROM s",
        AGGREGATE_IN_FILTER,
    );
    assert_refused(
        &db,
        "SELECT sum(v) FILTER (WHERE row_number() OVER () > 1) FROM s",
        WINDOW_IN_FILTER,
    );

    // An unknown column in the predicate is refused at plan time, so even an
    // empty input (where nothing would ever evaluate it) does not hide it.
    db.execute("CREATE TABLE e (id INT PRIMARY KEY, v INT)").unwrap();
    assert_refused(&db, "SELECT count(*) FILTER (WHERE nosuch > 0) FROM e", "nosuch");
}

/// Row `id` of the 40-row fast-path tables: `g` = `g{id % 3}`, `v` = `id * 10`,
/// NULL when `id` is a multiple of 7.
fn big_v(id: i64) -> Option<i64> {
    (id % 7 != 0).then_some(id * 10)
}

fn seed_big(db: &EmbeddedDatabase, table: &str, columnar: bool) {
    let storage = if columnar { " STORAGE COLUMNAR" } else { "" };
    db.execute(&format!(
        "CREATE TABLE {table} (id INT PRIMARY KEY, g TEXT{storage}, v INT{storage})"
    ))
    .unwrap();
    let values: Vec<String> = (1..=40_i64)
        .map(|id| {
            let v = big_v(id).map_or_else(|| "NULL".to_string(), |v| v.to_string());
            format!("({id}, 'g{}', {v})", id % 3)
        })
        .collect();
    db.execute(&format!("INSERT INTO {table} VALUES {}", values.join(", ")))
        .unwrap();
}

/// `count(*)` over the ids `keep` admits.
fn big_count(keep: impl Fn(i64) -> bool) -> i64 {
    (1..=40_i64).filter(|&id| keep(id)).count() as i64
}

/// `count(*) FILTER (WHERE v > 200)` over the ids `keep` admits.
fn big_count_over_200(keep: impl Fn(i64) -> bool) -> i64 {
    big_count(|id| keep(id) && big_v(id).is_some_and(|v| v > 200))
}

/// The non-NULL `v` of the ids `keep` admits.
fn big_vs(keep: impl Fn(i64) -> bool) -> Vec<i64> {
    (1..=40_i64).filter(|&id| keep(id)).filter_map(big_v).collect()
}

/// Every aggregate fast path, on a row-store and on a columnar table: the
/// shape that takes the path without FILTER, then the same shape with it.
fn check_fast_paths(db: &EmbeddedDatabase, t: &str) {
    let over_200 = big_count_over_200(|_| true);
    assert_eq!(over_200, 17);

    // The storage row counter, and the text-level `SELECT COUNT(...) FROM t`
    // shortcut ahead of the planner (both spellings reach both).
    assert_eq!(ints(&both(db, &format!("SELECT count(*) FROM {t}"))[0]), vec![Some(40)]);
    for sql in [
        format!("SELECT count(*) FILTER (WHERE v > 200) FROM {t}"),
        format!("SELECT COUNT(*) FILTER (WHERE v > 200) FROM {t}"),
        format!("SELECT COUNT(id) FILTER (WHERE v > 200) FROM {t}"),
    ] {
        assert_eq!(ints(&both(db, &sql)[0]), vec![Some(over_200)], "{sql}");
    }

    // The PK-cardinality shortcut: count / count DISTINCT of the identity key.
    assert_eq!(
        ints(&both(db, &format!("SELECT count(id) FROM {t}"))[0]),
        vec![Some(40)]
    );
    for sql in [
        format!("SELECT count(id) FILTER (WHERE v > 200) FROM {t}"),
        format!("SELECT count(DISTINCT id) FILTER (WHERE v > 200) FROM {t}"),
        format!("SELECT count(id + 0) FILTER (WHERE v > 200) FROM {t}"),
    ] {
        assert_eq!(ints(&both(db, &sql)[0]), vec![Some(over_200)], "{sql}");
    }

    // count(*) under a WHERE: PK point lookup, PK IN list, PK range (both
    // spellings), and scan + predicate.
    let cases: [(&str, i64, i64); 5] = [
        ("id = 10", 1, 0),
        ("id IN (1, 25, 26)", 3, 2),
        ("id BETWEEN 1 AND 30", 30, big_count_over_200(|id| id <= 30)),
        ("id >= 1 AND id <= 30", 30, big_count_over_200(|id| id <= 30)),
        (
            "g = 'g1'",
            big_count(|id| id % 3 == 1),
            big_count_over_200(|id| id % 3 == 1),
        ),
    ];
    for (predicate, all, filtered) in cases {
        let plain = format!("SELECT count(*) FROM {t} WHERE {predicate}");
        assert_eq!(ints(&both(db, &plain)[0]), vec![Some(all)], "{plain}");
        let sql = format!("SELECT count(*) FILTER (WHERE v > 200) FROM {t} WHERE {predicate}");
        assert_eq!(ints(&both(db, &sql)[0]), vec![Some(filtered)], "{sql}");
    }

    // Ungrouped pushdown (row store: primitive COUNT/SUM/AVG; columnar: batch).
    let over: Vec<i64> = big_vs(|_| true).into_iter().filter(|&v| v > 200).collect();
    let sql = format!(
        "SELECT count(*) FILTER (WHERE v > 200), sum(v) FILTER (WHERE v > 200), \
                min(v) FILTER (WHERE id > 5), max(v) FILTER (WHERE id < 30), count(v) FILTER (WHERE id > 20), \
                sum(v), count(v) \
         FROM {t}"
    );
    let got = both(db, &sql);
    assert_eq!(
        ints(&got[0]),
        vec![
            Some(over.len() as i64),
            Some(over.iter().sum::<i64>()),
            big_vs(|id| id > 5).into_iter().min(),
            big_vs(|id| id < 30).into_iter().max(),
            Some(big_vs(|id| id > 20).len() as i64),
            Some(big_vs(|_| true).iter().sum::<i64>()),
            Some(big_vs(|_| true).len() as i64),
        ],
        "{sql}"
    );
    let low = big_vs(|id| id <= 10);
    let got = both(db, &format!("SELECT avg(v) FILTER (WHERE id <= 10), avg(v) FROM {t}"));
    let expected_low = low.iter().sum::<i64>() as f64 / low.len() as f64;
    assert!((float(&got[0][0]).unwrap() - expected_low).abs() < 1e-9, "{got:?}");

    // Grouped pushdown (row store: text-group count/sum; columnar: grouped
    // count/sum specialisation), without and with a pushed-down WHERE.
    let shapes: [(&str, fn(i64) -> bool); 2] = [("", |_| true), (" WHERE id > 10", |id| id > 10)];
    for (predicate, keep) in shapes {
        let sql = format!(
            "SELECT g, count(*) FILTER (WHERE v > 200), sum(v) FILTER (WHERE v > 200), \
                    min(v) FILTER (WHERE v > 200), count(*) \
             FROM {t}{predicate} GROUP BY g ORDER BY g"
        );
        let got = both(db, &sql);
        assert_eq!(got.len(), 3, "{sql}");
        for (g, row) in got.iter().enumerate() {
            let g = g as i64;
            assert_eq!(text(&row[0]), format!("g{g}"), "{sql}");
            let in_group: Vec<i64> = (1..=40_i64).filter(|&id| keep(id) && id % 3 == g).collect();
            let over: Vec<i64> = in_group
                .iter()
                .filter_map(|&id| big_v(id))
                .filter(|&v| v > 200)
                .collect();
            assert_eq!(
                ints(&row[1..]),
                vec![
                    Some(over.len() as i64),
                    if over.is_empty() {
                        None
                    } else {
                        Some(over.iter().sum::<i64>())
                    },
                    over.iter().copied().min(),
                    Some(in_group.len() as i64),
                ],
                "{sql} group g{g}"
            );
        }
    }

    // Inside a transaction (the slow read paths), seeing its own staged row.
    db.execute("BEGIN").unwrap();
    db.execute(&format!("INSERT INTO {t} VALUES (41, 'g2', 410)")).unwrap();
    let got = ints(
        &db.query(
            &format!("SELECT count(*) FILTER (WHERE v > 200), sum(v) FILTER (WHERE id > 39), count(*) FROM {t}"),
            &[],
        )
        .unwrap()[0]
            .values,
    );
    assert_eq!(got, vec![Some(over_200 + 1), Some(810), Some(41)]);
    db.execute("ROLLBACK").unwrap();
}

#[test]
fn filter_on_every_fast_path_row_store() {
    let db = db();
    seed_big(&db, "r", false);
    check_fast_paths(&db, "r");
}

#[test]
fn filter_on_every_fast_path_columnar() {
    let db = db();
    seed_big(&db, "c", true);
    check_fast_paths(&db, "c");
}

#[test]
fn filter_through_insert_select_on_both_families_and_a_materialized_view() {
    let db = db();
    seed_small(&db);
    db.execute("CREATE TABLE sink (g TEXT, n BIGINT, s BIGINT)").unwrap();
    let n = db
        .execute(
            "INSERT INTO sink SELECT g, count(*) FILTER (WHERE v >= 20), sum(v) FILTER (WHERE v >= 20) \
             FROM s GROUP BY g",
        )
        .unwrap();
    assert_eq!(n, 3);
    let n = db
        .execute_params(
            "INSERT INTO sink SELECT g, count(*) FILTER (WHERE v >= $1), sum(v) FILTER (WHERE v >= $1) \
             FROM s GROUP BY g",
            &[Value::Int4(30)],
        )
        .unwrap();
    assert_eq!(n, 3);
    let got = both(&db, "SELECT g, n, s FROM sink ORDER BY g, n");
    let got: Vec<(String, Vec<Option<i64>>)> = got.iter().map(|r| (text(&r[0]), ints(&r[1..]))).collect();
    assert_eq!(
        got,
        vec![
            ("a".to_string(), vec![Some(0), None]),
            ("a".to_string(), vec![Some(1), Some(20)]),
            ("b".to_string(), vec![Some(1), Some(30)]),
            ("b".to_string(), vec![Some(1), Some(30)]),
            ("c".to_string(), vec![Some(1), Some(50)]),
            ("c".to_string(), vec![Some(1), Some(50)]),
        ]
    );

    // A materialized view stores its plan; REFRESH re-runs the stored plan.
    db.execute("CREATE MATERIALIZED VIEW mvf AS SELECT g, count(*) FILTER (WHERE v >= 20) AS n FROM s GROUP BY g")
        .unwrap();
    let read = |db: &EmbeddedDatabase| -> Vec<(String, Option<i64>)> {
        both(db, "SELECT g, n FROM mvf ORDER BY g")
            .iter()
            .map(|r| (text(&r[0]), int(&r[1])))
            .collect()
    };
    let expect = |a: i64| {
        vec![
            ("a".to_string(), Some(a)),
            ("b".to_string(), Some(1)),
            ("c".to_string(), Some(1)),
        ]
    };
    assert_eq!(read(&db), expect(1));
    db.execute("INSERT INTO s VALUES (6, 'a', 60), (7, 'a', 5)").unwrap();
    db.execute("REFRESH MATERIALIZED VIEW mvf").unwrap();
    assert_eq!(read(&db), expect(2));
}
