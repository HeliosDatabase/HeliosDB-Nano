---
name: heliosdb-nano-merge-validation
description: The pre-merge validation methodology used in this repo. Required before merging any non-trivial change — bug fixes, optimizer passes, storage tweaks, parser changes, anything user-visible. Eight phases: branch + implement, targeted unit tests, integration regression, targeted feature bench, cross-feature regression, head-to-head OLTP comparison vs main, validation report, release. Use this when the user says "merge this", "this is ready to ship", "before merging", or asks for a perf gate before a release.
allowed-tools: Bash(git *), Bash(cargo *), Bash(gh *), Read, Edit, Write, Grep, Glob
---

# Pre-merge validation methodology

## When to use
- Any non-trivial change that touches engine code (planner, executor, storage, parser, optimizer, transactions, branches, MVCC).
- Anything user-visible: query results, performance, error messages, public API.
- **Required before** any tag-driven release. The release workflow gates on `cargo test --lib + --doc` only — it cannot catch perf regressions or behavioural changes in queries that aren't unit-tested. This methodology is the human-driven gate that complements the workflow.

Trivial changes (typos, comments, doc-only edits, README links) can skip phases 4–6 but should still run phase 3 (integration regression).

> **Risk note**: This methodology can take ~1–3 hours of wall time end-to-end for a meaningful change, mostly in compilation + bench iteration. Don't shortcut it for changes that affect query behaviour — the v3.23.0 fix that prompted this skill was a correctness bug latent for months because no validation gate caught it on the way in.

## The eight phases

| # | Phase | Output | Skip-able? |
|---|-------|--------|------------|
| 1 | Branch + implement | Working branch with code changes | No |
| 2 | Targeted unit tests | Matrix of unit tests in `src/.../tests` | No |
| 3 | Integration regression | `cargo test --lib --tests` is green | No |
| 4 | Targeted feature bench | A/B harness in `benches/<feature>_bench.rs` | If change has no perf surface |
| 5 | Cross-feature regression | Existing benches still match historical numbers | If change has no perf surface |
| 6 | Head-to-head OLTP vs main | `examples/oltp_smoke.rs` numbers match between main & branch | If change is read-only / non-engine |
| 7 | Validation report | `<FEATURE>_REPORT.md` with matrix + risk + merge call | No |
| 8 | Release | `nano_release_process.md` (memory) | n/a — separate step |

## Phase 1 — Branch + implement

```bash
git checkout main && git pull --ff-only origin main
git checkout -b feat/<short-name>
# … implement …
```

The branch name should match the area being changed: `feat/predicate-pushdown`, `fix/cross-process-on-conflict`, `perf/art-index-zero-copy`. One change per branch — small, focused, reviewable.

## Phase 2 — Targeted unit tests

For the change being made, enumerate the **matrix** of input shapes that exercise its behaviour. For an optimizer rule that's the join-type matrix × predicate-shape matrix × tree-position matrix. For a parser fix it's every dialect variation. For a storage change it's every persistence path.

The v3.23.0 example used 12 unit tests covering:
- Pushable cases (right-only literal / non-constant; left-only literal / non-constant)
- Non-pushable cases (cross-side equi; mixed equi + one-sided)
- Outer-join semantics matrix (LEFT/RIGHT/FULL never push the wrong side)
- Edge cases (LATERAL / cross-join / no-op)
- Properties (idempotency under repeated application)
- Recursion (rule descends through Project/Filter/Sort/Limit/Aggregate/With/Union)
- Applicability cheap-pre-filter (returns false in O(plan-depth) when no relevant nodes exist)

```bash
cargo test --lib <module>::<test_prefix>
```

A unit test that only covers the happy path is not enough. **Write the tests that would have caught the bug had it been written first.** For correctness bugs, a passing pre-fix unit test is the single best gate against re-regression.

## Phase 3 — Integration regression

```bash
cargo test --lib                                            # ~1700+ unit tests
cargo test --tests --skip ha_tests::streaming_tests --skip lock_management
                                                           # ~1500+ integration tests
                                                           # (HA streaming + lock-management hang on
                                                           #  constrained runners; pass locally;
                                                           #  filed under FR_ha_streaming_runner)
```

Goal: zero new failures. If a previously-passing test now fails, **that's the result of your change** until proven otherwise — investigate, don't `#[ignore]`. Pre-existing flakes (the HA-streaming and lock-management ones above) should be filtered out via `--skip`, never disabled.

If the change is correctness-related, also re-run any test that was previously `#[ignore]`'d for the same area — your fix may have closed it. The v3.23.0 fix un-ignored `cte_hardening::test_basic_cte_used_in_join` as part of this phase.

