//! R4.3 probe (run explicitly, release profile):
//!
//! ```text
//! cargo test --release --test version_gc_probe -- --ignored --nocapture
//! ```
//!
//! Legs:
//! (a) 100k rows x 50 transactional full-table update rounds → version-key
//!     count + approximate keyspace bytes before/after VACUUM VERSIONS.
//! (b) full-scan SELECT and AS OF scan throughput before/after GC (and
//!     after RocksDB compaction, which clears the delete tombstones).
//! (c) write-path overhead with the background collector running vs off.

use heliosdb_nano::{Config, EmbeddedDatabase};
use std::time::Instant;

const ROWS: usize = 100_000;
const ROUNDS: usize = 50;
const INSERT_BATCH: usize = 1_000;

fn build_table(db: &EmbeddedDatabase, rows: usize) {
    db.execute("CREATE TABLE bench (id INT PRIMARY KEY, val INT)").unwrap();
    let mut i = 0;
    while i < rows {
        let end = (i + INSERT_BATCH).min(rows);
        let values: Vec<String> = (i..end).map(|id| format!("({}, 0)", id)).collect();
        db.execute(&format!("INSERT INTO bench (id, val) VALUES {}", values.join(",")))
            .unwrap();
        i = end;
    }
}

fn update_rounds(db: &EmbeddedDatabase, rounds: usize) -> f64 {
    let start = Instant::now();
    for _ in 0..rounds {
        db.execute("BEGIN").unwrap();
        db.execute("UPDATE bench SET val = val + 1").unwrap();
        db.execute("COMMIT").unwrap();
    }
    start.elapsed().as_secs_f64()
}

/// Best-of-n full-scan throughput in rows/s.
fn scan_rows_per_sec(db: &EmbeddedDatabase, sql: &str, expected_rows: usize, n: usize) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..n {
        let start = Instant::now();
        let rows = db.query(sql, &[]).unwrap();
        let secs = start.elapsed().as_secs_f64();
        assert_eq!(rows.len(), expected_rows, "scan returned wrong row count for {}", sql);
        if secs < best {
            best = secs;
        }
    }
    expected_rows as f64 / best
}

#[test]
#[ignore]
fn probe_reclaim_and_scan_throughput() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.storage.path = Some(dir.path().to_path_buf());
    config.storage.version_retention = Some("1s".to_string());
    config.storage.version_gc_interval_secs = Some(0); // manual VACUUM
    let db = EmbeddedDatabase::with_config(config).unwrap();

    println!("== R4.3 probe: {} rows x {} update rounds ==", ROWS, ROUNDS);

    let t = Instant::now();
    build_table(&db, ROWS);
    println!("build: {:.1}s", t.elapsed().as_secs_f64());

    let secs = update_rounds(&db, ROUNDS);
    println!(
        "update rounds: {:.1}s ({:.0} row-updates/s)",
        secs,
        (ROWS * ROUNDS) as f64 / secs
    );

    // Recent anchor for AS OF scans that stays valid across the GC.
    std::thread::sleep(std::time::Duration::from_secs(2));
    let ts_recent = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let as_of_sql = format!("SELECT * FROM bench AS OF TIMESTAMP '{}'", ts_recent);

    let before = db.version_storage_stats().unwrap();
    println!(
        "before GC: {} v-keys, {} v_idx-keys, {:.1} MiB version keyspace",
        before.version_keys,
        before.version_index_keys,
        before.approx_bytes as f64 / (1024.0 * 1024.0)
    );

    let scan_before = scan_rows_per_sec(&db, "SELECT * FROM bench", ROWS, 3);
    let asof_before = scan_rows_per_sec(&db, &as_of_sql, ROWS, 3);
    println!(
        "scan before GC: {:.0} rows/s plain, {:.0} rows/s AS OF",
        scan_before, asof_before
    );

    std::thread::sleep(std::time::Duration::from_secs(2));
    let t = Instant::now();
    let collected = db.vacuum_versions().unwrap();
    println!(
        "VACUUM VERSIONS: {} collected in {:.1}s",
        collected,
        t.elapsed().as_secs_f64()
    );

    let after = db.version_storage_stats().unwrap();
    println!(
        "after GC: {} v-keys, {} v_idx-keys, {:.1} MiB version keyspace",
        after.version_keys,
        after.version_index_keys,
        after.approx_bytes as f64 / (1024.0 * 1024.0)
    );

    let scan_after = scan_rows_per_sec(&db, "SELECT * FROM bench", ROWS, 3);
    let asof_after = scan_rows_per_sec(&db, &as_of_sql, ROWS, 3);
    println!(
        "scan after GC: {:.0} rows/s plain, {:.0} rows/s AS OF",
        scan_after, asof_after
    );

    // Compaction clears the delete tombstones the GC left behind.
    let t = Instant::now();
    db.storage.vacuum().unwrap();
    println!("compaction: {:.1}s", t.elapsed().as_secs_f64());

    let scan_compacted = scan_rows_per_sec(&db, "SELECT * FROM bench", ROWS, 3);
    let asof_compacted = scan_rows_per_sec(&db, &as_of_sql, ROWS, 3);
    println!(
        "scan after compaction: {:.0} rows/s plain, {:.0} rows/s AS OF",
        scan_compacted, asof_compacted
    );

    assert_eq!(after.version_keys, ROWS as u64);
    assert!(collected > 0);
}

#[test]
#[ignore]
fn probe_write_overhead_with_gc_running() {
    const C_ROWS: usize = 20_000;
    const C_ROUNDS: usize = 10;

    let run = |gc_on: bool| -> f64 {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.storage.path = Some(dir.path().to_path_buf());
        if gc_on {
            config.storage.version_retention = Some("1s".to_string());
            config.storage.version_gc_interval_secs = Some(1); // aggressive worker
        }
        let db = EmbeddedDatabase::with_config(config).unwrap();
        build_table(&db, C_ROWS);
        // Warm-up round excluded from timing.
        update_rounds(&db, 1);
        update_rounds(&db, C_ROUNDS)
    };

    // Interleave to cancel machine drift.
    let mut off = 0.0;
    let mut on = 0.0;
    for _ in 0..3 {
        off += run(false);
        on += run(true);
    }
    let overhead = (on - off) / off * 100.0;
    println!(
        "write path {} rows x {} rounds x3: GC off {:.2}s, GC on (1s cycles) {:.2}s, overhead {:+.2}%",
        C_ROWS, C_ROUNDS, off, on, overhead
    );
}
