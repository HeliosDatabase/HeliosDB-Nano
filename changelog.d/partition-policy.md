**`storage.partition_policy` is deprecated and has no effect.**

The setting chose between a warning and a refusal while partitioning DDL was accepted as schema only. Partitioned tables now work, so the "schema only" warning is gone and the value `"error"` no longer refuses anything. The key is still read, with a warning in the log, so existing configuration files load; remove it. MySQL-style partition lists inside `CREATE TABLE` (`PARTITION p0 VALUES LESS THAN …`, `PARTITIONS n`) are refused with 0A000 and a message that says what to write instead (they were a syntax error).

Tests: `tests/partition_stage0_tests.rs`, `src/config.rs`, `src/protocol/postgres/wire_tests.rs`.
