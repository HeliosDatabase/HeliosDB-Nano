# Spec P5.2 — `SELECT pg_promote(wait, wait_seconds)`: online promotion of a running standby

Parent: `PHYSICAL_STANDBY_REPLICATION.md` §D8 (Proxy contract), decision 9; spec P5.1
(`P5_1_epoch_promote_fence.md`, the epoch, fencing and the offline `heliosdb-nano promote`).
Sprinter `8f0721af1a70`. Implementation agents never run cargo or git.

Value: P5.1 promotes a STOPPED standby. HeliosDB Proxy's failover and upgrade paths promote by
sending `SELECT pg_promote(true, N)` to a RUNNING standby and, right after the `t`, open a fresh
connection that must already answer `pg_is_in_recovery() = f` (Proxy `failover_controller.rs:544-560`;
`auto_promote.rs:167-196` polls every 500 ms within N). P5.2 makes that work: the standby fences
its primary through its live replication session, closes the receiver, converts the directory with
the P5.1 conversion, reopens the store as a primary in the same process, terminates every session
with FATAL `57P01` so Proxy redials and sees `in_hot_standby=off`, and answers `t`.

## Facts the design rests on (verified on `18085df7`; P5.1 facts on `76504cbd`)

- The standby's serving engine shares the applier's RocksDB handle and is write-guarded at open:
  `OpenMode::PhysicalStandby`, `WriteGate::closed`, no logical WAL, no workers (`main.rs:4757-4801`,
  `engine.rs:1950-1953, 2434-2438, 3061-3374`). It cannot be flipped; the store must be REOPENED.
- Holders of the serving `Arc<EmbeddedDatabase>`: `PgServer` (`main.rs:2644`), the HTTP task
  (`:2747`, captured at start), the serving watch (`:2649-2654`, its sender dropped after a one-shot
  publish, `:4567-4589`), the cascade store watch (`:2689`), every session's `handler.database`
  (`handler.rs:509`) and the statement thread-local engine (`sequences::current_engine()`). RocksDB
  releases its lock only when the last `Arc` drops (`engine.rs:1930-1935`).
- Sessions: one detached `tokio::spawn` per connection, no registry (`server.rs:257-321`); no push
  mechanism exists; `in_hot_standby` is sent once at startup (`handler.rs:1438-1447`); the only
  57P01 is lazy (next statement, P5.1 reuses it for pre-fence sessions).
- The receiver stops through a `watch::Sender<bool>` + supervisor join (`main.rs:4362-4436,
  4609-4631`); the in-flight run completes and the cursor is durable on a group boundary
  (`physical.rs:1361-1376`); `state.close()` fails while the serving engine shares the writer
  (`physical.rs:1743-1747`, accepted today because "it closes with the process").
- Primary-side services are built once at start: `physical_service` (`main.rs:3108-3161`),
  `prepare_physical_source` (`:7247-7375`, now takes `&Arc<StorageEngine>` after P5.1),
  `install_primary`, `ha_state` setters; `disable_forwarding` is sticky by design
  (`ha_state.rs:497-508`). WAL retention options are open-time (`engine.rs:2691-2707`).
- Admin SQL functions dispatch in `sql/evaluator.rs:1108-1135` from process-global state; a
  `SELECT pg_promote(true, 60)` already passes the standby guard's shape rule and is not a data
  read (`handler.rs:7330-7432`); it fails today with 42883 (pinned by
  `tests/promotion_refusal_tests.rs`, P5.1).
