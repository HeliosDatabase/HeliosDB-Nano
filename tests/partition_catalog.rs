//! The partition catalog is durable and the PostgreSQL catalogs describe it:
//! `pg_class.relkind` / `relispartition` / `relpartbound`, `pg_inherits`,
//! `pg_partitioned_table`, `pg_get_partkeydef`. It survives a restart, a
//! rename, a dump and restore in both formats, and a physical snapshot open.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use heliosdb_nano::{Config, EmbeddedDatabase, Value};

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "NULL".to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Int2(i) => i.to_string(),
        Value::Int4(i) => i.to_string(),
        Value::Int8(i) => i.to_string(),
        other => format!("{other:?}"),
    }
}

fn rows(db: &EmbeddedDatabase, sql: &str) -> Vec<Vec<String>> {
    db.query(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .iter()
        .map(|r| r.values.iter().map(text).collect())
        .collect()
}

fn build(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE sales (id INT NOT NULL, region TEXT NOT NULL, amount INT, PRIMARY KEY (id, region)) PARTITION BY LIST (region)")
        .unwrap();
    db.execute("CREATE TABLE sales_eu PARTITION OF sales FOR VALUES IN ('eu', 'uk') PARTITION BY RANGE (id)")
        .unwrap();
    db.execute("CREATE TABLE sales_eu_1 PARTITION OF sales_eu FOR VALUES FROM (MINVALUE) TO (1000)")
        .unwrap();
    db.execute("CREATE TABLE sales_eu_2 PARTITION OF sales_eu FOR VALUES FROM (1000) TO (MAXVALUE)")
        .unwrap();
    db.execute("CREATE TABLE sales_rest PARTITION OF sales DEFAULT")
        .unwrap();
    db.execute("CREATE TABLE h (k BIGINT) PARTITION BY HASH (k)").unwrap();
    db.execute("CREATE TABLE h_0 PARTITION OF h FOR VALUES WITH (MODULUS 2, REMAINDER 0)")
        .unwrap();
    db.execute("CREATE TABLE h_1 PARTITION OF h FOR VALUES WITH (MODULUS 2, REMAINDER 1)")
        .unwrap();
    db.execute("CREATE TABLE plain (id INT)").unwrap();
    db.execute("INSERT INTO sales VALUES (1, 'eu', 10), (2000, 'uk', 20), (3, 'us', 30)")
        .unwrap();
    db.execute("INSERT INTO h VALUES (1), (2), (3), (4), (5)").unwrap();
}

fn check(db: &EmbeddedDatabase) {
    assert_eq!(
        rows(
            db,
            "SELECT relname, relkind, relispartition, relpartbound FROM pg_class \
             WHERE relname IN ('sales', 'sales_eu', 'sales_eu_1', 'sales_eu_2', 'sales_rest', 'plain', 'h_1') \
             ORDER BY relname"
        ),
        vec![
            vec!["h_1", "r", "true", "FOR VALUES WITH (modulus 2, remainder 1)"],
            vec!["plain", "r", "false", "NULL"],
            vec!["sales", "p", "false", "NULL"],
            vec!["sales_eu", "p", "true", "FOR VALUES IN ('eu', 'uk')"],
            vec!["sales_eu_1", "r", "true", "FOR VALUES FROM (MINVALUE) TO (1000)"],
            vec!["sales_eu_2", "r", "true", "FOR VALUES FROM (1000) TO (MAXVALUE)"],
            vec!["sales_rest", "r", "true", "DEFAULT"],
        ]
    );
    assert_eq!(
        rows(
            db,
            "SELECT c.relname, p.relname FROM pg_inherits i \
             JOIN pg_class c ON c.oid = i.inhrelid JOIN pg_class p ON p.oid = i.inhparent \
             ORDER BY c.relname"
        ),
        vec![
            vec!["h_0", "h"],
            vec!["h_1", "h"],
            vec!["sales_eu", "sales"],
            vec!["sales_eu_1", "sales_eu"],
            vec!["sales_eu_2", "sales_eu"],
            vec!["sales_rest", "sales"],
        ]
    );
    assert_eq!(
        rows(
            db,
            "SELECT c.relname, t.partstrat, t.partnatts, t.partattrs, pg_get_partkeydef(c.oid) \
             FROM pg_partitioned_table t JOIN pg_class c ON c.oid = t.partrelid ORDER BY c.relname"
        ),
        vec![
            vec!["h", "h", "1", "1", "HASH (k)"],
            vec!["sales", "l", "1", "2", "LIST (region)"],
            vec!["sales_eu", "r", "1", "1", "RANGE (id)"],
        ]
    );
    // the implicit constraints of partitioning are not user constraints
    for view in [
        "SELECT constraint_name FROM information_schema.check_constraints",
        "SELECT constraint_name FROM information_schema.table_constraints",
        "SELECT conname FROM pg_constraint",
    ] {
        for row in rows(db, view) {
            assert!(!row[0].contains("__heliosdb_partition"), "{view}: {row:?}");
        }
    }
    // and the data is where the catalog says
    assert_eq!(rows(db, "SELECT count(*) FROM sales"), vec![vec!["3"]]);
    assert_eq!(rows(db, "SELECT id FROM sales_eu_2"), vec![vec!["2000"]]);
    assert_eq!(rows(db, "SELECT id FROM sales_rest"), vec![vec!["3"]]);
    assert_eq!(rows(db, "SELECT count(*) FROM h"), vec![vec!["5"]]);
    assert_eq!(
        rows(db, "SELECT amount FROM sales WHERE region = 'uk'"),
        vec![vec!["20"]]
    );
    // routing and bounds still work
    db.execute("INSERT INTO sales VALUES (5, 'uk', 50)").unwrap();
    assert_eq!(
        rows(db, "SELECT id FROM sales_eu_1 ORDER BY id"),
        vec![vec!["1"], vec!["5"]]
    );
    assert!(db.execute("INSERT INTO sales_eu_1 VALUES (6, 'us', 1)").is_err());
    assert!(
        db.execute("INSERT INTO sales VALUES (5, 'uk', 51)").is_err(),
        "primary key lost"
    );
    db.execute("DELETE FROM sales WHERE id = 5").unwrap();
}

#[test]
fn catalogs_describe_partitioned_tables() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    build(&db);
    check(&db);
}

