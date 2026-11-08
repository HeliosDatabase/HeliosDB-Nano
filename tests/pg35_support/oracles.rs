use super::cells::{self, both, int, text, Cell, Rows};
use super::model::{self, Model, REGIONS};
use super::{RunContext, CATEGORIES, EXISTS_QUERY};
use crate::PgClient;
use heliosdb_nano::EmbeddedDatabase;
use serde_json::{json, Value};

fn seed(db: &EmbeddedDatabase, pg: &PgClient, model: &Model) -> u64 {
    ["customers", "products", "orders", "order_items", "categories"]
        .into_iter()
        .map(|name| both(db, pg, &format!("SELECT * FROM {name}"), model.table(name), false))
        .sum()
}

pub fn verify_seed_data(db: &EmbeddedDatabase, pg: &PgClient) -> Value {
    seed(db, pg, &Model::new(false));
    let counts = json!({"customers":200,"products":50,"orders":500,"order_items":1000,"categories":20});
    for (table, count) in counts.as_object().unwrap() {
        both(
            db,
            pg,
            &format!("SELECT COUNT(*) FROM {table}"),
            vec![vec![int(count.as_i64().unwrap())]],
            true,
        );
    }
    json!({"nano":counts,"postgres":counts})
}

fn nx(db: &EmbeddedDatabase, sql: &str) {
    db.execute(sql)
        .unwrap_or_else(|e| panic!("Nano semantic control {sql}: {e}"));
}
fn px(pg: &PgClient, sql: &str) {
    pg.execute(sql)
        .unwrap_or_else(|e| panic!("PG semantic control {sql}: {e}"));
}
fn x(db: &EmbeddedDatabase, pg: &PgClient, sql: &str) {
    nx(db, sql);
    px(pg, sql);
}

fn absent_table(db: &EmbeddedDatabase, pg: &PgClient, name: &str) -> u64 {
    assert!(
        !db.storage.catalog().table_exists(name).unwrap(),
        "Nano {name} survived DROP"
    );
    cells::assert_rows(
        cells::pg(
            pg,
            &format!("SELECT COUNT(*) FROM pg_class WHERE relnamespace='public'::regnamespace AND relname='{name}'"),
        ),
        vec![vec![int(0)]],
        true,
        name,
    );
    2
}
fn absent_view(db: &EmbeddedDatabase, pg: &PgClient, name: &str) -> u64 {
    let error = db
        .query(&format!("SELECT * FROM {name}"), &[])
        .expect_err("Nano dropped view must not resolve");
    missing_nano_object(error, name);
    let error = pg
        .rt
        .block_on(pg.client.query(&format!("SELECT * FROM {name}"), &[]))
        .expect_err("PG dropped view must not resolve");
    assert_eq!(error.as_db_error().unwrap().code().code(), "42P01");
    2
}

fn missing_nano_object(error: heliosdb_nano::Error, name: &str) {
    let message = error.to_string().to_ascii_lowercase();
    assert!(
        matches!(error, heliosdb_nano::Error::QueryExecution(_)),
        "expected missing-object error, got {error}"
    );
    assert!(
        message.contains(&name.to_ascii_lowercase())
            && (message.contains("does not exist") || message.contains("not found")),
        "unexpected negative-control error: {error}"
    );
}
fn absent_index(db: &EmbeddedDatabase, pg: &PgClient, name: &str) -> u64 {
    assert!(
        !db.storage.art_indexes().index_exists(name),
        "Nano {name} survived DROP"
    );
    cells::assert_rows(
        cells::pg(
            pg,
            &format!("SELECT COUNT(*) FROM pg_indexes WHERE schemaname='public' AND indexname='{name}'"),
        ),
        vec![vec![int(0)]],
        true,
        name,
    );
    2
}

