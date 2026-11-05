# Physical standby backlog for 4.42.0

The physical standby plan (`docs/plans/PHYSICAL_STANDBY_REPLICATION.md`, phases P0-P5) is
delivered on `feat/physical-standby`. Maintainer decision 2026-10-30: 4.42.0 ships it
together with the branch-isolation work (`docs/plans/BRANCH_ISOLATION.md`) AND with the
standby backlog below finished. Work branch `fix/4420-standby-backlog` (worktree
`nano-r`, from the backlog branch at 429ce5aa); R3 and R4 run in their own worktrees and
merge back.

| Group | Delivers | Sprinter | Spec |
|---|---|---|---|
| R1 tooling and guards | `heliosdb-nano standby verify --against <primary>` (plan Verification §3); `recover` no longer adopts a stale leftover full copy; key rotation refused while it would strand a standby; plan doc corrected; residual items re-triaged with tests | 6146b39a5beb, e21903322db7, a3ffb9ef3c2c | `specs/R1_standby_tooling.md` |
| R2 consistent cut | the DDL-window torn-read test (decision 7). It found tears on the standby AND on the primary; a standby reader gate with cut markers was built and rejected after review; the windowed test is R5's acceptance | (decision 7) | `specs/R2_ddl_window_cut.md` |
| R3 switchover | rewind an old primary into a standby by fast-forward (no full copy); `SWITCHOVER TO` on the primary: the standby promotes online, the old primary demotes in-process and follows it | 1c8963417a35, bbba90ddfc61 | `specs/R3_switchover_rewind.md` |
| R4 HNSW | dump cache defects (leaked files after reload, per-clone dumps, constructor argument order, fsync/atomicity, encryption) and the recall flake family | 746342a0bfbb, 66aeb488f312 | `specs/R4_hnsw_dump_recall.md` |
| R5 statement atomicity | every statement changes the primary's reader-visible state in one write group (row-shape decode funnel, per-table writer lock, layout epochs with bounded retry, explicit DDL batches, missing-value ADD COLUMN, one-group DROP COLUMN up to a cap, INSERT ... SELECT one group); DROP TABLE / REFRESH MATVIEW scans bounded (pg35 DDL profile). On top of B3, in parallel with B4 | 925803b35113, c214c2e73938 | `specs/R5_statement_atomicity.md` |
| R6 harness | the daemon readiness flake | ffe805264aac | `specs/R6_harness_readiness.md` |
| R7 dependency | fastembed 4 → 7.5 so `code-embed` no longer pulls webpki-roots 0.26 (RUSTSEC-2026-0381 ignore removed) | b93af8c4a65f | `specs/R7_fastembed.md` |

Not in scope, by decision: moving the HNSW graph dump into RocksDB (decided against on
8f0721af1a70 — standbys never read the dump and rebuild indexes from rows; moving it would
put ~1.2× the vector bytes through the WAL on every close); the plan's optional change
tracking for fast-forward. Already delivered and only to be closed at release: field J
58d4e3113e56, serving rebuild 551a9cdd2e0b, catch-up window 36b56ac796e2, deployment #7
b4fafe6875ae, recv cancel-safety 0c9e1a4faab6 (8d1575a8), trigger-clone Drop 96e5a8cfa254
(4517cd09), catalog orphans 3b10bcb2a804 and prefix-mode scans e142fe16ef6d (fefaf131).

## R1 re-triage (Sprinter e21903322db7, a3ffb9ef3c2c)

Verdicts on the residuals of `REPLICATION_4_41_1_DESIGN.md` §7-8 now that every standby
is physical: a standby applies the primary's RocksDB write groups and never reads the
logical WAL (it has no production reader left: `StorageEngine::logical_wal_catchup`,
engine.rs:12518, is called by tests only), ACKs name the physical sequence, the receiver
refuses a sequence gap (physical.rs:1636, `apply_source_with`, 8d1575a8) and a resume is
verified by its last group's content (physical_source.rs:1222, `verify_last_group`,
NPHYS004 `Diverged`). What is left of the logical WAL is the PRIMARY's own redo window,
replayed at open after a crash (engine.rs:3711 `recover_wal_at_open` → engine.rs:12249
`replay_wal_after`, entries above `wal:checkpoint`). Line numbers are on this branch
after R1.

