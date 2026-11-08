//! Strict HeliosDB-Nano vs native PostgreSQL 18.4 performance comparison
//!
//! Runs 35 SQL categories against both engines with identical schema/data.
//!
//! Prerequisites:
//!   docker run -d --name pg_bench_nano -e POSTGRES_USER=bench -e POSTGRES_PASSWORD=benchpass \
//!     -e POSTGRES_DB=benchdb -p 25433:5432 postgres:18.4-bookworm
//!
//! Run with:
//!   cargo test --release --test pg35_benchmark -- --nocapture --ignored

// Legacy import included Value even though the benchmark does not use it:
// use heliosdb_nano::{EmbeddedDatabase, Value};
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::EmbeddedDatabase;
#[path = "pg35_support/mod.rs"]
mod strict;
use std::time::{Duration, Instant};

// --- Result types ---

struct CategoryResult {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    iterations: usize,
    #[allow(dead_code)]
    wall_time: Duration,
    avg_per_iter: Duration,
    successes: usize,
    warmup_successes: usize,
}

struct ComparisonRow {
    name: String,
    helios_avg_us: f64,
    pg_avg_us: f64,
    ratio: f64,
    #[allow(dead_code)]
    winner: String,
    helios_na: bool,
    proof: serde_json::Value,
}

// --- PostgreSQL client (synchronous wrapper around tokio-postgres) ---

struct PgClient {
    rt: tokio::runtime::Runtime,
    client: tokio_postgres::Client,
    _handle: tokio::task::JoinHandle<()>,
}

impl PgClient {
    fn connect(connstr: &str) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let (client, connection) = rt.block_on(tokio_postgres::connect(connstr, tokio_postgres::NoTls))?;
        let handle = rt.spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("PG connection error: {}", e);
            }
        });
        Ok(PgClient {
            rt,
            client,
            _handle: handle,
        })
    }

    fn execute(&self, sql: &str) -> std::result::Result<u64, Box<dyn std::error::Error>> {
        Ok(self.rt.block_on(self.client.execute(sql, &[]))?)
    }

    fn query_count(&self, sql: &str) -> std::result::Result<usize, Box<dyn std::error::Error>> {
        let rows = self.rt.block_on(self.client.query(sql, &[]))?;
        Ok(rows.len())
    }
}

// --- Benchmark runner ---

fn expected_affected_rows(category: &str, sql: &str) -> Option<u64> {
    match category {
        "INSERT single" | "UPDATE point" | "DELETE point" | "UPSERT" | "UPDATE+subquery" => Some(1),
        "INSERT multi-row" | "INSERT..SELECT" => Some(10),
        "TRUNCATE" | "Transaction ctl" if sql.trim_start().starts_with("INSERT ") => Some(1),
        _ => None,
    }
}

struct TimedNano<'a> {
    db: &'a EmbeddedDatabase,
    category: &'a str,
}
impl std::ops::Deref for TimedNano<'_> {
    type Target = EmbeddedDatabase;
    fn deref(&self) -> &Self::Target {
        self.db
    }
}
impl TimedNano<'_> {
    fn execute(&self, sql: &str) -> heliosdb_nano::Result<u64> {
        self.db.execute(sql).map(|rows| {
            if let Some(expected) = expected_affected_rows(self.category, sql) {
                assert_eq!(rows, expected, "Nano {} affected rows: {sql}", self.category);
            }
            rows
        })
    }
}
struct TimedPg<'a> {
    pg: &'a PgClient,
    category: &'a str,
}
impl std::ops::Deref for TimedPg<'_> {
    type Target = PgClient;
    fn deref(&self) -> &Self::Target {
        self.pg
    }
}
impl TimedPg<'_> {
    fn execute(&self, sql: &str) -> std::result::Result<u64, Box<dyn std::error::Error>> {
        self.pg.execute(sql).map(|rows| {
            if let Some(expected) = expected_affected_rows(self.category, sql) {
                assert_eq!(rows, expected, "PG {} affected rows: {sql}", self.category);
            }
            rows
        })
    }
}

