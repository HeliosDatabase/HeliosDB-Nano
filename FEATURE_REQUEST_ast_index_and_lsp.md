---
requested-by: Claude Code code-graph initiative — danimoya
requested-against: HeliosDB-Nano v3.13.x
priority: high
status: proposed
date-filed: 2026-04-23
track: code-graph
doc: 2/5
depends-on: —
---

# Feature Request: `CREATE AST INDEX`, `hdb_code` extension, and LSP-shaped SQL functions

## TL;DR

Make HeliosDB-Nano AST-aware. Ship a built-in extension (`hdb_code`)
that bundles tree-sitter grammars and language-specific resolvers;
add a new index type (`USING tree_sitter(<lang>)`) that parses a
text column into a graph of symbols and references stored in system
tables; expose the common LSP operations (`definition`, `references`,
`call_hierarchy`, `hover`, `document_symbols`, `rename_preview`) as
stored functions on top of the existing graph engine
(`src/graph/{storage.rs, traverse.rs, sql.rs}`).

This closes the only real gap between HeliosDB-Nano and a full
code-retrieval stack (tree-sitter + vector DB + wrapper). Everything
else — vectors, FTS, hybrid search, graph traversal, triggers — is
already in the engine.

## Motivation

AI coding agents ask four questions constantly:

1. *Where is `X` defined?* → `lsp_definition`
2. *Who uses `X`?* → `lsp_references`
3. *What is the call tree rooted at `X`?* → `lsp_call_hierarchy`
4. *What does `X` look like / mean?* → `lsp_hover`

Today, an agent has to bounce between ripgrep (imprecise), a language
server or Serena (live but heavy), and a vector DB for semantic
expansion. A SQL-backed answer for all three is strictly better for
agents because it composes with joins, filters, branches, and
time-travel — all things HeliosDB already supports for rows.

## Current state in HeliosDB-Nano

The primitives are in place:

- Graph engine with typed nodes/edges: `src/graph/storage.rs`,
  `src/graph/traverse.rs`, SQL bridge in `src/graph/sql.rs`
- Existing MCP graph tools (added in `src/mcp_extensions/`):
  `heliosdb_graph_add_edge`, `heliosdb_graph_traverse`,
  `heliosdb_graph_path` (see `BLOCKER_mcp_legacy.md`)
- BM25 + hybrid + reranker: `src/search/{bm25.rs, hybrid.rs, reranker.rs}`
- Triggers: `src/sql/triggers.rs` and the working example at
  `examples/trigger_new_old_example.rs`
- Stored function / procedural layer: `src/sql/procedural/`
- Extension loading surface: `CREATE EXTENSION` parsing already exists
  (see `src/sql/parser.rs`); today used for things like `plpgsql`
  (README `docs/compatibility/plpgsql.md`)

What does *not* exist yet:

- A first-class **AST index type** (the analogue of `USING hnsw` or
  `USING gin` but for source code).
- A bundled **tree-sitter** loader (or equivalent) with grammars and
  cross-file symbol resolvers for the target languages.
- **LSP-shaped stored functions** over those tables.

## Proposed design

### 2.1 `CREATE EXTENSION hdb_code`

A new built-in extension that ships with the binary. Installing it:

- Loads embedded tree-sitter grammars for Rust, Python, TypeScript,
  JavaScript, Go, SQL, Markdown (MVP set), exposed via
  `hdb_code.supported_languages()` system view.
- Creates the system schema `_hdb_code` with the tables described
  in §2.3.
- Registers the `USING tree_sitter(...)` index method with the
  planner (`src/sql/planner.rs`) and the executor
  (`src/sql/executor/`).
- Registers the `lsp_*` stored functions (§2.4) in
  `src/sql/procedural/` / `src/sql/functions.rs`.

Grammar set is extensible: a later follow-up can add Java, Kotlin,
Swift, C++, C#, Ruby, Bash. The grammar registration API lives at
`hdb_code.register_grammar(lang TEXT, wasm BYTEA)` for ops who want
to ship a custom parser without rebuilding HeliosDB.

### 2.2 `CREATE AST INDEX` DDL

