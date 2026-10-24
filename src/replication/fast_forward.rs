//! Leaves of an incremental fast-forward (spec P4.1 §3, plan
//! `PHYSICAL_STANDBY_REPLICATION.md` §D6 "from the primary"): Oracle's
//! `RECOVER STANDBY DATABASE FROM SERVICE`, for a standby away longer than
//! its primary's recovery window.
//!
//! # Leaves
//!
//! A physical standby's default column family is a byte-for-byte copy of its
//! primary's at its cursor — ciphertext included, every value is applied
//! verbatim — so two stores hold the same key range exactly when the SHA-256
//! of its pairs agrees (`physical::range_digest`'s framing,
//! `len_le64(k) ‖ k ‖ len_le64(v) ‖ v` in key order). The primary scans the
//! checkpoint of an export ONCE and cuts it into leaves of about
//! `leaf_bytes` key and value bytes ([`LeafPlanner`]): a leaf closes before
//! the key that would make it reach the size, so every leaf but the last
//! holds at least `leaf_bytes` and every boundary is a real key. Leaf 0
//! starts at the empty key; leaf `i` ends where leaf `i + 1` starts; the last
//! ends at [`LEAF_END`]. The leaves therefore partition the WHOLE default
//! column family — on a standby it holds the primary's bytes only, its own
//! state lives in the node-local family.
//!
//! The standby digests its own store against the primary's boundaries
//! ([`LocalDigests`]) in one ordered scan; only the leaves whose digests
//! differ travel, each as an SST file the primary builds from its checkpoint
//! on request ([`FastForwardPlan::leaf`]) and the standby ingests after
//! deleting the leaf's range. A flat list costs 32 bytes and a boundary per
//! leaf (1 TiB in 4 MiB leaves: about 25 MB, paged); no tree is needed.
//!
//! # The primary's plan
//!
//! One plan per export and leaf size (`OnlineCheckpointExport::fast_forward_plan`),
//! built on a blocking thread under the source's one planning slot (never a
//! session's work permit) while its sessions answer "planning", over a
//! read-only open of the checkpoint kept for the plan's life. A plan whose
//! planning failed is replaced by the next session that asks. Leaf files are
//! built one at a time, on request, into a directory of the plan's own inside
//! the export's lease, removed with the plan's last reference.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use rocksdb::{ColumnFamilyDescriptor, Direction, IteratorMode, Options, ReadOptions, SstFileWriter, DB};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::physical_wire::{
    empty_sha256, LeafDigests, LeafEntry, LeafReady, LeafStatus, LEAF_DIGESTS_HEADER_BYTES, LEAF_END,
    MAX_LEAF_DIGESTS_BYTES, MAX_LEAF_START_BYTES, MAX_LEAVES,
};
use crate::storage::StorageEngine;
use crate::Config;

