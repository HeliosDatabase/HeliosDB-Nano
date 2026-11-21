//! Partition bounds are validated when a partition is created or attached,
//! and enforced on every write that goes straight to a partition.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use heliosdb_nano::{EmbeddedDatabase, Value};

fn count(db: &EmbeddedDatabase, table: &str) -> i64 {
    match db.query(&format!("SELECT count(*) FROM {table}"), &[]).unwrap()[0].values[0] {
        Value::Int8(n) => n,
        Value::Int4(n) => i64::from(n),
        ref other => panic!("{other:?}"),
    }
}

fn parent(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE m (id INT NOT NULL, v INT) PARTITION BY RANGE (id)")
        .unwrap();
    db.execute("CREATE TABLE m_1 PARTITION OF m FOR VALUES FROM (0) TO (10)")
        .unwrap();
}

#[test]
fn overlapping_range_refused_42p17() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    parent(&db);
    let err = db
        .execute("CREATE TABLE m_2 PARTITION OF m FOR VALUES FROM (5) TO (20)")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("partition \"m_2\" would overlap partition \"m_1\""),
        "{err}"
    );
    assert!(
        db.query("SELECT * FROM m_2", &[]).is_err(),
        "the refused partition exists"
    );
    db.execute("CREATE TABLE m_2 PARTITION OF m FOR VALUES FROM (10) TO (20)")
        .unwrap();
    // list values and hash remainders overlap too
    db.execute("CREATE TABLE l (k INT) PARTITION BY LIST (k)").unwrap();
    db.execute("CREATE TABLE l_1 PARTITION OF l FOR VALUES IN (1, 2)")
        .unwrap();
    assert!(db
        .execute("CREATE TABLE l_2 PARTITION OF l FOR VALUES IN (2, 3)")
        .is_err());
    db.execute("CREATE TABLE h (k INT) PARTITION BY HASH (k)").unwrap();
    db.execute("CREATE TABLE h_0 PARTITION OF h FOR VALUES WITH (MODULUS 2, REMAINDER 0)")
        .unwrap();
    assert!(db
        .execute("CREATE TABLE h_x PARTITION OF h FOR VALUES WITH (MODULUS 4, REMAINDER 2)")
        .is_err());
}

#[test]
fn second_default_refused() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    parent(&db);
    db.execute("CREATE TABLE m_d PARTITION OF m DEFAULT").unwrap();
    let err = db
        .execute("CREATE TABLE m_d2 PARTITION OF m DEFAULT")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("conflicts with existing default partition \"m_d\""),
        "{err}"
    );
}

#[test]
fn hash_remainder_must_be_below_modulus() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE h (k INT) PARTITION BY HASH (k)").unwrap();
    let err = db
        .execute("CREATE TABLE h_bad PARTITION OF h FOR VALUES WITH (MODULUS 2, REMAINDER 5)")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("remainder for hash partition must be less than modulus"),
        "{err}"
    );
    assert!(db
        .execute("CREATE TABLE h_bad PARTITION OF h FOR VALUES WITH (MODULUS 0, REMAINDER 0)")
        .is_err());
}

#[test]
fn a_bound_must_match_the_strategy_and_the_key_type() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    parent(&db);
    for bound in [
        "FOR VALUES IN (1, 2)",
        "FOR VALUES WITH (MODULUS 2, REMAINDER 0)",
        "FOR VALUES FROM ('abc') TO ('abd')",
        "FOR VALUES FROM (30) TO (30)",
        "FOR VALUES FROM (30, 1) TO (40, 1)",
    ] {
        assert!(
            db.execute(&format!("CREATE TABLE m_x PARTITION OF m {bound}")).is_err(),
            "{bound} was accepted"
        );
    }
    // PARTITION OF a table that is not partitioned
    db.execute("CREATE TABLE plain (id INT)").unwrap();
    let err = db
        .execute("CREATE TABLE plain_1 PARTITION OF plain FOR VALUES FROM (0) TO (1)")
        .unwrap_err()
        .to_string();
    assert!(err.contains("\"plain\" is not partitioned"), "{err}");
    // a key column that does not exist, a key type that cannot be a key
    assert!(db.execute("CREATE TABLE q (a INT) PARTITION BY RANGE (b)").is_err());
    assert!(db.execute("CREATE TABLE q (a JSONB) PARTITION BY LIST (a)").is_err());
}

