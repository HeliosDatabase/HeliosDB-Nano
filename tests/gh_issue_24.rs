//! GH#24 — (1) unique-index lookups going stale after a long `UPDATE … RETURNING`
//! toggle of a nullable UNIQUE VARCHAR column between a value and NULL, and
//! (2) UNIQUE enforcement that depends on how old the table is.
//!
//! Reported against 3.58.1 through Prisma 7.10 / node-pg (extended protocol) and
//! psql (simple protocol):
//!
//!   1. A row updated ~8 times (`UPDATE … RETURNING`, one nullable UNIQUE
//!      `VARCHAR(39)` column toggling value ⇄ NULL, other columns changing)
//!      vanished from `WHERE login = 'danimoya'` while `WHERE login LIKE 'dani%'`
//!      still returned it. `findUnique` therefore returned null, the app inserted
//!      a second row with the same login, and the UNIQUE did not reject it.
//!   2. A FRESH table with an inline `UNIQUE` accepted a duplicate while an
//!      OLDER table with identical DDL (which had received two UPDATEs) rejected
//!      it.
//!
//! # The two mechanisms behind those symptoms
//!
//! Both were addressed by 79e2255; this file is the reported SHAPE, at the
//! reported LENGTH (>= 40 toggle rounds, not 8), across BOTH executor families,
//! so neither half can rot back in unobserved.
//!
//!   * **Symptom 1a — stale ART.** `UPDATE … RETURNING` with no open transaction
//!     is the params family's autocommit funnel
//!     (`execute_plan_with_params_inner`'s Update arm → the `else` branch around
//!     `StorageEngine::update_tuples_branch_aware`, src/lib.rs:15974). That
//!     storage funnel writes the row, its versions, the WAL record, the MV/SMFI
//!     deltas and the HNSW index — and touches NO ART index. Every row updated
//!     through it left the unique index pointing at the value the row used to
//!     hold, so `SELECT … WHERE login = <new value>` (an ART point lookup —
//!     src/sql/executor/scan.rs:396) missed the live row while the `LIKE` scan
//!     found it. The fix maintains ART in that arm (src/lib.rs:15925-15999),
//!     after the row is durable.
//!   * **Symptom 1b — a row rejected as a duplicate of ITSELF.** A single-column
//!     table-level `UNIQUE (login)` is recorded TWICE (the table-level constraint
//!     record plus the column flag `CREATE TABLE` derives from it), so
//!     `enforce_unique_on_update` validated the same row against the same column
//!     set twice and its intra-statement dedup — then keyed on
//!     `(columns, values)` only — reported the FIRST pass's own entry as a
//!     duplicate. The fix keys the dedup on the row id
//!     (`intra_statement_collision`, src/lib.rs:21348).
//!   * **Symptom 2 — unenforced-by-nothing.** `ArtIndexManager` keeps ONE global
//!     map keyed by index NAME and `Catalog::create_table` registered a
//!     column-level UNIQUE index under the BARE COLUMN NAME, so the first table
//!     to declare `login UNIQUE` owned the name `login` and every later table's
//!     registration failed with `IndexAlreadyExists` — logged at warn and
//!     swallowed. Constraint indexes are now `{table}_{cols}_key` and a
//!     registration failure fails the CREATE (src/storage/catalog.rs:507-640).
//!
//! # Reading a failure
//!
//! * `*** ROW VANISHED ***` / `= and LIKE disagree` → symptom 1a is back: ART
//!   maintenance is missing on some UPDATE funnel.
//! * `*** SELF-DUPLICATE ***` (an UPDATE that fails with a UNIQUE violation
//!   while it holds the only copy of the value) → symptom 1b is back.
//! * `*** UNENFORCED CONSTRAINT ***` → symptom 2 is back, or the index lost the
//!   value the row does hold.
//! * `*** PHANTOM CONSTRAINT ***` → the mirror image: a value the row no longer
//!   holds is still reserved by a stale index entry.
//!
//! Every case runs on BOTH executor families, because a fix in one says nothing
//! about the other:
//!   * text family — `db.execute()` → `execute_in_transaction_inner`
//!     (psql simple query, MySQL wire, embedded);
//!   * params family — `db.execute_params()` / `db.execute_params_returning()` →
//!     `execute_plan_with_params_inner` (the PostgreSQL EXTENDED protocol every
//!     real driver uses, plus REST/BaaS). Note `db.execute_returning()` is NOT
//!     the text family: it delegates to `execute_params_returning`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};

/// The issue's own length: "~8 updates" was where the reporter noticed it, and a
/// 40-round loop is what they asked for ("a reproducer of the index maintenance
/// on UPDATE would be valuable"). Each round is TWO updates (to NULL and back).
const ROUNDS: usize = 40;

