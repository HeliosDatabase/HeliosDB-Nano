//! Spec P1.1: a physical standby keeps the serving engine's derived state
//! right as it applies its primary's raw batches.
//!
//! # Why
//!
//! P0 made a standby's default column family a verbatim copy of its
//! primary's. A `StorageEngine` serving it still answers PRIMARY KEY lookups
//! and `COUNT(*)` from in-memory ART indexes, `AS OF` from the snapshot map,
//! and a new reader's visibility from the MVCC timestamp counter — state only
//! the write path maintains, and nothing writes through a standby's engine.
//! `PhysicalApplier` maintains it from the keys each applied batch touched.
//!
//! # What
//!
//! Each case runs statements on a primary `EmbeddedDatabase`, stages a
//! physical standby from it (`OnlineCheckpointExport` → `ReceiverStaging` →
//! `LocalWriter`), opens the standby as a `StorageEngine`
//! (`open_physical_standby`, hydrated once by the applier) and feeds it the
//! primary's batches through `PhysicalApplier::apply`. At each settle point
//! the primary is checkpointed, the standby is brought to exactly the
//! checkpoint's source sequence (its store is then byte-identical to the
//! checkpoint), and the checkpoint is hydrated from scratch
//! (`EmbeddedDatabase::open_physical_snapshot`): the derived state a restart
//! computes from the same bytes. The standby's incrementally maintained
//! state must equal it exactly — every ART index entry for entry,
//! `pk_index_len`, `COUNT(*)`, PRIMARY KEY lookups, the snapshot map and its
//! counters, the branch registry — and its rows and lookups must equal the
//! primary's. `rebuilds == 0` proves the row-by-row path handled a case
//! rather than the scan fallback.
//!
//! # Red without the applier
//!
//! `bypassing_the_applier_leaves_the_derived_state_behind` applies the same
//! kind of batches with `LocalWriter::apply_source` alone — what a standby did
//! before P1.1 — and shows the comparison catches it: the PK index keeps its
//! open-time size, the new row's lookup misses, the snapshot map and the
//! MVCC timestamp stay at their open-time values. Every case below that
//! changes derived state compares that same state at its first settle point,
//! and so fails the same way without the applier (the sequence case is the
//! exception by design: its keys have nothing to publish).
#![cfg(all(feature = "ha-tier1", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::replication::checkpoint_transfer::{
    OnlineCheckpointExport, PhysicalStage, ReceiverStaging, TransferLimits,
};
use heliosdb_nano::replication::history::load_or_create_primary_history;
use heliosdb_nano::replication::physical::{poll_source, range_digest, MAX_BATCH_BYTES};
use heliosdb_nano::replication::physical_applier::{
    Applied, NoopDerivedStateListener, PhysicalApplier, RANGE_ENUMERATION_CAP,
};
use heliosdb_nano::storage::{ArtIndexManager, StorageEngine, WriteGate};
use heliosdb_nano::{Config, EmbeddedDatabase, Tuple, Value};
use rocksdb::{checkpoint::Checkpoint, WriteBatch, DB};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Harness: primary → checkpoint export → stage → LocalWriter → standby engine
// ---------------------------------------------------------------------------

/// A primary's configuration: persistent, with retained RocksDB WAL so a
/// stage can continue from its checkpoint cut.
fn primary_config(path: &Path) -> Config {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    config.replication.physical.enabled = true;
    config
}

/// A sealed checkpoint's configuration, for the fresh reference hydration.
fn snapshot_config(path: &Path) -> Config {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    config
}

/// Export a checkpoint of the live primary `source`, download it into `root`
/// and open it as a stage: the production path up to the first cursor.
/// (The same steps as `tests/physical_standby_local_namespace.rs`.)
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

/// A primary and its physical standby. Fields drop in declaration order: the
/// applier and the standby engine release the writer before the stage that
/// owns it, and everything before the directory they live in.
struct Pair {
    applier: PhysicalApplier,
    standby: Arc<StorageEngine>,
    _stage: PhysicalStage,
    raw: Arc<DB>,
    primary: EmbeddedDatabase,
    history: Uuid,
    references: usize,
    root: tempfile::TempDir,
}

/// The primary's checkpoint at source sequence `at`, hydrated from scratch.
struct Reference {
    database: EmbeddedDatabase,
    at: u64,
    /// What the standby's applier reported while reaching `at`.
    applied: Vec<Applied>,
}

impl Reference {
    fn engine(&self) -> &StorageEngine {
        &self.database.storage
    }
}

