# Next Perf Batch — Executor Handbook

Companion to `NEXT_PERF_BATCH_2026_07.md` (roadmap v2). That doc says *what and why*;
this one carries every load-bearing detail an executor needs to implement at full
potential without re-deriving the analysis: verified anchors, invariants, traps, test
recipes, and the exact gate methodology this campaign used for six merged milestones.

**Anchor policy:** line numbers below were verified at v4.0.0+ (`main` ≥ `62ecb83`) and
WILL drift — always re-locate by the **symbol name** (grep the quoted `fn`/const) before
editing; treat line numbers as hints only.

---

## 0. Ground truth at handoff (2026-07-05, v4.0.0 released)

- **Released:** `heliosdb-nano 4.0.0` live on crates.io + GitHub release; tag `v4.0.0`.
- **Suite baselines (what "green" means):** `cargo test --locked --lib -- --test-threads=2`
  = **1997 pass / 0 fail / 3 ignored** (ignored are documented pre-existing:
  `protocol::postgres::catalog::tests::test_handle_query_information_schema_columns`,
  `wire_tests::probe_w1_*`, `probe_w2_*`).
- **Known pre-existing failures — do NOT chase these, do NOT count as regressions:**
  1. `fk_validation_modes::bulk_load_mode_setting_reaches_storage_engine`
     (tests/fk_validation_modes.rs:89, `SET bulk_load_mode` plumbing never worked —
     reproduced on 68e814a in a throwaway worktree during the M3 gate).
  2. `tests/protocol_tests/test_postgres.py` step 7: `Table 'helios_sessions' does not
     exist` — dead schema, never provisioned on any version (C17, deferred; needs a live
     session-tracking feature, `SessionRegistry` in src/sql/system_tables.rs has zero
     callers).
- **Perf standings (paired, order-swapped, this host):** indexed point-read
  14.5k/137k/169k/172k TPS @ c=1/16/32/64 (1.7–2.3× PG); SELECT 1 ~26k/245k (≈2.5× PG);
  COPY 76/227/423 ms @10k/50k/100k (PG 82/106/133); DROP 100k ~135 ms; nextval-bound
  INSERT ~2,000 TPS @ c=16-32.
- **Baseline binary for future A/B:** the old stash
  `perf/baseline_runs/bins/heliosdb-nano-baseline-main-68e814a` **hangs on
  `ALTER TABLE … RENAME`** (pre-M1 bug) — every gate that used it needed a RENAME-free
  fixture. **First action of the next batch: build and stash a v4.0.0 baseline**
  (`git worktree add /tmp/nano-v4 v4.0.0 && cargo build --release` there, copy to
  `perf/baseline_runs/bins/heliosdb-nano-baseline-v4.0.0`) and A/B against THAT.
- **A2 normalizer state:** wired into `query_with_columns` ONLY. `query()` is
  deliberately un-normalized — it is the differential oracle's raw reference. Keep it
  that way. Kill switch env: `NANO_DISABLE_QUERY_NORMALIZATION` (read once via OnceLock —
  set it per-process, not per-query).

## 1. Cross-cutting invariants and traps (the "do NOT" list)

1. **Wire-path rule:** anything touching `src/protocol/`, the catalog, the planner, or
   the caches MUST be validated over psycopg, not just embedded — embedded tests miss
   `src/protocol/postgres/` entirely. Server for tests:
   `./target/release/heliosdb-nano start --auth trust --http-port 0 --port 20000
   --data-dir <fresh>`; client `tests/protocol_tests/venv/bin/python` (psycopg2 2.9.11
   installed); DB name `heliosdb`, any user under trust.
2. **Sequences publication order is load-bearing** (`src/sql/sequences.rs`, comment at
   the refill: "PUBLICATION ORDER IS LOAD-BEARING"): store `next` BEFORE widening
   `block_end`; the durable high-water fsync (`persist_high_water`) must complete BEFORE
   the window is published. Never reorder; never move the fsync after publication.
   The refill mutex may NOT be released before `persist_high_water` returns unless you
   implement the full forward-only publication protocol (direction-aware CAS-max) — the
   naive "fsync outside mutex" re-serves values on interleave. (This is why the campaign
   chose the CACHE-32 default instead.)
3. **Row-cache commit fence:** written rows must leave the row cache BEFORE
   `end_commit` lifts the snapshot barrier (`src/storage/transaction.rs`, comment
   "Stale-row-cache fence") — it is a lost-update fence, observed 10-50% lost updates
   without it. D3 sharded the lock; the ORDER is unchanged and must stay.
