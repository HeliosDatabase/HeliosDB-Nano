//! Physical replication primitives for resync staging and the standby's writer.
//!
//! Source positions are RocksDB sequence numbers, never logical Nano LSNs. A
//! receiver applies its primary's write batches verbatim, so its default
//! column family is a byte-for-byte copy of the primary's at the same source
//! sequence. Everything node-local — the receiver cursor today, the standby's
//! posture and archive catalog later — lives in the separate [`LOCAL_CF`]
//! column family, and every write a receiver makes goes through one
//! [`LocalWriter`], which keeps the store's sequence number accounted for.
//! These primitives do not publish changes to a serving `StorageEngine`: its
//! catalog, indexes and caches require a separate publication protocol, which
//! `replication::physical_applier` implements on top of [`LocalWriter`].
//!
//! Spec P4.1 §4: an incremental fast-forward from the primary
//! (`recover --from-primary`) replaces key ranges of the default column
//! family wholesale — range deletions and ingested SST files — whose
//! sequence cost RocksDB decides. It writes through its own
//! [`FastForwardWriter`], never through the [`LocalWriter`], and the
//! durable [`FastForwardRecord`] it keeps in [`LOCAL_CF`] makes every other
//! open of the directory refuse it ([`PhysicalError::FastForwardInterrupted`])
//! until the fast-forward has re-cut the stage.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use rocksdb::{
    ColumnFamily, Direction, IngestExternalFileOptions, IteratorMode, Options, ReadOptions, WriteBatch, WriteOptions,
    DB,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::storage::StorageEngine;
use crate::Config;

/// The node-local column family, owned by `storage::keyspace` (the engine
/// refuses every open mode but the standby's on a directory that holds it).
pub use crate::storage::keyspace::LOCAL_CF;

const FORMAT: u32 = 1;
/// The receiver cursor: a key INSIDE [`LOCAL_CF`], never in the replicated
/// default column family.
pub const CURSOR_KEY: &[u8] = b"cursor";
/// Where a receiver kept its cursor before the node-local namespace: in the
/// default column family. A staging directory holding it is refused with
/// [`PhysicalError::FullResyncRequired`], a source batch carrying it is
/// refused as receiver-local progress, and the engine refuses to open a store
/// holding it (`storage::engine`).
const LEGACY_CURSOR_KEY: &[u8] = b"__nano_physical_replica_cursor_v1";
/// Spec P4.1 §4: the record of a fast-forward in progress, a key INSIDE
/// [`LOCAL_CF`] like the cursor. While it exists the directory is a mix of
/// two cuts and nothing but the fast-forward may open it.
pub const FAST_FORWARD_KEY: &[u8] = b"ff:dirty";
/// Spec P4.1 §4: one key per leaf the fast-forward in progress has replaced,
/// `FAST_FORWARD_DONE_PREFIX ‖ leaf (u32, big-endian)` → the cut (u64,
/// big-endian), in [`LOCAL_CF`]. Written with each batch, so a batch costs a
/// write of its own leaves only; cleared with the record.
pub const FAST_FORWARD_DONE_PREFIX: &[u8] = b"ff:done:";
/// The first key past every [`FAST_FORWARD_DONE_PREFIX`] key.
const FAST_FORWARD_DONE_END: &[u8] = b"ff:done;";
/// The record's bound: a few fixed fields.
const MAX_FAST_FORWARD_RECORD_BYTES: usize = 4096;
const MAX_FAST_FORWARD_LEAVES: usize = 1 << 24;
/// Spec P4.1c: the largest raw write group ONE replication `Batch` frame or
/// ONE archived segment frame can carry. A representability bound, not a
/// policy: both frames give their length as a `u32`, and the 4096 bytes kept
/// back hold every prefix a frame adds to the raw group (the `Batch`
/// metadata, a segment frame's metadata and its AES-GCM sealing), with slack.
/// A write group is the unit of atomicity and is never split, so this is the
/// only bound on its size; how many groups travel together is a poll's byte
/// limit, a coalescing target ([`poll_source`]).
pub const MAX_BATCH_BYTES: usize = u32::MAX as usize - 4096;
/// RocksDB's own operation-count width (a raw batch's count field is a
/// `u32`): every count a source batch can carry is accepted (spec P4.1c).
const MAX_BATCH_OPERATIONS: u32 = u32::MAX;
/// A node-local batch ([`LocalWriter::write_local`]) is never a source
/// group: it keeps the bound every batch had before spec P4.1c, far below
/// the count field's width (its cursor put is one more operation).
const MAX_LOCAL_BATCH_OPERATIONS: usize = 1_000_000;
/// A receiver's own WAL record as a cascading source reads it: one source
/// group (at most [`MAX_BATCH_BYTES`]) plus the cursor put its writer adds in
/// the same write group (a record of a few hundred bytes, [`decode_cursor`]
/// refuses one over 2 KiB), which the bytes [`MAX_BATCH_BYTES`] keeps back
/// below `u32::MAX` cover.
const MAX_RECEIVER_RECORD_BYTES: usize = u32::MAX as usize;
// RocksDB encodes its sequence into the high 56 bits of an internal key.
const MAX_SEQUENCE: u64 = (1_u64 << 56) - 1;

/// The operator's remedy for a directory a fast-forward left between two
/// cuts (spec P4.1 §4): the same command finishes it. `data_dir` and
/// `primary_host` are the directory and the primary's replication port as
/// the caller knows them.
pub fn fast_forward_remedy(from: u64, to: u64, data_dir: &str, primary_host: &str) -> String {
    format!(
        "fast-forward from the primary interrupted at sequence {from}→{to}: run `heliosdb-nano recover \
         --from-primary --data-dir {data_dir} --primary-host {primary_host}` to finish it"
    )
}

#[derive(Debug, thiserror::Error)]
pub enum PhysicalError {
    #[error("physical replication storage error: {0}")]
    Storage(#[from] rocksdb::Error),
    #[error("invalid physical replication input: {0}")]
    Invalid(String),
    #[error("physical replication interval missing: expected {expected}, observed {observed}")]
    Gap { expected: u64, observed: u64 },
    /// The source's retained WAL starts after the sequence a receiver needs
    /// (`oldest` is the first sequence it still has; `None` when it retains no
    /// record of the interval at all): the receiver cannot be continued from
    /// this source. Spec P2.1: reported to the receiver as `WalPruned`.
    #[error("physical replication WAL pruned: sequence {wanted} is no longer retained")]
    Pruned { wanted: u64, oldest: Option<u64> },
    /// The receiver claims a position its source never reached: the source is
    /// not the dataset the receiver followed (restored from an older copy,
    /// say), whatever its history identity says.
    #[error("physical receiver is at sequence {after}, ahead of its source's {barrier}")]
    Ahead { after: u64, barrier: u64 },
    #[error("physical replication history mismatch")]
    HistoryMismatch,
    #[error("physical receiver must be reopened after an uncertain storage failure")]
    Poisoned,
    /// The directory cannot be continued in place; only a fresh resync can
    /// rebuild it. Nothing was modified.
    #[error("physical replication requires a full resync: {0}")]
    FullResyncRequired(&'static str),
    /// Spec P4.1 §4: a fast-forward from the primary (source sequence
    /// `from` → `to`) stopped before it re-cut the stage: the default column
    /// family holds ranges of both cuts, so the cursor accounts for nothing.
    /// Only `recover --from-primary` may open the directory, and finishes it.
    /// The text is [`fast_forward_remedy`]'s with the directory as `D` and
    /// the primary as `H`; a caller that knows both names them.
    #[error(
        "fast-forward from the primary interrupted at sequence {from}→{to}: run `heliosdb-nano recover \
         --from-primary --data-dir D --primary-host H` to finish it"
    )]
    FastForwardInterrupted { from: u64, to: u64 },
}

type Result<T> = std::result::Result<T, PhysicalError>;

fn invalid(message: impl Into<String>) -> PhysicalError {
    PhysicalError::Invalid(message.into())
}

fn check_history(history: Uuid) -> Result<()> {
    if history.is_nil() {
        return Err(invalid("nil history identity"));
    }
    Ok(())
}

fn check_local_advance(current: u64, operations: u64) -> Result<()> {
    if operations == 0 || current.checked_add(operations).is_none_or(|last| last > MAX_SEQUENCE) {
        return Err(invalid("receiver-local RocksDB sequence exhausted"));
    }
    Ok(())
}

/// A complete, checksummed source batch. Its fields are private so a caller
/// cannot bypass validation before passing it to a staging receiver.
#[derive(Clone, Debug)]
pub struct PhysicalBatch {
    history: Uuid,
    first: u64,
    last: u64,
    digest: [u8; 32],
    raw: Vec<u8>,
}

impl PhysicalBatch {
    /// Validate a bounded wire payload *before* constructing a C++ WriteBatch.
    /// The transport must enforce MAX_BATCH_BYTES before allocating its frame.
    pub fn decode(history: Uuid, first: u64, raw: Vec<u8>, expected_digest: [u8; 32]) -> Result<Self> {
        check_history(history)?;
        let count = validate_raw_batch(&raw)?;
        let encoded_first = u64::from_le_bytes(
            raw.get(..8)
                .and_then(|slice| slice.try_into().ok())
                .ok_or_else(|| invalid("batch header too short"))?,
        );
        if first == 0 || first != encoded_first {
            return Err(invalid("batch source sequence disagrees with its header"));
        }
        let last = first
            .checked_add(u64::from(count) - 1)
            .filter(|last| *last <= MAX_SEQUENCE)
            .ok_or_else(|| invalid("source sequence overflow"))?;
        let digest: [u8; 32] = Sha256::digest(&raw).into();
        if digest != expected_digest {
            return Err(invalid("batch checksum mismatch"));
        }
        Ok(Self {
            history,
            first,
            last,
            digest,
            raw,
        })
    }

    pub fn history(&self) -> Uuid {
        self.history
    }

    pub fn first_sequence(&self) -> u64 {
        self.first
    }

    pub fn last_sequence(&self) -> u64 {
        self.last
    }

    pub fn digest(&self) -> [u8; 32] {
        self.digest
    }

    pub fn data(&self) -> &[u8] {
        &self.raw
    }
}

/// One sequence-consuming record of a raw default-column-family write batch,
/// borrowing its key and value from the batch bytes.
///
/// `rocksdb::WriteBatchIterator` reports puts and deletes only — it drops range
/// deletions and column-family ids — so whoever must know exactly what a batch
/// did (a physical standby's applier, publishing derived state) reads the raw
/// records through [`walk_raw_batch`] instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawOp<'a> {
    /// `kTypeValue`.
    Put { key: &'a [u8], value: &'a [u8] },
    /// `kTypeDeletion` / `kTypeSingleDeletion`.
    Delete { key: &'a [u8] },
    /// `kTypeRangeDeletion` over the half-open interval `[start, end)`.
    DeleteRange { start: &'a [u8], end: &'a [u8] },
}

/// Decode the pinned RocksDB 8.10 default-column-family batch format. Preserve
/// the original bytes for application; the C API's put/delete callback loses
/// range deletions. Unknown opcodes fail closed rather than silently advancing,
/// and that includes every column-family opcode: a receiver's own WAL records
/// carry its `put_cf` into [`LOCAL_CF`] and must never be shipped as a source.
fn validate_raw_batch(raw: &[u8]) -> Result<u32> {
    walk_raw_batch(raw, |_| ())
}

/// The one record walker behind [`validate_raw_batch`] and the physical
/// applier's `decode_raw_batch`: every check `validate_raw_batch` makes, with
/// `visit` called for each sequence-consuming record in batch order. A batch
/// that fails a check may already have visited a prefix of its records, so a
/// caller acts on what it collected only after `Ok`.
pub(crate) fn walk_raw_batch<'a>(raw: &'a [u8], mut visit: impl FnMut(RawOp<'a>)) -> Result<u32> {
    if !(12..=MAX_BATCH_BYTES).contains(&raw.len()) {
        return Err(invalid("batch byte limit/header"));
    }
    let count = u32::from_le_bytes(
        raw.get(8..12)
            .and_then(|slice| slice.try_into().ok())
            .ok_or_else(|| invalid("batch header too short"))?,
    );
    // A write group with no operations consumes no sequence number and is
    // never part of a source interval (`poll_source` skips it), so one that
    // arrives as a batch is refused by name rather than as oversized.
    if count == 0 {
        return Err(invalid(
            "empty batch: a write group with no operations is never a source batch",
        ));
    }
    // Spec P4.1c: every count the field can hold is accepted
    // ([`MAX_BATCH_OPERATIONS`] is RocksDB's own width). The walk below proves
    // it against the records, which the byte bound above bounds.
    let count_wanted = u64::from(count);
    let mut input = raw.get(12..).ok_or_else(|| invalid("batch header too short"))?;
    let mut observed = 0_u64;
    while let Some((&tag, rest)) = input.split_first() {
        input = rest;
        match tag {
            // kTypeDeletion / kTypeSingleDeletion.
            0 | 7 => {
                let key = take_slice(&mut input)?;
                check_source_key(key)?;
                visit(RawOp::Delete { key });
                observed += 1;
            }
            // kTypeValue.
            1 => {
                let key = take_slice(&mut input)?;
                check_source_key(key)?;
                let value = take_slice(&mut input)?;
                visit(RawOp::Put { key, value });
                observed += 1;
            }
            // kTypeLogData: does not consume a sequence number.
            3 => {
                let _blob = take_slice(&mut input)?;
            }
            // kTypeRangeDeletion, half-open interval [start, end). The cursor
            // lives in its own column family, which a default-family range
            // deletion cannot reach.
            15 => {
                let start = take_slice(&mut input)?;
                let end = take_slice(&mut input)?;
                if start > end {
                    return Err(invalid("invalid range deletion"));
                }
                visit(RawOp::DeleteRange { start, end });
                observed += 1;
            }
            _ => return Err(invalid(format!("unsupported RocksDB write opcode {tag}"))),
        }
        if observed > count_wanted {
            return Err(invalid("batch operation count mismatch"));
        }
    }
    if observed != count_wanted {
        return Err(invalid("batch operation count mismatch"));
    }
    Ok(count)
}

/// A source batch must not plant the legacy default-family cursor: applied, it
/// would make the receiver look like a pre-namespace staging directory.
fn check_source_key(key: &[u8]) -> Result<()> {
    if key == LEGACY_CURSOR_KEY {
        return Err(invalid("source batch contains receiver-local progress"));
    }
    Ok(())
}

/// A RocksDB varint32: a slice length, or a column-family id.
fn take_varint32(input: &mut &[u8]) -> Result<u32> {
    let mut value = 0_u32;
    for shift in (0..35).step_by(7) {
        let (&byte, rest) = input.split_first().ok_or_else(|| invalid("truncated length"))?;
        *input = rest;
        if shift == 28 && byte > 15 {
            return Err(invalid("length overflow"));
        }
        value |= u32::from(byte & 127) << shift;
        if byte < 128 {
            return Ok(value);
        }
    }
    Err(invalid("truncated value"))
}

fn take_slice<'a>(input: &mut &'a [u8]) -> Result<&'a [u8]> {
    let length = take_varint32(input)? as usize;
    if length > input.len() {
        return Err(invalid("truncated value"));
    }
    let (value, rest) = input.split_at(length);
    *input = rest;
    Ok(value)
}

/// A bounded contiguous part of the durable source interval. `through` can be
/// below `durable_barrier` when the caller's byte limit stopped the poll. No
/// empty result is returned across a missing interval.
#[derive(Debug)]
pub struct PhysicalPoll {
    pub batches: Vec<PhysicalBatch>,
    pub through: u64,
    pub durable_barrier: u64,
}

/// Refuse to read a receiver's store as a source: its WAL carries its own
/// node-local writes. A standby that serves a downstream standby (spec P3.2
/// §4) reads its store through [`poll_source_cascade`], which strips them.
///
/// A writable RocksDB open must name every column family, so the live handle
/// alone settles it for any store opened for writing; the MANIFEST
/// (`DB::list_cf`, at the handle's own path) also covers a read-only handle
/// that opened only the default family; the default-family key covers a
/// staging directory from before the node-local namespace. A MANIFEST that
/// cannot be read refuses too.
fn refuse_receiver_source(db: &DB) -> Result<()> {
    if db.cf_handle(LOCAL_CF).is_some()
        || DB::list_cf(&Options::default(), db.path())?
            .iter()
            .any(|family| family == LOCAL_CF)
        || db.get(LEGACY_CURSOR_KEY)?.is_some()
    {
        return Err(invalid("a physical receiver cannot be exported as a primary"));
    }
    Ok(())
}

