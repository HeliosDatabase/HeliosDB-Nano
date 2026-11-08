# Native full-tier count adjudicator v1

This is a postrun diagnostic for the unchanged `final-gates-v5` library/full tiers: `lib`, `core-default`, `core-internal`, `core-optins`, and `core-embed-optins`. It never invokes Cargo and never grants release acceptance. Preparation and review executed no native test binary. Root alone may invoke the bounded native-list collector after the corresponding final full stage has completed and its source/target are frozen.

The input is that stage's actual `receipt.json`, the exact **relative-key source and expected manifests emitted by v5 inventory**, the source checkout, and that stage's isolated Cargo target root. Root supplies all three manifest/receipt SHA256 values explicitly. Do not substitute the source28 absolute-path projected manifest for v5's source capture. The projected manifest only supplies the separately pinned source28 critical-fixture expectations.

## Root execution sequence

Use new absolute external output directories for every step. Replace the capitalized placeholders with actual completed-stage paths and actual SHA256 values. None of the following commands has been executed as part of this preparation.

```bash
python3.11 /home/gpc/HDB/sprint/baselines/nano/4420-resume-20261107/full-tier-count-adjudicator-v1/adjudicate.py plan \
  --receipt /ABS/COMPLETED_STAGE/receipt.json --receipt-sha ACTUAL_RECEIPT_SHA256 \
  --tree /ABS/FINAL_CHECKOUT --target-root /ABS/ISOLATED_TARGET \
  --source-manifest /ABS/FINAL_CAPTURE/source-sha256.json --source-sha ACTUAL_SOURCE_MANIFEST_SHA256 \
  --expected-manifest /ABS/FINAL_CAPTURE/expected-manifest.json --expected-sha ACTUAL_EXPECTED_MANIFEST_SHA256 \
  --bundle /home/gpc/HDB/sprint/baselines/nano/4420-resume-20261107/final-gates-v5 \
  --out /ABS/NEW_LIST_PLAN

bash /home/gpc/HDB/sprint/baselines/nano/4420-resume-20261107/full-tier-count-adjudicator-v1/collect-gated.sh \
  --plan /ABS/NEW_LIST_PLAN/plan.json --plan-sha ACTUAL_PLAN_SHA256 \
  --out /ABS/NEW_NATIVE_LISTS

python3.11 /home/gpc/HDB/sprint/baselines/nano/4420-resume-20261107/full-tier-count-adjudicator-v1/adjudicate.py adjudicate \
  --plan /ABS/NEW_LIST_PLAN/plan.json --plan-sha ACTUAL_PLAN_SHA256 \
  --inventory /ABS/NEW_NATIVE_LISTS/inventory.json --inventory-sha ACTUAL_INVENTORY_SHA256 \
  --out /ABS/NEW_COUNT_REPORT
```

`plan` and `adjudicate` perform read-only file/hash checks plus `git ls-files` for complete tracked/untracked source inventory. They run no compiler or test executable. `collect-gated.sh` acquires the shared fleet lock and a unique 24GiB/zero-swap systemd scope. It executes exactly these argument vectors for each **actual native ELF test executable identified by the completed Cargo Running records**, with a 30-second deadline per invocation:

```text
EXACT_NATIVE_BINARY --list --format terse
EXACT_NATIVE_BINARY --list --ignored --format terse
```

These are standard libtest discovery commands, not test-body executions. The collector removes all private `__RUST_TEST_*` child-runner variables and legacy `RUST_TEST_INVOKE` before spawn: libtest's private invoker can otherwise bypass CLI parsing and dispatch a test body even with `--list`. Only removed variable names are recorded. Native loader/startup code still runs. Only ordinary libtest Cargo targets are supported; custom harnesses and target-discovery overrides are refused. A timeout kills only the collector-owned child process group. stdout/stderr, exit status, command, elapsed time, and binary hashes before/after are retained even when discovery is incomplete. A failed inventory cannot yield a green adjudication.

## What is compared