/// The reported login value and a LIKE pattern that matches it and nothing else
/// in these tables.
const LOGIN: &str = "danimoya";
const LIKE_PATTERN: &str = "dani%";

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
/// keys 23505 on "duplicate key" / "unique constraint"). Deliberately not a bare
/// `contains("unique")`, which unrelated "Unsupported … Unique { … }" planner
/// errors would satisfy for the wrong reason.
fn assert_unique_violation(err: &heliosdb_nano::Error, context: &str) {
    let text = err.to_string().to_ascii_lowercase();
    assert!(
        text.contains("duplicate key") || text.contains("unique constraint"),
        "{context}: the error must read as a UNIQUE violation (23505 on the wire), got: {err}"
    );
}

/// Integer ids out of a result set, width-agnostic and sorted.
fn ids_of(rows: &[Tuple], context: &str) -> Vec<i64> {
    let mut ids: Vec<i64> = rows
        .iter()
        .map(|r| match r.values.first() {
            Some(Value::Int2(v)) => i64::from(*v),
            Some(Value::Int4(v)) => i64::from(*v),
            Some(Value::Int8(v)) => *v,
            other => panic!("{context}: expected an integer id, got {other:?}"),
        })
        .collect();
    ids.sort_unstable();
    ids
}

/// `WHERE login = '<value>'` as a LITERAL — the simple-protocol read path.
/// This is the lookup that goes through the ART point-lookup pushdown
/// (`try_index_point_lookup_for_scan`), i.e. the one that went stale.
fn eq_ids_literal(db: &EmbeddedDatabase, table: &str, value: &str) -> Vec<i64> {
    let sql = format!("SELECT id FROM {table} WHERE login = '{value}'");
    let rows = db.query(&sql, &[]).unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
    ids_of(&rows, &sql)
}

/// `WHERE login = $1` as a BOUND parameter — Prisma's `findUnique`, the
/// extended-protocol read path.
fn eq_ids_params(db: &EmbeddedDatabase, table: &str, value: &str) -> Vec<i64> {
    let sql = format!("SELECT id FROM {table} WHERE login = $1");
    let rows = db
        .query_params(&sql, &[Value::String(value.to_string())])
        .unwrap_or_else(|e| panic!("`{sql}` [{value}] failed: {e}"));
    ids_of(&rows, &sql)
}

/// `WHERE login LIKE '<pattern>'` — the ground truth. LIKE has no index
/// pushdown, so it is always a filtered scan of the stored rows; when it
/// disagrees with `=`, the index is lying.
fn like_ids(db: &EmbeddedDatabase, table: &str, pattern: &str) -> Vec<i64> {
    let sql = format!("SELECT id FROM {table} WHERE login LIKE '{pattern}'");
    let rows = db.query(&sql, &[]).unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
    ids_of(&rows, &sql)
}

/// The payload column of row 1, as text — the in-loop positive control that
/// proves the UPDATE under test really executed and really changed the row.
fn bio_of_row_1(db: &EmbeddedDatabase, table: &str) -> String {
    let sql = format!("SELECT bio FROM {table} WHERE id = 1");
    let rows = db.query(&sql, &[]).unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
    // Width/representation-agnostic, like `scalar_text` in
    // tests/prisma_p0_unique_on_conflict.rs: a VARCHAR can surface as
    // `Value::String` or (under a dictionary-encoded column) as some other
    // representation whose `Display` is still the text. Panicking on the
    // variant would turn a harness detail into a fake #24 failure. The one case
    // that MUST still panic is "no row at all", which is a real defect.
    match rows.first().and_then(|r| r.values.first()) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) => panic!("`{sql}`: the payload column is NULL — the UPDATE did not apply"),
        Some(other) => other.to_string(),
        None => panic!("*** ROW VANISHED *** `{sql}`: row 1 is not readable by primary key"),
    }
}

/// The three UNIQUE spellings that reach this shape in the wild, as
/// (label, DDL statements, table name).
///
///  * inline `UNIQUE` — hand-written DDL and most migration tools;
///  * table-level `UNIQUE (login)` — the spelling that is recorded TWICE and
///    produced the self-duplicate (symptom 1b);
///  * `CREATE UNIQUE INDEX` — what Prisma emits for every `@unique`.
fn spellings() -> Vec<(&'static str, Vec<String>, &'static str)> {
    vec![
        (
            "inline UNIQUE",
            vec![
                "CREATE TABLE p_inline (id INT PRIMARY KEY, login VARCHAR(39) UNIQUE, bio VARCHAR(64), n INT)"
                    .to_string(),
            ],
            "p_inline",
        ),
        (
            "table-level UNIQUE (login)",
            vec![
                "CREATE TABLE p_table (id INT PRIMARY KEY, login VARCHAR(39), bio VARCHAR(64), n INT, UNIQUE (login))"
                    .to_string(),
            ],
            "p_table",
        ),
        (
            "CREATE UNIQUE INDEX",
            vec![
                "CREATE TABLE p_idx (id INT PRIMARY KEY, login VARCHAR(39), bio VARCHAR(64), n INT)".to_string(),
                "CREATE UNIQUE INDEX p_idx_login_uidx ON p_idx (login)".to_string(),
            ],
            "p_idx",
        ),
    ]
}

