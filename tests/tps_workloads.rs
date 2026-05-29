//! Cross-workload TPS harness (perf investigation, not a correctness test).
//!
//! Run explicitly, single-threaded, with output shown:
//!   HELIOS_TPS=1 HELIOS_TPS_MODE=mem  HELIOS_TPS_N=50000 \
//!     cargo test --release --test tps_workloads run_tps_suite -- --nocapture --test-threads=1
//!
//! HELIOS_TPS_MODE: mem | disk | disk_group | disk_nowal
//!   mem        -> new_in_memory()           (no WAL, no fsync — pure CPU path)
//!   disk       -> on-disk, WAL=Sync         (fsync per write — the durable default)
//!   disk_group -> on-disk, WAL=GroupCommit  (batched fsync)
//!   disk_nowal -> on-disk, WAL disabled     (RocksDB default durability only)

use heliosdb_nano::config::WalSyncModeConfig;
use heliosdb_nano::{Config, EmbeddedDatabase, Result};
use std::time::Instant;

/// Analytics scan benchmark over a large table (P1#6 parallel-decode).
/// Set HELIOS_SCAN_SERIAL=1 to force the serial decode path for A/B comparison.
///   HELIOS_SCAN=1 HELIOS_SCAN_N=300000 cargo test --profile perf --test tps_workloads run_scan_bench -- --nocapture --test-threads=1
#[test]
fn run_scan_bench() {
    if std::env::var("HELIOS_SCAN").is_err() {
        eprintln!("skipping run_scan_bench (set HELIOS_SCAN=1)");
        return;
    }
    let n: usize = std::env::var("HELIOS_SCAN_N").ok().and_then(|s| s.parse().ok()).unwrap_or(300_000);
    let serial = std::env::var("HELIOS_SCAN_SERIAL").is_ok();
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE wide (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER, c TEXT, d INTEGER, e INTEGER)")
        .unwrap();
    db.execute("BEGIN").unwrap();
    for i in 0..n {
        db.execute(&format!(
            "INSERT INTO wide (id,a,b,c,d,e) VALUES ({i},{},{},'row{}',{},{})",
            i % 1000,
            (i * 7) % 100000,
            i,
            (i * 13) % 100000,
            i % 8
        ))
        .unwrap();
    }
    db.execute("COMMIT").unwrap();

    println!(
        "\n=== scan bench: N={n} decode={} ===",
        if serial { "SERIAL" } else { "PARALLEL" }
    );
    let runs = 5usize;
    // Vary the predicate each run (`b >= r`, b is non-indexed and >= 0 so it
    // still matches ~all rows) so the SQL text differs → result cache MISSES →
    // we measure real scan+decode every iteration, not an Arc cache clone.
    let bench = |label: &str, mk: &dyn Fn(usize) -> String| {
        let t = Instant::now();
        let mut rows = 0;
        for r in 0..runs {
            rows = db.query(&mk(r), &[]).unwrap().len();
        }
        let us = t.elapsed().as_secs_f64() * 1e6 / runs as f64;
        println!("{label:<26} {us:>10.1} us/query   ({rows} rows out)");
    };
    bench("full_scan(SELECT *)", &|r| format!("SELECT * FROM wide WHERE b >= {r}"));
    bench("filter_scan(d>50000)", &|r| {
        format!("SELECT id, a FROM wide WHERE d > 50000 AND b >= {r}")
    });
    bench("agg_sum_avg", &|r| {
        format!("SELECT SUM(a), AVG(d), MAX(b) FROM wide WHERE b >= {r}")
    });
    bench("group_by_e", &|r| {
        format!("SELECT e, COUNT(*), SUM(a) FROM wide WHERE b >= {r} GROUP BY e")
    });
    println!();
}

fn bench<F: FnMut() -> Result<()>>(label: &str, ops: usize, mut f: F) {
    let start = Instant::now();
    f().expect("workload failed");
    let secs = start.elapsed().as_secs_f64();
    let tps = ops as f64 / secs;
    println!(
        "{:<28} {:>10} ops  {:>9.3} s  {:>14.0} ops/s  {:>10.2} us/op",
        label,
        ops,
        secs,
        tps,
        secs * 1e6 / ops as f64
    );
}

