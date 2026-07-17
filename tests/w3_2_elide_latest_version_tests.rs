//! W3.2: single-copy latest version (`storage.elide_latest_version`),
//! end-to-end through the SQL surface.
//!
//! Verifies that with the knob ON a versioned INSERT writes NO `v:` copy (the
//! ~65%-of-INSERT-bytes prize), that AS-OF reads across an insert→mutation
//! transition stay correct because the first mutation materializes the elided
//! version, and that the version GC naturally skips a flagged event that has no
//! `v:`. The knob is OFF by default (control test), so these all flip on
//! pre-W3.2 code (no elision, `v:` always written).
//!
//! Timing note (mirrors `version_gc_tests.rs`): version timestamps are logical
//! but AS OF TIMESTAMP resolves through snapshot-metadata wall clocks (second
//! resolution), so the AS-OF test uses >1s gaps.

use heliosdb_nano::{Config, EmbeddedDatabase, Value};
use tempfile::TempDir;

/// A persistent config on `path` with elision set to `on` and background
/// version-GC off (tests drive collection explicitly). Reopening the same path
/// with a different `on` is the only SQL-surface way to flip the open-time
/// elision knob mid-life (it is applied once at `configure_elision`).
fn persistent_config(path: &std::path::Path, on: bool) -> Config {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    config.storage.elide_latest_version = on;
    config.storage.version_gc_interval_secs = Some(0);
    config
}

fn db_with_elision(on: bool) -> EmbeddedDatabase {
    let mut config = Config::in_memory();
    config.storage.elide_latest_version = on;
    // Background version-GC worker off: tests drive collection explicitly.
    config.storage.version_gc_interval_secs = Some(0);
    EmbeddedDatabase::with_config(config).unwrap()
}

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int2(x) => *x as i64,
        Value::Int4(x) => *x as i64,
        Value::Int8(x) => *x,
        other => panic!("expected an integer value, got {:?}", other),
    }
}

fn now_sql_timestamp() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn sleep_secs(secs: f64) {
    std::thread::sleep(std::time::Duration::from_millis((secs * 1000.0) as u64));
}

// ---------------------------------------------------------------------------
// 1. The write-volume prize: elided INSERTs write zero `v:` copies.
//    (This doubles as the §7 re-measurement the coordinator runs live.)
// ---------------------------------------------------------------------------
#[test]
fn elision_collapses_version_keys_to_zero() {
    let db = db_with_elision(true);
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, val INT)").unwrap();
    for i in 0..20 {
        db.execute(&format!("INSERT INTO t (id, val) VALUES ({}, 0)", i))
            .unwrap();
    }

    let stats = db.version_storage_stats().unwrap();
    assert_eq!(stats.version_keys, 0, "every elided INSERT omits its `v:` copy");
    assert!(
        stats.version_index_keys >= 20,
        "the small `v_idx:` events survive (found {})",
        stats.version_index_keys
    );

    // Current reads are unaffected — the value is served from `data:`.
    let rows = db.query("SELECT val FROM t", &[]).unwrap();
    assert_eq!(rows.len(), 20);
    for row in &rows {
        assert_eq!(as_i64(&row.values[0]), 0);
    }
}

#[test]
fn without_elision_inserts_write_full_v_copies() {
    // Control: the SAME inserts under the default (elision OFF) DO write `v:`.
    let db = db_with_elision(false);
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, val INT)").unwrap();
    for i in 0..20 {
        db.execute(&format!("INSERT INTO t (id, val) VALUES ({}, 0)", i))
            .unwrap();
    }
    let stats = db.version_storage_stats().unwrap();
    assert!(
        stats.version_keys >= 20,
        "the default path writes a full `v:` per insert (found {})",
        stats.version_keys
    );
}

