//! SEQ-1 — durable sequence foundation tests.
//!
//! Covers the two things SEQ-1 is responsible for:
//!   1. the `PersistedSequence` / `PersistedSeqState` catalog records
//!      round-trip across a real data-directory reopen, and
//!   2. the cached-block `nextval` upholds the NO-DUPLICATE invariant across a
//!      reopen — after reserving a CACHE-sized block and serving a few values,
//!      a fresh attach to the same dir resumes STRICTLY PAST the durable
//!      high-water mark (a gap is allowed; a duplicate or any value <= an
//!      already-served value is a failure).
//!
//! These talk directly to `StorageEngine` + `Catalog` + the `sequences`
//! runtime so they exercise the durable path deterministically without needing
//! the (later-slice) CREATE SEQUENCE executor wiring.

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod sequence_durability {
    use heliosdb_nano::config::Config;
    use heliosdb_nano::sql::sequences;
    use heliosdb_nano::storage::{Catalog, PersistedSeqState, PersistedSequence, StorageEngine};
    use std::sync::Arc;

    fn scratch_dir() -> std::path::PathBuf {
        let id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("nano_seqdur_{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open(dir: &std::path::Path) -> Arc<StorageEngine> {
        let mut config = Config::default();
        config.storage.memory_only = false;
        // Durable path under test; default group-commit/durable behaviour.
        Arc::new(StorageEngine::open(dir, &config).unwrap())
    }

    fn sample_def(name: &str, cache: i64) -> PersistedSequence {
        PersistedSequence {
            name: name.to_string(),
            data_type: "bigint".to_string(),
            start_value: 1,
            increment_by: 1,
            min_value: 1,
            max_value: i64::MAX,
            cache,
            cycle: false,
            owned_by_table: Some("orders".to_string()),
            owned_by_column: Some("id".to_string()),
        }
    }

    #[test]
    fn persisted_sequence_records_round_trip_across_reopen() {
        let _e2e = e2e_guard();
        let dir = scratch_dir();
        let def = sample_def("round_trip_seq", 16);

        // Session 1 — write the definition and a high-water state.
        {
            let engine = open(&dir);
            let catalog = Catalog::new(&engine);
            catalog.save_sequence(&def).unwrap();
            catalog
                .save_sequence_state(
                    &def.name,
                    &PersistedSeqState {
                        last_reserved: 48,
                        is_called: true,
                    },
                )
                .unwrap();
            assert!(catalog.sequence_exists(&def.name).unwrap());
            // Force the records to disk before dropping the engine.
            engine.flush_wal().unwrap();
        }

        // Session 2 — fresh attach to the SAME dir.
        {
            let engine = open(&dir);
            let catalog = Catalog::new(&engine);

            let loaded = catalog
                .get_sequence(&def.name)
                .unwrap()
                .expect("def missing after reopen");
            assert_eq!(loaded, def, "definition did not round-trip");

            let state = catalog
                .get_sequence_state(&def.name)
                .unwrap()
                .expect("state missing after reopen");
            assert_eq!(state.last_reserved, 48);
            assert!(state.is_called);

            // list_sequences sees it; the meta:seqstate: prefix does NOT bleed in.
            let all = catalog.list_sequences().unwrap();
            assert_eq!(all.len(), 1, "expected exactly one sequence, got {:?}", all);
            assert_eq!(all[0].name, def.name);

            // Drop removes BOTH records.
            catalog.drop_sequence(&def.name).unwrap();
            assert!(!catalog.sequence_exists(&def.name).unwrap());
            assert!(catalog.get_sequence_state(&def.name).unwrap().is_none());
            assert!(catalog.list_sequences().unwrap().is_empty());
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cached_nextval_resumes_past_high_water_after_reopen() {
        let _e2e = e2e_guard();
        let dir = scratch_dir();
        let name = "cached_resume_seq";
        let cache = 32_i64;

        // Session 1 — reserve a CACHE=32 block, serve 5 values (expect 1..=5),
        // then drop the engine. The durable high-water is the END of the
        // reserved block (>= 32), not 5.
        let served: Vec<i64> = {
            let engine = open(&dir);
            // Persist the definition (as the CREATE SEQUENCE executor will),
            // install the durable handle (as DB-open does), seed the runtime.
            Catalog::new(&engine).save_sequence(&sample_def(name, cache)).unwrap();
            sequences::install_persistence(&engine);
            sequences::install_runtime(&sample_def(name, cache), None);

            let mut got = Vec::new();
            for _ in 0..5 {
                got.push(sequences::try_nextval(name).unwrap());
            }
            assert_eq!(got, vec![1, 2, 3, 4, 5], "first block did not serve 1..=5");

            // The durable high-water must already cover the whole reserved
            // block (>= cache), proving we fsynced the block END before serving.
            let st = Catalog::new(&engine)
                .get_sequence_state(name)
                .unwrap()
                .expect("no durable state after nextval");
            assert!(
                st.last_reserved >= cache,
                "durable high-water {} did not cover the reserved cache block of {}",
                st.last_reserved,
                cache
            );
            engine.flush_wal().unwrap();
            got
        };

        let max_served = *served.iter().max().unwrap();

        // Simulate a fresh process: drop the in-process runtime so the reopen
        // lazy-loads from the durable state (a real crash gives a fresh STORE).
        sequences::invalidate_cache(name);

        // Session 2 — fresh attach; the next value MUST be strictly past every
        // value the prior process could have served. Gaps are correct.
        {
            let engine = open(&dir);
            sequences::install_persistence(&engine);

            let resumed = sequences::try_nextval(name).unwrap();
            assert!(
                resumed > max_served,
                "RESUME VIOLATION: nextval returned {} which is <= a previously served value {} \
                 (duplicate-after-crash)",
                resumed,
                max_served
            );
            // And it must clear the durable high-water from session 1 (>= 32).
            assert!(
                resumed > cache,
                "expected resume past the reserved block end (> {}), got {}",
                cache,
                resumed
            );

            // A second reopen confirms idempotent resume (still monotonic).
            let after = sequences::try_nextval(name).unwrap();
            assert!(after > resumed, "second value not monotonic: {} !> {}", after, resumed);
            engine.flush_wal().unwrap();
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- SEQ-2 / SEQ-3: CREATE & ALTER SEQUENCE through the full SQL stack
    //
    // These go end-to-end (parse -> planner -> executor -> catalog) over a
    // REAL data dir, so the persisted CREATE SEQUENCE definition (its
    // increment/min/max/cycle/cache) and ALTER SEQUENCE mutations are observed
    // via `nextval`, which lazy-loads the durable def through the installed
    // handle. Sequence names are unique to tolerate the process-global handle.

    // The durable persistence handle is a PROCESS-GLOBAL `Weak<StorageEngine>`
    // (installed at DB-open). Two `EmbeddedDatabase` instances in the same test
    // binary therefore fight over it: whoever opened last wins. The end-to-end
    // tests below each open their OWN dir, so they must not run concurrently —
    // serialize them through this mutex so each test's open is the last open
    // for the duration it holds the lock.
    static E2E_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn e2e_guard() -> std::sync::MutexGuard<'static, ()> {
        E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn open_db(dir: &std::path::Path) -> heliosdb_nano::EmbeddedDatabase {
        heliosdb_nano::EmbeddedDatabase::new(dir).unwrap()
    }

    fn nextval(db: &heliosdb_nano::EmbeddedDatabase, name: &str) -> i64 {
        let rows = db.query(&format!("SELECT nextval('{name}')"), &[]).unwrap();
        match rows[0].values[0] {
            heliosdb_nano::Value::Int8(v) => v,
            ref other => panic!("nextval returned non-int8: {other:?}"),
        }
    }

    #[test]
    fn create_sequence_captures_all_options() {
        let _e2e = e2e_guard();
        let dir = scratch_dir();
        let db = open_db(&dir);
        let name = "seq2_opts";
        // INCREMENT 10, range [100,130], START 100, CACHE 1, NO CYCLE.
        db.execute(&format!(
            "CREATE SEQUENCE {name} INCREMENT BY 10 MINVALUE 100 MAXVALUE 130 START WITH 100 CACHE 1"
        ))
        .unwrap();

        // The captured INCREMENT/START drive the served values...
        assert_eq!(nextval(&db, name), 100);
        assert_eq!(nextval(&db, name), 110);
        assert_eq!(nextval(&db, name), 120);
        assert_eq!(nextval(&db, name), 130);
        // ...and the captured MAXVALUE bounds the 5th value. With SEQ-5 the SQL
        // `nextval` evaluator now PROPAGATES the NO-CYCLE overflow as a
        // PostgreSQL error (instead of clamping to the bound), proving both that
        // MAXVALUE was captured/enforced AND that the error reaches the SQL
        // layer.
        let err = db.execute(&format!("SELECT nextval('{name}')")).unwrap_err();
        assert!(
            err.to_string().contains("reached maximum value"),
            "expected MAXVALUE overflow error from SQL nextval, got: {err}"
        );
        // The fallible runtime entry point raises the same error.
        let err = heliosdb_nano::sql::sequences::try_nextval(name).unwrap_err();
        assert!(
            err.to_string().contains("reached maximum value"),
            "expected MAXVALUE overflow error from try_nextval, got: {err}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn create_sequence_cycle_wraps() {
        let _e2e = e2e_guard();
        let dir = scratch_dir();
        let db = open_db(&dir);
        let name = "seq2_cycle";
        db.execute(&format!(
            "CREATE SEQUENCE {name} INCREMENT BY 1 MINVALUE 1 MAXVALUE 3 START WITH 1 CYCLE CACHE 1"
        ))
        .unwrap();
        assert_eq!(nextval(&db, name), 1);
        assert_eq!(nextval(&db, name), 2);
        assert_eq!(nextval(&db, name), 3);
        // CYCLE was captured -> wraps to MINVALUE.
        assert_eq!(nextval(&db, name), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn create_sequence_descending() {
        let _e2e = e2e_guard();
        let dir = scratch_dir();
        let db = open_db(&dir);
        let name = "seq2_desc";
        db.execute(&format!(
            "CREATE SEQUENCE {name} INCREMENT BY -1 MINVALUE -3 MAXVALUE -1 START WITH -1 CACHE 1"
        ))
        .unwrap();
        assert_eq!(nextval(&db, name), -1);
        assert_eq!(nextval(&db, name), -2);
        assert_eq!(nextval(&db, name), -3);
        // 4th underflows MINVALUE with NO CYCLE. With SEQ-5 the SQL evaluator
        // now raises the PostgreSQL "reached minimum value" error.
        let err = db.execute(&format!("SELECT nextval('{name}')")).unwrap_err();
        assert!(err.to_string().contains("reached minimum value"), "SQL nextval: {err}");
        let err = heliosdb_nano::sql::sequences::try_nextval(name).unwrap_err();
        assert!(err.to_string().contains("reached minimum value"), "got: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn create_sequence_persists_across_reopen() {
        let _e2e = e2e_guard();
        let dir = scratch_dir();
        let name = "seq2_reopen";
        {
            let db = open_db(&dir);
            db.execute(&format!(
                "CREATE SEQUENCE {name} INCREMENT BY 5 MINVALUE 1 MAXVALUE 1000 START WITH 50 CACHE 1"
            ))
            .unwrap();
            assert_eq!(nextval(&db, name), 50);
        }
        // Drop the in-process runtime so the reopen rebuilds from the durable
        // definition + state (a real crash gives a fresh STORE).
        sequences::invalidate_cache(name);
        {
            let db = open_db(&dir);
            // Definition (INCREMENT 5) survived: resume strictly past 50.
            let v = nextval(&db, name);
            assert!(v > 50, "expected resume past 50, got {v}");
            // Increment of 5 is preserved across the reopen.
            let v2 = nextval(&db, name);
            assert_eq!(v2 - v, 5, "increment not preserved across reopen: {v} -> {v2}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn alter_sequence_restart_with() {
        let _e2e = e2e_guard();
        let dir = scratch_dir();
        let db = open_db(&dir);
        let name = "seq3_restart";
        db.execute(&format!("CREATE SEQUENCE {name} INCREMENT BY 1 START WITH 1 CACHE 32"))
            .unwrap();
        assert_eq!(nextval(&db, name), 1);
        assert_eq!(nextval(&db, name), 2);
        // RESTART discards the in-flight cached block; next value is EXACTLY 100.
        db.execute(&format!("ALTER SEQUENCE {name} RESTART WITH 100")).unwrap();
        assert_eq!(nextval(&db, name), 100, "RESTART WITH did not take effect");
        assert_eq!(nextval(&db, name), 101);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn alter_sequence_set_increment_and_maxvalue() {
        let _e2e = e2e_guard();
        let dir = scratch_dir();
        let db = open_db(&dir);
        let name = "seq3_setopts";
        db.execute(&format!("CREATE SEQUENCE {name} INCREMENT BY 1 START WITH 1 CACHE 1"))
            .unwrap();
        assert_eq!(nextval(&db, name), 1);
        // Change INCREMENT and clamp MAXVALUE.
        db.execute(&format!("ALTER SEQUENCE {name} INCREMENT BY 3 MAXVALUE 10"))
            .unwrap();
        // is_called was true (we served 1); next value steps from the durable
        // high-water by the NEW increment.
        let v = nextval(&db, name);
        assert_eq!((v - 1) % 3, 0, "new increment of 3 not applied (got {v})");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn alter_sequence_if_exists_missing_is_noop_else_errors() {
        let _e2e = e2e_guard();
        let dir = scratch_dir();
        let db = open_db(&dir);
        // IF EXISTS on a missing sequence is a no-op.
        db.execute("ALTER SEQUENCE seq3_absent_xyz IF EXISTS RESTART WITH 5")
            .ok();
        let ok = db.execute("ALTER SEQUENCE IF EXISTS seq3_absent_xyz RESTART WITH 5");
        assert!(ok.is_ok(), "ALTER SEQUENCE IF EXISTS missing should be a no-op: {ok:?}");
        // Without IF EXISTS, a missing sequence errors.
        let err = db.execute("ALTER SEQUENCE seq3_definitely_absent RESTART WITH 5");
        assert!(err.is_err(), "ALTER SEQUENCE on a missing sequence must error");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn drop_sequence_does_not_drop_table() {
        let _e2e = e2e_guard();
        let dir = scratch_dir();
        let db = open_db(&dir);
        // Regression for the planner.rs:430 latent bug where DROP SEQUENCE fell
        // through to DROP TABLE.
        db.execute("CREATE TABLE seq3_foo (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO seq3_foo (id) VALUES (1)").unwrap();
        db.execute("CREATE SEQUENCE seq3_foo_seq START WITH 1").unwrap();
        db.execute("DROP SEQUENCE seq3_foo_seq").unwrap();
        // The TABLE must still exist and be queryable.
        let rows = db.query("SELECT id FROM seq3_foo", &[]).unwrap();
        assert_eq!(rows.len(), 1, "DROP SEQUENCE wrongly dropped the table");
        // Dropping a missing sequence without IF EXISTS errors; with it, ok.
        assert!(db.execute("DROP SEQUENCE seq3_foo_seq").is_err());
        assert!(db.execute("DROP SEQUENCE IF EXISTS seq3_foo_seq").is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn create_sequence_if_not_exists_does_not_reset() {
        let _e2e = e2e_guard();
        let dir = scratch_dir();
        let db = open_db(&dir);
        let name = "seq2_ine";
        db.execute(&format!("CREATE SEQUENCE {name} START WITH 1")).unwrap();
        assert_eq!(nextval(&db, name), 1);
        assert_eq!(nextval(&db, name), 2);
        // IF NOT EXISTS on an existing sequence must NOT reset it.
        db.execute(&format!("CREATE SEQUENCE IF NOT EXISTS {name} START WITH 1000"))
            .unwrap();
        let v = nextval(&db, name);
        assert!(v >= 3, "IF NOT EXISTS wrongly reset the sequence (got {v})");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn setval_state_survives_reopen() {
        let _e2e = e2e_guard();
        let dir = scratch_dir();
        let name = "setval_resume_seq";

        // Session 1 — setval(., 5000, true): next nextval should be 5001 here,
        // and the state must persist.
        {
            let engine = open(&dir);
            Catalog::new(&engine).save_sequence(&sample_def(name, 1)).unwrap();
            sequences::install_persistence(&engine);
            sequences::install_runtime(&sample_def(name, 1), None);

            assert_eq!(sequences::try_setval(name, 5000, true).unwrap(), 5000);
            let st = Catalog::new(&engine).get_sequence_state(name).unwrap().unwrap();
            assert_eq!(st.last_reserved, 5000);
            assert!(st.is_called);
            engine.flush_wal().unwrap();
        }

        sequences::invalidate_cache(name);

        // Session 2 — after reopen, nextval continues past the setval point.
        {
            let engine = open(&dir);
            sequences::install_persistence(&engine);
            let v = sequences::try_nextval(name).unwrap();
            assert!(v > 5000, "expected nextval past setval(5000), got {}", v);
        }

        std::fs::remove_dir_all(&dir).ok();
    }
}
