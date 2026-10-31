//! R4.2 durable index tests.
//!
//! Covers: restart survival via snapshots (no row scan at open), crash
//! between checkpoint and later mutations (markers invalidated → scan
//! rebuild, never a stale snapshot), marker consumption at open, HNSW graph
//! reload, and the open-transaction guard on the close-time checkpoint.
//!
//! Also covers the ORDER of the two durable writes `EmbeddedDatabase::drop`
//! performs — the row-counter flush and the index-snapshot checkpoint. They
//! are independent but not atomic with each other, and a crash landing between
//! them has an asymmetric outcome, so the counter flush must run first. See
//! the `drop_*` / `*_counter_*` tests at the bottom of this file.

use heliosdb_nano::storage::ArtIndexManager;
use heliosdb_nano::{EmbeddedDatabase, Value};
use tempfile::TempDir;

fn index_row_count(db: &EmbeddedDatabase, index_name: &str, value: Value) -> usize {
    let key = ArtIndexManager::encode_key(&[value]);
    db.storage.art_indexes().index_get_all(index_name, &key).len()
}

fn count_rows(db: &EmbeddedDatabase, sql: &str) -> i64 {
    let rows = db.query(sql, &[]).unwrap();
    match rows[0].values.first() {
        Some(Value::Int8(n)) => *n,
        Some(Value::Int4(n)) => i64::from(*n),
        other => panic!("expected integer count, got {other:?}"),
    }
}

#[test]
fn clean_restart_loads_art_snapshot_without_row_scan() {
    let temp = TempDir::new().unwrap();

    {
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        db.execute("CREATE TABLE d_users (id INT PRIMARY KEY, email TEXT, bucket INT)")
            .unwrap();
        for i in 0..300 {
            db.execute(&format!(
                "INSERT INTO d_users VALUES ({}, 'user{}@example.com', {})",
                i,
                i % 50,
                i % 7
            ))
            .unwrap();
        }
        db.execute("CREATE INDEX idx_d_users_email ON d_users(email)").unwrap();
        db.execute("CREATE INDEX idx_d_users_bucket ON d_users(bucket)")
            .unwrap();
        // Clean drop → checkpoint written.
    }

    let db = EmbeddedDatabase::new(temp.path()).unwrap();
    let report = db
        .storage
        .last_index_open_report()
        .expect("open must record an index open report");
    assert_eq!(
        report.tables_from_snapshot, 1,
        "the only user table must load from the snapshot, report: {report:?}"
    );
    assert_eq!(
        report.tables_scanned, 0,
        "no table may be scan-rebuilt, report: {report:?}"
    );
    assert_eq!(report.rows_scanned, 0, "snapshot load must not replay rows");
    assert!(report.entries_loaded >= 900, "PK + 2 secondary indexes x 300 rows");

    // Index lookups behave exactly as a scan rebuild would have produced.
    assert_eq!(
        index_row_count(&db, "idx_d_users_email", Value::String("user7@example.com".into())),
        6,
        "300 rows mod 50 → 6 rows per email"
    );
    assert_eq!(
        count_rows(&db, "SELECT COUNT(*) FROM d_users WHERE email = 'user7@example.com'"),
        6
    );
    // PK constraint state survived: duplicate insert must fail.
    assert!(db
        .execute("INSERT INTO d_users VALUES (5, 'dup@example.com', 0)")
        .is_err());
    // And the indexes keep tracking new mutations.
    db.execute("INSERT INTO d_users VALUES (1000, 'user7@example.com', 1)")
        .unwrap();
    assert_eq!(
        index_row_count(&db, "idx_d_users_email", Value::String("user7@example.com".into())),
        7
    );
}

