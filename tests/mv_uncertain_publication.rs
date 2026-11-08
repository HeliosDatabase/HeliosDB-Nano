//! Actual-state MV publication after an uncertain native write result.
#![cfg(feature = "internal-tests")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::storage::ddl_batch::{
    debug_fail_next_applied_commit, debug_fail_next_applied_commit_with, debug_fail_next_commit,
};
use heliosdb_nano::{Column, Config, DataType, EmbeddedDatabase, Schema, Tuple, Value};
use std::path::Path;

fn open(path: &Path, history: bool, strict: bool) -> EmbeddedDatabase {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    config.storage.wal_enabled = true;
    config.storage.logical_wal_per_statement = strict;
    config.storage.time_travel_enabled = history;
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    EmbeddedDatabase::with_config(config).unwrap()
}

fn execute(db: &EmbeddedDatabase, params: bool, sql: &str) -> heliosdb_nano::Result<()> {
    if params {
        db.execute_params(sql, &[]).map(|_| ())
    } else {
        db.execute(sql).map(|_| ())
    }
}

fn rows(db: &EmbeddedDatabase, table: &str) -> Vec<Vec<Value>> {
    let mut result: Vec<_> = db
        .query(&format!("SELECT * FROM {table}"), &[])
        .unwrap()
        .into_iter()
        .map(|row| row.values)
        .collect();
    result.sort_by_key(|row| format!("{row:?}"));
    result
}

fn seed(db: &EmbeddedDatabase, params: bool) {
    execute(
        db,
        params,
        "CREATE TABLE src (id INT PRIMARY KEY, tag TEXT, body TEXT, n INT)",
    )
    .unwrap();
    execute(
        db,
        params,
        "INSERT INTO src VALUES (1, 'old', 'body', 10), (2, 'old', 'more', 20)",
    )
    .unwrap();
}

fn assert_hot(db: &EmbeddedDatabase, sql: &str) {
    let (tx, rx) = std::sync::mpsc::channel();
    db.debug_next_hot_cache_lookup(move |hit| tx.send(hit).unwrap());
    db.query(sql, &[]).unwrap();
    assert!(
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
        "the real result-cache probe must hit"
    );
}

fn uncertain(error: impl std::fmt::Display) {
    let error = error.to_string();
    assert!(
        error.contains("injected schema durability failure after the group applied"),
        "{error}"
    );
}

#[test]
fn applied_create_error_keeps_actual_rows_metadata_and_snapshot() {
    for history in [false, true] {
        for params in [false, true] {
            let path = tempfile::tempdir().unwrap();
            let db = open(path.path(), history, params);
            seed(&db, params);
            let before = db.storage.db().latest_sequence_number();
            debug_fail_next_applied_commit();
            uncertain(execute(&db, params, "CREATE MATERIALIZED VIEW mv AS SELECT * FROM src").unwrap_err());
            assert!(
                db.storage.db().latest_sequence_number() > before,
                "the real group landed"
            );
            assert!(db.storage.mv_catalog().view_exists("mv").unwrap());
            assert_eq!(rows(&db, "mv"), rows(&db, "src"));
            assert_eq!(db.storage.mv_catalog().get_view("mv").unwrap().row_count, Some(2));
            assert_eq!(db.query("SELECT tag FROM __mv_mv WHERE id = 1", &[]).unwrap().len(), 1);

            if history {
                let snapshot = db.storage.snapshot_manager().max_snapshot_timestamp().unwrap();
                assert_eq!(db.storage.scan_table_at_snapshot("__mv_mv", snapshot).unwrap().len(), 2);
            }
            let expected = rows(&db, "mv");
            drop(db);
            let reopened = open(path.path(), history, params);
            assert_eq!(rows(&reopened, "mv"), expected);
        }
    }
}

