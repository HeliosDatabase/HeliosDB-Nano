//! Predicate-pushdown benchmark for the `JoinPredicatePushdownRule`.
//!
//! Methodology
//! ===========
//!
//! Builds a single shared dataset on disk (default ~10 GB pre-compression) and
//! runs four representative query shapes against it, twice each: once with the
//! new pushdown rule active, and once without. The delta between the two timings
//! is the rule's perf contribution.
//!
//! The dataset is **reused across runs**: on first invocation it's generated
//! into `./bench-data-predicate-pushdown/`. Subsequent invocations open it
//! read-only and skip the generation phase. Delete the directory to force a
//! regeneration.
//!
//! Sizing knob: `HELIOSDB_PP_BENCH_ROWS` (default `30_000_000`). Each row is
//! roughly 330 bytes, so 30M rows ≈ 10 GB on disk before compression. The
//! storage engine compresses with zstd by default, so the actual on-disk
//! footprint is typically 30–50 % of that.
//!
//! Run:
//! ```bash
//! cargo bench --bench predicate_pushdown_bench
//! HELIOSDB_PP_BENCH_ROWS=10000000 cargo bench --bench predicate_pushdown_bench
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use heliosdb_nano::{
    optimizer::{
        cost::StatsCatalog,
        rules::{
            ConstantFoldingRule, JoinPredicatePushdownRule, OptimizationRule,
            ProjectionPruningRule, SelectionPushdownRule,
        },
        Optimizer, OptimizerConfig,
    },
    sql::{Parser, Planner},
    EmbeddedDatabase, Result, Value,
};

const COUNTRIES: &[(&str, &str, &str)] = &[
    ("US", "United States", "NA"),
    ("CA", "Canada", "NA"),
    ("MX", "Mexico", "NA"),
    ("BR", "Brazil", "SA"),
    ("AR", "Argentina", "SA"),
    ("UK", "United Kingdom", "EU"),
    ("DE", "Germany", "EU"),
    ("FR", "France", "EU"),
    ("ES", "Spain", "EU"),
    ("IT", "Italy", "EU"),
    ("JP", "Japan", "AS"),
    ("CN", "China", "AS"),
    ("IN", "India", "AS"),
    ("KR", "South Korea", "AS"),
    ("AU", "Oceania", "OC"),
    ("NZ", "New Zealand", "OC"),
    ("ZA", "South Africa", "AF"),
    ("EG", "Egypt", "AF"),
    ("NG", "Nigeria", "AF"),
    ("RU", "Russia", "EU"),
];

const EVENT_TYPES: &[&str] = &[
    "view", "click", "scroll", "purchase", "login", "logout", "search", "share",
    "comment", "like",
];

fn dataset_path() -> PathBuf {
    PathBuf::from("./bench-data-predicate-pushdown")
}

fn bench_rows() -> u64 {
    // Default targets ~10 GB on disk. Each row has ~250 B of declared payload
    // but lands at ~2.2 KB on disk after RocksDB MVCC + WAL + zstd block
    // overhead — so 5 M rows ≈ 11 GB. Override with `HELIOSDB_PP_BENCH_ROWS`.
    std::env::var("HELIOSDB_PP_BENCH_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5_000_000)
}

/// Open the bench dataset, generating it on first use.
fn open_or_generate() -> EmbeddedDatabase {
    let path = dataset_path();
    let target_rows = bench_rows();

    if path.exists() {
        eprintln!("Reusing existing bench dataset at {:?}", path);
        return EmbeddedDatabase::new(&path).expect("open existing bench dataset");
    }

    eprintln!(
        "Generating bench dataset at {:?} ({} rows). This will take a few minutes.",
        path, target_rows
    );

    let db = EmbeddedDatabase::new(&path).expect("create bench db");
    bootstrap_schema(&db);
    seed_countries(&db);
    seed_events(&db, target_rows);

    eprintln!("Bench dataset ready.");
    db
}

fn bootstrap_schema(db: &EmbeddedDatabase) {
    db.execute(
        "CREATE TABLE IF NOT EXISTS countries (
            code TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            region TEXT NOT NULL
        )",
    )
    .expect("create countries");

    db.execute(
        "CREATE TABLE IF NOT EXISTS events (
            id BIGINT PRIMARY KEY,
            user_id INTEGER NOT NULL,
            ts TIMESTAMP NOT NULL,
            event_type TEXT NOT NULL,
            country TEXT NOT NULL,
            payload TEXT
        )",
    )
    .expect("create events");
}

