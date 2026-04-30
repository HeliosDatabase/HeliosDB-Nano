---
requested-by: heliosdb-codekb-mcp pilot — danimoya
requested-against: HeliosDB-Nano v3.22.1
priority: low
status: fixed-in-v3.22.1 (commit 3c0ce3e); stats surfaced in v3.22.2 (commit 26956ba)
date-filed: 2026-04-30
date-fixed: 2026-04-30
track: mcp / token-economy
---

# Feature Request: Server-side tool-result caching for MCP `tools/call`

## TL;DR

Within an agentic-coding session, the same `helios_lsp_definition`
or `helios_graphrag_search` is often called several times in a row
(refactor-then-verify loops, Read-then-confirm patterns). Each call
re-parses, re-traverses, re-ranks. A short-lived per-tool LRU cache
on the engine side, keyed by `(tool_name, canonical_args, kb_commit)`,
would near-zero-cost subsequent identical calls.

## Motivation

Observed in pilot transcripts:

| Pattern | Same-call repeats / session |
|---|---|
| `lsp_definition('foo')` → Read → `lsp_definition('foo')` (verify) | 2-4× |
| `graphrag_search('topic')` → expand → `graphrag_search('topic')` (broaden) | 3-8× |
| `lsp_references(N)` → review → `lsp_references(N)` (after edit) | 2-5× |

A 10–60 ms call avoided per repeat is small per-call but adds up.
The bigger win is **token consistency** — repeated calls return
identical row sets, so the agent doesn't second-guess the answer
across calls.

## Proposed design

Per-process LRU keyed by:

```rust
struct CacheKey {
    tool_name: String,
    canonical_args: String,   // canonicalised JSON (sorted keys, stable)
    kb_commit: Option<String>, // KB's last commit hash; invalidates on write
}
```

- Capacity: configurable, default 256 entries.
- TTL: configurable, default 5 minutes.
- Invalidation: any write to `_hdb_code_*` or `_hdb_graph_*`
  bumps the `kb_commit` stamp; old entries become unreachable
  (LRU evicts).
- Per-tool opt-out: tools that take side-effecting args
  (`helios_lsp_rename_apply`, `heliosdb_graph_add_edge`) skip the
  cache.

## Acceptance criteria

- [ ] Repeated identical `tools/call` against the same KB returns
      identical row sets in ≤ 1 ms (ignoring transport).
- [ ] Cache invalidates on any `INSERT/UPDATE/DELETE` against
      `_hdb_code_*` or `_hdb_graph_*`.
- [ ] Cache statistics exposed via `helios/info` (hit rate,
      eviction count) so users can tune capacity.
- [ ] No false-positive cache hits across different KBs (the
      `kb_commit` stamp prevents this).

## Non-goals

- Cross-process / distributed cache. Per-process only.
- Cache persistence. RAM-only.

## Open questions

1. **Where does the cache sit?** Inside the MCP dispatcher
   (`src/mcp/tools.rs`), wrapping `call_tool`. Doesn't change tool
   handler signatures.
2. **Argument canonicalisation.** Need to sort JSON keys + drop
   no-effect fields (e.g. `_meta.progressToken`) so equivalent
   calls dedupe.

## Related

- Pilot: `~/Helios/heliosdb-codekb-mcp`.
- `token-dashboard`'s session view would benefit — repeated calls
  would show in the journal but with per-call latency near zero.