fn bench_helios<F>(db: &EmbeddedDatabase, name: &str, iters: usize, f: F) -> CategoryResult
where
    F: Fn(&TimedNano<'_>, usize),
{
    let db = &TimedNano { db, category: name };
    let mut warmup_successes = 0;
    for i in iters..iters + 2 {
        f(db, i);
        warmup_successes += 1;
    }
    let mut successes = 0;
    let wall_start = Instant::now();
    for i in 0..iters {
        f(db, i);
        successes += 1;
    }
    let wall_time = wall_start.elapsed();
    CategoryResult {
        name: name.into(),
        iterations: iters,
        wall_time,
        avg_per_iter: wall_time / u32::try_from(iters).expect("iteration count fits u32"),
        successes,
        warmup_successes,
    }
}

fn bench_helios_safe<F>(db: &EmbeddedDatabase, name: &str, iters: usize, f: F) -> CategoryResult
where
    F: Fn(&TimedNano<'_>, usize) -> std::result::Result<(), String>,
{
    let db = &TimedNano { db, category: name };
    let mut warmup_successes = 0;
    for i in iters..iters + 2 {
        f(db, i).unwrap_or_else(|e| panic!("Nano {name} warmup {i} failed: {e}"));
        warmup_successes += 1;
    }
    let mut successes = 0;
    let wall_start = Instant::now();
    for i in 0..iters {
        f(db, i).unwrap_or_else(|e| panic!("Nano {name} timed iteration {i} failed: {e}"));
        successes += 1;
    }
    let wall_time = wall_start.elapsed();
    CategoryResult {
        name: name.into(),
        iterations: iters,
        wall_time,
        avg_per_iter: wall_time / u32::try_from(iters).expect("iteration count fits u32"),
        successes,
        warmup_successes,
    }
}

fn bench_pg<F>(pg: &PgClient, name: &str, iters: usize, f: F) -> CategoryResult
where
    F: Fn(&TimedPg<'_>, usize),
{
    let pg = &TimedPg { pg, category: name };
    let mut warmup_successes = 0;
    for i in iters..iters + 2 {
        f(pg, i);
        warmup_successes += 1;
    }
    let mut successes = 0;
    let wall_start = Instant::now();
    for i in 0..iters {
        f(pg, i);
        successes += 1;
    }
    let wall_time = wall_start.elapsed();
    CategoryResult {
        name: name.into(),
        iterations: iters,
        wall_time,
        avg_per_iter: wall_time / u32::try_from(iters).expect("iteration count fits u32"),
        successes,
        warmup_successes,
    }
}

fn format_us(us: f64) -> String {
    if us >= 1_000_000.0 {
        format!("{:.1}s", us / 1_000_000.0)
    } else if us >= 1_000.0 {
        format!("{:.2}ms", us / 1_000.0)
    // Legacy output floored all sub-millisecond values to whole microseconds:
    // } else {
    //     format!("{:.0}us", us)
    } else if us >= 100.0 {
        format!("{:.0}us", us)
    } else if us >= 10.0 {
        format!("{:.1}us", us)
    } else {
        format!("{:.2}us", us)
    }
}

fn pg35_label() -> String {
    std::env::var("PG35_PG_LABEL").unwrap_or_else(|_| "POSTGRESQL 18.4".to_string())
}

fn pg35_connstr() -> String {
    std::env::var("PG35_CONNSTR").unwrap_or_else(|_| {
        // Legacy hard-coded DSN:
        // "host=127.0.0.1 port=25433 user=bench password=benchpass dbname=benchdb"
        "host=127.0.0.1 port=25433 user=bench password=benchpass dbname=benchdb".to_string()
    })
}

fn pg35_iters() -> usize {
    let iters = std::env::var("PG35_ITERS")
        .unwrap_or_else(|_| "300".into())
        .parse::<usize>()
        .expect("PG35_ITERS must be an integer");
    assert_eq!(iters, 300, "strict acceptance requires exactly300timed iterations");
    iters
}

// --- Schema setup ---

fn schema_ddl() -> Vec<String> {
    vec![
        "CREATE TABLE customers (id INT PRIMARY KEY, name TEXT, email TEXT, age INT, region TEXT, metadata TEXT, bio TEXT)".into(),
        "CREATE TABLE products (product_id INT PRIMARY KEY, name TEXT, category TEXT, price INT, description TEXT)".into(),
        "CREATE TABLE orders (order_id INT PRIMARY KEY, customer_id INT, order_date TEXT, status TEXT, total INT)".into(),
        "CREATE TABLE order_items (item_id INT PRIMARY KEY, order_id INT, product_id INT, quantity INT, unit_price INT)".into(),
        "CREATE TABLE categories (cat_id INT PRIMARY KEY, name TEXT, parent_id INT)".into(),
        "CREATE INDEX idx_cust_age ON customers(age)".into(),
        "CREATE INDEX idx_ord_cust ON orders(customer_id)".into(),
        "CREATE INDEX idx_oi_ord ON order_items(order_id)".into(),
    ]
}

fn schema_ddl_pg() -> Vec<String> {
    // Apply the same constraint/index workload to every Nano control and PG.
    schema_ddl()
}

fn setup_helios_schema(db: &EmbeddedDatabase) {
    let stmts = schema_ddl();
    for sql in &stmts {
        db.execute(sql)
            .unwrap_or_else(|e| panic!("Helios DDL failed: {} -- {}", sql, e));
    }
}

fn setup_pg_schema(pg: &PgClient) {
    let drops = [
        "DROP TABLE IF EXISTS order_items CASCADE",
        "DROP TABLE IF EXISTS orders CASCADE",
        "DROP TABLE IF EXISTS products CASCADE",
        "DROP TABLE IF EXISTS customers CASCADE",
        "DROP TABLE IF EXISTS categories CASCADE",
    ];
    for sql in &drops {
        pg.execute(sql).expect("PG35 constituent SQL must succeed");
    }
    // The same native reference is reused by ABBA arms. Clear only objects
    // owned by this harness so a prior partial run cannot skip timed CREATEs
    // through IF NOT EXISTS or seed a helper with extra rows/columns.
    for name in [
        "bench_alt",
        "bench_trunc",
        "bench_ins",
        "bench_ins_m",
        "bench_ins_s",
        "bench_del",
        "bench_ups",
        "strict_probe_ct",
    ] {
        pg.execute(&format!("DROP TABLE IF EXISTS {name} CASCADE"))
            .expect("reset owned PG35 helper table");
    }
    pg.execute("DROP VIEW IF EXISTS strict_probe_view")
        .expect("reset owned PG35 probe view");
    for i in 0..302 {
        for prefix in ["bench_ct_", "bench_drop_"] {
            pg.execute(&format!("DROP TABLE IF EXISTS {prefix}{i} CASCADE"))
                .expect("reset owned PG35 DDL helper");
        }
        pg.execute(&format!("DROP VIEW IF EXISTS bench_v_{i}"))
            .expect("reset owned PG35 DDL view");
    }
    let stmts = schema_ddl_pg();
    for sql in &stmts {
        pg.execute(sql)
            .unwrap_or_else(|e| panic!("PG DDL failed: {} -- {}", sql, e));
    }
}

// --- Data population ---

fn populate_helios(db: &EmbeddedDatabase) {
    let regions = ["East", "West", "North", "South", "Central"];
    let statuses = ["pending", "shipped", "delivered", "cancelled"];

    // Customers: 200 rows
    for i in 1..=200 {
        let region = regions[i % regions.len()];
        let age = 18 + (i % 62);
        db.execute(&format!(
            "INSERT INTO customers VALUES ({}, 'Customer_{}', 'c{}@test.com', {}, '{}', '{{\"tier\":\"{}\"}}', 'Bio for customer {}')",
            i, i, i, age, region, if i % 3 == 0 { "gold" } else { "silver" }, i
        )).expect("PG35 constituent SQL must succeed");
    }

    // Products: 50 rows
    let cats = ["Electronics", "Books", "Clothing", "Food", "Sports"];
    for i in 1..=50 {
        let cat = cats[i % cats.len()];
        db.execute(&format!(
            "INSERT INTO products VALUES ({}, 'Product_{}', '{}', {}, 'Description {}')",
            i,
            i,
            cat,
            10 + (i % 990),
            i
        ))
        .expect("PG35 constituent SQL must succeed");
    }

    // Orders: 500 rows
    for i in 1..=500 {
        let cust_id = (i % 200) + 1;
        let status = statuses[i % statuses.len()];
        db.execute(&format!(
            "INSERT INTO orders VALUES ({}, {}, '2024-{:02}-{:02}', '{}', {})",
            i,
            cust_id,
            (i % 12) + 1,
            (i % 28) + 1,
            status,
            50 + (i % 950)
        ))
        .expect("PG35 constituent SQL must succeed");
    }

    // Order items: 1000 rows
    for i in 1..=1000 {
        let ord_id = (i % 500) + 1;
        let prod_id = (i % 50) + 1;
        db.execute(&format!(
            "INSERT INTO order_items VALUES ({}, {}, {}, {}, {})",
            i,
            ord_id,
            prod_id,
            1 + (i % 10),
            5 + (i % 100)
        ))
        .expect("PG35 constituent SQL must succeed");
    }

    // Categories: 20 rows
    let cat_names = [
        "Electronics",
        "Books",
        "Clothing",
        "Food",
        "Sports",
        "Phones",
        "Laptops",
        "Tablets",
        "Fiction",
        "NonFiction",
        "Shirts",
        "Pants",
        "Shoes",
        "Snacks",
        "Drinks",
        "Soccer",
        "Tennis",
        "Swimming",
        "Camping",
        "Cycling",
    ];
    for (i, name) in cat_names.iter().enumerate() {
        let cat_id = (i + 1) as i32;
        let parent = if i < 5 {
            "NULL".to_string()
        } else {
            format!("{}", (i % 5) + 1)
        };
        db.execute(&format!(
            "INSERT INTO categories VALUES ({}, '{}', {})",
            cat_id, name, parent
        ))
        .expect("PG35 constituent SQL must succeed");
    }
}

fn populate_pg(pg: &PgClient) {
    let regions = ["East", "West", "North", "South", "Central"];
    let statuses = ["pending", "shipped", "delivered", "cancelled"];

    for batch in 0..2 {
        let mut values = Vec::new();
        for j in 0..100 {
            let i = batch * 100 + j + 1;
            let region = regions[i % regions.len()];
            let age = 18 + (i % 62);
            let tier = if i % 3 == 0 { "gold" } else { "silver" };
            values.push(format!(
                "({}, 'Customer_{}', 'c{}@test.com', {}, '{}', '{{\"tier\":\"{}\"}}', 'Bio for customer {}')",
                i, i, i, age, region, tier, i
            ));
        }
        pg.execute(&format!("INSERT INTO customers VALUES {}", values.join(",")))
            .expect("PG35 constituent SQL must succeed");
    }

    let cats = ["Electronics", "Books", "Clothing", "Food", "Sports"];
    {
        let mut values = Vec::new();
        for i in 1..=50 {
            let cat = cats[i % cats.len()];
            values.push(format!(
                "({}, 'Product_{}', '{}', {}, 'Description {}')",
                i,
                i,
                cat,
                10 + (i % 990),
                i
            ));
        }
        pg.execute(&format!("INSERT INTO products VALUES {}", values.join(",")))
            .expect("PG35 constituent SQL must succeed");
    }

    for batch in 0..5 {
        let mut values = Vec::new();
        for j in 0..100 {
            let i = batch * 100 + j + 1;
            let cust_id = (i % 200) + 1;
            let status = statuses[i % statuses.len()];
            values.push(format!(
                "({}, {}, '2024-{:02}-{:02}', '{}', {})",
                i,
                cust_id,
                (i % 12) + 1,
                (i % 28) + 1,
                status,
                50 + (i % 950)
            ));
        }
        pg.execute(&format!("INSERT INTO orders VALUES {}", values.join(",")))
            .expect("PG35 constituent SQL must succeed");
    }

    for batch in 0..2 {
        let mut values = Vec::new();
        for j in 0..500 {
            let i = batch * 500 + j + 1;
            let ord_id = (i % 500) + 1;
            let prod_id = (i % 50) + 1;
            values.push(format!(
                "({}, {}, {}, {}, {})",
                i,
                ord_id,
                prod_id,
                1 + (i % 10),
                5 + (i % 100)
            ));
        }
        pg.execute(&format!("INSERT INTO order_items VALUES {}", values.join(",")))
            .expect("PG35 constituent SQL must succeed");
    }

    let cat_names = [
        "Electronics",
        "Books",
        "Clothing",
        "Food",
        "Sports",
        "Phones",
        "Laptops",
        "Tablets",
        "Fiction",
        "NonFiction",
        "Shirts",
        "Pants",
        "Shoes",
        "Snacks",
        "Drinks",
        "Soccer",
        "Tennis",
        "Swimming",
        "Camping",
        "Cycling",
    ];
    for (i, name) in cat_names.iter().enumerate() {
        let cat_id = (i + 1) as i32;
        let parent = if i < 5 {
            "NULL".to_string()
        } else {
            format!("{}", (i % 5) + 1)
        };
        pg.execute(&format!(
            "INSERT INTO categories VALUES ({}, '{}', {})",
            cat_id, name, parent
        ))
        .expect("PG35 constituent SQL must succeed");
    }

    pg.execute("ANALYZE").expect("PG35 constituent SQL must succeed");
}

// --- 35 benchmark categories ---

fn run_all_categories(
    db: &EmbeddedDatabase,
    pg: &PgClient,
    iters: usize,
    context: &strict::RunContext,
) -> Vec<ComparisonRow> {
    let mut results = Vec::new();

    // Helper tables for DDL benchmarks
    db.execute("CREATE TABLE IF NOT EXISTS bench_alt (id INT PRIMARY KEY, base TEXT)")
        .expect("PG35 constituent SQL must succeed");
    pg.execute("CREATE TABLE IF NOT EXISTS bench_alt (id INT PRIMARY KEY, base TEXT)")
        .expect("PG35 constituent SQL must succeed");

    // ============ DDL ============

    // 1. CREATE TABLE
    {
        let h = bench_helios_safe(db, "CREATE TABLE", iters, |db, i| {
            db.execute(&format!(
                "CREATE TABLE IF NOT EXISTS bench_ct_{} (id INT PRIMARY KEY, val TEXT)",
                i
            ))
            .map(|_| ())
            .map_err(|e| e.to_string())?;
            db.execute(&format!("DROP TABLE IF EXISTS bench_ct_{}", i))
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "CREATE TABLE", iters, |pg, i| {
            pg.execute(&format!(
                "CREATE TABLE IF NOT EXISTS bench_ct_{} (id INT PRIMARY KEY, val TEXT)",
                i
            ))
            .expect("PG35 constituent SQL must succeed");
            pg.execute(&format!("DROP TABLE IF EXISTS bench_ct_{}", i))
                .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "CREATE TABLE", &h, &p, iters));
    }

    // 2. CREATE INDEX
    {
        let h = bench_helios_safe(db, "CREATE INDEX", iters, |db, i| {
            db.execute(&format!(
                "CREATE INDEX IF NOT EXISTS bench_idx_{} ON customers(name)",
                i
            ))
            .map(|_| ())
            .map_err(|e| e.to_string())?;
            db.execute(&format!("DROP INDEX IF EXISTS bench_idx_{}", i))
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "CREATE INDEX", iters, |pg, i| {
            pg.execute(&format!(
                "CREATE INDEX IF NOT EXISTS bench_idx_{} ON customers(name)",
                i
            ))
            .expect("PG35 constituent SQL must succeed");
            pg.execute(&format!("DROP INDEX IF EXISTS bench_idx_{}", i))
                .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "CREATE INDEX", &h, &p, iters));
    }

    // 3. ALTER TABLE
    {
        let h = bench_helios_safe(db, "ALTER TABLE", iters, |db, i| {
            db.execute(&format!("ALTER TABLE bench_alt ADD COLUMN col_{} TEXT", i))
                .map(|_| ())
                .map_err(|e| e.to_string())?;
            db.execute(&format!("ALTER TABLE bench_alt DROP COLUMN col_{}", i))
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "ALTER TABLE", iters, |pg, i| {
            pg.execute(&format!("ALTER TABLE bench_alt ADD COLUMN col_{} TEXT", i))
                .expect("PG35 constituent SQL must succeed");
            pg.execute(&format!("ALTER TABLE bench_alt DROP COLUMN col_{}", i))
                .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "ALTER TABLE", &h, &p, iters));
    }

    // 4. DROP TABLE
    {
        for i in 0..(iters + 2) {
            db.execute(&format!("CREATE TABLE IF NOT EXISTS bench_drop_{} (id INT)", i))
                .expect("PG35 constituent SQL must succeed");
            pg.execute(&format!("CREATE TABLE IF NOT EXISTS bench_drop_{} (id INT)", i))
                .expect("PG35 constituent SQL must succeed");
        }
        let h = bench_helios_safe(db, "DROP TABLE", iters, |db, i| {
            db.execute(&format!("DROP TABLE IF EXISTS bench_drop_{}", i))
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "DROP TABLE", iters, |pg, i| {
            pg.execute(&format!("DROP TABLE IF EXISTS bench_drop_{}", i))
                .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "DROP TABLE", &h, &p, iters));
    }

    // 5. CREATE/DROP VIEW
    {
        let h = bench_helios_safe(db, "CREATE/DROP VIEW", iters, |db, i| {
            db.execute(&format!(
                "CREATE VIEW bench_v_{} AS SELECT id, name FROM customers WHERE age > 30",
                i
            ))
            .map(|_| ())
            .map_err(|e| e.to_string())?;
            db.execute(&format!("DROP VIEW IF EXISTS bench_v_{}", i))
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "CREATE/DROP VIEW", iters, |pg, i| {
            pg.execute(&format!(
                "CREATE VIEW bench_v_{} AS SELECT id, name FROM customers WHERE age > 30",
                i
            ))
            .expect("PG35 constituent SQL must succeed");
            pg.execute(&format!("DROP VIEW IF EXISTS bench_v_{}", i))
                .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "CREATE/DROP VIEW", &h, &p, iters));
    }

    // 6. REFRESH MATERIALIZED VIEW
    {
        db
            .execute("CREATE MATERIALIZED VIEW IF NOT EXISTS bench_mv AS SELECT region, COUNT(*) as cnt FROM customers GROUP BY region")
            .expect("Nano materialized-view setup must succeed");
        pg.execute("DROP MATERIALIZED VIEW IF EXISTS bench_mv")
            .expect("PG35 constituent SQL must succeed");
        pg.execute(
            "CREATE MATERIALIZED VIEW bench_mv AS SELECT region, COUNT(*) as cnt FROM customers GROUP BY region",
        )
        .expect("PG35 constituent SQL must succeed");

        let h = bench_helios_safe(db, "REFRESH MATVIEW", iters, |db, _| {
            db.execute("REFRESH MATERIALIZED VIEW bench_mv")
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "REFRESH MATVIEW", iters, |pg, _| {
            pg.execute("REFRESH MATERIALIZED VIEW bench_mv")
                .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "REFRESH MATVIEW", &h, &p, iters));
    }

    // 7. TRUNCATE
    {
        db.execute("CREATE TABLE IF NOT EXISTS bench_trunc (id INT, val TEXT)")
            .expect("PG35 constituent SQL must succeed");
        pg.execute("CREATE TABLE IF NOT EXISTS bench_trunc (id INT, val TEXT)")
            .expect("PG35 constituent SQL must succeed");

        let h = bench_helios_safe(db, "TRUNCATE", iters, |db, _| {
            for j in 0..5 {
                db.execute(&format!("INSERT INTO bench_trunc VALUES ({}, 'x')", j))
                    .map_err(|e| e.to_string())?;
            }
            db.execute("TRUNCATE bench_trunc")
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "TRUNCATE", iters, |pg, _| {
            for j in 0..5 {
                pg.execute(&format!("INSERT INTO bench_trunc VALUES ({}, 'x')", j))
                    .expect("PG35 constituent SQL must succeed");
            }
            pg.execute("TRUNCATE bench_trunc")
                .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "TRUNCATE", &h, &p, iters));
    }

    // ============ DML ============

    // 8. INSERT single row
    {
        db.execute("CREATE TABLE IF NOT EXISTS bench_ins (id INT PRIMARY KEY, val TEXT)")
            .expect("PG35 constituent SQL must succeed");
        pg.execute("DROP TABLE IF EXISTS bench_ins")
            .expect("PG35 constituent SQL must succeed");
        pg.execute("CREATE TABLE bench_ins (id INT PRIMARY KEY, val TEXT)")
            .expect("PG35 constituent SQL must succeed");

        let h = bench_helios(db, "INSERT single", iters, |db, i| {
            db.execute(&format!("INSERT INTO bench_ins VALUES ({}, 'v{}')", 10000 + i, i))
                .expect("PG35 constituent SQL must succeed");
        });
        let p = bench_pg(pg, "INSERT single", iters, |pg, i| {
            pg.execute(&format!("INSERT INTO bench_ins VALUES ({}, 'v{}')", 10000 + i, i))
                .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "INSERT single", &h, &p, iters));
    }

    // 9. INSERT multi-row
    {
        db.execute("CREATE TABLE IF NOT EXISTS bench_ins_m (id INT PRIMARY KEY, val TEXT)")
            .expect("PG35 constituent SQL must succeed");
        pg.execute("DROP TABLE IF EXISTS bench_ins_m")
            .expect("PG35 constituent SQL must succeed");
        pg.execute("CREATE TABLE bench_ins_m (id INT PRIMARY KEY, val TEXT)")
            .expect("PG35 constituent SQL must succeed");

        let h = bench_helios(db, "INSERT multi-row", iters, |db, i| {
            let base = 20000 + i * 10;
            let vals: Vec<String> = (0..10).map(|j| format!("({}, 'b{}')", base + j, j)).collect();
            db.execute(&format!("INSERT INTO bench_ins_m VALUES {}", vals.join(",")))
                .expect("PG35 constituent SQL must succeed");
        });
        let p = bench_pg(pg, "INSERT multi-row", iters, |pg, i| {
            let base = 20000 + i * 10;
            let vals: Vec<String> = (0..10).map(|j| format!("({}, 'b{}')", base + j, j)).collect();
            pg.execute(&format!("INSERT INTO bench_ins_m VALUES {}", vals.join(",")))
                .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "INSERT multi-row", &h, &p, iters));
    }

    // 10. INSERT...SELECT
    {
        db.execute("CREATE TABLE IF NOT EXISTS bench_ins_s (id INT PRIMARY KEY, name TEXT)")
            .expect("PG35 constituent SQL must succeed");
        pg.execute("DROP TABLE IF EXISTS bench_ins_s")
            .expect("PG35 constituent SQL must succeed");
        pg.execute("CREATE TABLE bench_ins_s (id INT PRIMARY KEY, name TEXT)")
            .expect("PG35 constituent SQL must succeed");

        let h = bench_helios_safe(db, "INSERT..SELECT", iters, |db, i| {
            let off = 30000 + i * 10;
            db.execute(&format!(
                "INSERT INTO bench_ins_s SELECT id + {}, name FROM customers WHERE id <= 10",
                off
            ))
            .map(|_| ())
            .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "INSERT..SELECT", iters, |pg, i| {
            let off = 30000 + i * 10;
            pg.execute(&format!(
                "INSERT INTO bench_ins_s SELECT id + {}, name FROM customers WHERE id <= 10",
                off
            ))
            .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "INSERT..SELECT", &h, &p, iters));
    }

    // 11. UPDATE point
    {
        let h = bench_helios(db, "UPDATE point", iters, |db, i| {
            let id = (i % 200) + 1;
            db.execute(&format!("UPDATE customers SET age = age + 0 WHERE id = {}", id))
                .expect("PG35 constituent SQL must succeed");
        });
        let p = bench_pg(pg, "UPDATE point", iters, |pg, i| {
            let id = (i % 200) + 1;
            pg.execute(&format!("UPDATE customers SET age = age + 0 WHERE id = {}", id))
                .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "UPDATE point", &h, &p, iters));
    }

    // 12. DELETE point
    {
        db.execute("CREATE TABLE IF NOT EXISTS bench_del (id INT PRIMARY KEY, val TEXT)")
            .expect("PG35 constituent SQL must succeed");
        pg.execute("DROP TABLE IF EXISTS bench_del")
            .expect("PG35 constituent SQL must succeed");
        pg.execute("CREATE TABLE bench_del (id INT PRIMARY KEY, val TEXT)")
            .expect("PG35 constituent SQL must succeed");
        for i in 0..(iters + 2) {
            db.execute(&format!("INSERT INTO bench_del VALUES ({}, 'd{}')", i, i))
                .expect("PG35 constituent SQL must succeed");
            pg.execute(&format!("INSERT INTO bench_del VALUES ({}, 'd{}')", i, i))
                .expect("PG35 constituent SQL must succeed");
        }

        let h = bench_helios(db, "DELETE point", iters, |db, i| {
            db.execute(&format!("DELETE FROM bench_del WHERE id = {}", i))
                .expect("PG35 constituent SQL must succeed");
        });
        let p = bench_pg(pg, "DELETE point", iters, |pg, i| {
            pg.execute(&format!("DELETE FROM bench_del WHERE id = {}", i))
                .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "DELETE point", &h, &p, iters));
    }

    // 13. UPSERT
    {
        db.execute("CREATE TABLE IF NOT EXISTS bench_ups (id INT PRIMARY KEY, val TEXT, counter INT)")
            .expect("PG35 constituent SQL must succeed");
        pg.execute("DROP TABLE IF EXISTS bench_ups")
            .expect("PG35 constituent SQL must succeed");
        pg.execute("CREATE TABLE bench_ups (id INT PRIMARY KEY, val TEXT, counter INT)")
            .expect("PG35 constituent SQL must succeed");
        for i in 0..30 {
            db.execute(&format!("INSERT INTO bench_ups VALUES ({}, 'orig', 0)", i))
                .expect("PG35 constituent SQL must succeed");
            pg.execute(&format!("INSERT INTO bench_ups VALUES ({}, 'orig', 0)", i))
                .expect("PG35 constituent SQL must succeed");
        }

        let h = bench_helios_safe(db, "UPSERT", iters, |db, i| {
            let id = i % 30;
            db.execute(&format!(
                "INSERT INTO bench_ups VALUES ({}, 'new', 1) ON CONFLICT (id) DO UPDATE SET counter = bench_ups.counter + 1",
                id
            ))
            .map(|_| ())
            .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "UPSERT", iters, |pg, i| {
            let id = i % 30;
            pg.execute(&format!(
                "INSERT INTO bench_ups VALUES ({}, 'new', 1) ON CONFLICT (id) DO UPDATE SET counter = bench_ups.counter + 1",
                id
            )).expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "UPSERT", &h, &p, iters));
    }

    // 14. UPDATE with subquery
    {
        let h = bench_helios_safe(db, "UPDATE+subquery", iters, |db, i| {
            let id = (i % 500) + 1;
            db.execute(&format!(
                "UPDATE orders SET total = (SELECT COUNT(*) FROM order_items WHERE order_id = {}) WHERE order_id = {}",
                id, id
            ))
            .map(|_| ())
            .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "UPDATE+subquery", iters, |pg, i| {
            let id = (i % 500) + 1;
            pg.execute(&format!(
                "UPDATE orders SET total = (SELECT COUNT(*) FROM order_items WHERE order_id = {}) WHERE order_id = {}",
                id, id
            ))
            .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "UPDATE+subquery", &h, &p, iters));
    }

    // ============ QUERIES ============

    // 15. Point lookup
    {
        let h = bench_helios(db, "Point lookup", iters, |db, i| {
            let id = (i % 200) + 1;
            db.query(&format!("SELECT * FROM customers WHERE id = {}", id), &[])
                .expect("PG35 constituent SQL must succeed");
        });
        let p = bench_pg(pg, "Point lookup", iters, |pg, i| {
            let id = (i % 200) + 1;
            pg.query_count(&format!("SELECT * FROM customers WHERE id = {}", id))
                .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "Point lookup", &h, &p, iters));
    }

    // 16. Full scan + filter
    {
        let h = bench_helios(db, "Full scan+filter", iters, |db, _| {
            db.query("SELECT * FROM customers WHERE age > 50", &[])
                .expect("PG35 constituent SQL must succeed");
        });
        let p = bench_pg(pg, "Full scan+filter", iters, |pg, _| {
            pg.query_count("SELECT * FROM customers WHERE age > 50")
                .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "Full scan+filter", &h, &p, iters));
    }

    // 17. Aggregation
    {
        let h = bench_helios(db, "Aggregation", iters, |db, _| {
            db.query(
                "SELECT region, COUNT(*), SUM(age), AVG(age) FROM customers GROUP BY region HAVING COUNT(*) > 10",
                &[],
            )
            .expect("PG35 constituent SQL must succeed");
        });
        let p = bench_pg(pg, "Aggregation", iters, |pg, _| {
            pg.query_count(
                "SELECT region, COUNT(*), SUM(age), AVG(age) FROM customers GROUP BY region HAVING COUNT(*) > 10",
            )
            .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "Aggregation", &h, &p, iters));
    }

    // 18. INNER JOIN
    {
        let h = bench_helios(db, "INNER JOIN", iters, |db, i| {
            let id = (i % 200) + 1;
            db.query(
                &format!(
                    "SELECT c.name, o.order_id, o.total FROM customers c INNER JOIN orders o ON c.id = o.customer_id WHERE c.id = {}",
                    id
                ),
                &[],
            ).expect("PG35 constituent SQL must succeed");
        });
        let p = bench_pg(pg, "INNER JOIN", iters, |pg, i| {
            let id = (i % 200) + 1;
            pg.query_count(&format!(
                "SELECT c.name, o.order_id, o.total FROM customers c INNER JOIN orders o ON c.id = o.customer_id WHERE c.id = {}",
                id
            )).expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "INNER JOIN", &h, &p, iters));
    }

    // 19. LEFT JOIN
    {
        let h = bench_helios(db, "LEFT JOIN", iters, |db, i| {
            let id = (i % 200) + 1;
            db.query(
                &format!(
                    "SELECT c.name, o.order_id FROM customers c LEFT JOIN orders o ON c.id = o.customer_id WHERE c.id = {}",
                    id
                ),
                &[],
            ).expect("PG35 constituent SQL must succeed");
        });
        let p = bench_pg(pg, "LEFT JOIN", iters, |pg, i| {
            let id = (i % 200) + 1;
            pg.query_count(&format!(
                "SELECT c.name, o.order_id FROM customers c LEFT JOIN orders o ON c.id = o.customer_id WHERE c.id = {}",
                id
            ))
            .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "LEFT JOIN", &h, &p, iters));
    }

    // 20. 4-table JOIN
    {
        let h = bench_helios(db, "4-table JOIN", iters, |db, i| {
            let id = (i % 200) + 1;
            db.query(
                &format!(
                    "SELECT c.name, o.order_id, oi.quantity, p.name \
                     FROM customers c \
                     INNER JOIN orders o ON c.id = o.customer_id \
                     INNER JOIN order_items oi ON o.order_id = oi.order_id \
                     INNER JOIN products p ON oi.product_id = p.product_id \
                     WHERE c.id = {}",
                    id
                ),
                &[],
            )
            .expect("PG35 constituent SQL must succeed");
        });
        let p = bench_pg(pg, "4-table JOIN", iters, |pg, i| {
            let id = (i % 200) + 1;
            pg.query_count(&format!(
                "SELECT c.name, o.order_id, oi.quantity, p.name \
                 FROM customers c \
                 INNER JOIN orders o ON c.id = o.customer_id \
                 INNER JOIN order_items oi ON o.order_id = oi.order_id \
                 INNER JOIN products p ON oi.product_id = p.product_id \
                 WHERE c.id = {}",
                id
            ))
            .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "4-table JOIN", &h, &p, iters));
    }

    // 21. Scalar subquery
    {
        let h = bench_helios_safe(db, "Scalar subquery", iters, |db, _| {
            db.query(
                "SELECT name, (SELECT COUNT(*) FROM orders WHERE customer_id = customers.id) AS order_count FROM customers WHERE id <= 20",
                &[],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "Scalar subquery", iters, |pg, _| {
            pg.query_count(
                "SELECT name, (SELECT COUNT(*) FROM orders WHERE customer_id = customers.id) AS order_count FROM customers WHERE id <= 20",
            ).expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "Scalar subquery", &h, &p, iters));
    }

    // 22. EXISTS subquery
    {
        let h = bench_helios_safe(db, "EXISTS subquery", iters, |db, _| {
            db.query(strict::EXISTS_QUERY, &[])
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "EXISTS subquery", iters, |pg, _| {
            pg.query_count(strict::EXISTS_QUERY)
                .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "EXISTS subquery", &h, &p, iters));
    }

    // 23. IN subquery
    {
        let h = bench_helios_safe(db, "IN subquery", iters, |db, _| {
            db.query(
                "SELECT name FROM customers WHERE id IN (SELECT customer_id FROM orders WHERE total > 500) AND id <= 100",
                &[],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "IN subquery", iters, |pg, _| {
            pg.query_count(
                "SELECT name FROM customers WHERE id IN (SELECT customer_id FROM orders WHERE total > 500) AND id <= 100",
            ).expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "IN subquery", &h, &p, iters));
    }

    // 24. CTE (non-recursive)
    {
        let h = bench_helios_safe(db, "CTE", iters, |db, _| {
            db.query(
                "WITH high_value AS (SELECT customer_id, SUM(total) as total_spent FROM orders GROUP BY customer_id HAVING SUM(total) > 500) \
                 SELECT c.name, hv.total_spent FROM customers c JOIN high_value hv ON c.id = hv.customer_id",
                &[],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "CTE", iters, |pg, _| {
            pg.query_count(
                "WITH high_value AS (SELECT customer_id, SUM(total) as total_spent FROM orders GROUP BY customer_id HAVING SUM(total) > 500) \
                 SELECT c.name, hv.total_spent FROM customers c JOIN high_value hv ON c.id = hv.customer_id",
            ).expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "CTE", &h, &p, iters));
    }

    // 25. Recursive CTE
    {
        let h = bench_helios_safe(db, "Recursive CTE", iters, |db, _| {
            db.query(
                "WITH RECURSIVE cat_tree AS (\
                    SELECT cat_id, name, parent_id, 0 AS depth FROM categories WHERE parent_id IS NULL \
                    UNION ALL \
                    SELECT c.cat_id, c.name, c.parent_id, ct.depth + 1 FROM categories c JOIN cat_tree ct ON c.parent_id = ct.cat_id\
                 ) SELECT * FROM cat_tree",
                &[],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "Recursive CTE", iters, |pg, _| {
            pg.query_count(
                "WITH RECURSIVE cat_tree AS (\
                    SELECT cat_id, name, parent_id, 0 AS depth FROM categories WHERE parent_id IS NULL \
                    UNION ALL \
                    SELECT c.cat_id, c.name, c.parent_id, ct.depth + 1 FROM categories c JOIN cat_tree ct ON c.parent_id = ct.cat_id\
                 ) SELECT * FROM cat_tree",
            ).expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "Recursive CTE", &h, &p, iters));
    }

    // 26. Window functions
    {
        let h = bench_helios_safe(db, "Window funcs", iters, |db, _| {
            db.query(
                "SELECT name, age, region, \
                    ROW_NUMBER() OVER (PARTITION BY region ORDER BY age DESC, id ASC) as rn, \
                    RANK() OVER (PARTITION BY region ORDER BY age DESC) as rnk, \
                    SUM(age) OVER (PARTITION BY region) as region_total \
                 FROM customers WHERE id <= 100",
                &[],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "Window funcs", iters, |pg, _| {
            pg.query_count(
                "SELECT name, age, region, \
                    ROW_NUMBER() OVER (PARTITION BY region ORDER BY age DESC, id ASC) as rn, \
                    RANK() OVER (PARTITION BY region ORDER BY age DESC) as rnk, \
                    SUM(age) OVER (PARTITION BY region) as region_total \
                 FROM customers WHERE id <= 100",
            )
            .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "Window funcs", &h, &p, iters));
    }

    // 27. UNION
    {
        let h = bench_helios_safe(db, "UNION", iters, |db, _| {
            db.query(
                "SELECT name FROM customers WHERE age < 30 UNION SELECT name FROM customers WHERE region = 'East'",
                &[],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "UNION", iters, |pg, _| {
            pg.query_count(
                "SELECT name FROM customers WHERE age < 30 UNION SELECT name FROM customers WHERE region = 'East'",
            )
            .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "UNION", &h, &p, iters));
    }

    // 28. DISTINCT
    {
        let h = bench_helios(db, "DISTINCT", iters, |db, _| {
            db.query("SELECT DISTINCT region FROM customers", &[])
                .expect("PG35 constituent SQL must succeed");
        });
        let p = bench_pg(pg, "DISTINCT", iters, |pg, _| {
            pg.query_count("SELECT DISTINCT region FROM customers")
                .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "DISTINCT", &h, &p, iters));
    }

    // 29. ORDER BY + LIMIT
    {
        let h = bench_helios(db, "ORDER+LIMIT", iters, |db, i| {
            let off = (i % 50) * 10;
            db.query(
                &format!(
                    "SELECT * FROM customers ORDER BY age DESC, name ASC LIMIT 20 OFFSET {}",
                    off
                ),
                &[],
            )
            .expect("PG35 constituent SQL must succeed");
        });
        let p = bench_pg(pg, "ORDER+LIMIT", iters, |pg, i| {
            let off = (i % 50) * 10;
            pg.query_count(&format!(
                "SELECT * FROM customers ORDER BY age DESC, name ASC LIMIT 20 OFFSET {}",
                off
            ))
            .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "ORDER+LIMIT", &h, &p, iters));
    }

    // 30. CASE expressions
    {
        let h = bench_helios(db, "CASE expr", iters, |db, _| {
            db.query(
                "SELECT name, \
                    CASE WHEN age < 25 THEN 'young' WHEN age < 50 THEN 'middle' ELSE 'senior' END AS age_group \
                 FROM customers WHERE id <= 100",
                &[],
            )
            .expect("PG35 constituent SQL must succeed");
        });
        let p = bench_pg(pg, "CASE expr", iters, |pg, _| {
            pg.query_count(
                "SELECT name, \
                    CASE WHEN age < 25 THEN 'young' WHEN age < 50 THEN 'middle' ELSE 'senior' END AS age_group \
                 FROM customers WHERE id <= 100",
            )
            .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "CASE expr", &h, &p, iters));
    }

    // 31. LIKE / BETWEEN / IN list
    {
        let h = bench_helios(db, "LIKE/BETWEEN/IN", iters, |db, _| {
            db.query(
                "SELECT * FROM customers WHERE name LIKE 'Customer_1%' AND age BETWEEN 20 AND 50 AND region IN ('East', 'West', 'North')",
                &[],
            ).expect("PG35 constituent SQL must succeed");
        });
        let p = bench_pg(pg, "LIKE/BETWEEN/IN", iters, |pg, _| {
            pg.query_count(
                "SELECT * FROM customers WHERE name LIKE 'Customer_1%' AND age BETWEEN 20 AND 50 AND region IN ('East', 'West', 'North')",
            ).expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "LIKE/BETWEEN/IN", &h, &p, iters));
    }

    // 32. String ops
    {
        let h = bench_helios(db, "String ops", iters, |db, _| {
            db.query(
                "SELECT id, name, LENGTH(bio) as bio_len FROM customers WHERE bio IS NOT NULL AND id <= 100",
                &[],
            )
            .expect("PG35 constituent SQL must succeed");
        });
        let p = bench_pg(pg, "String ops", iters, |pg, _| {
            pg.query_count(
                "SELECT id, name, LENGTH(bio) as bio_len FROM customers WHERE bio IS NOT NULL AND id <= 100",
            )
            .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "String ops", &h, &p, iters));
    }

    // ============ CONTROL ============

    // 33. Transaction control
    {
        let h = bench_helios_safe(db, "Transaction ctl", iters, |db, _| {
            db.execute("BEGIN").map(|_| ()).map_err(|e| e.to_string())?;
            db.execute("SAVEPOINT sp1").map(|_| ()).map_err(|e| e.to_string())?;
            db.execute("INSERT INTO bench_ins VALUES (99999, 'txn')")
                .map(|_| ())
                .map_err(|e| e.to_string())?;
            db.execute("ROLLBACK TO SAVEPOINT sp1")
                .map(|_| ())
                .map_err(|e| e.to_string())?;
            db.execute("COMMIT").map(|_| ()).map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "Transaction ctl", iters, |pg, _| {
            pg.execute("BEGIN").expect("PG35 constituent SQL must succeed");
            pg.execute("SAVEPOINT sp1").expect("PG35 constituent SQL must succeed");
            pg.execute("INSERT INTO bench_ins VALUES (99999, 'txn')")
                .expect("PG35 constituent SQL must succeed");
            pg.execute("ROLLBACK TO SAVEPOINT sp1")
                .expect("PG35 constituent SQL must succeed");
            pg.execute("COMMIT").expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "Transaction ctl", &h, &p, iters));
    }

    // 34. PREPARE / EXECUTE / DEALLOCATE
    {
        let h = bench_helios_safe(db, "Prepared stmts", iters, |db, i| {
            let name = format!("bench_s_{}", i);
            db.query(
                &format!("PREPARE {} AS SELECT * FROM customers WHERE id = $1", name),
                &[],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())?;
            strict::timed_execute_nano(db, &format!("EXECUTE {}(42)", name));
            db.query(&format!("DEALLOCATE {}", name), &[])
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "Prepared stmts", iters, |pg, i| {
            let name = format!("bench_s_{}", i);
            pg.execute(&format!(
                "PREPARE {} (int) AS SELECT * FROM customers WHERE id = $1",
                name
            ))
            .expect("PG35 constituent SQL must succeed");
            strict::timed_execute_pg(pg, &format!("EXECUTE {} (42)", name));
            pg.execute(&format!("DEALLOCATE {}", name))
                .expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "Prepared stmts", &h, &p, iters));
    }

    // 35. SET/SHOW/RESET
    {
        let h = bench_helios_safe(db, "SET/SHOW/RESET", iters, |db, _| {
            db.query("SET work_mem = '8192'", &[])
                .map(|_| ())
                .map_err(|e| e.to_string())?;
            strict::timed_show_nano(db);
            db.query("RESET work_mem", &[]).map(|_| ()).map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "SET/SHOW/RESET", iters, |pg, _| {
            pg.execute("SET work_mem = '8MB'")
                .expect("PG35 constituent SQL must succeed");
            strict::timed_show_pg(pg);
            pg.execute("RESET work_mem").expect("PG35 constituent SQL must succeed");
        });
        results.push(checked_result(db, pg, context, "SET/SHOW/RESET", &h, &p, iters));
    }

    results
}

fn checked_result(
    db: &EmbeddedDatabase,
    pg: &PgClient,
    context: &strict::RunContext,
    name: &str,
    h: &CategoryResult,
    p: &CategoryResult,
    iters: usize,
) -> ComparisonRow {
    assert_eq!(h.name, name);
    assert_eq!(p.name, name);
    assert_eq!((h.iterations, h.successes, h.warmup_successes), (iters, 300, 2));
    assert_eq!((p.iterations, p.successes, p.warmup_successes), (iters, 300, 2));
    let semantic_checks = strict::verify_category(db, pg, context, name, iters);
    assert!(semantic_checks > 0, "each category requires a real oracle");
    let mut row = compare_result(name, h, p);
    row.proof = serde_json::json!({"category":name,"nano_successes":h.successes,"postgres_successes":p.successes,
        "nano_errors":0,"postgres_errors":0,"nano_warmup_successes":h.warmup_successes,"postgres_warmup_successes":p.warmup_successes,
        "warmup_errors":0,"semantic_checks":semantic_checks,"semantic_checks_passed":true,
        "nano_elapsed_ns":u64::try_from(h.wall_time.as_nanos()).expect("Nano elapsed time fits u64"),
        "postgres_elapsed_ns":u64::try_from(p.wall_time.as_nanos()).expect("Postgres elapsed time fits u64")});
    row
}

fn compare_result(name: &str, h: &CategoryResult, p: &CategoryResult) -> ComparisonRow {
    // Legacy microsecond-floor precision:
    // let h_us = h.avg_per_iter.as_micros() as f64;
    // let p_us = p.avg_per_iter.as_micros() as f64;
    let h_us = h.avg_per_iter.as_nanos() as f64 / 1_000.0;
    let p_us = p.avg_per_iter.as_nanos() as f64 / 1_000.0;
    let ratio = if p_us > 0.0 { h_us / p_us } else { 0.0 };
    let winner = if ratio <= 1.0 {
        "Helios".to_string()
    } else {
        "PG".to_string()
    };
    ComparisonRow {
        name: name.to_string(),
        helios_avg_us: h_us,
        pg_avg_us: p_us,
        ratio,
        winner,
        helios_na: false,
        proof: serde_json::Value::Null,
    }
}

// --- Output formatting ---

// Legacy fixed-label signature:
// fn print_comparison_table(results: &[ComparisonRow]) {
fn print_comparison_table(results: &[ComparisonRow], pg_label: &str) {
    println!("\n{}", "=".repeat(105));
    // Legacy fixed label:
    // println!("  HELIOSDB-NANO vs POSTGRESQL 16 -- HEAD-TO-HEAD COMPARISON");
    println!("  HELIOSDB-NANO vs {} -- HEAD-TO-HEAD COMPARISON", pg_label);
    println!("  Dataset: 200 customers, 50 products, 500 orders, 1000 items, 20 categories");
    println!("{}\n", "=".repeat(105));

    let pg_col = format!("{}(avg)", pg_label);
    println!(
        "{:<22} | {:>12} | {:>12} | {:>10} | {:>8}",
        // Legacy fixed column label:
        // "Category", "Nano(avg)", "PG 16(avg)", "Ratio", "Winner"
        "Category",
        "Nano(avg)",
        pg_col,
        "Ratio",
        "Winner"
    );
    println!("{}", "-".repeat(105));

    let mut helios_wins = 0;
    let mut pg_wins = 0;
    let mut ties = 0;
    let mut na_count = 0;

    for r in results {
        if r.helios_na {
            na_count += 1;
            println!(
                "{:<22} | {:>12} | {:>12} | {:>10} | {:>8}",
                r.name,
                "N/A",
                format_us(r.pg_avg_us),
                "--",
                "N/A"
            );
            continue;
        }

        let ratio_str = if r.ratio == 0.0 {
            "N/A".to_string()
        } else if r.ratio <= 1.0 {
            format!("{:.2}x", 1.0 / r.ratio)
        } else {
            format!("{:.2}x", r.ratio)
        };

        let winner_display = if r.ratio == 0.0 {
            "tie"
        } else if r.ratio <= 0.95 {
            helios_wins += 1;
            "Nano"
        } else if r.ratio >= 1.05 {
            pg_wins += 1;
            "PG"
        } else {
            ties += 1;
            "~tie"
        };

        println!(
            "{:<22} | {:>12} | {:>12} | {:>10} | {:>8}",
            r.name,
            format_us(r.helios_avg_us),
            format_us(r.pg_avg_us),
            ratio_str,
            winner_display,
        );
    }

    println!("{}", "-".repeat(105));
    println!(
        "SCOREBOARD: Nano wins {} | PG wins {} | Ties {} | N/A {} | Total {}",
        helios_wins,
        pg_wins,
        ties,
        na_count,
        results.len()
    );
}

fn print_trace_breakdown(results: &[ComparisonRow]) {
    println!("\n{}", "=".repeat(80));
    println!("  TOP OPTIMIZATION TARGETS (where PG wins by largest margin)");
    println!("{}\n", "=".repeat(80));

    let mut sorted: Vec<&ComparisonRow> = results.iter().filter(|r| !r.helios_na && r.ratio > 1.05).collect();
    sorted.sort_by(|a, b| b.ratio.partial_cmp(&a.ratio).unwrap_or(std::cmp::Ordering::Equal));

    for (i, r) in sorted.iter().take(10).enumerate() {
        println!(
            "  {:>2}. {:<22} Nano {:>10} vs PG {:>10} ({:.1}x slower)",
            i + 1,
            r.name,
            format_us(r.helios_avg_us),
            format_us(r.pg_avg_us),
            r.ratio,
        );
    }

    if sorted.is_empty() {
        println!("  (Nano is competitive or faster across all categories!)");
    }

    // Nano advantages
    let mut helios_better: Vec<&ComparisonRow> = results
        .iter()
        .filter(|r| !r.helios_na && r.ratio < 0.95 && r.ratio > 0.0)
        .collect();
    helios_better.sort_by(|a, b| a.ratio.partial_cmp(&b.ratio).unwrap_or(std::cmp::Ordering::Equal));

    if !helios_better.is_empty() {
        println!("\n  NANO ADVANTAGES (faster than PG):");
        for (i, r) in helios_better.iter().take(10).enumerate() {
            println!(
                "  {:>2}. {:<22} Nano {:>10} vs PG {:>10} ({:.1}x faster)",
                i + 1,
                r.name,
                format_us(r.helios_avg_us),
                format_us(r.pg_avg_us),
                1.0 / r.ratio,
            );
        }
    }
}

fn print_analysis(results: &[ComparisonRow]) {
    println!("\n{}", "=".repeat(80));
    println!("  PERFORMANCE TIER ANALYSIS");
    println!("{}\n", "=".repeat(80));

    let active: Vec<&ComparisonRow> = results.iter().filter(|r| !r.helios_na).collect();

    let critical: Vec<&&ComparisonRow> = active.iter().filter(|r| r.ratio > 5.0).collect();
    let significant: Vec<&&ComparisonRow> = active.iter().filter(|r| r.ratio > 2.0 && r.ratio <= 5.0).collect();
    let moderate: Vec<&&ComparisonRow> = active.iter().filter(|r| r.ratio > 1.05 && r.ratio <= 2.0).collect();
    let competitive: Vec<&&ComparisonRow> = active.iter().filter(|r| r.ratio >= 0.95 && r.ratio <= 1.05).collect();
    let helios_faster: Vec<&&ComparisonRow> = active.iter().filter(|r| r.ratio > 0.0 && r.ratio < 0.95).collect();
    let na_list: Vec<&ComparisonRow> = results.iter().filter(|r| r.helios_na).collect();

    println!("    Critical (>5x slower):    {} categories", critical.len());
    println!("    Significant (2-5x):       {} categories", significant.len());
    println!("    Moderate (1.05-2x):       {} categories", moderate.len());
    println!("    Competitive (~1x):        {} categories", competitive.len());
    println!("    Nano faster (<0.95x):     {} categories", helios_faster.len());
    println!("    Not supported (N/A):      {} categories", na_list.len());

    if !na_list.is_empty() {
        println!("\n  N/A CATEGORIES (not supported in Nano):");
        for r in &na_list {
            println!("    - {}", r.name);
        }
    }
}

// --- Main test ---

#[test]
#[ignore = "Requires the root-serialized native PostgreSQL18.4 reference and strict receipt environment"]
fn pg35_benchmark() {
    let environment = strict::ReceiptEnvironment::from_env();
    let iters = pg35_iters();
    let pg_label = pg35_label();
    assert_eq!(pg_label, "POSTGRESQL 18.4");
    let pg_connstr = pg35_connstr();
    let pg = PgClient::connect(&pg_connstr)
        .unwrap_or_else(|e| panic!("Cannot connect to required PostgreSQL18.4 reference: {e}"));
    let db = EmbeddedDatabase::new_in_memory().expect("Nano in-memory instance must open");
    let context = strict::RunContext::new(&db, &pg, &pg_connstr, environment);
    println!("HeliosDB-Nano embedded/in-memory vs actual PostgreSQL18.4 on pg_bench_nano");
    setup_helios_schema(&db);
    setup_pg_schema(&pg);
    populate_helios(&db);
    populate_pg(&pg);
    let dataset_counts = strict::verify_seed_data(&db, &pg);
    // Keep the inherited independently parsed announcement's exact spelling.
    println!("Running 35 categories x 300 iterations each x 2 engines...");
    let start = Instant::now();
    let results = run_all_categories(&db, &pg, iters, &context);
    let total_time = start.elapsed();
    strict::finish_receipt(&db, &pg, &context, &results, dataset_counts);
    print_comparison_table(&results, &pg_label);
    print_trace_breakdown(&results);
    print_analysis(&results);
    println!("BENCHMARK COMPLETE -- Total time: {total_time:.1?}");
}
