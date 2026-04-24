//! `CREATE AST INDEX`, auto_reparse trigger, and hdb_code.pause /
//! resume bulk-load helpers. Feature-gated behind `code-graph`.
//!
//! AST indexes are tracked in a process-static registry so tests
//! below use distinct table + index names to stay independent under
//! parallel execution.

#![cfg(feature = "code-graph")]

use heliosdb_nano::{EmbeddedDatabase, Result, Value};

fn setup(db: &EmbeddedDatabase, table: &str) -> Result<()> {
    db.execute(&format!(
        "CREATE TABLE IF NOT EXISTS {table} \
         (path TEXT PRIMARY KEY, lang TEXT, content TEXT)"
    ))?;
    Ok(())
}

fn count(db: &EmbeddedDatabase, table: &str) -> Result<i64> {
    let rows = db.query(&format!("SELECT count(*) FROM {table}"), &[])?;
    Ok(match &rows[0].values[0] {
        Value::Int4(n) => *n as i64,
        Value::Int8(n) => *n,
        _ => -1,
    })
}

#[test]
fn create_ast_index_populates_tables() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let tbl = "src_cap";
    setup(&db, tbl)?;
    db.execute_params_returning(
        &format!("INSERT INTO {tbl} (path, lang, content) VALUES ($1, $2, $3)"),
        &[
            Value::String("a.rs".into()),
            Value::String("rust".into()),
            Value::String("pub fn a() {}\n".into()),
        ],
    )?;
    db.execute(&format!(
        "CREATE AST INDEX idx_cap ON {tbl} (content) USING tree_sitter(lang)"
    ))?;
    let syms = count(&db, "_hdb_code_symbols")?;
    assert!(syms >= 1, "symbols were not populated: {syms}");
    Ok(())
}

#[test]
fn create_ast_index_if_not_exists_ignores_duplicate() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let tbl = "src_ine";
    setup(&db, tbl)?;
    db.execute(&format!(
        "CREATE AST INDEX idx_ine ON {tbl} (content) USING tree_sitter(lang)"
    ))?;
    let err = db.execute(&format!(
        "CREATE AST INDEX idx_ine ON {tbl} (content) USING tree_sitter(lang)"
    ));
    assert!(err.is_err());
    db.execute(&format!(
        "CREATE AST INDEX IF NOT EXISTS idx_ine ON {tbl} (content) USING tree_sitter(lang)"
    ))?;
    Ok(())
}

#[test]
fn auto_reparse_fires_on_insert_after_create_ast_index() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let tbl = "src_ar";
    setup(&db, tbl)?;
    db.execute(&format!(
        "CREATE AST INDEX idx_ar ON {tbl} (content) USING tree_sitter(lang) \
         WITH (auto_reparse = true)"
    ))?;
    assert_eq!(count(&db, "_hdb_code_symbols")?, 0);
    db.execute_params_returning(
        &format!("INSERT INTO {tbl} (path, lang, content) VALUES ($1, $2, $3)"),
        &[
            Value::String("a.py".into()),
            Value::String("python".into()),
            Value::String("def inserted():\n    return 1\n".into()),
        ],
    )?;
    let after = count(&db, "_hdb_code_symbols")?;
    assert!(after >= 1, "auto_reparse didn't fire: {after} symbols");
    Ok(())
}

#[test]
fn pause_resume_round_trip() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let tbl = "src_pr";
    setup(&db, tbl)?;
    db.execute(&format!(
        "CREATE AST INDEX idx_pr ON {tbl} (content) USING tree_sitter(lang) \
         WITH (auto_reparse = true)"
    ))?;
    db.execute("SELECT hdb_code.pause('idx_pr')")?;
    db.execute_params_returning(
        &format!("INSERT INTO {tbl} (path, lang, content) VALUES ($1, $2, $3)"),
        &[
            Value::String("paused.rs".into()),
            Value::String("rust".into()),
            Value::String("pub fn paused() {}\n".into()),
        ],
    )?;
    assert_eq!(count(&db, "_hdb_code_symbols")?, 0);
    db.execute("SELECT hdb_code.resume('idx_pr')")?;
    db.execute_params_returning(
        &format!("INSERT INTO {tbl} (path, lang, content) VALUES ($1, $2, $3)"),
        &[
            Value::String("resumed.rs".into()),
            Value::String("rust".into()),
            Value::String("pub fn resumed() {}\n".into()),
        ],
    )?;
    let after = count(&db, "_hdb_code_symbols")?;
    assert!(after >= 2, "resume didn't index everything: {after} symbols");
    Ok(())
}

#[test]
fn pause_unknown_index_errors() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let err = db.execute("SELECT hdb_code.pause('nope')");
    assert!(err.is_err());
    Ok(())
}
