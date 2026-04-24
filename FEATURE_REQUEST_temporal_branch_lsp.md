---
requested-by: Claude Code code-graph initiative — danimoya
requested-against: HeliosDB-Nano v3.13.x
priority: medium
status: proposed
date-filed: 2026-04-23
track: code-graph
doc: 3/5
depends-on: FEATURE_REQUEST_ast_index_and_lsp.md
---

# Feature Request: Temporal and branch-aware LSP (`AS OF COMMIT` / `AS OF TIMESTAMP`, per-branch AST)

## TL;DR

Extend the `lsp_*` stored functions (FR 2) so they honour HeliosDB's
existing branching (`src/storage/branch.rs`) and time-travel
(`src/storage/engine_timetravel_extension.rs`) machinery. Let an AI
agent ask not only *"where is `X` defined?"* but *"where was `X`
defined on `main` three months ago?"* or *"how does `X` differ on
`feature/new-auth` vs. `main` right now?"* — as one SQL call each.

This is the first capability in the track that is strictly **more
than any LSP server or indexer can do today** (Serena, Sourcegraph
SCIP, built-in rust-analyzer / pyright / tsserver all operate on the
current working tree only). It falls out nearly for free because
HeliosDB already has the underlying row-level temporal/branching
primitives.

## Motivation

Use cases that are painful today and trivial with this FR:

1. **Investor diligence** — "when did the vector quantisation code
   first ship?" → `SELECT path, line FROM lsp_definition('ProductQuantizer') AS OF COMMIT 'v3.10.0';`
2. **Regression hunting** — "the behaviour changed between v3.12 and
   v3.13; what references of `retrieve_topk` moved?" → diff the
   result of `lsp_references` at two SHAs.
3. **Branch review** — "show me every caller of the function I'm
   about to change on `feature/new-auth`." → `SET branch = 'feature/new-auth'; SELECT * FROM lsp_call_hierarchy(...);`
4. **Code-archaeology Q&A** — "what did `UserRepository::save` look
   like one year ago?" → `lsp_hover(symbol_id) AS OF TIMESTAMP '2025-04-23'`.

No other code-graph tool on the market answers any of those without
checking the branch out to disk and rebuilding its index.

## Current state in HeliosDB-Nano

- Branches are a storage primitive. Branch-aware CoW machinery lives
  in `src/storage/branch.rs`; there is already a session-level
  branch selector (legacy MCP had `db.query_branch(branch, sql,
  params)` per `BLOCKER_mcp_legacy.md`, now folded into the unified
  `query` with a session setting).
- Time-travel queries are implemented in
  `src/storage/engine_timetravel_extension.rs`. They accept `AS OF
  COMMIT '<sha>'` or `AS OF TIMESTAMP '...'` against any table
  backed by the MVCC engine (`src/storage/mvcc.rs`).
- Git integration (`src/git_integration/{commit_tracker.rs, diff/,
  hooks/, ddl_versioning/}`) already wires HeliosDB commits to the
  host git repository — the "current SHA" is already known to the
  engine.

What is missing:

- The `lsp_*` stored functions from FR 2 must *propagate* the temporal
  and branch context to their underlying scans and graph traversals.
- A small set of **diff helpers** that make multi-point queries
  ergonomic.

## Proposed design

### 3.1 Temporal-aware `lsp_*`

Any `lsp_*` call accepts optional `at_commit` / `at_timestamp`
arguments, mirroring the engine-level `AS OF` syntax:

```sql
-- commit-pinned
SELECT * FROM lsp_definition(
    'ProductQuantizer',
    at_commit := 'a3f91b2'
);

-- timestamp-pinned
SELECT * FROM lsp_references(
    symbol_id := 84213,
    at_timestamp := TIMESTAMPTZ '2025-11-01 00:00:00+00'
);
```

Equivalent sugar (preferred for readability):

```sql
SELECT * FROM lsp_definition('ProductQuantizer')
    AS OF COMMIT 'a3f91b2';

SELECT * FROM lsp_references(84213)
    AS OF TIMESTAMP '2025-11-01';
```

Both forms delegate to the same planner path; the sugar is rewritten
into the explicit argument by `src/sql/planner.rs`.

Semantics:

- When `at_commit` / `at_timestamp` is set, **all** underlying
  `_hdb_code.*` scans are time-travel scans at that point. This
  includes the graph edges (the `CALLS` / `REFERENCES` relation is
  likewise time-travelled, since edges live in MVCC rows).
- Symbol IDs are *stable across time*: the `_hdb_code.symbols` row
  for `fn foo` keeps its `node_id` across edits; only its body, line
  ranges, and edges change. `lsp_hover(symbol_id) AS OF COMMIT 'X'`
  is therefore well-defined.
- If a symbol did not exist at the requested point (e.g. the function
  was added later), `lsp_definition` returns zero rows rather than an
  error. `lsp_hover` returns NULLs. Callers (agents) handle this.

### 3.2 Branch-scoped `lsp_*`

Session-level branch selection already exists. When a session is on
branch `B`, the `lsp_*` functions return results as observed on `B`:

```sql
SET branch = 'feature/new-auth';

SELECT * FROM lsp_references(84213);   -- as seen on feature/new-auth
```

Per-call branch override, for the cross-branch case:

```sql
SELECT * FROM lsp_references(84213) ON BRANCH 'main';
```

This composes with `AS OF`:

```sql
SELECT * FROM lsp_definition('UserRepository::save')
    ON BRANCH 'main'
    AS OF COMMIT 'v3.12.0';
```

### 3.3 Diff helpers

Three helpers, each implemented as a SQL function that calls the
underlying `lsp_*` twice and returns a diff.

```sql
-- Which references changed between two points?
lsp_references_diff(
    symbol_id   BIGINT,
    at_a        JSONB,           -- {"commit": "..."} or {"timestamp": "..."} or {"branch": "..."}
    at_b        JSONB
) RETURNS TABLE (change TEXT, path TEXT, line INT, caller_symbol_id BIGINT);
-- change ∈ {'added','removed','moved'}

-- What did this symbol look like at A vs B (body diff)?
lsp_body_diff(symbol_id BIGINT, at_a JSONB, at_b JSONB)
    RETURNS TABLE (line_a INT, line_b INT, op TEXT, text TEXT);

-- File-level AST diff (kind + identity, not character-level).
ast_diff(file_path TEXT, at_a JSONB, at_b JSONB)
    RETURNS TABLE (change TEXT, kind TEXT, qualified TEXT, line_a INT, line_b INT);
```

Each is ~30 lines of SQL over two temporal scans. They exist because
the two-point pattern is common enough that every caller would
otherwise reinvent it.

### 3.4 Interaction with branches: cheap parallel indexes

HeliosDB branches are CoW. When a branch `B` diverges from `main`,
only the pages that actually change are duplicated. The AST index
(FR 2) inherits this: indexing `feature/new-auth` after already
indexing `main` costs proportional to the *changed* files' AST
work, not the whole repo. No other code-graph tool offers this — they
all rebuild per branch from scratch.

This is not additional engineering in *this* FR — it is a property
that falls out of FR 2 + existing branching — but is called out
because it is a product-differentiating fact that should be
tested for and documented.

### 3.5 Lineage: connecting git SHAs to engine commits

`src/git_integration/commit_tracker.rs` already tracks a mapping
between host-git commits and HeliosDB internal commits. The FR
requires that `AS OF COMMIT 'a3f91b2'` accepts either:

- a short/full **git** SHA — resolved via `commit_tracker`, or
- a HeliosDB-internal commit id — passed through.

Ambiguity: if `a3f91b2` matches both, prefer the git SHA (which is
what agents have on hand) and warn if the engine-internal differs.

## Worked examples

**"How did the vector quantisation constructor evolve?"**

```sql
WITH points(label, at) AS (
    VALUES ('v3.10', '{"commit":"v3.10.0"}'::jsonb),
           ('v3.12', '{"commit":"v3.12.0"}'::jsonb),
           ('HEAD',  '{"commit":"HEAD"}'::jsonb)
)
SELECT p.label, d.path, d.line, d.signature
FROM   points p
       CROSS JOIN LATERAL lsp_definition(
           'ProductQuantizer',
           at_commit := p.at->>'commit'
       ) d;
```

**"On feature/new-auth vs main, which callers of `authenticate` appeared?"**

```sql
SELECT *
FROM lsp_references_diff(
    symbol_id := (SELECT symbol_id FROM lsp_definition('authenticate') LIMIT 1),
    at_a      := '{"branch":"main"}'::jsonb,
    at_b      := '{"branch":"feature/new-auth"}'::jsonb
)
WHERE change = 'added';
```

## Acceptance criteria

- [ ] `SELECT * FROM lsp_definition('X') AS OF COMMIT '<git sha>'`
      parses and returns results scoped to that commit.
- [ ] The same with `AS OF TIMESTAMP '...'` works.
- [ ] `SET branch = 'feature/x'; SELECT * FROM lsp_references(N)` is
      scoped to the selected branch.
- [ ] `... ON BRANCH 'main'` overrides session branch for one call.
- [ ] `lsp_references_diff`, `lsp_body_diff`, `ast_diff` return
      correct add/remove/move classifications on a crafted fixture
      (e.g. rename `foo` → `bar` between two commits on a small
      test repo).
- [ ] Git SHA → engine-commit resolution round-trips through
      `src/git_integration/commit_tracker.rs`.
- [ ] Branch-divergent indexing measured: cost of indexing a branch
      that touches 1 % of files is within 2× of that 1 %, not the
      whole repo.

## Non-goals

- Time-travelling across *schema* changes to the AST index itself
  (e.g. column additions to `_hdb_code.symbols`). DDL history is
  `src/git_integration/ddl_versioning/`'s concern, not this FR's.
- Interactive bisect UX. Agents can build it on top.

## Open questions

1. Should `lsp_rename_preview` accept `AS OF`? Arguable yes (preview
   what a rename would have done historically) but of limited value.
   Recommendation: no for v1.
2. Cost of graph traversal under MVCC — worth benchmarking on the
   seed corpus at SHAs spread across 6 months.
3. When `at_commit` is provided but the `_hdb_code` index was created
   *later* than that commit, do we refuse, or lazily build the
   historical index from the committed source? Recommendation:
   refuse with a clear error, and provide an
   `hdb_code.backfill(index_name, since := commit)` admin function.

## Related

- Depends on FR 2 (`ast_index_and_lsp`).
- Blocked until FR 2 lands.
- Unblocks, but is not required by, FR 4 (`graphrag_with_context`).
- FR 5 (`native_mcp_endpoint`) wraps these as additional MCP tool
  variants (`lsp_definition_at_commit`, etc.) or accepts the `at`
  param in the base tools.