New index method — added to the existing DDL grammar
(`src/sql/parser.rs`) and planner:

```sql
CREATE AST INDEX source_files_ast
    ON source_files (content)
    USING tree_sitter(lang)
    WITH (
        resolve_cross_file = true,
        embed_bodies       = true,
        embed_model        = 'bge-small-en',
        include_comments   = 'doc_only',
        chunk_markdown     = true
    );
```

Semantics:

- `source_files` is any user table with at least `(content TEXT, lang
  TEXT)`. Other columns (`path`, `sha256`, `branch`, `mtime`, …) are
  preserved and carried through.
- `USING tree_sitter(lang)` tells the index method to look at the
  named column for the language to parse, per-row. Alternatively:
  `USING tree_sitter('rust')` pins a single language.
- The executor parses each row's `content`, populates the system
  tables (§2.3), and (if `embed_bodies = true`) populates embeddings
  for symbol bodies and markdown sections using the configured model.
- Cross-file resolution (`resolve_cross_file = true`) runs a second
  pass that links `CALLS` / `IMPORTS` / `REFERENCES` / `EXTENDS` /
  `IMPLEMENTS` edges across the indexed set.

The index participates in the planner's index-selection pass so that
queries against `_hdb_code.symbols` / `_hdb_code.ast_nodes` go through
it rather than scanning raw content.

### 2.3 System schema `_hdb_code`

All tables live in a reserved schema so they don't clash with user
tables. Names and types are illustrative — final shapes TBD by the
implementing engineer.

```sql
CREATE TABLE _hdb_code.files (
    node_id   BIGSERIAL PRIMARY KEY,
    source_table REGCLASS NOT NULL,     -- which user table this came from
    source_pk    JSONB NOT NULL,        -- pk in that user table
    path         TEXT,
    lang         TEXT,
    sha256       TEXT,
    branch       TEXT,                  -- honours HeliosDB branches
    mtime        TIMESTAMPTZ,
    summary      TEXT,                  -- optional AI-generated one-liner
    summary_tsv  TSVECTOR,
    summary_vec  VECTOR                 -- model-dim from WITH (embed_model = ...)
);

CREATE TABLE _hdb_code.ast_nodes (
    node_id      BIGSERIAL PRIMARY KEY,
    file_id      BIGINT REFERENCES _hdb_code.files(node_id) ON DELETE CASCADE,
    parent_id    BIGINT REFERENCES _hdb_code.ast_nodes(node_id),
    kind         TEXT,                  -- tree-sitter node kind ('function_item', 'class_definition', …)
    byte_start   INT,
    byte_end     INT,
    line_start   INT,
    line_end     INT,
    text         TEXT,
    subtree_hash BYTEA                  -- semantic-merkle hash (see FR 4)
);

CREATE TABLE _hdb_code.symbols (
    node_id      BIGSERIAL PRIMARY KEY,
    file_id      BIGINT REFERENCES _hdb_code.files(node_id) ON DELETE CASCADE,
    ast_node_id  BIGINT REFERENCES _hdb_code.ast_nodes(node_id),
    name         TEXT NOT NULL,
    qualified    TEXT,                  -- 'module::Class::method'
    kind         TEXT,                  -- 'function','method','class','struct','type','var','const','module'
    signature    TEXT,
    visibility   TEXT,                  -- 'public','private','crate','module'
    line_start   INT,
    line_end     INT,
    body_tsv     TSVECTOR,
    body_vec     VECTOR
);
CREATE INDEX ON _hdb_code.symbols (name);
CREATE INDEX ON _hdb_code.symbols (qualified);
CREATE INDEX ON _hdb_code.symbols USING gin  (body_tsv);
CREATE INDEX ON _hdb_code.symbols USING hnsw (body_vec vector_cosine_ops);

-- edges piggy-back on the existing graph engine (src/graph/storage.rs)
-- rather than being a plain table; each edge is a typed directed edge
-- with kind ∈ {CALLS, IMPORTS, REFERENCES, EXTENDS, IMPLEMENTS, OVERRIDES,
-- DEFINES, CONTAINS}.
CREATE TABLE _hdb_code.symbol_refs (
    edge_id      BIGSERIAL PRIMARY KEY,
    from_symbol  BIGINT REFERENCES _hdb_code.symbols(node_id),
    to_symbol    BIGINT REFERENCES _hdb_code.symbols(node_id),
    kind         TEXT,
    call_site    BIGINT REFERENCES _hdb_code.ast_nodes(node_id),
    resolution   TEXT                    -- 'exact','heuristic','unresolved'
);
```

