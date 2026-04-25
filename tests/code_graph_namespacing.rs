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

#[test]
fn pg_tables_over_sql_reports_schema_split() {
    let db = indexed_db();
    let rows = db
        .query(
            "SELECT schemaname, tablename FROM pg_tables WHERE tablename = 'symbols'",
            &[],
        )
        .unwrap();
    let r = rows.first().expect("symbols row in pg_tables");
    let schema = match r.values.first() {
        Some(Value::String(s)) => s.clone(),
        other => panic!("got {other:?}"),
    };
    let table = match r.values.get(1) {
        Some(Value::String(s)) => s.clone(),
        other => panic!("got {other:?}"),
    };
    assert_eq!(schema, "_hdb_code");
    assert_eq!(table, "symbols");
}

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
