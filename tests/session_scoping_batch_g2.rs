//! Batch G2 — state that must be PER-SESSION and was process-global.
//!
//! Three sprinter items, one defect class: a value one connection sets, or a
//! limit one connection is measured against, was kept in a single slot the whole
//! process shared.
//!
//! * **sprinter a3077a3f68d8 (HIGH)** — `SET` wrote the process-global settings
//!   registry. `SET statement_timeout = 1` from any client cancelled EVERY
//!   other connection's queries after 1 ms, and `SET bulk_load_mode = on`
//!   flipped the storage engine's flag for everyone. PostgreSQL GUCs are
//!   per-session; on a shared or multi-tenant server the old behaviour was a
//!   denial-of-service lever handed to any authenticated client.
//!
//! * **sprinter 7903b7111cb4 (HIGH)** — `currval('s')` read the process-wide
//!   sequence runtime, so connection A's `nextval` answered connection B's
//!   `currval` (a client recovering the id it just inserted could be handed
//!   ANOTHER TENANT's id, silently), and a sequence this session had never
//!   advanced answered `0` instead of raising. `LASTVAL()` shipped correctly
//!   session-scoped in v4.38.0, so the two functions disagreed about what "this
//!   session" means.
//!
//! * **sprinter d03de7fc3b22 (HIGH) — SHIPPED; the two tripwires below are
//!   FLIPPED.** `TenantManager::record_query` had exactly ONE production
//!   caller, on the simple-query / embedded `execute()` funnel. Statements that
//!   bind parameters — psycopg3, JDBC, sqlx, node-postgres, Prisma, Drizzle,
//!   i.e. most real traffic — were never counted, and neither was any READ, so
//!   `max_qps` was unenforced for them while the operator believed a limit was
//!   in force. The call sites that close that gap were written and then backed
//!   out of v4.39.0, because `TenantManager::current_context` was one
//!   process-global slot with no `SessionId -> TenantId` binding behind it:
//!   metering every family against it charged a connection's statements to
//!   whichever tenant another connection last selected — enforcement that is
//!   WRONG rather than merely absent. The binding now exists
//!   (`SessionScopedState::bind_tenant`, populated from the database name at
//!   startup — see `tests/tenant_session_binding_h1.rs`), so the charge sites
//!   are back and the `max_qps_is_not_*` cases have become
//!   `max_qps_is_enforced_*`. The two cases here exercise the resolver's
//!   FALLBACK layer (a session-less embedded caller, and a wire connection to
//!   the reserved `postgres` database); the per-connection layer is proved in
//!   the H1 file.
//!
//! # What these tests pin that a "global" implementation would fail
//!
//! The cross-session tests are the load-bearing ones, not the happy paths:
//! `*_never_crosses_sessions` and `two_sessions_never_see_each_others_currval`
//! fail on the pre-fix tree, and everything else is the surface needed to make
//! the per-session value reachable on both executor families and both wires.
//! The tenant cases invert that: they pin an ABSENCE, and their failure messages
//! say so, because a green tripwire there means somebody re-enabled metering
//! without the binding it depends on.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;
use std::time::Duration;

use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::session::SessionId;
use heliosdb_nano::tenant::{IsolationMode, ResourceLimits, TenantContext, TenantId, TenantManager};
use heliosdb_nano::{EmbeddedDatabase, Value};
use tokio_postgres::NoTls;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().unwrap()
}

/// Which executor family a shared assertion runs on — the same split
/// `tests/session_surface_batch_e.rs` uses, and for the same reason: GH#28's
/// lesson was a feature that silently worked on the text family only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    /// `query_with_columns_for_session` / `execute_for_session` — literal text.
    Text,
    /// `query_params_for_session` / `execute_params_for_session` — the
    /// bound-parameter planner the PostgreSQL extended protocol reaches.
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

/// Enough rows that a self-join cannot possibly finish inside a 1 ms budget on
/// any host, so "B was not cancelled" is a real observation and not a race the
/// runner happened to win.
fn seed_wide(db: &EmbeddedDatabase, rows: i64) {
    db.execute("CREATE TABLE g2_big (id INT PRIMARY KEY, v INT)").unwrap();
    for chunk in (1..=rows).collect::<Vec<_>>().chunks(1000) {
        let vals: String = chunk
            .iter()
            .map(|i| format!("({i},{})", i % 400))
            .collect::<Vec<_>>()
            .join(",");
        db.execute(&format!("INSERT INTO g2_big VALUES {vals}")).unwrap();
    }
}

const HEAVY: &str = "SELECT count(*) FROM g2_big a JOIN g2_big b ON a.v = b.v WHERE a.id > 0";

// ===========================================================================
// a3077a3f68d8 — SET is per-session, not a process-global registry write
// ===========================================================================

