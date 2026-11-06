//! Spec R2 (`docs/plans/specs/R2_ddl_window_cut.md`): engine findings of the
//! DDL-window test (`tests/physical_ddl_window.rs`) reproduced on one
//! embedded primary — no daemons, no standby — so each is a defect of the
//! engine, not of physical replication or of the test harness.
//!
//! Each asserts the CORRECT behaviour of a defect reported in R2. R5.1 closed
//! finding (b) beside a reader and R5.2 findings (a) and (c), and those tests
//! run. The sequential (b) test is still `#[ignore]`d: run it with
//! `cargo test --test physical_ddl_window_repro -- --ignored --nocapture`
//! (and with `--features internal-tests`: finding (b) appeared only in that
//! build of the daemon test).
//!
//! * (a) the primary's own readers saw ALTER TABLE … ADD COLUMN … DEFAULT
//!   half-applied: the schema was written first, then every row in its own
//!   write group, and nothing kept a concurrent reader out — rows not yet
//!   rewritten read NULL, not the default. Now no row is rewritten, and the
//!   schema and the column's missing value are one group;
//! * (b) `ALTER TABLE ddl_a RENAME TO ddl_b` failed `Table 'ddl_b' already
//!   exists` at iteration 6 of the workload in the `internal-tests` build of
//!   the daemon test (both runs, deterministically), although `DROP TABLE
//!   ddl_b` of iteration 5 had succeeded; the default build ran 20 iterations
//!   clean. Run here sequentially and with a concurrent reader on the same
//!   database, in either build;
//! * (c) a row inserted inside a transaction before an `ADD COLUMN … DEFAULT`
//!   of the same transaction stayed narrow: its primary-key point read failed
//!   `Column index 3 out of bounds in tuple` on the primary (and over the
//!   wire the bounded point-read path sent a DataRow the client could not
//!   parse: the daemon test's `broken 1`). Now every decode shapes the row.

#![cfg(feature = "ha-tier1")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{Config, EmbeddedDatabase, Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// A persistent primary a physical standby could follow.
fn primary_config(path: &Path) -> Config {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    config.replication.physical.enabled = true;
    config
}

fn vector(id: usize, iteration: usize) -> String {
    let component = |numerator: usize, denominator: usize| numerator as f64 / denominator as f64;
    format!(
        "[{:.4}, {:.4}, {:.4}, {:.4}]",
        component(id % 7, 8),
        component((id * 3) % 11, 16),
        component((id + iteration) % 5, 8),
        component(id, 32)
    )
}

/// The DDL-window workload's statements for iteration `it`
/// (`tests/physical_ddl_window.rs`, `script`), except that here the
/// in-transaction ADD COLUMN comes before the INSERT: finding (c) has its own
/// test below, so it must not stop this one.
fn workload(it: usize) -> Vec<String> {
    let rows: Vec<String> = (1..=12)
        .map(|id| format!("({id}, {it}, 'm{id}', 'v{id}', '{}')", vector(id, it)))
        .collect();
    vec![
        "CREATE TABLE ddl_a (id INT PRIMARY KEY, it INT NOT NULL, mid TEXT, v TEXT, emb VECTOR(4))".to_string(),
        format!("INSERT INTO ddl_a VALUES {}", rows.join(", ")),
        "CREATE INDEX ddl_a_v ON ddl_a (v)".to_string(),
        "ALTER TABLE ddl_a ADD COLUMN d INT DEFAULT 7".to_string(),
        "ALTER TABLE ddl_a DROP COLUMN mid".to_string(),
        "ALTER TABLE ddl_a RENAME COLUMN v TO w".to_string(),
        "CREATE INDEX ddl_a_emb ON ddl_a USING hnsw (emb)".to_string(),
        format!(
            "INSERT INTO ddl_a (id, it, w, emb, d) VALUES (13, {it}, 'v13', '{}', 8)",
            vector(13, it)
        ),
        "DROP INDEX ddl_a_emb".to_string(),
        "ALTER TABLE ddl_a RENAME TO ddl_b".to_string(),
        "TRUNCATE TABLE ddl_b".to_string(),
        format!(
            "INSERT INTO ddl_b (id, it, w, emb, d) VALUES (21, {it}, 'v21', '{}', 9), (22, {it}, 'v22', '{}', 10)",
            vector(21, it),
            vector(22, it)
        ),
        "BEGIN".to_string(),
        "CREATE TABLE ddl_c (id INT PRIMARY KEY, it INT NOT NULL, note TEXT)".to_string(),
        "ALTER TABLE ddl_c ADD COLUMN e INT DEFAULT 5".to_string(),
        format!("INSERT INTO ddl_c (id, it, note) VALUES (1, {it}, 'c1'), (2, {it}, 'c2')"),
        "UPDATE ddl_b SET w = 'u21' WHERE id = 21".to_string(),
        "DELETE FROM ddl_b WHERE id = 22".to_string(),
        "COMMIT".to_string(),
        "DROP TABLE ddl_b".to_string(),
        "DROP TABLE ddl_c".to_string(),
    ]
}

