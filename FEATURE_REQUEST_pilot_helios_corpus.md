---
requested-by: Claude Code code-graph initiative — danimoya
requested-against: HeliosDB-Nano v3.13.x
priority: medium
status: proposed
date-filed: 2026-04-23
track: code-graph
doc: pilot
depends-on: FEATURE_REQUEST_ast_index_and_lsp.md (sufficient for phase 1)
---

# Feature Request: `~/Helios` pilot corpus — end-to-end validation

## TL;DR

Stand up the whole code-graph track against the real `~/Helios`
corpus on `danielmoya.cv` (host `51.77.68.69`). Use the pilot to
validate scale (≈ 10 git repos, hundreds of docs, investor material),
ergonomics (`.helios-index/` per repo + a parent-level aggregate),
and the agent-facing workflow (Claude Code answers 200 prepared
investor questions with ≤ 3 tool calls each, no manual reads).

This is the forcing-function document for the whole track. It is not
engine engineering — it is deployment, schema choices, and the
per-repo layout that downstream teams will copy.

## Pilot corpus inventory

From `~/Helios/` on the pilot host:

| Path | Kind | Notes |
|---|---|---|
| `Nano/` | git repo (this repo) | Rust, the engine itself |
| `Lite/` | git repo | Cloud-scale edition |
| `Full/` | git repo | Enterprise edition |
| `Cloud/` | git repo | Hosted edition |
| `Proxy/` | git repo | Connection proxy |
| `SDKs/` | git repo | Go / Python / TS / Rust clients |
| `Website/` | git repo | Marketing site |
| `Docs-Public/` | git repo | Public docs |
| `Docs-Internal/` | git repo | Internal docs |
| `Documentation/` | git repo | Long-form documentation |
| `Investors/` | prepared material | ≈ 200 Q&A candidates |
| `HeliosDB-SeriesA-DataRoom-*.tar.gz` | archive | Historical snapshots |
| `packages/`, `test-app/` | scratch | Pilot-only; exclude from indexing |

## Per-repo layout (the "`.helios-index/`" convention)

Each git repo gets a hidden, git-ignored directory that holds its
HeliosDB-Nano instance for code-graph purposes. Laid out so Claude
Code (and any other MCP-capable agent) can discover and use it
without bespoke configuration per repo.

```
<repo>/
├── .helios-index/                    # git-ignored
│   ├── heliosdb-data/                # HeliosDB-Nano data directory
│   ├── config.toml                   # embed_model, chunk size, file filters
│   ├── manifest.json                 # path → (mtime, sha256, node_ids)
│   └── refresh.log
├── .gitignore                        # contains /.helios-index/
├── .githooks/
│   ├── post-commit                   # UPSERT changed files into src table
│   ├── post-merge                    # same
│   └── post-checkout                 # SET branch = <new branch>
├── .mcp.json                         # advertises the helios MCP server
├── .claude/
│   └── skills/
│       └── helios-index/SKILL.md     # how an agent should use the index
└── CLAUDE.md                         # already exists; add one paragraph
```

Wire-up (one-time per repo):

```bash
git config core.hooksPath .githooks
heliosdb-nano init --data-dir .helios-index/heliosdb-data
heliosdb-nano exec --data-dir .helios-index/heliosdb-data < scripts/bootstrap.sql
```

`scripts/bootstrap.sql` is small and identical across repos:

```sql
CREATE EXTENSION hdb_code;

CREATE TABLE src (
    path     TEXT PRIMARY KEY,
    lang     TEXT,
    content  TEXT,
    sha256   TEXT,
    branch   TEXT,
    mtime    TIMESTAMPTZ DEFAULT now()
);

CREATE AST INDEX src_ast
    ON src (content) USING tree_sitter(lang)
    WITH (resolve_cross_file = true,
          embed_bodies       = true,
          embed_model        = 'bge-small-en',
          auto_reparse       = true);

-- For doc-bearing repos, also:
-- CREATE EXTENSION hdb_corpus;
-- CREATE TABLE docs (...);
-- SELECT hdb_corpus.ingest_docs('docs', 'body');
```

The git hook body is a one-liner psql upsert — no bash gymnastics,
no refresh script, no cron. HeliosDB's CDC (FR 2 §2.5) does the
rest.

## Parent-level aggregate index