/// Seed: the toggled row (id 1) plus a neighbour that keeps a second live entry
/// in the unique index and never matches the LIKE pattern.
fn seed(db: &EmbeddedDatabase, table: &str) {
    db.execute(&format!(
        "INSERT INTO {table} (id, login, bio, n) VALUES (1, '{LOGIN}', 'seed', 0)"
    ))
    .unwrap_or_else(|e| panic!("seeding {table} row 1: {e}"));
    db.execute(&format!(
        "INSERT INTO {table} (id, login, bio, n) VALUES (2, 'zzz-neighbour', 'seed', 0)"
    ))
    .unwrap_or_else(|e| panic!("seeding {table} row 2: {e}"));
}

/// One toggle half-round, on the family under test. `value = None` sets the
/// UNIQUE column to NULL. Other columns change too, exactly as reported.
/// Returns the affected-row count.
fn toggle(db: &EmbeddedDatabase, table: &str, params_family: bool, value: Option<&str>, bio: &str, n: i32) -> u64 {
    if params_family {
        // The Prisma shape: every value bound, `RETURNING` appended.
        let sql = format!("UPDATE {table} SET login = $1, bio = $2, n = $3 WHERE id = $4 RETURNING id");
        let login_param = match value {
            Some(v) => Value::String(v.to_string()),
            None => Value::Null,
        };
        let (count, rows) = db
            .execute_params_returning(
                &sql,
                &[
                    login_param,
                    Value::String(bio.to_string()),
                    Value::Int4(n),
                    Value::Int4(1),
                ],
            )
            .unwrap_or_else(|e| {
                panic!("*** SELF-DUPLICATE or lost row *** [params] `{sql}` (login={value:?}, n={n}) failed: {e}")
            });
        assert_eq!(
            rows.len(),
            1,
            "[params] `UPDATE … RETURNING id` must return the one updated row (n={n})"
        );
        count
    } else {
        let login_literal = match value {
            Some(v) => format!("'{v}'"),
            None => "NULL".to_string(),
        };
        let sql =
            format!("UPDATE {table} SET login = {login_literal}, bio = '{bio}', n = {n} WHERE id = 1 RETURNING id");
        db.execute(&sql)
            .unwrap_or_else(|e| panic!("*** SELF-DUPLICATE or lost row *** [text] `{sql}` failed: {e}"))
    }
}

// ===========================================================================
// Positive controls — these pass on ANY tree. If one of them fails, the
// harness (not the engine) is broken, and nothing else in this file means
// anything.
// ===========================================================================

/// The same 40-round `UPDATE … RETURNING` toggle loop on a table with NO unique
/// constraint at all, on both families. Nothing here can regress with the
/// UNIQUE machinery, so it proves the loop, the readers and the two families'
/// entry points all work.
#[test]
fn positive_control_a_forty_round_toggle_without_unique_is_consistent() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE plain (id INT PRIMARY KEY, login VARCHAR(39), bio VARCHAR(64), n INT)")
            .unwrap();
        seed(&db, "plain");

        for round in 0..ROUNDS {
            let n = round as i32;
            assert_eq!(
                toggle(&db, "plain", params_family, None, &format!("r{round}-null"), n),
                1,
                "[{fam}] round {round}: the NULL half must affect exactly one row"
            );
            assert_eq!(
                eq_ids_literal(&db, "plain", LOGIN),
                Vec::<i64>::new(),
                "[{fam}] round {round}: a NULLed column must not match `=`"
            );
            assert_eq!(
                like_ids(&db, "plain", LIKE_PATTERN),
                Vec::<i64>::new(),
                "[{fam}] round {round}: a NULLed column must not match LIKE"
            );
            assert_eq!(bio_of_row_1(&db, "plain"), format!("r{round}-null"));

            assert_eq!(
                toggle(&db, "plain", params_family, Some(LOGIN), &format!("r{round}-val"), n),
                1,
                "[{fam}] round {round}: the value half must affect exactly one row"
            );
            assert_eq!(
                eq_ids_literal(&db, "plain", LOGIN),
                vec![1],
                "[{fam}] round {round}: `=` must find the restored value"
            );
            assert_eq!(like_ids(&db, "plain", LIKE_PATTERN), vec![1]);
            assert_eq!(bio_of_row_1(&db, "plain"), format!("r{round}-val"));
        }
        assert_eq!(
            rows_in(&db, "plain"),
            2,
            "[{fam}] the loop must not change the row count"
        );
    }
}

