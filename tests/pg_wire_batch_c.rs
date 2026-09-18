//! Batch C — three sprinter items, all proven over the REAL PostgreSQL wire.
//!
//! The embedded API touches neither `src/protocol/postgres/catalog.rs` nor the
//! startup handler, so an introspection or handshake claim proven through
//! `EmbeddedDatabase::query` proves nothing about what psql or libpq sees.
//! Every assertion below therefore runs against a real listener: the raw
//! `PgConnectionHandler` over a TCP socket for the startup handshake (C1, where
//! the messages BEFORE ReadyForQuery are the whole subject), and the in-process
//! `PgServer` for the catalog and extended-protocol items (C2, C3).
//!
//! What each item is, and what it looked like before:
//!
//! C1 — sprinter c5afe5e41eac (SECURITY). `handle_startup` validated the
//!      `database` startup parameter BEFORE sending a single authentication
//!      message. An unknown name got a FATAL that named the condition and ZERO
//!      `R` messages; a known one got the full SCRAM/password exchange. With
//!      one tenant per customer — and libpq defaulting `dbname` to the user
//!      name — that is tenant/account enumeration at one connection per guess,
//!      no credential required. PostgreSQL resolves the database in
//!      `InitPostgres`, AFTER `PerformAuthentication`.
//!
//! C2 — sprinter cec8ab163448. A `CREATE INDEX … USING HNSW` index was
//!      invisible to `pg_class`, to psql's `\d <table>` and to `\di`, because
//!      every one of those surfaces enumerated the ART index registry
//!      (`storage.art_indexes()`) or the table schema's PK/UNIQUE flags, while
//!      vector indexes live in a SEPARATE registry
//!      (`storage.vector_indexes()`).
//!
//! C3 — sprinter 84ac05bd69c4. Three claims about RETURNING residuals, pinned
//!      here end-to-end rather than re-read.
//!      (1) expression items typed `text` — already fixed; `returning_schema`
//!          delegates to `ReturningProjection::bind`, which types an expression
//!          exactly as the SELECT list would.
//!      (2) unknown columns failing open to NULL — already fixed: the list is
//!          bound ONCE, before the first write, on every route, so the refusal
//!          is 42703 and nothing is written. Writing this test did surface a
//!          residual the item did not name — the EXTENDED protocol defers that
//!          refusal from Parse to Execute, where PostgreSQL raises it during
//!          parse analysis — which is documented at the test and filed
//!          separately (it has to move together with the 42803 / 42P20
//!          siblings that `wire_tests::gh23_c3_*` pins at Execute).
//!      (3) `Describe(Statement)` reporting format code 0 — NOT a bug. Real
//!          PostgreSQL passes a NULL `formats` array there, because no portal
//!          exists yet. Pinned as correct.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use bytes::{BufMut, BytesMut};
use heliosdb_nano::protocol::postgres::handler::PgConnectionHandler;
use heliosdb_nano::protocol::postgres::password_store::{InMemoryPasswordStore, SharedPasswordStore};
use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::protocol::postgres::{AuthManager, AuthMethod};
use heliosdb_nano::{EmbeddedDatabase, Value};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

/// Read budget for one backend frame. Generous — nothing here is a timing test.
const FRAME_TIMEOUT: Duration = Duration::from_secs(10);

// ===========================================================================
// Raw frontend/backend frame plumbing (shared by C1 and C3)
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

/// A v3 StartupMessage carrying `user`, and `database` only when `database` is
/// `Some` — the "no dbname at all" case is exactly the libpq default that makes
/// C1's oracle an ACCOUNT oracle rather than merely a tenant one.
fn startup_packet(user: &str, database: Option<&str>) -> BytesMut {
    let mut body = BytesMut::new();
    body.put_i32(196_608); // protocol 3.0
    put_cstr(&mut body, "user");
    put_cstr(&mut body, user);
    if let Some(name) = database {
        put_cstr(&mut body, "database");
        put_cstr(&mut body, name);
    }
    body.put_u8(0);

    let mut msg = BytesMut::new();
    msg.put_i32((body.len() + 4) as i32);
    msg.extend_from_slice(&body);
    msg
}

