//! HDB-011 — `pg_type` answered every query from a fixed rowset, ignored most
//! predicates, and compared literals after lowercasing them.
//!
//! The report, reproduced on v4.31.1 and d44d4eb over the PostgreSQL wire:
//!
//! ```text
//! SELECT typname FROM pg_type WHERE typname = 'int4';     -- all 12 built-in rows
//! SELECT count(*) FROM pg_type WHERE typname = 'hstore';  -- 12, not 0
//! ```
//!
//! `PgCatalog::handle_query` intercepted ANY SELECT mentioning `pg_type` and
//! answered it from a prebuilt 12-row, 5-column rowset. The WHERE clause was
//! then "applied" by string-splitting the statement text: only a handful of
//! shapes were recognised, and only WITH SPACES AROUND THE OPERATOR, so
//! `typname='int4'`, any `OR`, and every `count(*)` over a non-matching
//! predicate kept the entire table. Worse, the whole statement was lowercased
//! first, so `WHERE typname = 'INT4'` matched the row named `int4`.
//!
//! The fix retires the interception: `pg_type` is served by the planner-backed
//! `SystemViewRegistry`, which filters, projects, joins, orders and aggregates
//! with real SQL semantics — over a full PostgreSQL type inventory (real OIDs,
//! array types linked through `typarray`/`typelem`, Nano's `vector` at 16385)
//! instead of 12 hand-written rows. (`vector` was registered at 3614 until
//! HDB-002 moved it to its own user-band OID: 3614 is PostgreSQL's `tsvector`,
//! which is now a real Nano type.)
//!
//! Each test runs the same SQL on all THREE surfaces: embedded
//! (`EmbeddedDatabase::query`), the PostgreSQL simple-query protocol
//! (`client.simple_query`) and the PostgreSQL extended protocol
//! (`client.query` / `client.query_typed` / `client.prepare`). The WIRE halves
//! are the reported regression — the embedded halves went through the planner
//! already and mostly passed before the fix, though against a 7-row inventory.
//!
//! Not covered here, deliberately:
//!   * tokio-postgres' `TYPEINFO` lookup does not fire on its own: every OID
//!     Nano puts on the wire (`datatype_to_oid` in handler.rs — `vector`
//!     included, which is advertised as `text`) is one tokio-postgres already
//!     knows. Case 12 runs the statement the driver WOULD send by hand
//!     instead, including its `LEFT OUTER JOIN pg_catalog.pg_range`, which is
//!     now a registered (empty) system view.
//!   * `client.prepare`/`client.query` with an INFERRED parameter: Nano used to
//!     report OID 0 (unknown) in ParameterDescription, and tokio-postgres
//!     answers an unknown OID by recursing into its own `TYPEINFO` lookup.
//!     FIXED by sprinter 6ac716be10ea (`tests/param_oid_batch_g5.rs`) — Parse
//!     now infers the real type — but the parameterised cases below keep using
//!     `query_typed`, which states the parameter type on the wire: it is still
//!     a real Parse/Bind/Describe/Execute round trip, and stating the type is
//!     what keeps THIS file's subject the catalogue, not the inference.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{
    protocol::postgres::server::{PgServer, PgServerConfig},
    EmbeddedDatabase, Value,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use tokio_postgres::types::Type;
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);

// ===========================================================================
// Harness (same shape as tests/security_hdb_004.rs)
// ===========================================================================

async fn setup_server() -> (String, tokio::task::JoinHandle<()>) {
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
    (conn_string, handle)
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

/// One connected client against a fresh server.
async fn server_client() -> (Client, tokio::task::JoinHandle<()>) {
    let (conn_string, handle) = setup_server().await;
    (connect(&conn_string).await, handle)
}

// --------------------------------------------------------------- embedded

/// Column `idx` of every row, stringified, through the embedded API.
fn emb(db: &EmbeddedDatabase, sql: &str, idx: usize) -> Vec<String> {
    let rows = db.query(sql, &[]).unwrap_or_else(|e| panic!("embedded `{sql}`: {e}"));
    rows.iter()
        .map(|row| match row.values.get(idx) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Int2(v)) => v.to_string(),
            Some(Value::Int4(v)) => v.to_string(),
            Some(Value::Int8(v)) => v.to_string(),
            Some(Value::Null) | None => "NULL".to_string(),
            Some(other) => format!("{other:?}"),
        })
        .collect()
}

