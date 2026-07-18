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
    // Schema namespacing (coexistence): a non-`public` qualifier is a REAL
    // namespace now — the child keys as `stats_import.part_child_1` and its
    // parent's columns resolve; the child is addressed by its qualified name.
    db.execute("CREATE TABLE stats_import.part_parent (id INT NOT NULL, note TEXT) PARTITION BY RANGE (id)")?;
    db.execute(
        "CREATE TABLE stats_import.part_child_1 PARTITION OF stats_import.part_parent \
         FOR VALUES FROM (0) TO (10) WITH (autovacuum_enabled = false)",
    )?;
    db.execute("INSERT INTO stats_import.part_child_1 (id, note) VALUES (3, 'n')")?;
    assert_eq!(db.query("SELECT id, note FROM stats_import.part_child_1", &[])?.len(), 1);
    Ok(())
}

#[test]
fn drop_in_both_orders() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    // child-then-parent: dropping the child directly unregisters it (PG parity),
    // then the parent drops with no child left to cascade.
    db.execute("CREATE TABLE p1 (a INT) PARTITION BY RANGE (a)")?;
    db.execute("CREATE TABLE p1_c PARTITION OF p1 FOR VALUES FROM (0) TO (10)")?;
    db.execute("DROP TABLE p1_c")?;
    db.execute("DROP TABLE p1")?;

    // parent-then-child: round-3 makes `DROP TABLE parent` CASCADE to its
    // partition children (PostgreSQL parity — the behavior this feature adds),
    // so the child is already gone after the parent drop. A bare `DROP TABLE
    // p2_c` would now correctly error; an explicit follow-up must use IF EXISTS.
    db.execute("CREATE TABLE p2 (a INT) PARTITION BY RANGE (a)")?;
    db.execute("CREATE TABLE p2_c PARTITION OF p2 FOR VALUES FROM (0) TO (10)")?;
    db.execute("DROP TABLE p2")?;
    assert!(
        db.query("SELECT a FROM p2_c", &[]).is_err(),
        "parent drop cascades to the child (round-3 behavior change)"
    );
    db.execute("DROP TABLE IF EXISTS p2_c")?;
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
    db.execute("CREATE TABLE plain_empty ()")?;
    // `CREATE TABLE fail () INHERITS (base_tbl)` also has empty columns, but is
    // NOT a PARTITION OF — the partition column-copy must not fire for it. The
    // observable is the CATALOG column count: if the partition path had fired,
    // `fail` would register base_tbl's 2 columns; it must instead register
    // exactly what a plain zero-column CREATE registers (whatever that is on
    // this engine — SELECT permissiveness on empty tables is pre-existing
    // behavior this test deliberately does not pin).
    if db.execute("CREATE TABLE fail () INHERITS (base_tbl)").is_ok() {
        let count = |t: &str| -> Result<usize> {
            Ok(db
                .query(
                    &format!(
                        "SELECT column_name FROM information_schema.columns WHERE table_name = '{t}'"
                    ),
                    &[],
                )?
                .len())
        };
        assert_eq!(
            count("fail")?,
            count("plain_empty")?,
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

// --------------------------------------------------------------------------
// Round-3 partition-round3: DROP TABLE parent cascades to its Stage-0
// PARTITION OF children (PostgreSQL parity). Under the flatten, children are
// independent tables that would otherwise ORPHAN when the parent drops, so a
// corpus file that re-creates a child name later hit "table already exists".
// --------------------------------------------------------------------------

/// (a) `DROP TABLE parent` drops BOTH a `FOR VALUES` child and a `DEFAULT`
/// child; the freed child name is then re-creatable as a plain table.
/// FAILS on pre-fix code: the children orphan, so the re-CREATE hits
/// "already exists" and the post-drop SELECTs wrongly succeed.
#[test]
fn drop_parent_drops_children() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE cas_parent (id INT NOT NULL, v TEXT) PARTITION BY RANGE (id)")?;
    db.execute("CREATE TABLE cas_named PARTITION OF cas_parent FOR VALUES FROM (0) TO (100)")?;
    db.execute("CREATE TABLE cas_def PARTITION OF cas_parent DEFAULT")?;

    // Children are usable standalone tables at Stage 0.
    db.execute("INSERT INTO cas_named (id, v) VALUES (5, 'a')")?;
    db.execute("INSERT INTO cas_def (id, v) VALUES (999, 'b')")?;

    // Dropping the parent cascades to every registered child.
    db.execute("DROP TABLE cas_parent")?;
    assert!(
        db.query("SELECT id FROM cas_named", &[]).is_err(),
        "FOR VALUES child must be dropped with its parent"
    );
    assert!(
        db.query("SELECT id FROM cas_def", &[]).is_err(),
        "DEFAULT child must be dropped with its parent"
    );

    // The freed child name is re-creatable as an ordinary (non-partition) table.
    db.execute("CREATE TABLE cas_named (a INT, b INT)")?;
    db.execute("INSERT INTO cas_named (a, b) VALUES (1, 2)")?;
    assert_eq!(db.query("SELECT a, b FROM cas_named", &[])?.len(), 1);
    Ok(())
}

/// (b) A sub-partitioned child cascades RECURSIVELY: dropping the grandparent
/// removes the sub-partitioned child and its own grandchild.
/// FAILS on pre-fix code (no cascade at all).
#[test]
fn sub_partitioned_child_cascades_recursively() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE sp_parent (k INT, v TEXT) PARTITION BY LIST (k)")?;
    // A DEFAULT child that is itself sub-partitioned (its own PARTITION BY tail).
    db.execute("CREATE TABLE sp_mid PARTITION OF sp_parent DEFAULT PARTITION BY RANGE (k)")?;
    db.execute("CREATE TABLE sp_leaf PARTITION OF sp_mid FOR VALUES FROM (100) TO (200)")?;
    db.execute("INSERT INTO sp_leaf (k, v) VALUES (150, 'g')")?;

    // Drop the top parent → the mid child AND the leaf grandchild both go.
    db.execute("DROP TABLE sp_parent")?;
    assert!(
        db.query("SELECT k FROM sp_mid", &[]).is_err(),
        "sub-partitioned child dropped with grandparent"
    );
    assert!(
        db.query("SELECT k FROM sp_leaf", &[]).is_err(),
        "grandchild dropped recursively"
    );

    // Names are free again.
    db.execute("CREATE TABLE sp_leaf (z INT)")?;
    assert_eq!(db.query("SELECT z FROM sp_leaf", &[])?.len(), 0);
    Ok(())
}

