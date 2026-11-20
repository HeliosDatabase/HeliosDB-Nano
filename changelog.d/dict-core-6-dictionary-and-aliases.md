**`hdb_dictionary` lists every system view; an alias is a projection over its canonical view.**

There was no way to ask the database which system views it has, and each compatibility name
(`pg_*`, `information_schema.*`, `sqlite_master`, `heliosdb_*`) was its own implementation.

- `hdb_dictionary` has one row per name the dictionary answers to: `name`, `kind` (`view` or
  `alias`), `canonical` (the view an alias reads; NULL for a view), `family` (`hdb`,
  `postgresql`, `information_schema`, `sqlite`, `mysql` or `legacy`) and `description`.
- An alias is a name, the canonical view it reads, and for each of its columns the expression
  over the canonical view that produces it. The planner expands it into that projection over a
  scan of the canonical view, so an alias can be filtered, joined, aggregated and described
  like any relation and cannot disagree with its canonical view.
- The first canonical view and alias: `hdb_features` (the Cargo features compiled into the
  binary) is canonical, and `heliosdb_features` is its alias with the same columns. Statements
  that read `heliosdb_features` keep working.
- `hdb.<view>` is the schema-qualified spelling of `hdb_<view>` (`SELECT * FROM hdb.dictionary`).

Tests: `tests/dict_core.rs`, `src/protocol/postgres/dict_core_wire_tests.rs`,
`src/sql/dict/alias.rs`, `src/sql/dict/dictionary.rs`.
