//! Spec R5.6: layout epochs. A statement reads its catalog and its rows under one view,
//! on a primary and on a physical standby (`storage::layout_epochs`).
//!
//! # Red before R5.6
//!
//! * `a_plan_cached_before_a_volatile_add_column_is_not_served_while_a_standby_publishes_it`:
//!   the merged-tree review's M3, made deterministic. A plan of
//!   `SELECT count(*), count(u)` admitted before the column existed (an aggregate's
//!   argument is not resolved at plan time) stayed in the standby's plan cache until the
//!   END of the run that added the column, while the new schema was readable from the
//!   run's `publish_meta_tables` on: run over the new layout it decoded no column of it
//!   and answered `(N, 0)`. The probe reads in the middle of that publication.
//! * `standby_readers_beside_catalog_changes_never_see_a_torn_answer`: the same, as the
//!   review measured it, with readers looping on another thread.
//! * `a_statement_whose_table_is_changing_retries_then_is_refused_40001` and
//!   `a_statement_that_took_a_sequence_value_is_refused_at_once`: there was no epoch.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::storage::layout_epochs::{self, is_layout_changed, LAYOUT_CHANGED_MARKER};
use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

fn int(value: &Value) -> i64 {
    match value {
        Value::Int2(n) => i64::from(*n),
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        Value::Numeric(text) => text.parse().unwrap_or_else(|_| panic!("not an integer: {text}")),
        #[allow(clippy::cast_possible_truncation)]
        Value::Float8(f) => *f as i64,
        Value::Null => 0,
        other => panic!("not an integer: {other:?}"),
    }
}

/// The first row's integers.
fn ints(rows: &[Tuple]) -> Vec<i64> {
    rows.first()
        .map(|row| row.values.iter().map(int).collect())
        .unwrap_or_default()
}

/// Hold a change of `table`'s layout in flight from another thread, as a schema
/// statement's write and publication do, until the returned sender is dropped or sent to.
fn hold_change(db: &EmbeddedDatabase, table: &'static str) -> (mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let epochs = Arc::clone(db.storage.layout_epochs());
    let (begun_tx, begun_rx) = mpsc::channel();
    let (end_tx, end_rx) = mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let change = epochs.debug_begin_change(&[table]);
        begun_tx.send(()).unwrap();
        let _ = end_rx.recv();
        drop(change);
    });
    begun_rx.recv().unwrap();
    (end_tx, holder)
}

// ---------------------------------------------------------------------------
// Retry, refusal, side effects (decisions 2(f), (g))
// ---------------------------------------------------------------------------

#[test]
fn a_statement_whose_table_is_changing_retries_then_is_refused_40001() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE r (id INT PRIMARY KEY, a INT)").unwrap();
    db.execute("INSERT INTO r VALUES (1, 10), (2, 20)").unwrap();
    let retries = layout_epochs::retries();
    let refusals = layout_epochs::refusals();

    let (end, holder) = hold_change(&db, "r");
    let started = Instant::now();
    let error = db.query("SELECT count(*), sum(a) FROM r", &[]).unwrap_err();
    let waited = started.elapsed();
    assert!(is_layout_changed(&error), "{error}");
    assert!(error.to_string().contains(LAYOUT_CHANGED_MARKER), "{error}");
    assert!(
        waited >= Duration::from_millis(90) && waited < Duration::from_secs(5),
        "retried for the 100 ms budget, then refused: {waited:?}"
    );
    assert!(layout_epochs::retries() > retries, "it was retried");
    assert!(layout_epochs::refusals() > refusals, "then refused");

    end.send(()).unwrap();
    holder.join().unwrap();
    let rows = db.query("SELECT count(*), sum(a) FROM r", &[]).unwrap();
    assert_eq!(ints(&rows), vec![2, 30], "answered once the change ended");
}

#[test]
fn a_change_that_ends_inside_the_budget_is_waited_out() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE w (id INT PRIMARY KEY, a INT)").unwrap();
    db.execute("INSERT INTO w VALUES (1, 10)").unwrap();
    let (end, holder) = hold_change(&db, "w");
    let ender = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        end.send(()).unwrap();
    });
    let rows = db.query("SELECT count(*), sum(a) FROM w", &[]).unwrap();
    assert_eq!(ints(&rows), vec![1, 10]);
    ender.join().unwrap();
    holder.join().unwrap();
}

