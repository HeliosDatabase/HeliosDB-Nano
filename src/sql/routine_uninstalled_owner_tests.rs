//! An engine-scoped caller without a bridge cannot borrow a foreign routine.
#![allow(clippy::unwrap_used, clippy::panic)]

use super::*;
use crate::sql::functions::StoredFunction;
use crate::sql::{Executor, LogicalPlan, Parser, Planner};
use crate::storage::StorageEngine;
use crate::{Config, DataType, EmbeddedDatabase, Tuple};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_CONTEXTLESS_NAME: AtomicU64 = AtomicU64::new(0);

fn config(history: bool) -> Config {
    let mut config = Config::default();
    config.storage.path = None;
    config.storage.memory_only = true;
    config.storage.time_travel_enabled = history;
    config.storage.version_gc_interval_secs = Some(0);
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    config
}

fn plan(name: &str) -> LogicalPlan {
    Planner::new()
        .statement_to_plan(Parser::new().parse_one(&format!("SELECT {name}()")).unwrap())
        .unwrap()
}

fn scalar(rows: Vec<Tuple>) -> i64 {
    match rows.as_slice() {
        [row] => match row.values.as_slice() {
            [Value::Int4(value)] => i64::from(*value),
            [Value::Int8(value)] => *value,
            other => panic!("unexpected scalar values: {other:?}"),
        },
        other => panic!("unexpected scalar rows: {other:?}"),
    }
}

fn image(engine: &StorageEngine) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut read = rocksdb::ReadOptions::default();
    read.set_total_order_seek(true);
    engine
        .db()
        .iterator_opt(rocksdb::IteratorMode::Start, read)
        .map(|record| {
            let (key, value) = record.unwrap();
            (key.to_vec(), value.to_vec())
        })
        .collect()
}

#[test]
fn engine_scope_without_installed_bridge_never_executes_another_database_body() {
    for with_schema in [false, true] {
        for history in [false, true] {
            let config = config(history);
            let a = Arc::new(StorageEngine::open_in_memory(&config).unwrap());
            // This public registration establishes A's real EngineScope
            // identity without installing any UDF registry/bridge for it.
            crate::sql::sequences::install_persistence(&a);
            let b = EmbeddedDatabase::with_config(config).unwrap();
            let name = format!("routine_uninstalled_owner_{}", a.instance_id());
            b.execute(&format!(
                "CREATE FUNCTION {name}() RETURNS INTEGER AS $$ SELECT 23 $$ LANGUAGE sql"
            ))
            .unwrap();
            let calls = Arc::new(AtomicU64::new(0));
            let observed = Arc::clone(&calls);
            let original = Arc::clone(b.udf_bridge.get().unwrap());
            let counting = Arc::new(UdfBridge {
                registry: Arc::clone(&b.function_registry),
                max_call_depth: original.max_call_depth,
                exec: Box::new(move |sql| {
                    observed.fetch_add(1, Ordering::Relaxed);
                    (original.exec)(sql)
                }),
            });
            // The same normal managed registration API, wrapping B's REAL
            // SQL body executor, proves a foreign body was never invoked.
            install_on(&b.storage, &counting);
            assert_eq!(scalar(b.query(&format!("SELECT {name}()"), &[]).unwrap()), 23);
            assert!(calls.load(Ordering::Relaxed) > 0, "real B body control must run");
            calls.store(0, Ordering::Relaxed);
            let a_before = image(&a);
            let b_before = image(&b.storage);
            let a_sequence = a.db().latest_sequence_number();
            let b_sequence = b.storage.db().latest_sequence_number();
            {
                let _owner = crate::sql::sequences::EngineScope::enter(&a);
                assert_eq!(
                    crate::sql::sequences::current_engine().unwrap().instance_id(),
                    a.instance_id()
                );
            }

            let mut executor = Executor::with_storage(&a);
            let result = if with_schema {
                executor.execute_with_schema(&plan(&name)).map(|(rows, _)| rows)
            } else {
                executor.execute(&plan(&name))
            };
            let error = result.unwrap_err();
            assert!(
                matches!(error, Error::QueryExecution(_)),
                "ordinary unknown function: {error}"
            );
            assert!(
                error.to_string().contains(&format!("Unknown scalar function: {name}")),
                "{error}"
            );
            assert_eq!(
                calls.load(Ordering::Relaxed),
                0,
                "A must never invoke B's actual SQL body"
            );
            assert_eq!(image(&a), a_before);
            assert_eq!(image(&b.storage), b_before);
            assert_eq!(a.db().latest_sequence_number(), a_sequence);
            assert_eq!(b.storage.db().latest_sequence_number(), b_sequence);
            {
                let _owner = crate::sql::sequences::EngineScope::enter(&a);
                assert!(
                    resolve_owned(&name).unwrap().is_none(),
                    "absence under an engine owner is authoritative"
                );
            }
            assert_eq!(scalar(b.query(&format!("SELECT {name}()"), &[]).unwrap()), 23);
            eprintln!("ROUTINE_UNINSTALLED_OWNER_CELL with_schema={with_schema} tt={history}");
        }
    }
}

