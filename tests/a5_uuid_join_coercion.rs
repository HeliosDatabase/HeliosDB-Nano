use heliosdb_nano::{EmbeddedDatabase, Value};

fn count_value(value: &Value) -> i64 {
    match value {
        Value::Int2(n) => i64::from(*n),
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        other => panic!("expected integer count, got {other:?}"),
    }
}

fn query_count(db: &EmbeddedDatabase, sql: &str) -> i64 {
    let rows = db.query(sql, &[]).unwrap();
    assert_eq!(rows.len(), 1);
    count_value(&rows[0].values[0])
}

fn uuid_values(db: &EmbeddedDatabase, sql: &str) -> Vec<uuid::Uuid> {
    db.query(sql, &[])
        .unwrap()
        .into_iter()
        .map(|row| match row.values.first() {
            Some(Value::Uuid(value)) => *value,
            other => panic!("expected UUID first column, got {other:?}"),
        })
        .collect()
}

#[test]
fn uuid_text_join_and_where_literal_use_same_coercion_rules() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let expected_uuid = uuid::Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
    db.execute("CREATE TABLE a5b_u_col (id UUID)").unwrap();
    db.execute("CREATE TABLE a5b_u_col2 (id UUID)").unwrap();
    db.execute("CREATE TABLE a5b_t_col (id TEXT)").unwrap();

    db.execute("INSERT INTO a5b_u_col VALUES ('11111111-2222-3333-4444-555555555555'::uuid)")
        .unwrap();
    db.execute("INSERT INTO a5b_u_col2 VALUES ('11111111-2222-3333-4444-555555555555'::uuid)")
        .unwrap();
    db.execute("INSERT INTO a5b_t_col VALUES ('11111111-2222-3333-4444-555555555555')")
        .unwrap();

    assert_eq!(
        query_count(&db, "SELECT count(*) FROM a5b_u_col u JOIN a5b_t_col t ON u.id = t.id"),
        1
    );
    assert_eq!(
        query_count(&db, "SELECT count(*) FROM a5b_t_col t JOIN a5b_u_col u ON t.id = u.id"),
        1
    );
    assert_eq!(
        query_count(
            &db,
            "SELECT count(*) FROM a5b_u_col WHERE id = '11111111-2222-3333-4444-555555555555'"
        ),
        1
    );
    assert_eq!(
        query_count(
            &db,
            "SELECT count(*) FROM a5b_u_col WHERE id = '11111111-2222-3333-4444-555555555555'::uuid"
        ),
        1
    );
    assert_eq!(
        query_count(
            &db,
            "SELECT count(*) FROM a5b_t_col WHERE id = '11111111-2222-3333-4444-555555555555'"
        ),
        1
    );
    assert_eq!(
        uuid_values(
            &db,
            "SELECT id FROM a5b_u_col WHERE id = '11111111-2222-3333-4444-555555555555'"
        ),
        vec![expected_uuid]
    );
    assert_eq!(
        query_count(&db, "SELECT count(*) FROM a5b_u_col u JOIN a5b_u_col2 v ON u.id = v.id"),
        1
    );
}
