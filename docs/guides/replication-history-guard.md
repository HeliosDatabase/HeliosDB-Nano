# Replication history guard

A primary and its [physical standby](physical-standby.md) compare a dataset
**history id** before the standby accepts anything, on the one replication
connection (`NPHYS002`). The history id is a UUID of the dataset, independent of
`--node-id`: reusing a node name or UUID does not authorize another dataset.
Both ends must run this release; a 4.41.x native-protocol peer and an `NPHYS001`
peer are refused.

This guard prevents a standby from silently following a different primary
history. It does **not** protect against a deliberate fork made by copying an
entire primary directory, and it is not fencing: a verified history means the
standby follows the right dataset, not that it is caught up. Whether it is
caught up is its posture (`/ready`, `heliosdb_standby_status`).

## The primary's history

For a disk primary, `NANO-PRIMARY-HISTORY.json` is created or validated after
RocksDB has acquired its database lock and before the replication port starts
listening. The record includes RocksDB's storage identity, so recreating the
database while retaining only the history record is refused. Every `HelloOk` the
primary sends carries its history. An in-memory primary serves no replication
port, so it has no standbys to guard.

## The standby's history

A standby learns its history from the copy it installs. While it bootstraps, its
`Hello` names no history, and the verified checkpoint it downloads carries the
primary's history into its physical replica marker (`NANO-PHYSICAL-REPLICA.json`).
From then on every session's `Hello` names that history, and a primary with
another one answers `Refuse { HistoryMismatch }`. The standby then stays up in
posture `needs_recovery`, refusing data reads with SQLSTATE `55000` that names
the mismatch (or exits, with `[replication] on_unrecoverable = "exit"`). It never
serves rows of another history and never re-binds itself to one; adopting a new
history is always a rebuild into a new, empty directory.

A standby's data directory is classified before anything is opened:

- absent, or an existing empty directory: bootstrapped from the primary;
- an installed physical standby: resumed at its durable cursor, under its history;
- a 4.41.x logical standby (it carries `NANO-STANDBY-BINDING.json`): refused with
  the conversion note ("this directory is a logical standby from 4.41.x; physical
  standbys are rebuilt once ..."). See
  [converting a 4.41.x logical standby](physical-standby.md#converting-a-441x-logical-standby);
- a primary's data directory (it carries `NANO-PRIMARY-HISTORY.json`): refused;
- anything else that is not empty: refused.

Corrupt, ambiguous or mismatched identity records are refused. The CLI does not
repair, delete, reset or replace them. Preserve a rejected directory and read the
error. Do not remove marker files to get past a refusal: the marker is what lets
the standby prove which dataset its rows belong to.

A bootstrap copy is verified and renamed into place only when it is complete, so
an interrupted bootstrap never leaves a half-installed standby; the next start
downloads the copy again from the beginning.

## Startup budget

`[replication] startup_timeout_secs` (default 30, range 1 through 3600) bounds
how long a standby's listeners wait for it to stream, including DNS. When it runs
out, the listeners open anyway and the posture decides what they serve: probes
and status answer, data reads are refused with `55000` until the standby is
ready. Explicit `--replication-startup-timeout-secs` overrides the file value;
the file overrides the default. A daemon standby's parent allows this budget plus
its usual 5-second startup allowance, and the resolved value is passed to the
daemon child.

## Standby interface restrictions

A standby never forwards writes, in any sync mode. Connect to the primary to
write. On a standby, writes, DDL and `nextval()` are refused with SQLSTATE
`25006`, on the PostgreSQL simple and extended protocols and the Unix socket
alike; `COPY FROM` is refused.

The standby HTTP listener exposes only `/`, `/health`, `/ready` and `/version`.
Writable REST, auth, realtime and MCP routes are not mounted, even when MCP is
compiled in or explicitly configured. MySQL TCP and Unix-socket options are
rejected before storage opens.

Below the SQL guard, the standby's storage engine is opened in a mode whose write
gate refuses every write the engine would make on its own. The replication
applier is the only writer. A write that reached the store some other way would
poison it: the standby would stop in `needs_recovery` instead of serving a copy
that no longer matches the primary.

## Promotion is refused

The SQL `SWITCHOVER TO '<node>'` statement is **refused, not initiated**. It
performs the same checks it always did (the target must resolve, this node must
be the primary, and the target must be a connected standby) and then returns an
error. No successful switchover is claimed, because none of the required runtime
exists yet. A standby's data directory is also refused as a primary or standalone
database, so restarting a standby with `--replication-role primary` does not
promote it.

A safe promotion needs, at minimum:

- durable fencing **and** write admission of the OLD primary (best-effort
  connection attempts are not fencing);
- a complete durable source boundary: a defined cut of the source WAL the
  promoted node has durably applied;
- an explicit new epoch for the promoted dataset, carried by every session (the
  `Fence` frame and the `EpochStale` refusal are reserved for it today); and
- catalog, auth and session serving publication and routing enabled only after
  the new primary is proven writable.

That is plan phase P5. Until it ships there is no automatic or manual takeover;
do not redirect writes to a standby or treat a standby's directory as a primary.

The [offline snapshot helper](offline-reseed.md) remains an inspection and
recovery procedure. Its output carries no physical replica marker, so a standby
start refuses it; copying or editing identity records does not create a standby.
