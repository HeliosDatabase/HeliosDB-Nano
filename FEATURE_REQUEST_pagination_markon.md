---
requested-by: Markon (foor.network/markon) — danimoya
requested-against: HeliosDB-Nano v3.11.0 (currently pinned in markon-heliosdb)
upstream-current: v3.19.1
priority: high
status: open
date-filed: 2026-04-17
last-updated: 2026-05-14
---

# Feature Request: Pagination for the SQLAlchemy + psycopg path (markon)

## TL;DR

Markon (and any SQLAlchemy-backed app on the `heliosdb+psycopg://` dialect)
needs reliable `LIMIT/OFFSET` and ideally keyset pagination through the
SQLAlchemy + psycopg layer. After cross-checking the public docs at
<https://www.heliosdb.com/docs/nano/guides/quickstart/keyset_pagination_quickref/#uvp>,
most of what we need is **already shipping upstream** — but Markon is pinned
at v3.11.0 (below the v3.12.0 row-constructor cutoff), and the docs do not
cover two specific gaps that bit us. This issue captures both halves.

## What the public docs already confirm

From the keyset pagination quickref (verified 2026-05-14):

- `LIMIT N OFFSET M` is supported. Quote: *"`LIMIT 20 OFFSET 100000` is a
  valid syntax, though with performance caveats on deep pages."*
- Keyset pagination via row-constructor tuples is supported:
  `WHERE (created_at, id) < ($1, $2) ORDER BY created_at DESC, id DESC LIMIT 20`
- PostgreSQL-style positional bind parameters work (psycopg should be fine
  by extension).
- `ORDER BY` + `LIMIT/OFFSET` combine in both traditional and keyset forms.
- Pitfalls documented: sort key must be unique (always tail with `id`);
  avoid floating-point sort keys; do not mix ASC/DESC inside the tuple.

Versions per docs:

- v3.6.0+ — base keyset paths
- v3.12.0+ — row-constructor tuple syntax + Top-K optimization
- v3.19.1 — current upstream

## What is NOT covered by the docs (the actual gap)

### Gap 1 — Markon is on v3.11.0; row-constructor needs v3.12.0+

The Markon stack pins `markon-heliosdb` (see `docker-compose.yml`) at the
3.11.0 line. The keyset row-constructor syntax we want lives in v3.12.0+,
and the rest of the executor / Top-K work lives further along. Action item
on the Markon side: bump the image. **But the upgrade path / supported
upgrade target / breaking changes between 3.11.0 → 3.12.x → 3.19.1 are
not documented in the quickref and would unblock us.** Specifically:

- Is v3.11.0 → v3.19.1 a drop-in upgrade for the on-disk RocksDB layout?
- Or is a step-wise migration required (3.11 → 3.12 → … → 3.19)?
- Are there schema changes / re-index steps?

### Gap 2 — `LEFT OUTER JOIN` + `LIMIT/OFFSET` (or keyset) composition

The docs do not cover JOIN composition with paginated queries. Markon's
heaviest list endpoints (`/api/v1/leads`, `/api/v1/conversations`,
`/api/v1/dashboard/search`) all do `Lead JOIN Company` (or similar) and
then page. Today they read the full result set and slice in Python:

```python
# backend/app/api/leads.py — current workaround
result = await db.execute(select(Lead, Company.name).outerjoin(Company))
all_rows = result.all()                         # full scan
paginated = all_rows[offset:offset + limit]     # Python-side slice
```

What we want to write:

```python
result = await db.execute(
    select(Lead, Company.name)
    .outerjoin(Company)
    .order_by(Lead.created_at.desc(), Lead.id)
    .limit(limit)
    .offset(offset)
)
```

Please confirm (or document) that this composes cleanly via the
`heliosdb+psycopg` dialect at v3.12.0+ / v3.19.1, including:

- `LIMIT/OFFSET` with `LEFT OUTER JOIN` — does pushdown still work, or
  does it materialise the join first then apply LIMIT?
- Keyset `WHERE (col, id) < ($1, $2)` over a join — what happens if the
  ORDER BY columns live on the left table only?

### Gap 3 — `_row_as_tuple_getter NotImplementedError` on empty result sets

This was the bug that originally forced Markon to wrap every paginated
query in `try/except` and slice in Python. It surfaces when a SQLAlchemy
ORM-model select hits an empty table:

```python
# backend/app/api/dashboard.py (search) had to switch to column-level selects
# and try/except wrap each entity:
try:
    lead_result = await db.execute(
        select(Lead.id, Lead.first_name, Lead.last_name, Lead.email, Lead.stage)
        .where(...)
        .limit(limit)
    )
    for row in lead_result.all():
        ...
except Exception:
    pass
```

`select(Lead)` against an empty table raises
`NotImplementedError: _row_as_tuple_getter`. We side-stepped it via
column-level selects + try/except. Confirm whether this is fixed in the
v3.12 → v3.19 line, or file as a separate bug if not.

## Acceptance criteria for closing this on the Markon side

We can drop our Python-side pagination workarounds when **all** of these
are true on the upgraded image:

- [ ] `SELECT * FROM leads ORDER BY created_at DESC, id DESC LIMIT 10
      OFFSET 20` succeeds via psycopg with `$1`/`$2` bind parameters.
- [ ] Same with `LEFT OUTER JOIN companies ON leads.company_id = companies.id`.
- [ ] Keyset variant: `WHERE (created_at, id) < ($1, $2) ORDER BY
      created_at DESC, id DESC LIMIT $3` with the join.
- [ ] `select(Lead)` against an empty `leads` table returns 0 rows
      cleanly (no `_row_as_tuple_getter NotImplementedError`).
- [ ] `COUNT(*)` works (separate but related — needed to drop the
      `len(all_rows)` fallback for the `/leads/count` endpoint).

## Markon-side cleanup unlocked once the above lands

Once the upgrade is confirmed safe and the gaps above are closed:

- Drop the `try/except` wrappers + Python-side slicing in:
  - `backend/app/api/dashboard.py` (`global_search`)
  - `backend/app/api/leads.py`
  - `backend/app/api/tools.py`
  - `backend/app/api/campaigns.py`
  - `backend/app/api/conversations.py`
- Replace Python pagination with SQL `LIMIT/OFFSET` (then with keyset
  for high-volume lists).
- Add a real `count_*` endpoint without falling back to `len(all_rows)`.

## Suggested resolution

1. **Upstream side (HeliosDB):** publish an upgrade-path doc covering
   3.11.0 → 3.19.1 (storage compat, step-wise vs drop-in). Add a JOIN +
   pagination example to the keyset quickref. Confirm or fix the
   `_row_as_tuple_getter` empty-table case.
2. **Markon side (foor.network/markon):** bump `markon-heliosdb` image to
   the recommended target (likely v3.19.1) and re-enable SQL pagination.

## Contact

Open follow-ups against this file or ping Markon repo at
`foor.network/markon` (`danimoya`).
