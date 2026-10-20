# Spec P1.3 — the physical standby start path, posture, readiness and the PG-wire contract

Parent: `docs/plans/PHYSICAL_STANDBY_REPLICATION.md` §D3.5, §D6 (posture), §D8 (Proxy contract).
Sprinter `db11a1ef3b76`. After P1.1 (applier) and P1.4 (`with_physical_standby`).

Value: this is what the field and Proxy see. `start --replication-role standby` runs the
physical receiver instead of the logical applicator, serves read-only SQL from the applied
store, reports a posture, never exits on a refusal (J2), and answers the exact PG-wire probes
Proxy sends.

## 1. Start path (`src/main.rs`, standby branch of `start_ha_components` ~3318-3475; re-verify)
- Keep `prepare_receiver_history`. Then, instead of the writable `EmbeddedDatabase::with_config`:
  - directory without `NANO-PHYSICAL-REPLICA.json`: **bootstrap** = `run_receiver(ReceiverMode::InstallOnce)`
    (physical_receiver.rs) from `[replication.physical]`/`--primary-host` (today loopback-only;
    remote arrives in P2 — keep the existing checks), into this data dir (fresh leaf rule as today).
    A directory that is a LEGACY LOGICAL standby (`NANO-STANDBY-BINDING.json`, no physical marker)
    is refused with: "this directory is a logical standby from 4.41.x; physical standbys are
    rebuilt once: start with a new, empty data directory (docs/guides/physical-standby.md)".
  - then **follow**: `ReopenedPhysicalStage::open` → `LocalWriter` → `StorageEngine::open_physical_standby`
    → `EmbeddedDatabase::with_physical_standby` (P1.4) → `PhysicalApplier` (P1.1, listener = the
    EmbeddedDatabase) → the receiver worker's `Command::Apply` calls `applier.apply(&batch)`
    (replace the raw `stage.apply`). `run_receiver(Follow)` replaces `WalApplicator` + forwarder
    (~3366-3408). The `StreamingClient` (native v3) stays for handshake/history/heartbeat/ACK only;
    its `entry_rx` is drained and ignored (a v3 primary still sends WalBatch — log once at debug).
- Readiness gate: wait for posture `Streaming` (not the old `CaughtUp`) within
  `startup_timeout_secs`; on timeout **stay up** in the current posture and open listeners
  anyway (listeners always open; data reads are refused by posture, below). Remove the exit on
  failure: the failure `select!` arm (~2394-2422) now only logs and updates posture.
- `on_unrecoverable = "stay-up" | "exit"` (`[replication]`, default stay-up) decides whether a
  terminal receiver status (history mismatch, poison, legacy layout) exits the process.

## 2. Posture (`src/replication/standby_posture.rs`, new)
```rust
pub enum StandbyPosture { Connecting, CatchingUp { cursor: u64, barrier: u64 }, Streaming { cursor: u64 },
    Reconnecting { since: Instant, cursor: u64 }, NeedsRecovery { reason: String, position: u64, since: Instant },
    Recovering { mode: &'static str, progress: String } }
pub struct PostureHandle { rx: watch::Receiver<StandbySnapshot> }   // snapshot = posture + cursor + primary_cursor + lag_secs + primary addr + sync_mode + epoch + last_ack
```
Derived from `ReceiverStatus` (physical_receiver.rs ~127-162) and the connect barrier
(`SourceSessionInfo::Resume{durable_barrier}` ~490-497): `Connecting`→connecting;
Downloading/Applying→catching_up; CaughtUp with `cursor.source_sequence >= barrier`→streaming;
retryable Failed→reconnecting (keeps the last cursor); Gap/HistoryMismatch/Poisoned/legacy→
needs_recovery. `ready()` = Streaming, or Reconnecting with `now - since <= max_standby_lag`
(`[replication] max_standby_lag = "30s"`, duration parser). `lag_secs` = time since the last
applied batch's commit (the primary's wall time is in the batch? if not, time since last
apply) — state which. `primary_cursor` = the barrier from the last poll.

