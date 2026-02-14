//! Pipeline Performance Benchmark for HeliosDB Nano
//!
//! Measures per-phase timing (parse, plan, execute, commit) for every
//! statement type across both in-memory and persistent (RocksDB) deployment modes.
//!
//! Run with: cargo test --test pipeline_performance_test -- --nocapture

use heliosdb_nano::EmbeddedDatabase;
use std::sync::{Arc, Mutex, Once};
use std::time::Instant;
use tempfile::TempDir;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;

// ─── Tracing Capture Infrastructure ─────────────────────────────────────────

/// A captured tracing event with phase name and duration
#[derive(Debug, Clone)]
struct CapturedEvent {
    phase: String,
    duration_us: u64,
    rows: Option<u64>,
}

/// Visitor that extracts phase, duration_us, and rows from tracing event fields
struct FieldVisitor {
    phase: Option<String>,
    duration_us: Option<u64>,
    rows: Option<u64>,
}

impl FieldVisitor {
    fn new() -> Self {
        Self {
            phase: None,
            duration_us: None,
            rows: None,
        }
    }
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "phase" {
            self.phase = Some(value.to_string());
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "duration_us" => self.duration_us = Some(value),
            "rows" => self.rows = Some(value),
            _ => {}
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
}

/// Custom tracing layer that captures events with `phase=` fields
struct EventCollector {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl<S> tracing_subscriber::Layer<S> for EventCollector
where
    S: tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let mut visitor = FieldVisitor::new();
        event.record(&mut visitor);

        if let (Some(phase), Some(duration_us)) = (visitor.phase, visitor.duration_us) {
            if let Ok(mut events) = self.events.lock() {
                events.push(CapturedEvent {
                    phase,
                    duration_us,
                    rows: visitor.rows,
                });
            }
        }
    }
}

/// Global tracing initialization (can only happen once per process)
static TRACING_INIT: Once = Once::new();
static mut GLOBAL_EVENTS: Option<Arc<Mutex<Vec<CapturedEvent>>>> = None;

fn setup_tracing() -> Arc<Mutex<Vec<CapturedEvent>>> {
    let events = Arc::new(Mutex::new(Vec::new()));
    let for_collector = Arc::clone(&events);
    let for_global = Arc::clone(&events);

    TRACING_INIT.call_once(move || {
        let collector = EventCollector {
            events: for_collector,
        };
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new("heliosdb_nano=trace"))
            .with(collector);
        let _ = tracing::subscriber::set_global_default(subscriber);
        // Safety: only written once inside call_once
        unsafe {
            GLOBAL_EVENTS = Some(for_global);
        }
    });

    // If already initialized by a previous call, return the global reference
    // Safety: GLOBAL_EVENTS is only written once in call_once above
    unsafe {
        GLOBAL_EVENTS.as_ref().map_or(events, Arc::clone)
    }
}

// ─── Metrics Collection ─────────────────────────────────────────────────────

/// Aggregated per-statement pipeline metrics
#[derive(Debug, Clone)]
struct PipelineMetrics {
    label: String,
    wall_time_us: u64,
    parse_us: u64,
    plan_us: u64,
    execute_us: u64,
    txn_begin_us: u64,
    txn_commit_us: u64,
    operator_build_us: u64,
    operator_exec_us: u64,
    rows: u64,
}

impl PipelineMetrics {
    fn traced_total(&self) -> u64 {
        self.parse_us + self.plan_us + self.execute_us + self.txn_begin_us + self.txn_commit_us
    }

    fn overhead_us(&self) -> u64 {
        self.wall_time_us.saturating_sub(self.traced_total())
    }

    fn phase_pct(&self, phase_us: u64) -> f64 {
        if self.wall_time_us == 0 {
            0.0
        } else {
            (phase_us as f64 / self.wall_time_us as f64) * 100.0
        }
    }

    fn dominant_phase(&self) -> &str {
        let phases = [
            (self.parse_us, "parse"),
            (self.plan_us, "plan"),
            (self.execute_us, "execute"),
            (self.txn_begin_us, "txn_begin"),
            (self.txn_commit_us, "txn_commit"),
            (self.overhead_us(), "overhead"),
        ];
        phases.iter().max_by_key(|(us, _)| *us).map_or("unknown", |(_, name)| name)
    }
}

