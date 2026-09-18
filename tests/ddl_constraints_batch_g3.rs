//! Column-level DDL vs. the constraints the catalog claims to hold.
//!
//! Two sprinter items, one failure shape: a constraint record that survives —
//! or is never created — while nothing enforces it, so `\d` and
//! `information_schema` keep advertising a rule that every write ignores.
//!
//! # sprinter 885ffe24eab6 — `ADD COLUMN … UNIQUE / PRIMARY KEY / CHECK`
//!
//! `ALTER TABLE t ADD COLUMN u INT UNIQUE` added the column, copied `unique`
//! into the stored `Column`, created NO index and NO constraint record, and
//! raised no error (src/sql/planner.rs, the `AlterTableOperation::AddColumn`
//! arm: it desugared only an inline `REFERENCES`, per GH#27). `PRIMARY KEY`
//! behaved the same way, and `CHECK` was worse — `ColumnDef` has no slot for a
//! predicate, so `sql_column_def_to_column_def` dropped it on the floor.
//!
//! The UNIQUE case had a second edge that makes it a data-corruption risk
//! rather than a missing feature: the FLAG persists, and
//! `Catalog::rebuild_all_indexes` mints an enforcing index from every
//! `schema.columns[i].unique` at open. So the SAME statement produced an
//! unenforced constraint before a restart and an enforced one after it —
//! duplicates could be written in the first process and then made the table
//! un-reopenable-as-declared in the second.
//!
//! # sprinter 0f258ed23d13 — `RENAME COLUMN` / `DROP COLUMN`
//!
//! Both arms mutated `schema.columns[i].name` (or removed the entry) and
//! stopped. Two things went on naming the old column afterwards, and BOTH are
//! enforcement:
//!
//!   * the persisted `TableConstraints` record. At the next open,
//!     `register_unique_constraint_indexes` builds a tree over a column the
//!     schema no longer has; every probe resolves that name through
//!     `Schema::get_column_index` (exact match) to `None`, and
//!     `check_unique_constraints_tuple` SKIPS the index. A declared UNIQUE
//!     then accepts duplicates forever.
//!   * the LIVE ART entries, whose `columns` are resolved the same way on every
//!     write — so the same UNIQUE stops being enforced in the process that ran
//!     the rename, with no restart involved.
//!
//! The reopen-based tests below are the load-bearing ones: in-process behaviour
//! can be correct while the persisted record is wrong, so a test that never
//! reopens proves nothing about durability. They use a file-backed database and
//! a real `drop` + re-`open`.
//!
//! # Both executor families
//!
//! `db.execute()` is the text family (`execute_in_transaction_inner`: psql
//! simple query, MySQL wire, embedded, REPL). `db.execute_params()` is the
//! params family (`execute_plan_with_params_inner`: the PG EXTENDED protocol
//! every server-side-binding driver uses, plus REST/BaaS). The ALTER arms were
//! duplicated between them until this change, which is exactly how a fix like
//! this gets half-applied; every behavioural test here runs on both.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{Config, EmbeddedDatabase, Value};
use std::path::Path;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Family {
    Text,
    Params,
}

impl Family {
    const ALL: [Family; 2] = [Family::Text, Family::Params];

    fn label(self) -> &'static str {
        match self {
            Family::Text => "text",
            Family::Params => "params",
        }
    }
}

/// Run one statement through the requested executor family.
fn run(db: &EmbeddedDatabase, sql: &str, family: Family) -> heliosdb_nano::Result<u64> {
    match family {
        Family::Text => db.execute(sql),
        Family::Params => db.execute_params(sql, &[]),
    }
}

fn ok(db: &EmbeddedDatabase, sql: &str, family: Family) {
    if let Err(e) = run(db, sql, family) {
        panic!("[{}] `{sql}` should have succeeded: {e}", family.label());
    }
}

fn err(db: &EmbeddedDatabase, sql: &str, family: Family) -> String {
    match run(db, sql, family) {
        Ok(_) => panic!("[{}] `{sql}` should have been REFUSED", family.label()),
        Err(e) => format!("{e}"),
    }
}

fn config_for(dir: &Path) -> Config {
    let mut c = Config::default();
    c.storage.path = Some(dir.to_path_buf());
    c.storage.memory_only = false;
    c
}

