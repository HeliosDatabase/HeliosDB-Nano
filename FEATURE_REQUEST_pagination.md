---
requested-by: Markon (foor.network/markon) — danimoya
requested-against: HeliosDB-Nano v3.11.0
priority: high
status: open
date-filed: 2026-04-17
---

# Feature Request: First-class `LIMIT` / `OFFSET` pagination for the SQLAlchemy + psycopg path

## TL;DR

Markon (and any SQLAlchemy-backed app) needs `SELECT ... LIMIT N OFFSET M`
to round-trip correctly through the `heliosdb+psycopg://` dialect so that
list endpoints can paginate at the storage layer instead of in Python.

Today, all `list_*` endpoints in `backend/app/api/*.py` fetch the entire
table and slice it in Python (see `backend/app/api/leads.py`,
`tools.py`, `campaigns.py`, `conversations.py`). At ~10k+ rows this becomes
a real latency and memory problem. We have a TODO in `CLAUDE.md`:

> "HeliosDB doesn't support OFFSET/LIMIT reliably — manual Python pagination."

## What we need

1. `SELECT ... LIMIT $1 OFFSET $2` with bound parameters (psycopg sends
   them as positional `$1, $2`, not literals).
2. `SELECT ... ORDER BY <col> LIMIT $1 OFFSET $2` — sort-then-paginate, the
   common case for stable pagination.
3. Keyset / cursor pagination would be even better:
   `SELECT ... WHERE (created_at, id) < ($1, $2) ORDER BY ... LIMIT $3`
   — this avoids the offset-scan problem at scale and is what we'd
   ultimately migrate Markon to.
4. `LIMIT` / `OFFSET` must compose with `JOIN`, since several Markon
   queries do `Lead JOIN Company` then paginate.

## What we observed (3.11.0)

- `src/sql/planner.rs:818` parses `LIMIT` / `OFFSET` and produces
  `LogicalPlan::Limit { input, limit, offset }`.
- `src/sql/executor/mod.rs:661` implements the operator and even has a
  pushdown to `storage.scan_table_with_limit(table_name, limit + offset)`
  for `Scan` / `Project(Scan)` inputs.
- So the feature *partially* works in raw SQL, but the SQLAlchemy +
  psycopg path emits queries (often with parameter placeholders, ORDER BY,
  and JOIN) that either hit unimplemented branches or trigger
  `_row_as_tuple_getter NotImplementedError` on empty result sets — which
  is what forced us to wrap every paginated query in `try/except` and slice
  in Python.

## Repro from Markon

```python
# backend/app/api/leads.py — current workaround
result = await db.execute(select(Lead, Company.name).outerjoin(Company))
all_rows = result.all()                 # full scan
paginated = all_rows[offset:offset+limit]   # Python-side slice
```

What we want to write instead:

```python
result = await db.execute(
    select(Lead, Company.name)
    .outerjoin(Company)
    .order_by(Lead.created_at.desc(), Lead.id)
    .limit(limit)
    .offset(offset)
)
```

## Acceptance criteria

- [ ] `SELECT * FROM leads LIMIT 10 OFFSET 20` works via psycopg with
      `$1`/`$2` bind parameters.
- [ ] Same query with an `ORDER BY` works deterministically.
- [ ] Same query with a `LEFT OUTER JOIN companies ON ...` works.
- [ ] Empty-table case returns 0 rows cleanly (no
      `NotImplementedError: _row_as_tuple_getter`).
- [ ] (Stretch) Keyset pagination — `WHERE (col, id) < ($1, $2)` —
      planned and pushed down to the storage scan.

## Related Markon-side work that becomes possible once this lands

- Drop `_row_as_tuple_getter` workarounds and `try/except` wrappers in:
  - `backend/app/api/dashboard.py` (`global_search`)
  - `backend/app/api/leads.py`, `tools.py`, `campaigns.py`,
    `conversations.py`
- Replace Python-side pagination with SQL `LIMIT` / `OFFSET`.
- Add a real `count_*` endpoint without falling back to `len(all_rows)`
  (also needs `COUNT(*)` to land — separate request).

## Suggested release

HeliosDB-Nano **v3.12.0** (or v3.11.x if it's a small executor patch).

## Contact

Open follow-ups against this file, or ping Markon repo at
`foor.network/markon` (`danimoya`).
