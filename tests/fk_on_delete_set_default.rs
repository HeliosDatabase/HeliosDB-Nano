//! `ON DELETE SET DEFAULT`, where a client meets it: the PostgreSQL wire (simple
//! and extended protocol), a session transaction that rolls back, a restart, and
//! the cases the default itself decides (it must satisfy the foreign key).
//!
//! `tests/fk_delete_actions.rs` covers the action on both embedded statement
//! families; these cases cover the surfaces that file does not.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

mod ddl_forms_support;
use ddl_forms_support::*;

const FIXTURE: &[&str] = &[
    "CREATE TABLE team (id INT PRIMARY KEY, name TEXT)",
    "INSERT INTO team VALUES (0, 'unassigned'), (1, 'red'), (2, 'blue')",
    "CREATE TABLE player (id INT PRIMARY KEY, team_id INT DEFAULT 0, \
     CONSTRAINT player_team FOREIGN KEY (team_id) REFERENCES team(id) ON DELETE SET DEFAULT)",
    "CREATE INDEX player_team_idx ON player (team_id)",
    "INSERT INTO player VALUES (10, 1), (11, 1), (12, 2), (13, NULL)",
];

const PLAYERS: &str = "SELECT id, team_id FROM player ORDER BY id";

fn after_deleting_red() -> Vec<Vec<&'static str>> {
    vec![vec!["10", "0"], vec!["11", "0"], vec!["12", "2"], vec!["13", "NULL"]]
}

#[test]
fn the_children_take_the_default_and_the_index_follows() {
    for family in FAMILIES {
        let db = setup(FIXTURE);
        assert_eq!(ok(&db, family, "DELETE FROM team WHERE id = 1"), 1);
        assert_eq!(text_rows(&db, PLAYERS), after_deleting_red());
        assert!(text_rows(&db, "SELECT id FROM player WHERE team_id = 1").is_empty());
        assert_eq!(
            text_rows(&db, "SELECT id FROM player WHERE team_id = 0 ORDER BY id"),
            vec![vec!["10"], vec!["11"]]
        );
    }
}

#[test]
fn a_default_that_is_the_deleted_key_or_names_no_parent_refuses_the_delete() {
    for family in FAMILIES {
        let db = setup(FIXTURE);
        // The default row itself: its children would keep pointing at it.
        ok(&db, family, "INSERT INTO player VALUES (14, 0)");
        let message = refused(&db, family, "DELETE FROM team WHERE id = 0");
        assert!(message.to_lowercase().contains("foreign key"), "{family:?}: {message}");
        assert_eq!(text_rows(&db, "SELECT count(*) FROM team"), vec![vec!["3"]]);
        assert_eq!(
            text_rows(&db, "SELECT team_id FROM player WHERE id = 14"),
            vec![vec!["0"]]
        );

        // A default no parent row has.
        let db = setup(&[
            "CREATE TABLE p (id INT PRIMARY KEY)",
            "INSERT INTO p VALUES (1), (2)",
            "CREATE TABLE c (id INT PRIMARY KEY, pid INT DEFAULT 99 REFERENCES p(id) ON DELETE SET DEFAULT)",
            "INSERT INTO c VALUES (1, 1)",
        ]);
        let message = refused(&db, family, "DELETE FROM p WHERE id = 1");
        assert!(message.to_lowercase().contains("foreign key"), "{family:?}: {message}");
        assert_eq!(text_rows(&db, "SELECT id, pid FROM c"), vec![vec!["1", "1"]]);
        assert_eq!(text_rows(&db, "SELECT count(*) FROM p"), vec![vec!["2"]]);
    }
}

#[test]
fn a_column_with_no_default_becomes_null() {
    for family in FAMILIES {
        let db = setup(&[
            "CREATE TABLE p (id INT PRIMARY KEY)",
            "INSERT INTO p VALUES (1), (2)",
            "CREATE TABLE c (id INT PRIMARY KEY, pid INT REFERENCES p(id) ON DELETE SET DEFAULT)",
            "INSERT INTO c VALUES (1, 1), (2, 2)",
        ]);
        ok(&db, family, "DELETE FROM p WHERE id = 1");
        assert_eq!(
            text_rows(&db, "SELECT id, pid FROM c ORDER BY id"),
            vec![vec!["1", "NULL"], vec!["2", "2"]]
        );
    }
}

#[test]
fn a_session_transaction_that_rolls_back_takes_the_action_back() {
    let db = setup(FIXTURE);
    let before = text_rows(&db, PLAYERS);
    let session = db.create_wire_session("u").unwrap();
    db.execute_for_session(session, "BEGIN").unwrap();
    db.execute_for_session(session, "DELETE FROM team WHERE id = 1")
        .unwrap();
    let (seen, _) = db
        .query_with_columns_for_session(session, "SELECT id FROM player WHERE team_id = 0 ORDER BY id")
        .unwrap();
    assert_eq!(seen.len(), 2, "the transaction sees its own action");
    db.execute_for_session(session, "ROLLBACK").unwrap();
    assert_eq!(text_rows(&db, PLAYERS), before);
    assert_eq!(
        text_rows(&db, "SELECT id FROM player WHERE team_id = 1 ORDER BY id"),
        vec![vec!["10"], vec!["11"]]
    );

    db.execute_for_session(session, "BEGIN").unwrap();
    db.execute_for_session(session, "DELETE FROM team WHERE id = 1")
        .unwrap();
    db.execute_for_session(session, "COMMIT").unwrap();
    db.destroy_session(session).unwrap();
    assert_eq!(text_rows(&db, PLAYERS), after_deleting_red());
}

#[test]
fn the_action_and_its_result_survive_a_restart() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let db = open(dir.path());
        for statement in FIXTURE {
            db.execute(statement).unwrap();
        }
        db.execute("DELETE FROM team WHERE id = 1").unwrap();
        db.close().unwrap();
    }
    let db = open(dir.path());
    assert_eq!(text_rows(&db, PLAYERS), after_deleting_red());
    // The constraint is still SET DEFAULT after the reopen.
    db.execute_params("DELETE FROM team WHERE id = 2", &[]).unwrap();
    assert_eq!(
        text_rows(&db, "SELECT team_id FROM player WHERE id = 12"),
        vec![vec!["0"]]
    );
}

#[tokio::test]
async fn over_the_wire_simple_and_extended() {
    let server = serve(setup(FIXTURE)).await;
    let (client, task) = connect(&server).await;
    // Extended protocol, with a parameter.
    let deleted = client
        .execute("DELETE FROM team WHERE id = $1", &[&1i32])
        .await
        .unwrap();
    assert_eq!(deleted, 1);
    assert_eq!(wire_rows(&client, PLAYERS).await, after_deleting_red());
    // Simple protocol, in an explicit transaction that rolls back.
    wire_ok(&client, "BEGIN").await;
    wire_ok(&client, "DELETE FROM team WHERE id = 2").await;
    assert_eq!(
        wire_rows(&client, "SELECT team_id FROM player WHERE id = 12").await,
        vec![vec!["0"]]
    );
    wire_ok(&client, "ROLLBACK").await;
    assert_eq!(
        wire_rows(&client, "SELECT team_id FROM player WHERE id = 12").await,
        vec![vec!["2"]]
    );
    // The default row cannot go while children would point at it: 23503.
    let (code, message) = wire_refused(&client, "DELETE FROM team WHERE id = 0").await;
    assert_eq!(code, "23503", "{message}");
    let error = client
        .execute("DELETE FROM team WHERE id = $1", &[&0i32])
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&error), "23503", "{error}");
    task.abort();
}
