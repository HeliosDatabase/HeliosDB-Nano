---
name: heliosdb-nano-vector
description: Vector search in HeliosDB-Nano. Covers HNSW index creation (`CREATE INDEX … USING HNSW`), the three distance operators (`<->` L2, `<#>` negative inner product, `<=>` cosine), bulk vector inserts via the library API (`insert_vectors` / `delete_vectors` / `delete_vector_store`), hybrid BM25 + vector search via the MCP tool, and tuning knobs in `[vector]` config (`hnsw_ef_construction`, `pq_codebook_bits`). Use this when the user wants similarity search, embedding lookups, or RAG retrieval (without the higher-level graph layer).
allowed-tools: Bash(heliosdb-nano *), Bash(psql *), Read
---

# Vector Search

## When to use
- Similarity search over text/image/audio embeddings.
- "Find rows similar to this one".
- Building the retrieval half of a RAG pipeline (without the graph extras of `heliosdb-nano-graph-rag`).

## Prerequisites
- Cargo feature: `vector-search` (**default**, on out-of-the-box).
- Embeddings: bring-your-own — Nano does not ship an embedder by default. Add `--features code-embed` for a local fastembed-rs runtime, or call any HTTP embedding service from your app.

## Verbs

| Verb | Surface | One-liner |
|------|---------|-----------|
| create HNSW index | SQL | `CREATE INDEX vidx ON t USING HNSW (col) WITH (dim = 384, metric = 'cosine')` |
| insert vectors (SQL) | SQL | `INSERT INTO t (id, embedding) VALUES (1, '[0.1, 0.2, …]'::VECTOR)` |
| L2 distance | SQL | `ORDER BY embedding <-> $1 LIMIT k` |
| inner product (negated) | SQL | `ORDER BY embedding <#> $1 LIMIT k` |
| cosine distance | SQL | `ORDER BY embedding <=> $1 LIMIT k` |
| insert vectors (lib) | Rust | `db.insert_vectors("docs", vec)?` |
| delete vectors (lib) | Rust | `db.delete_vectors("docs", &[id1, id2])?` |
| drop store (lib) | Rust | `db.delete_vector_store("docs")?` |
| BM25 index (MCP) | MCP tool | `heliosdb_bm25_index` (`mcp-endpoint` feature) |
| hybrid search (MCP) | MCP tool | `heliosdb_hybrid_search` (`mcp-endpoint` feature) |
| embed + store (MCP) | MCP tool | `heliosdb_embed_and_store` (`mcp-endpoint` feature) |

## Recipes

### Recipe 1: Schema for similarity search
```sql
CREATE TABLE docs (
    id        INTEGER PRIMARY KEY,
    title     TEXT,
    body      TEXT,
    embedding VECTOR(384)
);

CREATE INDEX docs_emb_idx ON docs
USING HNSW (embedding) WITH (
    dim    = 384,
    metric = 'cosine',
    m      = 16,         -- graph degree (defaults are usually fine)
    ef_construction = 200
);
```

### Recipe 2: Insert (SQL — small batches)
```sql
INSERT INTO docs (id, title, body, embedding) VALUES
  (1, 'intro',   'hello world',  '[0.12, 0.04, …]'::VECTOR),
  (2, 'review',  'good doc',     '[0.07, 0.11, …]'::VECTOR);
```

### Recipe 3: Insert (library — bulk)
```rust
use heliosdb_nano::EmbeddedDatabase;

let db = EmbeddedDatabase::new("./mydata")?;
let vectors: Vec<Vec<f32>> = … ;             // your batch
let ids = db.insert_vectors("docs", vectors)?;
println!("inserted {} vectors", ids.len());
```

### Recipe 4: Top-k similarity query (cosine)
```sql
SELECT id, title, body
  FROM docs
 ORDER BY embedding <=> $1     -- $1 = query embedding (the literal vector)
 LIMIT 5;
```
Lower distance = closer. With cosine, range is [0, 2]; with L2 it's unbounded.

### Recipe 5: Filter + similarity (hybrid pre-filter)
```sql
SELECT id, title
  FROM docs
 WHERE author = 'alice' AND created > NOW() - INTERVAL '30 days'
 ORDER BY embedding <=> $1
 LIMIT 10;
```
The planner may pre-filter then re-rank, or use the index then post-filter, depending on selectivity. Check with `EXPLAIN ANALYZE`.

### Recipe 6: Hybrid BM25 + vector (via MCP)
With `--features mcp-endpoint`, an agent can call:
```jsonrpc
{
  "jsonrpc": "2.0", "method": "tools/call", "id": 1,
  "params": {
    "name": "heliosdb_hybrid_search",
    "arguments": {
      "table":     "docs",
      "text_col":  "body",
      "vec_col":   "embedding",
      "query":     "vector index tuning",
      "alpha":     0.5,        // 0 = BM25 only, 1 = vector only
      "top_k":     10
    }
  }
}
```
Returns ranked rows with both scores. See `heliosdb-nano-mcp` for transport setup.

### Recipe 7: Local embedder (`code-embed` feature)
With `cargo install heliosdb-nano --features code-embed,mcp-endpoint`, the in-process fastembed-rs runtime is available — `heliosdb_embed_and_store` takes raw text, embeds it, and inserts.
```jsonrpc
{
  "name": "heliosdb_embed_and_store",
  "arguments": {
    "table":   "docs",
    "id":      42,
    "text":    "this body gets embedded inside the engine",
    "vec_col": "embedding"
  }
}
```
First invocation downloads the ONNX model into `./.fastembed_cache/` (~80–500 MB depending on model). Cache that directory in CI.

### Recipe 8: Delete + drop a store
```rust
db.delete_vectors("docs", &[1, 2, 3])?;     // remove rows by id
db.delete_vector_store("docs")?;            // drop the store entirely
```
Or via SQL:
```sql
DELETE FROM docs WHERE id IN (1, 2, 3);
DROP TABLE docs;                            -- drops associated HNSW index
```

## Tuning knobs

`config.toml`:
```toml
[vector]
hnsw_ef_construction = 200    # build-time recall/cost knob
hnsw_ef_search       = 64     # query-time recall/latency knob (override at query)
pq_codebook_bits     = 8      # product-quantization codebook size
```

## Pitfalls
- **Dimension mismatch fails at insert time, not at index creation.** Keep `dim` exact.
- **Cosine metric requires normalized vectors** for the distance to be meaningful. Many embedding APIs return normalized vectors; verify yours does.
- **`<->` is L2; `<#>` is *negated* inner product** (smaller = "more similar"). Easy to flip in code.
- **HNSW indexes are not the right tool for tiny tables** (≤ a few thousand vectors). For small N, a brute-force scan with cosine is faster end-to-end.
- **HNSW build is single-threaded today**. Bulk-loading millions of vectors takes minutes; do it once, then incremental inserts.
- **`code-embed` (`fastembed`)** is heavy; if your app already produces embeddings externally, don't add the feature flag — call the external embedder and use the SQL `INSERT` form.

## See also
- `heliosdb-nano-schema` — table definitions and index DDL.
- `heliosdb-nano-graph-rag` — higher-level RAG pipeline that uses vectors as one of its inputs.
- `heliosdb-nano-mcp` — the 16-tool catalog including `heliosdb_hybrid_search` and `heliosdb_embed_and_store`.
- `scripts/demo_hnsw_vector_search.sh` — end-to-end demo.
