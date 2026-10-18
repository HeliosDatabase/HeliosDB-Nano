# Native replication history guard

The CLI primary and standby use native protocol version 3 to compare a dataset
history UUID before a receiver accepts WAL. This UUID is independent of
`--node-id`: reusing a node name or UUID does not authorize another dataset.
Legacy native peers are refused. Upgrade both endpoints together.

This guard prevents a receiver from silently attaching to a different primary
history. It does **not** provide online resync, a snapshot boundary, complete WAL
retention, or protection against a deliberate fork made by copying an entire
primary directory. A verified handshake means identity was checked; it does not
mean catch-up completed.

Since 4.41.1 a standby stores its applied position durably, in the same write as
each replicated row, and on reconnect the primary catches it up from its
persisted logical WAL. The standby opens its SQL listener only after it has
caught up. The primary's checkpoint (which advances every 1000 entries or 30
seconds by default, and at a clean shutdown) is only its crash-recovery floor: a
primary also keeps the newest `[replication] logical_wal_retention_entries`
entries below it (default 100000) for standbys. A restarted primary, a standby
outage shorter than that window, and a new standby on a primary whose whole
history still fits the window are all caught up. A standby that needs an entry
older than the window is refused with a terminal "full resync required" error
naming the LSN history was pruned through, rather than served with missing rows.
Each retained entry carries the row images of one change, so size the window for
your write rate and the longest standby outage you want to survive.

For a disk primary, `NANO-PRIMARY-HISTORY.json` is created or validated after
RocksDB has acquired its database lock and before the native listener starts.
The record includes RocksDB's storage identity, so recreating the database while
retaining only the history record is refused. A memory primary gets a fresh
history UUID on each process start.

A disk standby's **final data directory must not already exist on its first
startup**. The CLI claims that leaf before opening RocksDB. Even an existing empty
directory is not proof of a fresh receiver. Existing receivers must have a valid
`NANO-STANDBY-BINDING.json` matching their RocksDB identity. Corrupt, missing,
ambiguous primary/receiver, or mismatched records are refused; the CLI does not
repair, delete, reset or replace them. Preserve rejected directories and inspect
the error. Do not remove marker files to bypass refusal.

A fresh receiver binds durably to its first verified primary before accepting WAL
or announcing readiness. Every reconnect verifies the same binding. A terminal
history/protocol/metadata failure stops the CLI's replication and listener tasks
and returns an error; it does not keep serving as a healthy standby. Initial
verification (including DNS) is bounded by `[replication] startup_timeout_secs`,
default 30, with an allowed range of 1 through 3600 seconds. Explicit
`--replication-startup-timeout-secs` overrides the file value; the file overrides
the default. The same value bounds each native connection/handshake attempt.
Daemon standby startup allows this verification window plus the existing
5-second startup allowance; standalone/primary/observer daemon startup retains
its 5-second readiness deadline. The resolved value is passed to the daemon child.
A failed first startup can leave a new unbound directory; preserve it and use a
new unused path rather than silently adopting it on retry.

## Standby interface restrictions

Transparent PostgreSQL write forwarding is disabled for guarded CLI standbys in
**all** sync modes. Connect directly to the primary for writes. Native protocol
history verification cannot authorize a separate PostgreSQL endpoint, so this
restriction stays set for the process lifetime, including reconnects or local
role/read-only flag changes.

The standby HTTP listener exposes only `/`, `/health`, and `/version`. Writable
REST, auth, realtime and MCP routes are not mounted, even when MCP is compiled
in or explicitly configured. MySQL TCP and Unix-socket options are rejected
before opening storage. PostgreSQL TCP and Unix sockets remain available after
history verification.

These restrictions are **not a complete standby read-only security boundary**.
Existing extended-query, COPY and other local execution paths have not been
certified by this history guard. The bounded claim is rejection of unrelated
native history and disabled transparent forwarding, not universal prevention of
local writes, safe promotion or automatic recovery.

## Promotion is refused

The SQL `SWITCHOVER TO '<node>'` statement is **refused, not initiated**. It
performs the same checks it always did — the target must resolve, this node must
be the primary, and the target must be a connected standby — and then returns an
error. No successful switchover is claimed, because none of the required runtime
exists.

Native catch-up repair is **not** promotion. Raw physical resync remains
**NON-SERVING**, and a retained standby binding still refuses an implicit primary
reopen. A safe promotion needs, at minimum:

- durable fencing **and** write admission of the OLD primary (best-effort
  connection attempts are not fencing);
- a complete durable source boundary — a defined cut of the source WAL the
  promoted node has durably received;
- an explicit new history/epoch for the promoted dataset; and
- catalog, auth and session serving publication and routing enabled only after
  the new primary is proven writable.

None of the in-process helpers meet that bar, and none of them are wired into the
refusal path: `RoleManager` only mutates an in-memory role, `WalApplicator::promote`
only stops application/logging, and the legacy failover path continues after a
failed election and best-effort fencing. They are not a safe promotion
implementation. Do not wire them, remove the existing guards, redirect writes to a
standby, speculate an automatic takeover, or treat raw resync as accepted.

The [offline snapshot helper](offline-reseed.md) remains an inspection/recovery
procedure. Its output is not authorized as a streaming receiver by this guard;
copying or editing identity records does not create a coherent snapshot-to-WAL
boundary.
