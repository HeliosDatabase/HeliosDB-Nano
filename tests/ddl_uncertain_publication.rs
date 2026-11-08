//! Final-owner DDL write outcomes must govern eager ART/vector settlement.
#![cfg(feature = "internal-tests")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::storage::ddl_batch::{
    debug_fail_next_applied_commit, debug_fail_next_applied_commit_with, debug_fail_next_commit,
    debug_next_before_native_commit,
};
use heliosdb_nano::{Config, EmbeddedDatabase, Error, Value};
use rocksdb::{IteratorMode, ReadOptions, WriteBatchIterator};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Copy, Debug)]
enum Fault {
    AppliedDurability,
    UnknownApplied,
    UnknownUnapplied,
}

impl Fault {
    fn arm(self, db: &EmbeddedDatabase) {
        match self {
            Self::AppliedDurability => debug_fail_next_applied_commit(),
            Self::UnknownApplied => {
                let gate = db.storage.write_gate().clone();
                debug_next_before_native_commit(move || gate.debug_fail_next_native_write_after_apply());
            }
            Self::UnknownUnapplied => {
                let gate = db.storage.write_gate().clone();
                debug_next_before_native_commit(move || gate.debug_fail_next_native_write_before_apply());
            }
        }
    }

    fn assert_error(self, error: Error) {
        let text = error.to_string();
        let marker = match self {
            Self::AppliedDurability => "injected schema durability failure after the group applied",
            Self::UnknownApplied => "injected native write error after apply",
            Self::UnknownUnapplied => "injected native write error before apply",
        };
        assert!(text.contains(marker), "{self:?}: {text}");
    }

    fn applied(self) -> bool {
        !matches!(self, Self::UnknownUnapplied)
    }
}

fn open(path: &Path, history: bool, per_statement: bool) -> EmbeddedDatabase {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    config.storage.wal_enabled = true;
    config.storage.time_travel_enabled = history;
    config.storage.logical_wal_per_statement = per_statement;
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
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

fn image(db: &EmbeddedDatabase) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut options = ReadOptions::default();
    options.set_total_order_seek(true);
    db.storage
        .db()
        .iterator_opt(IteratorMode::Start, options)
        .map(|entry| {
            let (key, value) = entry.unwrap();
            (key.to_vec(), value.to_vec())
        })
        .collect()
}

#[derive(Default)]
struct Keys(Vec<Vec<u8>>);

impl WriteBatchIterator for Keys {
    fn put(&mut self, key: Box<[u8]>, _value: Box<[u8]>) {
        self.0.push(key.into_vec());
    }

    fn delete(&mut self, key: Box<[u8]>) {
        self.0.push(key.into_vec());
    }
}

fn groups(db: &EmbeddedDatabase, before: u64) -> Vec<Keys> {
    let raw = db.storage.db();
    let latest = raw.latest_sequence_number();
    let mut next = before + 1;
    let mut groups = Vec::new();
    for item in raw.get_updates_since(before).unwrap() {
        let (first, batch) = item.unwrap();
        if first > latest {
            break;
        }
        assert_eq!(first, next, "native WAL history must be complete");
        next += batch.len() as u64;
        let mut keys = Keys::default();
        batch.iterate(&mut keys);
        groups.push(keys);
    }
    assert_eq!(next, latest + 1);
    groups
}

fn assert_catalog_group_and_no_later_repairs(db: &EmbeddedDatabase, before: u64, catalog_key: &[u8]) {
    let groups = groups(db, before);
    let owners: Vec<_> = groups
        .iter()
        .enumerate()
        .filter(|(_, group)| group.0.iter().any(|key| key == catalog_key))
        .collect();
    assert_eq!(owners.len(), 1, "one final owning catalog group");
    let (position, owner) = owners[0];
    assert!(owner.0.iter().any(|key| key.starts_with(b"wal:entries:")));
    assert_eq!(position + 1, groups.len(), "no error-path repair write after the owner");
}