For cross-repo investor Q&A, one additional HeliosDB-Nano instance
at `~/Helios/.helios-index/` federates the ten per-repo indexes.
Two plausible approaches; the FR asks the engine team to pick:

**Option A — physical mirror.** Parent index subscribes to each
per-repo `src` / `docs` / `_hdb_graph.nodes` table via replication
(`src/replication/`) and maintains its own AST / graph indexes over
the union. Higher disk, simpler query. Recommended default.

**Option B — logical federation.** Parent index holds only
`_hdb_graph.nodes/edges` (dedup'd across repos) and runs cross-repo
queries by fan-out to each per-repo instance, merging results. Lower
disk, more moving parts. Useful for repos that are too large to
mirror.

Recommendation: ship A as the pilot, instrument to see when B would
be needed.

## Ingesting non-repo material

Investor material lives outside any single repo:

- `~/Helios/Investors/questions/*.md` → `hdb_corpus.ingest_qa(...)`
  produces `InvestorQuestion` nodes with `extra.code` carrying the
  question ID.
- `~/Helios/HeliosDB-SeriesA-DataRoom-*.tar.gz` → extract once into
  `~/Helios/DataRoom-latest/`, then `hdb_corpus.ingest_docs(...)`.
- Email export (if available) → `hdb_corpus.ingest_email(...)`.

These feed the parent index only (per-repo indexes stay focused on
the repo's own source).

## Agent-facing surface

Three touch-points per repo, weighted low-to-high:

1. **`CLAUDE.md`** gains one paragraph:
   > *This repository has a HeliosDB-Nano code-graph at
   > `.helios-index/`. Prefer the `helios` MCP tools
   > (`helios_lsp_definition`, `helios_lsp_references`,
   > `helios_graphrag_search`) over `Grep` / `Read` for discovery.
   > The index is refreshed automatically on commit; run
   > `heliosdb-nano exec --data-dir .helios-index/heliosdb-data
   > -c 'SELECT hdb_code.rebuild();'` if it falls out of sync.*
2. **`.mcp.json`** (checked in) registers the local MCP server:
   ```json
   { "mcpServers": {
       "helios": { "url": "http://localhost:8080/mcp" } } }
   ```
3. **`.claude/skills/helios-index/SKILL.md`** (checked in) tells the
   agent *when* to use each tool, how to combine them with
   `Read`, and how to fall back to `Grep` if the index is
   unavailable.

## Success metric

200 prepared investor questions in `~/Helios/Investors/`. Pilot
succeeds if:

- ≥ 90% of questions are answerable with ≤ 3 MCP tool calls (no
  file reads).
- Median end-to-end latency per question ≤ 2 seconds.
- Incremental reindex after a typical commit ≤ 1 second.
- Cold-boot full `~/Helios` index build < 30 minutes on the pilot
  host with local embeddings.

These numbers define "the track worked" and should be reported
against each release in the track's changelog.

## Acceptance criteria

- [ ] `.helios-index/` layout documented and reproduced in each of
      the 10 `~/Helios` repos.
- [ ] `.githooks/post-commit` upserts changed files; AST index
      reparse completes within the same transaction.
- [ ] Parent-level index at `~/Helios/.helios-index/` federates
      per-repo data per Option A (or documented exception).
- [ ] `.claude/skills/helios-index/SKILL.md` is checked into each
      repo and renders correctly in Claude Code.
- [ ] Success-metric pass on 200-question pilot documented in
      `docs/PILOT_RESULTS.md`.

## Non-goals

- Packaging this layout as a public "getting started" template. Pilot
  first, template later.
- Moving away from local embeddings. Voyage / OpenAI embeddings are
  a later optimisation if quality is insufficient.

## Open questions

1. Where does the parent index run relative to per-repo ones — same
   host, separate port? Recommendation: same host, port 8090 to
   distinguish from per-repo :8080.
2. Branch strategy for the parent aggregate — follow `main` of each
   sub-repo, or maintain its own branch-set? Recommendation:
   follow `main` by default, with explicit `ON BRANCH` usage in
   queries when cross-branch diligence is needed.
3. Retention of time-travel history on the pilot — keep forever, or
   trim after N months? Recommendation: keep forever for the pilot;
   measure size, decide policy for GA.

## Related

- Consumes FR 2 (`ast_index_and_lsp`) — sufficient to bootstrap the
  pilot.
- Grows to consume FR 3, FR 4, FR 5 as they land.
- Produces the empirical data that justifies the track publicly.