/// Spec P4.1 §3.3.4: a standby applies the differing leaves in batches of
/// this many — one range-deletion write and one ingestion per batch.
pub const BATCH_LEAVES: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum FastForwardError {
    #[error("fast-forward storage: {0}")]
    Storage(#[from] rocksdb::Error),
    #[error("fast-forward I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("fast-forward: {0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, FastForwardError>;

fn invalid(message: impl Into<String>) -> FastForwardError {
    FastForwardError::Invalid(message.into())
}

/// The key every leaf must stay below: [`LEAF_END`]. No classified prefix of
/// the engine's keyspace starts with `0xff`, so a key at or past it is not a
/// replicated key and the fast-forward refuses the store.
fn check_in_keyspace(key: &[u8]) -> Result<()> {
    if key >= &LEAF_END[..] {
        return Err(invalid(format!(
            "a key outside the replicated keyspace ({} bytes from 0x{}): the store cannot be fast-forwarded",
            key.len(),
            hex::encode(key.get(..key.len().min(8)).unwrap_or_default())
        )));
    }
    Ok(())
}

/// One leaf's running digest: SHA-256 over the `len_le64(k) ‖ k ‖ len_le64(v)
/// ‖ v` framing of its pairs, in key order (`physical::range_digest`'s), and
/// its key and value bytes and keys.
#[derive(Clone)]
pub struct LeafHasher {
    hash: Sha256,
    bytes: u64,
    keys: u64,
}

impl Default for LeafHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl LeafHasher {
    pub fn new() -> Self {
        Self {
            hash: Sha256::new(),
            bytes: 0,
            keys: 0,
        }
    }

    pub fn add(&mut self, key: &[u8], value: &[u8]) {
        for field in [key, value] {
            self.hash.update((field.len() as u64).to_le_bytes());
            self.hash.update(field);
        }
        self.bytes = self.bytes.saturating_add((key.len() + value.len()) as u64);
        self.keys = self.keys.saturating_add(1);
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn keys(&self) -> u64 {
        self.keys
    }

    pub fn finish(self) -> [u8; 32] {
        self.hash.finalize().into()
    }
}

/// One leaf of a plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedLeaf {
    pub start: Vec<u8>,
    pub bytes: u64,
    pub keys: u32,
    pub digest: [u8; 32],
}

impl PlannedLeaf {
    pub fn entry(&self) -> LeafEntry {
        LeafEntry {
            start: self.start.clone(),
            bytes: self.bytes,
            keys: self.keys,
            digest: self.digest,
        }
    }
}

/// Cuts an ordered stream of pairs into leaves (see the module
/// documentation): a leaf closes BEFORE the key that arrives once it holds
/// `leaf_bytes` — so leaves are at least `leaf_bytes` but for the last, and
/// boundaries are real keys — unless that key is too long to name a
/// boundary on the wire, when the leaf grows on. At most `max_leaves`.
pub struct LeafPlanner {
    leaf_bytes: u64,
    max_leaves: usize,
    leaves: Vec<PlannedLeaf>,
    start: Vec<u8>,
    current: LeafHasher,
    /// The previous key (its buffer reused), once there is one.
    last_key: Vec<u8>,
    any_key: bool,
}

impl LeafPlanner {
    pub fn new(leaf_bytes: u64, max_leaves: usize) -> Self {
        Self {
            leaf_bytes: leaf_bytes.max(1),
            max_leaves: max_leaves.max(1),
            leaves: Vec::new(),
            start: Vec::new(),
            current: LeafHasher::new(),
            last_key: Vec::new(),
            any_key: false,
        }
    }

    /// The next pair, in strictly increasing key order.
    pub fn add(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        check_in_keyspace(key)?;
        if self.any_key && self.last_key.as_slice() >= key {
            return Err(invalid("the planned keys are not in strictly increasing order"));
        }
        if self.current.bytes() >= self.leaf_bytes
            && self.current.keys() > 0
            && key.len() <= MAX_LEAF_START_BYTES
            && !key.is_empty()
        {
            if self.leaves.len() + 2 > self.max_leaves {
                return Err(invalid(format!(
                    "more than {} leaves of {} bytes: raise --leaf-bytes",
                    self.max_leaves, self.leaf_bytes
                )));
            }
            let start = std::mem::replace(&mut self.start, key.to_vec());
            let current = std::mem::take(&mut self.current);
            self.leaves.push(Self::closed(start, current)?);
        }
        self.current.add(key, value);
        self.last_key.clear();
        self.last_key.extend_from_slice(key);
        self.any_key = true;
        Ok(())
    }

    fn closed(start: Vec<u8>, hasher: LeafHasher) -> Result<PlannedLeaf> {
        let keys = u32::try_from(hasher.keys())
            .map_err(|_| invalid("a leaf holds more than 2^32 keys: lower --leaf-bytes"))?;
        let bytes = hasher.bytes();
        Ok(PlannedLeaf {
            start,
            bytes,
            keys,
            digest: hasher.finish(),
        })
    }

    /// The plan: at least one leaf (an empty store is one empty leaf).
    pub fn finish(mut self) -> Result<Vec<PlannedLeaf>> {
        let start = std::mem::take(&mut self.start);
        let last = Self::closed(start, self.current)?;
        self.leaves.push(last);
        Ok(self.leaves)
    }
}

/// A store's own digests against another store's leaf boundaries (spec
/// P4.1 §3.3.3): `starts` are the leaves' first keys (leaf 0's empty), and
/// each pair, in key order, is hashed into the leaf whose range holds it. A
/// leaf with no pair here has the digest of nothing.
pub struct LocalDigests<'a> {
    starts: &'a [Vec<u8>],
    index: usize,
    current: LeafHasher,
    digests: Vec<[u8; 32]>,
}

impl<'a> LocalDigests<'a> {
    pub fn new(starts: &'a [Vec<u8>]) -> Result<Self> {
        if starts.first().is_none_or(|first| !first.is_empty()) {
            return Err(invalid("a leaf plan starts with leaf 0 at the empty key"));
        }
        Ok(Self {
            starts,
            index: 0,
            current: LeafHasher::new(),
            digests: Vec::with_capacity(starts.len()),
        })
    }

    pub fn add(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        check_in_keyspace(key)?;
        while self
            .starts
            .get(self.index + 1)
            .is_some_and(|next| key >= next.as_slice())
        {
            let done = std::mem::take(&mut self.current);
            self.digests.push(done.finish());
            self.index += 1;
        }
        self.current.add(key, value);
        Ok(())
    }

    /// One digest per leaf, in leaf order.
    pub fn finish(mut self) -> Vec<[u8; 32]> {
        self.digests.push(self.current.finish());
        while self.digests.len() < self.starts.len() {
            self.digests.push(empty_sha256());
        }
        self.digests
    }
}

/// Every pair of `db`'s default column family, in total key order (the
/// engine's prefix extractor must not end the scan at a prefix boundary).
fn scan_default<'a>(db: &'a DB, from: &[u8], until: Option<&[u8]>) -> rocksdb::DBIterator<'a> {
    let mut read = ReadOptions::default();
    read.set_total_order_seek(true);
    read.fill_cache(false);
    if let Some(until) = until {
        read.set_iterate_upper_bound(until.to_vec());
    }
    db.iterator_opt(IteratorMode::From(from, Direction::Forward), read)
}

