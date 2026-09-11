# HeliosDB Nano

Single-binary embedded database in Rust (crate `heliosdb-nano`, workspace incl. `bindings/python`).
PostgreSQL- and MySQL-wire compatible, with TDE/ZKE encryption, HNSW vector search, git-like
database branching, time-travel (`AS OF`) queries, materialized views, RLS, HA replication, and a
built-in REST/Auth/Realtime (BaaS) HTTP layer. Default cargo features:
`encryption, vector-search, ring-crypto, ha-tier1`.

See `AGENTS.md` (repo root) for the full operations reference and the per-topic skills under
`.claude/skills/heliosdb-nano-*/` — especially `heliosdb-nano-merge-validation` (the 8-phase
pre-merge methodology this repo requires for engine changes).

## Build & Test

- Build: `cargo build --release` (feature recipes in README "Building from Source")
- Unit tests (~1700+): `cargo test --lib`
- Integration tests (~1500+): `cargo test --tests -- --skip ha_tests::streaming_tests --skip lock_management`
  (the `--skip` filters must come after `--`; those two skips are documented pre-existing flakes
  on constrained runners — never add new skips)
- Doc tests (release CI gate): `cargo test --doc`
- Internal tier (mandatory, local only): `cargo test --features internal-tests --tests -- --skip ha_tests::streaming_tests --skip lock_management`
- HA-touching changes (src/storage/wal.rs, src/replication/, src/storage/lock_manager.rs,
  src/storage/lockfree/): also `cargo test --features ha-tier1 --test ha_integration`
- Python binding smoke test: `bindings/python/tests/test_smoke.py` (maturin build; not in CI)

**What CI actually runs, and what it does not.** `release.yml` triggers only on a `v*` tag
and runs `cargo test --locked --lib` + `--doc`. `perf-gate.yml` runs the perf smoke on PR.
That is all. **`cargo test --tests` has no automated execution anywhere** — the ~1500
integration tests run only when a human or agent runs them locally, which is why the gate
discipline above is load-bearing rather than ceremonial.

**49 files in `tests/` run ZERO tests under the default command** (see `docs/GATES.md` §3b).
Most are legitimate opt-ins (`code-graph`, `mcp-endpoint`, `graph-rag`). The exception is
`internal-tests`: 16 files, 232 tests covering time-travel/`AS OF`, encryption,
materialized views, branch merge, protocol integration and REPL tenant commands. It is not
a user-facing feature and the default command never runs it, so it is a **mandatory extra
tier of the gate**, not an opt-in:
`cargo test --features internal-tests --tests -- --skip ha_tests::streaming_tests --skip lock_management`
(last green 2026-09-04 on the v4.30.0 gate: 5553 passed / 0 failed across 283 suites, of
which the 16 gated files contributed 232 / 0). It compiles again since the v4.23.0 CI work (2026-08-31);
it did NOT compile between 2026-02 and 2026-08-17, which is how that coverage rotted
unobserved. Touching storage, time travel, branches, MVs, encryption or protocol? Run this
tier and cite its numbers — do not imply the default suite covered it.

**A suite reporting `ok. 0 passed; 0 failed; 0 ignored` ran nothing.** Treat it as a gate
FAILURE, not a pass. Grep every full-suite log:
`awk '/^     Running/{s=$2} /^test result: ok\. 0 passed; 0 failed; 0 ignored/{print "EMPTY:", s}'`

## Quality Gates (mandatory — every change must pass ALL before commit/merge)

Per-gate rationale, exact invocations, and real failure captures: `docs/GATES.md`.

1. **Full test suite passes**: `cargo test --lib && cargo test --tests -- --skip ha_tests::streaming_tests --skip lock_management && cargo test --doc`,
   PLUS the internal tier `cargo test --features internal-tests --tests -- --skip ha_tests::streaming_tests --skip lock_management`.
   No new skipped/`#[ignore]`d tests without a written justification in the commit message.
2. **No regression**: any behavior change must keep all existing tests green; add tests for every
   new code path (see merge-validation Phase 2: test the matrix, not just the happy path).
3. **Benchmarks**: run the perf gate `benches/public/ci_perf_smoke.sh` — it FAILS if any
   workload is more than `PERF_GATE_THRESHOLD` (default 2.5x) slower than the recorded
   baseline in `benches/public/ci_baseline.json` (a cliff-catcher, not a drift gate). CUMULATIVE
   performance degradation across a work session must stay under 3% vs baseline. For
   feature-specific work also run the matching criterion bench: `cargo bench --bench <name>`
   (art_index_bench, vector_search_bench, branch_performance, phase3_benchmarks,
   multi_tenancy_bench, predicate_pushdown_bench, encryption_benchmark, simd_benchmark,
   time_travel_optimization; feature-gated: conflict_detection_bench / sync_benchmark /
   mv_incremental_bench need `sync-experimental`; with_context_bench / linker_precision need
   `graph-rag,code-graph`). Reference numbers live in `perf/BASELINE_2026_06_10.md` (raw runs in
   `perf/baseline_runs/`). If no applicable baseline is recorded yet, the FIRST task of any
   implementation session is to record one: CI-gate numbers via `benches/public/regen_ci_baseline.sh`
   (writes `benches/public/ci_baseline.json`), full-suite numbers into a new dated
   `perf/BASELINE_<YYYY_MM_DD>.md` following the existing file's format.
