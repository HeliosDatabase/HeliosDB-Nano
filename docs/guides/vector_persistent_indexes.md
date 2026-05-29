# Vector Stores And Persistent Indexes

The default vector store uses in-process HNSW. It supports API-visible IDs,
metadata filters, namespaces, fetch, delete, and upsert:

```rust
let db = heliosdb_nano::EmbeddedDatabase::new_in_memory()?;
db.create_vector_store_with_options("docs", 384, "cosine", "hnsw")?;
db.insert_vectors_with_options(
    "docs",
    Some(vec!["doc-1".into()]),
    vec![embedding],
    Some(vec![metadata]),
    Some("prod".into()),
)?;
```

Namespaces scope external IDs. The same ID can exist in `prod` and `dev`, and
fetch/delete can target one namespace without affecting the other.

Persistent vector indexes are available behind the `vector-persist` feature:

```bash
cargo build --release --features vector-persist
```

```sql
CREATE TABLE chunks (
  id INTEGER PRIMARY KEY,
  embedding VECTOR(768)
);

CREATE INDEX chunks_embedding_hnsw
ON chunks USING hnsw (embedding)
WITH (
  persistent = true,
  quantization = 'product',
  pq_subquantizers = 16,
  pq_centroids = 256,
  rerank_precision = 'i8'
);
```

Supported `rerank_precision` values are `f32`, `f16`, and `i8`. Product
quantization currently requires a non-empty training set and uses the L2 metric
on the persistent path. Builds without `vector-persist` reject persistent
indexes with a feature-gate error instead of silently creating an in-memory
index.

For schema portability, Nano also accepts `VECTOR_F16(n)`, `VECTOR_I8(n)`,
`VECTOR_I16(n)`, and `HALFVEC(n)` as vector type aliases. Row values are still
validated as `f32` vectors at the SQL boundary; compact physical storage is an
index option.