/// **THE PROOF (statement_timeout).** Connection A sets a 1 ms
/// `statement_timeout`; connection B's heavy query must still run to completion.
///
/// On the pre-fix tree the `SET` landed in the ONE
/// `sql::settings::SessionSettings` registry that
/// `EmbeddedDatabase::effective_statement_timeout_ms` consults for EVERY
/// executor it builds, so B is cancelled with 57014 after 1 ms and this test
/// fails at the `b_result` assertion. A's OWN query still being cancelled is
/// what keeps the test non-vacuous: without it, simply ignoring the `SET`
/// would pass.
#[test]
fn set_statement_timeout_never_crosses_sessions() {
    for family in FAMILIES {
        let db = db();
        seed_wide(&db, 8_000);
        let a = db.create_wire_session("a").unwrap();
        let b = db.create_wire_session("b").unwrap();

        family.execute(&db, a, "SET statement_timeout = 1").unwrap();

        // B is untouched.
        let started = std::time::Instant::now();
        let b_result = family.query(&db, b, HEAVY);
        let b_elapsed = started.elapsed();
        assert!(
            b_result.is_ok(),
            "{family:?}: *** connection A's SET statement_timeout cancelled connection B *** \
             (a cross-session denial-of-service lever): {:?}",
            b_result.err()
        );

        // A really is limited — so the isolation above is scope, not a no-op.
        let a_started = std::time::Instant::now();
        let a_result = family.query(&db, a, HEAVY);
        let a_elapsed = a_started.elapsed();
        match a_result {
            Err(e) => {
                let msg = e.to_string().to_lowercase();
                assert!(
                    msg.contains("timeout") || msg.contains("timed out") || msg.contains("cancel"),
                    "{family:?}: A must be cancelled by ITS OWN 1ms budget, got: {e}"
                );
            }
            Ok(_) => assert!(
                a_elapsed < Duration::from_millis(5),
                "{family:?}: A ran {a_elapsed:?} with a 1ms statement_timeout — not enforced \
                 (B's identical query took {b_elapsed:?})"
            ),
        }

        db.destroy_session(a).unwrap();
        db.destroy_session(b).unwrap();
    }
}

/// **THE PROOF (bulk_load_mode).** Same shape for the storage-engine flag: A's
/// `SET bulk_load_mode = on` must not reach the process-wide switch that turns
/// off MV-delta tracking, SMFI tracking and compression metrics for every
/// connection in the process.
#[test]
fn set_bulk_load_mode_never_crosses_sessions() {
    for family in FAMILIES {
        let db = db();
        let a = db.create_wire_session("a").unwrap();
        let b = db.create_wire_session("b").unwrap();

        family.execute(&db, a, "SET bulk_load_mode = on").unwrap();

        assert_eq!(
            family.scalar_text(&db, a, "SHOW bulk_load_mode"),
            "on",
            "{family:?}: A's own SET did not take effect"
        );
        assert_eq!(
            family.scalar_text(&db, b, "SHOW bulk_load_mode"),
            "off",
            "{family:?}: *** connection A's SET bulk_load_mode reached connection B ***"
        );
        assert!(
            !db.storage.is_bulk_load_mode(),
            "{family:?}: the process-wide storage-engine flag was flipped by a session SET"
        );
        assert!(
            !db.bulk_load_mode(),
            "{family:?}: a wire session's SET reached the session-less embedded backend"
        );

        // RESET drops the override; it must not write the server flag either.
        family.execute(&db, a, "RESET bulk_load_mode").unwrap();
        assert_eq!(family.scalar_text(&db, a, "SHOW bulk_load_mode"), "off");
        assert!(!db.storage.is_bulk_load_mode());

        db.destroy_session(a).unwrap();
        db.destroy_session(b).unwrap();
    }
}

/// The value is READABLE back through all three surfaces a client uses, on both
/// families: `SHOW`, `current_setting()` and (for the timeout) the executor.
/// PostgreSQL's duration rendering, so `SET statement_timeout = 30000` reads
/// back as `30s` and the default as `0` — not this engine's internal `0ms`.
#[test]
fn a_session_reads_back_its_own_guc_on_both_families() {
    for family in FAMILIES {
        let db = db();
        let sid = db.create_wire_session("u").unwrap();

        assert_eq!(family.scalar_text(&db, sid, "SHOW statement_timeout"), "0");
        assert_eq!(
            family.scalar_text(&db, sid, "SELECT current_setting('statement_timeout')"),
            "0"
        );

        family.execute(&db, sid, "SET statement_timeout = 30000").unwrap();
        assert_eq!(
            family.scalar_text(&db, sid, "SHOW statement_timeout"),
            "30s",
            "{family:?}"
        );
        assert_eq!(
            family.scalar_text(&db, sid, "SELECT current_setting('statement_timeout')"),
            "30s",
            "{family:?}"
        );

        // PostgreSQL GUC duration syntax, and a value that is not one fails
        // closed rather than being stored as a silent lie.
        family.execute(&db, sid, "SET statement_timeout = '2min'").unwrap();
        assert_eq!(family.scalar_text(&db, sid, "SHOW statement_timeout"), "2min");
        let err = family
            .execute(&db, sid, "SET statement_timeout = 'banana'")
            .expect_err(&format!("{family:?}: a non-duration must be refused"));
        assert!(
            err.to_string().contains("invalid value for parameter"),
            "{family:?}: wrong refusal: {err}"
        );

        // `RESET` and `SET … TO DEFAULT` both drop the override.
        family.execute(&db, sid, "RESET statement_timeout").unwrap();
        assert_eq!(family.scalar_text(&db, sid, "SHOW statement_timeout"), "0");
        family.execute(&db, sid, "SET statement_timeout = 5000").unwrap();
        family.execute(&db, sid, "SET statement_timeout TO DEFAULT").unwrap();
        assert_eq!(family.scalar_text(&db, sid, "SHOW statement_timeout"), "0");

        db.destroy_session(sid).unwrap();
    }
}

