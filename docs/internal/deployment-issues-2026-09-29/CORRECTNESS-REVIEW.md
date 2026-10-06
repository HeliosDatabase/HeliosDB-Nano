# Independent correctness and adversarial source review

Reviewed 2026-09-29, approximately 15:33 UTC, against campaign base
`916138311c69f1cde070fb3d53fc601e22cc0a06` and the current uncommitted candidate.
Scope: `src/main.rs`, `src/protocol/postgres/server.rs`, `src/config.rs`,
`scripts/nano-offline-reseed.py`, its Python/Rust regression sources, and the
offline recovery guide. The reviewer authored the separate streaming/transport
changes; those are excluded from this independent verdict and require the other
reviewer's assessment. No build or runtime test was executed during this review.

## Verdict

No unresolved blocker or major correctness finding in the reviewed source after
the fixes below. This permits proceeding to gates; it is not runtime, performance,
merge, release, or online replication recovery acceptance evidence.

## Findings and verified resolutions

1. **Major, resolved: prebound listener bypassed the trust guard.** The draft
   validated only the configured address in constructors. A caller could create a
   loopback Trust server and supply a wildcard listener to `serve_with_listener`.
   The final method checks `listener.local_addr()` using the effective
   `AuthManager::method()` and the default-false explicit override before entering
   the accept loop. Logging also uses the actual listener and effective method.
   `prebound_listener_cannot_bypass_trust_guard` exercises default refusal; the
   constructor regressions cover misleading config/manager combinations,
   loopback allowances, and explicit opt-in.
2. **Major, resolved: daemon TCP readiness raced HA startup.** Prebinding PG before
   replication made the old connection-only readiness probe succeed during WAL
   initialization, even if native replication binding subsequently failed. The
   parent now creates a private temporary directory and waits for this child's
   PID acknowledgment. The child writes the marker with `create_new` only after
   listener initialization. A different process accepting the PG port cannot
   satisfy this check. Child startup exit is checked; timeout and PID-record
   failure kill and reap the exact owned child. The previous malformed IPv6 probe
   is gone. Existing best-effort HTTP bind behavior remains explicit.
3. **Minor, resolved: case-sensitive role checks followed case-insensitive
   validation.** Effective role and sync mode are now normalized before the
   standby requirement and no-HA feature checks.
4. **Major, resolved: offline snapshot enumeration suppressed traversal errors.**
   Python's default `os.walk` behavior could omit an unreadable subtree and
   publish an incomplete copy. Inventory now supplies `onerror=walk_error`, which
   raises. The regression source injects a directory enumeration failure and
   checks refusal without target publication, even when tests run as root.

## Contracts examined

- Replication precedence is explicit CLI field > TOML field > default. Loading
  TOML parses without prematurely applying role requirements; effective values
  are validated after overrides. Explicit empty standby/observer lists clear
  configured lists, and daemon re-exec forwards those resolved values. Unknown
  replication fields fail rather than silently disabling intended settings.
- HTTP literals support IPv4 and bracketed/bare IPv6, socket suffixes, and an
  explicit port override. Explicit `--http-port 0` disables HTTP before parsing
  the supplied listen value. TLS boolean argument parsing accepts the explicit
  values emitted during daemon re-exec.
- Both PostgreSQL constructors enforce the actual authentication mode. The
  override stays default-false, propagates through daemon startup, and emits an
  insecurity warning; the parent also warns because daemon stderr is discarded.
- PG binding and native primary listener binding precede readiness. Bind errors
  include the relevant endpoint and port distinction. HA roles other than a
  primary do not gain a new listener or stronger readiness promise.
- Persistent replication WAL now has a node-private directory below the resolved
  data path. Memory mode retains an owned temporary directory in `HAHandles`.
  Neither change deletes, migrates, or silently reuses the old shared cwd store.
  Neither establishes durable replication history or correct catch-up by itself.
- Offline snapshot scope is a cleanly stopped, local Linux POSIX database with
  no other writers. The helper takes RocksDB's whole-file POSIX record lock,
  preserves that descriptor, and deliberately does not reopen/copy the source
  LOCK inode. Existing destinations, nested paths, symlinks, hard links and
  nonregular entries are rejected. Files are copied into a private staging
  directory, byte hashes checked and files synced; publication uses atomic
  `renameat2(RENAME_NOREPLACE)`. Failed partial copies remain distinguishable.
- The offline guide states that lock acquisition cannot prove a prior clean
  shutdown, external keys/configuration must be preserved, and the result is not
  a certified streaming standby. Source and prior standby state remain intact.

## Required remaining evidence and limits

Execute targeted regressions plus the repository's complete applicable gate
matrix under the shared build lock. Particularly verify real daemon startup and
collision refusal, config-to-runtime precedence, IPv6 listeners, remote-trust
refusal/opt-in, actual Nano lock exclusion, and reopening a stopped physical copy.

Issue #7's online recovery request remains open: the reviewed changes do not add
durable database-history identity, snapshot-to-WAL boundaries, retained catch-up
coverage, or persisted applied LSNs. Existing storage-origin broadcasts still
bypass durable append to the streaming `WalStore` and its private LSN state.
No handshake, copied dataset, or green source review should close that gap.
