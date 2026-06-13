//! MySQL binary prepared-statement protocol tests (R5.W4 / discovery C6).
//!
//! Drives raw MySQL protocol bytes at the handler level over an in-memory
//! duplex stream (the same pattern as the PG handler's
//! `failed_transaction_state_tests`): full handshake, COM_STMT_PREPARE,
//! COM_STMT_EXECUTE with binary-encoded parameters, and binary result-set
//! decoding.
//!
//! Before this track, COM_STMT_EXECUTE discarded all bound parameters and
//! replayed the raw SQL with `?` placeholders intact — every test here
//! failed at the engine with "Unsupported placeholder format".

use heliosdb_nano::{protocol::mysql::MySqlHandler, EmbeddedDatabase, Value};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

// ---------------------------------------------------------------------------
// Minimal binary-protocol client
// ---------------------------------------------------------------------------

const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;

const COM_STMT_PREPARE: u8 = 0x16;
const COM_STMT_EXECUTE: u8 = 0x17;

const TYPE_TINY: u8 = 0x01;
const TYPE_SHORT: u8 = 0x02;
const TYPE_LONG: u8 = 0x03;
const TYPE_FLOAT: u8 = 0x04;
const TYPE_DOUBLE: u8 = 0x05;
const TYPE_LONGLONG: u8 = 0x08;
const TYPE_DATE: u8 = 0x0a;
const TYPE_DATETIME: u8 = 0x0c;
const TYPE_NEWDECIMAL: u8 = 0xf6;
const TYPE_BLOB: u8 = 0xfc;
const TYPE_VAR_STRING: u8 = 0xfd;

/// One binary-protocol parameter as the client sends it.
enum BinParam {
    Null,
    Tiny(i8),
    Short(i16),
    Long(i32),
    LongLong(i64),
    Float(f32),
    Double(f64),
    Str(&'static str),
    Blob(Vec<u8>),
    Decimal(&'static str),
    /// (year, month, day)
    Date(u16, u8, u8),
    /// (year, month, day, hour, min, sec)
    DateTime(u16, u8, u8, u8, u8, u8),
}

impl BinParam {
    fn type_byte(&self) -> u8 {
        match self {
            // NULL params still carry a type pair; VAR_STRING is what
            // libmysqlclient sends for NULL by default.
            BinParam::Null => TYPE_VAR_STRING,
            BinParam::Tiny(_) => TYPE_TINY,
            BinParam::Short(_) => TYPE_SHORT,
            BinParam::Long(_) => TYPE_LONG,
            BinParam::LongLong(_) => TYPE_LONGLONG,
            BinParam::Float(_) => TYPE_FLOAT,
            BinParam::Double(_) => TYPE_DOUBLE,
            BinParam::Str(_) => TYPE_VAR_STRING,
            BinParam::Blob(_) => TYPE_BLOB,
            BinParam::Decimal(_) => TYPE_NEWDECIMAL,
            BinParam::Date(..) => TYPE_DATE,
            BinParam::DateTime(..) => TYPE_DATETIME,
        }
    }

    fn encode_value(&self, out: &mut Vec<u8>) {
        match self {
            BinParam::Null => {}
            BinParam::Tiny(v) => out.push(*v as u8),
            BinParam::Short(v) => out.extend_from_slice(&v.to_le_bytes()),
            BinParam::Long(v) => out.extend_from_slice(&v.to_le_bytes()),
            BinParam::LongLong(v) => out.extend_from_slice(&v.to_le_bytes()),
            BinParam::Float(v) => out.extend_from_slice(&v.to_le_bytes()),
            BinParam::Double(v) => out.extend_from_slice(&v.to_le_bytes()),
            BinParam::Str(s) => write_lenenc_bytes(out, s.as_bytes()),
            BinParam::Blob(b) => write_lenenc_bytes(out, b),
            BinParam::Decimal(s) => write_lenenc_bytes(out, s.as_bytes()),
            BinParam::Date(y, m, d) => {
                out.push(4);
                out.extend_from_slice(&y.to_le_bytes());
                out.push(*m);
                out.push(*d);
            }
            BinParam::DateTime(y, mo, d, h, mi, s) => {
                out.push(7);
                out.extend_from_slice(&y.to_le_bytes());
                out.push(*mo);
                out.push(*d);
                out.push(*h);
                out.push(*mi);
                out.push(*s);
            }
        }
    }
}

fn write_lenenc_bytes(out: &mut Vec<u8>, b: &[u8]) {
    assert!(b.len() < 251, "test helper only supports short blobs");
    out.push(b.len() as u8);
    out.extend_from_slice(b);
}

fn read_lenenc(buf: &[u8], pos: &mut usize) -> u64 {
    let first = buf[*pos];
    *pos += 1;
    match first {
        0xFC => {
            let v = u16::from_le_bytes([buf[*pos], buf[*pos + 1]]) as u64;
            *pos += 2;
            v
        }
        0xFD => {
            let v = (buf[*pos] as u64) | ((buf[*pos + 1] as u64) << 8) | ((buf[*pos + 2] as u64) << 16);
            *pos += 3;
            v
        }
        0xFE => {
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[*pos..*pos + 8]);
            *pos += 8;
            u64::from_le_bytes(b)
        }
        v => v as u64,
    }
}

fn read_lenenc_slice<'a>(buf: &'a [u8], pos: &mut usize) -> &'a [u8] {
    let len = read_lenenc(buf, pos) as usize;
    let s = &buf[*pos..*pos + len];
    *pos += len;
    s
}

