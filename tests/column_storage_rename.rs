//! A column rename moves representation bytes atomically and preserves logical
//! fork/history values. SQL comparisons intentionally ignore Tuple.row_id metadata.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{Config, EmbeddedDatabase, Tuple, Value};
use rocksdb::{IteratorMode, ReadOptions, WriteBatchIterator};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
enum Family {
    Text,
    Params,
}

const FAMILIES: [Family; 2] = [Family::Text, Family::Params];

fn execute(db: &EmbeddedDatabase, family: Family, sql: &str) -> heliosdb_nano::Result<u64> {
    match family {
        Family::Text => db.execute(sql),
        Family::Params => db.execute_params(sql, &[]),
    }
}

fn run(db: &EmbeddedDatabase, family: Family, sql: &str) {
    execute(db, family, sql).unwrap_or_else(|error| panic!("{family:?}: {sql}: {error}"));
}

// Branch lifecycle is a text-only setup surface. The schema/storage ALTER,
// mutation, historical query and branch read probes still run on both families.
fn run_branch_setup(db: &EmbeddedDatabase, sql: &str) {
    db.execute(sql)
        .unwrap_or_else(|error| panic!("branch setup: {sql}: {error}"));
}

fn values(db: &EmbeddedDatabase, family: Family, sql: &str) -> Vec<Vec<Value>> {
    match family {
        Family::Text => db.query(sql, &[]),
        Family::Params => db.query_params(sql, &[]),
    }
    .unwrap_or_else(|error| panic!("{family:?}: {sql}: {error}"))
    .into_iter()
    .map(|row| row.values)
    .collect()
}

fn assert_live_child(db: &EmbeddedDatabase, family: Family, phase: &str) {
    db.with_request_branch_scope("child", || {
        for sql in [
            "SELECT id,a,b,c,d FROM s",
            "SELECT id,a,b,c,d FROM s WHERE id=1",
            "SELECT id,a,b,c,d FROM s WHERE a='left'",
        ] {
            let rows = match family {
                Family::Text => db.query(sql, &[]),
                Family::Params => db.query_params(sql, &[]),
            }
            .unwrap_or_else(|error| panic!("{phase} {family:?} {sql}: {error}"));
            assert_eq!(
                rows.into_iter().map(|row| row.values).collect::<Vec<_>>(),
                original(),
                "{phase} {family:?} {sql}"
            );
        }
        let schema = db.storage.catalog().get_table_schema("s")?;
        let view = db.storage.branch_view()?.expect("request branch is bound");
        // Main has no index on old name a after the rename: exercise the key
        // scan fallback, separately from SQL's own scan/PK routes.
        for _ in 0..2 {
            let rows = view.rows_with_key(None, "s", &schema, &["a".to_string()], &[Value::String("left".into())])?;
            assert_eq!(
                rows.into_iter().map(|row| row.values).collect::<Vec<_>>(),
                original(),
                "{phase}: key scan fallback"
            );
        }
        let row = view.get(None, "s", 1, &schema)?.expect("copied row exists");
        assert_eq!(vec![row.values], original(), "{phase}: public point read");
        Ok(())
    })
    .unwrap_or_else(|error| panic!("{phase} {family:?} child: {error}"));
}

fn config(dir: &Path, history: bool) -> Config {
    let mut config = Config::default();
    config.storage.path = Some(dir.to_path_buf());
    config.storage.memory_only = false;
    config.storage.time_travel_enabled = history;
    config.storage.wal_enabled = true;
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    config.replication.physical.enabled = true;
    config
}

