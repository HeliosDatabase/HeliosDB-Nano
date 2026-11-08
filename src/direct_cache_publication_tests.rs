//! Real direct writes must reject a reader publishing old rows after a clearing reader.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use super::*;
use std::sync::{mpsc, Arc};
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
enum Writer {
    Insert,
    Batch,
    Payload,
}

fn projected(rows: &[Tuple]) -> Vec<Vec<Value>> {
    rows.iter().map(|row| row.values.clone()).collect()
}

fn direct_write(db: &EmbeddedDatabase, writer: Writer, schema: &Schema, old: &Tuple) {
    let tuple = |id, payload: &str| {
        Tuple::new(vec![
            Value::Int4(id),
            Value::String(payload.into()),
            Value::Vector(vec![id as f32, 0.0, 0.0]),
        ])
    };
    match writer {
        Writer::Insert => {
            db.storage.insert_tuple("t", tuple(3, "new")).unwrap();
        }
        Writer::Batch => {
            let fresh = db.storage.next_row_id_volatile("t");
            assert_eq!(
                db.storage
                    .insert_prepared_tuples_fast_batch("t", vec![(fresh, tuple(3, "new"))], schema)
                    .unwrap(),
                1
            );
        }
        Writer::Payload => {
            let row = old.row_id.unwrap();
            let _row = db.storage.lock_row_for_fast_write("t", row).unwrap();
            assert_eq!(
                db.storage
                    .update_tuple_fast_no_index("t", row, tuple(1, "changed"), schema, Some(old))
                    .unwrap(),
                1
            );
        }
    }
}