// ----------------------------------------------------------- simple query

/// Column `idx` of every row, through the simple-query (`Q`) protocol. The
/// simple protocol is all-text, so every column comes back as a string.
async fn simple(client: &Client, sql: &str, idx: usize) -> Vec<String> {
    let messages = timeout(QUERY_TIMEOUT, client.simple_query(sql))
        .await
        .expect("simple_query timeout")
        .unwrap_or_else(|e| panic!("simple `{sql}`: {e}"));
    messages
        .into_iter()
        .filter_map(|message| match message {
            SimpleQueryMessage::Row(row) => Some(row.get(idx).unwrap_or("NULL").to_string()),
            _ => None,
        })
        .collect()
}

// --------------------------------------------------------- extended query

/// Column 0 of every row as text, through Parse/Bind/Describe/Execute.
async fn ext_text(client: &Client, sql: &str) -> Vec<String> {
    let rows = timeout(QUERY_TIMEOUT, client.query(sql, &[]))
        .await
        .expect("extended query timeout")
        .unwrap_or_else(|e| panic!("extended `{sql}`: {e}"));
    rows.iter().map(|row| row.get::<_, String>(0)).collect()
}

/// Column 0 of every row as int4, through Parse/Bind/Describe/Execute.
async fn ext_i32(client: &Client, sql: &str) -> Vec<i32> {
    let rows = timeout(QUERY_TIMEOUT, client.query(sql, &[]))
        .await
        .expect("extended query timeout")
        .unwrap_or_else(|e| panic!("extended `{sql}`: {e}"));
    rows.iter().map(|row| row.get::<_, i32>(0)).collect()
}

/// `count(*)` through Parse/Bind/Describe/Execute (int8).
async fn ext_i64(client: &Client, sql: &str) -> Vec<i64> {
    let rows = timeout(QUERY_TIMEOUT, client.query(sql, &[]))
        .await
        .expect("extended query timeout")
        .unwrap_or_else(|e| panic!("extended `{sql}`: {e}"));
    rows.iter().map(|row| row.get::<_, i64>(0)).collect()
}

/// Column 0 of every row as text, through the extended protocol with ONE bound
/// text parameter. `query_typed` states the parameter's type on the wire, so it
/// does not depend on Nano inferring one (see the module doc).
async fn ext_text_param(client: &Client, sql: &str, param: &str) -> Vec<String> {
    let rows = timeout(QUERY_TIMEOUT, client.query_typed(sql, &[(&param, Type::TEXT)]))
        .await
        .expect("extended query timeout")
        .unwrap_or_else(|e| panic!("extended(param) `{sql}`: {e}"));
    rows.iter().map(|row| row.get::<_, String>(0)).collect()
}

// ===========================================================================
// 1. The headline: an equality predicate selects ONE row, not the table
// ===========================================================================

/// REGRESSION: the wire halves. Before the fix both the simple and extended
/// paths returned all 12 rows of the interceptor's fixed set.
#[tokio::test]
async fn hdb011_equality_predicate_returns_one_row() {
    const SQL: &str = "SELECT typname FROM pg_type WHERE typname = 'int4'";

    let db = EmbeddedDatabase::new_in_memory().expect("db");
    assert_eq!(emb(&db, SQL, 0), vec!["int4".to_string()], "embedded");

    let (client, _h) = server_client().await;
    assert_eq!(simple(&client, SQL, 0).await, vec!["int4".to_string()], "wire, simple");
    assert_eq!(ext_text(&client, SQL).await, vec!["int4".to_string()], "wire, extended");
    assert_eq!(
        ext_text_param(&client, "SELECT typname FROM pg_type WHERE typname = $1", "int4").await,
        vec!["int4".to_string()],
        "wire, extended with a bound parameter"
    );
}

