//! Regression coverage for checklist item T3.
//!
//! Parameter references inside CTE bodies must bind to the same values as
//! equivalent non-CTE filters.

use heliosdb_nano::{EmbeddedDatabase, Result, Tuple, Value};
use std::time::Instant;

fn ids(rows: &[Tuple]) -> Vec<i32> {
    rows.iter()
        .map(|tuple| match tuple.values.first() {
            Some(Value::Int4(value)) => *value,
            Some(Value::Int8(value)) => *value as i32,
            other => panic!("expected integer id, got {other:?}"),
        })
        .collect()
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

fn seed_users(db: &EmbeddedDatabase) -> Result<()> {
    db.execute("CREATE TABLE t3_users (id INT PRIMARY KEY, tenant INT, status TEXT, score INT)")?;
    db.execute(
        "INSERT INTO t3_users VALUES \
         (1, 10, 'active', 90), \
         (2, 20, 'active', 80), \
         (3, 10, 'disabled', 70), \
         (4, 10, 'active', 60), \
         (5, 30, 'active', 50)",
    )?;
    Ok(())
}

#[test]
fn t3_cte_body_binds_single_parameter_like_direct_filter() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed_users(&db)?;

    let params = [Value::Int4(10)];
    let direct = db.query_params("SELECT id FROM t3_users WHERE tenant = $1 ORDER BY id", &params)?;
    let via_cte = db.query_params(
        "WITH tenant_users AS ( \
             SELECT id, tenant, status FROM t3_users WHERE tenant = $1 \
         ) \
         SELECT id FROM tenant_users ORDER BY id",
        &params,
    )?;

    assert_eq!(ids(&direct), vec![1, 3, 4]);
    assert_eq!(ids(&via_cte), ids(&direct));
    Ok(())
}

#[test]
fn t3_cte_body_binds_multiple_and_reused_parameters() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed_users(&db)?;

    let params = [Value::String("active".to_string()), Value::Int4(10), Value::Int4(50)];
    let direct = db.query_params(
        "SELECT id FROM t3_users \
         WHERE tenant = $2 AND status = $1 AND score > $3 \
         ORDER BY id",
        &params,
    )?;
    let via_cte = db.query_params(
        "WITH filtered AS ( \
             SELECT id, tenant, status, score FROM t3_users \
             WHERE tenant = $2 AND status = $1 \
         ) \
         SELECT id FROM filtered WHERE score > $3 AND tenant = $2 ORDER BY id",
        &params,
    )?;

    assert_eq!(ids(&direct), vec![1, 4]);
    assert_eq!(ids(&via_cte), ids(&direct));
    Ok(())
}

#[test]
fn t3_cte_column_aliases_and_count_bind_parameters() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed_users(&db)?;

    let params = [Value::Int4(10), Value::String("active".to_string())];
    let rows = db.query_params(
        "WITH active_ids(user_id) AS ( \
             SELECT id FROM t3_users WHERE tenant = $1 AND status = $2 \
         ) \
         SELECT COUNT(*) FROM active_ids",
        &params,
    )?;

    assert_eq!(scalar_i64(&rows), 2);
    Ok(())
}

#[test]
fn t3_query_params_with_columns_binds_cte_parameters() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed_users(&db)?;

    let (rows, columns) = db.query_params_with_columns(
        "WITH selected AS ( \
             SELECT id, tenant, status FROM t3_users WHERE tenant = $1 AND status = $2 \
         ) \
         SELECT id AS user_id FROM selected ORDER BY id",
        &[Value::Int4(10), Value::String("active".to_string())],
    )?;

    assert_eq!(columns, vec!["user_id".to_string()]);
    assert_eq!(ids(&rows), vec![1, 4]);
    Ok(())
}

#[test]
fn t3_cte_parameter_binding_survives_join_and_reuse() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed_users(&db)?;

    let joined = db.query_params(
        "WITH filtered AS ( \
             SELECT id FROM t3_users WHERE status = $1 \
         ) \
         SELECT u.id FROM t3_users u \
         JOIN filtered f ON u.id = f.id \
         WHERE u.tenant = $2 \
         ORDER BY u.id",
        &[Value::String("active".to_string()), Value::Int4(10)],
    )?;
    let reused = db.query_params(
        "WITH filtered AS ( \
             SELECT id, status FROM t3_users WHERE status = $1 \
         ) \
         SELECT id FROM filtered WHERE status = $1 ORDER BY id",
        &[Value::String("active".to_string())],
    )?;

    assert_eq!(ids(&joined), vec![1, 4]);
    assert_eq!(ids(&reused), vec![1, 2, 4, 5]);
    Ok(())
}

#[test]
fn t3_cte_parameter_binding_survives_set_operations_and_multiple_refs() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed_users(&db)?;

    let set_rows = db.query_params(
        "WITH filtered AS ( \
             SELECT id FROM t3_users WHERE tenant = $1 \
         ) \
         SELECT id FROM filtered WHERE id < $2 \
         UNION \
         SELECT id FROM filtered WHERE id > $3 \
         ORDER BY id",
        &[Value::Int4(10), Value::Int4(2), Value::Int4(3)],
    )?;
    let multi_ref_count = db.query_params(
        "WITH filtered AS ( \
             SELECT id FROM t3_users WHERE tenant = $1 \
         ) \
         SELECT COUNT(*) FROM filtered a JOIN filtered b ON a.id = b.id",
        &[Value::Int4(10)],
    )?;

    assert_eq!(ids(&set_rows), vec![1, 4]);
    assert_eq!(scalar_i64(&multi_ref_count), 3);
    Ok(())
}

#[test]
fn t3_recursive_cte_binds_parameters() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    let rows = db.query_params(
        "WITH RECURSIVE nums(n) AS ( \
             SELECT $1 \
             UNION ALL \
             SELECT n + $2 FROM nums WHERE n < $3 \
         ) \
         SELECT n FROM nums ORDER BY n",
        &[Value::Int4(1), Value::Int4(1), Value::Int4(4)],
    )?;

    assert_eq!(ids(&rows), vec![1, 2, 3, 4]);
    Ok(())
}

#[test]
fn t3_cte_parameter_binding_stress_smoke() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE t3_events (id INT PRIMARY KEY, bucket INT, label TEXT)")?;
    for id in 0..200 {
        db.execute_params(
            "INSERT INTO t3_events VALUES ($1, $2, $3)",
            &[
                Value::Int4(id),
                Value::Int4(id % 5),
                Value::String(format!("label-{}", id % 3)),
            ],
        )?;
    }

    let started = Instant::now();
    for bucket in 0..5 {
        let rows = db.query_params(
            "WITH selected AS ( \
                 SELECT id, bucket FROM t3_events WHERE bucket = $1 \
             ) \
             SELECT COUNT(*) FROM selected WHERE bucket = $1",
            &[Value::Int4(bucket)],
        )?;
        assert_eq!(scalar_i64(&rows), 40);
    }
    let elapsed = started.elapsed();
    eprintln!("T3 stress: 5 CTE parameterized counts over 200 rows completed in {elapsed:?}");

    Ok(())
}
