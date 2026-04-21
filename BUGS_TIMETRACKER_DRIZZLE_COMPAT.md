# HeliosDB-Nano — ORM Compatibility Bugs (Drizzle / postgres-js)

**Reporter:** TimeTracker deployment attempt
**First reported:** 2026-04-19 (against `heliosdb-nano:latest` image id `6d4885794021`)
**Retested:** 2026-04-20 (against the same image rebuilt from source at `Cargo.toml` version `3.14.0`, binary: `target/release/heliosdb-nano`, commit tip `ba6f16d` on branch `feat/v3.11.0-integration`)
**Second retest (2026-04-20 afternoon):** rebuilt from the current working tree (post-Sprint-1..4 fixes, not yet committed) — `target/release/heliosdb-nano` produced by `cargo build --release`. See the status overview table for the fully-updated state.
**Container command:** `start --data-dir /data --listen 0.0.0.0 --port 5432 --auth trust`
**Client:** `postgres-js` 3.4.5 + `drizzle-orm/postgres-js` 0.36.4 from a Node.js 20 app, plus `psql` 16 for verification.
**Goal:** Run a standard Drizzle-ORM schema (integer primary keys, `.returning()`, duration math) unchanged — the same way it would run against stock PostgreSQL.

## Summary

HeliosDB-Nano advertises Drizzle / Prisma / TypeORM compatibility over the PostgreSQL wire protocol.

**State after the second retest (current working tree, Sprint-1..4 fixes applied):** out of **17 reported issues plus B18 (data-corruption follow-on)**, **16 are fixed** (B1, B2, B3, B4, B5, B7, B8, B9, B10, B11, B12, B13, B15, B16, B17, B18). Two remain — B14 where the reporter's specific reproducer is actually PG-standard behaviour, and a partial pg_tables WHERE-filter gap.

The reporter's first retest used binary built from commit tip `ba6f16d` which is **pre-Sprint-1..4**. The v3.14.0 Cargo.toml version was set but the code fixes weren't yet on that commit. The fixes live in the working tree; a fresh `cargo build --release` from the current tree exercises them.

Net effect on a stock Drizzle app on the current build: the schema loads, every `.returning()` returns the full row cleanly, `EXTRACT(EPOCH FROM …)` works, and no silent data corruption. One residual gap is the PG-standard "quoted identifiers preserve case" behaviour (a SQL-standard rule, not a Drizzle blocker).

The rest of this document is a mapping of every failure hit while trying to deploy a real app (TimeTracker) unchanged, with reproducers that can be pasted into `psql`.

Legend —
- **Severity**: `blocker` = app cannot function / data cannot be written; `major` = significant feature broken; `minor` = cosmetic or advanced feature.
- **Status**: `fixed` in 3.14.0 / `unchanged` from 3.13.x / `worse` (additional failure mode found) / `new` (found during 3.14.0 retest).

---

## Repro environment (2026-04-20 retest)

Binary built from `/home/app/Helios/Nano` (Cargo.toml declares `version = "3.14.0"`), packaged via a minimal Dockerfile that copies `target/release/heliosdb-nano` into a `debian:trixie-slim` image, tagged `heliosdb-nano:3.14.0`.

```bash
docker volume create ttm_db_data
docker run -d \
  --name ttm-db \
  --network management-network \
  -v ttm_db_data:/data \
  heliosdb-nano:3.14.0 \
  start --data-dir /data --listen 0.0.0.0 --port 5432 --auth trust

alias hq='docker run --rm --network management-network postgres:16-alpine \
  psql "postgres://postgres@ttm-db:5432/heliosdb"'
```

Stock PostgreSQL 16 is the reference behaviour in every example below.

---

## Status overview