fn exercise(vector: bool) {
    let sql = "SELECT id, payload FROM t WHERE id > 0 ORDER BY id";
    let mut cells = 0;
    for history in [false, true] {
        for writer in [Writer::Insert, Writer::Batch, Writer::Payload] {
            let mut config = Config::in_memory();
            config.storage.time_travel_enabled = history;
            config.storage.logical_wal_per_statement = false;
            let db = EmbeddedDatabase::with_config(config).unwrap();
            db.execute("CREATE TABLE t (id INT PRIMARY KEY, payload TEXT, embedding VECTOR(3))")
                .unwrap();
            db.execute("INSERT INTO t VALUES (1, 'one', '[1,0,0]'), (2, 'two', '[2,0,0]')")
                .unwrap();
            if vector {
                db.execute("CREATE INDEX t_vec ON t USING hnsw (embedding)").unwrap();
            }
            let schema = db.storage.catalog().get_table_schema("t").unwrap();
            let old = db
                .storage
                .scan_table_with_schema("t", &schema)
                .unwrap()
                .into_iter()
                .find(|row| row.values[0] == Value::Int4(1))
                .unwrap();
            let original = vec![
                vec![Value::Int4(1), Value::String("one".into())],
                vec![Value::Int4(2), Value::String("two".into())],
            ];
            let mut expected = original.clone();
            match writer {
                Writer::Insert | Writer::Batch => expected.push(vec![Value::Int4(3), Value::String("new".into())]),
                Writer::Payload => expected[0][1] = Value::String("changed".into()),
            }
            for _ in 0..3 {
                assert_eq!(projected(&db.query(sql, &[]).unwrap()), original);
            }
            assert!(
                db.result_cache.get(sql).is_some(),
                "the real repeated SELECT populated shared rows"
            );
            assert!(
                db.hot_cached_query_result(sql).is_some(),
                "the actual hot cache is warm"
            );
            std::thread::scope(|scope| {
                // Keep senders inside this scope closure: an assertion failure
                // disconnects the parked child before scope joins it.
                let (ready_tx, ready_rx) = mpsc::sync_channel(0);
                let (resume_tx, resume_rx) = mpsc::sync_channel(0);
                let reader_db = &db;
                let reader_expected = expected.clone();
                let reader_original = original.clone();
                let reader = scope.spawn(move || {
                    let retained = reader_db.result_cache.get(sql).expect("real old cache entry");
                    assert_eq!(projected(retained.rows.as_ref()), reader_original);
                    let before = retained.epoch;
                    // Model the existing producer's early epoch check, then
                    // suspend before its map insert. The real writer below
                    // must complete entirely inside this handshake (ABA case).
                    assert_eq!(before, reader_db.result_cache_epoch());
                    ready_tx.send(before).unwrap();
                    resume_rx
                        .recv_timeout(Duration::from_secs(20))
                        .expect("writer/clearing reader completed");
                    assert_ne!(
                        before,
                        reader_db.result_cache_epoch(),
                        "actual storage writer advanced data epoch"
                    );

                    // The normal publication entrypoint must reject this stale
                    // computation instead of replacing the current warm entry.
                    reader_db.cache_query_result(sql, retained.rows.as_ref(), before);
                    let current = reader_db
                        .result_cache
                        .get(sql)
                        .expect("clearing reader's current entry survives");
                    assert_ne!(current.epoch, before);
                    assert_eq!(projected(current.rows.as_ref()), reader_expected);

                    // Existing private cache seam: force an insert which had
                    // already passed its preflight before the writer, after the
                    // clearing reader consumed that writer's epoch. Retain the
                    // ORIGINAL entry/epoch; never relabel stale rows as current.
                    reader_db.result_cache.put(sql.to_string(), Arc::clone(&retained));
                    *reader_db.hot_result_cache_entry.write() = Some((sql.to_string(), retained));
                    reader_db
                        .result_cache_nonempty
                        .store(true, std::sync::atomic::Ordering::Release);
                    assert!(
                        reader_db.hot_cached_query_result(sql).is_none(),
                        "late old hot rows must miss"
                    );
                    *reader_db.hot_result_cache_entry.write() = None;
                    assert!(
                        reader_db.cached_query_result(sql).is_none(),
                        "late old shared rows must miss"
                    );
                    let fresh = reader_db.query(sql, &[]).unwrap();
                    assert_eq!(projected(&fresh), reader_expected);
                    reader_db.cache_query_result(sql, &fresh, reader_db.result_cache_epoch());
                    *reader_db.hot_result_cache_entry.write() = None;
                    let shared = reader_db
                        .cached_query_result(sql)
                        .expect("fresh shared entry remains useful");
                    let hot = reader_db
                        .hot_cached_query_result(sql)
                        .expect("fresh shared-to-hot promotion");
                    assert!(Arc::ptr_eq(&shared, &hot));
                });
                let before = ready_rx
                    .recv_timeout(Duration::from_secs(20))
                    .expect("old reader captured its real entry");
                direct_write(&db, writer, &schema, &old);
                assert!(
                    !db.storage.has_uncommitted_writes(),
                    "writer fully completed; census is not hiding stale cache"
                );
                assert_ne!(before, db.result_cache_epoch());
                let fresh = db.query(sql, &[]).unwrap();
                assert_eq!(
                    projected(&fresh),
                    expected,
                    "{writer:?}, vector={vector}, history={history}"
                );
                db.cache_query_result(sql, &fresh, db.result_cache_epoch());
                assert!(
                    db.hot_cached_query_result(sql).is_some(),
                    "clearing reader warmed current rows"
                );
                resume_tx.send(()).unwrap();
                reader.join().unwrap();
            });
            assert_eq!(projected(&db.query(sql, &[]).unwrap()), expected);
            cells += 1;
        }
    }
    assert_eq!(cells, 6);
    eprintln!("direct-cache-late-publication vector={vector} cells=6");
}

#[test]
fn late_direct_non_vector_publishers_miss_after_the_writer_and_clearing_reader() {
    exercise(false);
}

#[test]
fn late_direct_vector_publishers_miss_after_the_writer_and_clearing_reader() {
    exercise(true);
}