/// Open the embedded database, retrying briefly: the previous handle's RocksDB
/// background threads release the directory lock asynchronously, so a
/// `drop` + immediate re-open can lose a race that has nothing to do with the
/// behaviour under test.
fn open_db(dir: &Path) -> EmbeddedDatabase {
    let mut last = None;
    for _ in 0..100 {
        match EmbeddedDatabase::with_config(config_for(dir)) {
            Ok(db) => return db,
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
    panic!("embedded open failed: {last:?}");
}

fn mem_db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory database")
}

/// Rows physically present. Deliberately NOT `SELECT COUNT(*)`: a count query
/// returns one row whether the count is 0 or 10,000, and every test here is
/// about rows that should not exist.
fn rows_in(db: &EmbeddedDatabase, table: &str) -> usize {
    let sql = format!("SELECT * FROM {table}");
    db.query(&sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
        .len()
}

/// Is `column` present in `table`'s CATALOG shape? Used to prove a REFUSED
/// `ADD COLUMN … <constraint>` left NOTHING behind — the atomicity half of
/// sprinter 885ffe24eab6.
///
/// Deliberately the projected column NAMES of `SELECT *`, not
/// `SELECT <column> FROM …`.is_err(): the tables this is asked about are
/// usually EMPTY (an `ADD COLUMN … PRIMARY KEY` is only legal on an empty one),
/// and "a query over an empty table returned no rows" is a vacuous absence
/// probe — it cannot tell "the column is gone" from "there was nothing to
/// scan". `SELECT *` expands against the catalog whether or not a row exists.
fn column_exists(db: &EmbeddedDatabase, table: &str, column: &str) -> bool {
    let (_, names) = db
        .query_with_columns(&format!("SELECT * FROM {table}"))
        .unwrap_or_else(|e| panic!("`SELECT * FROM {table}` failed: {e}"));
    names.iter().any(|n| n.eq_ignore_ascii_case(column))
}

// ===========================================================================
// 1. sprinter 885ffe24eab6 — ADD COLUMN … UNIQUE
// ===========================================================================

/// THE regression test. On the pre-fix tree the second INSERT is ACCEPTED: the
/// statement created no index and no constraint record, so nothing probed.
#[test]
fn add_column_unique_rejects_a_duplicate_on_both_families() {
    for family in Family::ALL {
        let db = mem_db();
        ok(&db, "CREATE TABLE au (id INT PRIMARY KEY)", Family::Text);
        ok(&db, "ALTER TABLE au ADD COLUMN u INT UNIQUE", family);
        ok(&db, "INSERT INTO au (id, u) VALUES (1, 5)", family);

        let message = err(&db, "INSERT INTO au (id, u) VALUES (2, 5)", family);
        assert!(
            message.to_lowercase().contains("unique") || message.contains("23505"),
            "[{}] *** UNENFORCED CONSTRAINT *** the duplicate was refused, but not as a UNIQUE \
             violation: {message}",
            family.label()
        );
        assert_eq!(
            rows_in(&db, "au"),
            1,
            "[{}] the duplicate must not be stored",
            family.label()
        );
        // NULLs are distinct under UNIQUE (PostgreSQL's default), so the
        // constraint must not have become "NOT NULL by accident".
        ok(&db, "INSERT INTO au (id) VALUES (3)", family);
        ok(&db, "INSERT INTO au (id) VALUES (4)", family);
        assert_eq!(rows_in(&db, "au"), 3, "[{}] two NULLs must coexist", family.label());
    }
}

/// The constraint must be DURABLE, and it must come back as exactly ONE
/// enforcing rule — the column flag and the constraint record both describe it,
/// and a second index would change the name a 23505 prints.
#[test]
fn add_column_unique_is_still_enforced_after_a_reopen() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    {
        let db = open_db(temp.path());
        ok(&db, "CREATE TABLE au (id INT PRIMARY KEY)", Family::Text);
        ok(&db, "ALTER TABLE au ADD COLUMN u INT UNIQUE", Family::Text);
        ok(&db, "INSERT INTO au (id, u) VALUES (1, 5)", Family::Text);
    }
    let db = open_db(temp.path());
    let message = err(&db, "INSERT INTO au (id, u) VALUES (2, 5)", Family::Text);
    assert!(
        message.to_lowercase().contains("unique") || message.contains("23505"),
        "after a reopen the duplicate was refused, but not as a UNIQUE violation: {message}"
    );
    assert_eq!(rows_in(&db, "au"), 1, "the duplicate must not be stored");
}

/// A named inline constraint keeps its name: the record is what
/// `DROP CONSTRAINT` resolves, so a name that was discarded would leave a
/// constraint the user cannot remove.
#[test]
fn add_column_unique_honours_an_explicit_constraint_name() {
    let db = mem_db();
    ok(&db, "CREATE TABLE anu (id INT PRIMARY KEY)", Family::Text);
    ok(
        &db,
        "ALTER TABLE anu ADD COLUMN u INT CONSTRAINT anu_u_uq UNIQUE",
        Family::Text,
    );
    ok(&db, "INSERT INTO anu (id, u) VALUES (1, 5)", Family::Text);
    err(&db, "INSERT INTO anu (id, u) VALUES (2, 5)", Family::Text);

    ok(&db, "ALTER TABLE anu DROP CONSTRAINT anu_u_uq", Family::Text);
    // Dropping the constraint must retire the enforcement with it.
    ok(&db, "INSERT INTO anu (id, u) VALUES (3, 5)", Family::Text);
    assert_eq!(rows_in(&db, "anu"), 2, "the duplicate is legal once the rule is gone");
}

/// A UNIQUE over a column the SAME statement fills with a literal DEFAULT is a
/// guaranteed duplicate. It must be refused BEFORE the column is added —
/// GH#27's validate-all-first rule, which this item extends to the other
/// inline constraints.
#[test]
fn add_column_unique_with_a_duplicating_default_is_refused_and_adds_nothing() {
    for family in Family::ALL {
        let db = mem_db();
        ok(&db, "CREATE TABLE ad (id INT PRIMARY KEY)", Family::Text);
        ok(&db, "INSERT INTO ad (id) VALUES (1)", Family::Text);
        ok(&db, "INSERT INTO ad (id) VALUES (2)", Family::Text);

        err(&db, "ALTER TABLE ad ADD COLUMN c INT DEFAULT 5 UNIQUE", family);
        assert!(
            !column_exists(&db, "ad", "c"),
            "[{}] *** HALF-APPLIED DDL *** the refused statement left the column behind",
            family.label()
        );
    }
}

// ===========================================================================
// 2. sprinter 885ffe24eab6 — ADD COLUMN … CHECK
// ===========================================================================

/// On the pre-fix tree the predicate is discarded silently and BOTH inserts
/// succeed.
#[test]
fn add_column_check_rejects_a_violating_row_on_both_families() {
    for family in Family::ALL {
        let db = mem_db();
        ok(&db, "CREATE TABLE ac (id INT PRIMARY KEY)", Family::Text);
        ok(&db, "ALTER TABLE ac ADD COLUMN c INT CHECK (c > 0)", family);

        ok(&db, "INSERT INTO ac (id, c) VALUES (1, 5)", family);
        let message = err(&db, "INSERT INTO ac (id, c) VALUES (2, -1)", family);
        assert!(
            message.to_uppercase().contains("CHECK"),
            "[{}] the row was refused, but not as a CHECK violation: {message}",
            family.label()
        );
        assert_eq!(
            rows_in(&db, "ac"),
            1,
            "[{}] the violating row must not be stored",
            family.label()
        );
        // SQL three-valued logic: an unknown CHECK passes, exactly as in
        // PostgreSQL. This is also why the ALTER itself succeeds against the
        // pre-existing rows, which all read NULL in the new column.
        ok(&db, "INSERT INTO ac (id) VALUES (3)", family);
    }
}

#[test]
fn add_column_check_is_still_enforced_after_a_reopen() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    {
        let db = open_db(temp.path());
        ok(&db, "CREATE TABLE ac (id INT PRIMARY KEY)", Family::Text);
        ok(&db, "ALTER TABLE ac ADD COLUMN c INT CHECK (c > 0)", Family::Text);
        ok(&db, "INSERT INTO ac (id, c) VALUES (1, 5)", Family::Text);
    }
    let db = open_db(temp.path());
    err(&db, "INSERT INTO ac (id, c) VALUES (2, -1)", Family::Text);
    ok(&db, "INSERT INTO ac (id, c) VALUES (3, 7)", Family::Text);
    assert_eq!(rows_in(&db, "ac"), 2, "only the violating row may be missing");
}

