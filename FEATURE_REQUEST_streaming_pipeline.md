---
requested-by: heliosdb-codekb-mcp pilot — danimoya
requested-against: HeliosDB-Nano v3.22.1
priority: medium
status: open
date-filed: 2026-04-30
track: code-graph / performance
related: FEATURE_REQUEST_parallel_writes.md (orthogonal — they compose)
---

# Feature Request: Streaming pipeline — overlap parse + write phases

## TL;DR

Today `code_index_with_embedder` runs phases sequentially:

```
[ triage ] → [ parallel parse ] → [ serial write ] → [ cross-file resolve ]
```

For the pilot corpus that's 3.1 s parse + 83.4 s write = 86.5 s
linear. Pipelining them — workers start writing chunk N+1's
buffers while chunk N+2 is being parsed — could push effective
total close to `max(parse, write)` instead of `sum`. On
parse-heavy corpora (lots of small files, light symbol density),
this could halve the wall clock independent of FR
`parallel_writes`.

## Motivation

Two perf levers compose multiplicatively:

1. **Parallel writes** (FR `parallel_writes`) — split the write
   phase across cores. Best for symbol/ref-dense repos.
2. **Streaming pipeline** (this FR) — overlap parse and write.
   Best for parse-heavy or per-chunk fluctuating workloads.

For repos with mixed-language content (Rust + tons of small
markdown/JSON), parse can be a non-trivial fraction of total even
after Phase 1's parallelism. Pipelining smooths the wall clock.

## Proposed design

Buffered MPMC channel between phases:

```
┌──────────────────────────────────┐
│ Triage (single-threaded)         │
│ produces ChunkInputs              │
└────────────┬─────────────────────┘
             │  bounded(N=4)
             ▼
┌──────────────────────────────────┐
│ Parser pool (rayon, par_iter)    │
│ produces ChunkParsed              │
└────────────┬─────────────────────┘
             │  bounded(N=2)
             ▼
┌──────────────────────────────────┐
│ Writer (single thread, or         │
│ parallel after FR parallel_writes)│
│ commits chunks in order           │
└──────────────────────────────────┘
```

Bounded buffers keep memory pressure predictable; back-pressure
self-regulates if writes lag.

## Acceptance criteria

- [ ] On a parse-heavy fixture (e.g. ~/Helios/Nano with markdown
      docs ratio'd up), wall clock ≤ 0.7 × (current
      parse + write).
- [ ] Output rows byte-identical to today's serial pipeline.
- [ ] Memory ceiling: peak working set ≤ 2 × per-chunk buffer size.
- [ ] Telemetry: parse_ms, write_ms, **pipeline_ms** (overlap
      saving) reported in `CodeIndexStats`.

## Non-goals

- Reordering writes. Chunks commit in input order so result-set
  ordering and FK constraints behave the same as today.
- Streaming parse (within a single file, parsing while reading) —
  out of scope; tree-sitter is fast enough per-file.

## Open questions

1. **Backpressure policy.** When the writer falls behind, do we
   stall the parser or drop chunks (re-parse on next ingest)?
   Recommendation: stall. Simpler; the writer being the bottleneck
   matches measurement reality.
2. **Interaction with `parallelism` knob.** When 2.A lands, both
   parse and write have their own pool. The pipeline is between
   pools, not within. Should parser-pool size = writer-pool size?
   Probably no — they're independent dials.
3. **Cross-file resolve in pipeline.** Currently runs after all
   writes. Could it run after each chunk write? Probably no —
   resolve needs the full symbol set.

## Related

- `FEATURE_REQUEST_parallel_writes.md` — orthogonal; both compose.
- `FEATURE_REQUEST_parallel_code_index.md` (Phase 1, v3.21.0).
- Pilot: `~/Helios/heliosdb-codekb-mcp`.
