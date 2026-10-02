# Transport subagent bounded draft handoff

Prepared 2026-09-29. Parent owns shared-host gates and overall disposition. User deadline reported by parent: 23:43:44 UTC; unfinished resync/auth/gates must be handed to OpenCode deepseek/deepseek-flash by then. This subagent ran no build, service or new benchmark for these drafts and acquired no heavy lock. Only external evidence copies/artifacts were changed; reviewed/frozen worktrees were not edited.

Evidence root: `/home/gpc/HDB/sprint/baselines/nano/deployment-20260929`.

## Install/review chain, not an acceptance claim

1. `physical-committed-draft/physical-committed.patch` SHA256 `5b5f577c53c636e57d679e55b348f01b8d2b2e0526951908c1e87ab5648610c6`. Fixed opaque source-bound cap and checkpoint capture/finish split. Six unrun tests. Both independent reviews passed. Needs `replication::physical_source_coordinator` (resync owns final module/durable phase; see their handoff, API matches checked_sequence/capture methods).
2. `physical-certified-draft/physical-certified.patch` SHA256 `503e69c66a68fde0f2904d64079d9074fda6c191a9c92f36c4f91298dc0b76a7`. Mandatory physical wire v2 explicit certified Snapshot/Resume modes/response kinds, no raw downgrade, coordinated factory, cap retained through byte-limited interval, receiver owner+full durable cursor-bound private VerifiedCommittedCut. Seven unrun tests. Both independent reviews passed. Existing CLI compatibility `main-raw-compat.patch` adds require_committed:false; it must remain raw absent complete production source coverage.
3. `physical-serving-draft/physical-serving.patch` SHA256 `f192e7b7de61964afe67127d8df078d7b910280ed3f99a3a34aee1b87ba3cc0c`, 983 lines. New physical_generations module plus private checkpoint copy and receiver worker integration. Four unrun tests. Rustfmt --check passes. Config independently reviewed exact final f192e7b7 and passed the bounded source/API scope, including final accounting fix (physical-serving-independent-review-config.md). Resync passed ownership-complete a9233a59; its final accounting-only delta verdict was pending at freeze. Tests remain uncompiled/unrun. `serving-compat.patch` exports module and adds existing raw CLI serving:None after step2 compatibility patch. Bases are recorded in each draft's source-base.sha256. Patches3 expects patches1+2 already installed, not the original raw file versions.

Serving module final full SHA: `4f29816ad5338b943c61020bb598d0ef665001245a06ebe65ecc1be902ebdcac`.
Other final serving files: physical.rs `eab9379dc849d2993f263468a28e7c5b707b99a451f1e1837d1e24a710893d00`; checkpoint_transfer.rs `46307f48c7207c1f3f59f7988585130a23bcdbc1bae020efacfbb0ecc7e3a920`; physical_receiver.rs `1992c1eef6d7f34bea24315d45688d53596d2d0c681e1b5d04091a2e4041f2a2`.

## Stable serving APIs for PG integration

`ServingGenerations::new(parent:PathBuf,config:Config,limits:TransferLimits,max_resident:usize)->Result<Arc<Self>,String>`; `lease()->Option<ServingLease>`; `last_error()->Option<String>`; `close_and_join()` / `reap_retired()` -> Result<(),String>. ServingLease Clone exposes database()->&EmbeddedDatabase, crate-private database_arc()->Arc<EmbeddedDatabase>, history(), source_sequence(), report(), image_bytes(). PG must retain the lease through all cloned DB/catalog/session use. Config agent owns PG wiring; parent owns engine-bound idle session migration and plan descriptor validation. No PG listener/readiness was added here.

ReceiverOptions adds require_committed:bool and serving:Option<Arc<ServingGenerations>>. Serving images are refused in raw mode. Current main remains false/None via compatibility patches. Source coordinated factory signature is `open_committed_primary_session(db,coordinator,history,parent,limits,request,work,fence_timeout)`; caller must prove every semantic source mutator/error cleanup uses that exact coordinator before exposing it.

## Ownership decisions already reviewed/fixed

