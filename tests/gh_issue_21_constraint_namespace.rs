//! GH#21 candidate 2 — the CONSTRAINT-INDEX NAMESPACE, fail-closed constraint
//! registration, and complete `DROP CONSTRAINT` retirement.
//!
//! # What was wrong
//!
//! `ArtIndexManager::indexes` is ONE database-global map keyed by index name,
//! and UNIQUE enforcement reads only that map: a name that resolves to nothing
//! is skipped SILENTLY — there is no scan fallback. `Catalog::
//! register_unique_constraint_indexes` passed the USER CONSTRAINT NAME as that
//! global key and then swallowed the collision (`IndexAlreadyExists` at
//! `debug!`, everything else at `warn!`), so the DDL SUCCEEDED with the
//! constraint enforced by NOTHING. Two spellings reached it:
//!
//!   1. `UNIQUE (a, b), UNIQUE (c, d)` on ONE table. An unnamed table-level
//!      constraint was recorded as `{table}_unique` — a name that embeds the
//!      table but NOT the columns — so both constraints minted the same key
//!      `dbl_unique` and only the FIRST got an index
//!      (tests/gh_issue_24.rs:731).
//!   2. `CONSTRAINT ux UNIQUE (a, b)` on a SECOND table. A constraint name is
//!      per-TABLE in PostgreSQL and in MySQL, but the registry key is
//!      database-global, so `n2` lost to `n1` (tests/gh_issue_24.rs:776).
//!
//! And the DROP half (Sprinter 3441d3e21453): the planner propagates a
//! single-column table-level `UNIQUE (u)` into `ColumnDef.unique`, and the
//! CREATE path then synthesised a SECOND constraint record from that flag. One
//! SQL declaration left TWO claims, so `DROP CONSTRAINT gh21_u` retired only
//! one of them, the index survived, duplicates stayed rejected, and the reopen
//! rebuild resurrected the rule from the flag.
//!
//! # What must be true now
//!
//! * A constraint-backed index key is MINTED — `{table}_{cols}_key`, with the
//!   smallest free `_{n}` suffix if that is taken — and the user's constraint
//!   name rides along as an in-memory LABEL.
//! * The 23505 text still names the USER's constraint. That is load-bearing,
//!   not cosmetic: Prisma parses that name out of the message into `P2002`.
//! * A `CREATE TABLE` / `ALTER TABLE ADD CONSTRAINT` that cannot install a
//!   declared constraint ERRORS; it never reports success with the constraint
//!   unenforced.
//! * `DROP CONSTRAINT` retires the WHOLE rule — record, co-declared column
//!   flag, and the index resolved BY COLUMN SET — without touching a sibling
//!   constraint, the PRIMARY KEY, another table, or a user
//!   `CREATE UNIQUE INDEX`.
//!
//! # Reading a failure
//!
//! `*** UNENFORCED CONSTRAINT ***` — a declared rule accepted a duplicate.
//! `*** PHANTOM CONSTRAINT ***`   — a retired rule is still rejecting rows.
//! `*** WRONG NAME ***`           — the 23505 stopped naming the user's
//!                                  constraint, so Prisma's `P2002` loses its
//!                                  target.
//!
//! Both DML executor families are covered everywhere the statement has an arm
//! on both: `db.execute()` (text family → `execute_in_transaction_inner`: psql
//! simple query, MySQL wire, embedded) and `db.execute_params()` (params family
//! → `execute_plan_with_params_inner`: the PostgreSQL EXTENDED protocol every
//! real driver uses, plus REST/BaaS). `CREATE TABLE` has no params-family arm
//! (`LogicalPlan::CreateTable` is matched on the text path only), so table
//! creation runs on the text family in both passes while every statement whose
//! behaviour is under test — `ALTER TABLE … ADD/DROP CONSTRAINT`,
//! `CREATE UNIQUE INDEX`, and every `INSERT` — runs on the family under test.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::EmbeddedDatabase;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn mem_db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory database")
}

fn family(params_family: bool) -> &'static str {
    if params_family {
        "params"
    } else {
        "text"
    }
}

/// Run one statement through the requested executor family.
fn run(db: &EmbeddedDatabase, sql: &str, params_family: bool) -> heliosdb_nano::Result<u64> {
    if params_family {
        db.execute_params(sql, &[])
    } else {
        db.execute(sql)
    }
}

/// Rows physically present. Deliberately NOT `SELECT COUNT(*)`: a count query
/// returns one row whether the count is 0 or 10,000, and COUNT can be answered
/// from an index — the very structure under test.
fn rows_in(db: &EmbeddedDatabase, table: &str) -> usize {
    let sql = format!("SELECT * FROM {table}");
    db.query(&sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
        .len()
}

/// The message shape the PG wire maps to SQLSTATE 23505 (`sqlstate_for_error`
/// keys 23505 on "duplicate key" / "unique constraint"). Deliberately not a
/// bare `contains("unique")`, which unrelated `Unsupported … Unique { … }`
/// planner errors would satisfy for the wrong reason.
fn assert_unique_violation(err: &heliosdb_nano::Error, context: &str) {
    let text = err.to_string().to_ascii_lowercase();
    assert!(
        text.contains("duplicate key") || text.contains("unique constraint"),
        "{context}: the error must read as a UNIQUE violation (23505 on the wire), got: {err}"
    );
}

/// The 23505 must name the constraint the USER wrote. Prisma turns that name
/// into `P2002.target`; the minted registry key (`n2_a_b_key`) is an
/// implementation detail the driver has never seen.
fn assert_names_constraint(err: &heliosdb_nano::Error, expected: &str, context: &str) {
    let text = err.to_string();
    assert!(
        text.contains(&format!("\"{expected}\"")),
        "*** WRONG NAME *** {context}: the 23505 must name the user's constraint \
         \"{expected}\" (Prisma parses it into P2002), got: {err}"
    );
}

/// Insert a row and demand it be REJECTED as a duplicate.
fn expect_duplicate(db: &EmbeddedDatabase, sql: &str, params_family: bool, context: &str) -> heliosdb_nano::Error {
    let err = run(db, sql, params_family)
        .err()
        .unwrap_or_else(|| panic!("*** UNENFORCED CONSTRAINT *** {context}: `{sql}` was accepted"));
    assert_unique_violation(&err, context);
    err
}

/// Insert a row and demand it be ACCEPTED.
fn expect_accepted(db: &EmbeddedDatabase, sql: &str, params_family: bool, context: &str) {
    run(db, sql, params_family)
        .unwrap_or_else(|e| panic!("*** PHANTOM CONSTRAINT *** {context}: `{sql}` was rejected: {e}"));
}

// ===========================================================================
// 1. The two observed failures (tests/gh_issue_24.rs:731 and :776)
// ===========================================================================

/// Two UNNAMED table-level UNIQUE constraints on ONE table. Both records are
/// still called `{table}_unique` — the record name is not what this fix
/// changes — but the two ENFORCING indexes are minted from the COLUMNS
/// (`dbl_a_b_key`, `dbl_c_d_key`), so the second one exists.
#[test]
fn two_unnamed_table_level_uniques_on_one_table_both_enforce() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE dbl (id INT PRIMARY KEY, a INT, b INT, c INT, d INT, UNIQUE (a, b), UNIQUE (c, d))")
            .expect("dbl");

        run(
            &db,
            "INSERT INTO dbl (id, a, b, c, d) VALUES (1, 1, 1, 1, 1)",
            params_family,
        )
        .unwrap();

        // Control: the FIRST constraint IS enforced. If this ever fails the
        // test is no longer isolating the second-constraint gap.
        expect_duplicate(
            &db,
            "INSERT INTO dbl (id, a, b, c, d) VALUES (2, 1, 1, 9, 9)",
            params_family,
            &format!("[{fam}] the FIRST UNIQUE (a, b)"),
        );
        // The reported defect.
        expect_duplicate(
            &db,
            "INSERT INTO dbl (id, a, b, c, d) VALUES (3, 8, 8, 1, 1)",
            params_family,
            &format!("[{fam}] the SECOND UNIQUE (c, d)"),
        );
        assert_eq!(rows_in(&db, "dbl"), 1, "[{fam}] a rejected row was stored anyway");

        // Neither constraint over-rejects: a row distinct on BOTH pairs lands.
        expect_accepted(
            &db,
            "INSERT INTO dbl (id, a, b, c, d) VALUES (4, 2, 2, 2, 2)",
            params_family,
            &format!("[{fam}] a row distinct on both pairs"),
        );
        assert_eq!(rows_in(&db, "dbl"), 2);
    }
}

