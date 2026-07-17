# W3.3 — Same-row statement retry: typed `WriteConflict` + contended-writer bench + retry design

Status: **DESIGN-FIRST. Typed error + microbench landed; NO retry loop ships this
campaign.** Base: `perf/w3-design` off v4.2.0 (`a2e1b5b`). Companion analysis:
`PERF_ANALYSIS_2026_07_13.md` §"WAVE 3" (W3.3), spec `WAVE_IMPL_SPEC_2026_07_16.md` §W3.3.
Prior art this design builds on: the optimistic first-committer-wins registry
(`src/storage/conflict.rs`, `WriteConflictRegistry::validate_and_record:464`) and the
D4 SQLSTATE mapping (`protocol/postgres/handler.rs sqlstate_for_error:2782`).

> **STOP rule (binding).** No retry loop is implemented this campaign. This document
> ships (1) the typed `Error::WriteConflict` variant produced at the lock-timeout site
> and mapped to SQLSTATE 40001, and (2) an `#[ignore]`d contended-writer microbench that
> documents today's pessimistic spin. The retry loop itself (§5) is a SEPARATE, gated
> task; the go/no-go and the config-default choice are coordinator decisions, not
> assumptions.

---

## 1. Today's behavior: the pessimistic same-row spin

Two writers contending for one row do not fail fast — the second **spins** for the full
lock-acquire timeout and then errors, even though it can provably never be granted the
lock. The author already documented the futility in-code, on
`LockManager::with_default_timeout` (`src/storage/lock_manager.rs:173-185`), quoted verbatim:

> The previous 60-second default was harmful: the lock-acquire wait is a
> synchronous spin, and a single same-row write conflict wedged the *whole
> server* for up to 60s — new connections could not complete startup and
> unrelated statements stalled. Worse, the lock holder's own COMMIT cannot
> make progress until the waiter gives up, so for a write-write conflict
> *waiting is futile*: the wait can only ever end in a timeout, never in
> the lock being granted. A short bound turns that 60s server-wide stall
> into a fast, retriable serialization/lock-timeout error.
>
> The proper fix (NANO_v3.58 HTAP spec, Option 2) is to drop this
> redundant pessimistic write lock entirely and rely on the optimistic
> first-committer-wins registry (`WriteConflictRegistry::validate_and_record`),
> which already reports write-write conflicts at COMMIT with no spin.

### 1.1 The spin, precisely (cited)

`acquire_lock` (`lock_manager.rs:209`) loops: `try_acquire_lock` → on "Lock conflict"
run deadlock detection, then check `start.elapsed() >= timeout` (`:254`), else
`std::thread::sleep(Duration::from_millis(1))` (`:279`) and retry. The default timeout is
1000 ms (`with_default_timeout:186-193`, env `NANO_LOCK_TIMEOUT_MS`). For a **same-row
write-write** conflict the waiter is never in a cycle with the holder (the holder isn't
waiting on the waiter), so `detect_deadlock` (`:357`) returns false.

**Grants-on-release (load-bearing for §4/§5.4).** The loop re-runs `try_acquire_lock`
every poll, so the instant the holder releases (its `commit`/`rollback` clears
`acquired_locks`, `transaction.rs:920` commit / `:1104` rollback) the waiter is granted on
its NEXT 1 ms poll — provided that poll lands before `:254`. The corollary, which an adversarial review of the
first draft correctly demanded be made explicit: **a full-timeout `WriteConflict` is
produced if and only if the holder held the row for ≥ the timeout.** A holder that releases
in microseconds does NOT yield a `WriteConflict` — the waiter is granted ~≤1 ms after
release, having paid one poll, not the timeout. So "two autocommit writers collide" does
not, by itself, produce anything to retry. Only two regimes drive a waiter all the way to
`:254`:
1. the holder is a **long-held explicit** transaction (open `BEGIN…`, not yet committed) —
   retry is futile until the client commits (§5.2 / §5.4 sub-case 2b); or
2. the holder is a **short-lived autocommit** statement that is *prevented from releasing*
   inside the timeout because the waiter's own spin has pinned the resource the holder's
   commit needs — the futility note's coarse block, made precise in §1.3.

