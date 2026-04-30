# Changelog

All notable changes to HeliosDB Nano will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [3.22.2] - 2026-04-30

### Fixed — cross-process `INSERT … ON CONFLICT` no longer duplicates rows

Closes `FEATURE_REQUEST_cross_process_on_conflict.md`. A second
process attaching to a KB written by a prior process and issuing
`INSERT … ON CONFLICT(col) DO UPDATE` (or any parameterised
`INSERT` via `db.execute_params`) silently inserted duplicate
rows — `Catalog::rebuild_all_indexes()` had populated the ART, but
the relevant write paths weren't consulting it.

**Root cause** — two divergences:

1. `EmbeddedDatabase::insert_tuple_versioned_with_schema`
   (the storage primitive used by `insert_tuple_branch_aware_with_schema`
   on the main branch and by `execute_plan_with_params_inner`'s INSERT
   arm) **did not call `check_unique_constraints`**. Its SQL fast-path
   sibling `insert_tuple_fast` already did.
2. `execute_plan_with_params_inner`'s `LogicalPlan::Insert` arm
   discarded `on_conflict` (`on_conflict: _`).

**Fix** (commit `6ec74d3`). Added the constraint check to
`insert_tuple_versioned_with_schema` (matching the fast-path twin)
and wired `on_conflict` through `execute_plan_with_params_inner` —
pre-check uniqueness, route `DoNothing` (silent skip) and
`DoUpdate` (UNIQUE-column scan / PK lookup → read existing tuple
→ resolve `EXCLUDED` refs and apply assignments via a
parameter-aware `Evaluator` → write back via `update_tuple_fast`).

New test `tests/cross_process_index_rebuild_tests::on_conflict_named_column_upserts_after_reopen`
reproduces the original FR repro and now passes alongside 6
existing cross-process tests + 10 in-lib `on_conflict` tests.

### Added — MCP server LRU cache stats in `helios/info` + `GET /mcp/info`

The discovery payload now includes a `cache` sub-object covering
the engine's MCP `result_cache` (server-side LRU for read-only
`tools/call` results, 256-entry, 5-minute TTL):

```json
{
  "cache": {
    "size": 42, "capacity": 256, "generation": 7,
    "hits": 1284, "misses": 201, "evictions": 0, "hit_rate": 0.864
  }
}
```

`hit_rate` is `0.0` when no requests have been served yet (avoids
div-by-zero). `generation` increments on every mutating tool call
(invalidates the cache). Both transports — JSON-RPC `helios/info`
and HTTP `GET /mcp/info` — surface the same field.  New test
`tests/mcp_introspection::helios_info_rpc_includes_cache_stats`.

Commit `26956ba`.

### Fixed — stale `heliosdb-nano mcp-server` CLI doc references