| Residual (4.41.1 §7-8) | Standbys | Primary's own crash recovery | Verdict |
|---|---|---|---|
| LSN continuity: LSNs are not a commit order and can have holes; catch-up cannot verify contiguity | Does not apply: no standby consumes LSNs; continuity is the physical sequence, checked per group (physical.rs:1636) and at every resume (physical_source.rs:1222) | A hole is an absent key, skipped by the replay (wal.rs:1367 `replay_from` reads what exists). Order: a transaction's entries are reserved and staged into its own commit batch (wal.rs:853 `stage`, transaction.rs:1739), so a row's entries are in the order its writes committed | **Closed** for standbys; harmless for redo (crash tests below) |
| ACK identity is the numeric LSN (`Received`); `report_applied` has no caller | Does not apply: an ACK is a `Heartbeat {applied_sequence, durable_sequence}` (physical_wire.rs:803) recorded by the source (physical_source.rs:1472 → ha_state.rs:338 `record_ack`); a commit waits on the physical sequence read right after its write (wal.rs:834 → storage/replication_wait.rs `barrier`/`wait` → ha_state.rs:715 `wait_for_sequence`). `report_applied`, `report_flushed` and the LSN `wait_replicated` no longer exist (no match in `src/`) | Not involved | **Closed** (P2.1) |
| `log_*` outside a capture frame appends at once, BEFORE its catalog change (DDL inside an explicit `BEGIN`) | Does not apply: a standby receives the catalog change's own write group | Applies. An entry appended on its own — before the change (as on the base) or after it — is not atomic with it. Before: a change that then failed (I/O error) left a phantom entry the next crash replays (a CREATE its client was told failed; a DROP or RENAME under `warn`/`apply`). After: a crash between the change and the entry left the change without it, while the table's CREATE and inserts above the checkpoint were still there to be replayed — the dropped or renamed-away table came back; and a failure to append, reported only as a WARN, acknowledged the DDL anyway and skipped the replication wait | **Fixed**: CREATE TABLE, DROP TABLE and the rename stage their entry INTO their own catalog write group (`StorageEngine::write_ddl_batch`, engine.rs:2736: `WriteAheadLog::stage` → write → `publish_staged`, or `abandon_staged` on a failed write, whose error is returned), also inside a statement-capture frame (the change is not transactional); the unwind of a failed CREATE deletes with a `DropTable` entry. The replication wait is the DDL's last step, after its in-memory finalization (catalog.rs:1110, :2001, :3759). Crash tests (`internal-tests` failpoint right after the catalog write, every policy): `tests/wal_replay_crash_residuals.rs::a_rename_crashed_after_its_catalog_write_does_not_come_back`, `::a_drop_crashed_after_its_catalog_write_does_not_come_back`, `::ddl_inside_an_explicit_begin_crashed_after_its_catalog_write`; the shapes without a failpoint: `::the_residual_logging_shapes_replay_to_the_committed_state`. Views, sequences, schema changes and indexes log after their write; `CREATE FUNCTION`/`PROCEDURE` (lib.rs:6590, :6630) still log before their durable record (non-destructive; a phantom needs an I/O error after the log) — left as is |
| (found with it) a transactional TRUNCATE (text family, GH#36) logged at once | Does not apply | Applies: `BEGIN; TRUNCATE; ROLLBACK` left a `Truncate` entry, and `warn`/`apply` truncated the table the rollback kept | **Fixed**: the entry goes with the transaction's commit (`Transaction::log_on_commit`, transaction.rs:989, emitted at transaction.rs:1683, cut back by `ROLLBACK TO SAVEPOINT`; lib.rs:11137). Test `::begin_truncate_rollback_keeps_the_rows_under_every_policy` |
| Autocommit DDL logged at commit, after the catalog change: a crash between leaves a table the standby never receives | Does not apply: the standby receives the change's write group itself | Superseded by the row above: the entry is now in the change's own write group | **Fixed** with the row above |
| Group-commit ordering when `sync_commit` is on not re-measured; no performance measurement | Not a correctness item: a sync commit's wait is on the physical sequence (above) | Not involved | **Stays open as a perf-gate item** (release A/B of commit latency, plan "Verification" §4); not in R1 |
| (§7) Native protocol version not bumped | Closed: one versioned protocol, `NPHYS004`; mixed versions refused | — | **Closed** (P2.1, P5.1 §11.F) |
| (§7) `replay_from` loads the whole retained range into memory | Does not apply | Applies to open recovery only, bounded by the periodic checkpoint (default 1000 entries / 30 s, wal.rs:744) and the clean-close checkpoint (engine.rs:12065) | **Harmless under the default checkpoint settings**; unbounded only if an operator sets both intervals to 0 (documented risk, not changed) |

**Defect found by the re-triage, fixed (affects only the primary's own crash recovery).**
The entries BEFORE a destructive DDL entry (`DropTable`, `Truncate`, `RenameTable`) that
name the same table describe the incarnation that DDL ended. They were replayed and
re-applied to whatever bore the name at the crash: under `refuse` (GH#35 Half B,
engine.rs:12116) around the skipped destructive entry, and under `warn`/`apply` too,
because the replay batched their rows and wrote the batch only after it had run the DDL
entries that followed. With logged rows (any explicit transaction; every autocommit
statement on an HA primary, where `fast_dml_requires_logical_wal` is true,
engine.rs:4910): a table created and dropped since the last checkpoint came back with
its rows, a table dropped and re-created under the same name received the old rows
(overwriting its own at colliding row ids), a renamed table's old rows came back under a
re-created source name. Fix: (1) a destructive entry fences off every EARLIER entry
naming one of its tables, under every policy (engine.rs:12173 `destructive_fences`,
applied in `replay_wal_after` at engine.rs:12316; a rename fences both names) — sound
because a destructive entry is now in its change's own write group (above), so what it
ended is already absent from the store; (2) the pending row batch is written before any
entry that changes the store at once (engine.rs:12196 `replays_into_batch`, at
engine.rs:12353). Tests: `::a_dropped_and_recreated_table_keeps_only_its_new_rows_under_every_policy`
(red under every policy: `d = [2, 3, 10]`) and
`::a_destructive_entry_fences_the_entries_of_the_incarnation_it_ended` (`refuse`: red with
`x` resurrected and `s = [2, 9]`). Under `warn`/`apply` a replayed rename still consumes
a re-created source table — the documented hazard of those escape hatches (GH#35).

**a3ffb9ef3c2c — explicit transactions reach a standby.** The defect was the logical
standby's (an explicit transaction emitted no logical WAL). Physical replication is
API-independent: the standby applies the primary's RocksDB write groups, whatever made
them — embedded API, PG simple or extended protocol, MySQL wire, REST — so the question
is only whether a transaction commits as write groups, which every path does.
`tests/physical_explicit_transactions.rs` runs, over the PG wire on the physical pair
harness, `BEGIN … COMMIT` (insert, update, delete), `BEGIN … ROLLBACK` and a
`SAVEPOINT` rolled back inside a committed transaction, in the simple protocol and in the
extended protocol with bound parameters; the standby holds exactly the committed rows and
the `range_digest` of its default column family equals the primary's at the same
sequence. **Closed.**

**Spec R1 notes.** `standby verify` needs a fast-forward session at a receiver AT the
primary's cut (an idle primary's new export is cut at the caught-up standby's cursor),
which every earlier release refuses (`after >= cut`). It is a negotiated capability,
`CAP_FAST_FORWARD_AT_CUT` (physical_wire.rs:203): the source serves a session at the cut
only to a receiver that offered it (physical_wire.rs:2753; `after > cut` stays refused,
physical_source.rs:1164), a receiver accepts one only when both offered it
(physical_wire.rs:3116), and `standby verify` against a primary without it is refused
plainly. `recover --from-primary` keeps its previous answer for a receiver at the cut (a
diverged or unaccounted store whose cursor equals the primary's sequence): a full copy —
its fast-forward record requires `from < to` (physical.rs `FastForwardRecord::validate`).
Key rotation (R1 §3) has no SQL, CLI or HTTP surface in this build and no re-encryption
path. The one entry point is now the store's own, `StorageEngine::rotate_encryption_key`
(engine.rs:2641): it computes the standing from that engine alone — its open mode (a
physical standby's) and the census of the replication source serving it
(`attach_replication_census`, engine.rs:2611, attached by `prepare_physical_source` from
the source's own `SessionCensus`, physical_source.rs:1436; no process-wide state) —
refuses on a standby, on a primary whose replication port serves (naming the standbys
attached), and otherwise refuses because nothing re-encrypts the sealed values.
`KeyManager::rotate` (crypto/key_manager.rs:276) is crate-private and takes that
standing too.
