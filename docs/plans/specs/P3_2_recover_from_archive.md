# Spec P3.2 — recover a standby from archived WAL; gap fill from the primary's archive; archive-only standby; cascading

Parent: `PHYSICAL_STANDBY_REPLICATION.md` §D6 ("from WAL"), §D7 (cascading). Sprinter `e619d47c3457`.
After P3.1 (`77c7f8d8`): `archive_segment.rs` (`SegmentReader::open/open_expecting -> SegmentInfo`,
`SegmentInfo::frames(key) -> FrameIter` of `PhysicalBatch`, `verify_deep(key)`, `encode/decode_batch_payload`,
`parse_segment_name`, `segment_dir`), `archive_catalog.rs` (`ArchiveCatalog::scan(root, PartialPolicy)`,
`entries()`, `gaps()`, `last_sequence(history, epoch)`, `ArchiveStatus::installed()`), `archiver.rs`
(`ArchiveSettings::from_config`, `dataset_key(config)`, `SegmentWriter`, `ArchiveLock::acquire`,
`prune_offline`, `start_primary/start_standby`), `[replication.archive]`, the three views, `/health.archive`
and `Commands::Archive(ArchiveArgs)` with `List|Verify|Gaps|Prune` (`run_archive_command`, main.rs ~6244).
P3.2 makes a standby READ segments.

Value: with P3.1 a primary keeps 7 days of write groups on disk, but a standby that fell behind
RocksDB's own WAL retention is still refused `WalPruned` and rebuilt from a full copy. P3.2 closes
that: (1) the primary serves the gap from its archive on the existing replication connection, so a
standby away for days simply catches up; (2) an operator can advance or build a standby from
segments copied by hand (`recover --from-archive`), with or without a reachable primary; (3) a
standby can run archive-only (Oracle managed recovery from archived logs, no primary connection);
(4) a standby can be a source for another standby (cascading, opt-in).

## 1. Gap fill from the primary's archive (`physical_source.rs`, `archiver.rs`)

- `open_primary_session` `Resume { history, after }` and `poll()` today call `poll_source` and map
  `PhysicalError::Pruned` to `RefuseCode::WalPruned` (`poll_error`, ~line 50). New: when
  `poll_source` reports `Pruned { wanted, oldest }` and the node's archive catalog holds a segment
  chain covering `wanted ..= oldest-1` (contiguous, same history, verified header/footer), the
  session serves batches FROM THE SEGMENTS (an `ArchiveTail` over `ArchiveCatalog::scan` + `SegmentInfo::frames(key)`, key = `archiver::dataset_key(config)`),
  bounded by the poll byte limit, until the cursor reaches a sequence RocksDB still retains, then
  switches back to `poll_source`. Sealed frames are opened with the node's key manager. The
  switch point is re-checked per poll (`wal_retains_after`). The receiver sees ordinary
  `PollStart/Batch/PollEnd` frames: no protocol change. `/health` on the primary: the standby's
  entry gains `"source": "archive"|"wal"` (additive, inside `standbys[]`).
- `WalPruned` is still sent when the archive cannot cover the gap; its reason now names the
  exact missing range and both floors (`archive holds from S_a, RocksDB from S_w`).
- Epoch/history: only segments of the primary's current history and epoch are served.

## 2. `heliosdb-nano recover` (new `Commands::Recover`, sibling of `Resync`, `main.rs` ~1703)

`recover --data-dir DIR --from-archive SEGDIR [--from-archive SEGDIR …] [--through SEQ]
[--then primary|stop] [--primary-host …] [--config …]` — offline, no daemon running on the data
directory (pid-file check as `archive prune`).

- Builds the chain from `cursor + 1` across every given directory (operator copies from any node
  of the same history: segments are matched by history/epoch/first/last, duplicates by SHA-256),
  **verifies every segment deep before applying anything** (hash, every frame's digest, contiguity,
  history, epoch; a corrupt or foreign file is named and the command stops before the first apply),
  then opens the stage with `ReopenedPhysicalStage::open` and applies through the SAME path a live
  receiver uses (`PhysicalApplier::apply_with(batch, Synced)` with hydration first) so derived state
  and cursor semantics are identical; progress line per segment; a crash mid-apply resumes from the
  cursor (every frame = one durable write group; re-running the command is idempotent: already
  applied frames are skipped by sequence).
