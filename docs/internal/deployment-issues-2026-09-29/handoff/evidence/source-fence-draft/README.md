# External source admission draft

This draft is not installed and has not been compiled or executed. No frozen worktree was changed. `rustfmt` parsed the coordinator and engine proposal; `git apply --check` accepted the engine patch against its recorded base.

Artifacts:

- `physical_source_coordinator.rs`: proposed `replication::physical_source_coordinator`, SHA256 `edc2b97bcdece484767124ad98c680c77e8e59b1dfbffac99dc398604f57560e`; eleven authored tests.
- `storage-source-admission.patch`: exact engine field/constructor/getter and propagation delta, SHA256 `cbb5cfe4518dbb65b0f4f3a604ebaaa22ce9cc335abcfdb6926d756d154eb779`; full external file and base hash in `engine-proposal.rs`/`storage-source-admission.json`.
- Depends on transport owner's capture/finish checkpoint split and root owner's Transaction/VersionGc setter/builder patches. Add coordinator module under the same `ha-tier1` gate as physical replication.

## Interfaces

`StorageEngine::physical_source_operation(&self) -> crate::Result<Option<PhysicalOperationPermit>>` is crate-visible under HA. The no-HA variant returns `crate::Result<Option<()>>` and always None. Default-disabled/read-only/in-memory engines have no coordinator and acquire no mutex. `physical_source_coordinator(&self) -> Option<Arc<PhysicalSourceCoordinator>>` is the HA getter. The coordinator is created immediately after persistent RocksDB open, before background owners, when physical replication is enabled and the engine is writable. The one exact Arc must propagate to all mutators; construction alone does not complete coverage.

Coordinator APIs: `new(Arc<DB>)`, `operation()`, `fence(Duration)`, `poison_certification(reason)`, `capture_barrier(history, timeout)`, `capture_checkpoint(history, parent, limits, timeout)`. Capture returns `(CapturedCheckpoint, CommittedPhysicalBarrier)`; call captured.finish() after the coordinator method returns, so file hashing occurs outside the fence. Ordinary `OnlineCheckpointExport::create` remains an explicitly raw checkpoint helper and does not mint a committed capability.

The barrier has private Arc<DB>/history/sequence, public history()/source_sequence(), and crate-only checked_sequence(db,history) refusing another DB pointer or history. It cannot be deserialized from arbitrary peer integers. Transport's poll_source_through must enforce its fixed cap, never replace it with a later raw DB.latest_sequence.

## Admission and failure contract

Each complete synchronous semantic operation acquires an owned !Send/!Sync permit. Reentrancy is per OS thread, not async task; guards must never cross await, including a local executor. Existing same-thread nested operations can enter despite a waiting fence; new threads wait. All nested guards share an explicit depth count, so dropping the outer guard first does not release the operation. Multiple fences exclude each other. The positive <=1h timeout bounds drain waiting only, not synchronous RocksDB checkpoint/flush or kernel I/O.

Self-fencing and entering an operation while the same thread owns a fence fail. Panic in an operation or capture disables future certification. Explicit uncertain durable semantic errors must call poison_certification after their error boundary is audited. This is sticky without reset, but ordinary primary operations continue. Structural admission/accounting failure separately refuses operation admission. Do not indiscriminately poison every constraint/parse error or compare a global sequence delta to infer this operation wrote: other operations may write concurrently.

A fence samples and fsyncs an exact raw sequence, validates the actual primary history/IDENTITY, captures checkpoint files, then checks exact cut and unchanged source sequence before returning. Detecting a raw write during capture disables certification. This detection is diagnostic, not protection against unadmitted writers that run between checks. No complete SQL-boundary claim is valid until every mutator and semantic-error boundary participates.

## Authored tests

Nested admission with a queued fence and reversed guard drop order; queued-fence priority over unrelated new operations; drain timeout removal and self-fence refusal; independent engine coordinators; panic and explicit certification-only refusal; two raw batches held inside one complete operation captured together with later writes admitted before hashing; DB/history capability mismatch; concurrent fence exclusion; detected raw bypass refusing certification while primary writes remain available.

Still required: all public/raw engine and catalog writes; branch constructors/merges; snapshot manager, logical-WAL, sequences, row-counter and sidecar writes; remaining background workers and shutdown ordering; error-boundary audit; fixed committed-barrier source adapter/wire integration; sealed serving publication; runtime regressions, feature gates, and performance evidence. Public raw Arc<DB> access is an explicit protocol bypass unless wrapped by its caller. No narrower readiness contract is proposed.

Durable phases: begin_durable_phase(reason) retains its own nested operation. Dropping an incomplete phase poisons certification before releasing admission; complete() disarms only that phase. Two added tests cover a waiting fence observing failed durable work and normal prewrite errors remaining unpoisoned. Engine helper storage-durable-phase-helper.patch exposes physical_durable_phase(reason) -> Result<PhysicalStoragePhase>, with complete() and default-disabled/no-HA no-op behavior.
