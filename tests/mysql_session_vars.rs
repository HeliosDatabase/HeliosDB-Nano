//! Session variables on the MySQL listener are real.
//!
//! The listener used to answer OK to every `SET` and keep nothing: a client
//! that sent `SET autocommit = 0` believed it was in a transaction and was
//! not, and its `ROLLBACK` undid nothing. Each variable now takes effect, is
//! stored and read back with a documented "no effect", or is refused.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::many_single_char_names)]

mod mysql_wire_client;

use heliosdb_nano::protocol::postgres::timeouts::ConnectionTimeouts;
use mysql_wire_client::*;
use std::time::Duration;

const COM_STMT_PREPARE: u8 = 0x16;
const COM_STMT_EXECUTE: u8 = 0x17;

fn in_trans(status: u16) -> bool {
    status & SERVER_STATUS_IN_TRANS != 0
}

fn autocommit(status: u16) -> bool {
    status & SERVER_STATUS_AUTOCOMMIT != 0
}

// ---------------------------------------------------------------------------
// autocommit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn autocommit_off_opens_a_transaction_that_rollback_undoes() {
    let db = test_db();
    let mut c = trust_session(&db, CAPS).await;
    let mut other = trust_session(&db, CAPS).await;
    let status = c.ok("CREATE TABLE ac (a INT PRIMARY KEY)").await;
    assert!(autocommit(status) && !in_trans(status), "a fresh session: {status:#x}");

    let status = c.ok("SET autocommit = 0").await;
    assert!(!autocommit(status), "the OK of SET autocommit = 0 says autocommit is off");
    assert!(!in_trans(status), "nothing has opened a transaction yet");
    assert_eq!(c.scalar("SELECT @@autocommit").await, "0");
    assert_eq!(c.variable("autocommit").await, "OFF");

    let Answer::Ok { status, affected, .. } = c.query("INSERT INTO ac VALUES (1)").await else {
        panic!("INSERT");
    };
    assert_eq!(affected, 1);
    assert!(in_trans(status) && !autocommit(status), "the INSERT opened a transaction: {status:#x}");
    assert_eq!(c.scalar("SELECT COUNT(*) FROM ac").await, "1", "its own session sees the row");
    assert_eq!(other.scalar("SELECT COUNT(*) FROM ac").await, "0", "nobody else does");

    let status = c.ok("ROLLBACK").await;
    assert!(!in_trans(status));
    assert_eq!(c.scalar("SELECT COUNT(*) FROM ac").await, "0", "ROLLBACK undid the INSERT");
    c.ok("ROLLBACK").await;

    // And COMMIT keeps it.
    c.ok("INSERT INTO ac VALUES (2)").await;
    assert_eq!(other.scalar("SELECT COUNT(*) FROM ac").await, "0");
    let status = c.ok("COMMIT").await;
    assert!(!in_trans(status) && !autocommit(status));
    assert_eq!(other.scalar("SELECT COUNT(*) FROM ac").await, "1");

    // A read opens the transaction as well.
    let answer = c.query("SELECT COUNT(*) FROM ac").await;
    assert!(in_trans(answer.status()), "a SELECT under autocommit = 0 is in a transaction");
    c.ok("COMMIT").await;
}

#[tokio::test]
async fn statements_that_commit_implicitly_commit_the_open_transaction() {
    let db = test_db();
    let mut c = trust_session(&db, CAPS).await;
    let mut other = trust_session(&db, CAPS).await;
    c.ok("CREATE TABLE ic (a INT PRIMARY KEY)").await;
    c.ok("SET autocommit = 0").await;

    // DDL.
    c.ok("INSERT INTO ic VALUES (1)").await;
    let status = c.ok("CREATE TABLE ic_other (a INT PRIMARY KEY)").await;
    assert!(!in_trans(status), "DDL runs by itself");
    assert_eq!(other.scalar("SELECT COUNT(*) FROM ic").await, "1", "the DDL committed the INSERT");
    c.ok("ROLLBACK").await;
    assert_eq!(c.scalar("SELECT COUNT(*) FROM ic").await, "1");
    c.ok("COMMIT").await;

    // BEGIN / START TRANSACTION while one is open.
    c.ok("INSERT INTO ic VALUES (2)").await;
    let status = c.ok("START TRANSACTION").await;
    assert!(in_trans(status));
    assert_eq!(other.scalar("SELECT COUNT(*) FROM ic").await, "2", "BEGIN committed the open transaction");
    c.ok("INSERT INTO ic VALUES (3)").await;
    c.ok("ROLLBACK").await;
    assert_eq!(c.scalar("SELECT COUNT(*) FROM ic").await, "2", "the new transaction was rolled back alone");
    c.ok("COMMIT").await;

    // SET autocommit = 1.
    c.ok("INSERT INTO ic VALUES (4)").await;
    let status = c.ok("SET autocommit = 1").await;
    assert!(autocommit(status) && !in_trans(status));
    assert_eq!(other.scalar("SELECT COUNT(*) FROM ic").await, "3", "SET autocommit = 1 committed");
    // Back in autocommit: a statement is durable by itself and ROLLBACK undoes nothing.
    c.ok("INSERT INTO ic VALUES (5)").await;
    c.ok("ROLLBACK").await;
    assert_eq!(other.scalar("SELECT COUNT(*) FROM ic").await, "4");
}

