//! Spec R3b (Sprinter bbba90ddfc61): a node's replication role as a
//! `SWITCHOVER` (or a promotion, or a rebuild) left it, recorded BESIDE its
//! data directory and bound to that directory.
//!
//! # Why
//!
//! A switchover changes two nodes' roles at runtime: the old primary is
//! demoted into a standby of the target, the target promoted. Their
//! configuration files still say what they were (`[replication] role`,
//! `primary_host`), and a restart must start each as what it is now — a
//! demoted primary's directory is a physical standby's (a primary start of it
//! is refused), a promoted standby's a primary's (a standby start of it is
//! refused). The operator's configuration file is never edited (it may be
//! generated, read-only, shared): the switchover writes this override
//! instead, and `start` applies it over the configuration file, said at WARN.
//!
//! # Bound to its directory (review item 2)
//!
//! The override records the directory it was written for: its dataset
//! history, its epoch and its store's identity (RocksDB's `IDENTITY`). `start`
//! applies it only to that directory — never to a directory rebuilt,
//! restored or wiped since (it refuses, naming the file): an old override
//! must never turn a NEW, empty directory into a writable primary of a new
//! history. A command line whose `--replication-role` or `--primary-host`
//! contradicts it is refused too, naming the file (never silently overridden
//! either way). `promote`, `pg_promote` and `recover` rewrite an override
//! they find (the role they leave the directory in, its new binding), so one
//! never goes stale behind them. A standby that followed over TLS records it
//! (`primary_tls`, its CA and server name): a restart never drops to
//! plaintext.
//!
//! # The file
//!
//! `<data dir>.role.toml` — a sibling of the data directory (never inside it:
//! a standby's directory holds only its store's files), mode 0600, e.g.
//!
//! ```toml
//! # SWITCHOVER …: this node, the primary of epoch 1, was demoted into a standby of 10.0.0.6:5433 (epoch 2)
//! # Written by HeliosDB-Nano at 2026-10-30T09:12:44Z: …
//! [replication]
//! role = "standby"
//! primary_host = "10.0.0.6:5433"
//! primary_tls = true
//! primary_tls_ca_path = "/etc/heliosdb/ca.pem"
//!
//! [directory]
//! history = "6f1c…"
//! epoch = 2
//! storage_identity = "f3b0…"
//! ```
//!
//! Delete the file to go back to the configuration file's role (a directory
//! still starts only in the role its content allows). It is written whole
//! (temporary, fsync, rename, directory fsync), so a crash leaves the
//! previous file or the new one.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The suffix of the override's file name: `<data dir name>.role.toml`.
pub const ROLE_OVERRIDE_SUFFIX: &str = "role.toml";
/// A record is a few short keys and a comment; anything larger is not one.
const MAX_OVERRIDE_BYTES: u64 = 16 * 1024;
/// RocksDB's `IDENTITY` is a short line.
const MAX_IDENTITY_BYTES: u64 = 256;

/// What the override says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleOverride {
    pub replication: ReplicationRole,
    /// The directory it was written for.
    pub directory: DirectoryBinding,
    /// Why it was written (the file's comment line; not a TOML key).
    #[serde(skip)]
    pub reason: String,
}

/// The `[replication]` keys the override sets.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplicationRole {
    /// `"primary"` or `"standby"`.
    pub role: String,
    /// The primary a standby follows (its replication port); none for a
    /// primary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_host: Option<String>,
    /// A standby whose pair replicated over TLS follows over TLS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_tls: Option<bool>,
    /// The CA it verifies its primary's certificate with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_tls_ca_path: Option<PathBuf>,
    /// The server name it verifies, when not its primary's host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_tls_server_name: Option<String>,
}

/// The directory an override was written for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryBinding {
    /// Its dataset history (a primary's history record, or a physical
    /// standby's marker).
    pub history: Uuid,
    /// Its store's replication epoch.
    pub epoch: u64,
    /// Its store's identity (RocksDB's `IDENTITY`): a directory rebuilt by a
    /// full copy, restored or re-created has another.
    pub storage_identity: String,
}

