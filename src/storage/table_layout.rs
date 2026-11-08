//! Spec R5.5: what a row version written before a change of main's table needs to
//! be read under the table's definition now: `meta:table_layout:{t}`.
//!
//! A row version (`v:`) keeps the layout it was written with, values by position.
//! `ALTER TABLE … DROP COLUMN` writes the new image of every row, with a version of
//! each at the statement's one timestamp, in its one write group, so a read at a
//! snapshot after it finds every row in today's layout. A read at an older snapshot
//! (`AS OF`, a REPEATABLE READ transaction begun before it, the transaction that ran
//! the DROP itself) finds a version with the dropped column still in place, and read
//! by position under today's schema it showed the dropped column's value under the
//! next column's name. So the record keeps, in order, every such change since the
//! table took its name, and [`TableLayout::bring_forward`] takes a version through
//! the changes made after it: a version is read under the definition it was written
//! with, then shown in today's columns.
//!
//! What cannot be brought forward is recorded as a cut, and a read that crosses one
//! is refused (0A000, the marker of a branch anchor refused for the same reason):
//!
//! * `RENAME TO`: the history of the rows stayed with the old name, which purges it.
//!   Before the rename the new name holds nothing right to read.
//! * a materialized view refreshed by swapping its data table, the same way;
//! `SET STORAGE` preserves logical values and positions. Its legacy cut record is
//! retained on disk for compatibility, but logical historical images cross it. An
//! actually retained physical reference is refused per image, rather than resolving
//! it using mutable current side storage or refusing the whole table's history.
//!
//! Needs no record: `ADD COLUMN` (a shorter version reads the column's missing value,
//! `storage::row_shape`) and `RENAME COLUMN` (positions stay; the value is the
//! column's under its new name).
//!
//! Written in the change's own group (`Catalog`), deleted by `DROP TABLE`, replaced on
//! the new name by `RENAME TO` (the old name's goes), loaded into
//! `SnapshotManager::table_layout`'s cache and forgotten with a table's missing values,
//! which every schema change on the primary and every applied catalog change on a
//! standby forget.

use serde::{Deserialize, Serialize};

use crate::{Error, Result, Tuple};

/// The prefix of the family.
pub const TABLE_LAYOUT_PREFIX: &str = "meta:table_layout:";

/// Why a table's history before a time cannot be read under its name now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoryCut {
    /// `ALTER TABLE … RENAME TO` gave the table this name.
    Renamed,
    /// A materialized view's rows were replaced by another table's (`table` is its
    /// data table).
    Refreshed,
    /// Legacy representation boundary (`SET STORAGE`), not a logical-history cut.
    StorageMoved,
}

/// One change of a table's layout, at the timestamp of its write group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutStep {
    /// `DROP COLUMN`: a version older than `at` was `width` columns wide, and its
    /// value at `position` belongs to no column now.
    DroppedColumn { at: u64, width: u32, position: u32 },
    /// Nothing older than `at` can be read under the table's name (the module docs).
    /// `through_row`: the highest row id the table held when a rename (or a refresh's
    /// swap) moved its rows here without their history. A row at or under it existed
    /// at `at`; one above it was created since.
    Cut {
        at: u64,
        cause: HistoryCut,
        through_row: Option<u64>,
    },
}

impl LayoutStep {
    fn at(&self) -> u64 {
        match self {
            Self::DroppedColumn { at, .. } | Self::Cut { at, .. } => *at,
        }
    }
}

/// `meta:table_layout:{t}`: the layout changes of a table, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableLayout {
    steps: Vec<LayoutStep>,
}

impl TableLayout {
    /// The record's key.
    pub fn key(table: &str) -> Vec<u8> {
        format!("{TABLE_LAYOUT_PREFIX}{table}").into_bytes()
    }

    /// A record as stored (decrypted).
    pub fn decode(table: &str, bytes: &[u8]) -> Result<Self> {
        bincode::deserialize(bytes)
            .map_err(|e| Error::storage(format!("the layout record of table '{table}' does not decode: {e}")))
    }

    /// The record's stored form (before sealing).
    pub fn encode(&self) -> Result<Vec<u8>> {
        bincode::serialize(self).map_err(|e| Error::storage(format!("Failed to serialize a layout record: {e}")))
    }

    /// Record a `DROP COLUMN` of the column at `position` of a table `width` wide,
    /// whose new row images carry the timestamp `at`.
    pub fn dropped_column(&mut self, at: u64, width: usize, position: usize) -> Result<()> {
        let narrow = |n: usize| {
            u32::try_from(n).map_err(|_| Error::internal("a table wider than u32::MAX columns has no layout record"))
        };
        self.steps.push(LayoutStep::DroppedColumn {
            at,
            width: narrow(width)?,
            position: narrow(position)?,
        });
        Ok(())
    }

