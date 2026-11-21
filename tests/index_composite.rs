//! Composite indexes: an ordinary multi-column index holds every column.
//!
//! Before: `CREATE INDEX ix ON t (a, b)` indexed `a` alone; `b` was dropped
//! without a word, `pg_indexes` showed `(a)`, and a query on both columns read
//! every row with that `a`. Now the index key is `(a, b)`: equality on a
//! leading run of its columns, equality plus a range on the next column, and
//! `ORDER BY` on its columns with a `LIMIT` read through it, and `EXPLAIN`
//! names it with the condition it serves.
//!
//! Every answer is compared with the same query on a twin table that has no
//! index. Both statement families; the main cases over the PostgreSQL wire; a
//! reopen; a dump/restore round trip in both formats.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

mod index_forms_common;

use std::sync::Arc;

use heliosdb_nano::{EmbeddedDatabase, Value};
use index_forms_common::*;

/// `t` (indexed) and `twin` (no index), with the same 240 rows:
/// `a` in 0..12, `b` in 0..20 (each `(a, b)` once), `c` a short text.
fn build(db: &EmbeddedDatabase, params: bool) {
    for table in ["t", "twin"] {
        must(
            db,
            &format!("CREATE TABLE {table} (id INT PRIMARY KEY, a INT NOT NULL, b INT NOT NULL, c TEXT, n INT)"),
            params,
        );
        let mut values = Vec::new();
        for id in 0..240 {
            let (a, b) = (id % 12, id / 12);
            let n = if id % 7 == 0 {
                "NULL".to_string()
            } else {
                (id % 5).to_string()
            };
            values.push(format!("({id}, {a}, {b}, 'c{}', {n})", id % 9));
        }
        must(db, &format!("INSERT INTO {table} VALUES {}", values.join(", ")), params);
    }
    must(db, "CREATE INDEX ix_ab ON t (a, b)", params);
}

/// The same question to the indexed table and to its twin.
fn same_as_twin(db: &EmbeddedDatabase, tail: &str, params: bool) -> Vec<i64> {
    let indexed = id_set(db, &format!("SELECT id FROM t WHERE {tail}"), params);
    let twin = id_set(db, &format!("SELECT id FROM twin WHERE {tail}"), params);
    assert_eq!(indexed, twin, "`{tail}` (params={params})");
    indexed
}

#[test]
fn every_column_of_the_key_is_indexed() {
    for params in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        build(&db, params);

        // Equality on the whole key: one probe.
        let p = plan(&db, "SELECT id FROM t WHERE a = 3 AND b = 7", params);
        assert!(p.contains("Index Scan using ix_ab on t (a = 3 AND b = 7)"), "{p}");
        assert_eq!(same_as_twin(&db, "a = 3 AND b = 7", params), vec![87]);

        // Equality on a prefix.
        let p = plan(&db, "SELECT id FROM t WHERE a = 3", params);
        assert!(p.contains("Index Scan using ix_ab on t (a = 3)"), "{p}");
        assert_eq!(same_as_twin(&db, "a = 3", params).len(), 20);

        // Equality on the prefix and a range on the next column.
        let p = plan(&db, "SELECT id FROM t WHERE a = 3 AND b >= 5 AND b < 9", params);
        assert!(
            p.contains("Index Scan using ix_ab on t (a = 3 AND b >= 5 AND b < 9)"),
            "{p}"
        );
        assert_eq!(same_as_twin(&db, "a = 3 AND b >= 5 AND b < 9", params).len(), 4);
        assert_eq!(same_as_twin(&db, "a = 3 AND b BETWEEN 18 AND 40", params).len(), 2);
        assert_eq!(same_as_twin(&db, "b > 17 AND 3 = a", params).len(), 2);

        // A further condition is still applied to what the index hands back.
        assert_eq!(
            same_as_twin(&db, "a = 3 AND b = 7 AND c = 'nope'", params),
            Vec::<i64>::new()
        );
        assert_eq!(same_as_twin(&db, "a = 3 AND n IS NULL", params).len(), 3);

        // The second column alone is not a prefix: the index is not used.
        let p = plan(&db, "SELECT id FROM t WHERE b = 7", params);
        assert!(!p.contains("ix_ab"), "{p}");
        assert_eq!(same_as_twin(&db, "b = 7", params).len(), 12);

        // The twin has no index to name.
        let p = plan(&db, "SELECT id FROM twin WHERE a = 3 AND b = 7", params);
        assert!(!p.contains("Index Scan"), "{p}");
    }
}

