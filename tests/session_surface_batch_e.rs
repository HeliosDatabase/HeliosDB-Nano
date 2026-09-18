//! Batch E — session-scoped identity / metadata readable from SQL.
//!
//! Two sprinter items, one theme: a client must be able to ask the server
//! "which connection am I, and what did *my* connection just do?" and get an
//! answer that is TRUE FOR THIS CONNECTION ONLY.
//!
//! * **E1 — sprinter 6dc0cc115db9** — `LASTVAL()`. `nextval`/`currval`/`setval`
//!   exist; `lastval` did not, so every DB-API-2.0 shim's `cursor.lastrowid`
//!   (including the `heliosdb_sqlite` sqlite3 drop-in, where `lastrowid` is
//!   core to the emulated interface) was permanently `None`. `currval('seq')`
//!   is not a substitute: a generic driver does not know the sequence name.
//!
//! * **E2 — sprinter f4f5d450e816** — `pg_backend_pid()` and the
//!   `application_name` GUC. The first is how a pool proves two statements ran
//!   on the same backend; the second is set by psycopg / JDBC / node-postgres /
//!   sqlx on EVERY connect and was silently dropped.
//!
//! # What these tests pin that a "global" implementation would fail
//!
//! The whole point of both items is SESSION scope. A process-global
//! "last nextval" would hand connection B connection A's row id — strictly
//! worse than the absence it replaces — so `two_sessions_never_see_each_others_*`
//! are the load-bearing tests here, not the happy paths.
//!
//! # Coverage shape
//!
//! Every behaviour is pinned on BOTH executor families (text-SQL
//! `query()`/`execute()`, and bound-params `query_params()`/`execute_params()`
//! via `parameterized_plan_cached`, which is also what the PostgreSQL extended
//! protocol reaches) and on BOTH wires (PostgreSQL, MySQL).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;
use std::time::Duration;

use heliosdb_nano::protocol::mysql::MySqlHandler;
use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::protocol::postgres::timeouts::ConnectionTimeouts;
use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::{EmbeddedDatabase, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio_postgres::NoTls;

// ---------------------------------------------------------------------------
// Embedded harness
// ---------------------------------------------------------------------------

fn db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().unwrap()
}

/// Which executor family a shared assertion runs on.
///
/// The GUC work in GH#28 shipped a feature that silently worked on only one of
/// these, because the text family had a pre-parse path the params family did
/// not. Every shared behaviour here is therefore parameterised over the family
/// rather than written once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    /// `query()` / `execute()` — literal text SQL.
    Text,
    /// `query_params()` / `execute_params()` — the bound-parameter planner
    /// (`parameterized_plan_cached`), which the PG extended protocol reaches.
    Params,
}

impl Family {
    fn query(
        self,
        db: &EmbeddedDatabase,
        sid: SessionId,
        sql: &str,
    ) -> heliosdb_nano::Result<Vec<heliosdb_nano::Tuple>> {
        match self {
            Family::Text => db.query_with_columns_for_session(sid, sql).map(|(rows, _)| rows),
            Family::Params => db.query_params_for_session(sid, sql, &[]),
        }
    }

    fn execute(self, db: &EmbeddedDatabase, sid: SessionId, sql: &str) -> heliosdb_nano::Result<u64> {
        match self {
            Family::Text => db.execute_for_session(sid, sql),
            Family::Params => db.execute_params_for_session(sid, sql, &[]),
        }
    }

    /// One-row, one-column scalar as `i64`.
    fn scalar_i64(self, db: &EmbeddedDatabase, sid: SessionId, sql: &str) -> i64 {
        let rows = self
            .query(db, sid, sql)
            .unwrap_or_else(|e| panic!("{self:?} `{sql}`: {e}"));
        assert_eq!(rows.len(), 1, "{self:?} `{sql}` returned {} rows", rows.len());
        match &rows[0].values[0] {
            Value::Int8(v) => *v,
            Value::Int4(v) => i64::from(*v),
            Value::Int2(v) => i64::from(*v),
            other => panic!("{self:?} `{sql}` returned {other:?}, expected an integer"),
        }
    }

