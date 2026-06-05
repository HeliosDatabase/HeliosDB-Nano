//! A3 regression coverage: scalar secondary indexes must be populated on
//! CREATE INDEX and usable for equality point lookups.

use heliosdb_nano::{sql::SystemViewRegistry, storage::ArtIndexManager};
use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};
use tempfile::TempDir;

fn int_values(rows: &[Tuple]) -> Vec<i64> {
    rows.iter()
        .map(|row| match row.values.first() {
            Some(Value::Int2(value)) => i64::from(*value),
            Some(Value::Int4(value)) => i64::from(*value),
            Some(Value::Int8(value)) => *value,
            other => panic!("expected integer first column, got {other:?}"),
        })
        .collect()
}

fn index_row_count(db: &EmbeddedDatabase, index_name: &str, value: Value) -> usize {
    let key = ArtIndexManager::encode_key(&[value]);
    db.storage.art_indexes().index_get_all(index_name, &key).len()
}

#[test]
fn create_index_backfills_existing_rows_and_tracks_mutations() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE a3_users (id INT PRIMARY KEY, email TEXT, bucket INT)")
        .unwrap();
    db.execute("INSERT INTO a3_users VALUES (1, 'a@example.com', 1)")
        .unwrap();
    db.execute("INSERT INTO a3_users VALUES (2, 'b@example.com', 1)")
        .unwrap();
    db.execute("INSERT INTO a3_users VALUES (3, 'b@example.com', 2)")
        .unwrap();

    db.execute("CREATE INDEX idx_a3_users_email ON a3_users(email)")
        .unwrap();
    assert_eq!(
        index_row_count(&db, "idx_a3_users_email", Value::String("b@example.com".into())),
        2,
        "CREATE INDEX must backfill rows inserted before the index existed"
    );

    let rows = db
        .query(
            "SELECT id FROM a3_users WHERE email = 'b@example.com' AND bucket = 2 ORDER BY id",
            &[],
        )
        .unwrap();
    assert_eq!(int_values(&rows), vec![3], "residual predicates must still be applied");

    db.execute("INSERT INTO a3_users VALUES (4, 'b@example.com', 2)")
        .unwrap();
    assert_eq!(
        index_row_count(&db, "idx_a3_users_email", Value::String("b@example.com".into())),
        3,
        "manual ART index must receive post-CREATE inserts"
    );

    db.execute("UPDATE a3_users SET email = 'c@example.com' WHERE id = 2")
        .unwrap();
    assert_eq!(
        index_row_count(&db, "idx_a3_users_email", Value::String("b@example.com".into())),
        2,
        "manual ART index must drop old keys on UPDATE"
    );
    assert_eq!(
        index_row_count(&db, "idx_a3_users_email", Value::String("c@example.com".into())),
        1,
        "manual ART index must add new keys on UPDATE"
    );

    db.execute("DELETE FROM a3_users WHERE id = 3").unwrap();
    assert_eq!(
        index_row_count(&db, "idx_a3_users_email", Value::String("b@example.com".into())),
        1,
        "manual ART index must remove deleted rows"
    );

    let rows = db
        .query_params(
            "SELECT id FROM a3_users WHERE email = $1 ORDER BY id",
            &[Value::String("b@example.com".into())],
        )
        .unwrap();
    assert_eq!(int_values(&rows), vec![4]);
}

#[test]
fn indexed_equality_lookup_coerces_numeric_parameter_to_column_type() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE a3_events (id INT PRIMARY KEY, account_id BIGINT, label TEXT)")
        .unwrap();
    db.execute("INSERT INTO a3_events VALUES (1, 42, 'first')").unwrap();
    db.execute("INSERT INTO a3_events VALUES (2, 42, 'second')").unwrap();
    db.execute("INSERT INTO a3_events VALUES (3, 7, 'other')").unwrap();

    db.execute("CREATE INDEX idx_a3_events_account ON a3_events(account_id)")
        .unwrap();
    assert_eq!(
        index_row_count(&db, "idx_a3_events_account", Value::Int8(42)),
        2,
        "BIGINT secondary-index keys must be encoded as Int8"
    );

    let rows = db
        .query_params(
            "SELECT id FROM a3_events WHERE account_id = $1 ORDER BY id",
            &[Value::Int4(42)],
        )
        .unwrap();
    assert_eq!(int_values(&rows), vec![1, 2]);
}

#[test]
fn manual_secondary_index_definition_survives_reopen_and_uuid_lookup_uses_art() {
    let temp = TempDir::new().unwrap();
    let uuid = uuid::Uuid::new_v4();

    {
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        db.execute("CREATE TABLE a3_uuid_files (row_no INT PRIMARY KEY, id UUID, path TEXT)")
            .unwrap();
        db.execute(&format!("INSERT INTO a3_uuid_files VALUES (1, '{}', 'target')", uuid))
            .unwrap();
        db.execute("CREATE INDEX idx_a3_uuid_files_id ON a3_uuid_files(id)")
            .unwrap();
        assert_eq!(
            index_row_count(&db, "idx_a3_uuid_files_id", Value::Uuid(uuid)),
            1,
            "UUID secondary index must be populated before close"
        );
    }

    let db = EmbeddedDatabase::new(temp.path()).unwrap();
    assert_eq!(
        index_row_count(&db, "idx_a3_uuid_files_id", Value::Uuid(uuid)),
        1,
        "manual secondary index must be rebuilt from persisted metadata on reopen"
    );

    let indexes = SystemViewRegistry::new().execute("pg_indexes", &db.storage).unwrap();
    assert!(
        indexes.iter().any(|row| matches!(
            row.values.get(2),
            Some(Value::String(name)) if name == "idx_a3_uuid_files_id"
        )),
        "pg_indexes must expose persisted secondary indexes after reopen"
    );

    let rows = db
        .query_params(
            "SELECT path FROM a3_uuid_files WHERE id = $1::uuid",
            &[Value::String(uuid.to_string())],
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values.first(), Some(&Value::String("target".into())));
}
