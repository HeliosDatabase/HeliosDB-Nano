# Nano pending status under OpenCode ownership

- Owner: `deepseek/deepseek-flash` (opencode), session `ses_f109d4f50ffe3UXlZpZ5C3Rg7y`, pid 1328546.
- Scope: Nano issues 1, 5, 6, 7, 8. Proxy root owns 2, 3, 4, 9, 10.
- Ownership marker: `OPENCODE-OWNERSHIP-TRANSFER.json` (utc 23:01:41Z), `root_source_work_stopped=true`,
  `all_issues_still_in_progress=true`, `continue_after_frontier_deadline=true`.
- This is a status snapshot, NOT closure. No item is done/fixed/merged/released.

## Frontier deadline

- User frontier deadline 2026-09-29T23:43:44Z bounds "fix or hand off"; successor is authorized to continue the
  campaign afterward. If closure is not reached, the pending status below stands.

## Accepted evidence already on disk (unchanged by me)

- Original replay candidate live: `candidate-live-r4/replication-_l38ze_7/result.json`
  SHA `f24e4b79888954db362ee508cb1d7f17b33008bee8d17a359328d0a68d03b48f`, 11 checks pass; binary
  `f213bf31c31a684c1c31b5356dc571460cd9195be1588f7fc33e6ca5320ed385`. Password auth is PostgreSQL-only;
  native registration/WAL is UNAUTHENTICATED.
- Guard default (final): 353/353 targets summarized, **6698 pass / 1 fail / 46 ignored / 16 filtered**,
  sole retained GH36 `a_replica_follows_alter_table_rename_to` failure; source before/after match.
- Guard lib-r2 2768/0/2ignored; guard targeted-r2 135/0. Guard containment only (`resync_certified=false`).
- physical-lib-r1 2768-pass result is INVALID (stale copied test binary; 10 new cases missing) —
  do not cite as acceptance.
- Admission candidate `nano-admission-20260929` freeze integrity re-verified by me: `sha256sum -c
  admission-source.sha256` exit=0, 0 FAILED, 868 inputs; `target-campaign` absent (bootstrap precondition).

## Pending / unaccepted (must not be narrowed)

1. **Bounded admission candidate runtime gates UNRUN** (queued `priority-reschedule-2.json` →
   `admission-gates-r1.sh`): frozen lib/targeted/live admission matrix + coverage audit + GH36/PG suites.
   SQL-shape admission still does NOT reject all SELECT/UDF/sequence effects.
2. **Native logical authentication MISSING**: explicit credentials rejected BEFORE registration/WAL; tests for
   valid/wrong/missing; preserve endpoint/history distinction. Physical endpoint token does not retrofit this.
3. **Online resync / coherent serving incomplete**: resync_certified=false; EXTERNAL serving/
   pg-generation drafts NOT installed/compiled.
4. **No safely fenced promotion contract**; never route writes to a read-only standby.
5. **Remaining source certification**: ALTER/DROP/index/routines/views/MV/sequence + VersionGC partial-error.
6. **Full campaign matrix/perf**: broad default/internal/doc/features/clippy/deny/MSRV, PG35 300-iter,
   simple/extended/prepared × clients 1/8/16/32/64, public smoke N1000M200, concurrency/lost-updates,
   replaybench, independent reviews.

## Live queue state (observed 23:06–23:10Z, read-only)

- Fleet lock held by `guard-internal` (guard-broad parent 3621374); compiling test binaries.
- Waiting flock children: 1343365 physical-integrated-r3, 1343367 base-pg35-r2, 1343368 admission-r1.
- Proxy short jobs 964250/994778. No queued job duplicated/reordered/cancelled/edited by me.
- Env hygiene of admission continuation confirmed: no HELIOS opt-ins, no TMPDIR; `CARGO_BUILD_JOBS` overridden
  to 2 inside `run-admission-gate.sh`.

## Concrete next steps (in priority order)

1. Let the queue drain; when `admission` lock child (1343368) runs, verify `admission-*-source-before/after.log`
   hashes, `admission-lib-r1-coverage.json` both classifier tests, targeted/live summaries and identity files
   from evidence — not exit code alone. `set -e` means an early failure leaves later stages UNRUN (record as
   incomplete, not passed).
2. If admission passes, capture it as bounded repair evidence only; do NOT transfer guard/physical results to it.
3. Implement native credential authentication (reject before registration/WAL) in a NEW isolated worktree;
   test valid/wrong/missing. Compile/gate only under the fleet lock.
