//! Real eager ART mutations versus committed row cardinality.

use super::RowId;
use std::cell::RefCell;
use std::collections::HashSet;

type InsertionHook = Box<dyn FnOnce()>;
type ProbeHook = Box<dyn FnOnce(usize)>;

struct Insertion {
    table: String,
    target: usize,
    rows: HashSet<RowId>,
    callback: Option<InsertionHook>,
}

#[derive(Default)]
struct Hooks {
    insertion: Option<Insertion>,
    before_probe: Option<(String, InsertionHook)>,
    after_probe: Option<(String, ProbeHook)>,
    before_enrollment: Option<(String, InsertionHook)>,
    enrollment_attempt: Option<(String, InsertionHook)>,
}

thread_local! {
    static HOOKS: RefCell<Hooks> = const { RefCell::new(Hooks {
        insertion: None, before_probe: None, after_probe: None, before_enrollment: None, enrollment_attempt: None,
    }) };
}

pub(super) fn before_cardinality_enrollment(table: &str) {
    let callback = HOOKS.with(|slot| {
        let mut hooks = slot.borrow_mut();
        if hooks.before_enrollment.as_ref().is_some_and(|(name, _)| name == table) {
            hooks.before_enrollment.take().map(|(_, callback)| callback)
        } else {
            None
        }
    });
    if let Some(callback) = callback {
        callback();
    }
}

pub(super) fn at_cardinality_enrollment_attempt(table: &str) {
    let callback = HOOKS.with(|slot| {
        let mut hooks = slot.borrow_mut();
        if hooks.enrollment_attempt.as_ref().is_some_and(|(name, _)| name == table) {
            hooks.enrollment_attempt.take().map(|(_, callback)| callback)
        } else {
            None
        }
    });
    if let Some(callback) = callback {
        callback();
    }
}

pub(super) fn after_index_insertion(table: &str, row: RowId) {
    let callback = HOOKS.with(|slot| {
        let mut hooks = slot.borrow_mut();
        let Some(insertion) = hooks.insertion.as_mut().filter(|hook| hook.table == table) else {
            return None;
        };
        insertion.rows.insert(row);
        if insertion.rows.len() == insertion.target {
            insertion.callback.take()
        } else {
            None
        }
    });
    if let Some(callback) = callback {
        callback();
    }
}

use crate::{Config, EmbeddedDatabase, Error, Result, Tuple, Value};
use rocksdb::IteratorMode;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

type Check<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;
type Image = Vec<(Vec<u8>, Vec<u8>)>;
const TABLE: &str = "count_publication_rows";
const OTHER: &str = "count_publication_other";
const WAIT: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug)]
enum Family {
    Text,
    Params,
}

impl Family {
    fn query(self, db: &EmbeddedDatabase, sql: &str) -> Result<Vec<Tuple>> {
        match self {
            Self::Text => db.query(sql, &[]),
            Self::Params => db.query_params(sql, &[]),
        }
    }

