//! The records a promotion leaves in a data directory, as the ENGINE reads
//! them at every open (spec P5.1 §3.2 step 4, §4 step 3, §11.A).
//!
//! # Why the engine reads them
//!
//! A fenced primary must stay fenced however its directory is opened next —
//! `start` in any role, `repl`, an MCP server, the embedded API, a build
//! without the HA feature (which compiles no `replication` module) or a FIPS
//! build. Checking the records in one CLI entry point would leave every other
//! one a way to bring the superseded primary back writable, which is the
//! split brain the fence exists to prevent. So the storage layer, which every
//! open passes through, reads them:
//!
//! * [`FENCED_FILE`] in a writable open: the engine opens with its write gate
//!   already fenced, with the record's reason — reads are served, every write
//!   is refused (SQLSTATE 25006 on the wire). A record that is there but does
//!   not decode still means a fence happened: the open is refused, naming the
//!   file and the rebuild.
//! * [`PROMOTION_FILE`] in any open: a promotion was interrupted part-way
//!   through its conversion, so the store is neither a standby's nor a
//!   primary's yet. Only `heliosdb-nano promote`, whose own opens are raw
//!   RocksDB opens outside the engine, may touch it; every engine open is
//!   refused, naming the command that finishes it.
//!
//! The HA code (`replication::fence`, `replication::promote`) re-exports these
//! names and writes the records; it reads them through its own hardened
//! reader (`replication::history`), which refuses symlinks and hard links.

use std::path::Path;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The durable record of a fence, in the fenced primary's data directory.
pub const FENCED_FILE: &str = "NANO-FENCED.json";
/// The dirty record of a promotion in progress, in the standby's directory.
pub const PROMOTION_FILE: &str = "NANO-PROMOTION.json";
/// The format both records are written in.
pub const RECORD_FORMAT: u32 = 1;
/// A record is a handful of fixed fields; anything larger is not one.
const MAX_RECORD_BYTES: u64 = 64 * 1024;

/// The content of [`FENCED_FILE`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FencedRecord {
    pub format: u32,
    /// The dataset history the fenced primary served.
    pub history: Uuid,
    /// Its own epoch.
    pub epoch: u64,
    /// The epoch of the promoted standby that fenced it.
    pub fenced_by_epoch: u64,
    /// Unix seconds.
    pub fenced_at: u64,
    /// The promotion that fenced it (its random identity, spec P5.1 §11.C).
    pub by_node: Uuid,
    /// The store's sequence once every write before the fence was durable.
    pub final_sequence: u64,
}

impl FencedRecord {
    /// The fields agree with one another: format 1, a named history and
    /// promotion, and a fence by a higher epoch than the primary's own.
    pub fn validate(&self) -> Result<(), String> {
        if self.format != RECORD_FORMAT
            || self.history.is_nil()
            || self.by_node.is_nil()
            || self.fenced_by_epoch <= self.epoch
        {
            return Err(format!("{FENCED_FILE} is malformed: {self:?}"));
        }
        Ok(())
    }
}

/// Spec P5.1 §4 step 3: what every entry point but `promote` says about a
/// directory carrying [`PROMOTION_FILE`].
pub fn interrupted_promotion_refusal(dir: &Path) -> String {
    format!(
        "promotion interrupted: finish it with `heliosdb-nano promote --data-dir {}`",
        dir.display()
    )
}

/// The rebuild command a fenced primary's directory is named with.
pub fn fenced_rebuild_hint(dir: &Path) -> String {
    format!(
        "rebuild it as a standby: `heliosdb-nano recover --from-primary --primary-host <the new primary's \
         replication port> --replace --data-dir {}`",
        dir.display()
    )
}

fn present(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("{} cannot be inspected: {error}", path.display())),
    }
}

/// Spec P5.1 §11.A: the refusal of an engine open of `dir` while a promotion
/// record is there (or cannot even be looked for), `None` otherwise.
pub(crate) fn promotion_open_refusal(dir: &Path) -> Option<String> {
    match present(&dir.join(PROMOTION_FILE)) {
        Ok(false) => None,
        Ok(true) => Some(interrupted_promotion_refusal(dir)),
        Err(error) => Some(format!("{error}; {}", interrupted_promotion_refusal(dir))),
    }
}

/// Spec P5.1 §11.A: the fence record a WRITABLE engine open of `dir` must
/// honour — `Ok(Some)` when the directory is a fenced primary's (the engine
/// then opens with its write gate fenced), `Ok(None)` when it is not, and the
/// refusal (naming the file and the rebuild) when a record is there that
/// cannot be read: it still means the directory was fenced.
pub(crate) fn fenced_record_at_open(dir: &Path) -> Result<Option<FencedRecord>, String> {
    let path = dir.join(FENCED_FILE);
    if !present(&path)? {
        return Ok(None);
    }
    let unreadable = |why: String| {
        format!(
            "{} carries {FENCED_FILE}, which cannot be read ({why}): it is a fenced primary's directory; {}",
            dir.display(),
            fenced_rebuild_hint(dir)
        )
    };
    let metadata = std::fs::symlink_metadata(&path).map_err(|error| unreadable(error.to_string()))?;
    if !metadata.is_file() || metadata.len() > MAX_RECORD_BYTES {
        return Err(unreadable("not a regular file of a record's size".to_string()));
    }
    let bytes = std::fs::read(&path).map_err(|error| unreadable(error.to_string()))?;
    let record: FencedRecord = serde_json::from_slice(&bytes).map_err(|error| unreadable(error.to_string()))?;
    record.validate().map_err(unreadable)?;
    Ok(Some(record))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn record() -> FencedRecord {
        FencedRecord {
            format: RECORD_FORMAT,
            history: Uuid::new_v4(),
            epoch: 1,
            fenced_by_epoch: 2,
            fenced_at: 1_761_000_000,
            by_node: Uuid::new_v4(),
            final_sequence: 77,
        }
    }

    /// Spec P5.1 §11.A: a writable open honours a fence record, refuses an
    /// unreadable one naming the file and the rebuild, and every open
    /// refuses a promotion record naming `promote`.
    #[test]
    fn the_engine_reads_the_fence_and_refuses_an_interrupted_promotion() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(fenced_record_at_open(dir.path()).unwrap(), None);
        assert_eq!(promotion_open_refusal(dir.path()), None);

        let written = record();
        std::fs::write(dir.path().join(FENCED_FILE), serde_json::to_vec(&written).unwrap()).unwrap();
        assert_eq!(fenced_record_at_open(dir.path()).unwrap(), Some(written.clone()));

        std::fs::write(dir.path().join(FENCED_FILE), b"{").unwrap();
        let refusal = fenced_record_at_open(dir.path()).unwrap_err();
        assert!(
            refusal.contains(FENCED_FILE)
                && refusal.contains("recover --from-primary")
                && refusal.contains("--replace"),
            "{refusal}"
        );
        let malformed = FencedRecord {
            fenced_by_epoch: 1,
            ..written
        };
        std::fs::write(dir.path().join(FENCED_FILE), serde_json::to_vec(&malformed).unwrap()).unwrap();
        assert!(fenced_record_at_open(dir.path()).is_err());

        std::fs::write(dir.path().join(PROMOTION_FILE), b"anything").unwrap();
        let refusal = promotion_open_refusal(dir.path()).unwrap();
        assert!(
            refusal.starts_with("promotion interrupted: finish it with `heliosdb-nano promote --data-dir"),
            "{refusal}"
        );
    }
}
