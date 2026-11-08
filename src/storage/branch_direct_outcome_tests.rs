//! Direct public branch writers settle cached overlays even when the atomic
//! native write reports an error. This module exercises real owning groups.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

use crate::storage::{branch_row_key, BranchView};
use crate::{Config, EmbeddedDatabase, Result, Schema, Tuple, Value};
use rocksdb::{IteratorMode, WriteBatchIterator};

#[derive(Clone, Copy, Debug)]
enum Change {
    Insert,
    Move,
    Delete,
    Truncate,
}
const CHANGES: [Change; 4] = [Change::Insert, Change::Move, Change::Delete, Change::Truncate];

#[derive(Clone, Copy, Debug)]
enum Fault {
    BeforeApply,
    AfterApply,
    Cap,
}

fn open(path: &Path, history: bool) -> EmbeddedDatabase {
    let mut config = Config::default();
    config.storage.path = Some(path.into());
    config.storage.memory_only = false;
    config.storage.time_travel_enabled = history;
    config.storage.version_gc_interval_secs = Some(0);
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    EmbeddedDatabase::with_config(config).unwrap()
}

fn values(id: i32, unique: i32, payload: &str) -> Vec<Value> {
    vec![Value::Int4(id), Value::Int4(unique), Value::String(payload.into())]
}

fn original() -> Vec<Vec<Value>> {
    vec![values(1, 10, "branch-one"), values(2, 20, "branch-two")]
}

fn changed(change: Change) -> Vec<Vec<Value>> {
    match change {
        Change::Insert => vec![
            values(1, 10, "branch-one"),
            values(2, 20, "branch-two"),
            values(3, 30, "branch-new"),
        ],
        Change::Move => vec![values(2, 20, "branch-two"), values(11, 110, "branch-moved")],
        Change::Delete => vec![values(2, 20, "branch-two")],
        Change::Truncate => Vec::new(),
    }
}

fn integer(value: &Value) -> i64 {
    match value {
        Value::Int4(value) => i64::from(*value),
        Value::Int8(value) => *value,
        other => panic!("expected integer, got {other:?}"),
    }
}

fn ordered(rows: impl IntoIterator<Item = Tuple>) -> Vec<Vec<Value>> {
    let mut rows: Vec<_> = rows.into_iter().map(|row| row.values).collect();
    rows.sort_by_key(|row| integer(row.first().unwrap()));
    rows
}

fn sql_rows(db: &EmbeddedDatabase, branch: &str, sql: &str) -> Vec<Vec<Value>> {
    db.with_request_branch_scope(branch, || db.query(sql, &[]).map(ordered))
        .unwrap()
}

fn setup(db: &EmbeddedDatabase, child: bool) -> (u64, Option<u64>, Schema) {
    db.execute("CREATE TABLE s (id INT PRIMARY KEY, u INT UNIQUE, payload TEXT)")
        .unwrap();
    db.execute("INSERT INTO s VALUES (100,1000,'main-only')").unwrap();
    let branch = db.storage.create_branch("b", Some("main"), Default::default()).unwrap();
    db.with_request_branch_scope("b", || -> Result<()> {
        db.execute("TRUNCATE TABLE s")?;
        db.execute("INSERT INTO s VALUES (1,10,'branch-one'),(2,20,'branch-two')")?;
        Ok(())
    })
    .unwrap();
    let schema = db
        .with_request_branch_scope("b", || db.storage.catalog().get_table_schema("s"))
        .unwrap();
    let child_id = child.then(|| {
        db.storage
            .create_branch("child", Some("b"), Default::default())
            .unwrap()
    });
    let view = BranchView::for_chain(&db.storage, "b", vec![branch]).unwrap();
    assert_truth(&view, &schema, &original());
    // Warm the decoded overlay before the fault. The two rows fit the normal
    // committed-overlay cache budget. Indexed probes separately verify durable
    // primary/unique key entries; payload probes exercise its memoized key map.
    for column in ["id", "u"] {
        let key = if column == "id" { 1 } else { 10 };
        assert_eq!(
            ordered(
                view.rows_with_key(None, "s", &schema, &[column.into()], &[Value::Int4(key)])
                    .unwrap()
            ),
            vec![values(1, 10, "branch-one")]
        );
    }
    if let Some(child) = child_id {
        let child = BranchView::for_chain(&db.storage, "child", vec![branch, child]).unwrap();
        assert_truth(&child, &schema, &original());
    }
    // Warm user-visible query/PK paths too; the direct API does not return
    // through a SQL commit owner that could compensate for its missed bump.
    assert_eq!(sql_rows(db, "b", "SELECT id,u,payload FROM s ORDER BY id"), original());
    assert_eq!(
        sql_rows(db, "b", "SELECT id,u,payload FROM s WHERE id=1"),
        vec![original().first().unwrap().clone()]
    );
    (branch, child_id, schema)
}

