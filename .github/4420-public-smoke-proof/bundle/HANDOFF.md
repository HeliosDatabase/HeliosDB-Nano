# V8 adoption boundary

Adopt the three product paths from ../public-perf-native-proof-producer-v1/delta.patch, commit/capture the assembled product, then adopt this 24-member bundle in the root adapter. Keep an owned fresh target: `target/perf` must not exist before the proof helper starts. Run the same `public-perf-smoke` stage through the existing fleet wrapper. Raw logs and COMPLETE.json must be adjudicated after actual execution. There was no Cargo, native `--list`, test body or server execution in this preparation.

Only commands.json public-smoke metadata/env, run_stage.py's mandatory structured public-smoke consumer, selfcheck.py public-proof expectations and these preparation docs/receipts change. No public Rust workload, baseline, repeat count, best-of-three aggregation,2.5 cliff or unrelated stage command changes. Readiness lineage/assertions remain inherited; the new public-only fixture commands are recorded externally.

The preserved V7 public-smoke red was a producer/consumer proof mismatch, not a measured cliff: native quiet/terse output cannot satisfy literal pretty markers. V8 requires strictly stronger explicit compiler/process/native receipts and retains the old result without waiving it.

Historical handoff follows, with its old public marker/glob paragraph superseded above.

# Final gates v5 preparation handoff

Minimal external derivative of frozen `../final-gates-v4`; 37 prepared stages. No Cargo, product binary, client, server, Docker or load execution was performed. All runtime acceptance remains with root and source-bound receipts.

- `v4-input-sha256.json` binds all inherited v4 file bytes; v4 and its historical captures were verified unchanged. Historical source/integration inventories are deliberately absent from v5. Root must create and independently review a fresh final-source capture after its safe source copy, then pass both manifests explicitly to every stage.
- `run_stage.py` pins the initial source/expected manifest bytes once, uses the pinned expectations throughout, records exact before/after SHA256 and fails on either changed/deleted input. It fixes and records `CARGO_PROFILE_TEST_DEBUG=0` and `CARGO_PROFILE_DEV_DEBUG=0`. Direct Cargo command feature receipts derive from that command's argv, including default/all-feature mode; non-Cargo harness receipts are explicitly labeled metadata.
- Exact selected counts are MV7/overlay10/uncertain7/fast-side7, all ten overlay markers/193 asserted cells, PQ backend6/public manager1, correlated integration4/once integration2, plus forced fused/context and feature-appropriate catalog/config library test completion markers. `expectation-source-check.json` is only the selected source expectation check, not a final-source inventory or runtime receipt.
- Readiness invokes this v5 bundle's own helper; five missing stage feature declarations now match actual argv. Codegraph field timing runs `--release`. Performance pointers use frozen `perf-matrix-v3` + `perf-build-orchestrator-v2` (75 jobs/1216 declared cells); no semantic or performance result is inferred.
- New `public-perf-smoke` invokes the unchanged `benches/public/ci_perf_smoke.sh` under the existing outer fleet lock and 24 GiB/no-swap scope. Complete `ci_perf_smoke.sh`/`common.sh` inspection found no inner flock/systemd lock. The helpers generate ignored `benches/public/results` files. Fixed3 repetitions and stored2.5x cliff threshold; require all12 exact labels (11 TPS+FK), test-success markers and script pass. Copy raw/JSON output, hash baseline and the sole isolated perf-profile TPS executable, record compiler versions. Use a fresh default-feature target for this stage so an ambiguous binary set fails closed. Never regenerate/loosen the baseline. This does not replace the separate <3% release ABBA/native-wire/physical ACK matrix.
- Existing full `--tests --no-fail-fast`, exact MSRV/ORT, FIPS/ring separation, TLS/archive/P4/readiness/physical commands and source identity guard remain. The only documented full-suite skip is `lock_management`; no `ha_integration` skip was added.

`selfcheck-receipt.json` records10 passing synthetic groups, including a mocked runner (no process launched) that weakens the expected manifest mid-command: the initial nonzero demand remains enforced and the changed hash fails the receipt. Python AST and `bash -n` checks pass (`syntax-check-receipt.json`). These are preparation checks only.

Run with `run-one.sh` and fresh explicit source/expected captures, feature/toolchain-isolated targets and unique output directories as documented in README. Actual source18 capture, default/internal/opt-in/FIPS/MSRV/security/docs/format/new-Clippy, native physical/scale, MySQL branch/standby and full performance acceptance are not supplied by this package.

ORT prerequisite documentation points to the actual task-owned1.24.4 library/env package, bound by `ort-prerequisite-reference.json`; runtime loading, model download/cache and384-dimensional inference remain pending real gates. HF_HOME and FASTEMBED_CACHE_DIR are included in actual environment receipts when set.

Independent R56 read-only source/receipt review found no concrete blocker (`REVIEW.md`). The frozen package file hashes are published in `bundle-sha256.json`; root should verify them before using this preparation.