#[test]
fn applied_compatible_refresh_error_rebuilds_manual_keys_and_current_side_storage() {
    for history in [false, true] {
        for params in [false, true] {
            let path = tempfile::tempdir().unwrap();
            let db = open(path.path(), history, params);
            seed(&db, params);
            execute(&db, params, "CREATE MATERIALIZED VIEW mv AS SELECT * FROM src").unwrap();
            // A SELECT projection need not inherit source constraints. Install
            // a supported explicit key and prove it exists before the failure.
            db.execute("CREATE UNIQUE INDEX mv_id_key ON __mv_mv (id)").unwrap();
            assert!(db
                .execute("INSERT INTO __mv_mv VALUES (1, 'duplicate', '', 0)")
                .is_err());
            db.execute("CREATE INDEX mv_tag_idx ON __mv_mv (tag)").unwrap();
            db.execute("ALTER TABLE __mv_mv ALTER COLUMN tag SET STORAGE DICTIONARY")
                .unwrap();
            db.execute("ALTER TABLE __mv_mv ALTER COLUMN body SET STORAGE CONTENT_ADDRESSED")
                .unwrap();
            db.execute("ALTER TABLE __mv_mv ALTER COLUMN n SET STORAGE COLUMNAR")
                .unwrap();
            let old = rows(&db, "mv");
            let old_snapshot = db.storage.snapshot_manager().max_snapshot_timestamp();
            let names = db.storage.art_indexes().list_table_indexes("__mv_mv");
            let warm = "SELECT COUNT(*) FROM __mv_mv";
            for _ in 0..10 {
                assert_eq!(db.query(warm, &[]).unwrap().len(), 1);
            }
            assert_hot(&db, warm);
            let cached_count = db.query(warm, &[]).unwrap()[0].values[0].clone();
            execute(
                &db,
                params,
                "UPDATE src SET tag = 'fresh', body = 'rewritten', n = n + 100",
            )
            .unwrap();
            execute(&db, params, "INSERT INTO src VALUES (3, 'fresh', 'third', 130)").unwrap();
            for _ in 0..10 {
                db.query(warm, &[]).unwrap();
            }
            assert_hot(&db, warm);
            debug_fail_next_applied_commit();
            uncertain(execute(&db, params, "REFRESH MATERIALIZED VIEW CONCURRENTLY mv").unwrap_err());
            assert_eq!(rows(&db, "mv"), rows(&db, "src"));
            assert_eq!(db.storage.art_indexes().list_table_indexes("__mv_mv"), names);
            assert!(db.storage.art_indexes().index_exists("mv_tag_idx"));
            assert!(db
                .query("SELECT id FROM __mv_mv WHERE tag = 'old'", &[])
                .unwrap()
                .is_empty());
            assert_eq!(
                db.query("SELECT id FROM __mv_mv WHERE tag = 'fresh'", &[])
                    .unwrap()
                    .len(),
                3
            );
            assert_ne!(db.query(warm, &[]).unwrap()[0].values[0], cached_count);
            assert_eq!(db.query("SELECT tag FROM __mv_mv WHERE id = 1", &[]).unwrap().len(), 1);
            assert!(db
                .execute("INSERT INTO __mv_mv VALUES (1, 'duplicate', '', 0)")
                .is_err());
            if history {
                let mut historical: Vec<_> = db
                    .storage
                    .scan_table_at_snapshot("__mv_mv", old_snapshot.unwrap())
                    .unwrap()
                    .into_iter()
                    .map(|row| row.values)
                    .collect();
                historical.sort_by_key(|row| format!("{row:?}"));
                assert_eq!(historical, old);
            }
            let expected = rows(&db, "mv");
            drop(db);
            let reopened = open(path.path(), history, params);
            assert_eq!(rows(&reopened, "mv"), expected);
            assert!(reopened.storage.art_indexes().index_exists("mv_tag_idx"));
            assert_eq!(
                reopened
                    .query("SELECT id FROM __mv_mv WHERE tag = 'fresh'", &[])
                    .unwrap()
                    .len(),
                3
            );
        }
    }
}

