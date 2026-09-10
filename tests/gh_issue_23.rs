//! GH #23 — the executor-side (VALUE) half of "RETURNING loses its column
//! metadata".
//!
//! Install as `tests/gh_issue_23.rs`; run with `cargo test --test gh_issue_23`.
//!
//! # Scope, and why the OID half lives in the wire tests
//!
//! Issue #23 is primarily a WIRE-FORMAT defect: `RowDescription.dataTypeID` for
//! `INSERT/UPDATE/DELETE … RETURNING` is OID 25 (text) where PostgreSQL sends
//! the column's real type. The embedded API never materialises a
//! `RowDescription` — `EmbeddedDatabase::execute_params_returning` hands back
//! bare `Tuple`s — so the OID assertions live in the wire tests appended to
//! `src/protocol/postgres/wire_tests.rs` (`gh23_*`).
//!
//! What IS embedded-visible is the second-order damage from the same root
//! cause. `Planner::convert_returning` (src/sql/planner.rs:6464-6549) lowers a
//! RETURNING item to one of two shapes:
//!
//!   * `ReturningItem::Column(bare_name)` — for a bare (planner.rs:6511-6513)
//!     or QUALIFIED (planner.rs:6516-6523) column reference.
//!     `EmbeddedDatabase::returning_schema` (src/lib.rs:14833-14846) then types
//!     it from the CATALOG, and `project_returning_columns`
//!     (src/lib.rs:14646-14657) resolves its VALUE by bare name — the qualifier
//!     having already been dropped at plan time.
//!   * `ReturningItem::Expression { expr, alias }` — for everything else,
//!     INCLUDING every `col AS alias` (`SelectItem::ExprWithAlias`,
//!     src/sql/planner.rs:6534-6540, which has NO column-reference special
//!     case). `returning_schema` hard-codes that shape to `DataType::Text`
//!     (src/lib.rs:14850-14862) — the OID-25 half — and
//!     `project_returning_columns` resolves its VALUE through
//!     `Evaluator::evaluate` (src/sql/evaluator.rs:226-272), which matches the
//!     qualifier against the schema's stamped `source_table` /
//!     `source_table_name` BYTE-EXACTLY (`Schema::get_qualified_column_index`,
//!     src/types.rs:659-670) and whose `Err` `project_returning_columns` maps to
//!     `Value::Null` (src/lib.rs:14658-14663).
//!
//! `StorageCatalog::get_table_schema` stamps `source_table_name` with the
//! CANONICAL table name (src/storage/catalog.rs:1017-1021), i.e. `Typed` for a
//! table created as `"Typed"`. So an aliased item survives only when its
//! qualifier is spelled `"Typed"` byte-for-byte; `Typed."n" AS "c"` (unquoted →
//! case-folded to `typed` by `Planner::normalize_ident`, src/sql/planner.rs:329)
//! and `x."n" AS "c"` (a FROM-alias) both miss and silently yield NULL.
//!
//! # A correction to the executor-family framing (verified, do not re-derive)
//!
//! There is exactly ONE observable RETURNING executor family in this tree, and
//! it is the PARAMS family:
//!
//!   * `EmbeddedDatabase::execute_returning` is a one-line delegation to
//!     `execute_params_returning` (src/lib.rs:8906-8908), which plans and runs
//!     through `execute_plan_with_params` → `execute_plan_with_params_inner`.
//!   * The TEXT family (`execute_in_transaction_inner`, src/lib.rs:4781) returns
//!     `Result<u64>`. It DOES call `project_returning_columns` (src/lib.rs:5926,
//!     6103, 6336, 6374, 6413, 6815, 6949) but then throws the result away —
//!     see the literal `let _ = returned_tuples; // RETURNING clause results
//!     handled separately` at src/lib.rs:6820. No caller can observe it.
//!   * Both wire routes converge on the params family too:
//!     `execute_returning_for_session` (simple protocol, src/lib.rs:18717)
//!     delegates to `execute_returning` when there is no open session
//!     transaction (src/lib.rs:18742) and otherwise calls
//!     `execute_plan_with_params(.., Some(&txn))` (src/lib.rs:18752);
//!     `execute_params_returning_for_session` (extended protocol,
//!     src/lib.rs:18800) does the same at :18837 / :18853.
//!
//! So a `execute_returning` vs `execute_params_returning` matrix is NOT
//! two-family coverage — it is the same function twice. The axis that IS real
//! is AUTOCOMMIT vs INSIDE AN EXPLICIT TRANSACTION, because
//! `execute_plan_with_params_inner` takes a different `active_txn` leg in each
//! (`Some(txn)` vs the global `current_transaction` slot; src/lib.rs:15689-15695
//! for UPDATE, :16011-16018 for DELETE), and a different projection call site.
//! Every test below runs over BOTH of those, and over all three DML verbs, so a
//! fix that repairs one arm of `execute_plan_with_params_inner` and not the
//! others cannot pass.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Result, Tuple, Value};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// The two RETURNING entry points the crate exposes. They are the SAME code
/// path (see the module note); running both is a cheap pin on that delegation,
/// not independent coverage. The real coverage axis is [`TXN_MODES`].
const ENTRY_POINTS: [(bool, &str); 2] = [(false, "execute_returning"), (true, "execute_params_returning")];

