# Final accepted requirement mapping v5

Prepared command mapping only; all18 groups remain OPEN here. Root must supply a fresh immutable final-source capture; no old source inventory is embedded in v5. Runtime receipts are adjudicated separately.

## R0 — Standby posture stays up, typed data refusal, readiness/lag/history binding

Spec: `docs/plans/STANDBY_RECOVERY_PLAN.md#52-standby-posture-r0`.

Targets: `physical_standby_serving`, `physical_standby_refusals`, `ha_integration`, `physical_promote_online`.

Stages: `physical-default`, `physical-internal`, `readiness20`.

- Liveness /health returns200 while /ready is503 until servable. Unavailable reads return55000; writes return25006. History mismatch fails closed, with safe stop/restart.


## R1 — Stopped standby verify, safe leftover recovery, keyrotation guard and explicit txn replication

Spec: `docs/plans/specs/R1_standby_tooling.md`.

Targets: `physical_standby_verify`, `physical_fast_forward`, `physical_explicit_transactions`, `physical_standby_embedded`, `wal_replay_crash_residuals`.

Stages: `physical-default`, `physical-internal`, `core-internal`.

- Stopped verify: exit0 for identical digests at one source sequence, exit1 after an altered leaf, exit2 for running/pruned input. Prove stale completed copies are recopied, fresh copies adopted, key rotation refused, and explicit transactions/savepoints/rollback replicate across both PG protocols.


## R2 — No torn DDL reads on primary or physical standby; detector itself catches synthetic split

Spec: `docs/plans/specs/R2_ddl_window_cut.md`.

Targets: `physical_ddl_window`, `physical_ddl_window_repro`, `layout_epochs`.

Stages: `ddl-window`, `physical-default`, `physical-internal`.

- Run all4 exact window controls with1 pass each. Positive controls require0 torn answers and nonzero queries/groups/iterations; the deliberately split detector must observe torn answers. No skipped positive acceptance.


## R3 — Rewind and online switchover, preserved acknowledged writes, epoch/role/failure handling

Spec: `docs/plans/specs/R3_switchover_rewind.md`.

Targets: `physical_rewind`, `physical_switchover`, `physical_promote_online`.

Stages: `physical-default`, `physical-internal`.

- Switchover in both directions over TLS preserves every acknowledged row, fences the old primary before accepting new writes, and converges to an identical digest at one sequence. Killed targets and lost responses never create dual writers; stale history is refused.


## R4 — Durable HNSW dump cache lifecycle, encryption, no visible recall misses

Spec: `docs/plans/specs/R4_hnsw_dump_recall.md`.

Targets: `durable_index_tests`, `hnsw_recall_measurement`, `physical_standby_fireproof`, `vector_sql_integration_test`, `standard_vector_row_recovery`.

Stages: `core-default`, `core-internal`, `recall-measure`, `standard-publication-default`, `standard-publication-internal`, `checkpoint-interleaving`, `pq-live-codec`, `ann-publication`.

- Reload/close/reopen has no orphan dumps or dump failures; configured M and ef are observed. TDE leaves no plaintext graph dump. Public top1 controls stay strict, with retained raw results for50 seeded measurement cells.
- DefaultStandard real checkpoint/native-owning-row crash proof2checkpoint+6nativecells; actualcheckpoint serialization5controls; PQlive/latest save/load exactcodec+config/codebook with deletion and replacement.

Open: graph-only measurement tests print without asserting; independently adjudicate against strictpublicsearch and spec, never call2greenmeasurements recallacceptance

## R5 — One durable statement group, logical rows/history, bounded rewrites, layout coherence, startup adoption

Spec: `docs/plans/specs/R5_statement_atomicity.md`.

Targets: `table_write_lock`, `row_shape`, `statement_atomicity_ddl`, `statement_atomicity_dml`, `statement_atomicity_alter`, `statement_atomicity_mv`, `layout_epochs`, `direct_write_atomicity`, `direct_mutation_atomicity`, `fast_side_storage_atomicity`, `generic_mutation_indexes`, `historical_side_storage`, `rr_snapshot_deletes`, `transaction_snapshot_history`, `mv_delta_id_durability`, `raw_metadata_locks`, `row_counter_ordering`, `wal_atomic_adoption`, `fk_update_actions`, `fk_delete_actions`, `fk_merge_constraints`, `mv_uncertain_publication`, `mv_staged_dml_refresh`, `sql_prepare_params`, `alter_storage_params`, `fk_commit_boundaries`, `fk_merge_custom_routing`, `persistent_mv_atomicity`, `vector_row_overlay_recovery`, `commit_outcome_boundaries`.

Stages: `lib`, `core-default`, `core-internal`, `physical-internal`, `branch-all-interfaces`, `ddl-window`, `persistent-publication`, `applied-publication-errors`, `checkpoint-interleaving`.