#[test]
fn shape_replacement_reconciles_the_actual_schema_for_both_write_outcomes() {
    for applied in [false, true] {
        for history in [false, true] {
            let path = tempfile::tempdir().unwrap();
            let db = open(path.path(), history, false);
            seed(&db, false);
            db.execute("CREATE MATERIALIZED VIEW mv AS SELECT * FROM src").unwrap();
            db.execute("CREATE INDEX mv_tag_idx ON __mv_mv (tag)").unwrap();
            db.execute("ALTER TABLE __mv_mv ALTER COLUMN n SET STORAGE COLUMNAR")
                .unwrap();
            let original = rows(&db, "mv");
            let mut id = Column::new("replacement", DataType::Int4);
            id.primary_key = true;
            let mut value = Column::new("new_value", DataType::Int4);
            value.storage_mode = heliosdb_nano::ColumnStorageMode::Columnar;
            let shape = Schema::new(vec![id, value]);
            if applied {
                debug_fail_next_applied_commit();
            } else {
                debug_fail_next_commit();
            }
            let error = db
                .storage
                .mv_catalog()
                .store_view_data("mv", vec![Tuple::new(vec![Value::Int4(9), Value::Int4(900)])], &shape)
                .unwrap_err();
            if applied {
                uncertain(error);
            }
            let expected = if applied {
                assert!(!db.storage.art_indexes().index_exists("mv_tag_idx"));
                assert_eq!(
                    db.storage.catalog().get_table_schema("__mv_mv").unwrap().columns[0].name,
                    "replacement"
                );
                assert_eq!(
                    db.storage.mv_catalog().get_view("mv").unwrap().schema.columns[0].name,
                    "replacement"
                );
                assert_eq!(
                    db.query("SELECT new_value FROM __mv_mv WHERE replacement = 9", &[])
                        .unwrap()[0]
                        .values,
                    vec![Value::Int4(900)]
                );
                assert!(db.query("SELECT tag FROM __mv_mv", &[]).is_err());
                assert!(db.execute("INSERT INTO __mv_mv VALUES (9, 901)").is_err());
                vec![vec![Value::Int4(9), Value::Int4(900)]]
            } else {
                assert!(db.storage.art_indexes().index_exists("mv_tag_idx"));
                assert_eq!(
                    db.query("SELECT id FROM __mv_mv WHERE tag = 'old'", &[]).unwrap().len(),
                    2
                );
                original
            };
            assert_eq!(rows(&db, "mv"), expected);
            drop(db);
            let reopened = open(path.path(), history, false);
            assert_eq!(rows(&reopened, "mv"), expected);
        }
    }
}