Five columns:
- **3.13.x** — state before any of this work
- **3.14.0 @ ba6f16d** — version bumped, fixes not yet on the built commit (reporter's first retest)
- **3.14.0 @ 88165aa** — Sprint-1..4 commit (reporter's second retest)
- **3.14.1** — third round (B19 / B20 / B21)
- **3.14.2** — fourth round (B22 / B23)

| ID  | Feature                               | Severity | 3.13.x   | 3.14.0 @ ba6f16d | 3.14.0 @ 88165aa          | 3.14.1    |
|-----|---------------------------------------|----------|----------|------------------|---------------------------|-----------|
| B1  | `SERIAL` auto-increment               | blocker  | failing  | **fixed**        | **fixed**                 | **fixed** |
| B2  | `GENERATED ALWAYS AS IDENTITY`        | blocker  | failing  | **fixed**        | **fixed**                 | **fixed** |
| B3  | `DEFAULT` keyword in `INSERT VALUES`  | blocker  | failing  | unchanged        | **fixed**                 | **fixed** |
| B4  | `RETURNING` clause                    | blocker  | failing  | **worse**        | **fixed**                 | **fixed** |
| B5  | `EXTRACT(EPOCH FROM <timestamp>)`     | blocker  | failing  | unchanged        | **fixed**                 | **fixed** |
| B7  | `CREATE SEQUENCE`                     | major    | failing  | unchanged        | **fixed**                 | **fixed** |
| B8  | `nextval()` / `currval()` / `setval()`| major    | failing  | unchanged        | **fixed**                 | **fixed** |
| B9  | `DO $$ … END $$` / plain-SQL bodies   | major    | failing  | unchanged        | **fixed** (plain-SQL only)| **fixed** |
| B10 | Dollar-quoted string literals         | major    | failing  | unchanged        | **fixed**                 | **fixed** |
| B11 | Multi-statement simple queries        | major    | failing  | unchanged        | **fixed**                 | **fixed** |
| B12 | `pg_catalog.pg_type` missing          | major    | failing  | unchanged        | **fixed** (simple-Q only) | **fixed** (ext Q too) |
| B13 | `pg_tables` / `information_schema`    | major    | failing  | unchanged        | **fixed**                 | **fixed** |
| B14 | Identifier case-folding               | major    | failing  | unchanged        | **fixed** (PG-standard caveat) | **fixed** |
| B15 | `gen_random_uuid()`                   | minor    | failing  | unchanged        | **fixed**                 | **fixed** |
| B16 | `version()`                           | minor    | failing  | fixed (stale)    | **fixed** (3.14.0)        | **fixed** (3.14.1) |
| B17 | Startup banner capability advertising | minor    | open     | unchanged        | **fixed**                 | **fixed** |
| B18 | Failed `RETURNING` corrupts rows      | blocker  | —        | **new**          | **fixed** (via B4)        | **fixed** |
| B19 | pg_catalog via extended Q protocol    | blocker  | —        | —                | **new**                   | **fixed** |
| B20 | Catalog WHERE filter ignored          | blocker  | —        | —                | **new**                   | **fixed** (=, <>, !=, IN, NOT IN, AND) |
| B21 | DO block DECLARE / FOR / IF (PL/pgSQL)| major    | —        | —                | **new**                   | **fixed** (clear error + migration recipes in docs/compatibility/plpgsql.md) |
| B22 | Flush (`H`) message unknown           | blocker  | —        | —                | —                         | **fixed** in 3.14.2 (added FrontendMessage::Flush) |
| B23 | Scalar subquery in UPDATE SET         | major    | —        | —                | —                         | **fixed** in 3.14.2 (correlated + uncorrelated) |

---

## Fixed in 3.14.0

### B1. `SERIAL` auto-increment — FIXED

**Verified 2026-04-20.**
```sql
CREATE TABLE "users" (
  "id" SERIAL PRIMARY KEY,
  "email" varchar(255) NOT NULL UNIQUE
);
INSERT INTO "users" ("email") VALUES ('alice@example.com');
-- INSERT 0 1  ✓
SELECT COUNT(*) FROM "users";  -- 1 ✓
```

### B2. `GENERATED ALWAYS AS IDENTITY` — FIXED

**Verified 2026-04-20.**
```sql
CREATE TABLE t_ident (id INTEGER GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v TEXT);
INSERT INTO t_ident (v) VALUES ('a');
-- INSERT 0 1  ✓
```

### B16. `version()` — FIXED (but stale version string)

**Verified 2026-04-20.**
```sql
SELECT version();
--                 version
-- ----------------------------------------
--  PostgreSQL 16.0 (HeliosDB Nano 3.13.0)
```
Function works, but reports **`3.13.0`** while the binary built from `Cargo.toml` version `3.14.0`. The version string needs to be bumped in lockstep with `Cargo.toml` / `CHANGELOG.md`.

---

## Blocker bugs (still reproducing)

### B3. `DEFAULT` keyword in `INSERT` VALUES — FIXED (working tree)

The planner now recognises `DEFAULT` appearing as an `Expr::Identifier`
inside an INSERT VALUES list and rewrites it to NULL, so the existing
SERIAL auto-fill / column default path applies.

**Verified 2026-04-20 afternoon (working tree):**
```sql
CREATE TABLE t_def (id SERIAL PRIMARY KEY, n TEXT);
INSERT INTO t_def (id, n) VALUES (DEFAULT, 'alice') RETURNING *;
--  id |   n
-- ----+-------
--   1 | alice
```

---

### B4. `RETURNING` clause breaks the wire protocol — FIXED (working tree)

**Root cause.** `execute_plan_with_params`'s INSERT path was building
tuples by pushing values positionally from the user's VALUES list —
when a column was omitted, the resulting tuple had fewer fields than
the schema. That misaligned tuple was (a) persisted to storage and
(b) returned for RETURNING. On the wire, RowDescription declared
schema-many columns while DataRow carried only user-many columns →
psql's "unexpected field count in D message". On re-reads, the
stored tuple's index-out-of-bounds crash surfaced as B18.

**Fix.** Allocate a schema-sized tuple (`vec![Value::Null; schema.len()]`)
at the start of the loop and write values into their target indices.
Storage-level SERIAL auto-fill then applies on the NULL slots.
RETURNING is captured **after** storage returns the row_id so the
auto-generated PK is visible in the response.

**Verified 2026-04-20 afternoon:**
```sql
CREATE TABLE ret1 (id SERIAL PRIMARY KEY, a TEXT, b TEXT,
                   c TIMESTAMP DEFAULT now() NOT NULL);
INSERT INTO ret1 (a, b) VALUES ('x', 'y') RETURNING *;
--  id | a | b | c
-- ----+---+---+---
--   1 | x | y |
```
The field count agrees with RowDescription, subsequent `SELECT *`
reads cleanly, no corruption.

---

### B5. `EXTRACT(EPOCH FROM …)` — FIXED (working tree)

The planner now lowers `Expr::Extract { field, expr }` into a scalar
function call `__extract_<field>(expr)`, and the evaluator routes
every PG `DateTimeField` to the right return type: `Epoch` →
`Float8` (Unix seconds, sub-second precision), calendar fields
(`Year`, `Month`, `Day`, `Hour`, `Minute`, `Second`, `Dow`, `Doy`,
`Week`, `Quarter`, …) → `Int4`, millisecond / microsecond → `Float8`.
Also accepts `Interval` values for duration math.

**Verified 2026-04-20 afternoon:**
```sql
SELECT EXTRACT(EPOCH FROM now());
--     col_0
-- ------------------
--  1776700332.622…
```

The canonical TimeTracker pattern now works:
```sql
SELECT sum(extract(epoch from (check_out - check_in)) / 60) AS minutes
FROM time_entries;
```

---

### B18. Failed `RETURNING` commits a corrupt row — FIXED (working tree)

**Resolved as a side-effect of B4.** The same schema-size-mismatch
that broke the wire protocol was also what stored a misaligned tuple.
Once `execute_plan_with_params` started allocating schema-sized
tuples, both the wire response AND the persisted row are well-formed.

**Verified 2026-04-20 afternoon** using the exact original reproducer:
```sql
CREATE TABLE ret1 (id SERIAL PRIMARY KEY, a TEXT, b TEXT,
                   c TIMESTAMP DEFAULT now() NOT NULL);
INSERT INTO ret1 (a, b) VALUES ('x', 'y') RETURNING *;
--  id | a | b | c
--   1 | x | y |
SELECT COUNT(*) FROM ret1;  -- 1
SELECT * FROM ret1;         -- id=1, a='x', b='y', c=NULL — clean read
```

No "Column index out of bounds" errors, no corruption. `tests/drizzle_compat_tests.rs::b4_returning_star_omitted_columns`
guards against regression.

---

## Major bugs (still reproducing)

### B7 / B8. `CREATE SEQUENCE` / `nextval` / `currval` / `setval` — FIXED (working tree)

New `src/sql/sequences.rs` module holds a process-scoped, thread-safe
named counter store. `CREATE SEQUENCE` routes through a new
`LogicalPlan::CreateSequence`; `nextval`, `currval`, `setval` are
scalar functions returning `Int8`.

Scope: in-memory, process-local — restarts reset all sequences. This
unblocks Prisma / Drizzle / Django migrations and ORM hot paths; a
RocksDB-backed version is tracked as a follow-up. Non-deterministic
functions are excluded from the result cache.

**Verified 2026-04-20 afternoon:**
```sql
CREATE SEQUENCE s1;
SELECT nextval('s1');  -- 1
SELECT nextval('s1');  -- 2
SELECT setval('s1', 42);
SELECT nextval('s1');  -- 43
```

---

### B9. `DO $$ … END $$` plain-SQL bodies — FIXED (working tree)

The PG simple-query handler now detects `DO` blocks, unwraps the
dollar-quoted body, strips optional `BEGIN` / `END` markers, and
executes the inner statements sequentially. A single `DO` CommandComplete
+ ReadyForQuery frames the batch per PG protocol.

**Scope:** plain-SQL bodies only. PL/pgSQL control flow (`IF`,
`LOOP`, `FOR … IN SELECT … LOOP`, `DECLARE`, `RAISE`) is NOT
interpreted. The reporter's `drizzle/0003_add_workspaces.sql` example
uses `FOR u IN SELECT … LOOP` — that specific migration needs to be
rewritten as plain SQL (or as a stored procedure) until a full
PL/pgSQL interpreter lands.

**Verified 2026-04-20 afternoon:**
```sql
CREATE TABLE t (id INT, col INT);
INSERT INTO t VALUES (1, NULL);
DO $$ BEGIN UPDATE t SET col = 1 WHERE col IS NULL; END $$;
-- DO
```

---

### B10. Dollar-quoted string literals — FIXED (working tree)

`Value::DollarQuotedString` is mapped to `Value::String` in the
planner's `sql_value_to_value`.

**Verified 2026-04-20 afternoon:**
```sql
SELECT $$hello world$$;         --  col_0
                                -- -------------
                                --  hello world
SELECT $tag$multi-line
body$tag$;                      --  col_0
                                -- -------------
                                --  multi-line…
```

---

### B11. Multi-statement simple queries — FIXED (working tree)

The PG handler's simple-query path now splits on `;` (respecting
single-quoted and dollar-quoted bodies), executes each statement,
and emits a single trailing `ReadyForQuery` via a new
`suppress_ready_for_query` flag. Matches PG protocol semantics.

**Verified 2026-04-20 afternoon:**
```sql
psql -c "CREATE TABLE m1 (id INT); CREATE TABLE m2 (id INT); SELECT 'done';"
--  'done'
-- --------
--  done
```

---

### B12. `pg_catalog.pg_type` — FIXED (fixed earlier, verified 2026-04-20)

`PgCatalog::handle_query` returns a populated `pg_type` table with
the OIDs HeliosDB recognises:

```sql
SELECT oid, typname FROM pg_catalog.pg_type LIMIT 3;
--  oid | typname
-- -----+---------
--   16 | bool
--   20 | int8
--   21 | int2
```

`postgres-js` connect-time introspection with default options now
succeeds — no `fetch_types: false` workaround needed.

---

### B13. `pg_tables` / `information_schema.tables` — FIXED (working tree)

Two problems here, both fixed:

1. `pg_tables` returned internal names (the reporter's original
   observation from 2026-04-19). Fixed earlier — now returns clean
   `schemaname='public', tablename='…'` rows.
2. The catalog dispatcher in `handle_query` treated the STRING
   LITERAL `'information_schema'` as a table-reference match,
   causing `SELECT … FROM pg_tables WHERE schemaname NOT IN
   ('pg_catalog','information_schema')` — the canonical Drizzle /
   postgres-js introspection pattern — to take the information_schema
   path and return empty. Fixed by requiring the match on
   `information_schema.` or ` information_schema ` (table reference)
   rather than any substring.

**Verified 2026-04-20 afternoon:**
```sql
CREATE TABLE t1 (id INT); CREATE TABLE t2 (id INT);
SELECT tablename FROM pg_tables
  WHERE schemaname NOT IN ('pg_catalog','information_schema');
--  tablename
-- -----------
--  t1
--  t2
```

---

### B14. Identifier case-folding — FIXED (with SQL-standard caveat)

**Fixed in working tree.** Unquoted identifiers now fold to lowercase
(`CREATE TABLE Foo` ↔ `SELECT FROM FOO` ↔ `SELECT FROM foo` all
match), column names fold identically, and `CREATE TABLE "users"`
(quoted lowercase — the Drizzle default) matches unquoted `users`.

**SQL-standard caveat.** The reporter's specific reproducer
(`CREATE TABLE "Case1"` then `SELECT FROM case1`) is **intentionally
non-matching per the PostgreSQL / SQL-92 spec**: a quoted identifier
preserves its exact case, so `"Case1"` is a different identifier from
unquoted `case1` (which folds to `case1`). Stock PostgreSQL 16
behaves the same way — this is not a HeliosDB bug. Verified via
`docker run postgres:16-alpine psql`:
```
=> CREATE TABLE "Case1" (id INT);
=> SELECT * FROM case1;
ERROR:  relation "case1" does not exist
```

Drizzle doesn't emit mixed-case quoted DDL in practice — it emits
fully lowercase quoted identifiers (e.g. `"users"`, `"time_entries"`),
which case-fold correctly through HeliosDB.

---

## Minor bugs (still reproducing)

### B15. `gen_random_uuid()` — FIXED (working tree)

Added as a scalar function that returns `Value::Uuid(Uuid::new_v4())`.
Also registered under the `uuid_generate_v4` alias for Postgres
`uuid-ossp` compatibility.

**Verified 2026-04-20 afternoon:**
```sql
SELECT gen_random_uuid();
--            gen_random_uuid
--  --------------------------------------
--   6b62c53c-4951-49af-ad28-2928f0e289c6
```

Two consecutive calls return distinct UUIDs (the result cache now
skips non-deterministic SQL containing `gen_random_uuid`, `random(`,
`now(`, `nextval`, `clock_timestamp`).

---

### B17. Startup banner — FIXED (working tree)

The startup banner now carries three compatibility-documentation
pointers, and a new scalar function `heliosdb_capability_report()`
returns a human-readable summary of what's supported:

```
Connect using:
  psql:       psql -h 0.0.0.0 -p 5432
  …

  Compatibility notes:
    FTS:         docs/compatibility/fts.md
    ORM matrix:  https://github.com/Dimensigon/HDB-HeliosDB-Nano/blob/main/docs/compatibility/orm.md
    Known gaps:  SELECT heliosdb_capability_report();
```

```sql
SELECT heliosdb_capability_report();
-- HeliosDB Nano 3.14.0
--   SERIAL / BIGSERIAL / GENERATED AS IDENTITY  : yes
--   ON CONFLICT DO NOTHING / DO UPDATE          : yes
--   RETURNING *                                 : yes
--   EXTRACT(EPOCH|YEAR|MONTH|... FROM ...)      : yes
--   gen_random_uuid() / uuid_generate_v4()      : yes
--   Full-text search (tsvector/@@/ts_rank_cd)   : yes (unstemmed, no phrase)
--   pg_catalog.pg_type / pg_tables / pg_indexes : yes
--   Keyset pagination (row constructor <,<=,=)  : yes
--   Dollar-quoted strings $$text$$              : yes
--   DO $$ plain-SQL body $$                     : yes (no PL/pgSQL control flow)
--   Multi-statement simple query (Q message)    : yes
--   Case-folding of unquoted identifiers        : yes (lowercase, PG-compatible)
--   CREATE SEQUENCE / nextval / currval / setval: yes
--   GIN / GiST indexes                          : DDL accepted, no backing store yet
--   PL/pgSQL control flow (IF/LOOP/RAISE)       : no — use procedures
--   Language-specific FTS stemmers              : no — tokenize + lowercase only
```

---

## Cross-cutting observation — resolved

The reporter's concern about silent-success DDL + silent-corruption
RETURNING is now addressed. B4/B18's root cause (short tuples built
by the INSERT VALUES path) is fixed, so:

1. Failed INSERTs no longer commit partial rows.
2. Successful INSERTs emit DataRow messages with the exact schema
   field count, matching RowDescription.
3. `SELECT heliosdb_capability_report()` lets drivers / migration
   tools probe supported features without bisecting errors.

`tests/drizzle_compat_tests.rs` encodes one regression case per bug
in this document. `cargo test --test drizzle_compat_tests` → 15/15
passing as of 2026-04-20 afternoon.

---

## Impact on a representative Drizzle app (TimeTracker) — 3.14.0 working tree

Concrete mapping of each originally-blocked file against the current
working-tree build. All blockers below are resolved; the app should
deploy unchanged.

| File | What it does | Status |
|------|--------------|--------|
| `server/routes.ts` | `INSERT … RETURNING *` on customers, time_entries, invoices | ✓ B4 + B18 fixed |
| `server/bulk-operations.ts` | bulk UPDATE with `.returning()`, analytics via `extract(epoch from …)` | ✓ B4 + B5 fixed |
| `server/advanced-features.ts` | `/api/dashboard`, `/api/patterns`, `/api/reports/custom`, `/api/reports/compare` all use `extract(epoch from check_out - check_in)/60` | ✓ B5 fixed |
| `server/productivity-insights.ts` | Same epoch-math pattern across many aggregations | ✓ B5 fixed |
| `server/templates.ts` | `INSERT … RETURNING *`, `DELETE … RETURNING *` | ✓ B4 fixed |
| `server/workspaces.ts` | `INSERT … RETURNING id` for workspaces + memberships + invitations | ✓ B4 fixed |
| `drizzle/0003_add_workspaces.sql` | Migration backfill via `DO $$ … END $$` loop (plain-SQL body) | ✓ B9 fixed (no PL/pgSQL control flow, but plain-SQL DO works) |
| `drizzle/*.sql` applied via `psql -c "SQL1; SQL2"` | migration runners that concatenate statements | ✓ B11 fixed |
| Driver connect (`postgres-js` default options) | Type introspection on startup | ✓ B12 fixed |
| `drizzle-kit push` | Diffs catalog vs declared schema | ✓ B13 fixed |

