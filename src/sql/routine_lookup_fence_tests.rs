//! Calling-database routine ownership and unresolved-outcome read fences.
#![allow(clippy::unwrap_used, clippy::panic)]

use super::*;
use crate::{EmbeddedDatabase, Tuple};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_NAME: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug)]
enum Family {
    Text,
    Params,
}

fn open(path: &std::path::Path, time_travel: bool) -> EmbeddedDatabase {
    let mut config = crate::Config::default();
    config.storage.path = Some(path.into());
    config.storage.memory_only = false;
    config.storage.wal_enabled = true;
    config.storage.time_travel_enabled = time_travel;
    config.storage.version_gc_interval_secs = Some(0);
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    EmbeddedDatabase::with_config(config).unwrap()
}

fn execute(db: &EmbeddedDatabase, family: Family, sql: &str) -> Result<u64> {
    match family {
        Family::Text => db.execute(sql),
        Family::Params => db.execute_params(sql, &[]),
    }
}

fn query(db: &EmbeddedDatabase, family: Family, sql: &str) -> Result<Vec<Tuple>> {
    match family {
        Family::Text => db.query(sql, &[]),
        Family::Params => db.query_params(sql, &[]),
    }
}

fn scalar(db: &EmbeddedDatabase, family: Family, sql: &str) -> i64 {
    let rows = query(db, family, sql).unwrap();
    assert_eq!(rows.len(), 1, "{sql}");
    assert_eq!(rows[0].values.len(), 1, "{sql}");
    match rows[0].values[0] {
        Value::Int4(value) => i64::from(value),
        Value::Int8(value) => value,
        ref other => panic!("{sql}: {other:?}"),
    }
}

fn image(db: &EmbeddedDatabase) -> Vec<(Vec<u8>, Vec<u8>)> {
    db.storage
        .db()
        .iterator(rocksdb::IteratorMode::Start)
        .map(|entry| {
            let (key, value) = entry.unwrap();
            (key.to_vec(), value.to_vec())
        })
        .collect()
}

fn unavailable(error: Error, name: &str) {
    assert!(matches!(error, Error::Storage(_)), "typed storage refusal: {error}");
    let message = error.to_string();
    assert!(message.contains(name), "target name: {message}");
    assert!(
        message.contains("unavailable") && message.contains("reopen"),
        "{message}"
    );
}

fn corrupt_after_applied(db: &EmbeddedDatabase, family: Family, sql: &str, key: Vec<u8>) -> Vec<u8> {
    let corrupt = vec![0xff, 0x01, 0xe2];
    let bytes = corrupt.clone();
    let engine = Arc::clone(&db.storage);
    let recorded = Arc::new(AtomicU64::new(0));
    let hook_sequence = Arc::clone(&recorded);
    crate::storage::ddl_batch::debug_fail_next_applied_commit_with(move || {
        // Real owner group has applied; inject unreadable actual metadata
        // before its witness, then prove settlement performs no repair write.
        engine.put(&key, &bytes).unwrap();
        hook_sequence.store(engine.db().latest_sequence_number(), Ordering::Release);
    });
    let error = execute(db, family, sql).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected schema durability failure after the group applied"),
        "original native outcome: {error}"
    );
    assert!(recorded.load(Ordering::Acquire) > 0, "actual after-apply hook must run");
    assert_eq!(
        db.storage.db().latest_sequence_number(),
        recorded.load(Ordering::Acquire)
    );
    corrupt
}

fn fenced_function(form: &str) {
    for family in [Family::Text, Family::Params] {
        for time_travel in [false, true] {
            let suffix = NEXT_NAME.fetch_add(1, Ordering::Relaxed);
            let target = format!("routine_read_target_{suffix}");
            let healthy = format!("routine_read_healthy_{suffix}");
            let wrapper = format!("routine_read_wrapper_{suffix}");
            let absent = format!("routine_read_absent_{suffix}");
            let first_dir = tempfile::tempdir().unwrap();
            let first = open(first_dir.path(), time_travel);
            let create = |name: &str, value| {
                format!("CREATE FUNCTION {name}() RETURNS INTEGER AS $$ SELECT {value} $$ LANGUAGE sql")
            };
            execute(&first, family, &create(&target, 10)).unwrap();
            execute(&first, family, &create(&healthy, 7)).unwrap();
            execute(
                &first,
                family,
                &format!("CREATE FUNCTION {wrapper}() RETURNS INTEGER AS $$ SELECT {target}() $$ LANGUAGE sql"),
            )
            .unwrap();

            // Newer live database deliberately defines the SAME name. SQL
            // must call its own registry both before and after a fence.
            let second_dir = tempfile::tempdir().unwrap();
            let second = open(second_dir.path(), time_travel);
            execute(&second, family, &create(&target, 20)).unwrap();
            assert_eq!(scalar(&second, family, &format!("SELECT {target}()")), 20);

            let key = format!("meta:function:{target}").into_bytes();
            let corrupt = corrupt_after_applied(
                &first,
                family,
                &format!("CREATE OR REPLACE FUNCTION {target}() RETURNS INTEGER AS $$ SELECT 30 $$ LANGUAGE sql"),
                key.clone(),
            );
            assert!(first.function_registry.get_function(&target).is_none());
            let first_image = image(&first);
            let second_image = image(&second);
            let first_sequence = first.storage.db().latest_sequence_number();
            let second_sequence = second.storage.db().latest_sequence_number();
            let sql = match form {
                "direct" => format!("SELECT {target}()"),
                "body" => format!("SELECT {wrapper}()"),
                "scalar" => format!("SELECT (SELECT {target}())"),
                "exists" => format!("SELECT EXISTS (SELECT 1 WHERE {target}() = 10)"),
                "not_exists" => format!("SELECT NOT EXISTS (SELECT 1 WHERE {target}() = 10)"),
                "in" => format!("SELECT 10 IN (SELECT {target}())"),
                _ => panic!("unknown form {form}"),
            };
            unavailable(query(&first, family, &sql).unwrap_err(), &target);
            assert_eq!(first.storage.get(&key).unwrap(), Some(corrupt.clone()));
            assert_eq!(first.storage.db().latest_sequence_number(), first_sequence, "{sql}");
            unavailable(
                first
                    .function_registry
                    .execute_function(&target, &[], |_| panic!("fenced body must not run"))
                    .unwrap_err(),
                &target,
            );
            assert_eq!(scalar(&first, family, &format!("SELECT {healthy}()")), 7);
            assert_eq!(scalar(&second, family, &format!("SELECT {target}()")), 20);
            // A genuine unknown name retains the ordinary query error; it is
            // not confused with this name's unresolved-storage fence.
            let unknown = query(&first, family, &format!("SELECT {absent}()")).unwrap_err();
            assert!(!matches!(unknown, Error::Storage(_)), "{unknown}");
            assert_eq!(image(&first), first_image, "no failed-read repair or metadata rewrite");
            assert_eq!(
                image(&second),
                second_image,
                "foreign body/registry must remain untouched"
            );
            assert_eq!(first.storage.db().latest_sequence_number(), first_sequence);
            assert_eq!(second.storage.db().latest_sequence_number(), second_sequence);
            eprintln!("ROUTINE_READ_FENCE_CELL {form} {family:?} tt={time_travel}");
        }
    }
}