/// Spec P4.1d: the highest durable barrier one source's polls have fsynced
/// the WAL for — `0` until its first poll. [`poll_source`] and
/// [`poll_source_cascade`] fsync only when the barrier they sampled is above
/// it, so a source polled again and again at an idle barrier (a caught-up
/// standby every poll interval, the archiver every `poll_ms`) pays one fsync
/// per barrier instead of one per poll.
///
/// Why skipping is safe. A poll samples its barrier — the store's latest
/// sequence, or a cascading source's cursor — BEFORE it fsyncs, and RocksDB
/// makes a sequence visible only once its write group is in the WAL, so that
/// fsync covers every write at or below the barrier. A later poll that samples
/// a barrier no higher has nothing new to make durable: every write it can
/// serve or report was covered by the earlier fsync. The engine's own commit
/// path syncs its writes as `wal_sync_mode` says; the poll's fsync only closes
/// the window for a write that raced the sample (visible, but not yet fsynced
/// by its writer), and that window is closed once per barrier.
///
/// One memo belongs to one source and one store — the primary's session, a
/// cascading standby's session, the archiver each own theirs — and is never
/// shared across stores: another store's barrier says nothing about this
/// one's WAL.
#[derive(Debug, Default)]
pub struct FlushMemo(AtomicU64);

impl FlushMemo {
    /// A memo that has fsynced nothing yet: the next poll fsyncs.
    pub fn new() -> Self {
        Self::default()
    }

    /// The highest barrier a poll of this memo fsynced the WAL for: every
    /// write at or below it was durable when that poll returned.
    pub fn last_flushed(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }

    /// Whether a poll that sampled `barrier` has anything to make durable:
    /// only a barrier above [`Self::last_flushed`] does.
    pub fn needs_flush(&self, barrier: u64) -> bool {
        barrier > self.last_flushed()
    }

    /// fsync `db`'s WAL for a poll that sampled `barrier` BEFORE this call,
    /// unless this memo already covers it. Returns whether it fsynced. A
    /// failed fsync leaves the memo where it was, so the next poll retries.
    fn flush(&self, db: &DB, barrier: u64) -> Result<bool> {
        if !self.needs_flush(barrier) {
            return Ok(false);
        }
        db.flush_wal(true)?;
        self.0.fetch_max(barrier, Ordering::AcqRel);
        Ok(true)
    }
}

/// Poll committed physical changes. The owner must ensure all source writes
/// retain the RocksDB WAL; this function detects gaps but cannot manufacture a
/// retention lease. Only use cursors from a completed checkpoint or full batch.
///
/// Spec P4.1c: `byte_limit` (`1..=MAX_BATCH_BYTES`) is a coalescing target,
/// not a ceiling. The first write group of a poll is returned whatever its
/// size — a group is the unit of atomicity, never split, and one larger than
/// the limit would otherwise never be served — and a later one joins it only
/// while the poll's raw bytes stay within the limit. So a limit of one byte
/// reads exactly one group.
///
/// Spec P4.1d: the reported barrier is durable — the WAL is fsynced after it
/// was sampled, unless `memo` (the caller's, one per source) shows an earlier
/// poll already fsynced it ([`FlushMemo`]).
pub fn poll_source(db: &DB, history: Uuid, after: u64, byte_limit: usize, memo: &FlushMemo) -> Result<PhysicalPoll> {
    check_history(history)?;
    if byte_limit == 0 || byte_limit > MAX_BATCH_BYTES {
        return Err(invalid("invalid poll byte limit"));
    }
    refuse_receiver_source(db)?;
    let barrier = db.latest_sequence_number();
    if barrier > MAX_SEQUENCE {
        return Err(invalid("source sequence overflow"));
    }
    if after > barrier {
        return Err(PhysicalError::Ahead { after, barrier });
    }
    // Sample the barrier first; fsync subsequently covers every sampled write
    // (once per barrier: `FlushMemo`).
    memo.flush(db, barrier)?;
    let mut result = PhysicalPoll {
        batches: Vec::new(),
        through: after,
        durable_barrier: barrier,
    };
    if after == barrier {
        return Ok(result);
    }
    let mut iterator = db.get_updates_since(after)?;
    let mut bytes = 0_usize;
    while result.through < barrier {
        let Some(entry) = iterator.next() else {
            iterator.status()?;
            // No record at all although the interval is not empty: the WAL
            // that held it is gone, or `after` is inside the last write group.
            if result.through == after {
                return Err(missing_after(db, after, barrier));
            }
            return Err(PhysicalError::Gap {
                expected: result.through + 1,
                observed: barrier,
            });
        };
        let (first, batch) = entry?;
        // RocksDB logs a write group with no operations (a `DB::write` of an
        // empty batch) at the next sequence number but consumes none: the
        // record after it starts at that same number. It changes nothing, so
        // it is not part of the interval; shipped, it would ask the receiver
        // to account for a sequence number nobody used.
        if batch.is_empty() {
            continue;
        }
        if first != result.through + 1 {
            // The very first record past the wanted sequence: its WAL was
            // purged, or `after` is inside a write group the iterator skipped.
            if result.through == after {
                return Err(missing_after(db, after, first));
            }
            return Err(PhysicalError::Gap {
                expected: result.through + 1,
                observed: first,
            });
        }
        // The first group travels whatever its size; a later one joins it
        // only within the limit (spec P4.1c).
        if !result.batches.is_empty() && bytes.saturating_add(batch.size_in_bytes()) > byte_limit {
            break;
        }
        let raw = batch.data().to_vec();
        let digest = Sha256::digest(&raw).into();
        let batch = PhysicalBatch::decode(history, first, raw, digest)?;
        if batch.last > barrier {
            return Err(invalid("source barrier cuts a batch"));
        }
        bytes += batch.raw.len();
        result.through = batch.last;
        result.batches.push(batch);
    }
    iterator.status()?;
    Ok(result)
}

/// Why the record that should follow `after` is missing (the first record
/// the iterator yields starts at `observed`). RocksDB's iterator skips the
/// write group that holds `after` (`DBWALIterator` drops every record that
/// starts at or before it), so a WAL purged by retention and a cursor INSIDE
/// a write group look alike from the iterator. The oldest record the WAL
/// still holds tells them apart: at or before `after`, the WAL is all there
/// and the cursor splits a group (a gap); past it, the records the receiver
/// needs are gone (spec P2.1: refused as `WalPruned`, never retried).
fn missing_after(db: &DB, after: u64, observed: u64) -> PhysicalError {
    match oldest_retained_sequence(db) {
        Ok(Some(oldest)) if oldest <= after => PhysicalError::Gap {
            expected: after + 1,
            observed,
        },
        Ok(oldest) => PhysicalError::Pruned {
            wanted: after + 1,
            oldest,
        },
        Err(error) => error,
    }
}

/// Spec P2.2: whether the source's WAL still holds the write group that
/// follows `after` — a leased snapshot cut at `after` can still be continued
/// from (else a resumed download would be refused `WalPruned` once it is
/// installed). Reads one record at most.
pub fn wal_retains_after(db: &DB, after: u64) -> Result<bool> {
    if after >= db.latest_sequence_number() {
        return Ok(true);
    }
    let mut iterator = db.get_updates_since(after)?;
    for entry in iterator.by_ref() {
        let (first, batch) = entry?;
        if batch.is_empty() {
            continue;
        }
        return Ok(first <= after.saturating_add(1));
    }
    iterator.status()?;
    Ok(false)
}

/// The first sequence of the oldest write group the source's WAL still
/// holds; `None` when it holds none. Read on the error path above, and by the
/// WAL archiver (spec P3.1) to name the interval its archive lacks when the
/// store no longer holds it.
pub fn oldest_retained_sequence(db: &DB) -> Result<Option<u64>> {
    // Sequences start at 1, so nothing is skipped from 0.
    let mut iterator = db.get_updates_since(0)?;
    for entry in iterator.by_ref() {
        let (first, batch) = entry?;
        if !batch.is_empty() {
            return Ok(Some(first));
        }
    }
    iterator.status()?;
    Ok(None)
}

// ---------------------------------------------------------------------------
// Cascading source (spec P3.2 §4): a physical standby serving its store.
// ---------------------------------------------------------------------------

/// The node-local family of a receiver's store that may serve as a cascading
/// source. Its MANIFEST must list exactly the default and the node-local
/// family: RocksDB names a family in a WAL record by a numeric id this crate
/// cannot read back from a handle, so "every non-default record belongs to
/// the node-local family" holds only when there is no third family.
fn cascade_local_family(db: &DB) -> Result<&ColumnFamily> {
    let local = db.cf_handle(LOCAL_CF).ok_or_else(|| {
        invalid("a cascading source must be a physical standby's store, opened with its node-local family")
    })?;
    let mut families = DB::list_cf(&Options::default(), db.path())?;
    families.sort();
    let mut expected = vec!["default".to_string(), LOCAL_CF.to_string()];
    expected.sort();
    if families != expected {
        return Err(invalid(format!(
            "a cascading source's store must hold exactly the default and the '{LOCAL_CF}' column families, not \
             {families:?}"
        )));
    }
    Ok(local)
}

/// A receiver's cursor as its store holds it NOW, while its writer may be
/// writing: one read of the cursor key, whose value is written in the same
/// write group as the data it accounts for, so the pair (source sequence,
/// local sequence) it carries is always consistent. Unlike the cursor a
/// writer adopts, it is not compared with the store's latest sequence — a
/// write landing between the two reads would make that comparison fail.
fn live_cursor(db: &DB, local: &ColumnFamily) -> Result<PhysicalCursor> {
    let bytes = db
        .get_cf(local, CURSOR_KEY)?
        .ok_or_else(|| invalid("missing physical receiver cursor"))?;
    decode_cursor(&bytes)
}

/// Decode and validate a receiver cursor as its node-local family stores it
/// (the value of [`CURSOR_KEY`]): a bounded JSON record whose fields must be
/// consistent with one another. Says nothing about which store it belongs to.
pub fn decode_cursor(bytes: &[u8]) -> Result<PhysicalCursor> {
    if bytes.len() > 2048 {
        return Err(invalid("oversized durable cursor"));
    }
    let cursor: PhysicalCursor = serde_json::from_slice(bytes).map_err(|_| invalid("malformed durable cursor"))?;
    cursor.validate()?;
    Ok(cursor)
}

/// The source sequence a cascading source serves up to: the cursor its own
/// store holds (spec P3.2 §4). Reported as the barrier of every cascaded
/// session, in the PRIMARY's sequence numbers.
pub fn receiver_source_barrier(db: &DB) -> Result<u64> {
    let local = cascade_local_family(db)?;
    Ok(live_cursor(db, local)?.source_sequence)
}

/// Where a cascading source last found the records of its own WAL that
/// follow a source sequence: `source sequence -> local sequence of the end of
/// the record that brought the store to it`. A downstream receiver asks in
/// the PRIMARY's numbering while the store's WAL is read in its own, and the
/// two drift apart by one per applied group (module documentation of
/// [`poll_source_cascade`]); without a hint the first poll of a session reads
/// the retained WAL from its oldest record. Bounded: the oldest positions are
/// forgotten first.
#[derive(Debug, Default)]
pub struct CascadeHint {
    positions: std::collections::BTreeMap<u64, u64>,
}

impl CascadeHint {
    const MAX_POSITIONS: usize = 4096;

    fn note(&mut self, source: u64, local_end: u64) {
        self.positions.insert(source, local_end);
        while self.positions.len() > Self::MAX_POSITIONS {
            if self.positions.pop_first().is_none() {
                break;
            }
        }
    }

    /// The local sequence to start reading at for a receiver at `after`:
    /// the end of the last known record at or below it (0, the oldest
    /// retained record, when none is known). Records between it and `after`
    /// are read and skipped.
    fn start_for(&self, after: u64) -> u64 {
        self.positions
            .range(..=after)
            .next_back()
            .map_or(0, |(_, local)| *local)
    }
}

/// The column-family id of a column-family record: never the default
/// family's (0), whose records carry no id.
fn expect_nondefault_family(input: &mut &[u8]) -> Result<()> {
    if take_varint32(input)? == 0 {
        return Err(invalid("a column-family record addressed to the default family"));
    }
    Ok(())
}

/// One record of a receiver's own WAL, split for a cascading source.
struct ReceiverRecord {
    /// The default-family records, byte for byte and in batch order (the
    /// primary's write group, without the receiver's additions).
    kept: Vec<u8>,
    /// Sequence-consuming default-family records.
    kept_count: u32,
    /// Every sequence-consuming record, node-local ones included: what the
    /// record occupied of the store's own sequence.
    total: u32,
    /// The last receiver cursor the record wrote (its `put_cf`).
    cursor: Option<PhysicalCursor>,
}

/// Split a receiver's WAL record into the default-family records it carried
/// verbatim and the node-local ones its writer added. Every column-family
/// record is node-local ([`cascade_local_family`] proved the store holds no
/// third family); a column-family opcode naming the default family, or any
/// opcode the source walker does not know, is refused.
fn split_receiver_record(raw: &[u8]) -> Result<ReceiverRecord> {
    if !(12..=MAX_RECEIVER_RECORD_BYTES).contains(&raw.len()) {
        return Err(invalid("batch byte limit/header"));
    }
    let count = u32::from_le_bytes(
        raw.get(8..12)
            .and_then(|slice| slice.try_into().ok())
            .ok_or_else(|| invalid("batch header too short"))?,
    );
    let body = raw.get(12..).ok_or_else(|| invalid("batch header too short"))?;
    let mut input = body;
    let mut record = ReceiverRecord {
        kept: Vec::with_capacity(body.len()),
        kept_count: 0,
        total: 0,
        cursor: None,
    };
    while let Some((&tag, rest)) = input.split_first() {
        let start = body.len() - input.len();
        input = rest;
        // (keep it, does it consume a sequence number)
        let (keep, sequenced) = match tag {
            // kTypeDeletion / kTypeSingleDeletion.
            0 | 7 => {
                check_source_key(take_slice(&mut input)?)?;
                (true, true)
            }
            // kTypeValue.
            1 => {
                check_source_key(take_slice(&mut input)?)?;
                let _value = take_slice(&mut input)?;
                (true, true)
            }
            // kTypeLogData: the primary's blob; consumes no sequence number.
            3 => {
                let _blob = take_slice(&mut input)?;
                (true, false)
            }
            // kTypeRangeDeletion.
            15 => {
                let _start = take_slice(&mut input)?;
                let _end = take_slice(&mut input)?;
                (true, true)
            }
            // kTypeColumnFamilyDeletion / kTypeColumnFamilySingleDeletion.
            4 | 8 => {
                expect_nondefault_family(&mut input)?;
                let _key = take_slice(&mut input)?;
                (false, true)
            }
            // kTypeColumnFamilyValue: the receiver cursor, or node-local state.
            5 => {
                expect_nondefault_family(&mut input)?;
                let key = take_slice(&mut input)?;
                let value = take_slice(&mut input)?;
                if key == CURSOR_KEY {
                    record.cursor = Some(decode_cursor(value)?);
                }
                (false, true)
            }
            // kTypeColumnFamilyRangeDeletion.
            14 => {
                expect_nondefault_family(&mut input)?;
                let _start = take_slice(&mut input)?;
                let _end = take_slice(&mut input)?;
                (false, true)
            }
            _ => {
                return Err(invalid(format!(
                    "unsupported RocksDB write opcode {tag} in a receiver's WAL"
                )))
            }
        };
        if sequenced {
            record.total += 1;
            if keep {
                record.kept_count += 1;
            }
            if record.total > count {
                return Err(invalid("batch operation count mismatch"));
            }
        }
        if keep {
            let end = body.len() - input.len();
            let bytes = body.get(start..end).ok_or_else(|| invalid("record bounds"))?;
            record.kept.extend_from_slice(bytes);
        }
    }
    if record.total != count {
        return Err(invalid("batch operation count mismatch"));
    }
    Ok(record)
}

