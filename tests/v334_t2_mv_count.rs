//! Regression coverage for checklist item T2.
//!
//! `COUNT(*)` over a materialized view must count rows from the MV backing
//! table, just like row reads and `SUM(1)` do.

use heliosdb_nano::{EmbeddedDatabase, Result, Tuple, Value};
use std::time::Instant;

fn scalar_i64(rows: &[Tuple]) -> i64 {
    assert_eq!(rows.len(), 1, "expected a single aggregate row");
    match rows[0].values.first() {
        Some(Value::Int2(value)) => i64::from(*value),
        Some(Value::Int4(value)) => i64::from(*value),
        Some(Value::Int8(value)) => *value,
        Some(Value::Float4(value)) => *value as i64,
        Some(Value::Float8(value)) => *value as i64,
        other => panic!("expected numeric scalar value, got {other:?}"),
    }
}

fn seed_orders_mv(db: &EmbeddedDatabase) -> Result<()> {
    db.execute("CREATE TABLE t2_orders (id INT PRIMARY KEY, status TEXT, amount INT)")?;
    db.execute(
        "INSERT INTO t2_orders VALUES \
         (1, 'paid', 100), \
         (2, 'paid', 200), \
         (3, 'draft', 50), \
         (4, 'paid', 300)",
    )?;
    db.execute(
        "CREATE MATERIALIZED VIEW t2_paid_orders AS \
         SELECT id, amount FROM t2_orders WHERE status = 'paid'",
    )?;
    Ok(())
}

#[test]
fn t2_count_star_over_materialized_view_matches_sum_one_and_rows() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed_orders_mv(&db)?;

    let rows = db.query("SELECT id, amount FROM t2_paid_orders ORDER BY id", &[])?;
    let sum_one = db.query("SELECT SUM(1) FROM t2_paid_orders", &[])?;
    let count_star = db.query("SELECT COUNT(*) FROM t2_paid_orders", &[])?;
    let count_id = db.query("SELECT COUNT(id) FROM t2_paid_orders", &[])?;
    let filtered_sum = db.query("SELECT SUM(1) FROM t2_paid_orders WHERE amount >= 200", &[])?;
    let filtered_count = db.query("SELECT COUNT(*) FROM t2_paid_orders WHERE amount >= 200", &[])?;
    let filtered_pk_range_count = db.query("SELECT COUNT(*) FROM t2_paid_orders WHERE id >= 2", &[])?;
    let filtered_pk_point_count = db.query("SELECT COUNT(*) FROM t2_paid_orders WHERE id = 2", &[])?;

    assert_eq!(rows.len(), 3, "direct MV row read should see three paid orders");
    assert_eq!(scalar_i64(&sum_one), 3, "SUM(1) should scan the same three MV rows");
    assert_eq!(
        scalar_i64(&count_star),
        3,
        "COUNT(*) over an MV must count the backing __mv_ table rows"
    );
    assert_eq!(
        scalar_i64(&count_id),
        3,
        "COUNT(pk) over an MV should use the same backing table count"
    );
    assert_eq!(scalar_i64(&filtered_sum), 2);
    assert_eq!(
        scalar_i64(&filtered_count),
        2,
        "filtered COUNT(*) over an MV should stay on the scan path"
    );
    assert_eq!(
        scalar_i64(&filtered_pk_range_count),
        2,
        "filtered COUNT(*) over an MV key range should count backing rows"
    );
    assert_eq!(
        scalar_i64(&filtered_pk_point_count),
        1,
        "filtered COUNT(*) over an MV key point should count backing rows"
    );
    Ok(())
}

#[test]
fn t2_count_star_over_materialized_view_after_refresh() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed_orders_mv(&db)?;

    db.execute("INSERT INTO t2_orders VALUES (5, 'paid', 400), (6, 'draft', 60)")?;
    db.execute("REFRESH MATERIALIZED VIEW t2_paid_orders")?;

    let rows = db.query("SELECT id FROM t2_paid_orders ORDER BY id", &[])?;
    let sum_one = db.query("SELECT SUM(1) FROM t2_paid_orders", &[])?;
    let count_star = db.query("SELECT COUNT(*) FROM t2_paid_orders", &[])?;

    assert_eq!(rows.len(), 4, "refreshed MV should include the new paid order");
    assert_eq!(scalar_i64(&sum_one), 4);
    assert_eq!(scalar_i64(&count_star), 4);
    Ok(())
}

#[test]
fn t2_count_star_materialized_view_stress() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE t2_events (id INT PRIMARY KEY, kind TEXT)")?;
    for id in 0..100 {
        let kind = if id % 2 == 0 { "keep" } else { "skip" };
        db.execute_params(
            "INSERT INTO t2_events VALUES ($1, $2)",
            &[Value::Int4(id), Value::String(kind.to_string())],
        )?;
    }
    db.execute(
        "CREATE MATERIALIZED VIEW t2_kept_events AS \
         SELECT id FROM t2_events WHERE kind = 'keep'",
    )?;

    let started = Instant::now();
    let count_star = db.query("SELECT COUNT(*) FROM t2_kept_events", &[])?;
    let elapsed = started.elapsed();
    let sum_one = db.query("SELECT SUM(1) FROM t2_kept_events", &[])?;

    assert_eq!(scalar_i64(&sum_one), 50);
    assert_eq!(scalar_i64(&count_star), 50);
    eprintln!("T2 stress: COUNT(*) over 50-row materialized view completed in {elapsed:?}");

    Ok(())
}