fn work_mem_kib(rows: &Rows) -> u64 {
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 1);
    let raw = match &rows[0][0] {
        Cell::Text(v) => v.clone(),
        Cell::Int(v) => v.to_string(),
        other => panic!("work_mem result {other:?}"),
    };
    let raw = raw.trim().to_ascii_lowercase();
    for (suffix, multiplier) in [("gb", 1024 * 1024), ("mb", 1024), ("kb", 1), ("", 1)] {
        if let Some(number) = raw.strip_suffix(suffix) {
            if let Ok(value) = number.trim().parse::<u64>() {
                return value.checked_mul(multiplier).unwrap();
            }
        }
    }
    panic!("unrecognized work_mem unit {raw}");
}

fn prepared_expected() -> Rows {
    vec![vec![
        int(42),
        text("Customer_42"),
        text("c42@test.com"),
        int(60),
        text("North"),
        text("{\"tier\":\"gold\"}"),
        text("Bio for customer 42"),
    ]]
}
pub fn timed_execute_nano(db: &EmbeddedDatabase, sql: &str) {
    cells::assert_rows(cells::nano(db, sql), prepared_expected(), true, "timed Nano EXECUTE");
}
pub fn timed_execute_pg(pg: &PgClient, sql: &str) {
    cells::assert_rows(cells::pg(pg, sql), prepared_expected(), true, "timed PG EXECUTE");
}
pub fn timed_show_nano(db: &EmbeddedDatabase) {
    assert_eq!(work_mem_kib(&cells::nano(db, "SHOW work_mem")), 8192);
}
pub fn timed_show_pg(pg: &PgClient) {
    assert_eq!(work_mem_kib(&cells::pg(pg, "SHOW work_mem")), 8192);
}

