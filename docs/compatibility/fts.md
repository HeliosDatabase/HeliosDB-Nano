# PostgreSQL full-text search compatibility

HeliosDB Nano v3.13.0 ships Postgres-compatible full-text search at the
SQL surface: the `tsvector` and `tsquery` column types, the `@@` match
operator, and the `to_tsvector` / `to_tsquery` / `ts_rank` /
`ts_rank_cd` scalar functions. All are backed by the same BM25 engine
(`src/search/bm25.rs`) that powers hybrid search.

This document is the source of truth for **exactly** what works and
what doesn't, so adapters and ORMs know what to rely on.

---

## Supported

### Types

- **`TSVECTOR`**: column type, stored as a JSON array of normalised
  tokens (`["hello", "world"]`). Writes accept either a text literal
  or the result of `to_tsvector(...)`.
- **`TSQUERY`**: same encoding as `TSVECTOR`.

### Scalar functions

| Function | Signature | Notes |
|---|---|---|
| `to_tsvector(text)` | `text → tsvector` | Single-arg form. |
| `to_tsvector(config, text)` | `text, text → tsvector` | The `config` (e.g. `'english'`) is accepted for compatibility and **ignored** — we use one Unicode-word tokenizer regardless. |
| `to_tsquery(text)` | `text → tsquery` | Boolean operators (`&`, `|`, `!`, `<->`) in the input are treated as term separators — see "Not supported" below. |
| `plainto_tsquery(text)` | `text → tsquery` | Alias — same behaviour as `to_tsquery` in Nano. |
| `phraseto_tsquery(text)` | `text → tsquery` | Alias — we do not do phrase matching; accepted for compatibility. |
| `ts_rank(doc, query)` | `tsvector, tsquery → float8` | BM25 score against a 1-doc ephemeral index. |
| `ts_rank_cd(doc, query)` | `tsvector, tsquery → float8` | Alias — same semantics as `ts_rank` in Nano. The `_cd` (cover density) distinction requires position information, which our tsvector doesn't carry. |
| `ts_rank(weights, doc, query[, norm])` | extra args accepted | Weight array and normalisation flag accepted for signature compatibility and **ignored**. |

### Operators

- **`@@`** (`tsvector @@ tsquery`): returns `true` iff **any** query
  term appears in the document's token set. Three-valued logic:
  `NULL @@ _` and `_ @@ NULL` yield `NULL`.

### DDL

- `CREATE INDEX name ON table USING gin (col)` — accepted.
- `CREATE INDEX name ON table USING gist (col)` — accepted.

Both DDL forms are preserved in the WAL and echoed back through
introspection. See the "Known limitation" note under the DDL section
below for what they do at runtime.

---

## Not supported

These are the cases where migrating from PostgreSQL needs an
accommodation.

### Language-specific stemmers

Our tokenizer **normalises** (lower-case, Unicode word boundaries) but
does **not stem**. So `to_tsvector('foxes')` yields `["foxes"]`, not
`["fox"]`. If stemming matters, do it at ingest time:

```python
def to_tsvector_stemmed(text: str) -> str:
    tokens = [stemmer.stem(t) for t in tokenize(text)]
    return json.dumps(tokens)
```

and then insert directly into a `TSVECTOR` column.

### Phrase queries and proximity operators

`to_tsquery('quick <-> fox')` parses but yields a bag of terms —
proximity / phrase information is discarded. If you need phrase
matching, post-filter in application code.

### Positional weights / `setweight()`

Neither `setweight(tsvector, 'A')` nor the weight array form of
`ts_rank` produces weighted output. Weights pass through the API
unchanged and are ignored.

### Persistent GIN / GiST inverted index

`CREATE INDEX ... USING gin` is accepted as DDL for compatibility with
Django migrations, SQLAlchemy's `postgresql.GIN`, and hand-written
`ALTER TABLE ... ADD INDEX` scripts. At runtime, the index is **not**
consulted — the `@@` operator walks matching rows and evaluates the
match in the evaluator.

In practice:
- Up to ~100k rows of moderate-length text: fine.
- Beyond that: prefilter with another predicate (`tenant_id`, a vector
  proximity cut, a time range) before `@@` so the walk stays bounded.
- The BM25 engine itself can handle millions of documents — the gap
  is in wiring a persistent inverted index into the storage layer,
  which is tracked as a follow-up.

### Multi-column `tsvector`

`to_tsvector('english', col_a || ' ' || col_b)` works (produces the
combined vector on the fly), but there is no `setweight(to_tsvector(a),
'A') || setweight(to_tsvector(b), 'B')` path since we don't support
weights.

---

## Usage examples

### Basic match

```sql
SELECT id, title
FROM articles
WHERE to_tsvector(body) @@ to_tsquery('heliosdb');
```

### Ranked search

```sql
SELECT id, title,
       ts_rank_cd(to_tsvector(body), to_tsquery('heliosdb')) AS rank
FROM articles
WHERE to_tsvector(body) @@ to_tsquery('heliosdb')
ORDER BY rank DESC
LIMIT 10;
```

### With a persistent tsvector column

```sql
CREATE TABLE articles (
    id    SERIAL PRIMARY KEY,
    body  TEXT,
    body_tsv TSVECTOR
);

CREATE INDEX articles_body_fts ON articles USING gin (body_tsv);

INSERT INTO articles (body, body_tsv)
VALUES ('hello heliosdb', to_tsvector('hello heliosdb'));

SELECT id, ts_rank_cd(body_tsv, to_tsquery('heliosdb')) AS rank
FROM articles
WHERE body_tsv @@ to_tsquery('heliosdb')
ORDER BY rank DESC;
```

### Hybrid search (FTS + vector)

Compose FTS with vector distance in a single query:

```sql
SELECT id, text,
       1.0 - (embedding <=> $1::vector) AS vec_score,
       ts_rank_cd(to_tsvector(text), plainto_tsquery($2)) AS bm25_score
FROM chunks
WHERE tenant_id = $3
  AND (embedding <=> $1::vector) < 0.8
ORDER BY 0.7 * (1.0 - (embedding <=> $1::vector))
       + 0.3 * ts_rank_cd(to_tsvector(text), plainto_tsquery($2))
       DESC
LIMIT 10;
```

---

## Implementation references

- Scalar functions: `src/sql/evaluator.rs` (search for `fts_`).
- `@@` operator: `BinaryOperator::TsMatch` in
  `src/sql/logical_plan.rs`; planner mapping in `src/sql/planner.rs`
  (look for `SqlBinaryOp::AtAt`); evaluation in
  `src/sql/evaluator.rs::evaluate_ts_match`.
- `TSVECTOR` / `TSQUERY` type: `src/sql/planner.rs` (look for
  `"TSVECTOR"`).
- `USING gin` DDL: `src/sql/executor/ddl.rs` (look for `idx_type ==
  "gin"`).
- Tests: `tests/fts_tests.rs` — 8 regression cases.

---

*Added in v3.13.0 (2026-04-19).*