    /// One-row, one-column scalar as text (NULL renders as the empty string,
    /// which is also `application_name`'s default — the tests that care
    /// distinguish by asserting a non-empty value).
    fn scalar_text(self, db: &EmbeddedDatabase, sid: SessionId, sql: &str) -> String {
        let rows = self
            .query(db, sid, sql)
            .unwrap_or_else(|e| panic!("{self:?} `{sql}`: {e}"));
        assert_eq!(rows.len(), 1, "{self:?} `{sql}` returned {} rows", rows.len());
        match &rows[0].values[0] {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => panic!("{self:?} `{sql}` returned {other:?}, expected text"),
        }
    }
}

const FAMILIES: [Family; 2] = [Family::Text, Family::Params];

// ===========================================================================
// E1 — sprinter 6dc0cc115db9 — LASTVAL()
// ===========================================================================

/// PIN (E1): a FRESH session must ERROR — not return 0, not NULL, and above
/// all not another session's value. PostgreSQL's wording and SQLSTATE 55000
/// (`object_not_in_prerequisite_state`).
#[test]
fn lastval_on_a_fresh_session_errors_55000() {
    for family in FAMILIES {
        let db = db();
        let sid = db.create_wire_session("fresh").unwrap();
        let err = family.query(&db, sid, "SELECT lastval()").expect_err(&format!(
            "{family:?}: lastval() on a fresh session must error, not answer"
        ));
        let msg = err.to_string();
        assert!(
            msg.contains("lastval is not yet defined in this session"),
            "{family:?}: wrong message for an undefined lastval: {msg}"
        );
        db.destroy_session(sid).unwrap();
    }
}

/// PIN (E1): after `nextval`, `lastval()` returns exactly what `nextval`
/// returned — on both executor families.
#[test]
fn lastval_returns_what_nextval_just_produced_on_both_families() {
    for family in FAMILIES {
        let db = db();
        db.execute("CREATE SEQUENCE s_e1").unwrap();
        let sid = db.create_wire_session("u").unwrap();

        let first = family.scalar_i64(&db, sid, "SELECT nextval('s_e1')");
        assert_eq!(family.scalar_i64(&db, sid, "SELECT lastval()"), first, "{family:?}");

        let second = family.scalar_i64(&db, sid, "SELECT nextval('s_e1')");
        assert_eq!(second, first + 1, "{family:?}: sequence did not advance");
        assert_eq!(
            family.scalar_i64(&db, sid, "SELECT lastval()"),
            second,
            "{family:?}: lastval() is stale"
        );
        db.destroy_session(sid).unwrap();
    }
}

/// PIN (E1): `lastval()` does not need the sequence NAME — it follows whichever
/// sequence `nextval` touched most recently. That is the entire reason a
/// generic driver can use it and cannot use `currval`.
#[test]
fn lastval_follows_the_most_recent_sequence_not_a_named_one() {
    for family in FAMILIES {
        let db = db();
        db.execute("CREATE SEQUENCE s_a").unwrap();
        db.execute("CREATE SEQUENCE s_b START WITH 500").unwrap();
        let sid = db.create_wire_session("u").unwrap();

        family.scalar_i64(&db, sid, "SELECT nextval('s_a')");
        let b = family.scalar_i64(&db, sid, "SELECT nextval('s_b')");
        assert_eq!(
            family.scalar_i64(&db, sid, "SELECT lastval()"),
            b,
            "{family:?}: lastval() must track the LAST sequence advanced, not the first"
        );
        db.destroy_session(sid).unwrap();
    }
}

