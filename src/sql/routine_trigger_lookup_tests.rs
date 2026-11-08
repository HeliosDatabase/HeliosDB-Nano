//! An unresolved function cannot silently become a missing trigger recipe.
#![allow(clippy::unwrap_used, clippy::panic)]

use super::*;
use crate::EmbeddedDatabase;
use std::sync::atomic::{AtomicU64, Ordering};

fn open(path: &std::path::Path, history: bool) -> EmbeddedDatabase {
    let mut config = crate::Config::default();
    config.storage.path = Some(path.into());
    config.storage.memory_only = false;
    config.storage.wal_enabled = true;
    config.storage.time_travel_enabled = history;
    config.storage.version_gc_interval_secs = Some(0);
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    EmbeddedDatabase::with_config(config).unwrap()
}

fn execute(db: &EmbeddedDatabase, params: bool, sql: &str) -> Result<u64> {
    if params {
        db.execute_params(sql, &[])
    } else {
        db.execute(sql)
    }
}

fn observed(db: &EmbeddedDatabase, params: bool, table: &str) -> i64 {
    let sql = format!("SELECT v FROM {table}");
    let rows = if params {
        db.query_params(&sql, &[])
    } else {
        db.query(&sql, &[])
    }
    .unwrap();
    match rows.as_slice() {
        [row] => match row.values.as_slice() {
            [Value::Int4(value)] => i64::from(*value),
            [Value::Int8(value)] => *value,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
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

fn create_function(name: &str, rewrite: bool) -> String {
    let assignment = if rewrite { "NEW.v = NEW.v * 2;" } else { "" };
    format!("CREATE FUNCTION {name}() RETURNS TRIGGER AS $$ BEGIN {assignment} RETURN NEW; END $$ LANGUAGE plpgsql")
}

#[test]
fn fenced_before_insert_recipe_refuses_before_any_trigger_publication() {
    for params in [false, true] {
        for history in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let db = open(dir.path(), history);
            let target = format!("recipe_target_{}", db.storage.instance_id());
            execute(&db, params, "CREATE TABLE recipe_rows (v INT)").unwrap();
            execute(&db, params, "CREATE TABLE after_rows (v INT)").unwrap();
            execute(&db, params, &create_function(&target, true)).unwrap();
            let key = format!("meta:function:{target}").into_bytes();
            let corrupt = vec![0xff, 0x01, 0xe2];
            let hook_key = key.clone();
            let hook_value = corrupt.clone();
            let engine = Arc::clone(&db.storage);
            let sequence = Arc::new(AtomicU64::new(0));
            let hook_sequence = Arc::clone(&sequence);
            crate::storage::ddl_batch::debug_fail_next_applied_commit_with(move || {
                engine.put(&hook_key, &hook_value).unwrap();
                hook_sequence.store(engine.db().latest_sequence_number(), Ordering::Release);
            });
            let error = execute(
                &db,
                params,
                &create_function(&target, true).replacen("CREATE FUNCTION", "CREATE OR REPLACE FUNCTION", 1),
            )
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("injected schema durability failure after the group applied"),
                "{error}"
            );
            let injected = sequence.load(Ordering::Acquire);
            assert!(injected > 0);
            assert_eq!(db.storage.db().latest_sequence_number(), injected);
            assert!(db.function_registry.get_function(&target).is_none());
            let before = image(&db);
            assert!(!db
                .trigger_registry
                .trigger_exists("recipe_rows", "must_refuse")
                .unwrap());
            assert!(!db.trigger_registry.has_row_mutations("recipe_rows"));
            let error = execute(
                &db,
                params,
                &format!(
                    "CREATE TRIGGER must_refuse BEFORE INSERT ON recipe_rows FOR EACH ROW EXECUTE FUNCTION {target}()"
                ),
            )
            .unwrap_err();
            assert!(
                matches!(error, Error::Storage(_)),
                "typed unresolved read error: {error}"
            );
            assert!(
                error.to_string().contains(&target) && error.to_string().contains("unavailable"),
                "{error}"
            );
            assert!(!db
                .trigger_registry
                .trigger_exists("recipe_rows", "must_refuse")
                .unwrap());
            assert!(!db.trigger_registry.has_row_mutations("recipe_rows"));
            assert_eq!(db.storage.get(&key).unwrap(), Some(corrupt));
            assert_eq!(
                db.storage.db().latest_sequence_number(),
                injected,
                "no refused-trigger group or repair write"
            );
            assert_eq!(image(&db), before, "complete durable image must remain unchanged");

            // AFTER bodies are the existing inert surface; this change must
            // not introduce a function-read requirement for those definitions.
            execute(
                &db,
                params,
                &format!(
                    "CREATE TRIGGER still_inert AFTER INSERT ON after_rows FOR EACH ROW EXECUTE FUNCTION {target}()"
                ),
            )
            .unwrap();
            assert!(!db.trigger_registry.has_row_mutations("after_rows"));
            execute(&db, params, "INSERT INTO after_rows VALUES (9)").unwrap();
            assert_eq!(observed(&db, params, "after_rows"), 9);
            eprintln!("ROUTINE_TRIGGER_FENCE_CELL params={params} tt={history}");
        }
    }
}

#[test]
fn ordinary_missing_and_unsupported_recipes_stay_inert_and_healthy_recipe_rewrites() {
    for params in [false, true] {
        for history in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let db = open(dir.path(), history);
            for table in ["missing_recipe", "unsupported_recipe", "healthy_recipe"] {
                execute(&db, params, &format!("CREATE TABLE {table} (v INT)")).unwrap();
            }
            let suffix = db.storage.instance_id();
            let unsupported = format!("recipe_unsupported_{suffix}");
            let healthy = format!("recipe_healthy_{suffix}");
            let missing = format!("recipe_missing_{suffix}");
            execute(&db, params, &create_function(&unsupported, false)).unwrap();
            execute(&db, params, &create_function(&healthy, true)).unwrap();
            for (table, function, expected) in [
                ("missing_recipe", missing, 7),
                ("unsupported_recipe", unsupported, 7),
                ("healthy_recipe", healthy, 14),
            ] {
                execute(&db, params, &format!("CREATE TRIGGER recipe_{table} BEFORE INSERT ON {table} FOR EACH ROW EXECUTE FUNCTION {function}()")).unwrap();
                assert!(db
                    .trigger_registry
                    .trigger_exists(table, &format!("recipe_{table}"))
                    .unwrap());
                assert_eq!(db.trigger_registry.has_row_mutations(table), expected == 14);
                execute(&db, params, &format!("INSERT INTO {table} VALUES (7)")).unwrap();
                assert_eq!(observed(&db, params, table), expected);
            }
            eprintln!("ROUTINE_TRIGGER_COMPAT_CELL params={params} tt={history}");
        }
    }
}
