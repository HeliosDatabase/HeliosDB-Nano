//! Prisma P0 — UNIQUE enforcement for every spelling, ON CONFLICT that never
//! inserts a duplicate, and FOREIGN KEY targets validated at DDL time.
//!
//! Sprinter: 73203474ba7f (UNIQUE spellings), 96b05d23555c (ON CONFLICT),
//! cab2236d7ebe (FK at DDL).
//!
//! # What was broken, and why it looked random
//!
//! `ArtIndexManager` keeps ONE GLOBAL map of indexes keyed by index NAME, and
//! `Catalog::create_table` registered a column-level `UNIQUE` index under the
//! BARE COLUMN NAME. So the first table in a database to declare `UNIQUE email`
//! took the name `email`; every later table's registration failed with
//! `IndexAlreadyExists`, which that loop logged at warn and swallowed. With no
//! index registered there was nothing for `check_unique_constraints` to probe,
//! so the second table's UNIQUE constraint was enforced by NOTHING — on every
//! write path, on both executor families.
//!
//! That is why the Prisma spike saw `u1 (v VARCHAR UNIQUE)` reject duplicates
//! while `u2 (…, UNIQUE (v))` accepted them, and why the portal saw a fresh
//! `"O2"."login"` accept duplicates that an older, identical `"O1"."login"`
//! rejected. It is not the spelling and not the column order: it is whether some
//! EARLIER table in the same database already claimed the column name. Every
//! test below therefore creates the "first claimant" table first — remove it and
//! the test passes even against the unfixed tree, which is exactly the trap that
//! let this ship.
//!
//! Two more spellings had no enforcement at all, for their own reasons:
//!   * `CREATE UNIQUE INDEX` — the planner discarded sqlparser's `unique` flag,
//!     so it built an ordinary secondary index. Prisma emits this for EVERY
//!     `@unique` and `@@unique`.
//!   * `ALTER TABLE … ADD CONSTRAINT … UNIQUE` — the planner rejected it
//!     outright ("Unsupported ALTER TABLE operation: AddConstraint(Unique …)").
//!
//! And `INSERT … ON CONFLICT (target)` discarded the target entirely: a target
//! matching no unique constraint was accepted (PostgreSQL: 42P10), and the
//! DO UPDATE leg located the existing row by walking the column-level `unique`
//! FLAG plus an `Int4`/`Int8`-only primary-key fallback — blind to composite
//! constraints, to `CREATE UNIQUE INDEX`, and to UUID/TEXT primary keys.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn mem_db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory database")
}

/// Run one statement through the requested executor family.
///
/// `false` = the text family (`db.execute()` → `execute_in_transaction_inner`:
/// psql simple query, MySQL wire, embedded). `true` = the params family
/// (`db.execute_params()` → `execute_plan_with_params_inner`: the PostgreSQL
/// EXTENDED protocol every real driver uses, plus REST/BaaS).
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

/// Rows physically present. Deliberately NOT `SELECT COUNT(*)`: a count query
/// returns one row whether the count is 0 or 10,000.
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

/// Width-agnostic integer read: an `INT` column can surface as `Int2`/`Int4`/
/// `Int8` depending on the path that produced it, and this file is testing
/// constraints, not integer widths.
fn scalar_int(db: &EmbeddedDatabase, sql: &str) -> i64 {
    match scalar(db, sql) {
        Value::Int2(v) => i64::from(v),
        Value::Int4(v) => i64::from(v),
        Value::Int8(v) => v,
        other => panic!("`{sql}` did not return an integer, got {other:?}"),
    }
}

fn scalar_text(db: &EmbeddedDatabase, sql: &str) -> String {
    match scalar(db, sql) {
        Value::String(s) => s,
        other => other.to_string(),
    }
}

/// The message shape the PG wire maps to SQLSTATE 23505 unique_violation
/// (`sqlstate_for_error`: a `ConstraintViolation` containing "duplicate key" or
/// "unique constraint"). Asserting the SHAPE here is what makes the wire tests
/// in `src/protocol/postgres/wire_tests.rs` and these agree.
fn assert_unique_violation(err: &heliosdb_nano::Error, context: &str) {
    let text = err.to_string().to_ascii_lowercase();
    // Anchored on the two phrases `sqlstate_for_error` keys 23505 on, plus the
    // "unique index" wording of the CREATE UNIQUE INDEX backfill (which also
    // carries "duplicate key"). Deliberately NOT a bare `contains("unique")`:
    // the unfixed tree's `Unsupported ALTER TABLE operation: AddConstraint(
    // Unique { … })` contains that word and would pass this assertion for
    // entirely the wrong reason.
    assert!(
        text.contains("duplicate key") || text.contains("unique constraint"),
        "{context}: the error must read as a UNIQUE violation (23505 on the wire), got: {err}"
    );
}

/// The FIRST claimant of the column name `v`. Its presence is what makes every
/// "second table" case below reproduce the real defect — see the module docs.
fn seed_first_claimant(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE u1 (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE)")
        .expect("u1");
    db.execute("INSERT INTO u1 (id, v) VALUES (1, 'a')").expect("u1 seed");
}

// ===========================================================================
// 1. Every spelling rejects a duplicate — INSERT, UPDATE, multi-row INSERT
// ===========================================================================

/// `u2`: table-level `UNIQUE (v)` on a table whose column name a previous table
/// already claimed. ACCEPTED the duplicate before the fix (spike case).
#[test]
fn table_level_unique_rejects_a_duplicate_on_both_families() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        seed_first_claimant(&db);
        db.execute("CREATE TABLE u2 (id INT PRIMARY KEY, v VARCHAR(50), UNIQUE (v))")
            .unwrap();
        run(&db, "INSERT INTO u2 (id, v) VALUES (1, 'a')", params_family).unwrap();

        let err = run(&db, "INSERT INTO u2 (id, v) VALUES (2, 'a')", params_family)
            .err()
            .unwrap_or_else(|| {
                panic!("[{fam}] *** UNENFORCED CONSTRAINT *** a duplicate v was accepted by `UNIQUE (v)`")
            });
        assert_unique_violation(&err, fam);
        assert_eq!(rows_in(&db, "u2"), 1, "[{fam}] the duplicate row was stored anyway");
    }
}

/// `u4`: `CREATE UNIQUE INDEX` — the statement Prisma emits for every `@unique`.
/// Built an ordinary index (no enforcement) before the fix.
#[test]
fn create_unique_index_rejects_a_duplicate_on_both_families() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE u4 (id INT PRIMARY KEY, v VARCHAR(50))")
            .unwrap();
        // The DDL itself runs on the family under test: `CREATE INDEX` reaches
        // the SHARED `plan_to_operator` arm from both.
        run(&db, "CREATE UNIQUE INDEX u4_v ON u4 (v)", params_family).unwrap();
        run(&db, "INSERT INTO u4 (id, v) VALUES (1, 'a')", params_family).unwrap();

        let err = run(&db, "INSERT INTO u4 (id, v) VALUES (2, 'a')", params_family)
            .err()
            .unwrap_or_else(|| {
                panic!("[{fam}] *** UNENFORCED CONSTRAINT *** CREATE UNIQUE INDEX accepted a duplicate")
            });
        assert_unique_violation(&err, fam);
        assert_eq!(rows_in(&db, "u4"), 1, "[{fam}] the duplicate row was stored anyway");

        // A distinct value still inserts — the index is not rejecting everything
        // (which would make the assertion above pass for the wrong reason).
        run(&db, "INSERT INTO u4 (id, v) VALUES (3, 'b')", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] a distinct value must still be accepted: {e}"));
        assert_eq!(rows_in(&db, "u4"), 2);
    }
}

/// `u5`: `ALTER TABLE … ADD CONSTRAINT … UNIQUE` — rejected at plan time before
/// the fix ("Unsupported ALTER TABLE operation"), so migrations that add unique
/// constraints as a trailing step could not run at all.
#[test]
fn alter_table_add_unique_rejects_a_duplicate_on_both_families() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE u5 (id INT PRIMARY KEY, v VARCHAR(50))")
            .unwrap();
        run(&db, "ALTER TABLE u5 ADD CONSTRAINT u5_v_key UNIQUE (v)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] ALTER TABLE … ADD CONSTRAINT … UNIQUE must be supported: {e}"));
        run(&db, "INSERT INTO u5 (id, v) VALUES (1, 'a')", params_family).unwrap();

        let err = run(&db, "INSERT INTO u5 (id, v) VALUES (2, 'a')", params_family)
            .err()
            .unwrap_or_else(|| panic!("[{fam}] *** UNENFORCED CONSTRAINT *** the added UNIQUE accepted a duplicate"));
        assert_unique_violation(&err, fam);
        assert_eq!(rows_in(&db, "u5"), 1, "[{fam}] the duplicate row was stored anyway");
    }
}

/// The portal's own shape (spec addendum, NANO-FINDING-unique-index): a table
/// with TWO inline UNIQUE columns, declared `NOT NULL UNIQUE` and
/// `UNIQUE NOT NULL`, created AFTER another table that already used the same
/// column names. `"O2"."login"` accepted duplicates on main.
#[test]
fn two_inline_unique_columns_after_an_identical_table_still_enforce() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute(
            "CREATE TABLE \"O1\" (\"id\" INT PRIMARY KEY, \"login\" VARCHAR(39) UNIQUE NOT NULL, \"n\" INTEGER)",
        )
        .unwrap();
        db.execute(
            "CREATE TABLE \"O2\" (\"id\" INT PRIMARY KEY, \"login\" VARCHAR(39) NOT NULL UNIQUE, \"acc\" VARCHAR(36) UNIQUE)",
        )
        .unwrap();

        run(
            &db,
            "INSERT INTO \"O1\" (\"id\", \"login\") VALUES (1, 'x')",
            params_family,
        )
        .unwrap();
        run(
            &db,
            "INSERT INTO \"O2\" (\"id\", \"login\", \"acc\") VALUES (1, 'x', 'acc-1')",
            params_family,
        )
        .unwrap();

        // Enforcement RIGHT AFTER CREATE TABLE, in the same session (the portal
        // reported a fresh table accepting duplicates while an older identical
        // one rejected them).
        let err = run(
            &db,
            "INSERT INTO \"O2\" (\"id\", \"login\", \"acc\") VALUES (2, 'x', 'acc-2')",
            params_family,
        )
        .err()
        .unwrap_or_else(|| panic!("[{fam}] *** UNENFORCED CONSTRAINT *** O2.login accepted a duplicate"));
        assert_unique_violation(&err, fam);

        // The SECOND unique column of the same table is enforced too.
        let err = run(
            &db,
            "INSERT INTO \"O2\" (\"id\", \"login\", \"acc\") VALUES (3, 'y', 'acc-1')",
            params_family,
        )
        .err()
        .unwrap_or_else(|| panic!("[{fam}] *** UNENFORCED CONSTRAINT *** O2.acc accepted a duplicate"));
        assert_unique_violation(&err, fam);

        assert_eq!(rows_in(&db, "\"O2\""), 1, "[{fam}] a rejected row was stored");
        // And O1 still enforces its own copy of the column name.
        assert!(
            run(
                &db,
                "INSERT INTO \"O1\" (\"id\", \"login\") VALUES (2, 'x')",
                params_family
            )
            .is_err(),
            "[{fam}] O1.login stopped being enforced"
        );
    }
}

/// Quoted identifiers in a table-level constraint. `Ident::to_string()` re-emits
/// the quotes, so `UNIQUE ("v")` used to record the column name `"v"` — which
/// matches no column, leaving the constraint attached to nothing. Every ORM
/// quotes its identifiers.
#[test]
fn quoted_table_level_unique_is_enforced_on_both_families() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE q1 (id INT PRIMARY KEY, \"v\" VARCHAR(50), UNIQUE (\"v\"))")
            .unwrap();
        run(&db, "INSERT INTO q1 (id, v) VALUES (1, 'a')", params_family).unwrap();
        assert!(
            run(&db, "INSERT INTO q1 (id, v) VALUES (2, 'a')", params_family).is_err(),
            "[{fam}] *** UNENFORCED CONSTRAINT *** UNIQUE (\"v\") accepted a duplicate"
        );
        assert_eq!(rows_in(&db, "q1"), 1);
    }
}

/// A quoted COMPOSITE table-level constraint — the `@@unique([a, b])` shape.
#[test]
fn quoted_composite_unique_is_enforced_on_both_families() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE q2 (id INT PRIMARY KEY, \"a\" INT, \"b\" INT, UNIQUE (\"a\", \"b\"))")
            .unwrap();
        run(&db, "INSERT INTO q2 (id, a, b) VALUES (1, 7, 8)", params_family).unwrap();
        assert!(
            run(&db, "INSERT INTO q2 (id, a, b) VALUES (2, 7, 8)", params_family).is_err(),
            "[{fam}] *** UNENFORCED CONSTRAINT *** UNIQUE (\"a\", \"b\") accepted a duplicate pair"
        );
        run(&db, "INSERT INTO q2 (id, a, b) VALUES (3, 7, 9)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] a distinct pair must still be accepted: {e}"));
        assert_eq!(rows_in(&db, "q2"), 2);
    }
}

