# P4.1c — Write groups of any size replicate: the poll limit is a coalescing target, not a ceiling

Plan: `docs/plans/PHYSICAL_STANDBY_REPLICATION.md` §D1 (one write group = one unit of
replication, never split) and §D4 (archive). Sprinter `c3a87c995ad2` (critical). Found by the
P4.1 live probe (`docs/plans/specs/P4_1_recover_from_primary_fast_forward.md`). Status: spec.
Implementation agents never run cargo or git.

## 1. The defect (verified on 3e0ea92e, release b5e66988)

A 20 000-row `INSERT INTO t SELECT i, repeat('x', 900) || i FROM generate_series(a, a+19999)`
is one RocksDB write group of ~80 MiB (the WAL of 1 M such rows measured 4 105 MiB). With a
physical standby attached, the standby stops at the first such group and never advances:
`/health` shows `state: reconnecting`, `cursor 0/2C099`, `primary_cursor 0/B2CA07`, reason
`physical replication refused: source not serving: physical transfer source: invalid physical
replication input: single source batch exceeds poll byte limit`. With the archive on, the
archiver fails at the same sequence (archiver.rs ~1159: `Err(error)` → complaint
"cannot read the store's WAL after sequence N", retried forever) and no segment past N is
ever sealed: the recovery window silently ends there.

Three ceilings cause it:

| where | today | effect |
|---|---|---|
| `poll_source` / `poll_source_cascade` (physical.rs ~435, ~800) | `batch.size_in_bytes() > byte_limit` → `invalid("single source batch exceeds poll byte limit")` | a group larger than the caller's limit is never served |
| `STANDBY_POLL_BYTES` (main.rs:4037) = 8 MiB; `PollRequest` carries it (physical_wire.rs ~2274, client ~2985 `bytes > byte_limit`) | the standby's limit | 8 MiB ceiling per group |
| `MAX_BATCH_BYTES` (physical.rs:67) = 64 MiB, `MAX_BATCH_OPERATIONS` = 1 000 000 | bounds `Kind::Batch` (`valid_size`), `walk_raw_batch`, `PhysicalBatch::decode`, `PollRequest` validation, archive `MAX_FRAME_LEN` (archive_segment.rs:79) | even a 64 MiB poll limit cannot carry an 80 MiB group; a 100 k-row statement (~400 MiB) or a 1 M-row one (~4 GiB) is impossible everywhere |

A write group is a statement or a transaction: it is the unit of atomicity and is applied
as one `WriteBatch`. It is never split. The primary already held the group in memory to
commit it; a standby holds it once more to apply it. The only true bound is the frame length
field (u32) on the wire and in segments.

## 2. The change

### 2.1 Limits (physical.rs)

- `pub const MAX_BATCH_BYTES: usize = u32::MAX as usize - 4096;` — the largest raw write
  group one wire frame or one segment frame can carry (u32 length minus every meta/sealing
  prefix, with slack). Document it as a representability bound, not a policy.