/// THREE tables that each name their UNIQUE constraint `ux`. The registry key
/// is database-global; the constraint name is per-table in both PostgreSQL and
/// MySQL. Every table must enforce, and every 23505 must say `ux`.
///
/// The third table is not decoration: with a single global key the second
/// table is the only observable victim, and a fix that merely made the SECOND
/// registration fall back would still lose the third.
#[test]
fn three_tables_may_reuse_one_constraint_name_and_all_three_enforce() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        for table in ["n1", "n2", "n3"] {
            db.execute(&format!(
                "CREATE TABLE {table} (id INT PRIMARY KEY, a INT, b INT, CONSTRAINT ux UNIQUE (a, b))"
            ))
            .unwrap_or_else(|e| {
                panic!(
                    "[{fam}] CREATE TABLE {table} with a reused CONSTRAINT name must be accepted \
                     (its backing index key is minted, not the constraint name): {e}"
                )
            });
        }

        // Each table takes its OWN pair, so nothing below can pass or fail for
        // a reason another table caused.
        let tables = ["n1", "n2", "n3"];
        for (n, table) in tables.iter().enumerate() {
            let v = (n as i32 + 1) * 5;
            run(
                &db,
                &format!("INSERT INTO {table} (id, a, b) VALUES (1, {v}, {v})"),
                params_family,
            )
            .unwrap_or_else(|e| panic!("[{fam}] the first {table} row must insert: {e}"));

            let err = expect_duplicate(
                &db,
                &format!("INSERT INTO {table} (id, a, b) VALUES (2, {v}, {v})"),
                params_family,
                &format!("[{fam}] {table}'s CONSTRAINT ux"),
            );
            // The LABEL the user wrote, not the minted key `{table}_a_b_key`.
            assert_names_constraint(&err, "ux", &format!("[{fam}] {table}"));

            assert_eq!(rows_in(&db, table), 1, "[{fam}] {table} stored the duplicate anyway");
        }

        // Each table's constraint is its OWN: the pair the NEXT table holds is
        // brand new here, so none of the rejections above can be one table's
        // index answering for another's.
        for (n, table) in tables.iter().enumerate() {
            let other = ((n + 1) % tables.len() + 1) as i32 * 5;
            expect_accepted(
                &db,
                &format!("INSERT INTO {table} (id, a, b) VALUES (3, {other}, {other})"),
                params_family,
                &format!("[{fam}] {table} rejecting a pair only ANOTHER table holds"),
            );
            assert_eq!(rows_in(&db, table), 2, "[{fam}] {table}");
        }
    }
}

/// An unnamed table-level constraint is recorded as `{table}_unique`, and THAT
/// — the name `DROP CONSTRAINT` takes and `pg_constraint` prints — is what its
/// 23505 must keep saying. Byte-for-byte what this shape reported before the
/// change, which is the point: minting the index key must not leak into the
/// message.
///
/// (The record name embedding the table but not the columns is a separate,
/// pre-existing wart — two unnamed constraints on one table share it. The
/// enforcement no longer depends on it; see the first test.)
#[test]
fn an_unnamed_constraint_still_reports_its_record_name() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE anon (id INT PRIMARY KEY, a INT, b INT, UNIQUE (a, b))")
            .unwrap();
        run(&db, "INSERT INTO anon (id, a, b) VALUES (1, 1, 1)", params_family).unwrap();
        let err = expect_duplicate(
            &db,
            "INSERT INTO anon (id, a, b) VALUES (2, 1, 1)",
            params_family,
            &format!("[{fam}] anon's unnamed UNIQUE (a, b)"),
        );
        assert_names_constraint(&err, "anon_unique", &format!("[{fam}] anon"));
    }
}

// ===========================================================================
// 2. Minting must not collide with — or evict — a user's own index
// ===========================================================================

/// A user `CREATE UNIQUE INDEX` squatting the name a constraint would mint.
/// The constraint must still be INSTALLED (under a suffixed key), not refused
/// and not silently dropped, and the user's index must survive.
///
/// This is why the mint has a free-name fallback instead of a refusal: a
/// refusal turns a name clash into a rejected CREATE, and — far worse — into a
/// store that cannot come back enforcing after a restart.
#[test]
fn a_squatted_derived_name_falls_back_and_both_rules_enforce() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE sq (id INT PRIMARY KEY, v INT, w INT)")
            .unwrap();
        // Occupy the exact key `ALTER TABLE sq ADD CONSTRAINT … UNIQUE (v)`
        // would mint, with an index over a DIFFERENT column.
        run(&db, "CREATE UNIQUE INDEX sq_v_key ON sq (w)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] the squatting index must be creatable: {e}"));
        run(&db, "ALTER TABLE sq ADD CONSTRAINT sq_c UNIQUE (v)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] the constraint must be installed under a fallback key: {e}"));

        run(&db, "INSERT INTO sq (id, v, w) VALUES (1, 1, 1)", params_family).unwrap();
        // The user's index (on w) still enforces…
        expect_duplicate(
            &db,
            "INSERT INTO sq (id, v, w) VALUES (2, 2, 1)",
            params_family,
            &format!("[{fam}] the user's CREATE UNIQUE INDEX on w"),
        );
        // …and so does the constraint whose key had to be suffixed (on v).
        let err = expect_duplicate(
            &db,
            "INSERT INTO sq (id, v, w) VALUES (3, 1, 2)",
            params_family,
            &format!("[{fam}] the constraint whose derived key was squatted"),
        );
        assert_names_constraint(&err, "sq_c", &format!("[{fam}] sq"));
        assert_eq!(rows_in(&db, "sq"), 1, "[{fam}] a rejected row was stored");
    }
}