/// UPDATE onto an existing value must be rejected for EVERY spelling — the
/// constraint is not insert-only. `CREATE UNIQUE INDEX` sets no schema flag and
/// writes no constraint record, so both the UPDATE fast path and
/// `enforce_unique_on_update` used to miss it entirely.
#[test]
fn update_onto_an_existing_value_is_rejected_for_every_spelling() {
    for params_family in [false, true] {
        let fam = family(params_family);
        for (label, setup) in [
            (
                "table-level UNIQUE (v)",
                "CREATE TABLE t (id INT PRIMARY KEY, v VARCHAR(50), UNIQUE (v))",
            ),
            (
                "CREATE UNIQUE INDEX",
                "CREATE TABLE t (id INT PRIMARY KEY, v VARCHAR(50))",
            ),
            (
                "ALTER TABLE ADD CONSTRAINT",
                "CREATE TABLE t (id INT PRIMARY KEY, v VARCHAR(50))",
            ),
        ] {
            let db = mem_db();
            seed_first_claimant(&db);
            db.execute(setup).unwrap();
            if label == "CREATE UNIQUE INDEX" {
                db.execute("CREATE UNIQUE INDEX t_v_uidx ON t (v)").unwrap();
            }
            if label == "ALTER TABLE ADD CONSTRAINT" {
                db.execute("ALTER TABLE t ADD CONSTRAINT t_v_key UNIQUE (v)").unwrap();
            }
            db.execute("INSERT INTO t (id, v) VALUES (1, 'a')").unwrap();
            db.execute("INSERT INTO t (id, v) VALUES (2, 'b')").unwrap();

            let err = run(&db, "UPDATE t SET v = 'a' WHERE id = 2", params_family)
                .err()
                .unwrap_or_else(|| panic!("[{fam}/{label}] *** UNENFORCED CONSTRAINT *** UPDATE created a duplicate"));
            assert_unique_violation(&err, &format!("{fam}/{label}"));
            assert_eq!(
                scalar_text(&db, "SELECT v FROM t WHERE id = 2"),
                "b",
                "[{fam}/{label}] the rejected UPDATE was applied anyway"
            );
        }
    }
}

/// A multi-row INSERT whose OWN rows collide is rejected, and no duplicate `v`
/// is ever stored — on BOTH families.
///
/// What uniqueness owns is asserted identically on both: the statement fails
/// with 23505, and afterwards `v = 'x'` matches AT MOST ONE row. Neither family
/// may leave the duplicate behind.
///
/// # Known divergence, deliberately NOT pinned as "expected"
///
/// Whole-statement ROLLBACK of an AUTOCOMMIT multi-row INSERT is real on the
/// text family only: `execute_in_transaction_inner` stages every row into the
/// implicit transaction, while the params family's Insert arm writes each row
/// straight to storage when no transaction is open (`active_txn == None`) and
/// so keeps the rows that landed before the failing one. That is a pre-existing
/// atomicity gap in the params family — it has nothing to do with uniqueness,
/// it is not what this change touches, and it is reported in the handback
/// rather than fixed here.
///
/// It is not asserted as the expected result either, because that would pin a
/// defect. Instead the spec's requirement — "a multi-row INSERT with an
/// internal duplicate rolls back entirely" — is proved on BOTH families inside
/// an EXPLICIT transaction (`explicit_transaction_rolls_back_a_self_colliding_multi_row_insert`),
/// where the params family does stage through the write set, so the divergence
/// is scoped to autocommit statement atomicity and cannot hide a uniqueness
/// regression.
#[test]
fn a_multi_row_insert_with_an_internal_duplicate_is_rejected() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        seed_first_claimant(&db);
        db.execute("CREATE TABLE m1 (id INT PRIMARY KEY, v VARCHAR(50), UNIQUE (v))")
            .unwrap();

        let err = run(
            &db,
            "INSERT INTO m1 (id, v) VALUES (1, 'x'), (2, 'y'), (3, 'x')",
            params_family,
        )
        .err()
        .unwrap_or_else(|| panic!("[{fam}] a self-colliding multi-row INSERT was accepted"));
        assert_unique_violation(&err, fam);
        let x_rows = db.query("SELECT id FROM m1 WHERE v = 'x'", &[]).unwrap().len();
        assert!(
            x_rows <= 1,
            "[{fam}] the duplicate row landed: `v = 'x'` now matches {x_rows} rows"
        );
        // Every row that DID land must be one of the statement's own rows and
        // must not exceed what the statement proposed — a family that leaks
        // more than it wrote is a regression on either atomicity model.
        assert!(
            rows_in(&db, "m1") <= 2,
            "[{fam}] the rejected multi-row INSERT left more rows behind than it proposed"
        );
        if !params_family {
            assert_eq!(
                rows_in(&db, "m1"),
                0,
                "[{fam}] the rejected multi-row INSERT left rows behind"
            );
        }
    }
}

/// Spec test 1, the rollback half, on BOTH families: inside an EXPLICIT
/// transaction a self-colliding multi-row INSERT rolls the whole unit back.
///
/// This is the family-parity proof the autocommit test above cannot give (see
/// its "Known divergence" section): with a transaction open, the params family's
/// Insert arm stages through `txn.put` + the ART undo log exactly as the text
/// family does, so ROLLBACK must leave the table empty on both.
#[test]
fn explicit_transaction_rolls_back_a_self_colliding_multi_row_insert() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        seed_first_claimant(&db);
        db.execute("CREATE TABLE m2 (id INT PRIMARY KEY, v VARCHAR(50), UNIQUE (v))")
            .unwrap();

        db.execute("BEGIN").unwrap();
        let err = run(
            &db,
            "INSERT INTO m2 (id, v) VALUES (1, 'x'), (2, 'y'), (3, 'x')",
            params_family,
        )
        .err()
        .unwrap_or_else(|| panic!("[{fam}] a self-colliding multi-row INSERT was accepted"));
        assert_unique_violation(&err, fam);
        db.execute("ROLLBACK").unwrap();

        assert_eq!(
            rows_in(&db, "m2"),
            0,
            "[{fam}] ROLLBACK left rows from the rejected multi-row INSERT behind"
        );
        // And the constraint still enforces afterwards: a rolled-back INSERT
        // must not leave phantom keys in the ART index either.
        db.execute("INSERT INTO m2 (id, v) VALUES (1, 'x')")
            .unwrap_or_else(|e| panic!("[{fam}] a key from the rolled-back INSERT is still in the index: {e}"));
        let err = db.execute("INSERT INTO m2 (id, v) VALUES (9, 'x')").err();
        assert!(
            err.is_some(),
            "[{fam}] the UNIQUE constraint stopped enforcing after a rolled-back INSERT"
        );
    }
}

/// PostgreSQL: NULLs are distinct under UNIQUE. Pinned for the spellings that
/// gained enforcement, so the fix cannot tighten a case PostgreSQL allows.
#[test]
fn nulls_stay_distinct_under_every_spelling() {
    for (label, setup, extra) in [
        (
            "table-level",
            "CREATE TABLE n1 (id INT PRIMARY KEY, v VARCHAR(50), UNIQUE (v))",
            None,
        ),
        (
            "unique index",
            "CREATE TABLE n1 (id INT PRIMARY KEY, v VARCHAR(50))",
            Some("CREATE UNIQUE INDEX n1_v ON n1 (v)"),
        ),
        (
            "alter add constraint",
            "CREATE TABLE n1 (id INT PRIMARY KEY, v VARCHAR(50))",
            Some("ALTER TABLE n1 ADD CONSTRAINT n1_v_key UNIQUE (v)"),
        ),
    ] {
        let db = mem_db();
        seed_first_claimant(&db);
        db.execute(setup).unwrap();
        if let Some(extra) = extra {
            db.execute(extra).unwrap();
        }
        db.execute("INSERT INTO n1 (id, v) VALUES (1, NULL)").unwrap();
        db.execute("INSERT INTO n1 (id, v) VALUES (2, NULL)")
            .unwrap_or_else(|e| panic!("[{label}] two NULLs must not collide under UNIQUE: {e}"));
        assert_eq!(rows_in(&db, "n1"), 2, "[{label}] a NULL-bearing row was rejected");
    }
}

/// The three NULL-under-UNIQUE spellings, but asking the question the row-count
/// assertion above CANNOT ask: is the second NULL row still INDEXED?
///
/// # Why the count alone was not enough
///
/// `Value::Null` encodes to the single ART key byte `0x00`, so every NULL in one
/// column produces the SAME key. Handing that key to a PK/UNIQUE tree makes the
/// SECOND NULL row a duplicate and the tree refuses it. That refusal used to be
/// swallowed by the post-write maintenance call site (`insert_tuple_fast` logs
/// and carries on), which is why "NULLs are distinct" looked correct — by
/// accident: the row was stored, the count was 2, and the only casualty was the
/// missing UNIQUE-index entry nobody probed for.
///
/// Adding the all-or-nothing undo turned that accident into data loss: the
/// refusal took back everything the row had already written — its PRIMARY KEY
/// entry first — so the stored row became invisible to every indexed lookup and
/// its primary key was free for a second row to claim. (That undo is now
/// confined to `RowState::NotStored`, the one state where the row really is
/// being unwound; see
/// `a_stored_row_keeps_its_primary_key_when_a_unique_index_refuses_it` below.
/// The NULL skip is the half that keeps the refusal from happening at all, and
/// this test pins that half.)
///
/// So this test pins the two consequences a row count cannot see:
///   * `WHERE id = <lit>` is answered from the PK ART tree
///     (`try_index_point_lookup_for_scan` probes it unconditionally for an
///     equality on an indexed column), so a row whose PK entry was undone
///     returns ZERO rows while the full scan still counts it; and
///   * the PK is still OWNED — a second row claiming it is rejected
///     (`check_unique_constraints_tuple` asks the same tree).
///
/// FAILS on the pre-fix tree: `SELECT id FROM n1 WHERE id = 2` returns 0 rows,
/// and `INSERT … VALUES (2, 'z')` is ACCEPTED, storing a duplicate primary key.
#[test]
fn a_second_null_row_stays_fully_indexed_under_every_spelling() {
    for (label, setup, extra) in [
        (
            "table-level",
            "CREATE TABLE n1 (id INT PRIMARY KEY, v VARCHAR(50), UNIQUE (v))",
            None,
        ),
        (
            "unique index",
            "CREATE TABLE n1 (id INT PRIMARY KEY, v VARCHAR(50))",
            Some("CREATE UNIQUE INDEX n1_v ON n1 (v)"),
        ),
        (
            "alter add constraint",
            "CREATE TABLE n1 (id INT PRIMARY KEY, v VARCHAR(50))",
            Some("ALTER TABLE n1 ADD CONSTRAINT n1_v_key UNIQUE (v)"),
        ),
        (
            "inline",
            "CREATE TABLE n1 (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE)",
            None,
        ),
    ] {
        for params_family in [false, true] {
            let fam = family(params_family);
            let db = mem_db();
            seed_first_claimant(&db);
            db.execute(setup).unwrap();
            if let Some(extra) = extra {
                db.execute(extra).unwrap();
            }

            // Three NULLs, not two: the third proves the skip is not a
            // one-shot "the first duplicate is forgiven" behaviour.
            for id in 1..=3 {
                run(
                    &db,
                    &format!("INSERT INTO n1 (id, v) VALUES ({id}, NULL)"),
                    params_family,
                )
                .unwrap_or_else(|e| {
                    panic!("[{label}/{fam}] NULL row {id} was rejected — NULLs are DISTINCT under UNIQUE: {e}")
                });
            }
            assert_eq!(
                rows_in(&db, "n1"),
                3,
                "[{label}/{fam}] a NULL-bearing row was not stored"
            );

            // Every stored row is still reachable through the PRIMARY KEY index.
            for id in 1..=3 {
                let sql = format!("SELECT id FROM n1 WHERE id = {id}");
                let found = db.query(&sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}")).len();
                assert_eq!(
                    found, 1,
                    "[{label}/{fam}] *** ROW UNINDEXED *** id = {id} is stored (the scan counts it) but the \
                     indexed lookup cannot see it: the NULL in `v` made the UNIQUE tree refuse the row and the \
                     all-or-nothing undo took its PRIMARY KEY entry back"
                );
            }

            // …and the primary key those rows hold is still THEIRS.
            let err = run(&db, "INSERT INTO n1 (id, v) VALUES (2, 'z')", params_family)
                .err()
                .unwrap_or_else(|| {
                    panic!(
                        "[{label}/{fam}] *** DUPLICATE PRIMARY KEY *** id = 2 was accepted a second time: the \
                         PK entry of the NULL-bearing row is missing from the index"
                    )
                });
            assert_unique_violation(&err, &format!("{label}/{fam}"));
            assert_eq!(
                rows_in(&db, "n1"),
                3,
                "[{label}/{fam}] the duplicate-PK row was stored anyway"
            );

            // The UNIQUE constraint still enforces for values that are NOT NULL
            // — the skip must be about NULL, not about giving up on the column.
            run(&db, "INSERT INTO n1 (id, v) VALUES (4, 'x')", params_family)
                .unwrap_or_else(|e| panic!("[{label}/{fam}] a distinct non-NULL value must be accepted: {e}"));
            let err = run(&db, "INSERT INTO n1 (id, v) VALUES (5, 'x')", params_family)
                .err()
                .unwrap_or_else(|| {
                    panic!("[{label}/{fam}] *** UNENFORCED CONSTRAINT *** a real duplicate of 'x' was accepted")
                });
            assert_unique_violation(&err, &format!("{label}/{fam}"));

            // A row may also be UPDATEd to NULL while another row already holds
            // NULL — `on_update`'s re-insert half goes through the same skip.
            run(&db, "UPDATE n1 SET v = NULL WHERE id = 4", params_family)
                .unwrap_or_else(|e| panic!("[{label}/{fam}] UPDATE … SET v = NULL onto an existing NULL: {e}"));
            let found = db.query("SELECT id FROM n1 WHERE id = 4", &[]).unwrap().len();
            assert_eq!(
                found, 1,
                "[{label}/{fam}] *** ROW UNINDEXED *** the row updated to NULL lost its PRIMARY KEY entry"
            );
        }
    }
}

