//! Spec P1.2: a physical standby's vector indexes follow its primary's, in
//! memory, and never write.
//!
//! # Why
//!
//! HNSW indexes live in memory. A standard one is a graph built from rows;
//! a persistent one (`vector-persist`) is an in-RAM copy of its
//! `__vidx:{id}:*` keys whose own mutators WRITE those keys. A standby
//! receives the rows and the keys byte for byte through its applier, and its
//! engine's write gate refuses every write. Before P1.2 the applier's vector
//! hook did nothing: a standby served the graphs its hydration built from
//! then on, and could not hydrate a persistent index at all.
//!
//! # What
//!
//! Each case stages a standby from a primary as P1.4's harness does
//! (`OnlineCheckpointExport` → `ReceiverStaging` → `LocalWriter` →
//! `StorageEngine::open_physical_standby` → `EmbeddedDatabase::with_physical_standby`
//! → `PhysicalApplier`), runs statements on the primary, applies its
//! batches, and compares the two engines' vector indexes: the live entry
//! count and, for every probe query, the top-k row ids and distances. A
//! standard index is asked for ALL its live entries and the answer cut to k
//! after sorting by (distance, row id): that answer is exact on both sides
//! (`HnswIndex::search` falls back to brute force when the graph returns
//! fewer), so the comparison is about what each index holds, not about two
//! independently built graphs agreeing on an approximation. A persistent
//! index is the same state on both sides, so its plain top-k graph search is
//! compared as well. Throughout, the standby's gate counts no refusal.
//!
//! # Red before P1.2
//!
//! `a_noop_vector_hook_leaves_the_standby_index_behind` is the control: with
//! P1.1's no-op hook the same comparison fails as soon as the primary
//! inserts. Every case below compares after its first DML batch, so each
//! fails the same way on 4517cd09; the persistent cases fail earlier still,
//! at hydration ("unsupported strict-hydration family") or at registration.
#![cfg(all(feature = "ha-tier1", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::replication::checkpoint_transfer::{
    OnlineCheckpointExport, PhysicalStage, ReceiverStaging, TransferLimits,
};
use heliosdb_nano::replication::history::load_or_create_primary_history;
use heliosdb_nano::replication::physical::{poll_source, FlushMemo, MAX_BATCH_BYTES};
#[cfg(feature = "vector-persist")]
use heliosdb_nano::replication::physical_applier::{decode_raw_batch, RawOp};
use heliosdb_nano::replication::physical_applier::{DerivedStateListener, NoopVectorHook, PhysicalApplier, VectorHook};
use heliosdb_nano::replication::physical_vector::StandbyVectorHook;
use heliosdb_nano::storage::{PersistentPolicy, StorageEngine, WriteGate};
use heliosdb_nano::{Config, EmbeddedDatabase};
use rocksdb::DB;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Harness (the minimum of `tests/physical_standby_embedded.rs`)
// ---------------------------------------------------------------------------

/// A primary's configuration: persistent, with retained RocksDB WAL so a
/// stage can continue from its checkpoint cut. The standby opens with the
/// same configuration over its own path.
fn primary_config(path: &Path) -> Config {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    config.replication.physical.enabled = true;
    config
}

/// Export a checkpoint of the live primary `source`, download it into `root`
/// and open it as a stage. (As in `tests/physical_standby_derived_state.rs`.)
fn stage(source: &DB, history: Uuid, root: &Path, config: &Config) -> PhysicalStage {
    let limits = TransferLimits::default();
    let export = OnlineCheckpointExport::create(source, history, root, limits).unwrap();
    let mut receiver = ReceiverStaging::begin(
        &root.join("standby"),
        &export.manifest_bytes().unwrap(),
        history,
        limits,
    )
    .unwrap();
    for (index, file) in export.manifest().files.iter().enumerate() {
        let mut offset = 0;
        while offset < file.size {
            let bytes = export.read_chunk(index as u32, offset, 64 * 1024).unwrap();
            receiver.append_chunk(index as u32, offset, &bytes).unwrap();
            offset += bytes.len() as u64;
        }
    }
    receiver.finish_download(config).unwrap().open(config).unwrap()
}