/// Plain DML on a UNIQUE column with no toggling: the shapes this file relies on
/// (insert, duplicate rejection, distinct value accepted) work on any tree.
#[test]
fn positive_control_a_untouched_unique_column_still_behaves() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        // NOTE: `CREATE TABLE` deliberately runs on the TEXT family in every
        // test in this file. The params family has NO `CreateTable` arm — it
        // falls to the catch-all in `execute_plan_with_params_inner`
        // (src/lib.rs:16383) which hands the plan to `Executor::plan_to_operator`
        // (src/sql/executor/mod.rs:3394), and that match has no `CreateTable`
        // arm either, so `db.execute_params("CREATE TABLE …", &[])` returns
        // `Operator not yet implemented: CreateTable { … }`
        // (src/sql/executor/mod.rs:4952). That is a REAL parity gap, but it is
        // NOT issue #24 — running the DDL through it here would abort every
        // params pass before the constraint under test was ever exercised.
        // Same reason tests/prisma_p0_unique_on_conflict.rs uses `db.execute()`
        // for CREATE TABLE and `run()` only for DML / index DDL.
        db.execute("CREATE TABLE ctl (id INT PRIMARY KEY, login VARCHAR(39) UNIQUE, bio VARCHAR(64), n INT)")
            .unwrap();
        run(
            &db,
            &format!("INSERT INTO ctl (id, login, bio, n) VALUES (1, '{LOGIN}', 'a', 0)"),
            params_family,
        )
        .unwrap();
        let err = run(
            &db,
            &format!("INSERT INTO ctl (id, login, bio, n) VALUES (2, '{LOGIN}', 'b', 0)"),
            params_family,
        )
        .err()
        .unwrap_or_else(|| panic!("[{fam}] a fresh UNIQUE column must reject a duplicate"));
        assert_unique_violation(&err, fam);
        run(
            &db,
            "INSERT INTO ctl (id, login, bio, n) VALUES (3, 'other', 'c', 0)",
            params_family,
        )
        .unwrap_or_else(|e| panic!("[{fam}] a distinct value must still insert: {e}"));
        assert_eq!(rows_in(&db, "ctl"), 2, "[{fam}]");
        assert_eq!(eq_ids_literal(&db, "ctl", LOGIN), vec![1], "[{fam}]");
        assert_eq!(eq_ids_params(&db, "ctl", LOGIN), vec![1], "[{fam}]");
    }
}

// ===========================================================================
// (1) The reported shape: a long `UPDATE … RETURNING` toggle loop
// ===========================================================================

