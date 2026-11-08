//! Spec R5.4 (`docs/plans/specs/R5_4_dml_one_group.md`): every DML statement is one
//! durable write group, and no committed write is lost or hidden from a later snapshot.
//!
//! * One group: [`write_groups_since`] reads the RocksDB WAL after a statement and names
//!   the keys of every write group it applied (`DB::get_updates_since`). An autocommit
//!   `INSERT … SELECT`, a params-family multi-row `UPDATE` / `DELETE` / `TRUNCATE`, a
//!   fast PK `UPDATE` / `DELETE` and a single-row `INSERT` each touch their table's
//!   `data:` keys in exactly one group, with their versions and their `snapshot:` record.
//! * Nothing lost (P12 and its family): a writer of an existing row waits for its row
//!   lock and changes the row as committed under it (`storage::row_locks`). Three
//!   clients incrementing one counter end at the number of increments they were told
//!   committed, on both families, autocommit and in transactions, embedded and on the
//!   extended protocol; an upsert beside a DELETE of its keys never duplicates a primary
//!   key; a multi-row UPDATE never brings back a row a DELETE removed; a key-moving
//!   UPDATE beside an upsert keeps its UNIQUE key findable.
//! * Nothing hidden (P2 / P9 / D4): a fast PK UPDATE writes a version, so a later
//!   transaction's scan reads it.
//! * P8: a params-family upsert reads the row its own transaction staged.
//! * P11: the extended protocol sees a session's temp tables, shadowing a permanent one.
//!
//! Handshakes are on the lock's own counters (`RowLocks::waiting`), never on time.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::collections::HashSet;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::session::IsolationLevel;
use heliosdb_nano::{Config, EmbeddedDatabase, Tuple, Value, BRANCH_MATERIALIZE_TOO_LARGE_MARKER};
use rocksdb::WriteBatchIterator;
use tokio_postgres::types::Type;
use tokio_postgres::NoTls;

const TEST_AT_MOST: Duration = Duration::from_secs(600);

/// Ends the process if a statement never returns (see `tests/table_write_lock.rs`).
#[must_use = "dropping it disarms the watchdog"]
fn watchdog(test: &'static str, limit: Duration) -> mpsc::Sender<()> {
    let (disarm, disarmed) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        if let Err(mpsc::RecvTimeoutError::Timeout) = disarmed.recv_timeout(limit) {
            let _ = writeln!(
                std::io::stderr(),
                "\nwatchdog: `{test}` still running after {} s. Ending the process.",
                limit.as_secs()
            );
            std::process::abort();
        }
    });
    disarm
}

fn int(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Int2(v)) => i64::from(*v),
        Some(Value::Int4(v)) => i64::from(*v),
        Some(Value::Int8(v)) => *v,
        other => panic!("not an integer: {other:?}"),
    }
}

fn rows(db: &EmbeddedDatabase, sql: &str) -> Vec<Tuple> {
    db.query(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"))
}

fn count(db: &EmbeddedDatabase, sql: &str) -> i64 {
    int(rows(db, sql)[0].values.first())
}

/// A condition another thread will make true: wait on its state, never on time.
fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !condition() {
        assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::yield_now();
    }
}

/// A database on disk, so that its RocksDB WAL holds every write group.
/// A database on disk whose RocksDB WAL keeps every write group: configured as
/// `tests/statement_atomicity_ddl.rs` (spec R5.3) configures its own, so both count
/// groups the same way. `replication.physical.enabled` is a replicating primary's
/// setting: RocksDB archives its WAL files instead of deleting them (the WAL-retention
/// options at open). The logical WAL's periodic checkpoint is off, so it adds no groups.
fn wal_keeping_config(dir: &tempfile::TempDir) -> Config {
    let mut config = Config::default();
    config.storage.path = Some(dir.path().to_path_buf());
    config.storage.memory_only = false;
    config.storage.wal_enabled = true;
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    config.storage.time_travel_enabled = true;
    config.replication.physical.enabled = true;
    config
}

fn on_disk(dir: &tempfile::TempDir) -> EmbeddedDatabase {
    EmbeddedDatabase::with_config(wal_keeping_config(dir)).unwrap()
}

/// In memory, with versions (P2 is about what a snapshot reads).
fn in_memory() -> EmbeddedDatabase {
    let mut config = Config::in_memory();
    config.storage.time_travel_enabled = true;
    EmbeddedDatabase::with_config(config).unwrap()
}

struct Keys(Vec<Vec<u8>>);

impl WriteBatchIterator for Keys {
    fn put(&mut self, key: Box<[u8]>, _value: Box<[u8]>) {
        self.0.push(key.into_vec());
    }

    fn delete(&mut self, key: Box<[u8]>) {
        self.0.push(key.into_vec());
    }
}

/// The RocksDB sequence the next write starts after.
fn sequence(db: &EmbeddedDatabase) -> u64 {
    db.storage.db().latest_sequence_number()
}

/// Every write group applied after sequence `after`, as its keys, read the way the
/// physical source reads them for a standby (`replication::physical::poll_source`):
/// sample the latest sequence, fsync the WAL, then iterate from `after`. rust-rocksdb's
/// WAL iterator yields only the records that START AFTER the sequence it is given (it
/// skips the one starting at it), so `after` itself is the argument, not `after + 1`;
/// a group with no operations consumes no sequence and is skipped, as the source skips
/// it. The walk must be gap-free from `after + 1` to the sampled sequence: a group the
/// WAL does not hold fails the test here instead of going uncounted.
fn write_groups_since(db: &EmbeddedDatabase, after: u64) -> Vec<Vec<Vec<u8>>> {
    let raw = db.storage.db();
    let latest = raw.latest_sequence_number();
    if latest <= after {
        return Vec::new();
    }
    raw.flush_wal(true)
        .expect("fsync the RocksDB WAL, as the physical source does");
    let mut groups = Vec::new();
    let mut next = after + 1;
    let mut updates = raw
        .get_updates_since(after)
        .expect("the RocksDB WAL holds the statement's writes");
    for item in updates.by_ref() {
        let (seq, batch) = item.expect("a WAL batch");
        if batch.is_empty() {
            continue;
        }
        if seq > latest {
            // Written after the sample (a background writer): not the statement's.
            break;
        }
        assert_eq!(seq, next, "a gap in the RocksDB WAL: a write group was not retained");
        next = seq + batch.len() as u64;
        let mut keys = Keys(Vec::new());
        batch.iterate(&mut keys);
        groups.push(keys.0);
    }
    updates.status().expect("WAL iterator status");
    assert_eq!(next, latest + 1, "the WAL walk ended before the latest sequence");
    groups
}

/// How many of `groups` hold a key starting with `prefix`.
fn groups_touching(groups: &[Vec<Vec<u8>>], prefix: &str) -> usize {
    groups
        .iter()
        .filter(|keys| keys.iter().any(|key| key.starts_with(prefix.as_bytes())))
        .count()
}