/// A primary and its served physical standby. Fields drop in declaration
/// order: the applier (which holds the database as its listener) first,
/// then the database, the engine, the stage that owns their RocksDB handle
/// (reached through `applier.writer()`), the primary, and the directory last.
struct Standby {
    applier: PhysicalApplier,
    database: Arc<EmbeddedDatabase>,
    engine: Arc<StorageEngine>,
    _stage: PhysicalStage,
    raw: Arc<DB>,
    primary: EmbeddedDatabase,
    history: Uuid,
    _root: tempfile::TempDir,
}

impl Standby {
    /// A primary that ran `setup`, a standby staged from it right after and
    /// served in the production order. `hook` replaces the applier's default
    /// vector hook; `None` keeps it.
    fn new(setup: &[&str], hook: Option<Arc<dyn VectorHook>>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let config = primary_config(&root.path().join("primary"));
        let primary = EmbeddedDatabase::with_config(config.clone()).unwrap();
        for sql in setup {
            primary
                .execute(sql)
                .unwrap_or_else(|e| panic!("primary setup `{sql}`: {e}"));
        }
        let raw = primary.storage.db();
        let history = load_or_create_primary_history(raw.path()).unwrap();
        let stage = stage(&raw, history, root.path(), &config);
        let path = stage.staging_path().to_path_buf();
        let engine = Arc::new(
            StorageEngine::open_physical_standby(
                &path,
                &primary_config(&path),
                Arc::clone(stage.writer()),
                WriteGate::closed("physical standby"),
            )
            .unwrap(),
        );
        let database = Arc::new(
            EmbeddedDatabase::with_physical_standby(primary_config(&path), Arc::clone(&engine))
                .unwrap_or_else(|e| panic!("with_physical_standby: {e}")),
        );
        let listener: Arc<dyn DerivedStateListener> = database.clone();
        let mut applier = PhysicalApplier::new(Arc::clone(&engine), Arc::clone(stage.writer()), listener).unwrap();
        if let Some(hook) = hook {
            applier = applier.with_vector_hook(hook);
        }
        Self {
            applier,
            database,
            engine,
            _stage: stage,
            raw,
            primary,
            history,
            _root: root,
        }
    }

    fn run(&self, statements: &[&str]) {
        for sql in statements {
            self.primary
                .execute(sql)
                .unwrap_or_else(|e| panic!("primary `{sql}`: {e}"));
        }
    }

    fn cursor(&self) -> u64 {
        self.applier.writer().cursor().source_sequence()
    }

    /// Apply everything the primary has.
    fn sync(&self) {
        loop {
            let poll = poll_source(
                &self.raw,
                self.history,
                self.cursor(),
                MAX_BATCH_BYTES,
                &FlushMemo::new(),
            )
            .unwrap();
            if poll.batches.is_empty() {
                return;
            }
            for batch in &poll.batches {
                self.applier
                    .apply(batch)
                    .unwrap_or_else(|e| panic!("applying {}..={}: {e}", batch.first_sequence(), batch.last_sequence()));
            }
        }
    }

    /// Apply exactly the next source batch (one primary write group) and
    /// return its records as `(deleted, key)`; `None` when there is none.
    #[cfg(feature = "vector-persist")]
    fn step(&self) -> Option<Vec<(bool, Vec<u8>)>> {
        let poll = poll_source(
            &self.raw,
            self.history,
            self.cursor(),
            MAX_BATCH_BYTES,
            &FlushMemo::new(),
        )
        .unwrap();
        let batch = poll.batches.into_iter().next()?;
        let records = decode_raw_batch(batch.data())
            .unwrap()
            .into_iter()
            .map(|op| match op {
                RawOp::Put { key, .. } => (false, key.to_vec()),
                RawOp::Delete { key } => (true, key.to_vec()),
                RawOp::DeleteRange { start, .. } => (true, start.to_vec()),
            })
            .collect();
        self.applier.apply(&batch).unwrap();
        Some(records)
    }

