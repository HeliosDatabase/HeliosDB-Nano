//! Real physical range-cap and full-rebuild publications retain branch fences.
use super::{collect_stamp, LayoutStamp};
use crate::replication::checkpoint_transfer::{OnlineCheckpointExport, PhysicalStage, ReceiverStaging, TransferLimits};
use crate::replication::history::load_or_create_primary_history;
use crate::replication::physical::{poll_source, range_digest, FlushMemo, MAX_BATCH_BYTES};
use crate::replication::physical_applier::{DerivedStateListener, PhysicalApplier, RANGE_ENUMERATION_CAP};
use crate::storage::keyspace::KeyClass;
use crate::storage::{StorageEngine, WriteGate};
use crate::{Config, EmbeddedDatabase, Schema};
use rocksdb::{checkpoint::Checkpoint, Direction, IteratorMode, ReadOptions, WriteBatch, DB};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

fn config(path: &Path, history: bool) -> Config {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    config.storage.time_travel_enabled = history;
    config.replication.physical.enabled = true;
    config
}

fn branch_schema(engine: &StorageEngine) -> crate::Result<(Schema, LayoutStamp)> {
    let state = crate::session::scoped::SessionScopedState::new_request_scoped(engine.instance_id());
    engine.bind_session_branch(&state, "a")?;
    let _session = crate::SessionScopedStateGuard::install(state);
    let (schema, stamp) = collect_stamp(|| engine.catalog().get_table_schema("d"));
    Ok((schema?, stamp))
}

#[derive(Default)]
struct PublicationObserver {
    engine: parking_lot::Mutex<Weak<StorageEngine>>,
    enabled: AtomicBool,
    samples: parking_lot::Mutex<Vec<std::result::Result<LayoutStamp, String>>>,
}

impl PublicationObserver {
    fn observe(&self) {
        if !self.enabled.load(Ordering::Acquire) {
            return;
        }
        let Some(engine) = self.engine.lock().upgrade() else {
            self.samples.lock().push(Err("the serving engine disappeared".into()));
            return;
        };
        // The applier owns its publication on this thread; a real independent
        // catalog reader must sample that in-flight epoch, not its own writer.
        let sample = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    branch_schema(&engine)
                        .map(|(_, stamp)| stamp)
                        .map_err(|error| error.to_string())
                })
                .join()
                .map_err(|_| "the physical publication reader panicked".to_string())
                .and_then(|sample| sample)
        });
        self.samples.lock().push(sample);
    }

    fn assert_inflight_samples(&self) -> crate::Result<()> {
        let samples = self.samples.lock();
        assert!(!samples.is_empty(), "the actual applier listener was reached");
        for sample in samples.iter() {
            let stamp = sample
                .as_ref()
                .map_err(|error| crate::Error::storage(format!("the real publication reader failed: {error}")))?;
            assert!(
                !stamp.is_current(),
                "a publication-time branch plan cannot become valid"
            );
        }
        Ok(())
    }
}

impl DerivedStateListener for PublicationObserver {
    fn on_catalog_change(&self, _class: KeyClass, _key: &[u8], _deleted: bool) {}

    fn on_layout_change(&self, _tables: &[String]) {
        self.observe();
    }

    fn on_full_rebuild(&self) {
        self.observe();
    }
}

struct Pair {
    applier: PhysicalApplier,
    observer: Arc<PublicationObserver>,
    standby: Arc<StorageEngine>,
    _stage: PhysicalStage,
    raw: Arc<DB>,
    primary: EmbeddedDatabase,
    history_id: uuid::Uuid,
    history: bool,
    root: tempfile::TempDir,
}

