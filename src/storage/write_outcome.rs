//! A native write's outcome, independent of its later durability/ACK result.
//! A borrowed observer adds no durable key, transaction field or row-image copy.
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    NotWritten,
    Applied,
    Unknown,
}

#[derive(Default)]
pub struct WriteAttempt(AtomicU8);

impl WriteAttempt {
    pub(crate) fn outcome(&self) -> WriteOutcome {
        match self.0.load(Ordering::Acquire) {
            0 => WriteOutcome::NotWritten,
            1 => WriteOutcome::Applied,
            _ => WriteOutcome::Unknown,
        }
    }

    pub(crate) fn native_started(&self) {
        self.0.store(2, Ordering::Release);
    }

    pub(crate) fn native_succeeded(&self) {
        self.0.store(1, Ordering::Release);
    }
}

/// A native error does not prove rejection. Fence affected row tables before
/// the caller releases the transaction/census; reopen rebuilds actual state.
/// This path performs no durable repair writes.
pub fn quarantine_unknown_transaction(engine: &super::StorageEngine, txn: &super::Transaction) {
    let tables = txn.affected_row_tables();
    if tables.is_empty() {
        return;
    }
    engine.bump_schema_generation();
    for table in &tables {
        engine.layout_epochs().quarantine(table);
        engine.write_gate().quarantine_table(
            table,
            "transaction native write outcome is unknown; reopen to reconcile",
        );
        engine.invalidate_schema_cache(table);
        engine.invalidate_table_constraints_cache(table);
        engine.forget_table_dictionaries(table);
        engine.snapshot_manager().invalidate_cached_versions(table);
        engine.data_write_attempt_publication(table.clone())();
    }
    // Fence every member of a multi-table group before any retirement can wait.
    for table in &tables {
        if let Err(error) = engine.art_indexes().drop_table_indexes(table) {
            tracing::error!("Cannot retire indexes after an uncertain commit of '{table}': {error}");
        }
        engine
            .vector_indexes()
            .retire_table_indexes(table, "transaction native write outcome is unknown");
    }
}
