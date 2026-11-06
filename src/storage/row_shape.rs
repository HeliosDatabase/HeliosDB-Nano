//! Spec R5.2: the one row-shape funnel, and the missing values it answers from.
//!
//! A stored row keeps the arity it was written with; the schema a reader holds
//! may have moved since. Every full-row decode comes through
//! [`StorageEngine::shape_row`] (or [`RowShape::apply`], for the transaction's
//! staged rows), which gives the reader a tuple exactly as wide as its schema:
//!
//! * as wide as the schema: nothing happens. One length comparison, on the path
//!   every row takes on a table that was never altered;
//! * narrower: each absent column answers its MISSING VALUE, or NULL when it has
//!   none;
//! * wider: the tail is cut off.
//!
//! The partial decoders (`prefix_decode`) get the same answer through
//! [`MissingValues`], for a column past the end of the row.
//!
//! # Missing values
//!
//! `meta:attmissing:{table}:{column}` holds the sealed bincode [`Value`] a row
//! older than the column reads for it, as PostgreSQL's `attmissingval`:
//! `ALTER TABLE … ADD COLUMN … DEFAULT <stable>` evaluates the default once,
//! writes this key in the schema's own write group, and rewrites no row, at any
//! table size. The key belongs to the column, not to its default: it is removed
//! by DROP COLUMN of the column and DROP TABLE, carried by RENAME COLUMN and
//! RENAME TABLE, and a later change of the column's default never touches it.
//!
//! A table's values are read once and kept in
//! [`StorageEngine::missing_values`]'s cache beside the schema cache (a
//! negative answer too), so the funnel pays no read per row. The cache follows
//! the schema generation's discipline: a loader reads the generation first and
//! publishes under the map's shard lock only while it is unchanged; a writer
//! writes durably, bumps the generation, then removes the entry. Whichever order
//! they interleave in, a loader's stale answer is either refused or removed.
//!
//! Positions come from main's durable schema when the values are loaded. Every
//! schema change on main invalidates the entry, so they stay current; a reader
//! still holding the schema of a moment ago is the torn read R5's layout epochs
//! close, and adding columns moves no position anyway.
//!
//! # The row cache
//!
//! The row cache holds a row AS STORED, never shaped: [`StorageEngine::shaped_row_arc`]
//! shapes on the way out, with one length comparison when it fits. A row wider
//! than the reader's schema is not cached at all. A cached entry is therefore a
//! function of the row's stored bytes alone, and neither a schema change nor a
//! reader planned under the previous schema can make it serve the wrong shape.
//!
//! # Versions
//!
//! A snapshot-resolved read shapes the version it resolves, not `data:`, so a
//! statement that rewrites rows in place writes a version of each in the same
//! group ([`StorageEngine::stage_added_column_rows`]). Without one it would resolve
//! the row's previous version for good, and a volatile column, which has no missing
//! value to fall back on, would read NULL in every transaction.

use super::prefix_decode::DecodedNumericValue;
use super::{Catalog, StorageEngine};
use crate::sql::default_volatility::{default_is_stable, evaluate_column_default};
use crate::sql::LogicalExpr;
use crate::{DataType, Error, Result, Schema, Tuple, Value};
use rocksdb::{Direction, IteratorMode, ReadOptions};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// The NULL a column with no missing value answers.
static NO_MISSING_VALUE: Value = Value::Null;

/// `DataRow`s refused because their field count disagreed with the row
/// description, on any wire, since this process started: see
/// [`refuse_mismatched_row`].
static DATA_ROW_ARITY_REFUSALS: AtomicU64 = AtomicU64::new(0);

/// How many rows the wire writers refused for disagreeing with their row
/// description (spec R5.2). Execute compares a Described statement's shape before
/// it runs anything, so this moves only for what that cannot see: a schema change
/// between that comparison and the row, or a row a reader path built wrongly,
/// which is a bug. In both the client got an error instead of a row it could not
/// parse.
pub fn data_row_arity_refusals() -> u64 {
    DATA_ROW_ARITY_REFUSALS.load(Ordering::Relaxed)
}