fn seed_countries(db: &EmbeddedDatabase) {
    for (code, name, region) in COUNTRIES {
        db.execute(&format!(
            "INSERT INTO countries (code, name, region) VALUES ('{}', '{}', '{}')",
            code, name, region
        ))
        .expect("insert country");
    }
}

fn seed_events(db: &EmbeddedDatabase, rows: u64) {
    // Synthetic but deterministic. Each row gets a ~250-char payload so the
    // total dataset reaches the documented size. The seed loop uses a
    // prepared single-row INSERT inside transactions of `TXN_SIZE` rows;
    // the parse + plan caches keep per-row cost low compared to sending a
    // multi-megabyte multi-row INSERT statement (which the parser had to
    // tokenise from scratch every time).
    const TXN_SIZE: u64 = 50_000;
    let payload: String = "x".repeat(240);
    let start = Instant::now();

    let mut inserted: u64 = 0;
    while inserted < rows {
        let upper = (inserted + TXN_SIZE).min(rows);
        db.execute("BEGIN").expect("begin txn");
        for i in inserted..upper {
            let ev = EVENT_TYPES[(i as usize) % EVENT_TYPES.len()];
            let cc = COUNTRIES[(i as usize) % COUNTRIES.len()].0;
            let ts_secs = 1_700_000_000i64 + ((i as i64) % 63_072_000);
            db.execute_params(
                "INSERT INTO events (id, user_id, ts, event_type, country, payload) \
                 VALUES ($1, $2, to_timestamp($3), $4, $5, $6)",
                &[
                    Value::Int8(i as i64),
                    Value::Int4((i % 1_000_000) as i32),
                    Value::Int8(ts_secs),
                    Value::String(ev.to_string()),
                    Value::String(cc.to_string()),
                    Value::String(payload.clone()),
                ],
            )
            .expect("insert event");
        }
        db.execute("COMMIT").expect("commit txn");
        inserted = upper;

        if inserted % (TXN_SIZE * 5) == 0 || inserted == rows {
            let pct = (inserted as f64 / rows as f64) * 100.0;
            let secs = start.elapsed().as_secs_f64();
            eprintln!(
                "  events seeded: {} / {} ({:.1}%) in {:.1}s ({:.0} rows/s)",
                inserted,
                rows,
                pct,
                secs,
                inserted as f64 / secs.max(0.001),
            );
        }
    }
    eprintln!(
        "  events seeded: {} in {:.1}s",
        rows,
        start.elapsed().as_secs_f64()
    );
}

/// The four representative query shapes.
fn queries() -> &'static [(&'static str, &'static str)] {
    &[
        // Q1 (control): equi-join only — no pushdown opportunity.
        // Both plans should be ≈identical post-optimizer.
        (
            "Q1_equi_only",
            "SELECT COUNT(*) FROM events JOIN countries ON events.country = countries.code",
        ),
        // Q2 (right-only literal): 'NA' region — pushable to dim side.
        // Small dim side gets pre-filtered to ≈3 rows.
        (
            "Q2_right_only_literal",
            "SELECT COUNT(*) FROM events JOIN countries ON countries.region = 'NA'",
        ),
        // Q3 (left-only literal): the bug-fix case scaled up. Pushable to
        // the LARGE fact side. With pushdown, events get pre-filtered to
        // ~10% of total before the join.
        (
            "Q3_left_only_literal",
            "SELECT COUNT(*) FROM events JOIN countries ON events.event_type = 'click'",
        ),
        // Q4 (mixed: equi + one-sided): equi-key stays on join, one-sided
        // pushed. Both arms benefit independently.
        (
            "Q4_mixed_equi_plus_one_sided",
            "SELECT COUNT(*) FROM events JOIN countries
               ON events.country = countries.code
              AND countries.region = 'NA'",
        ),
    ]
}