/// The rows already in the table are validated before the constraint is
/// recorded, so the catalog never claims a rule the data violates. The new
/// column reads NULL everywhere, so this one must SUCCEED — the assertion is
/// that validation happened and came out right, not that it refused.
#[test]
fn add_column_check_validates_the_existing_rows() {
    let db = mem_db();
    ok(&db, "CREATE TABLE acv (id INT PRIMARY KEY)", Family::Text);
    ok(&db, "INSERT INTO acv (id) VALUES (1)", Family::Text);
    ok(&db, "INSERT INTO acv (id) VALUES (2)", Family::Text);
    ok(&db, "ALTER TABLE acv ADD COLUMN c INT CHECK (c > 0)", Family::Text);
    assert_eq!(rows_in(&db, "acv"), 2, "the pre-existing NULL rows survive");
    err(&db, "INSERT INTO acv (id, c) VALUES (3, 0)", Family::Text);
}

/// `ALTER TABLE … ADD [CONSTRAINT n] CHECK (…)` is the statement that
/// re-creates a CHECK, and it used to report
/// `Unsupported ALTER TABLE operation: AddConstraint(Check { … })`. It is wired
/// alongside the ADD COLUMN desugaring because the RENAME/DROP COLUMN refusals
/// tell the user to drop a CHECK and re-create it — advice that needs a
/// statement that can.
#[test]
fn alter_table_add_constraint_check_is_enforced_on_both_families() {
    for family in Family::ALL {
        let db = mem_db();
        ok(&db, "CREATE TABLE acc (id INT PRIMARY KEY, n INT)", Family::Text);
        ok(&db, "ALTER TABLE acc ADD CONSTRAINT acc_n_pos CHECK (n > 0)", family);

        ok(&db, "INSERT INTO acc (id, n) VALUES (1, 5)", family);
        err(&db, "INSERT INTO acc (id, n) VALUES (2, -1)", family);
        assert_eq!(rows_in(&db, "acc"), 1, "[{}] the violating row", family.label());

        // …and DROP CONSTRAINT retires it again, which is the other half of the
        // recovery path.
        ok(&db, "ALTER TABLE acc DROP CONSTRAINT acc_n_pos", family);
        ok(&db, "INSERT INTO acc (id, n) VALUES (3, -1)", family);
    }
}

