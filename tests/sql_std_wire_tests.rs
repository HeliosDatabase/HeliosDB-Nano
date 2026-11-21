#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::many_single_char_names)]
//! `MERGE INTO` and SQL cursors as a PostgreSQL client sees them: the command
//! tags, the row description of a `FETCH` and of `MERGE … RETURNING`, the
//! SQLSTATE of each refusal, the simple and the extended protocol, and one
//! cursor name in two connections. And what `RAISE` in a routine sends: the
//! SQLSTATE, message, detail and hint of an exception, and a notice.

use bytes::{BufMut, BytesMut};
use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::EmbeddedDatabase;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_postgres::{Client, NoTls};

const WAIT: Duration = Duration::from_secs(20);

async fn serve(db: Arc<EmbeddedDatabase>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("address");
    drop(listener);
    let server = PgServer::new(PgServerConfig::with_address(addr), db).expect("server");
    let handle = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    (addr, handle)
}

async fn driver(addr: SocketAddr) -> Client {
    let conn = format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port());
    let (client, connection) = timeout(WAIT, tokio_postgres::connect(&conn, NoTls))
        .await
        .expect("connect in time")
        .expect("connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// A client that speaks the protocol by hand, so the test sees every message.
struct Raw {
    stream: TcpStream,
}

type Frame = (u8, Vec<u8>);

fn cstr(buf: &mut BytesMut, s: &str) {
    buf.extend_from_slice(s.as_bytes());
    buf.put_u8(0);
}

impl Raw {
    async fn connect(addr: SocketAddr) -> Self {
        let stream = timeout(WAIT, TcpStream::connect(addr)).await.unwrap().unwrap();
        let mut raw = Self { stream };
        let mut body = BytesMut::new();
        body.put_i32(196_608);
        cstr(&mut body, "user");
        cstr(&mut body, "postgres");
        cstr(&mut body, "database");
        cstr(&mut body, "postgres");
        body.put_u8(0);
        let mut msg = BytesMut::new();
        msg.put_i32(body.len() as i32 + 4);
        msg.extend_from_slice(&body);
        raw.stream.write_all(&msg).await.unwrap();
        raw.until_ready().await;
        raw
    }

    async fn send(&mut self, tag: u8, body: &[u8]) {
        let mut msg = BytesMut::new();
        msg.put_u8(tag);
        msg.put_i32(body.len() as i32 + 4);
        msg.extend_from_slice(body);
        self.stream.write_all(&msg).await.unwrap();
    }

    async fn frame(&mut self) -> Frame {
        let mut header = [0u8; 5];
        timeout(WAIT, self.stream.read_exact(&mut header))
            .await
            .expect("a frame in time")
            .expect("frame header");
        let len = i32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut body = vec![0u8; len - 4];
        timeout(WAIT, self.stream.read_exact(&mut body))
            .await
            .expect("a frame body in time")
            .expect("frame body");
        (header[0], body)
    }

    /// Every message up to and including ReadyForQuery.
    async fn until_ready(&mut self) -> Vec<Frame> {
        let mut frames = Vec::new();
        loop {
            let frame = self.frame().await;
            let done = frame.0 == b'Z';
            frames.push(frame);
            if done {
                return frames;
            }
        }
    }

    async fn simple(&mut self, sql: &str) -> Vec<Frame> {
        let mut body = BytesMut::new();
        cstr(&mut body, sql);
        self.send(b'Q', &body).await;
        self.until_ready().await
    }

    /// Parse, Bind (text parameters), Describe portal, Execute, Sync.
    async fn extended(&mut self, sql: &str, params: &[&str]) -> Vec<Frame> {
        let mut parse = BytesMut::new();
        cstr(&mut parse, "");
        cstr(&mut parse, sql);
        parse.put_i16(0);
        self.send(b'P', &parse).await;
        let mut bind = BytesMut::new();
        cstr(&mut bind, "");
        cstr(&mut bind, "");
        bind.put_i16(0);
        bind.put_i16(params.len() as i16);
        for p in params {
            bind.put_i32(p.len() as i32);
            bind.extend_from_slice(p.as_bytes());
        }
        bind.put_i16(0);
        self.send(b'B', &bind).await;
        let mut describe = BytesMut::new();
        describe.put_u8(b'P');
        cstr(&mut describe, "");
        self.send(b'D', &describe).await;
        let mut execute = BytesMut::new();
        cstr(&mut execute, "");
        execute.put_i32(0);
        self.send(b'E', &execute).await;
        self.send(b'S', &[]).await;
        self.until_ready().await
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim_end_matches('\0').to_string()
}

/// The command tags, in order.
fn tags(frames: &[Frame]) -> Vec<String> {
    frames.iter().filter(|f| f.0 == b'C').map(|f| text(&f.1)).collect()
}

/// The column names of the (first) RowDescription.
fn columns(frames: &[Frame]) -> Option<Vec<String>> {
    let body = &frames.iter().find(|f| f.0 == b'T')?.1;
    let n = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut at = 2;
    let mut names = Vec::new();
    for _ in 0..n {
        let end = at + body[at..].iter().position(|b| *b == 0).unwrap();
        names.push(text(&body[at..end]));
        at = end + 1 + 18;
    }
    Some(names)
}

/// Every DataRow as text values (`None` = NULL).
fn data(frames: &[Frame]) -> Vec<Vec<Option<String>>> {
    frames
        .iter()
        .filter(|f| f.0 == b'D')
        .map(|f| {
            let body = &f.1;
            let n = i16::from_be_bytes([body[0], body[1]]) as usize;
            let mut at = 2;
            let mut row = Vec::new();
            for _ in 0..n {
                let len = i32::from_be_bytes([body[at], body[at + 1], body[at + 2], body[at + 3]]);
                at += 4;
                if len < 0 {
                    row.push(None);
                } else {
                    row.push(Some(text(&body[at..at + len as usize])));
                    at += len as usize;
                }
            }
            row
        })
        .collect()
}

fn first_column(frames: &[Frame]) -> Vec<String> {
    data(frames).into_iter().map(|r| r[0].clone().unwrap_or_default()).collect()
}

/// The fields of the ErrorResponse, if the batch has one.
fn error(frames: &[Frame]) -> Option<BTreeMap<char, String>> {
    let body = &frames.iter().find(|f| f.0 == b'E')?.1;
    let mut fields = BTreeMap::new();
    let mut at = 0;
    while at < body.len() && body[at] != 0 {
        let code = body[at] as char;
        let end = at + 1 + body[at + 1..].iter().position(|b| *b == 0).unwrap();
        fields.insert(code, text(&body[at + 1..end]));
        at = end + 1;
    }
    Some(fields)
}

/// The fields of every NoticeResponse.
fn notices(frames: &[Frame]) -> Vec<BTreeMap<char, String>> {
    frames
        .iter()
        .filter(|f| f.0 == b'N')
        .map(|f| {
            let body = &f.1;
            let mut fields = BTreeMap::new();
            let mut at = 0;
            while at < body.len() && body[at] != 0 {
                let end = at + 1 + body[at + 1..].iter().position(|b| *b == 0).unwrap();
                fields.insert(body[at] as char, text(&body[at + 1..end]));
                at = end + 1;
            }
            fields
        })
        .collect()
}

fn code(frames: &[Frame]) -> String {
    error(frames).map(|f| f[&'C'].clone()).unwrap_or_else(|| "-".to_string())
}

/// The transaction status byte of ReadyForQuery.
fn status(frames: &[Frame]) -> char {
    frames.last().map(|f| f.1[0] as char).unwrap()
}

fn seeded() -> Arc<EmbeddedDatabase> {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, val TEXT)").unwrap();
    db.execute("CREATE TABLE s (id INT, val TEXT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')").unwrap();
    db.execute("INSERT INTO s VALUES (2, 'B'), (3, 'C'), (4, 'D')").unwrap();
    Arc::new(db)
}

const MERGE: &str = "MERGE INTO t USING s ON t.id = s.id \
     WHEN MATCHED THEN UPDATE SET val = s.val \
     WHEN NOT MATCHED THEN INSERT (id, val) VALUES (s.id, s.val)";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn merge_reports_its_own_tag_on_the_simple_protocol() {
    let db = seeded();
    let (addr, _server) = serve(Arc::clone(&db)).await;
    let mut c = Raw::connect(addr).await;
    let frames = c.simple(MERGE).await;
    assert_eq!(tags(&frames), ["MERGE 3"], "{:?}", error(&frames));
    assert!(columns(&frames).is_none(), "no RETURNING, no row description");
    assert_eq!(status(&frames), 'I');
    let frames = c.simple("SELECT val FROM t ORDER BY id").await;
    assert_eq!(first_column(&frames), ["a", "B", "C", "D"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn merge_returning_is_described_and_sent_as_rows() {
    let db = seeded();
    let (addr, _server) = serve(Arc::clone(&db)).await;
    let mut c = Raw::connect(addr).await;
    let frames = c
        .simple(&format!("{MERGE} RETURNING merge_action() AS did, t.id, s.val AS src"))
        .await;
    assert_eq!(columns(&frames).unwrap(), ["did", "id", "src"], "{:?}", error(&frames));
    let mut rows: Vec<String> = data(&frames)
        .into_iter()
        .map(|r| r.into_iter().map(Option::unwrap_or_default).collect::<Vec<_>>().join(","))
        .collect();
    rows.sort();
    assert_eq!(rows, ["INSERT,4,D", "UPDATE,2,B", "UPDATE,3,C"]);
    assert_eq!(tags(&frames), ["MERGE 3"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn merge_on_the_extended_protocol_binds_parameters() {
    let db = seeded();
    let (addr, _server) = serve(Arc::clone(&db)).await;
    let mut c = Raw::connect(addr).await;
    let frames = c
        .extended(
            "MERGE INTO t USING (SELECT $1::int AS id, $2::text AS val) AS src ON t.id = src.id \
             WHEN MATCHED THEN UPDATE SET val = src.val \
             WHEN NOT MATCHED THEN INSERT (id, val) VALUES (src.id, src.val) \
             RETURNING merge_action(), t.val",
            &["9", "x'; DROP TABLE t; --"],
        )
        .await;
    assert_eq!(code(&frames), "-", "{:?}", error(&frames));
    assert_eq!(columns(&frames).unwrap(), ["merge_action", "val"]);
    assert_eq!(
        data(&frames),
        [[Some("INSERT".to_string()), Some("x'; DROP TABLE t; --".to_string())]]
    );
    assert_eq!(tags(&frames), ["MERGE 1"]);

    // Through a driver: the count a driver reads off the tag.
    let client = driver(addr).await;
    let n = client
        .execute(
            "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED AND s.id = $1::int THEN DELETE",
            &[&3i32],
        )
        .await
        .unwrap();
    assert_eq!(n, 1);
    let left = client.query("SELECT id FROM t ORDER BY id", &[]).await.unwrap();
    let left: Vec<i32> = left.iter().map(|r| r.get(0)).collect();
    assert_eq!(left, [1, 2, 9]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn merge_errors_carry_their_sqlstate_and_change_nothing() {
    let db = seeded();
    db.execute("INSERT INTO s VALUES (2, 'again')").unwrap();
    let (addr, _server) = serve(Arc::clone(&db)).await;
    let mut c = Raw::connect(addr).await;
    let frames = c.simple(MERGE).await;
    let fields = error(&frames).expect("an error");
    assert_eq!(fields[&'C'], "21000");
    assert_eq!(fields[&'M'], "MERGE command cannot affect row a second time");
    assert!(fields.contains_key(&'H'));
    let frames = c.simple("SELECT val FROM t ORDER BY id").await;
    assert_eq!(first_column(&frames), ["a", "b", "c"]);

    // Inside a block the failure fails the block; ROLLBACK ends it.
    c.simple("BEGIN").await;
    c.simple("INSERT INTO t VALUES (50, 'kept?')").await;
    let frames = c.simple(MERGE).await;
    assert_eq!(code(&frames), "21000");
    assert_eq!(status(&frames), 'E');
    c.simple("ROLLBACK").await;
    let frames = c.simple("SELECT count(*) FROM t").await;
    assert_eq!(first_column(&frames), ["3"]);

    // A view is not a target.
    c.simple("CREATE VIEW v AS SELECT id, val FROM t").await;
    let frames = c
        .simple("MERGE INTO v USING s ON v.id = s.id WHEN MATCHED THEN DELETE")
        .await;
    assert_eq!(code(&frames), "42809");

    // MERGE BRANCH is still its own statement, not this one.
    let frames = c.simple("MERGE BRANCH nope INTO main").await;
    let message = error(&frames).expect("no such branch")[&'M'].to_lowercase();
    assert!(message.contains("branch") || message.contains("nope"), "{message}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_statements_answer_with_postgresql_s_tags_and_columns() {
    let db = seeded();
    let (addr, _server) = serve(Arc::clone(&db)).await;
    let mut c = Raw::connect(addr).await;

    let frames = c.simple("DECLARE c CURSOR FOR SELECT id FROM t").await;
    assert_eq!(code(&frames), "25P01");

    assert_eq!(tags(&c.simple("BEGIN").await), ["BEGIN"]);
    let frames = c
        .simple("DECLARE c SCROLL CURSOR FOR SELECT id AS k, val FROM t ORDER BY id")
        .await;
    assert_eq!(tags(&frames), ["DECLARE CURSOR"], "{:?}", error(&frames));
    assert_eq!(status(&frames), 'T');

    let frames = c.simple("FETCH 2 FROM c").await;
    assert_eq!(columns(&frames).unwrap(), ["k", "val"]);
    assert_eq!(first_column(&frames), ["1", "2"]);
    assert_eq!(tags(&frames), ["FETCH 2"]);

    let frames = c.simple("MOVE BACKWARD 1 IN c").await;
    assert!(columns(&frames).is_none());
    assert_eq!(tags(&frames), ["MOVE 1"]);

    let frames = c.simple("FETCH ALL FROM c").await;
    assert_eq!(first_column(&frames), ["2", "3"]);
    assert_eq!(tags(&frames), ["FETCH 2"]);

    // Past the end: the columns are still described, with no rows.
    let frames = c.simple("FETCH NEXT FROM c").await;
    assert_eq!(columns(&frames).unwrap(), ["k", "val"]);
    assert_eq!(tags(&frames), ["FETCH 0"]);

    let frames = c.simple("SELECT name, is_scrollable FROM pg_cursors").await;
    assert_eq!(data(&frames), [[Some("c".to_string()), Some("t".to_string())]]);

    let frames = c.simple("CLOSE c").await;
    assert_eq!(tags(&frames), ["CLOSE CURSOR"]);
    let frames = c.simple("FETCH c").await;
    assert_eq!(code(&frames), "34000");
    assert_eq!(status(&frames), 'E', "an error inside a block fails the block");
    assert_eq!(tags(&c.simple("ROLLBACK").await), ["ROLLBACK"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hold_and_the_end_of_the_block_on_the_wire() {
    let db = seeded();
    let (addr, _server) = serve(Arc::clone(&db)).await;
    let mut c = Raw::connect(addr).await;
    c.simple("BEGIN").await;
    c.simple("DECLARE held CURSOR WITH HOLD FOR SELECT id FROM t ORDER BY id").await;
    c.simple("DECLARE plain CURSOR FOR SELECT id FROM t ORDER BY id").await;
    assert_eq!(tags(&c.simple("COMMIT").await), ["COMMIT"]);
    assert_eq!(first_column(&c.simple("FETCH held").await), ["1"]);
    assert_eq!(code(&c.simple("FETCH plain").await), "34000");

    c.simple("BEGIN").await;
    c.simple("DECLARE doomed CURSOR WITH HOLD FOR SELECT id FROM t").await;
    c.simple("ROLLBACK").await;
    assert_eq!(code(&c.simple("FETCH doomed").await), "34000");
    assert_eq!(first_column(&c.simple("FETCH held").await), ["2"]);

    // DISCARD ALL closes what is left; CLOSE ALL is accepted with nothing open.
    assert_eq!(db.open_cursor_count(), 1);
    assert_eq!(tags(&c.simple("DISCARD ALL").await), ["DISCARD ALL"]);
    assert_eq!(db.open_cursor_count(), 0);
    assert_eq!(code(&c.simple("FETCH held").await), "34000");
    assert_eq!(tags(&c.simple("CLOSE ALL").await), ["CLOSE CURSOR"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_connections_each_have_their_own_cursor_of_one_name() {
    let db = seeded();
    let (addr, _server) = serve(Arc::clone(&db)).await;
    let mut a = Raw::connect(addr).await;
    let mut b = Raw::connect(addr).await;
    a.simple("DECLARE c CURSOR WITH HOLD FOR SELECT id FROM t WHERE id = 1").await;
    let frames = b
        .simple("DECLARE c CURSOR WITH HOLD FOR SELECT val FROM t WHERE id = 3")
        .await;
    assert_eq!(tags(&frames), ["DECLARE CURSOR"], "{:?}", error(&frames));
    assert_eq!(first_column(&a.simple("FETCH c").await), ["1"]);
    assert_eq!(first_column(&b.simple("FETCH c").await), ["c"]);
    assert_eq!(first_column(&a.simple("SELECT count(*) FROM pg_cursors").await), ["1"]);
    a.simple("CLOSE c").await;
    assert_eq!(tags(&b.simple("MOVE BACKWARD ALL IN c").await), ["MOVE 1"]);
    assert_eq!(db.open_cursor_count(), 1);
    // A connection that goes away takes its cursors with it.
    drop(b);
    for _ in 0..100 {
        if db.open_cursor_count() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(db.open_cursor_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursors_on_the_extended_protocol() {
    let db = seeded();
    let (addr, _server) = serve(Arc::clone(&db)).await;
    let mut c = Raw::connect(addr).await;
    let frames = c
        .extended(
            "DECLARE c CURSOR WITH HOLD FOR SELECT id, val FROM t WHERE id >= $1::int ORDER BY id",
            &["2"],
        )
        .await;
    assert_eq!(tags(&frames), ["DECLARE CURSOR"], "{:?}", error(&frames));
    assert!(frames.iter().any(|f| f.0 == b'n'), "a DECLARE describes no rows");

    let frames = c.extended("FETCH 1 FROM c", &[]).await;
    assert_eq!(columns(&frames).unwrap(), ["id", "val"], "{:?}", error(&frames));
    assert_eq!(data(&frames), [[Some("2".to_string()), Some("b".to_string())]]);
    assert_eq!(tags(&frames), ["FETCH 1"]);

    let frames = c.extended("MOVE 1 FROM c", &[]).await;
    assert_eq!(tags(&frames), ["MOVE 1"]);
    let frames = c.extended("CLOSE c", &[]).await;
    assert_eq!(tags(&frames), ["CLOSE CURSOR"]);
    let frames = c.extended("FETCH c", &[]).await;
    assert_eq!(code(&frames), "34000");

    // Through a driver: typed rows of a FETCH.
    let client = driver(addr).await;
    client.batch_execute("BEGIN").await.unwrap();
    client
        .execute("DECLARE d CURSOR FOR SELECT id, val FROM t ORDER BY id", &[])
        .await
        .unwrap();
    let rows = client.query("FETCH 2 FROM d", &[]).await.unwrap();
    let got: Vec<(i32, String)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(got, [(1, "a".to_string()), (2, "b".to_string())]);
    client.batch_execute("COMMIT").await.unwrap();
    let err = client.query("FETCH d", &[]).await.unwrap_err();
    assert_eq!(err.code().map(|c| c.code().to_string()).as_deref(), Some("34000"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raise_reaches_the_client_with_its_sqlstate_and_a_notice_is_delivered() {
    let db = seeded();
    db.execute(
        "CREATE PROCEDURE complain(kind INT, who TEXT) LANGUAGE plpgsql AS $$ \
         BEGIN \
           IF kind = 1 THEN RAISE EXCEPTION 'no account for %', who; END IF; \
           IF kind = 2 THEN RAISE EXCEPTION 'custom' USING ERRCODE = 'AB123', DETAIL = 'about ' || who, HINT = 'try again'; END IF; \
           RAISE NOTICE 'only a notice for %', who; \
           RAISE WARNING 'and a warning'; \
           INSERT INTO s VALUES (kind, who); \
         END $$",
    )
    .unwrap();
    let (addr, _server) = serve(Arc::clone(&db)).await;
    let mut c = Raw::connect(addr).await;

    let frames = c.simple("CALL complain(1, 'zed')").await;
    let fields = error(&frames).expect("an error");
    assert_eq!(fields[&'C'], "P0001");
    assert_eq!(fields[&'M'], "no account for zed");

    let frames = c.simple("CALL complain(2, 'zed')").await;
    let fields = error(&frames).expect("an error");
    assert_eq!(fields[&'C'], "AB123");
    assert_eq!(fields[&'M'], "custom");
    assert_eq!(fields[&'D'], "about zed");
    assert_eq!(fields[&'H'], "try again");

    let frames = c.simple("CALL complain(0, 'zed')").await;
    assert_eq!(code(&frames), "-", "{:?}", error(&frames));
    let seen = notices(&frames);
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_eq!(seen[0][&'S'], "NOTICE");
    assert_eq!(seen[0][&'M'], "only a notice for zed");
    assert_eq!(seen[1][&'S'], "WARNING");
    assert_eq!(seen[1][&'C'], "01000");
    assert_eq!(tags(&frames), ["CALL"]);
    assert_eq!(first_column(&c.simple("SELECT val FROM s WHERE id = 0").await), ["zed"]);

    // The extended protocol reports the same error.
    let frames = c.extended("CALL complain($1::int, $2::text)", &["2", "pat"]).await;
    let fields = error(&frames).expect("an error");
    assert_eq!(fields[&'C'], "AB123");
    assert_eq!(fields[&'D'], "about pat");
}
