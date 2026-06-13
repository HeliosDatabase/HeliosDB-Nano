//! R4.3: MVCC version garbage collection — retention watermark, collector,
//! VACUUM VERSIONS, pinning (readers/transactions/branches), and the
//! beyond-horizon error contract.
//!
//! Timing note: version timestamps are logical; the retention window is
//! wall-clock, mapped through snapshot-metadata wall clocks (second
//! resolution). Tests therefore use 1-2s retentions and >= 2s sleeps.

use heliosdb_nano::{Config, EmbeddedDatabase, Value};

fn db_with_retention(retention: &str) -> EmbeddedDatabase {
    let mut config = Config::in_memory();
    config.storage.version_retention = Some(retention.to_string());
    // Background worker off: every test drives collection explicitly.
    config.storage.version_gc_interval_secs = Some(0);
    EmbeddedDatabase::with_config(config).unwrap()
}

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int2(x) => *x as i64,
        Value::Int4(x) => *x as i64,
        Value::Int8(x) => *x,
        other => panic!("expected integer value, got {:?}", other),
    }
}

fn now_sql_timestamp() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn sleep_secs(secs: f64) {
    std::thread::sleep(std::time::Duration::from_millis((secs * 1000.0) as u64));
}

/// Run `rounds` full-table transactional updates (the commit path writes
/// complete `v:` + `v_idx:` chains, unlike the statement-level fast paths).
fn update_rounds(db: &EmbeddedDatabase, table: &str, rounds: usize) {
    for _ in 0..rounds {
        db.execute("BEGIN").unwrap();
        db.execute(&format!("UPDATE {} SET val = val + 1", table)).unwrap();
        db.execute("COMMIT").unwrap();
    }
}

fn setup_rows(db: &EmbeddedDatabase, table: &str, rows: usize) {
    db.execute(&format!("CREATE TABLE {} (id INT PRIMARY KEY, val INT)", table))
        .unwrap();
    for i in 0..rows {
        db.execute(&format!("INSERT INTO {} (id, val) VALUES ({}, 0)", table, i))
            .unwrap();
    }
}

// ---------------------------------------------------------------------------
// 1. Grow → update → GC → verify reclaim (version-key count before/after).
// ---------------------------------------------------------------------------
#[test]
fn vacuum_reclaims_dead_versions() {
    let db = db_with_retention("1s");
    const ROWS: usize = 50;
    const ROUNDS: usize = 5;
    setup_rows(&db, "t", ROWS);
    update_rounds(&db, "t", ROUNDS);

    let before = db.version_storage_stats().unwrap();
    assert!(
        before.version_keys >= (ROWS * (ROUNDS + 1)) as u64,
        "expected at least {} version keys before GC, found {}",
        ROWS * (ROUNDS + 1),
        before.version_keys
    );

    sleep_secs(2.0); // let the whole history age past the 1s retention

    let collected = db.vacuum_versions().unwrap();
    let after = db.version_storage_stats().unwrap();

    // Chain preservation: exactly the newest version per row survives.
    assert_eq!(
        after.version_keys, ROWS as u64,
        "expected one surviving version per row, stats: {:?}",
        after
    );
    assert_eq!(collected, before.version_keys - after.version_keys);
    assert!(after.approx_bytes < before.approx_bytes);

    // Current reads unaffected: every row went through ROUNDS increments.
    let rows = db.query("SELECT val FROM t", &[]).unwrap();
    assert_eq!(rows.len(), ROWS);
    for row in &rows {
        assert_eq!(as_i64(&row.values[0]), ROUNDS as i64);
    }

    // Idempotent: an immediate second pass collects nothing new.
    let again = db.vacuum_versions().unwrap();
    assert_eq!(again, 0, "second VACUUM must be a no-op");
    let after2 = db.version_storage_stats().unwrap();
    assert_eq!(after2.version_keys, after.version_keys);
}

