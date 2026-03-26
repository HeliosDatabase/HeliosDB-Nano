//! HeliosDB-Nano vs PostgreSQL 16 — Head-to-Head Performance Comparison
//!
//! Runs 35 SQL categories against both engines with identical schema/data.
//!
//! Prerequisites:
//!   docker run -d --name pg_bench_nano -e POSTGRES_USER=bench -e POSTGRES_PASSWORD=benchpass \
//!     -e POSTGRES_DB=benchdb -p 25433:5432 postgres:16-alpine
//!
//! Run with:
//!   cargo test --release --test pg35_benchmark -- --nocapture --ignored

use heliosdb_nano::{EmbeddedDatabase, Value};
use std::time::{Duration, Instant};

// --- Result types ---

struct CategoryResult {
    name: String,
    #[allow(dead_code)]
    iterations: usize,
    #[allow(dead_code)]
    wall_time: Duration,
    avg_per_iter: Duration,
}

struct ComparisonRow {
    name: String,
    helios_avg_us: f64,
    pg_avg_us: f64,
    ratio: f64,
    #[allow(dead_code)]
    winner: String,
    helios_na: bool,
}

// --- PostgreSQL client (synchronous wrapper around tokio-postgres) ---

struct PgClient {
    rt: tokio::runtime::Runtime,
    client: tokio_postgres::Client,
    _handle: tokio::task::JoinHandle<()>,
}

impl PgClient {
    fn connect(connstr: &str) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (client, connection) =
            rt.block_on(tokio_postgres::connect(connstr, tokio_postgres::NoTls))?;
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

fn bench_helios<F>(db: &EmbeddedDatabase, name: &str, iters: usize, f: F) -> CategoryResult
where
    F: Fn(&EmbeddedDatabase, usize),
{
    // Warmup (2 iterations, not timed)
    for i in 0..2 {
        f(db, i);
    }

    let wall_start = Instant::now();
    for i in 0..iters {
        f(db, i);
    }
    let wall_time = wall_start.elapsed();

    CategoryResult {
        name: name.to_string(),
        iterations: iters,
        wall_time,
        avg_per_iter: wall_time / iters as u32,
    }
}

fn bench_helios_safe<F>(
    db: &EmbeddedDatabase,
    name: &str,
    iters: usize,
    f: F,
) -> Option<CategoryResult>
where
    F: Fn(&EmbeddedDatabase, usize) -> std::result::Result<(), String>,
{
    // Test if the feature works at all
    match f(db, 99999) {
        Ok(_) => {}
        Err(e) => {
            eprintln!("  [N/A] {} -- {}", name, e);
            return None;
        }
    }

    // Warmup
    for i in 0..2 {
        let _ = f(db, i);
    }

    let wall_start = Instant::now();
    for i in 0..iters {
        let _ = f(db, i);
    }
    let wall_time = wall_start.elapsed();

    Some(CategoryResult {
        name: name.to_string(),
        iterations: iters,
        wall_time,
        avg_per_iter: wall_time / iters as u32,
    })
}

fn bench_pg<F>(pg: &PgClient, name: &str, iters: usize, f: F) -> CategoryResult
where
    F: Fn(&PgClient, usize),
{
    // Warmup
    for i in 0..2 {
        f(pg, i);
    }

    let wall_start = Instant::now();
    for i in 0..iters {
        f(pg, i);
    }
    let wall_time = wall_start.elapsed();

    CategoryResult {
        name: name.to_string(),
        iterations: iters,
        wall_time,
        avg_per_iter: wall_time / iters as u32,
    }
}

fn format_us(us: f64) -> String {
    if us >= 1_000_000.0 {
        format!("{:.1}s", us / 1_000_000.0)
    } else if us >= 1_000.0 {
        format!("{:.2}ms", us / 1_000.0)
    } else {
        format!("{:.0}us", us)
    }
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
    vec![
        "CREATE TABLE customers (id INT PRIMARY KEY, name TEXT, email TEXT, age INT, region TEXT, metadata TEXT, bio TEXT)".into(),
        "CREATE TABLE products (product_id INT PRIMARY KEY, name TEXT, category TEXT, price INT, description TEXT)".into(),
        "CREATE TABLE orders (order_id INT PRIMARY KEY, customer_id INT REFERENCES customers(id), order_date TEXT, status TEXT, total INT)".into(),
        "CREATE TABLE order_items (item_id INT PRIMARY KEY, order_id INT REFERENCES orders(order_id), product_id INT REFERENCES products(product_id), quantity INT, unit_price INT)".into(),
        "CREATE TABLE categories (cat_id INT PRIMARY KEY, name TEXT, parent_id INT)".into(),
        "CREATE INDEX idx_cust_age ON customers(age)".into(),
        "CREATE INDEX idx_ord_cust ON orders(customer_id)".into(),
        "CREATE INDEX idx_oi_ord ON order_items(order_id)".into(),
    ]
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
        let _ = pg.execute(sql);
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
        let _ = db.execute(&format!(
            "INSERT INTO customers VALUES ({}, 'Customer_{}', 'c{}@test.com', {}, '{}', '{{\"tier\":\"{}\"}}', 'Bio for customer {}')",
            i, i, i, age, region, if i % 3 == 0 { "gold" } else { "silver" }, i
        ));
    }