/// PIN (E1): **the actual use case.** `INSERT INTO t (name) VALUES ('a')` on a
/// SERIAL table, then `LASTVAL()` must return that row's id. An implementation
/// that only hooks an explicit `nextval()` call fails this and is useless to
/// `cursor.lastrowid`.
#[test]
fn lastval_after_a_serial_insert_returns_that_rows_id() {
    for family in FAMILIES {
        let db = db();
        db.execute("CREATE TABLE t_e1 (id SERIAL PRIMARY KEY, name TEXT)")
            .unwrap();
        let sid = db.create_wire_session("u").unwrap();

        family
            .execute(&db, sid, "INSERT INTO t_e1 (name) VALUES ('a')")
            .unwrap();
        let first = family.scalar_i64(&db, sid, "SELECT lastval()");
        let stored = family.scalar_i64(&db, sid, "SELECT id FROM t_e1 WHERE name = 'a'");
        assert_eq!(
            first, stored,
            "{family:?}: lastval() != the id the INSERT actually stored"
        );

        family
            .execute(&db, sid, "INSERT INTO t_e1 (name) VALUES ('b')")
            .unwrap();
        let second = family.scalar_i64(&db, sid, "SELECT lastval()");
        let stored_b = family.scalar_i64(&db, sid, "SELECT id FROM t_e1 WHERE name = 'b'");
        assert_eq!(second, stored_b, "{family:?}: lastval() stale after the second INSERT");
        assert_ne!(first, second, "{family:?}: two inserts produced the same lastval");
        db.destroy_session(sid).unwrap();
    }
}

/// PIN (E1): `GENERATED ... AS IDENTITY` is the SQL-standard spelling of
/// SERIAL and must behave identically.
#[test]
fn lastval_after_an_identity_insert_returns_that_rows_id() {
    let db = db();
    db.execute("CREATE TABLE t_ident (id INTEGER GENERATED ALWAYS AS IDENTITY PRIMARY KEY, name TEXT)")
        .unwrap();
    let sid = db.create_wire_session("u").unwrap();
    db.execute_for_session(sid, "INSERT INTO t_ident (name) VALUES ('x')")
        .unwrap();
    let last = Family::Text.scalar_i64(&db, sid, "SELECT lastval()");
    let stored = Family::Text.scalar_i64(&db, sid, "SELECT id FROM t_ident WHERE name = 'x'");
    assert_eq!(last, stored, "IDENTITY insert did not define lastval");
    db.destroy_session(sid).unwrap();
}

/// PIN (E1): **the load-bearing test.** Two concurrent sessions must never see
/// each other's value. A process-global "last nextval" passes every test above
/// and fails this one — and would be strictly worse than the absence it
/// replaces, because a driver would report another connection's row id as its
/// own `lastrowid`.
#[test]
fn two_sessions_never_see_each_others_lastval() {
    for family in FAMILIES {
        let db = db();
        db.execute("CREATE TABLE t_iso (id SERIAL PRIMARY KEY, v TEXT)")
            .unwrap();
        let a = db.create_wire_session("a").unwrap();
        let b = db.create_wire_session("b").unwrap();

        // A inserts. B has still never run a nextval → B must ERROR.
        family
            .execute(&db, a, "INSERT INTO t_iso (v) VALUES ('from-a')")
            .unwrap();
        let a_last = family.scalar_i64(&db, a, "SELECT lastval()");
        let err = family
            .query(&db, b, "SELECT lastval()")
            .expect_err(&format!("{family:?}: session B leaked session A's lastval"));
        assert!(
            err.to_string().contains("lastval is not yet defined in this session"),
            "{family:?}: B got {err}"
        );

        // Now B inserts. Each session keeps ITS OWN value.
        family
            .execute(&db, b, "INSERT INTO t_iso (v) VALUES ('from-b')")
            .unwrap();
        let b_last = family.scalar_i64(&db, b, "SELECT lastval()");
        assert_ne!(a_last, b_last, "{family:?}: the two sessions share one lastval slot");
        assert_eq!(
            family.scalar_i64(&db, a, "SELECT lastval()"),
            a_last,
            "{family:?}: B's insert overwrote A's lastval"
        );
        db.destroy_session(a).unwrap();
        db.destroy_session(b).unwrap();
    }
}

/// PIN (E1): PostgreSQL does NOT roll a sequence advance back, so `LASTVAL()`
/// still reports it after the transaction that produced it aborted.
#[test]
fn lastval_survives_a_rolled_back_transaction() {
    for family in FAMILIES {
        let db = db();
        db.execute("CREATE SEQUENCE s_rb").unwrap();
        let sid = db.create_wire_session("u").unwrap();

        family.execute(&db, sid, "BEGIN").unwrap();
        let inside = family.scalar_i64(&db, sid, "SELECT nextval('s_rb')");
        family.execute(&db, sid, "ROLLBACK").unwrap();

        assert_eq!(
            family.scalar_i64(&db, sid, "SELECT lastval()"),
            inside,
            "{family:?}: a sequence advance must not be rolled back (PostgreSQL semantics)"
        );
        db.destroy_session(sid).unwrap();
    }
}

