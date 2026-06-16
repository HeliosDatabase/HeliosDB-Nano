# Migrating Oracle to HeliosDB with Ora2Pg

Ora2Pg is an Oracle-to-PostgreSQL migration tool. HeliosDB Nano exposes a
PostgreSQL-compatible wire protocol, so the usual workflow is:

1. Use Ora2Pg to inspect Oracle and generate PostgreSQL-compatible SQL.
2. Review the generated SQL for Oracle and PostgreSQL features that need
   HeliosDB-specific handling.
3. Load the reviewed schema and data through `psql` against HeliosDB.
4. Validate object counts, row counts, and application queries before cutover.

This guide is written for HeliosDB Nano, but the same pattern applies to any
HeliosDB edition that accepts PostgreSQL client connections.

## When to use this path

Use Ora2Pg when the source of truth is Oracle and you need a repeatable
schema/data migration into HeliosDB. It is a good fit for:

- Table, primary key, unique key, foreign key, check constraint, sequence, and
  index migration.
- Initial data loads from Oracle into HeliosDB.
- Migration assessment reports that estimate how much manual SQL and PL/SQL
  conversion remains.
- Iterative migrations where you regenerate SQL, review diffs, and rerun a
  clean load in a fresh HeliosDB data directory.

Do not expect a fully automatic migration for large PL/SQL-heavy systems.
Ora2Pg can convert some PL/SQL to PL/pgSQL, but generated functions,
procedures, packages, triggers, FDWs, extensions, and Oracle-specific behavior
must be reviewed before loading.

## Prerequisites

On the migration host:

- Oracle Instant Client or a full Oracle client installation.
- Perl, DBI, and DBD::Oracle.
- Ora2Pg.
- A PostgreSQL client package that provides `psql`.
- `heliosdb-nano` installed and on `PATH`.

Install HeliosDB Nano with Cargo:

```bash
cargo install heliosdb-nano --locked
heliosdb-nano --version
```

Install Ora2Pg from your package manager where available, or from the upstream
source distribution:

```bash
tar xjf ora2pg-x.y.tar.bz2
cd ora2pg-x.y
perl Makefile.PL
make
sudo make install
```

Make sure the Oracle client libraries are visible to Perl:

```bash
export ORACLE_HOME=/opt/oracle/instantclient_21_13
export LD_LIBRARY_PATH="$ORACLE_HOME:${LD_LIBRARY_PATH:-}"
export PATH="$ORACLE_HOME:$PATH"
```

Verify that Ora2Pg starts:

```bash
ora2pg --version
```

## Create a migration workspace

Keep generated SQL and reports outside the HeliosDB data directory:

```bash
mkdir -p oracle_to_heliosdb/{config,reports,schema,data,logs}
cd oracle_to_heliosdb
```

You can either start from the installed sample config:

```bash
cp /etc/ora2pg/ora2pg.conf config/ora2pg.conf
```

or let Ora2Pg generate a project tree:

```bash
ora2pg --project_base "$PWD" --init_project app_migration
```

The generated project includes export and import helper scripts. For HeliosDB,
prefer inspecting the SQL files and loading with explicit `psql` commands until
the migration is repeatable.

## Configure Ora2Pg

Use a minimal first-pass configuration. Avoid committing passwords to the
config file; use environment variables or a secrets manager.

```conf
# config/ora2pg.conf

ORACLE_HOME    /opt/oracle/instantclient_21_13
ORACLE_DSN     dbi:Oracle:host=oracle.example.com;service_name=ORCLPDB1;port=1521
ORACLE_USER    app_owner
# ORACLE_PWD intentionally omitted; set ORA2PG_PASSWD in the shell.

SCHEMA         APP_OWNER
PG_SCHEMA      public

OUTPUT_DIR     ./out
CLIENT_ENCODING UTF8
NLS_LANG       AMERICAN_AMERICA.AL32UTF8

# Conservative defaults for repeatable exports.
DATA_LIMIT     100000
STOP_ON_ERROR  1
CREATE_SCHEMA  0
```

Set credentials for the current shell:

```bash
export ORA2PG_USER=app_owner
export ORA2PG_PASSWD='replace-with-secret'
```

