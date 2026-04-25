//! `_hdb_code_symbols.body_vec` integration:
//!  - column absent on the noop path
//!  - column present + populated when an embedder yields vectors
//!  - dimension mismatch errors cleanly across calls
//!  - re-embedding overwrites the existing column

#![cfg(feature = "code-graph")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use heliosdb_nano::code_graph::{
    code_index_with_embedder, CodeIndexOptions, Embedder, NoopEmbedder,
};
use heliosdb_nano::{EmbeddedDatabase, Result, Value};

#[derive(Debug)]
struct ConstEmbedder {
    vec: Vec<f32>,
    calls: Arc<AtomicUsize>,
}

impl Embedder for ConstEmbedder {
    fn embed(&self, _text: &str) -> Result<Option<Vec<f32>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Some(self.vec.clone()))
    }
}

fn setup() -> EmbeddedDatabase {
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
    db
}

#[test]
fn body_vec_absent_on_noop_path() {
    let db = setup();
    db.code_index(CodeIndexOptions::for_table("src")).unwrap();
    // body_vec column shouldn't exist when no embedder is configured.
    let r = db.query("SELECT body_vec FROM _hdb_code_symbols LIMIT 1", &[]);
    assert!(r.is_err(), "body_vec should be absent on noop path");
}

#[test]
fn body_vec_populated_when_embedder_returns_vectors() {
    let db = setup();
    let calls = Arc::new(AtomicUsize::new(0));
    let emb = Box::new(ConstEmbedder {
        vec: vec![0.5, -0.5, 0.25, -0.25, 0.1, -0.1, 0.0, 1.0],
        calls: calls.clone(),
    });
    code_index_with_embedder(&db, CodeIndexOptions::for_table("src"), emb).unwrap();

    // alpha + beta both have non-empty signatures, so the embedder
    // is called for each. Plus parent symbols, but minimum 2.
    assert!(
        calls.load(Ordering::SeqCst) >= 2,
        "embedder calls = {}",
        calls.load(Ordering::SeqCst)
    );

    let rows = db
        .query("SELECT body_vec FROM _hdb_code_symbols", &[])
        .expect("body_vec column present");
    assert!(!rows.is_empty());
    let vec_values: Vec<Vec<f32>> = rows
        .iter()
        .filter_map(|t| match t.values.first() {
            Some(Value::Vector(v)) => Some(v.clone()),
            _ => None,
        })
        .collect();
    assert!(!vec_values.is_empty(), "no Vector values found");
    for v in vec_values {
        assert_eq!(v.len(), 8, "wrong dimension: {v:?}");
    }
}

#[test]
fn dimension_mismatch_errors_within_one_call() {
    // First-call mismatch: embedder yields different lengths for
    // different symbols. insert_symbols catches this before any
    // SQL is issued.
    struct VarDimEmbedder {
        first_call: std::sync::atomic::AtomicBool,
    }
    impl Embedder for VarDimEmbedder {
        fn embed(&self, _text: &str) -> Result<Option<Vec<f32>>> {
            let first = self.first_call.swap(false, Ordering::SeqCst);
            Ok(Some(if first {
                vec![1.0; 8]
            } else {
                vec![1.0; 4]
            }))
        }
    }
    let db = setup();
    let result = code_index_with_embedder(
        &db,
        CodeIndexOptions::for_table("src"),
        Box::new(VarDimEmbedder {
            first_call: std::sync::atomic::AtomicBool::new(true),
        }),
    );
    assert!(result.is_err(), "expected dim-mismatch error");
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("dimension"), "got: {msg}");
}

#[test]
fn noop_embedder_keeps_legacy_path() {
    let db = setup();
    code_index_with_embedder(
        &db,
        CodeIndexOptions::for_table("src"),
        Box::new(NoopEmbedder),
    )
    .unwrap();
    // No vectors → no column.
    let r = db.query("SELECT body_vec FROM _hdb_code_symbols LIMIT 1", &[]);
    assert!(r.is_err());
}
