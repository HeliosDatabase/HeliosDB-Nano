use super::*;
use crate::{Config, EmbeddedDatabase, Tuple, Value};
use std::sync::mpsc;
use std::time::Duration;

fn raw_image(db: &EmbeddedDatabase) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut options = rocksdb::ReadOptions::default();
    options.set_total_order_seek(true);
    db.storage
        .db()
        .iterator_opt(rocksdb::IteratorMode::Start, options)
        .map(|entry| {
            entry
                .map(|(key, value)| (key.to_vec(), value.to_vec()))
                .map_err(|error| Error::storage(error.to_string()))
        })
        .collect()
}

fn ids(rows: &[Tuple]) -> Vec<Value> {
    rows.iter().filter_map(|row| row.values.first().cloned()).collect()
}

fn owner_cell(history: bool, bound: bool, commit: bool) -> Result<()> {
    let mut config = Config::in_memory();
    config.storage.time_travel_enabled = history;
    let db = EmbeddedDatabase::with_config(config)?;
    db.execute("CREATE TABLE counter_owner (id INT PRIMARY KEY, v INT)")?;
    db.execute("INSERT INTO counter_owner VALUES (1, 10)")?;
    db.execute("CREATE BRANCH b AS OF NOW")?;
    db.storage.debug_set_table_lock_timeout(Duration::from_millis(40));
    let session = db.create_session("branch_counter", crate::session::IsolationLevel::ReadCommitted)?;
    db.execute_for_session(session, "USE BRANCH b")?;

    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let observed = std::thread::scope(|scope| -> Result<_> {
        let storage = &db.storage;
        let holding = scope.spawn(move || -> Result<()> {
            let _main = storage.lock_tables_exclusive(
                super::super::table_locks::MAIN_LEVEL,
                &["counter_owner"],
                "counter-owner control main DDL",
            )?;
            ready_tx.send(()).map_err(|error| Error::internal(error.to_string()))?;
            release_rx
                .recv_timeout(Duration::from_secs(10))
                .map_err(|error| Error::internal(error.to_string()))?;
            Ok(())
        });
        let body = (|| -> Result<_> {
            ready_rx
                .recv_timeout(Duration::from_secs(10))
                .map_err(|error| Error::internal(error.to_string()))?;
            let before = raw_image(&db)?;
            let sequence = db.storage.db().latest_sequence_number();
            let raw = db.storage.begin_autocommit_transaction()?;
            let refusal = db.with_request_branch_scope("b", || raw.stage_row_counter("counter_owner", 999));
            let raw_untouched = raw.row_counter_stages.read().is_empty()
                && raw.written_tables.is_empty()
                && before == raw_image(&db)?
                && sequence == db.storage.db().latest_sequence_number();
            raw.rollback()?;

            db.begin_transaction_for_session(session)?;
            if bound {
                db.execute_params_for_session(
                    session,
                    "INSERT INTO counter_owner VALUES ($1, $2), ($3, $4)",
                    &[Value::Int4(2), Value::Int4(20), Value::Int4(3), Value::Int4(30)],
                )?;
            } else {
                db.execute_for_session(session, "INSERT INTO counter_owner VALUES (2, 20), (3, 30)")?;
            }
            let staged = db
                .query_with_columns_for_session(session, "SELECT id FROM counter_owner ORDER BY id")?
                .0;
            if commit {
                db.commit_transaction_for_session(session)?;
            } else {
                db.rollback_transaction_for_session(session)?;
            }
            Ok((refusal, raw_untouched, staged))
        })();
        let _ = release_tx.send(());
        holding
            .join()
            .map_err(|_| Error::internal("main counter owner panicked"))??;
        body
    })?;

    let (refusal, raw_untouched, staged) = observed;
    let raw_error = refusal
        .err()
        .ok_or_else(|| Error::internal("raw counter bypassed main ownership"))?;
    if !raw_error.to_string().contains(crate::error::TABLE_LOCK_TIMEOUT_MARKER) || !raw_untouched {
        return Err(Error::internal(format!(
            "raw counter refusal changed state: {raw_error:?}; untouched={raw_untouched}"
        )));
    }
    let all = vec![Value::Int4(1), Value::Int4(2), Value::Int4(3)];
    let main = db.query("SELECT id FROM counter_owner ORDER BY id", &[])?;
    let branch = db
        .query_with_columns_for_session(session, "SELECT id FROM counter_owner ORDER BY id")?
        .0;
    if ids(&staged) != all
        || ids(&main) != vec![Value::Int4(1)]
        || ids(&branch) != if commit { all } else { vec![Value::Int4(1)] }
    {
        return Err(Error::internal(format!(
            "counter owner row truth: staged={staged:?}; main={main:?}; branch={branch:?}"
        )));
    }
    let counter = db
        .storage
        .get(&b"counter:counter_owner".to_vec())?
        .ok_or_else(|| Error::internal("counter missing"))?;
    let high_water: u64 = bincode::deserialize(&counter).map_err(|error| Error::storage(error.to_string()))?;
    if commit && !(3..999).contains(&high_water) {
        return Err(Error::internal(format!("managed counter high water is {high_water}")));
    }
    db.destroy_session(session)?;
    Ok(())
}

#[test]
fn managed_branch_counter_uses_its_level_and_raw_counter_stays_main_owned() -> Result<()> {
    let mut failures = Vec::new();
    for history in [false, true] {
        for bound in [false, true] {
            for commit in [false, true] {
                let result = owner_cell(history, bound, commit);
                eprintln!(
                    "branch-counter-owner-cell history={history} bound={bound} commit={commit} ok={}",
                    result.is_ok()
                );
                if let Err(error) = result {
                    failures.push(format!("history={history} bound={bound} commit={commit}: {error:?}"));
                }
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(Error::internal(failures.join("\n")))
    }
}
