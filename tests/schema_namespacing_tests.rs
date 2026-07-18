//! Schema namespacing (coexistence) — real per-schema table identity.
//!
//! Before this feature Nano collapsed every schema qualifier to a bare table
//! name (flat namespace), so same-named tables in different schemas could not
//! coexist and `CREATE SCHEMA` / `SET search_path` / `DROP SCHEMA CASCADE` were
//! effectively no-ops. These exercise the end-to-end coexistence surface a
//! PostgreSQL regression corpus depends on (e.g. `fkpart3.pk3` staying alive
//! while `fkpart5.pk3` is created). Every test FAILS on pre-change code, where
//! `a.t` and `b.t` collapse to the same key.

use heliosdb_nano::{EmbeddedDatabase, Result, Value};

/// Extract a single integer scalar from a one-row, one-column result.
fn one_i64(db: &EmbeddedDatabase, sql: &str) -> i64 {
    let rows = db
        .query(sql, &[])
        .unwrap_or_else(|e| panic!("query `{sql}` failed: {e}"));
    assert_eq!(rows.len(), 1, "expected exactly one row from `{sql}`");
    match &rows[0].values[0] {
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        other => panic!("expected an integer from `{sql}`, got {other:?}"),
    }
}

/// COEXISTENCE: two same-named tables in different schemas are independent, and
/// dropping one leaves the other. Distinct column sets prove distinct identity.
#[test]
fn qualified_tables_coexist_and_drop_independently() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE SCHEMA a")?;
    db.execute("CREATE SCHEMA b")?;
    db.execute("CREATE TABLE a.t (x INT)")?;
    db.execute("CREATE TABLE b.t (y TEXT)")?;

    db.execute("INSERT INTO a.t (x) VALUES (11)")?;
    db.execute("INSERT INTO b.t (y) VALUES ('hello')")?;

    assert_eq!(one_i64(&db, "SELECT x FROM a.t"), 11);
    assert_eq!(db.query("SELECT y FROM b.t", &[])?.len(), 1);

    // Dropping a.t must not touch b.t (pre-change: same key → both gone).
    db.execute("DROP TABLE a.t")?;
    assert!(db.query("SELECT x FROM a.t", &[]).is_err(), "a.t should be gone");
    assert_eq!(db.query("SELECT y FROM b.t", &[])?.len(), 1, "b.t must survive");
    Ok(())
}

/// SEARCH_PATH: a bare CREATE lands in the current schema; a bare reference
/// resolves to it; switching back to public un-resolves the bare name.
#[test]
fn search_path_scopes_bare_names() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE SCHEMA ns1")?;
    db.execute("SET search_path TO ns1")?;
    // Bare create targets ns1.
    db.execute("CREATE TABLE t2 (v INT)")?;
    db.execute("INSERT INTO t2 (v) VALUES (7)")?;
    // Bare and qualified both resolve to ns1.t2.
    assert_eq!(one_i64(&db, "SELECT v FROM t2"), 7);
    assert_eq!(one_i64(&db, "SELECT v FROM ns1.t2"), 7);

    // Back to public: bare t2 no longer resolves (only ns1.t2 exists).
    db.execute("SET search_path TO public")?;
    assert!(db.query("SELECT v FROM t2", &[]).is_err(), "bare t2 gone under public");
    // Qualified still works from any schema.
    assert_eq!(one_i64(&db, "SELECT v FROM ns1.t2"), 7);

    // A public t2 now coexists with ns1.t2 and is what bare resolves to.
    db.execute("CREATE TABLE t2 (w INT)")?;
    db.execute("INSERT INTO t2 (w) VALUES (99)")?;
    assert_eq!(one_i64(&db, "SELECT w FROM t2"), 99);
    assert_eq!(one_i64(&db, "SELECT v FROM ns1.t2"), 7);

    // RESET clears the search path (already public here — stays public).
    db.execute("RESET search_path")?;
    assert_eq!(one_i64(&db, "SELECT w FROM t2"), 99);
    Ok(())
}

/// CORPUS SHAPE: the `fkpart3.pk3` / `fkpart5.pk3` pattern — two schemas, each
/// with a `search_path`-scoped BARE create of the SAME name; both alive.
#[test]
fn corpus_same_bare_name_across_schemas() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE SCHEMA fkpart3")?;
    db.execute("SET search_path TO fkpart3")?;
    db.execute("CREATE TABLE pk3 (id INT)")?;
    db.execute("INSERT INTO pk3 (id) VALUES (1)")?;

    db.execute("CREATE SCHEMA fkpart5")?;
    db.execute("SET search_path TO fkpart5")?;
    db.execute("CREATE TABLE pk3 (id INT)")?;
    db.execute("INSERT INTO pk3 (id) VALUES (2)")?;

    // Both coexist with independent contents.
    assert_eq!(one_i64(&db, "SELECT id FROM fkpart3.pk3"), 1);
    assert_eq!(one_i64(&db, "SELECT id FROM fkpart5.pk3"), 2);
    // The bare name resolves to the current schema (fkpart5).
    assert_eq!(one_i64(&db, "SELECT id FROM pk3"), 2);
    Ok(())
}

