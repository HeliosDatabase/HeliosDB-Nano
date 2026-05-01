---
name: heliosdb-nano-graph-rag
description: Knowledge-graph + RAG pipeline in HeliosDB-Nano. Adds the `_hdb_graph_*` universal schema on top of `code-graph`, plus seed/expand/rerank traversal and domain-specific ingestors for documents (PDF, Office, Markdown), audio (transcribed), images (captioned), email, issues, and Q&A. Pair with HNSW vector search (`heliosdb-nano-vector`) for hybrid retrieval. Use this when the user wants a full RAG stack — indexing heterogeneous content into a typed graph that an agent can traverse, not just a flat vector store.
allowed-tools: Bash(heliosdb-nano *), Read
---

# Graph-RAG Pipeline

## When to use
- Building a RAG knowledge base over heterogeneous content (code + docs + email + tickets).
- Need typed edges between entities (not just a flat vector store).
- Want seed-and-expand graph traversal at retrieval time.

## Prerequisites
- Cargo feature: **`graph-rag`** required (implies `code-graph`).
  ```bash
  cargo install heliosdb-nano --features graph-rag,mcp-endpoint
  ```
- Optional: `code-embed` for in-process embedding (otherwise call an external embedder from your app).

Verify:
```bash
heliosdb-nano repl --memory <<'SQL'
SELECT name FROM sqlite_master WHERE name LIKE '_hdb_graph_%';
SQL
# Expect _hdb_graph_nodes and _hdb_graph_edges (and helpers).
```

## Verbs

| Verb | Surface | One-liner |
|------|---------|-----------|
| seed-expand-rerank search | Rust | `db.graph_rag_search(query)?` |
| add exact (typed) link | Rust | `db.graph_rag_link_exact(src, dst, rel_type)?` |
| add vector link | Rust | `db.graph_rag_link_vector(src, dst, weight)?` |
| project code symbols | Rust | `db.graph_rag_project_symbols()?` |
| ingest docs (md/txt) | Rust | `db.graph_rag_ingest_docs(paths)?` |
| ingest PDF | Rust | `db.graph_rag_ingest_pdf(paths)?` |
| ingest Office (docx/pptx/xlsx) | Rust | `db.graph_rag_ingest_office(paths)?` |
| ingest audio (transcribed) | Rust | `db.graph_rag_ingest_audio(paths)?` |
| ingest image (captioned) | Rust | `db.graph_rag_ingest_image(paths)?` |
| ingest email | Rust | `db.graph_rag_ingest_email(paths_or_imap)?` |
| ingest issues | Rust | `db.graph_rag_ingest_issues(source)?` |
| ingest Q&A | Rust | `db.graph_rag_ingest_qa(pairs)?` |
| MCP graph add edge | MCP tool | `heliosdb_graph_add_edge` |
| MCP graph traverse | MCP tool | `heliosdb_graph_traverse` |
| MCP graph path | MCP tool | `heliosdb_graph_path` |

## Recipes

### Recipe 1: Bootstrap a code-aware graph
```rust
use heliosdb_nano::EmbeddedDatabase;

let db = EmbeddedDatabase::new("./.helios-kb/heliosdb-data")?;

// 1. Ingest source code (uses code-graph under the hood)
db.execute("CREATE TABLE IF NOT EXISTS src (path TEXT PRIMARY KEY, body TEXT, lang TEXT)")?;
// (populate src — see heliosdb-nano-code-graph Recipe 3)
db.code_index()?;

// 2. Project code symbols into the graph layer
let stats = db.graph_rag_project_symbols()?;
println!("nodes: {}, edges: {}", stats.nodes, stats.edges);

// 3. Add doc / issue / email layers
db.graph_rag_ingest_docs(&["./docs/architecture.md", "./README.md"])?;
db.graph_rag_ingest_issues(IssueSource::JsonlFile("./issues.jsonl"))?;
```

### Recipe 2: Seed → expand → rerank query
```rust
let hits = db.graph_rag_search("how does the planner handle ON CONFLICT")?;
for h in hits {
    println!("{:.3}  {}  {}", h.score, h.node_kind, h.snippet);
}
```
Internally:
1. **Seed** — vector search over node embeddings finds top-k starting points.
2. **Expand** — a typed graph walk follows `imports`, `references`, `mentions`, etc.
3. **Rerank** — combines vector similarity with traversal-distance scoring.

### Recipe 3: Add a typed link manually
```rust
db.graph_rag_link_exact(
    /*src=*/ doc_node_id,
    /*dst=*/ symbol_node_id,
    "documents",            // typed relation name
)?;

// Or a soft (vector-similarity) link:
db.graph_rag_link_vector(node_a, node_b, /*weight=*/0.83)?;
```

### Recipe 4: PDF ingest
```rust
db.graph_rag_ingest_pdf(&[
    "./papers/hnsw.pdf",
    "./papers/postgres-mvcc.pdf",
])?;
```
Each PDF becomes a node tree (document → pages → chunks); chunks get embeddings; chunks linked to mentioned entities (e.g., a chunk mentioning function `foo` gets a `mentions` edge to that symbol node).

### Recipe 5: Email + issue ingest (incident triage)
```rust
db.graph_rag_ingest_email(EmailSource::Mbox("./inbox.mbox"))?;
db.graph_rag_ingest_issues(IssueSource::JsonlFile("./jira-export.jsonl"))?;

let hits = db.graph_rag_search(
    "incidents related to the planner regression in march 2026"
)?;
```

### Recipe 6: MCP traversal from an agent
```jsonrpc
{
  "jsonrpc": "2.0", "method": "tools/call", "id": 7,
  "params": {
    "name": "heliosdb_graph_traverse",
    "arguments": {
      "from_node": 12345,
      "rel_types": ["references", "imports"],
      "depth":     3,
      "limit":     50
    }
  }
}
```
Or `heliosdb_graph_path` for shortest-path between two nodes.

## Pitfalls
- **`graph-rag` implies `code-graph`** — you cannot enable just one. The code-symbol projection is the primary node population strategy.
- **Embeddings can be external**. If you don't compile `--features code-embed`, you must populate `embedding` columns yourself (call any embedding API from your app and `INSERT INTO _hdb_graph_nodes (…, embedding) VALUES (…)`).
- **First-pass ingestion is heavy**. PDF/Office/audio extractors use external libraries and can take minutes for large corpora. Run ingestion as a one-off or background job, not on the request path.
- **The graph schema is "universal" but typed** — relation names are user-defined. Establish a consistent vocabulary (`mentions`, `references`, `imports`, `documents`, `quotes`) early; the search reranker uses these names.
- **Rerank weights** are configurable (typically 0.4 vector + 0.6 graph-distance). Tune via the `[vector]` and graph-rag-specific config (see source).

## See also
- `heliosdb-nano-code-graph` — the AST-symbol layer that graph-rag builds on.
- `heliosdb-nano-vector` — HNSW indexes and similarity operators.
- `heliosdb-nano-mcp` — `heliosdb_graph_{add_edge,traverse,path}` and `heliosdb_hybrid_search` tools.
- `scripts/demo_rag_workflow.sh` — end-to-end RAG demo.
