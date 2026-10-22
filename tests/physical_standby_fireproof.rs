//! Spec P2.2 §3 (plan §D5, "fireproof"): what a primary persists outside the
//! replicated keyspace, or writes around it, must not reach — or silently
//! diverge on — a physical standby.
//!
//! * Lock-free ingestion writes rows straight into the store from its own
//!   worker, bypassing the logical WAL, MVCC versions and indexes: refused on
//!   every HA role. Its WAL lives inside the data directory, never relative
//!   to the process's working directory (red before: `data/lockfree_wal`
//!   under the cwd, and no role check).
//! * An HNSW graph dump (`hnsw_snapshots/`, outside RocksDB) is loaded only
//!   when it is the dump its `vecsnap:` sidecar describes — the sidecar now
//!   records both files' SHA-256 — else the index is rebuilt from rows (red
//!   before: a stale but valid dump was loaded under a newer sidecar).
//! * `--dump-on-shutdown` is refused on a standby (a dump file in its data
//!   directory would stop it from reopening).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss
)]

use heliosdb_nano::storage::lockfree::LockFreeIngestionConfig;
use heliosdb_nano::{Config, EmbeddedDatabase, Value};
use std::path::Path;

fn persistent(dir: &Path, role: &str) -> EmbeddedDatabase {
    let mut config = Config::default();
    config.storage.memory_only = false;
    config.storage.path = Some(dir.to_path_buf());
    config.replication.role = role.to_string();
    EmbeddedDatabase::with_config(config).expect("open")
}

#[test]
fn lock_free_ingestion_is_refused_on_an_ha_role() {
    for role in ["primary", "Primary"] {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let db = persistent(&data, role);
        let error = db
            .storage
            .enable_lockfree_ingestion(LockFreeIngestionConfig::for_bulk_load())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("enable_lockfree_ingestion is refused") && error.contains(role),
            "{error}"
        );
        assert!(!db.storage.is_lockfree_enabled());
        assert!(!data.join("lockfree_wal").exists(), "nothing was created");
    }
}

#[test]
fn the_lock_free_wal_lives_in_the_data_directory() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let db = persistent(&data, "standalone");
    let cwd_relative = std::env::current_dir().unwrap().join("data").join("lockfree_wal");
    let existed = cwd_relative.exists();
    db.storage
        .enable_lockfree_ingestion(LockFreeIngestionConfig::for_bulk_load())
        .unwrap();
    assert!(
        data.join("lockfree_wal").is_dir(),
        "the lock-free WAL is <data_dir>/lockfree_wal"
    );
    if !existed {
        assert!(
            !cwd_relative.exists(),
            "nothing is created relative to the working directory"
        );
    }
    db.storage.disable_lockfree_ingestion().unwrap();
}

fn insert_vectors(db: &EmbeddedDatabase, ids: std::ops::Range<i32>) {
    for i in ids {
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
}

fn dump_files(data: &Path) -> Vec<std::path::PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(data.join("hnsw_snapshots"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    files.sort();
    files
}

/// A graph dump that is valid but not the one the sidecar describes (an
/// older checkpoint's) is refused; the index is rebuilt from rows and holds
/// every row.
#[test]
fn a_graph_dump_the_sidecar_does_not_describe_is_rebuilt_from_rows() {
    let data = tempfile::tempdir().unwrap();
    let saved = tempfile::tempdir().unwrap();
    {
        let db = EmbeddedDatabase::new(data.path()).unwrap();
        db.execute("CREATE TABLE d_docs (id INT PRIMARY KEY, emb VECTOR(4))")
            .unwrap();
        insert_vectors(&db, 0..300);
        db.execute("CREATE INDEX idx_d_docs_emb ON d_docs USING hnsw (emb)")
            .unwrap();
    } // clean close: dump A + sidecar A
    let older = dump_files(data.path());
    assert_eq!(older.len(), 2, "{older:?}");
    for file in &older {
        std::fs::copy(file, saved.path().join(file.file_name().unwrap())).unwrap();
    }
    {
        let db = EmbeddedDatabase::new(data.path()).unwrap();
        assert_eq!(db.storage.last_index_open_report().unwrap().vector_reloaded, 1);
        insert_vectors(&db, 300..400);
    } // clean close: dump B + sidecar B
      // Put dump A back under sidecar B.
    for file in &older {
        std::fs::copy(saved.path().join(file.file_name().unwrap()), file).unwrap();
    }
    let db = EmbeddedDatabase::new(data.path()).unwrap();
    let report = db.storage.last_index_open_report().unwrap();
    assert_eq!(
        report.vector_reloaded, 0,
        "a dump that is not the sidecar's is never loaded: {report:?}"
    );
    assert_eq!(report.vector_rebuilt, 1, "{report:?}");
    let stats = db.storage.vector_indexes().get_index_stats("idx_d_docs_emb").unwrap();
    assert_eq!(stats.num_vectors, 400, "the rebuilt index holds every row");
    let rows = db
        .query(
            "SELECT id FROM d_docs ORDER BY emb <-> '[95.0, 96.0, 47.5, 1.0]' LIMIT 50",
            &[],
        )
        .unwrap();
    assert!(
        rows.iter().any(|row| row.values.first() == Some(&Value::Int4(380))),
        "a row only the newer checkpoint had is found"
    );
}

/// A missing dump file is refused the same way.
#[test]
fn a_missing_graph_dump_file_is_rebuilt_from_rows() {
    let data = tempfile::tempdir().unwrap();
    {
        let db = EmbeddedDatabase::new(data.path()).unwrap();
        db.execute("CREATE TABLE d_docs (id INT PRIMARY KEY, emb VECTOR(4))")
            .unwrap();
        insert_vectors(&db, 0..100);
        db.execute("CREATE INDEX idx_d_docs_emb ON d_docs USING hnsw (emb)")
            .unwrap();
    }
    let files = dump_files(data.path());
    std::fs::remove_file(&files[1]).unwrap();
    let db = EmbeddedDatabase::new(data.path()).unwrap();
    let report = db.storage.last_index_open_report().unwrap();
    assert_eq!(report.vector_reloaded, 0, "{report:?}");
    assert_eq!(report.vector_rebuilt, 1, "{report:?}");
    assert_eq!(
        db.storage
            .vector_indexes()
            .get_index_stats("idx_d_docs_emb")
            .unwrap()
            .num_vectors,
        100
    );
}

/// `--dump-on-shutdown` on a standby is refused before anything starts.
#[cfg(all(feature = "ha-tier1", unix))]
#[test]
fn dump_on_shutdown_is_refused_on_a_standby() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("replication.token");
    std::fs::write(&token, [0x42_u8; 32]).unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"))
        .arg("start")
        .arg("--data-dir")
        .arg(dir.path().join("standby"))
        .args(["--listen", "127.0.0.1", "--auth", "trust", "--http-port", "0"])
        .args(["--port", &port.to_string()])
        .args(["--replication-role", "standby", "--primary-host", "127.0.0.1:1"])
        .arg("--replication-token-file")
        .arg(&token)
        .arg("--dump-on-shutdown")
        .env_remove("HELIOSDB_NANO_READY_FILE")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains("--dump-on-shutdown is not supported on a standby"),
        "{text}"
    );
    assert!(!dir.path().join("standby").exists(), "nothing was created");
}
