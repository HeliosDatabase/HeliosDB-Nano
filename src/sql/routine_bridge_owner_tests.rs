//! Managed UDF discovery must not retain the global bridge mutex during DDL IO.
#![allow(clippy::unwrap_used, clippy::panic)]

use super::*;
use crate::{EmbeddedDatabase, Tuple};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Evidence {
    map_owned: bool,
    reader_started: bool,
    reader_selected: bool,
    other_finished_while_parked: Option<std::result::Result<i64, String>>,
}

type Worker = std::thread::JoinHandle<Result<i64>>;

#[derive(Default)]
struct Workers(Vec<(&'static str, Worker)>);

impl Drop for Workers {
    fn drop(&mut self) {
        // Panic cleanup also reaps workers after the owning DDL guard unwinds.
        // Normal proof joins every worker explicitly before any assertion.
        for (_, handle) in self.0.drain(..) {
            let _ = handle.join();
        }
    }
}

fn open(path: &std::path::Path, history: bool) -> Arc<EmbeddedDatabase> {
    let mut config = crate::Config::default();
    config.storage.path = Some(path.into());
    config.storage.memory_only = false;
    config.storage.time_travel_enabled = history;
    config.storage.wal_enabled = true;
    config.storage.version_gc_interval_secs = Some(0);
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    config.storage.query_timeout_ms = Some(10_000);
    Arc::new(EmbeddedDatabase::with_config(config).unwrap())
}

fn execute(db: &EmbeddedDatabase, params: bool, sql: &str) -> Result<u64> {
    if params {
        db.execute_params(sql, &[])
    } else {
        db.execute(sql)
    }
}

fn query(db: &EmbeddedDatabase, params: bool, sql: &str) -> Result<Vec<Tuple>> {
    if params {
        db.query_params(sql, &[])
    } else {
        db.query(sql, &[])
    }
}

fn value(rows: Vec<Tuple>) -> Result<i64> {
    match rows.as_slice() {
        [row] => match row.values.as_slice() {
            [Value::Int4(value)] => Ok(i64::from(*value)),
            [Value::Int8(value)] => Ok(*value),
            other => Err(Error::internal(format!("unexpected scalar shape: {other:?}"))),
        },
        other => Err(Error::internal(format!("unexpected result shape: {other:?}"))),
    }
}

#[test]
fn managed_udf_reader_waiting_for_ddl_does_not_block_other_database_lookup() {
    for params in [false, true] {
        for history in [false, true] {
            let dir_a = tempfile::tempdir().unwrap();
            let dir_b = tempfile::tempdir().unwrap();
            let a = open(dir_a.path(), history);
            let b = open(dir_b.path(), history);
            let target = format!("routine_bridge_wait_{}_{}", a.storage.instance_id(), params);
            let other = format!("routine_bridge_other_{}", b.storage.instance_id());
            execute(
                &a,
                params,
                &format!("CREATE FUNCTION {target}() RETURNS INTEGER AS $$ SELECT 10 $$ LANGUAGE sql"),
            )
            .unwrap();
            execute(
                &b,
                params,
                &format!("CREATE FUNCTION {other}() RETURNS INTEGER AS $$ SELECT 20 $$ LANGUAGE sql"),
            )
            .unwrap();

            let registry = Arc::clone(&a.function_registry);
            let engine = Arc::clone(&a.storage);
            let bridge = Arc::clone(a.udf_bridge.get().unwrap());
            let lookup_name = target.clone();
            let workers = Arc::new(Mutex::new(Workers::default()));
            let hook_workers = Arc::clone(&workers);
            let evidence = Arc::new(Mutex::new(Evidence::default()));
            let hook_evidence = Arc::clone(&evidence);
            let other_db = Arc::clone(&b);
            let other_sql = format!("SELECT {other}()");
            crate::storage::ddl_batch::debug_next_before_native_commit(move || {
                let map_owned = matches!(registry.functions.try_read(), Err(std::sync::TryLockError::WouldBlock));
                let before = Arc::strong_count(&bridge);
                let (started_tx, started_rx) = mpsc::sync_channel(0);
                let reader = std::thread::spawn(move || {
                    let _owner = crate::sql::sequences::EngineScope::enter(&engine);
                    let _ = started_tx.send(());
                    // Direct bridge lookup, never EmbeddedDatabase's global
                    // transaction mutex: only the real map owner can block it.
                    Ok(i64::from(
                        crate::sql::udf_bridge::resolve_owned(&lookup_name)?.is_some(),
                    ))
                });
                hook_workers.lock().unwrap().0.push(("owner lookup", reader));
                let reader_started = started_rx.recv_timeout(Duration::from_secs(5)).is_ok();
                let deadline = Instant::now() + Duration::from_secs(5);
                while Arc::strong_count(&bridge) <= before && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(1));
                }
                // This Arc is upgraded only by resolve_owned's actual bridge
                // selection; map ownership prevents the lookup from returning.
                let reader_selected = Arc::strong_count(&bridge) > before;
                let (done_tx, done_rx) = mpsc::sync_channel(1);
                let other_reader = std::thread::spawn(move || {
                    let result = query(&other_db, params, &other_sql).and_then(value);
                    let _ = done_tx.send(result.as_ref().map(|value| *value).map_err(ToString::to_string));
                    result
                });
                hook_workers.lock().unwrap().0.push(("other SQL", other_reader));
                let other_finished_while_parked = done_rx.recv_timeout(Duration::from_secs(2)).ok();
                *hook_evidence.lock().unwrap() = Evidence {
                    map_owned,
                    reader_started,
                    reader_selected,
                    other_finished_while_parked,
                };
                // Always release the writer even on a timeout: old V2 holds
                // BRIDGES here, so asserting before release would deadlock joins.
            });
            let written = execute(
                &a,
                params,
                &format!("CREATE OR REPLACE FUNCTION {target}() RETURNS INTEGER AS $$ SELECT 30 $$ LANGUAGE sql"),
            );
            let joined: Vec<_> = workers
                .lock()
                .unwrap()
                .0
                .drain(..)
                .map(|(name, handle)| (name, handle.join()))
                .collect();

            // Every worker has been reaped before reporting a failure.
            written.unwrap();
            assert_eq!(joined.len(), 2);
            for (name, outcome) in joined {
                let got = outcome.unwrap().unwrap();
                assert_eq!(got, if name == "owner lookup" { 1 } else { 20 }, "{name}");
            }
            let proof = evidence.lock().unwrap();
            assert!(
                proof.map_owned,
                "actual existing function map must be owned at native cut"
            );
            assert!(
                proof.reader_started && proof.reader_selected,
                "reader must reach actual bridge selection"
            );
            assert_eq!(
                proof.other_finished_while_parked,
                Some(Ok(20)),
                "other DB SQL must finish before writer releases"
            );
            assert_eq!(
                value(query(&a, params, &format!("SELECT {target}()")).unwrap()).unwrap(),
                30
            );
            assert_eq!(
                value(query(&b, params, &format!("SELECT {other}()")).unwrap()).unwrap(),
                20
            );
            eprintln!("ROUTINE_BRIDGE_OWNER_CELL params={params} tt={history}");
        }
    }
}