/// The axis that genuinely exercises two different legs of
/// `execute_plan_with_params_inner`: `active_txn = None` (autocommit, global
/// slot) vs `active_txn = Some(txn)` (an open explicit transaction).
const TXN_MODES: [(bool, &str); 2] = [(false, "autocommit"), (true, "in explicit txn")];

fn returning(db: &EmbeddedDatabase, sql: &str, params_entry: bool) -> Result<(u64, Vec<Tuple>)> {
    if params_entry {
        db.execute_params_returning(sql, &[])
    } else {
        db.execute_returning(sql)
    }
}

/// Run `sql` through one entry point, optionally wrapped in an explicit
/// transaction. On success the transaction is COMMITted so the fixture stays
/// coherent; on failure it is ROLLBACKed and the error propagated, so an error
/// can never leak an open transaction into the next iteration.
fn returning_in_mode(db: &EmbeddedDatabase, sql: &str, params_entry: bool, in_txn: bool) -> Result<(u64, Vec<Tuple>)> {
    if !in_txn {
        return returning(db, sql, params_entry);
    }
    db.execute("BEGIN").expect("BEGIN must be accepted");
    match returning(db, sql, params_entry) {
        Ok(out) => {
            db.execute("COMMIT").expect("COMMIT must be accepted");
            Ok(out)
        }
        Err(e) => {
            let _ = db.execute("ROLLBACK");
            Err(e)
        }
    }
}

/// A table quoted and mixed-cased exactly the way Prisma emits it. The
/// mixed case is load-bearing: `Planner::normalize_ident` preserves a QUOTED
/// identifier and lower-cases an unquoted one, which is what makes
/// `Typed."n"` miss the catalog's stamped `Typed`.
fn seeded_db() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
    db.execute(
        r#"CREATE TABLE "Typed" (
             "id" INTEGER PRIMARY KEY,
             "isStaff" BOOLEAN NOT NULL,
             "n" INTEGER NOT NULL,
             "note" TEXT
           )"#,
    )
    .expect("create \"Typed\"");
    db.execute(
        r#"INSERT INTO "public"."Typed" ("id","isStaff","n","note")
           VALUES (1, true, 7, 'hello')"#,
    )
    .expect("seed");
    db
}

/// True only for SQL NULL — lets an assertion say "must not be NULL" without
/// also committing to which non-NULL value a not-yet-written fix will produce.
fn is_null(v: &Value) -> bool {
    matches!(v, Value::Null)
}

// ---------------------------------------------------------------------------
// Positive controls
// ---------------------------------------------------------------------------

