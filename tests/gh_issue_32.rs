//! GH#32 — UPDATE and DELETE whose WHERE holds a subquery.
//!
//! Through 4.42.0 the DML arms evaluated their predicate with a bare
//! evaluator that has no executor, so `IN (SELECT …)`, `NOT IN`, `EXISTS`,
//! `NOT EXISTS` and a scalar subquery comparison failed with "IN subquery
//! evaluation requires executor context" (or the EXISTS / scalar twin) as
//! soon as there was a row to test. They now run: an uncorrelated subquery
//! is executed once per statement, a correlated one once per candidate row,
//! and every row's verdict is decided before the statement writes anything.
//!
//! Every case runs through BOTH statement families: `execute` (text) and
//! `execute_params` (params).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

fn run(db: &EmbeddedDatabase, params_family: bool, sql: &str) -> u64 {
    let out = if params_family {
        db.execute_params(sql, &[])
    } else {
        db.execute(sql)
    };
    out.unwrap_or_else(|e| panic!("[params={params_family}] `{sql}` must run: {e}"))
}

fn ints(db: &EmbeddedDatabase, sql: &str) -> Vec<i64> {
    db.query(sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}`: {e}"))
        .iter()
        .map(|t| match t.values.first() {
            Some(Value::Int2(v)) => i64::from(*v),
            Some(Value::Int4(v)) => i64::from(*v),
            Some(Value::Int8(v)) => *v,
            Some(Value::Null) => -1,
            other => panic!("unexpected {other:?}"),
        })
        .collect()
}

/// acct(id 1..=6, grp = id % 3), other(id, grp) holding ids 2, 4 and a NULL.
fn setup(with_null: bool) -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE acct (id INT PRIMARY KEY, grp INT, n INT)")
        .unwrap();
    db.execute("CREATE TABLE other (id INT, grp INT)").unwrap();
    for i in 1..=6 {
        db.execute(&format!("INSERT INTO acct VALUES ({i}, {}, 0)", i % 3))
            .unwrap();
    }
    db.execute("INSERT INTO other VALUES (2, 1), (4, 1)").unwrap();
    if with_null {
        db.execute("INSERT INTO other VALUES (NULL, 0)").unwrap();
    }
    db
}

const FAMILIES: [bool; 2] = [false, true];

#[test]
fn delete_in_subquery() {
    for p in FAMILIES {
        let db = setup(false);
        assert_eq!(run(&db, p, "DELETE FROM acct WHERE id IN (SELECT id FROM other)"), 2);
        assert_eq!(ints(&db, "SELECT id FROM acct ORDER BY id"), vec![1, 3, 5, 6]);
    }
}

#[test]
fn delete_not_in_subquery_and_null() {
    for p in FAMILIES {
        let db = setup(false);
        assert_eq!(
            run(&db, p, "DELETE FROM acct WHERE id NOT IN (SELECT id FROM other)"),
            4
        );
        assert_eq!(ints(&db, "SELECT id FROM acct ORDER BY id"), vec![2, 4]);
        // A NULL beside no match is unknown: NOT IN keeps every row.
        let db = setup(true);
        assert_eq!(
            run(&db, p, "DELETE FROM acct WHERE id NOT IN (SELECT id FROM other)"),
            0
        );
        assert_eq!(ints(&db, "SELECT count(*) FROM acct"), vec![6]);
    }
}

#[test]
fn not_in_with_null_over_sixteen_rows() {
    for p in FAMILIES {
        let db = setup(true);
        for i in 100..130 {
            db.execute(&format!("INSERT INTO other VALUES ({i}, 0)")).unwrap();
        }
        assert_eq!(
            run(&db, p, "DELETE FROM acct WHERE id NOT IN (SELECT id FROM other)"),
            0
        );
        assert_eq!(
            run(&db, p, "UPDATE acct SET n = 1 WHERE id NOT IN (SELECT id FROM other)"),
            0
        );
        assert_eq!(run(&db, p, "DELETE FROM acct WHERE id IN (SELECT id FROM other)"), 2);
        db.execute("DELETE FROM other WHERE id IS NULL").unwrap();
        assert_eq!(
            run(&db, p, "DELETE FROM acct WHERE id NOT IN (SELECT id FROM other)"),
            4
        );
        assert_eq!(ints(&db, "SELECT count(*) FROM acct"), vec![0]);
    }
}

#[test]
fn exists_and_not_exists() {
    for p in FAMILIES {
        let db = setup(false);
        // Uncorrelated.
        assert_eq!(
            run(
                &db,
                p,
                "UPDATE acct SET n = 1 WHERE EXISTS (SELECT 1 FROM other WHERE grp = 1)"
            ),
            6
        );
        assert_eq!(
            run(&db, p, "DELETE FROM acct WHERE NOT EXISTS (SELECT 1 FROM other)"),
            0
        );
        // Correlated.
        assert_eq!(
            run(
                &db,
                p,
                "UPDATE acct SET n = 2 WHERE EXISTS (SELECT 1 FROM other o WHERE o.id = acct.id)"
            ),
            2
        );
        assert_eq!(ints(&db, "SELECT id FROM acct WHERE n = 2 ORDER BY id"), vec![2, 4]);
        assert_eq!(
            run(
                &db,
                p,
                "DELETE FROM acct WHERE NOT EXISTS (SELECT 1 FROM other o WHERE o.id = acct.id)"
            ),
            4
        );
        assert_eq!(ints(&db, "SELECT id FROM acct ORDER BY id"), vec![2, 4]);
    }
}

#[test]
fn correlated_in_and_scalar() {
    for p in FAMILIES {
        let db = setup(false);
        assert_eq!(
            run(
                &db,
                p,
                "UPDATE acct SET n = 3 WHERE grp IN (SELECT o.grp FROM other o WHERE o.id = acct.id)"
            ),
            1
        );
        assert_eq!(
            run(
                &db,
                p,
                "DELETE FROM acct WHERE id NOT IN (SELECT o.id FROM other o WHERE o.grp = acct.grp)"
            ),
            5
        );
        assert_eq!(ints(&db, "SELECT id FROM acct ORDER BY id"), vec![4]);
        assert_eq!(
            run(
                &db,
                p,
                "UPDATE acct SET n = 4 WHERE grp = (SELECT max(o.grp) FROM other o WHERE o.id = acct.id)"
            ),
            1
        );
    }
}

#[test]
fn scalar_subquery_comparison() {
    for p in FAMILIES {
        let db = setup(false);
        assert_eq!(
            run(&db, p, "UPDATE acct SET n = 9 WHERE grp = (SELECT max(grp) FROM other)"),
            2
        );
        assert_eq!(ints(&db, "SELECT id FROM acct WHERE n = 9 ORDER BY id"), vec![1, 4]);
        assert_eq!(
            run(&db, p, "DELETE FROM acct WHERE id > (SELECT max(id) FROM other)"),
            2
        );
        assert_eq!(ints(&db, "SELECT id FROM acct ORDER BY id"), vec![1, 2, 3, 4]);
        // Primary-key equality against a subquery: the point lookup sees a literal.
        assert_eq!(
            run(&db, p, "DELETE FROM acct WHERE id = (SELECT min(id) FROM other)"),
            1
        );
        assert_eq!(ints(&db, "SELECT id FROM acct ORDER BY id"), vec![1, 3, 4]);
    }
}

#[test]
fn inside_transaction_sees_own_inserts() {
    for p in FAMILIES {
        let db = setup(false);
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO other VALUES (5, 2)").unwrap();
        db.execute("INSERT INTO acct VALUES (7, 1, 0)").unwrap();
        db.execute("INSERT INTO other VALUES (7, 1)").unwrap();
        assert_eq!(run(&db, p, "DELETE FROM acct WHERE id IN (SELECT id FROM other)"), 4);
        assert_eq!(
            run(
                &db,
                p,
                "UPDATE acct SET n = 5 WHERE NOT EXISTS (SELECT 1 FROM other o WHERE o.id = acct.id)"
            ),
            3
        );
        db.execute("COMMIT").unwrap();
        assert_eq!(ints(&db, "SELECT id FROM acct WHERE n = 5 ORDER BY id"), vec![1, 3, 6]);
    }
}

#[test]
fn self_referencing_dedupe_delete() {
    for p in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE d (id INT PRIMARY KEY, k INT)").unwrap();
        db.execute("INSERT INTO d VALUES (1, 10), (2, 10), (3, 20), (4, 30), (5, 30)")
            .unwrap();
        // Every row with a duplicate key goes, as in PostgreSQL: the verdicts
        // are taken against the table as it was before the statement.
        // The outer table is aliased: written unaliased (`… b.k = d.k`), the
        // planner resolves `d` to the inner `d AS b` (the 4.42.0 known limit
        // on a correlated subquery over its own outer table, SELECT included).
        assert_eq!(
            run(
                &db,
                p,
                "DELETE FROM d AS a WHERE EXISTS (SELECT 1 FROM d AS b WHERE b.k = a.k AND b.id <> a.id)"
            ),
            4
        );
        assert_eq!(ints(&db, "SELECT id FROM d"), vec![3]);
    }
}

#[test]
fn self_referencing_update() {
    for p in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE u (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("INSERT INTO u VALUES (1, 1), (2, 2), (3, 3)").unwrap();
        // Statement snapshot: the max is 3 for every row, even after the first change.
        assert_eq!(
            run(&db, p, "UPDATE u SET v = v + 10 WHERE v < (SELECT max(v) FROM u)"),
            2
        );
        assert_eq!(ints(&db, "SELECT v FROM u ORDER BY id"), vec![11, 12, 3]);
        assert_eq!(
            run(
                &db,
                p,
                "UPDATE u AS a SET v = 0 WHERE a.v > (SELECT min(b.v) FROM u AS b WHERE b.id <> a.id)"
            ),
            2
        );
        assert_eq!(ints(&db, "SELECT v FROM u ORDER BY id"), vec![0, 0, 3]);
    }
}

#[test]
fn returning_rows() {
    let db = setup(false);
    let rows = db
        .query("DELETE FROM acct WHERE id IN (SELECT id FROM other) RETURNING id", &[])
        .unwrap();
    let mut got: Vec<Value> = rows.iter().filter_map(|t| t.values.first().cloned()).collect();
    got.sort_by_key(|v| format!("{v:?}"));
    assert_eq!(got, vec![Value::Int4(2), Value::Int4(4)]);

    let (n, rows) = db
        .execute_params_returning(
            "UPDATE acct SET n = 8 WHERE EXISTS (SELECT 1 FROM other o WHERE o.grp = acct.grp) RETURNING id",
            &[],
        )
        .unwrap();
    // Ids 2 and 4 are gone; of grp 1 (the only grp in `other`) only id 1 is left.
    assert_eq!(n, 1);
    assert_eq!(rows.len(), 1);
}

#[test]
fn bound_parameter_inside_subquery() {
    let db = setup(false);
    assert_eq!(
        db.execute_params(
            "UPDATE acct SET n = $2 WHERE id IN (SELECT id FROM other WHERE grp = $1)",
            &[Value::Int4(1), Value::Int4(6)],
        )
        .unwrap(),
        2
    );
    assert_eq!(ints(&db, "SELECT id FROM acct WHERE n = 6 ORDER BY id"), vec![2, 4]);
    assert_eq!(
        db.execute_params(
            "DELETE FROM acct WHERE EXISTS (SELECT 1 FROM other o WHERE o.id = acct.id AND o.id = $1)",
            &[Value::Int4(4)],
        )
        .unwrap(),
        1
    );
    assert_eq!(ints(&db, "SELECT id FROM acct ORDER BY id"), vec![1, 2, 3, 5, 6]);
}

#[test]
fn gh32_uuid_reproduction() {
    for p in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute(r#"CREATE TABLE "P" ("id" UUID PRIMARY KEY, "login" VARCHAR(39) NOT NULL)"#)
            .unwrap();
        db.execute(
            r#"CREATE TABLE "S" ("id" UUID PRIMARY KEY, "pid" UUID NOT NULL REFERENCES "P"("id"), "n" INTEGER)"#,
        )
        .unwrap();
        db.execute(r#"INSERT INTO "P" VALUES ('11111111-1111-4111-8111-111111111111', 'alice')"#)
            .unwrap();
        db.execute(r#"INSERT INTO "S" VALUES ('22222222-2222-4222-8222-222222222222', '11111111-1111-4111-8111-111111111111', 1)"#)
            .unwrap();
        assert_eq!(
            run(
                &db,
                p,
                r#"DELETE FROM "S" WHERE "pid" IN (SELECT "id" FROM "P" WHERE "login" LIKE 'ali%')"#
            ),
            1
        );
        assert_eq!(ints(&db, r#"SELECT count(*) FROM "S""#), vec![0]);
    }
}

/// Statements without a subquery keep the point paths; a subquery predicate
/// never matches the fast `pk = value` shapes and is answered correctly.
#[test]
fn fast_paths_decline_subquery_predicates() {
    for p in FAMILIES {
        let db = setup(false);
        assert_eq!(run(&db, p, "UPDATE acct SET n = 1 WHERE id = 1"), 1);
        assert_eq!(run(&db, p, "DELETE FROM acct WHERE id = 6"), 1);
        assert_eq!(
            run(&db, p, "UPDATE acct SET n = 2 WHERE id = (SELECT max(id) FROM other)"),
            1
        );
        assert_eq!(ints(&db, "SELECT n FROM acct WHERE id = 4"), vec![2]);
        assert_eq!(
            run(&db, p, "DELETE FROM acct WHERE id = (SELECT min(id) FROM other)"),
            1
        );
        assert_eq!(ints(&db, "SELECT id FROM acct ORDER BY id"), vec![1, 3, 4, 5]);
    }
}
