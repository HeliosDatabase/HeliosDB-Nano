# Nano deployment campaign handoff — 2026-10-01

User requested a commit, draft PR and ownership transfer to tmux `nano:0.0` (Claude Opus 5.5). This is a preservation/review checkpoint, not merge or release acceptance. Scope: Nano archive issues **1,5,6,7,8**; Proxy work stays with `hproxy`.

## What is committed and what is separate

Implementation checkpoint commit: `2f161df`. Archive source: `/home/gpc/HDB/Proxy/heliosdb-issues.tar.gz`, SHA256 `399391dc4de895c32e49b3964ad8f5c087d0f981f61c48353ebf6ac5746168e9`; extracted issue descriptions at `/tmp/proxy-issues-20260929/heliosdb-issues/ISSUES.md`. Do not run the supplied destructive repro script.

The production changes in this branch are the original deployment/replay candidate from `/home/gpc/HDB/worktrees/nano-issues-20260929`, based on `916138311c69f1cde070fb3d53fc601e22cc0a06`. They cover endpoint/configuration handling, explicit development trust override, fail-fast replication startup, replay/cache correctness, and the offline reseed helper.

Later independently gated candidates are preserved as **alternative full patches from that same base**, not silently combined production code:

| Candidate | Portable patch | Preserved tested work |
| --- | --- | --- |
| Admission/history | [admission.patch](handoff/candidates/admission.patch) | History identity containment, raw-PK repair, standby SQL-shape write admission, GH36 row-delete namespace repair |
| Native authentication | [native-auth.patch](handoff/candidates/native-auth.patch) | Credential validation before registration/WAL, explicit wire version, validation/redaction/restricted plaintext transport, extra controls |
| Physical raw resync | [physical-resync.patch](handoff/candidates/physical-resync.patch) | Raw checkpoint/Follow/Resume transfer and strict read-only hydration; explicitly NON-SERVING |

[Candidate index](handoff/candidate-index.json) records original paths, base, patch hashes and sizes; corresponding `*-source.json` records every checked source hash. Apply each patch to a **separate clean checkout of the stated base** with `git apply --index path/to/patch`. They overlap: do not apply them sequentially or assume their tests validate a combined build. They do not contain build outputs, databases or credentials. External, unintegrated source-certification/serving/session drafts and review records are preserved in [handoff/evidence](handoff/evidence/); their relative ordering and missing module/test wiring are explained in the included subagent handoffs. Some generators refer to original local drafting paths: inspect before using, never rerun them over final artifacts blindly.

## Per-issue outcome — none closed

| Archive issue / Sprinter | Implemented or tried | Evidence and remaining acceptance |
| --- | --- | --- |
| #1 / `7540f7a17c2e` | Fail-fast native listener binding, endpoint diagnostic; later separate native auth candidate | Original library2752/targeted109/live11 pass. Original live uses password-authenticated SQL **plus unauthenticated native registration/WAL**. Later native auth adds actual credentials; final controls/library and final-binary live pass, but integration, security review and broader final-candidate gates remain. |
| #5 / `f170d1796652` | Normalize HTTP IP/socket endpoints, IPv4/IPv6/port precedence and disabled port behavior | Implemented and covered in original CLI/config tests; final adoption/required campaign gates pending. |
| #6 / `ec8a50974152` | `[replication]` schema and explicit CLI/config precedence, daemon propagation | Implemented/tested in original candidate; later auth/physical config branches need deliberate integration and final checks. |
| #7 / `b4fafe6875ae` | Offline reseed, durable-history containment, replay/cache fixes, raw physical transfer/hydration | Original unrelated-history mixing reproduced; containment14checks passed. Corrected admission353/353 and physical356/356 full default+internal target sets pass. Raw-resync CLI fixture passes. **Coherent online SQL serving/resync, generation/session wiring and source-cut certification remain unfinished.** |
| #8 / `7dbefa3f5621` | Explicit default-false development remote-trust opt-in with normal default rejection retained | Implemented/tested; no general default-auth weakening claim, final acceptance pending. This is NOT the promotion issue. |

Additional replay item `ca50db01375f`; declared MSRV issue `e84ae33466f7`. Promotion is a separate unmet backend contract relevant to #7/Proxy#2: RoleManager/WalApplicator helpers do not form a safely fenced end-to-end promotion API. **Never route writes to a read-only standby as remediation.**

## Latest actual evidence (exact candidates remain separate)

