//! MySQL wire: translator DML/type fixes over the text protocol.
//!
//! * v02.3 - type-name rewrites apply to DDL only; CAST targets are mapped
//! * v02.6 - SQLite conflict clauses come from the parser; `REPLACE INTO`
//! * v02.7 - `INSERT ... SELECT` honours `ON CONFLICT` (MySQL spellings)
//! * multi-table `DELETE` decides the rows of every named table once
//!
//! The client below is the raw-packet text-protocol client of
//! `tests/mysql_wire_hygiene_batch_d.rs`, extended with `err`.

#![allow(dead_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use heliosdb_nano::protocol::mysql::MySqlHandler;
use heliosdb_nano::protocol::postgres::timeouts::ConnectionTimeouts;
use heliosdb_nano::EmbeddedDatabase;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

// ------------------------------------------------------------ wire client --

const COM_QUERY: u8 = 0x03;
const COM_RESET_CONNECTION: u8 = 0x1f;
const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
/// `StatusFlags::SERVER_STATUS_IN_TRANS`.
const SERVER_STATUS_IN_TRANS: u16 = 0x0001;

/// MySQL's documented maximum for `wait_timeout` and friends — what the
/// listener reports when no idle budget is configured (MySQL has no
/// `0 = disabled` spelling for these variables).
const NEVER: &str = "31536000";

fn read_lenenc_u64(buf: &[u8], pos: &mut usize) -> u64 {
    let first = buf[*pos];
    *pos += 1;
    match first {
        0xFC => {
            let v = u64::from(u16::from_le_bytes([buf[*pos], buf[*pos + 1]]));
            *pos += 2;
            v
        }
        0xFD => {
            let v = u64::from(buf[*pos]) | (u64::from(buf[*pos + 1]) << 8) | (u64::from(buf[*pos + 2]) << 16);
            *pos += 3;
            v
        }
        0xFE => {
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[*pos..*pos + 8]);
            *pos += 8;
            u64::from_le_bytes(b)
        }
        v => u64::from(v),
    }
}

fn read_lenenc_str(buf: &[u8], pos: &mut usize) -> String {
    let len = read_lenenc_u64(buf, pos) as usize;
    let s = String::from_utf8_lossy(&buf[*pos..*pos + len]).into_owned();
    *pos += len;
    s
}

struct MySqlTestClient {
    stream: DuplexStream,
}

impl MySqlTestClient {
    /// Log in over an in-memory duplex stream against a listener with NO
    /// connection-lifetime policy (the embedder / pre-GH#28 default).
    async fn login(db: Arc<EmbeddedDatabase>) -> Self {
        Self::login_with_timeouts(db, ConnectionTimeouts::disabled()).await
    }

    /// Log in against a listener running the given GH#28 policy — the same
    /// entry point `MysqlServer` uses for an accepted socket.
    async fn login_with_timeouts(db: Arc<EmbeddedDatabase>, policy: ConnectionTimeouts) -> Self {
        let (client, srv) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move {
            let _ = MySqlHandler::handle_connection_with_timeouts(db, srv, 1, policy).await;
        });
        let mut this = Self { stream: client };

        let (_seq, _greeting) = this.read_packet().await;
        let mut p = Vec::new();
        p.extend_from_slice(&(CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION).to_le_bytes());
        p.extend_from_slice(&(1u32 << 24).to_le_bytes());
        p.push(45); // utf8mb4_general_ci
        p.extend_from_slice(&[0u8; 23]);
        p.extend_from_slice(b"root");
        p.push(0);
        p.push(0); // empty auth response (SECURE_CONNECTION framing)
        this.write_packet(1, &p).await;

