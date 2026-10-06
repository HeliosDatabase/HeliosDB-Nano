//! External draft: public API schedule for PK/index-versus-row publication.
//! No production hooks, source edits, global HA state, ports, or user data.
//! SQL predicate integrity and raw storage lookup identity are separate verdicts.
use heliosdb_nano::{
    storage::{ArtIndexManager, WalOperation, WalSyncMode, WriteAheadLog},
    Config, EmbeddedDatabase, Tuple, Value,
};
use std::{
    sync::{mpsc, Arc},
    thread,
    time::{Duration, Instant},
};

const TABLE: &str = "replay_window";
const SECONDARY: &str = "replay_window_blocker";
const DEADLINE: Duration = Duration::from_secs(5);
type Rows = Vec<Vec<i32>>;

#[derive(Debug)]
struct Probe {
    requested_id: i32,
    description: String,
    result: Result<Rows, String>,
}
#[derive(Debug)]
struct Observation {
    from: i32,
    to: i32,
    boundary: &'static str,
    raw_at_boundary: Result<Option<Tuple>, String>,
    completed_while_blocker_held: bool,
    sql: Vec<Probe>,
    storage: Vec<Probe>,
    writer: Result<(), String>,
}

fn db(initial_id: i32) -> Arc<EmbeddedDatabase> {
    let mut config = Config::in_memory();
    config.storage.wal_enabled = true;
    config.storage.logical_wal_per_statement = true;
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    let db = Arc::new(EmbeddedDatabase::with_config(config).unwrap());
    db.execute("CREATE TABLE replay_window (id INT PRIMARY KEY, v INT)")
        .unwrap();
    // Register separately AFTER the PK. table_index_add preserves insertion
    // order; live replay therefore stages PK before attempting this tree lock.
    db.execute("CREATE INDEX replay_window_blocker ON replay_window(v)")
        .unwrap();
    db.execute(&format!(
        "INSERT INTO replay_window VALUES ({initial_id}, {})",
        initial_id * 10
    ))
    .unwrap();
    db
}

fn values(rows: Vec<Tuple>) -> Result<Rows, String> {
    rows.into_iter()
        .map(|row| {
            row.values
                .into_iter()
                .map(|value| match value {
                    Value::Int4(value) => Ok(value),
                    other => Err(format!("fixture expected INT values, got {other:?}")),
                })
                .collect()
        })
        .collect()
}

fn captured_update(primary: &EmbeddedDatabase, from: i32, to: i32) -> (WalOperation, Vec<u8>) {
    let before = primary.storage.wal_lsn().unwrap();
    primary
        .execute(&format!(
            "UPDATE replay_window SET id = {to}, v = {} WHERE id = {from}",
            to * 10
        ))
        .unwrap();
    let reader = WriteAheadLog::open(primary.storage.db(), WalSyncMode::Async).unwrap();
    let updates: Vec<_> = reader
        .replay_from(before)
        .unwrap()
        .into_iter()
        .filter_map(|entry| match entry.operation {
            operation @ WalOperation::Update { .. } => Some(operation),
            _ => None,
        })
        .collect();
    assert_eq!(
        updates.len(),
        1,
        "fixture must capture one actual SQL UPDATE row record"
    );
    let operation = updates.into_iter().next().unwrap();
    let key = match &operation {
        WalOperation::Update { table, key, .. } => {
            assert_eq!(table, TABLE);
            key.clone()
        }
        _ => unreachable!(),
    };
    (operation, key)
}