// ===========================================================================
// 2. A type we do not have counts as ZERO
// ===========================================================================

/// REGRESSION: the wire halves answered 12 — the interceptor's whole rowset,
/// collapsed by its own `count(*)` stage after a predicate it never applied.
#[tokio::test]
async fn hdb011_count_of_a_missing_type_is_zero() {
    const SQL: &str = "SELECT count(*) FROM pg_type WHERE typname = 'hstore'";

    let db = EmbeddedDatabase::new_in_memory().expect("db");
    assert_eq!(emb(&db, SQL, 0), vec!["0".to_string()], "embedded");

    let (client, _h) = server_client().await;
    assert_eq!(simple(&client, SQL, 0).await, vec!["0".to_string()], "wire, simple");
    assert_eq!(ext_i64(&client, SQL).await, vec![0_i64], "wire, extended");
}

// ===========================================================================
// 3. Literals keep their case
// ===========================================================================

/// REGRESSION: the wire halves. `handle_query` lowercased the whole statement
/// before looking at it, so the literal `'INT4'` was compared as `'int4'` and
/// matched. (The embedded path never went through that code.)
#[tokio::test]
async fn hdb011_literals_are_case_sensitive() {
    const UPPER: &str = "SELECT typname FROM pg_type WHERE typname = 'INT4'";
    const LOWER: &str = "SELECT typname FROM pg_type WHERE typname = 'int4'";

    let db = EmbeddedDatabase::new_in_memory().expect("db");
    assert!(emb(&db, UPPER, 0).is_empty(), "embedded: 'INT4' must match nothing");
    assert_eq!(emb(&db, LOWER, 0).len(), 1, "embedded: 'int4' matches exactly one row");

    let (client, _h) = server_client().await;
    assert!(
        simple(&client, UPPER, 0).await.is_empty(),
        "wire simple: 'INT4' must match nothing"
    );
    assert_eq!(simple(&client, LOWER, 0).await.len(), 1, "wire simple: 'int4'");
    assert!(
        ext_text(&client, UPPER).await.is_empty(),
        "wire extended: 'INT4' must match nothing"
    );
    assert_eq!(ext_text(&client, LOWER).await.len(), 1, "wire extended: 'int4'");
}

// ===========================================================================
// 4. Predicate shapes the substring router could not parse
// ===========================================================================

/// REGRESSION: the wire halves. `typname='int4'` (no spaces), `OR`, and
/// `IN (…) AND …` each fell into "unknown shape — keep the row", so all three
/// returned the entire fixed rowset.
#[tokio::test]
async fn hdb011_no_space_and_or_predicates() {
    const NO_SPACE: &str = "SELECT typname FROM pg_type WHERE typname='int4'";
    const OR: &str = "SELECT typname FROM pg_type WHERE typname = 'int4' OR typname = 'text'";
    const IN_AND: &str = "SELECT typname FROM pg_type WHERE typname IN ('int4','int8') AND typtype = 'b'";

    let db = EmbeddedDatabase::new_in_memory().expect("db");
    assert_eq!(emb(&db, NO_SPACE, 0), vec!["int4".to_string()], "embedded, no spaces");
    assert_eq!(emb(&db, OR, 0).len(), 2, "embedded, OR");
    assert_eq!(emb(&db, IN_AND, 0).len(), 2, "embedded, IN + AND");

    let (client, _h) = server_client().await;
    for (label, sql, want) in [
        ("no spaces around =", NO_SPACE, 1_usize),
        ("OR", OR, 2),
        ("IN + AND", IN_AND, 2),
    ] {
        assert_eq!(simple(&client, sql, 0).await.len(), want, "wire simple, {label}");
        assert_eq!(ext_text(&client, sql).await.len(), want, "wire extended, {label}");
    }

    let mut or_rows = simple(&client, OR, 0).await;
    or_rows.sort();
    assert_eq!(
        or_rows,
        vec!["int4".to_string(), "text".to_string()],
        "wire simple, OR rows"
    );
}

