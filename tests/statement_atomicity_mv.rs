//! MV/executor preconditions for R5.7: each logical operation rides its data group.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::sql::{Executor, LogicalPlan};
use heliosdb_nano::storage::{WalEntry, WalOperation};
use heliosdb_nano::{Column, Config, DataType, EmbeddedDatabase, Schema, Tuple, Value};
use rocksdb::WriteBatchIterator;
use std::path::Path;
use std::time::Duration;

fn open(path: &Path, strict: bool) -> EmbeddedDatabase {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    config.storage.wal_enabled = true;
    config.storage.logical_wal_per_statement = strict;
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    config.replication.physical.enabled = true;
    let mut last = None;
    for _ in 0..100 {
        match EmbeddedDatabase::with_config(config.clone()) {
            Ok(db) => return db,
            Err(error) => last = Some(error),
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("open failed: {last:?}");
}

#[derive(Default, Debug)]
struct Group {
    keys: Vec<String>,
    operations: Vec<WalOperation>,
}
impl WriteBatchIterator for Group {
    fn put(&mut self, key: Box<[u8]>, value: Box<[u8]>) {
        if key.starts_with(b"wal:entries:") {
            self.operations.push(WalEntry::deserialize(&value).unwrap().operation);
        }
        self.keys.push(String::from_utf8_lossy(&key).into_owned());
    }
    fn delete(&mut self, key: Box<[u8]>) {
        self.keys.push(String::from_utf8_lossy(&key).into_owned());
    }
}
impl Group {
    fn has(&self, prefix: &str) -> bool {
        self.keys.iter().any(|key| key.starts_with(prefix))
    }
}
fn latest(db: &EmbeddedDatabase) -> u64 {
    db.storage.db().latest_sequence_number()
}
fn groups(db: &EmbeddedDatabase, after: u64) -> Vec<Group> {
    let end = latest(db);
    let raw = db.storage.db();
    let mut next = after + 1;
    let mut groups = Vec::new();
    let mut updates = raw.get_updates_since(after).unwrap();
    for item in updates.by_ref() {
        let (first, batch) = item.unwrap();
        if first > end {
            break;
        }
        assert_eq!(first, next, "no missing WAL groups");
        next = first + batch.len() as u64;
        let mut group = Group::default();
        batch.iterate(&mut group);
        // Derived snapshots and empty outer-transaction bookkeeping have no
        // row/catalog/WAL effects. No logical-WAL-only group is excluded.
        if group.keys.iter().any(|key| {
            !["snapshot:", "scn_map:@", "idxsnap", "vecsnap"]
                .iter()
                .any(|p| key.starts_with(p))
        }) {
            groups.push(group);
        }
    }
    updates.status().unwrap();
    assert_eq!(next, end + 1);
    groups
}
fn rows(db: &EmbeddedDatabase, table: &str) -> Vec<Vec<Value>> {
    let mut rows: Vec<_> = db
        .query(&format!("SELECT * FROM {table}"), &[])
        .unwrap()
        .into_iter()
        .map(|tuple| tuple.values)
        .collect();
    rows.sort_by_key(|row| format!("{row:?}"));
    rows
}
fn run(db: &EmbeddedDatabase, params: bool, sql: &str) {
    if params {
        db.execute_params(sql, &[]).unwrap();
    } else {
        db.execute(sql).unwrap();
    }
}
fn seed(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE src (id INT PRIMARY KEY, tag TEXT, body TEXT, v INT)")
        .unwrap();
    db.execute("INSERT INTO src VALUES (1, 'old', 'body1', 8), (2, 'old', 'body2', 16)")
        .unwrap();
}
fn assert_mv_group(db: &EmbeddedDatabase, before: u64, create: bool) {
    let groups = groups(db, before);
    assert_eq!(groups.len(), 1, "one visible group: {groups:#?}");
    let group = &groups[0];
    assert!(group.has("meta:mv:mv"), "{group:#?}");
    assert!(group.has("data:__mv_mv:"), "{group:#?}");
    assert!(group.has("snapshot:"), "{group:#?}");
    assert_eq!(
        group
            .operations
            .iter()
            .filter(|op| matches!(op,
                WalOperation::Insert { table, .. } if table == "__mv_mv"
            ))
            .count(),
        2,
        "the two row entries are in the data group: {group:#?}"
    );
    assert_eq!(
        group
            .operations
            .iter()
            .filter(|op| if create {
                matches!(op, WalOperation::CreateMaterializedView { name, .. } if name == "mv")
            } else {
                matches!(op, WalOperation::RefreshMaterializedView { name, .. } if name == "mv")
            })
            .count(),
        1,
        "the operation's entry is in its own group: {group:#?}"
    );
}

#[test]
fn create_materialized_view_data_catalog_and_wal_are_one_group_on_both_families() {
    for strict in [false, true] {
        for params in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let db = open(dir.path(), strict);
            seed(&db);
            let before = latest(&db);
            run(&db, params, "CREATE MATERIALIZED VIEW mv AS SELECT * FROM src");
            assert_mv_group(&db, before, true);
            assert_eq!(rows(&db, "mv"), rows(&db, "src"));
        }
    }
}

#[test]
fn indexed_and_columnar_dictionary_cas_refresh_keeps_layout_history_and_one_group() {
    for strict in [false, true] {
        for params in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let db = open(dir.path(), strict);
            seed(&db);
            run(&db, params, "CREATE MATERIALIZED VIEW mv AS SELECT * FROM src");
            db.execute("CREATE INDEX mv_tag_idx ON __mv_mv (tag)").unwrap();
            db.execute("ALTER TABLE __mv_mv ALTER COLUMN tag SET STORAGE DICTIONARY")
                .unwrap();
            db.execute("ALTER TABLE __mv_mv ALTER COLUMN body SET STORAGE CONTENT_ADDRESSED")
                .unwrap();
            db.execute("ALTER TABLE __mv_mv ALTER COLUMN v SET STORAGE COLUMNAR")
                .unwrap();
            let before_rows = rows(&db, "mv");
            let snapshot = db.storage.snapshot_manager().max_snapshot_timestamp().unwrap();
            let schema = db.storage.catalog().get_table_schema("__mv_mv").unwrap();
            let indexes = db.storage.art_indexes().list_table_indexes("__mv_mv");
            db.execute("UPDATE src SET tag = 'new', v = v + 1").unwrap();
            for sql in [
                "REFRESH MATERIALIZED VIEW mv",
                "REFRESH MATERIALIZED VIEW CONCURRENTLY mv",
            ] {
                let before = latest(&db);
                run(&db, params, sql);
                assert_mv_group(&db, before, false);
                assert_eq!(rows(&db, "mv"), rows(&db, "src"));
                assert!(db.storage.art_indexes().index_exists("mv_tag_idx"));
                assert_eq!(db.storage.art_indexes().list_table_indexes("__mv_mv"), indexes);
                assert_eq!(
                    db.storage.catalog().get_table_schema("__mv_mv").unwrap().columns,
                    schema.columns
                );
                assert_eq!(
                    db.query("SELECT id FROM __mv_mv WHERE tag = 'new' ORDER BY id", &[])
                        .unwrap()
                        .len(),
                    2
                );
                assert!(db
                    .query("SELECT id FROM __mv_mv WHERE tag = 'old'", &[])
                    .unwrap()
                    .is_empty());
            }
            let mut historical: Vec<_> = db
                .storage
                .scan_table_at_snapshot("__mv_mv", snapshot)
                .unwrap()
                .into_iter()
                .map(|tuple| tuple.values)
                .collect();
            historical.sort_by_key(|row| format!("{row:?}"));
            assert_eq!(historical, before_rows, "refresh retains its old logical images");
            drop(db);
            let db = open(dir.path(), strict);
            assert_eq!(rows(&db, "mv"), rows(&db, "src"));
            assert!(db.storage.art_indexes().index_exists("mv_tag_idx"));
        }
    }
}