#[test]
fn bound_parameters_read_through_it() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    build(&db, true);
    let sql = "SELECT id FROM t WHERE a = $1 AND b >= $2";
    let bound = [Value::Int4(5), Value::Int4(18)];
    let p = plan_bound(&db, sql, &bound);
    assert!(p.contains("Index Scan using ix_ab on t (a = 5 AND b >= 18)"), "{p}");
    assert_eq!(
        id_set_bound(&db, sql, &bound),
        id_set_bound(&db, "SELECT id FROM twin WHERE a = $1 AND b >= $2", &bound)
    );
    assert_eq!(id_set_bound(&db, sql, &bound), vec![221, 233]);
    // The same statement with other values: another part of the index.
    let bound = [Value::Int4(0), Value::Int4(19)];
    assert_eq!(id_set_bound(&db, sql, &bound), vec![228]);
}

#[test]
fn writes_keep_it_right() {
    for params in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        build(&db, params);
        for table in ["t", "twin"] {
            must(
                &db,
                &format!("INSERT INTO {table} VALUES (1000, 3, 100, 'new', 1)"),
                params,
            );
            // The second key column moves: the row leaves one key and enters another.
            must(&db, &format!("UPDATE {table} SET b = 200 WHERE id = 87"), params);
            // The first key column moves.
            must(&db, &format!("UPDATE {table} SET a = 3 WHERE id = 4"), params);
            must(&db, &format!("DELETE FROM {table} WHERE id = 3"), params);
        }
        assert_eq!(same_as_twin(&db, "a = 3 AND b = 7", params), Vec::<i64>::new());
        assert_eq!(same_as_twin(&db, "a = 3 AND b = 200", params), vec![87]);
        assert_eq!(same_as_twin(&db, "a = 3 AND b >= 100", params), vec![87, 1000]);
        assert_eq!(same_as_twin(&db, "a = 3 AND b = 0", params), vec![4]);
        assert_eq!(same_as_twin(&db, "a = 4 AND b = 0", params), Vec::<i64>::new());
        assert_eq!(same_as_twin(&db, "a = 3", params).len(), 21);

        // A rolled-back transaction leaves the index as it was.
        for table in ["t", "twin"] {
            must(&db, "BEGIN", params);
            must(
                &db,
                &format!("INSERT INTO {table} VALUES (2000, 3, 300, 'gone', 1)"),
                params,
            );
            must(&db, &format!("UPDATE {table} SET b = 301 WHERE id = 1000"), params);
            must(&db, &format!("DELETE FROM {table} WHERE id = 87"), params);
            must(&db, "ROLLBACK", params);
        }
        assert_eq!(same_as_twin(&db, "a = 3 AND b >= 100", params), vec![87, 1000]);
        // And a committed one changes it.
        for table in ["t", "twin"] {
            must(&db, "BEGIN", params);
            must(&db, &format!("UPDATE {table} SET b = 301 WHERE id = 1000"), params);
            must(&db, "COMMIT", params);
        }
        assert_eq!(same_as_twin(&db, "a = 3 AND b > 300", params), vec![1000]);
        let p = plan(&db, "SELECT id FROM t WHERE a = 3 AND b > 300", params);
        assert!(p.contains("Index Scan using ix_ab"), "{p}");
    }
}

