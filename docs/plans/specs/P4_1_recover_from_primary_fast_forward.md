# Spec P4.1 — `recover --from-primary`: the recovery ladder and incremental fast-forward

Parent: `PHYSICAL_STANDBY_REPLICATION.md` §D6 ("from the primary"), `STANDBY_RECOVERY_PLAN.md` §5.5 (R4).
Sprinter `dafb3894ef87`. After P3.2 (`9ce82e7e`, merged `ddfe1708`): `recover --from-archive`, gap fill
from the primary's archive, archive-only standby, cascading. P4.1 is the Oracle
`RECOVER STANDBY DATABASE FROM SERVICE` analogue: rebuild a standby from the primary by shipping only
the key ranges that differ. P4.2 (in-process `auto_recover`, R4b change tracking) is NOT in this spec.

Value: a standby away longer than the primary's recovery window, or whose directory was damaged, is
today rebuilt by a full copy into a new directory. With P4.1 `heliosdb-nano recover --from-primary`
picks the cheapest safe route and reports what it moved: (1) nothing to do, the primary still serves
the tail; (2) incremental fast-forward: both sides digest the replicated keyspace in ~4 MiB leaves
and only the differing leaves travel, as SST files; (3) full copy when the directory is not a
replica of this history, when the operator asks, or when most data differs.

## Facts the design rests on (verified on `ddfe1708`)

- A snapshot's sequence number is not obtainable (crate and C API expose only
  `latest_sequence_number`). `OnlineCheckpointExport::export` already gets an exact cut: it creates
  a checkpoint and reads `latest_sequence_number()` of a read-only open of it
  (`checkpoint_transfer.rs:649-667`, `manifest.checkpoint_sequence`). The fast-forward serves from
  that checkpoint, so `T = manifest.checkpoint_sequence` exactly, and the primary's WAL/archive
  serve `T+1…` afterwards (`reusable()` already checks `retains_after(cut)`).
- Ingestion consumes 0 or 1 sequence numbers per file (data-dependent) and writes no WAL record;
  `delete_range` consumes one per op. The cursor after a fast-forward is therefore DERIVED from the
  observed `latest_sequence_number()`, never predicted. `LocalWriter` must not be used for it (it
  poisons on any accounting mismatch): a separate writer type in `physical.rs` owns these writes
  (the write-funnel test `tests/write_gate_funnel.rs` allows raw writes only in `write_gate.rs`,
  `physical.rs`, `checkpoint_transfer.rs`; keep it that way).
