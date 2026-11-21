# Partitioning

HeliosDB Nano implements PostgreSQL's declarative partitioning on one node: a
partitioned table holds no rows of its own, every row is stored in the
partition its key belongs to, reading the table reads its partitions, and a
partition never holds a row outside its bound.

## What works

```sql
CREATE TABLE events (id INT NOT NULL, day DATE NOT NULL, note TEXT,
                     PRIMARY KEY (id, day)) PARTITION BY RANGE (day);
CREATE TABLE events_2024 PARTITION OF events FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');
CREATE TABLE events_rest PARTITION OF events DEFAULT;

INSERT INTO events VALUES (1, '2024-03-01', 'routed to events_2024');
SELECT * FROM events WHERE day = '2024-03-01';      -- scans events_2024 only
UPDATE events SET day = '2030-01-01' WHERE id = 1;  -- moves the row to events_rest
```

- **Strategies.** `RANGE` (lower bound inclusive, upper exclusive, `MINVALUE`
  and `MAXVALUE`, several key columns compared as a tuple), `LIST` (a list may
  contain `NULL`), `HASH` (`MODULUS`, `REMAINDER`), and one `DEFAULT` partition.
  A partition can itself be partitioned (`… PARTITION OF p FOR VALUES … PARTITION BY …`).
- **Key columns** of type boolean, smallint, integer, bigint, real, double
  precision, numeric, text, varchar, char, bytea, date, time, timestamp,
  timestamptz and uuid.
- **Routing.** `INSERT` into the partitioned table through every route
  (`VALUES`, `INSERT … SELECT`, `ON CONFLICT`, `RETURNING`, parameters, prepared
  statements, `COPY`, inside a transaction) stores each row in its partition. A
  row no partition accepts fails with SQLSTATE 23514, `no partition of relation
  "t" found for row`, with the key in the message. A statement that writes
  several partitions is all or nothing.
- **Reads.** `SELECT … FROM parent` returns the rows of every partition, in
  joins, subqueries, views and materialized views, with `AS OF`, and on a
  branch.
- **UPDATE, DELETE, TRUNCATE** on the partitioned table act on its partitions.
  An `UPDATE` that changes the partition key moves the row, or fails with 23514
  and changes nothing when no partition accepts the new key.
- **Pruning.** A predicate on the partition key (`=`, `<`, `<=`, `>`, `>=`,
  `BETWEEN`, `IN`, `IS NULL`, combined with `AND`; constants or bound
  parameters) restricts the partitions that are scanned, updated or deleted
  from. `EXPLAIN` shows the partitions that will be scanned.
- **Bounds are validated and enforced.** An overlapping bound, a second
  `DEFAULT`, a remainder that is not below its modulus, an empty range or a
  bound of the wrong kind is refused (42P17). A write straight to a partition
  that does not fit its bound, at any level, fails with 23514, `new row for
  relation "p" violates partition constraint`.
- **`ATTACH PARTITION`** checks that the table has the parent's columns (same
  names, types and order) and keys, that its rows fit the bound, and that the
  `DEFAULT` partition holds no row the new partition would claim (23514).
  **`DETACH PARTITION`** turns the partition back into an ordinary table that
  keeps its rows; `CONCURRENTLY` and `FINALIZE` are run as the plain form, with
  a notice. `DROP TABLE` of a partition removes it and its rows; `DROP TABLE` of
  the partitioned table drops its partitions.
- **Keys and defaults.** `PRIMARY KEY` and `UNIQUE` constraints, column
  defaults and `CHECK` constraints declared on the partitioned table apply to
  every partition, including those created later. As in PostgreSQL a primary
  key or unique constraint must include every partition key column (0A000
  otherwise). `ALTER TABLE` on the partitioned table (`ADD` / `DROP` / `RENAME
  COLUMN`, `SET` / `DROP NOT NULL`, `ADD` / `DROP CONSTRAINT`) is applied to
  every partition.
- **Catalogs.** `pg_class.relkind` is `p` for a partitioned table,
  `relispartition` and `relpartbound` describe a partition, `pg_inherits` links
  them, `pg_partitioned_table` and `pg_get_partkeydef()` give the key. `psql`'s
  `\d` shows the partition key, the partitions and their bounds.
- **Durability.** The partition key and the bounds are part of the catalog:
  they are written in the same write as the table, survive a restart, are on a
  physical standby, and are in dumps of both formats. An SQL dump writes
  `PARTITION BY` on the table and `ALTER TABLE … ATTACH PARTITION … FOR VALUES …`
  for each partition, which PostgreSQL reads too.

## Differences from PostgreSQL

