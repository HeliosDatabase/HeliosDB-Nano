use heliosdb_nano::{EmbeddedDatabase, Result, Value};

#[test]
fn execute_accepts_multi_row_values_for_user_tables() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE items (id INT4 PRIMARY KEY, name TEXT NOT NULL, qty INT4 DEFAULT 7)")?;

    let affected = db.execute("INSERT INTO items (id, name) VALUES (1, 'alpha'), (2, 'beta'), (3, 'gamma')")?;
    assert_eq!(affected, 3);

    let rows = db.query("SELECT id, name, qty FROM items ORDER BY id", &[])?;
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0].values,
        vec![Value::Int4(1), Value::String("alpha".to_string()), Value::Int4(7),]
    );
    assert_eq!(
        rows[2].values,
        vec![Value::Int4(3), Value::String("gamma".to_string()), Value::Int4(7),]
    );
    Ok(())
}

#[test]
fn query_surface_routes_multi_row_dml_to_write_executor() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE q_items (id INT4 PRIMARY KEY, name TEXT)")?;

    let rows = db.query("INSERT INTO q_items (id, name) VALUES (1, 'one'), (2, 'two')", &[])?;
    assert!(rows.is_empty());

    let rows = db.query("SELECT id, name FROM q_items ORDER BY id", &[])?;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].values, vec![Value::Int4(2), Value::String("two".to_string())]);
    Ok(())
}

#[test]
fn query_params_preserves_multi_row_on_conflict_returning() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE up_items (id INT4 PRIMARY KEY, name TEXT)")?;
    db.execute("INSERT INTO up_items VALUES (1, 'old')")?;

    let rows = db.query_params(
        "INSERT INTO up_items (id, name) VALUES ($1, $2), ($3, $4) \
         ON CONFLICT DO UPDATE SET name = EXCLUDED.name RETURNING id, name",
        &[
            Value::Int4(1),
            Value::String("new".to_string()),
            Value::Int4(2),
            Value::String("two".to_string()),
        ],
    )?;

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values, vec![Value::Int4(1), Value::String("new".to_string())]);
    assert_eq!(rows[1].values, vec![Value::Int4(2), Value::String("two".to_string())]);

    let rows = db.query("SELECT id, name FROM up_items ORDER BY id", &[])?;
    assert_eq!(rows[0].values, vec![Value::Int4(1), Value::String("new".to_string())]);
    assert_eq!(rows[1].values, vec![Value::Int4(2), Value::String("two".to_string())]);
    Ok(())
}

#[test]
fn execute_batch_accepts_multibyte_string_literals() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let affected = db.execute_batch(&[
        "CREATE TABLE unicode_items (id INT4 PRIMARY KEY, label TEXT)",
        "INSERT INTO unicode_items VALUES (1, 'a—b')",
    ])?;
    assert_eq!(affected, 2);

    let rows = db.query("SELECT label FROM unicode_items WHERE id = 1", &[])?;
    assert_eq!(rows[0].values[0], Value::String("a—b".to_string()));
    Ok(())
}