/// MINOR 1 (fifth-pass review): the TRANSACTIONAL insert funnel is the one that
/// PROPAGATES the ART refusal instead of swallowing it, so before the NULL skip
/// a second NULL in a UNIQUE column did not merely lose its index entries — the
/// statement FAILED, with a duplicate-key error for a value PostgreSQL treats as
/// distinct from every other value including itself.
///
/// Path: `insert_validated_tuple_in_transaction_id` →
/// `insert_prepared_tuple_in_transaction` (`constraints_prechecked = false`) →
/// `on_insert_tuple_collect_index_values`, whose `Err` arm is
/// `return Err(Error::constraint_violation(…))`.
///
/// FAILS on the pre-fix tree at the second INSERT with
/// "Duplicate key value violates UNIQUE constraint".
#[test]
fn a_transaction_accepts_a_second_null_in_a_unique_column() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        seed_first_claimant(&db);
        db.execute("CREATE TABLE tn (id INT PRIMARY KEY, v VARCHAR(50), UNIQUE (v))")
            .unwrap();

        db.execute("BEGIN").unwrap();
        run(&db, "INSERT INTO tn (id, v) VALUES (1, NULL)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] the first NULL was rejected in a transaction: {e}"));
        run(&db, "INSERT INTO tn (id, v) VALUES (2, NULL)", params_family).unwrap_or_else(|e| {
            panic!(
                "[{fam}] *** SPURIOUS 23505 *** the transactional insert funnel rejected a legal second NULL \
                 in a UNIQUE column — NULLs are DISTINCT in PostgreSQL: {e}"
            )
        });
        db.execute("COMMIT").unwrap();

        assert_eq!(rows_in(&db, "tn"), 2, "[{fam}] a committed NULL row is missing");
        for id in [1, 2] {
            let sql = format!("SELECT id FROM tn WHERE id = {id}");
            let found = db.query(&sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}")).len();
            assert_eq!(
                found, 1,
                "[{fam}] *** ROW UNINDEXED *** committed row id = {id} is not reachable through the PK index"
            );
        }

        // A third NULL, this time in autocommit, on the table the transaction
        // built: the two funnels must agree.
        run(&db, "INSERT INTO tn (id, v) VALUES (3, NULL)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] a third NULL was rejected after COMMIT: {e}"));

        // And a REAL duplicate is still rejected, inside a transaction too.
        run(&db, "INSERT INTO tn (id, v) VALUES (4, 'k')", params_family).unwrap();
        db.execute("BEGIN").unwrap();
        let err = run(&db, "INSERT INTO tn (id, v) VALUES (5, 'k')", params_family)
            .err()
            .unwrap_or_else(|| panic!("[{fam}] *** UNENFORCED CONSTRAINT *** a real duplicate of 'k' was accepted"));
        assert_unique_violation(&err, fam);
        let _ = db.execute("ROLLBACK");
        assert_eq!(rows_in(&db, "tn"), 4, "[{fam}] row count drifted");
    }
}

// ===========================================================================
// 2/3. ALTER TABLE ADD CONSTRAINT: validation, DROP, durability
// ===========================================================================

/// Adding a UNIQUE constraint to a table that ALREADY holds duplicates must
/// FAIL (23505) and must not leave the constraint behind — PostgreSQL builds the
/// index first and fails if the data violates it.
#[test]
fn add_constraint_on_existing_duplicates_fails_and_adds_nothing() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE d1 (id INT PRIMARY KEY, v VARCHAR(50))")
            .unwrap();
        db.execute("INSERT INTO d1 (id, v) VALUES (1, 'a')").unwrap();
        db.execute("INSERT INTO d1 (id, v) VALUES (2, 'a')").unwrap();

        let err = run(&db, "ALTER TABLE d1 ADD CONSTRAINT d1_v_key UNIQUE (v)", params_family)
            .err()
            .unwrap_or_else(|| panic!("[{fam}] *** FALSE CONSTRAINT *** ADD CONSTRAINT succeeded over duplicate data"));
        assert_unique_violation(&err, fam);

        // Nothing was added: the table still behaves exactly as before.
        db.execute("INSERT INTO d1 (id, v) VALUES (3, 'a')")
            .unwrap_or_else(|e| panic!("[{fam}] a constraint that failed to be added is being enforced: {e}"));
        assert_eq!(rows_in(&db, "d1"), 3);

        // And the failed ADD did not leave a half-built index that a later
        // (valid) ADD would trip over.
        db.execute("DELETE FROM d1 WHERE id > 1").unwrap();
        run(&db, "ALTER TABLE d1 ADD CONSTRAINT d1_v_key UNIQUE (v)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] re-adding the constraint after cleanup must work: {e}"));
        assert!(
            db.execute("INSERT INTO d1 (id, v) VALUES (4, 'a')").is_err(),
            "[{fam}] the re-added constraint is not enforced"
        );
    }
}

/// ADD then DROP: the constraint really goes away (the ART index that ENFORCES
/// it must go too, or DROP CONSTRAINT reports success while rows keep being
/// rejected).
#[test]
fn add_then_drop_constraint_lets_duplicates_back_in() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE d2 (id INT PRIMARY KEY, v VARCHAR(50))")
            .unwrap();
        run(&db, "ALTER TABLE d2 ADD CONSTRAINT d2_v_key UNIQUE (v)", params_family).unwrap();
        db.execute("INSERT INTO d2 (id, v) VALUES (1, 'a')").unwrap();
        assert!(
            db.execute("INSERT INTO d2 (id, v) VALUES (2, 'a')").is_err(),
            "[{fam}] the constraint was not enforced before the drop"
        );

        run(&db, "ALTER TABLE d2 DROP CONSTRAINT d2_v_key", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] DROP CONSTRAINT must work on this family: {e}"));
        db.execute("INSERT INTO d2 (id, v) VALUES (2, 'a')")
            .unwrap_or_else(|e| panic!("[{fam}] a dropped UNIQUE constraint is still being enforced: {e}"));
        assert_eq!(rows_in(&db, "d2"), 2);
    }
}

/// Durability: an added constraint (and a `CREATE UNIQUE INDEX`) must survive a
/// reopen AND be backfilled with the pre-restart rows — the dangerous half-fix
/// registers the index only at DDL time, so after a restart duplicates of
/// existing rows sail through. Mirrors `composite_unique_tests`' durability
/// proof.
#[test]
fn added_constraint_and_unique_index_survive_a_reopen_backfilled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_str().unwrap().to_string();

    {
        let db = EmbeddedDatabase::new(&path).expect("open");
        db.execute("CREATE TABLE p1 (id INT PRIMARY KEY, v VARCHAR(50))")
            .unwrap();
        db.execute("ALTER TABLE p1 ADD CONSTRAINT p1_v_key UNIQUE (v)").unwrap();
        db.execute("INSERT INTO p1 (id, v) VALUES (1, 'a')").unwrap();

        db.execute("CREATE TABLE p2 (id INT PRIMARY KEY, w VARCHAR(50))")
            .unwrap();
        db.execute("CREATE UNIQUE INDEX p2_w_key ON p2 (w)").unwrap();
        db.execute("INSERT INTO p2 (id, w) VALUES (1, 'a')").unwrap();
    }

    let db = EmbeddedDatabase::new(&path).expect("reopen");
    assert_eq!(rows_in(&db, "p1"), 1, "the pre-restart row must still be there");
    assert_eq!(rows_in(&db, "p2"), 1);

    assert!(
        db.execute("INSERT INTO p1 (id, v) VALUES (2, 'a')").is_err(),
        "*** UNENFORCED AFTER RESTART *** the ADD CONSTRAINT UNIQUE did not survive the reopen \
         (or its index was registered but never backfilled)"
    );
    assert!(
        db.execute("INSERT INTO p2 (id, w) VALUES (2, 'a')").is_err(),
        "*** UNENFORCED AFTER RESTART *** the CREATE UNIQUE INDEX did not survive the reopen \
         (or its index was registered but never backfilled)"
    );
    assert_eq!(rows_in(&db, "p1"), 1, "a rejected row was stored");
    assert_eq!(rows_in(&db, "p2"), 1);

    // Still accepts genuinely new values — the reopened indexes are not
    // rejecting everything.
    db.execute("INSERT INTO p1 (id, v) VALUES (3, 'b')")
        .expect("new value p1");
    db.execute("INSERT INTO p2 (id, w) VALUES (3, 'b')")
        .expect("new value p2");
}

/// `DROP INDEX` on a user-created UNIQUE index is allowed (PostgreSQL: only an
/// index owned by a CONSTRAINT is undroppable), and it really stops enforcing.
#[test]
fn dropping_a_user_created_unique_index_stops_enforcement() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE d3 (id INT PRIMARY KEY, v VARCHAR(50))")
            .unwrap();
        db.execute("CREATE UNIQUE INDEX d3_v_key ON d3 (v)").unwrap();
        db.execute("INSERT INTO d3 (id, v) VALUES (1, 'a')").unwrap();
        assert!(db.execute("INSERT INTO d3 (id, v) VALUES (2, 'a')").is_err());

        run(&db, "DROP INDEX d3_v_key", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] a user-created UNIQUE index must be droppable: {e}"));
        db.execute("INSERT INTO d3 (id, v) VALUES (2, 'a')")
            .unwrap_or_else(|e| panic!("[{fam}] the dropped unique index is still enforcing: {e}"));
        assert_eq!(rows_in(&db, "d3"), 2);
    }
}

// ===========================================================================
// 4/5/6. ON CONFLICT
// ===========================================================================

/// Case 4 — the spike's exact statement: `ON CONFLICT ("v")` (quoted) against an
/// inline UNIQUE. Before the fix the table ended up holding BOTH rows.
#[test]
fn on_conflict_quoted_target_updates_the_existing_row_on_both_families() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        seed_first_claimant(&db);
        db.execute("CREATE TABLE oc (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE, n INT)")
            .unwrap();
        db.execute("INSERT INTO oc (id, v, n) VALUES (1, 'a', 1)").unwrap();

        run(
            &db,
            "INSERT INTO oc (id, v, n) VALUES (2, 'a', 2) ON CONFLICT (\"v\") DO UPDATE SET n = EXCLUDED.n",
            params_family,
        )
        .unwrap_or_else(|e| panic!("[{fam}] the upsert must succeed: {e}"));

        assert_eq!(
            rows_in(&db, "oc"),
            1,
            "[{fam}] *** DUPLICATE INSERTED *** ON CONFLICT added a second row with the same v"
        );
        assert_eq!(
            scalar_int(&db, "SELECT n FROM oc WHERE v = 'a'"),
            2,
            "[{fam}] EXCLUDED.n was not applied to the existing row"
        );
        assert_eq!(
            scalar_int(&db, "SELECT id FROM oc WHERE v = 'a'"),
            1,
            "[{fam}] the EXISTING row must be updated, not replaced by the proposed one"
        );
    }
}

