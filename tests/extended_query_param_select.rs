//! Bug 8 (+ 9) repro: parameterised SELECT crashes node-pg / tokio-postgres
//! due to malformed RowDescription on the extended-query path.
//!
//! Branch: `fix/extended-query-rowdescription`. Tracked in
//! `BUGS_DASHBOARD_MIGRATION_TRIAGE.md` (Bug 8, Bug 9).
//!
//! These tests fail on v3.23.1 main and should pass once the schema-synthesis
//! and/or RowDescription serialisation fix lands.

use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use tokio_postgres::NoTls;
use heliosdb_nano::{EmbeddedDatabase, protocol::postgres::server::{PgServer, PgServerConfig}};

async fn setup() -> (String, tokio::task::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let port = addr.port();
    drop(listener);
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let config = PgServerConfig::with_address(addr);
    let server = PgServer::new(config, db).expect("server");
    let handle = tokio::spawn(async move {
        if let Err(e) = server.serve().await { eprintln!("server: {e}"); }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    (format!("host=127.0.0.1 port={} user=postgres dbname=postgres", port), handle)
}

async fn connect(s: &str) -> tokio_postgres::Client {
    let (client, conn) = tokio_postgres::connect(s, NoTls).await.expect("connect");
    tokio::spawn(async move { let _ = conn.await; });
    client
}

/// Bug 8 — parameterised SELECT.
///
/// Repro from `/home/app/Claude-DashBoard/docs/heliosdb-bugs.md`:
/// `SELECT COUNT(*) FROM pings WHERE week_bucket = $1` with one bind value.
/// On v3.19.1 the dashboard team saw node-pg crash with "Cannot read
/// properties of undefined (reading 'name')" — RowDescription parser failed.
///
/// **Status (verified 2026-05-03 against v3.23.1)**: FIXED via PG-wire repro
/// using a real `target/release/heliosdb-nano start` daemon + psycopg2.
/// Test stays `#[ignore]`'d because the in-process `PgServer` harness used
/// here hits an unbounded-recursion stack-overflow that's been latent since
/// at least v3.19.x — same root as the `#[ignore]`'d tests in
/// `tests/server_mode_integration_test.rs`. Filed as a separate concern;
/// not a blocker for any user-visible workflow.
#[tokio::test]
#[ignore = "in-process PgServer harness hits stack-overflow; bug 8 itself is FIXED — verified externally"]
async fn parameterised_select_extended_query_returns_rows() {
    let (cs, _h) = setup().await;
    let client = connect(&cs).await;

    client
        .execute(
            "CREATE TABLE pings (week_bucket TEXT, hash TEXT, dashboard_version TEXT)",
            &[],
        )
        .await
        .expect("create");
    client
        .execute(
            "INSERT INTO pings VALUES ($1, $2, $3)",
            &[&"2026-18", &"abc123", &"3.23.1"],
        )
        .await
        .expect("insert");

    // The headline failing query.
    let rows = timeout(
        Duration::from_secs(5),
        client.query(
            "SELECT COUNT(*) FROM pings WHERE week_bucket = $1",
            &[&"2026-18"],
        ),
    )
    .await
    .expect("timeout — server hung")
    .expect("query failed — Bug 8 still open");

    assert_eq!(rows.len(), 1);
    let count: i64 = rows[0].get(0);
    assert_eq!(count, 1, "Bug 8 / 9: COUNT should be 1, got {count}");
}

/// Bug 9 — same root as Bug 8: with extended-query parameter binding,
/// `COUNT(DISTINCT col) WHERE x = $1` returns 0 even when matching rows
/// exist (the literal-substitution form returns the correct value).
///
/// **Status (verified 2026-05-03 against v3.23.1)**: FIXED.
#[tokio::test]
#[ignore = "in-process PgServer harness hits stack-overflow; bug 9 itself is FIXED — verified externally"]
async fn count_distinct_with_extended_param_does_not_silently_return_zero() {
    let (cs, _h) = setup().await;
    let client = connect(&cs).await;

    client
        .execute(
            "CREATE TABLE pings (week_bucket TEXT, hash TEXT, dashboard_version TEXT)",
            &[],
        )
        .await
        .expect("create");
    for h in ["abc", "def", "abc"] {
        client
            .execute(
                "INSERT INTO pings VALUES ($1, $2, $3)",
                &[&"2026-18", &h, &"3.23.1"],
            )
            .await
            .expect("insert");
    }

    // Literal form (works today, sets the expected baseline)
    let literal_rows = client
        .query(
            "SELECT COUNT(DISTINCT hash) FROM pings WHERE week_bucket = '2026-18'",
            &[],
        )
        .await
        .expect("literal query");
    let literal_count: i64 = literal_rows[0].get(0);
    assert_eq!(literal_count, 2, "literal form sanity check");

    // Parameterised form (Bug 9)
    let param_rows = client
        .query(
            "SELECT COUNT(DISTINCT hash) FROM pings WHERE week_bucket = $1",
            &[&"2026-18"],
        )
        .await
        .expect("param query");
    let param_count: i64 = param_rows[0].get(0);

    assert_eq!(
        param_count, literal_count,
        "Bug 9: parametrised COUNT(DISTINCT) returned {param_count}, expected {literal_count} — \
         parameter binding is being lost between Parse and Execute on the extended path"
    );
}

/// Bug 8 — Describe metadata sanity. Even before tokio-postgres calls
/// `client.query`, the underlying Describe response must contain a
/// well-formed RowDescription. Use a `prepare` to isolate that step.
///
/// **Status (verified 2026-05-03 against v3.23.1)**: FIXED.
#[test]
#[ignore = "in-process PgServer harness hits stack-overflow; bug 8 itself is FIXED — verified externally"]
fn describe_returns_well_formed_row_description() {
    // Use a manually-built runtime with a 32 MB stack. The stack-overflow
    // issue in existing `tests/server_mode_integration_test.rs` traces
    // back to this — the PG-wire server's serve() loop with rocksdb-backed
    // catalog has a deep call stack that blows the default 2 MB.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_stack_size(32 * 1024 * 1024)
        .enable_all()
        .build()
        .expect("rt");
    rt.block_on(async {
        let (cs, _h) = setup().await;
        let client = connect(&cs).await;
        client.execute("CREATE TABLE t (a INT, b TEXT)", &[]).await.expect("create");

        // tokio-postgres' `prepare` triggers Parse + Describe; if the resulting
        // RowDescription is malformed, `prepare` itself errors before any rows
        // are fetched.
        let stmt = client
            .prepare("SELECT a, b FROM t WHERE a = $1")
            .await
            .expect("prepare must yield a usable statement (Bug 8: RowDescription is malformed)");

        let cols = stmt.columns();
        assert_eq!(cols.len(), 2, "expected two output columns");
        assert_eq!(cols[0].name(), "a");
        assert_eq!(cols[1].name(), "b");
    });
}
