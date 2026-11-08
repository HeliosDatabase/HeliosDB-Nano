//! Explicit logical replay must publish durable TRUNCATE to every read path.
use heliosdb_nano::{Config, EmbeddedDatabase, Error, Result, Value};

fn open(path: &std::path::Path, history: bool) -> Result<EmbeddedDatabase> {
    open_with_policy(path, history, heliosdb_nano::config::DestructiveDdlReplay::Apply)
}

fn open_with_policy(
    path: &std::path::Path,
    history: bool,
    policy: heliosdb_nano::config::DestructiveDdlReplay,
) -> Result<EmbeddedDatabase> {
    let mut config = Config::default();
    config.storage.memory_only = false;
    config.storage.path = Some(path.to_path_buf());
    config.storage.time_travel_enabled = history;
    config.storage.version_gc_interval_secs = Some(0);
    config.storage.wal_enabled = true;
    config.storage.wal_checkpoint_on_close = false;
    config.storage.wal_replay_destructive_ddl = policy;
    EmbeddedDatabase::with_config(config)
}

fn seed(db: &EmbeddedDatabase) -> Result<()> {
    #[cfg(feature = "vector-search")]
    {
        db.execute("CREATE TABLE replay_rows(id INT PRIMARY KEY,u INT UNIQUE,v TEXT,embedding VECTOR(3))")?;
        db.execute("INSERT INTO replay_rows VALUES(1,11,'one','[1,0,0]'),(2,22,'two','[0,1,0]')")?;
        db.execute("CREATE INDEX replay_ann ON replay_rows USING hnsw(embedding vector_l2_ops)")?;
    }
    #[cfg(not(feature = "vector-search"))]
    {
        db.execute("CREATE TABLE replay_rows(id INT PRIMARY KEY,u INT UNIQUE,v TEXT)")?;
        db.execute("INSERT INTO replay_rows VALUES(1,11,'one'),(2,22,'two')")?;
    }
    db.execute("CREATE INDEX replay_v ON replay_rows(v)")?;
    for _ in 0..3 {
        assert_eq!(db.query("SELECT id FROM replay_rows ORDER BY id", &[])?.len(), 2);
        assert_eq!(db.query("SELECT id FROM replay_rows WHERE id=1", &[])?.len(), 1);
        assert_eq!(db.query("SELECT id FROM replay_rows WHERE v='one'", &[])?.len(), 1);
        assert_eq!(count(db)?, 2);
    }
    Ok(())
}

fn count(db: &EmbeddedDatabase) -> Result<i64> {
    let rows = db.query("SELECT COUNT(*) FROM replay_rows", &[])?;
    match rows.first().and_then(|row| row.values.first()) {
        Some(Value::Int8(value)) => Ok(*value),
        Some(Value::Int4(value)) => Ok(i64::from(*value)),
        _ => Err(Error::storage("explicit replay COUNT requires one integer")),
    }
}

fn vector_count(db: &EmbeddedDatabase) -> Result<Option<i64>> {
    #[cfg(feature = "vector-search")]
    {
        Ok(Some(
            db.storage.vector_indexes().get_index_stats("replay_ann")?.num_vectors,
        ))
    }
    #[cfg(not(feature = "vector-search"))]
    {
        let _ = db;
        Ok(None)
    }
}

fn queue(db: &EmbeddedDatabase) -> Result<(u64, u64)> {
    let after = db
        .storage
        .wal_lsn()
        .ok_or_else(|| Error::storage("missing logical WAL"))?;
    db.storage.log_truncate("replay_rows")?;
    let latest = db
        .storage
        .wal_lsn()
        .ok_or_else(|| Error::storage("missing appended logical WAL"))?;
    assert_eq!(latest, after + 1);
    match db.storage.logical_wal_catchup(after)? {
        heliosdb_nano::storage::LogicalCatchup::Entries {
            latest: observed,
            entries,
        } => {
            assert_eq!(observed, latest);
            let [entry] = entries.as_slice() else {
                return Err(Error::storage("expected one explicit TRUNCATE"));
            };
            assert_eq!(entry.lsn, latest);
            assert!(
                matches!(&entry.operation,heliosdb_nano::storage::WalOperation::Truncate { table } if table=="replay_rows")
            );
        }
        _ => return Err(Error::storage("explicit history unavailable")),
    }
    Ok((after, latest))
}

