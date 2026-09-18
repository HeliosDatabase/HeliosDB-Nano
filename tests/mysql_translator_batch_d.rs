//! MySQL→PostgreSQL translator hygiene (sprinter ecc77a75df2b + 948023755a35).
//!
//! Two defects in `src/protocol/mysql/translator.rs`, both pre-existing on
//! `main` and both of them "the translator turns quoted text into code":
//!
//! 1. **ecc77a75df2b (SECURITY).** `translate_backticks` REMOVED every backtick
//!    instead of emitting a PostgreSQL identifier, so a backtick-quoted name
//!    that happens to contain SQL was spliced into the statement as SQL.
//!    `VALUES(` + "`" + `a) ; DROP TABLE x --` + "`" + `)` came back as
//!    `EXCLUDED.a) ; DROP TABLE x --`, and because `execute_dml` splits the
//!    translated text on `;` the tail ran as its own statement. Under real
//!    MySQL the same bytes are one (non-existent) column name and nothing is
//!    dropped, so this is translator-introduced, not app-introduced.
//!
//! 2. **948023755a35.** An apostrophe inside a region that the pipeline left
//!    "open" desynchronised the passes that skip literals: a double-quoted
//!    IDENTIFIER (`"it's_col"`) was scanned through by the escape pass, and a
//!    `/*! … */` executable comment was deliberately left unmasked so
//!    `translate_misc` could strip it — an apostrophe in either one then ate
//!    the next real literal / stranded the following backticks.
//!
//! The wire client at the bottom is the minimal text-protocol MySQL client
//! already used by `tests/mysql_wire_hygiene_batch_d.rs`, trimmed to what the
//! end-to-end injection proof needs.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::protocol::mysql::translator::translate;
use heliosdb_nano::protocol::mysql::MySqlHandler;
use heliosdb_nano::protocol::postgres::timeouts::ConnectionTimeouts;
use heliosdb_nano::EmbeddedDatabase;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

/// Everything in `sql` that is NOT inside a double-quoted identifier.
///
/// The security property both items are really about is "text the client
/// supplied as a NAME must never end up where the parser reads it as SQL", so
/// the assertions below strip the quoted spans and then look for SQL in what is
/// left. `""` inside a quoted identifier is an escaped quote, not a close.
fn outside_double_quotes(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut in_quote = false;
    let mut chars = sql.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '"' {
            if in_quote && chars.peek() == Some(&'"') {
                chars.next(); // `""` — an escaped quote inside the name
                continue;
            }
            in_quote = !in_quote;
            continue;
        }
        if !in_quote {
            out.push(ch);
        }
    }
    out
}

// ===========================================================================
// 1. sprinter ecc77a75df2b — a backtick-quoted name is a NAME, never code
// ===========================================================================