fn collect_metrics(events: &Arc<Mutex<Vec<CapturedEvent>>>) -> PipelineMetrics {
    let captured = events.lock().unwrap();
    let mut metrics = PipelineMetrics {
        label: String::new(),
        wall_time_us: 0,
        parse_us: 0,
        plan_us: 0,
        execute_us: 0,
        txn_begin_us: 0,
        txn_commit_us: 0,
        operator_build_us: 0,
        operator_exec_us: 0,
        rows: 0,
    };

    for event in captured.iter() {
        match event.phase.as_str() {
            "parse" => metrics.parse_us += event.duration_us,
            "plan" => metrics.plan_us += event.duration_us,
            "execute" => metrics.execute_us += event.duration_us,
            "txn_begin" => metrics.txn_begin_us += event.duration_us,
            "txn_commit" => {
                metrics.txn_commit_us += event.duration_us;
                if let Some(rows) = event.rows {
                    metrics.rows = rows;
                }
            }
            "operator_build" => metrics.operator_build_us += event.duration_us,
            "operator_exec" => {
                metrics.operator_exec_us += event.duration_us;
                if let Some(rows) = event.rows {
                    metrics.rows = rows;
                }
            }
            _ => {}
        }
    }

    metrics
}

// ─── Benchmark Execution ────────────────────────────────────────────────────

fn benchmark_execute(
    db: &EmbeddedDatabase,
    events: &Arc<Mutex<Vec<CapturedEvent>>>,
    sql: &str,
    label: &str,
) -> PipelineMetrics {
    events.lock().unwrap().clear();

    let start = Instant::now();
    let result = db.execute(sql);
    let wall = start.elapsed();

    let mut metrics = collect_metrics(events);
    metrics.label = label.to_string();
    metrics.wall_time_us = wall.as_micros() as u64;

    if let Ok(count) = result {
        if metrics.rows == 0 {
            metrics.rows = count;
        }
    }

    metrics
}

fn benchmark_query(
    db: &EmbeddedDatabase,
    events: &Arc<Mutex<Vec<CapturedEvent>>>,
    sql: &str,
    label: &str,
) -> PipelineMetrics {
    events.lock().unwrap().clear();

    let start = Instant::now();
    let result = db.query(sql, &[]);
    let wall = start.elapsed();

    let mut metrics = collect_metrics(events);
    metrics.label = label.to_string();
    metrics.wall_time_us = wall.as_micros() as u64;

    if let Ok(ref rows) = result {
        if metrics.rows == 0 {
            metrics.rows = rows.len() as u64;
        }
    }

    metrics
}

fn benchmark_query_cached(
    db: &EmbeddedDatabase,
    events: &Arc<Mutex<Vec<CapturedEvent>>>,
    sql: &str,
    label: &str,
) -> PipelineMetrics {
    // Warm the plan cache with first execution
    let _ = db.query(sql, &[]);
    events.lock().unwrap().clear();

    // Measure the cached (2nd) execution
    let start = Instant::now();
    let result = db.query(sql, &[]);
    let wall = start.elapsed();

    let mut metrics = collect_metrics(events);
    metrics.label = format!("{} (cached)", label);
    metrics.wall_time_us = wall.as_micros() as u64;

    if let Ok(ref rows) = result {
        if metrics.rows == 0 {
            metrics.rows = rows.len() as u64;
        }
    }

    metrics
}

fn benchmark_execute_batch(
    db: &EmbeddedDatabase,
    events: &Arc<Mutex<Vec<CapturedEvent>>>,
    sqls: &[&str],
    label: &str,
) -> PipelineMetrics {
    events.lock().unwrap().clear();

    let start = Instant::now();
    let total_rows = db.execute_batch(sqls).unwrap_or(0);
    let wall = start.elapsed();

    let mut metrics = collect_metrics(events);
    metrics.label = label.to_string();
    metrics.wall_time_us = wall.as_micros() as u64;
    metrics.rows = total_rows;

    metrics
}

