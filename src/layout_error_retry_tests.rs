//! Real indexed predicates that straddle an atomic physical-row shape change.
//! The one-shot scan seam never supplies or replaces a query result/error.

use crate::{storage::layout_epochs, Config, EmbeddedDatabase, Error, Result, Tuple, Value};
use std::sync::{mpsc, Arc};
use std::time::Duration;

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;
const TABLE: &str = "layout_error_rows";
const WAIT: Duration = Duration::from_secs(20);
const QUERY: &str = "SELECT id FROM layout_error_rows WHERE v = 'v5'";
const VOLATILE_QUERY: &str = "SELECT id FROM layout_error_rows WHERE nextval('layout_error_once') > 0 AND v = 'v5'";

fn fixture(history: bool) -> Result<Arc<EmbeddedDatabase>> {
    let mut config = Config::default();
    config.storage.memory_only = true;
    config.storage.path = None;
    config.storage.time_travel_enabled = history;
    config.storage.version_gc_interval_secs = Some(0);
    let db = Arc::new(EmbeddedDatabase::with_config(config)?);
    db.execute(
        "CREATE TABLE layout_error_rows (id INT PRIMARY KEY, it INT NOT NULL, mid TEXT, v TEXT, emb VECTOR(4))",
    )?;
    db.execute("INSERT INTO layout_error_rows VALUES (5, 1, 'm5', 'v5', '[0.625,0.25,0.25,0.1562]')")?;
    db.execute("CREATE INDEX layout_error_v ON layout_error_rows (v)")?;
    db.execute("ALTER TABLE layout_error_rows ADD COLUMN d INT DEFAULT 7")?;
    Ok(db)
}

fn query(db: &EmbeddedDatabase, params: bool, sql: &str) -> Result<Vec<Tuple>> {
    if params {
        db.query_params(sql, &[])
    } else {
        db.query(sql, &[])
    }
}

fn exact_id_five(rows: &[Tuple]) {
    assert_eq!(rows.len(), 1, "the indexed predicate must retain its row");
    assert_eq!(
        rows.first().map(|row| row.values.as_slice()),
        Some([Value::Int4(5)].as_slice())
    );
}

fn exact_type_error(error: &Error) {
    assert!(
        matches!(error, Error::QueryExecution(_)),
        "actual evaluator error: {error}"
    );
    let text = error.to_string();
    assert!(
        text.contains("Cannot compare Vector("),
        "actual vector value must reach the stale v predicate: {text}"
    );
    assert!(
        text.contains("String(\"v5\")"),
        "original string predicate must be retained: {text}"
    );
}

