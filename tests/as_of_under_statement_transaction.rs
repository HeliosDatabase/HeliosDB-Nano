//! A read of the past is a read of the past wherever it stands.
//!
//! `CREATE TABLE restored AS SELECT * FROM t AS OF …` is the documented way to
//! take back a bad UPDATE. Since statements became atomic the source of
//! `CREATE TABLE … AS SELECT` and of `INSERT … SELECT` runs under the
//! statement's own transaction, and the scan took a transaction before it looked
//! at the clause: it copied today's rows and reported success. An anchor that
//! names no snapshot was accepted the same way. Inside `BEGIN` the clause had
//! always been ignored.
//!
//! The anchor now decides: `AS OF TIMESTAMP / TRANSACTION / SCN` and
//! `VERSIONS BETWEEN` read committed history, under a transaction or not, and
//! nothing staged is merged into them. `AS OF NOW` stays the reader's present,
//! which inside a transaction includes its own writes.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

fn int(value: &Value) -> i64 {
    match value {
        Value::Int2(n) => i64::from(*n),
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        other => panic!("not an integer: {other:?}"),
    }
}

fn balances(db: &EmbeddedDatabase, sql: &str) -> Vec<i64> {
    db.query(sql, &[])
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
        .iter()
        .map(|row| int(&row.values[0]))
        .collect()
}

/// Three accounts, then the bad UPDATE. Returns the database and the change
/// number at which the accounts still had their balances, found with the plain
/// `SELECT … AS OF SCN n` that has always been right.
fn accounts_after_a_bad_update() -> (EmbeddedDatabase, u64) {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE acc (id INT PRIMARY KEY, bal INT)").unwrap();
    db.execute("INSERT INTO acc VALUES (1, 100), (2, 200), (3, 300)")
        .unwrap();
    db.execute("UPDATE acc SET bal = 0").unwrap();
    assert_eq!(balances(&db, "SELECT bal FROM acc ORDER BY id"), [0, 0, 0]);
    let before = (1..=32u64)
        .find(|scn| {
            db.query(&format!("SELECT bal FROM acc AS OF SCN {scn} ORDER BY id"), &[])
                .is_ok_and(|rows| rows.iter().map(|row| int(&row.values[0])).collect::<Vec<_>>() == [100, 200, 300])
        })
        .expect("a change number at which the accounts had their balances");
    (db, before)
}

#[test]
fn create_table_as_copies_the_rows_of_the_anchor() {
    let (db, before) = accounts_after_a_bad_update();
    // The recipe as documented, with a filter (a filtered scan) …
    db.execute(&format!(
        "CREATE TABLE acc_restored AS SELECT * FROM acc AS OF SCN {before} WHERE id IN (1, 2, 3)"
    ))
    .unwrap();
    assert_eq!(
        balances(&db, "SELECT bal FROM acc_restored ORDER BY id"),
        [100, 200, 300]
    );
    // … and without one (a plain scan).
    db.execute(&format!(
        "CREATE TABLE acc_whole AS SELECT * FROM acc AS OF SCN {before}"
    ))
    .unwrap();
    assert_eq!(balances(&db, "SELECT bal FROM acc_whole ORDER BY id"), [100, 200, 300]);
    // A predicate is applied to the historical rows, not to today's.
    db.execute(&format!(
        "CREATE TABLE acc_rich AS SELECT * FROM acc AS OF SCN {before} WHERE bal > 150"
    ))
    .unwrap();
    assert_eq!(balances(&db, "SELECT bal FROM acc_rich ORDER BY id"), [200, 300]);
    // The table read is untouched.
    assert_eq!(balances(&db, "SELECT bal FROM acc ORDER BY id"), [0, 0, 0]);
}

#[test]
fn an_anchor_that_names_no_snapshot_is_an_error_not_todays_rows() {
    let (db, _) = accounts_after_a_bad_update();
    let refused = db
        .execute("CREATE TABLE acc_old AS SELECT * FROM acc AS OF TIMESTAMP '2000-01-01 00:00:00'")
        .expect_err("no snapshot is that old");
    assert!(refused.to_string().contains("No snapshot"), "{refused}");
    let copied = db.query("SELECT bal FROM acc_old", &[]);
    assert!(
        !copied.as_ref().is_ok_and(|rows| !rows.is_empty()),
        "the refused statement must not have copied today's rows: {copied:?}"
    );
}

#[test]
fn insert_select_copies_the_rows_of_the_anchor_in_both_statement_families() {
    let (db, before) = accounts_after_a_bad_update();
    db.execute("CREATE TABLE copy_text (id INT PRIMARY KEY, bal INT)")
        .unwrap();
    db.execute("CREATE TABLE copy_params (id INT PRIMARY KEY, bal INT)")
        .unwrap();
    assert_eq!(
        db.execute(&format!("INSERT INTO copy_text SELECT * FROM acc AS OF SCN {before}"))
            .unwrap(),
        3
    );
    assert_eq!(balances(&db, "SELECT bal FROM copy_text ORDER BY id"), [100, 200, 300]);
    assert_eq!(
        db.execute_params(
            &format!("INSERT INTO copy_params SELECT * FROM acc AS OF SCN {before} WHERE id >= $1"),
            &[Value::Int4(2)],
        )
        .unwrap(),
        2
    );
    assert_eq!(balances(&db, "SELECT bal FROM copy_params ORDER BY id"), [200, 300]);
}

#[test]
fn inside_a_transaction_the_anchor_reads_history_and_now_reads_the_transaction() {
    let (db, before) = accounts_after_a_bad_update();
    db.execute("BEGIN").unwrap();
    db.execute("UPDATE acc SET bal = 7 WHERE id = 1").unwrap();
    // History is committed state: the staged 7 is not part of it.
    assert_eq!(
        balances(&db, &format!("SELECT bal FROM acc AS OF SCN {before} ORDER BY id")),
        [100, 200, 300]
    );
    assert_eq!(
        balances(
            &db,
            &format!("SELECT bal FROM acc AS OF SCN {before} WHERE bal > 150 ORDER BY id")
        ),
        [200, 300]
    );
    // The present of a transaction is its own view.
    assert_eq!(balances(&db, "SELECT bal FROM acc ORDER BY id"), [7, 0, 0]);
    assert_eq!(balances(&db, "SELECT bal FROM acc AS OF NOW ORDER BY id"), [7, 0, 0]);
    db.execute("ROLLBACK").unwrap();
    assert_eq!(balances(&db, "SELECT bal FROM acc ORDER BY id"), [0, 0, 0]);
}

#[test]
fn the_documented_restore_puts_the_balances_back() {
    let (db, before) = accounts_after_a_bad_update();
    db.execute(&format!(
        "CREATE TABLE acc_restored AS SELECT * FROM acc AS OF SCN {before}"
    ))
    .unwrap();
    db.execute("DELETE FROM acc").unwrap();
    assert_eq!(db.execute("INSERT INTO acc SELECT * FROM acc_restored").unwrap(), 3);
    assert_eq!(balances(&db, "SELECT bal FROM acc ORDER BY id"), [100, 200, 300]);
    // The present after the restore is the restored rows, with and without NOW.
    assert_eq!(
        balances(&db, "SELECT bal FROM acc AS OF NOW ORDER BY id"),
        [100, 200, 300]
    );
}
