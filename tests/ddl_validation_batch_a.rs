//! DDL / constraint validation batch A — four DDL-time refusals this engine
//! did not make, and the ART error label that misreported the fifth.
//!
//! Every item here is one defect class: a declaration the engine ACCEPTED and
//! then could not enforce, reported to the client as success.
//!
//!   1. sprinter `ad6a982e16d5` — `FOREIGN KEY (nosuch) REFERENCES p(id)`. The
//!      REFERENCED side has been validated since GH#27; the REFERENCING (child)
//!      side never was. The constraint was persisted naming a column the child
//!      does not have, and the write path — which resolves the referencing
//!      column by name to build the parent probe — then had nothing to probe
//!      with, so the foreign key enforced nothing at all.
//!   2. sprinter `cd879f076084` — `CREATE TABLE t (a INT, PRIMARY KEY (nosuch))`
//!      created `t` with NO primary key and no error: the lowering's `.find()`
//!      over the declared columns simply missed. The `UNIQUE` record persisted
//!      for the constraint named a column that does not exist, and index
//!      registration skips primary keys, so nothing enforced it either.
//!   3. sprinter `7a0a2ba2c8c3` — `ALTER TABLE t ADD CONSTRAINT fk1 …` twice
//!      appended a SECOND constraint under the same name. PostgreSQL keeps one
//!      constraint namespace per relation and reports 42710; here the name then
//!      resolved to two records, so `DROP CONSTRAINT fk1` retired both and
//!      `information_schema.table_constraints` listed the name twice.
//!   4. sprinter `332da6771914` — `UPDATE t SET serial_col = DEFAULT`
//!      substituted NULL. SERIAL / IDENTITY columns carry no stored
//!      `default_expr` (INSERT fills them at storage time), so the "no declared
//!      default ⇒ NULL" rule was applied to them: fail-closed on a SERIAL
//!      PRIMARY KEY (the NULL-PK guard rejected it, with the wrong diagnostic)
//!      and SILENT DATA LOSS on any other serial column.
//!   5. sprinter `fa2d11f140fe` — `ArtIndexManager::stored_duplicate_error`
//!      relabelled EVERY enforcing-index refusal as a stored duplicate. That
//!      one needs `art_manager.rs` internals an integration test cannot reach,
//!      so it lives as an in-crate unit test instead:
//!      `src/storage/art_manager.rs`, `tests::stored_duplicate_error_relabels_only_a_duplicate_key`.
//!
//! # SQLSTATE
//!
//! The wire classifier is not reachable from an integration test
//! (`sqlstate_for_error` is `pub(crate)`), so — as in `tests/gh_issue_27*.rs`
//! and `tests/gh_issue_21_constraint_namespace.rs` — these tests pin the
//! MESSAGE SHAPE the classifier keys on, which is what makes the SQLSTATE:
//!
//! * `column "c" … does not exist`                        → 42703 undefined_column
//! * `constraint "c" for relation "t" already exists`      → 42710 duplicate_object
//! * `SET c = DEFAULT on a serial column is not supported` → 0A000 feature_not_supported
//!
//! The last two are PostgreSQL's own wording but have no arm in
//! `sqlstate_for_query_execution_message` yet — see the notes on
//! `assert_duplicate_constraint` and `assert_feature_not_supported`.
//!
//! # Both executor families
//!
//! `db.execute()` is the TEXT family (`execute_in_transaction_inner`: psql
//! simple query, MySQL wire, embedded) and `db.execute_params()` is the PARAMS
//! family (`execute_plan_with_params_inner`: the PostgreSQL EXTENDED protocol
//! every real driver uses, plus REST/BaaS). `CREATE TABLE` and
//! `ALTER TABLE … ADD FOREIGN KEY` route to the same bodies on both since
//! sprinter `15bfe577751a`, and items 2 and 4 are refused at PLAN time, which
//! precedes both — so every test here runs its whole scenario twice.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{EmbeddedDatabase, Value};

/// One executor family bound to one database, so every statement in a test
/// body is a one-argument call.
struct Family<'a> {
    db: &'a EmbeddedDatabase,
    params: bool,
}