#[test]
fn a_statement_that_took_a_sequence_value_is_refused_at_once() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE SEQUENCE layout_seq").unwrap();
    db.execute("CREATE TABLE s (id INT PRIMARY KEY, a INT)").unwrap();
    db.execute("INSERT INTO s VALUES (1, 10)").unwrap();

    let (end, holder) = hold_change(&db, "s");
    let started = Instant::now();
    let error = db
        .query("SELECT nextval('layout_seq') AS n, a FROM s WHERE id = 1", &[])
        .unwrap_err();
    let waited = started.elapsed();
    end.send(()).unwrap();
    holder.join().unwrap();

    assert!(is_layout_changed(&error), "{error}");
    assert!(
        waited < Duration::from_millis(90),
        "no retry for a statement that advanced a sequence: {waited:?}"
    );
    let rows = db.query("SELECT nextval('layout_seq')", &[]).unwrap();
    assert_eq!(ints(&rows), vec![2], "the refused statement took exactly one value");
}

/// A durable setval is not the SELECT's durable row effect. This exercises its
/// real on-disk write inside a statement that already resolved a table, then
/// moves that layout before the outer statement validates the result. Counting
/// the sequence write as the first durable effect used to skip the final check.
#[test]
fn a_durable_setval_does_not_skip_the_statements_final_layout_validation() {
    let dir = tempfile::tempdir().unwrap();
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    db.execute("CREATE TABLE s (id INT PRIMARY KEY, a INT)").unwrap();
    db.execute("CREATE SEQUENCE layout_set_seq").unwrap();
    let scope = heliosdb_nano::storage::StatementScope::enter();
    db.storage.catalog().get_table_schema("s").unwrap();
    let rows = db.query("SELECT setval('layout_set_seq', 77, false)", &[]).unwrap();
    assert_eq!(ints(&rows), vec![77]);

    let (end, holder) = hold_change(&db, "s");
    let error = scope.finish().unwrap_err();
    assert!(is_layout_changed(&error), "{error}");
    assert!(!scope.retriable(), "setval cannot be repeated on a layout retry");
    end.send(()).unwrap();
    holder.join().unwrap();
    drop(scope);

    let rows = db.query("SELECT nextval('layout_set_seq')", &[]).unwrap();
    assert_eq!(ints(&rows), vec![77], "the refused statement kept setval's effect");
}

#[test]
fn a_write_that_read_a_changing_layout_is_refused_before_it_reaches_the_store() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE wf (id INT PRIMARY KEY, a INT)").unwrap();
    db.execute("INSERT INTO wf VALUES (1, 1), (2, 2)").unwrap();
    // The statement resolves `wf` while its layout is changing: the write funnel
    // refuses its write (nothing reaches the store), it is run again inside the
    // budget, then refused.
    let (end, holder) = hold_change(&db, "wf");
    let error = db.execute("INSERT INTO wf SELECT id + 100, a FROM wf").unwrap_err();
    end.send(()).unwrap();
    holder.join().unwrap();
    assert!(is_layout_changed(&error), "{error}");
    let rows = db.query("SELECT count(*), sum(a) FROM wf", &[]).unwrap();
    assert_eq!(ints(&rows), vec![2, 3], "no row of the refused statement was written");
    db.execute("INSERT INTO wf SELECT id + 100, a FROM wf").unwrap();
    let rows = db.query("SELECT count(*), sum(a) FROM wf", &[]).unwrap();
    assert_eq!(ints(&rows), vec![4, 6]);
}

/// A prepared statement's plan of an older layout is planned again from its text (the
/// scan refuses to run it), through both `PREPARE` paths; a changed result type is
/// refused, as PostgreSQL refuses it.
#[test]
fn a_prepared_statement_is_planned_again_after_a_schema_change_of_its_table() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE ps (id INT PRIMARY KEY, a INT)").unwrap();
    db.execute("INSERT INTO ps VALUES (1, 10), (2, 20)").unwrap();
    // The query family keeps the text it split off; the execute family's PREPARE is
    // planned by the parser, which renders the text.
    db.query("PREPARE by_query AS SELECT count(*), sum(a) FROM ps WHERE a > $1", &[])
        .unwrap();
    db.execute("PREPARE by_execute AS SELECT count(*), sum(a) FROM ps WHERE a > $1")
        .unwrap();
    db.query("PREPARE every_column AS SELECT * FROM ps WHERE a > $1", &[])
        .unwrap();
    db.execute("ALTER TABLE ps ADD COLUMN u UUID DEFAULT gen_random_uuid()")
        .unwrap();
    for name in ["by_query", "by_execute"] {
        let rows = db.query(&format!("EXECUTE {name}(0)"), &[]).unwrap();
        assert_eq!(ints(&rows), vec![2, 30], "{name}");
    }
    let error = db.query("EXECUTE every_column(0)", &[]).unwrap_err();
    assert!(
        error.to_string().contains("cached plan must not change result type"),
        "{error}"
    );
}

