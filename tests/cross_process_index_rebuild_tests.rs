//! Cross-process / cross-open ART index rebuild tests.
//!
//! Verify that when a fresh `EmbeddedDatabase` attaches to an existing
//! on-disk data directory the in-memory ART (PK + UNIQUE + FK) indexes are
//! re-populated from on-disk rows, so PK lookups and conflict detection
//! work without falling back to table scans.
//!
//! Without `Catalog::rebuild_all_indexes()` running on open, these tests
//! all fail or return wrong rows: the second `EmbeddedDatabase::new` would
//! see an empty ART, treat the existing rows as non-existent, and let
//! INSERT-OR-REPLACE create duplicates instead of upserting.

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod cross_process_rebuild {
    use heliosdb_nano::{EmbeddedDatabase, Value};

    /// A unique scratch directory for each test invocation.
    fn scratch_dir() -> std::path::PathBuf {
        let id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("nano_xproc_{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn to_i64(v: &Value) -> i64 {
        match v {
            Value::Int2(n) => *n as i64,
            Value::Int4(n) => *n as i64,
            Value::Int8(n) => *n,
            other => panic!("expected integer, got {:?}", other),
        }
    }

    fn to_str(v: &Value) -> String {
        match v {
            Value::String(s) => s.clone(),
            other => panic!("expected string, got {:?}", other),
        }
    }

    #[test]
    fn pk_lookup_works_after_reopen() {
        let dir = scratch_dir();

        // Session 1 — create + populate.
        {
            let db = EmbeddedDatabase::new(&dir).unwrap();
            db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)").unwrap();
            db.execute("INSERT INTO users (id, name) VALUES (1, 'alice')").unwrap();
            db.execute("INSERT INTO users (id, name) VALUES (2, 'bob')").unwrap();
        }

        // Session 2 — reopen and look up by PK. With ART rebuild this hits
        // the index path; without it the planner falls back to a scan but
        // still finds the row, so we additionally check the upsert below
        // to detect index emptiness.
        {
            let db = EmbeddedDatabase::new(&dir).unwrap();
            let rows = db.query("SELECT name FROM users WHERE id = 1", &[]).unwrap();
            assert_eq!(rows.len(), 1, "PK lookup after reopen returned no row");
            assert_eq!(to_str(&rows[0].values[0]), "alice");

            let rows = db.query("SELECT name FROM users WHERE id = 2", &[]).unwrap();
            assert_eq!(to_str(&rows[0].values[0]), "bob");
        }
    }

    #[test]
    fn insert_or_replace_upserts_across_reopens() {
        let dir = scratch_dir();

        // Session 1 — insert original row.
        {
            let db = EmbeddedDatabase::new(&dir).unwrap();
            db.execute(
                "CREATE TABLE files (path TEXT PRIMARY KEY, mtime REAL, size INTEGER)",
            ).unwrap();
            db.execute(
                "INSERT INTO files (path, mtime, size) VALUES ('/x', 1.0, 100)",
            ).unwrap();
        }

        // Session 2 — INSERT OR REPLACE the same PK. Without ART rebuild
        // the conflict isn't detected and we'd end up with two rows for
        // path='/x'. With rebuild, the existing row is updated in place.
        {
            let db = EmbeddedDatabase::new(&dir).unwrap();
            db.execute(
                "INSERT OR REPLACE INTO files (path, mtime, size) VALUES ('/x', 2.0, 200)",
            ).unwrap();

            let rows = db.query("SELECT path, size FROM files", &[]).unwrap();
            assert_eq!(rows.len(), 1, "expected one row, got {}", rows.len());
            assert_eq!(to_str(&rows[0].values[0]), "/x");
            assert_eq!(to_i64(&rows[0].values[1]), 200);

            // Lookup by PK must also return exactly the upserted row.
            let rows = db.query("SELECT size FROM files WHERE path = '/x'", &[]).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(to_i64(&rows[0].values[0]), 200);
        }
    }

    #[test]
    fn pk_uniqueness_enforced_after_reopen() {
        let dir = scratch_dir();

        {
            let db = EmbeddedDatabase::new(&dir).unwrap();
            db.execute("CREATE TABLE kv (k TEXT PRIMARY KEY, v TEXT)").unwrap();
            db.execute("INSERT INTO kv (k, v) VALUES ('foo', 'one')").unwrap();
        }

        // A naked INSERT (no ON CONFLICT) of the same PK in a fresh process
        // must raise. Without ART rebuild it would silently succeed and
        // produce two rows for k='foo'.
        {
            let db = EmbeddedDatabase::new(&dir).unwrap();
            let res = db.execute("INSERT INTO kv (k, v) VALUES ('foo', 'two')");
            assert!(
                res.is_err(),
                "expected unique-violation on duplicate PK after reopen, got Ok"
            );

            let rows = db.query("SELECT v FROM kv WHERE k = 'foo'", &[]).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(to_str(&rows[0].values[0]), "one");
        }
    }

    #[test]
    fn unique_constraint_enforced_after_reopen() {
        let dir = scratch_dir();

        {
            let db = EmbeddedDatabase::new(&dir).unwrap();
            db.execute(
                "CREATE TABLE accounts (id INT PRIMARY KEY, email TEXT UNIQUE)",
            ).unwrap();
            db.execute(
                "INSERT INTO accounts (id, email) VALUES (1, 'a@b.com')",
            ).unwrap();
        }

        // Same email, different PK — must trip the UNIQUE index after reopen.
        {
            let db = EmbeddedDatabase::new(&dir).unwrap();
            let res = db.execute(
                "INSERT INTO accounts (id, email) VALUES (2, 'a@b.com')",
            );
            assert!(
                res.is_err(),
                "expected unique-violation on duplicate email after reopen"
            );
        }
    }

    #[test]
    fn rebuild_handles_empty_database() {
        // Open an empty data dir, close, reopen — must not crash.
        let dir = scratch_dir();
        {
            let _ = EmbeddedDatabase::new(&dir).unwrap();
        }
        {
            let _ = EmbeddedDatabase::new(&dir).unwrap();
        }
    }

    #[test]
    fn on_conflict_named_column_upserts_after_reopen() {
        // FEATURE_REQUEST_cross_process_on_conflict.md repro.
        //
        // `INSERT OR REPLACE` (translated to bare `ON CONFLICT DO UPDATE`)
        // is already covered by `insert_or_replace_upserts_across_reopens`.
        // The bug specific to a *named-column* ON CONFLICT
        // (`ON CONFLICT(path) DO UPDATE …`) — the form a plugin emits
        // directly — must also detect prior-process committed rows.
        let dir = scratch_dir();

        // Process 1.
        {
            let db = EmbeddedDatabase::new(&dir).unwrap();
            db.execute(
                "CREATE TABLE src (path TEXT PRIMARY KEY, content TEXT)",
            ).unwrap();
            db.execute_params(
                "INSERT INTO src (path, content) VALUES ($1, $2) \
                 ON CONFLICT(path) DO UPDATE SET content = excluded.content",
                &[Value::String("a.rs".into()), Value::String("v1".into())],
            ).unwrap();
        }

        // Process 2: re-attach, run the same upsert. Expected: 1 row,
        // content='v2'. Without the FR fix: 2 rows.
        {
            let db = EmbeddedDatabase::new(&dir).unwrap();
            db.execute_params(
                "INSERT INTO src (path, content) VALUES ($1, $2) \
                 ON CONFLICT(path) DO UPDATE SET content = excluded.content",
                &[Value::String("a.rs".into()), Value::String("v2".into())],
            ).unwrap();

            let rows = db.query("SELECT path, content FROM src", &[]).unwrap();
            assert_eq!(
                rows.len(), 1,
                "named-column ON CONFLICT failed to detect prior-process row — {} rows present",
                rows.len()
            );
            assert_eq!(to_str(&rows[0].values[0]), "a.rs");
            assert_eq!(to_str(&rows[0].values[1]), "v2");
        }
    }

    #[test]
    fn rebuild_skips_helios_internal_tables() {
        // helios_* prefix tables are skipped by the rebuild loop. This test
        // simply confirms that opening a DB with a helios_* present (which
        // we can't easily create from SQL) doesn't break, by going through
        // the empty-DB path.
        let dir = scratch_dir();
        let db = EmbeddedDatabase::new(&dir).unwrap();
        db.execute("CREATE TABLE user_data (id INT PRIMARY KEY)").unwrap();
        drop(db);
        let _ = EmbeddedDatabase::new(&dir).unwrap();
    }
}
