---
reported-by: heliosdb-codekb-mcp pilot — danimoya
reported-against: HeliosDB-Nano v3.21.1
priority: high
status: open
date-filed: 2026-04-29
kind: regression / data integrity
related: FEATURE_REQUEST_parallel_code_index.md (v3.21.0), CHANGELOG `## [3.21.1]` (Tier 1.1 + 1.3)
---

# `code_index` foreign-key violation on populated KB in v3.21.1

## Symptom

Every `code_index` call against a populated KB returns:

```
WARN heliosdb_codekb_mcp::ingest: code_index failed: Constraint violation:
  Foreign key constraint 'fk__hdb_code_symbol_refs_from_symbol___hdb_code_symbols'
  violated: cannot delete row from '_hdb_code_symbols' -
  referenced by '_hdb_code_symbol_refs'
```

Preceded (immediately above) by a slow-query warning:

```
WARN heliosdb_nano: Slow query (2512ms, 14 rows):
  DELETE FROM _hdb_code_symbol_refs WHERE file_id = 1760
  duration_ms=2512 rows=14
```

The downstream pilot (`heliosdb-codekb-mcp`) catches the error at `tracing::warn!` level and continues, so callers see "no `code_index` summary" but no fatal exit.

The 2.5 s for a 14-row DELETE on `_hdb_code_symbol_refs` is itself a separate concern (covering-index gap or compaction stall) — possibly related to the same root cause.

## Reproduction

Two-line repro against any KB that already has `_hdb_code_*` rows for at least one file in the corpus:

```bash
heliosdb-codekb-mcp init --source <repo> --mode co-located
heliosdb-codekb-mcp ingest --source <repo>          # first call — succeeds (fresh KB)
heliosdb-codekb-mcp ingest --source <repo>          # second call — FK violation
```

Or directly against the engine's `EmbeddedDatabase::code_index` API: any second invocation against a corpus that produced `symbol_refs` rows on the first invocation will fail.

## Confirmed environments

| Engine version | Fresh KB | Populated KB |
|---|---|---|
| v3.19.1 | works | works (12+ hours of pilot use) |
| v3.21.0 | works | works (Phase 2.5 smoke run reported `unchanged=21 parsed=2`) |
| **v3.21.1** | **works** | **FAILS** — FK violation (this bug) |

So the regression sits between v3.21.0 and v3.21.1. The CHANGELOG for v3.21.1 lists Tier 1.1 (single-transaction write phase) and Tier 1.3 (TRUNCATE fast path for `force_reparse`) — both touched the write path; one of them likely changed the operation order such that symbols are deleted before the corresponding refs. Hypothesis (unverified): the new chunked single-transaction commit reordered `DELETE FROM _hdb_code_symbols WHERE file_id = ?` ahead of `DELETE FROM _hdb_code_symbol_refs WHERE file_id = ?`, or the trigger/cascade that used to clean up refs first was lost.

## Impact

`code_index` is unusable for **incremental re-ingest**, which is the daily workflow:

- `helios-code-graph:refresh` (the plugin's slash command) — broken on every refresh after the first.
- The `code_graph::git_hook::run_from_stdin` post-commit hook — broken on every commit after the first.
- `heliosdb-codekb-mcp ingest` without `--force` — broken on every re-ingest.
- Any agent driving incremental updates is silently producing stale state.

Force-reparse (`--force`, which goes through Tier 1.3 TRUNCATE fast path) appears to take longer to surface the issue but the resulting summary still lacks `code_index` stats — likely the same root cause (symbol delete before refs) under a different code path. Worth double-checking with `RUST_LOG=warn` whether the TRUNCATE path errors too or whether something else happens.

## Verification on this host

- Engine: `~/Helios/Nano` at `version = "3.21.1"`.
- Plugin: `heliosdb-codekb-mcp` with `path = "../Nano"` (so it links the engine source directly).
- Pilot KB: `/tmp/nano-bench-rollback` (registered via `init --mode hybrid --kb /tmp/nano-bench-rollback`).
- Repro: `RUST_LOG=warn heliosdb-codekb-mcp ingest --source /home/app/Helios/Nano/src/code_graph` reproduces in ~85 s on this host.

## Acceptance criteria

- [ ] Per-file DELETE order in `code_index_with_embedder` (and any other `_hdb_code_*` mutator) is: refs first, symbols second, ast_nodes third, files last (or whatever order satisfies the FK closure). Equivalent: cascading DELETE on the parent rows.
- [ ] `cargo test --release --features code-graph code_graph_phase2` exercises the
       "ingest twice against the same KB" path (today's existing fixture only ingests once).
- [ ] On a populated KB, the second `ingest` produces a `code_index` summary line
       (`files_seen=N parsed=M unchanged=K …`) with **no swallowed warnings** at `RUST_LOG=warn`.
- [ ] `force_reparse = true` against a populated KB completes in seconds, not minutes
      (the original Tier 1.3 promise).
- [ ] The 2.5 s / 14-row DELETE warning is investigated separately — likely a covering-index gap on `(file_id)` for `_hdb_code_symbol_refs`. Even with FK ordering fixed, that's slow.

## Suggested next steps for the engine team

1. Land a quick fix: swap the DELETE order in the per-file path. Should be a few lines.
2. Add the "second ingest" fixture before merging.
3. Investigate the slow-DELETE warning — likely a missing index on `_hdb_code_symbol_refs(file_id)`.

## Update 2026-04-29 — fresh-KB pilot reveals a second regression

Re-ran the pilot against an isolated, truly empty KB
(`/tmp/codekb-pilot-v3211-…`) to factor out any populated-KB
state. Two findings:

### Finding 1 — Tier 1.3 TRUNCATE fast path fails the same way

`heliosdb-codekb-mcp ingest --source ~/Helios/Nano --force` against
the freshly populated KB completes in 3 m 07 s but the
`ingest summary` block has **no `code_index` line** — meaning
`db.code_index()` returned `Err` and was logged at `WARN`. Same
FK violation pattern as the per-file path. So Tier 1.3 either
shares the same bug or has its own ordering issue
(`TRUNCATE _hdb_code_symbols` happening before
`TRUNCATE _hdb_code_symbol_refs`).

The CHANGELOG claim "Closes the pilot's 1 h 55 m anti-pattern
outright" is technically true in the sense that the failure
surfaces in 3 min instead of 1 h 55 m — but no work is persisted.

### Finding 2 — Cold-ingest regression vs v3.21.0

The v3.21.1 telemetry (newly added — thank you) makes the
regression unambiguous:

```
v3.21.0 cold  (no telemetry, total wall):  5 m 43 s
v3.21.1 cold:
  code_index ms : parse=3585  write=621772  workers=8  chunks=1
  total wall:                                 11 m 21 s
```

| Phase | v3.21.0 | v3.21.1 | Delta |
|---|---|---|---|
| parallel parse | ~3 s (estimated) | **3.585 s** | flat ✓ |
| serial write | ~340 s | **622 s** | **+82 % slower** ❌ |
| total | 343 s | 681 s | **+98 % slower** ❌ |

Identical corpus (666 files vs 663 — within noise), almost
identical symbol/ref counts (18 435 vs 18 400; 114 750 vs 114 662).
The only thing that changed between the two runs is the engine.

**Hypothesis.** The new `_hdb_code_*` covering indexes shipped
in v3.21.1 add per-INSERT index-maintenance cost that exceeds
the per-chunk fsync savings from Tier 1.1. The "tens-of-thousands
→ tens of fsyncs" win from Tier 1.1 is real but a smaller line-item
than the new per-row B-tree updates. Worth confirming with `top -H`
during the write phase: if rocksdb compaction threads are visible
during 90 % of the wall time, the bottleneck is index maintenance.

**Suggested investigation.**

1. Profile a v3.21.1 cold ingest with `perf record` against the
   `tests/code_graph_phase2.rs` fixture, focused on the write phase.
2. Confirm whether the covering indexes are rebuilt incrementally
   (good) or rebuilt at-end-of-transaction (bad — would explain the
   regression).
3. If incremental: consider deferring covering-index population to
   a post-ingest one-shot, so the bulk-load path uses the fast
   per-row INSERT path and gets the index in one final scan.

### Net effect

v3.21.1 is, on the pilot workload, **worse than v3.21.0** in both
measurable scenarios:

- Cold ingest: 11 m 21 s vs 5 m 43 s (regression).
- Re-ingest against populated KB: silently fails (this bug).
- Force re-parse: silently fails (this bug, surfacing through Tier 1.3).

Recommendation: **either roll v3.21.1 back, or block crates.io
publish on a v3.21.2 that fixes both issues.**

## Related

- Pilot client: `~/Helios/heliosdb-codekb-mcp` (separate repo).
- Phase 2.5f (transaction-wrapped client-side upserts) interacts with the engine's new outer-transaction detection in v3.21.1; worth confirming `db.in_transaction()` returns `true` correctly when the client opens its own BEGIN/COMMIT.
- This bug currently blocks the v3.21.1 perf measurement requested by the pilot
  (cold + force-reparse benchmarks vs the 13 m 39 s v3.19.1 baseline).
