//! GH #21 — "UNIQUE enforced only for inline column constraints (table-level
//! `UNIQUE (c)`, `CREATE UNIQUE INDEX` and `ALTER TABLE … ADD CONSTRAINT …
//! UNIQUE` all accept duplicates)", reproduced with the issue's VERBATIM SQL.
//!
//! Install as `tests/gh_issue_21.rs`.
//!
//! # Why this file exists next to `tests/prisma_p0_unique_on_conflict.rs`
//!
//! That file proves the FIX (commit 79e2255) with its own, deliberately
//! minimal shapes: one table per spelling, each in a fresh database, with a
//! synthetic `seed_first_claimant()` helper standing in for "some earlier table
//! already named this column". This file instead runs the reporter's script
//! byte-for-byte, in ONE database, in the ORDER the issue gives it:
//!
//! ```sql
//! CREATE TABLE u1 (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE);
//! CREATE TABLE u2 (id INT PRIMARY KEY, v VARCHAR(50), UNIQUE (v));
//! CREATE TABLE u3 (id INT PRIMARY KEY, v VARCHAR(50), w INT, UNIQUE (v,w));
//! CREATE TABLE u4 (id INT PRIMARY KEY, v VARCHAR(50));
//! CREATE UNIQUE INDEX u4_v ON u4 (v);
//! ALTER TABLE u4 ADD CONSTRAINT u4_v_key UNIQUE (v);
//! ```
//!
//! Three things only this ordering exercises:
//!
//!  1. FOUR tables in one database all name the column `v`. The original defect
//!     was that `Catalog::create_table` registered the column-level UNIQUE index
//!     in `ArtIndexManager`'s ONE GLOBAL name map under the BARE COLUMN NAME, so
//!     the first table to declare `v` owned the name and every later table's
//!     registration failed with `IndexAlreadyExists` — swallowed at warn, leaving
//!     the constraint enforced by nothing. `u1` is the first claimant here for
//!     real, not by a helper, and `u2`/`u3`/`u4` are the victims.
//!  2. `ALTER TABLE u4 ADD CONSTRAINT u4_v_key UNIQUE (v)` lands on a table that
//!     ALREADY carries `CREATE UNIQUE INDEX u4_v ON u4 (v)` over the same column
//!     set. `alter_table_add_unique` (src/lib.rs:11255) deliberately registers NO
//!     second index in that case (`has_unique_index_on`) and records the
//!     constraint alone — a combination no existing test covers.
//!  3. …and that combination has to come back correctly after a restart, where
//!     `Catalog::rebuild_all_indexes` (src/storage/catalog.rs:1480) registers
//!     constraint-derived indexes BEFORE the persisted `CREATE UNIQUE INDEX`
//!     definitions — i.e. in the opposite order to the DDL that created them.
//!
//! Both DML executor families are covered: `db.execute()` (text family →
//! `execute_in_transaction_inner`: psql simple query, MySQL wire, embedded) and
//! `db.execute_params()` (params family → `execute_plan_with_params_inner`: the
//! PostgreSQL EXTENDED protocol every real driver uses, including the node-pg
//! adapter in the issue, plus REST/BaaS). `CREATE TABLE` itself has no arm on
//! the params family (`LogicalPlan::CreateTable` is matched only at
//! src/lib.rs:4996), so table creation runs on the text family in both passes;
//! every statement whose behaviour the issue is about — `CREATE UNIQUE INDEX`,
//! `ALTER TABLE … ADD CONSTRAINT`, and every `INSERT` — runs on the family under
//! test.
//!
//! Expected on a FIXED tree: every test passes.
//! Expected on the tree the issue was filed against: every test that calls
//! `issue_21_schema()` dies on `ALTER TABLE u4 ADD CONSTRAINT u4_v_key
//! UNIQUE (v)` with "Unsupported ALTER TABLE operation: AddConstraint"; drop
//! that one line on such a tree and the residual failures are the real
//! enforcement holes — "*** UNENFORCED CONSTRAINT ***" for u2, u3 and u4, with
//! the `u1` (inline UNIQUE) block still passing.
//!
//! `issue21_positive_control_plain_columns_still_accept_duplicates` deliberately
//! does NOT use `issue_21_schema()`: it touches no ALTER and no UNIQUE spelling
//! the issue is about, so it runs and passes on BOTH trees. That is what makes
//! it a control — a "control" that cannot execute on the broken tree proves
//! nothing about the harness.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn mem_db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory database")
}

