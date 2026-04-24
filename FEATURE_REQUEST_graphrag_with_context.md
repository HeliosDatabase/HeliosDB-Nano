---
requested-by: Claude Code code-graph initiative — danimoya
requested-against: HeliosDB-Nano v3.13.x
priority: high
status: proposed
date-filed: 2026-04-23
track: code-graph
doc: 4/5
depends-on: FEATURE_REQUEST_ast_index_and_lsp.md
---

# Feature Request: Cross-modal GraphRAG schema, `WITH CONTEXT` clause, graph-weighted HNSW, and semantic Merkle indexing

## TL;DR

Promote the code-graph into a *universal* graph that also holds docs,
tickets, emails, and people; introduce a new SQL clause
`WITH CONTEXT (HOPS n, EDGES ..., RERANK BY $q, LIMIT k)` that
post-processes any result set by expanding it along graph edges and
reranking by vector similarity; and add two supporting storage
primitives (graph-weighted HNSW navigation, semantic-Merkle
invalidation) that make the flagship query pattern fast at the
`~/Helios` corpus scale.

This is the flagship query capability of the track. It is what lets
a single SQL / MCP call answer:

> *"Which functions and docs across all HeliosDB editions relate to
> revenue-recognition logic, and which investor questions have asked
> about them, with the relevant supporting emails?"*

— in one round-trip.

## Motivation

Investor-diligence, due-diligence, incident post-mortems, and
open-source audit workflows all share a shape: **gather a
semantically-relevant subgraph across heterogeneous sources, then
return it as a single answer**. Today each of these workflows is
hand-built glue on top of two or three stores. The HeliosDB-Nano
engine already has all the ingredients to make this a built-in.

Concretely: the `~/Helios` corpus contains 10 git repositories,
`Docs-Internal/`, `Docs-Public/`, `Documentation/`, `Investors/`
(with ~200 prepared questions), a website, and SDKs. A good answer
to one diligence question typically cites 3–8 sources from ≥2 of
those buckets. No separate vector DB + graph DB + rerank stack does
this neatly; HeliosDB-Nano can.

## Current state in HeliosDB-Nano

- Graph engine with typed nodes / edges / traversal:
  `src/graph/{storage.rs, traverse.rs, sql.rs}`.
- BM25 + hybrid search + reranker: `src/search/{bm25.rs, hybrid.rs, reranker.rs}`.
- HNSW + PQ for vectors: `src/vector/{hnsw_index.rs, quantized_hnsw.rs, quantization/}`.
- Hybrid SQL search as shown in `README.md:172–177`
  (`0.7 * (1.0 - (embedding <=> $1)) + 0.3 * ts_rank_cd(...)`).
- MCP graph tools ready in `src/mcp_extensions/tools.rs`:
  `heliosdb_graph_add_edge`, `heliosdb_graph_traverse`,
  `heliosdb_graph_path`.

What is missing:

- A canonical **universal-node / universal-edge** schema so disparate
  source types (code, doc chunks, emails, issues, people) can be
  joined without bespoke tables per modality.
- An **entity-linker** pass that emits cross-modal `MENTIONS` edges
  (doc text → code symbol, email → person, …).
- A SQL-level **`WITH CONTEXT`** clause that fuses vector scoring
  with graph expansion in one plan node.
- Two performance primitives needed to make the above cheap at
  `~/Helios` scale: graph-weighted HNSW navigation, and a
  semantic-Merkle invalidation index.

## Proposed design

### 4.1 Universal cross-modal schema

Sits beside `_hdb_code` (FR 2). Consumes `_hdb_code.symbols` as one
of several node populations.