/// The parameter split IS the deliverable, so it is pinned: a SERVER-level
/// parameter never becomes per-session state, and the postmaster-scoped subset
/// is still refused outright.
#[test]
fn server_level_parameters_are_not_session_state() {
    // Names classified as server-level are exactly the ones whose value is
    // consumed by process-wide machinery on behalf of OTHER sessions.
    for name in [
        "shared_buffers",
        "default_compression",
        "compression_level",
        "time_travel_enabled",
        "mv_auto_refresh",
        "smfi_enabled",
        "smfi_max_workers",
        "server_version",
        "max_connections",
        "authentication_timeout",
    ] {
        assert!(heliosdb_nano::sql::is_server_level(name), "{name} must be server-level");
        assert!(
            !heliosdb_nano::sql::is_user_settable(name),
            "{name} must not be settable"
        );
    }
    for name in [
        "statement_timeout",
        "query_timeout",
        "work_mem",
        "bulk_load_mode",
        "optimizer",
        "enable_seqscan",
        "enable_hashjoin",
        "vector_index_type",
        "hnsw_m",
        "client_encoding",
        "timezone",
    ] {
        assert!(
            heliosdb_nano::sql::is_user_settable(name),
            "{name} must be user-settable"
        );
        assert!(
            !heliosdb_nano::sql::is_server_level(name),
            "{name} must not be server-level"
        );
    }
    // An unknown name is neither, so it keeps falling through to the planner /
    // PostgreSQL's 42704.
    assert!(!heliosdb_nano::sql::is_user_settable("helios_no_such_guc"));

    let db = db();
    let sid = db.create_wire_session("u").unwrap();
    // A server-level parameter still goes to the registry, and the
    // postmaster-scoped subset is still refused with PostgreSQL's wording.
    db.execute_for_session(sid, "SET default_compression = 'lz4'").unwrap();
    let err = db
        .execute_for_session(sid, "SET authentication_timeout = 0")
        .expect_err("a postmaster-scoped parameter must be refused");
    assert!(err.to_string().contains("cannot be changed now"), "{err}");
    db.destroy_session(sid).unwrap();
}

/// `SET LOCAL` is scoped to the transaction block, on both COMMIT and ROLLBACK,
/// and reverts to the value the block STARTED with (not to an intervening
/// `SET LOCAL`) — PostgreSQL semantics.
#[test]
fn set_local_reverts_at_the_end_of_the_block() {
    for ending in ["COMMIT", "ROLLBACK"] {
        let db = db();
        let sid = db.create_wire_session("u").unwrap();
        db.execute_for_session(sid, "SET statement_timeout = 5000").unwrap();

        db.execute_for_session(sid, "BEGIN").unwrap();
        db.execute_for_session(sid, "SET LOCAL statement_timeout = 250")
            .unwrap();
        db.execute_for_session(sid, "SET LOCAL statement_timeout = 750")
            .unwrap();
        assert_eq!(
            db.query_with_columns_for_session(sid, "SHOW statement_timeout")
                .unwrap()
                .0[0]
                .values[0],
            Value::String("750ms".to_string())
        );
        db.execute_for_session(sid, ending).unwrap();

        assert_eq!(
            db.query_with_columns_for_session(sid, "SHOW statement_timeout")
                .unwrap()
                .0[0]
                .values[0],
            Value::String("5s".to_string()),
            "{ending}: SET LOCAL must revert to the value the block started with"
        );
        db.destroy_session(sid).unwrap();
    }
}

/// PostgreSQL wire, both protocols: `SET statement_timeout` from connection A
/// takes effect on A and is invisible to B, and `SHOW` answers per connection.
///
/// `client.execute`/`client.query` bind parameters, so this is the EXTENDED
/// protocol end to end — the path psycopg3 / JDBC / sqlx / node-postgres take,
/// and the one GH#28 showed a session feature can silently miss.
#[tokio::test]
async fn set_is_per_connection_on_the_postgres_wire() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let server = PgServer::new(
        PgServerConfig::with_address(addr).with_max_connections(16),
        Arc::clone(&db),
    )
    .unwrap();
    let handle = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let cs = format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port());

    let (a, a_task) = {
        let (c, conn) = tokio_postgres::connect(&cs, NoTls).await.unwrap();
        (c, tokio::spawn(async move { conn.await.map(|_| ()).unwrap_or(()) }))
    };
    let (b, b_task) = {
        let (c, conn) = tokio_postgres::connect(&cs, NoTls).await.unwrap();
        (c, tokio::spawn(async move { conn.await.map(|_| ()).unwrap_or(()) }))
    };

    // Simple protocol (`simple_query`) and extended protocol (`execute`) both
    // land on the session, and both are visible to `SHOW` on this connection.
    a.simple_query("SET statement_timeout = 7000").await.unwrap();
    let v: String = a.query_one("SHOW statement_timeout", &[]).await.unwrap().get(0);
    assert_eq!(v, "7s", "simple-protocol SET was not applied to this connection");

    a.execute("SET work_mem = 16384", &[]).await.unwrap();
    let v: String = a.query_one("SHOW work_mem", &[]).await.unwrap().get(0);
    assert_eq!(v, "16384", "extended-protocol SET was not applied to this connection");

    // Connection B still sees the server defaults.
    let v: String = b.query_one("SHOW statement_timeout", &[]).await.unwrap().get(0);
    assert_eq!(
        v, "0",
        "*** connection A's SET statement_timeout crossed to B on the wire ***"
    );
    let v: String = b.query_one("SHOW work_mem", &[]).await.unwrap().get(0);
    assert_eq!(
        v, "4096",
        "*** connection A's SET work_mem crossed to B on the wire ***"
    );

    // RESET is per connection too, and the process-wide storage flag never saw
    // any of it.
    a.simple_query("RESET statement_timeout").await.unwrap();
    let v: String = a.query_one("SHOW statement_timeout", &[]).await.unwrap().get(0);
    assert_eq!(v, "0");
    a.simple_query("SET bulk_load_mode = on").await.unwrap();
    assert!(
        !db.storage.is_bulk_load_mode(),
        "a wire SET bulk_load_mode reached the process-wide storage-engine flag"
    );

    drop(a);
    drop(b);
    a_task.abort();
    b_task.abort();
    handle.abort();
}

// ===========================================================================
// 7903b7111cb4 — currval() is session-scoped and fails closed
// ===========================================================================

