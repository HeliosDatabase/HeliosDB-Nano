//! Item I4 / sprinter `f469f178aa29` — the residue the `98c88a573a7d` sweep
//! identified and deliberately left in place.
//!
//! Two scopes, and they get OPPOSITE verdicts. That is the point of the file:
//! a "process-global" finding is not automatically a defect, and the census
//! that decides which is which has to be written down somewhere a later sweep
//! will read.
//!
//! # Scope 1 — the MCP SSE session namespace: PER-ENGINE (fixed)
//!
//! `src/mcp/session.rs` kept one `static SESSIONS: DashMap<String, Session>`
//! (line 34 before the fix) keyed by the session id ALONE, and that id is
//! CLIENT-SUPPLIED: `handle_sse` takes it straight off `?session=<id>`
//! (`src/mcp/axum_routes.rs:387-391`) and `handle_post` looks it up straight
//! off the `Mcp-Session-Id` header (`src/mcp/axum_routes.rs:124-130`). Two
//! `mcp_router()`s in one process — different `McpState`, different database,
//! possibly different auth — therefore shared ONE namespace.
//!
//! What that actually causes, established rather than assumed:
//!
//!   1. **The incumbent's live stream is torn down.** `register` was a plain
//!      `SESSIONS.insert(session_id, …)` (`src/mcp/session.rs:41`), so the
//!      second registration of an id DROPPED the first `Session` — and with it
//!      the only `UnboundedSender<Event>` feeding the first client's SSE body.
//!      The `stream::unfold` in `handle_sse` then sees `rx.recv() == None`,
//!      ends the stream, and axum closes the response. A client of router A
//!      picking a session id silently terminates the open SSE connection of a
//!      client of router B on a DIFFERENT database. That is what
//!      `an_sse_stream_is_not_torn_down_by_another_routers_client` measures,
//!      and it needs no progress-emitting tool to observe.
//!   2. **Progress events cross.** Whichever registration survives, BOTH
//!      routers' `sender_for` resolve to it, so the forwarding loop in
//!      `dispatch_streaming_post` (`src/mcp/axum_routes.rs:151-186`) delivers
//!      one database's `notifications/progress` to the other database's
//!      client. The payload is not row data, but it is not nothing either:
//!      `helios_graphrag_search` puts the caller's own query text and its hit
//!      count into `message` (`src/mcp/graphrag_tools.rs:81-96` —
//!      `"graph_rag_search: seeding for '{seed_text}', hops={hops}"` and
//!      `"graph_rag_search: {n} hits"`). Query text plus result cardinality,
//!      delivered to an unrelated client on an unrelated database.
//!
//! The secondary defect filed with it: even within ONE router the id is
//! client-chosen and the SSE handshake is only `Scope::Read`-gated, so any
//! authenticated reader could name another client's id and take its stream
//! over by that same `insert`.
//!
//! # Scope 2 — the per-engine config statics: SPLIT
//!
//! * `JOIN_MEMORY_LIMIT_MB` (`src/sql/executor/join.rs:1739`) is **per-engine
//!   and was wrong**. One writer, `EmbeddedDatabase::with_config` via
//!   `set_join_memory_limit_mb` (`src/lib.rs:10166`), no `SET`, no session
//!   dimension — so it is ENGINE configuration, and last-open-wins meant
//!   opening a second database silently re-capped the first's joins. It
//!   changes whether a query SUCCEEDS or raises `Join exceeds memory limit`,
//!   which is why it is the one member of the family that had to move.
//!   (`EmbeddedDatabase::new` and `new_in_memory` never wrote it at all, so a
//!   database opened either of those ways ran under a cap some OTHER database
//!   had configured. Reading the cap off the engine fixes that too.)
//! * `lock_census::ENABLED`, `write_volume::ENABLED` and
//!   `copy_phase_stats::ENABLED` are **legitimately process-wide**. Their
//!   counters are process-global aggregates by construction — the view
//!   executors take no engine at all (`execute_heliosdb_write_volume()` /
//!   `execute_heliosdb_copy_phase_stats()`,
//!   `src/sql/phase3/system_views.rs:6196` and `:6218`) — and the flag only
//!   decides whether a DIAGNOSTIC counter moves; no query result depends on
//!   it. Scoping the flag per engine while the counters stayed shared would
//!   produce a partially-attributed aggregate, which is strictly worse than
//!   the documented "last config wins".
//!   `the_write_volume_census_is_deliberately_one_process_wide_aggregate`
//!   pins that verdict so a later sweep does not "fix" it for symmetry.
//!
//! # Serialization
//!
//! Every test here takes `residue_guard()`. Every `EmbeddedDatabase::with_config`
//! open writes the three census enable flags (and, before the fix, the
//! join-memory global), and this file opens several, so a concurrent open in
//! this binary would be indistinguishable from the defect under test. Other
//! test binaries are separate processes and cannot interfere.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{Config, EmbeddedDatabase, Value};

