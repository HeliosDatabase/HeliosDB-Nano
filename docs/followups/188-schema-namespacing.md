# Task 188 — `_hdb_code.schema` dotted namespacing

## Goal

Replace the flat-prefix tables (`_hdb_code_files`,
`_hdb_code_symbols`, `_hdb_code_symbol_refs`, `_hdb_code_merkle`,
`_hdb_graph_nodes`, `_hdb_graph_edges`) with proper
`schema.table` namespacing under `_hdb_code` and `_hdb_graph`,
while keeping the flat names working as aliases so existing code
doesn't break.

## Acceptance

```sql
SELECT * FROM _hdb_code.symbols WHERE name = 'foo';
SELECT * FROM _hdb_code.files;
SELECT * FROM _hdb_graph.nodes WHERE node_kind = 'DocChunk';
```
all work alongside the existing flat-prefix references.
`pg_catalog.pg_tables` lists the schema-qualified rows with
`schemaname` reflecting `_hdb_code` / `_hdb_graph` correctly.

## Design

### Catalog (`src/storage/catalog.rs`)

The catalog already stores tables under `meta:table:{name}` flat
keys. To preserve semantics with minimum disruption:

* Treat `schema.table` as a single composite name in catalog keys.
  `meta:table:_hdb_code.symbols` is the new canonical key.
* Add a `schema(&self) -> Option<&str>` accessor on `Schema` that
  parses any `.` in the table name as schema/table split for
  reporting purposes.
* Add a thin alias layer: `meta:alias:_hdb_code_symbols → _hdb_code.symbols`.
  Catalog lookups that miss the canonical key fall through the
  alias map. New code writes the canonical name; legacy code that
  still references flat names continues to work via alias.

### Planner (`src/sql/planner.rs`)

Sqlparser already handles `schema.table` as `ObjectName(["schema",
"table"])`. The current planner flattens it to a single string —
extend the table-resolution path to:

1. Try the canonical `schema.table` lookup first.
2. Fall back to the flat-name lookup.
3. Fall back to alias resolution.

### `pg_catalog.pg_tables`

`execute_pg_tables` in `src/sql/system_views.rs` already populates
the `schemaname` column with `'public'` unconditionally. Update to
emit the parsed schema (everything before the first `.`), or
`'public'` if the table name contains no dot.

### Migration

* On startup, the catalog scans for legacy flat-prefix entries and
  materialises the alias map in-memory. Idempotent.
* Existing data files keep working unchanged — we only renamed
  the catalog key + added the alias map.
* `code_graph::storage::ensure_tables` updated to use the new
  schema-qualified names. CREATE TABLE IF NOT EXISTS is idempotent
  so existing installs continue to find their data via the alias.

### Code-graph code paths

All `_hdb_code_files / _hdb_code_symbols / _hdb_code_symbol_refs
/ _hdb_code_merkle` references in `src/code_graph/` updated to the
schema-qualified names. Driven from a `const` table-name table at
the top of `storage.rs` so the strings live in one place.

## Files to touch

* `src/storage/catalog.rs` — alias layer + composite-name
  awareness.
* `src/sql/planner.rs` — schema-qualified resolution + fallback.
* `src/sql/system_views.rs` — `pg_tables.schemaname` from name.
* `src/code_graph/storage.rs` — table-name constants.
* `src/code_graph/lsp.rs`, `diff.rs`, `semantic_merkle.rs`,
  `sql_rewrite.rs` — table-name references.
* `src/graph_rag/schema.rs`, `search.rs`, `linker.rs`, `ingest.rs`,
  `with_context.rs`, `centrality.rs` — same.
* `tests/code_graph_namespacing.rs` — new, covers both syntaxes.

## Tests

1. New install: `_hdb_code.symbols` and `_hdb_code_symbols` both
   resolve to the same table.
2. `pg_tables.schemaname = '_hdb_code'` for namespaced tables.
3. Legacy install (data on disk under flat names) continues to
   work via alias.
4. Cross-schema joins: `SELECT s.name FROM _hdb_code.symbols s
   JOIN _hdb_graph.nodes n ON n.source_ref = ('code_symbol:' ||
   s.node_id)` works.
5. DROP via canonical name removes both alias entries.

## Out of scope

- Multi-tenant `tenant.schema.table` 3-part names.
- `CREATE SCHEMA` / `DROP SCHEMA` DDL — we hardcode the two
  schemas (`_hdb_code`, `_hdb_graph`).
- Changing data layout on disk. Pure catalog rename.
