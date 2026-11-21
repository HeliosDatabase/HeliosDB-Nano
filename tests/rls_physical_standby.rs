//! A physical standby has the primary's policies, table switches and tenants:
//! they are catalog records, replicated like the others, and the standby's
//! policy store reads them again when they arrive.
//!
//! The harness is the one of `tests/physical_standby_derived_state.rs`: a
//! primary, a standby staged from its checkpoint, and the primary's batches
//! fed to the standby through `PhysicalApplier::apply`.
#![cfg(all(feature = "ha-tier1", target_os = "linux"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use heliosdb_nano::replication::checkpoint_transfer::{
    OnlineCheckpointExport, PhysicalStage, ReceiverStaging, TransferLimits,
};
use heliosdb_nano::replication::history::load_or_create_primary_history;
use heliosdb_nano::replication::physical::{poll_source, FlushMemo, MAX_BATCH_BYTES};
use heliosdb_nano::replication::physical_applier::{NoopDerivedStateListener, PhysicalApplier};
use heliosdb_nano::rls::{PolicyStore, TableSecurity};
use heliosdb_nano::storage::{StorageEngine, WriteGate};
use heliosdb_nano::tenant::{IsolationMode, TenantManager};
use heliosdb_nano::{Config, EmbeddedDatabase};
use rocksdb::DB;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

fn primary_config(path: &Path) -> Config {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    config.replication.physical.enabled = true;
    config
}

fn stage(source: &DB, history: Uuid, root: &Path, config: &Config) -> PhysicalStage {
    let limits = TransferLimits::default();
    let export = OnlineCheckpointExport::create(source, history, root, limits).unwrap();
    let mut receiver = ReceiverStaging::begin(
        &root.join("standby"),
        &export.manifest_bytes().unwrap(),
        history,
        limits,
    )
    .unwrap();
    for (index, file) in export.manifest().files.iter().enumerate() {
        let mut offset = 0;
        while offset < file.size {
            let bytes = export.read_chunk(index as u32, offset, 64 * 1024).unwrap();
            receiver.append_chunk(index as u32, offset, &bytes).unwrap();
            offset += bytes.len() as u64;
        }
    }
    receiver.finish_download(config).unwrap().open(config).unwrap()
}

/// Fields drop in declaration order: the applier and the standby engine
/// before the stage that owns their writer, everything before the directory.
struct Pair {
    applier: PhysicalApplier,
    standby: Arc<StorageEngine>,
    _stage: PhysicalStage,
    raw: Arc<DB>,
    primary: EmbeddedDatabase,
    history: Uuid,
    _root: tempfile::TempDir,
}

impl Pair {
    fn new(setup: &[&str]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let config = primary_config(&root.path().join("primary"));
        let primary = EmbeddedDatabase::with_config(config.clone()).unwrap();
        for sql in setup {
            primary.execute(sql).unwrap_or_else(|e| panic!("primary setup `{sql}`: {e}"));
        }
        let raw = primary.storage.db();
        let history = load_or_create_primary_history(raw.path()).unwrap();
        let stage = stage(&raw, history, root.path(), &config);
        let path = stage.staging_path().to_path_buf();
        let standby = Arc::new(
            StorageEngine::open_physical_standby(
                &path,
                &primary_config(&path),
                Arc::clone(stage.writer()),
                WriteGate::closed("physical standby"),
            )
            .unwrap(),
        );
        let applier = PhysicalApplier::new(
            Arc::clone(&standby),
            Arc::clone(stage.writer()),
            Arc::new(NoopDerivedStateListener),
        )
        .unwrap();
        applier.hydrate().unwrap();
        Self {
            applier,
            standby,
            _stage: stage,
            raw,
            primary,
            history,
            _root: root,
        }
    }

    /// Apply everything the primary has written.
    fn sync(&self) {
        loop {
            let cursor = self.applier.writer().cursor().source_sequence();
            let poll = poll_source(&self.raw, self.history, cursor, MAX_BATCH_BYTES, &FlushMemo::new()).unwrap();
            if poll.batches.is_empty() {
                return;
            }
            for batch in &poll.batches {
                self.applier.apply(batch).unwrap();
            }
        }
    }
}

fn policy_names(store: &PolicyStore, table: &str) -> Vec<String> {
    store
        .table(table)
        .unwrap()
        .map(|entry| entry.policies.iter().map(|p| p.name.clone()).collect())
        .unwrap_or_default()
}

#[test]
fn a_standby_has_the_policies_the_switches_and_the_tenants_of_its_primary() {
    let pair = Pair::new(&[
        "CREATE ROLE alice LOGIN",
        "CREATE TABLE docs (id INT PRIMARY KEY, owner TEXT)",
        "INSERT INTO docs VALUES (1, 'alice'), (2, 'bob')",
        "ALTER TABLE docs ENABLE ROW LEVEL SECURITY",
        "CREATE POLICY own ON docs USING (owner = current_user)",
    ]);
    let before = pair
        .primary
        .tenant_manager
        .register_tenant("staged".to_string(), IsolationMode::SharedSchema);

    // What the stage carried.
    pair.sync();
    let store = PolicyStore::new();
    store.attach(Arc::clone(&pair.standby)).unwrap();
    assert!(store.active());
    assert_eq!(policy_names(&store, "docs"), ["own"]);
    assert_eq!(
        store.table("docs").unwrap().unwrap().security,
        TableSecurity {
            enabled: true,
            forced: false
        }
    );
    let manager = TenantManager::new();
    manager.attach_storage(Arc::clone(&pair.standby));
    assert_eq!(manager.list_tenants().len(), 1);
    assert_eq!(manager.get_tenant(before.id).unwrap().name, "staged");

    // What arrives afterwards reaches the same store and the same manager,
    // without a reopen.
    for sql in [
        "ALTER TABLE docs FORCE ROW LEVEL SECURITY",
        "CREATE POLICY second ON docs AS RESTRICTIVE FOR SELECT TO alice USING (id < 10)",
        "ALTER POLICY own ON docs RENAME TO mine",
        "CREATE TABLE other (id INT PRIMARY KEY)",
        "ALTER TABLE other ENABLE ROW LEVEL SECURITY",
    ] {
        pair.primary.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let later = pair
        .primary
        .tenant_manager
        .register_tenant("later".to_string(), IsolationMode::SharedSchema);
    pair.sync();
    assert_eq!(policy_names(&store, "docs"), ["mine", "second"]);
    assert!(store.table("docs").unwrap().unwrap().security.forced);
    assert!(store.table("other").unwrap().unwrap().security.enabled);
    let second = store.table("docs").unwrap().unwrap().policy("second").cloned().unwrap();
    assert!(!second.permissive);
    assert_eq!(second.roles, ["alice"]);
    assert_eq!(manager.list_tenants().len(), 2);
    assert!(manager.get_tenant(later.id).unwrap().rls_enabled);

    // And what is removed is gone.
    pair.primary.execute("DROP POLICY second ON docs").unwrap();
    pair.primary.execute("DROP TABLE other").unwrap();
    pair.primary.tenant_manager.delete_tenant(before.id).unwrap();
    pair.sync();
    assert_eq!(policy_names(&store, "docs"), ["mine"]);
    assert!(store.table("other").unwrap().is_none());
    assert_eq!(manager.list_tenants().len(), 1);
    assert!(manager.get_tenant(before.id).is_none());

    // The catalog view on the standby's engine shows the same.
    let rows = heliosdb_nano::rls::catalog::pg_policies_rows(&pair.standby).unwrap();
    assert_eq!(rows.len(), 1);
}
