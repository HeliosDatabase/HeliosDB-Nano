# Claude handoff — Nano deployment campaign, 2026-10-07

Work stopped at the user's explicit request: “Write final statement and close this session, handing over to Claude.” No owned build/test/runtime job remains active or queued. All subagents were told to stop; the final ongoing SCRAM audit completed. No deployment issue is accepted as fully fixed, merged, or released.

## Receiver and ownership

`tmux Nano:0.0` is this Codex/Astra session, not a Claude receiver. The default tmux server has only Full, Lite, Nano, and Proxy panes; no separate Claude pane was found. No message was sent into this session or an unrelated pane. This file is the handoff; delivery to, or acknowledgment by, a running Claude instance is **not claimed**. The successor should read this file and `WORK-CHECKPOINT-20261007.md` before resuming.

Nano only. Proxy belongs to its separate session. Do not route writes to read-only standby. Preserve all existing dirty work, source snapshots, rejected preparations, failed runs, evidence, and `target-campaign/`. Do not reset or broadly clean. No push, PR publication, issue closure, release, tag, image publication, or security publication is authorized by these gate results.

The user requested implementation through OpenCode `deepseek/deepseek-flash` under cheap controllers; evaluation/acceptance only by Astra or Claude Opus 5.5. Preserve that separation. Flash is recharged. New Flash invocations must use the existing OS-fenced wrapper pattern: host/source/evidence read-only; only its new code folder, private application state, and private temporary directory writable. This follows a preserved prior wrong-controller editing incident.

## Exact candidates and worktrees

- Campaign documentation worktree: `/home/gpc/HDB/worktrees/nano-issues-20260929`. Parent documentation commit at handoff preparation: `02927c19b879c173624a0c582a408760fb29b1de`. Only the checkpoint and this handoff are being committed locally; existing untracked `target-campaign/` is untouched.
- Frozen integrated candidate: `/home/gpc/HDB/worktrees/nano-encrypted-repair-20261006`, commit `ea1fa46052b4d77b5a8cf00bc3eadf9a46701981`, tree `4513d4b8b35398ed0ea18c30346c17bceca67910`.
- Frozen candidate release: `target-encrypted-release/release/heliosdb-nano`, SHA `34509596703560b01c9645732905a80a1f8ac9739d3277fc52f1227fd7352f0f`.
- Comparison: `/home/gpc/HDB/worktrees/nano-upgrade-base-20261004`, commit `916138311c69f1cde070fb3d53fc601e22cc0a06`, release `target-upgrade/release/heliosdb-nano`, SHA `02862f46af156ed056883d9721c2d518efed755ec2f1d99db533bda57c203838`.
- Separate source-reviewed, **uncompiled/unrun** lint commits: `08f168c6266d060b354b5bd541d003cf1089c870` in `nano-http-lint-repair-20261007`, then `33b4a38819755534316b54b263b309463ed6fbed` in `nano-auth-lint-next-20261007`.
- Separate source-reviewed, **uncompiled/unrun** startup diagnostic commit: `bf579aef1c6841ce93adb5b6c6afed11067ff9f3` in `nano-encrypted-startup-instrumentation-20261007`. Its opt-in monotonic span logging does not waive performance failures.

Other candidates do not inherit frozen-candidate gates.

## Evidence locations

Root: `/home/gpc/HDB/sprint/baselines/nano/parallel-20261004`.

Below, `CR` means that root plus `/combined-gates-20261006/encrypted-repair-review-20261007`. `CURRENT-STATE.json`, `CURRENT-CHECKPOINT-OCT7.md`, and `GATE-MATRIX.md` have a final stopped-for-handoff update; their previous bytes are retained under `CR/recharge-resume-0718`.

## Latest actual acceptance

Default integration coverage is **16/358 targets**: 14 nonempty plus two expected-empty targets. **342 default and 352 internal variants remain outstanding.** The two newly accepted runtime targets contribute 16 actual tests. Compiles alone add no coverage. `deployment_modes_e2e_test` has 26 print-only placeholders and receives no functional credit.

| Gate | Actual result | Independent audit under CR | SHA-256 |
|---|---|---|---|
| TLS-only compile | PASS_COMPILE_ONLY; 439 reused artifacts, one test compiled, 2,396,160 bytes growth | `next-tls-actual-bindings-r1/ASTRA-INDEPENDENT-BUILD-AUDIT.json` | `e5b3ab6b0f3f9e4ca809c7a56476cac1b723ebbb39dc248fc9d79c3071a24a1d` |
| Deployment + SCRAM compile | PASS_COMPILE_ONLY; exact two test artifacts, 439 reused, 32,808,960 bytes growth | `next-admission-actual-bindings-r1/ASTRA-INDEPENDENT-BUILD-AUDIT.json` | `fcba1d9b4971dc19e10cbaab52e9614b25d49a2ee8490335993f71823b0279c2` |
| TLS runtime R5 | 3/3, zero failures/ignored/measured/filtered/skips; 0.10s | `next-tls-runtime-actual-bindings-r5/ASTRA-INDEPENDENT-RUNTIME-AUDIT.json` | `9253533c53b6a73cb7a51136eb869fd32b1235f0321455c1d470b86c48269be8` |
| SCRAM/trust runtime R1 | 13/13, zero failures/ignored/measured/filtered/skips; 0.02s | `next-scram-runtime-actual-bindings-r1/ASTRA-INDEPENDENT-RUNTIME-AUDIT.json` | `285a8fc6e87a4b58f41b1a49487b7159b2c26a0f3d3711fe0d4408f841210727` |