fn assert_empty_and_reuse(db: &EmbeddedDatabase) -> Result<()> {
    let raw = db.storage.scan_table("replay_rows")?.len();
    let sql = db.query("SELECT id FROM replay_rows ORDER BY id", &[])?.len();
    let counted = count(db)?;
    let pk = db.query("SELECT id FROM replay_rows WHERE id=1", &[])?.len();
    let manual = db.query("SELECT id FROM replay_rows WHERE v='one'", &[])?.len();
    let vectors = vector_count(db)?;
    #[cfg(feature = "vector-search")]
    let reused = db.execute("INSERT INTO replay_rows VALUES(1,11,'one','[1,0,0]')");
    #[cfg(not(feature = "vector-search"))]
    let reused = db.execute("INSERT INTO replay_rows VALUES(1,11,'one')");
    eprintln!("explicit-replay-truth raw={raw} sql={sql} count={counted} pk={pk} manual={manual} vectors={vectors:?} reused={reused:?}");
    assert_eq!((raw, sql, counted, pk, manual), (0, 0, 0, 0, 0));
    assert!(vectors.is_none_or(|value| value == 0));
    assert_eq!(reused?, 1, "old PK/UNIQUE values must be reusable");
    assert_eq!(db.query("SELECT id FROM replay_rows WHERE id=1", &[])?.len(), 1);
    assert_eq!(count(db)?, 1);
    assert!(vector_count(db)?.is_none_or(|value| value == 1));
    Ok(())
}

#[test]
fn explicit_truncate_publishes_cached_rows_counts_keys_and_vectors() {
    let mut failures = Vec::new();
    for history in [false, true] {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
            let dir = tempfile::tempdir().map_err(|error| Error::storage(error.to_string()))?;
            let db = open(dir.path(), history)?;
            seed(&db)?;
            let (after, latest) = queue(&db)?;
            assert_eq!(db.storage.replay_wal_after(after)?, 0, "DDL is not a batched row");
            assert_eq!(db.storage.wal_lsn(), Some(latest), "replay must not log itself");
            assert_empty_and_reuse(&db)
        }));
        eprintln!(
            "explicit-replay-cell history={history} ok={}",
            matches!(&outcome, Ok(Ok(())))
        );
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(error)) => failures.push(format!("history={history}: {error}")),
            Err(_) => failures.push(format!("history={history}: native assertion")),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}

fn image(db: &EmbeddedDatabase) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut options = rocksdb::ReadOptions::default();
    options.set_total_order_seek(true);
    db.storage
        .db()
        .iterator_opt(rocksdb::IteratorMode::Start, options)
        .map(|row| {
            row.map(|(key, value)| (key.into_vec(), value.into_vec()))
                .map_err(Error::from)
        })
        .collect()
}

fn assert_seeded(db: &EmbeddedDatabase) -> Result<()> {
    assert_eq!(db.storage.scan_table("replay_rows")?.len(), 2);
    assert_eq!(db.query("SELECT id FROM replay_rows ORDER BY id", &[])?.len(), 2);
    assert_eq!(count(db)?, 2);
    assert_eq!(db.query("SELECT id FROM replay_rows WHERE id=1", &[])?.len(), 1);
    assert_eq!(db.query("SELECT id FROM replay_rows WHERE v='one'", &[])?.len(), 1);
    assert!(vector_count(db)?.is_none_or(|value| value == 2));
    Ok(())
}

#[test]
fn explicit_truncate_refuse_policy_preserves_every_row_key_cache_and_native_byte() -> Result<()> {
    for history in [false, true] {
        let dir = tempfile::tempdir().map_err(|error| Error::storage(error.to_string()))?;
        let db = open_with_policy(dir.path(), history, heliosdb_nano::config::DestructiveDdlReplay::Refuse)?;
        seed(&db)?;
        let (after, latest) = queue(&db)?;
        let before = image(&db)?;
        let sequence = db.storage.db().latest_sequence_number();
        assert_eq!(db.storage.replay_wal_after(after)?, 0);
        assert_eq!(db.storage.wal_lsn(), Some(latest));
        assert_eq!(image(&db)?, before);
        assert_eq!(db.storage.db().latest_sequence_number(), sequence);
        assert_seeded(&db)?;
        eprintln!("explicit-replay-policy-cell history={history} refused=true native_unchanged=true");
    }
    Ok(())
}

