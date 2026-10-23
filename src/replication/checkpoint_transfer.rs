//! Physical checkpoint transfer into an owned, fresh, NON-SERVING dataset.
//! Export ownership retains snapshot files, never WAL history. Poll continuity
//! and retention must be checked separately. Local parent directories must be
//! owned and not group/world writable; hostile same-user renames are out of scope.
//! Every RocksDB open of a receiver's directory uses the engine's options
//! (`StorageEngine::build_db_options`), so the stage and a standby serving it
//! run with the primary's prefix extractor, filters, cache and compression.
//!
//! # Resumable download (spec P2.2)
//!
//! A download stages into one private sibling of its target,
//! `.nano-resync-<hash of the target's name>`, and keeps its progress there in
//! [`PHYSICAL_PARTIAL_FILE`]: the snapshot, the manifest's digest, and per
//! file the bytes that are fsynced. The record is rewritten (temporary file,
//! rename, fsync) after every completed file and after at most
//! [`PARTIAL_RECORD_INTERVAL`] bytes inside a file. A later download for the
//! same target adopts that staging directory ONLY through a verified record
//! ([`ReceiverStaging::begin_or_resume`]): the same snapshot and manifest
//! digest, a strict file census, every complete file re-hashed (a mismatch is
//! downloaded again), the file in progress truncated to its recorded length.
//! Anything else deletes the staging directory — one this receiver created,
//! never anything else — and starts over, saying why. The record is removed
//! before the completed download is verified and opened, so an installed
//! standby carries no extra file; installation is unchanged.

use super::fast_forward::FastForwardPlan;
use super::physical::{
    decode_cursor, fast_forward_record, Durability, FastForwardRecord, LocalWriter, PhysicalBatch, PhysicalCursor,
    PhysicalError, StagedPhysicalReplica, CURSOR_KEY, FAST_FORWARD_KEY, LOCAL_CF,
};
use crate::{storage::StorageEngine, Config};
use parking_lot::Mutex;
use rocksdb::{checkpoint::Checkpoint, ColumnFamilyDescriptor, Options, DB};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, File, Metadata, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    sync::Arc,
};
use uuid::Uuid;

pub const PHYSICAL_MARKER_FILE: &str = "NANO-PHYSICAL-REPLICA.json";
pub const PHYSICAL_MANIFEST_FILE: &str = "NANO-PHYSICAL-MANIFEST.json";
/// Spec P4.1 §4: the marker's and the manifest's temporary files while a
/// fast-forward re-cuts the stage ([`recut_stage`]). A crash can leave one
/// behind; the re-run removes it.
pub const PHYSICAL_MARKER_TEMP_FILE: &str = "NANO-PHYSICAL-REPLICA.json.tmp";
pub const PHYSICAL_MANIFEST_TEMP_FILE: &str = "NANO-PHYSICAL-MANIFEST.json.tmp";
/// Spec P2.2: a download's progress record, in its staging directory only.
pub const PHYSICAL_PARTIAL_FILE: &str = "NANO-PHYSICAL-PARTIAL.json";
/// The record's temporary file while it is rewritten.
const PHYSICAL_PARTIAL_TEMP_FILE: &str = "NANO-PHYSICAL-PARTIAL.json.tmp";
/// Inside one file, the progress record is rewritten after at most this many
/// newly received bytes (and always when the file completes): the most a
/// resumed download fetches again for the file it was killed in.
pub const PARTIAL_RECORD_INTERVAL: u64 = 8 * 1024 * 1024;
/// A download's staging directory is this prefix and a hash of its target's name.
pub const STAGING_PREFIX: &str = ".nano-resync-";
const STAGING_MODE: &str = "physical-staging-not-serving";
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_PARTIAL_BYTES: usize = 2 * MAX_MANIFEST_BYTES;
pub const MAX_CHUNK_BYTES: usize = 1024 * 1024;
const MAX_FILES: usize = 2048;
const MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
const MAX_SEQUENCE: u64 = (1_u64 << 56) - 1;
const PRIMARY_MARKER: &str = "NANO-PRIMARY-HISTORY.json";
const STANDBY_MARKER: &str = "NANO-STANDBY-BINDING.json";