/// PIN (E1): the session-less embedded funnels (`query()` / `execute()` and
/// `query_params()` / `execute_params()`) are ONE backend — the handle — so
/// both families must answer the same value, and it must be the id the INSERT
/// actually stored.
///
/// Deliberately does NOT assert that a brand-new handle refuses `lastval()`:
/// the constructor is free to seed engine-owned tables, and a test that breaks
/// when it does would be pinning the bootstrap, not the feature. The
/// fresh-backend refusal is pinned where it is unambiguous — on a wire session
/// (`lastval_on_a_fresh_session_errors_55000`).
#[test]
fn lastval_agrees_across_the_session_less_embedded_funnels() {
    let db = db();
    db.execute("CREATE TABLE t_emb (id SERIAL PRIMARY KEY, v TEXT)")
        .unwrap();
    db.execute("INSERT INTO t_emb (v) VALUES ('x')").unwrap();

    let read = |sql: &str, params_family: bool| -> i64 {
        let rows = if params_family {
            db.query_params(sql, &[]).unwrap()
        } else {
            db.query(sql, &[]).unwrap()
        };
        match rows[0].values[0] {
            Value::Int8(v) => v,
            Value::Int4(v) => i64::from(v),
            ref other => panic!("`{sql}` returned {other:?}"),
        }
    };

    let text_family = read("SELECT lastval()", false);
    let params_family = read("SELECT lastval()", true);
    let stored = read("SELECT id FROM t_emb WHERE v = 'x'", false);
    assert_eq!(
        text_family, stored,
        "embedded text family: lastval() != the inserted id"
    );
    assert_eq!(
        params_family, stored,
        "embedded params family: lastval() != the inserted id"
    );
}

// ===========================================================================
// E2 — sprinter f4f5d450e816 — pg_backend_pid()
// ===========================================================================

/// PIN (E2): stable for the life of ONE session, and unique among live
/// sessions — the two properties a pool's affinity/health check relies on.
#[test]
fn pg_backend_pid_is_stable_per_session_and_unique_across_sessions() {
    for family in FAMILIES {
        let db = db();
        let a = db.create_wire_session("a").unwrap();
        let b = db.create_wire_session("b").unwrap();

        let a1 = family.scalar_i64(&db, a, "SELECT pg_backend_pid()");
        let a2 = family.scalar_i64(&db, a, "SELECT pg_backend_pid()");
        let b1 = family.scalar_i64(&db, b, "SELECT pg_backend_pid()");

        assert_eq!(a1, a2, "{family:?}: pg_backend_pid() changed mid-session");
        assert_ne!(a1, b1, "{family:?}: two live sessions share one backend pid");
        assert!(a1 != 0 && b1 != 0, "{family:?}: a backend pid of 0 is not a pid");
        db.destroy_session(a).unwrap();
        db.destroy_session(b).unwrap();
    }
}