/// A decoded value from a binary result row.
#[derive(Debug, Clone, PartialEq)]
enum WireValue {
    Null,
    Int(i64),
    Float(f64),
    Text(String),
}

struct MySqlTestClient {
    stream: DuplexStream,
}

impl MySqlTestClient {
    /// Spawn the handler over a duplex stream and complete the handshake.
    async fn connect(db: Arc<EmbeddedDatabase>) -> Self {
        let (client, server) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move {
            let _ = MySqlHandler::handle_connection(db, server, 1).await;
        });

        let mut this = Self { stream: client };

        // Server greeting (seq 0)
        let (_seq, _greeting) = this.read_packet().await;

        // HandshakeResponse41 (seq 1): flags, max packet, charset, 23 zeros,
        // user NUL, auth-len 0 (SECURE_CONNECTION framing), no database.
        let mut p = Vec::new();
        p.extend_from_slice(&(CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION).to_le_bytes());
        p.extend_from_slice(&(1u32 << 24).to_le_bytes());
        p.push(45); // utf8mb4_general_ci
        p.extend_from_slice(&[0u8; 23]);
        p.extend_from_slice(b"root\0");
        p.push(0); // empty auth response
        this.write_packet(1, &p).await;

        // OK packet
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

    /// COM_STMT_PREPARE → (stmt_id, num_params). Consumes the param
    /// definition packets and trailing EOF the server sends for
    /// num_params > 0 (we do not set CLIENT_DEPRECATE_EOF).
    async fn prepare(&mut self, sql: &str) -> (u32, u16) {
        let mut p = vec![COM_STMT_PREPARE];
        p.extend_from_slice(sql.as_bytes());
        self.write_packet(0, &p).await;

        let (_seq, ok) = self.read_packet().await;
        assert_eq!(
            ok[0],
            0x00,
            "COM_STMT_PREPARE failed: {:?}",
            String::from_utf8_lossy(&ok)
        );
        let stmt_id = u32::from_le_bytes([ok[1], ok[2], ok[3], ok[4]]);
        let _num_columns = u16::from_le_bytes([ok[5], ok[6]]);
        let num_params = u16::from_le_bytes([ok[7], ok[8]]);

        // Param definition packets + EOF
        if num_params > 0 {
            for _ in 0..num_params {
                let _ = self.read_packet().await;
            }
            let (_s, eof) = self.read_packet().await;
            assert_eq!(eof[0], 0xFE, "expected EOF after param defs");
        }
        (stmt_id, num_params)
    }

