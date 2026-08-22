//! Materialized-view auto-refresh, end to end, through the PUBLIC API only.
//!
//! WHY THIS FILE EXISTS. `tests/mv_auto_refresh_integration_test.rs` was 9/9 green while
//! auto-refresh refreshed nothing: it is gated behind `--features internal-tests`, it sets
//! the `auto_refresh` metadata key BY HAND (so it never exercises the SQL opt-in), and it
//! never asserts a view's CONTENT or that the scheduler queue drains. Three stacked defects
//! hid behind that:
//!
//!   1. `WITH (auto_refresh = true)` never reached the key the worker reads. The documented
//!      TRAILING spelling was swallowed by sqlparser as an MSSQL table hint; the PG-standard
//!      pre-`AS` spelling only set the display-only `refresh_strategy`; and the
//!      `IF NOT EXISTS` pre-parse hard-coded "no options".
//!   2. `MVScheduler::run()` — the only consumer of the refresh queue — was never spawned by
//!      the library, so scheduled refreshes sat in the queue forever.
//!   3. The scheduler's `perform_refresh` used an incremental refresher fed by a delta
//!      tracker NO DML path writes to, so it would have "succeeded" with zero deltas and
//!      reset the staleness clock over unchanged content.
//!
//! THE RULE FOR THIS FILE: assert CONTENT. A queue that moved, a staleness counter that
//! reset and a `last_refresh` that advanced are all things the broken build could have
//! produced. Only the rows changing proves a refresh happened. The `auto_refresh` metadata
//! key the worker gates on has no SQL projection (`pg_matviews` is an empty PG-compat stub,
//! `pg_mv_staleness` does not expose the flag), so the content assertions below ARE the
//! proof that the opt-in landed on the right key.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::storage::AutoRefreshConfig;
use heliosdb_nano::{Config, EmbeddedDatabase, Value};
use std::time::{Duration, Instant};

const EMPTY: &[&dyn std::fmt::Display] = &[];

/// Longest a positive assertion waits. Generous because a refresh needs a worker tick, a
/// scheduler tick and a full re-materialization; still bounded so a regression fails fast.
const DEADLINE: Duration = Duration::from_secs(45);

/// How long a "must NOT refresh" assertion waits before concluding nothing happened.
/// Several worker + scheduler ticks.
const QUIET_PERIOD: Duration = Duration::from_secs(8);

/// Grace period for a refresh that was already in flight when auto-refresh was turned off,
/// so a following mutation is unambiguously after the last possible refresh.
const SETTLE: Duration = Duration::from_secs(3);

/// An in-memory database whose MV scheduler ticks every second and ignores CPU load, so the
/// tests observe refreshes in bounded time instead of at the 60 s / 15 % production defaults.
fn fast_db() -> EmbeddedDatabase {
    EmbeddedDatabase::with_config(fast_config()).expect("in-memory database")
}

fn fast_config() -> Config {
    let mut config = Config::in_memory();
    config.materialized_views.refresh_check_interval_secs = 1;
    config.materialized_views.default_max_cpu_percent = 100;
    config.materialized_views.max_concurrent_refreshes = 4;
    config
}

/// Worker settings matching `fast_db`: check every second, treat anything as stale, never
/// throttle on CPU.
fn fast_worker() -> AutoRefreshConfig {
    AutoRefreshConfig::new()
        .with_enabled(true)
        .with_interval_seconds(1)
        .with_staleness_threshold(0)
        .with_max_cpu_percent(100.0)
        .with_max_concurrent(4)
}

/// First column of the first row as an integer, whatever numeric variant the aggregate
/// produced. `None` also covers the transient window in which a CONCURRENT refresh has
/// renamed the view's data table away.
fn scalar_i64(db: &EmbeddedDatabase, sql: &str) -> Option<i64> {
    let rows = db.query(sql, EMPTY).ok()?;
    match rows.first()?.values.first()? {
        Value::Int2(n) => Some(i64::from(*n)),
        Value::Int4(n) => Some(i64::from(*n)),
        Value::Int8(n) => Some(*n),
        Value::Float4(n) => Some(*n as i64),
        Value::Float8(n) => Some(*n as i64),
        Value::Numeric(s) | Value::String(s) => s.trim().parse::<f64>().ok().map(|f| f as i64),
        _ => None,
    }
}

