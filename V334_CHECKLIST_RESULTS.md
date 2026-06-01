# HeliosDB-Nano v3.34 Fix Checklist — Cross-Agent Results

Two independent coding agents (**Claude Code** in `/home/gpc/HDB/Nano-CC/HeliosDB-Nano`
and **Codex** in `/home/gpc/HDB/Nano-CODEX/HeliosDB-Nano`) worked the same 13-item
checklist item-by-item from a shared baseline, each baselining/fixing/testing
independently, then comparing and integrating the agreed best version here.

- **Final integration repo:** `/home/gpc/HDB/Nano`, branch `main`, **ahead 13 of
  `origin/main`, NOT pushed** (staged for human review).
- **Base commit:** `37238cc` (v3.34.0).
- **Per-item result files:** `claude_results/<ITEM>.md`, `codex_results/<ITEM>.md`.
- **Per-item regression tests:** `tests/v334_*.rs` (+ Codex's
  `a14_postgres_transaction_error_recovery.rs`, `a15_postgres_binary_int4_transaction.rs`).

## Outcome tally

- **10 REAL bugs fixed:** A8, A14, A15, A5, A1, A7, T2, T8, A4, A11
- **3 genuinely already-fixed** (regression tests only, no source change): T3, T4, A10
- The checklist's opening triage (3 real / 7 already-fixed / 3 unconfirmed) was
  badly off: **6 of the 7 "already-fixed" guesses were real bugs**, and several
  *root-cause descriptions* were wrong (see notes).

## Final verification (this repo)

- `cargo test --lib`: **1819 passed, 1 failed, 1 ignored** — the one failure is
  the pre-existing, unrelated `vector::hnsw_index::tests::test_vector_count_tracking`
  (present on base `37238cc`).
- All 15 per-item regression test binaries: **green**.
- `cargo fmt --check`: clean. `cargo clippy --lib`: only the pre-existing
  `src/replication/streaming.rs:69` `unwrap_used` deny (on base).

## Summary table

| # | Item | Verdict | Final commit | Integrated impl | Decisive catch | Quality (Claude / Codex) |
|---|------|---------|--------------|-----------------|----------------|--------------------------|
| 1 | A8 | Real | `5b29224` | Codex (Claude ported + broader tests) | **Codex** — extended-protocol recovery gap | 93 / 88 |
| 2 | A14 | Real | `ddb651b` | Claude approach (BEGIN-time recovery) | **Claude** — Codex's first fix regressed 25P02 atomicity | 92 / — |
| 3 | A15 | Real | `49cbd39` | Codex base + Claude (uuid/bytea, client test) | converged; Codex ported Claude's tokio-postgres test | 89 / 95 |
| 4 | A5 | Real | `e32d58d` | Codex | **Codex** — CHECK skipped on the *parameterized* write path | (corrected) / 94 |
| 5 | A1 | Real | `6566104` | Codex (confined marker) + Claude's IN guard | **Claude** — naive fix changed `IN($1)` semantics | (superseded) / 94 |
| 6 | A7 | Real | `65b3629` | Codex | **Codex** — ART path-compression (not `encode_key`); needed wide stress | (corrected) / 95 |
| 7 | T2 | Real | `6cf1566` | Codex (4 fast paths) | Claude's compare-prompt → **Codex** found `COUNT(id)` hole | 91 / 94 |
| 8 | T3 | Already-fixed | `51591d2` | tests only (Codex superset) | both confirmed deep (recursive CTE / UNION) | 88 / 90 |
| 9 | T8 | Real | `6d46a67` | converged identical (Claude compact + union tests) | both — *general* GROUP-BY-no-agg, not MV-only | 91 / 93 |
| 10 | A4 | Real | `4546be5` | Codex (exact-length, +1184) + Claude conversion | Claude conversion + 1184 idea; **Codex** exact-length | 90 / 94 |
| 11 | T4 | Already-fixed | `da4c591` | tests only (Codex superset) | both; Codex found adjacent `IF NOT EXISTS` bug | 86 / 90 |
| 12 | A11 | Real | `3ba2015` | Codex | **Claude flagged** `DO UPDATE … WHERE excluded`; **Codex** fixed | (corrected) / 92 |
| 13 | A10 | Already-fixed | `1fdf2e1` | tests only (Codex superset) | both confirmed deep (NULL/param/coercion) | 90 / 90 |