TLS root session 65145 and SCRAM root session 63934 were collected with exit zero. Both had list/run/worker/parent exit zero, natural completion, inactive/dead scopes with empty cgroups, no TERM/KILL, and no private-namespace leftovers. Full postchecks verified 7,586 regular source files, eight symlinks, both release binaries, all 4,943 cache entries, archive/B/old-pilot artifacts, and continued A executable absence.

- TLS full postcheck: `next-tls-runtime-actual-bindings-r5/ASTRA-FULL-POSTCHECK.json`, SHA `1dbbb5fffd9724d89fa74c7c99ef4ced9ed8632551356f85f7a740c02b0e2c59`.
- SCRAM full postcheck: `next-scram-runtime-actual-bindings-r1/ASTRA-FULL-POSTCHECK.json`, SHA `3168f421bc474c34a7221d5d43d2917879ddb437cf4cfe10ab00d14b7e7d780a`.
- TLS checks only incomplete startup TLS flag refusal; no handshake/auth claim. Child diagnostics/non-success are asserted in Rust, without separately retained child diagnostic logs or exact exit-1 assertion.
- SCRAM checks six parser cases and seven constructor policies; no daemon spawn, listener, IPv6 availability skip, or wire authentication. Some inherited introductory harness prose says otherwise; Astra review explicitly corrects that wording.

## Immediate unfinished next step: deployment runtime

**No deployment runtime has launched.** Its actual 23-test binary is already compiled:

`/home/gpc/HDB/worktrees/nano-encrypted-repair-20261006/target-encrypted-archive-progression-r1/debug/deps/deployment_cli_tests-29e9f1d260e7257b`

SHA `37de99e51ac6cecf05a646979c9294166b9527b19ef73c2cd70638f2f388e06d`, 1,050,936 bytes. Distinct embedded daemon: `debug/heliosdb-nano`, SHA `5f9bb8b2f41d905b3463966c6d4b311893b28e0b77b5f06e087122bb60b3587f`, 49,490,736 bytes. Bind actual Cargo audit fields, not a guessed binary or manufactured audit.

Flash preparation is frozen at `CR/next-deployment-runtime-prep-r1`:

- `deployment_outer_gate.py`: `676b35f762919c016c726af70cc779085de3b2036ad6a5bcd0d0629b14c4b9f5`.
- `deployment_inner_worker.py`: `c27734e8912919b20d7ae3a6e8be227c257918149dac319c82ae4d5e5fb79e24`.
- Tests: `c376f73be6817a92962a8b9df5060c914165ebc70e283071788fb29dec58230e`.
- Root independently invoked all **103 hermetic test functions**, passing, with raw output `CR/next-deployment-astra-review-r1/HERMETIC-RAW.txt`. An earlier unittest invocation discovered zero tests; it is explicitly uncredited in `ROOT-TEST-DISCOVERY-CORRECTION.json`. Do not use unittest's zero-test green as a gate.
- Root inspected the delta: Supervisor AST unchanged from accepted TLS R5; main differs in job name and reporting; intended functional changes are target/audit mapping and explicit IPv6 semantic-skip reporting. **Final source/contract acceptance was not written before the stop request.** No execution authorization or concrete deployment binding exists.
- `CR/next-deployment-runtime-actual-bindings-r1/ASTRA-FULL-PRECHECK.json` exists from the same cut as the accepted SCRAM postcheck. It is preparation evidence, not authorization; refresh before execution.
- Exact IDs: `CR/next-admission-runtime-plan-r3/INVENTORY.json` plus `CFG-CORRECTION.json`. There are ten unconditional and thirteen `ha-tier1` declarations, all enabled by the actual default five-feature closure. R1/R2 inventories were wrong and preserved; R3's “no feature-gated declarations” text is superseded by the correction. Root exact-ID review SHA `1097b29b3ccc3d67d79eaee9305758c7a5c45c1094a06a9c4dd29b95aa474dc7`.
- Runtime should use exact `--list`, then `--test-threads=2 --nocapture`; bounds are 120s list/600s run/900s outer. Unproven 45s per-test proposals were not adopted.
- IPv6 unavailable early return must remain an explicit semantic skip, not credited IPv6 behavior. Private PID namespace is essential for fixture process cleanup.

