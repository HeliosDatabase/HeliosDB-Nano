# Spec P5.1 — The epoch made real: `heliosdb-nano promote`, fencing, stale-node recovery

Parent: `PHYSICAL_STANDBY_REPLICATION.md` §D7 ("Promotion (epoch)"), §D8 (Proxy contract),
decisions 9 and 10; `REPLICATION_4_41_1_DESIGN.md` §3.4 (fenced promotion). Sprinter
`8f0721af1a70`. After P4 (merged `18085df7`). Implementation agents never run cargo or git.

P5 is split: **P5.1 (this spec)** makes the epoch a stored value, adds fencing on the wire, the
offline `heliosdb-nano promote` command and the recovery routes for nodes left behind by a
promotion. **P5.2** (next spec) adds the online form, `SELECT pg_promote(wait, seconds)` on a
running standby, which reopens the store in-process and needs push-style session termination.
Both share one conversion function written here. The third P5 bullet of the plan, removal of the
native v3 listener, was done in P2.1. The fourth, the HNSW graph dump moved into RocksDB, is
decided separately (Sprinter note on `8f0721af1a70`): the standby already starts with no files
outside RocksDB, so the dump stays a primary-local cache.

Value: today a standby can never become the primary (the SQL `SWITCHOVER` always refuses, and
there is no `promote`). After P5.1 an operator promotes a stopped standby with one command that
fences the old primary when it is reachable, loses nothing in that case, and leaves every other
node in a state the existing `recover --from-primary` repairs. The epoch is Oracle's incarnation /
PostgreSQL's timeline: it is what lets a proxy, a standby and an archive tell a superseded primary
from the real one.

## Facts the design rests on (verified on `18085df7`)

- The epoch is the compile-time constant `standby_posture::EPOCH = 1` (`standby_posture.rs:81-84`),
  read at ~18 sites: `standby_posture.rs:414,806,915`; `main.rs:4425` (standby Hello), `:7326`
  (primary identity), `:7565` (cascade), `:7862` (resync); `archiver.rs:941`;
  `archive_recovery.rs:65,181,522`; `physical_source.rs:54,331,743`; `recover_primary.rs:72,457`;
  `system_views.rs:6356-6358` (`pg_control_checkpoint`). Nothing writes, stores or compares an
  epoch other than by equality.
- The wire already carries it: `Hello.epoch` (`physical_wire.rs:558-570`, 0 rejected), `HelloOk.epoch`
  (`:627-639`), `RefuseCode::EpochStale{have,want}` id 5 (`needs_recovery`), `Kind::Fence = 20` with
  `Fence{epoch}` (`:775-797`), `CAP_FENCE = 1<<1` never offered (`:146`). Both ends refuse a Fence today
  (`:1982-2006` receiver, `:2595-2605` source); the two epoch checks are strict `!=` (`:2300-2309`
  source, `:2656-2664` client). `OpenRequest` carries no epoch.
- The archive carries it: segment header field `epoch` (`archive_segment.rs:172-185`), directory
  layout `<archive>/<history>/<epoch>/…` (`:334-336`), the catalog groups by (history, epoch)
  (`archive_catalog.rs:192-216`), the chain reads one epoch only (`archive_chain.rs:22,37-41`).
  Decode accepts epoch 0 (`:242-244`).
- History identity is a file: `NANO-PRIMARY-HISTORY.json` `{format, history_id, storage_identity}`
  written by `load_or_create_primary_history` (`history.rs:202-232`), which MINTS a new id when the file
  is absent. `write_new_record` is private (`:412-433`). A standby directory carries the marker
  `NANO-PHYSICAL-REPLICA.json` and manifest `NANO-PHYSICAL-MANIFEST.json`
  (`checkpoint_transfer.rs:45-46`), the local column family `__nano_local` with `cursor`,
  `ff:dirty`, `ff:done:*` (`physical.rs:44-62`), and the primary's `IDENTITY` byte for byte.