fn open(dir: &Path, history: bool) -> EmbeddedDatabase {
    let mut last = None;
    for _ in 0..100 {
        match EmbeddedDatabase::with_config(config(dir, history)) {
            Ok(db) => return db,
            Err(error) => last = Some(error),
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("reopening {} failed: {last:?}", dir.display());
}

fn setup(db: &EmbeddedDatabase, family: Family) {
    run(
        db,
        family,
        "CREATE TABLE s (id INT PRIMARY KEY, a TEXT STORAGE DICTIONARY, b TEXT STORAGE DICTIONARY, \
         c BIGINT STORAGE COLUMNAR, d BIGINT STORAGE COLUMNAR)",
    );
    db.storage
        .insert_tuple_versioned(
            "s",
            Tuple::new(vec![
                Value::Int4(1),
                Value::String("left".into()),
                Value::String("right".into()),
                Value::Int8(10),
                Value::Int8(20),
            ]),
        )
        .unwrap();
    let raw = db.storage.db().get(b"data:s:1").unwrap().unwrap();
    let row: Tuple = bincode::deserialize(&raw).unwrap();
    assert!(matches!(row.values[1], Value::DictRef { .. }));
    assert!(matches!(row.values[2], Value::DictRef { .. }));
    assert!(matches!(row.values[3], Value::ColumnarRef));
    assert!(matches!(row.values[4], Value::ColumnarRef));
    assert_eq!(values(db, family, "SELECT id, a, b, c, d FROM s"), original());
}

fn original() -> Vec<Vec<Value>> {
    vec![vec![
        Value::Int4(1),
        Value::String("left".into()),
        Value::String("right".into()),
        Value::Int8(10),
        Value::Int8(20),
    ]]
}

fn image(db: &EmbeddedDatabase, prefixes: &[&str]) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let raw = db.storage.db();
    let mut options = ReadOptions::default();
    options.set_total_order_seek(true);
    raw.iterator_opt(IteratorMode::Start, options)
        .map(|entry| {
            let (key, value) = entry.unwrap();
            (key.to_vec(), value.to_vec())
        })
        .filter(|(key, _)| prefixes.iter().any(|prefix| key.starts_with(prefix.as_bytes())))
        .collect()
}

#[derive(Default)]
struct Keys(Vec<Vec<u8>>);

impl WriteBatchIterator for Keys {
    fn put(&mut self, key: Box<[u8]>, _: Box<[u8]>) {
        self.0.push(key.to_vec());
    }
    fn delete(&mut self, key: Box<[u8]>) {
        self.0.push(key.to_vec());
    }
}

fn assert_one_rename_group(db: &EmbeddedDatabase, before: u64, data_expected: bool) {
    let raw = db.storage.db();
    let mut batches = raw.get_updates_since(before).unwrap();
    let groups: Vec<_> = batches
        .by_ref()
        .map(|entry| {
            let (_, batch) = entry.unwrap();
            let mut keys = Keys::default();
            batch.iterate(&mut keys);
            keys.0
        })
        .collect();
    batches.status().unwrap();
    let visible: Vec<_> = groups
        .iter()
        .filter(|keys| {
            keys.iter().any(|key| {
                [
                    b"meta:table:s".as_slice(),
                    b"dict:s:",
                    b"col:s:",
                    b"colz:s:",
                    b"colzm:s:",
                    b"data:s:",
                ]
                .iter()
                .any(|prefix| key.starts_with(prefix))
            })
        })
        .collect();
    assert_eq!(
        visible.len(),
        1,
        "rename metadata/sidecars/rows split across groups: {visible:?}"
    );
    let keys = visible[0];
    for prefix in [b"meta:table:s".as_slice(), b"dict:s:", b"col:s:"] {
        assert!(
            keys.iter().any(|key| key.starts_with(prefix)),
            "missing {prefix:?} from {keys:?}"
        );
    }
    assert_eq!(keys.iter().any(|key| key.starts_with(b"data:s:")), data_expected);
}

#[test]
fn rename_swaps_read_all_original_sidecars_once_and_preserve_metadata_only_fast_path() {
    for family in FAMILIES {
        for history in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            {
                let db = open(dir.path(), history);
                setup(&db, family);
                run(&db, family, "CREATE UNIQUE INDEX s_a ON s (a)");
                run(&db, family, "CREATE INDEX s_c ON s (c)");
                let before_rows = image(&db, &["data:s:"]);
                let before = db.storage.db().latest_sequence_number();
                run(
                    &db,
                    family,
                    "ALTER TABLE s RENAME COLUMN a TO temporary_a, RENAME COLUMN b TO a, \
                     RENAME COLUMN temporary_a TO b, RENAME COLUMN c TO temporary_c, \
                     RENAME COLUMN d TO c, RENAME COLUMN temporary_c TO d",
                );
                assert_one_rename_group(&db, before, false);
                assert_eq!(
                    image(&db, &["data:s:"]),
                    before_rows,
                    "no child: main rows stay untouched"
                );
                let swapped = vec![vec![
                    Value::Int4(1),
                    Value::String("right".into()),
                    Value::String("left".into()),
                    Value::Int8(20),
                    Value::Int8(10),
                ]];
                assert_eq!(values(&db, family, "SELECT id, a, b, c, d FROM s"), swapped);
                assert_eq!(
                    values(&db, family, "SELECT id FROM s WHERE b = 'left' AND d = 10"),
                    vec![vec![Value::Int4(1)]]
                );
                assert!(db.storage.art_indexes().find_column_index("s", "b").is_some());
                assert!(db.storage.art_indexes().find_column_index("s", "d").is_some());
                assert!(execute(
                    &db,
                    family,
                    "INSERT INTO s (id,a,b,c,d) VALUES (2, 'fresh', 'left', 200, 100)"
                )
                .is_err());
                // A value encoded after the move must not reuse an old ID.
                run(
                    &db,
                    family,
                    "INSERT INTO s (id,a,b,c,d) VALUES (2, 'new-right', 'new-left', 200, 100)",
                );
            }
            let db = open(dir.path(), history);
            assert_eq!(
                values(&db, family, "SELECT id, a, b, c, d FROM s ORDER BY id"),
                vec![
                    vec![
                        Value::Int4(1),
                        Value::String("right".into()),
                        Value::String("left".into()),
                        Value::Int8(20),
                        Value::Int8(10)
                    ],
                    vec![
                        Value::Int4(2),
                        Value::String("new-right".into()),
                        Value::String("new-left".into()),
                        Value::Int8(200),
                        Value::Int8(100)
                    ],
                ]
            );
            assert!(execute(
                &db,
                family,
                "INSERT INTO s (id,a,b,c,d) VALUES (3, 'unused', 'left', 300, 300)"
            )
            .is_err());
        }
    }
}

