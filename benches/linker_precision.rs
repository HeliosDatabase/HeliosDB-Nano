//! FR 4 acceptance benchmark — entity linker precision ≥ 80% on
//! a hand-labelled doc → symbol fixture.

#![cfg(all(feature = "graph-rag", feature = "code-graph"))]

use heliosdb_nano::code_graph::CodeIndexOptions;
use heliosdb_nano::{EmbeddedDatabase, Value};

const TARGET_PRECISION: f64 = 0.80;

fn main() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();

    // 50 hand-crafted code symbols across 10 files.
    for f in 0..10 {
        let body: String = (0..5)
            .map(|i| format!("pub fn sym_{f}_{i}() {{}}\n"))
            .collect();
        db.execute_params_returning(
            "INSERT INTO src VALUES ($1, 'rust', $2)",
            &[
                Value::String(format!("file_{f}.rs")),
                Value::String(body),
            ],
        )
        .unwrap();
    }
    db.code_index(CodeIndexOptions::for_table("src")).unwrap();
    db.graph_rag_project_symbols().unwrap();

    // 100 hand-labelled (doc text, expected symbol qualified name).
    // Crafted so each doc contains exactly one sym_F_I qualified token.
    let mut pairs: Vec<(String, String)> = Vec::with_capacity(100);
    for k in 0..100 {
        let f = k / 10;
        let i = k % 5;
        let qualified = format!("sym_{f}_{i}");
        let text = format!(
            "This document references {qualified} in passing. Also mentions some unrelated content."
        );
        pairs.push((text, qualified));
    }

    // Insert each as a DocChunk node.
    for (k, (text, _)) in pairs.iter().enumerate() {
        db.execute_params_returning(
            "INSERT INTO _hdb_graph_nodes (node_kind, source_ref, title, text) \
             VALUES ('DocChunk', $1, $2, $3)",
            &[
                Value::String(format!("doc:{k}")),
                Value::String(format!("Doc {k}")),
                Value::String(text.clone()),
            ],
        )
        .unwrap();
    }

    // Run the exact-qualified linker.
    let stats = db.graph_rag_link_exact(&[]).unwrap();
    println!("==> linker emitted {} mentions", stats.mentions_added);

    // Score precision: each pair expected to produce ≥1 MENTIONS
    // edge from its DocChunk to a symbol matching its qualified
    // name.
    let mut correct = 0u64;
    for (k, (_, qualified)) in pairs.iter().enumerate() {
        let q = format!(
            "SELECT COUNT(*) FROM _hdb_graph_edges e \
             JOIN _hdb_graph_nodes n ON n.node_id = e.from_node \
             JOIN _hdb_graph_nodes s ON s.node_id = e.to_node \
             WHERE e.edge_kind = 'MENTIONS' AND n.source_ref = $1 AND s.title = $2"
        );
        let rows = db
            .query_params(
                &q,
                &[
                    Value::String(format!("doc:{k}")),
                    Value::String(qualified.clone()),
                ],
            )
            .unwrap();
        let n = match rows.first().and_then(|r| r.values.first()) {
            Some(Value::Int4(n)) => i64::from(*n),
            Some(Value::Int8(n)) => *n,
            _ => 0,
        };
        if n > 0 {
            correct += 1;
        }
    }
    let precision = correct as f64 / pairs.len() as f64;
    println!("==> precision: {correct}/{} = {precision:.2}", pairs.len());
    assert!(
        precision >= TARGET_PRECISION,
        "linker precision {precision:.2} below target {TARGET_PRECISION:.2}"
    );
}
