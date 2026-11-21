//! A predicate on the partition key restricts which partitions are scanned.
//! Measured two ways: `EXPLAIN` names only the partitions that will be
//! scanned, and `last_pruning()` reports how many of the table's partitions the
//! executed plan scans.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use heliosdb_nano::sql::partition_plan::{last_pruning, reset_last_pruning};
use heliosdb_nano::{EmbeddedDatabase, Value};

const PARTITIONS: usize = 64;

fn many_partitions() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE big (id INT NOT NULL, v INT) PARTITION BY RANGE (id)")
        .unwrap();
    for p in 0..PARTITIONS {
        db.execute(&format!(
            "CREATE TABLE big_{p} PARTITION OF big FOR VALUES FROM ({}) TO ({})",
            p * 100,
            (p + 1) * 100
        ))
        .unwrap();
    }
    for p in 0..PARTITIONS {
        db.execute(&format!(
            "INSERT INTO big VALUES ({}, {p}), ({}, {p})",
            p * 100 + 1,
            p * 100 + 50
        ))
        .unwrap();
    }
    db
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int2(i) => i64::from(*i),
        Value::Int4(i) => i64::from(*i),
        Value::Int8(i) => *i,
        other => panic!("not an integer: {other:?}"),
    }
}

fn scanned(db: &EmbeddedDatabase, sql: &str, params: &[Value]) -> (Vec<i64>, usize) {
    reset_last_pruning();
    let rows = if params.is_empty() {
        db.query(sql, &[]).unwrap()
    } else {
        db.query_params(sql, params).unwrap()
    };
    let (total, kept) = last_pruning().unwrap_or_else(|| panic!("{sql}: no partitioned scan ran"));
    assert_eq!(total, PARTITIONS, "{sql}");
    let mut values: Vec<i64> = rows.iter().map(|r| int(&r.values[0])).collect();
    values.sort_unstable();
    (values, kept)
}

#[test]
fn a_point_query_touches_one_partition_of_many() {
    let db = many_partitions();
    let (rows, kept) = scanned(&db, "SELECT v FROM big WHERE id = 1750", &[]);
    assert_eq!(rows, vec![17]);
    assert_eq!(kept, 1, "a point query scanned {kept} of {PARTITIONS} partitions");
    // with the key bound at execution
    let (rows, kept) = scanned(&db, "SELECT v FROM big WHERE id = $1", &[Value::Int4(6301)]);
    assert_eq!(rows, vec![63]);
    assert_eq!(kept, 1);
    let (rows, kept) = scanned(&db, "SELECT v FROM big WHERE id = $1", &[Value::Int4(101)]);
    assert_eq!(rows, vec![1]);
    assert_eq!(kept, 1);
    // a key no partition holds scans nothing and returns nothing
    let (rows, kept) = scanned(&db, "SELECT v FROM big WHERE id = 999999", &[]);
    assert!(rows.is_empty());
    assert_eq!(kept, 0);
    // without a predicate on the key every partition is scanned
    reset_last_pruning();
    assert_eq!(db.query("SELECT v FROM big WHERE v = 17", &[]).unwrap().len(), 2);
    assert!(matches!(last_pruning(), None | Some((PARTITIONS, PARTITIONS))));
}

#[test]
fn ranges_and_in_lists_prune() {
    let db = many_partitions();
    let (rows, kept) = scanned(&db, "SELECT id FROM big WHERE id >= 300 AND id < 500", &[]);
    assert_eq!(rows, vec![301, 350, 401, 450]);
    assert_eq!(kept, 2);
    let (rows, kept) = scanned(&db, "SELECT id FROM big WHERE id BETWEEN 250 AND 301", &[]);
    assert_eq!(rows, vec![250, 301]);
    assert_eq!(kept, 2);
    let (rows, kept) = scanned(&db, "SELECT id FROM big WHERE id IN (1, 1050, 6350)", &[]);
    assert_eq!(rows, vec![1, 1050, 6350]);
    assert_eq!(kept, 3);
    let (rows, kept) = scanned(&db, "SELECT id FROM big WHERE id < 100", &[]);
    assert_eq!(rows, vec![1, 50]);
    assert_eq!(kept, 1);
    let (rows, kept) = scanned(
        &db,
        "SELECT id FROM big WHERE id > $1 AND id <= $2",
        &[Value::Int4(6249), Value::Int4(6301)],
    );
    assert_eq!(rows, vec![6250, 6301]);
    assert_eq!(kept, 2);
    // a predicate that mixes the key with another column still prunes on the key
    let (rows, kept) = scanned(&db, "SELECT id FROM big WHERE id < 200 AND v = 1", &[]);
    assert_eq!(rows, vec![101, 150]);
    assert_eq!(kept, 2);
    // OR is not used for pruning, and loses no row
    let (rows, kept) = scanned(&db, "SELECT id FROM big WHERE id = 1 OR id = 6350", &[]);
    assert_eq!(rows, vec![1, 6350]);
    assert_eq!(kept, PARTITIONS);
    // aggregates and aliases above the scan
    let (rows, kept) = scanned(&db, "SELECT count(*) FROM big b WHERE b.id >= 6300", &[]);
    assert_eq!(rows, vec![2]);
    assert_eq!(kept, 1);
}