/// PIN (E2): `pg_stat_activity` must report the SAME value, so a client can
/// join on it. A catalog that disagrees with the function is worse than none.
#[test]
fn pg_backend_pid_agrees_with_pg_stat_activity() {
    for family in FAMILIES {
        let db = db();
        // `create_session` (not `create_wire_session`) so the session carries a
        // published login identity: `create_wire_session` deliberately leaves it
        // unset until a wire handler authenticates (HDB-009), and this test is
        // about `usename` agreeing with that identity.
        let a = db.create_session("alice", IsolationLevel::ReadCommitted).unwrap();
        let b = db.create_session("bob", IsolationLevel::ReadCommitted).unwrap();

        let a_pid = family.scalar_i64(&db, a, "SELECT pg_backend_pid()");
        let b_pid = family.scalar_i64(&db, b, "SELECT pg_backend_pid()");

        // The self-join every pool and every test suite writes.
        let seen = family.scalar_i64(
            &db,
            a,
            "SELECT count(*) FROM pg_stat_activity WHERE pid = pg_backend_pid()",
        );
        assert_eq!(seen, 1, "{family:?}: the scanning backend is not in pg_stat_activity");

        let rows = family
            .query(&db, a, "SELECT pid, usename FROM pg_stat_activity ORDER BY pid")
            .unwrap();
        let pids: Vec<i64> = rows
            .iter()
            .map(|t| match &t.values[0] {
                Value::Int4(v) => i64::from(*v),
                Value::Int8(v) => *v,
                other => panic!("pg_stat_activity.pid is {other:?}"),
            })
            .collect();
        assert!(
            pids.contains(&a_pid),
            "{family:?}: {a_pid} missing from pg_stat_activity"
        );
        assert!(
            pids.contains(&b_pid),
            "{family:?}: {b_pid} missing from pg_stat_activity"
        );

        // usename must be this session's login identity, on the same row.
        let usename = family.scalar_text(
            &db,
            a,
            "SELECT usename FROM pg_stat_activity WHERE pid = pg_backend_pid()",
        );
        assert_eq!(usename, "alice", "{family:?}: pg_stat_activity names the wrong user");

        db.destroy_session(a).unwrap();
        db.destroy_session(b).unwrap();

        // A destroyed session must LEAVE pg_stat_activity — a stale row is a
        // pid a client would try to join against forever.
        let rows = db.query("SELECT pid FROM pg_stat_activity", &[]).unwrap();
        let live: Vec<i64> = rows
            .iter()
            .map(|t| match &t.values[0] {
                Value::Int4(v) => i64::from(*v),
                Value::Int8(v) => *v,
                other => panic!("pg_stat_activity.pid is {other:?}"),
            })
            .collect();
        assert!(!live.contains(&a_pid), "{family:?}: destroyed session still listed");
        assert!(!live.contains(&b_pid), "{family:?}: destroyed session still listed");
    }
}

// ===========================================================================
// E2 — sprinter f4f5d450e816 — application_name GUC
// ===========================================================================

/// PIN (E2): `SET` / `SHOW` / `current_setting()` / `RESET`, both families.
/// Default is the empty string (PostgreSQL's), not an error.
#[test]
fn application_name_set_show_reset_on_both_families() {
    for family in FAMILIES {
        let db = db();
        let sid = db.create_wire_session("u").unwrap();

        assert_eq!(
            family.scalar_text(&db, sid, "SHOW application_name"),
            "",
            "{family:?}: default application_name must be the empty string"
        );
        assert_eq!(
            family.scalar_text(&db, sid, "SELECT current_setting('application_name')"),
            "",
            "{family:?}: current_setting disagrees with SHOW at the default"
        );

        family
            .execute(&db, sid, "SET application_name = 'reporting-worker'")
            .unwrap();
        assert_eq!(
            family.scalar_text(&db, sid, "SHOW application_name"),
            "reporting-worker",
            "{family:?}: SET application_name did not take"
        );
        assert_eq!(
            family.scalar_text(&db, sid, "SELECT current_setting('application_name')"),
            "reporting-worker",
            "{family:?}: current_setting disagrees with SHOW"
        );

        // PIN: the value SURVIVES across statements on one session.
        family.execute(&db, sid, "SELECT 1").ok();
        assert_eq!(
            family.scalar_text(&db, sid, "SHOW application_name"),
            "reporting-worker",
            "{family:?}: application_name did not survive the next statement"
        );

        family.execute(&db, sid, "RESET application_name").unwrap();
        assert_eq!(
            family.scalar_text(&db, sid, "SHOW application_name"),
            "",
            "{family:?}: RESET application_name did not restore the default"
        );

        // `SET ... TO DEFAULT` is PostgreSQL's synonym for RESET.
        family.execute(&db, sid, "SET application_name = 'again'").unwrap();
        family.execute(&db, sid, "SET application_name TO DEFAULT").unwrap();
        assert_eq!(
            family.scalar_text(&db, sid, "SHOW application_name"),
            "",
            "{family:?}: SET ... TO DEFAULT did not reset"
        );
        db.destroy_session(sid).unwrap();
    }
}

