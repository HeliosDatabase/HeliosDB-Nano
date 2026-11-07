//! Spec R5.1 (`docs/plans/specs/R5_1_writer_lock.md`): the per-table writer lock.
//!
//! The B3 reviews reproduced one defect many ways: nothing kept a writer out of a
//! table while something rewrote it. A params-family `UPDATE … WHERE g >= $1` during a
//! merge put 686 merged rows back to their pre-merge image; a `DELETE` reported 3000
//! deleted and 1920 came back; `TRUNCATE` cleared the indexes and then failed at commit;
//! a key-moving statement left every old key stale; an autocommit `UPDATE` beside `ALTER
//! TABLE … ADD COLUMN … DEFAULT` left rows narrower than the schema.
//!
//! So every writer kind, on both executor families, runs beside a merge and beside an
//! `ADD COLUMN`, and afterwards the rows, every index lookup, the `COUNT(*)` fast paths
//! and a restart must agree, and every statement either succeeded or got a clean 55P03.
//! Handshakes are deterministic: the counters of the lock itself, and the merge's stop
//! points. Nothing here sleeps to arrange a race.
//!
//! Nothing here may hang either: a run of this file once held the build lock for four
//! hours, because one writer's failure left the others looping and the scope joining them.
//! [`beside_writers`] stops every writer on every way out and bounds them; [`watchdog`]
//! ends the process if a statement itself never returns.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::collections::HashSet;
use std::io::Write as _;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::session::IsolationLevel;
use heliosdb_nano::storage::branch_catalog::MergeFailSite;
use heliosdb_nano::{EmbeddedDatabase, Tuple, Value, TABLE_LOCK_TIMEOUT_MARKER};
use tokio_postgres::NoTls;

const SEED_ROWS: i64 = 20_000;

/// [`watchdog`]'s limit for the tests that seed [`SEED_ROWS`] and restart.
const LONG_TEST_AT_MOST: Duration = Duration::from_secs(1200);
/// [`watchdog`]'s limit for every other test that waits for something.
const TEST_AT_MOST: Duration = Duration::from_secs(600);

/// What no flag can stop: a statement that never returns. Keep the result alive for the
/// length of the test. Past `limit` this names the test and ends the process, taking the
/// rest of the run with it, which is what a run that would otherwise hold the build lock
/// until somebody looks should do. It writes to `stderr` itself because libtest captures
/// `eprintln!`, and what it captured would die with the process.
#[must_use = "dropping it disarms the watchdog"]
fn watchdog(test: &'static str, limit: Duration) -> mpsc::Sender<()> {
    let (disarm, disarmed) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        if let Err(mpsc::RecvTimeoutError::Timeout) = disarmed.recv_timeout(limit) {
            let _ = writeln!(
                std::io::stderr(),
                "\nwatchdog: `{test}` still running after {} s: something does not return. \
                 Ending the process.",
                limit.as_secs()
            );
            std::process::abort();
        }
    });
    disarm
}

fn int(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Int2(v)) => i64::from(*v),
        Some(Value::Int4(v)) => i64::from(*v),
        Some(Value::Int8(v)) => *v,
        other => panic!("not an integer: {other:?}"),
    }
}

fn text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(v)) => v.clone(),
        other => panic!("not text: {other:?}"),
    }
}

fn rows(db: &EmbeddedDatabase, sql: &str) -> Vec<Tuple> {
    db.query(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"))
}

fn count(db: &EmbeddedDatabase, sql: &str) -> i64 {
    int(rows(db, sql)[0].values.first())
}

fn is_lock_timeout(error: &heliosdb_nano::Error) -> bool {
    error.to_string().contains(TABLE_LOCK_TIMEOUT_MARKER)
}

fn is_unique_violation(error: &heliosdb_nano::Error) -> bool {
    let text = error.to_string().to_lowercase();
    text.contains("unique") || text.contains("duplicate")
}

/// `d`: a PRIMARY KEY, a UNIQUE key, and a band of 1 row in 100 for a multi-row statement
/// on a non-key predicate.
fn seed(db: &EmbeddedDatabase, rows: i64) {
    db.execute("CREATE TABLE d (id INT PRIMARY KEY, code TEXT UNIQUE, v INT, g INT, band INT)")
        .unwrap();
    for start in (1..=rows).step_by(1000) {
        let values: Vec<String> = (start..(start + 1000).min(rows + 1))
            .map(|id| format!("({id}, 'a{id}', 0, 0, {})", id % 100))
            .collect();
        db.execute(&format!("INSERT INTO d VALUES {}", values.join(", ")))
            .unwrap();
    }
    db.execute("CREATE TABLE src (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO src VALUES (1), (2), (3)").unwrap();
}

/// Branch `r` changes every seeded row's key and `v`; main does not see it until the merge.
fn branch_changing_every_row(db: &EmbeddedDatabase) {
    db.execute("CREATE BRANCH r AS OF NOW").unwrap();
    db.execute("USE BRANCH r").unwrap();
    db.execute("UPDATE d SET code = 'm' || CAST(id AS TEXT), v = 100")
        .unwrap();
    db.execute("USE BRANCH main").unwrap();
}

/// Stops whoever reaches the first tick of `site` there. The first channel says it got
/// there; dropping the second lets it go on.
fn stop_at_site(db: &EmbeddedDatabase, site: MergeFailSite) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (reached_tx, reached_rx) = mpsc::channel::<()>();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let (reached_tx, go_rx) = (Mutex::new(reached_tx), Mutex::new(go_rx));
    let ticks = AtomicUsize::new(0);
    db.storage.debug_on_merge_site(
        site,
        Some(Arc::new(move || {
            if ticks.fetch_add(1, Ordering::SeqCst) == 0 {
                let _ = reached_tx.lock().unwrap().send(());
                let _ = go_rx.lock().unwrap().recv();
            }
        })),
    );
    (reached_rx, go_tx)
}

/// A condition another thread will make true: wait on its state, never on time.
fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !condition() {
        assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::yield_now();
    }
}

/// Runs `statement` and stops it at `site`, holding its tables; `writer` starts and is
/// seen to wait for `table`; then the statement goes on, and the writer after it.
fn a_writer_waiting_for<T: Send>(
    db: &EmbeddedDatabase,
    statement: &str,
    site: MergeFailSite,
    table: &str,
    writer: impl FnOnce() -> T + Send,
) -> (heliosdb_nano::Result<u64>, T) {
    let (reached, go) = stop_at_site(db, site);
    let outcome = std::thread::scope(|scope| {
        let running = scope.spawn(|| db.execute(statement));
        while reached.recv_timeout(Duration::from_millis(2)).is_err() {
            assert!(!running.is_finished(), "`{statement}` ended before {site:?}");
        }
        let writing = scope.spawn(writer);
        wait_for("the writer to wait for the table", || {
            let (exclusive, shared) = db.storage.debug_table_lock_waiters(table);
            exclusive + shared > 0
        });
        assert!(!writing.is_finished(), "the writer waits for `{statement}`");
        drop(go);
        let ran = running.join().unwrap();
        (ran, writing.join().unwrap())
    });
    db.storage.debug_on_merge_site(site, None);
    outcome
}

/// The rows, the PRIMARY KEY and UNIQUE lookups on both families, the `COUNT(*)` fast
/// paths and the key uniqueness all say the same. Returns the rows, for a comparison
/// across a restart.
fn assert_everything_agrees(db: &EmbeddedDatabase, what: &str) -> Vec<(i64, String)> {
    let found: Vec<(i64, String)> = rows(db, "SELECT id, code FROM d ORDER BY id")
        .iter()
        .map(|row| (int(row.values.first()), text(row.values.get(1))))
        .collect();
    let total = found.len() as i64;
    assert_eq!(count(db, "SELECT COUNT(*) FROM d"), total, "{what}: COUNT(*)");
    assert_eq!(
        count(db, "SELECT COUNT(*) FROM d WHERE id >= 1"),
        total,
        "{what}: COUNT(*) by the PRIMARY KEY"
    );
    let distinct: HashSet<&str> = found.iter().map(|(_, code)| code.as_str()).collect();
    assert_eq!(distinct.len(), found.len(), "{what}: a UNIQUE key held twice");
    let step = (found.len() / 500).max(1);
    for (id, code) in found.iter().step_by(step) {
        let by_key = rows(db, &format!("SELECT code FROM d WHERE id = {id}"));
        assert_eq!(by_key.len(), 1, "{what}: row {id} by its PRIMARY KEY");
        assert_eq!(&text(by_key[0].values.first()), code, "{what}: row {id}");
        let by_code = rows(db, &format!("SELECT id FROM d WHERE code = '{code}'"));
        assert_eq!(by_code.len(), 1, "{what}: key {code}");
        assert_eq!(int(by_code[0].values.first()), *id, "{what}: key {code}");
        let bound = db
            .query_params("SELECT id FROM d WHERE code = $1", &[Value::String(code.clone())])
            .unwrap();
        assert_eq!(bound.len(), 1, "{what}: key {code}, bound");
    }
    found
}

/// One kind of writer: unique keys from `base + round`, with `round` counting up.
type Writer = fn(&EmbeddedDatabase, i64, i64) -> heliosdb_nano::Result<()>;

fn own_key(base: i64, round: i64) -> i64 {
    base + round * 10
}

/// How far apart two writers' keys start, so that [`MAX_WRITER_ROUNDS`] of [`own_key`] stay
/// far from the next writer's. Every writer in [`WRITERS`] still fits an `INT`.
const KEY_SPACING: i64 = 10_000_000;
/// A writer stops at this round whatever else is going on. [`own_key`] has then moved
/// 2,000,000, a fifth of [`KEY_SPACING`]: a run that lasts cannot end in a false
/// `Duplicate key` between two writers.
const MAX_WRITER_ROUNDS: i64 = 200_000;
/// A writer stops at this age too, and a statement that has not finished by then fails its
/// test.
const WRITERS_RUN_AT_MOST: Duration = Duration::from_secs(300);

fn new_row_values(key: i64) -> Vec<Value> {
    vec![
        Value::Int4(key as i32),
        Value::String(format!("w{key}")),
        Value::Int4(-1),
        Value::Int4(0),
        Value::Int4(-1),
    ]
}

const INSERT_ONE_BOUND: &str = "INSERT INTO d (id, code, v, g, band) VALUES ($1, $2, $3, $4, $5)";

fn text_insert_one(db: &EmbeddedDatabase, base: i64, round: i64) -> heliosdb_nano::Result<()> {
    let key = own_key(base, round);
    db.execute(&format!(
        "INSERT INTO d (id, code, v, g, band) VALUES ({key}, 'w{key}', -1, 0, -1)"
    ))
    .map(drop)
}