/// One backend frame, or `None` on EOF / timeout.
async fn read_frame<S: AsyncRead + Unpin>(stream: &mut S) -> Option<(u8, Vec<u8>)> {
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

async fn write_frames<S: AsyncWrite + Unpin>(stream: &mut S, frames: &[BytesMut]) {
    let mut out = BytesMut::new();
    for frame in frames {
        out.extend_from_slice(frame);
    }
    timeout(FRAME_TIMEOUT, stream.write_all(&out))
        .await
        .expect("write timeout")
        .expect("write");
}

/// Split an ErrorResponse/NoticeResponse body into its typed fields
/// (`C` = SQLSTATE, `M` = message, `S`/`V` = severity).
fn error_fields(payload: &[u8]) -> BTreeMap<char, String> {
    payload
        .split(|b| *b == 0)
        .filter(|f| !f.is_empty())
        .map(|f| (char::from(f[0]), String::from_utf8_lossy(&f[1..]).into_owned()))
        .collect()
}

// ===========================================================================
// C1 — sprinter c5afe5e41eac: the startup exchange an UNAUTHENTICATED peer
// sees must not depend on whether the database it named exists.
// ===========================================================================

/// Everything one login attempt exposes to the peer that made it. Deliberately
/// the same shape as `tests/security_hdb_001.rs`'s `Exchange`: HDB-001 closed
/// the same oracle in the `user` dimension, and this is its `database` sibling.
#[derive(Debug)]
struct Exchange {
    /// The `int32` type code of every `Authentication` (`R`) message, in order.
    /// THE observable the old ordering changed: an unknown database produced an
    /// EMPTY vector where a known one produced `[3]` / `[3, 0]`.
    auth_messages: Vec<u32>,
    error: BTreeMap<char, String>,
    ready: bool,
}

/// Drive one full startup + cleartext-password attempt against a real handler
/// over a real socket.
///
/// The cleartext arm (not SCRAM) is deliberate: C1 is about MESSAGE ORDERING,
/// not about SCRAM's cryptography, and cleartext gives the shortest exchange in
/// which "was an authentication request sent at all?" is unambiguous.
async fn startup_attempt(
    auth: &Arc<AuthManager>,
    db: &Arc<EmbeddedDatabase>,
    user: &str,
    database: Option<&str>,
    password: &str,
) -> Exchange {
    let (auth, db) = (Arc::clone(auth), Arc::clone(db));
    timeout(Duration::from_secs(20), async move {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
        let mut client = TcpStream::connect(listener.local_addr().expect("address"))
            .await
            .expect("connect");
        let (socket, _) = listener.accept().await.expect("accept");
        let mut handler = PgConnectionHandler::new(socket, db, auth, None);
        let server = tokio::spawn(async move { Box::pin(handler.handle()).await });

        write_frames(&mut client, &[startup_packet(user, database)]).await;

        let mut result = Exchange {
            auth_messages: Vec::new(),
            error: BTreeMap::new(),
            ready: false,
        };
        while let Some((tag, payload)) = read_frame(&mut client).await {
            match tag {
                b'R' => {
                    let kind = u32::from_be_bytes(payload[..4].try_into().expect("auth kind"));
                    result.auth_messages.push(kind);
                    if kind == 3 {
                        // AuthenticationCleartextPassword → PasswordMessage.
                        let mut body = BytesMut::new();
                        put_cstr(&mut body, password);
                        write_frames(&mut client, &[frontend_message(b'p', body)]).await;
                    }
                }
                b'E' => {
                    // A FATAL during startup is the LAST message: PostgreSQL
                    // never follows it with a ReadyForQuery and neither do we,
                    // so breaking here is not a truncated read.
                    result.error = error_fields(&payload);
                    break;
                }
                b'Z' => {
                    result.ready = true;
                    write_frames(&mut client, &[frontend_message(b'X', BytesMut::new())]).await;
                    break;
                }
                b'S' | b'K' | b'N' => (),
                other => panic!("unexpected backend message {}", other as char),
            }
        }
        drop(client);
        let server_result = server.await.expect("handler must not panic");
        assert_eq!(
            server_result.is_ok(),
            result.ready,
            "only an exchange that reached ReadyForQuery may end successfully"
        );
        result
    })
    .await
    .expect("startup exchange must finish")
}

/// A database with one registered tenant, and a cleartext-password listener
/// that knows exactly one account.
fn tenant_db_and_auth() -> (Arc<EmbeddedDatabase>, Arc<AuthManager>) {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    db.execute("CREATE DATABASE tenant_acme").expect("create tenant");
    let store = SharedPasswordStore::new(InMemoryPasswordStore::new());
    store.add_user("alice", "correct-password").expect("register user");
    let auth = Arc::new(AuthManager::with_password_store(AuthMethod::CleartextPassword, store));
    (db, auth)
}

/// THE defect. One account name, one wrong password, three database names: one
/// reserved, one a real tenant, one that has never existed. An unauthenticated
/// peer must not be able to tell them apart.
///
/// FAILS on the pre-fix tree: the nonexistent name short-circuits before the
/// authentication request, so `auth_messages` is `[]` instead of `[3]` and the
/// error is `08P01` / `Protocol error: database "…" does not exist` instead of
/// `28P01` / `password authentication failed for user "alice"` — an oracle in
/// the message count, the SQLSTATE and the text at once.
#[tokio::test]
async fn database_existence_is_invisible_to_an_unauthenticated_peer() {
    let (db, auth) = tenant_db_and_auth();

    let reserved = startup_attempt(&auth, &db, "alice", Some("heliosdb"), "wrong-password").await;
    let existing = startup_attempt(&auth, &db, "alice", Some("tenant_acme"), "wrong-password").await;
    let absent = startup_attempt(&auth, &db, "alice", Some("tenant_globex"), "wrong-password").await;

    assert_eq!(
        reserved.auth_messages,
        vec![3],
        "a failing cleartext login is exactly one AuthenticationCleartextPassword"
    );
    assert_eq!(
        existing.auth_messages, reserved.auth_messages,
        "a registered tenant must not change the authentication-message sequence"
    );
    assert_eq!(
        absent.auth_messages, reserved.auth_messages,
        "a database that does not exist must not be answered before authentication"
    );

    assert_eq!(reserved.error.get(&'C').map(String::as_str), Some("28P01"));
    assert_eq!(
        existing.error, reserved.error,
        "byte-identical: the database name is not echoed at all on a credential rejection"
    );
    assert_eq!(
        absent.error, reserved.error,
        "an absent database and an existing one must produce the SAME rejection"
    );
    assert_eq!(
        reserved.error.get(&'M').map(String::as_str),
        Some("password authentication failed for user \"alice\"")
    );
    assert!(
        reserved
            .error
            .values()
            .all(|field| !field.contains("does not exist") && !field.contains("tenant")),
        "the pre-auth rejection must not name the catalogue condition"
    );
    assert!(!reserved.ready && !existing.ready && !absent.ready);
}

/// The same oracle in the shape that actually ships: libpq defaults `dbname` to
/// the user name, so a startup packet with NO `database` parameter probes the
/// USER name against the tenant list. Before the fix an unauthenticated peer
/// could therefore enumerate ACCOUNTS (whichever of them have a tenant) by
/// varying `user` alone and counting `R` messages.
#[tokio::test]
async fn omitted_dbname_does_not_leak_the_tenant_list_either() {
    let (db, auth) = tenant_db_and_auth();

    // `tenant_acme` IS a database; `tenant_globex` is not. Neither is a
    // registered account, so both are wrong-password rejections either way.
    let names_a_tenant = startup_attempt(&auth, &db, "tenant_acme", None, "guess").await;
    let names_nothing = startup_attempt(&auth, &db, "tenant_globex", None, "guess").await;

    assert_eq!(names_a_tenant.auth_messages, vec![3]);
    assert_eq!(
        names_nothing.auth_messages, names_a_tenant.auth_messages,
        "the user-name fallback must not answer before authentication"
    );
    assert_eq!(names_a_tenant.error.get(&'C'), names_nothing.error.get(&'C'));

    // Across two different names only the echoed name itself may differ — the
    // peer put it in the packet, so it already knows it. (Same normalisation
    // trick as tests/security_hdb_001.rs.)
    let renamed: BTreeMap<char, String> = names_nothing
        .error
        .iter()
        .map(|(field, value)| (*field, value.replace("tenant_globex", "tenant_acme")))
        .collect();
    assert_eq!(
        renamed, names_a_tenant.error,
        "beyond the echoed user name nothing may distinguish a tenant from a non-tenant"
    );
    assert!(!names_a_tenant.ready && !names_nothing.ready);
}

/// The fix must not become "the database name is no longer checked". An
/// AUTHENTICATED peer still cannot open a connection to a database that does
/// not exist — and the refusal is now PostgreSQL's own class for the condition,
/// `ERRCODE_UNDEFINED_DATABASE` (3D000), delivered AFTER `AuthenticationOk` and
/// BEFORE any ParameterStatus, exactly where `InitPostgres` raises it.
#[tokio::test]
async fn an_authenticated_peer_is_still_refused_an_unknown_database() {
    let (db, auth) = tenant_db_and_auth();

    let refused = startup_attempt(&auth, &db, "alice", Some("tenant_globex"), "correct-password").await;

    assert_eq!(
        refused.auth_messages,
        vec![3, 0],
        "the challenge AND AuthenticationOk must precede the refusal — the check is post-auth"
    );
    assert!(!refused.ready, "an unknown database must not reach ReadyForQuery");
    assert_eq!(
        refused.error.get(&'C').map(String::as_str),
        Some("3D000"),
        "PostgreSQL answers ERRCODE_UNDEFINED_DATABASE, not a protocol violation"
    );
    assert_eq!(
        refused.error.get(&'M').map(String::as_str),
        Some("database \"tenant_globex\" does not exist"),
        "PostgreSQL's own wording, with no `Protocol error: ` wrapper prefix"
    );
    assert_eq!(refused.error.get(&'S').map(String::as_str), Some("FATAL"));
}

/// Positive control: the uniformity above must not be uniform failure. A real
/// credential against a reserved name, a registered tenant and the libpq
/// `dbname`-defaults-to-user shape all reach ReadyForQuery.
#[tokio::test]
async fn valid_credentials_still_open_every_real_database() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    db.execute("CREATE DATABASE tenant_acme").expect("create tenant");
    let store = SharedPasswordStore::new(InMemoryPasswordStore::new());
    store.add_user("alice", "correct-password").expect("register alice");
    // An account whose NAME is a database, for the omitted-`dbname` path.
    store
        .add_user("tenant_acme", "correct-password")
        .expect("register tenant");
    let auth = Arc::new(AuthManager::with_password_store(AuthMethod::CleartextPassword, store));

    for (user, database) in [
        ("alice", Some("heliosdb")),
        ("alice", Some("postgres")),
        ("alice", Some("tenant_acme")),
        ("tenant_acme", None),
    ] {
        let ok = startup_attempt(&auth, &db, user, database, "correct-password").await;
        assert_eq!(
            ok.auth_messages,
            vec![3, 0],
            "user={user} database={database:?} must be challenged and accepted"
        );
        assert!(
            ok.ready && ok.error.is_empty(),
            "user={user} database={database:?} must reach ReadyForQuery, got {ok:?}"
        );
    }
}

