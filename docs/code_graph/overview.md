# Code-graph track (phase 1, v3.15.0)

HeliosDB-Nano can act as an embedded code-graph for AI coding agents:
an AST index, LSP-shaped queries, and every row pushed through the
same storage layer as any other Nano table.

**Status.** Phase 1 ships an embedded Rust API plus auto-created
`_hdb_code_*` tables. Rust and Python grammars are bundled. Wire-level
DDL (`CREATE EXTENSION hdb_code`, `CREATE AST INDEX`) and temporal
queries (`AS OF COMMIT`) land in phase 2. See
`FEATURE_REQUEST_code_graph_overview.md` for the full track.

## Enabling the feature

The track is opt-in via a Cargo feature flag. Default builds do not
pull any tree-sitter dependency.

```bash
cargo build --release --features code-graph
cargo test  --lib     --features code-graph
```

Without the flag, `src/code_graph/` is not compiled and the
`EmbeddedDatabase::code_index` / `lsp_*` methods are absent.

## Minimum-viable flow

```rust
use heliosdb_nano::{
    code_graph::{CodeIndexOptions, DefinitionHint},
    EmbeddedDatabase,
};

# fn example() -> heliosdb_nano::Result<()> {
let db = EmbeddedDatabase::new_in_memory()?;

// 1. A user table that carries (path, lang, content).
db.execute(
    r#"CREATE TABLE src (
         path TEXT PRIMARY KEY,
         lang TEXT,
         content TEXT
       )"#,
)?;
db.execute(
    "INSERT INTO src (path, lang, content) VALUES \
       ('lib.rs', 'rust', 'pub fn answer() -> i32 { 42 }')",
)?;

// 2. Build / refresh the code index.
let stats = db.code_index(CodeIndexOptions::for_table("src"))?;
println!("parsed {} files, wrote {} symbols", stats.files_parsed, stats.symbols_written);

// 3. Query it.
let defs = db.lsp_definition("answer", &DefinitionHint::default())?;
for d in defs {
    println!("{} at {}:{} -- {}", d.qualified, d.path, d.line, d.signature);
}
# Ok(())
# }
```

## Tables created

`code_index` creates these on first call (IF NOT EXISTS):

| Table | Purpose |
|---|---|
| `_hdb_code_files` | One row per source file ingested. |
| `_hdb_code_symbols` | One row per named definition (function, struct, class, method, trait, ...). |
| `_hdb_code_symbol_refs` | Directed edges between symbols (CALLS / REFERENCES / CONTAINS / DEFINES / IMPORTS). |

All are plain user tables — queryable, joinable, and branch-aware like
any other Nano table.

## Public API

```rust
impl EmbeddedDatabase {
    pub fn code_index(&self, opts: CodeIndexOptions) -> Result<CodeIndexStats>;
    pub fn lsp_definition(&self, name: &str, hint: &DefinitionHint) -> Result<Vec<DefinitionRow>>;
    pub fn lsp_references(&self, symbol_id: i64) -> Result<Vec<ReferenceRow>>;
    pub fn lsp_call_hierarchy(&self, symbol_id: i64, direction: CallDirection, depth: u32)
        -> Result<Vec<CallHierarchyRow>>;
    pub fn lsp_hover(&self, symbol_id: i64) -> Result<Option<HoverRow>>;
}
```

## Supported languages (phase 1)

- Rust (`tree-sitter-rust`)
- Python (`tree-sitter-python`)

TypeScript, Go, SQL, and Markdown extractors arrive in phase 2.

## Embeddings

Phase 1 does **not** populate a vector column by default. `body_vec`
is absent from the schema; it returns in phase 2 behind a size-
parameterised `VECTOR(n)` column whose `n` is negotiated with the
user's embedding endpoint at index time.

If you want semantic retrieval today:

1. Keep phase-1 BM25-ready data as-is (works fine for name / qualified
   / signature text).
2. Wait for phase 2 if you need `body_vec` populated via an HTTP
   endpoint. The wire shape `{"input": "..."} → {"embedding": [...]}`
   is already defined in `src/code_graph/embed.rs` so the endpoint can
   be tested standalone.

Nano ships no in-process inference runtime — by design. Tree-sitter
is bundled; model inference is always external.

## Storage-level filtering

Every `lsp_*` query pushes its WHERE predicate through the regular
`FilteredScan` path, so the bloom-filter / zone-map / SIMD pipeline
in `src/storage/predicate_pushdown.rs` kicks in for free. A typical
`lsp_definition('foo')` is an Eq-pushdown on a single column — cheap
even on a multi-million-symbol corpus.

## What's out of scope (phase 1)

- `CREATE EXTENSION hdb_code` / `CREATE AST INDEX ... USING tree_sitter(...)` DDL
- Temporal / branch variants of `lsp_*` (`AS OF COMMIT`, `ON BRANCH`)
- Cross-file resolution beyond qualified-name match
- Incremental reparse trigger
- Semantic-Merkle subtree hashing
- `WITH CONTEXT` clause (phase 3)
- Native MCP endpoint (phase 4)

Each is tracked in the corresponding `FEATURE_REQUEST_*.md` file at
the repo root.
