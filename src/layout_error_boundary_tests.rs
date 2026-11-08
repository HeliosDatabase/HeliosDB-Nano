//! Real session/lock-control failures racing a completed layout change.
//! These are wire-facing session APIs; actual socket coverage is separate.

use crate::{storage::layout_epochs, Config, EmbeddedDatabase, Error, Result, Tuple, Value};
use rocksdb::IteratorMode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

type Check<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;
type Image = Vec<(Vec<u8>, Vec<u8>)>;
const ROWS: &str = "layout_boundary_rows";
const MUT: &str = "layout_boundary_mut";
const WAIT: Duration = Duration::from_secs(20);
const QUERY: &str = "SELECT id FROM layout_boundary_rows WHERE v = 'v5'";
const UPDATE: &str = "UPDATE layout_boundary_mut SET v=20 WHERE id=1";

#[derive(Clone, Copy, Debug)]
enum Family {
    Text,
    Params,
}

impl Family {
    fn execute(self, db: &EmbeddedDatabase, sid: crate::session::SessionId, sql: &str) -> Result<u64> {
        match self {
            Self::Text => db.execute_for_session(sid, sql),
            Self::Params => db.execute_params_for_session(sid, sql, &[]),
        }
    }

    fn query(self, db: &EmbeddedDatabase, sid: crate::session::SessionId, sql: &str) -> Result<Vec<Tuple>> {
        match self {
            Self::Text => db.query_in_session(sid, sql, &[]),
            Self::Params => db.query_params_for_session(sid, sql, &[]),
        }
    }
}

fn config(path: &std::path::Path, history: bool) -> Config {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    config.storage.time_travel_enabled = history;
    config.storage.version_gc_interval_secs = Some(0);
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    config.storage.wal_checkpoint_on_close = false;
    config.storage.flush_on_close = false;
    config
}

fn open(config: Config) -> Result<Arc<EmbeddedDatabase>> {
    let db = Arc::new(EmbeddedDatabase::with_config(config)?);
    db.storage.set_index_snapshots_on_close(false);
    db.storage.set_row_counter_flush_on_close(false);
    db.storage.table_locks().set_timeout(Duration::from_secs(5));
    Ok(db)
}

fn seed(db: &EmbeddedDatabase) -> Result<()> {
    db.execute("CREATE TABLE layout_boundary_rows(id INT PRIMARY KEY,it INT,mid TEXT,v TEXT,emb VECTOR(4))")?;
    db.execute("INSERT INTO layout_boundary_rows VALUES(5,1,'middle','v5','[0.625,0.25,0.125,0.1562]')")?;
    db.execute("CREATE INDEX layout_boundary_v ON layout_boundary_rows(v)")?;
    db.execute("ALTER TABLE layout_boundary_rows ADD COLUMN d INT DEFAULT 7")?;
    db.execute("CREATE TABLE layout_boundary_audit(id INT PRIMARY KEY)")?;
    db.execute("CREATE TABLE layout_boundary_mut(id INT PRIMARY KEY,v INT,emb VECTOR(4))")?;
    db.execute("INSERT INTO layout_boundary_mut VALUES(1,10,'[1,0,0,0]')")?;
    Ok(())
}

fn image(db: &EmbeddedDatabase) -> Result<Image> {
    let raw = db.storage.db();
    raw.iterator(IteratorMode::Start)
        .map(|row| {
            row.map(|(key, value)| (key.into_vec(), value.into_vec()))
                .map_err(Error::from)
        })
        .collect()
}

fn joined<T>(result: std::thread::Result<T>, what: &str) -> Check<T> {
    result.map_err(|_| std::io::Error::other(format!("{what} panicked")).into())
}

fn exact_id(rows: &[Tuple], id: i32) {
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows.first().map(|row| row.values.as_slice()),
        Some([Value::Int4(id)].as_slice())
    );
}