    /// Record that nothing older than `at` can be read under the name; `through_row`
    /// as [`LayoutStep::Cut`] has it.
    pub fn cut(&mut self, at: u64, cause: HistoryCut, through_row: Option<u64>) {
        self.steps.push(LayoutStep::Cut { at, cause, through_row });
    }

    /// The time of the newest change; 0 for none.
    pub fn newest(&self) -> u64 {
        self.steps.iter().map(LayoutStep::at).max().unwrap_or(0)
    }

    /// Does a version written at `written` need [`Self::bring_forward`]?
    #[inline]
    pub fn changed_since(&self, written: u64) -> bool {
        self.newest() > written
    }

    /// The first cut after `snapshot`: before it, even a row that has no version at all
    /// (which reads its current image at every snapshot) is not what the name held
    /// then. `AS OF` and a branch's anchor refuse it alike.
    pub fn first_cut_after(&self, snapshot: u64) -> Option<HistoryCut> {
        self.steps.iter().find_map(|step| match step {
            LayoutStep::Cut { at, cause, .. } if snapshot < *at && *cause != HistoryCut::StorageMoved => Some(*cause),
            _ => None,
        })
    }

    /// [`Self::first_cut_after`] as `AS OF`'s refusal (0A000).
    pub fn refuse_reads_before(&self, table: &str, snapshot: u64) -> Result<()> {
        match self.first_cut_after(snapshot) {
            Some(cause) => Err(Error::history_before_layout_change(table, &describe_cut(cause, table))),
            None => Ok(()),
        }
    }

    /// The cut at or before `snapshot` that moved row `row` here without its history,
    /// if any: a read at `snapshot` that finds no version of the row, when the row has
    /// one after it, cannot tell what the row was (it existed: its id is at or under
    /// what the cut moved). The newest such cut decides.
    pub fn lost_history(&self, row: u64, snapshot: u64) -> Option<HistoryCut> {
        self.steps.iter().rev().find_map(|step| match step {
            LayoutStep::Cut {
                at,
                cause,
                through_row: Some(through),
            } if *at <= snapshot => (row <= *through).then_some(*cause),
            _ => None,
        })
    }

    /// Take `tuple`, a version written at `written`, through every change made after
    /// it, so it is in today's layout: a value of a dropped column is taken out (a
    /// version older than the column holds none, and loses nothing). Stops at a cut,
    /// which it returns. The caller then shapes the row to today's width as ever
    /// (`storage::row_shape`).
    pub fn try_bring_forward(&self, written: u64, tuple: &mut Tuple) -> Option<HistoryCut> {
        for step in &self.steps {
            match step {
                LayoutStep::DroppedColumn { at, width, position } if *at > written => {
                    // A version wider than its table was at the time (a row an old
                    // DROP left wide) answers that many columns, as the funnel does.
                    tuple.values.truncate(*width as usize);
                    let position = *position as usize;
                    if position < tuple.values.len() {
                        tuple.values.remove(position);
                    }
                }
                LayoutStep::Cut { at, cause, .. } if *at > written && *cause != HistoryCut::StorageMoved => {
                    return Some(*cause);
                }
                _ => {}
            }
        }
        None
    }

    /// [`Self::try_bring_forward`], a cut being `AS OF`'s refusal (0A000).
    pub fn bring_forward(&self, table: &str, written: u64, tuple: &mut Tuple) -> Result<()> {
        match self.try_bring_forward(written, tuple) {
            Some(cause) => Err(Error::history_before_layout_change(table, &describe_cut(cause, table))),
            None => Ok(()),
        }
    }
}

pub(crate) fn has_physical_storage_refs(tuple: &Tuple) -> bool {
    fn physical(value: &crate::Value) -> bool {
        match value {
            crate::Value::DictRef { .. } | crate::Value::CasRef { .. } | crate::Value::ColumnarRef => true,
            crate::Value::Array(values) => values.iter().any(physical),
            _ => false,
        }
    }
    tuple.values.iter().any(physical)
}

/// Refuse unresolved references after layout mapping and immutable-payload
/// decoding. A retained ColumnarRef has no historical payload identity.
pub(crate) fn refuse_physical_version(table: &str, row: u64, tuple: &Tuple) -> Result<()> {
    if has_physical_storage_refs(tuple) {
        return Err(Error::history_before_layout_change(
            table,
            &format!(
                "retained row version {row} contains physical storage references without a retained logical payload"
            ),
        ));
    }
    Ok(())
}

