//! `UPDATE t SET c = (SELECT … FROM t WHERE t.x = …)`: whose column is `t.x`?
//!
//! A name belongs to the nearest scope that has it. The subquery reads `t`
//! itself, so `t.x` is the subquery's column and the statement is not
//! correlated: every row gets the same value. An alias hides the table's name
//! (`FROM t AS a`); there `t.x` can only be the row being updated. (That last
//! shape, the updated table read again under an alias and correlated with
//! itself, is the known limit of 4.42.0 and is not pinned here.)
//!
//! The row binder of `SET` subqueries went by the name alone. Once it walked
//! below an aggregate (4.42.0 development) it bound the subquery's own column
//! to the row being updated: `SET n = (SELECT COUNT(*) FROM acct WHERE
//! acct.grp = 2)` wrote the size of the table to the rows of group 2 and zero to
//! the rest, where 4.41.1 wrote 3 to every row. The same binder had always
//! got the shape without an aggregate wrong.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

fn int(value: &Value) -> Option<i64> {
    match value {
        Value::Null => None,
        Value::Int2(n) => Some(i64::from(*n)),
        Value::Int4(n) => Some(i64::from(*n)),
        Value::Int8(n) => Some(*n),
        other => panic!("not an integer: {other:?}"),
    }
}

/// Groups: 1, 1, 2, 2, 2, 3.
fn accounts() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE acct (id INT PRIMARY KEY, grp INT, n INT)")
        .unwrap();
    db.execute("INSERT INTO acct VALUES (1, 1, 0), (2, 1, 0), (3, 2, 0), (4, 2, 0), (5, 2, 0), (6, 3, 0)")
        .unwrap();
    db
}

fn n(db: &EmbeddedDatabase) -> Vec<Option<i64>> {
    db.query("SELECT n FROM acct ORDER BY id", &[])
        .unwrap()
        .iter()
        .map(|row| int(&row.values[0]))
        .collect()
}

fn all(value: i64) -> Vec<Option<i64>> {
    vec![Some(value); 6]
}

#[test]
fn an_aggregate_over_the_updated_table_is_the_same_for_every_row() {
    let db = accounts();
    assert_eq!(
        db.execute("UPDATE acct SET n = (SELECT COUNT(*) FROM acct WHERE acct.grp = 2)")
            .unwrap(),
        6
    );
    assert_eq!(n(&db), all(3));

    db.execute("UPDATE acct SET n = (SELECT MAX(id) FROM acct WHERE acct.grp = 1)")
        .unwrap();
    assert_eq!(n(&db), all(2));

    db.execute("UPDATE acct SET n = (SELECT SUM(id) FROM acct WHERE acct.grp = 3) + 1")
        .unwrap();
    assert_eq!(n(&db), all(7));

    // With an outer filter: the row updated is not in group 2, the count is still 3.
    db.execute("UPDATE acct SET n = (SELECT COUNT(*) FROM acct WHERE acct.grp = 2) WHERE id = 1")
        .unwrap();
    assert_eq!(n(&db).first(), Some(&Some(3)));
}

#[test]
fn the_same_through_bound_parameters() {
    let db = accounts();
    assert_eq!(
        db.execute_params(
            "UPDATE acct SET n = (SELECT COUNT(*) FROM acct WHERE acct.grp = $1)",
            &[Value::Int4(2)],
        )
        .unwrap(),
        6
    );
    assert_eq!(n(&db), all(3));
    db.execute_params(
        "UPDATE acct SET n = (SELECT MAX(id) FROM acct WHERE acct.grp = $1) WHERE id = $2",
        &[Value::Int4(1), Value::Int4(6)],
    )
    .unwrap();
    assert_eq!(n(&db), [Some(3), Some(3), Some(3), Some(3), Some(3), Some(2)]);
}

#[test]
fn a_plain_subquery_over_the_updated_table_is_not_correlated_either() {
    let db = accounts();
    db.execute("UPDATE acct SET n = (SELECT id FROM acct WHERE acct.id = 3)")
        .unwrap();
    assert_eq!(n(&db), all(3));
}

#[test]
fn the_unqualified_spelling_and_an_inner_alias_give_the_same_answer() {
    let db = accounts();
    db.execute("UPDATE acct SET n = (SELECT COUNT(*) FROM acct WHERE grp = 2)")
        .unwrap();
    assert_eq!(n(&db), all(3));
    db.execute("UPDATE acct SET n = (SELECT COUNT(*) FROM acct AS a WHERE a.grp = 1)")
        .unwrap();
    assert_eq!(n(&db), all(2));
}

#[test]
fn a_subquery_on_another_table_still_sees_the_row_being_updated() {
    let db = accounts();
    db.execute("CREATE TABLE entries (id INT PRIMARY KEY, acct_id INT, amount INT)")
        .unwrap();
    db.execute("INSERT INTO entries VALUES (1, 1, 10), (2, 1, 5), (3, 3, 7), (4, 6, 1)")
        .unwrap();
    db.execute("UPDATE acct SET n = (SELECT COUNT(*) FROM entries WHERE entries.acct_id = acct.id)")
        .unwrap();
    assert_eq!(n(&db), [Some(2), Some(0), Some(1), Some(0), Some(0), Some(1)]);
    db.execute("UPDATE acct SET n = (SELECT SUM(amount) FROM entries WHERE entries.acct_id = acct.id) WHERE id = 1")
        .unwrap();
    assert_eq!(n(&db).first(), Some(&Some(15)));
    // Under an alias the other table's name is gone from the subquery, and the
    // updated table's name still means the row being updated.
    db.execute("UPDATE acct SET n = (SELECT COUNT(*) FROM entries AS e WHERE e.acct_id = acct.id)")
        .unwrap();
    assert_eq!(n(&db), [Some(2), Some(0), Some(1), Some(0), Some(0), Some(1)]);
}

#[test]
fn inside_a_transaction_the_subquery_counts_staged_rows_once_for_all() {
    let db = accounts();
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO acct VALUES (7, 2, 0)").unwrap();
    db.execute("UPDATE acct SET n = (SELECT COUNT(*) FROM acct WHERE acct.grp = 2)")
        .unwrap();
    let staged: Vec<Option<i64>> = db
        .query("SELECT n FROM acct ORDER BY id", &[])
        .unwrap()
        .iter()
        .map(|row| int(&row.values[0]))
        .collect();
    assert_eq!(staged, vec![Some(4); 7]);
    db.execute("ROLLBACK").unwrap();
    assert_eq!(n(&db), all(0));
}