4. **bincode enum tags:** `WalOperation` (src/storage/wal.rs) is bincode-encoded by
   variant INDEX — new variants go at the END of the enum only (see the
   `RenameTable` comment there). Same logic applies to any persisted bincode enum.
5. **`cache_admits()` is shared state** (src/lib.rs): one admission decision per cold
   query, consumed by BOTH the plan-cache and result-cache inserts. If you add a new
   cache-insert site, take ONE decision and reuse it — two calls for the same SQL
   admit-then-churn.
6. **Normalizer typing oracle:** parameter `Value`s MUST be produced via
   `Planner::number_literal_to_value` / `Planner::single_quoted_content_to_value`
   (src/sql/planner.rs, extracted precisely so the `$n` and inline-literal paths cannot
   drift). Never hand-roll literal typing in normalize.rs.
7. **Normalized path must NOT populate the result cache** (params vary per call; only
   the plan is reusable). It currently doesn't — keep it that way.
8. **`ColumnStorageMode::Columnar` is NOT a free default:** true-columnar storage
   replaces row-blob values with `ColumnarRef` sentinels
   (grep `ColumnarRef` in src/storage/engine.rs, ~10387-10402) and a point read then
   decodes a whole 1024-row batch per column (`ColumnarStore::get`,
   src/storage/columnar.rs ~932-946, `BATCH_SIZE` = 1024 at ~:100) — flipping the default
   regresses the just-won OLTP lead. The OLAP item (#3) is side-copies for this reason.
9. **COPY fast path is fallback-gated:** `copy_bulk_insert` (src/lib.rs:~6378) returns
   `None` → the handler falls back to the generic SQL path. Any new gate you add must
   return `None`, never error, for shapes the generic path can still serve. Current
   gates: session/global txn, savepoints, RLS/tenant, active branch, triggers, FK/CHECK
   (via `fast_literal_insert_spec`), dictionary/CAS storage, `fast_dml_requires_logical_wal`.
10. **pg35 is timing-only** (closures discard rows, `let _ = db.query(...)`) — it is an
    erosion gate, NOT a correctness oracle. Correctness proof for query-path changes is
    the differential oracle pattern + row-asserting integration tests.
11. **Shared host etiquette:** port 25433 hosts the long-lived `codex-pg184-bench` PG
    container (shared with other sessions) — WAIT for it, never kill. ~7 unrelated
    heliosdb-nano service processes run on this box — only kill servers YOU started (by
    PID, not pkill by name). Use ports 5490+/20050+ for scratch servers, fresh
    `--data-dir` under the session scratchpad, and remove them after.
12. **`.claude/scheduled_tasks.lock` and `tests/protocol_tests/venv/**` churn** in
    `git status` is ambient noise — never commit it; stage files explicitly.

## 2. Gate methodology (what "done" means — used for all six merged milestones)

**Regression gate** (run per milestone, machine may be busy):
```bash
cargo test --locked --lib -- --test-threads=2        # expect ≥1997/0/3
cargo test --test <touched-area suites>              # 0 fail (see per-item lists)
# wire path (if applicable):
./target/release/heliosdb-nano start --auth trust --http-port 0 --port 20000 --data-dir /tmp/gate-$$ &
tests/protocol_tests/venv/bin/python tests/protocol_tests/test_postgres.py   # step 7 pre-existing-fails
tests/protocol_tests/venv/bin/python tests/protocol_tests/test_copy.py       # must be 7/7
benches/public/ci_perf_smoke.sh                      # 12/12, 2.5x headroom gate
```
pg35 erosion check when the query path changes:
`PG35_CONNSTR=<pg conn> PG35_ITERS=100 cargo test --release --test pg35_benchmark -- --nocapture --test-threads=1`
(env gates verified in tests/pg35_benchmark.rs: `PG35_CONNSTR`, `PG35_ITERS`,
`PG35_PG_LABEL`; compare vs `perf/v358_program/pg35_category_history.json`, tracker
`perf/v358_program/pg35_track.py`; do NOT persist a new history snapshot from a gate run).

**Scalability gate** (quiet-ish machine, docker available):
```bash
cd docs/benchmarks && ./bench-engines.sh base:<v4.0.0-bin> new:<branch-bin>
```
- **One run is NOT evidence** on this host: PG's own numbers drift 5–40% between runs;
  apparent regressions on SELECT 1/COPY/DROP have reversed under order-swap in FOUR
  separate gates. Any eroded-looking cell ⇒ paired order-swapped retest (baseline↔new
  back-to-back, ×2 rounds) before calling it real. Effect sizes ≥2× survive the noise;
  ±10% cells need pairing.
- Durable-write microbench: `HELIOS_DURABLE=1 HELIOS_DURABLE_WINDOWS=200,1000
  HELIOS_DURABLE_M=200 cargo test --release --test tps_workloads
  run_durable_commit_bench -- --nocapture --test-threads=1`.
- General TPS suite knobs: see `perf/SUMMARY.md` §Reproduce.

**Merge convention:** feature branch → PR with BOTH gate reports in the body → merge
commit (repo allows, no branch protection). Commits end with the Co-Authored-By line.
Commit messages via `git commit -F <file>` when they contain backticks/parens (shell
substitution mangled one message this campaign).

**Release flow:** bump `Cargo.toml` + `bindings/python/Cargo.toml` (workspace member) →
`cargo check --workspace` (refreshes Cargo.lock — the gate runs `--locked`) → CHANGELOG
`## [X.Y.Z] - date` entry (release notes are extracted from it by awk — keep the heading
format exact) → local gates (`--lib`, `--doc`, `cargo publish --dry-run --locked` — the
dry-run FAILS on untracked files; keep scratch gitignored) → commit → `git tag -a vX.Y.Z`
→ push tag. CI verifies tag==manifest version. If the release run flakes
(dep-download or a vector test): `gh run rerun --failed`, never re-tag.

---

## 3. Per-item execution specs

### #1 Normalizer widening (IN/BETWEEN/casts + arity padding) — S/low — DO FIRST

**Touch points** (all in `src/sql/normalize.rs` unless noted):
- `fn is_predicate_bail_kw` (~:333): remove `"in"`, `"between"` from the list. KEEP
  `"select"`, `"exists"`, `"any"`, `"all"`, `"some"`, `"values"`, `"array"`, `"case"`.
- `::`-cast bail (~:167, `if b == b':' && bytes[i+1] == b':' && in_where`): replace the
  bail with: read the cast target word; if it is in the WHITELIST
  {uuid,int2,int4,int8,int,bigint,smallint,integer,text,varchar,date,timestamp,
  timestamptz,numeric,decimal,float4,float8,real,boolean,bool} copy `::type` through
  verbatim (the PRECEDING literal token has already been parameterized to `$n`, so the
  output reads `$n::type`); otherwise bail as today. Only a cast IMMEDIATELY following an
  emitted parameter or an identifier is in scope — a cast after `)` (expression cast)
  should bail in v1.
- NEW `fn pad_in_list_params` logic inside the main loop: when inside `IN (`…`)` at the
  predicate level, after emitting the last element, pad to the next power of two by
  repeating the LAST param's placeholder — i.e. emit extra `,$k` (k = index of last
  param, do NOT push duplicate Values; repeated placeholder means the executor binds the
  same param twice — verify `fast_param_value` / `lookup_bound_value` allow the same
  index referenced twice; they do — params are read by index, not consumed). Bail above
  128 elements. Track "am I inside an IN-list" with a small state (depth at which `IN (`
  opened); nested parens inside an IN element (function call) must not confuse the
  element count — count elements by top-level commas at that depth only.
