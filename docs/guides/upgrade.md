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