```sql
CREATE SCHEMA _hdb_graph;

CREATE TABLE _hdb_graph.nodes (
    node_id      BIGSERIAL PRIMARY KEY,
    node_kind    TEXT NOT NULL,       -- 'Function','Class','DocChunk','Email','Issue','InvestorQuestion','Person','Commit',...
    source_ref   JSONB NOT NULL,      -- {"table":"_hdb_code.symbols","id":84213} or {"table":"doc_chunks","id":...}
    title        TEXT,
    text         TEXT,
    text_tsv     TSVECTOR,
    embedding    VECTOR,              -- canonical retrieval vector
    extra        JSONB                -- node-kind-specific metadata
);
CREATE INDEX ON _hdb_graph.nodes (node_kind);
CREATE INDEX ON _hdb_graph.nodes USING gin  (text_tsv);
CREATE INDEX ON _hdb_graph.nodes USING hnsw (embedding vector_cosine_ops);

-- Typed edges. Uses the underlying graph storage (src/graph/storage.rs)
-- but presents a SQL-table face for uniformity.
CREATE TABLE _hdb_graph.edges (
    edge_id      BIGSERIAL PRIMARY KEY,
    from_node    BIGINT REFERENCES _hdb_graph.nodes(node_id),
    to_node      BIGINT REFERENCES _hdb_graph.nodes(node_id),
    edge_kind    TEXT NOT NULL,       -- 'CALLS','IMPORTS','REFERENCES','MENTIONS','CITES','REPLIES_TO','ASKS_ABOUT','AUTHORED_BY',...
    weight       REAL DEFAULT 1.0,
    extra        JSONB
);
CREATE INDEX ON _hdb_graph.edges (from_node, edge_kind);
CREATE INDEX ON _hdb_graph.edges (to_node,   edge_kind);
```

Code symbols from FR 2 are **projected** into `_hdb_graph.nodes`
automatically (a materialised view, or a pair of triggers) so they
participate in cross-modal queries without being duplicated
editorially.

### 4.2 Ingestion adapters for common sources

Shipped with the `hdb_code` extension (or a new `hdb_corpus`
sibling, TBD):

| Source | Adapter function | Node kinds produced | Edge kinds produced |
|---|---|---|---|
| Markdown / rST / text files | `hdb_corpus.ingest_docs(table, text_col, opts)` | `DocChunk`, `DocSection` | `PART_OF`, `CITES` |
| Email (mbox / imap dump) | `hdb_corpus.ingest_email(table, opts)` | `Email`, `Person` | `AUTHORED_BY`, `REPLIES_TO`, `SENT_TO`, `MENTIONS` |
| Issue tracker export | `hdb_corpus.ingest_issues(table, opts)` | `Issue`, `Comment`, `Person` | `REPORTED_BY`, `REPLIES_TO`, `MENTIONS`, `FIXED_BY` |
| Investor Q&A (structured) | `hdb_corpus.ingest_qa(table, opts)` | `InvestorQuestion`, `Answer`, `Person` | `ASKS_ABOUT`, `ANSWERED_BY` |

Each adapter is thin — it just writes to `_hdb_graph.nodes/edges`
using the source-of-truth row IDs so updates propagate via CDC.

### 4.3 Entity linker (cross-modal `MENTIONS`)

Runs once after initial ingest, then incrementally via triggers on
`_hdb_graph.nodes`. For each text-bearing node, match tokens /
n-grams against:

1. **Exact qualified symbol names** (from `_hdb_code.symbols.qualified`).
2. **Short aliases** (from the same, minus module prefix) — case-sensitive.
3. **Vector-similar phrases** over `_hdb_code.symbols.body_vec` with
   a high-precision threshold (default cosine ≤ 0.18).

Emit `MENTIONS` edges from the text node to the matched symbol. The
algorithm is pluggable (`hdb_corpus.set_linker(name)` — defaults to
`hybrid_exact_vec`), and its configuration lives in a system table
so it can be retrained / retuned without touching ingestion.

The linker is **source-of-truth-aware**: when a code symbol is
deleted, its inbound `MENTIONS` are garbage-collected; when a symbol
is renamed, the edges rebind to the new `node_id` (which is stable
per FR 3 §3.1).

### 4.4 `WITH CONTEXT` clause — the flagship

New SQL clause, attachable to any `SELECT`:

```
SELECT ...
    [ FROM / JOIN / WHERE / ORDER BY / LIMIT as usual ]
WITH CONTEXT (
    HOPS <n>,
    [ EDGES <kind1>|<kind2>|... ],        -- default: all edge kinds
    [ DIRECTION in | out | both ],         -- default: both
    [ RERANK BY <expr> ],                  -- e.g. RERANK BY $query_vec
    [ EXPAND_LIMIT <k> ],                  -- per-seed cap
    [ LIMIT <k> ]                          -- final cap after rerank
);
```