#[test]
fn every_layout_key_family_moves_its_tables_epoch() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let epochs = Arc::clone(db.storage.layout_epochs());
    let last = std::cell::Cell::new(epochs.current("k"));
    let moved = |label: &str| {
        let now = epochs.current("k");
        let before = last.get();
        assert!(now > before, "{label}: the epoch of k did not move ({before} -> {now})");
        assert!(!epochs.in_flight("k"), "{label}: left in flight");
        last.set(now);
    };
    db.execute("CREATE TABLE k (id INT PRIMARY KEY, a INT, b INT)").unwrap();
    moved("CREATE TABLE");
    db.execute("INSERT INTO k VALUES (1, 1, 1), (2, 2, 2)").unwrap();
    assert_eq!(epochs.current("k"), last.get(), "rows do not move a layout");
    db.execute("ALTER TABLE k ADD COLUMN c INT DEFAULT 7").unwrap();
    moved("ADD COLUMN, stable");
    db.execute("ALTER TABLE k ADD COLUMN d UUID DEFAULT gen_random_uuid()")
        .unwrap();
    moved("ADD COLUMN, volatile");
    db.execute("ALTER TABLE k RENAME COLUMN b TO bb").unwrap();
    moved("RENAME COLUMN");
    db.execute("CREATE INDEX k_a ON k (a)").unwrap();
    moved("CREATE INDEX");
    db.execute("DROP INDEX k_a").unwrap();
    moved("DROP INDEX");
    db.execute("ALTER TABLE k DROP COLUMN bb").unwrap();
    moved("DROP COLUMN");
    db.execute("DROP TABLE k").unwrap();
    moved("DROP TABLE");
}

/// The inner UNION drains its inputs while the outer UNION is constructed, so
/// nextval runs before construction reaches the stale scan of p_effect_rows.
/// Replanning at that point must refuse instead of executing that effect twice.
#[test]
fn replanning_a_prepared_union_never_repeats_a_sequence_effect() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE SEQUENCE prepared_effect_seq").unwrap();
    db.execute("CREATE TABLE p_effect_rows (id BIGINT PRIMARY KEY)")
        .unwrap();
    db.execute("INSERT INTO p_effect_rows VALUES (10)").unwrap();
    db.execute(
        "PREPARE p_effect AS SELECT nextval('prepared_effect_seq') AS n \
         UNION ALL SELECT 0 UNION ALL SELECT id FROM p_effect_rows",
    )
    .unwrap();
    db.execute("ALTER TABLE p_effect_rows ADD COLUMN unused INT").unwrap();

    let error = db.query("EXECUTE p_effect", &[]).unwrap_err();
    assert!(is_layout_changed(&error), "{error}");
    let rows = db.query("SELECT nextval('prepared_effect_seq')", &[]).unwrap();
    assert_eq!(ints(&rows), vec![2], "the refused EXECUTE advanced its sequence once");
}

// ---------------------------------------------------------------------------
// Readers beside catalog changes, on a primary
// ---------------------------------------------------------------------------

/// The only refusals a reader may see while these tests deliberately remove a
/// table or column. Layout serialization failures are also expected; storage,
/// timeout and every other query error are test failures.
#[derive(Clone, Copy)]
struct SchemaRaces {
    tables: &'static [&'static str],
    columns: &'static [&'static str],
}

impl SchemaRaces {
    fn allows(self, error: &heliosdb_nano::Error) -> bool {
        if is_layout_changed(error) {
            return true;
        }
        let heliosdb_nano::Error::QueryExecution(message) = error else {
            return false;
        };
        self.tables
            .iter()
            .any(|table| message == &format!("Table '{table}' does not exist"))
            || self.columns.iter().any(|column| {
                message == &format!("Column '{column}' not found in schema")
                    || message == &format!("Column '{column}' not found in aggregate input schema")
                    || message == &format!("Column \"{column}\" does not exist")
            })
    }
}

/// A continuously running reader with a start handshake and an observation count
/// for EACH query. A phase waits for a fresh nonempty answer before moving to the
/// next layout, so errors alone (or an unscheduled reader) cannot pass the test.
struct ConcurrentReader {
    counts: Arc<Vec<AtomicUsize>>,
    failed: Arc<std::sync::Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ConcurrentReader {
    fn start(
        db: Arc<EmbeddedDatabase>,
        queries: Vec<&'static str>,
        check: fn(&[Tuple]) -> Result<(), String>,
        allowed: SchemaRaces,
    ) -> Self {
        let counts = Arc::new(queries.iter().map(|_| AtomicUsize::new(0)).collect::<Vec<_>>());
        let failed = Arc::new(std::sync::Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::sync_channel(0);
        let handle = {
            let counts = Arc::clone(&counts);
            let failed = Arc::clone(&failed);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                ready_tx.send(()).unwrap();
                while !stop.load(Ordering::Acquire) {
                    for (index, query) in queries.iter().enumerate() {
                        let failure = match db.query(query, &[]) {
                            Ok(rows) => match check(&rows) {
                                Ok(()) => {
                                    // Empty SELECT results are legitimate between CREATE
                                    // and INSERT; they do not establish the reader saw rows.
                                    if !rows.is_empty() {
                                        counts[index].fetch_add(1, Ordering::Release);
                                    }
                                    None
                                }
                                Err(torn) => Some(format!("`{query}` answered a torn state: {torn}")),
                            },
                            Err(error) if allowed.allows(&error) => None,
                            Err(error) => Some(format!("`{query}` raised an unexpected error: {error}")),
                        };
                        if let Some(failure) = failure {
                            *failed.lock().unwrap() = Some(failure);
                            return;
                        }
                    }
                }
            })
        };
        let reader = Self {
            counts,
            failed,
            stop,
            handle: Some(handle),
        };
        ready_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the reader started");
        reader
    }

