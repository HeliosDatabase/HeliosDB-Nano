//! Partitioned tables: an INSERT through the table stores each row in the
//! partition its key routes to, a SELECT through the table reads every
//! partition, UPDATE / DELETE / TRUNCATE act on the partitions, and an UPDATE
//! of the partition key moves the row.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use heliosdb_nano::{EmbeddedDatabase, Value};

fn int(v: &Value) -> i64 {
    match v {
        Value::Int2(i) => i64::from(*i),
        Value::Int4(i) => i64::from(*i),
        Value::Int8(i) => *i,
        other => panic!("not an integer: {other:?}"),
    }
}

fn ints(db: &EmbeddedDatabase, sql: &str) -> Vec<i64> {
    db.query(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .iter()
        .map(|r| int(&r.values[0]))
        .collect()
}

fn count(db: &EmbeddedDatabase, table: &str) -> i64 {
    ints(db, &format!("SELECT count(*) FROM {table}"))[0]
}

fn range_table(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE m (id INT NOT NULL, v TEXT) PARTITION BY RANGE (id)")
        .unwrap();
    db.execute("CREATE TABLE m_1 PARTITION OF m FOR VALUES FROM (0) TO (10)")
        .unwrap();
    db.execute("CREATE TABLE m_2 PARTITION OF m FOR VALUES FROM (10) TO (20)")
        .unwrap();
}

#[test]
fn range_routing_at_both_boundaries() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    range_table(&db);
    for id in [0, 9, 10, 19] {
        db.execute(&format!("INSERT INTO m (id, v) VALUES ({id}, 'x')"))
            .unwrap();
    }
    assert_eq!(ints(&db, "SELECT id FROM m_1 ORDER BY id"), vec![0, 9]);
    assert_eq!(ints(&db, "SELECT id FROM m_2 ORDER BY id"), vec![10, 19]);
    // the parent reads every partition
    assert_eq!(ints(&db, "SELECT id FROM m ORDER BY id"), vec![0, 9, 10, 19]);
    assert_eq!(count(&db, "m"), 4);
}

#[test]
fn a_row_outside_every_bound_is_refused_with_its_key() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    range_table(&db);
    let err = db
        .execute("INSERT INTO m (id, v) VALUES (20, 'x')")
        .unwrap_err()
        .to_string();
    assert!(err.contains("no partition of relation \"m\" found for row"), "{err}");
    assert!(err.contains("(id) = (20)"), "{err}");
    let err = db
        .execute_params(
            "INSERT INTO m (id, v) VALUES ($1, $2)",
            &[Value::Int4(-1), Value::String("x".into())],
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("no partition of relation \"m\" found for row"), "{err}");
    assert_eq!(count(&db, "m"), 0);
}

#[test]
fn a_multi_row_insert_is_all_or_nothing_across_partitions() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    range_table(&db);
    let err = db.execute("INSERT INTO m (id, v) VALUES (1, 'a'), (11, 'b'), (99, 'c')");
    assert!(err.is_err());
    assert_eq!(count(&db, "m"), 0, "rows of a refused statement stayed");
    assert_eq!(count(&db, "m_1"), 0);
    db.execute("INSERT INTO m (id, v) VALUES (1, 'a'), (11, 'b'), (2, 'c')")
        .unwrap();
    assert_eq!(count(&db, "m_1"), 2);
    assert_eq!(count(&db, "m_2"), 1);
}

#[test]
fn list_routing_with_null_and_a_default_partition() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE l (k TEXT, n INT) PARTITION BY LIST (k)")
        .unwrap();
    db.execute("CREATE TABLE l_ab PARTITION OF l FOR VALUES IN ('a', 'b')")
        .unwrap();
    db.execute("CREATE TABLE l_null PARTITION OF l FOR VALUES IN (NULL)")
        .unwrap();
    let err = db.execute("INSERT INTO l VALUES ('z', 1)").unwrap_err().to_string();
    assert!(err.contains("no partition of relation \"l\""), "{err}");
    db.execute("CREATE TABLE l_rest PARTITION OF l DEFAULT").unwrap();
    db.execute("INSERT INTO l VALUES ('a', 1), ('b', 2), (NULL, 3), ('z', 4)")
        .unwrap();
    assert_eq!(ints(&db, "SELECT n FROM l_ab ORDER BY n"), vec![1, 2]);
    assert_eq!(ints(&db, "SELECT n FROM l_null"), vec![3]);
    assert_eq!(ints(&db, "SELECT n FROM l_rest"), vec![4]);
    assert_eq!(ints(&db, "SELECT n FROM l ORDER BY n"), vec![1, 2, 3, 4]);
    assert_eq!(ints(&db, "SELECT n FROM l WHERE k IS NULL"), vec![3]);
}

