//! `SHOW` statements that were answered without being run, `SHOW DATABASES`,
//! `SHOW PROCESSLIST` and `KILL` on the MySQL listener.
//!
//! A `SHOW` the listener had no answer of its own for got an empty OK: a
//! client that asked `SHOW BRANCHES` was told there was nothing. Such a
//! statement is now run by the engine. `SHOW DATABASES` listed one fixed name
//! and `SHOW PROCESSLIST` only the asking connection; `KILL` did not exist.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::many_single_char_names)]

mod mysql_wire_client;

use mysql_wire_client::*;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[tokio::test]
async fn the_engines_show_statements_are_run() {
    let db = test_db();
    let mut c = trust_session(&db, CAPS).await;

    let branches = c.query("SHOW BRANCHES").await;
    assert!(matches!(branches, Answer::Rows { .. }), "SHOW BRANCHES returns rows, got {branches:?}");
    assert!(
        branches.rows().iter().any(|row| row.iter().any(|cell| cell == "main")),
        "the main branch is listed: {branches:?}"
    );
    let from_engine = db.query_with_columns("SHOW BRANCHES").unwrap();
    assert_eq!(branches.rows().len(), from_engine.0.len(), "the engine's own answer");
    assert_eq!(branches.columns(), from_engine.1);

    // The engine's answer or the engine's error: never an empty OK.
    let topology = c.query("SHOW TOPOLOGY").await;
    assert!(!topology.is_ok(), "SHOW TOPOLOGY is run by the engine, got {topology:?}");
    match db.query_with_columns("SHOW TOPOLOGY") {
        Ok((rows, columns)) => {
            assert_eq!(topology.columns(), columns);
            assert_eq!(topology.rows().len(), rows.len());
        }
        Err(_) => assert!(topology.err_code().is_some()),
    }

    // A parameter of the engine by its own name.
    assert_eq!(c.scalar("SHOW lock_timeout").await, "0");
    // A SHOW nobody knows is an error.
    assert!(c.query("SHOW NO_SUCH_THING_AT_ALL").await.err_code().is_some());
    // A table whose name holds another SHOW keyword is still about its columns.
    c.ok("CREATE TABLE my_tables_status (a INT PRIMARY KEY)").await;
    let columns = c.rows("SHOW COLUMNS FROM my_tables_status").await;
    assert_eq!(columns.len(), 1);
    assert_eq!(columns[0][0], "a");
}

#[tokio::test]
async fn show_databases_lists_the_databases_that_exist() {
    let db = test_db();
    let mut c = trust_session(&db, CAPS).await;
    let expected = |db: &heliosdb_nano::EmbeddedDatabase| -> Vec<String> {
        db.query_with_columns("SELECT datname FROM pg_database ORDER BY datname")
            .unwrap()
            .0
            .iter()
            .map(|row| match &row.values[0] {
                heliosdb_nano::Value::String(name) => name.clone(),
                other => panic!("{other:?}"),
            })
            .collect()
    };
    let before = expected(&db);
    assert!(!before.is_empty());
    let listed: Vec<String> = c.rows("SHOW DATABASES").await.into_iter().map(|row| row[0].clone()).collect();
    assert_eq!(listed, before);

    db.execute("CREATE DATABASE shop_db").unwrap();
    let after = expected(&db);
    assert!(after.contains(&"shop_db".to_string()), "{after:?}");
    let answer = c.query("SHOW DATABASES").await;
    assert_eq!(answer.columns(), ["Database"]);
    let listed: Vec<String> = answer.rows().into_iter().map(|row| row[0].clone()).collect();
    assert_eq!(listed, after, "a database made later is listed");
    assert_eq!(c.rows("SHOW SCHEMAS LIKE 'shop%'").await, [["shop_db"]]);
    assert!(c.rows("SHOW DATABASES LIKE 'no_such_%'").await.is_empty());
}