/// The issue's part (1), at 40 rounds (80 updates), over BOTH executor families
/// and all three UNIQUE spellings.
///
/// After EVERY half-round the three readers must agree and the constraint must
/// still enforce:
///   * `WHERE login = '<v>'` as a literal (simple protocol, ART point lookup),
///   * `WHERE login = $1` as a bound parameter (extended protocol / findUnique),
///   * `WHERE login LIKE 'dani%'` (a scan — the ground truth),
/// and a duplicate INSERT of the value the row currently holds must be rejected
/// on both families, alternating round by round.
///
/// Each spelling runs in its OWN fresh in-memory database with no earlier table
/// claiming the column name, so this test isolates index MAINTENANCE (symptom
/// 1a/1b). The registration half of the bug (symptom 2) is pinned separately
/// below, where an older claimant is deliberately created first.
#[test]
fn a_forty_round_returning_toggle_keeps_equality_lookups_and_unique_in_step() {
    for (label, ddl, table) in spellings() {
        for params_family in [false, true] {
            let fam = family(params_family);
            let ctx = format!("[{fam}/{label}]");
            let db = mem_db();
            for stmt in &ddl {
                db.execute(stmt)
                    .unwrap_or_else(|e| panic!("{ctx} `{stmt}` failed: {e}"));
            }
            seed(&db, table);

            for round in 0..ROUNDS {
                let n = round as i32;

                // --- half A: the UNIQUE column goes to NULL -----------------
                let affected = toggle(&db, table, params_family, None, &format!("r{round}-null"), n);
                assert_eq!(
                    affected, 1,
                    "{ctx} round {round}: the NULL half updated {affected} rows"
                );

                let eq_lit = eq_ids_literal(&db, table, LOGIN);
                let eq_par = eq_ids_params(&db, table, LOGIN);
                let like = like_ids(&db, table, LIKE_PATTERN);
                assert_eq!(
                    eq_lit, like,
                    "{ctx} round {round} (NULL half): `=` and LIKE disagree — = {eq_lit:?}, LIKE {like:?}"
                );
                assert_eq!(
                    eq_par, like,
                    "{ctx} round {round} (NULL half): `= $1` and LIKE disagree — = {eq_par:?}, LIKE {like:?}"
                );
                assert!(
                    like.is_empty(),
                    "{ctx} round {round}: the row holds NULL, so nothing may match — got {like:?}"
                );
                // Positive control: the statement really ran.
                assert_eq!(
                    bio_of_row_1(&db, table),
                    format!("r{round}-null"),
                    "{ctx} round {round}"
                );

                // --- half B: the UNIQUE column returns to its value ---------
                let affected = toggle(&db, table, params_family, Some(LOGIN), &format!("r{round}-val"), n);
                assert_eq!(
                    affected, 1,
                    "{ctx} round {round}: the value half updated {affected} rows"
                );

                let eq_lit = eq_ids_literal(&db, table, LOGIN);
                let eq_par = eq_ids_params(&db, table, LOGIN);
                let like = like_ids(&db, table, LIKE_PATTERN);
                assert_eq!(
                    eq_lit, like,
                    "*** ROW VANISHED *** {ctx} round {round}: `=` and LIKE disagree — = {eq_lit:?}, LIKE {like:?}"
                );
                assert_eq!(
                    eq_par, like,
                    "*** ROW VANISHED *** {ctx} round {round}: `= $1` (findUnique) and LIKE disagree — \
                     = {eq_par:?}, LIKE {like:?}"
                );
                assert_eq!(
                    eq_lit,
                    vec![1],
                    "{ctx} round {round}: the toggled row must be the one and only match"
                );
                assert_eq!(bio_of_row_1(&db, table), format!("r{round}-val"), "{ctx} round {round}");

                // --- the duplicate the application went on to insert --------
                // Alternate the family that attempts it, so both INSERT paths
                // are probed against the same index state.
                let dup_params = round % 2 == 1;
                let dup_sql = format!("INSERT INTO {table} (id, login, bio, n) VALUES (999, '{LOGIN}', 'dup', 0)");
                let err = run(&db, &dup_sql, dup_params).err().unwrap_or_else(|| {
                    panic!(
                        "*** UNENFORCED CONSTRAINT *** {ctx} round {round}: a duplicate login was accepted \
                         by the {} family",
                        family(dup_params)
                    )
                });
                assert_unique_violation(&err, &format!("{ctx} round {round}"));
                assert_eq!(
                    rows_in(&db, table),
                    2,
                    "{ctx} round {round}: the rejected duplicate was stored anyway"
                );
            }
        }
    }
}

/// The other half of correct index maintenance: while the column holds NULL the
/// value must be FREE (a second row may take it), and once the row toggles back
/// the value must be TAKEN again. A stale entry shows up as a phantom
/// constraint; a missing entry as an unenforced one.
///
/// Runs after a full 40-round loop so it is testing the index state the long
/// loop leaves behind, not a fresh one.
#[test]
fn after_forty_rounds_the_vacated_value_is_free_and_the_held_value_is_taken() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE tgl (id INT PRIMARY KEY, login VARCHAR(39) UNIQUE, bio VARCHAR(64), n INT)")
            .unwrap();
        seed(&db, "tgl");

        for round in 0..ROUNDS {
            let n = round as i32;
            toggle(&db, "tgl", params_family, None, &format!("r{round}-null"), n);
            toggle(&db, "tgl", params_family, Some(LOGIN), &format!("r{round}-val"), n);
        }

        // Park the column on NULL: the value is now unowned.
        toggle(&db, "tgl", params_family, None, "parked", 999);
        run(
            &db,
            &format!("INSERT INTO tgl (id, login, bio, n) VALUES (3, '{LOGIN}', 'claimed', 0)"),
            params_family,
        )
        .unwrap_or_else(|e| {
            panic!("*** PHANTOM CONSTRAINT *** [{fam}] the vacated value is still reserved by the index: {e}")
        });
        assert_eq!(
            eq_ids_literal(&db, "tgl", LOGIN),
            vec![3],
            "[{fam}] `=` must now resolve the NEW claimant only"
        );
        assert_eq!(eq_ids_params(&db, "tgl", LOGIN), vec![3], "[{fam}]");
        assert_eq!(like_ids(&db, "tgl", LIKE_PATTERN), vec![3], "[{fam}]");

        // And row 1 can no longer take it back — row 3 owns it.
        let refused = if params_family {
            db.execute_params_returning(
                "UPDATE tgl SET login = $1 WHERE id = $2 RETURNING id",
                &[Value::String(LOGIN.to_string()), Value::Int4(1)],
            )
            .err()
        } else {
            db.execute(&format!("UPDATE tgl SET login = '{LOGIN}' WHERE id = 1 RETURNING id"))
                .err()
        };
        let err = refused.unwrap_or_else(|| {
            panic!("*** UNENFORCED CONSTRAINT *** [{fam}] an UPDATE onto a value another row owns was accepted")
        });
        assert_unique_violation(&err, fam);
        assert_eq!(rows_in(&db, "tgl"), 3, "[{fam}]");
        assert_eq!(
            eq_ids_literal(&db, "tgl", LOGIN),
            vec![3],
            "[{fam}] the refused UPDATE must have changed nothing"
        );
    }
}