- `MAX_BATCH_OPERATIONS` → `u32::MAX` (RocksDB's own count width); `walk_raw_batch` keeps
  every structural check and the `count == 0` refusal.
- `poll_source` and `poll_source_cascade`: the byte limit is a **coalescing target**. The
  first group of a poll is always returned whatever its size; a later group is appended only
  while `bytes + group ≤ byte_limit`. The `invalid("single source batch exceeds poll byte
  limit")` branches are deleted. `byte_limit == 0` stays invalid; `byte_limit > MAX_BATCH_BYTES`
  stays invalid.

### 2.2 Wire (physical_wire.rs)

- `PollRequest`: `byte_limit` is a u32 on the wire; its validation keeps `0 < limit ≤
  MAX_BATCH_BYTES`. The server's contiguity loop (~2305) and the client's (~2985) accept
  `bytes > byte_limit` **only for the first batch of the poll** (`bytes > byte_limit &&
  index > 0` is the error). `PollSummary::validate`'s `batches ≤ byte_limit / 12` stays.
- `Kind::Batch` `valid_size`: `BATCH_META + 12 ..= BATCH_META + MAX_BATCH_BYTES`.
- `read_frame`: the header read stays bounded by `frame_timeout`. The payload is read in
  steps of at most `FRAME_IO_STEP = 1 MiB`, each step bounded by `frame_timeout` (an idle
  timeout: a peer that keeps sending never times out; one that stalls 30 s does). Allocation
  of `vec![0; length]` happens after `valid_size`, as today; document the trust boundary in
  a comment on `Kind::Batch`: a `Batch` frame is only ever accepted from the authenticated
  source the receiver follows and applies verbatim, so its size is bounded by what that
  source committed.
- `write_frame`: payloads larger than `FRAME_IO_STEP` are written in steps of that size,
  each bounded by `frame_timeout` (same idle semantics). The P4.1b one-write rule for small
  frames stays.
- `VERSION` (NPHYS002 → NPHYS003): the frame bounds changed, so an old peer refuses new
  frames and vice versa by version, not by a late "invalid length". Update every place that
  prints or tests the version string (Refuse message, `an_nphys001_client_is_refused_with_version`
  style tests, docs). No compatibility shim.

### 2.3 Receiver (physical_receiver.rs, physical.rs apply)

- `STANDBY_POLL_BYTES` stays 8 MiB as the coalescing target; `poll_byte_limit` validation
  keeps `≤ MAX_BATCH_BYTES`.
- `LocalWriter::apply_source*` and the applier take the batch as today (one `WriteBatch`);
  no change except that `walk_raw_batch` accepts the new bounds. Verify the applier's
  decoded-ops vector and pre-read map have no fixed capacity assumptions tied to 64 MiB.

### 2.4 Archive (archive_segment.rs, archiver.rs, archive_recovery.rs, physical_source.rs)

- `MAX_FRAME_LEN` follows the new `MAX_BATCH_BYTES` (u32 length field, already used).
- Segment roll: `segment_max_bytes` is a **soft** target. A batch that alone exceeds it goes
  into a segment of its own: if the open segment holds anything, roll first; append; roll
  after. `session.append` must never return `false` for size. The sealed (AES-GCM) frame of
  such a batch is one ciphertext; `MIN_CIPHERTEXT_LEN` accounting unchanged.
- The segment reader allocates per frame after checking `size ≤ MAX_FRAME_LEN` (lines ~524,
  ~648, ~928, ~1083), as today.
- `archive verify --deep`, the archive feed (`poll_or_fill`), `recover --from-archive` and
  the cascade source (`poll_source_cascade`) need no new logic beyond §2.1; the agent checks
  each for a hard-coded 64 MiB or `MAX_BATCH_BYTES`-derived buffer and reports it.

### 2.5 Docs

- `docs/guides/physical-standby.md`: in the limits section, replace any "64 MiB" / batch-size
  wording with: one write group travels and is applied as one unit; the standby and the
  archiver must hold the largest write group the primary commits in memory once (a 1 M-row
  single statement is ~4 GiB); the poll limit (8 MiB) only decides how many small groups
  travel together.
- `docs/guides/deployment-endpoints.md` and the skill/verb-map wherever `NPHYS002` appears.
- `CHANGELOG.md` Unreleased → Fixed: the stall, in the maintainer's words (one 20 k-row
  statement stalled every physical standby and the archive for good).

## 3. Tests (red first)

Unit:
- physical.rs: a store with one 100 MiB write group (one `WriteBatch` of 100 × 1 MiB values)
  followed by two small ones; `poll_source(after, 8 MiB)` returns the big group alone with
  `through` at its last sequence; the next poll returns the two small ones together. Same for
  `poll_source_cascade`. `PhysicalBatch::decode` accepts it; `walk_raw_batch` with 2 000 000
  operations passes.
- physical_wire.rs: loopback source/client with `frame_timeout = 1 s`: a 100 MiB `Batch`
  frame delivered by a peer that writes 1 MiB every 200 ms succeeds (idle semantics); a
  peer that stops mid-payload for 2 s fails with the timeout. The server→client poll with a
  first batch above `byte_limit` is accepted; a second batch that crosses it is refused.
- archive_segment.rs / archiver.rs: a 100 MiB batch is sealed into a segment of its own
  (roll before and after), read back equal, `verify --deep` passes; sealed variant too.

Integration (real daemons, `tests/support/physical_pair.rs`):
- `tests/physical_large_write_group.rs`: primary with the archive on, standby attached;
  `INSERT … SELECT … generate_series(1, 20000)` with 900-byte values (one ~80 MiB group), then
  a 100 k-row statement (~400 MiB) if the harness can afford it in the default tier (else the
  internal tier); the standby settles equal (`range_digest` of the default family equal at the
  cursor), `/health` never reports `reconnecting` with the "exceeds poll byte limit" reason;
  the archiver's catalog shows a segment covering the group; `recover --from-archive` of a
  fresh copy applies it; a cascaded standby follows it.
- SIGKILL the standby while the big frame is in flight (kill hook or timing) → restart
  resumes from the cursor, nothing partial applied, settles equal.

## 4. Gates

Pre-commit gate (`p41gate.sh`), lint (0 new clippy findings), phase gate, pg35 (no flips),
perf smoke, and the 1 GB live probe re-run with 20 000-row statements
(`jlive/ff_probe.sh` load) — the standby must follow the bulk load live, with the streaming
rate reported in MB/s of WAL bytes.

## 5. Out of scope

Pipelined polls / more than one poll in flight; compressing frames; the applier's per-row
publish cost (measured separately); Sprinter `6146b39a5beb`.