fn boundary(from: i32, to: i32) -> Observation {
    let primary = db(from);
    let standby = db(from);
    let (operation, storage_key) = captured_update(&primary, from, to);
    let new_key = ArtIndexManager::encode_key(&[Value::Int4(to)]);
    let old_key = ArtIndexManager::encode_key(&[Value::Int4(from)]);
    let original_id = standby.storage.art_indexes().pk_index_lookup(TABLE, &old_key).unwrap();
    assert_eq!(standby.storage.art_indexes().pk_index_lookup(TABLE, &new_key), None);
    let secondary = standby.storage.art_indexes().get_index(SECONDARY).unwrap();
    let blocker = secondary.write().unwrap();
    let (writer_tx, writer_rx) = mpsc::channel();
    let writer_storage = Arc::clone(&standby.storage);
    let writer_handle = thread::spawn(move || {
        let result = writer_storage
            .apply_replicated_operation(operation)
            .map_err(|error| error.to_string());
        let _ = writer_tx.send(result);
    });

    // No assertions, early returns or joins while holding the blocker. On a
    // baseline that performs raw puts only, the writer finishes without touching
    // this lock. That path is observed separately, never called a staged window.
    let deadline = Instant::now() + DEADLINE;
    let mut writer_done = None;
    let observed_boundary = loop {
        if standby.storage.art_indexes().pk_index_lookup(TABLE, &new_key) == Some(original_id) {
            break "new_pk_claim_visible_before_secondary";
        }
        match writer_rx.try_recv() {
            Ok(result) => {
                writer_done = Some(result);
                break "writer_completed_without_new_pk_claim";
            }
            Err(mpsc::TryRecvError::Disconnected) => break "writer_disconnected",
            Err(mpsc::TryRecvError::Empty) => (),
        }
        if Instant::now() >= deadline {
            break "boundary_timeout";
        }
        thread::sleep(Duration::from_millis(1));
    };
    let raw_at_boundary = standby
        .storage
        .get(&storage_key)
        .map_err(|error| error.to_string())
        .and_then(|raw| {
            raw.map(|bytes| bincode::deserialize::<Tuple>(&bytes).map_err(|error| error.to_string()))
                .transpose()
        });
    let (reader_tx, reader_rx) = mpsc::channel();
    let reader_db = Arc::clone(&standby);
    let reader_handle = thread::spawn(move || {
        let mut sql = Vec::new();
        for requested_id in [to, from] {
            // The existing fast-row PK predicate check is load-bearing. Test
            // both the SELECT * fast shape and an explicit projection/fallback.
            for projection in ["*", "id, v"] {
                let text = format!("SELECT {projection} FROM replay_window WHERE id = {requested_id}");
                let result = reader_db
                    .query(&text, &[])
                    .map_err(|error| error.to_string())
                    .and_then(values);
                sql.push(Probe {
                    requested_id,
                    description: text,
                    result,
                });
            }
        }
        // Run raw storage probes AFTER SQL, so their cache fills cannot create
        // the SQL symptom being investigated. Their own identity contract gets
        // a separate assertion/test; no SQL failure is inferred from these rows.
        let mut storage = Vec::new();
        for requested_id in [to, from] {
            let result = reader_db
                .storage
                .get_row_by_pk(TABLE, &Value::Int4(requested_id))
                .map_err(|error| error.to_string())
                .and_then(|row| values(row.into_iter().collect()));
            storage.push(Probe {
                requested_id,
                description: "StorageEngine::get_row_by_pk".into(),
                result,
            });
        }
        let _ = reader_tx.send((sql, storage));
    });
    let while_held = reader_rx.recv_timeout(DEADLINE);
    let completed_while_blocker_held = while_held.is_ok();

    // ALWAYS release the writer's secondary tree before joining either thread.
    // A SQL fallback may itself need that tree; a timeout is an inconclusive
    // schedule and remains a test failure, not a hidden post-commit observation.
    drop(blocker);
    let (sql, storage) = match while_held {
        Ok(probes) => probes,
        Err(_) => reader_rx
            .recv_timeout(DEADLINE)
            .expect("reader did not finish after blocker release"),
    };
    let writer = writer_done.unwrap_or_else(|| {
        writer_rx
            .recv_timeout(DEADLINE)
            .expect("writer did not finish after blocker release")
    });
    // Completion messages prove the workers have finished their storage/query
    // operations. Shared engines remain owned here while worker handles return.
    reader_handle.join().expect("reader panicked");
    writer_handle.join().expect("writer panicked");
    Observation {
        from,
        to,
        boundary: observed_boundary,
        raw_at_boundary,
        completed_while_blocker_held,
        sql,
        storage,
        writer,
    }
}

fn violations(probes: &[Probe]) -> Vec<String> {
    let mut failures = Vec::new();
    for probe in probes {
        match &probe.result {
            Ok(rows) => {
                for row in rows {
                    if row != &vec![probe.requested_id, probe.requested_id * 10] {
                        failures.push(format!(
                            "{} requested id {} but returned {row:?}",
                            probe.description, probe.requested_id
                        ));
                    }
                }
            }
            Err(error) => failures.push(format!("{}: {error}", probe.description)),
        }
    }
    failures
}

fn observations() -> Vec<Observation> {
    // Independent fixtures in both directions avoid assuming that a known-bad
    // baseline repaired its indexes sufficiently to prepare the next toggle.
    [(1, 2), (2, 1)]
        .into_iter()
        .map(|(from, to)| boundary(from, to))
        .collect()
}

fn assert_schedule(observation: &Observation) {
    assert!(observation.writer.is_ok(), "{observation:#?}");
    assert!(
        observation.completed_while_blocker_held,
        "SQL probe could not run at the intended boundary: {observation:#?}"
    );
    assert!(
        matches!(
            observation.boundary,
            "new_pk_claim_visible_before_secondary" | "writer_completed_without_new_pk_claim"
        ),
        "{observation:#?}"
    );
    let raw = observation.raw_at_boundary.as_ref().unwrap().as_ref().unwrap();
    let expected_id = if observation.boundary == "new_pk_claim_visible_before_secondary" {
        observation.from
    } else {
        observation.to
    };
    assert_eq!(
        raw.values,
        vec![Value::Int4(expected_id), Value::Int4(expected_id * 10)],
        "boundary must be the old row during a staged claim, or the new row after baseline completion"
    );
}

#[test]
fn sql_pk_predicates_hold_during_live_replay_publication() {
    let observations = observations();
    let mut failures = Vec::new();
    for observation in &observations {
        eprintln!("SQL AND STORAGE PUBLICATION OBSERVATION: {observation:#?}");
        assert_schedule(observation);
        failures.extend(violations(&observation.sql));
    }
    assert!(failures.is_empty(), "SQL predicate violations: {failures:#?}");
}

#[test]
fn storage_pk_lookup_returns_only_the_requested_identity_during_live_replay_publication() {
    let observations = observations();
    let mut failures = Vec::new();
    for observation in &observations {
        eprintln!("RAW STORAGE PUBLICATION OBSERVATION: {observation:#?}");
        assert_schedule(observation);
        failures.extend(violations(&observation.storage));
    }
    assert!(
        failures.is_empty(),
        "raw storage PK identity violations (separate from SQL): {failures:#?}"
    );
}