impl Pair {
    /// A primary that ran `setup`, and a standby staged from it right after.
    fn new(setup: &[&str]) -> Self {
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
        let standby = Arc::new(
            StorageEngine::open_physical_standby(
                &path,
                &primary_config(&path),
                Arc::clone(stage.writer()),
                WriteGate::closed("physical standby"),
            )
            .unwrap(),
        );
        let applier = PhysicalApplier::new(
            Arc::clone(&standby),
            Arc::clone(stage.writer()),
            Arc::new(NoopDerivedStateListener),
        )
        .unwrap();
        applier.hydrate().unwrap();
        Self {
            applier,
            standby,
            _stage: stage,
            raw,
            primary,
            history,
            references: 0,
            root,
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

    /// Apply source batches until the standby's cursor is exactly `through`.
    fn apply_through(&self, through: u64) -> Vec<Applied> {
        let mut applied = Vec::new();
        'poll: while self.cursor() < through {
            let poll = poll_source(&self.raw, self.history, self.cursor(), MAX_BATCH_BYTES).unwrap();
            assert!(!poll.batches.is_empty(), "the primary has nothing up to {through}");
            for batch in &poll.batches {
                assert!(
                    batch.last_sequence() <= through,
                    "source batch {}..={} crosses the cut {through}",
                    batch.first_sequence(),
                    batch.last_sequence()
                );
                let result = self
                    .applier
                    .apply(batch)
                    .unwrap_or_else(|e| panic!("applying {}..={}: {e}", batch.first_sequence(), batch.last_sequence()));
                applied.push(result);
                if batch.last_sequence() == through {
                    break 'poll;
                }
            }
        }
        assert_eq!(self.cursor(), through);
        applied
    }

    /// Apply everything the primary has, timing the applier alone.
    fn sync(&self) -> (Vec<Applied>, Duration) {
        let mut applied = Vec::new();
        let mut spent = Duration::ZERO;
        loop {
            let poll = poll_source(&self.raw, self.history, self.cursor(), MAX_BATCH_BYTES).unwrap();
            if poll.batches.is_empty() {
                return (applied, spent);
            }
            for batch in &poll.batches {
                let started = Instant::now();
                applied.push(self.applier.apply(batch).unwrap());
                spent += started.elapsed();
            }
        }
    }

    /// Checkpoint the primary and hydrate the checkpoint from scratch, without
    /// touching the standby.
    fn reference_at_head(&mut self) -> Reference {
        self.references += 1;
        let path = self.root.path().join(format!("reference-{}", self.references));
        Checkpoint::new(&self.raw).unwrap().create_checkpoint(&path).unwrap();
        let hydrated = EmbeddedDatabase::open_physical_snapshot(snapshot_config(&path))
            .unwrap_or_else(|e| panic!("hydrating the reference checkpoint: {e}"));
        let at = hydrated.database.storage.db().latest_sequence_number();
        Reference {
            database: hydrated.database,
            at,
            applied: Vec::new(),
        }
    }

    /// The reference at the primary's current head, and the standby brought
    /// to exactly its sequence through the applier. Their stores are then
    /// byte-identical.
    fn settle(&mut self) -> Reference {
        let mut reference = self.reference_at_head();
        reference.applied = self.apply_through(reference.at);
        assert_eq!(
            range_digest(self.applier.writer().db(), None, b"").unwrap(),
            range_digest(&reference.engine().db(), None, b"").unwrap(),
            "the standby's store differs from the primary's checkpoint at {}",
            reference.at
        );
        reference
    }
}

// ---------------------------------------------------------------------------
// Comparisons
// ---------------------------------------------------------------------------

fn sorted_rows(mut rows: Vec<Tuple>) -> Vec<Tuple> {
    rows.sort_by_key(|row| row.row_id);
    rows
}

fn rows(engine: &StorageEngine, table: &str) -> Option<Vec<Tuple>> {
    engine.scan_table(table).ok().map(sorted_rows)
}

/// Every ART index of `table`, entry for entry, keyed by kind and columns
/// (constraint index NAMES are minted, so an incremental registration and a
/// fresh one may name the same index differently).
type IndexCensus = BTreeMap<(String, Vec<String>), Vec<Vec<(Vec<u8>, u64)>>>;

fn index_census(engine: &StorageEngine, table: &str) -> IndexCensus {
    let art = engine.art_indexes();
    let mut census = IndexCensus::new();
    for (name, kind, columns) in art.list_table_indexes(table) {
        // Row ids under one key of a non-unique index come back in insertion
        // order, which an incremental and a fresh build need not share.
        let mut entries = art.index_range_scan(&name, None, None, None);
        entries.sort();
        census.entry((format!("{kind:?}"), columns)).or_default().push(entries);
    }
    for entries in census.values_mut() {
        entries.sort();
    }
    census
}

/// The position of `table`'s single-column PRIMARY KEY, if it has one.
fn pk_column(engine: &StorageEngine, table: &str) -> Option<usize> {
    let schema = engine.catalog().get_table_schema(table).ok()?;
    let mut keys = schema.columns.iter().enumerate().filter(|(_, c)| c.primary_key);
    match (keys.next(), keys.next()) {
        (Some((index, _)), None) => Some(index),
        _ => None,
    }
}

fn snapshot_census(engine: &StorageEngine) -> Vec<(u64, u64, u64, i64)> {
    engine
        .snapshot_manager()
        .list_snapshots()
        .unwrap()
        .into_iter()
        .map(|s| (s.timestamp, s.transaction_id, s.scn, s.wall_clock_micros))
        .collect()
}

fn branch_names(engine: &StorageEngine) -> Option<Vec<String>> {
    let mut names: Vec<String> = engine.list_branches().ok()?.into_iter().map(|b| b.name).collect();
    names.sort();
    Some(names)
}

/// The standby's derived state equals a fresh hydration of the same bytes,
/// and its answers equal the primary's (`primary_too`; false where the test
/// wrote below the primary's engine, or where the primary's own live indexes
/// are known to lag its rows, as after MERGE BRANCH).
fn assert_same(pair: &Pair, reference: &Reference, tables: &[&str], primary_too: bool) {
    let standby = &*pair.standby;
    let fresh = reference.engine();
    for &table in tables {
        let context = format!("table '{table}' at source sequence {}", reference.at);
        let expected = rows(fresh, table);
        assert_eq!(rows(standby, table), expected, "rows of {context}");
        if primary_too {
            assert_eq!(
                rows(&pair.primary.storage, table),
                expected,
                "the primary's rows of {context}"
            );
        }
        assert_eq!(
            standby.art_indexes().pk_index_len(table),
            fresh.art_indexes().pk_index_len(table),
            "pk_index_len (the COUNT(*) fast path) of {context}"
        );
        assert_eq!(
            standby.count_table_rows(table).ok(),
            fresh.count_table_rows(table).ok(),
            "COUNT(*) of {context}"
        );
        assert_eq!(
            index_census(standby, table),
            index_census(fresh, table),
            "ART indexes of {context}"
        );
        if let (Some(rows), Some(pk)) = (&expected, pk_column(fresh, table)) {
            let step = (rows.len() / 200).max(1);
            for row in rows.iter().step_by(step) {
                let key = &row.values[pk];
                let found = standby.get_row_by_pk(table, key).unwrap();
                assert!(found.is_some(), "PK lookup {key:?} of {context} misses on the standby");
                assert_eq!(
                    found,
                    fresh.get_row_by_pk(table, key).unwrap(),
                    "PK lookup {key:?} of {context}"
                );
                if primary_too {
                    assert_eq!(
                        found,
                        pair.primary.storage.get_row_by_pk(table, key).unwrap(),
                        "PK lookup {key:?} of {context} against the primary"
                    );
                }
            }
            if matches!(rows.first().map(|row| &row.values[pk]), Some(Value::Int4(_))) {
                let absent = Value::Int4(i32::MAX);
                assert_eq!(
                    standby.get_row_by_pk(table, &absent).unwrap(),
                    None,
                    "a PK miss of {context}"
                );
            }
        }
        if let Some(highest) = expected.iter().flatten().filter_map(|row| row.row_id).max() {
            let mark = standby.row_counter_high_water(table).unwrap_or(0);
            assert!(
                mark >= highest,
                "row counter of {context}: {mark} below row id {highest}"
            );
        }
    }
    // Time travel: the snapshot map, its counters, the GC horizon, and a
    // timestamp counter at least as far as a fresh open puts it.
    assert_eq!(
        snapshot_census(standby),
        snapshot_census(fresh),
        "snapshot map at {}",
        reference.at
    );
    assert_eq!(
        standby.snapshot_manager().current_scn(),
        fresh.snapshot_manager().current_scn(),
        "SCN counter at {}",
        reference.at
    );
    assert_eq!(
        standby.snapshot_manager().current_transaction_id(),
        fresh.snapshot_manager().current_transaction_id(),
        "transaction-id counter at {}",
        reference.at
    );
    assert_eq!(
        standby.snapshot_manager().gc_low_watermark(),
        fresh.snapshot_manager().gc_low_watermark()
    );
    assert!(
        standby.current_timestamp() >= fresh.current_timestamp(),
        "the standby's MVCC timestamp {} is behind a fresh open's {}",
        standby.current_timestamp(),
        fresh.current_timestamp()
    );
    assert_eq!(
        branch_names(standby),
        branch_names(fresh),
        "branch registry at {}",
        reference.at
    );
    assert_eq!(
        standby.branch_manager().map(|m| m.has_user_branches_registered()),
        fresh.branch_manager().map(|m| m.has_user_branches_registered())
    );
}

/// A reader that starts now on the standby sees every applied commit: its
/// snapshot (the next timestamp) resolves to what a reader starting now on
/// the primary resolves. The primary allocates every commit timestamp from
/// its own counter, so its next timestamp is past all of them; the standby's
/// must be too, or versions the primary committed later than the standby's
/// open stay invisible to it.
fn assert_new_readers_see_every_commit(pair: &Pair, table: &str) {
    let reader = pair.standby.current_timestamp() + 1;
    let primary_reader = pair.primary.storage.current_timestamp() + 1;
    assert_eq!(
        sorted_rows(pair.standby.scan_table_at_snapshot(table, reader).unwrap()),
        sorted_rows(
            pair.primary
                .storage
                .scan_table_at_snapshot(table, primary_reader)
                .unwrap()
        ),
        "a reader of '{table}' at the standby's next timestamp {reader} misses applied commits"
    );
}

fn assert_no_fallback(pair: &Pair) {
    let stats = pair.applier.stats();
    assert_eq!(
        stats.rebuilds, 0,
        "a case the row-by-row path must handle fell back: {stats:?}"
    );
    assert_eq!(stats.pending_tables, 0, "{stats:?}");
    assert!(!stats.derived_state_suspect, "{stats:?}");
    assert_eq!(stats.unknown_keys, 0, "{stats:?}");
}

// ---------------------------------------------------------------------------
// The non-vacuity control
// ---------------------------------------------------------------------------

#[test]
fn bypassing_the_applier_leaves_the_derived_state_behind() {
    let mut pair = Pair::new(&[
        "CREATE TABLE items (id INT PRIMARY KEY, qty INT)",
        "INSERT INTO items VALUES (1, 10), (2, 20)",
    ]);
    pair.run(&[
        "INSERT INTO items VALUES (3, 30)",
        "BEGIN",
        "UPDATE items SET qty = 11 WHERE id = 1",
        "COMMIT",
    ]);
    // What a standby did before P1.1: the bytes, and nothing else.
    let reference = pair.reference_at_head();
    'poll: while pair.cursor() < reference.at {
        let poll = poll_source(&pair.raw, pair.history, pair.cursor(), MAX_BATCH_BYTES).unwrap();
        for batch in &poll.batches {
            assert!(pair.applier.writer().apply_source(batch).unwrap());
            if batch.last_sequence() == reference.at {
                break 'poll;
            }
        }
    }
    assert_eq!(
        rows(&pair.standby, "items"),
        rows(reference.engine(), "items"),
        "the bytes did arrive"
    );
    assert_eq!(
        pair.standby.art_indexes().pk_index_len("items"),
        Some(2),
        "the PK index kept its open-time size"
    );
    assert_eq!(reference.engine().art_indexes().pk_index_len("items"), Some(3));
    assert_eq!(pair.standby.get_row_by_pk("items", &Value::Int4(3)).unwrap(), None);
    assert!(reference
        .engine()
        .get_row_by_pk("items", &Value::Int4(3))
        .unwrap()
        .is_some());
    assert_ne!(
        index_census(&pair.standby, "items"),
        index_census(reference.engine(), "items")
    );
    assert_ne!(snapshot_census(&pair.standby), snapshot_census(reference.engine()));
    assert!(pair.standby.current_timestamp() < reference.engine().current_timestamp());

