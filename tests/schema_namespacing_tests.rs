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
    let rows = db.query(sql, &[]).unwrap_or_else(|e| panic!("query `{sql}` failed: {e}"));
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
    assert!(db.query("SELECT id FROM sp.child", &[]).is_err(), "partition child cascaded");
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

/// SHOW search_path reflects the session's current schema.
#[test]
fn show_search_path_reflects_current_schema() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let default_rows = db.query("SHOW search_path", &[])?;
    assert_eq!(default_rows.len(), 1);
    assert_eq!(default_rows[0].values[0], Value::String("\"$user\", public".to_string()));

    db.execute("CREATE SCHEMA showp")?;
    db.execute("SET search_path TO showp")?;
    let set_rows = db.query("SHOW search_path", &[])?;
    assert_eq!(set_rows[0].values[0], Value::String("showp, public".to_string()));
    Ok(())
}
