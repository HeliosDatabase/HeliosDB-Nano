//! SQL INCREMENTALLY currently re-executes the stored plan, including when a
//! nonzero durable delta set selects its incremental branch. These controls prove
//! visible SQL behavior; they do not claim delta-only execution or its cost.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};

#[derive(Clone, Copy, Debug)]
enum Family {
    Text,
    Params,
}
const FAMILIES: [Family; 2] = [Family::Text, Family::Params];

fn run(db: &EmbeddedDatabase, family: Family, sql: &str) -> u64 {
    match family {
        Family::Text => db.execute(sql),
        Family::Params => db.execute_params(sql, &[]),
    }
    .unwrap_or_else(|error| panic!("{family:?}: {sql}: {error}"))
}

fn rows(db: &EmbeddedDatabase, table: &str) -> Vec<Vec<Value>> {
    db.query(&format!("SELECT * FROM {table} ORDER BY id"), &[])
        .unwrap()
        .into_iter()
        .map(|tuple| tuple.values)
        .collect()
}

fn pair(first: i32, second: i32) -> Vec<Value> {
    vec![Value::Int4(first), Value::Int4(second)]
}

/// Record the same preconditions used by the SQL selector. The public storage
/// insertion API supplies real durable deltas; a zero-delta full fallback cannot satisfy
/// this receipt. Keeping the count small also avoids the ordinary REFRESH's
/// 1000-delta threshold and the metadata helper's half-row cost threshold.
fn small_nonzero_delta_receipt(db: &EmbeddedDatabase, view: &str) {
    let metadata = db.storage.mv_catalog().get_view(view).unwrap();
    let since = metadata.last_refresh.expect("CREATE populated the materialized view");
    let count = db
        .storage
        .mv_delta_tracker()
        .count_deltas_since(&metadata.base_tables, since)
        .unwrap();
    let row_count = metadata.row_count.expect("CREATE recorded its row count");
    assert!(count > 0, "{view}: must exercise the nonzero-delta selection");
    assert!(count < 1000 && count * 2 < row_count, "{view}: {count}/{row_count}");
    eprintln!("SQL refresh receipt: view={view} base_rows={row_count} durable_deltas={count}");
}

#[test]
fn explicit_incremental_sql_refresh_includes_staged_update_delete_and_tracked_insert() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE items (id INT PRIMARY KEY, amount INT)")
            .unwrap();
        let values = (1..=32)
            .map(|id| format!("({id},{})", id * 10))
            .collect::<Vec<_>>()
            .join(",");
        db.execute(&format!("INSERT INTO items VALUES {values}")).unwrap();
        db.execute("CREATE MATERIALIZED VIEW item_mv AS SELECT id, amount FROM items")
            .unwrap();
        let original: Vec<_> = (1..=32).map(|id| pair(id, id * 10)).collect();
        assert_eq!(rows(&db, "item_mv"), original);

        // Both executor families stage these mutations in the explicit SQL
        // transaction. REFRESH is issued after COMMIT, so the stored plan's
        // fresh executor must see the committed final image.
        run(&db, family, "BEGIN");
        assert_eq!(run(&db, family, "UPDATE items SET amount = 111 WHERE id = 1"), 1);
        assert_eq!(run(&db, family, "DELETE FROM items WHERE id = 2"), 1);
        run(&db, family, "COMMIT");
        db.storage.insert_tuple("items", Tuple::new(pair(100, 1000))).unwrap();
        small_nonzero_delta_receipt(&db, "item_mv");

        let mut expected = vec![pair(1, 111)];
        expected.extend((3..=32).map(|id| pair(id, id * 10)));
        expected.push(pair(100, 1000));
        assert_eq!(rows(&db, "items"), expected);
        assert_eq!(rows(&db, "item_mv"), original, "MV remains old until REFRESH");
        run(&db, family, "REFRESH MATERIALIZED VIEW item_mv INCREMENTALLY");
        assert_eq!(rows(&db, "item_mv"), expected, "{family:?}");
        assert_eq!(db.storage.mv_catalog().get_view("item_mv").unwrap().row_count, Some(32));
        // A second refresh with no new base edits also preserves the exact
        // image, whether the selector takes its zero-delta full fallback or not.
        run(&db, family, "REFRESH MATERIALIZED VIEW item_mv INCREMENTALLY");
        assert_eq!(rows(&db, "item_mv"), expected, "{family:?}: repeat refresh");
    }
}

