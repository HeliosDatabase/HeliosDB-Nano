> Current preservation/PR handoff: [HANDOFF-TO-NANO-20261001.md](HANDOFF-TO-NANO-20261001.md). Earlier entries below are chronological checkpoints, not current acceptance.

# Nano deployment issue campaign — 2026-09-29

Status: implementation and validation in progress; no issue accepted, merged or released.

Latest checkpoint (2026-09-29 19:39 UTC): earlier library (2744 pass) and targeted
(54 pass) results cover the endpoint/config/auth/offline patch. Enhanced live
reproduction then exposed an additional baseline replay defect. Storage/cache/
applicator repair drafts have now passed independent source reviews, and a fresh
library/targeted/live gate sequence is queued under the fleet build lock. The
older passing tests do not validate the new replay code. Full integration,
feature, static-analysis and performance gates remain pending. Issue 7's full
online resync contract remains incomplete.

Base: `916138311c69f1cde070fb3d53fc601e22cc0a06` (v4.41.0).
Branch: `fix/deployment-issues-20260929`.
Worktree: `/home/gpc/HDB/worktrees/nano-issues-20260929`.
Unchanged comparison: `/home/gpc/HDB/worktrees/nano-deployment-base-20260929`.
Evidence: `/home/gpc/HDB/sprint/baselines/nano/deployment-20260929/`.
The inherited partial `server.rs` edit is saved as `inherited.patch`; the main Nano checkout is untouched.

## Scope and implementation

| Issue | Sprinter | Change | Verification status |
|---|---|---|---|
| 1 | 7540f7a17c2e | Bind PostgreSQL and native replication before readiness; retain sockets; native/PG mismatch diagnostic; node-private replication WAL path; explicit daemon acknowledgment | Pending runtime gates |
| 5 | f170d1796652 | Literal IP or IP:port HTTP address, IPv6, explicit port precedence, port-zero disable | CLI regressions pass; full matrix pending |
| 6 | ec8a50974152 | Supported strict replication TOML section and explicit CLI > file > default resolution preserved through daemon re-exec | Pending runtime gates |
| 7 | b4fafe6875ae | Offline physical snapshot helper into new directory with actual RocksDB lock, file verification and no-replace publication | 11 Python + 2 real Nano snapshot tests pass; online resync remains unsupported |
| 8 | 7dbefa3f5621 | Explicit default-false development trust override, effective AuthManager checks, both constructors and actual prebound listener guard | Pending runtime gates |

The original scripts in the archive are not executed. Fixtures use private temporary directories, ephemeral ports, and owned process IDs.

## Required gate matrix

| Gate | Required evidence | State |
|---|---|---|
| Independent review | Correctness/adversarial and compile/type reviewers; blockers fixed | Source reviews complete; runtime pending |
| Proof-first reproductions | Same CLI regression assertions against unchanged base and candidate | Base: 10 failures, 3 passing controls; candidate queued |
| Focused tests | CLI, replication handshake/listener, config precedence, trust auth, physical reseed | Pass: 54 tests, 0 failures/ignored/filtered |
| Library | `cargo test --lib` | R2 pass: 2744 passed, 0 failed, 2 ignored |
| Default integration | `cargo test --tests --no-fail-fast -- --skip ha_tests::streaming_tests --skip lock_management` | Pending |
| Internal integration | Same with `--features internal-tests` | Pending |
| Documentation | `cargo test --doc` | Pending |
| Empty-suite audit | Explain expected opt-ins and execute applicable suites; no false pass | Pending |
| Formatting/whitespace | Changed-file rustfmt and `git diff --check` | Pass before final freeze |
| Clippy | Same-environment baseline/new-finding comparison; retain raw exit | Pending |
| Dependencies | `cargo deny check` | Pending |
| Feature/MSRV | Applicable HA/no-default compile and declared Rust 1.85 check | Pending |
| Live wire/replication | Real SQL auth, native handshake, streaming row verification, startup failure behavior | Pending |
| Performance | Load-gated PG35 300, startup/wire A/B simple/extended/prepared, public smoke and concurrency; original/latest comparisons | Pending |

All heavy commands acquire `/home/gpc/HDB/sprint/coordination/build.lock` and execute in a `systemd-run --user --scope --collect` with `MemoryMax=24G`, `MemorySwapMax=0`, `CARGO_BUILD_JOBS=2`, and a worktree-specific target directory.

