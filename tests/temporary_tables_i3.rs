//! Item I3 / sprinter `1703dba8e82d` — **`CREATE TEMPORARY TABLE` must create a
//! table only THIS session can see.**
//!
//! # The defect these tests were written against
//!
//! `Planner::plan` accepted `TEMPORARY` and threw it away. The planner said so
//! in its own words at `src/sql/planner.rs:3083-3086` and again at
//! `src/sql/planner.rs:6525-6527`:
//!
//! ```text
//! // `SelectInto` also carries `temporary` / `unlogged` / `table`
//! // (`SELECT … INTO TEMP t`, `… INTO TABLE t`). Those modifiers are
//! // accepted and ignored, matching how the plain `CREATE TABLE` path
//! // already ignores TEMPORARY / UNLOGGED — deliberately consistent.
//! ```
//!
//! and the `Statement::CreateTable` arm at `src/sql/planner.rs:1801-1825`
//! destructured every other field of `sqlparser`'s `CreateTable` while never
//! reading `create_table.temporary`. So `CREATE TEMPORARY TABLE t (…)` took the
//! byte-identical path a plain `CREATE TABLE t (…)` takes: one durable,
//! globally-visible row at the storage key `t`.
//!
//! Verified live on the v4.40.0 release binary on 2026-09-19: `CREATE TEMPORARY
//! TABLE tmp_probe (id INT)` on one connection, then `SELECT count(*) FROM
//! tmp_probe` on a **second, independent connection**, resolved the table and
//! answered `0`. PostgreSQL raises `42P01 undefined_table` for that second
//! session.
//!
//! The security-relevant half is the second sentence. A permanent table that
//! merely outlives its session is a leak; a table every other connection can
//! **read and write** is a channel between sessions that the SQL asked to be
//! private.
//!
//! # The shape of the fix these tests pin
//!
//! A temp table is stored under a per-backend schema key — `pg_temp_<backend>.t`
//! — which is structurally the same thing `SET search_path TO s; CREATE TABLE t`
//! already produces (`s.t`). Three consequences are asserted below, and each one
//! FAILS on the pre-fix tree:
//!
//! * another session resolving the bare name finds nothing → `42P01`
//!   ([`a_second_session_cannot_see_another_sessions_temp_table`]);
//! * two sessions may each hold their own `tmp_shared` with different rows
//!   ([`two_sessions_hold_the_same_temp_table_name_independently`]);
//! * the table is gone once its session ends, including on an abrupt
//!   disconnect ([`a_temp_table_dies_with_its_session`]).
//!
//! The control — [`a_permanent_table_of_the_same_shape_is_still_shared`] — is
//! what stops the fix from being "hide every table": a plain `CREATE TABLE` of
//! the same shape must stay visible to every connection.
//!
//! Catalog agreement is asserted separately
//! ([`another_sessions_temp_table_is_absent_from_every_catalog_surface`])
//! because this codebase routinely has TWO implementations of one catalog
//! surface — a registry/catalog path and a wire interceptor — and fixing only
//! one is the recurring defect.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;
use std::time::Duration;

use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::EmbeddedDatabase;
use tokio_postgres::error::SqlState;
use tokio_postgres::{Client, NoTls};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A live PG-wire server over an in-memory database. The listener task is
/// aborted on drop so each test leaves nothing behind.
struct Server {
    #[allow(dead_code)]
    db: Arc<EmbeddedDatabase>,
    port: u16,
    listener: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.listener.abort();
    }
}

async fn serve() -> Server {
    // No hardcoded port: bind :0, take the port the OS assigned, release it.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let server = PgServer::new(
        PgServerConfig::with_address(addr).with_max_connections(16),
        Arc::clone(&db),
    )
    .unwrap();
    let listener = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    Server {
        db,
        port: addr.port(),
        listener,
    }
}

/// One independent connection. The driver task is returned with the client so
/// the caller controls exactly when the connection dies — which is the whole
/// point of [`a_temp_table_dies_with_its_session`].
async fn connect(port: u16) -> (Client, tokio::task::JoinHandle<()>) {
    let cs = format!("host=127.0.0.1 port={port} user=i3 dbname=heliosdb");
    let (client, conn) = tokio_postgres::connect(&cs, NoTls).await.expect("connect");
    let task = tokio::spawn(async move { conn.await.map(|_| ()).unwrap_or(()) });
    (client, task)
}