impl Family<'_> {
    fn label(&self) -> &'static str {
        if self.params {
            "params"
        } else {
            "text"
        }
    }

    fn run(&self, sql: &str) -> heliosdb_nano::Result<u64> {
        if self.params {
            self.db.execute_params(sql, &[])
        } else {
            self.db.execute(sql)
        }
    }

    /// Run a statement that must succeed.
    fn ok(&self, sql: &str) {
        if let Err(e) = self.run(sql) {
            panic!("[{}] `{sql}` must succeed: {e}", self.label());
        }
    }

    /// Run a statement that must be REFUSED, and hand back the message.
    fn reject(&self, sql: &str) -> String {
        match self.run(sql) {
            Ok(_) => panic!(
                "[{}] *** UNENFORCEABLE DECLARATION ACCEPTED *** `{sql}` must be rejected at DDL time",
                self.label()
            ),
            Err(e) => e.to_string(),
        }
    }

    fn refuses(&self, sql: &str) -> bool {
        self.run(sql).is_err()
    }

    fn table_exists(&self, table: &str) -> bool {
        let sql = format!("SELECT * FROM {table}");
        self.db.query_with_columns(&sql).is_ok()
    }

    fn one_row(&self, sql: &str) -> Vec<Value> {
        let rows = match self.db.query_with_columns(sql) {
            Ok((rows, _)) => rows,
            Err(e) => panic!("[{}] `{sql}`: {e}", self.label()),
        };
        assert_eq!(rows.len(), 1, "`{sql}` must return exactly one row");
        rows[0].values.clone()
    }
}

/// PostgreSQL's 42703 wording. The `column "` shape plus a not-found token is
/// what `sqlstate_for_query_execution_message`'s column arms anchor on.
fn assert_undefined_column(err: &str, col: &str) {
    let lower = err.to_ascii_lowercase();
    let quoted = format!("column \"{}\"", col.to_ascii_lowercase());
    assert!(
        lower.contains(&quoted) && lower.contains("does not exist"),
        "expected PostgreSQL's 42703 wording (`column \"{col}\" … does not exist`), got: {err}"
    );
}

/// PostgreSQL's 42710 wording, `constraint "c" for relation "t" already
/// exists`.
///
/// RECORDED GAP: the PG wire classifier has no constraint arm, so this message
/// currently falls through to the generic `(table|relation) && already exists`
/// arm and reports 42P07 duplicate_TABLE rather than 42710 duplicate_object.
/// Wrong code, right class, and an enormous improvement on the silent
/// acceptance it replaces — but the arm belongs in
/// `src/protocol/postgres/handler.rs`, which this change does not own. The
/// wording is PostgreSQL's exactly, so adding the arm is a one-liner keyed on
/// this text.
fn assert_duplicate_constraint(err: &str, name: &str, table: &str) {
    let lower = err.to_ascii_lowercase();
    let constraint = format!("constraint \"{}\"", name.to_ascii_lowercase());
    let relation = format!("relation \"{}\"", table.to_ascii_lowercase());
    assert!(
        lower.contains(&constraint) && lower.contains(&relation) && lower.contains("already exists"),
        "expected `constraint \"{name}\" for relation \"{table}\" already exists` (42710), got: {err}"
    );
}

/// The refusal of `SET <serial col> = DEFAULT`.
///
/// RECORDED GAP: the same shape of gap as `assert_duplicate_constraint`. The
/// 0A000 arms of the classifier are all anchored on emitter-owned marker
/// consts and there is none for this message yet, so it currently reports
/// XX000 on the wire. The refusal itself is the fix — the statement no longer
/// writes NULL — and the arm belongs to `handler.rs`.
fn assert_feature_not_supported(err: &str, col: &str) {
    let lower = err.to_ascii_lowercase();
    assert!(
        lower.contains("not supported") && lower.contains(&col.to_ascii_lowercase()),
        "expected the 0A000 refusal naming column `{col}`, got: {err}"
    );
}

// ===========================================================================
// 1. sprinter ad6a982e16d5 — the REFERENCING (child) columns of a FOREIGN KEY
// ===========================================================================

