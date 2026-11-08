//! Raw metadata staging holds the same table boundary as ordinary row writes.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::storage::{ColumnDictionary, Transaction};
use heliosdb_nano::{Config, EmbeddedDatabase, Tuple, Value};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

fn database() -> Arc<EmbeddedDatabase> {
    let db = Arc::new(EmbeddedDatabase::with_config(Config::in_memory()).unwrap());
    db.storage.debug_set_table_lock_timeout(Duration::from_secs(4));
    db.execute("CREATE TABLE guarded (id INT PRIMARY KEY, body TEXT)")
        .unwrap();
    db.execute("INSERT INTO guarded VALUES (1, 'kept')").unwrap();
    db
}
fn transaction(db: &EmbeddedDatabase, explicit: bool) -> Transaction {
    if explicit {
        db.storage.begin_transaction().unwrap()
    } else {
        db.storage.begin_autocommit_transaction().unwrap()
    }
}
fn dictionary() -> Vec<u8> {
    let mut dictionary = ColumnDictionary::new();
    dictionary.encode("kept");
    bincode::serialize(&dictionary).unwrap()
}
fn wait_for_exclusive_request(db: &EmbeddedDatabase, table: &str) {
    let end = Instant::now() + Duration::from_secs(2);
    while db.storage.debug_table_lock_waiters(table).0 == 0 {
        assert!(
            Instant::now() < end,
            "DDL never waited for the staged metadata on {table}"
        );
        std::thread::yield_now();
    }
}
fn run_ddl_after_staging(db: &Arc<EmbeddedDatabase>, txn: Transaction, table: &str, ddl: &str, rollback: bool) {
    let other = Arc::clone(db);
    let ddl = ddl.to_owned();
    let (done, completed) = mpsc::channel();
    let worker = std::thread::spawn(move || done.send(other.execute(&ddl)).unwrap());
    wait_for_exclusive_request(db, table);
    assert!(
        completed.try_recv().is_err(),
        "DDL cannot finish before the metadata transaction ends"
    );
    if rollback {
        txn.rollback().unwrap();
    } else {
        txn.commit_with_timestamp(db.storage.next_timestamp()).unwrap();
    }
    completed.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
    worker.join().unwrap();
    assert_eq!(db.storage.debug_table_lock_waiters(table), (0, 0));
}

#[test]
fn raw_dictionary_put_and_delete_hold_rename_drop_and_truncate_until_commit() {
    for explicit in [false, true] {
        for delete in [false, true] {
            for ddl in [
                "ALTER TABLE guarded RENAME TO moved",
                "DROP TABLE guarded",
                "TRUNCATE TABLE guarded",
            ] {
                let db = database();
                let key = b"dict:guarded:body".to_vec();
                let seed = db.storage.begin_autocommit_transaction().unwrap();
                seed.put(key.clone(), dictionary()).unwrap();
                seed.commit().unwrap();
                let txn = transaction(&db, explicit);
                if delete {
                    txn.delete(key.clone()).unwrap();
                } else {
                    txn.put(key.clone(), dictionary()).unwrap();
                }
                run_ddl_after_staging(&db, txn, "guarded", ddl, false);
                if ddl.starts_with("ALTER") || ddl.starts_with("DROP") {
                    assert!(
                        db.storage.get(&key).unwrap().is_none(),
                        "no late old-name dictionary may reappear"
                    );
                }
                if ddl.starts_with("TRUNCATE") {
                    assert_eq!(db.query("SELECT id FROM guarded", &[]).unwrap().len(), 0);
                }
            }
        }
    }
}

#[test]
fn raw_counter_put_delete_and_typed_stage_hold_ddl_until_commit() {
    for explicit in [false, true] {
        for operation in ["put", "delete", "typed"] {
            for ddl in [
                "ALTER TABLE guarded RENAME TO moved",
                "DROP TABLE guarded",
                "TRUNCATE TABLE guarded",
            ] {
                let db = database();
                let txn = transaction(&db, explicit);
                match operation {
                    "put" => txn
                        .put(b"counter:guarded".to_vec(), bincode::serialize(&42_u64).unwrap())
                        .unwrap(),
                    "delete" => txn.delete(b"counter:guarded".to_vec()).unwrap(),
                    "typed" => txn.stage_row_counter("guarded", 42).unwrap(),
                    _ => unreachable!(),
                }
                run_ddl_after_staging(&db, txn, "guarded", ddl, false);
                if ddl.starts_with("ALTER") || ddl.starts_with("DROP") {
                    assert!(db.storage.get(&b"counter:guarded".to_vec()).unwrap().is_none());
                }
                if ddl.starts_with("TRUNCATE") {
                    assert_eq!(db.query("SELECT id FROM guarded", &[]).unwrap().len(), 0);
                }
            }
        }
    }
}

