# Quality gates — what each one is for

Every gate below exists because it catches a class of defect the others miss. Each entry
gives the exact invocation, the defect class it catches, when to run it, and a **real
capture** from this repo's history proving why it earns its slot. Written for anyone
(human or agent session) landing changes in this repo; the mandatory list lives in
`CLAUDE.md` ("Quality Gates") — this file explains the *why*.

## Operational ground rules (shared host)

This host runs production-like services and once livelocked for 16h under a runaway
benchmark (38 GiB RSS — `sprint/status/incident-2026-07-08.md`). Therefore every heavy
cargo op (build / test run / clippy / benchmark):

```bash
flock /home/gpc/HDB/sprint/coordination/build.lock \
  systemd-run --user --scope -p MemoryMax=24G -p MemorySwapMax=0 -- <cmd>
```

- **One heavy op at a time, fleet-wide.** The flock serializes across the five product
  sessions; the bounded scope means a runaway dies alone.
- **Load-gate timing-sensitive runs**: wait for `load1 < 6` before pg35/bench-engines
  (a 300-iter run under load 27 is noise, not data).
- **Never build in a tree another task owns.** A background task that checks out /
  builds a tree owns it; foreground git ops against it corrupt both (capture: a
  cherry-pick landed on a detached HEAD mid-task and contaminated the task's verdict,
  2026-07-16).
- **No hardcoded test ports.** `heliosdb-lite-ca` permanently maps 15432-15433; the SSL
  suite hung a gate for 66 minutes at 0 CPU connecting to it (capture below). Use an
  ephemeral probe (`TcpListener::bind("127.0.0.1:0")`) + a negotiation timeout.
- Exact-PID kills only, never pattern-match `pkill` (other live heliosdb processes).

## The gates

### 1. `cargo test --lib` — the cheapest signal
~2,000 unit tests, ~1 min runtime after build. Run FIRST after any change; it catches
compile breaks, borrow/lifetime slips, and in-crate contract drift before you spend an
hour on the integration suite.
**Capture:** the TRUNCATE contract unification (2026-07-16) — the lib test
`test_truncate_returns_zero` had drifted to assert the row count *under its
zero-asserting name*, directly contradicting the integration suite. Only running both
tiers exposed that the two contracts could never both pass.

### 2. Targeted integration suites — fast per-area confidence
`cargo test --test <suite> [--test <suite>…]` for the areas your diff touches. Seconds
to minutes; run between every increment. Map: touched `scan.rs`/optimizer →
`fast_path_eligibility_tests`, `parameterized_query_tests`; RLS/tenancy →
`multi_tenancy_integration`; branching → `branch_data_isolation_test`,
`branch_storage_test`; COPY → `fk_validation_modes` + `tests/protocol_tests` (psycopg);
catalog/wire → `information_schema_completion` + psycopg suite.
**Capture:** W1.3's new branch-switch test failed in the targeted run and unearthed TWO
pre-existing wrong-data bugs (SQL caches surviving `USE BRANCH`; row-cache cross-branch
poisoning) — neither reachable by unit tests.

### 3. Full integration suite — ALWAYS `--no-fail-fast`
```bash
cargo test --tests --no-fail-fast -- --skip ha_tests::streaming_tests --skip lock_management
```
(the two skips are documented pre-existing flakes on constrained runners — never add new
ones without written justification in the commit message).
**Why `--no-fail-fast` is load-bearing:** cargo stops scheduling test binaries after the
first failing TARGET. On 2026-07-16, run 1 died at `f…` and run 2 at `i…` — suites `j…z`
had NEVER RUN until the `--no-fail-fast` pass, which then surfaced four more findings in
one sweep. Without the flag you don't have a full-suite result; you have a prefix.
**Capture:** that first complete run: 239 suites / 4461 passed / 5 findings — a hung SSL
test (port squat), a hollowed `/tmp` fixture, a sequences test-parallelism race, a real
planner ORDER-BY-over-GROUP-BY bug, and the TRUNCATE contract split. None were caused by
the change under test; all had been invisible because **the integration suite has zero
CI execution** (an open infrastructure gap — budget triage time for pre-existing rot on
every first full run).

### 3b. The empty-suite check — a green line that ran nothing

A suite reporting `test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered
out` is an **empty test binary**. It scrolls past as `ok`, contributes 0 to the totals, and
is indistinguishable from a suite that does not exist. Grep every full-suite log for it:

```bash
awk '/^     Running/{s=$2} /^test result: ok\. 0 passed; 0 failed; 0 ignored/{print "EMPTY:", s}' full.log
```

**As of 2026-08-17, 49 of the files in `tests/` run zero tests under the default command.**
Most are legitimately opt-in and expected: `code-graph` (16), `mcp-endpoint` (6),
`graph-rag` (6), four more gated on `cfg(all(...))` combinations of those, plus
`legacy-network` and `code-embed`. Note the compound gates — a naive grep for
`#![cfg(feature = "x")]` misses `#![cfg(all(feature = "x", feature = "y"))]`, and misses
feature names containing underscores.

**The one that is not opt-in: `internal-tests`.** 15 files, ~202 `#[test]` functions —
all time-travel/`AS OF`, encryption, materialized-view, branch-merge, protocol-integration
and REPL-tenant coverage. `internal-tests = []` (Cargo.toml) is not a default feature and
is enabled by no workflow and no command in this document's history, so those 202 tests
have never run in any gate. **They also no longer compile**: `cargo build --features
internal-tests --tests` fails with `E0063` missing-field errors in struct literals whose
structs gained fields in 2026-02 and 2026-04. Six months of unobserved rot.

```bash
cargo test --features internal-tests --tests --no-fail-fast   # currently fails to build
```