fn view_total(db: &EmbeddedDatabase, view: &str) -> Option<i64> {
    scalar_i64(db, &format!("SELECT * FROM {view}"))
}

/// Poll `cond` until it holds or `deadline` elapses. Returns whether it held.
async fn poll_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if cond() {
            return true;
        }
        if start.elapsed() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// `base(id, amount)` summing to 30.
fn seed_base(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE base (id INT PRIMARY KEY, amount INT)")
        .expect("create base table");
    db.execute("INSERT INTO base VALUES (1, 10)").expect("seed 1");
    db.execute("INSERT INTO base VALUES (2, 20)").expect("seed 2");
}

/// Pushes `base`'s sum from 30 to 100.
fn mutate_base(db: &EmbeddedDatabase) {
    db.execute("INSERT INTO base VALUES (3, 70)").expect("mutate base");
}

// ---------------------------------------------------------------------------
// The acceptance test. Everything else in this file supports it.
// ---------------------------------------------------------------------------

/// The measured defect, end to end: create with the DOCUMENTED trailing `WITH` spelling,
/// start the worker, mutate the base table, and assert the VIEW'S CONTENT CHANGES.
///
/// One test, all three fixes: the parser rewrite (A), the metadata key the worker reads (A),
/// the spawned consumer loop (B) and the real refresh dispatch (C). It fails on any one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn end_to_end_with_clause_auto_refresh_updates_content() {
    let db = fast_db();
    seed_base(&db);

    db.execute("CREATE MATERIALIZED VIEW mv_sum AS SELECT SUM(amount) AS total FROM base WITH (auto_refresh = true)")
        .expect("the documented trailing WITH (...) spelling must be accepted");

    assert_eq!(view_total(&db, "mv_sum"), Some(30), "initial materialization");

    db.start_auto_refresh(Some(fast_worker())).await.expect("start worker");
    assert!(db.is_auto_refresh_running());

    mutate_base(&db);

    let refreshed = poll_until(DEADLINE, || view_total(&db, "mv_sum") == Some(100)).await;
    let observed = view_total(&db, "mv_sum");
    db.stop_auto_refresh().await.expect("stop worker");

    assert!(
        refreshed,
        "auto-refresh must change the MV's CONTENT (10+20+70=100); still reading {observed:?}"
    );
}

// ---------------------------------------------------------------------------
// Every documented opt-in spelling — and every opt-OUT
// ---------------------------------------------------------------------------

