//! Round-3 PARTITION BY — Stage 0 (parse-accept + flatten).
//!
//! Stage-0 semantics (see docs/plans / charter-partition-*): the parent is an
//! ordinary empty table (its `PARTITION BY` clause is stripped); each
//! `PARTITION OF` child is an independent, self-consistent plain table cloning
//! the parent's columns (types + NOT NULL); `ATTACH`/`DETACH PARTITION` are
//! accepted no-ops. No tuple routing / parent-scan union yet — that is Stage 1.
//!
//! These exercise the EmbeddedDatabase surface end-to-end (parser rewrite →
//! planner column-copy → executor), the shape the pgrust corpus hits hardest.

use heliosdb_nano::{EmbeddedDatabase, Result, Value};

/// Fetch the single-column `relkind` char pg_class reports for a relation.
fn relkind(db: &EmbeddedDatabase, relname: &str) -> String {
    let rows = db
        .query(
            &format!("SELECT relkind FROM pg_class WHERE relname = '{relname}'"),
            &[],
        )
        .expect("pg_class query");
    assert!(!rows.is_empty(), "pg_class is missing relation {relname}");
    match &rows[0].values[0] {
        Value::String(s) => s.clone(),
        other => panic!("relkind not text for {relname}: {other:?}"),
    }
}

#[test]
fn range_parent_and_child_round_trip() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    // Single-column RANGE parent already parses today; still flattens to a
    // plain empty table.
    db.execute("CREATE TABLE r_parent (id INT NOT NULL, label TEXT) PARTITION BY RANGE (id)")?;
    db.execute("CREATE TABLE r_child PARTITION OF r_parent FOR VALUES FROM (0) TO (100)")?;

    // Child cloned the parent's columns → direct INSERT/SELECT work.
    db.execute("INSERT INTO r_child (id, label) VALUES (5, 'hello')")?;
    let rows = db.query("SELECT id, label FROM r_child", &[])?;
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0].values[0], Value::Int4(5) | Value::Int8(5)));

    // Both introspect as ordinary tables (relkind 'r') at Stage 0.
    assert_eq!(relkind(&db, "r_parent"), "r");
    assert_eq!(relkind(&db, "r_child"), "r");
    Ok(())
}

#[test]
fn list_parent_and_child_in_bounds() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE l_parent (k INT, v TEXT) PARTITION BY LIST (k)")?;
    db.execute("CREATE TABLE l_child PARTITION OF l_parent FOR VALUES IN (1, 2, 3)")?;
    db.execute("INSERT INTO l_child (k, v) VALUES (2, 'two')")?;
    let rows = db.query("SELECT k, v FROM l_child", &[])?;
    assert_eq!(rows.len(), 1);
    Ok(())
}

#[test]
fn hash_parent_with_opclass_key() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    // Opclass token in the key is a hard parse-reject pre-change; the whole
    // clause strips uniformly.
    db.execute("CREATE TABLE h_parent (a INT, b TEXT) PARTITION BY HASH (a part_test_int4_ops)")?;
    db.execute("CREATE TABLE h_child PARTITION OF h_parent FOR VALUES WITH (MODULUS 4, REMAINDER 0)")?;
    db.execute("INSERT INTO h_child (a, b) VALUES (7, 'x')")?;
    assert_eq!(db.query("SELECT a FROM h_child", &[])?.len(), 1);
    Ok(())
}

#[test]
fn expression_key_parent() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    // Multi-column / expression key: another hard parse-reject pre-change.
    db.execute("CREATE TABLE e_parent (a INT, b INT) PARTITION BY RANGE (a, (b + 0))")?;
    db.execute("CREATE TABLE e_child PARTITION OF e_parent FOR VALUES FROM (0, 0) TO (10, 10)")?;
    db.execute("INSERT INTO e_child (a, b) VALUES (1, 2)")?;
    assert_eq!(db.query("SELECT a, b FROM e_child", &[])?.len(), 1);
    Ok(())
}

#[test]
fn default_and_subpartitioned_default_child() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE d_parent (k INT, v TEXT) PARTITION BY LIST (k)")?;
    db.execute("CREATE TABLE d_named PARTITION OF d_parent FOR VALUES IN (1)")?;
    // A DEFAULT child that is itself sub-partitioned: its own PARTITION BY tail
    // must strip too, leaving a plain standalone clone.
    db.execute("CREATE TABLE d_default PARTITION OF d_parent DEFAULT PARTITION BY RANGE (k)")?;
    // A grandchild of the sub-partitioned DEFAULT child clones ITS columns.
    db.execute("CREATE TABLE d_default_c PARTITION OF d_default FOR VALUES FROM (100) TO (200)")?;

    db.execute("INSERT INTO d_default_c (k, v) VALUES (150, 'g')")?;
    assert_eq!(db.query("SELECT k, v FROM d_default_c", &[])?.len(), 1);
    Ok(())
}

