//! Local embedder smoke test (gated on `code-embed`).
//!
//! Marked `#[ignore]` because the first run downloads ~30 MB of
//! ONNX model weights to the fastembed cache, which we don't want
//! every CI run to hit.  Run manually with:
//!
//! ```sh
//! cargo test --features code-embed --test code_graph_local_embedder \
//!     -- --ignored --nocapture
//! ```

#![cfg(feature = "code-embed")]

use heliosdb_nano::code_graph::{
    code_index_with_embedder, CodeIndexOptions, FastEmbedder,
};
use heliosdb_nano::{EmbeddedDatabase, Value};

#[test]
#[ignore = "downloads ~30 MB of ONNX weights on first run"]
fn fastembed_populates_body_vec() {
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
    let emb = Box::new(FastEmbedder::try_default().expect("fastembed init"));
    code_index_with_embedder(&db, CodeIndexOptions::for_table("src"), emb).unwrap();

    let rows = db
        .query("SELECT body_vec FROM _hdb_code_symbols", &[])
        .expect("body_vec column present");
    assert!(!rows.is_empty());
    let any_vec = rows.iter().any(|t| {
        matches!(t.values.first(), Some(Value::Vector(v)) if v.len() == 384)
    });
    assert!(any_vec, "expected at least one 384-dim BGE-Small embedding");
}
