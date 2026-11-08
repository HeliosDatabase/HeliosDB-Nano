//! Logical WAL is a history of atomic data groups, never automatic redo.
//! Unclassified legacy redo needs explicit offline adoption of local state.

use super::{time_travel::bounded_read_options, WriteGate};
use crate::{Error, Result};
use rocksdb::{WriteBatch, WriteOptions, DB};

pub const ATOMIC_GROUPS_KEY: &[u8] = b"wal:atomic_groups";
const ATOMIC_GROUPS_V1: &[u8] = &[1];
const LAST_LSN_KEY: &[u8] = b"wal:last_lsn";
const CHECKPOINT_KEY: &[u8] = b"wal:checkpoint";
const PRUNED_THROUGH_KEY: &[u8] = b"wal:pruned_through";
const ENTRY_PREFIX: &[u8] = b"wal:entries:";

/// Metadata changed by an explicit offline adoption. Retained entries and every
/// data/catalog/history/branch key stay byte-for-byte intact.
#[derive(Debug, Clone, Copy)]
pub struct WalAdoptionReport {
    /// The redo/application floor before this explicit migration.
    pub previous_checkpoint: Option<u64>,
    /// Highest durable metadata/retained-entry LSN adopted without redo.
    pub adopted_through: u64,
    /// Preserved old logical-history floor; retained entries are untouched.
    pub history_floor: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct State {
    pub atomic_groups: bool,
    pub checkpoint: Option<u64>,
    pub highest: u64,
    pub retained_highest: Option<u64>,
    last_lsn: Option<u64>,
    pruned_through: Option<u64>,
}

fn marker(db: &DB, key: &[u8]) -> Result<Option<u64>> {
    db.get(key)
        .map_err(|error| Error::storage(format!("WAL recovery metadata read failed: {error}")))?
        .map(|bytes| {
            let bytes: [u8; 8] = bytes
                .as_slice()
                .try_into()
                .map_err(|_| Error::storage("invalid WAL recovery metadata length"))?;
            Ok(u64::from_le_bytes(bytes))
        })
        .transpose()
}

pub fn inspect(db: &DB) -> Result<State> {
    let atomic_groups = match db
        .get(ATOMIC_GROUPS_KEY)
        .map_err(|error| Error::storage(format!("WAL format marker read failed: {error}")))?
    {
        None => false,
        Some(value) if value.as_slice() == ATOMIC_GROUPS_V1 => true,
        Some(_) => return Err(Error::storage("unknown or corrupt wal:atomic_groups format marker")),
    };
    let last_lsn = marker(db, LAST_LSN_KEY)?;
    let checkpoint = marker(db, CHECKPOINT_KEY)?;
    let pruned_through = marker(db, PRUNED_THROUGH_KEY)?;
    // Total order and both bounds are required with Nano's prefix extractor,
    // including stores containing range tombstones from older builds.
    let mut read = bounded_read_options(ENTRY_PREFIX);
    read.set_iterate_lower_bound(ENTRY_PREFIX.to_vec());
    let decode_key = |key: &[u8]| -> Result<u64> {
        let digits = key
            .strip_prefix(ENTRY_PREFIX)
            .ok_or_else(|| Error::storage("WAL recovery iterator escaped its bounds"))?;
        if digits.len() != 20 || !digits.iter().all(u8::is_ascii_digit) {
            return Err(Error::storage("invalid retained WAL entry key"));
        }
        std::str::from_utf8(digits)
            .ok()
            .and_then(|digits| digits.parse::<u64>().ok())
            .ok_or_else(|| Error::storage("invalid retained WAL entry LSN"))
    };
    let retained_highest = if atomic_groups {
        let mut entries = db.raw_iterator_opt(read);
        entries.seek_to_last();
        let highest = entries.key().map(decode_key).transpose()?;
        entries
            .status()
            .map_err(|error| Error::storage(format!("WAL recovery iterator failed: {error}")))?;
        highest
    } else {
        // Legacy provenance is untrusted. Validate every retained key once
        // before declaring its history covered; a malformed earlier key must
        // not hide behind a well-formed maximum. Current stores need one seek.
        let mut highest: Option<u64> = None;
        for entry in db.iterator_opt(
            rocksdb::IteratorMode::From(ENTRY_PREFIX, rocksdb::Direction::Forward),
            read,
        ) {
            let (key, _) = entry.map_err(|error| Error::storage(format!("WAL recovery iterator failed: {error}")))?;
            if !key.starts_with(ENTRY_PREFIX) {
                break;
            }
            let lsn = decode_key(&key)?;
            highest = Some(highest.map_or(lsn, |prior| prior.max(lsn)));
        }
        highest
    };
    let highest = [last_lsn, checkpoint, pruned_through, retained_highest]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(0);
    if highest == u64::MAX {
        return Err(Error::storage("WAL sequence exhausted"));
    }
    Ok(State {
        atomic_groups,
        checkpoint,
        highest,
        retained_highest,
        last_lsn,
        pruned_through,
    })
}

impl State {
    fn history_floor(self) -> u64 {
        // Preserve the OLD logical history floor when advancing a checkpoint:
        // absence formerly meant old checkpoint, not the checkpoint written now.
        self.pruned_through.or(self.checkpoint).unwrap_or(0)
    }