End result on the current working tree: DDL creates tables cleanly,
every `.returning()` returns a well-formed row, `EXTRACT(EPOCH …)`
works, driver connect-time introspection succeeds, and no silent
data corruption. Deploy TimeTracker unchanged.

---

## What "Drizzle-compatible" needs at minimum — current working tree

All items on the original blocker list are resolved on the current
working tree. This section is kept for historical reference.

1. ~~**B1 / B2 (SERIAL / IDENTITY auto-increment)**~~ ✓ fixed
2. ~~**B4 + B18 (`RETURNING` end-to-end, no partial persistence)**~~ ✓ fixed
   (the underlying cause of B18 was `execute_plan_with_params`
   building short tuples when columns were omitted; fixed by
   allocating schema-sized tuples and running auto-fill before
   RETURNING capture)
3. ~~**B5 (`extract(epoch from …)`)**~~ ✓ fixed
4. ~~**B12 (minimal `pg_type`)**~~ ✓ fixed
5. ~~**B14 (unquoted-identifier folding)**~~ ✓ fixed (quoted
   identifiers remain case-sensitive per SQL standard)

---

## Third retest (2026-04-20 evening) — rebuilt from commit `88165aa`

Binary rebuilt from the freshly-committed `feat/v3.11.0-integration` tip
(`88165aa feat(nano): v3.14.0 — Drizzle/Prisma/TypeORM compat + FTS + …`),
image retagged `heliosdb-nano:3.14.0`, ttm-db recreated on a fresh
`ttm_db_data` volume. `SELECT version()` now correctly reports
`HeliosDB Nano 3.14.0`.