fn params_insert_one(db: &EmbeddedDatabase, base: i64, round: i64) -> heliosdb_nano::Result<()> {
    db.execute_params(INSERT_ONE_BOUND, &new_row_values(own_key(base, round)))
        .map(drop)
}

fn text_insert_many(db: &EmbeddedDatabase, base: i64, round: i64) -> heliosdb_nano::Result<()> {
    let (a, b) = (own_key(base, round), own_key(base, round) + 1);
    db.execute(&format!(
        "INSERT INTO d (id, code, v, g, band) VALUES ({a}, 'w{a}', -1, 0, -1), ({b}, 'w{b}', -1, 0, -1)"
    ))
    .map(drop)
}

fn params_insert_many(db: &EmbeddedDatabase, base: i64, round: i64) -> heliosdb_nano::Result<()> {
    let (a, b) = (own_key(base, round), own_key(base, round) + 1);
    let mut params = new_row_values(a);
    params.extend(new_row_values(b));
    db.execute_params(
        "INSERT INTO d (id, code, v, g, band) VALUES ($1, $2, $3, $4, $5), ($6, $7, $8, $9, $10)",
        &params,
    )
    .map(drop)
}

fn params_insert_batch(db: &EmbeddedDatabase, base: i64, round: i64) -> heliosdb_nano::Result<()> {
    let key = own_key(base, round);
    db.execute_many_params(INSERT_ONE_BOUND, &[new_row_values(key), new_row_values(key + 1)])
        .map(drop)
}

fn text_update_by_key(db: &EmbeddedDatabase, _base: i64, round: i64) -> heliosdb_nano::Result<()> {
    let id = 1 + round % SEED_ROWS;
    db.execute(&format!("UPDATE d SET g = g + 1 WHERE id = {id}")).map(drop)
}

fn params_update_by_key(db: &EmbeddedDatabase, _base: i64, round: i64) -> heliosdb_nano::Result<()> {
    let id = 1 + round % SEED_ROWS;
    db.execute_params("UPDATE d SET g = g + 1 WHERE id = $1", &[Value::Int4(id as i32)])
        .map(drop)
}

fn text_update_by_predicate(db: &EmbeddedDatabase, _base: i64, round: i64) -> heliosdb_nano::Result<()> {
    db.execute(&format!("UPDATE d SET g = g + 1 WHERE band = {}", round % 100))
        .map(drop)
}

fn params_update_by_predicate(db: &EmbeddedDatabase, _base: i64, round: i64) -> heliosdb_nano::Result<()> {
    db.execute_params(
        "UPDATE d SET g = g + 1 WHERE band = $1",
        &[Value::Int4((round % 100) as i32)],
    )
    .map(drop)
}

fn text_delete_by_key(db: &EmbeddedDatabase, base: i64, round: i64) -> heliosdb_nano::Result<()> {
    let key = own_key(base, round);
    text_insert_one(db, base, round)?;
    db.execute(&format!("DELETE FROM d WHERE id = {key}")).map(drop)
}

fn params_delete_by_key(db: &EmbeddedDatabase, base: i64, round: i64) -> heliosdb_nano::Result<()> {
    let key = own_key(base, round);
    params_insert_one(db, base, round)?;
    db.execute_params("DELETE FROM d WHERE id = $1", &[Value::Int4(key as i32)])
        .map(drop)
}

fn text_delete_by_predicate(db: &EmbeddedDatabase, base: i64, round: i64) -> heliosdb_nano::Result<()> {
    let key = own_key(base, round);
    text_insert_one(db, base, round)?;
    db.execute(&format!("DELETE FROM d WHERE code = 'w{key}' AND v = -1"))
        .map(drop)
}

fn params_delete_by_predicate(db: &EmbeddedDatabase, base: i64, round: i64) -> heliosdb_nano::Result<()> {
    let key = own_key(base, round);
    params_insert_one(db, base, round)?;
    db.execute_params(
        "DELETE FROM d WHERE code = $1 AND v = $2",
        &[Value::String(format!("w{key}")), Value::Int4(-1)],
    )
    .map(drop)
}

fn text_on_conflict(db: &EmbeddedDatabase, base: i64, round: i64) -> heliosdb_nano::Result<()> {
    let key = own_key(base, round % 10);
    db.execute(&format!(
        "INSERT INTO d (id, code, v, g, band) VALUES ({key}, 'w{key}', -1, 0, -1) \
         ON CONFLICT (id) DO UPDATE SET g = d.g + 1"
    ))
    .map(drop)
}

fn params_on_conflict(db: &EmbeddedDatabase, base: i64, round: i64) -> heliosdb_nano::Result<()> {
    let key = own_key(base, round % 10);
    db.execute_params(
        "INSERT INTO d (id, code, v, g, band) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (id) DO UPDATE SET g = d.g + 1",
        &new_row_values(key),
    )
    .map(drop)
}

fn text_insert_select(db: &EmbeddedDatabase, base: i64, round: i64) -> heliosdb_nano::Result<()> {
    let key = own_key(base, round);
    db.execute(&format!(
        "INSERT INTO d (id, code, v, g, band) SELECT id + {key}, 'w' || CAST(id + {key} AS TEXT), -1, 0, -1 FROM src"
    ))
    .map(drop)
}

fn params_insert_select(db: &EmbeddedDatabase, base: i64, round: i64) -> heliosdb_nano::Result<()> {
    let key = own_key(base, round);
    db.execute_params(
        "INSERT INTO d (id, code, v, g, band) SELECT id + $1, $2 || CAST(id AS TEXT), -1, 0, -1 FROM src",
        &[Value::Int4(key as i32), Value::String(format!("w{key}_"))],
    )
    .map(drop)
}

fn params_returning(db: &EmbeddedDatabase, base: i64, round: i64) -> heliosdb_nano::Result<()> {
    let key = own_key(base, round);
    db.execute_params_returning(
        "INSERT INTO d (id, code, v, g, band) VALUES ($1, $2, $3, $4, $5) RETURNING id",
        &new_row_values(key),
    )
    .map(drop)
}

/// A whole explicit transaction, on a session of its own: an update of a seeded row, on
/// the text family, and an insert, on the params family. Its COMMIT, or the rollback of
/// a statement refused, is part of the test.
fn in_a_session_transaction(db: &EmbeddedDatabase, base: i64, round: i64) -> heliosdb_nano::Result<()> {
    let session = db.create_session("writer", IsolationLevel::ReadCommitted)?;
    let key = own_key(base, round);
    let id = 1 + round % SEED_ROWS;
    let result = (|| {
        db.begin_transaction_for_session(session)?;
        db.execute_for_session(session, &format!("UPDATE d SET g = g + 1 WHERE id = {id}"))?;
        db.execute_params_for_session(session, INSERT_ONE_BOUND, &new_row_values(key))?;
        db.commit_transaction_for_session(session)
    })();
    if result.is_err() {
        let _ = db.rollback_transaction_for_session(session);
    }
    db.destroy_session(session)?;
    result
}

/// [`in_a_session_transaction`], on the handle's own transaction.
fn in_a_transaction_handle(db: &EmbeddedDatabase, base: i64, round: i64) -> heliosdb_nano::Result<()> {
    let key = own_key(base, round);
    #[allow(deprecated)]
    let txn = db.begin_transaction()?;
    txn.execute(&format!(
        "INSERT INTO d (id, code, v, g, band) VALUES ({key}, 'w{key}', -1, 0, -1)"
    ))?;
    txn.commit()
}

const WRITERS: &[(&str, Writer)] = &[
    ("text INSERT", text_insert_one),
    ("params INSERT", params_insert_one),
    ("text multi-row INSERT", text_insert_many),
    ("params multi-row INSERT", params_insert_many),
    ("params batch INSERT", params_insert_batch),
    ("text UPDATE by PK", text_update_by_key),
    ("params UPDATE by PK", params_update_by_key),
    ("text UPDATE by predicate", text_update_by_predicate),
    ("params UPDATE by predicate", params_update_by_predicate),
    ("text DELETE by PK", text_delete_by_key),
    ("params DELETE by PK", params_delete_by_key),
    ("text DELETE by predicate", text_delete_by_predicate),
    ("params DELETE by predicate", params_delete_by_predicate),
    ("text ON CONFLICT", text_on_conflict),
    ("params ON CONFLICT", params_on_conflict),
    ("text INSERT SELECT", text_insert_select),
    ("params INSERT SELECT", params_insert_select),
    ("params RETURNING", params_returning),
    ("a session transaction", in_a_session_transaction),
    ("a transaction handle", in_a_transaction_handle),
];

/// Sets the flag it is given on every way out of the scope it is in, unwinding included.
struct StopWriters<'a>(&'a AtomicBool);

impl Drop for StopWriters<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|text| (*text).to_string()))
        .unwrap_or_else(|| "a panic with no message".to_string())
}

/// One thread per name loops on `write(index, round)` until `statement` is done, having
/// each finished one round before it starts. Every round must succeed or be a clean 55P03.
///
/// This cannot hang on anything a flag can stop, and it reports the first thing that went
/// wrong rather than what that led to:
/// - a writer that fails, or panics, stops the others and the warm-up at once, and the test
///   fails with that writer's round and error;
/// - `statement` unwinding stops the writers too, so the scope joining them returns and the
///   panic comes through;
/// - a writer stops at [`MAX_WRITER_ROUNDS`] or [`WRITERS_RUN_AT_MOST`], whichever is first.
///   If `statement` had not finished by then, the test fails saying so.
///
/// A statement or a writer that never returns is [`watchdog`]'s.
fn beside_writers<R>(
    what: &str,
    names: &[String],
    write: impl Fn(usize, i64) -> heliosdb_nano::Result<()> + Sync,
    statement: impl FnOnce() -> R,
) -> R {
    let deadline = Instant::now() + WRITERS_RUN_AT_MOST;
    let done = AtomicBool::new(false);
    let ran: Vec<AtomicBool> = names.iter().map(|_| AtomicBool::new(false)).collect();
    let failure: Mutex<Option<String>> = Mutex::new(None);
    let out_of_bound: Mutex<Option<String>> = Mutex::new(None);
    let finished = std::thread::scope(|scope| {
        // First, so it goes last: the writers are stopped before the scope joins them.
        let _stop = StopWriters(&done);
        for (index, name) in names.iter().enumerate() {
            let (write, done, ran, failure, out_of_bound) = (&write, &done, &ran, &failure, &out_of_bound);
            scope.spawn(move || {
                let mut round = 0_i64;
                while !done.load(Ordering::Acquire) {
                    if round >= MAX_WRITER_ROUNDS || Instant::now() >= deadline {
                        out_of_bound
                            .lock()
                            .unwrap()
                            .get_or_insert_with(|| format!("{name} stopped at round {round}"));
                        done.store(true, Ordering::Release);
                        return;
                    }
                    let complaint = match catch_unwind(AssertUnwindSafe(|| write(index, round))) {
                        Ok(Ok(())) => None,
                        Ok(Err(e)) if is_lock_timeout(&e) => None,
                        Ok(Err(e)) => Some(format!("neither a success nor a clean 55P03: {e}")),
                        Err(payload) => Some(format!("panicked: {}", panic_text(&*payload))),
                    };
                    if let Some(complaint) = complaint {
                        failure
                            .lock()
                            .unwrap()
                            .get_or_insert_with(|| format!("{name}, round {round}: {complaint}"));
                        done.store(true, Ordering::Release);
                        return;
                    }
                    round += 1;
                    ran[index].store(true, Ordering::Release);
                }
            });
        }
        loop {
            if done.load(Ordering::Acquire) {
                break None;
            }
            if ran.iter().all(|ran| ran.load(Ordering::Acquire)) {
                break Some(statement());
            }
            std::thread::yield_now();
        }
    });
    if let Some(failure) = failure.lock().unwrap().take() {
        panic!("beside `{what}`: {failure}");
    }
    if let Some(stopped) = out_of_bound.lock().unwrap().take() {
        panic!(
            "`{what}` had not finished when the writers reached their bound of {} s or {MAX_WRITER_ROUNDS} \
             rounds: {stopped}",
            WRITERS_RUN_AT_MOST.as_secs()
        );
    }
    finished.expect("the warm-up only ends early for one of the two reasons above")
}

