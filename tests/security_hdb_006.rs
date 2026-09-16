//! HDB-006 — constant folding must never abort the process on integer overflow.
//!
//! WHAT WAS BROKEN. `ConstantFoldingRule::fold_expr` (src/optimizer/rules.rs) folded
//! `Int4 op Int4` with raw `i32` arithmetic (`l + r`, `l - r`, `l * r`, `l / r`) and
//! unary minus with `-i`. The crate builds with overflow-checks on, so
//! `SELECT 2147483647 + 1` did not return a row and did not raise a SQL error — it
//! panicked at PLAN time. In embedded/REPL mode that is process exit 101; on the
//! PostgreSQL wire a `catch_unwind` in the handler caught it, so the statement
//! bypassed normal SQL error handling; the MySQL handler has no such guard at all.
//! Any client could therefore take the process down with one constant expression.
//!
//! THE INVARIANT. Constant folding is an optimisation: a folded literal expression
//! must produce EXACTLY what the runtime evaluator produces for the same operands,
//! or stay unfolded so the runtime produces it. It may never fail at plan time for
//! an arithmetic reason. Every case below is therefore checked twice — once as a
//! literal expression (the folded path) and once as the same operator over table
//! columns holding the same operands (the runtime path) — and the two must agree,
//! value for value and error message for error message.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};
use std::panic::{catch_unwind, AssertUnwindSafe};

/// The pinned answer for a case, derived from the runtime arms in
/// `src/sql/evaluator.rs` (see each case's comment).
#[derive(Debug)]
enum Expect {
    /// A single scalar value.
    Scalar(Value),
    /// A SQL error whose message contains this fragment.
    ErrorContains(&'static str),
}

/// One liveness case: a literal expression the optimiser folds, plus the SAME
/// operator applied over columns seeded with the SAME operands.
struct Case {
    /// The literal expression under test (the folded path).
    literal_sql: &'static str,
    /// SQL producing the operands as `INT` column values, in column order (a, b).
    operands: &'static [&'static str],
    /// The projection over those columns, e.g. `a + b` (the runtime path).
    column_expr: &'static str,
    /// What both paths must answer.
    expect: Expect,
}

fn scalar(rows: &[Tuple]) -> Value {
    assert_eq!(rows.len(), 1, "expected exactly one row, got {}", rows.len());
    let values = &rows[0].values;
    assert_eq!(values.len(), 1, "expected one column, got {}", values.len());
    values[0].clone()
}

/// Seed `hdb006_<index>` with the case's operands and return the parity query.
fn seed(db: &EmbeddedDatabase, index: usize, case: &Case) -> String {
    let table = format!("hdb006_{index}");
    let defs = ["a INT", "b INT"];
    let ddl = format!("CREATE TABLE {table} ({})", defs[..case.operands.len()].join(", "));
    db.execute(&ddl).expect(&ddl);
    let dml = format!("INSERT INTO {table} VALUES ({})", case.operands.join(", "));
    db.execute(&dml).expect(&dml);
    format!("SELECT {} FROM {table}", case.column_expr)
}

fn check(db: &EmbeddedDatabase, index: usize, case: &Case) {
    let column_sql = seed(db, index, case);
    let context = format!("folded `{}` vs runtime `{column_sql}`", case.literal_sql);

    // 1. Liveness. Before the fix this unwound inside the optimiser; `catch_unwind`
    //    turns a regression into a test failure instead of an aborted test binary.
    let folded = match catch_unwind(AssertUnwindSafe(|| db.query(case.literal_sql, &[]))) {
        Ok(result) => result,
        Err(_) => panic!("HDB-006: `{}` panicked instead of answering", case.literal_sql),
    };

    // 2. Parity: the same operands through the runtime evaluator.
    let runtime = db.query(&column_sql, &[]);
    match (&folded, &runtime) {
        (Ok(f), Ok(r)) => assert_eq!(scalar(f), scalar(r), "{context}"),
        (Err(f), Err(r)) => assert_eq!(f.to_string(), r.to_string(), "{context}"),
        _ => panic!("{context}: folded={folded:?} runtime={runtime:?}"),
    }

    // 3. The concrete expectation derived from the runtime arm.
    match (&case.expect, &folded) {
        (Expect::Scalar(want), Ok(rows)) => assert_eq!(&scalar(rows), want, "{context}"),
        (Expect::ErrorContains(fragment), Err(err)) => {
            let message = err.to_string();
            let matched = message.contains(fragment);
            assert!(matched, "{context}: {fragment:?} not in {message}");
        }
        (want, got) => panic!("{context}: expected {want:?}, answered {got:?}"),
    }
}