/// [`poll_source`] for a CASCADING source (spec P3.2 §4, opt-in with
/// `[replication.physical] cascade = true`): a physical standby serving its
/// own store to a downstream standby.
///
/// # Why not `poll_source`
///
/// A standby's WAL is its primary's write groups plus its own node-local
/// writes: every applied group also carries the `put_cf` of the receiver
/// cursor into [`LOCAL_CF`], and node-local state
/// ([`LocalWriter::write_local`]) is written there too. Shipped as they are,
/// those records would be refused downstream (`validate_raw_batch` refuses
/// every column-family opcode) — and would plant this standby's progress in
/// another store. So every record addressed to the node-local family is
/// STRIPPED, the default-family records are kept byte for byte, and the group
/// is re-framed with the primary's first sequence and its digest recomputed:
/// the result is byte for byte the primary's raw write group, digest
/// included.
///
/// # The sequence invariant
///
/// The served sequence numbers are the PRIMARY's, never this store's own. A
/// stripped record occupied a sequence number here, never at the primary:
/// this store's RocksDB sequence runs ahead of its source sequence by one per
/// applied group (the cursor put: `local_operations = last - first + 2` in
/// [`LocalWriter::apply_source_with`]) and by `n + 1` per node-local batch of
/// `n` operations ([`LocalWriter::write_local`]). The group a record carried
/// is therefore named by the cursor it wrote — `last_first_sequence ..=
/// source_sequence`, the primary's span — and a record that carried no group
/// (a node-local batch, the initial cursor) is skipped. A downstream receiver
/// keeps its own accounting unchanged: it applies the primary's groups and
/// adds its own cursor put, `last - first + 2` local sequences per group,
/// exactly as if it followed the primary itself, and it could follow the
/// primary directly from the same cursor.
///
/// The durable barrier is this store's durable cursor (its source sequence),
/// read before the WAL is fsynced — this standby's own WAL, which holds the
/// groups and the cursor puts that account for them, fsynced only when the
/// barrier is above `memo` (spec P4.1d, [`FlushMemo`]: the same rule as
/// [`poll_source`]; the cursor is written in the same write group as the data
/// it accounts for, so a barrier that did not move has no group to make
/// durable). A record the store holds from before it
/// became a receiver (its checkpoint's own WAL, at or below the snapshot cut)
/// carries no cursor and is skipped. A WAL that no longer holds the group
/// after `after` is [`PhysicalError::Pruned`], as from a primary.
pub fn poll_source_cascade(
    db: &DB,
    history: Uuid,
    after: u64,
    byte_limit: usize,
    hint: &mut CascadeHint,
    memo: &FlushMemo,
) -> Result<PhysicalPoll> {
    check_history(history)?;
    if byte_limit == 0 || byte_limit > MAX_BATCH_BYTES {
        return Err(invalid("invalid poll byte limit"));
    }
    let local = cascade_local_family(db)?;
    let current = live_cursor(db, local)?;
    if current.history != history {
        return Err(PhysicalError::HistoryMismatch);
    }
    let barrier = current.source_sequence;
    let local_barrier = current.local_sequence;
    if after > barrier {
        return Err(PhysicalError::Ahead { after, barrier });
    }
    // The cursor was read first; the fsync covers every write it accounts for
    // (once per barrier: `FlushMemo`).
    memo.flush(db, barrier)?;
    let mut result = PhysicalPoll {
        batches: Vec::new(),
        through: after,
        durable_barrier: barrier,
    };
    if after == barrier {
        return Ok(result);
    }
    let mut iterator = db.get_updates_since(hint.start_for(after))?;
    let mut bytes = 0_usize;
    // The first group the retained WAL showed: where it starts, when the
    // group after `after` is missing.
    let mut oldest_seen: Option<u64> = None;
    loop {
        let Some(entry) = iterator.next() else {
            iterator.status()?;
            break;
        };
        let (local_first, batch) = entry?;
        if local_first > local_barrier {
            break;
        }
        if batch.is_empty() {
            continue;
        }
        let record = split_receiver_record(batch.data())?;
        let local_end = local_first
            .checked_add(u64::from(record.total).saturating_sub(1))
            .ok_or_else(|| invalid("receiver-local sequence overflow"))?;
        let Some(cursor) = record.cursor else {
            // The checkpoint this receiver was built from may still hold its
            // own WAL: those groups are at or below the snapshot cut.
            if local_end <= current.snapshot_sequence {
                continue;
            }
            return Err(invalid("a receiver WAL record past the snapshot cut carries no cursor"));
        };
        if cursor.history != history {
            return Err(PhysicalError::HistoryMismatch);
        }
        hint.note(cursor.source_sequence, local_end);
        if record.kept_count == 0 {
            // Node-local state only: nothing of the primary's.
            continue;
        }
        let last = cursor.source_sequence;
        let first = cursor
            .last_first_sequence
            .ok_or_else(|| invalid("a receiver record carries a group but its cursor names none"))?;
        if last.checked_sub(first).and_then(|span| span.checked_add(1)) != Some(u64::from(record.kept_count)) {
            return Err(invalid(
                "a receiver record's cursor disagrees with the write group it carries",
            ));
        }
        let oldest = *oldest_seen.get_or_insert(first);
        if last <= result.through {
            continue;
        }
        if first != result.through + 1 {
            if result.through == after && first > after + 1 && oldest == first {
                return Err(PhysicalError::Pruned {
                    wanted: after + 1,
                    oldest: Some(first),
                });
            }
            return Err(PhysicalError::Gap {
                expected: result.through + 1,
                observed: first,
            });
        }
        let framed = record.kept.len().saturating_add(12);
        // As in `poll_source` (spec P4.1c): the first group travels whatever
        // its size; a later one joins it only within the limit.
        if !result.batches.is_empty() && bytes.saturating_add(framed) > byte_limit {
            break;
        }
        let mut raw = Vec::with_capacity(framed);
        raw.extend_from_slice(&first.to_le_bytes());
        raw.extend_from_slice(&record.kept_count.to_le_bytes());
        raw.extend_from_slice(&record.kept);
        let digest = Sha256::digest(&raw).into();
        let batch = PhysicalBatch::decode(history, first, raw, digest)?;
        if batch.last > barrier {
            return Err(invalid("source barrier cuts a batch"));
        }
        bytes += batch.raw.len();
        result.through = batch.last;
        result.batches.push(batch);
        if result.through == barrier {
            break;
        }
    }
    if result.through == after {
        return Err(match oldest_seen {
            Some(first) if first > after + 1 => PhysicalError::Pruned {
                wanted: after + 1,
                oldest: Some(first),
            },
            Some(_) => PhysicalError::Gap {
                expected: after + 1,
                observed: barrier,
            },
            None => PhysicalError::Pruned {
                wanted: after + 1,
                oldest: None,
            },
        });
    }
    Ok(result)
}

/// SHA-256 over every key/value pair of one column family whose key starts
/// with `prefix`, in key order, each pair framed as
/// `len(k) || k || len(v) || v` with 8-byte little-endian lengths.
///
/// Two stores hold the same range exactly when their digests agree: this is
/// how a physical standby proves its default column family (`cf = None`) is a
/// verbatim copy of its primary's at the same source sequence. The lengths
/// frame each field, so no two different listings hash alike. Iteration is
/// total-order — the engine's prefix extractor must not end it at a prefix
/// boundary — and reads one consistent view of the store.
pub fn range_digest(db: &DB, cf: Option<&str>, prefix: &[u8]) -> Result<[u8; 32]> {
    let mut read = ReadOptions::default();
    read.set_total_order_seek(true);
    let mode = IteratorMode::From(prefix, Direction::Forward);
    let iterator = match cf {
        None => db.iterator_opt(mode, read),
        Some(name) => {
            let family = db
                .cf_handle(name)
                .ok_or_else(|| invalid(format!("the store has no open column family named {name}")))?;
            db.iterator_cf_opt(family, read, mode)
        }
    };
    let mut hash = Sha256::new();
    for item in iterator {
        let (key, value) = item?;
        if !key.starts_with(prefix) {
            break;
        }
        for field in [&key, &value] {
            hash.update((field.len() as u64).to_le_bytes());
            hash.update(field);
        }
    }
    Ok(hash.finalize().into())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PhysicalCursor {
    format: u32,
    history: Uuid,
    snapshot_sequence: u64,
    source_sequence: u64,
    local_sequence: u64,
    last_first_sequence: Option<u64>,
    last_digest: Option<[u8; 32]>,
}

impl PhysicalCursor {
    pub fn history(&self) -> Uuid {
        self.history
    }

    /// The original verified snapshot cut, unchanged by physical continuation.
    pub fn snapshot_sequence(&self) -> u64 {
        self.snapshot_sequence
    }

    pub fn source_sequence(&self) -> u64 {
        self.source_sequence
    }

    /// The receiver store's own RocksDB sequence after its last write: every
    /// write a receiver makes is accounted here, so `latest_sequence_number`
    /// equals it whenever nothing else touched the store.
    pub fn local_sequence(&self) -> u64 {
        self.local_sequence
    }

    /// The source and local sequences are two counters of two stores: a
    /// receiver built from a checkpoint starts with the local one ahead,
    /// but a fast-forwarded one (spec P4.1 §4) takes the primary's position
    /// with its own counter, which may be behind it. Only the local counter's
    /// agreement with the store (`latest_sequence_number`) is a safety
    /// property, and the writers check it.
    fn validate(&self) -> Result<()> {
        check_history(self.history)?;
        if self.format != FORMAT
            || self.snapshot_sequence > self.source_sequence
            || self.source_sequence > MAX_SEQUENCE
            || self.local_sequence == 0
            || self.local_sequence > MAX_SEQUENCE
        {
            return Err(invalid("invalid durable cursor"));
        }
        match (self.last_first_sequence, self.last_digest) {
            (None, None) if self.source_sequence == self.snapshot_sequence => Ok(()),
            (Some(first), Some(_)) if first > self.snapshot_sequence && first <= self.source_sequence => Ok(()),
            _ => Err(invalid("inconsistent durable cursor batch identity")),
        }
    }
}

fn local_family(db: &DB) -> Result<&ColumnFamily> {
    db.cf_handle(LOCAL_CF)
        .ok_or_else(|| invalid("physical receiver was opened without its node-local column family"))
}

fn refuse_legacy_cursor(db: &DB) -> Result<()> {
    if db.get(LEGACY_CURSOR_KEY)?.is_some() {
        return Err(PhysicalError::FullResyncRequired(
            "the staging directory keeps its cursor in the default column family, so it predates the node-local \
             namespace; rebuild it",
        ));
    }
    Ok(())
}

fn read_cursor(db: &DB) -> Result<PhysicalCursor> {
    refuse_legacy_cursor(db)?;
    let Some(local) = db.cf_handle(LOCAL_CF) else {
        // A handle opened without the family is a caller's mistake; a store
        // without it was never initialized by this version and cannot resume.
        let stored = DB::list_cf(&Options::default(), db.path())?;
        return Err(if stored.iter().any(|family| family == LOCAL_CF) {
            invalid("physical receiver was opened without its node-local column family")
        } else {
            PhysicalError::FullResyncRequired(
                "the staging directory has no node-local column family: it predates the node-local namespace or \
                 was never initialized; rebuild it",
            )
        });
    };
    // Spec P4.1 §4: before the sequence equality, which an interrupted
    // fast-forward breaks by construction — it would otherwise read as a
    // store modified outside physical replication.
    refuse_interrupted_fast_forward(db, local)?;
    let bytes = db
        .get_cf(local, CURSOR_KEY)?
        .ok_or_else(|| invalid("missing physical receiver cursor"))?;
    if bytes.len() > 2048 {
        return Err(invalid("oversized durable cursor"));
    }
    let cursor: PhysicalCursor = serde_json::from_slice(&bytes).map_err(|_| invalid("malformed durable cursor"))?;
    cursor.validate()?;
    if db.latest_sequence_number() != cursor.local_sequence {
        return Err(invalid("receiver was modified outside physical replication"));
    }
    Ok(cursor)
}

/// [`PhysicalError::FastForwardInterrupted`] when the store carries a
/// fast-forward record. A record that cannot be decoded still means a
/// fast-forward was in progress, and is refused too.
fn refuse_interrupted_fast_forward(db: &DB, local: &ColumnFamily) -> Result<()> {
    let Some(bytes) = db.get_cf(local, FAST_FORWARD_KEY)? else {
        return Ok(());
    };
    let record = FastForwardRecord::decode(&bytes).map_err(|error| {
        invalid(format!(
            "the store carries an unreadable fast-forward record ({error}): a fast-forward from the primary was \
             interrupted; run `heliosdb-nano recover --from-primary` on it to finish it"
        ))
    })?;
    Err(PhysicalError::FastForwardInterrupted {
        from: record.from,
        to: record.to,
    })
}

/// The fast-forward record a receiver's store holds, if any (spec P4.1 §4).
/// `None` for a handle opened without [`LOCAL_CF`].
pub fn fast_forward_record(db: &DB) -> Result<Option<FastForwardRecord>> {
    let Some(local) = db.cf_handle(LOCAL_CF) else {
        return Ok(None);
    };
    db.get_cf(local, FAST_FORWARD_KEY)?
        .map(|bytes| FastForwardRecord::decode(&bytes))
        .transpose()
}

/// Spec P4.1 §4: the durable record of an incremental fast-forward in
/// progress, the value of [`FAST_FORWARD_KEY`] in [`LOCAL_CF`]. It is
/// written before the first default-column-family write of a fast-forward
/// and deleted by the write that re-cuts the cursor, so a store carrying it
/// is never taken for a consistent stage. It holds what the fast-forward is
/// (its cut and plan) only: the leaves of `snapshot_id`'s plan already
/// replaced are keys of their own ([`FAST_FORWARD_DONE_PREFIX`],
/// [`FastForwardWriter::mark_done`]), so a batch never rewrites the record.
/// A re-run against the same snapshot skips those leaves; a new cut clears
/// them with the record it writes ([`FastForwardWriter::start_record`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FastForwardRecord {
    pub format: u32,
    pub history: Uuid,
    /// The primary's export the leaves come from.
    pub snapshot_id: Uuid,
    /// The source sequence the standby was at (its cursor's, unchanged until
    /// the re-cut).
    pub from: u64,
    /// The cut the standby is fast-forwarded to.
    pub to: u64,
    /// The plan's leaf size.
    pub leaf_bytes: u32,
}

impl FastForwardRecord {
    /// A record of a new cut.
    pub fn new(history: Uuid, snapshot_id: Uuid, from: u64, to: u64, leaf_bytes: u32) -> Self {
        Self {
            format: FORMAT,
            history,
            snapshot_id,
            from,
            to,
            leaf_bytes,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.format != FORMAT
            || self.history.is_nil()
            || self.snapshot_id.is_nil()
            || self.from >= self.to
            || self.to > MAX_SEQUENCE
            || self.leaf_bytes == 0
        {
            return Err(invalid("invalid fast-forward record"));
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|error| invalid(error.to_string()))?;
        if bytes.len() > MAX_FAST_FORWARD_RECORD_BYTES {
            return Err(invalid("oversized fast-forward record"));
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_FAST_FORWARD_RECORD_BYTES {
            return Err(invalid("oversized fast-forward record"));
        }
        let record: Self = serde_json::from_slice(bytes)
            .map_err(|error| invalid(format!("malformed fast-forward record: {error}")))?;
        record.validate()?;
        Ok(record)
    }
}

/// The key that marks `leaf` replaced ([`FAST_FORWARD_DONE_PREFIX`]).
fn done_key(leaf: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(FAST_FORWARD_DONE_PREFIX.len() + 4);
    key.extend_from_slice(FAST_FORWARD_DONE_PREFIX);
    key.extend_from_slice(&leaf.to_be_bytes());
    key
}

fn sync_options() -> WriteOptions {
    write_options(Durability::Synced)
}

fn write_options(durability: Durability) -> WriteOptions {
    let mut options = WriteOptions::default();
    options.set_sync(durability == Durability::Synced);
    options
}

/// How one [`LocalWriter`] write reaches disk.
///
/// Either way the write is ONE RocksDB write group that carries its cursor,
/// so `latest_sequence_number == cursor.local_sequence` holds after it and
/// the data and its cursor survive or are lost together. What differs is only
/// when it is fsynced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durability {
    /// The write fsyncs the WAL: it, and every deferred write before it, is
    /// durable when the call returns.
    Synced,
    /// The write reaches the WAL (the OS page cache) without an fsync. A
    /// process crash loses nothing; a power or kernel failure may lose a
    /// tail of deferred writes — each together with its cursor, so the store
    /// reopens at an earlier, consistent cursor and re-fetches from there.
    /// Made durable by the next synced write or [`LocalWriter::sync`].
    Deferred,
}

fn encode_cursor(cursor: &PhysicalCursor) -> Result<Vec<u8>> {
    serde_json::to_vec(cursor).map_err(|e| invalid(e.to_string()))
}

#[derive(Clone, Debug)]
enum LocalOp {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
}

/// A batch of writes that can only address [`LOCAL_CF`]: node-local state a
/// physical standby persists (its posture, archive catalog, statistics).
///
/// It records operations rather than holding a RocksDB `WriteBatch` because a
/// `WriteBatch` names its column family by the numeric id of one open handle:
/// built against another handle it could land in a different family. The
/// [`LocalWriter`] encodes the operations against its own handle's family when
/// it applies them, together with its cursor.
#[derive(Clone, Debug, Default)]
pub struct LocalBatch {
    ops: Vec<LocalOp>,
}

impl LocalBatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) {
        self.ops.push(LocalOp::Put(key.to_vec(), value.to_vec()));
    }

    pub fn delete(&mut self, key: &[u8]) {
        self.ops.push(LocalOp::Delete(key.to_vec()));
    }

    /// Operations in the batch; each consumes one local sequence number.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