/// The error of a row whose field count differs from its row description, and
/// the count behind [`data_row_arity_refusals`]. Nothing of the row has been
/// written when a wire writer returns this.
#[cold]
pub fn refuse_mismatched_row(fields: usize, described: usize) -> Error {
    DATA_ROW_ARITY_REFUSALS.fetch_add(1, Ordering::Relaxed);
    tracing::warn!(
        "refused a result row of {} fields under a row description of {}: the row was not sent",
        fields,
        described
    );
    Error::query_execution(format!(
        "{}: a row of {fields} fields does not fit a row description of {described}. The row was not sent, and \
         whatever the statement wrote stands: check that before executing it again, after preparing it again",
        crate::error::ROW_DESCRIPTION_MISMATCH_MARKER
    ))
}

/// What [`StorageEngine::stage_added_column_rows`] put in its batch.
pub struct StagedRewrite {
    /// Rows rewritten.
    pub(crate) rows: u64,
    /// The rewrite's snapshot, to publish once the batch is durable. `None` with
    /// time travel off.
    pub(crate) snapshot: Option<super::time_travel::SnapshotMetadata>,
}

/// One table's missing values, by position in main's schema.
#[derive(Debug, Default)]
pub struct MissingValues {
    slots: Vec<Value>,
}

impl MissingValues {
    /// A table's values by position, for a unit test of a decoder.
    #[cfg(test)]
    pub(crate) fn from_slots(slots: Vec<Value>) -> Self {
        Self { slots }
    }

    /// `by_name` by position in `schema`; a column it does not name answers NULL.
    pub(crate) fn from_schema(schema: &Schema, mut by_name: HashMap<String, Value>) -> Self {
        Self {
            slots: schema
                .columns
                .iter()
                .map(|column| by_name.remove(&column.name).unwrap_or(Value::Null))
                .collect(),
        }
    }

    /// What column position `column` answers on a row that ends before it.
    #[inline]
    pub(crate) fn value(&self, column: usize) -> &Value {
        self.slots.get(column).unwrap_or(&NO_MISSING_VALUE)
    }

    /// [`Self::value`] for the numeric fast paths: `None` when the value is not
    /// one of the primitive numeric shapes, which sends the caller to its
    /// generic decoder exactly as a stored value of that shape does.
    pub(crate) fn numeric(&self, column: usize) -> Option<DecodedNumericValue> {
        match self.value(column) {
            Value::Null => Some(DecodedNumericValue::Null),
            Value::Int2(value) => Some(DecodedNumericValue::Int(i64::from(*value))),
            Value::Int4(value) => Some(DecodedNumericValue::Int(i64::from(*value))),
            Value::Int8(value) => Some(DecodedNumericValue::Int(*value)),
            Value::Float4(value) => Some(DecodedNumericValue::Float(f64::from(*value))),
            Value::Float8(value) => Some(DecodedNumericValue::Float(*value)),
            _ => None,
        }
    }
}

/// Shape `tuple` to `width` columns. The funnel's one rule, for every caller.
#[inline]
pub fn shape_tuple(tuple: &mut Tuple, width: usize, missing: Option<&MissingValues>) {
    if tuple.values.len() != width {
        reshape_tuple(tuple, width, missing);
    }
}

#[cold]
fn reshape_tuple(tuple: &mut Tuple, width: usize, missing: Option<&MissingValues>) {
    let stored = tuple.values.len();
    if stored > width {
        tuple.values.truncate(width);
        return;
    }
    tuple.values.reserve(width - stored);
    for column in stored..width {
        tuple
            .values
            .push(missing.map_or(Value::Null, |values| values.value(column).clone()));
    }
}

/// The funnel with everything it needs in hand, for code that holds no engine
/// (the transaction's staged rows) and for a loop over rows.
#[derive(Debug, Clone)]
pub struct RowShape {
    width: usize,
    missing: Option<Arc<MissingValues>>,
}

impl RowShape {
    pub(crate) fn new(width: usize, missing: Option<Arc<MissingValues>>) -> Self {
        Self { width, missing }
    }

    /// Shape `tuple` to this reader's schema.
    #[inline]
    pub(crate) fn apply(&self, tuple: &mut Tuple) {
        shape_tuple(tuple, self.width, self.missing.as_deref());
    }
}

/// What an `ADD COLUMN` gives the rows that exist when it runs.
#[derive(Debug, Clone)]
pub enum ColumnFill {
    /// One value for every row: no default (NULL), a literal, or an expression
    /// with only stable functions, evaluated once, at the statement.
    Constant(Value),
    /// A volatile default: evaluated once per row, which takes a rewrite.
    PerRow(LogicalExpr),
}

