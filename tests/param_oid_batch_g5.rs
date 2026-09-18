//! Batch G5 — parameter type OIDs on the extended query protocol, proven over
//! the REAL PostgreSQL wire.
//!
//! G5-1 — sprinter 6ac716be10ea (HIGH). `handle_parse_extended` answered
//!        `Describe(Statement)` with `vec![0i32; param_count]` for every `$N`
//!        the client did not type at Parse. PostgreSQL never sends 0 there — it
//!        reports what parse analysis RESOLVED — and for rust-postgres 0 is
//!        fatal rather than merely vague:
//!
//!          `tokio_postgres::prepare::prepare` resolves each parameter OID via
//!          `get_type`. `Type::from_oid(0)` is `None`, so it prepares its own
//!          `TYPEINFO_QUERY` (`… FROM pg_catalog.pg_type t … WHERE t.oid = $1`)
//!          to look 0 up — and Nano described THAT `$1` as 0 as well.
//!          `typeinfo_statement` caches the statement only AFTER `prepare_rec`
//!          returns, so the second lookup re-entered the first: unbounded
//!          recursion, client-side stack overflow, connection never answers.
//!
//!        Two whole test files were `#[ignore]`d on it — the ignore text blamed
//!        the SERVER ("the in-process `PgServer` … stack-overflows", "requires
//!        PostgreSQL wire protocol fixes"), which is why a 32 MB runtime stack
//!        in one of them never helped: the recursion was on the CLIENT side and
//!        no finite stack survives it. Both files run again as of this change:
//!        `tests/server_mode_integration_test.rs` (5 tests) and
//!        `tests/extended_query_param_select.rs` (3 tests).
//!
//! G5-2 — sprinter 671743292162 (MEDIUM), NOT shipped, and the last case here
//!        records why. G5-1 was supposed to unblock advertising `vector` under
//!        its registry OID 16385 instead of `text`. It does remove the
//!        recursion — but terminating is not succeeding, and a SECOND,
//!        independent blocker survives it: see
//!        `g5_typeinfo_row_is_not_pg_typed_so_vector_cannot_leave_text`.
//!
//! Everything below drives a real listener. The embedded API touches neither
//! `handler_extended.rs` nor `catalog.rs`, so a ParameterDescription claim
//! proven through `EmbeddedDatabase::query` proves nothing. The raw
//! `Parse/Describe/Bind/Execute/Sync` frames are how the OIDs themselves are
//! read (no driver can show you a ParameterDescription it has already
//! resolved), and `tokio_postgres` is how the recursion is proven gone.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use bytes::{BufMut, BytesMut};
use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::EmbeddedDatabase;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_postgres::types::Type;
use tokio_postgres::{Client, NoTls};

/// Read budget for one backend frame. Generous — nothing here is a timing test.
const FRAME_TIMEOUT: Duration = Duration::from_secs(10);

/// The exact statement `tokio_postgres::prepare` prepares to resolve an OID it
/// does not know natively (tokio-postgres 0.7.18, `src/prepare.rs`). Copied
/// verbatim because its SHAPE is the subject: one parameter, compared against
/// `pg_type.oid`.
const TYPEINFO_QUERY: &str = "SELECT t.typname, t.typtype, t.typelem, r.rngsubtype, t.typbasetype, n.nspname, \
                              t.typrelid FROM pg_catalog.pg_type t LEFT OUTER JOIN pg_catalog.pg_range r ON \
                              r.rngtypid = t.oid INNER JOIN pg_catalog.pg_namespace n ON t.typnamespace = n.oid WHERE \
                              t.oid = $1";

// ===========================================================================
// Raw frontend/backend frame plumbing (same shape as tests/pg_wire_batch_c.rs)
// ===========================================================================

fn put_cstr(buf: &mut BytesMut, value: &str) {
    buf.extend_from_slice(value.as_bytes());
    buf.put_u8(0);
}

fn frontend_message(tag: u8, body: BytesMut) -> BytesMut {
    let mut msg = BytesMut::new();
    msg.put_u8(tag);
    msg.put_i32((body.len() + 4) as i32);
    msg.extend_from_slice(&body);
    msg
}