    /// COM_STMT_EXECUTE with binary parameters; returns the raw first
    /// response packet plus the rest of the result (if any) pre-read.
    async fn execute_raw(&mut self, stmt_id: u32, params: &[BinParam], new_params_bound: bool) -> Vec<u8> {
        let mut p = vec![COM_STMT_EXECUTE];
        p.extend_from_slice(&stmt_id.to_le_bytes());
        p.push(0); // flags: CURSOR_TYPE_NO_CURSOR
        p.extend_from_slice(&1u32.to_le_bytes()); // iteration count

        if !params.is_empty() {
            let bitmap_len = (params.len() + 7) / 8;
            let mut bitmap = vec![0u8; bitmap_len];
            for (i, param) in params.iter().enumerate() {
                if matches!(param, BinParam::Null) {
                    bitmap[i / 8] |= 1 << (i % 8);
                }
            }
            p.extend_from_slice(&bitmap);
            p.push(u8::from(new_params_bound));
            if new_params_bound {
                for param in params {
                    p.push(param.type_byte());
                    p.push(0); // flags (signed)
                }
            }
            for param in params {
                if !matches!(param, BinParam::Null) {
                    param.encode_value(&mut p);
                }
            }
        }

        self.write_packet(0, &p).await;
        let (_seq, first) = self.read_packet().await;
        first
    }

    /// Execute expecting an OK packet; returns affected row count.
    async fn execute_ok(&mut self, stmt_id: u32, params: &[BinParam]) -> u64 {
        let first = self.execute_raw(stmt_id, params, true).await;
        assert_eq!(
            first[0],
            0x00,
            "expected OK, got: {:?}",
            String::from_utf8_lossy(&first)
        );
        let mut pos = 1;
        read_lenenc(&first, &mut pos) // affected rows
    }

    /// Execute expecting a binary result set; returns (columns, rows).
    async fn execute_query(&mut self, stmt_id: u32, params: &[BinParam]) -> (Vec<String>, Vec<Vec<WireValue>>) {
        self.execute_query_inner(stmt_id, params, true).await
    }