impl ColumnFill {
    /// The fill for a column of `data_type` declared with `default`. A stable
    /// default that cannot be evaluated or cast fails here, before anything is
    /// written, as an INSERT using it would.
    pub(crate) fn for_default(default: Option<&LogicalExpr>, data_type: &DataType) -> Result<Self> {
        let Some(expr) = default else {
            return Ok(Self::Constant(Value::Null));
        };
        if default_is_stable(expr) {
            return Ok(Self::Constant(evaluate_column_default(expr, data_type)?));
        }
        Ok(Self::PerRow(expr.clone()))
    }

    /// The value for one more row.
    pub(crate) fn next_value(&self, data_type: &DataType) -> Result<Value> {
        match self {
            Self::Constant(value) => Ok(value.clone()),
            Self::PerRow(expr) => evaluate_column_default(expr, data_type),
        }
    }
}

impl StorageEngine {
    /// `table`'s missing values, from memory after the first call. `None` when
    /// the table has none, which is every table no `ADD COLUMN … DEFAULT` ever
    /// touched. See the module docs for the cache's rule.
    pub(crate) fn missing_values(&self, table: &str) -> Result<Option<Arc<MissingValues>>> {
        if let Some(entry) = self.missing_values_cache.get(table) {
            return Ok(entry.value().clone());
        }
        let generation = self.schema_generation();
        let loaded = self.load_missing_values(table)?;
        if let dashmap::mapref::entry::Entry::Vacant(slot) = self.missing_values_cache.entry(table.to_string()) {
            if self.schema_generation() == generation {
                slot.insert(loaded.clone());
            }
        }
        Ok(loaded)
    }

    fn load_missing_values(&self, table: &str) -> Result<Option<Arc<MissingValues>>> {
        let by_name = self.stored_missing_values(table)?;
        if by_name.is_empty() {
            return Ok(None);
        }
        let Some(schema) = Catalog::new(self).durable_main_schema(table)? else {
            return Ok(None);
        };
        Ok(Some(Arc::new(MissingValues::from_schema(&schema, by_name))))
    }

    /// Every `meta:attmissing:{table}:` key, decoded, by column name.
    pub(crate) fn stored_missing_values(&self, table: &str) -> Result<HashMap<String, Value>> {
        let prefix = Catalog::attmissing_prefix(table);
        let mut by_name = HashMap::new();
        let mut read_opts = ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self
            .db
            .iterator_opt(IteratorMode::From(prefix.as_bytes(), Direction::Forward), read_opts);
        for item in iter {
            let (key, raw) = item.map_err(|e| Error::storage(format!("Iterator error: {e}")))?;
            let Some(column) = key.strip_prefix(prefix.as_bytes()) else {
                break;
            };
            let plain = self.decode_stored_ref(&key, &raw)?;
            let value: Value = bincode::deserialize(&plain).map_err(|e| {
                Error::storage(format!(
                    "the missing value of a column of '{table}' does not decode: {e}"
                ))
            })?;
            by_name.insert(String::from_utf8_lossy(column).into_owned(), value);
        }
        Ok(by_name)
    }

    /// Forget `table`'s loaded missing values. Called after the durable write
    /// and the generation bump (see the module docs).
    pub(crate) fn invalidate_missing_values(&self, table: &str) {
        self.missing_values_cache.remove(table);
    }

    /// Forget every table's loaded missing values.
    pub(crate) fn clear_missing_values(&self) {
        self.missing_values_cache.clear();
    }

    /// The funnel for a reader that takes `width` columns of `table`, for a
    /// loop over rows or for code without an engine (the transaction).
    pub(crate) fn row_shape(&self, table: &str, width: usize) -> Result<RowShape> {
        Ok(RowShape::new(width, self.missing_values(table)?))
    }

    /// THE funnel for one current row of main: `tuple`, decoded from its stored
    /// bytes, comes back exactly `width` columns wide. One length comparison
    /// when the row already fits.
    #[inline]
    pub(crate) fn shape_row(&self, table: &str, width: usize, tuple: &mut Tuple) -> Result<()> {
        if tuple.values.len() != width {
            shape_tuple(tuple, width, self.missing_values(table)?.as_deref());
        }
        Ok(())
    }

