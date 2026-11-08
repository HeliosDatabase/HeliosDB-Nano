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


## Source census snapshot — 2026-11-07 UTC (runtime acceptance open)

Provenance: commit `967c3704` plus the frozen `integration-stage4` overlay, reviewed separately
with `generic-mutation-index` v1 and `v2-cache-fence`, MV delta-ID v1/type-audit v2, and the R57
recovery ordering fixes. The repeatable 428-file source view, input hashes, all 31 concrete engine
append sites and independent reports are preserved under:
`/home/gpc/HDB/sprint/baselines/nano/4420-resume-20261107/r56-source-audit/`.
`R57-WRITER-CENSUS.md` contains the caller/group/verdict table; `engine-append-sites.json` names each
engine emitter. These are source evidence, not a passed gate or a release acceptance record.

| Producer | Owning data group and operation | Source verdict |
| --- | --- | --- |
| `DdlBatch::commit` / `write_ddl_batch_ops` | Catalog/row/sidecar/snapshot changes plus all DDL operations staged before one gate write | Same group; independent of ambient capture |
| `Transaction::commit_with_timestamp` | Final insert/write/counter stages, versions and snapshot plus derived/captured/on-commit WAL operations | Same group; rollback/savepoint removes staged work |
| lib implicit text/params/session SQL `log_data_*`, including FK cascades | Active matching frame holds history while rows remain staged; owning transaction takes it at commit | Captured, then same group; explicit paths derive instead |
| Direct common/fast INSERT and fast UPDATE/DELETE, raw row delete | Supplied row/counter/history/side-storage batch uses `stage` or `write_direct_batch` | Same group; no deferral of an already-durable row to another capture |
| Durable counter allocation, including GroupCommit | Counter value and typed UpdateCounter are attached to the same group | Same group; mixed-writer ordering is a separately staged correction |
| Generic storage UPDATE/DELETE and MV replacement/refresh | Existing transaction or DdlBatch owns rows, deltas/cleanup, snapshot and operations | Same group; generic cache/index correctness and MV allocator floors have separate source/runtime gates |
| Prepared fast batch INSERT | One supplied row/counter/columnar/snapshot group; refuses when logical history is required | Supported relaxed-mode no-log path; no split append |
| Public `append_with_batch` | Supplied data and supplied operation always stage+commit together, even with an unrelated frame open | Same group |
| Bare `append*`, Engine `log_*`, Catalog `log_sequence_definition` | Standalone history only, or an owning SQL frame while its data is still staged | Explicit history; never automatic startup redo |
| Catalog `publish_table_schema` | Private legacy body with no current caller | Dead; normal schema writes use DdlBatch |
| Explicit replay/live logical apply | Destination's explicit mutation/cursor API, with recursive local logging suppressed | Explicit replay only; physical/archive recovery never calls logical redo |

No current INTERNAL producer in this snapshot makes data durable and then offers its logical
entry to a future unrelated capture. Public history-only APIs do not associate unrelated external
writes and must not be used as an atomic mutation substitute. The experimental sync change log
and partitioned lockfree WAL use different formats/APIs and are outside local `wal:entries` recovery.

The staged startup decision is `wal:atomic_groups = [1]`: read-only eligibility inspection first,
then writable-handle TDE verification under the raw directory lock, then one synced adoption group
that preserves the old history floor and retained log bytes. Current-format open adopts/prunes and
never invokes logical redo. Unknown/corrupt markers refuse. Ambiguous absent-marker legacy with
retained entries above its checkpoint refuses writable startup and names the explicit offline
`recover-wal --data-dir <existing-dir> --adopt-local-state` command. Read-only inspection remains
available. The offline command changes only four WAL metadata keys and refuses live/physical,
promotion and fenced directories. `wal_replay_destructive_ddl` remains parseable for explicit
replay compatibility; it cannot enable automatic startup redo or bypass migration checks.

The five crash tests are unignored in staged source. Required runtime work remains their actual
nonzero execution in both logical-WAL modes, the review scripts above, legacy/current/unknown
marker and byte-preserving migration controls, physical standby/archive/promotion regressions,
and the full correctness/performance matrix. Recursive FK/MERGE follow-ons, MV-ID producer/
concurrency follow-ons and the final external integration must be rechecked against this census
when frozen. No issue, R5.7 gate, or 4.42.0 release is accepted by this source snapshot.