        let (_seq, ok) = this.read_packet().await;
        assert_eq!(ok[0], 0x00, "expected auth OK, got {:?}", &ok[..ok.len().min(16)]);
        this
    }

    async fn write_packet(&mut self, seq: u8, payload: &[u8]) {
        let len = payload.len();
        let hdr = [
            (len & 0xFF) as u8,
            ((len >> 8) & 0xFF) as u8,
            ((len >> 16) & 0xFF) as u8,
            seq,
        ];
        self.stream.write_all(&hdr).await.unwrap();
        self.stream.write_all(payload).await.unwrap();
        self.stream.flush().await.unwrap();
    }

    async fn read_packet(&mut self) -> (u8, Vec<u8>) {
        let mut hdr = [0u8; 4];
        self.stream.read_exact(&mut hdr).await.unwrap();
        let len = (hdr[0] as usize) | ((hdr[1] as usize) << 8) | ((hdr[2] as usize) << 16);
        let mut payload = vec![0u8; len];
        self.stream.read_exact(&mut payload).await.unwrap();
        (hdr[3], payload)
    }

    /// COM_QUERY; returns the first response packet.
    async fn send(&mut self, sql: &str) -> Vec<u8> {
        let mut p = vec![COM_QUERY];
        p.extend_from_slice(sql.as_bytes());
        self.write_packet(0, &p).await;
        let (_seq, first) = self.read_packet().await;
        first
    }

    /// Decode an OK packet — `0x00`, lenenc affected rows, lenenc
    /// last_insert_id, u16 status flags.
    fn decode_ok(pkt: &[u8], what: &str) -> (u64, u64, u16) {
        assert_eq!(
            pkt.first().copied(),
            Some(0x00),
            "expected OK for {what}, got {}",
            String::from_utf8_lossy(pkt)
        );
        let mut pos = 1usize;
        let affected = read_lenenc_u64(pkt, &mut pos);
        let last_insert_id = read_lenenc_u64(pkt, &mut pos);
        assert!(pkt.len() >= pos + 2, "short OK packet for {what}");
        let status = u16::from_le_bytes([pkt[pos], pkt[pos + 1]]);
        (affected, last_insert_id, status)
    }

    /// COM_QUERY that must answer OK; returns the status flags.
    async fn ok(&mut self, sql: &str) -> u16 {
        let pkt = self.send(sql).await;
        Self::decode_ok(&pkt, &format!("`{sql}`")).2
    }

    /// COM_QUERY that must answer OK; returns (affected rows, last_insert_id).
    async fn ok_insert(&mut self, sql: &str) -> (u64, u64) {
        let pkt = self.send(sql).await;
        let (affected, last_insert_id, _status) = Self::decode_ok(&pkt, &format!("`{sql}`"));
        (affected, last_insert_id)
    }

    /// `COM_RESET_CONNECTION`; returns the OK packet's status flags.
    async fn reset_connection(&mut self) -> u16 {
        self.write_packet(0, &[COM_RESET_CONNECTION]).await;
        let (_seq, pkt) = self.read_packet().await;
        Self::decode_ok(&pkt, "COM_RESET_CONNECTION").2
    }

    /// COM_QUERY that must answer a text-protocol result set; returns
    /// (column names, rows). NULL decodes as an empty string — nothing here
    /// distinguishes the two.
    async fn query(&mut self, sql: &str) -> (Vec<String>, Vec<Vec<String>>) {
        let first = self.send(sql).await;
        assert_ne!(
            first.first().copied(),
            Some(0xFF),
            "server error for `{sql}`: {}",
            String::from_utf8_lossy(&first)
        );
        assert_ne!(
            first.first().copied(),
            Some(0x00),
            "expected a result set for `{sql}`, got an OK packet"
        );

        let mut pos = 0usize;
        let ncols = read_lenenc_u64(&first, &mut pos) as usize;

        let mut names = Vec::with_capacity(ncols);
        for _ in 0..ncols {
            let (_seq, def) = self.read_packet().await;
            let mut dp = 0usize;
            // catalog, schema, virtual table, physical table, then the name.
            for _ in 0..4 {
                let _ = read_lenenc_str(&def, &mut dp);
            }
            names.push(read_lenenc_str(&def, &mut dp));
        }

        let (_seq, eof) = self.read_packet().await;
        assert_eq!(eof[0], 0xFE, "expected EOF after the column defs for `{sql}`");

        let mut rows = Vec::new();
        loop {
            let (_seq, pkt) = self.read_packet().await;
            if pkt[0] == 0xFE && pkt.len() < 9 {
                break;
            }
            let mut rp = 0usize;
            let mut row = Vec::with_capacity(ncols);
            for _ in 0..ncols {
                if pkt[rp] == 0xFB {
                    rp += 1;
                    row.push(String::new());
                } else {
                    row.push(read_lenenc_str(&pkt, &mut rp));
                }
            }
            rows.push(row);
        }
        (names, rows)
    }

    /// One-column, one-row result set → that value.
    async fn scalar(&mut self, sql: &str) -> String {
        let (_names, rows) = self.query(sql).await;
        assert_eq!(rows.len(), 1, "expected one row from `{sql}`, got {rows:?}");
        assert_eq!(rows[0].len(), 1, "expected one column from `{sql}`");
        rows[0][0].clone()
    }

    /// `SHOW VARIABLES LIKE '<pattern>'` → the single row it must return.
    async fn one_variable(&mut self, pattern: &str) -> (String, String) {
        let (cols, rows) = self.query(&format!("SHOW VARIABLES LIKE '{pattern}'")).await;
        assert_eq!(cols, ["Variable_name", "Value"]);
        assert_eq!(rows.len(), 1, "SHOW VARIABLES LIKE '{pattern}' listed {rows:?}");
        (rows[0][0].clone(), rows[0][1].clone())
    }
}