Semantics (executor; planner node lives in `src/sql/logical_plan.rs`,
physical operator in `src/sql/executor/`):

1. Run the base query. Collect the resulting `node_id`s (the inner
   SELECT must be project-compatible with `_hdb_graph.nodes`, either
   directly — `SELECT * FROM _hdb_graph.nodes WHERE ...` — or
   through a rowset with a `node_id` column).
2. Starting from those seeds, traverse the graph up to `HOPS` deep,
   filtered by the allowed `EDGES` / `DIRECTION`. Accumulate a
   subgraph.
3. Score each node in the expanded subgraph by the `RERANK BY`
   expression (typically the seed's query vector).
4. Return the top `LIMIT` rows as `TABLE (node_id, score, path,
   source_ref, text, hop_distance, path_via)` — `path_via` is a
   JSON array of the edge kinds traversed.

This single clause replaces ~50–200 lines of glue that every RAG
system writes today. In the planner, it is a single
`Expand(Seed, hops, edge_filter) → Rerank(...) → Limit`
chain that composes with other operators.

Worked example:

```sql
-- "Across the whole Helios corpus, what code & docs relate to
--  'revenue attribution', and who asked about it?"
WITH seeds AS (
    SELECT node_id
    FROM   _hdb_graph.nodes
    WHERE  node_kind IN ('DocChunk','Function','Class')
      AND  embedding <-> $query_vec < 0.30
    ORDER  BY embedding <-> $query_vec
    LIMIT  40
)
SELECT node_id, node_kind, source_ref, text, score, hop_distance, path_via
FROM   seeds
WITH CONTEXT (
    HOPS 2,
    EDGES CALLS|IMPLEMENTS|MENTIONS|CITES|ASKS_ABOUT|REPLIES_TO,
    RERANK BY $query_vec,
    EXPAND_LIMIT 20,
    LIMIT 30
);
```

One query, one plan, one network round-trip.

### 4.5 Graph-weighted HNSW navigation (performance)

When a node carries both an embedding and inbound/outbound graph
edges (every code symbol does), its HNSW-navigation neighbourhood
can be biased by graph centrality. Two small changes to the HNSW
index at `src/vector/hnsw_index.rs` / `quantized_hnsw.rs`:

1. Optionally include a per-node **centrality weight** (precomputed
   PageRank or call-frequency) loaded at index build.
2. During greedy descent, tie-break near-equal candidates by the
   centrality weight rather than insertion order.

Effect on queries like §4.4: the seed set is biased toward
"important" functions (hot paths, widely imported modules) rather
than obscure test helpers. For AI-agent workloads this reliably
improves answer relevance by a meaningful margin on the pilot corpus
(to be benchmarked).

Opt-in at index creation:

```sql
CREATE INDEX ... USING hnsw (embedding vector_cosine_ops)
    WITH (centrality_col = 'pagerank', centrality_weight = 0.15);
```

### 4.6 Semantic-Merkle invalidation (performance)

Embedding is the dominant cost of reindexing. Not every edit actually
changes an embedding — whitespace, unrelated sibling functions,
reformatting. A semantic-Merkle index over `_hdb_code.ast_nodes`
tracks a content hash per subtree (see FR 2 §2.3 `subtree_hash`),
and its *role in this FR* is as a reusable HeliosDB primitive:

```sql
CREATE SEMANTIC HASH INDEX on_ast
    ON _hdb_code.ast_nodes (subtree_hash)
    WITH (rollup = 'parent_id');
```

Semantics:

- Writes to any row update the hash at `node_id` and propagate
  upward via `parent_id`.
- The index supports a fast "what changed under this subtree" query
  (`SELECT ... WHERE subtree_hash <> '<prev>'`) that drives
  incremental re-embedding and re-linking.
- Useful beyond code: markdown sections, config trees, JSON
  documents all benefit.

Incremental reindex cost on typical commits in the pilot corpus
drops by an order of magnitude. Exact numbers to be measured.

## Worked examples

**The investor-Q&A flagship query** (reprise, full form):

```sql
WITH q AS (
    SELECT node_id, embedding
    FROM _hdb_graph.nodes
    WHERE node_kind = 'InvestorQuestion' AND extra->>'code' = 'Q-042'
),
seeds AS (
    SELECT n.node_id
    FROM   q, _hdb_graph.nodes n
    WHERE  n.node_kind IN ('DocChunk','Function','Class','Email')
      AND  n.embedding <-> q.embedding < 0.30
    ORDER  BY n.embedding <-> q.embedding
    LIMIT  50
)
SELECT n.node_kind, n.source_ref, n.text, sc.score, sc.path_via
FROM   seeds
WITH CONTEXT (
    HOPS 3,
    EDGES CALLS|IMPLEMENTS|MENTIONS|CITES|ASKS_ABOUT|REPLIES_TO|AUTHORED_BY,
    RERANK BY (SELECT embedding FROM q),
    EXPAND_LIMIT 15,
    LIMIT 40
) sc
JOIN _hdb_graph.nodes n USING (node_id);
```

One call returns: the definitions and callers of relevant code, the
supporting doc sections that mention them, the emails from investors
who asked about them, the authors of those emails, and the follow-up
chains — sorted by semantic relevance to Q-042.

## Acceptance criteria

- [ ] `_hdb_graph.nodes` / `_hdb_graph.edges` created by a
      `CREATE EXTENSION` step; code symbols appear as nodes without
      being duplicated.
- [ ] `hdb_corpus.ingest_docs`, `ingest_email`, `ingest_issues`,
      `ingest_qa` adapters run against the pilot `~/Helios` corpus
      end-to-end.
- [ ] Entity linker emits `MENTIONS` edges with ≥ 80% precision on
      a hand-labelled subset of 100 doc→symbol pairs.
- [ ] `SELECT ... WITH CONTEXT (HOPS n, EDGES ..., RERANK BY ...)`
      parses, plans, and executes; planner produces an `Expand`
      node in the logical plan.
- [ ] The flagship query (§4.4 or §worked example) returns under
      500 ms on the pilot corpus.
- [ ] `CREATE INDEX ... WITH (centrality_col = ..., centrality_weight
      = ...)` accepts the option and uses it in greedy descent.
- [ ] `CREATE SEMANTIC HASH INDEX` populates and maintains
      `subtree_hash` values; incremental reindex on a 1-file change
      touches < 5% of embeddings on a 10 k-file repo.

## Non-goals

- A general-purpose graph-query language at the level of Cypher /
  Gremlin. `WITH CONTEXT` is deliberately a bounded, declarative
  clause that composes with SQL; fuller graph queries already exist
  via `src/graph/sql.rs`.
- OCR / PDF parsing in the ingestion adapters. Out of scope — use
  external extractors and feed text into `_hdb_graph.nodes.text`.
- Speech-to-text ingestion. Same reasoning.

## Open questions

1. `WITH CONTEXT` return shape — should it always be wide (`node_id,
   score, hop_distance, path_via`) and require a join back to
   `_hdb_graph.nodes`, or project through the node row automatically?
   Recommendation: wide + explicit join, for clarity.
2. Where does edge weight come from in graph-weighted HNSW when the
   graph is sparse (rarely-called functions)? Recommendation:
   fallback to uniform weight, and expose a view so ops can inspect
   which nodes fell back.
3. Interaction of `WITH CONTEXT` with `AS OF COMMIT` (FR 3) — the
   natural semantics are "temporal on both seed query and graph
   traversal". Should be explicit in docs.
4. Entity linker cost at ~1M chunks — budget and back-off strategy.

## Related

- Depends on FR 2 (`ast_index_and_lsp`) for `_hdb_code` tables.
- Benefits from FR 3 (`temporal_branch_lsp`) but does not require it.
- Exposed via FR 5 (`native_mcp_endpoint`) as a single MCP tool
  `heliosdb_graphrag_search(seed_query, hops, edges, limit)`.
