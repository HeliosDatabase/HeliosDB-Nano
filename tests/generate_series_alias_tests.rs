//! PGConf.Brasil #10 (sprinter 098f8b31cae3): `FROM generate_series(1, n) AS g`
//!
//! PostgreSQL semantics for a table function that returns a single scalar column:
//! when the FROM item carries a table alias but no column list, the output column
//! is named after the alias. So `SELECT g FROM generate_series(1,3) AS g`,
//! `SELECT g.g FROM generate_series(1,3) g` and `SELECT u FROM unnest(...) AS u`
//! all work, and `SELECT * FROM generate_series(1,3) AS g` yields a column named `g`.
//!
//! Before the fix the alias only became the *source table* name, the column kept
//! the function name, and every one of those statements failed with
//! `Column 'g' not found in schema` (reproduced over the PG wire on v4.30.0).
//! An explicit column list (`g(i)`) and the alias-less form are unchanged.

mod test_helpers;

use heliosdb_nano::{Result, Value};
use test_helpers::*;

fn ints(rows: &[heliosdb_nano::Tuple]) -> Vec<i64> {
    rows.iter()
        .map(|t| match &t.values[0] {
            Value::Int8(v) => *v,
            Value::Int4(v) => i64::from(*v),
            other => panic!("expected an integer, got {other:?}"),
        })
        .collect()
}

#[test]
fn alias_without_column_list_names_the_column() -> Result<()> {
    let db = create_test_db()?;
    let rows = db.query("SELECT g FROM generate_series(1, 3) AS g", &[])?;
    assert_eq!(ints(&rows), vec![1, 2, 3]);
    Ok(())
}

#[test]
fn alias_without_as_keyword_names_the_column() -> Result<()> {
    let db = create_test_db()?;
    let rows = db.query("SELECT g FROM generate_series(1, 3) g", &[])?;
    assert_eq!(ints(&rows), vec![1, 2, 3]);
    Ok(())
}

#[test]
fn qualified_alias_column_resolves() -> Result<()> {
    let db = create_test_db()?;
    let rows = db.query("SELECT g.g FROM generate_series(1, 3) g", &[])?;
    assert_eq!(ints(&rows), vec![1, 2, 3]);
    Ok(())
}

#[test]
fn alias_column_usable_in_where_and_order_by() -> Result<()> {
    let db = create_test_db()?;
    let rows = db.query(
        "SELECT g FROM generate_series(1, 5) AS g WHERE g > 2 ORDER BY g DESC",
        &[],
    )?;
    assert_eq!(ints(&rows), vec![5, 4, 3]);
    Ok(())
}

#[test]
fn star_with_alias_exposes_alias_named_column() -> Result<()> {
    let db = create_test_db()?;
    let rows = db.query("SELECT * FROM generate_series(1, 2) AS g", &[])?;
    assert_eq!(rows.len(), 2);
    // Column name is observable through a subquery projecting it by name.
    let rows = db.query("SELECT g FROM (SELECT * FROM generate_series(1, 2) AS g) AS sub", &[])?;
    assert_eq!(ints(&rows), vec![1, 2]);
    Ok(())
}

#[test]
fn unnest_alias_names_the_column() -> Result<()> {
    let db = create_test_db()?;
    let rows = db.query("SELECT u FROM unnest(ARRAY[7, 8, 9]) AS u", &[])?;
    assert_eq!(ints(&rows), vec![7, 8, 9]);
    Ok(())
}

#[test]
fn alias_in_cross_join_with_a_table() -> Result<()> {
    let db = create_test_db()?;
    db.execute("CREATE TABLE colors (name TEXT)")?;
    db.execute("INSERT INTO colors VALUES ('red'), ('blue')")?;
    let rows = db.query(
        "SELECT c.name, g FROM colors c, generate_series(1, 2) AS g ORDER BY c.name, g",
        &[],
    )?;
    assert_eq!(rows.len(), 4);
    let second: Vec<i64> = rows
        .iter()
        .map(|t| match &t.values[1] {
            Value::Int8(v) => *v,
            Value::Int4(v) => i64::from(*v),
            other => panic!("expected an integer, got {other:?}"),
        })
        .collect();
    assert_eq!(second, vec![1, 2, 1, 2]);
    Ok(())
}

// ---- unchanged forms (pin them so the fix cannot regress them) ----

#[test]
fn explicit_column_list_still_wins_over_alias() -> Result<()> {
    let db = create_test_db()?;
    let rows = db.query("SELECT i FROM generate_series(1, 3) g(i)", &[])?;
    assert_eq!(ints(&rows), vec![1, 2, 3]);
    // With a column list the alias is NOT a column name.
    assert!(db.query("SELECT g FROM generate_series(1, 3) g(i)", &[]).is_err());
    Ok(())
}

#[test]
fn no_alias_keeps_the_function_name_as_column() -> Result<()> {
    let db = create_test_db()?;
    let rows = db.query("SELECT generate_series FROM generate_series(1, 3)", &[])?;
    assert_eq!(ints(&rows), vec![1, 2, 3]);
    let rows = db.query("SELECT * FROM generate_series(1, 10) WHERE generate_series > 8", &[])?;
    assert_eq!(ints(&rows), vec![9, 10]);
    Ok(())
}