/// The rows already present are validated before the constraint is recorded:
/// the catalog must never claim a rule the data violates.
#[test]
fn alter_table_add_constraint_check_refuses_when_existing_rows_violate_it() {
    let db = mem_db();
    ok(&db, "CREATE TABLE accv (id INT PRIMARY KEY, n INT)", Family::Text);
    ok(&db, "INSERT INTO accv (id, n) VALUES (1, -5)", Family::Text);
    err(
        &db,
        "ALTER TABLE accv ADD CONSTRAINT accv_n_pos CHECK (n > 0)",
        Family::Text,
    );
    // The rejected constraint must not have been recorded — a row that
    // satisfies nothing new is still insertable.
    ok(&db, "INSERT INTO accv (id, n) VALUES (2, -7)", Family::Text);
    assert_eq!(rows_in(&db, "accv"), 2);
}

// ===========================================================================
// 3. sprinter 885ffe24eab6 — ADD COLUMN … PRIMARY KEY
// ===========================================================================

/// The one case with unambiguous semantics: no existing key, no existing rows.
/// It is ACCEPTED, and enforced IMMEDIATELY — the pre-fix tree set the flag and
/// created nothing, so the key only began enforcing after the next restart.
#[test]
fn add_column_primary_key_on_an_empty_table_is_enforced_at_once() {
    for family in Family::ALL {
        let db = mem_db();
        ok(&db, "CREATE TABLE apk (v INT)", Family::Text);
        ok(&db, "ALTER TABLE apk ADD COLUMN id INT PRIMARY KEY", family);

        ok(&db, "INSERT INTO apk (v, id) VALUES (10, 1)", family);
        err(&db, "INSERT INTO apk (v, id) VALUES (20, 1)", family);
        assert_eq!(
            rows_in(&db, "apk"),
            1,
            "[{}] the duplicate key must not be stored",
            family.label()
        );
        ok(&db, "INSERT INTO apk (v, id) VALUES (30, 2)", family);
    }
}

#[test]
fn add_column_primary_key_is_still_enforced_after_a_reopen() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    {
        let db = open_db(temp.path());
        ok(&db, "CREATE TABLE apk (v INT)", Family::Text);
        ok(&db, "ALTER TABLE apk ADD COLUMN id INT PRIMARY KEY", Family::Text);
        ok(&db, "INSERT INTO apk (v, id) VALUES (10, 1)", Family::Text);
    }
    let db = open_db(temp.path());
    err(&db, "INSERT INTO apk (v, id) VALUES (20, 1)", Family::Text);
    assert_eq!(rows_in(&db, "apk"), 1, "the duplicate key must not be stored");
}

/// A table may have ONE primary key. Setting a second `primary_key` flag is not
/// a harmless no-op: `rebuild_all_indexes` derives the PK index from EVERY
/// flagged column, so the table would come back after a restart with a
/// COMPOSITE key nobody declared.
#[test]
fn add_column_primary_key_is_refused_when_the_table_already_has_one() {
    for family in Family::ALL {
        let db = mem_db();
        ok(&db, "CREATE TABLE apk2 (id INT PRIMARY KEY)", Family::Text);

        let message = err(&db, "ALTER TABLE apk2 ADD COLUMN id2 INT PRIMARY KEY", family);
        assert!(
            message.to_lowercase().contains("primary key"),
            "[{}] the refusal must name the problem: {message}",
            family.label()
        );
        assert!(
            !column_exists(&db, "apk2", "id2"),
            "[{}] *** HALF-APPLIED DDL *** the refused statement left the column behind",
            family.label()
        );
    }
}