#[test]
fn hash_routing_with_modulus_and_remainder() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE h (id INT, v TEXT) PARTITION BY HASH (id)")
        .unwrap();
    for r in 0..4 {
        db.execute(&format!(
            "CREATE TABLE h_{r} PARTITION OF h FOR VALUES WITH (MODULUS 4, REMAINDER {r})"
        ))
        .unwrap();
    }
    for id in 0..200 {
        db.execute_params("INSERT INTO h (id, v) VALUES ($1, 'x')", &[Value::Int4(id)])
            .unwrap();
    }
    assert_eq!(count(&db, "h"), 200);
    let mut total = 0;
    for r in 0..4 {
        let n = count(&db, &format!("h_{r}"));
        assert!(n > 20, "hash partition h_{r} holds {n} of 200 rows");
        total += n;
    }
    assert_eq!(total, 200);
    // every row is found again through the key
    for id in [0, 7, 42, 199] {
        assert_eq!(ints(&db, &format!("SELECT id FROM h WHERE id = {id}")), vec![id]);
    }
    // a hash table with a missing remainder refuses the rows that hash there
    db.execute("CREATE TABLE g (id INT) PARTITION BY HASH (id)").unwrap();
    db.execute("CREATE TABLE g_0 PARTITION OF g FOR VALUES WITH (MODULUS 2, REMAINDER 0)")
        .unwrap();
    let mut refused = 0;
    for id in 0..20 {
        if db.execute(&format!("INSERT INTO g VALUES ({id})")).is_err() {
            refused += 1;
        }
    }
    assert!(refused > 0 && refused < 20, "{refused} of 20 refused");
    assert_eq!(count(&db, "g") + refused, 20);
}

#[test]
fn multi_column_range_keys_route_as_tuples() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (a INT, b INT, v TEXT) PARTITION BY RANGE (a, b)")
        .unwrap();
    db.execute("CREATE TABLE t_lo PARTITION OF t FOR VALUES FROM (MINVALUE, MINVALUE) TO (1, 5)")
        .unwrap();
    db.execute("CREATE TABLE t_hi PARTITION OF t FOR VALUES FROM (1, 5) TO (MAXVALUE, MAXVALUE)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (0, 100, 'lo'), (1, 4, 'lo'), (1, 5, 'hi'), (2, 0, 'hi')")
        .unwrap();
    assert_eq!(count(&db, "t_lo"), 2);
    assert_eq!(count(&db, "t_hi"), 2);
    assert_eq!(count(&db, "t"), 4);
}