If the Oracle user cannot read DBA catalog views, add:

```conf
USER_GRANTS    1
```

Do not enable `PRESERVE_CASE` unless the application depends on quoted mixed
case identifiers. Lowercase PostgreSQL-style identifiers are easier to operate
and avoid quoting surprises.

## Assess the source database

First prove that Ora2Pg can connect:

```bash
ora2pg -c config/ora2pg.conf -t SHOW_VERSION
ora2pg -c config/ora2pg.conf -t SHOW_SCHEMA
ora2pg -c config/ora2pg.conf -t SHOW_TABLE > reports/tables.txt
ora2pg -c config/ora2pg.conf -t SHOW_COLUMN > reports/columns.txt
```

Generate an assessment report:

```bash
ora2pg -c config/ora2pg.conf \
  -t SHOW_REPORT \
  --estimate_cost \
  --dump_as_html \
  -o reports/assessment.html
```

Review the report before exporting data. Flag these items early:

| Source feature | Migration action |
|---|---|
| PL/SQL packages, procedures, functions, triggers | Convert manually or keep outside the initial load. HeliosDB Nano accepts simple SQL bodies in `DO` blocks, but full PL/pgSQL control flow is not interpreted yet. |
| Oracle synonyms and database links | Replace with application-side routing or explicit target tables/views. |
| Oracle global temporary tables | Revisit lifecycle semantics; do not blindly load `pgtt` extension output. |
| Spatial/PostGIS objects | Confirm the target HeliosDB edition supports the needed spatial surface before loading generated extension SQL. |
| `SYS_GUID()` defaults | Ora2Pg usually maps these to UUID functions. HeliosDB Nano accepts `gen_random_uuid()` and `uuid_generate_v4()`, but review defaults anyway. |
| Oracle empty string equals `NULL` assumptions | Decide whether to normalize data at export time or adjust application logic. |
| Case-sensitive quoted identifiers | Prefer lowercase unquoted identifiers unless the application requires exact Oracle casing. |

## Start a clean HeliosDB target

For a rehearsal, use a fresh data directory:

```bash
heliosdb-nano start \
  --data-dir ./heliosdb-target \
  --auth trust
```

In another shell, create and verify the target database:

```bash
psql -h 127.0.0.1 -p 5432 -U postgres -c "CREATE DATABASE appdb;"
psql -h 127.0.0.1 -p 5432 -U postgres -d appdb -c "SELECT 1;"
```

In HeliosDB Nano, `CREATE DATABASE` registers a **tenant** rather than a
separate PostgreSQL catalog. For a single-application migration you can skip it
and load directly into the default database that `heliosdb-nano start
--data-dir` already opens; create a tenant only when you need multi-tenant
isolation.

For production, use SCRAM-SHA-256 and TLS instead of `trust`:

```bash
heliosdb-nano start \
  --data-dir /var/lib/heliosdb/appdb \
  --auth scram-sha-256 \
  --password "$HELIOSDB_PWD" \
  --tls-cert /etc/heliosdb/tls.crt \
  --tls-key /etc/heliosdb/tls.key
```

Provide the password through `--password` (there is no `--password-file` flag);
source it from an environment variable or secrets manager — e.g.
`export HELIOSDB_PWD=...` — rather than hard-coding it in scripts or shell
history.

## Export and review schema

Export schema objects separately so review and load failures are easy to
isolate:

```bash
ora2pg -c config/ora2pg.conf -t TABLE    -o schema/01_tables.sql
ora2pg -c config/ora2pg.conf -t SEQUENCE -o schema/02_sequences.sql
ora2pg -c config/ora2pg.conf -t VIEW     -o schema/03_views.sql
ora2pg -c config/ora2pg.conf -t MVIEW    -o schema/04_mviews.sql
```

For PL/SQL-derived objects, export to review files first:

```bash
ora2pg -c config/ora2pg.conf -t FUNCTION  -o schema/review_functions.sql
ora2pg -c config/ora2pg.conf -t PROCEDURE -o schema/review_procedures.sql
ora2pg -c config/ora2pg.conf -t TRIGGER   -o schema/review_triggers.sql
ora2pg -c config/ora2pg.conf -t PACKAGE   -o schema/review_packages.sql
```