/// PIN (E2): two concurrent sessions see their own value, never each other's.
/// The `SessionSettings` registry is process-global, so an implementation that
/// simply registers `application_name` there fails exactly here.
#[test]
fn application_name_is_per_session_never_shared() {
    for family in FAMILIES {
        let db = db();
        let a = db.create_wire_session("a").unwrap();
        let b = db.create_wire_session("b").unwrap();

        family.execute(&db, a, "SET application_name = 'app-a'").unwrap();
        assert_eq!(
            family.scalar_text(&db, b, "SHOW application_name"),
            "",
            "{family:?}: session A's application_name leaked into session B"
        );

        family.execute(&db, b, "SET application_name = 'app-b'").unwrap();
        assert_eq!(
            family.scalar_text(&db, a, "SHOW application_name"),
            "app-a",
            "{family:?}"
        );
        assert_eq!(
            family.scalar_text(&db, b, "SHOW application_name"),
            "app-b",
            "{family:?}"
        );
        db.destroy_session(a).unwrap();
        db.destroy_session(b).unwrap();
    }
}

/// PIN (E2): `SET LOCAL` reverts at transaction end — on COMMIT and on
/// ROLLBACK alike.
#[test]
fn set_local_application_name_reverts_at_transaction_end() {
    for family in FAMILIES {
        for finish in ["COMMIT", "ROLLBACK"] {
            let db = db();
            let sid = db.create_wire_session("u").unwrap();
            family.execute(&db, sid, "SET application_name = 'outer'").unwrap();

            family.execute(&db, sid, "BEGIN").unwrap();
            family
                .execute(&db, sid, "SET LOCAL application_name = 'inner'")
                .unwrap();
            assert_eq!(
                family.scalar_text(&db, sid, "SHOW application_name"),
                "inner",
                "{family:?}/{finish}: SET LOCAL had no effect inside the block"
            );
            family.execute(&db, sid, finish).unwrap();

            assert_eq!(
                family.scalar_text(&db, sid, "SHOW application_name"),
                "outer",
                "{family:?}/{finish}: SET LOCAL outlived its transaction"
            );
            db.destroy_session(sid).unwrap();
        }
    }
}

/// PIN (E2): `application_name` is surfaced in `pg_stat_activity`, on the row
/// whose `pid` is this backend's.
#[test]
fn application_name_is_visible_in_pg_stat_activity() {
    let db = db();
    let sid = db.create_wire_session("u").unwrap();
    db.execute_for_session(sid, "SET application_name = 'grafana'").unwrap();
    let seen = Family::Text.scalar_text(
        &db,
        sid,
        "SELECT application_name FROM pg_stat_activity WHERE pid = pg_backend_pid()",
    );
    assert_eq!(seen, "grafana", "pg_stat_activity does not report application_name");
    db.destroy_session(sid).unwrap();
}

/// PIN (both): unknown-setting behaviour is UNCHANGED. A GUC this server does
/// not know still refuses with PostgreSQL's wording (42704 on the wire); it
/// must not have become an empty string now that a new name is registered.
#[test]
fn unknown_setting_behaviour_is_unchanged() {
    for family in FAMILIES {
        let db = db();
        let sid = db.create_wire_session("u").unwrap();
        let err = family
            .query(&db, sid, "SELECT current_setting('helios.no_such_guc')")
            .expect_err(&format!("{family:?}: an unknown GUC must still refuse"));
        assert!(
            err.to_string()
                .to_lowercase()
                .contains("unrecognized configuration parameter"),
            "{family:?}: wrong refusal for an unknown GUC: {err}"
        );
        // The two-argument `missing_ok` form still answers the empty string.
        assert_eq!(
            family.scalar_text(&db, sid, "SELECT current_setting('helios.no_such_guc', true)"),
            "",
            "{family:?}: current_setting(name, true) must stay lenient"
        );
        db.destroy_session(sid).unwrap();
    }
}

// ===========================================================================
// PostgreSQL wire
// ===========================================================================

async fn pg_setup() -> (String, tokio::task::JoinHandle<()>) {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
    let addr = probe.local_addr().expect("local_addr");
    drop(probe);

    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    db.execute("CREATE TABLE t_wire (id SERIAL PRIMARY KEY, v TEXT)")
        .unwrap();
    let config = PgServerConfig::with_address(addr).with_max_connections(16);
    let server = PgServer::new(config, db).expect("server");
    let handle = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    (
        format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port()),
        handle,
    )
}