/// POSITIVE CONTROL — passes BEFORE and AFTER the fix, on every entry point,
/// both transaction modes and all three DML verbs. If it ever fails, the
/// fixture, the transaction plumbing or the harness is broken and nothing else
/// in this file means anything.
///
/// A BARE and a QUALIFIED RETURNING column both lower to
/// `ReturningItem::Column` (src/sql/planner.rs:6511-6523) and are resolved by
/// bare name, so both carry the row's real value with its real Rust type.
#[test]
fn gh23_control_bare_and_qualified_returning_carry_real_values() {
    for (params_entry, entry) in ENTRY_POINTS {
        for (in_txn, mode) in TXN_MODES {
            let tag = format!("{entry} / {mode}");
            let db = seeded_db();

            // UPDATE arm.
            let (affected, rows) = returning_in_mode(
                &db,
                r#"UPDATE "public"."Typed" SET "n" = 8 RETURNING "n", "isStaff", "note""#,
                params_entry,
                in_txn,
            )
            .unwrap_or_else(|e| panic!("{tag}: bare UPDATE … RETURNING must succeed: {e}"));
            assert_eq!(affected, 1, "{tag}: exactly one row updated");
            assert_eq!(rows.len(), 1, "{tag}: exactly one RETURNING row");
            assert_eq!(rows[0].values[0], Value::Int4(8), "{tag}: post-update value, as int4");
            assert_eq!(rows[0].values[1], Value::Boolean(true), "{tag}: a bool stays a bool");
            assert_eq!(
                rows[0].values[2],
                Value::String("hello".to_string()),
                "{tag}: text stays text"
            );

            // UPDATE arm, fully qualified.
            let (_, rows) = returning_in_mode(
                &db,
                r#"UPDATE "public"."Typed" SET "n" = 9 RETURNING "public"."Typed"."n""#,
                params_entry,
                in_txn,
            )
            .unwrap_or_else(|e| panic!("{tag}: qualified UPDATE … RETURNING must succeed: {e}"));
            assert_eq!(
                rows[0].values[0],
                Value::Int4(9),
                "{tag}: a fully qualified RETURNING reference must carry the row's value"
            );

            // INSERT arm (a DIFFERENT projection call site: src/lib.rs:15292).
            let (_, rows) = returning_in_mode(
                &db,
                r#"INSERT INTO "public"."Typed" ("id","isStaff","n","note")
                   VALUES (2, false, 20, 'two') RETURNING "n", "public"."Typed"."note""#,
                params_entry,
                in_txn,
            )
            .unwrap_or_else(|e| panic!("{tag}: INSERT … RETURNING must succeed: {e}"));
            assert_eq!(rows.len(), 1, "{tag}: INSERT … RETURNING returns the inserted row");
            assert_eq!(rows[0].values[0], Value::Int4(20), "{tag}: inserted int4");
            assert_eq!(
                rows[0].values[1],
                Value::String("two".to_string()),
                "{tag}: inserted text, qualified reference"
            );

            // DELETE arm (src/lib.rs:16067).
            let (_, rows) = returning_in_mode(
                &db,
                r#"DELETE FROM "public"."Typed" WHERE "id" = 2 RETURNING "n", "note""#,
                params_entry,
                in_txn,
            )
            .unwrap_or_else(|e| panic!("{tag}: DELETE … RETURNING must succeed: {e}"));
            assert_eq!(rows.len(), 1, "{tag}: DELETE … RETURNING returns the deleted row");
            assert_eq!(rows[0].values[0], Value::Int4(20), "{tag}: deleted int4");
        }
    }
}

/// POSITIVE CONTROL #2 — `RETURNING *` is the `ReturningItem::Wildcard` arm
/// (src/lib.rs:14832 / :14642) and must keep returning the whole row, in schema
/// order, before and after the fix. The fix plan changes the `Expression` arm
/// and adds a lowering for `col AS alias`; neither may touch this.
#[test]
fn gh23_control_returning_star_returns_the_whole_row() {
    for (params_entry, entry) in ENTRY_POINTS {
        let db = seeded_db();
        let (_, rows) = returning_in_mode(
            &db,
            r#"UPDATE "public"."Typed" SET "n" = 42 RETURNING *"#,
            params_entry,
            false,
        )
        .unwrap_or_else(|e| panic!("{entry}: UPDATE … RETURNING * must succeed: {e}"));
        assert_eq!(rows.len(), 1, "{entry}: one row");
        assert_eq!(
            rows[0].values.len(),
            4,
            "{entry}: RETURNING * must expand to all 4 columns"
        );
        assert_eq!(rows[0].values[0], Value::Int4(1), "{entry}: id");
        assert_eq!(rows[0].values[1], Value::Boolean(true), "{entry}: isStaff");
        assert_eq!(rows[0].values[2], Value::Int4(42), "{entry}: n, post-update");
        assert_eq!(rows[0].values[3], Value::String("hello".to_string()), "{entry}: note");
    }
}