fn benchmark_execute_bulk(
    db: &EmbeddedDatabase,
    events: &Arc<Mutex<Vec<CapturedEvent>>>,
    sqls: &[String],
    label: &str,
) -> PipelineMetrics {
    events.lock().unwrap().clear();

    let start = Instant::now();
    let mut total_rows = 0u64;
    for sql in sqls {
        if let Ok(count) = db.execute(sql) {
            total_rows += count;
        }
    }
    let wall = start.elapsed();

    let mut metrics = collect_metrics(events);
    metrics.label = label.to_string();
    metrics.wall_time_us = wall.as_micros() as u64;
    metrics.rows = total_rows;

    metrics
}

// ─── Test Data Setup ────────────────────────────────────────────────────────

fn setup_test_data(db: &EmbeddedDatabase) {
    // Main benchmark table
    db.execute("CREATE TABLE bench_main (id INT PRIMARY KEY, name TEXT, age INT, score FLOAT, active BOOLEAN)")
        .expect("CREATE bench_main");

    // Second table for JOINs
    db.execute("CREATE TABLE bench_orders (id INT PRIMARY KEY, user_id INT, amount FLOAT, status TEXT)")
        .expect("CREATE bench_orders");

    // Insert 1000 rows into bench_main
    for i in 0..1000 {
        db.execute(&format!(
            "INSERT INTO bench_main VALUES ({}, 'user_{}', {}, {:.2}, {})",
            i,
            i,
            20 + (i % 50),
            (i as f64) * 1.5,
            if i % 3 == 0 { "true" } else { "false" }
        ))
        .expect("INSERT bench_main");
    }

    // Insert 500 orders
    for i in 0..500 {
        db.execute(&format!(
            "INSERT INTO bench_orders VALUES ({}, {}, {:.2}, '{}')",
            i,
            i % 200,
            10.0 + (i as f64) * 0.75,
            if i % 4 == 0 { "shipped" } else { "pending" }
        ))
        .expect("INSERT bench_orders");
    }

    // Table for DDL tests
    db.execute("CREATE TABLE bench_ddl_target (id INT PRIMARY KEY, value TEXT)")
        .expect("CREATE bench_ddl_target");
}

// ─── Benchmark Suite ────────────────────────────────────────────────────────