    // Products: 50 rows
    let cats = ["Electronics", "Books", "Clothing", "Food", "Sports"];
    for i in 1..=50 {
        let cat = cats[i % cats.len()];
        let _ = db.execute(&format!(
            "INSERT INTO products VALUES ({}, 'Product_{}', '{}', {}, 'Description {}')",
            i,
            i,
            cat,
            10 + (i % 990),
            i
        ));
    }

    // Orders: 500 rows
    for i in 1..=500 {
        let cust_id = (i % 200) + 1;
        let status = statuses[i % statuses.len()];
        let _ = db.execute(&format!(
            "INSERT INTO orders VALUES ({}, {}, '2024-{:02}-{:02}', '{}', {})",
            i,
            cust_id,
            (i % 12) + 1,
            (i % 28) + 1,
            status,
            50 + (i % 950)
        ));
    }

    // Order items: 1000 rows
    for i in 1..=1000 {
        let ord_id = (i % 500) + 1;
        let prod_id = (i % 50) + 1;
        let _ = db.execute(&format!(
            "INSERT INTO order_items VALUES ({}, {}, {}, {}, {})",
            i,
            ord_id,
            prod_id,
            1 + (i % 10),
            5 + (i % 100)
        ));
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
        let _ = db.execute(&format!(
            "INSERT INTO categories VALUES ({}, '{}', {})",
            cat_id, name, parent
        ));
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
        let _ = pg.execute(&format!(
            "INSERT INTO customers VALUES {}",
            values.join(",")
        ));
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
        let _ = pg.execute(&format!(
            "INSERT INTO products VALUES {}",
            values.join(",")
        ));
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
        let _ = pg.execute(&format!(
            "INSERT INTO orders VALUES {}",
            values.join(",")
        ));
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
        let _ = pg.execute(&format!(
            "INSERT INTO order_items VALUES {}",
            values.join(",")
        ));
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
        let _ = pg.execute(&format!(
            "INSERT INTO categories VALUES ({}, '{}', {})",
            cat_id, name, parent
        ));
    }

    let _ = pg.execute("ANALYZE");
}

// --- 35 benchmark categories ---