/// Spec P4.1 §3.3.3: the standby's digests of its default column family
/// against the primary's leaf boundaries: ONE ordered scan.
pub fn local_leaf_digests(db: &DB, starts: &[Vec<u8>]) -> Result<Vec<[u8; 32]>> {
    let mut digests = LocalDigests::new(starts)?;
    for item in scan_default(db, b"", None) {
        let (key, value) = item?;
        digests.add(&key, &value)?;
    }
    Ok(digests.finish())
}

// ---------------------------------------------------------------------------
// The primary's side: one plan per export and leaf size.
// ---------------------------------------------------------------------------

enum PlanState {
    /// Created; nobody started planning yet.
    Idle,
    Planning,
    Ready(Arc<ReadyPlan>),
    /// Planning failed: the reason refuses every session of this plan.
    Failed(String),
}

struct ReadyPlan {
    /// The read-only open of the checkpoint the leaves are read from.
    db: Arc<DB>,
    leaves: Vec<PlannedLeaf>,
    total_bytes: u64,
    built: Mutex<HashMap<u32, BuiltLeaf>>,
}

#[derive(Clone)]
struct BuiltLeaf {
    size: u64,
    sha256: [u8; 32],
    /// The file as it was hashed (none for a leaf with no key): a chunk is
    /// read only from that same file.
    file: Option<FileIdentity>,
}

#[derive(Clone, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

impl FileIdentity {
    fn of(metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        let (device, inode) = {
            use std::os::unix::fs::MetadataExt;
            (metadata.dev(), metadata.ino())
        };
        #[cfg(not(unix))]
        let (device, inode) = (0, 0);
        Self {
            device,
            inode,
            len: metadata.len(),
            modified: metadata.modified().ok(),
        }
    }
}

/// The leaf plan of one export's checkpoint at one leaf size (see the module
/// documentation). Shared by the export and every fast-forward session over
/// it; planned once, on a blocking thread.
pub struct FastForwardPlan {
    leaf_bytes: u32,
    checkpoint: PathBuf,
    leaves_dir: PathBuf,
    /// The storage options of the leaf files and of the checkpoint's open:
    /// the engine's (`StorageEngine::build_db_options`), so an ingested leaf
    /// is laid out as the standby's own tables.
    config: Config,
    state: Mutex<PlanState>,
    /// One leaf file is built at a time.
    building: Mutex<()>,
}

/// The leaf files go with the plan's LAST reference — never while a session
/// that may still download them holds it.
impl Drop for FastForwardPlan {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.leaves_dir) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    "replication: the leaf files of a fast-forward plan ({}) could not be removed: {error}",
                    self.leaves_dir.display()
                );
            }
        }
    }
}

/// The claim on a plan's planning ([`FastForwardPlan::begin`]). Dropped
/// without having planned — the job never started, panicked, or its future
/// was dropped — it abandons the plan, so its sessions are answered a
/// failure (and the next session gets a new plan) rather than "planning"
/// forever.
pub struct PlanningGuard {
    plan: Arc<FastForwardPlan>,
}

