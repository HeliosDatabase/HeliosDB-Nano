**A trigger records the function it runs: replace, drop, enable, disable and rename work, and survive a restart.**

What was wrong: a trigger did not store the name of its function. `CREATE OR REPLACE FUNCTION` and
`DROP FUNCTION` had no effect on an existing trigger, `CREATE TRIGGER` accepted a function that did not
exist, `CREATE OR REPLACE TRIGGER` kept the old trigger, literal arguments (`EXECUTE FUNCTION f('a')`)
did not parse, and there was no way to disable or rename a trigger.

What happens now:

- The function name and the arguments are stored with the trigger, in the same write as its definition.
  The function is looked up when the trigger fires, so a replaced function takes effect at once.
- `CREATE TRIGGER` naming a function that does not exist is refused (`42883`).
- `DROP FUNCTION` of a function a trigger runs is refused (`2BP01`); `DROP FUNCTION … CASCADE` drops
  those triggers too.
- `CREATE OR REPLACE TRIGGER` replaces the trigger. `DROP TRIGGER [IF EXISTS] name [ON table]`.
- `ALTER TABLE … ENABLE | DISABLE TRIGGER name | ALL | USER` and `ALTER TRIGGER name ON table RENAME TO
  new_name`.
- Definition, function, arguments, `WHEN` condition, column list and enabled state survive a restart and
  follow `ALTER TABLE … RENAME` and `DROP TABLE`.

Existing databases: a trigger created by an older build has no function recorded. It behaves exactly as
before the upgrade (a `BEFORE INSERT` row rewrite keeps rewriting, any other trigger does not fire), and
all such triggers are listed in one warning when the database is opened. Re-create each one with
`CREATE OR REPLACE TRIGGER … EXECUTE FUNCTION …` to make it run its function.

Tests: `tests/trigger_ddl_lifecycle.rs`.
