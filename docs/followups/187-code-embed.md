# Task 187 — `code-embed` feature: in-process local inference

## Goal

Activate the long-reserved `code-embed` Cargo feature with a
working in-process embedder so the indexer can populate
`body_vec` (#182) without an external service.

## Design choice

[fastembed-rs](https://github.com/Anush008/fastembed-rs) is the
target backend:
* Pure-Rust ONNX Runtime wrapper, no Python interop.
* Ships pre-quantised models (BGE-small, all-MiniLM) under
  permissive licences.
* CPU-only by default; ORT picks up CUDA/CoreML if the EP libs
  are present.
* ~150 MB on-disk first run (model download to cache); zero
  on-disk impact on the binary.

## Acceptance

```rust
use heliosdb_nano::{EmbeddedDatabase, code_graph::{CodeIndexOptions, FastEmbedder}};

let db = EmbeddedDatabase::new_in_memory()?;
db.code_index(
    CodeIndexOptions::for_table("src")
        .embed_bodies(true)
        .embedder(Box::new(FastEmbedder::default()))
)?;
// _hdb_code_symbols.body_vec populated, dim = 384
```

`cargo build` (default features) — clean, no fastembed dep
pulled in. `cargo build --features code-embed` — fastembed-rs
links and the new `FastEmbedder` impl is available.

## Architecture

```
                         Embedder trait
                              ▲
            ┌────────────────┼────────────────┐
            │                │                │
       NoopEmbedder    HttpEmbedder      FastEmbedder
                                          (code-embed)
```

`FastEmbedder` is a thin wrapper that holds an
`fastembed::TextEmbedding` instance, exposes `embed_batch(&[&str])
→ Result<Vec<Vec<f32>>>`, and tracks model dimension for the
indexer's negotiation step.

## Cargo

```toml
[dependencies]
# ...
fastembed = { version = "4", optional = true }

[features]
code-embed = ["dep:fastembed", "code-graph"]
```

## Files to touch

* `Cargo.toml` — feature flag, optional dep.
* `src/code_graph/embed.rs` — `FastEmbedder` impl gated on
  `code-embed`.
* `src/code_graph/mod.rs` — re-export `FastEmbedder` behind the
  flag.
* `tests/code_graph_local_embedder.rs` — gated on `code-embed`,
  runs a tiny corpus through the embedder.

## Tests

(Gated on `feature = "code-embed"` so default CI doesn't pull the
model.)

1. `FastEmbedder::default()` initialises and returns a known
   dimension (384 for BGEBase).
2. Embed a 5-symbol corpus → `_hdb_code_symbols.body_vec` rows
   non-null, dim = 384.
3. `link_vector_similar` over the produced vectors yields at least
   one MENTIONS edge for a paired DocChunk with related text.

## Out of scope

- GPU EP wiring beyond fastembed's defaults.
- Custom model training / fine-tuning.
- Tokeniser swap (fastembed handles that internally).