- A private cut only mints after certified full barrier == applied durable cursor; status itself is not SQL readiness. Worker checks its UUID and entire current cursor before/after copy. Later apply makes an old cut stale.
- Reservation moves into the copy command, then returns with copied image declared before reservation. Cancellation/unobserved results retain shared capacity through actual file cleanup.
- Raw checkpoint is copied to independent inodes; temporary hardlink lease is removed before worker acknowledges or resumes apply. Existing raw-stage reopen nlink checks are preserved. Clarification: SST link census is at reopen; every-apply verify checks marker/manifest/IDENTITY, not all SST links.
- Hydration keeps PreparedGeneration intact across fallible open so copied files drop before permit release on failure. After open succeeds, sentinel-aware Generation ownership is installed before the final fallible census.
- Atomic same-history increasing-sequence swap under a short lock; old leases remain intact. Capacity counts current, building, old request leases and escaped DB/storage references. Full capacity defers refresh while raw Follow advances.
- Retired objects hold EmbeddedDatabase Arc + raw DB sentinel + TempDir + resident permit. Arc::try_unwrap, not count-then-delete, atomically prevents a Weak upgrade race. Drop DB before directory. If references escape, retain path/capacity; shutdown joins worker and returns explicit pinned-resource error. Unexpected teardown retains private directory rather than deleting behind a live DB.
- Final post-hydration census includes generated marker and RocksDB LOG and checks file count/per-file/total before publish. image_bytes is that publication census, not an OS quota against subsequent diagnostic growth.

## New proof targets, all UNRUN

Certified protocol tests: explicit Snapshot/Resume/no-downgrade; v1 refusal before auth payload/factory; fixed-cap source prefixes while later writes occur; source binding/raw refusal/poison; raw/partial/other-owner cut refusal; actual private TCP snapshot/stream/Resume/Follow; raw source certification refusal before receiver creation.

Serving tests: actual SQL source -> certified TCP -> independent copied strict generations, old/new exact row leases, two-slot backpressure while raw Follow advances, raw reopen while leases live, escaped raw Arc retention and subsequent cleanup; strict malformed-routine hydration refusal leaves old reads and raw progress; atomic Weak retirement; all-file publication census.

## Other independent reviews completed here

- Coordinator source/durable-phase through `edc2b97bcdece484767124ad98c680c77e8e59b1dfbffac99dc398604f57560e`: source-fence-independent-review-transport.md. Later resync changes need their current hash review.
- CREATE TABLE phase patch `4b6446165297d2a5c17c8d08f4117dc5a40b6cb374b9dad9f926fc14cb246564`: ddl-source-admission-independent-review-transport.md. No scoped source/API blocker; tests unrun.
- GH36 row Delete namespace patch `dad4aac39f03e98a82cbd213b559d1ff872a6d7c8b50499254193b399d7dc8ae`: gh36-delete-namespace-independent-review-transport.md. Producer data:-only logical Delete preserves row semantics; owning DDL handles metadata. Tests unrun by this subagent.
- PG standby shape admission final attachment fix `f345e04fa13cb106b112fd76fa1bfbb39e6b72d6dd88e379de6f9beae3e1feda`, harness4a6ebf2...: standby-admission-independent-review-transport.md. SELECT UDF/sequence side effects remain outside that patch.

## Do not close these from source review

Source full mutator/transaction/DDL/background/HTTP/branch/error audit and actual coordinator wiring remain prerequisite to certification. PG per-cycle/transaction leases, session state and backend PID migration, prepared-plan engine identity/result descriptor compatibility, full readonly policy, listener readiness and connection-task joined shutdown remain separate unfinished work. No actual integrated end-to-end serving acceptance or performance gates have run for these drafts.

Legacy logical native replication remains unauthenticated; previous fixtures configured PG password only. The physical token protocol does not fix legacy native authentication. Loopback token transfer still needs an authenticated remote tunnel/TLS path and end-to-end remote acceptance. Raw physical Follow, containment history guard, or a green primitive test cannot close full issue7. Parent's exact current gate logs/results remain authoritative.