## Issue 7 boundary

There is no established coherent online snapshot/WAL boundary or durable database-history identity. Production storage broadcasts bypass the native catchup store and its LSN counter. A new private WAL path does not fix replay continuity. The offline helper prepares a stopped physical dataset, preserves old state, and does not certify it as a streaming standby. See `docs/guides/offline-reseed.md`; issue 7 remains open until the complete online contract is implemented and tested.

## Review checkpoint

Independent correctness and final type reviews found no remaining blocker/major
in the bounded patch. Earlier review caught and resolved an actual-listener trust
bypass, daemon false readiness from TCP-only probes, uppercase role handling, and
silent directory traversal failures during physical copies. See
`CORRECTNESS-REVIEW.md` and `TYPE-REVIEW-FINAL.md`. TLS boolean CLI options retain
bare-flag compatibility and now accept explicit values emitted by daemon re-exec.
The root reviewed the new process-level CLI regression fixture and cleanup logic.

Python offline snapshot safety suite: 11 passed (raw `offline-python.log`).
Rust production/test inputs are frozen in `candidate-source.sha256` before gates.

## Baseline evidence

- Unchanged production sources built successfully with stable Rust (v4.41.0;
  `base-build.log`, `base-binary.sha256`).
- Declared Rust 1.85 gate **fails on the unchanged base**: rustc 1.85.1 rejects
  locked `home 0.5.12`, `time 0.3.53`, `time-core 0.1.9` and `time-macros 0.2.31`,
  which require Rust 1.88. See `base-msrv.log` and metadata. Manifest and lockfile
  remain unchanged; this is a pre-existing gate limitation, separately tracked.
- Baseline CLI regression target is compiling/running; results pending.

Baseline CLI regressions completed: **3 passed / 10 failed**, cargo exit 101.
All 13 test cases ran, no filtered or ignored cases. Failures cover HTTP socket
literals/explicit port precedence/IPv6, TOML replication activation/validation,
missing trust opt-in in foreground/daemon, replication-bind false readiness, and
an unrelated listener satisfying daemon readiness. Controls for HTTP disable,
default remote-trust refusal, and explicit standalone passed. See `base-cli.log`,
`base-cli-binary.sha256`, and `baseline-repro-source.sha256`.
Candidate validation is queued; none of these failures is yet claimed repaired.
MSRV follow-up item: `e84ae33466f7`.

## Candidate library attempt 1

`candidate-lib.log`: 2743 passed, 1 failed, 2 ignored. The sole failure was the
new prebound-listener guard test constructing `PgServerConfig::default()`
(wildcard trust is correctly refused) before reaching the assertion. The fixture
now explicitly configures loopback. Production code is unchanged by this
correction; the full library and targeted/live stages are queued again under the
shared lock. Attempt 1 input hashes remain in `candidate-source-r1.sha256`.

## Candidate library attempt 2

`candidate-lib-r2.log`: **2744 passed / 0 failed / 2 ignored**, cargo exit 0;
75.16 seconds test runtime. The corrected actual-listener guard case passed.
The frozen candidate is now compiling the targeted CLI/auth/native-listener/
physical-reseed suites; live replication follows only after their success.

## Broad-suite isolation

The existing `mv_j_real_repro` test can mutate the live `/tmp/td-j-work` fixture;
the internal protocol tier has a fixed 15211 port. Full suites will use a private
network namespace and disk-backed private `/tmp` through bubblewrap, inside the
existing fleet flock and bounded systemd scope. Inherited
`HELIOS_CRASH_CHILD_DB_PATH` is cleared. The source audit identifies 50 expected
feature-empty default targets, 35 under internal-tests, with 15 activated internal
targets; actual logs must still be audited. See external
`full-suite-safety-review.md`; no production fixture is reused.

## Targeted candidate results

`candidate-targeted-r2.log`: **54 passed / 0 failed / 0 ignored / 0 filtered**,
including 13/13 deployment CLI cases (baseline 3/13), 2 binary unit cases,
7 auth-wire cases, 26 SCRAM cases, 3 native listener/framing cases,
2 physical-reseed cases, and 1 synchronous replication acknowledgment case.

