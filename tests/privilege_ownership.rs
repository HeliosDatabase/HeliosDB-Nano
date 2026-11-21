//! Ownership: who created an object owns it, DDL on it requires its owner,
//! and grants on it are the owner's to give.
//!
//! Also here: `CREATE` needs the schema's `CREATE` privilege, a foreign key
//! needs `REFERENCES` on the table it points at, `WITH GRANT OPTION` and
//! `REVOKE … CASCADE`, `ALTER … OWNER TO`, and what happens to owners and
//! grants when a table is dropped, re-created or renamed.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::many_single_char_names)]

use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::{EmbeddedDatabase, Error, Value};

fn login(db: &EmbeddedDatabase, role: &str) -> SessionId {
    db.create_session(role, IsolationLevel::ReadCommitted).unwrap()
}

/// Run on the text family (`false`) or the params family (`true`).
fn run(db: &EmbeddedDatabase, session: SessionId, params: bool, sql: &str) -> Result<u64, Error> {
    if params {
        db.execute_params_for_session(session, sql, &[])
    } else {
        db.execute_for_session(session, sql)
    }
}

fn count(db: &EmbeddedDatabase, session: SessionId, sql: &str) -> Result<usize, Error> {
    db.query_with_columns_for_session(session, sql).map(|(rows, _)| rows.len())
}

fn assert_error<T: std::fmt::Debug>(result: Result<T, Error>, expected: &str, context: &str) {
    match result {
        Ok(value) => panic!("{context}: must be refused with `{expected}`, but returned {value:?}"),
        Err(error) => {
            let text = error.to_string();
            assert!(text.contains(expected), "{context}: expected `{expected}`, got: {text}");
        }
    }
}

/// `maker` may create in `public`; `other` is an ordinary role.
fn setup() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE ROLE maker LOGIN").unwrap();
    db.execute("CREATE ROLE other LOGIN").unwrap();
    db.execute("GRANT CREATE ON SCHEMA public TO maker").unwrap();
    db
}

#[test]
fn creating_a_table_needs_create_on_the_schema() {
    for params in [false, true] {
        let db = setup();
        let other = login(&db, "other");
        assert_error(
            run(&db, other, params, "CREATE TABLE nope (id INT PRIMARY KEY)"),
            "permission denied for schema public",
            "CREATE TABLE without CREATE on the schema",
        );
        assert_error(
            run(&db, other, params, "CREATE TABLE nope2 AS SELECT 1 AS x"),
            "permission denied for schema public",
            "CREATE TABLE AS without CREATE on the schema",
        );
        assert_error(
            run(&db, other, params, "CREATE VIEW nope_v AS SELECT 1 AS x"),
            "permission denied for schema public",
            "CREATE VIEW without CREATE on the schema",
        );
        assert_error(
            run(&db, other, params, "CREATE SEQUENCE nope_seq"),
            "permission denied for schema public",
            "CREATE SEQUENCE without CREATE on the schema",
        );
        assert!(db.query("SELECT * FROM nope", &[]).is_err(), "nothing was created");
        let maker = login(&db, "maker");
        run(&db, maker, params, "CREATE TABLE made (id INT PRIMARY KEY, v TEXT)").unwrap();
    }
}