/// Case 5 — the same upsert against a table-level UNIQUE, a `CREATE UNIQUE
/// INDEX`, and a composite constraint. Each must update, never duplicate.
#[test]
fn on_conflict_updates_against_every_unique_spelling() {
    for params_family in [false, true] {
        let fam = family(params_family);

        // (a) table-level UNIQUE (v), behind a first claimant of `v`.
        let db = mem_db();
        seed_first_claimant(&db);
        db.execute("CREATE TABLE oc2 (id INT PRIMARY KEY, v VARCHAR(50), n INT, UNIQUE (v))")
            .unwrap();
        db.execute("INSERT INTO oc2 (id, v, n) VALUES (1, 'a', 1)").unwrap();
        run(
            &db,
            "INSERT INTO oc2 (id, v, n) VALUES (2, 'a', 5) ON CONFLICT (v) DO UPDATE SET n = EXCLUDED.n",
            params_family,
        )
        .unwrap_or_else(|e| panic!("[{fam}] table-level upsert: {e}"));
        assert_eq!(rows_in(&db, "oc2"), 1, "[{fam}] table-level UNIQUE: duplicate inserted");
        assert_eq!(scalar_int(&db, "SELECT n FROM oc2 WHERE v = 'a'"), 5);

        // (b) CREATE UNIQUE INDEX.
        let db = mem_db();
        db.execute("CREATE TABLE oc3 (id INT PRIMARY KEY, v VARCHAR(50), n INT)")
            .unwrap();
        db.execute("CREATE UNIQUE INDEX oc3_v_key ON oc3 (v)").unwrap();
        db.execute("INSERT INTO oc3 (id, v, n) VALUES (1, 'a', 1)").unwrap();
        run(
            &db,
            "INSERT INTO oc3 (id, v, n) VALUES (2, 'a', 7) ON CONFLICT (v) DO UPDATE SET n = EXCLUDED.n",
            params_family,
        )
        .unwrap_or_else(|e| panic!("[{fam}] unique-index upsert: {e}"));
        assert_eq!(
            rows_in(&db, "oc3"),
            1,
            "[{fam}] CREATE UNIQUE INDEX: duplicate inserted"
        );
        assert_eq!(scalar_int(&db, "SELECT n FROM oc3 WHERE v = 'a'"), 7);

        // (c) composite UNIQUE (v, w) — spike case 14 (a Prisma upsert on a
        // composite unique). The old existing-row lookup only knew the
        // column-level `unique` flag, so this could not resolve the row at all.
        let db = mem_db();
        db.execute("CREATE TABLE oc4 (id INT PRIMARY KEY, v VARCHAR(50), w INT, n INT, UNIQUE (v, w))")
            .unwrap();
        db.execute("INSERT INTO oc4 (id, v, w, n) VALUES (1, 'a', 1, 1)")
            .unwrap();
        run(
            &db,
            "INSERT INTO oc4 (id, v, w, n) VALUES (2, 'a', 1, 9) ON CONFLICT (v, w) DO UPDATE SET n = EXCLUDED.n",
            params_family,
        )
        .unwrap_or_else(|e| panic!("[{fam}] composite upsert: {e}"));
        assert_eq!(rows_in(&db, "oc4"), 1, "[{fam}] composite UNIQUE: duplicate inserted");
        assert_eq!(scalar_int(&db, "SELECT n FROM oc4 WHERE v = 'a'"), 9);

        // …and a non-conflicting row on the same composite still inserts.
        run(
            &db,
            "INSERT INTO oc4 (id, v, w, n) VALUES (3, 'a', 2, 3) ON CONFLICT (v, w) DO UPDATE SET n = EXCLUDED.n",
            params_family,
        )
        .unwrap();
        assert_eq!(rows_in(&db, "oc4"), 2);
    }
}

/// An upsert on a table whose PRIMARY KEY is not an integer. The old
/// existing-row lookup decoded only `Int4`/`Int8` primary keys, so this failed
/// with "ON CONFLICT DO UPDATE: could not find existing row" — and a UUID/TEXT
/// primary key is what Prisma generates by default.
#[test]
fn on_conflict_works_with_a_text_primary_key() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE oc5 (id VARCHAR(36) PRIMARY KEY, login VARCHAR(39) UNIQUE, n INT)")
            .unwrap();
        db.execute("INSERT INTO oc5 (id, login, n) VALUES ('11111111-1111-1111-1111-111111111111', 'octo', 1)")
            .unwrap();

        run(
            &db,
            "INSERT INTO oc5 (id, login, n) VALUES ('22222222-2222-2222-2222-222222222222', 'octo', 42) \
             ON CONFLICT (login) DO UPDATE SET n = EXCLUDED.n",
            params_family,
        )
        .unwrap_or_else(|e| panic!("[{fam}] upsert on a TEXT primary key: {e}"));

        assert_eq!(rows_in(&db, "oc5"), 1, "[{fam}] the upsert inserted a duplicate login");
        assert_eq!(scalar_int(&db, "SELECT n FROM oc5 WHERE login = 'octo'"), 42);
        assert_eq!(
            scalar_text(&db, "SELECT id FROM oc5 WHERE login = 'octo'"),
            "11111111-1111-1111-1111-111111111111",
            "[{fam}] the EXISTING row must be the one updated"
        );
    }
}

/// A conflict target that matches no unique constraint is an ERROR (42P10),
/// not a silent "conflict on whatever trips first".
#[test]
fn on_conflict_on_a_non_unique_column_is_rejected() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE oc6 (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE, n INT)")
            .unwrap();
        db.execute("INSERT INTO oc6 (id, v, n) VALUES (1, 'a', 1)").unwrap();

        let err = run(
            &db,
            "INSERT INTO oc6 (id, v, n) VALUES (2, 'b', 2) ON CONFLICT (n) DO UPDATE SET v = EXCLUDED.v",
            params_family,
        )
        .err()
        .unwrap_or_else(|| panic!("[{fam}] a target matching no unique constraint must be rejected"));
        assert!(
            err.to_string()
                .to_ascii_lowercase()
                .contains("no unique or exclusion constraint matching"),
            "[{fam}] the message must be PostgreSQL's 42P10 wording, got: {err}"
        );
        assert_eq!(rows_in(&db, "oc6"), 1, "[{fam}] the rejected statement wrote a row");

        // DO NOTHING is validated the same way — silently swallowing the rows of
        // a mis-targeted statement is the worse failure.
        assert!(
            run(
                &db,
                "INSERT INTO oc6 (id, v, n) VALUES (3, 'c', 3) ON CONFLICT (n) DO NOTHING",
                params_family
            )
            .is_err(),
            "[{fam}] ON CONFLICT (non-unique) DO NOTHING must be rejected too"
        );
    }
}

/// Case 6 — `DO UPDATE … WHERE false`: no update, and (the part that matters) no
/// insert either. The proposed row must never land as a duplicate.
#[test]
fn on_conflict_do_update_where_false_changes_nothing() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        seed_first_claimant(&db);
        db.execute("CREATE TABLE oc7 (id INT PRIMARY KEY, v VARCHAR(50), n INT, UNIQUE (v))")
            .unwrap();
        db.execute("INSERT INTO oc7 (id, v, n) VALUES (1, 'a', 1)").unwrap();

        run(
            &db,
            "INSERT INTO oc7 (id, v, n) VALUES (2, 'a', 99) ON CONFLICT (v) DO UPDATE SET n = EXCLUDED.n WHERE 1 = 0",
            params_family,
        )
        .unwrap_or_else(|e| panic!("[{fam}] a WHERE-false upsert must not error: {e}"));

        assert_eq!(
            rows_in(&db, "oc7"),
            1,
            "[{fam}] *** DUPLICATE INSERTED *** by WHERE false"
        );
        assert_eq!(
            scalar_int(&db, "SELECT n FROM oc7 WHERE v = 'a'"),
            1,
            "[{fam}] WHERE false still applied the update"
        );
    }
}

// ===========================================================================
// 7. FOREIGN KEY targets validated at DDL time
// ===========================================================================

/// `REFERENCES nosuch(id)` must be rejected at DDL time (42P01), not accepted
/// and then enforced by nothing.
#[test]
fn a_foreign_key_to_a_missing_table_is_rejected_at_ddl_time() {
    let db = mem_db();
    let err = db
        .execute("CREATE TABLE fk10 (id INT PRIMARY KEY, p INT REFERENCES nosuch(id))")
        .err()
        .expect("*** UNENFORCEABLE CONSTRAINT *** a FK to a missing table was accepted");
    let text = err.to_string().to_ascii_lowercase();
    assert!(
        text.contains("nosuch") && text.contains("does not exist") && text.contains("relation"),
        "the message must be PostgreSQL's 42P01 wording (`relation \"nosuch\" does not exist`), got: {err}"
    );
    // The table was not left half-created with a bogus constraint.
    assert!(
        db.execute("INSERT INTO fk10 (id, p) VALUES (1, 1)").is_err(),
        "the rejected CREATE TABLE left a usable table behind"
    );
}

/// `REFERENCES t(nocol)` must be rejected too (42703).
#[test]
fn a_foreign_key_to_a_missing_column_is_rejected_at_ddl_time() {
    let db = mem_db();
    db.execute("CREATE TABLE fkp (id INT PRIMARY KEY, name TEXT)").unwrap();
    let err = db
        .execute("CREATE TABLE fk11 (id INT PRIMARY KEY, p INT REFERENCES fkp(nocol))")
        .err()
        .expect("*** UNENFORCEABLE CONSTRAINT *** a FK to a missing column was accepted");
    let text = err.to_string().to_ascii_lowercase();
    assert!(
        text.contains("column \"nocol\"") && text.contains("does not exist"),
        "the message must be PostgreSQL's 42703 wording, got: {err}"
    );
}

/// A self-reference stays legal — the table being created already exists in the
/// catalog by the time its constraints are registered.
#[test]
fn a_self_referencing_foreign_key_is_still_accepted() {
    let db = mem_db();
    db.execute("CREATE TABLE tree (id INT PRIMARY KEY, parent INT REFERENCES tree(id))")
        .expect("a self-referencing FK must stay legal");
    db.execute("INSERT INTO tree (id, parent) VALUES (1, NULL)").unwrap();
    db.execute("INSERT INTO tree (id, parent) VALUES (2, 1)").unwrap();
    assert!(
        db.execute("INSERT INTO tree (id, parent) VALUES (3, 99)").is_err(),
        "the self-referencing FK must actually be enforced"
    );
    assert_eq!(rows_in(&db, "tree"), 2);
}

/// The same two rejections through `ALTER TABLE … ADD FOREIGN KEY`, on both
/// executor families (the params family reaches the SAME shared body).
#[test]
fn alter_table_add_foreign_key_validates_its_target_on_both_families() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE fkc (id INT PRIMARY KEY, p INT)").unwrap();
        db.execute("CREATE TABLE fkq (id INT PRIMARY KEY, name TEXT)").unwrap();

        let err = run(
            &db,
            "ALTER TABLE fkc ADD CONSTRAINT fkc_p_fkey FOREIGN KEY (p) REFERENCES nosuch(id)",
            params_family,
        )
        .err()
        .unwrap_or_else(|| panic!("[{fam}] *** UNENFORCEABLE *** ALTER accepted a FK to a missing table"));
        assert!(
            err.to_string().to_ascii_lowercase().contains("does not exist"),
            "[{fam}] expected an undefined-relation error, got: {err}"
        );

        let err = run(
            &db,
            "ALTER TABLE fkc ADD CONSTRAINT fkc_p_fkey2 FOREIGN KEY (p) REFERENCES fkq(nocol)",
            params_family,
        )
        .err()
        .unwrap_or_else(|| panic!("[{fam}] *** UNENFORCEABLE *** ALTER accepted a FK to a missing column"));
        assert!(
            err.to_string().to_ascii_lowercase().contains("column \"nocol\""),
            "[{fam}] expected an undefined-column error, got: {err}"
        );

        // A valid one still works on this family.
        run(
            &db,
            "ALTER TABLE fkc ADD CONSTRAINT fkc_p_ok FOREIGN KEY (p) REFERENCES fkq(id)",
            params_family,
        )
        .unwrap_or_else(|e| panic!("[{fam}] a valid ALTER … ADD FOREIGN KEY must still work: {e}"));
    }
}

// ===========================================================================
// Addendum regressions (spec 01 addendum) — cheap pins for behaviour that is
// CORRECT on main and must stay correct.
// ===========================================================================

/// Reported as "the updated row vanishes from `=` lookups after ~8
/// UPDATE … RETURNING toggling a UNIQUE column between a value and NULL".
///
/// REAL, and not an index-maintenance defect at all: the second round fails
/// with a UNIQUE violation naming the table-level constraint, and the row
/// keeps the NULL. `UNIQUE (v)` is recorded twice (table-level record plus the
/// column-flag record `CREATE TABLE` derives from the flag the planner sets for
/// single-column table-level UNIQUE), so `enforce_unique_on_update` validated
/// the row twice against one column set and its (columns, values)-keyed
/// intra-statement dedup reported the first pass's own entry as a duplicate —
/// the row rejected as a duplicate of ITSELF. All four funnels (text/params ×
/// with/without RETURNING) fail identically; the ART index is clean throughout,
/// which is why the `=` lookup finds nothing rather than finding a stale row.
#[test]
fn toggling_a_unique_column_through_null_keeps_lookups_and_enforcement_consistent() {
    // Deliberately the FIRST claimant of `v` in this database, so the constraint
    // is enforced on the unfixed tree too and this pins index MAINTENANCE (the
    // reported symptom) rather than the registration fix.
    let db = mem_db();
    db.execute("CREATE TABLE tog (id INT PRIMARY KEY, v VARCHAR(50), UNIQUE (v))")
        .unwrap();
    db.execute("INSERT INTO tog (id, v) VALUES (1, 'tick')").unwrap();
    db.execute("INSERT INTO tog (id, v) VALUES (2, 'other')").unwrap();

    for round in 0..8 {
        let sql = if round % 2 == 0 {
            "UPDATE tog SET v = NULL WHERE id = 1 RETURNING id"
        } else {
            "UPDATE tog SET v = 'tick' WHERE id = 1 RETURNING id"
        };
        db.query(sql, &[])
            .unwrap_or_else(|e| panic!("round {round}: `{sql}` failed: {e}"));
    }

    // Ends on 'tick' (the last round, 7, is odd).
    let by_eq = db.query("SELECT id FROM tog WHERE v = 'tick'", &[]).unwrap().len();
    let by_like = db.query("SELECT id FROM tog WHERE v LIKE 'tick'", &[]).unwrap().len();
    assert_eq!(
        by_eq, by_like,
        "`=` and LIKE disagree after toggling a UNIQUE column through NULL: = {by_eq}, LIKE {by_like}"
    );
    assert_eq!(by_eq, 1, "the toggled row is invisible to an `=` lookup");

    // …and the constraint still enforces against the toggled value.
    assert!(
        db.execute("INSERT INTO tog (id, v) VALUES (3, 'tick')").is_err(),
        "the UNIQUE constraint stopped enforcing after the toggles"
    );
}

