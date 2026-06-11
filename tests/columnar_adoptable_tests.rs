//! R3.3 columnar adoptability tests: batch-grouped writes + presence-bitmap
//! liveness.
//!
//! A `STORAGE COLUMNAR` table and a default-storage twin are driven through
//! identical DML (multi-row inserts, fast-batch bulk loads, deletes, updates,
//! transactions with commit/rollback, ON CONFLICT upserts, branches,
//! truncate); every query of the battery must return identical (normalized)
//! results on both tables.
//!
//! Run matrix (env vars are latched into `Lazy`s on first use, so each
//! setting needs its own process):
//!   cargo test --test columnar_adoptable_tests -- --test-threads=1
//!   HELIOS_COLP_OFF=1     cargo test --test columnar_adoptable_tests -- --test-threads=1
//!   HELIOS_ZONE_MAP_OFF=1 cargo test --test columnar_adoptable_tests -- --test-threads=1
//!   HELIOS_AGG_SERIAL=1   cargo test --test columnar_adoptable_tests -- --test-threads=1

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod columnar_adoptable {
    use heliosdb_nano::{storage::ColumnarStore, Config, EmbeddedDatabase, Value};

    fn test_db() -> EmbeddedDatabase {
        let mut config = Config::default();
        config.storage.memory_only = true;
        config.storage.wal_enabled = false;
        EmbeddedDatabase::with_config(config).expect("Failed to create database")
    }

    const COLUMNAR_DDL: &str = "CREATE TABLE {t} (\
         id INT8 PRIMARY KEY, \
         v INT8 STORAGE COLUMNAR, \
         w INT8 STORAGE COLUMNAR, \
         grp TEXT STORAGE COLUMNAR)";
    const ROW_DDL: &str = "CREATE TABLE {t} (\
         id INT8 PRIMARY KEY, \
         v INT8, \
         w INT8, \
         grp TEXT)";

    fn create_twins(db: &EmbeddedDatabase, col: &str, row: &str) {
        db.execute(&COLUMNAR_DDL.replace("{t}", col)).unwrap();
        db.execute(&ROW_DDL.replace("{t}", row)).unwrap();
    }

    fn w(i: i64) -> Option<i64> {
        if i % 23 == 0 {
            None
        } else {
            Some((i * 13) % 4000)
        }
    }

    fn grp(i: i64) -> String {
        format!("g{}", i % 7)
    }

    fn values_clause(lo: i64, hi: i64) -> String {
        let mut parts = Vec::with_capacity((hi - lo) as usize);
        for i in lo..hi {
            let w_sql = w(i).map_or("NULL".to_string(), |x| x.to_string());
            parts.push(format!("({i}, {i}, {w_sql}, '{}')", grp(i)));
        }
        parts.join(", ")
    }

    /// Multi-row VALUES insert — autocommit (drives the fast-batch path on
    /// the columnar table) into both twins.
    fn seed(db: &EmbeddedDatabase, col: &str, row: &str, lo: i64, hi: i64, per_stmt: i64) {
        let mut start = lo;
        while start < hi {
            let end = (start + per_stmt).min(hi);
            let values = values_clause(start, end);
            db.execute(&format!("INSERT INTO {col} (id, v, w, grp) VALUES {values}"))
                .unwrap();
            db.execute(&format!("INSERT INTO {row} (id, v, w, grp) VALUES {values}"))
                .unwrap();
            start = end;
        }
    }

    fn norm(v: &Value) -> String {
        match v {
            Value::Null => "NULL".to_string(),
            Value::Boolean(b) => b.to_string(),
            Value::Int2(n) => (*n as i64).to_string(),
            Value::Int4(n) => (*n as i64).to_string(),
            Value::Int8(n) => n.to_string(),
            Value::Float4(f) => format!("{:.6}", *f as f64),
            Value::Float8(f) => format!("{:.6}", f),
            Value::Numeric(s) => match s.parse::<f64>() {
                Ok(f) => format!("{:.6}", f),
                Err(_) => s.clone(),
            },
            Value::String(s) => format!("'{s}'"),
            other => format!("{other:?}"),
        }
    }

    fn run_normalized(db: &EmbeddedDatabase, sql: &str) -> Vec<Vec<String>> {
        let mut rows: Vec<Vec<String>> = db
            .query(sql, &[])
            .unwrap_or_else(|e| panic!("query failed: {sql}: {e}"))
            .iter()
            .map(|t| t.values.iter().map(norm).collect())
            .collect();
        rows.sort();
        rows
    }

    /// The query battery: aggregates (walk-driven and batch-driven), grouped
    /// aggregates (count/sum fast path), NULL-accepting predicates (the shape
    /// that NEEDS true liveness), zone-prunable range predicates, projected
    /// scans, Top-N, and point lookups.
    fn assert_twins_equal(db: &EmbeddedDatabase, col: &str, row: &str, ctx: &str) {
        let battery = [
            "SELECT COUNT(*) FROM {t}",
            "SELECT COUNT(v) FROM {t}",
            "SELECT SUM(v) FROM {t}",
            "SELECT MIN(v), MAX(v) FROM {t}",
            "SELECT COUNT(*) FROM {t} WHERE w IS NULL",
            "SELECT COUNT(*) FROM {t} WHERE w IS NOT NULL",
            "SELECT SUM(w) FROM {t} WHERE w IS NULL OR w < 100",
            "SELECT grp, COUNT(*), SUM(v) FROM {t} GROUP BY grp",
            "SELECT grp, COUNT(*) FROM {t} WHERE v < 500 GROUP BY grp",
            "SELECT COUNT(*) FROM {t} WHERE v >= 100 AND v < 200",
            "SELECT SUM(v) FROM {t} WHERE v BETWEEN 1000 AND 2000",
            "SELECT v, w FROM {t} WHERE v < 50 ORDER BY v",
            "SELECT v FROM {t} ORDER BY v DESC LIMIT 5",
            "SELECT v, grp FROM {t} WHERE grp = 'g3' ORDER BY v",
            "SELECT v FROM {t} WHERE v = 1234",
        ];
        for template in battery {
            let col_rows = run_normalized(db, &template.replace("{t}", col));
            let row_rows = run_normalized(db, &template.replace("{t}", row));
            assert_eq!(
                col_rows, row_rows,
                "[{ctx}] columnar vs row mismatch for query: {template}"
            );
        }
    }

    #[test]
    fn test_bulk_load_then_query_equivalence() {
        let db = test_db();
        create_twins(&db, "bulk_c", "bulk_r");
        // ~4 columnar batches; multi-row statements drive the grouped paths.
        seed(&db, "bulk_c", "bulk_r", 0, 4000, 500);
        assert_twins_equal(&db, "bulk_c", "bulk_r", "bulk load");
    }

    #[test]
    fn test_single_row_inserts_equivalence() {
        let db = test_db();
        create_twins(&db, "single_c", "single_r");
        for i in 0..300 {
            let w_sql = w(i).map_or("NULL".to_string(), |x| x.to_string());
            for t in ["single_c", "single_r"] {
                db.execute(&format!(
                    "INSERT INTO {t} (id, v, w, grp) VALUES ({i}, {i}, {w_sql}, '{}')",
                    grp(i)
                ))
                .unwrap();
            }
        }
        assert_twins_equal(&db, "single_c", "single_r", "single-row inserts");
    }

    #[test]
    fn test_delete_shapes_equivalence() {
        let db = test_db();
        create_twins(&db, "del_c", "del_r");
        seed(&db, "del_c", "del_r", 0, 3000, 500);

        for t in ["del_c", "del_r"] {
            // Point delete (fast autocommit path).
            db.execute(&format!("DELETE FROM {t} WHERE id = 7")).unwrap();
            // Range delete (plan-arm path).
            db.execute(&format!("DELETE FROM {t} WHERE v >= 1000 AND v < 1500"))
                .unwrap();
            // Whole-batch delete: empties columnar batch 2 entirely.
            db.execute(&format!("DELETE FROM {t} WHERE id >= 2048 AND id < 2300"))
                .unwrap();
        }
        assert_twins_equal(&db, "del_c", "del_r", "after deletes");

        // Delete everything, then verify empty results.
        for t in ["del_c", "del_r"] {
            db.execute(&format!("DELETE FROM {t}")).unwrap();
        }
        assert_twins_equal(&db, "del_c", "del_r", "after delete-all");
    }

    #[test]
    fn test_update_shapes_equivalence() {
        let db = test_db();
        create_twins(&db, "upd_c", "upd_r");
        seed(&db, "upd_c", "upd_r", 0, 2500, 500);

        for t in ["upd_c", "upd_r"] {
            // Point update of a columnar value.
            db.execute(&format!("UPDATE {t} SET v = 999999 WHERE id = 42"))
                .unwrap();
            // Range update touching several batches.
            db.execute(&format!("UPDATE {t} SET w = NULL WHERE v >= 500 AND v < 700"))
                .unwrap();
            // Update inside an explicit transaction.
            db.execute("BEGIN").unwrap();
            db.execute(&format!("UPDATE {t} SET grp = 'gX' WHERE v < 20")).unwrap();
            db.execute("COMMIT").unwrap();
        }
        assert_twins_equal(&db, "upd_c", "upd_r", "after updates");
    }

    #[test]
    fn test_transaction_commit_and_rollback_equivalence() {
        let db = test_db();
        create_twins(&db, "txn_c", "txn_r");
        seed(&db, "txn_c", "txn_r", 0, 2000, 400);

        // Committed transaction: multi-row insert + delete + update.
        for t in ["txn_c", "txn_r"] {
            db.execute("BEGIN").unwrap();
            db.execute(&format!(
                "INSERT INTO {t} (id, v, w, grp) VALUES {}",
                values_clause(2000, 2200)
            ))
            .unwrap();
            db.execute(&format!("DELETE FROM {t} WHERE id < 100")).unwrap();
            db.execute(&format!("UPDATE {t} SET v = v + 1000000 WHERE id >= 150 AND id < 160"))
                .unwrap();
            db.execute("COMMIT").unwrap();
        }
        assert_twins_equal(&db, "txn_c", "txn_r", "after committed txn");

        // Rolled-back transaction: must leave NO trace (values, zone stats,
        // presence bits all ride the write set).
        for t in ["txn_c", "txn_r"] {
            db.execute("BEGIN").unwrap();
            db.execute(&format!(
                "INSERT INTO {t} (id, v, w, grp) VALUES {}",
                values_clause(5000, 5400)
            ))
            .unwrap();
            db.execute(&format!("DELETE FROM {t} WHERE id >= 150 AND id < 400"))
                .unwrap();
            db.execute(&format!("UPDATE {t} SET v = 0 WHERE id >= 1000 AND id < 1100"))
                .unwrap();
            db.execute("ROLLBACK").unwrap();
        }
        assert_twins_equal(&db, "txn_c", "txn_r", "after rolled-back txn");

        // And a sanity check that the rolled-back rows really are absent.
        let rows = run_normalized(&db, "SELECT COUNT(*) FROM txn_c WHERE id >= 5000");
        assert_eq!(rows, vec![vec!["0".to_string()]]);
    }

    #[test]
    fn test_on_conflict_upsert_equivalence() {
        let db = test_db();
        create_twins(&db, "conf_c", "conf_r");
        seed(&db, "conf_c", "conf_r", 0, 1200, 400);

        for t in ["conf_c", "conf_r"] {
            // Multi-row upsert: half conflict (update), half are new inserts.
            db.execute(&format!(
                "INSERT INTO {t} (id, v, w, grp) VALUES \
                 (1100, 111, 11, 'gU'), (1150, 222, 22, 'gU'), (1300, 333, 33, 'gU'), (1301, 444, 44, 'gU') \
                 ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v, w = EXCLUDED.w, grp = EXCLUDED.grp"
            ))
            .unwrap();
            // DO NOTHING shape.
            db.execute(&format!(
                "INSERT INTO {t} (id, v, w, grp) VALUES (1300, 0, 0, 'zz'), (1400, 555, 55, 'gU') \
                 ON CONFLICT (id) DO NOTHING"
            ))
            .unwrap();
        }
        assert_twins_equal(&db, "conf_c", "conf_r", "after upserts");
    }

    #[test]
    fn test_branch_isolation_falls_back_to_row_walk() {
        let db = test_db();
        create_twins(&db, "br_c", "br_r");
        seed(&db, "br_c", "br_r", 0, 1500, 500);

        db.execute("CREATE BRANCH dev AS OF NOW").unwrap();
        db.execute("USE BRANCH dev").unwrap();
        // Branch DML: writes live in the branch overlay; columnar sidecars
        // and presence must be untouched on main.
        for t in ["br_c", "br_r"] {
            db.execute(&format!("INSERT INTO {t} (id, v, w, grp) VALUES (9000, 1, 1, 'b')"))
                .unwrap();
            db.execute(&format!("DELETE FROM {t} WHERE id = 3")).unwrap();
        }
        assert_twins_equal(&db, "br_c", "br_r", "on branch dev");

        db.execute("USE BRANCH main").unwrap();
        // Main must be unaffected by branch DML.
        let count = run_normalized(&db, "SELECT COUNT(*) FROM br_c");
        assert_eq!(count, vec![vec!["1500".to_string()]]);
        assert_twins_equal(&db, "br_c", "br_r", "back on main");
    }

    #[test]
    fn test_presence_backfill_from_legacy_sidecars() {
        let db = test_db();
        create_twins(&db, "leg_c", "leg_r");
        seed(&db, "leg_c", "leg_r", 0, 2600, 650);
        for t in ["leg_c", "leg_r"] {
            db.execute(&format!("DELETE FROM {t} WHERE id % 5 = 0")).unwrap();
        }

        // Simulate pre-R3.3 data: strip every colp:/colpm: key.
        let raw = db.storage.db();
        let mut purged = 0;
        for prefix in ["colp:leg_c:", "colpm:leg_c"] {
            let iter = raw.prefix_iterator(prefix.as_bytes());
            for item in iter {
                let (key, _) = item.unwrap();
                if !key.starts_with(prefix.as_bytes()) {
                    break;
                }
                raw.delete(&key).unwrap();
                purged += 1;
            }
        }
        if std::env::var("HELIOS_COLP_OFF").is_err() {
            // With presence enabled the DML above must have built sidecars.
            assert!(purged > 0, "expected presence sidecars to exist before purge");
        }
        assert!(!ColumnarStore::presence_manifest_complete(&raw, "leg_c"));

        // Queries fall back / lazily backfill — results must be identical.
        assert_twins_equal(&db, "leg_c", "leg_r", "legacy (presence stripped)");

        if std::env::var("HELIOS_COLP_OFF").is_err() {
            // The read battery above must have re-backfilled the manifest.
            assert!(
                ColumnarStore::presence_manifest_complete(&raw, "leg_c"),
                "presence manifest should be lazily backfilled by columnar reads"
            );
            // And subsequent DML keeps it fresh.
            db.execute("INSERT INTO leg_c (id, v, w, grp) VALUES (7777, 1, 1, 'z')")
                .unwrap();
            db.execute("INSERT INTO leg_r (id, v, w, grp) VALUES (7777, 1, 1, 'z')")
                .unwrap();
            assert_twins_equal(&db, "leg_c", "leg_r", "after backfill + insert");
        }
    }

    #[test]
    fn test_truncate_then_reinsert_equivalence() {
        let db = test_db();
        create_twins(&db, "tr_c", "tr_r");
        seed(&db, "tr_c", "tr_r", 0, 1200, 400);

        for t in ["tr_c", "tr_r"] {
            db.execute(&format!("TRUNCATE TABLE {t}")).unwrap();
        }
        assert_twins_equal(&db, "tr_c", "tr_r", "after truncate");
        let rows = run_normalized(&db, "SELECT COUNT(*) FROM tr_c");
        assert_eq!(rows, vec![vec!["0".to_string()]]);

        seed(&db, "tr_c", "tr_r", 0, 600, 200);
        assert_twins_equal(&db, "tr_c", "tr_r", "after truncate + reinsert");
    }

    #[test]
    fn test_drop_and_recreate_table_equivalence() {
        let db = test_db();
        create_twins(&db, "dr_c", "dr_r");
        seed(&db, "dr_c", "dr_r", 0, 800, 400);

        db.execute("DROP TABLE dr_c").unwrap();
        db.execute("DROP TABLE dr_r").unwrap();
        create_twins(&db, "dr_c", "dr_r");
        seed(&db, "dr_c", "dr_r", 0, 300, 100);
        assert_twins_equal(&db, "dr_c", "dr_r", "after drop + recreate");
    }

    #[test]
    fn test_storage_mode_migration_resets_presence() {
        let db = test_db();
        db.execute("CREATE TABLE mig (id INT8 PRIMARY KEY, v INT8)").unwrap();
        for i in 0..200 {
            db.execute(&format!("INSERT INTO mig VALUES ({i}, {i})")).unwrap();
        }
        db.execute("DELETE FROM mig WHERE id % 4 = 0").unwrap();

        // Row -> columnar migration, then queries must still be correct.
        db.execute("ALTER TABLE mig ALTER COLUMN v SET STORAGE COLUMNAR").unwrap();
        let sum = run_normalized(&db, "SELECT COUNT(*), SUM(v) FROM mig");
        // 150 rows remain: ids with id % 4 != 0. Sum of 0..200 = 19900,
        // minus sum of multiples of 4 below 200 (0+4+...+196 = 4900).
        assert_eq!(sum, vec![vec!["150".to_string(), "15000".to_string()]]);

        db.execute("INSERT INTO mig VALUES (500, 500)").unwrap();
        db.execute("DELETE FROM mig WHERE id = 1").unwrap();
        let sum = run_normalized(&db, "SELECT COUNT(*), SUM(v) FROM mig");
        assert_eq!(sum, vec![vec!["150".to_string(), "15499".to_string()]]);

        // Columnar -> row migration must also stay correct.
        db.execute("ALTER TABLE mig ALTER COLUMN v SET STORAGE DEFAULT").unwrap();
        let sum = run_normalized(&db, "SELECT COUNT(*), SUM(v) FROM mig");
        assert_eq!(sum, vec![vec!["150".to_string(), "15499".to_string()]]);
    }

    #[test]
    fn test_count_star_after_mixed_dml() {
        let db = test_db();
        create_twins(&db, "cnt_c", "cnt_r");
        seed(&db, "cnt_c", "cnt_r", 0, 2100, 700);
        for t in ["cnt_c", "cnt_r"] {
            db.execute(&format!("DELETE FROM {t} WHERE id >= 1024 AND id < 2048"))
                .unwrap();
            db.execute(&format!("INSERT INTO {t} (id, v, w, grp) VALUES {}", values_clause(3000, 3010)))
                .unwrap();
        }
        let col = run_normalized(&db, "SELECT COUNT(*) FROM cnt_c");
        let row = run_normalized(&db, "SELECT COUNT(*) FROM cnt_r");
        assert_eq!(col, row);
        assert_eq!(col, vec![vec!["1086".to_string()]]); // 2100 - 1024 + 10
    }
}
