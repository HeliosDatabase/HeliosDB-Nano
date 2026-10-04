//! Physical checkpoint transfer into an owned, fresh, NON-SERVING dataset.
//! Export ownership retains snapshot files, never WAL history. Poll continuity
//! and retention must be checked separately. Local parent directories must be
//! owned and not group/world writable; hostile same-user renames are out of scope.

use super::physical::{PhysicalBatch, PhysicalCursor, PhysicalError, StagedPhysicalReplica};
use rocksdb::{checkpoint::Checkpoint, Options, DB};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, File, Metadata, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

pub const PHYSICAL_MARKER_FILE: &str = "NANO-PHYSICAL-REPLICA.json";
pub const PHYSICAL_MANIFEST_FILE: &str = "NANO-PHYSICAL-MANIFEST.json";
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
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
    pub checkpoint_sequence: u64,
    pub storage_identity: String,
    pub files: Vec<CheckpointFile>,
    pub total_bytes: u64,
}
impl CheckpointManifest {
    pub fn validate(&self, limits: &TransferLimits) -> Result<()> {
        limits.validate()?;
        if self.format != 1
            || self.snapshot_id.is_nil()
            || self.history.is_nil()
            || self.checkpoint_sequence > MAX_SEQUENCE
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
pub struct OnlineCheckpointExport {
    _lease: tempfile::TempDir,
    directory: OwnedDirectory,
    manifest: CheckpointManifest,
    fingerprints: Vec<Metadata>,
    limits: TransferLimits,
}
impl OnlineCheckpointExport {
    /// `db` must be the exclusively opened live primary. Bind caller history to
    /// its actual, pre-existing primary marker and RocksDB IDENTITY first.
    pub fn create(db: &DB, expected_history: Uuid, export_parent: &Path, limits: TransferLimits) -> Result<Self> {
        limits.validate()?;
        let (source, storage_identity) = source_identity(db, expected_history)?;
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
        let checkpoint = DB::open_for_read_only(&Options::default(), &path, false)?;
        let cut = checkpoint.latest_sequence_number();
        drop(checkpoint);
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
        let (_, current_identity) = source_identity(db, expected_history)?;
        if current_identity != manifest.storage_identity {
            return Err(invalid("source identity changed"));
        }
        directory.verify()?;
        parent.verify()?;
        sync_dir(&path)?;
        Ok(Self {
            _lease: lease,
            directory,
            manifest,
            fingerprints,
            limits,
        })
    }
    pub fn manifest(&self) -> &CheckpointManifest {
        &self.manifest
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

/// Failed/interrupted receivers intentionally retain their private sibling
/// directory. Nothing here removes, resets, or adopts an existing user path.
pub struct ReceiverStaging {
    owner: StageOwner,
    received: Vec<u64>,
    limits: TransferLimits,
    poisoned: bool,
}
impl ReceiverStaging {
    pub fn begin(target: &Path, manifest_bytes: &[u8], expected_history: Uuid, limits: TransferLimits) -> Result<Self> {
        let manifest = CheckpointManifest::decode(manifest_bytes, &limits)?;
        if expected_history.is_nil() || manifest.history != expected_history {
            return Err(invalid("unexpected snapshot history"));
        }
        let target = absolute_path(target)?;
        let parent = OwnedDirectory::capture(target.parent().ok_or_else(|| invalid("target has no parent"))?)?;
        refuse_database_ancestors(&parent.path)?;
        absent(&target)?;
        let path = parent.path.join(format!(".nano-resync-{}", Uuid::new_v4()));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path)?;
        let directory = OwnedDirectory::capture(&path)?;
        let encoded = manifest.encoded()?;
        let marker = serde_json::to_vec(&PhysicalMarker {
            format: 1,
            mode: "physical-staging-not-serving".into(),
            snapshot_id: manifest.snapshot_id,
            history: manifest.history,
            checkpoint_sequence: manifest.checkpoint_sequence,
            storage_identity: manifest.storage_identity.clone(),
            manifest_sha256: Sha256::digest(&encoded).into(),
        })
        .map_err(|error| invalid(error.to_string()))?;
        create_file(&path.join(PHYSICAL_MARKER_FILE), &marker)?;
        create_file(&path.join(PHYSICAL_MANIFEST_FILE), &encoded)?;
        for file in &manifest.files {
            create_file(&path.join(&file.name), &[])?;
        }
        sync_dir(&path)?;
        sync_dir(&parent.path)?;
        let received = vec![0; manifest.files.len()];
        Ok(Self {
            owner: StageOwner {
                directory,
                parent,
                target,
                manifest,
                marker,
            },
            received,
            limits,
            poisoned: false,
        })
    }
    pub fn staging_path(&self) -> &Path {
        &self.owner.directory.path
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
            Ok(())
        })();
        if outcome.is_err() {
            self.poisoned = true;
        }
        outcome
    }
    pub fn finish_download(self) -> Result<VerifiedCheckpoint> {
        if self.poisoned
            || self
                .received
                .iter()
                .zip(&self.owner.manifest.files)
                .any(|(received, file)| *received != file.size)
        {
            return Err(invalid("incomplete or failed checkpoint download"));
        }
        self.owner.verify_download()?;
        for file in &self.owner.manifest.files {
            open_regular(&self.owner.directory.path.join(&file.name), file.size, true, false)?.sync_all()?;
        }
        sync_dir(&self.owner.directory.path)?;
        // Prove exact source cut BEFORE opening writable storage or adding a cursor.
        let checkpoint = DB::open_for_read_only(&Options::default(), &self.owner.directory.path, false)?;
        if checkpoint.latest_sequence_number() != self.owner.manifest.checkpoint_sequence {
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
    pub fn open(self) -> Result<PhysicalStage> {
        self.owner.verify()?;
        // Read-only RocksDB opening may add diagnostics, so recheck every
        // advertised file rather than requiring the original directory census.
        for file in &self.owner.manifest.files {
            let (digest, _) = fingerprint_file(&self.owner.directory.path.join(&file.name), file.size, true)?;
            if digest != file.sha256 {
                return Err(invalid("verified checkpoint changed before open"));
            }
        }
        let mut options = Options::default();
        options.create_if_missing(false);
        let db = DB::open(&options, &self.owner.directory.path)?;
        if identity(&self.owner.directory.path)? != self.owner.manifest.storage_identity {
            return Err(invalid("RocksDB repaired/replaced snapshot identity during open"));
        }
        let replica = StagedPhysicalReplica::from_checkpoint(
            db,
            self.owner.manifest.history,
            self.owner.manifest.checkpoint_sequence,
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
    pub fn cursor(&self) -> &PhysicalCursor {
        self.replica.cursor()
    }
    pub fn apply(&mut self, batch: &PhysicalBatch) -> Result<bool> {
        self.owner.verify()?;
        Ok(self.replica.apply(batch)?)
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
    for entry in fs::read_dir(&directory.path)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid("non-UTF8 stage file"))?;
        if !(valid_file_name(&name)
            || name == PHYSICAL_MARKER_FILE
            || name == PHYSICAL_MANIFEST_FILE
            || name == "LOCK"
            || name == "LOG"
            || name.starts_with("LOG.old."))
        {
            return Err(invalid(format!("unexpected existing physical stage file {name}")));
        }
        valid_regular(&fs::symlink_metadata(entry.path())?, u64::MAX, true)?;
    }
    let current = read_small(&directory.path.join("CURRENT"), 256)?;
    let current = std::str::from_utf8(&current)
        .map_err(|_| invalid("existing CURRENT is not UTF8"))?
        .trim_end_matches('\n');
    if !current.starts_with("MANIFEST-") || !valid_file_name(current) {
        return Err(invalid("existing CURRENT references an invalid manifest"));
    }
    open_regular(&directory.path.join(current), u64::MAX, true, false)?;
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

/// Resume a previously initialized physical stage, never a partial download or
/// ordinary Nano dataset. This owner can apply and close; it cannot rename,
/// overwrite, rebind history, reset progress, or publish serving readiness.
pub struct ReopenedPhysicalStage {
    owner: StageOwner,
    replica: StagedPhysicalReplica,
}
impl ReopenedPhysicalStage {
    pub fn open(path: &Path, expected_history: Uuid, limits: TransferLimits) -> Result<Self> {
        let owner = load_existing_stage(path, expected_history, &limits)?;
        // Raw read-only RocksDB replays its own WAL into an immutable view. No
        // Nano logical recovery, metadata adoption or writable open occurs until
        // an existing atomic cursor is validated against the current local seq.
        let readonly = DB::open_for_read_only(&Options::default(), &owner.directory.path, false)?;
        let preflight = StagedPhysicalReplica::resume(readonly, expected_history)?.close()?;
        if preflight.snapshot_sequence() != owner.manifest.checkpoint_sequence {
            return Err(invalid("existing physical cursor belongs to a different snapshot cut"));
        }
        verify_existing_stage(&owner)?;
        let mut options = Options::default();
        options.create_if_missing(false);
        let db = DB::open(&options, &owner.directory.path)?; // Exclusive RocksDB LOCK.
        verify_existing_stage(&owner)?;
        let replica = StagedPhysicalReplica::resume(db, expected_history)?;
        if replica.cursor() != &preflight {
            return Err(invalid("physical cursor changed between preflight and exclusive open"));
        }
        Ok(Self { owner, replica })
    }
    pub fn path(&self) -> &Path {
        &self.owner.directory.path
    }
    pub fn cursor(&self) -> &PhysicalCursor {
        self.replica.cursor()
    }
    pub fn apply(&mut self, batch: &PhysicalBatch) -> Result<bool> {
        verify_existing_stage(&self.owner)?;
        Ok(self.replica.apply(batch)?)
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::replication::{
        history::load_or_create_primary_history,
        physical::{poll_source, MAX_BATCH_BYTES},
    };
    use rocksdb::WriteBatch;
    use std::os::unix::fs::symlink;

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
        let poll = poll_source(&source, history, cut, MAX_BATCH_BYTES).unwrap();
        assert_eq!(poll.through, cut + 3);
        let target = root.path().join("installed");
        let receiver = download(&export, &target);
        let old_stage = receiver.staging_path().to_path_buf();
        let mut stage = receiver.finish_download().unwrap().open().unwrap();
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
        let db = DB::open(&Options::default(), &target).unwrap();
        assert_eq!(db.get(b"first").unwrap(), Some(b"committed-one".to_vec()));
        assert_eq!(db.get(b"second").unwrap(), Some(b"committed-two".to_vec()));
        assert_eq!(db.get(b"seed").unwrap(), None);
        let resumed = StagedPhysicalReplica::resume(db, history).unwrap();
        assert_eq!(resumed.cursor(), &installed.cursor);
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
        let stage = receiver.finish_download().unwrap().open().unwrap();
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
        assert!(receiver.finish_download().is_err());
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
        assert!(receiver.finish_download().is_err());
        assert!(path.exists());
        let receiver = download(&export, &root.path().join("bad-marker"));
        let path = receiver.staging_path().to_path_buf();
        fs::write(path.join(PHYSICAL_MARKER_FILE), b"{}").unwrap();
        assert!(receiver.finish_download().is_err());
        assert!(path.exists());
        let receiver = download(&export, &root.path().join("bad-open-manifest"));
        let path = receiver.staging_path().to_path_buf();
        let stage = receiver.finish_download().unwrap().open().unwrap();
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
        assert!(receiver.finish_download().is_err());
        assert!(!root.path().join("wrong-cut").exists());
    }
    fn installed_fixture(root: &Path) -> (DB, Uuid, PathBuf, CheckpointManifest) {
        let (source, history) = primary(root);
        let export = export(&source, history, root);
        let manifest = export.manifest.clone();
        let installed = download(&export, &root.join("installed"))
            .finish_download()
            .unwrap()
            .open()
            .unwrap()
            .publish()
            .unwrap();
        (source, history, installed.path, manifest)
    }

    #[test]
    fn reopened_installed_stage_preserves_progress_and_continues_without_original_file_hashes() {
        let root = tempfile::tempdir().unwrap();
        let (source, history, path, manifest) = installed_fixture(root.path());
        let mut resumed = ReopenedPhysicalStage::open(&path, history, TransferLimits::default()).unwrap();
        assert_eq!(resumed.path(), path);
        assert_eq!(resumed.cursor().snapshot_sequence(), manifest.checkpoint_sequence);
        let mut batch = WriteBatch::default();
        batch.put(b"after-a", b"a");
        batch.put(b"after-b", b"b");
        source.write(batch).unwrap();
        let poll = poll_source(&source, history, resumed.cursor().source_sequence(), MAX_BATCH_BYTES).unwrap();
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
        let mut again = ReopenedPhysicalStage::open(&path, history, TransferLimits::default()).unwrap();
        assert_eq!(again.cursor(), &closed.cursor);
        assert!(!again.apply(&poll.batches[0]).unwrap());
        source.put(b"after-c", b"c").unwrap();
        let later = poll_source(&source, history, 3, MAX_BATCH_BYTES).unwrap();
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

    #[test]
    fn reopening_does_not_adopt_partial_download_and_initial_close_does_not_install() {
        let root = tempfile::tempdir().unwrap();
        let (source, history) = primary(root.path());
        let export = export(&source, history, root.path());
        let target = root.path().join("not-installed");
        let verified = download(&export, &target).finish_download().unwrap();
        let sibling = verified.staging_path().to_path_buf();
        assert!(ReopenedPhysicalStage::open(&sibling, history, TransferLimits::default()).is_err());
        assert!(!target.exists());
        let initial = verified.open().unwrap();
        assert_eq!(initial.staging_path(), sibling);
        let closed = initial.close().unwrap();
        assert_eq!(closed.path, sibling);
        assert!(!target.exists());
        let resumed = ReopenedPhysicalStage::open(&sibling, history, TransferLimits::default()).unwrap();
        assert_eq!(resumed.cursor(), &closed.cursor);
        assert_eq!(resumed.close().unwrap().path, sibling);
        assert!(!target.exists());
    }

    #[test]
    fn reopening_checks_history_lock_and_original_snapshot_cut_without_rebinding() {
        let root = tempfile::tempdir().unwrap();
        let (_source, history, path, mut manifest) = installed_fixture(root.path());
        assert!(ReopenedPhysicalStage::open(&path, Uuid::new_v4(), TransferLimits::default()).is_err());
        let owned = ReopenedPhysicalStage::open(&path, history, TransferLimits::default()).unwrap();
        assert!(ReopenedPhysicalStage::open(&path, history, TransferLimits::default()).is_err());
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
        assert!(ReopenedPhysicalStage::open(&path, history, TransferLimits::default()).is_err());
        assert_eq!(fs::read(path.join(PHYSICAL_MARKER_FILE)).unwrap(), marker_bytes);
        let readonly = DB::open_for_read_only(&Options::default(), &path, false).unwrap();
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
        let db = DB::open(&Options::default(), &path).unwrap();
        db.put(b"unrelated-local-write", b"preserved").unwrap();
        drop(db);
        assert!(ReopenedPhysicalStage::open(&path, history, TransferLimits::default()).is_err());
        let db = DB::open_for_read_only(&Options::default(), &path, false).unwrap();
        assert_eq!(db.get(b"unrelated-local-write").unwrap(), Some(b"preserved".to_vec()));
        drop(db);
        let other = tempfile::tempdir().unwrap();
        let (_source, history, path, _) = installed_fixture(other.path());
        create_file(&path.join(STANDBY_MARKER), b"{}").unwrap();
        assert!(ReopenedPhysicalStage::open(&path, history, TransferLimits::default()).is_err());
        assert_eq!(fs::read(path.join(STANDBY_MARKER)).unwrap(), b"{}");
    }

    #[test]
    fn reopening_refuses_identity_symlink_and_hardlink_changes() {
        let root = tempfile::tempdir().unwrap();
        let (_source, history, path, _) = installed_fixture(root.path());
        let alias = root.path().join("alias");
        symlink(&path, &alias).unwrap();
        assert!(ReopenedPhysicalStage::open(&alias, history, TransferLimits::default()).is_err());
        fs::write(path.join("IDENTITY"), b"replacement-identity\n").unwrap();
        assert!(ReopenedPhysicalStage::open(&path, history, TransferLimits::default()).is_err());
        assert_eq!(fs::read(path.join("IDENTITY")).unwrap(), b"replacement-identity\n");
        let other = tempfile::tempdir().unwrap();
        let (_source, history, path, _) = installed_fixture(other.path());
        fs::hard_link(path.join("CURRENT"), other.path().join("aliased-current")).unwrap();
        assert!(ReopenedPhysicalStage::open(&path, history, TransferLimits::default()).is_err());
        assert!(other.path().join("aliased-current").is_file());
    }
}