- The engine refuses to open a store holding `__nano_local` in any mode but `PhysicalStandby`
  (`engine.rs:2556-2592, 2950-2958`) and a marked directory in any non-physical mode (`:2961-2970`);
  a standby start refuses a directory carrying the primary history file (`main.rs:4097-4103`).
  The write gate's mode is fixed for the engine's lifetime (`write_gate.rs:41-54`); the comment
  there reserves promotion as "the first transition, made here, in one place".
- A standby store's RocksDB sequence runs ahead of the primary's numbering by about one per
  applied group (the cursor put rides in each write, `physical.rs:1361-1376`). A promoted store
  therefore continues in ITS OWN numbering; the old primary's other standbys hold cursors in the old
  numbering and cannot tail the new primary. They are repaired by fast-forward (§6), which never
  uses the old cursor's numbers (it re-cuts at the new primary's checkpoint).
- `recover --from-primary` maps `HistoryMismatch | EpochStale` to the full-copy route
  (`recover_primary.rs:700-702, 765-770`). `strip_node_local_family`
  (`checkpoint_transfer.rs:486-546`) is the working precedent for turning a standby store into a
  primary's. The write-funnel test (`tests/write_gate_funnel.rs`) allows raw writes only in
  `write_gate.rs`, `physical.rs`, `checkpoint_transfer.rs`: the conversion writer lives in
  `physical.rs`.
- Opening a store as a primary runs `recover_wal_at_open` over the logical WAL keys the old
  primary replicated (`engine.rs:3342-3348`): a promoted store must not replay the old primary's
  logical entries (§4 step 5).
- Proxy (coordination note, `proxy-to-nano-physical-standby-20261020.md`): picks the strictly
  highest `pg_control_checkpoint().timeline_id` when two nodes claim to be writable; a tie or unknown
  authorises nothing. Fencing/demotion should terminate sessions with FATAL `57P01`. The only 57P01
  today is lazy (next statement, `handler.rs:1850-1869`); push termination is P5.2.

## 1. The epoch as a stored value

- **Key** `repl:epoch` in the default (replicated) column family. Value: JSON
  `{"format":1,"epoch":E,"previous":P,"promoted_at":<unix secs>,"node_id":<uuid>,
  "fenced":true|false,"old_primary_final_sequence":F|null}`, ≤ 1 KiB. Absent key = epoch 1.
  `KeyClass::ReplEpoch` in `keyspace.rs` (`FIXED_KEYS`), a literal for
  `tests/keyspace_classification.rs`, and an applier arm in `physical_applier.rs` that decodes the
  value, updates the process epoch (§1 below) and logs at INFO; no derived state.
- **Reading.** `StorageEngine::replication_epoch() -> u64` reads the key at open (every open mode,
  including the read-only opens `recover` and the archive tools use) and caches it; the applier arm
  and the conversion update the cache. `standby_posture::EPOCH` is deleted; `INITIAL_EPOCH: u64 = 1`
  remains only as the absent-key default and the archive tools' fallback for a directory with no
  store. Every site listed in the facts reads the epoch from the store it has opened (primary
  identity, standby Hello, cascade identity, resync, archiver header, archive readers, recover,
  `pg_control_checkpoint`, the posture blocks). `prev_timeline_id` = `previous` (1 when absent).
- **Where it shows:** `/health` and `heliosdb_standby_status` field `epoch` (frozen field set,
  value now real); `pg_control_checkpoint().timeline_id`; `heliosdb-nano status` prints
  `epoch`; `heliosdb-nano archive list` already prints the epoch directory.
- Segment decode rejects epoch 0 (`archive_segment.rs:242`).

## 2. Epoch policy on the wire

Replace the two equality checks with a direction-aware rule. Same history is checked first, as
today.

| side | comparison | action |
|---|---|---|
| source | `hello.epoch < identity.epoch` | refuse `EpochStale{have: ours, want: theirs}` (id 5, needs_recovery), reason "the receiver follows epoch W of this dataset; this source is at epoch H: run `heliosdb-nano recover --from-primary`" |
| source | `hello.epoch > identity.epoch` | **self-fence** (§3) with `fenced_by = hello.epoch`, then refuse **`SourceStale{have: ours, want: theirs}`** (new id 10, retryable, not needs_recovery) |
| client | `hello_ok.epoch > hello.epoch` | `EpochStale` as today (the receiver is behind an epoch; needs_recovery) |
| client | `hello_ok.epoch < hello.epoch` | `SourceStale` (the configured source is a superseded primary; the receiver stays `reconnecting` with that reason and keeps retrying so a config fix takes effect; WARN once per epoch) |