#[test]
fn unreadable_applied_schema_retires_indexes_and_fences_cached_and_direct_reads_until_reopen() {
    let path = tempfile::tempdir().unwrap();
    let db = open(path.path(), true, false);
    seed(&db, false);
    db.execute("CREATE MATERIALIZED VIEW mv AS SELECT * FROM src").unwrap();
    db.execute("CREATE INDEX mv_tag_idx ON __mv_mv (tag)").unwrap();
    let schema = db.storage.catalog().get_table_schema("__mv_mv").unwrap();
    let warm = "SELECT COUNT(*) FROM __mv_mv";
    for _ in 0..10 {
        assert_eq!(db.query(warm, &[]).unwrap().len(), 1);
    }
    assert_hot(&db, warm);
    db.execute("UPDATE src SET tag = 'new'").unwrap();
    for _ in 0..10 {
        db.query(warm, &[]).unwrap();
    }
    assert_hot(&db, warm);
    let raw = db.storage.db();
    let stored = std::sync::Arc::new(std::sync::Mutex::new(None));
    let hook_saved = stored.clone();
    let hook_raw = raw.clone();
    debug_fail_next_applied_commit_with(move || {
        *hook_saved.lock().unwrap() = hook_raw.get(b"meta:table:__mv_mv").unwrap();
        hook_raw.put(b"meta:table:__mv_mv", [0xff]).unwrap();
    });
    let error = db.execute("REFRESH MATERIALIZED VIEW mv").unwrap_err();
    assert!(error.to_string().contains("reconciliation failed"), "{error}");
    assert!(db.storage.art_indexes().list_table_indexes("__mv_mv").is_empty());
    for sql in [warm, "SELECT id, tag FROM __mv_mv WHERE id = 1", "SELECT * FROM mv"] {
        assert!(db.query(sql, &[]).is_err(), "quarantined read returned rows: {sql}");
    }
    assert!(db.storage.scan_table_with_schema("__mv_mv", &schema).is_err());
    assert!(db.storage.get_row_by_id("__mv_mv", 1, &schema).is_err());
    assert!(db
        .storage
        .get_row_by_pk_with_schema("__mv_mv", &Value::Int4(1), &schema)
        .is_err());
    assert!(db.storage.count_table_rows("__mv_mv").is_err());
    assert!(db.execute("INSERT INTO __mv_mv VALUES (3, 'blocked', '', 0)").is_err());
    assert_eq!(rows(&db, "src").len(), 2, "unrelated tables remain usable");
    raw.put(b"meta:table:__mv_mv", stored.lock().unwrap().take().unwrap())
        .unwrap();
    assert!(
        db.query(warm, &[]).is_err(),
        "repair requires a fresh rebuild, never an old cached plan"
    );
    drop(raw);
    drop(db);
    let reopened = open(path.path(), true, false);
    assert_eq!(
        reopened.query("SELECT tag FROM __mv_mv WHERE id = 1", &[]).unwrap()[0].values[0],
        Value::String("new".into())
    );
    assert!(reopened.storage.art_indexes().index_exists("mv_tag_idx"));
}

#[cfg(feature = "vector-search")]
#[test]
fn uncertain_vector_refresh_retires_only_memory_and_refuses_the_table_until_reopen() {
    let path = tempfile::tempdir().unwrap();
    let db = open(path.path(), true, false);
    db.execute("CREATE TABLE src (id INT PRIMARY KEY, embedding VECTOR(2))")
        .unwrap();
    db.execute("INSERT INTO src VALUES (1, CAST('[1.0, 0.0]' AS VECTOR(2)))")
        .unwrap();
    db.execute("CREATE MATERIALIZED VIEW mv AS SELECT * FROM src").unwrap();
    db.execute("CREATE INDEX mv_vec_idx ON __mv_mv USING hnsw (embedding)")
        .unwrap();
    assert!(db.storage.vector_indexes().index_exists("mv_vec_idx"));
    let after_write = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let hook_sequence = after_write.clone();
    let raw = db.storage.db();
    let hook_raw = raw.clone();
    debug_fail_next_applied_commit_with(move || {
        hook_sequence.store(hook_raw.latest_sequence_number(), std::sync::atomic::Ordering::Release);
    });
    let error = db.execute("REFRESH MATERIALIZED VIEW mv").unwrap_err();
    assert!(error.to_string().contains("reconciliation failed"), "{error}");
    assert!(!db.storage.vector_indexes().index_exists("mv_vec_idx"));
    assert!(db
        .storage
        .vector_indexes()
        .degraded_indexes()
        .iter()
        .any(|(name, _)| name == "mv_vec_idx"));
    assert!(
        after_write.load(std::sync::atomic::Ordering::Acquire) > 0,
        "actual boundary hook ran"
    );
    assert_eq!(
        raw.latest_sequence_number(),
        after_write.load(std::sync::atomic::Ordering::Acquire),
        "retirement issues no derived or persistent-vector writes"
    );
    assert!(
        raw.get(b"meta:index:mv_vec_idx").unwrap().is_some(),
        "definition survives for rebuild"
    );
    assert!(db.query("SELECT * FROM mv", &[]).is_err());
    drop(raw);
    drop(db);
    let reopened = open(path.path(), true, false);
    assert_eq!(rows(&reopened, "mv").len(), 1);
    assert!(reopened.storage.vector_indexes().index_exists("mv_vec_idx"));
}