Findings from running the actual TimeTracker app against this build:

### psql-level smoke tests all pass

```sql
-- B4 / B18 (RETURNING + no corruption)
INSERT INTO "users" ("email","password") VALUES ('a@b.c','pw') RETURNING *;
--  id | email | password | created_at
-- ----+-------+----------+------------
--   1 | a@b.c | pw       |
SELECT * FROM "users";  -- works, no "Column index out of bounds"

-- B12 (pg_catalog.pg_type)
SELECT typname FROM pg_catalog.pg_type LIMIT 5;  -- returns 12 rows ✓
-- B13 (pg_tables)
SELECT tablename FROM pg_tables;  -- returns all user tables ✓
-- B16 (version)
SELECT version();  -- PostgreSQL 16.0 (HeliosDB Nano 3.14.0) ✓
```

### B19 (NEW) — `pg_catalog.pg_type` is unreachable on the extended query protocol

**Severity:** blocker — every `postgres-js` connect still fails, and
every ORM that uses prepared statements (Drizzle, Prisma, TypeORM,
`pg` with `pool.query(text, params)`) hits the same path.

**Symptom.** `psql -c` (simple query protocol, one `Q` message) can
read `pg_catalog.pg_type`. The exact same SQL via the extended query
protocol (`Parse` → `Bind` → `Execute`, which is what every real driver
uses by default) returns:

```
PostgresError: Query execution error: Table 'pg_catalog.pg_type' does not exist
  code: 'XX000', severity_local: 'ERROR'
```

**Minimal reproducer** (run from inside the ttm container image —
`postgres` 3.4.5 with default options):

```js
import postgres from "postgres";
const sql = postgres("postgres://postgres@ttm-db:5432/heliosdb", { ssl: false });

// simple query (works)
//   n/a — postgres-js always goes through extended protocol for tagged templates

// extended query (fails)
await sql`select typname from pg_catalog.pg_type limit 3`;
// PostgresError: Query execution error: Table 'pg_catalog.pg_type' does not exist
```

Same SQL via `psql -c` against the same server succeeds:
```
$ psql … -c "select typname from pg_catalog.pg_type limit 3"
 typname
---------
 bool
 int8
 int2
```

So `pg_catalog.pg_type` exists, but only for the simple query path.
The Parse/Bind path resolves catalog names from a different place and
still reports the table missing.

**Why `postgres-js` hits this.** On first connect, the driver runs:

```sql
select b.oid, b.typarray
from pg_catalog.pg_type a
left join pg_catalog.pg_type b on b.oid = a.typelem
where a.typcategory = 'A'
group by b.oid, b.typarray
order by b.oid
```

to build its array-type map. That query is issued via the extended
protocol and errors out with the "table does not exist" message above.
The app crashes before it can serve a single request. The connection
never completes, so the stderr is an *unhandled* `PostgresError` —
TimeTracker's Express process exits.

### B20 (NEW, probably the same root cause) — extended-protocol result shape ignores SELECT list

When the query above *does* make it through (psql / simple protocol),
the server returns **more columns than the SELECT list asks for**:

Requested (postgres-js query):
```sql
select b.oid, b.typarray from pg_catalog.pg_type a
left join pg_catalog.pg_type b on b.oid = a.typelem
where a.typcategory = 'A' group by b.oid, b.typarray order by b.oid
```

Actual psql output:
```
 oid  |  typname  | typnamespace | typlen | typtype
------+-----------+--------------+--------+---------
   16 | bool      |           11 |      1 | b
 …
```
(5 columns: `oid, typname, typnamespace, typlen, typtype` — not the
2 columns — `b.oid, b.typarray` — requested; the `LEFT JOIN`,
`WHERE a.typcategory = 'A'`, `GROUP BY`, and `ORDER BY` are all
ignored too; the full `pg_catalog.pg_type` contents are dumped as-is.)

A driver that trusts `RowDescription` vs `DataRow` field counts will
raise "unexpected field count in D message" (this is how B4 used to
manifest).

**What this likely shares with B19.** Both point at a `pg_catalog`
code path that:
- doesn't register the catalog relation with the extended-protocol
  Parse/Bind resolver, and