// ===========================================================================
// In-process `PgServer` harness (C2, C3) — same shape as
// tests/pg_wire_hygiene_batch_c.rs.
// ===========================================================================

/// Start a trust-auth listener over `db` and return its address plus a libpq
/// connection string. The database is seeded by the caller BEFORE the server
/// starts, so every catalogue read below goes through the wire and not through
/// the embedded API that would bypass `catalog.rs` entirely.
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

/// Every `DataRow` of a simple query, as `column name -> value` maps.
async fn simple_rows(client: &Client, sql: &str) -> Vec<BTreeMap<String, String>> {
    let messages = timeout(FRAME_TIMEOUT, client.simple_query(sql))
        .await
        .expect("query timeout")
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
    messages
        .into_iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(row) => Some(
                (0..row.columns().len())
                    .map(|i| {
                        (
                            row.columns()[i].name().to_string(),
                            row.get(i).unwrap_or("").to_string(),
                        )
                    })
                    .collect(),
            ),
            _ => None,
        })
        .collect()
}

// ===========================================================================
// C2 — sprinter cec8ab163448: HNSW indexes must be visible in the catalogue.
// ===========================================================================

const HNSW_TABLE: &str = "c2_items";
const HNSW_INDEX: &str = "c2_items_embedding_hnsw";