#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    #[error("physical checkpoint I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("physical checkpoint storage: {0}")]
    Storage(#[from] rocksdb::Error),
    #[error("physical checkpoint continuation: {0}")]
    Physical(#[from] PhysicalError),
    #[error("physical checkpoint refused: {0}")]
    Invalid(String),
}
pub type Result<T> = std::result::Result<T, TransferError>;
fn invalid(message: impl Into<String>) -> TransferError {
    TransferError::Invalid(message.into())
}

#[derive(Clone, Copy, Debug)]
pub struct TransferLimits {
    pub max_files: usize,
    pub max_total_bytes: u64,
    pub max_file_bytes: u64,
    pub max_chunk_bytes: usize,
}
impl Default for TransferLimits {
    fn default() -> Self {
        Self {
            max_files: MAX_FILES,
            max_total_bytes: 64 * 1024 * 1024 * 1024,
            max_file_bytes: 32 * 1024 * 1024 * 1024,
            max_chunk_bytes: MAX_CHUNK_BYTES,
        }
    }
}
impl TransferLimits {
    fn validate(&self) -> Result<()> {
        if self.max_files == 0
            || self.max_files > MAX_FILES
            || self.max_total_bytes == 0
            || self.max_total_bytes > MAX_TOTAL_BYTES
            || self.max_file_bytes == 0
            || self.max_file_bytes > self.max_total_bytes
            || self.max_chunk_bytes == 0
            || self.max_chunk_bytes > MAX_CHUNK_BYTES
        {
            return Err(invalid("invalid transfer resource limits"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointFile {
    pub name: String,
    pub size: u64,
    pub sha256: [u8; 32],
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointManifest {
    pub format: u32,
    pub snapshot_id: Uuid,
    pub history: Uuid,
    /// The source sequence the checkpoint holds, in the PRIMARY's numbering:
    /// where its receiver's cursor starts.
    pub checkpoint_sequence: u64,
    pub storage_identity: String,
    pub files: Vec<CheckpointFile>,
    pub total_bytes: u64,
    /// Spec P3.2 §4: the checkpoint store's own RocksDB sequence when it
    /// differs from `checkpoint_sequence` — a CASCADED export, taken from a
    /// physical standby, whose sequence counts that standby's own writes
    /// (ahead of the source sequence; behind it after a fast-forward, spec
    /// P4.1). Absent (and not serialized, so a primary's manifest is
    /// unchanged byte for byte) when the two are equal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_sequence: Option<u64>,
}
impl CheckpointManifest {
    /// The store sequence a downloaded copy must open at
    /// ([`Self::store_sequence`] when set, else `checkpoint_sequence`).
    pub fn opened_sequence(&self) -> u64 {
        self.store_sequence.unwrap_or(self.checkpoint_sequence)
    }

    pub fn validate(&self, limits: &TransferLimits) -> Result<()> {
        limits.validate()?;
        if self.format != 1
            || self.snapshot_id.is_nil()
            || self.history.is_nil()
            || self.checkpoint_sequence > MAX_SEQUENCE
            // Set only when the two differ: a cascaded store's counter runs
            // ahead of the source sequence it holds, or behind it once that
            // standby was fast-forwarded (spec P4.1 §4).
            || self
                .store_sequence
                .is_some_and(|store| store == self.checkpoint_sequence || store > MAX_SEQUENCE)
            || self.files.is_empty()
            || self.files.len() > limits.max_files
            || !valid_identity(&self.storage_identity)
        {
            return Err(invalid("invalid checkpoint manifest identity or limits"));
        }
        let mut total = 0_u64;
        let mut previous: Option<&str> = None;
        for file in &self.files {
            if !valid_file_name(&file.name)
                || previous.is_some_and(|name| name >= file.name.as_str())
                || file.size > limits.max_file_bytes
            {
                return Err(invalid("invalid, duplicate, unordered, or oversized checkpoint file"));
            }
            if (file.name == "IDENTITY" && (file.size == 0 || file.size > 128))
                || (file.name == "CURRENT" && (file.size == 0 || file.size > 256))
            {
                return Err(invalid("invalid checkpoint control-file size"));
            }
            previous = Some(&file.name);
            total = total
                .checked_add(file.size)
                .ok_or_else(|| invalid("manifest size overflow"))?;
        }
        if total != self.total_bytes
            || total > limits.max_total_bytes
            || !self.files.iter().any(|file| file.name == "CURRENT")
            || !self.files.iter().any(|file| file.name == "IDENTITY")
            || !self.files.iter().any(|file| file.name.starts_with("MANIFEST-"))
        {
            return Err(invalid("manifest total or required RocksDB files invalid"));
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8], limits: &TransferLimits) -> Result<Self> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(invalid("manifest byte limit"));
        }
        let manifest: Self =
            serde_json::from_slice(bytes).map_err(|error| invalid(format!("manifest JSON: {error}")))?;
        manifest.validate(limits)?;
        Ok(manifest)
    }

    fn encoded(&self) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(self).map_err(|error| invalid(error.to_string()))?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(invalid("manifest byte limit"));
        }
        Ok(bytes)
    }
}

fn digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}
fn valid_file_name(name: &str) -> bool {
    if name.len() > 128 || !name.is_ascii() {
        return false;
    }
    if matches!(name, "CURRENT" | "IDENTITY") {
        return true;
    }
    if let Some(number) = name.strip_prefix("MANIFEST-").or_else(|| name.strip_prefix("OPTIONS-")) {
        return digits(number);
    }
    name.rsplit_once('.')
        .is_some_and(|(number, extension)| digits(number) && matches!(extension, "sst" | "ldb" | "log" | "blob"))
}
fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && value.bytes().all(|byte| byte.is_ascii_graphic())
}

#[derive(Debug)]
struct OwnedDirectory {
    path: PathBuf,
    metadata: Metadata,
}
impl OwnedDirectory {
    fn capture(path: &Path) -> Result<Self> {
        let path = absolute_path(path)?;
        let metadata = fs::symlink_metadata(&path)?;
        check_owned_directory(&metadata)?;
        Ok(Self { path, metadata })
    }
    fn verify(&self) -> Result<()> {
        let current = fs::symlink_metadata(absolute_path(&self.path)?)?;
        check_owned_directory(&current)?;
        if !same_file(&self.metadata, &current) {
            return Err(invalid("owned directory was replaced"));
        }
        Ok(())
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() || path.components().any(|part| matches!(part, Component::ParentDir)) {
        return Err(invalid("empty paths and parent traversal are refused"));
    }
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut current = PathBuf::new();
    for part in path.components() {
        current.push(part.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Err(invalid("symlink path refused")),
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path)
}
fn check_owned_directory(metadata: &Metadata) -> Result<()> {
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(invalid("not a directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
            return Err(invalid("directory must be owned and not group/world writable"));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        Err(invalid("physical transfer requires Unix file identity semantics"))
    }
}
fn same_file(first: &Metadata, second: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        first.dev() == second.dev() && first.ino() == second.ino()
    }
    #[cfg(not(unix))]
    {
        let _ = (first, second);
        false
    }
}
fn valid_regular(metadata: &Metadata, max_bytes: u64, single_link: bool) -> Result<()> {
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > max_bytes {
        return Err(invalid("nonregular, symlink or oversized file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if single_link && metadata.nlink() != 1 {
            return Err(invalid("hard-linked receiver metadata/file refused"));
        }
    }
    Ok(())
}
fn open_regular(path: &Path, max_bytes: u64, single_link: bool, write: bool) -> Result<File> {
    let before = fs::symlink_metadata(path)?;
    valid_regular(&before, max_bytes, single_link)?;
    let mut options = OpenOptions::new();
    options.read(true).write(write);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let opened = file.metadata()?;
    valid_regular(&opened, max_bytes, single_link)?;
    let after = fs::symlink_metadata(path)?;
    if after.file_type().is_symlink() || !same_file(&before, &opened) || !same_file(&opened, &after) {
        return Err(invalid("file replaced while opening"));
    }
    Ok(file)
}
fn read_small(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    open_regular(path, limit as u64, true, false)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(invalid("metadata grew beyond limit"));
    }
    Ok(bytes)
}
fn create_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}
/// The engine's options and column-family descriptors for `path`; see
/// `StorageEngine::build_db_options`. Never creates a store.
///
/// One exception to the engine's options: a receiver retains no archived WAL.
/// Its own records carry node-local writes and are refused as ordinary source
/// batches; a cascading standby (spec P3.2 §4) serves its LIVE WAL, stripped
/// (`physical::poll_source_cascade`), and what that no longer holds from its
/// own WAL archive (`[replication.archive]`). With a retention window RocksDB
/// would move obsolete logs into an `archive/` directory, which the stage
/// file census refuses. Everything that decides how data is laid out and read
/// (compression, table format, filters, prefix extractor, memtables) is the
/// engine's. The fast-forward's writer (`physical::FastForwardWriter`, spec
/// P4.1) opens the stage with these options too.
pub(crate) fn receiver_options(config: &Config, path: &Path) -> Result<(Options, Vec<ColumnFamilyDescriptor>)> {
    let (mut options, families) = StorageEngine::build_db_options(config, path)
        .map_err(|error| invalid(format!("receiver storage options: {error}")))?;
    options.set_wal_ttl_seconds(0);
    options.set_wal_size_limit_mb(0);
    options.create_if_missing(false);
    Ok((options, families))
}
fn identity(path: &Path) -> Result<String> {
    let bytes = read_small(&path.join("IDENTITY"), 128)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| invalid("IDENTITY is not UTF-8"))?
        .trim_end_matches(['\r', '\n']);
    if !valid_identity(text) {
        return Err(invalid("invalid RocksDB IDENTITY"));
    }
    Ok(text.to_string())
}
fn absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(invalid(format!("path already exists: {}", path.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn refuse_database_ancestors(path: &Path) -> Result<()> {
    for directory in path.ancestors() {
        for name in ["CURRENT", PRIMARY_MARKER, STANDBY_MARKER, PHYSICAL_MARKER_FILE] {
            match fs::symlink_metadata(directory.join(name)) {
                Ok(_) => return Err(invalid("receiver destination is nested inside a database")),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrimaryMarker {
    format: u32,
    history_id: Uuid,
    storage_identity: String,
}
fn source_identity(db: &DB, expected_history: Uuid) -> Result<(OwnedDirectory, String)> {
    if expected_history.is_nil() {
        return Err(invalid("nil expected source history"));
    }
    let directory = OwnedDirectory::capture(db.path())?;
    absent(&directory.path.join(STANDBY_MARKER))?;
    absent(&directory.path.join(PHYSICAL_MARKER_FILE))?;
    let record: PrimaryMarker = serde_json::from_slice(&read_small(&directory.path.join(PRIMARY_MARKER), 4096)?)
        .map_err(|error| invalid(format!("primary history metadata: {error}")))?;
    let storage_identity = identity(&directory.path)?;
    if record.format != 1 || record.history_id != expected_history || record.storage_identity != storage_identity {
        return Err(invalid("source primary marker/history/storage identity mismatch"));
    }
    directory.verify()?;
    Ok((directory, storage_identity))
}

/// Revalidate an already initialized primary; never creates or adopts metadata.
/// The caller holds the live RocksDB handle and uses the returned storage identity
/// to bind a physical Snapshot/Resume session across every subsequent poll.
pub fn validate_primary_source(db: &DB, expected_history: Uuid) -> Result<String> {
    source_identity(db, expected_history).map(|(_, identity)| identity)
}

/// Spec P3.2 §4: a cascading source is an installed physical standby — its
/// directory carries the physical marker of `expected_history` and no
/// primary or logical-standby marker — never a primary's directory.
fn cascade_source_identity(db: &DB, expected_history: Uuid) -> Result<(OwnedDirectory, String)> {
    if expected_history.is_nil() {
        return Err(invalid("nil expected source history"));
    }
    let directory = OwnedDirectory::capture(db.path())?;
    absent(&directory.path.join(PRIMARY_MARKER))?;
    absent(&directory.path.join(STANDBY_MARKER))?;
    if physical_stage_history(&directory.path)? != expected_history {
        return Err(invalid(
            "the cascading standby's physical marker names another dataset history",
        ));
    }
    let storage_identity = identity(&directory.path)?;
    directory.verify()?;
    Ok((directory, storage_identity))
}

/// [`validate_primary_source`] for a cascading source (spec P3.2 §4): a
/// physical standby following `expected_history`. Returns its storage
/// identity (its primary's: a standby's IDENTITY is a copy).
pub fn validate_cascade_source(db: &DB, expected_history: Uuid) -> Result<String> {
    cascade_source_identity(db, expected_history).map(|(_, identity)| identity)
}

/// Spec P3.2 §4: make the checkpoint copy at `path` of a physical standby's
/// store the primary's. The cursor in its node-local family names the
/// source sequence the copy is at (it is written in the same write group as
/// the data it accounts for, so the copy's last write is its cursor's); the
/// family is then dropped from the copy, which leaves the default column
/// family — the primary's at that source sequence — alone. Returns that
/// source sequence and the copy's own RocksDB sequence (`None` when equal),
/// proven on a read-only reopen. Opened with `config`'s storage options, so
/// whatever the open flushes is written as the engine writes it.
fn strip_node_local_family(path: &Path, config: &Config, expected_history: Uuid) -> Result<(u64, Option<u64>)> {
    let (options, families) = receiver_options(config, path)?;
    let mut copy = DB::open_cf_descriptors(&options, path, families)?;
    let cursor = {
        let local = copy
            .cf_handle(LOCAL_CF)
            .ok_or_else(|| invalid("the standby's checkpoint has no node-local column family"))?;
        // Spec P4.1 §4: a store a fast-forward left between two cuts is no
        // one cut of the primary's: never served.
        if let Some(bytes) = copy.get_cf(local, FAST_FORWARD_KEY)? {
            let record = FastForwardRecord::decode(&bytes)?;
            return Err(PhysicalError::FastForwardInterrupted {
                from: record.from,
                to: record.to,
            }
            .into());
        }
        let bytes = copy
            .get_cf(local, CURSOR_KEY)?
            .ok_or_else(|| invalid("the standby's checkpoint holds no receiver cursor"))?;
        decode_cursor(&bytes)?
    };
    if cursor.history() != expected_history {
        return Err(invalid(
            "the standby's checkpoint cursor follows another dataset history",
        ));
    }
    let store = copy.latest_sequence_number();
    if cursor.local_sequence() != store {
        return Err(invalid(
            "the standby's store was written outside its physical writer: its checkpoint cannot be served",
        ));
    }
    copy.drop_cf(LOCAL_CF)?;
    drop(copy);
    if DB::list_cf(&Options::default(), path)?
        .iter()
        .any(|family| family == LOCAL_CF)
    {
        return Err(invalid(
            "the node-local column family survived its drop from the checkpoint copy",
        ));
    }
    let reopened = DB::open_for_read_only(&Options::default(), path, false)?;
    let reopened_sequence = reopened.latest_sequence_number();
    drop(reopened);
    if reopened_sequence != store {
        return Err(invalid(
            "the checkpoint copy's sequence moved while its node-local family was dropped",
        ));
    }
    let source = cursor.source_sequence();
    Ok((source, (store != source).then_some(store)))
}

fn fingerprint_file(path: &Path, expected_size: u64, single_link: bool) -> Result<([u8; 32], Metadata)> {
    let mut file = open_regular(path, expected_size, single_link, false)?;
    let before = file.metadata()?;
    if before.len() != expected_size {
        return Err(invalid("file size mismatch"));
    }
    let mut hash = Sha256::new();
    let mut copied = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        copied = copied
            .checked_add(count as u64)
            .ok_or_else(|| invalid("file size overflow"))?;
        if copied > expected_size {
            return Err(invalid("file grew while hashing"));
        }
        let chunk = buffer
            .get(..count)
            .ok_or_else(|| invalid("read count exceeds buffer"))?;
        hash.update(chunk);
    }
    let after = fs::symlink_metadata(path)?;
    if copied != expected_size
        || !same_file(&before, &after)
        || after.len() != expected_size
        || before.modified()? != after.modified()?
    {
        return Err(invalid("file changed while hashing"));
    }
    Ok((hash.finalize().into(), after))
}

/// Snapshot-file lease only. Dropping it removes its own private TempDir;
/// archived WAL retention is not extended by this value.
///
/// Spec P4.1 §3.2: an export also serves incremental fast-forwards from its
/// checkpoint, through one leaf plan at a time ([`Self::fast_forward_plan`]),
/// whose leaf files live inside the lease directory and go with it.
pub struct OnlineCheckpointExport {
    lease: tempfile::TempDir,
    directory: OwnedDirectory,
    manifest: CheckpointManifest,
    fingerprints: Vec<Metadata>,
    limits: TransferLimits,
    /// The leaf plan of the last fast-forward session, keyed by its leaf size.
    fast_forward: Mutex<Option<Arc<FastForwardPlan>>>,
}
impl OnlineCheckpointExport {
    /// `db` must be the exclusively opened live primary. Bind caller history to
    /// its actual, pre-existing primary marker and RocksDB IDENTITY first.
    pub fn create(db: &DB, expected_history: Uuid, export_parent: &Path, limits: TransferLimits) -> Result<Self> {
        limits.validate()?;
        let (source, storage_identity) = source_identity(db, expected_history)?;
        Self::export(
            db,
            &source,
            storage_identity,
            expected_history,
            export_parent,
            limits,
            None,
            || source_identity(db, expected_history).map(|(_, identity)| identity),
        )
    }

    /// Spec P3.2 §4: the export of a CASCADING source — a physical standby
    /// (`[replication.physical] cascade = true`) serving a downstream
    /// standby. `db` is the standby's live store (its writer's handle), bound
    /// to `expected_history` by its physical marker and IDENTITY.
    ///
    /// The checkpoint copy is made the primary's: the cursor its node-local
    /// family holds names the source sequence the copy is at, then the family
    /// is dropped from the COPY (`DB::drop_cf`), so what is shipped is the
    /// primary's default column family alone — byte for byte the primary's at
    /// that source sequence — and the downstream creates a family of its own.
    /// The manifest's `checkpoint_sequence` is that source sequence (the
    /// primary's numbering, which the downstream continues in) and its
    /// `store_sequence` the copy's own RocksDB sequence, which runs ahead of
    /// it by this standby's cursor writes. `config` supplies the storage
    /// options the copy is opened with to drop the family.
    pub fn create_cascaded(
        db: &DB,
        expected_history: Uuid,
        export_parent: &Path,
        limits: TransferLimits,
        config: &Config,
    ) -> Result<Self> {
        limits.validate()?;
        let (source, storage_identity) = cascade_source_identity(db, expected_history)?;
        Self::export(
            db,
            &source,
            storage_identity,
            expected_history,
            export_parent,
            limits,
            Some(config),
            || cascade_source_identity(db, expected_history).map(|(_, identity)| identity),
        )
    }

    /// The export of [`Self::create`] and [`Self::create_cascaded`]:
    /// checkpoint, IDENTITY, the cut (made the primary's for a cascaded
    /// export), the census and the manifest. `revalidate` re-reads the
    /// source's identity after the copy was made.
    #[allow(clippy::too_many_arguments)]
    fn export(
        db: &DB,
        source: &OwnedDirectory,
        storage_identity: String,
        expected_history: Uuid,
        export_parent: &Path,
        limits: TransferLimits,
        cascade: Option<&Config>,
        revalidate: impl Fn() -> Result<String>,
    ) -> Result<Self> {
        let parent = OwnedDirectory::capture(export_parent)?;
        if fs::canonicalize(&parent.path)?.starts_with(fs::canonicalize(&source.path)?) {
            return Err(invalid("checkpoint export parent is inside source DB"));
        }
        let lease = tempfile::Builder::new()
            .prefix("nano-checkpoint-")
            .tempdir_in(&parent.path)?;
        let path = lease.path().join("checkpoint");
        Checkpoint::new(db)?.create_checkpoint(&path)?;
        // The pinned Checkpoint API omits external IDENTITY and Nano markers.
        // Written before any writable open of the copy: RocksDB would invent
        // an IDENTITY of its own for a directory without one.
        match fs::symlink_metadata(path.join("IDENTITY")) {
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                create_file(&path.join("IDENTITY"), format!("{storage_identity}\n").as_bytes())?;
            }
            Err(error) => return Err(error.into()),
        }
        if identity(&path)? != storage_identity {
            return Err(invalid("checkpoint IDENTITY differs from source"));
        }
        let (cut, store_sequence) = match cascade {
            None => {
                let checkpoint = DB::open_for_read_only(&Options::default(), &path, false)?;
                let cut = checkpoint.latest_sequence_number();
                drop(checkpoint);
                (cut, None)
            }
            Some(config) => strip_node_local_family(&path, config, expected_history)?,
        };
        if identity(&path)? != storage_identity {
            return Err(invalid("checkpoint IDENTITY changed while the copy was opened"));
        }
        let directory = OwnedDirectory::capture(&path)?;
        let mut files = Vec::new();
        let mut total_bytes = 0_u64;
        for entry in fs::read_dir(&path)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| invalid("non-UTF8 checkpoint file"))?;
            // A raw read-only open may create its own diagnostic log. Neither
            // diagnostics nor lock files are transfer state.
            if name == "LOCK" || name == "LOG" || name.starts_with("LOG.old.") {
                continue;
            }
            if !valid_file_name(&name) {
                return Err(invalid(format!("unexpected checkpoint file {name}")));
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            valid_regular(&metadata, limits.max_file_bytes, false)?;
            if files.len() >= limits.max_files {
                return Err(invalid("checkpoint file count exceeds transfer limit"));
            }
            total_bytes = total_bytes
                .checked_add(metadata.len())
                .ok_or_else(|| invalid("checkpoint size overflow"))?;
            if total_bytes > limits.max_total_bytes {
                return Err(invalid("checkpoint total exceeds transfer limit"));
            }
            files.push(CheckpointFile {
                name,
                size: metadata.len(),
                sha256: [0; 32],
            });
        }
        files.sort_by(|left, right| left.name.cmp(&right.name));
        let mut manifest = CheckpointManifest {
            format: 1,
            snapshot_id: Uuid::new_v4(),
            history: expected_history,
            checkpoint_sequence: cut,
            storage_identity,
            files,
            total_bytes,
            store_sequence,
        };
        manifest.validate(&limits)?;
        let mut fingerprints = Vec::with_capacity(manifest.files.len());
        for file in &mut manifest.files {
            let (hash, metadata) = fingerprint_file(&path.join(&file.name), file.size, false)?;
            file.sha256 = hash;
            fingerprints.push(metadata);
        }
        manifest.encoded()?;
        // History metadata must not have changed during snapshot creation.
        if revalidate()? != manifest.storage_identity {
            return Err(invalid("source identity changed"));
        }
        directory.verify()?;
        parent.verify()?;
        sync_dir(&path)?;
        Ok(Self {
            lease,
            directory,
            manifest,
            fingerprints,
            limits,
            fast_forward: Mutex::new(None),
        })
    }
    pub fn manifest(&self) -> &CheckpointManifest {
        &self.manifest
    }
    /// The checkpoint directory: what a fast-forward's leaves are read from.
    pub fn checkpoint_path(&self) -> &Path {
        &self.directory.path
    }
    /// Spec P4.1 §3.2: the export's leaf plan for `leaf_bytes`, created (not
    /// yet planned: [`FastForwardPlan::begin`]) on the first fast-forward
    /// session that asks for that size. A plan of another size, or one whose
    /// planning failed or was orphaned (its planning claim dropped without a
    /// plan: [`FastForwardPlan::is_failed`]), is replaced — returned second,
    /// so the caller drops it off the lock — and a session still holding it
    /// keeps it until it ends. Each plan writes its leaf files into a
    /// directory of its own, removed when its last reference drops, so a
    /// replaced plan's files never disappear under a session still reading
    /// them and never mix with its successor's. The leaf files are written
    /// with `config`'s storage options, the engine's.
    pub fn fast_forward_plan(
        &self,
        leaf_bytes: u32,
        config: &Config,
    ) -> (Arc<FastForwardPlan>, Option<Arc<FastForwardPlan>>) {
        let mut slot = self.fast_forward.lock();
        if let Some(plan) = slot
            .as_ref()
            .filter(|plan| plan.leaf_bytes() == leaf_bytes && !plan.is_failed())
        {
            return (Arc::clone(plan), None);
        }
        let plan = Arc::new(FastForwardPlan::new(
            leaf_bytes,
            self.directory.path.clone(),
            self.lease
                .path()
                .join(format!("leaves-{leaf_bytes}-{}", Uuid::new_v4().simple())),
            config.clone(),
        ));
        let replaced = slot.replace(Arc::clone(&plan));
        (plan, replaced)
    }
    pub fn manifest_bytes(&self) -> Result<Vec<u8>> {
        self.directory.verify()?;
        self.manifest.encoded()
    }
    pub fn read_chunk(&self, file_index: u32, offset: u64, length: u32) -> Result<Vec<u8>> {
        self.directory.verify()?;
        if length == 0 || length as usize > self.limits.max_chunk_bytes {
            return Err(invalid("chunk byte limit"));
        }
        let file = self
            .manifest
            .files
            .get(file_index as usize)
            .ok_or_else(|| invalid("unknown file index"))?;
        if offset > file.size {
            return Err(invalid("chunk offset exceeds file"));
        }
        let path = self.directory.path.join(&file.name);
        let mut input = open_regular(&path, file.size, false, false)?;
        let current = input.metadata()?;
        let expected = self
            .fingerprints
            .get(file_index as usize)
            .ok_or_else(|| invalid("unknown file fingerprint"))?;
        if !same_file(expected, &current) || current.len() != file.size || current.modified()? != expected.modified()? {
            return Err(invalid("export file changed after manifest"));
        }
        let count = u64::from(length).min(file.size - offset) as usize;
        let mut bytes = vec![0; count];
        input.seek(SeekFrom::Start(offset))?;
        input.read_exact(&mut bytes)?;
        let after = fs::symlink_metadata(path)?;
        if !same_file(&current, &after) || current.modified()? != after.modified()? || after.len() != file.size {
            return Err(invalid("export file changed during chunk read"));
        }
        Ok(bytes)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PhysicalMarker {
    format: u32,
    mode: String,
    snapshot_id: Uuid,
    history: Uuid,
    checkpoint_sequence: u64,
    storage_identity: String,
    manifest_sha256: [u8; 32],
}

/// The dataset history an installed physical standby directory follows, read
/// from its marker: what a restarted standby resumes with (spec P1.3). Only a
/// first answer — [`ReopenedPhysicalStage::open`] proves it against the
/// marker's manifest and the durable cursor, and the primary's history is
/// checked again at every connection.
pub fn physical_stage_history(path: &Path) -> Result<Uuid> {
    let marker: PhysicalMarker = serde_json::from_slice(&read_small(&path.join(PHYSICAL_MARKER_FILE), 4096)?)
        .map_err(|error| invalid(format!("physical stage marker: {error}")))?;
    if marker.format != 1 || marker.history.is_nil() {
        return Err(invalid("physical stage marker has an unknown format or a nil history"));
    }
    Ok(marker.history)
}

struct StageOwner {
    directory: OwnedDirectory,
    parent: OwnedDirectory,
    target: PathBuf,
    manifest: CheckpointManifest,
    marker: Vec<u8>,
}
impl StageOwner {
    fn verify(&self) -> Result<()> {
        self.parent.verify()?;
        self.directory.verify()?;
        if read_small(&self.directory.path.join(PHYSICAL_MARKER_FILE), 4096)? != self.marker {
            return Err(invalid("physical stage marker changed"));
        }
        if read_small(&self.directory.path.join(PHYSICAL_MANIFEST_FILE), MAX_MANIFEST_BYTES)?
            != self.manifest.encoded()?
        {
            return Err(invalid("saved manifest changed"));
        }
        Ok(())
    }
    fn verify_download(&self) -> Result<()> {
        self.verify()?;
        let mut expected: HashSet<&str> = self.manifest.files.iter().map(|file| file.name.as_str()).collect();
        expected.insert(PHYSICAL_MARKER_FILE);
        expected.insert(PHYSICAL_MANIFEST_FILE);
        for entry in fs::read_dir(&self.directory.path)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| invalid("non-UTF8 stage file"))?;
            if !expected.contains(name.as_str()) {
                return Err(invalid("unexpected file in download staging"));
            }
        }
        if read_small(&self.directory.path.join(PHYSICAL_MANIFEST_FILE), MAX_MANIFEST_BYTES)?
            != self.manifest.encoded()?
        {
            return Err(invalid("saved manifest changed"));
        }
        for file in &self.manifest.files {
            let (digest, _) = fingerprint_file(&self.directory.path.join(&file.name), file.size, true)?;
            if digest != file.sha256 {
                return Err(invalid(format!("checksum mismatch: {}", file.name)));
            }
        }
        if identity(&self.directory.path)? != self.manifest.storage_identity {
            return Err(invalid("download identity mismatch"));
        }
        let current = read_small(&self.directory.path.join("CURRENT"), 256)?;
        let current = std::str::from_utf8(&current)
            .map_err(|_| invalid("CURRENT is not UTF8"))?
            .trim_end_matches('\n');
        if !current.starts_with("MANIFEST-") || !self.manifest.files.iter().any(|file| file.name == current) {
            return Err(invalid("CURRENT does not reference a listed manifest"));
        }
        Ok(())
    }
}

/// One file of a [`PartialRecord`]: the manifest's entry and the bytes of it
/// that are fsynced in the staging directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialFile {
    pub name: String,
    pub size: u64,
    pub sha256: [u8; 32],
    pub bytes_done: u64,
}

/// Spec P2.2: a download's progress, [`PHYSICAL_PARTIAL_FILE`] in its staging
/// directory. It only ever claims bytes that were fsynced before it was
/// written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialRecord {
    pub format: u32,
    pub snapshot_id: Uuid,
    /// SHA-256 of the manifest's canonical encoding.
    pub manifest_sha256: [u8; 32],
    pub history: Uuid,
    pub files: Vec<PartialFile>,
}

impl PartialRecord {
    fn new(manifest: &CheckpointManifest, manifest_sha256: [u8; 32], done: &[u64]) -> Self {
        Self {
            format: 1,
            snapshot_id: manifest.snapshot_id,
            manifest_sha256,
            history: manifest.history,
            files: manifest
                .files
                .iter()
                .zip(done)
                .map(|(file, done)| PartialFile {
                    name: file.name.clone(),
                    size: file.size,
                    sha256: file.sha256,
                    bytes_done: *done,
                })
                .collect(),
        }
    }

    /// Why this record cannot continue a download of `manifest`; `None` when
    /// it can.
    fn disagreement(&self, manifest: &CheckpointManifest, manifest_sha256: &[u8; 32]) -> Option<String> {
        if self.format != 1 {
            return Some(format!("the partial download record has format {}", self.format));
        }
        if self.snapshot_id != manifest.snapshot_id {
            return Some(format!(
                "the primary now serves snapshot {}; the partial download belongs to snapshot {} (its export \
                 lease expired, or the primary restarted)",
                manifest.snapshot_id, self.snapshot_id
            ));
        }
        if &self.manifest_sha256 != manifest_sha256 || self.history != manifest.history {
            return Some(format!(
                "the manifest of snapshot {} differs from the one the partial download recorded",
                manifest.snapshot_id
            ));
        }
        let same_files = self.files.len() == manifest.files.len()
            && self.files.iter().zip(&manifest.files).all(|(recorded, file)| {
                recorded.name == file.name
                    && recorded.size == file.size
                    && recorded.sha256 == file.sha256
                    // `recorded.size == file.size` held above, so the bound
                    // is the manifest's size as well.
                    && recorded.bytes_done <= recorded.size
            });
        (!same_files).then(|| "the partial download record disagrees with the manifest's files".to_string())
    }
}

/// What [`ReceiverStaging::begin_or_resume`] did with the target's staging
/// directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StagingStart {
    /// There was none: a new download.
    Fresh,
    /// A verified partial download of the same snapshot continues: `held`
    /// bytes are kept; the complete files listed in `redownload` failed their
    /// digest and are fetched again.
    Resumed { held: u64, redownload: Vec<String> },
    /// The staging directory could not be adopted, was deleted, and the
    /// download starts over: `reason`.
    StartedOver { reason: String },
}

/// Why an existing staging directory is not adopted.
enum Adoption {
    /// It is this receiver's and may be deleted: start over.
    StartOver(String),
    /// It cannot be judged safely (not an owned directory): refused, kept.
    Refused(TransferError),
}

impl From<TransferError> for Adoption {
    fn from(error: TransferError) -> Self {
        Self::StartOver(error.to_string())
    }
}

impl From<std::io::Error> for Adoption {
    fn from(error: std::io::Error) -> Self {
        Self::StartOver(format!("the staging directory cannot be read: {error}"))
    }
}

/// The staging directory of a download into `target` (absolute): a fixed
/// sibling, so an interrupted download is found again.
fn staging_path_of(target: &Path) -> Result<PathBuf> {
    let parent = target.parent().ok_or_else(|| invalid("target has no parent"))?;
    let name = target.file_name().ok_or_else(|| invalid("target has no file name"))?;
    let digest = Sha256::digest(name.as_encoded_bytes());
    let hash = hex::encode(digest.get(..16).unwrap_or(&[]));
    Ok(parent.join(format!("{STAGING_PREFIX}{hash}")))
}

/// The staging directory a download into `target` uses (spec P2.2).
pub fn staging_path_for(target: &Path) -> Result<PathBuf> {
    staging_path_of(&absolute_path(target)?)
}

/// The snapshot an interrupted download into `target` belongs to, from its
/// progress record, if there is one. Reads only; [`ReceiverStaging::begin_or_resume`]
/// decides whether that download is adopted.
pub fn partial_snapshot(target: &Path) -> Option<Uuid> {
    let staging = staging_path_for(target).ok()?;
    let bytes = read_small(&staging.join(PHYSICAL_PARTIAL_FILE), MAX_PARTIAL_BYTES).ok()?;
    let record: PartialRecord = serde_json::from_slice(&bytes).ok()?;
    (record.format == 1 && !record.snapshot_id.is_nil()).then_some(record.snapshot_id)
}

fn staging_marker(manifest: &CheckpointManifest, encoded: &[u8]) -> Result<Vec<u8>> {
    serde_json::to_vec(&PhysicalMarker {
        format: 1,
        mode: STAGING_MODE.into(),
        snapshot_id: manifest.snapshot_id,
        history: manifest.history,
        checkpoint_sequence: manifest.checkpoint_sequence,
        storage_identity: manifest.storage_identity.clone(),
        manifest_sha256: Sha256::digest(encoded).into(),
    })
    .map_err(|error| invalid(error.to_string()))
}

/// Delete a staging directory this receiver created — empty, or carrying a
/// staging marker — and nothing else.
fn remove_staging(path: &Path) -> Result<()> {
    let directory = OwnedDirectory::capture(path)?;
    let empty = fs::read_dir(&directory.path)?.next().is_none();
    if !empty {
        let ours = read_small(&directory.path.join(PHYSICAL_MARKER_FILE), 4096)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<PhysicalMarker>(&bytes).ok())
            .is_some_and(|marker| marker.format == 1 && marker.mode == STAGING_MODE);
        if !ours {
            return Err(invalid(format!(
                "{} carries no staging marker of a physical receiver; it is not deleted: move it aside or choose \
                 another target",
                directory.path.display()
            )));
        }
    }
    fs::remove_dir_all(&directory.path)?;
    if let Some(parent) = directory.path.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

/// Write `record` atomically: temporary file, fsync, rename, fsync the directory.
fn write_partial_record(directory: &Path, record: &PartialRecord) -> Result<()> {
    let bytes = serde_json::to_vec(record).map_err(|error| invalid(error.to_string()))?;
    if bytes.len() > MAX_PARTIAL_BYTES {
        return Err(invalid("partial download record byte limit"));
    }
    replace_atomically(directory, PHYSICAL_PARTIAL_TEMP_FILE, PHYSICAL_PARTIAL_FILE, &bytes)
}

/// Replace `directory/name` with `bytes` atomically: a fresh `temporary`
/// (any left by a crash is removed first), fsynced, renamed over `name`, the
/// directory fsynced. A reader sees the old file or the new one, never a
/// part of either.
fn replace_atomically(directory: &Path, temporary: &str, name: &str, bytes: &[u8]) -> Result<()> {
    let temporary = directory.join(temporary);
    match fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    create_file(&temporary, bytes)?;
    fs::rename(&temporary, directory.join(name))?;
    sync_dir(directory)
}

/// Remove the progress record (and a temporary left by a crash).
fn remove_partial_record(directory: &Path) -> Result<()> {
    for name in [PHYSICAL_PARTIAL_FILE, PHYSICAL_PARTIAL_TEMP_FILE] {
        match fs::remove_file(directory.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    sync_dir(directory)
}

/// A download's private staging sibling. Failed/interrupted receivers retain
/// it, with its progress record, for the next download into the same target
/// ([`Self::begin_or_resume`]); nothing here removes, resets, or adopts any
/// other path.
pub struct ReceiverStaging {
    owner: StageOwner,
    received: Vec<u64>,
    /// Per file, the bytes the progress record claims (all fsynced).
    recorded: Vec<u64>,
    manifest_sha256: [u8; 32],
    limits: TransferLimits,
    poisoned: bool,
}
impl ReceiverStaging {
    /// A download into `target`, which must be absent: a resumed one when its
    /// staging directory holds a verified partial download of this manifest,
    /// else a new one ([`Self::begin_or_resume`], whose outcome is dropped).
    pub fn begin(target: &Path, manifest_bytes: &[u8], expected_history: Uuid, limits: TransferLimits) -> Result<Self> {
        Self::begin_or_resume(target, manifest_bytes, expected_history, limits).map(|(stage, _)| stage)
    }

    /// Spec P2.2. `target` must be absent. Its staging directory, if one
    /// exists, is adopted only through a verified progress record of the SAME
    /// snapshot and manifest: every file the record marks complete is hashed
    /// again (a mismatch is downloaded again), the file in progress is
    /// truncated to its recorded length. No record, another snapshot or
    /// manifest, an unreadable record, a file whose size disagrees, or an
    /// unexpected file: the staging directory is deleted and the download
    /// starts over ([`StagingStart::StartedOver`] says why). A path there that
    /// is not this receiver's staging directory is refused and kept.
    pub fn begin_or_resume(
        target: &Path,
        manifest_bytes: &[u8],
        expected_history: Uuid,
        limits: TransferLimits,
    ) -> Result<(Self, StagingStart)> {
        let manifest = CheckpointManifest::decode(manifest_bytes, &limits)?;
        if expected_history.is_nil() || manifest.history != expected_history {
            return Err(invalid("unexpected snapshot history"));
        }
        let target = absolute_path(target)?;
        let parent_path = target
            .parent()
            .ok_or_else(|| invalid("target has no parent"))?
            .to_path_buf();
        refuse_database_ancestors(&OwnedDirectory::capture(&parent_path)?.path)?;
        absent(&target)?;
        let path = staging_path_of(&target)?;
        let mut start = StagingStart::Fresh;
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
            Ok(_) => match Self::resume(&path, &parent_path, &target, &manifest, limits) {
                Ok((stage, held, redownload)) => return Ok((stage, StagingStart::Resumed { held, redownload })),
                Err(Adoption::Refused(error)) => return Err(error),
                Err(Adoption::StartOver(reason)) => {
                    remove_staging(&path)?;
                    start = StagingStart::StartedOver { reason };
                }
            },
        }
        let stage = Self::create(path, &parent_path, target, manifest, limits)?;
        Ok((stage, start))
    }

    fn create(
        path: PathBuf,
        parent_path: &Path,
        target: PathBuf,
        manifest: CheckpointManifest,
        limits: TransferLimits,
    ) -> Result<Self> {
        let parent = OwnedDirectory::capture(parent_path)?;
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path)?;
        let directory = OwnedDirectory::capture(&path)?;
        let encoded = manifest.encoded()?;
        let manifest_sha256: [u8; 32] = Sha256::digest(&encoded).into();
        let marker = staging_marker(&manifest, &encoded)?;
        create_file(&path.join(PHYSICAL_MARKER_FILE), &marker)?;
        create_file(&path.join(PHYSICAL_MANIFEST_FILE), &encoded)?;
        for file in &manifest.files {
            create_file(&path.join(&file.name), &[])?;
        }
        let received = vec![0; manifest.files.len()];
        write_partial_record(&path, &PartialRecord::new(&manifest, manifest_sha256, &received))?;
        sync_dir(&parent.path)?;
        Ok(Self {
            owner: StageOwner {
                directory,
                parent,
                target,
                manifest,
                marker,
            },
            recorded: received.clone(),
            received,
            manifest_sha256,
            limits,
            poisoned: false,
        })
    }

    /// Adopt the existing staging directory `path` for `manifest`, or say why
    /// not (see [`Self::begin_or_resume`]).
    fn resume(
        path: &Path,
        parent_path: &Path,
        target: &Path,
        manifest: &CheckpointManifest,
        limits: TransferLimits,
    ) -> std::result::Result<(Self, u64, Vec<String>), Adoption> {
        let directory = OwnedDirectory::capture(path).map_err(Adoption::Refused)?;
        let record_bytes = match read_small(&directory.path.join(PHYSICAL_PARTIAL_FILE), MAX_PARTIAL_BYTES) {
            Ok(bytes) => bytes,
            Err(TransferError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(Adoption::StartOver(
                    "the staging directory holds no partial download record (an older or completed attempt)".into(),
                ))
            }
            Err(error) => {
                return Err(Adoption::StartOver(format!(
                    "the partial download record is unreadable: {error}"
                )))
            }
        };
        let record: PartialRecord = serde_json::from_slice(&record_bytes)
            .map_err(|error| Adoption::StartOver(format!("the partial download record is unreadable: {error}")))?;
        let encoded = manifest.encoded()?;
        let manifest_sha256: [u8; 32] = Sha256::digest(&encoded).into();
        if let Some(reason) = record.disagreement(manifest, &manifest_sha256) {
            return Err(Adoption::StartOver(reason));
        }
        let marker = staging_marker(manifest, &encoded)?;
        if read_small(&directory.path.join(PHYSICAL_MARKER_FILE), 4096)
            .ok()
            .as_ref()
            != Some(&marker)
            || read_small(&directory.path.join(PHYSICAL_MANIFEST_FILE), MAX_MANIFEST_BYTES)
                .ok()
                .as_ref()
                != Some(&encoded)
        {
            return Err(Adoption::StartOver(
                "the staging marker or saved manifest differs from the partial download record".into(),
            ));
        }
        // Strict census: the manifest's files, the marker, the saved manifest
        // and the record (its temporary, left by a crash, is removed).
        let mut expected: HashSet<&str> = manifest.files.iter().map(|file| file.name.as_str()).collect();
        expected.extend([PHYSICAL_MARKER_FILE, PHYSICAL_MANIFEST_FILE, PHYSICAL_PARTIAL_FILE]);
        for entry in fs::read_dir(&directory.path)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| Adoption::StartOver("a staging file name is not UTF-8".into()))?;
            if name == PHYSICAL_PARTIAL_TEMP_FILE {
                fs::remove_file(entry.path())?;
                continue;
            }
            if !expected.contains(name.as_str()) {
                return Err(Adoption::StartOver(format!(
                    "unexpected file {name} in the staging directory"
                )));
            }
            valid_regular(&fs::symlink_metadata(entry.path())?, u64::MAX, true)?;
        }
        let mut done = Vec::with_capacity(manifest.files.len());
        let mut redownload = Vec::new();
        for (file, recorded) in manifest.files.iter().zip(&record.files) {
            let file_path = directory.path.join(&file.name);
            let on_disk = fs::symlink_metadata(&file_path)?.len();
            if on_disk < recorded.bytes_done || on_disk > file.size {
                return Err(Adoption::StartOver(format!(
                    "file sizes disagree: {} holds {on_disk} bytes, the record says {} of {}",
                    file.name, recorded.bytes_done, file.size
                )));
            }
            let keep = if recorded.bytes_done == file.size {
                let (digest, _) = fingerprint_file(&file_path, file.size, true)?;
                if digest == file.sha256 {
                    file.size
                } else {
                    redownload.push(file.name.clone());
                    0
                }
            } else {
                recorded.bytes_done
            };
            if keep != on_disk {
                let output = open_regular(&file_path, file.size, true, true)?;
                output.set_len(keep)?;
                output.sync_all()?;
            }
            done.push(keep);
        }
        let parent = OwnedDirectory::capture(parent_path).map_err(Adoption::Refused)?;
        let held: u64 = done.iter().sum();
        let stage = Self {
            owner: StageOwner {
                directory,
                parent,
                target: target.to_path_buf(),
                manifest: manifest.clone(),
                marker,
            },
            recorded: done.clone(),
            received: done,
            manifest_sha256,
            limits,
            poisoned: false,
        };
        stage.owner.verify()?;
        stage.write_partial()?;
        Ok((stage, held, redownload))
    }

    fn write_partial(&self) -> Result<()> {
        write_partial_record(
            &self.owner.directory.path,
            &PartialRecord::new(&self.owner.manifest, self.manifest_sha256, &self.recorded),
        )
    }

    pub fn staging_path(&self) -> &Path {
        &self.owner.directory.path
    }
    /// The snapshot this download belongs to.
    pub fn snapshot_id(&self) -> Uuid {
        self.owner.manifest.snapshot_id
    }
    /// Whether `manifest` is the one this download continues.
    pub fn holds(&self, manifest: &CheckpointManifest) -> bool {
        manifest.snapshot_id == self.owner.manifest.snapshot_id
            && manifest
                .encoded()
                .is_ok_and(|encoded| <[u8; 32]>::from(Sha256::digest(&encoded)) == self.manifest_sha256)
    }
    /// Per manifest file, the bytes received so far: where the download continues.
    pub fn progress(&self) -> &[u64] {
        &self.received
    }
    pub fn append_chunk(&mut self, file_index: u32, offset: u64, bytes: &[u8]) -> Result<()> {
        if self.poisoned {
            return Err(invalid("download already failed; preserve staging"));
        }
        let outcome = (|| {
            self.owner.verify()?;
            if bytes.is_empty() || bytes.len() > self.limits.max_chunk_bytes {
                return Err(invalid("chunk byte limit"));
            }
            let index = file_index as usize;
            let file = self
                .owner
                .manifest
                .files
                .get(index)
                .ok_or_else(|| invalid("unknown file index"))?;
            let end = offset
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| invalid("chunk offset overflow"))?;
            let received = *self
                .received
                .get(index)
                .ok_or_else(|| invalid("unknown file progress index"))?;
            if offset != received || end > file.size {
                return Err(invalid("chunk out of order or oversized"));
            }
            let name = file.name.clone();
            let size = file.size;
            let mut output = open_regular(&self.owner.directory.path.join(&name), size, true, true)?;
            if output.metadata()?.len() != offset {
                return Err(invalid("staging file changed between chunks"));
            }
            output.seek(SeekFrom::Start(offset))?;
            output.write_all(bytes)?;
            let slot = self
                .received
                .get_mut(index)
                .ok_or_else(|| invalid("unknown file progress index"))?;
            *slot = end;
            // Spec P2.2: record the progress after every completed file and
            // after at most `PARTIAL_RECORD_INTERVAL` bytes inside one, and
            // only bytes that are fsynced first.
            let recorded = self
                .recorded
                .get_mut(index)
                .ok_or_else(|| invalid("unknown file progress index"))?;
            if end == size || end.saturating_sub(*recorded) >= PARTIAL_RECORD_INTERVAL {
                output.sync_data()?;
                *recorded = end;
                self.write_partial()?;
            }
            Ok(())
        })();
        if outcome.is_err() {
            self.poisoned = true;
        }
        outcome
    }
    /// `config` supplies the storage options every open of the receiver uses.
    /// The progress record is removed first: a complete download is verified
    /// whole below, and an installed standby carries no extra file.
    pub fn finish_download(self, config: &Config) -> Result<VerifiedCheckpoint> {
        if self.poisoned
            || self
                .received
                .iter()
                .zip(&self.owner.manifest.files)
                .any(|(received, file)| *received != file.size)
        {
            return Err(invalid("incomplete or failed checkpoint download"));
        }
        self.owner.verify()?;
        remove_partial_record(&self.owner.directory.path)?;
        self.owner.verify_download()?;
        for file in &self.owner.manifest.files {
            open_regular(&self.owner.directory.path.join(&file.name), file.size, true, false)?.sync_all()?;
        }
        sync_dir(&self.owner.directory.path)?;
        // Prove exact source cut BEFORE opening writable storage or adding a cursor.
        let (options, families) = receiver_options(config, &self.owner.directory.path)?;
        let checkpoint = DB::open_cf_descriptors_read_only(&options, &self.owner.directory.path, families, false)?;
        // A cascaded export's store runs ahead of the source sequence it holds
        // (spec P3.2 §4): the copy opens at its store sequence.
        if checkpoint.latest_sequence_number() != self.owner.manifest.opened_sequence() {
            return Err(invalid("downloaded checkpoint cut mismatch"));
        }
        drop(checkpoint);
        self.owner.verify()?;
        Ok(VerifiedCheckpoint { owner: self.owner })
    }
}