#[test]
fn changed_logical_shape_replacement_is_atomic_and_has_its_own_history_cut_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), false);
    seed(&db);
    db.execute("CREATE MATERIALIZED VIEW mv AS SELECT * FROM src").unwrap();
    db.execute("CREATE INDEX mv_tag_idx ON __mv_mv (tag)").unwrap();
    let old = db.storage.snapshot_manager().max_snapshot_timestamp().unwrap();
    let schema = Schema::new(vec![Column::new("replacement", DataType::Text)]);
    let before = latest(&db);
    db.storage
        .mv_catalog()
        .store_view_data_concurrent("mv", vec![Tuple::new(vec![Value::String("new shape".into())])], &schema)
        .unwrap();
    let written_groups = groups(&db, before);
    assert_eq!(written_groups.len(), 1, "{written_groups:#?}");
    assert!(written_groups[0].has("meta:table_layout:__mv_mv"));
    assert!(written_groups[0].has("meta:mv:mv"));
    assert!(written_groups[0].has("snapshot:"));
    assert_eq!(rows(&db, "mv"), vec![vec![Value::String("new shape".into())]]);
    assert!(!db.storage.art_indexes().index_exists("mv_tag_idx"));
    let error = db.storage.scan_table_at_snapshot("__mv_mv", old).unwrap_err();
    assert!(error.to_string().contains("refreshed"), "{error}");
    let at = db.storage.snapshot_manager().max_snapshot_timestamp().unwrap();
    assert!(at > old);
    assert_eq!(db.storage.scan_table_at_snapshot("__mv_mv", at).unwrap().len(), 1);
    let empty_shape = Schema::new(vec![Column::new("empty", DataType::Int4)]);
    let before = latest(&db);
    db.storage
        .mv_catalog()
        .store_view_data("mv", Vec::new(), &empty_shape)
        .unwrap();
    let empty_groups = groups(&db, before);
    assert_eq!(empty_groups.len(), 1, "{empty_groups:#?}");
    assert!(
        empty_groups[0].has("snapshot:"),
        "an empty replacement has its own snapshot"
    );
    let empty_at = db.storage.snapshot_manager().max_snapshot_timestamp().unwrap();
    assert!(empty_at > at);
    assert!(db
        .storage
        .scan_table_at_snapshot("__mv_mv", empty_at)
        .unwrap()
        .is_empty());
    let error = db.storage.scan_table_at_snapshot("__mv_mv", at).unwrap_err();
    assert!(error.to_string().contains("refreshed"), "{error}");
}