/// DROP SCHEMA RESTRICT (default) refuses a non-empty schema; CASCADE drops
/// every member.
#[test]
fn drop_schema_restrict_and_cascade() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE SCHEMA s1")?;
    db.execute("CREATE TABLE s1.a (x INT)")?;
    db.execute("CREATE TABLE s1.b (y INT)")?;

    // RESTRICT (default) errors while members remain.
    assert!(db.execute("DROP SCHEMA s1").is_err(), "RESTRICT must refuse non-empty");
    assert_eq!(db.query("SELECT x FROM s1.a", &[])?.len(), 0, "members untouched");

    // CASCADE drops the members.
    db.execute("DROP SCHEMA s1 CASCADE")?;
    assert!(db.query("SELECT x FROM s1.a", &[]).is_err(), "s1.a cascaded");
    assert!(db.query("SELECT y FROM s1.b", &[]).is_err(), "s1.b cascaded");
    Ok(())
}

/// DROP SCHEMA CASCADE composes with the Stage-0 partition-child cascade: a
/// partitioned parent in the schema takes its children with it.
#[test]
fn drop_schema_cascade_partitioned_parent() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE SCHEMA sp")?;
    db.execute("CREATE TABLE sp.parent (id INT NOT NULL, note TEXT) PARTITION BY RANGE (id)")?;
    db.execute("CREATE TABLE sp.child PARTITION OF sp.parent FOR VALUES FROM (0) TO (10)")?;
    db.execute("INSERT INTO sp.child (id, note) VALUES (3, 'n')")?;
    assert_eq!(db.query("SELECT id FROM sp.child", &[])?.len(), 1);

    db.execute("DROP SCHEMA sp CASCADE")?;
    assert!(db.query("SELECT id FROM sp.parent", &[]).is_err(), "parent dropped");
    assert!(
        db.query("SELECT id FROM sp.child", &[]).is_err(),
        "partition child cascaded"
    );
    Ok(())
}

/// DROP SCHEMA edge cases: IF EXISTS, a multi-schema list, and a hard error on
/// a missing schema without IF EXISTS.
#[test]
fn drop_schema_if_exists_multi_and_missing() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    // IF EXISTS on a missing schema is a clean no-op.
    db.execute("DROP SCHEMA IF EXISTS ghost")?;
    // Missing schema without IF EXISTS errors.
    assert!(db.execute("DROP SCHEMA ghost").is_err(), "missing schema must error");

    // Multi-schema list drops each (both empty here).
    db.execute("CREATE SCHEMA m1")?;
    db.execute("CREATE SCHEMA m2")?;
    db.execute("DROP SCHEMA m1, m2")?;
    // Re-declaring proves they were removed (no duplicate error).
    db.execute("CREATE SCHEMA m1")?;
    db.execute("CREATE SCHEMA m2")?;
    Ok(())
}

/// CREATE SCHEMA duplicate errors without IF NOT EXISTS (PostgreSQL parity).
#[test]
fn create_schema_duplicate_semantics() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE SCHEMA dup")?;
    assert!(db.execute("CREATE SCHEMA dup").is_err(), "duplicate must error");
    db.execute("CREATE SCHEMA IF NOT EXISTS dup")?; // idempotent
    Ok(())
}

/// BACKWARD COMPAT: the plain public-schema flow is unchanged — no schema DDL
/// involved, bare names key exactly as before.
#[test]
fn public_schema_flow_unchanged() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE bc (id INT PRIMARY KEY, v TEXT)")?;
    db.execute("INSERT INTO bc (id, v) VALUES (1, 'a')")?;
    db.execute("INSERT INTO bc (id, v) VALUES (2, 'b')")?;
    assert_eq!(db.query("SELECT id FROM bc", &[])?.len(), 2);
    // A `public.` qualifier still collapses to the bare key.
    assert_eq!(db.query("SELECT id FROM public.bc", &[])?.len(), 2);
    db.execute("DROP TABLE bc")?;
    assert!(db.query("SELECT id FROM bc", &[]).is_err());
    Ok(())
}

/// INTROSPECTION: `information_schema.columns.table_schema` reports the real
/// schema; `pg_class.relname` reports the BARE name (so coexisting `a.t` and a
/// public `t` show as two `relname = 't'` rows).
#[test]
fn introspection_reports_real_schema_and_bare_relname() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE SCHEMA ia")?;
    db.execute("CREATE TABLE ia.t (c INT)")?;
    db.execute("CREATE TABLE t (d INT)")?;

    // Both coexisting tables surface `relname = 't'` in pg_class (two rows).
    assert_eq!(
        db.query("SELECT relname FROM pg_class WHERE relname = 't'", &[])?.len(),
        2,
        "pg_class must list both coexisting `t` relations as bare relname"
    );

    // information_schema.columns carries the real table_schema for each.
    let ia_schema = db.query(
        "SELECT table_schema FROM information_schema.columns WHERE table_name = 't' AND column_name = 'c'",
        &[],
    )?;
    assert_eq!(ia_schema.len(), 1);
    assert_eq!(ia_schema[0].values[0], Value::String("ia".to_string()));

    let pub_schema = db.query(
        "SELECT table_schema FROM information_schema.columns WHERE table_name = 't' AND column_name = 'd'",
        &[],
    )?;
    assert_eq!(pub_schema.len(), 1);
    assert_eq!(pub_schema[0].values[0], Value::String("public".to_string()));
    Ok(())
}