#[test]
fn order_by_its_columns_with_a_limit_walks_it() {
    for params in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        build(&db, params);

        // The prefix is fixed, the next column orders.
        let sql = "SELECT id FROM t WHERE a = 3 ORDER BY b LIMIT 5";
        let p = plan(&db, sql, params);
        assert!(p.contains("Index Ordered Scan using ix_ab on t"), "{p}");
        assert_eq!(ids(&db, sql, params), vec![3, 15, 27, 39, 51]);
        assert_eq!(
            ids(&db, sql, params),
            ids(&db, "SELECT id FROM twin WHERE a = 3 ORDER BY b LIMIT 5", params)
        );

        // Both columns order.
        let sql = "SELECT id FROM t ORDER BY a, b LIMIT 4";
        let p = plan(&db, sql, params);
        assert!(p.contains("Index Ordered Scan using ix_ab on t"), "{p}");
        assert_eq!(ids(&db, sql, params), vec![0, 12, 24, 36]);
        let sql = "SELECT id FROM t ORDER BY a, b LIMIT 3 OFFSET 19";
        assert_eq!(ids(&db, sql, params), vec![228, 1, 13]);
        assert_eq!(
            ids(&db, sql, params),
            ids(&db, "SELECT id FROM twin ORDER BY a, b LIMIT 3 OFFSET 19", params)
        );

        // A further condition is applied while walking.
        let sql = "SELECT id FROM t WHERE a = 3 AND c = 'c3' ORDER BY b LIMIT 2";
        assert_eq!(
            ids(&db, sql, params),
            ids(
                &db,
                "SELECT id FROM twin WHERE a = 3 AND c = 'c3' ORDER BY b LIMIT 2",
                params
            )
        );

        // Descending is sorted the ordinary way, and still right.
        let sql = "SELECT id FROM t WHERE a = 3 ORDER BY b DESC LIMIT 2";
        assert!(!plan(&db, sql, params).contains("Index Ordered Scan using ix_ab"));
        assert_eq!(ids(&db, sql, params), vec![231, 219]);
    }
}

/// A column that may be NULL: the index is used for lookups (a NULL is never
/// equal to anything) but an ORDER BY is sorted the ordinary way, so NULLs land
/// where the sort puts them.
#[test]
fn a_nullable_column_is_never_misordered() {
    for params in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        build(&db, params);
        must(&db, "CREATE INDEX ix_an ON t (a, n)", params);
        let p = plan(&db, "SELECT id FROM t WHERE a = 2 AND n = 4", params);
        assert!(p.contains("Index Scan using ix_an on t (a = 2 AND n = 4)"), "{p}");
        same_as_twin(&db, "a = 2 AND n = 4", params);
        same_as_twin(&db, "a = 2 AND n IS NULL", params);
        let sql = "SELECT id, n FROM t WHERE a = 2 ORDER BY n, id LIMIT 6";
        assert_eq!(
            rows(&db, sql, params),
            rows(&db, "SELECT id, n FROM twin WHERE a = 2 ORDER BY n, id LIMIT 6", params)
        );
        let sql = "SELECT n FROM t WHERE a = 2 ORDER BY n LIMIT 6";
        assert!(!plan(&db, sql, params).contains("Index Ordered Scan using ix_an"));
        assert_eq!(
            rows(&db, sql, params),
            rows(&db, "SELECT n FROM twin WHERE a = 2 ORDER BY n LIMIT 6", params)
        );
    }
}

#[test]
fn text_and_three_column_keys() {
    for params in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        build(&db, params);
        must(&db, "CREATE INDEX ix_cab ON t (c, a, b)", params);
        let p = plan(&db, "SELECT id FROM t WHERE c = 'c4' AND a = 1", params);
        assert!(p.contains("Index Scan using ix_cab on t (c = "), "{p}");
        same_as_twin(&db, "c = 'c4' AND a = 1", params);
        same_as_twin(&db, "c = 'c4' AND a = 1 AND b > 10", params);
        same_as_twin(&db, "c = 'c4' AND a > 6", params);
        same_as_twin(&db, "c = 'c' AND a = 1", params);
        same_as_twin(&db, "c = 'c44'", params);
        // A text that is a prefix of another is a different key.
        for table in ["t", "twin"] {
            must(
                &db,
                &format!("INSERT INTO {table} VALUES (3000, 1, 1, 'c', 1), (3001, 1, 1, 'c44', 1)"),
                params,
            );
        }
        assert_eq!(same_as_twin(&db, "c = 'c' AND a = 1", params), vec![3000]);
        assert_eq!(same_as_twin(&db, "c = 'c44'", params), vec![3001]);
        assert_eq!(same_as_twin(&db, "c = 'c4' AND a = 1 AND b = 1", params), vec![13]);
    }
}

