//! Vector-similar entity linker (FR 4 §4.3 strategy 3).
//!
//! Indexes a small Rust source so the projection pipeline writes
//! `_hdb_graph_nodes` rows for the symbols, then exercises
//! `graph_rag_link_vector` with hand-crafted embeddings and asserts
//! the produced `MENTIONS` edges + their similarity-as-weight.

#![cfg(feature = "graph-rag")]

use heliosdb_nano::code_graph::CodeIndexOptions;
use heliosdb_nano::graph_rag::{SymbolEmbedding, TextEmbedding};
use heliosdb_nano::{EmbeddedDatabase, Value};

fn setup() -> (EmbeddedDatabase, i64, i64) {
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
    db.graph_rag_project_symbols().unwrap();

    // Capture the two code-symbol IDs (alpha, beta) — these are the
    // node_ids the linker will need.
    let rows = db
        .query(
            "SELECT name, node_id FROM _hdb_code_symbols ORDER BY line_start",
            &[],
        )
        .unwrap();
    let mut alpha = 0i64;
    let mut beta = 0i64;
    for r in rows {
        let name = match r.values.first() {
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        let id = match r.values.get(1) {
            Some(Value::Int4(n)) => i64::from(*n),
            Some(Value::Int8(n)) => *n,
            _ => continue,
        };
        if name == "alpha" {
            alpha = id;
        }
        if name == "beta" {
            beta = id;
        }
    }
    assert!(alpha > 0 && beta > 0, "could not resolve alpha/beta");
    (db, alpha, beta)
}

fn insert_text_node(db: &EmbeddedDatabase, kind: &str, text: &str) -> i64 {
    db.execute_params_returning(
        "INSERT INTO _hdb_graph_nodes (node_kind, source_ref, title, text) \
         VALUES ($1, $2, $3, $4)",
        &[
            Value::String(kind.into()),
            Value::String(format!("doc:{}", uuid::Uuid::new_v4())),
            Value::String("doc title".into()),
            Value::String(text.into()),
        ],
    )
    .unwrap();
    let row = db
        .query("SELECT MAX(node_id) FROM _hdb_graph_nodes", &[])
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    match row.values.first() {
        Some(Value::Int4(n)) => i64::from(*n),
        Some(Value::Int8(n)) => *n,
        _ => panic!("max node_id missing"),
    }
}

#[test]
fn top_k_picks_the_closer_symbol() {
    let (db, alpha, beta) = setup();
    let doc = insert_text_node(&db, "DocChunk", "talks about fn alpha");

    // Hand-crafted embeddings: doc is closer to alpha (both [1,0]).
    let queries = vec![TextEmbedding {
        node_id: doc,
        vector: vec![1.0, 0.0],
    }];
    let targets = vec![
        SymbolEmbedding { node_id: alpha, vector: vec![1.0, 0.0] },
        SymbolEmbedding { node_id: beta,  vector: vec![0.0, 1.0] },
    ];
    let stats = db
        .graph_rag_link_vector(&queries, &targets, 1, 0.5)
        .unwrap();
    assert_eq!(stats.mentions_added, 1, "{stats:?}");

    // The single MENTIONS edge points at alpha's graph node, with
    // weight = 1.0 (perfect match).
    let edges = db
        .query_params(
            "SELECT to_node, weight FROM _hdb_graph_edges \
             WHERE edge_kind = 'MENTIONS' AND from_node = $1",
            &[Value::Int8(doc)],
        )
        .unwrap();
    assert_eq!(edges.len(), 1, "expected 1 edge");
    let weight = match edges[0].values.get(1) {
        Some(Value::Float4(f)) => *f,
        Some(Value::Float8(f)) => *f as f32,
        _ => panic!("missing weight"),
    };
    assert!((weight - 1.0).abs() < 1e-4, "weight = {weight}");
}

#[test]
fn threshold_filters_low_similarity_pairs() {
    let (db, alpha, beta) = setup();
    let doc = insert_text_node(&db, "DocChunk", "completely unrelated text");

    let queries = vec![TextEmbedding {
        node_id: doc,
        vector: vec![1.0, 0.0, 0.0],
    }];
    // Both targets are nearly orthogonal — well below threshold 0.5.
    let targets = vec![
        SymbolEmbedding { node_id: alpha, vector: vec![0.0, 1.0, 0.0] },
        SymbolEmbedding { node_id: beta,  vector: vec![0.0, 0.0, 1.0] },
    ];
    let stats = db
        .graph_rag_link_vector(&queries, &targets, 5, 0.5)
        .unwrap();
    assert_eq!(stats.mentions_added, 0);
    assert!(stats.candidates_seen >= 2);
}

#[test]
fn dimension_mismatch_is_silently_skipped() {
    let (db, alpha, _) = setup();
    let doc = insert_text_node(&db, "DocChunk", "x");
    let queries = vec![TextEmbedding {
        node_id: doc,
        vector: vec![1.0, 0.0, 0.0, 0.0],
    }];
    let targets = vec![SymbolEmbedding {
        node_id: alpha,
        vector: vec![1.0, 0.0],
    }];
    let stats = db
        .graph_rag_link_vector(&queries, &targets, 5, 0.0)
        .unwrap();
    assert_eq!(stats.mentions_added, 0);
}

#[test]
fn empty_inputs_no_op() {
    let (db, _, _) = setup();
    let stats = db.graph_rag_link_vector(&[], &[], 5, 0.0).unwrap();
    assert_eq!(stats.mentions_added, 0);
    assert_eq!(stats.nodes_scanned, 0);
}
