# Upgrading HeliosDB Nano

This guide covers in-place Nano binary upgrades. The current on-disk
RocksDB layout is forward-compatible across supported releases in the
common case. Read the notes below before upgrading across a
wire-protocol or schema boundary.

## Upgrade matrix at a glance

| Source data directory | Strategy | Data migration |
|------|----------|---------------|
| Supported current layout | Drop-in binary swap | None |
| Older incompatible layout | Dump-and-restore via `heliosdb-nano dump` | Required |

There is **no required step-wise migration** across supported releases.
Stop the server, replace the binary, and start it again — recovery is
automatic via WAL replay.

```bash
# Stop the running server (graceful)
heliosdb-nano stop --data-dir ./mydata

# Swap the binary. crates.io (cargo), the prebuilt per-release GitHub binaries,
# and the ghcr.io Docker image are the active channels today (Homebrew and npx
# are not published yet):
cargo install heliosdb-nano --locked --force
# or download the prebuilt archive for your platform from the GitHub release,
# or pull the container image from ghcr.io,
# or, from a source checkout:
#   cargo build --release --locked

# Start with the same data-dir
heliosdb-nano start --data-dir ./mydata
# → WAL replay runs once, no manual reindex
```

## What's new in 3.58.1

3.58.1 is a drop-in patch over 3.58.0 — no storage-format change and no data
migration: stop, swap the binary, start. It lands PostgreSQL-compatibility
fixes, so several statements that errored against 3.58.0 now work after the
upgrade (server-side changes; no client or driver bump needed):

| Statement | 3.58.0 | 3.58.1 |
|---|---|---|
| `DROP TABLE a, b CASCADE` (multi-object) | error "Multiple drops not supported" | drops every named object |
| `CREATE SEQUENCE s START 100 INCREMENT 10` (clauses in any order) | parse error | parses; `nextval` honors `START` / `INCREMENT` |
| `WITH t (a, b) AS (VALUES (1,'x'),(2,'y')) …` | "Unsupported set expression" | plans correctly |
| `CREATE FUNCTION … RETURNS TRIGGER` + `BEFORE INSERT … FOR EACH ROW EXECUTE FUNCTION f()` | function rejected ("Data type not yet supported: Trigger") | runs `NEW.<col> = <expr>; RETURN NEW\|NULL` before the row is written |
| CHECK-constraint violation message | dumped the internal serialized expression | PG-style `new row violates CHECK constraint '<name>' on table '<table>'` |

Nothing here changes existing behavior — they are additive compatibility fixes.
See the `CHANGELOG.md` `[3.58.1]` entry for the full list.

> **Scope note on the trigger row (read before relying on it).** That row is the
> *only* trigger behaviour HeliosDB Nano has. **Trigger BODIES are still not
> executed**: `CREATE TRIGGER` parses, registers and (since 4.20.0) persists, but no
> trigger body is ever run and nothing fires on INSERT/UPDATE/DELETE — silently, with
> no error. The exception above is narrow: `BEFORE INSERT … FOR EACH ROW EXECUTE
> FUNCTION f()` where `f`'s body contains literal `NEW.<col> = <expr>` assignments
> and/or `RETURN NULL` rewrites or skips the row being inserted. It does not extend to
> `BEFORE UPDATE`/`BEFORE DELETE`, to `AFTER` timings, or to side effects such as
> `INSERT INTO audit_log …` inside the body.
>
> **What 4.20.0 changed about that exception:** it now applies identically on both DML
> executor families, so it takes effect over the PostgreSQL *extended* query protocol
> (psycopg with bound params, JDBC, sqlx, Drizzle, node-postgres), over REST, and in
> `INSERT … RETURNING` — through 4.19.0 it applied only on the text family, so a REST
> insert and a `psql` insert into the same table produced different rows. It honours
> the trigger's `WHEN` clause and enabled flag, and it survives a restart.
> `CREATE TRIGGER`/`DROP TRIGGER` also stopped hard-erroring `Operator not yet
> implemented: CreateTrigger` over the extended protocol — **a migration that used to
> fail there will now succeed, creating a trigger whose body does not run.**

## Breaking changes in 4.20.0 — read before upgrading

No storage-format change and no data migration. These are **statement-level**
behaviour changes: things that used to be accepted silently now fail loudly, and
two catalog views changed shape. Full detail in `CHANGELOG.md`.