The first live-harness attempt failed during setup before any server launch:
Python 3.9 lacks `hashlib.file_digest`. Its hashing now uses compatible streaming
SHA256; the three existing mocked harness checks pass. Rust sources are unchanged.
Live attempt 3 is queued in a private network and `/tmp` namespace, with fresh
evidence directory; no live streaming success is claimed yet.

## Live attempt 3: unresolved read visibility failure

The Python compatibility correction allowed a real password-authenticated fixture
to run. Native handshake/registration, schema propagation, explicit committed
rows, primary rollback behavior and wrong-PG-endpoint diagnostics passed.
However the later autocommit row 4 did not appear in the standby's repeated
SELECT. The standby log records its LSN 9 received and applied successfully;
therefore this is not evidence of a dropped transport message.

Raw result: `candidate-live-r3/replication-8ioqpiey/result.json`; cargo/tool
stage `candidate-live-r3` exited 1. Exact unchanged-baseline reproduction and
fresh-query/fresh-connection diagnostic fixtures are queued. A stale result
cache is a source-based hypothesis, not yet a completed diagnosis. The original
failed assertion is retained, and broad gates wait for resolution.


## Baseline replay defect and repair checkpoint

Five forced repeated reads make the live stale-result failure deterministic on
both baseline and the earlier candidate. Fresh full scans find the received row,
while repeated SQL, PK lookup and COUNT disagree. The public actual-SQL-generated
WAL regression target fails 4 cases on baseline with 1 passing transaction-counter
control (`base-replay-visibility.log`). After autocommit replay, a subsequent local
insert overwrites an existing replicated row. This is tracked as critical item
`ca50db01375f`, linked to issue 1.

The new repair updates ART memberships with reversible claims, persists row and
monotonic allocation counter in one RocksDB batch, invalidates row/result caches
with generation stamps that reject delayed stale publication, and stops the WAL
applicator at its first decode/storage error. Ordinary row-store DML is the tested
scope; missing column-storage sidecars are refused. No transaction snapshot,
concurrent local-writer, vector correctness, or restart/resync continuity claim is
made. Existing pause/drop and promotion integration limitations remain explicit.

Independent reviews: external `REPLAY-TYPE-REVIEW.md`,
`live-replay-implementation-review.md`, and `live-replay-independent-review.md`.
The new deterministic live harness retains the original failed assertion and adds
five warm reads plus separate PK/count observations. Five mocked harness tests
pass. Source manifest `candidate-source.sha256` now identifies the new replay
candidate; `candidate-source-pre-replay.sha256` preserves the preceding version.

Queued `replay-gates.sh`: unchanged-baseline applicator proof; candidate library;
CLI/auth/replay/applicator/native-listener/offline/synchronous-HA tests; real native
streaming in private network and /tmp namespaces. Pending results must be read
before accepting any repair or advancing to broader gates.


Baseline applicator proof completed: `base-applicator.log`, exit 101,
**0 passed / 3 failed / 0 ignored / 0 filtered**. Both failed-entry queue closure
cases and the legacy LSN-only bypass regression fail on unchanged production
sources. Before/after source manifests match. The candidate library rerun is
queued; this establishes baseline defects, not candidate acceptance.


## Updated replay candidate library result

`candidate-lib-r3.log`: **2752 passed / 0 failed / 2 ignored / 0 filtered**,
76.04 seconds test runtime after 2m25s compilation. All eight new cache-publication
and ART rollback/idempotency unit cases passed. Source hashes match before and
after; the running scope's 24 GiB/no-swap limits were verified. This validates the
updated library; targeted and live streaming stages are still pending.


## Issue 7 live baseline: mixed histories after primary recreation

The reviewed private experiment completed with all owned children cleaned and
stopped primary A's dataset hashes unchanged. The standby first received A's
rows 101/102. After clean A shutdown, a fresh B dataset reused the same native
endpoint and configured node UUID. B held rows 201..208, but the old standby
accepted its handshake and returned **101,102,203,204,205,206,207,208** from cold
scans and a fresh connection. This proves history mixing on the unchanged base;
it is more serious than an unavailable resync command.

Evidence: external
`issue7-baseline-PJwPFRLi/primary-replacement-l0y4fvdi/result.json`, both primary
logs and standby log. `safe_harness_completion=true` certifies only fixture
completion/preservation; `resync_certified=false`. Candidate observation remains
pending. Durable history refusal containment is being assessed; no such guard is
implemented yet. Issue 7 remains open with raised impact/urgency.