/// RESOLUTION COVERAGE — TRUNCATE. `TRUNCATE TABLE pk` under a non-`public`
/// `search_path` must resolve the bare name to `<schema>.pk` (corpus
/// `foreign_key:1067/1080`). Pre-fix the TRUNCATE arm used a raw `to_string()`
/// that left the bare key the schema owns, so it errored "table 'pk' does not
/// exist".
#[test]
fn truncate_resolves_through_search_path() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE SCHEMA tsp")?;
    db.execute("SET search_path TO tsp")?;
    db.execute("CREATE TABLE pk (a INT)")?;
    db.execute("INSERT INTO pk (a) VALUES (1)")?;
    db.execute("INSERT INTO pk (a) VALUES (2)")?;
    assert_eq!(db.query("SELECT a FROM tsp.pk", &[])?.len(), 2);
    // Bare TRUNCATE resolves to tsp.pk (not a bare `pk` that does not exist).
    db.execute("TRUNCATE TABLE pk")?;
    assert_eq!(
        db.query("SELECT a FROM tsp.pk", &[])?.len(),
        0,
        "tsp.pk emptied via bare TRUNCATE under search_path"
    );
    Ok(())
}

/// RESOLUTION COVERAGE — CREATE VIEW body. A view created under a non-`public`
/// `search_path` whose body references a bare table (corpus
/// `create_function_sql:94`, `create_index:656`) must resolve that table to the
/// schema-scoped key when the view is expanded at read time. Pre-fix the view
/// expansion planner carried no session schema, so the body's bare name stayed
/// bare and read "table 'functest3' does not exist".
#[test]
fn view_body_resolves_through_search_path() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE SCHEMA vsp")?;
    db.execute("SET search_path TO vsp")?;
    db.execute("CREATE TABLE t3 (a INT)")?;
    db.execute("INSERT INTO t3 (a) VALUES (1)")?;
    db.execute("INSERT INTO t3 (a) VALUES (2)")?;
    // The bare view NAME collapses (views stay flat); its body `SELECT * FROM
    // t3` resolves t3 to vsp.t3 at expansion.
    db.execute("CREATE VIEW v3 AS SELECT * FROM t3")?;
    assert_eq!(
        db.query("SELECT a FROM v3", &[])?.len(),
        2,
        "bare view over a schema-scoped table resolves its body"
    );
    // A qualified view reference collapses to the flat view key (create-site and
    // reference-site both collapse — the consistent rule for non-table objects).
    assert_eq!(
        db.query("SELECT a FROM vsp.v3", &[])?.len(),
        2,
        "qualified view reference resolves to the flat view"
    );
    Ok(())
}

/// BIND-AT-CREATE: a view's body must resolve against the CREATING session's
/// schema, not the reader's. `CREATE VIEW vb AS SELECT x FROM t` under
/// `search_path=a` must return `a.t`'s rows to a reader under `search_path=b`,
/// even though `b.t` also exists with DIFFERENT rows. Pre-fix the read-time
/// re-plan used the reader's schema and silently returned `b.t`'s rows (PG binds
/// at CREATE).
#[test]
fn view_binds_to_creator_schema() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE SCHEMA a")?;
    db.execute("CREATE SCHEMA b")?;
    // a.t and b.t both exist, with DIFFERENT cardinality.
    db.execute("CREATE TABLE a.t (x INT)")?;
    db.execute("CREATE TABLE b.t (x INT)")?;
    db.execute("INSERT INTO a.t (x) VALUES (1)")?;
    db.execute("INSERT INTO a.t (x) VALUES (2)")?; // a.t: 2 rows
    db.execute("INSERT INTO b.t (x) VALUES (7)")?; // b.t: 1 row

    // Create the view under search_path=a, over the BARE name t.
    db.execute("SET search_path TO a")?;
    db.execute("CREATE VIEW vb AS SELECT x FROM t")?;

    // Read it under search_path=b (where b.t also exists). It must bind to a.t.
    db.execute("SET search_path TO b")?;
    assert_eq!(
        db.query("SELECT x FROM vb", &[])?.len(),
        2,
        "view binds to the CREATOR's schema (a.t = 2 rows), not the reader's (b.t = 1 row)"
    );

    // A view created under `public` carries NO creator schema (== a
    // pre-namespacing stored view with no schema field): its bare body resolves
    // as public regardless of the reader's search_path.
    db.execute("SET search_path TO public")?;
    db.execute("CREATE TABLE t (x INT)")?; // public.t
    db.execute("INSERT INTO t (x) VALUES (100)")?;
    db.execute("CREATE VIEW vpub AS SELECT x FROM t")?; // creator_schema = None
    db.execute("SET search_path TO b")?; // read under b (b.t has its own row)
    assert_eq!(
        db.query("SELECT x FROM vpub", &[])?.len(),
        1,
        "a public-created view (no creator schema) resolves its body as public"
    );
    assert_eq!(one_i64(&db, "SELECT x FROM vpub"), 100, "public.t's row, not b.t's");
    Ok(())
}