| Statement | ≤ 4.19.0 | 4.20.0 | What to do |
|---|---|---|---|
| `DROP INDEX x` / `DROP INDEX IF EXISTS x` | planned as `DROP TABLE x` — **dropped the TABLE `x`** if one existed, otherwise "table does not exist" (or, with `IF EXISTS`, silent success) | 4.20.0: errors, *DROP INDEX is not supported yet*. **4.21.0: really drops the index** (a same-named table is never touched; a PK/UNIQUE/FK backing index is refused; `IF EXISTS` silences a missing index again) | **Audit your data** if you ever ran it against a name that was also a table. From 4.21.0 the statement can be used normally |
| `DROP ROLE x` / `DROP ROLE IF EXISTS x` | same fallback — **dropped the TABLE `x`** | real role DDL; can never reach a relation | Nothing, but audit as above |
| `SET ROLE <x>`, `SET SESSION AUTHORIZATION <x>` | acknowledged with **zero effect** — a session that thought it had dropped privileges had not | `0A000 feature_not_supported` (simple query AND extended protocol) | Remove it, or set `[authentication] legacy_acl_noop = true` to restore the ack. `SET ROLE NONE` / `… AUTHORIZATION DEFAULT` are still acked |
| `GRANT` / `REVOKE` naming a role or table that does not exist | silent success, storing nothing | ERROR | Create the role/table first, or set `legacy_acl_noop = true` |
| `SELECT * FROM information_schema.columns` over the PG wire | 7 columns, including the non-standard `is_pk` | 17 columns (matches the embedded route); **`is_pk` is gone with no replacement** | Select columns by name, not by position. For `is_pk`, join `information_schema.key_column_usage` against `table_constraints` where `constraint_type = 'PRIMARY KEY'` — it was a Nano invention that PostgreSQL never had |
| `SELECT * FROM information_schema.tables` | base tables only | base tables **and** views (`table_type = 'VIEW'`) | Filter on `table_type` if you only want base tables; row counts change |
| `CREATE FUNCTION f …` then `SELECT f(x)` | `Unknown scalar function: f` | the function runs | If you caught that error as a feature probe, the probe now succeeds |

New optional config keys (both defaulted; an existing `config.toml` keeps
parsing unchanged):

```toml
[authentication]
# Restore the pre-4.20 silent acceptance of GRANT/REVOKE on unknown names and
# of SET ROLE / SET SESSION AUTHORIZATION. Default: false.
legacy_acl_noop = false

[session]
# Recursion ceiling for user-defined function invocation. Default: 32.
udf_max_call_depth = 32
```

> **Roles and grants are stored and introspectable; privileges are NOT
> enforced.** 4.20.0 makes the catalog tell the truth about what was granted.
> It does not add a permission check to any read or write path. Do not treat a
> successful `GRANT` as a security control.

## Wire-protocol notes

After upgrading, a few SQL-side features may become available that older
clients did not yet use:

| Feature | Client impact |
|---|---|
| Row-constructor keyset (`WHERE (col, id) < ($1, $2)`) | Optional — the equivalent `OR`-expanded form continues to work |
| Top-K optimisation over `ORDER BY ... LIMIT` | Transparent (planner picks it automatically) |
| `JoinPredicatePushdownRule` (JOIN + WHERE composes correctly) | Transparent — no client change needed |
| `information_schema` completion | Transparent — DDL-aware tools work with richer introspection |
| `CREATE DATABASE` / `DROP DATABASE` SQL | SQL surface for tools that provision databases |
| SCRAM-SHA-256 GS2 header parsing | Supports libpq / asyncpg / node-postgres / JDBC SCRAM clients |

The drivers themselves do not need to be bumped — these are server-side
changes that improve compatibility with already-conformant clients.

## Older incompatible storage layout

If you have a data directory written by an incompatible old storage
layout, dump the data on the old binary and restore it on the new one:

```bash
# On the old binary
heliosdb-nano-old dump --data-dir ./old --output ./snapshot.json.zst

# Install the new binary, then restore
heliosdb-nano restore --data-dir ./mydata --input ./snapshot.json.zst
```

If you are not sure which binary wrote your data-dir, run
`heliosdb-nano start --data-dir ./mydata` — current binaries refuse to
open an incompatible directory and print the writer metadata. No data is
mutated when the open fails.

## SQLite-import compatibility

The bundled `.sqlite` importer is independent of the on-disk layout for
supported releases. If a `.sqlite` file imports cleanly on a supported
binary, it should import cleanly after the upgrade.

## Rolling-back

The forward-compatible storage layout is **not symmetric** — a
data-dir written by a newer binary may use record types unknown to an
older binary.
Take a `heliosdb-nano dump` of the data-dir *before* upgrading if you
need a clean rollback path. Branches (`docs/code_graph/overview.md` →
"Git-Like Branching") are also a low-cost way to rehearse an upgrade:
create a branch, point the upgraded binary at it, and merge back if
the rehearsal succeeds.

## Help with a specific version pair

If your upgrade path isn't covered above (e.g. very old custom build,
internal-fork branch), file an issue with the source version, target
version, and the startup log from a `heliosdb-nano start --data-dir …`
run against a **copy** of the data directory. Always work on a copy
(or take a `heliosdb-nano dump` first) — there is no check-only /
dry-run startup mode yet.