    // The full re-derivation brings the same bytes' derived state back.
    pair.applier.rebuild_all_derived_state().unwrap();
    assert_same(&pair, &reference, &["items"], true);
}

// ---------------------------------------------------------------------------
// DML
// ---------------------------------------------------------------------------

#[test]
fn inserts_updates_and_deletes_single_and_multi_row() {
    let mut pair = Pair::new(&[
        "CREATE TABLE items (id INT PRIMARY KEY, name TEXT, qty INT)",
        "INSERT INTO items VALUES (1, 'one', 10), (2, 'two', 20), (3, 'three', 30)",
    ]);
    // A table created after the cut: its row counter comes from the applier
    // alone, so it must equal the primary's exactly.
    pair.run(&["CREATE TABLE notes (id INT PRIMARY KEY, body TEXT UNIQUE)"]);
    let steps: &[&[&str]] = &[
        &[
            "INSERT INTO items VALUES (4, 'four', 40)",
            "INSERT INTO notes VALUES (1, 'first')",
        ],
        &[
            "INSERT INTO items VALUES (5, 'five', 50), (6, 'six', 60), (7, 'seven', 70)",
            "INSERT INTO notes VALUES (2, 'second'), (3, 'third')",
        ],
        &[
            "UPDATE items SET qty = 41 WHERE id = 4",
            "UPDATE items SET qty = qty + 1 WHERE id <= 3",
            "UPDATE notes SET body = 'renamed' WHERE id = 2",
        ],
        &[
            "DELETE FROM items WHERE id = 2",
            "DELETE FROM items WHERE id >= 6",
            "DELETE FROM notes WHERE id = 1",
        ],
    ];
    for statements in steps {
        pair.run(statements);
        let reference = pair.settle();
        assert_same(&pair, &reference, &["items", "notes"], true);
        assert_new_readers_see_every_commit(&pair, "items");
        assert_eq!(
            pair.standby.row_counter_high_water("notes"),
            pair.primary.storage.row_counter_high_water("notes"),
            "row-id high-water of a table created after the cut"
        );
    }
    // The UNIQUE probe follows the update: the old value is free, the new taken.
    let body = vec!["body".to_string()];
    let art = pair.standby.art_indexes();
    assert!(art.unique_key_exists("notes", &body, &[Value::String("renamed".into())]));
    assert!(!art.unique_key_exists("notes", &body, &[Value::String("second".into())]));
    assert!(!art.unique_key_exists("notes", &body, &[Value::String("first".into())]));

    // An exact retransmission publishes nothing.
    pair.run(&["INSERT INTO items VALUES (8, 'eight', 80)"]);
    let poll = poll_source(&pair.raw, pair.history, pair.cursor(), MAX_BATCH_BYTES).unwrap();
    for batch in &poll.batches {
        pair.applier.apply(batch).unwrap();
    }
    let last = poll.batches.last().unwrap();
    let batches = pair.applier.stats().batches;
    let again = pair.applier.apply(last).unwrap();
    assert_eq!(again.rows, 0);
    assert_eq!(again.source_sequence, last.last_sequence());
    assert_eq!(pair.applier.stats().batches, batches);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["items", "notes"], true);
    assert_no_fallback(&pair);
}