    /// After the caller's current layout has been published, every query must
    /// succeed twice while the writer remains active and before it changes that
    /// layout: at most one query was already in flight, so at least one began
    /// after publication. A final read after the writer stopped cannot satisfy it.
    fn observe_current_layout(&self) {
        let before: Vec<usize> = self.counts.iter().map(|count| count.load(Ordering::Acquire)).collect();
        let started = Instant::now();
        loop {
            if let Some(error) = self.failed.lock().unwrap().as_ref() {
                panic!("{error}");
            }
            if self
                .counts
                .iter()
                .zip(&before)
                .all(|(count, before)| count.load(Ordering::Acquire) >= before.saturating_add(2))
            {
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the concurrent reader made no successful observation of each query in this layout"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn finish(mut self) -> Vec<usize> {
        self.stop.store(true, Ordering::Release);
        self.handle.take().unwrap().join().unwrap();
        if let Some(error) = self.failed.lock().unwrap().as_ref() {
            panic!("{error}");
        }
        let counts: Vec<usize> = self.counts.iter().map(|count| count.load(Ordering::Acquire)).collect();
        assert!(
            counts.iter().all(|count| *count > 0),
            "each query observed rows: {counts:?}"
        );
        eprintln!("LAYOUT-READER successful nonempty answers per query: {counts:?}");
        counts
    }
}

impl Drop for ConcurrentReader {
    fn drop(&mut self) {
        // Also stops the thread when a DDL assertion or observation fails.
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Each valid layout calls `observe_current_layout` before it is removed. Refusals
/// are restricted to the table/columns the workload intentionally changes.
fn read_beside(
    db: &Arc<EmbeddedDatabase>,
    query: &'static str,
    check: fn(&[Tuple]) -> Result<(), String>,
    allowed: SchemaRaces,
    rounds: usize,
    mut ddl: impl FnMut(usize, &ConcurrentReader),
) {
    let reader = ConcurrentReader::start(Arc::clone(db), vec![query], check, allowed);
    for round in 0..rounds {
        ddl(round, &reader);
    }
    reader.finish();
}

#[test]
fn only_the_workloads_expected_schema_errors_are_ignored() {
    use heliosdb_nano::Error;
    let allowed = SchemaRaces {
        tables: &["tv"],
        columns: &["u"],
    };
    assert!(allowed.allows(&Error::query_execution("Table 'tv' does not exist")));
    assert!(allowed.allows(&Error::query_execution("Column 'u' not found in schema")));
    assert!(allowed.allows(&Error::query_execution(
        "Column 'u' not found in aggregate input schema"
    )));
    assert!(allowed.allows(&layout_epochs::layout_changed("tv")));
    for error in [
        Error::storage("Table 'tv' does not exist"),
        Error::query_execution("Table 'another_table' does not exist"),
        Error::query_execution("Column 'other' not found in schema"),
        Error::query_execution("Column 'other' not found in aggregate input schema"),
        Error::storage("Column 'u' not found in aggregate input schema"),
        Error::query_execution("Column index 3 out of bounds in tuple"),
        Error::query_timeout("reader stalled"),
    ] {
        assert!(
            !allowed.allows(&error),
            "unexpected error must fail the reader: {error}"
        );
    }
}

const ROWS: i64 = 300;

const CASE_COUNT_U: &str = "SELECT count(*), sum(CASE WHEN u IS NULL THEN 0 ELSE 1 END) FROM tv";

#[test]
fn aggregate_arguments_resolve_unknown_columns_before_empty_input_or_side_effects() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE tv (id INT PRIMARY KEY, a INT)").unwrap();
    db.execute("CREATE SEQUENCE empty_aggregate_seq").unwrap();
    // Streamed, DISTINCT/materialized, and nested CASE/scalar arguments all
    // bind against the input schema, independently of input cardinality.
    for query in [
        "SELECT count(u) FROM tv",
        CASE_COUNT_U,
        "SELECT count(DISTINCT CASE WHEN u IS NULL THEN a ELSE 1 END) FROM tv",
        "SELECT sum(CASE WHEN u IS NULL THEN nextval('empty_aggregate_seq') ELSE 1 END) FROM tv",
    ] {
        let error = db.query(query, &[]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Column 'u' not found in aggregate input schema"),
            "{query}: {error}"
        );
    }
    assert_eq!(
        ints(&db.query("SELECT nextval('empty_aggregate_seq')", &[]).unwrap()),
        vec![1],
        "column validation cannot evaluate a sequence"
    );
    db.execute("ALTER TABLE tv ADD COLUMN u INT DEFAULT 7").unwrap();
    let empty = db.query(CASE_COUNT_U, &[]).unwrap();
    assert_eq!(empty.len(), 1);
    assert_eq!(int(&empty[0].values[0]), 0);
    assert_eq!(empty[0].values[1], Value::Null, "a valid empty SUM remains NULL");
    insert_rows_with_u(&db);
    assert_eq!(every_row_has_u(&db.query(CASE_COUNT_U, &[]).unwrap()), Ok(()));
}

fn insert_rows_with_u(db: &EmbeddedDatabase) {
    let values: Vec<String> = (1..=ROWS).map(|id| format!("({id}, {}, 7)", 10 * id)).collect();
    db.execute(&format!("INSERT INTO tv VALUES {}", values.join(", ")))
        .unwrap();
}

fn insert_rows(db: &EmbeddedDatabase, table: &str) {
    let values: Vec<String> = (1..=ROWS).map(|id| format!("({id}, {})", 10 * id)).collect();
    db.execute(&format!("INSERT INTO {table} VALUES {}", values.join(", ")))
        .unwrap();
}

/// `count(*), count(u)`: once `u` exists, every row has it.
fn every_row_has_u(rows: &[Tuple]) -> Result<(), String> {
    let counts = ints(rows);
    if counts == vec![ROWS, ROWS] {
        Ok(())
    } else {
        Err(format!("{counts:?}"))
    }
}

#[test]
fn readers_beside_a_volatile_and_a_stable_add_column_never_see_the_column_null() {
    for column in ["u UUID DEFAULT gen_random_uuid()", "u TEXT DEFAULT 'x'"] {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        read_beside(
            &db,
            "SELECT count(*), count(u) FROM tv",
            every_row_has_u,
            SchemaRaces {
                tables: &["tv"],
                columns: &["u"],
            },
            15,
            |_, reader| {
                db.execute("CREATE TABLE tv (id INT PRIMARY KEY, a INT)").unwrap();
                insert_rows(&db, "tv");
                db.execute(&format!("ALTER TABLE tv ADD COLUMN {column}")).unwrap();
                reader.observe_current_layout();
                db.execute("DROP TABLE tv").unwrap();
            },
        );
    }
}

/// `count(*), sum(a)` of [`insert_rows`]: every row, each once.
fn whole(rows: &[Tuple]) -> Result<(), String> {
    let total = (1..=ROWS).map(|id| 10 * id).sum::<i64>();
    (ints(rows) == vec![ROWS, total])
        .then_some(())
        .ok_or_else(|| format!("{:?}", ints(rows)))
}

/// `count(*)` of one row.
fn one(rows: &[Tuple]) -> Result<(), String> {
    (ints(rows) == vec![1])
        .then_some(())
        .ok_or_else(|| format!("{:?}", ints(rows)))
}

#[test]
fn readers_beside_rename_column_and_index_changes_see_one_state() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    db.execute("CREATE TABLE rn (id INT PRIMARY KEY, a INT)").unwrap();
    insert_rows(&db, "rn");
    read_beside(
        &db,
        "SELECT count(*), sum(a) FROM rn",
        whole,
        SchemaRaces {
            tables: &[],
            columns: &["a"],
        },
        20,
        |round, reader| {
            let (from, to) = if round % 2 == 0 { ("a", "b") } else { ("b", "a") };
            db.execute(&format!("ALTER TABLE rn RENAME COLUMN {from} TO {to}"))
                .unwrap();
            if to == "a" {
                reader.observe_current_layout();
            }
        },
    );
    read_beside(
        &db,
        "SELECT count(*) FROM rn WHERE a = 70",
        one,
        SchemaRaces {
            tables: &[],
            columns: &[],
        },
        20,
        |round, reader| {
            if round % 2 == 0 {
                db.execute("CREATE INDEX rn_a ON rn (a)").unwrap();
            } else {
                db.execute("DROP INDEX rn_a").unwrap();
            }
            reader.observe_current_layout();
        },
    );
    let rows = db.query("SELECT count(*), sum(a) FROM rn", &[]).unwrap();
    assert_eq!(whole(&rows), Ok(()));
}

/// Every row of one refresh: `v = id + k` for one `k`.
fn one_refresh(rows: &[Tuple]) -> Result<(), String> {
    let n = ROWS;
    let got = ints(rows);
    let [count, sum, min, max] = got.as_slice() else {
        return Err(format!("{got:?}"));
    };
    let k = min - 1;
    let fits = *count == n && *max - *min == n - 1 && *sum == n * (n + 1) / 2 + n * k;
    fits.then_some(()).ok_or_else(|| format!("{got:?}"))
}

#[test]
fn readers_beside_refresh_materialized_view_see_one_refresh() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    db.execute("CREATE TABLE base (id INT PRIMARY KEY, v INT)").unwrap();
    let values: Vec<String> = (1..=ROWS).map(|id| format!("({id}, {id})")).collect();
    db.execute(&format!("INSERT INTO base VALUES {}", values.join(", ")))
        .unwrap();
    db.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, v FROM base")
        .unwrap();
    db.execute("REFRESH MATERIALIZED VIEW mv").unwrap();
    read_beside(
        &db,
        "SELECT count(*), sum(v), min(v), max(v) FROM mv",
        one_refresh,
        SchemaRaces {
            tables: &[],
            columns: &[],
        },
        10,
        |_, reader| {
            db.execute("UPDATE base SET v = v + 1").unwrap();
            db.execute("REFRESH MATERIALIZED VIEW mv").unwrap();
            reader.observe_current_layout();
        },
    );
}

/// Rows `(id, 10 * id, 20 * id)`, or `(id, 20 * id)` once `x` is dropped: never a value
/// under its neighbour's name.
fn unshifted(rows: &[Tuple]) -> Result<(), String> {
    for row in rows {
        let values: Vec<i64> = row.values.iter().map(int).collect();
        let fits = match values.as_slice() {
            [id, x, y] => *x == 10 * id && *y == 20 * id,
            [id, y] => *y == 20 * id,
            _ => false,
        };
        if !fits {
            return Err(format!("{values:?}"));
        }
    }
    Ok(())
}

/// R5.5 and R5.6 together: a reader that picked up the schema before DROP COLUMN's
/// group and reads the rows after it is refused and planned again, never shown a
/// value under its neighbour's name.
#[test]
fn a_reader_never_sees_a_shifted_value_during_drop_column() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    read_beside(
        &db,
        "SELECT * FROM rl ORDER BY id",
        unshifted,
        SchemaRaces {
            tables: &["rl"],
            columns: &[],
        },
        20,
        |_, reader| {
            db.execute("CREATE TABLE rl (id INT PRIMARY KEY, x INT, y INT)")
                .unwrap();
            let values: Vec<String> = (1..=200)
                .map(|id| format!("({id}, {}, {})", 10 * id, 20 * id))
                .collect();
            db.execute(&format!("INSERT INTO rl VALUES {}", values.join(", ")))
                .unwrap();
            reader.observe_current_layout();
            db.execute("ALTER TABLE rl DROP COLUMN x").unwrap();
            reader.observe_current_layout();
            db.execute("DROP TABLE rl").unwrap();
        },
    );
}

