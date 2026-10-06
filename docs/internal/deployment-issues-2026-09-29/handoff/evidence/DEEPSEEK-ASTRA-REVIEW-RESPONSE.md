# DeepSeek response to Astra Nano review — 2026-09-30

- Author: `deepseek/deepseek-flash` (opencode), session `ses_f109d4f50ffe3UXlZpZ5C3Rg7y`. Nano only.
- This is an **implementation + evidence statement, NOT acceptance**. Acceptance is Astra / authorized Opus.
- Input packet left immutable: `ASTRA-NANO-REVIEW-20260930.md` (unchanged). Source snapshot it referenced:
  `ASTRA-NANO-REVIEW-20260930-source.json` (utc 06:25:07Z); the reviewed hashes are superseded below.
- No Proxy edits. No commit/merge/push/release. Frozen originals untouched. Heavy work under
  `flock build.lock` + systemd 24G/no-swap/jobs2. State labels: implemented / gate-passed / submitted-for-Astra-review.

## Candidate and hashes (superseding the reviewed snapshot)

Tree `/home/gpc/HDB/worktrees/nano-native-auth-20260930`; manifest `native-auth-source.sha256` (853 files,
`sha256sum -c` OK). Changed-file sha256:

| file | sha256 |
| --- | --- |
| src/replication/transport.rs | `78610034ecac52703369e0467368ee0d087caee5a5f7c71a15427d3e4494de32` |
| src/replication/streaming.rs | `e73934d94dc908e30d5f627ccb9748fcaf556078688c8fb43d5ffc3611d4b440` |
| src/config.rs | `a14391483ca9540fa38cc84e5790dfa944a1dfac537d6ba2168d63181a2d62f4` |
| src/main.rs | `fb4f29fedf91ad268b97dc04b309b296ae304f748b009aa700624ed22d8bf19d` |
| tests/native_replication_auth.rs | `4216102db8cd978b94162ea2b0746cd074249fdf90965cba411f50cec16512e1` |

Bounded re-gate `native-auth-astra` **exit=0** (2026-09-30T14:44:26Z):
`--lib` **2754 passed / 0 failed / 2 ignored**; `--test replication_listener_startup` 3/0;
`--test native_replication_auth` **5/0**. Source before/after matched.

## Native logical authentication — blocker disposition

1. **Empty credentials must be rejected, never downgraded — IMPLEMENTED + gate-passed.**
   - `ReplicationConfig::validate` now errors on `Some("")` (`replication.auth_token must not be empty`).
   - `main.rs` no longer `.filter(|t| !t.is_empty())`; an env `HELIOSDB_REPLICATION_AUTH_TOKEN` (even empty)
     replaces the config value and is validated before listeners open.
   - Tests: `replication_config_rejects_empty_auth_token` (None ok / empty err / non-empty ok) passes.
   - CLI-level env-empty precedence now gate-passed: `native_auth_live.py` sets
     `HELIOSDB_REPLICATION_AUTH_TOKEN=""`, spawns a primary, and asserts it **exits non-zero with an
     `auth_token`/empty diagnostic and never prints `Server ready!`** (no downgrade).

2. **Real wire-version contract — IMPLEMENTED + gate-passed (test for one direction).**
   - `PROTOCOL_VERSION` bumped **1 → 2** with rationale (adding `HandshakeRequest.auth` broke bincode layout).
   - Server `connection_loop` refuses a mismatched header version with `accepted:false`,
     error `native protocol version mismatch: server vX, peer vY`, **before decode/registration**.
   - Client `handshake_client` refuses a mismatched response version.
    - Test `old_protocol_version_is_refused_before_registration` (raw frame with `header.version = 1`) passes
      and asserts no registration.
    - Raw **NEW-client-vs-OLD-server** control `new_client_refuses_an_old_version_server` now passes: a raw
      server that returns a v1-versioned `HandshakeResponse` is refused by the new client with
      `version mismatch`.
    - NOT DONE: a combined history-guard-v2 acceptance claim. Current base is the original replay candidate
      WITHOUT guard/admission fixes → **no combined acceptance**.

