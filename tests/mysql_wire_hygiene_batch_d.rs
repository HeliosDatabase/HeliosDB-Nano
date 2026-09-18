//! MySQL wire protocol hygiene (sprinter 35c74e86 + 57416d9c).
//!
//! Three reporting / state bugs on the MySQL listener, all of them silent —
//! the client is told something plausible and wrong rather than being given an
//! error:
//!
//! 1. **`wait_timeout` / `interactive_timeout` were hardcoded.** The listener
//!    answered MySQL's stock `28800` (8 h) and `30` no matter what
//!    `idle_session_timeout` the server was actually enforcing, and
//!    `SHOW VARIABLES` did not list them at all. A pool that sizes its idle
//!    recycling from `wait_timeout` believed it had eight hours while the
//!    server was closing the connection after 45 s; the only symptom is a
//!    pooled connection that comes back dead.
//!
//! 2. **`COM_RESET_CONNECTION` must end the ENGINE transaction**, not only the
//!    handler's `in_transaction` flag. That half was already fixed (HDB-008)
//!    and is pinned here so it cannot regress: a reset mid-transaction rolls
//!    the staged rows back AND leaves the reused connection able to open a
//!    fresh block.
//!
//! 3. **`query_last_serial_id` spliced identifiers raw.** The `SELECT MAX(pk)
//!    FROM t` probe that backs `LAST_INSERT_ID()` runs after every INSERT and
//!    swallows its own error, so a table or PK whose name needs quoting — a
//!    reserved word, a case-preserving quoted name — reported
//!    `LAST_INSERT_ID() = 0` forever.
//!
//! The client below is the minimal text-protocol MySQL client already used by
//! `tests/security_hdb_008.rs` and `tests/mysql_stmt_execute_tests.rs`, plus
//! result-set and `COM_RESET_CONNECTION` decoding.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::protocol::mysql::MySqlHandler;
use heliosdb_nano::protocol::postgres::timeouts::ConnectionTimeouts;
use heliosdb_nano::EmbeddedDatabase;
use std::sync::Arc;
use std::time::Duration;
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

/// A GH#28 listener policy whose ONLY armed budget is `idle_session_timeout`.
fn idle_policy(idle: Duration) -> ConnectionTimeouts {
    let mut timeouts = ConnectionTimeouts::disabled();
    timeouts.idle_session_timeout = idle;
    timeouts
}

// ===========================================================================
// 1. sprinter 35c74e86 — the timeout variables report the CONFIGURED budget
// ===========================================================================

/// `wait_timeout` / `interactive_timeout` are what a connection pool sizes its
/// idle recycling from. They used to be the constant `28800` (and
/// `net_read_timeout` / `net_write_timeout` the constant `30`) whatever the
/// listener was enforcing, and `SHOW VARIABLES` did not list any of them at
/// all — so a client that probed them that way got an EMPTY result set.
#[tokio::test]
async fn show_variables_reports_the_configured_idle_timeout() {
    let policy = idle_policy(Duration::from_secs(45));
    let mut c = MySqlTestClient::login_with_timeouts(test_db(), policy).await;

    // The `SHOW VARIABLES` surface, which used to return nothing.
    let (name, value) = c.one_variable("wait_timeout").await;
    assert_eq!(name, "wait_timeout");
    assert_eq!(value, "45", "wait_timeout must report the configured budget");

    let (name, value) = c.one_variable("interactive_timeout").await;
    assert_eq!(name, "interactive_timeout");
    assert_eq!(value, "45");

    // …and the `@@variable` surface, which used to answer 28800 / 30.
    for var in [
        "wait_timeout",
        "interactive_timeout",
        "net_read_timeout",
        "net_write_timeout",
    ] {
        let value = c.scalar(&format!("SELECT @@{var}")).await;
        assert_eq!(value, "45", "@@{var} must report the configured budget");
    }

    // One `LIKE '%timeout%'` lists all four, and not one of them is a stock
    // MySQL default any more.
    let (_cols, rows) = c.query("SHOW VARIABLES LIKE '%timeout%'").await;
    let mut listed: Vec<String> = rows.iter().map(|r| format!("{}={}", r[0], r[1])).collect();
    listed.sort();
    assert_eq!(
        listed.join(" "),
        "interactive_timeout=45 net_read_timeout=45 net_write_timeout=45 wait_timeout=45"
    );
}

/// A sub-second budget rounds UP to MySQL's minimum of `1` rather than down to
/// `0`, which is out of range and would read as "no timeout".
#[tokio::test]
async fn show_variables_rounds_a_sub_second_idle_timeout_up_to_one() {
    let policy = idle_policy(Duration::from_millis(500));
    let mut c = MySqlTestClient::login_with_timeouts(test_db(), policy).await;

    assert_eq!(c.scalar("SELECT @@wait_timeout").await, "1");
}

/// A listener with NO idle budget reports MySQL's own "effectively never"
/// value — the documented maximum — not the stock `28800`, which would tell a
/// pool to recycle connections this server never closes.
#[tokio::test]
async fn show_variables_reports_no_timeout_when_the_idle_budget_is_disabled() {
    let mut c = MySqlTestClient::login(test_db()).await;

    let value = c.scalar("SELECT @@wait_timeout").await;
    assert_ne!(value, "28800", "a disabled budget must not report the stock default");
    assert_eq!(value, NEVER);

    let (name, value) = c.one_variable("net_write_timeout").await;
    assert_eq!(name, "net_write_timeout");
    assert_eq!(value, NEVER);
}

