//! Regression coverage for checklist item T8.
//!
//! `GROUP BY` without aggregates must produce one row per distinct group, both
//! directly and when materialized into an MV.

use heliosdb_nano::{EmbeddedDatabase, Result, Tuple, Value};
use std::time::Instant;

fn int_value(tuple: &Tuple, index: usize) -> i32 {
    match tuple.values.get(index) {
        Some(Value::Int2(value)) => i32::from(*value),
        Some(Value::Int4(value)) => *value,
        Some(Value::Int8(value)) => *value as i32,
        other => panic!("expected integer at column {index}, got {other:?}"),
    }
}

fn pairs(rows: &[Tuple]) -> Vec<(i32, i32)> {
    rows.iter()
        .map(|tuple| (int_value(tuple, 0), int_value(tuple, 1)))
        .collect()
}

fn first_ints(rows: &[Tuple]) -> Vec<i32> {
    rows.iter().map(|tuple| int_value(tuple, 0)).collect()
}

fn scalar_i64(rows: &[Tuple]) -> i64 {
    assert_eq!(rows.len(), 1, "expected one scalar row");
    match rows[0].values.first() {
        Some(Value::Int2(value)) => i64::from(*value),
        Some(Value::Int4(value)) => i64::from(*value),
        Some(Value::Int8(value)) => *value,
        other => panic!("expected integer scalar, got {other:?}"),
    }
}

fn seed_duplicate_pairs(db: &EmbeddedDatabase) -> Result<()> {
    db.execute("CREATE TABLE t8_pairs (id INT PRIMARY KEY, a INT, b INT, tenant INT)")?;
    for (id, a, b, tenant) in [
        (1, 1, 10, 100),
        (2, 1, 10, 100),
        (3, 1, 20, 100),
        (4, 2, 10, 100),
        (5, 2, 10, 100),
        (6, 2, 10, 200),
    ] {
        db.execute_params(
            "INSERT INTO t8_pairs VALUES ($1, $2, $3, $4)",
            &[Value::Int4(id), Value::Int4(a), Value::Int4(b), Value::Int4(tenant)],
        )?;
    }
    Ok(())
}

#[test]
fn t8_direct_group_by_without_aggregates_dedupes_rows() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed_duplicate_pairs(&db)?;

    let grouped = db.query(
        "SELECT a, b FROM t8_pairs WHERE tenant = 100 GROUP BY a, b ORDER BY a, b",
        &[],
    )?;
    let distinct = db.query(
        "SELECT DISTINCT a, b FROM t8_pairs WHERE tenant = 100 ORDER BY a, b",
        &[],
    )?;

    assert_eq!(pairs(&distinct), vec![(1, 10), (1, 20), (2, 10)]);
    assert_eq!(
        pairs(&grouped),
        pairs(&distinct),
        "GROUP BY without aggregates should match DISTINCT pairs"
    );

    let projected_grouped = db.query(
        "SELECT a FROM t8_pairs WHERE tenant = 100 GROUP BY a, b ORDER BY a, b",
        &[],
    )?;
    assert_eq!(
        first_ints(&projected_grouped),
        vec![1, 1, 2],
        "GROUP BY should emit one row per group key, not DISTINCT over the final projection"
    );
    Ok(())
}

#[test]
fn t8_materialized_view_group_by_without_aggregates_dedupes_on_create() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed_duplicate_pairs(&db)?;

    db.execute(
        "CREATE MATERIALIZED VIEW t8_mv_pairs AS \
         SELECT a, b FROM t8_pairs WHERE tenant = 100 GROUP BY a, b",
    )?;

    let mv_rows = db.query("SELECT a, b FROM t8_mv_pairs ORDER BY a, b", &[])?;
    let mv_count = db.query("SELECT COUNT(*) FROM t8_mv_pairs", &[])?;
    let direct_grouped = db.query(
        "SELECT a, b FROM t8_pairs WHERE tenant = 100 GROUP BY a, b ORDER BY a, b",
        &[],
    )?;

    assert_eq!(pairs(&mv_rows), vec![(1, 10), (1, 20), (2, 10)]);
    assert_eq!(pairs(&mv_rows), pairs(&direct_grouped));
    assert_eq!(scalar_i64(&mv_count), 3);
    Ok(())
}