#[test]
fn the_catalog_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        build(&db);
    }
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    check(&db);
}

#[test]
fn a_dropped_partition_and_a_dropped_parent_leave_no_record() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    build(&db);
    db.execute("DROP TABLE sales_rest").unwrap();
    assert_eq!(
        rows(
            &db,
            "SELECT count(*) FROM pg_inherits i JOIN pg_class p ON p.oid = i.inhparent WHERE p.relname = 'sales'"
        ),
        vec![vec!["1"]]
    );
    db.execute("DROP TABLE sales").unwrap();
    assert_eq!(
        rows(&db, "SELECT relname FROM pg_class WHERE relname LIKE 'sales%'"),
        Vec::<Vec<String>>::new()
    );
    // the names are free again, as ordinary tables
    db.execute("CREATE TABLE sales (id INT)").unwrap();
    db.execute("CREATE TABLE sales_eu (id INT)").unwrap();
    db.execute("INSERT INTO sales VALUES (1)").unwrap();
    db.execute("INSERT INTO sales_eu VALUES (99999)").unwrap();
    assert_eq!(
        rows(
            &db,
            "SELECT relkind, relispartition FROM pg_class WHERE relname IN ('sales', 'sales_eu')"
        ),
        vec![vec!["r", "false"], vec!["r", "false"]]
    );
    assert_eq!(rows(&db, "SELECT count(*) FROM sales"), vec![vec!["1"]]);
}

#[test]
fn renaming_a_partition_or_a_parent_keeps_it_partitioned() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE m (id INT NOT NULL) PARTITION BY RANGE (id)")
        .unwrap();
    db.execute("CREATE TABLE m_1 PARTITION OF m FOR VALUES FROM (0) TO (10)")
        .unwrap();
    db.execute("INSERT INTO m VALUES (1)").unwrap();
    db.execute("ALTER TABLE m_1 RENAME TO m_first").unwrap();
    db.execute("INSERT INTO m VALUES (2)").unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM m_first"), vec![vec!["2"]]);
    assert!(db.execute("INSERT INTO m_first VALUES (50)").is_err());
    db.execute("ALTER TABLE m RENAME TO measurements").unwrap();
    db.execute("INSERT INTO measurements VALUES (3)").unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM measurements"), vec![vec!["3"]]);
    assert!(db.execute("INSERT INTO measurements VALUES (50)").is_err());
    assert_eq!(
        rows(&db, "SELECT relname, relpartbound FROM pg_class WHERE relispartition"),
        vec![vec!["m_first", "FOR VALUES FROM (0) TO (10)"]]
    );
}