/// Who wrote row `id`, for a failure message: a writer's keys start at `KEY_SPACING *
/// (its index + 1)` and move ten per round.
fn writer_of(id: i64) -> String {
    if id <= SEED_ROWS {
        return "a seeded row: UPDATE by PK or predicate, or a session transaction".to_string();
    }
    match usize::try_from(id / KEY_SPACING - 1)
        .ok()
        .and_then(|index| WRITERS.get(index))
    {
        Some((kind, _)) => format!("{kind}, near round {}", id % KEY_SPACING / 10),
        None => "no writer's range".to_string(),
    }
}

/// After an `ADD COLUMN x INT DEFAULT 5`, every row has it. Names the rows that do not,
/// and whose they are, so that a failure says which writer without a second run.
fn assert_every_row_has_the_default(db: &EmbeddedDatabase, what: &str) {
    let all = rows(db, "SELECT id, code, v, g, band, x FROM d ORDER BY id");
    let wrong: Vec<String> = all
        .iter()
        .filter(|row| {
            !matches!(
                row.values.get(5),
                Some(Value::Int2(5) | Value::Int4(5) | Value::Int8(5))
            )
        })
        .map(|row| {
            let id = int(row.values.first());
            format!(
                "id {id}, code {:?}, x {:?}, g {:?}: {}",
                row.values.get(1),
                row.values.get(5),
                row.values.get(3),
                writer_of(id)
            )
        })
        .collect();
    assert!(
        wrong.is_empty(),
        "{what}: {} of {} rows do not have x = 5. The first of them:\n{}",
        wrong.len(),
        all.len(),
        wrong.iter().take(50).cloned().collect::<Vec<_>>().join("\n")
    );
}

/// Every writer loops on `d` until `statement` is done, having each finished at least one
/// statement before it starts. Every statement must succeed, or be a clean 55P03.
fn every_writer_beside(db: &EmbeddedDatabase, statement: &str) {
    let late_before = db.storage.debug_table_lock_late_acquisitions();
    let names: Vec<String> = WRITERS.iter().map(|(kind, _)| (*kind).to_string()).collect();
    let result = beside_writers(
        statement,
        &names,
        |index, round| (WRITERS[index].1)(db, KEY_SPACING * (index as i64 + 1), round),
        || db.execute(statement),
    );
    result.unwrap_or_else(|e| panic!("`{statement}` beside every writer: {e}"));
    assert_eq!(
        db.storage.debug_table_lock_late_acquisitions(),
        late_before,
        "every SQL path locks at its statement, not at its first staged write"
    );
}

#[test]
fn every_writer_kind_beside_a_merge_leaves_everything_agreeing() {
    let _watchdog = watchdog(
        "every_writer_kind_beside_a_merge_leaves_everything_agreeing",
        LONG_TEST_AT_MOST,
    );
    let dir = tempfile::tempdir().unwrap();
    let found = {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        seed(&db, SEED_ROWS);
        branch_changing_every_row(&db);
        every_writer_beside(&db, "MERGE BRANCH r INTO main");
        let found = assert_everything_agrees(&db, "after the merge");
        // Nothing came back: every seeded row has the merged key and the merged value.
        let seeded: Vec<(i64, String)> = found.iter().filter(|(id, _)| *id <= SEED_ROWS).cloned().collect();
        assert_eq!(seeded.len() as i64, SEED_ROWS, "no seeded row is gone");
        for (id, code) in &seeded {
            assert_eq!(code, &format!("m{id}"), "row {id} went back to its pre-merge image");
        }
        assert_eq!(count(&db, "SELECT COUNT(*) FROM d WHERE v = 100"), SEED_ROWS);
        // A key the merge freed is free, and one it took is taken.
        db.execute("INSERT INTO d (id, code, v, g, band) VALUES (900000, 'a6', 0, 0, 0)")
            .expect("the merge freed it");
        let err = db
            .execute("INSERT INTO d (id, code, v, g, band) VALUES (900001, 'm6', 0, 0, 0)")
            .expect_err("the merge took it");
        assert!(is_unique_violation(&err), "{err}");
        db.execute("DELETE FROM d WHERE id = 900000").unwrap();
        found
    };
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    assert_eq!(
        assert_everything_agrees(&db, "after the merge and a restart"),
        found,
        "a restart agrees"
    );
}

#[test]
fn every_writer_kind_beside_an_add_column_leaves_everything_agreeing() {
    let _watchdog = watchdog(
        "every_writer_kind_beside_an_add_column_leaves_everything_agreeing",
        LONG_TEST_AT_MOST,
    );
    let dir = tempfile::tempdir().unwrap();
    let found = {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        seed(&db, SEED_ROWS);
        every_writer_beside(&db, "ALTER TABLE d ADD COLUMN x INT DEFAULT 5");
        let found = assert_everything_agrees(&db, "after the ADD COLUMN");
        // No row was left narrower than the schema, or lost its default.
        assert_every_row_has_the_default(&db, "after the ADD COLUMN");
        assert_eq!(count(&db, "SELECT COUNT(*) FROM d WHERE x = 5"), found.len() as i64);
        for (id, _) in found.iter().step_by(found.len() / 200 + 1) {
            let by_key = rows(&db, &format!("SELECT x FROM d WHERE id = {id}"));
            assert_eq!(int(by_key[0].values.first()), 5, "row {id} by its PRIMARY KEY");
        }
        found
    };
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    assert_eq!(
        assert_everything_agrees(&db, "after the ADD COLUMN and a restart"),
        found,
        "a restart agrees"
    );
    assert_every_row_has_the_default(&db, "after the ADD COLUMN and a restart");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM d WHERE x = 5"), found.len() as i64);
}

/// The row the matrix above lost, with no race in it. The text family's DO UPDATE read the
/// row through its transaction, which resolves a committed row through its version
/// history, and an in-place rewrite writes no version: it wrote back the row as it was
/// before the ALTER. In the matrix: the text ON CONFLICT writer, waiting for the ALTER on
/// a key it had already written, one narrow row, and only if it had gone round its ten
/// keys before the ALTER started. So here on disk with versions on, as there, and each
/// way `data:` moves on without a version: ADD COLUMN, a fast UPDATE, DROP COLUMN.
#[test]
fn an_upsert_starts_from_the_row_as_it_is_not_from_its_last_version() {
    const UPSERT: &str =
        "INSERT INTO u (id, a, b, c) VALUES (1, 10, 20, 30) ON CONFLICT (id) DO UPDATE SET b = u.b + 1";
    let _watchdog = watchdog(
        "an_upsert_starts_from_the_row_as_it_is_not_from_its_last_version",
        TEST_AT_MOST,
    );
    for (family, bound) in [("text", false), ("params", true)] {
        let dir = tempfile::tempdir().unwrap();
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        let run = |sql: &str| {
            let result = if bound {
                db.execute_params(sql, &[])
            } else {
                db.execute(sql)
            };
            result.unwrap_or_else(|e| panic!("[{family}] `{sql}`: {e}"));
        };
        let b_and_c = || {
            let row = rows(&db, "SELECT b, c FROM u WHERE id = 1");
            (int(row[0].values.first()), int(row[0].values.get(1)))
        };
        db.execute("CREATE TABLE u (id INT PRIMARY KEY, a INT, b INT, c INT)")
            .unwrap();
        run(UPSERT);
        run(UPSERT);
        assert_eq!(b_and_c(), (21, 30), "[{family}]");

        db.execute("ALTER TABLE u ADD COLUMN x INT DEFAULT 5").unwrap();
        run(UPSERT);
        assert_eq!(b_and_c(), (22, 30), "[{family}] after the ADD COLUMN");
        assert_eq!(count(&db, "SELECT x FROM u WHERE id = 1"), 5, "[{family}]");
        assert_eq!(count(&db, "SELECT COUNT(*) FROM u WHERE x = 5"), 1, "[{family}]");

        db.execute("UPDATE u SET c = 7 WHERE id = 1").unwrap();
        run(UPSERT);
        assert_eq!(b_and_c(), (23, 7), "[{family}] after the UPDATE");

        db.execute("ALTER TABLE u DROP COLUMN a").unwrap();
        run("INSERT INTO u (id, b, c) VALUES (1, 0, 0) ON CONFLICT (id) DO UPDATE SET b = u.b + 1");
        assert_eq!(b_and_c(), (24, 7), "[{family}] after the DROP COLUMN");
        assert_eq!(count(&db, "SELECT x FROM u WHERE id = 1"), 5, "[{family}]");
    }

    // What this transaction staged itself still comes first.
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE u (id INT PRIMARY KEY, b INT)").unwrap();
    let session = db.create_session("upserter", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(session).unwrap();
    db.execute_for_session(session, "INSERT INTO u (id, b) VALUES (1, 1)")
        .unwrap();
    db.execute_for_session(
        session,
        "INSERT INTO u (id, b) VALUES (1, 0) ON CONFLICT (id) DO UPDATE SET b = u.b + 1",
    )
    .unwrap();
    db.commit_transaction_for_session(session).unwrap();
    assert_eq!(count(&db, "SELECT b FROM u WHERE id = 1"), 2);
}

/// Scripts t_generic.py (params, text): a multi-row UPDATE during a merge. It used to read
/// the rows, then write them back over what had landed: 686 went back to their pre-merge
/// image, and a lookup by either key found nothing.
#[test]
fn a_multi_row_update_during_a_merge_keeps_what_landed() {
    let _watchdog = watchdog("a_multi_row_update_during_a_merge_keeps_what_landed", TEST_AT_MOST);
    for (family, statement) in [("text", true), ("params", false)] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed(&db, 2000);
        branch_changing_every_row(&db);
        let (merged, updated) = a_writer_waiting_for(
            &db,
            "MERGE BRANCH r INTO main",
            MergeFailSite::TableClaimed,
            "d",
            || {
                if statement {
                    db.execute("UPDATE d SET g = g + 1 WHERE g >= 0")
                } else {
                    db.execute_params("UPDATE d SET g = g + 1 WHERE g >= $1", &[Value::Int4(0)])
                }
            },
        );
        merged.unwrap();
        assert_eq!(updated.unwrap(), 2000, "[{family}]");
        let found = assert_everything_agrees(&db, family);
        assert!(found.iter().all(|(id, code)| code == &format!("m{id}")), "[{family}]");
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM d WHERE v = 100 AND g = 1"),
            2000,
            "[{family}]"
        );
        db.execute("INSERT INTO d (id, code, v, g, band) VALUES (5000, 'a6', 0, 0, 0)")
            .expect("the old key is free");
    }
}

