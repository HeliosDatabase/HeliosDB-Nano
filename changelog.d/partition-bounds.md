**Partition bounds are validated and enforced.**

Wrong: a partition accepted any row, overlapping ranges and a second `DEFAULT` partition were accepted, `MODULUS 2, REMAINDER 5` was accepted, and `ATTACH PARTITION` did nothing.

Now: an overlapping bound, a second `DEFAULT`, a remainder not below its modulus, an empty range and a bound of the wrong kind are refused (42P17). `ATTACH PARTITION` checks the table's columns and keys and that its rows fit the bound, and that the `DEFAULT` partition holds no row the new partition claims (23514); `DETACH PARTITION` turns the partition back into an ordinary table. A write straight to a partition that does not fit its bound fails with 23514 "new row for relation … violates partition constraint", on every insert and update route. This is a behaviour change: statements that were accepted without effect can now fail.

Tests: `tests/partition_bounds.rs`, `tests/partition_stage0_tests.rs`, `src/protocol/postgres/partition_wire_tests.rs`.
