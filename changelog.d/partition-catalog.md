**Partitioned tables are recorded in the catalog and the PostgreSQL catalogs describe them.**

Wrong or missing: `PARTITION BY` was discarded and `PARTITION OF` stored only a parent-child link, so nothing knew a table's partition key or a partition's bound. `pg_class` reported every table as an ordinary one, `relpartbound` was always NULL, `pg_inherits` was empty, and a dump restored partitions as unrelated tables.

Now: the partition key and each bound are written with the table, in the same write, and survive a restart, a rename, a physical standby and a dump in both formats (an SQL dump writes `PARTITION BY` and `ALTER TABLE … ATTACH PARTITION`). `pg_class.relkind` is `p` for a partitioned table, `relispartition` and `relpartbound` describe a partition, `pg_inherits` and `pg_partitioned_table` list them, `pg_get_partkeydef()` returns the key, and `psql`'s `\d` shows partitions and bounds.

Tests: `tests/partition_catalog.rs`, `src/sql/partition.rs`, `src/protocol/postgres/partition_wire_tests.rs`.