fn run_benchmark_suite(
    db: &EmbeddedDatabase,
    events: &Arc<Mutex<Vec<CapturedEvent>>>,
) -> Vec<PipelineMetrics> {
    let mut results = Vec::new();

    // ── DDL ──
    results.push(benchmark_execute(
        db, events,
        "CREATE TABLE bench_temp (id INT PRIMARY KEY, val TEXT, num INT)",
        "CREATE TABLE",
    ));

    results.push(benchmark_execute(
        db, events,
        "ALTER TABLE bench_temp ADD COLUMN extra TEXT",
        "ALTER TABLE ADD COL",
    ));

    results.push(benchmark_execute(
        db, events,
        "DROP TABLE bench_temp",
        "DROP TABLE",
    ));

    // ── INSERT ──
    results.push(benchmark_execute(
        db, events,
        "INSERT INTO bench_ddl_target VALUES (9999, 'single_insert')",
        "INSERT (single)",
    ));

    let bulk_inserts: Vec<String> = (10000..10100)
        .map(|i| format!("INSERT INTO bench_ddl_target VALUES ({}, 'bulk_{}')", i, i))
        .collect();
    results.push(benchmark_execute_bulk(
        db, events,
        &bulk_inserts,
        "INSERT (bulk 100)",
    ));

    // Batch insert (single transaction)
    let batch_strs: Vec<String> = (20000..20100)
        .map(|i| format!("INSERT INTO bench_ddl_target VALUES ({}, 'batch_{}')", i, i))
        .collect();
    let batch_refs: Vec<&str> = batch_strs.iter().map(|s| s.as_str()).collect();
    results.push(benchmark_execute_batch(
        db, events,
        &batch_refs,
        "INSERT (batch 100)",
    ));

    // ── UPDATE ──
    results.push(benchmark_execute(
        db, events,
        "UPDATE bench_main SET name = 'updated' WHERE id = 500",
        "UPDATE (single)",
    ));

    results.push(benchmark_execute(
        db, events,
        "UPDATE bench_main SET score = score + 1.0 WHERE age > 60",
        "UPDATE (bulk WHERE)",
    ));

    // ── DELETE ──
    results.push(benchmark_execute(
        db, events,
        "DELETE FROM bench_ddl_target WHERE id = 9999",
        "DELETE (single)",
    ));

    results.push(benchmark_execute(
        db, events,
        "DELETE FROM bench_ddl_target WHERE id >= 10050",
        "DELETE (bulk WHERE)",
    ));

    // ── SELECT (Simple) ──
    results.push(benchmark_query(
        db, events,
        "SELECT * FROM bench_main",
        "SELECT * (full scan)",
    ));

    results.push(benchmark_query(
        db, events,
        "SELECT * FROM bench_main WHERE age > 60",
        "SELECT WHERE",
    ));

    results.push(benchmark_query(
        db, events,
        "SELECT * FROM bench_main WHERE id = 42",
        "SELECT WHERE id=",
    ));

    results.push(benchmark_query(
        db, events,
        "SELECT * FROM bench_main LIMIT 10",
        "SELECT LIMIT 10",
    ));

    results.push(benchmark_query(
        db, events,
        "SELECT name, age FROM bench_main WHERE active = true",
        "SELECT projection+filter",
    ));

    // ── Aggregations ──
    results.push(benchmark_query(
        db, events,
        "SELECT COUNT(*) FROM bench_main",
        "COUNT(*)",
    ));

    results.push(benchmark_query(
        db, events,
        "SELECT AVG(score), SUM(age), MIN(id), MAX(id) FROM bench_main",
        "AVG/SUM/MIN/MAX",
    ));

    results.push(benchmark_query(
        db, events,
        "SELECT age, COUNT(*), AVG(score) FROM bench_main GROUP BY age",
        "GROUP BY",
    ));

    results.push(benchmark_query(
        db, events,
        "SELECT age, COUNT(*) as cnt FROM bench_main GROUP BY age HAVING COUNT(*) > 15",
        "GROUP BY + HAVING",
    ));

    // ── Sorting ──
    results.push(benchmark_query(
        db, events,
        "SELECT * FROM bench_main ORDER BY score DESC",
        "ORDER BY DESC",
    ));

    results.push(benchmark_query(
        db, events,
        "SELECT * FROM bench_main ORDER BY age, name",
        "ORDER BY (multi-col)",
    ));

    // ── JOINs ──
    results.push(benchmark_query(
        db, events,
        "SELECT m.name, o.amount FROM bench_main m INNER JOIN bench_orders o ON m.id = o.user_id WHERE m.id < 50",
        "INNER JOIN",
    ));

    results.push(benchmark_query(
        db, events,
        "SELECT m.name, o.amount FROM bench_main m LEFT JOIN bench_orders o ON m.id = o.user_id WHERE m.id < 50",
        "LEFT JOIN",
    ));

    // ── Advanced ──
    results.push(benchmark_query(
        db, events,
        "WITH active_users AS (SELECT * FROM bench_main WHERE active = true) SELECT COUNT(*) FROM active_users WHERE age > 30",
        "CTE",
    ));

    results.push(benchmark_query(
        db, events,
        "SELECT name, age, ROW_NUMBER() OVER (ORDER BY score DESC) as rank FROM bench_main WHERE id < 100",
        "Window (ROW_NUMBER)",
    ));

    results.push(benchmark_query(
        db, events,
        "SELECT name FROM bench_main WHERE id < 500 UNION ALL SELECT name FROM bench_main WHERE id >= 500",
        "UNION ALL",
    ));

    // ── Subqueries ──
    results.push(benchmark_query(
        db, events,
        "SELECT * FROM bench_main WHERE id IN (SELECT user_id FROM bench_orders WHERE amount > 100)",
        "IN (subquery)",
    ));

    // ── Cached Queries (plan cache warm) ──
    results.push(benchmark_query_cached(
        db, events,
        "SELECT * FROM bench_main WHERE age > 60",
        "SELECT WHERE",
    ));

    results.push(benchmark_query_cached(
        db, events,
        "SELECT age, COUNT(*), AVG(score) FROM bench_main GROUP BY age",
        "GROUP BY",
    ));

    results.push(benchmark_query_cached(
        db, events,
        "SELECT m.name, o.amount FROM bench_main m INNER JOIN bench_orders o ON m.id = o.user_id WHERE m.id < 50",
        "INNER JOIN",
    ));

    // ── Row Cache Hit (ART index + row cache) ──
    // First call populates the row cache, second measures cache hit
    let _ = db.query("SELECT * FROM bench_main WHERE id = 42", &[]);
    events.lock().unwrap().clear();

    let start = Instant::now();
    let result = db.query("SELECT * FROM bench_main WHERE id = 42", &[]);
    let wall = start.elapsed();

    let mut metrics = collect_metrics(events);
    metrics.label = "SELECT WHERE id= (row cached)".to_string();
    metrics.wall_time_us = wall.as_micros() as u64;
    if let Ok(ref rows) = result {
        if metrics.rows == 0 {
            metrics.rows = rows.len() as u64;
        }
    }
    results.push(metrics);

    results
}

