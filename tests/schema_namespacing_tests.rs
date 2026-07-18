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
