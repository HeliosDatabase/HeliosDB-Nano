//! The comments of `COMMENT ON`.
//!
//! One record per commented object, `meta:comment:{kind}:{name}`, holding the
//! comment's text. `kind` is the object kind in lower case with underscores
//! (`table`, `column`, `index`, `view`, `materialized_view`, `function`,
//! `schema`, `constraint`, …); `name` is the object's catalog name, and for a
//! column or a constraint `{table}:{name}` (a table name holds no `:`).
//! `COMMENT … IS NULL` deletes the record. Read at the storage engine each time
//! (a catalog query, not a hot path); a physical standby receives a record with
//! the write that made it and derives nothing from it.
//!
//! A table's records, with its columns' and its constraints', go with `DROP
//! TABLE` and move with `RENAME TO`, in the statement's own write group; a
//! column's follows `RENAME COLUMN` and goes with `DROP COLUMN`; an index's and
//! a view's go with the object.

use super::Catalog;
use crate::{Error, Result};

/// The prefix of the family.
pub const COMMENT_PREFIX: &str = "meta:comment:";

/// One stored comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredComment {
    /// The object kind (`table`, `column`, …).
    pub kind: String,
    /// The object's name; `{table}:{name}` for a column or a constraint.
    pub name: String,
    /// The text.
    pub text: String,
}

impl Catalog<'_> {
    pub(crate) fn comment_key(kind: &str, name: &str) -> Vec<u8> {
        format!("meta:comment:{kind}:{name}").into_bytes()
    }

    /// Store (`Some`) or remove (`None`) the comment of one object.
    pub fn set_comment(&self, kind: &str, name: &str, text: Option<&str>) -> Result<()> {
        let key = Self::comment_key(kind, name);
        match text {
            Some(text) => self.storage.put(&key, text.as_bytes()),
            None => {
                if self.storage.get(&key)?.is_some() {
                    self.storage.delete(&key)?;
                }
                Ok(())
            }
        }
    }

    /// The comment of one object.
    pub fn get_comment(&self, kind: &str, name: &str) -> Result<Option<String>> {
        match self.storage.get(&Self::comment_key(kind, name))? {
            Some(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| Error::storage(format!("the comment of {kind} \"{name}\" is not text"))),
            None => Ok(None),
        }
    }

    /// Every stored comment, in key order.
    pub fn list_comments(&self) -> Result<Vec<StoredComment>> {
        let mut comments = Vec::new();
        for (suffix, bytes) in self.storage.meta_blobs_with_prefix(COMMENT_PREFIX)? {
            let Some((kind, name)) = suffix.split_once(':') else {
                continue;
            };
            let Ok(text) = String::from_utf8(bytes) else {
                continue;
            };
            comments.push(StoredComment {
                kind: kind.to_string(),
                name: name.to_string(),
                text,
            });
        }
        Ok(comments)
    }

    /// The comment records that belong to `table`: its own, its columns' and its
    /// constraints'. `(kind, name, text bytes)` each. Best effort, like the other
    /// prefix scans of a table's side records: a scan that fails finds nothing.
    fn table_comment_records(&self, table: &str) -> Vec<(String, String, Vec<u8>)> {
        let mut records = Vec::new();
        if let Ok(Some(value)) = self.storage.get(&Self::comment_key("table", table)) {
            records.push(("table".to_string(), table.to_string(), value));
        }
        for kind in ["column", "constraint"] {
            let prefix = format!("meta:comment:{kind}:{table}:");
            for (suffix, value) in self.storage.meta_blobs_with_prefix(&prefix).unwrap_or_default() {
                records.push((kind.to_string(), format!("{table}:{suffix}"), value));
            }
        }
        records
    }

    /// `DROP TABLE`: the table's comment records leave in `batch`.
    pub(super) fn stage_table_comments_drop(&self, table: &str, batch: &mut rocksdb::WriteBatch) {
        for (kind, name, _) in self.table_comment_records(table) {
            batch.delete(Self::comment_key(&kind, &name));
        }
    }

    /// `RENAME TO`: the table's comment records under its new name, as `(key to
    /// delete, key to put, value)` moves for the caller's batch (which seals the
    /// values).
    pub(super) fn table_comment_moves(&self, old: &str, new: &str) -> Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> {
        let mut moves = Vec::new();
        for (kind, name, value) in self.table_comment_records(old) {
            let moved = match name.strip_prefix(old).and_then(|rest| rest.strip_prefix(':')) {
                Some(rest) if kind != "table" => format!("{new}:{rest}"),
                _ => new.to_string(),
            };
            moves.push((Self::comment_key(&kind, &name), Self::comment_key(&kind, &moved), value));
        }
        moves
    }

    /// `ALTER TABLE`: the comments of the columns the statement renamed (`old`,
    /// `new`) follow them and those of the columns it dropped go, in its group.
    pub(crate) fn stage_column_comment_changes(
        &self,
        table: &str,
        renamed: &[(String, String)],
        dropped: &[String],
        ddl: &mut crate::storage::DdlBatch<'_>,
    ) -> Result<()> {
        if renamed.is_empty() && dropped.is_empty() {
            return Ok(());
        }
        // Read every value first: a swap of two names must not read a value this
        // statement has already moved.
        let mut moved: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for (old, new) in renamed {
            let from = Self::comment_key("column", &format!("{table}:{old}"));
            if let Some(value) = self.storage.get(&from)? {
                ddl.delete(&from);
                moved.push((Self::comment_key("column", &format!("{table}:{new}")), value));
            }
        }
        for column in dropped {
            let key = Self::comment_key("column", &format!("{table}:{column}"));
            if self.storage.get(&key)?.is_some() {
                ddl.delete(&key);
            }
        }
        for (key, value) in moved {
            ddl.put(&key, &value)?;
        }
        Ok(())
    }

    /// The comment of a dropped index or view leaves with it.
    pub(crate) fn stage_object_comment_drop(kind: &str, name: &str, ddl: &mut crate::storage::DdlBatch<'_>) {
        ddl.delete(Self::comment_key(kind, name));
    }
}
