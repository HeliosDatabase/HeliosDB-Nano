//! Deterministic publication-boundary controls. These run with a real graph
//! larger than the small-index exact-scan threshold; no sleep is a handshake.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::sql::{BinaryOperator, LogicalExpr};
use crate::{EmbeddedDatabase, Value};
use std::cell::{Cell, RefCell};
use std::sync::{mpsc, Arc};
use std::time::Duration;

type Hook = (&'static str, Box<dyn FnOnce()>);
thread_local! {
    static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    static SERVED: Cell<usize> = const { Cell::new(0) };
    static REACHED: Cell<usize> = const { Cell::new(0) };
}
pub(crate) fn run_hook(phase: &str) {
    let hook = HOOK.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().is_some_and(|(wanted, _)| *wanted == phase) {
            slot.take()
        } else {
            None
        }
    });
    if let Some((_, hook)) = hook {
        REACHED.with(|n| n.set(n.get() + 1));
        hook();
    }
}
pub(super) fn note_served() {
    SERVED.with(|n| n.set(n.get() + 1));
}
pub(crate) fn arm(phase: &'static str, action: impl FnOnce() + 'static) {
    HOOK.with(|slot| assert!(slot.borrow_mut().replace((phase, Box::new(action))).is_none()));
}
struct Reset;
impl Drop for Reset {
    fn drop(&mut self) {
        drop(HOOK.with(|slot| slot.borrow_mut().take()));
    }
}
fn reset() -> Reset {
    HOOK.with(|slot| assert!(slot.borrow().is_none()));
    SERVED.with(|n| n.set(0));
    REACHED.with(|n| n.set(0));
    Reset
}
fn served() -> usize {
    SERVED.with(Cell::get)
}
fn seed() -> Arc<EmbeddedDatabase> {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    db.execute("CREATE TABLE ann_rows (id INT PRIMARY KEY, emb VECTOR(2))")
        .unwrap();
    let values = (1..=512)
        .map(|id| format!("({id}, '[{},0]')", (id - 1) * 100))
        .collect::<Vec<_>>()
        .join(",");
    db.execute(&format!("INSERT INTO ann_rows VALUES {values}")).unwrap();
    db.execute("CREATE INDEX ann_graph ON ann_rows USING hnsw (emb vector_l2_ops)")
        .unwrap();
    // Index DDL clears watermarks; this tracked write establishes the exact
    // before-snapshot proof needed by a healthy RC transaction.
    db.execute("UPDATE ann_rows SET emb = '[0,0]' WHERE id = 1").unwrap();
    assert!(db.storage.vector_indexes().index_live_size("ann_graph").unwrap() > 256);
    db
}
const SQL: &str = "SELECT id FROM ann_rows ORDER BY emb <-> '[0,0]' LIMIT 1";
fn plan(db: &EmbeddedDatabase) -> Arc<LogicalPlan> {
    db.wire_parameterized_plan(SQL).unwrap().0.plan
}
fn run(db: &EmbeddedDatabase, plan: &LogicalPlan, txn: Option<&crate::storage::Transaction>) -> Vec<Tuple> {
    let scope = crate::storage::layout_epochs::StatementScope::enter();
    let mut executor = Executor::with_storage(&db.storage);
    if let Some(txn) = txn {
        executor = executor.with_transaction(txn);
    }
    let rows = executor.execute(plan).unwrap();
    scope.finish().unwrap();
    rows
}
fn ids(rows: Vec<Tuple>) -> Vec<Value> {
    rows.into_iter().map(|row| row.values[0].clone()).collect()
}
fn move_nearest(db: &EmbeddedDatabase) {
    db.execute("UPDATE ann_rows SET emb = '[999999,0]' WHERE id = 1")
        .unwrap();
    db.execute("UPDATE ann_rows SET emb = '[0,0]' WHERE id = 2").unwrap();
}

