use heliosdb_nano::{EmbeddedDatabase, Result, Value};

#[test]
fn predict_and_infer_sql_surface_is_registered() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    let rows = db.query("SELECT predict('sum', ARRAY[1.0, 2.5, 3.5])", &[])?;
    assert_eq!(rows[0].values[0], Value::Float8(7.0));

    let rows = db.query("SELECT infer('mean', ARRAY[2.0, 4.0, 6.0])", &[])?;
    let Value::Json(body) = &rows[0].values[0] else {
        panic!("infer should return JSON");
    };
    let json: serde_json::Value = serde_json::from_str(body).expect("valid json");
    assert_eq!(json["runtime"], "builtin");
    assert_eq!(json["input_dimension"], 3);
    assert_eq!(json["prediction"], 4.0);
    Ok(())
}

#[test]
fn self_drive_plan_returns_safe_preview_with_recommendation() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let rows = db.query(
        "SELECT heliosdb_self_drive_plan('SELECT * FROM events WHERE tenant_id = 42')",
        &[],
    )?;
    let Value::Json(body) = &rows[0].values[0] else {
        panic!("self-drive plan should return JSON");
    };
    let json: serde_json::Value = serde_json::from_str(body).expect("valid json");
    assert_eq!(json["mode"], "preview");
    assert_eq!(json["auto_promote"], false);
    assert_eq!(json["recommendations"][0]["table"], "events");
    assert_eq!(json["recommendations"][0]["columns"][0], "tenant_id");
    Ok(())
}