#[test]
fn every_insert_route_goes_through_routing() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    range_table(&db);
    db.execute("CREATE TABLE src (id INT, v TEXT)").unwrap();
    db.execute("INSERT INTO src VALUES (3, 's'), (13, 's')").unwrap();

    // INSERT … VALUES, single and multi row, text and parameters
    db.execute("INSERT INTO m VALUES (1, 'a')").unwrap();
    db.execute("INSERT INTO m (v, id) VALUES ('b', 11), ('c', 2)").unwrap();
    db.execute_params(
        "INSERT INTO m (id, v) VALUES ($1, $2), ($3, $4)",
        &[
            Value::Int4(4),
            Value::String("p".into()),
            Value::Int4(14),
            Value::String("p".into()),
        ],
    )
    .unwrap();
    // INSERT … SELECT
    db.execute("INSERT INTO m SELECT id, v FROM src").unwrap();
    db.execute_params("INSERT INTO m SELECT id + $1, v FROM src", &[Value::Int4(2)])
        .unwrap();
    // RETURNING keeps the order of the rows written
    let (n, rows) = db
        .execute_returning("INSERT INTO m (id, v) VALUES (16, 'r'), (6, 'r'), (17, 'r') RETURNING id")
        .unwrap();
    assert_eq!(n, 3);
    assert_eq!(
        rows.iter().map(|r| int(&r.values[0])).collect::<Vec<_>>(),
        vec![16, 6, 17]
    );
    let (n, rows) = db
        .execute_params_returning(
            "INSERT INTO m (id, v) VALUES ($1, 'r') RETURNING id, v",
            &[Value::Int4(7)],
        )
        .unwrap();
    assert_eq!((n, int(&rows[0].values[0])), (1, 7));
    // inside a transaction, committed and rolled back
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO m VALUES (8, 't'), (18, 't')").unwrap();
    db.execute("COMMIT").unwrap();
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO m VALUES (9, 'gone'), (19, 'gone')").unwrap();
    db.execute("ROLLBACK").unwrap();

    assert_eq!(
        ints(&db, "SELECT id FROM m_1 ORDER BY id"),
        vec![1, 2, 3, 4, 5, 6, 7, 8]
    );
    assert_eq!(
        ints(&db, "SELECT id FROM m_2 ORDER BY id"),
        vec![11, 13, 14, 15, 16, 17, 18]
    );
    assert_eq!(count(&db, "m"), 15);
}

#[test]
fn on_conflict_and_keys_declared_on_the_parent() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE k (id INT PRIMARY KEY, v TEXT DEFAULT 'dflt') PARTITION BY RANGE (id)")
        .unwrap();
    db.execute("CREATE TABLE k_1 PARTITION OF k FOR VALUES FROM (0) TO (10)")
        .unwrap();
    db.execute("CREATE TABLE k_2 PARTITION OF k FOR VALUES FROM (10) TO (20)")
        .unwrap();
    db.execute("INSERT INTO k VALUES (1, 'a'), (11, 'b')").unwrap();
    // the parent's primary key is enforced in each partition
    assert!(db.execute("INSERT INTO k VALUES (1, 'dup')").is_err());
    assert!(db.execute("INSERT INTO k_2 VALUES (11, 'dup')").is_err());
    db.execute("INSERT INTO k VALUES (1, 'ignored') ON CONFLICT (id) DO NOTHING")
        .unwrap();
    db.execute("INSERT INTO k VALUES (11, 'new') ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v")
        .unwrap();
    let rows = db.query("SELECT id, v FROM k ORDER BY id", &[]).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values[1], Value::String("a".into()));
    assert_eq!(rows[1].values[1], Value::String("new".into()));
    // the parent's default reaches rows written through it and straight to a partition
    db.execute("INSERT INTO k (id) VALUES (2)").unwrap();
    db.execute("INSERT INTO k_2 (id) VALUES (12)").unwrap();
    let rows = db
        .query("SELECT v FROM k WHERE id IN (2, 12) ORDER BY id", &[])
        .unwrap();
    assert_eq!(rows[0].values[0], Value::String("dflt".into()));
    assert_eq!(rows[1].values[0], Value::String("dflt".into()));
    // a key that leaves out the partition key is refused
    let err = db
        .execute("CREATE TABLE bad (id INT PRIMARY KEY, d INT) PARTITION BY RANGE (d)")
        .unwrap_err()
        .to_string();
    assert!(err.contains("must include all partitioning columns"), "{err}");
    let err = db
        .execute("CREATE TABLE bad (id INT, d INT, UNIQUE (id)) PARTITION BY RANGE (d)")
        .unwrap_err()
        .to_string();
    assert!(err.contains("must include all partitioning columns"), "{err}");
}

