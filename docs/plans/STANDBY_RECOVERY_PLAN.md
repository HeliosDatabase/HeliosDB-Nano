# Standby recovery: recovery window, stay-up posture, rebuild from the primary or from WAL

**Status:** PLAN for review (Fable), no code. Written 2026-10-19 on `fix/replication-retention-j`
(`8d1575a8`: J1 history window + J1a cancel-safe transport, both gated, pushed, not released).
Continues `REPLICATION_J_STANDBY_CONTINUITY.md` (J2, J3) and builds on
`REPLICATION_4_41_1_DESIGN.md` §3 (physical channel as the recovery authority).
Sprinter: `58d4e3113e56` (field J), `36b56ac796e2` (retention), `551a9cdd2e0b` (serving rebuild),
`b4fafe6875ae` (archive #7 safe resync), `bbba90ddfc61` (promotion).

## 0. Decisions recorded (maintainer, 2026-10-19)

1. **J2 — the standby stays up.** A standby that the primary cannot serve no longer exits.
   The history a primary keeps for its standbys is a **recovery window**, tunable, default **7 days**.
2. **J3 — two rebuild modes:**
   - **from the primary**: fast-forward only what changed, after Oracle
     `RECOVER STANDBY DATABASE FROM SERVICE` (a feature request);
   - **from WAL**: history already available on the standby host, shipped by the primary or placed
     there by the operator, with a **catalog** of what is available (after the RMAN catalog).
3. No patch release for J1/J1a; they ship with 4.42.0.
4. This plan is reviewed with Fable before implementation.

**Reading of "recovery window" used here (confirm in review):** the RMAN retention sense
(`CONFIGURE RETENTION POLICY TO RECOVERY WINDOW OF 7 DAYS`). The primary keeps enough history
that any standby which was current at some point in the last 7 days can be brought up to date
without a full copy. It is not a limit on how long a refused standby keeps retrying: under
"stay up" it retries indefinitely, at a slow interval.

## 1. Vocabulary: Oracle terms in HeliosDB

| Oracle | HeliosDB today | This plan |
|---|---|---|
| SCN | Two positions: the logical LSN (`wal:entries:{lsn}`) and the RocksDB sequence number (physical channel) | Recovery positions are physical: `(history_id, epoch, sequence)`. The logical LSN stays for streaming. |
| Online redo | RocksDB WAL (`*.log`) | unchanged |
| Archived redo | RocksDB `archive/`. Exists only with physical replication on; TTL at most 1 day; no catalog | Archive **segments**: sealed files of physical batches, plus a catalog (§5.4) |
| `LOG_ARCHIVE_DEST_n`, `archive_command` | none | `[replication.archive]`: a local directory, an optional hook, and standby-side archiving |
| RMAN catalog / `V$ARCHIVED_LOG` | none | Catalog keys, `heliosdb_archived_wal` view, `heliosdb-nano archive …` commands |
| FAL (gap fetch) | catch-up from the primary's logical WAL | The standby fetches a missing range from the primary's history or archive |
| Datafile block, block SCN | No blocks: one key-value space. No durable per-key sequence (compaction drops it) | Key-range **leaves** with digests (§5.5) |
| Block change tracking file | none | Optional change-tracking map (§5.5, R4b) |
| `RECOVER … FROM SERVICE` | none. `resync` is a full copy only, loopback-only and non-serving | `heliosdb-nano standby recover --from-primary` (§5.5) |
| Managed recovery from archived logs | none | `heliosdb-nano standby recover --from-archive` (§5.4) |
| `RECOVERY WINDOW OF 7 DAYS` | J1 `logical_wal_retention_entries` (a count, unreleased); physical `wal_retention_secs` (max 86400) | `[replication] recovery_window = "7d"` |
| `DB_RECOVERY_FILE_DEST_SIZE` | physical `wal_retention_size_mb` | `[replication] recovery_window_max_bytes` |
| Incarnation / RESETLOGS | `history_id`; no epoch | `history_id` + `epoch` in every segment and cursor (the epoch arrives with promotion, §6 R5) |
| Standby MOUNTED vs Active Data Guard | Serves stale data while reconnecting; exits on refusal | Explicit states and readiness (§5.2) |

## 2. What the code does today

Facts this plan relies on. All were read on `8d1575a8`; the ones marked ✓ were re-checked by hand.

**Two replication channels.**
- **Logical (native protocol v3)** is today's serving standby.
  - It applies `WalOperation`s and keeps its durable cursor in `repl:applied_lsn` (`engine.rs:11411-11459`).
  - It catches up from the primary's `wal:entries` above `wal:pruned_through` (`engine.rs:11473-11495` ✓).
  - Its RocksDB has its own sequence numbers, `IDENTITY` and MVCC version keys. Values are re-encrypted with fresh random nonces (`crypto/mod.rs:101-111` ✓). Its keyspace is therefore equivalent to the primary's but not byte-comparable.
- **Physical (`NPHYS001`)** is the `resync` path.
  - It does a checkpoint transfer (`checkpoint_transfer.rs:431`), then tails raw `WriteBatch`es from `get_updates_since` (`physical.rs:217-275`).
  - The cursor `__nano_physical_replica_cursor_v1` is written in the same synced write as each batch (`physical.rs:392-436`).
  - Its keyspace is the primary's keyspace at the cursor, byte for byte: raw batches carry the primary's ciphertext.
  - Limits:
    - **Non-serving:** an ordinary open refuses the `NANO-PHYSICAL-REPLICA.json` marker (`engine.rs:2518-2527`).
    - **Loopback-only** (`physical_wire.rs:489-494`, `main.rs:5305`).
    - The snapshot download **does not resume** (`physical_receiver.rs:53-56`).
    - **Any local write poisons** the receiver (`physical.rs:325-333`, `396-399` ✓).
  - Every physical module is still headed "EXTERNAL DRAFT".

**History retention.**
- **Logical:** J1's count window `logical_wal_retention_entries` (not yet released). `replay_from` loads the whole requested range into memory (`wal.rs:1399-1464` ✓).
- **Physical:** the RocksDB `archive/` with `wal_ttl_seconds` 60..86400 (default 3600) and a size limit of 64..65536 MB, set only when physical replication is enabled (`engine.rs:2584-2600` ✓, `config.rs:3228-3262` ✓). There is no catalog.
- `WalStore` segment files exist under `replication_wal/`, but nothing writes to them (`main.rs:3241-3256`).
- `WriteAheadLog::cleanup_old_logs` (time-based) has no caller (`wal.rs:1851`).

**Standby posture.**
- On a "full resync required" refusal, a history mismatch or a protocol mismatch, the client stops (`streaming.rs:1166-1180` ✓). Then the listeners close and the process exits non-zero (`main.rs:2394-2458`).
- A fresh standby refused before its first binding leaves an unbound directory. Every later start of that directory then fails with `UnboundReceiver` (`streaming.rs:1276-1281` ✓, `history.rs:147-196`).
- While reconnecting, SQL keeps serving stale data.
- `/health` is a fixed `{"status":"ok"}` (`main.rs:3507-3509`). There is no `pg_is_in_recovery`. Standby write admission refuses with SQLSTATE 25006 (`protocol/postgres/handler.rs:1813-1830`).

**Building blocks.**
- No checksum or Merkle structure exists over table data.
- `SstFileWriter`, `ingest_external_file`, `WriteBatch::delete_range`, `open_as_secondary` / `try_catch_up_with_primary` and `live_files` all exist in the pinned `rocksdb 0.22` crate but are unused. `get_approximate_sizes` does not exist there (✓ crate source).
- MVCC versions cannot answer "what changed since X": deletes leave no version, and fast updates and COPY write none (`version_gc.rs:34`, `copy_marker.rs:23-27`).
- `parse_retention_duration_secs` already accepts `"7d"` (`config.rs:904` ✓).

**Encryption.**
- Encryption is per value: AES-256-GCM with a random nonce. Keys are stored in plaintext. HNSW snapshot files live outside RocksDB and are not encrypted (`engine.rs:9863-9866`).

## 3. Target behaviour (acceptance statements)

1. A standby that was current at any time inside the recovery window catches up on its own, from the primary or from an archive. No operator action and no full copy are needed.
2. A standby that cannot be caught up **stays up**.
   - It refuses data reads with a typed error that names the reason and the command to run.
   - It reports `needs_recovery` on `/ready`, in a status view and in logs.
   - It keeps retrying. It never crash-loops a supervisor.
3. `standby recover --from-archive` brings a standby forward from segments on its host. It verifies each segment before use and names any missing range exactly.
4. `standby recover --from-primary` brings a standby forward over the network. It moves only what changed when the standby still holds a copy of the same history, and falls back to a full copy otherwise. It always reports which route it took and how many bytes it moved.
5. None of this ever serves data from a different history, applies history out of order, or deletes an operator's data directory.

## 4. The central decision: which standby the recovery modes work on

**Decided 2026-10-20: the physical standby replaces the logical one for all standbys.** The
design (node-local column family, derived-state maintenance from raw batches, serving handle,
physical-sequence ACKs, archive segments, phases P0-P5) is `PHYSICAL_STANDBY_REPLICATION.md`.
The analysis below is kept for the record.

Both requested modes assume an Oracle physical standby: a block copy of the primary that applies redo. In HeliosDB that is the **physical receiver**, not today's serving standby.

- **"Changed blocks since the standby's SCN"** needs the standby's data to equal the primary's data as of a known position.
  - A physical receiver at cursor `S` holds exactly the primary's keyspace at `S`, so key ranges can be compared byte for byte.
  - A logical standby holds an equivalent but different keyspace: its own sequence numbers, its own MVCC version keys and timestamps, and re-encrypted values. It has no "block" that can be compared.
- **"Recover from WAL"** needs a history that is complete.
  - The RocksDB WAL records every writer: catalog, counters, indexes, branches, bulk load and sequences.
  - The logical WAL has documented coverage gaps (`REPLICATION_4_41_1_DESIGN.md` §8 residuals, and embedded explicit transactions emit none).

**Recommendation.** Make the physical standby the serving standby (`REPLICATION_4_41_1_DESIGN.md` §3, steps 2-3), and build both recovery modes on it.
- The logical channel stays as the wakeup and ACK path, and serves the existing logical standbys until they are converted. Conversion is one full copy.
- R0 below is independent of this decision and delivers the "stay up" decision and the recovery window first, on the logical channel the field runs today.

**Alternative (cheaper, weaker): logical only.**
- "From the primary" becomes a logical bootstrap: a consistent snapshot at a published LSN, then the tail. It is always a full copy.
- "From WAL" becomes exported logical-WAL segments.
- There is no incremental mode, and the logical coverage gaps are inherited.
- This is reasonable as a stopgap if serving a physical replica (R2) proves too slow to reopen.

**Serving technique to evaluate in R2.**
- The 4.41.1 design publishes generations: a checkpoint of the staged database, a read-only open, then a swap.
- A RocksDB secondary instance (`open_as_secondary` + `try_catch_up_with_primary`, present in the crate) could follow the staged database in-process without a reopen per generation.
- Either way, derived state (catalog, ART indexes, HNSW graphs, caches) must be rebuilt or refreshed per generation. That cost is the main R2 risk.

## 5. Design

### 5.1 Recovery window (R0, both channels)

```toml
[replication]
recovery_window = "7d"                 # default; seconds or <n>[s|m|h|d|w]
recovery_window_max_bytes = "16GiB"    # proposed default, open question 3
```

**Semantics**
- A primary keeps every history record younger than the window, so a standby whose position is no older than the window catches up without a rebuild.
- The byte cap bounds the disk. When the window would exceed the cap, the oldest history goes first.
  - A WARN names the effective window, and status views expose it.
  - Primary writes are never blocked by retention. Oracle stalls the archiver when the recovery area is full; that is open question 3.
- The window applies only to primaries. A standalone node keeps today's behaviour: everything below the checkpoint is deleted.

**Logical `wal:entries`** (replaces J1's unreleased `logical_wal_retention_entries`; open question 10)
- Each checkpoint batch also writes a time mark, `wal:mark:{unix_secs:020} = checkpoint LSN`. Checkpoints happen every 30 s or 1000 entries, so marks have about 30 s granularity.
- Pruning deletes entries through the LSN of the newest mark older than `now - window`, then deletes the marks below it.
  - It works on keys only, so a corrupt value can never wedge truncation. This is the property `truncate_to_checkpoint` relies on today.
  - The history floor `wal:pruned_through` and the redo floor `wal:checkpoint` keep their J1 meaning.
- **Byte cap:** the crate has no approximate-size API, so each mark also records the running total of entry bytes written. The cap prunes whole mark intervals from the oldest until the retained total fits.
- **Clock steps:** marks are ordered by LSN as well as time. A mark whose time goes backwards is clamped to the previous mark's time, so a backwards clock step can never prune newer history.
- **Prerequisite (from J1b):** `replay_from` and catch-up must read history in chunks, with bounded memory per standby. Seven days of history cannot be loaded at once (`engine.rs:11492`).
- **J1b per-standby floor:** never prune below the slowest registered standby's durable position, but still within the window and the cap.

**RocksDB WAL archive** (when physical replication is enabled)
- `wal_ttl_seconds = window` and `wal_size_limit_mb = cap`. Lift the 86400 s maximum.
- `[replication.physical] wal_retention_secs` and `wal_retention_size_mb` become deprecated aliases for the window and the cap.
- **Verify on RocksDB 8.10** that `get_updates_since` reads archived logs across the TTL, and that WAL recycling (`set_recycle_log_file_num`) stays off.

**With an archive (R3)** the window is served from segments. The in-database windows can then shrink to a short tail (for example 1 h), which saves compaction work on the primary.

**Sizing note for the docs.** History bytes per day ≈ row-image bytes written per day, plus RocksDB WAL bytes per day when physical replication is on. Status reports the oldest retained time and the bytes held.

### 5.2 Standby posture (R0)

One state machine, persisted with its reason, exposed in logs, `/health`, a new `/ready` and a status view `heliosdb_standby_status`:

| State | Meaning | Data reads | `/ready` |
|---|---|---|---|
| `connecting`, `catching_up` | before the catch-up barrier | refused, as at startup today | 503 |
| `streaming` | caught up | served | 200 |
| `reconnecting` | primary lost; data current as of the cursor | served with the lag visible. Optional `max_standby_lag` turns reads off beyond a bound | 200, or 503 past the bound |
| `needs_recovery {reason, position, primary_floor, since}` | position refused, applicator stopped, or history mismatch | refused with a typed error; status views still answer | 503 |
| `recovering {mode, progress}` | R3/R4 running | refused | 503 |

**Rules**
- A "full resync required" refusal, an applicator stop and a history mismatch keep the process and its listeners up.
  - Data reads get SQLSTATE `55000` (`object_not_in_prerequisite_state`, proposed). The message names the reason and the recovery command.
  - Status views, `SHOW` and `/health` still answer: liveness is 200, and a replication block reports the state.
- In `needs_recovery` the standby retries with backoff capped at about 5 minutes. Retrying helps because the primary's window may have been widened, the operator may have added archive segments, or `auto_recover` may be configured.
- `[replication] on_unrecoverable = "stay-up" | "exit"` defaults to `stay-up`. `exit` is kept for supervisors that alert on exit.
- **History mismatch stays fail-closed.** A different dataset at the primary endpoint is never served and never auto-recovered. Only an explicit `standby recover --from-primary --rebind` (always a full copy into a new directory) may change the binding.
- **Fix the unbound-directory trap.** A fresh standby refused before its first binding must be able to restart.
  - Option (a): bind to the `history_id` from the authenticated handshake before catch-up is planned. That is safe because the id comes from the same authenticated peer that refused.
  - Option (b): mark the directory "pending first sync", which a restart accepts.
- Writes are still never routed to a standby. `SWITCHOVER` stays refused until R5.

### 5.3 Physical standby that serves (R1, R2)

The mechanism is `REPLICATION_4_41_1_DESIGN.md` §3.1-3.3: cut markers, generation publication and the readiness barrier. This plan adds:

- **Remote transport (R1).**
  - The physical channel and the native channel must work between hosts. Proposed: TLS on both channels, reusing the `[server]` certificate and key or a dedicated replication pair, with optional mutual TLS; the bearer token stays.
  - Until this lands, the documented tunnel requirement stands. The field run hit it: "initial physical transfer supports loopback only".
- **Resumable snapshot (R1).** Resume a download by file and offset. Every file is already verified by the SHA-256 in the manifest.
- **Start path (R2).** `start --replication-role standby` on a physical replica directory runs the receiver, publishes generations, and serves SQL on the published one, with readiness per §5.2.
- **Conversion.** An existing logical standby becomes physical through one full copy into a new directory. The old directory is kept until the operator removes it.
- **No local writes.** A standby must refuse local writes on every interface, or the cursor is poisoned (`physical.rs:396-399`). Admission already exists (25006). R2 must prove there is no bypass, including embedded, MySQL, REST, COPY and extended protocol.
- **Derived state per generation.** The ART is rebuilt on open. HNSW snapshot files are outside RocksDB, so either rebuild them per generation or ship them alongside. Measure (§7).

### 5.4 Mode "from WAL": archive segments and catalog (R3)

**Segment file** — contiguous physical history, self-describing:
- **Header:** format version, payload kind (`physical`; `logical` reserved for the fallback in §4), `history_id`, `epoch` (0 until R5), first sequence, creation time, source node id, and the encryption flag with a key-check id.
- **Body:** the `PhysicalBatch` frames the physical wire already carries: history, first and last sequence, raw `WriteBatch` bytes and a SHA-256 per batch (`physical.rs:53-93`).
- **Footer:** last sequence, batch count, first and last commit wall-clock time, and the whole-file SHA-256.
- **Protection:** sealed with the dataset key when encryption is on, because keys inside batches are plaintext even though values are already ciphertext. Files are mode 0600.
- **Name:** `<history>/<epoch>/<first:020>-<last:020>.hseg`. Names sort in order, and a directory can be catalogued from its files alone.

**Producers**
1. **Primary archiver** (`[replication.archive] enabled, dir, segment_max_bytes, segment_max_secs`).
   - A local tailer over `get_updates_since` (reusing `poll_source`) rolls, fsyncs and catalogues segments.
   - Optional `archive_command` hook, with PostgreSQL's contract: a segment counts as archived only after exit status 0.
   - The RocksDB archive TTL then only has to cover the archiver's lag.
2. **Standby-side archive.** A standby persists the batches it receives into its own archive. It then has a local source for its own recovery, and later can act as a gap source for other standbys (cascading is out of scope).
3. **Operator.** Copy segment files by any means, then run `heliosdb-nano archive catalog --data-dir D --start-with DIR` (RMAN `CATALOG START WITH`).

**Catalog**
- **Storage:** node-local keys `arch:seg:{history}:{epoch}:{first:020}` holding the path, last sequence, bytes, SHA-256, first and last time, state (`available`, `missing`, `corrupt`, `expired`) and source.
  - The keys are excluded from replication and from the digests in §5.5.
  - The catalog can always be rebuilt by rescanning the files, so losing it is never fatal.
- **SQL views:** `heliosdb_archived_wal` (the `V$ARCHIVED_LOG` analogue) and `heliosdb_archive_gaps`.
- **CLI:** `archive list | catalog --start-with DIR | verify [--deep] | gaps --from POS | prune`. `prune` follows the recovery window by default.

**`heliosdb-nano standby recover --from-archive [DIR …] --data-dir D`** (also run by a `needs_recovery` standby whose `auto_recover` includes `archive`):
1. Read the cursor `(H, E, S)`. Refuse a directory that is not a physical replica of `H`.
2. Build the chain starting at `S+1` from the catalog plus any DIRs given.
   - Verify each segment's header, footer and file SHA-256 before applying any of it.
   - Refuse segments from another history or epoch.
   - Refuse a chain that does not start at exactly `S+1`.
3. Apply the batches through the existing synced batch-plus-cursor write. That makes the apply crash-safe and resumable at batch granularity.
4. At the end of the chain:
   - If the primary is reachable, continue with the live tail, first asking the primary for any range it still holds (the FAL step).
   - Otherwise stay `needs_recovery` with "archive ends at sequence X; next needed X+1". `archive gaps` shows the same range.
5. Publish a generation and become ready only at the primary's barrier.
   - `--until <sequence|time>` (point-in-time on a standby) is out of scope for now. The footers' commit times make it possible later.

### 5.5 Mode "from the primary": the recovery ladder and incremental fast-forward (R4)

`heliosdb-nano standby recover --from-primary --data-dir D` (and `auto_recover = "full"`) takes the cheapest safe route. It prints the route and the bytes moved.

1. **Tail.** The primary's RocksDB archive or archive segments still hold `S+1…`, so stream them. This is the WAL mode with the primary as the archive.
2. **Incremental fast-forward.** The directory is a physical replica of the same history with a valid cursor, but `S` is older than the primary's history. Ship only the key ranges that differ.
3. **Full copy.** Use this when there is no usable replica (different history, no cursor or a damaged directory), when `--full` is given, or when the digest pass finds that most data differs. It is the checkpoint transfer, made resumable in R1. It writes to a new directory and publishes with no-replace, as today.

**Incremental fast-forward (the `RECOVER … FROM SERVICE` analogue)**
- **Consistent point.** The primary pins a RocksDB snapshot at sequence `T`, taken at a cut marker so that `T` is a servable boundary.
  - It holds the snapshot for the operation, bounded by a timeout and released on disconnect.
  - It keeps WAL from `T` so the tail after `T` follows.
- **Keyspace classification.**
  - Replicated prefixes: `data:`, `v:`, `v_idx:`, `snapshot:`, `meta:`, `counter:`, index and vector snapshot keys, branch keys, `wal:` keys and so on.
  - Node-local prefixes: the physical cursor, recovery state, the archive catalog and binding-related keys.
  - A source-scan test fails CI when a prefix written anywhere in `src/` is unclassified, the same idea as the cut-marker writer test.
- **Leaves and digests.**
  - The primary splits the replicated keyspace into leaf ranges of about 4 MiB of key+value bytes each, choosing boundaries from its snapshot, and arranges them as a tree with fan-out 64.
  - A leaf's digest is BLAKE3 over length-prefixed `(key, value)` pairs in key order.
  - Raw bytes are comparable because a physical replica holds the primary's ciphertext unchanged. For the same reason this does not work on logical standbys (§4).
- **Protocol (physical wire v2).**
  - The primary sends boundaries and digests top-down. The standby computes its digests over the same boundaries at its cursor and requests the leaves that differ.
  - The primary streams each requested leaf as an SST built from its snapshot (`SstFileWriter`).
  - The standby stages the SSTs and verifies them. Then, per leaf, it writes `delete_range(leaf)` followed by `ingest_external_file`. A small leaf can instead go as one `WriteBatch`.
- **Crash safety.**
  - A leaf replacement is two steps, so the standby sits in `recovering` with a durable record `recovery:target = {H, E, T, leaf plan, completed leaves}`.
  - A crash re-runs the incomplete leaves. A leaf is replaced wholesale, so redoing it is idempotent and deletes are covered by the range delete.
  - Serving resumes only after the last leaf and `cursor = T` are durable. Then the tail runs from `T`.
- **Cost.**
  - Both sides read their whole dataset once, as RMAN does without block change tracking.
  - The network carries only the leaves that differ, plus 32 bytes of digest per leaf.
  - After the digest pass the command estimates the transfer. Above a threshold (proposed: more than 50% of bytes differ) it switches to a full copy.
- **R4b (optional) change tracking**, the block-change-tracking analogue.
  - A background task on the primary tails its own WAL (`get_updates_since`, off the commit path). It records, per leaf bucket, the highest sequence that touched the bucket, and persists that map periodically.
  - With the map, the primary lists the buckets changed since `S` without either side scanning. That is valid only when the map covers `S` and the standby's cursor proves it is unmodified since `S`. Otherwise the digest pass runs.
- **Derived state.** After the fast-forward the standby publishes a new generation, rebuilding ART and HNSW state as on open.

### 5.6 Operator surface

| Kind | Item |
|---|---|
| Config | `[replication] recovery_window`, `recovery_window_max_bytes`, `on_unrecoverable`, `auto_recover = off \| archive \| full` (default `off`, open question 6), `max_standby_lag` |
| Config | `[replication.archive] enabled, dir, segment_max_bytes, segment_max_secs, archive_command, standby_archive` |
| CLI | `heliosdb-nano standby recover --from-archive [DIR…] \| --from-primary [--full] [--rebind] --data-dir D` |
| CLI | `heliosdb-nano archive list \| catalog --start-with DIR \| verify [--deep] \| gaps --from POS \| prune` |
| SQL | `heliosdb_standby_status`, `heliosdb_recovery_window`, `heliosdb_archived_wal`, `heliosdb_archive_gaps`; `pg_is_in_recovery()` |
| HTTP | `/health` (liveness, with a replication block) and `/ready` (readiness) |

## 6. Phases

Each phase follows the project's usual gates: proof-first red→green tests, full default and internal suites, doc, fmt, new-findings-only clippy, deny, audit, ha_integration with no skips, pg35, and the CI perf smoke. Live real-daemon probes include the SIGKILL variants, which found `0c9e1a4faab6`.

| Phase | Delivers | Depends on | Acceptance (beyond the gates) |
|---|---|---|---|
| **R0** | §5.1 for the logical WAL (time marks, cap, chunked catch-up, J1b per-standby floor) and §5.2 (stay up, states, `/ready`, typed refusal, unbound fix, status view) | nothing | 1. Field J again, plus crash probes. 2. A standby outside the window stays up in `needs_recovery`: typed SQL refusal, `/ready` 503, process alive; widening the window on the primary lets it recover on its next retry. 3. Time pruning with second-scale windows; the cap enforced with its WARN. 4. A fresh standby refused before binding restarts without `UnboundReceiver`. 5. Catching up 1M entries uses bounded memory. 6. Primary write latency A/B unchanged. |
| **R1** | Remote TLS for both channels; resumable snapshot; recovery window applied to the RocksDB archive; physical modules reviewed (drop "EXTERNAL DRAFT") | — | A rebuild across two hosts or network namespaces; a download killed midway resumes; an encrypted dataset works; a cross-history source is refused. |
| **R2** | Serving physical standby: cut markers, generation publication, readiness, start path, logical→physical conversion guide | R1 | `REPLICATION_4_41_1_DESIGN.md` §6 steps 3 and 5; reopen cost measured at 10 MB / 1 GB / 10 GB; no local write path on a standby (every interface). |
| **R3** | Archive segments, catalog, archive CLI and views, `standby recover --from-archive`, standby-side archive | R1 (can recover a non-serving staged replica before R2) | Simulated multi-day outage (small windows): segments copied by the operator → `recover --from-archive` → exact data. A corrupt segment is refused by name; a missing one gives the exact gap; a cross-history segment is refused; a crash mid-apply resumes; the catalog is rebuilt from files. |
| **R4** | Recovery ladder and incremental fast-forward (+ optional R4b) | R2, R3 | 1 GB dataset with 1% changed beyond retention: bytes shipped ≈ changed leaves (ratio reported). Whole-keyspace digest equal afterwards. A kill during leaf apply resumes. Primary writes during the operation arrive through the tail. The full-copy switch fires above the threshold. |
| **R5** | Durable epoch, fencing, promotion (`REPLICATION_4_41_1_DESIGN.md` §3.4) | R2 | per that design. Segments and cursors already carry the epoch. |

R0 is independent and can start right after review. R3 and R4 can be built in parallel once R1 lands.

## 7. Risks

- **Disk growth.** A 7-day window on a write-heavy primary can be large. The cap, a WARN when it bites, docs on sizing, and the archive (which moves history off the primary's RocksDB) mitigate this.
- **Compaction cost.** Keeping row images inside RocksDB for days costs compaction work. The archive lets the in-database window shrink.
- **Generation reopen cost (R2)** is the largest unknown. It decides between per-generation reopen, a secondary instance, or the logical-only fallback.
- **RocksDB WAL archive semantics** on 8.10: contiguous `get_updates_since` across archived logs, TTL deletion timing, and recycling off. Verify before relying on them.
- **SST compatibility.** Both sides must run the same RocksDB format version (check it in the handshake), and the interplay of ingest, global sequence numbers and the receiver cursor's `local_sequence` check must be verified.
- **Confidentiality.** Segments and SSTs carry plaintext keys (row ids, index keys), so they are sealed and 0600. HNSW snapshot files are unencrypted today, which is a pre-existing gap and matters if they are shipped.
- **Clock.** Time marks depend on the wall clock. They are clamped to be monotonic, and pruning is bounded by LSN order.
- **Security.** Binding on a refusal (§5.2 option a) must not let a peer substitute a history. The plaintext bearer token over remote links stays refused until TLS (R1).

## 8. Questions for the Fable review

1. Should the physical standby become the serving standby (recommended), or should we take the logical-only route of §4?
2. In `needs_recovery`, should data reads be refused (recommended), or served stale with a bound?
3. What should `recovery_window_max_bytes` default to, and what happens on overflow? Recommended: prune the oldest history with a WARN and never stall primary writes. The Oracle-style alternative is to stall the primary.
4. Is "7 days of primary history so a standby can catch up" the intended meaning of the recovery window?
5. Remote transport: in-process TLS on both channels (recommended), or keep requiring a tunnel?
6. What should `auto_recover` default to: `off` (recommended at first), `archive`, or `full`?
7. Should standby-side archiving be on by default?
8. Is R4b change tracking worth a background WAL tailer on the primary, or is the digest pass enough?
9. Leaf size (about 4 MiB), fan-out (64) and the full-copy threshold (50%): are these right, or should they be tunable?
10. J1's `logical_wal_retention_entries` was never released. Should it be removed in favour of `recovery_window`, or kept as a minimum count?
