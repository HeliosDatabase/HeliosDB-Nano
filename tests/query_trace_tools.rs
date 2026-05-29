use heliosdb_nano::{EmbeddedDatabase, Value};

#[test]
fn query_trace_report_records_public_entrypoints() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    let empty = db.query("SHOW helios.trace_report", &[]).unwrap();
    assert!(matches!(
        empty[0].values.first(),
        Some(Value::String(s)) if s.contains("No query traces recorded")
    ));

    db.query("SET helios.trace_queries = on", &[]).unwrap();
    db.execute("CREATE TABLE trace_tool (id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    db.execute("INSERT INTO trace_tool VALUES (1, 10)").unwrap();
    let rows = db.query("SELECT * FROM trace_tool WHERE id = 1", &[]).unwrap();
    assert_eq!(rows.len(), 1);

    let report = db.query("SHOW helios.trace_report", &[]).unwrap();
    let Some(Value::String(report)) = report[0].values.first() else {
        panic!("trace report should be a string");
    };
    assert!(report.contains("Nano Query Trace Report"));
    assert!(report.contains("Traced queries:"));
    assert!(report.contains("SELECT * FROM trace_tool WHERE id = 1"));

    db.query("SHOW helios.trace_reset", &[]).unwrap();
    let reset = db.query("SHOW helios.trace_report", &[]).unwrap();
    assert!(matches!(
        reset[0].values.first(),
        Some(Value::String(s)) if s.contains("No query traces recorded")
    ));
}