pub struct VerifiedCheckpoint {
    owner: StageOwner,
}
impl VerifiedCheckpoint {
    pub fn staging_path(&self) -> &Path {
        &self.owner.directory.path
    }
    /// Open the verified checkpoint writable and initialize its receiver: the
    /// node-local column family and the first cursor ([`LocalWriter`]). A
    /// checkpoint that already carries that family came from a receiver and is
    /// refused.
    pub fn open(self, config: &Config) -> Result<PhysicalStage> {
        self.owner.verify()?;
        // Read-only RocksDB opening may add diagnostics, so recheck every
        // advertised file rather than requiring the original directory census.
        for file in &self.owner.manifest.files {
            let (digest, _) = fingerprint_file(&self.owner.directory.path.join(&file.name), file.size, true)?;
            if digest != file.sha256 {
                return Err(invalid("verified checkpoint changed before open"));
            }
        }
        let (options, families) = receiver_options(config, &self.owner.directory.path)?;
        let db = DB::open_cf_descriptors(&options, &self.owner.directory.path, families)?;
        if identity(&self.owner.directory.path)? != self.owner.manifest.storage_identity {
            return Err(invalid("RocksDB repaired/replaced snapshot identity during open"));
        }
        let replica = StagedPhysicalReplica::from_checkpoint_at(
            db,
            self.owner.manifest.history,
            self.owner.manifest.checkpoint_sequence,
            self.owner.manifest.opened_sequence(),
        )?;
        Ok(PhysicalStage {
            owner: self.owner,
            replica,
        })
    }
}