#[tokio::test]
async fn the_status_flags_follow_explicit_transactions() {
    let db = test_db();
    let mut c = trust_session(&db, CAPS).await;
    c.ok("CREATE TABLE sf (a INT PRIMARY KEY)").await;
    assert!(in_trans(c.ok("BEGIN").await));
    assert!(in_trans(c.ok("INSERT INTO sf VALUES (1)").await));
    assert!(in_trans(c.query("SELECT * FROM sf").await.status()));
    assert!(!in_trans(c.ok("COMMIT").await));
    assert!(in_trans(c.ok("START TRANSACTION").await));
    assert!(!in_trans(c.ok("ROLLBACK").await));
    // COMMIT AND CHAIN opens the next one.
    c.ok("BEGIN").await;
    assert!(in_trans(c.ok("COMMIT AND CHAIN").await));
    assert!(!in_trans(c.ok("COMMIT").await));
    assert_eq!(c.err("COMMIT RELEASE").await.0, 1235);
}

#[tokio::test]
async fn autocommit_off_holds_for_prepared_statements() {
    let db = test_db();
    let mut c = trust_session(&db, CAPS).await;
    let mut other = trust_session(&db, CAPS).await;
    c.ok("CREATE TABLE ps (a INT PRIMARY KEY)").await;
    c.ok("SET autocommit = 0").await;

    let mut prepare = vec![COM_STMT_PREPARE];
    prepare.extend_from_slice(b"INSERT INTO ps VALUES (7)");
    c.write_packet(0, &prepare).await;
    let prepared = c.read_packet().await;
    assert_eq!(prepared[0], 0x00, "COM_STMT_PREPARE: {prepared:?}");
    let statement_id = [prepared[1], prepared[2], prepared[3], prepared[4]];

    let mut execute = vec![COM_STMT_EXECUTE];
    execute.extend_from_slice(&statement_id);
    execute.push(0); // no cursor
    execute.extend_from_slice(&1u32.to_le_bytes());
    c.write_packet(0, &execute).await;
    let Answer::Ok { affected, status, .. } = decode_ok(&c.read_packet().await) else {
        unreachable!()
    };
    assert_eq!(affected, 1);
    assert!(in_trans(status), "COM_STMT_EXECUTE opened the transaction: {status:#x}");
    assert_eq!(other.scalar("SELECT COUNT(*) FROM ps").await, "0");
    c.ok("ROLLBACK").await;
    assert_eq!(c.scalar("SELECT COUNT(*) FROM ps").await, "0", "ROLLBACK undid the prepared INSERT");
}

