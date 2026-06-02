//! Token Dashboard outstanding item #3: a CTE combined with a `$N` parameter
//! used only in the OUTER query body returned zero rows, even though the same
//! query without the CTE matched. Existing T3 coverage always placed the
//! parameter inside the CTE body and passed an exactly-typed Value, so this
//! shape (param only in the outer body, plus a type that needs coercion) was
//! uncovered.

use heliosdb_nano::{EmbeddedDatabase, Result, Tuple, Value};

fn ints(rows: &[Tuple]) -> Vec<i64> {
    rows.iter()
        .map(|r| match r.values.first() {
            Some(Value::Int2(v)) => i64::from(*v),
            Some(Value::Int4(v)) => i64::from(*v),
            Some(Value::Int8(v)) => *v,
            other => panic!("expected int, got {other:?}"),
        })
        .collect()
}

#[test]
fn constant_cte_outer_param_filter() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    // The exact minimal repro from the report.
    let rows = db.query_params(
        "WITH x AS (SELECT 5 AS n) SELECT n FROM x WHERE n >= $1",
        &[Value::Int4(0)],
    )?;
    assert_eq!(ints(&rows), vec![5]);

    // And the excluding bound returns nothing.
    let none = db.query_params(
        "WITH x AS (SELECT 5 AS n) SELECT n FROM x WHERE n >= $1",
        &[Value::Int4(10)],
    )?;
    assert!(none.is_empty());
    Ok(())
}

#[test]
fn table_cte_outer_param_matches_non_cte() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE td3_msgs (id INT PRIMARY KEY, input_tokens INT)")?;
    db.execute("INSERT INTO td3_msgs VALUES (1, 100), (2, 0), (3, 250), (4, 50)")?;

    let non_cte = db.query_params(
        "SELECT id FROM td3_msgs WHERE input_tokens >= $1 ORDER BY id",
        &[Value::Int4(50)],
    )?;
    let via_cte = db.query_params(
        "WITH x AS (SELECT id, input_tokens FROM td3_msgs) \
         SELECT id FROM x WHERE input_tokens >= $1 ORDER BY id",
        &[Value::Int4(50)],
    )?;
    assert_eq!(ints(&via_cte), ints(&non_cte));
    assert_eq!(ints(&via_cte), vec![1, 3, 4]);
    Ok(())
}

#[test]
fn table_cte_outer_param_needs_type_coercion() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE td3_c (id INT PRIMARY KEY, v INT)")?;
    db.execute("INSERT INTO td3_c VALUES (1, 10), (2, 20), (3, 30)")?;

    // Mimic a text-protocol client binding $1 as int8 (or text) against an
    // int4 CTE-derived column — coercion must still apply through the CTE.
    let via_cte_i8 = db.query_params(
        "WITH x AS (SELECT id, v FROM td3_c) SELECT id FROM x WHERE v >= $1 ORDER BY id",
        &[Value::Int8(20)],
    )?;
    assert_eq!(ints(&via_cte_i8), vec![2, 3]);
    Ok(())
}