    /// Writes the standby engine's gate refused.
    fn refusals(&self) -> u64 {
        self.engine.write_gate().refusals()
    }
}

// ---------------------------------------------------------------------------
// Data and comparisons
// ---------------------------------------------------------------------------

const K: usize = 5;

/// A deterministic 4-dimensional vector literal for row `id` (three decimals,
/// so the primary's parse and the standby's decode meet the same `f32`s).
fn vector(id: u64, salt: u64) -> String {
    let component = |j: u64| {
        (id.wrapping_mul(2_654_435_761)
            .wrapping_add(j.wrapping_mul(40_503))
            .wrapping_add(salt.wrapping_mul(7_919))
            % 1000) as f32
            / 1000.0
    };
    format!(
        "[{:.3}, {:.3}, {:.3}, {:.3}]",
        component(1),
        component(2),
        component(3),
        component(4)
    )
}

/// `INSERT INTO {table} VALUES (id, 'doc-{id}', vector)` for every id.
fn insert(table: &str, ids: impl IntoIterator<Item = u64>, salt: u64) -> String {
    let values: Vec<String> = ids
        .into_iter()
        .map(|id| format!("({id}, 'doc-{id}', '{}')", vector(id, salt)))
        .collect();
    format!("INSERT INTO {table} VALUES {}", values.join(", "))
}

fn probes() -> Vec<Vec<f32>> {
    vec![
        vec![0.5, 0.5, 0.5, 0.5],
        vec![0.1, 0.9, 0.2, 0.8],
        vec![0.9, 0.1, 0.7, 0.3],
        vec![0.0, 0.0, 0.0, 0.0],
        vec![1.0, 1.0, 1.0, 1.0],
    ]
}

/// The `k` nearest live entries of `index`, by (distance, row id): every
/// live entry asked for, sorted, cut to `k` — exact for any HNSW (see the
/// module documentation).
fn knn(engine: &StorageEngine, index: &str, query: &[f32], k: usize) -> Vec<(u64, f32)> {
    let manager = engine.vector_indexes();
    let live = manager
        .index_live_size(index)
        .unwrap_or_else(|| panic!("index '{index}' is not registered"));
    let mut all = manager.search(index, &query.to_vec(), live.max(1)).unwrap();
    all.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
    all.truncate(k);
    all
}

/// The standby's index `index` holds what the primary's holds: the same live
/// entry count, and for every probe the same top-k ids and distances. The
/// standby's gate was never asked to write.
fn assert_same_index(standby: &Standby, index: &str, context: &str) {
    let primary = &standby.primary.storage;
    let replica = &*standby.engine;
    assert!(
        replica.vector_indexes().index_exists(index),
        "'{index}' is not registered on the standby {context}"
    );
    assert_eq!(
        replica.vector_indexes().index_live_size(index),
        primary.vector_indexes().index_live_size(index),
        "live entries of '{index}' {context}"
    );
    for probe in probes() {
        let expected = knn(primary, index, &probe, K);
        let got = knn(replica, index, &probe, K);
        assert_eq!(
            got.iter().map(|(row, _)| *row).collect::<Vec<_>>(),
            expected.iter().map(|(row, _)| *row).collect::<Vec<_>>(),
            "top-{K} ids of '{index}' for {probe:?} {context}"
        );
        for ((_, a), (_, b)) in got.iter().zip(&expected) {
            assert!(
                (a - b).abs() <= 1e-6,
                "distances of '{index}' for {probe:?} {context}: {got:?} vs {expected:?}"
            );
        }
    }
    assert_eq!(standby.refusals(), 0, "the standby asked its gate to write {context}");
}