`RefuseCode::needs_recovery()` = {2,3,4,5,8,9}; `retryable()` = {6,7,10}. `recover --from-primary`
maps `SourceStale` to an error ("the primary at H is a superseded primary (epoch X < this standby's
Y); point --primary-host at the current primary") and `EpochStale` with the same history to the
fast-forward route (§6).

## 3. Fencing

### 3.1 On the wire

- Sources offer `CAP_FENCE` in `HelloOk.capabilities`. A receiver may send `Fence{epoch}` after
  `HelloOk` and before `Open`, or between polls of a resume/snapshot session (keep it out of the
  fast-forward allowed list). The source answers with a new frame **`Fenced{epoch, final_sequence}`**
  (`Kind::Fenced = 29`, 16 bytes) once §3.2 has completed, or `Refuse(Version)` when
  `fence.epoch <= identity.epoch` ("a fence must name a higher epoch than this source's E").
  A second `Fence` with the same or a lower epoch than the one already applied answers the same
  `Fenced` (idempotent). After `Fenced` the session continues: the fenced source keeps serving polls,
  snapshots and fast-forwards (the promoting standby needs them), and keeps serving reads on the
  PG port.
- The receiver side (`physical_wire.rs` client): `fence(epoch) -> Fenced`. `read_reply` no longer
  turns an inbound `Fence` into a refusal (sources never send one).

### 3.2 On the node (`fence_self(by_epoch)`, one function in `physical_source.rs` or a new
`fence.rs`)

1. **Close the write gate.** `WriteGate` gets an atomic fenced state (`fence(reason)`), checked in
   `check()` after the mode: this is the one place the plan's comment reserved. Refused writes say
   "write refused: this primary was fenced at <time> by epoch E+1 (a standby was promoted); it
   serves reads only — rebuild it as a standby with `heliosdb-nano recover --from-primary
   --primary-host <new> --replace`". The SQL layer maps it to SQLSTATE `25006` like the standby
   refusal (`handler.rs:5571-5575`, `error.rs:119`), additive message.
2. `ha_state().set_read_only(true)`; posture: `install_primary` status gains `fenced_by: Option<u64>`
   and `fenced_at`; the block's `state` reads `fenced` (additive value; the field set is unchanged);
   `/ready` answers 503 while fenced; `heliosdb_require_ready()` raises 55000 ("fenced").
3. Wait for in-flight writes (the gate refuses new ones; take the engine's write guard or the
   group-commit drain the shutdown path uses), `flush_wal(sync=true)`, read
   `latest_sequence_number()` = `final_sequence`.
4. Write **`NANO-FENCED.json`** `{format:1, history, epoch, fenced_by_epoch, fenced_at, by_node}`
   in the data directory root, `create_new`, fsync file and directory (reuse `history.rs`'s
   `write_new_record` by making it `pub(crate)`). Durable: `start` in ANY role refuses a directory
   carrying it: "this directory is a primary that was fenced at <time> by epoch E+1; it cannot be
   started; rebuild it as a standby: `heliosdb-nano recover --from-primary --primary-host <new>
   --replace --data-dir D`" (exit 2). `recover --from-primary --replace` accepts it as route 3
   (full copy, old directory kept as `<D>.replaced-<utc>`); without `--replace` exit 3 as today.
5. Sessions: in P5.1 the lazy mechanism (`reject_guarded_standby_statement` path) sends FATAL
   `57P01` "terminating connection: this primary was fenced by epoch E+1" to every session created
   before the fence, at its next statement; new sessions are admitted with `in_hot_standby=off`
   and read-only. Push termination arrives with P5.2.
6. The primary's archiver seals its open segment and stops (no more writes to archive); the
   physical source keeps serving. Log one ERROR line with the epoch, the fencing node and
   `final_sequence`.