// ---------------------------------------------------------------------------
// 2. AS-OF across an insert→update transition: the first (versioned) mutation
//    materializes the elided insert version, so history is exact.
// ---------------------------------------------------------------------------
#[test]
fn as_of_before_update_returns_insert_value_under_elision() {
    let db = db_with_elision(true);
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, val INT)").unwrap();
    db.execute("INSERT INTO t (id, val) VALUES (1, 10)").unwrap();

    sleep_secs(1.2);
    let ts_after_insert = now_sql_timestamp(); // resolves to the insert state (val=10)
    sleep_secs(1.2);

    // Versioned (explicit-transaction) UPDATE: the commit path materializes the
    // elided insert version from `data:` BEFORE overwriting it.
    db.execute("BEGIN").unwrap();
    db.execute("UPDATE t SET val = 99 WHERE id = 1").unwrap();
    db.execute("COMMIT").unwrap();

    let rows = db
        .query(&format!("SELECT val FROM t AS OF TIMESTAMP '{}'", ts_after_insert), &[])
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        as_i64(&rows[0].values[0]),
        10,
        "AS-OF before the update must still resolve the insert value"
    );

    // Current read reflects the update.
    let rows = db.query("SELECT val FROM t WHERE id = 1", &[]).unwrap();
    assert_eq!(as_i64(&rows[0].values[0]), 99);
}

// ---------------------------------------------------------------------------
// 3. A fast (version-skipping) UPDATE still preserves the elided insert value
//    for AS-OF, by materializing before it overwrites `data:`.
// ---------------------------------------------------------------------------
#[test]
fn fast_update_materializes_elided_insert_for_as_of() {
    let db = db_with_elision(true);
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, val INT)").unwrap();
    db.execute("INSERT INTO t (id, val) VALUES (1, 7)").unwrap();

    sleep_secs(1.2);
    let ts_after_insert = now_sql_timestamp();
    sleep_secs(1.2);

    // Autocommit UPDATE (statement-level fast path — version-skipping).
    db.execute("UPDATE t SET val = 8 WHERE id = 1").unwrap();

    let rows = db
        .query(&format!("SELECT val FROM t AS OF TIMESTAMP '{}'", ts_after_insert), &[])
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        as_i64(&rows[0].values[0]),
        7,
        "AS-OF before a version-skipping fast UPDATE must still see the insert value"
    );
    let rows = db.query("SELECT val FROM t WHERE id = 1", &[]).unwrap();
    assert_eq!(as_i64(&rows[0].values[0]), 8, "current read reflects the fast UPDATE");
}

// ---------------------------------------------------------------------------
// 4. Version-GC naturally skips flagged events (no `v:` exists to reclaim), and
//    the data:-backed values are never disturbed while the rows live.
// ---------------------------------------------------------------------------
#[test]
fn vacuum_does_not_reclaim_elided_never_mutated_rows() {
    let mut config = Config::in_memory();
    config.storage.elide_latest_version = true;
    config.storage.version_retention = Some("1s".to_string());
    config.storage.version_gc_interval_secs = Some(0);
    let db = EmbeddedDatabase::with_config(config).unwrap();

    db.execute("CREATE TABLE t (id INT PRIMARY KEY, val INT)").unwrap();
    for i in 0..10 {
        db.execute(&format!("INSERT INTO t (id, val) VALUES ({}, {})", i, i))
            .unwrap();
    }

    sleep_secs(1.5);
    // The `v:` keyspace is empty (all inserts elided), so a full vacuum reclaims
    // nothing — a flagged event with no `v:` is naturally skipped by the GC's
    // `v:`-only scan.
    let reclaimed = db.vacuum_versions().unwrap();
    assert_eq!(reclaimed, 0, "elided rows have no `v:` copy to reclaim");

    let stats = db.version_storage_stats().unwrap();
    assert_eq!(stats.version_keys, 0);

    // The data:-backed values survive the vacuum, fully readable.
    let rows = db.query("SELECT id, val FROM t ORDER BY id", &[]).unwrap();
    assert_eq!(rows.len(), 10);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(as_i64(&row.values[1]), i as i64);
    }
}

