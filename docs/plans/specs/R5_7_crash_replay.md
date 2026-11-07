# R5.7 — crash replay at open stops undoing committed writes

Parent: `docs/plans/specs/R5_statement_atomicity.md`, Part 1 decision 14. Sprinter 4595394176a9
(CRITICAL). On the tree that holds R5.1-R5.6 and B4: by then every logical-WAL entry rides its
change's own write group (R1 for DDL, R5.3 for catalog DDL, R5.4 for DML, R5.5 for rewrites).

## Why

`recover_wal_at_open` re-applies logical-WAL entries since the last checkpoint over a store that
already holds their effects, and a later write that left no entry (a PK-fast UPDATE or DELETE
before R5.4, a volatile ADD COLUMN's rewrite, a DROP COLUMN's rewrite) is undone by the replay of
an older image. Observed: committed PK UPDATE and DELETE undone by kill -9 (`[(1,6),(3,3)]` became
`[(1,1),(2,2),(3,3)]`), volatile ADD COLUMN values lost, dropped-column values shifted back. With
`logical_wal_per_statement = false` (the shipped default) it is worse than with true.

## Decision 14

Once every logical entry rides its change's write group, open ADOPTS rather than REDOES: it
advances the checkpoint and truncates, and never re-applies entries whose group is durable. Prove
it first, by census: no path still appends an entry in a group before or after its data. Deprecate
`wal_replay_destructive_ddl` (document it; keep the setting parseable).

## Items

1. Census: every `WriteAheadLog::append*` and every staged WAL op, against the group that carries
   its data. Write the result into this spec (path, group, verdict). R5.3's detector reports WAL
   appends outside a DDL batch; use it, and extend the census to DML.
2. Open: adopt. An entry whose effects are in the store (same group) is not re-applied. Decide what
   is left for replay to do at all (a store written by an older release whose entries did not ride
   their group: replay those as today, or refuse to open and name the tool, decided by a marker
   written when this release first opens a store).
3. Physical standby and the archive: their own recovery paths must not re-apply either.
4. Un-ignore the five crash tests in `tests/row_shape.rs` (they run under
   `logical_wal_per_statement` false AND true) and add: a PK-fast UPDATE and DELETE beside multi-row
   and transactional INSERTs, kill -9 at many points, reopen, compare with what was acknowledged;
   the same on a standby and after promotion.

## Acceptance

All five ignored crash tests pass in both modes; the review scripts `$SCRATCH/g/r52rev_scripts/
t_crash.py`, `t_crash2.py noalter_pk`, `t_crash3.py` give SAME in every scenario; no regression in
the archive and recovery suites.