#[test]
fn the_creator_owns_the_table_and_ddl_requires_the_owner() {
    for params in [false, true] {
        let db = setup();
        let maker = login(&db, "maker");
        let other = login(&db, "other");
        run(&db, maker, params, "CREATE TABLE made (id INT PRIMARY KEY, v TEXT)").unwrap();
        // The owner needs no grant on its own table.
        run(&db, maker, params, "INSERT INTO made VALUES (1, 'a')").unwrap();
        assert_eq!(count(&db, maker, "SELECT * FROM made").unwrap(), 1);

        // Every privilege, and still not the owner.
        run(&db, maker, params, "GRANT ALL ON made TO other").unwrap();
        assert_eq!(count(&db, other, "SELECT * FROM made").unwrap(), 1);
        for ddl in [
            "ALTER TABLE made ADD COLUMN extra INT",
            "ALTER TABLE made DROP COLUMN v",
            "ALTER TABLE made RENAME COLUMN v TO w",
            "ALTER TABLE made RENAME TO made2",
            "CREATE INDEX made_v ON made (v)",
            "DROP TABLE made",
        ] {
            assert_error(
                run(&db, other, params, ddl),
                "must be owner of table made",
                &format!("a non-owner running `{ddl}`"),
            );
        }
        // The owner may.
        run(&db, maker, params, "ALTER TABLE made ADD COLUMN extra INT").unwrap();
        run(&db, maker, params, "CREATE INDEX made_v ON made (v)").unwrap();
        assert_error(
            run(&db, other, params, "DROP INDEX made_v"),
            "must be owner of table made",
            "a non-owner dropping the owner's index",
        );
        run(&db, maker, params, "DROP INDEX made_v").unwrap();
        run(&db, maker, params, "DROP TABLE made").unwrap();
    }
}

#[test]
fn a_table_nobody_recorded_an_owner_for_belongs_to_the_bootstrap_superuser() {
    let db = setup();
    // Created through the embedded API: no role, so no owner is recorded.
    db.execute("CREATE TABLE legacy (id INT PRIMARY KEY)").unwrap();
    db.execute("GRANT ALL ON legacy TO maker").unwrap();
    let maker = login(&db, "maker");
    assert_error(
        run(&db, maker, false, "DROP TABLE legacy"),
        "must be owner of table legacy",
        "a grantee dropping an ownerless table",
    );
    let admin = login(&db, "postgres");
    run(&db, admin, false, "ALTER TABLE legacy ADD COLUMN v INT").unwrap();
    db.execute("CREATE ROLE root_like SUPERUSER LOGIN").unwrap();
    let root_like = login(&db, "root_like");
    run(&db, root_like, false, "DROP TABLE legacy").unwrap();
}

#[test]
fn only_the_owner_or_a_holder_of_the_grant_option_may_grant() {
    let db = setup();
    db.execute("CREATE ROLE third LOGIN").unwrap();
    let maker = login(&db, "maker");
    let other = login(&db, "other");
    let third = login(&db, "third");
    run(&db, maker, false, "CREATE TABLE made (id INT PRIMARY KEY)").unwrap();
    run(&db, maker, false, "GRANT SELECT ON made TO other").unwrap();
    assert_error(
        run(&db, other, false, "GRANT SELECT ON made TO third"),
        "permission denied for table made",
        "granting on without the grant option",
    );
    assert_error(
        run(&db, third, false, "GRANT SELECT ON made TO third"),
        "permission denied for table made",
        "a role with nothing granting itself access",
    );
    assert_error(
        count(&db, third, "SELECT * FROM made"),
        "permission denied for table made",
        "the refused GRANT granted nothing",
    );

    run(&db, maker, false, "GRANT SELECT ON made TO other WITH GRANT OPTION").unwrap();
    run(&db, other, false, "GRANT SELECT ON made TO third").unwrap();
    assert_eq!(count(&db, third, "SELECT * FROM made").unwrap(), 0);
    // The grant option covers the privilege it came with, nothing more.
    assert_error(
        run(&db, other, false, "GRANT INSERT ON made TO third"),
        "permission denied for table made",
        "granting on a privilege one does not hold",
    );
    // The grantor is recorded.
    let (rows, _) = db
        .query_with_columns(
            "SELECT grantor, grantee, is_grantable FROM information_schema.table_privileges \
             WHERE table_name = 'made' ORDER BY grantee",
        )
        .unwrap();
    let seen: Vec<Vec<Value>> = rows.into_iter().map(|row| row.values).collect();
    assert_eq!(
        seen,
        vec![
            vec![
                Value::String("maker".into()),
                Value::String("other".into()),
                Value::String("YES".into())
            ],
            vec![
                Value::String("other".into()),
                Value::String("third".into()),
                Value::String("NO".into())
            ],
        ]
    );
}