System views mirror the shapes callers care about (e.g.
`_hdb_code.functions`, `_hdb_code.classes`) for ergonomics.

### 2.4 LSP-shaped stored functions

Each is a thin SQL / procedural wrapper over the tables + graph
traversal. Signatures:

```sql
-- "Go to definition". Returns zero or more rows; agents can disambiguate.
lsp_definition(
    name        TEXT,
    hint_file   TEXT DEFAULT NULL,
    hint_kind   TEXT DEFAULT NULL
) RETURNS TABLE (symbol_id BIGINT, path TEXT, line INT, signature TEXT, score REAL);

-- "Find references". Forward scan over symbol_refs by to_symbol.
lsp_references(
    symbol_id   BIGINT,
    include_tests BOOL DEFAULT TRUE
) RETURNS TABLE (file_id BIGINT, path TEXT, line INT, kind TEXT, caller_symbol_id BIGINT);

-- "Call hierarchy" (incoming or outgoing).
lsp_call_hierarchy(
    symbol_id   BIGINT,
    direction   TEXT DEFAULT 'incoming',   -- 'incoming' | 'outgoing'
    depth       INT  DEFAULT 3
) RETURNS TABLE (depth INT, symbol_id BIGINT, qualified TEXT, path TEXT, line INT);

-- "Hover". Signature + doc comment + (cached) AI summary.
lsp_hover(symbol_id BIGINT)
    RETURNS TABLE (signature TEXT, doc TEXT, ai_summary TEXT);

-- "Document symbols". Outline of one file.
lsp_document_symbols(file_id BIGINT)
    RETURNS TABLE (symbol_id BIGINT, parent_id BIGINT, name TEXT, kind TEXT, line_start INT, line_end INT);

-- "Rename preview". Dry-run: which spans would change.
lsp_rename_preview(symbol_id BIGINT, new_name TEXT)
    RETURNS TABLE (file_id BIGINT, path TEXT, line INT, byte_start INT, byte_end INT, original TEXT);
```

Implementation notes:

- `lsp_definition` is disambiguated by (a) exact qualified-name match,
  (b) optional file hint, (c) kind hint, (d) a fallback vector score
  using the embedded signature. Multiple candidates is a *feature* —
  the caller decides; we surface `score`.
- `lsp_call_hierarchy` uses a recursive CTE traversal over the graph
  engine (`src/graph/traverse.rs`).
- `lsp_hover.ai_summary` is populated lazily and cached in
  `_hdb_code.symbols.summary_vec` / `.summary`. Whether generation is
  eager or lazy is a `WITH (ai_summary = 'lazy' | 'eager' | 'off')`
  option on the index.
- `lsp_rename_preview` does not mutate — it returns a result set. A
  companion `lsp_rename_apply(...)` that patches through to the source
  table is explicitly **out of scope** for this FR; `preview` is
  enough for agents that then emit their own edits.

### 2.5 CDC: incremental reparse on write

Once the index exists, keeping it fresh is a trigger:

```sql
CREATE TRIGGER source_files_ast_reparse
    AFTER INSERT OR UPDATE OF content ON source_files
    FOR EACH ROW EXECUTE FUNCTION _hdb_code.reparse_row();
```

The index's `WITH (auto_reparse = true)` option registers this
automatically. The `reparse_row` body uses the semantic-merkle hash
(FR 4, §4.4) to skip work when the AST subtree hashes are unchanged.

For bulk load, `hdb_code.pause(index_name)` and
`hdb_code.resume(index_name)` disable/rebuild the index in one pass —
same pattern as disabling GIN/BTREE indexes during `COPY`.

## Worked example