/// SHADOWING: a real schema-local TABLE `s.v` must take precedence over a FLAT
/// view named `v`. Under `search_path=s`, a bare `v` scans the TABLE `s.v` — the
/// flat-view fallback fires only when no such table exists.
#[test]
fn schema_local_table_shadows_flat_view() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE SCHEMA s")?;
    db.execute("CREATE TABLE base (n INT)")?;
    db.execute("INSERT INTO base (n) VALUES (1)")?;
    db.execute("INSERT INTO base (n) VALUES (2)")?;
    db.execute("INSERT INTO base (n) VALUES (3)")?;
    db.execute("CREATE VIEW v AS SELECT n FROM base")?; // flat view v: 3 rows

    // A real TABLE s.v with different cardinality.
    db.execute("CREATE TABLE s.v (n INT)")?;
    db.execute("INSERT INTO s.v (n) VALUES (10)")?; // s.v: 1 row

    // Under search_path=s, bare `v` resolves to the TABLE s.v (1 row), not the
    // flat view (3 rows). Pre-fix the fallback shadowed the schema-local table.
    db.execute("SET search_path TO s")?;
    assert_eq!(
        db.query("SELECT n FROM v", &[])?.len(),
        1,
        "bare v under search_path=s scans the TABLE s.v, not the flat view"
    );
    assert_eq!(one_i64(&db, "SELECT n FROM v"), 10);

    // Under public (no s.v shadow), bare `v` still resolves to the flat view.
    db.execute("SET search_path TO public")?;
    assert_eq!(
        db.query("SELECT n FROM v", &[])?.len(),
        3,
        "bare v under public resolves to the flat view"
    );
    Ok(())
}

/// RECURSION GUARD: a cyclic view reference must ERROR cleanly, not overflow the
/// stack. Build `cyc1 -> cyc2 -> cyc1` via CREATE OR REPLACE and read it.
#[test]
fn cyclic_view_reference_errors_cleanly() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE cyc_seed (x INT)")?;
    db.execute("INSERT INTO cyc_seed (x) VALUES (1)")?;
    db.execute("CREATE VIEW cyc1 AS SELECT x FROM cyc_seed")?;
    db.execute("CREATE VIEW cyc2 AS SELECT x FROM cyc1")?;
    // Introduce the cycle: cyc1 now reads cyc2, which reads cyc1. (At this
    // point cyc1's OLD body is still what schema derivation sees, so the
    // redefine itself succeeds; the cycle only exists on disk afterward.)
    db.execute("CREATE OR REPLACE VIEW cyc1 AS SELECT x FROM cyc2")?;

    let res = db.query("SELECT x FROM cyc1", &[]);
    assert!(res.is_err(), "reading a cyclic view must error, not crash");
    let msg = format!("{}", res.unwrap_err());
    assert!(
        msg.to_lowercase().contains("cyclic"),
        "expected a cyclic-view error, got: {msg}"
    );
    Ok(())
}

/// FK AUTO-NAME DEDUP: because the schema no longer participates in the
/// auto-generated FK name, two FKs on the SAME table+column referencing
/// like-named tables in DIFFERENT schemas both base to `fk_fk_a__pk`. The dedup
/// must suffix the collision so both constraints persist under distinct names.
/// `information_schema.table_constraints` dedups its emitted rows by name, so a
/// name clash would surface as ONE row; distinct names surface as TWO.
#[test]
fn fk_auto_name_dedup_same_table() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE SCHEMA x")?;
    db.execute("CREATE SCHEMA y")?;
    db.execute("CREATE TABLE x.pk (id INT PRIMARY KEY)")?;
    db.execute("CREATE TABLE y.pk (id INT PRIMARY KEY)")?;
    db.execute("CREATE SCHEMA s")?;
    db.execute(
        "CREATE TABLE s.fk (a INT, \
         FOREIGN KEY (a) REFERENCES x.pk (id), \
         FOREIGN KEY (a) REFERENCES y.pk (id))",
    )?;

    let fks = db.query(
        "SELECT constraint_name FROM information_schema.table_constraints \
         WHERE table_name = 's.fk' AND constraint_type = 'FOREIGN KEY'",
        &[],
    )?;
    assert_eq!(
        fks.len(),
        2,
        "both auto-named FKs must persist under DISTINCT names (dedup suffix)"
    );
    Ok(())
}

/// SHOW search_path reflects the session's current schema.
#[test]
fn show_search_path_reflects_current_schema() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let default_rows = db.query("SHOW search_path", &[])?;
    assert_eq!(default_rows.len(), 1);
    assert_eq!(
        default_rows[0].values[0],
        Value::String("\"$user\", public".to_string())
    );

    db.execute("CREATE SCHEMA showp")?;
    db.execute("SET search_path TO showp")?;
    let set_rows = db.query("SHOW search_path", &[])?;
    assert_eq!(set_rows[0].values[0], Value::String("showp, public".to_string()));
    Ok(())
}

// ---------------------------------------------------------------------------
// FIX 1 — partition registry is session-schema-aware at registration.
//
// Under `SET search_path TO s`, a `PARTITION OF` child is created as `s.child`
// and its parent must register under `s.parent` (the parent's real key), not
// the bare name. Before the fix the child registered under bare `parent`, so
// `DROP TABLE parent` (which resolves to `s.parent`) found no children, skipped
// the Stage-0 cascade, orphaned them, and the corpus's re-create then collided.
// ---------------------------------------------------------------------------