#[cfg(feature = "internal-tests")]
#[test]
fn explicit_truncate_native_outcomes_preserve_truth_no_relogging_and_reopen() -> Result<()> {
    let mut failures = Vec::new();
    for history in [false, true] {
        for fault in ["cap", "applied", "unknown"] {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                let dir = tempfile::tempdir().map_err(|error| Error::storage(error.to_string()))?;
                {
                    let db = open(dir.path(), history)?;
                    db.storage.set_index_snapshots_on_close(false);
                    db.storage.set_row_counter_flush_on_close(false);
                    seed(&db)?;
                    db.execute("CREATE TABLE replay_healthy(id INT PRIMARY KEY)")?;
                    let (after, latest) = queue(&db)?;
                    let before = image(&db)?;
                    let sequence = db.storage.db().latest_sequence_number();
                    match fault {
                        "cap" => db.storage.debug_set_write_group_ceiling(Some(64)),
                        "applied" => db.storage.debug_fail_next_applied_commit(),
                        "unknown" => db.storage.write_gate().debug_fail_next_native_write_after_apply(),
                        _ => return Err(Error::internal("unknown test fault")),
                    }
                    let replay = db.storage.replay_wal_after(after);
                    db.storage.debug_set_write_group_ceiling(None);
                    assert!(replay.is_err(), "{fault}: injected refusal must reach replay caller");
                    assert_eq!(
                        db.storage.wal_lsn(),
                        Some(latest),
                        "{fault}: replay must not log itself"
                    );
                    assert!(!db.storage.has_uncommitted_writes());
                    match fault {
                        "cap" => {
                            assert_eq!(image(&db)?, before);
                            assert_eq!(db.storage.db().latest_sequence_number(), sequence);
                            assert_seeded(&db)?;
                            assert_eq!(db.storage.replay_wal_after(after)?, 0, "exact retry");
                            assert_empty_and_reuse(&db)?;
                        }
                        "applied" => {
                            assert!(db.storage.db().latest_sequence_number() > sequence);
                            assert_empty_and_reuse(&db)?;
                        }
                        "unknown" => {
                            assert!(db.storage.db().latest_sequence_number() > sequence);
                            assert_eq!(
                                image(&db)?
                                    .iter()
                                    .filter(|(key, _)| key.starts_with(b"data:replay_rows:"))
                                    .count(),
                                0
                            );
                            assert!(db.query("SELECT id FROM replay_rows ORDER BY id", &[]).is_err());
                            assert!(db.query("SELECT id FROM replay_rows WHERE id=1", &[]).is_err());
                            assert!(db.query_params("SELECT COUNT(*) FROM replay_rows", &[]).is_err());
                            assert!(db.storage.art_indexes().list_table_indexes("replay_rows").is_empty());
                        }
                        _ => return Err(Error::internal("unknown test fault")),
                    }
                    // The failed replay must release both global replay flags. A
                    // healthy table still admits and records a normal live write.
                    assert_eq!(db.execute("INSERT INTO replay_healthy VALUES(9)")?, 1);
                    assert!(db.storage.wal_lsn().is_some_and(|lsn| lsn > latest));
                }
                let db = open(dir.path(), history)?;
                assert_eq!(db.query("SELECT id FROM replay_healthy", &[])?.len(), 1);
                if fault == "unknown" {
                    assert_empty_and_reuse(&db)?;
                } else {
                    assert_eq!(count(&db)?, 1);
                    assert_eq!(db.query("SELECT id FROM replay_rows WHERE id=1", &[])?.len(), 1);
                    assert!(vector_count(&db)?.is_none_or(|value| value == 1));
                }
                Ok(())
            }));
            eprintln!(
                "explicit-replay-outcome-cell history={history} fault={fault} ok={}",
                matches!(&outcome, Ok(Ok(())))
            );
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failures.push(format!("history={history} fault={fault}: {error}")),
                Err(_) => failures.push(format!("history={history} fault={fault}: native assertion")),
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
    Ok(())
}

#[test]
fn retained_main_truncate_is_independent_of_callers_branch_catalog() -> Result<()> {
    let mut failures = Vec::new();
    for history in [false, true] {
        for shadow in [false, true] {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                let dir = tempfile::tempdir().map_err(|error| Error::storage(error.to_string()))?;
                let db = open(dir.path(), history)?;
                seed(&db)?;
                db.create_branch("replay_branch")?;
                db.with_request_branch_scope("replay_branch", || {
                    db.execute("DROP TABLE replay_rows")?;
                    if shadow {
                        db.execute("CREATE TABLE replay_rows(id INT PRIMARY KEY,u INT UNIQUE,v TEXT)")?;
                        db.execute("INSERT INTO replay_rows VALUES(9,99,'branch')")?;
                    }
                    Ok(())
                })?;
                let (after, latest) = queue(&db)?;
                db.with_request_branch_scope("replay_branch", || {
                    assert_eq!(db.storage.replay_wal_after(after)?, 0);
                    // The internal main scope must have restored this caller's
                    // branch before control returns from explicit replay.
                    if shadow {
                        let rows = db.query("SELECT id FROM replay_rows", &[])?;
                        assert_eq!(
                            rows.first().map(|row| row.values.as_slice()),
                            Some([Value::Int4(9)].as_slice())
                        );
                        assert_eq!(rows.len(), 1);
                    } else {
                        assert!(db.query("SELECT id FROM replay_rows", &[]).is_err());
                    }
                    Ok(())
                })?;
                assert_eq!(db.storage.wal_lsn(), Some(latest));
                assert_empty_and_reuse(&db)?;
                Ok(())
            }));
            eprintln!(
                "explicit-replay-branch-cell history={history} shadow={shadow} ok={}",
                matches!(&outcome, Ok(Ok(())))
            );
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failures.push(format!("history={history} shadow={shadow}: {error}")),
                Err(_) => failures.push(format!("history={history} shadow={shadow}: native assertion")),
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
    Ok(())
}