## 3. HTTP (`main.rs` ~3511-3529, the guarded standby router)
- `GET /ready` → 200 if `ready()` else 503; body = the replication block.
- `GET /health` → `{"status":"ok","replication":{role, state, ready, cursor, primary_cursor,
  lag_bytes, lag_secs, primary, sync_mode, last_ack_sequence, epoch}}` with `cursor`/`primary_cursor`
  as PG LSN text (`format_lsn(seq) = format!("{:X}/{:X}", seq >> 32, seq & 0xFFFF_FFFF)`),
  `lag_bytes = primary_cursor - cursor` (sequence delta, documented), `epoch = 1`.
  Same block on a primary with role=primary, state=streaming, ready=true, cursor=`latest_sequence_number`.

## 4. PG-wire surface (Proxy contract §D8) — handler.rs / system functions / startup params
- `SELECT heliosdb_require_ready()` → `t` when primary or `ready()`; otherwise SQLSTATE `55000`
  severity ERROR, message "standby is not ready: <state> (cursor X, primary Y)". Admitted by the
  guard in every posture.
- `pg_is_in_recovery()` → `t` on a standby (every posture), `f` on primary; both forms
  `SELECT pg_is_in_recovery()` and `SELECT pg_is_in_recovery()::text`; column name `pg_is_in_recovery`.
- `pg_current_wal_lsn()` (primary: `latest_sequence_number`; on a standby return the same as the
  replay LSN) and `pg_last_wal_replay_lsn()` (standby cursor; on a primary NULL) as `HI/LO` text;
  accept `::text`. Exactly one row, one column.
- `SELECT timeline_id::bigint FROM pg_control_checkpoint()` → 1 (epoch; P5 makes it real).
  Implement `pg_control_checkpoint()` as a set-returning catalog function with at least
  `timeline_id`; the `FROM` form must parse.
- ParameterStatus `in_hot_standby` = `on` on standby sessions, `off` on primary (exact bytes
  `in_hot_standby\0on\0`); keep `server_version`.
- Guard: `reject_guarded_standby_statement` consults the posture: when not `ready()`, data
  statements (anything that reads user tables) → SQLSTATE `55000`; the readiness/status
  functions, `SET/SHOW`, `BEGIN/COMMIT`, `SELECT 1`, system views remain allowed. Health/topology
  logins (`postgres`/`postgres` trust, app names `heliosdb-proxy-health`, `helios-topology`) are
  accepted in every posture; SSLRequest answered as today; never a startup ErrorResponse for
  readiness.
- View `heliosdb_standby_status` with the same fields as the `/health` block.
- Errors stay severity ERROR for statements. (FATAL 57P01 on fencing is P5.)

## 5. Fixture `scripts/regress/nano-physical-pair.sh`
Starts a primary (PG 27721, physical source on loopback) and a physical standby (PG 27722) from a
built binary (`--binary`), trust auth, both app names accepted; optional `--load N` runs N
single-row inserts/s and prints the measured ops/s; `--kill primary|standby` and `--restart`
helpers; prints a JSON summary {ports, pids, ops_per_s}. Survives SIGKILL/restart of either side
(the standby reconnects and catches up). Used by the live tests below and by Proxy's lab.

## 6. Tests
- `tests/physical_standby_serving.rs` (real daemons, pattern from `tests/replication_restart_gap.rs`):
  primary + standby; writes on the primary; poll `/ready` until 200; `SELECT` on the standby
  equals the primary; `SELECT heliosdb_require_ready()`, `pg_is_in_recovery()`, both LSN
  functions with `::text`, `pg_control_checkpoint()` FROM form, `in_hot_standby` ParameterStatus
  (raw startup frame check) all as specified; `INSERT` → 25006; `nextval` → 25006; SIGKILL the
  primary → standby stays up, `/ready` stays 200 within `max_standby_lag` then 503, reads keep
  being served; restart the primary → back to 200 with every row; SIGKILL the standby → restart
  resumes from the cursor (no full copy; check logs/bytes). A legacy logical standby dir is
  refused with the conversion message. With `on_unrecoverable = "exit"` a history mismatch exits.
- The field-J replay scripts (scratchpad `jlive/field_j*.sh`) adapted to physical standbys are run
  by me, not in the test suite.

## Constraints
No cargo/git; read before edit; keep the MySQL/HTTP-only restrictions of a guarded standby as
today; do not route writes; keep every existing refusal. Report: files; the exact wire outputs
for each contract query (copy from a test run you cannot do — so describe the code path and
cite the test that asserts it); deviations; commands.