    async fn execute_query_inner(
        &mut self,
        stmt_id: u32,
        params: &[BinParam],
        new_params_bound: bool,
    ) -> (Vec<String>, Vec<Vec<WireValue>>) {
        let first = self.execute_raw(stmt_id, params, new_params_bound).await;
        assert_ne!(first[0], 0xFF, "server error: {:?}", String::from_utf8_lossy(&first));
        assert_ne!(first[0], 0x00, "expected result set, got OK packet");

        let mut pos = 0;
        let ncols = read_lenenc(&first, &mut pos) as usize;

        // Column definitions
        let mut names = Vec::with_capacity(ncols);
        let mut types = Vec::with_capacity(ncols);
        for _ in 0..ncols {
            let (_s, def) = self.read_packet().await;
            let mut dp = 0;
            let _catalog = read_lenenc_slice(&def, &mut dp);
            let _schema = read_lenenc_slice(&def, &mut dp);
            let _table = read_lenenc_slice(&def, &mut dp);
            let _org_table = read_lenenc_slice(&def, &mut dp);
            let name = String::from_utf8(read_lenenc_slice(&def, &mut dp).to_vec()).unwrap();
            let _org_name = read_lenenc_slice(&def, &mut dp);
            let _fixed_len = read_lenenc(&def, &mut dp); // 0x0c
            dp += 2 + 4; // charset + column length
            let col_type = def[dp];
            names.push(name);
            types.push(col_type);
        }

        // EOF after defs
        let (_s, eof) = self.read_packet().await;
        assert_eq!(eof[0], 0xFE, "expected EOF after column defs");

        // Binary rows until EOF
        let mut rows = Vec::new();
        loop {
            let (_s, pkt) = self.read_packet().await;
            if pkt[0] == 0xFE && pkt.len() < 9 {
                break;
            }
            assert_eq!(pkt[0], 0x00, "binary row must start with 0x00");
            let bitmap_len = (ncols + 7 + 2) / 8;
            let bitmap = &pkt[1..1 + bitmap_len];
            let mut vp = 1 + bitmap_len;
            let mut row = Vec::with_capacity(ncols);
            for (i, &ty) in types.iter().enumerate() {
                let pos_in_bitmap = i + 2;
                let is_null = bitmap[pos_in_bitmap / 8] & (1 << (pos_in_bitmap % 8)) != 0;
                if is_null {
                    row.push(WireValue::Null);
                    continue;
                }
                row.push(match ty {
                    TYPE_TINY => {
                        let v = pkt[vp] as i8;
                        vp += 1;
                        WireValue::Int(v as i64)
                    }
                    TYPE_SHORT => {
                        let v = i16::from_le_bytes([pkt[vp], pkt[vp + 1]]);
                        vp += 2;
                        WireValue::Int(v as i64)
                    }
                    TYPE_LONG => {
                        let v = i32::from_le_bytes([pkt[vp], pkt[vp + 1], pkt[vp + 2], pkt[vp + 3]]);
                        vp += 4;
                        WireValue::Int(v as i64)
                    }
                    TYPE_LONGLONG => {
                        let mut b = [0u8; 8];
                        b.copy_from_slice(&pkt[vp..vp + 8]);
                        vp += 8;
                        WireValue::Int(i64::from_le_bytes(b))
                    }
                    TYPE_FLOAT => {
                        let v = f32::from_le_bytes([pkt[vp], pkt[vp + 1], pkt[vp + 2], pkt[vp + 3]]);
                        vp += 4;
                        WireValue::Float(v as f64)
                    }
                    TYPE_DOUBLE => {
                        let mut b = [0u8; 8];
                        b.copy_from_slice(&pkt[vp..vp + 8]);
                        vp += 8;
                        WireValue::Float(f64::from_le_bytes(b))
                    }
                    TYPE_DATE | TYPE_DATETIME | 0x07 => {
                        let len = pkt[vp] as usize;
                        let body = &pkt[vp + 1..vp + 1 + len];
                        vp += 1 + len;
                        let text = if len >= 4 {
                            let y = u16::from_le_bytes([body[0], body[1]]);
                            let mut s = format!("{:04}-{:02}-{:02}", y, body[2], body[3]);
                            if len >= 7 {
                                s.push_str(&format!(" {:02}:{:02}:{:02}", body[4], body[5], body[6]));
                            }
                            s
                        } else {
                            String::new()
                        };
                        WireValue::Text(text)
                    }
                    _ => {
                        // Length-encoded string family
                        let s = read_lenenc_slice(&pkt, &mut vp);
                        WireValue::Text(String::from_utf8_lossy(s).into_owned())
                    }
                });
            }
            rows.push(row);
        }

        (names, rows)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

fn test_db() -> Arc<EmbeddedDatabase> {
    Arc::new(EmbeddedDatabase::new_in_memory().expect("in-memory db"))
}

#[tokio::test]
async fn prepare_reports_quote_aware_param_count() {
    let db = test_db();
    db.execute("CREATE TABLE q (id INT PRIMARY KEY, name TEXT)").unwrap();
    let mut client = MySqlTestClient::connect(db).await;

    // '?' inside the string literal must NOT count as a parameter
    let (_id, n) = client.prepare("INSERT INTO q (id, name) VALUES (?, 'what?')").await;
    assert_eq!(n, 1, "placeholder inside string literal was counted");

    let (_id, n) = client.prepare("SELECT * FROM q WHERE id = ? AND name = ?").await;
    assert_eq!(n, 2);
}

#[tokio::test]
async fn insert_int_string_null_roundtrip() {
    let db = test_db();
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, score BIGINT)")
        .unwrap();
    let mut client = MySqlTestClient::connect(Arc::clone(&db)).await;

    let (stmt, n) = client
        .prepare("INSERT INTO users (id, name, score) VALUES (?, ?, ?)")
        .await;
    assert_eq!(n, 3);

    let affected = client
        .execute_ok(stmt, &[BinParam::Long(42), BinParam::Str("alice"), BinParam::Null])
        .await;
    assert_eq!(affected, 1);

    // Verify through the embedded API that the bound values landed —
    // before W4 this row would have been the literal text "?,?,?" failure.
    let rows = db.query("SELECT id, name, score FROM users", &[]).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int4(42));
    assert_eq!(rows[0].values[1], Value::String("alice".into()));
    assert_eq!(rows[0].values[2], Value::Null);
}

