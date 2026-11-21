//! Per-column options that are not part of the stored schema: the statistics
//! target of `ALTER TABLE … ALTER COLUMN … SET STATISTICS`.
//!
//! One record per table, `meta:attoptions:{table}`: a bincode map of column name
//! to target. A column with the default target (-1) has no entry, and a table
//! with no entry has no record. The record is main's, read at the storage engine
//! each time (a catalog query, not a hot path); a physical standby receives it
//! with the write group that changed it and derives nothing from it.

use std::collections::BTreeMap;

use super::Catalog;
use crate::{Error, Result};

impl Catalog<'_> {
    pub(super) fn column_options_key(table_name: &str) -> Vec<u8> {
        format!("meta:attoptions:{}", table_name.to_lowercase()).into_bytes()
    }

    /// The statistics targets recorded for `table_name`'s columns, by column name.
    pub fn column_statistics_targets(&self, table_name: &str) -> Result<BTreeMap<String, i32>> {
        match self.storage.get(&Self::column_options_key(table_name))? {
            Some(bytes) => bincode::deserialize(&bytes)
                .map_err(|e| Error::storage(format!("Failed to read the column options of '{table_name}': {e}"))),
            None => Ok(BTreeMap::new()),
        }
    }

    /// `targets` as the record of `table_name`, staged into the statement's group.
    pub(crate) fn stage_column_statistics_targets(
        &self,
        table_name: &str,
        targets: &BTreeMap<String, i32>,
        ddl: &mut crate::storage::DdlBatch<'_>,
    ) -> Result<()> {
        let key = Self::column_options_key(table_name);
        if targets.is_empty() {
            ddl.delete(&key);
            return Ok(());
        }
        let value = bincode::serialize(targets)
            .map_err(|e| Error::storage(format!("Failed to store the column options of '{table_name}': {e}")))?;
        ddl.put(&key, &value)
    }
}