/// The item's own example. On the pre-fix tree the backticks were dropped and
/// the payload became executable SQL:
/// `… ON CONFLICT DO UPDATE SET v = EXCLUDED.a) ; DROP TABLE x --`.
#[test]
fn backticked_name_carrying_sql_is_quoted_not_spliced() {
    let sql = "INSERT INTO t (v) VALUES (1) ON DUPLICATE KEY UPDATE v = VALUES(`a) ; DROP TABLE x --`)";
    let result = translate(sql);

    assert!(
        result.contains(r#"EXCLUDED."a) ; DROP TABLE x --""#),
        "the backticked name must come back as ONE double-quoted identifier: {result}"
    );
    assert!(
        !result.contains("EXCLUDED.a)"),
        "the name must not be spliced bare: {result}"
    );

    let code = outside_double_quotes(&result);
    assert!(
        !code.to_uppercase().contains("DROP"),
        "no part of the name may survive outside the quotes: {result}"
    );
    assert!(
        !code.contains(';'),
        "the statement separator must stay inside the identifier: {result}"
    );
}

/// The same shape in every position an identifier can appear — a select item,
/// a table name, a SET target and an ORDER BY key. None of them may leak a
/// statement separator or a comment introducer into executable position.
#[test]
fn backticked_names_never_leak_sql_in_any_position() {
    // The app splices the attacker's name between backticks. The name itself
    // contains NO backtick — under real MySQL these bytes are one (missing)
    // column name and the statement simply errors, which is what makes the old
    // behaviour a translator-introduced hole rather than an application one.
    let payload = "x ; DROP TABLE victim -- ";

    for sql in [
        format!("SELECT `{payload}` FROM t"),
        format!("SELECT * FROM `{payload}`"),
        format!("UPDATE t SET `{payload}` = 1"),
        format!("SELECT a FROM t ORDER BY `{payload}`"),
        format!("DELETE FROM t WHERE `{payload}` = 1"),
    ] {
        let result = translate(&sql);
        assert!(!result.contains('`'), "no backtick may reach the parser: {result}");
        let code = outside_double_quotes(&result);
        assert!(
            !code.to_uppercase().contains("DROP") && !code.contains(';') && !code.contains("--"),
            "name text escaped the identifier for `{sql}` -> {result}"
        );
    }
}

/// COMPATIBILITY: a plain name still comes out BARE and byte-identical, which
/// is what keeps the WordPress corpus (and PostgreSQL's unquoted case folding)
/// working — `Foo` folds to `foo` exactly as it did before the fix, whereas
/// `"Foo"` would not.
#[test]
fn plain_backtick_identifiers_are_still_emitted_bare() {
    assert_eq!(
        translate("SELECT `id`, `Name` FROM `wp_posts` WHERE `status` = 'publish'"),
        "SELECT id, Name FROM wp_posts WHERE status = 'publish'"
    );
    // `$` and a leading underscore are identifier characters to the PostgreSQL
    // dialect, so those names stay bare too.
    assert_eq!(translate("SELECT `_a$b` FROM `t1`"), "SELECT _a$b FROM t1");
}

/// A name that is NOT a single bare identifier token is double-quoted, with
/// any embedded `"` doubled so the quoting cannot be closed from inside.
#[test]
fn backtick_identifier_that_is_not_a_plain_name_is_double_quoted() {
    assert_eq!(translate("SELECT `my col` FROM t"), r#"SELECT "my col" FROM t"#);
    assert_eq!(translate("SELECT `2col` FROM t"), r#"SELECT "2col" FROM t"#);
    assert_eq!(translate(r#"SELECT `a"b` FROM t"#), r#"SELECT "a""b" FROM t"#);
    assert_eq!(translate("SELECT `it's` FROM t"), r#"SELECT "it's" FROM t"#);
}

/// MySQL escapes a backtick inside a backtick-quoted name by DOUBLING it.
/// That used to be read as two adjacent identifiers whose text was then
/// concatenated (`` `a``b` `` → `ab`); it is one name, `` a`b ``.
#[test]
fn doubled_backtick_is_one_identifier() {
    assert_eq!(translate("SELECT `a``b` FROM t"), "SELECT \"a`b\" FROM t");
    assert!(
        !translate("SELECT `a``b` FROM t").contains("ab"),
        "the two halves must not be concatenated into one bare name"
    );
}

/// An UNTERMINATED backtick is the other splice route: the masker kept the
/// opening delimiter and restored the rest of the input verbatim, and the
/// backtick-stripping pass then dropped that delimiter — so the tail became
/// code. It is now a (broken, but inert) quoted name.
#[test]
fn unterminated_backtick_identifier_is_not_spliced_as_code() {
    let result = translate("SELECT `a ; DROP TABLE x --");
    assert_eq!(result, r#"SELECT "a ; DROP TABLE x --""#);
    let code = outside_double_quotes(&result);
    assert!(!code.contains(';'), "unterminated name must stay inert: {result}");
}

/// END TO END over the MySQL wire: the DML path splits the TRANSLATED text on
/// `;`, so a spliced name used to run a second statement. `victim` is dropped
/// on the pre-fix tree.
#[tokio::test]
async fn backticked_update_target_cannot_run_a_second_statement() {
    let mut c = MySqlTestClient::login(test_db()).await;

    c.ok("CREATE TABLE victim (id INT)").await;
    c.ok("INSERT INTO victim (id) VALUES (1)").await;
    c.ok("CREATE TABLE t (v INT)").await;
    c.ok("INSERT INTO t (v) VALUES (7)").await;

    // The app builds `UPDATE t SET `$col` = 1` from an attacker-supplied $col.
    // Under real MySQL this is one non-existent column name and errors.
    let _ = c.send("UPDATE t SET `v = 1 ; DROP TABLE victim -- ` = 1").await;

    assert_eq!(
        c.scalar("SELECT COUNT(*) FROM victim").await,
        "1",
        "the injected DROP TABLE must never have run"
    );
    assert_eq!(
        c.scalar("SELECT v FROM t").await,
        "7",
        "the injected assignment must not have run either"
    );
}

// ===========================================================================
// 2. sprinter 948023755a35 — apostrophe carriers
// ===========================================================================

/// CARRIER 1. The escape pass only treated `"…"` as a region when its
/// MySQL-string heuristic fired; otherwise it pushed the bare `"` and kept
/// scanning the interior, so the apostrophe in `"it's_col"` opened a phantom
/// literal that "closed" on the next REAL literal's opening quote. The
/// statement came back as `… = 'a = 'b' c'`.
#[test]
fn quoted_identifier_with_apostrophe_does_not_open_a_phantom_string() {
    let sql = r#"SELECT "it's_col" FROM t WHERE note = 'a = "b" c'"#;
    assert_eq!(translate(sql), sql, "identifier and literal must both be untouched");
}

/// The desync only bites once a real literal FOLLOWS the quoted identifier, so
/// pin the round trip with every region kind in one statement: a quoted
/// identifier carrying an apostrophe, a backtick identifier, and two real
/// literals (one of which the double-quote heuristic re-quotes).
#[test]
fn quoted_identifier_apostrophe_then_more_literals_round_trips() {
    let sql = r#"SELECT "it's_col", `c` FROM t WHERE note = 'x' AND other = "y""#;
    assert_eq!(
        translate(sql),
        r#"SELECT "it's_col", c FROM t WHERE note = 'x' AND other = 'y'"#
    );

    // The literal that FOLLOWS the quoted identifier must still be
    // escape-normalised. With the pass mis-paired on the apostrophe, `q\\r`
    // was reached OUTSIDE any literal and its `\\` came back uncollapsed.
    assert_eq!(
        translate(r#"SELECT "it's_col" FROM t WHERE a = 'p' AND b = 'q\\r'"#),
        r#"SELECT "it's_col" FROM t WHERE a = 'p' AND b = 'q\r'"#
    );
}

/// CARRIER 2. `/*! … */` was left UNMASKED so `translate_misc` could strip it,
/// which meant the backtick pass saw its apostrophe, entered literal-skipping
/// mode and ran to end of input — stranding the backticks on `c`.
#[test]
fn executable_comment_with_apostrophe_does_not_strand_backticks() {
    let result = translate("SELECT /*! it's */ `c` FROM t");
    // The comment is stripped in place, leaving the space either side of it.
    assert_eq!(result, "SELECT  c FROM t");
    assert!(!result.contains('`'), "backticks must not survive: {result}");
}

/// The executable comment's SEMANTICS are unchanged: plain `/*! … */` and the
/// version-gated `/*!NNNNN … */` are both stripped WHOLE (body included), which
/// is what the pre-existing `EXEC_COMMENT_RE` did — the stripping only moved
/// earlier in the pipeline so no unmasked text can carry a quote.
#[test]
fn executable_comments_are_still_stripped_whole() {
    assert_eq!(
        translate("SELECT /*!40001 SQL_NO_CACHE */ * FROM t"),
        "SELECT  * FROM t"
    );
    assert_eq!(translate("SELECT /*!50100 STRAIGHT_JOIN */ 1"), "SELECT  1");

    // Quotes and backticks inside the comment are part of the comment.
    let result = translate("SELECT /*!50100 `it's` */ `c` FROM t");
    assert_eq!(result, "SELECT  c FROM t");
    assert!(!result.contains("it's"), "comment body must be gone: {result}");
}

/// An executable comment must not swallow a literal that FOLLOWS it — the
/// failure mode the carrier produced in the other direction.
#[test]
fn executable_comment_then_literal_still_translates() {
    let result = translate("SELECT /*! it's */ 1 FROM t WHERE a = 'b' LIMIT 5, 10");
    assert_eq!(result, "SELECT  1 FROM t WHERE a = 'b' LIMIT 10 OFFSET 5");
}

// ===========================================================================
// 3. The two heuristics the fixes must NOT disturb
// ===========================================================================

/// sprinter 57416d9c: inside a `CREATE TABLE`'s column list a `"` is always a
/// quoted identifier, never a MySQL string literal. This is the translator-level
/// twin of `query_last_serial_id_handles_quoted_identifiers`, which drives the
/// same DDL over the wire.
#[test]
fn create_table_with_a_quoted_column_name_keeps_the_identifier() {
    assert_eq!(
        translate(r#"CREATE TABLE quoted_pk ("Id" INT AUTO_INCREMENT PRIMARY KEY, v INT)"#),
        r#"CREATE TABLE quoted_pk ("Id" SERIAL PRIMARY KEY, v INT)"#
    );
    // …and an apostrophe in such a name is now opaque too.
    assert_eq!(
        translate(r#"CREATE TABLE t ("it's" INT, v INT)"#),
        r#"CREATE TABLE t ("it's" INT, v INT)"#
    );
}

/// MySQL's default `sql_mode` has no `ANSI_QUOTES`, so in VALUE position a
/// `"…"` IS a string literal and must still be re-quoted for PostgreSQL.
#[test]
fn mysql_double_quoted_string_literals_still_become_single_quoted() {
    assert_eq!(
        translate(r#"INSERT INTO t (a) VALUES ("hello")"#),
        "INSERT INTO t (a) VALUES ('hello')"
    );
    assert_eq!(
        translate(r#"SELECT * FROM t WHERE a = "it's""#),
        "SELECT * FROM t WHERE a = 'it''s'"
    );
}

// ------------------------------------------------------------ wire client --

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
            let v = u64::from(u32::from_le_bytes([buf[*pos], buf[*pos + 1], buf[*pos + 2], 0]));
            *pos += 3;
            v
        }
        0xFE => {
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[*pos..*pos + 8]);
            *pos += 8;
            u64::from_le_bytes(b)
        }
        other => u64::from(other),
    }
}

fn read_lenenc_str(buf: &[u8], pos: &mut usize) -> String {
    let len = read_lenenc_u64(buf, pos) as usize;
    let s = String::from_utf8_lossy(&buf[*pos..*pos + len]).to_string();
    *pos += len;
    s
}

struct MySqlTestClient {
    stream: DuplexStream,
}

impl MySqlTestClient {
    async fn login(db: Arc<EmbeddedDatabase>) -> Self {
        let (client, srv) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move {
            let _ = MySqlHandler::handle_connection_with_timeouts(db, srv, 1, ConnectionTimeouts::disabled()).await;
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

    /// COM_QUERY; returns the first response packet. The caller decides whether
    /// OK or ERR is expected — the injection probe accepts either, because the
    /// property under test is what happened to the DATABASE, not what the
    /// client was told.
    async fn send(&mut self, sql: &str) -> Vec<u8> {
        let mut p = vec![COM_QUERY];
        p.extend_from_slice(sql.as_bytes());
        self.write_packet(0, &p).await;
        let (_seq, first) = self.read_packet().await;
        first
    }

    async fn ok(&mut self, sql: &str) {
        let pkt = self.send(sql).await;
        assert_eq!(
            pkt.first().copied(),
            Some(0x00),
            "expected OK for `{sql}`, got {}",
            String::from_utf8_lossy(&pkt)
        );
    }

    async fn query(&mut self, sql: &str) -> Vec<Vec<String>> {
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
        for _ in 0..ncols {
            let (_seq, _def) = self.read_packet().await;
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
        rows
    }

    async fn scalar(&mut self, sql: &str) -> String {
        let rows = self.query(sql).await;
        assert_eq!(rows.len(), 1, "expected one row from `{sql}`, got {rows:?}");
        rows[0][0].clone()
    }
}

fn test_db() -> Arc<EmbeddedDatabase> {
    Arc::new(EmbeddedDatabase::new_in_memory().expect("in-memory db"))
}