fn test_db() -> Arc<EmbeddedDatabase> {
    Arc::new(EmbeddedDatabase::new_in_memory().expect("in-memory db"))
}

impl MySqlTestClient {
    /// COM_QUERY that must answer an ERR packet; returns (code, message).
    async fn err(&mut self, sql: &str) -> (u16, String) {
        let pkt = self.send(sql).await;
        assert_eq!(pkt.first().copied(), Some(0xFF), "expected ERR for `{sql}`");
        (
            u16::from_le_bytes([pkt[1], pkt[2]]),
            String::from_utf8_lossy(&pkt[3..]).into_owned(),
        )
    }

    /// Rows of a query, each row joined with `|`, sorted.
    async fn rows(&mut self, sql: &str) -> Vec<String> {
        let (_c, rows) = self.query(sql).await;
        let mut out: Vec<String> = rows.into_iter().map(|r| r.join("|")).collect();
        out.sort();
        out
    }
}

// ===========================================================================
// v02.3 - type-name rewrites are DDL-only; CAST targets are mapped
// ===========================================================================

#[tokio::test]
async fn archive_query_runs_over_the_wire() {
    let mut c = MySqlTestClient::login(test_db()).await;
    c.ok("CREATE TABLE wp_posts (ID BIGINT UNSIGNED NOT NULL AUTO_INCREMENT, post_date DATETIME NOT NULL, PRIMARY KEY (ID)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4").await;
    c.ok("INSERT INTO wp_posts (post_date) VALUES ('2026-03-05 10:00:00'), ('2026-03-20 11:00:00'), ('2026-04-01 09:00:00')").await;
    let rows = c
        .rows("SELECT YEAR(post_date), MONTH(post_date), COUNT(*) FROM wp_posts GROUP BY YEAR(post_date), MONTH(post_date)")
        .await;
    assert_eq!(rows, ["2026|3|2", "2026|4|1"]);
}

#[tokio::test]
async fn cast_meta_value_as_signed_runs() {
    let mut c = MySqlTestClient::login(test_db()).await;
    c.ok("CREATE TABLE wp_postmeta (meta_id INT NOT NULL, meta_value VARCHAR(20))")
        .await;
    c.ok("INSERT INTO wp_postmeta VALUES (1, '10'), (2, '9'), (3, '100')")
        .await;
    let (_c, rows) = c
        .query("SELECT meta_id FROM wp_postmeta ORDER BY CAST(meta_value AS SIGNED) DESC")
        .await;
    let ids: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
    assert_eq!(ids, ["3", "1", "2"]);
    assert_eq!(c.scalar("SELECT CAST('7' AS UNSIGNED)").await, "7");
}

// ===========================================================================
// v02.6 - REPLACE INTO through the parser; v02.7 - INSERT ... SELECT conflicts
// ===========================================================================