/// What the daemon test's readers ask (the subset that names every table).
const READS: [&str; 9] = [
    "SELECT * FROM ddl_a",
    "SELECT COUNT(*) FROM ddl_a",
    "SELECT id FROM ddl_a WHERE v = 'v5'",
    "SELECT * FROM ddl_b",
    "SELECT * FROM ddl_b WHERE id = 21",
    "SELECT column_name FROM information_schema.columns WHERE table_name = 'ddl_b'",
    "SELECT * FROM ddl_c",
    "SELECT * FROM ddl_c WHERE id = 1",
    "SELECT column_name FROM information_schema.columns WHERE table_name = 'ddl_c'",
];

/// Run `iterations` of the workload; a statement that fails panics naming
/// it, and after every `DROP TABLE ddl_b` the table must be gone.
fn drive(database: &EmbeddedDatabase, iterations: usize) {
    for iteration in 1..=iterations {
        for sql in workload(iteration) {
            if let Err(e) = database.execute(&sql) {
                panic!("iteration {iteration}: `{sql}` failed: {e}");
            }
            if sql == "DROP TABLE ddl_b" {
                assert!(
                    database.query("SELECT * FROM ddl_b", &[]).is_err(),
                    "iteration {iteration}: ddl_b is still readable after its DROP TABLE"
                );
            }
        }
    }
}

/// Finding (b), sequential: ten iterations of the workload on one primary.
#[test]
#[ignore = "R2 finding (b): run with --ignored"]
fn b_the_ddl_window_workload_runs_ten_iterations() {
    let root = tempfile::tempdir().unwrap();
    let database = EmbeddedDatabase::with_config(primary_config(&root.path().join("primary"))).unwrap();
    drive(&database, 10);
}

/// Finding (b) with what the daemon test also had: a reader on the primary
/// asking about every table while the workload runs. Closed (R5 decision 11): a
/// reader that filled the schema cache in the middle of a DROP TABLE re-cached the
/// table, and `RENAME x TO it` failed "already exists". Fills are guarded by the
/// generation now, and a DDL publishes after its write.
#[test]
fn b_the_ddl_window_workload_runs_ten_iterations_beside_a_reader() {
    /// Stops the reader on every way out of the test, a failed iteration included.
    struct StopReader(Arc<AtomicBool>);
    impl Drop for StopReader {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    // A statement that never returns: no flag stops that. Past this the process ends,
    // with a message libtest cannot capture, rather than hold the build lock.
    let (_disarm, disarmed) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let limit = std::time::Duration::from_secs(900);
        if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = disarmed.recv_timeout(limit) {
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "\nwatchdog: b_the_ddl_window_workload_runs_ten_iterations_beside_a_reader still running after 900 s"
            );
            std::process::abort();
        }
    });
    let root = tempfile::tempdir().unwrap();
    let database = Arc::new(EmbeddedDatabase::with_config(primary_config(&root.path().join("primary"))).unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicUsize::new(0));
    let reader = {
        let database = Arc::clone(&database);
        let stop = Arc::clone(&stop);
        let reads = Arc::clone(&reads);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                for sql in READS {
                    // Errors (a table that does not exist yet) are answers.
                    let _ = database.query(sql, &[]);
                    reads.fetch_add(1, Ordering::Relaxed);
                }
            }
        })
    };
    let stop_reader = StopReader(stop);
    drive(&database, 10);
    drop(stop_reader);
    reader.join().unwrap();
    assert!(reads.load(Ordering::Relaxed) > 0, "the reader asked nothing");
}

