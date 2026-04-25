//! Docling content-conversion ingestion (#186).
//!
//! Spins up a tiny HTTP mock that responds with a recorded
//! DoclingDocument JSON, then drives ingest_pdf against it and
//! verifies the projection into _hdb_graph_nodes / _hdb_graph_edges.

#![cfg(feature = "graph-rag")]

use std::sync::Arc;
use std::time::Duration;

use heliosdb_nano::graph_rag::DoclingIngestOptions;
use heliosdb_nano::{EmbeddedDatabase, Value};

const RECORDED_DOCLING_RESPONSE: &str = r##"{
  "documents": [
    {
      "name": "paper.pdf",
      "json_content": {
        "schema_name": "DoclingDocument",
        "version": "1.0.0",
        "texts": [
          { "self_ref": "#/texts/0", "label": "title", "text": "Paper Title", "level": 1, "prov": [{"page_no": 1}] },
          { "self_ref": "#/texts/1", "label": "section_header", "text": "Abstract", "level": 1, "prov": [{"page_no": 1}] },
          { "self_ref": "#/texts/2", "label": "text", "text": "We present a new method.", "prov": [{"page_no": 1}] },
          { "self_ref": "#/texts/3", "label": "text", "text": "Our results outperform prior art.", "prov": [{"page_no": 1}] },
          { "self_ref": "#/texts/4", "label": "section_header", "text": "Introduction", "level": 1, "prov": [{"page_no": 2}] },
          { "self_ref": "#/texts/5", "label": "text", "text": "Recent work has focused on X.", "prov": [{"page_no": 2}] }
        ],
        "tables": [
          { "self_ref": "#/tables/0", "prov": [{"page_no": 3}] }
        ]
      }
    }
  ]
}"##;

async fn bind_mock_docling() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    use axum::{routing::post, Json, Router};

    async fn handler(_body: Json<serde_json::Value>) -> Json<serde_json::Value> {
        let v: serde_json::Value =
            serde_json::from_str(RECORDED_DOCLING_RESPONSE).unwrap();
        Json(v)
    }

    let app = Router::new().route("/v1/convert/source", post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let h = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, h)
}

#[tokio::test]
async fn ingest_pdf_projects_recorded_document() {
    let (addr, mock_handle) = bind_mock_docling().await;
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let endpoint = format!("http://{addr}/v1/convert/source");

    // Run the (sync) ingest call on a blocking thread so the
    // tokio runtime stays free to serve the mock.
    let db2 = db.clone();
    let endpoint2 = endpoint.clone();
    let stats = tokio::task::spawn_blocking(move || {
        let opts = DoclingIngestOptions::from_url("https://example.com/paper.pdf")
            .with_endpoint(endpoint2);
        db2.graph_rag_ingest_pdf(&opts)
    })
    .await
    .unwrap()
    .expect("ingest ok");

    // 1 root + 2 sections + 3 text chunks + 1 table = 7 nodes;
    // 6 CONTAINS edges (root→2 sections + section→2 chunks +
    // section→1 chunk + root→table).
    assert!(stats.nodes_added >= 7, "stats={stats:?}");
    assert!(stats.edges_added >= 6, "stats={stats:?}");

    // Confirm DocSection rows.
    let sections = db
        .query(
            "SELECT title FROM _hdb_graph_nodes WHERE node_kind = 'DocSection' ORDER BY node_id",
            &[],
        )
        .unwrap();
    let titles: Vec<String> = sections
        .iter()
        .filter_map(|t| match t.values.first() {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert!(titles.iter().any(|t| t == "Abstract"), "have: {titles:?}");
    assert!(titles.iter().any(|t| t == "Introduction"), "have: {titles:?}");

    // Confirm root node has Pdf kind.
    let roots = db
        .query(
            "SELECT title FROM _hdb_graph_nodes WHERE node_kind = 'Pdf'",
            &[],
        )
        .unwrap();
    assert_eq!(roots.len(), 1);

    mock_handle.abort();
}

#[tokio::test]
async fn ingest_pdf_idempotent_on_re_run() {
    let (addr, mock_handle) = bind_mock_docling().await;
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let endpoint = format!("http://{addr}/v1/convert/source");

    let db2 = db.clone();
    let endpoint2 = endpoint.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let opts = DoclingIngestOptions::from_url("https://example.com/paper.pdf")
            .with_endpoint(endpoint2);
        db2.graph_rag_ingest_pdf(&opts)
    })
    .await
    .unwrap()
    .unwrap();

    let count_before = db
        .query("SELECT COUNT(*) FROM _hdb_graph_nodes", &[])
        .unwrap()[0]
        .values[0]
        .clone();

    let db3 = db.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let opts = DoclingIngestOptions::from_url("https://example.com/paper.pdf")
            .with_endpoint(endpoint);
        db3.graph_rag_ingest_pdf(&opts)
    })
    .await
    .unwrap()
    .unwrap();

    let count_after = db
        .query("SELECT COUNT(*) FROM _hdb_graph_nodes", &[])
        .unwrap()[0]
        .values[0]
        .clone();
    assert_eq!(count_before, count_after, "second run added rows");

    mock_handle.abort();
}

#[tokio::test]
async fn ingest_pdf_connection_refused_surfaces_error() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    // Bind a port and immediately drop the listener so the next
    // POST gets refused.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let endpoint = format!("http://{addr}/v1/convert/source");
    let db2 = db.clone();
    let r = tokio::task::spawn_blocking(move || {
        let opts = DoclingIngestOptions::from_url("https://example.com/paper.pdf")
            .with_endpoint(endpoint);
        db2.graph_rag_ingest_pdf(&opts)
    })
    .await
    .unwrap();
    assert!(r.is_err(), "expected connection-refused error");
}