3. **Plaintext bearer → restricted transport contract — PARTLY IMPLEMENTED.**
   - Handwritten XOR replaced with `ring::constant_time::verify_slices_are_equal` when `ring-crypto` is
     available; the no-crypto fallback is documented as NOT constant-time (protected-transport only).
   - `ReplicationAuth` Debug redacts the token; `ReplicationConfig` has a manual Debug that redacts
     `auth_token`; test `replication_config_debug_redacts_auth_token` passes. Token is passed via env, never
     argv; rejected-handshake logs print no token.
   - Loopback enforcement: with a configured token the primary refuses a non-loopback replication listener
     unless `HELIOSDB_REPLICATION_AUTH_ALLOW_REMOTE=1`; contract documented in `config.rs`.
   - NOT DONE: primary-to-standby proof, TLS, or any bespoke crypto (intentionally not invented).

4. **Begin-WAL (not just registration) — IMPLEMENTED + gate-passed.**
   - Rust control `authenticated_connection_receives_broadcast_wal_entry` now asserts a **real WAL entry
     delivered over the authenticated socket**: after an authenticated registration, a broadcast `WalEntry`
     (lsn 42, data `wal-socket-control`) is decoded from the standby's connection and matched on LSN and bytes.
   - `reconnect_with_the_same_credential_is_accepted` and `malformed_handshake_frame_is_refused_without_registration`
     (socket closed, zero registrations) added.
   - End-to-end source-write → native WAL receipt → replicated row is also demonstrated by the CLI live
     control `native_auth_live.py` on the final binary `8e46aff9…`: valid token → committed row visible on the
     standby; missing/wrong → `authentication required/failed`, standby never connects; **empty env token →
     primary refuses to start (no downgrade)**.

5. **Own fixture lifecycle — IMPLEMENTED + gate-passed.**
   - `stop()` no longer swallows the join timeout: it aborts and awaits on timeout and **panics** on
     unjoined tasks; `wal_store.close()` result is asserted; TempDir drops only after the task is joined.
   - Rejected-handshake tests now assert, over a 500 ms window, that `connected_standby_count` never becomes
     non-zero and that the server **closed the socket** (further `recv` errors), replacing the single
     immediate zero-count poll.

## Admission fixture reassessment — preserved + evidence

- Corrected GH36 draft keeps **AUTOCOMMIT** INSERT/DELETE and enables `logical_wal_per_statement=true`
  (`emitted_logical`), i.e. the appropriate way to observe autocommit logical WAL; the earlier
  explicit-transaction substitution is removed/superseded. Original rename regressions retained and passing.
- Bounded admission r3 evidence packet: `ASTRA-admission-r3-evidence-review.json`.
  `admission-r3`: clean exit0, lib 2770/0/2, coverage true, targeted 9/9 (key tests ok), live
  simple/copy/extended pass; wider default 353/353 and internal 353/353 (completeness pass).
- Remaining: a real HA-primary autocommit live control must be added to the combined candidate; and the
  combined history-guard-v2 candidate is not assembled.

## Physical fix scope

- The 5-file `physical-compile-fix.patch` is as reviewed: public trigger accessor, public catalog re-exports,
  byte-slice/bind lifetime fixes, early `is_dir()` refusal. Bounded evidence only (lib 2825/0/2, targeted 65/0;
  plus a ported GH36 fix) — **not** coherent online serving acceptance.
- Nonmutation blocker **IMPLEMENTED + gate-passed**: added
  `strict_open_on_an_existing_empty_directory_is_refused_without_mutation`
  (`tests/physical_storage_hydration.rs`), which creates an existing empty checkpoint dir, asserts strict open
  errors, and asserts the directory is left with exactly its prior (empty) entries. Gate
  `physical-nonmutation-r8 exit=0`: `physical_storage_hydration` 8/0 and `physical_snapshot_hydration` 8/0,
  including `physical_snapshot_requires_persistent_existing_storage`,
  `malformed_durable_runtime_objects_fail_with_keyed_diagnostics_and_preserve_bytes` (wrong/metadata bytes
  preserved), and `malformed_visibility_and_counter_records_refuse_strict_hydration`. Manifest
  `physical-fix-source.sha256` regenerated (0 FAILED). Submitted for Astra review.