/// Finding (c): PostgreSQL gives the row the default; the point read must
/// answer it like the scan does.
#[test]
fn c_a_row_inserted_in_a_transaction_before_its_add_column_default_reads_by_primary_key() {
    let root = tempfile::tempdir().unwrap();
    let database = EmbeddedDatabase::with_config(primary_config(&root.path().join("primary"))).unwrap();
    for sql in [
        "BEGIN",
        "CREATE TABLE c (id INT PRIMARY KEY, it INT NOT NULL, note TEXT)",
        "INSERT INTO c VALUES (1, 1, 'c1'), (2, 1, 'c2')",
        "ALTER TABLE c ADD COLUMN e INT DEFAULT 5",
        "COMMIT",
    ] {
        database.execute(sql).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    }
    let scanned = database.query("SELECT * FROM c ORDER BY id", &[]).unwrap();
    assert_eq!(scanned.len(), 2);
    let point = database
        .query("SELECT * FROM c WHERE id = 1", &[])
        .unwrap_or_else(|e| panic!("the primary-key point read failed: {e} (the scan answered {scanned:?})"));
    assert_eq!(point.len(), 1, "{point:?}");
    assert_eq!(point[0].values.len(), 4, "the row is as wide as the schema: {point:?}");
    assert_eq!(point[0].values[3], Value::Int4(5), "the column's default: {point:?}");
    assert_eq!(
        point[0].values, scanned[0].values,
        "the point read answers what the scan does"
    );
}

/// Finding (a): a reader of the primary itself never sees ADD COLUMN …
/// DEFAULT applied to some rows and not others (the column NULL in some,
/// the default in the rest).
#[test]
fn a_the_primarys_own_reader_never_sees_add_column_default_half_applied() {
    const ROWS: usize = 3_000;
    let root = tempfile::tempdir().unwrap();
    let database = Arc::new(EmbeddedDatabase::with_config(primary_config(&root.path().join("primary"))).unwrap());
    database.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
    let values: Vec<String> = (1..=ROWS).map(|id| format!("({id}, 'v{id}')")).collect();
    database
        .execute(&format!("INSERT INTO t VALUES {}", values.join(", ")))
        .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let torn = Arc::new(AtomicUsize::new(0));
    let reader = {
        let database = Arc::clone(&database);
        let stop = Arc::clone(&stop);
        let torn = Arc::clone(&torn);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                // Before the ALTER the column does not exist: an error.
                if let Ok(rows) = database.query("SELECT d FROM t", &[]) {
                    let nulls = rows
                        .iter()
                        .filter(|row| row.values.first() == Some(&Value::Null))
                        .count();
                    if nulls > 0 && nulls < rows.len() {
                        torn.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(50));
    database.execute("ALTER TABLE t ADD COLUMN d INT DEFAULT 7").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    stop.store(true, Ordering::Release);
    reader.join().unwrap();
    assert_eq!(
        torn.load(Ordering::Relaxed),
        0,
        "a reader of the primary saw ADD COLUMN DEFAULT applied to some rows and not others"
    );
}