    /// The funnel for a row VERSION (`v:`): under
    /// `snapshot_schema_evolution = "strict"` a version keeps its stored arity,
    /// the rollback that setting exists for.
    #[inline]
    pub(crate) fn shape_version_row(&self, table: &str, width: usize, tuple: &mut Tuple) -> Result<()> {
        if self.null_pad_snapshot_schema() {
            self.shape_row(table, width, tuple)?;
        }
        Ok(())
    }

    /// A cached row (held as stored, see the module docs) shaped for its
    /// reader. The shared handle itself when it fits: a refcount bump.
    #[inline]
    pub(crate) fn shaped_row_arc(&self, table: &str, width: usize, row: Arc<Tuple>) -> Result<Arc<Tuple>> {
        if row.values.len() == width {
            return Ok(row);
        }
        let mut owned = (*row).clone();
        self.shape_row(table, width, &mut owned)?;
        Ok(Arc::new(owned))
    }

    /// Does main hold any row of `table`? One seek.
    pub(crate) fn table_has_rows(&self, table: &str) -> Result<bool> {
        let prefix = format!("data:{table}:");
        let mut read_opts = ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let mut iter = self
            .db
            .iterator_opt(IteratorMode::From(prefix.as_bytes(), Direction::Forward), read_opts);
        match iter.next() {
            Some(item) => {
                let (key, _) = item.map_err(|e| Error::storage(format!("Iterator error: {e}")))?;
                Ok(key.starts_with(prefix.as_bytes()))
            }
            None => Ok(false),
        }
    }

    /// Does main hold a row of `table` outside `row_ids`? Stops at the first.
    pub(crate) fn table_has_rows_other_than(
        &self,
        table: &str,
        row_ids: &std::collections::HashSet<u64>,
    ) -> Result<bool> {
        let prefix = format!("data:{table}:");
        let mut read_opts = ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self
            .db
            .iterator_opt(IteratorMode::From(prefix.as_bytes(), Direction::Forward), read_opts);
        for item in iter {
            let (key, _) = item.map_err(|e| Error::storage(format!("Iterator error: {e}")))?;
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            match Self::parse_row_id_after_prefix(&key, prefix.len()) {
                Some(row_id) if row_ids.contains(&row_id) => {}
                _ => return Ok(true),
            }
        }
        Ok(false)
    }