// ===========================================================================
// 5. OID predicates, aliases and schema qualification
// ===========================================================================

/// PARTLY a regression: the interceptor did serve `pg_catalog.pg_type`, but it
/// returned every row (the `t.oid = 23` predicate names an aliased column its
/// `row_value` lookup could not resolve) and its `oid` column was the only
/// thing it got right.
#[tokio::test]
async fn hdb011_oid_predicates_aliases_and_qualification() {
    const ALIASED: &str = "SELECT t.typname FROM pg_catalog.pg_type t WHERE t.oid = 23";
    const OID_OF: &str = "SELECT oid FROM pg_type WHERE typname = 'int4'";

    let db = EmbeddedDatabase::new_in_memory().expect("db");
    assert_eq!(emb(&db, ALIASED, 0), vec!["int4".to_string()], "embedded, aliased");
    assert_eq!(emb(&db, OID_OF, 0), vec!["23".to_string()], "embedded, oid of int4");

    let (client, _h) = server_client().await;
    assert_eq!(
        simple(&client, ALIASED, 0).await,
        vec!["int4".to_string()],
        "wire simple, aliased"
    );
    assert_eq!(
        ext_text(&client, ALIASED).await,
        vec!["int4".to_string()],
        "wire extended, aliased"
    );
    // `oid` is int4 on the wire, so the simple protocol renders it as `23`.
    assert_eq!(
        simple(&client, OID_OF, 0).await,
        vec!["23".to_string()],
        "wire simple, oid"
    );
    assert_eq!(ext_i32(&client, OID_OF).await, vec![23_i32], "wire extended, oid");
}

// ===========================================================================
// 6. ORDER BY / LIMIT / multi-column projection
// ===========================================================================

/// REGRESSION: the wire halves. The interceptor never sorted or limited — it
/// returned its rows in declaration order, all of them.
#[tokio::test]
async fn hdb011_order_limit_and_projection() {
    const SQL: &str = "SELECT typname, oid FROM pg_type WHERE typcategory = 'N' ORDER BY oid LIMIT 3";

    let db = EmbeddedDatabase::new_in_memory().expect("db");
    assert_eq!(
        emb(&db, SQL, 0),
        vec!["int8".to_string(), "int2".to_string(), "int4".to_string()],
        "embedded, names"
    );
    assert_eq!(
        emb(&db, SQL, 1),
        vec!["20".to_string(), "21".to_string(), "23".to_string()],
        "embedded, oids"
    );

    let (client, _h) = server_client().await;
    assert_eq!(
        simple(&client, SQL, 0).await,
        vec!["int8".to_string(), "int2".to_string(), "int4".to_string()],
        "wire simple, names"
    );
    assert_eq!(
        simple(&client, SQL, 1).await,
        vec!["20".to_string(), "21".to_string(), "23".to_string()],
        "wire simple, oids"
    );
    assert_eq!(
        ext_text(&client, SQL).await,
        vec!["int8".to_string(), "int2".to_string(), "int4".to_string()],
        "wire extended, names"
    );
}

// ===========================================================================
// 7. JOIN pg_namespace
// ===========================================================================

/// REGRESSION: the wire halves. The interceptor could not join at all — it
/// returned its own 5 columns regardless of what the statement asked for, so a
/// client reading `n.nspname` got a type OID.
#[tokio::test]
async fn hdb011_namespace_join_resolves() {
    const SQL: &str = "SELECT n.nspname FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace \
                       WHERE t.typname = 'uuid'";

    let db = EmbeddedDatabase::new_in_memory().expect("db");
    assert_eq!(emb(&db, SQL, 0), vec!["pg_catalog".to_string()], "embedded");

    let (client, _h) = server_client().await;
    assert_eq!(
        simple(&client, SQL, 0).await,
        vec!["pg_catalog".to_string()],
        "wire, simple"
    );
    assert_eq!(
        ext_text(&client, SQL).await,
        vec!["pg_catalog".to_string()],
        "wire, extended"
    );
}