// ---------------------------------------------------------------------------
// 2. Time-travel inside the horizon stays exact; beyond it errors clearly.
// ---------------------------------------------------------------------------
#[test]
fn time_travel_inside_horizon_exact_beyond_horizon_errors() {
    let db = db_with_retention("2s");
    setup_rows(&db, "tt", 3);

    sleep_secs(1.2);
    let ts_after_insert = now_sql_timestamp(); // resolves to the insert state (val=0)

    sleep_secs(1.2);
    update_rounds(&db, "tt", 1); // val=1  (this becomes the horizon version)

    sleep_secs(1.2);
    let ts_between = now_sql_timestamp(); // resolves to val=1, AT/above the horizon

    sleep_secs(2.0);
    update_rounds(&db, "tt", 1); // val=2, inside retention (fresh)

    let collected = db.vacuum_versions().unwrap();
    assert!(collected > 0, "expected the insert versions to be collected");

    // Inside the horizon: exact historical answer.
    let rows = db
        .query(&format!("SELECT val FROM tt AS OF TIMESTAMP '{}'", ts_between), &[])
        .unwrap();
    assert_eq!(rows.len(), 3);
    for row in &rows {
        assert_eq!(as_i64(&row.values[0]), 1, "AS OF inside horizon must be exact");
    }

    // Beyond the horizon: a clear error, never wrong data.
    let err = db
        .query(
            &format!("SELECT val FROM tt AS OF TIMESTAMP '{}'", ts_after_insert),
            &[],
        )
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("version-GC low watermark") || msg.contains("version_retention"),
        "expected a version-GC horizon error, got: {}",
        msg
    );

    // Current reads unaffected.
    let rows = db.query("SELECT val FROM tt", &[]).unwrap();
    for row in &rows {
        assert_eq!(as_i64(&row.values[0]), 2);
    }
}

// ---------------------------------------------------------------------------
// 3. A long-running reader (open transaction) blocks GC past its snapshot.
// ---------------------------------------------------------------------------
#[test]
#[allow(deprecated)] // begin_transaction(): the RAII handle is exactly the long-lived reader we need
fn active_reader_blocks_gc_past_its_snapshot() {
    let db = db_with_retention("1s");
    const ROWS: usize = 10;
    setup_rows(&db, "r", ROWS);
    update_rounds(&db, "r", 2); // versions: insert(0), u1(1), u2(2)

    // Reader snapshots AFTER u2: it must keep seeing val=2.
    let reader = db.begin_transaction().unwrap();

    update_rounds(&db, "r", 2); // u3(3), u4(4) — younger than the reader

    sleep_secs(2.0); // everything is past the 1s retention now

    let collected = db.vacuum_versions().unwrap();
    assert!(collected > 0);

    // GC was clamped at the reader's snapshot: u2 (its visible version)
    // survives, as do u3/u4 (above the clamped horizon). Only insert+u1
    // were collectible: 3 versions per row remain.
    let mid = db.version_storage_stats().unwrap();
    assert_eq!(
        mid.version_keys,
        (ROWS * 3) as u64,
        "reader pin must keep its visible version and everything newer"
    );

    // The reader still reads its exact snapshot.
    let rows = reader.query("SELECT val FROM r", &[]).unwrap();
    assert_eq!(rows.len(), ROWS);
    for row in &rows {
        assert_eq!(as_i64(&row.values[0]), 2, "open reader must see its snapshot");
    }

    // Release the reader: GC may now advance to the retention horizon.
    drop(reader);
    let collected = db.vacuum_versions().unwrap();
    assert!(collected > 0);
    let after = db.version_storage_stats().unwrap();
    assert_eq!(after.version_keys, ROWS as u64, "one version per row after release");
}

// ---------------------------------------------------------------------------
// 4. Branch anchors pin history; dropping the branch releases the pin;
//    branching from an anchor beyond the horizon errors cleanly.
// ---------------------------------------------------------------------------
#[test]
fn branch_anchor_pins_history_until_dropped() {
    let db = db_with_retention("1s");
    const ROWS: usize = 10;
    setup_rows(&db, "b", ROWS);

    let ts_before_branch = now_sql_timestamp();
    sleep_secs(1.2);

    db.execute("CREATE BRANCH pinner FROM main AS OF NOW").unwrap();

    update_rounds(&db, "b", 3); // all younger than the branch anchor
    sleep_secs(2.0);

    // The branch anchor clamps the horizon below every post-anchor
    // version; the only version per row at-or-below the anchor is the
    // insert itself (which is kept). Nothing is collectible.
    let before = db.version_storage_stats().unwrap();
    let collected = db.vacuum_versions().unwrap();
    assert_eq!(collected, 0, "active branch must pin its anchor history");
    let mid = db.version_storage_stats().unwrap();
    assert_eq!(mid.version_keys, before.version_keys);

    // Dropping the branch releases the pin.
    db.execute("DROP BRANCH pinner").unwrap();
    let collected = db.vacuum_versions().unwrap();
    assert!(collected > 0, "dropping the branch must unpin history");
    let after = db.version_storage_stats().unwrap();
    assert_eq!(after.version_keys, ROWS as u64);

    // Branching from a historical anchor beyond the horizon errors cleanly.
    let err = db
        .execute(&format!(
            "CREATE BRANCH too_old FROM main AS OF TIMESTAMP '{}'",
            ts_before_branch
        ))
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("version-GC low watermark") || msg.contains("version_retention"),
        "expected a version-GC horizon error for the stale branch anchor, got: {}",
        msg
    );
}

