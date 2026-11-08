//! sprinter 6bc4a4040eb7 (phase B3): the branch CATALOG overlay — a branch's
//! own definitions of tables, read through the session's branch chain.
//!
//! # The model
//!
//! The main catalog (`meta:table:{t}`, `table_constraints:{t}`, the ART
//! registry, …) describes main. A branch level `L` may hold its OWN entry for
//! table `t` under `bmeta:{L}:table:{t}` ([`branch_table_key`]): either a full
//! definition of `t` or a tombstone ("`t` does not exist here"). A session on
//! a branch with chain `main → l1 → … → b` resolves `t` like it resolves a
//! row: the NEAREST level with an entry decides — a definition is the table's
//! definition, a tombstone means "no such table" — and with no entry on any
//! level main's catalog decides ([`BranchTableResolution`]). Every catalog
//! reader asks this first on a branch (`Catalog::get_table_schema`,
//! `table_exists`, `list_tables`, `load_table_constraints`,
//! `get_referencing_fks`, the identity and unique-key questions, and the
//! existence cache), so the planner, both DML executor families and every
//! introspection surface see the branch's catalog. On main the question costs
//! one thread-local read (`StorageEngine::is_branch_active`).
//!
//! # Isolated tables
//!
//! A definition written by `CREATE TABLE` or `ALTER TABLE … ADD COLUMN` on a
//! branch is `isolated`: the branch view (`branch_view.rs`) then reads `t`'s
//! rows from the defining level and its descendants ONLY — never from that
//! level's ancestors or main — and enforces PRIMARY KEY / UNIQUE with the
//! rules the definition carries ([`BranchTableDefinition::unique_rules`]),
//! never main's ART. `ADD COLUMN` therefore materializes every row the branch
//! sees into its own overlay under the new definition in the same atomic
//! batch as the definition (`EmbeddedDatabase` branch DDL, `branch_schema.rs`).
//!
//! # The record
//!
//! `BMT1` + format version `1` + the bincode encoding of
//! [`BranchTableRecord`], sealed like every catalog blob (`tde::seal`, read
//! back through `StorageEngine::get`). The record carries what phase B4 and
//! MERGE need:
//!
//! * `frozen` — a copy of the parent's definition (phase B4,
//!   `branch_freeze.rs`), NOT a change made on this level: what the level keeps
//!   seeing while the parent moves on. MERGE skips it. A tombstone is a table the
//!   parent is creating: this level never saw it.
//! * `parent_fingerprint` — the fingerprint ([`definition_fingerprint`]) of
//!   the definition `t` had on this level's PARENT when this level first wrote
//!   its own entry (`None`: the parent had no such table). `MERGE BRANCH`
//!   compares it with the target's CURRENT definition and refuses (55000) when
//!   they differ — the target changed the table after the branch did.
//! * `parent_incarnation` — the incarnation id of the table on the parent at
//!   that same moment. A fingerprint cannot tell "unchanged" from "dropped and
//!   recreated identically"; the incarnation can. MERGE compares it separately
//!   from the fingerprint (a merged `CREATE` gives the target a new id, so it
//!   could not be part of a definition's fingerprint).
//! * `created` / `added_columns` — how this level's definition relates to the
//!   parent's: created here (MERGE creates it on the target, replacing the
//!   target's), or the parent's plus these `ADD COLUMN`s, in order (MERGE
//!   applies them on the target with main's backfill rule).
//!
//! # Table incarnations (sprinter 6bc4a4040eb7, B3 fix pass)
//!
//! Every table of main has `meta:table_incarnation:{t}`: a RANDOM u64 minted
//! by `CREATE TABLE` in the batch that writes its schema, deleted by `DROP` in
//! its one-group purge, moved by `RENAME` (`Catalog`; WAL replay runs through
//! the same funnels). Random rather than a counter: no counter key has to stay
//! durable across a crash or be replicated. Tables older than this get theirs
//! at the first writable open (`Catalog::backfill_table_incarnations`, never on
//! a standby). A definition a branch creates mints its own; `ADD COLUMN` keeps
//! the base's.
//!
//! `bmeta:{L}:fork` is one record per branch, written in the branch's creation
//! batch: the incarnation, the definition fingerprint and the column names of
//! every table its parent had at the fork. MERGE refuses (55000) a table the
//! branch changed whose definition or incarnation on the target differs from
//! it, even when the branch only changed rows: those rows were written for the
//! definition at the fork, and are copied by position (B3 second fix pass; a
//! `DROP COLUMN` on main used to shift them silently). A branch with no fork
//! record was created by an earlier release: MERGE refuses it for any table it
//! changed rows of. A table with no entry in it was created after the fork, and
//! the parent's create froze a tombstone into the branch (phase B4), so it cannot
//! have changed rows of it. A table with no incarnation yet (a backfill that
//! failed) is UNKNOWN, never different.
//!
//! # Copies and the key index (B3 fix pass)
//!
//! `bfrz:{L}:{t}:{r}` marks level `L`'s entry for row `r` as a COPY — an
//! `ADD COLUMN`'s materialized row, phase B4's frozen pre-image — not a change
//! made on `L`: MERGE skips it, and `L`'s own write of the row clears it.
//! `bkey:{L}:{hex t}:{hex set}:{hex digest}:{r}` is `L`'s durable key index of an
//! isolated table (`branch_view.rs`).
//!
//! # Caches
//!
//! Records are cached per (engine, level, table), validated by the engine's
//! schema generation: every write of a record bumps it (and the level's write
//! generation for the table, so a cached committed overlay reloads). A
//! physical standby, whose records arrive below every bump, never caches.
//!
//! # Reclaim
//!
//! Every key of a level's catalog starts `bmeta:{L}:`; the reclaim of a dropped
//! or merged branch (phase B4, `BranchManager::reclaim_branch_keys`) deletes that
//! prefix with the rest of the overlay. Nothing reads a level's records except
//! through a chain that contains it, and a dropped branch can no longer be bound,
//! so a dropped branch's records are referenced by nothing.

use super::{Catalog, StorageEngine};
use crate::sql::logical_plan::ColumnDef;
use crate::sql::{LogicalExpr, TableConstraints};
use crate::storage::EnforcedUniqueIndex;
use crate::{Column, ColumnStorageMode, DataType, Error, Result, Schema};
use dashmap::DashMap;
use rocksdb::{Direction, IteratorMode, WriteBatch};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// The prefix of every branch-catalog key (`bmeta:{level}:…`).
pub const BRANCH_CATALOG_PREFIX: &str = "bmeta:";

/// `bmeta:{level}:table:{table}` — level `level`'s own entry for `table`.
pub fn branch_table_key(level: u64, table: &str) -> Vec<u8> {
    format!("bmeta:{level}:table:{table}").into_bytes()
}

/// `bmeta:{level}:table:` — every table entry of one level.
pub fn branch_tables_prefix(level: u64) -> String {
    format!("bmeta:{level}:table:")
}

/// Every catalog key of one level (`bmeta:{level}:`) — what a reclaim deletes.
pub fn branch_catalog_level_prefix(level: u64) -> String {
    format!("bmeta:{level}:")
}

/// `bmeta:{level}:missing:{table}` (phase B4 with spec R5.2) — the missing values
/// (`meta:attmissing:`) main had for `table` when level `level` took its frozen
/// record of it, by column name: what a narrow row there reads, whatever main does
/// to its own since. Written with the record, in its batch.
pub fn branch_missing_values_key(level: u64, table: &str) -> Vec<u8> {
    format!("bmeta:{level}:missing:{table}").into_bytes()
}

/// `bmeta:{level}:fork` — the incarnation of every table the level's parent
/// had when the level forked (see the module docs).
pub fn branch_fork_key(level: u64) -> Vec<u8> {
    format!("bmeta:{level}:fork").into_bytes()
}

/// The prefix of every unfinished MERGE BRANCH's durable intent.
pub const BRANCH_MERGE_INTENT_PREFIX: &str = "branch:merge:";

/// Durable FK validation policy of one merge. The separate versioned record
/// preserves existing MergeIntent bytes and is written/deleted with its intent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum MergeFkPolicy {
    #[default]
    Enforced,
    Audit,
    Bypass,
}

pub(crate) fn branch_merge_fk_policy_key(source: u64) -> Vec<u8> {
    format!("branch:merge_fk_policy:{source}").into_bytes()
}

/// `branch:merge:{source}` — the durable intent of an unfinished MERGE of the
/// branch `source` (`MergeIntent`).
pub fn branch_merge_intent_key(source: u64) -> Vec<u8> {
    format!("{BRANCH_MERGE_INTENT_PREFIX}{source}").into_bytes()
}

/// sprinter 6bc4a4040eb7 (B3 fix pass): a fresh table incarnation id — random,
/// so nothing has to keep a counter durable across a crash or a replica.
pub fn fresh_table_incarnation() -> u64 {
    rand::random()
}

// ---------------------------------------------------------------------------
// The record
// ---------------------------------------------------------------------------

const RECORD_MAGIC: &[u8; 4] = b"BMT1";
const RECORD_FORMAT_VERSION: u8 = 1;

/// One PRIMARY KEY / UNIQUE rule of a branch definition — what the branch
/// view enforces for a table the branch defines (main's ART describes main's
/// tables only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchUniqueRule {
    /// The rule's key in the claim registry (the index name main would use).
    pub index_name: String,
    /// The name a 23505 prints.
    pub label: String,
    /// The constrained columns, in rule order.
    pub columns: Vec<String>,
    /// PRIMARY KEY rather than UNIQUE.
    pub is_primary_key: bool,
}

impl BranchUniqueRule {
    /// The rule in the shape the branch view's checks take.
    pub fn to_enforced(&self) -> EnforcedUniqueIndex {
        EnforcedUniqueIndex {
            index_name: self.index_name.clone(),
            label: self.label.clone(),
            columns: self.columns.clone(),
            is_primary_key: self.is_primary_key,
        }
    }

    /// A rule of main's ART registry, kept as it is (same name, same label).
    pub fn from_enforced(rule: &EnforcedUniqueIndex) -> Self {
        Self {
            index_name: rule.index_name.clone(),
            label: rule.label.clone(),
            columns: rule.columns.clone(),
            is_primary_key: rule.is_primary_key,
        }
    }
}

/// A table definition held by a branch level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchTableDefinition {
    /// The columns, exactly as the main catalog keeps them (`meta:table:`).
    pub schema: Schema,
    /// FOREIGN KEY / CHECK / UNIQUE records (`table_constraints:`).
    pub constraints: TableConstraints,
    /// The PRIMARY KEY / UNIQUE rules the branch view enforces.
    pub unique_rules: Vec<BranchUniqueRule>,
    /// IDENTITY / SERIAL columns (`meta:identity:`), for introspection.
    pub identity_columns: Vec<String>,
    /// The table's incarnation id: fresh for a definition created on a branch,
    /// the base's for one derived by `ADD COLUMN`, main's for main's own (`0`
    /// for a main table with no id yet).
    pub incarnation: u64,
}

/// What a level holds for a table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BranchTableState {
    /// The level's definition of the table.
    Defined(BranchTableDefinition),
    /// The level dropped the table: it does not exist here (or below, unless
    /// a descendant defines it again).
    Dropped,
}

/// One `ADD COLUMN` a level applied over its parent's definition — replayed
/// by MERGE on the target with main's backfill rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchAddedColumn {
    /// The column as it was added (the catalog form).
    pub column: Column,
    /// Its `DEFAULT`, as planned (the backfill source).
    pub default: Option<LogicalExpr>,
    /// The statement's column definition, verbatim.
    pub definition: ColumnDef,
}

