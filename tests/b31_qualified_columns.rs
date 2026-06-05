//! B31 / bug-8: table-qualified column references in SELECT must resolve on the
//! parameterised (extended-protocol) path used by psycopg / SQLAlchemy.
use heliosdb_nano::EmbeddedDatabase;

#[test]
fn b31_qualified_select_via_query_params() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE leads (id TEXT, email TEXT, first_name TEXT)")
        .unwrap();
    db.execute("INSERT INTO leads (id, email, first_name) VALUES ('x', 'e@x.com', 'n')")
        .unwrap();

    // unqualified — known-good
    let r = db.query_params("SELECT id, email FROM leads", &[]).unwrap();
    assert_eq!(r.len(), 1, "unqualified select should return 1 row");

    // qualified — bug-8: currently fails with Column 'leads.id' not found in schema
    let r = db
        .query_params("SELECT leads.id, leads.email FROM leads", &[])
        .expect("qualified select via query_params must succeed");
    assert_eq!(r.len(), 1, "qualified select should return 1 row");
}