/// The SQLSTATE of a failed statement, or a panic naming what came back
/// instead. A statement that SUCCEEDS is the pre-fix behaviour, so the panic
/// message says so explicitly.
async fn sqlstate_of(client: &Client, sql: &str) -> SqlState {
    match client.simple_query(sql).await {
        Ok(_) => panic!("`{sql}` SUCCEEDED; it must have raised 42P01 undefined_table"),
        Err(e) => e
            .as_db_error()
            .unwrap_or_else(|| panic!("`{sql}` failed without a DbError: {e}"))
            .code()
            .clone(),
    }
}

/// Every value of the first column of a simple query's rows.
async fn column0(client: &Client, sql: &str) -> Vec<String> {
    client
        .simple_query(sql)
        .await
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
        .into_iter()
        .filter_map(|m| match m {
            tokio_postgres::SimpleQueryMessage::Row(r) => Some(r.get(0).unwrap_or_default().to_string()),
            _ => None,
        })
        .collect()
}

/// The single scalar a `SELECT <agg>` returns.
async fn scalar(client: &Client, sql: &str) -> String {
    let rows = column0(client, sql).await;
    assert_eq!(rows.len(), 1, "`{sql}` returned {} rows, wanted 1", rows.len());
    rows[0].clone()
}

// ---------------------------------------------------------------------------
// The defect
// ---------------------------------------------------------------------------

/// **The item, stated as a test.** Session A creates a temp table; session B
/// must not be able to name it.
///
/// Pre-fix this answered `0` — the table resolved, because `TEMPORARY` was
/// discarded at `src/sql/planner.rs:1801` and the storage key was the ordinary
/// bare `tmp_probe`.
#[tokio::test]
async fn a_second_session_cannot_see_another_sessions_temp_table() {
    let server = serve().await;
    let (a, _ta) = connect(server.port).await;
    let (b, _tb) = connect(server.port).await;

    a.simple_query("CREATE TEMPORARY TABLE tmp_probe (id INT)")
        .await
        .expect("CREATE TEMPORARY TABLE");
    a.simple_query("INSERT INTO tmp_probe VALUES (1), (2)")
        .await
        .expect("insert into own temp table");

    // The creating session sees its own rows.
    assert_eq!(scalar(&a, "SELECT count(*) FROM tmp_probe").await, "2");

    // The second session must not resolve the name at all.
    assert_eq!(
        sqlstate_of(&b, "SELECT count(*) FROM tmp_probe").await,
        SqlState::UNDEFINED_TABLE,
        "session B resolved session A's temp table"
    );
    // …and must not be able to WRITE through it either, which is the half that
    // makes this a security item rather than a tidiness one.
    assert_eq!(
        sqlstate_of(&b, "INSERT INTO tmp_probe VALUES (99)").await,
        SqlState::UNDEFINED_TABLE,
        "session B wrote into session A's temp table"
    );
    assert_eq!(
        sqlstate_of(&b, "DROP TABLE tmp_probe").await,
        SqlState::UNDEFINED_TABLE,
        "session B dropped session A's temp table"
    );

    // Session A is untouched by any of B's attempts.
    assert_eq!(scalar(&a, "SELECT count(*) FROM tmp_probe").await, "2");
}

/// `TEMP` is PostgreSQL's accepted abbreviation and must take the same path.
#[tokio::test]
async fn the_temp_abbreviation_takes_the_same_path() {
    let server = serve().await;
    let (a, _ta) = connect(server.port).await;
    let (b, _tb) = connect(server.port).await;

    a.simple_query("CREATE TEMP TABLE tmp_abbrev (id INT)")
        .await
        .expect("CREATE TEMP TABLE");
    assert_eq!(scalar(&a, "SELECT count(*) FROM tmp_abbrev").await, "0");
    assert_eq!(
        sqlstate_of(&b, "SELECT count(*) FROM tmp_abbrev").await,
        SqlState::UNDEFINED_TABLE,
    );
}