// ---------------------------------------------------------------------------
// Index and constraint DDL
// ---------------------------------------------------------------------------

#[test]
fn create_index_on_a_populated_table_and_drop_it() {
    let mut setup = vec!["CREATE TABLE items (id INT PRIMARY KEY, name TEXT, qty INT)".to_string()];
    let values: Vec<String> = (1..=50).map(|id| format!("({id}, 'item-{id}', {})", id % 7)).collect();
    setup.push(format!("INSERT INTO items VALUES {}", values.join(", ")));
    let setup: Vec<&str> = setup.iter().map(String::as_str).collect();
    let mut pair = Pair::new(&setup);

    pair.run(&["CREATE INDEX items_qty_idx ON items (qty)"]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["items"], true);
    let lookup = |engine: &StorageEngine, qty: i32| {
        let art = engine.art_indexes();
        let name = art
            .find_column_index("items", "qty")
            .expect("the qty index is registered");
        let mut ids = art.index_get_all(&name, &ArtIndexManager::encode_key(&[Value::Int4(qty)]));
        ids.sort_unstable();
        ids
    };
    assert_eq!(
        lookup(pair.standby.as_ref(), 3).len(),
        7,
        "a secondary-index lookup on the standby"
    );
    assert_eq!(lookup(pair.standby.as_ref(), 3), lookup(reference.engine(), 3));

    pair.run(&[
        "INSERT INTO items VALUES (51, 'item-51', 3)",
        "UPDATE items SET qty = 3 WHERE id = 1",
        "CREATE UNIQUE INDEX items_name_uidx ON items (name)",
    ]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["items"], true);
    assert_eq!(lookup(pair.standby.as_ref(), 3), lookup(reference.engine(), 3));
    let name = vec!["name".to_string()];
    assert!(pair
        .standby
        .art_indexes()
        .unique_key_exists("items", &name, &[Value::String("item-51".into())]));

    pair.run(&["DROP INDEX items_qty_idx"]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["items"], true);
    assert!(pair.standby.art_indexes().find_column_index("items", "qty").is_none());
    assert_no_fallback(&pair);
}