/// (c) A child dropped DIRECTLY is unregistered (PG parity): the later parent
/// drop cascades only to the surviving children, and every name re-creates
/// cleanly. PARTIALLY fails on pre-fix code — the direct child drop worked,
/// but the parent drop did not cascade, so the surviving child orphaned and
/// its post-drop SELECT wrongly succeeded / its re-CREATE hit "already exists".
#[test]
fn drop_child_directly_then_parent() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE dc_parent (a INT) PARTITION BY RANGE (a)")?;
    db.execute("CREATE TABLE dc_c1 PARTITION OF dc_parent FOR VALUES FROM (0) TO (10)")?;
    db.execute("CREATE TABLE dc_c2 PARTITION OF dc_parent FOR VALUES FROM (10) TO (20)")?;

    // Drop one child directly — allowed, and it unregisters from the parent.
    db.execute("DROP TABLE dc_c1")?;
    assert!(db.query("SELECT a FROM dc_c1", &[]).is_err());
    // The other child is untouched.
    db.execute("INSERT INTO dc_c2 (a) VALUES (11)")?;
    assert_eq!(db.query("SELECT a FROM dc_c2", &[])?.len(), 1);

    // Dropping the parent cascades to the surviving child only.
    db.execute("DROP TABLE dc_parent")?;
    assert!(
        db.query("SELECT a FROM dc_c2", &[]).is_err(),
        "surviving child cascaded with the parent"
    );

    // Every freed name re-creates cleanly (dc_c1 was already unregistered, so
    // its earlier direct drop must not have left a link that touches this one).
    db.execute("CREATE TABLE dc_c1 (a INT, b INT)")?;
    assert_eq!(db.query("SELECT a, b FROM dc_c1", &[])?.len(), 0);
    db.execute("CREATE TABLE dc_parent (a INT) PARTITION BY RANGE (a)")?;
    db.execute("CREATE TABLE dc_c2 PARTITION OF dc_parent FOR VALUES FROM (0) TO (10)")?;
    Ok(())
}

