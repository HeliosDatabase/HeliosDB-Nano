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
//! HELIOS_TPS_EMBEDDED_PROFILE:
//!   rowstore             -> default benchmark schema and SQL shapes
//!   columnar_analytics   -> in-memory-only A/B profile using STORAGE COLUMNAR
//!                           for analytical columns and columnar-friendly projections
//!   oltp_fast            -> in-memory-only OLTP ceiling: rowstore schema,
//!                           time travel off by default, memory quota accounting off
//! HELIOS_TPS_TIME_TRAVEL=0 disables MVCC version-key maintenance for write-path diagnosis.

use heliosdb_nano::config::WalSyncModeConfig;
use heliosdb_nano::{Config, EmbeddedDatabase, Result, Value};
use std::collections::HashSet;
use std::time::Instant;

/// Concurrent point-lookup benchmark (P0#4 row-cache read-lock).
/// Compares aggregate hot-row lookup throughput as thread count scales.
/// HELIOS_ROWCACHE_LEGACY=1 forces the legacy exclusive-write-lock read path.
///   HELIOS_CACHE_CONC=1 cargo test --profile perf --test tps_workloads run_cache_concurrency_bench -- --nocapture --test-threads=1
#[test]
fn run_cache_concurrency_bench() {
    if std::env::var("HELIOS_CACHE_CONC").is_err() {
        eprintln!("skipping run_cache_concurrency_bench (set HELIOS_CACHE_CONC=1)");
        return;
    }
    let legacy = std::env::var("HELIOS_ROWCACHE_LEGACY").is_ok();
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE hot (id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    db.execute("BEGIN").unwrap();
    for i in 0..2000 {
        db.execute(&format!("INSERT INTO hot (id, v) VALUES ({i}, {})", i * 2))
            .unwrap();
    }
    db.execute("COMMIT").unwrap();
    // Warm the row cache over the hot set.
    for k in 0..2000 {
        let _ = db.query(&format!("SELECT * FROM hot WHERE id = {k}"), &[]).unwrap();
    }

    let per_thread = 200_000usize;
    println!(
        "\n=== cache concurrency: read path = {} ===",
        if legacy { "LEGACY write-lock" } else { "read-lock+peek" }
    );
    for &threads in &[1usize, 4, 16] {
        let start = Instant::now();
        std::thread::scope(|s| {
            for t in 0..threads {
                let dbr = &db;
                s.spawn(move || {
                    let mut id = (t * 7919) % 2000;
                    for _ in 0..per_thread {
                        id = (id + 7919) % 2000;
                        let _ = dbr.query(&format!("SELECT * FROM hot WHERE id = {id}"), &[]).unwrap();
                    }
                });
            }
        });
        let total = threads * per_thread;
        let secs = start.elapsed().as_secs_f64();
        println!(
            "{threads:>3} threads  {:>12.0} lookups/s  ({:.2} M total in {:.2}s)",
            total as f64 / secs,
            total as f64 / 1e6,
            secs
        );
    }
    println!();
}

/// Analytics scan benchmark over a large table (P1#6 parallel-decode).
/// Set HELIOS_SCAN_SERIAL=1 to force the serial decode path for A/B comparison.
///   HELIOS_SCAN=1 HELIOS_SCAN_N=300000 cargo test --profile perf --test tps_workloads run_scan_bench -- --nocapture --test-threads=1
#[test]
fn run_scan_bench() {
    if std::env::var("HELIOS_SCAN").is_err() {
        eprintln!("skipping run_scan_bench (set HELIOS_SCAN=1)");
        return;
    }
    let n: usize = std::env::var("HELIOS_SCAN_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300_000);
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
    bench("count_star(all)", &|r| {
        format!("SELECT COUNT(*) FROM wide{}", " ".repeat(r + 1))
    });
    bench("count_star(id>=r)", &|r| {
        format!("SELECT COUNT(*) FROM wide WHERE id >= {r}")
    });
    bench("count_star(id=r)", &|r| {
        format!("SELECT COUNT(*) FROM wide WHERE id = {r}")
    });
    bench("count_star(id IN)", &|r| {
        format!(
            "SELECT COUNT(*) FROM wide WHERE id IN ({}, {}, {}, {}, {})",
            r,
            r + 1,
            r + 1,
            n + r,
            n / 2
        )
    });
    bench("count_star(id range)", &|r| {
        format!("SELECT COUNT(*) FROM wide WHERE id >= {r} AND id <= {}", r + n / 2)
    });
    bench("count_pk(id IN)", &|r| {
        format!(
            "SELECT COUNT(id) FROM wide WHERE id IN ({}, {}, {}, {}, {})",
            r,
            r + 1,
            r + 1,
            n + r,
            n / 2
        )
    });
    bench("count_distinct_expr(range)", &|r| {
        format!(
            "SELECT COUNT(DISTINCT id + 0) FROM wide WHERE id >= {r} AND id <= {}",
            r + n / 2
        )
    });
    bench("count_distinct_pk(range)", &|r| {
        format!(
            "SELECT COUNT(DISTINCT id) FROM wide WHERE id >= {r} AND id <= {}",
            r + n / 2
        )
    });
    bench("agg_sum_avg", &|r| {
        format!("SELECT SUM(a), AVG(d), MAX(b) FROM wide WHERE b >= {r}")
    });
    bench("group_by_e", &|r| {
        format!("SELECT e, COUNT(*), SUM(a) FROM wide WHERE b >= {r} GROUP BY e")
    });
    println!();
}

/// A/B benchmark for the columnar-native scan path. It compares identical
/// analytical queries over a default row-store table and a table whose referenced
/// scan columns are `STORAGE COLUMNAR`.
///
///   HELIOS_COLUMNAR_SCAN=1 HELIOS_COLUMNAR_SCAN_N=50000 cargo test --profile perf --test tps_workloads run_columnar_scan_bench -- --nocapture --test-threads=1
#[test]
fn run_columnar_scan_bench() {
    if std::env::var("HELIOS_COLUMNAR_SCAN").is_err() {
        eprintln!("skipping run_columnar_scan_bench (set HELIOS_COLUMNAR_SCAN=1)");
        return;
    }

    let n: usize = std::env::var("HELIOS_COLUMNAR_SCAN_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50_000);
    let runs = 5usize;
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    db.execute("CREATE TABLE row_wide (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER, c TEXT, d INTEGER, e INTEGER)")
        .unwrap();
    db.execute(
        "CREATE TABLE col_wide (
            id INTEGER PRIMARY KEY,
            a INTEGER STORAGE COLUMNAR,
            b INTEGER STORAGE COLUMNAR,
            c TEXT,
            d INTEGER STORAGE COLUMNAR,
            e INTEGER STORAGE COLUMNAR
        )",
    )
    .unwrap();

    db.execute("BEGIN").unwrap();
    for i in 0..n {
        let a = i % 1000;
        let b = (i * 7) % 100000;
        let d = (i * 13) % 100000;
        let e = i % 8;
        db.execute(&format!(
            "INSERT INTO row_wide (id,a,b,c,d,e) VALUES ({i},{a},{b},'row{i}',{d},{e})"
        ))
        .unwrap();
        db.execute(&format!(
            "INSERT INTO col_wide (id,a,b,c,d,e) VALUES ({i},{a},{b},'row{i}',{d},{e})"
        ))
        .unwrap();
    }
    db.execute("COMMIT").unwrap();

    println!("\n=== columnar scan bench: N={n} ===");
    let bench_pair = |label: &str, row_sql: &dyn Fn(usize) -> String, col_sql: &dyn Fn(usize) -> String| {
        let time_query = |mk: &dyn Fn(usize) -> String| {
            let t = Instant::now();
            let mut rows = 0usize;
            for r in 0..runs {
                rows = db.query(&mk(r), &[]).unwrap().len();
            }
            (t.elapsed().as_secs_f64() * 1e6 / runs as f64, rows)
        };

        let (row_us, row_rows) = time_query(row_sql);
        let (col_us, col_rows) = time_query(col_sql);
        assert_eq!(row_rows, col_rows, "{label} row/columnar row counts differ");
        println!(
            "{label:<24} row={row_us:>10.1} us/query  columnar={col_us:>10.1} us/query  speedup={:>5.2}x  ({col_rows} rows)",
            row_us / col_us
        );
    };

    bench_pair(
        "filter_scan",
        &|r| format!("SELECT a FROM row_wide WHERE d > 50000 AND b >= {r}"),
        &|r| format!("SELECT a FROM col_wide WHERE d > 50000 AND b >= {r}"),
    );
    bench_pair(
        "filter_eq_e",
        &|r| format!("SELECT a FROM row_wide WHERE e = {}", (r + 3) % 8),
        &|r| format!("SELECT a FROM col_wide WHERE e = {}", (r + 3) % 8),
    );
    bench_pair(
        "agg_sum_avg",
        &|r| format!("SELECT SUM(a), AVG(d), MAX(b) FROM row_wide WHERE b >= {r}"),
        &|r| format!("SELECT SUM(a), AVG(d), MAX(b) FROM col_wide WHERE b >= {r}"),
    );
    bench_pair(
        "agg_no_filter",
        &|r| {
            format!(
                "SELECT SUM(a), AVG(d), MAX(b), COUNT(a) FROM row_wide{}",
                " ".repeat(r + 1)
            )
        },
        &|r| {
            format!(
                "SELECT SUM(a), AVG(d), MAX(b), COUNT(a) FROM col_wide{}",
                " ".repeat(r + 1)
            )
        },
    );
    bench_pair(
        "count_star_filter",
        &|r| format!("SELECT COUNT(*) FROM row_wide WHERE b >= {r}"),
        &|r| format!("SELECT COUNT(*) FROM col_wide WHERE b >= {r}"),
    );
    bench_pair(
        "count_distinct_a",
        &|r| format!("SELECT COUNT(DISTINCT a) FROM row_wide WHERE b >= {r}"),
        &|r| format!("SELECT COUNT(DISTINCT a) FROM col_wide WHERE b >= {r}"),
    );
    bench_pair(
        "group_by_e",
        &|r| format!("SELECT e, COUNT(*), SUM(a) FROM row_wide WHERE b >= {r} GROUP BY e"),
        &|r| format!("SELECT e, COUNT(*), SUM(a) FROM col_wide WHERE b >= {r} GROUP BY e"),
    );
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

fn selected_workloads_env(name: &str) -> Option<HashSet<String>> {
    let raw = std::env::var(name).ok()?;
    let selected: HashSet<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| item.to_ascii_lowercase())
        .collect();
    (!selected.is_empty()).then_some(selected)
}

fn workload_enabled(selected: Option<&HashSet<String>>, label: &str, aliases: &[&str]) -> bool {
    let Some(selected) = selected else {
        return true;
    };
    selected.contains("all")
        || selected.contains(&label.to_ascii_lowercase())
        || aliases
            .iter()
            .any(|alias| selected.contains(&alias.to_ascii_lowercase()))
}

fn print_selected_workloads(selected: &HashSet<String>) {
    println!("selected workloads: {}", {
        let mut names: Vec<_> = selected.iter().map(String::as_str).collect();
        names.sort_unstable();
        names.join(",")
    });
    println!("{}", "-".repeat(80));
}

fn no_result_cache(sql: &str) -> String {
    // query() intentionally skips its result cache for SQL text containing
    // NOW(. Keep the marker inside a comment so the benchmark measures the
    // operator path with normal plan-cache reuse and unchanged SQL semantics.
    format!("{sql} /* NOW(helios_tps_uncached) */")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TpsEmbeddedProfile {
    RowStore,
    ColumnarAnalytics,
    OltpFast,
}

impl TpsEmbeddedProfile {
    fn from_env() -> Self {
        match std::env::var("HELIOS_TPS_EMBEDDED_PROFILE") {
            Ok(value) => match value.as_str() {
                "rowstore" | "row_store" | "default" => Self::RowStore,
                "columnar_analytics" | "columnar" | "analytics_columnar" => Self::ColumnarAnalytics,
                "oltp_fast" | "fast_oltp" | "mem_oltp_fast" => Self::OltpFast,
                other => panic!("unknown HELIOS_TPS_EMBEDDED_PROFILE: {other}"),
            },
            Err(_) => Self::RowStore,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::RowStore => "rowstore",
            Self::ColumnarAnalytics => "columnar_analytics",
            Self::OltpFast => "oltp_fast",
        }
    }

    fn require_supported(self, mode: &str) {
        if self != Self::RowStore && mode != "mem" {
            panic!(
                "HELIOS_TPS_EMBEDDED_PROFILE={} is currently gated to HELIOS_TPS_MODE=mem",
                self.label()
            );
        }
    }

    fn create_tables(self, db: &EmbeddedDatabase) -> Result<()> {
        match self {
            Self::RowStore | Self::OltpFast => {
                db.execute(
                    "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, email TEXT, age INTEGER, balance INTEGER)",
                )?;
                db.execute(
                    "CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, amount INTEGER, status TEXT)",
                )?;
            }
            Self::ColumnarAnalytics => {
                db.execute(
                    "CREATE TABLE users (
                        id INTEGER PRIMARY KEY,
                        name TEXT,
                        email TEXT,
                        age INTEGER STORAGE COLUMNAR,
                        balance INTEGER STORAGE COLUMNAR
                    )",
                )?;
                db.execute(
                    "CREATE TABLE orders (
                        id INTEGER PRIMARY KEY,
                        user_id INTEGER STORAGE COLUMNAR,
                        amount INTEGER STORAGE COLUMNAR,
                        status TEXT STORAGE COLUMNAR
                    )",
                )?;
            }
        }
        Ok(())
    }

    fn filter_scan_sql(self) -> &'static str {
        match self {
            Self::RowStore | Self::OltpFast => "SELECT id, name FROM users WHERE age > 50",
            // Keep this profile diagnostic: all referenced columns are columnar,
            // so the storage scan can avoid full row materialization.
            Self::ColumnarAnalytics => "SELECT age, balance FROM users WHERE age > 50",
        }
    }

    fn topn_sql(self) -> &'static str {
        match self {
            Self::RowStore | Self::OltpFast => "SELECT id, balance FROM users ORDER BY balance DESC LIMIT 10",
            Self::ColumnarAnalytics => "SELECT age, balance FROM users ORDER BY balance DESC LIMIT 10",
        }
    }

    fn default_time_travel(self) -> bool {
        !matches!(self, Self::OltpFast)
    }

    fn apply_config(self, config: &mut Config) {
        if matches!(self, Self::OltpFast) {
            config.resource_quotas.memory_limit_per_user_mb = 0;
        }
    }
}