#[test]
fn drop_then_rename_into_the_dropped_sidecar_name_keeps_the_surviving_column() {
    for family in FAMILIES {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = open(dir.path(), true);
            setup(&db, family);
            run(
                &db,
                family,
                "ALTER TABLE s DROP COLUMN c, RENAME COLUMN d TO c, DROP COLUMN a, RENAME COLUMN b TO a",
            );
            assert_eq!(
                values(&db, family, "SELECT id, a, c FROM s"),
                vec![vec![Value::Int4(1), Value::String("right".into()), Value::Int8(20)]]
            );
            run(&db, family, "INSERT INTO s VALUES (2, 'after', 30)");
        }
        let db = open(dir.path(), true);
        assert_eq!(
            values(&db, family, "SELECT id, a, c FROM s ORDER BY id"),
            vec![
                vec![Value::Int4(1), Value::String("right".into()), Value::Int8(20)],
                vec![Value::Int4(2), Value::String("after".into()), Value::Int8(30)],
            ]
        );
    }
}

#[test]
fn dropping_a_column_preserves_another_quoted_column_sharing_its_sidecar_prefix() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        run(
            &db,
            family,
            "CREATE TABLE quoted (id INT PRIMARY KEY, c BIGINT STORAGE COLUMNAR, \"c:other\" BIGINT STORAGE COLUMNAR)",
        );
        db.storage
            .insert_tuple_versioned(
                "quoted",
                Tuple::new(vec![Value::Int4(1), Value::Int8(10), Value::Int8(20)]),
            )
            .unwrap();
        run(&db, family, "ALTER TABLE quoted DROP COLUMN c");
        assert_eq!(
            values(&db, family, "SELECT id, \"c:other\" FROM quoted"),
            vec![vec![Value::Int4(1), Value::Int8(20)]]
        );
    }
}

