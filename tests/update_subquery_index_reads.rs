//! `UPDATE … SET col = (SELECT …)` in autocommit: the subquery reads through an
//! index, and still sees what the statement itself has written.
//!
//! Since 4.42.0 the subquery runs in the statement's own transaction, so that it
//! sees the rows the statement has already staged. That transaction was treated
//! like an explicit one, whose reads must come from its snapshot: every
//! subquery became a full scan with a write-set merge (pg35 `UPDATE+subquery`
//! ran 34x slower than 4.41.1). An autocommit statement's transaction took its
//! snapshot when the statement began, so a table the statement has not written
//! is read through its index again; a table it has written still comes from the
//! write set. The test pins the answers, including one the statement used to
//! refuse: an aggregate over a correlated filter in `SET`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

fn int(value: &Value) -> i64 {
    match value {
        Value::Int2(n) => i64::from(*n),
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        other => panic!("not an integer: {other:?}"),
    }
}

fn orders_and_items() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE orders (order_id INT PRIMARY KEY, total INT)")
        .unwrap();
    db.execute("CREATE TABLE order_items (item_id INT PRIMARY KEY, order_id INT, quantity INT)")
        .unwrap();
    db.execute("CREATE INDEX idx_oi_ord ON order_items(order_id)").unwrap();
    for order in 1..=40 {
        db.execute(&format!("INSERT INTO orders VALUES ({order}, -1)")).unwrap();
    }
    // Order n has n % 5 items.
    let mut item = 0;
    for order in 1..=40 {
        for _ in 0..order % 5 {
            item += 1;
            db.execute(&format!("INSERT INTO order_items VALUES ({item}, {order}, 1)"))
                .unwrap();
        }
    }
    db
}

#[test]
fn a_set_subquery_counts_the_rows_of_another_indexed_table() {
    let db = orders_and_items();
    // The pg35 shape: one row, a literal in the subquery.
    for order in [3, 4, 5, 39] {
        let changed = db
            .execute(&format!(
                "UPDATE orders SET total = (SELECT COUNT(*) FROM order_items WHERE order_id = {order}) \
                 WHERE order_id = {order}"
            ))
            .unwrap();
        assert_eq!(changed, 1);
        let rows = db
            .query(&format!("SELECT total FROM orders WHERE order_id = {order}"), &[])
            .unwrap();
        assert_eq!(int(&rows[0].values[0]), i64::from(order % 5), "order {order}");
    }
    // Correlated, every row: each subquery runs after earlier rows of `orders`
    // were staged, and reads `order_items`, which the statement never writes.
    // (An aggregate over a correlated filter was refused with "column not
    // found" before this release's fix, in 4.41 as well.)
    let changed = db
        .execute(
            "UPDATE orders SET total = \
             (SELECT COUNT(*) FROM order_items WHERE order_items.order_id = orders.order_id)",
        )
        .unwrap();
    assert_eq!(changed, 40);
    let rows = db
        .query("SELECT order_id, total FROM orders ORDER BY order_id", &[])
        .unwrap();
    assert_eq!(rows.len(), 40);
    for row in &rows {
        let order = int(&row.values[0]);
        assert_eq!(int(&row.values[1]), order % 5, "order {order}");
    }
}
