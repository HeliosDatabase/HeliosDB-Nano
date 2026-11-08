//! Abandoning a public RAII transaction must undo its eager index mutations.
use heliosdb_nano::{Config, EmbeddedDatabase, Error, Result, Value};

#[test]
fn abandoned_raii_handle_restores_committed_rows_and_enforcing_keys() {
    let mut failures = Vec::new();
    for history in [false, true] {
        for operation in ["insert", "delete", "move"] {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                let mut config = Config::default();
                config.storage.memory_only = true;
                config.storage.path = None;
                config.storage.time_travel_enabled = history;
                config.storage.version_gc_interval_secs = Some(0);
                config.session.lock_timeout_ms = 100;
                let db = EmbeddedDatabase::with_config(config)?;
                db.execute("CREATE TABLE raii_drop_rows(id INT PRIMARY KEY, v INT)")?;
                if operation != "insert" {
                    db.execute("INSERT INTO raii_drop_rows VALUES(1, 10)")?;
                }
                {
                    let tx = db.begin_transaction()?;
                    tx.execute(match operation {
                        "insert" => "INSERT INTO raii_drop_rows VALUES(1, 10)",
                        "delete" => "DELETE FROM raii_drop_rows WHERE id=1",
                        _ => "UPDATE raii_drop_rows SET id=2 WHERE id=1",
                    })?;
                    // The public handle is deliberately abandoned, with no
                    // explicit commit or rollback, as on an early return.
                }
                let committed = db.query("SELECT id, v FROM raii_drop_rows ORDER BY id", &[])?;
                let expected = usize::from(operation != "insert");
                assert_eq!(
                    committed.len(),
                    expected,
                    "committed rows history={history} op={operation}"
                );
                let count = db.query("SELECT COUNT(*) FROM raii_drop_rows", &[])?;
                assert_eq!(count.len(), 1);
                let counted = match count.first().and_then(|row| row.values.first()) {
                    Some(Value::Int8(value)) => *value,
                    Some(Value::Int4(value)) => i64::from(*value),
                    _ => return Err(Error::storage("COUNT did not return one integer")),
                };
                assert_eq!(
                    counted, expected as i64,
                    "eager cardinality after dropped handle history={history} op={operation}"
                );
                if operation != "insert" {
                    assert_eq!(
                        db.query("SELECT id FROM raii_drop_rows WHERE id=1", &[])?.len(),
                        1,
                        "original enforcing key must remain after dropped handle"
                    );
                }
                let reusable = if operation == "insert" { 1 } else { 2 };
                db.execute(&format!("INSERT INTO raii_drop_rows VALUES({reusable}, 99)"))?;
                assert_eq!(
                    db.query(&format!("SELECT id FROM raii_drop_rows WHERE id={reusable}"), &[])?
                        .len(),
                    1
                );
                Ok(())
            }));
            let passed = matches!(&outcome, Ok(Ok(())));
            eprintln!("raii-drop-cell history={history} operation={operation} passed={passed}");
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failures.push(format!("history={history} operation={operation}: {error}")),
                Err(_) => failures.push(format!("history={history} operation={operation}: native assertion")),
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[cfg(feature = "vector-search")]
#[test]
fn abandoned_raii_handle_restores_unique_and_vector_indexes_during_unwind() -> Result<()> {
    for history in [false, true] {
        for unwind in [false, true] {
            for operation in ["insert", "delete", "move"] {
                let mut config = Config::default();
                config.storage.memory_only = true;
                config.storage.path = None;
                config.storage.time_travel_enabled = history;
                config.storage.version_gc_interval_secs = Some(0);
                let db = EmbeddedDatabase::with_config(config)?;
                db.execute("CREATE TABLE abandoned_indexes(id INT PRIMARY KEY, u INT UNIQUE, embedding VECTOR(3))")?;
                db.execute("INSERT INTO abandoned_indexes VALUES(1,10,'[0,0,0]'),(100,1000,'[10,0,0]')")?;
                db.execute("CREATE INDEX abandoned_vector ON abandoned_indexes USING hnsw(embedding vector_l2_ops)")?;
                let ending = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                    let tx = db.begin_transaction()?;
                    assert_eq!(
                        tx.execute(match operation {
                            "insert" => "INSERT INTO abandoned_indexes VALUES(2,20,'[100,0,0]')",
                            "delete" => "DELETE FROM abandoned_indexes WHERE id=1",
                            _ => "UPDATE abandoned_indexes SET id=2,u=20,embedding='[100,0,0]' WHERE id=1",
                        })?,
                        1,
                        "the intended write must succeed before abandonment"
                    );
                    if unwind {
                        // A real Rust stack unwind exercises the handle's field
                        // cleanup, including its eager ART and HNSW undo.
                        std::panic::resume_unwind(Box::new("abandoned public transaction"));
                    }
                    Ok(())
                }));
                match ending {
                    Ok(result) => {
                        result?;
                        assert!(!unwind);
                    }
                    Err(payload) => {
                        assert!(unwind);
                        assert_eq!(
                            payload.downcast_ref::<&str>(),
                            Some(&"abandoned public transaction"),
                            "only the planned unwind after a successful write is a valid premise"
                        );
                    }
                }
                let rows = db.query("SELECT id,u FROM abandoned_indexes ORDER BY id", &[])?;
                let actual: Vec<_> = rows.into_iter().map(|row| row.values).collect();
                assert_eq!(
                    actual,
                    vec![
                        vec![Value::Int4(1), Value::Int4(10)],
                        vec![Value::Int4(100), Value::Int4(1000)]
                    ]
                );
                assert_eq!(db.query("SELECT id FROM abandoned_indexes WHERE id=1", &[])?.len(), 1);
                assert!(db.query("SELECT id FROM abandoned_indexes WHERE id=2", &[])?.is_empty());
                assert_eq!(
                    db.storage
                        .vector_indexes()
                        .get_index_stats("abandoned_vector")?
                        .num_vectors,
                    2
                );
                let nearest = db.query(
                    "SELECT id FROM abandoned_indexes ORDER BY embedding <-> '[0,0,0]' LIMIT 1",
                    &[],
                )?;
                assert_eq!(
                    nearest.first().map(|row| row.values.as_slice()),
                    Some([Value::Int4(1)].as_slice())
                );
                // Restoring row1's UNIQUE key must enforce it, and undoing
                // row2's key must permit a genuinely new committed row2.
                let duplicate = db.execute("INSERT INTO abandoned_indexes VALUES(99,10,'[5,0,0]')");
                assert!(
                    matches!(duplicate, Err(Error::ConstraintViolation(_))),
                    "original unique key must still be enforced: {duplicate:?}"
                );
                db.execute("INSERT INTO abandoned_indexes VALUES(2,20,'[100,0,0]')")?;
                assert_eq!(db.query("SELECT id FROM abandoned_indexes WHERE id=2", &[])?.len(), 1);
                assert_eq!(
                    db.storage
                        .vector_indexes()
                        .get_index_stats("abandoned_vector")?
                        .num_vectors,
                    3
                );
                eprintln!("raii-drop-index-cell history={history} unwind={unwind} operation={operation} passed=true");
            }
        }
    }
    Ok(())
}