// ===========================================================================
// 8. Array types are linked through typarray / typelem
// ===========================================================================

/// The inventory now carries array types. Split into separate statements
/// rather than a scalar subquery in WHERE, which the planner does not reliably
/// support (tests/subquery_hardening_tests.rs treats it as a known limitation).
#[tokio::test]
async fn hdb011_array_types_are_linked() {
    const TYPARRAY_OF_INT4: &str = "SELECT typarray FROM pg_type WHERE typname = 'int4'";
    const NAME_OF_1007: &str = "SELECT typname FROM pg_type WHERE oid = 1007";
    const TYPELEM_OF_INT4_ARRAY: &str = "SELECT typelem FROM pg_type WHERE typname = '_int4'";

    let db = EmbeddedDatabase::new_in_memory().expect("db");
    assert_eq!(
        emb(&db, TYPARRAY_OF_INT4, 0),
        vec!["1007".to_string()],
        "embedded, typarray"
    );
    assert_eq!(emb(&db, NAME_OF_1007, 0), vec!["_int4".to_string()], "embedded, _int4");
    assert_eq!(
        emb(&db, TYPELEM_OF_INT4_ARRAY, 0),
        vec!["23".to_string()],
        "embedded, typelem"
    );

    let (client, _h) = server_client().await;
    assert_eq!(
        simple(&client, TYPARRAY_OF_INT4, 0).await,
        vec!["1007".to_string()],
        "wire, typarray"
    );
    assert_eq!(
        simple(&client, NAME_OF_1007, 0).await,
        vec!["_int4".to_string()],
        "wire, _int4"
    );
    assert_eq!(
        simple(&client, TYPELEM_OF_INT4_ARRAY, 0).await,
        vec!["23".to_string()],
        "wire, typelem"
    );
    assert_eq!(
        ext_i32(&client, TYPARRAY_OF_INT4).await,
        vec![1007_i32],
        "extended, typarray"
    );
}

// ===========================================================================
// 9. Nothing the old rowset advertised was lost
// ===========================================================================

/// The 12 (typname, oid) pairs the retired interceptor served — plus Nano's
/// `vector` — must still resolve to the SAME oid. Partly a regression before
/// the fix: the names were there, but every one of these queries returned all
/// 12 rows instead of one.
///
/// `vector` is the one exception to "the SAME oid": HDB-002 moved it off 3614,
/// which is PostgreSQL's `tsvector` OID, onto its own private user-band OID.
/// The other 12 are PostgreSQL's real OIDs and never move.
#[tokio::test]
async fn hdb011_legacy_inventory_is_a_subset() {
    const LEGACY: &[(&str, &str)] = &[
        ("bool", "16"),
        ("int8", "20"),
        ("int2", "21"),
        ("int4", "23"),
        ("text", "25"),
        ("json", "114"),
        ("float4", "700"),
        ("float8", "701"),
        ("varchar", "1043"),
        ("timestamp", "1114"),
        ("uuid", "2950"),
        ("jsonb", "3802"),
        // Nano's own vector type. HDB-002: 16385, not the 3614 the registry
        // used to give it — that OID belongs to PostgreSQL's `tsvector`, which
        // Nano now declares for real.
        ("vector", "16385"),
    ];

    let db = EmbeddedDatabase::new_in_memory().expect("db");
    let (client, _h) = server_client().await;
    for (name, oid) in LEGACY {
        let sql = format!("SELECT oid FROM pg_type WHERE typname = '{name}'");
        assert_eq!(emb(&db, &sql, 0), vec![(*oid).to_string()], "embedded, {name}");
        assert_eq!(simple(&client, &sql, 0).await, vec![(*oid).to_string()], "wire, {name}");
    }
}