impl RoleOverride {
    /// The override of a node promoted to the primary of `directory`.
    pub fn primary(directory: DirectoryBinding, reason: impl Into<String>) -> Self {
        Self {
            replication: ReplicationRole {
                role: "primary".to_string(),
                ..ReplicationRole::default()
            },
            directory,
            reason: reason.into(),
        }
    }

    /// The override of a node that is a standby of `primary_host`.
    /// `tls`: it follows over TLS, with that CA and server name.
    pub fn standby(
        primary_host: impl Into<String>,
        tls: Option<(Option<PathBuf>, Option<String>)>,
        directory: DirectoryBinding,
        reason: impl Into<String>,
    ) -> Self {
        let (primary_tls, primary_tls_ca_path, primary_tls_server_name) = match tls {
            Some((ca, server_name)) => (Some(true), ca, server_name),
            None => (None, None, None),
        };
        Self {
            replication: ReplicationRole {
                role: "standby".to_string(),
                primary_host: Some(primary_host.into()),
                primary_tls,
                primary_tls_ca_path,
                primary_tls_server_name,
            },
            directory,
            reason: reason.into(),
        }
    }

    fn validate(&self) -> Result<(), String> {
        let role = &self.replication;
        let shape = match (role.role.as_str(), &role.primary_host) {
            ("primary", None) => role.primary_tls.is_none() && role.primary_tls_ca_path.is_none(),
            ("standby", Some(host)) => !host.trim().is_empty() && host.len() <= 512,
            _ => false,
        };
        if !shape {
            return Err(format!(
                "a role override is role = \"primary\" (no primary_host, no primary TLS) or role = \"standby\" with \
                 its primary_host, not {role:?}"
            ));
        }
        let directory = &self.directory;
        if directory.history.is_nil()
            || directory.epoch == 0
            || directory.storage_identity.is_empty()
            || directory.storage_identity.len() > 128
        {
            return Err(format!("a role override's [directory] is not one: {directory:?}"));
        }
        Ok(())
    }
}

/// `path` with its symbolic links resolved — the directory itself when it
/// exists; a dangling link followed to where it points (a wiped directory
/// behind a link still names the same place); otherwise its parent resolved
/// and its name kept — falling back to the absolute path.
fn resolved(absolute: &Path) -> PathBuf {
    let mut current = absolute.to_path_buf();
    for _ in 0..32 {
        if let Ok(canonical) = std::fs::canonicalize(&current) {
            return canonical;
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => match std::fs::read_link(&current) {
                Ok(target) => {
                    current = if target.is_absolute() {
                        target
                    } else {
                        current.parent().map_or(target.clone(), |parent| parent.join(&target))
                    };
                    continue;
                }
                Err(_) => break,
            },
            _ => break,
        }
    }
    match (current.parent(), current.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent).map_or(current.clone(), |parent| parent.join(name)),
        _ => current,
    }
}

/// `<data dir>.role.toml` beside `data_dir` — beside the directory its path
/// RESOLVES to (review: every writer and reader resolves symbolic links the
/// same way, so a `--data-dir` given through a link and one given
/// canonically name the same record).
pub fn role_override_path(data_dir: &Path) -> Result<PathBuf, String> {
    let absolute = if data_dir.is_absolute() {
        data_dir.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("cannot resolve {}: {error}", data_dir.display()))?
            .join(data_dir)
    };
    let absolute = resolved(&absolute);
    let (Some(parent), Some(name)) = (absolute.parent(), absolute.file_name()) else {
        return Err(format!(
            "{} has no parent directory to record its role beside",
            data_dir.display()
        ));
    };
    let mut file = name.to_os_string();
    file.push(".");
    file.push(ROLE_OVERRIDE_SUFFIX);
    Ok(parent.join(file))
}

