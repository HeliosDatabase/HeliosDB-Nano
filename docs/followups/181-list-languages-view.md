# Task 181 — `hdb_code.list_languages` system view

## Goal

Surface the live grammar set as a SQL-queryable table so agents and
admins can introspect what the indexer can parse without reading
Cargo features.

## Acceptance

```sql
SELECT name, source FROM hdb_code.list_languages();
```
returns one row per `SupportedLanguage` enum variant (source =
`'static'`) plus one row per dynamically-registered grammar (source
= `'runtime'`), ordered by name.

## Design

* New SQL function `hdb_code.list_languages()` registered as a
  built-in scalar-returning-set table function in
  `src/sql/system_views.rs`.
* Reads `SupportedLanguage::all()` plus
  `code_graph::parse::registered_grammars()`. Both already exist;
  this is purely a surface task.
* Schema: `(name TEXT, source TEXT)`. Source is `static` or
  `runtime`.
* Gated on `#[cfg(feature = "code-graph")]`.

## Files to touch

* `src/sql/system_views.rs` — register the view + executor.
* `src/code_graph/mod.rs` — re-export `SupportedLanguage` if not
  already.
* New test: `tests/code_graph_list_languages.rs` (3 cases).

## Tests

1. Default — 8 static rows, no runtime rows.
2. After `register_grammar("custom_lang", …)` — adds one
   runtime row.
3. After `unregister_grammar("custom_lang")` — back to 8 rows.

## Out of scope

- pg_catalog `pg_languages` shim — Postgres `pg_language` carries
  procedural-language metadata, semantically different. Our view
  lives under the `hdb_code` namespace, not `pg_catalog`.