// ---------------------------------------------------------------------------
// Transaction characteristics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_isolation_level_is_the_engines_and_is_read_back() {
    let db = test_db();
    let mut c = trust_session(&db, CAPS).await;
    let mut other = trust_session(&db, CAPS).await;
    c.ok("CREATE TABLE iso (a INT PRIMARY KEY)").await;
    assert_eq!(c.scalar("SELECT @@transaction_isolation").await, "READ-COMMITTED");

    // READ COMMITTED: a commit by another session is seen inside the transaction.
    c.ok("BEGIN").await;
    assert_eq!(c.scalar("SELECT COUNT(*) FROM iso").await, "0");
    other.ok("INSERT INTO iso VALUES (1)").await;
    assert_eq!(c.scalar("SELECT COUNT(*) FROM iso").await, "1");
    c.ok("COMMIT").await;

    // REPEATABLE READ: the transaction keeps the snapshot it started with.
    c.ok("SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ").await;
    assert_eq!(c.scalar("SELECT @@transaction_isolation").await, "REPEATABLE-READ");
    assert_eq!(c.scalar("SELECT @@tx_isolation").await, "REPEATABLE-READ");
    assert_eq!(c.scalar("SELECT @@session.transaction_isolation").await, "REPEATABLE-READ");
    assert_eq!(c.variable("transaction_isolation").await, "REPEATABLE-READ");
    c.ok("BEGIN").await;
    assert_eq!(c.scalar("SELECT COUNT(*) FROM iso").await, "1");
    other.ok("INSERT INTO iso VALUES (2)").await;
    assert_eq!(c.scalar("SELECT COUNT(*) FROM iso").await, "1", "the snapshot of a REPEATABLE READ transaction");
    c.ok("COMMIT").await;
    assert_eq!(c.scalar("SELECT COUNT(*) FROM iso").await, "2");

    // The variable spellings.
    c.ok("SET transaction_isolation = 'READ-COMMITTED'").await;
    assert_eq!(c.scalar("SELECT @@transaction_isolation").await, "READ-COMMITTED");
    c.ok("SET @@session.tx_isolation = 'SERIALIZABLE'").await;
    assert_eq!(c.scalar("SELECT @@transaction_isolation").await, "SERIALIZABLE");
    // There are no dirty reads here: the nearest level is used, and reported.
    c.ok("SET SESSION TRANSACTION ISOLATION LEVEL READ UNCOMMITTED").await;
    assert_eq!(c.scalar("SELECT @@transaction_isolation").await, "READ-COMMITTED");
    assert_eq!(c.err("SET transaction_isolation = 'CHAOS'").await.0, 1231);
    assert_eq!(c.err("SET SESSION TRANSACTION ISOLATION LEVEL CHAOS").await.0, 1064);

    // The form without SESSION names the next transaction only, and cannot
    // be used inside one.
    c.ok("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ").await;
    assert_eq!(c.scalar("SELECT @@transaction_isolation").await, "READ-COMMITTED", "the session's level is unchanged");
    c.ok("BEGIN").await;
    assert_eq!(c.scalar("SELECT COUNT(*) FROM iso").await, "2");
    other.ok("INSERT INTO iso VALUES (3)").await;
    assert_eq!(c.scalar("SELECT COUNT(*) FROM iso").await, "2", "this one transaction is REPEATABLE READ");
    assert_eq!(c.err("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE").await.0, 1568);
    c.ok("COMMIT").await;
    c.ok("BEGIN").await;
    assert_eq!(c.scalar("SELECT COUNT(*) FROM iso").await, "3");
    other.ok("INSERT INTO iso VALUES (4)").await;
    assert_eq!(c.scalar("SELECT COUNT(*) FROM iso").await, "4", "the one after it is READ COMMITTED again");
    c.ok("COMMIT").await;
}

#[tokio::test]
async fn a_read_only_transaction_refuses_writes() {
    let db = test_db();
    let mut c = trust_session(&db, CAPS).await;
    c.ok("CREATE TABLE ro (a INT PRIMARY KEY)").await;
    c.ok("INSERT INTO ro VALUES (1)").await;

    c.ok("START TRANSACTION READ ONLY").await;
    let (code, message) = c.err("INSERT INTO ro VALUES (2)").await;
    assert_eq!(code, 1792, "{message}");
    assert_eq!(c.err("UPDATE ro SET a = 5").await.0, 1792);
    assert_eq!(c.err("DELETE FROM ro").await.0, 1792);
    assert_eq!(c.err("CREATE TABLE ro2 (a INT)").await.0, 1792);
    assert_eq!(c.scalar("SELECT COUNT(*) FROM ro").await, "1", "reads run");
    c.ok("COMMIT").await;
    c.ok("INSERT INTO ro VALUES (2)").await;

    // For the session: every transaction, and every statement by itself.
    c.ok("SET SESSION TRANSACTION READ ONLY").await;
    assert_eq!(c.scalar("SELECT @@transaction_read_only").await, "1");
    assert_eq!(c.scalar("SELECT @@tx_read_only").await, "1");
    assert_eq!(c.err("INSERT INTO ro VALUES (3)").await.0, 1792);
    c.ok("BEGIN").await;
    assert_eq!(c.err("INSERT INTO ro VALUES (3)").await.0, 1792);
    c.ok("ROLLBACK").await;
    // START TRANSACTION READ WRITE overrides it for that transaction.
    c.ok("START TRANSACTION READ WRITE").await;
    c.ok("INSERT INTO ro VALUES (3)").await;
    c.ok("COMMIT").await;
    c.ok("SET SESSION TRANSACTION READ WRITE").await;
    c.ok("INSERT INTO ro VALUES (4)").await;
    assert_eq!(c.scalar("SELECT COUNT(*) FROM ro").await, "4");

    // The next transaction only.
    c.ok("SET TRANSACTION READ ONLY").await;
    c.ok("BEGIN").await;
    assert_eq!(c.err("INSERT INTO ro VALUES (5)").await.0, 1792);
    c.ok("COMMIT").await;
    c.ok("INSERT INTO ro VALUES (5)").await;
}