// ===========================================================================
// 10. Describe reports the real column types
// ===========================================================================

/// REGRESSION: the wire half. Describe used to come from the interceptor's
/// fixed 5-column schema, so a client preparing `SELECT oid, typname, typlen`
/// was told about columns the result would not have. `typlen` is int2 (21) as
/// in PostgreSQL, not int4.
///
/// The statement uses a literal rather than `$1`: when this was written an
/// INFERRED parameter was reported as OID 0, and tokio-postgres answers an
/// unknown OID by recursing into its own TYPEINFO lookup (module doc). That is
/// fixed — sprinter 6ac716be10ea — and the literal is kept because this case is
/// about the RESULT column types, which a parameter would not change.
#[tokio::test]
async fn hdb011_describe_reports_column_types() {
    let (client, _h) = server_client().await;
    let stmt = timeout(
        QUERY_TIMEOUT,
        client.prepare("SELECT oid, typname, typlen FROM pg_type WHERE typname = 'int4'"),
    )
    .await
    .expect("prepare timeout")
    .expect("prepare must succeed");

    let columns = stmt.columns();
    let shape: Vec<(String, Type)> = columns
        .iter()
        .map(|c| (c.name().to_string(), c.type_().clone()))
        .collect();
    assert_eq!(
        shape,
        vec![
            ("oid".to_string(), Type::INT4),
            ("typname".to_string(), Type::TEXT),
            ("typlen".to_string(), Type::INT2),
        ],
        "Describe RowDescription names + pg_type OIDs"
    );
}

// ===========================================================================
// 11. A WHERE the router cannot interpret must not widen the answer
// ===========================================================================