#[test]
fn update_and_delete_through_the_parent() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    range_table(&db);
    db.execute("INSERT INTO m VALUES (1, 'a'), (2, 'b'), (11, 'c'), (12, 'd')")
        .unwrap();
    assert_eq!(db.execute("UPDATE m SET v = 'u' WHERE id IN (2, 11)").unwrap(), 2);
    assert_eq!(ints(&db, "SELECT id FROM m WHERE v = 'u' ORDER BY id"), vec![2, 11]);
    assert_eq!(
        db.execute_params(
            "UPDATE m SET v = $1 WHERE id = $2",
            &[Value::String("p".into()), Value::Int4(12)]
        )
        .unwrap(),
        1
    );
    assert_eq!(ints(&db, "SELECT id FROM m_2 WHERE v = 'p'"), vec![12]);
    let (n, rows) = db.execute_returning("UPDATE m SET v = 'all' RETURNING id").unwrap();
    assert_eq!(n, 4);
    let mut ids: Vec<i64> = rows.iter().map(|r| int(&r.values[0])).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2, 11, 12]);
    assert_eq!(db.execute("DELETE FROM m WHERE id = 1").unwrap(), 1);
    assert_eq!(db.execute("DELETE FROM m WHERE id > 11").unwrap(), 1);
    assert_eq!(ints(&db, "SELECT id FROM m ORDER BY id"), vec![2, 11]);
    assert_eq!(db.execute("DELETE FROM m").unwrap(), 2);
    assert_eq!(count(&db, "m"), 0);
}

#[test]
fn an_update_of_the_partition_key_moves_the_row() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    range_table(&db);
    db.execute("INSERT INTO m VALUES (1, 'a'), (2, 'b'), (11, 'c')")
        .unwrap();
    assert_eq!(db.execute("UPDATE m SET id = 15 WHERE id = 1").unwrap(), 1);
    assert_eq!(ints(&db, "SELECT id FROM m_1 ORDER BY id"), vec![2]);
    assert_eq!(ints(&db, "SELECT id FROM m_2 ORDER BY id"), vec![11, 15]);
    let rows = db.query("SELECT v FROM m WHERE id = 15", &[]).unwrap();
    assert_eq!(rows[0].values[0], Value::String("a".into()));
    // a row that stays in its partition
    assert_eq!(db.execute("UPDATE m SET id = id + 1 WHERE id = 2").unwrap(), 1);
    assert_eq!(ints(&db, "SELECT id FROM m_1"), vec![3]);
    // with parameters and RETURNING
    let (n, rows) = db
        .execute_params_returning(
            "UPDATE m SET id = $1 WHERE id = $2 RETURNING id, v",
            &[Value::Int4(4), Value::Int4(15)],
        )
        .unwrap();
    assert_eq!((n, int(&rows[0].values[0])), (1, 4));
    assert_eq!(ints(&db, "SELECT id FROM m_1 ORDER BY id"), vec![3, 4]);
    // no partition accepts the new key: 23514, and nothing changed
    let err = db.execute("UPDATE m SET id = id + 100").unwrap_err().to_string();
    assert!(err.contains("no partition of relation \"m\" found for row"), "{err}");
    assert_eq!(ints(&db, "SELECT id FROM m ORDER BY id"), vec![3, 4, 11]);
    // the same inside a transaction: the statement fails as a whole
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO m VALUES (5, 'txn')").unwrap();
    assert!(db.execute("UPDATE m SET id = id + 16 WHERE id < 10").is_err());
    let _ = db.execute("ROLLBACK");
    assert_eq!(ints(&db, "SELECT id FROM m ORDER BY id"), vec![3, 4, 11]);
}

#[test]
fn truncate_and_drop() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    range_table(&db);
    db.execute("INSERT INTO m VALUES (1, 'a'), (11, 'c')").unwrap();
    db.execute("TRUNCATE m").unwrap();
    assert_eq!(count(&db, "m_1") + count(&db, "m_2"), 0);
    db.execute("INSERT INTO m VALUES (1, 'a'), (11, 'c')").unwrap();
    // dropping a partition takes its rows and its range
    db.execute("DROP TABLE m_2").unwrap();
    assert_eq!(ints(&db, "SELECT id FROM m"), vec![1]);
    assert!(db.execute("INSERT INTO m VALUES (12, 'x')").is_err());
    // dropping the parent drops the partitions
    db.execute("DROP TABLE m").unwrap();
    assert!(db.query("SELECT * FROM m_1", &[]).is_err());
}