fn assert_applier_clean(standby: &Standby) {
    let stats = standby.applier.stats();
    assert_eq!(stats.unknown_keys, 0, "{stats:?}");
    assert!(!stats.derived_state_suspect, "{stats:?}");
    assert_eq!(stats.pending_tables, 0, "{stats:?}");
}

const DOCS: &str = "CREATE TABLE docs (id INT PRIMARY KEY, title TEXT, emb VECTOR(4))";

// ---------------------------------------------------------------------------
// The control
// ---------------------------------------------------------------------------

#[test]
fn a_noop_vector_hook_leaves_the_standby_index_behind() {
    let standby = Standby::new(
        &[
            DOCS,
            &insert("docs", 1..=30, 0),
            "CREATE INDEX docs_emb_idx ON docs USING hnsw (emb)",
        ],
        Some(Arc::new(NoopVectorHook)),
    );
    // Hydration built the graph from the rows, so the two agree at the cut.
    assert_same_index(&standby, "docs_emb_idx", "at the cut");

    standby.run(&[&insert("docs", 31..=40, 1), "DELETE FROM docs WHERE id <= 3"]);
    standby.sync();
    let manager = standby.engine.vector_indexes();
    assert_eq!(manager.index_live_size("docs_emb_idx"), Some(30), "the open-time graph");
    assert_eq!(
        standby.primary.storage.vector_indexes().index_live_size("docs_emb_idx"),
        Some(37)
    );
    let probe = probes()[0].clone();
    assert_ne!(
        knn(&standby.engine, "docs_emb_idx", &probe, 37),
        knn(&standby.primary.storage, "docs_emb_idx", &probe, 37),
        "the comparison must see a standby that does not follow"
    );
    assert_eq!(standby.refusals(), 0);
}

// ---------------------------------------------------------------------------
// Standard HNSW
// ---------------------------------------------------------------------------

#[test]
fn standard_hnsw_follows_inserts_updates_and_deletes() {
    // The applier's DEFAULT hook: the wiring is part of what is tested.
    let standby = Standby::new(
        &[
            DOCS,
            &insert("docs", 1..=40, 0),
            "CREATE INDEX docs_emb_idx ON docs USING hnsw (emb)",
        ],
        None,
    );
    assert_eq!(
        standby.engine.vector_indexes().persistent_policy(),
        PersistentPolicy::ReplicaMemoryOnly
    );
    assert_eq!(
        standby.primary.storage.vector_indexes().persistent_policy(),
        PersistentPolicy::Durable
    );
    assert_same_index(&standby, "docs_emb_idx", "at the cut");

    let steps: Vec<Vec<String>> = vec![
        vec![
            insert("docs", 41..=50, 1),
            insert("docs", [51], 1),
            "INSERT INTO docs VALUES (52, 'no vector', NULL)".to_string(),
        ],
        vec![
            format!("UPDATE docs SET emb = '{}' WHERE id = 3", vector(3, 9)),
            "UPDATE docs SET title = 'retitled' WHERE id = 4".to_string(),
            "UPDATE docs SET emb = NULL WHERE id = 5".to_string(),
            format!("UPDATE docs SET emb = '{}' WHERE id = 52", vector(52, 9)),
            format!("UPDATE docs SET emb = '{}' WHERE id <= 2", vector(2, 5)),
        ],
        vec![
            "DELETE FROM docs WHERE id = 7".to_string(),
            "DELETE FROM docs WHERE id > 45".to_string(),
        ],
        vec![
            "BEGIN".to_string(),
            insert("docs", 60..=62, 2),
            format!("UPDATE docs SET emb = '{}' WHERE id = 60", vector(60, 3)),
            "DELETE FROM docs WHERE id = 61".to_string(),
            "COMMIT".to_string(),
            "BEGIN".to_string(),
            insert("docs", [70], 2),
            "DELETE FROM docs WHERE id = 8".to_string(),
            "ROLLBACK".to_string(),
        ],
    ];
    for (number, statements) in steps.iter().enumerate() {
        let statements: Vec<&str> = statements.iter().map(String::as_str).collect();
        standby.run(&statements);
        standby.sync();
        assert_same_index(&standby, "docs_emb_idx", &format!("after step {number}"));
    }
    let stats = standby.applier.stats();
    assert_eq!(stats.rebuilds, 0, "the row-by-row path handled every step: {stats:?}");
    assert_applier_clean(&standby);

    // The SQL layer answers the same (below 256 live entries the planner
    // scans exactly, so this checks the served rows, the index is checked
    // above). The table holds a NULL vector (row 5), and `<->` refuses a
    // NULL operand, so both sides filter it out first.
    assert!(standby.database.is_physical_standby());
    let knn_sql = "SELECT id FROM docs WHERE emb IS NOT NULL ORDER BY emb <-> '[0.5, 0.5, 0.5, 0.5]' LIMIT 5";
    assert_eq!(
        standby.database.query(knn_sql, &[]).unwrap(),
        standby.primary.query(knn_sql, &[]).unwrap()
    );
    assert_eq!(standby.refusals(), 0);
}

