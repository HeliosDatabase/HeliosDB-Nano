#![cfg(feature = "code-graph")]

use heliosdb_nano::code_graph::CodeIndexOptions;
use heliosdb_nano::{EmbeddedDatabase, Result, Value};

fn setup() -> Result<EmbeddedDatabase> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")?;
    Ok(db)
}

fn upsert(db: &EmbeddedDatabase, path: &str, body: &str) -> Result<()> {
    db.execute_params_returning(
        "DELETE FROM src WHERE path = $1",
        &[Value::String(path.into())],
    )?;
    db.execute_params_returning(
        "INSERT INTO src (path, lang, content) VALUES ($1, 'rust', $2)",
        &[
            Value::String(path.into()),
            Value::String(body.into()),
        ],
    )?;
    Ok(())
}

#[test]
fn merkle_builds_and_is_stable() -> Result<()> {
    let db = setup()?;
    upsert(&db, "a.rs", "pub fn a() {}\npub fn b() {}\n")?;
    db.code_index(CodeIndexOptions::for_table("src"))?;
    let s1 = db.code_graph_merkle_refresh()?;
    assert_eq!(s1.files_hashed, 1);

    // No changes → second refresh leaves files_unchanged = 1.
    let s2 = db.code_graph_merkle_refresh()?;
    assert_eq!(s2.files_hashed, 0);
    assert_eq!(s2.files_unchanged, 1);
    Ok(())
}

#[test]
fn merkle_detects_signature_change() -> Result<()> {
    let db = setup()?;
    upsert(&db, "m.rs", "pub fn foo() {}\n")?;
    db.code_index(CodeIndexOptions::for_table("src"))?;
    db.code_graph_merkle_refresh()?;

    // Change the signature and re-index.
    upsert(&db, "m.rs", "pub fn foo(x: i32) -> i32 { x }\n")?;
    db.code_index(CodeIndexOptions::for_table("src"))?;
    let s = db.code_graph_merkle_refresh()?;
    assert_eq!(s.files_hashed, 1, "expected file to re-hash");
    Ok(())
}