impl PlanningGuard {
    /// Run the planning ([`FastForwardPlan::plan`]); blocking.
    pub fn plan(self, stop: &dyn Fn() -> bool) {
        self.plan.plan(stop);
    }
}

impl Drop for PlanningGuard {
    fn drop(&mut self) {
        // A no-op once the planning reached Ready or Failed.
        self.plan.abandon(
            "the leaf planning ended without a plan (it did not start, or failed unexpectedly); see the primary's \
             log, and retry",
        );
    }
}

impl FastForwardPlan {
    pub fn new(leaf_bytes: u32, checkpoint: PathBuf, leaves_dir: PathBuf, config: Config) -> Self {
        Self {
            leaf_bytes,
            checkpoint,
            leaves_dir,
            config,
            state: Mutex::new(PlanState::Idle),
            building: Mutex::new(()),
        }
    }

    pub fn leaf_bytes(&self) -> u32 {
        self.leaf_bytes
    }

    /// Where its leaf files are written (inside the export's lease, a
    /// directory of this plan's own, removed when the plan is dropped).
    pub fn leaves_dir(&self) -> &Path {
        &self.leaves_dir
    }

    /// Claim the planning: the guard for the one caller that must now run
    /// [`Self::plan`] (through [`PlanningGuard::plan`]); `None` when it is
    /// under way or done. However the guard ends — its job planned, failed
    /// to start, panicked, or its future was dropped — the plan is never
    /// left planning: a dropped guard abandons it.
    pub fn begin(self: &Arc<Self>) -> Option<PlanningGuard> {
        let mut state = self.state.lock();
        if matches!(*state, PlanState::Idle) {
            *state = PlanState::Planning;
            return Some(PlanningGuard { plan: Arc::clone(self) });
        }
        None
    }

    /// The planning ended without a plan (or could not be started): every
    /// session of the plan is answered `reason`. No-op once it is ready or
    /// failed already.
    pub fn abandon(&self, reason: &str) {
        let mut state = self.state.lock();
        if matches!(*state, PlanState::Idle | PlanState::Planning) {
            *state = PlanState::Failed(reason.to_string());
        }
    }

    /// Whether the planning failed (or was abandoned): such a plan is
    /// replaced by the next session that asks for one.
    pub fn is_failed(&self) -> bool {
        matches!(*self.state.lock(), PlanState::Failed(_))
    }

    fn options(&self) -> Result<(Options, Vec<ColumnFamilyDescriptor>)> {
        let (mut options, families) = StorageEngine::build_db_options(&self.config, &self.checkpoint)
            .map_err(|error| invalid(format!("the storage options of the leaves: {error}")))?;
        options.create_if_missing(false);
        Ok((options, families))
    }

    /// Plan: open the checkpoint read-only and cut its default column family
    /// into leaves, in ONE ordered scan. Blocking. `stop` is asked every few
    /// thousand keys: the source is draining, or nobody holds the plan any
    /// more. The outcome — the plan, or why it failed — is what every
    /// session of the plan is answered from.
    pub fn plan(&self, stop: &dyn Fn() -> bool) {
        let outcome = self.scan(stop);
        let mut state = self.state.lock();
        *state = match outcome {
            Ok(ready) => {
                tracing::info!(
                    "replication: fast-forward plan of {} ready: {} leaves of {} bytes, {} bytes in all",
                    self.checkpoint.display(),
                    ready.leaves.len(),
                    self.leaf_bytes,
                    ready.total_bytes
                );
                PlanState::Ready(Arc::new(ready))
            }
            Err(error) => {
                tracing::warn!("replication: planning a fast-forward failed: {error}");
                PlanState::Failed(error.to_string())
            }
        };
    }

    fn scan(&self, stop: &dyn Fn() -> bool) -> Result<ReadyPlan> {
        let (options, families) = self.options()?;
        let db = DB::open_cf_descriptors_read_only(&options, &self.checkpoint, families, false)?;
        let mut planner = LeafPlanner::new(u64::from(self.leaf_bytes), MAX_LEAVES as usize);
        let mut seen = 0_u64;
        for item in scan_default(&db, b"", None) {
            let (key, value) = item?;
            planner.add(&key, &value)?;
            seen += 1;
            if seen % 4096 == 0 && stop() {
                return Err(invalid(
                    "the planning was abandoned: the source is stopping or nobody waits for it",
                ));
            }
        }
        let leaves = planner.finish()?;
        let total_bytes = leaves.iter().map(|leaf| leaf.bytes).sum();
        Ok(ReadyPlan {
            db: Arc::new(db),
            leaves,
            total_bytes,
            built: Mutex::new(HashMap::new()),
        })
    }