/// What happened to `table`, for a refusal: the words of the shape record
/// (`Catalog`'s `TableShapeChange`), which a branch anchor's refusal used before.
pub fn describe_cut(cause: HistoryCut, table: &str) -> String {
    match cause {
        HistoryCut::Renamed => format!("table \"{table}\" was renamed"),
        HistoryCut::Refreshed => format!(
            "materialized view \"{}\" was refreshed",
            table.strip_prefix("__mv_").unwrap_or(table)
        ),
        HistoryCut::StorageMoved => {
            format!("table \"{table}\" had a column moved to other storage")
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::Value;

    fn row(values: &[i32]) -> Tuple {
        Tuple::new(values.iter().map(|v| Value::Int4(*v)).collect())
    }

    /// (id, x, y, z): DROP x at 10, then DROP z (position 2 of (id, y, z)) at 20.
    fn two_drops() -> TableLayout {
        let mut layout = TableLayout::default();
        layout.dropped_column(10, 4, 1).unwrap();
        layout.dropped_column(20, 3, 2).unwrap();
        layout
    }

    #[test]
    fn a_version_older_than_both_drops_loses_both_columns() {
        let mut tuple = row(&[1, 10, 20, 30]);
        two_drops().bring_forward("t", 5, &mut tuple).unwrap();
        assert_eq!(tuple, row(&[1, 20]));
    }

    #[test]
    fn a_version_between_them_loses_only_the_later() {
        let mut tuple = row(&[1, 20, 30]);
        two_drops().bring_forward("t", 15, &mut tuple).unwrap();
        assert_eq!(tuple, row(&[1, 20]));
    }

    #[test]
    fn a_version_written_by_the_drop_itself_is_left_alone() {
        let mut tuple = row(&[1, 20]);
        two_drops().bring_forward("t", 20, &mut tuple).unwrap();
        assert_eq!(tuple, row(&[1, 20]));
        assert!(!two_drops().changed_since(20));
        assert!(two_drops().changed_since(19));
    }

    #[test]
    fn a_version_older_than_the_dropped_column_loses_nothing() {
        // (id) then ADD x, ADD y; DROP y (position 2 of 3): a version of (id) only.
        let mut layout = TableLayout::default();
        layout.dropped_column(10, 3, 2).unwrap();
        let mut tuple = row(&[1]);
        layout.bring_forward("t", 5, &mut tuple).unwrap();
        assert_eq!(tuple, row(&[1]));
    }

    #[test]
    fn a_cut_refuses_what_is_older_and_nothing_else() {
        let mut layout = two_drops();
        layout.cut(30, HistoryCut::Renamed, Some(3));
        let error = layout.refuse_reads_before("t", 29).unwrap_err().to_string();
        assert!(error.contains(crate::error::BRANCH_ANCHOR_UNREADABLE_MARKER), "{error}");
        assert!(error.contains("was renamed"), "{error}");
        layout.refuse_reads_before("t", 30).unwrap();
        let mut tuple = row(&[1, 20]);
        assert!(layout.bring_forward("t", 25, &mut tuple).is_err());
        layout.bring_forward("t", 31, &mut tuple).unwrap();
    }

    #[test]
    fn a_row_the_cut_moved_has_no_history_before_it_and_a_later_one_does() {
        let mut layout = two_drops();
        layout.cut(30, HistoryCut::Renamed, Some(3));
        assert_eq!(layout.lost_history(3, 35), Some(HistoryCut::Renamed));
        assert_eq!(layout.lost_history(4, 35), None, "created after the rename");
        assert_eq!(layout.lost_history(3, 29), None, "before the cut, refused as a whole");
        layout.cut(40, HistoryCut::StorageMoved, None);
        assert_eq!(layout.lost_history(3, 45), Some(HistoryCut::Renamed));
    }

    #[test]
    fn the_record_round_trips() {
        let mut layout = two_drops();
        layout.cut(30, HistoryCut::StorageMoved, None);
        let bytes = layout.encode().unwrap();
        assert_eq!(TableLayout::decode("t", &bytes).unwrap(), layout);
    }

    #[test]
    fn a_representation_boundary_keeps_logical_history_readable() {
        let mut layout = TableLayout::default();
        layout.cut(30, HistoryCut::StorageMoved, None);
        assert_eq!(layout.first_cut_after(1), None);
        layout.refuse_reads_before("t", 1).unwrap();
        let mut logical = row(&[1, 20]);
        layout.bring_forward("t", 1, &mut logical).unwrap();
        assert_eq!(logical, row(&[1, 20]));
        let physical = Tuple::new(vec![Value::Int4(1), Value::ColumnarRef]);
        let error = refuse_physical_version("t", 1, &physical).unwrap_err().to_string();
        assert!(error.contains(crate::error::BRANCH_ANCHOR_UNREADABLE_MARKER), "{error}");
    }
}