## Per-item detail

### A8 — connection wedge after constraint error — REAL — `5b29224`
- **Root cause:** the extended-query path emitted `ReadyForQuery` before the
  client's `Sync` on an Execute-time error, closing/wedging the driver. (The
  simple-query path + the acceptor semaphore were already sound.)
- **Claude:** verified simple path + acceptor; *initially* mis-verdicted as
  already-fixed (tested only simple protocol). Branch `d460fc2`/`38a9183`.
- **Codex (`6d16cf1`):** found the extended-protocol gap; the integrated fix is
  the `awaiting_sync_after_error` recovery (discard-until-Sync).
- **Tests:** `v334_a8_connection_resilience.rs` (Claude, simple+extended+acceptor),
  `a14_postgres_transaction_error_recovery.rs` adjacency.

### A14 — spurious "Transaction already active" — REAL — `ddb651b`
- **Root cause:** in-transaction error left the engine txn active; a follow-up
  `BEGIN` called `db.begin()` on the live txn → "already active".
- **Codex (`c425a21`):** first fix rolled back at *error-time* → **Claude caught**
  this dropped PostgreSQL's 25P02 "transaction aborted" semantics (post-error,
  pre-ROLLBACK statements would silently autocommit).
- **Reconciled (`ddb651b`):** BEGIN-time recovery (Claude's approach: error marks
  `Failed`, BEGIN rolls back the aborted txn then begins) **plus** Codex's
  extended failed-state 25P02 guard.

### A15 — binary result format ignored — REAL — `49cbd39`
- **Root cause:** extended result path always emitted text DataRows, ignoring the
  client's binary result-format request (`requested 4 remaining 1`). Checklist's
  "Bind frame-length offset" was a misdiagnosis.
- **Converged:** Codex base (`18e325c`) with uuid/bytea coverage + Describe-Portal
  format fix; **Claude** contributed the tokio-postgres client regression (which
  Codex ported) and flagged the bytea Describe/DataRow consistency.

### A5 — CHECK constraints not enforced on writes — REAL — `e32d58d`
- **Root cause:** the **parameterized** write path (`execute_params`) skipped
  CHECK enforcement; the simple `db.execute()` path enforced it.
- **Claude:** initial verdict "already-fixed" (tested only `db.execute`) — **wrong**.
- **Codex (`20cbf89`):** caught the param-path bypass; `validate_check_constraints`
  at all four parameterized write sites. Claude independently reproduced.

### A1 — `= ANY($array)` returns 0 rows — REAL — `6566104`
- **Root cause:** planner only rewrote literal-array casts; a param array (and
  `ARRAY[...]`) fell through to constant-`false`.
- **Claude:** working fix, but **discovered it changed `IN ($1)` with an array
  param** (0→2 rows) — an unwanted semantic change.
- **Codex (`9dc5d28`):** confined the expansion to a `__hdb_any_array` marker and
  **ported Claude's IN-guard test**, so `IN` semantics are untouched.

### A7 — UNIQUE TEXT false duplicate — REAL — `65b3629`
- **Root cause:** NOT `encode_key` (uses full bytes). ART inner nodes store only
  the first `MAX_PREFIX_LEN` prefix bytes; a mismatch in the hidden tail routed
  distinct keys to the wrong child → false UNIQUE duplicate after many
  long-shared-prefix keys.
- **Claude:** 2-row test passed → mis-verdicted already-fixed.
- **Codex (`69bacf9`):** wide stress (100 shared-prefix keys, fails at insert 20)
  surfaced it; fix verifies the hidden prefix tail against a representative leaf.
  Claude reproduced.

### T2 — COUNT(*) over a materialized view returns 0 — REAL — `6cf1566`
- **Root cause:** count fast paths used the user-visible MV name, not the
  `__mv_<name>` backing table.
- **Claude:** two-site fix (`COUNT(*)` + single-PK); compare-prompt asked "both
  sites?".