Triggers of `fence_self`: an inbound `Fence` (§3.1) and a `Hello` with a higher epoch (§2). The
process-level flag is also consulted by `heliosdb-nano status`.

## 4. `heliosdb-nano promote` (offline, on a stopped standby directory)

```
heliosdb-nano promote --data-dir D [--config F] [--primary-host H[:PORT]] [--token-file T] [tls flags as recover]
                      [--fence-timeout SECS=30] [--force] [--then start|stop (default stop)]
```

`main.rs` `Commands::Promote`, `run_promote` → `src/replication/promote.rs` (new). The
primary address and token default to the standby's config (`[replication] primary_host`,
`token_file`), as `recover --from-primary` does; reuse its connection code.

Steps, each logged on one line:

1. **Classify.** `D` must be a physical standby directory (marker + `__nano_local`), not running
   (take the RocksDB `LOCK` as the receiver would; a held lock → "the standby is running: stop it
   first (`heliosdb-nano stop`), or promote it online with `SELECT pg_promote(true, N)` once
   P5.2 ships", exit 2). Refuse `ff:dirty` ("an interrupted fast-forward: finish it with
   `recover --from-primary` first", exit 2). Verify the stage and cursor as `recover` does
   (`ReopenedPhysicalStage::open`, `LocalWriter::resume`). Read history `H`, epoch `E`, cursor `C`.
2. **Fence the old primary.** Connect as a receiver (Hello{H, E, cursor}), send `Fence{E+1}`,
   receive `Fenced{final}`. Then open a resume session after `C` and apply through `final` with the
   offline tail the recover ladder already has (route 1), so nothing the old primary committed is
   lost. If the primary refuses with `SourceStale`/`EpochStale`, or `HelloOk.epoch > E`, stop:
   another node was already promoted (exit 2, name it). If the primary is unreachable within
   `--fence-timeout` (connect/handshake), or the tail cannot reach `final` within it: without
   `--force` exit 2 with "the primary at H did not answer the fence within 30 s; if it is down or
   isolated, re-run with --force (writes committed on it after sequence C are lost)"; with
   `--force` continue, `fenced=false`, and print the same sentence as a WARNING.
3. **Dirty record** `NANO-PROMOTION.json` `{format:1, history:H, from_epoch:E, to_epoch:E+1,
   started_at, fenced, old_primary_final_sequence}` written `create_new` + fsync before any store
   change. While it exists, `start` (any role), `recover` and `resync` refuse the directory:
   "promotion interrupted: finish it with `heliosdb-nano promote --data-dir D`" (exit 2). A re-run
   of `promote` finds the record, skips steps 1-2 and resumes at 4 with its values (idempotent:
   each step checks before acting).
4. **Conversion** (`physical.rs`, `PromotionWriter`, the funnel-allowed module), opened on the
   store with both column families:
   a. put `repl:epoch` (value per §1) with `sync=true` — the first write of the new epoch;
   b. logical WAL: the promoted store must not replay the old primary's `wal:*` entries at its
      first open. Inspect `recover_wal_at_open`'s decision table and make the durable checkpoint
      watermark equal to the head (or delete the entries if the table replays unconditionally);
      the logical LSN counter continues from the inherited watermark so no LSN is reused;
   c. `drop_cf("__nano_local")` (cursor, `ff:done:*` go with it);
   d. `flush_wal(sync)`, close the handle.
5. **Identity files:** write `NANO-PRIMARY-HISTORY.json` with the INHERITED `history_id = H` and the
   store's `storage_identity` (new `history::write_primary_history(path, history_id)`), remove
   `NANO-PHYSICAL-REPLICA.json`, `NANO-PHYSICAL-MANIFEST.json` and their `.tmp` siblings, fsync the
   directory.
6. Remove `NANO-PROMOTION.json`, fsync. Print:
   `promote: D is the primary of history H at epoch E+1 (from E; the old primary at H:P was fenced
   at its sequence F and everything through F is here | NOT fenced (--force): writes after
   sequence C are lost); start it with heliosdb-nano start --replication-role primary --data-dir D`.
   `--then start` re-enters `run(cli)` with the primary role as `recover --then primary` does.

The first open as a primary: `prepare_physical_source` finds the history file and keeps `H`;
`replication_epoch()` = E+1; the primary archiver writes `<archive>/<H>/<E+1>/` starting at the
store's current sequence (the `repl:epoch` write is inside the first segment); the old
`<H>/<E>/` segments stay until the recovery window prunes them. No new history id is ever minted
by a promotion (a test pins it).

## 5. What the other nodes see

- **Standbys of the old primary**, still connected to it: the fenced primary keeps serving them;
  when reconfigured to the new primary they are refused `EpochStale` → `needs_recovery` with the
  exact command: `recover --from-primary --primary-host <new>`.
- **The old primary**, if it was fenced: read-only, `/ready` 503, state `fenced`, start refused
  after a stop. If it was NOT fenced (`--force`, partitioned): it fences itself the moment any
  node of epoch E+1 talks to it (a receiver's Hello, §2); until then Proxy disambiguates by the
  highest timeline. Documented as the operator's STONITH responsibility (plan §3.4 item 3).
- **Cascading downstreams** of the promoted standby: its cascade source stops with the standby
  process; when they reconnect to the new primary they are `EpochStale` → recover (fast-forward,
  cheap: same data).

## 6. `recover --from-primary` learns epochs

- `Probe`: `EpochStale{have > want}` with the SAME history → route 2 (fast-forward) instead of
  `Mismatch`. The fast-forward re-cuts the standby at the new primary's checkpoint `T` and writes the
  cursor `{T,T}` in the new numbering; leaves the standby had beyond the promotion point differ and
  are replaced (rows the old primary committed after the promotion vanish — the recover output
  says so when `old_primary_final_sequence` is known from the new primary's `repl:epoch`).
  `HistoryMismatch` stays route 3.
- A fenced-primary directory (`NANO-FENCED.json`) or a primary directory of the same history that
  is NOT fenced (operator rebuilding a dead primary) → route 3 with `--replace`; without
  `--replace` exit 3 naming the file. (Rewinding a primary through route 2 is a follow-up, filed.)
- `recover --from-archive` is unchanged (single epoch); its error for a chain whose segments are at
  another epoch than the standby names both epochs.

## 7. SQL and SWITCHOVER

`SWITCHOVER TO` / `SWITCHOVER CHECK` keep refusing (plan: until fencing is proven live), but the
message now points at `heliosdb-nano promote` and `SWITCHOVER CHECK`'s blockers list says
"use `heliosdb-nano promote` (offline) or `SELECT pg_promote(true, N)` (P5.2)". `pg_promote` is
NOT defined in P5.1 (it keeps failing with 42883; the P5.2 spec defines it). `pg_control_checkpoint`,
`heliosdb_standby_status`, `/health` show the stored epoch.

## 8. Tests (proof-first: each new test red on `18085df7`)

Unit (`physical_wire.rs`, `keyspace.rs`, `write_gate.rs`, `promote.rs`):
- `Fenced` codec round trip; `SourceStale` codec; `needs_recovery`/`retryable` tables.
- Hello policy table (§2) on an in-process source/receiver pair for all four rows.
- Write gate: `fence()` makes `check()` fail with the fenced message; `is_open()` false; refusal
  counted; idempotent.
- `repl:epoch` value encode/decode, absent = 1, format rejected ≠ 1.

Integration (`tests/physical_promote.rs`, real daemons via `tests/support/physical_pair.rs`):
1. **Graceful promotion**: primary P + standby S + standby S2; load; stop S; `promote --data-dir S`
   → P is fenced (writes on P fail 25006 with the fenced text, `/ready` 503, `/health` state
   `fenced`, `NANO-FENCED.json` present, a session opened before the fence gets FATAL 57P01 at its
   next statement); S holds everything through P's final sequence (row counts + `range_digest`
   equal to P's); S starts as primary: `pg_is_in_recovery()=f`, `in_hot_standby=off`,
   `pg_control_checkpoint().timeline_id = 2`, `prev_timeline_id = 1`, history id unchanged,
   `/health epoch=2`; writes on S succeed; S's archive has `<H>/2/` segments.
2. **S2 recovers from the new primary**: S2 (cursor in P's numbering) pointed at S → `needs_recovery`
   with the EpochStale reason; `recover --from-primary` takes route 2 (output says fast-forward),
   S2 streams from S afterwards, rows equal, `/health epoch=2`.
3. **Fenced primary cannot start; rebuilt as a standby**: `start` on P's dir exits 2 with the
   rebuild hint; `recover --from-primary --primary-host S --replace --data-dir P` → full copy,
   P follows S, `<P>.replaced-<utc>` kept.
4. **Forced promotion**: P SIGKILLed; `promote` without `--force` exits 2 within the fence timeout;
   with `--force` succeeds, `fenced=false` in `repl:epoch`; P restarted (old dir, epoch 1) and a
   receiver of epoch 2 pointed at it → P self-fences (`NANO-FENCED.json`, state fenced) and the
   receiver reports `SourceStale` while staying `reconnecting`.
5. **Interrupted promotion**: `promote` SIGKILLed after the dirty record (env hook, like
   `KILL_MID_BATCH_ENV`) at each of steps 4a, 4c, 5 → `start` refuses with the record's message;
   re-run finishes; final state identical to test 1 (epoch 2, history H, no local CF, no marker).
6. **Running standby refused**: `promote` on a live standby's directory exits 2 with the stop hint
   and changes nothing.
7. **Double promotion refused**: after test 1, `promote` on another standby S3 that still follows
   the fenced P: P answers the second `Fence{2}` with `Refuse(Version)` because it is already fenced
   by epoch 2 (a fence must name a higher epoch than any fence already applied), and `promote` exits
   2 with "P was already fenced by epoch 2 (node …): a primary at epoch 2 exists; recover S3 from
   it instead". S3 is unchanged.
8. **Logical WAL not replayed**: a table with a trigger/sequence-bearing insert workload on P
   before promotion; S promoted; S's first open replays nothing (row counts unchanged, sequence
   values continue) — assert through `heliosdb_capability_report`/logs or a direct count.
9. `tests/promotion_refusal_tests.rs`: `SWITCHOVER` still refuses with the new hint.
10. Archive: `archive list` on S after promotion shows `<H>/1/` and `<H>/2/`; `recover --from-archive`
    of an epoch-1 standby from the epoch-1 segments still works; the chain refuses epoch-2 segments
    for an epoch-1 standby with a message naming both.

Existing suites to keep green: `physical_*`, `ha_integration`, `keyspace_classification`,
`keyspace_wal_corpus`, `write_gate_funnel`, `physical_standby_refusals`.

## 9. Docs

`docs/guides/physical-standby.md` new section "Promotion and the epoch" (Oracle mapping table:
incarnation = epoch, `FAILOVER TO` = `promote --force`, `SWITCHOVER` = graceful promote;
PostgreSQL mapping: timeline = epoch, `pg_promote` = P5.2, `pg_rewind` = follow-up),
`docs/guides/replication-history-guard.md` (fenced file, promotion record, refusals),
`docs/guides/deployment-endpoints.md` (`state=fenced`, `/ready` 503), `docs/guides/ha_testing.md`
(the promote drill), `config.example.toml` (nothing new in P5.1), skill
`.claude/skills/heliosdb-nano-server/SKILL.md` + `_index/verb-map.md` (`promote`), CHANGELOG
(4.42.0 unreleased: the command, the wire additions `Fenced`/`SourceStale`/`CAP_FENCE`, the
`NANO-FENCED.json`/`NANO-PROMOTION.json` files, the epoch now stored; protocol name stays NPHYS003
since no existing frame changed shape — bump to NPHYS004 only if `Hello`/`HelloOk` change).

## 10. Gate

Pre-commit gate (`p41gate.sh`-style: check, new tests, daemon, physical, internal subsets, lib,
bin, full lib, fmt, clippy new-findings), lint, phase gate, pg35, perf smoke, live probe
`jlive/promote_probe.sh` (P + S + S2 on real daemons, 1 GB dataset: graceful promote with timing of
each step, S2 fast-forward, fenced P rebuilt, forced promote after SIGKILL of P, kill mid-promotion
and re-run), field-J probe unchanged.
