# Replication history guard

A primary and its [physical standby](physical-standby.md) compare a dataset
**history id** before the standby accepts anything, on the one replication
connection (`NPHYS004`). The history id is a UUID of the dataset, independent of
`--node-id`: reusing a node name or UUID does not authorize another dataset.
Both ends must run this release; a 4.41.x native-protocol peer and an `NPHYS001`
or `NPHYS002` peer are refused.

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

## Promotion: the epoch, the fence and the records

A standby becomes a primary only through a promotion asked of it: `SELECT
pg_promote(true, N)` on the RUNNING standby, or `heliosdb-nano promote` on its
STOPPED directory (see
[Promotion and the epoch](physical-standby.md#promotion-and-the-epoch)). Both
convert the directory the same way, under the same record. A standby's data
directory is otherwise refused as a primary or standalone database, so
restarting a standby with `--replication-role primary` does not promote it.
The SQL `SWITCHOVER TO '<node>'` statement on the primary is still **refused,
not initiated**: it performs the same checks it always did (the target must
resolve, this node must be the primary, the target must be a connected
standby) and returns an error naming both forms on the target; `SWITCHOVER
CHECK` lists the same as its blocker.

A promotion keeps the dataset's **history id** (it is the same dataset) and
raises its **epoch** by one. The epoch is stored in the dataset itself
(`repl:epoch`), so every copy, standby and archive of the new primary carries
it; every session that holds data compares it (a receiver behind is refused
`EpochStale`, a source behind fences itself and refuses `SourceStale`).

Three files in a data directory's root carry a promotion's state. None is
ever to be removed by hand:

| file | written by | meaning | what refuses the directory |
|---|---|---|---|
| `NANO-PRIMARY-HISTORY.json` | the first start of a primary; `promote` (inherited history) | this directory is a primary of the history it names | a standby start; `recover --from-primary` rebuilds it only with `--replace`, by a full copy of a primary of the same history |
| `NANO-FENCED.json` | a primary when a promoted standby fenced it (`{format, history, epoch, fenced_by_epoch, fenced_at, by_node, final_sequence}`) | this primary was superseded: it refused every write from `fenced_at` on and holds everything through `final_sequence` | every open of the directory is FENCED, in every build and through every entry point (`start` as a primary or standalone, `repl`, MCP, the embedded API): reads are served, every write is refused (SQLSTATE 25006), `/ready` 503, `state = fenced`; a `start` as a STANDBY is refused (exit 2: "this directory is a primary that was fenced at … by epoch E+1 …; it is not a standby's and cannot be started as one; rebuild it as a standby: `heliosdb-nano recover --from-primary --primary-host <new> --replace --data-dir D`"); `recover --from-primary --replace` rebuilds it (without `--replace`: exit 3); a record that cannot be read refuses every writable open, naming the file and the rebuild |
| `NANO-PROMOTION.json` | `promote`, before it changes the store | a promotion was interrupted | every open of the directory except `promote`'s own: `start` in every role, `recover`, `resync`, `repl`, the embedded API (exit 2 from the CLI: "promotion interrupted: finish it with `heliosdb-nano promote --data-dir D`"); a re-run of `promote` finishes it (an unreadable record on a store the conversion has not touched yet is discarded with a WARNING and the promotion starts again) |

The three are written whole: a temporary file, fsynced, renamed into place
without ever replacing an existing record, and the directory fsynced. A crash
leaves the record complete or absent, never torn.

The fence is graceful when the old primary is reachable: it is sent on the old
primary's own replication port, authenticated like every session, only after
the promoting standby proved the old primary serves its cursor (a resume
session, verified by its last write group, and its first poll), and only by a
standby of the same dataset that holds data, for the epoch after its own. The
promotion does not proceed until the old primary confirmed it refuses writes and
made everything durable. When the old primary is unreachable, `promote` stops —
unless `--force`, which is the operator's attestation that it is down or
isolated (fencing it then, STONITH, is the operator's job); a primary that
answers is fenced even with `--force`. An old primary that comes back unfenced
fences itself the moment a node of the new epoch reaches it. There is no
automatic failover.

The [offline snapshot helper](offline-reseed.md) remains an inspection and
recovery procedure. Its output carries no physical replica marker, so a standby
start refuses it; copying or editing identity records does not create a standby.
