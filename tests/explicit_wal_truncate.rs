//! Explicit logical replay must publish durable TRUNCATE to every read path.
use heliosdb_nano::{Config, EmbeddedDatabase, Error, Result, Value};

fn open(path: &std::path::Path, history: bool) -> Result<EmbeddedDatabase> {
    let mut config = Config::default();
    config.storage.memory_only = false;
    config.storage.path = Some(path.to_path_buf());
    config.storage.time_travel_enabled = history;
    config.storage.version_gc_interval_secs = Some(0);
    config.storage.wal_enabled = true;
    config.storage.wal_checkpoint_on_close = false;
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