// ─── Report Formatting ──────────────────────────────────────────────────────

fn format_us(us: u64) -> String {
    if us >= 1_000_000 {
        format!("{:.1}s", us as f64 / 1_000_000.0)
    } else if us >= 1_000 {
        format!("{:.1}ms", us as f64 / 1_000.0)
    } else {
        format!("{}us", us)
    }
}

fn print_report(mode_label: &str, metrics: &[PipelineMetrics]) {
    println!();
    println!("==========================================================================");
    println!("  {} Pipeline Performance", mode_label);
    println!("==========================================================================");
    println!(
        "{:<25} {:>10} {:>8} {:>8} {:>10} {:>8} {:>8} {:>6}",
        "Statement", "Wall", "Parse", "Plan", "Execute", "Commit", "Other", "Rows"
    );
    println!("{}", "-".repeat(86));

    for m in metrics {
        println!(
            "{:<25} {:>10} {:>8} {:>8} {:>10} {:>8} {:>8} {:>6}",
            m.label,
            format_us(m.wall_time_us),
            format_us(m.parse_us),
            format_us(m.plan_us),
            format_us(m.execute_us),
            format_us(m.txn_commit_us),
            format_us(m.overhead_us()),
            m.rows,
        );
    }

    println!("{}", "-".repeat(86));

    // Summary statistics
    let total_wall: u64 = metrics.iter().map(|m| m.wall_time_us).sum();
    let total_parse: u64 = metrics.iter().map(|m| m.parse_us).sum();
    let total_plan: u64 = metrics.iter().map(|m| m.plan_us).sum();
    let total_execute: u64 = metrics.iter().map(|m| m.execute_us).sum();
    let total_commit: u64 = metrics.iter().map(|m| m.txn_commit_us).sum();
    let total_overhead: u64 = metrics.iter().map(|m| m.overhead_us()).sum();

    println!(
        "{:<25} {:>10} {:>8} {:>8} {:>10} {:>8} {:>8}",
        "TOTAL",
        format_us(total_wall),
        format_us(total_parse),
        format_us(total_plan),
        format_us(total_execute),
        format_us(total_commit),
        format_us(total_overhead),
    );

    if total_wall > 0 {
        println!(
            "{:<25} {:>10} {:>7.1}% {:>7.1}% {:>9.1}% {:>7.1}% {:>7.1}%",
            "% of wall time",
            "100%",
            (total_parse as f64 / total_wall as f64) * 100.0,
            (total_plan as f64 / total_wall as f64) * 100.0,
            (total_execute as f64 / total_wall as f64) * 100.0,
            (total_commit as f64 / total_wall as f64) * 100.0,
            (total_overhead as f64 / total_wall as f64) * 100.0,
        );
    }
}

fn print_comparison(in_mem: &[PipelineMetrics], persistent: &[PipelineMetrics]) {
    println!();
    println!("==========================================================================");
    println!("  Comparison: Persistent (RocksDB) vs In-Memory");
    println!("==========================================================================");
    println!(
        "{:<25} {:>12} {:>12} {:>8} {:>14}",
        "Statement", "In-Mem", "Persistent", "Ratio", "Dominant Phase"
    );
    println!("{}", "-".repeat(75));

    for (im, ps) in in_mem.iter().zip(persistent.iter()) {
        let ratio = if im.wall_time_us > 0 {
            ps.wall_time_us as f64 / im.wall_time_us as f64
        } else {
            0.0
        };
        let ratio_str = if ratio > 1.0 {
            format!("{:.2}x slower", ratio)
        } else if ratio < 1.0 {
            format!("{:.2}x faster", 1.0 / ratio)
        } else {
            "same".to_string()
        };

        println!(
            "{:<25} {:>12} {:>12} {:>8} {:>14}",
            im.label,
            format_us(im.wall_time_us),
            format_us(ps.wall_time_us),
            ratio_str,
            ps.dominant_phase(),
        );
    }
}