/// Every write a physical receiver makes goes through here: the applier's
/// source batches ([`Self::apply_source`]) and node-local state
/// ([`Self::write_local`]). It owns the cursor invariant
/// `db.latest_sequence_number() == cursor.local_sequence`, checked before and
/// after every write: a write from anywhere else moves the sequence, and the
/// next write here poisons the writer instead of building on a store it can no
/// longer account for.
///
/// Sequence accounting: a source batch consumes `last - first + 2` local
/// sequence numbers (its own operations plus the cursor put into
/// [`LOCAL_CF`]); a [`LocalBatch`] of `n` operations consumes `n + 1`.
/// Creating the family consumes none.
///
/// Durability: every write is fsynced unless it says otherwise
/// ([`Self::apply_source_with`] with [`Durability::Deferred`]). The writer
/// tracks the durable barrier — the cursor as of its last fsync — separately
/// from the applied cursor ([`Self::cursor`]); [`Self::sync`] brings the two
/// together, and so does any later synced write.
pub struct LocalWriter {
    db: Arc<DB>,
    cursor: Mutex<PhysicalCursor>,
    /// The cursor's `source_sequence` / `local_sequence` as of the last
    /// fsync that covered it. Only written while `cursor` is locked.
    durable_source: AtomicU64,
    durable_local: AtomicU64,
    poisoned: AtomicBool,
}

impl LocalWriter {
    /// Initialize a receiver on a validated, independently copied physical
    /// checkpoint: create [`LOCAL_CF`] and write the first cursor into it.
    ///
    /// rocksdb 0.22's `DB` is single-threaded over its family map —
    /// `create_cf` needs `&mut DB` — so the family is created here, before the
    /// handle is shared. The caller proves manifest/history integrity and holds
    /// the DB lock.
    pub fn from_checkpoint(db: DB, history: Uuid, checkpoint_sequence: u64) -> Result<Self> {
        Self::from_checkpoint_at(db, history, checkpoint_sequence, checkpoint_sequence)
    }

    /// [`Self::from_checkpoint`] for a checkpoint whose store's own sequence
    /// (`store_sequence`, its `latest_sequence_number`) differs from the
    /// source sequence it holds (`source_sequence`, in the primary's
    /// numbering). A checkpoint of a primary has the two equal. A CASCADED
    /// export (spec P3.2 §4) is a physical standby's store: its sequence
    /// counts that standby's own writes, while its replicated keyspace is the
    /// primary's at `source_sequence` — usually behind the store's counter,
    /// ahead of it once that standby was fast-forwarded (spec P4.1 §4). The
    /// receiver's cursor starts at `source_sequence`, so it continues in the
    /// primary's numbering — from its cascading source, or from the primary
    /// itself — and its own accounting starts at `store_sequence + 1`.
    pub fn from_checkpoint_at(mut db: DB, history: Uuid, source_sequence: u64, store_sequence: u64) -> Result<Self> {
        check_history(history)?;
        if source_sequence > MAX_SEQUENCE
            || db.latest_sequence_number() != store_sequence
            || db.cf_handle(LOCAL_CF).is_some()
            || db.get(LEGACY_CURSOR_KEY)?.is_some()
        {
            return Err(invalid("checkpoint sequence mismatch or existing receiver cursor"));
        }
        check_local_advance(store_sequence, 1)?;
        let cursor = PhysicalCursor {
            format: FORMAT,
            history,
            snapshot_sequence: source_sequence,
            source_sequence,
            local_sequence: store_sequence + 1,
            last_first_sequence: None,
            last_digest: None,
        };
        cursor.validate()?;
        let bytes = encode_cursor(&cursor)?;
        db.create_cf(LOCAL_CF, &StorageEngine::local_cf_options())?;
        // Creating a family is a MANIFEST edit, not a write. Should it ever
        // move the sequence, the cut no longer matches the checkpoint.
        if db.latest_sequence_number() != store_sequence {
            return Err(invalid(
                "creating the node-local column family moved the checkpoint sequence",
            ));
        }
        db.put_cf_opt(local_family(&db)?, CURSOR_KEY, bytes, &sync_options())?;
        if db.latest_sequence_number() != cursor.local_sequence {
            return Err(invalid("receiver was modified outside physical replication"));
        }
        Ok(Self::adopt(db, cursor))
    }

    /// A writer over `db` at `cursor`, which is durable: just written with an
    /// fsync, or read back from the store.
    fn adopt(db: DB, cursor: PhysicalCursor) -> Self {
        Self {
            db: Arc::new(db),
            durable_source: AtomicU64::new(cursor.source_sequence),
            durable_local: AtomicU64::new(cursor.local_sequence),
            cursor: Mutex::new(cursor),
            poisoned: AtomicBool::new(false),
        }
    }

    /// Reopen a receiver at its durable cursor without Nano logical recovery
    /// or maintenance. `db` must be opened with [`LOCAL_CF`]; a directory
    /// whose cursor is still in the default family, or that has no node-local
    /// family at all, is refused with [`PhysicalError::FullResyncRequired`].
    pub fn resume(db: DB, history: Uuid) -> Result<Self> {
        let cursor = read_cursor(&db)?;
        if cursor.history != history {
            return Err(PhysicalError::HistoryMismatch);
        }
        // What the store recovered is its durable state: a writable open
        // flushes the recovered memtables to fsynced tables (RocksDB's
        // default, which the engine does not override).
        Ok(Self::adopt(db, cursor))
    }

    /// The receiver's handle. Reads are free; a write through it moves the
    /// sequence and poisons this writer at its next write.
    pub fn db(&self) -> &Arc<DB> {
        &self.db
    }

    /// The cursor as of the last successful write: the applied position. It
    /// is durable once [`Self::durable_source_sequence`] reaches its
    /// `source_sequence` (always, unless a write was [`Durability::Deferred`]).
    pub fn cursor(&self) -> PhysicalCursor {
        self.cursor.lock().clone()
    }

    /// The source sequence of the durable barrier: the cursor as of the last
    /// fsync that covered it. Never above `cursor().source_sequence()`. Lock
    /// free, so it can be read while a write (and its fsync) is in flight.
    pub fn durable_source_sequence(&self) -> u64 {
        self.durable_source.load(Ordering::Acquire)
    }

    /// Whether every applied write is durable.
    fn is_synced(&self, cursor: &PhysicalCursor) -> bool {
        self.durable_local.load(Ordering::Acquire) == cursor.local_sequence
    }

    /// `cursor` (the one adopted under the lock) is now durable.
    fn mark_durable(&self, cursor: &PhysicalCursor) {
        self.durable_local.store(cursor.local_sequence, Ordering::Release);
        self.durable_source.store(cursor.source_sequence, Ordering::Release);
    }

    /// Make every deferred write durable: one WAL fsync, unless nothing is
    /// pending. Returns the durable barrier's source sequence, which is then
    /// the cursor's. A failed fsync leaves what reached the disk uncertain and
    /// poisons the writer, like a failed write.
    pub fn sync(&self) -> Result<u64> {
        let cursor = self.cursor.lock();
        if self.is_poisoned() {
            return Err(PhysicalError::Poisoned);
        }
        if !self.is_synced(&cursor) {
            self.check_accounted(&cursor)?;
            if let Err(error) = self.db.flush_wal(true) {
                self.poison();
                return Err(error.into());
            }
            self.mark_durable(&cursor);
        }
        Ok(cursor.source_sequence)
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    fn poison(&self) {
        self.poisoned.store(true, Ordering::Release);
    }

    /// Refuse to build on a store this writer can no longer account for.
    fn check_accounted(&self, cursor: &PhysicalCursor) -> Result<()> {
        if self.is_poisoned() {
            return Err(PhysicalError::Poisoned);
        }
        if self.db.latest_sequence_number() != cursor.local_sequence {
            self.poison();
            return Err(invalid("receiver was modified outside physical replication"));
        }
        Ok(())
    }

    /// Apply `batch` (which already carries `next`'s cursor put) in one
    /// write, fsynced unless `durability` defers it, then adopt `next`. A
    /// failed write leaves the outcome uncertain and poisons; so does a
    /// sequence that moved by anything but this write.
    fn commit(
        &self,
        cursor: &mut PhysicalCursor,
        next: PhysicalCursor,
        batch: WriteBatch,
        durability: Durability,
    ) -> Result<()> {
        if let Err(error) = self.db.write_opt(batch, &write_options(durability)) {
            self.poison();
            return Err(error.into());
        }
        *cursor = next;
        if self.db.latest_sequence_number() != cursor.local_sequence {
            self.poison();
            return Err(invalid("receiver was modified outside physical replication"));
        }
        if durability == Durability::Synced {
            // The fsync covered every earlier deferred write too.
            self.mark_durable(cursor);
        }
        Ok(())
    }

    /// Returns false only for an exact retransmission of the last applied
    /// batch. Old, overlapping, conflicting and gap-crossing batches refuse.
    /// The source bytes are applied verbatim to the default column family and
    /// the cursor goes to [`LOCAL_CF`], in the same fsynced write.
    pub fn apply_source(&self, batch: &PhysicalBatch) -> Result<bool> {
        self.apply_source_with(batch, Durability::Synced)
    }

    /// [`Self::apply_source`], fsynced or not by `durability`. The cursor is
    /// in the batch's own write either way (see [`Durability`]); a receiver
    /// applies the groups of one poll [`Durability::Deferred`] and then calls
    /// [`Self::sync`] once, so a poll costs one fsync instead of one per group.
    pub fn apply_source_with(&self, batch: &PhysicalBatch, durability: Durability) -> Result<bool> {
        let mut cursor = self.cursor.lock();
        self.check_accounted(&cursor)?;
        if batch.history != cursor.history {
            return Err(PhysicalError::HistoryMismatch);
        }
        if batch.last == cursor.source_sequence
            && Some(batch.first) == cursor.last_first_sequence
            && Some(batch.digest) == cursor.last_digest
        {
            return Ok(false);
        }
        if batch.first != cursor.source_sequence + 1 {
            return Err(PhysicalError::Gap {
                expected: cursor.source_sequence + 1,
                observed: batch.first,
            });
        }
        let mut next = cursor.clone();
        // The receiver cursor consumes one extra local sequence per batch.
        let local_operations = batch.last - batch.first + 2;
        check_local_advance(next.local_sequence, local_operations)?;
        next.local_sequence += local_operations;
        next.source_sequence = batch.last;
        next.last_first_sequence = Some(batch.first);
        next.last_digest = Some(batch.digest);
        let bytes = encode_cursor(&next)?;
        let mut imported = WriteBatch::from_data(&batch.raw);
        imported.put_cf(local_family(&self.db)?, CURSOR_KEY, bytes);
        self.commit(&mut cursor, next, imported, durability)?;
        Ok(true)
    }

    /// Persist node-local state: `batch`'s operations and the advanced cursor
    /// in one fsynced write to [`LOCAL_CF`]. The cursor's local sequence moves
    /// by `batch.len() + 1`; its source position does not move. An empty batch
    /// writes nothing; a batch addressing [`CURSOR_KEY`] is refused (only this
    /// writer writes the cursor) without poisoning.
    pub fn write_local(&self, batch: LocalBatch) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        if batch.ops.len() > MAX_LOCAL_BATCH_OPERATIONS {
            return Err(invalid("local batch operation limit"));
        }
        if batch.ops.iter().any(|op| match op {
            LocalOp::Put(key, _) | LocalOp::Delete(key) => key.as_slice() == CURSOR_KEY,
        }) {
            return Err(invalid("a local batch cannot address the receiver cursor"));
        }
        let mut cursor = self.cursor.lock();
        self.check_accounted(&cursor)?;
        let mut next = cursor.clone();
        let local_operations = batch.ops.len() as u64 + 1;
        check_local_advance(next.local_sequence, local_operations)?;
        next.local_sequence += local_operations;
        let bytes = encode_cursor(&next)?;
        let local = local_family(&self.db)?;
        let mut write = WriteBatch::default();
        for op in batch.ops {
            match op {
                LocalOp::Put(key, value) => write.put_cf(local, key, value),
                LocalOp::Delete(key) => write.delete_cf(local, key),
            }
        }
        write.put_cf(local, CURSOR_KEY, bytes);
        self.commit(&mut cursor, next, write, Durability::Synced)
    }

    /// Prove the durable cursor is the one in memory, then close the handle.
    /// Refuses while anything else still holds the handle: a caller that
    /// renames or reopens the directory next relies on it being closed.
    /// Deferred writes are fsynced first: a closed (or installed) store is
    /// durable at its cursor.
    fn close(self) -> Result<PhysicalCursor> {
        self.sync()?;
        let cursor = self.cursor.into_inner();
        let durable = read_cursor(&self.db)?;
        if durable != cursor {
            return Err(invalid("durable receiver cursor changed unexpectedly"));
        }
        let db = Arc::try_unwrap(self.db)
            .map_err(|_| invalid("the receiver's RocksDB handle is still shared; it cannot be closed"))?;
        drop(db);
        Ok(cursor)
    }
}

/// Owns a staging receiver through its [`LocalWriter`]. Raw durability does
/// not update Nano's derived state, so the writer may be shared with a serving
/// engine only through `StorageEngine::open_physical_standby`, never attached
/// to an ordinary Nano handle.
pub struct StagedPhysicalReplica {
    writer: Arc<LocalWriter>,
}

impl StagedPhysicalReplica {
    /// Initialize only a validated, independently copied physical checkpoint.
    /// The caller proves manifest/history integrity and holds the DB lock.
    pub fn from_checkpoint(db: DB, history: Uuid, checkpoint_sequence: u64) -> Result<Self> {
        Self::from_checkpoint_at(db, history, checkpoint_sequence, checkpoint_sequence)
    }

    /// [`LocalWriter::from_checkpoint_at`]: a cascaded checkpoint, whose store
    /// sequence runs ahead of its source sequence.
    pub fn from_checkpoint_at(db: DB, history: Uuid, source_sequence: u64, store_sequence: u64) -> Result<Self> {
        Ok(Self {
            writer: Arc::new(LocalWriter::from_checkpoint_at(
                db,
                history,
                source_sequence,
                store_sequence,
            )?),
        })
    }

    /// Reopen raw staging without invoking Nano logical recovery/maintenance.
    pub fn resume(db: DB, history: Uuid) -> Result<Self> {
        Ok(Self {
            writer: Arc::new(LocalWriter::resume(db, history)?),
        })
    }

    pub fn writer(&self) -> &Arc<LocalWriter> {
        &self.writer
    }

    pub fn cursor(&self) -> PhysicalCursor {
        self.writer.cursor()
    }

    /// [`LocalWriter::apply_source`].
    pub fn apply(&self, batch: &PhysicalBatch) -> Result<bool> {
        self.writer.apply_source(batch)
    }

    /// [`LocalWriter::apply_source_with`].
    pub fn apply_with(&self, batch: &PhysicalBatch, durability: Durability) -> Result<bool> {
        self.writer.apply_source_with(batch, durability)
    }

    /// Close staging before installation. Serving requires its own validated
    /// physical-replica open path, not ordinary logical-recovery startup. The
    /// writer must no longer be shared.
    pub fn close(self) -> Result<PhysicalCursor> {
        Arc::try_unwrap(self.writer)
            .map_err(|_| invalid("the physical receiver's writer is still shared; it cannot be closed"))?
            .close()
    }
}

// ---------------------------------------------------------------------------
// Incremental fast-forward (spec P4.1 §3.3, §4).
// ---------------------------------------------------------------------------

/// The writer of an incremental fast-forward from the primary: range
/// deletions and ingested SST files into the default column family, the
/// [`FastForwardRecord`], and the re-cut cursor.
///
/// It exists beside [`LocalWriter`] because the two account differently.
/// `LocalWriter` predicts the sequence every write consumes and poisons on
/// any other outcome; an ingested file consumes zero or one sequence numbers
/// depending on what it overlaps, so a fast-forward cannot predict its cost.
/// This writer instead keeps the store refused for every other open while it
/// works — the record goes in before the first default-family write — and
/// DERIVES the cursor at the end from the store's observed sequence, in the
/// one write that also deletes the record ([`Self::recut`]). After that
/// write `latest_sequence_number == cursor.local_sequence` holds again, and
/// a `LocalWriter` can adopt the store.
pub struct FastForwardWriter {
    db: DB,
    history: Uuid,
    /// The cursor as opened, then as re-cut.
    cursor: Mutex<PhysicalCursor>,
}