/// Run one statement through the requested executor family.
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
/// returns one row whether the count is 0 or 10,000, and this file is about
/// rows that should not exist.
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
/// `Int8` depending on the path that produced it.
fn scalar_int(db: &EmbeddedDatabase, sql: &str) -> i64 {
    match scalar(db, sql) {
        Value::Int2(v) => i64::from(v),
        Value::Int4(v) => i64::from(v),
        Value::Int8(v) => v,
        other => panic!("`{sql}` did not return an integer, got {other:?}"),
    }
}

/// The message shape the PG wire maps to SQLSTATE 23505 unique_violation
/// (`sqlstate_for_error` keys on `ConstraintViolation` containing "duplicate
/// key" or "unique constraint"). Anchored on those two phrases and NOT on a
/// bare `contains("unique")`, because the unfixed tree's
/// `Unsupported ALTER TABLE operation: AddConstraint(Unique { … })` contains
/// that word and would satisfy a sloppier assertion for the wrong reason.
fn assert_unique_violation(err: &heliosdb_nano::Error, context: &str) {
    let text = err.to_string().to_ascii_lowercase();
    assert!(
        text.contains("duplicate key") || text.contains("unique constraint"),
        "{context}: the error must read as a UNIQUE violation (23505 on the wire), got: {err}"
    );
}

/// The issue's DDL, verbatim, in the issue's order, in one database.
///
/// `CREATE TABLE` runs on the text family in both passes (no params-family arm
/// exists for it); `CREATE UNIQUE INDEX` and `ALTER TABLE … ADD CONSTRAINT` run
/// on the family under test, because both DO have params-family arms
/// (src/lib.rs:14973 routes `AlterTableAddUnique` to the shared body) and both
/// are statements the reporter's client issued over the extended protocol.
fn issue_21_schema(db: &EmbeddedDatabase, params_family: bool) {
    let fam = family(params_family);
    db.execute("CREATE TABLE u1 (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE)")
        .expect("u1");
    db.execute("CREATE TABLE u2 (id INT PRIMARY KEY, v VARCHAR(50), UNIQUE (v))")
        .expect("u2");
    db.execute("CREATE TABLE u3 (id INT PRIMARY KEY, v VARCHAR(50), w INT, UNIQUE (v,w))")
        .expect("u3");
    db.execute("CREATE TABLE u4 (id INT PRIMARY KEY, v VARCHAR(50))")
        .expect("u4");
    run(db, "CREATE UNIQUE INDEX u4_v ON u4 (v)", params_family)
        .unwrap_or_else(|e| panic!("[{fam}] `CREATE UNIQUE INDEX u4_v ON u4 (v)` must be accepted: {e}"));
    run(db, "ALTER TABLE u4 ADD CONSTRAINT u4_v_key UNIQUE (v)", params_family).unwrap_or_else(|e| {
        panic!(
            "[{fam}] `ALTER TABLE u4 ADD CONSTRAINT u4_v_key UNIQUE (v)` must be supported \
             (the issue got \"Unsupported ALTER TABLE operation: AddConstraint\"): {e}"
        )
    });
}

// ===========================================================================
// 1. The issue's script, statement for statement
// ===========================================================================

