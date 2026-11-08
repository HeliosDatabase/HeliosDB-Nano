//! Spec R5.2 (`docs/plans/specs/R5_2_row_shape.md`): one row-shape funnel,
//! `ADD COLUMN` by missing value, and what follows from them.
//!
//! A row keeps the arity it was written with. `ALTER TABLE … ADD COLUMN … DEFAULT
//! <stable>` writes the schema and the column's missing value in one group and
//! rewrites nothing, so after it every row of the table is narrower than its
//! schema, and each read path must answer the default for them: that is what
//! [`every_read_path_answers_the_missing_value`] walks, on both executor families.
//! The stored widths are asserted too, so a test cannot pass because the rows
//! happen to have been widened.
//!
//! The physical standby's half is in `tests/physical_standby_embedded.rs`, the PG
//! wire's (simple, extended, COPY, the DataRow guard, the SQLSTATEs) in
//! `src/protocol/postgres/wire_tests.rs`, which can reach the handler directly.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::config::DestructiveDdlReplay;
use heliosdb_nano::protocol::mysql::MySqlHandler;
use heliosdb_nano::protocol::postgres::timeouts::ConnectionTimeouts;
use heliosdb_nano::{Config, EmbeddedDatabase, Tuple, Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Both DML executor families: the same SQL, the text and the params route.
#[derive(Debug, Clone, Copy)]
enum Family {
    Text,
    Params,
}
const FAMILIES: [Family; 2] = [Family::Text, Family::Params];

fn run(db: &EmbeddedDatabase, family: Family, sql: &str) -> u64 {
    match family {
        Family::Text => db.execute(sql),
        Family::Params => db.execute_params(sql, &[]),
    }
    .unwrap_or_else(|e| panic!("[{family:?}] `{sql}`: {e}"))
}

fn rows(db: &EmbeddedDatabase, family: Family, sql: &str) -> Vec<Tuple> {
    match family {
        Family::Text => db.query(sql, &[]),
        Family::Params => db.query_params(sql, &[]),
    }
    .unwrap_or_else(|e| panic!("[{family:?}] `{sql}`: {e}"))
}

fn returning(db: &EmbeddedDatabase, family: Family, sql: &str) -> Vec<Tuple> {
    match family {
        Family::Text => db.execute_returning(sql),
        Family::Params => db.execute_params_returning(sql, &[]),
    }
    .unwrap_or_else(|e| panic!("[{family:?}] `{sql}`: {e}"))
    .1
}

fn values(rows: Vec<Tuple>) -> Vec<Vec<Value>> {
    rows.into_iter().map(|row| row.values).collect()
}

fn s(text: &str) -> Value {
    Value::String(text.to_string())
}

fn int(value: &Value) -> i64 {
    match value {
        Value::Int2(n) => i64::from(*n),
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        other => panic!("expected an integer, got {other:?}"),
    }
}

fn float(value: &Value) -> f64 {
    match value {
        Value::Float4(n) => f64::from(*n),
        Value::Float8(n) => *n,
        Value::Numeric(n) => n.parse().unwrap(),
        other => int(other) as f64,
    }
}

/// The number of values each stored `data:` row of `table` holds, sorted.
fn stored_widths(db: &EmbeddedDatabase, table: &str) -> Vec<usize> {
    let prefix = format!("data:{table}:");
    let raw = db.storage.db();
    let mut read = rocksdb::ReadOptions::default();
    read.set_total_order_seek(true);
    let mut widths = Vec::new();
    for item in raw.iterator_opt(
        rocksdb::IteratorMode::From(prefix.as_bytes(), rocksdb::Direction::Forward),
        read,
    ) {
        let (key, value) = item.unwrap();
        if !key.starts_with(prefix.as_bytes()) {
            break;
        }
        let tuple: Tuple = bincode::deserialize(&value).unwrap();
        widths.push(tuple.values.len());
    }
    widths.sort_unstable();
    widths
}

fn key_exists(db: &EmbeddedDatabase, key: &str) -> bool {
    db.storage.db().get(key.as_bytes()).unwrap().is_some()
}

fn has_key_with_prefix(db: &EmbeddedDatabase, prefix: &str) -> bool {
    let mut read = rocksdb::ReadOptions::default();
    read.set_total_order_seek(true);
    let store = db.storage.db();
    let mut iter = store.iterator_opt(
        rocksdb::IteratorMode::From(prefix.as_bytes(), rocksdb::Direction::Forward),
        read,
    );
    match iter.next() {
        Some(item) => item.unwrap().0.starts_with(prefix.as_bytes()),
        None => false,
    }
}

/// `t` (id, v, n), three rows and an index, then three columns added: two with a
/// missing value and one without. No row has been rewritten.
fn seeded() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);
    db
}

fn seed(db: &EmbeddedDatabase) {
    for sql in [
        "CREATE TABLE t (id INT PRIMARY KEY, v TEXT, n INT)",
        "INSERT INTO t VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', 30)",
        "CREATE INDEX t_n ON t (n)",
        "ALTER TABLE t ADD COLUMN x INT DEFAULT 5",
        "ALTER TABLE t ADD COLUMN y TEXT DEFAULT 'usd'",
        "ALTER TABLE t ADD COLUMN z INT",
    ] {
        db.execute(sql).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    }
    assert_eq!(
        stored_widths(db, "t"),
        vec![3, 3, 3],
        "vacuity: the ALTERs rewrote the rows"
    );
}

fn full(id: i32, v: &str, n: i32) -> Vec<Value> {
    vec![
        Value::Int4(id),
        s(v),
        Value::Int4(n),
        Value::Int4(5),
        s("usd"),
        Value::Null,
    ]
}

// ---------------------------------------------------------------------------
// Every read path
// ---------------------------------------------------------------------------

#[test]
fn every_read_path_answers_the_missing_value() {
    for family in FAMILIES {
        let db = seeded();
        let all = vec![full(1, "a", 10), full(2, "b", 20), full(3, "c", 30)];
        let q = |sql: &str| values(rows(&db, family, sql));

        // Full scan.
        assert_eq!(q("SELECT * FROM t ORDER BY id"), all, "[{family:?}]");
        let mut unordered = q("SELECT * FROM t");
        unordered.sort_by_key(|row| int(&row[0]));
        assert_eq!(unordered, all, "[{family:?}] the plain scan");
        // Projections: the prefix and the selected-column decoders.
        assert_eq!(
            q("SELECT x FROM t ORDER BY id"),
            vec![vec![Value::Int4(5)]; 3],
            "[{family:?}]"
        );
        assert_eq!(
            q("SELECT id, y, z FROM t ORDER BY id"),
            vec![
                vec![Value::Int4(1), s("usd"), Value::Null],
                vec![Value::Int4(2), s("usd"), Value::Null],
                vec![Value::Int4(3), s("usd"), Value::Null],
            ],
            "[{family:?}]"
        );

        // Primary-key point reads: the fast path.
        assert_eq!(
            q("SELECT * FROM t WHERE id = 2"),
            vec![full(2, "b", 20)],
            "[{family:?}]"
        );
        assert_eq!(
            q("SELECT y, x, z FROM t WHERE id = 2"),
            vec![vec![s("usd"), Value::Int4(5), Value::Null]],
            "[{family:?}]"
        );
        // Twice: the second may be a row-cache or result-cache hit.
        assert_eq!(
            q("SELECT * FROM t WHERE id = 2"),
            vec![full(2, "b", 20)],
            "[{family:?}]"
        );

        // Secondary index.
        assert_eq!(
            q("SELECT * FROM t WHERE n = 20"),
            vec![full(2, "b", 20)],
            "[{family:?}]"
        );
        assert_eq!(
            q("SELECT x, y FROM t WHERE n = 30"),
            vec![vec![Value::Int4(5), s("usd")]],
            "[{family:?}]"
        );

        // LIMIT / OFFSET pushdown.
        for row in q("SELECT * FROM t LIMIT 2") {
            assert_eq!(row.len(), 6, "[{family:?}] {row:?}");
            assert_eq!(row[3], Value::Int4(5), "[{family:?}]");
            assert_eq!(row[4], s("usd"), "[{family:?}]");
        }
        assert_eq!(
            q("SELECT x FROM t LIMIT 1 OFFSET 1"),
            vec![vec![Value::Int4(5)]],
            "[{family:?}]"
        );

        // ORDER BY … LIMIT: top-k, by a stored and by a missing column.
        assert_eq!(
            q("SELECT id, x FROM t ORDER BY n DESC LIMIT 2"),
            vec![
                vec![Value::Int4(3), Value::Int4(5)],
                vec![Value::Int4(2), Value::Int4(5)],
            ],
            "[{family:?}]"
        );
        assert_eq!(
            q("SELECT id, y FROM t ORDER BY x, id LIMIT 2"),
            vec![vec![Value::Int4(1), s("usd")], vec![Value::Int4(2), s("usd")],],
            "[{family:?}]"
        );
        assert_eq!(
            q("SELECT id FROM t ORDER BY x DESC, id DESC LIMIT 1"),
            vec![vec![Value::Int4(3)]],
            "[{family:?}]"
        );

        // Filters on a missing column: the numeric and string fast decoders.
        let ids = |sql: &str| {
            let mut ids: Vec<i64> = q(sql).iter().map(|row| int(&row[0])).collect();
            ids.sort_unstable();
            ids
        };
        assert_eq!(ids("SELECT id FROM t WHERE x = 5"), vec![1, 2, 3], "[{family:?}]");
        assert!(ids("SELECT id FROM t WHERE x = 6").is_empty(), "[{family:?}]");
        assert_eq!(ids("SELECT id FROM t WHERE y = 'usd'"), vec![1, 2, 3], "[{family:?}]");
        assert!(ids("SELECT id FROM t WHERE y = 'eur'").is_empty(), "[{family:?}]");
        assert_eq!(ids("SELECT id FROM t WHERE x = 5 AND n = 20"), vec![2], "[{family:?}]");
        assert_eq!(
            ids("SELECT id FROM t WHERE x > 4 AND x < 6"),
            vec![1, 2, 3],
            "[{family:?}]"
        );
        assert_eq!(ids("SELECT id FROM t WHERE z IS NULL"), vec![1, 2, 3], "[{family:?}]");
        assert_eq!(
            ids("SELECT id FROM t WHERE x IS NOT NULL"),
            vec![1, 2, 3],
            "[{family:?}]"
        );

        // Aggregates: each fast path, and COUNT(*).
        assert_eq!(int(&q("SELECT COUNT(*) FROM t")[0][0]), 3, "[{family:?}]");
        let aggregated = q("SELECT COUNT(x), SUM(x), MIN(x), MAX(x) FROM t");
        assert_eq!(
            aggregated[0].iter().map(int).collect::<Vec<_>>(),
            vec![3, 15, 5, 5],
            "[{family:?}]"
        );
        let counted = q("SELECT COUNT(*), SUM(x), AVG(x) FROM t");
        assert_eq!(int(&counted[0][0]), 3, "[{family:?}]");
        assert_eq!(int(&counted[0][1]), 15, "[{family:?}]");
        assert!((float(&counted[0][2]) - 5.0).abs() < 1e-9, "[{family:?}] {counted:?}");
        let none = q("SELECT COUNT(z), SUM(z) FROM t");
        assert_eq!(int(&none[0][0]), 0, "[{family:?}]");
        assert_eq!(none[0][1], Value::Null, "[{family:?}]");
        let grouped = q("SELECT y, COUNT(*), SUM(x) FROM t GROUP BY y");
        assert_eq!(grouped.len(), 1, "[{family:?}] {grouped:?}");
        assert_eq!(grouped[0][0], s("usd"), "[{family:?}]");
        assert_eq!(int(&grouped[0][1]), 3, "[{family:?}]");
        assert_eq!(int(&grouped[0][2]), 15, "[{family:?}]");
        let by_x = q("SELECT x, COUNT(*) FROM t GROUP BY x");
        assert_eq!(by_x.len(), 1, "[{family:?}] {by_x:?}");
        assert_eq!(int(&by_x[0][0]), 5, "[{family:?}]");

        // Joins: hash, and index nested loop over the right side's row fetch.
        assert_eq!(
            q("SELECT a.id, b.x FROM t a JOIN t b ON a.id = b.id ORDER BY a.id"),
            vec![
                vec![Value::Int4(1), Value::Int4(5)],
                vec![Value::Int4(2), Value::Int4(5)],
                vec![Value::Int4(3), Value::Int4(5)],
            ],
            "[{family:?}]"
        );
        db.execute("CREATE TABLE k (id INT PRIMARY KEY, tid INT)").unwrap();
        db.execute("INSERT INTO k VALUES (7, 2)").unwrap();
        assert_eq!(
            q("SELECT k.id, t.x, t.y, t.z FROM k JOIN t ON k.tid = t.id"),
            vec![vec![Value::Int4(7), Value::Int4(5), s("usd"), Value::Null]],
            "[{family:?}]"
        );

        // A view, a subquery, UNION.
        db.execute("CREATE VIEW tv AS SELECT id, x FROM t").unwrap();
        assert_eq!(
            q("SELECT x FROM tv WHERE id = 1"),
            vec![vec![Value::Int4(5)]],
            "[{family:?}]"
        );
        assert_eq!(
            q("SELECT x FROM (SELECT x FROM t WHERE id = 3) s"),
            vec![vec![Value::Int4(5)]],
            "[{family:?}]"
        );

        // Nothing above widened a stored row.
        assert_eq!(stored_widths(&db, "t"), vec![3, 3, 3], "[{family:?}]");
    }
}

