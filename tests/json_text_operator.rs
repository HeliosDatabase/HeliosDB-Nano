//! Regression coverage for HELIOSDB_GAPS (Markon) A2.
//!
//! `->` / `->>` on a TEXT column holding JSON used to error
//! "Left operand of -> must be JSON, got String", forcing an explicit
//! `::json` cast. Many schemas store JSON-in-TEXT; the operators must accept a
//! TEXT operand and parse it as JSON.

use heliosdb_nano::{EmbeddedDatabase, Result, Tuple, Value};

fn first_string(rows: &[Tuple]) -> Option<String> {
    match rows.first().and_then(|r| r.values.first()) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Json(s)) => Some(s.clone()),
        _ => None,
    }
}

fn seed(db: &EmbeddedDatabase) -> Result<()> {
    db.execute("CREATE TABLE leads (id INT PRIMARY KEY, profile TEXT)")?;
    db.execute(
        "INSERT INTO leads VALUES (1, '{\"linkedin_url\": \"https://x.test/in/a\", \"score\": 7}')",
    )?;
    db.execute(
        "INSERT INTO leads VALUES (2, '{\"linkedin_url\": \"https://x.test/in/b\", \"score\": 3}')",
    )?;
    Ok(())
}

#[test]
fn json_text_operator_extracts_without_cast() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db)?;

    let rows = db.query(
        "SELECT profile->>'linkedin_url' FROM leads WHERE id = 1",
        &[],
    )?;
    assert_eq!(first_string(&rows).as_deref(), Some("https://x.test/in/a"));

    // The ::json cast must keep working too (no regression).
    let rows_cast = db.query(
        "SELECT profile::json->>'linkedin_url' FROM leads WHERE id = 1",
        &[],
    )?;
    assert_eq!(first_string(&rows_cast).as_deref(), Some("https://x.test/in/a"));
    Ok(())
}

#[test]
fn json_text_operator_filters_in_where() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db)?;

    // The scan that Markon's _find_lead_by_url wants to replace its LIKE scan.
    let rows = db.query(
        "SELECT id FROM leads WHERE profile->>'linkedin_url' = 'https://x.test/in/b'",
        &[],
    )?;
    let ids: Vec<i32> = rows
        .iter()
        .map(|r| match r.values.first() {
            Some(Value::Int4(v)) => *v,
            Some(Value::Int8(v)) => *v as i32,
            other => panic!("expected id, got {other:?}"),
        })
        .collect();
    assert_eq!(ids, vec![2]);
    Ok(())
}

#[test]
fn json_text_operator_get_returns_json() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db)?;

    // `->` (not `->>`) yields a JSON scalar; for an int field that's the text "7".
    let rows = db.query("SELECT profile->'score' FROM leads WHERE id = 1", &[])?;
    assert_eq!(first_string(&rows).as_deref(), Some("7"));
    Ok(())
}