fn make_db(mode: &str, dir: &std::path::Path) -> Result<EmbeddedDatabase> {
    match mode {
        "mem" => EmbeddedDatabase::new_in_memory(),
        "disk" => {
            let mut c = Config::default();
            c.storage.path = Some(dir.to_path_buf());
            c.storage.memory_only = false;
            c.storage.wal_enabled = true;
            c.storage.wal_sync_mode = WalSyncModeConfig::Sync;
            EmbeddedDatabase::with_config(c)
        }
        "disk_group" => {
            let mut c = Config::default();
            c.storage.path = Some(dir.to_path_buf());
            c.storage.memory_only = false;
            c.storage.wal_enabled = true;
            c.storage.wal_sync_mode = WalSyncModeConfig::GroupCommit;
            EmbeddedDatabase::with_config(c)
        }
        "disk_nowal" => {
            let mut c = Config::default();
            c.storage.path = Some(dir.to_path_buf());
            c.storage.memory_only = false;
            c.storage.wal_enabled = false;
            EmbeddedDatabase::with_config(c)
        }
        other => panic!("unknown HELIOS_TPS_MODE: {other}"),
    }
}

/// Scaling diagnostic: is UPDATE/DELETE WHERE pk=literal O(1) or O(rows)?
/// Measures single-op latency at growing table sizes. Linear growth ⇒ full scan.
/// Trace where a single autocommit DELETE spends time, at two table sizes.
/// Uses the engine's built-in tracing spans (txn_begin / execute / txn_commit).
#[test]
fn run_delete_trace() {
    if std::env::var("HELIOS_DIAG").is_err() {
        eprintln!("skipping run_delete_trace (set HELIOS_DIAG=1)");
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .try_init();

    for &rows in &[4_000usize, 32_000] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
            .unwrap();
        db.execute("BEGIN").unwrap();
        for i in 0..rows {
            db.execute(&format!("INSERT INTO t (id, v) VALUES ({i}, {i})")).unwrap();
        }
        // sentinels to delete
        for k in 0..5 {
            db.execute(&format!("INSERT INTO t (id, v) VALUES ({}, 0)", rows + k))
                .unwrap();
        }
        db.execute("COMMIT").unwrap();
        eprintln!("---- DELETE trace at rows={rows} ----");
        for k in 0..5 {
            let t = Instant::now();
            db.execute(&format!("DELETE FROM t WHERE id = {}", rows + k)).unwrap();
            eprintln!("[total] DELETE rows={rows} #{k} = {} us", t.elapsed().as_micros());
        }
    }
}