/// An index built on the column holds what the row reads, and an UPDATE takes the
/// missing value out of it.
#[test]
fn an_index_on_the_added_column_holds_the_missing_value() {
    let db = seeded();
    db.execute("CREATE INDEX t_x ON t (x)").unwrap();
    let mut ids: Vec<i64> = db
        .query("SELECT id FROM t WHERE x = 5", &[])
        .unwrap()
        .iter()
        .map(|row| int(&row.values[0]))
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2, 3]);
    db.execute("UPDATE t SET x = 9 WHERE id = 2").unwrap();
    assert_eq!(
        values(db.query("SELECT id FROM t WHERE x = 9", &[]).unwrap()),
        vec![vec![Value::Int4(2)]]
    );
    assert_eq!(db.query("SELECT id FROM t WHERE x = 5", &[]).unwrap().len(), 2);
}

// ---------------------------------------------------------------------------
// Writes over a narrow row
// ---------------------------------------------------------------------------

#[test]
fn returning_and_writes_read_a_narrow_row_whole() {
    for family in FAMILIES {
        let db = seeded();
        assert_eq!(
            values(returning(
                &db,
                family,
                "UPDATE t SET v = 'u' WHERE id = 1 RETURNING id, x, y"
            )),
            vec![vec![Value::Int4(1), Value::Int4(5), s("usd")]],
            "[{family:?}]"
        );
        assert_eq!(
            values(returning(&db, family, "DELETE FROM t WHERE id = 3 RETURNING *")),
            vec![full(3, "c", 30)],
            "[{family:?}]"
        );
        assert_eq!(
            values(returning(
                &db,
                family,
                "INSERT INTO t (id, v, n) VALUES (4, 'd', 40) RETURNING x, y, z"
            )),
            vec![vec![Value::Int4(5), s("usd"), Value::Null]],
            "[{family:?}] an INSERT takes the column's DEFAULT"
        );
        run(&db, family, "UPDATE t SET x = 9 WHERE id = 2");
        assert_eq!(
            values(rows(&db, family, "SELECT id, x, y FROM t ORDER BY id")),
            vec![
                vec![Value::Int4(1), Value::Int4(5), s("usd")],
                vec![Value::Int4(2), Value::Int4(9), s("usd")],
                vec![Value::Int4(4), Value::Int4(5), s("usd")],
            ],
            "[{family:?}]"
        );
        // Each row left was written by a statement that read it whole.
        assert_eq!(stored_widths(&db, "t"), vec![6, 6, 6], "[{family:?}]");
    }
}

#[test]
fn on_conflict_reads_the_conflicting_narrow_row_whole() {
    for family in FAMILIES {
        let db = seeded();
        run(
            &db,
            family,
            "INSERT INTO t (id, v, n) VALUES (1, 'x', 1) ON CONFLICT (id) DO UPDATE SET v = 'upserted'",
        );
        assert_eq!(
            values(rows(&db, family, "SELECT * FROM t WHERE id = 1")),
            vec![full(1, "upserted", 10)],
            "[{family:?}]"
        );
        run(
            &db,
            family,
            "INSERT INTO t (id, v, n) VALUES (2, 'x', 1) ON CONFLICT (id) DO UPDATE SET x = x + 1",
        );
        assert_eq!(
            values(rows(&db, family, "SELECT x FROM t WHERE id = 2")),
            vec![vec![Value::Int4(6)]],
            "[{family:?}] the assignment read the missing value"
        );
    }
}

/// An assignment to a column newer than the conflicting row lands: the row is
/// shaped before the assignments, and the arms have no quiet way out any more.
/// Each family, on main and on a branch; `z` has no missing value at all.
#[test]
fn on_conflict_assigns_a_column_newer_than_the_row() {
    let upsert = "INSERT INTO t (id, v, n) VALUES (1, 'x', 1) ON CONFLICT (id) DO UPDATE SET x = 7, z = 9";
    let want = vec![vec![
        Value::Int4(1),
        s("a"),
        Value::Int4(10),
        Value::Int4(7),
        s("usd"),
        Value::Int4(9),
    ]];
    for family in FAMILIES {
        let db = seeded();
        run(&db, family, upsert);
        assert_eq!(
            values(rows(&db, family, "SELECT * FROM t WHERE id = 1")),
            want,
            "[{family:?}]"
        );
        assert_eq!(
            stored_widths(&db, "t"),
            vec![3, 3, 6],
            "[{family:?}] that row was written whole, and only that one"
        );
        assert_eq!(
            values(rows(&db, family, "SELECT * FROM t WHERE id = 2")),
            vec![full(2, "b", 20)],
            "[{family:?}]"
        );

        let db = seeded();
        db.execute("CREATE BRANCH upsert_b AS OF NOW").unwrap();
        db.execute("USE BRANCH upsert_b").unwrap();
        run(&db, family, upsert);
        assert_eq!(
            values(rows(&db, family, "SELECT * FROM t WHERE id = 1")),
            want,
            "[{family:?}] on a branch"
        );
        db.execute("USE BRANCH main").unwrap();
        assert_eq!(
            values(rows(&db, family, "SELECT * FROM t WHERE id = 1")),
            vec![full(1, "a", 10)],
            "[{family:?}] main did not move"
        );
    }
}

/// `SET STORAGE` moves the column's value of every row, and a row older than the
/// column holds none: its missing value moves.
#[test]
fn set_storage_carries_the_missing_value_into_the_new_storage() {
    let db = seeded();
    db.execute("ALTER TABLE t ALTER COLUMN y SET STORAGE DICTIONARY")
        .unwrap();
    assert_eq!(
        values(db.query("SELECT y FROM t WHERE id = 1", &[]).unwrap()),
        vec![vec![s("usd")]]
    );
    assert_eq!(db.query("SELECT id FROM t WHERE y = 'usd'", &[]).unwrap().len(), 3);
}

// ---------------------------------------------------------------------------
// The caches
// ---------------------------------------------------------------------------

#[test]
fn the_result_and_plan_caches_do_not_serve_the_old_shape() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE r (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("INSERT INTO r VALUES (1, 10)").unwrap();
        for sql in ["SELECT * FROM r WHERE id = 1", "SELECT * FROM r", "SELECT v FROM r"] {
            rows(&db, family, sql);
            rows(&db, family, sql);
        }
        db.execute("ALTER TABLE r ADD COLUMN x INT DEFAULT 5").unwrap();
        for sql in ["SELECT * FROM r WHERE id = 1", "SELECT * FROM r"] {
            assert_eq!(
                values(rows(&db, family, sql)),
                vec![vec![Value::Int4(1), Value::Int4(10), Value::Int4(5)]],
                "[{family:?}] {sql}"
            );
        }
        assert_eq!(
            values(rows(&db, family, "SELECT v FROM r")),
            vec![vec![Value::Int4(10)]],
            "[{family:?}]"
        );
    }
}

/// The row cache holds a row as stored, so a reader planned under the schema of a
/// moment ago can neither poison it nor be served the wrong shape from it.
#[test]
fn the_row_cache_serves_each_reader_its_own_shape() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE r (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO r VALUES (1, 10)").unwrap();
    let raw = db.storage.db();
    let mut read = rocksdb::ReadOptions::default();
    read.set_total_order_seek(true);
    let key = raw
        .iterator_opt(
            rocksdb::IteratorMode::From(b"data:r:".as_slice(), rocksdb::Direction::Forward),
            read,
        )
        .next()
        .unwrap()
        .unwrap()
        .0;
    let row_id: u64 = std::str::from_utf8(&key[b"data:r:".len()..]).unwrap().parse().unwrap();
    let catalog = db.storage.catalog();
    let old = catalog.get_table_schema("r").unwrap();

    db.execute("ALTER TABLE r ADD COLUMN x INT DEFAULT 5").unwrap();
    let new = catalog.get_table_schema("r").unwrap();

    // The stale reader goes first and fills the cache.
    let stale = db.storage.get_row_by_id("r", row_id, &old).unwrap().unwrap();
    assert_eq!(stale.values, vec![Value::Int4(1), Value::Int4(10)]);
    let cached = db
        .storage
        .row_cache()
        .get("r", row_id)
        .expect("the read filled the cache");
    assert_eq!(cached.values.len(), 2, "the cache holds the row as stored");
    // The current reader, from that very entry.
    let current = db.storage.get_row_by_id("r", row_id, &new).unwrap().unwrap();
    assert_eq!(current.values, vec![Value::Int4(1), Value::Int4(10), Value::Int4(5)]);
    let arc = db.storage.get_row_by_id_arc("r", row_id, &new).unwrap().unwrap();
    assert_eq!(arc.values, vec![Value::Int4(1), Value::Int4(10), Value::Int4(5)]);

    // A row as wide as the new schema, read by the stale reader first: cut for it,
    // and left out of the cache, or the next reader would take the cut for a row
    // older than the column.
    db.execute("UPDATE r SET x = 7 WHERE id = 1").unwrap();
    assert!(
        db.storage.row_cache().get("r", row_id).is_none(),
        "the UPDATE evicted it"
    );
    let stale = db.storage.get_row_by_id("r", row_id, &old).unwrap().unwrap();
    assert_eq!(stale.values, vec![Value::Int4(1), Value::Int4(10)]);
    assert!(db.storage.row_cache().get("r", row_id).is_none());
    let current = db.storage.get_row_by_id("r", row_id, &new).unwrap().unwrap();
    assert_eq!(current.values, vec![Value::Int4(1), Value::Int4(10), Value::Int4(7)]);
}

// ---------------------------------------------------------------------------
// ADD COLUMN: what it does with each default
// ---------------------------------------------------------------------------

#[test]
fn a_stable_default_is_evaluated_once_and_writes_no_row() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE d (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO d VALUES (1), (2)").unwrap();
        run(&db, family, "ALTER TABLE d ADD COLUMN a INT DEFAULT 1 + 1");
        run(&db, family, "ALTER TABLE d ADD COLUMN b TEXT DEFAULT upper('x')");
        run(&db, family, "ALTER TABLE d ADD COLUMN c TIMESTAMP DEFAULT now()");
        run(&db, family, "ALTER TABLE d ADD COLUMN e BIGINT DEFAULT 5");
        run(&db, family, "ALTER TABLE d ADD COLUMN f INT DEFAULT NULL");
        run(&db, family, "ALTER TABLE d ADD COLUMN g INT");
        run(&db, family, "ALTER TABLE d ADD COLUMN h INT DEFAULT '7'");
        assert_eq!(stored_widths(&db, "d"), vec![1, 1], "[{family:?}]");

        let read = values(rows(&db, family, "SELECT a, b, c, e, f, g, h FROM d ORDER BY id"));
        assert_eq!(read[0], read[1], "[{family:?}] one value for every row");
        assert_eq!(read[0][0], Value::Int4(2), "[{family:?}]");
        assert_eq!(read[0][1], s("X"), "[{family:?}]");
        assert!(
            matches!(read[0][2], Value::Timestamp(_)),
            "[{family:?}] {:?}",
            read[0][2]
        );
        assert_eq!(read[0][3], Value::Int8(5), "[{family:?}] cast as an INSERT casts");
        assert_eq!(read[0][4], Value::Null, "[{family:?}]");
        assert_eq!(read[0][5], Value::Null, "[{family:?}]");
        assert_eq!(read[0][6], Value::Int4(7), "[{family:?}] cast as an INSERT casts");
        // A second read gets the same `now()`: it was evaluated at the ALTER.
        assert_eq!(
            values(rows(&db, family, "SELECT c FROM d WHERE id = 1"))[0][0],
            read[0][2],
            "[{family:?}]"
        );

        // The key exists exactly for a column with a value to answer.
        assert!(key_exists(&db, "meta:attmissing:d:a"), "[{family:?}]");
        assert!(!key_exists(&db, "meta:attmissing:d:f"), "[{family:?}]");
        assert!(!key_exists(&db, "meta:attmissing:d:g"), "[{family:?}]");

        // A later INSERT and the existing rows agree about the literal's type.
        db.execute("INSERT INTO d (id) VALUES (3)").unwrap();
        let inserted = values(rows(&db, family, "SELECT e, h FROM d ORDER BY id"));
        assert_eq!(inserted[0], inserted[2], "[{family:?}]");
    }
}

#[test]
fn not_null_with_no_usable_default_on_rows_is_refused_before_any_write() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE d (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO d VALUES (1)").unwrap();
        for sql in [
            "ALTER TABLE d ADD COLUMN c INT NOT NULL",
            "ALTER TABLE d ADD COLUMN c INT NOT NULL DEFAULT NULL",
        ] {
            let result = match family {
                Family::Text => db.execute(sql),
                Family::Params => db.execute_params(sql, &[]),
            };
            let err = result.expect_err(sql).to_string();
            assert!(err.contains("violates not-null constraint"), "[{family:?}] {err}");
            assert!(err.contains("contains null values"), "[{family:?}] {err}");
            assert_eq!(
                rows(&db, family, "SELECT * FROM d")[0].values.len(),
                1,
                "[{family:?}] {sql}"
            );
        }
        run(&db, family, "ALTER TABLE d ADD COLUMN c INT NOT NULL DEFAULT 5");
        assert_eq!(
            values(rows(&db, family, "SELECT c FROM d")),
            vec![vec![Value::Int4(5)]],
            "[{family:?}]"
        );
        db.execute("CREATE TABLE empty_d (id INT PRIMARY KEY)").unwrap();
        run(&db, family, "ALTER TABLE empty_d ADD COLUMN c INT NOT NULL");
    }
}