#[tokio::test]
async fn replace_into_twice_leaves_one_row_and_sets_last_insert_id() {
    let mut c = MySqlTestClient::login(test_db()).await;
    c.ok("CREATE TABLE r (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(20))")
        .await;
    let (affected, last_id) = c.ok_insert("REPLACE INTO r (name) VALUES ('x')").await;
    assert_eq!((affected, last_id), (1, 1));
    c.ok_insert("REPLACE INTO r (id, name) VALUES (1, 'y')").await;
    c.ok_insert("REPLACE INTO `r` (`id`, `name`) VALUES (1, 'z')").await;
    assert_eq!(c.rows("SELECT * FROM r").await, ["1|z"]);
    // the no-column-list form
    c.ok_insert("REPLACE INTO r VALUES (1, 'w')").await;
    assert_eq!(c.rows("SELECT * FROM r").await, ["1|w"]);
}

#[tokio::test]
async fn insert_ignore_select_skips_duplicates() {
    let mut c = MySqlTestClient::login(test_db()).await;
    c.ok("CREATE TABLE u (id INT PRIMARY KEY, email VARCHAR(20))").await;
    c.ok("CREATE TABLE src (id INT, email VARCHAR(20))").await;
    c.ok("INSERT INTO u VALUES (1, 'old1'), (2, 'old2')").await;
    c.ok("INSERT INTO src VALUES (1, 'new1'), (3, 'new3')").await;
    let (affected, _id) = c
        .ok_insert("INSERT IGNORE INTO u (id, email) SELECT id, email FROM src")
        .await;
    assert_eq!(affected, 1);
    assert_eq!(c.rows("SELECT * FROM u").await, ["1|old1", "2|old2", "3|new3"]);
}

#[tokio::test]
async fn insert_select_on_duplicate_key_update() {
    let mut c = MySqlTestClient::login(test_db()).await;
    c.ok("CREATE TABLE u (id INT PRIMARY KEY, email VARCHAR(20))").await;
    c.ok("CREATE TABLE src (id INT, email VARCHAR(20))").await;
    c.ok("INSERT INTO u VALUES (1, 'old1'), (2, 'old2')").await;
    c.ok("INSERT INTO src VALUES (1, 'new1'), (3, 'new3')").await;
    c.ok_insert("INSERT INTO u (id, email) SELECT id, email FROM src ON DUPLICATE KEY UPDATE email = VALUES(email)")
        .await;
    assert_eq!(c.rows("SELECT * FROM u").await, ["1|new1", "2|old2", "3|new3"]);
}

// ===========================================================================
// multi-table DELETE: the target rows of every named table are decided once
// ===========================================================================

async fn two_tables(c: &mut MySqlTestClient) {
    c.ok("CREATE TABLE t1 (id INT PRIMARY KEY, k VARCHAR(10))").await;
    c.ok("CREATE TABLE t2 (id INT PRIMARY KEY, t1_id INT, flag INT)").await;
    c.ok("INSERT INTO t1 VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd')")
        .await;
    c.ok("INSERT INTO t2 VALUES (10, 1, 1), (11, 2, 1), (12, 3, 0)").await;
}

#[tokio::test]
async fn multi_table_delete_removes_the_joined_rows_of_both_tables() {
    let mut c = MySqlTestClient::login(test_db()).await;
    two_tables(&mut c).await;
    let (affected, _) = c
        .ok_insert("DELETE t1, t2 FROM t1 INNER JOIN t2 ON t1.id = t2.t1_id WHERE t2.flag = 1")
        .await;
    assert_eq!(affected, 4, "two rows from each table");
    assert_eq!(c.rows("SELECT id FROM t1").await, ["3", "4"]);
    assert_eq!(c.rows("SELECT id FROM t2").await, ["12"]);
}

#[tokio::test]
async fn multi_table_delete_with_aliases_and_backticks() {
    let mut c = MySqlTestClient::login(test_db()).await;
    two_tables(&mut c).await;
    c.ok("DELETE `a`, `b` FROM `t1` AS `a` JOIN `t2` AS `b` ON `a`.id = `b`.t1_id WHERE `b`.flag = 1")
        .await;
    assert_eq!(c.rows("SELECT id FROM t1").await, ["3", "4"]);
    assert_eq!(c.rows("SELECT id FROM t2").await, ["12"]);
}

