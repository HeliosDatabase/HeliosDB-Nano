//! Full-text search regression tests (FTS / tsvector / tsquery / @@).
//!
//! Tracks what EasyRAG was forced to work around in
//! `heliosdb_nano_adapter.py` before v3.13.0 landed Postgres-compatible
//! FTS scalar functions.

use heliosdb_nano::{EmbeddedDatabase, Result, Value};

fn seed_articles(db: &EmbeddedDatabase) -> Result<()> {
    db.execute("CREATE TABLE articles (id SERIAL PRIMARY KEY, title TEXT, body TEXT, body_tsv TSVECTOR)")?;
    db.execute(
        "INSERT INTO articles (title, body, body_tsv) VALUES \
            ('A', 'hello heliosdb fulltext', to_tsvector('hello heliosdb fulltext')), \
            ('B', 'lorem ipsum dolor sit amet', to_tsvector('lorem ipsum dolor sit amet')), \
            ('C', 'HeliosDB Nano is fast',     to_tsvector('HeliosDB Nano is fast'))",
    )?;
    Ok(())
}

#[test]
fn to_tsvector_returns_token_list() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let rows = db.query("SELECT to_tsvector('Hello, World!')", &[])?;
    let got = match &rows[0].values[0] {
        Value::Json(s) => s.clone(),
        other => panic!("expected Json, got {other:?}"),
    };
    // Normalised, lower-cased, whitespace-tokenised.
    assert_eq!(got, r#"["hello","world"]"#);
    Ok(())
}

#[test]
fn tsvector_optional_config_arg_is_accepted() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    // `to_tsvector('english', text)` is the two-arg form Postgres
    // apps emit; we accept the config but ignore it.
    let rows = db.query("SELECT to_tsvector('english', 'HeliosDB')", &[])?;
    if let Value::Json(s) = &rows[0].values[0] {
        assert_eq!(s, r#"["heliosdb"]"#);
    } else {
        panic!("expected Json");
    }
    Ok(())
}

#[test]
fn ts_match_operator_filters_rows() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed_articles(&db)?;
    let rows = db.query(
        "SELECT title FROM articles WHERE body_tsv @@ to_tsquery('heliosdb')",
        &[],
    )?;
    // Rows A and C both contain 'heliosdb'; B does not.
    assert_eq!(rows.len(), 2);
    Ok(())
}

#[test]
fn ts_rank_cd_produces_non_zero_for_match() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed_articles(&db)?;
    let rows = db.query(
        "SELECT ts_rank_cd(body_tsv, to_tsquery('heliosdb')) AS r \
         FROM articles \
         WHERE body_tsv @@ to_tsquery('heliosdb')",
        &[],
    )?;
    assert_eq!(rows.len(), 2);
    for r in &rows {
        let score = match r.values[0] {
            Value::Float8(f) => f,
            Value::Float4(f) => f as f64,
            _ => panic!("expected float"),
        };
        assert!(score > 0.0, "expected positive BM25 score, got {score}");
    }
    Ok(())
}

#[test]
fn ts_rank_cd_returns_zero_for_miss() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let rows = db.query(
        "SELECT ts_rank_cd(to_tsvector('cats and dogs'), to_tsquery('fox'))",
        &[],
    )?;
    let score = match rows[0].values[0] {
        Value::Float8(f) => f,
        _ => panic!("expected Float8"),
    };
    assert_eq!(score, 0.0);
    Ok(())
}

#[test]
fn gin_index_ddl_is_accepted() -> Result<()> {
    // Django / SQLAlchemy migrations emit this — we accept the DDL
    // even though no backing index is built yet.
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE t (id SERIAL PRIMARY KEY, body TEXT, body_tsv TSVECTOR)")?;
    db.execute("CREATE INDEX t_body_fts ON t USING gin (body_tsv)")?;
    db.execute("CREATE INDEX t_body_gist ON t USING gist (body_tsv)")?;
    Ok(())
}

#[test]
fn version_string_matches_cargo() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let rows = db.query("SELECT version()", &[])?;
    let v = match &rows[0].values[0] {
        Value::String(s) => s.clone(),
        _ => panic!("expected String"),
    };
    // Must end with the real crate version — no more stale "3.7.0".
    let cargo_ver = env!("CARGO_PKG_VERSION");
    assert!(
        v.contains(cargo_ver),
        "version() = {:?}; expected to contain {:?}",
        v, cargo_ver
    );
    Ok(())
}

#[test]
fn fts_null_propagation() -> Result<()> {
    // NULL tsvector or tsquery should propagate NULL through @@ and ts_rank_cd.
    let db = EmbeddedDatabase::new_in_memory()?;
    let rows = db.query(
        "SELECT ts_rank_cd(NULL, to_tsquery('x'))",
        &[],
    )?;
    assert!(matches!(rows[0].values[0], Value::Null));
    Ok(())
}

#[test]
fn probe_insert_omitted_column_stores_full_tuple() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE hh (id SERIAL PRIMARY KEY, n TEXT)")?;
    db.execute("INSERT INTO hh (n) VALUES ('bb')")?;
    let rows = db.query("SELECT * FROM hh", &[])?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values.len(), 2, "tuple: {:?}", rows[0].values);
    Ok(())
}

#[test]
fn probe_insert_returning_omitted_column_returns_full_tuple() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE hh (id SERIAL PRIMARY KEY, n TEXT)")?;
    let (count, rows) = db.execute_returning("INSERT INTO hh (n) VALUES ('bb') RETURNING *")?;
    assert_eq!(count, 1);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values.len(), 2, "returning tuple: {:?}", rows[0].values);
    Ok(())
}