/// Script t_generic2.py del: a multi-row DELETE during a merge. It reported 3000 deleted
/// and 1920 came back, and the key of a truly deleted row was refused.
#[test]
fn a_multi_row_delete_during_a_merge_stays_deleted() {
    let _watchdog = watchdog("a_multi_row_delete_during_a_merge_stays_deleted", TEST_AT_MOST);
    for (family, statement) in [("text", true), ("params", false)] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed(&db, 2000);
        branch_changing_every_row(&db);
        let (merged, deleted) = a_writer_waiting_for(&db, "MERGE BRANCH r INTO main", MergeFailSite::Row, "d", || {
            if statement {
                db.execute("DELETE FROM d WHERE band = 7")
            } else {
                db.execute_params("DELETE FROM d WHERE band = $1", &[Value::Int4(7)])
            }
        });
        merged.unwrap();
        assert_eq!(deleted.unwrap(), 20, "[{family}]");
        let found = assert_everything_agrees(&db, family);
        assert_eq!(found.len(), 1980, "[{family}]");
        assert!(rows(&db, "SELECT id FROM d WHERE id = 7").is_empty(), "[{family}]");
        db.execute("INSERT INTO d (id, code, v, g, band) VALUES (5000, 'm7', 0, 0, 0)")
            .expect("the key of a row it deleted is free");
    }
}

/// Script t_generic3.py: a key-moving statement during a merge. Its undo put the pre-merge
/// keys back after the merge had removed them, so every old key got a false 23505. It
/// moves the merged keys now, and every other key is free.
#[test]
fn a_key_moving_statement_during_a_merge_leaves_no_key_behind() {
    let _watchdog = watchdog(
        "a_key_moving_statement_during_a_merge_leaves_no_key_behind",
        TEST_AT_MOST,
    );
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db, 2000);
    branch_changing_every_row(&db);
    let (merged, moved) = a_writer_waiting_for(
        &db,
        "MERGE BRANCH r INTO main",
        MergeFailSite::TableClaimed,
        "d",
        || db.execute("UPDATE d SET code = code || 'x' WHERE g >= 0"),
    );
    merged.unwrap();
    assert_eq!(moved.unwrap(), 2000);
    let found = assert_everything_agrees(&db, "after the move");
    assert!(found.iter().all(|(id, code)| code == &format!("m{id}x")));
    for (offset, free) in ["a5", "m5"].iter().enumerate() {
        db.execute(&format!(
            "INSERT INTO d (id, code, v, g, band) VALUES ({}, '{free}', 0, 0, 0)",
            5000 + offset
        ))
        .unwrap_or_else(|e| panic!("{free} is free: {e}"));
    }
    let err = db
        .execute("INSERT INTO d (id, code, v, g, band) VALUES (6000, 'm5x', 0, 0, 0)")
        .expect_err("held");
    assert!(is_unique_violation(&err), "{err}");
}

/// Scripts t_trunc.py and t_trunc2.py: a TRUNCATE during a merge, simple and extended. It
/// cleared the trees and then failed at commit, with every row still there and duplicates
/// accepted. It waits now, and empties the table the merge left.
#[test]
fn a_truncate_during_a_merge_waits_and_empties_what_landed() {
    let _watchdog = watchdog("a_truncate_during_a_merge_waits_and_empties_what_landed", TEST_AT_MOST);
    for (family, statement) in [("text", true), ("params", false)] {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = EmbeddedDatabase::new(dir.path()).unwrap();
            seed(&db, 2000);
            branch_changing_every_row(&db);
            let (merged, truncated) =
                a_writer_waiting_for(&db, "MERGE BRANCH r INTO main", MergeFailSite::Row, "d", || {
                    if statement {
                        db.execute("TRUNCATE d")
                    } else {
                        db.execute_params("TRUNCATE d", &[])
                    }
                });
            merged.unwrap();
            truncated.unwrap();
            assert!(assert_everything_agrees(&db, family).is_empty(), "[{family}]");
            for key in ["a1", "m1"] {
                db.execute(&format!(
                    "INSERT INTO d (id, code, v, g, band) VALUES (1, '{key}', 0, 0, 0)"
                ))
                .unwrap();
                db.execute("DELETE FROM d WHERE id = 1").unwrap();
            }
        }
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        assert!(
            assert_everything_agrees(&db, "after a restart").is_empty(),
            "[{family}] a restart agrees"
        );
    }
}

/// A transaction open before the rewrite starts: the rewrite waits for it, and a writer
/// arriving meanwhile waits behind the rewrite. Everything lands in that order.
#[test]
fn a_transaction_open_before_an_add_column_goes_first_and_a_later_writer_after_it() {
    let _watchdog = watchdog(
        "a_transaction_open_before_an_add_column_goes_first_and_a_later_writer_after_it",
        TEST_AT_MOST,
    );
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db, 100);
    let session = db.create_session("first", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(session).unwrap();
    db.execute_for_session(session, "UPDATE d SET g = 7 WHERE id = 1")
        .unwrap();
    std::thread::scope(|scope| {
        let altering = scope.spawn(|| db.execute("ALTER TABLE d ADD COLUMN x INT DEFAULT 5"));
        wait_for("the ALTER to wait", || db.storage.debug_table_lock_waiters("d").0 == 1);
        let later = scope.spawn(|| db.execute("UPDATE d SET g = 8 WHERE id = 2"));
        wait_for("the later writer to wait", || {
            db.storage.debug_table_lock_waiters("d").1 == 1
        });
        assert!(!altering.is_finished() && !later.is_finished());
        db.commit_transaction_for_session(session).unwrap();
        altering.join().unwrap().unwrap();
        later.join().unwrap().unwrap();
    });
    assert_eq!(count(&db, "SELECT g FROM d WHERE id = 1"), 7);
    assert_eq!(count(&db, "SELECT g FROM d WHERE id = 2"), 8);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM d WHERE x = 5"), 100);
    assert_eq!(count(&db, "SELECT x FROM d WHERE id = 2"), 5);
}

/// A held transaction makes ALTER and MERGE fail after `lock_timeout_ms`, naming what
/// holds the table, and changes nothing. The rest of the database does not notice.
#[test]
fn a_held_transaction_makes_ddl_and_a_merge_time_out_cleanly() {
    let _watchdog = watchdog(
        "a_held_transaction_makes_ddl_and_a_merge_time_out_cleanly",
        TEST_AT_MOST,
    );
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db, 10);
    branch_changing_every_row(&db);
    db.storage.debug_set_table_lock_timeout(Duration::from_millis(50));
    let session = db.create_session("holder", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(session).unwrap();
    db.execute_for_session(session, "UPDATE d SET g = 1 WHERE id = 1")
        .unwrap();

    for statement in [
        "ALTER TABLE d ADD COLUMN x INT DEFAULT 5",
        "CREATE INDEX d_g ON d (g)",
        "DROP TABLE d",
        "TRUNCATE d",
        "ALTER TABLE d RENAME TO e",
        "MERGE BRANCH r INTO main",
    ] {
        let err = db.execute(statement).expect_err(statement);
        let message = err.to_string();
        assert!(
            message.contains(TABLE_LOCK_TIMEOUT_MARKER) && message.contains("an open transaction holds it"),
            "{statement}: {message}"
        );
    }
    assert_eq!(count(&db, "SELECT COUNT(*) FROM d"), 10, "nothing changed");
    db.execute("UPDATE src SET id = 9 WHERE id = 1")
        .expect("another table does not wait");
    db.execute("UPDATE d SET g = 2 WHERE id = 2")
        .expect("another writer of the table does not wait");

    db.commit_transaction_for_session(session).unwrap();
    db.execute("MERGE BRANCH r INTO main").expect("nothing holds it now");
    db.execute("ALTER TABLE d ADD COLUMN x INT DEFAULT 5")
        .expect("nothing holds it now");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM d WHERE x = 5"), 10);
}