fn overlap(params: bool, warm: bool, history: bool, volatile: bool) -> TestResult {
    let db = fixture(history)?;
    if warm {
        exact_id_five(&query(&db, params, QUERY)?);
        // Invalidate the result entry through a genuine successful write while
        // preserving the cached plan and existing row/index layout.
        db.execute("INSERT INTO layout_error_rows VALUES (6, 1, 'm6', 'v6', '[0,1,0,1]', 7)")?;
    }
    if volatile {
        db.execute("CREATE SEQUENCE layout_error_once")?;
    }
    let old_schema = db.storage.catalog().get_table_schema(TABLE)?;
    assert_eq!(old_schema.get_column_index("v"), Some(3));
    let (arrived_tx, arrived_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (invalid_tx, invalid_rx) = mpsc::channel();
    let reader_db = Arc::clone(&db);
    let reader = std::thread::spawn(move || {
        let armed = super::next_after_scan_layout_check(TABLE, move || {
            arrived_tx
                .send(())
                .map_err(|error| Error::internal(format!("scan arrival: {error}")))?;
            release_rx
                .recv_timeout(WAIT)
                .map_err(|error| Error::internal(format!("scan release: {error}")))?;
            let invalid = layout_epochs::validate_statement()
                .err()
                .is_some_and(|error| layout_epochs::is_layout_changed(&error));
            invalid_tx
                .send(invalid)
                .map_err(|error| Error::internal(format!("read-set proof: {error}")))?;
            Ok(())
        });
        if let Err(error) = armed {
            return (Err(error), 0, None);
        }
        let mut attempts = 0;
        let result = reader_db.run_layout_statement(false, || {
            attempts += 1;
            query(&reader_db, params, if volatile { VOLATILE_QUERY } else { QUERY })
        });
        // currval belongs to the same reader/session evaluation context that
        // consumed nextval, independent of another OS thread's scoped state.
        let current = volatile.then(|| query(&reader_db, params, "SELECT currval('layout_error_once')"));
        (result, attempts, current)
    });
    if let Err(error) = arrived_rx.recv_timeout(WAIT) {
        let _released = release_tx.send(());
        let outcome = reader
            .join()
            .map_err(|_| std::io::Error::other("reader panicked before scan arrival"))?;
        return Err(std::io::Error::other(format!(
            "real indexed scan did not reach its seam: {error}; outcome {outcome:?}"
        ))
        .into());
    }
    let writer_db = Arc::clone(&db);
    let (written_tx, written_rx) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        let result = writer_db.execute("ALTER TABLE layout_error_rows DROP COLUMN mid");
        let _sent = written_tx.send(result);
    });
    // Always unblock and reap both threads before evaluating any premise or
    // result assertion, including a writer that unexpectedly waited on a lock.
    let written = written_rx.recv_timeout(WAIT);
    let new_v_index = match &written {
        Ok(Ok(_)) => db
            .storage
            .catalog()
            .get_table_schema(TABLE)
            .map(|schema| schema.get_column_index("v")),
        _ => Ok(None),
    };
    let _released = release_tx.send(());
    let read_joined = reader.join();
    let writer_joined = writer.join();
    let read = read_joined.map_err(|_| std::io::Error::other("reader panicked"))?;
    writer_joined.map_err(|_| std::io::Error::other("DDL writer panicked"))?;
    written??;
    assert_eq!(
        new_v_index?,
        Some(2),
        "a completed DROP really moved v before the read resumed"
    );
    assert!(
        invalid_rx.recv_timeout(WAIT)?,
        "the old schema's ACTUAL statement read set must be invalid"
    );
    let (result, attempts, current) = read;
    eprintln!(
        "layout-error-overlap-premise params={params} warm={warm} history={history} volatile={volatile} drop_done=true old_v=3 new_v=2 invalid_readset=true attempts={attempts}"
    );
    if volatile {
        let error = result
            .err()
            .ok_or_else(|| std::io::Error::other("volatile stale predicate unexpectedly succeeded"))?;
        assert!(
            matches!(&error, Error::Transaction(_)) && layout_epochs::is_layout_changed(&error),
            "a moved layout after nextval must refuse without repeating: {error}"
        );
        assert_eq!(crate::protocol::postgres::handler::sqlstate_for_error(&error), "40001");
        assert_eq!(attempts, 1, "a consumed sequence value forbids another evaluation");
        let rows = current.ok_or_else(|| std::io::Error::other("reader did not capture currval"))??;
        assert_eq!(rows.first().and_then(|row| row.values.first()), Some(&Value::Int8(1)));
    } else {
        // Before the production fix this propagates the real Vector/String
        // error (one attempt). After it, re-planning must give the exact row.
        let rows = result?;
        exact_id_five(&rows);
        assert_eq!(attempts, 2, "one completed DDL needs exactly one safe re-plan");
    }
    eprintln!(
        "layout-error-retry-cell params={params} warm={warm} history={history} volatile={volatile} attempts={attempts}"
    );
    Ok(())
}

#[test]
fn indexed_layout_overlap_replans_the_actual_stale_predicate() -> TestResult {
    for history in [false, true] {
        for params in [false, true] {
            for warm in [false, true] {
                overlap(params, warm, history, false)?;
            }
        }
    }
    Ok(())
}

#[test]
fn a_current_layout_keeps_the_real_type_error_without_repeating_it() -> TestResult {
    for history in [false, true] {
        for params in [false, true] {
            let db = fixture(history)?;
            let mut attempts = 0;
            let result = db.run_layout_statement(false, || {
                attempts += 1;
                query(&db, params, "SELECT id FROM layout_error_rows WHERE emb = 'v5'")
            });
            let error = result
                .err()
                .ok_or_else(|| std::io::Error::other("genuine vector/string predicate unexpectedly succeeded"))?;
            exact_type_error(&error);
            assert_eq!(attempts, 1, "an unchanged read set cannot justify a retry");
            assert_eq!(
                db.storage.catalog().get_table_schema(TABLE)?.get_column_index("v"),
                Some(3)
            );
            eprintln!("layout-error-current-cell params={params} history={history} attempts={attempts}");
        }
    }
    Ok(())
}

#[test]
fn a_sequence_side_effect_refuses_changed_layout_without_repeating_it() -> TestResult {
    for history in [false, true] {
        for params in [false, true] {
            overlap(params, false, history, true)?;
        }
    }
    Ok(())
}