- **Codex (`1111f0e`):** that prompt surfaced that **`COUNT(id)`** was still 0
  (column-aggregate pushdown); fallible `fast_path_storage_table_name` across all
  four fast paths.

### T3 — CTE + `$N` parameter binding — ALREADY-FIXED — `51591d2`
- Both confirmed with deep coverage (inside body, outer, multi-param, joined,
  recursive CTE with params, CTE in UNION, multi-reference, stress). Codex
  superset test integrated.

### T8 — GROUP BY without aggregates doesn't dedupe — REAL — `6d46a67`
- **Root cause (general, not MV-only):** the planner built the aggregate/group
  plan only when aggregate functions were present, so `GROUP BY` with no
  aggregates silently dropped the GROUP BY (affects direct queries AND MVs).
- **Converged identical fix:** `has_aggregates || has_group_by` → aggregate path
  with empty aggr_exprs (dedup by key), NOT a blanket DISTINCT. Claude's compact
  source + union of both agents' tests.

### A4 — binary TIMESTAMP/UUID parameter inputs — REAL — `4546be5`
- **Root cause:** `decode_binary_parameter` lacked OID 1114/2950 arms → fell
  through to `Value::Bytes` → cast failure.
- **Converged:** Codex (`15ce31c`) exact-length `1114|1184` + `2950`; **Claude**
  contributed the `from_timestamp_micros` PG-epoch conversion and the
  `timestamptz` (1184) union; Codex's exact-length check (reject overlong) > the
  lenient slice.

### T4 — CREATE BRANCH AS OF NOW empty name — ALREADY-FIXED — `da4c591`
- Both confirmed the name is recorded across all AS OF forms (incl. AS OF
  TIMESTAMP + 50-branch stress). Codex superset test integrated.
- **Adjacent NEW bug found (out of scope):** `CREATE BRANCH IF NOT EXISTS` —
  see `BUGS_CREATE_BRANCH_IF_NOT_EXISTS.md`.

### A11 — ON CONFLICT DO UPDATE excluded refs — REAL — `3ba2015`
- **Root cause:** the narrow `SET col = excluded.col` worked, but
  `ON CONFLICT DO UPDATE … WHERE <predicate>` **dropped the predicate** in the
  planner → conflict updates always ran.
- **Claude flagged** `DO UPDATE … WHERE excluded.col` as a probe (but didn't test
  it). **Codex (`fe030ec`)** tested it (false predicate → affected=1), and fixed:
  `OnConflictAction::DoUpdate` gains `selection`, evaluated (with excluded refs)
  before assignments; false/NULL skips.

### A10 — IS DISTINCT FROM — ALREADY-FIXED — `1fdf2e1`
- Both confirmed deep: full NULL truth table, IS NOT DISTINCT FROM, WHERE, param
  path, string/int coercion, `ON CONFLICT … WHERE IS DISTINCT FROM excluded`,
  300-row stress. Codex superset test integrated.

## Methodology notes / lessons

- **Test the real client path.** A5, A7, and A11 all flipped from "already-fixed"
  to real bugs only when tested on the **parameterized / at-scale** path (not
  `db.execute()` happy paths). After A5/A7, every already-fixed candidate (T3, T4,
  A11, A10) was baselined on the param/deep path — A11 still flipped.
- **Adversarial cross-agent comparison earned its keep on every item** — each was
  improved or corrected in one direction or the other; the checklist's stated
  root cause was wrong on A14, A15, A7, and incomplete on T2, T8.
- **Pre-existing, untouched issues** (on base `37238cc`): the `hnsw`
  `test_vector_count_tracking` lib failure and the `replication/streaming.rs:69`
  clippy `unwrap_used` deny.

## Follow-ups

- `BUGS_CREATE_BRANCH_IF_NOT_EXISTS.md` — `CREATE BRANCH IF NOT EXISTS` misparses
  the name.
- Pre-existing `hnsw` test failure and `streaming.rs:69` clippy deny (separate
  from this checklist).
- A4 output-side binary encoding for non-scalar types (numeric/temporal/json)
  remains text-only (documented in `claude_results/A15.md`).