/// The override beside `data_dir`, `None` when there is none. One that is
/// there but does not read is an error: it means a switchover changed the
/// node's role, and guessing the role would be worse than refusing to start.
pub fn read_role_override(data_dir: &Path) -> Result<Option<RoleOverride>, String> {
    let path = role_override_path(data_dir)?;
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{} cannot be inspected: {error}", path.display())),
    };
    if !metadata.is_file() || metadata.len() > MAX_OVERRIDE_BYTES {
        return Err(format!(
            "{} is not a regular file of a role override's size",
            path.display()
        ));
    }
    let text = std::fs::read_to_string(&path).map_err(|error| format!("{} cannot be read: {error}", path.display()))?;
    let mut record: RoleOverride =
        toml::from_str(&text).map_err(|error| format!("{} is not a role override: {error}", path.display()))?;
    record
        .validate()
        .map_err(|error| format!("{}: {error}", path.display()))?;
    record.reason = text
        .lines()
        .find_map(|line| line.strip_prefix("# "))
        .unwrap_or_default()
        .to_string();
    Ok(Some(record))
}

/// Write `record` beside `data_dir`, replacing any earlier one, whole: a
/// temporary file (mode 0600) fsynced, renamed over the old one, the
/// directory fsynced.
pub fn write_role_override(data_dir: &Path, record: &RoleOverride) -> Result<PathBuf, String> {
    record.validate()?;
    let path = role_override_path(data_dir)?;
    let body = toml::to_string(record).map_err(|error| format!("the role override cannot be written: {error}"))?;
    let comment: String = record
        .reason
        .chars()
        .map(|character| if character.is_control() { ' ' } else { character })
        .collect();
    let text = format!(
        "# {comment}\n# Written by HeliosDB-Nano at {}. `start` applies these keys over the configuration file, \
         only to the directory [directory] names; a command line that contradicts them is refused. Delete this \
         file to go back to the configuration file's role.\n{body}",
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
    );
    let temporary = path.with_extension("toml.tmp");
    let written = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        // An earlier temporary left with wider permissions is narrowed.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, &path)?;
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("{} cannot be written: {error}", path.display()));
    }
    Ok(path)
}

/// The identity of the store in `data_dir` (RocksDB's `IDENTITY`), `None`
/// when it holds none.
fn storage_identity(data_dir: &Path) -> Result<Option<String>, String> {
    let file = data_dir.join("IDENTITY");
    let metadata = match std::fs::symlink_metadata(&file) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{} cannot be inspected: {error}", file.display())),
    };
    if !metadata.is_file() || metadata.len() > MAX_IDENTITY_BYTES {
        return Err(format!("{} is not a store's identity", file.display()));
    }
    let text = std::fs::read_to_string(&file).map_err(|error| format!("{} cannot be read: {error}", file.display()))?;
    let identity = text.trim().to_string();
    if identity.is_empty() {
        return Err(format!("{} is empty", file.display()));
    }
    Ok(Some(identity))
}

/// The dataset history `data_dir` carries: a primary's history record, or a
/// physical standby's marker.
fn directory_history(data_dir: &Path) -> Result<Uuid, String> {
    if let Some(history) = crate::replication::history::read_primary_history(data_dir)
        .map_err(|error| format!("the dataset history of {} cannot be read: {error}", data_dir.display()))?
    {
        return Ok(history);
    }
    let marker = data_dir.join(crate::replication::checkpoint_transfer::PHYSICAL_MARKER_FILE);
    if std::fs::symlink_metadata(&marker).is_ok() {
        return crate::replication::checkpoint_transfer::physical_stage_history(data_dir).map_err(|error| {
            format!(
                "the physical standby marker of {} cannot be read: {error}",
                data_dir.display()
            )
        });
    }
    Err(format!(
        "{} carries no dataset history (no primary history record, no physical standby marker)",
        data_dir.display()
    ))
}

/// The binding of `data_dir` at `epoch` (the caller knows its epoch: a
/// promotion's or a rewind's), for an override about to be written.
pub fn binding_at(data_dir: &Path, epoch: u64) -> Result<DirectoryBinding, String> {
    let storage_identity =
        storage_identity(data_dir)?.ok_or_else(|| format!("{} holds no store (no IDENTITY)", data_dir.display()))?;
    Ok(DirectoryBinding {
        history: directory_history(data_dir)?,
        epoch,
        storage_identity,
    })
}