// ---------------------------------------------------------------------------
// A physical standby
// ---------------------------------------------------------------------------

#[cfg(all(feature = "ha-tier1", target_os = "linux"))]
mod standby {
    use super::*;
    use heliosdb_nano::replication::checkpoint_transfer::{
        OnlineCheckpointExport, PhysicalStage, ReceiverStaging, TransferLimits,
    };
    use heliosdb_nano::replication::history::load_or_create_primary_history;
    use heliosdb_nano::replication::physical::{poll_source, FlushMemo, MAX_BATCH_BYTES};
    use heliosdb_nano::replication::physical_applier::{DerivedStateListener, PhysicalApplier, VectorHook};
    use heliosdb_nano::storage::{StorageEngine, WriteGate};
    use heliosdb_nano::{Config, Schema};
    use rocksdb::DB;
    use std::path::Path;
    use std::sync::{Mutex, OnceLock, Weak};
    use uuid::Uuid;

    fn primary_config(path: &Path) -> Config {
        let mut config = Config::default();
        config.storage.path = Some(path.to_path_buf());
        config.storage.memory_only = false;
        config.replication.physical.enabled = true;
        config
    }

    /// As in `tests/physical_standby_embedded.rs`.
    fn stage(source: &DB, history: Uuid, root: &Path, config: &Config) -> PhysicalStage {
        let limits = TransferLimits::default();
        let export = OnlineCheckpointExport::create(source, history, root, limits).unwrap();
        let mut receiver = ReceiverStaging::begin(
            &root.join("standby"),
            &export.manifest_bytes().unwrap(),
            history,
            limits,
        )
        .unwrap();
        for (index, file) in export.manifest().files.iter().enumerate() {
            let mut offset = 0;
            while offset < file.size {
                let bytes = export.read_chunk(index as u32, offset, 64 * 1024).unwrap();
                receiver.append_chunk(index as u32, offset, &bytes).unwrap();
                offset += bytes.len() as u64;
            }
        }
        receiver.finish_download(config).unwrap().open(config).unwrap()
    }

