# Task 189 — Type-aware resolution scaffold

## Goal

Today the cross-file resolver uses qualified-name match plus a
heuristic IMPORTS edge pass. That misses common cases:

* `use foo::bar; bar();` — call site sees `bar`, definition is
  `foo::bar`.
* `from foo import bar` — same in Python.
* `import { bar } from './foo'` — TypeScript.
* `bar() // bar is a method on a known type` — needs receiver type
  to disambiguate.

Full HM/Pyright-grade type inference is out of scope. This task
ships a *scope-chain* resolver that handles the import + alias +
single-method-call cases, marking everything else as
`resolution = 'heuristic'`.

## Acceptance

* `lsp_definition('bar')` for a `use foo::bar; bar();` case
  returns the `foo::bar` definition with `resolution = 'exact'`,
  not just `'heuristic'`.
* `lsp_references(symbol_id_of_foo_bar)` finds the call site.
* `_hdb_code_symbol_refs.resolution` column distinguishes
  `'exact'` (resolved through scope chain) from `'heuristic'`
  (qualified-name match) from `'unresolved'`.

## Design

### Stage 1 — Scope tree per file

During parse, walk each AST emitting a Scope node for each block,
function, class, module. Each scope tracks:

* Local definitions (param, let, const, def).
* Imports brought into scope.
* Aliases (`use foo as f`, `import * as x`, `from foo import bar
  as b`).

Stored in `_hdb_code_scopes` with `(file_id, node_id, kind,
parent_scope_id, line_start, line_end)`.

### Stage 2 — Reference resolution pass

For each unresolved reference (`from_symbol`, `to_name`):
1. Find the smallest enclosing scope (by `(file_id, line)`).
2. Walk parent scopes; at each level, check imports + aliases.
3. If the name resolves to a different qualified path, look up
   that path in `_hdb_code_symbols.qualified` exact-match.
4. If found, set `to_symbol = matched.node_id` and
   `resolution = 'exact'`.
5. Else fall through to the existing heuristic path.

### Stage 3 — Per-language extractors

The scope-chain walker is generic but each language's extractor
needs to emit imports/aliases. Rust, Python, TypeScript, Go ship
this in phase 2. Add:

* `Symbol::Import` variant with `from_path: String, alias: Option<String>`.
* Per-language hooks in `src/code_graph/symbols.rs`.

## Files to touch

* `src/code_graph/storage.rs` — add `_hdb_code_scopes` table and
  `_hdb_code_symbol_refs.resolution` column.
* `src/code_graph/symbols.rs` — emit Import / scope spans per
  language.
* `src/code_graph/resolver.rs` — new module with `resolve_with_scopes(db)`.
* `src/code_graph/storage.rs::code_index` — call resolver after
  symbol/refs pass.
* `tests/code_graph_resolver.rs` — golden tests for
  Rust/Python/TS aliasing.

## Tests

1. Rust `use foo::bar; bar();` resolves to the `foo::bar`
   definition.
2. Python `from foo import bar as b; b()` resolves through the
   alias.
3. TypeScript `import { bar } from './foo'; bar()` resolves.
4. Unresolvable name still gets `resolution = 'unresolved'`.
5. Cross-file rename via `lsp_rename_apply` (#185) honors the
   resolved edges.

## Out of scope

- Generic / type-parameter inference.
- Method dispatch through inheritance / trait objects.
- Untyped Python duck-typing.
- Recovery from invalid syntax.