/// `ALTER TABLE … ADD CONSTRAINT … UNIQUE` on a table that ALREADY holds
/// duplicates must fail 23505 and leave NOTHING behind.
///
/// The single most dangerous line of this change: the backfill and its
/// rollback must address the MINTED key, not the constraint name. Addressing
/// the constraint name after the key is minted silently targets a nonexistent
/// index — the backfill would "succeed", leaving an EMPTY unique index over a
/// table full of duplicates, which is a brand-new fail-open. Proven by the
/// third leg: after the failure the table still accepts a duplicate.
#[test]
fn add_constraint_over_existing_duplicates_fails_and_leaves_no_index() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE bf (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("INSERT INTO bf (id, v) VALUES (1, 7)").unwrap();
        db.execute("INSERT INTO bf (id, v) VALUES (2, 7)").unwrap();

        let err = run(&db, "ALTER TABLE bf ADD CONSTRAINT bf_v_uq UNIQUE (v)", params_family)
            .err()
            .unwrap_or_else(|| {
                panic!(
                    "*** UNENFORCED CONSTRAINT *** [{fam}] ADD CONSTRAINT over a table holding \
                     duplicates must FAIL — it reported success, so the constraint now claims to \
                     hold over data that violates it"
                )
            });
        assert_unique_violation(&err, &format!("[{fam}] backfill refusal"));

        // Nothing was left behind: the rows are intact and a THIRD duplicate is
        // still accepted (an empty index registered under the wrong name would
        // reject it, and a correctly-rolled-back one accepts it).
        assert_eq!(rows_in(&db, "bf"), 2, "[{fam}] the failed ALTER changed the rows");
        expect_accepted(
            &db,
            "INSERT INTO bf (id, v) VALUES (3, 7)",
            params_family,
            &format!("[{fam}] a constraint that FAILED to be added is enforcing anyway"),
        );
        assert_eq!(rows_in(&db, "bf"), 3);
    }
}

// ===========================================================================
// 3. DROP CONSTRAINT — Sprinter 3441d3e21453, all three legs RUN
// ===========================================================================

/// The Sprinter shape, verbatim. `CONSTRAINT gh21_u UNIQUE(u)` is
/// single-column, so the planner propagates it into `ColumnDef.unique`; the
/// CREATE path used to synthesise a SECOND record from that flag, so ONE
/// declaration left TWO claims and `DROP CONSTRAINT gh21_u` retired only one.
///
/// All three legs are EXECUTED, not predicted from the source:
///   (a) a duplicate `u` is ACCEPTED after the drop,
///   (b) the sibling `UNIQUE (a, b)` is STILL enforcing,
///   (c) both are still true after a persistent-directory reopen — the leg
///       that catches a stale column flag, because `rebuild_all_indexes`
///       re-derives a UNIQUE index for every `col.unique && !col.primary_key`
///       column at every open.
#[test]
fn drop_constraint_retires_the_whole_rule_and_survives_a_reopen() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_str().expect("utf-8 path").to_string();

        {
            let db = EmbeddedDatabase::new(&path).expect("open");
            db.execute(
                "CREATE TABLE gh21_m (id INT PRIMARY KEY, u INT, a INT, b INT, n INT, \
                 CONSTRAINT gh21_u UNIQUE(u), UNIQUE(a,b))",
            )
            .expect("gh21_m");
            run(
                &db,
                "INSERT INTO gh21_m (id, u, a, b, n) VALUES (1, 1, 1, 1, 1)",
                params_family,
            )
            .unwrap();

            // Both rules demonstrably enforce BEFORE the drop on THIS family,
            // so a pass afterwards can only be the drop's doing.
            // NB: no name assertion here. A single-column table-level UNIQUE is
            // enforced by the index `Catalog::create_table` builds from the
            // COLUMN FLAG the planner propagated, which carries no constraint
            // label — so this 23505 says `gh21_m_u_key`, exactly as it did
            // before this change. Naming it `gh21_u` would need the create-time
            // column-flag loop to see `TableConstraints`, which it does not;
            // that is a separate change with its own blast radius.
            expect_duplicate(
                &db,
                "INSERT INTO gh21_m (id, u, a, b, n) VALUES (2, 1, 8, 8, 2)",
                params_family,
                &format!("[{fam}] gh21_u BEFORE the drop"),
            );
            expect_duplicate(
                &db,
                "INSERT INTO gh21_m (id, u, a, b, n) VALUES (3, 9, 1, 1, 3)",
                params_family,
                &format!("[{fam}] the sibling UNIQUE (a, b) BEFORE the drop"),
            );

            run(&db, "ALTER TABLE gh21_m DROP CONSTRAINT gh21_u", params_family)
                .unwrap_or_else(|e| panic!("[{fam}] DROP CONSTRAINT must be supported on this family: {e}"));

            // (a) the retired rule really is gone…
            expect_accepted(
                &db,
                "INSERT INTO gh21_m (id, u, a, b, n) VALUES (4, 1, 4, 4, 4)",
                params_family,
                &format!("[{fam}] gh21_u AFTER the drop"),
            );
            // (b) …and the sibling it shares a table with is untouched…
            expect_duplicate(
                &db,
                "INSERT INTO gh21_m (id, u, a, b, n) VALUES (5, 5, 1, 1, 5)",
                params_family,
                &format!("[{fam}] the sibling UNIQUE (a, b) AFTER the drop"),
            );
            // …as is the PRIMARY KEY.
            expect_duplicate(
                &db,
                "INSERT INTO gh21_m (id, u, a, b, n) VALUES (1, 6, 6, 6, 6)",
                params_family,
                &format!("[{fam}] the PRIMARY KEY AFTER the drop"),
            );
            assert_eq!(rows_in(&db, "gh21_m"), 2, "[{fam}] a rejected row was stored");
        }

        // (c) …and a restart does not bring the retired rule back from the
        // column flag the planner propagated.
        let db = EmbeddedDatabase::new(&path).expect("reopen");
        assert_eq!(rows_in(&db, "gh21_m"), 2, "[{fam}] the pre-restart rows must survive");
        expect_accepted(
            &db,
            "INSERT INTO gh21_m (id, u, a, b, n) VALUES (6, 1, 7, 7, 7)",
            params_family,
            &format!("[{fam}] gh21_u came back after the reopen"),
        );
        expect_duplicate(
            &db,
            "INSERT INTO gh21_m (id, u, a, b, n) VALUES (7, 8, 1, 1, 8)",
            params_family,
            &format!("[{fam}] the sibling UNIQUE (a, b) after the reopen"),
        );
        assert_eq!(rows_in(&db, "gh21_m"), 3);
    }
}

/// NEGATIVE CONTROL for the test above, and the shape
/// `Catalog::drop_unique_constraint_indexes` has always had to protect: two
/// RECORDS, ONE index. `alter_table_add_unique` deliberately registers no
/// second index when the column set is already covered, so dropping the
/// redundant record must leave the inline `UNIQUE` enforcing — the column flag
/// must NOT be cleared here, because a surviving record still claims it.
#[test]
fn dropping_a_redundant_constraint_leaves_the_inline_unique_enforcing() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE nc (id INT PRIMARY KEY, v INT UNIQUE)")
            .unwrap();
        run(&db, "ALTER TABLE nc ADD CONSTRAINT nc_extra UNIQUE (v)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] a redundant UNIQUE constraint must be addable: {e}"));
        run(&db, "ALTER TABLE nc DROP CONSTRAINT nc_extra", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] DROP CONSTRAINT failed: {e}"));

        run(&db, "INSERT INTO nc (id, v) VALUES (1, 1)", params_family).unwrap();
        expect_duplicate(
            &db,
            "INSERT INTO nc (id, v) VALUES (2, 1)",
            params_family,
            &format!("[{fam}] the inline UNIQUE after a redundant constraint was dropped"),
        );
        assert_eq!(rows_in(&db, "nc"), 1);
    }
}