/// **THE PROOF.** Two sessions advance the SAME sequence and each sees only its
/// own value; a session that never advanced it gets 55000, not `0`.
///
/// On the pre-fix tree `currval` read the process-wide `SeqRuntime`, so B's
/// first `currval` answers A's id and this test fails at the
/// `*** leaked ***` assertion. Mirrors
/// `two_sessions_never_see_each_others_lastval` in
/// `tests/session_surface_batch_e.rs`.
#[test]
fn two_sessions_never_see_each_others_currval() {
    for family in FAMILIES {
        let db = db();
        db.execute("CREATE SEQUENCE s_g2").unwrap();
        let a = db.create_wire_session("a").unwrap();
        let b = db.create_wire_session("b").unwrap();

        let a_first = family.scalar_i64(&db, a, "SELECT nextval('s_g2')");

        let err = family.query(&db, b, "SELECT currval('s_g2')").expect_err(&format!(
            "{family:?}: *** session B leaked session A's currval *** (it must raise 55000)"
        ));
        assert!(
            err.to_string()
                .contains("currval of sequence \"s_g2\" is not yet defined in this session"),
            "{family:?}: wrong message for an undefined currval: {err}"
        );

        let b_first = family.scalar_i64(&db, b, "SELECT nextval('s_g2')");
        assert_ne!(a_first, b_first, "{family:?}: the sequence did not advance");
        assert_eq!(
            family.scalar_i64(&db, a, "SELECT currval('s_g2')"),
            a_first,
            "{family:?}: B's nextval overwrote A's currval"
        );
        assert_eq!(
            family.scalar_i64(&db, b, "SELECT currval('s_g2')"),
            b_first,
            "{family:?}: B's currval is not B's own value"
        );

        db.destroy_session(a).unwrap();
        db.destroy_session(b).unwrap();
    }
}

/// A fresh session must ERROR — not answer `0`. A `0` handed to a client asking
/// "what id did I just get?" is strictly worse than an error, which is exactly
/// the argument `LASTVAL()` already shipped with.
#[test]
fn currval_on_a_fresh_session_errors_55000_not_zero() {
    for family in FAMILIES {
        let db = db();
        db.execute("CREATE SEQUENCE s_g2_fresh").unwrap();
        let sid = db.create_wire_session("fresh").unwrap();
        let err = family
            .query(&db, sid, "SELECT currval('s_g2_fresh')")
            .expect_err(&format!("{family:?}: currval on a fresh session must error"));
        assert!(
            err.to_string().contains("is not yet defined in this session"),
            "{family:?}: {err}"
        );
        // The `pg_catalog.` spelling is in step.
        assert!(family
            .query(&db, sid, "SELECT pg_catalog.currval('s_g2_fresh')")
            .is_err());
        db.destroy_session(sid).unwrap();
    }
}

/// `currval` and `LASTVAL()` agree about what "this session" means: one
/// `nextval` defines both, `lastval` then follows whichever sequence moved LAST
/// while `currval` follows the NAMED one, and neither is rolled back with the
/// transaction — a sequence advance is not transactional.
///
/// `setval`'s side effect used to be asserted here too and has been split into
/// `setval_defines_currval_for_this_session_without_moving_lastval`, because it
/// is what made this case flaky in a full-suite run: it reaches the DURABLE
/// sequence store through ONE process-global slot (sprinter d15933f528b0), and
/// when that slot's `Weak` fails to upgrade `setval` REFUSES outright.
///
/// Precise about the residual, because "immune" would be a claim this test
/// cannot make: `nextval`'s refill path touches the same slot
/// (`sequences::persist_high_water`, reached on every window refill of a
/// non-volatile runtime), so a dead slot can fail a `nextval` here too. Two
/// things make that far less likely than the `setval` it replaces, and neither
/// is a fix: the exposure window is the two statements between this database's
/// construction — which installs the slot — and its first `nextval`, rather
/// than the whole test; and the OTHER way the slot goes wrong, holding a live
/// FOREIGN engine, is harmless here, because it only misdirects the durable
/// high-water write while `nextval` still serves from this runtime's own
/// in-memory window. `currval` and `lastval` do not touch the slot at all.
#[test]
fn currval_agrees_with_lastval_and_survives_a_rollback() {
    for family in FAMILIES {
        let db = db();
        db.execute("CREATE SEQUENCE s_g2_txn").unwrap();
        db.execute("CREATE SEQUENCE s_g2_other START WITH 500").unwrap();
        let sid = db.create_wire_session("u").unwrap();

        let v = family.scalar_i64(&db, sid, "SELECT nextval('s_g2_txn')");
        assert_eq!(family.scalar_i64(&db, sid, "SELECT lastval()"), v, "{family:?}");
        assert_eq!(
            family.scalar_i64(&db, sid, "SELECT currval('s_g2_txn')"),
            v,
            "{family:?}"
        );

        // `lastval` follows the LAST sequence; `currval` follows the NAMED one.
        let other = family.scalar_i64(&db, sid, "SELECT nextval('s_g2_other')");
        assert_eq!(family.scalar_i64(&db, sid, "SELECT lastval()"), other, "{family:?}");
        assert_eq!(
            family.scalar_i64(&db, sid, "SELECT currval('s_g2_txn')"),
            v,
            "{family:?}: currval('s_g2_txn') followed the wrong sequence"
        );

        // A rolled-back transaction does not roll the advance back.
        db.execute_for_session(sid, "BEGIN").unwrap();
        let inside = family.scalar_i64(&db, sid, "SELECT nextval('s_g2_other')");
        db.execute_for_session(sid, "ROLLBACK").unwrap();
        assert_eq!(
            family.scalar_i64(&db, sid, "SELECT currval('s_g2_other')"),
            inside,
            "{family:?}: currval was rolled back with the transaction"
        );

        db.destroy_session(sid).unwrap();
    }
}