/// All four accepted ways to write the opt-in must reach the same runtime gate, and views
/// that did not opt in must never be touched. Run in ONE worker session so a spelling that
/// silently dropped its options cannot hide behind a slow tick.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_documented_opt_in_spelling_refreshes_and_opt_outs_do_not() {
    let db = fast_db();
    seed_base(&db);

    // Trailing — REPL help, `sql::phase3::materialized_views` docs, schema skill.
    db.execute(
        "CREATE MATERIALIZED VIEW mv_trailing AS SELECT SUM(amount) AS total FROM base WITH (auto_refresh = true)",
    )
    .expect("trailing WITH");
    // Pre-`AS` — the PostgreSQL standard position.
    db.execute(
        "CREATE MATERIALIZED VIEW mv_pre_as WITH (auto_refresh = true) AS SELECT SUM(amount) AS total FROM base",
    )
    .expect("pre-AS WITH");
    // `IF NOT EXISTS` takes its own pre-parse path, which used to hard-code "no options".
    db.execute(
        "CREATE MATERIALIZED VIEW IF NOT EXISTS mv_ine AS SELECT SUM(amount) AS total FROM base WITH (auto_refresh = true)",
    )
    .expect("IF NOT EXISTS + trailing WITH");
    db.execute(
        "CREATE MATERIALIZED VIEW IF NOT EXISTS mv_ine_pre WITH (auto_refresh = true) AS SELECT SUM(amount) AS total FROM base",
    )
    .expect("IF NOT EXISTS + pre-AS WITH");

    // Negatives.
    db.execute("CREATE MATERIALIZED VIEW mv_none AS SELECT SUM(amount) AS total FROM base")
        .expect("no WITH clause");
    db.execute("CREATE MATERIALIZED VIEW mv_off AS SELECT SUM(amount) AS total FROM base WITH (auto_refresh = false)")
        .expect("explicit opt-out");

    let opted_in = ["mv_trailing", "mv_pre_as", "mv_ine", "mv_ine_pre"];
    let opted_out = ["mv_none", "mv_off"];
    for view in opted_in.iter().chain(opted_out.iter()) {
        assert_eq!(
            view_total(&db, view),
            Some(30),
            "{view}: the WITH clause must not disturb the initial materialization"
        );
    }

    db.start_auto_refresh(Some(fast_worker())).await.expect("start worker");
    mutate_base(&db);

    let all_refreshed = poll_until(DEADLINE, || opted_in.iter().all(|v| view_total(&db, v) == Some(100))).await;
    let observed: Vec<_> = opted_in.iter().map(|v| (*v, view_total(&db, v))).collect();
    let frozen: Vec<_> = opted_out.iter().map(|v| (*v, view_total(&db, v))).collect();
    db.stop_auto_refresh().await.expect("stop worker");

    assert!(
        all_refreshed,
        "every documented opt-in spelling must refresh; got {observed:?}"
    );
    for (view, total) in frozen {
        assert_eq!(total, Some(30), "{view} did not opt in and must never be refreshed");
    }
}

/// `ALTER MATERIALIZED VIEW … SET (…)` must move the runtime gate — in both documented
/// spellings and in both directions — not just the display label.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alter_materialized_view_toggles_auto_refresh() {
    let db = fast_db();
    seed_base(&db);
    db.execute("CREATE MATERIALIZED VIEW mv_alt AS SELECT SUM(amount) AS total FROM base")
        .expect("create MV without opt-in");

    db.start_auto_refresh(Some(fast_worker())).await.expect("start worker");
    mutate_base(&db);

    // Not opted in yet.
    tokio::time::sleep(QUIET_PERIOD).await;
    assert_eq!(view_total(&db, "mv_alt"), Some(30), "no opt-in, no refresh");

    // The undocumented-but-accepted spelling.
    db.execute("ALTER MATERIALIZED VIEW mv_alt SET (auto_refresh = true)")
        .expect("SET auto_refresh = true");
    assert!(
        poll_until(DEADLINE, || view_total(&db, "mv_alt") == Some(100)).await,
        "ALTER … SET (auto_refresh = true) must enable refreshes"
    );

    // Off again — and it must actually stop. Let any refresh that was already in flight
    // when the ALTER landed finish first, so the next mutation is unambiguously after the
    // last possible refresh.
    db.execute("ALTER MATERIALIZED VIEW mv_alt SET (auto_refresh = false)")
        .expect("SET auto_refresh = false");
    tokio::time::sleep(SETTLE).await;
    db.execute("INSERT INTO base VALUES (4, 500)").expect("third mutation");
    tokio::time::sleep(QUIET_PERIOD).await;
    assert_eq!(
        view_total(&db, "mv_alt"),
        Some(100),
        "ALTER … SET (auto_refresh = false) must disable refreshes"
    );

    // The DOCUMENTED spelling must have the same effect as the flag.
    db.execute("ALTER MATERIALIZED VIEW mv_alt SET (refresh_strategy = 'auto')")
        .expect("SET refresh_strategy = 'auto'");
    let re_enabled = poll_until(DEADLINE, || view_total(&db, "mv_alt") == Some(600)).await;
    db.stop_auto_refresh().await.expect("stop worker");
    assert!(
        re_enabled,
        "ALTER … SET (refresh_strategy = 'auto') must enable refreshes too"
    );
}