    fn ready(&self) -> Result<Arc<ReadyPlan>> {
        match &*self.state.lock() {
            PlanState::Ready(ready) => Ok(Arc::clone(ready)),
            PlanState::Failed(reason) => Err(invalid(format!("the leaf plan failed: {reason}"))),
            PlanState::Idle | PlanState::Planning => Err(invalid("the leaf plan is not ready yet")),
        }
    }

    /// The page of leaves `from..` (at most `count`, and no more than a
    /// `LeafDigests` frame holds), or a planning page. `Err` carries the
    /// reason a failed planning gives. Answers from memory.
    pub fn page(&self, snapshot_id: Uuid, from: u32, count: u32) -> std::result::Result<LeafDigests, String> {
        let ready = match &*self.state.lock() {
            PlanState::Ready(ready) => Arc::clone(ready),
            PlanState::Failed(reason) => return Err(reason.clone()),
            PlanState::Idle | PlanState::Planning => return Ok(LeafDigests::planning(snapshot_id)),
        };
        let leaf_count = u32::try_from(ready.leaves.len()).map_err(|_| "too many leaves".to_string())?;
        if from > leaf_count {
            return Err(format!("leaf {from} is past the plan's {leaf_count} leaves"));
        }
        let mut size = LEAF_DIGESTS_HEADER_BYTES;
        let mut entries = Vec::new();
        for leaf in ready.leaves.iter().skip(from as usize).take(count as usize) {
            let entry = leaf.entry();
            if size + entry.encoded_len() > MAX_LEAF_DIGESTS_BYTES {
                break;
            }
            size += entry.encoded_len();
            entries.push(entry);
        }
        Ok(LeafDigests {
            snapshot_id,
            status: LeafStatus::Ready,
            leaf_count,
            total_bytes: ready.total_bytes,
            from,
            entries,
        })
    }

    /// Spec P4.1 §3.2: build leaf `index`'s SST file from the checkpoint —
    /// once; a second request announces the same file — and announce it. A
    /// leaf with no key has no file (size 0). Blocking, one leaf at a time.
    pub fn leaf(&self, snapshot_id: Uuid, index: u32) -> Result<LeafReady> {
        let ready = self.ready()?;
        let _building = self.building.lock();
        if let Some(built) = ready.built.lock().get(&index) {
            return Ok(LeafReady {
                snapshot_id,
                index,
                size: built.size,
                sha256: built.sha256,
            });
        }
        let leaf = ready
            .leaves
            .get(index as usize)
            .ok_or_else(|| invalid(format!("leaf {index} is past the plan's {} leaves", ready.leaves.len())))?;
        let end = ready.leaves.get(index as usize + 1).map(|next| next.start.clone());
        let built = if leaf.keys == 0 {
            BuiltLeaf {
                size: 0,
                sha256: empty_sha256(),
                file: None,
            }
        } else {
            self.build(&ready.db, index, leaf, end.as_deref())?
        };
        ready.built.lock().insert(index, built.clone());
        Ok(LeafReady {
            snapshot_id,
            index,
            size: built.size,
            sha256: built.sha256,
        })
    }

    fn leaf_path(&self, index: u32) -> PathBuf {
        self.leaves_dir.join(format!("{index}.sst"))
    }

