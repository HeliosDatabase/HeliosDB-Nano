//! MySQL wire: a statement is never answered because its TEXT merely contains
//! `FOUND_ROWS()`, `LAST_INSERT_ID()` or `VERSION()` (plan brief v02.1).
//!
//! The listener used to answer any statement whose upper-cased text CONTAINED
//! one of those calls with a canned one-row result, without running it: an
//! `INSERT` whose value merely mentioned `VERSION()` came back as a result set
//! and the row was never stored. Now only a statement that is exactly the call
//! (optionally aliased) is answered from the session; in every other statement
//! the calls in its code are replaced by the session value and the statement
//! runs, and text inside literals or comments is left alone.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::protocol::mysql::MySqlHandler;
use heliosdb_nano::protocol::postgres::timeouts::ConnectionTimeouts;
use heliosdb_nano::EmbeddedDatabase;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

const COM_QUERY: u8 = 0x03;
const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;

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
}

fn test_db() -> Arc<EmbeddedDatabase> {
    Arc::new(EmbeddedDatabase::new_in_memory().expect("in-memory db"))
}

async fn setup() -> (Arc<EmbeddedDatabase>, MySqlTestClient) {
    let db = test_db();
    let mut c = MySqlTestClient::login(Arc::clone(&db)).await;
    c.ok("CREATE TABLE notes (id INT AUTO_INCREMENT PRIMARY KEY, body TEXT)")
        .await;
    (db, c)
}

fn stored(db: &EmbeddedDatabase) -> Vec<String> {
    db.query("SELECT body FROM notes ORDER BY id", &[])
        .unwrap()
        .iter()
        .map(|t| format!("{:?}", t.values[0]))
        .collect()
}

/// The reported defect: four INSERTs whose literal text mentions a session
/// function each answered with a result set and stored nothing.
#[tokio::test]
async fn insert_whose_text_mentions_session_functions_is_stored() {
    let (db, mut c) = setup().await;
    for body in [
        "FOUND_ROWS()",
        "LAST_INSERT_ID()",
        "VERSION()",
        "see information_schema.tables",
    ] {
        let (affected, _id) = c
            .ok_insert(&format!("INSERT INTO notes (body) VALUES ('{body}')"))
            .await;
        assert_eq!(affected, 1, "INSERT mentioning {body} must run");
    }
    let rows = stored(&db);
    assert_eq!(rows.len(), 4, "{rows:?}");
    assert!(
        rows[0].contains("FOUND_ROWS()") && rows[2].contains("VERSION()"),
        "{rows:?}"
    );
}

/// A comment that mentions a call does not make the statement a session query.
#[tokio::test]
async fn comment_mentioning_a_session_function_does_not_intercept() {
    let (db, mut c) = setup().await;
    let (affected, _) = c
        .ok_insert("INSERT INTO notes (body) VALUES ('x') /* VERSION() */")
        .await;
    assert_eq!(affected, 1);
    assert_eq!(stored(&db).len(), 1);
}

#[tokio::test]
async fn select_filtering_on_a_literal_version_call_reaches_the_engine() {
    let (_db, mut c) = setup().await;
    c.ok("INSERT INTO notes (body) VALUES ('VERSION()')").await;
    c.ok("INSERT INTO notes (body) VALUES ('other')").await;
    let (cols, rows) = c.query("SELECT id, body FROM notes WHERE body = 'VERSION()'").await;
    assert_eq!(cols, ["id", "body"]);
    assert_eq!(rows, vec![vec!["1".to_string(), "VERSION()".to_string()]]);
    let (_c, rows) = c.query("SELECT body FROM notes WHERE body LIKE '%FOUND_ROWS()%'").await;
    assert!(rows.is_empty(), "{rows:?}");
}

/// A call in CODE inside a larger statement is evaluated, not canned.
#[tokio::test]
async fn insert_values_last_insert_id_uses_the_session_value() {
    let (db, mut c) = setup().await;
    c.ok("CREATE TABLE child (id INT AUTO_INCREMENT PRIMARY KEY, parent_id BIGINT)")
        .await;
    let (_a, id) = c.ok_insert("INSERT INTO notes (body) VALUES ('parent')").await;
    assert_eq!(id, 1);
    let (affected, _) = c
        .ok_insert("INSERT INTO child (parent_id) VALUES (LAST_INSERT_ID())")
        .await;
    assert_eq!(affected, 1);
    let rows = db.query("SELECT parent_id FROM child", &[]).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(format!("{:?}", rows[0].values[0]).contains('1'), true, "{rows:?}");
}

#[tokio::test]
async fn select_with_a_session_call_among_other_items_runs_the_statement() {
    let (_db, mut c) = setup().await;
    c.ok("INSERT INTO notes (body) VALUES ('a')").await;
    let (cols, rows) = c.query("SELECT body, LAST_INSERT_ID() AS lid FROM notes").await;
    assert_eq!(cols.len(), 2, "{cols:?}");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], "a");
    assert_eq!(rows[0][1], "1");
    let (_c, rows) = c.query("SELECT body FROM notes WHERE id = LAST_INSERT_ID()").await;
    assert_eq!(rows, vec![vec!["a".to_string()]]);
}

/// What was intercepted correctly before still is.
#[tokio::test]
async fn standalone_session_selects_answer_as_before() {
    let (_db, mut c) = setup().await;
    c.ok("INSERT INTO notes (body) VALUES ('a')").await;
    let (cols, rows) = c.query("SELECT LAST_INSERT_ID()").await;
    assert_eq!((cols[0].as_str(), rows[0][0].as_str()), ("LAST_INSERT_ID()", "1"));
    let (cols, rows) = c.query("select version();").await;
    assert_eq!(cols[0], "VERSION()");
    assert!(rows[0][0].starts_with("8.0"), "{rows:?}");
    let (cols, _rows) = c.query("SELECT FOUND_ROWS()").await;
    assert_eq!(cols[0], "FOUND_ROWS()");
    let (cols, rows) = c.query("SELECT VERSION() AS v").await;
    assert_eq!(cols[0], "v");
    assert!(rows[0][0].starts_with("8.0"));
    let (cols, _r) = c.query("SELECT LAST_INSERT_ID() `lid`").await;
    assert_eq!(cols[0], "lid");
    assert!(c.scalar("SELECT @@version").await.starts_with("8.0"));
}

#[tokio::test]
async fn select_with_double_at_in_a_literal_reaches_the_engine() {
    let (_db, mut c) = setup().await;
    c.ok("INSERT INTO notes (body) VALUES ('50%@@host')").await;
    c.ok("INSERT INTO notes (body) VALUES ('plain')").await;
    let (_c, rows) = c.query("SELECT body FROM notes WHERE body LIKE '%@@%'").await;
    assert_eq!(rows, vec![vec!["50%@@host".to_string()]]);
}

#[tokio::test]
async fn write_mentioning_information_schema_in_a_literal_is_an_ok_packet() {
    let (db, mut c) = setup().await;
    let (affected, _) = c
        .ok_insert("INSERT INTO notes (body) VALUES ('from information_schema.columns')")
        .await;
    assert_eq!(affected, 1);
    assert_eq!(stored(&db).len(), 1);
}
