**Databases with schema-only partitions open unchanged and can be converted.**

A database written before 5.0 holds "partitioned" tables that are ordinary tables with all their rows. They open as they were: no row moves and none is hidden, and the log names them. New statements convert one: `ALTER TABLE t PARTITION BY …` declares the key (the rows the table holds stay readable through it), `ALTER TABLE t ATTACH PARTITION p FOR VALUES …` gives each partition its bound, and `ALTER TABLE t REDISTRIBUTE` moves the table's own rows into the partitions, all or none.

Tests: `tests/partition_legacy.rs`.
