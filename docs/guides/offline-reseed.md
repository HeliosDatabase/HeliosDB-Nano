# Recovering a snapshot after a primary is recreated

A recreated primary is a new authoritative dataset. Never attach an old standby's
rows or WAL position to it by deleting a marker, resetting an LSN, or simply
restarting streaming. Preserve the old standby until the recovered data has been
checked and the operator has selected which history to keep.

Nano does **not** currently provide an online `resync` / `basebackup` protocol with
a coherent snapshot-to-WAL boundary. The helper below prepares a full **offline
physical snapshot into a new directory**. It does not enable streaming or claim
that reconnecting this snapshot will catch up safely. Issue #7's online recovery
request remains open.

## Prepare an offline snapshot

Use the same Nano binary/version and compatible storage configuration on both
hosts. This procedure supports a local Linux POSIX filesystem and a normal
RocksDB-backed data directory. Network filesystems, externally stored database
files, live filesystem copies, and running embedded processes are outside this
procedure. Preserve any encryption keys separately and securely; the helper does
not reconstruct external configuration or key material.

1. Fence application writes and cleanly stop both the primary and old standby
   through their service manager or exact owned PID. Keep their old data paths.
   A lock can detect an open database; it cannot prove a prior shutdown was clean.
   After a crash, first recover the primary using Nano and then stop it cleanly.
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
It does **not** yet create a certified streaming standby. Do not resume application
writes on the assumption that this helper made HA safe. A physical copy preserves
internal row keys, index data, RocksDB WAL and metadata as one stopped state; a
logical dump is not an interchangeable native replication baseline.

## What blocks an online standby rebuild

At the campaign base `916138311c69f1cde070fb3d53fc601e22cc0a06`:

- The native handshake identifies a node, but has no durable database-history /
  timeline identity tied to the data directory. A newly generated node UUID is
  not a snapshot/history proof.
- `StreamingClient::new` and the WAL applicator initialize applied LSN to zero.
  There is no durable snapshot manifest binding a checkpoint to that receiver.
- Storage-origin broadcasts go through `HAState::broadcast_wal_entry`; that path
  does not durably append to the streaming server's `WalStore` or advance its
  private `current_lsn`. The CLI also constructed this store at `./data/wal`
  independently of `--data-dir`. Fixing its path alone cannot repair history.
- The binary dump interface does not export all physical/index state or an
  atomic native replication checkpoint. Restoring a logical dump and replaying
  WAL from zero can duplicate or misapply operations.

Before offering online resync, the protocol needs a coherent snapshot boundary,
durable history identity, durable applied/flush positions, retained WAL covering
the snapshot-to-current interval, and rejection of a missing interval or unrelated
history. End-to-end tests must cover reconnect, restart, truncation, primary
replacement, transactional visibility, and post-snapshot changes. A healthy
handshake or matching row count before new writes is insufficient.

## Regression checks

```bash
python3 scripts/test_nano_offline_reseed.py
# Heavy Rust gate: use the shared-host build lock and bounded scope.
cargo test --test offline_reseed_tests
```

The Python suite checks refusal and file preservation, including a separate
process holding RocksDB's lock primitive. The Rust suite checks refusal against
an actual open Nano instance and reopening a stopped copy with rows, uniqueness
constraints and independent subsequent writes. Neither suite certifies online
replication continuation.
