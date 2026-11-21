//! `ALTER TABLE … ADD CONSTRAINT … UNIQUE / CHECK` over rows the same
//! transaction inserted (a migration that seeds a table and then constrains it).
//!
//! Before: refused ("… after writing it in the same transaction is not
//! supported", 0A000), because the constraint was validated and indexed over the
//! committed rows alone. Now the inserted rows are validated and indexed with
//! them. A staged UPDATE or DELETE, a savepoint, and the PRIMARY KEY / FOREIGN
//! KEY / `ADD COLUMN … UNIQUE` forms are refused as before.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

mod ddl_forms_support;
use ddl_forms_support::*;
use heliosdb_nano::EmbeddedDatabase;

const STAGED_ROWS_REFUSAL: &str = "after writing it in the same transaction is not supported";

fn seeded() -> EmbeddedDatabase {
    setup(&[
        "CREATE TABLE par (id INT PRIMARY KEY)",
        "CREATE TABLE s (id INT, v INT)",
        "INSERT INTO s VALUES (1, 1)",
    ])
}

fn is_duplicate(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("duplicate") || lower.contains("unique")
}

#[test]
fn a_unique_constraint_over_inserted_rows_is_validated_and_indexed() {
    for family in FAMILIES {
        let db = seeded();
        ok(&db, family, "BEGIN");
        ok(&db, family, "INSERT INTO s VALUES (2, 2), (3, 3)");
        ok(&db, family, "ALTER TABLE s ADD CONSTRAINT s_v_key UNIQUE (v)");
        // Enforced at once, over the inserted rows too.
        let message = refused(&db, family, "INSERT INTO s VALUES (4, 3)");
        assert!(is_duplicate(&message), "{family:?}: {message}");
        ok(&db, family, "ROLLBACK");

        // A fresh run that commits.
        let db = seeded();
        ok(&db, family, "BEGIN");
        ok(&db, family, "INSERT INTO s VALUES (2, 2), (3, 3)");
        ok(&db, family, "ALTER TABLE s ADD CONSTRAINT s_v_key UNIQUE (v)");
        ok(&db, family, "INSERT INTO s VALUES (4, 4)");
        ok(&db, family, "COMMIT");
        for v in 1..=4 {
            assert_eq!(
                text_rows(&db, &format!("SELECT id FROM s WHERE v = {v}")),
                vec![vec![v.to_string()]],
                "{family:?}: a lookup by the key finds row {v}"
            );
        }
        for v in 1..=4 {
            let message = refused(&db, family, &format!("INSERT INTO s VALUES (9, {v})"));
            assert!(is_duplicate(&message), "{family:?}: {message}");
        }
        ok(&db, family, "INSERT INTO s VALUES (5, 5)");
    }
}

#[test]
fn a_duplicate_among_the_inserted_rows_or_against_a_committed_row_is_23505_and_nothing_is_written() {
    for family in FAMILIES {
        for staged in ["INSERT INTO s VALUES (2, 2), (3, 2)", "INSERT INTO s VALUES (2, 1)"] {
            let db = seeded();
            ok(&db, family, "BEGIN");
            ok(&db, family, staged);
            let message = refused(&db, family, "ALTER TABLE s ADD CONSTRAINT s_v_key UNIQUE (v)");
            assert!(is_duplicate(&message), "{family:?} {staged}: {message}");
            assert!(!message.contains(STAGED_ROWS_REFUSAL), "{family:?} {staged}: {message}");
            let _ = run(&db, family, "ROLLBACK");
            // No constraint was left behind: a duplicate is still accepted.
            ok(&db, family, "INSERT INTO s VALUES (7, 1)");
            assert_eq!(text_rows(&db, "SELECT count(*) FROM s WHERE v = 1"), vec![vec!["2"]]);
        }
    }
}

#[test]
fn after_a_rollback_the_constraint_holds_over_the_committed_rows() {
    for family in FAMILIES {
        let db = seeded();
        ok(&db, family, "BEGIN");
        ok(&db, family, "INSERT INTO s VALUES (2, 2), (3, 3)");
        ok(&db, family, "ALTER TABLE s ADD CONSTRAINT s_v_key UNIQUE (v)");
        ok(&db, family, "ROLLBACK");
        // DDL is not transactional: the constraint stays. The rolled-back rows'
        // keys are free again; the committed row's is taken.
        assert_eq!(text_rows(&db, "SELECT id, v FROM s ORDER BY id"), vec![vec!["1", "1"]]);
        ok(&db, family, "INSERT INTO s VALUES (2, 2)");
        ok(&db, family, "INSERT INTO s VALUES (3, 3)");
        let message = refused(&db, family, "INSERT INTO s VALUES (4, 1)");
        assert!(is_duplicate(&message), "{family:?}: {message}");
        let message = refused(&db, family, "INSERT INTO s VALUES (5, 2)");
        assert!(is_duplicate(&message), "{family:?}: {message}");
        assert_eq!(text_rows(&db, "SELECT id FROM s WHERE v = 2"), vec![vec!["2"]]);
    }
}