/// `DROP CONSTRAINT ux` must resolve the index to retire BY COLUMN SET. The old
/// candidate list was headed by the constraint's own NAME, so a user's
/// `CREATE UNIQUE INDEX ux ON t (w)` — a completely unrelated object on
/// different columns — was dropped instead. PostgreSQL keeps such an index;
/// only `DROP INDEX` owns it.
#[test]
fn drop_constraint_does_not_touch_a_like_named_user_index_on_other_columns() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE ui (id INT PRIMARY KEY, v INT, w INT)")
            .unwrap();
        run(&db, "CREATE UNIQUE INDEX ux ON ui (w)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] CREATE UNIQUE INDEX ux: {e}"));
        run(&db, "ALTER TABLE ui ADD CONSTRAINT ux UNIQUE (v)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] ADD CONSTRAINT ux UNIQUE (v): {e}"));
        run(&db, "ALTER TABLE ui DROP CONSTRAINT ux", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] DROP CONSTRAINT ux: {e}"));

        run(&db, "INSERT INTO ui (id, v, w) VALUES (1, 1, 1)", params_family).unwrap();
        // The dropped CONSTRAINT is gone…
        expect_accepted(
            &db,
            "INSERT INTO ui (id, v, w) VALUES (2, 1, 2)",
            params_family,
            &format!("[{fam}] the dropped CONSTRAINT ux on (v)"),
        );
        // …and the user's INDEX of the same name, on (w), is not.
        expect_duplicate(
            &db,
            "INSERT INTO ui (id, v, w) VALUES (3, 3, 1)",
            params_family,
            &format!("[{fam}] the user's CREATE UNIQUE INDEX ux on (w)"),
        );
        assert_eq!(rows_in(&db, "ui"), 2, "[{fam}] a rejected row was stored");
    }
}

// ===========================================================================
// 4. Upgrade — a store written by the previous binary
// ===========================================================================

/// A data directory written before this change must still OPEN and still
/// ENFORCE.
///
/// Nothing on disk changes shape, which is what makes this testable at all:
/// constraint index NAMES are persisted nowhere (`Schema` and
/// `TableConstraints` carry no index name; `meta:index:` records exist only for
/// an explicit `CREATE INDEX`), and no bincode struct gained a field — the
/// constraint label lives on `IndexEntry`, which derives `Debug, Clone` only.
/// So the entire persisted difference between an old store and a new one is the
/// EXTRA `{table}_{column}_unique` record the old CREATE path synthesised
/// alongside a single-column table-level `UNIQUE`.
///
/// `legacy` reproduces exactly that record set with statements this binary
/// still accepts — an inline `UNIQUE` (record + flag) plus an
/// `ADD CONSTRAINT` over the same column (a second record, no second index) —
/// so what is opened below is byte-for-byte the shape 4.31.x left behind.
/// `mixed` carries every other spelling in one directory so the reopen path is
/// exercised whole.
#[test]
fn a_store_written_before_this_change_still_opens_and_still_enforces() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_str().expect("utf-8 path").to_string();

    {
        let db = EmbeddedDatabase::new(&path).expect("open");
        // The pre-change duplicate-record shape.
        db.execute("CREATE TABLE legacy (id INT PRIMARY KEY, v INT UNIQUE)")
            .unwrap();
        db.execute("ALTER TABLE legacy ADD CONSTRAINT legacy_unique UNIQUE (v)")
            .unwrap();
        db.execute("INSERT INTO legacy (id, v) VALUES (1, 1)").unwrap();

        // Every other spelling, in one database, sharing column names — the
        // condition that made the global registry lose constraints.
        db.execute("CREATE TABLE m1 (id INT PRIMARY KEY, v INT UNIQUE)")
            .unwrap();
        db.execute("CREATE TABLE m2 (id INT PRIMARY KEY, v INT, UNIQUE (v))")
            .unwrap();
        db.execute("CREATE TABLE m3 (id INT PRIMARY KEY, v INT, w INT, CONSTRAINT ux UNIQUE (v, w))")
            .unwrap();
        db.execute("CREATE TABLE m4 (id INT PRIMARY KEY, v INT, w INT, CONSTRAINT ux UNIQUE (v, w))")
            .unwrap();
        db.execute("CREATE TABLE m5 (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("CREATE UNIQUE INDEX m5_v ON m5 (v)").unwrap();
        db.execute("ALTER TABLE m5 ADD CONSTRAINT m5_v_key UNIQUE (v)").unwrap();

        for table in ["m1", "m2", "m5"] {
            db.execute(&format!("INSERT INTO {table} (id, v) VALUES (1, 1)"))
                .unwrap();
        }
        for table in ["m3", "m4"] {
            db.execute(&format!("INSERT INTO {table} (id, v, w) VALUES (1, 1, 1)"))
                .unwrap();
        }
    }

    let db = EmbeddedDatabase::new(&path).expect("*** UPGRADE BROKE THE OPEN *** the store must still open");

    for table in ["legacy", "m1", "m2", "m3", "m4", "m5"] {
        assert_eq!(rows_in(&db, table), 1, "{table}: the pre-restart row must survive");
    }

    // Every spelling still enforces, on both families, and the rejection reads
    // as a UNIQUE violation — an index registered but NOT backfilled at open
    // would let the duplicate through instead.
    for params_family in [false, true] {
        let fam = family(params_family);
        for (n, table) in ["legacy", "m1", "m2", "m5"].iter().enumerate() {
            let id = 100 + n as i32;
            expect_duplicate(
                &db,
                &format!("INSERT INTO {table} (id, v) VALUES ({id}, 1)"),
                params_family,
                &format!("[{fam}] {table} after the reopen"),
            );
        }
        for (n, table) in ["m3", "m4"].iter().enumerate() {
            let id = 200 + n as i32;
            let err = expect_duplicate(
                &db,
                &format!("INSERT INTO {table} (id, v, w) VALUES ({id}, 1, 1)"),
                params_family,
                &format!("[{fam}] {table} after the reopen"),
            );
            assert_names_constraint(&err, "ux", &format!("[{fam}] {table} after the reopen"));
        }
    }
    for table in ["legacy", "m1", "m2", "m3", "m4", "m5"] {
        assert_eq!(
            rows_in(&db, table),
            1,
            "{table}: a rejected row was stored after the reopen"
        );
    }

    // Not rejecting everything: genuinely new values still land.
    for table in ["legacy", "m1", "m2", "m5"] {
        db.execute(&format!("INSERT INTO {table} (id, v) VALUES (9, 9)"))
            .unwrap_or_else(|e| panic!("{table}: a new value did not land after the reopen: {e}"));
    }
    for table in ["m3", "m4"] {
        db.execute(&format!("INSERT INTO {table} (id, v, w) VALUES (9, 9, 9)"))
            .unwrap_or_else(|e| panic!("{table}: a new value did not land after the reopen: {e}"));
    }

    // On the legacy double-record table, dropping ONE of the two records must
    // leave the other enforcing — the fail-CLOSED residue an old store carries,
    // stated here so it is a decision and not a surprise.
    db.execute("ALTER TABLE legacy DROP CONSTRAINT legacy_unique")
        .expect("DROP CONSTRAINT on a legacy double-record table");
    let err = db
        .execute("INSERT INTO legacy (id, v) VALUES (11, 9)")
        .err()
        .unwrap_or_else(|| panic!("*** UNENFORCED CONSTRAINT *** legacy's surviving inline UNIQUE stopped enforcing"));
    assert_unique_violation(&err, "legacy after dropping one of two records");
}

// ===========================================================================
// 5. Positive control — the harness measures enforcement, not blanket refusal
// ===========================================================================