#[test]
fn revoke_restrict_refuses_dependent_grants_and_cascade_removes_them() {
    let db = setup();
    db.execute("CREATE ROLE third LOGIN").unwrap();
    let maker = login(&db, "maker");
    let other = login(&db, "other");
    let third = login(&db, "third");
    run(&db, maker, false, "CREATE TABLE made (id INT PRIMARY KEY)").unwrap();
    run(&db, maker, false, "GRANT SELECT ON made TO other WITH GRANT OPTION").unwrap();
    run(&db, other, false, "GRANT SELECT ON made TO third").unwrap();

    assert_error(
        run(&db, maker, false, "REVOKE SELECT ON made FROM other"),
        "dependent privileges exist",
        "REVOKE without CASCADE",
    );
    assert_error(
        run(&db, maker, false, "REVOKE SELECT ON made FROM other RESTRICT"),
        "dependent privileges exist",
        "REVOKE … RESTRICT",
    );
    assert_eq!(count(&db, other, "SELECT * FROM made").unwrap(), 0, "nothing was revoked");
    assert_eq!(count(&db, third, "SELECT * FROM made").unwrap(), 0);

    // A role that is not the grantor of a grant cannot take it away.
    assert_error(
        run(&db, third, false, "REVOKE SELECT ON made FROM other CASCADE"),
        "permission denied for table made",
        "revoking somebody else's grant",
    );

    run(&db, maker, false, "REVOKE SELECT ON made FROM other CASCADE").unwrap();
    for session in [other, third] {
        assert_error(
            count(&db, session, "SELECT * FROM made"),
            "permission denied for table made",
            "after REVOKE … CASCADE",
        );
    }
}

#[test]
fn revoke_grant_option_for_keeps_the_privilege() {
    let db = setup();
    db.execute("CREATE ROLE third LOGIN").unwrap();
    let maker = login(&db, "maker");
    let other = login(&db, "other");
    run(&db, maker, false, "CREATE TABLE made (id INT PRIMARY KEY)").unwrap();
    run(&db, maker, false, "GRANT SELECT ON made TO other WITH GRANT OPTION").unwrap();
    run(&db, maker, false, "REVOKE GRANT OPTION FOR SELECT ON made FROM other").unwrap();
    assert_eq!(count(&db, other, "SELECT * FROM made").unwrap(), 0, "the privilege stays");
    assert_error(
        run(&db, other, false, "GRANT SELECT ON made TO third"),
        "permission denied for table made",
        "the grant option is gone",
    );
}

#[test]
fn granted_by_names_the_current_role_or_is_refused() {
    let db = setup();
    let maker = login(&db, "maker");
    run(&db, maker, false, "CREATE TABLE made (id INT PRIMARY KEY)").unwrap();
    run(&db, maker, false, "GRANT SELECT ON made TO other GRANTED BY maker").unwrap();
    assert_error(
        run(&db, maker, false, "GRANT INSERT ON made TO other GRANTED BY postgres"),
        "permission denied",
        "GRANTED BY another role",
    );
    // A superuser may record another grantor.
    db.execute("GRANT INSERT ON made TO other GRANTED BY maker").unwrap();
}

