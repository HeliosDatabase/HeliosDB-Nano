**Several statements in one MySQL query are each run and answered.**

A query that held several statements and started with `SET`, `USE` or `BEGIN`
was answered with one OK and the rest of it was dropped; a batch of writes was
answered with one OK carrying the sum of the affected rows.

Now the query is split at the separators outside literals, quoted names and
comments, and every statement gets its own answer, flagged "more results
follow" on all but the last. The first error ends the batch. As in MySQL, a
client that did not ask for multiple statements when it connected gets error
1064 for a second statement, and nothing of that query runs.

Tests: `tests/mysql_multi_statement.rs`.
