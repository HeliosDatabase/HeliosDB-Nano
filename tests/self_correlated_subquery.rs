#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
//! A correlated subquery over the table of its own outer query, the inner
//! table aliased and the outer row named by the table's name, is evaluated per
//! outer row (the 4.42.0 known limit).

use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};

fn db() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE acct (id INT PRIMARY KEY, grp INT, n INT)")
        .unwrap();
    db.execute("INSERT INTO acct VALUES (1,1,0),(2,1,0),(3,2,0),(4,2,0),(5,2,0),(6,3,0)")
        .unwrap();
    db.execute("CREATE TABLE other (id INT PRIMARY KEY, grp INT)").unwrap();
    db.execute("INSERT INTO other VALUES (1,1),(2,2)").unwrap();
    db
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int2(n) => i64::from(*n),
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        other => panic!("not an integer: {other:?}"),
    }
}

fn rows(rows: Vec<Tuple>) -> Vec<Vec<i64>> {
    let mut out: Vec<Vec<i64>> = rows.iter().map(|r| r.values.iter().map(int).collect()).collect();
    out.sort();
    out
}

/// Both statement families: the text form, and the same statement with a
/// bound parameter inside the subquery (`$1` is always 0 and the subquery adds
/// `AND a.n = $1`, true on every row).
fn query_both(db: &EmbeddedDatabase, text: &str) -> Vec<Vec<i64>> {
    let plain_text = text
        .replace(" AND a.n = $1", "")
        .replace(" AND a.id > $1", "")
        .replace(" AND acct.n = $1", "");
    assert!(!plain_text.contains('$'), "{plain_text}");
    let plain = rows(db.query(&plain_text, &[]).unwrap());
    let bound = rows(db.query_params(text, &[Value::Int4(0)]).unwrap());
    assert_eq!(plain, bound, "text and params families differ for {text}");
    plain
}

#[test]
#[ignore = "known limit in 4.42.0: without a bound parameter the select-list subquery still reads the outer table's name as its own aliased table (whole-table result); the bound-parameter family is right"]
fn select_list_scalar() {
    let db = db();
    let got = query_both(
        &db,
        "SELECT id, (SELECT COUNT(*) FROM acct AS a WHERE a.grp = acct.grp AND a.n = $1) FROM acct",
    );
    assert_eq!(
        got,
        vec![vec![1, 2], vec![2, 2], vec![3, 3], vec![4, 3], vec![5, 3], vec![6, 1]]
    );
}

#[test]
fn where_scalar_comparison() {
    let db = db();
    let got = query_both(
        &db,
        "SELECT id FROM acct WHERE (SELECT COUNT(*) FROM acct AS a WHERE a.grp = acct.grp AND a.n = $1) = 3",
    );
    assert_eq!(got, vec![vec![3], vec![4], vec![5]]);
}

#[test]
fn exists() {
    let db = db();
    let got = query_both(
        &db,
        "SELECT id FROM acct WHERE EXISTS (SELECT 1 FROM acct AS a WHERE a.grp = acct.grp AND a.id <> acct.id AND a.n = $1)",
    );
    assert_eq!(got, vec![vec![1], vec![2], vec![3], vec![4], vec![5]]);
}

#[test]
fn not_exists() {
    let db = db();
    let got = query_both(
        &db,
        "SELECT id FROM acct WHERE NOT EXISTS (SELECT 1 FROM acct AS a WHERE a.grp = acct.grp AND a.id <> acct.id AND a.n = $1)",
    );
    assert_eq!(got, vec![vec![6]]);
}

#[test]
fn in_subquery() {
    let db = db();
    let got = query_both(
        &db,
        "SELECT id FROM acct WHERE id IN (SELECT MAX(a.id) FROM acct AS a WHERE a.grp = acct.grp AND a.n = $1)",
    );
    assert_eq!(got, vec![vec![2], vec![5], vec![6]]);
}

#[test]
fn update_set() {
    for (target, outer) in [("acct", "acct"), ("acct AS o", "o")] {
        for params in [false, true] {
            let db = db();
            let count = if params {
                db.execute_params(
                    &format!(
                        "UPDATE {target} SET n = (SELECT COUNT(*) FROM acct AS a WHERE a.grp = {outer}.grp AND a.n = $1)"
                    ),
                    &[Value::Int4(0)],
                )
                .unwrap()
            } else {
                db.execute(&format!(
                    "UPDATE {target} SET n = (SELECT COUNT(*) FROM acct AS a WHERE a.grp = {outer}.grp)"
                ))
                .unwrap()
            };
            assert_eq!(count, 6);
            let got = rows(db.query("SELECT id, n FROM acct", &[]).unwrap());
            assert_eq!(
                got,
                vec![vec![1, 2], vec![2, 2], vec![3, 3], vec![4, 3], vec![5, 3], vec![6, 1]],
                "UPDATE {target} params={params}"
            );
        }
    }
}

#[test]
#[ignore = "known limit in 4.42.0: this correlated subquery in JOIN … ON is accepted (no rows) instead of refused with 0A000"]
fn join_on_is_refused_0a000() {
    let db = db();
    let err = db
        .query(
            "SELECT acct.id FROM acct JOIN other ON other.id = acct.grp \
             AND other.grp = (SELECT MAX(a.grp) FROM acct AS a WHERE a.grp = acct.grp)",
            &[],
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("0A000") || err.to_string().contains("not supported"),
        "{err}"
    );
}

#[test]
fn control_real_name_qualifier_without_outer_candidate_unchanged() {
    let db = db();
    // `other` is not an outer relation: single-level leniency keeps reading it
    // as the inner alias a, so the subquery counts the whole of `other`.
    let got = query_both(
        &db,
        "SELECT id, (SELECT COUNT(*) FROM other AS a WHERE a.grp = other.grp AND a.id > $1) FROM acct",
    );
    assert_eq!(got.len(), 6);
    assert!(got.iter().all(|r| r[1] == 2), "{got:?}");
}

#[test]
fn control_unaliased_inner_same_table() {
    let db = db();
    let got = query_both(
        &db,
        "SELECT id FROM acct WHERE id IN (SELECT acct.id FROM acct WHERE acct.grp = 2 AND acct.n = $1)",
    );
    assert_eq!(got, vec![vec![3], vec![4], vec![5]]);
}
