//! The replication epoch as a stored value (spec P5.1 §1, plan
//! `PHYSICAL_STANDBY_REPLICATION.md` §D7).
//!
//! # Why
//!
//! The epoch is Oracle's incarnation and PostgreSQL's timeline: the number
//! that tells a superseded primary from the real one. Until spec P5.1 it was
//! the compile-time constant 1, so it could say nothing — two nodes that both
//! believed they were the primary of a dataset reported the same epoch, and a
//! proxy, a standby or an archive could not tell them apart. It is now a value
//! of the dataset itself: one key, written by the node that is promoted as the
//! FIRST write of its epoch (`replication::physical::PromotionWriter`),
//! replicated verbatim to every standby, carried by every checkpoint and every
//! copy. A node's epoch is therefore what its store says, never what its
//! process believes.
//!
//! # The key
//!
//! [`EPOCH_KEY`] (`repl:epoch`) in the DEFAULT (replicated) column family,
//! classified `KeyClass::ReplEpoch`. Its value is an [`EpochRecord`] as JSON,
//! at most [`MAX_EPOCH_RECORD_BYTES`]. A store without the key is at
//! [`INITIAL_EPOCH`] (every dataset before its first promotion). The value is
//! stored in plain JSON, like the logical WAL's watermarks: it holds no user
//! data, and the readers that must see it hold no dataset key — `heliosdb-nano
//! status`, the archive tools, and the offline `promote` of an encrypted
//! standby, which never needs the key either (physical replication ships
//! ciphertext verbatim).
//!
//! # Readers
//!
//! The engine reads it at every open, in every mode
//! (`StorageEngine::replication_epoch`), and caches it; a standby's applier
//! updates the cache when an applied batch carries the key. Every surface that
//! reports or compares an epoch reads it from the store it opened: the
//! primary's identity on its replication port, a standby's `Hello`, a
//! cascading standby's identity, `resync`, the archiver's segment header, the
//! archive readers, `recover`, `pg_control_checkpoint()`, `/health` and
//! `heliosdb_standby_status`. [`INITIAL_EPOCH`] remains only as the default of
//! an absent key and the fallback of a directory that holds no store.
//!
//! # Where it lives
//!
//! In the storage layer, compiled in every build: the engine reads it at
//! every open and the SQL layer reports it, also in a build without an HA
//! feature (which has no `replication` module). `replication::epoch`
//! re-exports it beside the code that writes and compares it.

use std::path::Path;

use rocksdb::{Options, DB};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The key the epoch is stored under, in the default column family.
pub const EPOCH_KEY: &[u8] = b"repl:epoch";
/// The epoch of a dataset that was never promoted (the key is absent).
pub const INITIAL_EPOCH: u64 = 1;
/// The bound of one stored record: a handful of fixed fields.
pub const MAX_EPOCH_RECORD_BYTES: usize = 1024;
const FORMAT: u32 = 1;

/// Why an epoch could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum EpochError {
    #[error("replication epoch storage: {0}")]
    Storage(#[from] rocksdb::Error),
    #[error("replication epoch record `repl:epoch` refused: {0}")]
    Invalid(String),
    #[error("replication epoch: {0}")]
    Io(#[from] std::io::Error),
}

fn invalid(message: impl Into<String>) -> EpochError {
    EpochError::Invalid(message.into())
}

/// The value of [`EPOCH_KEY`]: which epoch the dataset is at, which it came
/// from, when and by which node it was promoted, and whether the old primary
/// was fenced first (and at which of its sequences).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpochRecord {
    pub format: u32,
    /// The dataset's epoch (at least 2: epoch 1 is the absent key).
    pub epoch: u64,
    /// The epoch it was promoted from (`epoch - 1` for every promotion this
    /// release makes).
    pub previous: u64,
    /// Unix seconds of the promotion.
    pub promoted_at: u64,
    /// The node that was promoted (its replication identity: what the fence
    /// named the old primary's fencer by).
    pub node_id: Uuid,
    /// The old primary acknowledged the fence before the promotion: nothing
    /// it committed is missing here. False for `promote --force`.
    pub fenced: bool,
    /// The old primary's last sequence when it was fenced (its numbering);
    /// `None` when it was not fenced.
    pub old_primary_final_sequence: Option<u64>,
}

impl EpochRecord {
    /// The record of a promotion from `previous` to `previous + 1`.
    pub fn promotion(
        previous: u64,
        node_id: Uuid,
        promoted_at: u64,
        old_primary_final_sequence: Option<u64>,
    ) -> Result<Self, EpochError> {
        let epoch = previous
            .checked_add(1)
            .ok_or_else(|| invalid("the epoch counter is exhausted"))?;
        let record = Self {
            format: FORMAT,
            epoch,
            previous,
            promoted_at,
            node_id,
            fenced: old_primary_final_sequence.is_some(),
            old_primary_final_sequence,
        };
        record.validate()?;
        Ok(record)
    }

