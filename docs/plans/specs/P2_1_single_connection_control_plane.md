# Spec P2.1 — one replication connection: NPHYS v2 carries the control plane; logical standby path removed

Parent: `PHYSICAL_STANDBY_REPLICATION.md` §D3.6, §D7, decisions 1, 8, 10. Sprinter `86d44281eabd`. After P1.

Value: P1 still runs two channels per standby (NPHYS for data, native v3 for handshake, history,
heartbeats and ACKs) and keeps the whole logical-standby machinery compiled and tested. The
maintainer decided no backward compatibility is needed, so P2.1 collapses to ONE connection and
deletes the logical standby path. Sync and semi-sync become real: a commit waits for a standby to
have durably applied the physical sequence that covers it.

## 1. Protocol (NPHYS v2, `src/replication/physical_wire.rs`)
- Version string `NPHYS002`; `NPHYS001` peers are refused with a typed error naming both versions.
- Frames added to the existing Auth/Open/Manifest/Chunk/Poll/Batch/Close set:
  - `Hello { node_id, role, history, epoch, cursor: Option<u64>, capabilities: u64, pg_port, sync_mode }`
    sent by the receiver right after `Auth`; the source answers `HelloOk { node_id, history, epoch,
    barrier, capabilities, physical_endpoint, pg_port }` or `Refuse { code, reason }` (see §3).
  - `Heartbeat { applied_sequence, durable_sequence, received_at }` receiver → source every
    `heartbeat_interval` (default 5 s) and whenever `durable_sequence` advances after an apply
    (coalesced, at most every 50 ms); `HeartbeatAck { barrier, now }` source → receiver.
  - `Ack { applied_sequence, durable_sequence }` is the same frame as `Heartbeat` (one type, two
    triggers); keep one struct.
  - `Fence { epoch }` source → receiver and receiver → source (P5 uses it; define now, refuse now).
- Error frames: `Refuse { code, reason }` replaces silently dropping a refused session
  (P1.3 residual: a gap/pruned WAL showed as `reconnecting` forever). Codes: `AuthFailed`,
  `HistoryMismatch`, `Gap { expected, have_from, have_to }`, `WalPruned { oldest }`,
  `EpochStale { have, want }`, `TooManySessions`, `SourceNotServing`, `Version`.
- The receiver turns `HistoryMismatch`, `EpochStale`, `Version`, `WalPruned`, `Gap` into posture
  `needs_recovery { reason }` (stays up per J2); `AuthFailed` likewise after 3 attempts;
  `TooManySessions`/`SourceNotServing`/transport errors → `reconnecting` with backoff.

## 2. Primary side (`physical_source.rs`, `main.rs`)
- `PrimarySession` tracks `StandbyInfo { node_id, history, applied_seq, durable_seq, last_seen,
  pg_port, sync_mode }` in a registry shared with the commit path (replaces `ha_state`'s standby
  list for physical standbys; monotonic `fetch_max` on both sequences so a late heartbeat can
  never regress the watermark — the v3 bug at streaming.rs:718-725 must not be reproduced).
- Commit wait: after the data write (transaction.rs commit path, wal.rs append paths) capture
  `seq = db.latest_sequence_number()` and call `ha_state().wait_for_sequence(seq)`:
  `SemiSync` → `count(durable_seq >= seq && history == ours) >= min_acks`; `Sync` →
  `count(applied_seq >= seq) >= min_acks`; `Async` → return. Timeout per
  `[replication] sync_timeout_ms` (default 10000) with `sync_timeout_action = "warn" | "fail"`
  (default warn; `fail` returns an error for a commit that is already durable locally — document
  it like PostgreSQL's cancelled synchronous commit). New `min_acks` (default 1). Remove
  `wait_replicated(lsn)` and the logical ACK plumbing.
- `/health` on a primary lists its standbys (node_id, applied/durable LSN text, lag, last_seen).

## 3. Removal of the logical standby path (no backward compatibility)
- Delete: the native v3 standby client start (`StreamingClient` as used by `start`), `WalApplicator`
  + forwarder, `replication_wal/` `WalStore` wiring, `HELIOSDB_PRIMARY_PG_PORT` forwarding,
  `logical_wal_catchup` as a standby source (keep the primary's own crash-recovery replay and
  `wal:entries` redo semantics; `logical_wal_retention_entries` becomes the primary's redo window
  only, default 0), the `CatchupSource` plumbing, the handshake refusal text "full resync required"
  (replaced by `Refuse` codes), `tests/replication_catchup_transport.rs` / the logical parts of
  `replication_restart_gap.rs` (rewrite them for physical standbys or delete with a note).
- The native listener (`replication_port`, protocol v3) is removed; the observer role and
  `failover_watcher`/`split_brain` code that only spoke v3 are removed or left unwired behind a
  clear `// P5` note (say which). `[replication] replication_port` becomes the SINGLE replication
  port (NPHYS v2); `[replication.physical] listen` and the "+7" default from P1.3 go away
  (`--physical-replication-listen` is an alias for one release? NO — no compatibility: remove it;
  update the fixture and tests). `--primary-host` names the primary's single replication port.
- Native-channel-only tests are deleted; `ha_integration` is rewritten for the physical pair
  (same scenarios: start order, restart both sides, sync/semi-sync waits, lag reporting).
- `docs/`: `replication-history-guard.md`, `offline-reseed.md`, `deployment-endpoints.md`,
  `ha_testing.md`, the server skill and README's HA section describe the single-port physical
  standby; new `docs/guides/physical-standby.md` (the file P1.3's refusal message points to):
  concepts (Oracle vocabulary table from `STANDBY_RECOVERY_PLAN.md` §1), start, readiness,
  Proxy contract, sync modes, rebuild, limits (loopback until P2.2 lands).

## 4. Tests (proof-first)
- `tests/physical_standby_sync.rs` (real daemons): sync mode, standby applier paused (SIGSTOP the
  standby) → INSERT on the primary blocks until SIGCONT (red: today it returns immediately);
  semi-sync with `min_acks = 2` and one standby → warn path commits with WARN, `fail` path errors;
  15 s of heartbeat-only traffic never regresses the primary's recorded sequences; `/health` on the
  primary lists the standby with increasing applied/durable.
- `tests/physical_standby_refusals.rs`: history mismatch → `Refuse{HistoryMismatch}` →
  `needs_recovery` and the process stays up (`/ready` 503, `heliosdb_require_ready()` 55000 with
  the reason); pruned WAL (small retention) → `WalPruned`; wrong token → `AuthFailed`;
  `NPHYS001` client → `Version`.
- Unit: frame encode/decode round-trips for every new frame; registry monotonicity.
- A source-scan test asserts no reference to the removed symbols remains (`WalApplicator`,
  `StorageCatchupSource`, `FULL_RESYNC_REQUIRED_MESSAGE`, `HELIOSDB_PRIMARY_PG_PORT`).

## Constraints
No cargo/git; read before edit; match style; keep every fail-closed check; the single-port change
must keep the Proxy contract (PG port surface) untouched. Report: files; the frame table; what was
deleted (file list + line counts); the commit-wait call sites; tests red/green; commands.
