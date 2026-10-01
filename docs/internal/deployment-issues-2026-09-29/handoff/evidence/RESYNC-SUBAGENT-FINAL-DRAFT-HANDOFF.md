# Resync subagent: external draft handoff

This file inventories work and acceptance gaps for the parent campaign handoff. It is not a completion record. Frozen campaign/guard/physical sources were not edited by this subagent during this follow-on draft phase. No heavy builds or runtime gates were run by this subagent; root owns host-lock execution and exact results.

## Stable external implementations

| Artifact | SHA256 | Status |
|---|---|---|
| source-fence-draft/physical_source_coordinator.rs | edc2b97bcdece484767124ad98c680c77e8e59b1dfbffac99dc398604f57560e | Transport reviewed; 11 authored unrun tests |
| source-fence-draft/storage-source-admission.patch | cbb5cfe4518dbb65b0f4f3a604ebaaa22ce9cc335abcfdb6926d756d154eb779 | Exact optional engine field/construction/transaction/GC propagation |
| source-fence-draft/storage-durable-phase-helper.patch | fb909db1a62b8b02f930421cf1477c0db3845bd073e6fffa80db8657d90f0528 | After engine admission; no-HA/inactive phase wrapper |
| remaining-source-admission-draft/http-branch-source-admission.patch | c117bb16cc4b26cd300abc2f6ac0554fd5752fc7d11281f7ea76df21612ae05d | Config review passed; four authored unrun regressions |
| remaining-source-admission-draft/branch-engine-propagation.patch | 526f8b440d5c74b99e51419cbd8f34538634edabcd7928125d2553227c5fd1ab | After engine admission; config review passed |
| snapshot-source-admission.patch | f8a0ad0645ab746a8e5813902aa00f36ac1237620f2461462fa3fd86e65146f0 | Config review passed; three authored unrun tests |
| snapshot-engine-propagation.patch | e48a261c8b9cd10158662a971626a13bb2aab32218a650975fdbe26fb291fc75 | After engine admission; constructor-only optional manager coordinator |

Full files and patch-order notes are in source-fence-draft/, remaining-source-admission-draft/ and snapshot-source-admission-draft/. The production snapshot generator requires appending tests.rs before final rustfmt; do not blindly rerun older HTTP generator over final artifacts. Parent composition may adjust hunk context but must preserve final semantics and record resulting hashes.

## Reviewed peer artifacts

Exact-hash source/API verdicts are recorded in:

- physical-committed-independent-review-resync.md: opaque source-bound cap and capture/hash split.
- physical-certified-independent-review-resync.md: mandatory certified wire modes, fixed cap through partial polls, durable owner/cursor-bound receiver capability. This review explicitly depends on complete managed-source admission.
- physical-receiver-independent-review-resync.md: joined raw worker and Resume ownership.
- library-source-admission-independent-review-resync.md: library synchronous entry and revised transaction phase/drop ordering.
- wal-group-admission-independent-review-resync.md: all encoding before batch I/O; publication before any waiter completion; no worker-side coordinator deadlock.
- fast-dml-source-phase-independent-review-resync.md: engine phase coverage and follow-up lazy columnar presence fix.
- generation-session-transfer-independent-review-resync.md: exclusively owned idle session transfer only; PG callers must prove all protocol state conditions.
- cli-resync-regression-independent-review-resync.md: private raw-only CLI runtime harness; census error suppression fixed.
- gh36-delete-namespace-independent-review-resync.md: main data namespace only in logical Delete producer; actual deletion unchanged.

Serving generations final patch a9233a5999e6d54f74eafdf6192ee79f86ecaa34fd60aa979ae5b2d51c0a8d16 passed scoped source/API review (physical-serving-independent-review-resync.md). Review found and author fixed raw Weak::upgrade retirement race using atomic Arc::try_unwrap, capacity lifetime across canceled copy commands, and failed-hydration image-before-permit cleanup ordering. Three authored tests remain uncompiled/unrun. No serving/readiness acceptance follows from this review.

## Unaccepted end-state gaps

Issue #7 is not complete merely because raw snapshots/Resume, history containment, or strict standalone hydration succeed. Remaining requirements include:

1. Integrate all source/certified/serving/session drafts into a recorded isolated candidate; compile all affected feature combinations and run targeted regressions. External rustfmt/source reviews are not compile or runtime evidence.
2. Complete managed daemon PG/MySQL/HTTP/background mutation/error boundary coverage. Ordinary validation errors must preserve primary availability; uncertain partial durable semantic errors revoke only certification. Public Arc<DB> escape and independently built managers are unsupported bypasses, not automatically protected. Root is refusing lock-free ingestion while physical source is enabled; unrelated experimental adapters are outside daemon scope.
3. Verify source snapshot and each serving publication are complete committed semantic cuts, including multi-batch transactions, DDL, counters, snapshot/GC metadata and side storage. Raw RocksDB consistency alone is insufficient.
4. Wire copied immutable generations into actual protocol requests with bounded leases/retirement, idle session migration, read-only write admission, authentication/role preservation, and truthful readiness. No serving listener is established by the low-level generation factory alone. Never silently downgrade to raw/legacy mode.
5. Implement and test the requested same/new-primary resync and promotion behavior without overwriting/rebinding unknown histories or deleting old user directories. Current native history mismatch guard is containment, not full replacement-primary online resync.
6. Run complete mandatory gates, exact binary/runtime campaigns, full feature tests with documented skips only, concurrency/negative cases and performance comparisons under the shared host build lock. Root's campaign evidence controls what actually ran; this packet claims no gate acceptance.

Useful diagnostic/design anchors: physical-source-committed-boundary-audit.md and physical-source-mutation-coverage-matrix.md. Earlier original-primary replacement experiment proved mixed histories on baseline; containment and full online continuity are separate acceptance outcomes.
