//! A transaction preserves a pre-existing unversioned row's history, including
//! after cache eviction. Bulk-loaded and older stores may have these rows.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};

fn values(rows: Vec<Tuple>) -> Vec<Vec<Value>> {
    rows.into_iter().map(|row| row.values).collect()
}

#[test]
fn a_transaction_keeps_the_preimage_of_an_unversioned_row() {
    for params in [false, true] {
        for delete in [false, true] {
            let db = EmbeddedDatabase::new_in_memory().unwrap();
            db.execute("CREATE TABLE history_row (id INT, v INT)").unwrap();
            let original = vec![Value::Int4(1), Value::Int4(10)];
            let image = bincode::serialize(&Tuple::new(original.clone())).unwrap();
            db.storage
                .direct_bulk_load("history_row", [(1, image)], 100, true)
                .unwrap();
            let anchor = db.storage.current_timestamp();
            let before = values(db.storage.scan_table_at_snapshot("history_row", anchor).unwrap());
            assert_eq!(
                before,
                vec![original.clone()],
                "the historical row exists before the writer"
            );

            db.execute("BEGIN").unwrap();
            let sql = if delete {
                "DELETE FROM history_row WHERE v = 10"
            } else {
                "UPDATE history_row SET v = 20 WHERE v = 10"
            };
            let changed = if params {
                db.execute_params(sql, &[]).unwrap()
            } else {
                db.execute(sql).unwrap()
            };
            assert_eq!(changed, 1, "the writer must actually change the seeded row");
            db.execute("COMMIT").unwrap();

            let expected_current = if delete {
                Vec::new()
            } else {
                vec![vec![Value::Int4(1), Value::Int4(20)]]
            };
            assert_eq!(
                values(db.query("SELECT * FROM history_row", &[]).unwrap()),
                expected_current
            );
            db.storage.snapshot_manager().clear_cache();
            assert_eq!(
                values(db.storage.scan_table_at_snapshot("history_row", anchor).unwrap()),
                vec![original],
                "params={params}, delete={delete}: a historical row cannot vanish after cache eviction"
            );
        }
    }
}

#[test]
fn a_snapshot_lookup_cache_does_not_hide_a_later_transaction_commit() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE history_row (id INT, v INT)").unwrap();
    db.execute("INSERT INTO history_row VALUES (1, 10)").unwrap();
    let manager = db.storage.snapshot_manager();
    // The storage API permits any upper bound. A cached lookup at a future
    // bound must see a version committed since the previous lookup.
    let before = manager.read_at_snapshot("history_row", 1, u64::MAX).unwrap().unwrap();
    assert_eq!(
        bincode::deserialize::<Tuple>(&before).unwrap().values,
        vec![Value::Int4(1), Value::Int4(10)]
    );
    db.execute("BEGIN").unwrap();
    db.execute("UPDATE history_row SET v = 20").unwrap();
    db.execute("COMMIT").unwrap();
    let after = manager.read_at_snapshot("history_row", 1, u64::MAX).unwrap().unwrap();
    assert_eq!(
        bincode::deserialize::<Tuple>(&after).unwrap().values,
        vec![Value::Int4(1), Value::Int4(20)]
    );
}