#[test]
fn standard_hnsw_created_on_a_populated_table_and_dropped() {
    let hook = Arc::new(StandbyVectorHook::new());
    let standby = Standby::new(
        &[
            DOCS,
            &insert("docs", 1..=40, 0),
            "CREATE TABLE notes (id INT PRIMARY KEY, title TEXT, emb VECTOR(4))",
            &insert("notes", 1..=25, 4),
        ],
        Some(hook.clone()),
    );
    assert!(!standby.engine.vector_indexes().has_any_index());

    standby.run(&["CREATE INDEX docs_emb_idx ON docs USING hnsw (emb)"]);
    standby.sync();
    assert_same_index(&standby, "docs_emb_idx", "after CREATE INDEX on a populated table");
    assert_eq!(hook.stats().registrations, 1, "{:?}", hook.stats());

    standby.run(&[&insert("docs", 41..=45, 1), "DELETE FROM docs WHERE id = 10"]);
    standby.sync();
    assert_same_index(&standby, "docs_emb_idx", "after DML on the new index");

    standby.run(&["DROP INDEX docs_emb_idx"]);
    standby.sync();
    assert!(!standby.primary.storage.vector_indexes().index_exists("docs_emb_idx"));
    assert!(
        !standby.engine.vector_indexes().index_exists("docs_emb_idx"),
        "DROP INDEX did not reach the standby"
    );

    // Another index, on another populated table, then its table dropped.
    standby.run(&[
        "CREATE INDEX notes_emb_idx ON notes USING hnsw (emb)",
        &insert("notes", 26..=30, 5),
    ]);
    standby.sync();
    assert_same_index(&standby, "notes_emb_idx", "after a second CREATE INDEX");
    standby.run(&["DROP TABLE notes"]);
    standby.sync();
    assert!(
        !standby.engine.vector_indexes().index_exists("notes_emb_idx"),
        "DROP TABLE left its vector index on the standby"
    );
    assert!(standby.engine.vector_indexes().indexes_on_table("notes").is_empty());

    // A table created after the cut, indexed while empty, then filled.
    standby.run(&[
        "CREATE TABLE late_docs (id INT PRIMARY KEY, title TEXT, emb VECTOR(4))",
        "CREATE INDEX late_docs_emb_idx ON late_docs USING hnsw (emb)",
        &insert("late_docs", 1..=20, 6),
    ]);
    standby.sync();
    assert_same_index(&standby, "late_docs_emb_idx", "on a table created after the cut");

    let stats = hook.stats();
    assert_eq!(stats.degraded, 0, "{stats:?}");
    assert_eq!(standby.refusals(), 0);
    assert_applier_clean(&standby);
}

