//! Regression guard for the v3.58 FastIngest profile (item 4).
//!
//! FastIngest sets `compression = Lz4`. This previously failed at runtime
//! ("Compression type LZ4 is not linked with the binary") because the rocksdb
//! dependency did not enable the `lz4` feature. These tests ensure (a) a
//! disk-backed DB actually OPENS under the profile, and (b) the code-index
//! override bridge returns the agreed values — so neither can silently regress.

use heliosdb_nano::config::CodeIndexProfileDefaults;
use heliosdb_nano::{Config, EmbeddedDatabase, ProfileConfig};

#[test]
fn fast_ingest_profile_opens_disk_db() {
    // Disk-backed on purpose: Lz4 compression applies to on-disk SSTs, so this
    // is the exact path that failed when lz4 was not linked.
    let dir = std::env::temp_dir().join(format!("helios_fast_ingest_open_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let mut config = Config::with_profile(ProfileConfig::FastIngest);
    config.storage.path = Some(dir.clone());
    config.storage.memory_only = false;

    let db = EmbeddedDatabase::with_config(config)
        .expect("FastIngest DB must open — rocksdb must link lz4 for compression=Lz4");

    // A trivial write/read must work under the profile.
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'a')").unwrap();

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fast_ingest_code_index_overrides() {
    let d: CodeIndexProfileDefaults = ProfileConfig::FastIngest
        .code_index_overrides()
        .expect("FastIngest must provide code-index overrides");
    assert!(d.skip_symbol_refs);
    assert!(d.skip_cross_file_resolve);
    assert_eq!(d.chunk_size, Some(2000));

    // Non-ingest profiles change no indexing behavior.
    assert!(ProfileConfig::Fast.code_index_overrides().is_none());
    assert!(ProfileConfig::Safe.code_index_overrides().is_none());
}
