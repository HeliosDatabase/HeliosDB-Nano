//! R3.4 typed columnar batch format v2 tests.
//!
//! Covers: v1/v2 read equivalence (row-twin pattern from
//! `columnar_adoptable_tests`, plus a hand-rewritten v1 triplet), mixed-type
//! downgrade, RMW v2→v1 downgrade, dictionary text GROUP BY correctness
//! including NULLs and a full 1024-distinct-codes batch, kill-switch
//! behavior, and lazy migration.
//!
//! Run matrix (env vars are latched into `Lazy`s on first use, so each
//! setting needs its own process):
//!   cargo test --test typed_batch_tests -- --test-threads=1
//!   HELIOS_BATCH_V2_OFF=1     cargo test --test typed_batch_tests -- --test-threads=1
//!   HELIOS_COLP_OFF=1         cargo test --test typed_batch_tests -- --test-threads=1
//!   HELIOS_ZONE_MAP_OFF=1     cargo test --test typed_batch_tests -- --test-threads=1
//!   HELIOS_AGG_SERIAL=1       cargo test --test typed_batch_tests -- --test-threads=1
//!   HELIOS_BATCH_V2_MIGRATE=1 cargo test --test typed_batch_tests -- --test-threads=1

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod typed_batches {
    use heliosdb_nano::storage::typed_batch::{decode_column_batch, BATCH_V2_MAGIC};
    use heliosdb_nano::storage::{ColumnBatch, ColumnarStore};
    use heliosdb_nano::{Config, EmbeddedDatabase, Value};

    fn test_db() -> EmbeddedDatabase {
        let mut config = Config::default();
        config.storage.memory_only = true;
        config.storage.wal_enabled = false;
        EmbeddedDatabase::with_config(config).expect("Failed to create database")
    }

    fn v2_writes_on() -> bool {
        std::env::var("HELIOS_BATCH_V2_OFF").is_err()
    }

    fn migrate_on() -> bool {
        std::env::var("HELIOS_BATCH_V2_MIGRATE").as_deref() == Ok("1")
    }

    fn is_v2(raw: &[u8]) -> bool {
        raw.len() >= 4 && raw[..4] == BATCH_V2_MAGIC
    }

    /// Raw bytes of one stored column batch.
    fn raw_batch(db: &EmbeddedDatabase, table: &str, column: &str, batch_id: u64) -> Option<Vec<u8>> {
        db.storage
            .db()
            .get(format!("col:{table}:{column}:{batch_id}").as_bytes())
            .unwrap()
    }

    /// Rewrite every `col:{table}:` batch as legacy v1 bincode (simulates a
    /// data directory written before R3.4).
    fn force_v1(db: &EmbeddedDatabase, table: &str) {
        let raw = db.storage.db();
        let prefix = format!("col:{table}:");
        let mut rewrites = Vec::new();
        for item in raw.prefix_iterator(prefix.as_bytes()) {
            let (key, value) = item.unwrap();
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            let mut batch: ColumnBatch = decode_column_batch(&value).unwrap();
            batch.typed = None;
            rewrites.push((key.to_vec(), bincode::serialize(&batch).unwrap()));
        }
        for (key, value) in rewrites {
            raw.put(&key, &value).unwrap();
        }
    }

    fn count_batches(db: &EmbeddedDatabase, table: &str, want_v2: bool) -> usize {
        let raw = db.storage.db();
        let prefix = format!("col:{table}:");
        let mut count = 0;
        for item in raw.prefix_iterator(prefix.as_bytes()) {
            let (key, value) = item.unwrap();
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            if is_v2(&value) == want_v2 {
                count += 1;
            }
        }
        count
    }

    const COLUMNAR_DDL: &str = "CREATE TABLE {t} (\
         id INT8 PRIMARY KEY, \
         v INT8 STORAGE COLUMNAR, \
         w INT8 STORAGE COLUMNAR, \
         f FLOAT8 STORAGE COLUMNAR, \
         grp TEXT STORAGE COLUMNAR)";
    const ROW_DDL: &str = "CREATE TABLE {t} (\
         id INT8 PRIMARY KEY, \
         v INT8, \
         w INT8, \
         f FLOAT8, \
         grp TEXT)";

    fn w(i: i64) -> Option<i64> {
        if i % 23 == 0 {
            None
        } else {
            Some((i * 13) % 4000)
        }
    }

    /// grp: 30 groups (engages the hash-group path) with NULLs every 17th row.
    fn grp(i: i64) -> Option<String> {
        if i % 17 == 0 {
            None
        } else {
            Some(format!("g{}", i % 30))
        }
    }

    fn seed(db: &EmbeddedDatabase, tables: &[&str], lo: i64, hi: i64, per_stmt: i64) {
        let mut start = lo;
        while start < hi {
            let end = (start + per_stmt).min(hi);
            let mut parts = Vec::with_capacity((end - start) as usize);
            for i in start..end {
                let w_sql = w(i).map_or("NULL".to_string(), |x| x.to_string());
                let g_sql = grp(i).map_or("NULL".to_string(), |g| format!("'{g}'"));
                let f_sql = format!("{}.25", (i * 3) % 100);
                parts.push(format!("({i}, {}, {w_sql}, {f_sql}, {g_sql})", (i * 7) % 5000));
            }
            let values = parts.join(", ");
            for table in tables {
                db.execute(&format!("INSERT INTO {table} (id, v, w, f, grp) VALUES {values}"))
                    .unwrap();
            }
            start = end;
        }
    }

    fn norm(v: &Value) -> String {
        match v {
            Value::Null => "NULL".to_string(),
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

    /// Query battery exercising every kernelized shape: filtered/unfiltered
    /// scalar aggregates, text GROUP BY (incl. NULL groups), int GROUP BY
    /// (small-int dense keys), NULL-accepting predicates, float aggregates,
    /// filter scans, MIN/MAX over ints/floats/text.
    fn assert_tables_equal(db: &EmbeddedDatabase, tables: &[&str], ctx: &str) {
        let battery = [
            "SELECT COUNT(*) FROM {t}",
            "SELECT COUNT(v), COUNT(w), COUNT(grp) FROM {t}",
            "SELECT SUM(v) FROM {t}",
            "SELECT SUM(v), COUNT(*) FROM {t} WHERE v < 2500",
            "SELECT SUM(w) FROM {t} WHERE w IS NULL OR w < 100",
            "SELECT COUNT(*) FROM {t} WHERE w IS NOT NULL",
            "SELECT COUNT(*) FROM {t} WHERE v != 7",
            "SELECT MIN(v), MAX(v), MIN(f), MAX(f), MIN(grp), MAX(grp) FROM {t}",
            "SELECT AVG(v) FROM {t}",
            "SELECT AVG(v) FROM {t} WHERE v >= 100 AND v < 3000",
            "SELECT SUM(f) FROM {t} WHERE f > 10.0",
            "SELECT grp, COUNT(*), SUM(v) FROM {t} GROUP BY grp",
            "SELECT grp, COUNT(*), SUM(w) FROM {t} GROUP BY grp",
            "SELECT grp, COUNT(*) FROM {t} WHERE v < 500 GROUP BY grp",
            "SELECT grp, MIN(v), MAX(v) FROM {t} GROUP BY grp",
            "SELECT w, COUNT(*), SUM(v) FROM {t} WHERE w < 50 GROUP BY w",
            "SELECT COUNT(*) FROM {t} WHERE grp = 'g3'",
            "SELECT COUNT(*) FROM {t} WHERE grp LIKE 'g1%'",
            "SELECT v, grp FROM {t} WHERE v >= 420 AND v < 460 ORDER BY v, grp",
            "SELECT v, w FROM {t} WHERE v < 50 ORDER BY v, w",
            "SELECT v FROM {t} WHERE v = 1234",
            "SELECT SUM(v) FROM {t} WHERE v BETWEEN 1000 AND 2000",
        ];
        for template in battery {
            let mut results = tables
                .iter()
                .map(|t| run_normalized(db, &template.replace("{t}", t)));
            let first = results.next().unwrap();
            for (table, rows) in tables.iter().skip(1).zip(results) {
                assert_eq!(
                    first, rows,
                    "[{ctx}] result mismatch between {} and {table} for query: {template}",
                    tables[0]
                );
            }
        }
    }

    /// Row twin vs v2-written columnar vs hand-rewritten-v1 columnar: the
    /// three must agree on the whole battery (v1/v2 read equivalence).
    #[test]
    fn test_v1_v2_row_twin_equivalence() {
        let db = test_db();
        db.execute(&ROW_DDL.replace("{t}", "eq_r")).unwrap();
        db.execute(&COLUMNAR_DDL.replace("{t}", "eq_c2")).unwrap();
        db.execute(&COLUMNAR_DDL.replace("{t}", "eq_c1")).unwrap();
        // ~3.4 batches; multi-row statements drive the grouped write paths.
        seed(&db, &["eq_r", "eq_c2", "eq_c1"], 0, 3500, 500);

        if v2_writes_on() {
            assert!(
                count_batches(&db, "eq_c2", true) > 0,
                "expected v2 batches from homogeneous typed writes"
            );
            // Every column of this table is v2-eligible.
            assert_eq!(count_batches(&db, "eq_c2", false), 0, "unexpected v1 batches");
        } else {
            assert_eq!(count_batches(&db, "eq_c2", true), 0, "kill switch must force v1 writes");
        }

        force_v1(&db, "eq_c1");
        assert_eq!(count_batches(&db, "eq_c1", true), 0);

        assert_tables_equal(&db, &["eq_r", "eq_c2", "eq_c1"], "bulk load");

        // DML over both formats: deletes punch NULL slots + presence clears,
        // updates RMW batches (v1 batches re-encode as v2 on RMW when
        // enabled) — results must stay identical.
        for t in ["eq_r", "eq_c2", "eq_c1"] {
            db.execute(&format!("DELETE FROM {t} WHERE id % 5 = 0")).unwrap();
            db.execute(&format!("UPDATE {t} SET v = v + 1, grp = 'bumped' WHERE id % 7 = 0"))
                .unwrap();
            db.execute(&format!("UPDATE {t} SET w = NULL WHERE id % 11 = 0")).unwrap();
        }
        assert_tables_equal(&db, &["eq_r", "eq_c2", "eq_c1"], "after deletes + updates");

        // Transactional writes stage batches through the txn write set.
        for t in ["eq_r", "eq_c2", "eq_c1"] {
            db.execute("BEGIN").unwrap();
            db.execute(&format!(
                "INSERT INTO {t} (id, v, w, f, grp) VALUES (9001, 1, 2, 3.5, 'tx'), (9002, 4, NULL, 0.5, NULL)"
            ))
            .unwrap();
            db.execute(&format!("DELETE FROM {t} WHERE id = 9001")).unwrap();
            db.execute("COMMIT").unwrap();
        }
        assert_tables_equal(&db, &["eq_r", "eq_c2", "eq_c1"], "after transaction");
    }

    /// A batch whose value set mixes families must be stored as v1 (no
    /// typed encoding) and still read back exactly.
    #[test]
    fn test_mixed_type_batch_stays_v1() {
        let db = test_db();
        let raw = db.storage.db();
        ColumnarStore::store(&raw, "mix_t", "c", 0, Value::Int8(42)).unwrap();
        ColumnarStore::store(&raw, "mix_t", "c", 1, Value::String("x".into())).unwrap();
        let bytes = raw_batch(&db, "mix_t", "c", 0).expect("batch must exist");
        assert!(!is_v2(&bytes), "mixed-family batch must be v1");
        assert_eq!(ColumnarStore::get(&raw, "mix_t", "c", 0).unwrap(), Some(Value::Int8(42)));
        assert_eq!(
            ColumnarStore::get(&raw, "mix_t", "c", 1).unwrap(),
            Some(Value::String("x".into()))
        );

        // Mixed int widths also stay v1 (round-trip exactness over format purity).
        ColumnarStore::store(&raw, "mix_w", "c", 0, Value::Int4(1)).unwrap();
        ColumnarStore::store(&raw, "mix_w", "c", 1, Value::Int8(2)).unwrap();
        let bytes = raw_batch(&db, "mix_w", "c", 0).expect("batch must exist");
        assert!(!is_v2(&bytes), "mixed-width batch must be v1");
        assert_eq!(ColumnarStore::get(&raw, "mix_w", "c", 0).unwrap(), Some(Value::Int4(1)));
        assert_eq!(ColumnarStore::get(&raw, "mix_w", "c", 1).unwrap(), Some(Value::Int8(2)));
    }

    /// RMW of a v2 batch with an incompatible value downgrades the batch to
    /// v1; values (old and new) survive exactly.
    #[test]
    fn test_rmw_v2_to_v1_downgrade() {
        let db = test_db();
        let raw = db.storage.db();
        for i in 0..10u64 {
            ColumnarStore::store(&raw, "rmw_t", "c", i, Value::Int8(i as i64 * 11)).unwrap();
        }
        let bytes = raw_batch(&db, "rmw_t", "c", 0).expect("batch must exist");
        assert_eq!(is_v2(&bytes), v2_writes_on(), "homogeneous int batch format");

        ColumnarStore::store(&raw, "rmw_t", "c", 5, Value::String("downgrade".into())).unwrap();
        let bytes = raw_batch(&db, "rmw_t", "c", 0).expect("batch must exist");
        assert!(!is_v2(&bytes), "RMW with incompatible value must downgrade to v1");
        assert_eq!(
            ColumnarStore::get(&raw, "rmw_t", "c", 5).unwrap(),
            Some(Value::String("downgrade".into()))
        );
        assert_eq!(
            ColumnarStore::get(&raw, "rmw_t", "c", 9).unwrap(),
            Some(Value::Int8(99))
        );

        // And back: overwriting the alien value with an int re-upgrades on
        // the next RMW (when v2 writes are enabled).
        ColumnarStore::store(&raw, "rmw_t", "c", 5, Value::Int8(55)).unwrap();
        let bytes = raw_batch(&db, "rmw_t", "c", 0).expect("batch must exist");
        assert_eq!(is_v2(&bytes), v2_writes_on());
        assert_eq!(ColumnarStore::get(&raw, "rmw_t", "c", 5).unwrap(), Some(Value::Int8(55)));
    }

    /// Dictionary text GROUP BY: NULL groups, >64 groups (hash path), and a
    /// batch with 1024 distinct strings (every slot its own dictionary code —
    /// the u32-code ceiling sanity check).
    #[test]
    fn test_text_group_by_dictionary_correctness() {
        let db = test_db();
        db.execute("CREATE TABLE hc_c (id INT8 PRIMARY KEY, g TEXT STORAGE COLUMNAR, v INT8 STORAGE COLUMNAR)")
            .unwrap();
        db.execute("CREATE TABLE hc_r (id INT8 PRIMARY KEY, g TEXT, v INT8)")
            .unwrap();

        // Batch 0: exactly 1024 rows, all-distinct group keys.
        // Batch 1: 100 groups with NULLs sprinkled in.
        let mut parts = Vec::new();
        for i in 0..1024i64 {
            parts.push(format!("({i}, 'unique_{i:04}', {})", i % 97));
        }
        for i in 1024..2048i64 {
            let g = if i % 13 == 0 {
                "NULL".to_string()
            } else {
                format!("'bucket_{}'", i % 100)
            };
            parts.push(format!("({i}, {g}, {})", (i * 3) % 1000));
        }
        for chunk in parts.chunks(512) {
            let values = chunk.join(", ");
            db.execute(&format!("INSERT INTO hc_c (id, g, v) VALUES {values}")).unwrap();
            db.execute(&format!("INSERT INTO hc_r (id, g, v) VALUES {values}")).unwrap();
        }

        for template in [
            "SELECT g, COUNT(*), SUM(v) FROM {t} GROUP BY g",
            "SELECT g, COUNT(*) FROM {t} WHERE v < 500 GROUP BY g",
            "SELECT g, MIN(v), MAX(v) FROM {t} GROUP BY g",
            "SELECT COUNT(*) FROM {t} WHERE g IS NULL",
            "SELECT g, COUNT(*) FROM {t} WHERE g LIKE 'unique_00%' GROUP BY g",
        ] {
            assert_eq!(
                run_normalized(&db, &template.replace("{t}", "hc_c")),
                run_normalized(&db, &template.replace("{t}", "hc_r")),
                "text GROUP BY mismatch for: {template}"
            );
        }

        // The all-distinct batch really did dictionary-encode 1024 codes.
        if v2_writes_on() {
            let bytes = raw_batch(&db, "hc_c", "g", 0).expect("batch must exist");
            assert!(is_v2(&bytes), "text batch should be v2");
        }
    }

    /// Lazy migration: a scan over a v1 batch rewrites it as v2 — but only
    /// behind HELIOS_BATCH_V2_MIGRATE=1 (default OFF).
    #[test]
    fn test_lazy_migration_gate() {
        let db = test_db();
        db.execute("CREATE TABLE mig_c (id INT8 PRIMARY KEY, v INT8 STORAGE COLUMNAR)")
            .unwrap();
        db.execute("CREATE TABLE mig_r (id INT8 PRIMARY KEY, v INT8)").unwrap();
        let values: Vec<String> = (0..600i64).map(|i| format!("({i}, {})", i * 7 % 500)).collect();
        for chunk in values.chunks(300) {
            let v = chunk.join(", ");
            db.execute(&format!("INSERT INTO mig_c (id, v) VALUES {v}")).unwrap();
            db.execute(&format!("INSERT INTO mig_r (id, v) VALUES {v}")).unwrap();
        }
        force_v1(&db, "mig_c");
        assert_eq!(count_batches(&db, "mig_c", true), 0);

        // A columnar scan decodes the v1 batch (and may migrate it).
        assert_eq!(
            run_normalized(&db, "SELECT SUM(v), COUNT(*) FROM mig_c"),
            run_normalized(&db, "SELECT SUM(v), COUNT(*) FROM mig_r"),
        );

        let migrated = count_batches(&db, "mig_c", true);
        if migrate_on() && v2_writes_on() {
            assert!(migrated > 0, "HELIOS_BATCH_V2_MIGRATE=1 must rewrite scanned v1 batches");
        } else {
            assert_eq!(migrated, 0, "migration must stay off by default");
        }

        // Results identical either way, before and after.
        assert_eq!(
            run_normalized(&db, "SELECT SUM(v), COUNT(*), MIN(v), MAX(v) FROM mig_c"),
            run_normalized(&db, "SELECT SUM(v), COUNT(*), MIN(v), MAX(v) FROM mig_r"),
        );
    }

    /// Float4/Bool columns round-trip through v2 and agree with a row twin
    /// (epsilon-equality and boolean comparison semantics ride the kernels).
    #[test]
    fn test_float4_and_bool_columns() {
        let db = test_db();
        db.execute(
            "CREATE TABLE fb_c (id INT8 PRIMARY KEY, r FLOAT4 STORAGE COLUMNAR, b BOOLEAN STORAGE COLUMNAR)",
        )
        .unwrap();
        db.execute("CREATE TABLE fb_r (id INT8 PRIMARY KEY, r FLOAT4, b BOOLEAN)")
            .unwrap();
        let mut parts = Vec::new();
        for i in 0..1500i64 {
            let r = if i % 19 == 0 {
                "NULL".to_string()
            } else {
                format!("{}.5", i % 200)
            };
            let b = match i % 3 {
                0 => "true",
                1 => "false",
                _ => "NULL",
            };
            parts.push(format!("({i}, {r}, {b})"));
        }
        for chunk in parts.chunks(500) {
            let values = chunk.join(", ");
            db.execute(&format!("INSERT INTO fb_c (id, r, b) VALUES {values}")).unwrap();
            db.execute(&format!("INSERT INTO fb_r (id, r, b) VALUES {values}")).unwrap();
        }
        for template in [
            "SELECT COUNT(r), COUNT(b) FROM {t}",
            "SELECT MIN(r), MAX(r) FROM {t}",
            "SELECT COUNT(*) FROM {t} WHERE r = 42.5",
            "SELECT COUNT(*) FROM {t} WHERE r > 100.0",
            "SELECT COUNT(*) FROM {t} WHERE b = true",
            "SELECT COUNT(*) FROM {t} WHERE b != false",
            "SELECT COUNT(*) FROM {t} WHERE b IS NULL",
            "SELECT AVG(r) FROM {t} WHERE r < 50.0",
        ] {
            assert_eq!(
                run_normalized(&db, &template.replace("{t}", "fb_c")),
                run_normalized(&db, &template.replace("{t}", "fb_r")),
                "float4/bool mismatch for: {template}"
            );
        }
    }
}
