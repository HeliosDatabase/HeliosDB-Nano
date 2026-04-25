//! Direct-API repro of the UUID WHERE-clause bug from #205.
//! Bypasses the wire protocol so we can pin the bug to the
//! storage / planner / evaluator without any tokio_postgres
//! interference.

use heliosdb_nano::{EmbeddedDatabase, Value};

#[test]
fn int_pk_round_trips_through_where() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE t (id INT4 PRIMARY KEY, name TEXT)").unwrap();
    db.execute("INSERT INTO t VALUES (42, 'foo')").unwrap();
    let n = db.query("SELECT id FROM t WHERE id = 42", &[]).unwrap();
    assert_eq!(n.len(), 1, "INT4 PK round-trip works");
}

#[test]
fn uuid_pk_round_trips_through_where() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE t (id UUID PRIMARY KEY, name TEXT)").unwrap();
    let uuid_str = uuid::Uuid::new_v4().to_string();
    db.execute(&format!(
        "INSERT INTO t VALUES ('{uuid_str}', 'foo')"
    ))
    .unwrap();

    // SELECT * with no WHERE — should see the row.
    let all = db.query("SELECT id FROM t", &[]).unwrap();
    assert_eq!(all.len(), 1, "table empty after INSERT");
    eprintln!("[probe] stored value type: {:?}", all[0].values.first());

    // SELECT … WHERE id = '<uuid>' — the broken case.
    let filtered = db
        .query(
            &format!("SELECT id FROM t WHERE id = '{uuid_str}'"),
            &[],
        )
        .unwrap();
    eprintln!("[probe] SELECT id WHERE id rows = {}", filtered.len());

    // Also try SELECT * which routes through try_fast_select.
    let star = db
        .query(
            &format!("SELECT * FROM t WHERE id = '{uuid_str}'"),
            &[],
        )
        .unwrap();
    eprintln!("[probe] SELECT *  WHERE id rows = {}", star.len());
    assert_eq!(
        filtered.len(),
        1,
        "WHERE id = '{uuid_str}' missed the row — UUID/string \
         coercion bug still present (root cause of #205)."
    );

    // Parameterised form should also match.
    let p_uuid: uuid::Uuid = uuid_str.parse().unwrap();
    let parameterised = db
        .query_params(
            "SELECT id FROM t WHERE id = $1",
            &[Value::Uuid(p_uuid)],
        )
        .unwrap();
    assert_eq!(parameterised.len(), 1);

    let parameterised_str = db
        .query_params(
            "SELECT id FROM t WHERE id = $1",
            &[Value::String(uuid_str.clone())],
        )
        .unwrap();
    assert_eq!(parameterised_str.len(), 1, "string-typed param mismatch");
}