// ---------------------------------------------------------------------------
// The aliased item: currently an `Expression`
// ---------------------------------------------------------------------------

/// An ALIASED RETURNING column must still carry the row's value.
///
/// PASSES on v4.31.1 — but only by luck: the qualifier here is spelled
/// `"Typed"` with quotes, so `normalize_ident` preserves the case and it
/// matches the catalog's stamped `source_table_name` byte-for-byte. It is here
/// so the fix for the OID half, which changes how `col AS alias` is lowered,
/// cannot regress the value on the spelling that works today.
#[test]
fn gh23_aliased_returning_column_carries_the_value() {
    for (params_entry, entry) in ENTRY_POINTS {
        for (in_txn, mode) in TXN_MODES {
            let tag = format!("{entry} / {mode}");
            let db = seeded_db();

            let (_, rows) = returning_in_mode(
                &db,
                r#"UPDATE "public"."Typed" SET "n" = 10
                   RETURNING "n" AS "cnt", "public"."Typed"."note" AS "label""#,
                params_entry,
                in_txn,
            )
            .unwrap_or_else(|e| panic!("{tag}: aliased UPDATE … RETURNING must succeed: {e}"));
            assert_eq!(rows.len(), 1, "{tag}: exactly one RETURNING row");
            assert_eq!(
                rows[0].values[0],
                Value::Int4(10),
                "{tag}: `\"n\" AS \"cnt\"` (unqualified) must carry the post-update value"
            );
            assert_eq!(
                rows[0].values[1],
                Value::String("hello".to_string()),
                "{tag}: `\"public\".\"Typed\".\"note\" AS \"label\"` must carry the value"
            );
        }
    }
}

/// FAILS on v4.31.1, on every entry point and both transaction modes.
///
/// An UNQUOTED qualifier is case-folded (`Typed."n"` → qualifier `typed`,
/// src/sql/planner.rs:329 + :4210-4214) and never matches the catalog's stamped
/// `source_table_name` (`Typed`, src/storage/catalog.rs:1019). WITHOUT an alias
/// the item lowers to `ReturningItem::Column` and the qualifier is simply
/// dropped, so it works — that is what the existing wire test
/// `returning_folded_and_alias_qualifiers_resolve_over_the_wire`
/// (src/protocol/postgres/wire_tests.rs:3652) pins. WITH an alias it lowers to
/// `ReturningItem::Expression` (src/sql/planner.rs:6534-6540),
/// `Evaluator::evaluate` fails the lookup (src/sql/evaluator.rs:265-271), and
/// `project_returning_columns` swallows the `Err` into `Value::Null`
/// (src/lib.rs:14658-14663).
///
/// The result is the worst possible shape: the field arrives under exactly the
/// name a client binds, carrying NULL for a `NOT NULL` column, with no error
/// anywhere. PostgreSQL raises 42P01/42703 for a qualifier that names no
/// relation in scope, so EITHER outcome is acceptable — return the value, or
/// refuse the statement — but silently substituting NULL is not. The assertion
/// is deliberately written that way so it does not over-constrain the fix.
///
/// NON-VACUITY: the `Ok` arm asserts the row count first, so a statement that
/// returned zero rows cannot pass this test by having nothing to check.
#[test]
fn gh23_case_folded_qualifier_with_alias_must_not_silently_return_null() {
    for (params_entry, entry) in ENTRY_POINTS {
        for (in_txn, mode) in TXN_MODES {
            let tag = format!("{entry} / {mode}");
            let db = seeded_db();

            let result = returning_in_mode(
                &db,
                r#"UPDATE "public"."Typed" SET "n" = 11 RETURNING Typed."n" AS "cnt""#,
                params_entry,
                in_txn,
            );

            match result {
                // Refusing the statement is fail-closed and PostgreSQL-shaped.
                Err(_) => {}
                Ok((affected, rows)) => {
                    assert_eq!(affected, 1, "{tag}: the UPDATE itself must have touched one row");
                    assert_eq!(rows.len(), 1, "{tag}: exactly one RETURNING row");
                    assert!(
                        !is_null(&rows[0].values[0]),
                        "{tag}: `RETURNING Typed.\"n\" AS \"cnt\"` silently returned NULL for a \
                         NOT NULL column — the case-folded qualifier missed \
                         Schema::get_qualified_column_index and project_returning_columns \
                         substituted Value::Null. It must return the value (11) or refuse the \
                         statement, never invent a NULL. Got {:?}",
                        rows[0].values[0]
                    );
                    assert_eq!(
                        rows[0].values[0],
                        Value::Int4(11),
                        "{tag}: …and if it returns a value it must be the post-update one"
                    );
                }
            }
        }
    }
}