// ---------------------------------------------------------------------------
// The variables of the brief, one by one
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sql_mode_is_stored_and_modes_that_change_semantics_are_refused() {
    let db = test_db();
    let mut c = trust_session(&db, CAPS).await;
    let default = c.scalar("SELECT @@sql_mode").await;
    assert!(default.contains("STRICT_TRANS_TABLES"), "{default}");

    c.ok("SET sql_mode = 'NO_AUTO_VALUE_ON_ZERO'").await;
    assert_eq!(c.scalar("SELECT @@sql_mode").await, "NO_AUTO_VALUE_ON_ZERO");
    assert_eq!(c.variable("sql_mode").await, "NO_AUTO_VALUE_ON_ZERO");
    c.ok("SET SESSION sql_mode = 'traditional,only_full_group_by'").await;
    assert_eq!(c.scalar("SELECT @@SESSION.sql_mode").await, "TRADITIONAL,ONLY_FULL_GROUP_BY");
    c.ok("SET sql_mode = ''").await;
    assert_eq!(c.scalar("SELECT @@sql_mode").await, "");

    for refused in ["ANSI_QUOTES", "NO_BACKSLASH_ESCAPES", "STRICT_TRANS_TABLES,ANSI", "PAD_CHAR_TO_FULL_LENGTH"] {
        let (code, message) = c.err(&format!("SET sql_mode = '{refused}'")).await;
        assert_eq!(code, 1231, "{refused}: {message}");
    }
    assert_eq!(c.err("SET sql_mode = 'NOT_A_MODE'").await.0, 1231);
    assert_eq!(c.scalar("SELECT @@sql_mode").await, "", "a refused value changes nothing");
    c.ok("SET sql_mode = DEFAULT").await;
    assert_eq!(c.scalar("SELECT @@sql_mode").await, default);
}

#[tokio::test]
async fn time_zone_and_character_sets_take_what_this_server_is() {
    let db = test_db();
    let mut c = trust_session(&db, CAPS).await;
    assert_eq!(c.scalar("SELECT @@time_zone").await, "SYSTEM");
    assert_eq!(c.scalar("SELECT @@system_time_zone").await, "UTC");
    c.ok("SET time_zone = '+00:00'").await;
    assert_eq!(c.scalar("SELECT @@time_zone").await, "+00:00");
    c.ok("SET time_zone = 'UTC'").await;
    // Sessions run in UTC: another zone would silently shift nothing, so it is refused.
    let (code, message) = c.err("SET time_zone = 'Europe/Madrid'").await;
    assert_eq!(code, 1231, "{message}");
    assert_eq!(c.err("SET time_zone = '+02:00'").await.0, 1231);
    assert_eq!(c.scalar("SELECT @@time_zone").await, "UTC");

    c.ok("SET NAMES utf8mb4").await;
    c.ok("SET NAMES 'utf8' COLLATE 'utf8_unicode_ci'").await;
    assert_eq!(c.scalar("SELECT @@character_set_client").await, "utf8");
    assert_eq!(c.scalar("SELECT @@character_set_connection").await, "utf8");
    assert_eq!(c.scalar("SELECT @@character_set_results").await, "utf8");
    assert_eq!(c.scalar("SELECT @@collation_connection").await, "utf8_unicode_ci");
    c.ok("SET CHARACTER SET utf8mb4").await;
    assert_eq!(c.scalar("SELECT @@character_set_client").await, "utf8mb4");
    c.ok("SET character_set_results = NULL").await;
    c.ok("SET collation_connection = utf8mb4_unicode_520_ci").await;
    assert_eq!(c.variable("collation_connection").await, "utf8mb4_unicode_520_ci");
    // This server stores and sends UTF-8 only.
    assert_eq!(c.err("SET NAMES latin1").await.0, 1231);
    assert_eq!(c.err("SET character_set_client = 'gbk'").await.0, 1231);
    assert_eq!(c.err("SET collation_connection = 'latin1_swedish_ci'").await.0, 1231);
    assert_eq!(c.scalar("SELECT @@character_set_client").await, "utf8mb4");
}