#[test]
fn direct_executor_truncate_data_and_log_are_one_group_and_transaction_abort_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), false);
    seed(&db);
    db.execute("ALTER TABLE src ALTER COLUMN v SET STORAGE COLUMNAR")
        .unwrap();
    let original = rows(&db, "src");
    let txn = db.storage.begin_transaction().unwrap();
    let before = latest(&db);
    Executor::with_storage(&db.storage)
        .with_transaction(&txn)
        .execute(&LogicalPlan::Truncate {
            table_name: "src".into(),
        })
        .unwrap();
    txn.rollback().unwrap();
    assert!(groups(&db, before).is_empty(), "aborted truncate writes no rows or WAL");
    assert_eq!(rows(&db, "src"), original);
    assert!(
        db.execute("INSERT INTO src VALUES (1, 'duplicate', '', 0)").is_err(),
        "PK survived rollback"
    );
    let before = latest(&db);
    Executor::with_storage(&db.storage)
        .execute(&LogicalPlan::Truncate {
            table_name: "src".into(),
        })
        .unwrap();
    let groups = groups(&db, before);
    assert_eq!(groups.len(), 1, "{groups:#?}");
    assert!(groups[0].has("data:src:"));
    assert_eq!(
        groups[0]
            .operations
            .iter()
            .filter(|op| matches!(op, WalOperation::Truncate { table } if table == "src"))
            .count(),
        1
    );
    assert!(rows(&db, "src").is_empty());
    db.execute("INSERT INTO src VALUES (1, 'again', 'again', 99)").unwrap();
    assert_eq!(rows(&db, "src").len(), 1);
}