fn assert_duplicate(db: &EmbeddedDatabase, params: bool, sql: &str) {
    let error = execute(db, params, sql).unwrap_err().to_string();
    assert!(
        error.contains("duplicate") || error.contains("Duplicate") || error.contains("UNIQUE"),
        "the actual committed key must be enforced: {error}"
    );
}

#[test]
fn create_table_settles_pk_and_unique_from_the_actual_native_outcome() {
    let mut cells = 0;
    for history in [false, true] {
        for params in [false, true] {
            for fault in [Fault::AppliedDurability, Fault::UnknownApplied, Fault::UnknownUnapplied] {
                let path = tempfile::tempdir().unwrap();
                let db = open(path.path(), history, params);
                let before = image(&db);
                let sequence = db.storage.db().latest_sequence_number();
                fault.arm(&db);
                fault.assert_error(
                    execute(&db, params, "CREATE TABLE made (id INT PRIMARY KEY, code INT UNIQUE)").unwrap_err(),
                );
                if fault.applied() {
                    assert!(db.storage.get(&b"meta:table:made".to_vec()).unwrap().is_some());
                    assert_eq!(db.storage.art_indexes().list_table_indexes("made").len(), 2);
                    assert_catalog_group_and_no_later_repairs(&db, sequence, b"meta:table:made");
                    execute(&db, params, "INSERT INTO made VALUES (1, 10)").unwrap();
                    assert_duplicate(&db, params, "INSERT INTO made VALUES (1, 20)");
                    assert_duplicate(&db, params, "INSERT INTO made VALUES (2, 10)");
                    assert_eq!(
                        db.query("SELECT code FROM made WHERE id = 1", &[]).unwrap()[0].values,
                        vec![Value::Int4(10)]
                    );
                } else {
                    assert_eq!(image(&db), before);
                    assert_eq!(db.storage.db().latest_sequence_number(), sequence);
                    assert!(db.storage.art_indexes().list_table_indexes("made").is_empty());
                    execute(&db, params, "CREATE TABLE made (id INT PRIMARY KEY, code INT UNIQUE)").unwrap();
                    execute(&db, params, "INSERT INTO made VALUES (1, 10)").unwrap();
                    assert_duplicate(&db, params, "INSERT INTO made VALUES (2, 10)");
                }
                drop(db);
                let reopened = open(path.path(), history, params);
                assert_eq!(
                    reopened.query("SELECT COUNT(*) FROM made", &[]).unwrap()[0].values,
                    vec![Value::Int8(1)]
                );
                assert_duplicate(&reopened, params, "INSERT INTO made VALUES (2, 10)");
                cells += 1;
            }
        }
    }
    assert_eq!(cells, 12);
    eprintln!("ddl-outcome-receipt create_table=12");
}

#[test]
fn create_manual_unique_index_never_undoes_an_applied_definition() {
    let mut cells = 0;
    for history in [false, true] {
        for params in [false, true] {
            for fault in [Fault::AppliedDurability, Fault::UnknownApplied, Fault::UnknownUnapplied] {
                let path = tempfile::tempdir().unwrap();
                let db = open(path.path(), history, params);
                execute(&db, params, "CREATE TABLE items (id INT PRIMARY KEY, code INT)").unwrap();
                execute(&db, params, "INSERT INTO items VALUES (1, 10)").unwrap();
                let before = image(&db);
                let sequence = db.storage.db().latest_sequence_number();
                fault.arm(&db);
                fault.assert_error(execute(&db, params, "CREATE UNIQUE INDEX code_key ON items (code)").unwrap_err());
                if fault.applied() {
                    assert!(db.storage.art_indexes().index_exists("code_key"));
                    assert_catalog_group_and_no_later_repairs(&db, sequence, b"meta:index:code_key");
                    assert_duplicate(&db, params, "INSERT INTO items VALUES (2, 10)");
                } else {
                    assert_eq!(image(&db), before);
                    assert_eq!(db.storage.db().latest_sequence_number(), sequence);
                    assert!(!db.storage.art_indexes().index_exists("code_key"));
                    execute(&db, params, "CREATE UNIQUE INDEX code_key ON items (code)").unwrap();
                    assert_duplicate(&db, params, "INSERT INTO items VALUES (2, 10)");
                }
                assert_eq!(
                    db.query("SELECT id FROM items WHERE code = 10", &[]).unwrap()[0].values,
                    vec![Value::Int4(1)]
                );
                drop(db);
                let reopened = open(path.path(), history, params);
                assert!(reopened.storage.art_indexes().index_exists("code_key"));
                assert_duplicate(&reopened, params, "INSERT INTO items VALUES (2, 10)");
                cells += 1;
            }
        }
    }
    assert_eq!(cells, 12);
    eprintln!("ddl-outcome-receipt manual_unique=12");
}

