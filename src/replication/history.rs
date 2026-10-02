//! Durable dataset identity and receiver binding for native history-mix guards.
//!
//! This is containment, not a snapshot/LSN continuity contract. A new receiver
//! may bind only when this startup atomically created its data-directory leaf.
//! Existing unbound paths never acquire that privilege, even if they are empty.
//! Node UUIDs are intentionally absent from this API. Copying a primary's files
//! can copy its identity; this module is not distributed fencing or a fork API.

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

pub const PRIMARY_HISTORY_FILE: &str = "NANO-PRIMARY-HISTORY.json";
pub const STANDBY_BINDING_FILE: &str = "NANO-STANDBY-BINDING.json";
const FORMAT_VERSION: u32 = 1;
const MAX_METADATA_BYTES: u64 = 4096;
const MAX_STORAGE_IDENTITY_BYTES: usize = 128;

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("replication history mismatch: expected {expected}, received {actual}; preserve this receiver and prepare a new dataset")]
    Mismatch { expected: Uuid, actual: Uuid },
    #[error("replication history metadata refused: {0}")]
    InvalidMetadata(String),
    #[error("existing receiver directory has no binding: {0:?}; preserve it and use a new data directory")]
    UnboundReceiver(PathBuf),
    #[error("receiver binding previously failed; this startup cannot retry or adopt another history")]
    BindingFailed,
    #[error("replication history I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

