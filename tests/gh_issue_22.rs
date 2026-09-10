//! GH #22 — "`INSERT … ON CONFLICT (col) DO UPDATE` inserts a duplicate instead
//! of updating", reproduced with the issue's VERBATIM SQL.
//!
//! Install as `tests/gh_issue_22.rs`.
//!
//! The reporter's script, byte-for-byte:
//!
//! ```sql
//! CREATE TABLE k (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE, n INT);
//! INSERT INTO k VALUES (1,'a',0);
//! INSERT INTO k (id, v, n) VALUES (2,'a',1) ON CONFLICT (v) DO UPDATE SET n = EXCLUDED.n;
//! SELECT * FROM k;   -- two rows with v='a'; expected one row (1,'a',1)
//! INSERT INTO k (id, v, n) VALUES (3,'a',2) ON CONFLICT ("v") DO UPDATE SET n = EXCLUDED.n; -- column '"v"' not found
//! ```
//!
//! # What this adds over `tests/prisma_p0_unique_on_conflict.rs`
//!
//! That file proves the fix (commit 79e2255) with ONE upsert per fresh table.
//! The issue's script runs TWO upserts back to back against the SAME row, first
//! with an unquoted target and then with the quoted spelling — and the second
//! one is the interesting one. The first upsert's DO UPDATE leg rewrites the
//! existing row and has to maintain the ART entries for it off the row's
//! PRE-IMAGE; commit 79e2255 records that this leg used to feed maintenance the
//! PROPOSED row's values instead, which erased the stored row's own index
//! entries — the row stayed visible to `SELECT *` but vanished from `WHERE v =
//! 'a'`, so the NEXT upsert found no conflict and appended a duplicate. Nothing
//! in the repo tests two consecutive upserts on one row, which is exactly the
//! shape a Prisma `upsert` in a request handler runs on every request.
//!
//! Each assertion therefore probes the row three ways after every statement:
//! `SELECT *` (row count), `WHERE v = 'a'` (the unique-index lookup) and
//! `WHERE id = 1` (the primary-key lookup). A fix that keeps the row in storage
//! but drops its index entries passes the first and fails the second.
//!
//! Both DML executor families are covered: `db.execute()` (text family →
//! `execute_in_transaction_inner`) and `db.execute_params()` (params family →
//! `execute_plan_with_params_inner`, which is what the PostgreSQL EXTENDED
//! protocol and the REST layer use — and what the issue's node-pg /
//! `@prisma/adapter-pg` client used). The two have SEPARATE ON CONFLICT legs
//! (src/lib.rs:5572 and src/lib.rs:15298), so a result on one says nothing about
//! the other.
//!
//! Expected on a FIXED tree: every test passes.
//! Expected on the tree the issue was filed against: `issue22_verbatim_*` fail
//! with "*** DUPLICATE INSERTED ***" or with `column '"v"' not found`, while
//! `issue22_positive_control_*` and the `DO NOTHING` control keep passing.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn mem_db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory database")
}

fn run(db: &EmbeddedDatabase, sql: &str, params_family: bool) -> heliosdb_nano::Result<u64> {
    if params_family {
        db.execute_params(sql, &[])
    } else {
        db.execute(sql)
    }
}

fn family(params_family: bool) -> &'static str {
    if params_family {
        "params"
    } else {
        "text"
    }
}