fn assert_truth(view: &BranchView<'_>, schema: &Schema, expected: &[Vec<Value>]) {
    // Point reads bypass the overlay memo, while scan/key probes consume it.
    // Comparing all three catches a stale old memo over a newly durable row.
    let mut row_ids = std::collections::BTreeSet::new();
    for level in view.chain() {
        // Use actual raw branch row/tombstone IDs, independently of both the
        // cached scan and the user PK (which a key-moving UPDATE changes).
        row_ids.extend(view.engine.branch_level_row_ids(*level, "s").unwrap());
        row_ids.extend(view.engine.branch_tombstone_row_ids(*level, "s").unwrap());
    }
    let expected_points: Vec<_> = row_ids
        .into_iter()
        .filter_map(|row_id| view.get(None, "s", row_id, schema).unwrap())
        .collect();
    assert_eq!(ordered(expected_points), expected, "point truth");
    assert_eq!(
        ordered(view.scan(None, "s", schema).unwrap()),
        expected,
        "cached scan truth"
    );
    for (column, keys) in [("id", vec![1, 2, 3, 11]), ("u", vec![10, 20, 30, 110])] {
        let position = usize::from(column == "u");
        for key in keys {
            let wanted: Vec<_> = expected
                .iter()
                .filter(|row| integer(row.get(position).unwrap()) == i64::from(key))
                .cloned()
                .collect();
            assert_eq!(
                ordered(
                    view.rows_with_key(None, "s", schema, &[column.into()], &[Value::Int4(key)])
                        .unwrap()
                ),
                wanted,
                "{column}={key}: durable branch key index and cached key map agree"
            );
        }
    }
    for payload in ["branch-one", "branch-two", "branch-new", "branch-moved", "absent"] {
        let wanted: Vec<_> = expected
            .iter()
            .filter(|row| row.get(2) == Some(&Value::String(payload.into())))
            .cloned()
            .collect();
        assert_eq!(
            ordered(
                view.rows_with_key(None, "s", schema, &["payload".into()], &[Value::String(payload.into())])
                    .unwrap()
            ),
            wanted,
            "non-index payload probe must revalidate the cached overlay"
        );
    }
}

fn mutate(db: &EmbeddedDatabase, branch: u64, schema: &Schema, change: Change) -> Result<()> {
    db.with_request_branch_scope("b", || {
        // Keep the public direct writer's table lease until its cache has
        // settled. insert_tuple_branch_aware takes its own matching lease.
        let _lease = db.storage.lock_table_for_write("s")?;
        let view = BranchView::for_chain(&db.storage, "b", vec![branch])?;
        let target = if matches!(change, Change::Move | Change::Delete) {
            view.rows_with_key(None, "s", schema, &["id".into()], &[Value::Int4(1)])?
                .first()
                .and_then(|row| row.row_id)
                .expect("the original target exists")
        } else {
            0
        };
        match change {
            Change::Insert => db
                .storage
                .insert_tuple_branch_aware("s", Tuple::new(values(3, 30, "branch-new")))
                .map(|_| ()),
            Change::Move => view.put(None, "s", target, &Tuple::new(values(11, 110, "branch-moved"))),
            Change::Delete => db.storage.delete_tuples_branch_aware("s", vec![target]).map(|_| ()),
            Change::Truncate => view.truncate(None, "s", schema).map(|_| ()),
        }
    })
}