/// A foreign key may only reference columns the CHILD table actually has —
/// through `CREATE TABLE` and through `ALTER TABLE … ADD FOREIGN KEY`, on both
/// executor families.
///
/// FAILS on the pre-fix tree: every refused statement below returned `Ok` and
/// persisted a constraint that enforced nothing.
#[test]
fn fk_child_column_must_exist() {
    for params in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
        let f = Family { db: &db, params };
        let fam = f.label();
        f.ok("CREATE TABLE p (id INT PRIMARY KEY)");
        f.ok("CREATE TABLE p2 (a INT, b INT, PRIMARY KEY (a, b))");

        // CREATE TABLE, table-level spelling.
        let err = f.reject("CREATE TABLE c (id INT, FOREIGN KEY (nosuch) REFERENCES p(id))");
        assert_undefined_column(&err, "nosuch");
        assert!(
            !f.table_exists("c"),
            "[{fam}] the rejected CREATE TABLE left table c behind"
        );

        // Composite, and wrong in the SECOND position only: the arity and
        // parent-side checks all pass, so only the child-side check can
        // catch this one.
        let err = f.reject("CREATE TABLE c2 (a INT, CONSTRAINT k FOREIGN KEY (a, nosuch) REFERENCES p2(a, b))");
        assert_undefined_column(&err, "nosuch");
        assert!(
            !f.table_exists("c2"),
            "[{fam}] the rejected composite CREATE TABLE left table c2 behind"
        );

        // A SELF-reference is resolved against the columns being DECLARED,
        // and its referencing side is checked the same way.
        let err = f.reject("CREATE TABLE sc (id INT PRIMARY KEY, FOREIGN KEY (nosuch) REFERENCES sc(id))");
        assert_undefined_column(&err, "nosuch");
        assert!(
            !f.table_exists("sc"),
            "[{fam}] the rejected self-reference left table sc behind"
        );

        // Control: the same statement with a real child column is accepted
        // AND enforced.
        f.ok("CREATE TABLE c3 (id INT PRIMARY KEY, pid INT, FOREIGN KEY (pid) REFERENCES p(id))");
        f.ok("INSERT INTO p VALUES (1)");
        f.ok("INSERT INTO c3 VALUES (1, 1)");
        assert!(
            f.refuses("INSERT INTO c3 VALUES (2, 99)"),
            "[{fam}] the legal CREATE TABLE foreign key must still be enforced"
        );

        // ALTER TABLE … ADD FOREIGN KEY on an existing table.
        f.ok("CREATE TABLE ac (id INT PRIMARY KEY, pid INT)");
        let err = f.reject("ALTER TABLE ac ADD FOREIGN KEY (nosuch) REFERENCES p(id)");
        assert_undefined_column(&err, "nosuch");
        let err = f.reject("ALTER TABLE ac ADD CONSTRAINT ac_fk FOREIGN KEY (nosuch) REFERENCES p(id)");
        assert_undefined_column(&err, "nosuch");

        // Nothing was recorded by the refusals: the legal ADD still installs
        // and enforces the constraint.
        f.ok("ALTER TABLE ac ADD FOREIGN KEY (pid) REFERENCES p(id)");
        f.ok("INSERT INTO ac VALUES (1, 1)");
        assert!(
            f.refuses("INSERT INTO ac VALUES (2, 99)"),
            "[{fam}] the legal ALTER-added foreign key must be enforced"
        );
    }
}

// ===========================================================================
// 2. sprinter cd879f076084 — table-level PRIMARY KEY (nosuch)
// ===========================================================================