impl FastForwardWriter {
    /// Open the receiver at `path` exclusively, with the storage options of
    /// `config` (the receiver's options: no archived WAL). The store must
    /// hold [`LOCAL_CF`] and a cursor of `history`; the cursor is read
    /// WITHOUT the sequence equality (an interrupted fast-forward broke it)
    /// and without the record check — this is the writer that finishes one.
    pub fn open(path: &Path, config: &Config, history: Uuid) -> Result<Self> {
        check_history(history)?;
        if !DB::list_cf(&Options::default(), path)?
            .iter()
            .any(|family| family == LOCAL_CF)
        {
            return Err(PhysicalError::FullResyncRequired(
                "the directory has no node-local column family: it predates the node-local namespace or was never \
                 initialized; rebuild it",
            ));
        }
        let (options, families) = super::checkpoint_transfer::receiver_options(config, path)
            .map_err(|error| invalid(format!("the receiver's storage options: {error}")))?;
        let db = DB::open_cf_descriptors(&options, path, families)?;
        refuse_legacy_cursor(&db)?;
        let local = local_family(&db)?;
        let bytes = db
            .get_cf(local, CURSOR_KEY)?
            .ok_or_else(|| invalid("missing physical receiver cursor"))?;
        let cursor = decode_cursor(&bytes)?;
        if cursor.history != history {
            return Err(PhysicalError::HistoryMismatch);
        }
        Ok(Self {
            db,
            history,
            cursor: Mutex::new(cursor),
        })
    }

    pub fn history(&self) -> Uuid {
        self.history
    }

    /// The cursor the store was opened at (its source sequence is where the
    /// fast-forward starts), or the re-cut one after [`Self::recut`].
    pub fn cursor(&self) -> PhysicalCursor {
        self.cursor.lock().clone()
    }

    /// The handle, for the local digest pass. Reads only: every write goes
    /// through this writer's methods.
    pub fn db(&self) -> &DB {
        &self.db
    }

    fn default_family(&self) -> Result<&ColumnFamily> {
        self.db
            .cf_handle(rocksdb::DEFAULT_COLUMN_FAMILY_NAME)
            .ok_or_else(|| invalid("the receiver was opened without its default column family handle"))
    }

    /// Whether the store's sequence is the one the cursor accounts for. It is
    /// not when a fast-forward was interrupted, or when something wrote the
    /// store outside its writers: either way the cursor no longer describes
    /// the default column family, and only a digest pass can tell what does.
    pub fn accounted(&self) -> bool {
        self.db.latest_sequence_number() == self.cursor.lock().local_sequence
    }

    /// The record of the fast-forward in progress, if one is.
    pub fn record(&self) -> Result<Option<FastForwardRecord>> {
        fast_forward_record(&self.db)
    }

    /// Write the record of a NEW cut, synced, before any default-family write
    /// of it: in the same write group, every leaf an earlier cut marked done
    /// is forgotten (it was replaced from another snapshot).
    pub fn start_record(&self, record: &FastForwardRecord) -> Result<()> {
        if record.history != self.history {
            return Err(PhysicalError::HistoryMismatch);
        }
        let bytes = record.encode()?;
        let local = local_family(&self.db)?;
        let mut batch = WriteBatch::default();
        batch.delete_range_cf(local, FAST_FORWARD_DONE_PREFIX, FAST_FORWARD_DONE_END);
        batch.put_cf(local, FAST_FORWARD_KEY, bytes);
        self.db.write_opt(batch, &sync_options())?;
        Ok(())
    }

    /// Mark `leaves` replaced at `cut`, synced: one key per leaf, so a batch
    /// writes its own leaves only, never the whole set.
    pub fn mark_done(&self, leaves: &[u32], cut: u64) -> Result<()> {
        if leaves.is_empty() {
            return Ok(());
        }
        let local = local_family(&self.db)?;
        let mut batch = WriteBatch::default();
        for &leaf in leaves {
            if leaf as usize >= MAX_FAST_FORWARD_LEAVES {
                return Err(invalid(format!(
                    "leaf {leaf} is past the {MAX_FAST_FORWARD_LEAVES} leaves a fast-forward covers"
                )));
            }
            batch.put_cf(local, done_key(leaf), cut.to_be_bytes());
        }
        self.db.write_opt(batch, &sync_options())?;
        Ok(())
    }

    /// The leaves marked replaced at `cut` (a key of another cut does not
    /// count).
    pub fn done_leaves(&self, cut: u64) -> Result<std::collections::BTreeSet<u32>> {
        let local = local_family(&self.db)?;
        let mut read = ReadOptions::default();
        read.set_total_order_seek(true);
        read.set_iterate_upper_bound(FAST_FORWARD_DONE_END.to_vec());
        let mut done = std::collections::BTreeSet::new();
        let mode = IteratorMode::From(FAST_FORWARD_DONE_PREFIX, Direction::Forward);
        for item in self.db.iterator_cf_opt(local, read, mode) {
            let (key, value) = item?;
            let Some(leaf) = key.strip_prefix(FAST_FORWARD_DONE_PREFIX) else {
                break;
            };
            let leaf: [u8; 4] = leaf
                .try_into()
                .map_err(|_| invalid("a malformed fast-forward leaf mark"))?;
            if value.as_ref() == cut.to_be_bytes().as_slice() {
                done.insert(u32::from_be_bytes(leaf));
            }
        }
        Ok(done)
    }

    /// One synced write group of range deletions `[start, end)` over the
    /// default column family: the leaves of one batch, emptied before their
    /// primary's files are ingested. The cursor's family is untouched.
    pub fn delete_ranges(&self, ranges: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
        if ranges.is_empty() {
            return Ok(());
        }
        let default = self.default_family()?;
        let mut batch = WriteBatch::default();
        for (start, end) in ranges {
            if start >= end {
                return Err(invalid("an empty or inverted leaf range"));
            }
            batch.delete_range_cf(default, start, end);
        }
        self.db.write_opt(batch, &sync_options())?;
        Ok(())
    }