Indexing a repo and asking a question:

```sql
-- 1. Ingest sources (done once; incrementally via the git post-commit hook)
CREATE TABLE src (
    path     TEXT PRIMARY KEY,
    lang     TEXT,
    content  TEXT,
    sha256   TEXT,
    branch   TEXT
);

COPY src FROM PROGRAM 'find ~/Helios/Nano/src -type f -name "*.rs" | xargs helios-slurp';

-- 2. Enable the code extension and create the AST index
CREATE EXTENSION hdb_code;

CREATE AST INDEX src_ast
    ON src (content) USING tree_sitter(lang)
    WITH (resolve_cross_file = true, embed_bodies = true, auto_reparse = true);

-- 3. Use it
--    "Who calls the vector quantisation constructor, up to 3 hops?"
WITH target AS (
    SELECT symbol_id
    FROM   lsp_definition('ProductQuantizerConfig', hint_file := 'config.rs')
    LIMIT 1
)
SELECT depth, qualified, path, line
FROM   lsp_call_hierarchy((SELECT symbol_id FROM target), 'incoming', 3)
ORDER  BY depth, qualified;
```

Behind the scenes: one HNSW-disambiguated lookup, one graph
traversal, zero file reads by the agent.

## Acceptance criteria

- [ ] `CREATE EXTENSION hdb_code` installs cleanly and exposes at
      least Rust, Python, TypeScript, Go, SQL, and Markdown grammars.
- [ ] `CREATE AST INDEX ... USING tree_sitter(<col>)` parses a
      seed corpus (`~/Helios/Nano/src/**.rs`, ~200k LOC) without error
      and populates `_hdb_code.{files, ast_nodes, symbols,
      symbol_refs}`.
- [ ] `lsp_definition('ProductQuantizer')` against that corpus
      returns `src/vector/quantization/mod.rs` with the correct line
      range.
- [ ] `lsp_references(<that symbol_id>)` returns all call sites
      in `quantized_hnsw.rs` and `storage/vector_index.rs`.
- [ ] `lsp_call_hierarchy(..., 'incoming', 3)` terminates in <100ms
      on the same corpus.
- [ ] `UPDATE src SET content = '...' WHERE path = 'foo.rs'` with
      `auto_reparse = true` updates the symbol rows within the same
      transaction.
- [ ] Bulk-load path: `hdb_code.pause(...)`, `COPY`, then
      `hdb_code.resume(...)` finishes faster than row-by-row trigger
      reparse on the seed corpus.
- [ ] Index survives process restart; no per-session rebuild.

## Non-goals

- Real-time incremental parsing at editor keystroke granularity.
- Full semantic (type-checked) resolution. We resolve by qualified
  name + heuristic scoping; `resolution = 'heuristic'` is a normal
  outcome and downstream consumers handle it.
- Refactoring APIs that write back. Preview-only.

## Open questions

1. Grammar shipping format — embed the tree-sitter C libraries
   compiled into the binary (larger artefact, zero install) or load
   precompiled WASM grammars at runtime (smaller binary, one-time
   download)? Recommendation: WASM, served from a known CDN path,
   cached under `$HELIOSDB_HOME/grammars/`.
2. Embedding model defaults — `bge-small-en` (local, free) vs.
   `voyage-3-lite` (metered, higher quality). Recommendation: local
   default, overridable per-index.
3. Whether `lsp_*` should be in the default catalog or require
   `SET search_path TO ..., _hdb_code;`. Recommendation: default.
4. Resolution of overloaded symbols across languages that allow it
   (TS declaration merging, Rust trait methods). Agreed minimum:
   return all candidates with distinct `score`; caller chooses.

## Related

- Depends on no other FR in the track.
- FR 3 (`temporal_branch_lsp`) extends these functions with
  `AS OF COMMIT` / `AS OF TIMESTAMP` once they exist.
- FR 4 (`graphrag_with_context`) promotes `_hdb_code` nodes/edges
  to first-class members of a wider cross-modal graph.
- FR 5 (`native_mcp_endpoint`) exposes every `lsp_*` function as an
  MCP tool without manual re-implementation.
