//! Failed pre-definition graph cleanup must not leave an incomplete ANN registered.
#![cfg(all(feature = "internal-tests", feature = "vector-persist"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::sql::executor::ddl::debug_next_before_vector_backfill;
use heliosdb_nano::{Config, EmbeddedDatabase, Error, Value};
use rocksdb::{IteratorMode, ReadOptions, WriteBatchIterator, DB};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

type Image = BTreeMap<Vec<u8>, Vec<u8>>;

fn image(raw: &DB) -> Image {
    let mut options = ReadOptions::default();
    options.set_total_order_seek(true);
    raw.iterator_opt(IteratorMode::Start, options)
        .map(|entry| {
            let (key, value) = entry.unwrap();
            (key.to_vec(), value.to_vec())
        })
        .collect()
}

fn open(path: &Path, history: bool) -> EmbeddedDatabase {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    config.storage.time_travel_enabled = history;
    config.storage.wal_enabled = true;
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    config.storage.version_gc_interval_secs = Some(0);
    config.replication.physical.enabled = true;
    let db = EmbeddedDatabase::with_config(config).unwrap();
    db.storage.set_index_snapshots_on_close(false);
    db.storage.set_row_counter_flush_on_close(false);
    db
}

fn execute(db: &EmbeddedDatabase, params: bool, sql: &str) -> heliosdb_nano::Result<()> {
    if params {
        db.execute_params(sql, &[]).map(|_| ())
    } else {
        db.execute(sql).map(|_| ())
    }
}

fn seed(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE items (id INT PRIMARY KEY, old_vec VECTOR(3), new_vec VECTOR(3))")
        .unwrap();
    let rows = (1..=512)
        .map(|id| format!("({id}, '[{id},0,0]', '[{id},0,0]')"))
        .collect::<Vec<_>>()
        .join(",");
    db.execute(&format!("INSERT INTO items VALUES {rows}")).unwrap();
    db.execute("CREATE INDEX keep_standard ON items USING hnsw (old_vec)")
        .unwrap();
    db.execute("CREATE INDEX keep_durable ON items USING hnsw (old_vec) WITH (persistent=true)")
        .unwrap();
    db.execute("CREATE TABLE healthy (id INT PRIMARY KEY, embedding VECTOR(3))")
        .unwrap();
    db.execute("INSERT INTO healthy VALUES (7, '[7,0,0]')").unwrap();
    db.execute("CREATE INDEX healthy_graph ON healthy USING hnsw (embedding)")
        .unwrap();
}

#[derive(Clone, Copy, Debug)]
enum Failure {
    Cap,
    Native,
}

struct Cut {
    image: Image,
    sequence: u64,
    graph_keys: BTreeSet<Vec<u8>>,
    refusals: u64,
    target_row: u64,
}

fn arm(db: &EmbeddedDatabase, failure: Failure, corrupt_proof: bool) -> Arc<Mutex<Option<Cut>>> {
    let before_keys: BTreeSet<_> = image(&db.storage.db()).into_keys().collect();
    let raw = db.storage.db();
    let gate = db.storage.write_gate().clone();
    let saved = Arc::new(Mutex::new(None));
    let capture = saved.clone();
    debug_next_before_vector_backfill(move |manager, name, vectors| {
        assert_eq!(name, "broken_new");
        assert_eq!(vectors.len(), 512);
        let partial: Vec<_> = vectors
            .iter()
            .filter(|(_, vector)| vector[0] <= 300.0)
            .cloned()
            .collect();
        assert_eq!(partial.len(), 300);
        manager.insert_vectors_batch(name, &partial).unwrap();
        let target_row = vectors.iter().find(|(_, vector)| vector[0] == 512.0).unwrap().0;
        let wrong = manager.search(name, &vec![512.0, 0.0, 0.0], 1).unwrap();
        assert!(!wrong.is_empty());
        assert!(
            wrong.iter().all(|(row, _)| *row != target_row),
            "real partial graph omits the closest heap row"
        );
        if corrupt_proof {
            raw.put(b"meta:index:keep_durable", b"unreadable actual-definition proof")
                .unwrap();
        }
        let bytes = image(&raw);
        let graph_keys = bytes
            .keys()
            .filter(|key| key.starts_with(b"__vidx:") && !before_keys.contains(*key))
            .cloned()
            .collect();
        *capture.lock().unwrap() = Some(Cut {
            image: bytes,
            sequence: raw.latest_sequence_number(),
            graph_keys,
            refusals: gate.refusals(),
            target_row,
        });
        match failure {
            Failure::Cap => gate.debug_set_write_group_ceiling(Some(0)),
            Failure::Native => {
                gate.debug_fail_next_native_write_before_apply();
                gate.debug_fail_next_native_write_after_apply();
            }
        }
    });
    saved
}