#[test]
fn live_children_and_later_past_anchors_keep_logical_values_after_sidecar_renames() {
    for family in FAMILIES {
        for history in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let anchor;
            {
                let db = open(dir.path(), history);
                setup(&db, family);
                anchor = db.storage.snapshot_manager().current_scn().saturating_sub(1);
                run_branch_setup(&db, "CREATE BRANCH child FROM main");
                let before = db.storage.db().latest_sequence_number();
                run(
                    &db,
                    family,
                    "ALTER TABLE s RENAME COLUMN a TO aa, RENAME COLUMN c TO cc",
                );
                assert_one_rename_group(&db, before, true);
                // Freeze copied rows must be self-contained, not old physical refs.
                let copies = image(&db, &["bdata:"]);
                assert_eq!(copies.len(), 1, "the live child's physical row was frozen");
                for (_, bytes) in copies {
                    let row: Tuple = bincode::deserialize(&bytes).unwrap();
                    assert!(row.values.iter().all(|value| !matches!(
                        value,
                        Value::DictRef { .. } | Value::CasRef { .. } | Value::ColumnarRef
                    )));
                }
                assert_live_child(&db, family, "immediately after rename");
                run(&db, family, "UPDATE s SET aa = 'changed', cc = 900 WHERE id = 1");
                assert_live_child(&db, family, "after later main mutation");
                if history {
                    assert_eq!(
                        values(
                            &db,
                            family,
                            &format!("SELECT id, aa, b, cc, d FROM s AS OF SCN {anchor}")
                        ),
                        original()
                    );
                    run_branch_setup(&db, &format!("CREATE BRANCH past FROM main AS OF SCN {anchor}"));
                    let past = db
                        .with_request_branch_scope("past", || Ok(values(&db, family, "SELECT id, aa, b, cc, d FROM s")))
                        .unwrap();
                    assert_eq!(past, original());
                }
            }
            let db = open(dir.path(), history);
            assert_live_child(&db, family, "after reopen");
            if history {
                assert_eq!(
                    values(
                        &db,
                        family,
                        &format!("SELECT id, aa, b, cc, d FROM s AS OF SCN {anchor}")
                    ),
                    original()
                );
                let past = db
                    .with_request_branch_scope("past", || Ok(values(&db, family, "SELECT id, aa, b, cc, d FROM s")))
                    .unwrap();
                assert_eq!(past, original());
            }
        }
    }
}

#[test]
fn an_undecided_corrupt_main_row_still_fails_branch_scan_key_fallback_and_point_reads() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        setup(&db, family);
        run_branch_setup(&db, "CREATE BRANCH untouched FROM main");
        let raw = db.storage.db();
        let original = raw.get(b"data:s:1").unwrap().unwrap();
        // Deliberately bypass the write gate: model unreadable existing bytes,
        // with no legitimate B4 copy/tombstone that could decide this row.
        raw.put(b"data:s:1", b"corrupt undecided row").unwrap();
        db.with_request_branch_scope("untouched", || {
            let result = match family {
                Family::Text => db.query("SELECT * FROM s", &[]),
                Family::Params => db.query_params("SELECT * FROM s", &[]),
            };
            assert!(
                result.is_err(),
                "undecided decode failure is not hidden by a scan retry"
            );
            let schema = db.storage.catalog().get_table_schema("s")?;
            let view = db.storage.branch_view()?.expect("branch is bound");
            assert!(view.scan(None, "s", &schema).is_err());
            assert!(view
                .rows_with_key(None, "s", &schema, &["a".into()], &[Value::String("left".into())])
                .is_err());
            assert!(view.get(None, "s", 1, &schema).is_err());
            Ok(())
        })
        .unwrap();
        raw.put(b"data:s:1", original).unwrap();
    }
}

