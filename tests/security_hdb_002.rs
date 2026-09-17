//! HDB-002 — plain text can be assigned to a `tsvector` / `tsquery` column.
//!
//! The report:
//!
//! ```sql
//! CREATE TABLE documents (search_vector tsvector);
//! INSERT INTO documents VALUES ('hello world foo');   -- ERROR: Invalid JSON string
//! ```
//!
//! `TSVECTOR` and `TSQUERY` were mapped to `DataType::Json` by the planner,
//! because Nano STORES them as a JSON array of normalised tokens. That is the
//! right storage and the wrong declared type: the assignment coercion for
//! `Json` validates the string as JSON, so every text write — the implicit
//! text→tsvector assignment PostgreSQL performs, `'hello world'::tsvector`,
//! and `'fox'::tsquery` — was rejected as malformed JSON. The fix gives the
//! two types their own `DataType` variants with their own input rule; the
//! storage, the `@@` operator and the BM25 engine are untouched.
//!
//! The same change takes PostgreSQL's real `tsvector` OID (3614) back from
//! Nano's `vector` type, which had been registered there in `pg_type` while
//! the wire advertised `1000` — PostgreSQL's `_bool`. `vector` is an
//! EXTENSION type: it is registered in `pg_type` under its own private
//! user-band OID (`16385`, where pgvector itself lands), and it is advertised
//! on the wire as TEXT (`25`), which is literally what the value on the wire
//! is (`[0.1,0.2]`). Case 8 pins BOTH numbers and the reason they differ.
//!
//! Case 9 is the COPY half of "prints the way PostgreSQL prints it": `COPY …
//! TO STDOUT` renders a tsvector column in the quoted-lexeme form and the
//! output re-imports through `COPY … FROM STDIN` with the identical token set.
//!
//! Expected on the UNFIXED tree: cases 1, 2, 3, 5, 6, 7, 8 and 9 fail. Case 4
//! is the control that must pass on BOTH trees: a `json` column still
//! validates.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use bytes::{BufMut, BytesMut};
use heliosdb_nano::{
    protocol::postgres::server::{PgServer, PgServerConfig},
    EmbeddedDatabase, Value,
};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

const IO_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);

// ===========================================================================
// Embedded helpers
// ===========================================================================

fn mem_db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory db")
}

