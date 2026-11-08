//! Private lexical caller-transaction contract. No function registry is needed:
//! these use real raw storage transactions and the scope's borrowed accessor.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::{with_caller_transaction, with_matching_caller_transaction};
use crate::storage::{StorageEngine, Transaction};
use crate::Config;
use std::panic::{catch_unwind, panic_any, AssertUnwindSafe};

fn engine() -> StorageEngine {
    let mut config = Config::in_memory();
    config.storage.version_gc_interval_secs = Some(0);
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    config.storage.wal_checkpoint_on_close = false;
    config.storage.flush_on_close = false;
    let engine = StorageEngine::open_in_memory(&config).unwrap();
    engine.set_index_snapshots_on_close(false);
    engine.set_row_counter_flush_on_close(false);
    engine
}

fn visible(engine_id: u64) -> Option<u64> {
    with_matching_caller_transaction(engine_id, |txn| txn.map(Transaction::transaction_id))
}

fn same_borrow(engine_id: u64, expected: &Transaction) {
    with_matching_caller_transaction(engine_id, |actual| {
        let actual = actual.expect("matching transaction is present");
        assert!(
            std::ptr::eq(actual, expected),
            "must borrow the original caller, not a reconstructed transaction"
        );
        assert_eq!(actual.transaction_id(), expected.transaction_id());
    });
}

fn expected_panic(result: std::thread::Result<()>, marker: &'static str) {
    let error = result.expect_err("deliberate scoped unwind must reach the catcher");
    assert_eq!(
        error.downcast_ref::<&'static str>().copied(),
        Some(marker),
        "must not disguise a failed inner assertion as the deliberate panic"
    );
}

#[test]
fn borrowed_caller_scope_is_engine_qualified_and_ends_with_its_closure() {
    let a = engine();
    let b = engine();
    let txn = a.begin_transaction().unwrap();
    let (aid, bid) = (a.instance_id(), b.instance_id());
    assert_ne!(aid, bid);
    assert_eq!(visible(aid), None);
    assert_eq!(visible(bid), None);

    let returned = with_caller_transaction(aid, Some(&txn), || {
        same_borrow(aid, &txn);
        assert_eq!(visible(bid), None, "another engine must not borrow this caller");
        txn.transaction_id()
    });
    assert_eq!(returned, txn.transaction_id(), "only an owned scalar escapes the scope");
    assert_eq!(visible(aid), None);
    assert_eq!(visible(bid), None);
    eprintln!("ROUTINE_CALLER_SCOPE_CELL engine_qualified_lifetime");
}

#[test]
fn explicit_none_masks_outer_transaction_for_same_and_other_engine() {
    let a = engine();
    let b = engine();
    let txn = a.begin_transaction().unwrap();
    let foreign = b.begin_transaction().unwrap();
    let (aid, bid) = (a.instance_id(), b.instance_id());

    with_caller_transaction(aid, Some(&txn), || {
        with_caller_transaction(aid, None, || {
            assert_eq!(
                visible(aid),
                None,
                "a distinct session on this engine has no caller transaction"
            );
            with_caller_transaction(bid, None, || {
                assert_eq!(visible(aid), None);
                assert_eq!(visible(bid), None);
                with_caller_transaction(bid, Some(&foreign), || {
                    same_borrow(bid, &foreign);
                    assert_eq!(visible(aid), None);
                });
                assert_eq!(visible(aid), None, "nested Some restores the intermediate None");
                assert_eq!(visible(bid), None);
            });
            assert_eq!(visible(aid), None, "nested masks restore the previous mask");
        });
        same_borrow(aid, &txn);
        with_caller_transaction(bid, None, || {
            assert_eq!(
                visible(aid),
                None,
                "an explicit other-engine frame hides the outer frame"
            );
            assert_eq!(visible(bid), None);
            with_caller_transaction(aid, None, || {
                with_caller_transaction(bid, Some(&foreign), || same_borrow(bid, &foreign));
                assert_eq!(visible(aid), None);
                assert_eq!(visible(bid), None);
            });
        });
        same_borrow(aid, &txn);
    });
    assert_eq!(visible(aid), None);
    assert_eq!(visible(bid), None);
    eprintln!("ROUTINE_CALLER_SCOPE_CELL explicit_none_masks");
}