#[test]
fn crash_after_checkpoint_with_later_mutations_rebuilds_from_rows() {
    let temp = TempDir::new().unwrap();

    {
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        db.execute("CREATE TABLE d_events (id INT PRIMARY KEY, kind TEXT)")
            .unwrap();
        db.execute("CREATE INDEX idx_d_events_kind ON d_events(kind)").unwrap();
        for i in 0..50 {
            db.execute(&format!("INSERT INTO d_events VALUES ({i}, 'pre')"))
                .unwrap();
        }

        // Explicit mid-run checkpoint.
        let report = db.storage.persist_index_snapshots().unwrap();
        assert!(report.persisted && report.art_tables >= 1);

        // Mutations AFTER the checkpoint: the first one must durably
        // invalidate the markers.
        for i in 50..80 {
            db.execute(&format!("INSERT INTO d_events VALUES ({i}, 'post')"))
                .unwrap();
        }
        db.execute("DELETE FROM d_events WHERE id = 0").unwrap();

        // Simulate a crash: no checkpoint at close.
        db.storage.set_index_snapshots_on_close(false);
    }

    let db = EmbeddedDatabase::new(temp.path()).unwrap();
    let report = db.storage.last_index_open_report().unwrap();
    assert_eq!(
        report.tables_from_snapshot, 0,
        "stale snapshot must NOT be trusted after post-checkpoint mutations, report: {report:?}"
    );
    assert_eq!(report.tables_scanned, 1, "table must be rebuilt from rows");

    // Replay correctness: post-checkpoint mutations are all visible.
    assert_eq!(
        index_row_count(&db, "idx_d_events_kind", Value::String("post".into())),
        30
    );
    assert_eq!(
        index_row_count(&db, "idx_d_events_kind", Value::String("pre".into())),
        49
    );
    assert_eq!(count_rows(&db, "SELECT COUNT(*) FROM d_events"), 79);
}

#[test]
fn crash_without_mutations_after_open_is_safe_either_way() {
    let temp = TempDir::new().unwrap();

    {
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        db.execute("CREATE TABLE d_static (id INT PRIMARY KEY, name TEXT)")
            .unwrap();
        db.execute("CREATE INDEX idx_d_static_name ON d_static(name)").unwrap();
        db.execute("INSERT INTO d_static VALUES (1, 'a'), (2, 'b'), (3, 'a')")
            .unwrap();
    } // clean close: snapshot + markers

    {
        // Open (loads snapshot), mutate NOTHING, crash.
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        let report = db.storage.last_index_open_report().unwrap();
        assert_eq!(report.tables_from_snapshot, 1);
        db.storage.set_index_snapshots_on_close(false);
    }

    // Whether or not the markers survived the registration-time consumption,
    // the reopen must produce correct indexes.
    let db = EmbeddedDatabase::new(temp.path()).unwrap();
    assert_eq!(index_row_count(&db, "idx_d_static_name", Value::String("a".into())), 2);
    assert_eq!(index_row_count(&db, "idx_d_static_name", Value::String("b".into())), 1);
    assert_eq!(count_rows(&db, "SELECT COUNT(*) FROM d_static"), 3);
}

#[test]
fn hnsw_graph_reloads_from_dump_and_stays_mutable() {
    let temp = TempDir::new().unwrap();

    {
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        db.execute("CREATE TABLE d_docs (id INT PRIMARY KEY, emb VECTOR(4))")
            .unwrap();
        // Distinct vectors: duplicate-heavy data (the original `i % 20` gave
        // 20 groups of identical points) degrades HNSW graph connectivity —
        // neighbor lists fill with zero-distance duplicates and a fresh
        // outlier insert can end up unreachable from the entry point. That is
        // an inherent ANN property (verified to flake identically on a fresh,
        // never-dumped graph), not a reload defect; this test is about reload
        // + mutability, so keep the dataset non-degenerate.
        for i in 0..400 {
            let v = i as f32 * 0.25;
            db.execute(&format!(
                "INSERT INTO d_docs VALUES ({i}, '[{}, {}, {}, {}]')",
                v,
                v + 1.0,
                v * 0.5,
                1.0
            ))
            .unwrap();
        }
        db.execute("CREATE INDEX idx_d_docs_emb ON d_docs USING hnsw (emb)")
            .unwrap();
        db.execute("DELETE FROM d_docs WHERE id = 399").unwrap(); // leave a tombstone
    }

    let db = EmbeddedDatabase::new(temp.path()).unwrap();
    let report = db.storage.last_index_open_report().unwrap();
    assert_eq!(
        report.vector_reloaded, 1,
        "HNSW index must reload from its graph dump, report: {report:?}"
    );
    assert_eq!(report.vector_rebuilt, 0, "no scan rebuild expected");
    assert_eq!(report.vector_degraded, 0);
    assert!(
        db.storage.vector_indexes().degraded_indexes().is_empty(),
        "no index may be degraded after a clean reload"
    );

    let stats = db.storage.vector_indexes().get_index_stats("idx_d_docs_emb").unwrap();
    assert_eq!(stats.num_vectors, 399, "tombstone state must survive the reload");

    // kNN through SQL works off the reloaded graph.
    let rows = db
        .query(
            "SELECT id FROM d_docs ORDER BY emb <-> '[5.0, 6.0, 2.5, 1.0]' LIMIT 3",
            &[],
        )
        .unwrap();
    assert_eq!(rows.len(), 3);

    // The reloaded graph must accept further DML (insert + search round-trip).
    // The new vector is IN-distribution (between existing points, near
    // v = 55.125): an extreme outlier is unreliable here by HNSW design —
    // simple neighbor pruning keeps the M closest, an outlier is never among
    // any node's M closest, so its reverse links can all be rejected and the
    // node becomes unreachable (verified to flake identically on a fresh,
    // never-dumped graph — not a reload defect). An in-distribution point IS
    // among its neighbors' closest, so its links survive pruning.
    db.execute("INSERT INTO d_docs VALUES (5000, '[55.13, 56.13, 27.56, 1.0]')")
        .unwrap();
    // Deterministic: the row itself is queryable.
    let rows = db.query("SELECT id FROM d_docs WHERE id = 5000", &[]).unwrap();
    assert_eq!(rows.len(), 1, "inserted row must be queryable after reload");
    // ANN round-trip: the new vector must be reachable through the reloaded
    // graph. HNSW is approximate, so assert membership in the top-5 instead
    // of demanding first place (the original LIMIT 1 form flaked under load:
    // graph build order is rayon-dependent).
    let rows = db
        .query(
            "SELECT id FROM d_docs ORDER BY emb <-> '[55.13, 56.13, 27.56, 1.0]' LIMIT 5",
            &[],
        )
        .unwrap();
    let ids: Vec<_> = rows.iter().map(|r| r.values.first().cloned()).collect();
    assert!(
        ids.contains(&Some(Value::Int4(5000))),
        "vector inserted after reload must be reachable via ANN search, got {ids:?}"
    );
}

