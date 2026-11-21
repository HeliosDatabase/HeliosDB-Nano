**`REINDEX` rebuilds; `CREATE DOMAIN` and `CREATE TABLESPACE` are refused.**

What was wrong: all three statements were accepted and did nothing. `REINDEX` of an index that does not exist
reported success; a script that declared a domain with a CHECK got columns that did not carry it;
`CREATE TABLESPACE` reported a tablespace that did not exist.

What happens now:

- `REINDEX { INDEX | TABLE | SCHEMA | DATABASE } name` builds the named index, or every index of the named
  table (or of every table), again from the table's rows and puts it in the place of the old one in one step,
  with the table locked against writers. An index or table that does not exist is an error. A PRIMARY KEY or
  UNIQUE index whose rows repeat a key is left as it was and the statement fails with 23505. `REINDEX SYSTEM`
  succeeds (the system catalogs are computed when read). A vector index is not rebuilt: `REINDEX INDEX` of
  one is refused (0A000), `REINDEX TABLE` leaves it as it is. `REINDEX` of a table the same transaction has
  written is refused (0A000); run it outside the transaction.
- `CREATE DOMAIN` is refused with 0A000: a column of a domain type would not have the domain's NOT NULL,
  DEFAULT and CHECK rules enforced. Declare the base type and the constraints on the column. `DROP DOMAIN IF
  EXISTS` still succeeds; `DROP DOMAIN name` reports that the type does not exist.
- `CREATE TABLESPACE` is refused with 0A000: a database is one data directory.

Tests: `tests/reindex_domain_tablespace.rs`.