/// The exact foreign_key/fkpart6 corpus flow: a two-level partition tree under
/// a search_path schema drops cleanly and the whole tree can be re-created.
#[test]
fn partition_tree_under_search_path_drops_and_recreates() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE SCHEMA fkpart6")?;
    db.execute("SET search_path TO fkpart6")?;

    let create_tree = |db: &EmbeddedDatabase| -> Result<()> {
        db.execute("CREATE TABLE fk (a int) PARTITION BY RANGE (a)")?;
        db.execute("CREATE TABLE fk1 PARTITION OF fk FOR VALUES FROM (1) TO (100) PARTITION BY RANGE (a)")?;
        db.execute("CREATE TABLE fk11 PARTITION OF fk1 FOR VALUES FROM (1) TO (10)")?;
        db.execute("CREATE TABLE fk_d PARTITION OF fk DEFAULT")?;
        Ok(())
    };

    // First build — every statement must succeed (parent-schema column copy is
    // session-aware, so the child clones `fkpart6.fk`, not a stray bare `fk`).
    create_tree(&db)?;

    // Children are addressable while the tree stands.
    db.execute("INSERT INTO fk1 (a) VALUES (5)")?;
    assert_eq!(one_i64(&db, "SELECT a FROM fk1"), 5);

    // Dropping the parent cascades to EVERY registered descendant (sub-
    // partitioned children included), because registration keyed them under
    // `fkpart6.fk` / `fkpart6.fk1`, matching what `DROP TABLE fk` resolves to.
    db.execute("DROP TABLE fk")?;
    for child in ["fk", "fk1", "fk11", "fk_d"] {
        assert!(
            db.query(&format!("SELECT a FROM {child}"), &[]).is_err(),
            "{child} must be gone after DROP TABLE fk (orphan → collision otherwise)"
        );
    }

    // Re-creating the identical tree must NOT collide with a survivor.
    create_tree(&db)?;
    assert_eq!(db.query("SELECT a FROM fk1", &[])?.len(), 0, "fk1 re-created empty");
    Ok(())
}