/// `bmeta:{level}:table:{table}` — see the module docs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchTableRecord {
    /// A definition, or a tombstone.
    pub state: BranchTableState,
    /// Rows are read from the defining level and its descendants only.
    pub isolated: bool,
    /// Phase B4: a copy of the parent's definition, not a change made on this
    /// level (`branch_freeze.rs`). MERGE skips it.
    pub frozen: bool,
    /// The fingerprint of the definition the PARENT had when this level first
    /// wrote its own entry for the table; `None` = the parent had no such
    /// table. MERGE refuses when the target's current definition differs.
    pub parent_fingerprint: Option<u64>,
    /// The incarnation of the table on the PARENT at that same moment; `None`
    /// = the parent had no such table. MERGE refuses when the target's current
    /// incarnation differs: the table was dropped and recreated.
    pub parent_incarnation: Option<u64>,
    /// The definition was created on this level (CREATE TABLE / CTAS), not
    /// derived from the parent's.
    pub created: bool,
    /// The `ADD COLUMN`s this level applied over the parent's definition, in
    /// order (empty when `created`).
    pub added_columns: Vec<BranchAddedColumn>,
}

impl BranchTableRecord {
    /// The definition, unless this is a tombstone.
    pub fn definition(&self) -> Option<&BranchTableDefinition> {
        match &self.state {
            BranchTableState::Defined(definition) => Some(definition),
            BranchTableState::Dropped => None,
        }
    }

    /// The stored form (unsealed): magic, format version, bincode body.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let body = bincode::serialize(self)
            .map_err(|e| Error::storage(format!("Failed to serialize a branch table record: {e}")))?;
        let mut out = Vec::with_capacity(RECORD_MAGIC.len() + 1 + body.len());
        out.extend_from_slice(RECORD_MAGIC);
        out.push(RECORD_FORMAT_VERSION);
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// Decode the stored (unsealed) form of `table`'s record. A record this
    /// build cannot read is an error, never "no record": guessing would let
    /// a branch read main's definition of a table it dropped or redefined.
    pub fn decode(table: &str, bytes: &[u8]) -> Result<Self> {
        let body = bytes
            .strip_prefix(RECORD_MAGIC.as_slice())
            .and_then(<[u8]>::split_first)
            .filter(|(version, _)| **version == RECORD_FORMAT_VERSION)
            .map(|(_, body)| body)
            .ok_or_else(|| {
                Error::storage(format!(
                    "the branch catalog record of table '{table}' has an unknown format"
                ))
            })?;
        bincode::deserialize(body).map_err(|e| {
            Error::storage(format!(
                "the branch catalog record of table '{table}' does not decode: {e}"
            ))
        })
    }
}

// ---------------------------------------------------------------------------
// Fingerprints and rules
// ---------------------------------------------------------------------------

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// sprinter 6bc4a4040eb7 (phase B3): a stable fingerprint of a table
/// definition — every column's name, type, nullability, key flags, default
/// and storage mode, in order, and the constraint records — for MERGE's
/// "has the target changed this table since the branch did?". The alias
/// fields a resolver stamps on a schema (`source_table`, `source_table_name`)
/// are not part of it. Stable across processes (bincode + FNV-1a).
///
/// B3 fix pass: the constraint lists are canonicalized (each record serialized,
/// each list sorted — declaration order is not part of a definition), and the
/// PRIMARY KEY / UNIQUE rules are part of it as column SETS (lowercased, sorted,
/// deduplicated; not names, which can be re-minted across a reopen), so a
/// `CREATE UNIQUE INDEX` on the target since the fork refuses the merge. Column
/// order IS part of it: rows are positional. The table's incarnation is NOT:
/// MERGE compares it separately.
pub fn definition_fingerprint(
    schema: &Schema,
    constraints: &TableConstraints,
    rules: &[BranchUniqueRule],
) -> Result<u64> {
    fn canonical_records<T: Serialize>(records: &[T]) -> Result<Vec<Vec<u8>>> {
        let mut encoded = records
            .iter()
            .map(|record| {
                bincode::serialize(record)
                    .map_err(|e| Error::storage(format!("Failed to fingerprint a table definition: {e}")))
            })
            .collect::<Result<Vec<_>>>()?;
        encoded.sort_unstable();
        Ok(encoded)
    }
    #[derive(Serialize)]
    struct CanonicalColumn<'a> {
        name: &'a str,
        data_type: &'a DataType,
        nullable: bool,
        primary_key: bool,
        unique: bool,
        default_expr: &'a Option<String>,
        storage_mode: ColumnStorageMode,
    }
    let columns: Vec<CanonicalColumn<'_>> = schema
        .columns
        .iter()
        .map(|column| CanonicalColumn {
            name: &column.name,
            data_type: &column.data_type,
            nullable: column.nullable,
            primary_key: column.primary_key,
            unique: column.unique,
            default_expr: &column.default_expr,
            storage_mode: column.storage_mode,
        })
        .collect();
    let mut rule_sets: Vec<Vec<String>> = rules
        .iter()
        .map(|rule| {
            let mut set: Vec<String> = rule.columns.iter().map(|c| c.to_ascii_lowercase()).collect();
            set.sort_unstable();
            set
        })
        .collect();
    rule_sets.sort_unstable();
    rule_sets.dedup();
    let bytes = bincode::serialize(&(
        columns,
        canonical_records(&constraints.foreign_keys)?,
        canonical_records(&constraints.check_constraints)?,
        canonical_records(&constraints.unique_constraints)?,
        rule_sets,
    ))
    .map_err(|e| Error::storage(format!("Failed to fingerprint a table definition: {e}")))?;
    Ok(fnv1a(&bytes))
}

/// The PRIMARY KEY / UNIQUE rules main's catalog would register for a table
/// created with `schema` and `constraints` (`Catalog::create_table`'s column
/// flags plus `register_unique_constraint_indexes`), with main's names: the
/// PK is `{t}_pkey`, a column UNIQUE `{t}_{col}_key`, a table-level UNIQUE
/// `{t}_{cols}_key` labelled with its constraint name. One rule per column
/// set.
pub fn derive_unique_rules(table: &str, schema: &Schema, constraints: &TableConstraints) -> Vec<BranchUniqueRule> {
    fn same_set(a: &[String], b: &[String]) -> bool {
        let normalize = |cols: &[String]| -> Vec<String> {
            let mut v: Vec<String> = cols.iter().map(|c| c.to_ascii_lowercase()).collect();
            v.sort_unstable();
            v
        };
        normalize(a) == normalize(b)
    }
    let mut rules: Vec<BranchUniqueRule> = Vec::new();
    let mut pk: Vec<String> = schema
        .columns
        .iter()
        .filter(|c| c.primary_key)
        .map(|c| c.name.clone())
        .collect();
    if pk.is_empty() {
        if let Some(declared) = constraints
            .unique_constraints
            .iter()
            .find(|u| u.is_primary_key && !u.columns.is_empty())
        {
            pk = declared.columns.clone();
        }
    }
    if !pk.is_empty() {
        let name = format!("{table}_pkey");
        rules.push(BranchUniqueRule {
            index_name: name.clone(),
            label: name,
            columns: pk,
            is_primary_key: true,
        });
    }
    for column in &schema.columns {
        if !column.unique || column.primary_key {
            continue;
        }
        let columns = vec![column.name.clone()];
        if rules.iter().any(|rule| same_set(&rule.columns, &columns)) {
            continue;
        }
        let name = format!("{table}_{}_key", column.name);
        rules.push(BranchUniqueRule {
            index_name: name.clone(),
            label: name,
            columns,
            is_primary_key: false,
        });
    }
    for unique in &constraints.unique_constraints {
        if unique.is_primary_key || unique.columns.is_empty() {
            continue;
        }
        if rules.iter().any(|rule| same_set(&rule.columns, &unique.columns)) {
            continue;
        }
        rules.push(BranchUniqueRule {
            index_name: format!("{table}_{}_key", unique.columns.join("_")),
            label: unique.name.clone(),
            columns: unique.columns.clone(),
            is_primary_key: false,
        });
    }
    rules
}

// ---------------------------------------------------------------------------
// The fork record
// ---------------------------------------------------------------------------

const FORK_MAGIC: &[u8; 4] = b"BMF1";
const FORK_FORMAT_VERSION: u8 = 1;

/// What a branch's fork record holds for one table of its parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkEntry {
    /// The table's incarnation id; `None` while it has none (a backfill that
    /// failed), which compares as unknown, never as different.
    pub incarnation: Option<u64>,
    /// The fingerprint of its definition ([`definition_fingerprint`]).
    pub fingerprint: u64,
    /// Its columns, for a refusal to say what changed.
    pub columns: Vec<String>,
}

/// Do two incarnation ids say the table is a different one? `None` is unknown.
pub(crate) fn incarnations_differ(left: Option<u64>, right: Option<u64>) -> bool {
    matches!((left, right), (Some(left), Some(right)) if left != right)
}

/// A definition's incarnation as a fact: `0` is "main's table has no id yet".
pub(crate) fn known_incarnation(incarnation: u64) -> Option<u64> {
    (incarnation != 0).then_some(incarnation)
}

/// The stored form (unsealed) of a branch's fork record.
pub(crate) fn encode_fork_record(entries: &BTreeMap<String, ForkEntry>) -> Result<Vec<u8>> {
    let body = bincode::serialize(entries)
        .map_err(|e| Error::storage(format!("Failed to serialize a branch fork record: {e}")))?;
    let mut out = Vec::with_capacity(FORK_MAGIC.len() + 1 + body.len());
    out.extend_from_slice(FORK_MAGIC);
    out.push(FORK_FORMAT_VERSION);
    out.extend_from_slice(&body);
    Ok(out)
}

fn decode_fork_record(bytes: &[u8]) -> Result<BTreeMap<String, ForkEntry>> {
    let body = bytes
        .strip_prefix(FORK_MAGIC.as_slice())
        .and_then(<[u8]>::split_first)
        .filter(|(version, _)| **version == FORK_FORMAT_VERSION)
        .map(|(_, body)| body)
        .ok_or_else(|| Error::storage("a branch fork record has an unknown format"))?;
    bincode::deserialize(body).map_err(|e| Error::storage(format!("a branch fork record does not decode: {e}")))
}

// ---------------------------------------------------------------------------
// MERGE BRANCH's durable intent
// ---------------------------------------------------------------------------

const INTENT_MAGIC: &[u8; 4] = b"BMI1";
const INTENT_FORMAT_VERSION: u8 = 1;

/// sprinter 6bc4a4040eb7 (B3 fix pass): an unfinished MERGE BRANCH — written,
/// synced, before the source is marked merged and before the target changes;
/// deleted when the merge is done. Rolled FORWARD: rerunning the MERGE, or the
/// next open, finishes it. Rolling back would need the pre-image of every table
/// the merge dropped and of every row an `ADD COLUMN` rewrote in place, and the
/// engine keeps neither; the source's rows and the steps below stay on disk
/// until the merge is finished.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MergeIntent {
    pub(crate) source_id: u64,
    pub(crate) source_name: String,
    pub(crate) target_id: u64,
    pub(crate) target_name: String,
    pub(crate) merge_to_main: bool,
    /// The target's chain; empty for main.
    pub(crate) target_chain: Vec<u64>,
    /// When the source became merged.
    pub(crate) merge_timestamp: u64,
    /// The schema changes, in table order.
    pub(crate) steps: Vec<MergeStep>,
    /// Every table the merge changes on the target, rows-only ones included:
    /// the target's DDL on them is refused until the merge is finished.
    pub(crate) tables: Vec<String>,
    /// Of those, the ones a branch target resolves to main's catalog: main's DDL
    /// on them would change what the merge validated, so it is refused too.
    pub(crate) main_tables: Vec<String>,
    /// Into main: the tables whose rows have landed. Written in the same commit as
    /// those rows, so a rerun leaves them alone: main may have changed them since,
    /// and those changes are not the merge's to undo.
    pub(crate) landed: Vec<String>,
}

