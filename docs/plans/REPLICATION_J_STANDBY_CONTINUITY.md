# Field finding J — a primary restart must not break the HA pair

**Status:** J1 in progress (branch `fix/replication-retention-j`, base `4af19e1c` = v4.41.1).
J2 and J3 are designed here but not started. Sprinter: `58d4e3113e56` (J), with
`36b56ac796e2` (retention), `551a9cdd2e0b` (serving rebuild), `bbba90ddfc61` (promotion).

## How this document was built

Read against v4.41.1 (`4af19e1c`): `src/storage/wal.rs` (`truncate_to_checkpoint`,
`checkpoint_lsn`, `replay_from`), `src/storage/engine.rs` (`recover_wal_at_open`,
`logical_wal_catchup`, the WAL checkpoint policy at open), `src/replication/streaming.rs`
(`StorageCatchupSource`, the handshake refusal), `src/main.rs` (`start_server`, replication
config resolution). The field report is `v4.41.1-findings-H-I-J.md` §J (2026-10-16).

## 1. The failure

Field sequence on 4.41.1: healthy pair; stop and restart the primary; the standby logs
`full resync required: primary history was checkpointed through LSN 77; the standby (at
LSN 76) must be rebuilt` and exits. A freshly wiped standby (LSN 0) is refused the same
way. The only recovery is to wipe both nodes and start them together.

## 2. Why

Two different facts share one marker.

- `wal:checkpoint = N` means "entries up to N are applied; never replay them as redo"
  (GH#35). It advances at every clean close and every 1000 entries / 30 s.
- `truncate_to_checkpoint(N)` also **deletes** every entry up to N in the same batch, and
  `logical_wal_catchup` refuses any standby whose position is below the checkpoint.

So the redo floor doubles as the history floor. A clean primary restart moves both to the
current LSN, and any standby that is even one entry behind, or new, can never be served.
Nothing about the GH#35 crash-replay rule requires the entries to be deleted: open-time
recovery replays only entries **above** the checkpoint (`replay_wal_after(cp)`), so entries
kept below it are never re-executed on the primary.

## 3. Plan

### J1 — keep a bounded history window below the checkpoint (this change)

- A separate history floor, `wal:pruned_through = P`: every entry with LSN <= P has been
  deleted. Stores written by 4.41.1 and earlier have no marker; for them P is the
  checkpoint (that is what they deleted).
- `truncate_to_checkpoint(N)` still records the checkpoint N, but deletes only entries up
  to `N - retention` and advances P to that bound (never backwards), atomically with the
  checkpoint. With retention 0 the behaviour is exactly today's.
- Catch-up refuses only a standby below P. The refusal names P and how to widen it.
- Retention applies to replication primaries only: `[replication]
  logical_wal_retention_entries` (default 100000), ignored unless `role = "primary"`. A
  standalone node keeps deleting everything at each checkpoint, so it gains no storage.

Effect: a clean primary restart, and any standby outage shorter than the window, catch up
without operator action. A fresh standby can join as long as the primary has never pruned
(its whole history fits the window). Beyond the window the refusal stays, as today.

Acceptance (real daemons, `tests/replication_restart_gap.rs`):
1. standby stopped, rows written, **primary restarted**, standby started -> every row;
2. primary written and restarted, then a **fresh** standby -> every row;
3. with a small window (TOML), a fresh standby after pruning is refused with "full resync
   required", while a standby whose position is inside the window catches up;
4. GH#35 redo semantics unchanged (`tests/gh_issue_35.rs`), standalone retention 0.

### J1a — a catch-up must arrive whole (`0c9e1a4faab6`, found by the J1 crash probe)

J1 makes catch-ups long: up to the window, at 500 entries per `WalBatch`. Replaying the
field sequence with a SIGKILLed primary showed a fresh standby serving 2010 of 2510 rows
after one 500-entry batch (message sequence 2 on its second connection) vanished without
an error. The cause predates J1. `ReplicationConnection::recv` read each frame with three
`read_exact` calls into locals and then awaited a lock. Both streaming loops race `recv`
against timers in `tokio::select!`, so a cancelled `recv` lost a partly read frame (the
next read started mid-frame: "Invalid magic") or, after the last read, a whole frame.

- `recv` keeps unconsumed bytes in a per-connection buffer filled with `read_buf`, and
  takes a frame out only in the synchronous step that returns it. The handshake reads go
  through the same path.
- The standby checks that data messages (`WalEntry`, `WalBatch`) arrive as 1, 2, 3, ... on
  each connection. A gap drops the connection before the next message is acted on, and the
  standby reconnects from its durable position instead of applying over the hole.

Acceptance: deterministic transport tests (a frame cut in the header, payload and checksum,
with `recv` dropped in between; 300 large frames against a 1 ms timer), a sequence-guard
unit test, and `multi_batch_catch_up_delivers_every_row` (three fresh standbys, 3000 rows,
exact counts).

### J1b — follow the slowest registered standby (next)

Persist each standby's durably applied position on the primary (from its acknowledgements,
which today carry only a numeric LSN and have no production caller of `report_applied`),
and never prune past the slowest one, still bounded by the window so a dead standby cannot
grow the log forever.

### J2 — the standby's terminal posture (needs a decision)

Today a refused standby closes its listeners and exits (history-guard contract: never keep
serving as a healthy standby). Under a supervisor that restarts it, the result is a crash
loop. Alternative: stay up, refuse SQL with a typed "needs rebuild" error, report it on
`/health`, and retry the handshake with backoff. Both fail closed; the second is easier to
operate. Not changed in J1.

### J3 — rebuild a standby whose position is gone (`551a9cdd2e0b`)

A standby below P, or a new one after the primary pruned, needs a snapshot plus the tail.
Two routes: make the physical `resync` serving (design steps 2-3 of
`REPLICATION_4_41_1_DESIGN.md`), or a logical bootstrap (a consistent snapshot read at a
published LSN, then catch-up from that LSN). Either needs the snapshot/LSN boundary to be
exact, and the field run notes the physical initial transfer is loopback-only today.

## 4. Risks

- Storage on primaries grows by up to `logical_wal_retention_entries` entries (they carry
  row images). The default is sized for tens of MB on typical rows; document sizing.
- `replay_from` loads the requested range into memory, so a standby far behind asks the
  primary for up to the whole window at once. Bounded by the window; chunking is J1b.
- A store downgraded to 4.41.1 ignores `wal:pruned_through` and deletes the window at its
  next checkpoint, which only loses history, never applied data.

## 5. Blocked on the maintainer

- J2: exit (today) or stay up as "needs rebuild"?
- J3: physical serving rebuild or logical bootstrap first?

## 6. Milestones

J1 (retention window + catch-up floor + tests) -> J1b (per-standby floor, chunked catch-up)
-> J2 (posture, after the decision) -> J3 (rebuild).
