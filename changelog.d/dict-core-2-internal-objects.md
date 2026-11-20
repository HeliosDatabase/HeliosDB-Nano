**A table named `mv_...` is a table; the storage of a materialized view is not listed as one.**

Listings decided which tables belong to the engine by testing name prefixes in about a dozen
places, and several tested the wrong prefix: a user table whose name starts with `mv_` was
reported as a materialized view by `sqlite_master`, left out of the compression views
(`heliosdb_compression_events`, `heliosdb_config`), of the MCP table listings and of
`ANALYZE` over all tables, while the table that really stores a materialized view's rows
(`__mv_<view>`) was listed as an ordinary table.

One function now classifies a catalogued table (materialized-view storage, code-graph table,
knowledge-graph table, other engine table, session temporary table, bookkeeping of an earlier
release), and every listing asks it:

- A user table named `mv_x` is a table everywhere.
- The storage of a materialized view `v` is no longer a row of `information_schema.tables`,
  is listed by `sqlite_master` as the view `v`, is listed by `pg_class` as `v` with
  `relkind = 'm'` (its columns still join through `pg_attribute`), and is left out of the
  compression views and the MCP table listings.
- `_hdb_code_*` and `_hdb_graph_*` tables are listed as before.

`information_schema.columns` and `pg_indexes` still name the storage table of a materialized
view.

Tests: `tests/dict_core.rs`, `src/storage/internal_relations.rs`.