#[test]
fn add_constraint_unique_on_a_populated_table() {
    let mut pair = Pair::new(&[
        "CREATE TABLE accounts (id INT PRIMARY KEY, email TEXT, region TEXT)",
        "INSERT INTO accounts VALUES (1, 'a@example.com', 'eu'), (2, 'b@example.com', 'us')",
    ]);
    pair.run(&["ALTER TABLE accounts ADD CONSTRAINT accounts_email_uq UNIQUE (email)"]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["accounts"], true);
    let email = vec!["email".to_string()];
    let probe = |engine: &StorageEngine, value: &str| {
        engine
            .art_indexes()
            .unique_key_exists("accounts", &email, &[Value::String(value.into())])
    };
    assert!(
        probe(pair.standby.as_ref(), "a@example.com"),
        "the backfilled UNIQUE index holds the existing row"
    );
    assert!(!probe(pair.standby.as_ref(), "c@example.com"));

    pair.run(&["INSERT INTO accounts VALUES (3, 'c@example.com', 'eu')"]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["accounts"], true);
    assert_eq!(
        probe(pair.standby.as_ref(), "c@example.com"),
        probe(reference.engine(), "c@example.com")
    );
    assert!(probe(pair.standby.as_ref(), "c@example.com"));
    assert_no_fallback(&pair);
}

// ---------------------------------------------------------------------------
// Column DDL
// ---------------------------------------------------------------------------

#[test]
fn add_column_then_insert_in_the_same_and_in_separate_batches() {
    let mut pair = Pair::new(&[
        "CREATE TABLE people (id INT PRIMARY KEY, name TEXT)",
        "INSERT INTO people VALUES (1, 'ada'), (2, 'bob'), (3, 'cy')",
        "CREATE INDEX people_name_idx ON people (name)",
    ]);
    // Same application step: the ALTER's and the INSERT's batches together.
    pair.run(&[
        "ALTER TABLE people ADD COLUMN age INT",
        "INSERT INTO people VALUES (4, 'dee', 40)",
    ]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["people"], true);

    // Separate steps.
    pair.run(&["ALTER TABLE people ADD COLUMN city TEXT"]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["people"], true);
    pair.run(&["INSERT INTO people VALUES (5, 'eve', 50, 'oslo')"]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["people"], true);
    // A row written before both ALTERs, rewritten after them.
    pair.run(&["UPDATE people SET city = 'rome', name = 'ada2' WHERE id = 1"]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["people"], true);
    assert_new_readers_see_every_commit(&pair, "people");
    assert_no_fallback(&pair);
}

#[test]
fn rename_column_repoints_the_live_indexes() {
    let mut pair = Pair::new(&[
        "CREATE TABLE accounts (id INT PRIMARY KEY, username TEXT UNIQUE, score INT)",
        "INSERT INTO accounts VALUES (1, 'ada', 10), (2, 'bob', 20)",
    ]);
    pair.run(&["ALTER TABLE accounts RENAME COLUMN username TO login"]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["accounts"], true);
    let login = vec!["login".to_string()];
    assert!(pair
        .standby
        .art_indexes()
        .unique_key_exists("accounts", &login, &[Value::String("ada".into())]));

    pair.run(&[
        "INSERT INTO accounts VALUES (3, 'cy', 30)",
        "UPDATE accounts SET login = 'bobby' WHERE id = 2",
    ]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["accounts"], true);
    assert_no_fallback(&pair);
}

// ---------------------------------------------------------------------------
// TRUNCATE, range deletions, DROP TABLE
// ---------------------------------------------------------------------------

#[test]
fn truncate_and_a_range_deletion_empty_the_indexes() {
    let mut pair = Pair::new(&[
        "CREATE TABLE logs (id INT PRIMARY KEY, msg TEXT)",
        "CREATE INDEX logs_msg_idx ON logs (msg)",
        "INSERT INTO logs VALUES (1, 'a'), (2, 'b'), (3, 'a')",
    ]);
    pair.run(&["TRUNCATE TABLE logs"]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["logs"], true);
    assert_eq!(pair.standby.art_indexes().pk_index_len("logs"), Some(0));

    pair.run(&["INSERT INTO logs VALUES (4, 'c'), (5, 'a'), (6, 'b')"]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["logs"], true);

    // No statement issues a range deletion today, so write one below the
    // primary's engine. The primary's own indexes then lag its rows; the
    // standby is compared with the fresh hydration only.
    let mut range = WriteBatch::default();
    range.delete_range(b"data:logs:".as_slice(), b"data:logs;".as_slice());
    pair.raw.write(range).unwrap();
    let reference = pair.settle();
    assert_same(&pair, &reference, &["logs"], false);
    assert_eq!(pair.standby.art_indexes().pk_index_len("logs"), Some(0));
    assert_eq!(pair.standby.count_table_rows("logs").unwrap(), 0);
    assert_no_fallback(&pair);
}

