//! `WITH CONTEXT (...)` SQL clause end-to-end.

#![cfg(feature = "graph-rag")]

use heliosdb_nano::code_graph::CodeIndexOptions;
use heliosdb_nano::{EmbeddedDatabase, Result, Value};

fn setup_seeded() -> Result<EmbeddedDatabase> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")?;
    db.execute_params_returning(
        "INSERT INTO src (path, lang, content) VALUES ($1, $2, $3)",
        &[
            Value::String("m.py".into()),
            Value::String("python".into()),
            Value::String(
                "def helper():\n    return 1\n\ndef caller():\n    return helper()\n"
                    .into(),
            ),
        ],
    )?;
    db.code_index(CodeIndexOptions::for_table("src"))?;
    db.graph_rag_project_symbols()?;
    Ok(db)
}

#[test]
fn with_context_expands_from_seed_query() -> Result<()> {
    let db = setup_seeded()?;
    // Pick the "helper" node as the seed.
    let rows = db.query(
        "SELECT node_id FROM _hdb_graph_nodes WHERE title = 'helper' \
         WITH CONTEXT (HOPS 1, EDGES CALLS, LIMIT 10)",
        &[],
    )?;
    // At least the seed (helper) must be in the result.
    let got_ids: Vec<i64> = rows
        .iter()
        .filter_map(|r| match r.values.first() {
            Some(Value::Int4(n)) => Some(*n as i64),
            Some(Value::Int8(n)) => Some(*n),
            _ => None,
        })
        .collect();
    assert!(!got_ids.is_empty(), "WITH CONTEXT produced no rows");
    // Shape: 6 cols per row.
    for row in &rows {
        assert_eq!(row.values.len(), 6, "row {row:?}");
    }
    Ok(())
}

#[test]
fn with_context_rejects_missing_hops() -> Result<()> {
    // HOPS is required; otherwise the rewriter falls through to the
    // regular planner and the "WITH CONTEXT" literal survives → parse
    // error from the normal engine path.
    let db = setup_seeded()?;
    let err = db.query(
        "SELECT node_id FROM _hdb_graph_nodes WITH CONTEXT (EDGES CALLS)",
        &[],
    );
    assert!(err.is_err(), "expected error, got {err:?}");
    Ok(())
}

#[test]
fn with_context_non_match_is_pass_through() -> Result<()> {
    let db = setup_seeded()?;
    // No WITH CONTEXT ⇒ falls through to the regular planner and
    // returns whatever the raw SELECT does.
    let rows = db.query("SELECT count(*) FROM _hdb_graph_nodes", &[])?;
    assert_eq!(rows.len(), 1);
    Ok(())
}