/// Passes on ANY tree: no UNIQUE spelling, no ALTER, nothing this file fixes.
/// If it fails, the test file — not the engine — is the problem.
#[test]
fn positive_control_a_plain_column_still_accepts_a_repeat() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE ctl (id INT PRIMARY KEY, v INT)").unwrap();
        run(&db, "INSERT INTO ctl (id, v) VALUES (1, 1)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] POSITIVE CONTROL BROKEN: a plain insert failed: {e}"));
        run(&db, "INSERT INTO ctl (id, v) VALUES (2, 1)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] POSITIVE CONTROL BROKEN: a non-unique column rejected a repeat: {e}"));
        assert_eq!(rows_in(&db, "ctl"), 2, "[{fam}] POSITIVE CONTROL BROKEN");
        // …and the PRIMARY KEY, the one constraint that always worked, still
        // rejects its own duplicate — so "no error at all" cannot be why the
        // assertions above pass.
        let err = run(&db, "INSERT INTO ctl (id, v) VALUES (1, 9)", params_family)
            .err()
            .unwrap_or_else(|| panic!("[{fam}] POSITIVE CONTROL BROKEN: the PRIMARY KEY accepted a duplicate"));
        assert_unique_violation(&err, &format!("[{fam}] PRIMARY KEY"));
    }
}

// ===========================================================================
// 6. Candidate 3 — the REOPEN ordering must not let a constraint steal a
//    user index's name
// ===========================================================================

/// The in-memory half of this shape is proven in section 2
/// (`a_squatted_derived_name_falls_back_and_both_rules_enforce`): a user
/// `CREATE UNIQUE INDEX sq_v_key ON sq (w)` squats the key that
/// `ADD CONSTRAINT sq_c UNIQUE (v)` would mint, the constraint falls back to
/// `sq_v_key_2`, and both rules enforce. This is the SAME store after a close
/// and reopen.
///
/// `Catalog::rebuild_all_indexes` walks each table in one fixed order: the
/// column-flag indexes, then the constraint records
/// (src/storage/catalog.rs:1687), then the persisted `meta:index:`
/// definitions (src/storage/catalog.rs:1727). So at open the constraint on
/// (v) asks FIRST, finds `sq_v_key` free, and takes it; the user's own index
/// on (w) then reaches `create_unique_index(.., Some("sq_v_key"))`, hits
/// `IndexAlreadyExists`, and is logged at WARN. The store comes back with the
/// user's UNIQUE index enforced by NOTHING — a fail-open that exists only
/// after a restart, the one leg an in-memory test cannot see.
#[test]
fn reopen_keeps_a_user_index_whose_name_a_constraint_would_mint() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_str().expect("utf-8 path").to_string();

        {
            let db = EmbeddedDatabase::new(&path).expect("open");
            db.execute("CREATE TABLE sq (id INT PRIMARY KEY, v INT, w INT)")
                .unwrap();
            run(&db, "CREATE UNIQUE INDEX sq_v_key ON sq (w)", params_family)
                .unwrap_or_else(|e| panic!("[{fam}] the squatting index must be creatable: {e}"));
            run(&db, "ALTER TABLE sq ADD CONSTRAINT sq_c UNIQUE (v)", params_family)
                .unwrap_or_else(|e| panic!("[{fam}] the constraint must be installed under a fallback key: {e}"));
            run(&db, "INSERT INTO sq (id, v, w) VALUES (1, 1, 1)", params_family).unwrap();

            // Control: both rules demonstrably enforce BEFORE the close, on
            // this family, so a failure below can only be the reopen's doing.
            expect_duplicate(
                &db,
                "INSERT INTO sq (id, v, w) VALUES (2, 2, 1)",
                params_family,
                &format!("[{fam}] the user's CREATE UNIQUE INDEX sq_v_key on (w) BEFORE the reopen"),
            );
            expect_duplicate(
                &db,
                "INSERT INTO sq (id, v, w) VALUES (3, 1, 2)",
                params_family,
                &format!("[{fam}] CONSTRAINT sq_c on (v) BEFORE the reopen"),
            );
            assert_eq!(
                rows_in(&db, "sq"),
                1,
                "[{fam}] a rejected row was stored before the reopen"
            );
        }

        let db = EmbeddedDatabase::new(&path).expect("reopen");
        assert_eq!(rows_in(&db, "sq"), 1, "[{fam}] the pre-restart row must survive");

        // The defect: the user's index on (w) did not come back.
        expect_duplicate(
            &db,
            "INSERT INTO sq (id, v, w) VALUES (4, 4, 1)",
            params_family,
            &format!(
                "[{fam}] the user's CREATE UNIQUE INDEX sq_v_key on (w) AFTER the reopen — \
                 rebuild_all_indexes registers constraint records (src/storage/catalog.rs:1687) \
                 BEFORE the persisted meta:index: definitions (src/storage/catalog.rs:1727), so \
                 CONSTRAINT sq_c claimed the key sq_v_key for (v) and the user's index on (w) \
                 failed to register with IndexAlreadyExists"
            ),
        );
        // Control: the constraint on (v) enforces either way — under
        // `sq_v_key_2` when the order is right, under the stolen `sq_v_key`
        // when it is not — and still names itself.
        let err = expect_duplicate(
            &db,
            "INSERT INTO sq (id, v, w) VALUES (5, 1, 5)",
            params_family,
            &format!("[{fam}] CONSTRAINT sq_c on (v) AFTER the reopen"),
        );
        assert_names_constraint(&err, "sq_c", &format!("[{fam}] sq after the reopen"));
        assert_eq!(
            rows_in(&db, "sq"),
            1,
            "[{fam}] a rejected row was stored after the reopen"
        );

        // Neither rule over-rejects.
        expect_accepted(
            &db,
            "INSERT INTO sq (id, v, w) VALUES (6, 6, 6)",
            params_family,
            &format!("[{fam}] a row distinct on both v and w after the reopen"),
        );
        assert_eq!(rows_in(&db, "sq"), 2);
    }
}