/// Rows physically present — the issue's `SELECT * FROM k`. Deliberately NOT
/// `SELECT COUNT(*)`, which returns one row whatever the count is.
fn rows_in(db: &EmbeddedDatabase, table: &str) -> usize {
    let sql = format!("SELECT * FROM {table}");
    db.query(&sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
        .len()
}

fn scalar(db: &EmbeddedDatabase, sql: &str) -> Value {
    db.query(sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
        .first()
        .and_then(|r| r.values.first().cloned())
        .unwrap_or(Value::Null)
}

fn scalar_int(db: &EmbeddedDatabase, sql: &str) -> i64 {
    match scalar(db, sql) {
        Value::Int2(v) => i64::from(v),
        Value::Int4(v) => i64::from(v),
        Value::Int8(v) => v,
        other => panic!("`{sql}` did not return an integer, got {other:?}"),
    }
}

fn row_count(db: &EmbeddedDatabase, sql: &str) -> usize {
    db.query(sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
        .len()
}

/// The message shape the PG wire maps to SQLSTATE 23505 unique_violation
/// (`sqlstate_for_error`, src/protocol/postgres/handler.rs:3556, keys 23505 on a
/// `ConstraintViolation` containing "duplicate key" or "unique constraint"; the
/// emitters write `Duplicate key value violates UNIQUE|PRIMARY KEY constraint
/// "<n>"`, src/storage/art_manager.rs:1818/1854).
///
/// Used instead of a bare `is_err()` wherever this file asserts that a write was
/// REFUSED: `is_err()` is satisfied by any error at all, including
/// "ON CONFLICT DO UPDATE: could not find existing row", which is the
/// fail-OPEN-adjacent bug this file is meant to catch, not a pass.
fn assert_unique_violation(err: &heliosdb_nano::Error, context: &str) {
    let text = err.to_string().to_ascii_lowercase();
    assert!(
        text.contains("duplicate key") || text.contains("unique constraint"),
        "{context}: the error must read as a UNIQUE violation (23505 on the wire), got: {err}"
    );
}

/// The state the issue says every one of its upserts must leave behind: exactly
/// one row, still row id 1, reachable by BOTH the unique-column lookup and the
/// primary-key lookup, carrying `n = expected_n`.
fn assert_single_row_updated_in_place(db: &EmbeddedDatabase, fam: &str, step: &str, expected_n: i64) {
    assert_eq!(
        rows_in(db, "k"),
        1,
        "[{fam}] {step}: *** DUPLICATE INSERTED *** ON CONFLICT DO UPDATE added a second row with v='a'"
    );
    assert_eq!(
        row_count(db, "SELECT id FROM k WHERE v = 'a'"),
        1,
        "[{fam}] {step}: the row is not reachable through its UNIQUE column — the upsert's index \
         maintenance dropped the row's ART entries (it is still in `SELECT *`, so this is not a \
         missing row, it is a missing index entry, and the NEXT upsert will not see a conflict)"
    );
    assert_eq!(
        row_count(db, "SELECT id FROM k WHERE id = 1"),
        1,
        "[{fam}] {step}: the row is not reachable through its PRIMARY KEY"
    );
    assert_eq!(
        scalar_int(db, "SELECT id FROM k WHERE v = 'a'"),
        1,
        "[{fam}] {step}: the EXISTING row must be updated, not replaced by the proposed one"
    );
    assert_eq!(
        scalar_int(db, "SELECT n FROM k WHERE v = 'a'"),
        expected_n,
        "[{fam}] {step}: EXCLUDED.n was not applied to the existing row"
    );
}

/// The issue's table, verbatim. `CREATE TABLE` has no params-family arm
/// (`LogicalPlan::CreateTable` is matched only at src/lib.rs:4996), so it runs on
/// the text family in both passes; every statement the issue is ABOUT runs on
/// the family under test.
fn issue_22_table(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE k (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE, n INT)")
        .expect("k");
}

// ===========================================================================
// 1. The issue's script, statement for statement
// ===========================================================================

/// The headline: two consecutive upserts on the same row, the first with an
/// unquoted conflict target and the second with the quoted one the issue says
/// failed with `column '"v"' not found`.
#[test]
fn issue22_verbatim_upsert_sequence_updates_in_place_on_both_families() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        issue_22_table(&db);

        // INSERT INTO k VALUES (1,'a',0);
        run(&db, "INSERT INTO k VALUES (1,'a',0)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] POSITIVE CONTROL: the seed row must insert: {e}"));
        assert_eq!(
            rows_in(&db, "k"),
            1,
            "[{fam}] POSITIVE CONTROL BROKEN: the seed row is missing"
        );

        // INSERT … ON CONFLICT (v) DO UPDATE SET n = EXCLUDED.n;
        run(
            &db,
            "INSERT INTO k (id, v, n) VALUES (2,'a',1) ON CONFLICT (v) DO UPDATE SET n = EXCLUDED.n",
            params_family,
        )
        .unwrap_or_else(|e| panic!("[{fam}] the unquoted-target upsert must succeed: {e}"));
        // Expected by the issue: one row (1,'a',1).
        assert_single_row_updated_in_place(&db, fam, "after ON CONFLICT (v)", 1);

        // INSERT … ON CONFLICT ("v") DO UPDATE SET n = EXCLUDED.n;
        run(
            &db,
            "INSERT INTO k (id, v, n) VALUES (3,'a',2) ON CONFLICT (\"v\") DO UPDATE SET n = EXCLUDED.n",
            params_family,
        )
        .unwrap_or_else(|e| {
            panic!(
                "[{fam}] the QUOTED conflict target must be equivalent to the unquoted one \
                 (the issue got `column '\"v\"' not found`): {e}"
            )
        });
        assert_single_row_updated_in_place(&db, fam, "after ON CONFLICT (\"v\")", 2);
    }
}