#[test]
fn a_range_deletion_wider_than_the_enumeration_cap_rederives_its_table() {
    let mut pair = Pair::new(&["CREATE TABLE bulk (id INT PRIMARY KEY, tag INT)"]);
    let total = RANGE_ENUMERATION_CAP + 50;
    let mut next = 1;
    while next <= total {
        let last = (next + 499).min(total);
        let values: Vec<String> = (next..=last).map(|id| format!("({id}, {})", id % 13)).collect();
        let insert = format!("INSERT INTO bulk VALUES {}", values.join(", "));
        pair.run(&[insert.as_str()]);
        next = last + 1;
    }
    pair.run(&["CREATE INDEX bulk_tag_idx ON bulk (tag)"]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["bulk"], true);
    assert_no_fallback(&pair);

    let mut range = WriteBatch::default();
    range.delete_range(b"data:bulk:".as_slice(), b"data:bulk;".as_slice());
    pair.raw.write(range).unwrap();
    let reference = pair.settle();
    assert!(
        reference
            .applied
            .iter()
            .any(|a| a.tables_rebuilt.iter().any(|t| t == "bulk")),
        "the over-wide range must re-derive 'bulk': {:?}",
        reference.applied
    );
    assert_same(&pair, &reference, &["bulk"], false);
    assert_eq!(pair.standby.art_indexes().pk_index_len("bulk"), Some(0));
    let stats = pair.applier.stats();
    assert_eq!(stats.rebuilds, 1, "{stats:?}");
    assert_eq!(stats.pending_tables, 0, "{stats:?}");
    assert!(!stats.derived_state_suspect, "{stats:?}");
}

#[test]
fn drop_table_retires_its_indexes() {
    let mut pair = Pair::new(&[
        "CREATE TABLE kept (id INT PRIMARY KEY, v TEXT)",
        "INSERT INTO kept VALUES (1, 'a'), (2, 'b')",
        "CREATE TABLE doomed (id INT PRIMARY KEY, v TEXT UNIQUE)",
        "CREATE INDEX doomed_v_idx ON doomed (v)",
        "INSERT INTO doomed VALUES (1, 'a'), (2, 'b')",
    ]);
    // A heap table (no key, no constraint record): after its DROP the store
    // still hydrates strictly, so it is compared with a fresh hydration.
    pair.run(&[
        "CREATE TABLE heap (k INT, v TEXT)",
        "CREATE INDEX heap_v_idx ON heap (v)",
        "INSERT INTO heap VALUES (1, 'a'), (2, 'b')",
    ]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["kept", "doomed", "heap"], true);
    pair.run(&["DROP TABLE heap"]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["kept", "doomed", "heap"], true);
    assert!(pair.standby.art_indexes().list_table_indexes("heap").is_empty());

    // Re-created under the same name: fresh registrations, no stale entries.
    pair.run(&[
        "CREATE TABLE heap (k INT, w INT)",
        "CREATE INDEX heap_w_idx ON heap (w)",
        "INSERT INTO heap VALUES (7, 70)",
    ]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["kept", "heap"], true);

    // A table with a PRIMARY KEY leaves its `table_constraints:` record behind
    // on DROP TABLE (`Catalog::drop_table` never deletes it), and strict
    // hydration refuses that orphan — so past this point no fresh reference
    // exists. The standby is asserted directly.
    pair.run(&["DROP TABLE doomed"]);
    let _ = pair.sync();
    let standby = pair.standby.as_ref();
    assert!(standby.art_indexes().list_table_indexes("doomed").is_empty());
    assert!(!standby.catalog().table_exists("doomed").unwrap());
    assert!(standby.scan_table("doomed").is_err());
    assert_eq!(standby.count_table_rows("doomed").unwrap(), 0);
    assert_eq!(
        rows(standby, "kept"),
        rows(&pair.primary.storage, "kept"),
        "the surviving table is untouched"
    );
    assert_eq!(standby.art_indexes().pk_index_len("kept"), Some(2));
    assert_no_fallback(&pair);
}

// ---------------------------------------------------------------------------
// Transactions and time travel
// ---------------------------------------------------------------------------

#[test]
fn create_table_and_insert_in_one_explicit_transaction() {
    let mut pair = Pair::new(&[]);
    pair.run(&[
        "BEGIN",
        "CREATE TABLE fresh (id INT PRIMARY KEY, a INT, b INT, UNIQUE (a, b))",
        "INSERT INTO fresh VALUES (1, 1, 1), (2, 1, 2), (3, 2, 1)",
        "COMMIT",
    ]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["fresh"], true);
    assert_eq!(
        pair.standby.row_counter_high_water("fresh"),
        pair.primary.storage.row_counter_high_water("fresh")
    );
    let ab = vec!["a".to_string(), "b".to_string()];
    let art = pair.standby.art_indexes();
    assert!(art.unique_key_exists("fresh", &ab, &[Value::Int4(1), Value::Int4(2)]));
    assert!(!art.unique_key_exists("fresh", &ab, &[Value::Int4(2), Value::Int4(2)]));
    assert_new_readers_see_every_commit(&pair, "fresh");
    assert_no_fallback(&pair);
}

#[test]
fn versioned_commits_resolve_as_of_on_the_standby() {
    let mut pair = Pair::new(&[
        "CREATE TABLE ledger (id INT PRIMARY KEY, amount INT)",
        "INSERT INTO ledger VALUES (1, 100), (2, 200)",
    ]);
    pair.run(&["BEGIN", "UPDATE ledger SET amount = 150 WHERE id = 1", "COMMIT"]);
    let first = pair.settle();
    assert_same(&pair, &first, &["ledger"], true);
    let at_first = first
        .engine()
        .snapshot_manager()
        .max_snapshot_timestamp()
        .expect("the commit registered a snapshot");
    assert_new_readers_see_every_commit(&pair, "ledger");

    pair.run(&[
        "BEGIN",
        "INSERT INTO ledger VALUES (3, 300)",
        "UPDATE ledger SET amount = 250 WHERE id = 2",
        "COMMIT",
    ]);
    let second = pair.settle();
    assert_same(&pair, &second, &["ledger"], true);
    let at_second = second.engine().snapshot_manager().max_snapshot_timestamp().unwrap();
    assert!(at_second > at_first);
    assert!(pair.standby.current_timestamp() >= at_second);
    for at in [at_first, at_second] {
        let standby = sorted_rows(pair.standby.scan_table_at_snapshot("ledger", at).unwrap());
        assert_eq!(
            standby,
            sorted_rows(second.engine().scan_table_at_snapshot("ledger", at).unwrap()),
            "AS OF {at}"
        );
        assert_eq!(
            standby,
            sorted_rows(pair.primary.storage.scan_table_at_snapshot("ledger", at).unwrap()),
            "AS OF {at} against the primary"
        );
    }
    assert_new_readers_see_every_commit(&pair, "ledger");
    assert_no_fallback(&pair);
}