/// The headline claim: after the verbatim DDL above, the SECOND
/// `INSERT INTO u2 VALUES (2,'a')` must be rejected. On the reported build it
/// succeeded, and so did the equivalents for `u3` and `u4`.
///
/// `u1` is asserted first on purpose. It was the ONE spelling that always
/// worked, so it is this file's in-band positive control: if the `u1` block
/// fails, the harness — not the fix — is broken.
#[test]
fn issue21_every_unique_spelling_rejects_a_duplicate_verbatim() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        issue_21_schema(&db, params_family);

        // --- u1: inline UNIQUE. Worked before the fix; must keep working. ---
        run(&db, "INSERT INTO u1 VALUES (1,'a')", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] POSITIVE CONTROL: the first u1 row must insert: {e}"));
        let err = run(&db, "INSERT INTO u1 VALUES (2,'a')", params_family)
            .err()
            .unwrap_or_else(|| panic!("[{fam}] POSITIVE CONTROL BROKEN: inline UNIQUE accepted a duplicate"));
        assert_unique_violation(&err, &format!("[{fam}] u1 inline UNIQUE"));
        assert_eq!(rows_in(&db, "u1"), 1, "[{fam}] u1 stored the duplicate anyway");

        // --- u2: table-level UNIQUE (v). The issue's exact two INSERTs. ---
        run(&db, "INSERT INTO u2 VALUES (1,'a')", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] the first u2 row must insert: {e}"));
        let err = run(&db, "INSERT INTO u2 VALUES (2,'a')", params_family)
            .err()
            .unwrap_or_else(|| {
                panic!("[{fam}] *** UNENFORCED CONSTRAINT *** u2: `UNIQUE (v)` accepted a duplicate 'a'")
            });
        assert_unique_violation(&err, &format!("[{fam}] u2 table-level UNIQUE (v)"));
        assert_eq!(rows_in(&db, "u2"), 1, "[{fam}] u2 stored the duplicate anyway");
        // …and a distinct value still inserts, so the assertion above cannot be
        // passing because u2 rejects everything.
        run(&db, "INSERT INTO u2 VALUES (3,'b')", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] u2 must still accept a distinct value: {e}"));
        assert_eq!(rows_in(&db, "u2"), 2);

        // --- u3: composite table-level UNIQUE (v,w), the issue's spelling
        // (no space after the comma). ---
        run(&db, "INSERT INTO u3 VALUES (1,'a',1)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] the first u3 row must insert: {e}"));
        let err = run(&db, "INSERT INTO u3 VALUES (2,'a',1)", params_family)
            .err()
            .unwrap_or_else(|| {
                panic!("[{fam}] *** UNENFORCED CONSTRAINT *** u3: `UNIQUE (v,w)` accepted a duplicate ('a',1) pair")
            });
        assert_unique_violation(&err, &format!("[{fam}] u3 composite UNIQUE (v,w)"));
        assert_eq!(rows_in(&db, "u3"), 1, "[{fam}] u3 stored the duplicate anyway");
        // A pair that differs in ONE column is not a duplicate.
        run(&db, "INSERT INTO u3 VALUES (3,'a',2)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] u3 must accept ('a',2): {e}"));
        assert_eq!(rows_in(&db, "u3"), 2);

        // --- u4: CREATE UNIQUE INDEX (+ the ADD CONSTRAINT layered on top). ---
        run(&db, "INSERT INTO u4 VALUES (1,'a')", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] the first u4 row must insert: {e}"));
        let err = run(&db, "INSERT INTO u4 VALUES (2,'a')", params_family)
            .err()
            .unwrap_or_else(|| {
                panic!("[{fam}] *** UNENFORCED CONSTRAINT *** u4: CREATE UNIQUE INDEX accepted a duplicate")
            });
        assert_unique_violation(&err, &format!("[{fam}] u4 CREATE UNIQUE INDEX"));
        assert_eq!(rows_in(&db, "u4"), 1, "[{fam}] u4 stored the duplicate anyway");
        run(&db, "INSERT INTO u4 VALUES (3,'b')", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] u4 must still accept a distinct value: {e}"));
        assert_eq!(rows_in(&db, "u4"), 2);
    }
}

