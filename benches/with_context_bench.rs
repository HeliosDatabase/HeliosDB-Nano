//! FR 4 acceptance benchmark — `WITH CONTEXT` query under 500 ms
//! on a 10 k-node fixture.
//!
//! Uses Criterion via the `harness = false` Cargo path so it runs
//! a self-contained main().  Build a synthetic 10 k-node graph
//! (8 k DocChunk + 2 k symbol projections + ~30 k edges across
//! CALLS / IMPORTS / MENTIONS), then time `helios_graphrag_search`
//! over 100 random seeds.

#![cfg(all(feature = "graph-rag", feature = "code-graph"))]

use std::time::Instant;

use heliosdb_nano::graph_rag::{Direction, GraphRagOptions};
use heliosdb_nano::{EmbeddedDatabase, Value};

const N_DOC_CHUNKS: usize = 8_000;
const N_CODE_SYMBOLS: usize = 2_000;
const QUERIES: usize = 100;
const TARGET_MS: u128 = 500;

fn main() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");

    println!("==> setting up 10k-node fixture");
    setup_graph(&db);

    println!("==> warming caches");
    let _ = db
        .graph_rag_search(&GraphRagOptions {
            seed_text: "doc-0".into(),
            hops: 2,
            limit: 30,
            direction: Direction::Both,
            ..GraphRagOptions::default()
        })
        .unwrap();

    println!("==> running {QUERIES} queries");
    let mut total = 0u128;
    let mut max = 0u128;
    for i in 0..QUERIES {
        let seed = format!("doc-{i}");
        let opts = GraphRagOptions {
            seed_text: seed,
            hops: 2,
            limit: 30,
            direction: Direction::Both,
            ..GraphRagOptions::default()
        };
        let t = Instant::now();
        let _ = db.graph_rag_search(&opts).unwrap();
        let elapsed = t.elapsed().as_millis();
        total += elapsed;
        if elapsed > max {
            max = elapsed;
        }
    }
    let mean = total / QUERIES as u128;
    println!("==> mean: {mean} ms, max: {max} ms, target: {TARGET_MS} ms");
    assert!(
        mean <= TARGET_MS,
        "mean {mean}ms exceeded target {TARGET_MS}ms"
    );
}

fn setup_graph(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE _hdb_graph_nodes (node_id BIGSERIAL PRIMARY KEY, node_kind TEXT, source_ref TEXT, title TEXT, text TEXT, extra TEXT)").ok();
    db.execute("CREATE TABLE _hdb_graph_edges (edge_id BIGSERIAL PRIMARY KEY, from_node BIGINT, to_node BIGINT, edge_kind TEXT, weight REAL, extra TEXT)").ok();

    // 8 k DocChunks.
    for i in 0..N_DOC_CHUNKS {
        let title = format!("doc-{i}");
        let text = format!("body of document {i} mentioning various concepts");
        db.execute_params_returning(
            "INSERT INTO _hdb_graph_nodes (node_kind, source_ref, title, text) \
             VALUES ('DocChunk', $1, $2, $3)",
            &[
                Value::String(format!("doc:{i}")),
                Value::String(title),
                Value::String(text),
            ],
        )
        .unwrap();
    }
    // 2 k code symbols.
    for i in 0..N_CODE_SYMBOLS {
        let title = format!("sym-{i}");
        db.execute_params_returning(
            "INSERT INTO _hdb_graph_nodes (node_kind, source_ref, title) \
             VALUES ('Function', $1, $2)",
            &[
                Value::String(format!("code_symbol:{i}")),
                Value::String(title),
            ],
        )
        .unwrap();
    }
    // ~30k edges: each DocChunk MENTIONS up to 3 symbols; symbols
    // CALL up to 2 other symbols.
    let total_nodes = (N_DOC_CHUNKS + N_CODE_SYMBOLS) as i64;
    for i in 0..N_DOC_CHUNKS as i64 {
        for j in 0..3 {
            let target = N_DOC_CHUNKS as i64 + ((i + j) % N_CODE_SYMBOLS as i64) + 1;
            db.execute(&format!(
                "INSERT INTO _hdb_graph_edges (from_node, to_node, edge_kind, weight) \
                 VALUES ({}, {}, 'MENTIONS', 1.0)",
                i + 1,
                target
            ))
            .ok();
        }
    }
    for i in 0..N_CODE_SYMBOLS as i64 {
        for j in 0..2 {
            let from = N_DOC_CHUNKS as i64 + i + 1;
            let to = N_DOC_CHUNKS as i64 + ((i + j + 1) % N_CODE_SYMBOLS as i64) + 1;
            if to <= total_nodes {
                db.execute(&format!(
                    "INSERT INTO _hdb_graph_edges (from_node, to_node, edge_kind, weight) \
                     VALUES ({from}, {to}, 'CALLS', 1.0)"
                ))
                .ok();
            }
        }
    }
}