    /// Write the leaf's pairs into `<leaves>/<index>.sst` (through a
    /// temporary name), check they are the plan's, and hash the file.
    fn build(&self, db: &DB, index: u32, leaf: &PlannedLeaf, end: Option<&[u8]>) -> Result<BuiltLeaf> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&self.leaves_dir)?;
        let path = self.leaf_path(index);
        let temporary = self.leaves_dir.join(format!("{index}.sst.tmp"));
        match fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let (options, _) = self.options()?;
        let mut hasher = LeafHasher::new();
        {
            let mut writer = SstFileWriter::create(&options);
            writer.open(&temporary)?;
            for item in scan_default(db, &leaf.start, end) {
                let (key, value) = item?;
                hasher.add(&key, &value);
                writer.put(key, value)?;
            }
            writer.finish()?;
        }
        let keys = hasher.keys();
        if keys != u64::from(leaf.keys) || hasher.finish() != leaf.digest {
            let _ = fs::remove_file(&temporary);
            return Err(invalid(format!(
                "leaf {index} of the checkpoint no longer matches its plan ({keys} keys read, {} planned)",
                leaf.keys
            )));
        }
        fs::rename(&temporary, &path)?;
        let (size, sha256, metadata) = hash_file(&path)?;
        Ok(BuiltLeaf {
            size,
            sha256,
            file: Some(FileIdentity::of(&metadata)),
        })
    }

    /// `length` bytes of leaf `index`'s built file from `offset`. The file is
    /// the one that was hashed — same inode, size and modification time,
    /// checked before and after the read. Blocking.
    pub fn read_leaf_chunk(&self, index: u32, offset: u64, length: u32) -> Result<Vec<u8>> {
        let ready = self.ready()?;
        let built = ready
            .built
            .lock()
            .get(&index)
            .cloned()
            .ok_or_else(|| invalid(format!("leaf {index} was not built")))?;
        let expected = built
            .file
            .ok_or_else(|| invalid(format!("leaf {index} has no file: it holds no key")))?;
        let end = offset
            .checked_add(u64::from(length))
            .filter(|end| *end <= built.size && length > 0)
            .ok_or_else(|| invalid("leaf chunk outside the leaf file"))?;
        let path = self.leaf_path(index);
        let mut file = OpenOptions::new().read(true).open(&path)?;
        if FileIdentity::of(&file.metadata()?) != expected {
            return Err(invalid(format!("leaf {index}'s file changed after it was built")));
        }
        let mut bytes = vec![0; (end - offset) as usize];
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut bytes)?;
        if FileIdentity::of(&fs::metadata(&path)?) != expected {
            return Err(invalid(format!("leaf {index}'s file changed while it was read")));
        }
        Ok(bytes)
    }
}