#[test]
fn metadata_rollback_releases_the_waiting_ddl_without_publishing_stages() {
    for explicit in [false, true] {
        let db = database();
        let before = db.storage.get(&b"counter:guarded".to_vec()).unwrap();
        let txn = transaction(&db, explicit);
        txn.put(b"dict:guarded:body".to_vec(), dictionary()).unwrap();
        txn.stage_row_counter("guarded", 42).unwrap();
        run_ddl_after_staging(&db, txn, "guarded", "ALTER TABLE guarded ADD COLUMN spare INT", true);
        assert!(db.storage.get(&b"dict:guarded:body".to_vec()).unwrap().is_none());
        assert_eq!(db.storage.get(&b"counter:guarded".to_vec()).unwrap(), before);
        assert_eq!(db.query("SELECT id FROM guarded", &[]).unwrap().len(), 1);
    }
}

#[test]
fn dictionary_owner_matching_handles_colon_columns_and_keeps_unrelated_tables_free() {
    let db = database();
    db.execute("CREATE TABLE a (\"b:c\" TEXT)").unwrap();
    db.execute("CREATE TABLE unrelated (c TEXT)").unwrap();
    // Colon is accepted in a quoted column, but deliberately refused in a
    // relation name because that delimiter belongs to the storage key format.
    let sequence = db.storage.db().latest_sequence_number();
    let error = db.execute("CREATE TABLE \"a:b\" (c TEXT)").unwrap_err();
    assert!(matches!(&error, heliosdb_nano::Error::QueryExecution(_)), "{error}");
    assert!(error.to_string().contains("cannot hold ':'"), "{error}");
    assert_eq!(db.storage.db().latest_sequence_number(), sequence);

    let txn = db.storage.begin_autocommit_transaction().unwrap();
    txn.put(b"dict:a:b:c".to_vec(), dictionary()).unwrap();
    let other = Arc::clone(&db);
    let (done, completed) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        done.send(other.execute("ALTER TABLE unrelated ADD COLUMN extra INT"))
            .unwrap();
    });
    completed.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
    worker.join().unwrap();
    // Splitting at the last ':' would lock a nonexistent a:b and let a's
    // DDL through. The real owner must stay locked until this rollback.
    run_ddl_after_staging(&db, txn, "a", "ALTER TABLE a ADD COLUMN extra INT", true);
    assert!(db.storage.get(&b"dict:a:b:c".to_vec()).unwrap().is_none());
}

#[test]
fn dictionary_owner_locks_cover_a_valid_table_created_after_staging() {
    let db = database();
    let txn = db.storage.begin_autocommit_transaction().unwrap();
    txn.put(b"dict:future:b:c".to_vec(), dictionary()).unwrap();
    let other = Arc::clone(&db);
    let (done, completed) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        done.send(other.execute("CREATE TABLE future (\"b:c\" TEXT)")).unwrap();
    });
    wait_for_exclusive_request(&db, "future");
    assert!(
        completed.try_recv().is_err(),
        "prospective owner cannot appear during staging"
    );
    txn.rollback().unwrap();
    completed.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
    worker.join().unwrap();
    let schema = db.storage.catalog().get_table_schema("future").unwrap();
    assert_eq!(schema.columns[0].name, "b:c");
    assert!(db.storage.get(&b"dict:future:b:c".to_vec()).unwrap().is_none());
}

#[test]
fn raw_autocommit_mixed_data_and_metadata_keep_both_tables_in_either_staging_order() {
    for dictionary_metadata in [false, true] {
        for metadata_first in [false, true] {
            for data_operation in ["put", "fast", "delete"] {
                for ddl in [
                    "ALTER TABLE other RENAME TO moved",
                    "DROP TABLE other",
                    "TRUNCATE TABLE other",
                ] {
                    let db = database();
                    db.execute("CREATE TABLE other (id INT PRIMARY KEY, body TEXT)")
                        .unwrap();
                    db.execute("INSERT INTO other VALUES (1, 'old')").unwrap();
                    let txn = db.storage.begin_autocommit_transaction().unwrap();
                    let stage_metadata = || {
                        if dictionary_metadata {
                            txn.put(b"dict:guarded:body".to_vec(), dictionary()).unwrap();
                        } else {
                            txn.stage_row_counter("guarded", 42).unwrap();
                        }
                    };
                    let stage_data = || {
                        let key = b"data:other:1".to_vec();
                        let value =
                            bincode::serialize(&Tuple::new(vec![Value::Int4(1), Value::String("new".into())])).unwrap();
                        match data_operation {
                            "put" => txn.put(key, value).unwrap(),
                            "fast" => txn.put_insert_fast(key, value).unwrap(),
                            "delete" => txn.delete(key).unwrap(),
                            _ => unreachable!(),
                        }
                    };
                    if metadata_first {
                        stage_metadata();
                        stage_data();
                    } else {
                        stage_data();
                        stage_metadata();
                    }
                    run_ddl_after_staging(&db, txn, "other", ddl, false);
                    if ddl.starts_with("ALTER") || ddl.starts_with("DROP") {
                        assert!(db.storage.get(&b"data:other:1".to_vec()).unwrap().is_none());
                    }
                    if dictionary_metadata {
                        assert!(db.storage.get(&b"dict:guarded:body".to_vec()).unwrap().is_some());
                    } else {
                        assert_eq!(
                            bincode::deserialize::<u64>(
                                &db.storage.get(&b"counter:guarded".to_vec()).unwrap().unwrap()
                            )
                            .unwrap(),
                            42
                        );
                    }
                }
            }
        }
    }
}