#[test]
fn schema_qualified_parent_and_child() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    // Nano collapses any schema qualifier to the bare table name; the child
    // must still resolve the parent's columns.
    db.execute("CREATE TABLE stats_import.part_parent (id INT NOT NULL, note TEXT) PARTITION BY RANGE (id)")?;
    db.execute(
        "CREATE TABLE stats_import.part_child_1 PARTITION OF stats_import.part_parent \
         FOR VALUES FROM (0) TO (10) WITH (autovacuum_enabled = false)",
    )?;
    db.execute("INSERT INTO part_child_1 (id, note) VALUES (3, 'n')")?;
    assert_eq!(db.query("SELECT id, note FROM part_child_1", &[])?.len(), 1);
    Ok(())
}

#[test]
fn drop_in_both_orders() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    // child-then-parent
    db.execute("CREATE TABLE p1 (a INT) PARTITION BY RANGE (a)")?;
    db.execute("CREATE TABLE p1_c PARTITION OF p1 FOR VALUES FROM (0) TO (10)")?;
    db.execute("DROP TABLE p1_c")?;
    db.execute("DROP TABLE p1")?;

    // parent-then-child (children are independent tables, so either order works)
    db.execute("CREATE TABLE p2 (a INT) PARTITION BY RANGE (a)")?;
    db.execute("CREATE TABLE p2_c PARTITION OF p2 FOR VALUES FROM (0) TO (10)")?;
    db.execute("DROP TABLE p2")?;
    db.execute("DROP TABLE p2_c")?;
    Ok(())
}

#[test]
fn attach_and_detach_are_noops() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE ad_parent (a INT) PARTITION BY RANGE (a)")?;
    db.execute("CREATE TABLE ad_child PARTITION OF ad_parent FOR VALUES FROM (0) TO (10)")?;

    // ATTACH / DETACH PARTITION accepted as no-ops (return 0 affected rows).
    assert_eq!(
        db.execute("ALTER TABLE ad_parent ATTACH PARTITION ad_child FOR VALUES FROM (0) TO (10)")?,
        0
    );
    assert_eq!(db.execute("ALTER TABLE ad_parent DETACH PARTITION ad_child")?, 0);
    assert_eq!(db.execute("ALTER TABLE ad_parent DETACH PARTITION ad_child CONCURRENTLY")?, 0);
    // ALTER INDEX ... ATTACH PARTITION also a no-op.
    assert_eq!(db.execute("ALTER INDEX ad_idx ATTACH PARTITION ad_child_idx")?, 0);

    // The child is untouched by the no-ops and still queryable.
    db.execute("INSERT INTO ad_child (a) VALUES (1)")?;
    assert_eq!(db.query("SELECT a FROM ad_child", &[])?.len(), 1);
    Ok(())
}

#[test]
fn child_of_missing_parent_errors_cleanly() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    // No parent created — must surface a clean relation-not-found error, never
    // a panic.
    let err = db
        .execute("CREATE TABLE orphan PARTITION OF nonexistent_parent FOR VALUES IN (1)")
        .expect_err("missing parent must error");
    let msg = err.to_string();
    assert!(
        msg.contains("does not exist"),
        "expected a relation-not-found error, got: {msg}"
    );
}

#[test]
fn empty_column_inherits_is_not_treated_as_partition() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE base_tbl (a INT, b TEXT)")?;
    // `CREATE TABLE fail () INHERITS (base_tbl)` also has empty columns, but is
    // NOT a PARTITION OF — the partition column-copy must not fire for it. (If
    // the engine rejects a zero-column table outright that pre-existing
    // behavior is unchanged; the point is only that base_tbl's columns are not
    // injected via the partition path.)
    if db.execute("CREATE TABLE fail () INHERITS (base_tbl)").is_ok() {
        assert!(
            db.query("SELECT a FROM fail", &[]).is_err(),
            "empty INHERITS table must not gain parent columns via the partition path"
        );
    }
    Ok(())
}

#[test]
fn multi_statement_batch_two_parents_no_cross_contamination() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE mp1 (a INT, b TEXT) PARTITION BY RANGE (a)")?;
    db.execute("CREATE TABLE mp2 (x INT, y INT) PARTITION BY LIST (x)")?;

    // Two PARTITION OF children of DIFFERENT parents in one batch — each
    // statement is parsed/planned with its own original SQL, so the per-call
    // side-channel must resolve each child against the correct parent.
    db.execute_batch(&[
        "CREATE TABLE mp1_c PARTITION OF mp1 FOR VALUES FROM (0) TO (100)",
        "CREATE TABLE mp2_c PARTITION OF mp2 FOR VALUES IN (1, 2, 3)",
    ])?;

    db.execute("INSERT INTO mp1_c (a, b) VALUES (5, 'hi')")?;
    db.execute("INSERT INTO mp2_c (x, y) VALUES (1, 42)")?;

    // mp1_c has mp1's columns (a, b); mp2_c has mp2's columns (x, y).
    assert_eq!(db.query("SELECT a, b FROM mp1_c", &[])?.len(), 1);
    assert_eq!(db.query("SELECT x, y FROM mp2_c", &[])?.len(), 1);
    // Cross columns must NOT exist on the wrong child.
    assert!(db.query("SELECT x FROM mp1_c", &[]).is_err());
    assert!(db.query("SELECT a FROM mp2_c", &[]).is_err());
    Ok(())
}
