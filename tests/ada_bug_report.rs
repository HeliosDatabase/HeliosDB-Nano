use heliosdb_nano::{EmbeddedDatabase, Result, Value};

#[test]
fn unique_text_uses_full_string_for_shared_prefixes() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE probe_unique_text (p TEXT NOT NULL UNIQUE)")?;

    db.execute("INSERT INTO probe_unique_text VALUES (REPEAT('x', 100) || 'a')")?;
    db.execute("INSERT INTO probe_unique_text VALUES (REPEAT('x', 100) || 'b')")?;

    let rows = db.query("SELECT COUNT(*) FROM probe_unique_text", &[])?;
    assert_eq!(rows[0].values[0], Value::Int8(2));
    Ok(())
}

#[test]
fn is_distinct_from_has_postgres_null_safe_semantics() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    assert_eq!(
        db.query("SELECT 'a' IS DISTINCT FROM 'b'", &[])?.get(0).unwrap().values[0],
        Value::Boolean(true),
    );
    assert_eq!(
        db.query("SELECT 'a' IS DISTINCT FROM 'a'", &[])?.get(0).unwrap().values[0],
        Value::Boolean(false),
    );
    assert_eq!(
        db.query("SELECT NULL IS DISTINCT FROM 'a'", &[])?
            .get(0)
            .unwrap()
            .values[0],
        Value::Boolean(true),
    );
    assert_eq!(
        db.query("SELECT NULL IS DISTINCT FROM NULL", &[])?
            .get(0)
            .unwrap()
            .values[0],
        Value::Boolean(false),
    );
    assert_eq!(
        db.query("SELECT NULL IS NOT DISTINCT FROM NULL", &[])?
            .get(0)
            .unwrap()
            .values[0],
        Value::Boolean(true),
    );
    Ok(())
}

#[test]
fn on_conflict_update_accepts_table_qualified_existing_refs() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE library_files (path TEXT PRIMARY KEY, sha256 TEXT, hashed_at TEXT)")?;
    db.execute("INSERT INTO library_files VALUES ('/a', 'old', 't0')")?;

    db.execute(
        "INSERT INTO library_files (path, sha256, hashed_at) VALUES ('/a', NULL, 't1') \
         ON CONFLICT (path) DO UPDATE SET \
             sha256 = COALESCE(EXCLUDED.sha256, library_files.sha256), \
             hashed_at = CASE WHEN library_files.sha256 IS DISTINCT FROM EXCLUDED.sha256 \
                              THEN EXCLUDED.hashed_at ELSE library_files.hashed_at END",
    )?;

    let rows = db.query("SELECT sha256, hashed_at FROM library_files WHERE path = '/a'", &[])?;
    assert_eq!(rows[0].values[0], Value::String("old".to_string()));
    assert_eq!(rows[0].values[1], Value::String("t1".to_string()));
    Ok(())
}