#[test]
fn a_volatile_default_gives_each_row_its_own_value_in_one_rewrite() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE d (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO d VALUES (1), (2), (3)").unwrap();
        db.execute("CREATE SEQUENCE d_s").unwrap();

        run(&db, family, "ALTER TABLE d ADD COLUMN u UUID DEFAULT gen_random_uuid()");
        run(&db, family, "ALTER TABLE d ADD COLUMN q BIGINT DEFAULT nextval('d_s')");
        run(
            &db,
            family,
            "ALTER TABLE d ADD COLUMN r DOUBLE PRECISION DEFAULT random()",
        );
        assert_eq!(stored_widths(&db, "d"), vec![4, 4, 4], "[{family:?}] rewritten");
        assert!(!key_exists(&db, "meta:attmissing:d:u"), "[{family:?}] no one value");

        let read = values(rows(&db, family, "SELECT u, q, r FROM d"));
        for column in 0..3 {
            let mut seen: Vec<String> = read.iter().map(|row| format!("{:?}", row[column])).collect();
            seen.sort();
            seen.dedup();
            assert_eq!(seen.len(), 3, "[{family:?}] column {column}: {read:?}");
        }
        assert!(
            read.iter().all(|row| !row.contains(&Value::Null)),
            "[{family:?}] {read:?}"
        );
    }
}

/// Every snapshot-resolved read used to shape the row's old narrow version, so a
/// NOT NULL column read NULL in any explicit transaction, and an `INSERT … SELECT`
/// copied those NULLs for good. The rewrite writes a version per row in its group.
#[test]
fn a_volatile_rewrite_is_what_a_snapshot_reads() {
    use heliosdb_nano::session::IsolationLevel;

    let pause = || std::thread::sleep(std::time::Duration::from_millis(1200));
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE v1 (id INT PRIMARY KEY, n INT)").unwrap();
    db.execute("CREATE TABLE v1c (id INT PRIMARY KEY, c TEXT)").unwrap();
    db.execute("INSERT INTO v1 VALUES (1, 10), (2, 20), (3, 30)").unwrap();

    // Began before the statement.
    let before = db.create_session("before", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(before).unwrap();
    assert_eq!(
        db.query_in_session(before, "SELECT id FROM v1", &[]).unwrap().len(),
        3,
        "vacuity: its snapshot holds the rows"
    );

    db.execute("ALTER TABLE v1 ADD COLUMN c TEXT NOT NULL DEFAULT gen_random_uuid()::text")
        .unwrap();
    let want = values(db.query("SELECT id, c FROM v1 ORDER BY id", &[]).unwrap());
    assert!(want.iter().all(|row| row[1] != Value::Null), "autocommit: {want:?}");
    assert_ne!(want[0][1], want[1][1]);

    // Its rows are still there, older than the column.
    let mut older = values(db.query_in_session(before, "SELECT id, n, c FROM v1", &[]).unwrap());
    older.sort_by_key(|row| int(&row[0]));
    assert_eq!(
        older,
        vec![
            vec![Value::Int4(1), Value::Int4(10), Value::Null],
            vec![Value::Int4(2), Value::Int4(20), Value::Null],
            vec![Value::Int4(3), Value::Int4(30), Value::Null],
        ],
    );
    db.commit_transaction_for_session(before).unwrap();
    db.destroy_session(before).unwrap();

    // An explicit transaction: the scan, the filters, the point read agree.
    db.execute("BEGIN").unwrap();
    assert_eq!(
        values(db.query("SELECT id, c FROM v1 ORDER BY id", &[]).unwrap()),
        want,
        "BEGIN: the scan"
    );
    assert_eq!(
        int(&db.query("SELECT COUNT(*) FROM v1 WHERE c IS NULL", &[]).unwrap()[0].values[0]),
        0,
        "BEGIN: the filter"
    );
    assert_eq!(
        values(db.query("SELECT id, c FROM v1 WHERE id = 2", &[]).unwrap()),
        vec![want[1].clone()],
        "BEGIN: the point read"
    );
    // What it copies, it copies for good.
    db.execute("INSERT INTO v1c SELECT id, c FROM v1").unwrap();
    db.execute("COMMIT").unwrap();
    assert_eq!(
        values(db.query("SELECT id, c FROM v1c ORDER BY id", &[]).unwrap()),
        want,
        "INSERT … SELECT in a transaction"
    );

    // REPEATABLE READ begun after it.
    let after = db.create_session("after", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(after).unwrap();
    let mut read = values(db.query_in_session(after, "SELECT id, c FROM v1", &[]).unwrap());
    read.sort_by_key(|row| int(&row[0]));
    assert_eq!(read, want, "REPEATABLE READ");
    db.commit_transaction_for_session(after).unwrap();
    db.destroy_session(after).unwrap();

    // AS OF a time after it.
    pause();
    let mut read = values(
        db.query(&format!("SELECT id, c FROM v1 AS OF TIMESTAMP '{}'", sql_now()), &[])
            .unwrap(),
    );
    read.sort_by_key(|row| int(&row[0]));
    assert_eq!(read, want, "AS OF");

    // What the statement's one version does not hide: a later write of the row.
    db.execute("UPDATE v1 SET n = 11 WHERE id = 1").unwrap();
    db.execute("BEGIN").unwrap();
    assert_eq!(
        values(db.query("SELECT id, c FROM v1 ORDER BY id", &[]).unwrap()),
        want,
        "after an UPDATE of another column"
    );
    db.execute("ROLLBACK").unwrap();
}

/// A row with no version at all reads `data:` at every snapshot. Its first version
/// must not hide it from a snapshot older than the statement.
#[test]
fn a_volatile_rewrite_keeps_a_versionless_row_visible_to_an_older_snapshot() {
    use heliosdb_nano::session::IsolationLevel;

    let temp = TempDir::new().unwrap();
    {
        let mut config = persistent(temp.path(), DestructiveDdlReplay::Refuse);
        config.storage.time_travel_enabled = false;
        let db = EmbeddedDatabase::with_config(config).unwrap();
        db.execute("CREATE TABLE p (id INT PRIMARY KEY, n INT)").unwrap();
        db.execute("INSERT INTO p VALUES (1, 10), (2, 20)").unwrap();
    }
    let db = open(temp.path(), DestructiveDdlReplay::Refuse);
    assert!(
        !has_key_with_prefix(&db, "v_idx:p:"),
        "vacuity: the rows have no version"
    );
    let older = db.create_session("older", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(older).unwrap();
    assert_eq!(db.query_in_session(older, "SELECT id FROM p", &[]).unwrap().len(), 2);

    db.execute("ALTER TABLE p ADD COLUMN u UUID DEFAULT gen_random_uuid()")
        .unwrap();

    let mut read = values(db.query_in_session(older, "SELECT id, n, u FROM p", &[]).unwrap());
    read.sort_by_key(|row| int(&row[0]));
    assert_eq!(
        read,
        vec![
            vec![Value::Int4(1), Value::Int4(10), Value::Null],
            vec![Value::Int4(2), Value::Int4(20), Value::Null],
        ],
        "the rows are still there, older than the column"
    );
    db.commit_transaction_for_session(older).unwrap();
    db.destroy_session(older).unwrap();

    db.execute("BEGIN").unwrap();
    let read = values(db.query("SELECT u FROM p", &[]).unwrap());
    db.execute("COMMIT").unwrap();
    assert_eq!(read.len(), 2);
    assert!(read.iter().all(|row| row[0] != Value::Null), "{read:?}");
}

/// With time travel off the commit path writes no versions, and neither does this.
#[test]
fn a_volatile_rewrite_writes_no_version_with_time_travel_off() {
    let mut config = Config::in_memory();
    config.storage.time_travel_enabled = false;
    let db = EmbeddedDatabase::with_config(config).unwrap();
    db.execute("CREATE TABLE f (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO f VALUES (1), (2)").unwrap();
    db.execute("ALTER TABLE f ADD COLUMN u UUID DEFAULT gen_random_uuid()")
        .unwrap();
    assert!(!has_key_with_prefix(&db, "v:f:"), "no version");
    assert!(!has_key_with_prefix(&db, "v_idx:f:"), "no version");
    db.execute("BEGIN").unwrap();
    let read = values(db.query("SELECT u FROM f", &[]).unwrap());
    db.execute("COMMIT").unwrap();
    assert!(read.iter().all(|row| row[0] != Value::Null), "{read:?}");
}

/// `random()` used to answer about 1e8, from a clock seed, the same for every row
/// of a fast statement, and `floor(random() * 1000)::int` failed as out of range.
#[test]
fn random_is_in_its_range_and_differs_by_row() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE r (id INT PRIMARY KEY)").unwrap();
        let ids: Vec<String> = (0..50).map(|id| format!("({id})")).collect();
        db.execute(&format!("INSERT INTO r VALUES {}", ids.join(", "))).unwrap();

        let read: Vec<f64> = rows(&db, family, "SELECT random() FROM r")
            .iter()
            .map(|row| float(&row.values[0]))
            .collect();
        assert_eq!(read.len(), 50);
        assert!(
            read.iter().all(|value| (0.0..1.0).contains(value)),
            "[{family:?}] {read:?}"
        );
        let mut distinct = read.clone();
        distinct.sort_by(f64::total_cmp);
        distinct.dedup();
        assert!(
            distinct.len() > 40,
            "[{family:?}] the rows of one statement differ: {read:?}"
        );

        run(
            &db,
            family,
            "ALTER TABLE r ADD COLUMN d INT DEFAULT floor(random() * 1000)::int",
        );
        let read: Vec<i64> = rows(&db, family, "SELECT d FROM r")
            .iter()
            .map(|row| int(&row.values[0]))
            .collect();
        assert!(
            read.iter().all(|value| (0..1000).contains(value)),
            "[{family:?}] {read:?}"
        );
        assert!(read.iter().any(|value| *value != read[0]), "[{family:?}] {read:?}");
    }
}

/// A function declared IMMUTABLE or STABLE is a stable default: one value, no
/// rewrite. Declared VOLATILE, or not declared, it is evaluated per row.
#[test]
fn a_function_declared_stable_is_a_stable_default() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        for sql in [
            "CREATE FUNCTION f_imm() RETURNS INT AS $$ SELECT 7 $$ LANGUAGE sql IMMUTABLE",
            "CREATE FUNCTION f_stb() RETURNS INT AS $$ SELECT 8 $$ LANGUAGE sql STABLE",
            "CREATE FUNCTION f_vol() RETURNS INT AS $$ SELECT 9 $$ LANGUAGE sql VOLATILE",
            "CREATE FUNCTION f_und() RETURNS INT AS $$ SELECT 6 $$ LANGUAGE sql",
            "CREATE TABLE d (id INT PRIMARY KEY)",
            "INSERT INTO d VALUES (1), (2)",
        ] {
            db.execute(sql).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
        }

        run(&db, family, "ALTER TABLE d ADD COLUMN a INT DEFAULT f_imm()");
        run(&db, family, "ALTER TABLE d ADD COLUMN b INT DEFAULT f_stb()");
        assert_eq!(stored_widths(&db, "d"), vec![1, 1], "[{family:?}] no rewrite");
        assert!(key_exists(&db, "meta:attmissing:d:a"), "[{family:?}]");
        assert!(key_exists(&db, "meta:attmissing:d:b"), "[{family:?}]");

        run(&db, family, "ALTER TABLE d ADD COLUMN c INT DEFAULT f_vol()");
        assert_eq!(stored_widths(&db, "d"), vec![4, 4], "[{family:?}] a rewrite");
        run(&db, family, "ALTER TABLE d ADD COLUMN e INT DEFAULT f_und()");
        assert_eq!(stored_widths(&db, "d"), vec![5, 5], "[{family:?}] a rewrite");
        assert!(!key_exists(&db, "meta:attmissing:d:c"), "[{family:?}]");
        assert!(!key_exists(&db, "meta:attmissing:d:e"), "[{family:?}]");

        let row = vec![Value::Int4(7), Value::Int4(8), Value::Int4(9), Value::Int4(6)];
        assert_eq!(
            values(rows(&db, family, "SELECT a, b, c, e FROM d")),
            vec![row.clone(), row],
            "[{family:?}]"
        );
    }
}

/// One row of about 1 KiB; `rows` of them pass a 1 MiB cap when `rows` > 1100 or so.
fn wide_rows_config_db(cap: u64, rows: usize) -> EmbeddedDatabase {
    let mut config = Config::in_memory();
    config.storage.max_atomic_rewrite_bytes = cap;
    let db = EmbeddedDatabase::with_config(config).unwrap();
    db.execute("CREATE TABLE big (id INT PRIMARY KEY, body TEXT)").unwrap();
    let body = "b".repeat(1000);
    for start in (0..rows).step_by(200) {
        let values: Vec<String> = (start..(start + 200).min(rows))
            .map(|id| format!("({id}, '{body}')"))
            .collect();
        db.execute(&format!("INSERT INTO big VALUES {}", values.join(", ")))
            .unwrap();
    }
    db
}