#[test]
fn unchanged_autocommit_and_proven_current_rc_keep_ann_enabled() {
    let _reset = reset();
    let db = seed();
    let plan = plan(&db);
    assert_eq!(ids(run(&db, &plan, None)), vec![Value::Int4(1)]);
    assert_eq!(served(), 1, "healthy autocommit used the real ANN path");
    let session = db.create_wire_session("ann_reader").unwrap();
    db.begin_transaction_for_session(session).unwrap();
    let slot = db.session_txn_slot(session).unwrap();
    let guard = slot.read();
    let txn = guard.as_ref().unwrap();
    assert_eq!(ids(run(&db, &plan, Some(txn))), vec![Value::Int4(1)]);
    assert_eq!(served(), 2, "a proven-current RC snapshot used ANN");
    drop(guard);
    db.rollback_transaction_for_session(session).unwrap();
}

#[test]
fn a_writer_fully_completed_after_candidates_declines_ann_and_returns_current_exact_rows() {
    let _reset = reset();
    let db = seed();
    let plan = plan(&db);
    let writer = Arc::clone(&db);
    arm("after_candidates", move || {
        assert!(!writer.storage.has_uncommitted_writes_for_table("ann_rows"));
        std::thread::spawn(move || move_nearest(&writer)).join().unwrap();
    });
    assert_eq!(ids(run(&db, &plan, None)), vec![Value::Int4(2)]);
    assert_eq!(REACHED.with(Cell::get), 1, "real ANN candidates were fetched");
    assert_eq!(served(), 0, "0->writer->0 census ABA must still decline ANN");
    assert!(!db.storage.has_uncommitted_writes_for_table("ann_rows"));
}

#[test]
fn an_rc_snapshot_predating_a_writer_before_ann_entry_uses_its_exact_history() {
    let _reset = reset();
    let db = seed();
    let plan = plan(&db);
    let session = db.create_wire_session("ann_reader").unwrap();
    db.begin_transaction_for_session(session).unwrap();
    let slot = db.session_txn_slot(session).unwrap();
    let guard = slot.read();
    let txn = guard.as_ref().unwrap();
    let writer = Arc::clone(&db);
    arm("before_ticket", move || {
        std::thread::spawn(move || move_nearest(&writer)).join().unwrap();
    });
    assert_eq!(ids(run(&db, &plan, Some(txn))), vec![Value::Int4(1)]);
    assert_eq!(REACHED.with(Cell::get), 1);
    assert_eq!(served(), 0, "RC identity alone cannot prove snapshot freshness");
    drop(guard);
    db.rollback_transaction_for_session(session).unwrap();
    assert_eq!(ids(run(&db, &plan, None)), vec![Value::Int4(2)]);
    assert_eq!(served(), 1, "current autocommit can use the published graph again");
}

#[test]
fn a_real_fast_writer_between_row_commit_and_graph_publication_keeps_ann_off() {
    let _reset = reset();
    let db = seed();
    let plan = plan(&db);
    let (arrived_tx, arrived_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let writer_db = Arc::clone(&db);
    let writer = std::thread::spawn(move || {
        let _reset = reset();
        arm("writer_row_applied", move || {
            arrived_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(20)).unwrap();
        });
        // Exercise the accepted public fast storage API directly. SQL may
        // choose the generic transaction route for a vector-valued SET.
        let schema = writer_db.storage.catalog().get_table_schema("ann_rows").unwrap();
        let row_id = writer_db
            .storage
            .get_row_by_pk("ann_rows", &Value::Int4(1))
            .unwrap()
            .unwrap()
            .row_id
            .unwrap();
        let _row = writer_db.storage.lock_row_for_fast_write("ann_rows", row_id).unwrap();
        let old = writer_db
            .storage
            .get_row_by_pk("ann_rows", &Value::Int4(1))
            .unwrap()
            .unwrap();
        let mut new = old.clone();
        new.values[1] = Value::Vector(vec![999999.0, 0.0]);
        writer_db
            .storage
            .update_tuple_fast("ann_rows", row_id, new, &old, &schema)
            .unwrap();
        assert_eq!(
            REACHED.with(Cell::get),
            1,
            "actual fast row write reached its graph boundary"
        );
    });
    arrived_rx.recv_timeout(Duration::from_secs(20)).unwrap();
    assert!(db.storage.has_uncommitted_writes_for_table("ann_rows"));
    assert_eq!(ids(run(&db, &plan, None)), vec![Value::Int4(2)]);
    assert_eq!(served(), 0);
    release_tx.send(()).unwrap();
    writer.join().unwrap();
    assert!(!db.storage.has_uncommitted_writes_for_table("ann_rows"));
}

