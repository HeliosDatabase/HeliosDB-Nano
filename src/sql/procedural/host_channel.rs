//! What a routine's interpreter and the engine running its statements tell
//! each other beside the statement text.
//!
//! The interpreter hands each embedded statement to its host as text. Two
//! things do not fit in text: the values of the routine variables the
//! statement names (they are bound as `$1`, `$2`, …, never written into the
//! text), and what the statement did (how many rows, and the names of the
//! columns it returned). Both travel here, on the thread that runs the
//! statement, for exactly one statement at a time.

use crate::Value;
use std::cell::RefCell;

/// What the host says a statement did.
#[derive(Debug, Clone, Default)]
pub(crate) struct StatementReport {
    /// Rows returned, or rows changed.
    pub row_count: u64,
    /// The names of the returned columns; empty when the host does not know.
    pub columns: Vec<String>,
}

thread_local! {
    static PARAMS: RefCell<Option<Vec<Value>>> = const { RefCell::new(None) };
    static REPORT: RefCell<Option<StatementReport>> = const { RefCell::new(None) };
}

/// Interpreter: nothing is pending for the statement about to run.
pub(crate) fn clear() {
    PARAMS.with(|p| *p.borrow_mut() = None);
    REPORT.with(|r| *r.borrow_mut() = None);
}

/// Interpreter: the values of `$1…` of the statement about to run.
pub(crate) fn offer_params(params: Vec<Value>) {
    PARAMS.with(|p| *p.borrow_mut() = Some(params));
}

/// Were parameters offered that no host took?
pub(crate) fn params_pending() -> bool {
    PARAMS.with(|p| p.borrow().is_some())
}

/// Host: the parameters of the statement it is about to run (none: empty).
/// Taking them is what tells the interpreter they were bound.
pub(crate) fn take_params() -> Vec<Value> {
    PARAMS.with(|p| p.borrow_mut().take()).unwrap_or_default()
}

/// Host: what the statement did.
pub(crate) fn report(row_count: u64, columns: Vec<String>) {
    REPORT.with(|r| *r.borrow_mut() = Some(StatementReport { row_count, columns }));
}

/// Interpreter: the host's report on the statement that just ran, if it made one.
pub(crate) fn take_report() -> Option<StatementReport> {
    REPORT.with(|r| r.borrow_mut().take())
}
