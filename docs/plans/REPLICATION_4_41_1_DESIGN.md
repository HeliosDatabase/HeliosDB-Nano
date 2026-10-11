# Nano 4.41.1 replication design: serving resync, committed catch-up, fenced promotion

Status: DESIGN, for owner decision. No code in this change. Baseline: `fix/nano-4411-integration` (P0 candidate `ea1fa460` + main `99d0c24d`).
Sources: Codex/Astra notes `RECOVERY-COMMITTED-PATH-ASTRA-NOTE-OCT7.md` and `recovery-writer-closure-r1/REPORT.md` (parallel-20261004 baselines), plus a read of `src/replication/*`, `src/main.rs` and `src/storage/transaction.rs` on this branch.

## 1. The three deliverables, as acceptance statements

1. **Serving resync**: a standby that fell behind, restarted, or was newly built reaches a state where it answers SQL with exactly the primary's committed data, and says "not ready" until then.
2. **Committed-SQL catch-up**: after any disconnect or restart the standby receives every committed change it missed, none that were not committed, with no silent gap. Original defect (archive issue 2e98f330e009): primary rows `[1,2,3,4]`, standby restarted mid-stream ends with `[1,4]` and reports healthy.
3. **Fenced promotion**: a standby can be promoted to primary such that the old primary can no longer acknowledge writes that the new primary lacks.

## 2. What exists and what is wrong with it

| Piece | State on this branch |
|---|---|
| Logical channel (`streaming.rs`, `wal_store.rs`, `wal_applicator.rs`) | Forwards broadcast entries; `WalStore` never receives them (`streaming.rs:473`); empty batch ends catch-up (`:656`); `Verified` certifies identity only; `wal/transaction.rs` appends intent BEFORE the data batch, so a failed commit can still be broadcast. Dedup by numeric LSN in RAM. Not a committed history. |
| Physical channel (`physical.rs`, `physical_source.rs`, `physical_wire.rs`, `physical_receiver.rs`, `checkpoint_transfer.rs`) | Authenticated snapshot + raw RocksDB WriteBatch tailing, atomic `batch + receiver cursor` with `sync=true`, history id, contiguous-sequence and duplicate-digest checks, bounded polls, WAL retention config (`engine.rs:2580`). **Non-serving by contract**: "No phase declares SQL readiness or opens a serving database." Source side is wired in `main.rs:1986`; receiver is `heliosdb-nano resync`. |
| Promotion (`role_manager.rs`, `switchover.rs`, `failover_watcher.rs`, `split_brain.rs`) | State machines and a fencing-token counter in memory. No durable epoch, no enforcement at the storage or wire boundary, no accepted API. |

Astra's conclusion: the logical route needs a committed-ordinal envelope inside every managed commit and coverage of ~70 distinct writer methods (156 raw RocksDB write sites, 100 in production), and a receiver that rebuilds ART/vector/catalog/cache state. Too broad to finish safely.

## 3. Recommendation: converge on the physical channel; drop the logical channel as the recovery authority

Rationale: RocksDB's own sequence-numbered WAL already is a complete, ordered, atomic history of every writer, including sidecars, catalog, counters, DDL, bulk load and branch merge. "Complete managed writer coverage" (the main blocker) stops being a property we must prove per call site. The physical code (about 4,000 lines) already provides the safe transport and atomic apply. What it lacks is exactly: (a) a definition of a servable cut, (b) turning a staged replica into a serving engine, (c) promotion. The logical channel stays as the low-latency wakeup and for the existing sync/semi-sync ACK path, but is demoted from authority: it carries no data the receiver trusts.

### 3.1 Servable cuts

A multi-batch semantic operation (DROP TABLE writes at `catalog.rs:1631` and `:1703`, ALTER ADD/DROP COLUMN, branch merge, vector snapshot removal, bulk load) is atomic per RocksDB batch but not as a whole. Applying it sequentially always converges on the primary's state; serving in the middle shows what the primary itself would show after a crash at that point.