- **Executor prerequisites (verified, do not re-implement):** `pk_in_list_value`
  resolves `LogicalExpr::Parameter` (grep in src/sql/executor/mod.rs ~2317-2324); range
  bounds resolve params; `lookup_bound_value` unwraps `Cast{expr: Parameter}`
  (src/sql/executor/scan.rs ~:800).

**Oracle upgrade (write BEFORE wiring the lexer changes):**
- Extend `src/sql/normalize.rs::differential` corpus + `tests/query_normalization.rs`
  with: `IN (1)`, `IN (1,2,3)`, 33-element, 128-element, 129-element (must bail),
  `IN` with NULL element (`x IN (1, NULL)` — result excludes NULL matches per SQL
  semantics; raw==normalized is what matters), `NOT IN` with and without NULL element,
  duplicate elements, string IN-lists with `''` escapes, `BETWEEN 5 AND 10`,
  `BETWEEN 10 AND 5` (empty), `BETWEEN` on floats/strings/dates,
  `'…'::uuid` equality (assert the index probe still fires — reuse the
  `Index Point Lookup` EXPLAIN assertion pattern from tests/uuid_index_probe_explain.rs),
  `x::int = 5` (column cast — v1 target is literal casts; decide bail vs support and
  test whichever), `POSITION('x' IN name)` (must NOT be treated as an IN-list — the
  word `in` appears; your IN-detection must require it as a standalone keyword FOLLOWED
  by `(` after optional ws, and POSITION's `IN` is inside parens of a function call —
  add the explicit test), `expr IN (SELECT …)` (must bail via `select`).
