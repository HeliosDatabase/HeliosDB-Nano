//! Acceptance tests for the v3.28.0 quirks reported by KanttBan in
//! `/home/app/Personal/KanttBan/Kanttban/BUGS_HELIOSDB.md` (re-test
//! results section + new bugs #12–#18).
//!
//! v3.29.0 coverage:
//! - Bug #12 pg_policies + pg_matviews stub views
//! - Bug #13 schema-qualified "public"."tbl" REFERENCES
//! - Bug #14 DO $$ BEGIN … EXCEPTION WHEN duplicate_object … END $$
//! - Bug #15 extended-query UPDATE FK enforcement (HIGH)
//! - Bug #7  psql \d <table> col-count (DEFERRED in v3.28, fixed here)
//! - Bug #16 pg_database lists user-created tenants (\l shows them)

use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::{EmbeddedDatabase, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use tokio_postgres::{Client, NoTls};

// ---------- Bug #13: schema-qualified "public"."tbl" -----------------------

#[test]
fn create_table_with_schema_qualified_reference_works() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute(r#"CREATE TABLE "teams" ("id" integer PRIMARY KEY, "name" text)"#)
        .expect("teams");
    db.execute(
        r#"CREATE TABLE "tasks" (
            "id" integer PRIMARY KEY,
            "team_id" integer REFERENCES "public"."teams"("id"),
            "title" text
        )"#,
    )
    .expect(r#"REFERENCES "public"."teams" must resolve"#);

    db.execute(r#"INSERT INTO "teams" VALUES (1, 'a')"#).expect("ins team");
    db.execute(r#"INSERT INTO "tasks" VALUES (1, 1, 't1')"#)
        .expect("ins task");
}

#[test]
fn alter_table_add_constraint_with_schema_qualified_reference() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute(r#"CREATE TABLE "teams" ("id" integer PRIMARY KEY, "name" text)"#)
        .expect("teams");
    db.execute(r#"CREATE TABLE "tasks" ("id" integer PRIMARY KEY, "team_id" integer)"#)
        .expect("tasks");

    // The exact ALTER drizzle-kit emits.
    db.execute(
        r#"ALTER TABLE "tasks" ADD CONSTRAINT "tasks_team_id_teams_id_fk"
           FOREIGN KEY ("team_id") REFERENCES "public"."teams"("id")
           ON DELETE no action ON UPDATE no action"#,
    )
    .expect(r#"ALTER ADD CONSTRAINT REFERENCES "public"."teams" must resolve"#);

    db.execute(r#"INSERT INTO "teams" VALUES (1, 'a')"#).expect("ins team");
    db.execute(r#"INSERT INTO "tasks" VALUES (1, 1)"#).expect("valid child");
    let orphan = db.execute(r#"INSERT INTO "tasks" VALUES (2, 999)"#);
    assert!(orphan.is_err(), "FK from schema-qualified ALTER must reject orphan");
}

// ---------- Bug #15: extended-query UPDATE FK ------------------------------

#[test]
fn extended_query_update_enforces_fk() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE users (id integer PRIMARY KEY, name text)")
        .expect("users");
    db.execute("CREATE TABLE tasks (id integer PRIMARY KEY, assigned_to integer REFERENCES users(id))")
        .expect("tasks");
    db.execute("INSERT INTO users VALUES (1, 'a')").expect("user1");
    db.execute("INSERT INTO users VALUES (2, 'b')").expect("user2");
    db.execute("INSERT INTO tasks VALUES (1, 1)").expect("ok task");

    // execute_params is the embedded-API mirror of the PG-wire
    // extended-query path. A parameterised UPDATE that violates
    // the FK must error — same as the simple-query path.
    let valid = db.execute_params(
        "UPDATE tasks SET assigned_to = $1 WHERE id = $2",
        &[Value::Int4(2), Value::Int4(1)],
    );
    assert!(valid.is_ok(), "valid parameterised UPDATE must succeed: {valid:?}");

    let orphan = db.execute_params(
        "UPDATE tasks SET assigned_to = $1 WHERE id = $2",
        &[Value::Int4(99999), Value::Int4(1)],
    );
    assert!(
        orphan.is_err(),
        "Bug #15: parameterised UPDATE that violates FK must be rejected; got {orphan:?}"
    );
}

// ---------- Bug #14: DO $$ ... EXCEPTION WHEN duplicate_object ... END $$ ---
// DO blocks are a PG-WIRE surface — `handle_do_block` strips the `$$` wrapper
// and runs the body statement-by-statement, and the EXCEPTION clause is what
// makes drizzle-kit's idempotent migrations re-runnable. The embedded API has
// no DO surface and the MySQL listener has none either (its translator strips
// backticks and has no DO branch), so the only honest test is over the
// PostgreSQL wire. This used to be a `let _ = body;` smoke stub that asserted
// nothing at all.

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// A PostgreSQL wire listener over an in-memory database, plus the `Arc` the
/// test reads the committed state back through.
async fn pg_wire_server() -> (String, Arc<EmbeddedDatabase>, tokio::task::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test port");
    let addr = listener.local_addr().expect("test addr");
    drop(listener);

    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let config = PgServerConfig::with_address(addr);
    let server = PgServer::new(config, Arc::clone(&db)).expect("server");
    let handle = tokio::spawn(async move {
        let _ = server.serve().await;
    });

    tokio::time::sleep(Duration::from_millis(150)).await;
    let conn = format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port());
    (conn, db, handle)
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

async fn simple_ok(client: &Client, sql: &str) {
    timeout(QUERY_TIMEOUT, client.simple_query(sql))
        .await
        .unwrap_or_else(|_| panic!("query timeout: {sql}"))
        .unwrap_or_else(|e| panic!("query failed: {sql}: {e}"));
}

async fn simple_err(client: &Client, sql: &str) -> tokio_postgres::Error {
    timeout(QUERY_TIMEOUT, client.simple_query(sql))
        .await
        .unwrap_or_else(|_| panic!("query timeout: {sql}"))
        .err()
        .unwrap_or_else(|| panic!("expected an error from: {sql}"))
}

/// Bug #14, the shape drizzle-kit actually emits: every `ALTER` is wrapped so
/// a second run of the same migration is a no-op.
///
/// ```sql
/// DO $$ BEGIN
///   ALTER TABLE "tasks" ADD CONSTRAINT "tasks_slug_uq" UNIQUE (slug);
/// EXCEPTION WHEN duplicate_object THEN null;
/// END $$;
/// ```
///
/// The inner `ALTER` fails on the re-run (`constraint … already exists`) and
/// the EXCEPTION clause swallows it, so the client must be told the block
/// completed and the session must stay usable — the constraint is still there,
/// still enforcing exactly once.
#[tokio::test]
async fn do_block_exception_when_duplicate_object_is_caught() {
    let (conn_string, db, server_handle) = pg_wire_server().await;
    let client = connect(&conn_string).await;

    let create = r#"CREATE TABLE "tasks" (id integer PRIMARY KEY, slug text)"#;
    let add_unique = r#"ALTER TABLE "tasks" ADD CONSTRAINT "tasks_slug_uq" UNIQUE (slug)"#;
    let caught = r#"DO $$ BEGIN ALTER TABLE "tasks" ADD CONSTRAINT "tasks_slug_uq" UNIQUE (slug); EXCEPTION WHEN duplicate_object THEN null; END $$;"#;
    let uncaught = r#"DO $$ BEGIN ALTER TABLE "tasks" ADD CONSTRAINT "tasks_slug_uq" UNIQUE (slug); EXCEPTION WHEN undefined_table THEN null; END $$;"#;

    simple_ok(&client, create).await;
    simple_ok(&client, add_unique).await;

    // The re-run: the ALTER fails "already exists", `duplicate_object` names
    // that condition, so the block itself must SUCCEED.
    simple_ok(&client, caught).await;

    // CONTROL: the clause is really consulted, not a blanket swallow — an
    // exception name that does not cover "already exists" still reaches the
    // client as an error.
    let err = simple_err(&client, uncaught).await;
    assert!(err.code().is_some(), "an unmatched exception must reach the client");

    // The session is still usable, and the constraint the caught block tried
    // to re-add is still installed exactly once.
    simple_ok(&client, r#"INSERT INTO "tasks" VALUES (1, 'a')"#).await;
    let dup = simple_err(&client, r#"INSERT INTO "tasks" VALUES (2, 'a')"#).await;
    assert!(dup.code().is_some(), "the UNIQUE constraint must still reject a dup");

    let rows = db.query(r#"SELECT slug FROM "tasks""#, &[]).expect("select slug");
    assert_eq!(rows.len(), 1, "the rejected duplicate must not be stored");

    server_handle.abort();
}

// ---------- Bug #16: pg_database catalog lists user-created tenants -------
// Verified end-to-end via PG-wire in tests/server_mode_integration_test.rs;
// at the embedded layer we just check that handle_create_database persists
// the tenant. (Full \l verification needs the daemon harness.)

#[test]
fn create_database_registers_tenant() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE DATABASE test_db").expect("create");
    let tenants = db.tenant_manager.list_tenants();
    assert!(
        tenants.iter().any(|t| t.name.eq_ignore_ascii_case("test_db")),
        "CREATE DATABASE should register a tenant (Bug #16); got {:?}",
        tenants.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
}