    fn needs_explicit_adoption(self) -> bool {
        !self.atomic_groups
            && self
                .checkpoint
                .is_some_and(|checkpoint| self.retained_highest.is_some_and(|highest| highest > checkpoint))
    }
}

pub fn adopt(db: &DB, gate: &WriteGate, state: State) -> Result<WalAdoptionReport> {
    let floor = state.history_floor();
    if !state.atomic_groups
        || state.checkpoint != Some(state.highest)
        || state.last_lsn != Some(state.highest)
        || state.pruned_through != Some(floor)
    {
        let mut batch = WriteBatch::default();
        batch.put(ATOMIC_GROUPS_KEY, ATOMIC_GROUPS_V1);
        batch.put(CHECKPOINT_KEY, state.highest.to_le_bytes());
        batch.put(LAST_LSN_KEY, state.highest.to_le_bytes());
        batch.put(PRUNED_THROUGH_KEY, floor.to_le_bytes());
        let mut options = WriteOptions::default();
        options.set_sync(true);
        gate.write(db, batch, Some(&options))
            .map_err(|error| Error::storage(format!("WAL local-state adoption failed: {error}")))?;
    }
    Ok(WalAdoptionReport {
        previous_checkpoint: state.checkpoint,
        adopted_through: state.highest,
        history_floor: floor,
    })
}

/// Read-only eligibility check before any open-time bookkeeping writes.
/// The caller verifies its encryption key again on the locked handle before
/// calling `adopt`; unknown/ambiguous legacy stores refuse before sentinel writes.
pub fn validate_open(db: &DB, writable: bool) -> Result<State> {
    let state = inspect(db)?;
    if writable && state.needs_explicit_adoption() {
        return Err(Error::storage(
            "legacy WAL contains entries above its checkpoint; automatic redo is refused because it can overwrite newer persisted state; stop the database and run heliosdb-nano recover-wal --data-dir <directory> --adopt-local-state to preserve and adopt local data",
        ));
    }
    Ok(state)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use rocksdb::{Options, WriteBatchIterator};

    fn raw() -> (tempfile::TempDir, DB) {
        let temp = tempfile::tempdir().unwrap();
        let mut options = Options::default();
        options.create_if_missing(true);
        options.set_prefix_extractor(rocksdb::SliceTransform::create_fixed_prefix(5));
        let db = DB::open(&options, temp.path()).unwrap();
        (temp, db)
    }
    fn prepare_open(db: &DB, gate: &WriteGate, writable: bool) -> Result<()> {
        let state = validate_open(db, writable)?;
        if writable {
            adopt(db, gate, state)?;
        }
        Ok(())
    }

    fn history(db: &DB, lsn: u64) {
        db.put(format!("wal:entries:{lsn:020}"), b"retained historical bytes")
            .unwrap();
        db.put(LAST_LSN_KEY, lsn.to_le_bytes()).unwrap();
    }
    #[derive(Default)]
    struct Keys(Vec<Vec<u8>>);
    impl WriteBatchIterator for Keys {
        fn put(&mut self, key: Box<[u8]>, _: Box<[u8]>) {
            self.0.push(key.into_vec());
        }
        fn delete(&mut self, key: Box<[u8]>) {
            self.0.push(key.into_vec());
        }
    }

    #[test]
    fn legacy_without_checkpoint_adopts_once_in_one_metadata_group() {
        let (_temp, db) = raw();
        history(&db, 7);
        db.put(b"data:t:1", b"newer local state").unwrap();
        let before = db.latest_sequence_number();
        prepare_open(&db, &WriteGate::open(), true).unwrap();
        let state = inspect(&db).unwrap();
        assert!(state.atomic_groups);
        assert_eq!(state.checkpoint, Some(7));
        assert_eq!(state.pruned_through, Some(0));
        assert_eq!(
            db.get(b"data:t:1").unwrap().as_deref(),
            Some(b"newer local state".as_slice())
        );
        assert_eq!(
            db.get(format!("wal:entries:{:020}", 7)).unwrap().as_deref(),
            Some(b"retained historical bytes".as_slice())
        );
        let mut updates = db.get_updates_since(before).unwrap();
        let (first, batch) = updates.next().unwrap().unwrap();
        assert_eq!(first, before + 1);
        let mut keys = Keys::default();
        batch.iterate(&mut keys);
        assert_eq!(keys.0.len(), 4);
        assert!(keys.0.iter().all(
            |key| [ATOMIC_GROUPS_KEY, CHECKPOINT_KEY, LAST_LSN_KEY, PRUNED_THROUGH_KEY].contains(&key.as_slice())
        ));
        assert!(updates.next().is_none());
        updates.status().unwrap();
        let sequence = db.latest_sequence_number();
        prepare_open(&db, &WriteGate::open(), true).unwrap();
        assert_eq!(db.latest_sequence_number(), sequence, "already adopted needs no write");
    }

    #[test]
    fn unmarked_legacy_redo_refuses_writes_but_readonly_and_explicit_adoption_preserve_state() {
        let (_temp, db) = raw();
        history(&db, 12);
        db.put(CHECKPOINT_KEY, 4_u64.to_le_bytes()).unwrap();
        let before = db.latest_sequence_number();
        let error = prepare_open(&db, &WriteGate::open(), true).unwrap_err();
        assert!(error.to_string().contains("recover-wal"));
        prepare_open(&db, &WriteGate::closed("read-only"), false).unwrap();
        assert_eq!(db.latest_sequence_number(), before);
        let report = adopt(&db, &WriteGate::open(), inspect(&db).unwrap()).unwrap();
        assert_eq!(report.adopted_through, 12);
        assert_eq!(report.history_floor, 4, "absence used to mean OLD checkpoint");
        assert!(db.get(format!("wal:entries:{:020}", 12)).unwrap().is_some());
    }

    #[test]
    fn marked_groups_adopt_lagging_checkpoint_and_preserve_explicit_history_floor() {
        let (_temp, db) = raw();
        history(&db, 30);
        db.put(ATOMIC_GROUPS_KEY, ATOMIC_GROUPS_V1).unwrap();
        db.put(CHECKPOINT_KEY, 10_u64.to_le_bytes()).unwrap();
        db.put(PRUNED_THROUGH_KEY, 3_u64.to_le_bytes()).unwrap();
        prepare_open(&db, &WriteGate::open(), true).unwrap();
        let state = inspect(&db).unwrap();
        assert_eq!(state.checkpoint, Some(30));
        assert_eq!(state.pruned_through, Some(3));
    }

    #[test]
    fn corrupt_markers_keys_and_exhaustion_refuse_without_mutation() {
        for marker in [vec![], vec![2], vec![1, 0]] {
            let (_temp, db) = raw();
            db.put(ATOMIC_GROUPS_KEY, marker).unwrap();
            let before = db.latest_sequence_number();
            assert!(prepare_open(&db, &WriteGate::open(), true).is_err());
            assert!(prepare_open(&db, &WriteGate::closed("read-only"), false).is_err());
            assert_eq!(db.latest_sequence_number(), before);
        }
        for key in [CHECKPOINT_KEY, LAST_LSN_KEY, PRUNED_THROUGH_KEY] {
            let (_temp, db) = raw();
            db.put(key, [0_u8; 9]).unwrap();
            assert!(inspect(&db).is_err());
        }
        let (_temp, db) = raw();
        db.put(b"wal:entries:bad", b"bad key").unwrap();
        assert!(inspect(&db).is_err());
        let (_temp, db) = raw();
        history(&db, 100);
        db.put(b"wal:entries:00000000000000000001extra", b"hidden malformed key")
            .unwrap();
        assert!(
            inspect(&db).is_err(),
            "a valid maximum cannot hide malformed legacy keys"
        );
        let (_temp, db) = raw();
        db.put(LAST_LSN_KEY, u64::MAX.to_le_bytes()).unwrap();
        assert!(inspect(&db).unwrap_err().to_string().contains("exhausted"));
    }

    #[test]
    fn eligibility_validation_is_read_only_when_adoption_is_needed() {
        let (_temp, db) = raw();
        history(&db, 9);
        let before = db.latest_sequence_number();
        let state = validate_open(&db, true).unwrap();
        assert!(!state.atomic_groups);
        assert_eq!(state.highest, 9);
        assert_eq!(db.latest_sequence_number(), before);
        assert!(db.get(ATOMIC_GROUPS_KEY).unwrap().is_none());
        assert!(db.get(CHECKPOINT_KEY).unwrap().is_none());
        assert!(db.get(PRUNED_THROUGH_KEY).unwrap().is_none());
        assert_eq!(marker(&db, LAST_LSN_KEY).unwrap(), Some(9));
    }

    #[test]
    fn a_refused_adoption_writes_none_of_its_metadata_group() {
        let (_temp, db) = raw();
        history(&db, 9);
        let before = db.latest_sequence_number();
        let error = adopt(&db, &WriteGate::closed("migration test"), inspect(&db).unwrap()).unwrap_err();
        assert!(error.to_string().contains("adoption failed"));
        assert_eq!(db.latest_sequence_number(), before);
        assert!(db.get(ATOMIC_GROUPS_KEY).unwrap().is_none());
        assert!(db.get(CHECKPOINT_KEY).unwrap().is_none());
        assert!(db.get(PRUNED_THROUGH_KEY).unwrap().is_none());
        assert_eq!(marker(&db, LAST_LSN_KEY).unwrap(), Some(9));
    }

    #[test]
    fn empty_retained_log_uses_pruned_highwater_and_bounded_entry_scan() {
        let (_temp, db) = raw();
        db.put(CHECKPOINT_KEY, 7_u64.to_le_bytes()).unwrap();
        db.put(PRUNED_THROUGH_KEY, 8_u64.to_le_bytes()).unwrap();
        db.put(b"wal:entries_extra", b"unrelated").unwrap();
        prepare_open(&db, &WriteGate::open(), true).unwrap();
        let state = inspect(&db).unwrap();
        assert_eq!(state.highest, 8);
        assert_eq!(state.retained_highest, None);
        assert_eq!(state.pruned_through, Some(8));
    }
}