/// (d) Schema-qualified parent + child: registration and drop-lookup normalize
/// IDENTICALLY (both key `sq.<name>`), so `DROP TABLE sq.parent` cascades to
/// `sq.child` through the Stage-0 partition registry. With real schema
/// namespacing the qualified `sq.pk11` and the bare `pk11` are DISTINCT tables
/// (coexistence), so the bare name stays free for an independent table.
#[test]
fn schema_qualified_parent_child_cascade() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE sq.fkpart11 (id INT NOT NULL, note TEXT) PARTITION BY RANGE (id)")?;
    db.execute("CREATE TABLE sq.pk11 PARTITION OF sq.fkpart11 FOR VALUES FROM (0) TO (10)")?;
    db.execute("INSERT INTO sq.pk11 (id, note) VALUES (3, 'n')")?;
    assert_eq!(db.query("SELECT id FROM sq.pk11", &[])?.len(), 1);

    // Drop the schema-qualified parent; the schema-qualified child cascades.
    db.execute("DROP TABLE sq.fkpart11")?;
    assert!(
        db.query("SELECT id FROM sq.pk11", &[]).is_err(),
        "schema-qualified child cascaded (qualified view)"
    );

    // The bare name `pk11` is a separate namespace — creatable and independent.
    db.execute("CREATE TABLE pk11 (x INT)")?;
    assert_eq!(db.query("SELECT x FROM pk11", &[])?.len(), 0);
    Ok(())
}

/// (e) `DROP TABLE IF EXISTS parent` twice, plus a multi-table DROP list. The
/// first drop cascades and clears the registry; the second is a clean no-op;
/// a multi-table list mixing a partitioned parent with a missing name works.
/// The second `IF EXISTS` drop errors on pre-fix code only if a stale link is
/// left; here it exercises that the registry is fully cleaned.
#[test]
fn drop_if_exists_parent_twice_and_multi() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE ie_parent (a INT) PARTITION BY RANGE (a)")?;
    db.execute("CREATE TABLE ie_child PARTITION OF ie_parent FOR VALUES FROM (0) TO (10)")?;

    // First drop cascades and removes the registry entries.
    db.execute("DROP TABLE IF EXISTS ie_parent")?;
    assert!(db.query("SELECT a FROM ie_child", &[]).is_err());
    // Second drop is a clean no-op (no stale registry, no error).
    assert_eq!(db.execute("DROP TABLE IF EXISTS ie_parent")?, 0);

    // Re-create and drop via a multi-table list that also names a missing table.
    db.execute("CREATE TABLE ie_parent (a INT) PARTITION BY RANGE (a)")?;
    db.execute("CREATE TABLE ie_child PARTITION OF ie_parent FOR VALUES FROM (0) TO (10)")?;
    db.execute("DROP TABLE IF EXISTS ie_parent, ie_other_missing")?;
    assert!(
        db.query("SELECT a FROM ie_child", &[]).is_err(),
        "multi-table DROP cascades the partitioned parent's child"
    );
    Ok(())
}

/// (f) pg_class exposes `relpartbound` as NULL for every Stage-0 row (the
/// corpus reads `SELECT relname, relpartbound FROM pg_class WHERE relname IN
/// (...)`). The query must also succeed (empty) when the tables do not exist.
/// ERRORS on pre-fix code (no `relpartbound` column).
#[test]
fn pg_class_relpartbound_is_null() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    // Empty catalog: the exact corpus SELECT shape returns no rows, not an error.
    let empty = db.query(
        "SELECT relname, relpartbound FROM pg_class WHERE relname IN ('pb_parent', 'pb_child')",
        &[],
    )?;
    assert!(empty.is_empty(), "no matching relations yet → empty result, not an error");

    db.execute("CREATE TABLE pb_parent (id INT) PARTITION BY RANGE (id)")?;
    db.execute("CREATE TABLE pb_child PARTITION OF pb_parent FOR VALUES FROM (0) TO (10)")?;
    let rows = db.query(
        "SELECT relname, relpartbound FROM pg_class WHERE relname IN ('pb_parent', 'pb_child')",
        &[],
    )?;
    assert_eq!(rows.len(), 2, "parent and child both present in pg_class");
    for row in &rows {
        assert!(matches!(row.values[0], Value::String(_)), "relname is text");
        assert!(
            matches!(row.values[1], Value::Null),
            "relpartbound must be NULL at Stage 0, got {:?}",
            row.values[1]
        );
    }
    Ok(())
}