/// One table with a vector column, one HNSW index over it, and a plain btree
/// index alongside so "the HNSW one is missing" cannot be confused with "no
/// index of any kind is listed".
fn hnsw_db() -> Arc<EmbeddedDatabase> {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    db.execute(&format!(
        "CREATE TABLE {HNSW_TABLE} (id INT PRIMARY KEY, label TEXT, embedding VECTOR(3))"
    ))
    .expect("create table");
    db.execute(&format!(
        "CREATE INDEX {HNSW_INDEX} ON {HNSW_TABLE} USING hnsw (embedding vector_cosine_ops)"
    ))
    .expect("create hnsw index");
    db.execute(&format!("CREATE INDEX c2_items_label_idx ON {HNSW_TABLE} (label)"))
        .expect("create btree index");
    db
}

/// `pg_indexes` — the ONE surface HC3 had already taught about the vector
/// registry. Asserted first so a failure elsewhere in C2 cannot be
/// misattributed to "vector indexes are not registered at all".
#[tokio::test]
async fn hnsw_index_is_listed_in_pg_indexes() {
    let (_addr, conn, _server) = serve(hnsw_db()).await;
    let (client, _task) = connect_client(&conn).await;

    let rows = simple_rows(
        &client,
        "SELECT schemaname, tablename, indexname, indexdef FROM pg_indexes",
    )
    .await;
    let hnsw = rows
        .iter()
        .find(|r| r["indexname"] == HNSW_INDEX)
        .unwrap_or_else(|| panic!("pg_indexes must list the HNSW index; got {rows:?}"));
    assert_eq!(hnsw["tablename"], HNSW_TABLE);
    assert_eq!(
        hnsw["indexdef"],
        format!("CREATE INDEX {HNSW_INDEX} ON public.{HNSW_TABLE} USING hnsw (embedding vector_cosine_ops)"),
        "the indexdef must be pgvector's own spelling, opclass included"
    );
}

/// `pg_class` / `pg_index`. FAILS on the pre-fix tree: `execute_pg_class`
/// enumerated `sorted_art_indexes(storage)` and stopped there, so the HNSW
/// relation had no row — which is why psql, SQLAlchemy and drizzle all reported
/// the index as not existing.
#[tokio::test]
async fn hnsw_index_is_a_pg_class_relation_and_a_pg_index_row() {
    let (_addr, conn, _server) = serve(hnsw_db()).await;
    let (client, _task) = connect_client(&conn).await;

    let relations = simple_rows(&client, "SELECT oid, relname, relkind FROM pg_class").await;
    let hnsw = relations
        .iter()
        .find(|r| r["relname"] == HNSW_INDEX)
        .unwrap_or_else(|| panic!("pg_class must contain the HNSW index; got {relations:?}"));
    assert_eq!(hnsw["relkind"], "i", "an index relation is relkind 'i'");
    let hnsw_oid = hnsw["oid"].clone();

    // OID disjointness: the vector registry is enumerated independently of the
    // ART one, so a shared OID counter would alias two different relations.
    let collisions: Vec<&BTreeMap<String, String>> = relations
        .iter()
        .filter(|r| r["oid"] == hnsw_oid && r["relname"] != HNSW_INDEX)
        .collect();
    assert!(
        collisions.is_empty(),
        "the HNSW index OID {hnsw_oid} collides with {collisions:?}"
    );

    // The btree index must still be there — the fix is additive.
    assert!(
        relations.iter().any(|r| r["relname"] == "c2_items_label_idx"),
        "the ART index must not have been displaced; got {relations:?}"
    );

    // pg_index must carry the SAME indexrelid, or a client that JOINs the two
    // (psql's `\d` does) resolves the index to nothing.
    let index_rows = simple_rows(
        &client,
        "SELECT indexrelid, indrelid, indisprimary, indisunique FROM pg_index",
    )
    .await;
    let entry = index_rows
        .iter()
        .find(|r| r["indexrelid"] == hnsw_oid)
        .unwrap_or_else(|| panic!("pg_index must have a row with indexrelid={hnsw_oid}; got {index_rows:?}"));
    assert_eq!(entry["indisprimary"], "f");
    assert_eq!(entry["indisunique"], "f", "HNSW is approximate — never unique");
}