#[test]
fn alter_owner_moves_the_table_and_who_may_do_it() {
    for params in [false, true] {
        let db = setup();
        db.execute("CREATE ROLE team").unwrap();
        let maker = login(&db, "maker");
        let other = login(&db, "other");
        run(&db, maker, params, "CREATE TABLE made (id INT PRIMARY KEY)").unwrap();
        assert_error(
            run(&db, other, params, "ALTER TABLE made OWNER TO other"),
            "must be owner of table made",
            "a non-owner taking the table",
        );
        assert_error(
            run(&db, maker, params, "ALTER TABLE made OWNER TO team"),
            "must be a member of role \"team\"",
            "giving the table to a role the owner is not in",
        );
        assert_error(
            run(&db, maker, params, "ALTER TABLE made OWNER TO nobody_here"),
            "role \"nobody_here\" does not exist",
            "a new owner that is not a role",
        );
        db.execute("GRANT team TO maker").unwrap();
        run(&db, maker, params, "ALTER TABLE made OWNER TO team").unwrap();
        // `maker` still reaches it, as a member of the owning role.
        run(&db, maker, params, "INSERT INTO made VALUES (1)").unwrap();
        // A superuser hands it to anyone.
        db.execute("ALTER TABLE made OWNER TO other").unwrap();
        assert_error(
            run(&db, maker, params, "INSERT INTO made VALUES (2)"),
            "permission denied for table made",
            "the previous owner after the hand-over",
        );
        run(&db, other, params, "DROP TABLE made").unwrap();
    }
}

#[test]
fn a_recreated_table_does_not_inherit_the_grants_of_the_dropped_one() {
    for params in [false, true] {
        let db = setup();
        let maker = login(&db, "maker");
        let other = login(&db, "other");
        run(&db, maker, params, "CREATE TABLE made (id INT PRIMARY KEY)").unwrap();
        run(&db, maker, params, "GRANT SELECT, INSERT ON made TO other").unwrap();
        run(&db, maker, params, "GRANT SELECT (id) ON made TO PUBLIC").unwrap();
        run(&db, maker, params, "DROP TABLE made").unwrap();
        // Re-created by somebody else entirely.
        db.execute("CREATE TABLE made (id INT PRIMARY KEY, secret TEXT)").unwrap();
        for session in [maker, other] {
            assert_error(
                count(&db, session, "SELECT id FROM made"),
                "permission denied for table made",
                "a grant on the dropped table must not apply to the new one",
            );
        }
        assert_error(
            run(&db, maker, params, "DROP TABLE made"),
            "must be owner of table made",
            "the previous owner does not own the new table",
        );
    }
}

#[test]
fn a_renamed_table_keeps_its_owner_and_its_grants() {
    for params in [false, true] {
        let db = setup();
        let maker = login(&db, "maker");
        let other = login(&db, "other");
        run(&db, maker, params, "CREATE TABLE made (id INT PRIMARY KEY, v TEXT)").unwrap();
        run(&db, maker, params, "GRANT SELECT ON made TO other").unwrap();
        run(&db, maker, params, "ALTER TABLE made RENAME TO renamed").unwrap();
        assert_eq!(count(&db, other, "SELECT * FROM renamed").unwrap(), 0);
        assert_error(
            run(&db, other, params, "INSERT INTO renamed VALUES (1, 'x')"),
            "permission denied for table renamed",
            "the rename carried SELECT only",
        );
        run(&db, maker, params, "ALTER TABLE renamed ADD COLUMN w INT").unwrap();
        run(&db, maker, params, "DROP TABLE renamed").unwrap();
    }
}

#[test]
fn truncate_needs_the_truncate_privilege_or_ownership() {
    let db = setup();
    let maker = login(&db, "maker");
    let other = login(&db, "other");
    run(&db, maker, false, "CREATE TABLE made (id INT PRIMARY KEY)").unwrap();
    run(&db, maker, false, "GRANT SELECT, DELETE ON made TO other").unwrap();
    for params in [false, true] {
        assert_error(
            run(&db, other, params, "TRUNCATE made"),
            "permission denied for table made",
            "DELETE is not TRUNCATE",
        );
        run(&db, maker, params, "TRUNCATE made").unwrap();
    }
    run(&db, maker, false, "GRANT TRUNCATE ON made TO other").unwrap();
    run(&db, other, false, "TRUNCATE TABLE made").unwrap();
}