/// Naming, stated as a test: two sessions may each hold `tmp_shared`, and the
/// rows must not mix. PostgreSQL gives each backend its own `pg_temp_NNN`
/// schema for exactly this reason.
///
/// Pre-fix the second `CREATE` failed with `Table 'tmp_shared' already exists`
/// — the collision the item describes.
#[tokio::test]
async fn two_sessions_hold_the_same_temp_table_name_independently() {
    let server = serve().await;
    let (a, _ta) = connect(server.port).await;
    let (b, _tb) = connect(server.port).await;

    a.simple_query("CREATE TEMPORARY TABLE tmp_shared (id INT)")
        .await
        .expect("session A CREATE TEMPORARY TABLE");
    b.simple_query("CREATE TEMPORARY TABLE tmp_shared (id INT)")
        .await
        .expect("session B must get its OWN tmp_shared, not a collision");

    a.simple_query("INSERT INTO tmp_shared VALUES (10), (11), (12)")
        .await
        .unwrap();
    b.simple_query("INSERT INTO tmp_shared VALUES (20)").await.unwrap();

    assert_eq!(scalar(&a, "SELECT count(*) FROM tmp_shared").await, "3");
    assert_eq!(scalar(&b, "SELECT count(*) FROM tmp_shared").await, "1");
    assert_eq!(scalar(&a, "SELECT min(id) FROM tmp_shared").await, "10");
    assert_eq!(scalar(&b, "SELECT min(id) FROM tmp_shared").await, "20");

    // A drop on one side leaves the other side's table alone.
    b.simple_query("DROP TABLE tmp_shared").await.unwrap();
    assert_eq!(scalar(&a, "SELECT count(*) FROM tmp_shared").await, "3");
}

/// The creating session must be able to use its own temp table for everything a
/// scratch buffer is for — the point of the feature, not just its isolation.
#[tokio::test]
async fn the_creating_session_reads_and_writes_its_own_temp_table() {
    let server = serve().await;
    let (a, _ta) = connect(server.port).await;

    a.simple_query("CREATE TEMPORARY TABLE tmp_rw (id INT PRIMARY KEY, label TEXT)")
        .await
        .unwrap();
    a.simple_query("INSERT INTO tmp_rw VALUES (1, 'one'), (2, 'two')")
        .await
        .unwrap();
    a.simple_query("UPDATE tmp_rw SET label = 'ONE' WHERE id = 1")
        .await
        .unwrap();
    assert_eq!(scalar(&a, "SELECT label FROM tmp_rw WHERE id = 1").await, "ONE");
    a.simple_query("DELETE FROM tmp_rw WHERE id = 2").await.unwrap();
    assert_eq!(scalar(&a, "SELECT count(*) FROM tmp_rw").await, "1");

    // A temp table joins a permanent one, which is the whole use case.
    a.simple_query("CREATE TABLE perm_join (id INT, note TEXT)")
        .await
        .unwrap();
    a.simple_query("INSERT INTO perm_join VALUES (1, 'kept')")
        .await
        .unwrap();
    assert_eq!(
        scalar(&a, "SELECT p.note FROM tmp_rw t JOIN perm_join p ON p.id = t.id").await,
        "kept"
    );

    // An explicit DROP of one's own temp table works.
    a.simple_query("DROP TABLE tmp_rw").await.unwrap();
    assert_eq!(
        sqlstate_of(&a, "SELECT count(*) FROM tmp_rw").await,
        SqlState::UNDEFINED_TABLE,
    );
}

/// Lifetime, stated as a test: the table is gone when the session ends, and the
/// disconnect here is ABRUPT — the driver task is aborted rather than sending a
/// Terminate — because that is the case that leaks.
#[tokio::test]
async fn a_temp_table_dies_with_its_session() {
    let server = serve().await;
    let (a, ta) = connect(server.port).await;

    a.simple_query("CREATE TEMPORARY TABLE tmp_doomed (id INT)")
        .await
        .unwrap();
    a.simple_query("INSERT INTO tmp_doomed VALUES (1)").await.unwrap();
    assert_eq!(scalar(&a, "SELECT count(*) FROM tmp_doomed").await, "1");

    // Kill the connection without a clean Terminate.
    drop(a);
    ta.abort();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // A brand-new session must not find it — not by name…
    let (c, _tc) = connect(server.port).await;
    assert_eq!(
        sqlstate_of(&c, "SELECT count(*) FROM tmp_doomed").await,
        SqlState::UNDEFINED_TABLE,
        "a temp table outlived the session that created it",
    );
    // …and not through the catalog either, which is where a leaked temp table
    // would resurface as a permanent, globally-visible relation.
    let listed = column0(&c, "SELECT tablename FROM pg_tables").await;
    assert!(
        !listed.iter().any(|t| t == "tmp_doomed"),
        "a dead session's temp table is still catalogued: {listed:?}"
    );
}