/// Every pre-existing row would read NULL (or the same DEFAULT) in the new
/// column, so the key cannot hold. PostgreSQL rejects this too; the one shape
/// it accepts — `ADD COLUMN id SERIAL PRIMARY KEY` — Nano cannot honour,
/// because SERIAL is filled at INSERT time and not by the ALTER.
#[test]
fn add_column_primary_key_is_refused_on_a_non_empty_table() {
    for family in Family::ALL {
        let db = mem_db();
        ok(&db, "CREATE TABLE apk3 (v INT)", Family::Text);
        ok(&db, "INSERT INTO apk3 (v) VALUES (1)", Family::Text);

        err(&db, "ALTER TABLE apk3 ADD COLUMN id INT PRIMARY KEY", family);
        assert!(
            !column_exists(&db, "apk3", "id"),
            "[{}] *** HALF-APPLIED DDL *** the refused statement left the column behind",
            family.label()
        );
    }
}

/// Two primary keys in ONE statement cannot be caught sub-plan by sub-plan (the
/// second is only impossible once the first has run), so it is a whole-statement
/// check. Nothing may be applied.
#[test]
fn two_primary_keys_in_one_statement_are_refused_before_anything_is_applied() {
    let db = mem_db();
    ok(&db, "CREATE TABLE apk4 (v INT)", Family::Text);
    err(
        &db,
        "ALTER TABLE apk4 ADD COLUMN a INT PRIMARY KEY, ADD COLUMN b INT PRIMARY KEY",
        Family::Text,
    );
    assert!(!column_exists(&db, "apk4", "a"), "*** HALF-APPLIED DDL *** on column a");
    assert!(!column_exists(&db, "apk4", "b"), "*** HALF-APPLIED DDL *** on column b");
}

/// GH#27's `REFERENCES` desugaring must keep working unchanged: this file
/// rewrote the loop it lives in.
#[test]
fn add_column_references_still_works() {
    let db = mem_db();
    ok(&db, "CREATE TABLE arp (k INT PRIMARY KEY)", Family::Text);
    ok(&db, "INSERT INTO arp (k) VALUES (1)", Family::Text);
    ok(&db, "CREATE TABLE arc (id INT PRIMARY KEY)", Family::Text);
    ok(&db, "ALTER TABLE arc ADD COLUMN kk INT REFERENCES arp(k)", Family::Text);

    ok(&db, "INSERT INTO arc (id, kk) VALUES (1, 1)", Family::Text);
    err(&db, "INSERT INTO arc (id, kk) VALUES (2, 999)", Family::Text);
}

// ===========================================================================
// 4. sprinter 0f258ed23d13 — RENAME COLUMN
// ===========================================================================

/// THE regression test for the item's headline. A COMPOSITE table-level UNIQUE
/// is used deliberately: a single-column `UNIQUE (v)` also sets the COLUMN
/// flag, which rides along with the rename, so it would keep working after a
/// restart for a reason that has nothing to do with the constraint record and
/// would make this test vacuous.
///
/// Pre-fix: the record still says `(a, b)` after `a` becomes `a2`, the index
/// rebuilt from it resolves nothing, and the duplicate is ACCEPTED.
#[test]
fn rename_column_keeps_a_composite_unique_enforced_across_a_reopen() {
    for family in Family::ALL {
        let temp = tempfile::TempDir::new().expect("temp dir");
        {
            let db = open_db(temp.path());
            ok(
                &db,
                "CREATE TABLE ru (id INT PRIMARY KEY, a INT, b INT, CONSTRAINT ru_ab UNIQUE (a, b))",
                Family::Text,
            );
            ok(&db, "INSERT INTO ru (id, a, b) VALUES (1, 7, 8)", Family::Text);
            ok(&db, "ALTER TABLE ru RENAME COLUMN a TO a2", family);

            // In-process, BEFORE any reopen: the live ART entry must have been
            // repointed too, or the rule stops applying immediately.
            err(&db, "INSERT INTO ru (id, a2, b) VALUES (2, 7, 8)", family);
            assert_eq!(
                rows_in(&db, "ru"),
                1,
                "[{}] in-process: the duplicate must not be stored",
                family.label()
            );
        }

        let db = open_db(temp.path());
        let message = err(&db, "INSERT INTO ru (id, a2, b) VALUES (3, 7, 8)", family);
        assert!(
            message.to_lowercase().contains("unique") || message.contains("23505"),
            "[{}] after a reopen the duplicate was refused, but not as a UNIQUE violation: {message}",
            family.label()
        );
        assert_eq!(
            rows_in(&db, "ru"),
            1,
            "[{}] *** SILENTLY UNENFORCED UNIQUE *** the renamed column's constraint stopped \
             applying after the restart",
            family.label()
        );
        // The rule is still on the PAIR, not on either column alone.
        ok(&db, "INSERT INTO ru (id, a2, b) VALUES (4, 7, 99)", family);
    }
}