Review generated SQL before loading:

```bash
rg -n "CREATE EXTENSION|LANGUAGE plpgsql|CREATE TRIGGER|CREATE SERVER|FDW|TABLESPACE|PARTITION|unaccent|pg_trgm|postgis|oracle_fdw|pgtt" schema
```

Common edits:

- Remove PostgreSQL extension DDL that HeliosDB Nano does not provide.
- Replace procedural migration blocks with plain SQL where possible. For
  example, a loop that inserts one row at a time is usually an
  `INSERT ... SELECT`.
- Move trigger/function/package code into an application-service backlog if it
  cannot be represented as plain SQL.
- Recheck multi-column indexes. HeliosDB Nano supports true multi-column
  (composite) B-tree/ART indexes — every key column participates, not just the
  leading one. Still run targeted query tests for critical composite-index
  workloads to confirm the planner picks the index you expect.

Load reviewed schema:

```bash
PSQL="psql -h 127.0.0.1 -p 5432 -U postgres -d appdb -v ON_ERROR_STOP=1"

$PSQL -f schema/01_tables.sql
$PSQL -f schema/02_sequences.sql
$PSQL -f schema/03_views.sql
$PSQL -f schema/04_mviews.sql
```

Do not load generated functions, procedures, packages, and triggers until each
file has been reviewed and tested.

## Export and load data

Ora2Pg can export data as `COPY` or `INSERT`. As of HeliosDB Nano **v3.58**,
the PostgreSQL-wire `COPY ... FROM STDIN` sub-protocol is supported in text and
CSV formats — the same text format Ora2Pg's default `-t COPY` export emits — so
`COPY` is a first-class fast path. Binary `COPY` (`WITH (FORMAT binary)`) is not
yet supported, and on v3.57 or earlier `COPY FROM STDIN` is unavailable. Use
`INSERT` for the first rehearsal regardless, because it is the easiest path to
debug; switch to `COPY` for speed once a small smoke test passes.

Conservative first pass:

```bash
ora2pg -c config/ora2pg.conf -t INSERT -o data/20_data_insert.sql
$PSQL -f data/20_data_insert.sql
```

If the generated data file is too large, export one file per table:

```conf
# config/ora2pg.conf
FILE_PER_TABLE 1
```

Then rerun:

```bash
ora2pg -c config/ora2pg.conf -t INSERT -o data.sql -b data
```

Load tables in dependency order. If foreign keys block loading, use one of
these patterns:

- Export/load tables first, then add foreign keys after the data is present.
- Temporarily mark foreign keys `NOT ENFORCED`, load data, validate, then
  switch them to `ENFORCED`.
- Split parent and child table loads manually.

For a `COPY` smoke test, load a small Ora2Pg-generated `COPY` file into a
throwaway HeliosDB database to confirm the text format round-trips on your
build:

```bash
ora2pg -c config/ora2pg.conf -t COPY -a SMALL_TABLE -o data/small_copy.sql
$PSQL -f data/small_copy.sql
```

On v3.58+ this loads through the native `COPY FROM STDIN` path; on older builds
without it, fall back to `-t INSERT`.

## Direct Ora2Pg import

Ora2Pg can import data directly into a PostgreSQL-compatible target when
`PG_DSN`, `PG_USER`, and `PG_PWD` are set. This is useful after the file-based
workflow is already proven.

```conf
PG_DSN   dbi:Pg:dbname=appdb;host=127.0.0.1;port=5432
PG_USER  postgres
# PG_PWD omitted; provide it securely if auth is enabled.
```

Then:

```bash
ora2pg -c config/ora2pg.conf -t INSERT
```

Use direct import only for `INSERT` or `COPY` data exports. Keep schema loading
file-based so DDL review remains explicit.

## Reset sequences

After loading data, ensure sequences are ahead of imported primary keys:

```bash
ora2pg -c config/ora2pg.conf -t SEQUENCE_VALUES -o schema/90_sequence_values.sql
$PSQL -f schema/90_sequence_values.sql
```

Spot-check high-write tables:

```sql
SELECT max(id) FROM orders;
SELECT nextval('orders_id_seq');
```

