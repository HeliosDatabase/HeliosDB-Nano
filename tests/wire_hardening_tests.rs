//! Stability-hardening regression tests (campaign 2026-07, group C-I):
//! client-reachable panics must be errors, and malformed input must never
//! take down the process. Wire-frame parsing itself is unit-tested in
//! `src/protocol/postgres/messages.rs`; these cover the SQL-level surfaces.

use heliosdb_nano::EmbeddedDatabase;

#[test]
fn to_date_non_string_format_is_error_not_panic() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    // Used to hit `unreachable!()` in the evaluator — a client-triggerable
    // panic (and via COPY/execute() paths, a poisoned global mutex).
    let res = db.query("SELECT TO_DATE('2020-01-01', 123)", &[]);
    assert!(res.is_err(), "non-string TO_DATE format must be a query error");

    let res = db.query("SELECT TO_TIMESTAMP('2020-01-01 00:00:00', 456)", &[]);
    assert!(res.is_err(), "non-string TO_TIMESTAMP format must be a query error");

    // The connection/handle must remain fully usable afterwards.
    let rows = db.query("SELECT 1", &[]).unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn to_date_valid_forms_still_work() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let rows = db.query("SELECT TO_DATE('2020-01-15', 'YYYY-MM-DD')", &[]).unwrap();
    assert_eq!(rows.len(), 1);
    let rows = db.query("SELECT TO_DATE(NULL, 'YYYY-MM-DD')", &[]).unwrap();
    assert_eq!(rows.len(), 1);
}