/// The liveness corpus. Every entry overflows `i32` somewhere, so every entry
/// aborted the process before the fix.
#[test]
fn folded_integer_overflow_neither_panics_nor_diverges_from_the_runtime() {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory db");

    let cases = [
        // `arithmetic_add`'s Int4/Int4 arm (src/sql/evaluator.rs:4238) widens to i64
        // and keeps Int8 when the result no longer fits i32.
        Case {
            literal_sql: "SELECT 2147483647 + 1",
            operands: &["2147483647", "1"],
            column_expr: "a + b",
            expect: Expect::Scalar(Value::Int8(2_147_483_648)),
        },
        // `arithmetic_multiply` (src/sql/evaluator.rs:4576), same shape.
        Case {
            literal_sql: "SELECT 1073741824 * 2",
            operands: &["1073741824", "2"],
            column_expr: "a * b",
            expect: Expect::Scalar(Value::Int8(2_147_483_648)),
        },
        // `arithmetic_subtract` (src/sql/evaluator.rs:4405), same shape.
        Case {
            literal_sql: "SELECT -2147483647 - 2",
            operands: &["-2147483647", "2"],
            column_expr: "a - b",
            expect: Expect::Scalar(Value::Int8(-2_147_483_649)),
        },
        // The widest i32 product: 2147483647^2 still fits i64.
        Case {
            literal_sql: "SELECT 2147483647 * 2147483647",
            operands: &["2147483647", "2147483647"],
            column_expr: "a * b",
            expect: Expect::Scalar(Value::Int8(4_611_686_014_132_420_609)),
        },
        // `arithmetic_divide` (src/sql/evaluator.rs:4716): i32::MIN / -1 is the one
        // division that overflows i32; the runtime answers Int8(2147483648).
        Case {
            literal_sql: "SELECT (-2147483647 - 1) / -1",
            operands: &["-2147483647 - 1", "-1"],
            column_expr: "a / b",
            expect: Expect::Scalar(Value::Int8(2_147_483_648)),
        },
        // Unary minus: `evaluate_unary_op` (src/sql/evaluator.rs:3630) uses
        // `checked_neg`, so -(i32::MIN) is a SQL error, never a panic.
        Case {
            literal_sql: "SELECT -(-2147483647 - 1)",
            operands: &["-2147483647 - 1"],
            column_expr: "-a",
            expect: Expect::ErrorContains("integer overflow"),
        },
        // Division by zero stays unfolded, exactly as before the fix, so the runtime
        // raises it. The message is "Division by zero"; the fragment drops the
        // leading capital so the assertion does not depend on it.
        Case {
            literal_sql: "SELECT 1 / 0",
            operands: &["1", "0"],
            column_expr: "a / b",
            expect: Expect::ErrorContains("ivision by zero"),
        },
    ];

    for (index, case) in cases.iter().enumerate() {
        check(&db, index, case);
    }

    // The corpus must not have left a dead engine (or a poisoned lock) behind.
    let rows = db.query("SELECT 42", &[]).expect("SELECT 42 after the corpus");
    assert_eq!(scalar(&rows), Value::Int4(42), "SELECT 42 after the corpus");
}

/// In-range arithmetic must keep folding to the narrow type — the fix must not
/// promote every folded expression to BIGINT.
#[test]
fn in_range_arithmetic_still_folds_to_int4() {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory db");

    for (sql, expected) in [
        ("SELECT 2 + 3", Value::Int4(5)),
        ("SELECT 10 - 4", Value::Int4(6)),
        ("SELECT 6 * 7", Value::Int4(42)),
        ("SELECT 9 / 3", Value::Int4(3)),
        ("SELECT -5", Value::Int4(-5)),
    ] {
        let rows = db.query(sql, &[]).expect(sql);
        assert_eq!(scalar(&rows), expected, "{sql}");
    }
}
