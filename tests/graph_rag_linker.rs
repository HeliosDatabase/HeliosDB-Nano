//! Entity-linker end-to-end: emit `MENTIONS` edges from doc-like
//! nodes to code symbols whose qualified name appears in the text.

#![cfg(feature = "graph-rag")]

use heliosdb_nano::code_graph::CodeIndexOptions;
use heliosdb_nano::{EmbeddedDatabase, Result, Value};

#[test]
fn exact_qualified_name_linker() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")?;
    db.execute_params_returning(
        "INSERT INTO src (path, lang, content) VALUES ($1, $2, $3)",
        &[
            Value::String("lib.rs".into()),
            Value::String("rust".into()),
            Value::String("pub fn UniqueSymbolName() {}\n".into()),
        ],
    )?;
    db.code_index(CodeIndexOptions::for_table("src"))?;
    // Auto-projection is now wired; graph nodes should exist.
    let n = db.query("SELECT count(*) FROM _hdb_graph_nodes", &[])?;
    let graph_count = match &n[0].values[0] {
        Value::Int4(v) => *v as i64,
        Value::Int8(v) => *v,
        _ => 0,
    };
    assert!(graph_count >= 1, "graph projection didn't fire");

    // Insert a DocChunk-shaped node that mentions UniqueSymbolName.
    db.execute(
        "INSERT INTO _hdb_graph_nodes (node_kind, title, text, source_ref) \
         VALUES ('DocChunk', 'Design', 'See UniqueSymbolName for details.', 'doc:1')",
    )?;
    let stats = db.graph_rag_link_exact(&[])?;
    assert!(stats.mentions_added >= 1, "no mentions added: {stats:?}");

    // MENTIONS edge must exist.
    let rows = db.query(
        "SELECT count(*) FROM _hdb_graph_edges WHERE edge_kind = 'MENTIONS'",
        &[],
    )?;
    let mentions = match &rows[0].values[0] {
        Value::Int4(v) => *v as i64,
        Value::Int8(v) => *v,
        _ => 0,
    };
    assert_eq!(mentions, 1, "expected exactly one MENTIONS edge");
    Ok(())
}

#[test]
fn linker_is_idempotent() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")?;
    db.execute_params_returning(
        "INSERT INTO src (path, lang, content) VALUES ($1, $2, $3)",
        &[
            Value::String("a.rs".into()),
            Value::String("rust".into()),
            Value::String("pub fn MentionTarget() {}\n".into()),
        ],
    )?;
    db.code_index(CodeIndexOptions::for_table("src"))?;
    db.execute(
        "INSERT INTO _hdb_graph_nodes (node_kind, title, text, source_ref) \
         VALUES ('DocChunk', 'q', 'MentionTarget is here', 'doc:a')",
    )?;
    let s1 = db.graph_rag_link_exact(&[])?;
    let s2 = db.graph_rag_link_exact(&[])?;
    assert_eq!(s2.mentions_added, 0, "second pass should add 0 edges");
    let _ = s1;
    Ok(())
}