Successor sequence: finish independent deployment harness review; assemble a fresh exact binding with **all** completed A/B/TLS/admission/SCRAM job and binding directories counted once, including actual TLS/SCRAM runtime jobs; refresh full source/release and 4,943-entry cache prechecks; authorize only the one gate; execute under fleet lock/private namespace; collect actual parent exit; verify scope inactive/reaped before full posthash; independently audit all 23 outcomes and any semantic skip. Only then consider target credit.

## Scratch preservation and host controls

Current target: `/home/gpc/HDB/worktrees/nano-encrypted-repair-20261006/target-encrypted-archive-progression-r1`, device 2307, inode 5867015746.

Current cache manifest: `CR/next-admission-actual-bindings-r1/CACHE-AFTER-ADMISSION.json`, SHA `b56b25f7ef228e7f30b25853814e5735888445e754fde1d186ad24c0d5141533`, exactly 4,943 entries. Historical cache comparison helpers hardcode older counts; reuse their read-only manifest builder, then compare current root identity and every entry against this latest manifest. Do not relabel historical manifests. A root stale-count validation failure was preserved and corrected before execution.

Fresh A native executable was archived, decode-verified, and removed exactly once by an accepted controlled gate. Do not rerun removal. Archive: `CR/scratch-a-archive-prep-r3/a-archive/archive/native_replication_auth-824b46e734f73d51.xz`, SHA `8c39b59317f5062f6534068b70801ff5f1bb60a9ab43925307e497b6cbe1bdfb`. Its original decoded SHA is `3b0cdc143b15872788290bc7f4cc8bc7e7eccc3eaf8de78c4a323a8851059714`. The older pilot executable remains protected. B executable `durable_index_tests-c785026e25ac295e`, SHA `c2dff03980edd1f97954c1d1956115f949fa2313bfe540f8ad75b2d41e0c9281`, is retained. Preserve all new TLS/SCRAM/deployment executables too.

Every heavy job separately uses `/home/gpc/HDB/sprint/coordination/build.lock`, `flock -E 75 -w 7200`, and a user systemd scope with `MemoryMax=24G`, `MemorySwapMax=0`. Release between jobs; never interrupt other sessions' running tests. Cargo/test parallelism two. Cargo home `/home/gpc/.cargo-homes/nano`. Source/target are read-only for runtimes; only owned job/tmp writable; private user/PID/net/proc/run, inner timeout, raw exits and post-reap census mandatory.

Scratch resource policy: 72 GiB **sampled fail-stop floor**, not hard quota; compile admission strictly above 74 GiB, 2 GiB incremental and 3 GiB cumulative caps; runtime admission above 72.25 GiB, 0.25 GiB own-job and 3 GiB cumulative caps. Count retained archives, cache, every actual job/binding root, failures, and data peaks. No full-matrix fit claim. Future batch proposal is not approved: `CR/future-matrix-astra-review-r1/ASTRA-PROPOSAL-REVIEW.json`, SHA `e3bc9f49fb275b6591c83612d5351d26b3b5606171bad2541adc381809f52d7a`. It cannot waive archive-before-removal or assume incompatible feature closures share a target; grouped full manifests can still grow quadratically. No new cleanup is authorized.

## Four product topics and unresolved acceptance

1. **HTTP authentication/admission:** bounded live gate accepted nine cells, 3,936 measured + 246 warmup requests, four auth probes, cleanup. Full gates/performance remain open.
2. **Native authentication:** configured shared-token native suite 12 passed. Original live-r4 remains its original 11 passing checks: password-authenticated PostgreSQL SQL plus **unauthenticated native registration/streaming**. Do not retroactively claim native credential validation on that original path.
3. **Physical transfer / serving resync:** non-serving physical-transfer and fail-closed containment evidence are separate. Original missing-row replay defect remains unclosed. Durable committed source history, complete managed writer coverage, atomic data/cursor application, publication generation/fencing, restart continuity, and sync ACK semantics are still unresolved. `RECOVERY-COMMITTED-PATH-ASTRA-NOTE-OCT7.md` and `recovery-writer-closure-r1/REPORT.md` under CR describe the blockers. WalStore is not a proven committed journal; broadcast wakeups, LSN allocation, and atomic raw transfer alone do not solve this. No safe small closed implementation tranche has been established. Rejected broad journal/receiver prototypes remain preserved.
4. **Promotion:** no accepted safely fenced standby-to-primary API. Separate refusal/containment candidates are not promotion. No direct standby routing remedy.

Strict Clippy remains FAIL; warn-capped diagnostics are not acceptance. Prior performance failures remain: throughput 14/44 median metrics at or above 3%, encrypted startup +15.102%, and PG35 one shutdown-cleanup failure plus 18/35 descriptive median increases at or above 3% (17 robust to rounding). No balanced-four PG35 acceptance or performance waiver. Full default/internal matrix, product container/release gates, and resource feasibility remain open.

All previous raw failures and rejected harness revisions are retained. This handoff transfers unfinished work and precise bounded evidence, not issue closure.