/// The one group holding `prefix`, which must also hold every one of `with`.
fn the_group_of(groups: &[Vec<Vec<u8>>], prefix: &str, with: &[&str], what: &str) {
    assert_eq!(
        groups_touching(groups, prefix),
        1,
        "{what}: the rows of `{prefix}` are written in exactly one group"
    );
    let group = groups
        .iter()
        .find(|keys| keys.iter().any(|key| key.starts_with(prefix.as_bytes())))
        .unwrap();
    for other in with {
        assert!(
            group.iter().any(|key| key.starts_with(other.as_bytes())),
            "{what}: `{other}` rides the rows' group"
        );
    }
}

fn counter(db: &EmbeddedDatabase) -> i64 {
    count(db, "SELECT n FROM cnt WHERE id = 1")
}

fn counter_table(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE cnt (id INT PRIMARY KEY, n BIGINT, tag TEXT)")
        .unwrap();
    db.execute("INSERT INTO cnt VALUES (1, 0, 'x'), (2, 0, 'z')").unwrap();
}

/// P12, deterministic: a transaction holds the row; an autocommit increment on each
/// family waits for its lock, and once the transaction commits, increments the value it
/// committed. Before R5.4 it read 0 and wrote 1 over the transaction's 1.
#[test]
fn a_waiting_writer_changes_the_row_its_lock_holder_committed() {
    let _watchdog = watchdog(
        "a_waiting_writer_changes_the_row_its_lock_holder_committed",
        TEST_AT_MOST,
    );
    let db = Arc::new(in_memory());
    counter_table(&db);
    type Writer = fn(&EmbeddedDatabase) -> heliosdb_nano::Result<u64>;
    let writers: [(&str, Writer); 4] = [
        ("text, by PK", |db| db.execute("UPDATE cnt SET n = n + 1 WHERE id = 1")),
        ("params, by PK", |db| {
            db.execute_params(
                "UPDATE cnt SET n = n + $1 WHERE id = $2",
                &[Value::Int8(1), Value::Int4(1)],
            )
        }),
        ("text, by a non-key predicate", |db| {
            db.execute("UPDATE cnt SET n = n + 1 WHERE tag = 'x'")
        }),
        ("params, by a non-key predicate", |db| {
            db.execute_params("UPDATE cnt SET n = n + 1 WHERE tag = $1", &[Value::String("x".into())])
        }),
    ];
    for (what, writer) in writers {
        let before = counter(&db);
        let session = db.create_session("holder", IsolationLevel::ReadCommitted).unwrap();
        db.begin_transaction_for_session(session).unwrap();
        db.execute_for_session(session, "UPDATE cnt SET n = n + 1 WHERE id = 1")
            .unwrap();
        let waiting = {
            let db = Arc::clone(&db);
            std::thread::spawn(move || writer(&db))
        };
        wait_for("the autocommit writer to wait for the row", || {
            db.storage.row_locks().waiting() > 0
        });
        db.commit_transaction_for_session(session).unwrap();
        db.destroy_session(session).unwrap();
        assert_eq!(waiting.join().unwrap().unwrap(), 1, "{what}: one row changed");
        assert_eq!(counter(&db), before + 2, "{what}: neither increment is lost");
    }
}

/// P12 with PostgreSQL's re-evaluation: the row the waiting statement wanted no longer
/// matches its WHERE clause once the holder committed, so it is not changed.
#[test]
fn a_waiting_writer_evaluates_its_where_clause_again_over_the_committed_row() {
    let _watchdog = watchdog(
        "a_waiting_writer_evaluates_its_where_clause_again_over_the_committed_row",
        TEST_AT_MOST,
    );
    let db = Arc::new(in_memory());
    counter_table(&db);
    for family in ["text", "params"] {
        db.execute("UPDATE cnt SET n = 0, tag = 'x' WHERE id = 1").unwrap();
        let session = db.create_session("holder", IsolationLevel::ReadCommitted).unwrap();
        db.begin_transaction_for_session(session).unwrap();
        db.execute_for_session(session, "UPDATE cnt SET n = n + 1, tag = 'y' WHERE id = 1")
            .unwrap();
        let waiting = {
            let db = Arc::clone(&db);
            std::thread::spawn(move || match family {
                "text" => db.execute("UPDATE cnt SET n = n + 100 WHERE tag = 'x'"),
                _ => db.execute_params(
                    "UPDATE cnt SET n = n + $1 WHERE tag = $2",
                    &[Value::Int8(100), Value::String("x".into())],
                ),
            })
        };
        wait_for("the writer to wait for the row", || {
            db.storage.row_locks().waiting() > 0
        });
        db.commit_transaction_for_session(session).unwrap();
        db.destroy_session(session).unwrap();
        assert_eq!(
            waiting.join().unwrap().unwrap(),
            0,
            "{family}: the row left its predicate"
        );
        assert_eq!(counter(&db), 1, "{family}: only the holder's change");
    }
}

/// A REPEATABLE READ transaction does not re-evaluate: a row changed by a commit after
/// its snapshot is a serialization failure (40001), never a silent overwrite.
#[test]
fn a_repeatable_read_transaction_refuses_a_row_changed_after_its_snapshot() {
    let db = in_memory();
    counter_table(&db);
    let session = db.create_session("rr", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(session).unwrap();
    let seen = db
        .query_in_session(session, "SELECT n FROM cnt WHERE id = 1", &[])
        .unwrap();
    assert_eq!(int(seen[0].values.first()), 0);
    db.execute("UPDATE cnt SET n = 5 WHERE id = 1").unwrap();
    let refused = db
        .execute_for_session(session, "UPDATE cnt SET n = n + 1 WHERE id = 1")
        .and_then(|_| db.commit_transaction_for_session(session));
    let err = refused.expect_err("the RR transaction may not overwrite the committed 5");
    assert!(
        err.to_string().to_ascii_lowercase().contains("serializ"),
        "a serialization failure: {err}"
    );
    let _ = db.rollback_transaction_for_session(session);
    db.destroy_session(session).unwrap();
    assert_eq!(counter(&db), 5);
}

/// P12 under load: clients on both families, by PK and by a non-key predicate, and in
/// session transactions, increment one counter; it ends at the number of increments
/// they were told committed.
#[test]
fn concurrent_increments_of_one_row_lose_nothing_on_either_family() {
    let _watchdog = watchdog(
        "concurrent_increments_of_one_row_lose_nothing_on_either_family",
        TEST_AT_MOST,
    );
    const ROUNDS: usize = 150;
    let db = in_memory();
    counter_table(&db);
    let acked = AtomicU64::new(0);
    let failures = std::sync::Mutex::new(Vec::<String>::new());
    let note = |result: heliosdb_nano::Result<u64>| match result {
        Ok(n) => {
            acked.fetch_add(n, Ordering::SeqCst);
        }
        Err(e) => failures.lock().unwrap().push(e.to_string()),
    };
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for _ in 0..ROUNDS {
                note(db.execute("UPDATE cnt SET n = n + 1 WHERE id = 1"));
            }
        });
        scope.spawn(|| {
            for _ in 0..ROUNDS {
                note(db.execute_params("UPDATE cnt SET n = n + 1 WHERE id = 1", &[]));
            }
        });
        scope.spawn(|| {
            for _ in 0..ROUNDS {
                note(db.execute_params(
                    "UPDATE cnt SET n = n + $1 WHERE id = $2",
                    &[Value::Int8(1), Value::Int4(1)],
                ));
            }
        });
        scope.spawn(|| {
            for _ in 0..ROUNDS {
                note(db.execute("UPDATE cnt SET n = n + 1 WHERE tag = 'x'"));
                note(db.execute_params("UPDATE cnt SET n = n + 1 WHERE tag = $1", &[Value::String("x".into())]));
            }
        });
        scope.spawn(|| {
            let session = db.create_session("txn", IsolationLevel::ReadCommitted).unwrap();
            for _ in 0..ROUNDS {
                let result = (|| -> heliosdb_nano::Result<u64> {
                    db.begin_transaction_for_session(session)?;
                    let changed = db.execute_for_session(session, "UPDATE cnt SET n = n + 1 WHERE id = 1")?;
                    db.commit_transaction_for_session(session)?;
                    Ok(changed)
                })();
                if result.is_err() {
                    let _ = db.rollback_transaction_for_session(session);
                }
                note(result);
            }
            db.destroy_session(session).unwrap();
        });
    });
    let failures = failures.into_inner().unwrap();
    assert!(failures.is_empty(), "no increment of one row fails: {failures:?}");
    assert_eq!(
        counter(&db),
        acked.load(Ordering::SeqCst) as i64,
        "the counter is the number of acknowledged increments"
    );
}