// ---------------------------------------------------------------------------
// 5. VACUUM VERSIONS SQL surface returns the reclaimed count.
// ---------------------------------------------------------------------------
#[test]
fn vacuum_versions_sql_statement_returns_count() {
    let db = db_with_retention("1s");
    const ROWS: usize = 8;
    const ROUNDS: usize = 4;
    setup_rows(&db, "v", ROWS);
    update_rounds(&db, "v", ROUNDS);
    sleep_secs(2.0);

    let before = db.version_storage_stats().unwrap();

    // Result-set surface: one row, named column, reclaimed count.
    let (rows, cols) = db.query_with_columns("VACUUM VERSIONS").unwrap();
    assert_eq!(cols, vec!["versions_collected".to_string()]);
    assert_eq!(rows.len(), 1);
    let counted = as_i64(&rows[0].values[0]) as u64;
    let after = db.version_storage_stats().unwrap();
    assert_eq!(counted, before.version_keys - after.version_keys);
    assert!(counted > 0);

    // execute() surface: count as the statement result; idempotent now.
    assert_eq!(db.execute("VACUUM VERSIONS;").unwrap(), 0);

    // query() surface also works.
    let rows = db.query("VACUUM VERSIONS", &[]).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(as_i64(&rows[0].values[0]), 0);
}

// ---------------------------------------------------------------------------
// 6. retention = infinite (default): VACUUM is a no-op, history untouched.
// ---------------------------------------------------------------------------
#[test]
fn infinite_retention_is_a_complete_noop() {
    let db = EmbeddedDatabase::with_config(Config::in_memory()).unwrap();
    setup_rows(&db, "n", 5);

    sleep_secs(1.2);
    let ts_old = now_sql_timestamp();
    sleep_secs(1.2);
    update_rounds(&db, "n", 3);
    sleep_secs(1.2);

    let before = db.version_storage_stats().unwrap();
    assert_eq!(db.vacuum_versions().unwrap(), 0);
    assert_eq!(db.execute("VACUUM VERSIONS").unwrap(), 0);
    let after = db.version_storage_stats().unwrap();
    assert_eq!(before.version_keys, after.version_keys);
    assert_eq!(before.version_index_keys, after.version_index_keys);

    // Arbitrarily old time-travel still works (no watermark ever set).
    let rows = db
        .query(&format!("SELECT val FROM n AS OF TIMESTAMP '{}'", ts_old), &[])
        .unwrap();
    assert_eq!(rows.len(), 5);
    for row in &rows {
        assert_eq!(as_i64(&row.values[0]), 0, "pre-update state must be intact");
    }
}

// ---------------------------------------------------------------------------
// 7. Crash/restart safety: the watermark survives reopen (beyond-horizon
//    reads keep erroring) and collection is idempotent across reopen.
// ---------------------------------------------------------------------------
#[test]
fn watermark_survives_reopen_and_collection_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let make_config = || {
        let mut config = Config::default();
        config.storage.path = Some(dir.path().to_path_buf());
        config.storage.version_retention = Some("1s".to_string());
        config.storage.version_gc_interval_secs = Some(0);
        config
    };

    let ts_old;
    let surviving_keys;
    {
        let db = EmbeddedDatabase::with_config(make_config()).unwrap();
        setup_rows(&db, "w", 5);
        sleep_secs(1.2);
        ts_old = now_sql_timestamp();
        sleep_secs(1.2);
        update_rounds(&db, "w", 3);
        sleep_secs(2.0);

        assert!(db.vacuum_versions().unwrap() > 0);
        surviving_keys = db.version_storage_stats().unwrap().version_keys;

        // Beyond-horizon error before reopen.
        assert!(db
            .query(&format!("SELECT val FROM w AS OF TIMESTAMP '{}'", ts_old), &[])
            .is_err());
        db.close().unwrap();
    }

    {
        let db = EmbeddedDatabase::with_config(make_config()).unwrap();

        // Watermark recovered: beyond-horizon reads still error.
        let err = db
            .query(&format!("SELECT val FROM w AS OF TIMESTAMP '{}'", ts_old), &[])
            .unwrap_err();
        assert!(
            err.to_string().contains("version-GC low watermark"),
            "expected recovered watermark error, got: {}",
            err
        );

        // Idempotent across reopen: nothing further to collect.
        assert_eq!(db.vacuum_versions().unwrap(), 0);
        assert_eq!(db.version_storage_stats().unwrap().version_keys, surviving_keys);

        // Current data intact.
        let rows = db.query("SELECT val FROM w", &[]).unwrap();
        assert_eq!(rows.len(), 5);
        for row in &rows {
            assert_eq!(as_i64(&row.values[0]), 3);
        }
    }
}

