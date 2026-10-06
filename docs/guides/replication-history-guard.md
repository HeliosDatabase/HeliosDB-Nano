# Native replication history guard

The CLI primary and standby use native protocol version 2 to compare a dataset
history UUID before a receiver accepts WAL. This UUID is independent of
`--node-id`: reusing a node name or UUID does not authorize another dataset.
Legacy native peers are refused. Upgrade both endpoints together.

This guard prevents a receiver from silently attaching to a different primary
history. It does **not** provide online resync, a snapshot boundary, durable
receiver LSN continuity, complete WAL retention, or protection against a deliberate
fork made by copying an entire primary directory. A verified handshake means
identity was checked; it does not mean catchup completed.

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

The [offline snapshot helper](offline-reseed.md) remains an inspection/recovery
procedure. Its output is not authorized as a streaming receiver by this guard;
copying or editing identity records does not create a coherent snapshot-to-WAL
boundary.
