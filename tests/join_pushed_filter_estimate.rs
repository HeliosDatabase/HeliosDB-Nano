//! A `WHERE` filter the optimizer pushes below a join sits over a plain scan. Its row
//! estimate now comes from the index the equality names (`indexed_equality_rows`), so a
//! primary-key filter is one row, and a left-deep chain of joins above it keeps an
//! estimate: each join whose right side has an index on its key becomes an index
//! nested loop instead of a scan of the whole right table and a hash join.
//!
//! The estimate only chooses the join strategy, so the check is differential: every
//! query answers the same with the index nested loop available and with it switched off
//! (`HELIOS_INLJ_OFF`, read per join). One test function: the switch is process-wide.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};

fn rows(db: &EmbeddedDatabase, sql: &str, params: &[Value]) -> Vec<String> {
    let result: Vec<Tuple> = if params.is_empty() {
        db.query(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"))
    } else {
        db.query_params(sql, params).unwrap_or_else(|e| panic!("{sql}: {e}"))
    };
    let mut out: Vec<String> = result.iter().map(|row| format!("{:?}", row.values)).collect();
    out.sort();
    out
}

#[test]
fn a_join_answers_the_same_with_and_without_the_index_nested_loop() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    for sql in [
        "CREATE TABLE customers (id INT PRIMARY KEY, name TEXT, region TEXT)",
        "CREATE TABLE orders (order_id INT PRIMARY KEY, customer_id INT, status TEXT, total INT)",
        "CREATE TABLE order_items (item_id INT PRIMARY KEY, order_id INT, product_id INT, quantity INT)",
        "CREATE TABLE products (product_id INT PRIMARY KEY, name TEXT, sku TEXT)",
        "CREATE INDEX idx_cust_region ON customers(region)",
        "CREATE INDEX idx_ord_cust ON orders(customer_id)",
        "CREATE INDEX idx_ord_status ON orders(status)",
        "CREATE INDEX idx_oi_ord ON order_items(order_id)",
        "CREATE INDEX idx_prod_sku ON products(sku)",
    ] {
        db.execute(sql).unwrap();
    }
    let regions = ["East", "West", "North"];
    for i in 1..=60 {
        db.execute(&format!(
            "INSERT INTO customers VALUES ({i}, 'c{i}', '{}')",
            regions[i % regions.len()]
        ))
        .unwrap();
    }
    for i in 1..=20 {
        db.execute(&format!("INSERT INTO products VALUES ({i}, 'p{i}', 'sku-{i}')"))
            .unwrap();
    }
    // Customer 7 has no orders; order 150 has no items; some items name no product.
    for i in 1..=150 {
        let customer = if i % 60 + 1 == 7 { 8 } else { i % 60 + 1 };
        let status = if i % 2 == 0 { "open" } else { "done" };
        db.execute(&format!(
            "INSERT INTO orders VALUES ({i}, {customer}, '{status}', {})",
            i * 3
        ))
        .unwrap();
    }
    for i in 1..=300 {
        let order = i % 149 + 1;
        let product = if i % 25 == 0 { 99 } else { i % 20 + 1 };
        db.execute(&format!(
            "INSERT INTO order_items VALUES ({i}, {order}, {product}, {})",
            i % 5 + 1
        ))
        .unwrap();
    }

    let four = "SELECT c.name, o.order_id, oi.quantity, p.name FROM customers c \
                INNER JOIN orders o ON c.id = o.customer_id \
                INNER JOIN order_items oi ON o.order_id = oi.order_id \
                INNER JOIN products p ON oi.product_id = p.product_id";
    let queries: Vec<(String, Vec<Value>)> = vec![
        // A primary-key filter under one, two and three joins; a key with no rows; a key with no orders.
        (format!("{four} WHERE c.id = 12"), vec![]),
        (format!("{four} WHERE c.id = 7"), vec![]),
        (format!("{four} WHERE c.id = 1000"), vec![]),
        (format!("{four} WHERE c.id = $1"), vec![Value::Int4(33)]),
        (
            "SELECT o.order_id, oi.item_id FROM orders o INNER JOIN order_items oi ON o.order_id = oi.order_id \
             WHERE o.order_id = 42"
                .to_string(),
            vec![],
        ),
        (
            "SELECT o.order_id, oi.item_id FROM orders o INNER JOIN order_items oi ON o.order_id = oi.order_id \
             WHERE o.order_id = 150"
                .to_string(),
            vec![],
        ),
        // A secondary index with a few rows under the value, and one with half the table.
        (
            "SELECT o.order_id, oi.item_id FROM orders o INNER JOIN order_items oi ON o.order_id = oi.order_id \
             WHERE o.customer_id = 12"
                .to_string(),
            vec![],
        ),
        (
            "SELECT o.order_id, oi.item_id FROM orders o INNER JOIN order_items oi ON o.order_id = oi.order_id \
             WHERE o.status = 'open'"
                .to_string(),
            vec![],
        ),
        (
            "SELECT c.name, o.order_id FROM customers c INNER JOIN orders o ON c.id = o.customer_id \
             WHERE c.region = 'East'"
                .to_string(),
            vec![],
        ),
        // Two terms: the index of one, a plain comparison beside it; and two indexed terms.
        (
            "SELECT c.name, o.order_id, oi.item_id FROM customers c INNER JOIN orders o ON c.id = o.customer_id \
             INNER JOIN order_items oi ON o.order_id = oi.order_id WHERE c.id = 20 AND c.name = 'c20'"
                .to_string(),
            vec![],
        ),
        (
            "SELECT c.name, o.order_id FROM customers c INNER JOIN orders o ON c.id = o.customer_id \
             WHERE c.id = 21 AND c.region = 'East'"
                .to_string(),
            vec![],
        ),
        // The literal is not of the column's type: no index count, the old estimate.
        (format!("{four} WHERE c.id = 12.0"), vec![]),
        (format!("{four} WHERE c.id = $1"), vec![Value::Int8(12)]),
        // LEFT joins keep the unmatched rows either way.
        (
            "SELECT c.name, o.order_id FROM customers c LEFT JOIN orders o ON c.id = o.customer_id WHERE c.id = 7"
                .to_string(),
            vec![],
        ),
        (
            "SELECT o.order_id, oi.item_id FROM orders o LEFT JOIN order_items oi ON o.order_id = oi.order_id \
             WHERE o.order_id = 150"
                .to_string(),
            vec![],
        ),
        // A text key through an index, and a filter on the right-hand table.
        (
            "SELECT p.name, oi.item_id FROM products p INNER JOIN order_items oi ON p.product_id = oi.product_id \
             WHERE p.sku = 'sku-3'"
                .to_string(),
            vec![],
        ),
        (
            "SELECT c.name, o.order_id FROM customers c INNER JOIN orders o ON c.id = o.customer_id \
             WHERE o.order_id = 77"
                .to_string(),
            vec![],
        ),
    ];

    let with_index: Vec<Vec<String>> = queries.iter().map(|(sql, params)| rows(&db, sql, params)).collect();
    // Results are cached by statement text; the switch changes no text.
    db.execute("CREATE TABLE cache_buster (id INT)").unwrap();
    std::env::set_var("HELIOS_INLJ_OFF", "1");
    let without: Vec<Vec<String>> = queries.iter().map(|(sql, params)| rows(&db, sql, params)).collect();
    std::env::remove_var("HELIOS_INLJ_OFF");

    for (((sql, _), a), b) in queries.iter().zip(&with_index).zip(&without) {
        assert_eq!(a, b, "{sql}");
    }
    // The shapes are not vacuous.
    assert_eq!(with_index[1].len(), 0, "customer 7 has no orders");
    assert_eq!(with_index[2].len(), 0, "no such customer");
    assert!(!with_index[0].is_empty() && !with_index[4].is_empty() && !with_index[7].is_empty());
    assert_eq!(with_index[13].len(), 1, "LEFT JOIN keeps the customer without orders");
    assert_eq!(with_index[14].len(), 1, "LEFT JOIN keeps the order without items");
}
