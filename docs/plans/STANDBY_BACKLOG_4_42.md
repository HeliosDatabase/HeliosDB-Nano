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
| R2 consistent cut | the DDL-window torn-read test (decision 7); cut markers only if it fails | (decision 7) | `specs/R2_ddl_window_cut.md` |
| R3 switchover | rewind an old primary into a standby by fast-forward (no full copy); `SWITCHOVER TO` on the primary: the standby promotes online, the old primary demotes in-process and follows it | 1c8963417a35, bbba90ddfc61 | `specs/R3_switchover_rewind.md` |
| R4 HNSW | dump cache defects (leaked files after reload, per-clone dumps, constructor argument order, fsync/atomicity, encryption) and the recall flake family | 746342a0bfbb, 66aeb488f312 | `specs/R4_hnsw_dump_recall.md` |
| R5 write grouping | one write group per row at least (ideally per statement) on the primary's insert paths; DROP TABLE / REFRESH MATVIEW scans bounded (pg35 DDL profile) — after B4, it shares the write paths | 925803b35113, c214c2e73938 | `specs/R5_write_grouping.md` (after B4) |
| R6 harness | the daemon readiness flake | ffe805264aac | `specs/R6_harness_readiness.md` |
| R7 dependency | fastembed 4 → 7 so `code-embed` no longer pulls webpki-roots 0.26 (RUSTSEC-2026-0381 ignore removed) | b93af8c4a65f | `specs/R7_fastembed.md` |

Not in scope, by decision: moving the HNSW graph dump into RocksDB (decided against on
8f0721af1a70 — standbys never read the dump and rebuild indexes from rows; moving it would
put ~1.2× the vector bytes through the WAL on every close); the plan's optional change
tracking for fast-forward. Already delivered and only to be closed at release: field J
58d4e3113e56, serving rebuild 551a9cdd2e0b, catch-up window 36b56ac796e2, deployment #7
b4fafe6875ae, recv cancel-safety 0c9e1a4faab6 (8d1575a8), trigger-clone Drop 96e5a8cfa254
(4517cd09), catalog orphans 3b10bcb2a804 and prefix-mode scans e142fe16ef6d (fefaf131).