/// A table-level `PRIMARY KEY (…)` must name declared columns; a miss is a
/// planning error, not a table with no primary key.
///
/// FAILS on the pre-fix tree: every refused statement below returned `Ok` and
/// left a key-less table behind.
#[test]
fn table_level_primary_key_must_name_a_real_column() {
    for params in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
        let f = Family { db: &db, params };
        let fam = f.label();

        let err = f.reject("CREATE TABLE t (a INT, PRIMARY KEY (nosuch))");
        assert_undefined_column(&err, "nosuch");
        assert!(
            !f.table_exists("t"),
            "[{fam}] the rejected CREATE TABLE left table t behind"
        );

        // Composite, wrong in the SECOND position only.
        let err = f.reject("CREATE TABLE t2 (a INT, b INT, PRIMARY KEY (a, nosuch))");
        assert_undefined_column(&err, "nosuch");
        assert!(
            !f.table_exists("t2"),
            "[{fam}] the rejected composite CREATE TABLE left table t2 behind"
        );

        // A NAMED table-level constraint is the same rule.
        let err = f.reject("CREATE TABLE t3 (a INT, CONSTRAINT t3_pk PRIMARY KEY (nosuch))");
        assert_undefined_column(&err, "nosuch");
        assert!(
            !f.table_exists("t3"),
            "[{fam}] the rejected named CREATE TABLE left table t3 behind"
        );

        // Control: a correct composite key still succeeds — and is a REAL
        // primary key, not a silently dropped declaration.
        f.ok("CREATE TABLE ok2 (a INT, b INT, PRIMARY KEY (a, b))");
        f.ok("INSERT INTO ok2 VALUES (1, 1)");
        f.ok("INSERT INTO ok2 VALUES (1, 2)");
        assert!(
            f.refuses("INSERT INTO ok2 VALUES (1, 1)"),
            "[{fam}] *** UNENFORCED PRIMARY KEY *** the composite key took a duplicate"
        );

        // Control: the single-column table-level spelling WordPress emits.
        f.ok("CREATE TABLE ok1 (a INT, b INT, PRIMARY KEY (a))");
        f.ok("INSERT INTO ok1 VALUES (1, 1)");
        assert!(
            f.refuses("INSERT INTO ok1 VALUES (1, 2)"),
            "[{fam}] *** UNENFORCED PRIMARY KEY *** the single-column key took a duplicate"
        );
    }
}

// ===========================================================================
// 3. sprinter 7a0a2ba2c8c3 — a duplicate constraint NAME
// ===========================================================================

/// One constraint namespace per relation: an explicit `ADD CONSTRAINT <name>`
/// that collides with an FK, CHECK or UNIQUE name the table already has is
/// refused.
///
/// FAILS on the pre-fix tree: the second `ADD CONSTRAINT fk1` returned `Ok`
/// and the table carried the name twice.
#[test]
fn duplicate_foreign_key_constraint_name_is_rejected() {
    for params in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
        let f = Family { db: &db, params };
        let fam = f.label();
        f.ok("CREATE TABLE dp (id INT PRIMARY KEY)");
        f.ok("CREATE TABLE dc (id INT PRIMARY KEY, a INT, b INT)");
        f.ok("ALTER TABLE dc ADD CONSTRAINT fk1 FOREIGN KEY (a) REFERENCES dp(id)");

        let err = f.reject("ALTER TABLE dc ADD CONSTRAINT fk1 FOREIGN KEY (b) REFERENCES dp(id)");
        assert_duplicate_constraint(&err, "fk1", "dc");

        // Case-insensitively, like `DROP CONSTRAINT`'s own lookup: a name
        // DROP would match is a name this table already has.
        let err = f.reject("ALTER TABLE dc ADD CONSTRAINT FK1 FOREIGN KEY (b) REFERENCES dp(id)");
        assert_duplicate_constraint(&err, "fk1", "dc");

        // Control: a differently-named second FK still succeeds.
        f.ok("ALTER TABLE dc ADD CONSTRAINT fk2 FOREIGN KEY (b) REFERENCES dp(id)");

        // `fk1` was recorded ONCE, so dropping it retires exactly one rule:
        // `a` becomes free and `b` stays constrained.
        f.ok("ALTER TABLE dc DROP CONSTRAINT fk1");
        f.ok("INSERT INTO dp VALUES (1)");
        f.ok("INSERT INTO dc VALUES (1, 7, 1)");
        assert!(
            f.refuses("INSERT INTO dc VALUES (2, 1, 99)"),
            "[{fam}] *** fk2 WAS RETIRED WITH fk1 *** the two names collided"
        );

        // The namespace spans constraint KINDS, not just foreign keys.
        f.ok("CREATE TABLE dk (id INT PRIMARY KEY, a INT, b INT)");
        f.ok("ALTER TABLE dk ADD CONSTRAINT dup UNIQUE (a)");
        let err = f.reject("ALTER TABLE dk ADD CONSTRAINT dup FOREIGN KEY (b) REFERENCES dp(id)");
        assert_duplicate_constraint(&err, "dup", "dk");

        // An OMITTED name is still minted collision-free — that path is
        // untouched, and two unnamed FKs on one table must both install.
        f.ok("CREATE TABLE da (id INT PRIMARY KEY, a INT, b INT)");
        f.ok("ALTER TABLE da ADD FOREIGN KEY (a) REFERENCES dp(id)");
        f.ok("ALTER TABLE da ADD FOREIGN KEY (b) REFERENCES dp(id)");
        f.ok("INSERT INTO da VALUES (1, 1, 1)");
        assert!(
            f.refuses("INSERT INTO da VALUES (2, 1, 99)"),
            "[{fam}] the second auto-named foreign key was never installed"
        );
    }
}

