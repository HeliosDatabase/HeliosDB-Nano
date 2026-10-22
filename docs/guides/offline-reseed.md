# Recovering a snapshot after a primary is recreated

A recreated primary is a new authoritative dataset with a new history. Never
attach an old standby's rows or position to it by deleting a marker, editing an
identity record, or simply restarting replication: the standby would be refused
with `HistoryMismatch` anyway, and stays up in `needs_recovery` (see the
[history guard](replication-history-guard.md)). Preserve the old standby until the
recovered data has been checked and the operator has selected which history to
keep.

Two different tools apply here:

- **Rebuilding a standby** from a running primary is online: stop the standby,
  keep its old directory aside, and start it with a new, empty data directory. It
  downloads a verified checkpoint of the primary over the replication connection
  and continues from the primary's WAL after it. See
  [refusals and rebuild](physical-standby.md#refusals-and-rebuild).
- **The offline helper below** copies a *stopped* database into a new directory
  for standalone inspection or recovery, for example to keep the old primary's
  history while a recreated primary runs. Its output is not a standby and is not
  attached to any primary.

## Prepare an offline snapshot

Use the same Nano binary/version and compatible storage configuration on both
hosts. This procedure supports a local Linux POSIX filesystem and a normal
RocksDB-backed data directory. Network filesystems, externally stored database
files, live filesystem copies, and running embedded processes are outside this
procedure. Preserve any encryption keys separately and securely; the helper does
not reconstruct external configuration or key material.

1. Fence application writes and cleanly stop the database you want to copy, and
   any standby of it, through their service manager or exact owned PID. Keep their
   old data paths. A lock can detect an open database; it cannot prove a prior
   shutdown was clean. After a crash, first recover the primary using Nano and
   then stop it cleanly.
2. On the primary host, run the repository helper with an unused destination:

   ```bash
   python3 scripts/nano-offline-reseed.py \
     --source /srv/nano-primary \
     --target /srv/nano-snapshot-20260929 \
     --confirm-offline
   ```

   The confirmation acknowledges that all writers remain stopped for this copy;
   it does not bypass the database lock. The helper holds the same whole-file
   POSIX `fcntl` lock on `LOCK` that RocksDB uses. `flock` alone would not exclude
   Nano. It refuses an existing destination (even an empty one), nested paths,
   symlinks, hard-linked files and nonregular entries. It copies to a private
   staging directory, checks SHA-256 bytes against the locked source, syncs files,
   then publishes with atomic no-replace rename. Failed partial copies are left
   under a clearly named `.reseed-*.partial` directory, never at the target path.
3. If transferring to another host, transfer this stopped snapshot in full into
   another unused path over an authenticated channel. Verify the SHA-256 entries
   in `NANO-OFFLINE-SNAPSHOT.json` **before** opening it. Keep directory access
   restricted. The manifest is local provenance, not a cryptographic signature.
4. Inspect the copied database with the same Nano version, in standalone mode
   and isolated from application routing. Validate schema, row counts, important
   data, constraints and indexes. Opening a database can legitimately alter its
   WAL/files, so manifest checks belong before opening it. Keep the primary and
   old standby intact until validation succeeds.

This creates a candidate recovered dataset for controlled standalone operation.
A physical copy preserves internal row keys, index data, RocksDB WAL and metadata
as one stopped state; a logical dump is not an interchangeable replication
baseline.

The helper deliberately does not manufacture a standby. Its output carries the
source's identity records but no physical replica marker, so starting it with
`--replication-role standby` is refused, and editing or copying identity records
does not make it one. To give a recovered dataset a standby, run it as a primary
and rebuild the standby from it online.

## Offline physical receiver

`heliosdb-nano resync` is the replication connection's receiver run on its own:
it installs a verified copy of a primary into an absent directory and, unless
`--once` is given, keeps following it. It never opens SQL, never reports
readiness, and never counts toward a primary's synchronous replication.

```bash
heliosdb-nano resync --primary-host 127.0.0.1:5433 --data-dir /srv/nano-copy \
  --replication-token-file /etc/heliosdb/replication.token --once
```

- `--primary-host` is the primary's **replication port**. A remote primary is
  reached over TLS (`--primary-tls`, the default for a non-loopback address,
  with `--primary-ca` and the other standby TLS flags of
  [the physical standby guide](physical-standby.md#remote-standbys-tls)).
- A fresh copy requires an absent destination; existing data is never
  overwritten. An interrupted copy into the same destination resumes from its
  progress record while the primary still leases the snapshot.
- `--resume-existing` resumes ONLY an already installed physical replica at its
  durable cursor, and requires `--expected-history`, the source's dataset UUID,
  so it can never continue from another dataset.

## Regression checks

```bash
python3 scripts/test_nano_offline_reseed.py
# Heavy Rust gate: use the shared-host build lock and bounded scope.
cargo test --test offline_reseed_tests
```

The Python suite checks refusal and file preservation, including a separate
process holding RocksDB's lock primitive. The Rust suite checks refusal against
an actual open Nano instance and reopening a stopped copy with rows, uniqueness
constraints and independent subsequent writes. Online rebuild and continuation are
covered by the physical standby suites listed in the
[HA testing guide](ha_testing.md).