async fn pg_connect(cs: &str) -> (tokio_postgres::Client, tokio::task::JoinHandle<()>) {
    let (client, conn) = tokio_postgres::connect(cs, NoTls).await.expect("connect");
    let task = tokio::spawn(async move {
        let _ = conn.await;
    });
    (client, task)
}

/// PIN (E1 + E2, PostgreSQL wire): `LASTVAL()` after a SERIAL insert, and
/// `pg_backend_pid()` stable across statements and distinct between two live
/// connections — over the real wire, through the extended protocol
/// (`client.query` binds parameters, so this is the params family end to end).
#[tokio::test]
async fn lastval_and_backend_pid_on_the_postgres_wire() {
    let (cs, server) = pg_setup().await;
    let (a, a_task) = pg_connect(&cs).await;
    let (b, b_task) = pg_connect(&cs).await;

    // A fresh connection has no lastval.
    assert!(
        a.query_one("SELECT lastval()", &[]).await.is_err(),
        "a fresh wire session must refuse lastval()"
    );

    a.execute("INSERT INTO t_wire (v) VALUES ('a')", &[]).await.unwrap();
    let last: i64 = a.query_one("SELECT lastval()", &[]).await.unwrap().get(0);
    let stored: i32 = a
        .query_one("SELECT id FROM t_wire WHERE v = 'a'", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(last, i64::from(stored), "wire lastval() != the inserted id");

    // B must not see A's value.
    assert!(
        b.query_one("SELECT lastval()", &[]).await.is_err(),
        "connection B leaked connection A's lastval over the wire"
    );

    let a_pid: i32 = a.query_one("SELECT pg_backend_pid()", &[]).await.unwrap().get(0);
    let a_pid2: i32 = a.query_one("SELECT pg_backend_pid()", &[]).await.unwrap().get(0);
    let b_pid: i32 = b.query_one("SELECT pg_backend_pid()", &[]).await.unwrap().get(0);
    assert_eq!(a_pid, a_pid2, "pg_backend_pid() changed mid-connection");
    assert_ne!(a_pid, b_pid, "two wire connections share one backend pid");

    let joined: i64 = a
        .query_one(
            "SELECT count(*) FROM pg_stat_activity WHERE pid = pg_backend_pid()",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(joined, 1, "wire session missing from pg_stat_activity");

    drop(a);
    drop(b);
    a_task.abort();
    b_task.abort();
    server.abort();
}

/// PIN (E2, PostgreSQL wire): the startup-packet `application_name` — which is
/// where EVERY driver actually sends it — is visible WITHOUT an explicit `SET`,
/// and a later `SET` on one connection does not disturb the other.
#[tokio::test]
async fn application_name_from_the_startup_packet_is_visible_without_set() {
    let (cs, server) = pg_setup().await;
    let (a, a_task) = pg_connect(&format!("{cs} application_name=my-reporter")).await;
    let (b, b_task) = pg_connect(&cs).await;

    let seen: String = a
        .query_one("SELECT current_setting('application_name')", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(seen, "my-reporter", "startup-packet application_name was dropped");

    let shown = a.simple_query("SHOW application_name").await.unwrap();
    let shown = shown
        .iter()
        .find_map(|m| match m {
            tokio_postgres::SimpleQueryMessage::Row(r) => r.get(0).map(str::to_string),
            _ => None,
        })
        .expect("SHOW application_name returned no row");
    assert_eq!(shown, "my-reporter", "SHOW disagrees with current_setting");

    let other: String = b
        .query_one("SELECT current_setting('application_name')", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(other, "", "a connection that sent no application_name inherited one");

    // A `SET` on B must stay on B.
    b.simple_query("SET application_name = 'b-only'").await.unwrap();
    let still: String = a
        .query_one("SELECT current_setting('application_name')", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(still, "my-reporter", "connection B's SET leaked into connection A");

    // And it is joinable in pg_stat_activity.
    let via_catalog: String = b
        .query_one(
            "SELECT application_name FROM pg_stat_activity WHERE pid = pg_backend_pid()",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(via_catalog, "b-only", "pg_stat_activity disagrees over the wire");

    drop(a);
    drop(b);
    a_task.abort();
    b_task.abort();
    server.abort();
}

// ===========================================================================
// MySQL wire
// ===========================================================================

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
            let v = u64::from(buf[*pos]) | (u64::from(buf[*pos + 1]) << 8) | (u64::from(buf[*pos + 2]) << 16);
            *pos += 3;
            v
        }
        0xFE => {
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[*pos..*pos + 8]);
            *pos += 8;
            u64::from_le_bytes(b)
        }
        v => u64::from(v),
    }
}

fn read_lenenc_str(buf: &[u8], pos: &mut usize) -> String {
    let len = read_lenenc_u64(buf, pos) as usize;
    let s = String::from_utf8_lossy(&buf[*pos..*pos + len]).into_owned();
    *pos += len;
    s
}

struct MySqlTestClient {
    stream: DuplexStream,
}

impl MySqlTestClient {
    async fn login(db: Arc<EmbeddedDatabase>, id: u32) -> Self {
        let (client, srv) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move {
            let _ = MySqlHandler::handle_connection_with_timeouts(db, srv, id, ConnectionTimeouts::disabled()).await;
        });
        let mut this = Self { stream: client };
        let (_seq, _greeting) = this.read_packet().await;
        let mut p = Vec::new();
        p.extend_from_slice(&(CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION).to_le_bytes());
        p.extend_from_slice(&(1u32 << 24).to_le_bytes());
        p.push(45);
        p.extend_from_slice(&[0u8; 23]);
        p.extend_from_slice(b"root");
        p.push(0);
        p.push(0);
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
        let mut pos = 0usize;
        let ncols = read_lenenc_u64(&first, &mut pos) as usize;
        for _ in 0..ncols {
            let _ = self.read_packet().await;
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

/// PIN (E1 + E2, MySQL wire). The MySQL listener translates to PostgreSQL SQL
/// and then runs the statement on ITS OWN engine session, so every property
/// must hold there too — including the per-connection isolation.
#[tokio::test]
async fn lastval_backend_pid_and_application_name_on_the_mysql_wire() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    db.execute("CREATE TABLE t_my (id SERIAL PRIMARY KEY, v TEXT)").unwrap();

    let mut a = MySqlTestClient::login(Arc::clone(&db), 1).await;
    let mut b = MySqlTestClient::login(Arc::clone(&db), 2).await;

    // E1 — lastval after a SERIAL insert.
    a.ok("INSERT INTO t_my (v) VALUES ('a')").await;
    let last = a.scalar("SELECT lastval()").await;
    let stored = a.scalar("SELECT id FROM t_my WHERE v = 'a'").await;
    assert_eq!(last, stored, "MySQL wire: lastval() != the inserted id");

    // …and B, which has inserted nothing, must not see it.
    let b_err = b.send("SELECT lastval()").await;
    assert_eq!(
        b_err.first().copied(),
        Some(0xFF),
        "MySQL connection B leaked connection A's lastval"
    );

    // E2 — pg_backend_pid.
    let a_pid = a.scalar("SELECT pg_backend_pid()").await;
    let a_pid2 = a.scalar("SELECT pg_backend_pid()").await;
    let b_pid = b.scalar("SELECT pg_backend_pid()").await;
    assert_eq!(a_pid, a_pid2, "MySQL wire: pg_backend_pid() changed mid-connection");
    assert_ne!(a_pid, b_pid, "MySQL wire: two connections share one backend pid");

    // E2 — application_name. The MySQL COM_QUERY path acknowledges every `SET`
    // silently, so this pins that `SET application_name` is NOT swallowed.
    a.ok("SET application_name = 'php-app'").await;
    assert_eq!(
        a.scalar("SELECT current_setting('application_name')").await,
        "php-app",
        "MySQL wire: SET application_name was acknowledged but dropped"
    );
    assert_eq!(
        b.scalar("SELECT current_setting('application_name')").await,
        "",
        "MySQL wire: connection A's application_name leaked into connection B"
    );
}