/// Reported as "`UPDATE … WHERE id = '<uuid>'` matches 0 rows on the simple
/// protocol". Not reproducible on main; pinned on both families.
#[test]
fn update_and_delete_by_a_uuid_literal_primary_key_affect_one_row() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE uu (id VARCHAR(36) PRIMARY KEY, n INT)")
            .unwrap();
        db.execute("INSERT INTO uu (id, n) VALUES ('3f1e5b2a-0000-4000-8000-000000000001', 1)")
            .unwrap();
        db.execute("INSERT INTO uu (id, n) VALUES ('3f1e5b2a-0000-4000-8000-000000000002', 2)")
            .unwrap();

        let updated = run(
            &db,
            "UPDATE uu SET n = 10 WHERE id = '3f1e5b2a-0000-4000-8000-000000000001'",
            params_family,
        )
        .unwrap();
        assert_eq!(updated, 1, "[{fam}] UPDATE by UUID literal affected {updated} rows");
        assert_eq!(
            scalar_int(
                &db,
                "SELECT n FROM uu WHERE id = '3f1e5b2a-0000-4000-8000-000000000001'"
            ),
            10
        );

        let deleted = run(
            &db,
            "DELETE FROM uu WHERE id = '3f1e5b2a-0000-4000-8000-000000000002'",
            params_family,
        )
        .unwrap();
        assert_eq!(deleted, 1, "[{fam}] DELETE by UUID literal affected {deleted} rows");
        assert_eq!(rows_in(&db, "uu"), 1);
    }
}

// ===========================================================================
// Anti-regression: the shapes that already worked must be untouched
// ===========================================================================

/// Inline `v UNIQUE` on the FIRST table to claim the name always worked, and the
/// composite constraint from v4.25.0 did too. Stated plainly so the fix cannot be
/// credited with them — or silently regress them.
#[test]
fn shapes_that_already_worked_still_work() {
    for (label, ddl, dup) in [
        (
            "inline UNIQUE",
            "CREATE TABLE s1 (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE)",
            "INSERT INTO s1 (id, v) VALUES (2, 'a')",
        ),
        (
            "composite UNIQUE",
            "CREATE TABLE s1 (id INT PRIMARY KEY, v VARCHAR(50), w INT, UNIQUE (v, w))",
            "INSERT INTO s1 (id, v, w) VALUES (2, 'a', 1)",
        ),
    ] {
        let db = mem_db();
        db.execute(ddl).unwrap();
        let seed = if label == "inline UNIQUE" {
            "INSERT INTO s1 (id, v) VALUES (1, 'a')"
        } else {
            "INSERT INTO s1 (id, v, w) VALUES (1, 'a', 1)"
        };
        db.execute(seed).unwrap();
        assert!(db.execute(dup).is_err(), "{label}: a duplicate must still be rejected");
        assert_eq!(rows_in(&db, "s1"), 1, "{label}: the duplicate was stored");
    }
}

// ===========================================================================
// The ON CONFLICT ARBITER: the target SELECTS a constraint, it does not merely
// document one
// ===========================================================================
//
// PostgreSQL's `ON CONFLICT (<cols>)` names the arbiter index. A collision on
// any OTHER constraint is an ordinary 23505 that the clause does not handle at
// all. HeliosDB validated the target at plan time and then discarded it, so both
// executor families upserted on "whatever index trips first" — with the PRIMARY
// KEY probed FIRST. Two silent wrongs followed: a PK collision was swallowed
// into an update on a statement that targeted a UNIQUE column, and a row
// colliding on BOTH constraints updated the PK's row instead of the target's.

/// A conflict on a NON-target constraint must raise 23505, not be upserted.
///
/// `oc (id PK, v UNIQUE)` holding `(1,'a',1)`; the statement proposes
/// `(1,'z',5)` — clean on the target `v`, colliding on the PRIMARY KEY.
/// PostgreSQL raises `duplicate key value violates unique constraint "oc_pkey"`.
/// Before the arbiter existed, `find_unique_conflict` probed the PK FIRST,
/// reported that conflict, and the DO UPDATE leg silently rewrote row 1.
#[test]
fn on_conflict_on_a_non_target_constraint_raises_instead_of_upserting() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE oc (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE, n INT)")
            .unwrap();
        db.execute("INSERT INTO oc (id, v, n) VALUES (1, 'a', 1)").unwrap();

        let err = run(
            &db,
            "INSERT INTO oc (id, v, n) VALUES (1, 'z', 5) ON CONFLICT (v) DO UPDATE SET n = EXCLUDED.n",
            params_family,
        )
        .err()
        .unwrap_or_else(|| panic!("[{fam}] *** FAIL-OPEN *** a PRIMARY KEY conflict was swallowed by ON CONFLICT (v)"));
        assert_unique_violation(&err, fam);

        // Nothing was written, and in particular row 1 was NOT updated to the
        // proposed `n` — the wrong-row write this arbiter exists to stop.
        assert_eq!(rows_in(&db, "oc"), 1, "[{fam}] the rejected upsert stored a row");
        assert_eq!(
            scalar_int(&db, "SELECT n FROM oc WHERE id = 1"),
            1,
            "[{fam}] a row the statement never targeted was updated"
        );
        assert_eq!(scalar_text(&db, "SELECT v FROM oc WHERE id = 1"), "a");
    }
}

/// A row colliding on BOTH the PK and the target updates the TARGET's row.
///
/// `oc` holding `(1,'a',1)` and `(2,'b',2)`; the statement proposes `(2,'a',9)`,
/// which collides with row 2 on the PK and with row 1 on `v`. PostgreSQL
/// arbitrates on `(v)` only, so it updates ROW 1. The pre-arbiter code returned
/// the PK conflict and rewrote ROW 2 — a different row than the statement named.
#[test]
fn on_conflict_updates_the_targets_row_not_the_primary_keys() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE oc (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE, n INT)")
            .unwrap();
        db.execute("INSERT INTO oc (id, v, n) VALUES (1, 'a', 1)").unwrap();
        db.execute("INSERT INTO oc (id, v, n) VALUES (2, 'b', 2)").unwrap();

        run(
            &db,
            "INSERT INTO oc (id, v, n) VALUES (2, 'a', 9) ON CONFLICT (v) DO UPDATE SET n = EXCLUDED.n",
            params_family,
        )
        .unwrap_or_else(|e| panic!("[{fam}] the upsert on the target constraint was rejected: {e}"));

        assert_eq!(rows_in(&db, "oc"), 2, "[{fam}] the upsert inserted a third row");
        assert_eq!(
            scalar_int(&db, "SELECT n FROM oc WHERE id = 1"),
            9,
            "[{fam}] the row the ON CONFLICT target names (v = 'a', id = 1) was not the one updated"
        );
        assert_eq!(
            scalar_int(&db, "SELECT n FROM oc WHERE id = 2"),
            2,
            "[{fam}] *** WRONG ROW *** the PRIMARY KEY's row was updated instead of the target's"
        );
    }
}

/// `DO NOTHING` arbitrates too: `ON CONFLICT (v) DO NOTHING` does not swallow a
/// PRIMARY KEY collision. PostgreSQL raises 23505; skipping the row would drop a
/// write the user never told us to drop.
#[test]
fn on_conflict_do_nothing_does_not_swallow_a_non_target_conflict() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE ocn (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE, n INT)")
            .unwrap();
        db.execute("INSERT INTO ocn (id, v, n) VALUES (1, 'a', 1)").unwrap();

        let err = run(
            &db,
            "INSERT INTO ocn (id, v, n) VALUES (1, 'z', 5) ON CONFLICT (v) DO NOTHING",
            params_family,
        )
        .err()
        .unwrap_or_else(|| panic!("[{fam}] a PRIMARY KEY conflict was silently skipped by ON CONFLICT (v)"));
        assert_unique_violation(&err, fam);
        assert_eq!(rows_in(&db, "ocn"), 1);

        // …and a conflict ON the target is still skipped silently.
        run(
            &db,
            "INSERT INTO ocn (id, v, n) VALUES (7, 'a', 5) ON CONFLICT (v) DO NOTHING",
            params_family,
        )
        .unwrap_or_else(|e| panic!("[{fam}] a conflict on the NAMED target must be skipped, not raised: {e}"));
        assert_eq!(
            rows_in(&db, "ocn"),
            1,
            "[{fam}] DO NOTHING inserted the conflicting row"
        );
        assert_eq!(scalar_int(&db, "SELECT n FROM ocn WHERE id = 1"), 1);
    }
}

/// A TARGETLESS `ON CONFLICT DO UPDATE` still means "any unique constraint" —
/// the arbiter must not tighten the spelling that has no target. Pinned so the
/// fix cannot break the shape it does not own.
#[test]
fn a_targetless_on_conflict_still_matches_any_constraint() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE oct (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE, n INT)")
            .unwrap();
        db.execute("INSERT INTO oct (id, v, n) VALUES (1, 'a', 1)").unwrap();

        // Conflict on the PRIMARY KEY, no target named → upsert.
        run(
            &db,
            "INSERT INTO oct (id, v, n) VALUES (1, 'a', 7) ON CONFLICT DO UPDATE SET n = EXCLUDED.n",
            params_family,
        )
        .unwrap_or_else(|e| panic!("[{fam}] a targetless ON CONFLICT must still upsert: {e}"));
        assert_eq!(rows_in(&db, "oct"), 1);
        assert_eq!(scalar_int(&db, "SELECT n FROM oct WHERE id = 1"), 7);
    }
}

// ===========================================================================
// DROP CONSTRAINT must not unenforce a DIFFERENT constraint
// ===========================================================================

/// Two records, ONE index: `alter_table_add_unique` deliberately creates no
/// second index when the column set is already covered, so a redundant
/// constraint over an inline `UNIQUE` column shares that column's index.
/// Dropping the redundant record must leave the inline constraint enforcing.
///
/// Before the fix `drop_unique_constraint_indexes` tried every candidate NAME
/// and dropped each one that resolved (no `break`), and one candidate is always
/// the generated `{table}_{cols}_key` — exactly the inline UNIQUE's index. The
/// schema still said `v UNIQUE`, so the table went on advertising a constraint
/// that nothing enforced: duplicates landing silently.
#[test]
fn dropping_a_redundant_constraint_keeps_the_inline_unique_enforcing() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE dr (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE)")
            .unwrap();
        run(
            &db,
            "ALTER TABLE dr ADD CONSTRAINT dr_v_extra UNIQUE (v)",
            params_family,
        )
        .unwrap_or_else(|e| panic!("[{fam}] a redundant UNIQUE constraint could not be added: {e}"));
        run(&db, "ALTER TABLE dr DROP CONSTRAINT dr_v_extra", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] DROP CONSTRAINT failed: {e}"));

        db.execute("INSERT INTO dr (id, v) VALUES (1, 'a')").unwrap();
        let err = db.execute("INSERT INTO dr (id, v) VALUES (2, 'a')").err().unwrap_or_else(|| {
            panic!("[{fam}] *** UNENFORCED CONSTRAINT *** dropping a redundant constraint unenforced the inline UNIQUE")
        });
        assert_unique_violation(&err, fam);
        assert_eq!(rows_in(&db, "dr"), 1);
    }
}

/// The same shape one level down: dropping a constraint must never drop the
/// PRIMARY KEY's index because a candidate name happened to resolve to it, and
/// must never reach into another table.
#[test]
fn dropping_a_constraint_leaves_the_primary_key_and_other_tables_alone() {
    let db = mem_db();
    db.execute("CREATE TABLE pk1 (id INT PRIMARY KEY, v VARCHAR(50))")
        .unwrap();
    db.execute("CREATE TABLE pk2 (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE)")
        .unwrap();
    db.execute("ALTER TABLE pk1 ADD CONSTRAINT pk1_v_key UNIQUE (v)")
        .unwrap();
    db.execute("ALTER TABLE pk1 DROP CONSTRAINT pk1_v_key").unwrap();

    // pk1's own PK still enforces…
    db.execute("INSERT INTO pk1 (id, v) VALUES (1, 'a')").unwrap();
    assert!(
        db.execute("INSERT INTO pk1 (id, v) VALUES (1, 'b')").is_err(),
        "the PRIMARY KEY of the altered table stopped enforcing"
    );
    // …the dropped constraint really is gone…
    db.execute("INSERT INTO pk1 (id, v) VALUES (2, 'a')")
        .unwrap_or_else(|e| panic!("the dropped UNIQUE constraint is still enforcing: {e}"));
    // …and the OTHER table's UNIQUE is untouched.
    db.execute("INSERT INTO pk2 (id, v) VALUES (1, 'a')").unwrap();
    assert!(
        db.execute("INSERT INTO pk2 (id, v) VALUES (2, 'a')").is_err(),
        "DROP CONSTRAINT on one table unenforced another table's UNIQUE"
    );
}