- P5.1 pieces reused unchanged (`76504cbd`): `promote::convert` + `promote::identity_files` on a
  CLOSED directory under the `NANO-PROMOTION.json` record (the promotion id is a random UUID kept
  in the node-local family, `promote::classify`); `fence_primary` semantics (resume session, first
  poll, `Fence` → `Fenced{epoch, final_sequence}`); `PromotionWriter`; the engine-level fenced open
  and `fenced_or_promoting_refusal(dir, role)` in `start`; `ArchiveSealer::seal_through` (the
  fenced primary archives through `final` and seals; the promoted node's own standby archiver must
  seal its last epoch-E segment the same way before the conversion); protocol NPHYS004 (Resume
  carries the cursor's last group; `Diverged` id 11); the posture states `fencing`/`fenced`.

## 1. Session registry and push termination (`src/protocol/postgres/server.rs`, `handler.rs`)

- `SessionRegistry` (process-global, installed by the server): each accepted connection registers
  `{session_id, backend_pid, admitted_at, database generation, terminate: watch::Receiver<Terminate>}`
  and unregisters on exit. `Terminate { generation: u64, sqlstate: &'static str, message: String }`.
- The handler's read loop `select!`s on the socket and on the terminate watch. On a terminate
  newer than the session's admission generation: if a statement is in flight, let it finish its
  current message cycle (bounded by the existing `teardown_write_budget`), then send ErrorResponse
  severity FATAL (fields `S`,`V`=`FATAL`, `C`=`57P01`, `M`), flush, close, unregister. An idle
  session gets the frame immediately (Proxy absorbs a FATAL on an idle backend and redials).
- `registry.terminate_all(reason) -> Drained` bumps the generation and waits (bounded, 5 s) until
  every session admitted before it has unregistered; sessions flagged `exempt` (the `pg_promote`
  caller) are skipped. Both fencing (§3.2 of P5.1, which keeps the lazy path as a backstop) and
  promotion call it. Message texts: fence "terminating connection: this primary was fenced by epoch
  E+1"; promotion "terminating connection: this standby is being promoted to primary (epoch E+1);
  reconnect". `pg_terminate_backend(pid)` is NOT in scope (follow-up; the registry makes it cheap).
- MySQL listeners never run on a standby; nothing to do there.

## 2. `pg_promote(wait boolean, wait_seconds integer) → boolean`

- Registered in `type_inference` (Boolean), excluded from the result cache, listed in the standby
  guard's always-answered set. Accepted forms: `SELECT pg_promote(true, N)`, `pg_promote(false, N)`,
  `pg_promote(true)` (default 60), `pg_promote()`; literal args only (Proxy sends literals).
- Dispatched at the HANDLER level (simple query and extended Execute), not in the evaluator: the
  handler must release its own `self.database` (and the thread-local engine) before the reopen.
  The evaluator branch exists only to raise 55000 "pg_promote is answered by the server session, not
  by the embedded API" when reached from elsewhere.
- Semantics:
  - On a primary: ERROR 55000 "this node is already the primary (epoch E)".
  - On a standby that is not `streaming`/`reconnecting-within-lag`, or carries `ff:dirty`, or is
    `recovering`/`needs_recovery`: ERROR 55000 naming the posture and the recover command.
  - `wait=false`: start the promotion in the background and answer `t` immediately (PostgreSQL
    semantics: "initiated"); the session is then terminated like the others.
  - `wait=true`: run the promotion with budget `wait_seconds` (min 1, max 3600). Answer `t` once the
    store is reopened as a primary and the PG listener serves it (a fresh connection sees
    `pg_is_in_recovery()=f`, `in_hot_standby=off`, `timeline_id=E+1`). If the budget runs out while
    the promotion is still running, answer `f` (the promotion continues; PostgreSQL semantics); if
    it FAILED before the dirty record, ERROR 55000 with the reason; a failure after the record ends
    the process (§4).
  - The calling session survives the terminate-all (exempt), answers, and is then closed by the
    server with FATAL 57P01 on its next message cycle so the client also redials (it holds the old
    `in_hot_standby=on` parameter).

## 3. The online promoter (`src/replication/promote.rs`, `promote::online`)

A process-global `Promoter` handle installed by `StandbyRuntime` (like `install_standby`), one
promotion at a time (a second call gets 55000 "a promotion is in progress since <time>").

1. **Posture** → `promoting` (additive `state` value; `/ready` 503 from here; `heliosdb_require_ready`
   55000). WARN log with the caller (backend pid, client address).
2. **Fence through the live session.** New receiver `Command::Fence{epoch: E+1, reply}`: the
   receiver sends `Fence` on its session at the next poll boundary (its Hello already carries the
   history, the cursor and epoch E, which is what P5.1's `answer_fence` requires; the receiver's
   `node_id` in that Hello must be the promotion id minted for this promotion, so a later offline
   re-run after a crash is answered the same `Fenced`), receives `Fenced{final}`, keeps polling in
   Follow mode until its DURABLE cursor ≥ `final`, then reports `ReachedFinal{final}`.
   Budget: `[replication] promote_fence_timeout` (default 10 s) or `wait_seconds/2`, whichever is
   smaller, for the Fence round trip; the catch-up to `final` has the rest of the budget. If the
   primary is unreachable (receiver `reconnecting`) or the fence/catch-up times out:
   `[replication] promote_without_fence = "allow" | "refuse"` (default `allow`, because Proxy's
   failover promotes precisely when the primary is dead): `allow` → continue with `fenced=false`
   and a WARN naming the last applied sequence; `refuse` → abort (step 8) with 55000 "the primary
   at H did not answer the fence: promote it offline with `heliosdb-nano promote --force` or set
   promote_without_fence = allow".
3. **Stop replication**: receiver (shutdown watch + join, the in-flight run completes), the
   standby archiver (seals its segment, releases `archiver.lock`), the cascade source (its sessions
   get `Refuse(SourceNotServing)`), the standby `/ready` loop.
4. **Terminate sessions** (§1, exempt = caller), bounded 5 s; sessions that do not drain in time
   are closed by dropping their sockets (the registry holds a cancellation for each).
5. **Release the store**: publish the in-memory placeholder into the serving watch (the one the
   readiness gate used before the store existed, `main.rs:4553-4565`; sessions on it get the lazy
   57P01), the cascade watch and the HTTP holder (change the HTTP task to read the serving watch
   instead of a captured `Arc`), drop the runtime's own `Arc`s, then wait (bounded 10 s) for
   `Arc::strong_count(engine) == 1` and drop it: RocksDB closes and the lock is released. If the
   count does not reach 1 in time: abort (step 8) with the holder diagnostics logged at ERROR —
   this is a bug class, not an operator error.
6. **Convert**: write `NANO-PROMOTION.json` (P5.1 record, `fenced`, `final`), run `promote::convert`
   and `promote::identity_files` exactly as the CLI does (same functions). The `promotion-node` key
   is the standby's own `node_id`.
7. **Reopen as primary**: build the primary configuration from the standby's config with
   `replication.role = primary` (the standby's `replication_port`, token, TLS, archive settings;
   `auto_enable_physical_source` + `validate_physical_source` applied at promote time; WAL retention
   = primary defaults), `EmbeddedDatabase::with_config`, `prepare_physical_source(&engine)` — the
   `physical_service` slot in the main `select!` becomes a `JoinSet`/`mpsc` of services so one can
   be added after start —, start the primary archiver (`<H>/<E+1>/`) and gap fill, `install_primary`
   (epoch from the store), `ha_state`: role Primary, `set_read_only(false)`, history, sequence
   source, serving; `enable_forwarding_after_promotion()` (new, the ONE sanctioned undo of
   `disable_forwarding`, logged); publish the new store into the serving/cascade/HTTP watches (the
   serving watch sender stays alive: `main.rs:2649-2654, 4567-4589` no longer drop it); posture
   `primary`. The listeners were never closed: new connections now get `in_hot_standby=off`.
8. **Abort before the record** (steps 1-5 failed): re-publish the old store, restart the supervisor
   (`run_physical_standby`) so the receiver reconnects, posture back to its machine state, ERROR
   log; the caller gets 55000 with the reason. Sessions already terminated simply reconnect.
   **Failure after the record** (steps 6-7): the process logs ERROR with the exact re-run command
   and EXITS with code 2 (`start` refuses the directory until `heliosdb-nano promote --data-dir D`
   finishes it; then `start --replication-role primary`). No half-state is ever served.
9. **Answer** `t` to the caller (step 2 semantics), then close it.

`/health` during the promotion: `state=promoting`, `role=standby` until step 7, then `role=primary`,
`epoch=E+1`. `heliosdb-nano status` unchanged.

## 4. Config (`[replication]`, `config.rs`, `config.example.toml`)

- `promote_without_fence = "allow"` (`refuse`), `promote_fence_timeout = "10s"`. Both documented
  with the Oracle mapping (allow = `FAILOVER` semantics; refuse = `SWITCHOVER`-only cluster).

## 5. Tests (`tests/physical_promote_online.rs`, real daemons; proof-first)

1. **Graceful online promotion**: P + S + S2 loaded; a second idle session on S (a client that reads
   unsolicited frames); `SELECT pg_promote(true, 60)` on S → one row `t` within the budget; the idle
   session received FATAL 57P01 WITHOUT issuing a statement; a fresh connection to S: `in_hot_standby`
   startup parameter `off`, `pg_is_in_recovery()=f`, `timeline_id=2`, `prev_timeline_id=1`, history
   unchanged, writes succeed; P is fenced (state fenced, `/ready` 503, 25006 on writes, its idle
   sessions got the pushed 57P01); S holds everything through P's final; `/health` on S: epoch 2,
   role primary; S's replication port serves S2 after `recover --from-primary` (route 2).
2. **Proxy exact forms**: `SELECT pg_promote(true, 5)` text answer `t` (column name `pg_promote`);
   `SELECT pg_is_in_recovery()::text` immediately after on a fresh connection = `false`.
3. **`wait=false`**: answers `t` at once; the promotion completes within 60 s; `wait=true` with a
   budget too short for the catch-up answers `f` and the promotion still completes.
4. **Refusals**: on a primary 55000; on a `needs_recovery` standby 55000 naming recover; a second
   `pg_promote` during a promotion 55000.
5. **Dead primary**: P SIGKILLed; `promote_without_fence=refuse` → 55000 naming the offline
   command, S still a streaming/reconnecting standby afterwards (abort path, receiver back); `allow`
   → `t`, `repl:epoch.fenced=false`, WARN logged.
6. **Kill after the record** (env hook from P5.1 at steps 6/7): the process exits 2; `start` refuses;
   CLI `promote` re-run finishes; `start --replication-role primary` serves epoch 2.
7. **Fence push**: on P5.1's `fence_self`, idle sessions of the fenced primary receive 57P01 within
   1 s (the lazy path now only covers a session mid-statement).
8. `tests/promotion_refusal_tests.rs`: `pg_promote` now exists; `SWITCHOVER` still refuses with the
   hint naming both forms.

Existing suites: `physical_promote` (P5.1), `physical_standby_serving`, `ha_integration`,
`daemon_http_auth_subprocess`, handler unit tests (the always-answered list).

## 6. Docs

`physical-standby.md` "Promotion and the epoch" gains the online form and the Proxy recipe
(`[auto_promote] method = pg_promote` on Proxy's side, the exact query), `deployment-endpoints.md`
(`state=promoting`), `config.example.toml`, skill + verb-map (`pg_promote`), CHANGELOG. The
`heliosdb-nano-server` skill's HA section describes failover = `pg_promote` or `promote --force`,
switchover = graceful `pg_promote` with the primary reachable, and the rebuild of the old primary.

## 7. Gate

P5.1 gate scripts + `physical_promote_online` in new_tests/daemon/internal; live probe
`jlive/promote_online_probe.sh` (1 GB: time from `pg_promote` to `t`, to the first accepted write,
to S2 fast-forwarded; dead-primary variant; kill-after-record variant); a Proxy drill request to the
Proxy session (tmux) with the pair fixture `scripts/regress/nano-physical-pair.sh` and
`[auto_promote]` pointed at `pg_promote`.

## 8. Review follow-ups (2026-10-29; these amend §2, §3 and §5)

Adversarial review of the first implementation (2 critical, 1 high, 7 medium, 9 low) decided:

A. **Drain the sessions BEFORE the fence** (C1). Order of §3 becomes: 1 posture `promoting` →
   2 publish the placeholder into the serving watch and `terminate_all` (caller exempt) → 3 wait
   until every session holder of the store is gone, bounded by `wait_seconds` (the receiver keeps
   applying meanwhile; a statement still inside the engine is waited for, never assumed gone) →
   4 fence through the live session and catch up to `final` → 5 stop replication → 6 release the
   store (now only the runtime's own handles remain; a holder that persists here is a bug: ERROR
   with the holder list) → 7 convert → 8 reopen → 9 answer. A holder that does not go within the
   budget aborts WHILE THE PRIMARY IS STILL UNFENCED (restore: re-publish the store, posture
   back, 55000 to the caller naming the sessions). After `Fenced` the promotion never aborts on
   a release timeout: it keeps waiting, the caller gets `f` when `wait_seconds` runs out, and the
   promotion finishes.
B. **`allow` means unreachable, not slow** (C2). `promote_without_fence = allow` applies only
   while the receiver has no live session to the primary (`connecting`/`reconnecting`, the
   renewal under the promotion id failing to connect) for `promote_fence_timeout`. Once the
   receiver is connected, or once `Fence` has been sent, the promotion waits for `Fenced` for at
   least the source's own bound (DRAIN_TIMEOUT + ARCHIVE_SEAL_TIMEOUT + one frame timeout, ~75 s),
   independent of `wait_seconds` (`f` covers the caller). Scenario tests assert
   `repl:epoch.fenced == true` and `old_primary_final_sequence` whenever the primary was alive.
C. **After `Fenced`, no unfenced go-ahead** (M4). If the catch-up stalls because the primary
   became unreachable after answering `Fenced`, the promotion aborts naming the facts (the old
   primary IS fenced at `final`; this standby holds through `held`; re-run `pg_promote` once it
   is reachable, or `heliosdb-nano promote --force` to accept losing `held+1..=final`); the record
   is never written with `fenced=false` in that case. A stall with a live session is waited out
   (the catch-up budget deviation stands: as long as it advances).
D. **Refusal classification** (H1): only `Version`, `EpochStale`, `SourceStale`,
   `HistoryMismatch` and `Diverged` are refusals of the fence. `SourceNotServing` (fence
   incomplete, source draining) is retried on a new session within the budget; the abort message
   says "re-run `SELECT pg_promote`" and whether the primary is fenced.
E. **A later `Fence` command is accepted** after the previous `events` channel closed (M1): a
   re-sent `Fence` is idempotent and a cached `Fenced` is re-reported; the catch-up continues.
F. **Preflight binds the replication listener** (M2) and hands it to `ServePrimary`; a bind
   failure refuses the promotion with nothing changed. Preflight has no other side effect: the
   export directory is created at reopen, not at preflight. The `running` lock is not held
   across preflight; `run` is wrapped so a panic routes to abort (before the record) or exit 2
   (after). Exit after the record flushes the logs and uses `libc::_exit(2)`.
G. **Abort messages** (M3) always name the next command and say whether the old primary is
   fenced.
H. **Placeholder sessions answer the always-answered probes** (M6): `pg_is_in_recovery()` = `t`
   until promoted, `heliosdb_require_ready()` 55000 "promoting", `SHOW` forms; only data
   statements get 55000 not-ready; FATAL 57P01 goes to sessions that were on the old store
   (terminate-all), never as the answer to a probe. The second `terminate_all` after the reopen
   targets only tickets the placeholder served.
I. **TLS after promotion** (M7): a standby's `[replication] tls_*` / `--replication-tls` settings
   are the keys it serves with once promoted (the "no effect" warning at standby start is
   dropped for them, docs say so); preflight refuses a promotion that would serve plaintext when
   the pair replicated over TLS, naming the keys to configure.
J. **Restore** is bounded (the supervisor is respawned once the handles are closed or after
   `wait_seconds`, whichever first, with the holder list logged) and never after
   `StandbyRuntime::shutdown`; `ReplicationServices` tracks services by id, and `/health`
   `cascade` reflects a failed restart.
K. **Tests** (M5): scenarios 1 and 2 assert `fenced`/`final` and run writes on the primary
   DURING the promotion (the catch-up path `Advanced` → `ReachedFinal` is exercised); a scenario
   with a long statement in flight on the standby (A: the promotion waits, the primary stays
   unfenced until the drain completes); `refuse`/`allow` against a primary that is dead BEFORE
   the call (allow → `t`, fenced=false) and against one that dies AFTER `Fenced` (abort naming
   `promote --force`); the restore path after a refused promotion (receiver back, cascade back);
   an extended-protocol `pg_promote` (Describe OID 16, DataRow, CommandComplete, then FATAL after
   Sync); a cascading standby promoted online; `tests/physical_promote.rs` keeps the 57P01
   SQLSTATE assertion by capturing the connection future's error; scenario 3's pause is gated on
   a file, not a timer.
