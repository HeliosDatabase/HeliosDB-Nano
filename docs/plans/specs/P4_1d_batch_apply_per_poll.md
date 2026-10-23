# P4.1d — The standby applies a poll as one unit of work; the primary flushes its WAL only when the barrier moved

Plan: `docs/plans/PHYSICAL_STANDBY_REPLICATION.md` §D3.4 (the applier) and §D3.6 (durable acks).
Sprinter `925803b35113`. Found while measuring P4.1 (`docs/plans/specs/P4_1_recover_from_primary_fast_forward.md`).
Status: spec. Implementation agents never run cargo or git.

## 1. What was measured (release 002a8392 = P4.1b, 2026-10-23)

| setting | streaming rate | note |
|---|---|---|
| md3 disks (fdatasync median 72 ms) | ~3 MB/s of WAL | standby finished 31 min after a 3.5 min load of 1 M rows |
| /dev/shm (no fsync cost) | ~4 MB/s of WAL, ~10 000 sequences/s | so it is not the disk |
| TCP_NODELAY (P4.1b) | no change | so it is not Nagle |

Both processes look idle to a stack sampler. The applier worker thread (`nano-physical-r…`)
spends 6.3 s of every 10 s in the kernel with ~7 400 voluntary context switches per second;
each tokio worker makes ~10 000 per second. `heliosdb-nano archive list` on a primary that
ran five `INSERT INTO t SELECT i, repeat('x', 900) || i FROM generate_series(a, a+999)`
statements shows **~35 000 write groups for 5 000 rows: about 7 write groups per row** (the
executor commits a row's keys as separate groups; a 20 000-row statement, by contrast, is one
80 MiB group). The receiver hands **every group** to the worker thread as its own
`Command::Apply` (physical_receiver.rs: `Sink::apply` → `worker.request`, a oneshot reply, a
tokio wake); the worker runs `stage.verify()` (an `absolute_path` symlink walk plus reading
the marker and the manifest, checkpoint_transfer.rs ~257/~887) and one RocksDB write, one
SHA-256 check, one decode and one derived-state publish per group. About 48 µs per group,
seven groups per row, one row at a time.

The primary's `poll_source` (physical.rs ~390) and `poll_source_cascade` (~792) call
`db.flush_wal(true)` on **every** poll, even when the barrier did not move since the last one;
a clean fdatasync costs ~10 ms median on this host.

Fast-forward and the full copy are not CPU-bound (165 MB/s leaf transfer on /dev/shm): they
are fsync-bound on disk and out of scope here.

## 2. The change

### 2.1 One command per poll (physical_receiver.rs, physical_wire.rs `BatchSink`)

- `BatchSink` gains `fn apply_many(&mut self, batches: Vec<PhysicalBatch>) -> Result<()>`
  with a default that calls `apply` in order (every other sink keeps working). The client's
  poll loop (`PhysicalClient::poll`, physical_wire.rs ~3068) collects the poll's validated
  batches and calls `apply_many` once per poll **unless** their raw bytes exceed
  `APPLY_CHUNK_BYTES = 32 MiB`, in which case it hands them over in chunks of at most that
  size, in order. Validation per batch (history, contiguity, digest via
  `PhysicalBatch::decode`, barrier) stays exactly where it is, before anything is applied.
- `Command::Apply` carries `Vec<PhysicalBatch>`; the worker handles the vector under ONE
  `stage.verify()` and ONE `record_position`, applying the batches in order through the
  applier (below). The archive feed is offered each batch as today (one `offer` per batch).
- The receiver's own ack/heartbeat path is unchanged: `Command::Sync` once per poll.

### 2.2 The applier applies a run of batches as one pass (physical_applier.rs)

- `PhysicalApplier::apply_run(&self, batches: &[PhysicalBatch], durability) -> Result<Applied>`:
  hold the replay guard once; decode every batch (same `decode_raw_batch`); build ONE
  `Touched`/pre-read over the union (the pre-read of a row touched by several batches reads
  the image **before the first** of them, as a single batch does for a key it touches twice);
  write the batches **one RocksDB write per batch, in order**, through
  `LocalWriter::apply_source_with(batch, Deferred)` exactly as today (the cursor put per
  group and the `latest_sequence_number == local_sequence` invariant are untouched: no
  accounting change, no cascade change); then ONE publish pass over the union, with the
  post-run schema for `Data` decoding, the same per-class rules as `apply_with`. `apply_with`
  becomes `apply_run` over one batch.
- Invariant kept: a reader on the standby sees, between two polls, exactly a state the primary
  itself could have crashed into (every applied group is a complete group; publication
  lags the store by at most one run instead of one group — document this in the module doc
  and in physical-standby.md where the "one batch under the lock" sentence is).
- Failure in the middle of a run: the batches already written stay written (they were
  durable groups of the primary); the run's publish is replaced by the existing table
  re-derivation path for the touched tables (`derived_state_suspect` / rebuild), exactly
  what a decode failure does today. Say so in a comment.

### 2.3 Flush only when the barrier moved (physical.rs, physical_source.rs)

- `poll_source` / `poll_source_cascade` take a `&FlushMemo` (an `AtomicU64` of the last
  barrier fsynced by this source, owned by the source/session, `0` at start):
  `if barrier > memo.load() { db.flush_wal(true)?; memo.store(barrier) }`. The archiver's
  own poll uses its own memo. Safety argument in the doc comment: the barrier was sampled
  before the flush, and a later poll with the same barrier has nothing new to make durable;
  the engine's own commit path syncs its writes (`wal_sync_mode`), the flush only closes
  the window for a write that raced the sample.
- `poll_source_cascade` syncs the standby's own WAL: same memo rule.

### 2.4 Poll size

- `STANDBY_POLL_BYTES` (main.rs ~4037) 8 MiB → 32 MiB (matches `APPLY_CHUNK_BYTES`): with
  P4.1c the limit is a coalescing target, so a poll now carries up to 32 MiB of small
  groups per round trip. The memory cost is one poll held twice (frames + decoded) and is
  stated in physical-standby.md's sizing paragraph.

### 2.5 Not in this step

The primary's write grouping (one WriteBatch per row / per statement) is an engine change
with its own gate (pg35) — a separate item. Pipelining the next poll under the current apply
is a later step if the numbers still ask for it.

## 3. Tests (red first)

- physical_receiver.rs: a sink test that a poll of N small groups reaches the worker as ONE
  `Command::Apply` (count commands through the test harness that already exists there) and
  that a poll above `APPLY_CHUNK_BYTES` is split in order.
- physical_applier.rs: `apply_run` over three batches that touch the same row (insert,
  update, delete) publishes the final state exactly once; a run where the second batch's row
  fails to decode re-derives that table and the third batch's table is still published; the
  `range_digest` of the default family after a run equals the digest after applying the same
  batches one by one (byte identity of the store does not depend on the run size).
- physical.rs: with the memo, two polls at the same barrier call `flush_wal` once (count
  through a test hook or by timing is not acceptable; use the memo's value).
- Existing suites unchanged: `physical_standby_serving`, `physical_standby_derived_state`,
  `physical_fast_forward`, `physical_large_write_group`, `ha_integration`.

## 4. Gate and measurement

Pre-commit gate (`p41gate.sh c`), lint, phase gate, pg35, perf smoke; the live measurement
is `jlive/ff_probe4.sh` on /dev/shm (CPU ceiling) and on disk, before/after, phases 0 and
0b: MB/s of WAL and sequences/s. CHANGELOG: Fixed — replication, with the measured
before/after rates filled in by the gate run (placeholder `MEASURED_AFTER_FIX`).