fn run_all_categories(
    db: &EmbeddedDatabase,
    pg: &PgClient,
    iters: usize,
) -> Vec<ComparisonRow> {
    let mut results = Vec::new();

    // Helper tables for DDL benchmarks
    let _ = db.execute("CREATE TABLE IF NOT EXISTS bench_alt (id INT PRIMARY KEY, base TEXT)");
    let _ = pg.execute("CREATE TABLE IF NOT EXISTS bench_alt (id INT PRIMARY KEY, base TEXT)");

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
            let _ = pg.execute(&format!(
                "CREATE TABLE IF NOT EXISTS bench_ct_{} (id INT PRIMARY KEY, val TEXT)",
                i
            ));
            let _ = pg.execute(&format!("DROP TABLE IF EXISTS bench_ct_{}", i));
        });
        results.push(compare_safe("CREATE TABLE", h.as_ref(), &p));
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
            let _ = pg.execute(&format!(
                "CREATE INDEX IF NOT EXISTS bench_idx_{} ON customers(name)",
                i
            ));
            let _ = pg.execute(&format!("DROP INDEX IF EXISTS bench_idx_{}", i));
        });
        results.push(compare_safe("CREATE INDEX", h.as_ref(), &p));
    }

    // 3. ALTER TABLE
    {
        let h = bench_helios_safe(db, "ALTER TABLE", iters, |db, i| {
            db.execute(&format!(
                "ALTER TABLE bench_alt ADD COLUMN col_{} TEXT",
                i
            ))
            .map(|_| ())
            .map_err(|e| e.to_string())?;
            db.execute(&format!("ALTER TABLE bench_alt DROP COLUMN col_{}", i))
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "ALTER TABLE", iters, |pg, i| {
            let _ = pg.execute(&format!(
                "ALTER TABLE bench_alt ADD COLUMN col_{} TEXT",
                i
            ));
            let _ = pg.execute(&format!("ALTER TABLE bench_alt DROP COLUMN col_{}", i));
        });
        results.push(compare_safe("ALTER TABLE", h.as_ref(), &p));
    }

    // 4. DROP TABLE
    {
        for i in 0..(iters + 2) {
            let _ = db.execute(&format!(
                "CREATE TABLE IF NOT EXISTS bench_drop_{} (id INT)",
                i
            ));
            let _ = pg.execute(&format!(
                "CREATE TABLE IF NOT EXISTS bench_drop_{} (id INT)",
                i
            ));
        }
        let h = bench_helios_safe(db, "DROP TABLE", iters, |db, i| {
            db.execute(&format!("DROP TABLE IF EXISTS bench_drop_{}", i))
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "DROP TABLE", iters, |pg, i| {
            let _ = pg.execute(&format!("DROP TABLE IF EXISTS bench_drop_{}", i));
        });
        results.push(compare_safe("DROP TABLE", h.as_ref(), &p));
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
            let _ = pg.execute(&format!(
                "CREATE VIEW bench_v_{} AS SELECT id, name FROM customers WHERE age > 30",
                i
            ));
            let _ = pg.execute(&format!("DROP VIEW IF EXISTS bench_v_{}", i));
        });
        results.push(compare_safe("CREATE/DROP VIEW", h.as_ref(), &p));
    }

    // 6. REFRESH MATERIALIZED VIEW
    {
        let mv_ok = db
            .execute("CREATE MATERIALIZED VIEW IF NOT EXISTS bench_mv AS SELECT region, COUNT(*) as cnt FROM customers GROUP BY region")
            .is_ok();
        let _ = pg.execute("DROP MATERIALIZED VIEW IF EXISTS bench_mv");
        let _ = pg.execute("CREATE MATERIALIZED VIEW bench_mv AS SELECT region, COUNT(*) as cnt FROM customers GROUP BY region");

        let h = if mv_ok {
            bench_helios_safe(db, "REFRESH MATVIEW", iters, |db, _| {
                db.execute("REFRESH MATERIALIZED VIEW bench_mv")
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            })
        } else {
            None
        };
        let p = bench_pg(pg, "REFRESH MATVIEW", iters, |pg, _| {
            let _ = pg.execute("REFRESH MATERIALIZED VIEW bench_mv");
        });
        results.push(compare_safe("REFRESH MATVIEW", h.as_ref(), &p));
    }

    // 7. TRUNCATE
    {
        let _ =
            db.execute("CREATE TABLE IF NOT EXISTS bench_trunc (id INT, val TEXT)");
        let _ = pg.execute("CREATE TABLE IF NOT EXISTS bench_trunc (id INT, val TEXT)");

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
                let _ = pg.execute(&format!("INSERT INTO bench_trunc VALUES ({}, 'x')", j));
            }
            let _ = pg.execute("TRUNCATE bench_trunc");
        });
        results.push(compare_safe("TRUNCATE", h.as_ref(), &p));
    }

    // ============ DML ============

    // 8. INSERT single row
    {
        let _ = db.execute("CREATE TABLE IF NOT EXISTS bench_ins (id INT PRIMARY KEY, val TEXT)");
        let _ = pg.execute("DROP TABLE IF EXISTS bench_ins");
        let _ = pg.execute("CREATE TABLE bench_ins (id INT PRIMARY KEY, val TEXT)");

        let h = bench_helios(db, "INSERT single", iters, |db, i| {
            let _ = db.execute(&format!(
                "INSERT INTO bench_ins VALUES ({}, 'v{}')",
                10000 + i,
                i
            ));
        });
        let p = bench_pg(pg, "INSERT single", iters, |pg, i| {
            let _ = pg.execute(&format!(
                "INSERT INTO bench_ins VALUES ({}, 'v{}')",
                10000 + i,
                i
            ));
        });
        results.push(compare_result("INSERT single", &h, &p));
    }

    // 9. INSERT multi-row
    {
        let _ = db.execute("CREATE TABLE IF NOT EXISTS bench_ins_m (id INT PRIMARY KEY, val TEXT)");
        let _ = pg.execute("DROP TABLE IF EXISTS bench_ins_m");
        let _ = pg.execute("CREATE TABLE bench_ins_m (id INT PRIMARY KEY, val TEXT)");

        let h = bench_helios(db, "INSERT multi-row", iters, |db, i| {
            let base = 20000 + i * 10;
            let vals: Vec<String> = (0..10)
                .map(|j| format!("({}, 'b{}')", base + j, j))
                .collect();
            let _ = db.execute(&format!(
                "INSERT INTO bench_ins_m VALUES {}",
                vals.join(",")
            ));
        });
        let p = bench_pg(pg, "INSERT multi-row", iters, |pg, i| {
            let base = 20000 + i * 10;
            let vals: Vec<String> = (0..10)
                .map(|j| format!("({}, 'b{}')", base + j, j))
                .collect();
            let _ = pg.execute(&format!(
                "INSERT INTO bench_ins_m VALUES {}",
                vals.join(",")
            ));
        });
        results.push(compare_result("INSERT multi-row", &h, &p));
    }

    // 10. INSERT...SELECT
    {
        let _ = db.execute("CREATE TABLE IF NOT EXISTS bench_ins_s (id INT PRIMARY KEY, name TEXT)");
        let _ = pg.execute("DROP TABLE IF EXISTS bench_ins_s");
        let _ = pg.execute("CREATE TABLE bench_ins_s (id INT PRIMARY KEY, name TEXT)");

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
            let _ = pg.execute(&format!(
                "INSERT INTO bench_ins_s SELECT id + {}, name FROM customers WHERE id <= 10",
                off
            ));
        });
        results.push(compare_safe("INSERT..SELECT", h.as_ref(), &p));
    }

    // 11. UPDATE point
    {
        let h = bench_helios(db, "UPDATE point", iters, |db, i| {
            let id = (i % 200) + 1;
            let _ = db.execute(&format!(
                "UPDATE customers SET age = age + 0 WHERE id = {}",
                id
            ));
        });
        let p = bench_pg(pg, "UPDATE point", iters, |pg, i| {
            let id = (i % 200) + 1;
            let _ = pg.execute(&format!(
                "UPDATE customers SET age = age + 0 WHERE id = {}",
                id
            ));
        });
        results.push(compare_result("UPDATE point", &h, &p));
    }

    // 12. DELETE point
    {
        let _ = db.execute("CREATE TABLE IF NOT EXISTS bench_del (id INT PRIMARY KEY, val TEXT)");
        let _ = pg.execute("DROP TABLE IF EXISTS bench_del");
        let _ = pg.execute("CREATE TABLE bench_del (id INT PRIMARY KEY, val TEXT)");
        for i in 0..(iters + 2) {
            let _ = db.execute(&format!("INSERT INTO bench_del VALUES ({}, 'd{}')", i, i));
            let _ = pg.execute(&format!("INSERT INTO bench_del VALUES ({}, 'd{}')", i, i));
        }

        let h = bench_helios(db, "DELETE point", iters, |db, i| {
            let _ = db.execute(&format!("DELETE FROM bench_del WHERE id = {}", i));
        });
        let p = bench_pg(pg, "DELETE point", iters, |pg, i| {
            let _ = pg.execute(&format!("DELETE FROM bench_del WHERE id = {}", i));
        });
        results.push(compare_result("DELETE point", &h, &p));
    }

    // 13. UPSERT
    {
        let _ = db.execute(
            "CREATE TABLE IF NOT EXISTS bench_ups (id INT PRIMARY KEY, val TEXT, counter INT)",
        );
        let _ = pg.execute("DROP TABLE IF EXISTS bench_ups");
        let _ = pg.execute("CREATE TABLE bench_ups (id INT PRIMARY KEY, val TEXT, counter INT)");
        for i in 0..30 {
            let _ = db.execute(&format!("INSERT INTO bench_ups VALUES ({}, 'orig', 0)", i));
            let _ = pg.execute(&format!("INSERT INTO bench_ups VALUES ({}, 'orig', 0)", i));
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
            let _ = pg.execute(&format!(
                "INSERT INTO bench_ups VALUES ({}, 'new', 1) ON CONFLICT (id) DO UPDATE SET counter = bench_ups.counter + 1",
                id
            ));
        });
        results.push(compare_safe("UPSERT", h.as_ref(), &p));
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
            let _ = pg.execute(&format!(
                "UPDATE orders SET total = (SELECT COUNT(*) FROM order_items WHERE order_id = {}) WHERE order_id = {}",
                id, id
            ));
        });
        results.push(compare_safe("UPDATE+subquery", h.as_ref(), &p));
    }

    // ============ QUERIES ============

    // 15. Point lookup
    {
        let h = bench_helios(db, "Point lookup", iters, |db, i| {
            let id = (i % 200) + 1;
            let _ = db.query(
                &format!("SELECT * FROM customers WHERE id = {}", id),
                &[],
            );
        });
        let p = bench_pg(pg, "Point lookup", iters, |pg, i| {
            let id = (i % 200) + 1;
            let _ = pg.query_count(&format!("SELECT * FROM customers WHERE id = {}", id));
        });
        results.push(compare_result("Point lookup", &h, &p));
    }

    // 16. Full scan + filter
    {
        let h = bench_helios(db, "Full scan+filter", iters, |db, _| {
            let _ = db.query("SELECT * FROM customers WHERE age > 50", &[]);
        });
        let p = bench_pg(pg, "Full scan+filter", iters, |pg, _| {
            let _ = pg.query_count("SELECT * FROM customers WHERE age > 50");
        });
        results.push(compare_result("Full scan+filter", &h, &p));
    }

    // 17. Aggregation
    {
        let h = bench_helios(db, "Aggregation", iters, |db, _| {
            let _ = db.query(
                "SELECT region, COUNT(*), SUM(age), AVG(age) FROM customers GROUP BY region HAVING COUNT(*) > 10",
                &[],
            );
        });
        let p = bench_pg(pg, "Aggregation", iters, |pg, _| {
            let _ = pg.query_count(
                "SELECT region, COUNT(*), SUM(age), AVG(age) FROM customers GROUP BY region HAVING COUNT(*) > 10",
            );
        });
        results.push(compare_result("Aggregation", &h, &p));
    }

    // 18. INNER JOIN
    {
        let h = bench_helios(db, "INNER JOIN", iters, |db, i| {
            let id = (i % 200) + 1;
            let _ = db.query(
                &format!(
                    "SELECT c.name, o.order_id, o.total FROM customers c INNER JOIN orders o ON c.id = o.customer_id WHERE c.id = {}",
                    id
                ),
                &[],
            );
        });
        let p = bench_pg(pg, "INNER JOIN", iters, |pg, i| {
            let id = (i % 200) + 1;
            let _ = pg.query_count(&format!(
                "SELECT c.name, o.order_id, o.total FROM customers c INNER JOIN orders o ON c.id = o.customer_id WHERE c.id = {}",
                id
            ));
        });
        results.push(compare_result("INNER JOIN", &h, &p));
    }

    // 19. LEFT JOIN
    {
        let h = bench_helios(db, "LEFT JOIN", iters, |db, i| {
            let id = (i % 200) + 1;
            let _ = db.query(
                &format!(
                    "SELECT c.name, o.order_id FROM customers c LEFT JOIN orders o ON c.id = o.customer_id WHERE c.id = {}",
                    id
                ),
                &[],
            );
        });
        let p = bench_pg(pg, "LEFT JOIN", iters, |pg, i| {
            let id = (i % 200) + 1;
            let _ = pg.query_count(&format!(
                "SELECT c.name, o.order_id FROM customers c LEFT JOIN orders o ON c.id = o.customer_id WHERE c.id = {}",
                id
            ));
        });
        results.push(compare_result("LEFT JOIN", &h, &p));
    }

    // 20. 4-table JOIN
    {
        let h = bench_helios(db, "4-table JOIN", iters, |db, i| {
            let id = (i % 200) + 1;
            let _ = db.query(
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
            );
        });
        let p = bench_pg(pg, "4-table JOIN", iters, |pg, i| {
            let id = (i % 200) + 1;
            let _ = pg.query_count(&format!(
                "SELECT c.name, o.order_id, oi.quantity, p.name \
                 FROM customers c \
                 INNER JOIN orders o ON c.id = o.customer_id \
                 INNER JOIN order_items oi ON o.order_id = oi.order_id \
                 INNER JOIN products p ON oi.product_id = p.product_id \
                 WHERE c.id = {}",
                id
            ));
        });
        results.push(compare_result("4-table JOIN", &h, &p));
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
            let _ = pg.query_count(
                "SELECT name, (SELECT COUNT(*) FROM orders WHERE customer_id = customers.id) AS order_count FROM customers WHERE id <= 20",
            );
        });
        results.push(compare_safe("Scalar subquery", h.as_ref(), &p));
    }

    // 22. EXISTS subquery
    {
        let h = bench_helios_safe(db, "EXISTS subquery", iters, |db, _| {
            db.query(
                "SELECT name FROM customers WHERE EXISTS (SELECT 1 FROM orders WHERE customer_id = customers.id) AND id <= 50",
                &[],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "EXISTS subquery", iters, |pg, _| {
            let _ = pg.query_count(
                "SELECT name FROM customers WHERE EXISTS (SELECT 1 FROM orders WHERE customer_id = customers.id) AND id <= 50",
            );
        });
        results.push(compare_safe("EXISTS subquery", h.as_ref(), &p));
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
            let _ = pg.query_count(
                "SELECT name FROM customers WHERE id IN (SELECT customer_id FROM orders WHERE total > 500) AND id <= 100",
            );
        });
        results.push(compare_safe("IN subquery", h.as_ref(), &p));
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
            let _ = pg.query_count(
                "WITH high_value AS (SELECT customer_id, SUM(total) as total_spent FROM orders GROUP BY customer_id HAVING SUM(total) > 500) \
                 SELECT c.name, hv.total_spent FROM customers c JOIN high_value hv ON c.id = hv.customer_id",
            );
        });
        results.push(compare_safe("CTE", h.as_ref(), &p));
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
            let _ = pg.query_count(
                "WITH RECURSIVE cat_tree AS (\
                    SELECT cat_id, name, parent_id, 0 AS depth FROM categories WHERE parent_id IS NULL \
                    UNION ALL \
                    SELECT c.cat_id, c.name, c.parent_id, ct.depth + 1 FROM categories c JOIN cat_tree ct ON c.parent_id = ct.cat_id\
                 ) SELECT * FROM cat_tree",
            );
        });
        results.push(compare_safe("Recursive CTE", h.as_ref(), &p));
    }

    // 26. Window functions
    {
        let h = bench_helios_safe(db, "Window funcs", iters, |db, _| {
            db.query(
                "SELECT name, age, region, \
                    ROW_NUMBER() OVER (PARTITION BY region ORDER BY age DESC) as rn, \
                    RANK() OVER (PARTITION BY region ORDER BY age DESC) as rnk, \
                    SUM(age) OVER (PARTITION BY region) as region_total \
                 FROM customers WHERE id <= 100",
                &[],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "Window funcs", iters, |pg, _| {
            let _ = pg.query_count(
                "SELECT name, age, region, \
                    ROW_NUMBER() OVER (PARTITION BY region ORDER BY age DESC) as rn, \
                    RANK() OVER (PARTITION BY region ORDER BY age DESC) as rnk, \
                    SUM(age) OVER (PARTITION BY region) as region_total \
                 FROM customers WHERE id <= 100",
            );
        });
        results.push(compare_safe("Window funcs", h.as_ref(), &p));
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
            let _ = pg.query_count(
                "SELECT name FROM customers WHERE age < 30 UNION SELECT name FROM customers WHERE region = 'East'",
            );
        });
        results.push(compare_safe("UNION", h.as_ref(), &p));
    }

    // 28. DISTINCT
    {
        let h = bench_helios(db, "DISTINCT", iters, |db, _| {
            let _ = db.query("SELECT DISTINCT region FROM customers", &[]);
        });
        let p = bench_pg(pg, "DISTINCT", iters, |pg, _| {
            let _ = pg.query_count("SELECT DISTINCT region FROM customers");
        });
        results.push(compare_result("DISTINCT", &h, &p));
    }

    // 29. ORDER BY + LIMIT
    {
        let h = bench_helios(db, "ORDER+LIMIT", iters, |db, i| {
            let off = (i % 50) * 10;
            let _ = db.query(
                &format!(
                    "SELECT * FROM customers ORDER BY age DESC, name ASC LIMIT 20 OFFSET {}",
                    off
                ),
                &[],
            );
        });
        let p = bench_pg(pg, "ORDER+LIMIT", iters, |pg, i| {
            let off = (i % 50) * 10;
            let _ = pg.query_count(&format!(
                "SELECT * FROM customers ORDER BY age DESC, name ASC LIMIT 20 OFFSET {}",
                off
            ));
        });
        results.push(compare_result("ORDER+LIMIT", &h, &p));
    }

    // 30. CASE expressions
    {
        let h = bench_helios(db, "CASE expr", iters, |db, _| {
            let _ = db.query(
                "SELECT name, \
                    CASE WHEN age < 25 THEN 'young' WHEN age < 50 THEN 'middle' ELSE 'senior' END AS age_group \
                 FROM customers WHERE id <= 100",
                &[],
            );
        });
        let p = bench_pg(pg, "CASE expr", iters, |pg, _| {
            let _ = pg.query_count(
                "SELECT name, \
                    CASE WHEN age < 25 THEN 'young' WHEN age < 50 THEN 'middle' ELSE 'senior' END AS age_group \
                 FROM customers WHERE id <= 100",
            );
        });
        results.push(compare_result("CASE expr", &h, &p));
    }

    // 31. LIKE / BETWEEN / IN list
    {
        let h = bench_helios(db, "LIKE/BETWEEN/IN", iters, |db, _| {
            let _ = db.query(
                "SELECT * FROM customers WHERE name LIKE 'Customer_1%' AND age BETWEEN 20 AND 50 AND region IN ('East', 'West', 'North')",
                &[],
            );
        });
        let p = bench_pg(pg, "LIKE/BETWEEN/IN", iters, |pg, _| {
            let _ = pg.query_count(
                "SELECT * FROM customers WHERE name LIKE 'Customer_1%' AND age BETWEEN 20 AND 50 AND region IN ('East', 'West', 'North')",
            );
        });
        results.push(compare_result("LIKE/BETWEEN/IN", &h, &p));
    }

    // 32. String ops
    {
        let h = bench_helios(db, "String ops", iters, |db, _| {
            let _ = db.query(
                "SELECT id, name, LENGTH(bio) as bio_len FROM customers WHERE bio IS NOT NULL AND id <= 100",
                &[],
            );
        });
        let p = bench_pg(pg, "String ops", iters, |pg, _| {
            let _ = pg.query_count(
                "SELECT id, name, LENGTH(bio) as bio_len FROM customers WHERE bio IS NOT NULL AND id <= 100",
            );
        });
        results.push(compare_result("String ops", &h, &p));
    }

    // ============ CONTROL ============

    // 33. Transaction control
    {
        let h = bench_helios_safe(db, "Transaction ctl", iters, |db, _| {
            db.execute("BEGIN").map(|_| ()).map_err(|e| e.to_string())?;
            db.execute("SAVEPOINT sp1")
                .map(|_| ())
                .map_err(|e| e.to_string())?;
            db.execute("INSERT INTO bench_ins VALUES (99999, 'txn')")
                .map(|_| ())
                .map_err(|e| e.to_string())?;
            db.execute("ROLLBACK TO SAVEPOINT sp1")
                .map(|_| ())
                .map_err(|e| e.to_string())?;
            db.execute("COMMIT")
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "Transaction ctl", iters, |pg, _| {
            let _ = pg.execute("BEGIN");
            let _ = pg.execute("SAVEPOINT sp1");
            let _ = pg.execute("INSERT INTO bench_ins VALUES (99999, 'txn')");
            let _ = pg.execute("ROLLBACK TO SAVEPOINT sp1");
            let _ = pg.execute("COMMIT");
        });
        results.push(compare_safe("Transaction ctl", h.as_ref(), &p));
    }

    // 34. PREPARE / EXECUTE / DEALLOCATE
    {
        let h = bench_helios_safe(db, "Prepared stmts", iters, |db, i| {
            let name = format!("bench_s_{}", i);
            db.query(
                &format!(
                    "PREPARE {} AS SELECT * FROM customers WHERE id = $1",
                    name
                ),
                &[],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())?;
            db.query(&format!("EXECUTE {}(42)", name), &[])
                .map(|_| ())
                .map_err(|e| e.to_string())?;
            db.query(&format!("DEALLOCATE {}", name), &[])
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "Prepared stmts", iters, |pg, i| {
            let name = format!("bench_s_{}", i);
            let _ = pg.execute(&format!(
                "PREPARE {} (int) AS SELECT * FROM customers WHERE id = $1",
                name
            ));
            let _ = pg.execute(&format!("EXECUTE {} (42)", name));
            let _ = pg.execute(&format!("DEALLOCATE {}", name));
        });
        results.push(compare_safe("Prepared stmts", h.as_ref(), &p));
    }

    // 35. SET/SHOW/RESET
    {
        let h = bench_helios_safe(db, "SET/SHOW/RESET", iters, |db, _| {
            db.query("SET work_mem = '8192'", &[])
                .map(|_| ())
                .map_err(|e| e.to_string())?;
            db.query("SHOW work_mem", &[])
                .map(|_| ())
                .map_err(|e| e.to_string())?;
            db.query("RESET work_mem", &[])
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        let p = bench_pg(pg, "SET/SHOW/RESET", iters, |pg, _| {
            let _ = pg.execute("SET work_mem = '8MB'");
            let _ = pg.execute("SHOW work_mem");
            let _ = pg.execute("RESET work_mem");
        });
        results.push(compare_safe("SET/SHOW/RESET", h.as_ref(), &p));
    }

    results
}

fn compare_result(name: &str, h: &CategoryResult, p: &CategoryResult) -> ComparisonRow {
    let h_us = h.avg_per_iter.as_micros() as f64;
    let p_us = p.avg_per_iter.as_micros() as f64;
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
    }
}

fn compare_safe(name: &str, h: Option<&CategoryResult>, p: &CategoryResult) -> ComparisonRow {
    match h {
        Some(h) => compare_result(name, h, p),
        None => {
            let p_us = p.avg_per_iter.as_micros() as f64;
            ComparisonRow {
                name: name.to_string(),
                helios_avg_us: 0.0,
                pg_avg_us: p_us,
                ratio: 0.0,
                winner: "N/A".to_string(),
                helios_na: true,
            }
        }
    }
}

// --- Output formatting ---

fn print_comparison_table(results: &[ComparisonRow]) {
    println!("\n{}", "=".repeat(105));
    println!(
        "  HELIOSDB-NANO vs POSTGRESQL 16 -- HEAD-TO-HEAD COMPARISON"
    );
    println!(
        "  Dataset: 200 customers, 50 products, 500 orders, 1000 items, 20 categories"
    );
    println!("{}\n", "=".repeat(105));

    println!(
        "{:<22} | {:>12} | {:>12} | {:>10} | {:>8}",
        "Category", "Nano(avg)", "PG 16(avg)", "Ratio", "Winner"
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

    let mut sorted: Vec<&ComparisonRow> = results
        .iter()
        .filter(|r| !r.helios_na && r.ratio > 1.05)
        .collect();
    sorted.sort_by(|a, b| {
        b.ratio
            .partial_cmp(&a.ratio)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

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
    helios_better.sort_by(|a, b| {
        a.ratio
            .partial_cmp(&b.ratio)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

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
    let significant: Vec<&&ComparisonRow> = active
        .iter()
        .filter(|r| r.ratio > 2.0 && r.ratio <= 5.0)
        .collect();
    let moderate: Vec<&&ComparisonRow> = active
        .iter()
        .filter(|r| r.ratio > 1.05 && r.ratio <= 2.0)
        .collect();
    let competitive: Vec<&&ComparisonRow> = active
        .iter()
        .filter(|r| r.ratio >= 0.95 && r.ratio <= 1.05)
        .collect();
    let helios_faster: Vec<&&ComparisonRow> = active
        .iter()
        .filter(|r| r.ratio > 0.0 && r.ratio < 0.95)
        .collect();
    let na_list: Vec<&ComparisonRow> = results.iter().filter(|r| r.helios_na).collect();

    println!("    Critical (>5x slower):    {} categories", critical.len());
    println!(
        "    Significant (2-5x):       {} categories",
        significant.len()
    );
    println!("    Moderate (1.05-2x):       {} categories", moderate.len());
    println!(
        "    Competitive (~1x):        {} categories",
        competitive.len()
    );
    println!(
        "    Nano faster (<0.95x):     {} categories",
        helios_faster.len()
    );
    println!(
        "    Not supported (N/A):      {} categories",
        na_list.len()
    );

    if !na_list.is_empty() {
        println!("\n  N/A CATEGORIES (not supported in Nano):");
        for r in &na_list {
            println!("    - {}", r.name);
        }
    }
}

// --- Main test ---

#[test]
#[ignore] // Requires Docker PostgreSQL on port 25433
fn pg35_benchmark() {
    println!("\n{}", "=".repeat(105));
    println!(
        "  HELIOSDB-NANO vs POSTGRESQL 16 -- COMPREHENSIVE 35-CATEGORY BENCHMARK"
    );
    println!(
        "  HeliosDB-Nano v3.7.0 (Embedded/In-Memory) vs PostgreSQL 16 (Docker/25433)"
    );
    println!("{}\n", "=".repeat(105));

    // Connect to PostgreSQL
    let pg = match PgClient::connect(
        "host=127.0.0.1 port=25433 user=bench password=benchpass dbname=benchdb",
    ) {
        Ok(pg) => {
            println!("  [OK] Connected to PostgreSQL 16 on port 25433");
            pg
        }
        Err(e) => {
            println!("  [SKIP] Cannot connect to PostgreSQL: {}", e);
            println!("  Start with: docker run -d --name pg_bench_nano -e POSTGRES_USER=bench -e POSTGRES_PASSWORD=benchpass -e POSTGRES_DB=benchdb -p 25433:5432 postgres:16-alpine");
            return;
        }
    };

    // Set up HeliosDB-Nano
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create HeliosDB-Nano");
    println!("  [OK] HeliosDB-Nano in-memory instance created");

    // Schema + Data
    println!("\n  Setting up schemas...");
    setup_helios_schema(&db);
    setup_pg_schema(&pg);
    println!("  [OK] Schema created in both engines");

    println!("  Populating data (200+50+500+1000+20 = 1770 rows)...");
    populate_helios(&db);
    populate_pg(&pg);
    println!("  [OK] Data populated in both engines");

    // Verify row counts
    let h_count = db
        .query("SELECT COUNT(*) FROM customers", &[])
        .map(|r| {
            if r.is_empty() {
                "0".to_string()
            } else {
                format!("{:?}", r[0].values[0])
            }
        })
        .unwrap_or_else(|_| "err".to_string());
    let p_count = pg.query_count("SELECT COUNT(*) FROM customers").unwrap_or(0);
    println!("  [OK] Customers: Nano={} PG={}", h_count, p_count);

    // Run benchmarks
    let iters = 20;
    println!(
        "\n  Running 35 categories x {} iterations each x 2 engines...\n",
        iters
    );

    let start = Instant::now();
    let results = run_all_categories(&db, &pg, iters);
    let total_time = start.elapsed();

    // Output
    print_comparison_table(&results);
    print_trace_breakdown(&results);
    print_analysis(&results);

    println!("\n{}", "=".repeat(105));
    println!("  BENCHMARK COMPLETE -- Total time: {:.1?}", total_time);
    println!("{}\n", "=".repeat(105));
}