- Property fuzz: generate N random predicates over a fixture table (columns of each
  type), assert raw==normalized rows for every accepted rewrite and no panic for every
  bail. Seed it deterministically (no Date::now in tests that gate).
- Plan-cache shape check: after sweeping arities 1..40 of the same query, assert
  `plan_cache` contains ≤ 7 normalized IN shapes (log₂ bound) — direct regression test
  for the padding.

**Gate:** standard + pg35 (query path changed) + psycopg suite. Perf claim to measure:
IN-list microbench (pgbench script `SELECT … WHERE aid IN (:a,:b,:c)` shape with random
literals) A/B vs v4.0.0 baseline.

**Rollback:** kill switch already global (`NANO_DISABLE_QUERY_NORMALIZATION`); the
widening is additive inside normalize.rs — revertible in isolation.

### #2 COPY → PG parity (elide-v: increment first, transient marker second) — L/medium

**Where the cost lives (verified at HEAD):** `insert_prepared_tuples_fast_batch`
(src/storage/engine.rs:~10330): `commit_ts` once per batch (~10374, only when
`time_travel_enabled`); then per row inside `if let (Some(ts), Some(reverse_ts))`:
`data:` put, `v:{table}:{row}` put with the FULL `logical_value` (byte-identical clone of
the row), `v_idx:{table}:{row}:{rev_ts:020}` put with value `ts.to_be_bytes()` (8B);
one `counter:` key per batch; columnar grouped writes appended after (~10472). The
measured A/B: time-travel on = 368-425 ms, off = 102-156 ms @100k — the version writes
ARE the remaining gap.

**Increment 1 — `HELIOS_COPY_ELIDE_V` (elide the `v:` value-duplicate only):**
1. In the fast-batch loop, when the env/config flag is on AND the batch is a COPY-origin
   insert (thread the flag through `copy_bulk_insert` → a new parameter or an
   engine-level bool; do NOT apply to generic multi-row INSERT in v1), skip the `v:` put;
   keep `data:`, `v_idx:`, counter.
2. AS-OF read path (src/storage/time_travel.rs): the point-in-time read seeks
   `v_idx:{t}:{row}:` (format comment at :653) then gets `v:{t}:{row}:{revts}`. Change:
   on `v:` MISS where the v_idx: seek shows this is the NEWEST version for the row
   (the first entry in reverse-ts order — the same seek result already tells you),
   serve the current `data:` value. On `v:` miss for a NON-newest version → data
   corruption error (must never happen if backfill is correct).
3. UPDATE/DELETE backfill: every path that writes a NEW version for a row must first
   ensure the prior version's `v:` exists. Insertion points = the three version-writing
   paths found by the D4 analysis: `put_versioned_batch`
   (src/storage/transaction.rs ~892-963), the branch-aware UPDATE
   (src/storage/engine.rs ~12242-12335), and `write_data_version_and_register_snapshot`
   (src/storage/time_travel.rs ~780-847). In each: point-get
   `v:{t}:{row}:{revts(prior_ts)}` — prior_ts recovered from the row's newest `v_idx:`
   entry (one prefix seek; its VALUE is the ts, verified) — and if missing, put it with
   the OLD row value (all three paths already have or can read the old value: the
   executor passes it / the path reads it for ART maintenance).
   NOTE: this adds one v_idx: seek + one conditional get per updated row on TT-on
   paths. Measure the UPDATE tps cost (tps_workloads update cycle) — budget ≤3%.
4. Tests: extend `tests/` with a dedicated `copy_time_travel.rs`: COPY 10k → AS OF
   before-COPY (0 rows), AS OF after (10k), UPDATE some rows → AS OF between COPY and
   UPDATE returns the ORIGINAL values (backfill proof), DELETE likewise, then restart
   the DB and repeat the AS-OF assertions (durability), then branch-from-timestamp over
   the same states. Gate additionally on: automatic_time_travel_tests, branch_as_of_tests,
   time-travel suites, crash suites, and the psycopg copy suite.

**Increment 2 — transient `vmeta:` marker (only if increment 1 measures short of
~PG parity):** design in roadmap v2 §2. Extra anchors: background worker pattern to copy =
SMFI rebuild scheduling (`suspend_smfi_for_bulk_load`, src/storage/engine.rs ~7979);
open-time scan = add to engine init near where