/// `setval('s', n)` defines `currval('s')` FOR THE CALLING SESSION and does NOT
/// move `lastval()` — PostgreSQL's documented side effect, and a behaviour
/// sprinter 7903b7111cb4 added deliberately (`note_session_currval` in the
/// evaluator's `setval` arm, which is NOT `note_session_nextval`: `setval` does
/// not produce a value `nextval` returned).
///
/// # Why this test tolerates one specific failure
///
/// `setval` is the only statement in this batch that must reach the DURABLE
/// sequence store, and `sql::sequences` reaches it through ONE process-global
/// slot: `static PERSIST: OnceLock<Mutex<Option<Weak<StorageEngine>>>>`, written
/// with an unconditional overwrite by every `EmbeddedDatabase` constructor, so
/// the most recently built database wins. In a full-suite run that `Weak` can
/// fail to upgrade while THIS database is still very much alive, and
/// `try_setval` refuses with "setval requires storage context". That is sprinter
/// d15933f528b0 — a third process-global of the same family as the settings
/// registry and `TenantManager::current_context` — and it is not reachable from
/// a test: the volatile branch that skips the persist path is only taken when NO
/// handle was ever installed, which no `EmbeddedDatabase` permits, and
/// re-registering the handle from here would be a test writing a global that
/// other concurrently-running suites read.
///
/// So BOTH outcomes are asserted, and neither is a tautology:
///
/// * `Ok` — the full property (currval moved, lastval did not).
/// * `Err` — it must be EXACTLY d15933f528b0's refusal (any other error fails
///   the test), AND the refused statement must have left this session's
///   `currval` and `lastval` untouched, which is a real fail-closed assertion
///   about the evaluator arm: `note_session_currval` runs only after
///   `try_setval` returns `Ok`.
///
/// The semantics half is additionally pinned unconditionally, with no globals in
/// reach, by `session::scoped`'s unit test `currval_is_per_sequence_and_per_session`
/// (`note_currval` moves currval and leaves lastval alone). What this case adds
/// is the WIRING — that SQL `setval()` calls it — and when d15933f528b0 is fixed
/// the `Err` arm becomes unreachable and should be deleted.
#[test]
fn setval_defines_currval_for_this_session_without_moving_lastval() {
    for family in FAMILIES {
        let db = db();
        db.execute("CREATE SEQUENCE s_g2_setval").unwrap();
        db.execute("CREATE SEQUENCE s_g2_setval_other START WITH 500").unwrap();
        let sid = db.create_wire_session("u").unwrap();

        let before = family.scalar_i64(&db, sid, "SELECT nextval('s_g2_setval')");
        // A second sequence moves LAST, so `lastval()` is pinned to a value that
        // `setval` on the FIRST sequence must not disturb.
        //
        // It is advanced TWICE rather than relying on the `START WITH 500` above
        // to make the two values differ: that clause is parsed
        // (`planner.rs` `SequenceOptions::StartWith`) but the first `nextval`
        // was observed returning 1, not 500, on a fresh database. That is worth
        // its own proof-first investigation and is filed separately — this test
        // is about `setval` and must not depend on it either way.
        family.scalar_i64(&db, sid, "SELECT nextval('s_g2_setval_other')");
        let last = family.scalar_i64(&db, sid, "SELECT nextval('s_g2_setval_other')");
        assert_ne!(before, last, "{family:?}: the two sequences must differ");

        match family.query(&db, sid, "SELECT setval('s_g2_setval', 4242)") {
            Ok(rows) => {
                assert_eq!(rows[0].values[0], Value::Int8(4242), "{family:?}: setval returns n");
                assert_eq!(
                    family.scalar_i64(&db, sid, "SELECT currval('s_g2_setval')"),
                    4242,
                    "{family:?}: setval must define currval for the calling session"
                );
                assert_eq!(
                    family.scalar_i64(&db, sid, "SELECT lastval()"),
                    last,
                    "{family:?}: setval must NOT move lastval — it is not a value nextval returned"
                );
            }
            Err(e) => {
                assert!(
                    e.to_string().contains("setval requires storage context"),
                    "{family:?}: setval failed for a reason that is NOT sprinter d15933f528b0's \
                     process-global persistence slot, so it is a real defect: {e}"
                );
                // The refusal must be clean: `note_session_currval` runs only on
                // the `Ok` path, so neither value may have moved.
                assert_eq!(
                    family.scalar_i64(&db, sid, "SELECT currval('s_g2_setval')"),
                    before,
                    "{family:?}: a REFUSED setval moved this session's currval"
                );
                assert_eq!(
                    family.scalar_i64(&db, sid, "SELECT lastval()"),
                    last,
                    "{family:?}: a REFUSED setval moved this session's lastval"
                );
            }
        }

        db.destroy_session(sid).unwrap();
    }
}