// ===========================================================================
// 4. sprinter 332da6771914 — UPDATE … SET <serial col> = DEFAULT
// ===========================================================================

/// `SET <serial column> = DEFAULT` is refused, and the column keeps its value.
///
/// FAILS on the pre-fix tree: the non-key serial column was silently NULLed
/// (`Ok(1)`), and the key column's refusal came back as a primary-key / NOT
/// NULL complaint rather than as "this is not supported".
#[test]
fn update_set_serial_default_is_refused_not_null() {
    for params in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
        let f = Family { db: &db, params };
        let fam = f.label();

        // A SERIAL PRIMARY KEY: fail-closed before, but with the wrong
        // diagnostic, and only because the NULL-PK guard sat downstream.
        f.ok("CREATE TABLE sd (id SERIAL PRIMARY KEY, v INT)");
        f.ok("INSERT INTO sd (v) VALUES (5)");
        let before = f.one_row("SELECT id, v FROM sd");
        assert_ne!(before[0], Value::Null, "[{fam}] the SERIAL key was not filled");

        let err = f.reject("UPDATE sd SET id = DEFAULT WHERE v = 5");
        assert_feature_not_supported(&err, "id");
        let after = f.one_row("SELECT id, v FROM sd");
        assert_eq!(after[0], before[0], "[{fam}] the refused UPDATE changed id");

        // A NON-key serial column: the silent-data-loss half.
        f.ok("CREATE TABLE sn (id INT, n SERIAL, w INT DEFAULT 7, z INT)");
        f.ok("INSERT INTO sn (id, n, w, z) VALUES (1, 42, 1, 1)");
        let err = f.reject("UPDATE sn SET n = DEFAULT WHERE id = 1");
        assert_feature_not_supported(&err, "n");
        let row = f.one_row("SELECT n FROM sn WHERE id = 1");
        assert_ne!(row[0], Value::Null, "[{fam}] *** SILENT NULL *** n was NULLed");
        assert_eq!(row[0], Value::Int4(42), "[{fam}] the refused UPDATE changed n");

        // The refusal is per-COLUMN: a statement that names no serial column
        // keeps PostgreSQL's behaviour exactly — the declared default, or
        // NULL where there is none.
        f.ok("UPDATE sn SET w = DEFAULT, z = DEFAULT WHERE id = 1");
        let row = f.one_row("SELECT w, z FROM sn WHERE id = 1");
        assert_eq!(row[0], Value::Int4(7), "[{fam}] the declared default");
        assert_eq!(row[1], Value::Null, "[{fam}] no declared default: NULL");

        // And a serial column is still assignable to an explicit value.
        f.ok("UPDATE sn SET n = 99 WHERE id = 1");
        let row = f.one_row("SELECT n FROM sn WHERE id = 1");
        assert_eq!(row[0], Value::Int4(99), "[{fam}] an explicit SET must still work");
    }
}