#[derive(Default)]
struct Deletes(Vec<Vec<u8>>);

impl WriteBatchIterator for Deletes {
    fn put(&mut self, key: Box<[u8]>, _: Box<[u8]>) {
        panic!("cleanup must delete only its unreferenced graph: {key:?}");
    }

    fn delete(&mut self, key: Box<[u8]>) {
        self.0.push(key.into_vec());
    }
}

fn no_extra_repair(db: &EmbeddedDatabase, cut: &Cut, failure: Failure) {
    assert!(!cut.graph_keys.is_empty());
    let after = image(&db.storage.db());
    match failure {
        Failure::Cap => {
            assert_eq!(
                after, cut.image,
                "backfill/cleanup refusals and RAM settlement write no repair"
            );
            assert_eq!(db.storage.db().latest_sequence_number(), cut.sequence);
            assert_eq!(
                db.storage.write_gate().refusals() - cut.refusals,
                2,
                "one failed backfill and one failed cleanup"
            );
        }
        Failure::Native => {
            let expected: Image = cut
                .image
                .iter()
                .filter(|(key, _)| !cut.graph_keys.contains(*key))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            assert_eq!(
                after, expected,
                "only the one possibly-applied native cleanup may change bytes"
            );
            assert_eq!(
                db.storage.db().latest_sequence_number(),
                cut.sequence + cut.graph_keys.len() as u64
            );
            let mut groups = 0;
            for record in db.storage.db().get_updates_since(cut.sequence).unwrap() {
                let (first, batch) = record.unwrap();
                assert_eq!(first, cut.sequence + 1);
                let mut deleted = Deletes::default();
                batch.iterate(&mut deleted);
                assert_eq!(deleted.0.into_iter().collect::<BTreeSet<_>>(), cut.graph_keys);
                groups += 1;
            }
            assert_eq!(groups, 1, "no retry/repair native group after cleanup failed");
        }
    }
}

fn healthy(db: &EmbeddedDatabase, target_row: u64) {
    assert!(db.storage.vector_indexes().index_exists("keep_standard"));
    assert!(db.storage.vector_indexes().index_exists("keep_durable"));
    assert!(db.storage.vector_indexes().index_exists("healthy_graph"));
    for name in ["keep_standard", "keep_durable"] {
        assert_eq!(
            db.storage
                .vector_indexes()
                .search(name, &vec![512.0, 0.0, 0.0], 1)
                .unwrap()[0]
                .0,
            target_row
        );
    }
    assert_eq!(
        db.query("SELECT id FROM items ORDER BY new_vec <-> '[512,0,0]' LIMIT 1", &[])
            .unwrap()[0]
            .values,
        vec![Value::Int4(512)]
    );
    assert_eq!(
        db.query("SELECT COUNT(*) FROM items", &[]).unwrap()[0].values,
        vec![Value::Int8(512)]
    );
    assert_eq!(
        db.query("SELECT id FROM healthy ORDER BY embedding <-> '[7,0,0]' LIMIT 1", &[])
            .unwrap()[0]
            .values,
        vec![Value::Int4(7)]
    );
}