#[test]
fn a_volatile_rewrite_above_the_cap_is_refused_and_writes_nothing() {
    const CAP: u64 = 1024 * 1024;
    for family in FAMILIES {
        let db = wide_rows_config_db(CAP, 2000);
        let sql = "ALTER TABLE big ADD COLUMN u UUID DEFAULT gen_random_uuid()";
        let result = match family {
            Family::Text => db.execute(sql),
            Family::Params => db.execute_params(sql, &[]),
        };
        let err = result.expect_err("above the cap").to_string();
        assert!(
            err.contains(heliosdb_nano::ATOMIC_REWRITE_TOO_LARGE_MARKER),
            "[{family:?}] {err}"
        );
        assert!(
            err.contains("without a default"),
            "[{family:?}] names the alternative: {err}"
        );
        assert_eq!(stored_widths(&db, "big"), vec![2; 2000], "[{family:?}]");
        assert!(
            db.storage.catalog().get_table_schema("big").unwrap().columns.len() == 2,
            "[{family:?}] the schema did not move"
        );

        // A stable default does not read the cap, whatever the table's size.
        run(&db, family, "ALTER TABLE big ADD COLUMN s INT DEFAULT 1");
        assert_eq!(stored_widths(&db, "big"), vec![2; 2000], "[{family:?}]");
        assert_eq!(
            int(&rows(&db, family, "SELECT SUM(s) FROM big")[0].values[0]),
            2000,
            "[{family:?}]"
        );

        // Below the cap.
        let small = wide_rows_config_db(CAP, 100);
        run(&small, family, sql);
        assert_eq!(stored_widths(&small, "big"), vec![3; 100], "[{family:?}]");
    }
}

/// Phase B4: each live branch that reads the table through main takes a copy of every
/// row a volatile rewrite of main changes, from the same write, and the cap counts those
/// copies too, measured before anything is written. A rewrite that fits alone is
/// refused with four such branches, and fits again once they are gone.
#[test]
fn a_volatile_rewrite_counts_the_copies_mains_live_branches_take() {
    const CAP: u64 = 1024 * 1024;
    let db = wide_rows_config_db(CAP, 250);
    let sql = "ALTER TABLE big ADD COLUMN u UUID DEFAULT gen_random_uuid()";
    let branches = ["c1", "c2", "c3", "c4"];
    for branch in branches {
        db.execute(&format!("CREATE BRANCH {branch} AS OF NOW")).unwrap();
    }
    let err = db
        .execute(sql)
        .expect_err("the copies take it past the cap")
        .to_string();
    assert!(err.contains(heliosdb_nano::ATOMIC_REWRITE_TOO_LARGE_MARKER), "{err}");
    assert_eq!(stored_widths(&db, "big"), vec![2; 250]);
    for branch in branches {
        db.execute(&format!("DROP BRANCH {branch}")).unwrap();
    }
    db.execute(sql).expect("alone it fits");
    assert_eq!(stored_widths(&db, "big"), vec![3; 250]);
}

#[test]
fn the_cap_is_validated_by_the_configuration() {
    let mut config = Config::in_memory();
    config.storage.max_atomic_rewrite_bytes = 1024;
    assert!(config.validate().is_err());
    config.storage.max_atomic_rewrite_bytes = 3 * 1024 * 1024 * 1024;
    assert!(config.validate().is_err());
    config.storage.max_atomic_rewrite_bytes = 1024 * 1024;
    assert!(config.validate().is_ok());

    // `start --config` loads through this, and takes no other check on the way.
    for bad in ["0", "1", "1048575", "2147483649", "99999999999999"] {
        let toml = format!("[storage]\nmax_atomic_rewrite_bytes = {bad}\n");
        let err = Config::from_toml_str(&toml).expect_err(bad).to_string();
        assert!(err.contains("max_atomic_rewrite_bytes"), "{bad}: {err}");
    }
    for good in ["1048576", "268435456", "2147483648"] {
        let toml = format!("[storage]\nmax_atomic_rewrite_bytes = {good}\n");
        Config::from_toml_str(&toml).unwrap_or_else(|e| panic!("{good}: {e}"));
    }

    // And a configuration built in code, when the database opens.
    for bad in [0u64, 1, 5 * 1024 * 1024 * 1024] {
        let mut config = Config::in_memory();
        config.storage.max_atomic_rewrite_bytes = bad;
        let err = EmbeddedDatabase::with_config(config)
            .err()
            .unwrap_or_else(|| panic!("{bad}"));
        assert!(err.to_string().contains("max_atomic_rewrite_bytes"), "{bad}: {err}");
    }
}

#[test]
fn a_unique_over_a_stable_default_on_two_rows_is_refused_with_no_column_left() {
    for sql in [
        "ALTER TABLE d ADD COLUMN c INT DEFAULT 5 UNIQUE",
        "ALTER TABLE d ADD COLUMN c INT DEFAULT 2 + 3 UNIQUE",
    ] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE d (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO d VALUES (1), (2)").unwrap();
        db.execute(sql).expect_err(sql);
        assert_eq!(
            db.storage.catalog().get_table_schema("d").unwrap().columns.len(),
            1,
            "{sql}"
        );
    }
    // NULLs are distinct.
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE d (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO d VALUES (1), (2)").unwrap();
    db.execute("ALTER TABLE d ADD COLUMN c INT UNIQUE").unwrap();
    // One row: the constraint holds the value every later row must not repeat.
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE d (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO d VALUES (1)").unwrap();
    db.execute("ALTER TABLE d ADD COLUMN c INT DEFAULT 5 UNIQUE").unwrap();
    db.execute("INSERT INTO d VALUES (2, 5)")
        .expect_err("the row of id 1 holds 5");
}

/// A reader during the statement sees it before or after, never a mix: there is
/// one group, and no row in it.
/// A clause's refusal used to come after the earlier clauses had written. Every
/// `ADD COLUMN` clause's refusals are asked before the first clause writes, on each
/// family. (They are still one group each: see `execute_alter_table_multi`.)
#[test]
fn a_later_clause_of_a_multi_clause_alter_refuses_before_any_clause_writes() {
    let attempt = |db: &EmbeddedDatabase, family: Family, sql: &str| match family {
        Family::Text => db.execute(sql),
        Family::Params => db.execute_params(sql, &[]),
    };
    for family in FAMILIES {
        for (sql, marker) in [
            (
                "ALTER TABLE t ADD COLUMN c INT NOT NULL DEFAULT 5, ADD COLUMN d INT NOT NULL",
                "violates not-null constraint",
            ),
            (
                "ALTER TABLE t ADD COLUMN w INT DEFAULT 1, ADD COLUMN z2 TEXT DEFAULT gen_random_uuid()::text, \
                 ADD COLUMN q INT NOT NULL",
                "violates not-null constraint",
            ),
            (
                "ALTER TABLE t ADD COLUMN w INT DEFAULT 1, \
                 ADD COLUMN q INT NOT NULL DEFAULT (CASE WHEN random() > 2 THEN 1 END)",
                "violates not-null constraint",
            ),
            (
                "ALTER TABLE t ADD COLUMN w INT DEFAULT 2, ADD COLUMN w INT DEFAULT 3",
                "already exists",
            ),
            (
                "ALTER TABLE t ADD COLUMN w INT DEFAULT 2, ADD COLUMN v INT DEFAULT 3",
                "already exists",
            ),
        ] {
            let db = seeded();
            let before = values(rows(&db, family, "SELECT * FROM t ORDER BY id"));
            let err = attempt(&db, family, sql).expect_err(sql).to_string();
            assert!(err.contains(marker), "[{family:?}] {sql}: {err}");
            assert_eq!(
                values(rows(&db, family, "SELECT * FROM t ORDER BY id")),
                before,
                "[{family:?}] {sql}"
            );
            assert_eq!(
                db.storage.catalog().get_table_schema("t").unwrap().columns.len(),
                6,
                "[{family:?}] {sql}: no column stayed"
            );
            assert_eq!(stored_widths(&db, "t"), vec![3, 3, 3], "[{family:?}] {sql}");
        }

        // The cap, in a later clause.
        let db = wide_rows_config_db(1024 * 1024, 2000);
        let sql = "ALTER TABLE big ADD COLUMN a INT DEFAULT 1, ADD COLUMN u UUID DEFAULT gen_random_uuid()";
        let err = attempt(&db, family, sql).expect_err("above the cap").to_string();
        assert!(
            err.contains(heliosdb_nano::ATOMIC_REWRITE_TOO_LARGE_MARKER),
            "[{family:?}] {err}"
        );
        assert_eq!(
            db.storage.catalog().get_table_schema("big").unwrap().columns.len(),
            2,
            "[{family:?}] no column stayed"
        );

        // And what is legal all lands.
        let db = seeded();
        run(
            &db,
            family,
            "ALTER TABLE t ADD COLUMN a1 INT DEFAULT 1, ADD COLUMN u1 UUID DEFAULT gen_random_uuid(), \
             ADD COLUMN b1 INT NOT NULL DEFAULT 2",
        );
        let read = values(rows(&db, family, "SELECT a1, u1, b1 FROM t ORDER BY id"));
        assert_eq!(read.len(), 3, "[{family:?}]");
        assert!(
            read.iter()
                .all(|row| row[0] == Value::Int4(1) && row[1] != Value::Null && row[2] == Value::Int4(2)),
            "[{family:?}] {read:?}"
        );
    }

    for family in FAMILIES {
        // Own staged rows count as rows.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE s (id INT PRIMARY KEY)").unwrap();
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO s VALUES (1)").unwrap();
        let err = attempt(
            &db,
            family,
            "ALTER TABLE s ADD COLUMN a INT DEFAULT 1, ADD COLUMN b INT NOT NULL",
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("violates not-null constraint"),
            "[{family:?}] {err}"
        );
        db.execute("ROLLBACK").ok();
        assert_eq!(
            db.storage.catalog().get_table_schema("s").unwrap().columns.len(),
            1,
            "[{family:?}]"
        );

        // And a volatile clause over them under a savepoint is refused first.
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO s VALUES (1)").unwrap();
        db.execute("SAVEPOINT sp").unwrap();
        let err = attempt(
            &db,
            family,
            "ALTER TABLE s ADD COLUMN a INT DEFAULT 1, ADD COLUMN u UUID DEFAULT gen_random_uuid()",
        )
        .unwrap_err();
        assert!(
            err.to_string().contains(heliosdb_nano::DDL_OVER_STAGED_ROWS_MARKER),
            "[{family:?}] {err}"
        );
        db.execute("ROLLBACK").ok();
        assert_eq!(
            db.storage.catalog().get_table_schema("s").unwrap().columns.len(),
            1,
            "[{family:?}]"
        );
    }
}

#[test]
fn a_concurrent_reader_sees_add_column_before_or_after() {
    const ROWS: usize = 4_000;
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
    let values: Vec<String> = (1..=ROWS).map(|id| format!("({id}, 'v{id}')")).collect();
    db.execute(&format!("INSERT INTO t VALUES {}", values.join(", ")))
        .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let torn = Arc::new(AtomicUsize::new(0));
    let reads = Arc::new(AtomicUsize::new(0));
    let reader = {
        let (db, stop, torn, reads) = (
            Arc::clone(&db),
            Arc::clone(&stop),
            Arc::clone(&torn),
            Arc::clone(&reads),
        );
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                if let Ok(rows) = db.query("SELECT d FROM t", &[]) {
                    reads.fetch_add(1, Ordering::Relaxed);
                    if rows.iter().any(|row| row.values.first() != Some(&Value::Int4(7))) {
                        torn.fetch_add(1, Ordering::Relaxed);
                    }
                }
                if let Ok(rows) = db.query("SELECT * FROM t WHERE id = 17", &[]) {
                    if rows
                        .first()
                        .is_some_and(|row| row.values.len() == 3 && row.values[2] != Value::Int4(7))
                    {
                        torn.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(30));
    db.execute("ALTER TABLE t ADD COLUMN d INT DEFAULT 7").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    stop.store(true, Ordering::Release);
    reader.join().unwrap();
    assert!(
        reads.load(Ordering::Relaxed) > 0,
        "vacuity: the reader never read after the ALTER"
    );
    assert_eq!(torn.load(Ordering::Relaxed), 0);
}

// ---------------------------------------------------------------------------
// The key's life
// ---------------------------------------------------------------------------

#[test]
fn drop_column_then_add_of_the_name_does_not_resurrect_the_old_value() {
    for family in FAMILIES {
        let db = seeded();
        run(&db, family, "ALTER TABLE t DROP COLUMN x");
        assert!(!key_exists(&db, "meta:attmissing:t:x"), "[{family:?}]");
        run(&db, family, "ALTER TABLE t ADD COLUMN x INT");
        assert_eq!(
            values(rows(&db, family, "SELECT x, y FROM t WHERE id = 1")),
            vec![vec![Value::Null, s("usd")]],
            "[{family:?}] no default now: NULL, and y still reads its value"
        );
        run(&db, family, "ALTER TABLE t DROP COLUMN x");
        run(&db, family, "ALTER TABLE t ADD COLUMN x INT DEFAULT 6");
        assert_eq!(
            values(rows(&db, family, "SELECT x FROM t WHERE id = 1")),
            vec![vec![Value::Int4(6)]],
            "[{family:?}]"
        );
    }
}

#[test]
fn drop_column_keeps_the_other_columns_values_in_place() {
    let db = seeded();
    // A stored column, before the missing ones: every position after it moves.
    db.execute("ALTER TABLE t DROP COLUMN v").unwrap();
    assert_eq!(
        values(db.query("SELECT * FROM t WHERE id = 1", &[]).unwrap()),
        vec![vec![
            Value::Int4(1),
            Value::Int4(10),
            Value::Int4(5),
            s("usd"),
            Value::Null
        ]]
    );
    // A missing one: the row holds nothing of it.
    db.execute("ALTER TABLE t DROP COLUMN x").unwrap();
    assert_eq!(
        values(db.query("SELECT * FROM t ORDER BY id LIMIT 1", &[]).unwrap()),
        vec![vec![Value::Int4(1), Value::Int4(10), s("usd"), Value::Null]]
    );
}

#[test]
fn rename_column_and_rename_table_carry_the_value() {
    for family in FAMILIES {
        let db = seeded();
        run(&db, family, "ALTER TABLE t RENAME COLUMN x TO renamed");
        assert!(!key_exists(&db, "meta:attmissing:t:x"), "[{family:?}]");
        assert!(key_exists(&db, "meta:attmissing:t:renamed"), "[{family:?}]");
        assert_eq!(
            values(rows(&db, family, "SELECT renamed FROM t WHERE id = 1")),
            vec![vec![Value::Int4(5)]],
            "[{family:?}]"
        );

        db.execute("ALTER TABLE t RENAME TO t2").unwrap();
        assert!(!key_exists(&db, "meta:attmissing:t:renamed"), "[{family:?}]");
        assert!(key_exists(&db, "meta:attmissing:t2:renamed"), "[{family:?}]");
        assert!(key_exists(&db, "meta:attmissing:t2:y"), "[{family:?}]");
        assert_eq!(
            values(rows(&db, family, "SELECT renamed, y FROM t2 WHERE id = 1")),
            vec![vec![Value::Int4(5), s("usd")]],
            "[{family:?}]"
        );
    }
}

#[test]
fn drop_table_purges_the_values_and_a_new_table_of_the_name_has_none() {
    let db = seeded();
    db.execute("DROP TABLE t").unwrap();
    assert!(!key_exists(&db, "meta:attmissing:t:x"));
    assert!(!key_exists(&db, "meta:attmissing:t:y"));
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT, n INT)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 'a', 10)").unwrap();
    db.execute("ALTER TABLE t ADD COLUMN x INT").unwrap();
    assert_eq!(
        values(db.query("SELECT x FROM t", &[]).unwrap()),
        vec![vec![Value::Null]]
    );
}

#[test]
fn rows_narrower_than_their_schema_with_no_missing_value_read_null() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 10)").unwrap();
    db.execute("ALTER TABLE t ADD COLUMN c INT").unwrap();
    assert_eq!(stored_widths(&db, "t"), vec![2]);
    for sql in ["SELECT * FROM t", "SELECT * FROM t WHERE id = 1"] {
        assert_eq!(
            values(db.query(sql, &[]).unwrap()),
            vec![vec![Value::Int4(1), Value::Int4(10), Value::Null]],
            "{sql}"
        );
    }
    assert_eq!(db.query("SELECT id FROM t WHERE c IS NULL", &[]).unwrap().len(), 1);
    assert_eq!(db.query("SELECT id FROM t WHERE c = 0", &[]).unwrap().len(), 0);
}

