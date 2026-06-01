use std::time::Instant;

use heliosdb_nano::{EmbeddedDatabase, Result, Value};

fn bool_at(row: &[Value], index: usize) -> bool {
    match row.get(index) {
        Some(Value::Boolean(value)) => *value,
        other => panic!("expected boolean at {index}, got {other:?}"),
    }
}

fn int_at(row: &[Value], index: usize) -> i64 {
    match row.get(index) {
        Some(Value::Int2(value)) => i64::from(*value),
        Some(Value::Int4(value)) => i64::from(*value),
        Some(Value::Int8(value)) => *value,
        other => panic!("expected integer at {index}, got {other:?}"),
    }
}

#[test]
fn a10_scalar_truth_table_and_inverse() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    let rows = db.query(
        "SELECT \
            'a' IS DISTINCT FROM 'b', \
            'a' IS DISTINCT FROM 'a', \
            NULL IS DISTINCT FROM 'a', \
            NULL IS DISTINCT FROM NULL, \
            NULL IS NOT DISTINCT FROM NULL, \
            1 IS NOT DISTINCT FROM 1.0",
        &[],
    )?;

    assert_eq!(rows.len(), 1);
    let row = &rows[0].values;
    assert!(bool_at(row, 0));
    assert!(!bool_at(row, 1));
    assert!(bool_at(row, 2));
    assert!(!bool_at(row, 3));
    assert!(bool_at(row, 4));
    assert!(bool_at(row, 5));
    Ok(())
}

#[test]
fn a10_table_projection_and_where_filters() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE a10_pairs (id INT4 PRIMARY KEY, a INT4, b INT4)")?;
    db.execute(
        "INSERT INTO a10_pairs VALUES \
         (1, 1, 1), \
         (2, 1, 2), \
         (3, NULL, 2), \
         (4, NULL, NULL)",
    )?;

    let rows = db.query(
        "SELECT id, a IS DISTINCT FROM b, a IS NOT DISTINCT FROM b FROM a10_pairs ORDER BY id",
        &[],
    )?;
    let projected: Vec<(i64, bool, bool)> = rows
        .iter()
        .map(|row| (int_at(&row.values, 0), bool_at(&row.values, 1), bool_at(&row.values, 2)))
        .collect();
    assert_eq!(
        projected,
        vec![(1, false, true), (2, true, false), (3, true, false), (4, false, true)]
    );

    let distinct_ids: Vec<i64> = db
        .query("SELECT id FROM a10_pairs WHERE a IS DISTINCT FROM b ORDER BY id", &[])?
        .iter()
        .map(|row| int_at(&row.values, 0))
        .collect();
    assert_eq!(distinct_ids, vec![2, 3]);

    let not_distinct_ids: Vec<i64> = db
        .query(
            "SELECT id FROM a10_pairs WHERE a IS NOT DISTINCT FROM b ORDER BY id",
            &[],
        )?
        .iter()
        .map(|row| int_at(&row.values, 0))
        .collect();
    assert_eq!(not_distinct_ids, vec![1, 4]);
    Ok(())
}

#[test]
fn a10_query_params_bind_null_safe_distinct_operands() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE a10_params (id INT4 PRIMARY KEY, marker INT4)")?;
    db.execute("INSERT INTO a10_params VALUES (1, 7), (2, NULL), (3, 9), (4, NULL)")?;

    let rows = db.query_params(
        "SELECT $1 IS DISTINCT FROM $2, $1 IS NOT DISTINCT FROM $2, $3 IS NOT DISTINCT FROM $4",
        &[
            Value::Null,
            Value::Int4(7),
            Value::String("42".to_string()),
            Value::Int4(42),
        ],
    )?;
    assert_eq!(rows.len(), 1);
    assert!(bool_at(&rows[0].values, 0));
    assert!(!bool_at(&rows[0].values, 1));
    assert!(bool_at(&rows[0].values, 2));

    let null_distinct_ids: Vec<i64> = db
        .query_params(
            "SELECT id FROM a10_params WHERE marker IS DISTINCT FROM $1 ORDER BY id",
            &[Value::Null],
        )?
        .iter()
        .map(|row| int_at(&row.values, 0))
        .collect();
    assert_eq!(null_distinct_ids, vec![1, 3]);

    let null_not_distinct_ids: Vec<i64> = db
        .query_params(
            "SELECT id FROM a10_params WHERE marker IS NOT DISTINCT FROM $1 ORDER BY id",
            &[Value::Null],
        )?
        .iter()
        .map(|row| int_at(&row.values, 0))
        .collect();
    assert_eq!(null_not_distinct_ids, vec![2, 4]);
    Ok(())
}

#[test]
fn a10_on_conflict_predicate_uses_is_distinct_from() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE a10_upsert (id INT4 PRIMARY KEY, val TEXT, hits INT4)")?;
    db.execute("INSERT INTO a10_upsert VALUES (1, 'same', 0)")?;

    let skipped = db.execute(
        "INSERT INTO a10_upsert VALUES (1, 'same', 1) \
         ON CONFLICT (id) DO UPDATE SET val = excluded.val, hits = hits + 1 \
         WHERE a10_upsert.val IS DISTINCT FROM excluded.val",
    )?;
    assert_eq!(skipped, 0);

    let updated = db.execute(
        "INSERT INTO a10_upsert VALUES (1, 'different', 1) \
         ON CONFLICT (id) DO UPDATE SET val = excluded.val, hits = hits + 1 \
         WHERE a10_upsert.val IS DISTINCT FROM excluded.val",
    )?;
    assert_eq!(updated, 1);

    let rows = db.query("SELECT val, hits FROM a10_upsert WHERE id = 1", &[])?;
    assert_eq!(
        rows[0].values,
        vec![Value::String("different".to_string()), Value::Int4(1)]
    );
    Ok(())
}

#[test]
fn a10_distinct_from_stress_count() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE a10_stress (id INT4 PRIMARY KEY, a INT4, b INT4)")?;

    for i in 0..300 {
        match i % 3 {
            0 => {
                db.execute_params(
                    "INSERT INTO a10_stress VALUES ($1, $2, $3)",
                    &[Value::Int4(i), Value::Int4(i), Value::Int4(i)],
                )?;
            }
            1 => {
                db.execute_params(
                    "INSERT INTO a10_stress VALUES ($1, $2, $3)",
                    &[Value::Int4(i), Value::Int4(i), Value::Int4(i + 1)],
                )?;
            }
            _ => {
                db.execute_params(
                    "INSERT INTO a10_stress VALUES ($1, $2, $3)",
                    &[Value::Int4(i), Value::Null, Value::Int4(i)],
                )?;
            }
        }
    }

    let started = Instant::now();
    let rows = db.query("SELECT COUNT(*) FROM a10_stress WHERE a IS DISTINCT FROM b", &[])?;
    let elapsed = started.elapsed();

    assert_eq!(rows[0].values[0], Value::Int8(200));
    eprintln!("A10 stress: COUNT over 300 rows with IS DISTINCT FROM in {:?}", elapsed);
    Ok(())
}
