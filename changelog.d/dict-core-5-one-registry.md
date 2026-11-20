**`pg_tables` has one shape on every route; the second, unreachable view registry is gone.**

`pg_tables` answered in two shapes over the PostgreSQL wire: five columns when the statement had
no WHERE clause or a "simple" one (answered by the wire layer from the raw statement text), and
PostgreSQL's eight columns otherwise (answered by the SQL engine). The five-column answer also
reported every table under schema `public` and `hasindexes = true` for all of them.

`pg_tables` is now answered by the SQL engine only: `schemaname`, `tablename`, `tableowner`,
`tablespace`, `hasindexes`, `hasrules`, `hastriggers`, `rowsecurity`, with or without a WHERE
clause, on the simple and the extended protocol and on the embedded API. `schemaname` is the
table's real schema, `hasindexes` is true only for a table with a primary key, a unique column
or an index, and `hastriggers` is true for a table with a trigger. The storage of a
materialized view is not listed.

A second view registry that no statement could reach was deleted. Views that existed only
there never answered a query and still do not: `pg_branches`, `pg_snapshots`,
`pg_transaction_map`, `pg_scn_map`, `pg_stat_database`, `pg_stat_ssl`, `pg_stat_optimizer`,
`pg_compression_stats`, `pg_stat_replication`, `pg_stat_progress_vacuum`, `helios_sync_status`,
`helios_query_history`, `helios_table_memory_stats`, `helios_transaction_stats`,
`helios_topology`, `helios_node_aliases`, `helios_ha_status`. Rust code that used
`heliosdb_nano::sql::SystemViewRegistry`, `SystemView` or `ViewCategory` must use
`heliosdb_nano::sql::phase3::SystemViewRegistry` with a `heliosdb_nano::sql::dict::DictContext`.

Tests: `tests/dict_core.rs`, `src/protocol/postgres/dict_core_wire_tests.rs`,
`tests/system_views_tests.rs`.
