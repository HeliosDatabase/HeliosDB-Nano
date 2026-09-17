# PostgreSQL full-text search compatibility

HeliosDB Nano ships Postgres-compatible full-text search at the
SQL surface: the `tsvector` and `tsquery` column types, the `@@` match
operator, and the `to_tsvector` / `to_tsquery` / `ts_rank` /
`ts_rank_cd` scalar functions. All are backed by the same BM25 engine
(`src/search/bm25.rs`) that powers hybrid search.

This document is the source of truth for **exactly** what works and
what doesn't, so adapters and ORMs know what to rely on.

---

## Supported

### Types

Since **4.35** `TSVECTOR` and `TSQUERY` are distinct declared types
(they used to be declared as `JSON`, which made the assignment
coercion validate the input as JSON — so the plain-text write below
failed with `Invalid JSON string`).

- **`TSVECTOR`**: column type, **stored** as a JSON array of
  normalised tokens (`["hello", "world"]`) — the storage format is
  unchanged, so existing data and every `@@` / `ts_rank` path are
  unaffected.
- **`TSQUERY`**: same storage as `TSVECTOR`.

**What a write accepts.** Three spellings, on `INSERT`, `UPDATE`, a
bound parameter and an explicit `::tsvector` / `::tsquery` cast alike:

| Input | Result |
|---|---|
| `to_tsvector(...)` / `to_tsquery(...)` (or any JSON token array) | kept verbatim — a hand-stemmed vector is **not** re-tokenised |
| plain text — `'hello world'` | tokenised with the same tokenizer `to_tsvector` uses |
| the quoted-lexeme form — `'hello' 'world'`, with optional `:1,2` / `:3A` suffixes | each lexeme taken **verbatim** (case preserved, `''` un-escaped to `'`); positions and weights are parsed and dropped |

```sql
CREATE TABLE documents (search_vector tsvector);
INSERT INTO documents VALUES ('hello world foo');       -- plain text: OK
INSERT INTO documents VALUES ('''Hello'' ''World''');   -- lexemes: Hello, World
SELECT 'hello world'::tsvector @@ to_tsquery('hello');  -- true
```

> **Divergence.** PostgreSQL's `::tsvector` input does **not** normalise
> — `'Fox'::tsvector` keeps `Fox`, and `@@ to_tsquery('fox')` then finds
> nothing. Nano lower-cases and tokenises plain text so that the
> implicit text→tsvector assignment keeps matching `to_tsquery`, which
> is the reason applications rely on it. Use the quoted-lexeme form when
> you need the exact lexemes preserved.

**On the wire.** A `tsvector` column is advertised as PostgreSQL's real
OID **3614** and a `tsquery` column as **3615** — both are builtin OIDs in
every PostgreSQL driver, so a client resolves them without asking the
server. Their values are **printed** in PostgreSQL's quoted-lexeme form
(`'hello' 'world'`), not as the JSON array, in **`SELECT` rows and in
`COPY … TO STDOUT` alike** — so `COPY … TO STDOUT` output re-imports
through `COPY … FROM STDIN` with the identical token set.

A bound parameter of either type is accepted in **text or binary**
format; the binary payload is read as UTF-8 text, because PostgreSQL's
binary `tsvector` layout (lexeme count, NUL-terminated lexemes, position
arrays) carries positions that Nano does not store and is not
implemented.

**Other surfaces.** The embedded API, REST, MCP and dumps still **read**
the JSON token array they always read. REST **writes** take the same
three input shapes the SQL path takes, spelled in JSON: a string (plain
text, or the quoted-lexeme form), an array of strings (the lexemes,
verbatim), or `null`. Anything else — a number, an object, an array with
a non-string element — is refused rather than silently coerced.

**Migration.** A database written before 4.35 declared its tsvector
columns as `JSON`, and they keep that declared type on reopen (the
declared type is persisted, not re-derived). Recreate the column to pick
up the new input rule:

```sql
ALTER TABLE documents ADD COLUMN search_vector_new tsvector;
UPDATE documents SET search_vector_new = search_vector;
ALTER TABLE documents DROP COLUMN search_vector;
ALTER TABLE documents RENAME COLUMN search_vector_new TO search_vector;
```

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

> **`vector`'s OID changed in 4.35.** Giving `tsvector` PostgreSQL's real
> OID (3614) required taking it back from Nano's `vector` type, which had
> been registered there in `pg_type` while the PG wire advertised `1000`
> (PostgreSQL's `_bool`). `vector` now answers two different, deliberate
> numbers:
>
> * **Name lookup → `16385`.** `vector` is registered in `pg_type` under
>   its own private OID in the first user band, where an extension type
>   belongs and where pgvector itself typically lands; its array type
>   `_vector` is **16386**, and `pg_type.typnamespace` for both is
>   `public` (2200), not `pg_catalog`. Resolve the type by NAME —
>   `SELECT oid FROM pg_type WHERE typname = 'vector'` — as you would
>   against a real pgvector install, rather than hard-coding the number.
> * **`RowDescription` → `text` (`25`).** A vector column is **advertised
>   on the wire as text**, which is exactly what its value on the wire is
>   (`[0.1,0.2]`, pgvector's text form). It is not advertised as 16385:
>   a result-column OID that a driver does not know natively sends
>   tokio-postgres — and therefore sqlx and Prisma's query engine — into
>   a server-side `TYPEINFO` lookup whose `$1` Nano describes as OID `0`,
>   which the driver cannot resolve either, so it re-prepares the lookup
>   without bound.
>
> The practical consequence: a pgvector client that registers the type by
> NAME will never meet 16385 on the wire, and decodes the text form. If
> your driver needs a typed `vector` codec, register it against `text`.

---

## Implementation references

- Scalar functions: `src/sql/evaluator.rs` (search for `fts_`).
- `@@` operator: `BinaryOperator::TsMatch` in
  `src/sql/logical_plan.rs`; planner mapping in `src/sql/planner.rs`
  (look for `SqlBinaryOp::AtAt`); evaluation in
  `src/sql/evaluator.rs::evaluate_ts_match`.
- `TSVECTOR` / `TSQUERY` type: `DataType::TsVector` / `DataType::TsQuery`
  in `src/types.rs`; declared-type parsing in `src/sql/planner.rs` (look
  for `"TSVECTOR"`); the input rule in
  `src/sql/evaluator.rs::fts_input_tokens` and the `cast_value` arm next
  to it; wire OIDs in `src/protocol/postgres/handler.rs::datatype_to_oid`
  and `BUILTIN_TYPES` in `src/sql/phase3/system_views.rs`.
- Write vs read: the quoted-lexeme spelling is an **input** form, parsed
  once on the write side (`fts_input_tokens`, reached through
  `cast_value`). The read side (`Evaluator::fts_decode_tokens`) plainly
  tokenises a `Value::String`, because it is also reached for ordinary
  `text` columns used with `@@`, whose behaviour must not change.
- Bound parameters: `src/protocol/postgres/prepared.rs::decode_parameter`
  (OIDs 3614 / 3615, text and binary).
- REST writes: `src/api/models/data.rs::json_to_value`.
- `COPY … TO STDOUT` rendering:
  `src/protocol/postgres/handler.rs::handle_copy_to_stdout`.
- `USING gin` DDL: `src/sql/executor/ddl.rs` (look for `idx_type ==
  "gin"`).
- Tests: `tests/fts_tests.rs` — 8 regression cases;
  `tests/security_hdb_002.rs` — the text-input and OID cases.
