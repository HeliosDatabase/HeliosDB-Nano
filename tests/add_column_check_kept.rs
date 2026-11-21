//! A column-level `CHECK` given in `ALTER TABLE … ADD COLUMN` is kept.
//!
//! Two comments in the planner disagreed on whether it was (one said the CHECK
//! was "still dropped — a recorded gap"). It is kept: the clause is planned as an
//! `ADD CHECK` of the same statement. These cases pin that on both statement
//! families, over the PostgreSQL wire and across a restart.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

mod ddl_forms_support;
use ddl_forms_support::*;

const TABLE: &[&str] = &["CREATE TABLE t (id INT PRIMARY KEY)", "INSERT INTO t VALUES (1), (2)"];

#[test]
fn the_check_is_enforced_on_insert_and_update() {
    for family in FAMILIES {
        let db = setup(TABLE);
        ok(&db, family, "ALTER TABLE t ADD COLUMN qty INT CHECK (qty > 0)");
        // The rows that exist hold NULL, which a CHECK accepts.
        assert_eq!(text_rows(&db, "SELECT id, qty FROM t ORDER BY id").len(), 2);

        let message = refused(&db, family, "INSERT INTO t VALUES (3, -1)");
        assert!(message.to_lowercase().contains("check"), "{family:?}: {message}");
        ok(&db, family, "INSERT INTO t VALUES (3, 5)");
        let message = refused(&db, family, "UPDATE t SET qty = 0 WHERE id = 3");
        assert!(message.to_lowercase().contains("check"), "{family:?}: {message}");
        assert_eq!(
            text_rows(&db, "SELECT id, qty FROM t ORDER BY id"),
            vec![vec!["1", "NULL"], vec!["2", "NULL"], vec!["3", "5"]]
        );
    }
}

#[test]
fn a_named_check_keeps_its_name_and_a_default_that_breaks_it_refuses_the_statement() {
    for family in FAMILIES {
        let db = setup(TABLE);
        ok(
            &db,
            family,
            "ALTER TABLE t ADD COLUMN price INT CONSTRAINT price_positive CHECK (price > 0)",
        );
        let message = refused(&db, family, "INSERT INTO t VALUES (9, 0)");
        assert!(message.contains("price_positive"), "{family:?}: {message}");

        // Every existing row would hold 0, which the CHECK refuses: nothing is added.
        let message = refused(
            &db,
            family,
            "ALTER TABLE t ADD COLUMN stock INT DEFAULT 0 CHECK (stock > 0)",
        );
        assert!(message.to_lowercase().contains("check"), "{family:?}: {message}");
        let columns = text_rows(
            &db,
            "SELECT column_name FROM information_schema.columns WHERE table_name = 't' ORDER BY ordinal_position",
        );
        assert_eq!(columns, vec![vec!["id"], vec!["price"]], "{family:?}");
    }
}

#[test]
fn the_check_is_still_enforced_after_a_restart() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let db = open(dir.path());
        for statement in TABLE {
            db.execute(statement).unwrap();
        }
        db.execute("ALTER TABLE t ADD COLUMN qty INT CHECK (qty > 0)").unwrap();
        db.close().unwrap();
    }
    let db = open(dir.path());
    let message = refused(&db, Family::Text, "INSERT INTO t VALUES (3, -1)");
    assert!(message.to_lowercase().contains("check"), "{message}");
    ok(&db, Family::Params, "INSERT INTO t VALUES (3, 1)");
}

#[tokio::test]
async fn over_the_wire_a_violation_is_23514() {
    let server = serve(setup(TABLE)).await;
    let (client, task) = connect(&server).await;
    wire_ok(&client, "ALTER TABLE t ADD COLUMN qty INT CHECK (qty > 0)").await;
    let (code, message) = wire_refused(&client, "INSERT INTO t VALUES (3, -1)").await;
    assert_eq!(code, "23514", "{message}");
    // The extended protocol.
    let error = client
        .execute("INSERT INTO t VALUES ($1, $2)", &[&4i32, &-2i32])
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&error), "23514", "{error}");
    client
        .execute("INSERT INTO t VALUES ($1, $2)", &[&4i32, &2i32])
        .await
        .unwrap();
    assert_eq!(
        wire_rows(&client, "SELECT qty FROM t WHERE id = 4").await,
        vec![vec!["2"]]
    );
    task.abort();
}