fn startup_packet(user: &str, database: &str) -> BytesMut {
    let mut body = BytesMut::new();
    body.put_i32(196_608); // protocol 3.0
    put_cstr(&mut body, "user");
    put_cstr(&mut body, user);
    put_cstr(&mut body, "database");
    put_cstr(&mut body, database);
    body.put_u8(0);

    let mut msg = BytesMut::new();
    msg.put_i32((body.len() + 4) as i32);
    msg.extend_from_slice(&body);
    msg
}

async fn read_frame(stream: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    timeout(FRAME_TIMEOUT, stream.read_exact(&mut header))
        .await
        .ok()?
        .ok()?;
    let len = i32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    let body_len = usize::try_from(len - 4).ok()?;
    let mut body = vec![0u8; body_len];
    if body_len > 0 {
        timeout(FRAME_TIMEOUT, stream.read_exact(&mut body)).await.ok()?.ok()?;
    }
    Some((header[0], body))
}

async fn write_frames(stream: &mut TcpStream, frames: &[BytesMut]) {
    let mut out = BytesMut::new();
    for frame in frames {
        out.extend_from_slice(frame);
    }
    timeout(FRAME_TIMEOUT, stream.write_all(&out))
        .await
        .expect("write timeout")
        .expect("write");
}

async fn read_until_ready(stream: &mut TcpStream) -> Vec<(u8, Vec<u8>)> {
    let mut frames = Vec::new();
    while let Some(frame) = read_frame(stream).await {
        let tag = frame.0;
        frames.push(frame);
        if tag == b'Z' {
            break;
        }
    }
    frames
}

/// Parse, declaring `param_types` (empty = "server, you decide", which is what
/// every driver in this file's blast radius sends).
fn parse_message(statement: &str, sql: &str, param_types: &[i32]) -> BytesMut {
    let mut body = BytesMut::new();
    put_cstr(&mut body, statement);
    put_cstr(&mut body, sql);
    body.put_i16(param_types.len() as i16);
    for oid in param_types {
        body.put_i32(*oid);
    }
    frontend_message(b'P', body)
}

fn describe_message(kind: u8, name: &str) -> BytesMut {
    let mut body = BytesMut::new();
    body.put_u8(kind);
    put_cstr(&mut body, name);
    frontend_message(b'D', body)
}

/// Bind with explicit parameter values and formats (`0` text, `1` binary).
fn bind_message(portal: &str, statement: &str, formats: &[i16], params: &[&[u8]]) -> BytesMut {
    let mut body = BytesMut::new();
    put_cstr(&mut body, portal);
    put_cstr(&mut body, statement);
    body.put_i16(formats.len() as i16);
    for format in formats {
        body.put_i16(*format);
    }
    body.put_i16(params.len() as i16);
    for value in params {
        body.put_i32(value.len() as i32);
        body.extend_from_slice(value);
    }
    body.put_i16(0); // result formats: text everywhere
    frontend_message(b'B', body)
}

fn execute_message(portal: &str) -> BytesMut {
    let mut body = BytesMut::new();
    put_cstr(&mut body, portal);
    body.put_i32(0); // unlimited rows
    frontend_message(b'E', body)
}

fn sync_message() -> BytesMut {
    frontend_message(b'S', BytesMut::new())
}

/// A `ParameterDescription` (`t`) body: int16 count, then one int32 OID each.
fn parameter_description(body: &[u8]) -> Vec<i32> {
    let count = i16::from_be_bytes([body[0], body[1]]) as usize;
    (0..count)
        .map(|i| {
            let at = 2 + i * 4;
            i32::from_be_bytes(body[at..at + 4].try_into().expect("oid"))
        })
        .collect()
}