#[test]
fn explicit_wire_session_layout_refusal_never_repeats_and_rolls_back_cleanly() -> Check {
    for history in [false, true] {
        for family in [Family::Text, Family::Params] {
            let root = tempfile::tempdir()?;
            let db = open(config(root.path(), history))?;
            seed(&db)?;
            let old_v = db.storage.catalog().get_table_schema(ROWS)?.get_column_index("v");
            let sid = db.create_session(
                "layout-boundary-explicit",
                crate::session::IsolationLevel::ReadCommitted,
            )?;
            family.execute(&db, sid, "BEGIN")?;
            family.execute(&db, sid, "INSERT INTO layout_boundary_audit VALUES(99)")?;
            exact_id(&family.query(&db, sid, "SELECT id FROM layout_boundary_audit")?, 99);
            assert!(db.query("SELECT id FROM layout_boundary_audit", &[])?.is_empty());
            let (arrived_tx, arrived_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let (invalid_tx, invalid_rx) = mpsc::channel();
            let reader_db = Arc::clone(&db);
            let reader = std::thread::spawn(move || {
                let armed = super::next_after_scan_layout_check(ROWS, move || {
                    arrived_tx
                        .send(())
                        .map_err(|error| Error::internal(error.to_string()))?;
                    release_rx
                        .recv_timeout(WAIT)
                        .map_err(|error| Error::internal(error.to_string()))?;
                    let invalid = layout_epochs::validate_statement()
                        .err()
                        .is_some_and(|error| layout_epochs::is_layout_changed(&error));
                    invalid_tx
                        .send(invalid)
                        .map_err(|error| Error::internal(error.to_string()))?;
                    Ok(())
                });
                if let Err(error) = armed {
                    return (Err(error), 0);
                }
                let mut attempts = 0;
                let result = reader_db.run_layout_statement(true, || {
                    attempts += 1;
                    family.query(&reader_db, sid, QUERY)
                });
                (result, attempts)
            });
            let arrived = arrived_rx.recv_timeout(WAIT);
            let written = if arrived.is_ok() {
                db.execute("ALTER TABLE layout_boundary_rows DROP COLUMN mid")
            } else {
                Err(Error::internal("explicit reader did not reach its real scan"))
            };
            let new_v = if written.is_ok() {
                db.storage
                    .catalog()
                    .get_table_schema(ROWS)
                    .map(|schema| schema.get_column_index("v"))
            } else {
                Ok(None)
            };
            let before_release = image(&db);
            let sequence = db.storage.db().latest_sequence_number();
            let _released = release_tx.send(());
            let read = reader.join();
            // Cleanup the real session even if the query/premise unexpectedly failed.
            let failed = family.query(&db, sid, "SELECT 1");
            let rollback = family.execute(&db, sid, "ROLLBACK");
            let (result, attempts) = joined(read, "explicit reader")?;
            arrived?;
            written?;
            rollback?;
            assert_eq!(old_v, Some(3));
            assert_eq!(new_v?, Some(2));
            assert!(
                invalid_rx.recv_timeout(WAIT)?,
                "the completed DROP must invalidate the recorded layout"
            );
            let error = result
                .err()
                .ok_or_else(|| std::io::Error::other("explicit stale scan unexpectedly succeeded"))?;
            assert_eq!(
                crate::protocol::postgres::handler::sqlstate_for_error(&error),
                "40001",
                "{error}"
            );
            assert_eq!(
                attempts, 1,
                "an explicit transaction must not execute its statement again"
            );
            assert!(failed.err().is_some_and(|error| error.is_failed_transaction()));
            assert_eq!(
                db.storage.db().latest_sequence_number(),
                sequence,
                "refusal and ROLLBACK must not write"
            );
            assert_eq!(image(&db)?, before_release?);
            assert!(db.query("SELECT id FROM layout_boundary_audit", &[])?.is_empty());
            exact_id(&family.query(&db, sid, QUERY)?, 5);
            family.execute(&db, sid, "INSERT INTO layout_boundary_audit VALUES(99)")?;
            exact_id(&family.query(&db, sid, "SELECT id FROM layout_boundary_audit")?, 99);
            family.execute(&db, sid, "DELETE FROM layout_boundary_audit WHERE id=99")?;
            db.destroy_session(sid)?;
            eprintln!("layout-boundary-explicit-cell family={family:?} history={history} attempts={attempts} rollback_clean=true");
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum Interrupt {
    Cancel,
    Deadline,
}

#[test]
fn real_lock_cancellation_and_deadline_keep_their_original_error_after_layout_moves() -> Check {
    for history in [false, true] {
        for family in [Family::Text, Family::Params] {
            for interrupt in [Interrupt::Cancel, Interrupt::Deadline] {
                let root = tempfile::tempdir()?;
                let db = open(config(root.path(), history))?;
                seed(&db)?;
                db.storage.table_locks().set_timeout(Duration::from_secs(5));
                let sid = db.create_session(
                    "layout-boundary-interrupt",
                    crate::session::IsolationLevel::ReadCommitted,
                )?;
                let pid = db.session_backend_pid(sid)?;
                let _cancel = db.arm_session_cancel(sid, 7241)?;
                if matches!(interrupt, Interrupt::Deadline) {
                    family.execute(&db, sid, "SET statement_timeout=100")?;
                }
                let (holder_ready_tx, holder_ready_rx) = mpsc::channel();
                let (holder_release_tx, holder_release_rx) = mpsc::channel();
                let locks = Arc::clone(db.storage.table_locks());
                let holder = std::thread::spawn(move || -> Result<()> {
                    let _guard = locks.exclusive(
                        crate::storage::table_locks::MAIN_LEVEL,
                        &[MUT],
                        "layout boundary interrupt control",
                        WAIT,
                    )?;
                    holder_ready_tx
                        .send(())
                        .map_err(|error| Error::internal(error.to_string()))?;
                    holder_release_rx
                        .recv_timeout(WAIT)
                        .map_err(|error| Error::internal(error.to_string()))?;
                    Ok(())
                });
                let holder_ready = holder_ready_rx.recv_timeout(WAIT);
                if let Err(error) = holder_ready {
                    let _release = holder_release_tx.send(());
                    let held = joined(holder.join(), "table holder")?;
                    held?;
                    return Err(error.into());
                }
                let (recorded_tx, recorded_rx) = mpsc::channel();
                let (invalid_tx, invalid_rx) = mpsc::channel();
                let done = Arc::new(AtomicBool::new(false));
                let reader_done = Arc::clone(&done);
                let reader_db = Arc::clone(&db);
                let reader = std::thread::spawn(move || {
                    let mut once = Some(recorded_tx);
                    let mut attempts = 0;
                    let result = reader_db.run_layout_statement(false, || {
                        attempts += 1;
                        reader_db.storage.catalog().get_table_schema(ROWS)?;
                        let recorded = once
                            .take()
                            .ok_or_else(|| Error::internal("an interrupted operation was incorrectly repeated"))?;
                        recorded.send(()).map_err(|error| Error::internal(error.to_string()))?;
                        // Enter the actual lock wait while this old read set is
                        // still valid. Starting nested SQL after the DROP would
                        // correctly refuse its stale context before lock entry.
                        let result = family.execute(&reader_db, sid, UPDATE);
                        invalid_tx
                            .send(
                                layout_epochs::validate_statement()
                                    .err()
                                    .is_some_and(|error| layout_epochs::is_layout_changed(&error)),
                            )
                            .map_err(|error| Error::internal(error.to_string()))?;
                        result
                    });
                    reader_done.store(true, Ordering::Release);
                    (result, attempts)
                });
                let recorded = recorded_rx.recv_timeout(WAIT);
                let started = Instant::now();
                let mut waiting = false;
                while started.elapsed() < WAIT && !done.load(Ordering::Acquire) {
                    if db
                        .storage
                        .table_locks()
                        .waiters(crate::storage::table_locks::MAIN_LEVEL, MUT)
                        .1
                        > 0
                    {
                        waiting = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                let written = if recorded.is_ok() && waiting {
                    db.execute("ALTER TABLE layout_boundary_rows DROP COLUMN mid")
                } else {
                    Err(Error::internal("interrupt SQL did not reach its real shared lock wait"))
                };
                let before_release = image(&db);
                let sequence = db.storage.db().latest_sequence_number();
                let cancelled = !matches!(interrupt, Interrupt::Cancel)
                    || (waiting && crate::session::scoped::request_cancel(pid, 7241));
                let read = reader.join();
                let _release = holder_release_tx.send(());
                let held = holder.join();
                let (result, attempts) = joined(read, "interrupted reader")?;
                joined(held, "table holder")??;
                recorded?;
                let observed = result.as_ref().err().map(ToString::to_string);
                assert!(
                    waiting,
                    "the SQL write must actually wait on the held table; family={family:?} history={history} kind={interrupt:?} observed={observed:?} attempts={attempts}"
                );
                written?;
                assert!(cancelled, "the real backend cancel key must be accepted");
                assert!(
                    invalid_rx.recv_timeout(WAIT)?,
                    "a concurrent DROP must have invalidated the old read set"
                );
                let error = result
                    .err()
                    .ok_or_else(|| std::io::Error::other("interrupted SQL unexpectedly succeeded"))?;
                match interrupt {
                    Interrupt::Cancel => assert!(matches!(&error, Error::QueryCancelled(_)), "{error}"),
                    Interrupt::Deadline => assert!(matches!(&error, Error::QueryTimeout(_)), "{error}"),
                }
                assert_eq!(
                    crate::protocol::postgres::handler::sqlstate_for_error(&error),
                    "57014",
                    "{error}"
                );
                assert!(
                    error
                        .to_string()
                        .contains("waiting for a lock on table layout_boundary_mut"),
                    "{error}"
                );
                assert_eq!(attempts, 1);
                assert_eq!(
                    db.storage.db().latest_sequence_number(),
                    sequence,
                    "an interrupted UPDATE cannot write"
                );
                assert_eq!(image(&db)?, before_release?);
                family.execute(&db, sid, "SET statement_timeout=0")?;
                let rows = family.query(&db, sid, "SELECT v FROM layout_boundary_mut WHERE id=1")?;
                assert_eq!(rows.first().and_then(|row| row.values.first()), Some(&Value::Int4(10)));
                db.destroy_session(sid)?;
                eprintln!("layout-boundary-interrupt-cell family={family:?} history={history} kind={interrupt:?} attempts={attempts}");
            }
        }
    }
    Ok(())
}

#[cfg(feature = "internal-tests")]
#[derive(Clone, Copy, Debug)]
enum NativeFault {
    UnknownBefore,
    UnknownAfter,
    KnownAppliedLate,
    #[cfg(feature = "vector-persist")]
    Publication,
}

#[cfg(feature = "internal-tests")]
impl NativeFault {
    fn publication(self) -> bool {
        #[cfg(feature = "vector-persist")]
        {
            matches!(self, Self::Publication)
        }
        #[cfg(not(feature = "vector-persist"))]
        {
            let _ = self;
            false
        }
    }

    fn arm(self, db: &EmbeddedDatabase) {
        match self {
            Self::UnknownBefore => db.storage.write_gate().debug_fail_next_native_write_before_apply(),
            Self::UnknownAfter => db.storage.write_gate().debug_fail_next_native_write_after_apply(),
            Self::KnownAppliedLate => db.storage.debug_fail_next_applied_commit(),
            #[cfg(feature = "vector-persist")]
            Self::Publication => {}
        }
    }
}

#[cfg(feature = "internal-tests")]
fn native_error_case(history: bool, family: Family, fault: NativeFault) -> Check {
    let root = tempfile::tempdir()?;
    let cfg = config(root.path(), history);
    let initial = open(cfg.clone())?;
    seed(&initial)?;
    if fault.publication() {
        initial.execute(
            "CREATE INDEX layout_boundary_hnsw ON layout_boundary_mut USING hnsw(emb) WITH(persistent=true)",
        )?;
    }
    // Settle legitimate first-open counter/catalog hydration before fault oracles.
    drop(initial);
    let db = open(cfg.clone())?;
    let sid = db.create_session("layout-boundary-native", crate::session::IsolationLevel::ReadCommitted)?;
    let key = crate::storage::ArtIndexManager::encode_key(&[Value::Int4(1)]);
    let physical = db
        .storage
        .art_indexes()
        .pk_index_lookup(MUT, &key)
        .ok_or_else(|| std::io::Error::other("the seeded row must own its primary key"))?;
    let row_key = format!("data:{MUT}:{physical}").into_bytes();
    family.execute(&db, sid, "BEGIN")?;
    family.execute(
        &db,
        sid,
        "UPDATE layout_boundary_mut SET v=20,emb='[0,1,0,0]' WHERE id=1",
    )?;
    let raw = db.storage.db();
    let before_commit = raw.latest_sequence_number();
    let before_row = raw.get(&row_key)?;
    let (arrived_tx, arrived_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let invalid = Arc::new(AtomicBool::new(false));
    let reader_invalid = Arc::clone(&invalid);
    let native_seen = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let reader_native = Arc::clone(&native_seen);
    let reader_db = Arc::clone(&db);
    let reader = std::thread::spawn(move || {
        let mut once = Some((arrived_tx, release_rx));
        let mut attempts = 0;
        let result = reader_db.run_layout_statement(false, || {
            attempts += 1;
            let (schema, stamp) = layout_epochs::collect_stamp(|| reader_db.storage.catalog().get_table_schema(ROWS));
            schema?;
            let (arrived, release) = once
                .take()
                .ok_or_else(|| Error::internal("a native-error COMMIT was incorrectly repeated"))?;
            let raw = reader_db.storage.db();
            let gate = Arc::clone(reader_db.storage.write_gate());
            let hook_invalid = Arc::clone(&reader_invalid);
            reader_db.storage.debug_next_commit_after_barrier(move || {
                // Publication has already failed if this is the cap case. Restore
                // the limit before the unrelated writer's legitimate DDL group.
                gate.debug_set_write_group_ceiling(None);
                let sent = arrived.send((raw.latest_sequence_number(), stamp.clone())).is_ok();
                let released = release.recv_timeout(WAIT).is_ok();
                hook_invalid.store(sent && released && !stamp.is_current(), Ordering::Release);
            });
            if fault.publication() {
                let raw = reader_db.storage.db();
                let gate = Arc::clone(reader_db.storage.write_gate());
                let seen = Arc::clone(&reader_native);
                reader_db.storage.debug_next_after_native_transaction_write(move || {
                    seen.store(raw.latest_sequence_number(), Ordering::Release);
                    gate.debug_set_write_group_ceiling(Some(64));
                });
            }
            fault.arm(&reader_db);
            family.execute(&reader_db, sid, "COMMIT")
        });
        (result, attempts)
    });
    let arrived = arrived_rx.recv_timeout(WAIT);
    let written = if arrived.is_ok() {
        db.execute("ALTER TABLE layout_boundary_rows DROP COLUMN mid")
    } else {
        Err(Error::internal("the real commit did not reach its post-native barrier"))
    };
    let after_ddl = raw.latest_sequence_number();
    let after_ddl_image = image(&db);
    let _release = release_tx.send(());
    let read = reader.join();
    // NativeUnknown/Applied has ended the session transaction; release the
    // connection even when a premise/result assertion will subsequently fail.
    let cleanup = db.destroy_session(sid);
    let (result, attempts) = joined(read, "native error reader")?;
    cleanup?;
    let (native_cut, old_stamp) = arrived?;
    written?;
    let after_ddl_image = after_ddl_image?;
    assert!(
        invalid.load(Ordering::Acquire),
        "a real completed DROP must move the captured pre-COMMIT stamp"
    );
    assert_eq!(attempts, 1);
    let error = result
        .err()
        .ok_or_else(|| std::io::Error::other("the armed real COMMIT unexpectedly succeeded"))?;
    assert!(
        !layout_epochs::is_layout_changed(&error),
        "the original native/publication failure must survive: {error}"
    );
    match fault {
        NativeFault::UnknownBefore => {
            assert!(
                error
                    .to_string()
                    .contains("injected native write error before apply; outcome is unknown"),
                "{error}"
            );
            assert_eq!(native_cut, before_commit);
        }
        NativeFault::UnknownAfter => {
            assert!(
                error
                    .to_string()
                    .contains("injected native write error after apply; outcome is unknown"),
                "{error}"
            );
            assert!(native_cut > before_commit);
        }
        NativeFault::KnownAppliedLate => {
            assert_eq!(
                error.to_string(),
                "Transaction error: injected durability failure after the commit applied"
            );
            assert!(native_cut > before_commit);
        }
        #[cfg(feature = "vector-persist")]
        NativeFault::Publication => {
            assert!(error.is_applied_publication_failure(), "{error}");
            assert!(
                error.to_string().contains(crate::BRANCH_MATERIALIZE_TOO_LARGE_MARKER),
                "{error}"
            );
            assert!(
                error.to_string().contains("row changes were already applied"),
                "{error}"
            );
            assert_eq!(crate::protocol::postgres::handler::sqlstate_for_error(&error), "58000");
            assert_eq!(
                native_cut,
                native_seen.load(Ordering::Acquire),
                "graph refusal did no error-path native repair"
            );
            assert!(native_cut > before_commit);
        }
    }
    assert_eq!(
        raw.latest_sequence_number(),
        after_ddl,
        "after the legitimate DDL no failed-COMMIT repair may write"
    );
    assert_eq!(image(&db)?, after_ddl_image);
    let applied = !matches!(fault, NativeFault::UnknownBefore);
    let bytes = raw
        .get(&row_key)?
        .ok_or_else(|| std::io::Error::other("seeded row vanished"))?;
    let tuple: Tuple = bincode::deserialize(&bytes)?;
    let value = if applied { 20 } else { 10 };
    let vector = if applied {
        vec![0.0, 1.0, 0.0, 0.0]
    } else {
        vec![1.0, 0.0, 0.0, 0.0]
    };
    assert_eq!(
        tuple.values,
        vec![Value::Int4(1), Value::Int4(value), Value::Vector(vector)]
    );
    if !applied {
        assert_eq!(Some(bytes), before_row);
    }
    if matches!(fault, NativeFault::UnknownBefore | NativeFault::UnknownAfter) || fault.publication() {
        // An actual quarantine Storage error must also beat a stale cached-plan
        // read set; use the genuinely captured stamp, not a made-up enum error.
        let original = db
            .query("SELECT v FROM layout_boundary_mut WHERE id=1", &[])
            .err()
            .ok_or_else(|| std::io::Error::other("an uncertain/faulted table stayed readable"))?;
        assert!(matches!(&original, Error::Storage(_)), "{original}");
        let mut guarded_attempts = 0;
        let guarded = db.run_layout_statement(false, || {
            guarded_attempts += 1;
            old_stamp.record();
            assert!(!layout_epochs::statement_still_valid());
            db.query("SELECT v FROM layout_boundary_mut WHERE id=1", &[])
        });
        let guarded = guarded
            .err()
            .ok_or_else(|| std::io::Error::other("a quarantined stale-plan read succeeded"))?;
        assert_eq!(guarded.to_string(), original.to_string());
        assert_eq!(guarded_attempts, 1);
        assert_eq!(raw.latest_sequence_number(), after_ddl);
        assert_eq!(image(&db)?, after_ddl_image);
    }
    drop(raw);
    drop(db);
    let reopened = open(cfg)?;
    let rows = reopened.query("SELECT v FROM layout_boundary_mut WHERE id=1", &[])?;
    assert_eq!(
        rows.first().and_then(|row| row.values.first()),
        Some(&Value::Int4(value))
    );
    #[cfg(feature = "vector-persist")]
    if fault.publication() {
        let graph = reopened.storage.vector_indexes();
        let stats = graph.get_index_stats("layout_boundary_hnsw")?;
        assert!(stats.quantization.contains("Persistent"));
        let found = graph.search("layout_boundary_hnsw", &vec![0.0, 1.0, 0.0, 0.0], 2)?;
        assert_eq!(found.len(), 1);
        assert!(found
            .first()
            .is_some_and(|(row, distance)| *row == physical && distance.abs() < 1e-6));
    }
    eprintln!("layout-boundary-native-cell family={family:?} history={history} fault={fault:?} attempts={attempts} applied={applied} reopen=true");
    Ok(())
}

#[cfg(feature = "internal-tests")]
#[test]
fn real_native_unknown_and_known_applied_failures_survive_layout_changes_once() -> Check {
    for history in [false, true] {
        for family in [Family::Text, Family::Params] {
            for fault in [
                NativeFault::UnknownBefore,
                NativeFault::UnknownAfter,
                NativeFault::KnownAppliedLate,
            ] {
                native_error_case(history, family, fault)?;
            }
        }
    }
    Ok(())
}

#[cfg(all(feature = "internal-tests", feature = "vector-persist"))]
#[test]
fn real_applied_graph_publication_error_beats_a_moved_layout_and_reopens_from_row_truth() -> Check {
    for history in [false, true] {
        for family in [Family::Text, Family::Params] {
            native_error_case(history, family, NativeFault::Publication)?;
        }
    }
    Ok(())
}