#[test]
fn sub_partitioning_routes_and_reads_at_every_level() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE s (region TEXT, id INT, v TEXT) PARTITION BY LIST (region)")
        .unwrap();
    db.execute("CREATE TABLE s_eu PARTITION OF s FOR VALUES IN ('eu') PARTITION BY RANGE (id)")
        .unwrap();
    db.execute("CREATE TABLE s_eu_lo PARTITION OF s_eu FOR VALUES FROM (0) TO (100)")
        .unwrap();
    db.execute("CREATE TABLE s_eu_hi PARTITION OF s_eu FOR VALUES FROM (100) TO (200)")
        .unwrap();
    db.execute("CREATE TABLE s_us PARTITION OF s FOR VALUES IN ('us')")
        .unwrap();
    db.execute("INSERT INTO s VALUES ('eu', 1, 'a'), ('eu', 150, 'b'), ('us', 7, 'c')")
        .unwrap();
    assert_eq!(ints(&db, "SELECT id FROM s_eu_lo"), vec![1]);
    assert_eq!(ints(&db, "SELECT id FROM s_eu_hi"), vec![150]);
    assert_eq!(ints(&db, "SELECT id FROM s_us"), vec![7]);
    assert_eq!(ints(&db, "SELECT id FROM s_eu ORDER BY id"), vec![1, 150]);
    assert_eq!(ints(&db, "SELECT id FROM s ORDER BY id"), vec![1, 7, 150]);
    // no partition at the second level
    assert!(db.execute("INSERT INTO s VALUES ('eu', 500, 'x')").is_err());
    // a row written straight to a leaf must fit every level above it
    let err = db
        .execute("INSERT INTO s_eu_lo VALUES ('us', 5, 'x')")
        .unwrap_err()
        .to_string();
    assert!(err.contains("violates partition constraint"), "{err}");
    let err = db
        .execute("INSERT INTO s_eu_lo VALUES ('eu', 150, 'x')")
        .unwrap_err()
        .to_string();
    assert!(err.contains("violates partition constraint"), "{err}");
    db.execute("INSERT INTO s_eu_lo VALUES ('eu', 5, 'ok')").unwrap();
    // updates and deletes reach the leaves; a key change moves across levels
    assert_eq!(db.execute("UPDATE s SET v = 'z' WHERE region = 'eu'").unwrap(), 3);
    assert_eq!(db.execute("UPDATE s SET id = 120 WHERE id = 1").unwrap(), 1);
    assert_eq!(ints(&db, "SELECT id FROM s_eu_hi ORDER BY id"), vec![120, 150]);
    assert_eq!(db.execute("UPDATE s SET region = 'us' WHERE id = 5").unwrap(), 1);
    assert_eq!(ints(&db, "SELECT id FROM s_us ORDER BY id"), vec![5, 7]);
    assert_eq!(db.execute("DELETE FROM s WHERE region = 'eu'").unwrap(), 2);
    assert_eq!(ints(&db, "SELECT id FROM s ORDER BY id"), vec![5, 7]);
}

#[test]
fn joins_aggregates_views_and_time_keys_over_a_partitioned_table() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE ev (day DATE NOT NULL, n INT) PARTITION BY RANGE (day)")
        .unwrap();
    db.execute("CREATE TABLE ev_jan PARTITION OF ev FOR VALUES FROM ('2024-01-01') TO ('2024-02-01')")
        .unwrap();
    db.execute("CREATE TABLE ev_feb PARTITION OF ev FOR VALUES FROM ('2024-02-01') TO ('2024-03-01')")
        .unwrap();
    db.execute("INSERT INTO ev VALUES ('2024-01-15', 1), ('2024-01-31', 2), ('2024-02-01', 3)")
        .unwrap();
    assert_eq!(count(&db, "ev_jan"), 2);
    assert_eq!(count(&db, "ev_feb"), 1);
    assert_eq!(ints(&db, "SELECT sum(n) FROM ev"), vec![6]);
    assert_eq!(ints(&db, "SELECT n FROM ev WHERE day >= '2024-02-01'"), vec![3]);
    db.execute("CREATE TABLE names (n INT, label TEXT)").unwrap();
    db.execute("INSERT INTO names VALUES (1, 'one'), (3, 'three')").unwrap();
    assert_eq!(
        ints(&db, "SELECT e.n FROM ev e JOIN names x ON x.n = e.n ORDER BY e.n"),
        vec![1, 3]
    );
    assert_eq!(
        ints(&db, "SELECT ev.n FROM ev WHERE ev.n > 1 ORDER BY ev.n"),
        vec![2, 3]
    );
    db.execute("CREATE VIEW ev_v AS SELECT n FROM ev WHERE n > 1").unwrap();
    assert_eq!(ints(&db, "SELECT n FROM ev_v ORDER BY n"), vec![2, 3]);
    db.execute("CREATE MATERIALIZED VIEW ev_mv AS SELECT count(*) AS c FROM ev")
        .unwrap();
    assert_eq!(ints(&db, "SELECT c FROM ev_mv"), vec![3]);
    assert_eq!(
        ints(&db, "SELECT n FROM ev WHERE n IN (SELECT n FROM names) ORDER BY n"),
        vec![1, 3]
    );
}