/// The issue's ALTER, on its own, on a table with NO pre-existing index over the
/// column — the plain path through `alter_table_add_unique`, as opposed to the
/// dedup path `issue_21_schema` exercises. It must be accepted (the issue got
/// "Unsupported ALTER TABLE operation: AddConstraint") AND actually enforce.
#[test]
fn issue21_alter_table_add_constraint_unique_is_supported_and_enforces() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        // The first claimant of `v`, exactly as the issue's script has it: the
        // whole defect was about the SECOND table to name a column.
        db.execute("CREATE TABLE u1 (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE)")
            .expect("u1");
        db.execute("CREATE TABLE u4 (id INT PRIMARY KEY, v VARCHAR(50))")
            .expect("u4");

        run(&db, "ALTER TABLE u4 ADD CONSTRAINT u4_v_key UNIQUE (v)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] ALTER TABLE … ADD CONSTRAINT … UNIQUE must be supported, got: {e}"));

        run(&db, "INSERT INTO u4 VALUES (1,'a')", params_family).unwrap();
        let err = run(&db, "INSERT INTO u4 VALUES (2,'a')", params_family)
            .err()
            .unwrap_or_else(|| {
                panic!("[{fam}] *** UNENFORCED CONSTRAINT *** the added UNIQUE constraint accepted a duplicate")
            });
        assert_unique_violation(&err, &format!("[{fam}] ADD CONSTRAINT UNIQUE"));
        assert_eq!(rows_in(&db, "u4"), 1);
    }
}

/// Layering both u4 spellings — `CREATE UNIQUE INDEX u4_v` and then
/// `ALTER TABLE u4 ADD CONSTRAINT u4_v_key UNIQUE (v)` over the SAME column —
/// must leave exactly one working rule, not a constraint record with no index
/// behind it and not a rejection of the ALTER as "already exists".
#[test]
fn issue21_unique_index_then_add_constraint_over_the_same_column_stays_enforced() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        issue_21_schema(&db, params_family);

        run(&db, "INSERT INTO u4 VALUES (1,'a')", params_family).unwrap();
        // Not a bare `is_err()`: ANY error would satisfy that, including an
        // unrelated "ART index 'u4_v_key' already exists" from the layered DDL.
        // The error has to read as a UNIQUE violation (23505 on the wire).
        let err = run(&db, "INSERT INTO u4 VALUES (2,'a')", params_family)
            .err()
            .unwrap_or_else(|| {
                panic!("[{fam}] *** UNENFORCED CONSTRAINT *** index + constraint over the same column enforce nothing")
            });
        assert_unique_violation(&err, &format!("[{fam}] CREATE UNIQUE INDEX + ADD CONSTRAINT"));
        // The doubled rule must not over-reject either.
        run(&db, "INSERT INTO u4 VALUES (2,'b')", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] a distinct value must still insert: {e}"));
        assert_eq!(rows_in(&db, "u4"), 2);

        // And an UPDATE onto the taken value is rejected too — the constraint is
        // not insert-only. `CREATE UNIQUE INDEX` sets no schema flag and writes
        // no constraint record, so this is the path that used to miss it (the
        // UPDATE fast path asks `column_in_unique_index`, not `column.unique`).
        let err = run(&db, "UPDATE u4 SET v = 'a' WHERE id = 2", params_family)
            .err()
            .unwrap_or_else(|| panic!("[{fam}] *** UNENFORCED ON UPDATE *** an UPDATE created a duplicate v"));
        assert_unique_violation(&err, &format!("[{fam}] UPDATE onto a taken value"));
        // …and the rejected UPDATE was not applied anyway.
        assert_eq!(
            rows_in(&db, "u4"),
            2,
            "[{fam}] the rejected UPDATE changed the row count"
        );
    }
}

// ===========================================================================
// 2. Durability — the same script across a restart
// ===========================================================================