/// Two transactions holding two tables and each wanting the other's, behind two schema
/// statements waiting for them: a cycle nothing can break but the timeout. It breaks it:
/// everybody ends, and what is left is consistent.
#[test]
fn a_lock_cycle_with_ddl_waiting_ends_at_the_timeout() {
    let _watchdog = watchdog("a_lock_cycle_with_ddl_waiting_ends_at_the_timeout", TEST_AT_MOST);
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.storage.debug_set_table_lock_timeout(Duration::from_millis(300));
    for table in ["a", "b"] {
        db.execute(&format!("CREATE TABLE {table} (id INT PRIMARY KEY, v INT)"))
            .unwrap();
        db.execute(&format!("INSERT INTO {table} VALUES (1, 0), (2, 0)"))
            .unwrap();
    }
    let one = db.create_session("one", IsolationLevel::ReadCommitted).unwrap();
    let two = db.create_session("two", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(one).unwrap();
    db.begin_transaction_for_session(two).unwrap();
    db.execute_for_session(one, "UPDATE a SET v = 1 WHERE id = 1").unwrap();
    db.execute_for_session(two, "UPDATE b SET v = 1 WHERE id = 1").unwrap();

    let outcomes = std::thread::scope(|scope| {
        let alter_a = scope.spawn(|| db.execute("ALTER TABLE a ADD COLUMN x INT"));
        let alter_b = scope.spawn(|| db.execute("ALTER TABLE b ADD COLUMN x INT"));
        wait_for("both to wait", || {
            db.storage.debug_table_lock_waiters("a").0 == 1 && db.storage.debug_table_lock_waiters("b").0 == 1
        });
        let one_wants_b = scope.spawn(|| db.execute_for_session(one, "UPDATE b SET v = 2 WHERE id = 2"));
        let two_wants_a = scope.spawn(|| db.execute_for_session(two, "UPDATE a SET v = 2 WHERE id = 2"));
        [
            alter_a.join().unwrap(),
            alter_b.join().unwrap(),
            one_wants_b.join().unwrap(),
            two_wants_a.join().unwrap(),
        ]
    });
    let timed_out = outcomes.iter().filter(|outcome| outcome.is_err()).count();
    assert!(timed_out > 0, "somebody has to give up: {outcomes:?}");
    for outcome in &outcomes {
        if let Err(e) = outcome {
            assert!(is_lock_timeout(e), "{e}");
        }
    }
    // A failed statement aborts its transaction, and either way both end here.
    let _ = db.rollback_transaction_for_session(one);
    let _ = db.rollback_transaction_for_session(two);
    db.execute("ALTER TABLE a ADD COLUMN y INT")
        .expect("nothing holds it now");
    db.execute("ALTER TABLE b ADD COLUMN y INT")
        .expect("nothing holds it now");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM a"), 2);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM b"), 2);
}

/// A waiting schema statement holds new writers back, so it gets through under a writer
/// load that never stops.
#[test]
fn ddl_gets_through_a_continuous_writer_load() {
    let _watchdog = watchdog("ddl_gets_through_a_continuous_writer_load", TEST_AT_MOST);
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db, 1000);
    let names: Vec<String> = (0..8).map(|writer| format!("writer {writer}")).collect();
    beside_writers(
        "three ADD COLUMNs",
        &names,
        |writer, round| {
            // Not even a 55P03: nothing here holds the table for 30 s.
            db.execute_params(
                "UPDATE d SET g = g + 1 WHERE id = $1",
                &[Value::Int4((1 + (writer as i64 * 100 + round) % 1000) as i32)],
            )
            .unwrap();
            Ok(())
        },
        || {
            for column in ["x", "y", "z"] {
                db.execute(&format!("ALTER TABLE d ADD COLUMN {column} INT DEFAULT 5"))
                    .expect("a statement's wait is bounded, and new writers wait behind it");
            }
        },
    );
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM d WHERE x = 5 AND y = 5 AND z = 5"),
        1000
    );
}

/// DDL in an explicit transaction that wrote the table: the owner upgrades. It waits for
/// whoever else is there, as any DDL does, and the timeout bounds that. It used to be
/// refused at once, on the grounds that waiting would be waiting for itself, which a
/// statement that is about to finish is not.
#[test]
fn ddl_in_a_transaction_that_wrote_the_table_upgrades_and_waits() {
    let _watchdog = watchdog(
        "ddl_in_a_transaction_that_wrote_the_table_upgrades_and_waits",
        TEST_AT_MOST,
    );
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db, 10);
    let writer = db.create_session("writer", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(writer).unwrap();
    db.execute_for_session(writer, "UPDATE d SET g = 1 WHERE id = 1")
        .unwrap();
    db.execute_for_session(writer, "ALTER TABLE d ADD COLUMN x INT DEFAULT 5")
        .expect("it is alone there");
    db.commit_transaction_for_session(writer).unwrap();
    assert_eq!(count(&db, "SELECT g FROM d WHERE id = 1"), 1);

    // Somebody else is there: it waits for them, and goes when they have.
    let other = db.create_session("other", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(other).unwrap();
    db.execute_for_session(other, "UPDATE d SET g = 2 WHERE id = 2")
        .unwrap();
    db.begin_transaction_for_session(writer).unwrap();
    db.execute_for_session(writer, "UPDATE d SET g = 3 WHERE id = 3")
        .unwrap();
    std::thread::scope(|scope| {
        let altering = scope.spawn(|| db.execute_for_session(writer, "ALTER TABLE d ADD COLUMN y INT"));
        wait_for("the upgrade to wait", || {
            db.storage.debug_table_lock_waiters("d").0 == 1
        });
        assert!(!altering.is_finished(), "it waits for the other transaction");
        db.commit_transaction_for_session(other).unwrap();
        altering.join().unwrap().unwrap();
    });
    db.commit_transaction_for_session(writer).unwrap();
    assert_eq!(count(&db, "SELECT COUNT(*) FROM d WHERE y IS NULL"), 10);

    // They never go: the timeout says so, and the transaction is aborted like any other.
    db.storage.debug_set_table_lock_timeout(Duration::from_millis(50));
    db.begin_transaction_for_session(other).unwrap();
    db.execute_for_session(other, "UPDATE d SET g = 4 WHERE id = 4")
        .unwrap();
    db.begin_transaction_for_session(writer).unwrap();
    db.execute_for_session(writer, "UPDATE d SET g = 5 WHERE id = 5")
        .unwrap();
    let err = db
        .execute_for_session(writer, "ALTER TABLE d ADD COLUMN z INT")
        .expect_err("another transaction holds it");
    assert!(is_lock_timeout(&err), "{err}");
    assert!(err.to_string().contains("an open transaction holds it"), "{err}");
    db.rollback_transaction_for_session(writer).unwrap();
    db.commit_transaction_for_session(other).unwrap();
}

/// What `BEGIN; ALTER; ALTER; COMMIT` used to get beside any writer: 55P03 on the second
/// statement, at once, with the first one already persisted, so that a retry got 42701.
/// Those figures were 0 of 20 against 20 of 20 before the lock. What Alembic, Flyway and
/// Rails do by default.
#[test]
fn a_migration_in_a_transaction_goes_through_beside_a_writer() {
    let _watchdog = watchdog(
        "a_migration_in_a_transaction_goes_through_beside_a_writer",
        TEST_AT_MOST,
    );
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db, 200);
    let migrations: [&[&str]; 3] = [
        &["ALTER TABLE d ADD COLUMN a1 INT", "ALTER TABLE d ADD COLUMN a2 INT"],
        &["ALTER TABLE d ADD COLUMN a3 INT", "CREATE INDEX d_a3 ON d (a3)"],
        &["UPDATE d SET g = g + 1 WHERE id = 1", "ALTER TABLE d ADD COLUMN a4 INT"],
    ];
    beside_writers(
        "three migrations",
        &["a writer".to_string()],
        |_, round| {
            db.execute_params(
                "UPDATE d SET g = g + 1 WHERE id = $1",
                &[Value::Int4((1 + round % 200) as i32)],
            )
            .map(drop)
        },
        || {
            for statements in migrations {
                let session = db.create_session("migration", IsolationLevel::ReadCommitted).unwrap();
                db.begin_transaction_for_session(session).unwrap();
                for statement in statements {
                    db.execute_for_session(session, statement)
                        .unwrap_or_else(|e| panic!("`{statement}`: {e}"));
                }
                db.commit_transaction_for_session(session).unwrap();
                db.destroy_session(session).unwrap();
            }
        },
    );
    assert_eq!(rows(&db, "SELECT a1, a2, a3, a4 FROM d WHERE id = 1").len(), 1);
}

/// A schema statement's exclusive stays until its transaction ends, as in PostgreSQL. The
/// next statement of the migration finds the table its own, ahead of the writers.
#[test]
fn a_transaction_holds_what_its_ddl_took_until_it_ends() {
    let _watchdog = watchdog("a_transaction_holds_what_its_ddl_took_until_it_ends", TEST_AT_MOST);
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db, 10);
    let session = db.create_session("migration", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(session).unwrap();
    db.execute_for_session(session, "ALTER TABLE d ADD COLUMN x INT DEFAULT 5")
        .unwrap();
    assert!(
        db.storage.debug_table_lock_held_exclusively("d"),
        "the statement ended, not it"
    );
    std::thread::scope(|scope| {
        let writing = scope.spawn(|| db.execute("UPDATE d SET g = 1 WHERE id = 1"));
        wait_for("the writer to wait", || db.storage.debug_table_lock_waiters("d").1 == 1);
        db.execute_for_session(session, "ALTER TABLE d ADD COLUMN y INT")
            .expect("it is its own");
        assert!(!writing.is_finished(), "the writer waits for the whole migration");
        db.commit_transaction_for_session(session).unwrap();
        assert_eq!(writing.join().unwrap().unwrap(), 1);
    });
    assert!(!db.storage.debug_table_lock_held_exclusively("d"));
    assert_eq!(count(&db, "SELECT x FROM d WHERE id = 1"), 5);
}

/// `55P03` says "it changed nothing; retry it". The params family commits each child's
/// cascade by itself, and it used to reach the second child with the first one's rows gone
/// for good. On both families, whichever child is the held one, and for SET NULL too.
#[test]
fn a_cascade_that_cannot_get_a_child_changes_nothing() {
    let _watchdog = watchdog("a_cascade_that_cannot_get_a_child_changes_nothing", TEST_AT_MOST);
    for (family, bound) in [("text", false), ("params", true)] {
        for held in ["c_a", "c_b", "c_n"] {
            let db = EmbeddedDatabase::new_in_memory().unwrap();
            db.execute("CREATE TABLE pr (id INT PRIMARY KEY, v INT)").unwrap();
            for (child, action) in [("c_a", "CASCADE"), ("c_b", "CASCADE"), ("c_n", "SET NULL")] {
                db.execute(&format!(
                    "CREATE TABLE {child} (id INT PRIMARY KEY, pid INT REFERENCES pr(id) ON DELETE {action})"
                ))
                .unwrap();
            }
            db.execute("INSERT INTO pr VALUES (1, 1), (2, 2), (3, 3)").unwrap();
            for child in ["c_a", "c_b", "c_n"] {
                db.execute(&format!("INSERT INTO {child} VALUES (1, 1), (2, 2), (3, 3)"))
                    .unwrap();
            }
            let delete = |id: i32| {
                if bound {
                    db.execute_params("DELETE FROM pr WHERE id = $1", &[Value::Int4(id)])
                } else {
                    db.execute(&format!("DELETE FROM pr WHERE id = {id}"))
                }
            };
            let state = || ["pr", "c_a", "c_b"].map(|table| count(&db, &format!("SELECT COUNT(*) FROM {table}")));

            db.storage.debug_set_table_lock_timeout(Duration::from_millis(50));
            let holder = db.create_session("holder", IsolationLevel::ReadCommitted).unwrap();
            db.begin_transaction_for_session(holder).unwrap();
            db.execute_for_session(holder, &format!("TRUNCATE {held}")).unwrap();
            let err = delete(1).expect_err("a child is held");
            assert!(is_lock_timeout(&err), "[{family}, {held}] {err}");
            db.rollback_transaction_for_session(holder).unwrap();
            assert_eq!(state(), [3, 3, 3], "[{family}, {held}] it changed nothing");
            assert_eq!(
                count(&db, "SELECT COUNT(*) FROM c_n WHERE pid IS NULL"),
                0,
                "[{family}, {held}] it changed nothing"
            );

            delete(1).expect("nothing holds them now");
            assert_eq!(state(), [2, 2, 2], "[{family}, {held}] the retry did it all");
            assert_eq!(
                count(&db, "SELECT COUNT(*) FROM c_n WHERE pid IS NULL"),
                1,
                "[{family}, {held}]"
            );
        }
    }
}