fn capped(children: bool) -> EmbeddedDatabase {
    let mut config = Config::in_memory();
    config.storage.max_atomic_rewrite_bytes = 1024 * 1024;
    let db = EmbeddedDatabase::with_config(config).unwrap();
    db.execute("CREATE TABLE cap (id INT PRIMARY KEY, d TEXT STORAGE DICTIONARY, x TEXT STORAGE CONTENT_ADDRESSED, c BIGINT STORAGE COLUMNAR)").unwrap();
    for id in 1..=60 {
        db.storage
            .insert_tuple_versioned(
                "cap",
                Tuple::new(vec![
                    Value::Int4(id),
                    Value::String("d".repeat(3500)),
                    Value::String("x".repeat(3500)),
                    Value::Int8(i64::from(id)),
                ]),
            )
            .unwrap();
    }
    if children {
        db.execute("CREATE BRANCH child_a FROM main").unwrap();
        db.execute("CREATE BRANCH child_b FROM main").unwrap();
    }
    db
}

#[test]
fn stored_rename_counts_expanded_frozen_copies_and_refusal_keeps_source_and_children() {
    for family in FAMILIES {
        let control = capped(false);
        run(&control, family, "ALTER TABLE cap RENAME COLUMN d TO renamed");
        let db = capped(true);
        let before = values(&db, family, "SELECT * FROM cap ORDER BY id");
        let before_source = image(
            &db,
            &[
                "data:cap:",
                "dict:cap:",
                "col:cap:",
                "colz:cap:",
                "colzm:cap:",
                "meta:table:cap",
            ],
        );
        let error = execute(&db, family, "ALTER TABLE cap RENAME COLUMN d TO renamed")
            .expect_err("expanded logical child copies exceed the cap");
        assert!(
            error
                .to_string()
                .contains(heliosdb_nano::ATOMIC_REWRITE_TOO_LARGE_MARKER),
            "{error}"
        );
        assert_eq!(
            image(
                &db,
                &[
                    "data:cap:",
                    "dict:cap:",
                    "col:cap:",
                    "colz:cap:",
                    "colzm:cap:",
                    "meta:table:cap"
                ]
            ),
            before_source
        );
        assert_eq!(values(&db, family, "SELECT * FROM cap ORDER BY id"), before);
        for child in ["child_a", "child_b"] {
            let rows = db
                .with_request_branch_scope(child, || Ok(values(&db, family, "SELECT * FROM cap ORDER BY id")))
                .unwrap();
            assert_eq!(rows, before);
        }
    }
}

#[cfg(feature = "internal-tests")]
mod internal {
    use super::*;
    use heliosdb_nano::storage::ddl_batch::{debug_fail_next_applied_commit, debug_fail_next_commit};