/// The binding of the CLOSED store in `data_dir` as it is now — its epoch
/// read from the store — or `None` when the directory is missing, empty, or
/// holds no store.
pub fn current_binding(data_dir: &Path) -> Result<Option<DirectoryBinding>, String> {
    match std::fs::read_dir(data_dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{} cannot be listed: {error}", data_dir.display())),
        Ok(mut entries) => {
            if entries.next().is_none() {
                return Ok(None);
            }
        }
    }
    let Some(storage_identity) = storage_identity(data_dir)? else {
        return Ok(None);
    };
    let epoch = crate::replication::epoch::read_directory_epoch(data_dir)
        .map_err(|error| format!("the epoch of {} cannot be read: {error}", data_dir.display()))?;
    Ok(Some(DirectoryBinding {
        history: directory_history(data_dir)?,
        epoch: crate::replication::epoch::epoch_of(epoch.as_ref()),
        storage_identity,
    }))
}

/// What `start` makes of an override: applied, or refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartOverride {
    /// No override beside the directory.
    None,
    /// Applied: the role (and primary, and TLS) `start` uses.
    Apply(RoleOverride),
    /// The start is refused; why, naming the file.
    Refuse(String),
}

/// Review item 2: whether the override beside `data_dir` applies to this
/// start. `cli_role` / `cli_primary_host`: what the command line said
/// explicitly (a daemon child is forwarded the role its parent resolved,
/// which agrees). Refused: an override that does not read; a directory that
/// is missing, empty or holds no store (a new node is never started from an
/// old node's role); a directory that is not the one it was written for
/// (another history, epoch or store identity: rebuilt, restored); a command
/// line that contradicts it.
pub fn for_start(data_dir: &Path, cli_role: Option<&str>, cli_primary_host: Option<&str>) -> StartOverride {
    let record = match read_role_override(data_dir) {
        Ok(Some(record)) => record,
        Ok(None) => return StartOverride::None,
        Err(error) => return StartOverride::Refuse(error),
    };
    let file = role_override_path(data_dir)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| format!("{}.{ROLE_OVERRIDE_SUFFIX}", data_dir.display()));
    let delete = format!("delete {file} to start this directory with the role its configuration and command line say");
    match current_binding(data_dir) {
        Ok(None) => {
            return StartOverride::Refuse(format!(
                "{file} records the role ({}) a switchover or promotion gave the node whose data directory is {}, but \
                 that directory is missing, empty or holds no store: an old node's role is never given to a new \
                 directory (it could start a new, writable primary of a new dataset); {delete}",
                record.replication.role,
                data_dir.display()
            ))
        }
        Ok(Some(binding)) if binding != record.directory => {
            return StartOverride::Refuse(format!(
                "{file} was written for another directory than {} holds now (history {} epoch {} store {}; it \
                 records history {} epoch {} store {}): the directory was rebuilt, restored or recovered since; \
                 {delete}",
                data_dir.display(),
                binding.history,
                binding.epoch,
                binding.storage_identity,
                record.directory.history,
                record.directory.epoch,
                record.directory.storage_identity
            ))
        }
        Ok(Some(_)) => {}
        Err(error) => {
            return StartOverride::Refuse(format!(
                "{file} cannot be checked against {} ({error}); {delete}",
                data_dir.display()
            ))
        }
    }
    if let Some(role) = cli_role.map(str::trim).filter(|role| !role.is_empty()) {
        if !role.eq_ignore_ascii_case(&record.replication.role) {
            return StartOverride::Refuse(format!(
                "--replication-role {role} contradicts {file}, which records that this node is a {} ({}): start it \
                 without --replication-role (or with --replication-role {}), or {delete}",
                record.replication.role, record.reason, record.replication.role
            ));
        }
    }
    if let (Some(host), Some(recorded)) = (
        cli_primary_host.map(str::trim).filter(|host| !host.is_empty()),
        record.replication.primary_host.as_deref(),
    ) {
        if host != recorded {
            return StartOverride::Refuse(format!(
                "--primary-host {host} contradicts {file}, which records that this node follows {recorded} ({}): \
                 start it without --primary-host (or with --primary-host {recorded}), or {delete}",
                record.reason
            ));
        }
    }
    StartOverride::Apply(record)
}

