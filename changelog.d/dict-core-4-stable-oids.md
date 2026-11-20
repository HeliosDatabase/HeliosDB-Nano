**Object identifiers in the catalog views no longer change when other objects are created or dropped.**

`pg_class.oid` and the columns that refer to it were positions in a sorted list: a table's
identifier was 1000 plus its rank among the table names. Creating or dropping any table
renumbered the tables after it, so an identifier a client had cached pointed at a different
table afterwards. Table identifiers also ran into the array type identifiers of `pg_type`
(both started at 1000) and, with enough tables, into the constraint numbers. psql's `\d` used
a third numbering of its own.

An identifier is now derived from the object's kind and name, in a range (100000 and above)
that no built-in identifier and no type identifier reaches. It is the same in `pg_class`,
`pg_attribute.attrelid`, `pg_attrdef.adrelid`, `pg_index.indrelid` / `indexrelid`,
`pg_constraint.oid` / `conrelid` / `confrelid` / `conindid`, `pg_namespace.oid`, `pg_proc.oid`
and psql's `\d` lookup, in every process, and it does not move when unrelated objects come and
go. If two names ever hash to the same number, the one that sorts later takes the next number
of its own sequence, the same way every time.

An identifier that a client stored from an earlier release is not valid any more; read it again.
The identifiers of built-in schemas (`pg_catalog` 11, `public` 2200, `information_schema` 13183)
are unchanged; those of user schemas changed once with this release.

Tests: `tests/dict_core.rs`, `src/protocol/postgres/dict_core_wire_tests.rs`, `src/sql/dict/oid.rs`.
