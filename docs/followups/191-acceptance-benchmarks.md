# Task 191 — Acceptance benchmarks + binary size guard

## Goal

Run the four FR acceptance criteria end-to-end so we can claim
"ships within budget" rather than just "ships".

## Acceptance

| FR criterion | Mechanism |
|---|---|
| Flagship `WITH CONTEXT` query < 500 ms on 10k-node fixture | `benches/with_context_bench.rs` via Criterion |
| Entity linker ≥ 80% precision on 100 hand-labelled pairs | `benches/linker_precision.rs` + `tests/fixtures/linker_pairs.json` |
| Default-feature binary +5% guard | `scripts/binary-size-check.sh` invoked from CI |
| MCP conformance handshake | `tests/mcp_conformance.rs` against `mcp-spec` test vectors |

## Design

### `benches/with_context_bench.rs`

* Build a 10k-node fixture: 8k `DocChunk` + 2k `code_symbol`
  projections, ~30k edges across `CALLS / IMPORTS / MENTIONS`.
* Pre-warm Nano (single open, indices built, statistics fresh).
* Criterion timer around `db.query("SELECT … WITH CONTEXT (HOPS 2,
  EDGES …, RERANK BY $q, LIMIT 30)")` for 100 queries.
* Asserts mean < 500 ms; fails the test if the budget regresses.

### `benches/linker_precision.rs`

* `tests/fixtures/linker_pairs.json` — 100 hand-labelled pairs of
  `(graph_node text, expected code_symbol qualified)`.
* Run `link_exact_qualified` against the corpus, count
  precision = correct / proposed. Asserts ≥ 0.8.
* Vector-similar precision is run separately when `code-embed`
  is enabled.

### `scripts/binary-size-check.sh`

* Builds two binaries:
  1. `cargo build --release` (default features only)
  2. `cargo build --release --features code-graph,graph-rag,mcp-endpoint`
* Records size of each `target/release/heliosdb-nano`.
* Compares against committed baseline in `docs/followups/binary-size-baseline.json`.
* Fails if default-feature size grew > 5%.
* CI calls this on every PR.

### `tests/mcp_conformance.rs`

* Boots the MCP stdio server in-process.
* Drives the canonical handshake: `initialize`, `tools/list`,
  `tools/call`, `resources/list`, `resources/read`, `ping`.
* Compares response shapes against the MCP 2024-11-05 spec
  schemas (vendored in `tests/fixtures/mcp_spec/`).
* Fails on shape divergence.

## Files to touch

* `benches/with_context_bench.rs` — new.
* `benches/linker_precision.rs` — new.
* `scripts/binary-size-check.sh` — new.
* `docs/followups/binary-size-baseline.json` — committed reference.
* `tests/mcp_conformance.rs` — new.
* `tests/fixtures/linker_pairs.json` — new.
* `tests/fixtures/mcp_spec/*` — vendored.
* `Cargo.toml` — `[[bench]]` entries.

## Tests

The benches *are* the tests; each panics on budget breach.

## Out of scope

- Continuous benchmarking dashboard. CI runs the suite per PR;
  trend tracking is left for a separate doc.
- Multi-platform sizing (macOS / Windows). Linux x86_64 only here.
