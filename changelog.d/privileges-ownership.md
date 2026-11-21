**Objects have owners: DDL needs the owner, and the owner decides who is granted what.**

There was no ownership: any connection could alter or drop any table, and
`GRANT` recorded `helios` as the grantor whoever ran it. `WITH GRANT OPTION`
was recorded and meant nothing; `REVOKE GRANT OPTION FOR`, `GRANTED BY`,
`CASCADE` and `ALTER … OWNER TO` were not accepted.

Now the role that creates a table, view, materialized view, sequence, schema,
function or procedure owns it. `ALTER`, `DROP`, `CREATE INDEX` and `DROP INDEX`
need the owner (`must be owner of table t`, SQLSTATE `42501`); `TRUNCATE` needs
the owner or the `TRUNCATE` privilege; creating in a schema needs `CREATE` on
it; a foreign key needs `REFERENCES` on the table it points at. The owner grants
and revokes freely; another role needs the privilege `WITH GRANT OPTION` and
revokes only what it granted. `REVOKE` is refused while the grantee has granted
the privilege on (SQLSTATE `2BP01`) unless `CASCADE` is given. The grantor of
each grant is recorded and shown in `information_schema.table_privileges`.
`ALTER TABLE | VIEW | MATERIALIZED VIEW | SEQUENCE | SCHEMA | FUNCTION |
PROCEDURE | DATABASE … OWNER TO role` changes the owner. Dropping an object
forgets its grants, so a new object of the same name starts with none; renaming
a table keeps them.

`ALTER DEFAULT PRIVILEGES` is refused with SQLSTATE `0A000`. A routine defined
`SECURITY DEFINER` is refused with `0A000` too: until now the clause was
accepted and ignored, and the routine ran with its caller's rights.

Tests: `tests/privilege_ownership.rs`.