#[tokio::test]
async fn select_with_params_returns_binary_rows() {
    let db = test_db();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT, score BIGINT)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 'alpha', 10), (2, 'beta', 20), (3, 'gamma', NULL)")
        .unwrap();
    let mut client = MySqlTestClient::connect(db).await;

    let (stmt, _) = client
        .prepare("SELECT id, name, score FROM t WHERE id > ? ORDER BY id")
        .await;
    let (cols, rows) = client.execute_query(stmt, &[BinParam::Long(1)]).await;

    assert_eq!(cols, vec!["id", "name", "score"]);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], WireValue::Int(2));
    assert_eq!(rows[0][1], WireValue::Text("beta".into()));
    assert_eq!(rows[0][2], WireValue::Int(20));
    assert_eq!(rows[1][0], WireValue::Int(3));
    assert_eq!(rows[1][1], WireValue::Text("gamma".into()));
    assert_eq!(rows[1][2], WireValue::Null, "NULL must come back via the row bitmap");
}

#[tokio::test]
async fn string_param_filters_rows() {
    let db = test_db();
    db.execute("CREATE TABLE s (id INT PRIMARY KEY, name TEXT)").unwrap();
    db.execute("INSERT INTO s VALUES (1, 'apple'), (2, 'banana')").unwrap();
    let mut client = MySqlTestClient::connect(db).await;

    let (stmt, _) = client.prepare("SELECT id FROM s WHERE name = ?").await;
    let (_cols, rows) = client.execute_query(stmt, &[BinParam::Str("banana")]).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], WireValue::Int(2));
}

#[tokio::test]
async fn all_numeric_widths_decode() {
    let db = test_db();
    db.execute(
        "CREATE TABLE nums (id INT PRIMARY KEY, t SMALLINT, s SMALLINT, l INT, \
         ll BIGINT, f REAL, d DOUBLE PRECISION, dec NUMERIC)",
    )
    .unwrap();
    let mut client = MySqlTestClient::connect(Arc::clone(&db)).await;

    let (stmt, n) = client.prepare("INSERT INTO nums VALUES (?, ?, ?, ?, ?, ?, ?, ?)").await;
    assert_eq!(n, 8);

    let affected = client
        .execute_ok(
            stmt,
            &[
                BinParam::Long(1),
                BinParam::Tiny(-5),
                BinParam::Short(-1234),
                BinParam::Long(123_456),
                BinParam::LongLong(9_876_543_210),
                BinParam::Float(1.5),
                BinParam::Double(2.25),
                BinParam::Decimal("123.45"),
            ],
        )
        .await;
    assert_eq!(affected, 1);

    let rows = db.query("SELECT t, s, l, ll, f, d, dec FROM nums", &[]).unwrap();
    assert_eq!(rows.len(), 1);
    let v = &rows[0].values;
    assert_eq!(v[0], Value::Int2(-5));
    assert_eq!(v[1], Value::Int2(-1234));
    assert_eq!(v[2], Value::Int4(123_456));
    assert_eq!(v[3], Value::Int8(9_876_543_210));
    assert_eq!(v[4], Value::Float4(1.5));
    assert_eq!(v[5], Value::Float8(2.25));
    // NUMERIC may surface as Numeric or Float8 depending on column coercion
    match &v[6] {
        Value::Numeric(s) => assert!(s.starts_with("123.45")),
        Value::Float8(f) => assert!((f - 123.45).abs() < 1e-9),
        other => panic!("unexpected NUMERIC value: {other:?}"),
    }
}