#[test]
fn fresh_and_changed_shape_columnar_populations_discard_orphan_presence_bits() {
    use heliosdb_nano::{storage::BatchPresence, ColumnStorageMode};
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), false);
    seed(&db);
    db.execute("ALTER TABLE src ALTER COLUMN v SET STORAGE COLUMNAR")
        .unwrap();
    let mut stale = BatchPresence::new();
    stale.set(999);
    db.storage
        .db()
        .put(b"colp:__mv_orphan:0", bincode::serialize(&stale).unwrap())
        .unwrap();
    db.storage.db().put(b"colpm:__mv_orphan", [1u8]).unwrap();
    db.execute("CREATE MATERIALIZED VIEW orphan AS SELECT * FROM src")
        .unwrap();
    // A SELECT projection has a fresh logical schema; it does not inherit the
    // source column's physical storage mode. Its fresh noncolumnar table must
    // still purge orphan presence, rather than leaving it for a later ALTER.
    assert!(db
        .storage
        .catalog()
        .get_table_schema("__mv_orphan")
        .unwrap()
        .columns
        .iter()
        .all(|column| column.storage_mode == ColumnStorageMode::Default));
    assert!(db.storage.db().get(b"colp:__mv_orphan:0").unwrap().is_none());
    assert!(db.storage.db().get(b"colpm:__mv_orphan").unwrap().is_none());
    assert_eq!(rows(&db, "orphan"), rows(&db, "src"));
    db.storage
        .db()
        .put(b"colp:__mv_orphan:0", bincode::serialize(&stale).unwrap())
        .unwrap();
    let mut column = Column::new("different", DataType::Int4);
    column.storage_mode = ColumnStorageMode::Columnar;
    let schema = Schema::new(vec![column]);
    db.storage
        .db()
        .put(b"colp:__mv_fresh:0", bincode::serialize(&stale).unwrap())
        .unwrap();
    db.storage.db().put(b"colpm:__mv_fresh", [1u8]).unwrap();
    db.storage
        .mv_catalog()
        .store_view_data(
            "fresh",
            vec![Tuple::new(vec![Value::Int4(3)]), Tuple::new(vec![Value::Int4(5)])],
            &schema,
        )
        .unwrap();
    let fresh: BatchPresence =
        bincode::deserialize(&db.storage.db().get(b"colp:__mv_fresh:0").unwrap().unwrap()).unwrap();
    assert_eq!(fresh.live_count, 2);
    assert!(!fresh.is_live(999));
    assert_eq!(
        db.storage
            .scan_table("__mv_fresh")
            .unwrap()
            .into_iter()
            .map(|row| row.values)
            .collect::<Vec<_>>(),
        vec![vec![Value::Int4(3)], vec![Value::Int4(5)]]
    );
    db.storage
        .mv_catalog()
        .store_view_data("orphan", vec![Tuple::new(vec![Value::Int4(7)])], &schema)
        .unwrap();
    let actual: BatchPresence =
        bincode::deserialize(&db.storage.db().get(b"colp:__mv_orphan:0").unwrap().unwrap()).unwrap();
    assert_eq!(actual.live_count, 1);
    assert!(!actual.is_live(999));
    assert_eq!(rows(&db, "orphan"), vec![vec![Value::Int4(7)]]);
}

#[cfg(feature = "internal-tests")]
mod internal {
    use super::*;
    use heliosdb_nano::storage::ddl_batch::debug_fail_next_commit;

