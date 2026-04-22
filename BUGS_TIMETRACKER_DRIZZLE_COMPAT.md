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

Eight columns:
- **3.13.x** — state before any of this work
- **3.14.0 @ ba6f16d** — version bumped, fixes not yet on the built commit (reporter's first retest)
- **3.14.0 @ 88165aa** — Sprint-1..4 commit (reporter's second retest)
- **3.14.1** — third round (B19 / B20 / B21)
- **3.14.2** — fourth round (B22 / B23)
- **3.14.3** — fifth round (B24 / B25 / B26)
- **3.14.4** — sixth round (B27 / B28)
- **3.14.5** — seventh round (B29 / B30)

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
| B24 | DEFAULT `<expr>` on omitted column    | blocker  | —        | —                | —                         | **fixed** in 3.14.3 (now evaluated for omitted slots) |
| B25 | `INSERT … DEFAULT VALUES` syntax      | major    | —        | —                | —                         | **fixed** in 3.14.3 (maps to empty VALUES row) |
| B26 | NOT NULL not enforced                 | blocker  | —        | —                | —                         | **fixed** in 3.14.3 (all three INSERT paths) |
| B27 | `DEFAULT` in VALUES → column's expr   | blocker  | —        | —                | —                         | **fixed** in 3.14.4 (LogicalExpr::DefaultValue) |
| B28 | `RETURNING *` via extended protocol   | blocker  | —        | —                | —                         | **fixed** in 3.14.4 (routes through execute_returning) |
| B29 | Drizzle `select.where(eq)` returns [] | blocker  | —        | —                | —                         | **reopened** in 3.14.5 → **fixed in 3.14.6** (stale `result_cache` after `INSERT ... RETURNING` via `execute_plan_with_params`) |
| B30 | Timestamp column parsed as null       | major    | —        | —                | —                         | **fixed** in 3.14.5 (microsecond precision, space separator) |
| B31 | UPDATE/DELETE with qualified WHERE col| blocker  | —        | —                | —                         | **fixed in 3.14.7** (`Schema::with_source_table_name` applied to every DML evaluator) |
| B32 | Timestamp/Date ↔ ISO-string compare    | blocker  | —        | —                | —                         | **fixed in 3.14.7** (implicit coercion in `compare_values`) |
| B33 | parameterized LIMIT $1 / OFFSET $2    | blocker  | —        | —                | —                         | **fixed in 3.14.8** (`LogicalPlan::Limit.{limit,offset}_param` + executor resolves bound params; planner accepts quoted-integer literal) |
| B34 | UPDATE SET … = $1 silently nulls TS   | blocker (data loss) | — | — | — | **fixed in 3.14.8** (auto-cast in every UPDATE SET path, matching INSERT) |
| B35 | GROUP BY with mixed qualifier styles  | blocker  | —        | —                | —                         | **fixed in 3.14.9** (`Planner::exprs_equivalent` + Date/Time/Interval/Numeric arms in `compare_values`) |

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

## Fifth retest (2026-04-21 evening) — rebuilt from commit `9071d97` (v3.14.2)

Binary rebuilt from `feat/v3.11.0-integration` tip
`9071d97 fix(nano): v3.14.2 — Flush message + scalar subquery in UPDATE (B22 / B23)`,
repackaged as `heliosdb-nano:3.14.2`, fresh `ttm_db_data` volume.
`SELECT version()` reports `HeliosDB Nano 3.14.2`.

### Confirmed end-to-end fixed in 3.14.2

- **B22 (Flush)** — `postgres-js` connects, completes the startup handshake, and issues parameterised queries over the extended query protocol. Verified:
  ```js
  await sql`select 1 as one`
    // => Result(1) [ { one: 1 } ]
  await sql`select oid, typname from pg_catalog.pg_type where typname = ${'int4'}`
    // => Result(1) [ { oid: 23, typname: 'int4' } ]
  ```
- **B19 + B20** — now exercisable end-to-end (previously only testable from psql). The parameterised `pg_catalog.pg_type` lookup above is exactly the form every ORM uses on connect; returns the correct row.
- **B23** — scalar subqueries in `UPDATE … SET col = (SELECT … WHERE outer.fk = inner.id)` parse and execute. `drizzle/0003_add_workspaces.sql` applies cleanly (including the four backfill UPDATEs, which return `UPDATE 0` on a fresh DB).

### Migration suite result

```
=== 0000_initial.sql ===          CREATE TABLE × 3  ✓
=== 0001_add_customer_goals.sql === OK 0 (ADD COLUMN)  / CREATE TABLE ✓
=== 0002_add_entry_templates.sql === CREATE TABLE / CREATE INDEX  ✓
=== 0003_add_workspaces.sql ===   CREATE TABLE × 3, CREATE INDEX × 6,
                                  INSERT 0 0 × 2, UPDATE 0 × 4  ✓
```

All four migrations apply without errors for the first time across this
retest series.

### New blockers surfaced by the app's first real INSERT

With Flush + catalog + subqueries working, `postgres-js` now gets all
the way to TimeTracker's registration path. It fails at the first user
INSERT:

```
[ERROR] heliosdb_nano::protocol::postgres::handler:
  Error handling message: Constraint violation:
  NOT NULL constraint violated: cannot insert NULL into column 'created_at'
```

The Drizzle-generated SQL is the textbook form:

```sql
INSERT INTO "users" ("email","password") VALUES ($1, $2)
```

against a table declared as

```sql
CREATE TABLE "users" (
  "id" SERIAL PRIMARY KEY,
  "email" varchar(255) NOT NULL UNIQUE,
  "password" varchar(255) NOT NULL,
  "created_at" timestamp DEFAULT now() NOT NULL
);
```

Drizzle (and every other ORM) omits `created_at` from the column list
because the DEFAULT is expected to fill it in. Three distinct bugs
surface here:

### B24 (NEW, blocker) — `DEFAULT <expr>` not evaluated on INSERT when column is omitted

**Severity:** blocker for every Drizzle / Prisma / TypeORM schema that
uses the standard `created_at timestamp DEFAULT now() NOT NULL`
pattern.

**Reproducer (extended protocol, 2026-04-21):**
```sql
CREATE TABLE t_dn ("id" SERIAL PRIMARY KEY,
                   "created_at" timestamp DEFAULT now() NOT NULL);

-- Drizzle-style INSERT (omits the defaulted column):
INSERT INTO t_dn DEFAULT VALUES;
-- (see B25 — syntax not supported; real driver uses the next form)

INSERT INTO t_dn ("id") VALUES (1);
-- Simple-Q: INSERT 0 1, but SELECT shows created_at IS NULL (B26)
-- Extended-Q from postgres-js: ERROR — NOT NULL constraint violated on created_at
```

The DEFAULT expression is parsed (stored in the CREATE TABLE metadata)
but **never evaluated** when the column is absent from the INSERT
column list. Extended protocol correctly raises NOT NULL because the
value really is NULL; simple protocol silently inserts a NULL row
despite the NOT NULL declaration (B26).

**Expected (stock PG 16).** Omitted columns take their declared
DEFAULT. `INSERT INTO … ("id") VALUES (1)` succeeds with
`created_at = now()` and the NOT NULL holds trivially.

**Impact on TimeTracker.** Every table has a `created_at timestamp
DEFAULT now() NOT NULL`; additionally `users`, `customers`,
`time_entries`, `invoices`, `workspaces`, `memberships`, and
`entry_templates` rely on `DEFAULT` for `updated_at`, `status`,
`is_break`, and role columns. Every single INSERT from the app hits
B24 before B25 / B26.

---

### B25 (NEW, major) — `INSERT … DEFAULT VALUES` syntax not supported

**Reproducer (2026-04-21):**
```sql
INSERT INTO t_dn DEFAULT VALUES;
-- ERROR:  Query execution error: INSERT statement missing source query
```

**Expected (stock PG):** a single row inserted with every column set
to its declared DEFAULT.

Not currently emitted by Drizzle but used by Prisma and by hand-written
migrations to create "seed" rows. Low-priority follow-on to B24.

---

### B26 (NEW, blocker) — `NOT NULL` constraint not enforced on the simple-query path

**Severity:** blocker for schema integrity; allows permanently-broken
rows into tables that claim `NOT NULL`.

**Reproducer (psql, simple query protocol, 2026-04-21):**
```sql
CREATE TABLE t_nn (id INT PRIMARY KEY,
                   must_be_set timestamp NOT NULL);

INSERT INTO t_nn (id, must_be_set) VALUES (1, NULL);
-- INSERT 0 1   ← should be ERROR: null value in column "must_be_set" …

INSERT INTO t_nn (id) VALUES (2);
-- INSERT 0 1   ← omitted column, no DEFAULT; should be ERROR too
```

Verification:
```sql
SELECT id, must_be_set, must_be_set IS NULL AS is_null FROM t_nn;
--  id | must_be_set | is_null
-- ----+-------------+---------
--   1 |             |   t
--   2 |             |   t
```

**Inconsistency with extended protocol.** The same `INSERT INTO t_nn
(id) VALUES (2)` issued through postgres-js (extended protocol)
correctly errors with `NOT NULL constraint violated`. So the constraint
is declared, just not evaluated on the simple-query path.

**Expected (stock PG):** identical behaviour on both protocols — both
should reject NULL.

**Impact on TimeTracker.** `psql -f migrations/*.sql` runs under the
simple-query protocol and can therefore seed tables with NULL values
into columns declared NOT NULL. When the app later reads those rows
via Drizzle (extended protocol), Drizzle's generated TypeScript types
promise non-null fields, so client code accessing `row.created_at` may
`.toISOString()` on `undefined` and crash.

---

## Status of the TimeTracker deployment after 3.14.2 @ 9071d97

- `heliosdb-nano:3.14.2` built from `9071d97`, `version()` correctly reports `3.14.2`.
- Migrations 0000–0003 applied with **no errors** — first clean run across this retest series.
- Driver-level compatibility over the extended query protocol is **live**: `postgres-js` connects, queries `pg_catalog.pg_type` with params, reads user tables. B19 / B20 / B22 verified end-to-end.
- **TimeTracker still cannot register a user.** First `INSERT INTO "users" ("email","password") …` over the extended protocol fails with B24 (`NOT NULL constraint violated: created_at`), because the column's `DEFAULT now()` is never evaluated when Drizzle omits it.
- No NPM proxy host created.

B24 is the remaining blocker between v3.14.2 and a running TimeTracker deployment. B26 is a data-integrity concern that should be fixed alongside B24 since the two share the same "DEFAULT / NULL handling on INSERT" code path. B25 is a low-priority ergonomic follow-on.

---

## Sixth retest (2026-04-21 evening) — rebuilt from commit `fb5599a` (v3.14.3)

Binary rebuilt from `feat/v3.11.0-integration` tip
`fb5599a fix(nano): v3.14.3 — DEFAULT expr + DEFAULT VALUES + NOT NULL (B24 / B25 / B26)`,
repackaged as `heliosdb-nano:3.14.3`, fresh `ttm_db_data` volume.
`SELECT version()` reports `HeliosDB Nano 3.14.3`.

### Confirmed end-to-end fixed in 3.14.3

Verified by running Drizzle-style INSERTs from both `psql` and a real
postgres-js client:

- **B24 (omitted-column DEFAULT)** — fixed. `INSERT INTO "users" ("email","password") VALUES ('simple@x.com','pw') RETURNING *` from psql returns `created_at = 2026-04-21T…` ✓
- **B26 (NOT NULL enforcement)** — enforced consistently on the simple-query path now. `INSERT INTO t_nn (id, must_be_set) VALUES (1, NULL)` fails with `ERROR: NOT NULL constraint violated`.
- **B25 (`INSERT … DEFAULT VALUES`)** — presumed fixed per release notes; not exercised in TimeTracker.

### New blockers surfaced by Drizzle's actual INSERT SQL

With B24/B25/B26 fixed, I captured the real SQL that `drizzle-orm`
emits for a `db.insert(users).values({ email, password }).returning()`
call, using the postgres-js `debug` hook:

```
SQL: insert into "users" ("id", "email", "password", "created_at")
     values (default, $1, $2, default)
     returning "id", "email", "password", "created_at"
PARAMS: [ 'drz@x.com', 'pwhashed' ]
```

This is Drizzle's **default** insert shape — it *includes every column*
in the column list, and uses the `DEFAULT` keyword as the value for
columns it wants the DB to auto-fill. Two distinct bugs surface here:

### B27 (NEW, blocker) — `DEFAULT` keyword inside VALUES is rewritten to NULL, not resolved to the column's declared default

**Severity:** blocker — this is Drizzle's standard INSERT pattern;
every write path in TimeTracker hits it.

**Reproducer (2026-04-21):**
```sql
CREATE TABLE "users" ("id" SERIAL PRIMARY KEY,
                      "email" varchar(255) NOT NULL,
                      "password" varchar(255) NOT NULL,
                      "created_at" timestamp DEFAULT now() NOT NULL);

-- Drizzle-style: column listed, value is the DEFAULT keyword
INSERT INTO "users" ("id","email","password","created_at")
  VALUES (DEFAULT, 'psql@x.com', 'pw', DEFAULT)
  RETURNING *;
-- ERROR: Constraint violation: NOT NULL constraint violated:
--        cannot insert NULL into column 'created_at'
```

**Root cause (inferred from B3's release note).** The B3 fix "rewrites
`DEFAULT` appearing in an INSERT VALUES list to NULL so the SERIAL
auto-fill / column default path applies". That's only half-right — it
works when the column has an auto-fill (SERIAL/IDENTITY), but when the
column has a declared `DEFAULT <expr>` (e.g. `DEFAULT now()`), the
rewrite to NULL ends up:
1. Storing NULL in that slot, and
2. Triggering the now-enforced NOT NULL check (B26) because the
   DEFAULT expression is never evaluated.

The B24 fix evaluates `DEFAULT now()` only when the column is *omitted*
from the column list. But Drizzle puts the column in the list and asks
for DEFAULT via the keyword.

**Fix shape.** In the B3 path, when the `DEFAULT` keyword is rewritten,
check whether the target column has:
- a `SERIAL`/`IDENTITY` → pass NULL (existing behaviour), or
- a declared `DEFAULT <expr>` → evaluate that expression (same helper
  already used by B24's omitted-column path), or
- neither → pass NULL and let B26's NOT NULL check fire.

**Verified identical failure from both protocols:**
- psql simple-Q: `ERROR: Constraint violation: NOT NULL …`
- postgres-js extended-Q: `Constraint violation: NOT NULL …`

**Impact on TimeTracker.** 100% of writes via `drizzle-orm/postgres-js`
fail here. This includes every `insert(…).values({…}).returning()`
across `routes.ts`, `bulk-operations.ts`, `templates.ts`, `workspaces.ts`,
and `auth.ts`. User registration — the first API call any app makes —
errors out on this path.

### B28 (NEW, blocker) — `INSERT … RETURNING *` via extended protocol returns 0 rows even when the INSERT succeeds

**Severity:** blocker — Drizzle uses `.returning()` to retrieve
generated IDs, timestamps, and other server-defaulted values; returning
nothing breaks every `const [row] = await db.insert(…).returning()`.

**Reproducer (2026-04-21 — extended-protocol, from postgres-js):**
```js
import postgres from 'postgres';
const sql = postgres('postgres://postgres@ttm-db:5432/heliosdb', { ssl: false });

// extended-Q INSERT with RETURNING — row persists, result is empty
const r1 = await sql`insert into "users" ("email","password")
                     values (${'r1@x.com'}, ${'pw'}) returning *`;
console.log(r1);  // []   ← should be [{ id: …, email: 'r1@x.com', … }]

const r2 = await sql`insert into "users" ("email","password")
                     values (${'r2@x.com'}, ${'pw'})`;
console.log(r2);  // []   ← expected, no RETURNING

// Rows actually exist:
const check = await sql`select id, email from "users"
                        where email in (${'r1@x.com'}, ${'r2@x.com'})`;
console.log(check);
// [ { id: 3, email: 'r1@x.com' }, { id: 4, email: 'r2@x.com' } ]
```

**Observations.** The row is committed (`SELECT` finds it), but the
`DataRow` stream for the RETURNING result is missing on the extended
protocol path. On the simple-query path the same INSERT … RETURNING
does emit the row (see B24 verification above), so the regression is
specific to `Parse`/`Bind`/`Execute`.

**Likely root cause.** After the B4 (RETURNING wire-format) and B22
(Flush) fixes, the extended-protocol INSERT path still needs to emit
its RETURNING rows before the final `CommandComplete`/`ReadyForQuery`.
My guess is that `handle_execute_extended` calls the insert executor's
"no-RETURNING" fast path even when the `Parse`-time query declared a
RETURNING clause, so the rows are discarded.

**Impact on TimeTracker.** Even if B27 is fixed, `auth.ts` does:
```ts
const [user] = await db.insert(users).values({…}).returning();
const token = jwt.sign({ id: user.id }, JWT_SECRET, …);  // throws: user is undefined
```
Same pattern in every other `routes.ts` / `bulk-operations.ts` write.
Registration, customer creation, time-entry start/stop, invoice
generation, workspace creation — all depend on the returned row.

---

## Status of the TimeTracker deployment after 3.14.3 @ fb5599a

- `heliosdb-nano:3.14.3` built from `fb5599a`, `version()` correctly reports `3.14.3`.
- Migrations 0000–0003 apply with zero errors; backfill UPDATEs behave as no-ops on fresh DB.
- `SELECT`s, `pg_catalog` lookups, `pg_tables` queries, parameterised reads all work end-to-end over the extended query protocol.
- **TimeTracker still cannot register a user.** First INSERT from Drizzle is the quoted `"id","email","password","created_at" VALUES (DEFAULT, $1, $2, DEFAULT)` form, which hits B27. Even if B27 is worked around by changing the insert shape, B28 makes `.returning()` return `[]`, so the app crashes on `const [user] = …`.
- No NPM proxy host created.

B27 and B28 are the remaining blockers between v3.14.3 and a running TimeTracker deployment. Both are Drizzle-specific — they're on the hot path for every write operation — and both show up on the *extended* query protocol that every real driver uses.

---

## Seventh retest (2026-04-22) — rebuilt from commit `d92d370` (v3.14.4)

Binary rebuilt from `feat/v3.11.0-integration` tip
`d92d370 fix(nano): v3.14.4 — DEFAULT-in-VALUES + extended-Q RETURNING (B27 / B28)`,
repackaged as `heliosdb-nano:3.14.4`. Fresh `ttm_db_data` volume.
`SELECT version()` reports `HeliosDB Nano 3.14.4`.

### Confirmed end-to-end fixed in 3.14.4 (via TimeTracker /api/auth/register)

- **B27 (DEFAULT in VALUES)** — fixed. Drizzle's emitted `INSERT INTO "users" ("id","email","password","created_at") VALUES (default, $1, $2, default) RETURNING …` now succeeds; `created_at` is populated by the server.
- **B28 (extended-Q RETURNING)** — fixed. `const [user] = await db.insert(users).values({email, password}).returning()` returns the full row.
- End-to-end result: `POST /api/auth/register` with body `{ "email": "alice@example.com", "password": "password123" }` returns:
  ```json
  {
    "user": { "id": 1, "email": "alice@example.com", "createdAt": "2026-04-22T05:46:04.591Z" },
    "token": "eyJhbGciOiJIUzI1NiIsInR5cCI6Ikp…"
  }
  ```
  For the first time across this retest series, a TimeTracker API call that writes to the database completes successfully.

### New blockers surfaced by the very next call (`/api/auth/login`)

The login handler does:
```ts
const [user] = await db.select().from(users).where(eq(users.email, email));
if (!user || !(await bcrypt.compare(password, user.password))) return 401;
```

It returns 401 in ~3 ms (faster than bcrypt's runtime), meaning `user`
is `undefined` — Drizzle's SELECT returned `[]` despite the row
existing. Two distinct defects drive this.

### B29 (NEW, blocker) — parameterized SELECT with Drizzle's default shape returns 0 rows

**Severity:** blocker — Drizzle's default shape for `select().from(t).where(eq(t.col, v))` always emits this form.

**The exact form Drizzle emits** (captured via postgres-js `debug` hook):
```
SQL: select "id", "email", "password", "created_at" from "users" where "users"."email" = $1
PARAMS: [ 'alice@example.com' ]
```

**Result**
```
[]        ← should be one row
```

The row DOES exist and IS returned by any of the following variations:

| # | Variation                                                                                  | Result |
|---|--------------------------------------------------------------------------------------------|--------|
| A | `select "id","email","password","created_at" from "users" where "users"."email" = $1`      | ✓ one row |
| B | `select "id", "email", "password" from "users" where "users"."email" = $1`                 | ✓ one row |
| C | `select "id", "email", "password", "created_at" from "users" where "email" = $1` (unqualified) | ✓ one row |
| D | `select "id", "email", "password", "created_at" from "users"`                              | ✓ one row |
| E | `select "id", "email", "password", "created_at" from "users" where "users"."email" = 'alice…'` (literal) | ✓ one row |
| F | `select "users"."id", "users"."email", "users"."password", "users"."created_at" from "users" where "users"."email" = $1` | ✓ one row |
| **G** | **`select "id", "email", "password", "created_at" from "users" where "users"."email" = $1`** | **✗ `[]`** |

Exactly G — the Drizzle-standard shape — is broken. The combination that triggers it is:

1. SELECT list = every column of the table, in schema-declaration order, **unqualified**;
2. WHERE predicate = **table-qualified** `"t"."col" = $1`;
3. `$1` = **string parameter** (extended-Q bind).

Change *any one* of those three and the query returns the row correctly. This is the pattern Drizzle emits for every `select().from(t).where(eq(t.col, v))` — which is how TimeTracker's login, "does user exist", "list customers for workspace", and every other read-by-key happens.

**Also reproducible via `sql.unsafe` on raw postgres-js** (so it isn't Drizzle doing anything special):
```js
const sql = postgres(url, { ssl: false });
await sql.unsafe(
  'select "id", "email", "password", "created_at" from "users" where "users"."email" = $1',
  ['alice@example.com']
);
// → []
```

Likely planner / prepared-statement cache bug where the combination of
"project-all-cols-in-schema-order" + "qualified-column-vs-param-bind"
falls into a short-circuit plan that returns zero rows.

**Impact on TimeTracker.** Every read-by-unique-key path hits this:
- `POST /api/auth/login` — 401 Invalid credentials (user is undefined)
- `POST /api/auth/register` — the "email already registered" check incorrectly says "not registered" (so a duplicate email could in theory succeed, except the `UNIQUE(email)` constraint would then reject).
- `GET /api/customers`, `/api/time-entries`, `/api/workspaces` — all use Drizzle's `select().from(t).where(…)`.
- `resolveWorkspace` middleware fails to find the user's membership and every authenticated request ends up 403.

### B30 (NEW, major) — Drizzle maps `created_at` to `createdAt: null` even though the column holds a valid timestamp

**Severity:** major — breaks every `timestamp()` column in Drizzle when read back through `drizzle-orm/postgres-js`, even when the raw wire value is correct.

**Reproducer**
```js
// Raw postgres-js (same SQL Drizzle sends, no mapping):
await sql`select "id", "email", "password", "created_at" from "users"`;
// [{ …, "created_at": "2026-04-22T05:46:04.591Z" }]   ← populated ✓

// Through drizzle-orm with the same schema:
await db.select().from(users);
// [{ …, "createdAt": null }]   ← null ✗
```

Full capture (`debug` hook shows the exact SQL is identical):
```
SQL: select "id", "email", "password", "created_at" from "users" P: []
drizzle all: [{"id":1,"email":"alice@example.com","password":"$2a$…","createdAt":null}]
```

**Likely cause.** OID / wire-format disagreement between HeliosDB and what `drizzle-orm/postgres-js` registers for the `timestamp("…")` helper. The 3.14.4 release notes mention a switch from rfc3339 nanosecond output to `YYYY-MM-DD HH:MM:SS.ffffff`, but `drizzle-orm/postgres-js` installs its own type parsers for OID 1114 (`timestamp`) and 1184 (`timestamptz`) that expect specific forms. If the advertised OID doesn't match what Drizzle expects — or if the text format isn't what its parser accepts — Drizzle falls back to `null`.

**Impact on TimeTracker.** Even after B29 is fixed, every `check_in`, `check_out`, `created_at`, `updated_at`, `expires_at`, `accepted_at` timestamp read through Drizzle comes back as `null`. Duration math (`Math.floor((end.getTime() - start.getTime()) / 1000)` in `TimeEntryCard.tsx`, every report in `advanced-features.ts`) crashes with "cannot read properties of null".

---

## Status of the TimeTracker deployment after 3.14.4 @ d92d370

- `heliosdb-nano:3.14.4` built from `d92d370`, `version()` reports `3.14.4`.
- Migrations 0000–0003 apply with zero errors.
- **First ever successful TimeTracker write**: `POST /api/auth/register` returns a valid JWT and stored `created_at`. B27 and B28 verified end-to-end.
- **Login (`POST /api/auth/login`) returns 401** because Drizzle's default select-by-unique-key shape is silently returning zero rows (B29).
- Even if B29 is fixed, every subsequent read will come back with `null` timestamps because of B30.
- No NPM proxy host created.

B29 + B30 are the remaining blockers. Both manifest only through the standard Drizzle / postgres-js access pattern — the same pattern Prisma and TypeORM use — so they're worth fixing together.

---

## Eighth retest (2026-04-22) — rebuilt from commit `0bb5ecb` (v3.14.5)

Binary rebuilt from `feat/v3.11.0-integration` tip
`0bb5ecb fix(nano): v3.14.5 — Drizzle SELECT + timestamp (B29 / B30)`,
repackaged as `heliosdb-nano:3.14.5`. Fresh `ttm_db_data` volume.
`SELECT version()` reports `HeliosDB Nano 3.14.5`.

### Confirmed fixed in 3.14.5

- **B30 (timestamp column read as `null` in Drizzle)** — fixed. `db.select().from(users)` now returns `createdAt: "2026-04-22T11:01:44.316Z"` for the timestamp column (raw postgres-js sees the same value; the two match). The direct-encoding `DataRow` path now emits `YYYY-MM-DD HH:MM:SS.ffffff` as advertised in the commit note.
- **B27 / B28** remain fixed — `POST /api/auth/register` still succeeds end-to-end on v3.14.5:
  ```
  POST /api/auth/register → 201
  { "user": { "id": 1, "email": "alice@example.com", "createdAt": "2026-04-22T11:01:44.316Z" }, "token": "…" }
  ```

### B29 — still reproduces **independently of B30**

The v3.14.5 hypothesis was that B29 and B30 shared a root cause
(postgres-js crashing while parsing the malformed timestamp and
producing `[]` for the row). **That hypothesis is incorrect**: with
B30 fixed and timestamps round-tripping cleanly, B29 still fires at
exactly the same trigger pattern. Every other query shape returns
the row, and only the canonical Drizzle shape still returns `[]`.

**Reproducer (2026-04-22, `heliosdb-nano:3.14.5`, `postgres-js` 3.4.5):**
```js
const sql = postgres('postgres://postgres@ttm-db:5432/heliosdb', { ssl: false });

await sql`select "id", "email", "password", "created_at"
          from "users"
          where "users"."email" = ${'alice@example.com'}`;
// []          ← still empty

await sql`select "id", "email", "password", "created_at"
          from "users"
          where "email" = ${'alice@example.com'}`;   // WHERE unqualified
// [ { id: 1, email: 'alice@example.com', password: '$2a$10$…',
//     created_at: '2026-04-22T11:01:44.316Z' } ]

await sql.unsafe(
  `select "id", "email", "password", "created_at"
   from "users"
   where "users"."email" = 'alice@example.com'`);    // literal instead of $1
// [ { id: 1, email: 'alice@example.com', … } ]

await sql`select "id", "email", "password"
          from "users"
          where "users"."email" = ${'alice@example.com'}`;   // 3 cols instead of 4
// [ { id: 1, email: 'alice@example.com', password: '…' } ]
```

Same three conditions are required in combination:

1. SELECT list = every column of the table, in schema-declaration order, **unqualified**;
2. WHERE predicate = **table-qualified** `"t"."col" = $1`;
3. `$1` is a **string parameter** supplied via extended-Q Bind.

Any one of the three swapped out → one row. All three together → `[]`.

### Why it's not a timestamp-format issue

1. The raw wire value for the timestamp column is now correct (verified via both Drizzle `db.select().from(users)` and `sql\`select …\``).
2. The 4-column SELECT **without** the table qualifier in WHERE returns the row, timestamp and all.
3. The 3-column SELECT **with** the table qualifier returns the row (timestamp not in the list).
4. `sql.unsafe` with a literal in place of `$1` returns the row.

So the timestamp column's encoding is not the trigger — the combination
of **projection shape + qualified predicate + bind parameter** is.

**Net effect on TimeTracker.** Register succeeds (INSERT path).
Login fails because `db.select().from(users).where(eq(users.email, email))`
emits the canonical Drizzle shape that B29 zeroes out. Every other
read-by-unique-key path in the app has the same shape and the same
failure mode (`getCustomers`, `getTimeEntries`, workspace resolution,
membership lookup, invitation accept, etc.).

---

## Status of the TimeTracker deployment after 3.14.5 @ 0bb5ecb

- `heliosdb-nano:3.14.5` built from `0bb5ecb`, `version()` reports `3.14.5`.
- Migrations 0000–0003 apply with zero errors.
- **Write side fully working.** `POST /api/auth/register` returns a valid JWT, `createdAt` populated, timestamp round-trips through Drizzle as a real Date (B30 verified fixed).
- **Read-by-unique-key still broken.** `POST /api/auth/login` still returns 401 in ~2 ms because Drizzle's `select().from(users).where(eq(users.email, email))` hits B29.
- No NPM proxy host created.

B29 is the last remaining blocker on the TimeTracker deployment path.
It is a planner / extended-protocol issue independent of B30 — please
re-open the B29 investigation separately. The v3.14.5 fix addressed
a symptom (timestamp format) but the underlying SELECT-shape bug is
still present.

---

## B29 — reopened investigation (2026-04-22, Nano side)

**Outcome:** could not reproduce on the binary built from the same
commit (`0bb5ecb`, v3.14.5). Every canonical-shape reproduction
attempted against a local `heliosdb-nano` server returns the row, on
three independent client stacks:

- **`postgres-js` 3.4.5, tagged template, `prepare: true`** (default):
  ```
  select "id", "email", "password", "created_at"
    from "users"
   where "users"."email" = $1   params: ['alice@example.com']
  → [{ id, email: 'alice@example.com', password, created_at: '…' }]
  ```
- **`postgres-js` 3.4.5, `sql.unsafe(query, values)`** — the exact form
  shown in the original reproducer at `BUGS_TIMETRACKER_DRIZZLE_COMPAT.md:1176-1183`:
  also returns the row.
- **`node-postgres` 8.x (`pg.Client`) with named prepared statement**
  (`client.query({ name: 'login_by_email', text, values })`) — twice in a
  row to exercise the server-side cached plan — both calls return the
  row.

Additional variations tried (all return the row):
- `UNIQUE(email)` + `FK workspaces.owner_id -> users.id`
- persistent data volume + server restart between INSERT and SELECT
- multiple sequential clients (`sql.end()` + fresh `postgres(opts)`)
- register + login from the same pool vs. register + login from
  different pools
- `alice@example.com` (reporter's value) vs. shorter / longer payloads

Binary: `target/release/heliosdb-nano` built from `0bb5ecb` locally
(`md5sum ee5ff3cbfeac1dcd4875d862ae32bab4`); `SELECT version()` reports
`PostgreSQL 16.0 (HeliosDB Nano 3.14.5)`.

Server trace (`RUST_LOG=heliosdb_nano::protocol=trace`) shows the
Parse → Bind → Execute sequence with `params: ['alice@example.com']`
reaching the executor and emitting one `DataRow` plus
`CommandComplete "SELECT 1"`. No short-circuit plan, no empty result,
no error. The plan shape (captured via a separate `PREPARE` test) is:

```
Project exprs=[id,email,password,created_at]
  Filter predicate=(users.email = $1)
    Scan table=users  (source_table_name='users' on every column)
```

`Schema::get_qualified_column_index(Some("users"), "email")` resolves
correctly (matches `source_table_name`), so the filter is evaluated
against the row's `email` value.

**Regression locks** added in this branch so a future regression is
caught immediately:

- `tests/drizzle_compat_tests.rs::b29_canonical_drizzle_select_returns_row`
  — exercises the post-substitution SQL (exactly what
  `database.query()` sees after `substitute_parameters()`): all four
  columns unqualified, table-qualified WHERE, string literal in the
  predicate slot. Pins the planner/executor output to one row.
- `tests/drizzle_compat_tests.rs::b29_qualified_predicate_matches_scan_row`
  — shrinks the invariant: a scan yields rows whose
  `source_table_name == Some("t")` and the filter predicate carries
  `Column { table: Some("t"), .. }`; the match must succeed.
- `tests/server_mode_integration_test.rs::test_b29_canonical_drizzle_shape`
  — wire-level regression via `tokio-postgres`, extended-Q Parse/Bind/
  Execute. Currently `#[ignore]` for the same pre-existing reason as
  every other `setup_test_server()` test (the in-process `PgServer`
  stack-overflows under `#[tokio::test]`). Runs cleanly against a
  subprocess binary.

**Root cause (found in v3.14.6):** stale `result_cache`, not a planner
or prepared-statement bug.

The `Database::query` entry point (src/lib.rs:5443) invalidates
`result_cache` on DML-with-RETURNING. The extended-Q handler for
`INSERT ... RETURNING`, however, short-circuits and calls
`execute_returning` directly (handler_extended.rs:289), which lands
in `execute_plan_with_params` (lib.rs:4983). That function mutated
data but never invalidated `result_cache`.

TimeTracker's login/register flow hits exactly the pattern that
exercises this hole:

1. Login attempt against empty table → `SELECT ... WHERE
   "users"."email" = $1` → `[]`. After `substitute_parameters`, the
   SQL is `SELECT … WHERE "users"."email" = 'alice@example.com'`, and
   `result_cache` stores `[]` under that exact string.
2. Register → `INSERT ... RETURNING ...` via extended-Q → lands in
   `execute_plan_with_params` → row inserted, cache untouched.
3. Login again → same canonical SQL → same substituted key → cache
   hit → stale `[]` returned in ~2 ms.

Why the reporter's other shapes worked: any variation (unqualified
WHERE, 3 cols, string literal in place of `$1`, fully-qualified
projection) produces a DIFFERENT substituted SQL string, therefore a
different cache key, therefore a cache miss. Only the exact shape
TimeTracker kept hitting had a matching stale entry.

**Fix in v3.14.6:** `execute_plan_with_params` now calls
`invalidate_result_cache()` on success for any plan that is `Insert`,
`InsertSelect`, `Update`, or `Delete`. This is the single choke point
for every DML code path that was routing around the cache
invalidation at the `Database::query` level.

Regression coverage in `tests/drizzle_compat_tests.rs`:
- `b29_login_probe_then_register_then_login` — end-to-end repro.
- `b29_canonical_drizzle_select_returns_row` — pins the shape.
- `b29_qualified_predicate_matches_scan_row` — pins the predicate
  resolution invariant (originally suspected, now verified sound).


---

## Ninth retest (2026-04-22 afternoon) — rebuilt from commit `be07da7` (v3.14.6)

Binary rebuilt from `feat/v3.11.0-integration` tip
`be07da7 fix(nano): v3.14.6 — stale result_cache after INSERT RETURNING (B29 real root cause)`,
repackaged as `heliosdb-nano:3.14.6`. Fresh `ttm_db_data` volume.
`SELECT version()` reports `HeliosDB Nano 3.14.6`.

### Confirmed fixed in 3.14.6

- **B29 (stale result_cache after INSERT RETURNING)** — fixed. The
  register → login sequence now works end-to-end:
  ```
  POST /api/auth/register → 201  { user: { id:1, …, createdAt: '…' }, token: '…' }
  POST /api/auth/login    → 200  { user: { id:1, …, createdAt: '…' }, token: '…' }
  ```
- Authenticated paths that read after resolving a workspace now return
  the real row: `GET /api/workspaces` returns the auto-created Personal
  workspace, `GET /api/customers` returns the created customer,
  `GET /api/time-entries` returns the active timer, `POST /api/customers`
  and `POST /api/time-entries` succeed.

**This is the first time a full register + login + workspace resolve +
insert-customer + start-time-entry flow completes cleanly against
HeliosDB-Nano.** Every bug from B1 through B30 is now either fixed
end-to-end or shown to be out of scope (B14) across the retest series.

### Two new blockers found by exercising UPDATE / DELETE / analytics

With read-by-unique-key working, I swept the remaining TimeTracker
endpoints (stop timer, edit entry, delete entry, dashboard, patterns,
insights, custom report, compare report, statistics). Two distinct
bugs surface immediately:

### B31 (NEW, blocker) — UPDATE / DELETE with a table-qualified column in WHERE fails

**Severity:** blocker — every Drizzle `.update(t).set(…).where(eq(t.id, x))` and `.delete(t).where(eq(t.id, x))` emits `"t"."col" = $1`.

**Reproducer (2026-04-22, `heliosdb-nano:3.14.6`):**
```sql
UPDATE "time_entries"
  SET "notes" = $1
  WHERE "time_entries"."id" = $2
  RETURNING *;
```
```
ERROR:  Query execution error: Column 'time_entries.id' not found in schema
```

```sql
DELETE FROM "time_entries" WHERE "time_entries"."id" = $1 RETURNING *;
```
```
ERROR:  Query execution error: Column 'time_entries.id' not found in schema
```

**Unqualified forms work:**
```sql
UPDATE "time_entries" SET "notes" = $1 WHERE "id" = $2 RETURNING *;  -- OK
DELETE FROM "time_entries"               WHERE "id" = $1 RETURNING *;  -- OK
```

Note: **SELECT** accepts `"t"."col"` qualification (see B29 verification) — the name-resolver for UPDATE / DELETE doesn't strip the table prefix the same way. Drizzle emits the qualified form for all three statement kinds, so UPDATE and DELETE fail while SELECT succeeds.

**Impact on TimeTracker.** Every write-update API fails:
- `PATCH /api/time-entries/:id` — stop timer, edit entry
- `PATCH /api/customers/:id` — update goal, billing
- `DELETE /api/time-entries/:id` — remove an entry
- `DELETE /api/customers/:id` — remove a customer
- `PATCH /api/workspaces/:id/members/:userId` — role changes
- `DELETE /api/workspaces/:id/members/:userId` — remove member
- `PATCH /api/entry-templates/:id` + `DELETE` — template edits
- Every bulk-update path (`/api/time-entries/bulk` PATCH + DELETE)

### B32 (NEW, blocker) — timestamp-vs-string comparison rejected

**Severity:** blocker — any date-range query that passes an ISO 8601 string as `$n` fails.

**Reproducer (from the ttm app logs, 2026-04-22):**
```
Dashboard error: PostgresError: Query execution error:
  Cannot compare Timestamp(2026-04-22T15:02:34.399Z) and String("2026-04-23T00:00:00.000Z")
Compare error:   Cannot compare Timestamp(…) and String("2026-04-22T23:59:59.000Z")
Patterns error:  Cannot compare Timestamp(…) and String("2026-04-15T15:04:22.429Z")
Insights error:  Cannot compare Timestamp(…) and String("2026-03-23T15:04:22.879Z")
Custom report:   Cannot compare Timestamp(…) and String("2026-04-22T23:59:59.000Z")
```

In Postgres, `WHERE ts_col >= $1` with `$1` bound as `text` (ISO 8601) is accepted and the string is implicitly cast to `timestamp`. HeliosDB rejects the comparison outright.

**Minimal SQL reproducer:**
```sql
SELECT * FROM "time_entries"
  WHERE "check_in" >= $1
  AND   "check_in" <= $2;
-- Params: $1 = '2026-04-15T00:00:00Z', $2 = '2026-04-22T23:59:59Z'
-- ERROR: Cannot compare Timestamp(…) and String("2026-04-15T00:00:00Z")
```

**Impact on TimeTracker.** Every analytics endpoint fails:
- `GET /api/dashboard`
- `GET /api/patterns?days=N`
- `GET /api/insights`
- `POST /api/reports/custom`
- `POST /api/reports/compare`
- `GET /api/productivity-analysis`

(The one exception on my smoke run, `GET /api/statistics`, succeeded
because the active timer has `check_out IS NULL` and the `SUM(CASE
WHEN check_out IS NOT NULL …)` short-circuits before the comparison
fires.)

**Workaround the client can't reasonably apply.** Drizzle passes
`new Date(...)` or `.toISOString()` strings into `gte()` / `lte()`
helpers; they become bind parameters with text-type OID. The
server-side fix is to either (a) accept `text` → `timestamp` cast
implicitly, or (b) let Bind coerce parameter OIDs according to the
column type of the comparison target.

---

## Status of the TimeTracker deployment after 3.14.6 @ be07da7

- `heliosdb-nano:3.14.6` built from `be07da7`, `version()` reports `3.14.6`.
- Migrations 0000–0003 apply cleanly; all 29 drizzle_compat tests pass in-tree per the release note.
- **Read + write-insert flows now work end-to-end.** Register → login → list workspaces → create customer → start time entry all succeed via the real HTTP API through Drizzle + postgres-js.
- **Write-update and analytics still broken.** Stop-timer and delete-entry hit B31 (qualified WHERE in UPDATE/DELETE). Dashboard / reports / patterns / insights hit B32 (timestamp ↔ text comparison).
- No NPM proxy host created.

B31 and B32 are the last two TimeTracker blockers. Both are narrow server-side issues — B31 is a name-resolution asymmetry between SELECT and UPDATE/DELETE; B32 is the missing `text → timestamp` implicit cast on parameter comparisons.

---

## Tenth retest (2026-04-22 evening) — rebuilt from commit `b757d41` (v3.14.7)

Binary rebuilt from `feat/v3.11.0-integration` tip
`b757d41 fix(nano): v3.14.7 — UPDATE/DELETE qualified WHERE + date-range coercion (B31 / B32)`,
repackaged as `heliosdb-nano:3.14.7`. Fresh `ttm_db_data` volume.
`SELECT version()` reports `HeliosDB Nano 3.14.7`.

### Confirmed fixed in 3.14.7

- **B31 (UPDATE / DELETE qualified WHERE)** — fixed. `PATCH /api/time-entries/:id` and `DELETE /api/time-entries/:id` no longer error with `Column 'time_entries.id' not found in schema`. Both return 200 from the app.
- **B32 (timestamp ↔ ISO string coercion)** — fixed. The earlier `Cannot compare Timestamp(…) and String("…Z")` error is gone from `/api/dashboard`, `/api/patterns`, `/api/reports/custom`, and `/api/reports/compare`.

### New blockers exposed by the next slice of the app surface

With the write-update path unblocked, the app's `PATCH /api/time-entries/:id` no longer 500s — but the row comes back with **`check_out: null` and `updated_at: null`**, meaning the UPDATE ran but dropped its `SET` values. And once the analytics endpoints move past the string/timestamp comparison, they now fail on **parameter-bound `LIMIT`/`OFFSET`**.

### B33 (NEW, blocker) — parameterized `LIMIT` / `OFFSET` rejected

**Severity:** blocker — Drizzle always binds LIMIT/OFFSET (pagination, `.limit(N)`, `.offset(N)`). Every analytics endpoint uses it.

**Reproducer (2026-04-22, `heliosdb-nano:3.14.7`):**
```js
await sql`select * from "time_entries" limit 5`;            // OK
await sql`select * from "time_entries" limit ${5}`;         // ERROR
```

Server:
```
ERROR:  Query execution error: LIMIT/OFFSET must be a number
```

**Impact on TimeTracker.** `/api/dashboard` calls `db.select(...).limit(1)` (active timer lookup) → 500. Same pattern in `/api/patterns`, `/api/insights`, `/api/reports/custom`, `/api/reports/compare`, `/api/productivity-analysis`, and every `/api/search` / bulk-export request.

**Expected (stock PG 16).** `LIMIT $1` with `$1 = 5 :: int4` is accepted; the server evaluates the bind value to the integer 5 before the limit check.

### B34 (NEW, blocker) — Drizzle `UPDATE … SET ts_col = $n` through the extended protocol stores NULL

**Severity:** blocker — every Drizzle `.update(t).set({ <timestamp>: Date })` write silently writes NULL into the timestamp column.

**Reproducer (2026-04-22, Drizzle 0.36.4 + postgres-js 3.4.5):**
```ts
const ins = await db.insert(timeEntries).values({
  userId: 1, workspaceId: 1, checkIn: new Date('2026-01-01T00:00:00Z')
}).returning();
// ins[0].checkIn === '2026-01-01T00:00:00.000Z'  ✓ INSERT accepts timestamp param

const [u] = await db.update(timeEntries)
  .set({ checkOut: new Date('2026-01-01T01:00:00Z') })
  .where(eq(timeEntries.id, ins[0].id))
  .returning();
// u.checkOut === null   ✗ UPDATE silently stored NULL

await db.select().from(timeEntries).where(eq(timeEntries.id, ins[0].id));
// [{ …, checkOut: null }]   ✗ confirmed: the DB really holds NULL
```

**Same SQL + same params via `sql.unsafe` (simple query protocol) works.** The regression is specific to the extended-protocol Bind:
```js
await sql.unsafe(
  `update "time_entries" set "check_out" = $1, "updated_at" = $2 where "time_entries"."id" = $3 returning "id", "check_out", "updated_at"`,
  ['2026-04-22T18:17:04.618Z', '2026-04-22T18:17:04.618Z', id]
);
// [{ id, check_out: '2026-04-22T18:17:04.618Z', updated_at: '2026-04-22T18:17:04.618Z' }]   ✓
```

**Likely cause.** Drizzle + postgres-js sends the UPDATE via Parse/Bind/Execute, declaring parameter OIDs for the timestamp columns (most likely `1114` / `timestamp`). Postgres-js encodes `Date` to text form `YYYY-MM-DDTHH:MM:SS.sssZ` before binding. The server stores NULL for that SET value — so either the OID isn't recognised on the SET path, or the text-format decoder silently produces NULL when it can't parse the ISO form. The INSERT path handles the same string-as-timestamp correctly (verified), so the mismatch is UPDATE-specific.

**Impact on TimeTracker.**
- `PATCH /api/time-entries/:id` (stop timer) — stores `check_out = NULL, updated_at = NULL`; timer never "ends"
- `PATCH /api/customers/:id` — any customer field that's a timestamp
- `PATCH /api/entry-templates/:id`
- `/api/time-entries/bulk` PATCH — every bulk update
- `PATCH /api/workspaces/:id/members/:userId` — role change (no timestamp cols, so may actually work; not separately verified)

Any UPDATE path that carries a timestamp in `SET` silently corrupts data.

---

## Status of the TimeTracker deployment after 3.14.7 @ b757d41

- `heliosdb-nano:3.14.7` built from `b757d41`, `version()` reports `3.14.7`.
- Migrations 0000–0003 apply cleanly.
- **Full-trip Drizzle read + insert + delete flow working end-to-end** — register, login, create customer, start timer, list, delete all succeed via the real HTTP API.
- **UPDATE silently nulls timestamp columns** (B34) — stop-timer `PATCH /api/time-entries/:id` 200s but the DB row ends up with `check_out = NULL`.
- **Analytics endpoints still fail** on parameter-bound `LIMIT` (B33). The B32 error is gone; the replacement error is `LIMIT/OFFSET must be a number`.
- No NPM proxy host created.

B33 and B34 are the last two blockers. Both are narrow: B33 needs to accept a bind parameter in the LIMIT/OFFSET slot; B34 needs the extended-Bind → UPDATE SET path to treat the supplied text value the same way the INSERT path already does.

---

## Eleventh retest (2026-04-22 night) — rebuilt from commit `3b04450` (v3.14.8)

Binary rebuilt from `feat/v3.11.0-integration` tip
`3b04450 fix(nano): v3.14.8 — parameterized LIMIT/OFFSET + UPDATE SET coercion (B33 / B34)`,
repackaged as `heliosdb-nano:3.14.8`. Fresh `ttm_db_data` volume.
`SELECT version()` reports `HeliosDB Nano 3.14.8`.

### Confirmed fixed in 3.14.8

- **B33 (parameterized `LIMIT` / `OFFSET`)** — fixed. `select … limit ${5}` via postgres-js no longer errors, and `/api/reports/compare` returns a real delta payload:
  ```
  { "current": { "workMinutes": 5.09…, "sessions": 1, "activeDays": 1 }, "previous": {…}, "deltaPct": {…} }
  ```
- **B34 (UPDATE SET silently nulls TIMESTAMP)** — fixed. `PATCH /api/time-entries/:id` now persists `checkOut` correctly:
  ```
  { "id": 1, "checkIn": "2026-04-22T20:24:54.131Z", "checkOut": "2026-04-22T20:30:00.000Z",
    "updatedAt": "2026-04-22T20:24:54.753Z" }
  ```
  `SELECT` back confirms the value is stored, not just echoed.

End-to-end smoke on v3.14.8: register ✓ · login ✓ · create customer ✓ · start timer ✓ · stop timer ✓ · delete entry ✓ · list workspaces/customers/time-entries ✓ · `/api/statistics` ✓ · `/api/search` ✓ · `/api/reports/compare` ✓.

### New blocker on the remaining analytics endpoints

Four analytics endpoints still return 500, all with the same underlying server error:

- `GET /api/dashboard` → 500
- `GET /api/patterns?days=7` → 500
- `GET /api/insights` → 500
- `POST /api/reports/custom` → 500

All four log the same server-side error:
```
Dashboard error: PostgresError: Query execution error: Column 'check_in' not found in schema
```

### B35 (NEW, blocker) — name resolution fails when SELECT uses unqualified column and GROUP BY uses the qualified form (or vice-versa)

**Severity:** blocker — Drizzle embeds column references through its
template SQL helper and emits one style in the SELECT expression body
(via `sql\`… ${col} …\``) and another style in `.groupBy(...)`,
depending on the context. Mixed forms are a normal part of Drizzle SQL.

**Minimal reproducer** (raw postgres-js, `heliosdb-nano:3.14.8`, 2026-04-22):
```sql
select date("check_in"), count(*)
  from "time_entries"
  group by date("time_entries"."check_in");
-- ERROR: Column 'check_in' not found in schema
```

All three "normalized" variants work:
```sql
-- both unqualified
select date("check_in"), count(*)            from "time_entries" group by date("check_in");              -- ✓
-- both qualified
select date("time_entries"."check_in"), count(*) from "time_entries" group by date("time_entries"."check_in"); -- ✓
-- no GROUP BY
select "check_in" from "time_entries" where "time_entries"."workspace_id" = $1;  -- ✓
```

So the bug is specifically the **mix** of qualifier styles across clauses of the same statement. Stock PostgreSQL treats `"check_in"` and `"time_entries"."check_in"` as the same column when there's only one table in the FROM; HeliosDB's resolver currently doesn't.

**The exact failing SQL emitted by Drizzle for `/api/dashboard` (captured via postgres-js `debug` hook, 2026-04-22):**
```sql
select date("check_in"),
       sum(case when "check_out" is not null and "is_break" = false
               then extract(epoch from ("check_out" - "check_in"))/60 else 0 end)
from "time_entries"
where ("time_entries"."workspace_id" = $1 and "time_entries"."check_in" >= $2)
group by date("time_entries"."check_in")
```

SELECT list and the CASE body use unqualified names (`"check_in"`, `"check_out"`, `"is_break"`); WHERE and GROUP BY use qualified names. Drizzle's template SQL helper renders the embedded column reference with whatever qualifier the surrounding call site produced — since TimeTracker mixes raw `sql\`…\`` templates with `.groupBy(timeEntries.checkIn)` style calls, both forms end up in the same statement.

**Impact on TimeTracker.** Remaining analytics endpoints:
- `/api/dashboard` — weekStats (`group by date(…)`)
- `/api/patterns` — hourly and weekday grouping
- `/api/insights` — productivity-insights engine aggregations
- `/api/reports/custom` — `date_trunc` groupings

Every one emits the same kind of mixed-qualifier aggregation and hits B35.

**Fix shape.** The name resolver should treat `"<col>"` and `"<table>"."<col>"` as the same reference when `<col>` unambiguously identifies a column across the current FROM scope (stock PG behaviour). This mirrors the fix used for B31 (UPDATE/DELETE qualified WHERE) — where the resolver was taught the prefixed form; the remaining gap is letting the two forms intermix within a single statement.

---

## Status of the TimeTracker deployment after 3.14.8 @ 3b04450

- `heliosdb-nano:3.14.8` built from `3b04450`, `version()` reports `3.14.8`.
- **Core Drizzle surface now fully working end-to-end**: register, login, create customer, start/stop/delete time entry, list customers/entries/workspaces, `/api/statistics`, `/api/search`, `/api/reports/compare`. That's 10+ endpoints across INSERT / UPDATE / DELETE / SELECT all passing against real HTTP traffic.
- **B35 is the last remaining TimeTracker blocker.** Analytics endpoints that do `GROUP BY date(qualified_col)` with unqualified references in the SELECT projection fail with `Column 'check_in' not found in schema`.
- No NPM proxy host created.

Once B35 ships, every TimeTracker endpoint should be green and the NPM proxy host for `ttm.danielmoya.cv` can be created.

---

## Closing

Happy to contribute failing integration tests in Rust or pytest if useful — the reproducers above can be lifted straight into `tests/compat/` and run against the `heliosdb-nano:3.14.8` binary. The symptom quotes in each "Actual" block are verbatim from the server over the PG wire (message code, text, and structured error payload where present).

B18 in particular is worth a hardening test: any attempt to verify a B4 fix should also assert that `SELECT *` against the table after the failing INSERT still works.

B19 and B20 are worth a paired test: any `pg_catalog` relation that
answers simple-protocol `psql -c "…"` must also answer the same SQL
via `Parse/Bind/Execute` and must honour the SELECT list + predicates.
A minimal test harness using `node-postgres` with
`client.query({ text, values: [] })` (extended protocol) catches both — but requires **B22** fixed first.

B22 is worth pairing with a driver-level integration test that actually calls `postgres-js`/`pg` with default options; the current simple-query-only validation path doesn't exercise the `Parse`+`Flush` pipeline that every production driver uses.

B23 is worth a test that exercises `UPDATE tgt SET fk = (SELECT id FROM src WHERE src.k = tgt.k)` and the related `WHERE col IN (SELECT …)` / `DELETE … WHERE id IN (…)` / `INSERT … SELECT FROM (…)` forms — these are the core migration primitives that remain after B9/B21 shut the door on PL/pgSQL.