// ===========================================================================
// RENAME TABLE must not desync a user index from its durable record
// ===========================================================================

/// Prisma names its indexes `"<Table>_<col>_key"` — the same PREFIX shape the
/// generated constraint namespace uses. Renaming the table used to rewrite that
/// live ART entry to `<NewTable>_<col>_key` while `Catalog::rename_table` left
/// the `meta:index:` record under the ORIGINAL name, so `DROP INDEX
/// "Account_email_key"` found no live index, deleted the record, and left the
/// renamed entry enforcing forever — an index nobody could name or drop.
///
/// PostgreSQL does not rename indexes on `ALTER TABLE … RENAME TO`, and neither
/// do we now: the name survives, the record follows the table.
#[test]
fn renaming_a_table_keeps_a_user_unique_index_nameable_and_enforcing() {
    let db = mem_db();
    db.execute(r#"CREATE TABLE "Account" ("id" INT PRIMARY KEY, "email" VARCHAR(50))"#)
        .unwrap();
    db.execute(r#"CREATE UNIQUE INDEX "Account_email_key" ON "Account" ("email")"#)
        .unwrap();
    db.execute(r#"ALTER TABLE "Account" RENAME TO "Account2""#).unwrap();

    // The index followed the table and still enforces on the new name.
    db.execute(r#"INSERT INTO "Account2" ("id", "email") VALUES (1, 'a@x')"#)
        .unwrap();
    assert!(
        db.execute(r#"INSERT INTO "Account2" ("id", "email") VALUES (2, 'a@x')"#)
            .is_err(),
        "the unique index stopped enforcing after RENAME TABLE"
    );

    // It is still reachable by the name the user gave it…
    db.execute(r#"DROP INDEX "Account_email_key""#)
        .unwrap_or_else(|e| panic!("*** UNNAMEABLE INDEX *** DROP INDEX by the original name failed: {e}"));

    // …and dropping it really stops the enforcement (proving the DROP hit the
    // live index and not just its record).
    db.execute(r#"INSERT INTO "Account2" ("id", "email") VALUES (2, 'a@x')"#)
        .unwrap_or_else(|e| panic!("*** ORPHAN INDEX *** a dropped index is still rejecting rows: {e}"));
    assert_eq!(rows_in(&db, r#""Account2""#), 2);
}

// ===========================================================================
// CREATE TABLE fails CLOSED — and leaves nothing behind when it does
// ===========================================================================

/// The fail-closed path must not create the table.
///
/// `create_table` refuses to report success for a UNIQUE column whose index it
/// could not register (the index IS the enforcement). That check used to sit
/// AFTER the WAL record, the schema, the schema cache and the row counter had
/// all been written, so the statement returned an error over a table that
/// `table_exists()` reported as present, with its UNIQUE column indexed by
/// nothing — and the retry then failed with "Table 'x' already exists".
///
/// Reproduced by squatting the generated constraint-index name from another
/// table (the ART registry is one GLOBAL map keyed by index name).
///
/// Not looped over the two DML families on purpose: `CREATE TABLE` has no
/// params-family arm (`LogicalPlan::CreateTable` is not routed through
/// `execute_plan_with_params_inner`), and `Catalog::create_table` is the ONE
/// funnel every interface reaches — wire, REPL, HTTP, embedded, dump/restore
/// and WAL recovery all land here, which is what the method's own W1.3 comment
/// records. There is no second implementation to diverge from.
#[test]
fn a_create_table_that_cannot_enforce_its_unique_leaves_no_table_behind() {
    let db = mem_db();
    db.execute("CREATE TABLE squat (id INT PRIMARY KEY, v VARCHAR(50))")
        .unwrap();
    // Claim exactly the name `CREATE TABLE t2 (… v UNIQUE)` would generate.
    db.execute("CREATE UNIQUE INDEX t2_v_key ON squat (v)").unwrap();

    let err = db
        .execute("CREATE TABLE t2 (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE)")
        .err()
        .expect("*** UNENFORCED CONSTRAINT *** CREATE TABLE succeeded without an index for its UNIQUE");
    assert!(
        err.to_string().to_ascii_lowercase().contains("unique"),
        "unexpected error: {err}"
    );

    // NOTHING was left behind: the table does not exist…
    assert!(
        db.query("SELECT * FROM t2", &[]).is_err(),
        "*** HALF-CREATED TABLE *** the failed CREATE TABLE left the table behind"
    );
    // …and the name is free, so a corrected statement can use it.
    db.execute("CREATE TABLE t2 (id INT PRIMARY KEY, v VARCHAR(50))")
        .unwrap_or_else(|e| panic!("the failed CREATE TABLE squatted the table name: {e}"));
    assert_eq!(rows_in(&db, "t2"), 0);
}

// ===========================================================================
// Index MAINTENANCE — the three write paths that rewrite a row in place
// ===========================================================================

/// After an upsert the row must still be findable by EVERY unique/PK column,
/// and no OTHER row may lose one of its entries.
///
/// The DO UPDATE leg fed the ART maintenance the PROPOSED row's values instead
/// of the pre-image of the row the arbiter picked. Two failures fell out of
/// that one argument, and this table shows both at once:
///
///  * the delete removed entries the PROPOSED values name — here `id = 9`,
///    which is nobody's; give it an id that exists (see
///    `on_conflict_updates_the_targets_row_not_the_primary_keys`) and it erases
///    a bystander row's primary-key entry; and
///  * it did NOT remove the updated row's own entries, so the re-insert hit
///    the still-present PRIMARY KEY entry, `on_insert` propagated that
///    duplicate-key error out of its loop, and every index registered after
///    the PK — BOTH unique columns here — kept the entry the delete had just
///    taken away. The row vanished from `WHERE v = 'a'` and `WHERE w = 'p'`
///    while `SELECT *` still returned it.
#[test]
fn an_upsert_leaves_the_row_findable_by_every_unique_column_on_both_families() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE up (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE, w VARCHAR(50) UNIQUE, n INT)")
            .unwrap();
        db.execute("INSERT INTO up (id, v, w, n) VALUES (1, 'a', 'p', 1)")
            .unwrap();
        db.execute("INSERT INTO up (id, v, w, n) VALUES (2, 'b', 'q', 2)")
            .unwrap();

        run(
            &db,
            "INSERT INTO up (id, v, w, n) VALUES (9, 'a', 'z', 5) ON CONFLICT (v) DO UPDATE SET n = EXCLUDED.n",
            params_family,
        )
        .unwrap_or_else(|e| panic!("[{fam}] the upsert must succeed: {e}"));

        assert_eq!(rows_in(&db, "up"), 2, "[{fam}] the upsert inserted a row");
        assert_eq!(
            scalar_int(&db, "SELECT n FROM up WHERE id = 1"),
            5,
            "[{fam}] the updated row is not findable by its PRIMARY KEY"
        );
        assert_eq!(
            scalar_int(&db, "SELECT n FROM up WHERE v = 'a'"),
            5,
            "[{fam}] *** ROW VANISHED *** not findable by the arbitrated unique column"
        );
        assert_eq!(
            scalar_int(&db, "SELECT n FROM up WHERE w = 'p'"),
            5,
            "[{fam}] *** ROW VANISHED *** not findable by the table's OTHER unique column"
        );

        // The row the statement never named kept all of its own entries.
        assert_eq!(
            scalar_int(&db, "SELECT n FROM up WHERE id = 2"),
            2,
            "[{fam}] a bystander row lost its primary-key entry"
        );
        assert_eq!(scalar_int(&db, "SELECT n FROM up WHERE v = 'b'"), 2);

        // …and both constraints still enforce afterwards.
        assert!(
            db.execute("INSERT INTO up (id, v, w, n) VALUES (3, 'a', 'r', 3)")
                .is_err(),
            "[{fam}] *** UNENFORCED CONSTRAINT *** v stopped enforcing after the upsert"
        );
        assert!(
            db.execute("INSERT INTO up (id, v, w, n) VALUES (4, 'c', 'p', 4)")
                .is_err(),
            "[{fam}] *** UNENFORCED CONSTRAINT *** w stopped enforcing after the upsert"
        );
    }
}

/// `UPDATE … RETURNING` with no open transaction is the ONE write funnel that
/// maintained no ART index at all (`update_tuples_branch_aware`): it wrote the
/// row, its versions, the WAL record, the MV/SMFI deltas and the HNSW index,
/// and left every ART entry pointing at the value the row used to hold.
///
/// Invisible while nothing probed those entries on UPDATE; the moment UNIQUE is
/// enforced there, the row's own stale entry rejects its own value.
#[test]
fn an_autocommit_returning_update_keeps_the_unique_index_in_step() {
    let db = mem_db();
    db.execute("CREATE TABLE ai (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE)")
        .unwrap();
    db.execute("INSERT INTO ai (id, v) VALUES (1, 'old')").unwrap();

    // RETURNING routes to the params family; with no transaction open that arm
    // writes through the generic autocommit funnel.
    db.query("UPDATE ai SET v = 'new' WHERE id = 1 RETURNING id", &[])
        .expect("the update must succeed");

    assert_eq!(
        db.query("SELECT id FROM ai WHERE v = 'new'", &[]).unwrap().len(),
        1,
        "*** ROW VANISHED *** the updated row is invisible to an `=` lookup on its new value"
    );
    // The value the row no longer holds must match nothing.
    assert!(
        db.query("SELECT id FROM ai WHERE v = 'old'", &[]).unwrap().is_empty(),
        "an `=` lookup still resolves the row by a value it no longer holds"
    );

    // The vacated value is free again…
    db.execute("INSERT INTO ai (id, v) VALUES (2, 'old')")
        .unwrap_or_else(|e| panic!("*** PHANTOM CONSTRAINT *** the vacated value is still taken: {e}"));
    // …and the new one is taken.
    let err = db
        .execute("INSERT INTO ai (id, v) VALUES (3, 'new')")
        .err()
        .expect("*** UNENFORCED CONSTRAINT *** the updated value stopped enforcing");
    assert_unique_violation(&err, "autocommit RETURNING update");
    assert_eq!(rows_in(&db, "ai"), 2);
}

/// `ALTER TABLE "T" RENAME TO "T2"` — the quoting every camelCase schema (and
/// Prisma) emits. The target went through `ObjectName::to_string()`, which
/// keeps the quote characters, so the table was renamed to the key `"T2"`
/// (quotes included) while the SOURCE name had been normalised. The rename
/// reported success and every later statement failed with `Table 'T2' does not
/// exist`: a table nothing could select from, insert into or drop.
#[test]
fn a_quoted_rename_to_keeps_the_table_addressable() {
    let db = mem_db();
    db.execute(r#"CREATE TABLE "Acct" ("id" INT PRIMARY KEY)"#).unwrap();
    db.execute(r#"INSERT INTO "Acct" ("id") VALUES (1)"#).unwrap();
    db.execute(r#"ALTER TABLE "Acct" RENAME TO "Acct2""#).unwrap();

    assert_eq!(rows_in(&db, r#""Acct2""#), 1, "the renamed table lost its rows");
    db.execute(r#"INSERT INTO "Acct2" ("id") VALUES (2)"#)
        .unwrap_or_else(|e| {
            panic!("*** UNADDRESSABLE TABLE *** RENAME TO \"Acct2\" produced a name no statement can use: {e}")
        });
    assert_eq!(rows_in(&db, r#""Acct2""#), 2);

    // The old name is gone, as in PostgreSQL.
    assert!(
        db.query(r#"SELECT * FROM "Acct""#, &[]).is_err(),
        "the old table name still resolves after RENAME TO"
    );
}

// ===========================================================================
// RENAME TABLE — the schema half of the target name, and the side records
//
// FAMILY NOTE for every test below: the rename STATEMENT is issued through
// `db.execute()` only. `ALTER TABLE … RENAME TO` has no params-family arm —
// `execute_alter_table_op` is reached only from the text family's
// `AlterTableMulti` arm, and `tests/rename_table_trigger_tests.rs::
// alter_table_rename_is_still_unimplemented_on_the_params_family` pins that gap
// deliberately. Both fixes here live BELOW that split — one in the planner arm
// that builds `LogicalPlan::AlterTableRename` (shared by both families' plan
// construction) and one in `Catalog::rename_table`, the single funnel every
// caller reaches — so they cover the params family the moment its arm exists.
// The DML that PROVES the constraints still enforce is run on BOTH families.
// ===========================================================================

/// `ALTER TABLE … RENAME TO <bare>` must keep the table in the schema it is
/// ALREADY in. PostgreSQL's `RENAME TO` never changes a relation's schema (that
/// is what `SET SCHEMA` is for) and its grammar takes only a bare name.
///
/// The target used to be resolved with `resolve_table_create`, which eagerly
/// prefixes the SESSION's `current_schema` onto any bare name — correct for
/// `CREATE TABLE` (the table does not exist yet), a silent SCHEMA MOVE for
/// `RENAME`. With `search_path = sess`, a `public` table renamed by a bare name
/// landed on the key `sess.<new>`: it vanished from `public`, `public.<new>`
/// resolved to nothing, and the statement reported success.
///
/// Both a QUALIFIED source (`public.qual`) and a BARE source that resolved to
/// `public` through the search-path fallback are covered — the defect is in the
/// TARGET's resolution, so it fires for either spelling of the source.
#[test]
fn a_bare_rename_keeps_a_public_table_in_public_under_a_search_path() {
    let db = mem_db();
    db.execute("CREATE SCHEMA sess").unwrap();
    // Both tables live in `public`.
    db.execute("CREATE TABLE qual (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO qual (id) VALUES (1)").unwrap();
    db.execute("CREATE TABLE bare (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO bare (id) VALUES (1)").unwrap();
    // …while the SESSION's current schema is a different one.
    db.execute("SET search_path TO sess").unwrap();

    db.execute("ALTER TABLE public.qual RENAME TO qual2")
        .unwrap_or_else(|e| panic!("qualified-source rename failed: {e}"));
    db.execute("ALTER TABLE bare RENAME TO bare2")
        .unwrap_or_else(|e| panic!("bare-source rename failed: {e}"));

    for name in ["qual2", "bare2"] {
        let sql = format!("SELECT id FROM public.{name}");
        assert_eq!(
            db.query(&sql, &[]).map(|r| r.len()).unwrap_or(0),
            1,
            "*** SILENT SCHEMA MOVE *** RENAME TO {name} moved the table out of `public`"
        );
        let stray = format!("SELECT id FROM sess.{name}");
        assert!(
            db.query(&stray, &[]).is_err(),
            "*** SILENT SCHEMA MOVE *** {name} landed in the session schema `sess`"
        );
    }

    // The old names are gone from `public`, as in PostgreSQL.
    assert!(db.query("SELECT id FROM public.qual", &[]).is_err());
    assert!(db.query("SELECT id FROM public.bare", &[]).is_err());
}

/// The mirror case: a table that lives in a NON-`public` session schema, renamed
/// with a BARE (quoted) target, stays in that schema and is addressable there —
/// bare AND qualified, with its quoted case intact.
#[test]
fn a_bare_quoted_rename_keeps_a_session_schema_table_in_its_schema() {
    let db = mem_db();
    db.execute("CREATE SCHEMA app").unwrap();
    db.execute("SET search_path TO app").unwrap();
    db.execute(r#"CREATE TABLE "Acct" ("id" INT PRIMARY KEY)"#).unwrap();
    db.execute(r#"INSERT INTO "Acct" ("id") VALUES (1)"#).unwrap();

    db.execute(r#"ALTER TABLE "Acct" RENAME TO "Acct2""#)
        .unwrap_or_else(|e| panic!("rename failed: {e}"));

    // Addressable both ways, and the quoted case survived.
    assert_eq!(
        db.query(r#"SELECT "id" FROM app."Acct2""#, &[]).unwrap().len(),
        1,
        "the renamed table is not addressable in its own schema"
    );
    assert_eq!(
        db.query(r#"SELECT "id" FROM "Acct2""#, &[]).unwrap().len(),
        1,
        "the renamed table is not addressable by a bare name under its search_path"
    );
    // It did NOT leak into the bare/`public` key-space.
    assert!(
        db.query(r#"SELECT "id" FROM public."Acct2""#, &[]).is_err(),
        "the renamed table escaped its schema into `public`"
    );
    // Still writable under the new name, and the PK still enforces there.
    db.execute(r#"INSERT INTO app."Acct2" ("id") VALUES (2)"#)
        .unwrap_or_else(|e| panic!("the renamed table is not writable: {e}"));
    assert!(
        db.execute(r#"INSERT INTO app."Acct2" ("id") VALUES (2)"#).is_err(),
        "the PRIMARY KEY stopped enforcing after the rename"
    );
    // The old name is gone.
    assert!(db.query(r#"SELECT "id" FROM app."Acct""#, &[]).is_err());
}

/// A schema-QUALIFIED target naming a DIFFERENT schema is rejected, the way
/// PostgreSQL rejects it (there, `RENAME TO` simply has no qualified-name
/// grammar). `ALTER TABLE … SET SCHEMA` is the operation that moves a table.
///
/// The same-schema qualified spelling stays legal, because it names exactly the
/// rename the bare spelling would have done.
#[test]
fn a_rename_to_a_different_schema_is_rejected_and_changes_nothing() {
    let db = mem_db();
    db.execute("CREATE SCHEMA s1").unwrap();
    db.execute("CREATE SCHEMA s2").unwrap();
    db.execute("CREATE TABLE s1.t (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO s1.t (id) VALUES (1)").unwrap();
    db.execute("CREATE TABLE pubt (id INT PRIMARY KEY)").unwrap();

    let err = db
        .execute("ALTER TABLE s1.t RENAME TO s2.t2")
        .err()
        .expect("*** SILENT SCHEMA MOVE *** RENAME TO s2.t2 was accepted");
    assert!(
        err.to_string().to_ascii_lowercase().contains("cannot change schema"),
        "unexpected error text: {err}"
    );

    // A public table cannot be renamed INTO a schema either.
    let err = db
        .execute("ALTER TABLE pubt RENAME TO s1.pubt2")
        .err()
        .expect("*** SILENT SCHEMA MOVE *** RENAME TO s1.pubt2 was accepted");
    assert!(
        err.to_string().to_ascii_lowercase().contains("cannot change schema"),
        "unexpected error text: {err}"
    );

    // Nothing moved and nothing was renamed.
    assert_eq!(db.query("SELECT id FROM s1.t", &[]).unwrap().len(), 1);
    assert!(db.query("SELECT id FROM s2.t2", &[]).is_err());
    assert!(db.query("SELECT id FROM s1.t2", &[]).is_err());
    assert_eq!(db.query("SELECT id FROM pubt", &[]).unwrap().len(), 0);
    assert!(db.query("SELECT id FROM s1.pubt2", &[]).is_err());

    // The SAME-schema qualified spelling is still a plain rename.
    db.execute("ALTER TABLE s1.t RENAME TO s1.t2")
        .unwrap_or_else(|e| panic!("a same-schema qualified rename must work: {e}"));
    assert_eq!(db.query("SELECT id FROM s1.t2", &[]).unwrap().len(), 1);
    assert!(db.query("SELECT id FROM s1.t", &[]).is_err());
}

/// Sprinter 6dbb03fcaac6 — FAIL-OPEN: `ALTER TABLE … RENAME TO` left the
/// `table_constraints:{table}` record behind under the OLD name, so after a
/// rename every CHECK constraint and FOREIGN KEY the table owned silently
/// stopped being enforced (the enforcement paths load the record for the CURRENT
/// name, and an absent record reads as "no constraints" — no error, no warning),
/// and `DROP CONSTRAINT <name>` could no longer find them.
///
/// `ALTER TABLE … SET SCHEMA` — which is implemented AS a rename onto a new
/// storage key — already carried them (`move_table_side_records`); a plain
/// rename did not. The fix moves that call INTO `Catalog::rename_table`, the one
/// funnel every rename caller reaches.
///
/// The enforcement DML runs on both executor families; see the FAMILY NOTE above
/// for why the rename statement itself does not.
#[test]
fn renaming_a_table_keeps_its_check_and_foreign_key_enforcing() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE par (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO par (id) VALUES (1)").unwrap();
        db.execute(
            "CREATE TABLE ch (id INT PRIMARY KEY, pid INT REFERENCES par(id), qty INT, \
             CONSTRAINT ch_qty_pos CHECK (qty > 0))",
        )
        .unwrap();
        db.execute("INSERT INTO ch (id, pid, qty) VALUES (1, 1, 5)").unwrap();
        // Both constraints demonstrably enforce BEFORE the rename on THIS
        // family, so a pass afterwards can only be the rename's doing.
        assert!(
            run(&db, "INSERT INTO ch (id, pid, qty) VALUES (2, 1, -1)", params_family).is_err(),
            "[{fam}] the CHECK was not enforcing even before the rename"
        );
        assert!(
            run(&db, "INSERT INTO ch (id, pid, qty) VALUES (2, 99, 1)", params_family).is_err(),
            "[{fam}] the FK was not enforcing even before the rename"
        );

        db.execute("ALTER TABLE ch RENAME TO ch2")
            .unwrap_or_else(|e| panic!("[{fam}] rename failed: {e}"));

        assert!(
            run(&db, "INSERT INTO ch2 (id, pid, qty) VALUES (2, 1, -1)", params_family).is_err(),
            "[{fam}] *** UNENFORCED CONSTRAINT *** the CHECK stopped enforcing after RENAME TO"
        );
        assert!(
            run(&db, "INSERT INTO ch2 (id, pid, qty) VALUES (3, 99, 1)", params_family).is_err(),
            "[{fam}] *** UNENFORCED CONSTRAINT *** the FOREIGN KEY stopped enforcing after RENAME TO"
        );
        // A legal row is still accepted — the constraints came across intact,
        // they did not turn into a blanket refusal.
        run(&db, "INSERT INTO ch2 (id, pid, qty) VALUES (4, 1, 2)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] a valid row was rejected after the rename: {e}"));
        assert_eq!(rows_in(&db, "ch2"), 2, "[{fam}] a rejected row was stored");

        // The constraint is reachable BY NAME under the new table name…
        run(&db, "ALTER TABLE ch2 DROP CONSTRAINT ch_qty_pos", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] *** UNNAMEABLE CONSTRAINT *** DROP CONSTRAINT after rename: {e}"));
        // …and dropping it really stops the CHECK (proving the DROP hit the
        // record the enforcement reads, not some other copy).
        run(&db, "INSERT INTO ch2 (id, pid, qty) VALUES (5, 1, -1)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] the dropped CHECK is still enforcing: {e}"));
        // The FK, which was NOT dropped, still enforces.
        assert!(
            run(&db, "INSERT INTO ch2 (id, pid, qty) VALUES (6, 99, 1)", params_family).is_err(),
            "[{fam}] DROP CONSTRAINT took the foreign key with it"
        );
    }
}

/// The same record move, but durable: it must be on disk under the NEW name
/// after a reopen, and GONE from the old name — an orphaned record left behind
/// is inherited by the next `CREATE TABLE <old_name>` (`CREATE TABLE` merges
/// with whatever `load_table_constraints` returns for the name), which is the
/// fail-open turned inside out: constraints nobody declared.
#[test]
fn a_renamed_tables_constraints_survive_a_reopen_and_free_the_old_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_str().unwrap().to_string();

    {
        let db = EmbeddedDatabase::new(&path).expect("open");
        db.execute("CREATE TABLE par (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO par (id) VALUES (1)").unwrap();
        db.execute(
            "CREATE TABLE ch (id INT PRIMARY KEY, pid INT REFERENCES par(id), qty INT, \
             CONSTRAINT ch_qty_pos CHECK (qty > 0))",
        )
        .unwrap();
        db.execute("INSERT INTO ch (id, pid, qty) VALUES (1, 1, 5)").unwrap();
        db.execute("ALTER TABLE ch RENAME TO ch2").unwrap();
    }

    let db = EmbeddedDatabase::new(&path).expect("reopen");
    assert_eq!(
        rows_in(&db, "ch2"),
        1,
        "the renamed table lost its rows across the reopen"
    );
    assert!(
        db.execute("INSERT INTO ch2 (id, pid, qty) VALUES (2, 1, -1)").is_err(),
        "*** UNENFORCED AFTER RESTART *** the CHECK did not survive the rename + reopen"
    );
    assert!(
        db.execute("INSERT INTO ch2 (id, pid, qty) VALUES (3, 99, 1)").is_err(),
        "*** UNENFORCED AFTER RESTART *** the FOREIGN KEY did not survive the rename + reopen"
    );
    db.execute("INSERT INTO ch2 (id, pid, qty) VALUES (4, 1, 2)")
        .expect("a valid row must still be accepted after the reopen");
    assert_eq!(rows_in(&db, "ch2"), 2);

    // The OLD name carries nothing: a brand-new `ch` has only the constraints it
    // declares itself.
    db.execute("CREATE TABLE ch (id INT PRIMARY KEY, qty INT)").unwrap();
    db.execute("INSERT INTO ch (id, qty) VALUES (1, -5)")
        .unwrap_or_else(|e| {
            panic!("*** INHERITED CONSTRAINT *** the new `ch` inherited the renamed table's CHECK: {e}")
        });
    assert_eq!(rows_in(&db, "ch"), 1);
}

// ===========================================================================
// A refusal that arrives AFTER the row is stored costs that row ONE entry
//
// These two run against the ART funnels through the PUBLIC API rather than
// through SQL, and deliberately so: every SQL write path pre-checks PK/UNIQUE
// (`StorageEngine::check_insert_constraints`) BEFORE it stores anything, so the
// only way a stored row reaches the index refusal is a race between that check
// (read locks) and the tree insert. That race is real — two concurrent
// autocommit INSERTs of the same UNIQUE value both pass the check, and the loser
// is refused by the tree with its row already durable — but it is not
// deterministic enough to assert on. What IS deterministic is what the funnel
// does when it happens, which is the whole of the contract below. (Closing the
// race itself is a separate change; the pre-check/index window is filed on its
// own.)
//
// The signatures used here are the ones the STORED callers use
// (`StorageEngine::insert_tuple_fast` -> `on_insert_tuple`, and
// `insert_prepared_tuples_fast_batch` -> `on_insert_tuples`), unchanged by this
// fix, so both tests compile and RUN against the tree that has the bug.
// ===========================================================================

/// The MAJOR from the sixth-pass review, exactly: the all-or-nothing undo fired
/// on a funnel whose row is stored anyway, so a genuine duplicate stripped a
/// DURABLE row of every index entry INCLUDING its PRIMARY KEY.
///
/// Concrete failure it leaves behind (the reason this is not cosmetic): the row
/// is countable by a full scan, `SELECT … WHERE id = 3` returns 0 rows, and the
/// next `INSERT … (3, …)` is ACCEPTED — two rows, one primary key.
///
/// FAILS on the unfixed tree at the first assertion:
/// "*** ROW UNINDEXED *** the stored row lost its PRIMARY KEY entry".
#[test]
fn a_stored_row_keeps_its_primary_key_when_a_unique_index_refuses_it() {
    use heliosdb_nano::storage::ArtIndexManager;
    use heliosdb_nano::{Column, DataType, Schema, Tuple};

    let m = ArtIndexManager::new();
    m.create_pk_index("st", &["id".to_string()]).unwrap();
    m.create_unique_index("st", &["v".to_string()], None).unwrap();
    m.create_unique_index("st", &["w".to_string()], None).unwrap();

    let schema = Schema::new(vec![
        Column::new("id", DataType::Int4).primary_key(),
        Column::new("v", DataType::Text).unique(),
        Column::new("w", DataType::Text).unique(),
    ]);
    let mk = |id: i32, v: &str, w: &str| {
        Tuple::new(vec![
            Value::Int4(id),
            Value::String(v.to_string()),
            Value::String(w.to_string()),
        ])
    };

    // The winner of the race.
    m.on_insert_tuple("st", 1, &schema, &mk(1, "a", "p")).unwrap();

    // The loser: `insert_tuple_fast` has ALREADY written this row and its
    // logical-WAL record before it calls `on_insert_tuple`, so the refusal below
    // arrives too late to unmake it.
    let err = m
        .on_insert_tuple("st", 3, &schema, &mk(3, "a", "r"))
        .expect_err("the duplicate on `v` must be reported to the caller");
    assert!(
        err.to_string().to_ascii_lowercase().contains("duplicate"),
        "the refusal must read as a duplicate, got: {err}"
    );

    assert!(
        m.unique_key_exists("st", &["id".to_string()], &[Value::Int4(3)]),
        "*** ROW UNINDEXED *** the stored row lost its PRIMARY KEY entry to the undo: a full scan still \
         counts it, `WHERE id = 3` cannot find it, and `id = 3` is free for a second row to claim"
    );
    assert!(
        m.unique_key_exists("st", &["w".to_string()], &[Value::String("r".to_string())]),
        "*** ROW UNINDEXED *** the stored row is not findable by `w`, an index that never refused it"
    );

    // Only the entry it was REFUSED is absent — and it still belongs to the row
    // that claimed the value first, so the constraint keeps enforcing.
    assert!(
        !m.unique_key_taken_by_other_row("st", &["v".to_string()], &[Value::String("a".to_string())], Some(1)),
        "the `v = 'a'` entry must still be row 1's — it was there first"
    );
    assert!(
        m.on_insert_tuple("st", 4, &schema, &mk(4, "a", "s")).is_err(),
        "*** UNENFORCED CONSTRAINT *** `v = 'a'` was handed out a third time"
    );
    // …and nobody may take the stored row's primary key.
    assert!(
        m.on_insert_tuple("st", 5, &schema, &mk(3, "z", "t")).is_err(),
        "*** DUPLICATE PRIMARY KEY *** `id = 3` was accepted a second time: the stored row's PK entry is \
         missing from the index"
    );
    // The winner is untouched by the loser's refusal.
    assert!(
        m.unique_key_exists("st", &["id".to_string()], &[Value::Int4(1)])
            && m.unique_key_exists("st", &["w".to_string()], &[Value::String("p".to_string())]),
        "*** COLLATERAL DAMAGE *** row 1 lost an entry to another row's refusal"
    );
}

/// The same contract for the COPY batch funnel, whose rows are COMMITTED before
/// ART maintenance runs — and which additionally used to drop the refusal at
/// `tracing::debug!`, off in every shipped configuration.
///
/// FAILS on the unfixed tree at "*** ROW UNINDEXED *** the committed batch row
/// lost its PRIMARY KEY entry" (and, after that, on the swallowed refusal).
#[test]
fn a_committed_copy_batch_row_keeps_its_primary_key_when_refused() {
    use heliosdb_nano::storage::ArtIndexManager;
    use heliosdb_nano::{Column, DataType, Schema, Tuple};

    let m = ArtIndexManager::new();
    m.create_pk_index("cb", &["id".to_string()]).unwrap();
    m.create_unique_index("cb", &["v".to_string()], None).unwrap();
    m.create_unique_index("cb", &["w".to_string()], None).unwrap();

    let schema = Schema::new(vec![
        Column::new("id", DataType::Int4).primary_key(),
        Column::new("v", DataType::Text).unique(),
        Column::new("w", DataType::Text).unique(),
    ]);
    let mk = |id: i32, v: &str, w: &str| {
        Tuple::new(vec![
            Value::Int4(id),
            Value::String(v.to_string()),
            Value::String(w.to_string()),
        ])
    };

    // Row 2 collides with row 1 on `v`; rows 1 and 3 are clean. All three are
    // already durable when this runs.
    let rows = vec![
        (1u64, mk(1, "a", "p")),
        (2u64, mk(2, "a", "q")),
        (3u64, mk(3, "b", "s")),
    ];
    let outcome = m.on_insert_tuples("cb", &schema, &rows);

    assert!(
        m.unique_key_exists("cb", &["id".to_string()], &[Value::Int4(2)]),
        "*** ROW UNINDEXED *** the committed batch row lost its PRIMARY KEY entry: a full scan still \
         counts it, `WHERE id = 2` cannot find it, and `id = 2` is free for a second row to claim"
    );
    assert!(
        m.unique_key_exists("cb", &["w".to_string()], &[Value::String("q".to_string())]),
        "*** ROW UNINDEXED *** the committed batch row is not findable by `w`, which never refused it"
    );
    for (id, v, w) in [(1, "a", "p"), (3, "b", "s")] {
        assert!(
            m.unique_key_exists("cb", &["id".to_string()], &[Value::Int4(id)])
                && m.unique_key_exists("cb", &["v".to_string()], &[Value::String(v.to_string())])
                && m.unique_key_exists("cb", &["w".to_string()], &[Value::String(w.to_string())]),
            "*** COLLATERAL DAMAGE *** batch row {id} is missing an entry"
        );
    }
    assert!(
        m.on_insert_tuple("cb", 9, &schema, &mk(2, "z", "y")).is_err(),
        "*** DUPLICATE PRIMARY KEY *** `id = 2` was accepted a second time"
    );

    // A stored duplicate is an operator-visible fact, not a debug detail.
    let err = outcome.expect_err(
        "*** SILENT DUPLICATE *** the COPY batch dropped the refusal: a committed row a UNIQUE index \
         cannot find was reported nowhere the operator can see",
    );
    assert!(
        err.to_string().contains("row 2"),
        "the report must name the refused row, got: {err}"
    );
}

/// The row-keyed intra-statement dedup must still REJECT the case it exists
/// for: one UPDATE that moves two DIFFERENT rows onto the same unique value.
///
/// Keying the `seen_*` vectors by row id (so a row cannot be reported as a
/// duplicate of itself when two constraint records cover one column set) would
/// be a fail-OPEN change if it also stopped catching two rows colliding inside
/// a single statement — neither row conflicts with anything already in the
/// index, so nothing else in the write path would notice. Pinned on both
/// executor families, and the table is left untouched.
#[test]
fn one_update_that_moves_two_rows_onto_the_same_unique_value_is_rejected() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE dup2 (id INT PRIMARY KEY, v VARCHAR(50), UNIQUE (v))")
            .unwrap();
        db.execute("INSERT INTO dup2 (id, v) VALUES (1, 'a')").unwrap();
        db.execute("INSERT INTO dup2 (id, v) VALUES (2, 'b')").unwrap();

        let err = run(&db, "UPDATE dup2 SET v = 'same' WHERE id IN (1, 2)", params_family)
            .err()
            .unwrap_or_else(|| {
                panic!("{fam}: one UPDATE moved both rows onto 'same' — the intra-statement dedup did not fire")
            });
        assert!(
            err.to_string().to_lowercase().contains("unique"),
            "{fam}: expected a UNIQUE violation, got: {err}"
        );

        // …and neither row was written.
        let same = db.query("SELECT id FROM dup2 WHERE v = 'same'", &[]).unwrap().len();
        assert_eq!(same, 0, "{fam}: a refused multi-row UPDATE still wrote rows");
        assert_eq!(rows_in(&db, "dup2"), 2, "{fam}: row count changed");
    }
}

/// The column-level spelling of the same statement, for the second `seen_*`
/// vector (`seen_column_keys`), which the table-level test above does not reach.
#[test]
fn one_update_that_collides_two_rows_on_an_inline_unique_column_is_rejected() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE dup1 (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE)")
            .unwrap();
        db.execute("INSERT INTO dup1 (id, v) VALUES (1, 'a')").unwrap();
        db.execute("INSERT INTO dup1 (id, v) VALUES (2, 'b')").unwrap();

        let err = run(&db, "UPDATE dup1 SET v = 'same' WHERE id IN (1, 2)", params_family)
            .err()
            .unwrap_or_else(|| panic!("{fam}: one UPDATE moved both rows onto 'same'"));
        assert!(
            err.to_string().to_lowercase().contains("unique"),
            "{fam}: expected a UNIQUE violation, got: {err}"
        );
        assert_eq!(
            db.query("SELECT id FROM dup1 WHERE v = 'same'", &[]).unwrap().len(),
            0,
            "{fam}: a refused multi-row UPDATE still wrote rows"
        );
    }
}

/// A plain `UPDATE … SET <unique col> = <a value no other row holds>` must
/// succeed on EVERY spelling of the constraint. It did not: a table-level
/// `UNIQUE (v)` is recorded twice (as the table-level constraint and as the
/// column-flag constraint `CREATE TABLE` derives from the flag the planner
/// sets for single-column table-level UNIQUE), so the row was validated twice
/// against the same column set and the second pass saw the first pass's own
/// dedup entry and reported the row as a duplicate of ITSELF — 23505 on a
/// statement PostgreSQL accepts, on both families, with and without RETURNING.
#[test]
fn updating_a_unique_column_to_a_free_value_succeeds_for_every_spelling() {
    for params_family in [false, true] {
        let fam = family(params_family);
        for (ddl, spelling) in [
            (
                "CREATE TABLE sp (id INT PRIMARY KEY, v VARCHAR(50), UNIQUE (v))",
                "table-level UNIQUE (v)",
            ),
            (
                "CREATE TABLE sp (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE)",
                "inline UNIQUE",
            ),
            (
                "CREATE TABLE sp (id INT PRIMARY KEY, v VARCHAR(50), CONSTRAINT sp_v_uq UNIQUE (v))",
                "named table-level UNIQUE",
            ),
        ] {
            let db = mem_db();
            db.execute(ddl).unwrap();
            db.execute("INSERT INTO sp (id, v) VALUES (1, 'one')").unwrap();
            db.execute("INSERT INTO sp (id, v) VALUES (2, 'two')").unwrap();

            run(&db, "UPDATE sp SET v = 'free' WHERE id = 1", params_family)
                .unwrap_or_else(|e| panic!("{fam}/{spelling}: UPDATE onto a free value failed: {e}"));
            assert_eq!(
                db.query("SELECT id FROM sp WHERE v = 'free'", &[]).unwrap().len(),
                1,
                "{fam}/{spelling}: the updated row is invisible to an `=` lookup"
            );
            // The constraint is still enforced afterwards.
            assert!(
                db.execute("INSERT INTO sp (id, v) VALUES (3, 'free')").is_err(),
                "{fam}/{spelling}: UNIQUE stopped enforcing after the UPDATE"
            );
            // …and taking the value a DIFFERENT row holds still fails.
            assert!(
                run(&db, "UPDATE sp SET v = 'two' WHERE id = 1", params_family).is_err(),
                "{fam}/{spelling}: UPDATE onto another row's value was accepted"
            );
        }
    }
}