/// The `ALTER TABLE … ADD CONSTRAINT … UNIQUE` spelling writes a record and NO
/// column flag, so it has nothing to fall back on — the record is the whole
/// constraint.
#[test]
fn rename_column_keeps_an_alter_added_unique_enforced_across_a_reopen() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    {
        let db = open_db(temp.path());
        ok(&db, "CREATE TABLE ru2 (id INT PRIMARY KEY, v INT)", Family::Text);
        ok(&db, "ALTER TABLE ru2 ADD CONSTRAINT ru2_v_key UNIQUE (v)", Family::Text);
        ok(&db, "INSERT INTO ru2 (id, v) VALUES (1, 5)", Family::Text);
        ok(&db, "ALTER TABLE ru2 RENAME COLUMN v TO w", Family::Text);
    }
    let db = open_db(temp.path());
    err(&db, "INSERT INTO ru2 (id, w) VALUES (2, 5)", Family::Text);
    assert_eq!(
        rows_in(&db, "ru2"),
        1,
        "*** SILENTLY UNENFORCED UNIQUE *** an ALTER-added constraint stopped applying after the rename"
    );
}

/// A CHECK stores a SERIALIZED expression, not a column list. Renaming the
/// column it names must rewrite the stored body.
///
/// Pre-fix this is not a silent fail-open but a hard WRITE BLOCK: the stored
/// body still names `c`, and evaluating it against a schema that no longer has
/// `c` raises "Column 'c' not found in schema" for EVERY row written to the
/// table, valid or not.
#[test]
fn rename_column_rewrites_a_check_constraint_across_a_reopen() {
    for family in Family::ALL {
        let temp = tempfile::TempDir::new().expect("temp dir");
        {
            let db = open_db(temp.path());
            ok(
                &db,
                "CREATE TABLE rch (id INT PRIMARY KEY, c INT, CONSTRAINT rch_c_pos CHECK (c > 0))",
                Family::Text,
            );
            ok(&db, "INSERT INTO rch (id, c) VALUES (1, 5)", Family::Text);
            ok(&db, "ALTER TABLE rch RENAME COLUMN c TO d", family);
            ok(&db, "INSERT INTO rch (id, d) VALUES (2, 6)", family);
        }
        let db = open_db(temp.path());
        ok(&db, "INSERT INTO rch (id, d) VALUES (3, 7)", family);
        err(&db, "INSERT INTO rch (id, d) VALUES (4, -1)", family);
        assert_eq!(
            rows_in(&db, "rch"),
            3,
            "[{}] the CHECK must still refuse exactly the violating row after the rename",
            family.label()
        );
    }
}

/// The PARENT side of a foreign key lives in the CHILD's record
/// (`references_columns`), so renaming a parent column has to reach into every
/// other table — the column-level analogue of `move_inbound_foreign_keys`.
///
/// Pre-fix, after the reopen, the child's record names a parent column that no
/// longer exists: the ART fast path finds no index for it and the scan
/// fallback matches nothing, so EVERY child insert is reported as a foreign-key
/// violation — valid parents included.
#[test]
fn rename_column_repoints_an_inbound_foreign_keys_referenced_side() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    {
        let db = open_db(temp.path());
        ok(&db, "CREATE TABLE rfp (k INT PRIMARY KEY)", Family::Text);
        ok(
            &db,
            "CREATE TABLE rfc (id INT PRIMARY KEY, kk INT REFERENCES rfp(k))",
            Family::Text,
        );
        ok(&db, "INSERT INTO rfp (k) VALUES (1)", Family::Text);
        ok(&db, "ALTER TABLE rfp RENAME COLUMN k TO k2", Family::Text);
    }
    let db = open_db(temp.path());
    ok(&db, "INSERT INTO rfc (id, kk) VALUES (1, 1)", Family::Text);
    err(&db, "INSERT INTO rfc (id, kk) VALUES (2, 999)", Family::Text);
    assert_eq!(
        rows_in(&db, "rfc"),
        1,
        "the child's foreign key must still admit exactly the rows with a real parent"
    );
}

/// A SELF-referencing foreign key has both ends on the renamed table.
#[test]
fn rename_column_repoints_both_ends_of_a_self_referencing_foreign_key() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    {
        let db = open_db(temp.path());
        ok(
            &db,
            "CREATE TABLE rsf (id INT PRIMARY KEY, parent_id INT REFERENCES rsf(id))",
            Family::Text,
        );
        ok(&db, "INSERT INTO rsf (id, parent_id) VALUES (1, NULL)", Family::Text);
        ok(&db, "ALTER TABLE rsf RENAME COLUMN id TO node_id", Family::Text);
    }
    let db = open_db(temp.path());
    ok(&db, "INSERT INTO rsf (node_id, parent_id) VALUES (2, 1)", Family::Text);
    err(
        &db,
        "INSERT INTO rsf (node_id, parent_id) VALUES (3, 999)",
        Family::Text,
    );
}

