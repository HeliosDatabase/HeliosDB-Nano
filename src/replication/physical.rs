//! Physical replication primitives for resync staging.
//!
//! Source positions are RocksDB sequence numbers, never logical Nano LSNs.
//! These primitives do not publish changes to a serving `StorageEngine`: its
//! catalog, indexes and caches require a separate publication protocol.

use rocksdb::{WriteBatch, WriteOptions, DB};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const FORMAT: u32 = 1;
const CURSOR_KEY: &[u8] = b"__nano_physical_replica_cursor_v1";
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
        let encoded_first = u64::from_le_bytes(raw[..8].try_into().expect("validated header"));
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

/// Decode the pinned RocksDB 8.10 default-column-family batch format. Preserve
/// the original bytes for application; the C API's put/delete callback loses
/// range deletions. Unknown opcodes fail closed rather than silently advancing.
fn validate_raw_batch(raw: &[u8]) -> Result<u32> {
    if !(12..=MAX_BATCH_BYTES).contains(&raw.len()) {
        return Err(invalid("batch byte limit/header"));
    }
    let count = u32::from_le_bytes(raw[8..12].try_into().expect("validated header"));
    if count == 0 || count > MAX_BATCH_OPERATIONS {
        return Err(invalid("batch operation limit"));
    }
    let mut input = &raw[12..];
    let mut observed = 0_u32;
    while let Some((&tag, rest)) = input.split_first() {
        input = rest;
        match tag {
            // kTypeDeletion / kTypeSingleDeletion.
            0 | 7 => {
                let key = take_slice(&mut input)?;
                check_source_key(key)?;
                observed += 1;
            }
            // kTypeValue.
            1 => {
                let key = take_slice(&mut input)?;
                check_source_key(key)?;
                let _value = take_slice(&mut input)?;
                observed += 1;
            }
            // kTypeLogData: does not consume a sequence number.
            3 => {
                let _blob = take_slice(&mut input)?;
            }
            // kTypeRangeDeletion, half-open interval [start, end).
            15 => {
                let start = take_slice(&mut input)?;
                let end = take_slice(&mut input)?;
                if start > end || (start <= CURSOR_KEY && CURSOR_KEY < end) {
                    return Err(invalid("invalid or receiver-metadata-overlapping range deletion"));
                }
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

fn check_source_key(key: &[u8]) -> Result<()> {
    if key == CURSOR_KEY {
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

/// Poll committed physical changes. The owner must ensure all source writes
/// retain the RocksDB WAL; this function detects gaps but cannot manufacture a
/// retention lease. Only use cursors from a completed checkpoint or full batch.
pub fn poll_source(db: &DB, history: Uuid, after: u64, byte_limit: usize) -> Result<PhysicalPoll> {
    check_history(history)?;
    if byte_limit == 0 || byte_limit > MAX_BATCH_BYTES {
        return Err(invalid("invalid poll byte limit"));
    }
    if db.get(CURSOR_KEY)?.is_some() {
        return Err(invalid("a physical receiver cannot be exported as a primary"));
    }
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

fn read_cursor(db: &DB) -> Result<PhysicalCursor> {
    let bytes = db
        .get(CURSOR_KEY)?
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

/// Owns a staging DB exclusively. It must never be attached directly to a
/// serving Nano handle: raw durability does not update Nano's derived state.
pub struct StagedPhysicalReplica {
    db: DB,
    cursor: PhysicalCursor,
    poisoned: bool,
}

impl StagedPhysicalReplica {
    /// Initialize only a validated, independently copied physical checkpoint.
    /// The caller proves manifest/history integrity and holds the DB lock.
    pub fn from_checkpoint(db: DB, history: Uuid, checkpoint_sequence: u64) -> Result<Self> {
        check_history(history)?;
        if db.latest_sequence_number() != checkpoint_sequence || db.get(CURSOR_KEY)?.is_some() {
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
        let bytes = serde_json::to_vec(&cursor).map_err(|e| invalid(e.to_string()))?;
        let mut options = WriteOptions::default();
        options.set_sync(true);
        db.put_opt(CURSOR_KEY, bytes, &options)?;
        Ok(Self {
            db,
            cursor,
            poisoned: false,
        })
    }

    /// Reopen raw staging without invoking Nano logical recovery/maintenance.
    pub fn resume(db: DB, history: Uuid) -> Result<Self> {
        let cursor = read_cursor(&db)?;
        if cursor.history != history {
            return Err(PhysicalError::HistoryMismatch);
        }
        Ok(Self {
            db,
            cursor,
            poisoned: false,
        })
    }

    pub fn cursor(&self) -> &PhysicalCursor {
        &self.cursor
    }

    /// Returns false only for an exact retransmission of the last applied
    /// batch. Old, overlapping, conflicting and gap-crossing batches refuse.
    pub fn apply(&mut self, batch: &PhysicalBatch) -> Result<bool> {
        if self.poisoned {
            return Err(PhysicalError::Poisoned);
        }
        if self.db.latest_sequence_number() != self.cursor.local_sequence {
            self.poisoned = true;
            return Err(invalid("receiver was modified outside physical replication"));
        }
        if batch.history != self.cursor.history {
            return Err(PhysicalError::HistoryMismatch);
        }
        if batch.last == self.cursor.source_sequence
            && Some(batch.first) == self.cursor.last_first_sequence
            && Some(batch.digest) == self.cursor.last_digest
        {
            return Ok(false);
        }
        if batch.first != self.cursor.source_sequence + 1 {
            return Err(PhysicalError::Gap {
                expected: self.cursor.source_sequence + 1,
                observed: batch.first,
            });
        }
        let mut cursor = self.cursor.clone();
        // The receiver cursor consumes one extra local sequence per batch.
        let local_operations = batch.last - batch.first + 2;
        check_local_advance(cursor.local_sequence, local_operations)?;
        cursor.local_sequence += local_operations;
        cursor.source_sequence = batch.last;
        cursor.last_first_sequence = Some(batch.first);
        cursor.last_digest = Some(batch.digest);
        let bytes = serde_json::to_vec(&cursor).map_err(|e| invalid(e.to_string()))?;
        let mut imported = WriteBatch::from_data(&batch.raw);
        imported.put(CURSOR_KEY, bytes);
        let mut options = WriteOptions::default();
        options.set_sync(true);
        if let Err(error) = self.db.write_opt(imported, &options) {
            self.poisoned = true;
            return Err(error.into());
        }
        self.cursor = cursor;
        Ok(true)
    }

    /// Close staging before installation. Serving requires its own validated
    /// physical-replica open path, not ordinary logical-recovery startup.
    pub fn close(self) -> Result<PhysicalCursor> {
        if self.poisoned {
            return Err(PhysicalError::Poisoned);
        }
        let durable = read_cursor(&self.db)?;
        if durable != self.cursor {
            return Err(invalid("durable receiver cursor changed unexpectedly"));
        }
        Ok(self.cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rocksdb::{checkpoint::Checkpoint, Options};
    use tempfile::TempDir;

    fn open(path: &std::path::Path) -> DB {
        let mut options = Options::default();
        options.create_if_missing(true);
        options.set_wal_ttl_seconds(3600);
        DB::open(&options, path).unwrap()
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
        let mut receiver = StagedPhysicalReplica::from_checkpoint(copy, history, cut).unwrap();
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
        assert_eq!(receiver.db.get(b"seed").unwrap(), None);
        assert_eq!(receiver.db.get(b"a").unwrap().as_deref(), Some(b"first".as_slice()));
        let cursor = receiver.close().unwrap();
        assert_eq!(cursor.source_sequence(), cut + 3);
        let copy = open(&copy_path);
        // Receiver-local cursor writes make its own sequence differ from source.
        assert!(copy.latest_sequence_number() > cursor.source_sequence());
        let mut resumed = StagedPhysicalReplica::resume(copy, history).unwrap();
        assert_eq!(resumed.cursor(), &cursor);
        assert!(!resumed.apply(&polled.batches[0]).unwrap());
        assert_eq!(resumed.db.get(b"b").unwrap().as_deref(), Some(b"second".as_slice()));
    }

    #[test]
    fn staging_rejects_history_gaps_conflicting_duplicates_without_advancing() {
        let root = TempDir::new().unwrap();
        let history = Uuid::new_v4();
        let mut receiver = StagedPhysicalReplica::from_checkpoint(open(root.path()), history, 0).unwrap();
        let mut original = WriteBatch::default();
        original.put(b"key", b"original");
        let original = encoded(history, 1, original);
        receiver.apply(&original).unwrap();
        let cursor = receiver.cursor().clone();
        for (batch_history, first) in [(Uuid::new_v4(), 2), (history, 3), (history, 1)] {
            let mut batch = WriteBatch::default();
            batch.put(b"key", b"bad");
            assert!(receiver.apply(&encoded(batch_history, first, batch)).is_err());
            assert_eq!(receiver.cursor(), &cursor);
            assert_eq!(read_cursor(&receiver.db).unwrap(), cursor);
            assert_eq!(
                receiver.db.get(b"key").unwrap().as_deref(),
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
        reserved.put(CURSOR_KEY, b"forged");
        assert!(validate_raw_batch(reserved.data()).is_err());
        let mut range = WriteBatch::default();
        range.delete_range(&b""[..], &b"zz"[..]);
        assert!(validate_raw_batch(range.data()).is_err());
        let mut unsupported = WriteBatch::default();
        unsupported.merge(b"a", b"b");
        assert!(validate_raw_batch(unsupported.data()).is_err());
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
        let mut receiver = StagedPhysicalReplica::from_checkpoint(db, history, cut).unwrap();
        let mut batch = WriteBatch::default();
        batch.delete_range(b"rb", b"rd");
        receiver.apply(&encoded(history, cut + 1, batch)).unwrap();
        assert!(receiver.db.get(b"ra").unwrap().is_some());
        assert!(receiver.db.get(b"rb").unwrap().is_none());
        assert!(receiver.db.get(b"rc").unwrap().is_none());
        assert!(receiver.db.get(b"rd").unwrap().is_some());
        assert_eq!(receiver.cursor().source_sequence(), cut + 1);
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

    #[test]
    fn receiver_cannot_become_source_and_checkpoint_cannot_overwrite_progress() {
        let root = TempDir::new().unwrap();
        let history = Uuid::new_v4();
        let receiver = StagedPhysicalReplica::from_checkpoint(open(root.path()), history, 0).unwrap();
        assert!(poll_source(&receiver.db, history, 0, MAX_BATCH_BYTES).is_err());
        receiver.close().unwrap();
        let db = open(root.path());
        let local = db.latest_sequence_number();
        assert!(StagedPhysicalReplica::from_checkpoint(db, history, local).is_err());
        let db = open(root.path());
        assert!(StagedPhysicalReplica::resume(db, Uuid::new_v4()).is_err());
    }

    #[test]
    fn cursor_corruption_is_never_adopted() {
        let root = TempDir::new().unwrap();
        let db = open(root.path());
        db.put(CURSOR_KEY, b"{}").unwrap();
        assert!(StagedPhysicalReplica::resume(db, Uuid::new_v4()).is_err());
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
        let db = open(root.path());
        db.put(b"local", b"unrelated").unwrap();
        assert!(StagedPhysicalReplica::resume(db, history).is_err());

        let other = TempDir::new().unwrap();
        let mut receiver = StagedPhysicalReplica::from_checkpoint(open(other.path()), history, 0).unwrap();
        receiver.db.put(b"local", b"unrelated").unwrap();
        let mut batch = WriteBatch::default();
        batch.put(b"key", b"not applied");
        assert!(receiver.apply(&encoded(history, 1, batch)).is_err());
        assert!(receiver.db.get(b"key").unwrap().is_none());
        assert!(matches!(receiver.close(), Err(PhysicalError::Poisoned)));
    }
}