/// A `RowDescription` (`T`) body: `(name, type_oid)` per field.
fn row_description(body: &[u8]) -> Vec<(String, i32)> {
    let count = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut cursor = 2usize;
    let mut fields = Vec::with_capacity(count);
    for _ in 0..count {
        let end = body[cursor..].iter().position(|b| *b == 0).expect("field name") + cursor;
        let name = String::from_utf8_lossy(&body[cursor..end]).into_owned();
        cursor = end + 1;
        // table_oid(4) column_attr(2) type_oid(4) type_len(2) type_mod(4) format(2)
        let type_oid = i32::from_be_bytes(body[cursor + 6..cursor + 10].try_into().expect("oid"));
        cursor += 18;
        fields.push((name, type_oid));
    }
    fields
}

/// Every `DataRow` cell of the exchange, as text.
fn data_row_cells(frames: &[(u8, Vec<u8>)]) -> Vec<Vec<Option<String>>> {
    frames
        .iter()
        .filter(|(tag, _)| *tag == b'D')
        .map(|(_, body)| {
            let count = i16::from_be_bytes([body[0], body[1]]) as usize;
            let mut cursor = 2usize;
            let mut cells = Vec::with_capacity(count);
            for _ in 0..count {
                let len = i32::from_be_bytes(body[cursor..cursor + 4].try_into().expect("len"));
                cursor += 4;
                if len < 0 {
                    cells.push(None);
                } else {
                    let end = cursor + len as usize;
                    cells.push(Some(String::from_utf8_lossy(&body[cursor..end]).into_owned()));
                    cursor = end;
                }
            }
            cells
        })
        .collect()
}

fn error_sqlstates(frames: &[(u8, Vec<u8>)]) -> Vec<String> {
    frames
        .iter()
        .filter(|(tag, _)| *tag == b'E')
        .filter_map(|(_, body)| {
            body.split(|b| *b == 0)
                .find(|field| field.first() == Some(&b'C'))
                .map(|field| String::from_utf8_lossy(&field[1..]).into_owned())
        })
        .collect()
}

// ===========================================================================
// Harness
// ===========================================================================

/// Start a trust-auth listener over `db`, seeded by the caller BEFORE the
/// server starts.
async fn serve(db: Arc<EmbeddedDatabase>) -> (SocketAddr, String, tokio::task::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test port");
    let addr = listener.local_addr().expect("test addr");
    drop(listener);

    let config = PgServerConfig::with_address(addr);
    let server = PgServer::new(config, db).expect("server");
    let handle = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(150)).await;

    let conn_string = format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port());
    (addr, conn_string, handle)
}

async fn connect_client(conn_string: &str) -> (Client, tokio::task::JoinHandle<()>) {
    let (client, connection) = timeout(FRAME_TIMEOUT, tokio_postgres::connect(conn_string, NoTls))
        .await
        .expect("connect timeout")
        .expect("connect");
    let task = tokio::spawn(async move {
        let _ = connection.await;
    });
    (client, task)
}

/// A raw socket parked at ReadyForQuery.
async fn raw_session(addr: SocketAddr) -> TcpStream {
    let mut stream = timeout(FRAME_TIMEOUT, TcpStream::connect(addr))
        .await
        .expect("connect timeout")
        .expect("connect");
    write_frames(&mut stream, &[startup_packet("postgres", "postgres")]).await;
    loop {
        let (tag, _) = read_frame(&mut stream).await.expect("startup frame");
        if tag == b'Z' {
            return stream;
        }
        assert_ne!(tag, b'E', "trust startup must not fail");
    }
}

/// THE probe: Parse `sql` with no declared parameter types, then read the OIDs
/// `Describe(Statement)` answers with.
async fn described_param_oids(stream: &mut TcpStream, name: &str, sql: &str) -> Vec<i32> {
    write_frames(
        stream,
        &[
            parse_message(name, sql, &[]),
            describe_message(b'S', name),
            sync_message(),
        ],
    )
    .await;
    let frames = read_until_ready(stream).await;
    assert!(
        error_sqlstates(&frames).is_empty(),
        "`{sql}` must Parse/Describe cleanly, got {:?}",
        error_sqlstates(&frames)
    );
    let (_, body) = frames
        .iter()
        .find(|(tag, _)| *tag == b't')
        .unwrap_or_else(|| panic!("no ParameterDescription for `{sql}`"));
    parameter_description(body)
}

