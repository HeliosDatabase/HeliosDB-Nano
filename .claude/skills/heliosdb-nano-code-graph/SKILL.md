---
name: heliosdb-nano-code-graph
description: Index a repository's source code as an AST symbol graph in HeliosDB-Nano. Covers grammar registration (Rust / Python / TypeScript / Go / Markdown / SQL), full-project indexing (`code_index`), LSP-shaped queries (`lsp_definition`, `lsp_references`, `lsp_call_hierarchy`, `lsp_hover`), the git-hook helper (`heliosdb-nano code-graph hook`), and the `_hdb_code_symbols` / `_hdb_code_symbol_refs` tables. Use this when the user wants AI-grade "where is this defined / used / called" queries across a codebase, or wants to wire a code-graph into a Claude Code / MCP workflow.
allowed-tools: Bash(heliosdb-nano *), Bash(git *), Read
---

# Code-Graph Indexing

## When to use
- Build an index over a project's source so an AI agent can answer "definition", "references", "callers", "hover" with grounded results.
- Run an MCP server (`heliosdb-nano-mcp`) that exposes code-graph queries.
- Pair the index with embeddings (`code-embed` feature) for semantic + structural search.

## Prerequisites
- Cargo feature: **`code-graph`** required.
  ```bash
  cargo install heliosdb-nano --features code-graph
  ```
- For embeddings: add `--features code-embed`.
- For MCP serving: add `--features mcp-endpoint`.

Verify:
```bash
heliosdb-nano code-graph --help \
  && echo "code-graph: ENABLED" \
  || echo "rebuild with --features code-graph"
```

## Verbs

| Verb | Surface | One-liner |
|------|---------|-----------|
| git-hook ingest | CLI | `git diff-tree --no-commit-id --name-only -r HEAD \| heliosdb-nano code-graph hook --data-dir .helios-index/heliosdb-data --source-table src` |
| register grammar (lib) | Rust | `db.register_grammar("rust", tree_sitter_rust::language())?` |
| list registered (lib) | Rust | `db.registered_grammars()` |
| project index (lib) | Rust | `let stats = db.code_index()?;` |
| LSP: definition | Rust | `db.lsp_definition("rust", "src/foo.rs", line, col)?` |
| LSP: references | Rust | `db.lsp_references("rust", "src/foo.rs", line, col)?` |
| LSP: call hierarchy | Rust | `db.lsp_call_hierarchy(...)?` |
| LSP: hover | Rust | `db.lsp_hover(...)?` |
| refresh content hash | Rust | `db.code_graph_merkle_refresh()?` |
| query symbol table | SQL | `SELECT * FROM _hdb_code_symbols WHERE name = 'foo'` |
| query refs | SQL | `SELECT * FROM _hdb_code_symbol_refs WHERE from_symbol = 42` |

## Recipes

### Recipe 1: One-shot full-repo index (Rust embedded)
```rust
use heliosdb_nano::EmbeddedDatabase;

let db = EmbeddedDatabase::new("./.helios-index/heliosdb-data")?;

// Built-in grammar registration is automatic for the languages compiled
// in via the `code-graph` feature; you can also register custom grammars:
// db.register_grammar("kotlin", tree_sitter_kotlin::language())?;

// Tell the indexer where to find your sources (you maintain a `src` table):
db.execute("CREATE TABLE IF NOT EXISTS src (path TEXT PRIMARY KEY, body TEXT, lang TEXT)")?;
// (populate src however you want; for git-tracked repos see Recipe 3)

let stats = db.code_index()?;
println!("symbols: {}, refs: {}, files: {}/{} ({} unchanged, {} skipped)",
    stats.symbols_written, stats.refs_written,
    stats.files_parsed, stats.files_seen,
    stats.files_unchanged, stats.files_skipped);
```

