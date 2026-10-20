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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use rocksdb::{ColumnFamily, Direction, IteratorMode, Options, ReadOptions, WriteBatch, WriteOptions, DB};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::storage::StorageEngine;

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
pub const MAX_BATCH_BYTES: usize = 64 * 1024 * 1024;
const MAX_BATCH_OPERATIONS: u32 = 1_000_000;
// RocksDB encodes its sequence into the high 56 bits of an internal key.
const MAX_SEQUENCE: u64 = (1_u64 << 56) - 1;

#[derive(Debug, thiserror::Error)]
pub enum PhysicalError {
    #[error("physical replication storage error: {0}")]
    Storage(#[from] rocksdb::Error),
    #[error("invalid physical replication input: {0}")]
    Invalid(String),
    #[error("physical replication interval missing: expected {expected}, observed {observed}")]
    Gap { expected: u64, observed: u64 },
    #[error("physical replication history mismatch")]
    HistoryMismatch,
    #[error("physical receiver must be reopened after an uncertain storage failure")]
    Poisoned,
    /// The directory cannot be continued in place; only a fresh resync can
    /// rebuild it. Nothing was modified.
    #[error("physical replication requires a full resync: {0}")]
    FullResyncRequired(&'static str),
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
    if count > MAX_BATCH_OPERATIONS {
        return Err(invalid("batch operation limit"));
    }
    let mut input = raw.get(12..).ok_or_else(|| invalid("batch header too short"))?;
    let mut observed = 0_u32;
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
        if observed > count {
            return Err(invalid("batch operation count mismatch"));
        }
    }
    if observed != count {
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

fn take_slice<'a>(input: &mut &'a [u8]) -> Result<&'a [u8]> {
    let mut length = 0_u32;
    let mut ended = false;
    for shift in (0..35).step_by(7) {
        let (&byte, rest) = input.split_first().ok_or_else(|| invalid("truncated length"))?;
        *input = rest;
        if shift == 28 && byte > 15 {
            return Err(invalid("length overflow"));
        }
        length |= u32::from(byte & 127) << shift;
        if byte < 128 {
            ended = true;
            break;
        }
    }
    if !ended || length as usize > input.len() {
        return Err(invalid("truncated value"));
    }
    let (value, rest) = input.split_at(length as usize);
    *input = rest;
    Ok(value)
}

/// A bounded contiguous part of the durable source interval. `through` can be
/// below `durable_barrier` when the caller's byte cap is reached. No empty result
/// is returned across a missing interval.
#[derive(Debug)]
pub struct PhysicalPoll {
    pub batches: Vec<PhysicalBatch>,
    pub through: u64,
    pub durable_barrier: u64,
}

/// Refuse to read a receiver's store as a source: its WAL carries its own
/// node-local writes, and cascading is not supported.
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

/// Poll committed physical changes. The owner must ensure all source writes
/// retain the RocksDB WAL; this function detects gaps but cannot manufacture a
/// retention lease. Only use cursors from a completed checkpoint or full batch.
pub fn poll_source(db: &DB, history: Uuid, after: u64, byte_limit: usize) -> Result<PhysicalPoll> {
    check_history(history)?;
    if byte_limit == 0 || byte_limit > MAX_BATCH_BYTES {
        return Err(invalid("invalid poll byte limit"));
    }
    refuse_receiver_source(db)?;
    let barrier = db.latest_sequence_number();
    if after > barrier || barrier > MAX_SEQUENCE {
        return Err(invalid("source is behind receiver or sequence overflow"));
    }
    // Sample the barrier first; fsync subsequently covers every sampled write.
    db.flush_wal(true)?;
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
            return Err(PhysicalError::Gap {
                expected: result.through + 1,
                observed: first,
            });
        }
        if batch.size_in_bytes() > byte_limit {
            return Err(invalid("single source batch exceeds poll byte limit"));
        }
        if bytes + batch.size_in_bytes() > byte_limit {
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

    fn validate(&self) -> Result<()> {
        check_history(self.history)?;
        if self.format != FORMAT
            || self.snapshot_sequence > self.source_sequence
            || self.source_sequence > MAX_SEQUENCE
            || self.local_sequence <= self.source_sequence
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

fn sync_options() -> WriteOptions {
    let mut options = WriteOptions::default();
    options.set_sync(true);
    options
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
pub struct LocalWriter {
    db: Arc<DB>,
    cursor: Mutex<PhysicalCursor>,
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
    pub fn from_checkpoint(mut db: DB, history: Uuid, checkpoint_sequence: u64) -> Result<Self> {
        check_history(history)?;
        if db.latest_sequence_number() != checkpoint_sequence
            || db.cf_handle(LOCAL_CF).is_some()
            || db.get(LEGACY_CURSOR_KEY)?.is_some()
        {
            return Err(invalid("checkpoint sequence mismatch or existing receiver cursor"));
        }
        check_local_advance(checkpoint_sequence, 1)?;
        let cursor = PhysicalCursor {
            format: FORMAT,
            history,
            snapshot_sequence: checkpoint_sequence,
            source_sequence: checkpoint_sequence,
            local_sequence: checkpoint_sequence + 1,
            last_first_sequence: None,
            last_digest: None,
        };
        cursor.validate()?;
        let bytes = encode_cursor(&cursor)?;
        db.create_cf(LOCAL_CF, &StorageEngine::local_cf_options())?;
        // Creating a family is a MANIFEST edit, not a write. Should it ever
        // move the sequence, the cut no longer matches the checkpoint.
        if db.latest_sequence_number() != checkpoint_sequence {
            return Err(invalid(
                "creating the node-local column family moved the checkpoint sequence",
            ));
        }
        db.put_cf_opt(local_family(&db)?, CURSOR_KEY, bytes, &sync_options())?;
        if db.latest_sequence_number() != cursor.local_sequence {
            return Err(invalid("receiver was modified outside physical replication"));
        }
        Ok(Self {
            db: Arc::new(db),
            cursor: Mutex::new(cursor),
            poisoned: AtomicBool::new(false),
        })
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
        Ok(Self {
            db: Arc::new(db),
            cursor: Mutex::new(cursor),
            poisoned: AtomicBool::new(false),
        })
    }

    /// The receiver's handle. Reads are free; a write through it moves the
    /// sequence and poisons this writer at its next write.
    pub fn db(&self) -> &Arc<DB> {
        &self.db
    }

    /// The durable cursor as of the last successful write.
    pub fn cursor(&self) -> PhysicalCursor {
        self.cursor.lock().clone()
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
    /// fsynced write, then adopt `next`. A failed write leaves the outcome
    /// uncertain and poisons; so does a sequence that moved by anything but
    /// this write.
    fn commit(&self, cursor: &mut PhysicalCursor, next: PhysicalCursor, batch: WriteBatch) -> Result<()> {
        if let Err(error) = self.db.write_opt(batch, &sync_options()) {
            self.poison();
            return Err(error.into());
        }
        *cursor = next;
        if self.db.latest_sequence_number() != cursor.local_sequence {
            self.poison();
            return Err(invalid("receiver was modified outside physical replication"));
        }
        Ok(())
    }

    /// Returns false only for an exact retransmission of the last applied
    /// batch. Old, overlapping, conflicting and gap-crossing batches refuse.
    /// The source bytes are applied verbatim to the default column family and
    /// the cursor goes to [`LOCAL_CF`], in the same fsynced write.
    pub fn apply_source(&self, batch: &PhysicalBatch) -> Result<bool> {
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
        self.commit(&mut cursor, next, imported)?;
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
        if batch.ops.len() > MAX_BATCH_OPERATIONS as usize {
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
        self.commit(&mut cursor, next, write)
    }

    /// Prove the durable cursor is the one in memory, then close the handle.
    /// Refuses while anything else still holds the handle: a caller that
    /// renames or reopens the directory next relies on it being closed.
    fn close(self) -> Result<PhysicalCursor> {
        if self.is_poisoned() {
            return Err(PhysicalError::Poisoned);
        }
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
        Ok(Self {
            writer: Arc::new(LocalWriter::from_checkpoint(db, history, checkpoint_sequence)?),
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

    /// Close staging before installation. Serving requires its own validated
    /// physical-replica open path, not ordinary logical-recovery startup. The
    /// writer must no longer be shared.
    pub fn close(self) -> Result<PhysicalCursor> {
        Arc::try_unwrap(self.writer)
            .map_err(|_| invalid("the physical receiver's writer is still shared; it cannot be closed"))?
            .close()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
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
        let polled = poll_source(&source, history, cut, MAX_BATCH_BYTES).unwrap();
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
        let first = poll_source(&source, history, 0, 30).unwrap();
        assert_eq!(first.batches.len(), 1);
        assert_eq!(first.through, 2);
        assert_eq!(first.durable_barrier, 6);
        let remaining = poll_source(&source, history, first.through, MAX_BATCH_BYTES).unwrap();
        assert_eq!(remaining.batches.len(), 2);
        assert_eq!(remaining.through, 6);
        assert!(matches!(
            poll_source(&source, history, 1, MAX_BATCH_BYTES),
            Err(PhysicalError::Gap { .. })
        ));
        assert!(poll_source(&source, history, 7, MAX_BATCH_BYTES).is_err());
        let caught_up = poll_source(&source, history, 6, MAX_BATCH_BYTES).unwrap();
        assert!(caught_up.batches.is_empty());
        assert_eq!(caught_up.through, 6);
        assert!(poll_source(&source, history, 0, 1).is_err());
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

        let polled = poll_source(&source, history, cut, MAX_BATCH_BYTES).unwrap();
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
        let caught_up = poll_source(&source, history, cut + 3, MAX_BATCH_BYTES).unwrap();
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
        assert!(poll_source(receiver.writer().db(), history, 0, MAX_BATCH_BYTES).is_err());
        receiver.close().unwrap();
        // A read-only handle may open only the default family; the MANIFEST
        // still names the node-local one.
        let partial = DB::open_for_read_only(&Options::default(), root.path(), false).unwrap();
        assert!(partial.cf_handle(LOCAL_CF).is_none());
        assert!(poll_source(&partial, history, 0, MAX_BATCH_BYTES).is_err());
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
}