    fn insert(self, db: &EmbeddedDatabase) -> Result<u64> {
        match self {
            Self::Text => {
                let rows = (1..=12)
                    .map(|id| format!("({id},'v{id}')"))
                    .collect::<Vec<_>>()
                    .join(",");
                db.execute(&format!("INSERT INTO {TABLE} VALUES {rows}"))
            }
            Self::Params => {
                let rows = (0..12)
                    .map(|id| format!("(${},${})", id * 2 + 1, id * 2 + 2))
                    .collect::<Vec<_>>()
                    .join(",");
                let values = (1..=12)
                    .flat_map(|id| [Value::Int4(id), Value::String(format!("v{id}"))])
                    .collect::<Vec<_>>();
                db.execute_params(&format!("INSERT INTO {TABLE} VALUES {rows}"), &values)
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Writer {
    Sql(Family),
    Single,
    Batch,
}

impl Writer {
    fn partial(self) -> usize {
        if matches!(self, Self::Single) {
            1
        } else {
            6
        }
    }

    fn total(self) -> usize {
        if matches!(self, Self::Single) {
            1
        } else {
            12
        }
    }

    fn run(self, db: &EmbeddedDatabase) -> Result<u64> {
        match self {
            Self::Sql(family) => family.insert(db),
            Self::Single => db.storage.insert_tuple(TABLE, tuple(1)),
            Self::Batch => {
                let schema = db.storage.catalog().get_table_schema(TABLE)?;
                let rows = (1..=12).map(|id| (id as u64, tuple(id))).collect();
                db.storage.insert_prepared_tuples_fast_batch(TABLE, rows, &schema)
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Form {
    Bare,
    Range,
    In,
}

impl Form {
    fn sql(self, table: &str) -> String {
        let predicate = match self {
            Self::Bare => "",
            Self::Range => " WHERE id >= 1 AND id <= 12",
            Self::In => " WHERE id IN (1,2,3,4,5,6,7,8,9,10,11,12)",
        };
        format!("SELECT COUNT(*) FROM {table}{predicate}")
    }
}

fn tuple(id: i32) -> Tuple {
    Tuple {
        values: vec![Value::Int4(id), Value::String(format!("v{id}"))],
        row_id: None,
        branch_id: None,
    }
}

fn open(path: &std::path::Path, history: bool, writer: Writer) -> Result<Arc<EmbeddedDatabase>> {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    config.storage.time_travel_enabled = history;
    config.storage.version_gc_interval_secs = Some(0);
    config.storage.logical_wal_per_statement = matches!(writer, Writer::Sql(_));
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    config.storage.wal_checkpoint_on_close = false;
    config.storage.flush_on_close = false;
    let db = Arc::new(EmbeddedDatabase::with_config(config)?);
    db.storage.set_index_snapshots_on_close(false);
    db.storage.set_row_counter_flush_on_close(false);
    db.storage.table_locks().set_timeout(Duration::from_secs(5));
    db.execute(&format!("CREATE TABLE {TABLE}(id INT PRIMARY KEY,v TEXT)"))?;
    db.execute(&format!("CREATE TABLE {OTHER}(id INT PRIMARY KEY,v TEXT)"))?;
    db.execute(&format!("INSERT INTO {OTHER} VALUES(1,'other')"))?;
    Ok(db)
}

fn image(db: &EmbeddedDatabase) -> Result<Image> {
    db.storage
        .db()
        .iterator(IteratorMode::Start)
        .map(|item| {
            item.map(|(key, value)| (key.into_vec(), value.into_vec()))
                .map_err(Error::from)
        })
        .collect()
}

fn row_count(db: &EmbeddedDatabase) -> Result<usize> {
    let prefix = format!("data:{TABLE}:");
    db.storage
        .db()
        .iterator(IteratorMode::Start)
        .try_fold(0, |count, record| {
            let (key, _) = record.map_err(Error::from)?;
            Ok(count + usize::from(key.starts_with(prefix.as_bytes())))
        })
}

fn count(rows: &[Tuple]) -> Check<usize> {
    match (rows.len(), rows.first().map(|row| row.values.as_slice())) {
        (1, Some([Value::Int8(value)])) => Ok(usize::try_from(*value)?),
        (1, Some([Value::Int4(value)])) => Ok(usize::try_from(*value)?),
        _ => Err(std::io::Error::other(format!("not a scalar COUNT: {rows:?}")).into()),
    }
}

fn joined<T>(result: std::thread::Result<T>) -> Check<T> {
    result.map_err(|_| std::io::Error::other("COUNT worker panicked").into())
}

// Each cell releases and reaps its workers before checking its oracles. Keep
// later independent cells visible in a deliberately failing baseline run.
fn cell(body: impl FnOnce() -> Check) -> Check {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(result) => result,
        Err(payload) => {
            let message = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("COUNT cell assertion failed");
            Err(std::io::Error::other(message.to_string()).into())
        }
    }
}

struct Armed;
impl Drop for Armed {
    fn drop(&mut self) {
        HOOKS.with(|slot| *slot.borrow_mut() = Hooks::default());
    }
}

fn arm_insert(table: &str, target: usize, callback: impl FnOnce() + 'static) -> Armed {
    HOOKS.with(|slot| {
        slot.borrow_mut().insertion = Some(Insertion {
            table: table.to_string(),
            target,
            rows: HashSet::new(),
            callback: Some(Box::new(callback)),
        })
    });
    Armed
}

fn hook_error(errors: &Mutex<Vec<String>>, error: impl ToString) {
    errors
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(error.to_string());
}

#[derive(Debug)]
struct Premise {
    art: Option<usize>,
    rows: Result<usize>,
    sequence: u64,
}

struct Paused {
    start: mpsc::Sender<()>,
    ready: mpsc::Receiver<Premise>,
    release: mpsc::Sender<()>,
    finished: mpsc::Receiver<()>,
    worker: std::thread::JoinHandle<Result<u64>>,
    errors: Arc<Mutex<Vec<String>>>,
}

fn paused(db: &Arc<EmbeddedDatabase>, writer: Writer, refused: bool) -> Paused {
    let (start, start_rx) = mpsc::channel();
    let (ready_tx, ready) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let (finished_tx, finished) = mpsc::channel();
    let errors = Arc::new(Mutex::new(Vec::new()));
    let writer_errors = Arc::clone(&errors);
    let writer_db = Arc::clone(db);
    let worker = std::thread::spawn(move || {
        let result = (|| {
            start_rx
                .recv_timeout(WAIT)
                .map_err(|error| Error::internal(error.to_string()))?;
            let hook_db = Arc::clone(&writer_db);
            let errors = Arc::clone(&writer_errors);
            let _armed = arm_insert(TABLE, writer.partial(), move || {
                let premise = Premise {
                    art: hook_db.storage.art_indexes().pk_index_len(TABLE),
                    rows: row_count(&hook_db),
                    sequence: hook_db.storage.db().latest_sequence_number(),
                };
                // Let legitimate planning/preflight finish, then refuse the
                // owning native group after these real eager claims exist.
                if refused {
                    hook_db.storage.debug_set_write_group_ceiling(Some(0));
                }
                if let Err(error) = ready_tx.send(premise) {
                    hook_error(&errors, error);
                }
                if let Err(error) = release_rx.recv_timeout(WAIT) {
                    hook_error(&errors, error);
                }
            });
            writer.run(&writer_db)
        })();
        let _sent = finished_tx.send(());
        result
    });
    Paused {
        start,
        ready,
        release,
        finished,
        worker,
        errors,
    }
}

fn valid_premise(premise: &Premise, writer: Writer, sequence: u64) -> Check {
    eprintln!(
        "count-publication-eager-premise writer={writer:?} art={:?} raw_rows={:?} native_sequence={} before_sequence={sequence}",
        premise.art, premise.rows, premise.sequence
    );
    assert_eq!(premise.art, Some(writer.partial()), "actual unique ART prefix");
    assert_eq!(
        premise.rows.as_ref().map_err(ToString::to_string)?,
        &0,
        "owning native row group has not run"
    );
    assert_eq!(
        premise.sequence, sequence,
        "no native group before the eager prefix pause"
    );
    Ok(())
}

fn finish(
    db: &EmbeddedDatabase,
    writer: Writer,
    refused: bool,
    before: &Image,
    sequence: u64,
    written: Result<u64>,
) -> Check {
    db.storage.debug_set_write_group_ceiling(None);
    if refused {
        let error = written
            .err()
            .ok_or_else(|| std::io::Error::other("cap0 writer succeeded"))?;
        let sqlstate = crate::protocol::postgres::handler::sqlstate_for_error(&error);
        eprintln!("count-publication-resource-refusal writer={writer:?} sqlstate={sqlstate} outer={error:?}");
        assert!(
            error
                .to_string()
                .contains(crate::error::BRANCH_MATERIALIZE_TOO_LARGE_MARKER),
            "{error}"
        );
        assert_eq!(sqlstate, "54000", "{error}");
        assert!(!error.is_applied_publication_failure(), "{error}");
        assert_eq!(image(db)?, *before, "proven NotWritten whole KV truth");
        assert_eq!(db.storage.db().latest_sequence_number(), sequence);
    } else {
        let _affected = written?;
    }
    assert!(
        !db.storage.has_uncommitted_writes_for_table(TABLE),
        "owner fully settled"
    );
    assert!(!db.storage.has_uncommitted_index_removals_for_table(TABLE));
    let expected = if refused { 0 } else { writer.total() };
    let rows = db.query(&format!("SELECT id FROM {TABLE} ORDER BY id"), &[])?;
    let ids = rows.iter().map(|row| row.values.first().cloned()).collect::<Vec<_>>();
    let wanted = (1..=expected)
        .map(|id| Some(Value::Int4(id as i32)))
        .collect::<Vec<_>>();
    assert_eq!(ids, wanted);
    assert_eq!(row_count(db)?, expected);
    for family in [Family::Text, Family::Params] {
        for form in [Form::Bare, Form::Range, Form::In] {
            assert_eq!(count(&family.query(db, &form.sql(TABLE))?)?, expected);
        }
    }
    Ok(())
}

fn check_errors(errors: &Mutex<Vec<String>>) -> Check {
    let errors = errors.lock().unwrap_or_else(|poison| poison.into_inner());
    if errors.is_empty() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("COUNT hook failures: {errors:?}")).into())
    }
}

#[test]
fn public_sql_counts_never_publish_an_uncommitted_eager_prefix() -> Check {
    let mut failures = Vec::new();
    for history in [false, true] {
        for family in [Family::Text, Family::Params] {
            for refused in [false, true] {
                for form in [Form::Bare, Form::Range, Form::In] {
                    let result = cell(|| -> Check {
                        let temp = tempfile::tempdir()?;
                        let writer = Writer::Sql(family);
                        let db = open(temp.path(), history, writer)?;
                        let before = image(&db)?;
                        let sequence = db.storage.db().latest_sequence_number();
                        let paused = paused(&db, writer, refused);
                        let started = paused.start.send(());
                        let ready = paused.ready.recv_timeout(WAIT);
                        let reader_db = Arc::clone(&db);
                        let sql = form.sql(TABLE);
                        let (read_tx, read_rx) = mpsc::channel();
                        let reader = std::thread::spawn(move || {
                            let _sent = read_tx.send(family.query(&reader_db, &sql));
                        });
                        // A safe fallback may wait for publication. An early response
                        // must already describe committed rows, never the eager six.
                        let early = read_rx.recv_timeout(Duration::from_millis(200));
                        let _released = paused.release.send(());
                        let written = paused.worker.join();
                        let read = match early {
                            Ok(read) => Ok(read),
                            Err(mpsc::RecvTimeoutError::Timeout) => read_rx.recv_timeout(WAIT),
                            Err(error) => Err(error),
                        };
                        let reader_join = reader.join();
                        started?;
                        valid_premise(&ready?, writer, sequence)?;
                        let written = joined(written)?;
                        let read = read??;
                        joined(reader_join)?;
                        check_errors(&paused.errors)?;
                        finish(&db, writer, refused, &before, sequence, written)?;
                        let observed = count(&read)?;
                        assert!(
                            observed == 0 || (!refused && observed == 12),
                            "torn public COUNT: {observed}"
                        );
                        Ok(())
                    });
                    eprintln!("count-publication-cell public history={history} family={family:?} refused={refused} form={form:?} ok={}", result.is_ok());
                    if let Err(error) = result {
                        failures.push(error.to_string());
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:?}");
    Ok(())
}

#[test]
fn completed_sql_writers_invalidate_the_entire_cardinality_probe() -> Check {
    let mut failures = Vec::new();
    for history in [false, true] {
        for family in [Family::Text, Family::Params] {
            for refused in [false, true] {
                for form in [Form::Bare, Form::Range, Form::In] {
                    let result = cell(|| -> Check {
                        let temp = tempfile::tempdir()?;
                        let writer = Writer::Sql(family);
                        let db = open(temp.path(), history, writer)?;
                        let before = image(&db)?;
                        let sequence = db.storage.db().latest_sequence_number();
                        let paused = paused(&db, writer, refused);
                        let Paused {
                            start,
                            ready,
                            release,
                            finished,
                            worker,
                            errors,
                        } = paused;
                        let premises = Arc::new(Mutex::new(Vec::new()));
                        let seen = Arc::clone(&premises);
                        let before_errors = Arc::clone(&errors);
                        let start_cleanup = start.clone();
                        let release_cleanup = release.clone();
                        let after_errors = Arc::clone(&errors);
                        let measured = Arc::new(Mutex::new(Vec::new()));
                        let measured_hook = Arc::clone(&measured);
                        HOOKS.with(|slot| {
                            let mut hooks = slot.borrow_mut();
                            hooks.before_probe = Some((
                                TABLE.to_string(),
                                Box::new(move || {
                                    if let Err(error) = start.send(()) {
                                        hook_error(&before_errors, error);
                                    }
                                    match ready.recv_timeout(WAIT) {
                                        Ok(premise) => {
                                            seen.lock().unwrap_or_else(|poison| poison.into_inner()).push(premise)
                                        }
                                        Err(error) => hook_error(&before_errors, error),
                                    }
                                }),
                            ));
                            hooks.after_probe = Some((
                                TABLE.to_string(),
                                Box::new(move |value| {
                                    measured_hook
                                        .lock()
                                        .unwrap_or_else(|poison| poison.into_inner())
                                        .push(value);
                                    if let Err(error) = release.send(()) {
                                        hook_error(&after_errors, error);
                                    }
                                    if let Err(error) = finished.recv_timeout(WAIT) {
                                        hook_error(&after_errors, error);
                                    }
                                }),
                            ));
                        });
                        let _armed = Armed;
                        // Same real private COUNT helper used by cold text SQL;
                        // Family labels name the writer, not a second reader path.
                        let probe = db.try_fast_count_pk_query(&form.sql(TABLE));
                        let _started = start_cleanup.send(());
                        let _released = release_cleanup.send(());
                        let written = joined(worker.join())?;
                        check_errors(&errors)?;
                        let premises = premises.lock().unwrap_or_else(|poison| poison.into_inner());
                        assert_eq!(premises.len(), 1, "real pre-probe handshake");
                        valid_premise(
                            premises
                                .first()
                                .ok_or_else(|| std::io::Error::other("missing eager prefix"))?,
                            writer,
                            sequence,
                        )?;
                        assert_eq!(
                            *measured.lock().unwrap_or_else(|poison| poison.into_inner()),
                            vec![6],
                            "actual ART read before writer settlement"
                        );
                        finish(&db, writer, refused, &before, sequence, written)?;
                        if let Some(rows) = probe {
                            let observed = count(&rows?)?;
                            assert!(
                                observed == 0 || (!refused && observed == 12),
                                "completed-writer ABA leaked {observed}"
                            );
                        }
                        Ok(())
                    });
                    eprintln!("count-publication-cell aba history={history} writer_family={family:?} refused={refused} form={form:?} ok={}", result.is_ok());
                    if let Err(error) = result {
                        failures.push(error.to_string());
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:?}");
    Ok(())
}

#[test]
fn direct_single_and_batch_claims_cover_the_first_eager_key() -> Check {
    let mut failures = Vec::new();
    for history in [false, true] {
        for writer in [Writer::Single, Writer::Batch] {
            for refused in [false, true] {
                for form in [Form::Bare, Form::Range, Form::In] {
                    let result = cell(|| -> Check {
                        let temp = tempfile::tempdir()?;
                        let db = open(temp.path(), history, writer)?;
                        let before = image(&db)?;
                        let sequence = db.storage.db().latest_sequence_number();
                        let paused = paused(&db, writer, refused);
                        let started = paused.start.send(());
                        let ready = paused.ready.recv_timeout(WAIT);
                        let probe = db.try_fast_count_pk_query(&form.sql(TABLE));
                        let _released = paused.release.send(());
                        let written = joined(paused.worker.join())?;
                        started?;
                        valid_premise(&ready?, writer, sequence)?;
                        check_errors(&paused.errors)?;
                        finish(&db, writer, refused, &before, sequence, written)?;
                        if let Some(rows) = probe {
                            assert_eq!(count(&rows?)?, 0, "direct claim exposes rows before its native group");
                        }
                        Ok(())
                    });
                    eprintln!("count-publication-cell direct history={history} writer={writer:?} refused={refused} form={form:?} ok={}", result.is_ok());
                    if let Err(error) = result {
                        failures.push(error.to_string());
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:?}");
    Ok(())
}

#[test]
fn insertion_census_preserves_unrelated_counts_and_index_miss_gates() -> Check {
    for history in [false, true] {
        for family in [Family::Text, Family::Params] {
            for form in [Form::Bare, Form::Range, Form::In] {
                let temp = tempfile::tempdir()?;
                let writer = Writer::Sql(family);
                let db = open(temp.path(), history, writer)?;
                let before = image(&db)?;
                let sequence = db.storage.db().latest_sequence_number();
                let paused = paused(&db, writer, false);
                let started = paused.start.send(());
                let ready = paused.ready.recv_timeout(WAIT);
                let fast = db.try_fast_count_pk_query(&form.sql(OTHER));
                let miss = family.query(&db, &format!("SELECT id FROM {OTHER} WHERE id=99"));
                let untouched = !db.storage.has_uncommitted_writes_for_table(OTHER);
                let removals = db.storage.has_uncommitted_index_removals_for_table(TABLE);
                let _released = paused.release.send(());
                let written = joined(paused.worker.join())?;
                started?;
                valid_premise(&ready?, writer, sequence)?;
                check_errors(&paused.errors)?;
                finish(&db, writer, false, &before, sequence, written)?;
                assert!(untouched);
                assert!(
                    !removals,
                    "INSERT cardinality must not arm the narrower removal/miss gate"
                );
                assert!(miss?.is_empty());
                let rows = fast.ok_or_else(|| std::io::Error::other("unrelated COUNT fast path was disabled"))??;
                assert_eq!(count(&rows)?, 1);
                eprintln!(
                    "count-publication-cell unrelated history={history} writer_family={family:?} form={form:?} ok=true"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn parallel_statements_cannot_overtake_their_shared_raii_enrollment() -> Check {
    let mut failures = Vec::new();
    for history in [false, true] {
        for rollback in [false, true] {
            let result = cell(|| -> Check {
                let temp = tempfile::tempdir()?;
                let db = open(temp.path(), history, Writer::Sql(Family::Text))?;
                let before = image(&db)?;
                let sequence = db.storage.db().latest_sequence_number();
                let txn = db.begin_transaction()?;
                let errors = Arc::new(Mutex::new(Vec::new()));
                let (ready_tx, ready_rx) = mpsc::channel();
                let (release_tx, release_rx) = mpsc::channel();
                let (started_tx, started_rx) = mpsc::channel();
                let (art_tx, art_rx) = mpsc::channel();
                let first_errors = Arc::clone(&errors);
                let (ready, started, early_art, before_native, probe, first, second) = std::thread::scope(|scope| {
                    let tx = &txn;
                    let first = scope.spawn(move || {
                        HOOKS.with(|slot| {
                            slot.borrow_mut().before_enrollment = Some((
                                TABLE.to_string(),
                                Box::new(move || {
                                    if let Err(error) = ready_tx.send(()) {
                                        hook_error(&first_errors, error);
                                    }
                                    if let Err(error) = release_rx.recv_timeout(WAIT) {
                                        hook_error(&first_errors, error);
                                    }
                                }),
                            ))
                        });
                        let _armed = Armed;
                        tx.execute(&format!("INSERT INTO {TABLE} VALUES(1,'first')"))
                    });
                    let ready = ready_rx.recv_timeout(WAIT);
                    let second = scope.spawn(move || {
                        HOOKS.with(|slot| {
                            slot.borrow_mut().enrollment_attempt = Some((
                                TABLE.to_string(),
                                Box::new(move || {
                                    let _sent = started_tx.send(());
                                }),
                            ))
                        });
                        let _armed = arm_insert(TABLE, 1, move || {
                            let _sent = art_tx.send(());
                        });
                        tx.execute(&format!("INSERT INTO {TABLE} VALUES(2,'second')"))
                    });
                    let started = started_rx.recv_timeout(WAIT);
                    let early_art = art_rx.recv_timeout(Duration::from_millis(200));
                    let before_native = row_count(&db);
                    let probe = db.try_fast_count_pk_query(&Form::Bare.sql(TABLE));
                    let _released = release_tx.send(());
                    let first = first.join();
                    let second = second.join();
                    (ready, started, early_art, before_native, probe, first, second)
                });
                // No assertions or fallible exits until both real SQL workers end.
                let first = joined(first)?;
                let second = joined(second)?;
                let own = txn.query(&format!("SELECT id FROM {TABLE} ORDER BY id"), &[]);
                let ended = if rollback { txn.rollback() } else { txn.commit() };
                ready?;
                started?;
                first?;
                second?;
                ended?;
                check_errors(&errors)?;
                assert!(
                    matches!(early_art, Err(mpsc::RecvTimeoutError::Timeout)),
                    "second statement passed unpublished enrollment: {early_art:?}"
                );
                assert_eq!(before_native?, 0);
                if let Some(rows) = probe {
                    assert_eq!(count(&rows?)?, 0, "shared RAII dirty ART escaped");
                }
                let own = own?;
                assert_eq!(
                    own.iter().map(|row| row.values.first().cloned()).collect::<Vec<_>>(),
                    vec![Some(Value::Int4(1)), Some(Value::Int4(2))]
                );
                let expected = if rollback { 0 } else { 2 };
                assert_eq!(row_count(&db)?, expected);
                assert_eq!(count(&db.query(&Form::Bare.sql(TABLE), &[])?)?, expected);
                assert!(!db.storage.has_uncommitted_writes_for_table(TABLE));
                if rollback {
                    assert_eq!(image(&db)?, before);
                    assert_eq!(db.storage.db().latest_sequence_number(), sequence);
                }
                Ok(())
            });
            eprintln!(
                "count-publication-cell shared_raii history={history} rollback={rollback} ok={}",
                result.is_ok()
            );
            if let Err(error) = result {
                failures.push(error.to_string());
            }
        }
    }
    assert!(failures.is_empty(), "{failures:?}");
    Ok(())
}

pub(super) fn before_cardinality_probe(table: &str) {
    let callback = HOOKS.with(|slot| {
        let mut hooks = slot.borrow_mut();
        if hooks.before_probe.as_ref().is_some_and(|(name, _)| name == table) {
            hooks.before_probe.take().map(|(_, callback)| callback)
        } else {
            None
        }
    });
    if let Some(callback) = callback {
        callback();
    }
}

pub(super) fn after_cardinality_probe(table: &str, count: usize) {
    let callback = HOOKS.with(|slot| {
        let mut hooks = slot.borrow_mut();
        if hooks.after_probe.as_ref().is_some_and(|(name, _)| name == table) {
            hooks.after_probe.take().map(|(_, callback)| callback)
        } else {
            None
        }
    });
    if let Some(callback) = callback {
        callback(count);
    }
}