fn users_db() -> Arc<EmbeddedDatabase> {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    db.execute("CREATE TABLE g5_users (id INT PRIMARY KEY, name TEXT, note TEXT)")
        .expect("create");
    db.execute("INSERT INTO g5_users VALUES (1, 'alice', 'first')")
        .expect("seed");
    db.execute("INSERT INTO g5_users VALUES (2, 'bob', 'second')")
        .expect("seed");
    db
}

// ===========================================================================
// G5-1 — the OIDs themselves
// ===========================================================================

/// THE defect, read straight off the wire.
///
/// Before the fix EVERY vector below was `[0]` / `[0, 0]`: `handle_parse_
/// extended` filled `param_types` with `vec![0i32; param_count]` whenever the
/// client declared none. Now each parameter reports the type its POSITION
/// determines — the compared column for a WHERE, the target column for an
/// INSERT position or an UPDATE assignment.
#[tokio::test]
async fn g5_inferred_parameter_oids_come_from_the_statement_not_zero() {
    let (addr, _conn, _server) = serve(users_db()).await;
    let mut stream = raw_session(addr).await;

    let cases: &[(&str, &str, Vec<i32>)] = &[
        ("g5_sel", "SELECT name FROM g5_users WHERE id = $1", vec![23]),
        ("g5_sel_txt", "SELECT id FROM g5_users WHERE name = $1", vec![25]),
        // Qualified by table name, and with the parameter on the LEFT — the
        // Drizzle/`postgres-js` shape.
        (
            "g5_sel_q",
            r#"SELECT "id" FROM "g5_users" WHERE $1 = "g5_users"."name""#,
            vec![25],
        ),
        // INSERT types by POSITION against the named column list …
        (
            "g5_ins",
            "INSERT INTO g5_users (id, name) VALUES ($1, $2)",
            vec![23, 25],
        ),
        // … and against the full declaration order when none is named.
        (
            "g5_ins_bare",
            "INSERT INTO g5_users VALUES ($1, $2, $3)",
            vec![23, 25, 25],
        ),
        // UPDATE: the SET target types the assignment, the WHERE the predicate.
        ("g5_upd", "UPDATE g5_users SET name = $1 WHERE id = $2", vec![25, 23]),
        ("g5_del", "DELETE FROM g5_users WHERE id = $1", vec![23]),
        ("g5_in", "SELECT id FROM g5_users WHERE id IN ($1, $2)", vec![23, 23]),
        (
            "g5_between",
            "SELECT id FROM g5_users WHERE id BETWEEN $1 AND $2",
            vec![23, 23],
        ),
        ("g5_like", "SELECT id FROM g5_users WHERE name LIKE $1", vec![25]),
        ("g5_cast", "SELECT $1::bigint", vec![20]),
    ];

    for (name, sql, expected) in cases {
        let oids = described_param_oids(&mut stream, name, sql).await;
        assert_eq!(
            &oids, expected,
            "*** `{sql}` described its parameters as {oids:?}, expected {expected:?}"
        );
        assert!(
            !oids.contains(&0),
            "*** OID 0 in a ParameterDescription is what recursed tokio-postgres into its \
             TYPEINFO lookup — `{sql}`"
        );
    }
}