fn raw_image(db: &EmbeddedDatabase) -> Vec<(Vec<u8>, Vec<u8>)> {
    db.storage
        .db()
        .iterator(IteratorMode::Start)
        .map(|item| {
            let (key, value) = item.unwrap();
            (key.to_vec(), value.to_vec())
        })
        .collect()
}

struct Keys(Vec<Vec<u8>>);
impl WriteBatchIterator for Keys {
    fn put(&mut self, key: Box<[u8]>, _: Box<[u8]>) {
        self.0.push(key.into_vec());
    }
    fn delete(&mut self, key: Box<[u8]>) {
        self.0.push(key.into_vec());
    }
}

fn assert_single_native_group(db: &EmbeddedDatabase, before: u64, branch: u64) {
    let raw = db.storage.db();
    raw.flush_wal(true).unwrap();
    let mut cursor = raw.get_updates_since(before).unwrap();
    let mut found = Vec::new();
    for item in cursor.by_ref() {
        let (sequence, batch) = item.unwrap();
        if sequence <= before || batch.is_empty() {
            continue;
        }
        let mut keys = Keys(Vec::new());
        batch.iterate(&mut keys);
        assert_eq!(
            sequence,
            before + 1,
            "one group starts immediately after the saved cursor"
        );
        assert_eq!(sequence + batch.len() as u64 - 1, raw.latest_sequence_number());
        assert!(keys.0.iter().any(|key| {
            key.starts_with(format!("bdata:{branch}:s:").as_bytes())
                || key.starts_with(format!("bdel:{branch}:s:").as_bytes())
        }));
        found.push(keys.0);
    }
    cursor.status().unwrap();
    assert_eq!(found.len(), 1, "error settlement adds no native repair write");
}

fn check_after_error(db: &EmbeddedDatabase, branch: u64, child: Option<u64>, schema: &Schema, expected: &[Vec<Value>]) {
    let view = BranchView::for_chain(&db.storage, "b", vec![branch]).unwrap();
    assert_truth(&view, schema, expected);
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let fresh = BranchView::for_chain(&db.storage, "b", vec![branch]).unwrap();
                assert_truth(&fresh, schema, expected);
            })
            .join()
            .unwrap();
    });
    assert_eq!(sql_rows(db, "b", "SELECT id,u,payload FROM s ORDER BY id"), expected);
    for id in [1, 2, 3, 11] {
        let wanted: Vec<_> = expected
            .iter()
            .filter(|row| integer(row.first().unwrap()) == id)
            .cloned()
            .collect();
        assert_eq!(
            sql_rows(db, "b", &format!("SELECT id,u,payload FROM s WHERE id={id}")),
            wanted
        );
    }
    if let Some(child) = child {
        let child = BranchView::for_chain(&db.storage, "child", vec![branch, child]).unwrap();
        assert_truth(&child, schema, &original());
    }
    assert_eq!(
        sql_rows(db, "main", "SELECT id,u,payload FROM s"),
        vec![values(100, 1000, "main-only")]
    );
}

// Keep the real admitted cache object alive solely to prove the fixture's
// premise. Its saved rows are NOT a public read oracle after the mutation.
fn retain_current_overlay(
    view: &BranchView<'_>,
    schema: &Schema,
) -> std::sync::Arc<parking_lot::Mutex<super::CommittedOverlay>> {
    let scope = view.scope("s").unwrap();
    let cached = view
        .committed_overlay("s", schema.columns.len(), scope.first_level)
        .unwrap();
    let key = super::CommittedKey {
        engine: view.engine.instance_id(),
        branch: view.branch_id(),
        table: "s".into(),
    };
    let admitted = super::COMMITTED
        .lock()
        .lookup(&key)
        .expect("fixture overlay must be admitted");
    assert!(
        std::sync::Arc::ptr_eq(&cached, &admitted),
        "the retained overlay is the actual public cache entry"
    );
    {
        let overlay = cached.lock();
        assert_eq!(overlay.width, schema.columns.len());
        assert_eq!(overlay.first_level, scope.first_level);
        assert_eq!(overlay.schema_generation, view.engine.schema_generation());
        let generations: Vec<_> = view
            .chain()
            .iter()
            .skip(scope.first_level)
            .map(|level| super::current_generation(view.engine.instance_id(), *level, "s"))
            .collect();
        assert_eq!(overlay.generations, generations);
    }
    report_overlay("before_native", view, &cached);
    cached
}