    #[test]
    fn failed_mv_create_refresh_and_shape_replacement_publish_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(dir.path(), false);
        seed(&db);
        let before = latest(&db);
        debug_fail_next_commit();
        assert!(db.execute("CREATE MATERIALIZED VIEW mv AS SELECT * FROM src").is_err());
        assert!(groups(&db, before).is_empty());
        assert!(!db.storage.mv_catalog().view_exists("mv").unwrap());
        assert!(!db.storage.catalog().table_exists("__mv_mv").unwrap());
        assert!(db.storage.art_indexes().list_table_indexes("__mv_mv").is_empty());
        db.execute("CREATE MATERIALIZED VIEW mv AS SELECT * FROM src").unwrap();
        db.execute("CREATE INDEX mv_tag_idx ON __mv_mv (tag)").unwrap();
        let original = rows(&db, "mv");
        let metadata = db.storage.mv_catalog().get_view("mv").unwrap();
        db.execute("UPDATE src SET tag = 'changed'").unwrap();
        let before = latest(&db);
        debug_fail_next_commit();
        assert!(db.execute("REFRESH MATERIALIZED VIEW CONCURRENTLY mv").is_err());
        assert!(groups(&db, before).is_empty());
        assert_eq!(rows(&db, "mv"), original);
        assert_eq!(
            db.storage.mv_catalog().get_view("mv").unwrap().last_refresh,
            metadata.last_refresh
        );
        assert_eq!(
            db.query("SELECT id FROM __mv_mv WHERE tag = 'old'", &[]).unwrap().len(),
            2
        );
        let before = latest(&db);
        debug_fail_next_commit();
        let schema = Schema::new(vec![Column::new("changed", DataType::Int4)]);
        assert!(db
            .storage
            .mv_catalog()
            .store_view_data("mv", vec![Tuple::new(vec![Value::Int4(9)])], &schema)
            .is_err());
        assert!(groups(&db, before).is_empty());
        assert_eq!(rows(&db, "mv"), original);
        assert!(db.storage.art_indexes().index_exists("mv_tag_idx"));
    }

    const CHILD: &str = "HELIOS_R57_MV_CRASH_PATH";
    const STRICT: &str = "HELIOS_R57_MV_CRASH_STRICT";
    fn crash(name: &str, path: &Path, point: &str, strict: bool) {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, path)
            .env(STRICT, if strict { "1" } else { "0" })
            .env("HELIOSDB_NANO_TEST_EXIT_AFTER_DDL_WRITE", point)
            .status()
            .unwrap();
        assert!(status.success(), "crash child: {status}");
    }
    #[test]
    fn create_mv_crash_after_group_keeps_full_definition_and_rows() {
        const NAME: &str = "internal::create_mv_crash_after_group_keeps_full_definition_and_rows";
        if let Ok(path) = std::env::var(CHILD) {
            let db = open(Path::new(&path), std::env::var(STRICT).unwrap() == "1");
            db.execute("CREATE MATERIALIZED VIEW mv AS SELECT * FROM src").unwrap();
            panic!("create_mv crash point did not fire");
        }
        for strict in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            {
                let db = open(dir.path(), strict);
                seed(&db);
            }
            crash(NAME, dir.path(), "create_mv:mv", strict);
            let db = open(dir.path(), strict);
            assert_eq!(rows(&db, "mv"), rows(&db, "src"));
            assert_eq!(db.storage.mv_catalog().get_view("mv").unwrap().row_count, Some(2));
        }
    }
    #[test]
    fn indexed_columnar_refresh_crash_after_group_keeps_complete_new_image() {
        const NAME: &str = "internal::indexed_columnar_refresh_crash_after_group_keeps_complete_new_image";
        if let Ok(path) = std::env::var(CHILD) {
            let db = open(Path::new(&path), std::env::var(STRICT).unwrap() == "1");
            db.execute("REFRESH MATERIALIZED VIEW CONCURRENTLY mv").unwrap();
            panic!("refresh_mv crash point did not fire");
        }
        for strict in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            {
                let db = open(dir.path(), strict);
                seed(&db);
                db.execute("CREATE MATERIALIZED VIEW mv AS SELECT * FROM src").unwrap();
                db.execute("CREATE INDEX mv_tag_idx ON __mv_mv (tag)").unwrap();
                db.execute("ALTER TABLE __mv_mv ALTER COLUMN v SET STORAGE COLUMNAR")
                    .unwrap();
                db.execute("UPDATE src SET tag = 'new', v = v + 1").unwrap();
            }
            crash(NAME, dir.path(), "refresh_mv:mv", strict);
            let db = open(dir.path(), strict);
            assert_eq!(rows(&db, "mv"), rows(&db, "src"));
            assert!(db.storage.art_indexes().index_exists("mv_tag_idx"));
            assert_eq!(
                db.query("SELECT id FROM __mv_mv WHERE tag = 'new'", &[]).unwrap().len(),
                2
            );
        }
    }
}