/// A type the CLIENT declared is authoritative and passes through untouched;
/// only the 0s it left for the server are resolved. psycopg3 and the JDBC
/// driver both declare their own OIDs, and inference must never second-guess
/// them — the client has already encoded the value in that type's format.
#[tokio::test]
async fn g5_client_declared_parameter_types_are_never_overridden() {
    let (addr, _conn, _server) = serve(users_db()).await;
    let mut stream = raw_session(addr).await;

    // `id` is int4; the client insists on int8. Its word stands.
    write_frames(
        &mut stream,
        &[
            parse_message("g5_declared", "SELECT name FROM g5_users WHERE id = $1", &[20]),
            describe_message(b'S', "g5_declared"),
            sync_message(),
        ],
    )
    .await;
    let frames = read_until_ready(&mut stream).await;
    let (_, body) = frames
        .iter()
        .find(|(tag, _)| *tag == b't')
        .expect("ParameterDescription");
    assert_eq!(parameter_description(body), vec![20], "a declared OID is authoritative");

    // A MIXED list: `$1` declared, `$2` left at 0 for the server. PostgreSQL
    // resolves exactly the zeros, and so does Nano — without changing the
    // LENGTH, which `handle_bind_extended` validates the client's count against.
    write_frames(
        &mut stream,
        &[
            parse_message("g5_mixed", "UPDATE g5_users SET note = $1 WHERE id = $2", &[1043, 0]),
            describe_message(b'S', "g5_mixed"),
            sync_message(),
        ],
    )
    .await;
    let frames = read_until_ready(&mut stream).await;
    let (_, body) = frames
        .iter()
        .find(|(tag, _)| *tag == b't')
        .expect("ParameterDescription");
    assert_eq!(
        parameter_description(body),
        vec![1043, 23],
        "*** the declared varchar must survive and only the 0 may be resolved"
    );
}

/// The case PostgreSQL itself declines to type (`42P18 could not determine data
/// type of parameter $1`). Nano answers `705` — `unknown` — rather than
/// guessing, because a WRONG OID is worse than an honest one: the client then
/// encodes the value in the wrong wire format. 705 is a driver BUILTIN, so it
/// costs no lookup; 0 is not, which is the whole defect.
#[tokio::test]
async fn g5_parameter_without_type_context_is_unknown_not_zero() {
    assert!(
        Type::from_oid(705).is_some(),
        "705 must be resolvable by the driver without asking the server"
    );
    assert!(
        Type::from_oid(0).is_none(),
        "0 is not — which is why `get_type(0)` prepared TYPEINFO and recursed"
    );

    let (addr, _conn, _server) = serve(users_db()).await;
    let mut stream = raw_session(addr).await;

    for (name, sql) in [
        ("g5_bare", "SELECT $1"),
        ("g5_fn", "SELECT pg_try_advisory_lock($1)"),
        ("g5_unknown_rel", "SELECT x FROM g5_nosuchtable WHERE x = $1"),
        // Arithmetic is NOT type-preserving in PostgreSQL (`int4 / unknown` is
        // `numeric`), so the inference declines rather than guessing `int4`.
        ("g5_arith", "SELECT id FROM g5_users WHERE id + $1 > 5"),
    ] {
        assert_eq!(
            described_param_oids(&mut stream, name, sql).await,
            vec![705],
            "*** `{sql}` has no type context — it must report `unknown`, never 0 and never a guess"
        );
    }
}

/// The rule that actually breaks the recursion cycle at its source.
///
/// tokio-postgres' own TYPEINFO statement filters `WHERE t.oid = $1` and binds
/// the value as an `Oid`: `impl ToSql for u32` accepts `Type::OID` (26) and
/// NOTHING else. Nano stores catalogue OID columns as `int4` (its `DataType`
/// has no `oid` variant), so describing that parameter honestly as 23 would
/// still fail the driver with `WrongType`. A `pg_catalog` relation's OID
/// columns therefore report 26 — PostgreSQL's own answer — while its ordinary
/// columns keep their real types.
#[tokio::test]
async fn g5_catalog_oid_column_is_described_as_oid_not_int4() {
    let (addr, _conn, _server) = serve(users_db()).await;
    let mut stream = raw_session(addr).await;

    assert_eq!(
        described_param_oids(&mut stream, "g5_typeinfo", TYPEINFO_QUERY).await,
        vec![26],
        "*** tokio-postgres' own TYPEINFO parameter must be `oid` (26)"
    );
    assert_eq!(
        described_param_oids(
            &mut stream,
            "g5_typeinfo_name",
            "SELECT oid FROM pg_catalog.pg_type WHERE typname = $1"
        )
        .await,
        vec![25],
        "a text column of the same catalogue relation is still text"
    );
    // A user table named `oid`-ish gets no special treatment: the rule is
    // scoped to relations resolved from the system-view registry.
    assert_eq!(
        described_param_oids(&mut stream, "g5_user_int", "SELECT name FROM g5_users WHERE id = $1").await,
        vec![23],
        "a user int4 column must stay int4"
    );
}