pub struct PhysicalStage {
    owner: StageOwner,
    replica: StagedPhysicalReplica,
}
impl PhysicalStage {
    pub fn staging_path(&self) -> &Path {
        &self.owner.directory.path
    }
    /// Close and retain the private sibling without installing a destination.
    pub fn close(self) -> Result<ClosedPhysicalStage> {
        close_owned_stage(self.owner, self.replica)
    }
    pub fn cursor(&self) -> PhysicalCursor {
        self.replica.cursor()
    }
    /// The stage's only writer. Every clone must be dropped before the stage
    /// can close or publish.
    pub fn writer(&self) -> &std::sync::Arc<LocalWriter> {
        self.replica.writer()
    }
    pub fn apply(&mut self, batch: &PhysicalBatch) -> Result<bool> {
        self.apply_with(batch, Durability::Synced)
    }
    /// [`Self::apply`], fsynced or not by `durability` ([`LocalWriter::apply_source_with`]).
    pub fn apply_with(&mut self, batch: &PhysicalBatch, durability: Durability) -> Result<bool> {
        self.owner.verify()?;
        Ok(self.replica.apply_with(batch, durability)?)
    }
    /// The checks [`Self::apply`] runs before every batch — the private
    /// directory and its parent are still this stage's, its marker and saved
    /// manifest unchanged — for a caller that applies a run of batches
    /// through the stage's writer under one check (spec P4.1d: the receiver
    /// worker, once per poll).
    pub fn verify(&self) -> Result<()> {
        self.owner.verify()
    }
    /// Install a closed physical stage, never a ready/serving Nano standby.
    /// Ordinary CLI startup must refuse PHYSICAL_MARKER_FILE until its physical
    /// hydration/serving path is implemented and validated independently.
    pub fn publish(self) -> Result<InstalledPhysicalStage> {
        self.owner.verify()?;
        let cursor = self.replica.close()?; // closes the owned RocksDB handle
        self.owner.verify()?;
        if identity(&self.owner.directory.path)? != self.owner.manifest.storage_identity {
            return Err(invalid("stage identity changed"));
        }
        sync_dir(&self.owner.directory.path)?;
        absent(&self.owner.target)?;
        rename_noreplace(&self.owner.directory.path, &self.owner.target)?;
        // A post-rename fsync failure returns an error and retains the installed
        // target; never delete/roll back a path that could already be observed.
        sync_dir(&self.owner.parent.path)?;
        Ok(InstalledPhysicalStage {
            path: self.owner.target,
            snapshot_id: self.owner.manifest.snapshot_id,
            cursor,
        })
    }
}
pub struct InstalledPhysicalStage {
    pub path: PathBuf,
    pub snapshot_id: Uuid,
    pub cursor: PhysicalCursor,
}