impl Pair {
    fn new(history: bool, branch_records: Option<usize>) -> TestResult<Self> {
        let root = tempfile::tempdir()?;
        let primary_config = config(&root.path().join("primary"), history);
        let primary = EmbeddedDatabase::with_config(primary_config.clone())?;
        primary.execute("CREATE TABLE d(id INT PRIMARY KEY, v INT)")?;
        primary.execute("INSERT INTO d VALUES(1,10)")?;
        primary.create_branch("a")?;
        primary.with_request_branch_scope("a", || primary.execute("ALTER TABLE d ADD COLUMN a_col INT DEFAULT 3"))?;
        let raw = primary.storage.db();
        if let Some(total) = branch_records {
            assert!(total >= 2);
            // Obtain a valid Dropped record through public branch DDL. Copies
            // vary only the owning key; there are no heap tables or fake codecs.
            primary.with_request_branch_scope("a", || {
                primary.execute("CREATE TABLE seed(id INT)")?;
                primary.execute("DROP TABLE seed")
            })?;
            let level = primary.storage.get_branch("a")?.branch_id;
            let key = crate::storage::branch_catalog::branch_table_key(level, "seed");
            let dropped = raw
                .get(&key)?
                .ok_or_else(|| crate::Error::storage("public DROP did not persist its branch record"))?;
            let mut batch = WriteBatch::default();
            batch.delete(&key);
            for index in 0..total - 1 {
                batch.put(
                    crate::storage::branch_catalog::branch_table_key(level, &format!("aa_{index:06}")),
                    &dropped,
                );
            }
            primary.storage.write_gate().write(raw.as_ref(), batch, None)?;
            let (start, end) = branch_range(level);
            let mut read = ReadOptions::default();
            read.set_total_order_seek(true);
            read.set_iterate_upper_bound(end);
            let mut found = 0;
            for row in raw.iterator_opt(IteratorMode::From(&start, Direction::Forward), read) {
                row?;
                found += 1;
            }
            assert_eq!(found, total, "the actual native enumeration boundary");
        }
        let history_id = load_or_create_primary_history(raw.path())?;
        let limits = TransferLimits::default();
        let export = OnlineCheckpointExport::create(&raw, history_id, root.path(), limits)?;
        let mut receiver = ReceiverStaging::begin(
            &root.path().join("standby"),
            &export.manifest_bytes()?,
            history_id,
            limits,
        )?;
        for (index, file) in export.manifest().files.iter().enumerate() {
            let mut offset = 0;
            while offset < file.size {
                let bytes = export.read_chunk(u32::try_from(index)?, offset, 64 * 1024)?;
                receiver.append_chunk(u32::try_from(index)?, offset, &bytes)?;
                offset += u64::try_from(bytes.len())?;
            }
        }
        let stage = receiver.finish_download(&primary_config)?.open(&primary_config)?;
        let path = stage.staging_path();
        let standby = Arc::new(StorageEngine::open_physical_standby(
            path,
            &config(path, history),
            Arc::clone(stage.writer()),
            WriteGate::closed("physical branch layout test"),
        )?);
        let observer = Arc::new(PublicationObserver::default());
        *observer.engine.lock() = Arc::downgrade(&standby);
        let applier = PhysicalApplier::new(Arc::clone(&standby), Arc::clone(stage.writer()), observer.clone())?;
        applier.hydrate()?;
        Ok(Self {
            applier,
            observer,
            standby,
            _stage: stage,
            raw,
            primary,
            history_id,
            history,
            root,
        })
    }

    fn apply_head(&self) -> TestResult {
        let through = self.raw.latest_sequence_number();
        while self.applier.writer().cursor().source_sequence() < through {
            let poll = poll_source(
                &self.raw,
                self.history_id,
                self.applier.writer().cursor().source_sequence(),
                MAX_BATCH_BYTES,
                &FlushMemo::new(),
            )?;
            assert!(!poll.batches.is_empty(), "the real source has the range group");
            for batch in &poll.batches {
                assert!(batch.last_sequence() <= through);
                self.applier.apply(batch)?;
            }
        }
        assert_eq!(self.applier.writer().cursor().source_sequence(), through);
        Ok(())
    }

    fn assert_checkpoint(&self, columns: usize) -> TestResult {
        let path = self.root.path().join("reference");
        Checkpoint::new(&self.raw)?.create_checkpoint(&path)?;
        let hydrated = EmbeddedDatabase::open_physical_snapshot(config(&path, self.history))?;
        assert_eq!(
            range_digest(self.applier.writer().db(), None, b"")?,
            range_digest(&hydrated.database.storage.db(), None, b"")?,
            "the applied store equals an actual native checkpoint at its cursor"
        );
        assert_eq!(branch_schema(&hydrated.database.storage)?.0.columns.len(), columns);
        assert_eq!(self.standby.scan_table("d")?.len(), 1);
        assert_eq!(self.primary.storage.scan_table("d")?.len(), 1);
        Ok(())
    }
}