#[tokio::test]
async fn foreign_key_checks_off_is_for_this_session_only() {
    let db = test_db();
    let mut c = trust_session(&db, CAPS).await;
    let mut other = trust_session(&db, CAPS).await;
    c.ok("CREATE TABLE fk_parent (id INT PRIMARY KEY)").await;
    c.ok("CREATE TABLE fk_child (id INT PRIMARY KEY, parent_id INT, FOREIGN KEY (parent_id) REFERENCES fk_parent (id))")
        .await;
    assert!(c.query("INSERT INTO fk_child VALUES (1, 99)").await.err_code().is_some(), "checked by default");

    c.ok("SET foreign_key_checks = 0").await;
    assert_eq!(c.scalar("SELECT @@foreign_key_checks").await, "0");
    c.ok("INSERT INTO fk_child VALUES (1, 99)").await;
    // What a dump restore does: the child first, its parent later.
    c.ok("INSERT INTO fk_parent VALUES (99)").await;
    // Every other session is still held to the constraint.
    assert_eq!(other.scalar("SELECT @@foreign_key_checks").await, "1");
    assert!(other.query("INSERT INTO fk_child VALUES (2, 77)").await.err_code().is_some());

    c.ok("SET foreign_key_checks = 1").await;
    assert!(c.query("INSERT INTO fk_child VALUES (3, 55)").await.err_code().is_some(), "checked again");
    assert_eq!(c.scalar("SELECT COUNT(*) FROM fk_child").await, "1");

    // The form a dump uses, with the value saved and put back.
    c.ok("/*!40014 SET @OLD_FOREIGN_KEY_CHECKS=@@FOREIGN_KEY_CHECKS, FOREIGN_KEY_CHECKS=0 */").await;
    assert_eq!(c.scalar("SELECT @@foreign_key_checks").await, "0");
    c.ok("INSERT INTO fk_child VALUES (4, 44)").await;
    c.ok("/*!40014 SET FOREIGN_KEY_CHECKS=@OLD_FOREIGN_KEY_CHECKS */").await;
    assert_eq!(c.scalar("SELECT @@foreign_key_checks").await, "1");
    assert!(c.query("INSERT INTO fk_child VALUES (5, 33)").await.err_code().is_some());
}

#[tokio::test]
async fn sql_safe_updates_and_sql_select_limit_take_effect() {
    let db = test_db();
    let mut c = trust_session(&db, CAPS).await;
    c.ok("CREATE TABLE lim (a INT PRIMARY KEY)").await;
    c.ok("INSERT INTO lim VALUES (1), (2), (3), (4), (5)").await;

    c.ok("SET sql_safe_updates = 1").await;
    let (code, message) = c.err("DELETE FROM lim").await;
    assert_eq!(code, 1175, "{message}");
    assert_eq!(c.err("UPDATE lim SET a = a + 10").await.0, 1175);
    assert_eq!(c.affected("DELETE FROM lim WHERE a = 5").await, 1);
    c.ok("SET sql_safe_updates = 0").await;
    assert_eq!(c.affected("UPDATE lim SET a = a + 10").await, 4);

    c.ok("SET sql_select_limit = 2").await;
    assert_eq!(c.rows("SELECT a FROM lim ORDER BY a").await.len(), 2);
    assert_eq!(c.rows("SELECT a FROM lim ORDER BY a LIMIT 3").await.len(), 3, "the statement's own LIMIT wins");
    assert_eq!(c.scalar("SELECT @@sql_select_limit").await, "2");
    c.ok("SET sql_select_limit = DEFAULT").await;
    assert_eq!(c.rows("SELECT a FROM lim").await.len(), 4);
    assert_eq!(c.err("SET sql_select_limit = 'many'").await.0, 1232);
}