Decision proposed: the standby publishes (serves) only at **cut markers**.
- Primary keeps a process-wide `OpenMultiBatchOps` gauge. Only the ~15 methods that issue two or more writes for one statement register (RAII guard). Single-batch writers (the bulk of the 100 sites, including all fast-path DML and `commit_with_timestamp`) need nothing.
- A marker is a tiny batch `repl:cut:{n}` written when the gauge is 0 (checked under the same short lock that registers guards, so a registration cannot slip between check and write). Emitted when dirty, at most every `cut_interval_ms` (default 200), and on demand before a switchover.
- Failure mode of a missed registration is bounded: the standby serves a state equal to a primary crash-recovery state, never a state the primary could not produce. That residual is stated in the docs, and covered by an enumerated list plus a test that asserts every `write*` site with 2+ writes per statement is registered (a source-scan test, so a new writer fails CI instead of silently opting out).

### 3.2 Serving a staged replica

Reuse the engine open path rather than mutating a live engine. Receiver applies to its staged RocksDB; at a cut marker it publishes a generation:
1. Take an immutable RocksDB checkpoint (hard-link) of the staged DB at the marker sequence.
2. Open a read-only `StorageEngine` over that checkpoint (catalog, ART, vector indexes, row cache hydrate exactly as on a normal open, TDE key validated first as at `ea1fa460`).
3. Swap it into the serving handle (`ArcSwap`); new sessions bind to the new generation, in-flight sessions finish on the old one; old generation is closed when its last session drains. Generation id is reported in `/health`.
- This removes the "partial live index mutation" hazard by construction: no live engine is ever mutated.
- Cost: hard-link checkpoint plus a reopen per generation. Publication is throttled (`publish_min_interval_ms`, default 1000) and skipped when no new cut arrived. Reopen time for large data dirs is the main risk and the first thing to measure (see §6, step 3).
- Readiness: `ready` only after (a) first publication whose marker sequence >= the source's latest sequence as pinned at connect time (the catch-up barrier), and revoked on disconnect, gap, apply error or retention loss. Reads on a revoked standby fail with a typed error; they do not serve stale data silently.

### 3.3 Catch-up semantics

- Cursor = `{history_id, source_sequence, batch_digest}`, already durable and atomic with the batch.
- Source retention: replication-enabled primaries must run with the RocksDB WAL enabled (refuse start otherwise) and retention TTL/size from config. Missing history is a typed `FullResyncRequired` terminal state (existing checkpoint transfer then rebuilds the standby), never an empty final batch.
- Sync / semi-sync ACK: ACK identity becomes the physical `{history, sequence}` durably applied on the standby, not the numeric logical LSN. The commit-side wait uses the source sequence returned by the write (RocksDB `latest_sequence_number` after the batch).
- Transport: physical wire is already versioned (`NPHYS001`, v1) and token-authenticated; the native v3 channel keeps only wakeups. Mixed-version peers refuse rather than fall back.
- Existing standbys with only logical state have no certified cursor: refuse incremental continuation, keep their data, require an operator-selected physical bootstrap (documented upgrade note).

### 3.4 Fenced promotion