/// P12 as found: three clients on the extended protocol, each a prepared
/// `UPDATE cnt SET n = n + 1 WHERE id = 1` (576 acknowledged, the counter at 426).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_extended_protocol_clients_lose_no_increment() {
    let _watchdog = watchdog("three_extended_protocol_clients_lose_no_increment", TEST_AT_MOST);
    let db = Arc::new(in_memory());
    counter_table(&db);
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let server = PgServer::new(
        PgServerConfig::with_address(addr).with_max_connections(16),
        Arc::clone(&db),
    )
    .unwrap();
    let listener = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let connect_string = format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port());

    let mut clients = Vec::new();
    for _ in 0..3 {
        let connect_string = connect_string.clone();
        clients.push(tokio::spawn(async move {
            let (client, connection) = tokio_postgres::connect(&connect_string, NoTls).await.unwrap();
            let connection = tokio::spawn(async move {
                let _ = connection.await;
            });
            let statement = client.prepare("UPDATE cnt SET n = n + 1 WHERE id = 1").await.unwrap();
            let mut acked = 0u64;
            for _ in 0..200 {
                acked += client.execute(&statement, &[]).await.unwrap();
            }
            drop(client);
            connection.abort();
            acked
        }));
    }
    let mut acked = 0u64;
    for client in clients {
        acked += client.await.unwrap();
    }
    assert_eq!(acked, 600);
    assert_eq!(counter(&db), 600, "every acknowledged increment is in the counter");
    listener.abort();
}

/// HIGH (pre-existing): an upsert beside a DELETE of the same keys left 519 rows over 20
/// ids, surviving a restart. The upsert wrote back a row the DELETE had removed.
#[test]
fn an_upsert_beside_a_delete_of_its_keys_never_duplicates_a_primary_key() {
    let _watchdog = watchdog(
        "an_upsert_beside_a_delete_of_its_keys_never_duplicates_a_primary_key",
        TEST_AT_MOST,
    );
    const ROUNDS: i64 = 300;
    let db = in_memory();
    db.execute("CREATE TABLE u (id INT PRIMARY KEY, v INT)").unwrap();
    let ids: Vec<String> = (1..=20).map(|id| format!("({id}, 0)")).collect();
    db.execute(&format!("INSERT INTO u VALUES {}", ids.join(", "))).unwrap();
    let unexpected = std::sync::Mutex::new(Vec::<String>::new());
    let tolerate = |result: heliosdb_nano::Result<u64>| {
        if let Err(e) = result {
            let text = e.to_string().to_ascii_lowercase();
            // A conflict reported is what may happen; a duplicate stored is not.
            if !(text.contains("serializ") || text.contains("duplicate") || text.contains("unique")) {
                unexpected.lock().unwrap().push(e.to_string());
            }
        }
    };
    std::thread::scope(|scope| {
        for upserter in 0..4 {
            let tolerate = &tolerate;
            let db = &db;
            scope.spawn(move || {
                for round in 0..ROUNDS {
                    let id = 1 + (round + upserter) % 20;
                    if upserter % 2 == 0 {
                        tolerate(db.execute(&format!(
                            "INSERT INTO u VALUES ({id}, 0) ON CONFLICT (id) DO UPDATE SET v = u.v + 1"
                        )));
                    } else {
                        tolerate(db.execute_params(
                            "INSERT INTO u VALUES ($1, 0) ON CONFLICT (id) DO UPDATE SET v = u.v + 1",
                            &[Value::Int4(id as i32)],
                        ));
                    }
                }
            });
        }
        for deleter in 0..2 {
            let tolerate = &tolerate;
            let db = &db;
            scope.spawn(move || {
                for round in 0..ROUNDS {
                    let id = 1 + (round + deleter * 7) % 20;
                    tolerate(db.execute(&format!("DELETE FROM u WHERE id = {id}")));
                }
            });
        }
    });
    let unexpected = unexpected.into_inner().unwrap();
    assert!(unexpected.is_empty(), "{unexpected:?}");
    let found: Vec<i64> = rows(&db, "SELECT id FROM u")
        .iter()
        .map(|row| int(row.values.first()))
        .collect();
    let distinct: HashSet<i64> = found.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        found.len(),
        "no primary key is held by two rows: {found:?}"
    );
    assert_eq!(count(&db, "SELECT COUNT(*) FROM u"), found.len() as i64);
    for id in &found {
        assert_eq!(
            rows(&db, &format!("SELECT id FROM u WHERE id = {id}")).len(),
            1,
            "row {id} by its key"
        );
    }
}

/// Pre-existing (ghost2): a params multi-row UPDATE beside a DELETE by PK brought 150 of
/// 167 deleted rows back, without index entries.
#[test]
fn a_multi_row_update_beside_deletes_brings_no_deleted_row_back() {
    let _watchdog = watchdog(
        "a_multi_row_update_beside_deletes_brings_no_deleted_row_back",
        TEST_AT_MOST,
    );
    const ROWS: i64 = 600;
    let db = in_memory();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, g INT)").unwrap();
    let values: Vec<String> = (1..=ROWS).map(|id| format!("({id}, 1)")).collect();
    db.execute(&format!("INSERT INTO t VALUES {}", values.join(", ")))
        .unwrap();
    let stop = AtomicBool::new(false);
    let deleted = std::thread::scope(|scope| {
        scope.spawn(|| {
            while !stop.load(Ordering::SeqCst) {
                db.execute_params(
                    "UPDATE t SET g = g + $1 WHERE g = $2",
                    &[Value::Int4(0), Value::Int4(1)],
                )
                .unwrap();
            }
        });
        let mut deleted = 0;
        for id in (1..=ROWS).step_by(3) {
            deleted += db.execute(&format!("DELETE FROM t WHERE id = {id}")).unwrap();
        }
        stop.store(true, Ordering::SeqCst);
        deleted
    });
    assert_eq!(deleted, 200, "every DELETE found its row");
    let found: Vec<i64> = rows(&db, "SELECT id FROM t")
        .iter()
        .map(|row| int(row.values.first()))
        .collect();
    let back: Vec<&i64> = found.iter().filter(|id| *id % 3 == 1).collect();
    assert!(back.is_empty(), "deleted rows came back: {back:?}");
    assert_eq!(found.len(), 400);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM t"), 400);
}