// ---------------------------------------------------------------------------
// 8. Mixed indexed/un-indexed chains: the PARAMETERIZED autocommit
//    UPDATE path (`execute_params` with no open transaction — the
//    extended-query wire path) runs `update_tuples_branch_aware`, which
//    writes `v:` versions WITHOUT `v_idx:` entries, while snapshot reads
//    resolve through `v_idx:` only (C15 interaction). The simple-query
//    `execute()` path wraps DML in an implicit transaction whose commit
//    writes fully indexed chains, so it cannot reproduce this. When the
//    newest version at-or-below the horizon is un-indexed, the collector
//    must also keep the newest INDEXED version at-or-below the horizon —
//    otherwise an AS OF read at t >= horizon silently changes its answer
//    after GC (falls back to the current data key here).
// ---------------------------------------------------------------------------
#[test]
fn gc_preserves_indexed_reads_over_unindexed_autocommit_versions() {
    let db = db_with_retention("1s");
    const ROWS: usize = 5;
    setup_rows(&db, "m", ROWS);

    update_rounds(&db, "m", 1); // val=1, transactional commit => indexed version
    sleep_secs(1.2);
    // Parameterized autocommit update: un-indexed `v:` version only.
    db.execute_params("UPDATE m SET val = val + 10", &[]).unwrap(); // val=11
    sleep_secs(1.2);
    let ts_probe = now_sql_timestamp(); // resolves AT the horizon after GC

    let as_of = format!("SELECT id, val FROM m AS OF TIMESTAMP '{}'", ts_probe);
    let snapshot_of = |db: &EmbeddedDatabase| -> Vec<(i64, i64)> {
        let mut rows: Vec<(i64, i64)> = db
            .query(&as_of, &[])
            .unwrap()
            .iter()
            .map(|r| (as_i64(&r.values[0]), as_i64(&r.values[1])))
            .collect();
        rows.sort_unstable();
        rows
    };

    let before = snapshot_of(&db);
    assert_eq!(before.len(), ROWS);

    sleep_secs(2.0); // age the whole history past the 1s retention
    let collected = db.vacuum_versions().unwrap();
    assert!(collected > 0);

    // The AS OF answer at t >= horizon is EXACTLY what it was before GC.
    assert_eq!(
        snapshot_of(&db),
        before,
        "GC must not change indexed AS OF results at t >= horizon"
    );

    // Chain rule for mixed chains: the un-indexed newest <= horizon AND
    // the newest indexed <= horizon both survive (2 versions per row);
    // older fully-indexed versions (the INSERT's) are collected.
    let stats = db.version_storage_stats().unwrap();
    assert_eq!(
        stats.version_keys,
        (ROWS * 2) as u64,
        "expected keep + keep_indexed per row, stats: {:?}",
        stats
    );
    assert_eq!(
        stats.version_index_keys, ROWS as u64,
        "expected exactly the keep_indexed index entry per row, stats: {:?}",
        stats
    );

    // Current reads unaffected.
    let rows = db.query("SELECT val FROM m", &[]).unwrap();
    assert_eq!(rows.len(), ROWS);
    for row in &rows {
        assert_eq!(as_i64(&row.values[0]), 11);
    }
}

// ---------------------------------------------------------------------------
// 9. Background worker collects on its own (no manual VACUUM).
// ---------------------------------------------------------------------------
#[test]
fn background_worker_collects_without_vacuum() {
    let mut config = Config::in_memory();
    config.storage.version_retention = Some("1s".to_string());
    config.storage.version_gc_interval_secs = Some(1);
    let db = EmbeddedDatabase::with_config(config).unwrap();

    const ROWS: usize = 10;
    setup_rows(&db, "bg", ROWS);
    update_rounds(&db, "bg", 4);

    let before = db.version_storage_stats().unwrap();
    assert!(before.version_keys >= (ROWS * 5) as u64);

    // retention 1s + 1s cycle interval: a few seconds suffice.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let stats = db.version_storage_stats().unwrap();
        if stats.version_keys == ROWS as u64 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "background GC did not reclaim versions in time: {:?}",
            stats
        );
        sleep_secs(0.25);
    }

    // Writes stayed correct underneath the background collector.
    let rows = db.query("SELECT val FROM bg", &[]).unwrap();
    assert_eq!(rows.len(), ROWS);
    for row in &rows {
        assert_eq!(as_i64(&row.values[0]), 4);
    }
}