### 1.2 Where the spin actually bites (scope of the whole item)

The pessimistic row lock lives ONLY on the `Transaction::put`/`get` path
(`storage/transaction.rs:466-471` write, `:346-354` read), and only when the transaction
carries a `LockManager` — i.e. `Transaction::new_with_session` (`transaction.rs:285/313`),
NOT `Transaction::new` (`:221`, `lock_manager: None`). The **engine autocommit fast paths**
(`insert_tuple_fast`, `update_tuple_fast*`, `delete_tuple_fast*`) take **no** lock (no
`acquire_lock` call exists in `storage/engine.rs`); they rely on the optimistic
`WriteConflictRegistry` at commit. Crucially, `execute_in_session` **skips those engine
fast paths for every session statement** ("Skip fast paths for session transactions —
writes must go through the transaction write set", `lib.rs:14631`), so ALL DML on a session
(= every wire connection) goes through a lock-carrying `new_with_session` transaction:

| caller | txn | locks? | conflict mechanism |
|--------|-----|--------|--------------------|
| `db.execute`/`query` (embedded autocommit, non-session) | `Transaction::new` (`engine.rs:3460`) or engine fast path | no | optimistic registry / none |
| session (wire) statement, **no** open txn | **implicit** single-statement `new_with_session` txn (`lib.rs:14634-14695`) | **yes** | **pessimistic spin** → retry target (§5.2) |
| session (wire) statement inside `BEGIN…` | user's open `new_with_session` txn (`lib.rs:14616-14633`) | **yes** | **pessimistic spin** → surfaces 40001 (§5.2) |

(`try_handle_session_fast_autocommit`, `lib.rs:14607`, is a narrow handler for
helios-specific `SET`-style statements, not DML — it returns `None` for an UPDATE, so DML
falls through to the two lock-carrying branches above.) The two "yes" rows are where a
same-row waiter spins. The futility note's "new connections could not complete startup" is
precisely a wire connection's implicit or explicit statement waiting on a long-held row
lock from another connection's open transaction.

### 1.3 Why a short-lived holder cannot release: the worker-pinning coarse block

The futility note's "the lock holder's own COMMIT cannot make progress until the waiter
gives up" is NOT a lock-manager property — `acquire_lock` grants on release (§1.1). It is a
**thread-scheduling** property: the resource the analysis names the *worker-pinning spin*
(`PERF_ANALYSIS §W3.3`, "1s pessimistic worker-pinning spin"). The wire server is
`#[tokio::main]` (`src/main.rs`) — a multi-threaded runtime with a bounded worker pool
(≈#cores); each connection is one `tokio::spawn`ed task (`protocol/postgres/server.rs:210`).
The query handler is `async` but calls the engine **synchronously** — `execute_for_session`
(`handler.rs:1085` → `lib.rs:14964`) with **no** `spawn_blocking` — so the
`std::thread::sleep`-based spin (`lock_manager.rs:279`) **blocks its tokio worker thread**
for the whole timeout (a blocking sleep is invisible to the cooperative scheduler; it never
hands the worker back). When ≥ #worker-threads waiters spin at once they pin every worker;
a short-lived holder that is *ready to commit* — itself a spawned task — cannot be scheduled
onto any worker to run its microsecond commit, so it cannot release, so every waiter runs to
`:254`. That is the exact mechanism behind the note's "new connections could not complete
startup" and its claim that the holder's COMMIT is blocked by the waiters.

Corollary that reconciles §1.3 with §1.1: under an **unbounded** thread supply this regime
cannot arise — the holder always gets a thread, commits in microseconds, and every waiter is
granted on release with no `WriteConflict`. **The full-timeout autocommit-vs-autocommit
`WriteConflict` is a pool-saturation artifact**, and that — not "prompt holder release" — is
what a retry must defeat (§4, §5.4).

---

## 2. Deliverable 1 (shipped): typed `WriteConflict`

### 2.1 The variant

`Error::WriteConflict { table, row, holder_txn, waiter_txn, waited_ms }`
(`src/error.rs`), constructor `Error::write_conflict(...)`. Struct variant (the enum's
first non-tuple variant) so the raise site names the contended row and the holder instead
of formatting a string a downstream layer must re-parse. Its `Display` begins
`serialization failure: …` — load-bearing (§2.3). `Error` is not `Clone`
(`#[from] std::io::Error`), so the variant stays owned `String`s, matching the enum idiom.

### 2.2 Produced where the spin times out (NO behavior change beyond the richer error)

At the `:254` timeout, before the existing `cleanup_transaction`, capture the live holder
(`primary_holder`, `lock_manager.rs:327`, best-effort — 0 if released in the race), split
the resource key `data:{table}:{row_id}` into `(table, row)` (`split_row_resource:573`),
and return `Error::write_conflict(table, row, holder_txn, transaction_id, self.timeout_ms)`
(`lock_manager.rs:269`). The **spin is unchanged**: same 1 ms poll, same timeout, same
`cleanup_transaction`, same grant-on-release (§1.1). What changes is the returned error
*type* and — as the deliverable intends — the **wire SQLSTATE it maps to** on both
protocols (§2.3): PG 25000→40001, MySQL 1105/HY000→1213/40001. The old text (`"Lock
acquisition timeout after {ms}ms for transaction {id}"`) is gone; **no test asserted on it**
(verified: `rg "Lock acquisition timeout" src/ tests/` hits nothing now), so no assertion
had to be rewritten — the sole consumers were the wire SQLSTATE mappers (§2.3), which
classified the old string as `INVALID_TRANSACTION_STATE` (25000) on PG and, matching no
branch, `ER_UNKNOWN_ERROR` (1105/HY000) on MySQL.

### 2.3 Wire + API surface (interface-coverage gate #5)

| interface | mapping | site |
|-----------|---------|------|
| PostgreSQL wire | `Error::WriteConflict { .. } => SERIALIZATION_FAILURE` **40001** (was 25000) | typed arm added to `sqlstate_for_error` (`handler.rs:2803`); detail/hint keyed on 40001 already says "Retry the transaction" (`detail_hint_for_error`, `:2862`) |
| MySQL wire | `1213` / SQLSTATE **40001** (ER_LOCK_DEADLOCK; was 1105/HY000 ER_UNKNOWN_ERROR — the old string matched no branch) | `map_error_code` (`protocol/mysql/handler.rs:3200`) sniffs `contains("serialization failure")` — matched for free by the `Display` text (§2.1); no code change |
| REST / HTTP | **409 Conflict** | `From<Error> for ApiError` exhaustive match (`api/models/error.rs:122`) — a new arm was required to compile |
| embedded API | the typed `Error::WriteConflict { .. }` itself | pattern-matchable by callers (the microbench does exactly this) |

The 25000→40001 change is the deliverable, not a side effect: a lock-timeout write
conflict IS a retriable serialization failure, and PG drivers key their retry loops on
40001. No new tunable/magic number is introduced (the timeout is already env-tunable;
§6 discusses wiring it to config).

### 2.4 Tests (would fail on pre-change code)

- `lock_manager.rs write_lock_timeout_yields_typed_write_conflict` — holder takes a write
  lock, a second writer times out; asserts the typed variant's five fields
  (`table="accounts"`, `row="42"`, `holder_txn=1`, `waiter_txn=2`, `waited_ms=200`). Fails
  on pre-change code (returned an `Error::Transaction` string).
- `lock_manager.rs split_row_resource_parses_only_data_keys` — the key-splitting contract.
- `handler.rs write_conflict_maps_to_40001_with_retry_hint` — the SQLSTATE arm + retry
  hint. Fails pre-change (the string mapped to 25000).

---

## 3. Deliverable 2 (shipped): contended-writer microbench

`tests/w3_3_contended_writer_bench.rs`, `#[ignore]`d
(`cargo test --test w3_3_contended_writer_bench -- --ignored --nocapture`). It documents
today's spin: 2 rows, a bounded 300 ms timeout (`NANO_LOCK_TIMEOUT_MS`, set before the db
is built; production default 1000), 8 same-row conflicts ⇒ well under the 30 s cap.

**Why single-threaded is deterministic and correct.** The holder opens a ReadCommitted
session transaction and UPDATEs row 1, taking the row's write lock, and never commits
(`begin_transaction_for_session` + `execute_in_session`). A second session's UPDATE of the
same row then blocks in `acquire_lock` and — because the holder never yields (futility,
§1) — spins to the full timeout and returns `Error::WriteConflict`. No second thread is
needed: the holder's lock is stored in the holder txn's `acquired_locks`
(`transaction.rs:471`), held across the waiter's whole spin; the waiter's 1 ms poll loop
advances on its own. This is the path NO existing test covers —
`conflict_detection_tests.rs` COMMITs the first writer before the second writes
(`:30`), so its conflict is the optimistic COMMIT-time path, never the lock.

The bench records per-conflict latency (min/median/mean/max) and asserts each is
`>= timeout/2` — the load-bearing claim that the spin runs ~to the timeout every time and is
never granted early (the futility, measured). It asserts **only** that lower bound: an upper
bound would add flakiness (a scheduler/GC stall on this OOM-prone host can push a single
iteration arbitrarily high without disproving futility) while proving nothing the
deliverable requires; total runtime stays `<30s` structurally (`CONFLICTS × TIMEOUT_MS`). It
asserts the typed variant (table, non-empty row, distinct holder/waiter txns,
`waited_ms == timeout`). Note the bench holder is a *long-held* open txn (regime 1, §1.1) —
it documents today's per-conflict spin cost; the pool-saturation regime 2 (§1.3) is a
concurrency property the coordinator's wire A/B measures, not this deterministic microbench.

---

## 4. The retry opportunity, quantified

A full-timeout `WriteConflict` costs ≈ **the whole timeout** (1000 ms default) and then the
statement FAILS. Retry's value is bounded to the ONE regime that both (i) produces such a
conflict and (ii) can succeed on re-execution: **autocommit-vs-autocommit under
worker-pool saturation** (§1.1 regime 2, §1.3). There the holder is ready to commit in
microseconds but is starved of a worker thread by the waiters' own spins; a retry that
*releases its pinned worker* (rollback + a yielding backoff — §5.1 / §5.4) lets the starved
holder be scheduled, commit, and release the row, and the retry then re-acquires at storage
speed. That is the `1/s → storage-speed` the analysis cites (`PERF_ANALYSIS §W3.3`): the 1 s
is the timeout paid per collision **under saturation** today; retry collapses it to
holder-commit + backoff time.

Two regimes are explicitly OUT of scope for a win, and the design must not claim one:
- **Uncontended / unsaturated** autocommit-vs-autocommit produces **no** full-timeout
  conflict at all (grants-on-release, §1.1) — there is nothing to retry and nothing to
  speed up. The win is a *high-contention* claim, never a per-collision one.
- **Long-held explicit** holders: retrying is futile until the client commits (§5.2 /
  §5.4 sub-case 2b), and the design does NOT auto-retry there.

---

## 5. Design: statement-level retry (autocommit only)

**No retry loop ships this campaign.** The mechanism, its exact insertion point, and its
invariants:

### 5.1 Atomicity: retry re-runs the WHOLE statement, never partial effects

The insertion point is the implicit-autocommit `Err(e)` arm at **`lib.rs:14689-14692`**:

```rust
Err(e) => {
    let _ = txn.rollback();          // ← nothing was committed
    self.finish_session_art_undo(session_id, true);
    Err(e)
}
```

A `WriteConflict` from `acquire_lock` happens at `txn.put`, BEFORE the single
`txn.commit_with_timestamp` (`:14681`). The failed attempt's WriteBatch is never applied;
`rollback` (`:14690`) discards the write-set. So retry starts from a clean slate — there is
no "partial effect" to reconcile, by construction. The retry re-executes
`execute_in_transaction_no_fast_path` (`:14669`) against a **fresh** implicit transaction
built with a new snapshot (`self.storage.next_timestamp()`, `:14639`). Sketch (design only):

```
let mut attempt = 0;
loop {
    let txn = build_implicit_txn(next_timestamp());   // fresh snapshot each attempt
    match self.execute_in_transaction_no_fast_path(sql, &txn) {
        Ok(count) => { commit(txn); break Ok(count); }
        Err(Error::WriteConflict { .. }) if attempt < retry_max => {
            let _ = txn.rollback();                     // frees the row + the worker
            attempt += 1;
            yield_backoff(backoff(attempt));            // MUST yield the worker — §5.4
            continue;
        }
        Err(e) => { let _ = txn.rollback(); break Err(e); }
    }
}
```

Re-execution re-reads the base rows at the new snapshot, so a lost-update is impossible:
the retried UPDATE recomputes `SET v = v + 1` from the value visible at the fresh snapshot,
which now includes the previously-conflicting committed write (read-committed semantics,
PG-equivalent).

### 5.2 Autocommit-only; explicit transactions surface 40001 (PG parity)

Retry is applied ONLY in the implicit-autocommit branch (`session_txn_slot(session_id)`
is `None`, `lib.rs:14634`), where the statement **is** its own transaction (the code already
calls it an "implicit single-statement session transaction", `:14664`, `set_conflict_registry(…,
false)` — it never validates, RC-atomic). The explicit-txn branch (`:14616-14633`, slot is
`Some`) must NOT auto-retry: the statement is one step of a user's multi-statement unit, and
silently re-running it against a newer snapshot would break the transaction's snapshot
isolation and could double-apply earlier effects. There, `WriteConflict` propagates to the
client as **40001** and the client owns the retry of its whole transaction — exactly
PostgreSQL's contract (a serialization failure aborts the transaction; the app retries).

### 5.3 Config knobs (named; NOT added live this campaign)

Following the W3.1 (`hot_shape_slots`) / W3.2 (`elide_latest_version`) precedent, the future
knobs are NAMED here in `config.example.toml` style but NOT wired into `config.rs` (no live
retry loop this campaign). Home them in the existing `[locks]` section (which already owns
`timeout_ms`):

```toml
[locks]
# Auto-retry an AUTOCOMMIT statement that hits a same-row write conflict
# (serialization failure, SQLSTATE 40001) against a fresh snapshot.
# 0 = OFF (surface 40001 to the client, today's behavior). Explicit
# transactions never auto-retry regardless of this setting.
# Default: 0
statement_retry_max = 0

# Base backoff between retries in milliseconds; grows exponentially with
# full jitter up to statement_retry_backoff_max_ms (anti-livelock, §5.4).
# Default: 5
statement_retry_backoff_ms = 5

# Cap on the backoff sleep per retry (milliseconds).
# Default: 100
statement_retry_backoff_max_ms = 100
```

Default `statement_retry_max = 0` makes the feature, when implemented, behavior-preserving
by default (a one-way opt-in, like W3.2's format flag) — the STOP-safe choice, leaving the
default flip a coordinator decision.

**Cautionary precedent for the implementer (do not repeat).** `[locks] timeout_ms`
(`config.rs LockConfig:1224`, default 30000) is **orphaned**: it is validated but never
wired to the LockManager — all three `EmbeddedDatabase` constructors build it via
`LockManager::with_default_timeout()` (`lib.rs:5173/5280/5389`), which reads only the env
`NANO_LOCK_TIMEOUT_MS` (default 1000). The retry knobs MUST be genuinely wired at
`with_config` (mirror W3.1's `lock_census`/W3.2's `write_volume_stats` `set_enabled`
pattern), and the implementer should ALSO wire `locks.timeout_ms` → the LockManager so the
spin bound is config-tunable, not just env — otherwise gate #5 (tunable, no magic numbers)
is only half-satisfied.

### 5.4 Deadlock / livelock-freedom (the load-bearing correctness section)

Three distinct failure shapes, kept separate:

1. **True deadlock (cycle).** Two statements each holding one row and waiting on the
   other's is caught by `detect_deadlock`'s DFS cycle check (`lock_manager.rs:357`), which
   aborts a victim with `Error::deadlock` → **40P01** (`:239-251`), a DIFFERENT error from
   `WriteConflict`. The retry loop keys ONLY on `Error::WriteConflict` (§5.1), so a genuine
   deadlock is NOT auto-retried into a livelock — it surfaces 40P01 and (for autocommit) the
   victim could be retried at most `retry_max` times with backoff, converging because the
   detector guarantees one side is chosen each cycle.

2. **Timeout without a cycle (the WriteConflict case).** The holder is not waiting on the
   waiter; it is simply holding. Two sub-cases:
   - **(sub-case 2a) autocommit vs autocommit, same row — the retry-wins case.** A
     full-timeout conflict here means the pool was saturated: the holder is ready to commit in **microseconds**
     but was starved of a worker by the waiters' spins (§1.3). Liveness therefore does NOT
     follow from "holders releasing promptly" — while the holder is pinned it *cannot*
     release, which is exactly what the initial `acquire_lock` spin failed to notice (it
     held its own worker the whole timeout and so never let the holder run). Liveness comes
     from the retry **releasing the coarse resource the waiter was holding**: on
     `WriteConflict` the retry rolls back (discarding its empty write-set, §5.1) and **backs
     off in a way that yields its worker thread**. During that backoff window a freed worker
     schedules the starved holder, which commits in microseconds and releases the row; the
     retried `acquire_lock` then finds it free and succeeds. **Load-bearing implementation
     constraint (the mechanism, not hand-waving):** the backoff MUST be a scheduler-yielding
     wait — an `.await` on `tokio::time::sleep`, or moving the blocking statement execution
     onto `spawn_blocking` so the sleep does not occupy a runtime worker — because a
     synchronous `std::thread::sleep` backoff (like today's `:279` poll) would re-pin the
     worker and *reproduce* the livelock instead of breaking it. Given a yielding backoff:
     bounded `retry_max` + **full-jitter exponential backoff** (§5.3) desynchronizes waiters
     that lost simultaneously so they do not re-collide in lockstep, and because each retry
     hands a worker back to a starved holder, the holders drain and the probability the same
     pair keeps colliding decays geometrically — expected retries to success O(1) for two
     contenders, hard-bounded by `retry_max` in the worst case.
   - **(sub-case 2b) autocommit vs a long-held explicit txn.** Retrying is futile until the
     explicit txn commits (§1). The autocommit waiter exhausts `retry_max` (bounded, with backoff) and
     surfaces 40001 — the correct, terminating outcome. Retry does not and must not paper
     over a client holding a transaction open; it only smooths transient autocommit-vs-
     autocommit contention.

3. **Tiebreak (design the guarantee, don't hand-wave).** For adversarial equal contention
   the deterministic tiebreak is **already present** and reused, not invented: (a) the
   deadlock detector's victim selection breaks true cycles; (b) full-jitter backoff breaks
   synchronized non-cycle retries probabilistically. If a future workload demonstrates a
   pathological synchronized livelock despite jitter (precluded once the yielding backoff
   frees a worker for the starved holder each retry, sub-case 2a), the deterministic
   fallback is **oldest-transaction-
   wins**: on the Nth retry (`N == retry_max`), the waiter with the lowest `waiter_txn` (a
   monotonic id, `next_timestamp`) is granted priority by NOT backing off, guaranteeing one
   side drains first. This is a design option to hold in reserve; the primary mechanism
   (yielding-backoff worker release + jitter + bounded count, sub-case 2a) needs no global
   coordination.

### 5.5 Interaction with triggers

A retried statement re-fires its triggers. This is safe for **data** effects: triggers run
SQL within the same transaction (`TriggerRegistry`/`TriggerContext`, `lib.rs`), so a failed
attempt's trigger writes are in the rolled-back WriteBatch (`:14690`) and never committed —
re-firing on retry is equivalent to the statement having executed exactly once. Two
non-transactional effects are re-run and are **accepted as PG-parity**, to be documented at
implementation:
- **Sequence / SERIAL draws** advance on the failed attempt and are NOT rolled back (Nano
  sequences are durable/non-transactional, matching PG `nextval`), so a retried INSERT can
  skip sequence values — exactly what PostgreSQL does when an app retries a 40001. Acceptable.
- **`RAISE NOTICE`-style side outputs** would re-emit; acceptable and PG-consistent.
Retry must therefore be gated to statements whose ONLY effects are transactional-or-
PG-parity. AFTER-trigger cascades that themselves conflict re-run under the same retry
(they are inside the same statement's WriteBatch), so no separate handling is needed.

### 5.6 Interaction with the W2.5 committed-write watermark

W2.5 (`storage/transaction.rs set_write_watermarks`, wired at `lib.rs:14663`;
`scan.rs:165-201`) lets an in-txn reader serve current `data:` when the table's
`write_watermark(table) <= snapshot_ts`. A retry acquires a **fresh** snapshot
(`next_timestamp`, §5.1), so the retried statement's reads re-evaluate the watermark against
the newer snapshot — it correctly sees the conflicting committer's write (whose commit
raised the watermark above the OLD snapshot). No special interaction is required: because
each retry attempt is a brand-new implicit transaction with an empty write-set, the
read-your-own-writes and snapshot-monotonicity invariants W2.5's tests pin
(`tests/w2_5_watermark_read_tests.rs`) hold per attempt unchanged. The one requirement:
the retry MUST take a genuinely new snapshot per attempt (not reuse the failed attempt's),
so the watermark comparison advances — the sketch's `build_implicit_txn(next_timestamp())`
encodes this.

### 5.7 Relationship to the optimistic `WriteConflictRegistry`

The registry (`conflict.rs:464`) reports write-write conflicts at COMMIT for the *explicit*
snapshot-isolation path (`serialization failure: write-write conflict on key …`,
`transaction.rs` commit) — a different site from the pessimistic lock spin this item
retries. Scope decision: **this design retries only the pessimistic-lock `WriteConflict`**
(the spin). The optimistic COMMIT-time failure already fails fast (no spin) and, for
autocommit, could be folded into the SAME retry loop later (both are 40001, both leave no
partial effect after rollback) — flagged as a natural extension, not in this campaign.

---

## 6. Interface coverage & tunables (gate #5)

- **Shipped now:** the typed `Error::WriteConflict` is surfaced on all four interfaces
  (§2.3: PG 40001, MySQL 1213/40001, REST 409, embedded typed variant). No new
  tunable/magic number; the spin timeout is already env-tunable.
- **Future (retry loop):** `[locks] statement_retry_max` /
  `statement_retry_backoff_ms` / `statement_retry_backoff_max_ms` (§5.3), all documented in
  `config.example.toml` style, default OFF, genuinely wired (unlike the orphaned
  `locks.timeout_ms`). No hardcoded retry count or backoff — every bound is a knob.

---

## 7. Go / No-Go (coordinator decision)

**PROCEED to a future retry-loop task** iff the coordinator gate confirms, from a
**concurrent** wire A/B (many same-row autocommit writers, not the deterministic microbench
— which documents only the long-held regime, §3), that full-timeout conflicts arise from
**pool saturation with short-lived autocommit holders** (§1.3 regime 2), the regime where
retry converges (§5.4 sub-case 2a). Then the implementation is a SEPARATE task that adds
§5's loop at `lib.rs:14689` behind `[locks] statement_retry_max` (default 0), wires the
knobs, **implements the backoff as a scheduler-yielding wait** (the §5.4 sub-case 2a
constraint — without it the retry re-pins the worker and cannot break the livelock), adds
the §5.4 livelock and §5.5 trigger/sequence tests, and re-runs a concurrent A/B to show
conflict latency collapse from ~timeout to holder-commit + backoff time.

**DO NOT PROCEED / re-scope** if the gate shows the contended holders are predominantly
**long-held explicit transactions** (§5.2 / §5.4 sub-case 2b): retry cannot help there by
construction, the correct outcome is already 40001, and the effort belongs instead on
reducing lock hold time or dropping the redundant pessimistic lock entirely in favor of the
optimistic registry (the futility note's "proper fix", §1) — a larger change than a retry
loop and out of this campaign's scope.
