//! Extra routine outcome proofs; no new production hook or guard API.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::EmbeddedDatabase;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
enum Kind {
    Function,
    Procedure,
}
#[derive(Clone, Copy, Debug)]
enum Family {
    Text,
    Params,
}
#[derive(Clone, Copy, Debug)]
enum Fault {
    Success,
    Before,
    After,
    Applied,
}

fn open(path: &std::path::Path) -> EmbeddedDatabase {
    let mut config = crate::Config::default();
    config.storage.path = Some(path.into());
    config.storage.memory_only = false;
    config.storage.wal_enabled = true;
    config.storage.version_gc_interval_secs = Some(0);
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    EmbeddedDatabase::with_config(config).unwrap()
}

fn exec(db: &EmbeddedDatabase, family: Family, sql: &str) -> Result<u64> {
    match family {
        Family::Text => db.execute(sql),
        Family::Params => db.execute_params(sql, &[]),
    }
}

fn create(kind: Kind, name: &str, replace: bool, value: i32) -> String {
    let replace = if replace { "OR REPLACE " } else { "" };
    match kind {
        Kind::Function => {
            format!("CREATE {replace}FUNCTION {name}() RETURNS INTEGER AS $$ SELECT {value} $$ LANGUAGE sql")
        }
        Kind::Procedure => {
            format!("CREATE {replace}PROCEDURE {name}() LANGUAGE sql AS $$ INSERT INTO sink VALUES ({value}) $$")
        }
    }
}

fn prefix(kind: Kind) -> &'static str {
    match kind {
        Kind::Function => "meta:function:",
        Kind::Procedure => "meta:procedure:",
    }
}

fn noun(kind: Kind) -> &'static str {
    match kind {
        Kind::Function => "FUNCTION",
        Kind::Procedure => "PROCEDURE",
    }
}

fn body_value(body: &str) -> i32 {
    match (body.contains("10"), body.contains("20")) {
        (true, false) => 10,
        (false, true) => 20,
        other => panic!("unexpected fixture body {body:?}: {other:?}"),
    }
}

fn registry_value(registry: &FunctionRegistry, kind: Kind, name: &str) -> Option<i32> {
    match kind {
        Kind::Function => registry.get_function(name).map(|value| body_value(&value.body)),
        Kind::Procedure => registry.get_procedure(name).map(|value| body_value(&value.body)),
    }
}

fn callable(db: &EmbeddedDatabase, kind: Kind, name: &str, value: Option<i32>) {
    match kind {
        Kind::Function => {
            let result = db.query(&format!("SELECT {name}()"), &[]);
            if let Some(value) = value {
                let rows = result.unwrap();
                assert_eq!(rows.len(), 1);
                let got = match rows.first().unwrap().values.first().unwrap() {
                    Value::Int4(value) => i64::from(*value),
                    Value::Int8(value) => *value,
                    other => panic!("{other:?}"),
                };
                assert_eq!(got, i64::from(value));
            } else {
                assert!(result.is_err());
            }
        }
        Kind::Procedure => {
            db.execute("TRUNCATE TABLE sink").unwrap();
            let result = db.execute(&format!("CALL {name}()"));
            if let Some(value) = value {
                assert_eq!(result.unwrap(), 0);
                let rows = db.query("SELECT observed FROM sink", &[]).unwrap();
                assert_eq!(rows.len(), 1);
                let got = match rows.first().unwrap().values.first().unwrap() {
                    Value::Int4(value) => i64::from(*value),
                    Value::Int8(value) => *value,
                    other => panic!("{other:?}"),
                };
                assert_eq!(got, i64::from(value));
            } else {
                assert!(result.is_err());
            }
        }
    }
}

fn arm(db: &EmbeddedDatabase, fault: Fault) {
    match fault {
        Fault::Success => {}
        Fault::Before => db.storage.write_gate().debug_fail_next_native_write_before_apply(),
        Fault::After => db.storage.write_gate().debug_fail_next_native_write_after_apply(),
        Fault::Applied => crate::storage::ddl_batch::debug_fail_next_applied_commit(),
    }
}

fn result(result: Result<u64>, fault: Fault) {
    let marker = match fault {
        Fault::Success => {
            assert_eq!(result.unwrap(), 0);
            return;
        }
        Fault::Before => "injected native write error before apply; outcome is unknown",
        Fault::After => "injected native write error after apply; outcome is unknown",
        Fault::Applied => "injected schema durability failure after the group applied",
    };
    let error = result.unwrap_err();
    assert!(error.to_string().contains(marker), "original outcome error: {error}");
}

