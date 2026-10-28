//! Shared replication token loading.
//!
//! One owner-only 32-byte file authenticates the replication connection
//! (`physical_wire`, protocol `NPHYS004`): the primary checks it, its standbys
//! present it. Every check lives here so both sides — and the tests — share
//! exactly one loader. The token is never read from TOML or the environment;
//! only the path is configured.
//!
//! Procedure (identical on primary and standby):
//! `head -c 32 /dev/urandom > replication.token && chmod 600 replication.token`

use crate::{Error, Result};
use std::path::Path;

use super::physical_wire::TransferToken;

/// Read and validate a replication token from an owner-only regular file.
///
/// Refuses symlinks (`O_NOFOLLOW`), non-owner files, group/other-readable
/// modes (anything wider than `0600`), multiply hard-linked files, and any file
/// that is not exactly 32 bytes. The read buffer is zeroized, and the file is
/// re-checked after reading so a size change cannot slip through.
#[cfg(feature = "ha-tier1")]
pub fn read_replication_token(path: &Path) -> Result<TransferToken> {
    #[cfg(unix)]
    {
        use std::io::Read;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .map_err(|e| Error::config(format!("cannot open replication token file: {e}")))?;
        let metadata = file.metadata().map_err(|e| Error::io(e.to_string()))?;
        // SAFETY: geteuid takes no pointers and has no preconditions.
        let uid = unsafe { libc::geteuid() };
        if !metadata.is_file()
            || metadata.uid() != uid
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
            || metadata.len() != 32
        {
            return Err(Error::config("replication token must be an owned, nonsymlink, single-link regular file, mode 0600 or stricter, containing exactly 32 raw bytes"));
        }
        let mut bytes = zeroize::Zeroizing::new([0u8; 32]);
        file.read_exact(&mut *bytes).map_err(|e| Error::io(e.to_string()))?;
        let mut extra = [0u8; 1];
        if file.read(&mut extra).map_err(|e| Error::io(e.to_string()))? != 0 {
            return Err(Error::config("replication token file changed size while reading"));
        }
        TransferToken::new(*bytes).map_err(|e| Error::config(e.to_string()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(Error::config(
            "replication token ownership validation currently requires Unix",
        ))
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn write_token(dir: &std::path::Path, mode: u32, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join("replication.token");
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn owner_only_32_byte_file_feeds_both_channels() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_token(dir.path(), 0o600, &[7u8; 32]);
        let token = read_replication_token(&path).unwrap();
        // Both channels consume this one token: the native handshake serializes
        // `wire_bytes()`; the physical wire verifies the same bytes with `accepts`.
        assert_eq!(token.wire_bytes(), [7u8; 32]);
        assert!(token.accepts(&token.wire_bytes()));
    }

    #[test]
    fn world_readable_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_token(dir.path(), 0o644, &[7u8; 32]);
        assert!(read_replication_token(&path).is_err());
    }

    #[test]
    fn symlink_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_token(dir.path(), 0o600, &[7u8; 32]);
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_replication_token(&link).is_err());
    }

    #[test]
    fn wrong_length_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_token(dir.path(), 0o600, &[7u8; 31]);
        assert!(read_replication_token(&path).is_err());
    }
}
