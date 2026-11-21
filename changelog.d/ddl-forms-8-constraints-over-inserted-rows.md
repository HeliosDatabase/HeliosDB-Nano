**`ALTER TABLE … ADD CONSTRAINT … UNIQUE` / `… CHECK` works over rows the same transaction inserted.**

What was wrong: a migration that inserts rows and then adds a constraint in one transaction was refused
("… after writing it in the same transaction is not supported", 0A000).

What happens now: when the transaction's changes of the table are inserts only, a table-level `ADD
[CONSTRAINT name] UNIQUE (…)` or `ADD [CONSTRAINT name] CHECK (…)` validates the inserted rows together with
the committed ones. A duplicate among them, or against a committed row, is 23505; a row that violates the
CHECK is 23514; in both cases nothing is written. A UNIQUE constraint's index takes the inserted rows' keys,
so the constraint is enforced at once and key lookups find every row after COMMIT. Schema statements are not
transactional: after a ROLLBACK the constraint stays, over the committed rows.

Still refused (0A000), as `CREATE INDEX` in a transaction refuses them: a staged UPDATE or DELETE of the
table, a savepoint in the transaction, a table with column storage, a branch session. Also still refused over
a transaction's own rows: `ADD PRIMARY KEY`, `ADD FOREIGN KEY`, and the `ADD COLUMN … UNIQUE / CHECK /
REFERENCES` forms.

Tests: `tests/constraints_over_staged_inserts.rs`.
