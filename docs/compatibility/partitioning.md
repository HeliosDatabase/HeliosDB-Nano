# Partitioning (stage 0: schema only)

HeliosDB Nano accepts PostgreSQL partitioning DDL so that schemas and pg_dump
output load, but it does **not** implement partitioning semantics yet.

## Accepted syntax

- `CREATE TABLE p (...) PARTITION BY RANGE | LIST | HASH (...)` — the clause is
  stripped; `p` is an ordinary, empty table.
- `CREATE TABLE c PARTITION OF p { FOR VALUES ... | DEFAULT }` — `c` is a plain
  table with `p`'s columns (types and NOT NULL).
- `ALTER TABLE p ATTACH PARTITION c ...` — checks both tables exist (42P01
  otherwise); otherwise no effect.
- `ALTER TABLE p DETACH PARTITION c` — unlinks `c` from `p`, so a later
  `DROP TABLE p` no longer drops `c`.
- `ALTER INDEX ... ATTACH PARTITION ...` — no effect.

Each of these statements raises a `WARNING` notice on the PostgreSQL wire and a
server-log line.

## What is not done

- Rows inserted into the parent are not routed to a partition.
- Selecting from the parent does not read its partitions.
- Partition bounds are not checked: a partition accepts rows outside its bound,
  and overlapping ranges and two DEFAULT partitions are accepted.
- A parent's PRIMARY KEY / UNIQUE constraints are not copied to its partitions.
- Dump/restore writes partitions as standalone tables, so a restored setup does
  not drop children with the parent.
- `DROP TABLE p` drops the partitions still attached to `p`.

## The policy setting

```toml
[storage]
partition_policy = "warn"   # default: accept as schema only, with a WARNING
# partition_policy = "error"  # refuse partitioning DDL with SQLSTATE 0A000
```

There is no silent value.