/// Pre-existing (stress, E): an autocommit key-moving UPDATE beside an upsert of the same
/// row left a UNIQUE lookup empty about once in 800: the upsert wrote back the old key.
#[test]
fn a_key_moving_update_beside_an_upsert_keeps_every_unique_key_findable() {
    let _watchdog = watchdog(
        "a_key_moving_update_beside_an_upsert_keeps_every_unique_key_findable",
        TEST_AT_MOST,
    );
    const ROWS: i64 = 50;
    let db = in_memory();
    db.execute("CREATE TABLE k (id INT PRIMARY KEY, code TEXT UNIQUE, v INT)")
        .unwrap();
    let values: Vec<String> = (1..=ROWS).map(|id| format!("({id}, 'c{id}', 0)")).collect();
    db.execute(&format!("INSERT INTO k VALUES {}", values.join(", ")))
        .unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for round in 0..400 {
                let id = 1 + round % ROWS;
                db.execute(&format!("UPDATE k SET code = 'm{round}' WHERE id = {id}"))
                    .unwrap();
            }
        });
        scope.spawn(|| {
            for round in 0..400 {
                let id = 1 + round % ROWS;
                let _ = db.execute(&format!(
                    "INSERT INTO k VALUES ({id}, 'c{id}', 0) ON CONFLICT (id) DO UPDATE SET v = k.v + 1"
                ));
            }
        });
        scope.spawn(|| {
            for round in 0..400 {
                let id = 1 + (round * 7) % ROWS;
                let _ = db.execute_params(
                    "INSERT INTO k VALUES ($1, $2, 0) ON CONFLICT (id) DO UPDATE SET v = k.v + 1",
                    &[Value::Int4(id as i32), Value::String(format!("c{id}"))],
                );
            }
        });
    });
    for row in rows(&db, "SELECT id, code FROM k") {
        let id = int(row.values.first());
        let Some(Value::String(code)) = row.values.get(1) else {
            panic!("a code: {row:?}");
        };
        let by_code = rows(&db, &format!("SELECT id FROM k WHERE code = '{code}'"));
        assert_eq!(by_code.len(), 1, "row {id} by its UNIQUE key {code}");
        assert_eq!(int(by_code[0].values.first()), id);
    }
}

/// F1.1: an autocommit `INSERT … SELECT` is one write group, on both families. It was
/// about seven per row.
#[test]
fn insert_select_autocommit_is_one_write_group_on_both_families() {
    let dir = tempfile::tempdir().unwrap();
    let db = on_disk(&dir);
    db.execute("CREATE TABLE src (id INT PRIMARY KEY, v INT)").unwrap();
    for start in (1..=1000).step_by(250) {
        let values: Vec<String> = (start..start + 250).map(|id| format!("({id}, {id})")).collect();
        db.execute(&format!("INSERT INTO src VALUES {}", values.join(", ")))
            .unwrap();
    }
    db.execute("CREATE TABLE dst_text (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("CREATE TABLE dst_params (id INT PRIMARY KEY, v INT)")
        .unwrap();

    let before = sequence(&db);
    assert_eq!(db.execute("INSERT INTO dst_text SELECT id, v FROM src").unwrap(), 1000);
    the_group_of(
        &write_groups_since(&db, before),
        "data:dst_text:",
        &["v_idx:dst_text:", "snapshot:"],
        "text",
    );

    let before = sequence(&db);
    assert_eq!(
        db.execute_params(
            "INSERT INTO dst_params SELECT id, v FROM src WHERE v >= $1",
            &[Value::Int4(0)]
        )
        .unwrap(),
        1000
    );
    the_group_of(
        &write_groups_since(&db, before),
        "data:dst_params:",
        &["v_idx:dst_params:", "snapshot:"],
        "params",
    );
    assert_eq!(count(&db, "SELECT COUNT(*) FROM dst_text"), 1000);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM dst_params"), 1000);
}

/// F1.1: a primary-key violation at row 500 of 1000 leaves nothing, on both families. It
/// left rows 1..499.
#[test]
fn insert_select_pk_violation_at_row_k_leaves_nothing() {
    let db = in_memory();
    db.execute("CREATE TABLE src (id INT PRIMARY KEY, v INT)").unwrap();
    let values: Vec<String> = (1..=1000).map(|id| format!("({id}, {id})")).collect();
    db.execute(&format!("INSERT INTO src VALUES {}", values.join(", ")))
        .unwrap();
    for (family, table) in [("text", "dst_t"), ("params", "dst_p")] {
        db.execute(&format!("CREATE TABLE {table} (id INT PRIMARY KEY, v INT)"))
            .unwrap();
        db.execute(&format!("INSERT INTO {table} VALUES (500, -1)")).unwrap();
        let sql = format!("INSERT INTO {table} SELECT id, v FROM src");
        let result = match family {
            "text" => db.execute(&sql),
            _ => db.execute_params(&sql, &[]),
        };
        assert!(result.is_err(), "{family}: the duplicate of 500 is refused");
        assert_eq!(
            count(&db, &format!("SELECT COUNT(*) FROM {table}")),
            1,
            "{family}: nothing of the failed statement is left"
        );
        assert_eq!(rows(&db, &format!("SELECT id FROM {table} WHERE id = 1")).len(), 0);
        // And its index claims went with it: the statement can run once the key is free.
        db.execute(&format!("DELETE FROM {table} WHERE id = 500")).unwrap();
        db.execute(&sql).unwrap();
        assert_eq!(count(&db, &format!("SELECT COUNT(*) FROM {table}")), 1000);
    }
}

/// Decision 10: the params family's multi-row UPDATE, DELETE and TRUNCATE are one write
/// group each. They wrote a group (or several) per row.
#[test]
fn params_multi_row_update_delete_and_truncate_are_one_write_group_each() {
    let dir = tempfile::tempdir().unwrap();
    let db = on_disk(&dir);
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, g INT, v INT)").unwrap();
    let values: Vec<String> = (1..=200).map(|id| format!("({id}, {}, 0)", id % 2)).collect();
    db.execute(&format!("INSERT INTO t VALUES {}", values.join(", ")))
        .unwrap();

    let before = sequence(&db);
    assert_eq!(
        db.execute_params(
            "UPDATE t SET v = v + $1 WHERE g = $2",
            &[Value::Int4(1), Value::Int4(0)]
        )
        .unwrap(),
        100
    );
    the_group_of(&write_groups_since(&db, before), "data:t:", &["v_idx:t:"], "UPDATE");

    let before = sequence(&db);
    assert_eq!(
        db.execute_params("DELETE FROM t WHERE g = $1", &[Value::Int4(1)])
            .unwrap(),
        100
    );
    the_group_of(&write_groups_since(&db, before), "data:t:", &["v_idx:t:"], "DELETE");

    let before = sequence(&db);
    db.execute_params("TRUNCATE t", &[]).unwrap();
    the_group_of(&write_groups_since(&db, before), "data:t:", &[], "TRUNCATE");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM t"), 0);
}