/// …and the OID it describes must be one the SERVER can then decode. A
/// parameter typed 26 arrives in BINARY from every rust-postgres client (they
/// bind format 1 unconditionally), which used to land in
/// `decode_binary_parameter`'s catch-all as `Value::Bytes` and match no row.
#[tokio::test]
async fn g5_oid_parameter_round_trips_in_both_wire_formats() {
    let (addr, _conn, _server) = serve(users_db()).await;
    let mut stream = raw_session(addr).await;

    const SQL: &str = "SELECT typname FROM pg_catalog.pg_type WHERE oid = $1";

    // Binary: 4 bytes big-endian, unsigned — `postgres_protocol`'s `oid_to_sql`.
    write_frames(
        &mut stream,
        &[
            parse_message("g5_oid_bin", SQL, &[]),
            bind_message("g5_oid_bin_p", "g5_oid_bin", &[1], &[&23i32.to_be_bytes()]),
            execute_message("g5_oid_bin_p"),
            sync_message(),
        ],
    )
    .await;
    let frames = read_until_ready(&mut stream).await;
    assert!(
        error_sqlstates(&frames).is_empty(),
        "a binary oid parameter must not error: {:?}",
        error_sqlstates(&frames)
    );
    assert_eq!(
        data_row_cells(&frames),
        vec![vec![Some("int4".to_string())]],
        "*** a BINARY `oid` parameter must decode to the OID it carries (23 → int4)"
    );

    // Text: the same query, the same answer.
    write_frames(
        &mut stream,
        &[
            parse_message("g5_oid_txt", SQL, &[]),
            bind_message("g5_oid_txt_p", "g5_oid_txt", &[0], &[b"25"]),
            execute_message("g5_oid_txt_p"),
            sync_message(),
        ],
    )
    .await;
    let frames = read_until_ready(&mut stream).await;
    assert!(
        error_sqlstates(&frames).is_empty(),
        "a text oid parameter must not error: {:?}",
        error_sqlstates(&frames)
    );
    assert_eq!(
        data_row_cells(&frames),
        vec![vec![Some("text".to_string())]],
        "*** and a TEXT `oid` parameter must decode the same way (25 → text)"
    );
}

/// THE INVARIANT. Every OID this inference can emit must be one
/// `Type::from_oid` answers LOCALLY — otherwise the driver goes straight back
/// into the server-side TYPEINFO lookup this item exists to stop, and a fix
/// that swapped one unresolvable OID (0) for another would be no fix at all.
///
/// Driven through a table with one column per declared type, so a future
/// `datatype_to_oid` arm that returns something exotic fails HERE rather than
/// hanging a user's driver.
#[tokio::test]
async fn g5_every_inferable_parameter_oid_is_a_driver_builtin() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    db.execute(
        "CREATE TABLE g5_types (
             c_bool BOOLEAN, c_i2 SMALLINT, c_i4 INT, c_i8 BIGINT,
             c_f4 REAL, c_f8 DOUBLE PRECISION, c_num NUMERIC,
             c_text TEXT, c_vc VARCHAR(8), c_ch CHAR(4), c_bytea BYTEA,
             c_date DATE, c_ts TIMESTAMP, c_uuid UUID, c_jsonb JSONB
         )",
    )
    .expect("create");
    let (addr, _conn, _server) = serve(db).await;
    let mut stream = raw_session(addr).await;

    let oids = described_param_oids(
        &mut stream,
        "g5_all_types",
        "INSERT INTO g5_types VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
    )
    .await;

    assert_eq!(
        oids,
        vec![16, 21, 23, 20, 700, 701, 1700, 25, 1043, 1042, 17, 1082, 1114, 2950, 3802],
        "*** each INSERT position must take its target column's PostgreSQL type"
    );
    for oid in &oids {
        assert!(
            Type::from_oid(*oid as u32).is_some(),
            "*** OID {oid} is not a driver builtin — advertising it sends every rust-postgres \
             client back into the TYPEINFO lookup 6ac716be10ea removed"
        );
    }
}