// ---------------------------------------------------------------------------
// Persistent HNSW (`vector-persist`)
// ---------------------------------------------------------------------------

/// Both indexes are the same state, so even the plain graph search agrees
/// exactly — ids and distance bits.
#[cfg(feature = "vector-persist")]
fn assert_same_persistent_index(standby: &Standby, index: &str, context: &str) {
    assert_same_index(standby, index, context);
    for probe in probes() {
        let bits = |engine: &StorageEngine| -> Vec<(u64, u32)> {
            engine
                .vector_indexes()
                .search(index, &probe, K)
                .unwrap()
                .into_iter()
                .map(|(row, distance)| (row, distance.to_bits()))
                .collect()
        };
        assert_eq!(
            bits(&*standby.engine),
            bits(&*standby.primary.storage),
            "top-{K} graph search of '{index}' for {probe:?} {context}"
        );
    }
    assert_eq!(
        standby
            .engine
            .vector_indexes()
            .get_index_stats(index)
            .unwrap()
            .quantization,
        "Persistent",
        "'{index}' is not served by the persistent index on the standby {context}"
    );
}

/// Every key of the standby's store under `prefix`.
#[cfg(feature = "vector-persist")]
fn standby_keys(standby: &Standby, prefix: &[u8]) -> Vec<Vec<u8>> {
    let mut read = rocksdb::ReadOptions::default();
    read.set_total_order_seek(true);
    standby
        .applier
        .writer()
        .db()
        .iterator_opt(rocksdb::IteratorMode::From(prefix, rocksdb::Direction::Forward), read)
        .map(|item| item.unwrap().0.to_vec())
        .take_while(|key| key.starts_with(prefix))
        .collect()
}

/// The `__vidx:{id}:` prefix of the one persistent index the primary has.
#[cfg(feature = "vector-persist")]
fn persistent_prefix(standby: &Standby) -> Vec<u8> {
    let mut read = rocksdb::ReadOptions::default();
    read.set_total_order_seek(true);
    let (key, _) = standby
        .raw
        .iterator_opt(
            rocksdb::IteratorMode::From(b"__vidx:", rocksdb::Direction::Forward),
            read,
        )
        .next()
        .expect("a persistent index keyspace")
        .unwrap();
    let (id, _) = heliosdb_nano::vector::persistent::split_record_key(&key).expect("a persistent index key");
    format!("__vidx:{id}:").into_bytes()
}

