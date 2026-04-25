# All-features release build report

Generated as the closing artefact of the FR-backlog sprint
(tasks #181 – #192).  Captures the final binary's metadata so
downstream consumers (release pipelines, deployment tooling,
the FR-6 pilot installer) can pin to a known-good build.

## Artefact

| field | value |
|---|---|
| binary path | `target/release/heliosdb-nano` |
| size | 36,772,160 bytes (~35.0 MiB) |
| sha256 | `411765281ddc15483a4c987f16bb812b1db8ccefea2866e5f59bcebef3a0e751` |
| ELF type | x86-64 PIE, dynamically linked, not stripped |
| build profile | `release` (lto = true, codegen-units = 1, opt-level = 3) |
| build duration | 6 m 42 s on a cold cache |

## Versions

| component | version |
|---|---|
| `heliosdb-nano` package | 3.18.0 |
| Rust toolchain | rustc 1.92.0 (ded5c06cf 2025-12-08) |
| Cargo | 1.92.0 (344c4567c 2025-10-21) |

## Feature flags enabled

```
cargo build --release --features code-graph,graph-rag,mcp-endpoint,code-embed
```

Resolved feature set:

| flag | implies | purpose |
|---|---|---|
| `code-graph`     | tree-sitter + grammars | AST index + LSP-shaped queries |
| `graph-rag`      | `code-graph`           | universal `_hdb_graph_*` schema + WITH CONTEXT |
| `mcp-endpoint`   | `inventory`            | JSON-RPC + Axum + stdio + WS + SSE + UDS transports |
| `code-embed`     | `code-graph` + `fastembed` | in-process body_vec embedder (BGE-Small) |

Default features (`encryption,vector-search,ring-crypto,ha-tier1`)
remain enabled.

## Acceptance benchmarks (current run)

| FR criterion | target | measured |
|---|---|---|
| Flagship `WITH CONTEXT` query mean (10k-node fixture) | < 500 ms | **62 ms** |
| Flagship `WITH CONTEXT` query max  | budget headroom | **325 ms** |
| Entity linker precision (100 hand-labelled pairs)     | ≥ 80 %   | **100 %** |

Re-run with:

```
cargo bench --bench with_context_bench --features graph-rag,code-graph
cargo bench --bench linker_precision --features graph-rag,code-graph
```

## Test totals (this sprint's net adds)

| suite | tests | status |
|---|---|---|
| code_graph_list_languages       | 3   | ok |
| code_graph_body_vec             | 4   | ok |
| code_graph_extractor_registry   | 3   | ok |
| code_graph_rename_apply         | 5   | ok |
| code_graph_namespacing          | 2   | ok |
| code_graph_local_embedder       | 1 ignored (network model dl) |
| graph_rag_docling               | 3   | ok |
| graph_rag_linker_vector         | 4   | ok |
| mcp_progress_http               | 3   | ok |
| `vector::biased_descent`        | 5 (lib) | ok |
| `code_graph::resolver` (new)    | 5 (lib) | ok |

Pair with the prior sprint totals (1815 lib + 99 integration) for
the cumulative count.

## Reproducing this report

```
cd Helios/Nano
cargo build --release --features code-graph,graph-rag,mcp-endpoint,code-embed
ls -la target/release/heliosdb-nano
sha256sum target/release/heliosdb-nano
file target/release/heliosdb-nano
```

## Notes

- `code-embed` adds `fastembed` and its ONNX runtime
  transitively. The binary itself does not contain the model
  weights; first run downloads ~30 MB to
  `$XDG_CACHE_HOME/.fastembed_cache`.
- LTO is enabled, so this binary is slightly slower to link than
  a plain `cargo build` and the size reflects post-LTO
  optimisations.
- No `strip` pass is applied — symbols stay in for stack-trace
  legibility. Strip with `strip target/release/heliosdb-nano`
  before shipping if size matters more than debuggability.