// ===========================================================================
// G5-1 — the recursion, through the driver that used to fall into it
// ===========================================================================

/// The end-to-end proof, and the shape the item asked for.
///
/// On the pre-fix tree this does not fail — it ABORTS the test binary. The
/// recursion is unbounded, so `prepare` overflows the worker thread's stack
/// before any assertion runs. (That is exactly what made the two `#[ignore]`d
/// files look like a SERVER bug, and why the 32 MB runtime stack one of them
/// builds never rescued it.)
#[tokio::test]
async fn g5_tokio_postgres_query_with_a_bound_parameter_returns_rows() {
    let (_addr, conn_string, _server) = serve(users_db()).await;
    let (client, _task) = connect_client(&conn_string).await;

    let rows = timeout(
        FRAME_TIMEOUT,
        client.query("SELECT name FROM g5_users WHERE id = $1", &[&1i32]),
    )
    .await
    .expect("*** `query` never returned — the TYPEINFO recursion is back")
    .expect("query must succeed");

    assert_eq!(rows.len(), 1, "one row for id = 1");
    assert_eq!(rows[0].get::<_, String>(0), "alice");

    // `prepare` is where the recursion actually lived: it is the call that
    // resolves every parameter OID through `get_type`.
    let stmt = timeout(
        FRAME_TIMEOUT,
        client.prepare("UPDATE g5_users SET name = $1 WHERE id = $2"),
    )
    .await
    .expect("*** `prepare` never returned")
    .expect("prepare must succeed");
    assert_eq!(
        stmt.params(),
        &[Type::TEXT, Type::INT4],
        "*** the driver must see the resolved types, not `unknown`"
    );

    // And the resolved types are usable: the driver encodes each value in the
    // type the server named, which only works if that type is right.
    let updated = timeout(FRAME_TIMEOUT, client.execute(&stmt, &[&"alice-renamed", &1i32]))
        .await
        .expect("execute timeout")
        .expect("execute must succeed");
    assert_eq!(updated, 1, "one row updated");
}

// ===========================================================================
// G5-2 — sprinter 671743292162: why `vector` still cannot leave `text`
// ===========================================================================