/// The dangerous half-fix registers the enforcing index only at DDL time. After
/// a reopen the whole script must still enforce, and the indexes must be
/// BACKFILLED with the pre-restart rows (an index registered but empty accepts
/// duplicates of everything already stored).
///
/// This is also the only place the rebuild ORDER for `u4` is exercised:
/// `Catalog::rebuild_all_indexes` registers constraint-derived indexes
/// (src/storage/catalog.rs:1566, `register_unique_constraint_indexes`) BEFORE
/// the persisted `CREATE UNIQUE INDEX` definitions (src/storage/catalog.rs:1580)
/// — the reverse of the order the DDL ran in.
#[test]
fn issue21_verbatim_schema_still_enforces_after_a_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_str().expect("utf-8 path").to_string();

    {
        let db = EmbeddedDatabase::new(&path).expect("open");
        issue_21_schema(&db, false);
        db.execute("INSERT INTO u1 VALUES (1,'a')").unwrap();
        db.execute("INSERT INTO u2 VALUES (1,'a')").unwrap();
        db.execute("INSERT INTO u3 VALUES (1,'a',1)").unwrap();
        db.execute("INSERT INTO u4 VALUES (1,'a')").unwrap();
    }

    let db = EmbeddedDatabase::new(&path).expect("reopen");
    for table in ["u1", "u2", "u3", "u4"] {
        assert_eq!(rows_in(&db, table), 1, "{table}: the pre-restart row must survive");
    }

    // Each rejection must read as a UNIQUE violation, not merely be "an error":
    // an index registered but NOT backfilled at open would still let the
    // duplicate through, and an index rebuilt against the wrong column would
    // reject with something else entirely.
    for (table, sql, label) in [
        ("u1", "INSERT INTO u1 VALUES (2,'a')", "inline UNIQUE"),
        ("u2", "INSERT INTO u2 VALUES (2,'a')", "table-level UNIQUE (v)"),
        ("u3", "INSERT INTO u3 VALUES (2,'a',1)", "composite UNIQUE (v,w)"),
        (
            "u4",
            "INSERT INTO u4 VALUES (2,'a')",
            "CREATE UNIQUE INDEX + ADD CONSTRAINT",
        ),
    ] {
        let err = db
            .execute(sql)
            .err()
            .unwrap_or_else(|| panic!("*** UNENFORCED AFTER RESTART *** {table} ({label}): `{sql}` was accepted"));
        assert_unique_violation(&err, &format!("after reopen, {table} ({label})"));
    }
    for table in ["u1", "u2", "u3", "u4"] {
        assert_eq!(
            rows_in(&db, table),
            1,
            "{table}: a rejected row was stored after the reopen"
        );
    }

    // Not rejecting everything: genuinely new values still land.
    db.execute("INSERT INTO u1 VALUES (9,'z')").expect("u1 new value");
    db.execute("INSERT INTO u2 VALUES (9,'z')").expect("u2 new value");
    db.execute("INSERT INTO u3 VALUES (9,'z',9)").expect("u3 new value");
    db.execute("INSERT INTO u4 VALUES (9,'z')").expect("u4 new value");
    for table in ["u1", "u2", "u3", "u4"] {
        assert_eq!(rows_in(&db, table), 2, "{table}: the new value did not land");
    }
}

// ===========================================================================
// 3. Positive controls — the harness is sound and nothing over-enforces
// ===========================================================================

