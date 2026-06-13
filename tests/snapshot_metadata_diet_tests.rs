//! R1.4: snapshot metadata diet — `txn_map:`/`scn_map:` keys are no longer
//! written (recovery rebuilds everything from `snapshot:` entries alone).
//! These tests pin the recovery contract across a process restart.

use heliosdb_nano::{Config, EmbeddedDatabase, Value};

#[test]
fn as_of_survives_restart_without_mapping_keys() {
    let dir = std::env::temp_dir().join(format!("helios_r14_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let open = || {
        let mut c = Config::default();
        c.storage.path = Some(dir.clone());
        c.storage.memory_only = false;
        c.storage.wal_enabled = true;
        EmbeddedDatabase::with_config(c).unwrap()
    };

    let ts_between;
    {
        let db = open();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
            .unwrap();
        db.execute("INSERT INTO t (id, v) VALUES (1, 10)").unwrap();
        // A wall-clock anchor between the two states.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        ts_between = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        db.execute("UPDATE t SET v = 20 WHERE id = 1").unwrap();
        db.execute("INSERT INTO t (id, v) VALUES (2, 30)").unwrap();
    }

    // Reopen: snapshot recovery must work from `snapshot:` keys alone.
    let db = open();
    let now_rows = db.query("SELECT * FROM t", &[]).unwrap();
    assert_eq!(now_rows.len(), 2, "current state after restart");

    let old_rows = db
        .query(&format!("SELECT * FROM t AS OF TIMESTAMP '{ts_between}'"), &[])
        .unwrap();
    assert_eq!(old_rows.len(), 1, "AS OF must see exactly the pre-anchor row");
    assert_eq!(old_rows[0].values[1], Value::Int4(10), "AS OF must see the OLD value");

    let _ = std::fs::remove_dir_all(&dir);
}