    /// A reader that runs once, on the applier's thread, in the middle of the
    /// publication of a run that changed rows of `table`: after the run's catalog bytes
    /// are in the store and its schema is readable, while its rows are still being
    /// staged into the indexes (`VectorHook::on_row`).
    struct Probe {
        db: OnceLock<Weak<EmbeddedDatabase>>,
        table: &'static str,
        sql: &'static str,
        armed: AtomicBool,
        seen: Mutex<Vec<Result<Vec<i64>, String>>>,
    }

    impl VectorHook for Probe {
        fn wants_rows(&self, _engine: &StorageEngine, table: &str) -> bool {
            table == self.table
        }

        fn on_row(
            &self,
            _engine: &StorageEngine,
            table: &str,
            _row_id: u64,
            _schema: &Schema,
            _old: Option<&Tuple>,
            _new: Option<&Tuple>,
        ) {
            if table != self.table || !self.armed.swap(false, Ordering::SeqCst) {
                return;
            }
            let Some(db) = self.db.get().and_then(Weak::upgrade) else {
                return;
            };
            let answer = db
                .query(self.sql, &[])
                .map(|rows| ints(&rows))
                .map_err(|e| e.to_string());
            self.seen.lock().unwrap().push(answer);
        }
    }

    /// A primary and its physical standby, served by an `EmbeddedDatabase`; fields drop
    /// in order (the applier first, the directory last).
    struct Standby {
        applier: PhysicalApplier,
        database: Arc<EmbeddedDatabase>,
        _engine: Arc<StorageEngine>,
        _stage: PhysicalStage,
        raw: Arc<DB>,
        primary: EmbeddedDatabase,
        history: Uuid,
        _root: tempfile::TempDir,
    }