// ===========================================================================
// (2) State-dependent enforcement: a fresh table after an older identical one
// ===========================================================================

/// The issue's part (2): "a fresh table with an inline UNIQUE accepted a
/// duplicate, while an older table with identical DDL (that had received two
/// UPDATEs) rejected it."
///
/// The older table is created FIRST and is then UPDATED twice, exactly as
/// reported — remove the older table and this passes even on the unfixed tree,
/// which is the trap that let the defect ship.
#[test]
fn a_fresh_tables_inline_unique_enforces_after_an_older_identical_table() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();

        // The OLDER table — first claimant of the column name `login`.
        // Text family: see the CREATE TABLE note in the control above.
        db.execute("CREATE TABLE old_t (id INT PRIMARY KEY, login VARCHAR(39) UNIQUE NOT NULL, n INTEGER)")
            .unwrap();
        run(
            &db,
            "INSERT INTO old_t (id, login, n) VALUES (1, 'bob', 0)",
            params_family,
        )
        .unwrap();
        // …that has received two UPDATEs.
        run(&db, "UPDATE old_t SET n = 1 WHERE id = 1", params_family).unwrap();
        run(&db, "UPDATE old_t SET n = 2 WHERE id = 1", params_family).unwrap();
        assert!(
            run(
                &db,
                "INSERT INTO old_t (id, login, n) VALUES (2, 'bob', 0)",
                params_family
            )
            .is_err(),
            "[{fam}] the older table stopped enforcing its own UNIQUE"
        );

        // The FRESH table — identical DDL, same column name, created later.
        db.execute("CREATE TABLE new_t (id INT PRIMARY KEY, login VARCHAR(39) UNIQUE NOT NULL, n INTEGER)")
            .unwrap_or_else(|e| panic!("[{fam}] the second table with the same column name must be creatable: {e}"));
        run(
            &db,
            "INSERT INTO new_t (id, login, n) VALUES (1, 'bob', 0)",
            params_family,
        )
        .unwrap_or_else(|e| panic!("[{fam}] the fresh table must accept its first row: {e}"));

        let err = run(
            &db,
            "INSERT INTO new_t (id, login, n) VALUES (2, 'bob', 1)",
            params_family,
        )
        .err()
        .unwrap_or_else(|| panic!("*** UNENFORCED CONSTRAINT *** [{fam}] the FRESH table accepted a duplicate login"));
        assert_unique_violation(&err, fam);
        assert_eq!(rows_in(&db, "new_t"), 1, "[{fam}] the duplicate row was stored anyway");
        assert_eq!(
            eq_ids_literal(&db, "new_t", "bob"),
            vec![1],
            "[{fam}] `= 'bob'` must resolve exactly the one live row"
        );

        // The fresh table's UNIQUE also survives its own UPDATE traffic.
        run(
            &db,
            "INSERT INTO new_t (id, login, n) VALUES (3, 'carol', 0)",
            params_family,
        )
        .unwrap();
        let refused = if params_family {
            db.execute_params_returning(
                "UPDATE new_t SET login = $1 WHERE id = $2 RETURNING id",
                &[Value::String("bob".into()), Value::Int4(3)],
            )
            .err()
        } else {
            db.execute("UPDATE new_t SET login = 'bob' WHERE id = 3 RETURNING id")
                .err()
        };
        let err = refused.unwrap_or_else(|| {
            panic!("*** UNENFORCED CONSTRAINT *** [{fam}] an UPDATE created a duplicate on the fresh table")
        });
        assert_unique_violation(&err, fam);
        assert_eq!(eq_ids_literal(&db, "new_t", "bob"), vec![1], "[{fam}]");
    }
}

