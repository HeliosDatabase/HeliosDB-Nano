//! Several statements in one `COM_QUERY`.
//!
//! A batch that started with `SET`, `USE` or `BEGIN` used to be answered with
//! one OK and the rest of it dropped; a batch of writes was answered with one
//! summed OK. Each statement is now run and answered by itself, as MySQL does
//! for a client that negotiated `CLIENT_MULTI_STATEMENTS`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::many_single_char_names)]

mod mysql_wire_client;

use mysql_wire_client::*;

fn more(answer: &Answer) -> bool {
    answer.status() & SERVER_MORE_RESULTS_EXISTS != 0
}

#[tokio::test]
async fn set_then_insert_in_one_query_runs_both() {
    let db = test_db();
    let mut c = trust_session(&db, CAPS_MULTI).await;
    c.ok("CREATE TABLE ms (a INT PRIMARY KEY, b TEXT)").await;

    let answers = c.query_all("SET NAMES utf8mb4; INSERT INTO ms VALUES (1, 'one')").await;
    assert_eq!(answers.len(), 2, "{answers:?}");
    assert!(answers[0].is_ok() && more(&answers[0]), "the SET's own OK, with more to come");
    assert!(
        matches!(answers[1], Answer::Ok { affected: 1, .. }) && !more(&answers[1]),
        "the INSERT's own OK, the last: {:?}",
        answers[1]
    );
    assert_eq!(c.scalar("SELECT COUNT(*) FROM ms").await, "1", "the INSERT behind the SET ran");

    // USE first, and a trailing separator.
    let answers = c.query_all("USE heliosdb; INSERT INTO ms VALUES (2, 'two');").await;
    assert_eq!(answers.len(), 2);
    assert_eq!(c.scalar("SELECT COUNT(*) FROM ms").await, "2");

    // Each write is answered with its own count.
    let answers = c
        .query_all("INSERT INTO ms VALUES (3, 'x'), (4, 'x'); UPDATE ms SET b = 'y' WHERE a > 1; DELETE FROM ms WHERE a = 1")
        .await;
    let counts: Vec<u64> = answers
        .iter()
        .map(|a| match a {
            Answer::Ok { affected, .. } => *affected,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(counts, [2, 3, 1]);
}

#[tokio::test]
async fn begin_insert_commit_in_one_query_commits() {
    let db = test_db();
    let mut c = trust_session(&db, CAPS_MULTI).await;
    let mut other = trust_session(&db, CAPS).await;
    c.ok("CREATE TABLE mt (a INT PRIMARY KEY)").await;

    let answers = c.query_all("BEGIN; INSERT INTO mt VALUES (1); INSERT INTO mt VALUES (2); COMMIT").await;
    assert_eq!(answers.len(), 4, "{answers:?}");
    assert!(answers.iter().all(Answer::is_ok));
    assert_ne!(answers[0].status() & SERVER_STATUS_IN_TRANS, 0, "BEGIN opened the transaction");
    assert_ne!(answers[2].status() & SERVER_STATUS_IN_TRANS, 0);
    assert_eq!(answers[3].status() & SERVER_STATUS_IN_TRANS, 0, "COMMIT ended it");
    assert_eq!(other.scalar("SELECT COUNT(*) FROM mt").await, "2");

    // And a batch that rolls back leaves nothing.
    let answers = c.query_all("START TRANSACTION; INSERT INTO mt VALUES (3); ROLLBACK").await;
    assert_eq!(answers.len(), 3);
    assert_eq!(c.scalar("SELECT COUNT(*) FROM mt").await, "2");
    // No transaction is left open behind the batch.
    assert_eq!(c.ok("INSERT INTO mt VALUES (4)").await & SERVER_STATUS_IN_TRANS, 0);
    assert_eq!(other.scalar("SELECT COUNT(*) FROM mt").await, "3");
}

#[tokio::test]
async fn two_selects_return_two_result_sets_with_the_more_results_flag() {
    let db = test_db();
    let mut c = trust_session(&db, CAPS_MULTI).await;
    c.ok("CREATE TABLE mr (a INT PRIMARY KEY)").await;
    c.ok("INSERT INTO mr VALUES (1), (2)").await;

    let answers = c.query_all("SELECT a FROM mr ORDER BY a; SELECT COUNT(*) AS n FROM mr").await;
    assert_eq!(answers.len(), 2);
    assert_eq!(answers[0].rows(), [["1"], ["2"]]);
    assert!(more(&answers[0]));
    assert_eq!(answers[1].columns(), ["n"]);
    assert_eq!(answers[1].rows(), [["2"]]);
    assert!(!more(&answers[1]));

    // Result sets and OKs mixed, a session variable set and read in the same batch.
    let answers = c
        .query_all("SET @v = 41; UPDATE mr SET a = a + 10; SELECT @v + 1; SELECT a FROM mr ORDER BY a")
        .await;
    assert_eq!(answers.len(), 4, "{answers:?}");
    assert!(answers[0].is_ok());
    assert!(matches!(answers[1], Answer::Ok { affected: 2, .. }));
    assert_eq!(answers[2].rows(), [["42"]]);
    assert_eq!(answers[3].rows(), [["11"], ["12"]]);
}

#[tokio::test]
async fn an_error_stops_the_batch_and_keeps_what_ran_before_it() {
    let db = test_db();
    let mut c = trust_session(&db, CAPS_MULTI).await;
    c.ok("CREATE TABLE me (a INT PRIMARY KEY)").await;

    let answers = c
        .query_all("INSERT INTO me VALUES (1); INSERT INTO no_such_table VALUES (1); INSERT INTO me VALUES (3)")
        .await;
    assert_eq!(answers.len(), 2, "the error is the last answer: {answers:?}");
    assert!(answers[0].is_ok() && more(&answers[0]));
    assert!(answers[1].err_code().is_some());
    assert_eq!(c.rows("SELECT a FROM me").await, [["1"]], "the first ran, the third did not");

    // A refused SET in the middle stops it as well.
    let answers = c.query_all("INSERT INTO me VALUES (5); SET no_such_variable = 1; INSERT INTO me VALUES (6)").await;
    assert_eq!(answers.len(), 2);
    assert_eq!(answers[1].err_code(), Some(1193));
    assert_eq!(c.scalar("SELECT COUNT(*) FROM me").await, "2");
}

#[tokio::test]
async fn a_batch_from_a_client_that_did_not_ask_for_them_is_error_1064() {
    let db = test_db();
    let mut c = trust_session(&db, CAPS).await;
    c.ok("CREATE TABLE mn (a INT PRIMARY KEY, b TEXT)").await;

    let (code, message) = c.err("INSERT INTO mn VALUES (1, 'a'); INSERT INTO mn VALUES (2, 'b')").await;
    assert_eq!(code, 1064, "{message}");
    assert_eq!(c.err("SET NAMES utf8mb4; INSERT INTO mn VALUES (1, 'a')").await.0, 1064);
    assert_eq!(c.scalar("SELECT COUNT(*) FROM mn").await, "0", "nothing of a refused batch ran");

    // One statement stays one statement: a trailing separator, a separator
    // inside a literal, inside a quoted name, inside a comment.
    c.ok("INSERT INTO mn VALUES (1, 'a;b');").await;
    c.ok("INSERT INTO mn VALUES (2, 'it''s; fine') /* ; */ ; -- ; done").await;
    assert_eq!(c.scalar("SELECT b FROM mn WHERE a = 1").await, "a;b");
    assert_eq!(c.scalar("SELECT b FROM mn WHERE a = 2").await, "it's; fine");
    assert_eq!(c.rows("SELECT a AS `x;y` FROM mn ORDER BY a;").await.len(), 2);
    // Nothing but separators and comments is an empty query.
    assert!(c.query(" ; /* nothing */ ; ").await.is_ok());
}
