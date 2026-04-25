//! Schema-namespacing alias (#188): `_hdb_code.<table>` and
//! `_hdb_graph.<table>` are accepted everywhere the flat-prefix
//! names are.  Existing code paths are unaffected.

#![cfg(feature = "code-graph")]

use heliosdb_nano::code_graph::CodeIndexOptions;
use heliosdb_nano::{EmbeddedDatabase, Value};

fn indexed_db() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();
    db.execute_params_returning(
        "INSERT INTO src VALUES ($1, 'rust', $2)",
        &[
            Value::String("a.rs".into()),
            Value::String("pub fn alpha() {}\npub fn beta() {}\n".into()),
        ],
    )
    .unwrap();
    db.code_index(CodeIndexOptions::for_table("src")).unwrap();
    db
}

#[test]
fn dotted_name_resolves_to_flat_prefix() {
    let db = indexed_db();
    // Both forms hit the same data.
    let flat = db.query("SELECT name FROM _hdb_code_symbols", &[]).unwrap();
    let dotted = db.query("SELECT name FROM _hdb_code.symbols", &[]).unwrap();
    assert_eq!(flat.len(), dotted.len());
    assert_eq!(flat.len(), 2);
}

// pg_tables.schemaname split is exercised via the older
// SystemViewRegistry's existing test surface in
// tests/system_views_tests.rs — the SQL planner here only
// dispatches phase-3 views.  We test the dotted-name aliasing
// path instead, which is what end-users actually see.

#[cfg(feature = "graph-rag")]
#[test]
fn graph_rag_dotted_form_works() {
    let db = indexed_db();
    db.graph_rag_project_symbols().unwrap();
    let flat = db.query("SELECT node_id FROM _hdb_graph_nodes", &[]).unwrap();
    let dotted = db.query("SELECT node_id FROM _hdb_graph.nodes", &[]).unwrap();
    assert_eq!(flat.len(), dotted.len());
    assert!(flat.len() >= 2);
}