/// The issue's literal part-(2) reproducer, verbatim DDL: quoted identifiers, a
/// `UUID` primary key and `VARCHAR(39) UNIQUE NOT NULL`, created after an older
/// table that already claimed the column name. The reporter got
/// `INSERT 0 1` where PostgreSQL gives 23505, and then `count(*) = 2`.
#[test]
fn the_issues_literal_uuid_table_rejects_the_second_bob_on_both_families() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();

        // An older table that already uses the column name `login`.
        db.execute(
            r#"CREATE TABLE "O_OLDER" ("id" UUID PRIMARY KEY, "login" VARCHAR(39) UNIQUE NOT NULL, "n" INTEGER)"#,
        )
        .unwrap_or_else(|e| panic!("[{fam}] the older table must be creatable: {e}"));
        db.execute(r#"INSERT INTO "O_OLDER" VALUES ('11111111-1111-4111-8111-111111111111','bob',0)"#)
            .unwrap_or_else(|e| panic!("[{fam}] older-table seed: {e}"));

        // The issue's own statements.
        db.execute(
            r#"CREATE TABLE "O_UNIQUE_NOT_NULL" ("id" UUID PRIMARY KEY, "login" VARCHAR(39) UNIQUE NOT NULL, "n" INTEGER)"#,
        )
        .unwrap_or_else(|e| panic!("[{fam}] CREATE TABLE from the issue failed: {e}"));
        run(
            &db,
            r#"INSERT INTO "O_UNIQUE_NOT_NULL" VALUES ('55555555-5555-4555-8555-555555555555','bob',0)"#,
            params_family,
        )
        .unwrap_or_else(|e| panic!("[{fam}] the first row must insert: {e}"));

        let err = run(
            &db,
            r#"INSERT INTO "O_UNIQUE_NOT_NULL" VALUES ('77777777-7777-4777-8777-777777777777','bob',1)"#,
            params_family,
        )
        .err()
        .unwrap_or_else(|| {
            panic!("*** UNENFORCED CONSTRAINT *** [{fam}] the issue's second INSERT was accepted (expected 23505)")
        });
        assert_unique_violation(&err, fam);
        assert_eq!(
            rows_in(&db, r#""O_UNIQUE_NOT_NULL""#),
            1,
            "[{fam}] the issue's `SELECT count(*) … WHERE login = 'bob'` must be 1, not 2"
        );
    }
}

// ===========================================================================
// (3) RESIDUAL — the same "silently unenforced UNIQUE" class the issue calls
//     "state-dependent enforcement", on the ONE registration path 79e2255 did
//     not harden. THESE TWO TESTS ARE EXPECTED TO FAIL ON THE CURRENT TREE.
// ===========================================================================
//
// `Catalog::create_table` was fixed to fail CLOSED: constraint indexes are
// named `{table}_{cols}_key` (src/storage/art_manager.rs:465) and a name that
// is already taken aborts the CREATE (src/storage/catalog.rs:507-537), with an
// unwind if registration still fails (src/storage/catalog.rs:604-640).
//
// But that pre-flight only walks `schema.columns[i].unique` — the COLUMN-FLAG
// spelling. TABLE-LEVEL constraints (`UNIQUE (a, b)`, and any
// `CONSTRAINT <name> UNIQUE (…)`) are registered by a DIFFERENT function,
// `Catalog::register_unique_constraint_indexes` (src/storage/catalog.rs:738),
// which:
//   * names the index after the CONSTRAINT, and the default constraint name a
//     `CREATE TABLE` arm mints for an unnamed table-level UNIQUE is
//     `format!("{}_unique", table)` (src/lib.rs:5191) — it embeds the table but
//     NOT the columns, so two unnamed table-level UNIQUE constraints on ONE
//     table both ask for the index name `{table}_unique`; and
//   * still swallows the collision: `create_unique_index` returns
//     `IndexAlreadyExists` (src/storage/art_manager.rs:629) and the match arm
//     logs it at `tracing::debug!` and CONTINUES
//     (src/storage/catalog.rs:756-759) — the exact warn-and-swallow shape that
//     produced symptom 2 in the first place.
//
// Net effect: the SECOND table-level UNIQUE of such a table is enforced by
// nothing, on both executor families, while the first one works — "UNIQUE
// enforcement is state-dependent", still true, just reached by a different
// spelling than the reporter's.
//
// Fail-closed direction (same as `Catalog::create_table` already takes):
// generate `{table}_{cols}_key` for an unnamed table-level constraint, and
// REFUSE the CREATE when a named one collides with a live index — PostgreSQL
// refuses too ("relation \"ux\" already exists"), because a constraint's
// backing index shares the relation namespace.

