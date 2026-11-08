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