/// Every embedded way to write outside a wire session: each locks at its statement and holds
/// through its commit. `execute_in_session` outside a transaction did not: the statement's
/// scope ended with the statement and its commit wrote with no lock, which only the detector
/// could see, as no wire route goes there.
#[test]
fn every_embedded_entry_point_holds_its_lock_through_its_commit() {
    let _watchdog = watchdog(
        "every_embedded_entry_point_holds_its_lock_through_its_commit",
        TEST_AT_MOST,
    );
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db, 10);
    let late_before = db.storage.debug_table_lock_late_acquisitions();
    let insert = |id: i64| format!("INSERT INTO d (id, code, v, g, band) VALUES ({id}, 'e{id}', 0, 0, 0)");

    let session = db.create_session("embedded", IsolationLevel::ReadCommitted).unwrap();
    db.execute_in_session(session, &insert(101)).unwrap();
    db.execute_in_session(session, "UPDATE d SET g = 1 WHERE id = 101")
        .unwrap();
    db.execute_in_session(session, "DELETE FROM d WHERE id = 1").unwrap();
    // A write on a query route may be refused, and that is its answer; a write with no
    // lock is the detector's panic.
    let _ = db.query_in_session(session, &format!("{} RETURNING id", insert(118)), &[]);
    db.execute_in_session(session, &insert(102)).unwrap();

    db.begin_transaction_for_session(session).unwrap();
    db.execute_in_session(session, &insert(103)).unwrap();
    db.execute_in_session(session, &insert(104)).unwrap();
    db.commit_transaction_for_session(session).unwrap();

    db.execute_for_session(session, &insert(105)).unwrap();
    db.execute_returning_for_session(session, &format!("{} RETURNING id", insert(106)))
        .unwrap();
    db.execute_params_for_session(session, INSERT_ONE_BOUND, &new_row_values(107))
        .unwrap();
    db.execute_params_returning_for_session(
        session,
        &format!("{INSERT_ONE_BOUND} RETURNING id"),
        &new_row_values(108),
    )
    .unwrap();
    db.query_params_for_session(
        session,
        &format!("{INSERT_ONE_BOUND} RETURNING id"),
        &new_row_values(109),
    )
    .unwrap();
    db.destroy_session(session).unwrap();

    #[allow(deprecated)]
    let handle = db.begin_transaction().unwrap();
    handle.execute(&insert(110)).unwrap();
    handle.execute(&insert(111)).unwrap();
    handle.commit().unwrap();
    #[allow(deprecated)]
    let handle = db.begin_transaction().unwrap();
    let _ = handle.query(&format!("{} RETURNING id", insert(119)), &[]);
    let _ = handle.commit();

    db.begin().unwrap();
    db.execute(&insert(112)).unwrap();
    db.commit().unwrap();

    db.execute_batch(&[&insert(113), &insert(114)]).unwrap();
    db.execute_returning(&format!("{} RETURNING id", insert(115))).unwrap();
    db.execute_many_params(INSERT_ONE_BOUND, &[new_row_values(116), new_row_values(117)])
        .unwrap();

    for id in 101..=117 {
        assert_eq!(
            rows(&db, &format!("SELECT id FROM d WHERE id = {id}")).len(),
            1,
            "row {id}"
        );
    }
    assert_eq!(
        db.storage.debug_table_lock_late_acquisitions(),
        late_before,
        "at the statement, not at the first staged write"
    );
}

/// PostgreSQL's spelling, on the session: SET, SET LOCAL, RESET, SHOW, with units. 0 is the
/// server's `lock_timeout_ms`, not PostgreSQL's unbounded wait: `pg_dump` scripts start with
/// it, and a wait stays bounded.
#[test]
fn set_lock_timeout_bounds_the_session_and_zero_is_the_servers() {
    let _watchdog = watchdog(
        "set_lock_timeout_bounds_the_session_and_zero_is_the_servers",
        TEST_AT_MOST,
    );
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db, 10);
    let holder = db.create_session("holder", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(holder).unwrap();
    db.execute_for_session(holder, "TRUNCATE d").unwrap();

    let session = db.create_session("waiter", IsolationLevel::ReadCommitted).unwrap();
    let set = |sql: &str| {
        db.execute_for_session(session, sql)
            .unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    };
    let show = || {
        let (found, _) = db.query_with_columns_for_session(session, "SHOW lock_timeout").unwrap();
        text(found[0].values.first())
    };
    let time_a_write = || {
        let started = Instant::now();
        let err = db
            .execute_for_session(session, "UPDATE d SET g = 1 WHERE id = 1")
            .expect_err("the table is held");
        assert!(is_lock_timeout(&err), "{err}");
        started.elapsed()
    };

    assert_eq!(show(), "0", "the server's");
    set("SET lock_timeout = '50ms'");
    assert_eq!(show(), "50ms");
    assert!(time_a_write() < Duration::from_secs(20), "not the server's 30 s");

    set("SET lock_timeout = '2s'");
    assert_eq!(show(), "2s");
    set("SET lock_timeout = 60");
    assert_eq!(show(), "60ms", "a bare number is milliseconds");
    set("SET lock_timeout_ms = 70");
    assert_eq!(show(), "70ms", "the config file's spelling is not ignored either");
    set("SET SESSION innodb_lock_wait_timeout = 3");
    assert_eq!(show(), "3s", "MySQL's spelling, in seconds");
    set("SET @@session.innodb_lock_wait_timeout = 4");
    assert_eq!(show(), "4s");
    assert!(
        db.execute_for_session(session, "SET lock_timeout = 'banana'").is_err(),
        "a value that is no duration"
    );
    // Bounded, as in PostgreSQL: INT_MAX milliseconds.
    set("SET lock_timeout = 2147483647");
    for too_long in [
        "SET lock_timeout = 2147483648",
        "SET innodb_lock_wait_timeout = 1073741824",
    ] {
        let err = db.execute_for_session(session, too_long).expect_err(too_long);
        assert!(err.to_string().contains("at most 2147483647 ms"), "{too_long}: {err}");
    }
    assert_eq!(show(), "2147483647ms", "a refused SET changes nothing");

    // 0 accepted, and bounded by the server's.
    db.storage.debug_set_table_lock_timeout(Duration::from_millis(100));
    set("SET lock_timeout = 0");
    assert_eq!(show(), "0");
    let waited = time_a_write();
    assert!(waited >= Duration::from_millis(100), "the server's: {waited:?}");

    set("RESET lock_timeout");
    assert_eq!(show(), "0");

    // SET LOCAL ends with its transaction. A failed statement ends that one here.
    db.storage.debug_set_table_lock_timeout(Duration::from_secs(60));
    db.begin_transaction_for_session(session).unwrap();
    set("SET LOCAL lock_timeout = '50ms'");
    assert_eq!(show(), "50ms");
    assert!(time_a_write() < Duration::from_secs(20));
    db.rollback_transaction_for_session(session).unwrap();
    assert_eq!(show(), "0", "SET LOCAL ended with its transaction");

    db.rollback_transaction_for_session(holder).unwrap();
}

/// A client's `statement_timeout` ends a wait for a table: 57014, and the statement does not
/// run when the lock comes free. It used to wait the whole wait and then commit, on both
/// families and the fast paths. In a transaction it aborts the transaction as any failure does.
#[test]
fn a_statement_timeout_ends_a_wait_and_nothing_commits() {
    let _watchdog = watchdog("a_statement_timeout_ends_a_wait_and_nothing_commits", TEST_AT_MOST);
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db, 10);
    let holder = db.create_session("holder", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(holder).unwrap();
    db.execute_for_session(holder, "TRUNCATE d").unwrap();

    let session = db.create_session("waiter", IsolationLevel::ReadCommitted).unwrap();
    db.execute_for_session(session, "SET statement_timeout = 50").unwrap();
    let in_time = || -> [heliosdb_nano::Result<u64>; 4] {
        [
            db.execute_for_session(
                session,
                "INSERT INTO d (id, code, v, g, band) VALUES (777, 'x777', 0, 0, 0)",
            ),
            db.execute_for_session(session, "UPDATE d SET g = 7 WHERE g >= 0"),
            db.execute_params_for_session(session, INSERT_ONE_BOUND, &new_row_values(778)),
            db.execute_params_for_session(session, "UPDATE d SET g = 7 WHERE id = $1", &[Value::Int4(1)]),
        ]
    };
    for outcome in in_time() {
        assert!(
            matches!(outcome, Err(heliosdb_nano::Error::QueryTimeout(_))),
            "{outcome:?}"
        );
    }
    db.rollback_transaction_for_session(holder).unwrap();
    assert!(
        rows(&db, "SELECT id FROM d WHERE id IN (777, 778)").is_empty(),
        "nothing ran"
    );
    assert_eq!(count(&db, "SELECT COUNT(*) FROM d WHERE g = 7"), 0, "nothing ran");
}