#[test]
fn alter_rejects_a_non_boolean_auto_refresh() {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
    seed_base(&db);
    db.execute("CREATE MATERIALIZED VIEW mv_v AS SELECT SUM(amount) AS total FROM base")
        .expect("create MV");
    assert!(
        db.execute("ALTER MATERIALIZED VIEW mv_v SET (auto_refresh = maybe)")
            .is_err(),
        "auto_refresh must reject a non-boolean value"
    );
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// Stopping must actually stop: the content freezes at whatever the last refresh produced.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_auto_refresh_actually_stops() {
    let db = fast_db();
    seed_base(&db);
    db.execute("CREATE MATERIALIZED VIEW mv_stop AS SELECT SUM(amount) AS total FROM base WITH (auto_refresh = true)")
        .expect("create MV");

    db.start_auto_refresh(Some(fast_worker())).await.expect("start worker");
    mutate_base(&db);
    assert!(
        poll_until(DEADLINE, || view_total(&db, "mv_stop") == Some(100)).await,
        "a refresh must land before we can prove that stopping suppresses the next one"
    );

    db.stop_auto_refresh().await.expect("stop worker");
    assert!(!db.is_auto_refresh_running());

    // A refresh task already detached when stop() ran finishes on its own; let it, so the
    // mutation below is unambiguously after the last possible refresh.
    tokio::time::sleep(SETTLE).await;
    db.execute("INSERT INTO base VALUES (4, 500)").expect("second mutation");
    tokio::time::sleep(QUIET_PERIOD).await;

    assert_eq!(
        view_total(&db, "mv_stop"),
        Some(100),
        "no refresh may run after stop_auto_refresh"
    );
}

/// A second start must be rejected rather than silently orphaning the first worker (whose
/// dropped command channel would leave its loop spinning forever) and racing a second
/// consumer onto the one shared queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn double_start_is_rejected() {
    let db = fast_db();
    db.start_auto_refresh(Some(fast_worker())).await.expect("first start");
    assert!(db.is_auto_refresh_running());

    assert!(
        db.start_auto_refresh(Some(fast_worker())).await.is_err(),
        "a second start_auto_refresh must be rejected"
    );
    assert!(
        db.is_auto_refresh_running(),
        "the first worker must survive the rejection"
    );

    db.stop_auto_refresh().await.expect("stop worker");
    assert!(!db.is_auto_refresh_running());

    // And a restart after a clean stop must work.
    db.start_auto_refresh(Some(fast_worker())).await.expect("restart");
    assert!(db.is_auto_refresh_running());
    db.stop_auto_refresh().await.expect("stop again");
}

// ---------------------------------------------------------------------------
// The trigger-clone regression
// ---------------------------------------------------------------------------

/// `clone_for_trigger()` hands out short-lived `EmbeddedDatabase` values that share the
/// auto-refresh worker and drop mid-statement. `Drop` used to call `request_stop()`
/// unconditionally, so the FIRST trigger to fire silently killed auto-refresh. Only the last
/// owner may stop it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trigger_execution_does_not_stop_the_worker() {
    let db = fast_db();
    seed_base(&db);
    db.execute("CREATE TABLE audit (note TEXT)").expect("audit table");
    db.execute(
        "CREATE FUNCTION trg_fn() RETURNS TRIGGER AS $$ BEGIN INSERT INTO audit (note) VALUES ('fired'); RETURN NEW; END $$ LANGUAGE plpgsql",
    )
    .expect("trigger function");
    db.execute("CREATE TRIGGER trg AFTER INSERT ON base FOR EACH ROW EXECUTE FUNCTION trg_fn()")
        .expect("trigger");

    db.execute("CREATE MATERIALIZED VIEW mv_trg AS SELECT SUM(amount) AS total FROM base WITH (auto_refresh = true)")
        .expect("create MV");
    db.start_auto_refresh(Some(fast_worker())).await.expect("start worker");

    // Drives the trigger machinery, which mints and drops a `clone_for_trigger()` handle.
    mutate_base(&db);

    assert!(
        db.is_auto_refresh_running(),
        "a trigger firing must not stop the auto-refresh worker"
    );

    let refreshed = poll_until(DEADLINE, || view_total(&db, "mv_trg") == Some(100)).await;
    let still_running = db.is_auto_refresh_running();
    db.stop_auto_refresh().await.expect("stop worker");
    assert!(refreshed, "refreshes must still land after a trigger has fired");
    assert!(still_running, "the worker must still be running after a trigger fired");
}