/// A closed durable physical dataset. Its path may still be a private staging
/// sibling or an installed NON-SERVING destination; neither status is SQL-ready.
#[derive(Clone, Debug)]
pub struct ClosedPhysicalStage {
    pub path: PathBuf,
    pub snapshot_id: Uuid,
    pub cursor: PhysicalCursor,
}

fn verify_existing_stage(owner: &StageOwner) -> Result<()> {
    owner.verify()?;
    absent(&owner.directory.path.join(PRIMARY_MARKER))?;
    absent(&owner.directory.path.join(STANDBY_MARKER))?;
    if identity(&owner.directory.path)? != owner.manifest.storage_identity {
        return Err(invalid("existing physical stage IDENTITY mismatch"));
    }
    Ok(())
}

fn close_owned_stage(owner: StageOwner, replica: StagedPhysicalReplica) -> Result<ClosedPhysicalStage> {
    verify_existing_stage(&owner)?;
    let cursor = replica.close()?;
    verify_existing_stage(&owner)?;
    sync_dir(&owner.directory.path)?;
    Ok(ClosedPhysicalStage {
        path: owner.directory.path,
        snapshot_id: owner.manifest.snapshot_id,
        cursor,
    })
}

fn load_existing_stage(path: &Path, expected_history: Uuid, limits: &TransferLimits) -> Result<StageOwner> {
    let directory = OwnedDirectory::capture(path)?;
    let parent = OwnedDirectory::capture(directory.path.parent().ok_or_else(|| invalid("stage has no parent"))?)?;
    refuse_database_ancestors(&parent.path)?;
    let marker_bytes = read_small(&directory.path.join(PHYSICAL_MARKER_FILE), 4096)?;
    let marker: PhysicalMarker = serde_json::from_slice(&marker_bytes)
        .map_err(|error| invalid(format!("existing physical stage marker: {error}")))?;
    let manifest_bytes = read_small(&directory.path.join(PHYSICAL_MANIFEST_FILE), MAX_MANIFEST_BYTES)?;
    let manifest = CheckpointManifest::decode(&manifest_bytes, limits)?;
    let digest: [u8; 32] = Sha256::digest(&manifest_bytes).into();
    if expected_history.is_nil()
        || marker.format != 1
        || marker.mode != "physical-staging-not-serving"
        || marker.snapshot_id != manifest.snapshot_id
        || marker.history != expected_history
        || marker.history != manifest.history
        || marker.checkpoint_sequence != manifest.checkpoint_sequence
        || marker.storage_identity != manifest.storage_identity
        || marker.manifest_sha256 != digest
        || manifest.encoded()? != manifest_bytes
    {
        return Err(invalid("existing physical stage marker/manifest/history mismatch"));
    }
    // After progress, original SST/WAL hashes are intentionally stale. Check
    // the owned directory's current file types/names instead, before RocksDB
    // can follow any of its control files. Do not remove or repair extras.
    census_stage_files(&directory.path, false)?;
    let owner = StageOwner {
        target: directory.path.clone(), // Existing path: never passed to rename.
        directory,
        parent,
        manifest,
        marker: marker_bytes,
    };
    verify_existing_stage(&owner)?;
    Ok(owner)
}

/// The census of an existing stage's files, before RocksDB may follow any of
/// its control files: only RocksDB's own names, the marker and the saved
/// manifest (and, for a directory a fast-forward re-cuts, their temporaries:
/// `recut_temporaries`), `LOCK` and the `LOG` files — each a regular file with
/// one link — and a `CURRENT` that names a manifest present as such a file.
fn census_stage_files(path: &Path, recut_temporaries: bool) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid("non-UTF8 stage file"))?;
        let temporary = name == PHYSICAL_MARKER_TEMP_FILE || name == PHYSICAL_MANIFEST_TEMP_FILE;
        if !(valid_file_name(&name)
            || name == PHYSICAL_MARKER_FILE
            || name == PHYSICAL_MANIFEST_FILE
            || name == "LOCK"
            || name == "LOG"
            || name.starts_with("LOG.old.")
            || (recut_temporaries && temporary))
        {
            return Err(invalid(format!("unexpected existing physical stage file {name}")));
        }
        valid_regular(&fs::symlink_metadata(entry.path())?, u64::MAX, true)?;
    }
    let current = read_small(&path.join("CURRENT"), 256)?;
    let current = std::str::from_utf8(&current)
        .map_err(|_| invalid("existing CURRENT is not UTF8"))?
        .trim_end_matches('\n');
    if !current.starts_with("MANIFEST-") || !valid_file_name(current) {
        return Err(invalid("existing CURRENT references an invalid manifest"));
    }
    open_regular(&path.join(current), u64::MAX, true, false)?;
    Ok(())
}

/// Spec P4.1 §3.3: the checks the fast-forward makes before it opens a
/// standby's store — the directory may be between two cuts, so its marker
/// and manifest are not compared and a re-cut temporary is tolerated, but
/// RocksDB follows its `CURRENT` only after the same census as
/// [`ReopenedPhysicalStage::open`]'s: an owned directory, not nested in a
/// database, carrying no primary or logical-standby marker.
pub fn check_stage_files(path: &Path) -> Result<()> {
    let directory = OwnedDirectory::capture(path)?;
    let parent = OwnedDirectory::capture(directory.path.parent().ok_or_else(|| invalid("stage has no parent"))?)?;
    refuse_database_ancestors(&parent.path)?;
    absent(&directory.path.join(PRIMARY_MARKER))?;
    absent(&directory.path.join(STANDBY_MARKER))?;
    census_stage_files(&directory.path, true)?;
    directory.verify()
}

/// Spec P4.1 §4: the record of the fast-forward that left the standby at
/// `path` between two cuts, if one did — read from a read-only open, after
/// [`check_stage_files`]. `None` when the directory carries none, or cannot
/// be read at all (the caller's own error then says why). Used to name the
/// interrupted fast-forward where a stage open fails for a reason it causes
/// (a marker that disagrees with a re-cut manifest).
pub fn interrupted_fast_forward(path: &Path, config: &Config) -> Option<FastForwardRecord> {
    check_stage_files(path).ok()?;
    if !DB::list_cf(&Options::default(), path)
        .ok()?
        .iter()
        .any(|family| family == LOCAL_CF)
    {
        return None;
    }
    let (options, families) = receiver_options(config, path).ok()?;
    let readonly = DB::open_cf_descriptors_read_only(&options, path, families, false).ok()?;
    fast_forward_record(&readonly).ok().flatten()
}

/// Spec P4.1 §3.3.5: the census of a fast-forwarded stage as the manifest
/// its re-cut writes: every file RocksDB keeps in `path` (not `LOCK`, the
/// `LOG` files, the marker, the saved manifest or their temporaries), with
/// its real size and SHA-256, at the cut `checkpoint_sequence` of the
/// primary's `snapshot_id`. The store must be flushed and CLOSED: closing
/// waits for RocksDB's background work, so no compaction can replace a
/// file while it is hashed, and one pass is the census.
pub fn stage_census_manifest(
    path: &Path,
    history: Uuid,
    snapshot_id: Uuid,
    checkpoint_sequence: u64,
    limits: &TransferLimits,
) -> Result<CheckpointManifest> {
    let mut names = Vec::new();
    for entry in fs::read_dir(path)? {
        let name = entry?
            .file_name()
            .into_string()
            .map_err(|_| invalid("non-UTF8 stage file"))?;
        if outside_census(&name) {
            continue;
        }
        if !valid_file_name(&name) {
            return Err(invalid(format!("unexpected file {name} in the fast-forwarded stage")));
        }
        names.push(name);
    }
    names.sort();
    let mut files = Vec::with_capacity(names.len());
    let mut total_bytes = 0_u64;
    for name in names {
        let file = path.join(&name);
        let size = fs::symlink_metadata(&file)?.len();
        let (sha256, _) = fingerprint_file(&file, size, true)?;
        total_bytes = total_bytes
            .checked_add(size)
            .ok_or_else(|| invalid("stage size overflow"))?;
        files.push(CheckpointFile { name, size, sha256 });
    }
    let manifest = CheckpointManifest {
        format: 1,
        snapshot_id,
        history,
        checkpoint_sequence,
        storage_identity: identity(path)?,
        files,
        total_bytes,
        store_sequence: None,
    };
    manifest.validate(limits).map_err(|error| {
        invalid(format!(
            "the stage's limits refuse its re-cut manifest ({error}): {} files, {} bytes; raise \
             [replication.physical] max_files / max_file_bytes / max_total_bytes (--physical-max-*) and re-run",
            manifest.files.len(),
            manifest.total_bytes
        ))
    })?;
    Ok(manifest)
}

/// The files a re-cut's census leaves out: `LOCK`, the `LOG` files, the
/// marker, the saved manifest and their temporaries.
fn outside_census(name: &str) -> bool {
    matches!(
        name,
        "LOCK"
            | "LOG"
            | PHYSICAL_MARKER_FILE
            | PHYSICAL_MANIFEST_FILE
            | PHYSICAL_MARKER_TEMP_FILE
            | PHYSICAL_MANIFEST_TEMP_FILE
    ) || name.starts_with("LOG.old.")
}

/// Spec P4.1 §3.3.5, before a fast-forward applies anything: the files of
/// the stage at `path` as the re-cut's census would count them, against the
/// transfer limits its manifest must satisfy ([`CheckpointManifest::validate`]:
/// the number of files, each file's size, their total). A stage the census
/// would already refuse is refused HERE, before its data directory is
/// touched, rather than after every leaf was applied. Sizes only, no hashing;
/// a file that disappears meanwhile (a compaction) is not counted. The files
/// the fast-forward adds can still take the stage over the limits.
pub fn check_stage_limits(path: &Path, limits: &TransferLimits) -> Result<()> {
    limits.validate()?;
    let (mut files, mut total, mut largest) = (0_usize, 0_u64, 0_u64);
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid("non-UTF8 stage file"))?;
        if outside_census(&name) {
            continue;
        }
        let size = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        files += 1;
        total = total.saturating_add(size);
        largest = largest.max(size);
    }
    if files > limits.max_files || total > limits.max_total_bytes || largest > limits.max_file_bytes {
        return Err(invalid(format!(
            "the stage's limits would refuse its re-cut manifest: it holds {files} files (at most {}), {total} \
             bytes (at most {}), the largest {largest} bytes (at most {}); this run applied nothing — raise \
             [replication.physical] max_files / max_file_bytes / max_total_bytes (--physical-max-*) and re-run",
            limits.max_files, limits.max_total_bytes, limits.max_file_bytes
        )));
    }
    Ok(())
}