/// Serializes this file's tests against each other.
static RESIDUE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn residue_guard() -> std::sync::MutexGuard<'static, ()> {
    RESIDUE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

// =============================================================================
// Scope 2 — per-engine config: the join materialization cap
// =============================================================================

/// Payload width. The join key IS the payload, for two reasons: a narrow key
/// would let `apply_join_input_projection` project the payload out of the
/// build side so the cap was never charged, and a column with no index keeps
/// `try_index_nested_loop_join` out of the plan (it requires an ART index on
/// the right join column, and that operator is deliberately uncapped).
const PAYLOAD_LEN: usize = 1024;
/// 1200 x (~1 KiB tuple + ~1 KiB key + 24 B entry overhead) ≈ 2.5 MB
/// materialized: comfortably over a 1 MB cap, nowhere near a 64 MB one.
const JOIN_ROWS: usize = 1200;

fn join_fixture(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE wide (id INT4 PRIMARY KEY, payload TEXT)")
        .expect("create wide");
    let filler = "x".repeat(PAYLOAD_LEN);
    for chunk_start in (0..JOIN_ROWS).step_by(100) {
        let mut sql = String::from("INSERT INTO wide (id, payload) VALUES ");
        for i in chunk_start..(chunk_start + 100).min(JOIN_ROWS) {
            if i > chunk_start {
                sql.push(',');
            }
            sql.push_str(&format!("({i}, '{i:06}{filler}')"));
        }
        db.execute(&sql).expect("insert wide chunk");
    }
}