#[test]
fn dump_and_restore_keep_what_the_rows_read() {
    let workspace = TempDir::new().unwrap();
    let dump = workspace.path().join("rows.hdmp");
    let source = seeded();
    let before = values(source.query("SELECT * FROM t ORDER BY id", &[]).unwrap());
    source.dump_full(&dump).unwrap();
    drop(source);

    let restored_dir = workspace.path().join("restored");
    std::fs::create_dir_all(&restored_dir).unwrap();
    let mut db = EmbeddedDatabase::new(&restored_dir).unwrap();
    db.restore_from_dump(&dump).unwrap();
    assert_eq!(values(db.query("SELECT * FROM t ORDER BY id", &[]).unwrap()), before);
    assert_eq!(
        values(db.query("SELECT x, y FROM t WHERE id = 2", &[]).unwrap()),
        vec![vec![Value::Int4(5), s("usd")]]
    );
    db.execute("INSERT INTO t (id, v, n) VALUES (9, 'z', 90)").unwrap();
    assert_eq!(
        values(db.query("SELECT x FROM t WHERE id = 9", &[]).unwrap()),
        vec![vec![Value::Int4(5)]],
        "the column's DEFAULT came through too"
    );
}

// ---------------------------------------------------------------------------
// Branches
// ---------------------------------------------------------------------------

#[test]
fn a_branch_view_reads_mains_missing_values() {
    for family in FAMILIES {
        let db = seeded();
        db.execute("CREATE BRANCH b AS OF NOW").unwrap();
        db.execute("USE BRANCH b").unwrap();
        assert_eq!(
            values(rows(&db, family, "SELECT * FROM t ORDER BY id")),
            vec![full(1, "a", 10), full(2, "b", 20), full(3, "c", 30)],
            "[{family:?}]"
        );
        assert_eq!(
            values(rows(&db, family, "SELECT * FROM t WHERE id = 2")),
            vec![full(2, "b", 20)],
            "[{family:?}]"
        );
        // A row the branch holds itself, forked from the narrow row.
        run(&db, family, "UPDATE t SET v = 'forked' WHERE id = 1");
        assert_eq!(
            values(rows(&db, family, "SELECT * FROM t WHERE id = 1")),
            vec![full(1, "forked", 10)],
            "[{family:?}]"
        );
        assert_eq!(
            int(&rows(&db, family, "SELECT SUM(x) FROM t")[0].values[0]),
            15,
            "[{family:?}]"
        );

        // The branch's own ADD COLUMN materializes copies, under main's values.
        run(&db, family, "ALTER TABLE t ADD COLUMN w INT DEFAULT 7");
        run(&db, family, "ALTER TABLE t ADD COLUMN e INT DEFAULT 1 + 1");
        run(&db, family, "ALTER TABLE t ADD COLUMN u UUID DEFAULT gen_random_uuid()");
        let read = values(rows(&db, family, "SELECT x, w, e, u FROM t ORDER BY id"));
        assert_eq!(read.len(), 3, "[{family:?}]");
        let mut uuids: Vec<String> = read.iter().map(|row| format!("{:?}", row[3])).collect();
        uuids.sort();
        uuids.dedup();
        assert_eq!(uuids.len(), 3, "[{family:?}] a volatile default, per row: {read:?}");
        for row in &read {
            assert_eq!(
                &row[..3],
                &[Value::Int4(5), Value::Int4(7), Value::Int4(2)],
                "[{family:?}]"
            );
        }
        // NOT NULL with nothing to give the rows the branch sees.
        let refused = match family {
            Family::Text => db.execute("ALTER TABLE t ADD COLUMN nn INT NOT NULL"),
            Family::Params => db.execute_params("ALTER TABLE t ADD COLUMN nn INT NOT NULL", &[]),
        };
        assert!(
            refused
                .unwrap_err()
                .to_string()
                .contains("violates not-null constraint"),
            "[{family:?}]"
        );
        db.execute("USE BRANCH main").unwrap();
        assert_eq!(
            rows(&db, family, "SELECT * FROM t WHERE id = 1")[0].values.len(),
            6,
            "[{family:?}] main's definition is its own"
        );
    }
}

/// B3's MERGE: `AddColumnsOnMain` is the schema and the missing value in one
/// group. Main's rows are not rewritten and read the branch's value at once; the
/// row the branch changed lands by its own commit.
#[test]
fn merge_adds_the_column_by_missing_value_and_rewrites_no_row_of_main() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE m (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("INSERT INTO m VALUES (1, 10), (2, 20), (3, 30)").unwrap();
        db.execute("CREATE BRANCH r AS OF NOW").unwrap();
        db.execute("USE BRANCH r").unwrap();
        run(&db, family, "ALTER TABLE m ADD COLUMN c TEXT NOT NULL DEFAULT 'USD'");
        run(&db, family, "UPDATE m SET c = 'EUR' WHERE id = 2");
        db.execute("USE BRANCH main").unwrap();
        db.execute("MERGE BRANCH r INTO main").unwrap();

        assert!(key_exists(&db, "meta:attmissing:m:c"), "[{family:?}]");
        assert_eq!(
            stored_widths(&db, "m"),
            vec![2, 2, 3],
            "[{family:?}] only the row the branch changed landed"
        );
        assert_eq!(
            values(rows(&db, family, "SELECT id, c FROM m ORDER BY id")),
            vec![
                vec![Value::Int4(1), s("USD")],
                vec![Value::Int4(2), s("EUR")],
                vec![Value::Int4(3), s("USD")],
            ],
            "[{family:?}]"
        );
        assert_eq!(
            values(rows(&db, family, "SELECT c FROM m WHERE id = 3")),
            vec![vec![s("USD")]],
            "[{family:?}]"
        );
    }
}

/// A volatile default can have no one value: the merge's step rewrites main's rows
/// in the schema's group, one value each.
#[test]
fn merge_of_a_volatile_default_gives_mains_rows_one_value_each() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE m (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO m VALUES (1), (2)").unwrap();
    db.execute("CREATE BRANCH r AS OF NOW").unwrap();
    db.execute("USE BRANCH r").unwrap();
    db.execute("ALTER TABLE m ADD COLUMN u UUID DEFAULT gen_random_uuid()")
        .unwrap();
    db.execute("USE BRANCH main").unwrap();
    db.execute("MERGE BRANCH r INTO main").unwrap();
    assert_eq!(stored_widths(&db, "m"), vec![2, 2]);
    let read = values(db.query("SELECT u FROM m ORDER BY id", &[]).unwrap());
    assert_ne!(read[0], read[1]);
    assert!(read.iter().all(|row| row[0] != Value::Null));
    db.execute("BEGIN").unwrap();
    assert_eq!(
        values(db.query("SELECT u FROM m ORDER BY id", &[]).unwrap()),
        read,
        "a snapshot-resolved read finds the merge's rewrite"
    );
    db.execute("COMMIT").unwrap();
}

/// A branch's NOT NULL column is judged against what main will hold. It used to be
/// judged against main's rows before the branch's deletes landed, after the merge
/// had begun, and the merge could neither finish nor be rerun.
#[test]
fn merge_judges_not_null_against_what_main_will_hold() {
    let branch_replaces_every_row = |db: &EmbeddedDatabase| {
        db.execute("CREATE TABLE a9 (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO a9 VALUES (1)").unwrap();
        db.execute("CREATE BRANCH r AS OF NOW").unwrap();
        db.execute("USE BRANCH r").unwrap();
        db.execute("DELETE FROM a9").unwrap();
        db.execute("ALTER TABLE a9 ADD COLUMN c INT NOT NULL").unwrap();
        db.execute("INSERT INTO a9 VALUES (2, 2)").unwrap();
        db.execute("USE BRANCH main").unwrap();
    };

    // Every row of main goes: legal.
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    branch_replaces_every_row(&db);
    db.execute("MERGE BRANCH r INTO main")
        .unwrap_or_else(|e| panic!("the merged result is legal: {e}"));
    assert_eq!(
        values(db.query("SELECT * FROM a9", &[]).unwrap()),
        vec![vec![Value::Int4(2), Value::Int4(2)]]
    );
    db.execute("CREATE BRANCH next AS OF NOW")
        .unwrap_or_else(|e| panic!("no merge is left pending: {e}"));

    // A row main gained itself stays, with nothing to give the column: refused
    // while the merge has changed nothing, and nothing is left pending.
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    branch_replaces_every_row(&db);
    db.execute("INSERT INTO a9 VALUES (9)").unwrap();
    let err = db.execute("MERGE BRANCH r INTO main").unwrap_err().to_string();
    assert!(err.contains("violates not-null constraint"), "{err}");
    assert!(!err.contains("began changing main"), "{err}");
    assert_eq!(
        values(db.query("SELECT * FROM a9 ORDER BY id", &[]).unwrap()),
        vec![vec![Value::Int4(1)], vec![Value::Int4(9)]],
        "main did not move"
    );
    assert_eq!(db.storage.catalog().get_table_schema("a9").unwrap().columns.len(), 1);
    db.execute("CREATE BRANCH next AS OF NOW")
        .unwrap_or_else(|e| panic!("no merge is left pending: {e}"));

    // Without that row it merges.
    db.execute("DELETE FROM a9 WHERE id = 9").unwrap();
    db.execute("MERGE BRANCH r INTO main")
        .unwrap_or_else(|e| panic!("legal now: {e}"));
    assert_eq!(
        values(db.query("SELECT * FROM a9", &[]).unwrap()),
        vec![vec![Value::Int4(2), Value::Int4(2)]]
    );
}

// ---------------------------------------------------------------------------
// Own writes of one explicit transaction
// ---------------------------------------------------------------------------

#[test]
fn rows_a_transaction_staged_before_add_column_read_right_at_once_and_after_commit() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE s (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("INSERT INTO s VALUES (1, 'committed')").unwrap();
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO s VALUES (2, 'staged')").unwrap();
        db.execute("UPDATE s SET v = 'changed' WHERE id = 1").unwrap();
        run(&db, family, "ALTER TABLE s ADD COLUMN x INT DEFAULT 5");

        let want = vec![
            vec![Value::Int4(1), s("changed"), Value::Int4(5)],
            vec![Value::Int4(2), s("staged"), Value::Int4(5)],
        ];
        let check = |db: &EmbeddedDatabase, when: &str| {
            assert_eq!(
                values(db.query("SELECT * FROM s ORDER BY id", &[]).unwrap()),
                want,
                "{when}: the scan"
            );
            assert_eq!(
                values(db.query("SELECT * FROM s WHERE id = 2", &[]).unwrap()),
                vec![want[1].clone()],
                "{when}: the point read"
            );
            assert_eq!(
                int(&db.query("SELECT SUM(x) FROM s", &[]).unwrap()[0].values[0]),
                10,
                "{when}: an aggregate"
            );
            assert_eq!(
                db.query("SELECT id FROM s WHERE x = 5", &[]).unwrap().len(),
                2,
                "{when}: a filter"
            );
        };
        check(&db, "inside the transaction");
        db.execute("COMMIT").unwrap();
        check(&db, "after COMMIT");
        assert_eq!(
            stored_widths(&db, "s"),
            vec![3, 3],
            "what a transaction commits is as wide as the table it commits into"
        );
    }
}