#[tokio::test]
async fn the_timeouts_map_onto_the_servers_own() {
    let db = test_db();
    let mut c = trust_session(&db, CAPS).await;

    // max_execution_time (milliseconds) is this session's statement timeout.
    c.ok("SET max_execution_time = 1500").await;
    assert_eq!(c.scalar("SELECT @@max_execution_time").await, "1500");
    assert_eq!(c.scalar("SHOW statement_timeout").await, "1500ms");
    c.ok("SET SESSION max_execution_time = 0").await;
    assert_eq!(c.scalar("SHOW statement_timeout").await, "0");
    assert_eq!(c.err("SET max_execution_time = -1").await.0, 1231);

    // innodb_lock_wait_timeout (seconds) is the lock timeout.
    c.ok("SET innodb_lock_wait_timeout = 7").await;
    assert_eq!(c.scalar("SELECT @@innodb_lock_wait_timeout").await, "7");
    assert_eq!(c.scalar("SHOW lock_timeout").await, "7s");

    // Stored and read back, with no effect here.
    c.ok("SET net_write_timeout = 120").await;
    assert_eq!(c.scalar("SELECT @@net_write_timeout").await, "120");
    assert_eq!(c.variable("net_write_timeout").await, "120");
    assert_eq!(c.err("SET net_write_timeout = 0").await.0, 1231);
    assert_eq!(c.err("SET net_write_timeout = 'soon'").await.0, 1232);
}

#[tokio::test]
async fn wait_timeout_closes_the_idle_connection() {
    let db = test_db();
    let mut c = open_with_timeouts(&db, None, false, ConnectionTimeouts::disabled()).await;
    c.send_handshake_response(CAPS, "root", &[], None, Some(NATIVE)).await;
    assert_eq!(c.read_packet().await[0], 0x00);

    c.ok("SET wait_timeout = 1").await;
    assert_eq!(c.scalar("SELECT @@wait_timeout").await, "1");
    assert_eq!(c.variable("wait_timeout").await, "1");
    tokio::time::sleep(Duration::from_millis(2500)).await;
    // The server said why and closed: nothing answers a query any more.
    let mut query = vec![COM_QUERY];
    query.extend_from_slice(b"SELECT 1");
    let _ = tokio::io::AsyncWriteExt::write_all(&mut c.stream, &[9, 0, 0, 0]).await;
    let _ = tokio::io::AsyncWriteExt::write_all(&mut c.stream, &query).await;
    let mut closed = false;
    for _ in 0..3 {
        match c.try_read_packet().await {
            None => {
                closed = true;
                break;
            }
            Some(pkt) => assert_eq!(pkt[0], 0xFF, "only the timeout notice may arrive: {pkt:?}"),
        }
    }
    assert!(closed, "the idle connection was closed after wait_timeout");
}

#[tokio::test]
async fn variables_this_server_behaves_as_one_value_of_refuse_the_other() {
    let db = test_db();
    let mut c = trust_session(&db, CAPS).await;
    c.ok("SET sql_auto_is_null = 0").await;
    let (code, message) = c.err("SET sql_auto_is_null = 1").await;
    assert_eq!(code, 1231, "{message}");
    assert_eq!(c.scalar("SELECT @@sql_auto_is_null").await, "0");

    // Stored and read back; unique constraints stay checked.
    c.ok("CREATE TABLE uq (a INT PRIMARY KEY, b INT UNIQUE)").await;
    c.ok("SET unique_checks = 0").await;
    assert_eq!(c.scalar("SELECT @@unique_checks").await, "0");
    c.ok("INSERT INTO uq VALUES (1, 1)").await;
    assert!(c.query("INSERT INTO uq VALUES (2, 1)").await.err_code().is_some());

    c.ok("SET sql_quote_show_create = 0").await;
    assert_eq!(c.scalar("SELECT @@sql_quote_show_create").await, "0");
    c.ok("SET sql_quote_show_create = ON").await;
    assert_eq!(c.variable("sql_quote_show_create").await, "ON");

    // Read-only variables.
    assert_eq!(c.err("SET version = '9'").await.0, 1238);
    assert_eq!(c.err("SET character_set_server = 'utf8mb4'").await.0, 1238);
    // A global value is the server's configuration, not a session's to change.
    assert_eq!(c.err("SET GLOBAL max_execution_time = 5").await.0, 1235);
    assert_eq!(c.err("SET @@global.sql_mode = ''").await.0, 1235);
}

#[tokio::test]
async fn an_unknown_system_variable_is_error_1193() {
    let db = test_db();
    let mut c = trust_session(&db, CAPS).await;
    let (code, message) = c.err("SET no_such_variable = 1").await;
    assert_eq!(code, 1193);
    assert_eq!(message, "Unknown system variable 'no_such_variable'");
    assert_eq!(c.err("SET SESSION no_such_variable = 1").await.0, 1193);
    assert_eq!(c.err("SELECT @@no_such_variable").await.0, 1193);
    assert_eq!(c.err("SELECT @@session.no_such_variable, @@version").await.0, 1193);
    // A SET of several stops at the first refusal.
    assert_eq!(c.err("SET sql_safe_updates = 1, no_such_variable = 1").await.0, 1193);
    assert_eq!(c.err("SET autocommit").await.0, 1064);
}