/// Reproduce the suite's multi-table DELETE cost with component timers.
#[test]
fn run_delete_repro() {
    if std::env::var("HELIOS_DIAG").is_err() {
        eprintln!("skipping run_delete_repro (set HELIOS_DIAG=1)");
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .try_init();
    let big: usize = std::env::var("REPRO_ORDERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000);
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, email TEXT, age INTEGER, balance INTEGER)")
        .unwrap();
    db.execute("CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, amount INTEGER, status TEXT)")
        .unwrap();
    db.execute("BEGIN").unwrap();
    for i in 0..50_000 {
        db.execute(&format!(
            "INSERT INTO users (id,name,email,age,balance) VALUES ({i},'U{i}','u{i}@x.com',{},{})",
            18 + (i % 60),
            i
        ))
        .unwrap();
    }
    for i in 0..big {
        db.execute(&format!(
            "INSERT INTO orders (id,user_id,amount,status) VALUES ({i},{},{},'{}')",
            i % 50_000,
            i % 5000,
            if i % 3 == 0 { "paid" } else { "pending" }
        ))
        .unwrap();
    }
    db.execute("COMMIT").unwrap();
    // sentinels in users
    db.execute("BEGIN").unwrap();
    for k in 0..5 {
        db.execute(&format!(
            "INSERT INTO users (id,name,email,age,balance) VALUES ({},'s','s',1,1)",
            50_000 + k
        ))
        .unwrap();
    }
    db.execute("COMMIT").unwrap();
    eprintln!("---- DELETE repro: users=50005 orders={big} ----");
    for k in 0..5 {
        let t = Instant::now();
        db.execute(&format!("DELETE FROM users WHERE id = {}", 50_000 + k))
            .unwrap();
        eprintln!("[total] delete #{k} = {} us", t.elapsed().as_micros());
    }
}

#[test]
fn run_scaling_diag() {
    if std::env::var("HELIOS_DIAG").is_err() {
        eprintln!("skipping run_scaling_diag (set HELIOS_DIAG=1)");
        return;
    }
    println!("\n=========== UPDATE/DELETE-by-PK scaling diagnostic (in-memory) ===========");
    println!(
        "{:>8}  {:>14}  {:>14}  {:>14}",
        "rows", "pointSEL us", "UPDATE us", "DELETE us"
    );
    for &rows in &[2_000usize, 4_000, 8_000, 16_000, 32_000] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
            .unwrap();
        db.execute("BEGIN").unwrap();
        for i in 0..rows {
            db.execute(&format!("INSERT INTO t (id, v) VALUES ({i}, {})", i * 3))
                .unwrap();
        }
        db.execute("COMMIT").unwrap();

        let reps = 200usize;
        // point SELECT by pk
        let t = Instant::now();
        for k in 0..reps {
            let _ = db
                .query(&format!("SELECT * FROM t WHERE id = {}", (k * 7919) % rows), &[])
                .unwrap();
        }
        let sel_us = t.elapsed().as_secs_f64() * 1e6 / reps as f64;
        // UPDATE by pk (literal RHS to allow any fast path)
        let t = Instant::now();
        for k in 0..reps {
            db.execute(&format!("UPDATE t SET v = {} WHERE id = {}", k, (k * 7919) % rows))
                .unwrap();
        }
        let upd_us = t.elapsed().as_secs_f64() * 1e6 / reps as f64;
        // DELETE by pk (delete distinct high ids we add first so table size stays ~constant during loop)
        db.execute("BEGIN").unwrap();
        for k in 0..reps {
            db.execute(&format!("INSERT INTO t (id, v) VALUES ({}, 0)", rows + k))
                .unwrap();
        }
        db.execute("COMMIT").unwrap();
        let t = Instant::now();
        for k in 0..reps {
            db.execute(&format!("DELETE FROM t WHERE id = {}", rows + k)).unwrap();
        }
        let del_us = t.elapsed().as_secs_f64() * 1e6 / reps as f64;

        println!("{:>8}  {:>14.2}  {:>14.2}  {:>14.2}", rows, sel_us, upd_us, del_us);
    }
    println!("(linear growth in a column ⇒ that op is doing a full table scan)\n");
}