/// **The control.** The fix must not hide ordinary tables. A permanent table of
/// exactly the same shape stays visible to, and writable by, every connection.
#[tokio::test]
async fn a_permanent_table_of_the_same_shape_is_still_shared() {
    let server = serve().await;
    let (a, _ta) = connect(server.port).await;
    let (b, _tb) = connect(server.port).await;

    a.simple_query("CREATE TABLE perm_probe (id INT)").await.unwrap();
    a.simple_query("INSERT INTO perm_probe VALUES (1), (2)").await.unwrap();

    assert_eq!(
        scalar(&b, "SELECT count(*) FROM perm_probe").await,
        "2",
        "the control table stopped being shared"
    );
    b.simple_query("INSERT INTO perm_probe VALUES (3)").await.unwrap();
    assert_eq!(scalar(&a, "SELECT count(*) FROM perm_probe").await, "3");

    let listed = column0(&b, "SELECT tablename FROM pg_tables").await;
    assert!(
        listed.iter().any(|t| t == "perm_probe"),
        "the control table vanished from pg_tables: {listed:?}"
    );
}

/// Catalog visibility across BOTH implementations of the table list — the
/// registry/`system_views` path (`information_schema.tables`) and the wire
/// interceptor (`pg_tables` / `pg_class`, which is also what `psql \dt` runs).
///
/// Pre-fix every one of these listed `tmp_cat` to session B, because the table
/// was catalogued under the ordinary bare key.
#[tokio::test]
async fn another_sessions_temp_table_is_absent_from_every_catalog_surface() {
    let server = serve().await;
    let (a, _ta) = connect(server.port).await;
    let (b, _tb) = connect(server.port).await;

    a.simple_query("CREATE TEMPORARY TABLE tmp_cat (id INT)").await.unwrap();
    a.simple_query("CREATE TABLE perm_cat (id INT)").await.unwrap();

    for probe in [
        "SELECT tablename FROM pg_tables",
        "SELECT table_name FROM information_schema.tables",
        "SELECT relname FROM pg_class",
    ] {
        let listed = column0(&b, probe).await;
        assert!(
            !listed.iter().any(|t| t == "tmp_cat"),
            "`{probe}` leaked session A's temp table to session B: {listed:?}"
        );
        assert!(
            listed.iter().any(|t| t == "perm_cat"),
            "`{probe}` lost the permanent control table: {listed:?}"
        );
    }

    // The schema the temp table lives in must not surface either — a
    // `pg_temp_NNN` row in `pg_namespace` names another backend's private
    // namespace.
    let schemas = column0(&b, "SELECT nspname FROM pg_namespace").await;
    assert!(
        !schemas.iter().any(|s| s.starts_with("pg_temp_")),
        "another session's temp schema is listed in pg_namespace: {schemas:?}"
    );
}

/// The catalog is not the only shared surface a temp table can leak through.
///
/// The plan cache and the result cache are keyed by SQL TEXT and shared by
/// every session, so `SELECT count(*) FROM tmp_cache` is ONE cache entry for
/// two connections whose `tmp_cache` is a different table. Repeating the
/// identical statement on alternating connections is what drives the caches'
/// repeat-sighting admission, so this is the shape that actually exercises
/// them — a single round trip each would never admit anything.
///
/// This is why a session holding a temp table must bypass those caches, the
/// same way a session under a non-`public` `search_path` already does.
#[tokio::test]
async fn the_shared_caches_never_serve_one_sessions_temp_rows_to_another() {
    let server = serve().await;
    let (a, _ta) = connect(server.port).await;
    let (b, _tb) = connect(server.port).await;

    a.simple_query("CREATE TEMPORARY TABLE tmp_cache (id INT)")
        .await
        .unwrap();
    b.simple_query("CREATE TEMPORARY TABLE tmp_cache (id INT)")
        .await
        .unwrap();
    a.simple_query("INSERT INTO tmp_cache VALUES (1), (2), (3)")
        .await
        .unwrap();
    b.simple_query("INSERT INTO tmp_cache VALUES (9)").await.unwrap();

    // Alternate the SAME statement text several times: by the third sighting a
    // shared cache would have admitted one session's plan/rows and be serving
    // them to the other.
    for round in 0..4 {
        assert_eq!(
            scalar(&a, "SELECT count(*) FROM tmp_cache").await,
            "3",
            "round {round}: session A got another session's rows"
        );
        assert_eq!(
            scalar(&b, "SELECT count(*) FROM tmp_cache").await,
            "1",
            "round {round}: session B got another session's rows"
        );
        assert_eq!(scalar(&a, "SELECT max(id) FROM tmp_cache").await, "3");
        assert_eq!(scalar(&b, "SELECT max(id) FROM tmp_cache").await, "9");
    }

    // A third connection, which owns no `tmp_cache` at all, must still get
    // 42P01 rather than whichever plan the cache happens to hold.
    let (c, _tc) = connect(server.port).await;
    assert_eq!(
        sqlstate_of(&c, "SELECT count(*) FROM tmp_cache").await,
        SqlState::UNDEFINED_TABLE,
    );
}