#[tokio::test]
async fn multi_table_delete_names_only_one_table() {
    let mut c = MySqlTestClient::login(test_db()).await;
    two_tables(&mut c).await;
    let (affected, _) = c
        .ok_insert("DELETE a FROM t1 a JOIN t2 b ON a.id = b.t1_id WHERE b.flag = 1")
        .await;
    assert_eq!(affected, 2);
    assert_eq!(c.rows("SELECT id FROM t1").await, ["3", "4"]);
    assert_eq!(
        c.rows("SELECT id FROM t2").await,
        ["10", "11", "12"],
        "the joined table is not touched"
    );
}

#[tokio::test]
async fn multi_table_delete_without_where_and_with_left_join() {
    let mut c = MySqlTestClient::login(test_db()).await;
    two_tables(&mut c).await;
    // rows of t1 without a partner (anti-join)
    c.ok("DELETE t1 FROM t1 LEFT JOIN t2 ON t1.id = t2.t1_id WHERE t2.id IS NULL")
        .await;
    assert_eq!(c.rows("SELECT id FROM t1").await, ["1", "2", "3"]);
    // no WHERE: every joined row
    c.ok("DELETE t1, t2 FROM t1 JOIN t2 ON t1.id = t2.t1_id").await;
    assert_eq!(c.rows("SELECT id FROM t1").await, Vec::<String>::new());
    assert_eq!(c.rows("SELECT id FROM t2").await, Vec::<String>::new());
}

#[tokio::test]
async fn multi_table_delete_comma_join_form() {
    let mut c = MySqlTestClient::login(test_db()).await;
    two_tables(&mut c).await;
    c.ok("DELETE a, b FROM t1 a, t2 b WHERE a.id = b.t1_id AND b.flag = 1")
        .await;
    assert_eq!(c.rows("SELECT id FROM t1").await, ["3", "4"]);
    assert_eq!(c.rows("SELECT id FROM t2").await, ["12"]);
}

#[tokio::test]
async fn multi_table_delete_using_form() {
    let mut c = MySqlTestClient::login(test_db()).await;
    two_tables(&mut c).await;
    c.ok("DELETE FROM t1, t2 USING t1 INNER JOIN t2 ON t1.id = t2.t1_id WHERE t2.flag = 1")
        .await;
    assert_eq!(c.rows("SELECT id FROM t1").await, ["3", "4"]);
    assert_eq!(c.rows("SELECT id FROM t2").await, ["12"]);
}

#[tokio::test]
async fn wordpress_transient_cleanup_self_join_deletes_both_rows() {
    let mut c = MySqlTestClient::login(test_db()).await;
    c.ok("CREATE TABLE wp_options (option_id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT, option_name VARCHAR(191), option_value TEXT, PRIMARY KEY (option_id))")
        .await;
    c.ok("INSERT INTO wp_options (option_name, option_value) VALUES \
          ('_transient_a', 'v'), ('_transient_timeout_a', '5'), \
          ('_transient_b', 'v'), ('_transient_timeout_b', '999999'), ('siteurl', 'x')")
        .await;
    let (affected, _) = c
        .ok_insert(
            "DELETE a, b FROM wp_options a, wp_options b WHERE a.option_name LIKE '_transient_%' \
             AND a.option_name NOT LIKE '_transient_timeout_%' \
             AND b.option_name = CONCAT('_transient_timeout_', SUBSTRING(a.option_name, 12)) AND b.option_value < '6'",
        )
        .await;
    assert_eq!(affected, 2);
    assert_eq!(
        c.rows("SELECT option_name FROM wp_options").await,
        ["_transient_b", "_transient_timeout_b", "siteurl"]
    );
}

#[tokio::test]
async fn multi_table_delete_failure_leaves_both_tables_untouched() {
    let mut c = MySqlTestClient::login(test_db()).await;
    two_tables(&mut c).await;
    // t3 has no primary key, so the rows to delete cannot be named: an explicit error, nothing deleted
    c.ok("CREATE TABLE t3 (x INT)").await;
    c.ok("INSERT INTO t3 VALUES (1)").await;
    let (_code, _msg) = c.err("DELETE t1, t3 FROM t1 JOIN t3 ON t1.id = t3.x").await;
    assert_eq!(c.rows("SELECT id FROM t1").await, ["1", "2", "3", "4"]);
    assert_eq!(c.rows("SELECT x FROM t3").await, ["1"]);
}
