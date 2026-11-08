//! A REPEATABLE READ transaction reads its snapshot, rows deleted after it
//! included (4.42.0 pre-release review, item F7).
//!
//! The transaction's snapshot scan took its rows from the current `data:` keys,
//! so a row another session deleted (or truncated) after the snapshot was gone
//! from the transaction too: SUM 49 / COUNT 9 instead of 55 / 10, and `WHERE
//! id = 6` empty, for every kind of DELETE and for TRUNCATE. Updates and inserts
//! were hidden already; they stay here as the control. The delete events B4
//! writes say when each row went, which is what the scan reads now.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::{EmbeddedDatabase, Value};

fn int(value: &Value) -> i64 {
    match value {
        Value::Int2(v) => i64::from(*v),
        Value::Int4(v) => i64::from(*v),
        Value::Int8(v) => *v,
        other => panic!("integer expected, got {other:?}"),
    }
}

/// The reader's answers: SUM and COUNT, the point read of id 6, and how many
/// rows a predicate on `v` finds.
fn seen(db: &EmbeddedDatabase, reader: SessionId) -> (i64, i64, Vec<i64>, i64) {
    let totals = db
        .query_in_session(reader, "SELECT SUM(v), COUNT(*) FROM r", &[])
        .unwrap();
    let point = db
        .query_in_session(reader, "SELECT v FROM r WHERE id = 6", &[])
        .unwrap();
    let by_value = db
        .query_in_session(reader, "SELECT COUNT(*) FROM r WHERE v = 6", &[])
        .unwrap();
    (
        int(&totals[0].values[0]),
        int(&totals[0].values[1]),
        point.iter().map(|row| int(&row.values[0])).collect(),
        int(&by_value[0].values[0]),
    )
}

/// Another session's transaction running `sql`, committed.
fn in_a_transaction(db: &EmbeddedDatabase, sql: &str) {
    let writer = db.create_session("writer", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(writer).unwrap();
    db.execute_in_session(writer, sql).unwrap();
    db.commit_transaction_for_session(writer).unwrap();
    db.destroy_session(writer).unwrap();
}

#[test]
fn a_repeatable_read_snapshot_keeps_rows_deleted_after_it() {
    type Writer = fn(&EmbeddedDatabase);
    // (what another session does, the rows a new snapshot counts after it)
    let writers: [(&str, Writer, i64); 8] = [
        (
            "DELETE by key",
            |db| {
                db.execute("DELETE FROM r WHERE id = 6").unwrap();
            },
            9,
        ),
        (
            "DELETE by a bound key",
            |db| {
                db.execute_params("DELETE FROM r WHERE id = $1", &[Value::Int4(6)])
                    .unwrap();
            },
            9,
        ),
        (
            "DELETE by a scan",
            |db| {
                db.execute("DELETE FROM r WHERE v = 6").unwrap();
            },
            9,
        ),
        (
            "DELETE in a transaction",
            |db| in_a_transaction(db, "DELETE FROM r WHERE id = 6"),
            9,
        ),
        (
            "TRUNCATE",
            |db| {
                db.execute("TRUNCATE TABLE r").unwrap();
            },
            0,
        ),
        (
            "TRUNCATE in a transaction",
            |db| in_a_transaction(db, "TRUNCATE TABLE r"),
            0,
        ),
        // Controls, hidden before the fix too.
        (
            "UPDATE",
            |db| {
                db.execute("UPDATE r SET v = 600 WHERE id = 6").unwrap();
            },
            10,
        ),
        (
            "INSERT",
            |db| {
                db.execute("INSERT INTO r VALUES (11, 11)").unwrap();
            },
            11,
        ),
    ];
    for (kind, write, rows_after) in writers {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE r (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("INSERT INTO r SELECT x, x FROM generate_series(1, 10) x")
            .unwrap();
        let reader = db.create_session("reader", IsolationLevel::RepeatableRead).unwrap();
        db.begin_transaction_for_session(reader).unwrap();
        assert_eq!(seen(&db, reader), (55, 10, vec![6], 1), "{kind}: before");

        write(&db);

        assert_eq!(
            seen(&db, reader),
            (55, 10, vec![6], 1),
            "{kind}: the snapshot predates it"
        );
        db.commit_transaction_for_session(reader).unwrap();
        db.destroy_session(reader).unwrap();
        let now = db.query("SELECT COUNT(*) FROM r", &[]).unwrap();
        assert_eq!(int(&now[0].values[0]), rows_after, "{kind}: a new snapshot sees it");
    }
}