- Execute all R5.1-R5.7 success/abort/savepoint/uncertain-write/crash controls with nonzero counts, relaxed+strict WAL, time travel on/off and both executor families. Count native groups containing data+catalog+WAL+sidecars+snapshot. Prove no sequence/advisory retry, exact FK actions/concurrency, strict partial MERGE resume, RR history and nonmutating54000 refusal.

Open: Final acceptance requires exact final-source compile and all applied/publication errors, overlay193/checkpoint5/newPQ and query-context controls to pass with source-bound receipts. Older focused results do not replace the full final matrix.
Open: Full R5.8 release ABBA and nativeclient/protocol matrix via E/perf-matrix-v3 + perf-build-orchestrator-v2 remain mandatory.

## R6 — Readiness diagnostics and startup/promotion stability under parallel load

Spec: `docs/plans/specs/R6_harness_readiness.md`.

Targets: `ha_integration`, `physical_promote_online`, `physical_fast_forward`.

Stages: `readiness20`, `physical-internal`.

- Each named readiness/promotion control succeeds20/20 under actual bounded parallel load. Capture first HTTP response and logs/process/listener diagnostics on failure; serial repetitions alone do not satisfy load coverage.

Open: New readiness20 stage prepares two concurrent native test processes,6casesx20, exact120identity receipts plus interval overlap. Runtime remains open; old serial repetitions do not satisfy this gate.

## R7 — Supported fastembed7/ORT API+MSRV with no obsolete truststore advisory waiver

Spec: `docs/plans/specs/R7_fastembed.md`.

Targets: `code_graph_local_embedder`, `code_graph_body_vec`.

Stages: `msrv-default`, `msrv-embed`, `embed-runtime`, `security`, `core-optins`.

- Compile default on Rust1.88 and code-embed on1.92 using the patched lockfile; resolve webpki-roots>=1.0.10 and run the actual embedder/model for384 dimensions. No obsolete RUSTSEC-2026-0381 waiver.

Open: compatibleORT1.24.x library/modelcache prerequisite mustbeprovided for olderhostglibc; compilation isnot runtimeembedcoverage

## P0 — Keyspace/writefunnel/openmode/localCF/options/diskbytesize foundations

Spec: `docs/plans/PHYSICAL_STANDBY_REPLICATION.md#phases`.

Targets: `keyspace_classification`, `keyspace_wal_corpus`, `write_gate_funnel`, `physical_standby_open_mode`, `physical_standby_local_namespace`, `physical_storage_hydration`.

Stages: `core-default`, `core-internal`, `physical-default`.

- No unclassified durable keys. Default-column-family bytes/digest match at one source sequence; local cursor is never copied. Read-only sequence/commit/open/close causes0 source writes; node options match supported open modes.


## P1 — Serving derived state, SQL readonlyadmission, startup/readiness, embeddedstandby

Spec: `docs/plans/specs/P1_1_physical_applier.md`.

Targets: `physical_standby_derived_state`, `physical_standby_vector`, `physical_standby_embedded`, `physical_standby_serving`, `physical_snapshot_hydration`, `ha_integration`.

Stages: `physical-default`, `physical-internal`, `physical-persistent`, `applier-throughput`.

- SQL rows/counts/indexes/kNN/AS OF agree with the source. Durable cursor resumes after a kill; every typed write entry refuses without sequence movement. Routines/triggers/schema hydrate strictly.

Open: LiveMySQL+REST/COPY standbyadmission mustbeproven through actuallisteners or currentdeployment harness, not inferred from embeddedrefusal

## P2 — Oneversionedcontrolconnection, physicalACKsync, TLS/remote/resumablebootstrap

Spec: `docs/plans/specs/P2_2_tls_remote_resumable.md`.

Targets: `physical_standby_sync`, `txn_replication_sync`, `physical_standby_tls`, `physical_snapshot_resume`, `physical_standby_refusals`, `logical_standby_removed`.

Stages: `physical-default`, `physical-internal`.

- Paused applier blocks SYNC; minimum ACK/timeout policy preserves commit truth. Heartbeats never regress. TLS/mTLS reject wrong CA and wrong token; interrupted bootstrap really resumes; native v3 protocol is refused.
- P2_2§4 explicitly uses real daemons on127.0.0.2/.3 plus nonloopback SocketAddr admission unit; no independent-host requirement is introduced.
- OpenSSL CLI is a checked runtime prerequisite; its fixture-skip line is forbidden, so a6test green wrapper cannot bypass actual TLS/mTLS execution.

Open: Exact specified real-daemon TLS/mTLS scenarios on127.0.0.2/.3 plus nonloopback SocketAddr admission unit, wrongCA/token, interruption/reconnect/resume and native-v3 refusal remain runtime gates. Independent hosts/namespaces are optional additional battle-testing.

## P3 — Archive/window/catalog/recovery/cascading, prune-never-stall