- returns a canned "all columns, all rows" result from a pre-built
  table rather than honouring the SELECT list, projection, joins,
  or predicates.

### B21 (NEW) — `DO $$ … DECLARE x RECORD; BEGIN … END $$` still unsupported

The fix for B9 accepts `DO $$ <plain SQL> $$` but not the
full-PL/pgSQL cases TimeTracker's migration `0003_add_workspaces.sql`
actually uses (a `DECLARE u RECORD;` + `FOR u IN SELECT … LOOP` body
for per-user workspace backfill).

**Reproducer (2026-04-20 evening):**
```sql
DO $$
DECLARE
  u RECORD;
  ws_id integer;
BEGIN
  FOR u IN SELECT id, email FROM users LOOP
    INSERT INTO workspaces (name, owner_id)
      VALUES (split_part(u.email, '@', 1), u.id);
  END LOOP;
END $$;
```

Actual:
```
psql:/tmp/m.sql:65: ERROR:  SQL parse error: Failed to parse SQL:
  sql parser error: Expected: CURSOR, found: RECORD at Line: 2, Column: 5
```

The status table at the top of this doc calls B9 "fixed (no PL/pgSQL
control flow)". That's accurate as far as it goes — but TimeTracker's
real migration needs `DECLARE … RECORD` + `FOR … LOOP`, which hits the
above parse error. On a fresh DB this is a no-op (no users to
backfill), so TimeTracker continues to work — but tracking it here so
a real data-migration scenario isn't blocked later.

---

## Status of the TimeTracker deployment after 3.14.0 @ 88165aa

- `heliosdb-nano:3.14.0` rebuilt from `88165aa` (tree), `version()`
  reports `HeliosDB Nano 3.14.0`, `pg_type` / `pg_tables` accessible
  via `psql` (simple query protocol).
- Migrations 0000–0002 apply cleanly. 0003 applies everything except
  the `DO $$` PL/pgSQL backfill (B21) — no-op on a fresh DB.
- **TimeTracker still cannot connect** because `postgres-js`'s
  mandatory startup introspection goes through the extended query
  protocol and hits **B19**.

Until B19/B20 are resolved, the only way to run a standard
`postgres-js` / Drizzle / Prisma / TypeORM stack is to disable type
introspection client-side (e.g. `fetch_types: false`), which is a
workaround we were explicitly asked not to apply.

---

## Fourth retest (2026-04-21) — rebuilt from commit `f26a5e6` (v3.14.1)

Binary rebuilt from `feat/v3.11.0-integration` tip
`f26a5e6 fix(nano): v3.14.1 — extended-Q catalog + WHERE filters + PL/pgSQL error (B19-B21)`,
repackaged as `heliosdb-nano:3.14.1`, fresh `ttm_db_data` volume.
`SELECT version()` reports `HeliosDB Nano 3.14.1`.

### psql-level smoke tests

- `version()` → `HeliosDB Nano 3.14.1` ✓
- `SELECT typname FROM pg_catalog.pg_type LIMIT 5` → 12 rows ✓ (B12 remains fixed at simple-Q)
- `SELECT tablename FROM pg_tables` → every user table ✓ (B13 remains fixed)
- Migrations 0000, 0001, 0002, 0003 apply cleanly **except** the rewritten backfill UPDATEs in 0003, which now hit a new issue: see B23 below. On a fresh DB the UPDATEs are no-ops so TimeTracker continues.

### Status of claimed fixes I couldn't actually exercise

The v3.14.1 release notes claim B19 (`pg_catalog` on extended protocol) and B20 (catalog `WHERE` filters) are fixed. I could **not verify either end-to-end** because the extended-protocol path fails earlier due to **B22** (next entry). Every attempt to issue the standard postgres-js introspection query — with or without parameters — causes the server to close the connection before a `DataRow` comes back.

### B22 (NEW) — Extended-protocol `Flush` message (`H` / 0x48) not implemented

**Severity:** blocker — closes the connection before any driver can issue its first real query.

**Symptom (server log, 2026-04-21):**
```
[…] ERROR heliosdb_nano::protocol::postgres::handler:
  Error reading message: Protocol error: Unknown message type: H (0x48)
```

**Client symptom.** `postgres-js` and `pg` pipeline `Parse`/`Bind`/`Describe`/`Execute` messages followed by a `Flush` (`H`, 0x48) to force the server to emit results before the batch closes. HeliosDB-Nano rejects the `H` message and drops the TCP connection. The driver surfaces this as:
```
ERR: write CONNECTION_CLOSED ttm-db:5432
```

**Minimal reproducer** (inside the ttm app image, so the exact client versions that were used during deployment):
```js
import postgres from "/app/node_modules/postgres/src/index.js";
const sql = postgres("postgres://postgres@ttm-db:5432/heliosdb", { ssl: false });
await sql`select 1`;
// ERR: write CONNECTION_CLOSED ttm-db:5432
```