/// The single value of a single-row, single-column query.
fn scalar(db: &EmbeddedDatabase, sql: &str) -> Value {
    let rows = db
        .query(sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}` must succeed: {e}"));
    assert_eq!(rows.len(), 1, "`{sql}` must return exactly one row, got {}", rows.len());
    rows[0].values.first().cloned().expect("one column")
}

fn scalar_bool(db: &EmbeddedDatabase, sql: &str) -> bool {
    match scalar(db, sql) {
        Value::Boolean(b) => b,
        other => panic!("`{sql}` must return a boolean, got {other:?}"),
    }
}

/// The token set a tsvector column holds, decoded from the canonical
/// `Value::Json` array. Storage is deliberately unchanged by this fix, so the
/// assertions read it directly rather than through a rendering.
fn stored_tokens(db: &EmbeddedDatabase, sql: &str) -> Vec<String> {
    match scalar(db, sql) {
        Value::Json(j) => serde_json::from_str::<Vec<String>>(&j)
            .unwrap_or_else(|e| panic!("`{sql}` stored `{j}`, not a token array: {e}")),
        other => panic!("`{sql}` must store a JSON token array, got {other:?}"),
    }
}

// ===========================================================================
// 1. The report, verbatim
// ===========================================================================

/// REGRESSION: `INSERT INTO documents VALUES ('hello world foo')` answered
/// "Invalid JSON string" because the column was declared `Json`.
#[test]
fn hdb002_report_poc_plain_text_is_accepted() {
    let db = mem_db();
    db.execute("CREATE TABLE documents (search_vector tsvector)")
        .expect("a tsvector column must be creatable");
    db.execute("INSERT INTO documents VALUES ('hello world foo')")
        .expect("*** the report: plain text must be assignable to a tsvector column");

    assert!(
        scalar_bool(&db, "SELECT search_vector @@ to_tsquery('world') FROM documents"),
        "the assigned text must be tokenised, so `@@ to_tsquery('world')` matches"
    );
    assert!(
        !scalar_bool(&db, "SELECT search_vector @@ to_tsquery('nothing') FROM documents"),
        "a term that is not in the document must NOT match — otherwise the \
         assertion above passes for the wrong reason"
    );
}

// ===========================================================================
// 2. The explicit cast, and PostgreSQL's quoted-lexeme input form
// ===========================================================================

/// `'hello world'::tsvector` is the spelling every PostgreSQL tutorial uses,
/// and `'hello' 'world'` is what `tsvectorout` PRINTS — so a value copied out
/// of psql, or read back by a client, must go back in.
#[test]
fn hdb002_explicit_cast_and_quoted_lexemes() {
    let db = mem_db();

    assert!(
        scalar_bool(&db, "SELECT 'hello world'::tsvector @@ to_tsquery('hello')"),
        "*** an explicit ::tsvector cast of plain text must work"
    );

    db.execute("CREATE TABLE documents (search_vector tsvector)")
        .expect("create");
    // The SQL literal `'''Hello'' ''World'''` is the string `'Hello' 'World'`:
    // PostgreSQL's quoted-lexeme form, two lexemes.
    db.execute("INSERT INTO documents VALUES ('''Hello'' ''World''')")
        .expect("*** the quoted-lexeme form must round-trip back in");

    let tokens = stored_tokens(&db, "SELECT search_vector FROM documents");
    assert!(
        tokens.contains(&"Hello".to_string()) && tokens.contains(&"World".to_string()),
        "a QUOTED lexeme is taken verbatim — case preserved, no re-tokenisation. Got {tokens:?}"
    );
    assert_eq!(tokens.len(), 2, "two lexemes in, two lexemes stored: {tokens:?}");
}

// ===========================================================================
// 3. `tsquery` has the same input rule
// ===========================================================================

/// `'fox & dog'::tsquery` failed identically before the fix. The boolean
/// operators are term separators in Nano (docs/compatibility/fts.md), which is
/// what `to_tsquery` already did — the point here is that the LITERAL is
/// accepted at all.
#[test]
fn hdb002_tsquery_literal_and_cast() {
    let db = mem_db();
    db.execute("CREATE TABLE q (query tsquery)")
        .expect("a tsquery column must be creatable");
    db.execute("INSERT INTO q VALUES ('fox & dog')")
        .expect("*** a text tsquery literal must be assignable");

    assert!(
        scalar_bool(&db, "SELECT to_tsvector('the quick fox') @@ query FROM q"),
        "the stored query must match a document containing `fox`"
    );
    assert!(
        !scalar_bool(&db, "SELECT to_tsvector('x') @@ 'fox'::tsquery"),
        "*** an inline ::tsquery cast must work — and must NOT match a \
         document without the term"
    );
}

// ===========================================================================
// 4. CONTROL — a real `json` column still validates its input
// ===========================================================================

/// The fix must not loosen `json`. This case passes on the unfixed tree too;
/// if it ever fails, the tsvector input rule leaked into the JSON types.
#[test]
fn hdb002_json_columns_still_validate() {
    let db = mem_db();
    db.execute("CREATE TABLE j (doc json)").expect("create");

    assert!(
        db.execute("INSERT INTO j VALUES ('hello world')").is_err(),
        "a json column must still REJECT text that is not JSON"
    );
    db.execute(r#"INSERT INTO j VALUES ('["a"]')"#)
        .expect("valid JSON must still be accepted");
    assert_eq!(
        scalar(&db, "SELECT count(*) FROM j"),
        Value::Int8(1),
        "exactly the one valid row is stored"
    );
}

// ===========================================================================
// 5. A bound parameter takes the same path
// ===========================================================================

/// Applications do not write literals — they bind. The coercion is the same
/// one, but it is reached through the params family, so it gets its own case.
#[test]
fn hdb002_bound_parameter_text_to_tsvector() {
    let db = mem_db();
    db.execute("CREATE TABLE documents (search_vector tsvector)")
        .expect("create");
    db.execute_params(
        "INSERT INTO documents VALUES ($1)",
        &[Value::String("param text".to_string())],
    )
    .expect("*** a bound text parameter must be assignable to a tsvector column");

    assert!(
        scalar_bool(&db, "SELECT search_vector @@ to_tsquery('param') FROM documents"),
        "the bound text must be tokenised like a literal"
    );
}

// ===========================================================================
// 6. The declared type survives a reopen
// ===========================================================================

/// `DataType` is persisted by bincode VARIANT INDEX, so the two new variants
/// were appended to the end of the enum. This case is the guard on that: a
/// column declared `tsvector` before a restart must still be `tsvector` after
/// one — not silently re-typed into whatever variant now occupies its index.
#[test]
fn hdb002_reopen_preserves_the_declared_type() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_str().expect("utf-8 path").to_string();

    {
        let db = EmbeddedDatabase::new(&path).expect("open");
        db.execute("CREATE TABLE documents (search_vector tsvector)")
            .expect("create");
        db.execute("INSERT INTO documents VALUES ('hello world foo')")
            .expect("insert");
    }

    let db = EmbeddedDatabase::new(&path).expect("reopen");

    // `data_type` is the declared variant's Debug spelling, `udt_name` the
    // canonical PG name; both must say tsvector, neither json.
    const SQL: &str = "SELECT data_type, udt_name FROM information_schema.columns \
                       WHERE table_name = 'documents' AND column_name = 'search_vector'";
    let rows = db.query(SQL, &[]).expect("introspection must succeed");
    assert_eq!(rows.len(), 1, "exactly one row describes documents.search_vector");
    let data_type = match &rows[0].values[0] {
        Value::String(s) => s.to_lowercase(),
        other => panic!("data_type must be text, got {other:?}"),
    };
    let udt_name = match &rows[0].values[1] {
        Value::String(s) => s.clone(),
        other => panic!("udt_name must be text, got {other:?}"),
    };
    assert_eq!(data_type, "tsvector", "*** the reopened column must still be tsvector");
    assert_eq!(udt_name, "tsvector", "*** udt_name must be tsvector, not json");

    assert_eq!(
        stored_tokens(&db, "SELECT search_vector FROM documents").len(),
        3,
        "the pre-restart row must still be there, with its three tokens"
    );
    db.execute("INSERT INTO documents VALUES ('after the restart')")
        .expect("*** plain text must still be assignable after a reopen");
    assert_eq!(
        scalar(
            &db,
            "SELECT count(*) FROM documents WHERE search_vector @@ to_tsquery('restart')"
        ),
        Value::Int8(1),
        "the post-restart row is matchable"
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT count(*) FROM documents WHERE search_vector @@ to_tsquery('hello')"
        ),
        Value::Int8(1),
        "so is the pre-restart one"
    );
}

// ===========================================================================
// Server + client harness (same shape as tests/security_hdb_004.rs)
// ===========================================================================

async fn setup_server() -> (SocketAddr, String, tokio::task::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test port");
    let addr = listener.local_addr().expect("test addr");
    drop(listener);

    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let config = PgServerConfig::with_address(addr);
    let server = PgServer::new(config, db).expect("server");
    let handle = tokio::spawn(async move {
        let _ = server.serve().await;
    });

    tokio::time::sleep(Duration::from_millis(150)).await;
    let conn_string = format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port());
    (addr, conn_string, handle)
}

async fn connect(conn_string: &str) -> Client {
    let (client, connection) = timeout(CONNECT_TIMEOUT, tokio_postgres::connect(conn_string, NoTls))
        .await
        .expect("connect timeout")
        .expect("connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// Column 0 of every row of a simple query, as text.
async fn simple_col0(client: &Client, sql: &str) -> Vec<String> {
    let messages = timeout(QUERY_TIMEOUT, client.simple_query(sql))
        .await
        .expect("query timeout")
        .unwrap_or_else(|e| panic!("`{sql}` must succeed: {e}"));
    messages
        .into_iter()
        .filter_map(|message| match message {
            SimpleQueryMessage::Row(row) => Some(row.get(0).unwrap_or_default().to_string()),
            _ => None,
        })
        .collect()
}

// --- raw frame-level client -------------------------------------------------
//
// Used where the assertion is about the BYTES the server put on the wire
// rather than about what a driver made of them: the RowDescription OID in
// case 8, and the CopyData payloads in case 9 (tokio-postgres' `copy_out`
// goes through the extended protocol, which is a different code path from
// the simple-query COPY the server implements). Reading the frames directly
// asserts exactly what the server sent, and nothing else.

fn put_cstr(buf: &mut BytesMut, value: &str) {
    buf.extend_from_slice(value.as_bytes());
    buf.put_u8(0);
}

fn startup_message() -> BytesMut {
    let mut body = BytesMut::new();
    body.put_i32(196_608);
    put_cstr(&mut body, "user");
    put_cstr(&mut body, "postgres");
    put_cstr(&mut body, "database");
    put_cstr(&mut body, "postgres");
    body.put_u8(0);

    let mut msg = BytesMut::new();
    msg.put_i32((body.len() + 4) as i32);
    msg.extend_from_slice(&body);
    msg
}

fn query_message(sql: &str) -> BytesMut {
    let mut body = BytesMut::new();
    put_cstr(&mut body, sql);
    let mut msg = BytesMut::new();
    msg.put_u8(b'Q');
    msg.put_i32((body.len() + 4) as i32);
    msg.extend_from_slice(&body);
    msg
}

/// A frontend `CopyData` (`d`) frame carrying one COPY line.
fn copy_data_message(payload: &[u8]) -> BytesMut {
    let mut msg = BytesMut::new();
    msg.put_u8(b'd');
    msg.put_i32((payload.len() + 4) as i32);
    msg.extend_from_slice(payload);
    msg
}

/// The frontend `CopyDone` (`c`) frame — header only.
fn copy_done_message() -> BytesMut {
    let mut msg = BytesMut::new();
    msg.put_u8(b'c');
    msg.put_i32(4);
    msg
}

async fn read_backend_message(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0u8; 5];
    timeout(IO_TIMEOUT, stream.read_exact(&mut header))
        .await
        .expect("read header timeout")
        .expect("read header");
    let len = i32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut body = vec![0u8; len - 4];
    timeout(IO_TIMEOUT, stream.read_exact(&mut body))
        .await
        .expect("read body timeout")
        .expect("read body");
    (header[0], body)
}

async fn connect_wire(addr: SocketAddr) -> TcpStream {
    let mut stream = timeout(IO_TIMEOUT, TcpStream::connect(addr))
        .await
        .expect("connect timeout")
        .expect("connect");
    timeout(IO_TIMEOUT, stream.write_all(&startup_message()))
        .await
        .expect("startup write timeout")
        .expect("startup write");
    loop {
        if read_backend_message(&mut stream).await.0 == b'Z' {
            return stream;
        }
    }
}

/// Send `sql` as one `Q` message and collect every frame through ReadyForQuery.
async fn wire_query(stream: &mut TcpStream, sql: &str) -> Vec<(u8, Vec<u8>)> {
    timeout(IO_TIMEOUT, stream.write_all(&query_message(sql)))
        .await
        .expect("query write timeout")
        .expect("query write");
    let mut frames = Vec::new();
    loop {
        let frame = read_backend_message(stream).await;
        let tag = frame.0;
        frames.push(frame);
        if tag == b'Z' {
            return frames;
        }
    }
}

/// Every field's `dataTypeOID` from a RowDescription (`T`) frame body.
fn row_description_oids(body: &[u8]) -> Vec<i32> {
    let count = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut oids = Vec::with_capacity(count);
    let mut pos = 2usize;
    for _ in 0..count {
        while body[pos] != 0 {
            pos += 1;
        }
        pos += 1; // the field name's NUL
        pos += 4; // tableOID
        pos += 2; // columnAttrNumber
        oids.push(i32::from_be_bytes([
            body[pos],
            body[pos + 1],
            body[pos + 2],
            body[pos + 3],
        ]));
        pos += 4; // dataTypeOID
        pos += 2; // dataTypeSize
        pos += 4; // typeModifier
        pos += 2; // formatCode
    }
    oids
}

/// The `dataTypeOID` of column 0 of the one RowDescription in `frames`.
fn first_column_oid(frames: &[(u8, Vec<u8>)]) -> i32 {
    let body = frames
        .iter()
        .find(|(tag, _)| *tag == b'T')
        .map(|(_, body)| body.clone())
        .expect("the response must carry a RowDescription");
    *row_description_oids(&body).first().expect("at least one column")
}

/// `COPY … TO STDOUT` over the raw wire: every `CopyData` (`d`) payload the
/// server streamed, in order, as text (one COPY line each, `\n`-terminated).
async fn wire_copy_out(stream: &mut TcpStream, sql: &str) -> Vec<String> {
    let frames = wire_query(stream, sql).await;
    assert!(
        frames.iter().any(|(tag, _)| *tag == b'H'),
        "`{sql}` must answer CopyOutResponse; got frames {:?}",
        frames.iter().map(|(tag, _)| *tag as char).collect::<Vec<_>>()
    );
    frames
        .iter()
        .filter(|(tag, _)| *tag == b'd')
        .map(|(_, body)| String::from_utf8(body.clone()).expect("a COPY payload must be UTF-8"))
        .collect()
}

/// `COPY … FROM STDIN` over the raw wire: issue the statement, wait for
/// `CopyInResponse` (`G`), stream each line as a `CopyData` frame, then
/// `CopyDone`. Panics with the server's frame tags if it refuses.
async fn wire_copy_in(stream: &mut TcpStream, sql: &str, lines: &[String]) {
    timeout(IO_TIMEOUT, stream.write_all(&query_message(sql)))
        .await
        .expect("copy-in query write timeout")
        .expect("copy-in query write");
    loop {
        let (tag, body) = read_backend_message(stream).await;
        match tag {
            b'G' => break,
            b'E' => panic!(
                "`{sql}` was refused: {}",
                String::from_utf8_lossy(&body).replace('\0', " ")
            ),
            b'Z' => panic!("`{sql}` finished without ever asking for CopyData"),
            _ => {}
        }
    }
    for line in lines {
        timeout(IO_TIMEOUT, stream.write_all(&copy_data_message(line.as_bytes())))
            .await
            .expect("CopyData write timeout")
            .expect("CopyData write");
    }
    timeout(IO_TIMEOUT, stream.write_all(&copy_done_message()))
        .await
        .expect("CopyDone write timeout")
        .expect("CopyDone write");
    loop {
        let (tag, body) = read_backend_message(stream).await;
        match tag {
            b'Z' => return,
            b'E' => panic!(
                "`{sql}` failed while loading: {}",
                String::from_utf8_lossy(&body).replace('\0', " ")
            ),
            _ => {}
        }
    }
}

// ===========================================================================
// 7. The wire reports the type, and prints the PostgreSQL text form
// ===========================================================================

/// A driver learns a column's type from the RowDescription OID. `tsvector`
/// must be PostgreSQL's real 3614 (it was 114 — `json` — before the fix), and
/// the value must PRINT the way PostgreSQL prints it: `'hello' 'world'`, not
/// the JSON array Nano stores.
#[tokio::test]
async fn hdb002_wire_reports_tsvector_type_and_text_form() {
    let (_addr, conn_string, _h) = setup_server().await;
    let client = connect(&conn_string).await;
    client
        .batch_execute("CREATE TABLE documents (search_vector tsvector)")
        .await
        .expect("a tsvector column must be creatable over the wire");
    client
        .batch_execute("INSERT INTO documents VALUES ('hello world')")
        .await
        .expect("*** plain text must be assignable over the wire too");

    let stmt = timeout(QUERY_TIMEOUT, client.prepare("SELECT search_vector FROM documents"))
        .await
        .expect("prepare timeout")
        .expect("prepare must succeed");
    assert_eq!(
        stmt.columns()[0].type_().oid(),
        3614,
        "*** a tsvector column must be advertised as PostgreSQL's tsvector OID"
    );

    // The text form: PostgreSQL's quoted lexemes, not `["hello","world"]`.
    assert_eq!(
        simple_col0(&client, "SELECT search_vector FROM documents").await,
        vec!["'hello' 'world'".to_string()],
        "*** a tsvector column prints as PostgreSQL prints it"
    );

    assert_eq!(
        simple_col0(&client, "SELECT typname FROM pg_type WHERE oid = 3614").await,
        vec!["tsvector".to_string()],
        "*** 3614 must name tsvector in pg_type — `vector` used to sit there"
    );
    assert_eq!(
        simple_col0(&client, "SELECT oid FROM pg_type WHERE typname = 'vector'").await,
        vec!["16385".to_string()],
        "*** and `vector` must have moved to its own private OID"
    );
    assert_eq!(
        simple_col0(&client, "SELECT typname FROM pg_type WHERE oid = 3615").await,
        vec!["tsquery".to_string()],
        "tsquery takes PostgreSQL's 3615"
    );
}

// ===========================================================================
// 8. `vector`: registered under 16385, advertised as text
// ===========================================================================

/// The sprinter item folded into HDB-002: `vector` answered 3614 in `pg_type`
/// — PostgreSQL's `tsvector` OID, which `tsvector` itself now needs — and
/// `1000` on the wire, which is PostgreSQL's `_bool`. This case pins the two
/// numbers `vector` answers today AND the reason they are deliberately
/// different:
///
/// * **`pg_type` says 16385.** `vector` is an EXTENSION type, so it gets one
///   private OID from the first user band — the band pgvector itself lands in
///   on a fresh install. A pgvector client resolves the type by NAME
///   (`WHERE typname = 'vector'`), which is the lookup this serves, and the
///   array type `_vector` follows it at 16386.
///
/// * **RowDescription says 25 (`text`).** A user-band OID must NOT appear in
///   a RowDescription. tokio-postgres — and therefore sqlx and Prisma's query
///   engine — resolves any result-column OID it does not know natively by
///   preparing its own TYPEINFO query against the server; that query's `$1` is
///   described by Nano as OID `0`, which the driver cannot resolve either, so
///   it re-prepares TYPEINFO forever. (Two test files are `#[ignore]`d on that
///   recursion: `tests/server_mode_integration_test.rs` and
///   `tests/extended_query_param_select.rs`; fixing ParameterDescription is
///   filed separately.) `text` is also what the value on the wire literally
///   is — pgvector's `[0.1,0.2]` text form — so a client that registered the
///   type by name decodes exactly the bytes it expects.
///
/// If Nano ever describes an inferred parameter as something a driver can
/// resolve, advertising 16385 here becomes safe and this case changes with it.
#[tokio::test]
async fn hdb002_wire_vector_columns_advertise_text_and_register_under_16385() {
    let (addr, conn_string, _h) = setup_server().await;
    let client = connect(&conn_string).await;
    client
        .batch_execute("CREATE TABLE v (e VECTOR(3))")
        .await
        .expect("create a vector column");
    client
        .batch_execute("INSERT INTO v VALUES ('[1,2,3]')")
        .await
        .expect("insert a vector");

    let mut stream = connect_wire(addr).await;
    let frames = wire_query(&mut stream, "SELECT e FROM v").await;
    assert_eq!(
        first_column_oid(&frames),
        25,
        "*** a vector column must advertise `text` (25) — NOT the user-band OID \
         `pg_type` registers it under, which sends every rust-postgres client \
         into an unbounded TYPEINFO lookup (it advertised 1000, PostgreSQL's \
         `_bool`, before the fix)"
    );

    // The same statement through the driver: with `text` advertised, `prepare`
    // resolves the column locally and returns. On 16385 this call never
    // returns — which is the regression the OID choice above prevents.
    let stmt = timeout(QUERY_TIMEOUT, client.prepare("SELECT e FROM v"))
        .await
        .expect("*** prepare over a vector column must not hang in TYPEINFO recursion")
        .expect("prepare must succeed");
    assert_eq!(
        stmt.columns()[0].type_().oid(),
        25,
        "the driver sees the same `text` the frame carried"
    );

    // ... and the registry still answers 16385 by name, which is how a
    // pgvector client resolves the type.
    assert_eq!(
        simple_col0(&client, "SELECT oid FROM pg_type WHERE typname = 'vector'").await,
        vec!["16385".to_string()],
        "*** `vector` is registered under its own private user-band OID"
    );
    assert_eq!(
        simple_col0(&client, "SELECT typname FROM pg_type WHERE oid = 16385").await,
        vec!["vector".to_string()],
        "*** and that OID must resolve back to `vector`"
    );
    assert_eq!(
        simple_col0(&client, "SELECT typname FROM pg_type WHERE oid = 16386").await,
        vec!["_vector".to_string()],
        "the array type follows its element type into the user band"
    );
}

// ===========================================================================
// 9. COPY TO STDOUT prints lexemes, and its output re-imports unchanged
// ===========================================================================

/// `COPY … TO STDOUT` is a TEXT rendering of the same rows `SELECT` renders,
/// so it has to obey the same rule: a column DECLARED `tsvector` prints in
/// PostgreSQL's quoted-lexeme form.
///
/// It did not — it went through `tuple_to_pg_values`, which has no access to
/// the declared type, so it emitted the raw JSON token array. That is not a
/// cosmetic difference: `COPY … FROM STDIN` re-reads its own output through
/// the tsvector INPUT rule, and `["New York","Hello"]` does not parse as the
/// lexeme form — it falls through to the tokenizer and comes back as three
/// lower-cased tokens (`new`, `york`, `hello`). Dumping and reloading a table
/// through the two halves of the same statement pair silently changed the
/// data. With the lexeme form on both sides the round trip is exact.
///
/// The two-word lexeme is load-bearing: `'New York'` is exactly the token a
/// re-tokenisation would split, and the capitals are what it would fold.
#[tokio::test]
async fn hdb002_copy_to_stdout_prints_lexemes_and_round_trips() {
    let (addr, conn_string, _h) = setup_server().await;
    let client = connect(&conn_string).await;
    client
        .batch_execute("CREATE TABLE documents (search_vector tsvector)")
        .await
        .expect("create the source table");
    client
        .batch_execute("CREATE TABLE documents_reloaded (search_vector tsvector)")
        .await
        .expect("create the destination table");
    // The SQL literal is the string `'New York' 'Hello'` — two lexemes, one of
    // them containing a space and both carrying capitals.
    client
        .batch_execute("INSERT INTO documents VALUES ('''New York'' ''Hello''')")
        .await
        .expect("insert the two lexemes");

    let mut stream = connect_wire(addr).await;
    let payloads = wire_copy_out(&mut stream, "COPY documents TO STDOUT").await;
    assert_eq!(
        payloads,
        vec!["'New York' 'Hello'\n".to_string()],
        "*** COPY TO STDOUT must print a tsvector column the way PostgreSQL \
         prints it — it emitted the JSON token array `[\"New York\",\"Hello\"]`, \
         which SELECT on the same column never did"
    );

    wire_copy_in(&mut stream, "COPY documents_reloaded FROM STDIN", &payloads).await;

    assert_eq!(
        simple_col0(&client, "SELECT search_vector FROM documents_reloaded").await,
        vec!["'New York' 'Hello'".to_string()],
        "*** COPY OUT → COPY IN must be an exact round trip; the JSON form \
         re-tokenised into three lower-cased tokens"
    );
    assert_eq!(
        simple_col0(&client, "SELECT search_vector FROM documents_reloaded").await,
        simple_col0(&client, "SELECT search_vector FROM documents").await,
        "the reloaded row holds the identical token set"
    );
    // And the reloaded value is still a working tsvector, matched on the
    // WHOLE two-word lexeme — the one thing a re-tokenisation destroys (it
    // would have split `New York` into `new` and `york`). The query literal
    // is the quoted-lexeme form too, so both sides stay verbatim.
    assert_eq!(
        simple_col0(
            &client,
            r#"SELECT count(*) FROM documents_reloaded WHERE search_vector @@ '''New York'''::tsquery"#
        )
        .await,
        vec!["1".to_string()],
        "*** the reloaded row must still match the two-word lexeme it was stored with"
    );
}