/// Review item 2: `promote`, `pg_promote` and `recover` rewrite an override
/// they find beside `data_dir` (never create one): `record` is the role they
/// leave the directory in. `Ok(None)` when there was none.
pub fn rewrite_if_present(data_dir: &Path, record: &RoleOverride) -> Result<Option<PathBuf>, String> {
    let path = role_override_path(data_dir)?;
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{} cannot be inspected: {error}", path.display())),
        Ok(_) => write_role_override(data_dir, record).map(Some),
    }
}

/// Whether an override exists beside `data_dir` (read or not).
pub fn exists(data_dir: &Path) -> bool {
    role_override_path(data_dir).is_ok_and(|path| std::fs::symlink_metadata(path).is_ok())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn binding() -> DirectoryBinding {
        DirectoryBinding {
            history: Uuid::new_v4(),
            epoch: 2,
            storage_identity: "f3b0c442-98fc-1c14-b39f-92b4d2e3e6a1".into(),
        }
    }

    #[test]
    fn a_role_override_round_trips_beside_the_data_directory_owner_only() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("node");
        assert_eq!(read_role_override(&data).unwrap(), None);
        let standby = RoleOverride::standby(
            "10.0.0.6:5433",
            Some((Some(PathBuf::from("/etc/heliosdb/ca.pem")), None)),
            binding(),
            "SWITCHOVER abc: demoted",
        );
        let path = write_role_override(&data, &standby).unwrap();
        assert_eq!(path, root.path().join("node.role.toml"));
        let read = read_role_override(&data).unwrap().unwrap();
        assert_eq!(read.replication, standby.replication);
        assert_eq!(read.replication.primary_tls, Some(true));
        assert_eq!(read.directory, standby.directory);
        assert!(read.reason.contains("SWITCHOVER abc"), "{}", read.reason);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let primary = RoleOverride::primary(binding(), "promoted");
        write_role_override(&data, &primary).unwrap();
        assert_eq!(
            read_role_override(&data).unwrap().unwrap().replication,
            primary.replication
        );
        assert!(!root.path().join("node.role.toml.tmp").exists());
    }

    #[test]
    fn a_role_override_that_does_not_read_is_an_error() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("node");
        let path = role_override_path(&data).unwrap();
        let directory = format!(
            "[directory]\nhistory = \"{}\"\nepoch = 2\nstorage_identity = \"x\"\n",
            Uuid::new_v4()
        );
        std::fs::write(&path, format!("[replication]\nrole = \"standby\"\n{directory}")).unwrap();
        assert!(read_role_override(&data).is_err(), "a standby names its primary");
        std::fs::write(
            &path,
            format!("[replication]\nrole = \"primary\"\nport = 1\n{directory}"),
        )
        .unwrap();
        assert!(read_role_override(&data).is_err(), "only its keys exist");
        std::fs::write(&path, "[replication]\nrole = \"primary\"\n").unwrap();
        assert!(read_role_override(&data).is_err(), "an override names its directory");
        std::fs::write(&path, "not toml").unwrap();
        assert!(read_role_override(&data).is_err());
        assert!(write_role_override(
            &data,
            &RoleOverride {
                replication: ReplicationRole {
                    role: "observer".into(),
                    ..ReplicationRole::default()
                },
                directory: binding(),
                reason: String::new()
            }
        )
        .is_err());
    }

    /// Review (third pass, item 1): a data directory given through a
    /// symbolic link and given canonically name the same record — written
    /// through one, read through the other — and a link left dangling by a
    /// wiped directory still finds it (so the start is refused, not started
    /// as a new node).
    #[cfg(unix)]
    #[test]
    fn a_symlinked_data_directory_and_its_target_name_the_same_record() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("ssd").join("hdb");
        std::fs::create_dir_all(&real).unwrap();
        let link_parent = root.path().join("var-lib");
        std::fs::create_dir_all(&link_parent).unwrap();
        let link = link_parent.join("hdb");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let canonical_real = std::fs::canonicalize(&real).unwrap();
        let expected = canonical_real.parent().unwrap().join("hdb.role.toml");
        assert_eq!(role_override_path(&link).unwrap(), expected);
        assert_eq!(role_override_path(&real).unwrap(), expected);
        let record = RoleOverride::primary(binding(), "promoted");
        write_role_override(&link, &record).unwrap();
        assert!(expected.is_file());
        assert_eq!(
            read_role_override(&real).unwrap().unwrap().replication,
            record.replication
        );
        // A relative link resolves against its own directory.
        let relative = link_parent.join("hdb-relative");
        std::os::unix::fs::symlink(std::path::Path::new("../ssd/hdb"), &relative).unwrap();
        assert_eq!(role_override_path(&relative).unwrap(), expected);
        // The directory wiped behind the link: the record is still found,
        // and the start is refused naming it.
        std::fs::remove_dir_all(&real).unwrap();
        assert_eq!(role_override_path(&link).unwrap(), expected);
        match for_start(&link, None, None) {
            StartOverride::Refuse(reason) => assert!(reason.contains("missing, empty or holds no store"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    /// Review item 2: an override applies only to the directory it was
    /// written for, and a command line that contradicts it is refused.
    #[test]
    fn an_override_is_applied_only_to_its_own_directory_and_never_against_the_command_line() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("node");
        assert_eq!(for_start(&data, None, None), StartOverride::None);
        // The directory is gone (wiped): refused, whatever the role.
        let record = RoleOverride::primary(binding(), "promoted");
        write_role_override(&data, &record).unwrap();
        match for_start(&data, Some("standby"), None) {
            StartOverride::Refuse(reason) => {
                assert!(reason.contains("missing, empty or holds no store"), "{reason}");
                assert!(reason.contains("node.role.toml"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        std::fs::create_dir_all(&data).unwrap();
        assert!(
            matches!(for_start(&data, None, None), StartOverride::Refuse(_)),
            "an empty directory"
        );
        // A store of another identity (no epoch record: epoch 1, a store
        // the read can open): refused as another directory.
        let db = rocksdb::DB::open_default(&data).unwrap();
        drop(db);
        let history = Uuid::new_v4();
        crate::replication::history::write_primary_history(&data, history).unwrap();
        let actual = match current_binding(&data) {
            Ok(Some(binding)) => binding,
            other => panic!("{other:?}"),
        };
        assert_eq!((actual.history, actual.epoch), (history, 1));
        match for_start(&data, None, None) {
            StartOverride::Refuse(reason) => assert!(reason.contains("written for another directory"), "{reason}"),
            other => panic!("{other:?}"),
        }
        // Its own directory: applied; a contradicting command line refused.
        let standby = RoleOverride::standby("127.0.0.1:5433", None, actual.clone(), "demoted");
        write_role_override(&data, &standby).unwrap();
        assert!(matches!(for_start(&data, None, None), StartOverride::Apply(_)));
        assert!(matches!(
            for_start(&data, Some("standby"), Some("127.0.0.1:5433")),
            StartOverride::Apply(_)
        ));
        match for_start(&data, Some("primary"), None) {
            StartOverride::Refuse(reason) => {
                assert!(reason.contains("--replication-role primary contradicts"), "{reason}")
            }
            other => panic!("{other:?}"),
        }
        match for_start(&data, None, Some("10.0.0.9:5433")) {
            StartOverride::Refuse(reason) => {
                assert!(reason.contains("--primary-host 10.0.0.9:5433 contradicts"), "{reason}")
            }
            other => panic!("{other:?}"),
        }
        // Only an existing override is rewritten.
        let elsewhere = root.path().join("other");
        assert_eq!(rewrite_if_present(&elsewhere, &standby).unwrap(), None);
        assert!(!exists(&elsewhere));
        assert!(rewrite_if_present(&data, &RoleOverride::primary(actual, "promoted"))
            .unwrap()
            .is_some());
        assert_eq!(read_role_override(&data).unwrap().unwrap().replication.role, "primary");
    }
}