    impl Standby {
        fn new(setup: &[&str], probe: Option<Arc<Probe>>) -> Self {
            let root = tempfile::tempdir().unwrap();
            let config = primary_config(&root.path().join("primary"));
            let primary = EmbeddedDatabase::with_config(config.clone()).unwrap();
            for sql in setup {
                primary
                    .execute(sql)
                    .unwrap_or_else(|e| panic!("primary setup `{sql}`: {e}"));
            }
            let raw = primary.storage.db();
            let history = load_or_create_primary_history(raw.path()).unwrap();
            let stage = stage(&raw, history, root.path(), &config);
            let path = stage.staging_path().to_path_buf();
            let engine = Arc::new(
                StorageEngine::open_physical_standby(
                    &path,
                    &primary_config(&path),
                    Arc::clone(stage.writer()),
                    WriteGate::closed("physical standby"),
                )
                .unwrap(),
            );
            let database =
                Arc::new(EmbeddedDatabase::with_physical_standby(primary_config(&path), Arc::clone(&engine)).unwrap());
            let listener: Arc<dyn DerivedStateListener> = database.clone();
            let mut applier = PhysicalApplier::new(Arc::clone(&engine), Arc::clone(stage.writer()), listener).unwrap();
            if let Some(probe) = probe {
                let _ = probe.db.set(Arc::downgrade(&database));
                applier = applier.with_vector_hook(probe);
            }
            Self {
                applier,
                database,
                _engine: engine,
                _stage: stage,
                raw,
                primary,
                history,
                _root: root,
            }
        }

        fn run(&self, statements: &[&str]) {
            for sql in statements {
                self.primary
                    .execute(sql)
                    .unwrap_or_else(|e| panic!("primary `{sql}`: {e}"));
            }
        }

        /// Apply everything the primary has, one write group per run.
        fn sync(&self) {
            loop {
                let after = self.applier.writer().cursor().source_sequence();
                let poll = poll_source(&self.raw, self.history, after, MAX_BATCH_BYTES, &FlushMemo::new()).unwrap();
                if poll.batches.is_empty() {
                    return;
                }
                for batch in &poll.batches {
                    self.applier.apply(batch).unwrap();
                }
            }
        }
    }

    const COUNT_U: &str = "SELECT count(*), count(u) FROM tv";

