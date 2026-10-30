# S1 — a statement never stalls its server (Sprinter ccebc95977f1)

## The defect (root cause confirmed 2026-10-30, gdb + tokio 1.53.3 source)

The server runtime is `new_multi_thread()` (src/main.rs ~1655). Exactly one parked worker
owns the I/O + timer driver (`park.rs` `driver.try_lock()`); the others park on condvars.
When client bytes wake the driver owner, it runs the woken connection task from its LIFO
slot, and `should_notify_others` is false for a single task, so no other worker is woken.
Statement execution is synchronous inside that task (no `block_in_place`, no
`spawn_blocking`). For the whole statement nobody polls epoll or the timers: new
PostgreSQL connections are not accepted (the accept loop runs on the main `block_on`
thread but needs the driver to see the socket), other sessions' messages are not read,
the physical source's frame I/O is not served (its polls themselves already run on
`spawn_blocking`, physical_source.rs ~498-531), and timers do not fire. Observed: a 60 k
row `INSERT … SELECT` (16 s, debug) blocked a fresh `SELECT 1`; statements over 30 s
made the standby flap `reconnecting`; `SELECT pg_promote(...)` on a standby waited for a
long query. `tokio::task::block_in_place` moves the LIFO task to the run queue and spawns
a replacement worker that takes the driver — the test hook `heliosdb_test_hold()`
(src/sql/evaluator.rs ~1150) already does this and no longer stalls anything.

## What to build

1. **One helper module** (e.g. `src/runtime_offload.rs`, re-exported as needed):
   - `pub fn off_worker<T>(f: impl FnOnce() -> T) -> T` — if `tokio::runtime::Handle::
     try_current()` is a `MultiThread` runtime, `tokio::task::block_in_place(f)`;
     otherwise (no runtime, `current_thread`, a blocking-pool thread) `f()` inline. Never
     panics. Nested calls cost a TLS check (tokio makes nested `block_in_place` a no-op).
   - `pub fn offload_statement<T>(f: impl FnOnce() -> T) -> T` — `off_worker(f)` when the
     process-wide switch is on, `f()` otherwise. The switch is an `AtomicBool`, default
     OFF (so embedders' runtimes — possibly inside a `tokio::task::LocalSet`, where
     `block_in_place` panics — see no change), turned ON by the server (`start`, `mcp
     serve`, any subcommand that serves a wire or HTTP listener) before its listeners
     start, unless the environment variable `HELIOSDB_NANO_STATEMENT_OFFLOAD=off` is set
     (an A/B and emergency switch; log once at startup when it is off). Expose
     `set_statement_offload(bool)` for embedders who want it.
   - The three existing hand-rolled copies — `src/sql/advisory_lock.rs` ~904-911,
     `src/replication/replication_wait.rs` ~74-81, `src/sql/evaluator.rs` ~1174-1179 —
     call `off_worker` (keep their current always-on behaviour).
2. **Wrap every per-statement engine entry point** in `offload_statement`, at the entry
   (so PostgreSQL simple + extended, MySQL, REST/BaaS, MCP, the REPL and embedded callers
   on a server runtime are all covered by one change). The survey's list (src/lib.rs,
   approximate lines): `execute_in_transaction_inner` 8125, `execute` 11998,
   `execute_params_inner` 18579, `execute_many_params_inner` 18701,
   `execute_params_returning_inner` 18784, `query` 20896, `query_with_schema` 21596,
   `execute_in_session` 22641, `query_in_session` 22809, `execute_for_session` 23786,
   `query_with_schema_for_session` 23910, `execute_returning_for_session` 23999,
   `execute_params_returning_for_session` 24121, `execute_params_for_session_inner`
   24240, `query_params_for_session_inner` 24353, `query_params_with_schema_for_session`
   24453, `query_params_inner` 24677, `query_params_with_schema_inner` 24956 (these are the
   functions that charge tenant QPS — use that as the census and confirm none is missed),
   the session transaction control functions (~22273, ~22408, ~22559),
   `copy_bulk_insert_for_session`, `try_cached_query_with_schema_for_session`, and the
   planning done at Parse/Describe (`shared_query_schema` handler_extended.rs ~122,
   `wire_parameterized_plan` ~652/~696). In the PostgreSQL handler also wrap
   `PgCatalog::handle_query` (handler.rs ~3071; handler_extended.rs ~98, ~468), the COPY
   fallback block (~3710-3721), `pragma_table_info` (~4271) and `parse_cached` (~4300).
   Wrapping the body of an entry point is enough; inner entry points re-entered from it
   (procedures, triggers, FK cascades) nest for free.
3. **Thread-bound state stays valid.** `block_in_place` runs `f` on the SAME thread, so
   the thread-locals (`SESSION_SCOPED_STATE`, the guards at lib.rs ~535,
   `temp_namespace_guard`, `write_volume::stmt_scope`) and the `replication_wait`
   task-local (`UNCONFIRMED`, replication_wait.rs ~30) keep working. Do NOT use
   `spawn_blocking` for statements.
4. **No change** to the replication source, receiver or archiver (already off-worker).

## Tests

- Unit tests for the helper: outside any runtime → inline; inside `current_thread` → inline,
  no panic; inside a multi-thread worker with the switch on → a concurrently spawned task
  makes progress while `f` sleeps, with `worker_threads(1)`; switch off → inline; nested
  calls fine.
- `tests/statement_off_worker.rs` (deterministic: run the DAEMON binary with
  `TOKIO_WORKER_THREADS=1`, so the single worker always owns the driver and the bug
  reproduces every time without the fix):
  - with a statement held inside the engine (the `internal-tests` hook
    `heliosdb_test_hold()`, gate that test on `internal-tests`): a NEW PostgreSQL
    connection completes startup and answers `SELECT 1` within 2 s; an already-open second
    connection answers `SELECT 1` within 1 s; `GET /health` answers; after the hold is
    released the held statement answers `t`. Extended-protocol variant of the held
    statement too.
  - default tier (no hook): a statement that runs at least 3 s in a debug build (calibrate
    — e.g. a cross join of two `generate_series` aggregated with `count(*)`), started on
    one connection; a new connection's `SELECT 1` must answer while the long statement is
    STILL running (assert from timestamps that it had not finished, so the test cannot
    pass vacuously; if it finished too fast, fail with "calibration" rather than pass).
  - with `HELIOSDB_NANO_STATEMENT_OFFLOAD=off` the default-tier test's probe does NOT
    answer while the statement runs (proves the test detects the bug and the switch works).
  - with a physical standby attached (`tests/support/physical_pair.rs`) and
    `TOKIO_WORKER_THREADS=1` on the primary: during a held statement on the PRIMARY of
    10 s, the standby stays `streaming` and its `lag_secs` stays under 2 s.
- Existing suites must pass unchanged: pg wire (simple, extended, COPY), MySQL wire,
  REST, MCP, physical replication, advisory locks, sync commit (`replication_wait`).

## Performance

The dispatching session measures it, not you: wire A/B (pgbench `SELECT 1` and indexed
point reads at 1, 8, 32 clients) with the switch on vs `HELIOSDB_NANO_STATEMENT_OFFLOAD=off`
on the same binary. Keep the helper's fast path to one relaxed atomic load plus
`try_current()`. If the A/B shows a loss above 3 %, the follow-up is static exemptions
(result-cache hits, catalog intercepts), not an adaptive scheme.

## Rules

Do not run cargo, git or any build; the dispatching session runs every build and gate.
No new `unwrap`/`expect` on non-test paths. Report: files touched, the final list of
wrapped functions and anything you found that the list above missed, and anything in
this spec that turned out wrong.