**Capture:** found 2026-08-16 while gating the RLS projection fix. That change altered
`handle_filtered_scan`'s `AS OF` handling, so a time-travel stage was added to the gate
specifically to cover it. The stage reported five suites `ok` — while executing zero
tests. Without the empty-suite check the change would have shipped with the coverage it
was gated on being imaginary. Two further files (`explain_tests.rs`,
`week6_visual_realtime_tests.rs`) turned out to be gated on features that were never
declared in `Cargo.toml` at all — permanently unbuildable, each holding a single
`assert!(true)` — and were deleted.

### 4. `cargo test --doc` — release CI parity
Doc tests are a release-gate in CI; a broken example blocks the tag. Cheap; run with the
full suite.

### 5. `cargo fmt --all -- --check` — changed-files standard
The repo carries pre-existing repo-wide drift (and `cargo fmt --all` touches ~26
unrelated files, some owned by other sessions' in-flight work). The enforceable local
standard: **your changed files are fmt-clean; you don't reformat files you didn't
change** (it invites cross-session merge conflicts).

### 6. `cargo clippy --all-targets -- -D warnings` — new-findings-only, with a trap
**Trap:** Nano inherited HeliosDB-Lite's strict lint profile in the *tracked*
`.cargo/config.toml` (unwrap_used, indexing_slicing, pedantic, nursery — since the fork
commit `eef110c`). Locally this makes the naive invocation report ~88 pre-existing error
kinds / ~1,767 sites AT ANY COMMIT. CI masks it because an exported `RUSTFLAGS` env
overrides config-file rustflags. Owner decision pending (adopt or remove).
**The workable gate:** set-diff your tree against the merge base under the identical
environment — zero NEW findings passes:
```bash
git stash && cargo clippy --all-targets -- -D warnings 2>&1 | grep '^error' | sort | uniq -c > /tmp/base.txt
git stash pop && cargo clippy --all-targets -- -D warnings 2>&1 | grep '^error' | sort | uniq -c > /tmp/mine.txt
diff /tmp/base.txt /tmp/mine.txt
```
**Capture:** Wave 1 shipped with a net −1 finding vs baseline under a profile far
stricter than the operative gate.

### 7. pg35 benchmark — the regression net that unit tests are not
35 SQL categories vs live PostgreSQL, 300 iterations (the 20-iter default is too noisy
for decisions), load-gated, PG 18.4 container:
```bash
docker run -d --name pg_bench_nano -e POSTGRES_USER=bench -e POSTGRES_PASSWORD=benchpass \
  -e POSTGRES_DB=benchdb -p 25433:5432 postgres:18.4-bookworm
PG35_ITERS=300 cargo test --release --test pg35_benchmark -- --nocapture --ignored
```
Compare category-by-category against the last recorded table
(`docs/benchmarks/PG35_BENCHMARK.md` + the status file's A/B history). Any category
regression ⇒ the offending commit is fixed or reverted before the next lands.
**Capture (the reason this gate is non-negotiable):** commit `10862ed` wired
normalization ahead of the raw result cache. Every unit test stayed green. pg35 showed
stable-text LIKE at **4.54 ms vs the 9.84 µs baseline — a 460× regression** — fixed the
same day in `6957cd4`. Run it after every perf-touching task.

### 8. `bench-engines.sh` with `PROTOCOLS` — measure the path drivers actually use
```bash
PROTOCOLS="simple extended prepared" DUR=8 CLIENTS="1 8 16 32 64" \
  ./docs/benchmarks/bench-engines.sh <ver:binary> [<ver:binary>…]
```
pgbench point-read TPS per query protocol, multi-binary A/B in one run.
**Capture:** until 2026-07-16 the harness only ever drove *simple* protocol — which is
why the extended/prepared path's global-mutex serialization (every parameterized read
took `current_transaction.lock()`) was invisible in every prior benchmark while being
the path every production driver uses. A gate can only catch what it measures; when a
workload class matters, add a cell for it.

### 9. `benches/public/ci_perf_smoke.sh` — the cliff-catcher
Fails if any workload is >2.5× slower than `benches/public/ci_baseline.json`
(N=1000/M=200 — keep these caps on this host). It catches cliffs, not drift; the <3%
cumulative-degradation budget is enforced via pg35/bench-engines A/Bs.

### 10. Interface coverage — no orphan features, no magic numbers
Every new function/feature must be reachable through a user-facing interface (CLI /
config / SQL / HTTP / wire) and tunable — new thresholds become config parameters
(`config.example.toml` style), not hardcoded constants. Review question at commit time:
"how would a user turn this off / tune this / observe this?"

### 11. Adversarial review (agent campaigns) — before any gate runs
Implementation by one agent, then two independent reviewers (correctness-adversarial +
compile/type), then a fix pass on blockers/majors. Cheap relative to a gate cycle.
**Capture:** the W1.4 reviewer caught out-of-range `$n` conjuncts being silently dropped
— unfiltered rows instead of an error — before anything compiled. The class (silent
wrong results) is precisely what test suites miss when nobody thought to write the test.

## Merge / release criteria

- One reviewable unit per commit on a wave branch; **wave gate** (suites + doc + clippy
  set-diff + pg35 + bench A/B + smoke) before merging the branch to main.
- Provenance discipline for gate failures: prove pre-existing vs introduced (run the
  exact failure at the merge base — a throwaway detached worktree keeps it clean:
  `git worktree add --detach /tmp/base <merge-base>`), then fix or file. Do not `#[ignore]`
  correctness bugs to get green.
- Merge ≠ push ≠ release: each is separately authorized. Release = tag → release CI →
  crates.io; the release gate flakes (dep-download / vector-index test) —
  `gh run rerun --failed`, never re-tag.