fn print_analysis(in_mem: &[PipelineMetrics], persistent: &[PipelineMetrics]) {
    println!();
    println!("==========================================================================");
    println!("  Performance Analysis & Improvement Suggestions");
    println!("==========================================================================");
    println!();

    // ── Phase Distribution Analysis ──
    println!("--- Phase Distribution (In-Memory) ---");
    let total_wall: u64 = in_mem.iter().map(|m| m.wall_time_us).sum();
    let total_parse: u64 = in_mem.iter().map(|m| m.parse_us).sum();
    let total_plan: u64 = in_mem.iter().map(|m| m.plan_us).sum();
    let total_execute: u64 = in_mem.iter().map(|m| m.execute_us).sum();
    let total_commit: u64 = in_mem.iter().map(|m| m.txn_commit_us).sum();
    let total_overhead: u64 = in_mem.iter().map(|m| m.overhead_us()).sum();

    if total_wall > 0 {
        println!("  Parse:   {:>7} ({:.1}%)", format_us(total_parse), (total_parse as f64 / total_wall as f64) * 100.0);
        println!("  Plan:    {:>7} ({:.1}%)", format_us(total_plan), (total_plan as f64 / total_wall as f64) * 100.0);
        println!("  Execute: {:>7} ({:.1}%)", format_us(total_execute), (total_execute as f64 / total_wall as f64) * 100.0);
        println!("  Commit:  {:>7} ({:.1}%)", format_us(total_commit), (total_commit as f64 / total_wall as f64) * 100.0);
        println!("  Other:   {:>7} ({:.1}%)", format_us(total_overhead), (total_overhead as f64 / total_wall as f64) * 100.0);
    }

    println!();
    println!("--- Phase Distribution (Persistent) ---");
    let p_total_wall: u64 = persistent.iter().map(|m| m.wall_time_us).sum();
    let p_total_parse: u64 = persistent.iter().map(|m| m.parse_us).sum();
    let p_total_plan: u64 = persistent.iter().map(|m| m.plan_us).sum();
    let p_total_execute: u64 = persistent.iter().map(|m| m.execute_us).sum();
    let p_total_commit: u64 = persistent.iter().map(|m| m.txn_commit_us).sum();
    let p_total_overhead: u64 = persistent.iter().map(|m| m.overhead_us()).sum();

    if p_total_wall > 0 {
        println!("  Parse:   {:>7} ({:.1}%)", format_us(p_total_parse), (p_total_parse as f64 / p_total_wall as f64) * 100.0);
        println!("  Plan:    {:>7} ({:.1}%)", format_us(p_total_plan), (p_total_plan as f64 / p_total_wall as f64) * 100.0);
        println!("  Execute: {:>7} ({:.1}%)", format_us(p_total_execute), (p_total_execute as f64 / p_total_wall as f64) * 100.0);
        println!("  Commit:  {:>7} ({:.1}%)", format_us(p_total_commit), (p_total_commit as f64 / p_total_wall as f64) * 100.0);
        println!("  Other:   {:>7} ({:.1}%)", format_us(p_total_overhead), (p_total_overhead as f64 / p_total_wall as f64) * 100.0);
    }

    // ── Bottleneck Identification ──
    println!();
    println!("--- Bottleneck Identification ---");

    // Find statements where parse is expensive relative to total
    for m in in_mem {
        if m.wall_time_us > 0 && m.phase_pct(m.parse_us) > 30.0 {
            println!(
                "  PARSE-HEAVY: '{}' — parse takes {:.0}% of wall time ({}). Consider caching parsed ASTs for repeated queries.",
                m.label, m.phase_pct(m.parse_us), format_us(m.parse_us)
            );
        }
    }

    // Find statements where plan is expensive
    for m in in_mem {
        if m.wall_time_us > 0 && m.phase_pct(m.plan_us) > 30.0 {
            println!(
                "  PLAN-HEAVY: '{}' — planning takes {:.0}% of wall time ({}). Complex logical plan construction.",
                m.label, m.phase_pct(m.plan_us), format_us(m.plan_us)
            );
        }
    }

    // Find statements where execute dominates
    for m in in_mem {
        if m.wall_time_us > 0 && m.phase_pct(m.execute_us) > 70.0 {
            println!(
                "  EXEC-HEAVY: '{}' — execution takes {:.0}% of wall time ({}, {} rows).",
                m.label, m.phase_pct(m.execute_us), format_us(m.execute_us), m.rows
            );
        }
    }

    // Find statements where commit is expensive
    for m in persistent {
        if m.wall_time_us > 0 && m.phase_pct(m.txn_commit_us) > 30.0 {
            println!(
                "  COMMIT-HEAVY (persistent): '{}' — commit takes {:.0}% of wall time ({}).",
                m.label, m.phase_pct(m.txn_commit_us), format_us(m.txn_commit_us)
            );
        }
    }

    // Find big persistent vs in-memory gaps
    for (im, ps) in in_mem.iter().zip(persistent.iter()) {
        if im.wall_time_us > 0 {
            let ratio = ps.wall_time_us as f64 / im.wall_time_us as f64;
            if ratio > 3.0 {
                println!(
                    "  DISK-PENALTY: '{}' is {:.1}x slower on persistent storage ({} vs {}). Check RocksDB write amplification.",
                    im.label, ratio, format_us(ps.wall_time_us), format_us(im.wall_time_us)
                );
            }
        }
    }

    // Find statements with high overhead (untraced time)
    for m in in_mem {
        if m.wall_time_us > 100 && m.phase_pct(m.overhead_us()) > 50.0 {
            println!(
                "  HIGH-OVERHEAD: '{}' — {:.0}% of time ({}) not attributed to any traced phase. Consider adding tracing to the DML dispatch path.",
                m.label, m.phase_pct(m.overhead_us()), format_us(m.overhead_us())
            );
        }
    }

    // ── Improvement Suggestions ──
    println!();
    println!("--- Improvement Suggestions ---");

    // Parse phase analysis
    let avg_parse = if in_mem.is_empty() { 0 } else { total_parse / in_mem.len() as u64 };
    println!("  1. PARSING: Average parse time = {}. {}", format_us(avg_parse),
        if avg_parse < 100 {
            "SQL parsing is efficient. No action needed."
        } else if avg_parse < 500 {
            "Moderate parse cost. Consider prepared statements for repeated queries."
        } else {
            "High parse cost. Strongly recommend prepared statement caching."
        }
    );

    // Plan phase analysis
    let avg_plan = if in_mem.is_empty() { 0 } else { total_plan / in_mem.len() as u64 };
    println!("  2. PLANNING: Average plan time = {}. {}", format_us(avg_plan),
        if avg_plan < 100 {
            "Logical planning is fast. No action needed."
        } else if avg_plan < 500 {
            "Moderate plan cost. Consider plan caching for identical queries."
        } else {
            "High plan cost. Implement plan cache with invalidation on DDL."
        }
    );

    // Execute phase analysis
    if total_execute > 0 {
        let exec_pct = (total_execute as f64 / total_wall as f64) * 100.0;
        println!("  3. EXECUTION: {:.0}% of total time. {}", exec_pct,
            if exec_pct > 70.0 {
                "Execution dominates — optimize hot-path operators (scans, joins)."
            } else if exec_pct > 40.0 {
                "Balanced execution cost. Consider index-based scans for filtered queries."
            } else {
                "Execution is not the bottleneck."
            }
        );
    }

    // Commit phase analysis (persistent)
    if p_total_commit > 0 && p_total_wall > 0 {
        let commit_pct = (p_total_commit as f64 / p_total_wall as f64) * 100.0;
        println!("  4. COMMIT (persistent): {:.0}% of total time. {}", commit_pct,
            if commit_pct > 30.0 {
                "Heavy commit overhead. Consider group commit or WAL batching for bulk operations."
            } else if commit_pct > 15.0 {
                "Moderate commit cost. RocksDB fsync is the likely bottleneck."
            } else {
                "Commit overhead is acceptable."
            }
        );
    }

    // Overhead analysis
    if total_overhead > 0 && total_wall > 0 {
        let overhead_pct = (total_overhead as f64 / total_wall as f64) * 100.0;
        println!("  5. UNTRACED OVERHEAD: {:.0}% of total time. {}", overhead_pct,
            if overhead_pct > 40.0 {
                "Large untraced gap — DML dispatch (INSERT/UPDATE/DELETE) path lacks tracing. Add phase instrumentation to execute_in_transaction()."
            } else if overhead_pct > 20.0 {
                "Moderate untraced time — likely in transaction management, RLS checks, or trigger evaluation."
            } else {
                "Tracing coverage is good — most time is attributed to known phases."
            }
        );
    }

    // Persistent vs in-memory summary
    if total_wall > 0 && p_total_wall > 0 {
        let overall_ratio = p_total_wall as f64 / total_wall as f64;
        println!("  6. STORAGE OVERHEAD: Persistent mode is {:.1}x slower overall. {}", overall_ratio,
            if overall_ratio > 5.0 {
                "Very high disk penalty. Consider memory-mapped mode or SSD optimization."
            } else if overall_ratio > 2.0 {
                "Expected disk overhead. Use in-memory mode where durability isn't critical."
            } else {
                "Minimal disk penalty — RocksDB caching is effective."
            }
        );
    }

    // Operator-level insights
    let total_op_build: u64 = in_mem.iter().map(|m| m.operator_build_us).sum();
    let total_op_exec: u64 = in_mem.iter().map(|m| m.operator_exec_us).sum();
    if total_op_build + total_op_exec > 0 {
        println!("  7. OPERATOR BREAKDOWN: Build={}, Exec={}. {}",
            format_us(total_op_build), format_us(total_op_exec),
            if total_op_build > total_op_exec {
                "Operator tree construction is slow — consider caching physical plans."
            } else {
                "Operator execution dominates (expected) — optimize scan and join operators."
            }
        );
    }

    // Throughput summary
    println!();
    println!("--- Throughput Estimates ---");
    for m in in_mem {
        if m.wall_time_us > 0 {
            let ops_per_sec = 1_000_000.0 / m.wall_time_us as f64;
            if ops_per_sec < 10_000.0 {
                println!("  {:<25} {:>8.0} ops/sec", m.label, ops_per_sec);
            } else {
                println!("  {:<25} {:>8.0} ops/sec", m.label, ops_per_sec);
            }
        }
    }

    println!();
    println!("==========================================================================");
}