4. Continue resync/serving/fenced-promotion implementation as bounded drafts; certify only with runtime gates.
5. Append per-issue Sprinter notes as each result lands, append-only; keep status in_progress.

## Update 2026-09-30T00:20Z (supersedes the queue state above)

- **Admission bounded repair now PASSES** on the corrected candidate
  `/home/gpc/HDB/worktrees/nano-admission-fix-20260929` (frozen admission tree + two test-only fixes):
  admission-clean-r2 exit0, admission-lib-r2 2770/0/2ignored, coverage true, admission-targeted-r2 all 9
  targets ok, admission-live-r2 simple/copy/extended admission matrix pass; source hashes matched throughout.
  See `OPENCODE-RESULTS-admission-r2.md`. Delta vs frozen r1 = two test files only.
- Physical integrated candidate `nano-resync-20260929` does NOT compile (physical.rs:542 E0308,
  physical_receiver.rs:1111 E0597). External fix `physical-compile-fix-draft` applied to a NEW isolated
  `nano-resync-fix-20260929`; re-gate launched (crate-clean-r4/lib-r3/targeted-r3, waiter PID 3516085,
  `physical-r4-dispatch.log`), queued behind guard-clippy/deny.
- base-pg35 blocked by host Docker DNAT/iptables (`exit=125`), no timing.
- Broad so far: default FAIL(GH36), internal FAIL(GH36 only), doc PASS, mcp-bin PASS, noHA PASS,
  ha-full PASS, msrv exit101 (declared review), clippy+deny pending.
- Still open: native logical auth, online resync/serving, fenced promotion, residual guard GH36,
  lint/MSRV/deny, performance matrix. No commit/merge/push/release; all issues in_progress.

## Update 2026-09-30T01:05Z

- **Physical integrated re-gate now PASSES** at lib+targeted on the corrected snapshot
  `/home/gpc/HDB/worktrees/nano-resync-fix-20260929`: physical-lib-r6 2825/0/2ignored (coverage expected 55,
  missing []), physical-targeted-r6 all 9 targets ok (65 passed/0 failed), source hashes matched.
  The frozen physical candidate was un-compilable; 5 fixes (see `OPENCODE-RESULTS-physical.md`,
  patch SHA `eb663978...`). Scope: integrated LIBRARY+TARGETED only — NOT online serving/resync;
  `resync_certified=false`; no fenced promotion.
- Guard broad is COMPLETE: failures=4 (default+internal + both completeness, all the same GH36 test),
  pending_reviews=3 (msrv + base/candidate clippy baseline comparisons), passed doc/mcp-bin/noHA/ha-full/deny.
- Two new isolated fix trees exist and are NOT frozen/adopted:
  `nano-admission-fix-20260929` and `nano-resync-fix-20260929`. Each differs from its frozen original only in
  the documented fix files; adopting either requires publishing a new manifest and re-running wider gates.
- Fleet lock FREE at 01:05Z; no campaign heavy jobs running; no queued job canceled or duplicated.
- Concrete next steps: (a) adopt the two fix trees via new frozen manifests and run the wider campaign on
  them; (b) port the GH36 `dad4aac` producer fix into the guard candidate if containment must pass default/
  internal; (c) implement native logical authentication (reject before registration/WAL; valid/wrong/missing);
  (d) install + gate the non-serving serving/generation draft and the fenced-promotion contract;
  (e) unblock host Docker DNAT for base-pg35 300-iter timing; (f) full performance matrix.

## Final consolidated status — 2026-09-30T13:05Z

Ownership unchanged (`deepseek/deepseek-flash`, session `ses_f109d4f50ffe3UXlZpZ5C3Rg7y`). Scope Nano 1/5/6/7/8.
No commit/merge/push/release; frozen originals untouched; no production data/services touched; no host
firewall/Docker global state changed; all heavy work under `flock build.lock` + systemd 24G/no-swap/jobs2.

### Done and runtime-verified this session