/// psql's `\d <table>`, sent verbatim. The query text is lifted from the
/// matchers in `src/protocol/postgres/catalog.rs` rather than invented, because
/// those matchers are substring signatures: a paraphrase would route somewhere
/// else and the test would prove nothing about what psql sees.
#[tokio::test]
async fn psql_backslash_d_shows_the_hnsw_index() {
    let (_addr, conn, _server) = serve(hnsw_db()).await;
    let (client, _task) = connect_client(&conn).await;

    // 1. psql resolves the relation OID with an anchored regex match.
    let resolved = simple_rows(
        &client,
        &format!(
            "SELECT c.oid, n.nspname, c.relname \
             FROM pg_catalog.pg_class c \
             LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relname OPERATOR(pg_catalog.~) '^({HNSW_TABLE})$' COLLATE pg_catalog.default \
               AND pg_catalog.pg_table_is_visible(c.oid) \
             ORDER BY 2, 3"
        ),
    )
    .await;
    assert_eq!(resolved.len(), 1, "\\d must resolve exactly one relation: {resolved:?}");
    let oid = resolved[0]["oid"].clone();

    // 2. The 15-column header pull. `relhasindex` GATES the index-list query
    //    below: psql skips it entirely when this is false, so a table whose
    //    only index is an HNSW one used to print no "Indexes:" section no
    //    matter what the index list would have said.
    let header = simple_rows(
        &client,
        &format!(
            "SELECT c.relchecks, c.relkind, c.relhasindex, c.relhasrules, c.relhastriggers, \
                    c.relrowsecurity, c.relforcerowsecurity, false AS relhasoids, c.relispartition, '', \
                    c.reltablespace, \
                    CASE WHEN c.reloftype = 0 THEN '' ELSE c.reloftype::pg_catalog.regtype::pg_catalog.text END, \
                    c.relpersistence, c.relreplident, am.amname \
             FROM pg_catalog.pg_class c \
             LEFT JOIN pg_catalog.pg_class tc ON (c.reltoastrelid = tc.oid) \
             LEFT JOIN pg_catalog.pg_am am ON (c.relam = am.oid) \
             WHERE c.oid = '{oid}'"
        ),
    )
    .await;
    assert_eq!(header.len(), 1, "the \\d header pull must answer one row: {header:?}");
    assert_eq!(
        header[0]["relhasindex"], "t",
        "relhasindex gates psql's index-list query; false here hides every index"
    );

    // 3. The 12-column index list psql renders as the "Indexes:" section.
    let indexes = simple_rows(
        &client,
        &format!(
            "SELECT c2.relname, i.indisprimary, i.indisunique, i.indisclustered, i.indisvalid, \
                    pg_catalog.pg_get_indexdef(i.indexrelid, 0, true), \
                    pg_catalog.pg_get_constraintdef(con.oid, true), contype, \
                    condeferrable, condeferred, i.indisreplident, c2.reltablespace \
             FROM pg_catalog.pg_class c, pg_catalog.pg_class c2, pg_catalog.pg_index i \
             LEFT JOIN pg_catalog.pg_constraint con ON \
               (conrelid = i.indrelid AND conindid = i.indexrelid AND contype IN ('p','u','x')) \
             WHERE c.oid = '{oid}' AND c.oid = i.indrelid AND i.indexrelid = c2.oid \
             ORDER BY i.indisprimary DESC, c2.relname"
        ),
    )
    .await;
    let names: Vec<&str> = indexes.iter().map(|r| r["relname"].as_str()).collect();
    assert!(
        names.contains(&HNSW_INDEX),
        "\\d {HNSW_TABLE} must list the HNSW index; got {names:?}"
    );
    assert!(
        names.contains(&format!("{HNSW_TABLE}_pkey").as_str()),
        "the primary-key index must still be listed; got {names:?}"
    );
    assert!(
        names.contains(&"c2_items_label_idx"),
        "the plain btree index must be listed too; got {names:?}"
    );

    let hnsw = indexes.iter().find(|r| r["relname"] == HNSW_INDEX).expect("hnsw row");
    assert_eq!(hnsw["indisprimary"], "f");
    assert_eq!(hnsw["indisunique"], "f");
    assert_eq!(hnsw["indisvalid"], "t");
    assert_eq!(
        hnsw["indexdef"],
        format!("CREATE INDEX {HNSW_INDEX} ON public.{HNSW_TABLE} USING hnsw (embedding vector_cosine_ops)"),
        "psql prints everything after USING verbatim, so the access method must read `hnsw`"
    );
}