/// Snapshot GC deletes `snapshot:` records with point deletes; a range
/// deletion over them is enumerated; one too wide to enumerate (or bytes that
/// reached the store past the applier) leaves the batch unable to name them,
/// and the full re-derivation must then forget them. The oldest records go:
/// GC always keeps the newest, so the counters stay comparable.
#[test]
fn a_deleted_snapshot_record_is_forgotten_in_every_form() {
    let mut pair = Pair::new(&[
        "CREATE TABLE ledger (id INT PRIMARY KEY, amount INT)",
        "INSERT INTO ledger VALUES (1, 100)",
    ]);
    pair.run(&[
        "INSERT INTO ledger VALUES (2, 200)",
        "INSERT INTO ledger VALUES (3, 300)",
        "INSERT INTO ledger VALUES (4, 400)",
    ]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["ledger"], true);
    let timestamps: Vec<u64> = snapshot_census(&pair.standby).iter().map(|s| s.0).collect();
    assert!(timestamps.len() >= 4, "{timestamps:?}");
    let record = |timestamp: u64| format!("snapshot:{timestamp}").into_bytes();

    // A point delete, as GC writes it.
    let mut delete = WriteBatch::default();
    delete.delete(record(timestamps[0]));
    pair.raw.write(delete).unwrap();
    let reference = pair.settle();
    assert_same(&pair, &reference, &["ledger"], true);
    assert_eq!(snapshot_census(&pair.standby).len(), timestamps.len() - 1);

    // A range deletion covering exactly the next record: enumerated.
    let start = record(timestamps[1]);
    let mut end = start.clone();
    end.push(0);
    let mut range = WriteBatch::default();
    range.delete_range(start.as_slice(), end.as_slice());
    pair.raw.write(range).unwrap();
    let reference = pair.settle();
    assert_same(&pair, &reference, &["ledger"], true);
    assert_eq!(snapshot_census(&pair.standby).len(), timestamps.len() - 2);

    // A deletion the applier never saw: the full re-derivation reconciles the
    // map with the records.
    let mut unseen = WriteBatch::default();
    unseen.delete(record(timestamps[2]));
    pair.raw.write(unseen).unwrap();
    let poll = poll_source(&pair.raw, pair.history, pair.cursor(), MAX_BATCH_BYTES).unwrap();
    for batch in &poll.batches {
        assert!(pair.applier.writer().apply_source(batch).unwrap());
    }
    assert_eq!(
        snapshot_census(&pair.standby).len(),
        timestamps.len() - 2,
        "applied past the applier, the record is still listed"
    );
    pair.applier.rebuild_all_derived_state().unwrap();
    let reference = pair.settle();
    assert_same(&pair, &reference, &["ledger"], true);
    assert_eq!(snapshot_census(&pair.standby).len(), timestamps.len() - 3);
}

// ---------------------------------------------------------------------------
// Branches, storage modes, sequences
// ---------------------------------------------------------------------------

#[test]
fn branch_create_write_and_merge() {
    let mut pair = Pair::new(&[
        "CREATE TABLE t (id INT PRIMARY KEY, val TEXT)",
        "INSERT INTO t VALUES (1, 'main_row')",
    ]);
    pair.run(&["CREATE BRANCH dev AS OF NOW"]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["t"], true);
    assert_eq!(
        pair.standby.branch_manager().map(|m| m.has_user_branches_registered()),
        Some(true),
        "the standby's branch registry follows the primary's"
    );

    // MERGE writes main's `data:` keys outside the primary's index funnels,
    // so the PRIMARY's live indexes lag its rows until it reopens; the
    // standby must equal the fresh hydration (what a reopen computes).
    pair.run(&[
        "USE BRANCH dev",
        "INSERT INTO t VALUES (2, 'dev_row')",
        "USE BRANCH main",
    ]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["t"], false);
    pair.run(&["MERGE BRANCH dev INTO main"]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["t"], false);
    assert!(pair.standby.get_row_by_pk("t", &Value::Int4(2)).unwrap().is_some());
    assert_no_fallback(&pair);
}

