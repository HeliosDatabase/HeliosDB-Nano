**A predicate on the partition key restricts the partitions that are scanned.**

Missing: there was nothing to prune.

Now: equality, ranges, `BETWEEN`, `IN` lists and `IS NULL` on the partition key, with constants or bound parameters, exclude the partitions that cannot hold a matching row, for reads, updates and deletes. `EXPLAIN` shows the partitions that will be scanned. A point query on a table with 64 partitions scans one.

Tests: `tests/partition_pruning.rs`, `src/sql/partition.rs`.