#[cfg(feature = "vector-persist")]
#[test]
fn persistent_hnsw_follows_its_replicated_records() {
    let hook = Arc::new(StandbyVectorHook::new());
    // Past the planner's small-index exact fallback (256 live entries), so
    // the SQL kNN below is served by the index on both sides.
    let standby = Standby::new(
        &[
            DOCS,
            &insert("docs", 1..=150, 0),
            &insert("docs", 151..=300, 0),
            "CREATE INDEX docs_emb_pidx ON docs USING hnsw (emb) WITH (persistent = true)",
        ],
        Some(hook.clone()),
    );
    // Hydrated by reopening the replicated keyspace.
    assert_same_persistent_index(&standby, "docs_emb_pidx", "at the cut");

    let steps: Vec<Vec<String>> = vec![
        vec![insert("docs", 301..=310, 1), insert("docs", [311], 1)],
        vec![
            format!("UPDATE docs SET emb = '{}' WHERE id = 3", vector(3, 9)),
            "UPDATE docs SET title = 'retitled' WHERE id = 4".to_string(),
            "UPDATE docs SET emb = NULL WHERE id = 5".to_string(),
        ],
        vec![
            "DELETE FROM docs WHERE id = 7".to_string(),
            "DELETE FROM docs WHERE id > 305".to_string(),
        ],
        vec![
            "BEGIN".to_string(),
            insert("docs", [320], 2),
            "DELETE FROM docs WHERE id = 9".to_string(),
            "COMMIT".to_string(),
            "BEGIN".to_string(),
            insert("docs", [330], 2),
            "ROLLBACK".to_string(),
        ],
    ];
    for (number, statements) in steps.iter().enumerate() {
        let statements: Vec<&str> = statements.iter().map(String::as_str).collect();
        standby.run(&statements);
        standby.sync();
        assert_same_persistent_index(&standby, "docs_emb_pidx", &format!("after step {number}"));
    }
    let stats = hook.stats();
    assert!(stats.records_applied > 0, "{stats:?}");
    assert_eq!(stats.reopens, 0, "every record applied: {stats:?}");
    assert_eq!(stats.records_unowned, 0, "{stats:?}");
    assert_applier_clean(&standby);

    // The SQL layer serves the same kNN from the standby's index (filtered:
    // row 5's vector is NULL, which `<->` refuses should the planner ever
    // fall back to its exact scan).
    let knn_sql = "SELECT id FROM docs WHERE emb IS NOT NULL ORDER BY emb <-> '[0.5, 0.5, 0.5, 0.5]' LIMIT 5";
    assert_eq!(
        standby.database.query(knn_sql, &[]).unwrap(),
        standby.primary.query(knn_sql, &[]).unwrap()
    );

    // ReplicaMemoryOnly: what only a write could express is refused, and the
    // gate is never asked.
    let manager = standby.engine.vector_indexes();
    let before = knn(&standby.engine, "docs_emb_pidx", &probes()[0], K);
    let sequence = standby.applier.writer().db().latest_sequence_number();
    assert!(manager.insert_vector("docs_emb_pidx", 9_999, &vec![0.5; 4]).is_err());
    assert!(manager
        .insert_vectors_batch("docs_emb_pidx", &[(9_999, vec![0.5; 4])])
        .is_err());
    assert!(manager.delete_vector("docs_emb_pidx", 1).is_err());
    assert!(manager
        .create_persistent_index(
            "another_pidx".into(),
            "docs".into(),
            "emb".into(),
            4,
            heliosdb_nano::vector::DistanceMetric::L2,
            None,
            None,
            &[],
            standby.engine.db(),
            Arc::clone(standby.engine.write_gate()),
        )
        .is_err());
    assert!(!manager.index_exists("another_pidx"));
    assert_eq!(knn(&standby.engine, "docs_emb_pidx", &probes()[0], K), before);
    // A drop forgets the copy and keeps the keys (the primary's to delete).
    let prefix = persistent_prefix(&standby);
    let keys = standby_keys(&standby, &prefix);
    assert!(!keys.is_empty());
    manager.drop_index("docs_emb_pidx").unwrap();
    assert!(!manager.index_exists("docs_emb_pidx"));
    assert_eq!(standby_keys(&standby, &prefix), keys);
    assert_eq!(standby.applier.writer().db().latest_sequence_number(), sequence);
    assert_eq!(standby.refusals(), 0, "a ReplicaMemoryOnly mutator asked the gate");
}