fn env_bool_enabled(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(v) if matches!(v.as_str(), "0" | "false" | "FALSE" | "off" | "OFF" | "no" | "NO") => false,
        Ok(_) => true,
        Err(_) => default,
    }
}

fn apply_tps_overrides(config: &mut Config, embedded_profile: TpsEmbeddedProfile) {
    config.storage.time_travel_enabled =
        env_bool_enabled("HELIOS_TPS_TIME_TRAVEL", embedded_profile.default_time_travel());
    embedded_profile.apply_config(config);
}

fn make_db(mode: &str, dir: &std::path::Path, embedded_profile: TpsEmbeddedProfile) -> Result<EmbeddedDatabase> {
    match mode {
        "mem" => {
            let mut c = Config::in_memory();
            apply_tps_overrides(&mut c, embedded_profile);
            EmbeddedDatabase::with_config(c)
        }
        "disk" => {
            let mut c = Config::default();
            c.storage.path = Some(dir.to_path_buf());
            c.storage.memory_only = false;
            c.storage.wal_enabled = true;
            c.storage.wal_sync_mode = WalSyncModeConfig::Sync;
            c.storage.logical_wal_per_statement = std::env::var("HELIOS_LOGICAL_WAL").is_ok();
            apply_tps_overrides(&mut c, embedded_profile);
            EmbeddedDatabase::with_config(c)
        }
        "disk_group" => {
            let mut c = Config::default();
            c.storage.path = Some(dir.to_path_buf());
            c.storage.memory_only = false;
            c.storage.wal_enabled = true;
            c.storage.wal_sync_mode = WalSyncModeConfig::GroupCommit;
            apply_tps_overrides(&mut c, embedded_profile);
            EmbeddedDatabase::with_config(c)
        }
        "disk_nowal" => {
            let mut c = Config::default();
            c.storage.path = Some(dir.to_path_buf());
            c.storage.memory_only = false;
            c.storage.wal_enabled = false;
            apply_tps_overrides(&mut c, embedded_profile);
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
        // UPDATE by pk with EXPRESSION RHS (`v = v + 1`) — P0#3 fast_eval_simple_expr path
        let t = Instant::now();
        for k in 0..reps {
            db.execute(&format!("UPDATE t SET v = v + 1 WHERE id = {}", (k * 7919) % rows))
                .unwrap();
        }
        let upd_expr_us = t.elapsed().as_secs_f64() * 1e6 / reps as f64;
        eprintln!("    [rows={rows}] UPDATE literal={upd_us:.1}us  UPDATE expr(v+1)={upd_expr_us:.1}us");
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
    let embedded_profile = TpsEmbeddedProfile::from_env();
    embedded_profile.require_supported(&mode);

    println!("\n================ HeliosDB-Nano TPS suite ================");
    println!(
        "mode={mode}  profile={}  N={n}  M={m}  time_travel={}  dir={}",
        embedded_profile.label(),
        env_bool_enabled("HELIOS_TPS_TIME_TRAVEL", embedded_profile.default_time_travel()),
        tmp.display()
    );
    println!("{}", "-".repeat(80));

    let db = make_db(&mode, &tmp, embedded_profile).expect("db open");
    let selected = selected_workloads_env("HELIOS_TPS_WORKLOADS");
    if let Some(selected) = &selected {
        print_selected_workloads(selected);
    }

    embedded_profile.create_tables(&db).unwrap();

    let run_bulk = workload_enabled(selected.as_ref(), "bulk_insert_users(txn)", &["bulk_insert", "bulk"]);
    let run_autocommit = workload_enabled(selected.as_ref(), "autocommit_insert", &["autocommit_insert", "insert"]);
    let run_point = workload_enabled(selected.as_ref(), "point_lookup_pk", &["point_lookup", "lookup"]);
    let run_hot = workload_enabled(selected.as_ref(), "point_lookup_hot", &["hot_lookup", "lookup_hot"]);
    let run_update = workload_enabled(selected.as_ref(), "update_by_pk", &["update"]);
    let run_delete = workload_enabled(selected.as_ref(), "delete_by_pk", &["delete"]);
    let run_filter = workload_enabled(selected.as_ref(), "filter_scan(age>50)", &["filter_scan", "filter"]);
    let run_agg = workload_enabled(selected.as_ref(), "agg_count_sum_avg", &["aggregate", "agg"]);
    let run_group = workload_enabled(selected.as_ref(), "group_by_status", &["group_by", "group"]);
    let run_join = workload_enabled(selected.as_ref(), "join_users_orders", &["join"]);
    let run_topn = workload_enabled(selected.as_ref(), "order_by_limit10", &["order_by", "topn", "top_n"]);

    let need_base_users =
        run_bulk || run_point || run_hot || run_update || run_filter || run_agg || run_join || run_topn;

    // 1) Bulk insert N users in one transaction (batch write throughput).
    let load_users = || -> Result<()> {
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
    };
    if run_bulk {
        bench("bulk_insert_users(txn)", n, load_users);
    } else if need_base_users {
        load_users().unwrap();
    }

    // 1b) Bulk insert orders (for joins/aggregates), 2 orders per user.
    if run_group || run_join {
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
    let load_autocommit_rows = || -> Result<()> {
        for i in 0..m {
            let id = n + i;
            db.execute(&format!(
                "INSERT INTO users (id, name, email, age, balance) VALUES ({id}, 'AC{id}', 'ac{id}@ex.com', 33, 500)"
            ))?;
        }
        Ok(())
    };
    if run_autocommit {
        bench("autocommit_insert", m, load_autocommit_rows);
    } else if run_delete {
        load_autocommit_rows().unwrap();
    }

    // 3) Point lookup by PK (read TPS).
    if run_point {
        bench("point_lookup_pk", m, || {
            for i in 0..m {
                let id = (i * 2654435761usize) % n;
                let r = db.query(&format!("SELECT * FROM users WHERE id = {id}"), &[])?;
                assert!(!r.is_empty());
            }
            Ok(())
        });
    }

    // 4) Point lookup, repeated hot key (row-cache / result-cache path).
    let hot_id = 12345usize.min(n - 1);
    let hot_sql = format!("SELECT * FROM users WHERE id = {hot_id}");
    if run_hot {
        bench("point_lookup_hot", m, || {
            for _ in 0..m {
                let _ = db.query(&hot_sql, &[])?;
            }
            Ok(())
        });
    }

    // 5) Update by PK (autocommit) — durable write TPS for updates.
    if run_update {
        bench("update_by_pk", m, || {
            for i in 0..m {
                let id = (i * 40503usize) % n;
                db.execute(&format!("UPDATE users SET balance = balance + 1 WHERE id = {id}"))?;
            }
            Ok(())
        });
    }

    // 6) Delete by PK (autocommit) — delete the autocommit-inserted rows.
    if run_delete {
        bench("delete_by_pk", m, || {
            for i in 0..m {
                let id = n + i;
                db.execute(&format!("DELETE FROM users WHERE id = {id}"))?;
            }
            Ok(())
        });
    }

    // 7) Filtered scan (non-indexed predicate over N rows).
    let scan_iters = 20usize;
    if run_filter {
        bench("filter_scan(age>50)", scan_iters, || {
            let sql = no_result_cache(embedded_profile.filter_scan_sql());
            for _ in 0..scan_iters {
                let _ = db.query(&sql, &[])?;
            }
            Ok(())
        });
    }

    // 8) Aggregate: COUNT + SUM + AVG, no group.
    if run_agg {
        bench("agg_count_sum_avg", scan_iters, || {
            let sql = no_result_cache("SELECT COUNT(*), SUM(balance), AVG(age) FROM users");
            for _ in 0..scan_iters {
                let _ = db.query(&sql, &[])?;
            }
            Ok(())
        });
    }

    // 9) GROUP BY aggregate.
    if run_group {
        bench("group_by_status", scan_iters, || {
            let sql = no_result_cache("SELECT status, COUNT(*), SUM(amount) FROM orders GROUP BY status");
            for _ in 0..scan_iters {
                let _ = db.query(&sql, &[])?;
            }
            Ok(())
        });
    }

    // 10) JOIN users x orders with filter.
    let join_iters = 10usize;
    if run_join {
        bench("join_users_orders", join_iters, || {
            let sql = no_result_cache(
            "SELECT u.name, o.amount FROM users u INNER JOIN orders o ON u.id = o.user_id WHERE o.status = 'paid' AND u.age > 40",
        );
            for _ in 0..join_iters {
                let _ = db.query(&sql, &[])?;
            }
            Ok(())
        });
    }

    // 11) ORDER BY ... LIMIT (top-N).
    if run_topn {
        bench("order_by_limit10", scan_iters, || {
            let sql = no_result_cache(embedded_profile.topn_sql());
            for _ in 0..scan_iters {
                let _ = db.query(&sql, &[])?;
            }
            Ok(())
        });
    }

    println!("{}", "-".repeat(80));
    println!("done.\n");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Parameterized DML TPS harness. This mirrors the write-heavy part of
/// `run_tps_suite`, but uses `execute_params` / `query_params`, which is the
/// path used by PG-wire extended protocol clients and sqlite-style bindings.
#[test]
fn run_param_tps_suite() {
    if std::env::var("HELIOS_TPS_PARAMS").is_err() {
        eprintln!("skipping run_param_tps_suite (set HELIOS_TPS_PARAMS=1 to run)");
        return;
    }
    let mode = std::env::var("HELIOS_TPS_MODE").unwrap_or_else(|_| "mem".into());
    let n: usize = std::env::var("HELIOS_TPS_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50_000);
    let m: usize = std::env::var("HELIOS_TPS_M")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or((n / 5).max(2_000));

    let tmp = std::env::temp_dir().join(format!("helios_param_tps_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let embedded_profile = TpsEmbeddedProfile::from_env();
    embedded_profile.require_supported(&mode);

    println!("\n============= HeliosDB-Nano parameterized TPS =============");
    println!(
        "mode={mode}  profile={}  N={n}  M={m}  time_travel={}  dir={}",
        embedded_profile.label(),
        env_bool_enabled("HELIOS_TPS_TIME_TRAVEL", embedded_profile.default_time_travel()),
        tmp.display()
    );
    println!("{}", "-".repeat(80));

    let db = make_db(&mode, &tmp, embedded_profile).expect("db open");
    let selected =
        selected_workloads_env("HELIOS_TPS_PARAM_WORKLOADS").or_else(|| selected_workloads_env("HELIOS_TPS_WORKLOADS"));
    if let Some(selected) = &selected {
        print_selected_workloads(selected);
    }

    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, email TEXT, age INTEGER, balance INTEGER)")
        .unwrap();
    db.execute("CREATE TABLE users_many (id INTEGER PRIMARY KEY, name TEXT, email TEXT, age INTEGER, balance INTEGER)")
        .unwrap();

    let run_bulk = workload_enabled(
        selected.as_ref(),
        "param_bulk_insert(txn)",
        &["param_bulk_insert", "bulk_insert", "bulk"],
    );
    let run_execute_many = workload_enabled(
        selected.as_ref(),
        "param_execute_many_insert",
        &["execute_many", "execute_many_insert", "param_execute_many"],
    );
    let run_autocommit = workload_enabled(
        selected.as_ref(),
        "param_autocommit_insert",
        &["param_autocommit", "autocommit_insert", "autocommit", "insert"],
    );
    let run_point = workload_enabled(
        selected.as_ref(),
        "param_point_lookup_pk",
        &["param_point_lookup", "point_lookup_pk", "point_lookup", "lookup"],
    );
    let run_update = workload_enabled(
        selected.as_ref(),
        "param_update_by_pk",
        &["param_update", "update_by_pk", "update"],
    );
    let run_execute_many_update = workload_enabled(
        selected.as_ref(),
        "param_execute_many_update",
        &["execute_many_update", "param_update_many", "update_many"],
    );
    let run_delete = workload_enabled(
        selected.as_ref(),
        "param_delete_by_pk",
        &["param_delete", "delete_by_pk", "delete"],
    );
    let run_execute_many_delete = workload_enabled(
        selected.as_ref(),
        "param_execute_many_delete",
        &["execute_many_delete", "param_delete_many", "delete_many"],
    );
    let need_base_users = run_bulk || run_point || run_update || run_execute_many_update;

    let load_users = || -> Result<()> {
        db.execute("BEGIN")?;
        for i in 0..n {
            db.execute_params(
                "INSERT INTO users (id, name, email, age, balance) VALUES ($1, $2, $3, $4, $5)",
                &[
                    Value::Int8(i as i64),
                    Value::String(format!("User{i}")),
                    Value::String(format!("u{i}@ex.com")),
                    Value::Int4(18 + (i % 60) as i32),
                    Value::Int8(((i * 7) % 100000) as i64),
                ],
            )?;
        }
        db.execute("COMMIT")?;
        Ok(())
    };
    if run_bulk {
        bench("param_bulk_insert(txn)", n, load_users);
    } else if need_base_users {
        load_users().unwrap();
    }

    if run_execute_many {
        bench("param_execute_many_insert", n, || {
            let rows: Vec<Vec<Value>> = (0..n)
                .map(|i| {
                    vec![
                        Value::Int8(i as i64),
                        Value::String(format!("User{i}")),
                        Value::String(format!("u{i}@ex.com")),
                        Value::Int4(18 + (i % 60) as i32),
                        Value::Int8(((i * 7) % 100000) as i64),
                    ]
                })
                .collect();
            db.execute_many_params(
                "INSERT INTO users_many (id, name, email, age, balance) VALUES ($1, $2, $3, $4, $5)",
                &rows,
            )?;
            Ok(())
        });
    }

    let load_autocommit_rows = || -> Result<()> {
        for i in 0..m {
            let id = n + i;
            db.execute_params(
                "INSERT INTO users (id, name, email, age, balance) VALUES ($1, $2, $3, $4, $5)",
                &[
                    Value::Int8(id as i64),
                    Value::String(format!("AC{id}")),
                    Value::String(format!("ac{id}@ex.com")),
                    Value::Int4(33),
                    Value::Int8(500),
                ],
            )?;
        }
        Ok(())
    };
    if run_autocommit {
        bench("param_autocommit_insert", m, load_autocommit_rows);
    } else if run_delete {
        load_autocommit_rows().unwrap();
    }

    let load_execute_many_delete_rows = || -> Result<()> {
        for i in 0..m {
            let id = n + m + i;
            db.execute_params(
                "INSERT INTO users (id, name, email, age, balance) VALUES ($1, $2, $3, $4, $5)",
                &[
                    Value::Int8(id as i64),
                    Value::String(format!("DM{id}")),
                    Value::String(format!("dm{id}@ex.com")),
                    Value::Int4(34),
                    Value::Int8(600),
                ],
            )?;
        }
        Ok(())
    };

    if run_point {
        bench("param_point_lookup_pk", m, || {
            for i in 0..m {
                let id = (i * 2654435761usize) % n;
                let rows = db.query_params("SELECT * FROM users WHERE id = $1", &[Value::Int8(id as i64)])?;
                assert!(!rows.is_empty());
            }
            Ok(())
        });
    }

    if run_update {
        bench("param_update_by_pk", m, || {
            for i in 0..m {
                let id = (i * 40503usize) % n;
                db.execute_params(
                    "UPDATE users SET balance = balance + 1 WHERE id = $1",
                    &[Value::Int8(id as i64)],
                )?;
            }
            Ok(())
        });
    }

    if run_execute_many_update {
        let rows: Vec<Vec<Value>> = (0..m)
            .map(|i| vec![Value::Int8(((i * 40503usize) % n) as i64)])
            .collect();
        bench("param_execute_many_update", m, || {
            db.execute_many_params("UPDATE users SET balance = balance + 1 WHERE id = $1", &rows)?;
            Ok(())
        });
    }

    if run_delete {
        bench("param_delete_by_pk", m, || {
            for i in 0..m {
                let id = n + i;
                db.execute_params("DELETE FROM users WHERE id = $1", &[Value::Int8(id as i64)])?;
            }
            Ok(())
        });
    }

    if run_execute_many_delete {
        load_execute_many_delete_rows().unwrap();
        let rows: Vec<Vec<Value>> = (0..m).map(|i| vec![Value::Int8((n + m + i) as i64)]).collect();
        bench("param_execute_many_delete", m, || {
            db.execute_many_params("DELETE FROM users WHERE id = $1", &rows)?;
            Ok(())
        });
    }

    println!("{}", "-".repeat(80));
    println!("done.\n");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Per-session transaction benchmark (R0.1 direct workloads).
///
/// Measures the transaction-cycle shapes the wire handlers use, before and
/// after the per-session-transaction migration:
///   - global_txn_cycle(1T): BEGIN; INSERT; COMMIT through the global slot
///     (the legacy wire path; >1 thread is impossible — second BEGIN errors)
///   - session_txn_cycle(1/4/16T): the same cycle through the per-session API,
///     one session per thread, disjoint key ranges
///   - session_autocommit_insert(1T): execute_in_session with no open txn
///     (the implicit-session-transaction arm)
///   - autocommit_insert_with_open_session_txn(1T): plain execute() autocommit
///     INSERT throughput while another session holds an open transaction
///     (measures the fast-path kill-switch gate)
///
///   HELIOS_SESSION_TXN=1 cargo test --profile perf --test tps_workloads run_session_txn_bench -- --nocapture --test-threads=1
#[test]
fn run_session_txn_bench() {
    if std::env::var("HELIOS_SESSION_TXN").is_err() {
        eprintln!("skipping run_session_txn_bench (set HELIOS_SESSION_TXN=1)");
        return;
    }
    use heliosdb_nano::session::IsolationLevel;

    let cycles: usize = std::env::var("HELIOS_SESSION_TXN_M")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2000);

    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE st (id INTEGER PRIMARY KEY, v INTEGER)").unwrap();

    println!("\n=== session transaction bench (cycles per measurement: {cycles}) ===");

    // 1) Global-slot txn cycle, single thread (legacy wire shape).
    let start = Instant::now();
    for i in 0..cycles {
        db.execute("BEGIN").unwrap();
        db.execute(&format!("INSERT INTO st (id, v) VALUES ({i}, 1)")).unwrap();
        db.execute("COMMIT").unwrap();
    }
    println!(
        "global_txn_cycle(1T)                     {:>10.0} txn/s",
        cycles as f64 / start.elapsed().as_secs_f64()
    );

    // 2) Per-session txn cycle at 1/4/16 threads, one session per thread.
    for (round, &threads) in [1usize, 4, 16].iter().enumerate() {
        let start = Instant::now();
        std::thread::scope(|s| {
            for t in 0..threads {
                let dbr = &db;
                let base = 1_000_000 * (round + 1) + t * cycles;
                s.spawn(move || {
                    let sid = dbr
                        .create_session(&format!("bench_u{t}"), IsolationLevel::ReadCommitted)
                        .unwrap();
                    for i in 0..cycles {
                        dbr.begin_transaction_for_session(sid).unwrap();
                        dbr.execute_in_session(sid, &format!("INSERT INTO st (id, v) VALUES ({}, 2)", base + i))
                            .unwrap();
                        dbr.commit_transaction_for_session(sid).unwrap();
                    }
                    dbr.destroy_session(sid).unwrap();
                });
            }
        });
        let total = threads * cycles;
        println!(
            "session_txn_cycle({threads:>2}T)                   {:>10.0} txn/s  ({total} total)",
            total as f64 / start.elapsed().as_secs_f64()
        );
    }

    // 3) Session autocommit (implicit session transaction per statement).
    {
        let sid = db
            .create_session("bench_autocommit", IsolationLevel::ReadCommitted)
            .unwrap();
        let start = Instant::now();
        for i in 0..cycles {
            db.execute_in_session(sid, &format!("INSERT INTO st (id, v) VALUES ({}, 3)", 8_000_000 + i))
                .unwrap();
        }
        println!(
            "session_autocommit_insert(1T)            {:>10.0} ops/s",
            cycles as f64 / start.elapsed().as_secs_f64()
        );
        db.destroy_session(sid).unwrap();
    }

    // 4) Plain autocommit INSERT throughput while a session txn sits open.
    {
        let sid = db
            .create_session("bench_holder", IsolationLevel::ReadCommitted)
            .unwrap();
        db.begin_transaction_for_session(sid).unwrap();
        db.execute_in_session(sid, "INSERT INTO st (id, v) VALUES (9999999, 4)")
            .unwrap();
        let start = Instant::now();
        for i in 0..cycles {
            db.execute(&format!("INSERT INTO st (id, v) VALUES ({}, 5)", 9_000_000 + i))
                .unwrap();
        }
        println!(
            "autocommit_insert_with_open_session_txn  {:>10.0} ops/s",
            cycles as f64 / start.elapsed().as_secs_f64()
        );
        db.rollback_transaction_for_session(sid).unwrap();
        db.destroy_session(sid).unwrap();
    }
    println!();
}

/// Write-write conflict benchmark (R0.2 direct workloads).
///
///   - update_txn_cycle(1/4/16T): BEGIN; UPDATE own row; COMMIT per thread —
///     disjoint keys, measures commit-validation overhead + false positives
///   - contended_counter(8T): all threads increment ONE row in RepeatableRead
///     transactions with retry-on-serialization-failure. Reports committed
///     cycles, retries, and lost updates (final - expected). At baseline
///     (no conflict detection) lost updates are nonzero; after R0.2 they
///     must be exactly zero.
///
///   HELIOS_CONFLICT=1 cargo test --profile perf --test tps_workloads run_conflict_bench -- --nocapture --test-threads=1
#[test]
fn run_conflict_bench() {
    if std::env::var("HELIOS_CONFLICT").is_err() {
        eprintln!("skipping run_conflict_bench (set HELIOS_CONFLICT=1)");
        return;
    }
    use heliosdb_nano::session::IsolationLevel;

    let cycles: usize = std::env::var("HELIOS_CONFLICT_M")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(500);

    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE cf (id INTEGER PRIMARY KEY, v INTEGER)").unwrap();
    for i in 0..64 {
        db.execute(&format!("INSERT INTO cf (id, v) VALUES ({i}, 0)")).unwrap();
    }

    println!("\n=== write-write conflict bench (cycles per measurement: {cycles}) ===");

    // 1) Disjoint-key update transaction cycles (RepeatableRead).
    for &threads in &[1usize, 4, 16] {
        let start = Instant::now();
        std::thread::scope(|s| {
            for t in 0..threads {
                let dbr = &db;
                s.spawn(move || {
                    let sid = dbr
                        .create_session(&format!("upd_u{t}"), IsolationLevel::RepeatableRead)
                        .unwrap();
                    for _ in 0..cycles {
                        dbr.begin_transaction_for_session(sid).unwrap();
                        dbr.execute_in_session(sid, &format!("UPDATE cf SET v = v + 1 WHERE id = {t}"))
                            .unwrap();
                        dbr.commit_transaction_for_session(sid).unwrap();
                    }
                    dbr.destroy_session(sid).unwrap();
                });
            }
        });
        let total = threads * cycles;
        println!(
            "update_txn_cycle({threads:>2}T, disjoint)        {:>10.0} txn/s  ({total} total)",
            total as f64 / start.elapsed().as_secs_f64()
        );
        // False-positive check: every disjoint commit must have applied.
        for t in 0..threads {
            let rows = db.query(&format!("SELECT * FROM cf WHERE id = {t}"), &[]).unwrap();
            let v = match rows[0].values[1] {
                Value::Int4(x) => x as i64,
                Value::Int8(x) => x,
                _ => -1,
            };
            assert!(v >= cycles as i64, "disjoint update lost rows: id={t} v={v} expected>={cycles}");
        }
    }

    // 2) Contended counter: 8 threads increment one shared row, retrying on
    //    serialization failure.
    {
        db.execute("UPDATE cf SET v = 0 WHERE id = 63").unwrap();
        let threads = 8usize;
        let committed = std::sync::atomic::AtomicU64::new(0);
        let retries = std::sync::atomic::AtomicU64::new(0);
        let zero_matches = std::sync::atomic::AtomicU64::new(0);
        let start = Instant::now();
        std::thread::scope(|s| {
            for t in 0..threads {
                let dbr = &db;
                let committed = &committed;
                let retries = &retries;
                let zero_matches = &zero_matches;
                s.spawn(move || {
                    let sid = dbr
                        .create_session(&format!("cnt_u{t}"), IsolationLevel::RepeatableRead)
                        .unwrap();
                    for _ in 0..cycles {
                        loop {
                            dbr.begin_transaction_for_session(sid).unwrap();
                            match dbr.execute_in_session(sid, "UPDATE cf SET v = v + 1 WHERE id = 63") {
                                Err(_) => {
                                    let _ = dbr.rollback_transaction_for_session(sid);
                                    retries.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    continue;
                                }
                                Ok(n) if n != 1 => {
                                    zero_matches.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    let _ = dbr.rollback_transaction_for_session(sid);
                                    retries.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    continue;
                                }
                                Ok(_) => {}
                            }
                            match dbr.commit_transaction_for_session(sid) {
                                Ok(()) => {
                                    committed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    break;
                                }
                                Err(_) => {
                                    retries.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                        }
                    }
                    dbr.destroy_session(sid).unwrap();
                });
            }
        });
        let secs = start.elapsed().as_secs_f64();
        let rows = db.query("SELECT * FROM cf WHERE id = 63", &[]).unwrap();
        let v = match rows[0].values[1] {
            Value::Int4(x) => x as i64,
            Value::Int8(x) => x,
            _ => -1,
        };
        let committed = committed.load(std::sync::atomic::Ordering::Relaxed) as i64;
        println!(
            "contended_counter(8T)                    {:>10.0} commit/s  committed={committed} retries={} zero_matches={} final_v={v} lost_updates={}",
            committed as f64 / secs,
            retries.load(std::sync::atomic::Ordering::Relaxed),
            zero_matches.load(std::sync::atomic::Ordering::Relaxed),
            committed - v,
        );
    }
    println!();
}