/// The `DROP INDEX` invariant, inverted by the same reopen ordering.
///
/// After the reopen the key `sq_v_key` carries BOTH the durable `meta:index:`
/// definition the user's `CREATE UNIQUE INDEX` persisted AND the
/// constraint-owned tree on (v). `handle_drop_index` decides "user-created,
/// droppable" by exactly that pairing — UNIQUE kind plus a definition record
/// (src/sql/executor/ddl.rs:721) — so `DROP INDEX sq_v_key` is let through
/// and tears down (src/sql/executor/ddl.rs:772) the tree that is enforcing
/// CONSTRAINT sq_c. The constraint RECORD survives in the catalog; the
/// enforcement does not. PostgreSQL: a constraint-owned index is undroppable
/// (2BP01), and dropping the user's index removes the user's index and
/// nothing else.
///
/// Deliberately does not re-assert the (w) enforcement after the reopen — the
/// test above owns that failure — so this one reaches the DROP on today's
/// tree and fails on ITS OWN defect.
#[test]
fn drop_index_after_reopen_takes_only_the_user_index_not_the_constraint() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_str().expect("utf-8 path").to_string();

        {
            let db = EmbeddedDatabase::new(&path).expect("open");
            db.execute("CREATE TABLE sq (id INT PRIMARY KEY, v INT, w INT)")
                .unwrap();
            run(&db, "CREATE UNIQUE INDEX sq_v_key ON sq (w)", params_family)
                .unwrap_or_else(|e| panic!("[{fam}] the squatting index must be creatable: {e}"));
            run(&db, "ALTER TABLE sq ADD CONSTRAINT sq_c UNIQUE (v)", params_family)
                .unwrap_or_else(|e| panic!("[{fam}] the constraint must be installed under a fallback key: {e}"));
            run(&db, "INSERT INTO sq (id, v, w) VALUES (1, 1, 1)", params_family).unwrap();
            // Control: the constraint enforces BEFORE the close on this family.
            expect_duplicate(
                &db,
                "INSERT INTO sq (id, v, w) VALUES (2, 1, 2)",
                params_family,
                &format!("[{fam}] CONSTRAINT sq_c on (v) BEFORE the reopen"),
            );
            assert_eq!(
                rows_in(&db, "sq"),
                1,
                "[{fam}] a rejected row was stored before the reopen"
            );
        }

        let db = EmbeddedDatabase::new(&path).expect("reopen");
        assert_eq!(rows_in(&db, "sq"), 1, "[{fam}] the pre-restart row must survive");

        // The user's index is the user's to drop, in PostgreSQL and here.
        run(&db, "DROP INDEX sq_v_key", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] DROP INDEX of the user's own CREATE UNIQUE INDEX must succeed: {e}"));

        // The defect: DROP INDEX took the CONSTRAINT's enforcement with it.
        let err = expect_duplicate(
            &db,
            "INSERT INTO sq (id, v, w) VALUES (3, 1, 3)",
            params_family,
            &format!(
                "[{fam}] CONSTRAINT sq_c on (v) after `DROP INDEX sq_v_key` following a reopen — \
                 the reopen let the constraint claim sq_v_key (src/storage/catalog.rs:1687 runs before \
                 :1727), and handle_drop_index classifies UNIQUE-kind + meta:index: definition as \
                 user-created (src/sql/executor/ddl.rs:721) and tears the constraint's tree down \
                 (src/sql/executor/ddl.rs:772); the constraint record survives, its enforcement does not"
            ),
        );
        assert_names_constraint(&err, "sq_c", &format!("[{fam}] sq after DROP INDEX"));
        assert_eq!(
            rows_in(&db, "sq"),
            1,
            "[{fam}] a rejected row was stored after DROP INDEX"
        );

        // And the DROP really did remove the user's index on (w): a duplicate
        // w is now ACCEPTED, so the drop is not being reported as a no-op.
        expect_accepted(
            &db,
            "INSERT INTO sq (id, v, w) VALUES (4, 4, 1)",
            params_family,
            &format!("[{fam}] the user's index sq_v_key on (w) after it was dropped"),
        );
        assert_eq!(rows_in(&db, "sq"), 2, "[{fam}] the accepted row was not stored");
    }
}

// ===========================================================================
// 7. Candidate 3 — two tables whose DERIVED index names coincide
// ===========================================================================

/// `{table}_{cols}_key` joins with a plain underscore
/// (src/storage/art_manager.rs:486) and table names may contain underscores,
/// so `acct_grp (UNIQUE (nm))` and `acct (UNIQUE (grp, nm))` BOTH derive
/// `acct_grp_nm_key` — two ordinary tables, two unnamed constraints, no
/// user-chosen name anywhere. PostgreSQL accepts both.
///
/// Which side wins depends on creation ORDER, because the two spellings take
/// different paths. A single-column `UNIQUE (nm)` is lowered to the column
/// flag (src/sql/planner.rs:5324) and registered by `Catalog::create_table`,
/// whose pre-flight HARD-REFUSES a taken derived name
/// (src/storage/catalog.rs:520); the composite `UNIQUE (grp, nm)` goes
/// through `create_constraint_unique_index`, which falls back to `_2`
/// (src/storage/art_manager.rs:731). So `acct_grp` then `acct` is accepted,
/// and `acct` then `acct_grp` is REFUSED: the same two statements, rejected
/// or accepted by the order the user typed them in. Both orders run; both
/// must be accepted and both must enforce.
#[test]
fn two_tables_whose_derived_index_names_coincide_may_both_be_created() {
    const ACCT_GRP: &str = "CREATE TABLE acct_grp (id INT PRIMARY KEY, nm INT, UNIQUE (nm))";
    const ACCT: &str = "CREATE TABLE acct (id INT PRIMARY KEY, grp INT, nm INT, UNIQUE (grp, nm))";

    for params_family in [false, true] {
        let fam = family(params_family);
        for (order, first, second) in [
            ("acct_grp then acct", ACCT_GRP, ACCT),
            ("acct then acct_grp", ACCT, ACCT_GRP),
        ] {
            let db = mem_db();
            db.execute(first)
                .unwrap_or_else(|e| panic!("[{fam}] {order}: the FIRST table must be creatable: {e}"));
            db.execute(second).unwrap_or_else(|e| {
                panic!(
                    "*** REFUSED DDL *** [{fam}] {order}: the SECOND table must be creatable — its derived \
                     index name `acct_grp_nm_key` coincides with the first table's \
                     (src/storage/art_manager.rs:486), and Catalog::create_table refuses a column-flag \
                     index whose derived name is taken (src/storage/catalog.rs:520) instead of falling \
                     back the way create_constraint_unique_index does (src/storage/art_manager.rs:731): {e}"
                )
            });

            run(&db, "INSERT INTO acct_grp (id, nm) VALUES (1, 1)", params_family)
                .unwrap_or_else(|e| panic!("[{fam}] {order}: the first acct_grp row must insert: {e}"));
            run(&db, "INSERT INTO acct (id, grp, nm) VALUES (1, 1, 1)", params_family)
                .unwrap_or_else(|e| panic!("[{fam}] {order}: the first acct row must insert: {e}"));

            // Both rules enforce, whichever of them had to take the suffix.
            expect_duplicate(
                &db,
                "INSERT INTO acct_grp (id, nm) VALUES (2, 1)",
                params_family,
                &format!("[{fam}] {order}: acct_grp's UNIQUE (nm)"),
            );
            expect_duplicate(
                &db,
                "INSERT INTO acct (id, grp, nm) VALUES (2, 1, 1)",
                params_family,
                &format!("[{fam}] {order}: acct's UNIQUE (grp, nm)"),
            );
            assert_eq!(
                rows_in(&db, "acct_grp"),
                1,
                "[{fam}] {order}: acct_grp stored the duplicate anyway"
            );
            assert_eq!(
                rows_in(&db, "acct"),
                1,
                "[{fam}] {order}: acct stored the duplicate anyway"
            );

            // Each rule is its OWN. `acct (2, 1)` is a new PAIR — nm=1 alone is
            // held only by acct_grp — so acct's composite must accept it; a
            // single index answering for both tables would reject it.
            expect_accepted(
                &db,
                "INSERT INTO acct (id, grp, nm) VALUES (3, 2, 1)",
                params_family,
                &format!("[{fam}] {order}: acct rejecting a pair only acct_grp's single column holds"),
            );
            expect_accepted(
                &db,
                "INSERT INTO acct_grp (id, nm) VALUES (3, 2)",
                params_family,
                &format!("[{fam}] {order}: acct_grp rejecting a value it does not hold"),
            );
            assert_eq!(rows_in(&db, "acct"), 2, "[{fam}] {order}");
            assert_eq!(rows_in(&db, "acct_grp"), 2, "[{fam}] {order}");
        }
    }
}