#[test]
fn what_is_refused_on_a_partitioned_table_is_refused_out_loud() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    range_table(&db);
    db.execute("CREATE TABLE other (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE pkp (id INT PRIMARY KEY) PARTITION BY RANGE (id)")
        .unwrap();
    for sql in [
        "CREATE INDEX m_v ON m (v)",
        "CREATE TABLE ref (id INT REFERENCES pkp (id))",
        "ALTER TABLE other ADD CONSTRAINT other_fk FOREIGN KEY (id) REFERENCES pkp (id)",
        "CREATE TABLE fkp (id INT REFERENCES other (id)) PARTITION BY RANGE (id)",
        "CREATE TABLE e (a INT) PARTITION BY RANGE ((a + 1))",
        "CREATE TABLE my (d INT) PARTITION BY RANGE (d) (PARTITION p0 VALUES LESS THAN (10))",
        "ALTER TABLE m DROP COLUMN id",
        "ALTER TABLE m_1 ADD COLUMN extra INT",
    ] {
        let err = db.execute(sql).expect_err(sql).to_string();
        assert!(
            err.contains("partitioning feature not supported") || err.contains("partitioning wrong object"),
            "{sql}: {err}"
        );
    }
    // the statements above left nothing behind
    for table in ["ref", "fkp", "e", "my"] {
        assert!(
            db.query(&format!("SELECT * FROM {table}"), &[]).is_err(),
            "{table} exists"
        );
    }
}

#[test]
fn column_and_constraint_changes_on_the_parent_reach_the_partitions() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    range_table(&db);
    db.execute("INSERT INTO m VALUES (1, 'a'), (11, 'b')").unwrap();
    db.execute("ALTER TABLE m ADD COLUMN extra INT DEFAULT 7").unwrap();
    db.execute("INSERT INTO m (id, v) VALUES (2, 'c')").unwrap();
    assert_eq!(ints(&db, "SELECT extra FROM m ORDER BY id"), vec![7, 7, 7]);
    assert_eq!(ints(&db, "SELECT extra FROM m_2"), vec![7]);
    db.execute("ALTER TABLE m ADD CONSTRAINT extra_small CHECK (extra < 100)")
        .unwrap();
    assert!(db.execute("INSERT INTO m VALUES (3, 'd', 500)").is_err());
    assert!(db.execute("INSERT INTO m_1 VALUES (3, 'd', 500)").is_err());
    // a partition created later has the column and the check too
    db.execute("CREATE TABLE m_3 PARTITION OF m FOR VALUES FROM (20) TO (30)")
        .unwrap();
    db.execute("INSERT INTO m (id, v) VALUES (21, 'e')").unwrap();
    assert_eq!(ints(&db, "SELECT extra FROM m_3"), vec![7]);
    assert!(db.execute("INSERT INTO m_3 VALUES (22, 'f', 500)").is_err());
    db.execute("ALTER TABLE m RENAME COLUMN v TO label").unwrap();
    assert_eq!(ints(&db, "SELECT id FROM m WHERE label = 'e'"), vec![21]);
    db.execute("ALTER TABLE m DROP COLUMN extra").unwrap();
    assert_eq!(db.query("SELECT * FROM m_1", &[]).unwrap()[0].values.len(), 2);
}