/// Passes before AND after the fix, and — the part the previous draft of this
/// file got wrong — it does NOT go through `issue_21_schema()`. That helper runs
/// `ALTER TABLE … ADD CONSTRAINT`, which the tree the issue was filed against
/// rejects outright, so a "control" built on it could not run at all on the
/// broken tree and therefore proved nothing about the harness. This one uses
/// only DDL that has always worked: if it fails, the test file (not the engine)
/// is the problem.
#[test]
fn issue21_positive_control_plain_columns_still_accept_duplicates() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        // Deliberately NO unique rule of any kind, and no ALTER: an in-memory
        // database, a table, inserts and reads.
        db.execute("CREATE TABLE ctl (id INT PRIMARY KEY, v VARCHAR(50), w INT)")
            .expect("ctl");

        run(&db, "INSERT INTO ctl VALUES (1,'a',1)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] POSITIVE CONTROL BROKEN: a plain insert failed: {e}"));
        // A column with NO unique rule must accept a repeat — so every
        // rejection asserted elsewhere in this file is measuring ENFORCEMENT,
        // not blanket rejection.
        run(&db, "INSERT INTO ctl VALUES (2,'a',1)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] POSITIVE CONTROL BROKEN: a non-unique column rejected a repeat: {e}"));
        assert_eq!(
            rows_in(&db, "ctl"),
            2,
            "[{fam}] POSITIVE CONTROL BROKEN: both rows must be present"
        );
        assert_eq!(
            scalar_int(&db, "SELECT id FROM ctl WHERE w = 1 AND id = 2"),
            2,
            "[{fam}] POSITIVE CONTROL BROKEN: the second row is not readable"
        );

        // And the PRIMARY KEY — the one constraint that worked on every build
        // the issue mentions — still rejects its own duplicate, so "no error at
        // all" cannot be why the assertions above pass.
        let err = run(&db, "INSERT INTO ctl VALUES (1,'z',9)", params_family)
            .err()
            .unwrap_or_else(|| panic!("[{fam}] POSITIVE CONTROL BROKEN: a duplicate PRIMARY KEY was accepted"));
        assert_unique_violation(&err, &format!("[{fam}] duplicate PRIMARY KEY"));
        assert_eq!(rows_in(&db, "ctl"), 2, "[{fam}] the rejected PK row was stored");
    }
}

/// PostgreSQL semantics that the fix must NOT have over-tightened: NULLs are
/// distinct under every UNIQUE spelling, and a column that merely PARTICIPATES
/// in a composite constraint is not unique on its own.
///
/// This one DOES use the issue's schema (so it cannot run on the pre-fix tree —
/// it is a pin, not a control), because the over-rejection risk it guards is
/// specific to the indexes that fix registers.
#[test]
fn issue21_nulls_stay_distinct_and_composite_members_are_not_unique_alone() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        issue_21_schema(&db, params_family);

        // `u3.w` is covered only as part of `UNIQUE (v,w)` — on its own it is
        // not unique, so two rows may share w = 1.
        run(&db, "INSERT INTO u3 VALUES (1,'a',1)", params_family).unwrap();
        run(&db, "INSERT INTO u3 VALUES (2,'b',1)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] a composite member is not unique on its own: {e}"));
        assert_eq!(rows_in(&db, "u3"), 2, "[{fam}] both rows must be present");
        assert_eq!(
            scalar_int(&db, "SELECT id FROM u3 WHERE v = 'b'"),
            2,
            "[{fam}] the second row must be readable by its own value"
        );

        // NULLs stay distinct under a table-level UNIQUE, as in PostgreSQL.
        run(&db, "INSERT INTO u2 VALUES (1,NULL)", params_family).unwrap();
        run(&db, "INSERT INTO u2 VALUES (2,NULL)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] a second NULL must be accepted under UNIQUE (v): {e}"));
        assert_eq!(rows_in(&db, "u2"), 2, "[{fam}] two NULL rows must both be present");

        // …and under `CREATE UNIQUE INDEX` + `ADD CONSTRAINT` on u4, which is
        // the spelling whose index the fix newly registers.
        run(&db, "INSERT INTO u4 VALUES (1,NULL)", params_family).unwrap();
        run(&db, "INSERT INTO u4 VALUES (2,NULL)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] a second NULL must be accepted under a unique INDEX: {e}"));
        assert_eq!(
            rows_in(&db, "u4"),
            2,
            "[{fam}] two NULL rows must both be present in u4"
        );

        // And under the composite, where only ONE of the two columns is NULL.
        run(&db, "INSERT INTO u3 VALUES (3,'c',NULL)", params_family).unwrap();
        run(&db, "INSERT INTO u3 VALUES (4,'c',NULL)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] a half-NULL composite key must not collide: {e}"));
        assert_eq!(rows_in(&db, "u3"), 4, "[{fam}] the half-NULL rows must both be present");
    }
}