#[test]
fn explain_shows_the_partitions_that_will_be_scanned() {
    let db = many_partitions();
    let text = |sql: &str| -> String {
        db.query(sql, &[])
            .unwrap()
            .iter()
            .map(|r| format!("{:?}", r.values))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let plan = text("EXPLAIN SELECT v FROM big WHERE id = 1750");
    assert!(plan.contains("big_17"), "{plan}");
    assert!(
        !plan.contains("big_16") && !plan.contains("big_18") && !plan.contains("big_0"),
        "{plan}"
    );
    let plan = text("EXPLAIN SELECT v FROM big WHERE id >= 6200");
    assert!(plan.contains("big_62") && plan.contains("big_63"), "{plan}");
    assert!(!plan.contains("big_61"), "{plan}");
    let plan = text("EXPLAIN SELECT v FROM big");
    assert!(plan.contains("big_0") && plan.contains("big_63"), "{plan}");
}

#[test]
fn list_hash_default_and_sub_partitions_prune_without_losing_rows() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE l (k TEXT, n INT) PARTITION BY LIST (k)")
        .unwrap();
    db.execute("CREATE TABLE l_a PARTITION OF l FOR VALUES IN ('a')")
        .unwrap();
    db.execute("CREATE TABLE l_b PARTITION OF l FOR VALUES IN ('b') PARTITION BY RANGE (n)")
        .unwrap();
    db.execute("CREATE TABLE l_b_lo PARTITION OF l_b FOR VALUES FROM (0) TO (10)")
        .unwrap();
    db.execute("CREATE TABLE l_b_hi PARTITION OF l_b FOR VALUES FROM (10) TO (20)")
        .unwrap();
    db.execute("CREATE TABLE l_d PARTITION OF l DEFAULT").unwrap();
    db.execute("INSERT INTO l VALUES ('a', 1), ('b', 2), ('b', 12), ('z', 3), (NULL, 4)")
        .unwrap();
    let run = |sql: &str| -> (Vec<i64>, (usize, usize)) {
        reset_last_pruning();
        let mut v: Vec<i64> = db.query(sql, &[]).unwrap().iter().map(|r| int(&r.values[0])).collect();
        v.sort_unstable();
        (v, last_pruning().unwrap())
    };
    assert_eq!(run("SELECT n FROM l WHERE k = 'a'"), (vec![1], (4, 1)));
    assert_eq!(run("SELECT n FROM l WHERE k = 'b'"), (vec![2, 12], (4, 2)));
    assert_eq!(run("SELECT n FROM l WHERE k = 'b' AND n >= 10"), (vec![12], (4, 1)));
    assert_eq!(run("SELECT n FROM l WHERE k = 'z'"), (vec![3], (4, 1)));
    assert_eq!(run("SELECT n FROM l WHERE k IS NULL"), (vec![4], (4, 1)));
    assert_eq!(run("SELECT n FROM l WHERE k IN ('a', 'z')"), (vec![1, 3], (4, 2)));
    assert_eq!(run("SELECT n FROM l WHERE k > 'a'").0, vec![2, 3, 12]);

    db.execute("CREATE TABLE h (id INT, n INT) PARTITION BY HASH (id)")
        .unwrap();
    for r in 0..8 {
        db.execute(&format!(
            "CREATE TABLE h_{r} PARTITION OF h FOR VALUES WITH (MODULUS 8, REMAINDER {r})"
        ))
        .unwrap();
    }
    for id in 0..80 {
        db.execute(&format!("INSERT INTO h VALUES ({id}, {id})")).unwrap();
    }
    for id in [0, 13, 79] {
        assert_eq!(run(&format!("SELECT n FROM h WHERE id = {id}")), (vec![id], (8, 1)));
    }
    assert_eq!(run("SELECT count(*) FROM h WHERE id < 40"), (vec![40], (8, 8)));
}

#[test]
fn update_and_delete_through_the_parent_reach_only_matching_rows() {
    let db = many_partitions();
    assert_eq!(db.execute("UPDATE big SET v = -1 WHERE id = 1750").unwrap(), 1);
    assert_eq!(db.execute("DELETE FROM big WHERE id >= 6300").unwrap(), 2);
    assert_eq!(
        db.execute_params("DELETE FROM big WHERE id = $1", &[Value::Int4(1)])
            .unwrap(),
        1
    );
    let rows = db.query("SELECT count(*) FROM big", &[]).unwrap();
    assert_eq!(int(&rows[0].values[0]), (PARTITIONS as i64) * 2 - 3);
    let rows = db.query("SELECT v FROM big_17 WHERE id = 1750", &[]).unwrap();
    assert_eq!(int(&rows[0].values[0]), -1);
}