/// F1.3: a fast PK UPDATE or DELETE writes its row, its version (or delete event) and
/// its `snapshot:` record in one group, and with strict logical WAL its log entry too.
#[test]
fn single_row_fast_update_and_delete_are_one_write_group_with_their_history() {
    for strict in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut config = wal_keeping_config(&dir);
        config.storage.logical_wal_per_statement = strict;
        let db = EmbeddedDatabase::with_config(config).unwrap();
        db.execute("CREATE TABLE f (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("INSERT INTO f VALUES (1, 1), (2, 2), (3, 3), (4, 4)")
            .unwrap();
        let mut with = vec!["v_idx:f:", "snapshot:"];
        if strict {
            with.push("wal:entries:");
        }
        let what = |statement: &str| format!("{statement} (strict WAL {strict})");

        let before = sequence(&db);
        assert_eq!(db.execute("UPDATE f SET v = 10 WHERE id = 1").unwrap(), 1);
        the_group_of(
            &write_groups_since(&db, before),
            "data:f:1",
            &with,
            &what("text UPDATE"),
        );

        let before = sequence(&db);
        assert_eq!(
            db.execute_params("UPDATE f SET v = $1 WHERE id = $2", &[Value::Int4(20), Value::Int4(2)])
                .unwrap(),
            1
        );
        the_group_of(
            &write_groups_since(&db, before),
            "data:f:2",
            &with,
            &what("params UPDATE"),
        );

        let before = sequence(&db);
        assert_eq!(db.execute("DELETE FROM f WHERE id = 3").unwrap(), 1);
        the_group_of(
            &write_groups_since(&db, before),
            "data:f:3",
            &with,
            &what("text DELETE"),
        );

        let before = sequence(&db);
        assert_eq!(
            db.execute_params("DELETE FROM f WHERE id = $1", &[Value::Int4(4)])
                .unwrap(),
            1
        );
        the_group_of(
            &write_groups_since(&db, before),
            "data:f:4",
            &with,
            &what("params DELETE"),
        );
    }
}

/// F1.2: a single-row INSERT on the params family's plan path (RETURNING keeps it off
/// the fast path) writes its row, counter, version and snapshot in one group.
#[test]
fn single_row_insert_on_the_plan_path_is_one_write_group() {
    let dir = tempfile::tempdir().unwrap();
    let db = on_disk(&dir);
    db.execute("CREATE TABLE s (id INT PRIMARY KEY, v INT)").unwrap();
    let before = sequence(&db);
    let (inserted, returned) = db
        .execute_params_returning(
            "INSERT INTO s VALUES ($1, $2) RETURNING id",
            &[Value::Int4(7), Value::Int4(70)],
        )
        .unwrap();
    assert_eq!(inserted, 1);
    assert_eq!(int(returned[0].values.first()), 7);
    the_group_of(
        &write_groups_since(&db, before),
        "data:s:",
        &["counter:s", "v_idx:s:", "snapshot:"],
        "single-row INSERT",
    );
}

/// F1.5: the `snapshot:` record of a commit rides the commit's group.
#[test]
fn commit_snapshot_record_rides_the_commit_group() {
    let dir = tempfile::tempdir().unwrap();
    let db = on_disk(&dir);
    db.execute("CREATE TABLE c (id INT PRIMARY KEY, v INT)").unwrap();
    let before = sequence(&db);
    db.execute("INSERT INTO c VALUES (1, 1), (2, 2), (3, 3)").unwrap();
    let groups = write_groups_since(&db, before);
    the_group_of(&groups, "data:c:", &["snapshot:"], "multi-row INSERT");
    assert_eq!(
        groups_touching(&groups, "snapshot:"),
        1,
        "no snapshot record in a group of its own"
    );
}