## History containment implementation checkpoint (2026-09-29T20:34:42.288401+00:00)

The isolated `nano-history-guard-20260929` worktree now contains a proposed mandatory native-v2 history guard. Primary identity is independent of node UUID and bound to RocksDB IDENTITY; receivers bind durably before readiness/WAL and reject replacement history. Existing unbound receiver paths refuse before database open. CLI startup/supervision propagates terminal failure; transparent SQL forwarding is disabled, guarded HTTP is health/version only, and MySQL listeners are refused. These compatibility restrictions and limits are documented in the guard worktree. This is source implementation under independent review, not runtime acceptance, and does not implement online resync. The running replay candidate remains untouched.

Additional independently reviewed replay storage-boundary/publication tests are installed in baseline and guard; unchanged-baseline run is queued. Full gates and performance remain pending. All five originally dirty tracked main-checkout file hashes still match the initial preservation snapshot.


## Targeted replay candidate and boundary baseline results

`candidate-targeted-r3.log`: **109 passed / 0 failed / 0 ignored / 0 filtered** across CLI, wire authentication, replay visibility, HA integration, offline reseed, native startup, sync and fail-closed applicator tests. Source hashes match before/after. Live warm streaming remains queued.

`base-replay-boundaries.log`: **1 passed / 4 failed**. SQL predicate integrity passes; raw storage PK identity fails separately. The base also accepts malformed/conflicting replay records and permits a stale counter reduction in the encrypted replay test. Earlier data sealing assertions passed: this is not evidence of an encryption leak. The guard candidate is now separately frozen with source and independent review records; library/targeted runs are queued under the fleet lock. No guard runtime acceptance yet.


## Online resync implementation checkpoint (2026-09-29T21:23:39.704784+00:00)

A separate `nano-resync-20260929` worktree now contains physical replication staging primitives: bounded validated RocksDB batches, checksums and history identity, contiguous source polling after a durable barrier, and atomic imported data plus source/local progress. Resume rejects unrelated local modifications; overflow, gap, conflicting duplicate and uncertain-write paths refuse. Two independent source reviews completed. Ten unit tests plus six independent checkpoint/continuation tests are installed, frozen, and queued; no runtime acceptance yet.

Authenticated transfer, checksummed checkpoint installation and strict snapshot hydration are being drafted externally while validation snapshots remain frozen. These are prerequisites for full online resync, which is still incomplete. No serving/publication, cross-history recovery or universal feature claim follows from these primitives. Main dirty files were rechecked and all five preserved hashes match.


## Live replay candidate passes (2026-09-29T21:25:16Z)

`candidate-live-r4` completed exit 0 on binary SHA256 `f213bf31c31a684c1c31b5356dc571460cd9195be1588f7fc33e6ca5320ed385`. All 11 checks pass: password-authenticated PostgreSQL access plus unauthenticated native registration and streaming, schema propagation, exact committed rows, rollback exclusion after a later commit, five pre-barrier cache warmups followed by repeated correct three-row reads, primary-key lookup and COUNT=3, plus actionable rejection of PostgreSQL framing on the native endpoint. Source manifests match before/after (853 inputs). Owned fixture children are absent in the post-run process audit.

This clears the previously failing bounded warm-replay fixture on the original replay candidate. It does not validate the later history guard or online resync. The history-guard library build is now active in run-u713.scope, with 24 GiB/no-swap limits verified; broader gates and performance remain outstanding.


## History guard library result (2026-09-29T21:29:55Z)

`guard-lib-r1` passes **2768 / 0 failed / 2 ignored / 0 filtered**, 75.77 seconds test runtime. This includes all 13 history metadata tests and the basic native server mismatch test. Source checks match before/after (867 inputs); run-u713.scope enforces 24 GiB/no swap. CLI/transport/replay boundary integration target is compiling next in the same locked scope. This is the first guard runtime evidence, not complete guard acceptance or online resync.