#[test]
fn borrowed_executor_truncate_invalidates_warm_results_without_refreshing_repeatable_read() {
    use heliosdb_nano::session::IsolationLevel;
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), false);
    seed(&db);
    let sql = "SELECT id, tag FROM src WHERE v IS NOT NULL";
    for _ in 0..10 {
        assert_eq!(db.query(sql, &[]).unwrap().len(), 2);
    }
    let reader = db.create_session("rr", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(reader).unwrap();
    assert_eq!(
        db.query_in_session(reader, "SELECT id FROM src ORDER BY id", &[])
            .unwrap()
            .len(),
        2
    );
    Executor::with_storage(db.storage.as_ref())
        .execute(&LogicalPlan::Truncate {
            table_name: "src".into(),
        })
        .unwrap();
    assert!(
        db.query(sql, &[]).unwrap().is_empty(),
        "direct executor invalidates the previously warm result"
    );
    let old = db
        .query_in_session(reader, "SELECT id FROM src ORDER BY id", &[])
        .unwrap();
    assert_eq!(
        old.iter().map(|row| row.values.clone()).collect::<Vec<_>>(),
        vec![vec![Value::Int4(1)], vec![Value::Int4(2)]]
    );
    db.commit_transaction_for_session(reader).unwrap();
}

#[cfg(feature = "internal-tests")]
#[test]
fn direct_truncate_applied_error_keeps_actual_rows_indexes_and_cache_fenced() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), false);
    seed(&db);
    let sql = "SELECT id, tag FROM src WHERE v IS NOT NULL";
    for _ in 0..10 {
        assert_eq!(db.query(sql, &[]).unwrap().len(), 2);
    }
    db.storage.debug_fail_next_applied_commit();
    let error = Executor::with_storage(db.storage.as_ref())
        .execute(&LogicalPlan::Truncate {
            table_name: "src".into(),
        })
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected durability failure after the commit applied"),
        "{error}"
    );
    assert!(db.query(sql, &[]).unwrap().is_empty());
    assert!(db.query("SELECT id FROM src WHERE id = 1", &[]).unwrap().is_empty());
    db.execute("INSERT INTO src VALUES (1, 'replacement', 'replacement', 99)")
        .unwrap();
    assert_eq!(db.query("SELECT id FROM src WHERE id = 1", &[]).unwrap().len(), 1);
}

#[cfg(feature = "internal-tests")]
#[test]
fn unreadable_truncate_rollback_retires_indexes_and_quarantines_writes_until_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), false);
    seed(&db);
    let raw = db.storage.db();
    let before = raw.get(b"data:src:1").unwrap().unwrap();
    let txn = db.storage.begin_transaction().unwrap();
    Executor::with_storage(db.storage.as_ref())
        .with_transaction(&txn)
        .execute(&LogicalPlan::Truncate {
            table_name: "src".into(),
        })
        .unwrap();
    raw.put(b"data:src:1", [0xff]).unwrap();
    txn.rollback().unwrap();
    assert!(
        db.storage.art_indexes().list_table_indexes("src").is_empty(),
        "unreliable indexes cannot give authoritative empty answers"
    );
    let error = db.execute("INSERT INTO src VALUES (3, 'blocked', '', 0)").unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unavailable after index reconciliation failed"),
        "{error}"
    );
    raw.put(b"data:src:1", before).unwrap();
    drop(raw);
    drop(db);
    let reopened = open(dir.path(), false);
    assert_eq!(reopened.query("SELECT id FROM src WHERE id = 1", &[]).unwrap().len(), 1);
    reopened
        .execute("INSERT INTO src VALUES (3, 'allowed', '', 0)")
        .unwrap();
}