/// PostgreSQL wire (extended protocol): the same isolation over a real socket,
/// and the undefined case arrives as SQLSTATE 55000 — the code a DB-API shim
/// branches on to decide "you have not called nextval yet" rather than "the
/// backend is broken" (XX000, which poolers treat as a server fault).
#[tokio::test]
async fn currval_is_per_connection_on_the_postgres_wire() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    db.execute("CREATE SEQUENCE s_g2_wire").unwrap();
    let server = PgServer::new(
        PgServerConfig::with_address(addr).with_max_connections(16),
        Arc::clone(&db),
    )
    .unwrap();
    let handle = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let cs = format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port());

    let (a, a_task) = {
        let (c, conn) = tokio_postgres::connect(&cs, NoTls).await.unwrap();
        (c, tokio::spawn(async move { conn.await.map(|_| ()).unwrap_or(()) }))
    };
    let (b, b_task) = {
        let (c, conn) = tokio_postgres::connect(&cs, NoTls).await.unwrap();
        (c, tokio::spawn(async move { conn.await.map(|_| ()).unwrap_or(()) }))
    };

    let a_val: i64 = a.query_one("SELECT nextval('s_g2_wire')", &[]).await.unwrap().get(0);

    let err = b
        .query_one("SELECT currval('s_g2_wire')", &[])
        .await
        .expect_err("*** connection B leaked connection A's currval over the wire ***");
    let db_err = err.as_db_error().expect("a DbError");
    assert_eq!(
        db_err.code().code(),
        "55000",
        "currval before nextval must be 55000 object_not_in_prerequisite_state, got {}",
        db_err.code().code()
    );

    let b_val: i64 = b.query_one("SELECT nextval('s_g2_wire')", &[]).await.unwrap().get(0);
    let a_cur: i64 = a.query_one("SELECT currval('s_g2_wire')", &[]).await.unwrap().get(0);
    assert_eq!(a_cur, a_val, "B's nextval overwrote A's currval over the wire");
    let b_cur: i64 = b.query_one("SELECT currval('s_g2_wire')", &[]).await.unwrap().get(0);
    assert_eq!(b_cur, b_val);

    drop(a);
    drop(b);
    a_task.abort();
    b_task.abort();
    handle.abort();
}

// ===========================================================================
// d03de7fc3b22 — tenant max_qps is metered on EVERY execution family
// ===========================================================================

fn tenant_with_qps(manager: &TenantManager, max_qps: usize) -> TenantId {
    let tenant = manager.register_tenant("batch-g2".to_string(), IsolationMode::SharedSchema);
    manager
        .update_resource_limits(
            tenant.id,
            ResourceLimits {
                max_storage_bytes: 100_000_000,
                max_connections: 50,
                max_qps,
            },
        )
        .expect("limits");
    tenant.id
}

/// Activate `tenant_id` and freeze the QPS window, so the only thing that can
/// refill the budget is the test itself.
fn activate_tenant(db: &EmbeddedDatabase, max_qps: usize) -> TenantId {
    db.tenant_manager.set_qps_window(Duration::from_secs(3600));
    let tenant_id = tenant_with_qps(&db.tenant_manager, max_qps);
    db.tenant_manager.set_current_context(TenantContext {
        tenant_id,
        user_id: "g2".to_string(),
        roles: Vec::new(),
        isolation_mode: IsolationMode::SharedSchema,
    });
    tenant_id
}

/// BLOCKER LIFTED (sprinter d03de7fc3b22) — the params family IS metered.
///
/// This was a NOT-SHIPPED tripwire, written to be flipped: it asserted that
/// bound-parameter statements were never counted, and said in its own message
/// what to do the day they were. This is that day, and the two assertions are
/// turned round.
///
/// What unblocked it was NOT the call sites — those were written, ran green and
/// were backed out of v4.39.0 intact. It was what `active_tenant_id()` resolved
/// against: `TenantManager::current_context`, ONE process-global slot shared by
/// every connection and thread, whose only production writer was the REPL's
/// `\tenant use`. Metering every family against that charged a connection's
/// statements to whichever tenant another connection last selected. A
/// connection now carries its OWN tenant
/// (`SessionScopedState::bind_tenant`, resolved from the database name at
/// startup), and the process-global slot survives only as the fallback for the
/// session-LESS callers that have nowhere else to put a context — the embedded
/// API this test drives, and the REPL.
///
/// So this case exercises the FALLBACK layer of the resolver on purpose: there
/// is no session here, `set_current_context` is the only way to express a
/// tenant, and the statements must still be counted against it.
#[test]
fn max_qps_is_enforced_for_bound_parameter_statements() {
    let db = db();
    db.execute("CREATE TABLE g2_qps (id INT PRIMARY KEY, v TEXT)").unwrap();
    // Set the context AFTER the DDL, so the setup statements are not charged.
    let tenant_id = activate_tenant(&db, 4);

    // Twice the budget, entirely in bound-parameter statements: the first four
    // land, the fifth is refused.
    let mut landed = 0usize;
    let mut refusal: Option<String> = None;
    for i in 1..=8i32 {
        match db.execute_params(
            "INSERT INTO g2_qps VALUES ($1, $2)",
            &[Value::Int4(i), Value::String("v".into())],
        ) {
            Ok(_) => landed += 1,
            Err(e) => {
                refusal = Some(e.to_string());
                break;
            }
        }
    }

    assert_eq!(
        landed, 4,
        "*** the params family is unmetered again — {landed} of 8 bound-parameter statements ran \
         against a max_qps of 4. The charge sites live at every execution family's entry point; \
         see `EmbeddedDatabase::charge_tenant_query`. ***"
    );
    let refusal = refusal.expect("statement 5 of 8 must be refused: the budget is 4");
    assert!(
        refusal.to_lowercase().contains("quota exceeded"),
        "the refusal must name the quota, got: {refusal}"
    );
    assert_eq!(
        charged(&db, tenant_id),
        4,
        "the refused statement must not consume budget"
    );

    db.tenant_manager.clear_current_context();
    // Non-vacuity: the four that were NOT refused really ran, so "4" is
    // "counted and admitted", not "never executed".
    let rows = db.query("SELECT count(*) FROM g2_qps", &[]).unwrap();
    assert_eq!(rows[0].values[0], Value::Int8(4));
}

