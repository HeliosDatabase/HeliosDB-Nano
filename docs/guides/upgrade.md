# Upgrading HeliosDB Nano

This guide covers in-place upgrades between Nano versions. The on-disk
RocksDB layout has been **forward-compatible since v3.6.0** — a binary
upgrade from any 3.x point release to the latest 3.x is a drop-in swap
in the common case. Read the version-specific notes below before
upgrading across a wire-protocol or schema boundary.

## Upgrade matrix at a glance

| From | To | Strategy | Data migration |
|------|----|----------|---------------|
| 3.6.0 → 3.11.x | 3.30.x | Drop-in binary swap | None |
| 3.11.x → 3.19.1+ | 3.30.x | Drop-in binary swap | None — but see "Wire-protocol notes" below |
| 3.19.1 → 3.30.x | 3.30.x | Drop-in binary swap | None |
| Pre-3.6.0 | 3.30.x | Dump-and-restore via `heliosdb-nano dump` | Required (storage layout changed in 3.6.0) |

There is **no required step-wise migration** for any 3.x → 3.x upgrade.
Stop the server, replace the binary, and start it again — recovery is
automatic via WAL replay.

```bash
# Stop the running server (graceful)
heliosdb-nano stop --data-dir ./mydata

# Swap the binary (npx, brew, docker tag, or direct download)
brew upgrade dimensigon/tap/heliosdb-nano   # macOS / Linux
# or
docker pull heliosdb/nano:latest
# or
curl -L https://github.com/Dimensigon/HDB-HeliosDB-Nano/releases/latest/download/heliosdb-nano-$(uname -m)-$(uname -s | tr A-Z a-z).tar.gz | tar xz

# Start with the same data-dir
heliosdb-nano start --data-dir ./mydata
# → WAL replay runs once, no manual reindex
```

## Wire-protocol notes for 3.11.x clients

If your application is pinned at 3.11.x and you are upgrading to
3.19.1 or later (the keyset row-constructor cutoff), a few SQL-side
features become available that the 3.11.x client may not yet use:

| Added in | Feature | Client impact |
|---|---|---|
| v3.12.0 | Row-constructor keyset (`WHERE (col, id) < ($1, $2)`) | Optional — the equivalent `OR`-expanded form continues to work |
| v3.12.0 | Top-K optimisation over `ORDER BY ... LIMIT` | Transparent (planner picks it automatically) |
| v3.23.0 | `JoinPredicatePushdownRule` (JOIN + WHERE composes correctly) | Transparent — fixes a planner bug, no client change needed |
| v3.24.0 | `information_schema` completion | Transparent — DDL-aware tools work that previously errored |
| v3.25.0 | `CREATE DATABASE` / `DROP DATABASE` SQL | New SQL surface; existing apps not affected |
| v3.26.0 | SCRAM-SHA-256 GS2 header parsing (Bug 2) | Re-enables libpq / asyncpg / node-postgres / JDBC SCRAM clients that previously failed handshake |

The drivers themselves do not need to be bumped — these are server-side
changes that improve compatibility with already-conformant clients.

## Pre-3.6.0 storage layout

If you have a data directory written by HeliosDB Nano < 3.6.0, the
storage layout is incompatible with current builds. Dump the data on
the old binary and restore it on the new one:

```bash
# On the old binary
heliosdb-nano-3.5.x dump --data-dir ./old --output ./snapshot.json.zst

# Install the new binary, then restore
heliosdb-nano restore --data-dir ./mydata --input ./snapshot.json.zst
```

Pre-3.6.0 has not been seen in the wild for some time. If you are not
sure which version wrote your data-dir, run
`heliosdb-nano start --data-dir ./mydata` — modern binaries refuse to
open an incompatible directory and print the writer's version. No data
is mutated when the open fails.

## SQLite-import compatibility

The bundled `.sqlite` importer is independent of the on-disk version
and works across all 3.x releases. If a `.sqlite` file imports cleanly
on 3.11.0 it will import cleanly on 3.30.x.

## Rolling-back

The forward-compatible storage layout is **not symmetric** — a
data-dir written by 3.30.x may use record types unknown to 3.11.x.
Take a `heliosdb-nano dump` of the data-dir *before* upgrading if you
need a clean rollback path. Branches (`docs/code_graph/overview.md` →
"Git-Like Branching") are also a low-cost way to rehearse an upgrade:
create a branch, point the upgraded binary at it, and merge back if
the rehearsal succeeds.

## Help with a specific version pair

If your upgrade path isn't covered above (e.g. very old custom build,
internal-fork branch), file an issue with the source version, target
version, and a `heliosdb-nano start --data-dir … --check-only` log
attached. The check-only mode runs WAL replay + catalog read + index
verification without opening the PG / MySQL listeners, so it's safe to
run against production data.