/// DROP SCHEMA CASCADE tears down a qualified sub-partitioned tree via the
/// registry — including a child that lives in ANOTHER schema.
#[test]
fn drop_schema_cascade_reaches_cross_schema_partition_children() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE SCHEMA sp")?;
    db.execute("CREATE SCHEMA other")?;
    db.execute("SET search_path TO sp")?;
    db.execute("CREATE TABLE p (a int) PARTITION BY RANGE (a)")?;
    // A partition child created in a DIFFERENT schema (qualified child of a
    // qualified parent). It is not enumerated by the `sp.` prefix scan, so it
    // can only be reached through the partition registry cascade.
    db.execute("CREATE TABLE other.c1 PARTITION OF sp.p FOR VALUES FROM (1) TO (10)")?;
    db.execute("SET search_path TO public")?;

    db.execute("DROP SCHEMA sp CASCADE")?;
    assert!(db.query("SELECT a FROM sp.p", &[]).is_err(), "sp.p dropped");
    assert!(
        db.query("SELECT a FROM other.c1", &[]).is_err(),
        "cross-schema partition child must cascade via the registry"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// FIX 2 — foreign-key enforcement is schema-aware.
//
// A `REFERENCES parent` FK under a search_path schema must (a) store the
// resolved parent key and (b) default its referenced columns to the parent's
// PRIMARY KEY, so enforcement probes the right table+columns instead of
// degrading to "does the parent have ANY row" with an empty `parent()` message.
// ---------------------------------------------------------------------------

/// Parent+child in a search_path schema: a valid child row is accepted once the
/// parent row exists; a dangling reference is still rejected. Covers both the
/// explicit `REFERENCES parent(col)` and the column-less `REFERENCES parent`.
#[test]
fn fk_enforcement_resolves_parent_under_search_path() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE SCHEMA fkpart9")?;
    db.execute("SET search_path TO fkpart9")?;
    db.execute("CREATE TABLE pk (a int PRIMARY KEY)")?;

    // (1) Explicit referenced column.
    db.execute("CREATE TABLE fk_explicit (b int REFERENCES pk(a))")?;
    // (2) Column-less REFERENCES — must default to pk's PRIMARY KEY (a).
    db.execute("CREATE TABLE fk_bare (b int REFERENCES pk)")?;

    // With no parent row, both children reject the reference (real violation,
    // not the empty-list false pass).
    assert!(
        db.execute("INSERT INTO fk_explicit (b) VALUES (1)").is_err(),
        "FK must reject a reference to a non-existent parent row"
    );
    assert!(
        db.execute("INSERT INTO fk_bare (b) VALUES (1)").is_err(),
        "column-less FK must also reject a dangling reference"
    );

    // Insert the parent row, then both children accept the matching reference
    // (this is the case that WRONGLY failed before the fix: the probe looked in
    // the wrong key-space / with an empty column list).
    db.execute("INSERT INTO pk (a) VALUES (1)")?;
    db.execute("INSERT INTO fk_explicit (b) VALUES (1)")?;
    db.execute("INSERT INTO fk_bare (b) VALUES (1)")?;
    assert_eq!(one_i64(&db, "SELECT b FROM fk_explicit"), 1);
    assert_eq!(one_i64(&db, "SELECT b FROM fk_bare"), 1);

    // A non-matching reference is still rejected after the parent has rows.
    assert!(
        db.execute("INSERT INTO fk_explicit (b) VALUES (999)").is_err(),
        "FK must reject a key the parent does not hold"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// FIX 3 — ALTER TABLE … SET SCHEMA, and DROP SCHEMA CASCADE marker hygiene.
// ---------------------------------------------------------------------------

/// A table (with data + a CHECK/PK) moves between schemas and is addressable at
/// its new key; the old key is gone. Moving back to `public` yields a bare key.
#[test]
fn alter_table_set_schema_moves_table_between_schemas() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE SCHEMA alter1")?;
    db.execute("CREATE SCHEMA alter2")?;
    db.execute("CREATE TABLE alter1.t1 (f1 int PRIMARY KEY, f2 int)")?;
    db.execute("INSERT INTO alter1.t1 (f1, f2) VALUES (1, 11), (2, 12)")?;

    // Move alter1.t1 -> alter2.t1.
    db.execute("ALTER TABLE alter1.t1 SET SCHEMA alter2")?;
    assert!(db.query("SELECT f1 FROM alter1.t1", &[]).is_err(), "old key gone");
    assert_eq!(db.query("SELECT f1 FROM alter2.t1", &[])?.len(), 2, "rows moved");
    assert_eq!(one_i64(&db, "SELECT f2 FROM alter2.t1 WHERE f1 = 1"), 11);

    // Same-schema move is a no-op success.
    db.execute("ALTER TABLE alter2.t1 SET SCHEMA alter2")?;
    assert_eq!(db.query("SELECT f1 FROM alter2.t1", &[])?.len(), 2);

    // IF EXISTS on a missing table is a silent no-op.
    db.execute("ALTER TABLE IF EXISTS alter2.nope SET SCHEMA alter1")?;

    // Move to public -> bare key.
    db.execute("ALTER TABLE alter2.t1 SET SCHEMA public")?;
    assert_eq!(db.query("SELECT f1 FROM t1", &[])?.len(), 2, "addressable bare in public");
    assert!(db.query("SELECT f1 FROM alter2.t1", &[]).is_err(), "old qualified key gone");
    Ok(())
}

/// SET SCHEMA carries the FK constraint along so enforcement still fires from
/// the moved table's new key.
#[test]
fn alter_table_set_schema_preserves_fk_enforcement() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE SCHEMA s1")?;
    db.execute("CREATE SCHEMA s2")?;
    db.execute("CREATE TABLE s1.parent (a int PRIMARY KEY)")?;
    db.execute("CREATE TABLE s1.child (b int REFERENCES s1.parent(a))")?;
    db.execute("INSERT INTO s1.parent (a) VALUES (1)")?;

    db.execute("ALTER TABLE s1.child SET SCHEMA s2")?;
    // Valid reference still accepted from the new location.
    db.execute("INSERT INTO s2.child (b) VALUES (1)")?;
    assert_eq!(one_i64(&db, "SELECT b FROM s2.child"), 1);
    // Dangling reference still rejected (constraint metadata migrated).
    assert!(
        db.execute("INSERT INTO s2.child (b) VALUES (7)").is_err(),
        "migrated FK must still reject a dangling reference"
    );
    Ok(())
}

/// DROP SCHEMA CASCADE clears the schema marker even when a member drop fails,
/// so the schema name can be re-created. Mirrors the alter_table corpus chain
/// where a stranded marker blocked a later `CREATE SCHEMA`.
#[test]
fn drop_schema_removes_marker_and_allows_recreate() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE SCHEMA alt")?;
    db.execute("CREATE TABLE alt.t (a int)")?;
    db.execute("CREATE SCHEMA dst")?;

    // Move the only member out, then RESTRICT-drop the now-empty schema.
    db.execute("ALTER TABLE alt.t SET SCHEMA dst")?;
    db.execute("DROP SCHEMA alt")?;

    // The marker must be gone: re-creating the schema succeeds (no phantom
    // "already exists").
    db.execute("CREATE SCHEMA alt")?;
    db.execute("CREATE TABLE alt.t2 (b int)")?;
    db.execute("INSERT INTO alt.t2 (b) VALUES (42)")?;
    assert_eq!(one_i64(&db, "SELECT b FROM alt.t2"), 42);

    // And CASCADE on a populated schema also clears the marker + re-creatable.
    db.execute("DROP SCHEMA alt CASCADE")?;
    db.execute("CREATE SCHEMA alt")?;
    assert!(
        db.query("SELECT b FROM alt.t2", &[]).is_err(),
        "member gone after CASCADE"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// FIX A — PUBLIC-path cross-type FK enforcement (round-3 regression).
// ---------------------------------------------------------------------------

/// Regression (FIX A): a PUBLIC-schema cross-type foreign key — an `int8` child
/// referencing an `int4` PRIMARY-KEY parent (a PostgreSQL implicit-cast FK) —
/// must ACCEPT a matching reference and REJECT a dangling one. Round-3 populated
/// the referenced-column list for column-less FKs (correct), which routed the
/// check onto the type-width-sensitive ART fast path; the child's `Int8` probe
/// key never matched the parent's `Int4` index, so a VALID row got a phantom
/// violation. Mirrors the foreign_key corpus 260-268 cluster.
#[test]
fn public_cross_type_fk_enforces_by_value() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE pktable (ptest1 int PRIMARY KEY)")?;
    db.execute("INSERT INTO pktable VALUES (42)")?;
    // int8 child referencing an int4 parent, column-less REFERENCES (binds PK).
    db.execute("CREATE TABLE fktable (ftest1 int8 REFERENCES pktable)")?;

    // The matching reference must be ACCEPTED (this is the regression).
    db.execute("INSERT INTO fktable VALUES (42)")?;
    assert_eq!(one_i64(&db, "SELECT ftest1 FROM fktable"), 42);

    // A dangling reference is still rejected (round-3's correct value check).
    assert!(
        db.execute("INSERT INTO fktable VALUES (43)").is_err(),
        "cross-type FK must reject a key the parent does not hold"
    );

    // A no-op UPDATE keeps the valid value; +1 (=43) becomes dangling.
    db.execute("UPDATE fktable SET ftest1 = ftest1")?;
    assert!(
        db.execute("UPDATE fktable SET ftest1 = ftest1 + 1").is_err(),
        "UPDATE to a dangling key must be rejected"
    );
    Ok(())
}

