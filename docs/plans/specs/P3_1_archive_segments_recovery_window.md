# Spec P3.1 — archived WAL segments, archiver, recovery window, catalog views and `archive` CLI

Parent: `PHYSICAL_STANDBY_REPLICATION.md` §D4 (and §D2 rules). Sprinter `e619d47c3457`. After P2.2
(`e96bb566`). P3.2 (recover a standby from segments, archive-only mode, gap fill from the primary's
archive, cascading) follows this step and consumes what it produces; P3.1 ships nothing a standby
*reads* yet.

Value: today a standby that falls behind RocksDB's own WAL retention (`[replication.physical]
wal_retention_secs`, a coarse per-file timer) is refused `WalPruned` and must be rebuilt. The
maintainer asked for Oracle's archived redo: every write group sealed into rotating, self-describing
files on the primary (and on each standby), kept for a **recovery window** (default 7 days) bounded
by disk, pruned oldest-first and **never stalling the primary**, catalogued for the operator
(`heliosdb_archived_wal`, `heliosdb_archive_gaps`, `heliosdb_recovery_window`, `archive` CLI).

## 1. Facts to build on (verified on `e96bb566`)

- `physical::poll_source(db, history, after, byte_limit) -> PhysicalPoll` (physical.rs ~321): the
  tailer every receiver session uses; flushes the WAL, walks `get_updates_since(after)`, returns
  whole write groups as `PhysicalBatch { history, first, last, digest, raw }` and `Gap` on a
  discontinuity. `PhysicalBatch::decode(history, first, raw, digest)` validates; accessors
  `history()`, `first_sequence()`, `last_sequence()`, `digest()`, `data()`.
  `wal_retains_after(db, after)` and `oldest_retained_sequence` exist. `refuse_receiver_source`
  rejects a store carrying `__nano_local`, so a standby's own store is NOT a poll source (P3.2).
- Wire `Batch` payload (physical_wire.rs ~1706): `history(16) | first u64 BE | digest(32) | raw`.
  The segment frame body below is exactly this payload, so a frame decodes with the existing code.
- The primary's source runs in `main.rs` `serve_physical_source` (~6365) from a
  `PreparedPhysicalSource` (~6187: db, history, export_dir, work limiter, leases, listener,
  StandbyRegistryObserver). The standby registry (`ha_state.rs` `StandbyInfo { applied_seq,
  durable_seq, history, .. }`, `get_standbys()`) is the only knowledge of what connected standbys
  still need.
- The receiver applies through `PhysicalApplier::apply_with(&batch, Durability)`
  (physical_receiver.rs ~554, `Command::Apply`): the one place every applied batch passes on a
  standby.
- Config helpers (config.rs): `parse_retention_duration_secs` (`"7d"`, ~1181), `parse_byte_size`
  (~1219), `parse_byte_size_or_unlimited` (~1253); `storage::disk::free_bytes(path)` (statvfs);
  `PhysicalReplicationConfig` (~3585: `enabled, export_dir, wal_retention_secs,
  wal_retention_size_mb, ...`); the data-dir-sibling convention `default_physical_export_dir`
  (main.rs ~6046: `<data dir>.physical-export`, mode 0700).
- TDE: `storage::tde::seal(key_manager, value)` / `open_ref(key_manager, storage_key, raw)`
  (engine.rs ~4746-4800 call them); `meta:tde:keycheck` identifies the dataset key. Verify the
  exact signatures before use; no new crypto code, no new dependency.
- Views: `system_views.rs` `heliosdb_standby_status` (~1777 definition with `status_column`,
  dispatch ~4121, `execute_heliosdb_standby_status(storage)` ~6152) is the pattern for a
  replication view reading process-wide state.
- `/health` replication block: 11 frozen P1.3 fields + `tls`, `reason`, `progress` (P2.2). Any
  P3 addition is additive and goes in a NEW sibling object `archive`, not inside `replication`.
- Epoch: the value `/health` reports today (constant until P5) is what segments carry.

## 2. Segment file (`src/replication/archive_segment.rs`, new)

`<archive_dir>/<history>/<epoch>/<first:020>-<last:020>.hseg`; names sort in sequence order, so a
directory is self-cataloguing. While being written: `<first:020>-open.hseg.partial` in the same
directory; sealing = footer + fsync + rename + directory fsync. Mode 0600, directories 0700.

- **Header** (fixed size, big-endian like the wire): magic `HSEG0001`, format `u16 = 1`, flags
  `u16` (bit 0: frames sealed with the dataset key), history `uuid`, epoch `u64`, first_sequence
  `u64`, created_at unix seconds `i64`, node_id `uuid`, key_check `[u8; 32]` (SHA-256 of the
  `meta:tde:keycheck` value when encryption is on, zeros otherwise), header SHA-256 over the
  preceding bytes.
- **Body**: frames `u32 BE length | payload`, payload = the wire `Batch` payload (so
  `PhysicalBatch::decode` verifies history, first, digest). When flag bit 0 is set the payload is
  `tde::seal`ed as one value with the frame's 8-byte first_sequence (BE) as its record key; keys
  inside the raw batch are plaintext data, values are whatever the primary stored (already
  ciphertext under TDE), which is why the whole frame is sealed. One write group = one frame; a
  frame is never split across segments.
- **Footer**: magic `HSEGEND1`, last_sequence `u64`, batch_count `u64`, first_observed_at and
  last_observed_at unix seconds `i64` (the archiver's clock when it saw the group: RocksDB carries
  no commit time; name the columns `observed_at`, never "commit time"), body_bytes `u64`, file
  SHA-256 over header + body.
- Reader API: `SegmentReader::open(path) -> Result<SegmentInfo>` verifies header and footer (cheap:
  two reads), `verify_deep()` re-hashes the file and decodes every frame, `frames()` iterates
  `PhysicalBatch`es in order (sealed frames need a `KeyManager`; without one `frames()` errors
  `sealed segment: dataset key required`, while header/footer verification still works).
  `repair_partial(path) -> Result<Option<SegmentInfo>>`: scan frames up to the first bad or
  truncated one, truncate there, write the footer, rename — the recovery of an open segment after a
  crash (frames are self-digested, so nothing valid is lost); an empty partial is deleted.
- Every refusal names the file: `segment <path>: <reason>` (bad magic, header hash, footer hash,
  file hash, frame digest, noncontiguous frame, other history, other epoch, sealed without key).

## 3. Archiver (`src/replication/archiver.rs`, new) — primary AND standby

One writer type `SegmentWriter { dir, history, epoch, node_id, key_manager, limits }` with
`append(&PhysicalBatch)` (writes the frame, rolls first when the open segment would exceed
`segment_max_bytes`), `roll_if_due(now)` (seals a non-empty open segment older than
`segment_max_secs`), `seal()`; fsync policy: the frame data is fsynced at every roll and every
`archive_fsync_bytes` (default 8 MiB) in between (archive durability is secondary to the store's
WAL; RocksDB's own WAL TTL covers archiver lag — keep `wal_retention_secs` as is and say so in the
guide).

- **Primary**: a task started with the physical source (`serve_physical_source` or beside it in
  `PreparedPhysicalSource`), owning its cursor = the last sealed sequence in the directory (after
  `repair_partial`), polling `poll_source(db, history, cursor, poll_byte_limit)` every
  `archive_poll_ms` (default 1000; a poll that returned data re-polls immediately). On start, a
  directory whose newest segment ends below `oldest_retained_sequence(db) - 1` (RocksDB has already
  dropped what the archive lacks) logs the gap once (`archive gap <from>..<to>: the store no longer
  holds it`), records it for `heliosdb_archive_gaps`, and continues from the live barrier: the
  archiver never blocks or fails the primary. A `Gap` from `poll_source` mid-run is handled the same
  way. An empty directory on a primary that already has data starts at the current barrier (the
  archive covers from "now"; the guide says to take a snapshot first for a full history).
- **Standby**: the receiver archives every batch it applied (hook after a successful
  `apply_with`, same `SegmentWriter`); its history/epoch are the standby's binding. A standby's
  archive is byte-identical in its frames to the primary's for the same sequences (test).
- Both roles: `[replication.archive]` (new table; flat keys, each with a CLI flag):
  `enabled` (default **true** whenever the node has a data directory and an HA role — primary with
  a replication token, or standby; `--archive=false` turns it off), `dir` (`--archive-dir`,
  default `<data dir>.archive` beside the data directory, created 0700 like the export dir),
  `segment_max_bytes` (`"64MiB"`), `segment_max_secs` (300), `poll_ms` (1000), `fsync_bytes`
  (`"8MiB"`), `recovery_window` (`"7d"`, `parse_retention_duration_secs`), `max_bytes`
  (`"unlimited"`, `parse_byte_size_or_unlimited`), `disk_reserve` (`"10%"`: a percentage of the
  filesystem's size, or a byte size via `parse_byte_size`; new small parser with tests),
  `prune_interval_secs` (60). Validate ranges; refuse a `dir` inside the data directory or equal
  to the export dir. `Debug` for the config lists every key.

## 4. Retention: the recovery window (`archiver.rs` prune task, both roles)

Runs every `prune_interval_secs` and after each roll. Candidates = sealed segments only; the open
segment and the newest sealed segment are never pruned. Order:

1. **Expired**: `last_observed_at < now - recovery_window` → prune (expected, no warning).
2. **Over `max_bytes`**: oldest first until under.
3. **Disk reserve**: oldest first while `free_bytes(dir) < reserve` (recomputed after each
   removal).

Steps 2 and 3 remove segments still inside the window: that is a **shortening**. WARN once per
shortening episode (`recovery window shortened to <effective>: <reason>`; a later episode warns
again; a restored window logs INFO once), count `pruned_inside_window_total`. A segment a
registered standby (`get_standbys()`, `applied_seq + 1 <= segment.last`) still needs is skipped by
steps 2 and 3 while it is inside the window and pruned by step 1 beyond it (the standby is then
refused and needs recovery — plan rule). Primary writes are never blocked or slowed by the archive:
no check in the write path, no backpressure.

`effective_window = now - oldest_retained_first_observed_at` (0 when empty).

## 5. Catalog, views, `/health`, CLI

- `ArchiveCatalog` (in memory, `archive_catalog.rs`): built at start by scanning the directory
  (header + footer of every `.hseg`, `repair_partial` for `.partial`), maintained by the writer and
  the pruner; a file that fails header/footer verification is listed with state `corrupt`, never
  deleted by the pruner (operator's call). States: `available | corrupt`. Gaps = holes between
  consecutive segments of one history/epoch, plus a hole recorded by the archiver (§3). There is
  no catalog in RocksDB: the primary has no `__nano_local` family (§D3.1) and the files are
  self-describing; the plan's `arch:seg:*` keys are not needed (say so in the guide's design note).
- Views (`system_views.rs`, both roles; empty/`disabled` rows when the archive is off):
  `heliosdb_archived_wal` (history, epoch, first_sequence, last_sequence, batches, bytes,
  first_observed_at, last_observed_at, sealed_at, state, path, sha256),
  `heliosdb_archive_gaps` (history, epoch, from_sequence, to_sequence, reason),
  `heliosdb_recovery_window` one row (enabled, archive_dir, configured_window_secs,
  effective_window_secs, oldest_retained_at, newest_sealed_at, segments, bytes_retained,
  max_bytes, disk_reserve_bytes, free_bytes, pruned_inside_window_total, shortened bool, state
  `ok|shortened|disabled|gap`).
- `/health`: new additive top-level object `archive` {enabled, segments, bytes_retained,
  effective_window_secs, shortened, gaps} (not inside `replication`; tell Proxy — it reads none
  of it).
- CLI `heliosdb-nano archive <subcommand> --archive-dir DIR` (offline: reads files only, takes no
  database lock; `--archive-dir` defaults from `--data-dir`/`--config` via the same rule as the
  daemon): `list` (one line per segment, `--json`), `verify [--deep] [--key-file …]` (header/footer
  always; `--deep` re-hashes and decodes every frame; exit 1 and the file name on the first
  failure, `--keep-going` to list all), `gaps [--from SEQ]`, `prune [--dry-run]` (applies the
  configured policy to a directory while the daemon is down; refuses when a daemon's pid file for
  that data dir is alive). An operator-copied directory (segments from another host of the same
  history) is just a directory to `list`/`verify`: no separate `catalog` command.

## 6. Tests (proof-first; real daemons via `tests/support/physical_pair.rs`)

- Unit (`archive_segment`, `archiver`, `archive_catalog`, `config`): write/seal/read round trip
  (plain and sealed with a `KeyManager`), each corruption class detected and named (header hash,
  footer hash, file hash, one frame's digest, truncated tail), `repair_partial` keeps every intact
  frame, roll by bytes and by time, names sort by sequence, retention decision table (expired /
  over max / under reserve / standby-needed inside and beyond the window / newest never pruned /
  corrupt never pruned), `disk_reserve` parser (`"10%"`, `"2GiB"`, bad input), config defaults
  and validation (dir inside data dir refused).
- `tests/physical_archive.rs` (real daemons): primary under load with `segment_max_bytes` small
  → several sealed segments whose frames cover `first..=last` contiguously and whose batch
  digests equal what the standby applied; `heliosdb_archived_wal` rows equal the files;
  SIGKILL the primary mid-segment → on restart the partial is repaired and sealed, archiving
  continues with no gap row; standby-side archive frames byte-equal the primary's for the same
  sequences; `recovery_window = "2s"`, `segment_max_secs = 1` → old segments expire,
  `heliosdb_recovery_window.effective_window_secs` tracks it with state `ok`; `max_bytes` tiny →
  pruning inside the window, WARN once, `shortened = true`, `pruned_inside_window_total > 0`, and
  the primary keeps accepting writes at full rate throughout (assert no insert errors); disk
  reserve path exercised through `free_bytes` injection behind `internal-tests`
  (`HELIOSDB_NANO_TEST_ARCHIVE_FREE_BYTES`); encrypted dataset → sealed flag, `archive verify`
  passes without a key, `--deep` fails without and passes with the key file.
- `tests/archive_cli_tests.rs`: `list`/`verify`/`gaps`/`prune --dry-run` on a prepared directory
  with one corrupt segment (byte flipped) and one segment of another history → named refusals,
  exit codes; `prune` refused while the daemon is alive.
- Fixture `scripts/regress/nano-physical-pair.sh`: `--status` JSON gains `"archive":{segments,
  bytes}` from the primary's `/health` (additive).

## 7. Docs

`docs/guides/physical-standby.md` new section "Archived WAL and the recovery window" with the
Oracle vocabulary (archived redo log ↔ `.hseg` segment; RMAN catalog ↔ `heliosdb_archived_wal`;
recovery window ↔ `recovery_window`; `RMAN> CROSSCHECK`/`VALIDATE` ↔ `archive verify`), the
prune-never-stall rule and what a shortened window means, the design note of §5, "recovering a
standby from segments arrives in P3.2". `config.example.toml` (new table, commented), CHANGELOG,
server skill, verb-map, `--help`.

## Constraints

No cargo/git; read before edit; match style (120 columns); never weaken the token, census or
history checks; `/health` `replication` block unchanged (archive is a sibling object); the
archive must never block, fail or slow the write path (no code in `WriteGate`/commit paths).
Report: files; config/flag table; the segment format as implemented; the retention algorithm;
tests red/green; the commands to run.