## Raw PK repair and next gates (2026-09-29T21:42:27.040518+00:00)
Guard-targeted-r1 completed 128 pass / 1 fail, sole failure raw storage primary-key identity under controlled replay publication. SQL predicate identity passes. Reviewed patch8eb05581 installed in guard plus6warm/cold true-owner tests; initial rustfmt check found formatting-only draft drift, formatted test then check passed. Guard source frozen868inputs. Queued guard-pk-gates.sh session71042 library-r2 then targeted-r2. Prior binary preserved binaries/guard-r1-debug withsha sidecar before edit; replacement-history live fixture queued57445 guard-r1 same explicitnodeUUID. Prior18393 confirmed terminal101. Baserelease98677, physical69532, baselinewire12468 confirmedlive viahandles. Original/guard-r1 manifests retained. Physical tree remains frozen unchanged; its dependency cachecopy serializes underfleetlock so futureguardbuild cannotrace.
Sprinter rawPK note manual21:50 label was clock typo; actual update ~21:41UTC. No claim that source review clears runtime gates.
Proxy issue2 source check: RoleManager::promote_to_primary only roletransition; WalApplicator::promote stops stream/marksDisconnected. No server/main/protocol wiring found for distributedfencedpromotion. Newhistoryguard refusesimplicit boundstandby promotion; resync preservesstandby role, no automaticpromotion planned.


## Physical primitive evidence and artifact correction (2026-09-29T21:57:54.441612+00:00)
Session69532 completedexit0; physical-primitives-r1 PASS6/0 withactual module imports/new testtarget. However physical-lib-r1 is INVALID acceptance evidence: reported2768/0/2ignored, missingall10newphysicalunitcases. ExactbinarySHA68cacaa3d78ef95af426d4d6ddc1cc762923ac7629f06bb0e85c1150ba327b9a equalscopiedguardbinary. Cargo reusedoldcrate-testfingerprint aftercachecopy withnewersourcedatesolderthanartifact. Evidence physical-lib-r1-artifact-audit.json. Force cargo clean -p heliosdb-nano underfleetlock withphysicaluniqueTARGET before nextlib; neverrelyon copiedcratefingerprints. Physicaltree NOWUNFROZEN for reviewedintegration, noedit yet. Allfuturecachecopies mustinvalidate copiedlocalcrateartifacts, retainonlydependencybenefit.
Baserelease98677 completedexit0 at21:53:21; base-release-executables.json recordsactualCargoexecutable/SHA. BaselinePG35run queuedwith300iterations/ownedcontainer andloadchecks, notmeasuredyet.
Baselinewire12468 terminalexit1. Exactresult standby-wire-baseline-7RPj5RlJ/standby-wire-writes-p646sh0r/result.json: COPYprovenlocalwrite (source1,2;standby1,2,99 all8samples), extendedsuccessandlocal99butcontrol2missing=>strictfixtureverdictunexpected_rows_inconclusive. Binary/scriptidentitiesunchanged. Guard-r1 identicalfixturequeued separately; nofixedclaim.
Guardbroadsession41172 waitsOUTSIDEfleetlock for finalguard-targeted-r2 exit0 thenfull default/internal/no-fail-fast+completeness/doc/features/clippy/deny; nonzero prerequisite exitswithoutlaunching. Guard/base remainfrozen whilejobsqueued. Guardreplacement57445 andPK71042 stillqueued/live.
Rootstrictlazycataloglatestaf84abe...305lines validatesview/MV/sequence metadataexactdecoding, fixeslegacyviewfallbacksilentbindingloss, tests positivecurrent/legacy and corrupttrailingplusrawpreservation; remains external/uncompiled. Source-consistency audit physical-source-committed-boundary-audit.md identifiesmultibatch SQLvisibility/DDL gaps requiringallmutator admission+certifiedcommittedrawbarrier beforeSQLserving. RawFollowCLI/receiverdrafts explicitlyNON-SERVINGuntilthatworkfinishes.