    /// Ingest the primary's leaf files into the default column family. They
    /// are COPIED (never moved or hard-linked), so the stage directory never
    /// shares a file with them and a crash leaves no link behind; a memtable
    /// overlapping them is flushed first, and they take a sequence above the
    /// range deletions that emptied their leaves.
    pub fn ingest(&self, files: &[PathBuf]) -> Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        let mut options = IngestExternalFileOptions::default();
        options.set_move_files(false);
        options.set_snapshot_consistency(true);
        options.set_allow_global_seqno(true);
        options.set_allow_blocking_flush(true);
        let default = self.default_family()?;
        self.db
            .ingest_external_file_cf_opts(default, &options, files.to_vec())?;
        Ok(())
    }

    /// Make everything written so far durable in tables: the WAL fsynced and
    /// both families flushed (step (i) of the re-cut).
    pub fn flush(&self) -> Result<()> {
        self.db.flush_wal(true)?;
        self.db.flush_cf(self.default_family()?)?;
        self.db.flush_cf(local_family(&self.db)?)?;
        Ok(())
    }

    /// Close the store as it is, the record still in it (it stays refused by
    /// every other open). Closing waits for RocksDB's background work, so the
    /// directory holds still afterwards: the re-cut takes its census of a
    /// closed store, then reopens it for the final write.
    pub fn release(self) {
        drop(self.db);
    }

    /// Re-cut the stage at the primary's `sequence` (spec P4.1 §3.3.5 (iv)):
    /// ONE synced write that puts the cursor `{snapshot: sequence, source:
    /// sequence, local: the store's sequence after this very write}` and
    /// deletes the record and every leaf mark. The cursor is derived from the
    /// observed sequence, never predicted beyond this write's own operations,
    /// and checked after it.
    pub fn recut(&self, sequence: u64, snapshot_id: Uuid) -> Result<PhysicalCursor> {
        // The cursor put, the record delete and the marks' range deletion.
        const RECUT_OPERATIONS: u64 = 3;
        if snapshot_id.is_nil() {
            return Err(invalid("a re-cut names the primary's snapshot"));
        }
        let mut current = self.cursor.lock();
        let local = local_family(&self.db)?;
        let latest = self.db.latest_sequence_number();
        check_local_advance(latest, RECUT_OPERATIONS)?;
        let cursor = PhysicalCursor {
            format: FORMAT,
            history: self.history,
            snapshot_sequence: sequence,
            source_sequence: sequence,
            local_sequence: latest + RECUT_OPERATIONS,
            last_first_sequence: None,
            last_digest: None,
        };
        cursor.validate()?;
        let mut batch = WriteBatch::default();
        batch.put_cf(local, CURSOR_KEY, encode_cursor(&cursor)?);
        batch.delete_cf(local, FAST_FORWARD_KEY);
        batch.delete_range_cf(local, FAST_FORWARD_DONE_PREFIX, FAST_FORWARD_DONE_END);
        if batch.len() as u64 != RECUT_OPERATIONS {
            return Err(invalid("the re-cut write does not hold the operations it accounts for"));
        }
        self.db.write_opt(batch, &sync_options())?;
        if self.db.latest_sequence_number() != cursor.local_sequence {
            return Err(invalid(
                "the store's sequence moved by more than the re-cut write: it was written outside the fast-forward",
            ));
        }
        *current = cursor.clone();
        Ok(cursor)
    }

    /// Flush both families — so the WAL that carried the fast-forward's own
    /// records (range deletions and record writes, none of them a cursor
    /// write) becomes obsolete and RocksDB removes it, and a cascading source
    /// built on this store never reads it — then prove the store is the
    /// consistent stage a `LocalWriter` adopts (no record, the cursor equal
    /// to the one in memory, the sequence accounted), and close it.
    pub fn close(self) -> Result<PhysicalCursor> {
        self.db.flush_cf(local_family(&self.db)?)?;
        self.db.flush_cf(self.default_family()?)?;
        self.db.flush_wal(true)?;
        let cursor = self.cursor.into_inner();
        let durable = read_cursor(&self.db)?;
        if durable != cursor {
            return Err(invalid("durable receiver cursor changed unexpectedly"));
        }
        drop(self.db);
        Ok(cursor)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use rocksdb::checkpoint::Checkpoint;
    use tempfile::TempDir;

    fn open(path: &std::path::Path) -> DB {
        let mut options = Options::default();
        options.create_if_missing(true);
        options.set_wal_ttl_seconds(3600);
        DB::open(&options, path).unwrap()
    }

    /// A receiver's store must be opened with every column family it holds.
    fn open_receiver(path: &std::path::Path) -> DB {
        let mut options = Options::default();
        options.set_wal_ttl_seconds(3600);
        DB::open_cf(&options, path, ["default", LOCAL_CF]).unwrap()
    }

    fn encoded(history: Uuid, first: u64, batch: WriteBatch) -> PhysicalBatch {
        let mut bytes = batch.data().to_vec();
        bytes[..8].copy_from_slice(&first.to_le_bytes());
        let digest = Sha256::digest(&bytes).into();
        PhysicalBatch::decode(history, first, bytes, digest).unwrap()
    }

    #[test]
    fn staging_resumes_complete_physical_batches_and_durable_progress() {
        let root = TempDir::new().unwrap();
        let source = open(&root.path().join("source"));
        source.put(b"seed", b"keep").unwrap();
        let copy_path = root.path().join("copy");
        Checkpoint::new(&source).unwrap().create_checkpoint(&copy_path).unwrap();
        let copy = open(&copy_path);
        let cut = copy.latest_sequence_number();
        let history = Uuid::new_v4();
        let receiver = StagedPhysicalReplica::from_checkpoint(copy, history, cut).unwrap();
        let mut batch = WriteBatch::default();
        batch.put(b"a", b"first");
        batch.put(b"b", b"second");
        batch.delete(b"seed");
        source.write(batch).unwrap();
        let polled = poll_source(&source, history, cut, MAX_BATCH_BYTES, &FlushMemo::new()).unwrap();
        assert_eq!(polled.batches.len(), 1);
        assert_eq!(polled.through, cut + 3);
        assert_eq!(polled.through, polled.durable_barrier);
        assert!(receiver.apply(&polled.batches[0]).unwrap());
        assert!(!receiver.apply(&polled.batches[0]).unwrap());
        let db = receiver.writer().db();
        assert_eq!(db.get(b"seed").unwrap(), None);
        assert_eq!(db.get(b"a").unwrap().as_deref(), Some(b"first".as_slice()));
        // The cursor lives in the node-local family, not in the copied keyspace.
        assert!(db.get(LEGACY_CURSOR_KEY).unwrap().is_none());
        assert!(db.get(CURSOR_KEY).unwrap().is_none());
        assert!(db.get_cf(local_family(db).unwrap(), CURSOR_KEY).unwrap().is_some());
        let cursor = receiver.close().unwrap();
        assert_eq!(cursor.source_sequence(), cut + 3);
        let copy = open_receiver(&copy_path);
        // Receiver-local cursor writes make its own sequence differ from source.
        assert!(copy.latest_sequence_number() > cursor.source_sequence());
        assert_eq!(copy.latest_sequence_number(), cursor.local_sequence());
        let resumed = StagedPhysicalReplica::resume(copy, history).unwrap();
        assert_eq!(resumed.cursor(), cursor);
        assert!(!resumed.apply(&polled.batches[0]).unwrap());
        assert_eq!(
            resumed.writer().db().get(b"b").unwrap().as_deref(),
            Some(b"second".as_slice())
        );
    }

    /// A receiver applies the groups of one poll deferred and syncs once: each
    /// deferred write still carries its cursor (the store's sequence equals
    /// the cursor's local sequence after every one), the durable barrier stays
    /// where the last fsync left it until the sync, and then equals the last
    /// group's source sequence.
    #[test]
    fn deferred_groups_keep_the_cursor_invariant_and_one_sync_moves_the_durable_barrier() {
        let root = TempDir::new().unwrap();
        let source = open(&root.path().join("source"));
        source.put(b"seed", b"keep").unwrap();
        let copy_path = root.path().join("copy");
        Checkpoint::new(&source).unwrap().create_checkpoint(&copy_path).unwrap();
        let copy = open(&copy_path);
        let cut = copy.latest_sequence_number();
        let history = Uuid::new_v4();
        let receiver = StagedPhysicalReplica::from_checkpoint(copy, history, cut).unwrap();
        let writer = Arc::clone(receiver.writer());
        assert_eq!(writer.durable_source_sequence(), cut);
        for index in 0..5_u32 {
            let mut batch = WriteBatch::default();
            batch.put(format!("k{index}"), b"v");
            batch.put(format!("i{index}"), b"w");
            source.write(batch).unwrap();
        }
        let polled = poll_source(&source, history, cut, MAX_BATCH_BYTES, &FlushMemo::new()).unwrap();
        assert_eq!(polled.batches.len(), 5);
        assert_eq!(polled.through, polled.durable_barrier);

        for batch in &polled.batches {
            assert!(receiver.apply_with(batch, Durability::Deferred).unwrap());
            let cursor = writer.cursor();
            assert_eq!(cursor.source_sequence(), batch.last_sequence());
            assert_eq!(writer.db().latest_sequence_number(), cursor.local_sequence());
            // The cursor went into the same write group as the data.
            assert_eq!(read_cursor(writer.db()).unwrap(), cursor);
            assert_eq!(writer.durable_source_sequence(), cut, "no fsync yet");
        }
        // An exact retransmission of a deferred group is still recognised.
        assert!(!receiver
            .apply_with(polled.batches.last().unwrap(), Durability::Deferred)
            .unwrap());

        let applied = writer.cursor();
        assert_eq!(writer.sync().unwrap(), polled.through);
        assert_eq!(writer.durable_source_sequence(), polled.through);
        assert_eq!(
            writer.durable_source_sequence(),
            polled.batches.last().unwrap().last_sequence()
        );
        assert_eq!(writer.cursor(), applied, "a sync moves no cursor");
        assert_eq!(writer.db().latest_sequence_number(), applied.local_sequence());
        // Nothing pending: a second sync is a no-op.
        assert_eq!(writer.sync().unwrap(), polled.through);
        assert!(!writer.is_poisoned());

        drop(writer);
        let closed = receiver.close().unwrap();
        assert_eq!(closed, applied);
        let resumed = StagedPhysicalReplica::resume(open_receiver(&copy_path), history).unwrap();
        assert_eq!(resumed.cursor(), applied);
        assert_eq!(resumed.writer().durable_source_sequence(), polled.through);
        assert_eq!(
            resumed.writer().db().get(b"k4").unwrap().as_deref(),
            Some(b"v".as_slice())
        );
    }

    /// `apply_source` keeps its synced behaviour (each write is its own
    /// durable barrier), any synced write covers the deferred ones before it,
    /// and closing a store with a deferred tail syncs it.
    #[test]
    fn synced_writes_are_durable_barriers_and_cover_deferred_ones() {
        let root = TempDir::new().unwrap();
        let history = Uuid::new_v4();
        let receiver = StagedPhysicalReplica::from_checkpoint(open(root.path()), history, 0).unwrap();
        let writer = Arc::clone(receiver.writer());
        let group = |first: u64, key: &str| {
            let mut batch = WriteBatch::default();
            batch.put(key, b"v");
            encoded(history, first, batch)
        };
        assert!(writer.apply_source(&group(1, "a")).unwrap());
        assert_eq!(writer.durable_source_sequence(), 1);
        assert_eq!(writer.db().latest_sequence_number(), writer.cursor().local_sequence());

        assert!(writer.apply_source_with(&group(2, "b"), Durability::Deferred).unwrap());
        assert!(writer.apply_source_with(&group(3, "c"), Durability::Deferred).unwrap());
        assert_eq!(writer.durable_source_sequence(), 1);
        assert!(writer.apply_source(&group(4, "d")).unwrap());
        assert_eq!(
            writer.durable_source_sequence(),
            4,
            "the fsync covers the deferred groups"
        );

        assert!(writer.apply_source_with(&group(5, "e"), Durability::Deferred).unwrap());
        let mut local = LocalBatch::new();
        local.put(b"posture", b"streaming");
        writer.write_local(local).unwrap();
        assert_eq!(writer.durable_source_sequence(), 5, "write_local stays synced");

        assert!(writer.apply_source_with(&group(6, "f"), Durability::Deferred).unwrap());
        assert_eq!(writer.durable_source_sequence(), 5);
        let applied = writer.cursor();
        drop(writer);
        // Closing syncs the deferred tail and proves the stored cursor.
        assert_eq!(receiver.close().unwrap(), applied);
        let resumed = StagedPhysicalReplica::resume(open_receiver(root.path()), history).unwrap();
        assert_eq!(resumed.cursor(), applied);
        assert_eq!(resumed.writer().durable_source_sequence(), 6);
    }

    #[test]
    fn staging_rejects_history_gaps_conflicting_duplicates_without_advancing() {
        let root = TempDir::new().unwrap();
        let history = Uuid::new_v4();
        let receiver = StagedPhysicalReplica::from_checkpoint(open(root.path()), history, 0).unwrap();
        let mut original = WriteBatch::default();
        original.put(b"key", b"original");
        let original = encoded(history, 1, original);
        receiver.apply(&original).unwrap();
        let cursor = receiver.cursor();
        for (batch_history, first) in [(Uuid::new_v4(), 2), (history, 3), (history, 1)] {
            let mut batch = WriteBatch::default();
            batch.put(b"key", b"bad");
            assert!(receiver.apply(&encoded(batch_history, first, batch)).is_err());
            assert_eq!(receiver.cursor(), cursor);
            assert_eq!(read_cursor(receiver.writer().db()).unwrap(), cursor);
            assert_eq!(
                receiver.writer().db().get(b"key").unwrap().as_deref(),
                Some(b"original".as_slice())
            );
        }
    }

    #[test]
    fn raw_parser_rejects_malformed_and_reserved_mutations_before_ffi() {
        let mut batch = WriteBatch::default();
        batch.put(b"a", b"b");
        let valid = batch.data().to_vec();
        assert_eq!(validate_raw_batch(&valid).unwrap(), 1);
        for length in 0..valid.len() {
            assert!(validate_raw_batch(&valid[..length]).is_err(), "prefix {length}");
        }
        let mut wrong_count = valid.clone();
        wrong_count[8..12].copy_from_slice(&2_u32.to_le_bytes());
        assert!(validate_raw_batch(&wrong_count).is_err());
        let mut unknown = valid.clone();
        unknown[12] = 255;
        assert!(validate_raw_batch(&unknown).is_err());
        let mut overflow_length = valid[..13].to_vec();
        overflow_length.extend_from_slice(&[255, 255, 255, 255, 31]);
        assert!(validate_raw_batch(&overflow_length).is_err());
        let mut reserved = WriteBatch::default();
        reserved.put(LEGACY_CURSOR_KEY, b"forged");
        assert!(validate_raw_batch(reserved.data()).is_err());
        let mut inverted = WriteBatch::default();
        inverted.delete_range(&b"zz"[..], &b"a"[..]);
        assert!(validate_raw_batch(inverted.data()).is_err());
        // The cursor is no longer in the default family: a default-family key
        // that shares its name, or a range deletion spanning the old location,
        // is ordinary source data.
        let mut same_name = WriteBatch::default();
        same_name.put(CURSOR_KEY, b"user data");
        assert_eq!(validate_raw_batch(same_name.data()).unwrap(), 1);
        let mut range = WriteBatch::default();
        range.delete_range(&b""[..], &b"zz"[..]);
        assert_eq!(validate_raw_batch(range.data()).unwrap(), 1);
        let mut unsupported = WriteBatch::default();
        unsupported.merge(b"a", b"b");
        assert!(validate_raw_batch(unsupported.data()).is_err());
    }

    #[test]
    fn a_receivers_own_wal_records_are_never_valid_source_batches() {
        let root = TempDir::new().unwrap();
        let history = Uuid::new_v4();
        let receiver = StagedPhysicalReplica::from_checkpoint(open(root.path()), history, 0).unwrap();
        let mut batch = WriteBatch::default();
        batch.put(b"key", b"value");
        receiver.apply(&encoded(history, 1, batch)).unwrap();
        let mut seen = 0;
        for record in receiver.writer().db().get_updates_since(0).unwrap() {
            let (_, record) = record.unwrap();
            // Every record this receiver wrote carries a node-local put_cf.
            assert!(validate_raw_batch(record.data()).is_err());
            seen += 1;
        }
        assert!(
            seen >= 2,
            "the initial cursor and the applied batch are both WAL records"
        );
    }

    #[test]
    fn envelopes_reject_corruption_nil_history_wrong_sequence_and_overflow() {
        let history = Uuid::new_v4();
        let mut batch = WriteBatch::default();
        batch.put(b"a", b"b");
        batch.put(b"c", b"d");
        let good = encoded(history, 1, batch);
        assert!(PhysicalBatch::decode(Uuid::nil(), 1, good.raw.clone(), good.digest).is_err());
        assert!(PhysicalBatch::decode(history, 2, good.raw.clone(), good.digest).is_err());
        assert!(PhysicalBatch::decode(history, 1, good.raw.clone(), [0; 32]).is_err());
        let mut overflow = good.raw.clone();
        overflow[..8].copy_from_slice(&MAX_SEQUENCE.to_le_bytes());
        let digest = Sha256::digest(&overflow).into();
        assert!(PhysicalBatch::decode(history, MAX_SEQUENCE, overflow, digest).is_err());
    }

    #[test]
    fn range_deletions_retain_native_semantics_and_progress() {
        let root = TempDir::new().unwrap();
        let db = open(root.path());
        db.put(b"ra", b"keep").unwrap();
        db.put(b"rb", b"remove").unwrap();
        db.put(b"rc", b"remove").unwrap();
        db.put(b"rd", b"keep").unwrap();
        let cut = db.latest_sequence_number();
        let history = Uuid::new_v4();
        let receiver = StagedPhysicalReplica::from_checkpoint(db, history, cut).unwrap();
        let mut batch = WriteBatch::default();
        batch.delete_range(b"rb", b"rd");
        receiver.apply(&encoded(history, cut + 1, batch)).unwrap();
        let db = receiver.writer().db();
        assert!(db.get(b"ra").unwrap().is_some());
        assert!(db.get(b"rb").unwrap().is_none());
        assert!(db.get(b"rc").unwrap().is_none());
        assert!(db.get(b"rd").unwrap().is_some());
        assert_eq!(receiver.cursor().source_sequence(), cut + 1);
        // A default-family range deletion over everything cannot reach the cursor.
        let mut everything = WriteBatch::default();
        everything.delete_range(&b""[..], &[0xff_u8; 64][..]);
        receiver.apply(&encoded(history, cut + 2, everything)).unwrap();
        assert_eq!(read_cursor(receiver.writer().db()).unwrap(), receiver.cursor());
    }

    /// Spec P2.1: a receiver whose next sequence the source no longer retains
    /// is told the WAL was pruned (and from where it still has it), never
    /// handed an ordinary gap it could only retry.
    #[test]
    fn source_poll_reports_purged_wal_as_pruned_with_its_oldest_sequence() {
        let root = TempDir::new().unwrap();
        {
            let source = open(root.path());
            for index in 0..3 {
                source.put(format!("old{index}"), b"v").unwrap();
            }
            // Into tables: the WAL holding 1..=3 is obsolete (and archived,
            // the TTL being set) once the flush completes.
            source.flush().unwrap();
        }
        // What retention does to that WAL, while the store is closed: every
        // archived file and every log but the newest goes.
        let archive = root.path().join("archive");
        if archive.is_dir() {
            for entry in std::fs::read_dir(&archive).unwrap() {
                std::fs::remove_file(entry.unwrap().path()).unwrap();
            }
        }
        let mut logs: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "log"))
            .collect();
        logs.sort();
        logs.pop();
        for log in logs {
            std::fs::remove_file(log).unwrap();
        }
        let source = open(root.path());
        assert_eq!(
            source.latest_sequence_number(),
            3,
            "the purged writes stay in the tables"
        );
        source.put(b"new", b"v").unwrap();
        let history = Uuid::new_v4();
        assert!(matches!(
            poll_source(&source, history, 0, MAX_BATCH_BYTES, &FlushMemo::new()),
            Err(PhysicalError::Pruned {
                wanted: 1,
                oldest: Some(4)
            })
        ));
        // A receiver at 3 needs only what is still retained.
        assert_eq!(
            poll_source(&source, history, 3, MAX_BATCH_BYTES, &FlushMemo::new())
                .unwrap()
                .through,
            4
        );
    }

    #[test]
    fn source_poll_is_bounded_and_rejects_mid_batch_or_future_cursor() {
        let root = TempDir::new().unwrap();
        let source = open(root.path());
        for index in 0..3 {
            let mut batch = WriteBatch::default();
            batch.put(format!("a{index}"), b"one");
            batch.put(format!("b{index}"), b"two");
            source.write(batch).unwrap();
        }
        let history = Uuid::new_v4();
        let first = poll_source(&source, history, 0, 30, &FlushMemo::new()).unwrap();
        assert_eq!(first.batches.len(), 1);
        assert_eq!(first.through, 2);
        assert_eq!(first.durable_barrier, 6);
        let remaining = poll_source(&source, history, first.through, MAX_BATCH_BYTES, &FlushMemo::new()).unwrap();
        assert_eq!(remaining.batches.len(), 2);
        assert_eq!(remaining.through, 6);
        assert!(matches!(
            poll_source(&source, history, 1, MAX_BATCH_BYTES, &FlushMemo::new()),
            Err(PhysicalError::Gap { .. })
        ));
        assert!(matches!(
            poll_source(&source, history, 7, MAX_BATCH_BYTES, &FlushMemo::new()),
            Err(PhysicalError::Ahead { after: 7, barrier: 6 })
        ));
        let caught_up = poll_source(&source, history, 6, MAX_BATCH_BYTES, &FlushMemo::new()).unwrap();
        assert!(caught_up.batches.is_empty());
        assert_eq!(caught_up.through, 6);
        // Spec P4.1c: a limit below the first group's size still serves it,
        // alone; a limit of 0 or past MAX_BATCH_BYTES is refused.
        let one = poll_source(&source, history, 0, 1, &FlushMemo::new()).unwrap();
        assert_eq!(one.batches.len(), 1);
        assert_eq!(one.through, 2);
        assert!(poll_source(&source, history, 0, 0, &FlushMemo::new()).is_err());
        assert!(poll_source(&source, history, 0, MAX_BATCH_BYTES + 1, &FlushMemo::new()).is_err());
    }

    /// A `DB::write` of an empty batch is logged as a WAL record with no
    /// operations that consumes no sequence number. The source must read past
    /// it, the receiver never sees it, and an empty raw batch is refused by
    /// name rather than as an oversized one.
    #[test]
    fn empty_write_groups_are_read_past_and_never_shipped() {
        let root = TempDir::new().unwrap();
        let source = open(&root.path().join("source"));
        source.put(b"seed", b"keep").unwrap();
        let copy_path = root.path().join("copy");
        Checkpoint::new(&source).unwrap().create_checkpoint(&copy_path).unwrap();
        let copy = open(&copy_path);
        let cut = copy.latest_sequence_number();
        let history = Uuid::new_v4();
        let receiver = StagedPhysicalReplica::from_checkpoint(copy, history, cut).unwrap();

        // Empty write groups before, between and after two real ones.
        source.write(WriteBatch::default()).unwrap();
        let mut batch = WriteBatch::default();
        batch.put(b"a", b"1");
        batch.put(b"b", b"2");
        source.write(batch).unwrap();
        source.write(WriteBatch::default()).unwrap();
        source.put(b"c", b"3").unwrap();
        source.write(WriteBatch::default()).unwrap();
        assert_eq!(
            source.latest_sequence_number(),
            cut + 3,
            "an empty write group consumes no sequence number"
        );
        // The WAL really holds them, so this exercises the skip.
        let empty_records = source
            .get_updates_since(cut)
            .unwrap()
            .filter(|record| record.as_ref().unwrap().1.is_empty())
            .count();
        assert!(empty_records >= 2, "RocksDB logged {empty_records} empty write groups");

        let polled = poll_source(&source, history, cut, MAX_BATCH_BYTES, &FlushMemo::new()).unwrap();
        assert_eq!(polled.through, cut + 3);
        assert_eq!(polled.durable_barrier, cut + 3);
        assert_eq!(
            polled
                .batches
                .iter()
                .map(|batch| (batch.first_sequence(), batch.last_sequence()))
                .collect::<Vec<_>>(),
            vec![(cut + 1, cut + 2), (cut + 3, cut + 3)]
        );
        for batch in &polled.batches {
            assert!(receiver.apply(batch).unwrap());
        }
        assert_eq!(receiver.cursor().source_sequence(), cut + 3);
        assert_eq!(
            receiver.writer().db().get(b"c").unwrap().as_deref(),
            Some(b"3".as_slice())
        );
        let caught_up = poll_source(&source, history, cut + 3, MAX_BATCH_BYTES, &FlushMemo::new()).unwrap();
        assert!(caught_up.batches.is_empty());

        let error = validate_raw_batch(WriteBatch::default().data())
            .unwrap_err()
            .to_string();
        assert!(error.contains("empty batch"), "{error}");
        let digest = Sha256::digest(WriteBatch::default().data()).into();
        assert!(PhysicalBatch::decode(history, 1, WriteBatch::default().data().to_vec(), digest).is_err());
    }

    #[test]
    fn receiver_cannot_become_source_and_checkpoint_cannot_overwrite_progress() {
        let root = TempDir::new().unwrap();
        let history = Uuid::new_v4();
        let receiver = StagedPhysicalReplica::from_checkpoint(open(root.path()), history, 0).unwrap();
        assert!(poll_source(receiver.writer().db(), history, 0, MAX_BATCH_BYTES, &FlushMemo::new()).is_err());
        receiver.close().unwrap();
        // A read-only handle may open only the default family; the MANIFEST
        // still names the node-local one.
        let partial = DB::open_for_read_only(&Options::default(), root.path(), false).unwrap();
        assert!(partial.cf_handle(LOCAL_CF).is_none());
        assert!(poll_source(&partial, history, 0, MAX_BATCH_BYTES, &FlushMemo::new()).is_err());
        assert!(StagedPhysicalReplica::resume(partial, history).is_err());
        let db = open_receiver(root.path());
        let local = db.latest_sequence_number();
        assert!(StagedPhysicalReplica::from_checkpoint(db, history, local).is_err());
        let db = open_receiver(root.path());
        assert!(StagedPhysicalReplica::resume(db, Uuid::new_v4()).is_err());
    }

    #[test]
    fn cursor_corruption_is_never_adopted() {
        let root = TempDir::new().unwrap();
        let mut db = open(root.path());
        db.create_cf(LOCAL_CF, &Options::default()).unwrap();
        db.put_cf(local_family(&db).unwrap(), CURSOR_KEY, b"{}").unwrap();
        assert!(StagedPhysicalReplica::resume(db, Uuid::new_v4()).is_err());
    }

    #[test]
    fn a_cursor_in_the_default_family_requires_a_full_resync() {
        let root = TempDir::new().unwrap();
        let db = open(root.path());
        db.put(LEGACY_CURSOR_KEY, b"{}").unwrap();
        assert!(matches!(
            StagedPhysicalReplica::resume(db, Uuid::new_v4()),
            Err(PhysicalError::FullResyncRequired(_))
        ));
        let other = TempDir::new().unwrap();
        let never_initialized = open(other.path());
        assert!(matches!(
            StagedPhysicalReplica::resume(never_initialized, Uuid::new_v4()),
            Err(PhysicalError::FullResyncRequired(_))
        ));
    }

    #[test]
    fn receiver_local_sequence_includes_cursor_and_cannot_overflow() {
        assert!(check_local_advance(MAX_SEQUENCE - 2, 2).is_ok());
        assert!(check_local_advance(MAX_SEQUENCE - 1, 2).is_err());
        assert!(check_local_advance(MAX_SEQUENCE, 1).is_err());
        assert!(check_local_advance(u64::MAX, 1).is_err());
        assert!(check_local_advance(0, 0).is_err());
    }

    #[test]
    fn unrelated_local_writes_invalidate_resume_and_live_receiver() {
        let root = TempDir::new().unwrap();
        let history = Uuid::new_v4();
        let receiver = StagedPhysicalReplica::from_checkpoint(open(root.path()), history, 0).unwrap();
        receiver.close().unwrap();
        let db = open_receiver(root.path());
        db.put(b"local", b"unrelated").unwrap();
        assert!(StagedPhysicalReplica::resume(db, history).is_err());

        let other = TempDir::new().unwrap();
        let receiver = StagedPhysicalReplica::from_checkpoint(open(other.path()), history, 0).unwrap();
        receiver.writer().db().put(b"local", b"unrelated").unwrap();
        let mut batch = WriteBatch::default();
        batch.put(b"key", b"not applied");
        assert!(receiver.apply(&encoded(history, 1, batch)).is_err());
        assert!(receiver.writer().db().get(b"key").unwrap().is_none());
        assert!(matches!(receiver.close(), Err(PhysicalError::Poisoned)));
    }

    #[test]
    fn local_writes_advance_only_the_local_sequence_and_never_the_copied_keyspace() {
        let root = TempDir::new().unwrap();
        let history = Uuid::new_v4();
        let receiver = StagedPhysicalReplica::from_checkpoint(open(root.path()), history, 0).unwrap();
        let writer = Arc::clone(receiver.writer());
        let default_digest = || range_digest(writer.db(), None, b"").unwrap();
        let before = default_digest();
        let start = writer.cursor();
        let mut batch = LocalBatch::new();
        batch.put(b"posture", b"streaming");
        batch.delete(b"absent");
        writer.write_local(batch).unwrap();
        let after = writer.cursor();
        assert_eq!(after.local_sequence(), start.local_sequence() + 3);
        assert_eq!(writer.db().latest_sequence_number(), after.local_sequence());
        assert_eq!(after.source_sequence(), start.source_sequence());
        assert_eq!(default_digest(), before);
        let local = local_family(writer.db()).unwrap();
        assert_eq!(
            writer.db().get_cf(local, b"posture").unwrap().as_deref(),
            Some(b"streaming".as_slice())
        );
        // Nothing to write writes nothing; only the writer writes the cursor.
        writer.write_local(LocalBatch::new()).unwrap();
        let mut forged = LocalBatch::new();
        forged.put(CURSOR_KEY, b"{}");
        assert!(writer.write_local(forged).is_err());
        let mut forged = LocalBatch::new();
        forged.delete(CURSOR_KEY);
        assert!(writer.write_local(forged).is_err());
        assert!(!writer.is_poisoned());
        assert_eq!(writer.cursor(), after);
        assert_eq!(writer.db().latest_sequence_number(), after.local_sequence());
        // Closing needs the only reference to the writer.
        drop(writer);
        assert_eq!(receiver.close().unwrap(), after);
        let resumed = StagedPhysicalReplica::resume(open_receiver(root.path()), history).unwrap();
        assert_eq!(resumed.cursor(), after);
    }

    #[test]
    fn a_shared_writer_refuses_to_close() {
        let root = TempDir::new().unwrap();
        let receiver = StagedPhysicalReplica::from_checkpoint(open(root.path()), Uuid::new_v4(), 0).unwrap();
        let held = Arc::clone(receiver.writer());
        assert!(receiver.close().is_err());
        // The other holder still owns a working writer.
        let mut batch = LocalBatch::new();
        batch.put(b"after", b"close refused");
        held.write_local(batch).unwrap();
    }

    #[test]
    fn range_digest_frames_sorted_pairs_by_length_and_selects_family_and_prefix() {
        let root = TempDir::new().unwrap();
        let mut db = open(root.path());
        db.create_cf(LOCAL_CF, &Options::default()).unwrap();
        // Inserted out of order; the digest follows key order.
        db.put(b"pb", b"2").unwrap();
        db.put(b"pa", b"1").unwrap();
        db.put(b"q", b"3").unwrap();
        db.put_cf(local_family(&db).unwrap(), b"pa", b"local").unwrap();
        let mut expected = Sha256::new();
        for (key, value) in [(&b"pa"[..], &b"1"[..]), (&b"pb"[..], &b"2"[..])] {
            expected.update((key.len() as u64).to_le_bytes());
            expected.update(key);
            expected.update((value.len() as u64).to_le_bytes());
            expected.update(value);
        }
        let expected: [u8; 32] = expected.finalize().into();
        assert_eq!(range_digest(&db, None, b"p").unwrap(), expected);
        assert_ne!(range_digest(&db, None, b"").unwrap(), expected);
        assert_ne!(range_digest(&db, Some(LOCAL_CF), b"p").unwrap(), expected);
        assert_eq!(
            range_digest(&db, None, b"none").unwrap(),
            <[u8; 32]>::from(Sha256::digest(b""))
        );
        assert!(range_digest(&db, Some("missing"), b"").is_err());
    }

    /// A receiver built from a checkpoint of `source` at its current sequence.
    fn receiver_of(source: &DB, path: &std::path::Path, history: Uuid) -> (StagedPhysicalReplica, u64) {
        Checkpoint::new(source).unwrap().create_checkpoint(path).unwrap();
        let copy = open(path);
        let cut = copy.latest_sequence_number();
        (StagedPhysicalReplica::from_checkpoint(copy, history, cut).unwrap(), cut)
    }

    /// Spec P3.2 §4, §5: a standby serving its store strips its node-local
    /// records and re-frames each group with the primary's sequences, so a
    /// cascaded group is byte for byte the primary's (digest included) for
    /// the same span, and a downstream receiver fed only cascaded groups holds
    /// the primary's default column family.
    #[test]
    fn cascade_stripping_yields_the_primarys_groups_and_digest_for_the_same_span() {
        let root = TempDir::new().unwrap();
        let source = open(&root.path().join("source"));
        source.put(b"seed", b"keep").unwrap();
        source.put(b"range-a", b"1").unwrap();
        source.put(b"range-b", b"2").unwrap();
        let history = Uuid::new_v4();
        let (upstream, cut) = receiver_of(&source, &root.path().join("upstream"), history);
        let (downstream, downstream_cut) = receiver_of(&source, &root.path().join("downstream"), history);
        assert_eq!(cut, downstream_cut);
        // Groups of every shape the walker keeps.
        let mut batch = WriteBatch::default();
        batch.put(b"a", b"1");
        batch.put(b"b", b"2");
        source.write(batch).unwrap();
        let mut batch = WriteBatch::default();
        batch.delete(b"seed");
        batch.delete_range(&b"range-"[..], &b"range."[..]);
        source.write(batch).unwrap();
        source.put(b"c", b"3").unwrap();
        let polled = poll_source(&source, history, cut, MAX_BATCH_BYTES, &FlushMemo::new()).unwrap();
        assert_eq!(polled.batches.len(), 3);
        let writer = Arc::clone(upstream.writer());
        for (index, batch) in polled.batches.iter().enumerate() {
            assert!(upstream.apply(batch).unwrap());
            if index == 0 {
                // Node-local state between two groups: never shipped.
                let mut local = LocalBatch::new();
                local.put(b"posture", b"streaming");
                writer.write_local(local).unwrap();
            }
        }
        assert!(
            writer.db().latest_sequence_number() > polled.through,
            "the upstream's own sequence runs ahead of its source sequence"
        );
        // An ordinary source refuses a receiver's store.
        assert!(poll_source(writer.db(), history, cut, MAX_BATCH_BYTES, &FlushMemo::new()).is_err());

        let mut hint = CascadeHint::default();
        let cascaded =
            poll_source_cascade(writer.db(), history, cut, MAX_BATCH_BYTES, &mut hint, &FlushMemo::new()).unwrap();
        assert_eq!(cascaded.through, polled.through);
        assert_eq!(
            cascaded.durable_barrier, polled.through,
            "the barrier is the source cursor"
        );
        assert_eq!(cascaded.batches.len(), polled.batches.len());
        for (ours, theirs) in cascaded.batches.iter().zip(&polled.batches) {
            assert_eq!(ours.first_sequence(), theirs.first_sequence());
            assert_eq!(ours.last_sequence(), theirs.last_sequence());
            assert_eq!(ours.digest(), theirs.digest(), "the primary's digest for the same span");
            assert_eq!(ours.data(), theirs.data());
        }
        // A receiver in the middle continues from the hint.
        let middle = polled.batches[0].last_sequence();
        let rest = poll_source_cascade(
            writer.db(),
            history,
            middle,
            MAX_BATCH_BYTES,
            &mut hint,
            &FlushMemo::new(),
        )
        .unwrap();
        assert_eq!(rest.batches.len(), 2);
        assert_eq!(rest.batches[0].first_sequence(), middle + 1);
        // Caught up, ahead, inside a group, another history.
        let caught_up = poll_source_cascade(
            writer.db(),
            history,
            polled.through,
            MAX_BATCH_BYTES,
            &mut hint,
            &FlushMemo::new(),
        )
        .unwrap();
        assert!(caught_up.batches.is_empty());
        assert!(matches!(
            poll_source_cascade(
                writer.db(),
                history,
                polled.through + 1,
                MAX_BATCH_BYTES,
                &mut hint,
                &FlushMemo::new()
            ),
            Err(PhysicalError::Ahead { .. })
        ));
        assert!(matches!(
            poll_source_cascade(
                writer.db(),
                history,
                cut + 1,
                MAX_BATCH_BYTES,
                &mut CascadeHint::default(),
                &FlushMemo::new()
            ),
            Err(PhysicalError::Gap { .. })
        ));
        assert!(matches!(
            poll_source_cascade(
                writer.db(),
                Uuid::new_v4(),
                cut,
                MAX_BATCH_BYTES,
                &mut hint,
                &FlushMemo::new()
            ),
            Err(PhysicalError::HistoryMismatch)
        ));
        // The barrier helper agrees.
        assert_eq!(receiver_source_barrier(writer.db()).unwrap(), polled.through);

        // A downstream receiver fed only the cascaded groups holds the
        // primary's default column family, and keeps its own accounting.
        for batch in &cascaded.batches {
            assert!(downstream.apply(batch).unwrap());
        }
        assert_eq!(
            range_digest(downstream.writer().db(), None, b"").unwrap(),
            range_digest(&source, None, b"").unwrap()
        );
        let downstream_cursor = downstream.cursor();
        assert_eq!(downstream_cursor.source_sequence(), polled.through);
        assert_eq!(
            downstream.writer().db().latest_sequence_number(),
            downstream_cursor.local_sequence()
        );
    }

    /// Spec P3.2 §4: a cascaded checkpoint's store sequence differs from the
    /// source sequence it holds; its receiver starts at the source sequence
    /// and accounts from the store's. Spec P4.1 §4: a fast-forwarded
    /// cascading source's store runs BEHIND the source sequence it holds,
    /// and its checkpoint is served the same way.
    #[test]
    fn a_receiver_from_a_cascaded_checkpoint_starts_at_the_source_sequence() {
        let root = TempDir::new().unwrap();
        let db = open(root.path());
        for index in 0..5 {
            db.put(format!("k{index}"), b"v").unwrap();
        }
        let store = db.latest_sequence_number();
        let history = Uuid::new_v4();
        // The store must be at the sequence the caller proved.
        let refused = StagedPhysicalReplica::from_checkpoint_at(db, history, 3, store + 1);
        assert!(refused.is_err());
        // A source cut past the store's own counter (a fast-forwarded source).
        let ahead_root = TempDir::new().unwrap();
        let ahead = open(ahead_root.path());
        ahead.put(b"k", b"v").unwrap();
        let ahead_store = ahead.latest_sequence_number();
        let receiver = StagedPhysicalReplica::from_checkpoint_at(ahead, history, 1_000, ahead_store).unwrap();
        assert_eq!(receiver.cursor().source_sequence(), 1_000);
        assert_eq!(receiver.cursor().local_sequence(), ahead_store + 1);
        let mut batch = WriteBatch::default();
        batch.put(b"next", b"group");
        assert!(receiver.apply(&encoded(history, 1_001, batch)).unwrap());
        assert_eq!(
            receiver.writer().db().latest_sequence_number(),
            receiver.cursor().local_sequence()
        );
        drop(receiver);
        let db = open(root.path());
        let receiver = StagedPhysicalReplica::from_checkpoint_at(db, history, 3, store).unwrap();
        let cursor = receiver.cursor();
        assert_eq!(cursor.snapshot_sequence(), 3);
        assert_eq!(cursor.source_sequence(), 3);
        assert_eq!(cursor.local_sequence(), store + 1);
        let mut batch = WriteBatch::default();
        batch.put(b"next", b"group");
        assert!(receiver.apply(&encoded(history, 4, batch)).unwrap());
        assert_eq!(receiver.cursor().source_sequence(), 4);
        assert_eq!(receiver.cursor().local_sequence(), store + 3);
    }

    // --- spec P4.1: the fast-forward record, writer and cursor rule ---------

    /// The record round-trips through its JSON and holds only what the
    /// fast-forward is (no per-leaf state), and an impossible record (no
    /// history, an empty interval) is refused.
    #[test]
    fn a_fast_forward_record_round_trips_and_holds_only_its_cut() {
        let history = Uuid::new_v4();
        let record = FastForwardRecord::new(history, Uuid::new_v4(), 10, 99, 4 * 1024 * 1024);
        let bytes = record.encode().unwrap();
        assert!(bytes.len() < 512, "{} bytes", bytes.len());
        assert!(!String::from_utf8_lossy(&bytes).contains("done"));
        assert_eq!(FastForwardRecord::decode(&bytes).unwrap(), record);
        for broken in [
            FastForwardRecord::new(Uuid::nil(), Uuid::new_v4(), 1, 2, 1),
            FastForwardRecord::new(history, Uuid::nil(), 1, 2, 1),
            FastForwardRecord::new(history, Uuid::new_v4(), 2, 2, 1),
            FastForwardRecord::new(history, Uuid::new_v4(), 1, MAX_SEQUENCE + 1, 1),
        ] {
            assert!(broken.encode().is_err(), "{broken:?}");
        }
        assert!(FastForwardRecord::decode(b"{\"format\":1}").is_err());
        assert!(FastForwardRecord::decode(&vec![b' '; MAX_FAST_FORWARD_RECORD_BYTES + 1]).is_err());
    }

    /// Spec P4.1 §4: the cursor rule is arithmetic, not safety — a
    /// fast-forwarded cursor's local counter may be behind its source
    /// sequence; a local counter of 0 is still impossible.
    #[test]
    fn a_cursor_whose_local_counter_is_behind_its_source_sequence_is_valid() {
        let history = Uuid::new_v4();
        let cursor = PhysicalCursor {
            format: FORMAT,
            history,
            snapshot_sequence: 1_000,
            source_sequence: 1_000,
            local_sequence: 7,
            last_first_sequence: None,
            last_digest: None,
        };
        assert_eq!(decode_cursor(&encode_cursor(&cursor).unwrap()).unwrap(), cursor);
        let zero = PhysicalCursor {
            local_sequence: 0,
            ..cursor.clone()
        };
        assert!(decode_cursor(&encode_cursor(&zero).unwrap()).is_err());
        let past = PhysicalCursor {
            local_sequence: MAX_SEQUENCE + 1,
            ..cursor
        };
        assert!(decode_cursor(&encode_cursor(&past).unwrap()).is_err());
    }

    /// Spec P4.1 §4: a store left with a fast-forward record is refused by
    /// every open that adopts a cursor, BEFORE the sequence equality (which
    /// the fast-forward's writes broke), naming the interval; only the
    /// fast-forward's own writer reopens it.
    #[test]
    fn an_interrupted_fast_forward_is_refused_before_the_sequence_check() {
        let root = TempDir::new().unwrap();
        let source = open(&root.path().join("source"));
        source.put(b"k", b"v").unwrap();
        let history = Uuid::new_v4();
        let copy_path = root.path().join("copy");
        let (receiver, cut) = receiver_of(&source, &copy_path, history);
        receiver.close().unwrap();
        let writer = FastForwardWriter::open(&copy_path, &Config::default(), history).unwrap();
        assert!(writer.record().unwrap().is_none());
        let record = FastForwardRecord::new(history, Uuid::new_v4(), cut, cut + 10, 64 * 1024);
        assert!(writer.accounted());
        writer.start_record(&record).unwrap();
        // A default-family write of the fast-forward: the store's sequence
        // no longer matches the cursor.
        writer.delete_ranges(&[(b"a".to_vec(), b"z".to_vec())]).unwrap();
        assert!(!writer.accounted());
        drop(writer);
        let reopened = [
            StagedPhysicalReplica::resume(open_receiver(&copy_path), history),
            StagedPhysicalReplica::resume(
                DB::open_cf_for_read_only(&Options::default(), &copy_path, ["default", LOCAL_CF], false).unwrap(),
                history,
            ),
        ];
        for resumed in reopened {
            match resumed {
                Err(PhysicalError::FastForwardInterrupted { from, to }) => assert_eq!((from, to), (cut, cut + 10)),
                Err(other) => panic!("not the fast-forward refusal: {other}"),
                Ok(_) => panic!("an interrupted fast-forward was resumed"),
            }
        }
        let text = PhysicalError::FastForwardInterrupted { from: 1, to: 2 }.to_string();
        assert!(
            text.contains("interrupted at sequence 1→2") && text.contains("recover --from-primary"),
            "{text}"
        );
        assert!(fast_forward_remedy(1, 2, "/srv/standby", "db:5440").contains("--data-dir /srv/standby"));
        let writer = FastForwardWriter::open(&copy_path, &Config::default(), history).unwrap();
        assert_eq!(writer.record().unwrap(), Some(record));
        assert_eq!(writer.cursor().source_sequence(), cut, "the cursor is untouched");
        drop(writer);
        assert!(matches!(
            FastForwardWriter::open(&copy_path, &Config::default(), Uuid::new_v4()),
            Err(PhysicalError::HistoryMismatch)
        ));
    }

    /// The SST file of the keys of `source` in `[start, end)`, as a primary
    /// builds a leaf.
    fn leaf_file(source: &DB, path: &std::path::Path, start: &[u8], end: &[u8]) {
        let options = Options::default();
        let mut writer = rocksdb::SstFileWriter::create(&options);
        writer.open(path).unwrap();
        let mut read = ReadOptions::default();
        read.set_total_order_seek(true);
        read.set_iterate_upper_bound(end.to_vec());
        for item in source.iterator_opt(IteratorMode::From(start, Direction::Forward), read) {
            let (key, value) = item.unwrap();
            writer.put(key, value).unwrap();
        }
        writer.finish().unwrap();
    }

    /// Spec P4.1 §3.3: range deletions plus ingested leaf files make the
    /// stale leaf the primary's (updates, inserts and deletes alike), leave
    /// the other leaves alone, and the re-cut derives a cursor that accounts
    /// for the store — so an ordinary writer adopts it and continues in the
    /// primary's numbering.
    #[test]
    fn delete_ranges_and_ingest_reach_the_primarys_family_and_the_recut_accounts_for_the_store() {
        let root = TempDir::new().unwrap();
        let source = open(&root.path().join("source"));
        for index in 0..40_u32 {
            source.put(format!("a{index:03}"), b"old").unwrap();
            source.put(format!("n{index:03}"), b"old").unwrap();
        }
        let history = Uuid::new_v4();
        let copy_path = root.path().join("copy");
        let (receiver, cut) = receiver_of(&source, &copy_path, history);
        receiver.close().unwrap();
        // The primary moves on in the second leaf only.
        source.put(b"n005", b"new").unwrap();
        source.put(b"n500", b"inserted").unwrap();
        source.delete(b"n007").unwrap();
        let target = source.latest_sequence_number();
        let end = [0xff_u8; 64];
        let file = root.path().join("1.sst");
        leaf_file(&source, &file, b"n", &end);

        let snapshot = Uuid::new_v4();
        let writer = FastForwardWriter::open(&copy_path, &Config::default(), history).unwrap();
        assert_eq!(writer.cursor().source_sequence(), cut);
        // An earlier cut's mark is forgotten by the new cut's record.
        writer.mark_done(&[9], target - 1).unwrap();
        let record = FastForwardRecord::new(history, snapshot, cut, target, 64 * 1024);
        writer.start_record(&record).unwrap();
        assert!(writer.done_leaves(target - 1).unwrap().is_empty());
        writer.delete_ranges(&[(b"n".to_vec(), end.to_vec())]).unwrap();
        writer.ingest(std::slice::from_ref(&file)).unwrap();
        writer.mark_done(&[1, 3], target).unwrap();
        assert_eq!(
            writer.done_leaves(target).unwrap().into_iter().collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert!(
            writer.done_leaves(target + 1).unwrap().is_empty(),
            "marks of another cut do not count"
        );
        assert!(writer.mark_done(&[1 << 24], target).is_err());
        assert!(file.exists(), "ingested by copy: the staged file stays where it was");
        assert_eq!(
            range_digest(writer.db(), None, b"").unwrap(),
            range_digest(&source, None, b"").unwrap()
        );
        assert_eq!(writer.record().unwrap(), Some(record));
        writer.flush().unwrap();
        // The re-cut takes its census of a CLOSED store, then reopens it for
        // the final write: the record survives the close.
        writer.release();
        let writer = FastForwardWriter::open(&copy_path, &Config::default(), history).unwrap();
        assert!(writer.record().unwrap().is_some());
        let cursor = writer.recut(target, snapshot).unwrap();
        assert_eq!((cursor.snapshot_sequence(), cursor.source_sequence()), (target, target));
        assert_eq!(writer.db().latest_sequence_number(), cursor.local_sequence());
        assert!(writer.accounted());
        assert!(writer.record().unwrap().is_none(), "the re-cut removed the record");
        assert!(writer.done_leaves(target).unwrap().is_empty(), "and the leaf marks");
        assert_eq!(writer.close().unwrap(), cursor);

        let resumed = StagedPhysicalReplica::resume(open_receiver(&copy_path), history).unwrap();
        assert_eq!(resumed.cursor(), cursor);
        source.put(b"after", b"cut").unwrap();
        let polled = poll_source(&source, history, target, MAX_BATCH_BYTES, &FlushMemo::new()).unwrap();
        for batch in &polled.batches {
            assert!(resumed.apply(batch).unwrap());
        }
        assert_eq!(
            range_digest(resumed.writer().db(), None, b"").unwrap(),
            range_digest(&source, None, b"").unwrap()
        );
        assert_eq!(
            resumed.writer().db().latest_sequence_number(),
            resumed.cursor().local_sequence()
        );
    }

    // --- spec P4.1c: write groups of any size --------------------------------

    const MIB: usize = 1024 * 1024;
    /// A serving standby's poll limit (`STANDBY_POLL_BYTES` in `main.rs`).
    const STANDBY_POLL: usize = 32 * MIB;

    /// One 100 MiB write group (100 puts of 1 MiB values), then two small ones.
    fn write_large_then_small(source: &DB) {
        let mut large = WriteBatch::default();
        for index in 0..100_u32 {
            large.put(
                format!("data:large:{index:03}"),
                vec![u8::try_from(index % 251).unwrap(); MIB],
            );
        }
        source.write(large).unwrap();
        source.put(b"data:small:1", b"one").unwrap();
        source.put(b"data:small:2", b"two").unwrap();
    }

    /// Spec P4.1c §3: the poll byte limit is a coalescing target. A 100 MiB
    /// write group is served whole and alone by a poll of the standby's 32 MiB
    /// limit, `through` at its last sequence; the next poll carries the two
    /// small groups after it together. The group decodes as a transport hands
    /// it over, and a receiver applies it to the source's exact state.
    #[test]
    fn a_write_group_above_the_poll_limit_travels_alone_and_small_ones_coalesce() {
        let root = TempDir::new().unwrap();
        let source = open(&root.path().join("source"));
        source.put(b"seed", b"keep").unwrap();
        let history = Uuid::new_v4();
        let (receiver, cut) = receiver_of(&source, &root.path().join("copy"), history);
        write_large_then_small(&source);

        let large = poll_source(&source, history, cut, STANDBY_POLL, &FlushMemo::new()).unwrap();
        assert_eq!(large.batches.len(), 1, "the large group alone");
        let group = &large.batches[0];
        assert!(group.data().len() > 100 * MIB, "{} bytes", group.data().len());
        assert_eq!((group.first_sequence(), group.last_sequence()), (cut + 1, cut + 100));
        assert_eq!(large.through, cut + 100, "through at the group's last sequence");
        assert_eq!(large.durable_barrier, cut + 102);
        let decoded =
            PhysicalBatch::decode(history, group.first_sequence(), group.data().to_vec(), group.digest()).unwrap();
        assert_eq!(decoded.last_sequence(), cut + 100);
        assert!(receiver.apply(group).unwrap());

        let small = poll_source(&source, history, large.through, STANDBY_POLL, &FlushMemo::new()).unwrap();
        assert_eq!(small.batches.len(), 2, "the two small groups together");
        assert_eq!(small.through, cut + 102);
        assert_eq!(small.through, small.durable_barrier);
        for batch in &small.batches {
            assert!(receiver.apply(batch).unwrap());
        }
        assert_eq!(
            range_digest(receiver.writer().db(), None, b"").unwrap(),
            range_digest(&source, None, b"").unwrap()
        );
        assert_eq!(receiver.cursor().source_sequence(), cut + 102);
    }

    /// Spec P4.1c §3: the same for a cascading source. A receiver that
    /// applied the 100 MiB group and the two small ones serves the large one
    /// alone to a poll of 32 MiB — byte for byte the primary's group, digest
    /// included — and the two small ones together on the next poll.
    #[test]
    fn a_cascading_source_serves_a_write_group_above_the_poll_limit_alone() {
        let root = TempDir::new().unwrap();
        let source = open(&root.path().join("source"));
        source.put(b"seed", b"keep").unwrap();
        let history = Uuid::new_v4();
        let (upstream, cut) = receiver_of(&source, &root.path().join("upstream"), history);
        write_large_then_small(&source);
        let polled = poll_source(&source, history, cut, MAX_BATCH_BYTES, &FlushMemo::new()).unwrap();
        assert_eq!(polled.batches.len(), 3, "a limit the groups fit: one poll");
        for batch in &polled.batches {
            assert!(upstream.apply(batch).unwrap());
        }
        let writer = Arc::clone(upstream.writer());

        let mut hint = CascadeHint::default();
        let large = poll_source_cascade(writer.db(), history, cut, STANDBY_POLL, &mut hint, &FlushMemo::new()).unwrap();
        assert_eq!(large.batches.len(), 1, "the large group alone");
        assert_eq!(large.through, cut + 100);
        assert_eq!(large.durable_barrier, cut + 102);
        assert_eq!(large.batches[0].digest(), polled.batches[0].digest());
        assert_eq!(large.batches[0].data(), polled.batches[0].data());

        let small = poll_source_cascade(
            writer.db(),
            history,
            large.through,
            STANDBY_POLL,
            &mut hint,
            &FlushMemo::new(),
        )
        .unwrap();
        assert_eq!(small.batches.len(), 2, "the two small groups together");
        assert_eq!(small.through, cut + 102);
        for (ours, theirs) in small.batches.iter().zip(&polled.batches[1..]) {
            assert_eq!(ours.digest(), theirs.digest());
            assert_eq!(ours.data(), theirs.data());
        }
    }

    /// Spec P4.1c: a raw batch of 2 000 000 operations — twice the bound
    /// every batch had before — walks with every record visited, and decodes.
    #[test]
    fn a_batch_of_two_million_operations_walks_and_decodes() {
        let mut batch = WriteBatch::default();
        for index in 0..2_000_000_u32 {
            batch.delete(index.to_be_bytes());
        }
        let mut visited = 0_u32;
        assert_eq!(walk_raw_batch(batch.data(), |_| visited += 1).unwrap(), 2_000_000);
        assert_eq!(visited, 2_000_000);
        let decoded = encoded(Uuid::new_v4(), 1, batch);
        assert_eq!(decoded.last_sequence(), 2_000_000);
    }

    // --- spec P4.1d: the WAL is fsynced once per barrier ---------------------

    /// Spec P4.1d §2.3: a poll fsyncs its source's WAL only when the barrier
    /// it sampled is above its memo, the last barrier the memo's polls
    /// fsynced. Two polls at the same barrier fsync once: the first moves the
    /// memo to the barrier, the second finds the barrier covered and leaves
    /// the memo where it was; a write moves the barrier and the next poll
    /// fsyncs again. Read through the memo's value, which only an fsync moves,
    /// and the decision the polls take (`FlushMemo::flush`), for a primary and
    /// for a cascading source alike.
    #[test]
    fn two_polls_at_the_same_barrier_fsync_the_wal_once() {
        let root = TempDir::new().unwrap();
        let source = open(&root.path().join("source"));
        source.put(b"seed", b"keep").unwrap();
        let history = Uuid::new_v4();
        let (upstream, cut) = receiver_of(&source, &root.path().join("upstream"), history);
        source.put(b"a", b"1").unwrap();

        let memo = FlushMemo::new();
        assert_eq!(memo.last_flushed(), 0, "nothing fsynced yet");
        let barrier = source.latest_sequence_number();
        assert!(memo.needs_flush(barrier));
        let first = poll_source(&source, history, cut, MAX_BATCH_BYTES, &memo).unwrap();
        assert_eq!((first.through, first.durable_barrier), (barrier, barrier));
        assert_eq!(memo.last_flushed(), barrier, "the first poll fsynced its barrier");
        assert!(
            !memo.needs_flush(barrier),
            "a second poll at the same barrier has nothing to make durable"
        );
        let second = poll_source(&source, history, first.through, MAX_BATCH_BYTES, &memo).unwrap();
        assert!(second.batches.is_empty());
        assert_eq!(second.durable_barrier, barrier);
        assert_eq!(memo.last_flushed(), barrier, "the second poll did not move the memo");
        // The polls' decision: an fsync for a barrier above the memo only.
        assert!(!memo.flush(&source, barrier).unwrap(), "covered: no fsync");
        assert!(!memo.flush(&source, barrier - 1).unwrap(), "below: no fsync");
        assert_eq!(memo.last_flushed(), barrier);

        source.put(b"b", b"2").unwrap();
        let moved = source.latest_sequence_number();
        assert!(memo.needs_flush(moved));
        let third = poll_source(&source, history, second.through, MAX_BATCH_BYTES, &memo).unwrap();
        assert_eq!((third.through, third.durable_barrier), (moved, moved));
        assert_eq!(memo.last_flushed(), moved, "a moved barrier is fsynced again");
        // A memo of its own fsyncs at its first poll, as every poll did before.
        let fresh = FlushMemo::new();
        assert!(fresh.flush(&source, moved).unwrap());
        assert_eq!(fresh.last_flushed(), moved);

        // A cascading source: the same rule over its cursor's source sequence.
        for batch in &first.batches {
            assert!(upstream.apply_with(batch, Durability::Deferred).unwrap());
        }
        let writer = Arc::clone(upstream.writer());
        let mut hint = CascadeHint::default();
        let cascade = FlushMemo::new();
        let served = poll_source_cascade(writer.db(), history, cut, MAX_BATCH_BYTES, &mut hint, &cascade).unwrap();
        assert_eq!(served.durable_barrier, barrier);
        assert_eq!(
            cascade.last_flushed(),
            barrier,
            "the cascading source fsynced its own WAL"
        );
        assert!(!cascade.needs_flush(barrier));
        let idle = poll_source_cascade(
            writer.db(),
            history,
            served.through,
            MAX_BATCH_BYTES,
            &mut hint,
            &cascade,
        )
        .unwrap();
        assert!(idle.batches.is_empty());
        assert_eq!(
            cascade.last_flushed(),
            barrier,
            "an idle cascaded poll did not move the memo"
        );
    }
}