/// REGRESSION: the wire half. `pg_tables` is still intercepted, and an `OR`
/// predicate fell into "unknown shape — keep the row", so this listed EVERY
/// table. It must now either be answered correctly by the planner or fail —
/// never silently return more rows than were asked for.
///
/// Both spellings of the SAME statement are exercised. The two-line one is the
/// review's FIX-1: the guard located its clause keywords with a plain
/// substring search for `" where "` / `" or "`, so a WHERE on the next line
/// looked like a statement with no WHERE at all and the whole catalog came
/// back — the very widening this test was written to prevent, reachable with
/// one line break (and every ORM emits one).
#[tokio::test]
async fn hdb011_pg_tables_unsupported_where_is_not_unfiltered() {
    const ONE_LINE: &str = "SELECT tablename FROM pg_tables WHERE tablename = 'a' OR tablename = 'zzz'";
    const TWO_LINE: &str = "SELECT tablename FROM pg_tables\nWHERE tablename = 'a' OR tablename = 'zzz'";

    let (client, _h) = server_client().await;
    client
        .batch_execute("CREATE TABLE a (id INT)")
        .await
        .expect("create table a");
    client
        .batch_execute("CREATE TABLE b (id INT)")
        .await
        .expect("create table b");

    // Sanity: both tables are visible without a predicate.
    let all = simple(&client, "SELECT tablename FROM pg_tables", 0).await;
    assert!(
        all.contains(&"a".to_string()) && all.contains(&"b".to_string()),
        "pg_tables must list both tables, got {all:?}"
    );

    for sql in [ONE_LINE, TWO_LINE] {
        match timeout(QUERY_TIMEOUT, client.simple_query(sql))
            .await
            .expect("simple_query timeout")
        {
            Ok(messages) => {
                let rows: Vec<String> = messages
                    .into_iter()
                    .filter_map(|message| match message {
                        SimpleQueryMessage::Row(row) => Some(row.get(0).unwrap_or("NULL").to_string()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    rows,
                    vec!["a".to_string()],
                    "`{sql}`: an OR predicate must select exactly `a` — never the unfiltered catalog"
                );
            }
            // Failing closed is acceptable; returning both rows is not.
            Err(e) => {
                let message = e.to_string();
                assert!(
                    !message.is_empty(),
                    "`{sql}`: an unsupported predicate may error, but it must say why"
                );
            }
        }
    }
}

// ===========================================================================
// 12. pg_range / pg_enum EXIST and answer zero rows
// ===========================================================================

/// Retiring the interceptor sends every `pg_type` query to the planner, which
/// means the catalogues drivers JOIN against it have to resolve too. Both are
/// registered EMPTY: tokio-postgres' TYPEINFO lookup `LEFT OUTER JOIN`s
/// `pg_catalog.pg_range`, and drizzle-kit / Prisma join `pg_enum` for enum
/// introspection. An empty table answers both correctly; "relation does not
/// exist" fails them outright.
#[tokio::test]
async fn hdb011_pg_range_and_pg_enum_are_empty_not_missing() {
    const RANGES: &str = "SELECT rngtypid FROM pg_range";
    const ENUMS: &str = "SELECT enumlabel FROM pg_enum";
    // tokio-postgres' TYPEINFO statement, narrowed to a single OID.
    const TYPEINFO: &str = "SELECT t.typname, r.rngsubtype FROM pg_catalog.pg_type t \
                            LEFT OUTER JOIN pg_catalog.pg_range r ON r.rngtypid = t.oid \
                            INNER JOIN pg_catalog.pg_namespace n ON t.typnamespace = n.oid \
                            WHERE t.oid = 23";

    let db = EmbeddedDatabase::new_in_memory().expect("db");
    assert!(emb(&db, RANGES, 0).is_empty(), "embedded, pg_range must be empty");
    assert!(emb(&db, ENUMS, 0).is_empty(), "embedded, pg_enum must be empty");
    assert_eq!(emb(&db, TYPEINFO, 0), vec!["int4".to_string()], "embedded, TYPEINFO");

    let (client, _h) = server_client().await;
    assert!(
        simple(&client, RANGES, 0).await.is_empty(),
        "wire, pg_range must be empty"
    );
    assert!(
        simple(&client, ENUMS, 0).await.is_empty(),
        "wire, pg_enum must be empty"
    );

    let messages = timeout(QUERY_TIMEOUT, client.simple_query(TYPEINFO))
        .await
        .expect("simple_query timeout")
        .expect("tokio-postgres' TYPEINFO shape must resolve");
    let rows: Vec<(String, Option<String>)> = messages
        .into_iter()
        .filter_map(|message| match message {
            SimpleQueryMessage::Row(row) => Some((
                row.get(0).unwrap_or_default().to_string(),
                row.get(1).map(str::to_string),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(rows.len(), 1, "exactly one row for oid 23, got {rows:?}");
    assert_eq!(
        rows.first().map(|(name, _)| name.as_str()),
        Some("int4"),
        "oid 23 is int4"
    );
    assert_eq!(
        rows.first().and_then(|(_, sub)| sub.clone()),
        None,
        "no pg_range row joins to int4, so rngsubtype is NULL"
    );
}

// ===========================================================================
// 13. A bound parameter's VALUE cannot change the route
// ===========================================================================

/// REGRESSION (review FIX-A / FIX-B): `pg_tables` is the one view this router
/// still answers itself, and the extended protocol classifies its WHERE clause
/// TWICE — at Parse, on text that still holds `$1` (that decision fixes the
/// RowDescription Describe sends), and again at Execute, on the text
/// `substitute_parameters` spliced the value into.
///
/// Before the fix the classifier read the RAW statement text, so the parameter
/// VALUE decided the route:
///
///   * `$1 = 'orders and returns'` — the ` and ` inside the value split the
///     predicate into two conjuncts, the second of which (`returns'`) matched
///     no supported shape. Execute therefore refused the interceptor and fell
///     through to the planner's EIGHT-column `pg_tables`, after Describe had
///     already announced the interceptor's FIVE. DataRows carrying more fields
///     than the RowDescription is a protocol violation — tokio-postgres
///     rejects such a row outright (`Row::new` compares the two counts),
///     node-postgres throws, pgjdbc silently truncates.
///   * `$1 = 'x is null'` — the ` is null` inside the value was read as an
///     `IS NULL` predicate on the "column" `tablename = 'x`, which resolves to
///     NULL for every row, so the answer was EVERY TABLE IN THE DATABASE
///     instead of the zero tables actually named `x is null`. That is the
///     reported HDB-011 symptom, reachable through the guard meant to stop it.
///
/// Both decisions are now made on the literal/comment-stripped copy, where a
/// parameter's value is blanked out: one route, one column count, one correct
/// answer — whatever the client binds.
///
/// `prepare_typed` rather than a bare `prepare`/`query` for the same reason as
/// every other parameterised case in this file (see the module doc): it states
/// `$1`'s type on the wire instead of relying on inference. It is still a full
/// Parse / Bind / Describe / Execute round trip — and preparing ONCE is what
/// lets the test compare every Execute against the single RowDescription
/// Describe actually sent.
#[tokio::test]
async fn hdb011_pg_tables_bound_parameter_cannot_change_the_route() {
    const SQL: &str = "SELECT * FROM pg_tables WHERE tablename = $1";

    let (client, _h) = server_client().await;
    client
        .batch_execute("CREATE TABLE a (id INT)")
        .await
        .expect("create table a");
    client
        .batch_execute("CREATE TABLE orders (id INT)")
        .await
        .expect("create table orders");

    // Sanity: both tables are visible without a predicate.
    let all = simple(&client, "SELECT tablename FROM pg_tables", 0).await;
    assert!(
        all.contains(&"a".to_string()) && all.contains(&"orders".to_string()),
        "pg_tables must list both tables, got {all:?}"
    );

    // ONE Parse/Describe, reused for every value — exactly the shape a driver
    // uses, and the shape in which Describe's answer is pinned before any
    // value exists. `prepare_typed` states `$1`'s type on the wire, so this
    // does not depend on parameter-type inference (see the module doc).
    let statement = timeout(QUERY_TIMEOUT, client.prepare_typed(SQL, &[Type::TEXT]))
        .await
        .expect("prepare timeout")
        .unwrap_or_else(|e| panic!("`{SQL}` must prepare: {e}"));
    let described_columns = statement.columns().len();
    assert!(
        statement.columns().iter().any(|c| c.name() == "tablename"),
        "Describe must announce a `tablename` column, got {:?}",
        statement
            .columns()
            .iter()
            .map(tokio_postgres::Column::name)
            .collect::<Vec<_>>()
    );

    for (param, expected) in [
        // Contains ` and ` — the route flip / protocol violation.
        ("orders and returns", Vec::<String>::new()),
        // An ordinary hit.
        ("a", vec!["a".to_string()]),
        // Reads like ` IS NULL ` — the widening to the whole catalog.
        ("x is null", Vec::<String>::new()),
    ] {
        let rows = timeout(QUERY_TIMEOUT, client.query(&statement, &[&param]))
            .await
            .expect("extended query timeout")
            .unwrap_or_else(|e| {
                panic!(
                    "`{SQL}` with $1 = `{param}` must not error — a DataRow field count that \
                     disagrees with the RowDescription Describe already sent is exactly what \
                     tokio-postgres reports here: {e}"
                )
            });

        for row in &rows {
            // tokio-postgres refuses to build a `Row` whose DataRow field
            // count differs from the statement's column count, so reaching
            // here at all is the real assertion; state it anyway so the
            // invariant is written down where the test can be read.
            assert_eq!(
                row.len(),
                row.columns().len(),
                "$1 = `{param}`: every DataRow must have exactly as many fields as Describe announced"
            );
            assert_eq!(
                row.columns().len(),
                described_columns,
                "$1 = `{param}`: the route (and so the column count) must not depend on the value"
            );
        }

        let names: Vec<String> = rows.iter().map(|row| row.get::<_, String>("tablename")).collect();
        assert_eq!(
            names, expected,
            "$1 = `{param}`: the predicate must be COMPARED against the value, never parsed as syntax"
        );
    }
}
