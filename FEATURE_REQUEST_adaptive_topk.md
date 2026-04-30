---
requested-by: heliosdb-codekb-mcp pilot — danimoya
requested-against: HeliosDB-Nano v3.22.1
priority: low
status: blocked-on-phase-3.1-scoring
date-filed: 2026-04-30
track: graph-rag / token-economy
---

> **Note (2026-04-30, post-filing).** Reading
> `src/graph_rag/search.rs:65–160` confirms the current
> `graph_rag_search` ranks results by `hop_distance` then `node_id`
> — there are no semantic scores to detect a knee on. The FR's
> design assumes the Phase 3.1 vector + BM25 hybrid scoring
> follow-up has landed (the comment at the top of `search.rs`
> already calls this out: *"The vector rerank step is a feature
> follow-up (phase 3.1)"*). Implementing knee-detection in
> advance of scoring is premature; this FR is parked until
> Phase 3.1.


# Feature Request: Adaptive top-k for `helios_graphrag_search`

## TL;DR

`helios_graphrag_search` returns a fixed `k` results. Most queries
have a long tail of low-similarity hits that the LLM doesn't read
but pays tokens for. An adaptive top-k that cuts at a cosine
distance threshold (or a knee in the score distribution) saves
~30-60 % of returned tokens on typical agentic queries.

## Motivation

Token economy is the product's headline value-prop. Over-fetching
defeats it. Concrete example from a pilot session:

```
helios_graphrag_search { seed_text: "auth token verification", k: 20 }
returned 20 rows; relevance scores: 0.92, 0.88, 0.84, 0.76, 0.61,
0.55, 0.49, 0.43, 0.41, 0.38, 0.36, 0.31, 0.28, 0.27, 0.26, 0.22,
0.20, 0.18, 0.17, 0.15
```

The first 5 are genuinely relevant. Rows 6–20 are noise; the LLM
ends up paying ~2 000 output tokens for ~600 useful and ~1 400
discarded. An adaptive cut-off at the score knee (between rows 5
and 6) would have returned 5 rows for ~600 tokens — a 70 %
saving on this single call.

## Proposed design

`helios_graphrag_search` accepts a new optional argument:

```json
{
  "seed_text": "...",
  "k": 20,
  "min_score": 0.5,           // hard cutoff
  "knee_detection": true       // enable adaptive cut at score knee
}
```

When `knee_detection = true`:

1. Compute cosine scores for the top-k candidates as today.
2. Sort descending.
3. Find the largest gap between adjacent scores where
   `gap > 1.5 × median_gap` (kneedle-like heuristic).
4. Return rows above that gap.
5. Fall back to `k` if no knee is found (uniform distribution).

Cheap to compute (O(k)), no additional DB reads. Default off (opt-in
via the new arg) so existing callers see no change.

## Acceptance criteria

- [ ] New optional args `min_score: f32` and `knee_detection: bool`
      on `helios_graphrag_search`.
- [ ] Both default to None / false — current behaviour preserved.
- [ ] When set, returned row count is ≤ requested `k` and shows a
      sharp drop at the cut-off.
- [ ] Telemetry in the tool result: `requested_k`, `returned_k`,
      `cutoff_score`.

## Non-goals

- Learnt cutoff models. Heuristic-only.
- Per-tenant calibration. Same heuristic for all callers.

## Related

- Pilot: `~/Helios/heliosdb-codekb-mcp` (the consumer side may
  expose `--knee-detection` via a tool-call default).
- `token-dashboard`'s `baseline.py` already estimates per-tool
  token costs; this FR shrinks the actual cost.
