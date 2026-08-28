//! R2.2 — ART index manager per-tree locking concurrency tests.
//!
//! Before R2.2 every INSERT/DELETE took the GLOBAL ArtIndexManager write lock
//! and iterated every index in the database, so a writer on table A blocked
//! point-lookup readers on table B. These tests assert the post-R2.2
//! contract:
//!   (a) a writer hammering table A and readers point-looking-up table B both
//!       complete a fixed amount of work, with correct results;
//!   (b) concurrent FK inserts across two tables with mutual FK references do
//!       not deadlock (the FK existence check reads the other table's PK
//!       index);
//!   (c) concurrent index create/drop (global registry WRITE lock) during
//!       reads does not deadlock or corrupt results.
//!
//! No timing assertions — only correctness and completion (with a watchdog
//! timeout so a deadlock fails the test instead of hanging CI).

#![allow(clippy::unwrap_used)]

use heliosdb_nano::{EmbeddedDatabase, Value};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Join all handles, panicking if they do not finish within `timeout`
/// (deadlock watchdog) or if any thread panicked.
fn join_with_timeout(handles: Vec<JoinHandle<()>>, timeout: Duration, what: &str) {
    let (tx, rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        for h in handles {
            h.join().expect("worker thread panicked");
        }
        // Receiver may already be gone if the watchdog fired.
        let _ = tx.send(());
    });
    match rx.recv_timeout(timeout) {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("{}: threads did not finish within {:?} — deadlock?", what, timeout)
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            // Waiter died => a worker panicked; surface its panic below.
        }
    }
    waiter.join().expect("a worker thread panicked");
}

/// (a) Writer hammering table A must not block point-lookup readers on
/// table B; both complete fixed work and the readers see correct values.
#[test]
fn writer_on_table_a_does_not_starve_readers_on_table_b() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());

    db.execute("CREATE TABLE art_conc_a (id INT PRIMARY KEY, payload TEXT)")
        .unwrap();
    db.execute("CREATE TABLE art_conc_b (id INT PRIMARY KEY, val TEXT)")
        .unwrap();

    // Pre-populate table B for the readers.
    const B_ROWS: i64 = 200;
    for i in 0..B_ROWS {
        db.execute(&format!("INSERT INTO art_conc_b VALUES ({}, 'val_{}')", i, i))
            .unwrap();
    }

    const WRITES: i64 = 400;
    const READS_PER_READER: i64 = 400;
    const READERS: usize = 2;

    let mut handles = Vec::new();

    // Writer thread: hammer table A.
    {
        let db = Arc::clone(&db);
        handles.push(std::thread::spawn(move || {
            for i in 0..WRITES {
                db.execute(&format!("INSERT INTO art_conc_a VALUES ({}, 'payload_{}')", i, i))
                    .unwrap();
            }
        }));
    }

    // Reader threads: point lookups on table B (PK / ART index path).
    for r in 0..READERS {
        let db = Arc::clone(&db);
        handles.push(std::thread::spawn(move || {
            for i in 0..READS_PER_READER {
                let key = (i * (r as i64 + 7)) % B_ROWS;
                let rows = db
                    .query(&format!("SELECT val FROM art_conc_b WHERE id = {}", key), &[])
                    .unwrap();
                assert_eq!(rows.len(), 1, "point lookup must find exactly one row for id={}", key);
                assert_eq!(
                    rows[0].values.first(),
                    Some(&Value::String(format!("val_{}", key))),
                    "point lookup returned the wrong row for id={}",
                    key
                );
            }
        }));
    }

    join_with_timeout(handles, Duration::from_secs(120), "writer-A / readers-B");

    // Both workloads completed: verify the writer's work too.
    let rows = db.query("SELECT COUNT(*) FROM art_conc_a", &[]).unwrap();
    assert_eq!(rows[0].values.first(), Some(&Value::Int8(WRITES)));
}

/// (b) Concurrent FK inserts across two tables with mutual FK references.
/// Thread 1 inserts into A (FK check reads B's PK index); thread 2 inserts
/// into B (FK check reads A's PK index). With per-tree locks this is the
/// classic ABBA shape — the manager must never hold two tree locks at once,
/// so no deadlock can form.
#[test]
fn mutual_fk_inserts_across_tables_do_not_deadlock() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());

    db.execute("CREATE TABLE fk_side_a (id INT PRIMARY KEY, b_ref INT)")
        .unwrap();
    db.execute("CREATE TABLE fk_side_b (id INT PRIMARY KEY, a_ref INT)")
        .unwrap();
    db.execute("ALTER TABLE fk_side_a ADD CONSTRAINT fk_a_to_b FOREIGN KEY (b_ref) REFERENCES fk_side_b(id)")
        .unwrap();
    db.execute("ALTER TABLE fk_side_b ADD CONSTRAINT fk_b_to_a FOREIGN KEY (a_ref) REFERENCES fk_side_a(id)")
        .unwrap();

    // Seed both sides with referenceable rows (NULL FK values are allowed).
    const SEED: i64 = 100;
    for i in 1..=SEED {
        db.execute(&format!("INSERT INTO fk_side_a VALUES ({}, NULL)", i))
            .unwrap();
        db.execute(&format!("INSERT INTO fk_side_b VALUES ({}, NULL)", i))
            .unwrap();
    }

    const INSERTS: i64 = 200;
    let mut handles = Vec::new();

    // Thread 1: insert into A, each row referencing an existing B row.
    {
        let db = Arc::clone(&db);
        handles.push(std::thread::spawn(move || {
            for i in 0..INSERTS {
                let id = SEED + 1 + i;
                let b_ref = (i % SEED) + 1;
                db.execute(&format!("INSERT INTO fk_side_a VALUES ({}, {})", id, b_ref))
                    .unwrap();
            }
        }));
    }

    // Thread 2: insert into B, each row referencing an existing A row.
    {
        let db = Arc::clone(&db);
        handles.push(std::thread::spawn(move || {
            for i in 0..INSERTS {
                let id = SEED + 1 + i;
                let a_ref = (i % SEED) + 1;
                db.execute(&format!("INSERT INTO fk_side_b VALUES ({}, {})", id, a_ref))
                    .unwrap();
            }
        }));
    }

    join_with_timeout(handles, Duration::from_secs(120), "mutual FK inserts");

    // All rows landed.
    let a_count = db.query("SELECT COUNT(*) FROM fk_side_a", &[]).unwrap();
    assert_eq!(a_count[0].values.first(), Some(&Value::Int8(SEED + INSERTS)));
    let b_count = db.query("SELECT COUNT(*) FROM fk_side_b", &[]).unwrap();
    assert_eq!(b_count[0].values.first(), Some(&Value::Int8(SEED + INSERTS)));

    // FK constraints are still enforced after the concurrent run.
    assert!(
        db.execute("INSERT INTO fk_side_a VALUES (999999, 888888)").is_err(),
        "orphan FK insert into fk_side_a must still be rejected"
    );
    assert!(
        db.execute("INSERT INTO fk_side_b VALUES (999999, 888888)").is_err(),
        "orphan FK insert into fk_side_b must still be rejected"
    );
}