/// A volatile default that refuses on the transaction's own rows used to refuse
/// after the schema was durable, and the column stayed. Their values are
/// evaluated before anything is written, single clause and multi-clause, and
/// once: a sequence is not consumed twice by a single clause.
#[test]
fn a_volatile_add_column_refused_by_the_transactions_own_rows_leaves_nothing() {
    let attempt = |db: &EmbeddedDatabase, family: Family, sql: &str| match family {
        Family::Text => db.execute(sql),
        Family::Params => db.execute_params(sql, &[]),
    };
    for family in FAMILIES {
        for sql in [
            "ALTER TABLE a6 ADD COLUMN c INT NOT NULL DEFAULT (CASE WHEN random() > 2 THEN 1 END)",
            "ALTER TABLE a6 ADD COLUMN c BIGINT DEFAULT 1 / (nextval('s7') - 2)",
            "ALTER TABLE a6 ADD COLUMN x INT DEFAULT 1, \
             ADD COLUMN c INT NOT NULL DEFAULT (CASE WHEN random() > 2 THEN 1 END)",
        ] {
            let db = EmbeddedDatabase::new_in_memory().unwrap();
            db.execute("CREATE TABLE a6 (id INT PRIMARY KEY)").unwrap();
            db.execute("CREATE SEQUENCE s7").unwrap();
            db.execute("BEGIN").unwrap();
            db.execute("INSERT INTO a6 VALUES (1), (2)").unwrap();
            attempt(&db, family, sql).expect_err(sql);
            db.execute("ROLLBACK").ok();
            assert_eq!(
                db.storage.catalog().get_table_schema("a6").unwrap().columns.len(),
                1,
                "[{family:?}] {sql}: no column stayed"
            );
        }

        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE b (id INT PRIMARY KEY)").unwrap();
        db.execute("CREATE SEQUENCE s8").unwrap();
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO b VALUES (1), (2)").unwrap();
        run(&db, family, "ALTER TABLE b ADD COLUMN q BIGINT DEFAULT nextval('s8')");
        db.execute("COMMIT").unwrap();
        let mut read: Vec<i64> = db
            .query("SELECT q FROM b", &[])
            .unwrap()
            .iter()
            .map(|row| int(&row.values[0]))
            .collect();
        read.sort_unstable();
        assert_eq!(read, vec![1, 2], "[{family:?}] one value per row, consumed once");
    }
}

/// A constraint is validated and indexed over committed rows, so over a
/// transaction's own rows it used to be accepted unseen: `ADD k INT PRIMARY KEY
/// DEFAULT 5` over two staged rows gave two rows with k = 5, and `WHERE k = 5` found
/// neither. Refused, on each family, with nothing left behind, for every spelling,
/// over an image or a delete.
#[test]
fn a_constraint_over_the_transactions_own_rows_is_refused() {
    let attempt = |db: &EmbeddedDatabase, family: Family, sql: &str| match family {
        Family::Text => db.execute(sql),
        Family::Params => db.execute_params(sql, &[]),
    };
    let statements = [
        "ALTER TABLE s ADD COLUMN k INT PRIMARY KEY DEFAULT 5",
        "ALTER TABLE s ADD COLUMN u INT UNIQUE DEFAULT 5",
        "ALTER TABLE s ADD COLUMN c INT DEFAULT -1 CHECK (c > 0)",
        "ALTER TABLE s ADD COLUMN p INT DEFAULT 99 REFERENCES par(id)",
        "ALTER TABLE s ADD CONSTRAINT s_v_key UNIQUE (v)",
        "ALTER TABLE s ADD CONSTRAINT s_v_check CHECK (v > 0)",
        "ALTER TABLE s ADD CONSTRAINT s_v_fkey FOREIGN KEY (v) REFERENCES par(id)",
    ];
    for family in FAMILIES {
        for staged in [
            "INSERT INTO s VALUES (2, 2), (3, 2)",
            "UPDATE s SET v = -2 WHERE id = 1",
            "DELETE FROM s WHERE id = 1",
        ] {
            for sql in statements {
                let db = EmbeddedDatabase::new_in_memory().unwrap();
                db.execute("CREATE TABLE par (id INT PRIMARY KEY)").unwrap();
                db.execute("CREATE TABLE s (id INT, v INT)").unwrap();
                db.execute("INSERT INTO s VALUES (1, 1)").unwrap();
                db.execute("BEGIN").unwrap();
                db.execute(staged).unwrap();
                let err = attempt(&db, family, sql).expect_err(sql).to_string();
                assert!(
                    err.contains(heliosdb_nano::DDL_OVER_STAGED_ROWS_MARKER),
                    "[{family:?}] {staged}; {sql}: {err}"
                );
                assert!(
                    err.contains("discarded") && err.contains("in a transaction of their own"),
                    "[{family:?}] says what a COMMIT after it does: {err}"
                );
                db.execute("ROLLBACK").ok();
                let catalog = db.storage.catalog();
                assert_eq!(
                    catalog.get_table_schema("s").unwrap().columns.len(),
                    2,
                    "[{family:?}] {staged}; {sql}: no column stayed"
                );
            }
        }
    }

    // A plain ADD COLUMN over them keeps working (GH#61), and with no staged change
    // of this table a constraint in a transaction is what it always was.
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE s (id INT, v INT)").unwrap();
    db.execute("CREATE TABLE other (id INT)").unwrap();
    db.execute("INSERT INTO s VALUES (1, 1)").unwrap();
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO other VALUES (1)").unwrap();
    db.execute("ALTER TABLE s ADD CONSTRAINT s_v_key UNIQUE (v)").unwrap();
    db.execute("INSERT INTO s VALUES (2, 2)").unwrap();
    db.execute("ALTER TABLE s ADD COLUMN x INT DEFAULT 5").unwrap();
    db.execute("COMMIT").unwrap();
    assert_eq!(
        values(db.query("SELECT id, x FROM s ORDER BY id", &[]).unwrap()),
        vec![
            vec![Value::Int4(1), Value::Int4(5)],
            vec![Value::Int4(2), Value::Int4(5)],
        ]
    );
}