/// The control: the family that WAS metered before sprinter d03de7fc3b22 still
/// is — and READS are now counted too.
///
/// The second half of this case used to be a tripwire in its own right. Reads
/// never reached `execute_in_transaction_inner`, the single pre-existing charge
/// site, so `SELECT` consumed no budget at all and `max_qps` was a write limit
/// wearing a query limit's name. `query()` is one of the eighteen entry points
/// that charge now.
#[test]
fn max_qps_counts_reads_and_writes_on_the_simple_text_family() {
    let db = db();
    db.execute("CREATE TABLE g2_qps_text (id INT PRIMARY KEY)").unwrap();
    let tenant_id = activate_tenant(&db, 3);

    db.execute("INSERT INTO g2_qps_text VALUES (1)").unwrap();
    assert_eq!(charged(&db, tenant_id), 1, "a text write must still be counted");

    db.query("SELECT count(*) FROM g2_qps_text", &[]).unwrap();
    assert_eq!(
        charged(&db, tenant_id),
        2,
        "*** a read is not being counted — `query()` no longer reaches a charge site, so \
         `max_qps` is a write-only limit again (sprinter d03de7fc3b22) ***"
    );

    db.execute("INSERT INTO g2_qps_text VALUES (2)").unwrap();
    let refused = db
        .execute("INSERT INTO g2_qps_text VALUES (3)")
        .expect_err("*** the text family stopped being throttled — pre-existing enforcement broke ***");
    assert!(
        refused.to_string().to_lowercase().contains("quota exceeded"),
        "{refused}"
    );
    assert_eq!(
        charged(&db, tenant_id),
        3,
        "the refused statement must not consume budget"
    );

    db.tenant_manager.clear_current_context();
}

/// The window counter for `tenant_id` right now.
fn charged(db: &EmbeddedDatabase, tenant_id: TenantId) -> usize {
    db.tenant_manager
        .get_quota_tracking(tenant_id)
        .expect("tracking")
        .queries_this_window
}

/// A nested execution the ENGINE performs inside one client statement is not
/// charged again.
///
/// This is NOT a tautology even though only one charge site survives
/// (`execute_in_transaction_inner`), because a `CALL` passes through that ONE
/// site TWICE on the same thread: `execute_call_plan` is dispatched from inside
/// `execute_in_transaction_inner`, and it runs the procedure body by re-entering
/// `execute()` on a `clone_for_trigger()` handle that shares this
/// `TenantManager`. Without the re-entrancy guard one `CALL` would cost two
/// units of a tenant's budget. The layer-delegation shapes (`execute_for_session`
/// -> `execute` -> `execute_in_transaction_inner`) really ARE tautological now —
/// they reach the site exactly once — so they are deliberately not tested here.
///
/// The nesting mechanism is `CALL` and not a TRIGGER because trigger BODIES are
/// not executed by this engine at all: `Planner::create_trigger_to_plan` emits
/// `let body = vec![]` (`src/sql/planner.rs`), so a trigger would have proved
/// nothing. It is also load-bearing that a tenant context is active:
/// `try_autocommit_fast_insert` bails when one is set, which is what guarantees
/// the body's INSERT reaches the charge site rather than a fast path that skips
/// it.
#[test]
fn a_nested_execution_inside_one_statement_is_not_charged_again() {
    let db = db();
    db.execute("CREATE TABLE g2_audit (id INT PRIMARY KEY, v TEXT)")
        .unwrap();
    db.execute("CREATE PROCEDURE g2_proc() LANGUAGE sql AS $$INSERT INTO g2_audit VALUES (99, 'from-body')$$")
        .expect("CREATE PROCEDURE");
    db.execute("CREATE TABLE g2_src (id INT PRIMARY KEY, v TEXT)").unwrap();
    let tenant_id = activate_tenant(&db, 100);

    // A plain write first, so the counter is demonstrably live.
    db.execute("INSERT INTO g2_src VALUES (1, 'a')").unwrap();
    assert_eq!(charged(&db, tenant_id), 1);

    // One CALL: charged at the outer traversal, free at the nested one.
    db.execute("CALL g2_proc()").expect("CALL must execute the body");
    assert_eq!(
        charged(&db, tenant_id),
        2,
        "*** the CALL body was charged as a second client statement ***"
    );

    db.tenant_manager.clear_current_context();

    // Non-vacuity: the nested execution really ran, so the count above is "the
    // body was free", not "the body never happened".
    let rows = db.query("SELECT v FROM g2_audit WHERE id = 99", &[]).unwrap();
    assert_eq!(rows.len(), 1, "the CALL body did not run — the nesting is unproven");
    assert_eq!(rows[0].values[0], Value::String("from-body".to_string()));
}