#[test]
fn the_catalog_lists_every_column_and_drop_removes_it() {
    for params in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        build(&db, params);
        let def = indexdef(&db, "ix_ab", params);
        assert!(def.contains("USING btree (a, b)"), "{def}");
        must(&db, "DROP INDEX ix_ab", params);
        let p = plan(&db, "SELECT id FROM t WHERE a = 3 AND b = 7", params);
        assert!(!p.contains("ix_ab"), "{p}");
        assert_eq!(same_as_twin(&db, "a = 3 AND b = 7", params), vec![87]);
        // A column that does not exist is refused, and nothing is left behind.
        let err = run(&db, "CREATE INDEX bad ON t (a, nope)", params).unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
        must(&db, "CREATE INDEX bad ON t (a, b)", params);
        // DROP COLUMN takes the index with it.
        must(&db, "ALTER TABLE t DROP COLUMN b", params);
        let left = rows(&db, "SELECT indexname FROM pg_indexes WHERE tablename = 't'", params);
        assert!(left.iter().all(|r| text(&r[0]) != "bad"), "{left:?}");
    }
}

#[test]
fn it_is_rebuilt_whole_at_open() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        build(&db, false);
    }
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    for params in FAMILIES {
        let def = indexdef(&db, "ix_ab", params);
        assert!(def.contains("USING btree (a, b)"), "{def}");
        let p = plan(&db, "SELECT id FROM t WHERE a = 3 AND b = 7", params);
        assert!(p.contains("Index Scan using ix_ab on t (a = 3 AND b = 7)"), "{p}");
        assert_eq!(same_as_twin(&db, "a = 3 AND b = 7", params), vec![87]);
        assert_eq!(same_as_twin(&db, "a = 3 AND b >= 18", params).len(), 2);
    }
    // Written after the reopen: still kept.
    db.execute("INSERT INTO t VALUES (5000, 3, 7, 'late', 1)").unwrap();
    db.execute("INSERT INTO twin VALUES (5000, 3, 7, 'late', 1)").unwrap();
    assert_eq!(same_as_twin(&db, "a = 3 AND b = 7", false), vec![87, 5000]);
}

#[test]
fn a_dump_carries_every_column() {
    for format in FORMATS {
        let src = EmbeddedDatabase::new_in_memory().unwrap();
        build(&src, false);
        let (db, _dir) = round_trip(&src, format);
        for params in FAMILIES {
            let def = indexdef(&db, "ix_ab", params);
            assert!(def.contains("USING btree (a, b)"), "{format:?}: {def}");
            let p = plan(&db, "SELECT id FROM t WHERE a = 3 AND b = 7", params);
            assert!(
                p.contains("Index Scan using ix_ab on t (a = 3 AND b = 7)"),
                "{format:?}: {p}"
            );
            assert_eq!(same_as_twin(&db, "a = 3 AND b = 7", params), vec![87], "{format:?}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn over_the_postgresql_wire() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (conn, server) = serve(Arc::clone(&db)).await;
    let client = connect(&conn).await;
    client
        .batch_execute("CREATE TABLE w (id INT PRIMARY KEY, a INT NOT NULL, b INT NOT NULL)")
        .await
        .unwrap();
    for id in 0..60 {
        client
            .batch_execute(&format!("INSERT INTO w VALUES ({id}, {}, {})", id % 6, id / 6))
            .await
            .unwrap();
    }
    client.batch_execute("CREATE INDEX w_ab ON w (a, b)").await.unwrap();

    let p = wire_rows(&client, "EXPLAIN SELECT id FROM w WHERE a = 2 AND b = 4").await;
    let p: Vec<String> = p.into_iter().flatten().collect();
    assert!(
        p.join("\n").contains("Index Scan using w_ab on w (a = 2 AND b = 4)"),
        "{p:?}"
    );
    assert_eq!(
        wire_rows(&client, "SELECT id FROM w WHERE a = 2 AND b = 4").await,
        vec![vec!["26".to_string()]]
    );
    // The extended protocol, with typed parameters.
    let got = client
        .query("SELECT id FROM w WHERE a = $1 AND b >= $2 ORDER BY id", &[&2i32, &8i32])
        .await
        .unwrap();
    let got: Vec<i32> = got.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, vec![50, 56]);
    let ordered = wire_rows(&client, "SELECT id FROM w WHERE a = 2 ORDER BY b LIMIT 3").await;
    assert_eq!(
        ordered,
        vec![vec!["2".to_string()], vec!["8".to_string()], vec!["14".to_string()]]
    );
    let def = wire_rows(&client, "SELECT indexdef FROM pg_indexes WHERE indexname = 'w_ab'").await;
    assert!(def[0][0].contains("(a, b)"), "{def:?}");
    server.abort();
}