/// Spec P4.1 §3.3.5 (ii)-(iii): re-cut the stage at `path` to `manifest` —
/// the manifest rewritten first, then the marker naming its digest, each
/// atomically (temporary, fsync, rename, directory fsync). The only writer of
/// these two files besides staging. Until the marker is rewritten the two
/// disagree, so a stage open refuses the directory; the fast-forward's
/// record (removed only after this) names why.
pub fn recut_stage(path: &Path, manifest: &CheckpointManifest) -> Result<()> {
    recut_stage_with(path, manifest, &mut || {})
}

/// [`recut_stage`], calling `after_manifest` between the two files (a test
/// hook stops the process there).
pub fn recut_stage_with(path: &Path, manifest: &CheckpointManifest, after_manifest: &mut dyn FnMut()) -> Result<()> {
    let directory = OwnedDirectory::capture(path)?;
    let encoded = manifest.encoded()?;
    let marker = staging_marker(manifest, &encoded)?;
    replace_atomically(
        &directory.path,
        PHYSICAL_MANIFEST_TEMP_FILE,
        PHYSICAL_MANIFEST_FILE,
        &encoded,
    )?;
    after_manifest();
    replace_atomically(
        &directory.path,
        PHYSICAL_MARKER_TEMP_FILE,
        PHYSICAL_MARKER_FILE,
        &marker,
    )?;
    directory.verify()
}

/// Resume a previously initialized physical stage, never a partial download or
/// ordinary Nano dataset. This owner can apply and close; it cannot rename,
/// overwrite, rebind history, reset progress, or publish serving readiness.
pub struct ReopenedPhysicalStage {
    owner: StageOwner,
    replica: StagedPhysicalReplica,
}
impl ReopenedPhysicalStage {
    /// `config` supplies the storage options every open of the receiver uses.
    ///
    /// Spec P4.1 §4: a directory a fast-forward left between two cuts is
    /// refused with [`PhysicalError::FastForwardInterrupted`] — by the cursor
    /// read below, or, when the re-cut had already rewritten the manifest but
    /// not the marker (so they disagree), by the record itself.
    pub fn open(path: &Path, expected_history: Uuid, limits: TransferLimits, config: &Config) -> Result<Self> {
        let owner = match load_existing_stage(path, expected_history, &limits) {
            Ok(owner) => owner,
            Err(error) => {
                return Err(match interrupted_fast_forward(path, config) {
                    Some(record) => PhysicalError::FastForwardInterrupted {
                        from: record.from,
                        to: record.to,
                    }
                    .into(),
                    None => error,
                })
            }
        };
        // The file census above vetted every name before RocksDB follows
        // CURRENT. A stage without the node-local family keeps its cursor in
        // the default one (it predates the namespace) or was never
        // initialized; either way it is refused here, before any RocksDB
        // handle exists, so not even a LOG is rotated.
        if !DB::list_cf(&Options::default(), &owner.directory.path)?
            .iter()
            .any(|family| family == LOCAL_CF)
        {
            return Err(PhysicalError::FullResyncRequired(
                "the staging directory predates the node-local namespace (or was never initialized); rebuild it",
            )
            .into());
        }
        // Raw read-only RocksDB replays its own WAL into an immutable view. No
        // Nano logical recovery, metadata adoption or writable open occurs until
        // an existing atomic cursor is validated against the current local seq.
        let (options, families) = receiver_options(config, &owner.directory.path)?;
        let readonly = DB::open_cf_descriptors_read_only(&options, &owner.directory.path, families, false)?;
        let preflight = StagedPhysicalReplica::resume(readonly, expected_history)?.close()?;
        if preflight.snapshot_sequence() != owner.manifest.checkpoint_sequence {
            return Err(invalid("existing physical cursor belongs to a different snapshot cut"));
        }
        verify_existing_stage(&owner)?;
        let (options, families) = receiver_options(config, &owner.directory.path)?;
        let db = DB::open_cf_descriptors(&options, &owner.directory.path, families)?; // Exclusive RocksDB LOCK.
        verify_existing_stage(&owner)?;
        let replica = StagedPhysicalReplica::resume(db, expected_history)?;
        if replica.cursor() != preflight {
            return Err(invalid("physical cursor changed between preflight and exclusive open"));
        }
        Ok(Self { owner, replica })
    }
    pub fn path(&self) -> &Path {
        &self.owner.directory.path
    }
    pub fn cursor(&self) -> PhysicalCursor {
        self.replica.cursor()
    }
    /// The stage's only writer. Every clone must be dropped before the stage
    /// can close.
    pub fn writer(&self) -> &std::sync::Arc<LocalWriter> {
        self.replica.writer()
    }
    pub fn apply(&mut self, batch: &PhysicalBatch) -> Result<bool> {
        self.apply_with(batch, Durability::Synced)
    }
    /// [`Self::apply`], fsynced or not by `durability` ([`LocalWriter::apply_source_with`]).
    pub fn apply_with(&mut self, batch: &PhysicalBatch, durability: Durability) -> Result<bool> {
        verify_existing_stage(&self.owner)?;
        Ok(self.replica.apply_with(batch, durability)?)
    }
    /// The checks [`Self::apply`] runs before every batch — the directory is
    /// still this stage's, carries no primary/standby marker and the same
    /// IDENTITY — for a caller that applies through the stage's writer
    /// itself (a serving standby's `PhysicalApplier`, spec P1.3), or applies
    /// a run of batches under one check (spec P4.1d: once per poll).
    pub fn verify(&self) -> Result<()> {
        verify_existing_stage(&self.owner)
    }
    pub fn close(self) -> Result<ClosedPhysicalStage> {
        close_owned_stage(self.owner, self.replica)
    }
}