/// A quoted name with a colon used to lock another table: a transaction writing `"a:b"` held
/// `a` too, from the first colon of the row key. Such a name can no longer be created (42602,
/// spec B4), so only an older store can hold one; the row-key parse reads the last colon for
/// it. Here: the name is refused, and writing `a` in a transaction still locks `a` alone.
#[test]
fn a_name_with_a_colon_is_refused_and_a_writer_locks_its_own_table() {
    let _watchdog = watchdog(
        "a_name_with_a_colon_is_refused_and_a_writer_locks_its_own_table",
        TEST_AT_MOST,
    );
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE a (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE b (id INT PRIMARY KEY)").unwrap();
    let err = db
        .execute(r#"CREATE TABLE "a:b" (id INT PRIMARY KEY)"#)
        .expect_err("a name that cannot be stored");
    assert!(err.to_string().contains("cannot hold ':'"), "{err}");
    db.storage.debug_set_table_lock_timeout(Duration::from_millis(50));
    let session = db.create_session("writer", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(session).unwrap();
    db.execute_for_session(session, "INSERT INTO a VALUES (1)").unwrap();
    db.execute("ALTER TABLE b ADD COLUMN x INT")
        .expect("not the table it wrote");
    let err = db
        .execute("ALTER TABLE a ADD COLUMN x INT")
        .expect_err("the table it wrote");
    assert!(is_lock_timeout(&err), "{err}");
    db.commit_transaction_for_session(session).unwrap();
}

/// `lock_timeout_ms = 0` would make every contended writer fail at once, and `start --config`
/// never ran the check that refuses it.
#[test]
fn a_zero_lock_timeout_is_refused_when_the_config_is_loaded() {
    let err = heliosdb_nano::Config::from_toml_str("[session]\nlock_timeout_ms = 0\n").expect_err("0");
    assert!(err.to_string().contains("lock_timeout_ms"), "{err}");
    assert!(heliosdb_nano::Config::from_toml_str("[session]\nlock_timeout_ms = 1\n").is_ok());
}

/// A params upsert read the stored row and wrote it back through the storage transform, with
/// its storage references still in it: a COLUMNAR column became the literal '<columnar>' for
/// good, and RETURNING gave a DICTIONARY column's id.
#[test]
fn a_params_upsert_keeps_what_a_storage_mode_column_holds() {
    let _watchdog = watchdog("a_params_upsert_keeps_what_a_storage_mode_column_holds", TEST_AT_MOST);
    for mode in ["DICTIONARY", "CONTENT_ADDRESSED", "COLUMNAR"] {
        let dir = tempfile::tempdir().unwrap();
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        db.execute(&format!(
            "CREATE TABLE sm (id INT PRIMARY KEY, s TEXT STORAGE {mode}, v INT)"
        ))
        .unwrap();
        db.execute("INSERT INTO sm VALUES (1, 'hello', 1)").unwrap();
        let (_, returned) = db
            .execute_params_returning(
                "INSERT INTO sm VALUES ($1, $2, $3) ON CONFLICT (id) DO UPDATE SET v = sm.v + $4 RETURNING id, s, v",
                &[
                    Value::Int4(1),
                    Value::String("not this".to_string()),
                    Value::Int4(0),
                    Value::Int4(1),
                ],
            )
            .unwrap();
        assert_eq!(text(returned[0].values.get(1)), "hello", "[{mode}] RETURNING");
        assert_eq!(int(returned[0].values.get(2)), 2, "[{mode}] RETURNING");
        for sql in ["SELECT s, v FROM sm WHERE id = 1", "SELECT s, v FROM sm WHERE v >= 0"] {
            let found = rows(&db, sql);
            assert_eq!(text(found[0].values.first()), "hello", "[{mode}] {sql}");
            assert_eq!(int(found[0].values.get(1)), 2, "[{mode}] {sql}");
        }
    }
}

/// A text upsert whose conflicting row is being deleted: the index still names it and the row
/// has gone. That is an insert about to be possible, not an internal error. It used to be XX000
/// "existing row not found" about 8 times in 1000 here; the look-again is bounded, so a
/// starved thread can still see it, and this counts rather than demands none.
#[test]
fn a_text_upsert_beside_a_delete_of_its_key_does_not_find_the_row_missing() {
    let _watchdog = watchdog(
        "a_text_upsert_beside_a_delete_of_its_key_does_not_find_the_row_missing",
        TEST_AT_MOST,
    );
    const UPSERTS: usize = 3000;
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE u (id INT PRIMARY KEY, n INT)").unwrap();
    let missing = beside_writers(
        "the upserts",
        &["a deleter".to_string()],
        |_, _| db.execute("DELETE FROM u WHERE id = 1").map(drop),
        || {
            let mut missing = 0;
            for _ in 0..UPSERTS {
                let upsert =
                    db.execute("INSERT INTO u (id, n) VALUES (1, 0) ON CONFLICT (id) DO UPDATE SET n = u.n + 1");
                if upsert.is_err_and(|e| e.to_string().contains("existing row not found")) {
                    missing += 1;
                }
            }
            missing
        },
    );
    assert!(missing < 10, "{missing} of {UPSERTS} upserts found the row missing");
}

/// What a transaction holds it holds until it ends: a rollback to a savepoint gives
/// nothing back, as in PostgreSQL.
#[test]
fn a_held_lock_survives_a_rollback_to_savepoint() {
    let _watchdog = watchdog("a_held_lock_survives_a_rollback_to_savepoint", TEST_AT_MOST);
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db, 10);
    db.storage.debug_set_table_lock_timeout(Duration::from_millis(50));
    let session = db.create_session("writer", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(session).unwrap();
    db.execute_for_session(session, "SAVEPOINT s").unwrap();
    db.execute_for_session(session, "UPDATE d SET g = 1 WHERE id = 1")
        .unwrap();
    db.execute_for_session(session, "ROLLBACK TO SAVEPOINT s").unwrap();
    let err = db.execute("ALTER TABLE d ADD COLUMN x INT").expect_err("still held");
    assert!(is_lock_timeout(&err), "{err}");
    db.commit_transaction_for_session(session).unwrap();
    db.execute("ALTER TABLE d ADD COLUMN x INT").unwrap();
}

/// A schema statement on one table holds nobody up on another, nor on the same name on a
/// branch: if it did, these would time out, since the statement is stopped until they
/// are done.
#[test]
fn unrelated_tables_and_branches_never_wait() {
    let _watchdog = watchdog("unrelated_tables_and_branches_never_wait", TEST_AT_MOST);
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db, 10);
    db.execute("CREATE TABLE other (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("CREATE BRANCH r AS OF NOW").unwrap();
    db.storage.debug_set_table_lock_timeout(Duration::from_millis(2000));
    let on_branch = db.create_session("on_branch", IsolationLevel::ReadCommitted).unwrap();
    db.execute_for_session(on_branch, "USE BRANCH r").unwrap();

    let (reached, go) = stop_at_site(&db, MergeFailSite::MainDdlStatement);
    std::thread::scope(|scope| {
        let altering = scope.spawn(|| db.execute("ALTER TABLE d ADD COLUMN x INT"));
        while reached.recv_timeout(Duration::from_millis(2)).is_err() {
            assert!(!altering.is_finished(), "the ALTER ended before its guard");
        }
        assert!(db.storage.debug_table_lock_held_exclusively("d"));
        db.execute("INSERT INTO other VALUES (1, 1)").expect("another table");
        db.execute_params("INSERT INTO other VALUES ($1, $2)", &[Value::Int4(2), Value::Int4(2)])
            .expect("another table, bound");
        db.execute_for_session(
            on_branch,
            "INSERT INTO d (id, code, v, g, band) VALUES (500, 'b', 0, 0, 0)",
        )
        .expect("the same table on a branch");
        assert_eq!(rows(&db, "SELECT id FROM d WHERE id = 1").len(), 1, "a reader");
        drop(go);
        altering.join().unwrap().unwrap();
    });
    db.storage.debug_on_merge_site(MergeFailSite::MainDdlStatement, None);
}

/// A writer waits for a schema statement it is behind, and writes what the statement left:
/// no narrower row, whatever spec or plan it arrived with. The cached forms of its own
/// statement were warm, which is the point.
#[test]
fn a_writer_with_warm_caches_reads_the_schema_a_statement_it_waited_for_left() {
    let _watchdog = watchdog(
        "a_writer_with_warm_caches_reads_the_schema_a_statement_it_waited_for_left",
        TEST_AT_MOST,
    );
    for bound in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed(&db, 10);
        let update = |id: i32| {
            if bound {
                db.execute_params("UPDATE d SET g = g + 1 WHERE id = $1", &[Value::Int4(id)])
            } else {
                db.execute(&format!("UPDATE d SET g = g + 1 WHERE id = {id}"))
            }
        };
        update(1).unwrap();
        update(1).unwrap();
        let (altered, updated) = a_writer_waiting_for(
            &db,
            "ALTER TABLE d DROP COLUMN v",
            MergeFailSite::MainDdlStatement,
            "d",
            || update(1),
        );
        altered.unwrap();
        assert_eq!(updated.unwrap(), 1, "bound: {bound}");
        assert_eq!(count(&db, "SELECT g FROM d WHERE id = 1"), 3, "bound: {bound}");
        assert_eq!(count(&db, "SELECT band FROM d WHERE id = 1"), 1, "bound: {bound}");
    }
}

/// `COPY FROM`, which reaches the engine from the wire handler with no statement above it,
/// takes the lock too. The waiting is off the async workers: with two of them, the server
/// keeps answering other connections while it waits. 55P03 comes through as itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn copy_and_the_wire_wait_off_the_workers_and_time_out_as_55p03() {
    let _watchdog = watchdog(
        "copy_and_the_wire_wait_off_the_workers_and_time_out_as_55p03",
        TEST_AT_MOST,
    );
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db, 10);
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let db = Arc::new(db);
    let server = PgServer::new(
        PgServerConfig::with_address(addr).with_max_connections(16),
        Arc::clone(&db),
    )
    .unwrap();
    let listener = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let connect_string = format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port());

    let (reached, go) = stop_at_site(&db, MergeFailSite::MainDdlStatement);
    let altering = {
        let db = Arc::clone(&db);
        std::thread::spawn(move || db.execute("ALTER TABLE d ADD COLUMN x INT DEFAULT 5"))
    };
    tokio::task::spawn_blocking(move || reached.recv_timeout(Duration::from_secs(60)))
        .await
        .unwrap()
        .expect("the ALTER reaches its guard");

    let copying = tokio::spawn(async move {
        let mut wire = raw_wire::connect(addr.port()).await;
        raw_wire::query(
            &mut wire,
            "COPY d (id, code, v, g, band) FROM STDIN",
            &["100\tc100\t0\t0\t0\n"],
        )
        .await
    });
    let waiting = Arc::clone(&db);
    tokio::task::spawn_blocking(move || {
        wait_for("the COPY to wait", || {
            waiting.storage.debug_table_lock_waiters("d").1 > 0
        });
    })
    .await
    .unwrap();

    // Every worker of a wire statement is free meanwhile.
    let (client, connection) = tokio_postgres::connect(&connect_string, NoTls).await.unwrap();
    let connection = tokio::spawn(async move {
        let _ = connection.await;
    });
    client.simple_query("SELECT 1").await.unwrap();

    drop(go);
    tokio::task::spawn_blocking(move || altering.join().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(copying.await.unwrap(), None, "the COPY went through after it");
    db.storage.debug_on_merge_site(MergeFailSite::MainDdlStatement, None);
    let row = client.query_one("SELECT x FROM d WHERE id = 100", &[]).await.unwrap();
    assert_eq!(row.get::<_, i32>(0), 5, "written under the schema it waited for");

    // 55P03, by SQLSTATE, on a client.
    db.storage.debug_set_table_lock_timeout(Duration::from_millis(50));
    client.simple_query("BEGIN").await.unwrap();
    client.simple_query("UPDATE d SET g = 1 WHERE id = 1").await.unwrap();
    let (ddl, ddl_connection) = tokio_postgres::connect(&connect_string, NoTls).await.unwrap();
    let ddl_connection = tokio::spawn(async move {
        let _ = ddl_connection.await;
    });
    let err = ddl
        .simple_query("ALTER TABLE d ADD COLUMN y INT")
        .await
        .err()
        .expect("an open transaction holds it");
    let code = err.as_db_error().map(|db_error| db_error.code().code().to_string());
    assert_eq!(code.as_deref(), Some("55P03"), "{err:?}");
    client.simple_query("ROLLBACK").await.unwrap();

    // COPY's too: a client retrying on the SQLSTATE has to recognise it there as well.
    client.simple_query("BEGIN").await.unwrap();
    client.simple_query("TRUNCATE d").await.unwrap();
    let mut wire = raw_wire::connect(addr.port()).await;
    let copy_error = raw_wire::query(
        &mut wire,
        "COPY d (id, code, v, g, band) FROM STDIN",
        &["101\tc101\t0\t0\t0\n"],
    )
    .await
    .expect("the table is held");
    assert!(copy_error.contains("C55P03"), "{copy_error}");
    client.simple_query("ROLLBACK").await.unwrap();

    drop(ddl);
    drop(client);
    ddl_connection.abort();
    connection.abort();
    listener.abort();
}