// ===========================================================================
// 2. sprinter 57416d9c — COM_RESET_CONNECTION ends the ENGINE transaction
// ===========================================================================

/// `COM_RESET_CONNECTION` is what a pool issues before handing a connection to
/// the next borrower. Clearing only the handler's `in_transaction` flag left
/// the ENGINE session's transaction open: the next `BEGIN` on that connection
/// was refused, and the previous borrower's staged rows were still live.
///
/// This is the already-fixed half of the item (HDB-008); the test is the
/// regression pin, and it fails against a flag-only reset in both directions —
/// the reused connection can open a fresh block, and the rows the reset threw
/// away really are gone.
#[tokio::test]
async fn com_reset_connection_does_not_leak_an_open_transaction() {
    let mut c = MySqlTestClient::login(test_db()).await;

    c.ok("CREATE TABLE reset_probe (id INT PRIMARY KEY)").await;
    let status = c.ok("BEGIN").await;
    assert_eq!(
        status & SERVER_STATUS_IN_TRANS,
        SERVER_STATUS_IN_TRANS,
        "BEGIN must report SERVER_STATUS_IN_TRANS"
    );
    c.ok("INSERT INTO reset_probe VALUES (1)").await;

    let status = c.reset_connection().await;
    assert_eq!(
        status & SERVER_STATUS_IN_TRANS,
        0,
        "COM_RESET_CONNECTION must clear the in-transaction status"
    );

    // The reused connection must be able to open a FRESH block — it cannot if
    // the engine session is still holding the previous one.
    let status = c.ok("BEGIN").await;
    assert_eq!(
        status & SERVER_STATUS_IN_TRANS,
        SERVER_STATUS_IN_TRANS,
        "BEGIN after a reset must open a new transaction, not fail"
    );
    c.ok("INSERT INTO reset_probe VALUES (2)").await;
    let status = c.ok("COMMIT").await;
    assert_eq!(status & SERVER_STATUS_IN_TRANS, 0, "COMMIT must end the block");

    // …and the pre-reset row must have been rolled back by the ENGINE.
    let (_cols, rows) = c.query("SELECT id FROM reset_probe ORDER BY id").await;
    assert_eq!(rows.len(), 1, "the pre-reset row must be gone: {rows:?}");
    assert_eq!(rows[0][0], "2");
}

// ===========================================================================
// 3. sprinter 57416d9c — LAST_INSERT_ID() survives identifiers needing quotes
// ===========================================================================

/// The `SELECT MAX(pk) FROM t` probe behind `LAST_INSERT_ID()` used to splice
/// both identifiers in RAW and swallow the resulting error, so every insert
/// into a table (or onto a PK) whose name needs quoting reported `0`.
///
/// Two shapes, both quoted on the way in and therefore both quoted in the
/// catalog: a reserved word as the TABLE name, and a case-preserving PK COLUMN
/// name. Raw splicing turns the first into a parse error (`… FROM select`) and
/// the second into an unknown column (`MAX(Id)` folds to `id`).
#[tokio::test]
async fn query_last_serial_id_handles_quoted_identifiers() {
    let mut c = MySqlTestClient::login(test_db()).await;

    // --- a reserved word as the table name --------------------------------
    c.ok(r#"CREATE TABLE "select" (id INT AUTO_INCREMENT PRIMARY KEY, v INT)"#)
        .await;

    let (affected, last_id) = c.ok_insert(r#"INSERT INTO "select" (v) VALUES (10)"#).await;
    assert_eq!(affected, 1);
    assert_eq!(last_id, 1, "a reserved-word table must still report its new id");
    assert_eq!(c.scalar("SELECT LAST_INSERT_ID()").await, "1");

    let (_affected, last_id) = c.ok_insert(r#"INSERT INTO "select" (v) VALUES (20)"#).await;
    assert_eq!(last_id, 2, "the probe must keep tracking the sequence");
    assert_eq!(c.scalar("SELECT LAST_INSERT_ID()").await, "2");

    // --- a case-preserving PK column name ---------------------------------
    c.ok(r#"CREATE TABLE quoted_pk ("Id" INT AUTO_INCREMENT PRIMARY KEY, v INT)"#)
        .await;

    let (affected, last_id) = c.ok_insert("INSERT INTO quoted_pk (v) VALUES (7)").await;
    assert_eq!(affected, 1);
    assert_eq!(last_id, 1, "the PK column must be quoted in the probe too");
    assert_eq!(c.scalar("SELECT LAST_INSERT_ID()").await, "1");
}

/// The swallow-to-0 behaviour that is NOT a bug stays: a plain unquoted table
/// keeps reporting its generated id, and a table with no primary key has
/// nothing to probe and still answers `0` instead of erroring.
#[tokio::test]
async fn query_last_serial_id_keeps_reporting_plain_tables() {
    let mut c = MySqlTestClient::login(test_db()).await;

    c.ok("CREATE TABLE plain (id INT AUTO_INCREMENT PRIMARY KEY, v INT)")
        .await;
    let (_affected, last_id) = c.ok_insert("INSERT INTO plain (v) VALUES (1)").await;
    assert_eq!(last_id, 1);

    c.ok("CREATE TABLE no_pk (v INT)").await;
    let (affected, last_id) = c.ok_insert("INSERT INTO no_pk (v) VALUES (5)").await;
    assert_eq!(affected, 1);
    assert_eq!(last_id, 0, "a table with no primary key has no id to report");
}