/// One schema change of a merge. Every step is idempotent: applying it checks
/// what the target holds now against what the plan saw.
///
/// The incarnation was compared by the plan, under the merge's exclusion, and
/// is not compared again here: the target's DDL has been held off the table ever
/// since, so a different id at a resume can only be a WAL replay at open minting
/// one for the same table (a DROP + CREATE inside the checkpoint window). That
/// must not leave the merge unfinishable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MergeStep {
    pub(crate) table: String,
    /// The target's definition fingerprint the step applies to.
    pub(crate) before: Option<u64>,
    pub(crate) action: MergeAction,
}

/// What one step does.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum MergeAction {
    /// Into main: the branch dropped the table.
    DropOnMain,
    /// Into main: the branch created the table; it replaces main's of the name
    /// and keeps the definition's incarnation, which is also how a resumed
    /// merge recognizes a create it already ran.
    CreateOnMain { definition: BranchTableDefinition },
    /// Into main: the branch added columns over `base_columns`.
    AddColumnsOnMain {
        base_columns: Vec<String>,
        columns: Vec<BranchAddedColumn>,
    },
    /// Into a branch: the target level takes `record`, computed at plan time.
    /// With `materialize`, the target's view is first rewritten under it.
    PutOnBranch {
        record: BranchTableRecord,
        materialize: Option<Vec<BranchAddedColumn>>,
    },
}

impl MergeIntent {
    /// The merge's tables whose rows are still to land.
    pub(crate) fn tables_left(&self) -> Vec<String> {
        self.tables
            .iter()
            .filter(|table| !self.landed.contains(table))
            .cloned()
            .collect()
    }

    fn encode(&self) -> Result<Vec<u8>> {
        let body =
            bincode::serialize(self).map_err(|e| Error::storage(format!("Failed to serialize a merge intent: {e}")))?;
        let mut out = Vec::with_capacity(INTENT_MAGIC.len() + 1 + body.len());
        out.extend_from_slice(INTENT_MAGIC);
        out.push(INTENT_FORMAT_VERSION);
        out.extend_from_slice(&body);
        Ok(out)
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self> {
        let body = bytes
            .strip_prefix(INTENT_MAGIC.as_slice())
            .and_then(<[u8]>::split_first)
            .filter(|(version, _)| **version == INTENT_FORMAT_VERSION)
            .map(|(_, body)| body)
            .ok_or_else(|| Error::storage("a MERGE BRANCH intent has an unknown format"))?;
        bincode::deserialize(body).map_err(|e| Error::storage(format!("a MERGE BRANCH intent does not decode: {e}")))
    }

    /// The level the merge's exclusion is keyed by.
    pub(crate) fn exclusion_level(&self) -> u64 {
        if self.merge_to_main {
            MERGE_TARGET_MAIN
        } else {
            self.target_id
        }
    }
}

/// The exclusion level of main (main's registry id is a branch id like any
/// other; `0` is never one).
pub(crate) const MERGE_TARGET_MAIN: u64 = 0;

/// One merge holding a table of a level off DDL.
#[derive(Debug, Clone)]
struct MergeHolder {
    source_id: u64,
    source: String,
    target: String,
    /// The merge changes the table on this level; otherwise it only depends on
    /// it (a branch target that inherits main's table), and any number of those
    /// can share it.
    writes: bool,
}

/// `(engine instance, level, table)` → the merges holding it.
static MERGE_EXCLUSIONS: LazyLock<DashMap<RecordKey, Vec<MergeHolder>>> = LazyLock::new(DashMap::new);

/// How many entries [`MERGE_EXCLUSIONS`] holds, over every engine — the one
/// relaxed load a schema plan on main pays.
static MERGE_EXCLUSION_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Where `merge_branch` can be made to fail or be stopped at, for the
/// crash-consistency tests.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MergeFailSite {
    /// The intent is durable, and the source is not marked merged yet.
    AfterIntent,
    /// A schema step (and between an `ADD COLUMN`'s schema update and backfill).
    Step,
    /// Into main: a table's rows are about to land. Into a branch: one row.
    Row,
    /// Into main: a table's keys are claimed, and its rows are not committed yet.
    TableClaimed,
    /// One chunk of a materialization written.
    Materialize,
    /// A schema statement on main, holding its tables and past its exclusion check.
    MainDdlStatement,
    /// Phase B4: one batch of a level's reclaim deleted.
    Reclaim,
    /// Phase B4: one write of a table's version history purge deleted.
    HistoryPurge,
}

/// A test's code to run at a [`MergeFailSite`], with no lock held.
#[doc(hidden)]
pub type MergeSiteHook = Arc<dyn Fn() + Send + Sync>;

/// Injected failures of `merge_branch`, per engine: fail at the n-th tick of
/// that site.
static MERGE_FAILPOINTS: LazyLock<DashMap<u64, (MergeFailSite, usize)>> = LazyLock::new(DashMap::new);

/// Test code to run at each tick of a site, per engine.
static MERGE_SITE_HOOKS: LazyLock<DashMap<(u64, MergeFailSite), MergeSiteHook>> = LazyLock::new(DashMap::new);

/// Whether any fail point or hook is armed: the one load a merged row pays.
static MERGE_FAILPOINTS_ARMED: AtomicUsize = AtomicUsize::new(0);

/// One tick of a fail point of engine `engine` (`StorageEngine::debug_fail_merge_at`,
/// `debug_on_merge_site`): one load when none is armed anywhere.
pub(crate) fn engine_failpoint_tick(engine: u64, site: MergeFailSite) -> Result<()> {
    if MERGE_FAILPOINTS_ARMED.load(Ordering::Relaxed) == 0 {
        return Ok(());
    }
    let hook = MERGE_SITE_HOOKS
        .get(&(engine, site))
        .map(|hook| Arc::clone(hook.value()));
    if let Some(hook) = hook {
        hook();
    }
    let fire = match MERGE_FAILPOINTS.get_mut(&engine) {
        Some(mut armed) if armed.0 == site => {
            if armed.1 > 0 {
                armed.1 -= 1;
                false
            } else {
                true
            }
        }
        _ => false,
    };
    if fire {
        MERGE_FAILPOINTS.remove(&engine);
        MERGE_FAILPOINTS_ARMED.fetch_sub(1, Ordering::Relaxed);
        return Err(Error::storage("injected MERGE BRANCH failure"));
    }
    Ok(())
}

/// A schema statement on main, or one write of main's catalog: it holds its tables
/// exclusively until this drops (spec R5.1, `storage::table_locks`). A merge takes the
/// same locks, so it waits for a statement already running and a statement waits for
/// the merge. Inert inside a merge's own scope, whose locks cover it.
///
/// Phase B4: it also holds the statement's place in the fork fence, taken BEFORE the
/// table locks and given back after them (`storage::branch_freeze`, "Lock order").
#[must_use]
pub(crate) struct MainDdlGuard {
    // In this order: the locks let go while the statement still acts for its transaction.
    _locks: Option<super::table_locks::ExclusiveTableLocks>,
    _bound: Option<super::table_locks::BoundLockOwner>,
    /// Phase B4: the fork fence this guard entered, and [`FORK_FENCE_ENTERED`] as it
    /// was. See [`StorageEngine::enter_fork_fence`].
    fork_fence: Option<(Arc<super::branch_freeze::BranchFreeze>, Option<u64>)>,
}

impl MainDdlGuard {
    fn inert() -> Self {
        Self {
            _locks: None,
            _bound: None,
            fork_fence: None,
        }
    }

    /// Locks taken by a route that binds the statement to its transaction itself.
    pub(crate) fn holding(
        locks: super::table_locks::ExclusiveTableLocks,
        bound: super::table_locks::BoundLockOwner,
    ) -> Self {
        Self {
            _locks: Some(locks),
            _bound: Some(bound),
            fork_fence: None,
        }
    }

    /// The statement stays bound to its transaction for this guard's life.
    pub(crate) fn bound_to(mut self, bound: super::table_locks::BoundLockOwner) -> Self {
        self._bound = Some(bound);
        self
    }
}

impl Drop for MainDdlGuard {
    fn drop(&mut self) {
        // The table locks first, then the fence: the reverse of how they were taken.
        drop(self._locks.take());
        drop(self._bound.take());
        if let Some((freeze, previous)) = self.fork_fence.take() {
            FORK_FENCE_ENTERED.with(|entered| entered.set(previous));
            freeze.leave_schema_statement();
        }
    }
}