fn report_overlay(
    phase: &str,
    view: &BranchView<'_>,
    retained: &std::sync::Arc<parking_lot::Mutex<super::CommittedOverlay>>,
) {
    let key = super::CommittedKey {
        engine: view.engine.instance_id(),
        branch: view.branch_id(),
        table: "s".into(),
    };
    let current = super::COMMITTED.lock().lookup(&key);
    let same = current
        .as_ref()
        .is_some_and(|entry| std::sync::Arc::ptr_eq(entry, retained));
    let overlay = retained.lock();
    let generations: Vec<_> = view
        .chain()
        .iter()
        .skip(overlay.first_level)
        .map(|level| super::current_generation(view.engine.instance_id(), *level, "s"))
        .collect();
    eprintln!("BRANCH_CACHE_PREMISE {phase} admitted_same={same} cached_schema={} actual_schema={} cached_generations={:?} actual_generations={generations:?} width={} first_level={} copy_in_flight={} physical_standby={}",
        overlay.schema_generation, view.engine.schema_generation(), overlay.generations,
        overlay.width, overlay.first_level,
        view.engine.branch_freeze_in_flight(view.chain().get(overlay.first_level..).unwrap_or(&[]), "s"),
        view.engine.is_physical_standby());
}

fn exercise(fault: Fault, change: Change, history: bool, child: bool) {
    let dir = tempfile::tempdir().unwrap();
    let final_rows;
    let (branch, child_id, schema);
    {
        let db = open(dir.path(), history);
        (branch, child_id, schema) = setup(&db, child);
        let view = BranchView::for_chain(&db.storage, "b", vec![branch]).unwrap();
        let original_row_id = view
            .rows_with_key(None, "s", &schema, &["id".into()], &[Value::Int4(1)])
            .unwrap()
            .first()
            .unwrap()
            .row_id
            .unwrap();
        // Warm scan plus primary/unique and non-index payload memo maps on
        // this exact live view, then prove/retain its globally admitted object.
        assert_truth(&view, &schema, &original());
        let retained_overlay = retain_current_overlay(&view, &schema);
        let before = db.storage.db().latest_sequence_number();
        let before_generation = super::current_generation(db.storage.instance_id(), branch, "s");
        let before_image = raw_image(&db);
        match fault {
            Fault::BeforeApply => db.storage.write_gate().debug_fail_next_native_write_before_apply(),
            Fault::AfterApply => db.storage.write_gate().debug_fail_next_native_write_after_apply(),
            Fault::Cap => db.storage.debug_set_write_group_ceiling(Some(0)),
        }
        let result = mutate(&db, branch, &schema, change);
        db.storage.debug_set_write_group_ceiling(None);
        let error = result.expect_err("the injected owning-group error must reach the direct caller");
        match fault {
            Fault::AfterApply => {
                assert!(
                    error.to_string().contains("injected native write error after apply"),
                    "{error}"
                );
                assert_single_native_group(&db, before, branch);
                final_rows = changed(change);
                // The raw own row has the new logical value (or is absent),
                // before consulting any point/cache/index accessor.
                let own = db.storage.get(&branch_row_key(branch, "s", original_row_id)).unwrap();
                match change {
                    Change::Move => assert_eq!(
                        bincode::deserialize::<Tuple>(&own.unwrap()).unwrap().values,
                        values(11, 110, "branch-moved")
                    ),
                    Change::Delete | Change::Truncate => assert!(own.is_none()),
                    Change::Insert => assert_eq!(
                        bincode::deserialize::<Tuple>(&own.unwrap()).unwrap().values,
                        original().first().unwrap().clone()
                    ),
                }
            }
            Fault::BeforeApply | Fault::Cap => {
                let marker = if matches!(fault, Fault::Cap) {
                    crate::error::BRANCH_MATERIALIZE_TOO_LARGE_MARKER
                } else {
                    "injected native write error before apply; outcome is unknown"
                };
                assert!(error.to_string().contains(marker), "{error}");
                assert_eq!(
                    db.storage.db().latest_sequence_number(),
                    before,
                    "no owning native group applied"
                );
                assert_eq!(
                    raw_image(&db),
                    before_image,
                    "refusal cannot change row/key/tombstone/copy bytes"
                );
                final_rows = original();
            }
        }
        report_overlay("after_native_before_public_read", &view, &retained_overlay);
        // Actual point/raw-ID truth is compared with a NORMAL public scan and
        // key probe. Never compare the retained Arc's old snapshot to new rows.
        assert_truth(&view, &schema, &final_rows);
        check_after_error(&db, branch, child_id, &schema, &final_rows);
        let after_generation = super::current_generation(db.storage.instance_id(), branch, "s");
        if matches!(fault, Fault::Cap) {
            assert_eq!(
                after_generation, before_generation,
                "definite refusal preserves the own overlay generation"
            );
        } else {
            assert!(
                after_generation > before_generation,
                "ambiguous native outcome invalidates before the caller returns"
            );
        }
        if !matches!(fault, Fault::AfterApply) {
            // Claims and table leases must have been released, and a failed
            // desired image must not hide an actual old key on retry.
            mutate(&db, branch, &schema, change).unwrap();
            check_after_error(&db, branch, child_id, &schema, &changed(change));
        }
        db.execute("INSERT INTO s VALUES (101,1001,'healthy-main')").unwrap();
        assert_eq!(
            sql_rows(&db, "main", "SELECT id FROM s ORDER BY id"),
            vec![vec![Value::Int4(100)], vec![Value::Int4(101)]]
        );
    }
    let db = open(dir.path(), history);
    let final_rows = changed(change);
    let view = BranchView::for_chain(&db.storage, "b", vec![branch]).unwrap();
    assert_truth(&view, &schema, &final_rows);
    if let Some(child) = child_id {
        let child = BranchView::for_chain(&db.storage, "child", vec![branch, child]).unwrap();
        assert_truth(&child, &schema, &original());
    }
    assert_eq!(
        sql_rows(&db, "main", "SELECT id FROM s ORDER BY id"),
        vec![vec![Value::Int4(100)], vec![Value::Int4(101)]]
    );
    // Constraint enforcement remains usable after the failed caller and reopen.
    if let Some(existing) = final_rows.first() {
        let duplicate = Tuple::new(vec![
            existing.first().unwrap().clone(),
            Value::Int4(9999),
            Value::String("duplicate".into()),
        ]);
        assert!(view.insert_checked(None, "s", &schema, 50, &duplicate).is_err());
    }
    db.with_request_branch_scope("b", || -> Result<()> {
        let _lease = db.storage.lock_table_for_write("s")?;
        view.insert_checked(None, "s", &schema, 60, &Tuple::new(values(60, 600, "after-reopen")))
    })
    .unwrap();
}

#[test]
fn applied_native_errors_reload_warmed_branch_rows_and_key_indexes() {
    for history in [false, true] {
        for change in CHANGES {
            exercise(Fault::AfterApply, change, history, false);
        }
    }
}

#[test]
fn applied_native_errors_preserve_frozen_children_and_healthy_main() {
    for change in CHANGES {
        exercise(Fault::AfterApply, change, true, true);
    }
}

#[test]
fn ambiguous_native_errors_before_apply_never_publish_desired_branch_images() {
    for change in CHANGES {
        exercise(Fault::BeforeApply, change, true, false);
    }
}

#[test]
fn cap_refusal_preserves_branch_bytes_keys_and_retry() {
    for change in CHANGES {
        exercise(Fault::Cap, change, true, true);
    }
}