/// P2 / P9 / D4 (pre-existing): after an autocommit PK-fast UPDATE, a later
/// transaction's scan read the old value; a later DELETE is gone from it too.
#[test]
fn a_fast_pk_update_and_delete_are_seen_by_a_later_transactions_scan() {
    let db = in_memory();
    db.execute("CREATE TABLE a0 (id INT PRIMARY KEY, v INT, c INT)")
        .unwrap();
    db.execute("INSERT INTO a0 VALUES (1, 1, 1), (2, 2, 2), (3, 3, 3)")
        .unwrap();
    db.execute("UPDATE a0 SET c = 6 WHERE id = 1").unwrap();
    db.execute_params("UPDATE a0 SET c = $1 WHERE id = $2", &[Value::Int4(7), Value::Int4(2)])
        .unwrap();
    db.execute("DELETE FROM a0 WHERE id = 3").unwrap();
    let session = db.create_session("reader", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(session).unwrap();
    let seen: Vec<(i64, i64)> = db
        .query_in_session(session, "SELECT id, c FROM a0 ORDER BY id", &[])
        .unwrap()
        .iter()
        .map(|row| (int(row.values.first()), int(row.values.get(1))))
        .collect();
    assert_eq!(
        seen,
        vec![(1, 6), (2, 7)],
        "the transaction's scan reads both updates, not the deleted row"
    );
    db.commit_transaction_for_session(session).unwrap();
    db.destroy_session(session).unwrap();
}

/// P8 (pre-existing): a params-family upsert against a row its own transaction staged
/// failed `existing row not found in storage`, or computed from the committed image
/// (RETURNING said 2 while the row ended at 7).
#[test]
fn a_params_upsert_reads_and_updates_the_row_its_transaction_staged() {
    let db = in_memory();
    db.execute("CREATE TABLE u (id INT PRIMARY KEY, code TEXT, v INT)")
        .unwrap();
    db.execute("INSERT INTO u VALUES (1, 'a1', 1)").unwrap();
    let session = db.create_session("upserter", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(session).unwrap();
    db.execute_for_session(session, "UPDATE u SET v = 7 WHERE id = 1")
        .unwrap();
    let upsert = "INSERT INTO u VALUES ($1, $2, $3) ON CONFLICT (id) DO UPDATE SET v = u.v + $4 RETURNING v";
    let (_, returned) = db
        .execute_params_returning_for_session(
            session,
            upsert,
            &[
                Value::Int4(1),
                Value::String("zz".into()),
                Value::Int4(0),
                Value::Int4(1),
            ],
        )
        .unwrap();
    assert_eq!(int(returned[0].values.first()), 8, "RETURNING reads the staged 7");
    db.execute_params_for_session(
        session,
        "INSERT INTO u VALUES ($1, $2, $3)",
        &[Value::Int4(5), Value::String("n5".into()), Value::Int4(1)],
    )
    .unwrap();
    let (_, returned) = db
        .execute_params_returning_for_session(
            session,
            upsert,
            &[
                Value::Int4(5),
                Value::String("zz".into()),
                Value::Int4(0),
                Value::Int4(1),
            ],
        )
        .unwrap();
    assert_eq!(
        int(returned[0].values.first()),
        2,
        "the row inserted in the transaction"
    );
    db.commit_transaction_for_session(session).unwrap();
    db.destroy_session(session).unwrap();
    assert_eq!(count(&db, "SELECT v FROM u WHERE id = 1"), 8);
    assert_eq!(count(&db, "SELECT v FROM u WHERE id = 5"), 2);
}

/// F1.4: the params family's `ON CONFLICT DO UPDATE` rolls back with its transaction. It
/// wrote straight to storage, outside it.
#[test]
fn params_on_conflict_do_update_rolls_back_with_its_transaction() {
    let db = in_memory();
    db.execute("CREATE TABLE u (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO u VALUES (1, 1)").unwrap();
    let session = db.create_session("upserter", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(session).unwrap();
    db.execute_params_for_session(
        session,
        "INSERT INTO u VALUES ($1, $2) ON CONFLICT (id) DO UPDATE SET v = $3",
        &[Value::Int4(1), Value::Int4(0), Value::Int4(99)],
    )
    .unwrap();
    db.rollback_transaction_for_session(session).unwrap();
    db.destroy_session(session).unwrap();
    assert_eq!(
        count(&db, "SELECT v FROM u WHERE id = 1"),
        1,
        "the ROLLBACK undid the upsert"
    );
}

/// Decision 6: a commit above the write-group ceiling is refused, 54000, before
/// anything is written; its index claims are given back.
#[test]
fn commit_above_the_write_group_ceiling_is_refused_before_writing() {
    let db = in_memory();
    db.execute("CREATE TABLE big (id INT PRIMARY KEY, body TEXT)").unwrap();
    let body = "x".repeat(200);
    let values: Vec<String> = (1..=100).map(|id| format!("({id}, '{body}')")).collect();
    let insert = format!("INSERT INTO big VALUES {}", values.join(", "));
    db.storage.debug_set_write_group_ceiling(Some(4096));
    let refused = db.execute(&insert);
    db.storage.debug_set_write_group_ceiling(None);
    let err = refused.expect_err("20 KB of rows are above a 4 KB ceiling");
    assert!(err.to_string().contains(BRANCH_MATERIALIZE_TOO_LARGE_MARKER), "{err}");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM big"), 0, "nothing was written");
    db.execute(&insert).unwrap();
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM big"),
        100,
        "and the keys were given back"
    );
}

// ---------------------------------------------------------------------------
// Review fix pass (r534_review, 2026-11-07)
// ---------------------------------------------------------------------------

fn row_id_of(db: &EmbeddedDatabase, table: &str, id: i32) -> u64 {
    let schema = db.storage.catalog().get_table_schema(table).unwrap();
    db.storage
        .get_row_by_typed_pk_for_write_with_schema(table, &Value::Int4(id), &schema)
        .unwrap()
        .unwrap_or_else(|| panic!("{table} has row {id}"))
        .row_id
        .unwrap()
}

/// Review F1: a writer computes its row from storage, never from the row cache. A point
/// reader that read a row's old image before a write put it back into the cache after
/// the write had dropped it (the fill was fenced by the schema generation only, which a
/// row write does not move); the next PK increment then read that old image, found the
/// table's change epoch unmoved since it loaded it, and wrote old + 1 over the
/// committed value: 7-15 increments lost in ~25k. The image is planted here as that
/// reader left it. Before the fix every writer below wrote 1.
#[test]
fn a_pk_writer_computes_from_storage_never_a_stale_cached_row() {
    let db = in_memory();
    counter_table(&db);
    db.execute("UPDATE cnt SET n = 5 WHERE id = 1").unwrap();
    let row_id = row_id_of(&db, "cnt", 1);
    type Writer = fn(&EmbeddedDatabase) -> heliosdb_nano::Result<u64>;
    let writers: [(&str, Writer); 3] = [
        ("text fast PK UPDATE", |db| {
            db.execute("UPDATE cnt SET n = n + 1 WHERE id = 1")
        }),
        ("params fast PK UPDATE", |db| {
            db.execute_params(
                "UPDATE cnt SET n = n + $1 WHERE id = $2",
                &[Value::Int8(1), Value::Int4(1)],
            )
        }),
        ("text UPDATE in a session transaction", |db| {
            let session = db.create_session("f1", IsolationLevel::ReadCommitted).unwrap();
            db.begin_transaction_for_session(session).unwrap();
            let changed = db.execute_for_session(session, "UPDATE cnt SET n = n + 1 WHERE id = 1");
            db.commit_transaction_for_session(session).unwrap();
            db.destroy_session(session).unwrap();
            changed
        }),
    ];
    for (what, writer) in writers {
        let committed = count(&db, "SELECT n FROM cnt WHERE id = 1");
        let mut stale = Tuple::new(vec![Value::Int4(1), Value::Int8(0), Value::String("x".into())]);
        stale.row_id = Some(row_id);
        db.storage.row_cache().put("cnt", row_id, stale);
        assert_eq!(writer(&db).unwrap(), 1, "{what}: one row");
        let schema = db.storage.catalog().get_table_schema("cnt").unwrap();
        let stored = db
            .storage
            .get_row_by_typed_pk_for_write_with_schema("cnt", &Value::Int4(1), &schema)
            .unwrap()
            .unwrap();
        assert_eq!(
            int(stored.values.get(1)),
            committed + 1,
            "{what}: computed from the committed {committed}, not the cached 0"
        );
    }
}

/// Review F9: a row locked after a SAVEPOINT is let go by ROLLBACK TO SAVEPOINT (its
/// change is undone, as PostgreSQL ends the subtransaction that locked it); one locked
/// before the savepoint stays locked. A writer of the first used to wait for the whole
/// transaction, until its lock_timeout.
#[test]
fn rollback_to_savepoint_lets_go_of_the_rows_it_locked() {
    let db = in_memory();
    counter_table(&db);
    let first = row_id_of(&db, "cnt", 1);
    let second = row_id_of(&db, "cnt", 2);
    let session = db.create_session("f9", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(session).unwrap();
    db.execute_for_session(session, "UPDATE cnt SET n = n + 1 WHERE id = 2")
        .unwrap();
    db.execute_for_session(session, "SAVEPOINT s1").unwrap();
    db.execute_for_session(session, "UPDATE cnt SET n = n + 1 WHERE id = 1")
        .unwrap();
    assert!(db.storage.row_locks().holder("cnt", first).is_some());
    db.execute_for_session(session, "ROLLBACK TO SAVEPOINT s1").unwrap();
    assert_eq!(
        db.storage.row_locks().holder("cnt", first),
        None,
        "the row locked after the savepoint is free"
    );
    assert!(
        db.storage.row_locks().holder("cnt", second).is_some(),
        "the row locked before it stays locked"
    );
    let contended = db.storage.row_locks().contended();
    assert_eq!(db.execute("UPDATE cnt SET n = n + 10 WHERE id = 1").unwrap(), 1);
    assert_eq!(db.storage.row_locks().contended(), contended, "taken without a wait");
    db.commit_transaction_for_session(session).unwrap();
    db.destroy_session(session).unwrap();
    assert_eq!(counter(&db), 10);
    assert_eq!(count(&db, "SELECT n FROM cnt WHERE id = 2"), 1);
}

/// Review F13: a group past the write-group ceiling is refused while it is staged, not
/// after the whole statement ran and its batch was built (an `INSERT … SELECT` of
/// 2.8 GB of rows built an 8.6 GB group at 25 GB of RSS before the 54000 came). The
/// gate never sees it: its refusal count does not move.
#[test]
fn a_group_past_the_ceiling_is_refused_while_it_is_staged() {
    let db = in_memory();
    db.execute("CREATE TABLE big (id INT PRIMARY KEY, body TEXT)").unwrap();
    let insert = "INSERT INTO big SELECT x, repeat('x', 200) FROM generate_series(1, 5000) x";
    let gate_refusals = db.storage.write_gate().refusals();
    db.storage.debug_set_write_group_ceiling(Some(64 * 1024));
    let refused = db.execute(insert);
    db.storage.debug_set_write_group_ceiling(None);
    let err = refused.expect_err("about 1 MB of rows are above a 64 KB ceiling");
    assert!(err.to_string().contains(BRANCH_MATERIALIZE_TOO_LARGE_MARKER), "{err}");
    assert_eq!(
        db.storage.write_gate().refusals(),
        gate_refusals,
        "refused while staging: no batch reached the gate"
    );
    assert_eq!(count(&db, "SELECT COUNT(*) FROM big"), 0, "nothing was written");
    assert_eq!(db.execute(insert).unwrap(), 5000, "and it fits the real ceiling");
}

/// Review F6: two upserts of one NEW key. The second waits for the first (it holds the
/// value it arbitrates on) and then takes its DO UPDATE once the first commits, or
/// inserts once it rolls back, as PostgreSQL's speculative insertion does. It used to
/// race: 23505 when both found the key free, 40001 when it found the first's key before
/// its row.
#[test]
fn a_second_upsert_of_a_new_key_waits_for_the_first() {
    let _watchdog = watchdog("a_second_upsert_of_a_new_key_waits_for_the_first", TEST_AT_MOST);
    let db = Arc::new(in_memory());
    db.execute("CREATE TABLE u (id INT PRIMARY KEY, n INT)").unwrap();
    for (key, commit) in [(5, true), (6, false)] {
        let upsert = format!("INSERT INTO u VALUES ({key}, 1) ON CONFLICT (id) DO UPDATE SET n = u.n + 1");
        let first = db.create_session("first", IsolationLevel::ReadCommitted).unwrap();
        db.begin_transaction_for_session(first).unwrap();
        db.execute_for_session(first, &upsert).unwrap();
        let second = {
            let db = Arc::clone(&db);
            let upsert = upsert.clone();
            std::thread::spawn(move || db.execute(&upsert))
        };
        wait_for("the second upsert to wait for the first", || {
            db.storage.row_locks().waiting() > 0
        });
        if commit {
            db.commit_transaction_for_session(first).unwrap();
        } else {
            db.rollback_transaction_for_session(first).unwrap();
        }
        db.destroy_session(first).unwrap();
        assert_eq!(second.join().unwrap().unwrap(), 1, "key {key}: one row");
        let found = rows(&db, &format!("SELECT n FROM u WHERE id = {key}"));
        assert_eq!(found.len(), 1, "key {key}: one row with it");
        let expected = if commit { 2 } else { 1 };
        assert_eq!(int(found[0].values.first()), expected, "key {key}");
    }
}

/// Review F3 (pre-existing): an open transaction's DELETE took its key out of the
/// index at once, so a concurrent INSERT of the key went ahead, and after that
/// transaction's ROLLBACK two rows shared one primary key; a concurrent PK UPDATE found
/// nothing and its change was lost. The key stays the deleter's until it ends: the
/// INSERT waits and fails 23505 after a rollback (succeeds after a commit), the UPDATE
/// waits and applies after a rollback.
#[test]
fn an_uncommitted_delete_holds_its_key_until_it_ends() {
    let _watchdog = watchdog("an_uncommitted_delete_holds_its_key_until_it_ends", TEST_AT_MOST);
    let db = Arc::new(in_memory());
    db.execute("CREATE TABLE r (id INT PRIMARY KEY, v INT, n INT)").unwrap();
    type Writer = fn(&EmbeddedDatabase) -> heliosdb_nano::Result<u64>;
    let cases: [(&str, Writer, bool); 4] = [
        (
            "INSERT, deleter rolls back",
            |db| db.execute("INSERT INTO r VALUES (1, 9, 9)"),
            false,
        ),
        (
            "INSERT, deleter commits",
            |db| db.execute("INSERT INTO r VALUES (1, 9, 9)"),
            true,
        ),
        (
            "PK UPDATE, deleter rolls back",
            |db| db.execute("UPDATE r SET n = n + 1 WHERE id = 1"),
            false,
        ),
        (
            "upsert, deleter rolls back",
            |db| db.execute("INSERT INTO r VALUES (1, 9, 9) ON CONFLICT (id) DO UPDATE SET n = r.n + 1"),
            false,
        ),
    ];
    for (what, writer, commit) in cases {
        db.execute("DELETE FROM r").unwrap();
        db.execute("INSERT INTO r VALUES (1, 1, 0), (2, 2, 0)").unwrap();
        let deleter = db.create_session("deleter", IsolationLevel::ReadCommitted).unwrap();
        db.begin_transaction_for_session(deleter).unwrap();
        db.execute_for_session(deleter, "DELETE FROM r WHERE id = 1").unwrap();
        let concurrent = {
            let db = Arc::clone(&db);
            std::thread::spawn(move || writer(&db))
        };
        wait_for("the writer to wait for the deleter", || {
            db.storage.row_locks().waiting() > 0
        });
        if commit {
            db.commit_transaction_for_session(deleter).unwrap();
        } else {
            db.rollback_transaction_for_session(deleter).unwrap();
        }
        db.destroy_session(deleter).unwrap();
        let outcome = concurrent.join().unwrap();
        let found: Vec<(i64, i64)> = rows(&db, "SELECT v, n FROM r WHERE id = 1")
            .iter()
            .map(|row| (int(row.values.first()), int(row.values.get(1))))
            .collect();
        let all = count(&db, "SELECT COUNT(*) FROM r");
        match what {
            "INSERT, deleter rolls back" => {
                let err = outcome.expect_err("the key is the restored row's");
                assert!(
                    err.to_string().to_ascii_lowercase().contains("duplicate"),
                    "{what}: {err}"
                );
                assert_eq!(found, vec![(1, 0)], "{what}: only the original row");
                assert_eq!(all, 2, "{what}");
            }
            "INSERT, deleter commits" => {
                assert_eq!(outcome.unwrap(), 1, "{what}");
                assert_eq!(found, vec![(9, 9)], "{what}: only the new row");
                assert_eq!(all, 2, "{what}");
            }
            "PK UPDATE, deleter rolls back" => {
                assert_eq!(outcome.unwrap(), 1, "{what}: the restored row is updated");
                assert_eq!(found, vec![(1, 1)], "{what}");
            }
            _ => {
                assert_eq!(outcome.unwrap(), 1, "{what}: the DO UPDATE of the restored row");
                assert_eq!(found, vec![(1, 1)], "{what}");
                assert_eq!(all, 2, "{what}");
            }
        }
        assert_eq!(
            rows(&db, "SELECT id FROM r WHERE id = 1").len(),
            found.len(),
            "{what}: the key finds every row that holds it"
        );
    }
}

/// Review F4 (pre-existing): ON DELETE CASCADE took no lock on the children, so a child
/// updated by an open transaction was deleted under it and written back by that
/// transaction's COMMIT: an orphan. The cascade's exclusive inbound-table request
/// waits for the child's transaction before it reads and deletes committed rows.
#[test]
fn a_cascade_waits_for_a_child_an_open_transaction_changed() {
    let _watchdog = watchdog("a_cascade_waits_for_a_child_an_open_transaction_changed", TEST_AT_MOST);
    let db = Arc::new(in_memory());
    db.execute("CREATE TABLE par (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO par VALUES (1), (2)").unwrap();
    db.execute("CREATE TABLE chd (id INT PRIMARY KEY, pid INT REFERENCES par(id) ON DELETE CASCADE, n INT)")
        .unwrap();
    db.execute("INSERT INTO chd VALUES (5, 1, 0), (6, 2, 0)").unwrap();
    let holder = db.create_session("child", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(holder).unwrap();
    db.execute_for_session(holder, "UPDATE chd SET n = n + 1 WHERE id = 5")
        .unwrap();
    let cascade = {
        let db = Arc::clone(&db);
        std::thread::spawn(move || db.execute("DELETE FROM par WHERE id = 1"))
    };
    wait_for("the cascade's exclusive child-table request", || {
        assert!(
            !cascade.is_finished(),
            "cascade cannot complete before the child commits"
        );
        db.storage.debug_table_lock_waiters("chd").0 > 0
    });
    assert!(!cascade.is_finished());
    db.commit_transaction_for_session(holder).unwrap();
    db.destroy_session(holder).unwrap();
    assert_eq!(cascade.join().unwrap().unwrap(), 1);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM chd WHERE pid = 1"), 0, "no orphan");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM chd"), 1);
    assert_eq!(
        rows(&db, "SELECT id,pid,n FROM chd")[0].values,
        vec![Value::Int4(6), Value::Int4(2), Value::Int4(0)]
    );
}

/// Review F5: a branch's rows are locked like main's (by level and row). Two sessions
/// on one branch incrementing one row lost up to 81% of the increments, with no error.
#[test]
fn a_waiting_writer_on_a_branch_changes_the_row_its_lock_holder_committed() {
    let _watchdog = watchdog(
        "a_waiting_writer_on_a_branch_changes_the_row_its_lock_holder_committed",
        TEST_AT_MOST,
    );
    let db = Arc::new(in_memory());
    counter_table(&db);
    db.execute("CREATE BRANCH bx AS OF NOW").unwrap();
    let on_branch = |name: &str| {
        let session = db.create_session(name, IsolationLevel::ReadCommitted).unwrap();
        db.execute_for_session(session, "USE BRANCH bx").unwrap();
        session
    };
    let reader = on_branch("reader");
    let branch_counter = || {
        let found = db
            .query_in_session(reader, "SELECT n FROM cnt WHERE id = 1", &[])
            .unwrap();
        int(found[0].values.first())
    };
    for family in ["text", "params"] {
        let before = branch_counter();
        let holder = on_branch("holder");
        db.begin_transaction_for_session(holder).unwrap();
        db.execute_for_session(holder, "UPDATE cnt SET n = n + 1 WHERE id = 1")
            .unwrap();
        let waiter = on_branch("waiter");
        let waiting = {
            let db = Arc::clone(&db);
            std::thread::spawn(move || match family {
                "text" => db.execute_for_session(waiter, "UPDATE cnt SET n = n + 1 WHERE id = 1"),
                _ => db.execute_params_for_session(
                    waiter,
                    "UPDATE cnt SET n = n + $1 WHERE id = $2",
                    &[Value::Int8(1), Value::Int4(1)],
                ),
            })
        };
        wait_for("the branch writer to wait for the row", || {
            db.storage.row_locks().waiting() > 0
        });
        db.commit_transaction_for_session(holder).unwrap();
        db.destroy_session(holder).unwrap();
        assert_eq!(waiting.join().unwrap().unwrap(), 1, "{family}: one row");
        db.destroy_session(waiter).unwrap();
        assert_eq!(branch_counter(), before + 2, "{family}: neither increment is lost");
    }
    assert_eq!(counter(&db), 0, "main is untouched");
}

/// P11 (pre-existing): the extended protocol did not see a session's temp tables. An
/// INSERT into one failed 42P01, and with a permanent table of the same name it wrote
/// into the permanent table.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_extended_protocol_sees_a_sessions_temp_tables() {
    let _watchdog = watchdog("the_extended_protocol_sees_a_sessions_temp_tables", TEST_AT_MOST);
    let db = Arc::new(in_memory());
    db.execute("CREATE TABLE sh (id INT, a INT, b INT)").unwrap();
    db.execute("INSERT INTO sh VALUES (1, 2, 3)").unwrap();
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let server = PgServer::new(
        PgServerConfig::with_address(addr).with_max_connections(16),
        Arc::clone(&db),
    )
    .unwrap();
    let listener = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let connect_string = format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port());
    let (client, connection) = tokio_postgres::connect(&connect_string, NoTls).await.unwrap();
    let connection = tokio::spawn(async move {
        let _ = connection.await;
    });

    client
        .batch_execute("CREATE TEMP TABLE tt (x TEXT, y INT)")
        .await
        .unwrap();
    let insert = client
        .prepare_typed("INSERT INTO tt VALUES ($1, $2)", &[Type::TEXT, Type::INT4])
        .await
        .unwrap();
    assert_eq!(client.execute(&insert, &[&"a", &1i32]).await.unwrap(), 1);
    let select = client
        .prepare_typed("SELECT x FROM tt WHERE y = $1", &[Type::INT4])
        .await
        .unwrap();
    let found = client.query(&select, &[&1i32]).await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].get::<_, String>(0), "a");

    client.batch_execute("CREATE TEMP TABLE sh (id INT)").await.unwrap();
    let shadowing = client
        .prepare_typed("INSERT INTO sh VALUES ($1)", &[Type::INT4])
        .await
        .unwrap();
    assert_eq!(client.execute(&shadowing, &[&9i32]).await.unwrap(), 1);
    let own = client.query("SELECT id FROM sh", &[]).await.unwrap();
    assert_eq!(own.len(), 1, "the session reads its temp table");
    assert_eq!(own[0].get::<_, i32>(0), 9);
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM sh"),
        1,
        "the permanent table of the same name is untouched"
    );

    drop(client);
    connection.abort();
    listener.abort();
}