/// Regression (FIX A): the referenced table is DROPped and re-CREATEd (a fresh
/// PK index) between references, and the child/parent types differ again
/// (int child → NUMERIC parent). Mirrors the foreign_key corpus 269-278 cluster.
#[test]
fn public_fk_survives_parent_drop_recreate_cross_type() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE pktable (ptest1 int PRIMARY KEY)")?;
    db.execute("INSERT INTO pktable VALUES (42)")?;
    db.execute("CREATE TABLE fktable (ftest1 int8 REFERENCES pktable)")?;
    db.execute("INSERT INTO fktable VALUES (42)")?;
    db.execute("DROP TABLE fktable")?;
    db.execute("DROP TABLE pktable")?;

    // Re-create the parent with a NUMERIC PK; a fresh int child references it.
    db.execute("CREATE TABLE pktable (ptest1 numeric PRIMARY KEY)")?;
    db.execute("INSERT INTO pktable VALUES (42)")?;
    db.execute("CREATE TABLE fktable (ftest1 int REFERENCES pktable)")?;
    // int → numeric must still match by value after the DROP+reCREATE.
    db.execute("INSERT INTO fktable VALUES (42)")?;
    assert_eq!(one_i64(&db, "SELECT ftest1 FROM fktable"), 42);
    assert!(
        db.execute("INSERT INTO fktable VALUES (43)").is_err(),
        "int -> numeric FK must reject a dangling reference after DROP+reCREATE"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// FIX C — ALTER TABLE ... SET SCHEMA into the bare key-space (public/pg_catalog).
// ---------------------------------------------------------------------------

/// Regression (FIX C): `pg_catalog` is part of Nano's BARE key-space (a
/// `pg_catalog.t` reference dealiases to bare `t`, exactly like `public.t`), so
/// `ALTER TABLE ... SET SCHEMA pg_catalog` must re-key to the BARE name — never a
/// stranded `pg_catalog.t` key that no later reference could resolve. Mirrors the
/// alter_table corpus 1222-1235 chain (SET SCHEMA pg_catalog / public, then bare
/// RENAME/UPDATE/DELETE/TRUNCATE/DROP).
#[test]
fn set_schema_to_bare_space_keeps_table_addressable() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE new_system_table (id int PRIMARY KEY, othercol text)")?;
    db.execute("INSERT INTO new_system_table (id, othercol) VALUES (1, 'somedata')")?;

    // public -> pg_catalog -> public are all no-op re-keys onto the SAME bare
    // key, so the table stays addressable by its bare name throughout.
    db.execute("ALTER TABLE new_system_table SET SCHEMA pg_catalog")?;
    assert_eq!(
        one_i64(&db, "SELECT id FROM new_system_table"),
        1,
        "addressable bare after SET SCHEMA pg_catalog"
    );
    db.execute("ALTER TABLE new_system_table SET SCHEMA public")?;
    assert_eq!(
        one_i64(&db, "SELECT id FROM new_system_table"),
        1,
        "addressable bare after SET SCHEMA public"
    );

    // The corpus's failing bare ops: RENAME/INSERT/UPDATE/DELETE/TRUNCATE/DROP.
    db.execute("ALTER TABLE new_system_table RENAME TO old_system_table")?;
    db.execute("INSERT INTO old_system_table (id, othercol) VALUES (2, 'otherdata')")?;
    db.execute("UPDATE old_system_table SET id = id + 100 WHERE id = 2")?;
    db.execute("DELETE FROM old_system_table WHERE othercol = 'somedata'")?;
    assert_eq!(one_i64(&db, "SELECT count(*) FROM old_system_table"), 1);
    db.execute("TRUNCATE old_system_table")?;
    assert_eq!(one_i64(&db, "SELECT count(*) FROM old_system_table"), 0);
    db.execute("DROP TABLE old_system_table")?;
    assert!(
        db.query("SELECT id FROM old_system_table", &[]).is_err(),
        "table dropped"
    );
    Ok(())
}