- `PhysicalCursor::validate` requires `local_sequence > source_sequence` (`physical.rs:929`). A
  fast-forwarded standby has `source = T` (the primary's counter) and a local counter that is its
  own; the rule is arithmetic sanity for the streaming path, not a safety property. P4.1 drops it
  (see §4). The safety property stays: `db.latest_sequence_number() == cursor.local_sequence`.
- The stage census (`checkpoint_transfer.rs:1570-1586`) allows only RocksDB names, the two
  markers, `LOCK`, `LOG*`, each a regular file with `nlink == 1`. Incoming SSTs are therefore staged
  OUTSIDE the data directory and ingested with `move_files = false` (copy; no hard link can be
  left behind by a crash).
- `ReopenedPhysicalStage::open` requires `cursor.snapshot_sequence == manifest.checkpoint_sequence`
  (`:1637`), and marker/manifest agreement on `snapshot_id`, `history`, `checkpoint_sequence`,
  `storage_identity`, `manifest_sha256`. A fast-forward ends by RE-CUTTING the stage: new manifest +
  marker at `T` and a cursor `{snapshot: T, source: T, last_first: None, last_digest: None}`.
- Values are sealed per key class on the primary and applied verbatim on a standby, so raw bytes
  (ciphertext included) are comparable; `range_digest` (`physical.rs:864`) already relies on that.
  Digest = SHA-256 (as `range_digest`, segments and manifests; FIPS-clean). The plan's BLAKE3 and
  fan-out-64 tree are NOT used: a flat leaf list costs 32 B + boundary per leaf (1 TiB at 4 MiB
  leaves ≈ 25 MB, paged), and nothing needs the tree.
- The only node-local key today is `cursor` in `__nano_local`. The default CF of a standby holds
  primary bytes only, so leaves partition the WHOLE default CF (`""` .. `[0xff; 64]`).
- rocksdb 0.22: `SstFileWriter::create(&Options)/open/put/finish/file_size` (keys in order, seq 0),
  `DB::ingest_external_file_cf_opts(cf, &IngestExternalFileOptions, paths)` with
  `set_move_files(false)`, `set_snapshot_consistency(true)`, `set_allow_global_seqno(true)`,
  `set_allow_blocking_flush(true)`; `DB::delete_range_cf`; `WriteBatch::delete_range_cf`;
  `Snapshot` + `ReadOptions::set_snapshot/set_iterate_upper_bound/set_total_order_seek` (the engine
  sets a fixed-prefix extractor: ALWAYS `set_total_order_seek(true)`). No approximate-sizes API:
  leaf sizes come from the plan scan. Writer `Options` = `StorageEngine::build_db_options(config,
  path)` on both sides so compression/bloom/prefix match.
- Wire: `Kind` is a flat private enum (next free id 21; 13 retired), `OpenRequest {Snapshot,
  SnapshotOf, Resume}` is a payload of `Open`, `SourceSessionInfo` is the typed response,
  `serve_session`'s allowed set and `match` are manual, `RefuseCode` ids 1-8, no capability
  negotiation beyond `CAP_*` bits in Hello/HelloOk (server ignores the client's). Tests that
  enumerate kinds/codes: `every_control_frame_round_trips_through_its_codec`,
  `malformed_control_frames_are_refused_by_their_codecs`,
  `refusal_codes_say_whether_reconnecting_can_help`, `an_open_may_prefer_a_snapshot_…`.

## 1. Command surface (`main.rs`, `RecoverArgs` ~7895, `run_recover` ~7971)

`heliosdb-nano recover --data-dir D (--from-archive SEGDIR… | --from-primary) [--primary-host H]
[--then primary|stop] [--full] [--no-full-copy] [--full-copy-threshold PCT] [--leaf-bytes SIZE]
[--replace] [--stage-dir DIR] [--config C] [PrimaryTlsArgs] [--replication-token-file F]
[--allow-insecure-replication-remote] [-- START_ARGS]`

- `--from-archive` and `--from-primary` form a required, exclusive clap group. `--from-primary`
  requires `--primary-host`; `--through` is refused with it. `--then` semantics as P3.2 (default:
  follow when `--primary-host` is given — so `--from-primary` follows unless `--then stop`).
- `--full` forces route 3; `--no-full-copy` refuses to fall through to route 3 (exit 3 with the
  estimate); `--full-copy-threshold` default `50` (percent of the primary's leaf bytes that differ;
  1..=100); `--leaf-bytes` default `4MiB` (`parse_byte_size`, 64KiB..=256MiB); `--replace` allows
  route 3 to replace an occupied `D`; `--stage-dir` default `<D>.recover-stage` (created 0700,
  removed on success; must not be inside `D`). Token/TLS/insecure flags exactly as `ResyncArgs`.
- Same preconditions as P3.2: `refuse_running_node(D)` first; a literal `--password`/`--mcp-token`
  in `START_ARGS` is refused.
- Progress lines (`recover: …`) on stdout, one per route step; final summary line (see §3.5).
  Exit codes: 0 success; 1 error (refused, transport, primary unreachable, verification);
  2 archive gap (unchanged, `--from-archive` only); **3** route 3 prepared or required but not
  applied (`--no-full-copy`, or `D` occupied without `--replace`). Document the table (guide
  `physical-standby.md` ~731).

## 2. The ladder (`src/replication/recover_primary.rs`, new)

`recover_from_primary(&PrimaryRecoverOptions, progress) -> Result<PrimaryRecoverReport, RecoverError>`
runs on `spawn_blocking` like `recover_from_archive`; the network client is the existing async
`PhysicalClient` driven from a small runtime handle (as `run_receiver` does) or the function is
async with blocking storage work on `spawn_blocking` — implementer's choice, but NO storage I/O on
the async executor.

1. **Classify `D`** (`classify_standby_directory`): `Physical{history}` → steps 2-4; `Fresh`
   (absent or empty) → route 3 straight into `D` (this is `resync` with the new flags; no
   `--replace` needed); `Refused` (logical standby dir, a primary dir, foreign content) → error 1
   naming the reason (a primary's directory is never replaced by this command).
2. **Open the dirty record** (§4). If `D` carries `ff:dirty`, skip the probe (its cursor is
   meaningless): go to route 2 directly, preferring the recorded `snapshot_id` (§3.2).
3. **Probe** (route 1): `connect_resume_replication(history, S)` with the ladder's identity
   (`PeerRole::Resync`, random node id, `EPOCH`, pg_port 0, Async) and `OpenRequest::Resume
   {history, after: S}`. Accepted (`ResumeInfo`) → close the session and print
   `recover: the primary still serves sequence S+1… (from its WAL or archive): nothing to copy`;
   with `--then primary` start follows as usual (it tails), else exit 0. Refused:
   `WalPruned`/`Gap` → route 2; `HistoryMismatch` (different history or receiver ahead) /
   `EpochStale` → route 3 (requires `--replace`); `AuthFailed`/`Version`/`TooManySessions`/
   `SourceNotServing`/transport → error 1 naming it (`TooManySessions` and `SourceNotServing` are
   retried 5× with the bootstrap backoff first).
4. **Route 2, incremental fast-forward** (§3). After the digest pass, if `differing_bytes >
   threshold% × total_bytes` and not `--no-full-copy` → route 3 (print the estimate and the
   reason); if `--no-full-copy` → exit 3 with the estimate; `--full` skips the digest pass.
5. **Route 3, full copy**: `run_receiver(ReceiverStart::Snapshot{target, expected_history}, InstallOnce)`
   exactly as `run_physical_resync --once` (resumable: the P2.2 staging under `target`'s parent
   resumes a killed download). `target` = `D` when `D` is Fresh, else `<D>.recover-full`.
   `expected_history` = the primary's history from HelloOk (for a `HistoryMismatch` directory the
   old history is irrelevant). After `Installed`: Fresh → done. Occupied: with `--replace`,
   `rename(D, <D>.replaced-<UTC yyyymmddThhmmssZ>)` then `rename(<D>.recover-full, D)` (same
   parent, both renames logged; the old directory is kept for the operator, its size printed);
   without `--replace`, exit 3 printing the two `mv` commands and the completed copy's path (a
   re-run with `--replace` reuses the completed copy: `<D>.recover-full` holding a published
   physical stage of the primary's history is adopted, not re-downloaded). Disk: before route 3
   require `free_bytes(parent) >= manifest.total_bytes + 10%`, else error 1 naming both numbers.
6. **Then**: `--then primary` builds the same `start` argv as P3.2 and runs it; the standby resumes
   at `T` (route 2) or at the copy's cut (route 3) and tails through WAL or archive gap fill.

`PrimaryRecoverReport { route: Tail|FastForward|FullCopy, history, from: u64, to: u64, leaves_total,
leaves_differing, bytes_total, bytes_shipped, leaves_deleted_only, duration, replaced: Option<PathBuf> }`.

## 3. Incremental fast-forward

### 3.1 Wire (NPHYS002 stays NPHYS002: same release on both sides; one new capability bit)

- `CAP_FAST_FORWARD = 1 << 3`, included in `CAPABILITIES`. A client refuses to start route 2 when
  HelloOk lacks it ("the primary's release has no fast-forward; use --full").
- `OpenRequest::FastForward { history: Uuid, after: u64, leaf_bytes: u32, prefer: Option<Uuid> }`:
  encodings `[3] uuid u64 u32` (29 B) and `[4] uuid u64 u32 uuid` (45 B); `leaf_bytes` is the
  client's `--leaf-bytes` (validated 64 KiB..=256 MiB on both sides); `agrees_with`:
  `hello.history == Some(history) && hello.cursor == Some(after)`; `is_snapshot() == false`.
  `Open` sizes gain 29 and 45.
- `SourceSessionInfo::FastForward { session_id: Uuid, snapshot_id: Uuid, sequence: u64 }` →
  new kind **21 `FastForwardInfo`** (40 B). `validate`: `after < sequence` (a receiver at or past
  the cut has nothing to fast-forward: the source refuses with `HistoryMismatch` as for a receiver
  ahead, reason "the receiver at S is not behind the cut T").
- **22 `LeafDigestsRequest { snapshot_id, from: u32, count: u32 }`** (24 B) →
  **23 `LeafDigests`**: header `snapshot_id | status u8 (0 ready, 1 planning) | leaf_count u32 |
  total_bytes u64 | from u32 | count u32`, then `count` entries `start_len u16 | start_key |
  bytes u64 | keys u32 | digest 32`; ≤ 1 MiB per frame (the server pages, `count` may be lower
  than requested); while planning the header has `status 1`, counts 0 and the client re-asks
  every second (each ask is a frame within `frame_timeout`, so a long plan never trips the
  operation timeout). `start_key` of leaf 0 is empty; leaf i ends at leaf i+1's start; the last
  leaf ends at `LEAF_END = [0xff; 64]` on both sides.
- **24 `LeafRequest { snapshot_id, index: u32 }`** (20 B) → **25 `LeafReady { snapshot_id,
  index, size: u64, sha256 }`** (60 B); `size 0` = the primary holds no key in the leaf (delete-only
  leaf, no file). The server builds the SST on request under the work limiter; the client waits
  `operation_timeout`.
- **26 `LeafChunkRequest { snapshot_id, index: u32, offset: u64, length: u32 }`** (32 B) →
  **27 `LeafChunk { snapshot_id, index, offset, sha256(bytes), bytes }`** (same layout and bounds
  as `Chunk`; reuse the codec with a distinct kind).
- Server: a FastForward session's allowed set is `LeafDigestsRequest, LeafRequest,
  LeafChunkRequest, Heartbeat, Close`; `PollRequest`/`ChunkRequest` are Protocol errors in it, and
  the leaf kinds are Protocol errors in Snapshot/Resume sessions. Registry: a session whose Hello
  role is `Resync` is listed with `state: "bootstrapping"` regardless of its cursor and never
  `counts_for_sync` (`StandbyRegistryObserver::admit`; today a cursor makes it "streaming").
- Update every enumerating test listed above; add round-trips and malformed-frame cases for 21-27.

### 3.2 Primary side (`physical_source.rs`, `checkpoint_transfer.rs`, new `fast_forward.rs`)

- `OpenRequest::FastForward` → `open_source_session`: `leases.reuse(prefer)` when `reusable()`
  (same rule as `SnapshotOf`: history, identity, directory verify, WAL/archive retains `cut+1`),
  else `kind.export(...)` + `leases.lease` (so a reconnect within the lease continues the same
  `T`; the lease idle clock starts when the session drops, as today). `SourceKind::Cascade` also
  serves FastForward (its checkpoint already has the local family stripped: `create_cascaded`).
- `OnlineCheckpointExport` gains `fast_forward: OnceLock<Arc<FastForwardPlan>>` built on the first
  FastForward session: a read-only `DB` open of the checkpoint directory (`receiver_options`-style
  options, kept open for the export's life), then ONE ordered scan of the default CF
  (`set_total_order_seek`) that emits leaves: accumulate `len(k)+len(v)`; when the running total
  reaches `leaf_bytes` close the leaf BEFORE the current key (so leaves are ≥ leaf_bytes except the
  last and boundaries are real keys); per leaf: `start_key`, `bytes`, `keys`, SHA-256 over
  `len_le64(k)‖k‖len_le64(v)‖v` in order (identical to `range_digest`). `leaf_bytes` is the value
  carried in the `FastForward` open; the plan is keyed by it, so a reused export asked with
  another `leaf_bytes` plans again (the previous plan and its leaf files are dropped). Planning
  runs on a blocking thread (`SourceWorkLimiter`) and publishes `status ready`; a planning failure
  refuses the session with `SourceNotServing` and the error in the reason. Cap: 2^24 leaves, else
  refuse (raise `--leaf-bytes`).
- Leaf SST: on `LeafRequest(i)`, if not built, `SstFileWriter::create(&build_db_options(config,
  checkpoint_dir))`, `open(<export dir>/leaves/<i>.sst)`, iterate `[start_i, start_{i+1})` of the
  checkpoint and `put` every pair, `finish`; record `size`, SHA-256 of the file; `0` keys → no
  file, `size 0`. Files are served by `LeafChunkRequest` like checkpoint chunks (inode/mtime/size
  re-checked) and live until the export is dropped. Memory bound: streaming; one leaf is built at
  a time per session.
- `/health` on the primary: nothing new (the session is listed as above).

### 3.3 Standby side (`fast_forward.rs` + `physical.rs` writer)

Precondition: the ladder holds the exclusive RocksDB open of `D` (`FastForwardWriter::open`, §4).

1. `FastForwardInfo{T, snapshot_id}`; require `T > S` (else nothing to do: route 1 semantics).
   Write/refresh the dirty record `{history, snapshot_id, from: S, to: T, leaf_bytes, done:
   bitmap}` BEFORE any default-CF write (§4). A record whose `snapshot_id` matches resumes with
   its `done` bitmap; otherwise `done` is cleared (a new cut: every leaf is re-decided by digest;
   leaves already equal cost nothing but the scan).
2. Fetch all leaf digests (paged). Disk: refuse unless `free_bytes(stage_dir) >= 2 × leaf_bytes ×
   batch` (the stage never holds more than one batch).
3. **Local digest pass**: ONE ordered scan of the standby's default CF computing the same framing
   per leaf against the primary's boundaries (advance the leaf index while `key >= next start`;
   a key `>= LEAF_END` on either side is a hard error "key outside the replicated keyspace",
   impossible with classified prefixes). Leaves with no local key have the empty digest. Compare:
   `differing = { i | digest_i != local_i }`, `differing_bytes = Σ bytes_i` over them,
   `total_bytes` from the header. Report `leaves_total, leaves_differing, bytes_total,
   differing_bytes, ratio`. Threshold decision (§2.4) happens here. Leaves in `done` are skipped
   (they are consistent with `T` by construction).
4. **Apply in batches** of `BATCH_LEAVES = 64` consecutive differing leaves: download each leaf of
   the batch to `<stage>/<i>.sst` (chunked, SHA-256 verified; a partial file is resumed by size
   and re-hashed; mismatch → re-download; `size 0` → no file); then ONE `WriteBatch` with
   `delete_range_cf(default, start_i, end_i)` for every leaf of the batch, written synced; then ONE
   `ingest_external_file_cf_opts(default, opts, files_of_the_batch)` (copy, blocking flush
   allowed); then `done |= batch` in the record (synced local write); then remove the batch's
   stage files. A crash anywhere inside a batch leaves the record without the batch: the re-run
   repeats the whole batch (idempotent: the range deletes make it so). Progress line per batch:
   `recover: leaves i..j applied (n of m, X of Y bytes)`.
5. **Re-cut** (§4), in this order: (i) `flush_wal(true)` + `flush` of the default CF; (ii) rewrite
   `NANO-PHYSICAL-MANIFEST.json` (`format 1, snapshot_id, history, checkpoint_sequence: T,
   storage_identity: IDENTITY, files: current census with real sizes and SHA-256, total_bytes`)
   via tmp + rename + dir fsync; (iii) rewrite `NANO-PHYSICAL-REPLICA.json` (`manifest_sha256` of
   the new bytes, same `mode`) the same way; (iv) `FastForwardWriter::recut(T, snapshot_id)`: ONE
   synced write that puts the cursor `{format 1, history, snapshot: T, source: T, local: the
   store's sequence after this very write, None, None}` and deletes `ff:dirty`, then asserts
   `latest_sequence_number() == cursor.local_sequence`. The dirty record is thus removed LAST: a
   crash before (iv) leaves it in place, `start` refuses with the dirty reason, and a re-run
   finds every leaf equal and re-cuts again; after (iv) the directory is a consistent stage at
   `T` by every check `ReopenedPhysicalStage::open` performs. Finally `close` the DB, remove
   `<stage>` (empty), print the summary.
6. The standby's own archive (`<D>.archive`) now has a gap `S+1..T` that its archiver will report;
   the primary's archive is authoritative. Print one line saying so when the archive dir exists.
   A cascaded downstream of this standby must itself run `recover --from-primary` against it
   (its WAL lacks `S+1..T`): document.

### 3.4 Correctness invariants (assert in tests)

(1) After route 2, `range_digest(standby, None, "")` equals the primary checkpoint's digest at `T`
and equals a freshly installed copy of the same export; the cursor is `{T, T}`;
`ReopenedPhysicalStage::open` succeeds; `start` resumes at `T` without "Bootstrapping". (2) Bytes
shipped = Σ sizes of differing leaves (reported ratio ≈ fraction changed: 1% change in a uniform
dataset ships < 5% with 4 MiB leaves). (3) A SIGKILL at any point (test hook
`HELIOSDB_NANO_TEST_KILL_FAST_FORWARD_AT=<phase:index>`, phases `after-delete`, `after-ingest`,
`before-recut`, `after-manifest`, under `internal-tests`) leaves a directory that `start` refuses
with the dirty reason and `/ready` 503 (never a poisoned writer, never a torn serve), and a re-run
completes to the same digest. (4) Keys present on the standby but deleted on the primary vanish
(delete-only leaves). (5) An encrypted dataset fast-forwards without the key (raw bytes); `start`
afterwards serves the rows with the key. (6) The primary keeps serving reads/writes during the
operation; rows written after `T` arrive through the tail after `--then primary`. (7) Route 3
switch above the threshold; `--no-full-copy` exit 3; `--replace` swap keeps the old directory.

### 3.5 Output

```
recover: standby abc… at sequence 1 827 is behind the primary (cut 31 897): WAL pruned, archive does not cover 1 828..27 078
recover: fast-forward from the primary at 127.0.0.1:5440 (cut 31 897, 4 MiB leaves)
recover: 256 leaves, 1 017 MiB; 11 differ (4.3%), 44 MiB to ship, 2 delete-only
recover: leaves 0..63 applied (11 of 11, 44 MiB of 44 MiB)
recover: the standby is at sequence 31 897 (from 1 827; 11 of 256 leaves replaced, 44 MiB shipped, 4.3% of 1 017 MiB; 12.4 s)
```

## 4. Dirty record, writer, cursor rule (`physical.rs`)

- Local key `b"ff:dirty"` → JSON `FastForwardRecord { format: 1, history, snapshot_id, from, to,
  leaf_bytes, done: Vec<u8> (bitmap) }` (cap 4 MiB: 2^24 leaves / 8). `LocalBatch` may write it
  (not `cursor`).
- `LocalWriter::resume` and `read_cursor` check `ff:dirty` BEFORE the sequence equality and return
  new `PhysicalError::FastForwardInterrupted { from, to }` → everywhere it surfaces the message is
  "fast-forward from the primary interrupted at sequence S→T: run `heliosdb-nano recover
  --from-primary --data-dir D --primary-host H` to finish it" (standby `start` → sticky
  `needs_recovery` with that reason, process up, `/ready` 503; `recover --from-archive` refused;
  cascade export `strip_node_local_family` refused; `resync --resume-existing` refused).
- `FastForwardWriter` (new, `physical.rs`): `open(path, config, history) -> Result<Self>` opens
  exclusively with `receiver_options`, requires `LOCAL_CF`, reads the cursor WITHOUT the sequence
  check (and tolerates nothing else: history must match); `record()/set_record()/clear`,
  `delete_ranges(&[(start,end)])` (one synced batch), `ingest(&[PathBuf])`, `recut(T, snapshot_id)
  -> PhysicalCursor` as §3.3.5, `db()` for the digest scan, `close()`. It never touches
  `LocalWriter`'s accounting. Raw-write call sites stay in `physical.rs` (funnel test).
- `PhysicalCursor::validate`: drop `local_sequence > source_sequence`; keep `format == 1`,
  `snapshot <= source <= MAX`, `local >= 1`, and the `(last_first, last_digest)` rule. Grep every
  arithmetic use of `local_sequence` vs `source_sequence` (`from_checkpoint_at`, cascade
  `receiver_source_barrier`, `strip_node_local_family`, tests) and confirm none assumes the
  dropped inequality; fix the unit tests that pin it. Nothing else about the cursor changes.
- `PhysicalMarker`/`CheckpointManifest` are rewritten by `checkpoint_transfer::recut_stage(path,
  manifest)` (new; the only writer of those two files besides staging), reusing the existing
  tmp+rename+fsync helper and `manifest_bytes` hashing.

## 5. Tests

- Unit (`fast_forward.rs`, `physical.rs`, `physical_wire.rs`): leaf planning from a synthetic
  ordered key set (boundaries are real keys, sizes ≥ leaf_bytes, last leaf open, 2^24 cap);
  local digest assignment against foreign boundaries (keys before the first start, between,
  beyond the last; empty leaves); equal digests ⇔ equal content on random edits (insert, update,
  delete, delete-all in a leaf); `delete_ranges + ingest` on a temp DB yields the primary's
  digest and a cursor whose `local == latest`; record round-trip and `FastForwardInterrupted`
  from `LocalWriter::resume`; `recut` correctness; cursor rule relaxed; frames 21-27 round-trip
  and malformed cases; `Open` 29/45 B; allowed-set enforcement per session kind.
- `tests/physical_fast_forward.rs` (real daemons via `tests/support/physical_pair.rs`; the
  `pruning_primary_config`/`standby_away_past_retention` helpers from `physical_archive_recovery.rs`
  moved into the support module): (a) standby away past retention with the archive OFF → `recover
  --from-primary --then stop` takes route 2: digest equal to the primary's checkpoint, cursor `T`,
  report ratio < 10% for 1% changed rows, then `start` resumes at `T` with no bootstrap and
  serves the rows written during the operation; (b) route 1: standby inside retention → "nothing
  to copy", exit 0; (c) delete-only leaves (rows deleted on the primary while away) vanish;
  (d) kill at each hook phase → `start` refuses with the dirty reason, `/ready` 503, re-run
  completes, digest equal; (e) threshold: rewrite > 50% of rows → route 3 chosen, `--no-full-copy`
  exit 3, `--replace` swaps and keeps `<D>.replaced-*`; (f) `HistoryMismatch` directory → route 3
  requires `--replace`; (g) encrypted dataset variant of (a); (h) a fresh/absent `D` → route 3
  into `D` (resync parity); (i) a cascading standby serves FastForward to its downstream.
- CLI: group validation (`--from-archive` with `--from-primary` refused, `--from-primary` without
  host, `--through` with `--from-primary`), exit-code table.

## 6. Docs

`physical-standby.md`: vocabulary rows ~70 (`RECOVER … FROM SERVICE` → `recover --from-primary`)
and ~74 (block change tracking → "digest pass; no tracking map yet"); new section "Rebuilding from
the primary: `recover --from-primary`" (the ladder, routes, what travels, the dirty record, exit
codes, the archive-gap note, sizing: stage dir, `<D>.replaced-*`); rewrite "Refusals and rebuild"
(~802) around the command; limits (~954, ~958). `CHANGELOG.md` "Added — recovering a standby from
the primary (spec P4.1)". Skill recipe 4e + verbs row; `verb-map.md` `recover` row + flags;
`deployment-endpoints.md` one paragraph (fast-forward uses the replication port and TLS as
everything else); `config.example.toml` unchanged (no new keys).