// ---------------------------------------------------------------------------
// User variables
// ---------------------------------------------------------------------------

#[tokio::test]
async fn user_variables_are_session_state() {
    let db = test_db();
    let mut c = trust_session(&db, CAPS).await;
    let mut other = trust_session(&db, CAPS).await;
    assert_eq!(c.scalar("SELECT @never_set").await, "NULL");

    c.ok("SET @n = 5, @s = 'it''s', @e := NULL").await;
    let answer = c.query("SELECT @n, @s, @e").await;
    assert_eq!(answer.columns(), ["@n", "@s", "@e"]);
    assert_eq!(answer.rows(), [["5", "it's", "NULL"]]);
    assert_eq!(other.scalar("SELECT @n").await, "NULL", "another session has its own");

    // An expression over variables, and a variable from a variable.
    c.ok("SET @m = @n * 2 + 1").await;
    assert_eq!(c.scalar("SELECT @m").await, "11");
    c.ok("SET @copy = @@sql_select_limit").await;
    assert_eq!(c.scalar("SELECT @copy").await, c.scalar("SELECT @@sql_select_limit").await);

    // `:=` inside a SELECT assigns and returns the value.
    assert_eq!(c.scalar("SELECT @counter := 10").await, "10");
    assert_eq!(c.scalar("SELECT @counter").await, "10");
    assert_eq!(c.scalar("SELECT @counter := @counter + 1").await, "11");
    assert_eq!(c.scalar("SELECT @counter").await, "11");

    // Inside statements that read and write tables.
    c.ok("CREATE TABLE uv (a INT PRIMARY KEY, b TEXT)").await;
    c.ok("INSERT INTO uv VALUES (@n, @s)").await;
    assert_eq!(c.rows("SELECT a, b FROM uv WHERE a = @n").await, [["5", "it's"]]);
    assert_eq!(c.scalar("SELECT COUNT(*) FROM uv WHERE b = @never_set").await, "0");
    // A string that only looks like a variable is text.
    c.ok("INSERT INTO uv VALUES (6, '@n and @@version')").await;
    assert_eq!(c.scalar("SELECT b FROM uv WHERE a = 6").await, "@n and @@version");

    // Not supported, and said so.
    assert_eq!(c.err("SELECT a INTO @picked FROM uv WHERE a = 5").await.0, 1235);
    assert_eq!(c.err("SELECT @t := a FROM uv").await.0, 1235);
}

// ---------------------------------------------------------------------------
// Reading: SELECT @@, SHOW VARIABLES, SHOW STATUS
// ---------------------------------------------------------------------------