/// A CancelRequest reaches a statement waiting for a table: 57014, and it does not run when the
/// lock comes free. The server used to ignore it: nothing routed the request at all, and the
/// statement committed for a client that had given up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_request_ends_a_wait_and_nothing_commits() {
    let _watchdog = watchdog("a_cancel_request_ends_a_wait_and_nothing_commits", TEST_AT_MOST);
    a_cancel_ends_a_wait(16).await;
}

/// The same with every connection slot taken, by the holder and the waiter: PostgreSQL serves a
/// cancel request whatever the connection count, and a full server is when one is needed. It
/// used to be dropped unread with the connection it came on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_request_gets_in_when_every_connection_is_taken() {
    let _watchdog = watchdog("a_cancel_request_gets_in_when_every_connection_is_taken", TEST_AT_MOST);
    a_cancel_ends_a_wait(2).await;
}

async fn a_cancel_ends_a_wait(max_connections: usize) {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db, 10);
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let db = Arc::new(db);
    let server = PgServer::new(
        PgServerConfig::with_address(addr).with_max_connections(max_connections),
        Arc::clone(&db),
    )
    .unwrap();
    let listener = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let connect_string = format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port());

    let (holder, holder_connection) = tokio_postgres::connect(&connect_string, NoTls).await.unwrap();
    let holder_connection = tokio::spawn(async move {
        let _ = holder_connection.await;
    });
    holder.simple_query("BEGIN").await.unwrap();
    holder.simple_query("TRUNCATE d").await.unwrap();

    let (waiter, waiter_connection) = tokio_postgres::connect(&connect_string, NoTls).await.unwrap();
    let waiter_connection = tokio::spawn(async move {
        let _ = waiter_connection.await;
    });
    let cancel = waiter.cancel_token();
    let waiting = tokio::spawn(async move {
        waiter
            .simple_query("INSERT INTO d (id, code, v, g, band) VALUES (777, 'x777', 0, 0, 0)")
            .await
            .map(drop)
    });
    let watching = Arc::clone(&db);
    tokio::task::spawn_blocking(move || {
        wait_for("the INSERT to wait", || {
            watching.storage.debug_table_lock_waiters("d").1 > 0
        });
    })
    .await
    .unwrap();

    // A pid with no backend behind it does nothing.
    assert!(!heliosdb_nano::session::scoped::request_cancel(i32::MAX, 0));

    cancel.cancel_query(NoTls).await.unwrap();
    let err = waiting.await.unwrap().expect_err("cancelled");
    let code = err.as_db_error().map(|db_error| db_error.code().code().to_string());
    assert_eq!(code.as_deref(), Some("57014"), "{err:?}");

    holder.simple_query("ROLLBACK").await.unwrap();
    assert!(rows(&db, "SELECT id FROM d WHERE id = 777").is_empty(), "nothing ran");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM d"), 10);

    drop(holder);
    holder_connection.abort();
    waiter_connection.abort();
    listener.abort();
}

/// A function body that writes runs statements of its own. After the transaction's own `ALTER`,
/// `CREATE INDEX` or `TRUNCATE` they waited for that transaction, the whole lock timeout, and
/// then 55P03 with the `ALTER` persisted. With a writer queued behind the transaction it was a
/// cycle the timeout ended. The body runs for the transaction now. (Its writes still commit by
/// themselves, as they did: a separate, older item.)
#[test]
fn a_writing_function_in_a_transaction_does_not_wait_for_it() {
    let _watchdog = watchdog("a_writing_function_in_a_transaction_does_not_wait_for_it", TEST_AT_MOST);
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 1)").unwrap();
    db.execute(
        "CREATE FUNCTION fins(x INT) RETURNS INT AS $$ INSERT INTO t VALUES ($1, 0) RETURNING 1 $$ LANGUAGE sql",
    )
    .unwrap();
    db.storage.debug_set_table_lock_timeout(Duration::from_secs(20));
    let session = db.create_session("migration", IsolationLevel::ReadCommitted).unwrap();
    let call = |id: i64| {
        let started = Instant::now();
        db.query_with_columns_for_session(session, &format!("SELECT fins({id})"))
            .unwrap_or_else(|e| panic!("fins({id}): {e}"));
        started.elapsed()
    };

    // A new id each time: nothing may answer the call from a cache.
    for (id, first) in [
        (21, "ALTER TABLE t ADD COLUMN x INT"),
        (22, "CREATE INDEX t_v ON t (v)"),
        (23, "TRUNCATE t"),
        (24, "INSERT INTO t VALUES (2, 2)"),
    ] {
        db.begin_transaction_for_session(session).unwrap();
        db.execute_for_session(session, first).unwrap();
        let took = call(id);
        assert!(
            took < Duration::from_secs(10),
            "after `{first}`: it waited {took:?} for its own transaction"
        );
        db.commit_transaction_for_session(session).unwrap();
        assert_eq!(
            rows(&db, &format!("SELECT id FROM t WHERE id = {id}")).len(),
            1,
            "after `{first}`"
        );
    }

    // A schema statement queued behind the transaction: the body's writes went in behind it, and
    // it waited for the transaction, which waited for them.
    db.begin_transaction_for_session(session).unwrap();
    db.execute_for_session(session, "INSERT INTO t VALUES (3, 3)").unwrap();
    std::thread::scope(|scope| {
        let altering = scope.spawn(|| db.execute("ALTER TABLE t ADD COLUMN y INT"));
        wait_for("the ALTER to wait", || db.storage.debug_table_lock_waiters("t").0 == 1);
        let took = call(13);
        assert!(took < Duration::from_secs(10), "behind a queued ALTER: {took:?}");
        db.commit_transaction_for_session(session).unwrap();
        altering.join().unwrap().unwrap();
    });
    assert_eq!(rows(&db, "SELECT id FROM t WHERE id IN (3, 13)").len(), 2);
}

/// The detector: in a test build a row written with no lock on its table is a missed
/// path, named. Below every statement funnel, on purpose.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "R5.1 writer lock: a row of table nobody")]
fn the_detector_names_a_row_write_with_no_lock() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.storage.put(&b"data:nobody:1".to_vec(), b"row").unwrap();
}

/// And it looks at batches, and at a branch's rows and at versions.
#[cfg(debug_assertions)]
#[test]
fn the_detector_knows_the_row_keys_of_every_family_and_only_those() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.storage.put(&b"meta:not_a_row".to_vec(), b"x").unwrap();
    for key in ["v:nobody:1:900", "bdata:7:nobody:1"] {
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            db.storage.put(&key.as_bytes().to_vec(), b"row").unwrap();
        }));
        assert!(unwound.is_err(), "{key} is a row");
    }
}

/// The smallest PostgreSQL wire client that can send COPY data.
mod raw_wire {
    use bytes::{BufMut, BytesMut};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    fn put_cstr(buf: &mut BytesMut, value: &str) {
        buf.extend_from_slice(value.as_bytes());
        buf.put_u8(0);
    }

    fn message(tag: u8, body: &[u8]) -> BytesMut {
        let mut msg = BytesMut::new();
        msg.put_u8(tag);
        msg.put_i32((body.len() + 4) as i32);
        msg.extend_from_slice(body);
        msg
    }

    async fn read_frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
        let mut header = [0u8; 5];
        stream.read_exact(&mut header).await.unwrap();
        let len = i32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut body = vec![0u8; len - 4];
        stream.read_exact(&mut body).await.unwrap();
        (header[0], body)
    }

    pub async fn connect(port: u16) -> TcpStream {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut body = BytesMut::new();
        body.put_i32(196_608);
        put_cstr(&mut body, "user");
        put_cstr(&mut body, "postgres");
        put_cstr(&mut body, "database");
        put_cstr(&mut body, "postgres");
        body.put_u8(0);
        let mut startup = BytesMut::new();
        startup.put_i32((body.len() + 4) as i32);
        startup.extend_from_slice(&body);
        stream.write_all(&startup).await.unwrap();
        while read_frame(&mut stream).await.0 != b'Z' {}
        stream
    }

    /// Run `sql` as one `Q`; a CopyInResponse is answered with `copy_rows` and
    /// CopyDone. The ErrorResponse's message, if any.
    pub async fn query(stream: &mut TcpStream, sql: &str, copy_rows: &[&str]) -> Option<String> {
        let mut body = BytesMut::new();
        put_cstr(&mut body, sql);
        stream.write_all(&message(b'Q', &body)).await.unwrap();
        let mut error = None;
        loop {
            let (tag, body) = read_frame(stream).await;
            match tag {
                b'G' => {
                    let mut out = BytesMut::new();
                    for row in copy_rows {
                        out.extend_from_slice(&message(b'd', row.as_bytes()));
                    }
                    out.extend_from_slice(&message(b'c', &[]));
                    stream.write_all(&out).await.unwrap();
                }
                b'E' => error = Some(String::from_utf8_lossy(&body).to_string()),
                b'Z' => return error,
                _ => {}
            }
        }
    }
}
