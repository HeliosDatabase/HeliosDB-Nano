//! Indexed lookups of a TEXT key that is a prefix of other keys.
//!
//! The ART index stores the row id of a key that ends at an inner node in that
//! node's header. Growing or shrinking the node (4 / 16 / 48 / 256 children)
//! dropped those ids, so on 4.41.0 and earlier, once `'k-1'` was a prefix of five
//! or more other keys (`'k-10'` .. `'k-19'`):
//!   * `WHERE k = 'k-1'` returned no row although the row existed, and
//!   * a TEXT PRIMARY KEY or UNIQUE column accepted `'k-1'` a second time.
//! A full scan (`LIKE`) still saw the row, which is how the wrong answer showed.

use heliosdb_nano::storage::index_snapshot::{self as snap, SnapshotMarker};
use heliosdb_nano::{Config, EmbeddedDatabase, Value};
use std::path::Path;

fn ids(db: &EmbeddedDatabase, sql: &str) -> Vec<i64> {
    db.query(sql, &[])
        .expect(sql)
        .iter()
        .map(|row| match row.values.first() {
            Some(Value::Int4(n)) => i64::from(*n),
            Some(Value::Int8(n)) => *n,
            other => panic!("unexpected value {other:?} for {sql}"),
        })
        .collect()
}

fn persistent(dir: &Path) -> EmbeddedDatabase {
    let mut config = Config::default();
    config.storage.memory_only = false;
    config.storage.path = Some(dir.to_path_buf());
    EmbeddedDatabase::with_config(config).expect("open")
}

fn keys() -> Vec<String> {
    let mut keys = vec!["k-1".to_string()];
    keys.extend((0..10).map(|d| format!("k-1{d}")));
    keys
}

#[test]
fn unique_text_keys_refuse_a_prefix_key_twice() {
    for ddl in [
        "CREATE TABLE x (k TEXT PRIMARY KEY, n INT)",
        "CREATE TABLE x (k TEXT UNIQUE, n INT)",
    ] {
        let db = EmbeddedDatabase::new_in_memory().expect("db");
        db.execute(ddl).expect("create");
        let values: Vec<String> = keys()
            .iter()
            .enumerate()
            .map(|(i, k)| format!("('{k}', {i})"))
            .collect();
        db.execute(&format!("INSERT INTO x VALUES {}", values.join(", ")))
            .expect("insert");

        assert_eq!(ids(&db, "SELECT n FROM x WHERE k = 'k-1'"), vec![0], "{ddl}: lookup");
        assert!(
            db.execute("INSERT INTO x VALUES ('k-1', 999)").is_err(),
            "{ddl}: a second 'k-1' was accepted"
        );
        assert_eq!(ids(&db, "SELECT n FROM x WHERE k LIKE 'k-1'"), vec![0], "{ddl}: scan");
    }
}

#[test]
fn secondary_index_finds_every_prefix_key_across_reopen_and_deletes() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let db = persistent(dir.path());
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
            .expect("create");
        db.execute("CREATE INDEX t_v ON t (v)").expect("index");
        let values: Vec<String> = (0..300).map(|i| format!("({i}, 'v-{i}')")).collect();
        db.execute(&format!("INSERT INTO t VALUES {}", values.join(", ")))
            .expect("insert");
        for i in 0..300 {
            assert_eq!(
                ids(&db, &format!("SELECT id FROM t WHERE v = 'v-{i}'")),
                vec![i],
                "v-{i}"
            );
        }
    }

    // Reopen (the index comes back from its snapshot), then delete most of the
    // keys that extend 'v-1' so its node shrinks back down.
    let db = persistent(dir.path());
    for i in 0..300 {
        assert_eq!(
            ids(&db, &format!("SELECT id FROM t WHERE v = 'v-{i}'")),
            vec![i],
            "v-{i} after reopen"
        );
    }
    db.execute("DELETE FROM t WHERE id >= 100 AND id < 200")
        .expect("delete");
    db.execute("DELETE FROM t WHERE id >= 10 AND id < 19").expect("delete");
    assert_eq!(ids(&db, "SELECT id FROM t WHERE v = 'v-1'"), vec![1]);
    assert_eq!(ids(&db, "SELECT id FROM t WHERE v = 'v-19'"), vec![19]);
    assert!(ids(&db, "SELECT id FROM t WHERE v = 'v-15'").is_empty());
}

/// Rewrite a snapshot marker on a CLOSED store as 4.41.0 wrote it (ART key
/// encoding v2). Retried: the previous handle releases the lock asynchronously.
fn mark_as_written_by_4_41_0(dir: &Path, marker_key: &[u8]) {
    let mut opts = rocksdb::Options::default();
    opts.create_if_missing(false);
    opts.set_prefix_extractor(rocksdb::SliceTransform::create_fixed_prefix(5));
    let mut raw = None;
    for _ in 0..100 {
        match rocksdb::DB::open(&opts, dir) {
            Ok(db) => {
                raw = Some(db);
                break;
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
    let raw = raw.expect("raw RocksDB open");
    let bytes = raw
        .get(marker_key)
        .expect("read marker")
        .expect("a clean close writes the marker");
    let mut marker: SnapshotMarker = bincode::deserialize(&bytes).expect("marker decodes");
    assert_eq!(marker.key_encoding_version, snap::ART_KEY_ENCODING_VERSION);
    marker.key_encoding_version = 2;
    raw.put(marker_key, bincode::serialize(&marker).expect("encode"))
        .expect("write marker");
}

/// Index snapshots written by an affected version can lack the dropped keys, so
/// they are rebuilt from rows once; HNSW graph snapshots are unaffected and must
/// still be reused rather than rebuilt.
#[test]
fn art_snapshots_from_affected_versions_are_rebuilt_but_hnsw_graphs_are_kept() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let db = persistent(dir.path());
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT, emb VECTOR(4))")
            .expect("create");
        db.execute("CREATE INDEX t_v ON t (v)").expect("index");
        let values: Vec<String> = (0..60)
            .map(|i| format!("({i}, 'v-{i}', '[{}, 1.0, 0.5, {}]')", i as f32 * 0.25, i as f32))
            .collect();
        db.execute(&format!("INSERT INTO t VALUES {}", values.join(", ")))
            .expect("insert");
        db.execute("CREATE INDEX t_emb ON t USING hnsw (emb)").expect("hnsw");
    }
    {
        // Control: an unmodified clean close reloads both from their snapshots.
        let db = persistent(dir.path());
        let report = db.storage.last_index_open_report().expect("report");
        assert_eq!(report.tables_from_snapshot, 1, "control: {report:?}");
        assert_eq!(report.vector_reloaded, 1, "control: {report:?}");
    }

    mark_as_written_by_4_41_0(dir.path(), &snap::art_snapshot_marker_key("t"));
    mark_as_written_by_4_41_0(dir.path(), &snap::vec_snapshot_marker_key("t_emb"));

    let db = persistent(dir.path());
    let report = db.storage.last_index_open_report().expect("report");
    assert_eq!(
        report.tables_from_snapshot, 0,
        "v2 ART snapshot must not be trusted: {report:?}"
    );
    assert_eq!(report.tables_scanned, 1, "{report:?}");
    assert_eq!(
        report.vector_reloaded, 1,
        "the HNSW graph must not be rebuilt: {report:?}"
    );
    assert_eq!(report.vector_rebuilt, 0, "{report:?}");
    for i in 0..60 {
        assert_eq!(
            ids(&db, &format!("SELECT id FROM t WHERE v = 'v-{i}'")),
            vec![i],
            "v-{i}"
        );
    }
}