#[test]
fn a_changed_publication_after_a_sequence_effect_refuses_without_repeating_it() {
    let _reset = reset();
    let db = seed();
    // Direct Executor calls must share this backend with the SQL currval probe.
    let _backend = crate::SessionScopedStateGuard::install_if_absent(&db.embedded_scoped);
    db.execute("CREATE SEQUENCE ann_filter_seq START 1").unwrap();
    let sql = "SELECT id, (SELECT nextval('ann_filter_seq')) AS n FROM ann_rows ORDER BY emb <-> '[0,0]' LIMIT 1";
    let pinned = db.wire_parameterized_plan(sql).unwrap().0;
    let writer = Arc::clone(&db);
    arm("after_rows", move || {
        std::thread::spawn(move || move_nearest(&writer)).join().unwrap();
    });
    let scope = crate::storage::layout_epochs::StatementScope::enter();
    let error = Executor::with_storage(&db.storage).execute(&pinned.plan).unwrap_err();
    assert!(
        error
            .to_string()
            .contains(crate::storage::layout_epochs::LAYOUT_CHANGED_MARKER),
        "{error}"
    );
    assert!(!scope.retriable());
    drop(scope);
    assert_eq!(REACHED.with(Cell::get), 1);
    assert_eq!(served(), 0);
    assert_eq!(
        db.query("SELECT currval('ann_filter_seq')", &[]).unwrap()[0].values,
        vec![Value::Int8(1)],
        "a fallback must not evaluate nextval a second time"
    );
}

#[test]
fn a_selective_volatile_filter_declines_before_ann_and_evaluates_each_row_once() {
    let _reset = reset();
    let db = seed();
    // Direct Executor calls must share this backend with the SQL currval probe.
    let _backend = crate::SessionScopedStateGuard::install_if_absent(&db.embedded_scoped);
    db.execute("CREATE SEQUENCE ann_filter_excluded START 1").unwrap();
    let sql = "SELECT id FROM ann_rows WHERE nextval('ann_filter_excluded') < 0 ORDER BY emb <-> '[0,0]' LIMIT 1";
    let pinned = db.wire_parameterized_plan(sql).unwrap().0;
    arm("after_candidates", || {
        panic!("effectful predicates must never enter ANN over-fetch")
    });
    assert!(run(&db, &pinned.plan, None).is_empty());
    assert_eq!(served(), 0);
    assert_eq!(REACHED.with(Cell::get), 0);
    assert_eq!(
        db.query("SELECT currval('ann_filter_excluded')", &[]).unwrap()[0].values,
        vec![Value::Int8(512)],
        "exact fallback evaluates the filter once per row"
    );
}

#[test]
fn a_failed_graph_hook_uses_exact_rows_even_when_heap_bytes_never_changed() {
    let _reset = reset();
    let db = seed();
    let plan = plan(&db);
    let schema = db.storage.catalog().get_table_schema("ann_rows").unwrap();
    let old = db.storage.get_row_by_id("ann_rows", 1, &schema).unwrap().unwrap();
    let before = db.storage.db().get(b"data:ann_rows:1").unwrap().unwrap();
    let manager = db.storage.vector_indexes();
    assert_eq!(manager.hook_error_generation("ann_rows"), Some(0));
    assert_eq!(manager.index_live_size("ann_graph"), Some(512));
    let mut invalid = old.clone();
    invalid.values[1] = Value::Vector(vec![0.0, 0.0, 0.0]);
    let _undo = manager.on_row_update("ann_rows", 1, &schema, Some(&old), &invalid);
    assert_ne!(
        manager.hook_error_generation("ann_rows"),
        Some(0),
        "real backend dimension error must record a fault"
    );
    assert_eq!(
        manager.index_live_size("ann_graph"),
        Some(511),
        "old graph member was removed before insertion failed"
    );
    assert_eq!(db.storage.db().get(b"data:ann_rows:1").unwrap().unwrap(), before);
    assert_eq!(
        ids(run(&db, &plan, None)),
        vec![Value::Int4(1)],
        "the correct nearest row remains in heap storage"
    );
    assert_eq!(served(), 0, "settled but damaged graph must not answer a later query");
}