#[test]
fn unreadable_post_applied_routine_image_fences_only_target_without_repair() {
    for kind in [Kind::Function, Kind::Procedure] {
        for family in [Family::Text, Family::Params] {
            let dir = tempfile::tempdir().unwrap();
            let db = open(dir.path());
            db.execute("CREATE TABLE sink (observed INT)").unwrap();
            db.execute(&create(Kind::Function, "healthy_fn", false, 10)).unwrap();
            db.execute(&create(Kind::Procedure, "healthy_proc", false, 10)).unwrap();
            exec(&db, family, &create(kind, "target", false, 10)).unwrap();
            let key = format!("{}target", prefix(kind)).into_bytes();
            let corruption = vec![0xff, 0x01, 0xe2];
            let hook_key = key.clone();
            let bad = corruption.clone();
            let engine = Arc::clone(&db.storage);
            let sequence = Arc::new(AtomicU64::new(0));
            let recorded = Arc::clone(&sequence);
            crate::storage::ddl_batch::debug_fail_next_applied_commit_with(move || {
                // Intentional fault injection, BEFORE the outcome witness.
                // Record this write's end; later settlement must add no repair.
                engine.put(&hook_key, &bad).unwrap();
                recorded.store(engine.db().latest_sequence_number(), Ordering::Release);
            });
            result(exec(&db, family, &create(kind, "target", true, 20)), Fault::Applied);
            let after_injection = sequence.load(Ordering::Acquire);
            assert!(after_injection > 0);
            assert_eq!(db.storage.db().latest_sequence_number(), after_injection);
            assert_eq!(db.storage.get(&key).unwrap(), Some(corruption.clone()));
            assert!(match kind {
                Kind::Function => bincode::deserialize::<StoredFunction>(&corruption).is_err(),
                Kind::Procedure => bincode::deserialize::<StoredProcedure>(&corruption).is_err(),
            });
            assert_eq!(registry_value(&db.function_registry, kind, "target"), None);
            let retry = exec(&db, family, &create(kind, "target", true, 20)).unwrap_err();
            assert!(retry.to_string().contains("unavailable"), "{retry}");
            let drop = exec(&db, family, &format!("DROP {} IF EXISTS target", noun(kind))).unwrap_err();
            assert!(drop.to_string().contains("unavailable"), "{drop}");
            match kind {
                Kind::Function => {
                    let mut candidate = db.function_registry.get_function("healthy_fn").unwrap();
                    candidate.name = "target".into();
                    candidate.or_replace = true;
                    assert!(db.function_registry.register_function(candidate.clone()).is_err());
                    assert!(db.function_registry.load_function(candidate).is_err());
                    assert!(db.function_registry.drop_function("target", true).is_err());
                }
                Kind::Procedure => {
                    let mut candidate = db.function_registry.get_procedure("healthy_proc").unwrap();
                    candidate.name = "target".into();
                    candidate.or_replace = true;
                    assert!(db.function_registry.register_procedure(candidate.clone()).is_err());
                    assert!(db.function_registry.load_procedure(candidate).is_err());
                    assert!(db.function_registry.drop_procedure("target", true).is_err());
                }
            }
            assert_eq!(db.storage.db().latest_sequence_number(), after_injection);
            assert_eq!(db.storage.get(&key).unwrap(), Some(corruption));
            callable(&db, kind, "target", None);
            callable(&db, Kind::Function, "healthy_fn", Some(10));
            callable(&db, Kind::Procedure, "healthy_proc", Some(10));
            eprintln!("ROUTINE_UNREADABLE_CELL {kind:?} {family:?}");
        }
    }
}

#[test]
fn native_inflight_routine_map_owner_blocks_public_registry_getter_until_settlement() {
    for kind in [Kind::Function, Kind::Procedure] {
        for family in [Family::Text, Family::Params] {
            for fault in [Fault::Success, Fault::Before, Fault::After, Fault::Applied] {
                let dir = tempfile::tempdir().unwrap();
                let db = open(dir.path());
                db.execute("CREATE TABLE sink (observed INT)").unwrap();
                exec(&db, family, &create(kind, "target", false, 10)).unwrap();
                let registry = Arc::clone(&db.function_registry);
                let (ready_tx, ready_rx) = mpsc::channel();
                let (done_tx, done_rx) = mpsc::channel();
                let handle = Arc::new(Mutex::new(None));
                let captured_handle = Arc::clone(&handle);
                crate::storage::ddl_batch::debug_next_before_native_commit(move || {
                    // This proves actual lock ownership, rather than guessing
                    // from a delay or EmbeddedDatabase's transaction mutex.
                    let owned = match kind {
                        Kind::Function => {
                            matches!(registry.functions.try_read(), Err(std::sync::TryLockError::WouldBlock))
                        }
                        Kind::Procedure => {
                            matches!(registry.procedures.try_read(), Err(std::sync::TryLockError::WouldBlock))
                        }
                    };
                    assert!(owned, "DDL must retain the real existing routine map write guard");
                    let reader_registry = Arc::clone(&registry);
                    *captured_handle.lock().unwrap() = Some(std::thread::spawn(move || {
                        ready_tx.send(()).unwrap();
                        // PUBLIC registry API, never an EmbeddedDatabase SQL
                        // entry whose global txn mutex could mask this race.
                        let value = registry_value(&reader_registry, kind, "target");
                        done_tx.send(value).unwrap();
                    }));
                    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                });
                arm(&db, fault);
                result(exec(&db, family, &create(kind, "target", true, 20)), fault);
                let expected = if matches!(fault, Fault::Before) { 10 } else { 20 };
                assert_eq!(done_rx.recv_timeout(Duration::from_secs(5)).unwrap(), Some(expected));
                handle.lock().unwrap().take().unwrap().join().unwrap();
                assert_eq!(registry_value(&db.function_registry, kind, "target"), Some(expected));
                callable(&db, kind, "target", Some(expected));
                eprintln!("ROUTINE_OWNER_CELL {kind:?} {family:?} {fault:?}");
            }
        }
    }
}

#[test]
fn exact_missing_routine_drop_native_errors_do_not_invent_an_application_proof_or_fence() {
    for kind in [Kind::Function, Kind::Procedure] {
        for family in [Family::Text, Family::Params] {
            for fault in [Fault::Before, Fault::After] {
                let dir = tempfile::tempdir().unwrap();
                let db = open(dir.path());
                db.execute("CREATE TABLE sink (observed INT)").unwrap();
                arm(&db, fault);
                result(
                    exec(&db, family, &format!("DROP {} IF EXISTS absent", noun(kind))),
                    fault,
                );
                assert_eq!(registry_value(&db.function_registry, kind, "absent"), None);
                exec(&db, family, &create(kind, "absent", false, 20)).unwrap();
                callable(&db, kind, "absent", Some(20));
                eprintln!("ROUTINE_NOOP_CELL {kind:?} {family:?} {fault:?}");
            }
        }
    }
}
