**Rows written through a partitioned table go to their partition; reading the table reads its partitions.**

Missing: a "partitioned" table was an ordinary table that kept every row written to it, and reading it never looked at its partitions.

Now: `INSERT` into a partitioned table (VALUES, INSERT … SELECT, ON CONFLICT, RETURNING, parameters, COPY, in a transaction) stores each row in the partition its key belongs to, for RANGE, LIST, HASH and DEFAULT partitions and for partitions of partitions; a row no partition accepts fails with 23514 "no partition of relation … found for row" and the statement writes nothing. `SELECT` from the table returns the rows of all partitions. `UPDATE`, `DELETE` and `TRUNCATE` act on the partitions, and an `UPDATE` of the partition key moves the row. Keys, defaults and checks declared on the table apply to every partition, and column changes on the table reach them. Hash partitions use HeliosDB's own documented hash, so rows are in different hash partitions than in PostgreSQL.

Tests: `tests/partition_routing.rs`, `src/sql/partition.rs` (fixed vectors of the hash), `src/protocol/postgres/partition_wire_tests.rs`.