/// FAILS on v4.31.1. The FROM-alias spelling of the same fail-open, on DELETE
/// — a different projection call site (src/lib.rs:16067) and a qualifier that
/// can never match the catalog stamp no matter how it is quoted.
///
/// `DELETE FROM "Typed" AS x … RETURNING x."n"` (no alias on the item) already
/// works, because the qualifier is dropped at plan time. Adding `AS "c"` routes
/// the very same reference through `Evaluator::evaluate`, whose schema is
/// stamped `Typed`, not `x` — so the deleted row's value is replaced by NULL
/// under the name the client binds.
#[test]
fn gh23_from_alias_qualifier_with_item_alias_must_not_silently_return_null() {
    for (params_entry, entry) in ENTRY_POINTS {
        for (in_txn, mode) in TXN_MODES {
            let tag = format!("{entry} / {mode}");
            let db = seeded_db();

            // Control leg, same statement WITHOUT the item alias: proves the
            // FROM-alias spelling is otherwise supported, so a failure below
            // cannot be blamed on `DELETE … AS x` being unsupported.
            let (_, rows) = returning_in_mode(
                &db,
                r#"DELETE FROM "Typed" AS x WHERE "id" = 1 RETURNING x."n""#,
                params_entry,
                in_txn,
            )
            .unwrap_or_else(|e| panic!("{tag}: CONTROL: DELETE … AS x RETURNING x.\"n\" must succeed: {e}"));
            assert_eq!(rows.len(), 1, "{tag}: CONTROL: one deleted row");
            assert_eq!(
                rows[0].values[0],
                Value::Int4(7),
                "{tag}: CONTROL: an unaliased FROM-alias-qualified item carries the value"
            );

            // The defect leg.
            let db = seeded_db();
            let result = returning_in_mode(
                &db,
                r#"DELETE FROM "Typed" AS x WHERE "id" = 1 RETURNING x."n" AS "c""#,
                params_entry,
                in_txn,
            );
            match result {
                Err(_) => {}
                Ok((affected, rows)) => {
                    assert_eq!(affected, 1, "{tag}: the DELETE itself must have removed one row");
                    assert_eq!(rows.len(), 1, "{tag}: exactly one RETURNING row");
                    assert!(
                        !is_null(&rows[0].values[0]),
                        "{tag}: `RETURNING x.\"n\" AS \"c\"` silently returned NULL for a NOT NULL \
                         column — the FROM-alias qualifier missed the catalog's stamped \
                         source_table_name. Got {:?}",
                        rows[0].values[0]
                    );
                    assert_eq!(
                        rows[0].values[0],
                        Value::Int4(7),
                        "{tag}: …and the value must be the deleted row's"
                    );
                }
            }
        }
    }
}