#[test]
fn run_tps_suite() {
    if std::env::var("HELIOS_TPS").is_err() {
        eprintln!("skipping run_tps_suite (set HELIOS_TPS=1 to run)");
        return;
    }
    let mode = std::env::var("HELIOS_TPS_MODE").unwrap_or_else(|_| "mem".into());
    let n: usize = std::env::var("HELIOS_TPS_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50_000);
    // Number of single-op (autocommit) statements for the latency-bound metrics.
    let m: usize = std::env::var("HELIOS_TPS_M")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or((n / 5).max(2_000));

    let tmp = std::env::temp_dir().join(format!("helios_tps_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    println!("\n================ HeliosDB-Nano TPS suite ================");
    println!("mode={mode}  N={n}  M={m}  dir={}", tmp.display());
    println!("{}", "-".repeat(80));

    let db = make_db(&mode, &tmp).expect("db open");

    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, email TEXT, age INTEGER, balance INTEGER)")
        .unwrap();
    db.execute("CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, amount INTEGER, status TEXT)")
        .unwrap();

    // 1) Bulk insert N users in one transaction (batch write throughput).
    bench("bulk_insert_users(txn)", n, || {
        db.execute("BEGIN")?;
        for i in 0..n {
            db.execute(&format!(
                "INSERT INTO users (id, name, email, age, balance) VALUES ({i}, 'User{i}', 'u{i}@ex.com', {}, {})",
                18 + (i % 60),
                (i * 7) % 100000
            ))?;
        }
        db.execute("COMMIT")?;
        Ok(())
    });

    // 1b) Bulk insert orders (for joins/aggregates), 2 orders per user.
    {
        db.execute("BEGIN").unwrap();
        for i in 0..(n * 2) {
            let uid = i % n;
            db.execute(&format!(
                "INSERT INTO orders (id, user_id, amount, status) VALUES ({i}, {uid}, {}, '{}')",
                (i * 13) % 5000,
                if i % 3 == 0 { "paid" } else { "pending" }
            ))
            .unwrap();
        }
        db.execute("COMMIT").unwrap();
    }

    // 2) Autocommit insert M rows (each its own implicit txn) — the durable OLTP write TPS.
    bench("autocommit_insert", m, || {
        for i in 0..m {
            let id = n + i;
            db.execute(&format!(
                "INSERT INTO users (id, name, email, age, balance) VALUES ({id}, 'AC{id}', 'ac{id}@ex.com', 33, 500)"
            ))?;
        }
        Ok(())
    });

    // 3) Point lookup by PK (read TPS).
    bench("point_lookup_pk", m, || {
        for i in 0..m {
            let id = (i * 2654435761usize) % n;
            let r = db.query(&format!("SELECT * FROM users WHERE id = {id}"), &[])?;
            assert!(!r.is_empty());
        }
        Ok(())
    });

    // 4) Point lookup, repeated hot key (row-cache / result-cache path).
    bench("point_lookup_hot", m, || {
        for _ in 0..m {
            let _ = db.query("SELECT * FROM users WHERE id = 12345", &[])?;
        }
        Ok(())
    });

    // 5) Update by PK (autocommit) — durable write TPS for updates.
    bench("update_by_pk", m, || {
        for i in 0..m {
            let id = (i * 40503usize) % n;
            db.execute(&format!("UPDATE users SET balance = balance + 1 WHERE id = {id}"))?;
        }
        Ok(())
    });

    // 6) Delete by PK (autocommit) — delete the autocommit-inserted rows.
    bench("delete_by_pk", m, || {
        for i in 0..m {
            let id = n + i;
            db.execute(&format!("DELETE FROM users WHERE id = {id}"))?;
        }
        Ok(())
    });

    // 7) Filtered scan (non-indexed predicate over N rows).
    let scan_iters = 20usize;
    bench("filter_scan(age>50)", scan_iters, || {
        for _ in 0..scan_iters {
            let _ = db.query("SELECT id, name FROM users WHERE age > 50", &[])?;
        }
        Ok(())
    });

    // 8) Aggregate: COUNT + SUM + AVG, no group.
    bench("agg_count_sum_avg", scan_iters, || {
        for _ in 0..scan_iters {
            let _ = db.query("SELECT COUNT(*), SUM(balance), AVG(age) FROM users", &[])?;
        }
        Ok(())
    });

    // 9) GROUP BY aggregate.
    bench("group_by_status", scan_iters, || {
        for _ in 0..scan_iters {
            let _ = db.query("SELECT status, COUNT(*), SUM(amount) FROM orders GROUP BY status", &[])?;
        }
        Ok(())
    });

    // 10) JOIN users x orders with filter.
    let join_iters = 10usize;
    bench("join_users_orders", join_iters, || {
        for _ in 0..join_iters {
            let _ = db.query(
                "SELECT u.name, o.amount FROM users u INNER JOIN orders o ON u.id = o.user_id WHERE o.status = 'paid' AND u.age > 40",
                &[],
            )?;
        }
        Ok(())
    });

    // 11) ORDER BY ... LIMIT (top-N).
    bench("order_by_limit10", scan_iters, || {
        for _ in 0..scan_iters {
            let _ = db.query("SELECT id, balance FROM users ORDER BY balance DESC LIMIT 10", &[])?;
        }
        Ok(())
    });

    println!("{}", "-".repeat(80));
    println!("done.\n");
    let _ = std::fs::remove_dir_all(&tmp);
}