## Phase 4 — Targeted feature bench

Write a new bench (or extend an existing one) that **measures the specific behaviour your change affects**. Two requirements:

1. **A/B comparison** — run the workload twice, once with the new code path active and once without. For an optimizer rule that means two `Vec<Box<dyn OptimizationRule>>` lists. For a query path it means a config flag. For a write path it means a separate code branch in the bench loader.

2. **Sanity check both arms produce the same logical result** — count, hash, or row-set comparison. **A bench that times two divergent answers is worse than no bench at all.** When the buggy path returns the wrong rows (as in v3.23.0 Q2/Q4 where the buggy classifier returned 6.7× extra rows), the harness should flag the divergence and skip the apples-to-oranges comparison rather than reporting a meaningless "speedup".

Example (`benches/predicate_pushdown_bench.rs`):
- 4 query shapes (control + three pushdown opportunities).
- Each query run twice: with rule, without rule.
- Sanity-check scalar `COUNT(*)` matches before timing.

Sizing: pick a dataset large enough that the rule's effect is measurable above noise, but small enough to iterate on. Default to ~100 K rows / ~200 MB for typical benches; document an env-var to scale up to 10 GB for full validation.

```bash
cargo bench --bench <feature>_bench
HELIOSDB_<FEATURE>_BENCH_ROWS=5000000 cargo bench --bench <feature>_bench  # full scale
```

## Phase 5 — Cross-feature regression bench

Run **existing** benches that exercise areas your change should not affect. If your change is purely SQL-planner, run `art_index_bench`, `vector_search_bench`, `branch_performance`. If it's purely storage, run `phase3_benchmarks`, `multi_tenancy_bench`. The point is to confirm a side-effect-free change.

```bash
cargo bench --bench art_index_bench -- --quick
cargo bench --bench vector_search_bench -- --quick
cargo bench --bench branch_performance -- --quick
```

Compare numbers to the previous run if you have one cached (`target/criterion/`), or to the historical baseline in `docs/BENCHMARK_PG_VS_HELIOS.txt`. **A regression here means your "isolated" change leaked across module boundaries** — investigate before merging.

## Phase 6 — Head-to-head OLTP vs main

This is the gate that catches latent perf regressions in code paths the targeted bench doesn't exercise. Run a representative OLTP workload on **main** AND on **your branch** with the **same release binary**, back-to-back, and compare.

The repo ships `examples/oltp_smoke.rs` for exactly this — it mirrors the workload shapes of `benches/external/pg_vs_helios.py` (batch INSERT, single INSERT, PK lookup, COUNT, INNER JOIN, repeated query) via the embedded API.

```bash
# 1. On the feat branch
cargo run --release --example oltp_smoke > /tmp/oltp-feat.txt 2>&1

# 2. Switch to main, copy the example over (it may not be there yet on main)
cp examples/oltp_smoke.rs /tmp/oltp_smoke_keep.rs
git checkout main
cp /tmp/oltp_smoke_keep.rs examples/oltp_smoke.rs

# 3. Run on main
cargo run --release --example oltp_smoke > /tmp/oltp-main.txt 2>&1

# 4. Compare
diff /tmp/oltp-main.txt /tmp/oltp-feat.txt

# 5. Restore feat branch
git checkout feat/<branch> -- examples/oltp_smoke.rs   # or stash/pop
```

For sub-millisecond measurements (the JOIN p50 in v3.23.0 was ~9 µs), a 5-sample median is too noisy. Bump sample size to 1 000–2 000 and report **p50, mean, and p99**. The v3.23.0 INNER JOIN p50 looked like a 33% regression at n=5 and turned into 11% improvement at n=2000 — same code, just enough samples to see signal through noise.

> **Reconcile against historical baselines**. If your branch's numbers diverge from `docs/BENCHMARK_PG_VS_HELIOS.txt`, understand WHY before assuming regression. The historical doc measures the **PG-wire / psycopg2** path; the embedded API in `oltp_smoke` is ~30× faster for OLTP workloads on localhost. That's wire-protocol overhead, not a property of the database core.

## Phase 7 — Validation report

Write a `<FEATURE>_REPORT.md` at the repo root (the v3.23.0 example is `PREDICATE_PUSHDOWN_REPORT.md`) capturing:

```markdown
---
branch: feat/<name>
parent-tag: vX.Y.Z
status: ready-for-review
date: YYYY-MM-DD
closes: <FR file or issue link>
---

# <Feature> — Implementation & Validation Report

## Summary
What changed, why, in one paragraph.

## Changes
| File | Δ | Notes |

## Correctness gates
1. Originally-failing test passes (Phase 2)
2. Lib unit tests (Phase 3)
3. Integration suite (Phase 3)

## Performance
### Methodology — what bench, what dataset, what knobs (Phase 4)
### Results — a table comparing baseline vs new
### Scalability projection — how the gap grows with data size

## OLTP head-to-head — the Phase 6 table
## Cross-feature regressions — Phase 5 results

## Risk
| Concern | Assessment |
| outer-join semantics | … |
| LATERAL | … |
| caching / plan-cache poisoning | … |
| rule ordering / interactions | … |

## Open / deferred
1. Things known-broken that this PR doesn't fix
2. Future polish

## Recommendation
**Merge** or **block on X**. Be explicit.
```

The report is the **single deliverable** a reviewer reads before approving. Make it skim-able: tables over prose, concrete numbers over hand-waving, a clear merge call at the end.

## Phase 8 — Release

Per `~/.claude/projects/-home-app-Helios-Nano/memory/nano_release_process.md`:

```bash
# 1. Bump Cargo.toml version (semver: minor for behaviour change, patch for perf-or-doc)
# 2. Update CHANGELOG.md: rename [Unreleased] to [X.Y.Z] - YYYY-MM-DD
# 3. Update Cargo.lock
cargo update -p heliosdb-nano --offline

# 4. Delete the FR file (per its own acceptance criterion)
git rm FEATURE_REQUEST_<area>.md

# 5. Commit + push + tag + push
git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "release: vX.Y.Z — <headline>"
git push origin main
git tag -a vX.Y.Z -m "vX.Y.Z — <headline>"
git push origin vX.Y.Z   # fires release workflow → publish → GitHub Release

# 6. Watch
gh run watch $(gh run list --workflow=release.yml --limit 1 --json databaseId -q '.[0].databaseId') --exit-status
```

The workflow gates again on `cargo test --lib + --doc`. If it fails here, you missed something in phase 3 (or the workflow runs in a different environment than your local cargo). **Don't skip phases 4–6 because the workflow passed them too** — those phases catch perf and behavioural regressions, which the workflow does not.

## Pitfalls

- **Skipping phase 6 because phase 4 looked good.** The targeted bench measures the change's intended effect; OLTP-smoke catches incidental damage to other paths. Both are required for engine changes.
- **Trusting a 5-sample median on sub-millisecond measurements.** Bump sample size and report percentiles. v3.23.0 looked like a 33% regression at n=5.
- **Not sanity-checking that both A/B arms produce the same answer.** A bench that times the buggy path doing 6.7× the work and reports it as "slower" is misleading — the fast path was wrong.
- **`#[ignore]`-ing a test that started failing during your change.** That's the bug your fix introduced, not a pre-existing flake. Pre-existing flakes (HA streaming, lock-management) are documented and filtered with `--skip` — they're known.
- **Bumping a patch when behaviour changed.** Semver: a query that used to return 9 rows now returns 3 — that's user-visible, that's a minor bump. Patch bumps are for invisible changes (perf, docs, no-op refactors).
- **Writing the report after merging.** The report is what convinces a reviewer to approve. Without it, the validation matrix is unverifiable.
- **Force-pushing a release tag.** Never (the safety policy denies it). If CI fails: `git push --delete origin vX.Y.Z`, fix the issue, recreate the tag at the new HEAD, push.
- **Inflating ingest-rate claims from one bench loader.** v3.22.3's "27 rows/s" was a multi-row-INSERT seeding strategy that hammered the SQL parser. The committed prepared-statement path is ~800 rows/s. Always measure with the path you'll actually ship.

## See also
- `heliosdb-nano-install` — set up a build for benchmarks (release mode, default features).
- `heliosdb-nano-observability` — `EXPLAIN ANALYZE`, slow-query log, and `\stats` for performance debugging during phases 4–6.
- `heliosdb-nano-server` — production-affecting flags; phase-6 OLTP bench should mirror the auth/TLS/replication flags the deployment uses.
- `~/.claude/projects/-home-app-Helios-Nano/memory/nano_release_process.md` — the saved memory for phase 8.
- `PREDICATE_PUSHDOWN_REPORT.md` — the v3.23.0 worked example. Use as a template.
- `examples/oltp_smoke.rs` — the OLTP workload harness from phase 6.
- `benches/predicate_pushdown_bench.rs` — the targeted-feature bench template from phase 4.
- `benches/external/pg_vs_helios.py` + `docs/BENCHMARK_PG_VS_HELIOS.txt` — historical PG-wire baselines for reconciling phase 6 numbers.