The source manifest must match **every** current tracked/untracked nonignored source file and its bytes. The v5 bundle is pinned, the executed command must equal its unchanged full-tier command, and manifest guards must have passed. Every top-level integration `.rs` from the whole-source capture must have an expected target. Ordinary library/main targets are derived from the actual Cargo manifest. All expected Cargo Running targets must exist, including `main` with zero tests and feature-disabled zero-test integration targets. Each native Running path must match the exact v5 executable hash and remain beneath the supplied target root's `deps` directory. The daemon's separate receipt role is also hashed/rechecked but never executed as a test inventory.

Native discovery, rather than a count of `#[test]` strings, defines the complete test/benchmark name set. This covers macro-generated tests and expanded modules. Ignored discovery must be a kind-preserving subset. The only permitted Rust runner filter is v5's documented substring skip `lock_management`; no `ha_integration` skip is added. For native total `N`, ignored set `I`, and permitted skipped set `S`, exact outer results must satisfy:

```text
running              = |N - S|
passed + failed      = |N - S - I|
ignored              = |I - S|
filtered out         = |S|
measured             = 0
```

This is exact, including zero and all-ignored targets. A source inventory's old `minimum_passed=1` never substitutes for a native count. When source inventory says a target is enabled, an unexpectedly empty native runnable set is additionally refused. Ordinary test mode converts discovered benchmark entries to test execution, so they are included in the count and `measured` remains zero. Every selected native parent name must appear in the actual run log. Nested child libtest logs from crash tests are tolerated: the first outer `running` count and final outer summary of each Cargo Running segment are used; child output is not added to parent totals.

`native_count_adjudication_pass` proves count completeness; it can remain true when some tests failed. `full_stage_correctness_green` additionally requires zero failed tests, successful v5 command/receipt, and no v5 completeness problems. `release_acceptance_pass` is always false. Requirement-specific correctness, native client/protocol, security, physical standby, crash, and performance gates still require their own actual evidence.

## Critical final-source checks

The independent source28 expectations require native **unignored** names, actual run-log names, and exact once-only marker identities for the point cap 5 parents/63 cells; raw routine owner 3/10; internal trigger 2/8; internal setter 7/36; and internal refill 4/16. The full internal/opt-in tiers require all five groups, 21 parents/133 cells per tier. Plain default library/full tiers require the two default-active unit groups, 8 parents/73 cells. These counts do not replace complete native inventories of the remaining tests.

The five exact R57 `row_shape` parents must be native unignored entries and successful actual parents. Their pinned source helper runs **logical_wal_per_statement false/true**, yielding 10 source-backed native crash cases. This dimension is not time travel. No bare `--ignored` run can substitute for the five unignored parents. A fixture-source hash change requires a new reviewed derivative of these expectations; the tool never silently keeps stale cell claims.

## Evidence boundary and synthetic checks

Existing v5 receipts did not attest raw log hashes at original run time. This tool reads/validates their final per-target summaries and first pins raw log bytes at **postrun intake**. It rechecks that hash through discovery/adjudication and labels the origin truthfully. It cannot retroactively prove that an earlier unattested log never changed. The same supplied receipt/source/expected hashes, all v5 bundle files, native binaries, source file set, listing bytes, and package bytes are rechecked through the relevant phases.

These source/hash checks do **not** prove compilation freshness. A recorded native executable can be stale even while the current source bytes match their capture, for example when preserved source mtimes permit Cargo to reuse older output. Required new native names/cells catch some such cases, including the new fifth point-cap parent, but a code-only fix can keep the same test inventory. Root must separately retain actual fresh-target/build-artifact/source linkage. This diagnostic neither rebuilds nor supplies that proof.

`selfcheck.py` tests only synthetic text/data, source mutations, fake ELF-header data files, private-invoker environment removal, and exact v5 schema linkage. It blocks all child-process APIs, including `git`, and executes no fake or real ELF. Its receipt is static evidence only. The final static receipt is listed in the package manifest; earlier checkpoint receipts are preserved. Local official libtest source excerpts and hashes are in `primary-source.json`.