    #[test]
    fn renamed_storage_and_new_unique_index_follow_actual_application_on_error() {
        for family in FAMILIES {
            for applied in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                {
                    let db = open(dir.path(), true);
                    setup(&db, family);
                    run_branch_setup(&db, "CREATE BRANCH child FROM main");
                    let sql =
                        "ALTER TABLE s RENAME COLUMN a TO aa, RENAME COLUMN c TO cc, ADD CONSTRAINT uq_aa UNIQUE (aa)";
                    if applied {
                        debug_fail_next_applied_commit();
                    } else {
                        debug_fail_next_commit();
                    }
                    assert!(
                        execute(&db, family, sql).is_err(),
                        "the injected error must be returned"
                    );
                    let select = if applied {
                        "SELECT id, aa, b, cc, d FROM s"
                    } else {
                        "SELECT id, a, b, c, d FROM s"
                    };
                    assert_eq!(values(&db, family, select), original());
                    let child = db
                        .with_request_branch_scope("child", || Ok(values(&db, family, "SELECT id, a, b, c, d FROM s")))
                        .unwrap();
                    assert_eq!(child, original());
                    assert_eq!(db.storage.art_indexes().find_column_index("s", "aa").is_some(), applied);
                    assert_eq!(
                        values(&db, family, "SELECT COUNT(*) FROM s"),
                        vec![vec![Value::Int8(1)]]
                    );
                    if applied {
                        assert_eq!(
                            values(&db, family, "SELECT id FROM s WHERE aa = 'left'"),
                            vec![vec![Value::Int4(1)]]
                        );
                        assert!(
                            execute(&db, family, "INSERT INTO s VALUES (2, 'left', 'unused', 99, 100)").is_err(),
                            "durable new UNIQUE tree must remain enforced after Err"
                        );
                    } else {
                        run(&db, family, "INSERT INTO s VALUES (2, 'left', 'unused', 99, 100)");
                    }
                }
                let db = open(dir.path(), true);
                let expected_count = if applied { 1 } else { 2 };
                assert_eq!(
                    values(&db, family, "SELECT COUNT(*) FROM s"),
                    vec![vec![Value::Int8(expected_count)]]
                );
                if applied {
                    assert_eq!(values(&db, family, "SELECT id, aa, b, cc, d FROM s"), original());
                    assert!(execute(&db, family, "INSERT INTO s VALUES (3, 'left', 'unused', 99, 100)").is_err());
                }
            }
        }
    }

    #[test]
    fn unreadable_application_witness_quarantines_only_its_table_until_reopen() {
        use heliosdb_nano::storage::ddl_batch::debug_fail_next_applied_commit_with;
        use std::sync::{Arc, Mutex};

        for family in FAMILIES {
            let dir = tempfile::tempdir().unwrap();
            {
                let db = open(dir.path(), true);
                setup(&db, family);
                run(&db, family, "CREATE TABLE healthy (id INT PRIMARY KEY)");
                let raw = db.storage.db();
                let saved = Arc::new(Mutex::new(None));
                let captured = Arc::clone(&saved);
                let corrupt = Arc::clone(&raw);
                debug_fail_next_applied_commit_with(move || {
                    *captured.lock().unwrap() = corrupt.get(b"meta:table_shape:s").unwrap();
                    corrupt.put(b"meta:table_shape:s", b"unreadable witness").unwrap();
                });
                assert!(execute(
                    &db,
                    family,
                    "ALTER TABLE s RENAME COLUMN a TO aa, ADD CONSTRAINT uq_aa UNIQUE (aa)"
                )
                .is_err());
                assert!(
                    db.query("SELECT * FROM s", &[]).is_err(),
                    "uncertain rows must not be admitted through missing trees"
                );
                assert!(db.storage.art_indexes().list_table_indexes("s").is_empty());
                let data = image(&db, &["data:s:"]);
                // Both public direct INSERT and SQL INSERT must retain the table
                // fence after its enforcement trees have been retired.
                assert!(db
                    .storage
                    .insert_tuple_versioned(
                        "s",
                        Tuple::new(vec![
                            Value::Int4(2),
                            Value::String("left".into()),
                            Value::String("other".into()),
                            Value::Int8(99),
                            Value::Int8(100),
                        ])
                    )
                    .is_err());
                assert!(execute(&db, family, "INSERT INTO s VALUES (2, 'left', 'other', 99, 100)").is_err());
                assert_eq!(image(&db, &["data:s:"]), data);
                run(&db, family, "INSERT INTO healthy VALUES (7)");
                assert_eq!(
                    values(&db, family, "SELECT id FROM healthy"),
                    vec![vec![Value::Int4(7)]]
                );
                // Repair only the injected corruption. A live process stays
                // fenced; reopening rebuilds the actual committed definition.
                let shape = saved
                    .lock()
                    .unwrap()
                    .take()
                    .expect("the applied group wrote its witness");
                raw.put(b"meta:table_shape:s", shape).unwrap();
                assert!(db.query("SELECT * FROM s", &[]).is_err());
            }
            let db = open(dir.path(), true);
            assert_eq!(values(&db, family, "SELECT id, aa, b, c, d FROM s"), original());
            assert!(
                execute(&db, family, "INSERT INTO s VALUES (2, 'left', 'other', 99, 100)").is_err(),
                "reopen restores durable UNIQUE enforcement"
            );
        }
    }
}