#[test]
fn a_check_constraint_over_inserted_rows_is_validated() {
    for family in FAMILIES {
        let db = seeded();
        ok(&db, family, "BEGIN");
        ok(&db, family, "INSERT INTO s VALUES (2, 2)");
        ok(&db, family, "ALTER TABLE s ADD CONSTRAINT s_v_check CHECK (v > 0)");
        let message = refused(&db, family, "INSERT INTO s VALUES (3, -1)");
        assert!(message.to_lowercase().contains("check"), "{family:?}: {message}");
        let _ = run(&db, family, "ROLLBACK");

        // An inserted row that violates it: the constraint's own error, nothing kept.
        let db = seeded();
        ok(&db, family, "BEGIN");
        ok(&db, family, "INSERT INTO s VALUES (2, -5)");
        let message = refused(&db, family, "ALTER TABLE s ADD CONSTRAINT s_v_check CHECK (v > 0)");
        assert!(message.contains("s_v_check"), "{family:?}: {message}");
        assert!(!message.contains(STAGED_ROWS_REFUSAL), "{family:?}: {message}");
        let _ = run(&db, family, "ROLLBACK");
        ok(&db, family, "INSERT INTO s VALUES (3, -1)");

        // And one that commits.
        let db = seeded();
        ok(&db, family, "BEGIN");
        ok(&db, family, "INSERT INTO s VALUES (2, 2)");
        ok(
            &db,
            family,
            "ALTER TABLE s ADD CONSTRAINT s_v_check CHECK (v > 0), ADD CONSTRAINT s_v_key UNIQUE (v)",
        );
        ok(&db, family, "COMMIT");
        let message = refused(&db, family, "INSERT INTO s VALUES (3, 0)");
        assert!(message.to_lowercase().contains("check"), "{family:?}: {message}");
        let message = refused(&db, family, "INSERT INTO s VALUES (3, 2)");
        assert!(is_duplicate(&message), "{family:?}: {message}");
    }
}

#[test]
fn what_is_still_refused() {
    for family in FAMILIES {
        // A staged UPDATE or DELETE of the table.
        for staged in ["UPDATE s SET v = 5 WHERE id = 1", "DELETE FROM s WHERE id = 1"] {
            for statement in [
                "ALTER TABLE s ADD CONSTRAINT s_v_key UNIQUE (v)",
                "ALTER TABLE s ADD CONSTRAINT s_v_check CHECK (v > 0)",
            ] {
                let db = seeded();
                ok(&db, family, "BEGIN");
                ok(&db, family, staged);
                let message = refused(&db, family, statement);
                assert!(
                    message.contains(STAGED_ROWS_REFUSAL),
                    "{family:?} {staged}; {statement}: {message}"
                );
                let _ = run(&db, family, "ROLLBACK");
            }
        }
        // The other constraint forms, over inserted rows.
        for statement in [
            "ALTER TABLE s ADD CONSTRAINT s_pk PRIMARY KEY (id)",
            "ALTER TABLE s ADD CONSTRAINT s_v_fkey FOREIGN KEY (v) REFERENCES par(id)",
            "ALTER TABLE s ADD COLUMN u INT UNIQUE DEFAULT 5",
            "ALTER TABLE s ADD COLUMN c INT DEFAULT -1 CHECK (c > 0)",
        ] {
            let db = seeded();
            ok(&db, family, "BEGIN");
            ok(&db, family, "INSERT INTO s VALUES (2, 2)");
            let message = refused(&db, family, statement);
            assert!(
                message.contains(STAGED_ROWS_REFUSAL),
                "{family:?} {statement}: {message}"
            );
            let _ = run(&db, family, "ROLLBACK");
        }
        // A savepoint in the transaction.
        let db = seeded();
        ok(&db, family, "BEGIN");
        ok(&db, family, "SAVEPOINT before_rows");
        ok(&db, family, "INSERT INTO s VALUES (2, 2)");
        let message = refused(&db, family, "ALTER TABLE s ADD CONSTRAINT s_v_key UNIQUE (v)");
        assert!(message.contains(STAGED_ROWS_REFUSAL), "{family:?}: {message}");
        let _ = run(&db, family, "ROLLBACK");
    }
}

#[tokio::test]
async fn a_seed_then_constrain_migration_over_the_wire() {
    let server = serve(seeded()).await;
    let (client, task) = connect(&server).await;
    wire_ok(&client, "BEGIN").await;
    wire_ok(&client, "INSERT INTO s VALUES (2, 2), (3, 3)").await;
    wire_ok(&client, "ALTER TABLE s ADD CONSTRAINT s_v_key UNIQUE (v)").await;
    wire_ok(&client, "ALTER TABLE s ADD CONSTRAINT s_v_check CHECK (v > 0)").await;
    wire_ok(&client, "COMMIT").await;
    assert_eq!(
        wire_rows(&client, "SELECT id FROM s WHERE v = 3").await,
        vec![vec!["3"]]
    );
    let (code, message) = wire_refused(&client, "INSERT INTO s VALUES (9, 3)").await;
    assert_eq!(code, "23505", "{message}");
    let (code, message) = wire_refused(&client, "INSERT INTO s VALUES (9, -3)").await;
    assert_eq!(code, "23514", "{message}");

    // A migration whose rows break the constraint: the constraint's own code.
    wire_ok(&client, "CREATE TABLE m (id INT, code TEXT)").await;
    wire_ok(&client, "BEGIN").await;
    wire_ok(&client, "INSERT INTO m VALUES (1, 'a'), (2, 'a')").await;
    let (code, message) = wire_refused(&client, "ALTER TABLE m ADD CONSTRAINT m_code_key UNIQUE (code)").await;
    assert_eq!(code, "23505", "{message}");
    wire_ok(&client, "ROLLBACK").await;
    task.abort();
}