#[test]
fn hnsw_crash_path_rebuilds_with_correct_results() {
    let temp = TempDir::new().unwrap();

    {
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        db.execute("CREATE TABLE d_vecs (id INT PRIMARY KEY, emb VECTOR(3))")
            .unwrap();
        db.execute("CREATE INDEX idx_d_vecs_emb ON d_vecs USING hnsw (emb)")
            .unwrap();
        db.execute("INSERT INTO d_vecs VALUES (1, '[1,0,0]'), (2, '[0,1,0]'), (3, '[0,0,1]')")
            .unwrap();
        db.storage.set_index_snapshots_on_close(false); // crash
    }

    let db = EmbeddedDatabase::new(temp.path()).unwrap();
    let report = db.storage.last_index_open_report().unwrap();
    assert_eq!(report.vector_reloaded, 0);
    assert_eq!(
        report.vector_rebuilt, 1,
        "crash path must rebuild from rows, report: {report:?}"
    );
    let rows = db
        .query("SELECT id FROM d_vecs ORDER BY emb <-> '[0.9,0.1,0]' LIMIT 1", &[])
        .unwrap();
    assert_eq!(rows[0].values.first(), Some(&Value::Int4(1)));
}

#[test]
fn open_transaction_at_drop_skips_checkpoint() {
    let temp = TempDir::new().unwrap();

    {
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        db.execute("CREATE TABLE d_txn (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("CREATE INDEX idx_d_txn_v ON d_txn(v)").unwrap();
        db.execute("INSERT INTO d_txn VALUES (1, 'committed')").unwrap();
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO d_txn VALUES (2, 'uncommitted')").unwrap();
        // Drop with the transaction still open: the checkpoint must be
        // skipped — the eager in-txn index entry for row 2 is not committed
        // state and must not be snapshotted.
    }

    let db = EmbeddedDatabase::new(temp.path()).unwrap();
    let report = db.storage.last_index_open_report().unwrap();
    assert_eq!(
        report.tables_from_snapshot, 0,
        "no snapshot may exist after a drop with an open transaction, report: {report:?}"
    );
    assert_eq!(
        index_row_count(&db, "idx_d_txn_v", Value::String("committed".into())),
        1
    );
    assert_eq!(
        index_row_count(&db, "idx_d_txn_v", Value::String("uncommitted".into())),
        0,
        "uncommitted row must not reappear via the index"
    );
}

#[test]
fn checkpoint_then_clean_close_refreshes_snapshot() {
    let temp = TempDir::new().unwrap();

    {
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        db.execute("CREATE TABLE d_fresh (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("CREATE INDEX idx_d_fresh_v ON d_fresh(v)").unwrap();
        db.execute("INSERT INTO d_fresh VALUES (1, 10)").unwrap();
        db.storage.persist_index_snapshots().unwrap();
        // Mutate after the checkpoint, then close CLEANLY: the close-time
        // checkpoint must refresh the snapshot to include the mutation.
        db.execute("INSERT INTO d_fresh VALUES (2, 10)").unwrap();
    }

    let db = EmbeddedDatabase::new(temp.path()).unwrap();
    let report = db.storage.last_index_open_report().unwrap();
    assert_eq!(report.tables_from_snapshot, 1, "clean close must rewrite the snapshot");
    assert_eq!(index_row_count(&db, "idx_d_fresh_v", Value::Int4(10)), 2);
}

// ---------------------------------------------------------------------------
// Close-time write ORDER: row counters BEFORE the index-snapshot checkpoint.
//
// `Catalog::rebuild_all_indexes` reconciles a stale row-id counter
// (`reseed_row_counter_from_max_row_id`) ONLY on its scan-fallback path, which a
// valid snapshot skips wholesale. So of the two states a crash between the two
// `Drop::drop` writes can leave:
//   * valid snapshot + stale counter  → unrecoverable: the next fast INSERT
//     reuses a live internal row_id and silently overwrites that row.
//   * correct counter + no snapshot   → harmless: the scan rebuild is slower
//     but always correct.
// Only the flush-first order can produce the second state, never the first.
// ---------------------------------------------------------------------------

/// Seed `rows` single-row literal INSERTs — the fast path that only re-persists
/// the durable `counter:{table}` every 64 rows, i.e. the one that leaves a stale
/// counter behind when the close-time flush does not run.
fn seed_counter_table(db: &EmbeddedDatabase, table: &str, rows: i32) {
    let ddl = format!("CREATE TABLE {table} (id INT PRIMARY KEY, v INT)");
    db.execute(&ddl).unwrap();
    let idx = format!("CREATE INDEX idx_{table}_v ON {table}(v)");
    db.execute(&idx).unwrap();
    for i in 0..rows {
        let dml = format!("INSERT INTO {table} VALUES ({i}, {i})");
        db.execute(&dml).unwrap();
    }
}

/// Every listed id must still resolve to exactly one row. A reused internal
/// row_id overwrites one row's tuple in place while leaving the total count
/// plausible, so a count assertion alone would not catch it.
fn assert_all_ids_present(db: &EmbeddedDatabase, table: &str, ids: impl IntoIterator<Item = i32>) {
    for id in ids {
        let sql = format!("SELECT COUNT(*) FROM {table} WHERE id = {id}");
        let hits = count_rows(db, &sql);
        assert_eq!(hits, 1, "row id={id} must be present, not overwritten");
    }
}

/// The mechanism that makes the order matter: with a valid snapshot on disk the
/// open takes the bulk-load fast path and scans NO rows, so it never observes
/// the true max row_id and can never reconcile a stale counter. That is why the
/// counter flush must be the FIRST of the two close-time writes — this state
/// (snapshot written, counter flush skipped) is exactly what a crash between
/// them produced under the old order, and nothing downstream repairs it.
#[test]
fn valid_snapshot_open_scans_no_rows_so_never_reseeds_the_counter() {
    let temp = TempDir::new().unwrap();

    {
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        seed_counter_table(&db, "d_ctr_trap", 10);

        // Crash simulation under the OLD order: the snapshot checkpoint
        // completed (left enabled, so `Drop` writes it) and the process died
        // before the counter flush ever ran.
        db.storage.set_row_counter_flush_on_close(false);
    }

    let db = EmbeddedDatabase::new(temp.path()).unwrap();
    let report = db.storage.last_index_open_report().unwrap();
    assert_eq!(
        report.tables_from_snapshot, 1,
        "the snapshot must be trusted here — that is the trap: {report:?}"
    );
    // No scan fallback ⇒ `reseed_row_counter_from_max_row_id` is unreachable,
    // and zero rows observed ⇒ the open cannot learn the true max row_id.
    assert_eq!(report.tables_scanned, 0, "no scan fallback: {report:?}");
    assert_eq!(report.rows_scanned, 0, "no rows observed: {report:?}");
}

/// The post-fix crash window: the counter flush completed, the snapshot
/// checkpoint did not. The reopen must fall back to the scan rebuild (always
/// correct) and the next INSERT must not collide with a live row.
#[test]
fn counter_flushed_without_snapshot_reopens_without_row_id_collision() {
    let temp = TempDir::new().unwrap();

    {
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        seed_counter_table(&db, "d_ctr_safe", 10);

        // Crash simulation under the FIXED order: the counter flush completed
        // (left enabled, so `Drop` runs it first) and the process died before
        // the snapshot checkpoint.
        db.storage.set_index_snapshots_on_close(false);
    }

    let db = EmbeddedDatabase::new(temp.path()).unwrap();
    let report = db.storage.last_index_open_report().unwrap();
    assert_eq!(report.tables_from_snapshot, 0, "no snapshot: {report:?}");
    assert_eq!(report.tables_scanned, 1, "scan rebuild: {report:?}");

    db.execute("INSERT INTO d_ctr_safe VALUES (999, 999)").unwrap();
    let total = count_rows(&db, "SELECT COUNT(*) FROM d_ctr_safe");
    assert_eq!(total, 11, "the new row must not overwrite an old one");
    assert_all_ids_present(&db, "d_ctr_safe", (0..10).chain(std::iter::once(999)));
}

/// Non-regression for the ordinary path: an uninterrupted `Drop` still leaves
/// BOTH a valid snapshot and a correct counter, so the reopen takes the fast
/// path AND the next INSERT lands on a fresh row_id. This is the test that
/// fails outright if the close-time counter flush is ever dropped, or moved
/// behind a guard that skips it.
#[test]
fn clean_drop_leaves_snapshot_and_counter_consistent() {
    let temp = TempDir::new().unwrap();

    {
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        seed_counter_table(&db, "d_ctr_clean", 10);
        // Full, uninterrupted Drop: counter flush, then snapshot checkpoint.
    }

    let db = EmbeddedDatabase::new(temp.path()).unwrap();
    let report = db.storage.last_index_open_report().unwrap();
    assert_eq!(report.tables_from_snapshot, 1, "snapshot: {report:?}");
    assert_eq!(report.tables_scanned, 0, "no scan needed: {report:?}");

    db.execute("INSERT INTO d_ctr_clean VALUES (999, 999)").unwrap();
    let total = count_rows(&db, "SELECT COUNT(*) FROM d_ctr_clean");
    assert_eq!(total, 11, "the new row must not overwrite an old one");
    assert_all_ids_present(&db, "d_ctr_clean", (0..10).chain(std::iter::once(999)));
}

/// The `txn_open` guard applies to the snapshot checkpoint ONLY. The counter
/// flush stays unconditional: it is monotonic-safe, so flushing a counter that a
/// still-open transaction bumped costs at most a row-id gap, never a collision —
/// while gating it would reopen the stale-counter window this ordering closes.
#[test]
fn open_transaction_at_drop_skips_snapshot_but_not_the_counter_flush() {
    let temp = TempDir::new().unwrap();

    {
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        seed_counter_table(&db, "d_ctr_txn", 10);
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO d_ctr_txn VALUES (500, 500)").unwrap();
        // Drop with the transaction still open: the snapshot is skipped by
        // design, the counter flush runs anyway. No toggles — the real
        // `Drop::drop` runs here, exactly as written.
    }

    let db = EmbeddedDatabase::new(temp.path()).unwrap();
    let report = db.storage.last_index_open_report().unwrap();
    assert_eq!(report.tables_from_snapshot, 0, "no snapshot: {report:?}");

    let stale = count_rows(&db, "SELECT COUNT(*) FROM d_ctr_txn WHERE id = 500");
    assert_eq!(stale, 0, "uncommitted row must not survive the drop");

    db.execute("INSERT INTO d_ctr_txn VALUES (999, 999)").unwrap();
    let total = count_rows(&db, "SELECT COUNT(*) FROM d_ctr_txn");
    assert_eq!(total, 11, "the new row must not overwrite an old one");
    assert_all_ids_present(&db, "d_ctr_txn", (0..10).chain(std::iter::once(999)));
}

// ---------------------------------------------------------------------------
// R4 (Sprinter 746342a0bfbb): the HNSW graph dump lifecycle.
//
// Each checkpoint dumps a changed graph under a fresh basename, switches the
// sidecar + marker to it in one batch and removes the previous dump; an
// unchanged index keeps its dump; the open sweeps whatever no valid sidecar
// references; an encrypted store writes no dump at all.
// ---------------------------------------------------------------------------

/// Sorted names in `{data}/hnsw_snapshots/` (empty when it does not exist).
fn hnsw_snapshot_entries(data: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(data.join("hnsw_snapshots")) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The fireproof tests' rows: `[v, v + 1, v / 2, 1]` with `v = id / 4`.
fn seed_line_rows(db: &EmbeddedDatabase, table: &str, ids: std::ops::Range<i32>) {
    for i in ids {
        let v = i as f32 * 0.25;
        db.execute(&format!(
            "INSERT INTO {table} VALUES ({i}, '[{}, {}, {}, {}]')",
            v,
            v + 1.0,
            v * 0.5,
            1.0
        ))
        .unwrap();
    }
}

fn line_vector_literal(id: i32) -> String {
    let v = id as f32 * 0.25;
    format!("[{}, {}, {}, {}]", v, v + 1.0, v * 0.5, 1.0)
}

/// Item 1: reload → change → checkpoint → close → reopen, three times over.
/// Every checkpoint of the RELOADED graph succeeds, exactly one dump pair is
/// ever on disk, and every reopen reloads. Red before: hnsw_rs refused to
/// overwrite a reloaded graph's dump and wrote a `-{rand}` copy, so each
/// checkpoint counted a dump failure and left a full-size orphan pair.
#[test]
fn closing_a_reloaded_hnsw_index_publishes_one_dump_and_counts_no_failure() {
    let temp = TempDir::new().unwrap();
    {
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        db.execute("CREATE TABLE d_cycle (id INT PRIMARY KEY, emb VECTOR(4))")
            .unwrap();
        seed_line_rows(&db, "d_cycle", 0..200);
        db.execute("CREATE INDEX idx_d_cycle_emb ON d_cycle USING hnsw (emb)")
            .unwrap();
    }
    assert_eq!(hnsw_snapshot_entries(temp.path()).len(), 2);

    for round in 0..3 {
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        let opened = db.storage.last_index_open_report().unwrap();
        assert_eq!(opened.vector_reloaded, 1, "round {round}: {opened:?}");
        assert_eq!(opened.vector_rebuilt, 0, "round {round}: {opened:?}");
        assert_eq!(
            opened.vector_dump_files_swept, 0,
            "round {round}: nothing stray: {opened:?}"
        );

        // A change, so the reloaded graph itself must be dumped again.
        seed_line_rows(&db, "d_cycle", 1000 + round..1001 + round);
        let persisted = db.storage.persist_index_snapshots().unwrap();
        assert_eq!(persisted.vector_dump_failures, 0, "round {round}: {persisted:?}");
        assert_eq!(persisted.vector_graphs, 1, "round {round}: {persisted:?}");
        let entries = hnsw_snapshot_entries(temp.path());
        assert_eq!(entries.len(), 2, "round {round}: one dump pair, no orphan: {entries:?}");
    } // clean close: unchanged since the checkpoint, nothing dumped again
    assert_eq!(hnsw_snapshot_entries(temp.path()).len(), 2);

    let db = EmbeddedDatabase::new(temp.path()).unwrap();
    let report = db.storage.last_index_open_report().unwrap();
    assert_eq!(report.vector_reloaded, 1, "{report:?}");
    let stats = db.storage.vector_indexes().get_index_stats("idx_d_cycle_emb").unwrap();
    assert_eq!(stats.num_vectors, 203);
    for id in 1000..1003 {
        let rows = db
            .query(
                &format!(
                    "SELECT id FROM d_cycle ORDER BY emb <-> '{}' LIMIT 1",
                    line_vector_literal(id)
                ),
                &[],
            )
            .unwrap();
        assert_eq!(rows[0].values.first(), Some(&Value::Int4(id)));
    }
}

/// Per-index dirty flag: an index unchanged since its dump keeps that dump.
/// Only `d_touch` changes; its first mutation deletes EVERY vector marker,
/// `d_keep`'s too, and the checkpoint re-arms `d_keep`'s without dumping it,
/// so both still reload.
#[test]
fn an_unchanged_hnsw_index_is_not_dumped_again_and_keeps_reloading() {
    let temp = TempDir::new().unwrap();
    {
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        for table in ["d_keep", "d_touch"] {
            db.execute(&format!("CREATE TABLE {table} (id INT PRIMARY KEY, emb VECTOR(4))"))
                .unwrap();
            seed_line_rows(&db, table, 0..60);
            db.execute(&format!("CREATE INDEX idx_{table}_emb ON {table} USING hnsw (emb)"))
                .unwrap();
        }
    }
    let first = hnsw_snapshot_entries(temp.path());
    assert_eq!(first.len(), 4, "{first:?}");
    {
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        assert_eq!(db.storage.last_index_open_report().unwrap().vector_reloaded, 2);
        seed_line_rows(&db, "d_touch", 500..501);
        let persisted = db.storage.persist_index_snapshots().unwrap();
        assert_eq!(
            persisted.vector_graphs, 1,
            "only the changed index is dumped: {persisted:?}"
        );
        assert_eq!(persisted.vector_unchanged, 1, "{persisted:?}");
        assert_eq!(persisted.vector_dump_failures, 0, "{persisted:?}");
    }
    let second = hnsw_snapshot_entries(temp.path());
    assert_eq!(second.len(), 4, "{second:?}");
    assert_eq!(
        first.iter().filter(|name| second.contains(name)).count(),
        2,
        "d_keep's dump is the same files, d_touch's was replaced: {first:?} -> {second:?}"
    );

    let db = EmbeddedDatabase::new(temp.path()).unwrap();
    let report = db.storage.last_index_open_report().unwrap();
    assert_eq!(report.vector_reloaded, 2, "{report:?}");
    assert_eq!(report.vector_rebuilt, 0, "{report:?}");
    assert_eq!(
        db.storage
            .vector_indexes()
            .get_index_stats("idx_d_touch_emb")
            .unwrap()
            .num_vectors,
        61
    );
}

/// Item 1: at open, every dump file no valid sidecar references (an old-style
/// orphan, another dump's leftovers) and every crashed dump's staging
/// directory is deleted; the referenced dump and unrelated files stay.
#[test]
fn stray_graph_dump_files_are_swept_at_open() {
    let temp = TempDir::new().unwrap();
    {
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        db.execute("CREATE TABLE d_stray (id INT PRIMARY KEY, emb VECTOR(4))")
            .unwrap();
        seed_line_rows(&db, "d_stray", 0..40);
        db.execute("CREATE INDEX idx_d_stray_emb ON d_stray USING hnsw (emb)")
            .unwrap();
    }
    let referenced = hnsw_snapshot_entries(temp.path());
    assert_eq!(referenced.len(), 2, "{referenced:?}");
    let dir = temp.path().join("hnsw_snapshots");
    for name in [
        "0123456789abcdef-0000000000000001.hnsw.graph",
        "0123456789abcdef-0000000000000001.hnsw.data",
        "idx_d_stray_emb-4242.hnsw.data",
    ] {
        std::fs::write(dir.join(name), b"stale").unwrap();
    }
    let staging = dir.join(".hnsw-staging-crashed-1-2-0");
    std::fs::create_dir(&staging).unwrap();
    std::fs::write(staging.join("crashed.hnsw.graph"), b"partial").unwrap();
    std::fs::write(dir.join("notes.txt"), b"not a dump").unwrap();

    let db = EmbeddedDatabase::new(temp.path()).unwrap();
    let report = db.storage.last_index_open_report().unwrap();
    assert_eq!(report.vector_reloaded, 1, "{report:?}");
    assert_eq!(report.vector_dump_files_swept, 4, "{report:?}");
    let mut expected = referenced;
    expected.push("notes.txt".to_string());
    expected.sort();
    assert_eq!(hnsw_snapshot_entries(temp.path()), expected);
}

/// Item 4: with at-rest encryption on, no graph dump (plaintext vectors) is
/// written; the index rebuilds from its sealed rows at open, and a plaintext
/// dump an unencrypted run left behind is deleted.
#[cfg(feature = "encryption")]
#[test]
fn an_encrypted_store_writes_no_graph_dump_and_rebuilds_from_rows() {
    const KEY_VAR: &str = "HELIOSDB_TEST_R4_HNSW_DUMP_TDE_KEY";
    std::env::set_var(
        KEY_VAR,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    );
    let temp = TempDir::new().unwrap();
    let encrypted = || {
        let mut config = heliosdb_nano::Config::default();
        config.storage.memory_only = false;
        config.storage.path = Some(temp.path().to_path_buf());
        config.encryption.enabled = true;
        config.encryption.key_source = heliosdb_nano::KeySource::Environment(KEY_VAR.to_string());
        config
    };
    let is_dump = |name: &String| name.ends_with(".hnsw.graph") || name.ends_with(".hnsw.data");
    {
        let db = EmbeddedDatabase::with_config(encrypted()).unwrap();
        db.execute("CREATE TABLE d_sealed (id INT PRIMARY KEY, emb VECTOR(4))")
            .unwrap();
        seed_line_rows(&db, "d_sealed", 0..60);
        db.execute("CREATE INDEX idx_d_sealed_emb ON d_sealed USING hnsw (emb)")
            .unwrap();
        let persisted = db.storage.persist_index_snapshots().unwrap();
        assert_eq!(persisted.vector_graphs, 0, "{persisted:?}");
        assert_eq!(persisted.vector_skipped, 1, "{persisted:?}");
        assert_eq!(persisted.vector_dump_failures, 0, "{persisted:?}");
    }
    let entries = hnsw_snapshot_entries(temp.path());
    assert!(!entries.iter().any(is_dump), "plaintext vectors written: {entries:?}");

    // A plaintext dump from before encryption was enabled.
    let dir = temp.path().join("hnsw_snapshots");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("idx_d_sealed_emb.hnsw.graph"), b"plaintext graph").unwrap();
    std::fs::write(dir.join("idx_d_sealed_emb.hnsw.data"), b"plaintext vectors").unwrap();
    {
        let db = EmbeddedDatabase::with_config(encrypted()).unwrap();
        let report = db.storage.last_index_open_report().unwrap();
        assert_eq!(report.vector_reloaded, 0, "{report:?}");
        assert_eq!(report.vector_rebuilt, 1, "{report:?}");
        assert_eq!(report.vector_dump_files_swept, 2, "{report:?}");
        let v = 30.0_f32 * 0.25;
        let hits = db
            .storage
            .vector_indexes()
            .search("idx_d_sealed_emb", &vec![v, v + 1.0, v * 0.5, 1.0], 1)
            .unwrap();
        assert_eq!(hits.first().map(|hit| hit.1), Some(0.0), "{hits:?}");
    }
    let entries = hnsw_snapshot_entries(temp.path());
    assert!(!entries.iter().any(is_dump), "{entries:?}");
}

/// Review of R4, item 3: binaries before 4.42 never range-checked `m` /
/// `ef_construction`, so a stored definition can carry values the index cannot
/// be built with. At open they are clamped (with a WARN) rather than refused:
/// the index is rebuilt and live, not degraded — and a physical standby's
/// strict hydration, which shares the same parameter resolution, does not fail.
#[test]
fn a_legacy_out_of_range_m_is_clamped_at_open_not_refused() {
    use heliosdb_nano::sql::logical_plan::IndexOption;
    use heliosdb_nano::storage::{Catalog, VectorIndexType};

    let temp = TempDir::new().unwrap();
    {
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        db.execute("CREATE TABLE d_legacy (id INT PRIMARY KEY, emb VECTOR(4))")
            .unwrap();
        seed_line_rows(&db, "d_legacy", 0..60);
        db.execute("CREATE INDEX idx_d_legacy_emb ON d_legacy USING hnsw (emb)")
            .unwrap();
        // What an older binary accepted: WITH (m = 0, ef_construction = 0).
        let catalog = Catalog::new(&db.storage);
        let (_, mut definition) = catalog
            .list_index_definitions()
            .unwrap()
            .into_iter()
            .find(|(name, _)| name == "idx_d_legacy_emb")
            .unwrap();
        definition.options.push(IndexOption::HnswM(0));
        definition.options.push(IndexOption::EfConstruction(0));
        catalog.save_index_definition("idx_d_legacy_emb", &definition).unwrap();
        // No dump: the next open must build the index from that definition.
        db.storage.set_index_snapshots_on_close(false);
    }

    let db = EmbeddedDatabase::new(temp.path()).unwrap();
    let report = db.storage.last_index_open_report().unwrap();
    assert_eq!(report.vector_rebuilt, 1, "{report:?}");
    assert_eq!(report.vector_degraded, 0, "{report:?}");
    assert!(db.storage.vector_indexes().degraded_indexes().is_empty());
    match db
        .storage
        .vector_indexes()
        .get_metadata("idx_d_legacy_emb")
        .unwrap()
        .index_type
    {
        VectorIndexType::Standard(config) => {
            assert_eq!(config.max_connections, 2, "m clamped into range");
            assert_eq!(config.ef_construction, 1, "ef_construction clamped into range");
        }
        other => panic!("expected a standard HNSW index, got {other:?}"),
    }
    let rows = db
        .query(
            &format!(
                "SELECT id FROM d_legacy ORDER BY emb <-> '{}' LIMIT 1",
                line_vector_literal(42)
            ),
            &[],
        )
        .unwrap();
    assert_eq!(rows[0].values.first(), Some(&Value::Int4(42)));
}