/// BLOCKER LIFTED (sprinter d03de7fc3b22), over a real socket: the PostgreSQL
/// EXTENDED protocol — the psycopg3 / JDBC / sqlx / node-postgres / Prisma path,
/// i.e. most real traffic — is metered.
///
/// The twin of the embedded case above, and it exercises the same FALLBACK layer
/// deliberately: the connection string says `dbname=postgres`, a RESERVED
/// database name, which binds no tenant (`heliosdb` / `postgres` are system
/// keyspaces, not tenants — see `EmbeddedDatabase::bind_session_tenant`). So
/// this connection resolves through to the process-global context the test sets,
/// exactly as it did before the binding existed, and the charge still lands.
/// `tests/tenant_session_binding_h1.rs` covers the other half — a connection
/// that names a TENANT and is metered against that tenant rather than against
/// whatever another connection selected.
///
/// The assertions are deliberately about the SHAPE of enforcement rather than
/// about statement number four exactly: a driver is free to add round trips
/// (tokio-postgres issues Parse/Describe before Bind/Execute), and the proof is
/// that the budget is finite, spent, and refused with a retryable code.
#[tokio::test]
async fn max_qps_is_enforced_over_the_postgres_extended_protocol() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    db.execute("CREATE TABLE g2_wire_qps (id INT PRIMARY KEY, v TEXT)")
        .unwrap();
    let server = PgServer::new(
        PgServerConfig::with_address(addr).with_max_connections(16),
        Arc::clone(&db),
    )
    .unwrap();
    let handle = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let cs = format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port());
    let (client, task) = {
        let (c, conn) = tokio_postgres::connect(&cs, NoTls).await.unwrap();
        (c, tokio::spawn(async move { conn.await.map(|_| ()).unwrap_or(()) }))
    };

    // The context is set AFTER connecting, so startup traffic is not charged.
    let tenant_id = activate_tenant(&db, 3);

    // Twice the budget, all of it bound-parameter. `client.execute` is
    // Parse/Bind/Execute, so every one of these reaches
    // `execute_params_for_session` — the family that reached NO charge site
    // before this item.
    let mut landed = 0usize;
    let mut refusal: Option<tokio_postgres::Error> = None;
    for i in 1..=6i32 {
        match client
            .execute("INSERT INTO g2_wire_qps VALUES ($1, $2)", &[&i, &"v"])
            .await
        {
            Ok(_) => landed += 1,
            Err(e) => {
                refusal = Some(e);
                break;
            }
        }
    }

    let refusal = refusal.unwrap_or_else(|| {
        panic!(
            "*** the extended protocol is unmetered again: all 6 statements ran against a \
             max_qps of 3 ({landed} landed). This is the psycopg3 / JDBC / sqlx / Prisma path; \
             see `EmbeddedDatabase::charge_tenant_query`. ***"
        )
    });
    assert!(landed >= 1, "the budget refused even the first statement");
    assert!(landed < 6, "nothing was refused");
    // `tokio_postgres::Error`'s own `Display` is just "db error" — the server's
    // SQLSTATE lives in the `DbError`, which is also the only place a real
    // client looks.
    let db_err = refusal.as_db_error().expect("a DbError, not a transport failure");
    assert_eq!(
        db_err.code().code(),
        "53400",
        "a spent quota must reach the client as 53400 configuration_limit_exceeded — XX000 is \
         what a pooler reads as a BROKEN BACKEND and evicts a healthy connection over; got {} / {:?}",
        db_err.code().code(),
        db_err.message()
    );
    assert_eq!(charged(&db, tenant_id), 3, "the window counter must stop at the budget");

    // Non-vacuity: the statements that were NOT refused really ran, so `landed`
    // is "admitted", not "never executed".
    db.tenant_manager.clear_current_context();
    let rows = db.query("SELECT count(*) FROM g2_wire_qps", &[]).unwrap();
    assert_eq!(rows[0].values[0], Value::Int8(landed as i64));

    drop(client);
    task.abort();
    handle.abort();
}

/// WHAT DID SHIP from sprinter d03de7fc3b22, proven end to end over a socket: a
/// spent tenant quota reaches the client as SQLSTATE 53400
/// `configuration_limit_exceeded`, not `XX000 internal_error`.
///
/// This is independent of WHERE metering runs, which is why it survived the item
/// being parked for a release. XX000 is what PgBouncer, pgpool and every HA
/// proxy read as a BROKEN BACKEND, so a perfectly healthy connection that merely
/// hit its own rate limit risked being evicted from the pool; 53400 is class 53
/// `insufficient_resources`, which a client can back off and retry on.
/// `sqlstate::CONFIGURATION_LIMIT_EXCEEDED` had existed with zero callers.
///
/// Driven over the SIMPLE protocol, which was the only family that could produce
/// a quota refusal when this was written. The extended protocol can now too —
/// `max_qps_is_enforced_over_the_postgres_extended_protocol` above asserts the
/// same code on that path — and this case stays as the simple-family twin.
#[tokio::test]
async fn a_spent_quota_reaches_the_client_as_53400_not_an_internal_error() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    db.execute("CREATE TABLE g2_wire_53400 (id INT PRIMARY KEY)").unwrap();
    let server = PgServer::new(
        PgServerConfig::with_address(addr).with_max_connections(16),
        Arc::clone(&db),
    )
    .unwrap();
    let handle = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let cs = format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port());
    let (client, task) = {
        let (c, conn) = tokio_postgres::connect(&cs, NoTls).await.unwrap();
        (c, tokio::spawn(async move { conn.await.map(|_| ()).unwrap_or(()) }))
    };

    let tenant_id = activate_tenant(&db, 1);

    // One statement inside the budget, over the metered family.
    client
        .simple_query("INSERT INTO g2_wire_53400 VALUES (1)")
        .await
        .expect("the first statement is inside the budget");
    assert_eq!(charged(&db, tenant_id), 1, "the simple family must still be metered");

    // The second is refused — and THIS is the subject.
    let err = client
        .simple_query("INSERT INTO g2_wire_53400 VALUES (2)")
        .await
        .expect_err("*** the text family stopped being throttled over the wire ***");
    // `tokio_postgres::Error`'s own `Display` is just "db error" — the server's
    // SQLSTATE and message live in the `DbError`, which is also the only place a
    // real client looks.
    let db_err = err.as_db_error().expect("a DbError, not a transport failure");
    assert_eq!(
        db_err.code().code(),
        "53400",
        "a spent tenant quota must be 53400 configuration_limit_exceeded, not the XX000 \
         internal_error a pooler reads as a broken backend; got {} / {:?}",
        db_err.code().code(),
        db_err.message()
    );
    assert!(
        db_err.message().to_lowercase().contains("quota exceeded"),
        "wrong refusal text over the wire: {:?}",
        db_err.message()
    );

    db.tenant_manager.clear_current_context();
    drop(client);
    task.abort();
    handle.abort();
}