pub fn verify_category(db: &EmbeddedDatabase, pg: &PgClient, context: &RunContext, name: &str, iters: usize) -> u64 {
    assert_eq!(iters, 300);
    let position = CATEGORIES
        .iter()
        .position(|category| *category == name)
        .expect("known category");
    let model = Model::new(position >= 13);
    let mut checks = seed(db, pg, &model);
    match name {
        "CREATE TABLE" => {
            for i in 0..302 {
                checks += absent_table(db, pg, &format!("bench_ct_{i}"));
            }
            x(db, pg, "CREATE TABLE strict_probe_ct(id INT PRIMARY KEY,val TEXT)");
            assert_eq!(
                db.execute("INSERT INTO strict_probe_ct VALUES(7,'created')").unwrap(),
                1
            );
            assert_eq!(
                pg.execute("INSERT INTO strict_probe_ct VALUES(7,'created')").unwrap(),
                1
            );
            checks += both(
                db,
                pg,
                "SELECT * FROM strict_probe_ct",
                vec![vec![int(7), text("created")]],
                true,
            );
            x(db, pg, "DROP TABLE strict_probe_ct");
            checks += absent_table(db, pg, "strict_probe_ct");
        }
        "CREATE INDEX" => {
            for i in 0..302 {
                checks += absent_index(db, pg, &format!("bench_idx_{i}"));
            }
            x(db, pg, "CREATE INDEX strict_probe_index ON customers(name)");
            assert!(db.storage.art_indexes().index_exists("strict_probe_index"));
            cells::assert_rows(
                cells::pg(
                    pg,
                    "SELECT COUNT(*) FROM pg_indexes WHERE schemaname='public' AND indexname='strict_probe_index'",
                ),
                vec![vec![int(1)]],
                true,
                "created index",
            );
            checks += 2;
            checks += both(
                db,
                pg,
                "SELECT id FROM customers WHERE name='Customer_42'",
                vec![vec![int(42)]],
                true,
            );
            x(db, pg, "DROP INDEX strict_probe_index");
            checks += absent_index(db, pg, "strict_probe_index");
        }
        "ALTER TABLE" => {
            let schema = db.storage.catalog().get_table_schema("bench_alt").unwrap();
            assert_eq!(
                schema.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
                ["id", "base"]
            );
            cells::assert_rows(cells::pg(pg,"SELECT column_name::text FROM information_schema.columns WHERE table_schema='public' AND table_name='bench_alt' ORDER BY ordinal_position"),vec![vec![text("id")],vec![text("base")]],true,"ALTER final columns");
            checks += 2;
            x(db, pg, "INSERT INTO bench_alt VALUES(1,'base')");
            x(db, pg, "ALTER TABLE bench_alt ADD COLUMN strict_added TEXT");
            checks += both(
                db,
                pg,
                "SELECT * FROM bench_alt",
                vec![vec![int(1), text("base"), Cell::Null]],
                true,
            );
            x(db, pg, "ALTER TABLE bench_alt DROP COLUMN strict_added");
            checks += both(
                db,
                pg,
                "SELECT * FROM bench_alt",
                vec![vec![int(1), text("base")]],
                true,
            );
            assert_eq!(db.execute("DELETE FROM bench_alt WHERE id=1").unwrap(), 1);
            assert_eq!(pg.execute("DELETE FROM bench_alt WHERE id=1").unwrap(), 1);
        }
        "DROP TABLE" => {
            for i in 0..302 {
                checks += absent_table(db, pg, &format!("bench_drop_{i}"));
            }
        }
        "CREATE/DROP VIEW" => {
            for i in 0..302 {
                checks += absent_view(db, pg, &format!("bench_v_{i}"));
            }
            x(
                db,
                pg,
                "CREATE VIEW strict_probe_view AS SELECT id,name FROM customers WHERE age>30",
            );
            checks += both(
                db,
                pg,
                "SELECT * FROM strict_probe_view",
                model
                    .customers
                    .iter()
                    .filter(|c| c.age > 30)
                    .map(|c| vec![int(c.id), text(&c.name)])
                    .collect(),
                false,
            );
            x(db, pg, "DROP VIEW strict_probe_view");
            checks += absent_view(db, pg, "strict_probe_view");
        }
        "REFRESH MATVIEW" => {
            checks += both(
                db,
                pg,
                "SELECT region,cnt FROM bench_mv",
                REGIONS.iter().map(|r| vec![text(*r), int(40)]).collect(),
                false,
            );
        }
        "TRUNCATE" => {
            checks += both(db, pg, "SELECT * FROM bench_trunc", vec![], false);
            for i in 0..5 {
                x(db, pg, &format!("INSERT INTO bench_trunc VALUES({i},'x')"));
            }
            checks += both(
                db,
                pg,
                "SELECT * FROM bench_trunc",
                (0..5).map(|i| vec![int(i), text("x")]).collect(),
                false,
            );
            x(db, pg, "TRUNCATE bench_trunc");
            checks += both(db, pg, "SELECT * FROM bench_trunc", vec![], false);
        }
        "INSERT single" => {
            checks += both(db, pg, "SELECT * FROM bench_ins", model::inserted_single(), false);
        }
        "INSERT multi-row" => {
            checks += both(db, pg, "SELECT * FROM bench_ins_m", model::inserted_multi(), false);
        }
        "INSERT..SELECT" => {
            checks += both(db, pg, "SELECT * FROM bench_ins_s", model::inserted_select(), false);
        }
        "UPDATE point" => {
            checks += both(db, pg, "SELECT * FROM customers", model.table("customers"), false);
        }
        "DELETE point" => {
            checks += both(db, pg, "SELECT * FROM bench_del", vec![], false);
        }
        "UPSERT" => {
            checks += both(db, pg, "SELECT * FROM bench_ups", model::upserted(), false);
            checks += both(db, pg, "SELECT SUM(counter) FROM bench_ups", vec![vec![int(302)]], true);
        }
        "UPDATE+subquery" => {
            checks += both(db, pg, "SELECT * FROM orders", model.table("orders"), false);
        }
        "Point lookup" => {
            for c in &model.customers {
                checks += both(
                    db,
                    pg,
                    &format!("SELECT * FROM customers WHERE id={}", c.id),
                    vec![c.cells()],
                    true,
                );
            }
        }
        "Full scan+filter" => {
            checks += both(
                db,
                pg,
                "SELECT * FROM customers WHERE age>50",
                model
                    .customers
                    .iter()
                    .filter(|c| c.age > 50)
                    .map(|c| c.cells())
                    .collect(),
                false,
            );
        }
        "Aggregation" => {
            let rows = REGIONS
                .iter()
                .map(|region| {
                    let ages: Vec<_> = model
                        .customers
                        .iter()
                        .filter(|c| c.region == *region)
                        .map(|c| c.age)
                        .collect();
                    let sum: i64 = ages.iter().sum();
                    vec![
                        text(*region),
                        int(ages.len() as i64),
                        int(sum),
                        Cell::Decimal(sum as f64 / ages.len() as f64),
                    ]
                })
                .collect();
            checks += both(
                db,
                pg,
                "SELECT region,COUNT(*),SUM(age),AVG(age) FROM customers GROUP BY region HAVING COUNT(*)>10",
                rows,
                false,
            );
        }
        "INNER JOIN" | "LEFT JOIN" | "4-table JOIN" => {
            for c in &model.customers {
                let mut rows = Vec::new();
                for order in model.orders.iter().filter(|o| o.customer == c.id) {
                    match name {
                        "INNER JOIN" => rows.push(vec![text(&c.name), int(order.id), int(order.total)]),
                        "LEFT JOIN" => rows.push(vec![text(&c.name), int(order.id)]),
                        _ => {
                            for item in model.items.iter().filter(|item| item.order == order.id) {
                                let product = model.products.iter().find(|p| p.id == item.product).unwrap();
                                rows.push(vec![
                                    text(&c.name),
                                    int(order.id),
                                    int(item.quantity),
                                    text(&product.name),
                                ]);
                            }
                        }
                    }
                }
                if name == "LEFT JOIN" && rows.is_empty() {
                    rows.push(vec![text(&c.name), Cell::Null]);
                }
                let sql=match name {
                    "INNER JOIN"=>format!("SELECT c.name,o.order_id,o.total FROM customers c INNER JOIN orders o ON c.id=o.customer_id WHERE c.id={}",c.id),
                    "LEFT JOIN"=>format!("SELECT c.name,o.order_id FROM customers c LEFT JOIN orders o ON c.id=o.customer_id WHERE c.id={}",c.id),
                    _=>format!("SELECT c.name,o.order_id,oi.quantity,p.name FROM customers c INNER JOIN orders o ON c.id=o.customer_id INNER JOIN order_items oi ON o.order_id=oi.order_id INNER JOIN products p ON oi.product_id=p.product_id WHERE c.id={}",c.id),
                };
                checks += both(db, pg, &sql, rows, false);
            }
        }
        "Scalar subquery" => {
            let rows = model
                .customers
                .iter()
                .filter(|c| c.id <= 20)
                .map(|c| {
                    vec![
                        text(&c.name),
                        int(model.orders.iter().filter(|o| o.customer == c.id).count() as i64),
                    ]
                })
                .collect();
            checks+=both(db,pg,"SELECT name,(SELECT COUNT(*) FROM orders WHERE customer_id=customers.id) AS order_count FROM customers WHERE id<=20",rows,false);
        }
        "EXISTS subquery" => {
            let rows = model
                .customers
                .iter()
                .filter(|c| c.id <= 50 && model.orders.iter().any(|o| o.customer == c.id))
                .map(|c| vec![text(&c.name)])
                .collect();
            checks += both(db, pg, EXISTS_QUERY, rows, false);
        }
        "IN subquery" => {
            let rows = model
                .customers
                .iter()
                .filter(|c| c.id <= 100 && model.orders.iter().any(|o| o.customer == c.id && o.total > 500))
                .map(|c| vec![text(&c.name)])
                .collect();
            checks += both(
                db,
                pg,
                "SELECT name FROM customers WHERE id IN(SELECT customer_id FROM orders WHERE total>500) AND id<=100",
                rows,
                false,
            );
        }
        "CTE" => {
            let rows = model
                .customers
                .iter()
                .filter_map(|c| {
                    let total: i64 = model
                        .orders
                        .iter()
                        .filter(|o| o.customer == c.id)
                        .map(|o| o.total)
                        .sum();
                    (total > 500).then(|| vec![text(&c.name), int(total)])
                })
                .collect();
            checks+=both(db,pg,"WITH high_value AS(SELECT customer_id,SUM(total) AS total_spent FROM orders GROUP BY customer_id HAVING SUM(total)>500) SELECT c.name,hv.total_spent FROM customers c JOIN high_value hv ON c.id=hv.customer_id",rows,false);
        }
        "Recursive CTE" => {
            let rows = Model::categories()
                .into_iter()
                .map(|mut row| {
                    row.push(int(if row[2] == Cell::Null { 0 } else { 1 }));
                    row
                })
                .collect();
            checks+=both(db,pg,"WITH RECURSIVE cat_tree AS(SELECT cat_id,name,parent_id,0 AS depth FROM categories WHERE parent_id IS NULL UNION ALL SELECT c.cat_id,c.name,c.parent_id,ct.depth+1 FROM categories c JOIN cat_tree ct ON c.parent_id=ct.cat_id) SELECT * FROM cat_tree",rows,false);
        }
        "Window funcs" => {
            let mut rows = Vec::new();
            for region in REGIONS {
                let mut customers: Vec<_> = model
                    .customers
                    .iter()
                    .filter(|c| c.id <= 100 && c.region == region)
                    .collect();
                customers.sort_by(|a, b| b.age.cmp(&a.age).then(a.id.cmp(&b.id)));
                let total: i64 = customers.iter().map(|c| c.age).sum();
                for (index, c) in customers.iter().enumerate() {
                    let rank = customers.iter().filter(|other| other.age > c.age).count() + 1;
                    rows.push(vec![
                        text(&c.name),
                        int(c.age),
                        text(&c.region),
                        int(index as i64 + 1),
                        int(rank as i64),
                        int(total),
                    ]);
                }
            }
            checks+=both(db,pg,"SELECT name,age,region,ROW_NUMBER() OVER(PARTITION BY region ORDER BY age DESC,id ASC) AS rn,RANK() OVER(PARTITION BY region ORDER BY age DESC) AS rnk,SUM(age) OVER(PARTITION BY region) AS region_total FROM customers WHERE id<=100",rows,false);
        }
        "UNION" => {
            checks += both(
                db,
                pg,
                "SELECT name FROM customers WHERE age<30 UNION SELECT name FROM customers WHERE region='East'",
                model
                    .customers
                    .iter()
                    .filter(|c| c.age < 30 || c.region == "East")
                    .map(|c| vec![text(&c.name)])
                    .collect(),
                false,
            );
        }
        "DISTINCT" => {
            checks += both(
                db,
                pg,
                "SELECT DISTINCT region FROM customers",
                REGIONS.iter().map(|r| vec![text(*r)]).collect(),
                false,
            );
        }
        "ORDER+LIMIT" => {
            let mut sorted = model.customers.clone();
            sorted.sort_by(|a, b| b.age.cmp(&a.age).then(a.name.cmp(&b.name)));
            for offset in (0..500).step_by(10) {
                checks += both(
                    db,
                    pg,
                    &format!("SELECT * FROM customers ORDER BY age DESC,name ASC LIMIT 20 OFFSET {offset}"),
                    sorted.iter().skip(offset).take(20).map(|c| c.cells()).collect(),
                    true,
                );
            }
        }
        "CASE expr" => {
            checks+=both(db,pg,"SELECT name,CASE WHEN age<25 THEN 'young' WHEN age<50 THEN 'middle' ELSE 'senior' END AS age_group FROM customers WHERE id<=100",model.customers.iter().filter(|c|c.id<=100).map(|c|vec![text(&c.name),text(if c.age<25 {"young"}else if c.age<50 {"middle"}else{"senior"})]).collect(),false);
        }
        "LIKE/BETWEEN/IN" => {
            checks+=both(db,pg,"SELECT * FROM customers WHERE name LIKE 'Customer_1%' AND age BETWEEN 20 AND 50 AND region IN('East','West','North')",model.customers.iter().filter(|c|c.name.starts_with("Customer_1")&&(20..=50).contains(&c.age)&&["East","West","North"].contains(&c.region.as_str())).map(|c|c.cells()).collect(),false);
        }
        "String ops" => {
            checks += both(
                db,
                pg,
                "SELECT id,name,LENGTH(bio) AS bio_len FROM customers WHERE bio IS NOT NULL AND id<=100",
                model
                    .customers
                    .iter()
                    .filter(|c| c.id <= 100)
                    .map(|c| vec![int(c.id), text(&c.name), int(c.bio.len() as i64)])
                    .collect(),
                false,
            );
        }
        "Transaction ctl" => {
            checks += both(db, pg, "SELECT * FROM bench_ins", model::inserted_single(), false);
            x(db, pg, "BEGIN");
            x(db, pg, "SAVEPOINT strict_sp");
            assert_eq!(db.execute("INSERT INTO bench_ins VALUES(99999,'txn')").unwrap(), 1);
            assert_eq!(pg.execute("INSERT INTO bench_ins VALUES(99999,'txn')").unwrap(), 1);
            checks += both(
                db,
                pg,
                "SELECT id,val FROM bench_ins WHERE id=99999",
                vec![vec![int(99999), text("txn")]],
                true,
            );
            x(db, pg, "ROLLBACK TO SAVEPOINT strict_sp");
            checks += both(db, pg, "SELECT id,val FROM bench_ins WHERE id=99999", vec![], true);
            x(db, pg, "COMMIT");
            checks += both(db, pg, "SELECT * FROM bench_ins", model::inserted_single(), false);
        }
        "Prepared stmts" => {
            db.query("PREPARE strict_prepared AS SELECT * FROM customers WHERE id=$1", &[])
                .unwrap();
            px(
                pg,
                "PREPARE strict_prepared(int) AS SELECT * FROM customers WHERE id=$1",
            );
            checks += both(
                db,
                pg,
                "EXECUTE strict_prepared(42)",
                vec![model.customers[41].cells()],
                true,
            );
            db.query("DEALLOCATE strict_prepared", &[]).unwrap();
            px(pg, "DEALLOCATE strict_prepared");
            missing_nano_object(
                db.query("EXECUTE strict_prepared(42)", &[])
                    .expect_err("deallocated Nano statement must be missing"),
                "strict_prepared",
            );
            let error = pg
                .rt
                .block_on(pg.client.query("EXECUTE strict_prepared(42)", &[]))
                .expect_err("deallocated PG statement must be missing");
            assert_eq!(error.as_db_error().unwrap().code().code(), "26000");
            checks += 2;
        }
        "SET/SHOW/RESET" => {
            cells::assert_rows(
                cells::nano(db, "SHOW work_mem"),
                context.nano_work_mem.clone(),
                true,
                "Nano RESET restored initial work_mem",
            );
            cells::assert_rows(
                cells::pg(pg, "SHOW work_mem"),
                context.pg_work_mem.clone(),
                true,
                "PG RESET restored initial work_mem",
            );
            checks += 2;
            db.query("SET work_mem='8192'", &[]).unwrap();
            px(pg, "SET work_mem='8MB'");
            assert_eq!(work_mem_kib(&cells::nano(db, "SHOW work_mem")), 8192);
            assert_eq!(work_mem_kib(&cells::pg(pg, "SHOW work_mem")), 8192);
            checks += 2;
            db.query("RESET work_mem", &[]).unwrap();
            px(pg, "RESET work_mem");
            cells::assert_rows(
                cells::nano(db, "SHOW work_mem"),
                context.nano_work_mem.clone(),
                true,
                "Nano reset control",
            );
            cells::assert_rows(
                cells::pg(pg, "SHOW work_mem"),
                context.pg_work_mem.clone(),
                true,
                "PG reset control",
            );
            checks += 2;
        }
        _ => panic!("no independent oracle for {name}"),
    }
    checks
}