/// A self-join whose build side materializes ~2.5 MB. `Ok` when the engine's
/// own cap allows it, `Err` carrying the cap's own wording when it does not.
///
/// `tag` only varies the table aliases. The plan is identical either way, but
/// the SQL TEXT is not — and the engine's result cache is keyed on the text.
/// Without it the later repeats of this query could be answered from cache
/// and would assert nothing about the join's cap at all.
fn run_wide_join(db: &EmbeddedDatabase, tag: u32) -> Result<(), String> {
    db.query(
        &format!("SELECT count(*) FROM wide a{tag} JOIN wide b{tag} ON a{tag}.payload = b{tag}.payload"),
        &[],
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

fn in_memory_with_join_limit(mb: usize) -> EmbeddedDatabase {
    let mut config = Config::in_memory();
    config.performance.join_memory_limit_mb = mb;
    EmbeddedDatabase::with_config(config).expect("open database")
}

/// The defect: `[performance] join_memory_limit_mb` is ENGINE configuration
/// with exactly one writer and no `SET`, yet it lived in a process-global
/// `static` that every `EmbeddedDatabase::with_config` open overwrote
/// (`src/sql/executor/join.rs:1739`, stored from `src/lib.rs:10166`). Opening
/// a second database with a different cap silently re-capped the FIRST
/// database's joins, so a query that had been running for hours started
/// failing `Join exceeds memory limit`.
///
/// Both directions are asserted. A "fix" that simply stopped reading the
/// configuration key would pass the first half and fail the second.
#[test]
fn a_second_database_does_not_recap_the_first_databases_joins() {
    let _guard = residue_guard();
    assert!(
        std::env::var("HELIOSDB_HASH_JOIN_MEM_MB").is_err(),
        "this test asserts the CONFIGURED cap; the documented environment override \
         wins over it and must not be set"
    );

    // Generous engine first, so the process-global holds 64 at this point.
    let generous = in_memory_with_join_limit(64);
    join_fixture(&generous);
    run_wide_join(&generous, 1).expect("a ~2.5 MB join fits under a 64 MB cap");

    // Opening the second engine IS the repro: its open stores ITS cap over the
    // process-global the first engine's joins were reading.
    let stingy = in_memory_with_join_limit(1);

    run_wide_join(&generous, 2).unwrap_or_else(|e| {
        panic!(
            "the generous database's own 64 MB cap must survive another database \
             opening with 1 MB; got: {e}"
        )
    });

    // The other direction: the stingy engine's OWN cap is still enforced, with
    // its own number in the message.
    join_fixture(&stingy);
    let err = run_wide_join(&stingy, 3).expect_err("a ~2.5 MB join must not fit under a 1 MB cap");
    assert!(
        err.contains("Join exceeds memory limit (1 MB)"),
        "the refusal must quote the refusing engine's OWN cap, got: {err}"
    );

    // And the first engine is still fine after the second has raised its error
    // — the cap is resolved per engine, not latched by whoever refused last.
    run_wide_join(&generous, 4).expect("the generous database is unaffected by the stingy one's refusal");
}

/// Control for the test above: within ONE database the configuration key is
/// still what decides. A change that scoped the value per engine but stopped
/// reading the config would pass the isolation test and fail this one.
#[test]
fn a_single_databases_configured_join_cap_is_honoured() {
    let _guard = residue_guard();
    let db = in_memory_with_join_limit(1);
    assert_eq!(
        db.storage.config().performance.join_memory_limit_mb,
        1,
        "the engine carries its own configured cap"
    );
    join_fixture(&db);
    let err = run_wide_join(&db, 5).expect_err("a 1 MB cap must refuse a ~2.5 MB join");
    assert!(
        err.contains("Join exceeds memory limit (1 MB)"),
        "unexpected refusal text: {err}"
    );
}

// =============================================================================
// Scope 2 — per-engine config: the census toggles stay process-wide (PINNED)
// =============================================================================

/// Sum `data_bytes` over every statement class in `heliosdb_write_volume`.
fn write_volume_data_bytes(db: &EmbeddedDatabase) -> i64 {
    let rows = db
        .query("SELECT data_bytes FROM heliosdb_write_volume", &[])
        .expect("heliosdb_write_volume must be queryable");
    rows.iter()
        .map(|t| match t.values.first() {
            Some(Value::Int8(n)) => *n,
            other => panic!("heliosdb_write_volume.data_bytes must be Int8, got {other:?}"),
        })
        .sum()
}

/// **PINS A DELIBERATE NON-FIX.** `write_volume` — and its siblings
/// `lock_census` and `copy_phase_stats` — keep BOTH halves of their state, the
/// enable flag and the counters, in process-globals, and that is correct, not
/// residue.
///
/// The evidence is the read side: `execute_heliosdb_write_volume`
/// (`src/sql/phase3/system_views.rs:6196`) takes no `StorageEngine` at all,
/// because the recording sites are storage funnels with no engine identity in
/// reach. The census is a diagnostic aggregate for the PROCESS, in the same
/// category as a metrics registry, and no query result depends on the flag —
/// unlike `join_memory_limit_mb` above, which decides whether a query is
/// refused.
///
/// So: do NOT "fix" this for symmetry with the join cap. Scoping the enable
/// flag per engine while the counters stay shared yields a partially
/// attributed aggregate, strictly worse than the documented last-config-wins.
/// If a future change really does want per-database write accounting, it has
/// to move the COUNTERS first — and this test is what will say so.
#[test]
fn the_write_volume_census_is_deliberately_one_process_wide_aggregate() {
    let _guard = residue_guard();

    let mut config = Config::in_memory();
    config.performance.write_volume_stats = true;
    let recorder = EmbeddedDatabase::with_config(config).expect("open recorder");
    // No PRIMARY KEY: an unconstrained table takes the instrumented fast
    // literal INSERT funnel (`write_volume` module docs, "Atoms per row").
    recorder
        .execute("CREATE TABLE wv (id INT4, body TEXT)")
        .expect("create wv");
    // Both instrumented INSERT funnels, so this does not depend on which one
    // the planner picks: the single-row literal fast path
    // (`StorageEngine::insert_tuple_fast`, `src/storage/engine.rs:11813`) and
    // the batch path (`insert_prepared_tuples_fast_batch`,
    // `src/storage/engine.rs:12158`).
    for i in 0..200 {
        recorder
            .execute(&format!("INSERT INTO wv (id, body) VALUES ({i}, 'census-{i}')"))
            .expect("insert wv");
    }
    for batch in 0..4 {
        let rows: Vec<String> = (0..50)
            .map(|k| {
                let i = 1000 + batch * 50 + k;
                format!("({i}, 'census-{i}')")
            })
            .collect();
        recorder
            .execute(&format!("INSERT INTO wv (id, body) VALUES {}", rows.join(",")))
            .expect("insert wv batch");
    }

    let recorded = write_volume_data_bytes(&recorder);
    assert!(
        recorded > 0,
        "the recorder enabled `[performance] write_volume_stats` and wrote 200 rows, \
         so the census must have counted bytes"
    );

    // A SECOND database that never enabled the census and has written no rows
    // reads the FIRST one's bytes. That is the process-wide contract.
    //
    // `with_config`, not `new_in_memory`: only `with_config` applies the four
    // `[performance]` runtime toggles at open (`src/lib.rs:10150-10166`);
    // `new` and `new_in_memory` apply none of them. That asymmetry is itself
    // why the join cap could not stay in a global — a database opened with
    // `new(path)` ran under whatever cap the last `with_config` database had
    // stored — and it is why this test has to use `with_config` to observe the
    // flag at all.
    let observer = EmbeddedDatabase::with_config(Config::in_memory()).expect("open observer");
    let observed = write_volume_data_bytes(&observer);
    assert!(
        observed >= recorded,
        "`heliosdb_write_volume` is ONE process-wide aggregate by design — a second \
         database with the census disabled reads the first's counters ({observed} vs \
         {recorded}). If this now fails because the census was scoped per engine, read \
         this test's doc comment before deleting it: the counters have to move too."
    );

    // ... and the flag itself is last-config-wins: the observer opened with the
    // census off, which stopped recording for the recorder as well.
    let frozen = write_volume_data_bytes(&recorder);
    for i in 200..300 {
        recorder
            .execute(&format!("INSERT INTO wv (id, body) VALUES ({i}, 'census-{i}')"))
            .expect("insert wv");
    }
    assert_eq!(
        write_volume_data_bytes(&recorder),
        frozen,
        "opening a database with `write_volume_stats` off disables the census for the \
         whole process — the documented last-config-wins behaviour this test pins"
    );
}

// =============================================================================
// Scope 1 — the MCP SSE session namespace
// =============================================================================

#[cfg(feature = "mcp-endpoint")]
mod mcp_sse {
    use super::residue_guard;

    use std::sync::Arc;
    use std::time::Duration;

    use futures::StreamExt;
    use heliosdb_nano::mcp::{mcp_router, session, McpState};
    use heliosdb_nano::EmbeddedDatabase;

    /// A router mounted on an ephemeral loopback port, the database it serves,
    /// and that database's session namespace.
    struct Mount {
        addr: std::net::SocketAddr,
        namespace: u64,
        _db: Arc<EmbeddedDatabase>,
        server: tokio::task::JoinHandle<()>,
    }

    impl Drop for Mount {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    async fn mount() -> Mount {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("open database"));
        let namespace = db.storage.instance_id();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral loopback port");
        let addr = listener.local_addr().expect("local addr");
        let app = mcp_router(McpState::new(Arc::clone(&db)));
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        // Give the accept loop a moment, matching `tests/mcp_axum_routes.rs`.
        tokio::time::sleep(Duration::from_millis(50)).await;
        Mount {
            addr,
            namespace,
            _db: db,
            server,
        }
    }

    /// Open `GET /sse?session=<id>`. The response headers only arrive after
    /// `handle_sse` has registered the session, so awaiting this is enough to
    /// know the registration landed.
    async fn open_sse(addr: std::net::SocketAddr, session_id: &str) -> reqwest::Response {
        let resp = reqwest::Client::new()
            .get(format!("http://{addr}/sse?session={session_id}"))
            .send()
            .await
            .expect("sse connect");
        assert!(resp.status().is_success(), "sse status {}", resp.status());
        resp
    }

    /// `true` if the SSE body ENDS within `budget`. A live stream with nothing
    /// to say simply pends (the keep-alive interval is 15 s), so the pending
    /// branch is the healthy one; chunks that do arrive (the `endpoint` event)
    /// are consumed and the wait continues.
    async fn stream_ends_within<S, T, E>(stream: &mut S, budget: Duration) -> bool
    where
        S: futures::Stream<Item = Result<T, E>> + Unpin,
    {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return false;
            }
            match tokio::time::timeout(deadline - now, stream.next()).await {
                Err(_elapsed) => return false,    // still pending at the deadline
                Ok(Some(Ok(_chunk))) => continue, // endpoint event / keep-alive
                Ok(Some(Err(_))) => return true,  // transport error
                Ok(None) => return true,          // end of body: the sender was dropped
            }
        }
    }

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(fut)
    }

    /// The headline repro. Two routers, two databases, one process. A client
    /// of router A opening an SSE stream with a session id that a client of
    /// router B already holds used to TEAR DOWN B's stream: `register` was
    /// `SESSIONS.insert(session_id, …)` on a `DashMap<String, Session>` keyed
    /// by the client-supplied id alone (`src/mcp/session.rs:34,41`), so the
    /// second insert dropped the first `Session` and with it the only sender
    /// feeding B's response body.
    #[test]
    fn an_sse_stream_is_not_torn_down_by_another_routers_client() {
        let _guard = residue_guard();
        block_on(async {
            let router_b = mount().await;
            let router_a = mount().await;
            assert_ne!(
                router_a.namespace, router_b.namespace,
                "two mounts must serve two distinct databases for this to prove anything"
            );

            let collide = format!("collide-{}", uuid::Uuid::new_v4());

            let mut stream_b = Box::pin(open_sse(router_b.addr, &collide).await.bytes_stream());
            assert!(
                session::sender_for(router_b.namespace, &collide).is_some(),
                "router B registered the session it was handed"
            );

            // Router A's client names the SAME id. Different database,
            // different `McpState`, possibly different auth.
            let _stream_a = Box::pin(open_sse(router_a.addr, &collide).await.bytes_stream());

            assert!(
                !stream_ends_within(&mut stream_b, Duration::from_millis(1500)).await,
                "a client of router A must not be able to end router B's SSE stream by \
                 naming its session id — the two mounts serve different databases and \
                 must not share a session namespace"
            );
        });
    }

    /// The disclosure direction, stated at the routing layer: the lookup
    /// router A's `handle_post` performs (`session::sender_for`,
    /// `src/mcp/axum_routes.rs:130`) must not resolve a session router B
    /// registered (`session::register`, `src/mcp/axum_routes.rs:391`). While
    /// it did, `dispatch_streaming_post` forwarded A's
    /// `notifications/progress` — which for `helios_graphrag_search` carries
    /// the caller's query text and hit count — into B's client's stream.
    #[test]
    fn one_databases_session_is_not_resolvable_from_another_databases_namespace() {
        let _guard = residue_guard();
        let db_a = EmbeddedDatabase::new_in_memory().expect("database a");
        let db_b = EmbeddedDatabase::new_in_memory().expect("database b");
        let (ns_a, ns_b) = (db_a.storage.instance_id(), db_b.storage.instance_id());

        let shared = format!("shared-{}", uuid::Uuid::new_v4());
        let (granted, _rx) = session::register(ns_a, Some(shared.clone()));
        assert_eq!(granted, shared, "an unused id is granted as asked");

        assert!(
            session::sender_for(ns_a, &shared).is_some(),
            "the owning database resolves its own session"
        );
        assert!(
            session::sender_for(ns_b, &shared).is_none(),
            "a second database must not resolve a session registered against the first"
        );
        assert_eq!(
            session::session_count_in(ns_b),
            0,
            "the second database's namespace is empty"
        );
        assert_eq!(
            session::session_count_in(ns_a),
            1,
            "the first database's namespace holds exactly its own session"
        );
    }

    /// The secondary defect filed with the same item: the session id is
    /// client-chosen and `GET /mcp/sse` is only `Scope::Read`-gated, so even
    /// within ONE router any authenticated reader could name another client's
    /// id and displace it by the same `insert`. A LIVE id is no longer
    /// displaceable — the newcomer is minted a fresh one, which the handshake
    /// announces in its `endpoint` event exactly as it does for a client that
    /// sent no `?session=` at all.
    #[test]
    fn a_live_session_id_cannot_be_seized_by_a_second_client_of_the_same_router() {
        let _guard = residue_guard();
        block_on(async {
            let router = mount().await;
            let wanted = format!("seize-{}", uuid::Uuid::new_v4());

            let mut first = Box::pin(open_sse(router.addr, &wanted).await.bytes_stream());
            assert_eq!(
                session::session_count_in(router.namespace),
                1,
                "the first client holds the id it asked for"
            );

            let _second = Box::pin(open_sse(router.addr, &wanted).await.bytes_stream());
            assert_eq!(
                session::session_count_in(router.namespace),
                2,
                "the second client must be given its OWN session rather than seizing the \
                 first client's; one entry here means the first was overwritten"
            );

            assert!(
                !stream_ends_within(&mut first, Duration::from_millis(1500)).await,
                "the incumbent's SSE stream must survive another client asking for its id"
            );
        });
    }

    /// Control: a reconnecting client whose previous stream is gone still gets
    /// its id back. The anti-seizure rule keys on LIVENESS, not on the id ever
    /// having been used — otherwise every reconnect would silently change
    /// session ids forever.
    #[test]
    fn an_id_whose_stream_has_closed_is_handed_back_on_reconnect() {
        let _guard = residue_guard();
        let db = EmbeddedDatabase::new_in_memory().expect("database");
        let ns = db.storage.instance_id();
        let wanted = format!("reconnect-{}", uuid::Uuid::new_v4());

        {
            let (granted, _rx) = session::register(ns, Some(wanted.clone()));
            assert_eq!(granted, wanted);
        } // receiver dropped: the stream this session fed is gone.

        let (regranted, _rx) = session::register(ns, Some(wanted.clone()));
        assert_eq!(
            regranted, wanted,
            "an id whose receiver has been dropped is free again and is handed back"
        );
    }

    /// A client that sends no `?session=` is minted a fresh id, unchanged.
    #[test]
    fn a_client_that_names_no_session_is_minted_one() {
        let _guard = residue_guard();
        let db = EmbeddedDatabase::new_in_memory().expect("database");
        let ns = db.storage.instance_id();

        let (first, _rx1) = session::register(ns, None);
        let (second, _rx2) = session::register(ns, None);
        assert!(!first.is_empty() && !second.is_empty(), "minted ids are non-empty");
        assert_ne!(first, second, "each anonymous client gets its own session");
        assert_eq!(session::session_count_in(ns), 2);

        session::drop_session(ns, &first);
        assert!(session::sender_for(ns, &first).is_none(), "dropped session is gone");
        assert!(session::sender_for(ns, &second).is_some(), "the other is untouched");
    }
}