/// FIX C, from-a-real-schema direction: moving `s1.moved` INTO `pg_catalog`
/// lands it on the BARE key (the pg_catalog dealias), not a stranded
/// `pg_catalog.moved`; the old qualified key is gone.
#[test]
fn set_schema_from_schema_into_pg_catalog_is_bare() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE SCHEMA s1")?;
    db.execute("CREATE TABLE s1.moved (k int PRIMARY KEY)")?;
    db.execute("INSERT INTO s1.moved (k) VALUES (7)")?;

    db.execute("ALTER TABLE s1.moved SET SCHEMA pg_catalog")?;
    assert_eq!(one_i64(&db, "SELECT k FROM moved"), 7, "addressable bare after move to pg_catalog");
    assert!(db.query("SELECT k FROM s1.moved", &[]).is_err(), "old qualified key gone");

    // And back out to a real schema keys it qualified again.
    db.execute("CREATE SCHEMA s2")?;
    db.execute("ALTER TABLE moved SET SCHEMA s2")?;
    assert_eq!(one_i64(&db, "SELECT k FROM s2.moved"), 7, "moved -> s2 qualified");
    assert!(db.query("SELECT k FROM moved", &[]).is_err(), "bare key gone after move to s2");
    Ok(())
}

// ---------------------------------------------------------------------------
// FIX B — SET CONSTRAINTS … DEFERRED on a qualified, partitioned FK parent.
// ---------------------------------------------------------------------------

/// Regression (FIX B): a DEFERRABLE foreign key whose parent is a QUALIFIED,
/// partitioned table (`fkpart9.pk`), deferred with `SET CONSTRAINTS ... DEFERRED`
/// inside a transaction. Round-3 already made the enforcement probe and the ART
/// PK-index agree on the qualified key `fkpart9.pk`, but `SET CONSTRAINTS` was a
/// complete no-op, so the check ran IMMEDIATELY against the still-empty parent
/// and the valid "insert child, then parent, then COMMIT" sequence failed.
/// Mirrors the foreign_key corpus 968-984 (fkpart9). The auto-generated FK name
/// is `fk_fk_a__pk`, so the corpus's `SET CONSTRAINTS fk_a_fkey DEFERRED` is
/// honored as ALL.
#[test]
fn set_constraints_defers_fk_on_partitioned_qualified_parent() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE SCHEMA fkpart9")?;
    db.execute("SET search_path TO fkpart9")?;
    db.execute("CREATE TABLE pk (a int PRIMARY KEY) PARTITION BY LIST (a)")?;
    db.execute("CREATE TABLE pk1 PARTITION OF pk FOR VALUES IN (1, 2) PARTITION BY LIST (a)")?;
    db.execute("CREATE TABLE pk11 PARTITION OF pk1 FOR VALUES IN (1)")?;
    db.execute("CREATE TABLE pk3 PARTITION OF pk FOR VALUES IN (3)")?;
    db.execute("CREATE TABLE fk (a int REFERENCES pk DEFERRABLE INITIALLY IMMEDIATE)")?;

    // Immediate (no SET CONSTRAINTS): a reference to the empty parent is rejected.
    assert!(
        db.execute("INSERT INTO fk VALUES (1)").is_err(),
        "immediate FK must reject a reference to the empty parent"
    );

    // Deferred, parent STILL empty at COMMIT: the deferred check fires at COMMIT
    // and rejects; the failed COMMIT rolls back cleanly (so BEGIN below works).
    db.execute("BEGIN")?;
    db.execute("SET CONSTRAINTS fk_a_fkey DEFERRED")?;
    db.execute("INSERT INTO fk VALUES (1)")?; // deferred — accepted for now
    assert!(
        db.execute("COMMIT").is_err(),
        "deferred FK must fail at COMMIT when the parent is still empty"
    );
    // The failed COMMIT aborted the txn: fk is empty and a new txn can start.
    assert_eq!(one_i64(&db, "SELECT count(*) FROM fkpart9.fk"), 0);

    // Deferred, parent gains the row LATER in the same txn (the classic case).
    db.execute("BEGIN")?;
    db.execute("SET CONSTRAINTS fk_a_fkey DEFERRED")?;
    db.execute("INSERT INTO fk VALUES (1)")?;         // child first
    db.execute("INSERT INTO fkpart9.pk VALUES (1)")?; // parent gains a=1 (lands in the parent)
    db.execute("COMMIT")?;                            // deferred check finds fkpart9.pk(a)=1

    assert_eq!(one_i64(&db, "SELECT a FROM fkpart9.fk"), 1);
    assert_eq!(one_i64(&db, "SELECT a FROM fkpart9.pk"), 1);

    // SET CONSTRAINTS is transaction-scoped: after COMMIT the flag is cleared,
    // so a plain autocommit dangling reference is rejected immediately again.
    assert!(
        db.execute("INSERT INTO fk VALUES (2)").is_err(),
        "deferral must not leak past the transaction it was set in"
    );
    Ok(())
}