/// NOT-SHIPPED RECORD for sprinter 671743292162, with the evidence inline.
///
/// The item's premise is that G5-1 unblocks advertising `vector` under its
/// registry OID 16385 on the wire. G5-1 does remove the RECURSION — but a
/// non-builtin RowDescription OID still FORCES tokio-postgres to run its
/// TYPEINFO query, and that query has to SUCCEED, not merely terminate. It
/// cannot, for a reason that has nothing to do with parameters:
///
///   tokio-postgres reads its TYPEINFO row as `typname: String`,
///   `typtype: i8`, `typelem: Oid`, `rngsubtype: Option<Oid>`,
///   `typbasetype: Oid`, `nspname: String`, `typrelid: Oid` — and
///   `impl FromSql for i8` accepts EXACTLY `Type::CHAR` (18) while
///   `impl FromSql for u32` accepts EXACTLY `Type::OID` (26). `Row::get_inner`
///   checks `accepts` before decoding, NULL or not, so a column advertised as
///   anything else is `WrongType` and takes the user's query down with it.
///
/// Nano's `pg_type` / `pg_range` views declare those columns `Text` and `Int4`
/// (`sql::phase3::system_views`), because `crate::DataType` has no `char` or
/// `oid` variant — which this case pins below. Shipping 16385 therefore needs
/// those catalogue columns typed as PostgreSQL types FIRST: a `DataType`
/// change, not a wire change. Until then `text` (25) is both correct and free
/// — it is literally what the value on the wire is, and it needs no lookup at
/// all. Advertising 16385 now would trade a working query for a failing one.
#[tokio::test]
async fn g5_typeinfo_row_is_not_pg_typed_so_vector_cannot_leave_text() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    db.execute("CREATE TABLE g5_v (e VECTOR(3))").expect("create");
    db.execute("INSERT INTO g5_v VALUES ('[1,2,3]')").expect("seed");
    let (addr, conn_string, _server) = serve(db).await;
    let mut stream = raw_session(addr).await;

    // 1. The TYPEINFO result columns a 16385 RowDescription would force the
    //    driver to read. `typtype` must be `char` (18) and the four OID columns
    //    `oid` (26) before 16385 can ship; they are `text` and `int4` today.
    write_frames(
        &mut stream,
        &[
            parse_message("g5_ti", TYPEINFO_QUERY, &[]),
            describe_message(b'S', "g5_ti"),
            sync_message(),
        ],
    )
    .await;
    let frames = read_until_ready(&mut stream).await;
    let (_, body) = frames
        .iter()
        .find(|(tag, _)| *tag == b'T')
        .expect("TYPEINFO must describe a row");
    let fields = row_description(body);
    let oid_of = |name: &str| -> i32 {
        fields
            .iter()
            .find(|(field, _)| field.eq_ignore_ascii_case(name))
            .map(|(_, oid)| *oid)
            .unwrap_or_else(|| panic!("TYPEINFO must project `{name}`, got {fields:?}"))
    };
    // Asserted as NOT-the-required-type rather than as a literal, so the case
    // holds whichever schema-derivation path answers — and so that the day
    // these DO become 18 / 26 this test FAILS and forces 671743292162 to be
    // reopened rather than quietly staying closed.
    assert_ne!(
        oid_of("typtype"),
        18,
        "BLOCKER LIFTED: typtype is `char` now — tokio-postgres reads it as `i8`, which accepts \
         only 18, so the TYPEINFO lookup a 16385 RowDescription forces can finally succeed. \
         Re-open sprinter 671743292162."
    );
    assert_ne!(
        oid_of("typelem"),
        26,
        "BLOCKER LIFTED: typelem is `oid` now — tokio-postgres reads it as `Oid`, which accepts \
         only 26. Re-open sprinter 671743292162."
    );

    // 2. So the wire keeps advertising `text` for a vector column …
    write_frames(
        &mut stream,
        &[
            parse_message("g5_vec", "SELECT e FROM g5_v", &[]),
            describe_message(b'S', "g5_vec"),
            sync_message(),
        ],
    )
    .await;
    let frames = read_until_ready(&mut stream).await;
    let (_, body) = frames.iter().find(|(tag, _)| *tag == b'T').expect("RowDescription");
    assert_eq!(
        row_description(body),
        vec![("e".to_string(), 25)],
        "*** a vector column stays `text` (25) — NOT 16385, and NOT the 1000 (`_bool`!) it \
         advertised before HDB-002"
    );

    // 3. … which is what keeps this query working at all through the driver.
    let (client, _task) = connect_client(&conn_string).await;
    let stmt = timeout(FRAME_TIMEOUT, client.prepare("SELECT e FROM g5_v"))
        .await
        .expect("*** prepare over a vector column must not hang")
        .expect("prepare must succeed");
    assert_eq!(stmt.columns()[0].type_(), &Type::TEXT);

    // 4. And the registry still answers 16385 BY NAME, which is how a pgvector
    //    client resolves an extension type — the surface HDB-002 fixed, and the
    //    one that stays consistent whatever the wire advertises.
    let rows = timeout(
        FRAME_TIMEOUT,
        client.query(
            "SELECT typname, oid, typarray FROM pg_type WHERE typname = 'vector'",
            &[],
        ),
    )
    .await
    .expect("query timeout")
    .expect("pgvector's register_vector shape must resolve");
    assert_eq!(rows.len(), 1, "exactly one `vector` row");
    assert_eq!(rows[0].get::<_, i32>(1), 16385, "`vector` keeps its user-band OID");
    assert_eq!(rows[0].get::<_, i32>(2), 16386, "`_vector` follows it");
}