#[test]
fn a_foreign_key_needs_references_on_the_parent() {
    for params in [false, true] {
        let db = setup();
        db.execute("CREATE TABLE parent (id INT PRIMARY KEY, code TEXT UNIQUE)").unwrap();
        let maker = login(&db, "maker");
        assert_error(
            run(
                &db,
                maker,
                params,
                "CREATE TABLE child (id INT PRIMARY KEY, p INT REFERENCES parent(id))",
            ),
            "permission denied for table parent",
            "CREATE TABLE with a foreign key, without REFERENCES",
        );
        assert!(db.query("SELECT * FROM child", &[]).is_err(), "the child was not created");

        // SELECT is not REFERENCES.
        db.execute("GRANT SELECT ON parent TO maker").unwrap();
        assert_error(
            run(
                &db,
                maker,
                params,
                "CREATE TABLE child (id INT PRIMARY KEY, p INT REFERENCES parent(id))",
            ),
            "permission denied for table parent",
            "SELECT does not allow a foreign key",
        );

        // A column grant covers a key on that column only.
        db.execute("GRANT REFERENCES (id) ON parent TO maker").unwrap();
        run(
            &db,
            maker,
            params,
            "CREATE TABLE child (id INT PRIMARY KEY, p INT REFERENCES parent(id))",
        )
        .unwrap();
        run(&db, maker, params, "CREATE TABLE child2 (id INT PRIMARY KEY, c TEXT)").unwrap();
        assert_error(
            run(
                &db,
                maker,
                params,
                "ALTER TABLE child2 ADD CONSTRAINT child2_fk FOREIGN KEY (c) REFERENCES parent(code)",
            ),
            "permission denied for table parent",
            "ALTER TABLE ADD FOREIGN KEY on a column without REFERENCES",
        );
        db.execute("GRANT REFERENCES ON parent TO maker").unwrap();
        run(
            &db,
            maker,
            params,
            "ALTER TABLE child2 ADD CONSTRAINT child2_fk FOREIGN KEY (c) REFERENCES parent(code)",
        )
        .unwrap();
    }
}

#[test]
fn views_have_owners_too() {
    let db = setup();
    db.execute("CREATE TABLE base (id INT PRIMARY KEY)").unwrap();
    db.execute("GRANT SELECT ON base TO maker").unwrap();
    let maker = login(&db, "maker");
    let other = login(&db, "other");
    run(&db, maker, false, "CREATE VIEW base_v AS SELECT id FROM base").unwrap();
    assert_eq!(count(&db, maker, "SELECT * FROM base_v").unwrap(), 0);
    assert_error(
        run(&db, other, false, "DROP VIEW base_v"),
        "must be owner of view base_v",
        "a non-owner dropping a view",
    );
    assert_error(
        run(&db, other, false, "CREATE OR REPLACE VIEW base_v AS SELECT 1 AS id"),
        "must be owner of view base_v",
        "a non-owner replacing a view",
    );
    run(&db, maker, false, "DROP VIEW base_v").unwrap();
}

#[test]
fn the_owner_is_reported_in_the_refusal_not_the_schema_qualified_key() {
    let db = setup();
    db.execute("CREATE SCHEMA app").unwrap();
    db.execute("CREATE TABLE app.things (id INT PRIMARY KEY)").unwrap();
    db.execute("GRANT USAGE ON SCHEMA app TO other").unwrap();
    let other = login(&db, "other");
    assert_error(
        count(&db, other, "SELECT * FROM app.things"),
        "permission denied for table things",
        "PostgreSQL names the relation without its schema",
    );
}

#[test]
fn ddl_through_the_embedded_api_stays_unrestricted() {
    let db = setup();
    let maker = login(&db, "maker");
    run(&db, maker, false, "CREATE TABLE made (id INT PRIMARY KEY)").unwrap();
    // The application that owns the process is not a role.
    db.execute("ALTER TABLE made ADD COLUMN v INT").unwrap();
    db.execute("DROP TABLE made").unwrap();
}
