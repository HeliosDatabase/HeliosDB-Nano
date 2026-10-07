# Nano campaign checkpoint — 2026-10-07

No deployment issue is accepted as fixed, merged, or released. Implementation
continues through OpenCode `deepseek/deepseek-flash`; Astra reviews and accepts
evidence. This session owns Nano only.

The frozen integrated candidate is `ea1fa46052b4d77b5a8cf00bc3eadf9a46701981`
in `/home/gpc/HDB/worktrees/nano-encrypted-repair-20261006`. Its release binary
SHA-256 is `34509596703560b01c9645732905a80a1f8ac9739d3277fc52f1227fd7352f0f`.
The comparison commit remains `916138311c69f1cde070fb3d53fc601e22cc0a06`.
Results from other candidates do not transfer to this candidate.

## Product status

| Topic | Accepted bounded evidence | Remaining work |
|---|---|---|
| HTTP authentication/admission | Live functional gate: nine cells, 3,936 measured requests, 246 warmups, and four authentication probes | Complete candidate gates and performance acceptance |
| Native authentication | Configured shared-token native suite: 12 passed, zero failed, ignored, measured, or filtered | Complete candidate gates; this does not establish every transport or deployment contract |
| Physical transfer and resync | Separate non-serving transfer and containment evidence is retained | Durable committed catch-up, writer coverage, serving publication, and restart continuity |
| Promotion | Refusal/containment candidates remain separate | No accepted safely fenced standby-to-primary promotion API |

The original live-r4 still has its original 11 passing checks: password-authenticated
PostgreSQL SQL plus **unauthenticated native registration/streaming**. The later
shared-token tests do not retroactively establish credential validation on that
original native path. Routing writes to a read-only standby is not remediation.

## Gate limits and retained failures

- The frozen candidate has bounded library, targeted integration, CLI, documentation,
  encrypted-upgrade, and live evidence. Default integration coverage is 14 of 358
  targets: 12 nonempty and two expected empty. There are 344 default and 352 internal
  target variants still outstanding. Repeating the native/durable-index pilot adds
  no unique target coverage.
- Strict Clippy remains failing. Separate source-reviewed lint commits
  `08f168c6266d060b354b5bd541d003cf1089c870` and
  `33b4a38819755534316b54b263b309463ed6fbed` are not compiled or runtime-accepted.
- Earlier throughput retains 14 of 44 median metric increases at or above the 3%
  threshold. Encrypted startup retains a 15.102% increase. Diagnostic commit
  `bf579aef1c6841ce93adb5b6c6afed11067ff9f3` is unbuilt and unrun.
- PG35 produced 280 raw rows across four planned pairs. Three pairs passed the
  bounded operational gate; one failed shutdown cleanup. Eighteen of 35 descriptive
  median Nano changes increased by at least 3%, with seventeen robust to printed
  rounding. This is not an accepted balanced four-pair performance result.
- Full matrix resource admission and product container/release gates remain open.
  No performance failure is waived.

## Scratch cache progression

A fresh scratch compile, its actual 12-test native runtime, archive/decode verification,
and exact executable removal have passed independent audits. Removal ran once:
parent and worker exited zero, the scope became inactive, and no child remained.
The full cache changed from 4,916 to 4,915 entries, removing only the approved
archived executable. Every other entry, the older pilot, both complete source trees,
and both release binaries were preserved. Two evaluator representation failures
are retained separately; the removal job was not retried.

The next build, `durable_index_tests`, has not run. Its driver remains under repair
and independent review. This progression does not establish full-matrix disk fit:
four retained cache manifests alone used 5,668,864 allocated bytes in this cycle.
Compiler growth, compressed executables, and retained evidence must all be counted.

## Host evidence

Current state and raw evidence live under
`/home/gpc/HDB/sprint/baselines/nano/parallel-20261004/`.
`CURRENT-CHECKPOINT-OCT7.md`, `CURRENT-STATE.json`, and `GATE-MATRIX.md` record later
progress; prior versions are preserved. Under
`combined-gates-20261006/encrypted-repair-review-20261007/`:

| Evidence | SHA-256 |
|---|---|
| `scratch-a-runtime-prep-r4/a-runtime/ASTRA-INDEPENDENT-RUNTIME-AUDIT.json` | `0b7cf43857682f561083a2f700a2cd9a41b7ee655922e1f7061be94b7cde9ee6` |
| `scratch-a-archive-prep-r3/a-archive/ASTRA-INDEPENDENT-ARCHIVE-AUDIT.json` | `88eeffbbb975d18b03228a89293031646137a13a22583d3b74d73b33a335a279` |
| `scratch-a-removal-astra-audit-r3/actual-audit-01/ASTRA-INDEPENDENT-REMOVAL-AUDIT.json` | `1a1d5041c6b0a9b15b9320882cb3e06916b13965975ab849046277927715d5d8` |
| `scratch-removal-preflight-root-r1/ASTRA-ACTUAL-REMOVAL-FULL-PRESERVATION-POSTCHECK.json` | `b99cdff496e395c3406e0f1a76a0221150421d56c1df92332ebbf823c5d6297e` |

Heavy jobs acquire the shared build lock individually and use a 24 GiB/no-swap
scope. Scratch currently uses a 72 GiB sampled fail-stop floor, not a hard quota.
Existing jobs, source snapshots, rejected preparations, and failed evidence remain
preserved. The unaccepted archive-worker editing overlap was quarantined; subsequent
Flash sessions have only their own draft, private application state, and temporary
directory writable. No push, PR publication, release, or issue closure follows from
this checkpoint.