1. **Admission bounded repair (issues 7/1 admission subreq) — CORRECTED and FULLY GREEN.**
   - Reassessed the v1 test fixes and rejected them (per user): the gh36 test now stays **AUTOCOMMIT** under
     strict logical WAL (`emitted_logical`, `logical_wal_per_statement=true`) instead of an explicit
     transaction; the CLI test asserts the shared `25006` + `history-guarded standby` contract.
   - Tree `nano-admission-fix-20260929`; manifest `admission-fix-source.sha256` (869 files, includes a copied
     `scripts/nano-offline-reseed.py` that the candidate was missing).
   - Bounded gate r3: lib 2770/0/2; coverage true; targeted all 9 targets ok; live simple/copy/extended pass.
   - Wider gates r2: **default 353/353, 0 failures, completeness pass; internal 353/353, 0 failures,
     completeness pass**; doc/mcp-bin/noHA/ha-full/deny pass; msrv declared preexisting; clippy 0 new vs the
     guard candidate (+2 vs base, inherited). See `OPENCODE-RESULTS-admission-r2.md`, `admission-wide-*.log`.

2. **Native logical credential validation (issue 1) — IMPLEMENTED and verified.**
   - Tree `nano-native-auth-20260930`; manifest `native-auth-source.sha256` (853). `ReplicationAuth::
     SharedSecret` + `HandshakeRequest.auth`; server rejects missing/wrong **before** registration/WAL
     (constant-time); client presents; `ReplicationConfig.auth_token` + `HELIOSDB_REPLICATION_AUTH_TOKEN`;
     `StreamingServer::connected_standby_count()`. Back-compat: no token → legacy handshake.
   - Rust: lib 2752/0/2, `replication_listener_startup` 3/0, `ha_integration` 47/0, `native_replication_auth`
     4/0. CLI live: valid token replicates a committed row; missing/wrong rejected, standby never connects;
     identities unchanged. Wider: default 350/350 & internal 350/350, only the pre-existing GH36 fails.
   - CAVEAT: the native channel is plaintext; this is a network-path credential, not transport encryption.

3. **Physical/resync integrated candidate (issue 7) — COMPILES and passes its gates.**
   - Frozen `nano-resync-20260929` was un-compilable; fixed in `nano-resync-fix-20260929` (manifest
     `physical-fix-source.sha256`, 876). Compile fixes: `physical.rs:542` E0308, `physical_receiver.rs:1111`
     E0597, `lib.rs:10473` E0599 (`list_all_triggers().len()`), test public-reexport fix, and the strict
     `open_physical_snapshot` no-create guard (`engine.rs`). Then ported the reviewed GH36
     `dad4aac` row-Delete namespace fix (see `physical-gh36-port-draft`).
   - Results: lib 2825/0/2 (coverage 55/55); targeted 9 targets ok; GH36 36/0; wider default/internal
     otherwise pass. **This is integrated LIBRARY+TARGETED only — NOT online serving/resync.**

4. **PG35 performance gate unblocked without DNAT.** New `run-pg35-hostnet.py` (owned `--network host`
   container, `-c port=<free loopback>`, unique name + label, removed after). `base-pg35-hostnet exit=0`:
   35 categories × 300 iterations, **Nano 34 / PG 1 / ties 0 / N/A 0**, load<6, container removed.

### Genuinely unfinished (external/dependency-blocked; do NOT claim)

- **Online resync / coherent serving generation**: `resync_certified=false`. The serving chain
  (`physical-committed` → `physical-certified` → `physical-serving` + compat patches) is a multi-draft
  assembly that explicitly depends on the resync-owned `physical_source_coordinator` module/durable phase and
  a full source mutator/transaction/DDL/background audit that are prerequisites; patches expect the prior
  steps installed. Not safely installable in this session. Exact next step: install the coordinator +
  committed + certified + serving chain into a new isolated tree in that order, compile, then wire PG
  generation leases/session migration/readiness, and gate with real TCP serving tests.
- **Fenced promotion**: no safely fenced standby→primary API/contract exists; `RoleManager`/`WalApplicator`
  only change local role/stream. Left UNMET per instruction (never route writes to a read-only standby).
- **Remaining source certification**: ALTER/DROP/index/routines/views/MV/sequence + VersionGC partial-error
  coherent-cut audit; operation admission alone is not proof.
- **Broader matrix**: candidate clippy baseline review (physical +27 new from new modules; native +1;
  admission 0 new); MSRV declared preexisting failure; full performance matrix beyond PG35 baseline.

### Terminal reason if stopped here

All work in flight completed; the remaining items are blocked on prerequisite modules/audits (serving
coordinator chain) or are explicitly retained as unmet (fenced promotion) rather than risked as
unverified partial changes. Reports (`OPENCODE-PROGRESS.md`, `OPENCODE-RESULTS-*.md`,
`OPENCODE-RESULTS-physical.md`, `OPENCODE-PENDING-STATUS.md`) are current.
