//! Integration test for `CREATE SEMANTIC HASH INDEX` DDL — surfaces
//! the existing semantic-Merkle Rust primitive at the SQL layer
//! (FR 4 §4.6).

#![cfg(feature = "code-graph")]

use heliosdb_nano::code_graph::CodeIndexOptions;
use heliosdb_nano::{EmbeddedDatabase, Value};

#[test]
fn create_semantic_hash_index_materialises_rollup() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();
    db.execute_params_returning(
        "INSERT INTO src VALUES ($1, 'rust', $2)",
        &[
            Value::String("a.rs".into()),
            Value::String("pub fn foo() {}\n".into()),
        ],
    )
    .unwrap();
    db.code_index(CodeIndexOptions::for_table("src")).unwrap();

    // First DDL run materialises the rollup: 1 file hashed.
    let n = db
        .execute("CREATE SEMANTIC HASH INDEX code_merkle")
        .expect("DDL accepted");
    assert_eq!(n, 1, "first run should hash 1 file, got {n}");

    // _hdb_code_merkle now has one row.
    let rows = db.query("SELECT file_id FROM _hdb_code_merkle", &[]).unwrap();
    assert_eq!(rows.len(), 1);

    // Re-running with no source change is idempotent — files_hashed
    // = 0 (everything goes through the unchanged path).
    let n2 = db
        .execute("CREATE SEMANTIC HASH INDEX IF NOT EXISTS code_merkle")
        .expect("DDL idempotent");
    assert_eq!(n2, 0, "second run should hash 0 files, got {n2}");
}

#[test]
fn create_semantic_hash_index_optional_on_clause() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();
    db.execute_params_returning(
        "INSERT INTO src VALUES ($1, 'rust', $2)",
        &[
            Value::String("b.rs".into()),
            Value::String("pub fn bar() {}\n".into()),
        ],
    )
    .unwrap();
    db.code_index(CodeIndexOptions::for_table("src")).unwrap();

    // Optional ON-clause is accepted and ignored (only one merkle
    // target today).
    let n = db
        .execute("CREATE SEMANTIC HASH INDEX m ON _hdb_code_symbols")
        .expect("DDL accepted");
    assert_eq!(n, 1);
}
