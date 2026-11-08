//! Mixed public storage/SQL calls must not retain an older cached result.
#![cfg(feature = "vector-search")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{Config, EmbeddedDatabase, Tuple, Value};

#[derive(Clone, Copy, Debug)]
enum Route {
    Insert,
    FastInsert,
    BatchInsert,
    KeyUpdate,
    PayloadUpdate,
    Delete,
    PkDelete,
}

fn rows(db: &EmbeddedDatabase, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql, &[]).unwrap().into_iter().map(|row| row.values).collect()
}

fn tuple(pk_only: bool, id: i32, payload: &str) -> Tuple {
    let mut values = vec![Value::Int4(id)];
    if !pk_only {
        values.push(Value::Int4(id * 10));
    }
    values.push(Value::String(payload.into()));
    values.push(Value::Vector(vec![id as f32, 0.0, 0.0]));
    Tuple::new(values)
}

fn exercise(vector: bool) {
    let routes = [
        Route::Insert,
        Route::FastInsert,
        Route::BatchInsert,
        Route::KeyUpdate,
        Route::PayloadUpdate,
        Route::Delete,
        Route::PkDelete,
    ];
    let mut cells = 0;
    for history in [false, true] {
        for route in routes {
            let dir = tempfile::tempdir().unwrap();
            let mut config = Config::default();
            config.storage.path = Some(dir.path().into());
            config.storage.memory_only = false;
            config.storage.time_travel_enabled = history;
            config.storage.logical_wal_per_statement = false;
            config.storage.version_gc_interval_secs = Some(0);
            let db = EmbeddedDatabase::with_config(config).unwrap();
            let pk_only = matches!(route, Route::PkDelete);
            let secondary = if pk_only { "" } else { "u INT UNIQUE," };
            db.execute(&format!(
                "CREATE TABLE t (id INT PRIMARY KEY,{secondary}payload TEXT, embedding VECTOR(3))"
            ))
            .unwrap();
            let secondary = if pk_only { "" } else { "10," };
            db.execute(&format!("INSERT INTO t VALUES(1,{secondary}'one','[1,0,0]')"))
                .unwrap();
            let secondary = if pk_only { "" } else { "20," };
            db.execute(&format!("INSERT INTO t VALUES(2,{secondary}'two','[2,0,0]')"))
                .unwrap();
            if vector {
                db.execute("CREATE INDEX embedding_idx ON t USING hnsw (embedding)")
                    .unwrap();
            }
            let scan = "SELECT id,payload FROM t ORDER BY id";
            let count = "SELECT COUNT(*) FROM t";
            let point = "SELECT id,payload FROM t WHERE id=1";
            let original = vec![
                vec![Value::Int4(1), Value::String("one".into())],
                vec![Value::Int4(2), Value::String("two".into())],
            ];
            // Repeated identical SQL admits both the shared and fast cache
            // paths. No SQL DML/local cache invalidation follows this warm-up.
            for (sql, expected) in [
                (scan, original.clone()),
                (count, vec![vec![Value::Int8(2)]]),
                (point, vec![original[0].clone()]),
            ] {
                for _ in 0..3 {
                    assert_eq!(rows(&db, sql), expected);
                }
            }
            let schema = db.storage.catalog().get_table_schema("t").unwrap();
            let old = db.query("SELECT * FROM t WHERE id=1", &[]).unwrap().remove(0);
            let row = old.row_id.unwrap();
            let mut expected = original.clone();
            {
                let _row = db.storage.lock_row_for_fast_write("t", row).unwrap();
                match route {
                    Route::Insert => {
                        db.storage.insert_tuple("t", tuple(false, 3, "new")).unwrap();
                        expected.push(vec![Value::Int4(3), Value::String("new".into())]);
                    }
                    Route::FastInsert => {
                        db.storage
                            .insert_tuple_fast("t", tuple(false, 3, "new"), &schema)
                            .unwrap();
                        expected.push(vec![Value::Int4(3), Value::String("new".into())]);
                    }
                    Route::BatchInsert => {
                        let fresh = db.storage.next_row_id_volatile("t");
                        db.storage
                            .insert_prepared_tuples_fast_batch("t", vec![(fresh, tuple(false, 3, "new"))], &schema)
                            .unwrap();
                        expected.push(vec![Value::Int4(3), Value::String("new".into())]);
                    }
                    Route::KeyUpdate => {
                        db.storage
                            .update_tuple_fast("t", row, tuple(false, 11, "moved"), &old, &schema)
                            .unwrap();
                        expected = vec![
                            original[1].clone(),
                            vec![Value::Int4(11), Value::String("moved".into())],
                        ];
                    }
                    Route::PayloadUpdate => {
                        db.storage
                            .update_tuple_fast_no_index("t", row, tuple(false, 1, "changed"), &schema, Some(&old))
                            .unwrap();
                        expected[0][1] = Value::String("changed".into());
                    }
                    Route::Delete => {
                        db.storage.delete_tuple_fast("t", row, &old, &schema).unwrap();
                        expected.remove(0);
                    }
                    Route::PkDelete => {
                        let key = heliosdb_nano::storage::ArtIndexManager::encode_key(&[Value::Int4(1)]);
                        db.storage
                            .delete_tuple_fast_pk_only("t", row, &key, &Value::Int4(1))
                            .unwrap();
                        expected.remove(0);
                    }
                }
            }
            assert_eq!(
                rows(&db, scan),
                expected,
                "{route:?}, vector={vector}, history={history}"
            );
            assert_eq!(rows(&db, count), vec![vec![Value::Int8(expected.len() as i64)]]);
            let expected_point: Vec<_> = expected
                .iter()
                .filter(|row| row[0] == Value::Int4(1))
                .cloned()
                .collect();
            assert_eq!(rows(&db, point), expected_point);
            // Warm the new values again: repeated reads must remain current.
            for _ in 0..3 {
                assert_eq!(rows(&db, scan), expected);
            }
            cells += 1;
        }
    }
    assert_eq!(cells, 14);
    eprintln!("direct-row-cache vector={vector} cells={cells}");
}

#[test]
fn direct_non_vector_writers_publish_to_warmed_sql_caches() {
    exercise(false);
}

#[test]
fn direct_vector_writers_publish_to_warmed_sql_caches() {
    exercise(true);
}