    #[test]
    fn a_plan_cached_before_a_volatile_add_column_is_not_served_while_a_standby_publishes_it() {
        let probe = Arc::new(Probe {
            db: OnceLock::new(),
            table: "tv",
            sql: COUNT_U,
            armed: AtomicBool::new(false),
            seen: Mutex::new(Vec::new()),
        });
        let standby = Standby::new(&["CREATE TABLE marker (x INT)"], Some(Arc::clone(&probe)));
        standby.run(&["CREATE TABLE tv (id INT PRIMARY KEY, a INT)"]);
        let values: Vec<String> = (1..=ROWS).map(|id| format!("({id}, {id})")).collect();
        standby.run(&[&format!("INSERT INTO tv VALUES {}", values.join(", "))]);
        standby.sync();
        // Readers of the standby before the column exists: each fails (42703, at
        // execution), and the second admits the statement's plan to the plan cache.
        for _ in 0..3 {
            assert!(standby.database.query(COUNT_U, &[]).is_err(), "no column u yet");
        }

        standby.run(&["ALTER TABLE tv ADD COLUMN u UUID DEFAULT gen_random_uuid()"]);
        probe.armed.store(true, Ordering::SeqCst);
        standby.sync();

        let seen = probe.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "the probe read during the run's publication: {seen:?}");
        let n = ROWS;
        match &seen[0] {
            Ok(counts) => assert_eq!(
                counts,
                &vec![n, n],
                "the column NULL in every row (merged-tree review M3)"
            ),
            Err(error) => assert!(error.contains(LAYOUT_CHANGED_MARKER), "{error}"),
        }
        let rows = standby.database.query(COUNT_U, &[]).unwrap();
        assert_eq!(ints(&rows), vec![n, n]);
    }

    #[test]
    fn empty_and_populated_standby_before_add_refuse_unknown_aggregate_arguments() {
        let standby = Standby::new(&["CREATE TABLE marker (x INT)"], None);
        let values: Vec<String> = (1..=ROWS).map(|id| format!("({id}, {id})")).collect();
        for column in ["u UUID DEFAULT gen_random_uuid()", "u TEXT DEFAULT 'x'"] {
            standby.run(&["CREATE TABLE tv (id INT PRIMARY KEY, a INT)"]);
            standby.sync();
            for query in [COUNT_U, CASE_COUNT_U] {
                for _ in 0..3 {
                    let error = standby.database.query(query, &[]).unwrap_err();
                    assert!(
                        error
                            .to_string()
                            .contains("Column 'u' not found in aggregate input schema"),
                        "empty before ADD: {query}: {error}"
                    );
                }
            }
            standby.run(&[&format!("INSERT INTO tv VALUES {}", values.join(", "))]);
            standby.sync();
            for query in [COUNT_U, CASE_COUNT_U] {
                let error = standby.database.query(query, &[]).unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("Column 'u' not found in aggregate input schema"),
                    "populated before ADD: {query}: {error}"
                );
            }
            standby.run(&[&format!("ALTER TABLE tv ADD COLUMN {column}")]);
            standby.sync();
            for query in [COUNT_U, CASE_COUNT_U] {
                // Errors alone cannot satisfy the proof; each query returns
                // actual complete rows after the applied schema/data cut.
                for _ in 0..3 {
                    assert_eq!(ints(&standby.database.query(query, &[]).unwrap()), vec![ROWS, ROWS]);
                }
            }
            standby.run(&["DROP TABLE tv"]);
            standby.sync();
        }
    }

    #[test]
    fn standby_readers_beside_catalog_changes_never_see_a_torn_answer() {
        let standby = Standby::new(&["CREATE TABLE marker (x INT)"], None);
        let reader = ConcurrentReader::start(
            Arc::clone(&standby.database),
            vec![COUNT_U, CASE_COUNT_U],
            every_row_has_u,
            SchemaRaces {
                tables: &["tv"],
                columns: &["u"],
            },
        );
        let values: Vec<String> = (1..=ROWS).map(|id| format!("({id}, {id})")).collect();
        for round in 0..6 {
            let column = if round % 2 == 0 {
                "u UUID DEFAULT gen_random_uuid()"
            } else {
                "u TEXT DEFAULT 'x'"
            };
            standby.run(&["CREATE TABLE tv (id INT PRIMARY KEY, a INT)"]);
            standby.run(&[&format!("INSERT INTO tv VALUES {}", values.join(", "))]);
            standby.sync();
            std::thread::sleep(Duration::from_millis(20));
            standby.run(&[&format!("ALTER TABLE tv ADD COLUMN {column}")]);
            standby.sync();
            reader.observe_current_layout();
            standby.run(&["CREATE INDEX tv_a ON tv (a)", "ALTER TABLE tv RENAME COLUMN a TO b"]);
            standby.sync();
            reader.observe_current_layout();
            standby.run(&["DROP TABLE tv"]);
            standby.sync();
        }
        reader.finish();
    }
}