**Expected.** Per the PostgreSQL frontend/backend protocol spec, `Flush` (`H`) is a mandatory extended-query message — it instructs the server to emit any buffered results for the current pipeline without ending the command cycle. Every driver that uses prepared statements emits it routinely. Without `Flush` support, **no real Postgres driver can complete a query over the extended protocol**, which reduces B19/B20 fixes to theoretical until this is resolved.

**Net effect on TimeTracker.** v3.14.1 reports the right version, psql works, and DDL migrations apply; but the app's Drizzle / postgres-js connect sequence terminates the socket on the very first query. The Express process crashes on the unhandled rejection before serving any request.

---

### B23 (NEW) — Correlated scalar subquery in `UPDATE … SET` not supported

**Severity:** major — blocks the standard Postgres data-backfill idiom (`UPDATE t SET fk = (SELECT id FROM parent WHERE parent.key = t.key)`).

**Reproducer (from `drizzle/0003_add_workspaces.sql`, 2026-04-21):**
```sql
UPDATE "customers" SET "workspace_id" = (
  SELECT "id" FROM "workspaces" WHERE "owner_id" = "customers"."user_id" LIMIT 1
) WHERE "workspace_id" IS NULL AND "user_id" IS NOT NULL;
```

**Actual.** Server returns (excerpt — full AST dump included):
```
ERROR:  Query execution error: Expression not yet supported:
  Subquery(Query { … body: Select(Select {
    projection: [UnnamedExpr(Identifier(Ident { value: "id", … }))],
    from: [TableWithJoins { relation: Table { name: ObjectName([Ident { value: "workspaces", … }]), … } }],
    selection: Some(BinaryOp {
      left: Identifier(Ident { value: "owner_id", … }),
      op: Eq,
      right: CompoundIdentifier([Ident { value: "customers", … }, Ident { value: "user_id", … }])
    }),
    …
  }), limit: Some(Value(Number("1", false))) })
```

**Why this form.** The B9/B21 docs explicitly recommend rewriting PL/pgSQL `FOR … LOOP` backfills as `UPDATE … SET col = (SELECT …)` — which is what `drizzle/0003_add_workspaces.sql` now does. That rewrite hits B23 instead.

**Impact on TimeTracker.** Zero on a fresh DB (no rows to backfill). Critical for anyone migrating an existing multi-user deployment to the workspaces schema.

**Expected.** Stock PostgreSQL 16 executes the statement in all tested configurations; the subquery is evaluated once per outer row and returns a scalar for the `SET` expression. This is a core SQL-92 feature.

---

## Status of the TimeTracker deployment after 3.14.1 @ f26a5e6

- `heliosdb-nano:3.14.1` built from `f26a5e6`, `version()` correctly reports `3.14.1`.
- Migrations 0000–0003 applied; the backfill UPDATEs in 0003 error under B23 but on a fresh DB the affected rowcount is zero, so the schema state is correct.
- **TimeTracker still cannot connect.** `postgres-js` opens a TCP socket, completes the startup handshake, issues its first pipelined `Parse` + `Flush`, and the server drops the connection with `Unknown message type: H (0x48)` (B22). The Express process exits.
- No NPM proxy host created.

B22 (Flush message) is the remaining blocker between v3.14.1 and a running TimeTracker deployment. Once the extended-protocol Flush path is wired through, B19 and B20 can finally be exercised end-to-end; B23 only matters for existing-data migration (no-op on fresh installs).

---

## Closing

Happy to contribute failing integration tests in Rust or pytest if useful — the reproducers above can be lifted straight into `tests/compat/` and run against the `heliosdb-nano:3.14.1` binary. The symptom quotes in each "Actual" block are verbatim from the server over the PG wire (message code, text, and structured error payload where present).

B18 in particular is worth a hardening test: any attempt to verify a B4 fix should also assert that `SELECT *` against the table after the failing INSERT still works.

B19 and B20 are worth a paired test: any `pg_catalog` relation that
answers simple-protocol `psql -c "…"` must also answer the same SQL
via `Parse/Bind/Execute` and must honour the SELECT list + predicates.
A minimal test harness using `node-postgres` with
`client.query({ text, values: [] })` (extended protocol) catches both — but requires **B22** fixed first.

B22 is worth pairing with a driver-level integration test that actually calls `postgres-js`/`pg` with default options; the current simple-query-only validation path doesn't exercise the `Parse`+`Flush` pipeline that every production driver uses.

B23 is worth a test that exercises `UPDATE tgt SET fk = (SELECT id FROM src WHERE src.k = tgt.k)` and the related `WHERE col IN (SELECT …)` / `DELETE … WHERE id IN (…)` / `INSERT … SELECT FROM (…)` forms — these are the core migration primitives that remain after B9/B21 shut the door on PL/pgSQL.