## Raw-resync CLI/inspector fixture — RUN + PASS (bounded, NON-SERVING)

- Inspector `inspect_physical_fixture.rs` (source sha256 `7e4e1e91d1ec2d5c6e51605f43854b7d10ba6e5da28466bac0d4257a02d7551f`)
  compiled as an isolated example in the corrected physical tree under the shared lock; driver
  `run_cli_resync_regression.py` sha256 `a3d92ab3aab9d87a032f9aac6d4b8e4f58c5c486df895c4fb8e45c69bf52d3c6`.
- Run on the exact corrected physical candidate: binary
  `bd936b9119acd847e579684cc0252cfeb4e0d62fd7a21cd328ee3ba18251d87d`, inspector binary `d69bc47a…`.
  Result `cli-resync-regression-result-final/*/result.json` — **status pass**, identity_unchanged.
  Checks: bad-auth nonzero/no-adoption; bad-history nonzero/no-adoption; pre-existing destination refused
  byte-for-byte; snapshot-only seed + later committed UPDATE/INSERT match the closed raw prefix; restart
  Resume preserves history/snapshot, advances cursor, copies committed DELETE/INSERT; normal SQL start
  refuses the physical receiver with no mutation/readiness.
- Scope unchanged: NON-SERVING closed raw row-prefix/history/cursor evidence only; no SQL is executed on the
  receiver and no serving readiness is claimed.

## Clippy findings — reported, NOT waived

`DEEPSEEK-ASTRA-CLIPPY-FINDINGS.txt` lists every NEW `(file, lint)` signature vs the base clippy log, with
locations. **1** for the native-auth candidate (`src/config.rs` `unwrap()` on `Result`) and **27** for the
physical candidate (mostly `checkpoint_transfer.rs`/`physical.rs`/`history.rs` `unwrap()`/`indexing may
panic`), plus the `src/config.rs` unwrap shared with base deltas. These are recorded as outstanding findings
for review; none is waived, excluded, or reclassified as accepted.

## Follow-up 2026-10-01 (bounded)

- **Native-auth live rerun on the final post-review binary**
  `8e46aff9c718d4a6f118a60d702b0ca2061783ad50a7de96f5fd7a35a7b6ea77` (prior `77d6a7e32…` is stale).
  `native-auth-live-result-final3` **status pass**, identity unchanged: empty env credential → primary refuses
  to start; valid token → row replicated; missing/wrong → rejected, standby never connects.
- **Rust controls** — `native-auth-controls exit=0` (2026-10-01T08:36:40Z): `--lib` 2754/0/2 and
  `--test native_replication_auth` **9/0**, covering the authenticated WAL socket receipt, reconnect,
  malformed frame, and raw NEW-client-vs-OLD-server, plus the original valid/wrong/missing/no-auth.
- **Raw-resync** — inspector built from the reviewed source and the driver run to **pass** on the exact
  corrected physical candidate (see "Raw-resync CLI/inspector fixture").
- **Clippy** — `DEEPSEEK-ASTRA-CLIPPY-FINDINGS.txt` (1 native / 27 physical new signatures, locations
  included); not waived.
- Test-file and harness hashes changed since the table above: `tests/native_replication_auth.rs` and
  `native_auth_live.py` were extended; the compiled native-auth **library/binary is unchanged** at `8e46aff9…`.
  Manifest `native-auth-source.sha256` regenerated (source before/after matched in the gate).

## Unchanged non-claims

Online serving/resync (`resync_certified=false`), generation/session wiring, large source certification, and
distributed fenced promotion remain **unaccepted**; promotion requires a separate complete contract and writes
must never be routed to a standby. Sprinter is not closed; no release is claimed. Astra / authorized Opus
remains the only acceptance authority.