fn ids(db: &EmbeddedDatabase, sql: &str) -> Vec<i64> {
    let mut read: Vec<i64> = db
        .query(sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}`: {e}"))
        .iter()
        .map(|row| int(&row.values[0]))
        .collect();
    read.sort_unstable();
    read
}

/// An index built in a transaction used to hold only the committed rows, so after
/// COMMIT `WHERE c = 5` missed what the scan showed. It takes the transaction's own
/// rows through the funnel, and a ROLLBACK leaves it over the committed ones.
#[test]
fn an_index_built_in_a_transaction_finds_its_own_rows() {
    for family in FAMILIES {
        let build = |db: &EmbeddedDatabase, sql: &str| {
            match family {
                Family::Text => db.execute(sql),
                Family::Params => db.execute_params(sql, &[]),
            }
            .unwrap_or_else(|e| panic!("[{family:?}] `{sql}`: {e}"))
        };
        let fresh = || {
            let db = EmbeddedDatabase::new_in_memory().unwrap();
            db.execute("CREATE TABLE s (id INT PRIMARY KEY, v TEXT)").unwrap();
            db.execute("INSERT INTO s VALUES (1, 'a'), (4, 'd')").unwrap();
            db
        };

        // A row only the transaction has, narrower than the column indexed.
        for commit in [true, false] {
            let db = fresh();
            db.execute("BEGIN").unwrap();
            db.execute("INSERT INTO s VALUES (2, 'b')").unwrap();
            db.execute("ALTER TABLE s ADD COLUMN c INT DEFAULT 5").unwrap();
            build(&db, "CREATE INDEX s_c ON s (c)");
            if commit {
                db.execute("COMMIT").unwrap();
                assert_eq!(ids(&db, "SELECT id FROM s WHERE c = 5"), vec![1, 2, 4], "[{family:?}]");
            } else {
                db.execute("ROLLBACK").unwrap();
                assert_eq!(ids(&db, "SELECT id FROM s WHERE c = 5"), vec![1, 4], "[{family:?}]");
                // No ghost: the row id is free, and the row comes back once.
                db.execute("INSERT INTO s (id, v) VALUES (2, 'b')").unwrap();
                assert_eq!(ids(&db, "SELECT id FROM s WHERE c = 5"), vec![1, 2, 4], "[{family:?}]");
            }
        }

        // A committed row the transaction changed, and one it deleted.
        for commit in [true, false] {
            let db = fresh();
            db.execute("BEGIN").unwrap();
            db.execute("UPDATE s SET v = 'z' WHERE id = 1").unwrap();
            db.execute("DELETE FROM s WHERE id = 4").unwrap();
            build(&db, "CREATE INDEX s_v ON s (v)");
            if commit {
                db.execute("COMMIT").unwrap();
                assert_eq!(ids(&db, "SELECT id FROM s WHERE v = 'z'"), vec![1], "[{family:?}]");
                assert!(ids(&db, "SELECT id FROM s WHERE v = 'a'").is_empty(), "[{family:?}]");
                assert!(ids(&db, "SELECT id FROM s WHERE v = 'd'").is_empty(), "[{family:?}]");
            } else {
                db.execute("ROLLBACK").unwrap();
                assert!(ids(&db, "SELECT id FROM s WHERE v = 'z'").is_empty(), "[{family:?}]");
                assert_eq!(ids(&db, "SELECT id FROM s WHERE v = 'a'"), vec![1], "[{family:?}]");
                assert_eq!(ids(&db, "SELECT id FROM s WHERE v = 'd'"), vec![4], "[{family:?}]");
            }
        }

        // UNIQUE sees them: two staged rows of one value leave no index.
        let db = fresh();
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO s VALUES (2, 'x'), (3, 'x')").unwrap();
        let err = match family {
            Family::Text => db.execute("CREATE UNIQUE INDEX s_vu ON s (v)"),
            Family::Params => db.execute_params("CREATE UNIQUE INDEX s_vu ON s (v)", &[]),
        }
        .expect_err("two staged rows of one value");
        assert!(
            err.to_string().to_lowercase().contains("duplicate"),
            "[{family:?}] {err}"
        );
        assert!(!db.storage.art_indexes().index_exists("s_vu"), "[{family:?}]");
        db.execute("ROLLBACK").unwrap();
        build(&db, "CREATE UNIQUE INDEX s_vu ON s (v)");

        // And from then on it refuses what they would collide with.
        let db = fresh();
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO s VALUES (2, 'x')").unwrap();
        build(&db, "CREATE UNIQUE INDEX s_vu ON s (v)");
        assert!(
            db.execute("INSERT INTO s VALUES (3, 'x')").is_err(),
            "[{family:?}] the index holds the staged row"
        );
        db.execute("ROLLBACK").ok();

        // Under a savepoint a ROLLBACK TO could bring back what it cannot reach.
        let db = fresh();
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO s VALUES (2, 'b')").unwrap();
        db.execute("SAVEPOINT sp").unwrap();
        let err = match family {
            Family::Text => db.execute("CREATE INDEX s_v ON s (v)"),
            Family::Params => db.execute_params("CREATE INDEX s_v ON s (v)", &[]),
        }
        .expect_err("under a savepoint");
        assert!(
            err.to_string().contains(heliosdb_nano::DDL_OVER_STAGED_ROWS_MARKER),
            "[{family:?}] {err}"
        );
        assert!(!db.storage.art_indexes().index_exists("s_v"), "[{family:?}]");
        db.execute("ROLLBACK").ok();
    }
}

/// The HNSW half: a row only the transaction has is found, and an index over a
/// committed row the transaction changed is refused.
#[cfg(feature = "vector-search")]
#[test]
fn an_hnsw_index_built_in_a_transaction_finds_its_own_rows() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE e (id INT PRIMARY KEY, emb VECTOR(3))")
        .unwrap();
    db.execute("INSERT INTO e VALUES (1, '[0.0, 0.0, 1.0]')").unwrap();
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO e VALUES (2, '[1.0, 0.0, 0.0]')").unwrap();
    db.execute("CREATE INDEX e_emb ON e USING hnsw (emb)").unwrap();
    db.execute("COMMIT").unwrap();
    assert_eq!(
        ids(&db, "SELECT id FROM e ORDER BY emb <-> '[1.0, 0.0, 0.0]' LIMIT 1"),
        vec![2]
    );

    db.execute("BEGIN").unwrap();
    db.execute("UPDATE e SET emb = '[0.0, 1.0, 0.0]' WHERE id = 1").unwrap();
    let err = db.execute("CREATE INDEX e_emb2 ON e USING hnsw (emb)").unwrap_err();
    assert!(
        err.to_string().contains(heliosdb_nano::DDL_OVER_STAGED_ROWS_MARKER),
        "{err}"
    );
    db.execute("ROLLBACK").ok();
}

#[test]
fn a_volatile_add_column_restages_the_transactions_rows() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE s (id INT PRIMARY KEY)").unwrap();
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO s VALUES (1), (2)").unwrap();
    db.execute("ALTER TABLE s ADD COLUMN u UUID DEFAULT gen_random_uuid()")
        .unwrap();
    let read = values(db.query("SELECT u FROM s ORDER BY id", &[]).unwrap());
    assert_ne!(read[0], read[1], "{read:?}");
    assert!(read.iter().all(|row| row[0] != Value::Null), "{read:?}");
    db.execute("COMMIT").unwrap();
    assert_eq!(stored_widths(&db, "s"), vec![2, 2]);
    assert_eq!(values(db.query("SELECT u FROM s ORDER BY id", &[]).unwrap()), read);
}

#[test]
fn not_null_add_column_is_refused_over_the_transactions_own_rows() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE s (id INT PRIMARY KEY)").unwrap();
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO s VALUES (1)").unwrap();
    let err = db.execute("ALTER TABLE s ADD COLUMN c INT NOT NULL").unwrap_err();
    assert!(err.to_string().contains("violates not-null constraint"), "{err}");
    db.execute("ROLLBACK").ok();
    assert_eq!(db.storage.catalog().get_table_schema("s").unwrap().columns.len(), 1);
}

#[test]
fn drop_column_in_a_transaction_reshapes_its_staged_rows() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE s (id INT PRIMARY KEY, mid TEXT, last INT)")
        .unwrap();
    db.execute("INSERT INTO s VALUES (1, 'committed', 10)").unwrap();
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO s VALUES (2, 'staged', 20)").unwrap();
    db.execute("UPDATE s SET last = 11 WHERE id = 1").unwrap();
    db.execute("ALTER TABLE s DROP COLUMN mid").unwrap();
    let want = vec![
        vec![Value::Int4(1), Value::Int4(11)],
        vec![Value::Int4(2), Value::Int4(20)],
    ];
    assert_eq!(values(db.query("SELECT * FROM s ORDER BY id", &[]).unwrap()), want);
    db.execute("COMMIT").unwrap();
    assert_eq!(values(db.query("SELECT * FROM s ORDER BY id", &[]).unwrap()), want);
    assert_eq!(
        values(db.query("SELECT last FROM s WHERE id = 2", &[]).unwrap()),
        vec![vec![Value::Int4(20)]],
        "the point read: the later column did not shift"
    );
    assert_eq!(stored_widths(&db, "s"), vec![2, 2]);
}

#[test]
fn drop_column_under_a_savepoint_over_own_rows_is_refused() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE s (id INT PRIMARY KEY, mid TEXT, last INT)")
        .unwrap();
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO s VALUES (1, 'staged', 20)").unwrap();
    db.execute("SAVEPOINT sp").unwrap();
    let err = db.execute("ALTER TABLE s DROP COLUMN mid").unwrap_err();
    assert!(
        err.to_string().contains(heliosdb_nano::DDL_OVER_STAGED_ROWS_MARKER),
        "{err}"
    );
    db.execute("ROLLBACK").unwrap();
    assert_eq!(db.storage.catalog().get_table_schema("s").unwrap().columns.len(), 3);
}

#[test]
fn rename_table_and_set_storage_after_writing_in_the_same_transaction_are_refused() {
    for sql in [
        "ALTER TABLE s RENAME TO s2",
        "ALTER TABLE s ALTER COLUMN v SET STORAGE DICTIONARY",
    ] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE s (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO s VALUES (1, 'staged')").unwrap();
        let err = db.execute(sql).expect_err(sql);
        assert!(
            err.to_string().contains(heliosdb_nano::DDL_OVER_STAGED_ROWS_MARKER),
            "{sql}: {err}"
        );
        db.execute("ROLLBACK").ok();
        assert!(db.query("SELECT * FROM s", &[]).unwrap().is_empty(), "{sql}");

        // A staged delete would be lost just the same: the row would come back.
        db.execute("INSERT INTO s VALUES (1, 'committed')").unwrap();
        db.execute("BEGIN").unwrap();
        db.execute("DELETE FROM s WHERE id = 1").unwrap();
        let err = db.execute(sql).expect_err(sql);
        assert!(
            err.to_string().contains(heliosdb_nano::DDL_OVER_STAGED_ROWS_MARKER),
            "{sql}, over a delete: {err}"
        );
        db.execute("ROLLBACK").ok();
        assert_eq!(db.query("SELECT * FROM s", &[]).unwrap().len(), 1, "{sql}");

        db.execute(sql)
            .unwrap_or_else(|e| panic!("{sql}, with no staged change: {e}"));
    }
}

// ---------------------------------------------------------------------------
// AS OF
// ---------------------------------------------------------------------------

fn sql_now() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

#[test]
fn as_of_before_and_after_the_alter_reads_the_missing_value() {
    let pause = || std::thread::sleep(std::time::Duration::from_millis(1200));
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE h (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO h VALUES (1, 10), (2, 20)").unwrap();
    pause();
    let before = sql_now();
    pause();
    db.execute("ALTER TABLE h ADD COLUMN x INT DEFAULT 5").unwrap();
    db.execute("UPDATE h SET v = 11, x = 6 WHERE id = 1").unwrap();
    pause();
    let after = sql_now();
    pause();

    let by_id = |sql: String| {
        let mut read = values(db.query(&sql, &[]).unwrap());
        read.sort_by_key(|row| int(&row[0]));
        read
    };
    assert_eq!(
        by_id(format!("SELECT * FROM h AS OF TIMESTAMP '{before}'")),
        vec![
            vec![Value::Int4(1), Value::Int4(10), Value::Int4(5)],
            vec![Value::Int4(2), Value::Int4(20), Value::Int4(5)],
        ],
        "versions older than the column read its missing value"
    );
    assert_eq!(
        by_id(format!("SELECT * FROM h AS OF TIMESTAMP '{after}'")),
        vec![
            vec![Value::Int4(1), Value::Int4(11), Value::Int4(6)],
            vec![Value::Int4(2), Value::Int4(20), Value::Int4(5)],
        ],
        "after: the updated row its own value, the other the missing one"
    );
}

// ---------------------------------------------------------------------------
// Restart and crash
// ---------------------------------------------------------------------------

fn persistent(dir: &Path, policy: DestructiveDdlReplay) -> Config {
    persistent_logging(dir, policy, true)
}

/// `persistent`, with `[storage] logical_wal_per_statement` as given. The shipped
/// default is `false`: a PK-fast UPDATE or DELETE then writes no logical entry.
fn persistent_logging(dir: &Path, policy: DestructiveDdlReplay, per_statement: bool) -> Config {
    let mut config = Config::default();
    config.storage.path = Some(dir.to_path_buf());
    config.storage.memory_only = false;
    config.storage.wal_enabled = true;
    config.storage.logical_wal_per_statement = per_statement;
    config.storage.wal_checkpoint_on_close = true;
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    config.storage.wal_replay_destructive_ddl = policy;
    config
}

/// Retried: a handle closed a moment ago may release the directory lock from its
/// background threads.
fn open(dir: &Path, policy: DestructiveDdlReplay) -> EmbeddedDatabase {
    open_logging(dir, policy, true)
}

fn open_logging(dir: &Path, policy: DestructiveDdlReplay, per_statement: bool) -> EmbeddedDatabase {
    let mut last = None;
    for _ in 0..100 {
        match EmbeddedDatabase::with_config(persistent_logging(dir, policy, per_statement)) {
            Ok(db) => return db,
            Err(error) => {
                last = Some(error);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
    panic!("open database: {last:?}");
}

#[test]
fn a_restart_keeps_the_values_and_the_new_shape() {
    let dir = TempDir::new().unwrap();
    {
        let db = open(dir.path(), DestructiveDdlReplay::Refuse);
        seed(&db);
    }
    let db = open(dir.path(), DestructiveDdlReplay::Refuse);
    assert_eq!(stored_widths(&db, "t"), vec![3, 3, 3]);
    assert_eq!(
        values(db.query("SELECT * FROM t WHERE id = 2", &[]).unwrap()),
        vec![full(2, "b", 20)]
    );
    assert_eq!(
        values(db.query("SELECT * FROM t WHERE n = 30", &[]).unwrap()),
        vec![full(3, "c", 30)]
    );
    assert_eq!(int(&db.query("SELECT SUM(x) FROM t", &[]).unwrap()[0].values[0]), 15);
}

/// Set by the parent on its re-exec of this binary: the directory a crash child
/// works on, and which workload.
const CRASH_DIR_ENV: &str = "HELIOS_R5_2_CRASH_DIR";

/// Re-exec THIS test binary, filtered to `test`, as the crash child: it runs its
/// workload and dies by `std::process::exit`, with no `Drop` and no close-time
/// checkpoint, so everything it did is above the checkpoint at the next open.
fn crash_child(test: &str, dir: &Path) {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg(test)
        .arg("--include-ignored")
        .arg("--nocapture")
        .env(CRASH_DIR_ENV, dir)
        .status()
        .unwrap();
    assert!(status.success(), "the crash child of {test} failed: {status:?}");
}

fn seed_a_checkpoint(dir: &Path) {
    let db = open(dir, DestructiveDdlReplay::Refuse);
    db.execute("CREATE TABLE seed0 (x INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO seed0 VALUES (1)").unwrap();
    drop(db);
}

/// The review's scenario: rows and an ADD COLUMN above the checkpoint. The redo
/// puts each row's narrow image back, and the backfill used to be lost with
/// `Column index 2 out of bounds` on the point read. It is the column's missing
/// value now, in the schema's own durable group.
#[test]
fn a_crash_right_after_the_alter_answers_the_default() {
    if let Ok(dir) = std::env::var(CRASH_DIR_ENV) {
        let db = open(Path::new(&dir), DestructiveDdlReplay::Refuse);
        db.execute("CREATE TABLE d (id INT PRIMARY KEY, v INT)").unwrap();
        let rows: Vec<String> = (1..=10).map(|id| format!("({id}, {id})")).collect();
        db.execute(&format!("INSERT INTO d VALUES {}", rows.join(", ")))
            .unwrap();
        db.execute("UPDATE d SET v = -1 WHERE id = 3").unwrap();
        db.execute("ALTER TABLE d ADD COLUMN x INT DEFAULT 5").unwrap();
        db.execute("UPDATE d SET x = 50 WHERE id = 1").unwrap();
        std::process::exit(0);
    }
    let temp = TempDir::new().unwrap();
    seed_a_checkpoint(temp.path());
    crash_child("a_crash_right_after_the_alter_answers_the_default", temp.path());

    let db = open(temp.path(), DestructiveDdlReplay::Refuse);
    assert_eq!(
        values(db.query("SELECT x FROM d WHERE id = 3", &[]).unwrap()),
        vec![vec![Value::Int4(5)]]
    );
    assert_eq!(
        values(db.query("SELECT x FROM d WHERE id = 1", &[]).unwrap()),
        vec![vec![Value::Int4(50)]]
    );
    assert_eq!(
        int(&db.query("SELECT COUNT(*) FROM d WHERE x = 5", &[]).unwrap()[0].values[0]),
        9
    );
    assert_eq!(db.query("SELECT * FROM d ORDER BY id", &[]).unwrap()[9].values.len(), 3);
}

/// Set in a crash child to the `logical_wal_per_statement` its parent runs it under.
const CRASH_PER_STATEMENT_ENV: &str = "HELIOS_R5_2_CRASH_PER_STATEMENT";

/// A crash test's database, in the child or after the crash, under the
/// `logical_wal_per_statement` the child was given (the parent passes it).
fn crash_open(dir: &Path) -> EmbeddedDatabase {
    let per_statement = std::env::var(CRASH_PER_STATEMENT_ENV).map_or(true, |value| value == "1");
    open_logging(dir, DestructiveDdlReplay::Refuse, per_statement)
}

/// Run `test`'s crash under both values of `logical_wal_per_statement`, the
/// shipped default (`false`) first, and hand each to `check` reopened. A failure
/// names the value.
fn crash_under_both_logging_modes(test: &str, check: impl Fn(&Path, &EmbeddedDatabase)) {
    for per_statement in [false, true] {
        let temp = TempDir::new().unwrap();
        {
            let db = open_logging(temp.path(), DestructiveDdlReplay::Refuse, per_statement);
            db.execute("CREATE TABLE seed0 (x INT PRIMARY KEY)").unwrap();
            db.execute("INSERT INTO seed0 VALUES (1)").unwrap();
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(test)
            .arg("--include-ignored")
            .arg("--nocapture")
            .env(CRASH_DIR_ENV, temp.path())
            .env(CRASH_PER_STATEMENT_ENV, if per_statement { "1" } else { "0" })
            .status()
            .unwrap();
        assert!(status.success(), "the crash child of {test} failed: {status:?}");
        let db = open_logging(temp.path(), DestructiveDdlReplay::Refuse, per_statement);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(temp.path(), &db)));
        if let Err(panic) = outcome {
            let what = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_string()))
                .unwrap_or_default();
            panic!("with logical_wal_per_statement = {per_statement}: {what}");
        }
    }
}

/// The answers a crash child saw, for its parent to compare with after the crash.
fn record_answers(dir: &Path, db: &EmbeddedDatabase, sql: &str) {
    let answers = format!("{:?}", values(db.query(sql, &[]).unwrap()));
    std::fs::write(dir.join("answers-before-the-crash"), answers).unwrap();
}

fn assert_same_answers(dir: &Path, db: &EmbeddedDatabase, sql: &str) {
    let before = std::fs::read_to_string(dir.join("answers-before-the-crash")).unwrap();
    let after = format!("{:?}", values(db.query(sql, &[]).unwrap()));
    assert_eq!(after, before, "`{sql}` across the crash");
}

/// Review F1. Rows written by a multi-row INSERT above the checkpoint are re-put at
/// their pre-ALTER image when the logical WAL is replayed at open, and a volatile
/// column has no missing value to fall back on: a completed ALTER's values read NULL.
#[test]
fn a_crash_after_a_volatile_add_column_keeps_its_values() {
    if let Ok(dir) = std::env::var(CRASH_DIR_ENV) {
        let db = crash_open(Path::new(&dir));
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 1), (2, 2), (3, 3)").unwrap();
        db.execute("ALTER TABLE t ADD COLUMN c TEXT NOT NULL DEFAULT gen_random_uuid()::text")
            .unwrap();
        record_answers(Path::new(&dir), &db, "SELECT * FROM t ORDER BY id");
        std::process::exit(0);
    }
    crash_under_both_logging_modes("a_crash_after_a_volatile_add_column_keeps_its_values", |dir, db| {
        assert_same_answers(dir, db, "SELECT * FROM t ORDER BY id");
        assert_eq!(
            int(&db.query("SELECT COUNT(c) FROM t", &[]).unwrap()[0].values[0]),
            3,
            "the column holds a value in every row"
        );
    });
}

/// Review F1, with a sequence: its values are consumed, so they cannot come back any
/// other way than from what the ALTER wrote.
#[test]
fn a_crash_after_a_nextval_add_column_keeps_its_values() {
    if let Ok(dir) = std::env::var(CRASH_DIR_ENV) {
        let db = crash_open(Path::new(&dir));
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("CREATE SEQUENCE t_s").unwrap();
        db.execute("INSERT INTO t VALUES (1, 1), (2, 2), (3, 3)").unwrap();
        db.execute("ALTER TABLE t ADD COLUMN s BIGINT DEFAULT nextval('t_s')")
            .unwrap();
        record_answers(Path::new(&dir), &db, "SELECT id, s FROM t ORDER BY id");
        std::process::exit(0);
    }
    crash_under_both_logging_modes("a_crash_after_a_nextval_add_column_keeps_its_values", |dir, db| {
        assert_same_answers(dir, db, "SELECT id, s FROM t ORDER BY id");
    });
}

/// Review P1, with no ALTER at all: under the shipped `logical_wal_per_statement =
/// false` a committed PK-fast UPDATE and DELETE leave no log entry, and the replay
/// of the INSERT's images undoes them. (Under `true` they are logged, and this
/// passes.)
#[test]
fn a_crash_keeps_committed_primary_key_updates_and_deletes() {
    if let Ok(dir) = std::env::var(CRASH_DIR_ENV) {
        let db = crash_open(Path::new(&dir));
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 1), (2, 2), (3, 3)").unwrap();
        db.execute("UPDATE t SET v = 6 WHERE id = 1").unwrap();
        db.execute("DELETE FROM t WHERE id = 2").unwrap();
        std::process::exit(0);
    }
    crash_under_both_logging_modes("a_crash_keeps_committed_primary_key_updates_and_deletes", |dir, db| {
        let _ = dir;
        assert_eq!(
            values(db.query("SELECT * FROM t ORDER BY id", &[]).unwrap()),
            vec![
                vec![Value::Int4(1), Value::Int4(6)],
                vec![Value::Int4(3), Value::Int4(3)],
            ]
        );
    });
}