#[test]
fn owned_healthy_same_name_functions_use_their_own_database() {
    for family in [Family::Text, Family::Params] {
        for time_travel in [false, true] {
            let suffix = NEXT_NAME.fetch_add(1, Ordering::Relaxed);
            let target = format!("routine_healthy_owner_{suffix}");
            let first_dir = tempfile::tempdir().unwrap();
            let first = open(first_dir.path(), time_travel);
            execute(
                &first,
                family,
                &format!("CREATE FUNCTION {target}() RETURNS INTEGER AS $$ SELECT 10 $$ LANGUAGE sql"),
            )
            .unwrap();
            let second_dir = tempfile::tempdir().unwrap();
            let second = open(second_dir.path(), time_travel);
            execute(
                &second,
                family,
                &format!("CREATE FUNCTION {target}() RETURNS INTEGER AS $$ SELECT 20 $$ LANGUAGE sql"),
            )
            .unwrap();
            for _ in 0..3 {
                assert_eq!(scalar(&first, family, &format!("SELECT {target}()")), 10);
                assert_eq!(scalar(&second, family, &format!("SELECT {target}()")), 20);
            }
            eprintln!("ROUTINE_HEALTHY_OWNER_CELL {family:?} tt={time_travel}");
        }
    }
}

#[test]
fn fenced_function_direct_lookup_refuses_foreign_body() {
    fenced_function("direct");
}
#[test]
fn fenced_function_body_lookup_refuses_foreign_body() {
    fenced_function("body");
}
#[test]
fn fenced_function_scalar_subquery_propagates_storage() {
    fenced_function("scalar");
}
#[test]
fn fenced_function_exists_subquery_propagates_storage() {
    fenced_function("exists");
}
#[test]
fn fenced_function_not_exists_subquery_propagates_storage() {
    fenced_function("not_exists");
}
#[test]
fn fenced_function_in_subquery_propagates_storage() {
    fenced_function("in");
}

#[test]
fn owned_procedure_lookup_preserves_unresolved_storage_refusal() {
    for family in [Family::Text, Family::Params] {
        for time_travel in [false, true] {
            let suffix = NEXT_NAME.fetch_add(1, Ordering::Relaxed);
            let target = format!("routine_proc_target_{suffix}");
            let healthy = format!("routine_proc_healthy_{suffix}");
            let dir = tempfile::tempdir().unwrap();
            let db = open(dir.path(), time_travel);
            execute(&db, family, "CREATE TABLE routine_sink (observed INT)").unwrap();
            let create = |name: &str, value| {
                format!("CREATE PROCEDURE {name}() LANGUAGE sql AS $$ INSERT INTO routine_sink VALUES ({value}) $$")
            };
            execute(&db, family, &create(&target, 10)).unwrap();
            execute(&db, family, &create(&healthy, 7)).unwrap();
            let key = format!("meta:procedure:{target}").into_bytes();
            let corrupt = corrupt_after_applied(
                &db,
                family,
                &format!(
                    "CREATE OR REPLACE PROCEDURE {target}() LANGUAGE sql AS $$ INSERT INTO routine_sink VALUES (30) $$"
                ),
                key.clone(),
            );
            let before = image(&db);
            let sequence = db.storage.db().latest_sequence_number();
            unavailable(execute(&db, family, &format!("CALL {target}()")).unwrap_err(), &target);
            unavailable(
                db.function_registry
                    .execute_procedure(&target, &[], |_| panic!("fenced body must not run"))
                    .unwrap_err(),
                &target,
            );
            assert_eq!(db.storage.get(&key).unwrap(), Some(corrupt));
            assert_eq!(db.storage.db().latest_sequence_number(), sequence);
            assert_eq!(image(&db), before, "refused CALL must perform no repair or body write");
            assert_eq!(scalar(&db, family, "SELECT COUNT(*) FROM routine_sink"), 0);
            execute(&db, family, &format!("CALL {healthy}()")).unwrap();
            assert_eq!(scalar(&db, family, "SELECT observed FROM routine_sink"), 7);
            eprintln!("ROUTINE_PROCEDURE_READ_FENCE_CELL {family:?} tt={time_travel}");
        }
    }
}
