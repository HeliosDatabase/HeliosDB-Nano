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
how long a standby waits for it to stream, including DNS, before it starts
serving. Until then its PostgreSQL TCP port is bound and connections queue in
its backlog, unanswered; its PostgreSQL Unix socket and a cascading standby's
replication port are not open. When the budget runs out they are served anyway
and the posture decides what they serve: probes and status answer, data reads
are refused with `55000` until the standby is ready. The HTTP `/health` and
`/ready` endpoint does not wait: it answers from the start (`/ready` 503 until
the standby is ready). Explicit `--replication-startup-timeout-secs` overrides the file value;
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
The SQL `SWITCHOVER TO '<standby>'` statement, run on the PRIMARY's
PostgreSQL port, swaps the two roles in one command (spec R3b, see
[SWITCHOVER](physical-standby.md#switchover-both-roles-in-one-command)): the
standby is asked over its replication session to promote itself online, it
fences this primary, and this primary is then demoted in-process into its
standby (rewound by fast-forward, never copied). `SWITCHOVER CHECK TO
'<standby>'` runs the preflight only and answers one row per check. Every
refusal before the standby accepted changes nothing (SQLSTATE 55000 naming
the failed check); once it accepted, the old primary is never writable again
— if the switchover cannot finish, it stays FENCED and the error names the
commands that finish it. Through the embedded API (or a prepared statement)
`SWITCHOVER` is refused: it is answered by the server's PostgreSQL session,
which lets go of the store the demotion reopens.

A promotion keeps the dataset's **history id** (it is the same dataset) and
raises its **epoch** by one. The epoch is stored in the dataset itself
(`repl:epoch`), so every copy, standby and archive of the new primary carries
it; every session that holds data compares it (a receiver behind is refused
`EpochStale`, a source behind fences itself and refuses `SourceStale`).

Three files in a data directory's root carry a promotion's state. None is
ever to be removed by hand:

| file | written by | meaning | what refuses the directory |
|---|---|---|---|
| `NANO-PRIMARY-HISTORY.json` | the first start of a primary; `promote` (inherited history) | this directory is a primary of the history it names | a standby start; `recover --from-primary` against the primary promoted over it (same history, higher epoch) REWINDS it in place into that primary's standby (spec R3a: only the leaves that differ travel; the file is removed by the rewind); `--full --replace` rebuilds it by a full copy instead; a source of another history, or at this directory's own or a lower epoch, is refused |
| `NANO-FENCED.json` | a primary when a promoted standby fenced it (`{format, history, epoch, fenced_by_epoch, fenced_at, by_node, final_sequence}`) | this primary was superseded: it refused every write from `fenced_at` on and holds everything through `final_sequence` | every open of the directory is FENCED, in every build and through every entry point (`start` as a primary or standalone, `repl`, MCP, the embedded API): reads are served, every write is refused (SQLSTATE 25006), `/ready` 503, `state = fenced`; a `start` as a STANDBY is refused (exit 2: "this directory is a primary that was fenced at … by epoch E+1 …; it is not a standby's and cannot be started as one; rebuild it as a standby: `heliosdb-nano recover --from-primary --primary-host <new> --replace --data-dir D`"); `recover --from-primary` rewinds it in place into the new primary's standby (spec R3a; nothing it committed through `final_sequence` is lost, and a copy of the record is kept in the rewind's own record); `--replace` matters only when the rewind falls back to a full copy; a record that cannot be read refuses every writable open, naming the file and the rebuild |
| `NANO-PROMOTION.json` | `promote`, before it changes the store | a promotion was interrupted | every open of the directory except `promote`'s own: `start` in every role, `recover`, `resync`, `repl`, the embedded API (exit 2 from the CLI: "promotion interrupted: finish it with `heliosdb-nano promote --data-dir D`"); a re-run of `promote` finishes it (an unreadable record on a store the conversion has not touched yet is discarded with a WARNING and the promotion starts again) |

The three are written whole: a temporary file, fsynced, renamed into place
without ever replacing an existing record, and the directory fsynced. A crash
leaves the record complete or absent, never torn.

Spec R3a adds one record INSIDE the store: a former primary under a rewind
carries, in its node-local family, the rewind's record (its epoch, its own
last sequence, a copy of its fence record) together with the fast-forward's.
While it is there every `start` refuses the directory (exit 2, naming `recover
--from-primary`; once the interim physical marker is written a standby start
stays up in `needs_recovery` naming the same command, like any interrupted
fast-forward). A re-run of `recover --from-primary` finishes it, whichever
step it stopped after; the re-cut removes it and keeps a copy (`rewound`) for
diagnosis.

Spec R3b adds one file BESIDE the data directory: `<data dir>.role.toml`
(mode 0600), written by a switchover on both nodes (`[replication] role` and,
for a standby, `primary_host` and the TLS it follows over) and bound to the
directory it was written for (`[directory]`: the dataset history, the epoch,
the store's `IDENTITY`). `start` applies it over the configuration file (said
at WARN), so a restart starts each node as what the switchover made it; delete
it to go back to the configuration file's role. It refuses the start (exit 2,
naming the file) when it does not read, when the directory is missing, empty
or not the one it was written for (rebuilt, restored, recovered — an old
node's role never turns a new directory into a writable primary of a new
history), and when the command line's `--replication-role` or `--primary-host`
contradicts it. `promote`, `pg_promote` and `recover` rewrite one they find.

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