#[test]
fn t8_materialized_view_group_by_without_aggregates_dedupes_after_refresh() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed_duplicate_pairs(&db)?;
    db.execute(
        "CREATE MATERIALIZED VIEW t8_mv_refresh AS \
         SELECT a, b FROM t8_pairs WHERE tenant = 100 GROUP BY a, b",
    )?;

    db.execute_params(
        "INSERT INTO t8_pairs VALUES ($1, $2, $3, $4)",
        &[Value::Int4(7), Value::Int4(1), Value::Int4(10), Value::Int4(100)],
    )?;
    db.execute_params(
        "INSERT INTO t8_pairs VALUES ($1, $2, $3, $4)",
        &[Value::Int4(8), Value::Int4(3), Value::Int4(30), Value::Int4(100)],
    )?;
    db.execute("REFRESH MATERIALIZED VIEW t8_mv_refresh")?;

    let mv_rows = db.query("SELECT a, b FROM t8_mv_refresh ORDER BY a, b", &[])?;
    let mv_count = db.query("SELECT COUNT(*) FROM t8_mv_refresh", &[])?;

    assert_eq!(pairs(&mv_rows), vec![(1, 10), (1, 20), (2, 10), (3, 30)]);
    assert_eq!(scalar_i64(&mv_count), 4);
    Ok(())
}

#[test]
fn t8_group_by_without_aggregates_stress_smoke() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE t8_events (id INT PRIMARY KEY, a INT, b INT)")?;
    for id in 0..300 {
        db.execute_params(
            "INSERT INTO t8_events VALUES ($1, $2, $3)",
            &[Value::Int4(id), Value::Int4(id % 10), Value::Int4(id % 3)],
        )?;
    }

    let started = Instant::now();
    db.execute("CREATE MATERIALIZED VIEW t8_mv_events AS SELECT a, b FROM t8_events GROUP BY a, b")?;
    let count = db.query("SELECT COUNT(*) FROM t8_mv_events", &[])?;
    let elapsed = started.elapsed();

    assert_eq!(scalar_i64(&count), 30);
    eprintln!("T8 stress: grouped 300 rows into 30 MV rows in {elapsed:?}");
    Ok(())
}

#[test]
fn t8_order_by_group_key_with_reordered_projection_sorts_correctly() -> Result<()> {
    // Latent-bug regression (fixed with the T8 ORDER-BY fix): the old ORDER BY
    // rewrite sliced the post-aggregate Project aliases POSITIONALLY as
    // [group cols…, agg cols…], which mis-maps whenever the select list is not
    // exactly the group list. `SELECT b, a … GROUP BY a, b ORDER BY a` then
    // silently redirected the sort to the alias at position 0 — column b —
    // returning wrongly-ordered rows. The group_N rewrite sorts below the
    // projection on the real group key.
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE t8_reorder (id INT PRIMARY KEY, a INT, b INT)")?;
    // (a, b) pairs chosen so ordering by a and ordering by b DISAGREE:
    // by a: (1,30), (2,20), (3,10) — by b would reverse it.
    db.execute("INSERT INTO t8_reorder VALUES (1, 2, 20), (2, 1, 30), (3, 3, 10)")?;

    let rows = db.query("SELECT b, a FROM t8_reorder GROUP BY a, b ORDER BY a", &[])?;
    // Output columns are (b, a); ordered by a ascending → a = 1, 2, 3.
    let a_values: Vec<i32> = rows.iter().map(|t| int_value(t, 1)).collect();
    assert_eq!(
        a_values,
        vec![1, 2, 3],
        "ORDER BY a must sort by column a even when the projection lists b first (old rewrite sorted by b)"
    );
    Ok(())
}