/// The persistent half. Written in the order today's tree ACCEPTS (`acct_grp`
/// first, `acct` under the `_2` fallback), then closed and reopened.
///
/// At open `rebuild_all_indexes` walks tables in SORTED order
/// (src/storage/catalog.rs:1409) — `acct` before `acct_grp` — the REVERSE of
/// the creation order, so the composite constraint now mints
/// `acct_grp_nm_key` first and `acct_grp`'s column-flag registration
/// (src/storage/catalog.rs:1641) collides and is logged at WARN. What rescues
/// it, today, is double bookkeeping: a single-column table-level `UNIQUE (nm)`
/// also leaves a `TableConstraints` record (`acct_grp_unique`), and the record
/// path (src/storage/catalog.rs:1687) falls back to `_2`. That is an accident
/// of the two records, not a design, and it is order-dependent by
/// construction; this test pins the OBSERVABLE — both rules enforce after the
/// reopen — so that however candidate 3 reorders the open, the store still
/// comes back enforcing.
#[test]
fn two_tables_whose_derived_index_names_coincide_both_enforce_after_a_reopen() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_str().expect("utf-8 path").to_string();

        {
            let db = EmbeddedDatabase::new(&path).expect("open");
            db.execute("CREATE TABLE acct_grp (id INT PRIMARY KEY, nm INT, UNIQUE (nm))")
                .unwrap();
            db.execute("CREATE TABLE acct (id INT PRIMARY KEY, grp INT, nm INT, UNIQUE (grp, nm))")
                .unwrap_or_else(|e| panic!("[{fam}] acct after acct_grp is the order today's tree accepts: {e}"));
            run(&db, "INSERT INTO acct_grp (id, nm) VALUES (1, 1)", params_family).unwrap();
            run(&db, "INSERT INTO acct (id, grp, nm) VALUES (1, 1, 1)", params_family).unwrap();

            // Control: both enforce BEFORE the close on this family.
            expect_duplicate(
                &db,
                "INSERT INTO acct_grp (id, nm) VALUES (2, 1)",
                params_family,
                &format!("[{fam}] acct_grp's UNIQUE (nm) BEFORE the reopen"),
            );
            expect_duplicate(
                &db,
                "INSERT INTO acct (id, grp, nm) VALUES (2, 1, 1)",
                params_family,
                &format!("[{fam}] acct's UNIQUE (grp, nm) BEFORE the reopen"),
            );
        }

        let db = EmbeddedDatabase::new(&path).expect("reopen");
        assert_eq!(
            rows_in(&db, "acct_grp"),
            1,
            "[{fam}] acct_grp's pre-restart row must survive"
        );
        assert_eq!(rows_in(&db, "acct"), 1, "[{fam}] acct's pre-restart row must survive");

        expect_duplicate(
            &db,
            "INSERT INTO acct_grp (id, nm) VALUES (3, 1)",
            params_family,
            &format!(
                "[{fam}] acct_grp's UNIQUE (nm) AFTER the reopen — rebuild_all_indexes walks tables \
                 sorted (src/storage/catalog.rs:1409), acct's composite constraint minted \
                 acct_grp_nm_key first, and acct_grp's column-flag registration \
                 (src/storage/catalog.rs:1641) collided with nothing rescuing it"
            ),
        );
        expect_duplicate(
            &db,
            "INSERT INTO acct (id, grp, nm) VALUES (3, 1, 1)",
            params_family,
            &format!(
                "[{fam}] acct's UNIQUE (grp, nm) AFTER the reopen — the constraint record path \
                 (src/storage/catalog.rs:1687) lost the derived name acct_grp_nm_key to acct_grp"
            ),
        );
        assert_eq!(
            rows_in(&db, "acct_grp"),
            1,
            "[{fam}] acct_grp stored a duplicate after the reopen"
        );
        assert_eq!(
            rows_in(&db, "acct"),
            1,
            "[{fam}] acct stored a duplicate after the reopen"
        );

        // Not rejecting everything: new values land on both.
        expect_accepted(
            &db,
            "INSERT INTO acct_grp (id, nm) VALUES (4, 4)",
            params_family,
            &format!("[{fam}] acct_grp rejecting a new value after the reopen"),
        );
        expect_accepted(
            &db,
            "INSERT INTO acct (id, grp, nm) VALUES (4, 4, 1)",
            params_family,
            &format!("[{fam}] acct rejecting a new pair after the reopen"),
        );
        assert_eq!(rows_in(&db, "acct_grp"), 2, "[{fam}]");
        assert_eq!(rows_in(&db, "acct"), 2, "[{fam}]");
    }
}

// ===========================================================================
// 8. Candidate 3 — positive controls for the reopen harness
// ===========================================================================

/// Passes on the tree BEFORE and AFTER candidate 3. Every UNIQUE spelling
/// that has NO name clash anywhere — a table-level `UNIQUE (v)`, an inline
/// `v INT UNIQUE` with no table-level twin, a user `CREATE UNIQUE INDEX` under
/// a name no constraint would mint, and the PRIMARY KEY on each — survives a
/// close and reopen and still rejects. If any leg here fails, the reopen
/// harness — not the engine — is what sections 6 and 7 are measuring.
#[test]
fn positive_control_unclashed_unique_spellings_survive_a_reopen() {
    let tables = ["pc_tl", "pc_in", "pc_ux"];
    for params_family in [false, true] {
        let fam = family(params_family);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_str().expect("utf-8 path").to_string();

        {
            let db = EmbeddedDatabase::new(&path).expect("open");
            db.execute("CREATE TABLE pc_tl (id INT PRIMARY KEY, v INT, UNIQUE (v))")
                .unwrap();
            db.execute("CREATE TABLE pc_in (id INT PRIMARY KEY, v INT UNIQUE)")
                .unwrap();
            db.execute("CREATE TABLE pc_ux (id INT PRIMARY KEY, v INT)").unwrap();
            run(&db, "CREATE UNIQUE INDEX pc_ux_by_v ON pc_ux (v)", params_family)
                .unwrap_or_else(|e| panic!("[{fam}] POSITIVE CONTROL BROKEN: CREATE UNIQUE INDEX failed: {e}"));
            for table in tables {
                run(
                    &db,
                    &format!("INSERT INTO {table} (id, v) VALUES (1, 1)"),
                    params_family,
                )
                .unwrap_or_else(|e| panic!("[{fam}] POSITIVE CONTROL BROKEN: {table}'s first row failed: {e}"));
            }
        }

        let db = EmbeddedDatabase::new(&path).expect("POSITIVE CONTROL BROKEN: the store must reopen");
        for table in tables {
            assert_eq!(
                rows_in(&db, table),
                1,
                "[{fam}] POSITIVE CONTROL BROKEN: {table}'s pre-restart row must survive"
            );
            // The UNIQUE on v came back and still rejects…
            expect_duplicate(
                &db,
                &format!("INSERT INTO {table} (id, v) VALUES (2, 1)"),
                params_family,
                &format!("[{fam}] POSITIVE CONTROL BROKEN: {table}'s UNIQUE on v after the reopen"),
            );
            // …and so does the PRIMARY KEY.
            let sql = format!("INSERT INTO {table} (id, v) VALUES (1, 9)");
            let err = run(&db, &sql, params_family).err().unwrap_or_else(|| {
                panic!("[{fam}] POSITIVE CONTROL BROKEN: {table}'s PRIMARY KEY accepted a duplicate after the reopen")
            });
            assert_unique_violation(&err, &format!("[{fam}] {table}'s PRIMARY KEY after the reopen"));
            assert_eq!(
                rows_in(&db, table),
                1,
                "[{fam}] POSITIVE CONTROL BROKEN: {table} stored a rejected row after the reopen"
            );
            // A genuinely new row still lands, so "rejects everything" is not
            // how the two assertions above passed.
            expect_accepted(
                &db,
                &format!("INSERT INTO {table} (id, v) VALUES (2, 2)"),
                params_family,
                &format!("[{fam}] POSITIVE CONTROL BROKEN: {table} rejected a new value after the reopen"),
            );
            assert_eq!(rows_in(&db, table), 2, "[{fam}] POSITIVE CONTROL BROKEN: {table}");
        }
    }
}

