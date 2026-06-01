use std::time::Instant;

use heliosdb_nano::{EmbeddedDatabase, Result, Value};

fn select_values(db: &EmbeddedDatabase, table: &str) -> Result<Vec<Value>> {
    let rows = db.query(&format!("SELECT val, qty, note FROM {} WHERE id = 1", table), &[])?;
    assert_eq!(rows.len(), 1, "expected one row in {table}");
    Ok(rows[0].values.clone())
}

#[test]
fn a11_execute_resolves_excluded_in_set_and_expression() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE a11_simple (id INT4 PRIMARY KEY, val TEXT, qty INT4, note TEXT)")?;
    db.execute("INSERT INTO a11_simple VALUES (1, 'old', 10, 'kept')")?;

    let affected = db.execute(
        "INSERT INTO a11_simple VALUES (1, 'new', 4, 'incoming') \
         ON CONFLICT (id) DO UPDATE SET val = EXCLUDED.val, qty = qty + excluded.qty, note = excluded.note",
    )?;

    assert_eq!(affected, 1);
    assert_eq!(
        select_values(&db, "a11_simple")?,
        vec![
            Value::String("new".to_string()),
            Value::Int4(14),
            Value::String("incoming".to_string()),
        ]
    );
    Ok(())
}

#[test]
fn a11_execute_params_resolves_excluded_in_set_and_expression() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE a11_params (id INT4 PRIMARY KEY, val TEXT, qty INT4, note TEXT)")?;
    db.execute_params(
        "INSERT INTO a11_params (id, val, qty, note) VALUES ($1, $2, $3, $4)",
        &[
            Value::Int4(1),
            Value::String("old".to_string()),
            Value::Int4(10),
            Value::String("kept".to_string()),
        ],
    )?;

    let affected = db.execute_params(
        "INSERT INTO a11_params (id, val, qty, note) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (id) DO UPDATE SET val = excluded.val, qty = qty + excluded.qty, note = EXCLUDED.note",
        &[
            Value::Int4(1),
            Value::String("param-new".to_string()),
            Value::Int4(7),
            Value::String("param-note".to_string()),
        ],
    )?;

    assert_eq!(affected, 1);
    assert_eq!(
        select_values(&db, "a11_params")?,
        vec![
            Value::String("param-new".to_string()),
            Value::Int4(17),
            Value::String("param-note".to_string()),
        ]
    );
    Ok(())
}

#[test]
fn a11_do_update_where_reads_excluded_and_existing_row() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE a11_where (id INT4 PRIMARY KEY, val TEXT, qty INT4, note TEXT)")?;
    db.execute("INSERT INTO a11_where VALUES (1, 'old', 10, 'kept')")?;

    let skipped = db.execute(
        "INSERT INTO a11_where VALUES (1, 'too-low', 5, 'skip') \
         ON CONFLICT (id) DO UPDATE SET val = excluded.val, qty = excluded.qty, note = excluded.note \
         WHERE excluded.qty > qty",
    )?;
    assert_eq!(skipped, 0, "false conflict WHERE predicate must skip the update");
    assert_eq!(
        select_values(&db, "a11_where")?,
        vec![
            Value::String("old".to_string()),
            Value::Int4(10),
            Value::String("kept".to_string()),
        ]
    );

    let updated = db.execute(
        "INSERT INTO a11_where VALUES (1, 'higher', 15, 'apply') \
         ON CONFLICT (id) DO UPDATE SET val = excluded.val, qty = excluded.qty, note = excluded.note \
         WHERE excluded.qty > qty",
    )?;
    assert_eq!(updated, 1);
    assert_eq!(
        select_values(&db, "a11_where")?,
        vec![
            Value::String("higher".to_string()),
            Value::Int4(15),
            Value::String("apply".to_string()),
        ]
    );
    Ok(())
}

#[test]
fn a11_execute_params_returning_respects_excluded_where() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE a11_returning (id INT4 PRIMARY KEY, val TEXT, qty INT4, note TEXT)")?;
    db.execute("INSERT INTO a11_returning VALUES (1, 'old', 10, 'kept')")?;

    let (skipped, skipped_rows) = db.execute_params_returning(
        "INSERT INTO a11_returning (id, val, qty, note) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (id) DO UPDATE SET val = excluded.val, qty = excluded.qty, note = excluded.note \
         WHERE excluded.qty > qty RETURNING val, qty, note",
        &[
            Value::Int4(1),
            Value::String("too-low".to_string()),
            Value::Int4(5),
            Value::String("skip".to_string()),
        ],
    )?;
    assert_eq!(skipped, 0);
    assert!(skipped_rows.is_empty());
    assert_eq!(
        select_values(&db, "a11_returning")?,
        vec![
            Value::String("old".to_string()),
            Value::Int4(10),
            Value::String("kept".to_string()),
        ]
    );

    let (updated, rows) = db.execute_params_returning(
        "INSERT INTO a11_returning (id, val, qty, note) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (id) DO UPDATE SET val = excluded.val, qty = excluded.qty, note = excluded.note \
         WHERE excluded.qty > qty RETURNING val, qty, note",
        &[
            Value::Int4(1),
            Value::String("higher".to_string()),
            Value::Int4(15),
            Value::String("apply".to_string()),
        ],
    )?;
    assert_eq!(updated, 1);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].values,
        vec![
            Value::String("higher".to_string()),
            Value::Int4(15),
            Value::String("apply".to_string()),
        ]
    );
    Ok(())
}

#[test]
fn a11_parameterized_excluded_where_stress() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE a11_stress (id INT4 PRIMARY KEY, val TEXT, qty INT4, note TEXT)")?;
    db.execute("INSERT INTO a11_stress VALUES (1, 'seed', 0, 'seed')")?;

    let started = Instant::now();
    let mut affected = 0_u64;
    for i in 1..=200 {
        let proposed_qty = if i % 2 == 0 { i } else { -i };
        affected += db.execute_params(
            "INSERT INTO a11_stress (id, val, qty, note) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (id) DO UPDATE SET val = excluded.val, qty = excluded.qty, note = excluded.note \
             WHERE excluded.qty > qty",
            &[
                Value::Int4(1),
                Value::String(format!("candidate-{i}")),
                Value::Int4(proposed_qty),
                Value::String(format!("note-{i}")),
            ],
        )?;
    }
    let elapsed = started.elapsed();

    assert_eq!(affected, 100, "only increasing even proposals should update");
    assert_eq!(
        select_values(&db, "a11_stress")?,
        vec![
            Value::String("candidate-200".to_string()),
            Value::Int4(200),
            Value::String("note-200".to_string()),
        ]
    );
    eprintln!(
        "A11 stress: 200 parameterized upserts with excluded WHERE in {:?}",
        elapsed
    );
    Ok(())
}