## Integrated raw resync source freeze (2026-09-29T22:10:25.833231+00:00)
Root installed reviewed checkpoint338bdd/wire7d7f5a75/sourcefb6e4ffa/receivercd5ed9ba, CLIconfigb0c5590d, strictstorage3daa074/libdf5d/lazyaf84, boundedretentionebc6e9, cursorgetter4aaecb32, rawPK8eb05581 +regressions into nano-resync-20260929. Physicalsource nowFROZEN876inputs undernew session59919 physical-integrated-gates.sh. Formatting changed/newRust andlockedoffline metadata PASS; directsubtle dependency usesexistinglockedversion. physical-integration-installed.json/metadata/prepatch/manifests retainprovenance. Fullfreshcrateclean underfleetlock first, assertoldcopiedunitartifactgone, fulllib then source-name newmodule testcoverageaudit, then CLI/physicalstrict/replay/historytargets nofailfast. Unitcoverage verifierprevents priorstale-library acceptance. No newruntimepassyet.
Other newlyqueued handles:37902 basePG35r1 (releaseCargoJSONtargetSHAe6ec4439);51273 guard-r1standbywire. Guardbroadsession41172 waits targetedr2success. Main5dirtytrackedhashesallmatch preserved-main-integrated-checkpoint.json.
Allagentsnowexternalonly followon: resync sourceoperationcoordinator+engineAPIs; transport committedsourcecap/checkpointcapture split; config readonlystandby PGCOPY/extendedadmission. Rootexternal library-source-admission.patch73 synchronousentrypoints prepared, uncompiled andincompleteuntilalllower/backgroundwriters anderrorboundarieswired. No frozen sourceedits.


## Guard live evidence and source admission follow-on (2026-09-29T22:25:00.585831+00:00)
Guard-lib-r2 PASS2768/0/2ignored, targeted-r2 PASS135/0, all868 source checks match. Full guard broad pipeline41172 started run-u727.scope; guard/base stay frozen. Replacement57445 complete: same endpoint/same nodeUUID freshhistory causes terminal typed mismatch and receiver exit1/listener closure. All14 evaluatorchecksPASS issue7-guard-r1-containment-verdict.json, noresynccertification. Guard-r1 binarySHAc095536d0d92b71436c488ed8ced20d65bafe348b12aa4ae4a405b49e13ba7ee. Guardstandbyprobe51273 complete: BOTHextendedINSERT andCOPYFROM localmutationPROVEN source1,2 vsstandby1,2,99 all8samples each. Externaladmissionrepairf345e04/harness4a6e reviewpendinglatestdocattachmentdelta; no frozenedits. guard-live-cleanup-audit.json confirms no exactfixturechildren forreplacement/baselinewire/guardwire.
Physical59919 terminal2 after successfulcrateclean: Cargo alphabetized manualsubtle rootdependency; verified reconstruction exactlymatchesoldmanifest, nootherlockchange. priorphysical-source-pre-lock-order.sha256 preserved; correctedmanifest876 andclean--locked, newphysical-integratedretry16343 queued(frozen). BaselinePG35r1 terminal1 beforetests Dockerexit125, ownedcontainerremoved; exceptionloststderr, helpernowretainsit, unchangedtestbinaryPG35r2retry82557queued.
Externalfollowingdrafts: coordinator946a5c9/cbb5cfe4 engine + committedpatch5b5f577c reviewed; rootlibrary73+txn+GC/MV errorconversions corrected(mapError::storage), notinstalled. WALgroupcommit source audit foundfailedencoder earlynotifyfollowedbywrite/broadcast; externalall-or-nothingencodingpatch42a5987 +2tests drafted, resyncreviewrequested. Agents externalonly: resyncactualdaemon mutatorcoverage/HTTP/Branchuncertainphases; transportcertified wire/source/receiver mode; configrawresync CLIprocessharness+ROinspector. SourceSQLcertification and servinggeneration remainunfinished. No automaticpromotioncontractfound/planned inresync; RoleManager458/WalApplicator355 arepartialcomponentsonly.


## Native authentication requirement correction (2026-09-29T22:45:01.855606+00:00)
Proxy independently audited the original replay candidate, bound its source/harness to the recorded manifests, and correctly identified an inaccurate authentication claim above. The original live-r4 result remains 11/11 passing on binary f213bf31c31a684c1c31b5356dc571460cd9195be1588f7fc33e6ca5320ed385; JSON f24e4b79888954db362ee508cb1d7f17b33008bee8d17a359328d0a68d03b48f is unchanged. Its password authenticates PostgreSQL SQL clients only. Native HandshakeRequest has metadata but no credential/proof, and that server path accepts registration without credential validation. Native connection/streaming is demonstrated; native credential authentication is an unmet issue1 subrequirement. Dataset-history verification in the later guard is a separate identity check and does not authenticate the peer.
Review: /home/gpc/HDB/Proxy/docs/internal/deployment-issues-2026-09-29/evidence/issue1-native-auth-requirement-review.json. Pre-correction report/state copies retained in evidence. No gate was edited, diverted or rescheduled for this correction.
The separate physical resync endpoint draft authenticates a dedicated 32-byte token before provider/checkpoint access and is default-off/loopback-only; its pending positive/wrong/missing-token tests will establish only that physical endpoint's contract if they pass. It does not repair or validate legacy logical-native authentication. Assess a separate explicit native credential contract with server-side rejection before registration/WAL and positive/wrong/missing credential controls; preserve distinct protocol/candidate/gate identities. No current source or runtime acceptance claim for native authentication.