#[tokio::test]
async fn show_processlist_lists_the_live_sessions() {
    let db = test_db();
    let mut a = trust_session_as(&db, CAPS, "alice").await;
    let mut b = trust_session_as(&db, CAPS, "bob").await;
    let (a_id, b_id) = (a.greeting.connection_id, b.greeting.connection_id);
    assert_ne!(a_id, b_id);
    assert_eq!(a.scalar("SELECT CONNECTION_ID()").await, a_id.to_string());

    let answer = a.query("SHOW FULL PROCESSLIST").await;
    assert_eq!(answer.columns(), ["Id", "User", "Host", "db", "Command", "Time", "State", "Info"]);
    let rows = answer.rows();
    let row_of = |id: u32| {
        rows.iter()
            .find(|row| row[0] == id.to_string())
            .unwrap_or_else(|| panic!("session {id} is listed: {rows:?}"))
    };
    assert_eq!(row_of(a_id)[1], "alice");
    assert_eq!(row_of(a_id)[7], "SHOW PROCESSLIST");
    assert_eq!(row_of(b_id)[1], "bob");

    // A connection that ended is gone from the list.
    b.write_packet(0, &[0x01]).await; // COM_QUIT
    drop(b);
    let mut gone = false;
    for _ in 0..50 {
        let rows = a.rows("SHOW PROCESSLIST").await;
        if !rows.iter().any(|row| row[0] == b_id.to_string()) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(gone, "the closed session left the list");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_query_interrupts_the_statement_and_keeps_the_connection() {
    let db = test_db();
    let mut holder = trust_session(&db, CAPS).await;
    let mut waiter = trust_session(&db, CAPS).await;
    let mut killer = trust_session(&db, CAPS).await;
    holder.ok("CREATE TABLE kq (a INT PRIMARY KEY, b INT)").await;
    holder.ok("INSERT INTO kq VALUES (1, 0)").await;

    // The holder keeps a row locked; the waiter's UPDATE waits for it.
    holder.ok("BEGIN").await;
    holder.ok("UPDATE kq SET b = 1 WHERE a = 1").await;
    waiter.ok("SET innodb_lock_wait_timeout = 30").await;
    waiter.start_query("UPDATE kq SET b = 2 WHERE a = 1").await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let started = Instant::now();
    killer.ok(&format!("KILL QUERY {}", waiter.greeting.connection_id)).await;
    let answer = waiter.read_answer().await;
    assert_eq!(answer.err_code(), Some(1317), "the waiting statement was interrupted: {answer:?}");
    assert!(started.elapsed() < Duration::from_secs(15), "long before its lock timeout");

    // The connection lives on, and so does the holder's transaction.
    assert_eq!(waiter.scalar("SELECT 1 + 1").await, "2");
    holder.ok("COMMIT").await;
    assert_eq!(waiter.scalar("SELECT b FROM kq WHERE a = 1").await, "1");

    // A KILL QUERY that finds nothing running does nothing to the next statement.
    killer.ok(&format!("KILL QUERY {}", waiter.greeting.connection_id)).await;
    assert_eq!(waiter.affected("UPDATE kq SET b = 3 WHERE a = 1").await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_connection_ends_the_other_session() {
    let db = test_db();
    let mut victim = trust_session(&db, CAPS).await;
    let mut killer = trust_session(&db, CAPS).await;
    victim.ok("CREATE TABLE kc (a INT PRIMARY KEY)").await;
    victim.ok("BEGIN").await;
    victim.ok("INSERT INTO kc VALUES (1)").await;
    let id = victim.greeting.connection_id;

    killer.ok(&format!("KILL {id}")).await;
    // The victim is told and its connection closes; its transaction is rolled back.
    let mut closed = false;
    for _ in 0..3 {
        match tokio::time::timeout(Duration::from_secs(5), victim.try_read_packet()).await {
            Ok(None) => {
                closed = true;
                break;
            }
            Ok(Some(pkt)) => assert_eq!(pkt[0], 0xFF, "{pkt:?}"),
            Err(_) => panic!("the killed connection stayed open"),
        }
    }
    assert!(closed);
    let mut gone = false;
    for _ in 0..50 {
        if !killer.rows("SHOW PROCESSLIST").await.iter().any(|row| row[0] == id.to_string()) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(gone, "the killed session left the list");
    assert_eq!(killer.scalar("SELECT COUNT(*) FROM kc").await, "0");

    // Its id names nothing now; neither does one that never existed.
    let (code, message) = killer.err(&format!("KILL CONNECTION {id}")).await;
    assert_eq!(code, 1094, "{message}");
    assert_eq!(killer.err("KILL QUERY 2000000000").await.0, 1094);
    assert_eq!(killer.err("KILL QUERY soon").await.0, 1064);

    // Killing one's own connection ends it with MySQL's error for that.
    let own = killer.greeting.connection_id;
    let (code, _) = killer.err(&format!("KILL {own}")).await;
    assert_eq!(code, 1317);
    assert!(killer.try_read_packet().await.is_none());
}

#[tokio::test]
async fn only_the_owner_or_a_superuser_may_kill_when_logins_are_authenticated() {
    let db = test_db();
    let auth = scram_auth(&[("admin", "admin-secret")]);
    db.execute("CREATE ROLE ann LOGIN PASSWORD 'ann-pass'").unwrap();
    db.execute("CREATE ROLE ben LOGIN PASSWORD 'ben-pass'").unwrap();
    db.execute("CREATE ROLE boss SUPERUSER LOGIN PASSWORD 'boss-pass'").unwrap();
    let login = |user: &'static str, password: &'static str| {
        let db = Arc::clone(&db);
        let auth = Arc::clone(&auth);
        async move {
            let mut c = open(&db, Some(auth), true).await;
            assert!(c.login_sha2(CAPS, user, password, FullAuth::Cleartext).await.accepted());
            c
        }
    };
    let mut ann = login("ann", "ann-pass").await;
    let mut ann2 = login("ann", "ann-pass").await;
    let mut ben = login("ben", "ben-pass").await;
    let mut boss = login("boss", "boss-pass").await;

    let (code, message) = ben.err(&format!("KILL QUERY {}", ann.greeting.connection_id)).await;
    assert_eq!(code, 1095, "{message}");
    assert_eq!(ben.err(&format!("KILL {}", ann.greeting.connection_id)).await.0, 1095);
    assert_eq!(ann.scalar("SELECT 1").await, "1", "untouched");
    // Its owner, from another connection, and a superuser may.
    ann2.ok(&format!("KILL QUERY {}", ann.greeting.connection_id)).await;
    boss.ok(&format!("KILL QUERY {}", ben.greeting.connection_id)).await;
    assert_eq!(ben.scalar("SELECT 1").await, "1");
}