/// Hiding a temp table from every LISTING is not enough on its own: a client
/// can spell the private key out. `pg_temp_<n>.t` naming another backend's
/// namespace must not resolve, and the refusal must be the ordinary `42P01` —
/// an error that said "exists but is not yours" would confirm the table to the
/// session it is being hidden from.
///
/// The session's OWN table is still reachable through PostgreSQL's `pg_temp.`
/// alias, which is the same check from the other side.
#[tokio::test]
async fn a_spelled_out_temp_key_does_not_cross_sessions() {
    let server = serve().await;
    let (a, _ta) = connect(server.port).await;
    let (b, _tb) = connect(server.port).await;

    a.simple_query("CREATE TEMPORARY TABLE tmp_named (id INT)")
        .await
        .unwrap();
    a.simple_query("INSERT INTO tmp_named VALUES (1)").await.unwrap();

    // `pg_temp.` resolves to the ASKING session's namespace.
    assert_eq!(scalar(&a, "SELECT count(*) FROM pg_temp.tmp_named").await, "1");
    assert_eq!(
        sqlstate_of(&b, "SELECT count(*) FROM pg_temp.tmp_named").await,
        SqlState::UNDEFINED_TABLE,
        "pg_temp resolved to another session's namespace"
    );

    // Session A's backend pid is public (`pg_backend_pid()`), so B can compute
    // A's schema name — and still must not read through it.
    let pid = scalar(&a, "SELECT pg_backend_pid()").await;
    let spelled = format!("SELECT count(*) FROM pg_temp_{pid}.tmp_named");
    assert_eq!(
        sqlstate_of(&b, &spelled).await,
        SqlState::UNDEFINED_TABLE,
        "a spelled-out pg_temp_<pid> key read another session's table"
    );
    let spelled_write = format!("INSERT INTO pg_temp_{pid}.tmp_named VALUES (2)");
    assert_eq!(
        sqlstate_of(&b, &spelled_write).await,
        SqlState::UNDEFINED_TABLE,
        "a spelled-out pg_temp_<pid> key wrote another session's table"
    );
    assert_eq!(scalar(&a, "SELECT count(*) FROM tmp_named").await, "1");
}

/// The reserved namespace is closed from the other direction too: a plain
/// `CREATE TABLE` must not be able to PLANT a relation inside a live backend's
/// private schema, and a `CREATE TEMPORARY TABLE` must not be able to name a
/// non-temporary schema. Both are refused loudly rather than half-honoured.
#[tokio::test]
async fn the_temp_namespace_cannot_be_named_by_an_ordinary_create() {
    let server = serve().await;
    let (a, _ta) = connect(server.port).await;
    let (b, _tb) = connect(server.port).await;

    a.simple_query("CREATE TEMPORARY TABLE tmp_guard (id INT)")
        .await
        .unwrap();
    let pid = scalar(&a, "SELECT pg_backend_pid()").await;

    let plant = format!("CREATE TABLE pg_temp_{pid}.planted (id INT)");
    assert_eq!(
        sqlstate_of(&b, &plant).await,
        SqlState::RESERVED_NAME, // 42939
        "a plain CREATE planted a table in another backend's temp schema"
    );

    // And a temp table may not be named into an ordinary schema — PostgreSQL's
    // 42P16 invalid_table_definition.
    b.simple_query("CREATE SCHEMA app").await.unwrap();
    assert_eq!(
        sqlstate_of(&b, "CREATE TEMPORARY TABLE app.confused (id INT)").await,
        SqlState::INVALID_TABLE_DEFINITION, // 42P16
        "CREATE TEMPORARY TABLE accepted a non-temporary schema qualifier"
    );

    // Session A's own temp table is untouched by either attempt.
    assert_eq!(scalar(&a, "SELECT count(*) FROM tmp_guard").await, "0");
}