If `nextval` is not greater than the current maximum, run a target-specific
`setval` statement.

## Validate the migration

Run structural and row-count checks with Ora2Pg:

```bash
ora2pg -c config/ora2pg.conf -t TEST > reports/migration_diff.txt
ora2pg -c config/ora2pg.conf -t TEST_COUNT > reports/row_counts.txt
```

For row-level validation, compare a bounded sample first:

```conf
DATA_VALIDATION_ROWS 10000
DATA_VALIDATION_ERROR 10
```

Then:

```bash
ora2pg -c config/ora2pg.conf -t TEST_DATA > reports/data_validation.txt
```

For smaller tables, set `DATA_VALIDATION_ROWS 0` to compare every row.
Ora2Pg row-level validation requires a primary key or unique index and should
be run before application writes modify the target.

Add HeliosDB-side validation queries:

```sql
-- Object inventory
SELECT table_schema, table_name
FROM information_schema.tables
WHERE table_schema = 'public'
ORDER BY table_name;

-- Row counts for critical tables
SELECT COUNT(*) FROM customers;
SELECT COUNT(*) FROM orders;
SELECT COUNT(*) FROM order_items;

-- Constraint smoke tests
INSERT INTO order_items (order_id, product_id, qty)
VALUES (-1, -1, 1);
-- Expect a foreign key or application-level validation error.
```

Also run the application's highest-value read and write paths:

- Login/session creation.
- Top N dashboard queries.
- Inserts/updates/deletes for the busiest tables.
- Reports that depend on date math, null semantics, or joins.
- Any code path formerly backed by Oracle packages, triggers, or procedures.

## Cutover checklist

1. Freeze Oracle writes or capture a final SCN for the export.
2. Export and load the final delta or rerun the full load into a fresh
   HeliosDB data directory.
3. Run `TEST`, `TEST_COUNT`, and application smoke tests.
4. Point one read-only application instance at HeliosDB.
5. Promote HeliosDB to write traffic.
6. Keep Oracle read-only until rollback is no longer required.
7. Take a HeliosDB dump immediately after cutover:

```bash
heliosdb-nano dump --data-dir /var/lib/heliosdb/appdb \
  --output /backups/appdb-post-cutover.hdb
```

## Troubleshooting

| Symptom | Action |
|---|---|
| `SHOW_VERSION` cannot connect | Recheck `ORACLE_HOME`, `LD_LIBRARY_PATH`, `ORACLE_DSN`, firewall rules, and Oracle service name vs SID. |
| Ora2Pg exports almost empty SQL | The Oracle user may lack catalog permissions. Use a more privileged user or set `USER_GRANTS 1`. |
| Load fails on `CREATE EXTENSION` | Remove or replace that extension DDL unless the target HeliosDB edition explicitly supports it. |
| Load fails on PL/pgSQL control flow | Rewrite the block as plain SQL, or keep that behavior in application code until the procedural surface is supported. |
| Data load is slow | Batch larger files, use one transaction per table, consider `COPY` only after a smoke test, and defer foreign keys/index-heavy checks until after load. |
| Row counts match but application behavior differs | Audit Oracle-specific semantics: empty string vs `NULL`, date/time functions, implicit casts, case-sensitive identifiers, triggers, packages, and sequence defaults. |
| Validation sort mismatches | Ora2Pg row validation sorts by primary/unique key. Use stable keys and watch for collation differences on text keys. |

## References

- Ora2Pg introduction: https://ora2pg.darold.net/docs
- Ora2Pg installation: https://ora2pg.darold.net/docs/installation
- Ora2Pg configuration: https://ora2pg.darold.net/docs/configuration
- Ora2Pg Oracle connection settings:
  https://ora2pg.darold.net/docs/configuration/oracle-connection
- Ora2Pg PostgreSQL import settings:
  https://ora2pg.darold.net/docs/configuration/postgresql-import
- Ora2Pg migration testing:
  https://ora2pg.darold.net/docs/configuration/test-the-migration
- Ora2Pg data validation:
  https://ora2pg.darold.net/docs/configuration/data-validation
- HeliosDB Nano PL/pgSQL compatibility: ../compatibility/plpgsql.md
- HeliosDB Nano authentication: authentication.md
