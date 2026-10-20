//! The one decision of whether this process may write to a database's RocksDB
//! store.
//!
//! Every storage component that holds the raw `Arc<DB>` — the engine, the
//! logical WAL, the snapshot manager, the branch manager, the version GC,
//! persistent vector indexes, and the stateless helpers the engine hands its
//! handle to — used to be able to write on its own authority. That is harmless
//! on a primary and fatal on a physical standby, whose default column family
//! must stay a byte-for-byte copy of the primary's: a single engine-initiated
//! write (a counter reseed, a snapshot registration, a lazy backfill) moves the
//! standby's sequence number and poisons the replica.
//!
//! So the decision is taken once, at open, from the engine's `OpenMode`, and
//! every raw write site asks this gate instead of calling
//! `DB::write`/`put`/`delete` itself. `tests/write_gate_funnel.rs`
//! fails, naming `file:line`, if a raw write appears anywhere in `src/` outside
//! this module and the two physical-replication modules that own a standby's
//! applier (`replication::physical`, `replication::checkpoint_transfer`; their
//! writes go through the `LocalWriter`, which owns the cursor invariant).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rocksdb::{WriteBatch, WriteOptions, DB};

use crate::{Error, Result};

/// Whether a [`WriteGate`] admits writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateMode {
    /// Every write is admitted: a primary, or a handle its caller owns outright.
    Open,
    /// Every write is refused. `reason` names the open mode in the error, e.g.
    /// "read-only open", "physical snapshot", "physical standby".
    Closed {
        /// The open mode that closed the gate.
        reason: &'static str,
    },
}

/// The one place that decides whether this process may write to the default
/// column family. Created at open and shared, as an `Arc`, by every component
/// that holds the raw `Arc<DB>`.
///
/// The mode is fixed for the gate's lifetime: a database's open mode does not
/// change while it is open. Promotion of a standby will be the first
/// transition, and it will be made here, in one place.
#[derive(Debug)]
pub struct WriteGate {
    mode: GateMode,
    /// Writes refused so far. A diagnostic and a test surface: a read path that
    /// is genuinely write-free leaves it at zero because nothing even tried.
    refusals: AtomicU64,
}

impl WriteGate {
    /// A gate that admits every write.
    pub fn open() -> Arc<Self> {
        Arc::new(Self {
            mode: GateMode::Open,
            refusals: AtomicU64::new(0),
        })
    }

    /// A gate that refuses every write, naming `reason` in the error.
    pub fn closed(reason: &'static str) -> Arc<Self> {
        Arc::new(Self {
            mode: GateMode::Closed { reason },
            refusals: AtomicU64::new(0),
        })
    }

    /// This gate's mode.
    pub fn mode(&self) -> GateMode {
        self.mode
    }

    /// Whether writes are admitted. Does not count as a refusal: callers whose
    /// write is an optional optimisation (a lazy backfill, a cache marker) ask
    /// this first and skip the write, so that only a write somebody actually
    /// needed shows up in [`Self::refusals`].
    pub fn is_open(&self) -> bool {
        matches!(self.mode, GateMode::Open)
    }

    /// How many writes this gate has refused.
    pub fn refusals(&self) -> u64 {
        self.refusals.load(Ordering::Relaxed)
    }

    /// `Ok` when writes are admitted; otherwise counts a refusal and returns
    /// the error a refused write reports.
    pub fn check(&self) -> Result<()> {
        match self.mode {
            GateMode::Open => Ok(()),
            GateMode::Closed { reason } => {
                self.refusals.fetch_add(1, Ordering::Relaxed);
                Err(Error::storage(format!(
                    "write refused on a {reason}: this database was opened without write access, and only a \
                     primary open may change it"
                )))
            }
        }
    }

    /// `DB::write` / `DB::write_opt` behind the gate.
    ///
    /// A batch with no operations changes nothing, so on a closed gate it is a
    /// no-op rather than a refusal: a read-only transaction's COMMIT applies an
    /// empty batch, and that is not an attempt to write. On an open gate it is
    /// handed to RocksDB unchanged, exactly as before the gate existed.
    pub fn write(&self, db: &DB, batch: WriteBatch, opts: Option<&WriteOptions>) -> Result<()> {
        if batch.is_empty() && !self.is_open() {
            return Ok(());
        }
        self.check()?;
        match opts {
            Some(opts) => db.write_opt(batch, opts),
            None => db.write(batch),
        }
        .map_err(Error::from)
    }

    /// `DB::put` / `DB::put_opt` behind the gate.
    pub fn put<K, V>(&self, db: &DB, key: K, value: V, opts: Option<&WriteOptions>) -> Result<()>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        self.check()?;
        match opts {
            Some(opts) => db.put_opt(key, value, opts),
            None => db.put(key, value),
        }
        .map_err(Error::from)
    }

    /// `DB::delete` / `DB::delete_opt` behind the gate.
    pub fn delete<K: AsRef<[u8]>>(&self, db: &DB, key: K, opts: Option<&WriteOptions>) -> Result<()> {
        self.check()?;
        match opts {
            Some(opts) => db.delete_opt(key, opts),
            None => db.delete(key),
        }
        .map_err(Error::from)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn scratch_db() -> (tempfile::TempDir, DB) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        let db = DB::open(&opts, dir.path()).expect("open");
        (dir, db)
    }

    #[test]
    fn an_open_gate_admits_every_write_and_counts_nothing() {
        let (_dir, db) = scratch_db();
        let gate = WriteGate::open();
        gate.put(&db, b"k", b"v", None).unwrap();
        let mut batch = WriteBatch::default();
        batch.put(b"b", b"1");
        gate.write(&db, batch, None).unwrap();
        gate.delete(&db, b"k", None).unwrap();
        assert!(gate.check().is_ok());
        assert_eq!(db.get(b"k").unwrap(), None);
        assert_eq!(db.get(b"b").unwrap().as_deref(), Some(b"1".as_slice()));
        assert_eq!(gate.refusals(), 0);
        assert_eq!(gate.mode(), GateMode::Open);
    }

    #[test]
    fn a_closed_gate_refuses_before_rocksdb_sees_the_write() {
        let (_dir, db) = scratch_db();
        db.put(b"present", b"before").unwrap();
        let sequence = db.latest_sequence_number();
        let gate = WriteGate::closed("physical snapshot");

        let error = gate.put(&db, b"k", b"v", None).unwrap_err().to_string();
        assert!(error.contains("write refused on a physical snapshot"), "{error}");
        let mut batch = WriteBatch::default();
        batch.delete(b"present");
        assert!(gate.write(&db, batch, None).is_err());
        assert!(gate.delete(&db, b"present", None).is_err());
        assert!(gate.check().is_err());

        assert_eq!(gate.refusals(), 4);
        assert_eq!(db.latest_sequence_number(), sequence);
        assert_eq!(db.get(b"present").unwrap().as_deref(), Some(b"before".as_slice()));
        assert_eq!(db.get(b"k").unwrap(), None);
    }

    #[test]
    fn asking_and_empty_batches_are_not_refusals() {
        let (_dir, db) = scratch_db();
        let sequence = db.latest_sequence_number();
        let gate = WriteGate::closed("read-only open");
        assert!(!gate.is_open());
        gate.write(&db, WriteBatch::default(), None).unwrap();
        assert_eq!(gate.refusals(), 0);
        assert_eq!(db.latest_sequence_number(), sequence);
    }
}
