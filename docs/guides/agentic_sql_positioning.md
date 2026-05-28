# Agentic SQL Positioning

HeliosDB-Nano should be positioned as an Apache-2.0, wire-compatible
agentic SQL database: existing PostgreSQL/MySQL tools keep working, while
agent workflows can use vectors, FTS, code-graph, graph-RAG, branches, and
time-travel without adopting a new query language.

The SQL MVP for local inference and self-driving optimization is intentionally
conservative:

```sql
SELECT predict('sum', ARRAY[1.0, 2.0, 3.0]);
SELECT infer('mean', ARRAY[1.0, 2.0, 3.0]);
SELECT generate('summarize this row', 'default');
SELECT heliosdb_self_drive_plan(
  'SELECT * FROM events WHERE tenant_id = 42'
);
```

`predict` and `infer` provide deterministic built-in models for smoke tests and
integration plumbing. Production model execution should route through configured
AI providers, the REST chat/RAG APIs, or the optional local embedder. `generate`
returns a structured provider-required response unless an external provider path
is used.

`heliosdb_self_drive_plan` is preview-only: it returns index recommendations and
the safe loop Nano is designed for: create a branch, apply candidate changes
there, benchmark against main, and promote only after measured improvement.
It does not mutate production data automatically.

Multi-model behavior is exposed as access patterns over SQL tables rather than
a lock-in data model: JSONB for documents, edge tables and graph-RAG tables for
relationships, HNSW/BM25 for retrieval, and MCP/REST surfaces for agents.