// ---------------------------------------------------------------------------
// Drop safety (the v4.7.0 guard)
// ---------------------------------------------------------------------------

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("nano_mv_autorefresh_{tag}_{id}"));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn file_backed_config(dir: &std::path::Path) -> Config {
    let mut config = Config::default();
    config.storage.memory_only = false;
    config.storage.path = Some(dir.to_path_buf());
    config.materialized_views.refresh_check_interval_secs = 1;
    config.materialized_views.default_max_cpu_percent = 100;
    config
}

/// Dropping the database WITHOUT stopping must still run `Drop` — v4.7.0 shipped a bug where
/// a background task held an `Arc<EmbeddedDatabase>`, so the refcount never reached zero and
/// the close-time row-counter flush never ran — and must not leave a loop pinning the storage
/// engine.
///
/// Proof of both: the directory reopens, and the counter `Drop` flushes was persisted, so the
/// next insert adds a row instead of reusing a live row id.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drop_after_start_releases_the_database() {
    let dir = scratch_dir("drop");

    {
        let db = EmbeddedDatabase::with_config(file_backed_config(&dir)).expect("file-backed database");
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)")
            .expect("create table");
        db.execute("INSERT INTO t VALUES (1, 1)").expect("insert");
        db.execute("CREATE MATERIALIZED VIEW mv_drop AS SELECT SUM(v) AS total FROM t WITH (auto_refresh = true)")
            .expect("create MV");
        // Nothing goes stale during the test, so the worker loop exits promptly on stop.
        db.start_auto_refresh(Some(
            AutoRefreshConfig::new()
                .with_enabled(true)
                .with_interval_seconds(1)
                .with_staleness_threshold(3600)
                .with_max_cpu_percent(100.0),
        ))
        .await
        .expect("start worker");
        assert!(db.is_auto_refresh_running());
        // Deliberately NO stop_auto_refresh(): the drop path is what is under test.
    }

    // The aborted consumer loop and the worker loop each hold an `Arc<StorageEngine>` until
    // they are reaped, so allow a bounded retry rather than demanding an instant reopen.
    let mut reopened = None;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        match EmbeddedDatabase::with_config(file_backed_config(&dir)) {
            Ok(db) => {
                reopened = Some(db);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(250)).await,
        }
    }
    let db = reopened.expect("the data directory must be reopenable after drop-without-stop");

    // `Drop` flushes row counters at close; if it never ran, the next insert reuses a row id
    // and silently overwrites the existing row instead of adding one.
    db.execute("INSERT INTO t VALUES (2, 2)").expect("insert after reopen");
    assert_eq!(
        scalar_i64(&db, "SELECT COUNT(*) FROM t"),
        Some(2),
        "EmbeddedDatabase::drop must still run (row-counter flush) after start_auto_refresh"
    );

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// start → stop → drop must also exit promptly, with no wedged runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn start_stop_drop_exits_promptly() {
    let dir = scratch_dir("stopdrop");
    {
        let db = EmbeddedDatabase::with_config(file_backed_config(&dir)).expect("file-backed database");
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)")
            .expect("create table");
        db.start_auto_refresh(Some(fast_worker())).await.expect("start worker");
        db.stop_auto_refresh().await.expect("stop worker");
        assert!(!db.is_auto_refresh_running());
    }
    let _ = std::fs::remove_dir_all(&dir);
}