#[test]
fn known_ddl_refusal_and_group_cap_leave_no_catalog_or_key_effects() {
    let mut cells = 0;
    for params in [false, true] {
        for cap in [false, true] {
            let path = tempfile::tempdir().unwrap();
            let db = open(path.path(), false, params);
            execute(&db, params, "CREATE TABLE old (id INT PRIMARY KEY, code INT)").unwrap();
            execute(&db, params, "INSERT INTO old VALUES (1, 10)").unwrap();
            for sql in [
                "CREATE TABLE refused (id INT PRIMARY KEY, code INT UNIQUE)",
                "CREATE UNIQUE INDEX refused_key ON old (code)",
            ] {
                let before = image(&db);
                let sequence = db.storage.db().latest_sequence_number();
                if cap {
                    db.storage.debug_set_write_group_ceiling(Some(0));
                } else {
                    debug_fail_next_commit();
                }
                let error = execute(&db, params, sql).unwrap_err().to_string();
                db.storage.debug_set_write_group_ceiling(None);
                assert!(
                    if cap {
                        error.contains("54000") || error.contains("write group")
                    } else {
                        error.contains("was not written")
                    },
                    "{error}"
                );
                assert_eq!(image(&db), before);
                assert_eq!(db.storage.db().latest_sequence_number(), sequence);
                assert!(db.storage.art_indexes().list_table_indexes("refused").is_empty());
                assert!(!db.storage.art_indexes().index_exists("refused_key"));
                assert_eq!(
                    db.query("SELECT code FROM old WHERE id = 1", &[]).unwrap()[0].values,
                    vec![Value::Int4(10)]
                );
                cells += 1;
            }
            execute(&db, params, "INSERT INTO old VALUES (2, 20)").unwrap();
        }
    }
    assert_eq!(cells, 8);
    eprintln!("ddl-outcome-receipt not_written=8");
}

#[test]
fn unresolved_create_definition_fences_only_its_table_without_repair_writes() {
    let mut cells = 0;
    for history in [false, true] {
        for params in [false, true] {
            let path = tempfile::tempdir().unwrap();
            let db = open(path.path(), history, params);
            execute(&db, params, "CREATE TABLE healthy (id INT PRIMARY KEY)").unwrap();
            execute(&db, params, "INSERT INTO healthy VALUES (7)").unwrap();
            execute(&db, params, "CREATE TABLE items (id INT PRIMARY KEY, code INT)").unwrap();
            execute(&db, params, "INSERT INTO items VALUES (1, 10)").unwrap();
            let raw = db.storage.db();
            debug_fail_next_applied_commit_with(move || {
                raw.put(
                    b"meta:index:code_key",
                    b"unreadable definition at the settlement boundary",
                )
                .unwrap();
            });
            Fault::AppliedDurability
                .assert_error(execute(&db, params, "CREATE UNIQUE INDEX code_key ON items (code)").unwrap_err());
            let before_reads = image(&db);
            let sequence = db.storage.db().latest_sequence_number();
            let error = db.query("SELECT code FROM items WHERE id = 1", &[]).unwrap_err();
            assert!(
                matches!(&error, Error::Storage(message) if message.contains("unavailable")),
                "{error}"
            );
            let error = execute(&db, params, "INSERT INTO items VALUES (2, 10)").unwrap_err();
            assert!(
                matches!(&error, Error::Storage(message) if message.contains("unavailable")),
                "{error}"
            );
            assert_eq!(
                image(&db),
                before_reads,
                "quarantine/retirement performs no durable repair"
            );
            assert_eq!(db.storage.db().latest_sequence_number(), sequence);
            assert_eq!(
                db.query("SELECT id FROM healthy", &[]).unwrap()[0].values,
                vec![Value::Int4(7)]
            );
            execute(&db, params, "INSERT INTO healthy VALUES (8)").unwrap();
            cells += 1;
        }
    }
    assert_eq!(cells, 4);
    eprintln!("ddl-outcome-receipt unresolved=4");
}