#[test]
fn dump_and_restore_rebuild_the_partitions_in_both_formats() {
    let src = EmbeddedDatabase::new_in_memory().unwrap();
    build(&src);
    let dir = tempfile::tempdir().unwrap();

    let sql_path = dir.path().join("dump.sql");
    src.dump_sql(&sql_path).unwrap();
    let script = std::fs::read_to_string(&sql_path).unwrap();
    assert!(
        script.contains("PARTITION BY LIST (\"region\")") || script.contains("PARTITION BY LIST (region)"),
        "{script}"
    );
    assert!(script.contains("ATTACH PARTITION"), "{script}");
    assert!(!script.contains("__heliosdb_partition"), "{script}");
    let from_sql = EmbeddedDatabase::new_in_memory().unwrap();
    from_sql
        .execute_sql_script(&script)
        .unwrap_or_else(|e| panic!("sql restore failed: {e}\n----\n{script}"));
    check(&from_sql);

    let bin_path = dir.path().join("dump.heliodump");
    src.dump_full(&bin_path).unwrap();
    let mut from_binary = EmbeddedDatabase::new_in_memory().unwrap();
    from_binary.restore_from_dump(&bin_path).unwrap();
    check(&from_binary);
}

#[test]
fn a_physical_snapshot_of_a_partitioned_database_reads_through_the_parent() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("primary");
    let checkpoint = root.path().join("checkpoint");
    let config = |path: &std::path::Path| {
        let mut config = Config::default();
        config.storage.path = Some(path.to_path_buf());
        config.storage.memory_only = false;
        config
    };
    {
        let db = EmbeddedDatabase::with_config(config(&source)).unwrap();
        build(&db);
        let raw = db.storage.db();
        rocksdb::checkpoint::Checkpoint::new(&raw)
            .unwrap()
            .create_checkpoint(&checkpoint)
            .unwrap();
    }
    let hydrated = EmbeddedDatabase::open_physical_snapshot(config(&checkpoint)).unwrap();
    let db = &hydrated.database;
    assert_eq!(rows(db, "SELECT count(*) FROM sales"), vec![vec!["3"]]);
    assert_eq!(
        rows(db, "SELECT amount FROM sales WHERE region = 'uk'"),
        vec![vec!["20"]]
    );
    assert_eq!(rows(db, "SELECT count(*) FROM pg_inherits"), vec![vec!["6"]]);
    assert_eq!(
        rows(db, "SELECT relkind FROM pg_class WHERE relname = 'sales'"),
        vec![vec!["p"]]
    );
}

#[test]
fn branches_and_time_travel_read_partitioned_tables() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE m (id INT NOT NULL, v INT) PARTITION BY RANGE (id)")
        .unwrap();
    db.execute("CREATE TABLE m_1 PARTITION OF m FOR VALUES FROM (0) TO (10)")
        .unwrap();
    db.execute("CREATE TABLE m_2 PARTITION OF m FOR VALUES FROM (10) TO (20)")
        .unwrap();
    db.execute("INSERT INTO m VALUES (1, 1), (11, 1)").unwrap();
    db.execute("CREATE DATABASE BRANCH dev FROM main AS OF NOW").unwrap();
    db.execute("USE BRANCH dev").unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM m"), vec![vec!["2"]]);
    db.execute("INSERT INTO m VALUES (2, 2), (12, 2)").unwrap();
    assert!(db.execute("INSERT INTO m VALUES (99, 2)").is_err());
    assert!(db.execute("INSERT INTO m_1 VALUES (15, 2)").is_err());
    db.execute("UPDATE m SET v = 9 WHERE id = 12").unwrap();
    db.execute("DELETE FROM m WHERE id = 1").unwrap();
    assert_eq!(
        rows(&db, "SELECT id FROM m ORDER BY id"),
        vec![vec!["2"], vec!["11"], vec!["12"]]
    );
    // partition DDL is refused on a branch
    assert!(db
        .execute("CREATE TABLE m_3 PARTITION OF m FOR VALUES FROM (20) TO (30)")
        .is_err());
    assert!(db.execute("CREATE TABLE b (id INT) PARTITION BY RANGE (id)").is_err());
    db.execute("USE BRANCH main").unwrap();
    assert_eq!(rows(&db, "SELECT id FROM m ORDER BY id"), vec![vec!["1"], vec!["11"]]);
}
