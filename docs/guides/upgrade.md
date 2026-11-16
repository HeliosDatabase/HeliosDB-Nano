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
Stop the server, replace the binary, and start it again. Upgrading to
4.42.0 from 4.41.x has its own section below: read it first.

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
# → no manual reindex
```

## Upgrading to 4.42.0 from 4.41.x — read before upgrading

4.42.0 is a drop-in upgrade for a standalone server or a primary that was shut down
cleanly. It is **not** a drop-in upgrade for standbys, and a few statements that used to
succeed are now refused. `CHANGELOG.md` has the full list; this is what to plan for.

**Before you start.** Take a `heliosdb-nano dump` of the data directory. A directory that
4.42.0 has written cannot be opened by 4.41.x again.

**Standalone server or primary.**

1. Stop the server cleanly and swap the binary, as above.
2. Start it. Startup no longer replays the logical WAL over committed data: the committed
   rows are authoritative.
3. If the start is refused because the directory has log entries above its checkpoint (a
   4.41.x process that did not shut down cleanly can leave that), stop everything that uses
   the directory, keep your backup, and run the command the message names:

   ```bash
   heliosdb-nano recover-wal --data-dir ./mydata --adopt-local-state
   ```

   It adopts the rows the directory holds. It does not recover writes an older process
   acknowledged and did not store.

**Standbys.** There is no replication between 4.41.x and 4.42.0 in either direction.

1. Upgrade the primary and its standbys in the same maintenance window.
2. Start each standby on a **new, empty data directory**; it copies itself from the
   primary. Its 4.41.x directory is refused, so keep it aside until the new standby is
   verified (`heliosdb-nano standby verify --data-dir …` on the stopped standby).
3. A standby's `--primary-host` names the primary's replication port (default 5433), the
   only replication port now. Remove `--physical-replication-listen`, `--observer-hosts`,
   `observer_hosts` and `HELIOSDB_PRIMARY_PG_PORT` from your configuration: they are gone,
   and an unknown key is an error.
4. Standbys refuse writes (`25006`) instead of forwarding them. Point writers at the
   primary.
5. A node with a replication role archives its WAL by default, to `<data dir>.archive`
   with a 7-day window. Plan the disk space, or set `[replication.archive]`.

See `docs/guides/physical-standby.md` for the standby's operation, promotion and
switchover.

**What applications may notice.**

| Change | What you see |
|---|---|
| Writers wait for a schema statement or `MERGE BRANCH` on their table | A wait of up to `[session] lock_timeout_ms` (default 30 s), then `55P03`; a deadlock is `40P01` |
| A concurrent schema change under a running statement | `40001` for a statement inside a transaction: retry the transaction |
| One statement or transaction writes at most about 1.33 GiB of row data | `54000`: split the load into batches |
| `DROP COLUMN` and `SET STORAGE` rewrite in one write, up to `[storage] max_atomic_rewrite_bytes` (256 MiB) | `54000` above it: raise the limit or rebuild the table |
| A prepared statement whose result columns changed | `0A000` "cached plan must not change result type": prepare again (asyncpg and pgjdbc do) |
| `USE BRANCH` moves only its own session | Connections that relied on another connection's `USE BRANCH` must select the branch themselves |
| A branch created before 4.42.0 | `MERGE BRANCH` refuses it (`55000`) for tables whose rows it changed: recreate it |
| The server hands each statement to a blocking thread | Short range scans cost a little more per statement; `HELIOSDB_NANO_STATEMENT_OFFLOAD=off` restores 4.41 behaviour, long statements stalling the server included |

**Building from source.** The minimum Rust version is 1.88 (1.92 with `--features
code-embed`, which also needs glibc 2.38 or later).

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
# How long a statement waits for a table's writer lock (milliseconds), before
# 55P03. Writers wait for DDL, TRUNCATE and MERGE BRANCH on their table, and those
# wait for the writers already there, including a transaction left open. Readers
# never wait. SET lock_timeout sets it per session (0 means this value, not an
# unbounded wait). Added in 4.42.0. Default: 30000.
lock_timeout_ms = 30000
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