// ===========================================================================
// 6. Crash recovery — a durable user index wins over a WAL-replayed mint
// ===========================================================================

/// The pass-1 guarantee of `Catalog::rebuild_all_indexes` — every durable
/// `meta:index:` name is registered BEFORE any constraint key is minted — only
/// covers the mints the rebuild itself makes. On the crash-recovery open,
/// `StorageEngine::open` runs `recover_wal_at_open` FIRST: a post-checkpoint
/// `CreateTable` entry whose `meta:table:` record never landed
/// (`log_create_table` appends the entry separately from the schema put, so a
/// crash between the two leaves exactly that) is replayed through
/// `Catalog::create_table`, which mints the table's column-flag key into the
/// still-empty registry. `CREATE UNIQUE INDEX acct_role_name_key ON z (x)`
/// (durable, checkpointed) then finds its own name taken when pass 1 reaches
/// it, `IndexAlreadyExists` is only warned, and the user's index enforces
/// NOTHING for the life of the process.
///
/// The window is modelled the way tests/wal_replay_upgrade_tests.rs models
/// crash recovery: the store is closed, the `CreateTable` entry is appended
/// through the ordinary `WriteAheadLog` API against the raw RocksDB handle,
/// `meta:table:acct_role` is deliberately NOT written, and the checkpoint is
/// planted just below the entry so it is unambiguously post-checkpoint.
///
/// Asserted on DATA, on the crash-recovery open AND on one more clean reopen
/// (the fix must converge, not merely survive one open):
///   - `acct_role` exists (the replay ran — control),
///   - a duplicate `x` on `z` is REJECTED (the defect),
///   - a duplicate `name` on `acct_role` is REJECTED (the constraint is still
///     enforced by exactly one tree after the mint is evicted and re-minted).
#[test]
fn durable_user_index_wins_over_a_wal_replayed_mint_on_crash_recovery() {
    use heliosdb_nano::storage::{WalOperation, WalSyncMode, WriteAheadLog};
    use heliosdb_nano::{Column, DataType, Schema};
    use std::sync::Arc;

    /// `WriteAheadLog::CHECKPOINT_KEY`, restated so the test fails loudly if
    /// the on-disk name ever changes without this test being revisited.
    const CHECKPOINT_KEY: &[u8] = b"wal:checkpoint";

    /// Raw RocksDB handle on a CLOSED store — the same 5-byte fixed prefix
    /// extractor `StorageEngine::open` configures, retried because the
    /// previous handle's background threads release the directory lock
    /// asynchronously.
    fn open_raw(dir: &std::path::Path) -> Arc<rocksdb::DB> {
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(false);
        opts.set_prefix_extractor(rocksdb::SliceTransform::create_fixed_prefix(5));
        let mut last_err = None;
        for _ in 0..100 {
            match rocksdb::DB::open(&opts, dir) {
                Ok(db) => return Arc::new(db),
                Err(e) => {
                    last_err = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
        panic!("raw RocksDB open failed: {:?}", last_err);
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_str().expect("utf-8 path").to_string();

    // (1) The durable store: a user UNIQUE index whose name is exactly what
    //     `acct_role (name UNIQUE)` derives, one row, closed cleanly.
    {
        let db = EmbeddedDatabase::new(&path).expect("open");
        db.execute("CREATE TABLE z (id INT PRIMARY KEY, x INT)").expect("z");
        db.execute("CREATE UNIQUE INDEX acct_role_name_key ON z (x)")
            .expect("the user index");
        db.execute("INSERT INTO z (id, x) VALUES (1, 1)").expect("seed row");
        // Control: the index enforces BEFORE the crash, so a pass after the
        // reopen can only be the rebuild's doing.
        expect_duplicate(
            &db,
            "INSERT INTO z (id, x) VALUES (2, 1)",
            false,
            "z.x before the crash",
        );
        assert_eq!(rows_in(&db, "z"), 1);
    }

    // (2) The crash window: the `CreateTable acct_role` entry is durable, its
    //     `meta:table:` record is not, and the checkpoint sits just below it.
    let planted_lsn = {
        let raw = open_raw(dir.path());
        let wal = WriteAheadLog::open(Arc::clone(&raw), WalSyncMode::Sync).expect("open wal");
        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4).primary_key(),
            Column::new("name", DataType::Int4).unique(),
        ]);
        let lsn = wal
            .append(WalOperation::CreateTable {
                table: "acct_role".to_string(),
                schema: bincode::serialize(&schema).expect("bincode schema"),
            })
            .expect("append CreateTable");

        // VACUITY GUARDS: the store must be in exactly the state this window
        // is about, else the reopen below proves nothing.
        assert!(
            raw.get(b"meta:table:acct_role").expect("read").is_none(),
            "vacuity: `meta:table:acct_role` must be ABSENT, or the replay skips the entry"
        );
        assert!(
            raw.get(b"meta:table:z").expect("read").is_some(),
            "vacuity: `z` must be durable before the reopen"
        );
        assert!(
            raw.get(b"meta:index:acct_role_name_key").expect("read").is_some(),
            "vacuity: the user index must have a durable `meta:index:` record"
        );
        let highest = wal
            .replay()
            .expect("read the retained log")
            .iter()
            .map(|e| e.lsn)
            .max()
            .expect("non-empty log");
        assert_eq!(
            highest, lsn,
            "vacuity: the planted entry must be the highest retained LSN"
        );

        // Everything strictly before the entry is checkpointed; the entry is not.
        raw.put(CHECKPOINT_KEY, (lsn - 1).to_le_bytes())
            .expect("plant checkpoint");
        lsn
    };
    assert!(planted_lsn > 0);

    // (3) The open under test, then ONE MORE clean reopen — the same three
    //     facts must hold on both.
    for round in 1..=2u32 {
        let db = EmbeddedDatabase::new(&path).unwrap_or_else(|e| panic!("reopen #{round}: {e}"));
        let ctx = |what: &str| format!("open #{round}: {what}");

        // Control: the replay ran and created the table.
        db.query("SELECT * FROM acct_role", &[]).unwrap_or_else(|e| {
            panic!(
                "{}: the WAL-replayed CreateTable did not create the table: {e}",
                ctx("control")
            )
        });

        // The defect: the durable user index on z must still enforce.
        assert_eq!(
            rows_in(&db, "z"),
            usize::try_from(round).expect("small"),
            "{}",
            ctx("z rows")
        );
        expect_duplicate(
            &db,
            "INSERT INTO z (id, x) VALUES (2, 1)",
            false,
            &ctx("the durable user index acct_role_name_key on z(x)"),
        );
        let new_id = i64::from(round) + 1;
        expect_accepted(
            &db,
            &format!("INSERT INTO z (id, x) VALUES ({new_id}, {new_id})"),
            false,
            &ctx("a distinct x on z"),
        );

        // Control: the replayed table's own UNIQUE is enforced too — by the
        // re-minted tree, not by nothing and not twice.
        let seed = i64::from(round) * 10;
        expect_accepted(
            &db,
            &format!("INSERT INTO acct_role (id, name) VALUES ({seed}, {round})"),
            false,
            &ctx("a fresh name on acct_role"),
        );
        expect_duplicate(
            &db,
            &format!("INSERT INTO acct_role (id, name) VALUES ({}, {round})", seed + 1),
            false,
            &ctx("the replayed acct_role.name UNIQUE"),
        );
        assert_eq!(
            rows_in(&db, "acct_role"),
            usize::try_from(round).expect("small"),
            "{}",
            ctx("a rejected acct_role row was stored anyway")
        );
    }
}