    /// The fields agree with one another: format 1, an epoch past the
    /// initial one and past its predecessor, a named node, and a final
    /// sequence exactly when the old primary was fenced.
    pub fn validate(&self) -> Result<(), EpochError> {
        if self.format != FORMAT {
            return Err(invalid(format!(
                "format {} (this release reads format {FORMAT})",
                self.format
            )));
        }
        if self.epoch <= INITIAL_EPOCH
            || self.previous < INITIAL_EPOCH
            || self.previous >= self.epoch
            || self.node_id.is_nil()
            || self.fenced != self.old_primary_final_sequence.is_some()
        {
            return Err(invalid("its fields disagree with one another"));
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, EpochError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|error| invalid(error.to_string()))?;
        if bytes.len() > MAX_EPOCH_RECORD_BYTES {
            return Err(invalid("oversized record"));
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, EpochError> {
        if bytes.len() > MAX_EPOCH_RECORD_BYTES {
            return Err(invalid("oversized record"));
        }
        let record: Self = serde_json::from_slice(bytes).map_err(|error| invalid(format!("malformed: {error}")))?;
        record.validate()?;
        Ok(record)
    }
}

/// The epoch a record says, [`INITIAL_EPOCH`] for an absent one.
pub fn epoch_of(record: Option<&EpochRecord>) -> u64 {
    record.map_or(INITIAL_EPOCH, |record| record.epoch)
}

/// The epoch a record was promoted from, [`INITIAL_EPOCH`] for an absent one
/// (PostgreSQL's `prev_timeline_id` of a timeline-1 cluster is 1).
pub fn previous_of(record: Option<&EpochRecord>) -> u64 {
    record.map_or(INITIAL_EPOCH, |record| record.previous)
}

/// The record `db`'s default column family holds, if any. A value that does
/// not decode is an error, never a guess: a store whose epoch cannot be read
/// cannot say which primary it follows.
pub fn read_epoch_record(db: &DB) -> Result<Option<EpochRecord>, EpochError> {
    db.get(EPOCH_KEY)?.map(|bytes| EpochRecord::decode(&bytes)).transpose()
}

/// `db`'s epoch ([`INITIAL_EPOCH`] without the key).
pub fn read_epoch(db: &DB) -> Result<u64, EpochError> {
    Ok(epoch_of(read_epoch_record(db)?.as_ref()))
}

/// The record of the store at `path`, read without the engine and without
/// any lock: every column family opened read-only (so a standby's directory,
/// which holds the node-local family, opens too), RocksDB's `LOG` redirected
/// to a private temporary directory so the store's own directory is not
/// touched. `Ok(None)` when the store holds no record; an error when `path`
/// holds no store. For `heliosdb-nano status`, `recover`, a standby's
/// archive and the archive tools. Spec P5.1 §11.K: a point read, so the open
/// is a cheap one — table files opened on demand (64 at most), no statistics
/// loaded, no file sizes checked — not the engine's full open.
pub fn read_directory_epoch(path: &Path) -> Result<Option<EpochRecord>, EpochError> {
    let families = DB::list_cf(&Options::default(), path)?;
    let log = tempfile::Builder::new().prefix("nano-epoch-read-").tempdir()?;
    let mut options = Options::default();
    options.set_db_log_dir(log.path());
    options.set_max_open_files(64);
    options.set_skip_stats_update_on_db_open(true);
    options.set_skip_checking_sst_file_sizes_on_db_open(true);
    let db = DB::open_cf_for_read_only(&options, path, families, false)?;
    let record = read_epoch_record(&db);
    drop(db);
    drop(log);
    record
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn record() -> EpochRecord {
        EpochRecord::promotion(1, Uuid::new_v4(), 1_760_000_000, Some(4_242)).unwrap()
    }

    #[test]
    fn a_record_round_trips_and_fits_its_bound() {
        let record = record();
        assert_eq!(record.epoch, 2);
        assert_eq!(record.previous, 1);
        assert!(record.fenced);
        let bytes = record.encode().unwrap();
        assert!(bytes.len() <= MAX_EPOCH_RECORD_BYTES);
        assert_eq!(EpochRecord::decode(&bytes).unwrap(), record);
        let forced = EpochRecord::promotion(2, Uuid::new_v4(), 1, None).unwrap();
        assert_eq!((forced.epoch, forced.previous, forced.fenced), (3, 2, false));
        assert_eq!(EpochRecord::decode(&forced.encode().unwrap()).unwrap(), forced);
    }

    #[test]
    fn an_absent_record_is_the_initial_epoch() {
        assert_eq!(epoch_of(None), INITIAL_EPOCH);
        assert_eq!(previous_of(None), INITIAL_EPOCH);
        let dir = tempfile::tempdir().unwrap();
        let mut options = Options::default();
        options.create_if_missing(true);
        let db = DB::open(&options, dir.path()).unwrap();
        assert_eq!(read_epoch(&db).unwrap(), INITIAL_EPOCH);
        db.put(EPOCH_KEY, record().encode().unwrap()).unwrap();
        assert_eq!(read_epoch(&db).unwrap(), 2);
        drop(db);
        assert_eq!(read_directory_epoch(dir.path()).unwrap().map(|r| r.epoch), Some(2));
    }

    #[test]
    fn a_record_of_another_format_or_with_disagreeing_fields_is_refused() {
        let mut other = record();
        other.format = 2;
        let bytes = serde_json::to_vec(&other).unwrap();
        let error = EpochRecord::decode(&bytes).unwrap_err().to_string();
        assert!(error.contains("format 2"), "{error}");
        for broken in [
            EpochRecord {
                epoch: 1,
                previous: 1,
                ..record()
            },
            EpochRecord {
                previous: 2,
                ..record()
            },
            EpochRecord {
                node_id: Uuid::nil(),
                ..record()
            },
            EpochRecord {
                fenced: false,
                ..record()
            },
        ] {
            assert!(
                EpochRecord::decode(&serde_json::to_vec(&broken).unwrap()).is_err(),
                "{broken:?}"
            );
        }
        assert!(EpochRecord::decode(b"{\"format\":1}").is_err());
        assert!(EpochRecord::decode(&vec![b' '; MAX_EPOCH_RECORD_BYTES + 1]).is_err());
        let dir = tempfile::tempdir().unwrap();
        let mut options = Options::default();
        options.create_if_missing(true);
        let db = DB::open(&options, dir.path()).unwrap();
        db.put(EPOCH_KEY, b"not json").unwrap();
        assert!(
            read_epoch(&db).is_err(),
            "an unreadable epoch is an error, never epoch 1"
        );
    }
}