fn rename_noreplace(source: &Path, target: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| invalid("NUL source path"))?;
        let target = CString::new(target.as_os_str().as_bytes()).map_err(|_| invalid("NUL target path"))?;
        if unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                target.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (source, target);
        Err(invalid("atomic no-replace publication requires Linux renameat2"))
    }
}

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::replication::{
        history::load_or_create_primary_history,
        physical::{poll_source, FlushMemo, MAX_BATCH_BYTES},
    };
    use rocksdb::WriteBatch;
    use std::os::unix::fs::symlink;

    fn config() -> Config {
        Config::default()
    }
    /// An installed receiver's store holds two column families; a writable
    /// open must name both.
    fn open_receiver(path: &Path) -> DB {
        DB::open_cf(&Options::default(), path, ["default", LOCAL_CF]).unwrap()
    }

    fn primary(root: &Path) -> (DB, Uuid) {
        let mut options = Options::default();
        options.create_if_missing(true);
        options.set_wal_ttl_seconds(3600);
        let source = DB::open(&options, root.join("source")).unwrap();
        source.put(b"seed", b"old").unwrap();
        let history = load_or_create_primary_history(source.path()).unwrap();
        (source, history)
    }
    fn export(source: &DB, history: Uuid, parent: &Path) -> OnlineCheckpointExport {
        OnlineCheckpointExport::create(source, history, parent, TransferLimits::default()).unwrap()
    }
    fn download(export: &OnlineCheckpointExport, target: &Path) -> ReceiverStaging {
        let mut receiver = ReceiverStaging::begin(
            target,
            &export.manifest_bytes().unwrap(),
            export.manifest.history,
            TransferLimits::default(),
        )
        .unwrap();
        for (index, file) in export.manifest.files.iter().enumerate() {
            let mut offset = 0;
            while offset < file.size {
                let bytes = export.read_chunk(index as u32, offset, 997).unwrap();
                assert!(!bytes.is_empty());
                receiver.append_chunk(index as u32, offset, &bytes).unwrap();
                offset += bytes.len() as u64;
            }
        }
        receiver
    }

    #[test]
    fn online_checkpoint_transfer_continues_raw_batches_and_installs_only_physical_stage() {
        let root = tempfile::tempdir().unwrap();
        let (source, history) = primary(root.path());
        let original_marker = fs::read(source.path().join(PRIMARY_MARKER)).unwrap();
        let export = export(&source, history, root.path());
        let cut = export.manifest.checkpoint_sequence;
        assert_eq!(cut, 1);
        let mut batch = WriteBatch::default();
        batch.put(b"first", b"committed-one");
        batch.put(b"second", b"committed-two");
        batch.delete(b"seed");
        source.write(batch).unwrap();
        let poll = poll_source(&source, history, cut, MAX_BATCH_BYTES, &FlushMemo::new()).unwrap();
        assert_eq!(poll.through, cut + 3);
        let target = root.path().join("installed");
        let receiver = download(&export, &target);
        let old_stage = receiver.staging_path().to_path_buf();
        let mut stage = receiver.finish_download(&config()).unwrap().open(&config()).unwrap();
        assert_eq!(stage.cursor().source_sequence(), cut);
        for batch in &poll.batches {
            assert!(stage.apply(batch).unwrap());
        }
        let installed = stage.publish().unwrap();
        assert_eq!(installed.path, target);
        assert!(!old_stage.exists());
        assert_eq!(installed.cursor.source_sequence(), cut + 3);
        let marker: PhysicalMarker =
            serde_json::from_slice(&fs::read(target.join(PHYSICAL_MARKER_FILE)).unwrap()).unwrap();
        assert_eq!(marker.mode, "physical-staging-not-serving");
        assert_eq!(marker.history, history);
        assert!(!target.join(PRIMARY_MARKER).exists());
        assert!(!target.join(STANDBY_MARKER).exists());
        let db = open_receiver(&target);
        assert_eq!(db.get(b"first").unwrap(), Some(b"committed-one".to_vec()));
        assert_eq!(db.get(b"second").unwrap(), Some(b"committed-two".to_vec()));
        assert_eq!(db.get(b"seed").unwrap(), None);
        let resumed = StagedPhysicalReplica::resume(db, history).unwrap();
        assert_eq!(resumed.cursor(), installed.cursor);
        resumed.close().unwrap();
        assert_eq!(fs::read(source.path().join(PRIMARY_MARKER)).unwrap(), original_marker);
    }

    #[test]
    fn existing_empty_target_and_publication_race_are_never_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let (source, history) = primary(root.path());
        let export = export(&source, history, root.path());
        let target = root.path().join("existing");
        fs::create_dir(&target).unwrap();
        assert!(ReceiverStaging::begin(
            &target,
            &export.manifest_bytes().unwrap(),
            history,
            TransferLimits::default()
        )
        .is_err());
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        let race_target = root.path().join("raced");
        let receiver = download(&export, &race_target);
        let retained = receiver.staging_path().to_path_buf();
        let stage = receiver.finish_download(&config()).unwrap().open(&config()).unwrap();
        fs::create_dir(&race_target).unwrap();
        fs::write(race_target.join("user-owned"), b"preserve").unwrap();
        assert!(stage.publish().is_err());
        assert_eq!(fs::read(race_target.join("user-owned")).unwrap(), b"preserve");
        assert!(retained.join(PHYSICAL_MARKER_FILE).is_file());
    }

    #[test]
    fn traversal_symlink_nested_source_and_unexpected_history_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let (source, history) = primary(root.path());
        assert!(OnlineCheckpointExport::create(&source, history, source.path(), TransferLimits::default()).is_err());
        assert!(
            OnlineCheckpointExport::create(&source, Uuid::new_v4(), root.path(), TransferLimits::default()).is_err()
        );
        let export = export(&source, history, root.path());
        let mut bad = export.manifest.clone();
        bad.files[0].name = "../outside".into();
        assert!(ReceiverStaging::begin(
            &root.path().join("bad"),
            &serde_json::to_vec(&bad).unwrap(),
            history,
            TransferLimits::default()
        )
        .is_err());
        assert!(!root.path().join("outside").exists());
        symlink(root.path(), root.path().join("alias")).unwrap();
        assert!(ReceiverStaging::begin(
            &root.path().join("alias/child"),
            &export.manifest_bytes().unwrap(),
            history,
            TransferLimits::default()
        )
        .is_err());
        assert!(ReceiverStaging::begin(
            &source.path().join("nested"),
            &export.manifest_bytes().unwrap(),
            history,
            TransferLimits::default()
        )
        .is_err());
        assert!(ReceiverStaging::begin(
            &root.path().join("wrong-history"),
            &export.manifest_bytes().unwrap(),
            Uuid::new_v4(),
            TransferLimits::default()
        )
        .is_err());
    }

    #[test]
    fn malformed_chunk_offset_poisoning_retains_partial_files() {
        let root = tempfile::tempdir().unwrap();
        let (source, history) = primary(root.path());
        let export = export(&source, history, root.path());
        let mut receiver = ReceiverStaging::begin(
            &root.path().join("target"),
            &export.manifest_bytes().unwrap(),
            history,
            TransferLimits::default(),
        )
        .unwrap();
        let path = receiver.staging_path().to_path_buf();
        let index = export.manifest.files.iter().position(|file| file.size > 1).unwrap() as u32;
        let bytes = export.read_chunk(index, 0, 1).unwrap();
        assert!(receiver.append_chunk(index, 1, &bytes).is_err());
        assert!(receiver.append_chunk(index, 0, &bytes).is_err());
        assert!(receiver.finish_download(&config()).is_err());
        assert!(path.join(PHYSICAL_MARKER_FILE).exists());
        assert!(!root.path().join("target").exists());
    }

    #[test]
    fn corrupt_download_and_changed_stage_marker_are_never_opened() {
        let root = tempfile::tempdir().unwrap();
        let (source, history) = primary(root.path());
        let export = export(&source, history, root.path());
        let receiver = download(&export, &root.path().join("bad-data"));
        let path = receiver.staging_path().to_path_buf();
        let mut bytes = fs::read(path.join("IDENTITY")).unwrap();
        bytes[0] ^= 1;
        fs::write(path.join("IDENTITY"), bytes).unwrap();
        assert!(receiver.finish_download(&config()).is_err());
        assert!(path.exists());
        let receiver = download(&export, &root.path().join("bad-marker"));
        let path = receiver.staging_path().to_path_buf();
        fs::write(path.join(PHYSICAL_MARKER_FILE), b"{}").unwrap();
        assert!(receiver.finish_download(&config()).is_err());
        assert!(path.exists());
        let receiver = download(&export, &root.path().join("bad-open-manifest"));
        let path = receiver.staging_path().to_path_buf();
        let stage = receiver.finish_download(&config()).unwrap().open(&config()).unwrap();
        fs::write(path.join(PHYSICAL_MANIFEST_FILE), b"{}").unwrap();
        assert!(stage.publish().is_err());
        assert!(path.exists());
        assert!(!root.path().join("bad-open-manifest").exists());
    }

    #[test]
    fn changed_export_file_and_changed_primary_metadata_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let (source, history) = primary(root.path());
        let export = export(&source, history, root.path());
        let index = export
            .manifest
            .files
            .iter()
            .position(|file| file.name == "IDENTITY")
            .unwrap();
        // IDENTITY is an independent copied file, never a hard-linked SST.
        fs::write(export.directory.path.join("IDENTITY"), b"changed").unwrap();
        assert!(export.read_chunk(index as u32, 0, 128).is_err());
        fs::write(source.path().join(PRIMARY_MARKER), b"{}").unwrap();
        assert!(OnlineCheckpointExport::create(&source, history, root.path(), TransferLimits::default()).is_err());
    }

    #[test]
    fn duplicate_files_byte_limits_and_wrong_checkpoint_cut_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let (source, history) = primary(root.path());
        let export = export(&source, history, root.path());
        let mut duplicate = export.manifest.clone();
        duplicate.files.push(duplicate.files[0].clone());
        assert!(duplicate.validate(&TransferLimits::default()).is_err());
        assert!(export.read_chunk(0, 0, (MAX_CHUNK_BYTES + 1) as u32).is_err());
        assert!(export.read_chunk(0, export.manifest.files[0].size + 1, 1).is_err());
        let tiny = TransferLimits {
            max_total_bytes: 1,
            max_file_bytes: 1,
            ..TransferLimits::default()
        };
        assert!(export.manifest.validate(&tiny).is_err());
        let mut wrong = export.manifest.clone();
        wrong.checkpoint_sequence += 1;
        let mut receiver = ReceiverStaging::begin(
            &root.path().join("wrong-cut"),
            &wrong.encoded().unwrap(),
            history,
            TransferLimits::default(),
        )
        .unwrap();
        for (index, file) in export.manifest.files.iter().enumerate() {
            let mut offset = 0;
            while offset < file.size {
                let bytes = export.read_chunk(index as u32, offset, MAX_CHUNK_BYTES as u32).unwrap();
                receiver.append_chunk(index as u32, offset, &bytes).unwrap();
                offset += bytes.len() as u64;
            }
        }
        assert!(receiver.finish_download(&config()).is_err());
        assert!(!root.path().join("wrong-cut").exists());
    }
    fn installed_fixture(root: &Path) -> (DB, Uuid, PathBuf, CheckpointManifest) {
        let (source, history) = primary(root);
        let export = export(&source, history, root);
        let manifest = export.manifest.clone();
        let installed = download(&export, &root.join("installed"))
            .finish_download(&config())
            .unwrap()
            .open(&config())
            .unwrap()
            .publish()
            .unwrap();
        (source, history, installed.path, manifest)
    }

    #[test]
    fn reopened_installed_stage_preserves_progress_and_continues_without_original_file_hashes() {
        let root = tempfile::tempdir().unwrap();
        let (source, history, path, manifest) = installed_fixture(root.path());
        let mut resumed = ReopenedPhysicalStage::open(&path, history, TransferLimits::default(), &config()).unwrap();
        assert_eq!(resumed.path(), path);
        assert_eq!(resumed.cursor().snapshot_sequence(), manifest.checkpoint_sequence);
        let mut batch = WriteBatch::default();
        batch.put(b"after-a", b"a");
        batch.put(b"after-b", b"b");
        source.write(batch).unwrap();
        let poll = poll_source(
            &source,
            history,
            resumed.cursor().source_sequence(),
            MAX_BATCH_BYTES,
            &FlushMemo::new(),
        )
        .unwrap();
        assert_eq!(poll.batches.len(), 1);
        assert!(resumed.apply(&poll.batches[0]).unwrap());
        assert!(!resumed.apply(&poll.batches[0]).unwrap());
        let closed = resumed.close().unwrap();
        assert_eq!(closed.path, path);
        assert_eq!(closed.cursor.source_sequence(), 3);
        assert_eq!(closed.cursor.snapshot_sequence(), 1);
        // Writable open/application evolves CURRENT/MANIFEST/WAL. The immutable
        // transfer manifest is provenance; it is no longer a live file census.
        assert!(
            manifest.files.iter().any(|file| {
                fingerprint_file(&path.join(&file.name), file.size, true)
                    .map_or(true, |(digest, _)| digest != file.sha256)
            }),
            "fixture did not evolve any original checkpoint file"
        );
        let mut again = ReopenedPhysicalStage::open(&path, history, TransferLimits::default(), &config()).unwrap();
        assert_eq!(again.cursor(), closed.cursor);
        assert!(!again.apply(&poll.batches[0]).unwrap());
        source.put(b"after-c", b"c").unwrap();
        let later = poll_source(&source, history, 3, MAX_BATCH_BYTES, &FlushMemo::new()).unwrap();
        for batch in &later.batches {
            assert!(again.apply(batch).unwrap());
        }
        let final_closed = again.close().unwrap();
        assert_eq!(final_closed.cursor.source_sequence(), 4);
        let db = DB::open_for_read_only(&Options::default(), &path, false).unwrap();
        for (key, value) in [(b"after-a", b"a"), (b"after-b", b"b"), (b"after-c", b"c")] {
            assert_eq!(db.get(key).unwrap(), Some(value.to_vec()));
        }
    }

    /// A column family adds files, not file names: the node-local family's
    /// own SST is an ordinary `<n>.sst`, so the census still admits the stage.
    #[test]
    fn a_stage_with_the_node_local_family_passes_the_file_census() {
        let root = tempfile::tempdir().unwrap();
        let (_source, history, path, _) = installed_fixture(root.path());
        {
            let db = open_receiver(&path);
            let sequence = db.latest_sequence_number();
            db.flush_cf(db.cf_handle(LOCAL_CF).unwrap()).unwrap();
            db.flush().unwrap();
            // A flush is not a write: the cursor still accounts for the sequence.
            assert_eq!(db.latest_sequence_number(), sequence);
            assert!(db
                .live_files()
                .unwrap()
                .iter()
                .any(|file| file.column_family_name == LOCAL_CF));
        }
        assert!(DB::list_cf(&Options::default(), &path)
            .unwrap()
            .iter()
            .any(|family| family == LOCAL_CF));
        load_existing_stage(&path, history, &TransferLimits::default()).unwrap();
        let reopened = ReopenedPhysicalStage::open(&path, history, TransferLimits::default(), &config()).unwrap();
        assert_eq!(reopened.cursor().snapshot_sequence(), 1);
        reopened.close().unwrap();
    }

    /// Spec P4.1 §3.3.5, §4: a fast-forwarded stage is re-cut at the new
    /// cut — manifest, then marker, then the cursor write that removes the
    /// record — and reopens at it; a stage left between the manifest and
    /// the marker is refused naming the interrupted fast-forward, as is one
    /// whose record is still there.
    #[test]
    fn a_recut_stage_reopens_at_its_new_cut_and_an_unfinished_one_names_the_fast_forward() {
        use crate::replication::physical::FastForwardWriter;
        let root = tempfile::tempdir().unwrap();
        let (source, history, path, manifest) = installed_fixture(root.path());
        source.put(b"later", b"value").unwrap();
        let cut = source.latest_sequence_number();
        let snapshot = Uuid::new_v4();
        let record = FastForwardRecord::new(history, snapshot, manifest.checkpoint_sequence, cut, 64 * 1024);
        let interrupted =
            |path: &Path| match ReopenedPhysicalStage::open(path, history, TransferLimits::default(), &config()) {
                Err(TransferError::Physical(PhysicalError::FastForwardInterrupted { from, to })) => (from, to),
                Err(other) => panic!("not the fast-forward refusal: {other}"),
                Ok(_) => panic!("an unfinished fast-forward was reopened"),
            };

        // The limits pre-check counts the files the census would.
        check_stage_limits(&path, &TransferLimits::default()).unwrap();
        let one_file = TransferLimits {
            max_files: 1,
            ..TransferLimits::default()
        };
        let refused = check_stage_limits(&path, &one_file).unwrap_err().to_string();
        assert!(refused.contains("at most 1)"), "{refused}");

        let writer = FastForwardWriter::open(&path, &config(), history).unwrap();
        writer.start_record(&record).unwrap();
        writer.flush().unwrap();
        writer.release();
        assert_eq!(interrupted(&path), (manifest.checkpoint_sequence, cut));
        assert_eq!(
            interrupted_fast_forward(&path, &config()).map(|found| found.to),
            Some(cut)
        );

        // Killed after the manifest: marker and manifest disagree.
        let recut = stage_census_manifest(&path, history, snapshot, cut, &TransferLimits::default()).unwrap();
        assert_eq!(recut.checkpoint_sequence, cut);
        assert!(recut.files.iter().any(|file| file.name == "CURRENT"));
        replace_atomically(
            &path,
            PHYSICAL_MANIFEST_TEMP_FILE,
            PHYSICAL_MANIFEST_FILE,
            &recut.encoded().unwrap(),
        )
        .unwrap();
        assert!(load_existing_stage(&path, history, &TransferLimits::default()).is_err());
        assert_eq!(interrupted(&path), (manifest.checkpoint_sequence, cut));

        // The re-run re-cuts: manifest, marker (the store closed), then the
        // store reopened — its record still there — for the cursor.
        recut_stage(&path, &recut).unwrap();
        assert_eq!(interrupted(&path), (manifest.checkpoint_sequence, cut));
        let writer = FastForwardWriter::open(&path, &config(), history).unwrap();
        assert_eq!(writer.record().unwrap(), Some(record));
        let cursor = writer.recut(cut, snapshot).unwrap();
        assert_eq!(writer.close().unwrap(), cursor);
        assert_eq!(interrupted_fast_forward(&path, &config()), None);
        let reopened = ReopenedPhysicalStage::open(&path, history, TransferLimits::default(), &config()).unwrap();
        assert_eq!(reopened.cursor().snapshot_sequence(), cut);
        assert_eq!(reopened.cursor().source_sequence(), cut);
        reopened.close().unwrap();
        // No temporary is left behind.
        assert!(!path.join(PHYSICAL_MANIFEST_TEMP_FILE).exists());
        assert!(!path.join(PHYSICAL_MARKER_TEMP_FILE).exists());
    }

    #[test]
    fn reopening_does_not_adopt_partial_download_and_initial_close_does_not_install() {
        let root = tempfile::tempdir().unwrap();
        let (source, history) = primary(root.path());
        let export = export(&source, history, root.path());
        let target = root.path().join("not-installed");
        let verified = download(&export, &target).finish_download(&config()).unwrap();
        let sibling = verified.staging_path().to_path_buf();
        assert!(ReopenedPhysicalStage::open(&sibling, history, TransferLimits::default(), &config()).is_err());
        assert!(!target.exists());
        let initial = verified.open(&config()).unwrap();
        assert_eq!(initial.staging_path(), sibling);
        let closed = initial.close().unwrap();
        assert_eq!(closed.path, sibling);
        assert!(!target.exists());
        let resumed = ReopenedPhysicalStage::open(&sibling, history, TransferLimits::default(), &config()).unwrap();
        assert_eq!(resumed.cursor(), closed.cursor);
        assert_eq!(resumed.close().unwrap().path, sibling);
        assert!(!target.exists());
    }

    #[test]
    fn reopening_checks_history_lock_and_original_snapshot_cut_without_rebinding() {
        let root = tempfile::tempdir().unwrap();
        let (_source, history, path, mut manifest) = installed_fixture(root.path());
        assert!(ReopenedPhysicalStage::open(&path, Uuid::new_v4(), TransferLimits::default(), &config()).is_err());
        let owned = ReopenedPhysicalStage::open(&path, history, TransferLimits::default(), &config()).unwrap();
        assert!(ReopenedPhysicalStage::open(&path, history, TransferLimits::default(), &config()).is_err());
        let closed = owned.close().unwrap();
        // Even internally consistent external metadata cannot change the
        // original cut already recorded in the durable physical cursor.
        manifest.checkpoint_sequence = 0;
        let bytes = manifest.encoded().unwrap();
        let mut marker: PhysicalMarker =
            serde_json::from_slice(&fs::read(path.join(PHYSICAL_MARKER_FILE)).unwrap()).unwrap();
        marker.checkpoint_sequence = 0;
        marker.manifest_sha256 = Sha256::digest(&bytes).into();
        let marker_bytes = serde_json::to_vec(&marker).unwrap();
        fs::write(path.join(PHYSICAL_MANIFEST_FILE), &bytes).unwrap();
        fs::write(path.join(PHYSICAL_MARKER_FILE), &marker_bytes).unwrap();
        assert!(ReopenedPhysicalStage::open(&path, history, TransferLimits::default(), &config()).is_err());
        assert_eq!(fs::read(path.join(PHYSICAL_MARKER_FILE)).unwrap(), marker_bytes);
        let readonly = DB::open_cf_for_read_only(&Options::default(), &path, ["default", LOCAL_CF], false).unwrap();
        let durable = StagedPhysicalReplica::resume(readonly, history)
            .unwrap()
            .close()
            .unwrap();
        assert_eq!(durable, closed.cursor);
    }

    #[test]
    fn reopening_refuses_untracked_writes_and_mixed_legacy_authorization() {
        let root = tempfile::tempdir().unwrap();
        let (_source, history, path, _) = installed_fixture(root.path());
        let db = open_receiver(&path);
        db.put(b"unrelated-local-write", b"preserved").unwrap();
        drop(db);
        assert!(ReopenedPhysicalStage::open(&path, history, TransferLimits::default(), &config()).is_err());
        let db = DB::open_for_read_only(&Options::default(), &path, false).unwrap();
        assert_eq!(db.get(b"unrelated-local-write").unwrap(), Some(b"preserved".to_vec()));
        drop(db);
        let other = tempfile::tempdir().unwrap();
        let (_source, history, path, _) = installed_fixture(other.path());
        create_file(&path.join(STANDBY_MARKER), b"{}").unwrap();
        assert!(ReopenedPhysicalStage::open(&path, history, TransferLimits::default(), &config()).is_err());
        assert_eq!(fs::read(path.join(STANDBY_MARKER)).unwrap(), b"{}");
    }

    #[test]
    fn reopening_refuses_identity_symlink_and_hardlink_changes() {
        let root = tempfile::tempdir().unwrap();
        let (_source, history, path, _) = installed_fixture(root.path());
        let alias = root.path().join("alias");
        symlink(&path, &alias).unwrap();
        assert!(ReopenedPhysicalStage::open(&alias, history, TransferLimits::default(), &config()).is_err());
        fs::write(path.join("IDENTITY"), b"replacement-identity\n").unwrap();
        assert!(ReopenedPhysicalStage::open(&path, history, TransferLimits::default(), &config()).is_err());
        assert_eq!(fs::read(path.join("IDENTITY")).unwrap(), b"replacement-identity\n");
        let other = tempfile::tempdir().unwrap();
        let (_source, history, path, _) = installed_fixture(other.path());
        fs::hard_link(path.join("CURRENT"), other.path().join("aliased-current")).unwrap();
        assert!(ReopenedPhysicalStage::open(&path, history, TransferLimits::default(), &config()).is_err());
        assert!(other.path().join("aliased-current").is_file());
    }

    // --- spec P2.2: resumable download ------------------------------------

    fn read_record(staging: &Path) -> PartialRecord {
        serde_json::from_slice(&fs::read(staging.join(PHYSICAL_PARTIAL_FILE)).unwrap()).unwrap()
    }

    /// Append every chunk of the files `take` selects (all of a file), then
    /// `partial` bytes of the file after them.
    fn append_files(receiver: &mut ReceiverStaging, export: &OnlineCheckpointExport, take: usize, partial: u64) {
        for (index, file) in export.manifest.files.iter().enumerate() {
            let limit = match index.cmp(&take) {
                std::cmp::Ordering::Less => file.size,
                std::cmp::Ordering::Equal => partial.min(file.size),
                std::cmp::Ordering::Greater => 0,
            };
            let mut offset = receiver.progress()[index];
            while offset < limit {
                let length = (limit - offset).min(997) as u32;
                let bytes = export.read_chunk(index as u32, offset, length).unwrap();
                receiver.append_chunk(index as u32, offset, &bytes).unwrap();
                offset += bytes.len() as u64;
            }
        }
    }

    /// The record round-trips, claims only completed files (a part of a file
    /// below the record interval is not claimed), and a download into the
    /// same target continues from it: the unclaimed tail is truncated, and
    /// the finished copy installs with no record left behind.
    #[test]
    fn a_partial_download_is_resumed_through_its_verified_record() {
        let root = tempfile::tempdir().unwrap();
        let (source, history) = primary(root.path());
        let export = export(&source, history, root.path());
        let manifest_bytes = export.manifest_bytes().unwrap();
        let target = root.path().join("resumed");
        let (mut receiver, start) =
            ReceiverStaging::begin_or_resume(&target, &manifest_bytes, history, TransferLimits::default()).unwrap();
        assert_eq!(start, StagingStart::Fresh);
        let staging = receiver.staging_path().to_path_buf();
        assert_eq!(staging, staging_path_for(&target).unwrap());
        assert!(staging
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(STAGING_PREFIX));
        let files = export.manifest.files.clone();
        let last = files.len() - 1;
        let half = files[last].size / 2;
        assert!(half > 0, "the fixture's last file has a middle");
        append_files(&mut receiver, &export, last, half);

        let record = read_record(&staging);
        assert_eq!(
            record,
            PartialRecord::new(&export.manifest, receiver.manifest_sha256, &record_done(&files, last))
        );
        let bytes = serde_json::to_vec(&record).unwrap();
        assert_eq!(
            serde_json::from_slice::<PartialRecord>(&bytes).unwrap(),
            record,
            "round trip"
        );
        assert_eq!(record.snapshot_id, export.manifest.snapshot_id);
        assert_eq!(record.files[last].bytes_done, 0, "half a small file is not claimed");
        assert_eq!(fs::metadata(staging.join(&files[last].name)).unwrap().len(), half);
        assert_eq!(partial_snapshot(&target), Some(export.manifest.snapshot_id));
        drop(receiver); // the process died

        let (mut receiver, start) =
            ReceiverStaging::begin_or_resume(&target, &manifest_bytes, history, TransferLimits::default()).unwrap();
        let held: u64 = files[..last].iter().map(|file| file.size).sum();
        assert_eq!(
            start,
            StagingStart::Resumed {
                held,
                redownload: Vec::new()
            }
        );
        assert_eq!(receiver.progress()[last], 0);
        assert_eq!(
            fs::metadata(staging.join(&files[last].name)).unwrap().len(),
            0,
            "the unclaimed tail was truncated"
        );
        assert!(receiver.holds(&export.manifest));
        append_files(&mut receiver, &export, files.len(), 0);
        let installed = receiver
            .finish_download(&config())
            .unwrap()
            .open(&config())
            .unwrap()
            .publish()
            .unwrap();
        assert_eq!(installed.path, target);
        assert!(!target.join(PHYSICAL_PARTIAL_FILE).exists());
        assert!(!target.join(PHYSICAL_PARTIAL_TEMP_FILE).exists());
        assert!(!staging.exists());
        assert_eq!(partial_snapshot(&target), None);
        let db = open_receiver(&target);
        assert_eq!(db.get(b"seed").unwrap(), Some(b"old".to_vec()));
    }

    /// The bytes the record claims after `append_files(.., take, small)`.
    fn record_done(files: &[CheckpointFile], take: usize) -> Vec<u64> {
        files
            .iter()
            .enumerate()
            .map(|(index, file)| if index < take { file.size } else { 0 })
            .collect()
    }

    /// The resume decision table: another snapshot, an unreadable record, a
    /// file shorter than recorded, no record, or an unexpected file delete
    /// the staging directory and start over, saying why; a complete file
    /// whose digest no longer matches is downloaded again; a directory at the
    /// staging path that is not a staging directory is refused and kept.
    #[test]
    fn the_resume_decision_table() {
        let root = tempfile::tempdir().unwrap();
        let (source, history) = primary(root.path());
        let export = export(&source, history, root.path());
        let manifest_bytes = export.manifest_bytes().unwrap();
        let files = export.manifest.files.clone();
        let sized = files.iter().position(|file| file.size > 1).unwrap();
        let begin = |target: &Path, manifest: &[u8]| {
            ReceiverStaging::begin_or_resume(target, manifest, history, TransferLimits::default()).unwrap()
        };
        // A complete download, abandoned before it was finished.
        let abandoned = |name: &str| {
            let target = root.path().join(name);
            let (mut receiver, _) = begin(&target, &manifest_bytes);
            append_files(&mut receiver, &export, files.len(), 0);
            let staging = receiver.staging_path().to_path_buf();
            drop(receiver);
            (target, staging)
        };
        let started_over = |start: StagingStart, why: &str| match start {
            StagingStart::StartedOver { reason } => assert!(reason.contains(why), "{why}: {reason}"),
            other => panic!("{why}: expected a start over, got {other:?}"),
        };

        // Same snapshot and manifest: resumed, everything held.
        let (target, _) = abandoned("same");
        let (receiver, start) = begin(&target, &manifest_bytes);
        assert_eq!(
            start,
            StagingStart::Resumed {
                held: export.manifest.total_bytes,
                redownload: Vec::new()
            }
        );
        assert!(receiver
            .progress()
            .iter()
            .zip(&files)
            .all(|(done, file)| *done == file.size));

        // Another snapshot (the lease expired: a new export).
        let (target, staging) = abandoned("other-snapshot");
        let newer = OnlineCheckpointExport::create(&source, history, root.path(), TransferLimits::default()).unwrap();
        let (receiver, start) = begin(&target, &newer.manifest_bytes().unwrap());
        started_over(start, "now serves snapshot");
        assert_eq!(receiver.snapshot_id(), newer.manifest.snapshot_id);
        assert!(receiver.progress().iter().all(|done| *done == 0), "a new download");
        assert_eq!(read_record(&staging).snapshot_id, newer.manifest.snapshot_id);

        // An unreadable record.
        let (target, staging) = abandoned("corrupt-record");
        fs::write(staging.join(PHYSICAL_PARTIAL_FILE), b"{").unwrap();
        started_over(begin(&target, &manifest_bytes).1, "unreadable");

        // A file shorter than the record claims.
        let (target, staging) = abandoned("short-file");
        OpenOptions::new()
            .write(true)
            .open(staging.join(&files[sized].name))
            .unwrap()
            .set_len(files[sized].size - 1)
            .unwrap();
        started_over(begin(&target, &manifest_bytes).1, "file sizes disagree");

        // No record at all.
        let (target, staging) = abandoned("no-record");
        fs::remove_file(staging.join(PHYSICAL_PARTIAL_FILE)).unwrap();
        started_over(begin(&target, &manifest_bytes).1, "no partial download record");

        // A file nobody downloaded.
        let (target, staging) = abandoned("extra-file");
        fs::write(staging.join("stray"), b"x").unwrap();
        started_over(begin(&target, &manifest_bytes).1, "unexpected file stray");
        assert!(
            !staging.join("stray").exists(),
            "the started-over directory is a new one"
        );

        // A complete file whose content no longer matches its digest.
        let (target, staging) = abandoned("bad-digest");
        let path = staging.join(&files[sized].name);
        let mut bytes = fs::read(&path).unwrap();
        bytes[0] ^= 1;
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .write_all(&bytes)
            .unwrap();
        let (receiver, start) = begin(&target, &manifest_bytes);
        assert_eq!(
            start,
            StagingStart::Resumed {
                held: export.manifest.total_bytes - files[sized].size,
                redownload: vec![files[sized].name.clone()]
            }
        );
        assert_eq!(receiver.progress()[sized], 0);
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);

        // Not a staging directory: refused, never deleted.
        let target = root.path().join("foreign");
        let staging = staging_path_for(&target).unwrap();
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("user-data"), b"keep").unwrap();
        assert!(
            ReceiverStaging::begin_or_resume(&target, &manifest_bytes, history, TransferLimits::default()).is_err()
        );
        assert_eq!(fs::read(staging.join("user-data")).unwrap(), b"keep");
    }

    /// Inside one file the record is rewritten after at most
    /// `PARTIAL_RECORD_INTERVAL` new bytes, and only for fsynced bytes.
    #[test]
    fn the_record_follows_a_large_file_every_interval() {
        let root = tempfile::tempdir().unwrap();
        let (source, history) = primary(root.path());
        // One large, incompressible SST.
        let mut value = vec![0_u8; 1024];
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        for row in 0..20_000_u32 {
            for byte in &mut value {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = state as u8;
            }
            source.put(format!("row{row:08}"), &value).unwrap();
        }
        source.flush().unwrap();
        let export = export(&source, history, root.path());
        let (large, file) = export
            .manifest
            .files
            .iter()
            .enumerate()
            .max_by_key(|(_, file)| file.size)
            .unwrap();
        assert!(file.size > PARTIAL_RECORD_INTERVAL + 1024 * 1024, "{} bytes", file.size);
        let target = root.path().join("large");
        let (mut receiver, _) = ReceiverStaging::begin_or_resume(
            &target,
            &export.manifest_bytes().unwrap(),
            history,
            TransferLimits::default(),
        )
        .unwrap();
        let staging = receiver.staging_path().to_path_buf();
        let mut offset = 0;
        let mut checked = false;
        while offset < file.size {
            let bytes = export.read_chunk(large as u32, offset, MAX_CHUNK_BYTES as u32).unwrap();
            receiver.append_chunk(large as u32, offset, &bytes).unwrap();
            offset += bytes.len() as u64;
            let done = read_record(&staging).files[large].bytes_done;
            assert!(
                done <= offset && offset - done < PARTIAL_RECORD_INTERVAL,
                "{done} of {offset}"
            );
            if done > 0 && done < file.size {
                checked = true;
            }
        }
        assert!(checked, "the record advanced inside the file");
        assert_eq!(read_record(&staging).files[large].bytes_done, file.size);
    }
}