    /// Stage into `batch` every row of `table` rewritten for columns appended
    /// with `fills` (`(column, type, NOT NULL, fill)`, in order), for a volatile
    /// `ADD COLUMN`: the row shaped to `old_width` through the funnel, then one
    /// value per added column, and its version at the statement's one timestamp
    /// (`SnapshotManager::stage_rewritten_row_version`), so a snapshot-resolved
    /// read finds the values, and one that began before the statement still finds
    /// the rows as they were. Publish the returned snapshot once `batch` is written
    /// (`SnapshotManager::publish_rewrite_snapshot`).
    ///
    /// `batch` is measured after each row, versions included, and so are the copies
    /// main's live branches will take of the rows (phase B4: one per row and branch
    /// that reads the table through main). Above `max_bytes`
    /// the statement is refused (54000), as a NULL in a NOT NULL column is (23502):
    /// nothing has been written, by this function or by anyone it called. Volatile
    /// functions have run, though, so a `nextval` default has consumed its values.
    pub(crate) fn stage_added_column_rows(
        &self,
        table: &str,
        old_width: usize,
        fills: &[(&str, &DataType, bool, &ColumnFill)],
        max_bytes: u64,
        batch: &mut rocksdb::WriteBatch,
    ) -> Result<StagedRewrite> {
        // A rewrite makes the per-table committed-write watermark untrustworthy
        // (an open reader's fast path would serve rows past its snapshot); its
        // re-check reads the absent entry as "changed".
        self.invalidate_write_watermark(table);
        // With time travel off the commit path writes no versions and every
        // snapshot read answers from `data:`.
        let version = if self.time_travel_enabled() {
            let timestamp = self.next_timestamp();
            let metadata = self.snapshot_manager().stage_rewrite_snapshot(batch, timestamp)?;
            Some((timestamp, metadata))
        } else {
            None
        };
        let prefix = format!("data:{table}:");
        let shape = self.row_shape(table, old_width)?;
        // Sprinter 6bc4a4040eb7 (B4): each live branch that reads the table through
        // main takes a copy of every row this rewrites, from the same write, at the
        // size of the image it replaces: counted too, so a dry run refuses what the
        // write would, and the copies never take the write past the cap.
        let copying = self.branches_copying_main_rows(table)?;
        let mut copy_bytes = 0u64;
        let mut read_opts = ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self
            .db
            .iterator_opt(IteratorMode::From(prefix.as_bytes(), Direction::Forward), read_opts);
        let mut rows = 0u64;
        for item in iter {
            let (key, raw) = item.map_err(|e| Error::storage(format!("Iterator error: {e}")))?;
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            let stored = self.decode_stored_ref(&key, &raw)?;
            let mut tuple: Tuple = bincode::deserialize(&stored)
                .map_err(|e| Error::storage(format!("Failed to deserialize tuple: {e}")))?;
            shape.apply(&mut tuple);
            for (column, data_type, not_null, fill) in fills {
                let value = fill.next_value(data_type)?;
                if *not_null && matches!(value, Value::Null) {
                    return Err(Error::add_column_null_on_existing_rows(table, column));
                }
                tuple.values.push(value);
            }
            let image =
                bincode::serialize(&tuple).map_err(|e| Error::storage(format!("Failed to serialize tuple: {e}")))?;
            let sealed = super::tde::seal_row_value(self.key_manager_ref(), &key, &image)?;
            if let Some((timestamp, _)) = &version {
                if let Some(row_id) = Self::parse_row_id_after_prefix(&key, prefix.len()) {
                    self.snapshot_manager().stage_rewritten_row_version(
                        batch,
                        table,
                        row_id,
                        *timestamp,
                        sealed.as_ref(),
                    )?;
                }
            }
            batch.put(&key, sealed.as_ref());
            rows += 1;
            copy_bytes = copy_bytes.saturating_add(((key.len() + raw.len()) as u64).saturating_mul(copying));
            let bytes = (batch.size_in_bytes() as u64).saturating_add(copy_bytes);
            if bytes > max_bytes {
                return Err(Error::atomic_rewrite_too_large(table, bytes, max_bytes));
            }
        }
        Ok(StagedRewrite {
            rows,
            snapshot: version.map(|(_, metadata)| metadata),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::Column;

    fn missing() -> MissingValues {
        let schema = Schema::new(vec![
            Column::new("a", DataType::Int4),
            Column::new("b", DataType::Text),
            Column::new("c", DataType::Int4),
            Column::new("d", DataType::Text),
        ]);
        let by_name = HashMap::from([
            ("c".to_string(), Value::Int4(7)),
            ("d".to_string(), Value::String("x".to_string())),
            ("not_a_column".to_string(), Value::Int4(9)),
        ]);
        MissingValues::from_schema(&schema, by_name)
    }

    #[test]
    fn a_row_that_fits_is_left_alone() {
        let mut tuple = Tuple::new(vec![Value::Int4(1), Value::Null]);
        shape_tuple(&mut tuple, 2, Some(&missing()));
        assert_eq!(tuple.values, vec![Value::Int4(1), Value::Null]);
    }

    #[test]
    fn a_narrow_row_answers_each_columns_missing_value_or_null() {
        let mut tuple = Tuple::new(vec![Value::Int4(1)]);
        shape_tuple(&mut tuple, 4, Some(&missing()));
        assert_eq!(
            tuple.values,
            vec![
                Value::Int4(1),
                Value::Null,
                Value::Int4(7),
                Value::String("x".to_string())
            ]
        );
        let mut tuple = Tuple::new(vec![Value::Int4(1)]);
        shape_tuple(&mut tuple, 3, None);
        assert_eq!(tuple.values, vec![Value::Int4(1), Value::Null, Value::Null]);
    }

    #[test]
    fn a_wide_row_is_cut_to_the_schema() {
        let mut tuple = Tuple::new(vec![Value::Int4(1), Value::Int4(2), Value::Int4(3)]);
        shape_tuple(&mut tuple, 2, Some(&missing()));
        assert_eq!(tuple.values, vec![Value::Int4(1), Value::Int4(2)]);
    }

    #[test]
    fn a_column_past_the_known_ones_answers_null() {
        assert_eq!(missing().value(9), &Value::Null);
    }

    #[test]
    fn numeric_answers_follow_the_stored_shapes() {
        let values = missing();
        assert_eq!(values.numeric(0), Some(DecodedNumericValue::Null));
        assert_eq!(values.numeric(2), Some(DecodedNumericValue::Int(7)));
        assert_eq!(values.numeric(3), None, "text sends the caller to its generic decoder");
    }
}