/// Same two statements, run in the reverse order (quoted first), so neither
/// spelling can be passing only because the other ran before it and warmed a
/// cache. The parameterized plan cache is keyed by SQL TEXT, so the two
/// spellings are two separate cache entries.
#[test]
fn issue22_quoted_target_first_then_unquoted_on_both_families() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        issue_22_table(&db);
        run(&db, "INSERT INTO k VALUES (1,'a',0)", params_family).unwrap();

        run(
            &db,
            "INSERT INTO k (id, v, n) VALUES (2,'a',5) ON CONFLICT (\"v\") DO UPDATE SET n = EXCLUDED.n",
            params_family,
        )
        .unwrap_or_else(|e| panic!("[{fam}] quoted target first: {e}"));
        assert_single_row_updated_in_place(&db, fam, "quoted first", 5);

        run(
            &db,
            "INSERT INTO k (id, v, n) VALUES (3,'a',6) ON CONFLICT (v) DO UPDATE SET n = EXCLUDED.n",
            params_family,
        )
        .unwrap_or_else(|e| panic!("[{fam}] unquoted target second: {e}"));
        assert_single_row_updated_in_place(&db, fam, "unquoted second", 6);
    }
}

/// The Prisma-shaped repeat: the same upsert issued round after round against
/// the same conflicting value, which is what an idempotent request handler does.
/// Each run must update the one row; the table must never grow. (The proposed
/// `id` advances each round so that every round conflicts on `v` ONLY — a
/// repeated `id` would also collide on the PRIMARY KEY and the round would then
/// be testing the non-target-collision path instead.)
#[test]
fn issue22_repeated_identical_upsert_never_grows_the_table() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        issue_22_table(&db);
        run(&db, "INSERT INTO k VALUES (1,'a',0)", params_family).unwrap();

        for round in 1..=4_i64 {
            let sql = format!(
                "INSERT INTO k (id, v, n) VALUES ({}, 'a', {round}) ON CONFLICT (v) DO UPDATE SET n = EXCLUDED.n",
                round + 1
            );
            run(&db, &sql, params_family).unwrap_or_else(|e| panic!("[{fam}] round {round}: {e}"));
            assert_single_row_updated_in_place(&db, fam, &format!("round {round}"), round);
        }
    }
}

// ===========================================================================
// 2. The arbiter — the fail-closed half
// ===========================================================================

/// The issue's own expectation: "`42P10` when no matching unique constraint
/// exists". A target that names no unique constraint must be REJECTED, not
/// silently arbitrated against whatever constraint happens to trip.
///
/// The message asserted here is the one
/// `sqlstate_for_query_execution_message` maps to `42P10`
/// (src/protocol/postgres/handler.rs:3766), so the embedded assertion and the
/// wire behaviour cannot drift apart.
#[test]
fn issue22_conflict_target_without_a_unique_constraint_is_rejected_42p10() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        issue_22_table(&db);
        run(&db, "INSERT INTO k VALUES (1,'a',0)", params_family).unwrap();

        let err = run(
            &db,
            "INSERT INTO k (id, v, n) VALUES (2,'b',1) ON CONFLICT (n) DO UPDATE SET v = EXCLUDED.v",
            params_family,
        )
        .err()
        .unwrap_or_else(|| panic!("[{fam}] a target matching no unique constraint must be rejected (42P10)"));
        assert!(
            err.to_string()
                .to_ascii_lowercase()
                .contains("no unique or exclusion constraint matching"),
            "[{fam}] the message must carry PostgreSQL's 42P10 wording (that string is what \
             `sqlstate_for_query_execution_message` maps to 42P10), got: {err}"
        );
        assert_eq!(
            rows_in(&db, "k"),
            1,
            "[{fam}] the rejected statement wrote a row anyway"
        );
    }
}