/// `SELECT … INTO TEMP t` is the other spelling of the same statement
/// (`src/sql/planner.rs:3083`), and it must land on the same path.
#[tokio::test]
async fn select_into_temp_takes_the_same_path() {
    let server = serve().await;
    let (a, _ta) = connect(server.port).await;
    let (b, _tb) = connect(server.port).await;

    a.simple_query("CREATE TABLE into_src (id INT)").await.unwrap();
    a.simple_query("INSERT INTO into_src VALUES (1), (2), (3)")
        .await
        .unwrap();
    a.simple_query("SELECT * INTO TEMP tmp_into FROM into_src")
        .await
        .expect("SELECT … INTO TEMP");

    assert_eq!(scalar(&a, "SELECT count(*) FROM tmp_into").await, "3");
    assert_eq!(
        sqlstate_of(&b, "SELECT count(*) FROM tmp_into").await,
        SqlState::UNDEFINED_TABLE,
        "SELECT … INTO TEMP produced a globally-visible table"
    );
    // `SELECT … INTO t` with no TEMP is still an ordinary shared table.
    a.simple_query("SELECT * INTO into_perm FROM into_src").await.unwrap();
    assert_eq!(scalar(&b, "SELECT count(*) FROM into_perm").await, "3");
}

/// A temp table must not shadow a permanent table of the same name for OTHER
/// sessions, and must shadow it for its own (PostgreSQL puts `pg_temp` ahead of
/// `public` in the implicit search path).
#[tokio::test]
async fn a_temp_table_shadows_a_permanent_one_only_for_its_own_session() {
    let server = serve().await;
    let (a, _ta) = connect(server.port).await;
    let (b, _tb) = connect(server.port).await;

    a.simple_query("CREATE TABLE shadowed (id INT)").await.unwrap();
    a.simple_query("INSERT INTO shadowed VALUES (1), (2), (3), (4)")
        .await
        .unwrap();

    a.simple_query("CREATE TEMPORARY TABLE shadowed (id INT)")
        .await
        .expect("a temp table may share a permanent table's name");
    a.simple_query("INSERT INTO shadowed VALUES (7)").await.unwrap();

    // A resolves to its temp table…
    assert_eq!(scalar(&a, "SELECT count(*) FROM shadowed").await, "1");
    // …and can still reach the permanent one by qualifying it.
    assert_eq!(scalar(&a, "SELECT count(*) FROM public.shadowed").await, "4");
    // B only ever sees the permanent one.
    assert_eq!(scalar(&b, "SELECT count(*) FROM shadowed").await, "4");
}

/// Fail-closed across a RESTART. A temp table left in the catalog by a crash
/// must not come back as a permanent, globally-visible relation when the
/// database is reopened — which is exactly what would happen if the only
/// cleanup were the session-teardown hook.
///
/// Driven through the embedded API on a file-backed directory so the process
/// boundary is real: the first handle is dropped and a second one opened over
/// the same bytes.
#[test]
fn a_leaked_temp_table_does_not_survive_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_path_buf();

    {
        let db = EmbeddedDatabase::new(&path).expect("open");
        db.execute("CREATE TEMPORARY TABLE tmp_restart (id INT)").unwrap();
        db.execute("INSERT INTO tmp_restart VALUES (1)").unwrap();
        db.execute("CREATE TABLE perm_restart (id INT)").unwrap();
        db.execute("INSERT INTO perm_restart VALUES (1)").unwrap();
        // Leak it deliberately: no DROP and no session teardown. The embedded
        // API has no `Session`, so nothing runs the per-session cleanup here —
        // which is precisely the "abrupt crash left a temp table behind" state
        // the restart sweep has to answer for. Dropping the handle normally is
        // required (RocksDB's LOCK must be released before the reopen below).
        drop(db);
    }

    let db = EmbeddedDatabase::new(&path).expect("reopen");
    let tables = db.storage.catalog().list_tables().expect("list_tables");
    assert!(
        !tables.iter().any(|t| t.contains("tmp_restart")),
        "a leaked temp table survived the restart and is now a permanent table: {tables:?}"
    );
    assert!(
        tables.iter().any(|t| t == "perm_restart"),
        "the restart sweep ate a permanent table: {tables:?}"
    );
    assert!(
        db.query("SELECT count(*) FROM perm_restart", &[]).is_ok(),
        "the permanent control table is unreadable after the restart sweep"
    );
}