Primary identity is an **epoch** stored durably in the dataset (`repl:epoch`, written in the same batch family as everything else, so it replicates).
- `heliosdb-nano promote` on a standby: (1) refuses unless ready and caught up to a fresh barrier from the old primary, or the operator passes `--force` (data loss possible, printed and logged); (2) closes the receiver; (3) writes `epoch+1` with `sync=true`; (4) opens the staged DB read-write as the new primary (a new history id is NOT created; history continuity is the point).
- Fencing the old primary, in decreasing strength:
  1. Graceful: old primary reachable, `promote` first sends it `Fence(epoch+1)`; it flips read-only, flushes, reports its final sequence; the standby must have applied that sequence before step 3.
  2. Wire-level: the physical and native handshakes carry the epoch. Any receiver or client that has seen a higher epoch refuses a lower-epoch source. A stale primary that reconnects to anything in the cluster learns it is stale and self-fences (read-only, health reports `fenced`).
  3. Unreachable old primary: not safely auto-detectable without a third-party arbiter. Documented: promotion with `--force` is an operator attestation that the old primary is down or isolated (STONITH/lease is the operator's job). **No automatic failover in 4.41.1**: `failover_watcher` stays advisory/off by default. If the owner wants automatic failover, that needs an arbiter (witness node or external lease) and is a separate item.
- Old primary rejoin: after fencing it must be rebuilt as a standby from a checkpoint of the new primary if its sequence is ahead of the divergence point; automatic rewind is out of scope.
- Routing: standby write attempts keep returning the existing read-only error; no write forwarding added.

## 4. Alternatives considered

| Option | Verdict |
|---|---|
| Logical committed-ordinal envelope in every commit (Astra route 1) | Correct in principle but needs all ~70 writers inside one funnel and a live receiver rebuilding indexes. Weeks, high regression risk to ordinary commit latency (already over the performance gate). Rejected for 4.41.1. |
| Physical tailing with live apply into the serving engine | Fast, but live ART/vector/catalog/cache mutation under running sessions is the unsolved hazard. Rejected in favour of generation swap. |
| Physical + cut markers + generation swap (this doc) | Reuses ~4,000 reviewed lines; correctness argument rests on RocksDB's WAL, not on enumerating writers (only multi-batch ops need registration). Recommended. |
| Ship containment only (typed refusal where catch-up cannot be certified) | Honest, small, already exists as a separate candidate. It is the **fallback** if the owner's "ships WITH replication" cannot be met by the end of the window. |

## 5. Risks and open questions for the owner

1. **Reopen cost** per generation on big datasets: unknown until measured. Mitigation: larger publish interval; reads are eventually consistent within it, and the staleness bound is reported.
2. **Encrypted datasets**: raw batches are ciphertext; standby needs the same key. Key rotation mid-stream is unsupported in 4.41.1 (refuse).
3. **Embedded explicit transactions emit no logical WAL** (known HA gap from the wave campaign). Physical tailing closes this as a side effect; confirm by test.
4. **Performance gate** is already failing (encrypted startup +15%, 14/44 metrics at least 3% worse). This design adds one gauge guard on ~15 DDL/bulk paths and nothing on the DML hot path; confirm with the A/B harness.
5. **Scope of "fenced"**: is the owner satisfied with operator-attested promotion plus epoch refusal (no automatic failover)? This is the biggest decision.
6. Sync-mode ACK change alters commit latency semantics for `sync`/`semi-sync`; needs its own measurement.

## 6. Work plan (each step: implementation, adversarial review, tests, gate; one commit each)

1. **Physical-only recovery truth** (smallest, first): wire `FullResyncRequired`, mandatory WAL-enabled primary check, catch-up barrier, replace logical-LSN ACK identity. Reproduce the `[1,4]` vs `[1,2,3,4]` defect as a red test first, then green.
2. **Cut markers**: gauge + guards on the enumerated multi-batch writers, marker writer, source-scan test.
3. **Generation publication**: checkpoint + read-only engine open + swap + drain; readiness gating; measure reopen cost at 10 MB / 1 GB / 10 GB; wire into `start --replication-role standby`.
4. **Epoch and fencing**: durable epoch, handshake enforcement, `Fence` RPC, `promote` command, rejoin refusal.
5. **End-to-end acceptance on real daemons** (restart gap, crash before/after batch, retention loss, duplicate/conflicting batch, kill during apply, broadcast loss, stale primary after promote, encrypted dataset, warm/cold/PK/secondary/catalog reads, perf A/B).

Steps 1-2 are independent of the owner's §5 decisions and can start immediately. Steps 3-5 start after items 1 and 5 are answered.

## 7. Step 1 as implemented (deviation from §3.3, read this)

Step 1 closed the silent restart gap on the **logical** channel, using the primary's persisted `wal:entries:*` as the durable source, because that was the smallest change that makes the real-daemon defect go green and fail closed. The physical-channel authority in §3 is still the target for steps 2-4; nothing here blocks it.

Delivered:
- **Standby durable cursor** `repl:applied_lsn`, written in the same RocksDB batch as each replicated row (DDL/counters: applied, then cursor fsynced; a crash between them re-delivers and a non-idempotent DDL fails closed). The applicator and the handshake both start from it, never from RAM.
- **Primary plans catch-up from durable history** (`CatchupSource` / `StorageCatchupSource`), after the connection has subscribed to the live broadcast, so nothing falls between the two; duplicates are dropped by the cursor. The old plan used a never-fed `WalStore` and a `current_lsn` stuck at 0.
- **Typed refusal**: history below the checkpoint, a standby ahead of the primary, or no logical WAL gives a rejected handshake carrying `full resync required`, terminal on the client. An empty batch is never the answer to an unservable position.
- **Catch-up barrier**: every catch-up ends with a final batch that fixes the barrier; the standby reports `CaughtUp` only once its durable cursor reaches it, and `start` waits for `CaughtUp` (not merely `Verified`) before opening the SQL listener.
- A lagged live feed now disconnects the standby so it re-syncs from durable history (it used to log a warning and skip the entries).
- A fresh standby joining after the data/DDL exist now receives the retained history (it used to crash with `Table 't' does not exist`); if that history was checkpointed away it gets the refusal instead.

Not delivered by step 1 (still open, tracked above):
- ~~The logical WAL is appended before the data batch commits~~ — closed in section 8.
- LSNs are not a commit order and can have holes; catch-up trusts the source, it cannot verify contiguity.
- ACK identity is still the numeric LSN (`Received`), and `report_applied` has no production caller.
- Native protocol version is not bumped: a new standby against an old primary never sees a barrier and times out (fail closed), an old standby against a new primary ignores the barrier.
- `replay_from` loads the whole retained range into memory; fine under the default checkpoint policy, needs chunked iteration before long retention is allowed.
- Column-storage tables are refused on the standby (pre-existing).

## 8. Append-before-commit hole (closed for the commit paths; read the residuals)

The logical WAL used to be appended, made durable and broadcast BEFORE the data write it described, in three places. A failed write, or a statement that logged and then rolled back, left history for data that never committed, and a standby applied it.

Changes:
- **Transaction commit** (`transaction.rs`): the entries are sealed into the commit's own RocksDB batch (`WriteAheadLog::stage`) and written with the data in one write; they are published only after that write succeeds (`publish_staged`) and become holes if it fails (`abandon_staged`). Data and history are atomic in both directions (no entry without data, no data without entry). Strict `logical_wal_per_statement` forces that single write to be synced.
- **Autocommit statements**: the `log_data_*`/`log_*` calls made while a statement runs are buffered in a statement-scoped frame (`statement_capture`, opened by the five autocommit wrappers in `lib.rs`) and written with that transaction's commit batch; a rollback drops the frame. Outside a frame (any caller that has none) appends behave exactly as before.
- **`StorageEngine::delete`** of a `data:` key writes the delete and its entry in one batch (`append_with_batch`).
- **Ordered publication**: entries reach standbys strictly in LSN order and only once every lower LSN has resolved; `published_lsn()` is the watermark and catch-up never reads above it. Without this, two committers finishing out of order let a standby move its cursor past an entry that was still in flight and then drop it as "already applied". `increment_lsn` now reserves and skips, so counter bumps are known holes instead of gaps that would stall publication.
- The group-commit thread resolves through the same publisher; a failed batch publishes nothing.

Tests: failed multi-row `INSERT` leaves no entries (red on the previous code: phantom entries for the rows before the violation), publication waits for the lowest unresolved LSN, abandoned entries leave no history and do not stall later ones, capture defers and discards, data+entry are one write, and a real-daemon check that rolled-back and failed statements never reach a standby.

Residuals (not closed here):
- Calls to `log_*` made OUTSIDE a capture frame still append immediately and in the old order. The sweep found 24 non-data `log_*` call sites (`lib.rs` 8, `sql/executor/ddl.rs` 7, `storage/catalog.rs` 7, others 2). They are covered only when the statement runs through an autocommit wrapper. Not covered: **DDL issued over a wire session** (a session transaction collects row changes at commit but does not frame `log_*`), the autocommit fast insert/update/delete paths that open no transaction (they log after their own write, so they cannot publish phantom rows, but a crash between the two loses the entry), and work on threads other than the statement's (the frame is thread-local). Moving session DDL into the commit batch is the next item.
- Autocommit DDL is now logged at commit, after the catalog change: a crash between the two leaves the primary with a table the standby never receives (previously the reverse: the standby could get a DDL the primary lacked). Neither is acceptable long-term; the physical channel removes the question.
- Sync/semi-sync ACK identity is still the numeric LSN, and group-commit ordering when `sync_commit` is on was not re-measured.
- No performance measurement yet. The transaction commit now issues one write instead of two when the WAL is enabled, so it should not regress, but that is unmeasured; the A/B harness must confirm.
