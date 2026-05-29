//! Regression tests for the P0#1 read-side gate (verification finding #3/#6):
//! with `time_travel_enabled=false`, the commit path stops writing version keys,
//! so the read path must NOT consult stale pre-existing `v_idx:` history, and
//! `AS OF` queries must error rather than silently return current state.

use heliosdb_nano::{Config, EmbeddedDatabase, Value};

/// #3: a row that has version history from when TT was on, then UPDATEd under
/// TT-off, must read back the LATEST value inside a transaction (snapshot read),
/// not the stale newest version. Before the fix, the txn snapshot read consulted
/// `v_idx:` and returned the stale version while `data:` had advanced.
#[test]
fn tt_off_txn_read_returns_latest_not_stale_version() {
    let dir = std::env::temp_dir().join(format!("helios_p01_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Phase 1 — time-travel ON: build version history for id=1 (v: 10 -> 20).
    {
        let mut c = Config::default();
        c.storage.path = Some(dir.clone());
        c.storage.memory_only = false;
        c.storage.time_travel_enabled = true;
        let db = EmbeddedDatabase::with_config(c).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)").unwrap();
        db.execute("INSERT INTO t (id, v) VALUES (1, 10)").unwrap();
        db.execute("UPDATE t SET v = 20 WHERE id = 1").unwrap();
    }

    // Phase 2 — reopen with time-travel OFF, advance data:, read inside a txn.
    {
        let mut c = Config::default();
        c.storage.path = Some(dir.clone());
        c.storage.memory_only = false;
        c.storage.time_travel_enabled = false;
        let db = EmbeddedDatabase::with_config(c).unwrap();
        db.execute("UPDATE t SET v = 30 WHERE id = 1").unwrap(); // data:=30, no new v_idx

        db.execute("BEGIN").unwrap();
        let rows = db.query("SELECT v FROM t WHERE id = 1", &[]).unwrap();
        db.execute("COMMIT").unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::Int4(30),
            "transactional snapshot read with TT-off must return latest (30), not a stale version"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// #6: `AS OF` queries must error when time_travel_enabled=false (no version
/// history is maintained, so they cannot be answered) instead of silently
/// returning current state.
#[test]
fn tt_off_as_of_query_errors() {
    let mut c = Config::in_memory();
    c.storage.time_travel_enabled = false;
    let db = EmbeddedDatabase::with_config(c).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)").unwrap();
    db.execute("INSERT INTO t (id, v) VALUES (1, 10)").unwrap();
    db.execute("UPDATE t SET v = 20 WHERE id = 1").unwrap();

    let res = db.query("SELECT v FROM t AS OF TIMESTAMP '2000-01-01 00:00:00' WHERE id = 1", &[]);
    assert!(
        res.is_err(),
        "AS OF query with time_travel_enabled=false must error, got: {:?}",
        res.map(|r| r.len())
    );
}

/// Sanity: with the default config (time_travel_enabled=true), AS OF still works
/// (resolves and returns rows) — the gate must not regress the default feature.
#[test]
fn tt_on_as_of_still_works() {
    let db = EmbeddedDatabase::new_in_memory().unwrap(); // TT on by default
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)").unwrap();
    db.execute("INSERT INTO t (id, v) VALUES (1, 10)").unwrap();
    // A future timestamp resolves to the latest snapshot; query must succeed.
    let res = db.query("SELECT v FROM t AS OF TIMESTAMP '2999-01-01 00:00:00' WHERE id = 1", &[]);
    assert!(res.is_ok(), "AS OF with TT-on must succeed, got: {:?}", res.err());
}