#[test]
fn nested_some_scopes_restore_the_exact_previous_caller() {
    let a = engine();
    let b = engine();
    let outer = a.begin_transaction().unwrap();
    let sibling = a.begin_transaction().unwrap();
    let foreign = b.begin_transaction().unwrap();
    let (aid, bid) = (a.instance_id(), b.instance_id());
    assert_ne!(outer.transaction_id(), sibling.transaction_id());

    with_caller_transaction(aid, Some(&outer), || {
        let value = with_caller_transaction(aid, Some(&sibling), || {
            same_borrow(aid, &sibling);
            17_u64
        });
        assert_eq!(value, 17);
        same_borrow(aid, &outer);
        with_caller_transaction(bid, Some(&foreign), || {
            same_borrow(bid, &foreign);
            assert_eq!(visible(aid), None);
            with_caller_transaction(aid, None, || {
                assert_eq!(visible(aid), None);
                assert_eq!(visible(bid), None);
            });
            same_borrow(bid, &foreign);
        });
        same_borrow(aid, &outer);
        assert_eq!(visible(bid), None);
    });
    assert_eq!(visible(aid), None);
    assert_eq!(visible(bid), None);
    eprintln!("ROUTINE_CALLER_SCOPE_CELL nested_exact_restore");
}

#[test]
fn scoped_unwind_restores_borrowed_and_masked_predecessors() {
    const INNER: &str = "routine-caller-scope-deliberate-inner";
    const MASK: &str = "routine-caller-scope-deliberate-mask";
    const MASKED_PREDECESSOR: &str = "routine-caller-scope-deliberate-masked-predecessor";
    const OUTER: &str = "routine-caller-scope-deliberate-outer";
    let a = engine();
    let b = engine();
    let outer = a.begin_transaction().unwrap();
    let inner = b.begin_transaction().unwrap();
    let (aid, bid) = (a.instance_id(), b.instance_id());

    with_caller_transaction(aid, Some(&outer), || {
        let result = catch_unwind(AssertUnwindSafe(|| {
            with_caller_transaction(bid, Some(&inner), || {
                same_borrow(bid, &inner);
                panic_any(INNER);
            });
        }));
        expected_panic(result, INNER);
        same_borrow(aid, &outer);
        assert_eq!(visible(bid), None);

        let result = catch_unwind(AssertUnwindSafe(|| {
            with_caller_transaction(aid, None, || {
                assert_eq!(visible(aid), None);
                panic_any(MASK);
            });
        }));
        expected_panic(result, MASK);
        same_borrow(aid, &outer);

        with_caller_transaction(aid, None, || {
            with_caller_transaction(bid, None, || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    with_caller_transaction(bid, Some(&inner), || {
                        same_borrow(bid, &inner);
                        panic_any(MASKED_PREDECESSOR);
                    });
                }));
                expected_panic(result, MASKED_PREDECESSOR);
                assert_eq!(visible(aid), None);
                assert_eq!(visible(bid), None);
            });
            assert_eq!(visible(aid), None, "unwind must restore the masked predecessor");
            assert_eq!(visible(bid), None);
        });
        same_borrow(aid, &outer);
    });
    assert_eq!(visible(aid), None);

    let result = catch_unwind(AssertUnwindSafe(|| {
        with_caller_transaction(aid, Some(&outer), || {
            same_borrow(aid, &outer);
            panic_any(OUTER);
        });
    }));
    expected_panic(result, OUTER);
    assert_eq!(visible(aid), None);
    assert_eq!(visible(bid), None);
    eprintln!("ROUTINE_CALLER_SCOPE_CELL unwind_restore");
}

#[test]
fn borrowed_scope_is_thread_local_and_publishes_no_native_write() {
    let a = engine();
    let txn = a.begin_transaction().unwrap();
    let aid = a.instance_id();
    let before = a.db().latest_sequence_number();

    with_caller_transaction(aid, Some(&txn), || {
        same_borrow(aid, &txn);
        // The join occurs while this outer borrowed scope is still active.
        // Only the stable engine ID crosses threads, never its transaction.
        let worker = std::thread::spawn(move || {
            assert_eq!(visible(aid), None, "a thread must not inherit another thread's caller");
            with_caller_transaction(aid, None, || assert_eq!(visible(aid), None));
            visible(aid)
        });
        assert_eq!(worker.join().unwrap(), None);
        same_borrow(aid, &txn);
    });
    assert_eq!(visible(aid), None);
    assert_eq!(
        a.db().latest_sequence_number(),
        before,
        "scope publication is RAM-only; no implicit commit/repair group"
    );
    eprintln!("ROUTINE_CALLER_SCOPE_CELL thread_local_no_native");
}