#[test]
fn contextless_public_install_and_resolve_keep_their_legacy_behavior() {
    for with_schema in [false, true] {
        assert!(
            crate::sql::sequences::current_engine().is_none(),
            "this control must have no engine scope"
        );
        let suffix = NEXT_CONTEXTLESS_NAME.fetch_add(1, Ordering::Relaxed);
        let name = format!("routine_contextless_legacy_{suffix}");
        let registry = Arc::new(FunctionRegistry::new());
        registry
            .register_function(StoredFunction {
                name: name.clone(),
                or_replace: false,
                params: vec![],
                return_type: Some(DataType::Int4),
                body: "SELECT 29".into(),
                language: "sql".into(),
                volatility: None,
                created_at: 0,
            })
            .unwrap();
        let calls = Arc::new(AtomicU64::new(0));
        let observed = Arc::clone(&calls);
        let bridge = Arc::new(UdfBridge {
            registry,
            max_call_depth: 4,
            exec: Box::new(move |_| {
                observed.fetch_add(1, Ordering::Relaxed);
                Ok(vec![vec![Value::Int4(29)]])
            }),
        });
        install(&bridge);
        let public = resolve(&name).unwrap();
        assert!(Arc::ptr_eq(&public, &bridge));
        assert_eq!(call_scalar(&public, &name, &[]).unwrap(), Value::Int4(29));
        assert!(Arc::ptr_eq(&resolve_owned(&name).unwrap().unwrap(), &bridge));
        let mut executor = Executor::new();
        let rows = if with_schema {
            executor.execute_with_schema(&plan(&name)).unwrap().0
        } else {
            executor.execute(&plan(&name)).unwrap()
        };
        assert_eq!(scalar(rows), 29);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert!(crate::sql::sequences::current_engine().is_none());
        eprintln!("ROUTINE_CONTEXTLESS_LEGACY_CELL with_schema={with_schema}");
    }
}

#[test]
fn unregistered_raw_engine_scope_never_borrows_the_sequence_fallback_database_bridge() {
    for with_schema in [false, true] {
        for history in [false, true] {
            let config = config(history);
            let b = EmbeddedDatabase::with_config(config.clone()).unwrap();
            let a = Arc::new(StorageEngine::open_in_memory(&config).unwrap());
            // Deliberately NO install_persistence(&a): public raw Engine SQL
            // still binds A's actual engine ID in EngineScope. A missing
            // sequence namespace must not turn that UDF owner into B.
            let name = format!("routine_unregistered_scope_{}", a.instance_id());
            b.execute(&format!(
                "CREATE FUNCTION {name}() RETURNS INTEGER AS $$ SELECT 23 $$ LANGUAGE sql"
            ))
            .unwrap();
            let calls = Arc::new(AtomicU64::new(0));
            let observed = Arc::clone(&calls);
            let original = Arc::clone(b.udf_bridge.get().unwrap());
            let counting = Arc::new(UdfBridge {
                registry: Arc::clone(&b.function_registry),
                max_call_depth: original.max_call_depth,
                exec: Box::new(move |sql| {
                    observed.fetch_add(1, Ordering::Relaxed);
                    (original.exec)(sql)
                }),
            });
            install_on(&b.storage, &counting);
            assert_eq!(scalar(b.query(&format!("SELECT {name}()"), &[]).unwrap()), 23);
            assert!(
                calls.load(Ordering::Relaxed) > 0,
                "real B body must execute in its own DB"
            );
            calls.store(0, Ordering::Relaxed);
            let a_before = image(&a);
            let b_before = image(&b.storage);
            let a_sequence = a.db().latest_sequence_number();
            let b_sequence = b.storage.db().latest_sequence_number();
            let mut executor = Executor::with_storage(&a);
            let result = if with_schema {
                executor.execute_with_schema(&plan(&name)).map(|(rows, _)| rows)
            } else {
                executor.execute(&plan(&name))
            };
            let error = result.unwrap_err();
            assert!(
                matches!(error, Error::QueryExecution(_)),
                "ordinary unknown function: {error}"
            );
            assert!(
                error.to_string().contains(&format!("Unknown scalar function: {name}")),
                "{error}"
            );
            assert_eq!(
                calls.load(Ordering::Relaxed),
                0,
                "unregistered A must not execute B's body"
            );
            assert_eq!(image(&a), a_before);
            assert_eq!(image(&b.storage), b_before);
            assert_eq!(a.db().latest_sequence_number(), a_sequence);
            assert_eq!(b.storage.db().latest_sequence_number(), b_sequence);
            assert_eq!(scalar(b.query(&format!("SELECT {name}()"), &[]).unwrap()), 23);
            assert!(
                crate::sql::sequences::current_engine().is_none(),
                "executor must restore its outer context"
            );
            eprintln!("ROUTINE_UNREGISTERED_SCOPE_CELL with_schema={with_schema} tt={history}");
        }
    }
}