// ─── Main Test ──────────────────────────────────────────────────────────────

#[test]
fn test_pipeline_performance_report() {
    let events = setup_tracing();

    // ── In-Memory Mode ──
    println!("\n\nSetting up in-memory database...");
    let db_mem = EmbeddedDatabase::new_in_memory().expect("Failed to create in-memory DB");
    setup_test_data(&db_mem);
    events.lock().unwrap().clear(); // Clear setup events

    println!("Running benchmark suite (in-memory)...");
    let in_mem_results = run_benchmark_suite(&db_mem, &events);
    drop(db_mem);

    // ── Persistent Mode ──
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    println!("Setting up persistent database at {:?}...", temp_dir.path());
    let db_disk = EmbeddedDatabase::new(temp_dir.path()).expect("Failed to create persistent DB");
    setup_test_data(&db_disk);
    events.lock().unwrap().clear(); // Clear setup events

    println!("Running benchmark suite (persistent)...");
    let persistent_results = run_benchmark_suite(&db_disk, &events);
    drop(db_disk);

    // ── Reports ──
    print_report("In-Memory", &in_mem_results);
    print_report("Persistent (RocksDB)", &persistent_results);
    print_comparison(&in_mem_results, &persistent_results);
    print_analysis(&in_mem_results, &persistent_results);

    // ── Assertions ──
    // Verify we actually captured tracing events
    let traced_count = in_mem_results.iter().filter(|m| m.parse_us > 0 || m.execute_us > 0).count();
    assert!(
        traced_count > 0,
        "Expected at least some statements to have traced phases, got 0"
    );

    // Verify all benchmarks completed
    assert_eq!(in_mem_results.len(), persistent_results.len());
    assert!(in_mem_results.len() >= 20, "Expected at least 20 benchmark results");
}