#[test]
fn a_write_straight_to_a_partition_outside_its_bound_is_refused_23514() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    parent(&db);
    let refused = |r: heliosdb_nano::Result<u64>, how: &str| {
        let err = r.expect_err(how).to_string();
        assert!(
            err.contains("new row for relation \"m_1\" violates partition constraint"),
            "{how}: {err}"
        );
    };
    refused(db.execute("INSERT INTO m_1 VALUES (999, 1)"), "literal insert");
    refused(
        db.execute("INSERT INTO m_1 (v, id) VALUES (1, 10)"),
        "upper bound is exclusive",
    );
    refused(
        db.execute("INSERT INTO m_1 VALUES (1, 1), (999, 1)"),
        "multi-row insert",
    );
    refused(
        db.execute_params("INSERT INTO m_1 VALUES ($1, $2)", &[Value::Int4(999), Value::Int4(1)]),
        "insert with parameters",
    );
    db.execute("CREATE TABLE src (id INT, v INT)").unwrap();
    db.execute("INSERT INTO src VALUES (999, 1)").unwrap();
    refused(db.execute("INSERT INTO m_1 SELECT id, v FROM src"), "insert select");
    db.execute("BEGIN").unwrap();
    refused(db.execute("INSERT INTO m_1 VALUES (999, 1)"), "insert in a transaction");
    db.execute("ROLLBACK").unwrap();
    assert_eq!(count(&db, "m_1"), 0);

    db.execute("INSERT INTO m_1 VALUES (5, 1)").unwrap();
    refused(db.execute("UPDATE m_1 SET id = 50 WHERE id = 5"), "update");
    refused(
        db.execute_params("UPDATE m_1 SET id = $1 WHERE id = 5", &[Value::Int4(50)]),
        "update with parameters",
    );
    db.execute("UPDATE m_1 SET id = 6 WHERE id = 5").unwrap();
    assert_eq!(count(&db, "m"), 1);

    // the DEFAULT partition refuses what another partition claims
    db.execute("CREATE TABLE m_d PARTITION OF m DEFAULT").unwrap();
    let err = db.execute("INSERT INTO m_d VALUES (3, 1)").unwrap_err().to_string();
    assert!(
        err.contains("new row for relation \"m_d\" violates partition constraint"),
        "{err}"
    );
    db.execute("INSERT INTO m_d VALUES (300, 1)").unwrap();
    // a new partition cannot take a range the default already holds rows of
    let err = db
        .execute("CREATE TABLE m_300 PARTITION OF m FOR VALUES FROM (300) TO (400)")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("default partition \"m_d\" would be violated by some row"),
        "{err}"
    );
    db.execute("CREATE TABLE m_400 PARTITION OF m FOR VALUES FROM (400) TO (500)")
        .unwrap();
}

#[test]
fn attach_with_out_of_bound_row_fails_23514_and_changes_nothing() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    parent(&db);
    db.execute("CREATE TABLE cand (id INT NOT NULL, v INT)").unwrap();
    db.execute("INSERT INTO cand VALUES (10, 1), (25, 2)").unwrap();
    let err = db
        .execute("ALTER TABLE m ATTACH PARTITION cand FOR VALUES FROM (10) TO (20)")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("partition constraint of relation \"cand\" would be violated by some row"),
        "{err}"
    );
    assert_eq!(count(&db, "m"), 0, "the refused table is read through the parent");
    db.execute("INSERT INTO cand VALUES (999, 3)").unwrap();
    db.execute("DELETE FROM cand WHERE id > 20").unwrap();
    // an overlapping bound, a missing bound, a table of another shape
    assert!(db
        .execute("ALTER TABLE m ATTACH PARTITION cand FOR VALUES FROM (5) TO (20)")
        .is_err());
    assert!(db.execute("ALTER TABLE m ATTACH PARTITION cand").is_err());
    db.execute("CREATE TABLE other_shape (id INT NOT NULL, v TEXT)")
        .unwrap();
    assert!(db
        .execute("ALTER TABLE m ATTACH PARTITION other_shape FOR VALUES FROM (50) TO (60)")
        .is_err());

    db.execute("ALTER TABLE m ATTACH PARTITION cand FOR VALUES FROM (10) TO (20)")
        .unwrap();
    assert_eq!(count(&db, "m"), 1);
    db.execute("INSERT INTO m VALUES (11, 5)").unwrap();
    assert_eq!(count(&db, "cand"), 2);
    assert!(db.execute("INSERT INTO cand VALUES (999, 3)").is_err());
    assert!(db
        .execute("ALTER TABLE m ATTACH PARTITION cand FOR VALUES FROM (10) TO (20)")
        .is_err());
}

#[test]
fn attach_then_drop_parent_cascades_and_detach_keeps_the_table() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    parent(&db);
    db.execute("CREATE TABLE a (id INT NOT NULL, v INT)").unwrap();
    db.execute("CREATE TABLE b (id INT NOT NULL, v INT)").unwrap();
    db.execute("ALTER TABLE m ATTACH PARTITION a FOR VALUES FROM (10) TO (20)")
        .unwrap();
    db.execute("ALTER TABLE m ATTACH PARTITION b DEFAULT").unwrap();
    db.execute("INSERT INTO m VALUES (1, 1), (11, 1), (99, 1)").unwrap();
    assert_eq!((count(&db, "m_1"), count(&db, "a"), count(&db, "b")), (1, 1, 1));

    db.execute("ALTER TABLE m DETACH PARTITION a").unwrap();
    assert_eq!(count(&db, "m"), 2, "a detached partition is no longer read");
    assert_eq!(count(&db, "a"), 1, "a detached partition keeps its rows");
    // it is an ordinary table again: no bound
    db.execute("INSERT INTO a VALUES (999, 1)").unwrap();
    // its range now belongs to the default partition
    db.execute("INSERT INTO m VALUES (12, 1)").unwrap();
    assert_eq!(count(&db, "b"), 2);
    assert!(db.execute("ALTER TABLE m DETACH PARTITION a").is_err());
    db.execute("ALTER TABLE m DETACH PARTITION m_1 CONCURRENTLY").unwrap();
    assert_eq!(count(&db, "m_1"), 1);

    db.execute("DROP TABLE m").unwrap();
    assert!(
        db.query("SELECT * FROM b", &[]).is_err(),
        "an attached partition survived its parent"
    );
    assert_eq!(count(&db, "a"), 2, "a detached table went with the parent");
    assert_eq!(count(&db, "m_1"), 1);
}