thread_local! {
    /// The engine whose fork fence a guard on this thread entered (phase B4): what
    /// runs under that statement is part of it, and must not wait for a fork that is
    /// waiting for the statement.
    static FORK_FENCE_ENTERED: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

thread_local! {
    /// The engine whose MERGE BRANCH is running on this thread: its own catalog
    /// writes are not refused as DDL on its own tables. Statement execution stays
    /// on its thread, which is what makes a thread-local the right home.
    static MERGE_SCOPE: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// A MERGE BRANCH running on this thread (see [`MERGE_SCOPE`]). Restores what it
/// found (the open-time finish, and a merge nested under nothing else, find none).
#[must_use]
pub(crate) struct MergeDdlScope {
    previous: Option<u64>,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl MergeDdlScope {
    pub(crate) fn enter(engine: u64) -> Self {
        Self {
            previous: MERGE_SCOPE.with(|scope| scope.replace(Some(engine))),
            _not_send: std::marker::PhantomData,
        }
    }
}

impl Drop for MergeDdlScope {
    fn drop(&mut self) {
        MERGE_SCOPE.with(|scope| scope.set(self.previous));
    }
}

/// The live trigger registry of each open database, for a MERGE's drop.
static LIVE_TRIGGER_REGISTRIES: LazyLock<DashMap<u64, std::sync::Weak<crate::sql::TriggerRegistry>>> =
    LazyLock::new(DashMap::new);

/// See `StorageEngine::ancestor_merge_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AncestorMerge {
    /// Its definitions are on the target: carried.
    IntoTheTarget,
    /// Active, or merged somewhere else: it has to be merged first.
    NotIntoTheTarget,
    /// Merged into a branch that has since been dropped: it never can be.
    IntoADroppedBranch,
}

/// Is any merge excluding DDL on some table of some engine?
pub(crate) fn merge_exclusions_exist() -> bool {
    MERGE_EXCLUSION_COUNT.load(Ordering::Relaxed) > 0
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// How a chain resolves one table name.
#[derive(Debug, Clone)]
pub enum BranchTableResolution {
    /// No level of the chain has an entry: main's catalog decides.
    Inherited,
    /// `chain[index]` (= `level`) defines the table.
    Defined {
        /// Position of the deciding level in the chain.
        index: usize,
        /// The deciding level.
        level: u64,
        /// Its record (`state` is `Defined`).
        record: Arc<BranchTableRecord>,
    },
    /// `chain[index]` (= `level`) dropped the table: it does not exist.
    Dropped {
        /// Position of the deciding level in the chain.
        index: usize,
        /// The deciding level.
        level: u64,
        /// Its record (`state` is `Dropped`).
        record: Arc<BranchTableRecord>,
    },
}

impl BranchTableResolution {
    fn from_record(index: usize, level: u64, record: Arc<BranchTableRecord>) -> Self {
        if record.definition().is_some() {
            Self::Defined { index, level, record }
        } else {
            Self::Dropped { index, level, record }
        }
    }

    /// The branch's definition of the table, when a level defines it.
    pub fn definition(&self) -> Option<&BranchTableDefinition> {
        match self {
            Self::Defined { record, .. } => record.definition(),
            _ => None,
        }
    }

    /// `Some(exists)` when a level decides, `None` when main does.
    pub fn exists(&self) -> Option<bool> {
        match self {
            Self::Inherited => None,
            Self::Defined { .. } => Some(true),
            Self::Dropped { .. } => Some(false),
        }
    }

    /// The deciding level, unless main decides.
    pub fn level(&self) -> Option<u64> {
        match self {
            Self::Inherited => None,
            Self::Defined { level, .. } | Self::Dropped { level, .. } => Some(*level),
        }
    }
}

/// `(engine instance, level, table)`.
type RecordKey = (u64, u64, String);

/// Upper bound on cached (level, table) records; past it the cache is
/// emptied (every entry is re-read on its next use).
const RECORD_CACHE_MAX_ENTRIES: usize = 65_536;

/// sprinter 6bc4a4040eb7 (phase B3): each level's record for a table (or its
/// absence), stamped with the engine's schema generation at the read.
static RECORDS: LazyLock<DashMap<RecordKey, (u64, Option<Arc<BranchTableRecord>>)>> = LazyLock::new(DashMap::new);

/// Phase B4 with spec R5.2: each level's frozen missing values for a table
/// ([`branch_missing_values_key`]), under the frozen definition's schema, stamped
/// like [`RECORDS`].
static FROZEN_MISSING: LazyLock<DashMap<RecordKey, (u64, Option<Arc<super::row_shape::MissingValues>>)>> =
    LazyLock::new(DashMap::new);

/// `(engine instance, branch, table)` whose isolated table this process has
/// already raised the shared row-id counter for (see
/// `StorageEngine::ensure_branch_row_id_floor`).
static ROW_ID_FLOORS: LazyLock<DashMap<RecordKey, ()>> = LazyLock::new(DashMap::new);

impl StorageEngine {
    fn branch_keys_with_prefix(&self, prefix: &[u8], mut each: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
        let iter = self.db.iterator_opt(
            IteratorMode::From(prefix, Direction::Forward),
            super::time_travel::bounded_read_options(prefix),
        );
        for item in iter {
            let (key, _) = item.map_err(|e| Error::storage(format!("Iterator error: {e}")))?;
            if !key.starts_with(prefix) {
                break;
            }
            each(&key)?;
        }
        Ok(())
    }

    /// sprinter 6bc4a4040eb7 (phase B3): level `level`'s OWN record for
    /// `table` (`None`: the level has no entry for it).
    pub(crate) fn branch_table_record(&self, level: u64, table: &str) -> Result<Option<Arc<BranchTableRecord>>> {
        let cacheable = !self.is_physical_standby();
        // Read the generation BEFORE the record: a write that lands between
        // the two leaves this entry stamped with the older generation, so the
        // next use re-reads it.
        let generation = self.schema_generation();
        let key: RecordKey = (self.instance_id(), level, table.to_string());
        if cacheable {
            if let Some(entry) = RECORDS.get(&key) {
                if entry.0 == generation {
                    return Ok(entry.1.clone());
                }
            }
        }
        let record = match self.get(&branch_table_key(level, table))? {
            Some(bytes) => Some(Arc::new(BranchTableRecord::decode(table, &bytes)?)),
            None => None,
        };
        if cacheable {
            if RECORDS.len() >= RECORD_CACHE_MAX_ENTRIES {
                RECORDS.clear();
            }
            RECORDS.insert(key, (generation, record.clone()));
        }
        Ok(record)
    }

    /// Phase B4 with spec R5.2: the missing values a frozen record of `table` on
    /// `level` reads its narrow rows with, by position in `schema`, the record's
    /// definition. `None` when main had none for the table when the record was taken.
    pub(crate) fn frozen_missing_values(
        &self,
        level: u64,
        table: &str,
        schema: &Schema,
    ) -> Result<Option<Arc<super::row_shape::MissingValues>>> {
        let cacheable = !self.is_physical_standby();
        let generation = self.schema_generation();
        let key: RecordKey = (self.instance_id(), level, table.to_string());
        if cacheable {
            if let Some(entry) = FROZEN_MISSING.get(&key) {
                if entry.0 == generation {
                    return Ok(entry.1.clone());
                }
            }
        }
        let loaded = match self.get(&branch_missing_values_key(level, table))? {
            Some(bytes) => {
                let by_name: Vec<(String, crate::Value)> = bincode::deserialize(&bytes).map_err(|e| {
                    Error::storage(format!(
                        "the frozen missing values of '{table}' on branch level {level} do not decode: {e}"
                    ))
                })?;
                (!by_name.is_empty()).then(|| {
                    Arc::new(super::row_shape::MissingValues::from_schema(
                        schema,
                        by_name.into_iter().collect(),
                    ))
                })
            }
            None => None,
        };
        if cacheable {
            if FROZEN_MISSING.len() >= RECORD_CACHE_MAX_ENTRIES {
                FROZEN_MISSING.clear();
            }
            FROZEN_MISSING.insert(key, (generation, loaded.clone()));
        }
        Ok(loaded)
    }

    /// sprinter 6bc4a4040eb7 (phase B3): resolve `table` through `chain`
    /// (oldest first): the NEAREST level with an entry decides.
    pub(crate) fn resolve_branch_table(&self, chain: &[u64], table: &str) -> Result<BranchTableResolution> {
        for (index, &level) in chain.iter().enumerate().rev() {
            if let Some(record) = self.branch_table_record(level, table)? {
                return Ok(BranchTableResolution::from_record(index, level, record));
            }
        }
        Ok(BranchTableResolution::Inherited)
    }

    /// The running statement's branch chain (oldest first, ending with the
    /// branch itself); `None` on main. One thread-local read on main.
    pub(crate) fn session_branch_chain(&self) -> Option<Vec<u64>> {
        if !self.is_branch_active() {
            return None;
        }
        let binding = self.active_branch()?;
        let mut chain = binding.chain.clone();
        if chain.last() != Some(&binding.branch_id) {
            chain.push(binding.branch_id);
        }
        Some(chain)
    }

    /// sprinter 6bc4a4040eb7 (phase B3): how the running statement's branch
    /// resolves `table` — `None` on main (main's catalog decides, and its
    /// caches apply). The question every catalog reader asks first.
    pub(crate) fn session_branch_table(&self, table: &str) -> Result<Option<BranchTableResolution>> {
        let Some(chain) = self.session_branch_chain() else {
            return Ok(None);
        };
        let resolution = self.resolve_branch_table(&chain, table)?;
        if let BranchTableResolution::Defined { index, record, .. } = &resolution {
            // Phase B4: a copy of main's definition too: the main table may be gone,
            // and its counter with it, while the branch still holds its rows.
            if record.isolated {
                if let Some(levels) = chain.get(*index..) {
                    self.ensure_branch_row_id_floor(levels, table)?;
                }
            } else if record.frozen {
                self.ensure_branch_row_id_floor(&chain, table)?;
            }
        }
        Ok(Some(resolution))
    }

    /// Every table some level of `chain` holds an entry for, the NEAREST
    /// level winning.
    pub(crate) fn branch_tables_in_chain(&self, chain: &[u64]) -> Result<BTreeMap<String, BranchTableResolution>> {
        let mut out: BTreeMap<String, BranchTableResolution> = BTreeMap::new();
        for (index, &level) in chain.iter().enumerate() {
            for (table, bytes) in self.meta_blobs_with_prefix(&branch_tables_prefix(level))? {
                let record = Arc::new(BranchTableRecord::decode(&table, &bytes)?);
                out.insert(table, BranchTableResolution::from_record(index, level, record));
            }
        }
        Ok(out)
    }

    /// Level `level`'s own records, by table.
    pub(crate) fn branch_table_records_at(&self, level: u64) -> Result<BTreeMap<String, BranchTableRecord>> {
        let mut out = BTreeMap::new();
        for (table, bytes) in self.meta_blobs_with_prefix(&branch_tables_prefix(level))? {
            let record = BranchTableRecord::decode(&table, &bytes)?;
            out.insert(table, record);
        }
        Ok(out)
    }

    /// Main's definition of `table` (`None`: main has no such table), with
    /// its fingerprint — read from main's catalog whatever branch the running
    /// statement is on.
    pub(crate) fn main_table_definition(&self, table: &str) -> Result<Option<(BranchTableDefinition, u64)>> {
        let catalog = Catalog::new(self);
        if !catalog.main_table_exists(table)? {
            return Ok(None);
        }
        let schema = catalog.main_table_schema(table)?;
        let constraints = catalog.main_table_constraints(table)?;
        let unique_rules: Vec<BranchUniqueRule> = self
            .art_indexes()
            .enforced_unique_indexes(table)
            .iter()
            .map(BranchUniqueRule::from_enforced)
            .collect();
        let identity_columns = catalog.main_identity_columns(table)?;
        let fingerprint = definition_fingerprint(&schema, &constraints, &unique_rules)?;
        Ok(Some((
            BranchTableDefinition {
                schema,
                constraints,
                unique_rules,
                identity_columns,
                incarnation: catalog.main_table_incarnation(table)?.unwrap_or(0),
            },
            fingerprint,
        )))
    }

    /// The definition of `table` seen through `chain` (an empty chain is
    /// main), with its fingerprint; `None` when the table does not exist
    /// there.
    pub(crate) fn chain_table_definition(
        &self,
        chain: &[u64],
        table: &str,
    ) -> Result<Option<(BranchTableDefinition, u64)>> {
        match self.resolve_branch_table(chain, table)? {
            BranchTableResolution::Inherited => self.main_table_definition(table),
            BranchTableResolution::Dropped { .. } => Ok(None),
            BranchTableResolution::Defined { record, .. } => match record.definition() {
                Some(definition) => {
                    let fingerprint =
                        definition_fingerprint(&definition.schema, &definition.constraints, &definition.unique_rules)?;
                    Ok(Some((definition.clone(), fingerprint)))
                }
                None => Ok(None),
            },
        }
    }

    /// sprinter 6bc4a4040eb7 (B3 fix pass): the incarnation of `table` seen
    /// through `chain` (an empty chain is main); `None` when the table does not
    /// exist there.
    pub(crate) fn chain_table_incarnation(&self, chain: &[u64], table: &str) -> Result<Option<u64>> {
        match self.resolve_branch_table(chain, table)? {
            BranchTableResolution::Inherited => Catalog::new(self).main_table_incarnation(table),
            BranchTableResolution::Dropped { .. } => Ok(None),
            BranchTableResolution::Defined { record, .. } => {
                Ok(record.definition().and_then(|d| known_incarnation(d.incarnation)))
            }
        }
    }

    /// The fork entry of every table `chain` sees (an empty chain is main):
    /// what a child forked from that level records.
    pub(crate) fn fork_entries(&self, chain: &[u64]) -> Result<BTreeMap<String, ForkEntry>> {
        fn entry(definition: &BranchTableDefinition, fingerprint: u64) -> ForkEntry {
            ForkEntry {
                incarnation: known_incarnation(definition.incarnation),
                fingerprint,
                columns: definition.schema.columns.iter().map(|c| c.name.clone()).collect(),
            }
        }
        let catalog = Catalog::new(self);
        let entries = self.branch_tables_in_chain(chain)?;
        let mut out = BTreeMap::new();
        for table in catalog.list_main_tables()? {
            if entries.contains_key(&table) {
                continue;
            }
            if let Some((definition, fingerprint)) = self.main_table_definition(&table)? {
                let forked = entry(&definition, fingerprint);
                out.insert(table, forked);
            }
        }
        for (table, resolution) in entries {
            if let Some(definition) = resolution.definition() {
                let fingerprint =
                    definition_fingerprint(&definition.schema, &definition.constraints, &definition.unique_rules)?;
                out.insert(table, entry(definition, fingerprint));
            }
        }
        Ok(out)
    }

    /// Level `level`'s fork record; `None` for a branch created without one.
    pub(crate) fn branch_fork_record(&self, level: u64) -> Result<Option<BTreeMap<String, ForkEntry>>> {
        match self.get(&branch_fork_key(level))? {
            Some(bytes) => Ok(Some(decode_fork_record(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Stage level `level`'s fork record into its creation batch.
    pub(crate) fn stage_branch_fork_record(
        &self,
        level: u64,
        entries: &BTreeMap<String, ForkEntry>,
        batch: &mut WriteBatch,
    ) -> Result<()> {
        let value = encode_fork_record(entries)?;
        let sealed = super::tde::seal(self.key_manager_ref(), &value)?;
        batch.put(branch_fork_key(level), sealed.as_ref());
        Ok(())
    }

    /// The unfinished merge of source `source_id`, if any.
    pub(crate) fn branch_merge_intent(&self, source_id: u64) -> Result<Option<MergeIntent>> {
        match self.get(&branch_merge_intent_key(source_id))? {
            Some(bytes) => Ok(Some(MergeIntent::decode(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Every unfinished merge of this store.
    pub(crate) fn branch_merge_intents(&self) -> Result<Vec<MergeIntent>> {
        self.meta_blobs_with_prefix(BRANCH_MERGE_INTENT_PREFIX)?
            .into_iter()
            .map(|(_, bytes)| MergeIntent::decode(&bytes))
            .collect()
    }

    /// sprinter 6bc4a4040eb7 (B3 second fix pass): an unfinished merge OTHER than
    /// `source_id`'s own that a merge of `source_id` into `target_id` would break:
    /// one that writes into `source_id` (merged away, it could never finish), or
    /// `target_id`'s own (it is on its way out). Two merges into one target may
    /// coexist; the exclusion keeps them off each other's tables.
    pub(crate) fn merge_intent_in_the_way(&self, source_id: u64, target_id: u64) -> Result<Option<MergeIntent>> {
        Ok(self.branch_merge_intents()?.into_iter().find(|intent| {
            intent.source_id != source_id && (intent.target_id == source_id || intent.source_id == target_id)
        }))
    }

    /// sprinter 6bc4a4040eb7 (B3 second fix pass): refuse (55006) `what`, a
    /// whole-database operation, while any merge is unfinished.
    pub(crate) fn refuse_while_a_merge_is_unfinished(&self, what: &str) -> Result<()> {
        match self.branch_merge_intents()?.first() {
            Some(intent) => Err(Error::branch_merge_pending_refuses(
                what,
                &intent.source_name,
                &intent.target_name,
            )),
            None => Ok(()),
        }
    }

    pub(crate) fn branch_merge_fk_policy(&self, source: u64) -> Result<MergeFkPolicy> {
        match self.get(&branch_merge_fk_policy_key(source))?.as_deref() {
            None | Some([1, 0]) => Ok(MergeFkPolicy::Enforced),
            Some([1, 1]) => Ok(MergeFkPolicy::Audit),
            Some([1, 2]) => Ok(MergeFkPolicy::Bypass),
            Some(_) => Err(Error::storage("MERGE BRANCH FK policy has an unknown version or value")),
        }
    }

    /// Write the intent and its policy in ONE synced group. Startup and explicit
    /// resume use the stored policy, even if this handle defaults to Enforced.
    pub(crate) fn put_branch_merge_intent_with_fk_policy(
        &self,
        intent: &MergeIntent,
        policy: MergeFkPolicy,
    ) -> Result<()> {
        let value = intent.encode()?;
        let sealed = super::tde::seal(self.key_manager_ref(), &value)?;
        let mut batch = WriteBatch::default();
        batch.put(branch_merge_intent_key(intent.source_id), sealed.as_ref());
        let encoded = [
            1,
            match policy {
                MergeFkPolicy::Enforced => 0,
                MergeFkPolicy::Audit => 1,
                MergeFkPolicy::Bypass => 2,
            },
        ];
        let policy_value = super::tde::seal(self.key_manager_ref(), &encoded)?;
        batch.put(branch_merge_fk_policy_key(intent.source_id), policy_value.as_ref());
        self.write_batch_through_gate(batch)?;
        self.durable_autocommit_barrier()
    }

    pub(crate) fn put_branch_merge_intent(&self, intent: &MergeIntent) -> Result<()> {
        self.put_branch_merge_intent_with_fk_policy(intent, MergeFkPolicy::Enforced)
    }

    /// `intent`, with a table's rows: in `txn`, so the two are one write.
    pub(crate) fn stage_branch_merge_intent(&self, intent: &MergeIntent, txn: &super::Transaction) -> Result<()> {
        let value = intent.encode()?;
        // The commit batch seals row keys only; this is the form the store holds.
        let sealed = super::tde::seal(self.key_manager_ref(), &value)?;
        txn.put(branch_merge_intent_key(intent.source_id), sealed.into_owned())
    }

    /// The merge of `source_id` is done (or never began): its intent goes.
    pub(crate) fn delete_branch_merge_intent(&self, source_id: u64) -> Result<()> {
        let mut batch = WriteBatch::default();
        batch.delete(branch_merge_intent_key(source_id));
        batch.delete(branch_merge_fk_policy_key(source_id));
        self.write_batch_through_gate(batch)?;
        self.durable_autocommit_barrier()
    }

    /// Hold `level`'s DDL off `tables` for the merge of `source_id`: `writes`
    /// when the merge changes them on that level, otherwise it only depends on
    /// them. Refused (55006) when another merge holds one of them, unless both
    /// only depend on it; the caller then changed nothing.
    pub(crate) fn register_merge_exclusion(
        &self,
        level: u64,
        source_id: u64,
        source_name: &str,
        target_name: &str,
        tables: &[String],
        writes: bool,
    ) -> Result<()> {
        let engine = self.instance_id();
        for table in tables {
            let key: RecordKey = (engine, level, table.clone());
            let mut holders = MERGE_EXCLUSIONS.entry(key).or_insert_with(|| {
                MERGE_EXCLUSION_COUNT.fetch_add(1, Ordering::Relaxed);
                Vec::new()
            });
            let conflict = holders
                .iter()
                .find(|holder| holder.source_id != source_id && (holder.writes || writes))
                .cloned();
            if let Some(holder) = conflict {
                drop(holders);
                self.release_merge_exclusion(source_id);
                return Err(Error::branch_merge_table_busy(table, &holder.source, &holder.target));
            }
            if !holders.iter().any(|holder| holder.source_id == source_id) {
                holders.push(MergeHolder {
                    source_id,
                    source: source_name.to_string(),
                    target: target_name.to_string(),
                    writes,
                });
            }
        }
        Ok(())
    }

    /// Hold the DDL an unfinished merge is exposed to: its target level's, and
    /// main's on what a branch target inherits.
    pub(crate) fn register_merge_intent_exclusion(&self, intent: &MergeIntent) -> Result<()> {
        self.register_merge_exclusion(
            intent.exclusion_level(),
            intent.source_id,
            &intent.source_name,
            &intent.target_name,
            &intent.tables,
            true,
        )?;
        let dependencies = self.merge_fk_dependency_tables(
            intent.source_id,
            &intent.target_chain,
            &intent.steps,
            &intent.tables,
            self.branch_merge_fk_policy(intent.source_id)?,
        )?;
        self.register_merge_exclusion(
            intent.exclusion_level(),
            intent.source_id,
            &intent.source_name,
            &intent.target_name,
            &dependencies,
            false,
        )?;
        let mut inherited = self.merge_fk_inherited_tables(&intent.target_chain, &dependencies)?;
        inherited.extend(intent.main_tables.iter().cloned());
        inherited.sort_unstable();
        inherited.dedup();
        self.register_merge_exclusion(
            MERGE_TARGET_MAIN,
            intent.source_id,
            &intent.source_name,
            &intent.target_name,
            &inherited,
            false,
        )
    }

    /// The merge of `source_id` is finished, or never began.
    pub(crate) fn release_merge_exclusion(&self, source_id: u64) {
        let engine = self.instance_id();
        MERGE_EXCLUSIONS.retain(|(owner, _, _), holders| {
            if *owner == engine {
                holders.retain(|holder| holder.source_id != source_id);
            }
            let keep = !holders.is_empty();
            if !keep {
                MERGE_EXCLUSION_COUNT.fetch_sub(1, Ordering::Relaxed);
            }
            keep
        });
    }

    /// Refuse (55006) DDL on `table` of `level` while a merge holds it, unless
    /// this IS that merge, running on this thread. A merge with a durable intent
    /// has something to finish or abandon; one still validating has neither, and
    /// is only something to wait for.
    pub(crate) fn refuse_ddl_on_merging_table(&self, level: u64, table: &str) -> Result<()> {
        if !merge_exclusions_exist() || self.in_merge_scope() {
            return Ok(());
        }
        let key: RecordKey = (self.instance_id(), level, table.to_string());
        let holder = MERGE_EXCLUSIONS
            .get(&key)
            .and_then(|entry| entry.value().first().cloned());
        let Some(holder) = holder else {
            return Ok(());
        };
        if self.branch_merge_intent(holder.source_id)?.is_some() {
            Err(Error::branch_merge_pending(
                &format!("table {table}"),
                &holder.source,
                &holder.target,
            ))
        } else {
            Err(Error::branch_merge_in_progress(table, &holder.source, &holder.target))
        }
    }

    fn in_merge_scope(&self) -> bool {
        MERGE_SCOPE.with(std::cell::Cell::get) == Some(self.instance_id())
    }

    /// Phase B4: a schema statement on main, or a write of main's catalog, on `tables`
    /// starts. It runs wholly before a fork of main or wholly after it ("Definitions" in
    /// `storage::branch_freeze`): it waits while a branch of main registers, and a new
    /// branch waits for it. Before anything looks for children, and BEFORE the
    /// statement's own table locks, so a fork waiting for the statements in the fence
    /// never waits, through one of them, for a lock held by a statement that waits for
    /// the fork (see below for one whose transaction holds locks already).
    /// Once per thread: what nests under a statement is that statement. The wait has a
    /// table lock wait's budget: 55P03 after `lock_timeout`, 57014 at `statement_timeout`
    /// or a cancel, with nothing done. A statement whose transaction holds writer locks
    /// already (its earlier DDL, or a write) does not wait for a fork that is waiting for
    /// the running statements, one of which may be waiting for those locks: it runs, and
    /// the fork waits for it too (`BranchFreeze::enter_schema_statement`). Nothing is
    /// refused at once, so a migration's later DDL is not refused after its first landed.
    fn enter_fork_fence(&self, tables: &[&str], guard: &mut MainDdlGuard) -> Result<()> {
        if tables.iter().all(|table| crate::sql::temp_tables::is_temp_key(table)) {
            return Ok(());
        }
        self.take_fork_fence(tables.first().copied().unwrap_or("?"), guard)
    }

    /// Phase B4: a statement that creates tables it does not name up front (`CREATE
    /// SCHEMA` with elements): the fork fence for all of what it runs. Nothing else of
    /// a schema statement: each element is a statement, with its own guard.
    pub(crate) fn enter_main_statement_of_unnamed_tables(&self) -> Result<MainDdlGuard> {
        let mut guard = MainDdlGuard::inert();
        self.take_fork_fence("?", &mut guard)?;
        Ok(guard)
    }

    fn take_fork_fence(&self, named: &str, guard: &mut MainDdlGuard) -> Result<()> {
        let Some(freeze) = self.branch_freeze() else {
            return Ok(());
        };
        let engine = self.instance_id();
        if FORK_FENCE_ENTERED.with(std::cell::Cell::get) == Some(engine) {
            return Ok(());
        }
        let holds_locks = self.table_locks().current_statement_holds_any();
        if !freeze.enter_schema_statement(std::time::Duration::ZERO, holds_locks) {
            self.table_locks().wait_like_a_lock(
                named,
                "a CREATE BRANCH of main is registering",
                |longest| freeze.enter_schema_statement(longest, holds_locks),
                || freeze.leave_schema_statement(),
            )?;
        }
        let previous = FORK_FENCE_ENTERED.with(|entered| entered.replace(Some(engine)));
        guard.fork_fence = Some((Arc::clone(freeze), previous));
        Ok(())
    }

    /// sprinter 6bc4a4040eb7 (B3 third fix pass, spec R5.1): a schema statement on
    /// main starts, on `tables`. Phase B4: it enters the fork fence first. Refused
    /// (55006) if a merge holds one of them, or an unfinished merge's intent does (the
    /// exclusion outlives a crash, when no lock is held by anybody); then it takes them
    /// exclusively, sorted, bounded by `[session] lock_timeout_ms`; and it looks again,
    /// for a merge that registered meanwhile and did not finish. Past this it is not
    /// refused again: a merge that registers now waits for this statement's lock, and a
    /// refusal half way would tear what it did. Inert inside a merge's own scope, but for
    /// the freeze of main's definitions for main's other live branches (phase B4).
    pub(crate) fn enter_main_statement(&self, tables: &[&str], statement: &str) -> Result<MainDdlGuard> {
        if self.in_merge_scope() {
            // Phase B4: a merge's DDL changes main's definition under main's other
            // live branches like any other.
            self.freeze_definitions_for_children(tables)?;
            return Ok(MainDdlGuard::inert());
        }
        let mut guard = MainDdlGuard::inert();
        self.enter_fork_fence(tables, &mut guard)?;
        // A statement nested in another one (a `CREATE TABLE … AS`, on a query route) was
        // looked at when it took what it holds.
        let to_check: Vec<&str> = tables
            .iter()
            .copied()
            .filter(|table| {
                !self
                    .table_locks()
                    .current_statement_holds_exclusively(super::table_locks::MAIN_LEVEL, table)
            })
            .collect();
        for table in &to_check {
            self.refuse_ddl_on_merging_table(MERGE_TARGET_MAIN, table)?;
        }
        guard._locks = Some(self.lock_tables_exclusive(super::table_locks::MAIN_LEVEL, tables, statement)?);
        for table in &to_check {
            self.refuse_ddl_on_merging_table(MERGE_TARGET_MAIN, table)?;
        }
        // Phase B4, in the fence and under the locks: a child registered by now gets the
        // definition before the statement's first row; none can register until the
        // statement is over. A statement can rewrite rows before its first catalog
        // write, and the funnels below it find the copy already there.
        self.freeze_definitions_for_children(tables)?;
        self.merge_failpoint_tick(MergeFailSite::MainDdlStatement)?;
        Ok(guard)
    }

    /// sprinter 6bc4a4040eb7 (B3 second and third fix passes, spec R5.1): one catalog
    /// write on main touching `tables`. What the statement above it already holds it
    /// holds and looked at; a table it takes itself (a direct catalog call, restore,
    /// the protocol adapter, or a table no statement named) is checked here, and
    /// refused (55006) if a merge holds it. Inert inside a merge's own scope. Phase B4:
    /// the fork fence first, as for a statement, then the freeze of main's definitions.
    pub(crate) fn enter_main_ddl(&self, tables: &[&str]) -> Result<MainDdlGuard> {
        if self.in_merge_scope() {
            self.freeze_definitions_for_children(tables)?;
            return Ok(MainDdlGuard::inert());
        }
        let mut guard = MainDdlGuard::inert();
        self.enter_fork_fence(tables, &mut guard)?;
        let mut sorted: Vec<&str> = tables.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        let timeout = self.table_locks().timeout();
        let mut locks = self.table_locks().exclusive_guard();
        for table in sorted {
            if locks.take(super::table_locks::MAIN_LEVEL, table, "a catalog write", timeout)? {
                self.refuse_ddl_on_merging_table(MERGE_TARGET_MAIN, table)?;
            }
        }
        guard._locks = Some(locks);
        // Phase B4: for a route with no statement guard this is the first anyone
        // hears of it; under one it finds its copies written.
        self.freeze_definitions_for_children(tables)?;
        Ok(guard)
    }

    /// For the tests' handshakes: is a merge holding `table` on main?
    #[doc(hidden)]
    pub fn debug_merge_holds_table(&self, table: &str) -> bool {
        MERGE_EXCLUSIONS.contains_key(&(self.instance_id(), MERGE_TARGET_MAIN, table.to_string()))
    }

    /// The backstop in MAIN's catalog mutators: refuse (0A000) a branch session
    /// (`refuse_main_catalog_write_on_branch`), and count the write, refusing
    /// (55006) a table a merge holds unless a statement guard already checked
    /// ([`Self::enter_main_ddl`]). The guard lives as long as the write.
    pub(crate) fn main_catalog_write(&self, tables: &[&str], statement: &str) -> Result<MainDdlGuard> {
        self.refuse_main_catalog_write_on_branch(statement)?;
        self.enter_main_ddl(tables)
    }

    /// The EmbeddedDatabase's live trigger registry, which a MERGE's drop must
    /// deregister a table from, as main's own DROP TABLE does
    /// (`EmbeddedDatabase::on_table_dropped`).
    pub(crate) fn attach_live_trigger_registry(&self, registry: &Arc<crate::sql::TriggerRegistry>) {
        // An attach happens once per open: the moment to forget closed databases.
        LIVE_TRIGGER_REGISTRIES.retain(|_, weak| weak.strong_count() > 0);
        LIVE_TRIGGER_REGISTRIES.insert(self.instance_id(), Arc::downgrade(registry));
    }

    /// Spec R5.3 (review F8): `old` was renamed to `new`: its triggers follow it in
    /// the live registry of this engine's database, as their records did in the
    /// rename's write group. Nothing when no registry is attached (an open's WAL
    /// replay: the registry is loaded from the records afterwards).
    pub(crate) fn rename_live_table_triggers(&self, old: &str, new: &str) {
        let registry = LIVE_TRIGGER_REGISTRIES
            .get(&self.instance_id())
            .and_then(|registry| registry.upgrade());
        if let Some(registry) = registry {
            if let Err(e) = registry.rename_table_triggers(old, new) {
                tracing::warn!(
                    "RENAME TABLE '{}' -> '{}': failed to move the live triggers: {}",
                    old,
                    new,
                    e
                );
            }
        }
    }

    /// `table` was dropped by a merge: its triggers leave the live registry.
    pub(crate) fn deregister_dropped_table_triggers(&self, table: &str) {
        let registry = LIVE_TRIGGER_REGISTRIES
            .get(&self.instance_id())
            .and_then(|registry| registry.upgrade());
        if let Some(registry) = registry {
            if let Err(e) = registry.drop_table_triggers(table) {
                tracing::warn!(
                    "MERGE BRANCH: dropping '{}': failed to deregister triggers: {}",
                    table,
                    e
                );
            }
        }
    }

    /// sprinter 6bc4a4040eb7 (B3 fix pass): fail the MERGE at the n-th tick of
    /// `site` on this engine (`0`: the first tick), once. For the
    /// crash-consistency tests.
    #[doc(hidden)]
    pub fn debug_fail_merge_at(&self, site: MergeFailSite, after: usize) {
        if MERGE_FAILPOINTS.insert(self.instance_id(), (site, after)).is_none() {
            MERGE_FAILPOINTS_ARMED.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Run `hook` at every tick of `site` on this engine, with no lock held
    /// (`None` removes it). For the tests' deterministic handshakes, and for a
    /// child process to die at a named point.
    #[doc(hidden)]
    pub fn debug_on_merge_site(&self, site: MergeFailSite, hook: Option<MergeSiteHook>) {
        let key = (self.instance_id(), site);
        let had = match hook {
            Some(hook) => MERGE_SITE_HOOKS.insert(key, hook).is_some(),
            None => MERGE_SITE_HOOKS.remove(&key).is_some(),
        };
        let has = MERGE_SITE_HOOKS.contains_key(&key);
        if has && !had {
            MERGE_FAILPOINTS_ARMED.fetch_add(1, Ordering::Relaxed);
        } else if had && !has {
            MERGE_FAILPOINTS_ARMED.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Disarm [`Self::debug_fail_merge_at`].
    #[doc(hidden)]
    pub fn debug_clear_merge_failpoint(&self) {
        if MERGE_FAILPOINTS.remove(&self.instance_id()).is_some() {
            MERGE_FAILPOINTS_ARMED.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// A smaller chunk for materializations in this process (`0`: the default),
    /// so a test reaches a chunk boundary with a few rows.
    #[doc(hidden)]
    pub fn debug_set_materialize_chunk_rows(&self, rows: u64) {
        super::branch_view::set_materialize_chunk_rows(rows);
    }

    /// [`Self::debug_fail_merge_at`] the schema steps.
    #[doc(hidden)]
    pub fn debug_fail_merge_after_steps(&self, steps: usize) {
        self.debug_fail_merge_at(MergeFailSite::Step, steps);
    }

    /// One tick of the fail point above.
    pub(crate) fn merge_failpoint_tick(&self, site: MergeFailSite) -> Result<()> {
        engine_failpoint_tick(self.instance_id(), site)
    }

    fn row_ids_with_prefix(&self, prefix: &str) -> Result<HashSet<u64>> {
        let prefix_len = prefix.len();
        let mut ids = HashSet::new();
        self.branch_keys_with_prefix(prefix.as_bytes(), |key| {
            if let Some(row_id) = Self::parse_row_id_after_prefix(key, prefix_len) {
                ids.insert(row_id);
            }
            Ok(())
        })?;
        Ok(ids)
    }

    /// The row ids level `level` holds an image of.
    pub(crate) fn branch_level_row_ids(&self, level: u64, table: &str) -> Result<HashSet<u64>> {
        self.row_ids_with_prefix(&format!("bdata:{level}:{table}:"))
    }

    /// The row ids level `level` holds a tombstone of.
    pub(crate) fn branch_tombstone_row_ids(&self, level: u64, table: &str) -> Result<HashSet<u64>> {
        self.row_ids_with_prefix(&format!("bdel:{level}:{table}:"))
    }

    /// The row ids whose entry on level `level` is a COPY (`bfrz:`), not a
    /// change made on the level.
    pub(crate) fn branch_frozen_row_ids(&self, level: u64, table: &str) -> Result<HashSet<u64>> {
        self.row_ids_with_prefix(&super::branch_view::frozen_prefix(level, table))
    }

    /// The rows level `level` itself CHANGED: its images and tombstones, minus
    /// its copies. What MERGE carries.
    pub(crate) fn branch_changed_row_ids(&self, level: u64, table: &str) -> Result<HashSet<u64>> {
        let frozen = self.branch_frozen_row_ids(level, table)?;
        let mut changed = self.branch_level_row_ids(level, table)?;
        changed.extend(self.branch_tombstone_row_ids(level, table)?);
        changed.retain(|row_id| !frozen.contains(row_id));
        Ok(changed)
    }

    /// sprinter 6bc4a4040eb7 (B3 second fix pass): where did level `level`, an
    /// ancestor of a merge's source, go? If it was merged, all the way, into the
    /// lineage of the merge's target, what it defined is on that target and a
    /// child of it has nothing left to wait for.
    pub(crate) fn ancestor_merge_state(
        &self,
        level: u64,
        target_id: u64,
        target_chain: &[u64],
    ) -> Result<AncestorMerge> {
        let branches = self.list_branches_for_catalog()?;
        let mut cursor = level;
        for _ in 0..=branches.len() {
            // Not listed: dropped. An ancestor itself cannot be, while it has a
            // child; what it was merged INTO can.
            let Some(branch) = branches.iter().find(|branch| branch.branch_id == cursor) else {
                return Ok(AncestorMerge::IntoADroppedBranch);
            };
            let super::BranchState::Merged { into_branch, .. } = branch.state else {
                return Ok(AncestorMerge::NotIntoTheTarget);
            };
            if into_branch == target_id || target_chain.contains(&into_branch) {
                return Ok(AncestorMerge::IntoTheTarget);
            }
            cursor = into_branch;
        }
        Ok(AncestorMerge::NotIntoTheTarget)
    }

    /// Stage the deletion of every row image and tombstone of `table` that
    /// level `level` holds itself (`bdata:{level}:{table}:*`,
    /// `bdel:{level}:{table}:*`), with their copy markers and the level's key
    /// index of the table, into `batch`. Returns how many row keys.
    pub(crate) fn stage_branch_rows_delete(&self, level: u64, table: &str, batch: &mut WriteBatch) -> Result<usize> {
        let mut count = 0usize;
        self.branch_keys_with_prefix(
            super::branch_view::key_index_table_prefix(level, table).as_bytes(),
            |key| {
                batch.delete(key);
                Ok(())
            },
        )?;
        for prefix in [
            format!("bdata:{level}:{table}:"),
            format!("bdel:{level}:{table}:"),
            super::branch_view::frozen_prefix(level, table),
        ] {
            let prefix_len = prefix.len();
            self.branch_keys_with_prefix(prefix.as_bytes(), |key| {
                // Only `{prefix}{row_id}`: a table whose name extends this one
                // past a ':' (`t:x`) is not this table's row.
                if Self::parse_row_id_after_prefix(key, prefix_len).is_some() {
                    batch.delete(key);
                    count += 1;
                }
                Ok(())
            })?;
        }
        Ok(count)
    }

    /// Phase B4: forget what this process holds for a level whose keys are gone.
    pub(crate) fn forget_level_records(engine: u64, level: u64) {
        RECORDS.retain(|(owner, cached_level, _), _| !(*owner == engine && *cached_level == level));
        FROZEN_MISSING.retain(|(owner, cached_level, _), _| !(*owner == engine && *cached_level == level));
        ROW_ID_FLOORS.retain(|(owner, cached_level, _), _| !(*owner == engine && *cached_level == level));
    }

    /// Stage `tuple` as level `level`'s own image of row `row_id` of `table`
    /// into `batch` (and drop the level's own tombstone of it — one entry per
    /// row per level), serialized and sealed exactly as the branch view's own
    /// direct writes are. For a materialization (`BranchView::materialize`).
    ///
    /// sprinter 6bc4a4040eb7 (B3 fix pass): `copy` marks the image as a COPY
    /// (`bfrz:`), not a change made on the level. `!copy` leaves the row's
    /// marker as it is: an entry the level already held stays what it was.
    pub(crate) fn stage_branch_row_image(
        &self,
        batch: &mut WriteBatch,
        level: u64,
        table: &str,
        row_id: u64,
        tuple: &crate::Tuple,
        copy: bool,
    ) -> Result<()> {
        let value = bincode::serialize(tuple).map_err(|e| Error::storage(format!("Failed to serialize tuple: {e}")))?;
        let sealed = super::tde::seal(self.key_manager_ref(), &value)?;
        batch.put(
            super::branch_view::branch_row_key(level, table, row_id),
            sealed.as_ref(),
        );
        batch.delete(super::branch_view::branch_tombstone_key(level, table, row_id));
        if copy {
            batch.put(super::branch_view::branch_frozen_key(level, table, row_id), b"");
        }
        Ok(())
    }

    /// The largest row id any of `levels` holds an image or a tombstone of
    /// for `table`.
    pub(crate) fn branch_rows_max_row_id(&self, levels: &[u64], table: &str) -> Result<Option<u64>> {
        let mut max: Option<u64> = None;
        for level in levels {
            for prefix in [format!("bdata:{level}:{table}:"), format!("bdel:{level}:{table}:")] {
                let prefix_len = prefix.len();
                self.branch_keys_with_prefix(prefix.as_bytes(), |key| {
                    if let Some(row_id) = Self::parse_row_id_after_prefix(key, prefix_len) {
                        max = Some(max.map_or(row_id, |m| m.max(row_id)));
                    }
                    Ok(())
                })?;
            }
        }
        Ok(max)
    }

    /// sprinter 6bc4a4040eb7 (phase B3): the rows of an ISOLATED table carry
    /// row ids from the table NAME's shared counter (`counter:{t}`), which a
    /// later main DDL on a same-named table can reset durably (CREATE TABLE
    /// seeds it at 0, DROP TABLE deletes it) — after a restart the counter
    /// could then hand out an id the branch already holds. Once per process
    /// per (branch, table), raise the counter above every row id the isolated
    /// table's levels hold. The branch view also refuses (never overwrites) a
    /// fresh id that is already taken (`BranchView::insert`).
    pub(crate) fn ensure_branch_row_id_floor(&self, levels: &[u64], table: &str) -> Result<()> {
        let Some(&branch) = levels.last() else {
            return Ok(());
        };
        let key: RecordKey = (self.instance_id(), branch, table.to_string());
        if ROW_ID_FLOORS.contains_key(&key) {
            return Ok(());
        }
        if let Some(max) = self.branch_rows_max_row_id(levels, table)? {
            self.raise_row_counter_to(table, max);
        }
        if ROW_ID_FLOORS.len() >= RECORD_CACHE_MAX_ENTRIES {
            ROW_ID_FLOORS.clear();
        }
        ROW_ID_FLOORS.insert(key, ());
        Ok(())
    }

    /// sprinter 6bc4a4040eb7 (phase B3): write `record` as level `level`'s
    /// own entry for `table`, in ONE atomic batch with whatever `batch`
    /// already holds (an `ADD COLUMN`'s materialized rows) and — with
    /// `clear_own_rows` — the deletion of every row image and tombstone the
    /// level holds for `table` (a DROP, or a CREATE over a dropped name).
    /// Through the write gate; then the caches learn of it.
    pub(crate) fn put_branch_table_record(
        &self,
        level: u64,
        table: &str,
        record: &BranchTableRecord,
        mut batch: WriteBatch,
        clear_own_rows: bool,
    ) -> Result<()> {
        // Phase B4: a parent's copy into this level checks for a record and then
        // writes one: not between the two.
        let freeze = self.write_gate().active_freeze();
        let definition = freeze.map(|freeze| freeze.lock_definition(table));
        if clear_own_rows {
            self.stage_branch_rows_delete(level, table, &mut batch)?;
        }
        let value = record.encode()?;
        let sealed = super::tde::seal(self.key_manager_ref(), &value)?;
        batch.put(branch_table_key(level, table), sealed.as_ref());
        self.write_batch_through_gate(batch)?;
        drop(definition);
        self.after_branch_catalog_write(level, table)
    }

    /// Restore level `level`'s entry for `table` to `previous` (`None`:
    /// remove it) — the unwind of a branch DDL that failed after its first
    /// write — in one batch with, when `clear_own_rows`, the deletion of the
    /// rows the failed statement wrote at the level.
    pub(crate) fn restore_branch_table_record(
        &self,
        level: u64,
        table: &str,
        previous: Option<&BranchTableRecord>,
        clear_own_rows: bool,
    ) -> Result<()> {
        let freeze = self.write_gate().active_freeze();
        let definition = freeze.map(|freeze| freeze.lock_definition(table));
        let mut batch = WriteBatch::default();
        if clear_own_rows {
            self.stage_branch_rows_delete(level, table, &mut batch)?;
        }
        match previous {
            Some(record) => {
                let value = record.encode()?;
                let sealed = super::tde::seal(self.key_manager_ref(), &value)?;
                batch.put(branch_table_key(level, table), sealed.as_ref());
            }
            None => batch.delete(branch_table_key(level, table)),
        }
        self.write_batch_through_gate(batch)?;
        drop(definition);
        self.after_branch_catalog_write(level, table)
    }

    /// sprinter 6bc4a4040eb7 (phase B3): the backstop in MAIN's catalog
    /// mutators (`Catalog::create_table`, `update_table_schema`, the column
    /// rename, `drop_table`, `rename_table`): refuse (0A000) when the running
    /// statement is on a branch. Every schema statement a branch scopes is
    /// answered by the branch DDL layer before it reaches them, and every other
    /// one is refused there; this keeps a path that slipped past both (an
    /// executor arm, an engine-internal call made while a branch session is
    /// installed) from changing main's catalog. MERGE runs its main-side DDL
    /// under a main scope (`crate::main_statement_scope`). One thread-local
    /// read on main.
    pub(crate) fn refuse_main_catalog_write_on_branch(&self, statement: &str) -> Result<()> {
        if !self.is_branch_active() {
            return Ok(());
        }
        let name = self
            .active_branch()
            .map_or_else(|| "?".to_string(), |binding| binding.name.clone());
        Err(Error::branch_schema_unsupported(statement, &name))
    }

    /// What every branch-catalog write is followed by: the schema generation
    /// (every cached record, existence answer and committed overlay of every
    /// branch is stamped with it) and the level's write generation for the
    /// table move, the level's cached record goes, and the autocommit
    /// durability barrier runs.
    pub(crate) fn after_branch_catalog_write(&self, level: u64, table: &str) -> Result<()> {
        self.bump_schema_generation();
        super::branch_view::bump_write_generation(self.instance_id(), level, table);
        RECORDS.remove(&(self.instance_id(), level, table.to_string()));
        FROZEN_MISSING.remove(&(self.instance_id(), level, table.to_string()));
        self.durable_autocommit_barrier()
    }
}

/// The catalog form of a column `ALTER TABLE … ADD COLUMN` adds — exactly the
/// `Column` main's `alter_table_add_column` builds (`default_expr` is the
/// default serialized as JSON).
pub fn added_column(table: &str, definition: &ColumnDef) -> Column {
    Column {
        name: definition.name.clone(),
        data_type: definition.data_type.clone(),
        nullable: !definition.not_null,
        primary_key: definition.primary_key,
        source_table: None,
        source_table_name: Some(table.to_string()),
        default_expr: definition
            .default
            .as_ref()
            .map(|expr| serde_json::to_string(expr).unwrap_or_default()),
        unique: definition.unique,
        storage_mode: definition.storage_mode,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::Config;

    fn schema() -> Schema {
        let mut id = Column::new("id", DataType::Int4);
        id.primary_key = true;
        id.nullable = false;
        let mut code = Column::new("code", DataType::Text);
        code.unique = true;
        Schema::new(vec![id, code])
    }

    fn defined(created: bool) -> BranchTableRecord {
        let schema = schema();
        let constraints = TableConstraints::default();
        BranchTableRecord {
            state: BranchTableState::Defined(BranchTableDefinition {
                unique_rules: derive_unique_rules("t", &schema, &constraints),
                schema,
                constraints,
                identity_columns: Vec::new(),
                incarnation: 11,
            }),
            isolated: true,
            frozen: false,
            parent_fingerprint: Some(7),
            parent_incarnation: Some(5),
            created,
            added_columns: Vec::new(),
        }
    }

    #[test]
    fn a_record_round_trips_and_an_unknown_format_is_refused() {
        let record = defined(true);
        let bytes = record.encode().unwrap();
        assert!(bytes.starts_with(b"BMT1"));
        let back = BranchTableRecord::decode("t", &bytes).unwrap();
        assert!(back.created && back.isolated && !back.frozen);
        assert_eq!(back.parent_fingerprint, Some(7));
        assert_eq!(back.parent_incarnation, Some(5));
        assert_eq!(back.definition().unwrap().incarnation, 11);
        assert_eq!(back.definition().unwrap().schema.columns.len(), 2);
        let mut wrong = bytes.clone();
        wrong[4] = 99;
        assert!(BranchTableRecord::decode("t", &wrong).is_err());
        assert!(BranchTableRecord::decode("t", b"junk").is_err());
    }

    #[test]
    fn rules_follow_mains_names() {
        let rules = derive_unique_rules("t", &schema(), &TableConstraints::default());
        assert_eq!(rules.len(), 2);
        assert!(rules[0].is_primary_key && rules[0].label == "t_pkey");
        assert_eq!(rules[1].label, "t_code_key");
        assert_eq!(rules[1].columns, vec!["code".to_string()]);
    }

    #[test]
    fn the_fingerprint_ignores_resolver_stamps_and_sees_columns() {
        let base = schema();
        let constraints = TableConstraints::default();
        let mut stamped = base.clone();
        for column in &mut stamped.columns {
            column.source_table_name = Some("t".into());
        }
        assert_eq!(
            definition_fingerprint(&base, &constraints, &[]).unwrap(),
            definition_fingerprint(&stamped, &constraints, &[]).unwrap()
        );
        let mut wider = base.clone();
        wider.columns.push(Column::new("extra", DataType::Int4));
        assert_ne!(
            definition_fingerprint(&base, &constraints, &[]).unwrap(),
            definition_fingerprint(&wider, &constraints, &[]).unwrap()
        );
    }

    /// B3 fix pass: a UNIQUE rule is part of a definition, by column SET (not
    /// its name, not the order rules or constraint records were declared in).
    #[test]
    fn the_fingerprint_sees_unique_sets_and_ignores_declaration_order() {
        use crate::sql::constraints::UniqueConstraint;
        let base = schema();
        let none = TableConstraints::default();
        let rule = |name: &str, columns: &[&str]| BranchUniqueRule {
            index_name: name.to_string(),
            label: name.to_string(),
            columns: columns.iter().map(|c| (*c).to_string()).collect(),
            is_primary_key: false,
        };
        let plain = definition_fingerprint(&base, &none, &[]).unwrap();
        let with_a = definition_fingerprint(&base, &none, &[rule("x", &["id", "code"])]).unwrap();
        assert_ne!(
            plain, with_a,
            "a unique index on the target must change the fingerprint"
        );
        assert_eq!(
            with_a,
            definition_fingerprint(&base, &none, &[rule("x_2", &["CODE", "id"])]).unwrap(),
            "a re-minted name or a column order is not a change"
        );
        let unique = |name: &str| UniqueConstraint {
            name: name.to_string(),
            table_name: "t".to_string(),
            columns: vec![name.to_string()],
            is_primary_key: false,
        };
        let forward = TableConstraints {
            unique_constraints: vec![unique("a"), unique("b")],
            ..TableConstraints::default()
        };
        let reverse = TableConstraints {
            unique_constraints: vec![unique("b"), unique("a")],
            ..TableConstraints::default()
        };
        assert_eq!(
            definition_fingerprint(&base, &forward, &[]).unwrap(),
            definition_fingerprint(&base, &reverse, &[]).unwrap()
        );
    }

    #[test]
    fn a_fork_record_round_trips_and_an_unknown_format_is_refused() {
        let mut entries = BTreeMap::new();
        entries.insert(
            "t".to_string(),
            ForkEntry {
                incarnation: Some(42),
                fingerprint: 7,
                columns: vec!["id".to_string()],
            },
        );
        let bytes = encode_fork_record(&entries).unwrap();
        assert!(bytes.starts_with(b"BMF1"));
        assert_eq!(decode_fork_record(&bytes).unwrap(), entries);
        assert!(decode_fork_record(b"junk").is_err());
    }

    /// A missing incarnation is unknown, never a difference.
    #[test]
    fn an_unknown_incarnation_never_differs() {
        assert!(incarnations_differ(Some(1), Some(2)));
        assert!(!incarnations_differ(Some(1), Some(1)));
        assert!(!incarnations_differ(None, Some(2)));
        assert!(!incarnations_differ(Some(1), None));
        assert!(!incarnations_differ(None, None));
        assert_eq!(known_incarnation(0), None);
        assert_eq!(known_incarnation(9), Some(9));
    }

    /// The fail point fires at the n-th tick of its site, once.
    #[test]
    fn the_merge_failpoint_fires_once_at_its_tick() {
        let engine = StorageEngine::open_in_memory(&Config::in_memory()).unwrap();
        engine.merge_failpoint_tick(MergeFailSite::Step).unwrap();
        engine.debug_fail_merge_after_steps(1);
        engine.merge_failpoint_tick(MergeFailSite::Row).unwrap();
        engine.merge_failpoint_tick(MergeFailSite::Step).unwrap();
        assert!(engine.merge_failpoint_tick(MergeFailSite::Step).is_err());
        engine.merge_failpoint_tick(MergeFailSite::Step).unwrap();
    }

    /// An exclusion refuses DDL on its table of its level only, refuses a second
    /// merge that writes the table, lets two merges that only depend on it share
    /// it, and is gone once released.
    #[test]
    fn a_merge_exclusion_holds_its_tables() {
        let engine = StorageEngine::open_in_memory(&Config::in_memory()).unwrap();
        let tables = vec!["t".to_string()];
        engine
            .register_merge_exclusion(MERGE_TARGET_MAIN, 7, "b", "main", &tables, true)
            .unwrap();
        let refused = engine
            .refuse_ddl_on_merging_table(MERGE_TARGET_MAIN, "t")
            .unwrap_err()
            .to_string();
        assert!(refused.contains(crate::error::BRANCH_MERGE_BUSY_MARKER), "{refused}");
        engine.refuse_ddl_on_merging_table(MERGE_TARGET_MAIN, "u").unwrap();
        engine.refuse_ddl_on_merging_table(3, "t").unwrap();
        let busy = engine
            .register_merge_exclusion(MERGE_TARGET_MAIN, 8, "c", "main", &tables, true)
            .unwrap_err()
            .to_string();
        assert!(busy.contains(crate::error::BRANCH_MERGE_BUSY_MARKER), "{busy}");
        assert!(!busy.starts_with("table t is part of"), "{busy}");
        engine
            .register_merge_exclusion(MERGE_TARGET_MAIN, 7, "b", "main", &tables, true)
            .unwrap();
        engine.release_merge_exclusion(7);
        engine.refuse_ddl_on_merging_table(MERGE_TARGET_MAIN, "t").unwrap();

        // Two dependents share it; DDL is refused until the last one is gone.
        for source in [10, 11] {
            engine
                .register_merge_exclusion(MERGE_TARGET_MAIN, source, "d", "b", &tables, false)
                .unwrap();
        }
        assert!(engine
            .register_merge_exclusion(MERGE_TARGET_MAIN, 12, "e", "main", &tables, true)
            .is_err());
        engine.release_merge_exclusion(10);
        assert!(engine.refuse_ddl_on_merging_table(MERGE_TARGET_MAIN, "t").is_err());
        engine.release_merge_exclusion(11);
        engine.refuse_ddl_on_merging_table(MERGE_TARGET_MAIN, "t").unwrap();
    }

    /// Spec R5.1: a schema statement holds its tables exclusively. What it holds is
    /// not looked at again, so a merge registering while it runs waits for it instead
    /// of tearing it; a statement arriving afterwards, and a route with no statement,
    /// is refused; and a refused one leaves nothing held.
    #[test]
    fn a_schema_statement_holds_its_tables_and_is_refused_by_an_exclusion_it_finds() {
        let engine = StorageEngine::open_in_memory(&Config::in_memory()).unwrap();
        engine.debug_set_table_lock_timeout(std::time::Duration::from_millis(20));
        let tables = vec!["t".to_string()];

        let on_t = engine.enter_main_statement(&["t"], "ALTER TABLE").unwrap();
        assert!(engine.debug_table_lock_held_exclusively("t"));
        engine
            .register_merge_exclusion(MERGE_TARGET_MAIN, 7, "b", "main", &tables, true)
            .unwrap();
        // Past its check, the statement's own catalog writes hold what it holds.
        drop(engine.enter_main_ddl(&["t"]).unwrap());
        assert!(
            engine.debug_table_lock_held_exclusively("t"),
            "the nested write took nothing of its own"
        );
        drop(on_t);
        assert!(!engine.debug_table_lock_held_exclusively("t"));

        assert!(
            engine.enter_main_statement(&["t"], "ALTER TABLE").is_err(),
            "after the exclusion: refused"
        );
        assert!(engine.enter_main_ddl(&["t"]).is_err(), "no statement guard: refused");
        assert!(
            !engine.debug_table_lock_held_exclusively("t"),
            "a refusal leaves nothing held"
        );
        engine.release_merge_exclusion(7);

        // Another table is none of its business.
        engine
            .register_merge_exclusion(MERGE_TARGET_MAIN, 8, "c", "main", &tables, true)
            .unwrap();
        drop(engine.enter_main_statement(&["u"], "ALTER TABLE").unwrap());
        // A merge's own catalog writes are not DDL on its own tables, and its own
        // locks cover them.
        {
            let _scope = MergeDdlScope::enter(engine.instance_id());
            let _inert = engine.enter_main_ddl(&["t"]).unwrap();
            assert!(!engine.debug_table_lock_held_exclusively("t"));
        }
        assert!(engine.enter_main_ddl(&["t"]).is_err());
        engine.release_merge_exclusion(8);
    }

    /// R5 decision 11: a reader's fill, with the generation it read before its durable
    /// read, cannot outlive a DDL that moved the generation: it read a record that DDL
    /// may have deleted. The DROP that re-cached a table this way made `RENAME x TO it`
    /// fail "already exists". A publication goes in whatever the generation is.
    #[test]
    fn a_readers_fill_does_not_outlive_a_ddl_that_moved_the_generation() {
        let engine = StorageEngine::open_in_memory(&Config::in_memory()).unwrap();
        let before = engine.schema_generation();
        engine.cache_schema_if_current("t", schema(), before);
        assert!(engine.get_cached_schema("t").is_some(), "nothing moved it");
        engine.invalidate_schema_cache("t");

        let before = engine.schema_generation();
        engine.bump_schema_generation();
        engine.invalidate_schema_cache("t");
        engine.cache_schema_if_current("t", schema(), before);
        assert!(engine.get_cached_schema("t").is_none(), "the DDL came first");
        engine.cache_table_constraints_if_current("t", crate::sql::TableConstraints::default(), before);
        assert!(engine.get_cached_table_constraints("t").is_none());
        engine.cache_referencing_fks_if_current("t", Vec::new(), before);
        assert!(engine.get_cached_referencing_fks("t").is_none());

        engine.cache_schema("t", schema());
        assert!(engine.get_cached_schema("t").is_some(), "a DDL's own publication");
    }

    /// A merge still validating has nothing to finish or abandon, and says so.
    #[test]
    fn ddl_during_a_merges_validation_is_told_to_wait() {
        let engine = StorageEngine::open_in_memory(&Config::in_memory()).unwrap();
        engine
            .register_merge_exclusion(MERGE_TARGET_MAIN, 7, "b", "main", &["t".to_string()], true)
            .unwrap();
        let refused = engine
            .refuse_ddl_on_merging_table(MERGE_TARGET_MAIN, "t")
            .unwrap_err()
            .to_string();
        assert!(refused.contains(crate::error::BRANCH_MERGE_BUSY_MARKER), "{refused}");
        assert!(
            !refused.contains(crate::error::BRANCH_MERGE_PENDING_MARKER),
            "{refused}"
        );
        assert!(!refused.contains("ABORT"), "{refused}");
    }

    /// The nearest level decides; a tombstone hides the table; no entry
    /// leaves it to main; level ids are matched exactly.
    #[test]
    fn the_nearest_level_decides() {
        let engine = StorageEngine::open_in_memory(&Config::in_memory()).unwrap();
        let tombstone = BranchTableRecord {
            state: BranchTableState::Dropped,
            isolated: true,
            frozen: false,
            parent_fingerprint: None,
            parent_incarnation: None,
            created: false,
            added_columns: Vec::new(),
        };
        engine
            .put_branch_table_record(2, "t", &defined(true), WriteBatch::default(), false)
            .unwrap();
        engine
            .put_branch_table_record(3, "t", &tombstone, WriteBatch::default(), false)
            .unwrap();
        assert!(matches!(
            engine.resolve_branch_table(&[2], "t").unwrap(),
            BranchTableResolution::Defined { level: 2, .. }
        ));
        assert!(matches!(
            engine.resolve_branch_table(&[2, 3], "t").unwrap(),
            BranchTableResolution::Dropped { level: 3, .. }
        ));
        assert!(matches!(
            engine.resolve_branch_table(&[12], "t").unwrap(),
            BranchTableResolution::Inherited
        ));
        assert!(matches!(
            engine.resolve_branch_table(&[2], "u").unwrap(),
            BranchTableResolution::Inherited
        ));
        // Removing a record is seen at once (the write bumps the generation).
        engine.restore_branch_table_record(3, "t", None, false).unwrap();
        assert!(matches!(
            engine.resolve_branch_table(&[2, 3], "t").unwrap(),
            BranchTableResolution::Defined { level: 2, .. }
        ));
        let listed = engine.branch_tables_in_chain(&[2, 3]).unwrap();
        assert_eq!(listed.keys().cloned().collect::<Vec<_>>(), vec!["t".to_string()]);
    }
}