#[test]
fn explicit_incremental_sql_refresh_includes_recursive_fk_update_and_delete_cascades() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
        db.execute("CREATE TABLE c (id INT PRIMARY KEY REFERENCES p(id) ON UPDATE CASCADE ON DELETE CASCADE)")
            .unwrap();
        db.execute("CREATE TABLE g (id INT PRIMARY KEY, cid INT REFERENCES c(id) ON UPDATE CASCADE ON DELETE CASCADE)")
            .unwrap();
        let ids = (1..=32).map(|id| format!("({id})")).collect::<Vec<_>>().join(",");
        let pairs = (1..=32).map(|id| format!("({id},{id})")).collect::<Vec<_>>().join(",");
        db.execute(&format!("INSERT INTO p VALUES {ids}")).unwrap();
        db.execute(&format!("INSERT INTO c VALUES {ids}")).unwrap();
        db.execute(&format!("INSERT INTO g VALUES {pairs}")).unwrap();
        db.execute("CREATE MATERIALIZED VIEW child_mv AS SELECT id FROM c")
            .unwrap();
        db.execute("CREATE MATERIALIZED VIEW grandchild_mv AS SELECT id, cid FROM g")
            .unwrap();
        let original_c: Vec<_> = (1..=32).map(|id| vec![Value::Int4(id)]).collect();
        let original_g: Vec<_> = (1..=32).map(|id| pair(id, id)).collect();
        assert_eq!(rows(&db, "child_mv"), original_c);
        assert_eq!(rows(&db, "grandchild_mv"), original_g);

        run(&db, family, "BEGIN");
        assert_eq!(run(&db, family, "UPDATE p SET id = 101 WHERE id = 1"), 1);
        assert_eq!(run(&db, family, "DELETE FROM p WHERE id = 2"), 1);
        run(&db, family, "COMMIT");
        // Each view's own base table receives a tracked storage-API INSERT, so both
        // refreshes have nonzero delta sets alongside generated FK mutations.
        run(&db, family, "INSERT INTO p VALUES (100)");
        db.storage
            .insert_tuple("c", Tuple::new(vec![Value::Int4(100)]))
            .unwrap();
        db.storage.insert_tuple("g", Tuple::new(pair(100, 100))).unwrap();
        small_nonzero_delta_receipt(&db, "child_mv");
        small_nonzero_delta_receipt(&db, "grandchild_mv");

        let mut expected_c: Vec<_> = (3..=32).map(|id| vec![Value::Int4(id)]).collect();
        expected_c.extend([vec![Value::Int4(100)], vec![Value::Int4(101)]]);
        let mut expected_g = vec![pair(1, 101)];
        expected_g.extend((3..=32).map(|id| pair(id, id)));
        expected_g.push(pair(100, 100));
        assert_eq!(rows(&db, "c"), expected_c);
        assert_eq!(rows(&db, "g"), expected_g);
        assert_eq!(rows(&db, "child_mv"), original_c);
        assert_eq!(rows(&db, "grandchild_mv"), original_g);

        run(&db, family, "REFRESH MATERIALIZED VIEW child_mv INCREMENTALLY");
        run(&db, family, "REFRESH MATERIALIZED VIEW grandchild_mv INCREMENTALLY");
        assert_eq!(rows(&db, "child_mv"), expected_c, "{family:?}");
        assert_eq!(rows(&db, "grandchild_mv"), expected_g, "{family:?}");
        assert_eq!(
            db.storage.mv_catalog().get_view("child_mv").unwrap().row_count,
            Some(32)
        );
        assert_eq!(
            db.storage.mv_catalog().get_view("grandchild_mv").unwrap().row_count,
            Some(32)
        );
    }
}