/// The buffered-before-registration case: an index created on a populated
/// table reaches the standby as its `meta` record, one write group per
/// backfilled vector, and only then its `meta:index:` definition.
#[cfg(feature = "vector-persist")]
#[test]
fn persistent_hnsw_records_before_its_definition_register_from_the_keys() {
    let hook = Arc::new(StandbyVectorHook::new());
    let standby = Standby::new(
        &[
            DOCS,
            &insert("docs", 1..=30, 0),
            "CREATE TABLE notes (id INT PRIMARY KEY, title TEXT, emb VECTOR(4))",
            &insert("notes", 1..=20, 4),
        ],
        Some(hook.clone()),
    );
    standby.run(&["CREATE INDEX docs_emb_pidx ON docs USING hnsw (emb) WITH (persistent = true)"]);
    let mut groups_before_definition = 0;
    loop {
        let records = standby.step().expect("the definition never arrived");
        if records
            .iter()
            .any(|(deleted, key)| !deleted && key.as_slice() == b"meta:index:docs_emb_pidx")
        {
            break;
        }
        if records.iter().any(|(_, key)| key.starts_with(b"__vidx:")) {
            groups_before_definition += 1;
            assert!(
                !standby.engine.vector_indexes().index_exists("docs_emb_pidx"),
                "registered before its definition"
            );
        }
    }
    assert!(
        groups_before_definition > 30,
        "the meta record and each backfilled vector arrive first: {groups_before_definition}"
    );
    let stats = hook.stats();
    assert!(stats.records_unowned > 0, "{stats:?}");
    assert_eq!(stats.registrations, 1, "{stats:?}");
    standby.sync();
    assert_same_persistent_index(&standby, "docs_emb_pidx", "once its definition arrived");

    standby.run(&[&insert("docs", 31..=40, 1), "DELETE FROM docs WHERE id = 2"]);
    standby.sync();
    assert_same_persistent_index(&standby, "docs_emb_pidx", "after DML on the new index");
    assert!(hook.stats().records_applied > 0);

    // DROP INDEX: the replicated deletes empty the keyspace, the definition's
    // delete forgets the copy.
    let prefix = persistent_prefix(&standby);
    standby.run(&["DROP INDEX docs_emb_pidx"]);
    standby.sync();
    assert!(!standby.engine.vector_indexes().index_exists("docs_emb_pidx"));
    assert!(
        standby_keys(&standby, &prefix).is_empty(),
        "the primary's deletes replicated"
    );

    // A second one on another populated table, then its table dropped.
    standby.run(&["CREATE INDEX notes_emb_pidx ON notes USING hnsw (emb) WITH (persistent = true)"]);
    standby.sync();
    assert_same_persistent_index(&standby, "notes_emb_pidx", "after a second CREATE INDEX");
    standby.run(&["DROP TABLE notes"]);
    standby.sync();
    assert!(!standby.engine.vector_indexes().index_exists("notes_emb_pidx"));

    let stats = hook.stats();
    assert_eq!(stats.reopens, 0, "{stats:?}");
    assert_eq!(stats.degraded, 0, "{stats:?}");
    assert_eq!(standby.refusals(), 0);
    assert_applier_clean(&standby);
}

#[cfg(feature = "vector-persist")]
#[test]
fn an_unknown_vidx_suffix_reopens_the_index_instead_of_panicking() {
    let hook = Arc::new(StandbyVectorHook::new());
    let standby = Standby::new(
        &[
            DOCS,
            &insert("docs", 1..=30, 0),
            "CREATE INDEX docs_emb_pidx ON docs USING hnsw (emb) WITH (persistent = true)",
        ],
        Some(hook.clone()),
    );
    // A record of the index's keyspace this binary does not know, written
    // straight into the primary's store (its own index never reads it).
    let mut key = persistent_prefix(&standby);
    key.extend_from_slice(b"bogus:1");
    standby.raw.put(&key, b"x").unwrap();
    standby.run(&[&insert("docs", [31], 1)]);
    standby.sync();
    let stats = hook.stats();
    assert_eq!(stats.reopens, 1, "the copy was replaced from its keys: {stats:?}");
    assert_same_persistent_index(&standby, "docs_emb_pidx", "after the reopen");
    // Still a known key family: nothing for a full re-derivation.
    assert_applier_clean(&standby);

    // And it follows on from there.
    standby.run(&[&insert("docs", 32..=36, 2), "DELETE FROM docs WHERE id = 1"]);
    standby.sync();
    assert_same_persistent_index(&standby, "docs_emb_pidx", "after DML past the reopen");
    assert_eq!(hook.stats().reopens, 1, "{:?}", hook.stats());
    assert_eq!(standby.refusals(), 0);
}