/// (c) Concurrent index create/drop (takes the global registry WRITE lock)
/// while readers run point lookups and a writer inserts into the DDL'd
/// table. Create/drop is driven through the ART manager API directly (the
/// embedded SQL layer does not support `DROP INDEX` yet — it now errors
/// loudly; it used to parse as `DROP TABLE`), which is exactly the registry
/// path R2.2 re-locked.
#[test]
fn concurrent_index_create_drop_during_reads() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());

    db.execute("CREATE TABLE ddl_target (id INT PRIMARY KEY, email TEXT)")
        .unwrap();
    db.execute("CREATE TABLE steady_reads (id INT PRIMARY KEY, val TEXT)")
        .unwrap();

    const ROWS: i64 = 100;
    for i in 0..ROWS {
        db.execute(&format!(
            "INSERT INTO ddl_target VALUES ({}, 'user{}@example.com')",
            i, i
        ))
        .unwrap();
        db.execute(&format!("INSERT INTO steady_reads VALUES ({}, 'val_{}')", i, i))
            .unwrap();
    }

    const DDL_CYCLES: usize = 50;
    const READS_PER_READER: i64 = 300;
    const EXTRA_WRITES: i64 = 200;
    let mut handles = Vec::new();

    // DDL thread: create/drop a secondary ART index on ddl_target.email
    // (global registry WRITE lock churn).
    {
        let db = Arc::clone(&db);
        handles.push(std::thread::spawn(move || {
            let art = db.storage.art_indexes();
            for _ in 0..DDL_CYCLES {
                art.create_manual_index("idx_conc_ddl_email", "ddl_target", &["email".to_string()])
                    .unwrap();
                art.drop_index("idx_conc_ddl_email").unwrap();
            }
        }));
    }

    // Reader 1: PK point lookups on the unrelated table.
    {
        let db = Arc::clone(&db);
        handles.push(std::thread::spawn(move || {
            for i in 0..READS_PER_READER {
                let key = i % ROWS;
                let rows = db
                    .query(&format!("SELECT val FROM steady_reads WHERE id = {}", key), &[])
                    .unwrap();
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].values.first(), Some(&Value::String(format!("val_{}", key))));
            }
        }));
    }

    // Reader 2: PK point lookups on the table whose secondary index is being
    // created/dropped.
    {
        let db = Arc::clone(&db);
        handles.push(std::thread::spawn(move || {
            for i in 0..READS_PER_READER {
                let key = i % ROWS;
                let rows = db
                    .query(&format!("SELECT email FROM ddl_target WHERE id = {}", key), &[])
                    .unwrap();
                assert_eq!(rows.len(), 1, "PK lookup must find exactly one row for id={}", key);
                assert_eq!(
                    rows[0].values.first(),
                    Some(&Value::String(format!("user{}@example.com", key)))
                );
            }
        }));
    }

    // Writer: inserts into ddl_target while its index set churns (on_insert
    // races create/drop of the manual index).
    {
        let db = Arc::clone(&db);
        handles.push(std::thread::spawn(move || {
            for i in 0..EXTRA_WRITES {
                let id = 1000 + i;
                db.execute(&format!(
                    "INSERT INTO ddl_target VALUES ({}, 'user{}@example.com')",
                    id, id
                ))
                .unwrap();
            }
        }));
    }

    join_with_timeout(handles, Duration::from_secs(120), "index create/drop during reads");

    // All inserts landed and the registry ends consistent (PK index only —
    // every create was paired with a drop).
    let rows = db.query("SELECT COUNT(*) FROM ddl_target", &[]).unwrap();
    assert_eq!(rows[0].values.first(), Some(&Value::Int8(ROWS + EXTRA_WRITES)));
    let art = db.storage.art_indexes();
    assert!(!art.index_exists("idx_conc_ddl_email"));
    assert_eq!(art.list_table_indexes("ddl_target").len(), 1);
}