- Admission corrected r3: library **2770 pass/0fail/2ignored**, targeted **112pass across9targets**, live simple/COPY/extended refusal/recovery/row-state matrix pass. Full default/internal reruns each **353/353targets**, completeness pass. Original rename regression retained. Autocommit row-delete regression uses strict logical WAL; earlier explicit-transaction workaround is superseded.
- Physical corrected candidate: library **2825/0/2**, targeted **65/0**; GH36 port36/0; full default/internal reruns each **356/356targets**, completeness pass. Additional empty-directory/nonmutation tests pass (8+8). These are recorded versions; later test-only changes are not automatically a repeat of every broad gate.
- Native final review/control run: library **2754/0/2**, native auth **9/0**, including authenticated WAL socket receipt, reconnect, malformed frame, old/new protocol controls. Final live result `native-auth-live-result-final3` has binary SHA **8e46aff9c718d4a6f118a60d702b0ca2061783ad50a7de96f5fd7a35a7b6ea77**; empty env refuses startup, valid token replicates committed row, wrong/missing reject. Older77d6a7e32 live evidence does not validate this binary.
- Raw resync/inspector final fixture passes: auth/history/path refusal, snapshot + continuing updates, Resume/delete/insert cursor progress, preserved old directories, normal SQL startup refusal on receiver. **No receiver SQL readiness claimed.**
- Lint findings remain unwaived: native **1**, physical **27** new `(file,lint)` signatures versus original base. Admission0 new relative to guard still includes2 inherited changes versus original base. MSRV declared1.85 vs locked dependencies requiring1.88 remains recorded. Performance acceptance remains incomplete; a baseline PG35 measurement alone is not a final candidate comparison.
- Historical failures retained: replay/stale cache, raw PK race, mixed histories, standby COPY/extended local writes, GH36 rename failures, old physical compile/nonmutation failures. `physical-lib-r1` is INVALID (copied stale test binary); do not count it.

Sources: [DeepSeek final response](handoff/evidence/DEEPSEEK-ASTRA-REVIEW-RESPONSE.md), [progress](handoff/evidence/OPENCODE-PROGRESS.md), [Astra review](handoff/evidence/ASTRA-NANO-REVIEW-20260930.md), [gate status](handoff/evidence/gates.status), preserved live JSON and completeness summaries. Full local evidence remains `/home/gpc/HDB/sprint/baselines/nano/deployment-20260929`.

## Ownership, live state and next steps

The lightweight controller is stopped from launching more work. The final DeepSeek recovery ran08:26:40–08:41:21UTC Oct1 and exited0; no owned child jobs or host lock remain at that checkpoint. Exact log/status and model/session are in [controller handoff](handoff/evidence/FINAL-CONTROLLER-HANDOFF-20261001.md). Prior Sep30 20:57 restart yielded a zero-byte log and no progress; its cause is unknown, not attributed to a model error. The successful recovery has separate durable stdout/exit evidence.

1. Opus5.5 in `nano` takes ownership; evaluation/acceptance remains Astra or authorized Opus. DeepSeek V4.1 Flash may implement under that review. No automatic successor is scheduled.
2. Review the draft PR and choose candidate integration order explicitly; preserve frozen originals and dirty `/home/gpc/HDB/Nano` work. Do not overwrite its unrelated pending CHAR/CALL fixes.
3. Assess native plaintext remote opt-in and no-crypto comparison fallback, final wire-version compatibility with history-guard protocol2, and fixture task ownership. Positive test results do not settle those design-review decisions.
4. Integrate bounded accepted components into a new branch/candidate; rerun applicable full gates and live scenarios on its exact binary. Finish new lint findings and candidate performance comparisons.
5. Continue source durable-cut certification and immutable serving/session integration from the preserved drafts. Current raw resync cannot be described as complete issue7. Keep promotion unmet until separately designed, fenced and runtime-verified.
6. Append Sprinter evidence one issue at a time; no `done`/release until required acceptance. This handoff/PR does not close any issue.

Every heavy build/test/benchmark must use `/home/gpc/HDB/sprint/coordination/build.lock` with `flock` and `systemd-run --user --scope --collect -p MemoryMax=24G -p MemorySwapMax=0`, jobs2, own target. Do not edit queued/running candidate sources. Test fixtures use private network/tmp and exact owned PID cleanup. Dependency cache copies must not hardlink; clean the destination local crate with `cargo clean --locked --package heliosdb-nano` and audit new test names to prevent stale binary reuse. Never change host firewall/global Docker state or production services to unblock a fixture.