### Recipe 2: LSP-style queries
```rust
// "definition of the symbol at line 42, column 18 in src/foo.rs"
let defs = db.lsp_definition("rust", "src/foo.rs", 42, 18)?;
for d in defs {
    println!("{}:{}:{}", d.path, d.line, d.col);
}

// "every reference to that symbol"
let refs = db.lsp_references("rust", "src/foo.rs", 42, 18)?;

// "who calls fn `bar`"
let calls = db.lsp_call_hierarchy("rust", "src/foo.rs", 42, 18, /*incoming=*/true)?;

// "hover info" (signature, doc comment)
let hov = db.lsp_hover("rust", "src/foo.rs", 42, 18)?;
```

### Recipe 3: Git hook — incremental on every commit
```bash
mkdir -p .helios-index
heliosdb-nano init .helios-index/heliosdb-data

# .git/hooks/post-commit (or pre-push)
#!/usr/bin/env bash
git diff-tree --no-commit-id --name-only -r HEAD \
  | heliosdb-nano code-graph hook \
      --data-dir .helios-index/heliosdb-data \
      --repo-root . \
      --source-table src
chmod +x .git/hooks/post-commit
```
Each commit prints `CodeIndexStats { files_seen, files_parsed, files_unchanged, files_skipped, symbols_written, refs_written }`.

### Recipe 4: Force-reparse (rebuild from scratch)
```bash
# 1. drop the index data dir
rm -rf .helios-index/heliosdb-data
heliosdb-nano init .helios-index/heliosdb-data

# 2. feed every tracked file (not just diff-tree)
git ls-files \
  | heliosdb-nano code-graph hook \
      --data-dir .helios-index/heliosdb-data \
      --source-table src
```
Use this when adding a new grammar (existing files need re-parsing) or after upgrading Nano (schema bumps).

### Recipe 5: Inspect via SQL
```sql
-- top-10 most-referenced symbols in the project
SELECT s.name, COUNT(r.id) AS refs
  FROM _hdb_code_symbols s
  JOIN _hdb_code_symbol_refs r ON r.to_symbol = s.id
 GROUP BY s.name
 ORDER BY refs DESC LIMIT 10;

-- find every symbol called `parse` in Rust
SELECT * FROM _hdb_code_symbols
 WHERE name = 'parse' AND lang = 'rust';
```

### Recipe 6: Branch-scoped queries (FR-3 `ON BRANCH '<n>'`)
LSP queries can be pinned to a branch — useful for asking "what does this look like on the `feature-x` branch":
```rust
let defs = db.lsp_definition_on_branch("rust", "src/foo.rs", 42, 18, "feature-x")?;
```
The branch guard restores the previous active branch on Drop, so any later queries continue against the original branch.

## Pitfalls
- **`heliosdb-nano code-graph hook` is opt-in** — it's only present if the binary was built with `--features code-graph`. If users see "unknown subcommand", reinstall.
- **Languages**: Phase-1 ships Rust, Python, TypeScript, Go, Markdown, SQL. Adding a language requires registering its tree-sitter grammar via `register_grammar`.
- **Large repos**: first full index can take minutes. Subsequent commits are fast (Merkle-hashed file diffing skips unchanged files — `files_unchanged` in the stats).
- **`bulk_insert_tuples` direct-write path**: ingest writes via the in-process bulk path, not via `INSERT … VALUES`. Don't try to ingest from outside the same OS process while the indexer is running.
- **FK in transaction (v3.21.x bug, fixed in v3.22.1)**: cascading deletes inside a transaction once raised phantom FK violations. Pin to `>=3.22.1` for the per-file delete-stale path.
- **Cross-process `INSERT … ON CONFLICT (path) DO UPDATE`** is still racy — see `FEATURE_REQUEST_cross_process_on_conflict.md`. The hook subcommand and SDK both run in-process, so this only matters if you wrap your own multi-process ingester.

## See also
- `heliosdb-nano-graph-rag` — wraps the symbol graph with seed/expand/rerank for RAG retrieval.
- `heliosdb-nano-mcp` — exposes code-graph results to AI agents via MCP tools.
- `heliosdb-nano-vector` — pair with embeddings for semantic similarity over symbols.
- `docs/code_graph/{overview,pilot,troubleshooting}.md` — design + troubleshooting docs.
- `tests/code_graph_phase2.rs` — acceptance fixtures (incremental, force-reparse, populated KB).