// ---------------------------------------------------------------------------
// 5. Toggling the knob OFF after elided rows exist keeps reading them correctly
//    (per-row durable flag; no migration). New inserts then write full `v:`,
//    old rows stay elided — mixed old/new resolve per row. Reopen is the only
//    SQL-surface way to flip the open-time knob, so this uses a persistent dir.
// ---------------------------------------------------------------------------
#[test]
fn mixed_elided_and_full_rows_coexist_in_one_table() {
    let temp = TempDir::new().unwrap();
    let db_path = temp.path().join("mixed_db");

    // Session 1: elision ON — row 1 is elided (writes no `v:`).
    {
        let db = EmbeddedDatabase::with_config(persistent_config(&db_path, true)).unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, val INT)").unwrap();
        db.execute("INSERT INTO t (id, val) VALUES (1, 100)").unwrap();
        let stats = db.version_storage_stats().unwrap();
        assert_eq!(stats.version_keys, 0, "row 1 inserted under elision writes no `v:`");
    }

    // Session 2: elision OFF — row 2 writes a full `v:`; row 1 STAYS elided (its
    // per-row durable flag is untouched — inserting a different row never
    // materializes it).
    {
        let db = EmbeddedDatabase::with_config(persistent_config(&db_path, false)).unwrap();
        db.execute("INSERT INTO t (id, val) VALUES (2, 200)").unwrap();

        let stats = db.version_storage_stats().unwrap();
        assert_eq!(
            stats.version_keys, 1,
            "exactly one full `v:` — row 2's; row 1 remains elided (mixed per-row resolution)"
        );
        assert!(
            stats.version_index_keys >= 2,
            "both rows keep their `v_idx:` event (found {})",
            stats.version_index_keys
        );

        // Both rows read correctly, each through its own resolution path.
        let rows = db.query("SELECT id, val FROM t ORDER BY id", &[]).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(as_i64(&rows[0].values[1]), 100);
        assert_eq!(as_i64(&rows[1].values[1]), 200);
    }
}

// ---------------------------------------------------------------------------
// 6. ALTER TABLE ADD COLUMN is a `data:`-overwriting mutation funnel: it must
//    materialize an elided row's insert version BEFORE rewriting `data:`, else
//    an AS-OF read predating the ALTER resolves the flagged event to the widened
//    post-ALTER `data:` and W3.5 `null_pad` pads the added column from the wrong
//    pre-image. Regression for the W3.2 ADD/DROP-column blocker: on pre-fix code
//    the ALTER overwrote `data:` to (10, 99) with no materialize, so the flagged
//    event resolved to (10, 99) and the pre-ALTER AS-OF wrongly returned b = 99.
// ---------------------------------------------------------------------------
#[test]
fn as_of_before_alter_add_column_preserves_elided_pre_image() {
    let db = db_with_elision(true);
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, a INT)").unwrap();
    db.execute("INSERT INTO t (id, a) VALUES (1, 10)").unwrap(); // elided: `v:` absent

    sleep_secs(1.2);
    let ts_before_alter = now_sql_timestamp();
    sleep_secs(1.2);

    db.execute("ALTER TABLE t ADD COLUMN b INT DEFAULT 99").unwrap();

    // Pre-ALTER AS-OF must project the added column as NULL (W3.5 null_pad on the
    // materialized 2-value pre-image), NEVER the 99 DEFAULT backfill.
    let rows = db
        .query(&format!("SELECT * FROM t AS OF TIMESTAMP '{}'", ts_before_alter), &[])
        .expect("a pre-ALTER AS OF read must succeed");
    assert_eq!(rows.len(), 1);
    assert_eq!(as_i64(&rows[0].values[1]), 10, "column a resolves to the insert value");
    assert!(
        matches!(rows[0].values.get(2), Some(Value::Null)),
        "the column added after the AS-OF ts must project as NULL, not the DEFAULT backfill, got {:?}",
        rows[0].values.get(2)
    );

    // The live read observes the backfill (sanity: the ALTER landed).
    let fresh = db.query("SELECT a, b FROM t WHERE id = 1", &[]).unwrap();
    assert_eq!(as_i64(&fresh[0].values[0]), 10);
    assert_eq!(
        as_i64(&fresh[0].values[1]),
        99,
        "the current read observes the DEFAULT backfill"
    );
}
