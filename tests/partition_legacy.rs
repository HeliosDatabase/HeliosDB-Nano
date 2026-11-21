//! A database written by a schema-only release: a "partitioned" table there is
//! an ordinary table that holds all its rows, linked to "partitions" that are
//! ordinary tables too, with no key and no bound recorded anywhere.
//!
//! The legacy state is built the way that release built it: plain tables, and
//! the parent-to-child links written through `Catalog::register_partition_child`
//! (the only partition record that release had). Opening such a database must
//! not lose or hide a row, and the operator can turn the table into a
//! partitioned one: `ALTER TABLE … PARTITION BY`, `ATTACH PARTITION … FOR
//! VALUES`, `ALTER TABLE … REDISTRIBUTE`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use heliosdb_nano::storage::partition_catalog;
use heliosdb_nano::{EmbeddedDatabase, Value};

fn ints(db: &EmbeddedDatabase, sql: &str) -> Vec<i64> {
    db.query(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .iter()
        .map(|r| match &r.values[0] {
            Value::Int2(i) => i64::from(*i),
            Value::Int4(i) => i64::from(*i),
            Value::Int8(i) => *i,
            other => panic!("not an integer: {other:?}"),
        })
        .collect()
}

/// What a schema-only release left on disk for
/// `CREATE TABLE ev (…) PARTITION BY RANGE (id)` plus two `PARTITION OF`
/// children, after an application wrote every row through the parent and a few
/// straight to a child.
fn legacy_database(path: &std::path::Path) {
    let db = EmbeddedDatabase::new(path).unwrap();
    db.execute("CREATE TABLE ev (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("CREATE TABLE ev_lo (id INT NOT NULL, v TEXT)").unwrap();
    db.execute("CREATE TABLE ev_hi (id INT NOT NULL, v TEXT)").unwrap();
    let catalog = db.storage.catalog();
    catalog.register_partition_child("ev", "ev_lo").unwrap();
    catalog.register_partition_child("ev", "ev_hi").unwrap();
    db.execute("INSERT INTO ev VALUES (1, 'a'), (2, 'b'), (150, 'c'), (151, 'd')")
        .unwrap();
    db.execute("INSERT INTO ev_lo VALUES (7, 'direct')").unwrap();
}

#[test]
fn a_schema_only_database_opens_with_every_row_where_it_was() {
    let dir = tempfile::tempdir().unwrap();
    legacy_database(dir.path());
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    assert_eq!(
        partition_catalog::schema_only_parents(&db.storage).unwrap(),
        vec!["ev".to_string()],
        "the table an operator has to look at is not named"
    );
    // nothing moved, nothing hidden: the table answers as it did
    assert_eq!(ints(&db, "SELECT id FROM ev ORDER BY id"), vec![1, 2, 150, 151]);
    assert_eq!(ints(&db, "SELECT id FROM ev WHERE id = 150"), vec![150]);
    assert_eq!(ints(&db, "SELECT id FROM ev_lo"), vec![7]);
    db.execute("INSERT INTO ev VALUES (3, 'still plain')").unwrap();
    db.execute("UPDATE ev SET v = 'u' WHERE id = 1").unwrap();
    assert_eq!(ints(&db, "SELECT count(*) FROM ev"), vec![5]);
    let rows = db
        .query(
            "SELECT relkind, relispartition FROM pg_class WHERE relname IN ('ev', 'ev_lo')",
            &[],
        )
        .unwrap();
    for row in &rows {
        assert_eq!(row.values[0], Value::String("r".into()));
        assert_eq!(row.values[1], Value::Boolean(false));
    }
    // a new partition cannot be added before the key is declared
    let err = db
        .execute("CREATE TABLE ev_new PARTITION OF ev FOR VALUES FROM (500) TO (600)")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("is not partitioned") && err.contains("PARTITION BY"),
        "{err}"
    );
    // DROP of the parent still takes the linked children, as it did
    db.execute("DROP TABLE ev").unwrap();
    assert!(db.query("SELECT * FROM ev_lo", &[]).is_err());
}

#[test]
fn a_schema_only_table_is_partitioned_and_redistributed() {
    let dir = tempfile::tempdir().unwrap();
    legacy_database(dir.path());
    let db = EmbeddedDatabase::new(dir.path()).unwrap();

    db.execute("ALTER TABLE ev PARTITION BY RANGE (id)").unwrap();
    assert!(partition_catalog::schema_only_parents(&db.storage).unwrap().is_empty());
    assert_eq!(
        partition_catalog::tables_with_unrouted_rows(&db.storage).unwrap(),
        vec!["ev".to_string()]
    );
    // the rows the table holds stay readable through it
    assert_eq!(ints(&db, "SELECT id FROM ev ORDER BY id"), vec![1, 2, 150, 151]);
    assert_eq!(ints(&db, "SELECT id FROM ev WHERE id = 150"), vec![150]);
    // no partition has a bound yet: a new row has nowhere to go
    assert!(db.execute("INSERT INTO ev VALUES (4, 'x')").is_err());

    // the linked children have no key constraint; the parent's key is required
    let err = db
        .execute("ALTER TABLE ev ATTACH PARTITION ev_lo FOR VALUES FROM (0) TO (100)")
        .unwrap_err()
        .to_string();
    assert!(err.contains("PRIMARY KEY"), "{err}");
    db.execute("ALTER TABLE ev_lo ADD PRIMARY KEY (id)").unwrap();
    db.execute("ALTER TABLE ev_hi ADD PRIMARY KEY (id)").unwrap();
    db.execute("ALTER TABLE ev ATTACH PARTITION ev_lo FOR VALUES FROM (0) TO (100)")
        .unwrap();
    db.execute("ALTER TABLE ev ATTACH PARTITION ev_hi FOR VALUES FROM (100) TO (200)")
        .unwrap();
    // reads: the table's own rows and the partitions' rows
    assert_eq!(ints(&db, "SELECT id FROM ev ORDER BY id"), vec![1, 2, 7, 150, 151]);
    // new rows are routed; updates and deletes reach the table's own rows too
    db.execute("INSERT INTO ev VALUES (4, 'routed')").unwrap();
    assert_eq!(ints(&db, "SELECT id FROM ev_lo ORDER BY id"), vec![4, 7]);
    assert_eq!(db.execute("UPDATE ev SET v = 'u' WHERE id IN (1, 7)").unwrap(), 2);
    assert_eq!(db.execute("DELETE FROM ev WHERE id = 151").unwrap(), 1);

    // survives a restart half way
    drop(db);
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    assert_eq!(
        partition_catalog::tables_with_unrouted_rows(&db.storage).unwrap(),
        vec!["ev".to_string()]
    );
    assert_eq!(ints(&db, "SELECT id FROM ev ORDER BY id"), vec![1, 2, 4, 7, 150]);

    assert_eq!(db.execute("ALTER TABLE ev REDISTRIBUTE").unwrap(), 3);
    assert!(partition_catalog::tables_with_unrouted_rows(&db.storage)
        .unwrap()
        .is_empty());
    assert_eq!(ints(&db, "SELECT id FROM ev ORDER BY id"), vec![1, 2, 4, 7, 150]);
    assert_eq!(ints(&db, "SELECT id FROM ev_lo ORDER BY id"), vec![1, 2, 4, 7]);
    assert_eq!(ints(&db, "SELECT id FROM ev_hi ORDER BY id"), vec![150]);
    assert_eq!(
        db.storage.scan_table("ev").unwrap().len(),
        0,
        "the parent still holds rows"
    );
    assert_eq!(db.execute("ALTER TABLE ev REDISTRIBUTE").unwrap(), 0);
    let rows = db.query("SELECT v FROM ev WHERE id = 1", &[]).unwrap();
    assert_eq!(rows[0].values[0], Value::String("u".into()));
}

#[test]
fn redistribute_moves_all_rows_or_none() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT NOT NULL, v TEXT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (500, 'nowhere')")
        .unwrap();
    db.execute("ALTER TABLE t PARTITION BY RANGE (id)").unwrap();
    db.execute("CREATE TABLE t_1 PARTITION OF t FOR VALUES FROM (0) TO (100)")
        .unwrap();
    let err = db.execute("ALTER TABLE t REDISTRIBUTE").unwrap_err().to_string();
    assert!(err.contains("no partition of relation \"t\" found for row"), "{err}");
    assert_eq!(ints(&db, "SELECT count(*) FROM t_1"), vec![0], "part of the rows moved");
    assert_eq!(ints(&db, "SELECT id FROM t ORDER BY id"), vec![1, 2, 500]);
    db.execute("CREATE TABLE t_d PARTITION OF t DEFAULT").unwrap();
    assert_eq!(db.execute("ALTER TABLE t REDISTRIBUTE").unwrap(), 3);
    assert_eq!(ints(&db, "SELECT id FROM t_1 ORDER BY id"), vec![1, 2]);
    assert_eq!(ints(&db, "SELECT id FROM t_d"), vec![500]);
    assert_eq!(ints(&db, "SELECT id FROM t ORDER BY id"), vec![1, 2, 500]);
    // statements that make no sense
    assert!(db.execute("ALTER TABLE t PARTITION BY RANGE (id)").is_err());
    assert!(db.execute("ALTER TABLE t_1 REDISTRIBUTE").is_err());
    assert!(db.execute("ALTER TABLE nope REDISTRIBUTE").is_err());
}