## One-hour priority and bounded admission candidate (2026-09-29 22:57 UTC)

User deadline is 23:43:44 UTC: close only with evidence or hand off to OpenCode DeepSeek V4.1 Flash. New isolated nano-admission-20260929 freezes868inputs from guard plus reviewed SQL-shape admission f345e04 and logical Delete row-namespace producer fix dad4aac. Changed Rust formatting and git diff check pass. Session71865 queues admission-gates-r1.sh under fleet lock/24GiB/no-swap: clean copied local crate, full library+expected-test audit, GH36/PG integration, then owned live simple/COPY/extended protocol refusal and exact-row matrix. No runtime pass yet; SELECT/UDF/sequence effects remain outside this bounded admission repair.

Guard broad default found real a_replica_follows_alter_table_rename_to failure (35pass1fail GH36 target); later targets continue no-fail-fast. Metadata table_constraints:t deletion incorrectly appeared as logical row Delete. New candidate fixes producer, retains existing failing regression, adds two controls; guard frozen source remains unchanged. Existing41172broad,16343physical,82557PG35 jobs preserved. Main five tracked dirty hashes still match; waiter inherits no known HELIOS benchmark opt-ins/TMPDIR (deadline-preservation-environment-audit.json).

OPENCODE-NANO-HANDOFF.md records exact job identities, source snapshots, accepted observations and all unfinished resync/auth/promotion/gates. OpenCode model deepseek/deepseek-flash launched22:56:55UTC PID1235392, initial read-only acknowledgment requested; ownership transfer pending explicit marker. No issue marked fixed/done and no commit/merge/release. Root Proxy controls separate fallback.


## Proxy short-job scheduling override (22:58 UTC)
User requested MD5capability/4ABBA66204 and baselineWASM66546 before another long Nano compile, without interrupting active tests. Active guard-default run-u727.scope/flock173759 remains untouched. Three Nano waiting-only flocks had no children and wchan=locks_lock_inode_wait; their waiters were terminated and exact commands preserved/requeued through outside-lock continuation scripts, waiting for existing Proxy waiter identities964250 and994778 to finish. This is deliberate scheduling, not a gate failure or canceled implementation. Old handles16343/82557/71865 can return143/TERM; actual continuation state is priority-reschedule-{0,1,2}.json, PIDs1285414/1285415/1285416. No source/script/test was changed, no active job interrupted. Logs and candidate snapshots retained. Monitor continuation result keys, not old terminal exit.


## Ownership transfer completed (2026-09-29T23:01:41.558840+00:00)
OpenCode DeepSeek V4.1 Flash accepted the Nano handoff in OPENCODE-ACK.md. Active session ses_f109d4f50ffe3UXlZpZ5C3Rg7y, PID1328546, model deepseek/deepseek-flash, event log opencode-nano-events-r2.jsonl. OPENCODE-OWNERSHIP-TRANSFER.json authorizes continued autonomous Nano work; frontier source work stops. Initial OpenCode launch stalled snapshotting its large untracked build directory, so only its owned snapshot subprocess/session were stopped and resumed with snapshot:false (verified config), preserving all campaign jobs.

Final root gate observation: guard-default completed23:00:25UTC exit101, all353targets started/summarized;6698pass,1fail,46ignored,16filtered. Expected empty-suite set matches; completeness checker fails only tests/gh_issue_36.rs. Sourcebefore/after match. Reviewed row-namespace fix is in the separately frozen admission candidate, still gated pending. First queued Proxy shortjob acquiredlock immediately afterward, respecting user's scheduling priority. Three Nano pending command continuations retain exact commands and source snapshots. No issue closed, merged or released.
