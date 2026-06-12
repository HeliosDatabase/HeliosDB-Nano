//! R4.2 durable index tests.
//!
//! Covers: restart survival via snapshots (no row scan at open), crash
//! between checkpoint and later mutations (markers invalidated → scan
//! rebuild, never a stale snapshot), marker consumption at open, HNSW graph
//! reload, and the open-transaction guard on the close-time checkpoint.

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
        db.execute("CREATE INDEX idx_d_users_bucket ON d_users(bucket)").unwrap();
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
    assert_eq!(report.tables_scanned, 0, "no table may be scan-rebuilt, report: {report:?}");
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
    assert!(db.execute("INSERT INTO d_users VALUES (5, 'dup@example.com', 0)").is_err());
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
        db.execute("CREATE TABLE d_events (id INT PRIMARY KEY, kind TEXT)").unwrap();
        db.execute("CREATE INDEX idx_d_events_kind ON d_events(kind)").unwrap();
        for i in 0..50 {
            db.execute(&format!("INSERT INTO d_events VALUES ({i}, 'pre')")).unwrap();
        }

        // Explicit mid-run checkpoint.
        let report = db.storage.persist_index_snapshots().unwrap();
        assert!(report.persisted && report.art_tables >= 1);

        // Mutations AFTER the checkpoint: the first one must durably
        // invalidate the markers.
        for i in 50..80 {
            db.execute(&format!("INSERT INTO d_events VALUES ({i}, 'post')")).unwrap();
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
    assert_eq!(index_row_count(&db, "idx_d_events_kind", Value::String("post".into())), 30);
    assert_eq!(index_row_count(&db, "idx_d_events_kind", Value::String("pre".into())), 49);
    assert_eq!(count_rows(&db, "SELECT COUNT(*) FROM d_events"), 79);
}

#[test]
fn crash_without_mutations_after_open_is_safe_either_way() {
    let temp = TempDir::new().unwrap();

    {
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        db.execute("CREATE TABLE d_static (id INT PRIMARY KEY, name TEXT)").unwrap();
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
        db.execute("CREATE TABLE d_docs (id INT PRIMARY KEY, emb VECTOR(4))").unwrap();
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
        db.execute("CREATE INDEX idx_d_docs_emb ON d_docs USING hnsw (emb)").unwrap();
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
        db.execute("CREATE TABLE d_vecs (id INT PRIMARY KEY, emb VECTOR(3))").unwrap();
        db.execute("CREATE INDEX idx_d_vecs_emb ON d_vecs USING hnsw (emb)").unwrap();
        db.execute("INSERT INTO d_vecs VALUES (1, '[1,0,0]'), (2, '[0,1,0]'), (3, '[0,0,1]')")
            .unwrap();
        db.storage.set_index_snapshots_on_close(false); // crash
    }

    let db = EmbeddedDatabase::new(temp.path()).unwrap();
    let report = db.storage.last_index_open_report().unwrap();
    assert_eq!(report.vector_reloaded, 0);
    assert_eq!(report.vector_rebuilt, 1, "crash path must rebuild from rows, report: {report:?}");
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
    assert_eq!(index_row_count(&db, "idx_d_txn_v", Value::String("committed".into())), 1);
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