4. **Lint gates**:
   - `cargo fmt --all -- --check`
   - `cargo clippy --all-targets -- -D warnings` (repo alias: `cargo clippy-all` adds
     `--all-features`; note fips+ring-crypto may conflict under --all-features)
   - `cargo deny check` (deny.toml: RustSec advisories, license allowlist, openssl banned)
5. **Interface coverage**: every new or changed function/feature must be reachable through at
   least one user-facing interface (CLI flag/subcommand, config.toml parameter, SQL surface,
   HTTP endpoint, or wire protocol) and be tunable — no new hardcoded magic numbers; expose
   thresholds/sizes as config parameters or CLI flags (see `config.example.toml` for style).

## Interfaces

- **CLI** (`src/main.rs`, clap): `heliosdb-nano start | stop | status | init | repl | dump | restore`
  plus `code-graph hook` (only with `--features code-graph`).
  Key `start` flags: `--data-dir/--memory`, `--port` (5432), `--listen`, `--config`, `--daemon`,
  `--pid-file`, `--dump-on-shutdown`, `--dump-schedule`, `--tls-cert/--tls-key`, `--auth`
  (trust|password|md5|scram-sha-256), `--password`, `--replication-role`, `--replication-port`
  (5433), `--primary-host`, `--standby-hosts`, `--observer-hosts`, `--sync-mode`, `--http-port`
  (8080, 0 disables), `--http-listen`, `--mcp-token`, `--allow-remote-mcp`, `--node-id`,
  `--mysql`, `--mysql-listen` (127.0.0.1:3306), `--mysql-socket`, `--pg-socket-dir`,
  `--max-connections` (100).
- **Config file** (`config.example.toml`, pass via `start --config`): top-level `profile`
  (safe|balanced|fast|agent) and sections `[storage]` (wal_sync_mode, cache size, compression,
  time_travel, timeouts, isolation, slow-query threshold, durable_commit), `[encryption]` +
  `[encryption.key_source]`, `[server]`, `[performance]`, `[audit]` + `[audit.capture_metadata]`,
  `[optimizer]`, `[authentication]`, `[compression]`, `[materialized_views]`, `[vector]`
  (default_index_type, hnsw_ef_construction, hnsw_m, enable_pq, pq_subvectors, pq_bits),
  `[session]`, `[locks]`, `[dump]`, `[resource_quotas]`.
- **Wire protocols**: PostgreSQL 5432 (+ Unix socket `<dir>/.s.PGSQL.<port>`), MySQL 3306
  (+ Unix socket), WAL-streaming replication port 5433.
- **HTTP (port 8080)**: `GET /health`, `GET /version`; PostgREST-style `/rest/v1/<table>` and
  `/rest/v1/rpc/<function>`; Swagger UI `/docs` (spec at `/openapi.json`); BaaS auth
  `/auth/v1/signup`, `/auth/v1/authorize?provider=…` (+ token/refresh/user/logout); realtime
  WebSocket `/realtime/v1/websocket`; MCP-over-HTTP JSON-RPC routes `/mcp`, `/mcp/info`,
  `/mcp/sse` (`mcp-endpoint` feature, gated by `--mcp-token`). Note: this build has NO
  `mcp serve` subcommand — MCP mounts on the HTTP listener (stdio/Unix-socket MCP servers
  exist only as library APIs in `src/mcp/`).
- **REPL** meta-commands: `\d \dt \dS \dmv \branches \use \snapshots \stats \compression
  \optimize \indexes` etc. (`src/repl/`).
- **Library APIs**: embedded Rust crate (`heliosdb_nano::EmbeddedDatabase`) and Python binding
  `heliosdb-nano-embedded` (`bindings/python`, maturin/pyo3).
- No GUI beyond Swagger UI.

## Resource Constraints

This host crashed ~16h ago (suspected OOM) and runs production-like services. Therefore:

- Run at most ONE heavy cargo build / test run / benchmark at a time — never in parallel;
  don't launch a bench while a build or test run is still going.
- Cap benchmark dataset sizes to what the recorded baselines used (CI gate: N=1000/M=200;
  full TPS suite: N=10000/M=2000). Do NOT use the `HELIOSDB_*_BENCH_ROWS` scale-up env vars
  or custom `SCALES=` this session.
- NEVER touch `/home/gpc/heliosdb-ada-data`, any `/data` directories (including the repo's
  `data/` dir), or running heliosdb services/containers.
- Prefer `cargo test --lib` first (cheapest signal) before the integration suite.

### Bounded benchmark invocation (mandatory — root cause of the 2026-07-08 host crash)
A runaway benchmark (38 GiB RSS) livelocked this host for 16h. Run ANY heavy benchmark or load-generating process in a bounded scope so it dies alone instead of taking the host down:
```bash
systemd-run --user --scope -p MemoryMax=24G -p MemorySwapMax=0 <bench command>
```
Full incident report: /home/gpc/HDB/sprint/status/incident-2026-07-08.md
