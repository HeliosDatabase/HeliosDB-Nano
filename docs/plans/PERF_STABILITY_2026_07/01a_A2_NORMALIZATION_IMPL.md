# A2 implementation appendix — token-level literal normalization

The risky heart of M2b. This spec exists so the normalizer is built **correctness-first
with a differential oracle**, not hand-verified. Wrong normalization = wrong query
results, so every design choice below favors "bail to the raw path" over cleverness.

## Why the prior AST auto-param failed, and why this differs
The reverted attempt rewrote the **AST after a full sqlparser parse** — it paid the parse
cost it was trying to save (net-neutral). This pass is a **byte lexer** (~100-300 ns) that
runs *before* parse; on a cache hit it skips parse + plan + optimize + both cache puts
entirely.

## The typing oracle (non-negotiable)
A normalized param MUST become the **byte-identical `Value`** the inline literal would.
The ground truth is `Planner::sql_value_to_value` (`src/sql/planner.rs:3868`):
- `Number(n)` → `Int4` if `n.parse::<i32>()` ok, else `Int8`, else `Float8`, else `Float4`.
- `SingleQuotedString(s)` → `String(repair_sqlparser_string(s))` (doubled `''` → `'`).
- `E'…'`/`U&'…'` → `String` verbatim (sqlparser already de-escaped).
- Dollar-quoted → `String`. `Boolean` → `Boolean`. `Null` → `Null`.

**Design rule:** the normalizer does NOT invent typing. For each literal span it captures
the exact source substring and produces the `Value` by feeding that substring through the
SAME path the planner uses. Extract a `pub(crate) fn literal_str_to_value(&str) -> Option<Value>`
from `sql_value_to_value` (share one body) so typing matches *by construction*. Any
literal the shared fn can't type → **bail**.

## v1 scope (deliberately narrow — widen only behind the oracle)
Fire ONLY when:
- Statement, trimmed, starts case-insensitively with `SELECT ` (not `SELECT` inside a CTE
  yet — bail on leading `WITH`).
- Exactly one statement (no top-level `;` except a single trailing one).
- Normalize literals **only in predicate position** — after the top-level `WHERE` and
  before any top-level `GROUP`/`ORDER`/`LIMIT`/`OFFSET`/`HAVING`/`WINDOW`. Rationale: a
  literal in the SELECT list (`SELECT 1 AS x`) or LIMIT can change the **output shape or
  row count**; a predicate literal cannot. This sidesteps the `SELECT 1 AS x` display
  hazard that helped kill the AST attempt.

Bail (return `None` → raw path, cache by raw key via A1 admission) on ANY of:
- `--` or `/* */` comment bytes anywhere (they can hide structure).
- Dollar-quote `$tag$…$tag$` or `$$…$$` (delimiter scanning is error-prone; rare in
  predicates).
- `E'…'` / `U&'…'` / `X'…'` / `B'…'` prefixed literals (let the raw path handle; typing
  subtleties).
- A `$N` placeholder already present (already parameterized — don't double up).
- `::` cast **immediately on a literal** in the predicate (`'…'::uuid`) UNLESS the whole
  `literal::type` span is treated as one opaque param whose Value comes from the shared
  typer AFTER the cast — v1: bail (the index path already fast-casts `$1::uuid`; keep v1
  simple).
- `IN (…)` / `VALUES` / `ARRAY[…]` / `BETWEEN` list literals in v1 (variable arity → a
  different normalized shape per list length defeats the cache; handle in v2 by
  normalizing the whole list to one array param).
- Nested `SELECT` (subquery) after WHERE — bail (scope tracking beyond v1).
- Any byte that isn't cleanly one of: whitespace, identifier char, operator, `(`/`)`,
  a number literal, a `'…'` string literal, `NULL`/`TRUE`/`FALSE` keyword.

## The lexer (predicate span only)
State machine over `sql.as_bytes()` from the first top-level `WHERE`:
- Track paren depth; stop normalizing at depth 0 when hitting a top-level clause keyword.
- **String literal**: opening `'`, consume to the closing `'`, handling doubled `''` as an
  escaped quote (stay in-string). Capture the raw span incl. quotes. Emit `$N`, push the
  substring for later `literal_str_to_value`.
- **Number literal**: `[+-]?` (only when preceded by an operator/`(`/`,`, NOT when it could
  be a column suffix), digits, optional single `.`, optional `eE[+-]?digits`. Capture span,
  emit `$N`. Reject weird forms (two dots, trailing `.`) → bail.
- **Keyword `NULL`/`TRUE`/`FALSE`** (word-boundary): treat as a literal param (matches
  `sql_value_to_value`). Optional in v1 — can leave inline (they're already stable cache
  keys). Prefer leaving `NULL`/`TRUE`/`FALSE` INLINE (they don't vary per call) → simpler.
- Everything else: copy through verbatim into the normalized buffer.

Output: `Some((normalized_sql, params: Vec<Value>))` or `None`.
Placeholders numbered `$1..$n` in source order.

## Integration (after A1 merges)
Hook in `query_with_columns` (`src/lib.rs`, after the fast-select attempt, before the
existing `try_cached_query_with_columns`) and its `query()` / `query_params` twins:
1. `let normalized = if params_empty { normalize_select_literals(sql) } else { None };`
   (never normalize a query that already carries wire params.)
2. On `Some((nsql, nparams))`: look up `plan_cache` by `nsql`. On hit → execute via
   `Executor::with_parameters(nparams)` on the cached plan (the executor already resolves
   `LogicalExpr::Parameter`, `scan.rs:800`). On miss → `parse_cached(nsql)` once, plan with
   Parameters, admit to plan_cache under `nsql` (A1 admission still applies — a
   normalized key that recurs admits on 2nd sighting), execute with params.
3. Do NOT insert into the **result** cache on the normalized path (params vary → the row
   set varies; only the *plan* is reused). Result cache stays keyed on raw SQL for the
   genuinely-identical-repeat case.
4. Kill switch: `NANO_DISABLE_QUERY_NORMALIZATION=1` env + a config flag, checked once and
   cached in a field. Ship defaulted ON but with the switch documented.

## The differential oracle (the actual deliverable that makes this safe)
A test harness `tests/normalization_differential.rs` that, for a large query corpus,
asserts **`execute(raw_sql)` == `execute(normalize→parameterize→execute)`** row-for-row
AND asserts the normalized form actually hit the parameterized path (via a counter or an
EXPLAIN check). Corpus MUST include: integer/negative/float/scientific/decimal predicates;
string literals with embedded `''`, unicode, empty; `WHERE a = 1 AND b = 'x' OR c > 3.5`;
parenthesized predicates; `col = 5` vs `5 = col`; NULL/TRUE/FALSE; every bail case
(assert they take the raw path and still return correct rows); `LIMIT`/`OFFSET` literals
(assert NOT normalized); a literal in the SELECT list (assert NOT normalized). Also fuzz:
random SQL-ish byte strings must never panic and must either normalize-correctly or bail.

Run the oracle against the same corpus with the kill switch ON and OFF — results identical.

## Gate (M2b)
Standard campaign gate PLUS: the differential oracle (0 mismatches), pg35 no erosion,
psycopg protocol suite (normalization is on the simple-query path for every client), and
the bench indexed-read sweep hitting the group-A target (c=1 ≥+50% & >PG; c=32 ≥+75% &
>PG). Kill-switch A/B parity test.