- Ends with: the exact position reached; if the chain stops short of `--through` (or of the newest
  segment) it prints the missing range `S..T` and exits 2 (`needs recovery: archive gap`). With
  `--then primary` (default when `--primary-host` is given) it starts following the primary from
  the new cursor like `start` would, otherwise exits 0.
- Empty `--data-dir` + `--from-archive` is NOT a build from nothing (segments carry deltas, not a
  base): refused with "a standby starts from a snapshot of the primary (`start` bootstraps it);
  segments advance it". P4 adds the base-plus-segments path.

## 3. Archive-only standby (`start --replication-role standby --source archive --archive-source DIR…`)

Managed recovery from archived logs: the standby serves reads and applies segments as they appear
in the given directories (operator copies, NFS, rsync), never opening a replication connection.
Posture: `recovering{mode:"archive", progress:"<cursor> of <newest>"}` while applying, `streaming`
is never reached; `/ready` is 200 when the cursor is within `max_standby_lag` of the newest
segment's `last_observed_at` (lag_secs = now - last_observed_at of the cursor's segment), else 503
with state `catching_up`; a gap in the directories → `needs_recovery` naming the range, stays up
(J2), resumes automatically when the missing segment appears. `--source primary` (default) is
today's behaviour. `pg_last_wal_replay_lsn()` and `heliosdb_standby_status.primary` report the
archive directory as the source (`primary` column = `archive:<dir>`).

## 4. Cascading source (opt-in: `[replication.physical] cascade = false`)

A standby with `cascade = true` serves its replication port like a primary: `Snapshot` exports
from its own store and `Resume` polls. Facts: `poll_source` refuses a store carrying `__nano_local`
(`refuse_receiver_source`) and `validate_raw_batch` refuses CF opcodes, so today this is fail-closed.
Change: a `poll_source_cascade` variant that walks `get_updates_since` and STRIPS every record
addressed to the local CF (the cursor put), re-encoding the write batch without them and
recomputing the digest; the served sequence numbers stay the PRIMARY's (a stripped record still
occupies its sequence, so the downstream cursor arithmetic is unchanged: document the invariant
`local_operations` accounting). Exports from a standby remove the local CF from the checkpoint
(`DB::drop_cf` on the checkpoint copy or exclude it from the manifest). The downstream standby
sees the same history/epoch. The upstream standby's registry lists downstreams; `/health`
`cascade: true`. P5's fencing applies unchanged.

## 5. Tests

- Unit: `ArchiveTail` serves a chain and stops at the WAL floor; refusal when the chain has a hole
  names it; chain building across directories with duplicates and a foreign segment; idempotent
  re-apply skips applied frames; cascade stripping yields the same default-CF digest as the
  primary's batch for the same sequence span.
- `tests/physical_archive_recovery.rs` (real daemons): standby away while the primary's RocksDB
  WAL retention expires (`wal_retention_secs` minimal + simulated purge as
  `physical_standby_refusals::prune_archived_wal` does) → reconnects and catches up from the
  archive, `source:"archive"` then `"wal"`, rows exact, no bootstrap; archive disabled → still
  `WalPruned` with both floors named; operator copies segments to a third directory, standby
  stopped: `recover --from-archive` advances it to the exact rows (digest equal to the primary's
  at that sequence), re-run is a no-op, corrupt segment → named, nothing applied, gap → exit 2 with
  the range; archive-only standby follows a directory the test appends segments to (copied from the
  primary's archive) with `/ready` 200/503 per lag, a missing segment → `needs_recovery`, appears →
  resumes; cascade: primary → standby A (cascade on) → standby B, B's rows and default-CF digest
  equal the primary's, B's `/ready` 200; encrypted dataset variant for recover.
- CLI: `recover` argument validation, pid-file refusal, exit codes.

## 6. Docs

`physical-standby.md`: "Recovering from archived WAL" (Oracle: `RECOVER STANDBY DATABASE`, FAL ↔
gap fill, managed recovery ↔ `--source archive`, cascaded standby), `recover` and `--source`
reference; CHANGELOG; skill; verb-map; `deployment-endpoints.md` (cascading port).