#[test]
fn pinned_ann_plans_refuse_completed_drop_and_storage_changes_on_both_scan_shapes() {
    fn use_filtered_scan(plan: &mut LogicalPlan) -> usize {
        match plan {
            LogicalPlan::Scan { .. } => {
                let LogicalPlan::Scan {
                    table_name,
                    alias,
                    schema,
                    projection,
                    as_of,
                } = plan.clone()
                else {
                    unreachable!()
                };
                *plan = LogicalPlan::FilteredScan {
                    table_name,
                    alias,
                    schema,
                    projection,
                    as_of,
                    predicate: Some(LogicalExpr::BinaryExpr {
                        left: Box::new(LogicalExpr::Column {
                            table: Some("a".into()),
                            name: "id".into(),
                        }),
                        op: BinaryOperator::GtEq,
                        right: Box::new(LogicalExpr::Literal(Value::Int4(1))),
                    }),
                };
                1
            }
            LogicalPlan::Limit { input, .. } | LogicalPlan::Project { input, .. } | LogicalPlan::Sort { input, .. } => {
                use_filtered_scan(input)
            }
            _ => 0,
        }
    }
    for ddl in [
        "ALTER TABLE ann_rows DROP COLUMN obsolete",
        "ALTER TABLE ann_rows ALTER COLUMN label SET STORAGE DICTIONARY",
    ] {
        for filtered in [false, true] {
            let _reset = reset();
            let db = EmbeddedDatabase::new_in_memory().unwrap();
            db.execute("CREATE TABLE ann_rows (obsolete INT, id INT PRIMARY KEY, emb VECTOR(2), label TEXT)")
                .unwrap();
            let values = (1..=512)
                .map(|id| format!("(9,{id},'[{},0]','label_{id}')", (id - 1) * 100))
                .collect::<Vec<_>>()
                .join(",");
            db.execute(&format!("INSERT INTO ann_rows VALUES {values}")).unwrap();
            db.execute("CREATE INDEX ann_graph ON ann_rows USING hnsw (emb vector_l2_ops)")
                .unwrap();
            assert_eq!(db.storage.vector_indexes().index_live_size("ann_graph"), Some(512));
            let sql = "SELECT a.id FROM ann_rows AS a ORDER BY a.emb <-> '[0,0]' LIMIT 1";
            let mut pinned = db.wire_parameterized_plan(sql).unwrap().0.plan.as_ref().clone();
            if filtered {
                assert_eq!(use_filtered_scan(&mut pinned), 1);
            }
            assert_eq!(ids(run(&db, &pinned, None)), vec![Value::Int4(1)]);
            assert_eq!(served(), 1, "control must use a real graph, {ddl} filtered={filtered}");
            db.execute(ddl).unwrap();
            // Deliberately bypass the SQL handle's replan and outer LayoutStamp.
            let error = Executor::with_storage(&db.storage).execute(&pinned).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains(crate::storage::layout_epochs::LAYOUT_CHANGED_MARKER),
                "{ddl} filtered={filtered}: {error}"
            );
            assert_eq!(served(), 1, "old layout must never serve ANN candidates");
            let mut fresh = db.wire_parameterized_plan(sql).unwrap().0.plan.as_ref().clone();
            if filtered {
                assert_eq!(use_filtered_scan(&mut fresh), 1);
            }
            assert_eq!(ids(run(&db, &fresh, None)), vec![Value::Int4(1)]);
            assert_eq!(served(), 2, "fresh qualified layout keeps healthy ANN enabled");
        }
    }
}