/// The size and SHA-256 of a regular file, with its metadata as hashed.
pub fn hash_file(path: &Path) -> Result<(u64, [u8; 32], std::fs::Metadata)> {
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(invalid(format!("{} is not a regular file", path.display())));
    }
    let mut hash = Sha256::new();
    let mut buffer = vec![0_u8; 256 * 1024];
    let mut size = 0_u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let chunk = buffer
            .get(..count)
            .ok_or_else(|| invalid("read count exceeds its buffer"))?;
        hash.update(chunk);
        size += count as u64;
    }
    if size != metadata.len() {
        return Err(invalid(format!("{} changed while it was hashed", path.display())));
    }
    Ok((size, hash.finalize().into(), metadata))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::replication::physical::range_digest;

    fn key(index: u32) -> Vec<u8> {
        format!("key{index:06}").into_bytes()
    }

    /// `count` pairs of `value_len`-byte values.
    fn pairs(count: u32, value_len: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
        (0..count)
            .map(|index| (key(index), vec![index as u8; value_len]))
            .collect()
    }

    fn plan(pairs: &[(Vec<u8>, Vec<u8>)], leaf_bytes: u64, max_leaves: usize) -> Result<Vec<PlannedLeaf>> {
        let mut planner = LeafPlanner::new(leaf_bytes, max_leaves);
        for (key, value) in pairs {
            planner.add(key, value)?;
        }
        planner.finish()
    }

    fn local(pairs: &[(Vec<u8>, Vec<u8>)], starts: &[Vec<u8>]) -> Vec<[u8; 32]> {
        let mut digests = LocalDigests::new(starts).unwrap();
        for (key, value) in pairs {
            digests.add(key, value).unwrap();
        }
        digests.finish()
    }

    /// One planner per plan; a claim dropped without a plan (the job never
    /// ran, panicked, its future was dropped) leaves the plan failed, never
    /// planning; the leaf files go with the plan's LAST reference only.
    #[test]
    fn a_dropped_planning_claim_fails_the_plan_and_its_last_reference_removes_its_files() {
        let root = tempfile::tempdir().unwrap();
        let leaves = root.path().join("leaves");
        fs::create_dir(&leaves).unwrap();
        fs::write(leaves.join("0.sst"), b"leaf").unwrap();
        let shared = Arc::new(FastForwardPlan::new(
            64 * 1024,
            root.path().join("checkpoint"),
            leaves.clone(),
            Config::default(),
        ));
        let claim = shared.begin().expect("the first claim");
        assert!(shared.begin().is_none(), "one planner");
        assert!(!shared.is_failed());
        assert_eq!(shared.page(Uuid::new_v4(), 0, 1).unwrap().status, LeafStatus::Planning);
        drop(claim);
        assert!(shared.is_failed(), "a claim dropped without a plan fails it");
        assert!(shared.page(Uuid::new_v4(), 0, 1).is_err());
        assert!(shared.begin().is_none(), "a failed plan is replaced, not planned again");

        let session = Arc::clone(&shared);
        drop(shared);
        assert!(leaves.join("0.sst").exists(), "a session still holds the plan");
        drop(session);
        assert!(!leaves.exists(), "its last reference removes its files");
    }

    /// Spec P4.1 §5: boundaries are real keys, every leaf but the last holds
    /// at least `leaf_bytes`, the last is open-ended, and the leaves cover
    /// every pair once, with the framing digests of their own pairs.
    #[test]
    fn leaves_close_before_the_key_that_reaches_the_size_at_real_keys() {
        let pairs = pairs(1000, 90);
        let leaf_bytes = 4096_u64;
        let leaves = plan(&pairs, leaf_bytes, 1 << 24).unwrap();
        assert!(leaves.len() > 10, "{} leaves", leaves.len());
        assert!(leaves[0].start.is_empty(), "leaf 0 starts at the empty key");
        let keys: Vec<&Vec<u8>> = pairs.iter().map(|(key, _)| key).collect();
        for leaf in &leaves[1..] {
            assert!(keys.contains(&&leaf.start), "a boundary is a real key");
        }
        for (index, leaf) in leaves.iter().enumerate() {
            if index + 1 < leaves.len() {
                assert!(leaf.bytes >= leaf_bytes, "leaf {index}: {} bytes", leaf.bytes);
                // Closed before the key that would have reached the size.
                assert!(leaf.bytes < leaf_bytes + 99, "leaf {index}: {} bytes", leaf.bytes);
            }
            let end = leaves.get(index + 1).map(|next| next.start.clone());
            let mut hasher = LeafHasher::new();
            for (key, value) in pairs
                .iter()
                .filter(|(key, _)| *key >= leaf.start && end.as_ref().is_none_or(|end| key < end))
            {
                hasher.add(key, value);
            }
            assert_eq!(hasher.keys(), u64::from(leaf.keys));
            assert_eq!(hasher.bytes(), leaf.bytes);
            assert_eq!(hasher.finish(), leaf.digest, "leaf {index}");
        }
        let total: u64 = leaves.iter().map(|leaf| u64::from(leaf.keys)).sum();
        assert_eq!(total, 1000, "every pair in exactly one leaf");
        // An empty store is one empty leaf.
        let empty = plan(&[], leaf_bytes, 4).unwrap();
        assert_eq!(empty.len(), 1);
        assert_eq!((empty[0].keys, empty[0].bytes), (0, 0));
        assert_eq!(empty[0].digest, empty_sha256());
        // The cap: more leaves than allowed is refused, naming --leaf-bytes.
        let error = plan(&pairs, leaf_bytes, 3).unwrap_err().to_string();
        assert!(error.contains("--leaf-bytes"), "{error}");
        // Out of order, or outside the replicated keyspace.
        let mut planner = LeafPlanner::new(leaf_bytes, 16);
        planner.add(b"b", b"1").unwrap();
        assert!(planner.add(b"a", b"1").is_err());
        assert!(LeafPlanner::new(leaf_bytes, 16).add(&[0xff; 64], b"").is_err());
        assert!(LeafPlanner::new(leaf_bytes, 16).add(&[0xff; 65], b"").is_err());
    }

    /// A key too long to name a boundary on the wire never becomes one: the
    /// leaf grows past it.
    #[test]
    fn a_key_too_long_for_a_boundary_extends_its_leaf() {
        let long = vec![b'k'; MAX_LEAF_START_BYTES + 1];
        let mut planner = LeafPlanner::new(100, 16);
        planner.add(b"a", &[0; 200]).unwrap();
        planner.add(&long, b"v").unwrap();
        planner.add(b"z", b"v").unwrap();
        let leaves = planner.finish().unwrap();
        assert_eq!(leaves.len(), 2);
        assert_eq!(leaves[1].start, b"z".to_vec());
        assert_eq!(leaves[0].keys, 2);
    }

    /// Spec P4.1 §5: a store's digests against another store's boundaries —
    /// keys between boundaries, past the last one, leaves with no key here —
    /// match the other store's exactly where the content is the same.
    #[test]
    fn local_digests_follow_foreign_boundaries() {
        let primary = pairs(400, 50);
        let leaves = plan(&primary, 2048, 1 << 24).unwrap();
        let starts: Vec<Vec<u8>> = leaves.iter().map(|leaf| leaf.start.clone()).collect();
        let theirs: Vec<[u8; 32]> = leaves.iter().map(|leaf| leaf.digest).collect();
        assert_eq!(local(&primary, &starts), theirs, "the same content digests alike");
        // No key at all: every leaf is empty.
        assert!(local(&[], &starts).iter().all(|digest| *digest == empty_sha256()));
        // Keys only in the last leaf (and past its start): the others empty.
        let tail: Vec<(Vec<u8>, Vec<u8>)> = vec![(b"zzz".to_vec(), b"v".to_vec())];
        let digests = local(&tail, &starts);
        assert_eq!(digests.len(), starts.len());
        assert!(digests[..starts.len() - 1]
            .iter()
            .all(|digest| *digest == empty_sha256()));
        assert_ne!(digests[starts.len() - 1], empty_sha256());
        // A key before every non-empty boundary lands in leaf 0.
        let head: Vec<(Vec<u8>, Vec<u8>)> = vec![(b"a".to_vec(), b"v".to_vec())];
        let digests = local(&head, &starts);
        assert_ne!(digests[0], empty_sha256());
        assert!(digests[1..].iter().all(|digest| *digest == empty_sha256()));
        // Boundaries must start at the empty key; keys must stay in the keyspace.
        assert!(LocalDigests::new(&[b"a".to_vec()]).is_err());
        assert!(LocalDigests::new(&[]).is_err());
        let mut digests = LocalDigests::new(&starts).unwrap();
        assert!(digests.add(&[0xff; 64], b"").is_err());
    }

    /// Spec P4.1 §5: equal digests exactly when the content of a leaf is
    /// equal, over random edits — an update, an insert, a delete, a whole
    /// leaf deleted — each changing exactly the leaves it touches.
    #[test]
    fn digests_differ_exactly_in_the_edited_leaves() {
        let primary = pairs(600, 40);
        let leaves = plan(&primary, 1500, 1 << 24).unwrap();
        assert!(leaves.len() >= 8, "{} leaves", leaves.len());
        let starts: Vec<Vec<u8>> = leaves.iter().map(|leaf| leaf.start.clone()).collect();
        let theirs: Vec<[u8; 32]> = leaves.iter().map(|leaf| leaf.digest).collect();
        let leaf_of = |key: &[u8]| starts.iter().rposition(|start| key >= start.as_slice()).unwrap();
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = move |bound: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % bound as u64) as usize
        };
        for round in 0..40 {
            let mut standby = primary.clone();
            let mut touched = std::collections::BTreeSet::new();
            match round % 4 {
                0 => {
                    let at = next(standby.len());
                    standby[at].1 = b"updated".to_vec();
                    touched.insert(leaf_of(&standby[at].0));
                }
                1 => {
                    let at = next(standby.len());
                    let mut inserted = standby[at].0.clone();
                    inserted.push(b'x');
                    touched.insert(leaf_of(&inserted));
                    standby.insert(at + 1, (inserted, b"new".to_vec()));
                }
                2 => {
                    let at = next(standby.len());
                    touched.insert(leaf_of(&standby[at].0));
                    standby.remove(at);
                }
                _ => {
                    let leaf = next(starts.len());
                    standby.retain(|(key, _)| leaf_of(key) != leaf);
                    touched.insert(leaf);
                }
            }
            let ours = local(&standby, &starts);
            for (index, (mine, primary)) in ours.iter().zip(&theirs).enumerate() {
                assert_eq!(mine != primary, touched.contains(&index), "round {round}: leaf {index}");
            }
        }
    }

    /// The local pass over a real store reads in total order and agrees with
    /// `range_digest` over the whole family when there is one leaf.
    #[test]
    fn the_local_pass_over_a_store_agrees_with_the_range_digest() {
        let root = tempfile::tempdir().unwrap();
        let mut options = Options::default();
        options.create_if_missing(true);
        options.set_prefix_extractor(rocksdb::SliceTransform::create_fixed_prefix(5));
        let db = DB::open(&options, root.path()).unwrap();
        for (key, value) in pairs(200, 30) {
            db.put(key, value).unwrap();
        }
        let digests = local_leaf_digests(&db, &[Vec::new()]).unwrap();
        assert_eq!(digests, vec![range_digest(&db, None, b"").unwrap()]);
    }
}