/// A collision on a constraint the clause did NOT name is an ordinary 23505, not
/// a blind update of a row the statement never arbitrated on. Here the proposed
/// row reuses the PRIMARY KEY (id = 1) but brings a brand-new `v`, so the only
/// conflict is on the PK while the arbiter names `v`.
#[test]
fn issue22_collision_on_a_non_target_constraint_is_not_swallowed() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        issue_22_table(&db);
        run(&db, "INSERT INTO k VALUES (1,'a',0)", params_family).unwrap();

        let err = run(
            &db,
            "INSERT INTO k (id, v, n) VALUES (1,'zzz',9) ON CONFLICT (v) DO UPDATE SET n = EXCLUDED.n",
            params_family,
        )
        .err()
        .unwrap_or_else(|| {
            panic!(
                "[{fam}] a PRIMARY KEY collision must still raise under `ON CONFLICT (v)` — the clause \
                 arbitrates on v only"
            )
        });
        // It must be the ORIGINAL 23505, re-raised. Not
        // "ON CONFLICT DO UPDATE: could not find existing row", which is what
        // the arbiter's `None` arm produces when it forgets it had a target —
        // a bare `is_err()` cannot tell those two apart, and only one of them
        // is the behaviour PostgreSQL specifies.
        assert_unique_violation(&err, &format!("[{fam}] PK collision under ON CONFLICT (v)"));
        assert_eq!(rows_in(&db, "k"), 1, "[{fam}] the rejected row was stored");
        assert_eq!(
            scalar_int(&db, "SELECT n FROM k WHERE id = 1"),
            0,
            "[{fam}] the untargeted collision silently updated the row"
        );
    }
}

// ===========================================================================
// 3. Positive controls — passing before AND after the fix
// ===========================================================================

/// The issue states `ON CONFLICT DO NOTHING` already worked. It must keep
/// working, and it must not swallow the row on a constraint it did not name.
#[test]
fn issue22_positive_control_do_nothing_still_works() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        issue_22_table(&db);
        run(&db, "INSERT INTO k VALUES (1,'a',0)", params_family).unwrap();

        run(
            &db,
            "INSERT INTO k (id, v, n) VALUES (2,'a',1) ON CONFLICT (v) DO NOTHING",
            params_family,
        )
        .unwrap_or_else(|e| panic!("[{fam}] ON CONFLICT DO NOTHING must not error: {e}"));
        assert_eq!(rows_in(&db, "k"), 1, "[{fam}] DO NOTHING inserted a duplicate");
        assert_eq!(
            scalar_int(&db, "SELECT n FROM k WHERE v = 'a'"),
            0,
            "[{fam}] DO NOTHING must not modify the existing row"
        );
    }
}

/// The harness is sound: a NON-conflicting row still inserts through the same
/// ON CONFLICT statement, and both rows are readable afterwards. Passes before
/// and after the fix; if it fails, the test file is broken, not the engine.
#[test]
fn issue22_positive_control_a_non_conflicting_upsert_still_inserts() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        issue_22_table(&db);
        run(&db, "INSERT INTO k VALUES (1,'a',0)", params_family).unwrap();

        run(
            &db,
            "INSERT INTO k (id, v, n) VALUES (2,'b',7) ON CONFLICT (v) DO UPDATE SET n = EXCLUDED.n",
            params_family,
        )
        .unwrap_or_else(|e| panic!("[{fam}] a non-conflicting upsert must INSERT: {e}"));
        assert_eq!(rows_in(&db, "k"), 2, "[{fam}] the new row did not land");
        assert_eq!(scalar_int(&db, "SELECT n FROM k WHERE v = 'b'"), 7);
        assert_eq!(
            scalar_int(&db, "SELECT n FROM k WHERE v = 'a'"),
            0,
            "[{fam}] the untouched row was modified"
        );
    }
}

// ===========================================================================
// 4. The THIRD entry point: `execute_params_returning`
// ===========================================================================