#[cfg(feature = "vector-persist")]
#[test]
fn persistent_create_keeps_its_graph_when_the_definition_group_applied() {
    let mut cells = 0;
    for params in [false, true] {
        for fault in [Fault::AppliedDurability, Fault::UnknownApplied, Fault::UnknownUnapplied] {
            let path = tempfile::tempdir().unwrap();
            let db = open(path.path(), false, params);
            execute(
                &db,
                params,
                "CREATE TABLE vectors (id INT PRIMARY KEY, embedding VECTOR(3))",
            )
            .unwrap();
            execute(&db, params, "INSERT INTO vectors VALUES (1, '[1,0,0]'), (2, '[0,1,0]')").unwrap();
            let before = image(&db);
            let sequence = db.storage.db().latest_sequence_number();
            fault.arm(&db);
            fault.assert_error(
                execute(
                    &db,
                    params,
                    "CREATE INDEX durable_vec ON vectors USING hnsw (embedding) WITH (persistent=true)",
                )
                .unwrap_err(),
            );
            if fault.applied() {
                let after = image(&db);
                assert!(
                    after.keys().any(|key| key.starts_with(b"__vidx:")),
                    "committed definition keeps its graph"
                );
                assert!(db.storage.vector_indexes().index_exists("durable_vec"));
                assert_catalog_group_and_no_later_repairs(&db, sequence, b"meta:index:durable_vec");
                assert_eq!(
                    db.query("SELECT id FROM vectors ORDER BY embedding <-> '[1,0,0]' LIMIT 1", &[])
                        .unwrap()[0]
                        .values,
                    vec![Value::Int4(1)]
                );
                assert_eq!(
                    image(&db),
                    after,
                    "error reconciliation and reads write no graph repair"
                );
            } else {
                assert_eq!(
                    image(&db),
                    before,
                    "unreachable pre-definition graph is cleaned after proven refusal"
                );
                assert!(!db.storage.vector_indexes().index_exists("durable_vec"));
                execute(
                    &db,
                    params,
                    "CREATE INDEX durable_vec ON vectors USING hnsw (embedding) WITH (persistent=true)",
                )
                .unwrap();
            }
            drop(db);
            let reopened = open(path.path(), false, params);
            assert!(reopened.storage.vector_indexes().index_exists("durable_vec"));
            assert_eq!(
                reopened
                    .query("SELECT id FROM vectors ORDER BY embedding <-> '[1,0,0]' LIMIT 1", &[])
                    .unwrap()[0]
                    .values,
                vec![Value::Int4(1)]
            );
            cells += 1;
        }
        let path = tempfile::tempdir().unwrap();
        let db = open(path.path(), false, params);
        execute(
            &db,
            params,
            "CREATE TABLE vectors (id INT PRIMARY KEY, embedding VECTOR(3))",
        )
        .unwrap();
        let before = image(&db);
        let sequence = db.storage.db().latest_sequence_number();
        db.storage.debug_set_write_group_ceiling(Some(0));
        let error = execute(
            &db,
            params,
            "CREATE INDEX capped_vec ON vectors USING hnsw (embedding) WITH (persistent=true)",
        )
        .unwrap_err()
        .to_string();
        db.storage.debug_set_write_group_ceiling(None);
        assert!(error.contains("54000") || error.contains("write group"), "{error}");
        assert_eq!(image(&db), before);
        assert_eq!(db.storage.db().latest_sequence_number(), sequence);
        assert!(!db.storage.vector_indexes().index_exists("capped_vec"));
        cells += 1;
    }
    assert_eq!(cells, 8);
    eprintln!("ddl-outcome-receipt persistent_create=8");
}