type HistoryResult<T> = Result<T, HistoryError>;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PrimaryRecord {
    format: u32,
    history_id: Uuid,
    storage_identity: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BindingRecord {
    format: u32,
    upstream_history_id: Uuid,
    storage_identity: String,
}

#[derive(Debug)]
enum BindingState {
    Fresh,
    Bound(Uuid),
    Failed,
}

/// A startup-owned fresh path or an already durable binding. Fields are private
/// so ordinary callers cannot manufacture disk-backed fresh-start privilege.
#[derive(Debug)]
pub struct ReceiverHistory {
    directory: Option<OwnedDirectory>,
    state: Mutex<BindingState>,
}

#[derive(Debug)]
struct OwnedDirectory {
    path: PathBuf,
    identity: Metadata,
}

impl ReceiverHistory {
    /// Ephemeral receiver: bind once during this instance's lifetime. There is
    /// deliberately no restart-persistence or on-disk continuity claim.
    pub fn in_memory() -> Self {
        Self {
            directory: None,
            state: Mutex::new(BindingState::Fresh),
        }
    }

    /// Expected identity for the next handshake. `None` authorizes first binding
    /// only for the fresh startup represented by this value.
    pub fn expected_history(&self) -> HistoryResult<Option<Uuid>> {
        match *self.state.lock() {
            BindingState::Fresh => Ok(None),
            BindingState::Bound(id) => {
                if let Some(directory) = &self.directory {
                    directory.verify_binding(id)?;
                }
                Ok(Some(id))
            }
            BindingState::Failed => Err(HistoryError::BindingFailed),
        }
    }

    /// Check every peer handshake and durably bind the first peer before any WAL
    /// is enqueued or readiness is published. Every failed first binding poisons
    /// this instance; no file is removed, repaired, replaced, or reset.
    pub fn verify_or_bind(&self, peer: Uuid) -> HistoryResult<()> {
        let mut state = self.state.lock();
        match *state {
            BindingState::Bound(expected) => {
                if expected != peer {
                    return Err(HistoryError::Mismatch { expected, actual: peer });
                }
                if let Some(directory) = &self.directory {
                    directory.verify_binding(expected)?;
                }
                Ok(())
            }
            BindingState::Failed => Err(HistoryError::BindingFailed),
            BindingState::Fresh => {
                // Set BEFORE validation/I/O: even a partial file or failed fsync
                // cannot become permission to rebind on a later connection.
                *state = BindingState::Failed;
                validate_id(peer)?;
                if let Some(directory) = &self.directory {
                    directory.verify()?;
                    refuse_marker(
                        &directory.path,
                        PRIMARY_HISTORY_FILE,
                        "receiver also carries primary identity",
                    )?;
                    write_new_record(
                        directory,
                        STANDBY_BINDING_FILE,
                        &BindingRecord {
                            format: FORMAT_VERSION,
                            upstream_history_id: peer,
                            storage_identity: read_storage_identity(&directory.path)?,
                        },
                    )?;
                }
                *state = BindingState::Bound(peer);
                Ok(())
            }
        }
    }
}

/// Call BEFORE opening Nano/RocksDB. Atomically create a new receiver directory,
/// or require an existing valid binding. Existing empty directories are not fresh.
pub fn prepare_receiver_history(path: &Path) -> HistoryResult<ReceiverHistory> {
    let path = checked_absolute_path(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| invalid("receiver path has no parent"))?
        .to_path_buf();
    fs::create_dir_all(&parent)?;
    reject_symlink_components(&parent)?;

    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    let fresh = match builder.create(&path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(error.into()),
    };
    let directory = OwnedDirectory::capture(path)?;
    refuse_marker(
        &directory.path,
        PRIMARY_HISTORY_FILE,
        "receiver directory carries primary identity",
    )?;
    let state = if fresh {
        // Persist the new leaf itself; a crash cannot turn this path back into
        // an apparently new startup while retaining its database files.
        sync_directory(&parent)?;
        BindingState::Fresh
    } else {
        let record: BindingRecord = match read_record(&directory.path.join(STANDBY_BINDING_FILE)) {
            Ok(record) => record,
            Err(HistoryError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(HistoryError::UnboundReceiver(directory.path));
            }
            Err(error) => return Err(error),
        };
        validate_format(record.format)?;
        validate_id(record.upstream_history_id)?;
        validate_storage_identity(&directory.path, &record.storage_identity)?;
        BindingState::Bound(record.upstream_history_id)
    };
    directory.verify()?;
    Ok(ReceiverHistory {
        directory: Some(directory),
        state: Mutex::new(state),
    })
}

/// Load/create the primary's durable dataset identity. The caller MUST already
/// hold this dataset's exclusive RocksDB lock and call before accepting peers.
/// Any receiver binding forbids an implicit promotion/role fork. There is no
/// reset operation; malformed existing identity is an error, never regenerated.
pub fn load_or_create_primary_history(path: &Path) -> HistoryResult<Uuid> {
    let directory = OwnedDirectory::capture(checked_absolute_path(path)?)?;
    refuse_marker(
        &directory.path,
        STANDBY_BINDING_FILE,
        "primary directory carries a standby binding; implicit promotion is refused",
    )?;
    match read_record::<PrimaryRecord>(&directory.path.join(PRIMARY_HISTORY_FILE)) {
        Ok(record) => {
            validate_format(record.format)?;
            validate_id(record.history_id)?;
            validate_storage_identity(&directory.path, &record.storage_identity)?;
            directory.verify()?;
            Ok(record.history_id)
        }
        Err(HistoryError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            let history_id = Uuid::new_v4();
            write_new_record(
                &directory,
                PRIMARY_HISTORY_FILE,
                &PrimaryRecord {
                    format: FORMAT_VERSION,
                    history_id,
                    storage_identity: read_storage_identity(&directory.path)?,
                },
            )?;
            Ok(history_id)
        }
        Err(error) => Err(error),
    }
}

impl OwnedDirectory {
    fn capture(path: PathBuf) -> HistoryResult<Self> {
        reject_symlink_components(&path)?;
        let identity = fs::symlink_metadata(&path)?;
        if !identity.is_dir() || identity.file_type().is_symlink() {
            return Err(invalid("data path is not a regular directory"));
        }
        Ok(Self { path, identity })
    }

    fn verify(&self) -> HistoryResult<()> {
        reject_symlink_components(&self.path)?;
        let current = fs::symlink_metadata(&self.path)?;
        if !current.is_dir() || !same_file(&self.identity, &current) {
            return Err(invalid("data-directory identity changed during history initialization"));
        }
        Ok(())
    }

    fn verify_binding(&self, expected: Uuid) -> HistoryResult<()> {
        self.verify()?;
        refuse_marker(
            &self.path,
            PRIMARY_HISTORY_FILE,
            "receiver also carries primary identity",
        )?;
        let record: BindingRecord = read_record(&self.path.join(STANDBY_BINDING_FILE))?;
        validate_format(record.format)?;
        validate_id(record.upstream_history_id)?;
        validate_storage_identity(&self.path, &record.storage_identity)?;
        if record.upstream_history_id != expected {
            return Err(invalid("persisted receiver binding differs from its startup identity"));
        }
        self.verify()
    }
}

fn invalid(message: impl Into<String>) -> HistoryError {
    HistoryError::InvalidMetadata(message.into())
}

fn validate_format(format: u32) -> HistoryResult<()> {
    if format != FORMAT_VERSION {
        return Err(invalid(format!("unsupported history metadata format {format}")));
    }
    Ok(())
}

fn validate_id(id: Uuid) -> HistoryResult<()> {
    if id.is_nil() {
        return Err(invalid("nil history identity is not valid"));
    }
    Ok(())
}

fn checked_absolute_path(path: &Path) -> HistoryResult<PathBuf> {
    if path.as_os_str().is_empty() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(invalid("empty data paths and '..' components are refused"));
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    reject_symlink_components(&absolute)?;
    Ok(absolute)
}

fn reject_symlink_components(path: &Path) -> HistoryResult<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Err(invalid("symlink data paths are refused")),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn refuse_marker(root: &Path, name: &str, reason: &str) -> HistoryResult<()> {
    match fs::symlink_metadata(root.join(name)) {
        Ok(_) => Err(invalid(reason)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
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
        // This guard currently requires Unix directory fsync and stable inode
        // identity. Do not silently weaken replacement checks on other systems.
        let _ = (first, second);
        false
    }
}

fn read_record<T: serde::de::DeserializeOwned>(path: &Path) -> HistoryResult<T> {
    let bytes = read_metadata_bytes(path)?;
    serde_json::from_slice(&bytes).map_err(|error| invalid(format!("invalid history record: {error}")))
}

fn read_metadata_bytes(path: &Path) -> HistoryResult<Vec<u8>> {
    let before = fs::symlink_metadata(path)?;
    if !before.is_file() || before.file_type().is_symlink() || before.len() > MAX_METADATA_BYTES {
        return Err(invalid("history record is symlink, nonregular, or oversized"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.nlink() != 1 {
            return Err(invalid("hard-linked history record is refused"));
        }
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Refuse a last-component symlink swap and avoid blocking if a regular
        // file is replaced with a FIFO between lstat and open.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || opened.len() > MAX_METADATA_BYTES {
        return Err(invalid("opened history record is nonregular or oversized"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.nlink() != 1 {
            return Err(invalid("opened history record is hard-linked"));
        }
    }
    let after = fs::symlink_metadata(path)?;
    if after.file_type().is_symlink() || !same_file(&before, &opened) || !same_file(&opened, &after) {
        return Err(invalid("history record changed while opening"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_METADATA_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_METADATA_BYTES {
        return Err(invalid("history record exceeds size limit"));
    }
    Ok(bytes)
}

fn read_storage_identity(root: &Path) -> HistoryResult<String> {
    let bytes = read_metadata_bytes(&root.join("IDENTITY"))?;
    if bytes.len() > MAX_STORAGE_IDENTITY_BYTES {
        return Err(invalid("RocksDB IDENTITY exceeds size limit"));
    }
    let identity = std::str::from_utf8(&bytes)
        .map_err(|_| invalid("RocksDB IDENTITY is not UTF-8"))?
        .trim_end_matches(['\r', '\n']);
    if identity.is_empty() || !identity.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(invalid("RocksDB IDENTITY is empty or contains invalid characters"));
    }
    Ok(identity.to_string())
}

fn validate_storage_identity(root: &Path, expected: &str) -> HistoryResult<()> {
    if read_storage_identity(root)? != expected {
        return Err(invalid(
            "RocksDB IDENTITY differs from the durable history record; database replacement or role fork is refused",
        ));
    }
    Ok(())
}

fn write_new_record<T: Serialize>(directory: &OwnedDirectory, name: &str, record: &T) -> HistoryResult<()> {
    directory.verify()?;
    let mut bytes =
        serde_json::to_vec(record).map_err(|error| invalid(format!("cannot encode history record: {error}")))?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_METADATA_BYTES {
        return Err(invalid("history record exceeds size limit"));
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(directory.path.join(name))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    directory.verify()?;
    sync_directory(&directory.path)?;
    Ok(())
}

fn sync_directory(path: &Path) -> HistoryResult<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::sync::{Arc, Barrier};
    use tempfile::TempDir;

    // Simulate the IDENTITY that RocksDB creates between receiver preparation
    // and its first network handshake. Integration tests exercise real DB opens.
    fn simulate_database_open(path: &Path) {
        fs::write(path.join("IDENTITY"), format!("{}\n", Uuid::new_v4())).unwrap();
    }

    fn fresh_receiver_with_database(path: &Path) -> ReceiverHistory {
        let receiver = prepare_receiver_history(path).unwrap();
        simulate_database_open(path);
        receiver
    }

    #[test]
    fn durable_receiver_reopens_accepts_same_and_refuses_other_history() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("receiver");
        let history = Uuid::new_v4();
        let receiver = fresh_receiver_with_database(&path);
        assert_eq!(receiver.expected_history().unwrap(), None);
        receiver.verify_or_bind(history).unwrap();
        let before = fs::read(path.join(STANDBY_BINDING_FILE)).unwrap();
        assert_eq!(
            fs::metadata(path.join(STANDBY_BINDING_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(receiver);
        let reopened = prepare_receiver_history(&path).unwrap();
        assert_eq!(reopened.expected_history().unwrap(), Some(history));
        reopened.verify_or_bind(history).unwrap();
        let other = Uuid::new_v4();
        assert!(
            matches!(reopened.verify_or_bind(other), Err(HistoryError::Mismatch { expected, actual }) if expected == history && actual == other)
        );
        assert_eq!(fs::read(path.join(STANDBY_BINDING_FILE)).unwrap(), before);
    }

    #[test]
    fn dataset_identity_is_durable_and_independent_of_node_uuid() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        simulate_database_open(first.path());
        simulate_database_open(second.path());
        let reused_node_id = Uuid::new_v4();
        let history_a = load_or_create_primary_history(first.path()).unwrap();
        assert_eq!(load_or_create_primary_history(first.path()).unwrap(), history_a);
        let history_b = load_or_create_primary_history(second.path()).unwrap();
        assert_ne!(history_a, history_b);
        assert_ne!(history_a, reused_node_id);
        assert_ne!(history_b, reused_node_id);
    }

    #[test]
    fn existing_unbound_or_aborted_fresh_directory_never_adopts() {
        let root = TempDir::new().unwrap();
        assert!(matches!(
            prepare_receiver_history(root.path()),
            Err(HistoryError::UnboundReceiver(_))
        ));
        let path = root.path().join("aborted");
        drop(prepare_receiver_history(&path).unwrap());
        assert!(matches!(
            prepare_receiver_history(&path),
            Err(HistoryError::UnboundReceiver(_))
        ));
        assert!(path.is_dir());
    }

    #[test]
    fn atomic_fresh_directory_has_only_one_winner() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("receiver");
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    prepare_receiver_history(&path).is_ok()
                })
            })
            .collect();
        assert_eq!(
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .filter(|won| *won)
                .count(),
            1
        );
    }

    #[test]
    fn failed_first_binding_is_poisoned_and_never_overwrites() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("receiver");
        let receiver = fresh_receiver_with_database(&path);
        fs::write(path.join(STANDBY_BINDING_FILE), b"external competing record").unwrap();
        assert!(receiver.verify_or_bind(Uuid::new_v4()).is_err());
        assert!(matches!(receiver.expected_history(), Err(HistoryError::BindingFailed)));
        assert!(matches!(
            receiver.verify_or_bind(Uuid::new_v4()),
            Err(HistoryError::BindingFailed)
        ));
        assert_eq!(
            fs::read(path.join(STANDBY_BINDING_FILE)).unwrap(),
            b"external competing record"
        );
    }

    #[test]
    fn reconnect_revalidates_binding_and_never_recreates_a_removed_record() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("receiver");
        let receiver = fresh_receiver_with_database(&path);
        let id = Uuid::new_v4();
        receiver.verify_or_bind(id).unwrap();
        let binding = path.join(STANDBY_BINDING_FILE);
        fs::rename(&binding, path.join("preserved-binding.json")).unwrap();
        assert!(receiver.expected_history().is_err());
        assert!(receiver.verify_or_bind(id).is_err());
        assert!(!binding.exists());
    }

    #[test]
    fn nil_identity_poisoning_does_not_create_a_binding() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("receiver");
        let receiver = prepare_receiver_history(&path).unwrap();
        assert!(receiver.verify_or_bind(Uuid::nil()).is_err());
        assert!(matches!(receiver.expected_history(), Err(HistoryError::BindingFailed)));
        assert!(!path.join(STANDBY_BINDING_FILE).exists());
    }

    #[test]
    fn corrupt_oversized_and_symlink_records_are_refused() {
        for content in [b"broken".to_vec(), vec![b'x'; MAX_METADATA_BYTES as usize + 1]] {
            let root = TempDir::new().unwrap();
            fs::write(root.path().join(STANDBY_BINDING_FILE), &content).unwrap();
            assert!(prepare_receiver_history(root.path()).is_err());
            assert_eq!(fs::read(root.path().join(STANDBY_BINDING_FILE)).unwrap(), content);
        }
        let root = TempDir::new().unwrap();
        let target = root.path().join("target");
        fs::write(&target, b"unchanged").unwrap();
        symlink(&target, root.path().join(STANDBY_BINDING_FILE)).unwrap();
        assert!(prepare_receiver_history(root.path()).is_err());
        assert_eq!(fs::read(target).unwrap(), b"unchanged");
    }

    #[test]
    fn symlink_data_path_and_replaced_directory_are_refused() {
        let root = TempDir::new().unwrap();
        let real = root.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = root.path().join("link");
        symlink(&real, &link).unwrap();
        assert!(prepare_receiver_history(&link).is_err());
        let path = root.path().join("receiver");
        let receiver = prepare_receiver_history(&path).unwrap();
        fs::rename(&path, root.path().join("preserved-receiver")).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(receiver.verify_or_bind(Uuid::new_v4()).is_err());
        assert!(!path.join(STANDBY_BINDING_FILE).exists());
    }

    #[test]
    fn metadata_role_conflicts_and_malformed_primary_are_not_repaired() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join(PRIMARY_HISTORY_FILE), b"broken").unwrap();
        assert!(load_or_create_primary_history(root.path()).is_err());
        assert!(prepare_receiver_history(root.path()).is_err());
        assert_eq!(fs::read(root.path().join(PRIMARY_HISTORY_FILE)).unwrap(), b"broken");
        let receiver = root.path().join("receiver");
        fresh_receiver_with_database(&receiver)
            .verify_or_bind(Uuid::new_v4())
            .unwrap();
        assert!(load_or_create_primary_history(&receiver).is_err());
        assert!(!receiver.join(PRIMARY_HISTORY_FILE).exists());
    }

    #[test]
    fn in_memory_binding_is_per_instance_and_immutable() {
        let first = ReceiverHistory::in_memory();
        let second = ReceiverHistory::in_memory();
        let history_a = Uuid::new_v4();
        let history_b = Uuid::new_v4();
        first.verify_or_bind(history_a).unwrap();
        first.verify_or_bind(history_a).unwrap();
        assert!(matches!(
            first.verify_or_bind(history_b),
            Err(HistoryError::Mismatch { .. })
        ));
        assert_eq!(second.expected_history().unwrap(), None);
        second.verify_or_bind(history_b).unwrap();
        assert_eq!(second.expected_history().unwrap(), Some(history_b));
    }

    #[test]
    fn retained_history_files_cannot_authorize_an_in_place_database_replacement() {
        let primary = TempDir::new().unwrap();
        simulate_database_open(primary.path());
        load_or_create_primary_history(primary.path()).unwrap();
        let marker = fs::read(primary.path().join(PRIMARY_HISTORY_FILE)).unwrap();
        simulate_database_open(primary.path());
        assert!(load_or_create_primary_history(primary.path()).is_err());
        assert_eq!(fs::read(primary.path().join(PRIMARY_HISTORY_FILE)).unwrap(), marker);

        let path = primary.path().join("receiver");
        let receiver = fresh_receiver_with_database(&path);
        let history = Uuid::new_v4();
        receiver.verify_or_bind(history).unwrap();
        let binding = fs::read(path.join(STANDBY_BINDING_FILE)).unwrap();
        simulate_database_open(&path);
        assert!(receiver.expected_history().is_err());
        assert!(receiver.verify_or_bind(history).is_err());
        assert!(prepare_receiver_history(&path).is_err());
        assert_eq!(fs::read(path.join(STANDBY_BINDING_FILE)).unwrap(), binding);
    }

    #[test]
    fn storage_identity_missing_symlink_or_oversized_never_authorizes_binding() {
        for kind in ["missing", "oversized", "symlink"] {
            let root = TempDir::new().unwrap();
            let path = root.path().join("receiver");
            let receiver = prepare_receiver_history(&path).unwrap();
            match kind {
                "oversized" => fs::write(path.join("IDENTITY"), vec![b'a'; MAX_STORAGE_IDENTITY_BYTES + 1]).unwrap(),
                "symlink" => {
                    let target = root.path().join("external-identity");
                    fs::write(&target, Uuid::new_v4().to_string()).unwrap();
                    symlink(target, path.join("IDENTITY")).unwrap();
                }
                _ => {}
            }
            assert!(receiver.verify_or_bind(Uuid::new_v4()).is_err(), "{kind}");
            assert!(matches!(receiver.expected_history(), Err(HistoryError::BindingFailed)));
            assert!(!path.join(STANDBY_BINDING_FILE).exists());
            assert!(load_or_create_primary_history(&path).is_err(), "{kind}");
            assert!(!path.join(PRIMARY_HISTORY_FILE).exists());
        }
    }
}