- **Hash partitions hold different rows.** The hash is HeliosDB's own (see
  below), not PostgreSQL's, so a row is in a different hash partition than it
  would be in PostgreSQL. Data dumped *per partition* from PostgreSQL has to be
  loaded through the partitioned table, not into the partitions directly: a
  direct load fails with 23514 for the rows that hash elsewhere. The reverse
  holds too.
- **Not supported, refused with 0A000:** an expression, a collation or a
  non-default operator class semantics in the partition key (an operator class
  name after a key column is accepted and ignored); `CREATE INDEX` on a
  partitioned table (create the index on each partition); foreign keys declared
  on a partitioned table or referencing one; triggers on a partitioned table
  (create them on the partitions); attaching a table that is itself
  partitioned; creating or changing partitions on a branch; writing through a
  partitioned table that has a row-level security policy for that command.
- **`ONLY`** (`SELECT … FROM ONLY parent`) is not parsed.
- **Row-level security** policies of a partitioned table are applied to reads
  of it *and* to reads of its partitions by name.
- **Column changes** on a partitioned table are applied partition by partition
  and then to the table; they are not one atomic write across all of them.
- An `UPDATE` that changes the partition key always stores the row again
  (delete and insert), also when it stays in its partition. `ON CONFLICT DO
  UPDATE` cannot move a row to another partition (23514).
- **MySQL-style partition lists** (`PARTITION BY RANGE (d) (PARTITION p0 VALUES
  LESS THAN (10), …)`, `PARTITIONS n`) are refused with 0A000: declare the key
  alone and create each partition with `CREATE TABLE … PARTITION OF`.

## The hash of HASH partitions

A row goes to the partition with `partition_hash(key) % modulus = remainder`.
`partition_hash` is FNV-1a (64-bit, offset basis `0xcbf29ce484222325`, prime
`0x100000001b3`) over the concatenated encodings of the key values, followed by
MurmurHash3's 64-bit finalizer. Each value is encoded as one tag byte and:

| type | tag | encoding |
|------|-----|----------|
| NULL | `00` | nothing |
| boolean | `01` | one byte, 0 or 1 |
| smallint, integer, bigint | `02` | 8 bytes, big-endian two's complement |
| real, double precision | `03` | the IEEE-754 double, big-endian; `-0` as `0`, one NaN |
| numeric | `04` | 4-byte big-endian length, then the decimal text without trailing zeros |
| text, varchar, char | `05` | 4-byte big-endian length, then UTF-8 (char without trailing spaces) |
| bytea | `06` | 4-byte big-endian length, then the bytes |
| date | `07` | 4 bytes big-endian: days, day 1 = 0001-01-01 |
| timestamp, timestamptz | `08` | 8 bytes big-endian: microseconds since 1970-01-01 UTC |
| time | `09` | 8 bytes big-endian: microseconds since midnight |
| uuid | `0a` | the 16 bytes |

The function does not depend on the platform, the build or the release; fixed
vectors pin it in the test suite.

## Databases written by a release before 5.0

Before 5.0 partitioning DDL was accepted as schema only: the "partitioned"
table was an ordinary table that held every row written to it, its
"partitions" were ordinary tables, and neither the key nor the bounds were
recorded. Such a database opens unchanged: every row is where it was and is
read as before, and the tables stay ordinary tables. The server log names them
at open. `CREATE TABLE … PARTITION OF` on such a table is refused (42809) until
its key is declared.

To turn one into a partitioned table:

```sql
ALTER TABLE events PARTITION BY RANGE (day);          -- declares the key
ALTER TABLE events ATTACH PARTITION events_2024       -- once per partition
      FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');
ALTER TABLE events REDISTRIBUTE;                      -- moves the table's own rows
```

- `ALTER TABLE t PARTITION BY …` (not a PostgreSQL statement) works on any
  ordinary table without foreign keys and triggers. From then on new rows are
  routed, and the rows the table already holds stay in it: reads, updates and
  deletes through the table include them, so nothing is hidden at any step.
- `ATTACH PARTITION` validates the rows each former "partition" holds against
  the bound it is given; a table without the parent's primary key or unique
  constraints needs them first.
- `ALTER TABLE t REDISTRIBUTE` moves the rows the table itself still holds
  into the partitions, all or none: if one row has no partition the statement
  fails with 23514 and moves nothing (add the partition, or a `DEFAULT`
  partition, and run it again). It returns the number of rows moved. The log
  names the tables that still hold rows of their own at every open.

## The `storage.partition_policy` setting

Deprecated, without effect. It chose between a warning and a refusal while
partitioning was schema only. The key is still read (with a warning in the
log) so that existing configuration files keep loading; remove it.