/// Renaming a column NO constraint mentions must stay the cheap metadata
/// operation it was — and must not start refusing.
#[test]
fn rename_column_untouched_by_any_constraint_still_works() {
    for family in Family::ALL {
        let db = mem_db();
        ok(&db, "CREATE TABLE rpl (id INT PRIMARY KEY, note TEXT)", Family::Text);
        ok(&db, "INSERT INTO rpl (id, note) VALUES (1, 'x')", Family::Text);
        ok(&db, "ALTER TABLE rpl RENAME COLUMN note TO comment", family);
        assert_eq!(rows_in(&db, "rpl"), 1, "[{}] the row survives", family.label());
        assert!(column_exists(&db, "rpl", "comment"), "[{}] renamed", family.label());
    }
}

// ===========================================================================
// 5. sprinter 0f258ed23d13 — DROP COLUMN
// ===========================================================================

/// PostgreSQL: "indexes and table constraints involving the column will be
/// automatically dropped". A composite `UNIQUE (a, b)` goes in FULL when `b`
/// goes — narrowing it to `UNIQUE (a)` would invent a rule nobody declared.
///
/// The second half is the one the pre-fix tree fails: with the record left
/// behind, re-adding a column called `b` makes the orphaned index resolve
/// again, so a constraint that was DROPPED starts rejecting rows.
#[test]
fn drop_column_retires_the_constraint_it_was_part_of() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    {
        let db = open_db(temp.path());
        ok(
            &db,
            "CREATE TABLE dcu (id INT PRIMARY KEY, a INT, b INT, CONSTRAINT dcu_ab UNIQUE (a, b))",
            Family::Text,
        );
        ok(&db, "INSERT INTO dcu (id, a, b) VALUES (1, 7, 8)", Family::Text);
        ok(&db, "ALTER TABLE dcu DROP COLUMN b", Family::Text);
    }
    let db = open_db(temp.path());
    ok(&db, "INSERT INTO dcu (id, a) VALUES (2, 7)", Family::Text);
    assert_eq!(rows_in(&db, "dcu"), 2, "UNIQUE (a, b) is not UNIQUE (a)");

    // Re-introducing the name must not resurrect the dropped rule.
    ok(&db, "ALTER TABLE dcu ADD COLUMN b INT", Family::Text);
    ok(&db, "INSERT INTO dcu (id, a, b) VALUES (3, 9, 9)", Family::Text);
    ok(&db, "INSERT INTO dcu (id, a, b) VALUES (4, 9, 9)", Family::Text);
    assert_eq!(
        rows_in(&db, "dcu"),
        4,
        "*** RESURRECTED CONSTRAINT *** a dropped UNIQUE started rejecting rows again once a \
         column with the old name reappeared"
    );
}

/// Pre-fix this is a hard WRITE BLOCK, not a fail-open: the orphaned CHECK body
/// still names the dropped column, and evaluating it raises
/// "Column 'c' not found in schema" for every row written to the table.
#[test]
fn drop_column_retires_the_check_constraint_that_named_it() {
    for family in Family::ALL {
        let db = mem_db();
        ok(
            &db,
            "CREATE TABLE dch (id INT PRIMARY KEY, c INT, CONSTRAINT dch_c_pos CHECK (c > 0))",
            Family::Text,
        );
        ok(&db, "INSERT INTO dch (id, c) VALUES (1, 5)", Family::Text);
        ok(&db, "ALTER TABLE dch DROP COLUMN c", family);

        ok(&db, "INSERT INTO dch (id) VALUES (2)", family);
        assert_eq!(
            rows_in(&db, "dch"),
            2,
            "[{}] *** WRITE BLOCKED *** the table became unwritable after the DROP COLUMN",
            family.label()
        );
    }
}

/// Another table's foreign key is a dependency PostgreSQL refuses to break
/// silently — and a child whose parent column has vanished is a constraint
/// enforced by nothing (or, on this engine, one that refuses every row). Refuse
/// without CASCADE; honour it with CASCADE.
#[test]
fn drop_column_refuses_when_an_inbound_foreign_key_depends_on_it() {
    let db = mem_db();
    ok(&db, "CREATE TABLE dfp (id INT PRIMARY KEY, k INT UNIQUE)", Family::Text);
    ok(
        &db,
        "CREATE TABLE dfc (id INT PRIMARY KEY, kk INT REFERENCES dfp(k))",
        Family::Text,
    );

    let message = err(&db, "ALTER TABLE dfp DROP COLUMN k", Family::Text);
    assert!(
        message.to_lowercase().contains("depend"),
        "the refusal must say what depends on the column: {message}"
    );
    assert!(
        column_exists(&db, "dfp", "k"),
        "*** HALF-APPLIED DDL *** the refused statement dropped the column anyway"
    );
}