/// psql's `\di`. Its entire purpose is "list the indexes", and it listed every
/// index except the ones in the vector registry.
#[tokio::test]
async fn psql_backslash_di_shows_the_hnsw_index() {
    let (_addr, conn, _server) = serve(hnsw_db()).await;
    let (client, _task) = connect_client(&conn).await;

    let rows = simple_rows(
        &client,
        "SELECT n.nspname as \"Schema\", c.relname as \"Name\", \
                CASE c.relkind WHEN 'r' THEN 'table' WHEN 'i' THEN 'index' END as \"Type\", \
                pg_catalog.pg_get_userbyid(c.relowner) as \"Owner\", c2.relname as \"Table\" \
         FROM pg_catalog.pg_class c \
         LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_catalog.pg_index i ON i.indexrelid = c.oid \
         LEFT JOIN pg_catalog.pg_class c2 ON i.indrelid = c2.oid \
         WHERE c.relkind IN ('i','I') AND n.nspname <> 'pg_catalog' \
         ORDER BY 1,2",
    )
    .await;

    let hnsw = rows
        .iter()
        .find(|r| r["Name"] == HNSW_INDEX)
        .unwrap_or_else(|| panic!("\\di must list the HNSW index; got {rows:?}"));
    assert_eq!(hnsw["Type"], "index");
    assert_eq!(hnsw["Table"], HNSW_TABLE);
    assert!(
        rows.iter().any(|r| r["Name"] == format!("{HNSW_TABLE}_pkey")),
        "the PK index must still be listed; got {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r["Name"] == "c2_items_label_idx"),
        "the btree index must be listed too; got {rows:?}"
    );
}

// ===========================================================================
// C3 — sprinter 84ac05bd69c4: RETURNING residuals.
// ===========================================================================

fn returning_db() -> Arc<EmbeddedDatabase> {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    db.execute("CREATE TABLE c3_items (id INT PRIMARY KEY, qty INT NOT NULL, note TEXT)")
        .expect("create table");
    db.execute("INSERT INTO c3_items VALUES (1, 10, 'one')").expect("seed");
    db
}

/// A parsed `RowDescription`: `(name, type_oid, format_code)` per field.
fn row_description(body: &[u8]) -> Vec<(String, i32, i16)> {
    let count = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut cursor = 2usize;
    let mut fields = Vec::with_capacity(count);
    for _ in 0..count {
        let end = body[cursor..].iter().position(|b| *b == 0).expect("field name") + cursor;
        let name = String::from_utf8_lossy(&body[cursor..end]).into_owned();
        cursor = end + 1;
        // table_oid(4) column_attr(2) type_oid(4) type_len(2) type_mod(4) format(2)
        let type_oid = i32::from_be_bytes(body[cursor + 6..cursor + 10].try_into().expect("oid"));
        let format = i16::from_be_bytes(body[cursor + 16..cursor + 18].try_into().expect("format"));
        cursor += 18;
        fields.push((name, type_oid, format));
    }
    fields
}

/// Raw extended-protocol client: connect, finish trust startup, and return the
/// socket parked at ReadyForQuery.
async fn raw_session(addr: SocketAddr) -> TcpStream {
    let mut stream = timeout(FRAME_TIMEOUT, TcpStream::connect(addr))
        .await
        .expect("connect timeout")
        .expect("connect");
    write_frames(&mut stream, &[startup_packet("postgres", Some("postgres"))]).await;
    loop {
        let (tag, _) = read_frame(&mut stream).await.expect("startup frame");
        if tag == b'Z' {
            return stream;
        }
        assert_ne!(tag, b'E', "trust startup must not fail");
    }
}

fn parse_message(statement: &str, sql: &str) -> BytesMut {
    let mut body = BytesMut::new();
    put_cstr(&mut body, statement);
    put_cstr(&mut body, sql);
    body.put_i16(0); // no parameter type OIDs
    frontend_message(b'P', body)
}

fn describe_message(kind: u8, name: &str) -> BytesMut {
    let mut body = BytesMut::new();
    body.put_u8(kind);
    put_cstr(&mut body, name);
    frontend_message(b'D', body)
}

/// Bind with NO parameters and `result_formats` applied to every column.
fn bind_message(portal: &str, statement: &str, result_formats: &[i16]) -> BytesMut {
    let mut body = BytesMut::new();
    put_cstr(&mut body, portal);
    put_cstr(&mut body, statement);
    body.put_i16(0); // parameter format codes
    body.put_i16(0); // parameter values
    body.put_i16(result_formats.len() as i16);
    for format in result_formats {
        body.put_i16(*format);
    }
    frontend_message(b'B', body)
}

/// Execute with no row limit.
fn execute_message(portal: &str) -> BytesMut {
    let mut body = BytesMut::new();
    put_cstr(&mut body, portal);
    body.put_i32(0); // unlimited rows
    frontend_message(b'E', body)
}

