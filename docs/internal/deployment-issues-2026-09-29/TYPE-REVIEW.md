# Independent compile/type source review

Reviewed 2026-09-29 candidate changes in `src/main.rs`, PostgreSQL server,
replication streaming/transport, and `tests/replication_listener_startup.rs`.
Reviewer authored `src/config.rs` and deliberately excluded that file from this
independent review. This is source review, not compiler or runtime gate evidence;
no build was run because the baseline build owns the shared heavy-job slot.

## Findings sent to implementation owner

1. **Major: prebound PostgreSQL listener bypasses the constructor address guard.**
   `serve_with_listener` is public and can receive a non-loopback listener even
   when the server was constructed with a loopback `config.address`. The Trust
   guard must inspect `listener.local_addr()` and the actual auth manager method
   before accepting connections. Add a regression for a loopback-configured
   server supplied a wildcard listener, both default refusal and explicit opt-in.
2. **Minor: case normalization must precede exact role checks.** The replication
   validator and existing startup role parsing accept case-insensitive names, but
   new `replication.role == "standby"` / `!= "standalone"` checks operate on the
   original string. Normalize the effective role before those checks, including
   the no-HA feature branch, so uppercase input follows the same validation path.

## Type and ownership checks

- Every in-repository `PgServerConfig` caller uses a builder/default; no external
  exhaustive literal needs the new `allow_insecure_trust` field. `AuthMethod` is
  `Copy`, so passing the effective method does not partially move configuration.
- The sole `HAConfig` construction includes the new field. Start command matching
  supplies all changed `Option` arguments and the new opt-in. Partial field moves
  used by daemon argument construction are legal; no whole-struct borrow follows.
- `SocketAddr` and `Option<SocketAddr>` are `Copy`; resolving `http_port` and
  `http_listen` from the same optional address does not consume needed state.
- `serve_with_listener` takes the same Tokio listener type used by startup.
  Listener ownership passes into the selected server future and closes on error.
- `StreamingServer::start` borrows self consistently, then passes owned listener
  into `start_with_listener`. Spawned startup captures an owned server; integration
  tests capture its `Arc`. Public methods/imports used in the new integration
  suite exist with matching signatures and feature availability.
- New handshake tests use the current `HandshakeRequest` and `HandshakeResponse`
  fields, `WalStoreConfig.wal_dir`, public `close` / `shutdown`, and async slice
  readers compatible with `AsyncRead + Unpin`.

Compiler gates and independent correctness review remain required. Findings are
recorded against the reviewed draft and need a follow-up after owner changes.

## Follow-up after owner fixes

Both findings above are resolved in the updated source: `serve_with_listener`
validates the actual bound address against `auth_manager.method()` before its
accept loop, with a wildcard-listener regression; CLI merging normalizes role
and sync mode before validation and exact checks. No remaining type or ownership
blocker identified in the reviewed scope. Compilation and runtime evidence are
still separate gates. The reviewer subsequently authored the process-level
`deployment_cli_tests` suite; that suite is also excluded from this independent
review and needs review by another agent.