#[test]
fn compatible_mv_refresh_preserves_repeatable_read_and_invalidates_warm_results() {
    use heliosdb_nano::session::IsolationLevel;
    for params in [false, true] {
        for concurrent in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let db = open(dir.path(), false);
            seed(&db);
            run(&db, params, "CREATE MATERIALIZED VIEW mv AS SELECT * FROM src");
            let before = rows(&db, "mv");
            let reader = db.create_session("mv_rr", IsolationLevel::RepeatableRead).unwrap();
            db.begin_transaction_for_session(reader).unwrap();
            let old = db
                .query_in_session(reader, "SELECT id,tag,body,v FROM mv ORDER BY id", &[])
                .unwrap();
            assert_eq!(old.iter().map(|row| row.values.clone()).collect::<Vec<_>>(), before);
            run(&db, params, "UPDATE src SET tag = 'fresh', v = v + 100");
            let sql = "SELECT id,tag,body,v FROM mv WHERE v IS NOT NULL ORDER BY id";
            for _ in 0..10 {
                assert_eq!(
                    db.query(sql, &[])
                        .unwrap()
                        .iter()
                        .map(|row| row.values.clone())
                        .collect::<Vec<_>>(),
                    before
                );
            }
            run(
                &db,
                params,
                if concurrent {
                    "REFRESH MATERIALIZED VIEW CONCURRENTLY mv"
                } else {
                    "REFRESH MATERIALIZED VIEW mv"
                },
            );
            let after = db
                .query(sql, &[])
                .unwrap()
                .into_iter()
                .map(|row| row.values)
                .collect::<Vec<_>>();
            assert_eq!(
                after,
                vec![
                    vec![
                        Value::Int4(1),
                        Value::String("fresh".into()),
                        Value::String("body1".into()),
                        Value::Int4(108)
                    ],
                    vec![
                        Value::Int4(2),
                        Value::String("fresh".into()),
                        Value::String("body2".into()),
                        Value::Int4(116)
                    ]
                ]
            );
            let old = db
                .query_in_session(reader, "SELECT id,tag,body,v FROM mv ORDER BY id", &[])
                .unwrap();
            assert_eq!(
                old.into_iter().map(|row| row.values).collect::<Vec<_>>(),
                before,
                "reader keeps pre-refresh MV rows"
            );
            db.commit_transaction_for_session(reader).unwrap();
        }
    }
}

#[test]
fn compatible_store_view_data_preserves_repeatable_read_and_invalidates_warm_results() {
    use heliosdb_nano::session::IsolationLevel;
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), false);
    seed(&db);
    db.execute("CREATE MATERIALIZED VIEW mv AS SELECT * FROM src").unwrap();
    let reader = db
        .create_session("mv_store_rr", IsolationLevel::RepeatableRead)
        .unwrap();
    db.begin_transaction_for_session(reader).unwrap();
    let before = db
        .query_in_session(reader, "SELECT id,tag,body,v FROM mv ORDER BY id", &[])
        .unwrap();
    let sql = "SELECT id,tag,body,v FROM mv WHERE v IS NOT NULL ORDER BY id";
    for _ in 0..10 {
        assert_eq!(db.query(sql, &[]).unwrap().len(), 2);
    }
    let schema = db.storage.catalog().get_table_schema("__mv_mv").unwrap();
    db.storage
        .mv_catalog()
        .store_view_data(
            "mv",
            vec![Tuple::new(vec![
                Value::Int4(7),
                Value::String("stored".into()),
                Value::String("payload".into()),
                Value::Int4(99),
            ])],
            &schema,
        )
        .unwrap();
    assert_eq!(
        db.query(sql, &[]).unwrap()[0].values,
        vec![
            Value::Int4(7),
            Value::String("stored".into()),
            Value::String("payload".into()),
            Value::Int4(99)
        ]
    );
    let old = db
        .query_in_session(reader, "SELECT id,tag,body,v FROM mv ORDER BY id", &[])
        .unwrap();
    assert_eq!(
        old.into_iter().map(|row| row.values).collect::<Vec<_>>(),
        before.into_iter().map(|row| row.values).collect::<Vec<_>>()
    );
    db.commit_transaction_for_session(reader).unwrap();
}
