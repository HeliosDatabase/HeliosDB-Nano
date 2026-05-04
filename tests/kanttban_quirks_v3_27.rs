//! Acceptance tests for the v3.27.0 quirks reported by the KanttBan
//! Drizzle ORM migration in `/home/app/Personal/KanttBan/Kanttban/BUGS_HELIOSDB.md`.
//!
//! Coverage:
//! - Bug #3  pg_sequences view exists (drizzle-kit pull blocker)
//! - Bug #4  GENERATED ALWAYS AS IDENTITY (sequence name … INCREMENT BY …)
//! - Bug #5  ALTER TABLE ADD CONSTRAINT FOREIGN KEY
//! - Bug #6  FK enforced on INSERT and UPDATE (not just DELETE)
//! - Bug #9  current_setting() scalar function
//!
//! Bugs #1, #2 (daemon / http-port) are tested via the heliosdb-nano
//! binary in `tests/server_mode_integration_test.rs` and the field
//! script `~/scripts/heliosdb-nano-smoke.sh`. Bugs #10, #11 (branding,
//! --version) are documented in the changelog and verified manually.

use heliosdb_nano::{EmbeddedDatabase, Value};

// ---------- Bug #4: IDENTITY (sequence options) -----------------------------

#[test]
fn identity_with_parenthesized_sequence_options_parses() {
    // Exact DDL drizzle-kit emits.
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute(
        r#"CREATE TABLE "tasks" (
            "id" integer PRIMARY KEY GENERATED ALWAYS AS IDENTITY (
                sequence name "tasks_id_seq" INCREMENT BY 1 MINVALUE 1
                MAXVALUE 2147483647 START WITH 1 CACHE 1
            ),
            "title" text NOT NULL
        )"#,
    ).expect("create with IDENTITY (options) must parse");

    // Auto-increment must still work.
    db.execute("INSERT INTO tasks (title) VALUES ('first')").expect("insert 1");
    db.execute("INSERT INTO tasks (title) VALUES ('second')").expect("insert 2");
    let rows = db.query("SELECT id, title FROM tasks ORDER BY id", &[]).expect("select");
    assert_eq!(rows.len(), 2);
}

#[test]
fn identity_bare_form_still_works() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute(r#"CREATE TABLE t (id integer PRIMARY KEY GENERATED ALWAYS AS IDENTITY, n text)"#)
        .expect("create");
    db.execute("INSERT INTO t (n) VALUES ('a')").expect("insert");
    let rows = db.query("SELECT id FROM t", &[]).expect("select");
    assert_eq!(rows.len(), 1);
}

// ---------- Bug #5: ALTER TABLE ADD CONSTRAINT FOREIGN KEY ------------------

#[test]
fn alter_table_add_constraint_foreign_key() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE parents (id integer PRIMARY KEY, name text)").expect("create parents");
    db.execute("CREATE TABLE children (id integer PRIMARY KEY, parent_id integer)")
        .expect("create children");

    // Drizzle / Prisma / Flyway emit FKs as a separate ALTER step.
    db.execute(
        r#"ALTER TABLE children ADD CONSTRAINT fk_children_parent
           FOREIGN KEY (parent_id) REFERENCES parents(id)"#,
    ).expect("ADD CONSTRAINT FOREIGN KEY must succeed");

    // Wire it up: insert a parent, an orphan attempt should now fail.
    db.execute("INSERT INTO parents (id, name) VALUES (1, 'root')").expect("insert parent");
    db.execute("INSERT INTO children (id, parent_id) VALUES (1, 1)").expect("valid child");
    let orphan = db.execute("INSERT INTO children (id, parent_id) VALUES (2, 999)");
    assert!(orphan.is_err(), "FK added by ALTER must reject orphan; got {orphan:?}");
}

// ---------- Bug #6: FK enforced on INSERT and UPDATE ------------------------

#[test]
fn fk_enforced_on_insert() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE p (id integer PRIMARY KEY, name text)").expect("create p");
    db.execute(
        "CREATE TABLE c (id integer PRIMARY KEY, parent_id integer REFERENCES p(id), name text)",
    ).expect("create c with inline FK");
    db.execute("INSERT INTO p (id, name) VALUES (1, 'p1')").expect("insert parent");

    // Valid: parent_id=1 exists.
    db.execute("INSERT INTO c (id, parent_id, name) VALUES (10, 1, 'ok')")
        .expect("valid INSERT");

    // Invalid: parent_id=999 does not exist — must be rejected (Bug #6).
    let orphan = db.execute("INSERT INTO c (id, parent_id, name) VALUES (11, 999, 'broken')");
    assert!(orphan.is_err(), "Bug #6: FK must be enforced on INSERT; got {orphan:?}");
}

#[test]
fn fk_enforced_on_update() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE p (id integer PRIMARY KEY, name text)").expect("create p");
    db.execute(
        "CREATE TABLE c (id integer PRIMARY KEY, parent_id integer REFERENCES p(id), name text)",
    ).expect("create c");
    db.execute("INSERT INTO p (id, name) VALUES (1, 'p1')").expect("insert parent");
    db.execute("INSERT INTO p (id, name) VALUES (2, 'p2')").expect("insert parent2");
    db.execute("INSERT INTO c (id, parent_id, name) VALUES (10, 1, 'a')").expect("ok child");

    // Valid: change to existing parent.
    db.execute("UPDATE c SET parent_id = 2 WHERE id = 10").expect("valid UPDATE");

    // Invalid: change to non-existent parent.
    let bad = db.execute("UPDATE c SET parent_id = 999 WHERE id = 10");
    assert!(bad.is_err(), "Bug #6: FK must be enforced on UPDATE; got {bad:?}");
}

#[test]
fn fk_null_short_circuits() {
    // PG MATCH SIMPLE: any NULL FK column → trivially valid.
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE p (id integer PRIMARY KEY)").expect("create p");
    db.execute("CREATE TABLE c (id integer PRIMARY KEY, parent_id integer REFERENCES p(id))")
        .expect("create c");
    // No parents, but NULL FK → accepted.
    db.execute("INSERT INTO c (id, parent_id) VALUES (1, NULL)")
        .expect("NULL FK must be accepted (MATCH SIMPLE)");
}

// ---------- Bug #9: current_setting() ---------------------------------------

#[test]
fn current_setting_known_param() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    let rows = db.query("SELECT current_setting('server_version')", &[]).expect("query");
    assert_eq!(rows.len(), 1);
    let v = match rows[0].values.first() {
        Some(Value::String(s)) => s.clone(),
        other => panic!("expected text, got {other:?}"),
    };
    assert!(!v.is_empty(), "server_version should not be empty");
}

#[test]
fn current_setting_unknown_with_missing_ok_returns_empty() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    // Two-arg form: missing_ok=true → empty string for unknown setting.
    let rows = db.query("SELECT current_setting('does_not_exist', true)", &[])
        .expect("query");
    let v = match rows[0].values.first() {
        Some(Value::String(s)) => s.clone(),
        other => panic!("expected text, got {other:?}"),
    };
    assert_eq!(v, "");
}

#[test]
fn current_setting_unknown_without_missing_ok_errors() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    let r = db.query("SELECT current_setting('also_does_not_exist')", &[]);
    assert!(r.is_err(), "unknown setting without missing_ok must error; got {r:?}");
}