fn run(failure: Failure) {
    let mut cells = 0;
    for history in [false, true] {
        for params in [false, true] {
            let path = tempfile::tempdir().unwrap();
            let db = open(path.path(), history);
            seed(&db);
            let before = image(&db.storage.db());
            let old_graph: Image = before
                .iter()
                .filter(|(key, _)| key.starts_with(b"__vidx:"))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            let captured = arm(&db, failure, false);
            let error = execute(
                &db,
                params,
                "CREATE INDEX broken_new ON items USING hnsw (new_vec) WITH (persistent=true)",
            )
            .unwrap_err()
            .to_string();
            db.storage.debug_set_write_group_ceiling(None);
            assert!(
                error.contains("Failed to backfill HNSW index"),
                "original backfill error: {error}"
            );
            assert!(
                match failure {
                    Failure::Cap => error.contains("write group"),
                    Failure::Native => error.contains("injected native write error before apply"),
                },
                "{error}"
            );
            let cut = captured
                .lock()
                .unwrap()
                .take()
                .expect("real registered/preflight/backfill boundary reached");
            no_extra_repair(&db, &cut, failure);
            assert!(db.storage.get(&b"meta:index:broken_new".to_vec()).unwrap().is_none());
            assert!(
                !db.storage.vector_indexes().index_exists("broken_new"),
                "an unreferenced partial graph must never serve ANN"
            );
            let settled = image(&db.storage.db());
            for (key, value) in &old_graph {
                assert_eq!(settled.get(key), Some(value));
            }
            let sequence = db.storage.db().latest_sequence_number();
            healthy(&db, cut.target_row);
            assert_eq!(image(&db.storage.db()), settled);
            assert_eq!(db.storage.db().latest_sequence_number(), sequence);
            drop(db);
            let reopened = open(path.path(), history);
            healthy(&reopened, cut.target_row);
            assert!(!reopened.storage.vector_indexes().index_exists("broken_new"));
            execute(
                &reopened,
                params,
                "CREATE INDEX broken_new ON items USING hnsw (new_vec) WITH (persistent=true)",
            )
            .unwrap();
            assert_eq!(
                reopened
                    .storage
                    .vector_indexes()
                    .search("broken_new", &vec![512.0, 0.0, 0.0], 1)
                    .unwrap()[0]
                    .0,
                cut.target_row
            );
            healthy(&reopened, cut.target_row);
            let after_retry = image(&reopened.storage.db());
            for (key, value) in &before {
                if key.starts_with(b"data:") || old_graph.contains_key(key) {
                    assert_eq!(
                        after_retry.get(key),
                        Some(value),
                        "retry preserves heap and every old durable graph"
                    );
                }
            }
            drop(reopened);
            let again = open(path.path(), history);
            assert!(again.storage.vector_indexes().index_exists("broken_new"));
            healthy(&again, cut.target_row);
            cells += 1;
        }
    }
    assert_eq!(cells, 4);
    eprintln!("vector-create-cleanup {failure:?}=4");
}

#[test]
fn failed_cap_cleanup_retires_partial_graph_and_explicit_retry_reclaims_its_name() {
    run(Failure::Cap);
}

#[test]
fn failed_native_cleanup_preserves_original_error_and_healthy_graphs() {
    run(Failure::Native);
}

#[test]
fn unreadable_cleanup_proof_fences_only_the_affected_table_without_repair_writes() {
    let mut cells = 0;
    for history in [false, true] {
        for params in [false, true] {
            let path = tempfile::tempdir().unwrap();
            let db = open(path.path(), history);
            seed(&db);
            let captured = arm(&db, Failure::Cap, true);
            let error = execute(
                &db,
                params,
                "CREATE INDEX broken_new ON items USING hnsw (new_vec) WITH (persistent=true)",
            )
            .unwrap_err()
            .to_string();
            db.storage.debug_set_write_group_ceiling(None);
            assert!(
                error.contains("Failed to backfill HNSW index") && error.contains("write group"),
                "{error}"
            );
            let cut = captured.lock().unwrap().take().unwrap();
            no_extra_repair(&db, &cut, Failure::Cap);
            let before = image(&db.storage.db());
            let sequence = db.storage.db().latest_sequence_number();
            for sql in [
                "SELECT id FROM items",
                "SELECT id FROM items ORDER BY new_vec <-> '[512,0,0]' LIMIT 1",
            ] {
                let error = db.query(sql, &[]).unwrap_err();
                assert!(
                    matches!(&error, Error::Storage(message) if message.contains("unavailable")),
                    "{error}"
                );
            }
            let error = execute(&db, params, "INSERT INTO items VALUES (999, '[999,0,0]', '[999,0,0]')").unwrap_err();
            assert!(
                matches!(&error, Error::Storage(message) if message.contains("unavailable")),
                "{error}"
            );
            assert_eq!(image(&db.storage.db()), before);
            assert_eq!(db.storage.db().latest_sequence_number(), sequence);
            assert!(db.storage.vector_indexes().index_exists("healthy_graph"));
            assert_eq!(
                db.query("SELECT id FROM healthy", &[]).unwrap()[0].values,
                vec![Value::Int4(7)]
            );
            execute(&db, params, "INSERT INTO healthy VALUES (8, '[8,0,0]')").unwrap();
            cells += 1;
        }
    }
    assert_eq!(cells, 4);
    eprintln!("vector-create-cleanup unreadable_proof=4");
}
