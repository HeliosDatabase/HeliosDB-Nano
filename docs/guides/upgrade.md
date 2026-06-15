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

# Swap the binary. Cargo / crates.io is the only active release channel
# today (Homebrew, Docker, npx, and prebuilt binaries are not published yet):
cargo install heliosdb-nano --locked --force
# or, from a source checkout:
#   cargo build --release --locked

# Start with the same data-dir
heliosdb-nano start --data-dir ./mydata
# → WAL replay runs once, no manual reindex
```

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