Spec: `docs/plans/specs/P3_1_archive_segments_recovery_window.md`.

Targets: `physical_archive`, `physical_archive_recovery`, `archive_cli_tests`.

Stages: `physical-default`, `physical-internal`, `archive-reserve-native`.

- Outage beyond retention recovers from copied archive segments with exact digest. Corrupt/missing/cross-history/key input is refused; encrypted segments are0600. Killed archiver repairs correctly; window/max-bytes/reserve controls never stall the primary.
- P3_1§6 reserve failure uses HELIOSDB_NANO_TEST_ARCHIVE_FREE_BYTES in a REAL managed daemon; primary inserts must continue without errors.

Open: Run P3_1§6 reserve injection in REAL daemons with no insert errors, pruning/window/max-bytes ACK proofs. Real quota/full-filesystem probing is optional extended stress, not an additional mandatory spec gap.

## P4 — Tail/incremental/full recovery ladder, batch/leaf crashresume

Spec: `docs/plans/specs/P4_1_recover_from_primary_fast_forward.md`.

Targets: `physical_fast_forward`, `physical_large_write_group`, `physical_archive_recovery`, `physical_snapshot_resume`.

Stages: `physical-default`, `physical-internal`, `p4-scale`.

- Measure shipped bytes for1% changed data and prove the full-copy threshold, delete propagation, refusal of ordinary startup on a dirty recovery record, resumable rerun, and multi-MB group convergence on standby/archive.

Open: Exact explicitly ignored >=1GiB/1% scale probe now prepared in E/p4-scale-acceptance-v2; no runtime result yet. Require cut whole-CF digest, shippedbytes report, SIGKILL/resume, dirty503 and acknowledged postcut tail. Report locality and retention version churn and0extra rows; uniform random1% can legitimately select full-copy. Existing threshold-control remains separately required.

## P5 — Offline/online/forcedpromotion fencing epochs and postpromotion recovery

Spec: `docs/plans/specs/P5_1_epoch_promote_fence.md`.

Targets: `physical_promote`, `physical_promote_online`, `physical_switchover`, `physical_rewind`.

Stages: `physical-default`, `physical-internal`.

- Reachable old primary is fenced before promotion; forced unreachable promotion emits an explicit data-loss warning. Idle/prepared sessions receive57P01; new epoch accepts writes, stale primary self-fences, interrupted promotion resumes. Reopen preserves AS OF, vector and byte/digest checks.

Open: Final persistent MV/row publication follows same configured backend through physical/read-only/promotion with exact rows/vectors/ASOF/byte image; run final integrated proof sources and existing promotion/rewind suites.

## B1 — Perconnection/request branch binding and caches/startup/drop races

Spec: `docs/plans/specs/B1_session_branch.md`.

Targets: `branch_session_isolation`.

Stages: `branch-all-interfaces`, `core-default`, `core-internal`.

- Independent PG simple/extended and embedded sessions, concurrent REST requests and MCP calls keep branch binding and caches isolated. Cover bound branch DROP/recreate and startup options with actual interface receipts.

Open: MySQL per-connection binding is a declared path requiring explicit native-client proof. The specified40 concurrent REST requests and MCP branch scopes can be proven by current request/tool integration harnesses; external HTTP hosting is optional additional deployment evidence.

## B2 — All branch writes/constraints/txns/COPY/TRUNCATE stay in overlay

Spec: `docs/plans/specs/B2_branch_writes.md`.

Targets: `branch_write_isolation`, `fk_update_actions`, `fk_delete_actions`.

Stages: `branch-all-interfaces`, `core-default`, `core-internal`.

- PK/UNIQUE/FK/upsert/recursive cascades enforce the final branch dataset; rollback/savepoints preserve the overlay. Compare branch and main, including actual PG COPY/RETURNING.


## B3 — Branchcatalog isolation and MERGE finaldataset/conflicts/crashresume

Spec: `docs/plans/specs/B3_branch_schema.md`.

Targets: `branch_schema_isolation`, `branch_merge_surface_tests`, `fk_merge_constraints`.

Stages: `branch-all-interfaces`, `core-default`, `core-internal`.

- Supported DDL affects only its branch; unsupported DDL returns0A000 without effects. Cover DROP/recreate incarnations, target key claims, genuine FK conflicts, explicit abort, partial crash/resume, intent+policy in one group and a non-main target.


## B4 — Forkpoint NOW/ASOF ancestry/freezes/reclaim and documentation

Spec: `docs/plans/specs/B4_branch_fork_point.md`.

Targets: `branch_fork_point`, `branch_as_of_tests`, `code_graph_on_branch`.

Stages: `branch-all-interfaces`, `core-default`, `core-internal`.

- Child/grandchild row/side-storage/schema anchors remain frozen, including historical anchors and concurrent parent commit. Cover TDE, reclaim after DROP/MERGE, fresh branch IDs and crash/reopen.