/// Two unnamed table-level UNIQUE constraints on ONE table. Both are minted
/// with the constraint name `{table}_unique`, so only the FIRST gets an index.
///
/// EXPECTED TO FAIL on the current tree: the `(c, d)` duplicate is accepted.
#[test]
fn a_second_unnamed_table_level_unique_on_the_same_table_is_still_enforced() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE dbl (id INT PRIMARY KEY, a INT, b INT, c INT, d INT, UNIQUE (a, b), UNIQUE (c, d))")
            .unwrap();

        run(
            &db,
            "INSERT INTO dbl (id, a, b, c, d) VALUES (1, 1, 1, 1, 1)",
            params_family,
        )
        .unwrap();

        // Control: the FIRST constraint IS enforced. If this ever fails the
        // test is no longer isolating the second-constraint gap.
        let first = run(
            &db,
            "INSERT INTO dbl (id, a, b, c, d) VALUES (2, 1, 1, 9, 9)",
            params_family,
        )
        .err()
        .unwrap_or_else(|| panic!("[{fam}] control: the FIRST `UNIQUE (a, b)` must be enforced"));
        assert_unique_violation(&first, &format!("{fam}/first constraint"));

        // The defect: the SECOND constraint's index was never registered.
        let second = run(
            &db,
            "INSERT INTO dbl (id, a, b, c, d) VALUES (3, 8, 8, 1, 1)",
            params_family,
        )
        .err()
        .unwrap_or_else(|| {
            panic!(
                "*** UNENFORCED CONSTRAINT *** [{fam}] the SECOND `UNIQUE (c, d)` accepted a duplicate — \
                     `register_unique_constraint_indexes` minted the index name `dbl_unique` for BOTH \
                     constraints and swallowed the `IndexAlreadyExists` at debug level \
                     (src/storage/catalog.rs:756)"
            )
        });
        assert_unique_violation(&second, &format!("{fam}/second constraint"));
        assert_eq!(rows_in(&db, "dbl"), 1, "[{fam}] a rejected row was stored anyway");
    }
}

/// Two tables that each name their UNIQUE constraint `ux`. PostgreSQL refuses
/// the second CREATE TABLE outright, because the constraint's backing index
/// lives in the relation namespace. Nano must either refuse it as well or mint
/// a distinct index name — what it must NOT do is accept the DDL and leave the
/// second table's constraint enforced by nothing.
///
/// EXPECTED TO FAIL on the current tree.
#[test]
fn an_identically_named_unique_constraint_on_a_second_table_is_not_silently_dropped() {
    for params_family in [false, true] {
        let fam = family(params_family);
        let db = mem_db();
        db.execute("CREATE TABLE n1 (id INT PRIMARY KEY, a INT, b INT, CONSTRAINT ux UNIQUE (a, b))")
            .unwrap();
        run(&db, "INSERT INTO n1 (id, a, b) VALUES (1, 1, 1)", params_family).unwrap();
        // Control: table 1's constraint works.
        let e1 = run(&db, "INSERT INTO n1 (id, a, b) VALUES (2, 1, 1)", params_family)
            .err()
            .unwrap_or_else(|| panic!("[{fam}] control: n1's named UNIQUE must be enforced"));
        assert_unique_violation(&e1, &format!("{fam}/n1"));

        // Either the CREATE is refused (PostgreSQL's answer, and fail-closed),
        // or the table exists WITH an enforced constraint. Silently accepting
        // the DDL and dropping the constraint is the one outcome that is wrong.
        let created = db.execute("CREATE TABLE n2 (id INT PRIMARY KEY, a INT, b INT, CONSTRAINT ux UNIQUE (a, b))");
        if created.is_err() {
            // Fail-closed: acceptable. Nothing further to assert.
            continue;
        }
        run(&db, "INSERT INTO n2 (id, a, b) VALUES (1, 5, 5)", params_family).unwrap();
        let e2 = run(&db, "INSERT INTO n2 (id, a, b) VALUES (2, 5, 5)", params_family)
            .err()
            .unwrap_or_else(|| {
                panic!(
                    "*** UNENFORCED CONSTRAINT *** [{fam}] `CREATE TABLE n2` succeeded but its \
                     `CONSTRAINT ux UNIQUE (a, b)` accepted a duplicate — the index name `ux` was already \
                     taken by n1 and `register_unique_constraint_indexes` swallowed the collision \
                     (src/storage/catalog.rs:756). Fail closed: refuse the CREATE, or mint \
                     `{{table}}_{{cols}}_key` for the backing index."
                )
            });
        assert_unique_violation(&e2, &format!("{fam}/n2"));
        assert_eq!(rows_in(&db, "n2"), 1, "[{fam}]");
    }
}