`docs/code_graph/{pilot,troubleshooting}.md` plus a handful of
source comments still showed CLI examples like
`./bin/heliosdb-nano mcp-server --db ...` that fail with
"unrecognized subcommand". The engine deliberately has no
`mcp-server` CLI subcommand — MCP is a library integration
consumed by the out-of-tree
[`heliosdb-codekb-mcp`](https://github.com/dimensigon/heliosdb-codekb-mcp)
plugin, which exposes `serve --source X [--http <addr>]`. Doc
content updated to redirect users at the plugin.

Commit `60f0460`.

## [3.22.1] - 2026-04-29

### Fixed — phantom FK violation on `DELETE child; DELETE parent` inside a transaction

Closes the publish-blocker filed in `BUGS_CODE_INDEX_FK_VIOLATION_v3_21_1.md`
(also `FEATURE_REQUEST_fk_in_txn.md`): every `code_index` call against a
populated KB raised
`fk__hdb_code_symbol_refs_from_symbol___hdb_code_symbols`
on the second of a `DELETE child WHERE …; DELETE parent WHERE …;`
sequence inside a single transaction — even though the first DELETE
removed every offending child row in the same txn.

**Root cause.** `EmbeddedDatabase::check_referencing_rows_exist`
called `storage.scan_table` directly, which reads RocksDB committed
state. The just-tombstoned child rows lived in the txn's write-set,
not yet committed, so they were still visible to the FK validator.

**Fix.** The DELETE plan executor passes the active
`storage::Transaction` through to `check_referencing_rows_exist`,
which now calls `txn.merge_with_write_set(table_name, base)` on the
scan result so in-txn DELETEs / INSERTs are reflected in the FK
check. Equivalent to PG's read-your-own-writes semantics for FK
validation.

**Hang during fix iteration.** The first cut re-acquired
`current_transaction.lock()` from inside `check_referencing_rows_exist`,
which deadlocked because the DELETE caller already held that mutex.
Resolved by threading the txn ref through as an
`active_txn: Option<&Transaction>` parameter — no extra lock
acquisition.

**Acceptance fixtures.** `tests/code_graph_phase2.rs` now has the
three populated-KB workflow tests the field-bench bug report
requested: `ingest_twice_against_populated_kb` (SHA gate
short-circuits), `ingest_twice_with_one_changed_file_against_populated_kb`
(per-file delete-stale runs for the changed file — was the failing
case), and `force_reparse_against_populated_kb`. All pass.

### What this means for the daily workflow

The pilot's silent-fail matrix is closed:

| Workflow                              | v3.22.0          | **v3.22.1**       |
|---------------------------------------|------------------|-------------------|
| First-time `init --ingest`            | ✓ works          | ✓ works           |
| `/helios-code-graph:refresh` (daily)  | ❌ silent fail   | **✓ works**       |
| Git post-commit hook                  | ❌ silent fail   | **✓ works**       |
| `--force` re-parse                    | ❌ silent fail   | **✓ works**       |

### Known interaction — cross-process ON CONFLICT bug × FK validator cost

The FK validator's `merge_with_write_set` walks the txn's
DashMap write-set on every check. When the cross-process
`INSERT ... ON CONFLICT (path) DO UPDATE` bug
(`FEATURE_REQUEST_cross_process_on_conflict.md`) doubles the
client's `src` table across re-runs, the indexer's
duplicate-path defense triggers per-file delete-stale on the
second occurrence of every path, and each FK check pays an
O(write_set_size) cost. On Nano's own corpus this turns a
~3-minute force-reparse into 30+ minutes. The correctness
issue this release fixes is **independent**: even on a
single-process KB without the cross-process bug, the FK
violation reproduced (see the new `ingest_twice_with_one_changed_file_against_populated_kb`
fixture). Closing the cross-process bug will eliminate the
duplicate-path defense path entirely.

## [3.22.0] - 2026-04-29

### Added — code_index write-phase optimisations (Tier 1 + indexes + Tier 2.4 v2)

Field-driven follow-up to the v3.21.0 parallel-parse work. The pilot
showed write-phase wall time was 95% of total ingest cost; v3.21.1
attacks that directly without compromising OLTP/ACID. ACID-positive
across the board (ingests are now atomic per chunk; force-reparse is
atomic against the populated KB).

- **Tier 1.1 — single-transaction write phase.** Per-chunk
  `BEGIN ... COMMIT` around `code_index`'s write loop +
  `cross_file_resolve`. Engine pays one WAL fsync per chunk instead
  of per-statement (tens-of-thousands → tens). With `chunk_size =
  None` (default) the whole ingest is one atomic commit. The
  indexer auto-detects an outer transaction via `db.in_transaction()`
  and honours it (skips its own begin/commit) so callers in
  long-running txns aren't blindsided.
  (`src/code_graph/storage.rs::code_index_with_embedder`)

- **Tier 1.3 — TRUNCATE fast path for `force_reparse`.** When the
  caller sets `force_reparse = true` against a populated KB, the
  indexer issues `TRUNCATE _hdb_code_*` once instead of per-file
  DELETE-then-INSERT. Closes the pilot's 1 h 55 m anti-pattern
  outright (the prior path triggered RocksDB compaction storms on
  bulk-delete-followed-by-bulk-insert against 36 K symbols + 178 K
  refs). After truncate the per-file write loop's
  `SELECT-existing/UPDATE-inbound/DELETE-stale-refs/DELETE-stale-symbols`
  preamble is skipped on the first occurrence of each path —
  guarded with a `processed_paths: HashSet<String>` so duplicate
  paths in `source_table` still get the second occurrence's
  preamble (defensive correctness when upstream upserts misbehave).

- **`_hdb_code_*` covering indexes.** `idx_..._symbols_file_id`,
  `idx_..._symbol_refs_file_id`, `idx_..._symbol_refs_to_symbol`,
  `idx_..._symbol_refs_from_symbol` added to `bootstrap_tables`.
  Eliminates the full-table-scan slow query the pilot observed
  (35 s for a 181-row `DELETE … WHERE file_id = X` against ~115 K
  refs). Also accelerates the cross-file resolver's per-symbol
  back-pointer rebinding. Idempotent — picked up automatically on
  the next `code_index` against an existing KB.

- **Tier 2.4 — bulk RocksDB bypass for `_hdb_code_*` writes
  (direct-write variant).** New crate-internal
  `EmbeddedDatabase::bulk_insert_tuples` primitive: builds `Tuple`
  rows in column order, allocates row_ids in batch, writes
  *directly* to RocksDB via `storage.put` (NOT through the active
  transaction's `write_set`), then updates ART indexes. Mirrors
  the convention `execute_plan_with_params` already uses for
  parameterised `INSERT … RETURNING` so subsequent SQL statements
  in the same outer txn don't pay the O(N) `merge_with_write_set`
  cost — the gotcha that killed an earlier v1 attempt (see
  "Earlier rejected variant" below). `insert_symbols` /
  `insert_refs` in the code-graph indexer route through this
  primitive instead of multi-row `INSERT … VALUES … RETURNING`.
  Trade-off documented in the doc comment: rows are NOT rolled
  back if the outer txn aborts, which is fine for engine-owned
  `_hdb_code_*` tables (the next force-reparse TRUNCATEs them
  anyway) but is why this primitive is `pub(crate)` and gated
  behind a "engine-owned tables only" caveat. User-facing tables
  keep going through `execute()` so triggers / RLS / FKs /
  rollback are honoured.

### Field-bench results — Nano's own `src/` (663 files, 18 425 symbols, 114 784 refs)

| Run                                                       | Wall    | Write   | Notes                                   |
|-----------------------------------------------------------|--------:|--------:|-----------------------------------------|
| Tier 1 only, cold ingest                                  |  7:24   |  6:31   | parse 3.1 s = 0.7 % of total            |
| Tier 1 + indexes, cold ingest                             |  5:39   |  4:46   | indexes save 27 % of cold time          |
| Tier 1.3 + indexes, force-reparse on populated KB         |  4:00   |  2:57   | 1.83× faster than cold                  |
| **Tier 1+2.4 v2, cold ingest**                            | **3:36** | **2:44** | direct-write bulk, **1.57× over Tier 1+ix** |
| **Tier 1+2.4 v2, force-reparse on populated KB**          | **1:22** | **~1:00** | **2.93× over Tier 1.3 force**           |
| **Tier 1+2.4 v2, force-reparse (warm KB, 2nd run)**       | **1:05** | **~0:50** | RocksDB block cache warm                |
| Prior baseline: force-reparse on populated KB             | KILLED at 1:55:00 | — | the anti-pattern this FR closes  |

(In-sandbox numbers; user's host clocked the v3.20 parallel-only
baseline at 5:43 cold.)

### Tests — `tests/code_graph_parallel_index.rs` (now 9, all green)

- New: `force_reparse_against_populated_kb_truncates` — content
  parity (file paths, symbol signatures, ref signatures) between
  cold and force-reparse-after-cold runs. ID columns are
  intentionally NOT compared because Nano's TRUNCATE preserves
  row-id counters (matching Postgres' default).
- New: `write_phase_under_explicit_outer_txn_succeeds` — proves the
  outer-txn-honouring branch produces the same row counts as the
  self-managed branch.

### Honest bug findings (filed as separate follow-ups)

Two pre-existing engine issues surfaced during field benchmarking
that the v3.21.0 work didn't introduce but did expose. Filed for
their own FRs; tracked here for visibility:

1. **Cross-process ON CONFLICT on PK doesn't detect prior committed
   rows.** Re-running a path through `INSERT … ON CONFLICT (path)
   DO UPDATE SET …` from a fresh process duplicates rows instead of
   updating. The v3.21.0 eager-ART-rebuild on `EmbeddedDatabase::open`
   *does* re-register the PK index but the conflict-detection path
   appears to bypass it for cross-process state.
2. **FK constraint sees pre-DELETE state inside a transaction.**
   `BEGIN; DELETE FROM _hdb_code_symbol_refs WHERE …; DELETE FROM
   _hdb_code_symbols WHERE …; COMMIT;` raises a from_symbol FK
   violation on the second DELETE despite the first DELETE removing
   every offending ref *in the same txn*.

### Earlier rejected variant — Tier 2.4 v1 (txn write_set buffered)

The first cut at the bulk primitive routed writes through
`txn.put` when an outer transaction was active. Cold ingest
dropped 5:39 → 2:40 (write phase 3.4×) — but the same KB's
`--force` re-ingest regressed 4:00 → 8:03 (~2× slower). Root
cause: with `chunk_size = None` the entire ingest's writes
accumulate in the txn's DashMap `write_set` (~133 K entries on
this corpus). Each subsequent SQL `DELETE … WHERE file_id = X`
inside the same outer txn falls through `scan_table_filtered` →
`merge_with_write_set`, which iterates and bincode-deserialises
*every* `write_set` entry on every scan call. With ~665 second-
occurrence delete-stale calls (forced by the duplicate-path
defense for the cross-process ON CONFLICT bug surfaced in this
release), the cumulative cost was O(N²) in `write_set` size —
~7 minutes of pure deserialisation work. The fix in v2 is to
write directly to RocksDB the way `execute_plan_with_params`
already does for `INSERT … RETURNING` (`storage.put`, bypassing
`txn.write_set`); subsequent SQL DELETEs read committed rows
from the RocksDB snapshot at no extra per-row cost. v2 fixes
both axes — see field-bench table above.

## [3.21.0] - 2026-04-28

### Added — parallel `code_index` (FR `parallel_code_index`)

`code_graph::storage::code_index` is now split into a single-threaded
**triage** pass (classify rows into skipped / unchanged / to-parse +
hash-gate), a parallel **parse + extract + in-file resolve** phase
running on a *dedicated* `rayon::ThreadPool`, and a single-threaded
**write** phase that walks results in input order. Closes
`FEATURE_REQUEST_parallel_code_index.md`.

- **Dedicated thread pool** — never the global one. Daemon-mode
  servers handling live OLTP traffic see no thread starvation
  during code indexing.
- **`CodeIndexOptions::parallelism: Option<usize>`** caps the
  worker count (default `min(num_cpus, 8)`, `Some(1)` forces
  serial). Operators on shared hosts can cap the indexer's
  footprint to protect query latency.
- **Per-OS-thread `tree_sitter::Parser` cache**
  (`src/code_graph/parse.rs`). `thread_local!` `RefCell<HashMap<…,
  Parser>>` reused across files within a worker — no
  `set_language` overhead per row.
- **Optional bounded-memory chunking** via
  `CodeIndexOptions::chunk_size: Option<usize>`. `None` =
  single-chunk all-in-one (default; max throughput; fits the
  ~1.8 K-file pilot scale). `Some(n)` = drain rows into batches
  of `n`, parse each chunk in parallel, write before moving on —
  bounds peak RAM at `n × avg_parsed_file_bytes` for ≥ 10 K-file
  corpora. Equivalence with the unchunked path is asserted by
  `chunked_output_matches_unchunked`.
- **`CodeIndexStats` telemetry**: `parse_elapsed_ms`,
  `write_elapsed_ms`, `parse_workers`, `chunks_processed`. Lets
  operators see the speed-up + memory-mode in the binary's own
  output.
- **Byte-identical** to the serial path: equivalence locked down
  by `tests/code_graph_parallel_index.rs` against an 8-file
  multi-language fixture (parallelism=1 vs parallelism=8, and
  chunked vs unchunked). 7 unit tests, all green.
- **Field-benchmarked** in release on Nano's own `src/` tree
  (352 Rust files, 12-core host, 8 workers): total wall-clock
  **49.8 s** (under the 2-minute FR budget); parse-phase speedup
  **1.97×**. Parse utilisation caps at ~2× because of allocator
  + grammar-init contention — not the worker pool — and is
  documented as out-of-scope for this FR. See
  `tests/code_graph_parallel_index_bench.rs`
  (`#[ignore]`-gated; run via `cargo test --release --features
  code-graph --test code_graph_parallel_index_bench --
  --ignored --nocapture`).

### Added — engine-level fixes for the v3.20 honest gaps

These are general-purpose engine improvements, not SQLite-shim work. Each
benefits every protocol (PG wire, MySQL wire, embedded REPL, embedded API)
and every mode (single-connection, daemon-server, HA standby).

- **Eager ART index rebuild on open** (`Catalog::rebuild_all_indexes`,
  wired into `EmbeddedDatabase::new` and `EmbeddedDatabase::with_config`
  for non-memory storage). A fresh process attaching to an existing data
  dir now re-registers PK / UNIQUE / FK index structures from the
  persisted schemas and replays existing rows through `on_insert` so
  in-memory ART matches on-disk state. Cost: O(rows + indexes) at
  startup; zero impact on the OLTP hot path. Closes the cross-process
  embedded consistency gap. Persistent index pages backed by a RocksDB
  column family are tracked separately for v3.22+.

- **PostgreSQL date/time function audit** (`src/sql/evaluator.rs`). Added
  `TO_CHAR(date, fmt)`, `TO_DATE(text, fmt)`,
  `TO_TIMESTAMP(epoch | text, fmt)`, `DATE_TRUNC(field, value)`,
  `DATE_PART(field, value)` (alias for EXTRACT), `AGE(t1, t2)`,
  `MAKE_DATE(y, m, d)`, `MAKE_TIMESTAMP(y, m, d, h, mi, s)`. PG format
  codes `YYYY/YY/MM/MON/Mon/mon/DD/DDD/DAY/Day/day/DY/HH24/HH12/MI/SS/MS/US/IW/IYYY/Q/W/D/AM/PM/am/pm`
  are translated to chrono format; case-significant tokens use a
  post-processing pass so output matches Postgres exactly. SQLite-only
  function names (`STRFTIME`, `JULIANDAY`) are not added — callers that
  need them can rewrite at the SDK layer.

- **Composite-PK on the conflict-detection scan path**
  (`src/lib.rs`). When the ART couldn't locate the conflicting row,
  the planner already retried via PK lookup; with rebuild on open this
  retry now sees the same indexes a fresh process would see.

### Added — SDK shim (`heliosdb_sqlite` 3.0.1+)

- **`cursor.lastrowid`** is now populated automatically. The SDK
  detects `INSERT INTO t (...) VALUES (...)` (incl. `INSERT OR REPLACE`
  / `OR IGNORE`), looks up the table's int PK column once via
  `PRAGMA table_info(t)` (cached on the `Connection`), appends
  `RETURNING <pk>`, and stores the returned value as
  `cursor.lastrowid`. No engine state, no protocol change — pure
  client-side use of standard SQL `RETURNING`. Tables with TEXT PKs
  return `None`, matching sqlite3 semantics. Existing
  `INSERT … RETURNING …` calls are passed through untouched.

- **DSN parsing for daemon mode.** `connect(..., mode='daemon')` now
  honours `HELIOSDB_DSN` (or an explicit `dsn=` kwarg) for
  `host/port/user/password/database`, instead of hard-coding `helios`/
  port 5432.

### Tests
- `tests/cross_process_index_rebuild_tests.rs` — 6 tests proving PK
  lookups, INSERT-OR-REPLACE upserts, and UNIQUE constraint enforcement
  all work after closing and reopening a populated data directory.
- `tests/datetime_functions_tests.rs` — 19 tests covering every PG
  date/time function added in this release plus a realistic OLTP-shaped
  GROUP BY DATE_TRUNC aggregation.
- 1746 lib tests + 11 sqlite_compat hardening + 72 aggregate hardening
  remain green.

## [3.20.0] - 2026-04-28

### Added — SQLite drop-in compatibility

Lifts the dialect ceiling so existing `sqlite3`-driven Python applications
(and other SQLite clients) can talk to Nano with no query rewrites. Combined
with the production-ready `heliosdb_sqlite` Python shim, swapping
`import sqlite3` for `import heliosdb_sqlite as sqlite3` is enough to
retarget most apps.

Engine changes:
- **`?` positional placeholders** — parser-level rewrite to PG-style `$N`
  with quote/comment/dollar-quote awareness; mixed `?`/`$N` in a single
  statement is rejected.
- **`INSERT OR REPLACE INTO t (cols) VALUES …`** → expanded to
  `INSERT … ON CONFLICT DO UPDATE SET col = EXCLUDED.col, …` so the same
  upsert semantics apply to PG-wire and embedded REPL clients.
- **`INSERT OR IGNORE INTO …`** → expanded to
  `INSERT … ON CONFLICT DO NOTHING`.
- **`INTEGER PRIMARY KEY AUTOINCREMENT`** → mapped to `BIGSERIAL PRIMARY KEY`
  in DDL.
- **`DATETIME('now')`** → recognised as `CURRENT_TIMESTAMP`.
- **`sqlite_master` system view** with the SQLite column shape
  (`type, name, tbl_name, rootpage, sql`).
- **`PRAGMA` shims** — `table_info(t)` returns SQLite-shaped rows
  (`cid, name, type, notnull, dflt_value, pk`); connection-tunable PRAGMAs
  (`foreign_keys`, `journal_mode`, `synchronous`, `busy_timeout`) are
  accepted as no-ops, intercepted at the protocol layer and at
  `EmbeddedDatabase::execute/query`.
- **Composite-column `CREATE INDEX`** is now accepted instead of erroring
  with the misleading "Multi-column vector indexes" message — only the
  leading column is indexed today (B-tree), but the DDL no longer breaks
  sqlite3 schema migrations. Vector-index variants (`USING hnsw|ivf|…`)
  still reject multi-column.

### Fixed
- **ON CONFLICT DO UPDATE within an explicit transaction** could fail with
  *"existing row not found in storage"* when the conflicting row was
  inserted earlier in the same transaction. The conflict path now reads
  through `txn.get(...)` so write-set rows are visible.

### Tests
- `tests/sqlite_compat_tests.rs` — 11 hardening tests covering each item
  above end-to-end against `EmbeddedDatabase`.
- `src/sql/sqlite_compat.rs` — 16 unit tests for the parser-level
  rewrites (placeholder quote-awareness, multi-statement boundaries,
  PRAGMA detection).

### Honest status
- **Cross-process embedded mode** (one `heliosdb-nano repl` per Connection,
  data shared via `--data-dir`) does not rebuild the in-memory ART
  indexes on startup, which means a fresh process can't see the unique /
  PK constraints registered by a prior process and falls back to scan
  paths. Single-connection embedded use is the recommended path; daemon
  mode (one `heliosdb-nano start`, many `psycopg2` connections) is the
  recommended scale-up.
- The Python SDK shim absorbs the rest: SQL-string positional binding for
  `?`, table-output parser for box-drawn results, PRAGMA-as-query
  routing, and a non-blocking `__del__` so finalisation doesn't time out
  on slow rollbacks.

See `docs/compatibility/sqlite.md` and
`docs/guides/sqlite_drop_in_tutorial.md` for the full feature matrix
and a runnable port walkthrough.

## [3.19.1] - 2026-04-25

### Fixed — UUID literal coercion at PK index lookup (#205)

Resolves the CloudV2 admin_db "INSERT-then-SELECT-misses-the-row"
bug.  Root cause was not the COMMIT path / deadpool recycling /
3.14.9 regression the original investigation theorised — it was a
planner literal-typing bug.

`SELECT … WHERE id = '<uuid>'` against a UUID-typed PK emitted
`Value::String("<uuid>")` regardless of the column's declared
type.  The ART point-lookup encoded the search key by Value
variant, so Value::String and Value::Uuid produced different
encoded keys → the lookup missed every row.

Three patches land the fix:
- `src/sql/executor/mod.rs::try_index_point_lookup` — coerce the
  literal to the PK column's type before the ART lookup. New
  helper `coerce_literal_to_column_type` handles
  String→UUID/Date/Timestamp.
- `src/lib.rs::fast_parse_one_value` — same coercion at the
  fast-select parse layer for `SELECT *` queries.
- `src/storage/simd_filter.rs::compare_eq` — Uuid↔String
  cross-type case so the SIMD post-walk filter also matches.

Verified by `tests/uuid_where_repro.rs` (direct API) and
`tests/persistence_repro.rs` (wire protocol, no longer
`#[ignore]`d).  All 1842 lib tests + every prior integration
suite remain green.

CloudV2 follow-ups: revert admin_db SELECT-all workarounds,
drop the daily restart cron, graduate `cloud-v2.heliosdb.com`
to production.

## [3.19.0] - 2026-04-25

### Added — backlog sweep #181-#193

Closes the residual FR backlog the v3.18 review left open.  Each
task carries a context doc under `docs/followups/`.

- **#181 `hdb_code_languages` system view** — exposes
  `SupportedLanguage::all()` + `parse::registered_grammars()` as a
  queryable SQL view.  Runtime grammars shadowing a static tag
  report `source = 'runtime'`.
- **#182 `body_vec VECTOR(n)` column** materialised on
  `_hdb_code_symbols` lazily on first non-null embedding.
  Dimension negotiated at insert time; `code_index_with_embedder`
  is the new public entry point that takes a pre-constructed
  `Box<dyn Embedder>`.
- **#183 SymbolExtractor pluggability** — runtime-registered
  grammars can ship with paired extractors via
  `EmbeddedDatabase::register_extractor`, so dynamic languages
  produce real symbols instead of empty parse trees.
- **#184 HTTP POST + SSE progress pairing** — process-static
  session table keyed by `Mcp-Session-Id`.  POSTs that pair with
  an open SSE channel get their `notifications/progress` events
  forwarded to the SSE stream while the POST returns the final
  `tools/call` response.
- **#185 `helios_lsp_rename_apply`** — write-back side of the
  preview tool; identifier-boundary aware, sha256 conflict check,
  optional dry_run.
- **#186 Docling content-conversion ingestion** —
  `graph_rag_ingest_pdf / _office / _audio / _image` POSTs to
  docling-serve, parses the DoclingDocument JSON, and projects
  sections + chunks + tables under `_hdb_graph_nodes` with
  CONTAINS edges.  Idempotent via source_ref keys.
- **#187 `code-embed` feature** — fastembed-rs as the in-process
  embedder.  Default model BGESmallENV15 (384-dim, ~30 MB cache
  on first run).  No on-disk impact on the binary itself.
- **#188 `_hdb_code.*` / `_hdb_graph.*` dotted namespacing
  aliases** — planner-level rewrite at
  `normalize_object_name`; `pg_tables.schemaname` reports the
  schema split.  Catalog keys remain flat (full refactor tracked
  separately).
- **#189 Scope-chain resolver via IMPORTS** — unresolved CALLS /
  REFERENCES refs upgrade to `Exact` when an unambiguous IMPORTS
  edge in the same file ends in the bare name.  Handles Rust
  `use foo::bar`, Python `from foo import bar`, TypeScript
  `import { bar } from './foo'`, Go imports.
- **#190 Centrality-biased + prefilter-aware HNSW wrapper** —
  over-fetches `k * over_fetch_multiplier` candidates, applies
  the row-level prefilter, re-scores with
  `(1 - α) × distance - α × centrality`.  Equivalent to the FR's
  Option B (post-rerank) lift; in-descent navigation bias is a
  separate phase-3.1 follow-up.
- **#191 Acceptance benchmarks** — `with_context_bench` (10k-node
  fixture, 100-query mean ≤ 500 ms) and `linker_precision`
  (≥ 80 % on a hand-labelled fixture).  Current run: mean 62 ms,
  precision 100 %.
- **#192 FR-6 pilot deployment** — `scripts/install-nano-pilot.sh`
  + `docs/code_graph/{pilot,troubleshooting}.md`.
- **#193 Build report** — `docs/followups/build-report.md`
  captures the all-features release binary metadata
  (35.0 MiB, sha256 `41176528…`, rustc 1.92.0).

### Added — code-graph / graph-rag / MCP follow-ups

Closes nine of the gaps a downstream client raised against the v3.18
merge (`feat/code-graph-phase1` → `main`).  All additive; no public
API breakage.

- **#1 `helios_lsp_document_symbols`** — file outline ordered by
  line, optional kind filter.
- **#2 `helios_lsp_rename_preview`** — preview-only edit list
  (definition + every reference site); never writes back.
- **#3 `helios_graphrag_search`** — wraps the embedded
  `graph_rag::graph_rag_search` Rust API as an MCP tool. The
  flagship cross-modal query is now reachable over JSON-RPC, not
  just over SQL.
- **#4 `helios_lsp_references_diff` / `helios_lsp_body_diff` /
  `helios_ast_diff`** — wraps the existing `diff::*` Rust API.
  Accepts AS OF refs as `{"now": true}`, `{"commit": "sha"}`,
  `{"timestamp": "iso"}`.
- **#5 FR-3 `ON BRANCH '<name>'` per-call override** on
  `lsp_*(...)` table functions. RAII branch guard restores the
  prior branch on every early-return path. Combines with `AS OF`
  in either order.
- **#6 `CREATE SEMANTIC HASH INDEX [IF NOT EXISTS] <name>`** DDL
  surfaces the existing `code_graph::merkle_refresh` Rust
  primitive at the SQL layer (FR 4 §4.6).
- **#7 `graph_rag_link_vector`** — vector-similar entity-linker
  stage (FR 4 §4.3 strategy 3). Takes caller-supplied
  `(node_id, vector)` pairs on both sides; runs cosine top-k with
  threshold gating; emits MENTIONS edges with
  `weight = similarity`.
- **#8 `tools/list?verbose=true`** + **`helios/info` JSON-RPC
  method** + **`GET /mcp/info` HTTP route** — single-shot
  discovery payload (serverInfo + capabilities + verbose tool
  catalogue + resource list).
- **#9 Streaming `notifications/progress` events.** Tools that
  call `mcp::progress::emit` from anywhere on the call stack
  produce JSON-RPC `notifications/progress` messages when the
  client opted in via `_meta.progressToken`. Wired into the
  WebSocket and stdio transports; HTTP POST stays single-shot
  (use the SSE channel for streaming there). The streaming
  dispatcher (`mcp::call_tool_streaming`) runs the sync handler
  on `spawn_blocking` and forwards events through an unbounded
  channel. `helios_graphrag_search` is the first tool wired —
  emits a "seeding" event on entry and a "<n> hits" event on
  exit so agents can render a progress indicator.
- **`SupportedLanguage`** alignment: enum now mirrors `Language`
  (Rust / Python / TypeScript / Tsx / JavaScript / Go / Markdown /
  Sql) so the planned `hdb_code.list_languages` system view
  doesn't lie about the static set. `SupportedLanguage::all()`
  + `From<Language>` conversion added.

### Tests

- 9 follow-up integration tests in `tests/mcp_followups.rs`.
- 4 introspection tests in `tests/mcp_introspection.rs`.
- 2 ON BRANCH integration tests in `tests/code_graph_on_branch.rs`.
- 2 semantic-hash DDL tests in
  `tests/code_graph_semantic_hash_ddl.rs`.
- 4 vector-similar linker tests in
  `tests/graph_rag_linker_vector.rs`.
- 4 progress-streaming integration tests in
  `tests/mcp_progress.rs` (WS round-trip with token, no token,
  numeric token, scalar-token contract) + 3 lib unit tests
  covering the channel sink + thread-local routing.
- 6 new `sql_rewrite` unit tests for `ON BRANCH` parsing
  (combinations, escape, tie-break) + 3 unit tests for
  `detect_create_semantic_hash_index`.

### Ratified deviations from FR text

These design choices in v3.15–v3.18 were reviewed by a downstream
client; we ratify them here as the intended end-state rather than
TODOs:

- **Flat-prefix tables (`_hdb_code_*` / `_hdb_graph_*`) instead of
  dotted schemas (`_hdb_code.*`).** Simpler bootstrap; no catalog
  refactor required. Promotion to real schema namespacing is a
  separate engine-wide refactor that benefits `pg_catalog` too,
  not part of the code-graph track.
- **Cargo features (`code-graph` / `graph-rag` / `mcp-endpoint`)
  instead of runtime `CREATE EXTENSION`.** Build-time opt-in; no
  per-process activation step; the static grammar set is fixed at
  build but the runtime grammar registry
  (`src/code_graph/parse.rs::register_grammar`) covers the dynamic
  plug-in case (caller supplies the loader — wasm runtime,
  dynamically-linked grammar, etc.).
- **Centrality is a post-rerank weighting, not an HNSW navigation
  bias** (`src/graph_rag/centrality.rs:10`). Ships the smaller
  relevance lift but avoids forking `hnsw_rs`. Descent-bias is a
  separate phase-3.1 follow-up if the relevance gap turns out to
  matter in the pilot.

### Known follow-ups

- **HTTP POST progress** — `POST /mcp` is request/response so
  streaming requires the paired SSE channel; not yet wired.
  WebSocket and stdio cover the streaming case today.

## [3.18.0] - 2026-04-24

### Added — MCP endpoint phase 4 MVP (FR 5, opt-in, feature = "mcp-endpoint")

First landing for the native MCP endpoint. Ships a JSON-RPC 2.0
dispatcher on top of the existing `src/mcp_extensions/` tool
catalogue so an MCP-capable agent (Claude Code, Cursor, Continue,
Codex, Aider) can drive HeliosDB-Nano with no wrapper process.

- New Cargo feature `mcp-endpoint`. Additive — embedded-only
  callers compile without it.
- New module `src/mcp_http/` with two files:
  - `rpc.rs` — `handle_rpc(req) → resp`, pure function over JSON-RPC
    `initialize`, `tools/list`, `tools/call`, `ping`. Unknown methods
    return the canonical `-32601 Method not found`.
  - `mod.rs` — re-exports.
- Tool catalogue: every tool already registered in
  `mcp_extensions::tools::list_tools()` is surfaced automatically.
  `heliosdb_bm25_index`, `heliosdb_hybrid_search`,
  `heliosdb_graph_add_edge`, `heliosdb_graph_traverse`,
  `heliosdb_graph_path`, `heliosdb_embed_and_store`.
- Server-info handshake reports `{"name":"heliosdb-nano","version":<pkg>}`.

Explicit non-goals for the MVP (tracked for follow-ups):
- WebSocket / SSE framing — HTTP JSON-RPC only.
- Repair of legacy `src/mcp/` module — `BLOCKER_mcp_legacy.md`
  stays accurate. Phase 4 deliberately does not touch it; the
  MVP handler backs onto the already-working `mcp_extensions/`
  crate directly.
- Axum route wiring — we ship `handle_rpc` as a pure function so
  embedders mount it on whatever route / auth surface they want.
- Macro-driven auto-registration of `lsp_*` / `graph_rag_*` as MCP
  tools (the tool catalogue remains the six-tool `mcp_extensions`
  set for now).

Regression coverage:
- 4 new unit tests (`src/mcp_http/rpc.rs`): `initialize`,
  `tools/list`, unknown method, `tools/call` without name.
- 4 new integration tests (`tests/mcp_endpoint_phase4.rs`):
  canonical handshake, real tool call, unknown tool as
  `isError=true`, ping.

## [3.17.0] - 2026-04-24

### Added — Graph-RAG phase 3 MVP (opt-in, feature = "graph-rag")

First landing for the universal cross-modal graph. Still embedded
Rust API; SQL-level `WITH CONTEXT` clause, graph-weighted HNSW
tie-breaking, and semantic-Merkle invalidation are follow-ups.

- New Cargo feature `graph-rag` (implies `code-graph`).
- New module `src/graph_rag/` (`mod.rs`, `schema.rs`, `search.rs`).
- `_hdb_graph_nodes` and `_hdb_graph_edges` tables bootstrapped on
  first call. Plain user tables; queryable and joinable.
- `EmbeddedDatabase::graph_rag_project_symbols()` — project every
  row of `_hdb_code_symbols` into `_hdb_graph_nodes` + every
  resolved row of `_hdb_code_symbol_refs` into `_hdb_graph_edges`.
  Idempotent. Tolerates the code-graph tables being absent (no-op
  when nothing to project).
- `EmbeddedDatabase::graph_rag_search(opts)` — seed → BFS expand →
  return subgraph with hop distances. `seed_text` matches title/
  text case-insensitively; `seed_kinds` + `edge_kinds` push down
  through `FilteredScan` so bloom / zone-map / SIMD selection
  applies automatically.

Regression coverage:
- `tests/graph_rag_phase3.rs`: 3 tests —
  `project_and_search_finds_symbol`, `empty_seed_text_errors`,
  `bfs_respects_hops`.

Explicitly out of scope for phase 3 (tracked for phase 3.1):
hybrid-search + vector rerank on seeds, graph-weighted HNSW
tie-breaking, semantic-Merkle index, `WITH CONTEXT` SQL clause,
corpus ingestion adapters (`ingest_docs` etc.), entity linker for
cross-modal MENTIONS.

## [3.16.0] - 2026-04-24

### Added — Code-graph phase 2 (opt-in, feature = "code-graph")

- `CREATE EXTENSION hdb_code` DDL. Parses through the standard
  planner, runs the code-graph bootstrap, and marks the extension
  installed in the process. `IF NOT EXISTS` with an unknown extension
  is a silent no-op (matches PG's permissive migration behaviour).
- TypeScript / JavaScript / TSX grammar support via
  `tree-sitter-typescript`. `Language` enum extended; symbol
  extractor handles `function_declaration`, `method_definition`,
  `class_declaration`, `abstract_class_declaration`,
  `interface_declaration`, `type_alias_declaration`,
  `enum_declaration`.
- Cross-file symbol resolver. After the per-file pass,
  `code_index` rebinds every `resolution='unresolved'` edge against
  a corpus-wide name index. Single match → `exact`, multiple → the
  first with `heuristic`.
- New `LogicalPlan::{CreateExtension, DropExtension}` variants;
  `DropExtension` is reserved for forward compatibility (sqlparser
  0.53 doesn't expose `DROP EXTENSION`).

Regression coverage:
- `tests/code_graph_phase2.rs`: 5 new integration tests —
  `typescript_extracts_class_and_method`,
  `create_extension_hdb_code_bootstraps_tables`,
  `create_extension_unknown_errors`,
  `create_extension_unknown_if_not_exists_is_noop`,
  `cross_file_ref_resolves`.

## [3.15.0] - 2026-04-24

### Added — Code-graph track, phase 1 (FR 2 MVP, opt-in)

New opt-in feature `code-graph` that turns HeliosDB-Nano into an
embedded code-graph for AI coding agents. Phase 1 ships the
foundational Rust API — wire-level DDL (`CREATE EXTENSION hdb_code`,
`CREATE AST INDEX`) and temporal queries land in phase 2.

- New Cargo feature `code-graph` pulling
  `tree-sitter = "0.23"`, `tree-sitter-rust`, and `tree-sitter-python`
  as optional deps. Default builds pull none of them; the default
  release binary stays the same size.
- New module `src/code_graph/` with a minimal in-file AST + symbol
  extractor for Rust and Python. Adds:
  - `EmbeddedDatabase::code_index(opts)` — parse every row of a user
    table `(path TEXT PK, lang TEXT, content TEXT)` and populate the
    `_hdb_code_*` tables idempotently.
  - `EmbeddedDatabase::lsp_definition(name, hint)` — "where is X defined?"
  - `EmbeddedDatabase::lsp_references(symbol_id)` — "who uses X?"
  - `EmbeddedDatabase::lsp_call_hierarchy(symbol_id, direction, depth)` —
    BFS over the `CALLS` edges.
  - `EmbeddedDatabase::lsp_hover(symbol_id)` — signature lookup.
- New tables created automatically on first `code_index` call:
  `_hdb_code_files`, `_hdb_code_symbols`, `_hdb_code_symbol_refs`.
  Plain user tables — queryable, joinable, branch-aware.
- Pluggable embedding surface (`src/code_graph/embed.rs`):
  `NoopEmbedder` (default) and `HttpEmbedder` for external endpoints
  matching `POST {"input": "..."} → {"embedding": [...]}`. Nano ships
  no inference runtime; by design, all inference is external.
- Storage-level filtering is the competitive lever: every `lsp_*`
  query pushes its WHERE through the existing `FilteredScan` path in
  `src/storage/predicate_pushdown.rs`, so bloom-filter / zone-map /
  SIMD selection applies without new code.

Out of scope for phase 1 (tracked for phase 2+ in the track docs):
`CREATE EXTENSION` DDL, `CREATE AST INDEX` DDL, real schema
namespacing, temporal / branch variants, incremental reparse,
semantic-Merkle subtree hashes, `WITH CONTEXT` clause, native MCP
endpoint.

Regression coverage:
- 12 new module-level unit tests (parser, symbol extraction,
  in-file resolver, embedder).
- 6 new integration tests at `tests/code_graph_mvp.rs`:
  `rust_lsp_definition_finds_function`,
  `lsp_references_returns_call_sites`,
  `lsp_call_hierarchy_incoming_terminates`,
  `lsp_hover_returns_signature`,
  `code_index_is_idempotent`,
  `unknown_lang_is_skipped_cleanly`.

Docs: `docs/code_graph/overview.md`.

## [3.14.10] - 2026-04-23

### Fixed — Foreign key validation with quoted identifiers, fast-path bypass (B36)

**Reporter's symptom.** `INSERT INTO "workspaces" (name, owner_id)
VALUES (…)` over the extended protocol failed with
`ERROR: Table '"users"' does not exist`, while the unquoted
`INSERT INTO workspaces (…)` silently succeeded even with a
nonexistent parent row. Drizzle emits every identifier quoted, so
every Drizzle-shaped INSERT on an FK-bearing table tripped this.

Two interlocking bugs:

**Root cause #1 — FK references stored with literal quote
characters.** `src/sql/planner.rs` built
`TableConstraint::ForeignKey` via `ObjectName::to_string()` at both
the inline `ColumnOption::ForeignKey` site and the table-level
`SqlTC::ForeignKey` site. `ObjectName::to_string()` preserves the
original quoting, so `REFERENCES "users"("id")` stored the
referenced table as the four-character string `"users"` (with the
quotes). FK validation later called `get_table_schema(&fk.references_table)`
and emitted the verbatim `Table '"users"' does not exist`.

Fix: normalise every identifier at FK construction time with the
same `Planner::normalize_ident` / `Planner::normalize_object_name`
helpers every other DDL path uses. `REFERENCES "users"("id")` and
`REFERENCES users(id)` both now store `references_table = "users"`
and `references_columns = ["id"]`.

**Root cause #2 — `try_fast_insert` skipped FK validation.**
`src/lib.rs::try_fast_insert` wrote rows directly to storage with no
call to `check_fk_constraints` / `check_foreign_key_exists`. Unquoted
INSERTs into FK-bearing tables silently succeeded regardless of
parent-row existence — a data-integrity hole. It also extracted the
target table name with its surrounding quotes intact, so quoted
INSERTs fell out of the fast path entirely and triggered root cause
#1 on the normal path.

Fix: (a) strip surrounding double quotes from the fast-path table
name so quoted and unquoted shapes route identically; (b) bail to the
normal path for any table with registered FK constraints so the
already-validated Insert arm handles the write.

Regression tests (`tests/drizzle_compat_tests.rs`):
- `b36_fk_insert_with_quoted_references` — verbatim Drizzle shape
  (CREATE TABLE with `REFERENCES "users"("id")`, INSERT via extended
  protocol with a valid FK). Must succeed.
- `b36_fk_violation_fires_on_unquoted_insert` — guards the fast-path
  bypass; unquoted INSERT without a matching parent row must fail.
- `b36_fk_violation_fires_on_quoted_insert` — same for the quoted
  shape.
- `b36_fk_succeeds_both_shapes` — both quoted and unquoted INSERTs
  succeed when the FK is satisfied (symmetry guard).

## [3.14.9] - 2026-04-22

### Fixed — GROUP BY correctness with mixed qualifier styles and DATE keys (B35)

**Reporter's symptom.** A Drizzle-emitted analytics query mixing
unqualified column refs in SELECT / CASE bodies with table-qualified
refs in GROUP BY / WHERE:

```sql
select date("check_in"), sum(case when "check_out" is not null ...)
from "time_entries"
where "time_entries"."workspace_id" = $1
group by date("time_entries"."check_in")
```

failed with `Column 'check_in' not found in schema`. Stock PostgreSQL
treats `"check_in"` and `"time_entries"."check_in"` as the same
column when unambiguous.

**Root cause #1 — projection-rewrite matching too strict.**
After planning `Aggregate`, the planner rewrites each SELECT item so
column refs that match a GROUP BY expression become references to the
aggregate operator's output column (`group_N`). The matching step
used `PartialEq`, so `date(Column{table:None,name:"check_in"})` did
not match `date(Column{table:Some("time_entries"),name:"check_in"})`
— the SELECT item's `"check_in"` reference was left as-is and then
failed to resolve against the aggregate's output schema.

Fix: new `Planner::exprs_equivalent` that recursively compares
expressions with qualifier-insensitive `Column` matching. Used at
both sites inside `rewrite_expr_replace_aggregates`.

**Root cause #2 — `compare_values` missing DATE / TIME / INTERVAL /
NUMERIC arms (found while reproducing).**
`GroupKey` in the aggregate operator is ordered via
`compare_values` (src/sql/executor/mod.rs). Any two values without a
dedicated arm fall through to `type_priority`, which returns `Equal`
for any two values of the same type. So `GROUP BY <date-col>` put
every row into a single group (count grew, distinct dates vanished);
`ORDER BY <date-col>` produced non-deterministic output.

Fix: add arms for `Date`, `Time`, `Interval`, `Numeric` in
`compare_values`.

Regression tests (`tests/drizzle_compat_tests.rs`):
- `b35_mixed_qualifier_group_by`
- `b35_both_qualified_group_by`
- `b35_both_unqualified_group_by`
- `b35_reporter_full_shape` (verbatim Drizzle query with SUM + CASE +
  EXTRACT + parameterised WHERE)
- `b35_date_column_group_by_correctness` (guards the second root cause)

## [3.14.8] - 2026-04-22

### Fixed — parameterized LIMIT/OFFSET and UPDATE SET type coercion (B33 / B34)

**B33** — `LIMIT $1 OFFSET $2` was rejected with `LIMIT/OFFSET must
be a number`. Two independent issues surfaced together:

- Wire path: postgres-js binds numeric parameters as TEXT (OID 0 or
  25) by default. `substitute_parameters` renders a string value with
  surrounding single quotes, so the planner saw `LIMIT '3'`, which
  the old `expr_to_usize` rejected.
- In-process path: the planner mapped `$N` to `usize::MAX` as a
  sentinel, but `LogicalPlan::Limit` only carried the sentinel — the
  bound integer never reached the executor. Queries silently returned
  all rows (or all-rows-minus-offset).

Fix:
1. `expr_to_limit_bound` (new) returns `(usize, Option<usize>)`.
   Accepts `Number`, `Placeholder($N)` → `(MAX, Some(N))`, and
   `SingleQuotedString(n)` → `(n, None)`. The quoted-string arm
   matches stock PG's implicit `text → integer` cast for LIMIT /
   OFFSET.
2. `LogicalPlan::Limit` gained `limit_param: Option<usize>` and
   `offset_param: Option<usize>` fields, propagated through the
   optimizer, RLS plan rewrite, and outer-ref binding paths.
3. The executor's Limit branch resolves these from the bound
   parameter list before running any of the Top-K, storage-offset, or
   generic Limit paths.

**B34** — `UPDATE t SET ts_col = $1` via extended-Q silently stored
NULL in TIMESTAMP columns. `sql.unsafe` with the same SQL + string
params worked. INSERT with the same pattern worked.

Root cause: INSERT's value path auto-casts each evaluated value to
its target column type before persistence; UPDATE's SET path did not
— a `Value::String("2026-04-23T10:00:00.000Z")` was pushed straight
into a TIMESTAMP slot, which the storage serializer dropped as an
implicit NULL.

Fix: mirror INSERT's auto-cast in every UPDATE SET path — the
`execute_plan_with_params::Update` arm, the trigger-aware
`execute_in_transaction_inner::Update` arm, and the RLS-aware
non-params Update arm. All three now call `evaluator.cast_value(new,
target_type)` when the new value and column type disagree.

Regression tests (`tests/drizzle_compat_tests.rs`):
- `b33_parameterized_limit`, `b33_parameterized_limit_offset`,
  `b33_quoted_string_limit_wire_substitution`
- `b34_update_set_param_timestamp`,
  `b34_update_set_literal_iso_string`

## [3.14.7] - 2026-04-22

### Fixed — Drizzle UPDATE/DELETE and analytics date ranges (B31 / B32)

**B31** — `UPDATE "t" SET … WHERE "t"."col" = $1` and the equivalent
DELETE fail with `Column 't.col' not found in schema`. Root cause:
the Update and Delete arms of `execute_plan_with_params` (and the
in-transaction variants) build their evaluator directly from the
catalog schema, which does not carry `source_table_name` on its
columns. The SELECT path works because the Scan operator stamps
`source_table_name` on every yielded column; DML didn't.

Fix: new helper `Schema::with_source_table_name(&str)` that stamps
`source_table` and `source_table_name` on every column.
Every single-table DML evaluator now builds its schema through this
helper, so qualified WHERE columns resolve the same way they do for
SELECT. Blocks the stop-timer, edit/delete entry, edit/delete
customer, bulk ops, and role/member management paths.

**B32** — `timestamp >= '2026-04-23T00:00:00.000Z'` (and the `date`
analogue) fail with `Cannot compare Timestamp(…) and String(…)`.
Stock PostgreSQL implicitly casts the literal to the column type;
Drizzle's `gte()` / `lte()` helpers bind JavaScript `Date` instances
as ISO 8601 strings, so every analytics / reporting endpoint hit
this.

Fix: `Evaluator::compare_values` gains four new arms —
`Timestamp ↔ String` and `Date ↔ String` — using the same ISO 8601
/ space-separated / date-only parser as the TIMESTAMP cast path
(`Self::parse_timestamp_string`, `Self::parse_date_string`). Falls
back to string-wise comparison if the literal isn't a valid date /
timestamp, matching the behaviour of the other coercion arms (e.g.
`Int ↔ String`).

Regression tests (tests/drizzle_compat_tests.rs):
- `b31_update_with_qualified_where_column`
- `b31_delete_with_qualified_where_column`
- `b32_timestamp_vs_iso_string_comparison`
- `b32_date_vs_iso_string_comparison`

## [3.14.6] - 2026-04-22

### Fixed — Drizzle login read-by-unique-key (B29, real root cause)

The 3.14.5 fix addressed timestamp formatting (B30) but assumed B29
was a downstream symptom. TimeTracker's retest proved otherwise: even
with timestamps round-tripping cleanly, the canonical Drizzle shape
`select <all cols> from t where t.col = $1` still returned `[]`.

Actual root cause: `execute_plan_with_params` (src/lib.rs:4983), the
plan-executor behind `EmbeddedDatabase::execute_returning` and
`execute_params_returning`, mutated data but never invalidated
`result_cache`. The `Database::query` entry point DOES invalidate on
DML, but the extended-Q handler for `INSERT ... RETURNING` calls
`execute_returning` **directly**, bypassing that invalidation.

Trigger sequence (TimeTracker's login/register flow):

1. User attempts login against an empty `users` table.
   `SELECT ... WHERE "users"."email" = $1` → `[]`. After parameter
   substitution the key is the fully-rendered SQL;
   `result_cache` stores `[]` under it.
2. User registers. `INSERT ... RETURNING ...` via extended-Q lands in
   `execute_plan_with_params`, which inserts the row but does NOT
   clear `result_cache`.
3. User logs in. Same canonical SQL → substitutes to the same key →
   cache hit → stale `[]` is served forever, even though the row now
   exists.

Swapping any trigger (unqualified WHERE, different projection, string
literal instead of `$1`) produces a different substituted SQL and
therefore a different cache key that misses, which is why every
variation returned the row while the canonical shape didn't — and why
the bug looked like a "planner/prepared-statement" issue to the
reporter.

Fix: invalidate `result_cache` at the single choke point in
`execute_plan_with_params` whenever the plan is `Insert` /
`InsertSelect` / `Update` / `Delete` and the execution succeeded.

Regression tests:
- `tests/drizzle_compat_tests.rs::b29_login_probe_then_register_then_login`
  — in-process reproduction of the trigger sequence.
- `tests/drizzle_compat_tests.rs::b29_canonical_drizzle_select_returns_row`
  — pins the canonical substituted shape.
- `tests/drizzle_compat_tests.rs::b29_qualified_predicate_matches_scan_row`
  — shrinks the qualified-predicate invariant.

## [3.14.5] - 2026-04-22

### Fixed — Drizzle login + timestamp reads (B29 / B30)

Both bugs had the same root cause: the direct-encoding path at
`send_data_row_direct` (src/protocol/postgres/handler.rs:952) was
still emitting `Timestamp` values as RFC-3339 with nanosecond
precision (`2026-04-21T20:43:55.674347541+00:00`). v3.14.4 fixed the
fallback `tuple_to_pg_values` path but missed this one. Consequences:

- **B29 Drizzle SELECT shape returns empty.** When Drizzle's
  `postgres-js` integration parsed the malformed timestamp it
  crashed the result binding silently, and Drizzle's type-coerced
  filter comparison (`eq(users.email, v)`) resolved against a
  null-valued row that the app-side filter then rejected as
  non-matching — the symptom being "empty result set". The
  underlying pg query *did* return the row; the client just failed
  to interpret it.
- **B30 timestamp columns parsed as null.** `drizzle-orm/postgres-js`
  registers a custom parser for OID 1114 (`timestamp`) that expects
  PG wire format `YYYY-MM-DD HH:MM:SS.ffffff` (microsecond precision,
  space separator, no zone). Our nanosecond-precision RFC-3339
  output silently produced `null`.

Fix: emit PostgreSQL-standard `YYYY-MM-DD HH:MM:SS.ffffff` on the
direct-encoding path (matching v3.14.4's `tuple_to_pg_values` fix).
Applied to `Timestamp` and `Time` — `Date` was already correct.

### Verified end-to-end with `drizzle-orm/postgres-js`

```js
const users = pgTable('users', {
  id: serial('id').primaryKey(),
  email: text('email').notNull(),
  password: text('password').notNull(),
  createdAt: timestamp('created_at').defaultNow().notNull(),
})

const [u] = await db.insert(users).values({ email, password }).returning()
// { id: 1, email: 'alice@x.com', password: 'pw',
//   createdAt: 2026-04-22T06:05:01.619Z }  ← real Date, not null

const rows = await db.select().from(users).where(eq(users.email, 'alice@x.com'))
// [{ id: 1, email: 'alice@x.com', password: 'pw', createdAt: Date(…) }]
```

## [3.14.4] - 2026-04-21

### Fixed — Drizzle `.insert().returning()` blockers (B27 / B28)

- **B27 `DEFAULT` keyword inside `VALUES` resolves the column's declared
  default.** v3.14.0 (B3) rewrote every `DEFAULT` token to NULL, which
  worked for SERIAL/IDENTITY columns (auto-filled later in storage) but
  broke any column with a real `DEFAULT <expr>` — v3.14.3's NOT NULL
  enforcement then rejected the NULL.  New `LogicalExpr::DefaultValue`
  marker flows from the planner to the INSERT executor; the executor
  treats it as "column omitted", so the B24 default-fill pass runs the
  declared DEFAULT expression.  Drizzle emits `VALUES (default, …,
  default)` on every `.insert()` — every write in TimeTracker hit this.
- **B28 `INSERT … RETURNING *` over the extended query protocol.**
  `handle_execute_extended` used to dispatch non-SELECT writes through
  `database.execute()` which drops the returning tuples. Now detects
  `INSERT/UPDATE/DELETE … RETURNING …`, routes through
  `execute_returning`, and emits the tuples as `DataRow` messages
  (RowDescription was already sent during Describe).  Matches the
  simple-query behaviour.
- **Timestamp wire format** now microsecond-precision with a space
  separator (`YYYY-MM-DD HH:MM:SS.ffffff`) — the PostgreSQL
  on-the-wire format. Previously `rfc3339` nanosecond-precision output
  crashed psycopg's timestamp parser ("timestamp too large (after year
  10K)"). `postgres-js` accepted both but produced slightly different
  `Date` values.

### Added

- `LogicalExpr::DefaultValue` — dedicated marker for the `DEFAULT`
  keyword in INSERT VALUES. Threaded through planner, optimizer,
  type_inference, and the three INSERT executor paths.
- `tests/drizzle_compat_tests.rs` — two B27 regression cases (DEFAULT
  for DEFAULT-expr column, DEFAULT for SERIAL column). B28 is a
  wire-level regression verified via postgres-js end-to-end.

### Verified end-to-end via `postgres-js 3.4.5` + Drizzle's exact INSERT shape

```js
const [user] = await sql`
  INSERT INTO "users" ("id","email","pw","created_at")
  VALUES (default, ${'alice@x.com'}, ${'pw'}, default)
  RETURNING "id","email","pw","created_at"
`
//  { id: 1, email: 'alice@x.com', pw: 'pw',
//    created_at: '2026-04-21T20:49:20.925Z' }
```

## [3.14.3] - 2026-04-21

### Fixed — first-user-registration blockers (B24 / B25 / B26)

- **B24 `DEFAULT <expr>` applied on omitted columns.** Every Drizzle
  table with `created_at TIMESTAMP DEFAULT now() NOT NULL` was
  inserting NULL instead of evaluating `now()`, then either erroring
  on the NOT NULL constraint or (worse) storing NULL silently. New
  helper `apply_defaults_and_check_not_null` parses the stored
  default expression JSON, evaluates it via the shared SQL evaluator,
  and fills in the omitted slot. Only omitted slots get defaults —
  explicit `NULL` bypasses the default and surfaces as a NOT NULL
  violation, matching stock PostgreSQL.
- **B25 `INSERT INTO t DEFAULT VALUES`.** sqlparser leaves
  `insert.source = None` for this syntax; the planner used to error
  with "INSERT statement missing source query". Now maps to an Insert
  with a single empty VALUES row — every schema column goes through
  the default-fill pass.
- **B26 `NOT NULL` enforcement on every INSERT path.** Three INSERT
  paths (fast-path `try_fast_insert`, per-params
  `execute_plan_with_params`, main transactional
  `execute_in_transaction`) all call the new NOT NULL check. Covers
  both omitted columns and explicit `NULL` in user VALUES. Consistent
  with the extended-protocol path.

### Added

- `EmbeddedDatabase::apply_defaults_and_check_not_null` — single
  source of truth for default application + NOT NULL enforcement
  across all three INSERT paths.
- `tests/drizzle_compat_tests.rs` — six B24 / B25 / B26 regression
  cases (DEFAULT with function call, DEFAULT with literal, DEFAULT
  VALUES, explicit NULL rejected, omitted NOT NULL rejected, NOT NULL
  satisfied by default). All 24 compat tests passing; 1730 lib tests
  unchanged.

## [3.14.2] - 2026-04-21

### Fixed — real-driver blockers found during v3.14.1 retest

- **B22 `Flush` (`H` / 0x48) message** is now a first-class
  `FrontendMessage` variant. Every pipelined Postgres driver
  (postgres-js, `pg`, psycopg internally, Npgsql, JDBC) emits
  `Parse → Bind → [Describe →] Execute → Flush` on every query and
  then waits for the server to push the ParseComplete / DataRows /
  CommandComplete before sending `Sync`. Without `Flush`, the driver
  is killed mid-query and the TCP connection goes down.
  The dispatch just flushes the socket buffer — no ReadyForQuery
  (that's `Sync`'s job). Verified end-to-end via `postgres-js 3.4.5`
  over TCP — connect + `SELECT version()` + parameterised
  `pg_catalog.pg_type` lookup + `pg_tables` with `NOT IN` filter all
  complete cleanly.
- **B23 scalar subquery in `UPDATE … SET`** (correlated + uncorrelated).
  `Expr::Subquery` is now a `LogicalExpr::ScalarSubquery` variant and
  the UPDATE executor materialises it per row:
  1. Walk the subquery plan, replace every
     `Column { table: Some(<outer_table>), name }` with the literal
     value from the current outer row.
  2. Execute the (now uncorrelated) plan and take the first column
     of the first row; return `NULL` if zero rows.
  Handles the canonical Drizzle-migration rewrite pattern from
  `docs/compatibility/plpgsql.md`:
  `UPDATE user_profile SET display_name =
   (SELECT email FROM users WHERE users.id = user_profile.user_id);`

### Added

- `tests/drizzle_compat_tests.rs` — three B23 regression cases
  (correlated with outer ref, uncorrelated aggregate, empty
  subquery → NULL). All 18 compat tests passing; 1730 lib tests
  unchanged.

## [3.14.1] - 2026-04-20

### Fixed — TimeTracker retest follow-ups

- **B19 pg_catalog visible on extended query protocol.**
  `PgCatalog::handle_query` now runs from the
  `Parse → Bind → Execute` path as well as the simple-Q path.
  `postgres-js`, `pg`, `psycopg` and every other real driver does its
  connect-time type introspection through the extended protocol;
  without this fix they got a bogus
  `Table 'pg_catalog.pg_type' does not exist` and couldn't connect.
- **B20 catalog queries honor WHERE.** The emulator used to return
  the full table and rely on projection-only filtering. Added a
  small WHERE-clause evaluator that handles `col = 'lit'`, `col = N`,
  `col <> 'lit'`, `col != 'lit'`, `col IN (…)`, `col NOT IN (…)`
  and left-to-right conjunctions. Covers every driver introspection
  query we've seen; complex WHEREs (OR, function calls, subqueries)
  fall through unchanged (keeps all rows).
- **B21 clear error for PL/pgSQL DO bodies.** `DO $$ DECLARE / IF /
  LOOP / FOR / RAISE / := … $$` now returns a targeted error
  identifying the unsupported keyword and pointing at
  `docs/compatibility/plpgsql.md`. Silent no-op would corrupt
  migrations — this version still refuses, but with a clear message
  and migration-rewrite recipes.

### Added

- `docs/compatibility/plpgsql.md` enumerates supported / unsupported
  PL/pgSQL features and gives rewrite recipes (backfill loop →
  `INSERT … SELECT`, conditional index → `CREATE INDEX IF NOT
  EXISTS`, conditional insert → `ON CONFLICT DO NOTHING`).
- `tests/drizzle_compat_tests.rs` notes B19/B20/B21 regression is
  live-verified at the wire level (psql smoke tests) — the core
  `EmbeddedDatabase::query` API doesn't touch the PG wire handler so
  those tests belong on the integration path rather than the unit
  suite.

## [3.14.0] - 2026-04-20

### Fixed — Drizzle / Prisma / TypeORM compatibility (tracks `BUGS_TIMETRACKER_DRIZZLE_COMPAT.md`)

- **B2 `GENERATED ALWAYS AS IDENTITY`**: planner now recognises the
  SQL-standard identity syntax and routes it through the same
  auto-fill path as `SERIAL`.
- **B3 `DEFAULT` keyword in `INSERT ... VALUES`**: sqlparser classifies
  `DEFAULT` as `Expr::Identifier`; the planner now rewrites it to
  `NULL` inside VALUES lists so the existing SERIAL / default-value
  path fires.
- **B4 RETURNING field-count**: fixed a long-standing bug in
  `execute_plan_with_params` where INSERT rows with omitted columns
  produced short tuples, causing the PG wire protocol to emit a
  `DataRow` with a different field count than the `RowDescription`.
  Every `.returning()` call through Drizzle / psycopg is affected.
- **B5 `EXTRACT(EPOCH|YEAR|MONTH|... FROM ...)`**: full coverage in the
  evaluator — Epoch returns Float8 (Unix seconds); calendar fields
  return Int4. `TIMESTAMP '2026-01-01'` and friends now parse (new
  `TypedString` planner arm that lowers to a CAST).
- **B7 `CREATE SEQUENCE`**: DDL is accepted and registers a named
  counter in the new process-scoped sequence store
  (`sql::sequences`). Persistent sequences are a follow-up.
- **B8 `nextval` / `currval` / `setval`**: scalar functions backed by
  the sequence store; always return Int8.
- **B9 `DO $$ … END $$` blocks**: the PG handler unwraps the
  dollar-quoted body and executes plain-SQL statements inside via a
  single `DO` CommandComplete. PL/pgSQL control flow (IF / LOOP /
  RAISE) is NOT interpreted — documented as out of scope.
- **B10 dollar-quoted string literals**: `$$text$$` and `$tag$text$tag$`
  values map to `Value::String` in the planner.
- **B11 multi-statement simple queries**: the `Q` message now accepts
  `;`-separated statements and emits one response per statement with a
  single trailing `ReadyForQuery`, matching PG protocol.
- **B14 identifier case-folding**: new `Planner::normalize_ident` and
  `normalize_object_name` helpers strip surrounding quotes
  (preserving case) and lower-case unquoted identifiers. Applied at
  every DDL and reference call site — `CREATE TABLE Foo` matches
  `SELECT FROM foo` matches `SELECT FROM FOO`, while quoted
  `"Foo"` stays case-sensitive (PG-compliant).
- **B15 `gen_random_uuid()` / `uuid_generate_v4()`**: new scalar
  functions returning `Value::Uuid`.
- **B17 startup banner**: now points to `docs/compatibility/`, the
  FTS doc, and the new `heliosdb_capability_report()` probe so
  drivers / migration tools can discover supported features before
  bisecting failures.

### Added

- **`heliosdb_capability_report()`** scalar function — returns a
  human-readable summary of what this server version supports vs.
  stock Postgres.
- **`src/sql/sequences.rs`** — process-scoped, thread-safe counter
  store shared by `CREATE SEQUENCE` / `nextval` / `currval` /
  `setval`.
- **`tests/drizzle_compat_tests.rs`** — 15 regression cases, one per
  bug in the `BUGS_TIMETRACKER_DRIZZLE_COMPAT.md` report.

### Query-engine changes

- Result cache now skips SQL that contains non-deterministic
  functions (`nextval`, `setval`, `currval`, `gen_random_uuid`,
  `random(`, `now(`, `clock_timestamp`). Previously, a second call
  returned the first result verbatim.

## [3.13.0] - 2026-04-19

### Added — PostgreSQL-compatible full-text search

- **Scalar FTS functions**: `to_tsvector(text)`,
  `to_tsvector(config, text)`, `to_tsquery(text)`,
  `plainto_tsquery(text)`, `phraseto_tsquery(text)`, `ts_rank(doc,
  query)`, `ts_rank_cd(doc, query)` — all implemented in
  `src/sql/evaluator.rs`. Values round-trip as `Value::Json` (array of
  normalised tokens) so they flow through the PostgreSQL wire
  protocol unchanged and render as JSON arrays for introspection.
- **`@@` operator** (`tsvector @@ tsquery → boolean`): new
  `BinaryOperator::TsMatch` in the logical plan, wired in the planner
  from `SqlBinaryOp::AtAt` and evaluated via the shared
  `search::tokenizer` + in-memory match.
- **`TSVECTOR` / `TSQUERY` column types**: accepted in `CREATE TABLE`
  (`src/sql/planner.rs:3044`). Stored as `DataType::Json` internally.
- **`CREATE INDEX ... USING gin | gist (col)`**: accepted as DDL for
  ORM/migration compatibility (`src/sql/executor/ddl.rs:79`). The
  index is currently a no-op — `@@` still walks rows in the evaluator
  — but the syntax round-trips cleanly so Django, SQLAlchemy, and
  hand-written migrations load without errors.
- Backed by `search::Bm25Index` (landed in v3.11.0), which had been
  unreachable from SQL until now.

### Fixed

- **Stale version strings**. `pg_catalog.version()`, the
  `server_version` parameter-status message, and the `SHOW
  server_version` response all now use `env!("CARGO_PKG_VERSION")`
  instead of the hardcoded `3.7.0` / `3.10.0` / `17.0 (HeliosDB-Lite
  2.0)` strings that had drifted across releases.

### Documentation

- New `docs/compatibility/fts.md` — honest scope of our FTS support:
  what works (token match, BM25 rank, JSON-encoded tsvector),
  what doesn't (stemming, phrase queries, `setweight()`, persistent
  GIN index), and the migration hook for when it does.
- `tests/fts_tests.rs` (8 regression cases): tsvector construction,
  `@@` match/miss, rank scoring, `GIN` DDL acceptance, null
  propagation, version-string drift.

### Tracks

- Request from the EasyRAG team (`foor.network/easyrag`) — their
  adapter (`backend/app/services/vectordb/adapters/heliosdb_nano_adapter.py`)
  was client-side reranking with `rank_bm25.BM25Okapi` to work
  around the missing FTS functions. Simplification guide published
  at `easyrag/docs/heliosdb_nano_adapter_simplification.md`.

## [3.12.0] - 2026-04-17

### Fixed

- **`LIMIT $1 OFFSET $2` via psycopg extended query protocol** (root
  cause of SQLAlchemy's `NotImplementedError: _row_as_tuple_getter`).
  The planner's `expr_to_usize` rejected `Expr::Value(Placeholder(_))`,
  which made Parse-time schema derivation fail and caused `Describe` to
  send `NoData` instead of `RowDescription`. Now accepts placeholders
  (the real values are substituted at Execute time before planning).
- **Fallback `RowDescription` for `SELECT`**: if schema derivation
  still fails for an exotic query, we now synthesise a best-effort
  schema from the sqlparser projection list rather than returning
  `NoData` — matching PostgreSQL's behaviour and keeping SQLAlchemy
  row decoders happy.

### Added — Pagination

- **Top-K operator** (`sql::executor::topk::TopKOperator`): streams the
  input through a bounded max-heap of size `k = limit + offset` when
  the plan is `Limit(Sort(…))` or `Limit(Project(Sort(…)))`.
  Complexity drops from O(N log N) to O(N log k) and memory from O(N)
  to O(k). Kicks in automatically whenever the `LIMIT` has a concrete
  bound.
- **Row-constructor comparison** for keyset pagination:
  `WHERE (created_at, id) < ($1, $2) ORDER BY created_at DESC, id DESC LIMIT N`
  is now planned and evaluated lexicographically. New
  `LogicalExpr::Tuple` variant and `evaluate_tuple_compare` in the
  evaluator. Supports `=`, `<>`, `<`, `<=`, `>`, `>=`.
- **Storage-level OFFSET pushdown** (`storage::EmbeddedStorage::scan_table_with_offset_limit`):
  skips `offset` rows at the RocksDB iterator level *without*
  deserialising them (no bincode, no decrypt, no dict/CAS resolve) and
  then fetches `limit` rows fully. Markon's `LIMIT 5 OFFSET 990` on
  1000 rows now returns in ~1 ms (previously required materialising
  all 995+ rows before the `LimitOperator` skipped).
- **Primary-key range scan API**
  (`storage::EmbeddedStorage::scan_table_pk_range`): low-level building
  block for future planner-driven keyset pushdown; currently exposed
  for callers that know the PK range up front.

### Changed

- `LogicalExpr` gains a `Tuple { items }` variant — every consumer
  (`optimizer::rules`, `optimizer::cost`, `sql::type_inference`,
  `sql::evaluator`) handles it.
- Pagination integration test suite (`tests/pagination_tests.rs`, 7
  tests) lands with the feature, covering empty tables, ORDER BY,
  LEFT OUTER JOIN, keyset, row-constructor equality, and large-offset
  correctness.

### Tracks

- `FEATURE_REQUEST_pagination.md` — acceptance criteria 1–5 met;
  cross-engine benchmark (vs Postgres / Oracle / MSSQL) and a website
  marketing page tracked as follow-up (see task #122).

## [3.11.0] - 2026-04-15

### Added (RAG-native integration -- 5 features)

- **Per-request bump arena** (`runtime::RequestArena`, idea 3): wraps
  `bumpalo::Bump` so transient buffers (HNSW candidate lists, scratch
  rows, BM25 term lists) are dropped wholesale when a request finishes
  -- amortising deallocation cost to a single `free`. New crate dep:
  `bumpalo` (with `collections` feature).
- **Native graph adjacency lists** (`graph::*`, idea 1): in-memory
  `GraphStore` backed by `dashmap` with O(1) edge insert / O(1)
  per-node neighbor lookup, plus `traverse` module implementing BFS,
  Dijkstra (non-negative weights), and bidirectional BFS, all gated
  by `TraversalLimits` to bound runaway queries.
- **BM25 + hybrid search + RRF/MMR** (`search::*`, idea 2):
  Unicode-aware tokenizer, in-memory inverted-index BM25 with
  configurable `(k1, b)`, Reciprocal Rank Fusion + Maximal Marginal
  Relevance rerankers, and `hybrid_search` orchestrator that fuses
  BM25 + vector hits via RRF / MMR / weighted-linear. Deterministic
  tie-breaks on doc_id throughout. New crate dep:
  `unicode-segmentation`.
- **Compiled query plans** (`sql::compiled::CompiledPlanCache`, idea
  4): LRU-bounded cache of parser output keyed by plan name.
  `PREPARE COMPILED <name> AS <sql>` + `EXECUTE <name>` surface
  recognised by `parse_prepare_compiled` / `parse_execute` /
  `try_handle_compiled`.
- **MCP idea-5 tools + resources** (`mcp_extensions::*`, idea 5):
  six new tools (`heliosdb_bm25_index`, `heliosdb_hybrid_search`,
  `heliosdb_graph_add_edge`, `heliosdb_graph_traverse`,
  `heliosdb_graph_path`, `heliosdb_embed_and_store`) plus two
  resource resolvers (`heliosdb://schema/{table}`,
  `heliosdb://stats/{table}`). Lives in a standalone module pending
  the legacy `src/mcp/` server's reconciliation with current
  EmbeddedDatabase API -- see `BLOCKER_idea_5.md`.

### Tests

- 43 new integration tests across 9 new test files; 56 new unit tests
  inside the new modules. Existing 1730 lib tests continue to pass.

## [3.10.0] - 2026-04-14

### Added
- Implicit comma-joins: `FROM t1, t2 WHERE t1.id = t2.id` now works
  (treated as CROSS JOIN + WHERE filter). WordPress uses this pattern
  for _update_post_term_count during tag/category operations.

### Fixed
- ALTER TABLE ADD KEY/INDEX with prefix lengths now silently accepted
  (stripped by translator). WordPress dbDelta() schema checks no longer error.

## [3.9.9] - 2026-04-11

### Fixed
- **WHERE ID = '1' returns 0 rows** (root cause of wp_capabilities not written):
  `coerce_pk_value()` handled Int→Int widening but NOT String→Int coercion.
  WordPress `$wpdb->prepare("WHERE ID = %s", 1)` produces `WHERE ID = '1'`.
  The ART index lookup received `String("1")` which didn't match stored
  `Int8(1)`. Added String→Int8/Int4/Int2 parsing in coerce_pk_value().
  This was the last piece: get_userdata(1) now finds the user, wp_insert_user()
  writes capabilities, and the full install chain completes.

## [3.9.8] - 2026-04-10

### Fixed
- MySQL double-quoted string literals: WordPress $wpdb->prepare() can produce
  VALUES with double-quoted strings ("a:1:{s:13:\"admin\";b:1;}"). These were
  passed through as identifier quotes, causing silent data loss. Translator
  now detects double-quoted values in string context (after VALUES(, SET =,
  etc.) and converts them to single-quoted PG string literals with proper
  backslash escape handling. Fixes wp_capabilities not written during install.

## [3.9.7] - 2026-04-10

### Fixed
- ON CONFLICT DO UPDATE now handles UNIQUE key conflicts (not just PK).
  WordPress wp_options has option_id as PK and option_name as UNIQUE.
  The conflict is on option_name but the old code only looked up by PK
  (which was NULL/auto-generated). Now scans UNIQUE columns for the
  conflicting value, falls back to PK lookup.
  Fixes update_option(), transients, rewrite rules, cron.

## [3.9.6] - 2026-04-10

### Fixed
- **CRITICAL REGRESSION**: Semicolons inside single-quoted strings were treated
  as statement terminators, breaking all WordPress serialized PHP data
  ('a:1:{s:13:"administrator";b:1;}'). Rewrote execute_dml SQL splitting to
  use quote-aware parser instead of naive .split(';').
  128 parse errors during install → 0.

## [3.9.5] - 2026-04-10

### Added
- **Native ON CONFLICT DO UPDATE / DO NOTHING** in planner and executor.
  No more handler-level INSERT-catch-UPDATE workaround. Supports both
  PostgreSQL `ON CONFLICT` and MySQL `ON DUPLICATE KEY UPDATE` syntax
  natively through the planner with EXCLUDED.col reference resolution.
- `OnConflictAction` enum in LogicalPlan::Insert (DoNothing, DoUpdate)
- MySQL translator now produces proper `ON CONFLICT DO UPDATE SET col = EXCLUDED.col`
  instead of stripping the clause
- 10 new upsert tests covering DO NOTHING, DO UPDATE, EXCLUDED refs,
  multi-column, partial update, and no-conflict paths

## [3.9.4] - 2026-04-10

### Fixed
- ON DUPLICATE KEY UPDATE: UNIQUE KEY constraints now preserved (converted to
  UNIQUE(col) instead of stripped). UNIQUE flag propagated to column defs.
  Duplicate INSERT now correctly triggers UPDATE fallback.
- SHOW INDEX: returns UNIQUE indexes from table constraints in addition to
  PRIMARY key entries. WordPress dbDelta() can now detect existing indexes.
- Multi-table DELETE: generates two separate DELETE...IN(subquery) statements
  instead of PostgreSQL USING syntax. execute_dml splits semicolons.

## [3.9.3] - 2026-04-10

### Fixed
- **ROOT CAUSE of LAST_INSERT_ID=0 and all WordPress content creation failures:**
  Table-level `PRIMARY KEY (col)` constraint (used by WordPress in all CREATE TABLE)
  was not propagated to the column's `primary_key` flag. Only inline `col INT PRIMARY KEY`
  was handled. The column was stored as a regular nullable BIGINT — no auto-fill,
  no sequence, no insert_id. Fixed by propagating PK from table-level constraints
  to column defs in the planner's create_table_to_plan().

## [3.9.2] - 2026-04-09

### Fixed
- MySQL wire protocol column type: bigint columns returned MYSQL_TYPE_NULL (type 6)
  because column type was inferred from the first row's value (NULL for auto-generated
  PK). Now scans all rows for first non-NULL value to determine correct type.
  This was the root cause of insert_id=0, WHERE ID=N returning 0 rows, and all
  content CRUD appearing to succeed but returning id=null.

## [3.9.1] - 2026-04-09

### Fixed
- KEY index regex matched inside column names (meta_key → corrupted DDL).
  Regex now requires comma anchor so only standalone KEY definitions match.
- Bigint equality: WHERE ID = 1 failed because Int4(1) literal didn't match
  Int8 PK in ART index. Added PK type coercion in get_row_by_pk_inner().
- Duplicate PK detection: insert_tuple_fast wrote data BEFORE checking
  constraints, silently creating duplicates. Now checks PK+UNIQUE first.
- check_unique_constraints() now covers pk_indexes (was only checking
  unique_indexes, missing PK violations entirely).
- ON DUPLICATE KEY handler: case-insensitive error detection for dup matching.
- 5 new WordPress-specific regression tests.

## [3.9.0] - 2026-04-08

### Fixed (WordPress zero-drop-in milestone)
- LAST_INSERT_ID: PK columns now auto-fill with row_id across ALL insert paths
  (transactional, fast, versioned, branch-aware). Missing PK in INSERT column list
  now generates NULL placeholder instead of erroring.
- DEFAULT CHARSET/COLLATE: translator now handles `DEFAULT CHARACTER SET utf8mb4`
  (with spaces) and `DEFAULT CHARSET=utf8mb4` (with equals) correctly
- ON DUPLICATE KEY UPDATE: implemented upsert via INSERT-then-UPDATE-on-conflict
  pattern in MySQL handler (planner lacks ON CONFLICT support, so handler detects
  duplicate error and falls back to UPDATE)
- SELECT VERSION(): MySQL handler now intercepts and returns MySQL-format
  "8.0.35-HeliosDB-Nano" instead of falling through to PG evaluator
- USE database: SQL-level `USE dbname` now accepted silently (was only handled
  at binary protocol COM_INIT_DB level)
- SHOW INDEX: fixed table name extraction to handle backtick-stripped and
  database-qualified names

## [3.8.3] - 2026-04-08

### Fixed
- SELECT alias.* in JOINs: added QualifiedWildcard handling in planner so
  `SELECT t.*, tt.* FROM wp_terms AS t JOIN wp_term_taxonomy AS tt ON ...`
  correctly expands to all columns of each aliased table (13/15 → 15/15)
- SHOW FULL COLUMNS: now returns all 9 MySQL fields including Collation
  (utf8mb4_unicode_ci), Privileges, and Comment. WordPress wpdb::get_col_charset()
  can now determine column charsets without falling back to bypass mode

## [3.8.2] - 2026-04-08

### Fixed
- SERIAL/BIGSERIAL columns now auto-fill with row_id when NULL on INSERT.
  This was the root cause of LAST_INSERT_ID() returning 0 — the column
  stayed NULL because only the storage-level row_id was generated, not the
  SQL-level column value. MAX(pk) now returns the correct ID.
- INNER JOIN cross-type hashing: Int4(1) and Int8(1) now hash identically
  and compare equal in JoinKey, fixing empty results on SERIAL↔BIGSERIAL joins.
- Prefix key indexes: nested-paren regex handles KEY meta_key(meta_key(191)).

## [3.8.1] - 2026-04-08

### Fixed
- LAST_INSERT_ID returns 0: query_last_serial_id used double-quoted identifiers
  that caused case-sensitive mismatch with unquoted table names
- INNER JOIN returns empty results: hash join key comparison failed across integer
  widths (Int4 vs Int8). JoinKey now uses cross-type numeric coercion for both
  Hash and PartialEq, so SERIAL(Int4) joins match BIGSERIAL(Int8)
- Prefix key indexes `KEY col(191)`: regex didn't handle nested parentheses.
  Fixed pattern to match `(col(191))` correctly
- Backtick identifiers: strip entirely instead of converting to double-quotes

## [3.8.0] - 2026-04-02

### Added
- **Built-in Backend-as-a-Service layer** — REST API, Auth, OAuth, Realtime, Storage
- REST API at `/rest/v1/{table}` with 19 PostgREST-compatible filter operators
  (eq, neq, gt, gte, lt, lte, like, ilike, is, in, cs, cd, ov, fts, not, or, and)
- Auth endpoints: `/auth/v1/signup`, `/auth/v1/token`, `/auth/v1/logout`,
  `/auth/v1/refresh`, `/auth/v1/user` with JWT sessions and Argon2id hashing
- OAuth2 support for Google and GitHub (`/auth/v1/authorize`, `/auth/v1/callback`)
  with PKCE, automatic user creation, and provider linking
- Realtime WebSocket at `/realtime/v1/websocket` with Phoenix-protocol
  channel subscriptions and INSERT/UPDATE/DELETE change notifications
- Row-Level Security enforcement on REST queries using JWT claims
- `ChangeNotifier` broadcasts DML events to WebSocket subscribers
- Auth persistence: `_auth_users` and `_auth_refresh_tokens` tables in DB
- MySQL wire protocol with WordPress compatibility layer
  (SQL translator, SHOW commands, AUTO_INCREMENT, ON DUPLICATE KEY, etc.)
- 14 MySQL date/time functions (DATE_FORMAT, DATE_ADD, UNIX_TIMESTAMP, etc.)
- MySQL `$10+` parameter substitution fix
- 9 convenience methods on `EmbeddedDatabase` (branches, explain, refresh MV)

### Fixed
- Transaction read-your-writes (INSERT visible in same-transaction SELECT)
- SQLAlchemy pg_catalog.version() compatibility
- Column names (column_0 → real names) and quoted strings in PG wire protocol
- CREATE TABLE IF NOT EXISTS errors when table exists
- LAST_INSERT_ID() tracking per MySQL connection
- Backslash-quote escaping for PHP serialize() compatibility

## [3.7.0] - 2026-03-21

### Added
- INSERT ... SELECT with full constraint, trigger, FK, and RLS support
- String concatenation `||` operator with NULL propagation and auto-cast
- `generate_series(start, stop, step)` and `unnest()` table functions
- Aggregate expressions: `SUM(a)+SUM(b)`, `CAST(AVG(...) AS INT)`, `CASE` on `COUNT`
- ORDER BY aggregate sorting (rewrite aggregate refs to column aliases)
- Named window references: `WINDOW w AS (...)` with inheritance
- Multiple ALTER TABLE operations in a single statement
- 456 hardening tests across 9 test suites (null semantics, type coercion, truncate, savepoints, aggregates, string/unicode, window functions, subqueries, set operations)
- 182 additional hardening tests (JOIN, CTE, JSONB, triggers, PL/pgSQL)

### Fixed
- Recursive CTE with LIMIT (fast-path bypass skipped CTE materialization)
- Recursive CTE with COUNT(*) (storage fast-path returned 0)
- SMALLINT CAST truncation (now errors on overflow instead of silent wrap)
- DECIMAL-to-FLOAT cast corruption (now errors on precision loss)
- LIMIT + OFFSET integer overflow (saturating arithmetic)
- NULL comparisons and arithmetic return NULL (SQL three-valued logic)
- AND/OR short-circuit with proper NULL handling
- MIN/MAX on empty set returns NULL
- COUNT(col) skips NULLs (fast path restricted to COUNT(*))
- CUME_DIST uses ORDER BY keys
- SUM OVER all-NULL partition returns NULL
- ORDER BY / GROUP BY ordinal positions (SQL-92)
- INT8 checked arithmetic (no panic on overflow)
- UTF-8 fast-path parser preserves multi-byte characters
- ART index cleared on TRUNCATE
- Savepoint data rollback via write set snapshot/restore
- UPDATE/DELETE in explicit transactions use branch-aware keys
- TRUNCATE respects active transactions (buffered in write set)
- INSERT rollback properly clears ART index entries
- WAL only logs committed changes (no phantom entries during transactions)

### Improved
- Zero clippy warnings (pedantic + nursery + cargo)
- All `eprintln!` in production code replaced with `tracing` macros
- All `unwrap()` in production code replaced with safe patterns or annotated
- Zero `todo!()` or `unimplemented!()` in production paths
- 1367 lib tests, all passing

## [3.6.0] - 2026-03-01

### Added
- Performance fast paths: `try_fast_insert()`, `try_fast_update()`, `try_fast_select()`
- Result cache: 128-entry LRU with DML/DDL invalidation
- Schema cache: in-memory HashMap, pre-warmed on connection
- ART index: zero-copy PK lookups
- RocksDB tuning: 14-bit bloom filter, 16KB blocks, prefix extractor
- 21/21 benchmarks won vs PostgreSQL 13