#[tokio::test]
async fn date_and_datetime_params_decode() {
    let db = test_db();
    db.execute("CREATE TABLE dt (id INT PRIMARY KEY, d DATE, ts TIMESTAMP)")
        .unwrap();
    let mut client = MySqlTestClient::connect(Arc::clone(&db)).await;

    let (stmt, _) = client.prepare("INSERT INTO dt VALUES (?, ?, ?)").await;
    let affected = client
        .execute_ok(
            stmt,
            &[
                BinParam::Long(1),
                BinParam::Date(2024, 3, 15),
                BinParam::DateTime(2024, 3, 15, 10, 30, 45),
            ],
        )
        .await;
    assert_eq!(affected, 1);

    let rows = db.query("SELECT d, ts FROM dt", &[]).unwrap();
    assert_eq!(rows.len(), 1);
    match &rows[0].values[0] {
        Value::Date(d) => assert_eq!(d.to_string(), "2024-03-15"),
        other => panic!("expected Date, got {other:?}"),
    }
    match &rows[0].values[1] {
        Value::Timestamp(ts) => assert!(ts.to_string().starts_with("2024-03-15 10:30:45")),
        other => panic!("expected Timestamp, got {other:?}"),
    }
}

#[tokio::test]
async fn blob_param_roundtrips_as_text_when_utf8() {
    let db = test_db();
    db.execute("CREATE TABLE b (id INT PRIMARY KEY, payload TEXT)").unwrap();
    let mut client = MySqlTestClient::connect(Arc::clone(&db)).await;

    // Client libraries routinely tag string params as BLOB
    let (stmt, _) = client.prepare("INSERT INTO b VALUES (?, ?)").await;
    client
        .execute_ok(stmt, &[BinParam::Long(7), BinParam::Blob(b"hello blob".to_vec())])
        .await;

    let rows = db.query("SELECT payload FROM b WHERE id = 7", &[]).unwrap();
    assert_eq!(rows[0].values[0], Value::String("hello blob".into()));
}

#[tokio::test]
async fn repeated_execute_reuses_cached_param_types() {
    let db = test_db();
    db.execute("CREATE TABLE r (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO r VALUES (1, 100), (2, 200), (3, 300)").unwrap();
    let mut client = MySqlTestClient::connect(db).await;

    let (stmt, _) = client.prepare("SELECT v FROM r WHERE id = ?").await;

    // First execute: new-params-bound = 1 (types sent)
    let (_c, rows) = client.execute_query_inner(stmt, &[BinParam::Long(1)], true).await;
    assert_eq!(rows[0][0], WireValue::Int(100));

    // Second execute: new-params-bound = 0 (types omitted — server must
    // reuse the cached type pairs from the first execute)
    let (_c, rows) = client.execute_query_inner(stmt, &[BinParam::Long(3)], false).await;
    assert_eq!(rows[0][0], WireValue::Int(300));
}

#[tokio::test]
async fn update_and_delete_with_params() {
    let db = test_db();
    db.execute("CREATE TABLE ud (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO ud VALUES (1, 'a'), (2, 'b'), (3, 'c')")
        .unwrap();
    let mut client = MySqlTestClient::connect(Arc::clone(&db)).await;

    let (upd, _) = client.prepare("UPDATE ud SET v = ? WHERE id = ?").await;
    let affected = client
        .execute_ok(upd, &[BinParam::Str("updated"), BinParam::Long(2)])
        .await;
    assert_eq!(affected, 1);

    let (del, _) = client.prepare("DELETE FROM ud WHERE id = ?").await;
    let affected = client.execute_ok(del, &[BinParam::Long(3)]).await;
    assert_eq!(affected, 1);

    let rows = db.query("SELECT v FROM ud WHERE id = 2", &[]).unwrap();
    assert_eq!(rows[0].values[0], Value::String("updated".into()));
    assert_eq!(db.query("SELECT id FROM ud", &[]).unwrap().len(), 2);
}

#[tokio::test]
async fn zero_param_prepared_select_still_works() {
    let db = test_db();
    db.execute("CREATE TABLE z (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO z VALUES (11)").unwrap();
    let mut client = MySqlTestClient::connect(db).await;

    let (stmt, n) = client.prepare("SELECT id FROM z").await;
    assert_eq!(n, 0);
    let (cols, rows) = client.execute_query(stmt, &[]).await;
    assert_eq!(cols, vec!["id"]);
    assert_eq!(rows, vec![vec![WireValue::Int(11)]]);
}