#[cfg(feature = "vector-search")]
#[test]
fn refused_vector_populations_keep_the_old_table_readable_and_writable_without_reopen() {
    let path = tempfile::tempdir().unwrap();
    let db = open(path.path(), true, false);
    db.execute("CREATE TABLE src (id INT PRIMARY KEY, embedding VECTOR(2))")
        .unwrap();
    db.execute("INSERT INTO src VALUES (1, CAST('[1.0, 0.0]' AS VECTOR(2)))")
        .unwrap();
    db.execute("CREATE MATERIALIZED VIEW mv AS SELECT * FROM src").unwrap();
    db.execute("CREATE INDEX mv_vec_idx ON __mv_mv USING hnsw (embedding)")
        .unwrap();
    let original = rows(&db, "mv");
    db.execute("INSERT INTO src VALUES (2, CAST('[0.0, 1.0]' AS VECTOR(2)))")
        .unwrap();
    for cap in [false, true] {
        let before = db.storage.db().latest_sequence_number();
        if cap {
            db.storage.debug_set_write_group_ceiling(Some(64));
        } else {
            debug_fail_next_commit();
        }
        let error = db.execute("REFRESH MATERIALIZED VIEW mv").unwrap_err();
        db.storage.debug_set_write_group_ceiling(None);
        if cap {
            assert!(
                error
                    .to_string()
                    .contains(heliosdb_nano::BRANCH_MATERIALIZE_TOO_LARGE_MARKER),
                "{error}"
            );
        }
        assert_eq!(
            db.storage.db().latest_sequence_number(),
            before,
            "refusal writes no group"
        );
        assert_eq!(rows(&db, "mv"), original);
        assert!(db.storage.vector_indexes().index_exists("mv_vec_idx"));
        assert_eq!(db.query("SELECT id FROM __mv_mv WHERE id = 1", &[]).unwrap().len(), 1);
        // Prove this is a usable live table; a blanket read/write quarantine
        // would fail here even though the failed population changed no data.
        db.execute("INSERT INTO __mv_mv VALUES (99, CAST('[0.5, 0.5]' AS VECTOR(2)))")
            .unwrap();
        db.execute("DELETE FROM __mv_mv WHERE id = 99").unwrap();
    }
}

#[test]
fn applied_public_population_creation_rebuilds_explicit_schema_owned_primary_keys() {
    use heliosdb_nano::storage::ArtIndexManager;
    for history in [false, true] {
        for strict in [false, true] {
            let path = tempfile::tempdir().unwrap();
            let db = open(path.path(), history, strict);
            let mut id = Column::new("id", DataType::Int4);
            id.primary_key = true;
            id.nullable = false;
            let schema = Schema::new(vec![id, Column::new("body", DataType::Text)]);
            debug_fail_next_applied_commit();
            uncertain(
                db.storage
                    .mv_catalog()
                    .store_view_data(
                        "owned",
                        vec![
                            Tuple::new(vec![Value::Int4(7), Value::String("seven".into())]),
                            Tuple::new(vec![Value::Int4(8), Value::String("eight".into())]),
                        ],
                        &schema,
                    )
                    .unwrap_err(),
            );
            assert!(db.storage.catalog().get_table_schema("__mv_owned").unwrap().columns[0].primary_key);
            for key in [7, 8] {
                assert_eq!(
                    db.storage
                        .art_indexes()
                        .pk_index_contains("__mv_owned", &ArtIndexManager::encode_key(&[Value::Int4(key)]),),
                    Some(true),
                    "the actual schema-owned PK was rebuilt"
                );
            }
            let expected = rows(&db, "__mv_owned");
            assert_eq!(expected.len(), 2);
            assert!(db.execute("INSERT INTO __mv_owned VALUES (7, 'duplicate')").is_err());
            drop(db);
            let reopened = open(path.path(), history, strict);
            assert_eq!(rows(&reopened, "__mv_owned"), expected);
            assert!(reopened
                .execute("INSERT INTO __mv_owned VALUES (8, 'duplicate')")
                .is_err());
        }
    }
}