/// Prisma's `upsert` does not just emit `ON CONFLICT … DO UPDATE`; it emits it
/// with `RETURNING`, and `@prisma/adapter-pg` reads the returned row back as the
/// record it hands the caller. Over the wire that lands on
/// `EmbeddedDatabase::execute_params_returning` (src/lib.rs:14596), a THIRD
/// entry point next to `execute()` and `execute_params()` — and the one whose
/// output the ORM actually consumes.
///
/// Everything above asserts what is left in the TABLE. This asserts what comes
/// BACK: the returned row must be the arbitrated EXISTING row carrying the new
/// value (`project_returning_columns(&updated_tuple, …)`, src/lib.rs:15490), not
/// the proposed row the statement supplied. Returning the proposed row would
/// make an ORM believe it had created record id 2 while the database holds
/// record id 1 — the same duplicate the issue reports, only invisible until the
/// next read.
///
/// Bound parameters, not literals: this is the extended-protocol shape, and the
/// parameterized plan cache is keyed by SQL text, so a literal-SQL result proves
/// nothing about it.
#[test]
fn issue22_prisma_upsert_returning_hands_back_the_existing_row() {
    let db = mem_db();
    issue_22_table(&db);
    db.execute("INSERT INTO k VALUES (1,'a',0)").expect("seed");

    // Round 1: unquoted target, quoted target in round 2 — two distinct SQL
    // texts, therefore two distinct plan-cache entries.
    for (round, (sql, id, n)) in [
        (
            "INSERT INTO k (id, v, n) VALUES ($1, $2, $3) ON CONFLICT (v) DO UPDATE SET n = EXCLUDED.n RETURNING id, v, n",
            2_i32,
            11_i32,
        ),
        (
            "INSERT INTO k (id, v, n) VALUES ($1, $2, $3) ON CONFLICT (\"v\") DO UPDATE SET n = EXCLUDED.n RETURNING id, v, n",
            3_i32,
            22_i32,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let (count, rows) = db
            .execute_params_returning(
                sql,
                &[Value::Int4(id), Value::String("a".to_string()), Value::Int4(n)],
            )
            .unwrap_or_else(|e| {
                panic!(
                    "round {round}: the parameterized upsert with RETURNING must succeed \
                     (the issue got `column '\"v\"' not found` for the quoted target): {e}"
                )
            });

        assert_eq!(count, 1, "round {round}: an upsert affects exactly one row");
        assert_eq!(
            rows.len(),
            1,
            "round {round}: RETURNING must hand back exactly one row, got {}",
            rows.len()
        );
        let returned = rows.first().expect("one returned row");
        let returned_id = match returned.values.first() {
            Some(Value::Int2(v)) => i64::from(*v),
            Some(Value::Int4(v)) => i64::from(*v),
            Some(Value::Int8(v)) => *v,
            other => panic!("round {round}: RETURNING id was not an integer, got {other:?}"),
        };
        assert_eq!(
            returned_id, 1,
            "round {round}: *** WRONG ROW RETURNED *** RETURNING handed back the PROPOSED row \
             (id {id}) instead of the arbitrated EXISTING row (id 1) the upsert actually wrote"
        );
        let returned_n = match returned.values.get(2) {
            Some(Value::Int2(v)) => i64::from(*v),
            Some(Value::Int4(v)) => i64::from(*v),
            Some(Value::Int8(v)) => *v,
            other => panic!("round {round}: RETURNING n was not an integer, got {other:?}"),
        };
        assert_eq!(
            returned_n,
            i64::from(n),
            "round {round}: RETURNING n must be the value the upsert applied"
        );

        // …and the table agrees with what was returned.
        assert_single_row_updated_in_place(&db, "params-returning", &format!("round {round}"), i64::from(n));
    }
}

/// POSITIVE CONTROL for the RETURNING entry point, and it passes on BOTH trees:
/// a NON-conflicting parameterized INSERT … RETURNING hands back the row it just
/// inserted. If this fails, `execute_params_returning` itself is broken and the
/// test above is measuring the wrong thing.
#[test]
fn issue22_positive_control_params_returning_on_a_plain_insert() {
    let db = mem_db();
    issue_22_table(&db);

    let (count, rows) = db
        .execute_params_returning(
            "INSERT INTO k (id, v, n) VALUES ($1, $2, $3) RETURNING id, v, n",
            &[Value::Int4(5), Value::String("e".to_string()), Value::Int4(50)],
        )
        .expect("POSITIVE CONTROL BROKEN: a plain parameterized INSERT … RETURNING failed");

    assert_eq!(count, 1, "POSITIVE CONTROL BROKEN: one row must be inserted");
    assert_eq!(rows.len(), 1, "POSITIVE CONTROL BROKEN: one row must be returned");
    assert_eq!(
        rows.first().and_then(|r| r.values.get(1)),
        Some(&Value::String("e".to_string())),
        "POSITIVE CONTROL BROKEN: RETURNING did not hand back the inserted value"
    );
    assert_eq!(rows_in(&db, "k"), 1, "POSITIVE CONTROL BROKEN: the row did not land");
}
