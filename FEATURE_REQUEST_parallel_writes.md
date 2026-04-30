---
requested-by: heliosdb-codekb-mcp pilot — danimoya
requested-against: HeliosDB-Nano v3.22.1
priority: high
status: open
date-filed: 2026-04-30
track: code-graph / performance
related: FEATURE_REQUEST_parallel_code_index.md (Phase 1, shipped in v3.21.0); v3.22.1 + Async wal_sync delivers 8× cold-ingest vs v3.19.1 baseline; Phase 2 unlocks 10k+ file repos under the 5-minute budget.
---

# Feature Request: Parallel write phase in `code_index_with_embedder`

## TL;DR

After v3.22.1 + the plugin's new Async `wal_sync_mode` default, the
write phase **still dominates the wall clock** at 83 s (vs 3 s parse)
for 666 files / 18 k symbols / 115 k refs. For 10 k+ file repos this
extrapolates to 20+ minutes — over the 5-minute UX budget. The
write loop is single-threaded by design (Phase 1 explicitly punted
on this). Phase 2 is the parallel-write follow-up.

## Motivation

Pilot data gathered today on the `~/Helios/Nano` corpus, same plugin,
same engine v3.22.1, fresh KB:

| `wal_sync_mode` | parse | **write** | total | vs Sync |
|---|---|---|---|---|
| Sync (engine default) | 3.0 s | 172.7 s | 3 m 46 s | 1.0× |
| Async (new plugin default) | 3.1 s | **83.4 s** | **1 m 42 s** | 2.21× |

Async halved the write phase by eliminating per-chunk fsync. The
remaining 83 s is RocksDB single-threaded INSERT cost for ~133 k
rows. Splitting the write phase across `min(num_cpus, 8)` workers
the same way Phase 1 split the parse phase would put us in the
**~10–20 s** range.

Extrapolation to a 10 k-file repo (linear in symbols + refs):

- Today (Async, serial write): ~20 min
- With parallel write (estimated 6–8× write speedup): **~2.5–3 min** ✓

That's the "5-minute budget on big repos" target.

## Current state

`code_graph::storage::code_index_with_embedder`:

- **Triage** (single-threaded): classify input rows.
- **Parse + extract + in-file resolve** (rayon par_iter on dedicated pool): Phase 1 win.
- **Write** (single-threaded): per-row INSERTs into `_hdb_code_files`,
  `_hdb_code_ast_nodes`, `_hdb_code_symbols`, `_hdb_code_symbol_refs`.
- **Cross-file resolve** (single-threaded — separate FR if needed).

The parse+extract phase produces self-contained per-file buffers
that are independent of each other. The write phase has serial
dependencies only through:

1. Auto-increment IDs on `_hdb_code_files` (`file_id` is referenced by
   children).
2. FK enforcement on `_hdb_code_symbol_refs.from_symbol →
   _hdb_code_symbols.node_id`.

Both can be addressed by **per-file ID pre-allocation** (allocate
`file_id` and `symbol_id` ranges up front in the triage phase, then
let workers write rows with their pre-assigned IDs in parallel).

## Proposed design

### Phase 2.A — pre-allocate IDs in triage; parallel writes per file

1. Triage phase computes per-file `(file_id, symbol_id_base)` ranges
   from existing `nextval` counters; each worker writes rows with
   IDs in its pre-allocated range so no two workers contend on the
   sequence.
2. Workers split the to-write set across the existing rayon pool
   (`parallelism`), each opening its own write-side cursor on
   `_hdb_code_*` tables.
3. Final commit fences all workers (one fsync of all chunks per
   chunk boundary, same as today).

### Phase 2.B — sharded `_hdb_code_*` (optional, larger lift)

If 2.A is bottlenecked by RocksDB compaction on a single column
family, shard the four tables across N column families keyed by
`file_id % N`. Engine reads stitch transparently.

## Acceptance criteria

- [ ] Wall-clock wins on the pilot corpus: write phase ≤ 20 s (down
      from 83 s on Async). Total cold ingest ≤ 30 s.
- [ ] No correctness regression — `_hdb_code_*` rows byte-identical
      to today's output when `parallelism = Some(1)`.
- [ ] No FK violations during the parallel write (the v3.22.1 fix
      remains valid).
- [ ] Test fixture: ingest the engine's own `src/` corpus
      (`tests/code_graph_phase2.rs`-style), check symbol + ref
      counts match the serial baseline.
- [ ] Telemetry: add `code_index ms : write_par_ms` so callers see
      the gain.

## Non-goals

- **Multi-writer transactions across processes.** Single-process
  multi-threaded only. Cross-process is FR `cross_process_on_conflict`'s
  territory.
- **Lock-free write path** (the engine has `src/storage/lockfree/`,
  but adopting it for `_hdb_code_*` is a larger refactor).

## Open questions

1. **Per-table column families.** Today `_hdb_code_*` likely share
   one CF. Splitting into four CFs (one per table) is a
   prerequisite for parallel write to actually scale; otherwise the
   workers contend on the same CF write lock. Worth measuring
   first.
2. **WriteBatch coalescing.** RocksDB's `WriteBatch` with
   `disableWAL` could combine the writes of one chunk into a single
   atomic batch — even on a single thread, this would help. Worth
   benchmarking before going parallel.
3. **Memory ceiling under parallel write.** Each worker holds its
   own chunk's symbol + ref buffer until commit. For 10 k-file repos
   this is bounded; still worth a sanity check.

## Related

- Pilot client: `~/Helios/heliosdb-codekb-mcp` — has been switched
  to default Async `wal_sync_mode` (v1.2 plugin release).
- `FEATURE_REQUEST_parallel_code_index.md` (Phase 1, shipped in
  v3.21.0).
- `FEATURE_REQUEST_streaming_pipeline.md` (orthogonal — overlap
  parse and write rather than parallelise within write).
- `BUGS_CODE_INDEX_FK_VIOLATION_v3_21_1.md` (closed in v3.22.1).