#[tokio::test]
async fn select_and_show_read_the_same_store_and_the_engines_parameters() {
    let db = test_db();
    let mut c = trust_session(&db, CAPS).await;

    let answer = c
        .query("SELECT @@session.auto_increment_increment AS auto_increment_increment, @@character_set_client, @@max_allowed_packet")
        .await;
    assert_eq!(
        answer.columns(),
        ["auto_increment_increment", "@@character_set_client", "@@max_allowed_packet"]
    );
    assert_eq!(answer.rows(), [["1", "utf8mb4", "67108864"]]);

    c.ok("SET sql_select_limit = 7, sql_safe_updates = 1").await;
    assert_eq!(c.variable("sql_select_limit").await, "7");
    assert_eq!(c.variable("sql_safe_updates").await, "ON");
    // SESSION is the session's value, GLOBAL the server's.
    assert_eq!(c.rows("SHOW SESSION VARIABLES LIKE 'sql_safe_updates'").await, [["sql_safe_updates", "ON"]]);
    assert_eq!(c.rows("SHOW GLOBAL VARIABLES LIKE 'sql_safe_updates'").await, [["sql_safe_updates", "OFF"]]);
    assert_eq!(c.scalar("SELECT @@global.sql_safe_updates").await, "0");
    assert_eq!(
        c.rows("SHOW VARIABLES WHERE Variable_name = 'sql_safe_updates' OR Variable_name = 'autocommit'").await,
        [["autocommit", "ON"], ["sql_safe_updates", "ON"]]
    );
    assert_eq!(c.rows("SHOW VARIABLES WHERE Variable_name IN ('autocommit')").await, [["autocommit", "ON"]]);
    assert_eq!(c.err("SHOW VARIABLES WHERE Value > 3").await.0, 1235);
    // LIKE is a pattern over the whole name, as in MySQL.
    assert!(c.rows("SHOW VARIABLES LIKE 'timeout'").await.is_empty());
    let timeouts = c.rows("SHOW VARIABLES LIKE '%timeout'").await;
    assert!(timeouts.iter().any(|row| row[0] == "wait_timeout"), "{timeouts:?}");

    // The engine's parameters are listed under their own names and can be read as @@name.
    let all = c.rows("SHOW VARIABLES").await;
    let names: Vec<&str> = all.iter().map(|row| row[0].as_str()).collect();
    for expected in ["autocommit", "sql_mode", "transaction_isolation", "statement_timeout", "work_mem", "version"] {
        assert!(names.contains(&expected), "SHOW VARIABLES lists {expected}");
    }
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted, names, "sorted by name, each name once");
    c.ok("SET statement_timeout = 2500").await;
    assert_eq!(c.variable("statement_timeout").await, "2500ms");
    assert_eq!(c.scalar("SELECT @@statement_timeout").await, "2500ms");
    assert_eq!(c.scalar("SELECT @@max_execution_time").await, "2500", "one timeout under both names");

    // A variable in a statement that reads a table.
    c.ok("CREATE TABLE rv (a INT PRIMARY KEY)").await;
    c.ok("INSERT INTO rv VALUES (7), (8)").await;
    c.ok("SET sql_select_limit = DEFAULT").await;
    assert_eq!(c.rows("SELECT a FROM rv WHERE a = @@sql_select_limit + 0").await.len(), 0);
    c.ok("SET @seven = 7").await;
    assert_eq!(c.rows("SELECT a, @@version_comment FROM rv WHERE a = @seven").await, [["7", "HeliosDB Nano"]]);
}

#[tokio::test]
async fn show_status_reports_this_server() {
    let db = test_db();
    let mut c = trust_session(&db, CAPS).await;
    let _second = trust_session(&db, CAPS).await;
    let rows = c.rows("SHOW STATUS").await;
    let value = |name: &str| -> String {
        rows.iter()
            .find(|row| row[0] == name)
            .unwrap_or_else(|| panic!("SHOW STATUS lists {name}: {rows:?}"))[1]
            .clone()
    };
    assert!(value("Threads_connected").parse::<u64>().unwrap() >= 2);
    assert!(value("Questions").parse::<u64>().unwrap() >= 1);
    let _: u64 = value("Uptime").parse().unwrap();
    assert_eq!(c.rows("SHOW GLOBAL STATUS LIKE 'Threads_conn%'").await.len(), 1);
    assert_eq!(c.rows("SHOW SESSION STATUS LIKE 'Ssl_kx_group'").await.len(), 1);
    assert!(c.rows("SHOW STATUS LIKE 'No_such_counter'").await.is_empty());
}

// ---------------------------------------------------------------------------
// COM_RESET_CONNECTION
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reset_connection_forgets_session_state() {
    let db = test_db();
    let mut c = trust_session(&db, CAPS).await;
    c.ok("CREATE TABLE rc (a INT PRIMARY KEY)").await;
    c.ok("SET autocommit = 0, sql_select_limit = 1, @kept = 'x', foreign_key_checks = 0").await;
    c.ok("SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY").await;
    c.ok("SET max_execution_time = 900").await;

    let Answer::Ok { status, .. } = c.reset_connection().await else {
        panic!("COM_RESET_CONNECTION");
    };
    assert!(autocommit(status) && !in_trans(status));
    assert_eq!(
        c.query("SELECT @@autocommit, @@sql_select_limit, @kept, @@foreign_key_checks, @@transaction_isolation, @@transaction_read_only, @@max_execution_time")
            .await
            .rows(),
        [["1", "18446744073709551615", "NULL", "1", "READ-COMMITTED", "0", "0"]]
    );
    assert_eq!(c.scalar("SHOW statement_timeout").await, "0");
    // Autocommit is back: a statement is durable by itself.
    c.ok("INSERT INTO rc VALUES (1)").await;
    c.ok("ROLLBACK").await;
    assert_eq!(c.scalar("SELECT COUNT(*) FROM rc").await, "1");
}