fn branch_range(level: u64) -> (Vec<u8>, Vec<u8>) {
    (
        format!("bmeta:{level}:table:").into_bytes(),
        format!("bmeta:{level}:table;").into_bytes(),
    )
}

#[test]
fn physical_branch_ranges_cover_the_exact_enumeration_cap_and_its_overflow() -> TestResult {
    for history in [false, true] {
        for overflow in [false, true] {
            let total = RANGE_ENUMERATION_CAP + usize::from(overflow);
            let pair = Pair::new(history, Some(total))?;
            let (before_schema, before) = branch_schema(&pair.standby)?;
            assert_eq!(before_schema.columns.len(), 3);
            assert!(before.is_current());
            let main_epoch = pair.standby.layout_epochs().current("d");
            let full_rebuilds = pair.applier.stats().full_rebuilds;
            let level = pair.primary.storage.get_branch("a")?.branch_id;
            let (start, end) = branch_range(level);
            // aa_* precedes d. At cap+1, d is genuinely beyond enumeration,
            // so only the branch-wide fence can invalidate this old plan.
            let mut batch = WriteBatch::default();
            batch.delete_range(&start, &end);
            let native_before = pair.raw.latest_sequence_number();
            pair.primary
                .storage
                .write_gate()
                .write(pair.raw.as_ref(), batch, None)?;
            assert_eq!(pair.raw.latest_sequence_number(), native_before + 1);
            pair.observer.enabled.store(true, Ordering::Release);
            pair.apply_head()?;
            assert!(!before.is_current());
            pair.observer.assert_inflight_samples()?;
            assert_eq!(pair.standby.layout_epochs().current("d"), main_epoch);
            assert_eq!(branch_schema(&pair.standby)?.0.columns.len(), 2);
            assert_eq!(pair.applier.stats().derived_state_suspect, overflow);
            assert_eq!(pair.applier.run_pending_rebuilds()?, overflow);
            assert_eq!(pair.applier.stats().full_rebuilds, full_rebuilds + u64::from(overflow));
            assert_eq!(
                pair.applier.stats().derived_state_suspect,
                overflow,
                "the unknown-range diagnostic is intentionally sticky after re-derivation"
            );
            pair.observer.assert_inflight_samples()?;
            let (_, fresh) = branch_schema(&pair.standby)?;
            assert!(fresh.is_current());
            pair.assert_checkpoint(2)?;
            eprintln!("branch-layout-physical-cell range history={history} overflow={overflow} records={total}");
        }
    }
    Ok(())
}

#[test]
fn a_real_full_rebuild_fences_branch_plans_through_listener_publication() -> TestResult {
    for history in [false, true] {
        let pair = Pair::new(history, None)?;
        let (schema, before) = branch_schema(&pair.standby)?;
        assert_eq!(schema.columns.len(), 3);
        assert!(before.is_current());
        let main_epoch = pair.standby.layout_epochs().current("d");
        let source_cursor = pair.applier.writer().cursor().source_sequence();
        let digest = range_digest(pair.applier.writer().db(), None, b"")?;
        let rebuilds = pair.applier.stats().full_rebuilds;
        pair.observer.enabled.store(true, Ordering::Release);
        pair.applier.rebuild_all_derived_state()?;
        assert!(!before.is_current());
        pair.observer.assert_inflight_samples()?;
        assert_eq!(pair.applier.stats().full_rebuilds, rebuilds + 1);
        assert_eq!(pair.applier.writer().cursor().source_sequence(), source_cursor);
        assert_eq!(range_digest(pair.applier.writer().db(), None, b"")?, digest);
        assert_eq!(pair.standby.layout_epochs().current("d"), main_epoch);
        let (fresh_schema, fresh) = branch_schema(&pair.standby)?;
        assert_eq!(fresh_schema.columns.len(), 3);
        assert!(fresh.is_current());
        pair.assert_checkpoint(3)?;
        eprintln!("branch-layout-physical-cell rebuild history={history}");
    }
    Ok(())
}