fn sync_message() -> BytesMut {
    frontend_message(b'S', BytesMut::new())
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

/// C3 sub-items (1) and (3) in one exchange, because they are two readings of
/// the SAME `RowDescription` pair.
///
/// (1) `RETURNING qty + 1` is a genuine `ReturningItem::Expression`. It must be
///     advertised with the type the SELECT list would give it — an integer —
///     not the blanket `DataType::Text` (OID 25) the old `returning_schema`
///     stamped on every expression item. OID 25 on an int column is not a
///     cosmetic mislabel: `text` HAS a binary encoder, so a portal that asks
///     for binary is GRANTED format 1 and then handed four big-endian bytes it
///     decodes as UTF-8.
///
/// (3) `Describe(Statement)` reports format code 0 for every field while
///     `Describe(Portal)` honours the formats that portal asked for. That is
///     PostgreSQL's behaviour, not a divergence: `exec_describe_statement_message`
///     calls `SendRowDescriptionMessage` with a NULL `formats` array (no portal
///     exists yet, so no result formats have been requested), and
///     `SendRowDescriptionMessage` writes 0 whenever `formats` is NULL. The
///     assertion pins it so the item can be closed as not-a-bug.
#[tokio::test]
async fn returning_expression_is_typed_numerically_and_describe_formats_follow_postgresql() {
    let (addr, _conn, _server) = serve(returning_db()).await;
    let mut stream = raw_session(addr).await;

    let sql = "INSERT INTO c3_items (id, qty, note) VALUES (2, 41, 'two') RETURNING qty + 1 AS bumped, note";
    write_frames(
        &mut stream,
        &[
            parse_message("c3_stmt", sql),
            describe_message(b'S', "c3_stmt"),
            // Binary results for BOTH columns — the case where a wrong OID
            // becomes wrong BYTES rather than a wrong label.
            bind_message("c3_portal", "c3_stmt", &[1, 1]),
            describe_message(b'P', "c3_portal"),
            sync_message(),
        ],
    )
    .await;

    let frames = read_until_ready(&mut stream).await;
    assert!(
        !frames.iter().any(|(tag, _)| *tag == b'E'),
        "the exchange must not error: {:?}",
        frames.iter().map(|(t, _)| *t as char).collect::<Vec<_>>()
    );
    let descriptions: Vec<Vec<(String, i32, i16)>> = frames
        .iter()
        .filter(|(tag, _)| *tag == b'T')
        .map(|(_, body)| row_description(body))
        .collect();
    assert_eq!(
        descriptions.len(),
        2,
        "one RowDescription from Describe(Statement), one from Describe(Portal)"
    );

    let (statement_desc, portal_desc) = (&descriptions[0], &descriptions[1]);
    assert_eq!(
        statement_desc.iter().map(|f| f.0.as_str()).collect::<Vec<_>>(),
        vec!["bumped", "note"],
        "the alias must survive to the wire"
    );

    // (1) — the residual this item filed.
    const PG_TEXT_OID: i32 = 25;
    const NUMERIC_OIDS: &[i32] = &[20, 21, 23, 700, 701, 1700]; // int8 int2 int4 float4 float8 numeric
    let bumped_oid = statement_desc[0].1;
    assert_ne!(
        bumped_oid, PG_TEXT_OID,
        "`qty + 1` must not be advertised as text — that is the GH#23 blanket \
         DataType::Text fallback for every ReturningItem::Expression"
    );
    assert!(
        NUMERIC_OIDS.contains(&bumped_oid),
        "`qty + 1` over an INT column must carry a numeric OID, got {bumped_oid}"
    );
    assert_eq!(statement_desc[1].1, PG_TEXT_OID, "`note` really is text");
    assert_eq!(
        portal_desc.iter().map(|f| (f.0.clone(), f.1)).collect::<Vec<_>>(),
        statement_desc.iter().map(|f| (f.0.clone(), f.1)).collect::<Vec<_>>(),
        "Describe(Statement) and Describe(Portal) must agree on names and types"
    );

    // (3) — the sub-item that is NOT a bug.
    assert!(
        statement_desc.iter().all(|f| f.2 == 0),
        "Describe(Statement) must report text format for every field (PostgreSQL \
         passes a NULL formats array there): {statement_desc:?}"
    );
    assert!(
        portal_desc.iter().all(|f| f.2 == 1),
        "Describe(Portal) must honour the binary formats THIS portal requested: {portal_desc:?}"
    );
}

/// C3 sub-item (2): a RETURNING item naming no column of the target table is
/// REFUSED with `42703 undefined_column` — never answered with a NULL column,
/// and never after a partial write — on the extended protocol as well as the
/// simple one.
///
/// WHY THE EXTENDED-PROTOCOL ASSERTION IS ON THE WHOLE Parse→Execute WINDOW
/// RATHER THAN ON Parse ALONE.
///
/// This test's first draft asserted an ErrorResponse at Parse and FAILED: the
/// run produced `ParseComplete, ParameterDescription, NoData, ReadyForQuery`.
/// `handle_parse_extended` swallows EVERY schema-derivation failure into its
/// lenient AST fallback, which synthesises nothing for DML — so the RETURNING
/// refusal is deferred to Execute and Describe(Statement) answers `NoData` in
/// the meantime.
///
/// Real PostgreSQL refuses it at PARSE: `exec_parse_message` runs full parse
/// analysis (`parse_analyze_varparams` → `transformInsertStmt` /
/// `transformUpdateStmt` / `transformDeleteStmt` → `transformReturningList`
/// under `EXPR_KIND_RETURNING`), and an unresolvable name there raises
/// `errorMissingColumn` → `ERRCODE_UNDEFINED_COLUMN`, so the client gets an
/// ErrorResponse instead of ParseComplete and never reaches Bind. That is why
/// `PREPARE p AS INSERT … RETURNING nosuch` errors immediately. Nano's
/// deferral IS therefore a genuine divergence — but a DIAGNOSTIC-TIMING one,
/// not a correctness one: every executor family binds the list before its
/// first write, so nothing is written and no NULL column is invented.
///
/// It is left unfixed HERE on purpose. `wire_tests::
/// gh23_c3_returning_aggregate_and_window_are_refused_with_zero_rows_written`
/// deliberately pins the SAME Execute-time timing for the sibling refusals
/// (42803 aggregate / 42P20 window, "the refusal arrives at Execute, before
/// any write"), and its `gh23_c2_extended` helper asserts outright that
/// "Parse/Bind/Describe must not fail". Moving 42703 alone would leave the
/// three RETURNING-bind refusals inconsistent and contradict that test;
/// moving all three is a separate change that has to rework that shared
/// helper across its call sites. Filed as its own item.
///
/// So the assertion below pins the invariant that must hold either way, and
/// that a future Parse-time fix will still satisfy: the statement is refused
/// with 42703 somewhere between Parse and Execute, emits no DataRow and no
/// CommandComplete, and writes nothing.
#[tokio::test]
async fn returning_unknown_column_is_42703_and_never_a_null_column() {
    let (addr, conn, _server) = serve(returning_db()).await;

    // --- extended protocol: Parse, Bind, Describe, Execute, Sync -----------
    let mut stream = raw_session(addr).await;
    write_frames(
        &mut stream,
        &[
            parse_message(
                "c3_bad",
                "INSERT INTO c3_items (id, qty, note) VALUES (9, 1, 'nine') RETURNING nosuchcol",
            ),
            describe_message(b'S', "c3_bad"),
            bind_message("c3_bad_p", "c3_bad", &[0]),
            execute_message("c3_bad_p"),
            sync_message(),
        ],
    )
    .await;
    let frames = read_until_ready(&mut stream).await;
    let tags: Vec<char> = frames.iter().map(|(t, _)| *t as char).collect();

    let (_, error_body) = frames
        .iter()
        .find(|(tag, _)| *tag == b'E')
        .unwrap_or_else(|| panic!("an unknown RETURNING column must be refused, not executed; got frames {tags:?}"));
    assert_eq!(
        error_fields(error_body).get(&'C').map(String::as_str),
        Some("42703"),
        "PostgreSQL's undefined_column"
    );
    // No RowDescription (`T`) may ever advertise the column, no DataRow (`D`)
    // may carry it as NULL — the filed defect — and no CommandComplete (`C`)
    // may acknowledge a statement that was refused.
    assert!(
        !tags.contains(&'T') && !tags.contains(&'D') && !tags.contains(&'C'),
        "a refused RETURNING list must produce no RowDescription, DataRow or CommandComplete; got {tags:?}"
    );
    assert_eq!(
        tags.last(),
        Some(&'Z'),
        "the extended-protocol error must be recoverable at Sync, got {tags:?}"
    );

    // --- simple query: the same refusal, at the only moment it can happen ---
    // There is no Parse step here, so Execute-time IS PostgreSQL's timing.
    let (client, _task) = connect_client(&conn).await;
    let err = timeout(
        FRAME_TIMEOUT,
        client.simple_query("UPDATE c3_items SET qty = qty + 1 WHERE id = 1 RETURNING nosuchcol"),
    )
    .await
    .expect("query timeout")
    .expect_err("an unknown RETURNING column must fail, not answer NULL");
    let db_error = err.as_db_error().expect("a DbError");
    assert_eq!(db_error.code().code(), "42703", "got {db_error:?}");

    // --- neither statement may have written anything ------------------------
    // The refused INSERT must not have added row 9, and the refused UPDATE must
    // not have bumped row 1 — both lists are bound before the first write.
    let rows = simple_rows(&client, "SELECT id, qty FROM c3_items ORDER BY id").await;
    assert_eq!(
        rows.iter()
            .map(|r| (r["id"].as_str(), r["qty"].as_str()))
            .collect::<Vec<_>>(),
        vec![("1", "10")],
        "a refused RETURNING list must leave the table exactly as it was"
    );
}

/// The embedded leg of sub-item (2), and the control that proves the refusal is
/// about the NAME and not about RETURNING expressions in general.
#[test]
fn returning_binds_before_it_writes_on_the_embedded_route() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE c3_emb (id INT PRIMARY KEY, qty INT NOT NULL)")
        .expect("create");

    let refused = db.query("INSERT INTO c3_emb VALUES (1, 5) RETURNING nosuchcol", &[]);
    let message = refused
        .expect_err("an unknown RETURNING column must be an error")
        .to_string();
    assert!(
        message.contains("nosuchcol") && message.contains("does not exist"),
        "the error must name the column, got: {message}"
    );
    assert_eq!(
        db.query("SELECT id FROM c3_emb", &[]).expect("select").len(),
        0,
        "a refused RETURNING list must not leave the INSERT half-applied"
    );

    // Control: the same statement with a REAL expression works and returns the
    // computed value, so the assertion above is about resolution, not about
    // expressions being rejected wholesale.
    let rows = db
        .query("INSERT INTO c3_emb VALUES (1, 5) RETURNING qty * 2", &[])
        .expect("expression RETURNING must work");
    assert_eq!(rows.len(), 1);
    // The exact integer WIDTH is a type-inference detail (`qty * 2` may widen);
    // the point is that the item is evaluated and typed as a number, not
    // stringified or nulled.
    let computed = match rows[0].get(0).expect("one output column") {
        Value::Int2(v) => i64::from(*v),
        Value::Int4(v) => i64::from(*v),
        Value::Int8(v) => *v,
        other => panic!("`qty * 2` must come back as an integer, got {other:?}"),
    };
    assert_eq!(computed, 10);
}