/// Review P3, the crash half. `DROP COLUMN` of a middle column rewrites the rows,
/// and the replay of the images logged before it put the dropped column's values
/// back under the next column's name.
///
/// Spec R5.5 made the DROP one write group (schema, every row's new image and its
/// version, `tests/statement_atomicity_alter.rs`), so no crash falls inside it. What
/// is left is the replay at open: the INSERT above the checkpoint is logged with its
/// three-column images, which the replay puts back over the DROP's narrow rows, and
/// nothing in the log narrows them again. That is decision 14's (spec R5.7: open adopts,
/// never redoes), so this stays ignored until then.
#[test]
fn a_crash_after_drop_column_keeps_the_other_columns_values() {
    if let Ok(dir) = std::env::var(CRASH_DIR_ENV) {
        let db = crash_open(Path::new(&dir));
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, x INT, c INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 10, 20), (2, 11, 21)").unwrap();
        db.execute("ALTER TABLE t DROP COLUMN x").unwrap();
        std::process::exit(0);
    }
    crash_under_both_logging_modes("a_crash_after_drop_column_keeps_the_other_columns_values", |dir, db| {
        let _ = dir;
        assert_eq!(
            values(db.query("SELECT * FROM t ORDER BY id", &[]).unwrap()),
            vec![
                vec![Value::Int4(1), Value::Int4(20)],
                vec![Value::Int4(2), Value::Int4(21)],
            ]
        );
    });
}

/// Review P3, the crash half, with a column dropped and added again under its name
/// and another one dropped after it: the last definition's missing value answers.
///
/// Ignored for the same reason as the test above: each statement is one group since
/// spec R5.5, and the replay of the logged INSERT images at open (spec R5.7) is what
/// still undoes them.
#[test]
fn a_crash_after_a_drop_and_readd_keeps_the_last_definition() {
    if let Ok(dir) = std::env::var(CRASH_DIR_ENV) {
        let db = crash_open(Path::new(&dir));
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 1), (2, 2), (3, 3)").unwrap();
        db.execute("ALTER TABLE t ADD COLUMN c INT DEFAULT 5").unwrap();
        db.execute("ALTER TABLE t DROP COLUMN c").unwrap();
        db.execute("ALTER TABLE t ADD COLUMN c INT DEFAULT 6").unwrap();
        db.execute("ALTER TABLE t DROP COLUMN v").unwrap();
        std::process::exit(0);
    }
    crash_under_both_logging_modes("a_crash_after_a_drop_and_readd_keeps_the_last_definition", |dir, db| {
        let _ = dir;
        assert_eq!(
            values(db.query("SELECT * FROM t ORDER BY id", &[]).unwrap()),
            vec![
                vec![Value::Int4(1), Value::Int4(6)],
                vec![Value::Int4(2), Value::Int4(6)],
                vec![Value::Int4(3), Value::Int4(6)],
            ]
        );
    });
}

/// DROP + CREATE + ADD COLUMN above the checkpoint: under the policy that replays
/// the DROP, its purge takes the key the crash left, and only the logical entry
/// `SetColumnMissing` can bring it back; under the default the store stands.
#[test]
fn a_replayed_drop_and_recreate_ends_with_the_primarys_missing_value() {
    if let Ok(dir) = std::env::var(CRASH_DIR_ENV) {
        let db = open(Path::new(&dir), DestructiveDdlReplay::Refuse);
        db.execute("DROP TABLE seed0").unwrap();
        db.execute("CREATE TABLE seed0 (x INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO seed0 VALUES (1), (2)").unwrap();
        db.execute("ALTER TABLE seed0 ADD COLUMN m INT DEFAULT 7").unwrap();
        std::process::exit(0);
    }
    for policy in [DestructiveDdlReplay::Refuse, DestructiveDdlReplay::Apply] {
        let temp = TempDir::new().unwrap();
        seed_a_checkpoint(temp.path());
        crash_child(
            "a_replayed_drop_and_recreate_ends_with_the_primarys_missing_value",
            temp.path(),
        );
        let db = open(temp.path(), policy);
        assert_eq!(
            values(db.query("SELECT m FROM seed0 WHERE x = 2", &[]).unwrap()),
            vec![vec![Value::Int4(7)]],
            "{policy:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// REST
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_rest_query_route_answers_the_missing_value() {
    use axum::extract::{Path as AxumPath, State};
    use axum::Json;
    use heliosdb_nano::api::handlers::query_handler::execute_query;

    let db = Arc::new(seeded());
    let state = heliosdb_nano::api::server::AppState {
        db,
        query_registry: Arc::new(heliosdb_nano::compute::QueryRegistry::new()),
        auth_bridge: None,
        oauth_registry: None,
        change_notifier: None,
    };
    // This route keys a row's values `column_N`, by position.
    for (sql, first) in [
        ("SELECT id, x, y, z FROM t WHERE id = 2", 1),
        ("SELECT * FROM t WHERE n = 20", 3),
    ] {
        let request = heliosdb_nano::api::models::query::QueryRequest {
            sql: sql.to_string(),
            params: None,
            as_of: None,
            timeout_ms: None,
        };
        let Json(response) = execute_query(State(state.clone()), AxumPath("main".to_string()), Json(request))
            .await
            .ok()
            .unwrap_or_else(|| panic!("REST `{sql}` failed"));
        assert_eq!(response.rows.len(), 1, "{sql}");
        let row = &response.rows[0];
        assert_eq!(
            row.get(&format!("column_{first}")).and_then(serde_json::Value::as_i64),
            Some(5),
            "{sql}: {row:?}"
        );
        assert_eq!(
            row.get(&format!("column_{}", first + 1))
                .and_then(serde_json::Value::as_str),
            Some("usd"),
            "{sql}: {row:?}"
        );
        assert!(
            row.get(&format!("column_{}", first + 2))
                .is_some_and(serde_json::Value::is_null),
            "{sql}: {row:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// MySQL wire
// ---------------------------------------------------------------------------

const COM_QUERY: u8 = 0x03;
const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;

struct MySqlClient {
    stream: DuplexStream,
}

fn read_lenenc(buf: &[u8], pos: &mut usize) -> usize {
    let first = buf[*pos];
    *pos += 1;
    match first {
        0xFC => {
            let value = usize::from(u16::from_le_bytes([buf[*pos], buf[*pos + 1]]));
            *pos += 2;
            value
        }
        value => usize::from(value),
    }
}

fn read_lenenc_text(buf: &[u8], pos: &mut usize) -> String {
    let len = read_lenenc(buf, pos);
    let text = String::from_utf8_lossy(&buf[*pos..*pos + len]).into_owned();
    *pos += len;
    text
}

impl MySqlClient {
    async fn login(db: Arc<EmbeddedDatabase>) -> Self {
        let (client, server) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move {
            let _ = MySqlHandler::handle_connection_with_timeouts(db, server, 1, ConnectionTimeouts::disabled()).await;
        });
        let mut this = Self { stream: client };
        let _greeting = this.read_packet().await;
        let mut login = Vec::new();
        login.extend_from_slice(&(CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION).to_le_bytes());
        login.extend_from_slice(&(1u32 << 24).to_le_bytes());
        login.push(45);
        login.extend_from_slice(&[0u8; 23]);
        login.extend_from_slice(b"root");
        login.push(0);
        login.push(0);
        this.write_packet(1, &login).await;
        let ok = this.read_packet().await;
        assert_eq!(ok[0], 0x00, "login: {ok:?}");
        this
    }

    async fn write_packet(&mut self, seq: u8, payload: &[u8]) {
        let len = payload.len();
        let header = [
            (len & 0xFF) as u8,
            ((len >> 8) & 0xFF) as u8,
            ((len >> 16) & 0xFF) as u8,
            seq,
        ];
        self.stream.write_all(&header).await.unwrap();
        self.stream.write_all(payload).await.unwrap();
        self.stream.flush().await.unwrap();
    }

    async fn read_packet(&mut self) -> Vec<u8> {
        let mut header = [0u8; 4];
        self.stream.read_exact(&mut header).await.unwrap();
        let len = usize::from(header[0]) | (usize::from(header[1]) << 8) | (usize::from(header[2]) << 16);
        let mut payload = vec![0u8; len];
        self.stream.read_exact(&mut payload).await.unwrap();
        payload
    }

    /// A text result set: NULL decodes as `None`.
    async fn query(&mut self, sql: &str) -> Vec<Vec<Option<String>>> {
        let mut request = vec![COM_QUERY];
        request.extend_from_slice(sql.as_bytes());
        self.write_packet(0, &request).await;
        let first = self.read_packet().await;
        assert_ne!(first[0], 0xFF, "`{sql}`: {}", String::from_utf8_lossy(&first));
        let mut pos = 0;
        let columns = read_lenenc(&first, &mut pos);
        for _ in 0..columns {
            self.read_packet().await;
        }
        let eof = self.read_packet().await;
        assert_eq!(eof[0], 0xFE, "`{sql}`");
        let mut rows = Vec::new();
        loop {
            let packet = self.read_packet().await;
            if packet[0] == 0xFE && packet.len() < 9 {
                return rows;
            }
            let mut pos = 0;
            let mut row = Vec::with_capacity(columns);
            for _ in 0..columns {
                if packet[pos] == 0xFB {
                    pos += 1;
                    row.push(None);
                } else {
                    row.push(Some(read_lenenc_text(&packet, &mut pos)));
                }
            }
            assert_eq!(pos, packet.len(), "`{sql}`: the row is exactly as wide as its columns");
            rows.push(row);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mysql_wire_answers_the_missing_value() {
    let db = Arc::new(seeded());
    let mut client = MySqlClient::login(db).await;
    let some = |text: &str| Some(text.to_string());
    for sql in ["SELECT * FROM t WHERE id = 2", "SELECT * FROM t WHERE n = 20"] {
        assert_eq!(
            client.query(sql).await,
            vec![vec![some("2"), some("b"), some("20"), some("5"), some("usd"), None]],
            "{sql}"
        );
    }
    assert_eq!(client.query("SELECT * FROM t ORDER BY id").await.len(), 3);
}