#[test]
fn drop_column_cascade_retires_the_inbound_foreign_key() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    {
        let db = open_db(temp.path());
        ok(&db, "CREATE TABLE dfp (id INT PRIMARY KEY, k INT UNIQUE)", Family::Text);
        ok(
            &db,
            "CREATE TABLE dfc (id INT PRIMARY KEY, kk INT REFERENCES dfp(k))",
            Family::Text,
        );
        ok(&db, "ALTER TABLE dfp DROP COLUMN k CASCADE", Family::Text);
    }
    let db = open_db(temp.path());
    // The foreign key is gone with the column it pointed at, so an arbitrary
    // value is now legal — and, crucially, the child is still WRITABLE.
    ok(&db, "INSERT INTO dfc (id, kk) VALUES (1, 999)", Family::Text);
    assert_eq!(rows_in(&db, "dfc"), 1, "the child must remain writable");
}

/// Dropping a column that no constraint mentions must stay exactly what it was.
#[test]
fn drop_column_untouched_by_any_constraint_still_works() {
    for family in Family::ALL {
        let db = mem_db();
        ok(
            &db,
            "CREATE TABLE dpl (id INT PRIMARY KEY, name TEXT, salary INT)",
            Family::Text,
        );
        ok(
            &db,
            "INSERT INTO dpl (id, name, salary) VALUES (1, 'a', 10)",
            Family::Text,
        );
        ok(&db, "ALTER TABLE dpl DROP COLUMN salary", family);
        assert_eq!(rows_in(&db, "dpl"), 1, "[{}] the row survives", family.label());
        assert!(
            !column_exists(&db, "dpl", "salary"),
            "[{}] the column is gone",
            family.label()
        );
    }
}

// ===========================================================================
// 6. POSITIVE CONTROLS — these pass on the pre-fix tree AND after the fix.
//    Without them, an "assert a duplicate is refused" suite that had silently
//    stopped exercising anything would look identical to a fixed one.
// ===========================================================================

/// An INLINE `CREATE TABLE … UNIQUE` was always enforced; nothing in either
/// item touches it.
#[test]
fn control_inline_unique_on_create_table_still_rejects_duplicates() {
    let db = mem_db();
    ok(&db, "CREATE TABLE ctl (id INT PRIMARY KEY, v INT UNIQUE)", Family::Text);
    ok(&db, "INSERT INTO ctl (id, v) VALUES (1, 5)", Family::Text);
    err(&db, "INSERT INTO ctl (id, v) VALUES (2, 5)", Family::Text);
    assert_eq!(rows_in(&db, "ctl"), 1);
}

/// A plain column with no constraint accepts duplicates. If this ever fails,
/// the suite above is measuring something other than constraint enforcement.
#[test]
fn control_a_plain_column_accepts_duplicates() {
    let db = mem_db();
    ok(&db, "CREATE TABLE ctl2 (id INT PRIMARY KEY, v INT)", Family::Text);
    ok(&db, "ALTER TABLE ctl2 ADD COLUMN w INT", Family::Text);
    ok(&db, "INSERT INTO ctl2 (id, v, w) VALUES (1, 5, 5)", Family::Text);
    ok(&db, "INSERT INTO ctl2 (id, v, w) VALUES (2, 5, 5)", Family::Text);
    assert_eq!(rows_in(&db, "ctl2"), 2);
}

/// The value really is readable under its new name after a rename + reopen —
/// so the rename tests above are talking about a live table, not a broken one.
#[test]
fn control_a_renamed_columns_data_survives_a_reopen() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    {
        let db = open_db(temp.path());
        ok(&db, "CREATE TABLE ctl3 (id INT PRIMARY KEY, v TEXT)", Family::Text);
        ok(&db, "INSERT INTO ctl3 (id, v) VALUES (1, 'kept')", Family::Text);
        ok(&db, "ALTER TABLE ctl3 RENAME COLUMN v TO w", Family::Text);
    }
    let db = open_db(temp.path());
    let rows = db.query("SELECT w FROM ctl3 WHERE id = 1", &[]).expect("select");
    assert_eq!(rows.len(), 1, "the row must survive the rename and the reopen");
    assert!(
        matches!(rows.first().and_then(|r| r.values.first()), Some(Value::String(s)) if s == "kept"),
        "the renamed column must still hold its value, got {:?}",
        rows.first().map(|r| r.values.clone())
    );
}