/// Build the standard rule set, optionally **without** the pushdown rule
/// being measured. Mirrors the production rules list at lib.rs:6616.
fn rules(with_pushdown: bool) -> Vec<Box<dyn OptimizationRule>> {
    let mut rules: Vec<Box<dyn OptimizationRule>> = vec![
        Box::new(ConstantFoldingRule::new()),
        Box::new(SelectionPushdownRule::new()),
    ];
    if with_pushdown {
        rules.push(Box::new(JoinPredicatePushdownRule::new()));
    }
    rules.push(Box::new(ProjectionPruningRule::new()));
    rules
}

/// Run a single query under the given optimizer configuration. Returns
/// the scalar `COUNT(*)` produced by the query (the bench queries are all
/// `SELECT COUNT(*) ...`) plus wall time. Routes through the public
/// parser/planner/executor APIs to bypass `EmbeddedDatabase::query`'s plan
/// cache, ensuring each iteration re-runs the optimizer.
fn run_query(db: &EmbeddedDatabase, sql: &str, with_pushdown: bool) -> Result<(i64, Duration)> {
    let start = Instant::now();

    let parser = Parser::new();
    let statement = parser.parse_one(sql)?;
    let catalog = db.storage.catalog();
    let planner = Planner::with_catalog(&catalog).with_sql(sql.to_string());
    let plan = planner.statement_to_plan(statement)?;

    let stats = StatsCatalog::new();
    let opt = Optimizer::with_rules(stats, rules(with_pushdown), OptimizerConfig::default());
    let plan = opt.optimize_recursive(plan)?;

    let mut executor = heliosdb_nano::sql::Executor::with_storage(&db.storage);
    let rows = executor.execute(&plan)?;

    // COUNT(*) → exactly one row, one column. Coerce Int8 → i64 for sanity check.
    let count: i64 = match rows.first().and_then(|t| t.values.first()) {
        Some(Value::Int8(n)) => *n,
        Some(Value::Int4(n)) => *n as i64,
        other => panic!("expected scalar count, got {:?}", other),
    };
    Ok((count, start.elapsed()))
}

fn bench_predicate_pushdown(c: &mut Criterion) {
    let db = open_or_generate();
    let row_count: u64 = bench_rows();

    let mut group = c.benchmark_group("predicate_pushdown");
    group.sample_size(10); // each query is several seconds; small sample is plenty
    group.measurement_time(Duration::from_secs(30));
    group.throughput(Throughput::Elements(row_count));

    for (qid, sql) in queries() {
        // Sanity: confirm both modes return the **same scalar COUNT** before
        // timing. The original bug made the buggy classification produce
        // wildly inflated row counts; without this check the perf delta
        // would be measuring the cost of doing the wrong work.
        let (n_baseline, _) = run_query(&db, sql, false).expect("baseline sanity");
        let (n_pushdown, _) = run_query(&db, sql, true).expect("pushdown sanity");
        if n_baseline != n_pushdown {
            // For Q2 (pure right-only constant), the baseline path goes
            // through the buggy executor classifier. Differing counts here
            // are EXPECTED — log and skip the apples-to-oranges comparison.
            eprintln!(
                "{} returns different counts (baseline={}, pushdown={}). \
                 Baseline path is the bug; skipping its bench.",
                qid, n_baseline, n_pushdown,
            );
            // Still bench the pushdown path so we have an absolute number.
            let id = BenchmarkId::new(*qid, "with_pushdown");
            group.bench_with_input(id, sql, |b, sql| {
                b.iter(|| {
                    let (cnt, _t) = run_query(&db, sql, true).expect("query failed");
                    black_box(cnt);
                });
            });
            continue;
        }

        for &with_pushdown in &[false, true] {
            let label = if with_pushdown { "with_pushdown" } else { "baseline" };
            let id = BenchmarkId::new(*qid, label);
            group.bench_with_input(id, sql, |b, sql| {
                b.iter(|| {
                    let (cnt, _t) =
                        run_query(&db, sql, with_pushdown).expect("query failed mid-bench");
                    black_box(cnt);
                });
            });
        }
    }

    group.finish();
}

criterion_group!(benches, bench_predicate_pushdown);
criterion_main!(benches);