#[test]
fn dictionary_and_content_addressed_migrations_converge() {
    let mut pair = Pair::new(&[]);
    pair.run(&[
        "CREATE TABLE ks_dict (id INT PRIMARY KEY, status TEXT, note TEXT)",
        "INSERT INTO ks_dict VALUES (1, 'open', 'a')",
        "INSERT INTO ks_dict VALUES (2, 'open', 'b')",
        "CREATE INDEX ks_dict_status_idx ON ks_dict (status)",
    ]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["ks_dict"], true);
    pair.run(&["ALTER TABLE ks_dict ALTER COLUMN status SET STORAGE DICTIONARY"]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["ks_dict"], true);
    pair.run(&["INSERT INTO ks_dict VALUES (3, 'closed', 'c')"]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["ks_dict"], true);

    // Over `content_addr::CAS_MIN_SIZE` (1 KiB), so the payloads go to `cas:`.
    let first = format!("INSERT INTO ks_cas VALUES (1, '{}', 'n')", "x".repeat(1200));
    let second = format!("INSERT INTO ks_cas VALUES (2, '{}', 'm')", "y".repeat(1200));
    pair.run(&[
        "CREATE TABLE ks_cas (id INT PRIMARY KEY, body TEXT, note TEXT)",
        first.as_str(),
        "CREATE INDEX ks_cas_note_idx ON ks_cas (note)",
        "ALTER TABLE ks_cas ALTER COLUMN body SET STORAGE CONTENT_ADDRESSED",
        second.as_str(),
    ]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["ks_dict", "ks_cas"], true);
    // A migration rewrites rows before it publishes the storage mode, so the
    // table is re-derived once its schema lands; nothing stays pending.
    let stats = pair.applier.stats();
    assert_eq!(stats.pending_tables, 0, "{stats:?}");
    assert!(!stats.derived_state_suspect, "{stats:?}");
    assert_eq!(stats.unknown_keys, 0, "{stats:?}");
}

#[test]
fn sequence_nextval_has_nothing_to_publish() {
    let mut pair = Pair::new(&[
        "CREATE TABLE items (id INT PRIMARY KEY)",
        "INSERT INTO items VALUES (1)",
    ]);
    pair.run(&["CREATE SEQUENCE ticket START 7"]);
    pair.primary.query("SELECT nextval('ticket')", &[]).unwrap();
    pair.primary.query("SELECT nextval('ticket')", &[]).unwrap();
    let reference = pair.settle();
    assert!(!reference.applied.is_empty(), "the sequence's keys arrive as batches");
    assert_same(&pair, &reference, &["items"], true);
    assert_eq!(
        pair.standby.catalog().get_sequence_state("ticket").unwrap(),
        reference.engine().catalog().get_sequence_state("ticket").unwrap()
    );
    assert!(pair.standby.catalog().get_sequence_state("ticket").unwrap().is_some());
    assert_no_fallback(&pair);
}

// ---------------------------------------------------------------------------
// Fail-closed paths
// ---------------------------------------------------------------------------

#[test]
fn an_unclassified_key_marks_the_state_suspect_without_stopping_the_applier() {
    let mut pair = Pair::new(&[
        "CREATE TABLE items (id INT PRIMARY KEY, qty INT)",
        "INSERT INTO items VALUES (1, 10)",
    ]);
    // A key family no keyspace entry describes (a newer primary, say),
    // written below the primary's engine.
    pair.raw.put(b"zzz:probe", b"written by no engine family").unwrap();
    pair.run(&["INSERT INTO items VALUES (2, 20)"]);
    let reference = pair.settle();
    let stats = pair.applier.stats();
    assert_eq!(stats.unknown_keys, 1, "{stats:?}");
    assert!(stats.derived_state_suspect, "{stats:?}");
    assert_eq!(stats.full_rebuilds, 0, "deferred to the idle poll: {stats:?}");
    // The rows around it were still published.
    assert_same(&pair, &reference, &["items"], true);

    assert!(pair.applier.run_pending_rebuilds().unwrap());
    let stats = pair.applier.stats();
    assert_eq!(stats.full_rebuilds, 1, "{stats:?}");
    assert!(stats.derived_state_suspect, "suspect is sticky: {stats:?}");
    assert!(!pair.applier.run_pending_rebuilds().unwrap(), "nothing left to run");
    assert_same(&pair, &reference, &["items"], true);

    pair.run(&["INSERT INTO items VALUES (3, 30)"]);
    let reference = pair.settle();
    assert_same(&pair, &reference, &["items"], true);
}

#[test]
fn an_applier_refuses_a_writer_its_engine_was_not_opened_over() {
    // A table with a PRIMARY KEY, so a hydrated registry is not empty.
    let first = Pair::new(&["CREATE TABLE t (id INT PRIMARY KEY)"]);
    let second = Pair::new(&[]);
    assert!(PhysicalApplier::new(
        Arc::clone(&first.standby),
        Arc::clone(second.applier.writer()),
        Arc::new(NoopDerivedStateListener),
    )
    .is_err());
    // And a second hydration of an engine already hydrated is refused.
    assert!(first.applier.hydrate().is_err());
}

// ---------------------------------------------------------------------------
// Throughput
// ---------------------------------------------------------------------------

/// A 100k-row corpus through the applier; prints rows/s (no assertion on it).
#[test]
#[ignore = "throughput probe: cargo test --release --test physical_standby_derived_state -- --ignored --nocapture"]
fn applier_throughput_on_a_100k_row_corpus() {
    let mut pair = Pair::new(&[
        "CREATE TABLE bulk (id INT PRIMARY KEY, name TEXT, qty INT)",
        "CREATE INDEX bulk_qty_idx ON bulk (qty)",
    ]);
    for chunk in 0..100 {
        let values: Vec<String> = (1..=1000)
            .map(|offset| {
                let id = chunk * 1000 + offset;
                format!("({id}, 'row-{id}', {})", id % 97)
            })
            .collect();
        let insert = format!("INSERT INTO bulk VALUES {}", values.join(", "));
        pair.run(&[insert.as_str()]);
    }
    let (applied, spent) = pair.sync();
    let rows: usize = applied.iter().map(|a| a.rows).sum();
    println!(
        "physical applier: {rows} rows in {} batches, {:.3}s in apply, {:.0} rows/s",
        applied.len(),
        spent.as_secs_f64(),
        rows as f64 / spent.as_secs_f64().max(f64::EPSILON)
    );
    let reference = pair.settle();
    assert_same(&pair, &reference, &["bulk"], true);
    assert_no_fallback(&pair);
}
