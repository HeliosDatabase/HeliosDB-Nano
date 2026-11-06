//! sprinter 6bc4a4040eb7 (phase B3): schema DDL on a branch.
//!
//! Plan `docs/plans/BRANCH_ISOLATION.md` D4, spec
//! `docs/plans/specs/B3_branch_schema.md`. On a branch:
//!
//! * `CREATE TABLE` / `CREATE TABLE … AS` (and `IF NOT EXISTS`) define the
//!   table in the branch's catalog overlay (`storage::branch_catalog`),
//!   isolated: its rows live on the branch only, its PRIMARY KEY / UNIQUE
//!   rules are the definition's, enforced by the branch view;
//! * `DROP TABLE` (and `IF EXISTS`, and the comma list) writes a tombstone in
//!   the overlay and deletes the branch's own rows of the table — main's
//!   table and rows, and every other branch, are untouched (this was item 1:
//!   a branch `DROP TABLE` dropped main's table);
//! * `ALTER TABLE … ADD COLUMN` (one, or a multi-op statement of nothing but
//!   ADD COLUMNs) computes the new definition exactly as main does,
//!   materializes every row the branch sees into its own overlay under it
//!   (main's rule for an existing row, `storage::row_shape::ColumnFill`) and
//!   writes the definition, isolated — all in one atomic batch;
//! * every OTHER schema statement is refused (SQLSTATE 0A000) before it
//!   changes anything, naming the alternative.
//!
//! [`EmbeddedDatabase::branch_schema_ddl`] is the ONE gate: both DML executor
//! families call it on every plan right after planning (text:
//! `execute_in_transaction_inner_body`; params:
//! `execute_plan_with_params_inner`), and the pre-plan DDL entry points
//! (`CREATE SCHEMA … <elements>`) refuse on a branch themselves. Behind it,
//! the executor's `plan_to_operator` refuses every schema plan that reaches it
//! on a branch ([`schema_statement_name`]), and main's catalog mutators refuse
//! a branch session (`StorageEngine::refuse_main_catalog_write_on_branch`) —
//! three layers, so a path that slips past one still fails closed.
//!
//! DDL on this engine is not transactional, on main or on a branch: a branch
//! DDL applies at once. Inside a transaction it is refused when the
//! transaction already wrote the table on the branch (its staged rows would
//! commit under a definition they were not written for).
//!
//! # A schema statement runs on the branch alone (B3 fix pass)
//!
//! Every branch DDL that is not a no-op holds the branch's exclusive section
//! ([`crate::storage::BranchDrains::exclusive`]) for its whole duration (a
//! `CREATE TABLE … AS` through its population): other statements on the branch
//! finish first, new ones wait for it, and a `CREATE BRANCH … PARENT` of the
//! branch waits too. It re-reads what it decided on, and refuses:
//!
//! * 55006, a table another open transaction has staged writes of: those rows
//!   would commit under a definition they were not written for;
//! * 0A000, a branch with live descendants, through merged branches too: they
//!   read through it, so changing a table under them would change what they see.
//!   Freezing it for them is phase B4's hook, which lifts this refusal;
//! * 55006, a table an unfinished MERGE BRANCH into this branch is changing.
//!
//! On MAIN, [`EmbeddedDatabase::main_ddl_statement_guard`] counts a schema
//! statement on the tables it names for its whole duration (a merge registering
//! its exclusion of one waits for it) and refuses (55006) one on a table an
//! unfinished merge holds. Past that check the statement is not refused again:
//! the merge is waiting for it, and a refusal half way would tear what it already
//! did. The catalog funnels count the same way and refuse only on a route with no
//! statement guard (`StorageEngine::main_catalog_write`), so a route that does not
//! come through a planned statement is covered too.
//!
//! Plans carry schemas, so a branch session neither reads nor fills the
//! shared SQL-text-keyed plan caches (`EmbeddedDatabase::current_schema_is_set`).

use crate::sql::logical_plan::{ColumnDef, LogicalPlan};
use crate::storage::branch_catalog::{
    added_column, derive_unique_rules, fresh_table_incarnation, known_incarnation, BranchAddedColumn,
    BranchTableDefinition, BranchTableRecord, BranchTableResolution, BranchTableState, MainDdlGuard,
};
use crate::storage::row_shape::ColumnFill;
use crate::storage::{BranchExclusiveGuard, BranchView, StorageEngine, Transaction, BRANCH_DRAIN_TIMEOUT};
use crate::{sql, EmbeddedDatabase, Error, Result, Schema, Value};
use rocksdb::WriteBatch;
use std::sync::Arc;

/// What a branch does with one plan.
enum BranchDdl<'p> {
    /// Not a schema statement: it runs as it would anyway.
    NotSchema,
    /// A schema statement the branch catalog does not scope: refused.
    Refuse(&'static str),
    /// `CREATE TABLE`.
    CreateTable,
    /// `CREATE TABLE … AS` / `SELECT … INTO`.
    CreateTableAs {
        name: &'p str,
        column_names: &'p [String],
        if_not_exists: bool,
        query: &'p LogicalPlan,
        with_data: bool,
    },
    /// `DROP TABLE` (one, or the comma list): (name, IF EXISTS).
    DropTables(Vec<(&'p str, bool)>),
    /// `ALTER TABLE t ADD COLUMN …` (every op of the statement): the table and
    /// (column, IF NOT EXISTS) in statement order.
    AddColumns {
        table: &'p str,
        columns: Vec<(&'p ColumnDef, bool)>,
    },
}

/// What `ALTER TABLE t ADD COLUMN …` makes of a table on a branch.
struct AddColumnsPlan {
    /// The table's schema as the branch sees it now.
    current: Schema,
    /// The definition it derives from.
    base: BranchTableDefinition,
    /// The level's own record of the table, if it has one.
    own: Option<Arc<BranchTableRecord>>,
    /// The new schema.
    schema: Schema,
    /// The columns the statement really adds.
    added: Vec<BranchAddedColumn>,
}

/// sprinter 6bc4a4040eb7 (phase B3): the name of every SCHEMA statement — a
/// plan that changes a catalog — for the refusals. `None` for everything
/// else: queries and DML, `TRUNCATE` (a branch empties its own view — B2),
/// the branch statements themselves, transaction control, the accept-and-
/// ignore no-ops (`CREATE EXTENSION` of an unknown extension, `DROP
/// EXTENSION`), and `CREATE` / `REFRESH MATERIALIZED VIEW`, which phase B2
/// already refuses on a branch with its own marker before anything is
/// written.
pub fn schema_statement_name(plan: &LogicalPlan) -> Option<&'static str> {
    Some(match plan {
        LogicalPlan::CreateTable { .. } => "CREATE TABLE",
        LogicalPlan::CreateTableAs { .. } => "CREATE TABLE … AS",
        LogicalPlan::DropTable { .. } => "DROP TABLE",
        LogicalPlan::DropMulti { drops } => return drops.iter().find_map(schema_statement_name).or(Some("DROP")),
        LogicalPlan::AlterTableAddColumn { .. } => "ALTER TABLE … ADD COLUMN",
        LogicalPlan::AlterTableMulti { operations } => {
            return operations
                .iter()
                .find(|op| !matches!(op, LogicalPlan::AlterTableAddColumn { .. }))
                .and_then(schema_statement_name)
                .or(Some("ALTER TABLE"))
        }
        LogicalPlan::AlterTableDropColumn { .. } => "ALTER TABLE … DROP COLUMN",
        LogicalPlan::AlterTableRenameColumn { .. } => "ALTER TABLE … RENAME COLUMN",
        LogicalPlan::AlterTableRename { .. } => "ALTER TABLE … RENAME TO",
        LogicalPlan::AlterTableSetSchema { .. } => "ALTER TABLE … SET SCHEMA",
        LogicalPlan::AlterTableAlterColumnNullability { .. } => "ALTER TABLE … ALTER COLUMN … NOT NULL",
        LogicalPlan::AlterColumnStorage { .. } => "ALTER TABLE … ALTER COLUMN … SET STORAGE",
        LogicalPlan::AlterTableAddForeignKey { .. } => "ALTER TABLE … ADD FOREIGN KEY",
        LogicalPlan::AlterTableAddUnique { .. } => "ALTER TABLE … ADD CONSTRAINT … UNIQUE",
        LogicalPlan::AlterTableAddPrimaryKey { .. } => "ALTER TABLE … ADD PRIMARY KEY",
        LogicalPlan::AlterTableAddCheck { .. } => "ALTER TABLE … ADD CONSTRAINT … CHECK",
        LogicalPlan::AlterTableAlterConstraintEnforcement { .. } => "ALTER TABLE … ALTER CONSTRAINT",
        LogicalPlan::AlterTableDropConstraint { .. } => "ALTER TABLE … DROP CONSTRAINT",
        LogicalPlan::CreateIndex { .. } => "CREATE INDEX",
        LogicalPlan::DropIndex { .. } => "DROP INDEX",
        LogicalPlan::CreateSequence { .. } => "CREATE SEQUENCE",
        LogicalPlan::AlterSequence(_) => "ALTER SEQUENCE",
        LogicalPlan::DropSequence { .. } => "DROP SEQUENCE",
        LogicalPlan::CreateEnumType { .. } => "CREATE TYPE",
        LogicalPlan::DropEnumType { .. } => "DROP TYPE",
        LogicalPlan::CreateSchema { .. } => "CREATE SCHEMA",
        LogicalPlan::DropSchema { .. } => "DROP SCHEMA",
        // `hdb_code` creates the code-graph tables in main's catalog; every
        // other name is an accept-and-ignore no-op.
        LogicalPlan::CreateExtension { name, .. } if name == "hdb_code" => "CREATE EXTENSION hdb_code",
        LogicalPlan::DropMaterializedView { .. } => "DROP MATERIALIZED VIEW",
        LogicalPlan::AlterMaterializedView { .. } => "ALTER MATERIALIZED VIEW",
        LogicalPlan::CreateView { .. } => "CREATE VIEW",
        LogicalPlan::DropView { .. } => "DROP VIEW",
        LogicalPlan::CreateTrigger { .. } => "CREATE TRIGGER",
        LogicalPlan::DropTrigger { .. } => "DROP TRIGGER",
        LogicalPlan::CreateFunction { .. } => "CREATE FUNCTION",
        LogicalPlan::CreateProcedure { .. } => "CREATE PROCEDURE",
        LogicalPlan::DropFunction { .. } => "DROP FUNCTION",
        LogicalPlan::DropProcedure { .. } => "DROP PROCEDURE",
        LogicalPlan::CreateRole { .. } => "CREATE ROLE",
        LogicalPlan::AlterRole { .. } => "ALTER ROLE",
        LogicalPlan::DropRole { .. } => "DROP ROLE",
        LogicalPlan::GrantPrivileges { .. } => "GRANT",
        LogicalPlan::RevokePrivileges { .. } => "REVOKE",
        _ => return None,
    })
}

/// sprinter 6bc4a4040eb7 (B3 fix pass): the tables a schema plan changes, for
/// the refusal on a table an unfinished MERGE BRANCH into main is changing.
/// `DROP INDEX` names an index, resolved here through main's index registry.
/// Every plan that names no table is not listed: it cannot touch a merge's rows.
fn schema_plan_tables(storage: &StorageEngine, plan: &LogicalPlan) -> Vec<String> {
    match plan {
        LogicalPlan::CreateTable { name, .. } | LogicalPlan::CreateTableAs { name, .. } => vec![name.clone()],
        // A drop takes the table's partition children with it, in writes of its
        // own (B3 third fix pass): they are refused or counted here, up front,
        // never half way through the drop.
        LogicalPlan::DropTable { name, .. } => storage.catalog().with_partition_descendants(vec![name.clone()]),
        LogicalPlan::DropMulti { drops } => drops
            .iter()
            .flat_map(|drop| schema_plan_tables(storage, drop))
            .collect(),
        LogicalPlan::AlterTableMulti { operations } => operations
            .iter()
            .flat_map(|operation| schema_plan_tables(storage, operation))
            .collect(),
        LogicalPlan::AlterTableAddColumn { table_name, .. }
        | LogicalPlan::AlterTableDropColumn { table_name, .. }
        | LogicalPlan::AlterTableRenameColumn { table_name, .. }
        | LogicalPlan::AlterTableAlterColumnNullability { table_name, .. }
        | LogicalPlan::AlterColumnStorage { table_name, .. }
        | LogicalPlan::AlterTableAddForeignKey { table_name, .. }
        | LogicalPlan::AlterTableAddUnique { table_name, .. }
        | LogicalPlan::AlterTableAddPrimaryKey { table_name, .. }
        | LogicalPlan::AlterTableAddCheck { table_name, .. }
        | LogicalPlan::AlterTableAlterConstraintEnforcement { table_name, .. }
        | LogicalPlan::AlterTableDropConstraint { table_name, .. }
        | LogicalPlan::CreateIndex { table_name, .. }
        | LogicalPlan::CreateTrigger { table_name, .. }
        | LogicalPlan::AlterTableSetSchema { table_name, .. } => vec![table_name.clone()],
        LogicalPlan::DropTrigger { table_name, .. } => table_name.iter().cloned().collect(),
        LogicalPlan::AlterTableRename {
            table_name,
            new_table_name,
        } => vec![table_name.clone(), new_table_name.clone()],
        // CASCADE takes every member with it.
        LogicalPlan::DropSchema { names, .. } => storage.catalog().with_partition_descendants(
            names
                .iter()
                .filter_map(|name| storage.catalog().schema_members(name).ok())
                .flatten()
                .collect(),
        ),
        LogicalPlan::DropIndex { name, .. } => storage
            .catalog()
            .get_index_definition(name)
            .ok()
            .flatten()
            .map(|definition| definition.table_name)
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

/// Classify `plan` for a statement on a branch.
fn classify(plan: &LogicalPlan) -> BranchDdl<'_> {
    match plan {
        LogicalPlan::CreateTable { .. } => BranchDdl::CreateTable,
        LogicalPlan::CreateTableAs {
            name,
            column_names,
            if_not_exists,
            query,
            with_data,
        } => BranchDdl::CreateTableAs {
            name,
            column_names,
            if_not_exists: *if_not_exists,
            query,
            with_data: *with_data,
        },
        LogicalPlan::DropTable { name, if_exists } => BranchDdl::DropTables(vec![(name.as_str(), *if_exists)]),
        LogicalPlan::DropMulti { drops } => {
            let mut tables = Vec::with_capacity(drops.len());
            for drop in drops {
                match drop {
                    LogicalPlan::DropTable { name, if_exists } => tables.push((name.as_str(), *if_exists)),
                    other => return BranchDdl::Refuse(schema_statement_name(other).unwrap_or("DROP")),
                }
            }
            BranchDdl::DropTables(tables)
        }
        LogicalPlan::AlterTableAddColumn {
            table_name,
            column_def,
            if_not_exists,
        } => add_columns(std::iter::once((table_name.as_str(), column_def, *if_not_exists))),
        LogicalPlan::AlterTableMulti { operations } => {
            let mut adds = Vec::with_capacity(operations.len());
            for op in operations {
                match op {
                    LogicalPlan::AlterTableAddColumn {
                        table_name,
                        column_def,
                        if_not_exists,
                    } => adds.push((table_name.as_str(), column_def, *if_not_exists)),
                    other => return BranchDdl::Refuse(schema_statement_name(other).unwrap_or("ALTER TABLE")),
                }
            }
            add_columns(adds.into_iter())
        }
        other => match schema_statement_name(other) {
            Some(statement) => BranchDdl::Refuse(statement),
            None => BranchDdl::NotSchema,
        },
    }
}

/// `ADD COLUMN`s of one table, each column a plain one: an inline PRIMARY KEY
/// / UNIQUE is a constraint (the planner desugars them into their own ops,
/// which refuse; a flag that survived on the column is refused here too).
fn add_columns<'p>(adds: impl Iterator<Item = (&'p str, &'p ColumnDef, bool)>) -> BranchDdl<'p> {
    let mut table: Option<&'p str> = None;
    let mut columns = Vec::new();
    for (name, column, if_not_exists) in adds {
        if column.primary_key {
            return BranchDdl::Refuse("ALTER TABLE … ADD COLUMN … PRIMARY KEY");
        }
        if column.unique {
            return BranchDdl::Refuse("ALTER TABLE … ADD COLUMN … UNIQUE");
        }
        match table {
            None => table = Some(name),
            Some(first) if first == name => {}
            Some(_) => return BranchDdl::Refuse("ALTER TABLE of several tables"),
        }
        columns.push((column, if_not_exists));
    }
    match table {
        Some(table) => BranchDdl::AddColumns { table, columns },
        None => BranchDdl::NotSchema,
    }
}

/// The chain of a view's PARENT level (the view's chain without its own
/// level) — where a level's first entry for a table takes its
/// `parent_fingerprint` from.
fn parent_chain<'v>(view: &'v BranchView<'_>) -> &'v [u64] {
    view.chain().split_last().map_or(&[][..], |(_, parents)| parents)
}

impl EmbeddedDatabase {
    /// sprinter 6bc4a4040eb7 (phase B3): THE branch DDL gate — see the module
    /// docs. `Ok(None)`: not on a branch (one thread-local read), or `plan` is
    /// not a schema statement; run it as usual. `Ok(Some(rows))`: the
    /// statement was applied to the branch's catalog overlay. `Err`: refused
    /// (0A000 for a schema statement a branch does not scope) or failed —
    /// before anything changed, unless said otherwise.
    ///
    /// `txn` is the transaction the statement runs in (the text family's
    /// implicit or explicit one, the params family's session transaction);
    /// `original_sql` the statement text (the `PARTITION OF` detection).
    pub(crate) fn branch_schema_ddl(
        &self,
        plan: &LogicalPlan,
        txn: Option<&Transaction>,
        original_sql: Option<&str>,
    ) -> Result<Option<u64>> {
        if !self.storage.is_branch_active() {
            return Ok(None);
        }
        let ddl = classify(plan);
        if matches!(ddl, BranchDdl::NotSchema) {
            return Ok(None);
        }
        // A binding whose branch was dropped or merged is refused (3D000)
        // rather than read as main.
        let Some(view) = self.storage.branch_view()? else {
            return Ok(None);
        };
        if let BranchDdl::Refuse(statement) = ddl {
            return Err(Error::branch_schema_unsupported(statement, view.name()));
        }
        let rows = match ddl {
            BranchDdl::NotSchema | BranchDdl::Refuse(_) => return Ok(None),
            BranchDdl::CreateTable => self.branch_create_table(&view, plan, txn, original_sql)?,
            BranchDdl::CreateTableAs {
                name,
                column_names,
                if_not_exists,
                query,
                with_data,
            } => self.branch_create_table_as(&view, name, column_names, if_not_exists, query, with_data, txn)?,
            BranchDdl::DropTables(targets) => self.branch_drop_tables(&view, &targets, txn)?,
            BranchDdl::AddColumns { table, columns } => self.branch_add_columns(&view, table, &columns, txn)?,
        };
        Ok(Some(rows))
    }

    /// sprinter 6bc4a4040eb7 (phase B3): refuse (0A000) a pre-plan schema
    /// entry point (`CREATE SCHEMA … CREATE TABLE …`) on a branch, before it
    /// runs any of its parts.
    pub(crate) fn refuse_schema_entry_on_branch(&self, statement: &str) -> Result<()> {
        if !self.storage.is_branch_active() {
            return Ok(());
        }
        let name = self
            .storage
            .active_branch()
            .map_or_else(|| "?".to_string(), |binding| binding.name.clone());
        Err(Error::branch_schema_unsupported(statement, &name))
    }

    /// sprinter 6bc4a4040eb7 (B3 second fix pass): a schema statement on main
    /// starts: counted until the guard drops, and refused (55006) when it touches
    /// a table an unfinished merge holds. `None` for anything else, and on a
    /// branch, whose own statements answer to `branch_schema_ddl`. Both executor
    /// families and the query path hold this for the statement's whole duration.
    pub(crate) fn main_ddl_statement_guard(&self, plan: &LogicalPlan) -> Result<Option<MainDdlGuard>> {
        if self.storage.is_branch_active() || schema_statement_name(plan).is_none() {
            return Ok(None);
        }
        let tables = schema_plan_tables(&self.storage, plan);
        let tables: Vec<&str> = tables.iter().map(String::as_str).collect();
        self.storage.enter_main_statement(&tables).map(Some)
    }

    /// sprinter 6bc4a4040eb7 (B3 second and third fix passes): a statement that
    /// arrived on a query route. A schema statement is the same statement there:
    /// on a branch it applies to the branch's catalog (`true`: done, no rows; it
    /// used to reach the executor's backstop and get a wrong 0A000), on main it is
    /// counted and checked like both executor families' (`Some` guard, to hold for
    /// the rest of the statement, whose plan must not go in the plan cache: a hit
    /// would skip all this). `(None, false)` for anything else.
    pub(crate) fn schema_statement_on_query_path(
        &self,
        plan: &LogicalPlan,
        txn: Option<&Transaction>,
        sql: &str,
    ) -> Result<(Option<MainDdlGuard>, bool)> {
        if schema_statement_name(plan).is_none() {
            return Ok((None, false));
        }
        let guard = self.main_ddl_statement_guard(plan)?;
        let handled = self.branch_schema_ddl(plan, txn, Some(sql))?.is_some();
        Ok((guard, handled))
    }

    /// sprinter 6bc4a4040eb7 (B3 fix pass): this schema statement runs on the
    /// branch alone until the guard drops. See the module docs.
    fn branch_ddl_section(&self, view: &BranchView<'_>) -> Result<BranchExclusiveGuard> {
        self.storage
            .branch_drains()
            .exclusive(view.branch_id(), view.name(), BRANCH_DRAIN_TIMEOUT)
    }

    /// The refusals of a schema statement that is not a no-op, inside its
    /// exclusive section: this transaction's own staged writes of `table`, live
    /// children of the branch, another transaction's staged writes of `table`,
    /// and an unfinished merge into the branch that is changing `table`.
    fn branch_ddl_preflight(
        &self,
        view: &BranchView<'_>,
        txn: Option<&Transaction>,
        table: &str,
        statement: &str,
    ) -> Result<()> {
        self.refuse_branch_ddl_after_writes(view, txn, table, statement)?;
        let children = self.storage.live_branch_children(view.branch_id(), None)?;
        if !children.is_empty() {
            return Err(Error::branch_schema_has_children(statement, view.name(), &children));
        }
        if crate::storage::branch_view::pending_writers(self.storage.instance_id(), view.branch_id(), table) > 0 {
            return Err(Error::branch_table_busy(table, view.name()));
        }
        self.storage.refuse_ddl_on_merging_table(view.branch_id(), table)
    }

    /// A branch DDL on `table` is refused when the transaction it runs in has
    /// already staged writes of `table` on the branch: they would commit under
    /// a definition they were not written for (DDL applies at once). With no
    /// session transaction, the embedded handle's global one is asked — never
    /// by taking its lock when this thread already holds it.
    fn refuse_branch_ddl_after_writes(
        &self,
        view: &BranchView<'_>,
        txn: Option<&Transaction>,
        table: &str,
        statement: &str,
    ) -> Result<()> {
        let wrote = match txn {
            Some(txn) => view.has_staged_writes(txn, table),
            None if self.global_txn_visible() && !crate::GLOBAL_TXN_LOCK_HELD.with(std::cell::Cell::get) => {
                let guard = self.lock_owned_global_txn();
                guard
                    .as_ref()
                    .and_then(|slot| slot.as_ref())
                    .is_some_and(|txn| view.has_staged_writes(txn, table))
            }
            None => false,
        };
        if wrote {
            return Err(Error::query_execution(format!(
                "{statement} of table '{table}' after the open transaction wrote it {} (branch \"{}\"): COMMIT \
                 or ROLLBACK first",
                crate::error::BRANCH_WRITE_UNSUPPORTED_MARKER,
                view.name()
            )));
        }
        Ok(())
    }

    /// The `parent_fingerprint` level `view`'s new entry for `table` records:
    /// its existing entry's, when it has one (the FIRST change is what MERGE
    /// compares against), else the fingerprint of the definition its parent
    /// sees now (`None`: the parent has no such table).
    fn branch_parent_fingerprint(
        &self,
        view: &BranchView<'_>,
        own: Option<&BranchTableRecord>,
        table: &str,
    ) -> Result<Option<u64>> {
        match own {
            Some(record) => Ok(record.parent_fingerprint),
            None => Ok(self
                .storage
                .chain_table_definition(parent_chain(view), table)?
                .map(|(_, fingerprint)| fingerprint)),
        }
    }

    /// The `parent_incarnation` level `view`'s new entry for `table` records —
    /// taken exactly as [`Self::branch_parent_fingerprint`] takes its answer.
    fn branch_parent_incarnation(
        &self,
        view: &BranchView<'_>,
        own: Option<&BranchTableRecord>,
        table: &str,
    ) -> Result<Option<u64>> {
        match own {
            Some(record) => Ok(record.parent_incarnation),
            None => self.storage.chain_table_incarnation(parent_chain(view), table),
        }
    }

    /// `CREATE TABLE` on a branch: the definition main would create (same
    /// columns, defaults, constraint records, PRIMARY KEY / UNIQUE rules,
    /// identity columns), written to the branch's catalog overlay, isolated.
    fn branch_create_table(
        &self,
        view: &BranchView<'_>,
        plan: &LogicalPlan,
        txn: Option<&Transaction>,
        original_sql: Option<&str>,
    ) -> Result<u64> {
        let LogicalPlan::CreateTable {
            name,
            columns,
            constraints,
            if_not_exists,
        } = plan
        else {
            return Err(Error::internal(
                "branch_create_table called with a non-CREATE-TABLE plan",
            ));
        };
        if sql::temp_tables::is_temp_key(name) {
            return Err(Error::branch_schema_unsupported("CREATE TEMPORARY TABLE", view.name()));
        }
        if original_sql.and_then(sql::Parser::extract_partition_of).is_some() {
            return Err(Error::branch_schema_unsupported(
                "CREATE TABLE … PARTITION OF",
                view.name(),
            ));
        }
        let catalog = self.storage.catalog();
        let exists = |catalog: &crate::storage::Catalog<'_>| -> Result<Option<u64>> {
            if !catalog.table_exists(name)? {
                return Ok(None);
            }
            if *if_not_exists {
                return Ok(Some(0));
            }
            Err(Error::query_execution(format!("Table '{}' already exists", name)))
        };
        // A no-op takes no section (two concurrent ones would refuse each other);
        // the question is asked again once the branch is this statement's alone.
        if let Some(rows) = exists(&catalog)? {
            return Ok(rows);
        }
        let _section = self.branch_ddl_section(view)?;
        if let Some(rows) = exists(&catalog)? {
            return Ok(rows);
        }
        self.branch_ddl_preflight(view, txn, name, "CREATE TABLE")?;
        let schema = Schema::new(Self::create_table_schema_columns(columns));
        // FOREIGN KEY targets first, as main does (42P01 / 42703 / 42704 /
        // 42830) — through the branch's catalog.
        Self::validate_create_table_fk_targets(&catalog, name, columns, constraints)?;

        let level = view.branch_id();
        let previous = self.storage.branch_table_record(level, name)?;
        let parent_fingerprint = self.branch_parent_fingerprint(view, previous.as_deref(), name)?;
        let parent_incarnation = self.branch_parent_incarnation(view, previous.as_deref(), name)?;
        let incarnation = fresh_table_incarnation();
        let identity_columns: Vec<String> = columns
            .iter()
            .filter(|column| column.is_identity)
            .map(|column| column.name.clone())
            .collect();
        let record = |set: sql::TableConstraints| BranchTableRecord {
            state: BranchTableState::Defined(BranchTableDefinition {
                unique_rules: derive_unique_rules(name, &schema, &set),
                schema: schema.clone(),
                constraints: set,
                identity_columns: identity_columns.clone(),
                incarnation,
            }),
            isolated: true,
            frozen: false,
            parent_fingerprint,
            parent_incarnation,
            created: true,
            added_columns: Vec::new(),
        };
        // Phase 1, as main orders it (`catalog.create_table` before the
        // constraint records): the table exists on the branch, so a
        // list-less self-REFERENCES binds to its PRIMARY KEY. Any rows the
        // level still held under the name (a table it inherited and main has
        // since dropped) go with this write: a new table starts empty.
        self.storage.put_branch_table_record(
            level,
            name,
            &record(sql::TableConstraints::default()),
            WriteBatch::default(),
            true,
        )?;
        // Phase 2: the constraint record exactly as main saves it.
        let built = if constraints.is_empty() {
            Ok(None)
        } else {
            Self::create_table_statement_constraints(&catalog, name, constraints).map(Some)
        };
        let final_constraints = built.map(|statement| {
            match Self::create_table_column_constraints(name, columns, constraints, statement.clone()) {
                Some(merged) => merged,
                None => statement.unwrap_or_default(),
            }
        });
        match final_constraints {
            Ok(set) => {
                self.storage
                    .put_branch_table_record(level, name, &record(set), WriteBatch::default(), false)?;
                Ok(1)
            }
            Err(e) => {
                // Leave no half-created table behind.
                if let Err(unwind) = self
                    .storage
                    .restore_branch_table_record(level, name, previous.as_deref(), true)
                {
                    tracing::warn!(
                        "CREATE TABLE '{}' on branch '{}' unwind failed: {}",
                        name,
                        view.name(),
                        unwind
                    );
                }
                Err(e)
            }
        }
    }

    /// `CREATE TABLE … AS` on a branch: the table defined on the branch from
    /// the query's static columns (as main does), then populated through the
    /// same `INSERT … SELECT` arm main uses — on a branch that arm writes the
    /// branch's overlay. A failed population leaves no table behind.
    #[allow(clippy::too_many_arguments)]
    fn branch_create_table_as(
        &self,
        view: &BranchView<'_>,
        name: &str,
        column_names: &[String],
        if_not_exists: bool,
        query: &LogicalPlan,
        with_data: bool,
        txn: Option<&Transaction>,
    ) -> Result<u64> {
        if sql::temp_tables::is_temp_key(name) {
            return Err(Error::branch_schema_unsupported(
                "CREATE TEMPORARY TABLE … AS",
                view.name(),
            ));
        }
        let catalog = self.storage.catalog();
        let exists = |catalog: &crate::storage::Catalog<'_>| -> Result<Option<u64>> {
            if !catalog.table_exists(name)? {
                return Ok(None);
            }
            if if_not_exists {
                return Ok(Some(0));
            }
            Err(Error::query_execution(format!("Table '{}' already exists", name)))
        };
        if let Some(rows) = exists(&catalog)? {
            return Ok(rows);
        }
        // Through the population.
        let _section = self.branch_ddl_section(view)?;
        if let Some(rows) = exists(&catalog)? {
            return Ok(rows);
        }
        self.branch_ddl_preflight(view, txn, name, "CREATE TABLE … AS")?;
        let schema = Schema::new(sql::Planner::ctas_target_columns(&query.schema(), column_names)?);
        let constraints = sql::TableConstraints::default();
        let level = view.branch_id();
        let previous = self.storage.branch_table_record(level, name)?;
        let parent_fingerprint = self.branch_parent_fingerprint(view, previous.as_deref(), name)?;
        let parent_incarnation = self.branch_parent_incarnation(view, previous.as_deref(), name)?;
        let record = BranchTableRecord {
            state: BranchTableState::Defined(BranchTableDefinition {
                unique_rules: derive_unique_rules(name, &schema, &constraints),
                schema,
                constraints,
                identity_columns: Vec::new(),
                incarnation: fresh_table_incarnation(),
            }),
            isolated: true,
            frozen: false,
            parent_fingerprint,
            parent_incarnation,
            created: true,
            added_columns: Vec::new(),
        };
        self.storage
            .put_branch_table_record(level, name, &record, WriteBatch::default(), true)?;
        if !with_data {
            return Ok(0);
        }
        // The SAME population main's CTAS runs (`execute_create_table_as`):
        // optimize the source, then the InsertSelect arm — branch-aware since
        // phase B2, so the rows land on this branch only.
        let populated = self
            .cold_optimizer()
            .optimize_recursive(query.clone())
            .and_then(|source| {
                let insert = LogicalPlan::InsertSelect {
                    table_name: name.to_string(),
                    columns: None,
                    source: Box::new(source),
                    returning: None,
                };
                self.execute_plan_with_params(&insert, &[], None)
                    .map(|(count, _returned)| count)
            });
        match populated {
            Ok(count) => Ok(count),
            Err(e) => {
                if let Err(unwind) = self
                    .storage
                    .restore_branch_table_record(level, name, previous.as_deref(), true)
                {
                    tracing::warn!(
                        "CREATE TABLE '{}' AS on branch '{}': population failed and the unwind failed too: {}",
                        name,
                        view.name(),
                        unwind
                    );
                }
                Err(e)
            }
        }
    }

    /// `DROP TABLE` on a branch: for every target (all checked first, so a
    /// missing one drops nothing), a tombstone in the branch's catalog
    /// overlay and the deletion of the branch's own rows of the table, in one
    /// batch per table. Main's table, its rows, and every other branch are
    /// untouched; a child branch of this one stops seeing the table too.
    fn branch_drop_tables(
        &self,
        view: &BranchView<'_>,
        targets: &[(&str, bool)],
        txn: Option<&Transaction>,
    ) -> Result<u64> {
        let catalog = self.storage.catalog();
        // Which targets exist, all checked first. Asked twice: a statement that
        // drops nothing takes no section.
        let existing = |catalog: &crate::storage::Catalog<'_>| -> Result<Vec<String>> {
            let mut dropping: Vec<String> = Vec::with_capacity(targets.len());
            for (name, if_exists) in targets {
                if !catalog.table_exists(name)? {
                    if *if_exists {
                        continue;
                    }
                    return Err(Error::query_execution(format!("Table '{}' does not exist", name)));
                }
                if sql::temp_tables::is_temp_key(name) {
                    return Err(Error::branch_schema_unsupported(
                        "DROP TABLE of a temporary table",
                        view.name(),
                    ));
                }
                if !dropping.iter().any(|seen| seen == name) {
                    dropping.push((*name).to_string());
                }
            }
            Ok(dropping)
        };
        if existing(&catalog)?.is_empty() {
            return Ok(0);
        }
        let _section = self.branch_ddl_section(view)?;
        // A partitioned table takes its children with it, as main's DROP does
        // (`ddl::drop_table_and_partition_children`, B3 second fix pass). The
        // registry is main's: a branch cannot create a partition.
        let mut dropping = existing(&catalog)?;
        let mut next = 0;
        while let Some(name) = dropping.get(next).cloned() {
            next += 1;
            for child in catalog.partition_children(&name)? {
                if catalog.table_exists(&child)? && !dropping.contains(&child) {
                    dropping.push(child);
                }
            }
        }
        for name in &dropping {
            self.branch_ddl_preflight(view, txn, name, "DROP TABLE")?;
        }
        let level = view.branch_id();
        for name in &dropping {
            let name = name.as_str();
            let previous = self.storage.branch_table_record(level, name)?;
            let parent_fingerprint = self.branch_parent_fingerprint(view, previous.as_deref(), name)?;
            let parent_incarnation = self.branch_parent_incarnation(view, previous.as_deref(), name)?;
            let tombstone = BranchTableRecord {
                state: BranchTableState::Dropped,
                isolated: true,
                frozen: false,
                parent_fingerprint,
                parent_incarnation,
                created: false,
                added_columns: Vec::new(),
            };
            self.storage
                .put_branch_table_record(level, name, &tombstone, WriteBatch::default(), true)?;
        }
        Ok(0)
    }

    /// What `ALTER TABLE t ADD COLUMN …` would make of the table as the branch
    /// sees it now.
    fn plan_branch_add_columns(
        &self,
        view: &BranchView<'_>,
        table: &str,
        adds: &[(&ColumnDef, bool)],
    ) -> Result<AddColumnsPlan> {
        let catalog = self.storage.catalog();
        // The branch's current definition (main's message when it has none).
        let current = catalog.get_table_schema(table)?;
        let level = view.branch_id();
        let resolution = self.storage.resolve_branch_table(view.chain(), table)?;
        let (base, own) = match &resolution {
            BranchTableResolution::Defined { level: at, record, .. } => {
                (record.definition().cloned(), (*at == level).then(|| Arc::clone(record)))
            }
            BranchTableResolution::Dropped { .. } => (None, None),
            BranchTableResolution::Inherited => (
                self.storage
                    .main_table_definition(table)?
                    .map(|(definition, _)| definition),
                None,
            ),
        };
        let base = base.ok_or_else(|| Error::query_execution(format!("Table '{}' does not exist", table)))?;

        let mut schema = base.schema.clone();
        let mut added: Vec<BranchAddedColumn> = Vec::new();
        for (definition, if_not_exists) in adds {
            if schema.columns.iter().any(|column| column.name == definition.name) {
                if *if_not_exists {
                    continue;
                }
                return Err(Error::query_execution(format!(
                    "Column '{}' already exists in table '{}'",
                    definition.name, table
                )));
            }
            let column = added_column(table, definition);
            schema.columns.push(column.clone());
            added.push(BranchAddedColumn {
                column,
                default: definition.default.clone(),
                definition: (*definition).clone(),
            });
        }
        Ok(AddColumnsPlan {
            current,
            base,
            own,
            schema,
            added,
        })
    }

    /// `ALTER TABLE t ADD COLUMN …` on a branch: the new definition exactly as
    /// main computes it (`alter_table_add_column`: the same `Column`, the same
    /// "already exists" refusal, `IF NOT EXISTS` honoured), every row the
    /// branch sees rewritten into its own overlay under it with main's
    /// backfill value, and the definition written isolated, last
    /// ([`BranchView::materialize`], in chunks). A row the level did not already
    /// hold is marked a COPY, so MERGE does not carry it. Returns the rows
    /// rewritten (main's count). O(rows the branch sees).
    fn branch_add_columns(
        &self,
        view: &BranchView<'_>,
        table: &str,
        adds: &[(&ColumnDef, bool)],
        txn: Option<&Transaction>,
    ) -> Result<u64> {
        if sql::temp_tables::is_temp_key(table) {
            return Err(Error::branch_schema_unsupported(
                "ALTER TABLE … ADD COLUMN of a temporary table",
                view.name(),
            ));
        }
        // A statement that adds nothing (IF NOT EXISTS) takes no section; the
        // definition is read again once the branch is this statement's alone.
        if self.plan_branch_add_columns(view, table, adds)?.added.is_empty() {
            return Ok(0);
        }
        let _section = self.branch_ddl_section(view)?;
        let AddColumnsPlan {
            current,
            base,
            own,
            schema,
            mut added,
        } = self.plan_branch_add_columns(view, table, adds)?;
        if added.is_empty() {
            return Ok(0);
        }
        self.branch_ddl_preflight(view, txn, table, "ALTER TABLE … ADD COLUMN")?;

        // Spec R5.2: main's rule for what an existing row gets: a stable default
        // evaluated once (and that one value kept in the record, so a MERGE gives
        // main's rows what the branch's copies hold), a volatile one per row, and
        // no NULL in a NOT NULL column. A branch still materializes its copies.
        let fills = added
            .iter()
            .map(|column| {
                ColumnFill::for_default(column.default.as_ref(), &column.column.data_type).map(|fill| {
                    (
                        fill,
                        column.column.data_type.clone(),
                        !column.column.nullable,
                        column.column.name.clone(),
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        for (column, (fill, ..)) in added.iter_mut().zip(&fills) {
            if let (ColumnFill::Constant(value), true) = (fill, column.default.is_some()) {
                column.default = Some(sql::LogicalExpr::Literal(value.clone()));
            }
        }
        for (fill, _, not_null, name) in &fills {
            if *not_null
                && matches!(fill, ColumnFill::Constant(Value::Null))
                && !view.scan(None, table, &current)?.is_empty()
            {
                return Err(Error::add_column_null_on_existing_rows(table, name));
            }
        }
        let backfill = || -> Result<Vec<Value>> {
            fills
                .iter()
                .map(|(fill, data_type, not_null, name)| -> Result<Value> {
                    let value = fill.next_value(data_type)?;
                    if *not_null && matches!(value, Value::Null) {
                        return Err(Error::add_column_null_on_existing_rows(table, name));
                    }
                    Ok(value)
                })
                .collect()
        };
        let parent_fingerprint = self.branch_parent_fingerprint(view, own.as_deref(), table)?;
        let parent_incarnation = self.branch_parent_incarnation(view, own.as_deref(), table)?;
        let (created, added_columns) = match own.as_deref() {
            Some(record) if record.created => (true, Vec::new()),
            Some(record) => {
                let mut columns = record.added_columns.clone();
                columns.extend(added);
                (false, columns)
            }
            None => (false, added),
        };
        let record = BranchTableRecord {
            state: BranchTableState::Defined(BranchTableDefinition {
                schema,
                constraints: base.constraints,
                unique_rules: base.unique_rules,
                identity_columns: base.identity_columns,
                // The base's: it is the same table. A main table with no id yet
                // (a failed backfill) still gets a real one: it salts the key
                // index, and an unknown id compares as unknown either way.
                incarnation: known_incarnation(base.incarnation).unwrap_or_else(fresh_table_incarnation),
            }),
            isolated: true,
            frozen: false,
            parent_fingerprint,
            parent_incarnation,
            created,
            added_columns,
        };
        let rewritten = view.materialize_with(table, &current, &record, &backfill)?;
        tracing::info!(
            "Added {} column(s) to table '{}' on branch '{}', materialized {} rows",
            adds.len(),
            table,
            view.name(),
            rewritten
        );
        Ok(rewritten)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use crate::EmbeddedDatabase;

    /// The reverse-FK answer of a branch is served from its cache: asked once per
    /// deleted row, it decodes every record of every level to compute. The next
    /// schema change on the branch invalidates it.
    #[test]
    fn a_merged_table_over_the_write_group_limit_is_refused_before_anything_changes() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 'main')").unwrap();
        db.execute("CREATE BRANCH r AS OF NOW").unwrap();
        db.with_request_branch_scope("r", || {
            db.execute("UPDATE t SET v = 'branch' WHERE id = 1")?;
            db.execute("INSERT INTO t VALUES (2, 'branch')")
        })
        .unwrap();

        crate::storage::branch_view::FINAL_BATCH_LIMIT_OVERRIDE.with(|limit| limit.set(Some(16)));
        let refused = db.execute("MERGE BRANCH r INTO main");
        crate::storage::branch_view::FINAL_BATCH_LIMIT_OVERRIDE.with(|limit| limit.set(None));
        let message = refused.expect_err("one write group cannot hold it").to_string();
        assert!(
            message.contains(crate::error::BRANCH_MATERIALIZE_TOO_LARGE_MARKER) && message.contains("table t"),
            "{message}"
        );
        assert_eq!(
            db.storage.get_branch("r").unwrap().state,
            crate::storage::BranchState::Active,
            "nothing changed"
        );
        assert!(db.storage.branch_merge_intents().unwrap().is_empty(), "no intent");
        assert_eq!(db.query("SELECT v FROM t WHERE id = 1", &[]).unwrap().len(), 1);
        assert_eq!(db.query("SELECT id FROM t", &[]).unwrap().len(), 1);

        db.execute("MERGE BRANCH r INTO main").unwrap();
        assert_eq!(db.query("SELECT id FROM t", &[]).unwrap().len(), 2);
    }

    #[test]
    fn a_branch_reverse_fk_answer_is_cached_until_its_ddl() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE parent (id INT PRIMARY KEY)").unwrap();
        db.execute("CREATE BRANCH r AS OF NOW").unwrap();
        db.with_request_branch_scope("r", || {
            db.execute("CREATE TABLE child (id INT PRIMARY KEY, pid INT REFERENCES parent(id))")
        })
        .unwrap();
        let branch = db.storage.get_branch("r").unwrap().branch_id;
        let cached = |db: &EmbeddedDatabase| {
            db.storage
                .cached_branch_referencing_fks(branch, "parent", db.storage.schema_generation())
        };
        // B3 third fix pass: count the hits, because an answer that is stored but
        // never read would look the same from outside.
        let hits = || crate::storage::BRANCH_FK_CACHE_HITS.with(std::cell::Cell::get);
        assert!(cached(&db).is_none(), "not asked yet");
        let before = hits();
        let answer = db
            .with_request_branch_scope("r", || db.storage.catalog().get_referencing_fks("parent"))
            .unwrap();
        assert_eq!(answer.len(), 1);
        assert_eq!(hits(), before, "the first question is a miss");
        assert_eq!(cached(&db).map(|fks| fks.len()), Some(1), "and it stores the answer");
        let again = db
            .with_request_branch_scope("r", || db.storage.catalog().get_referencing_fks("parent"))
            .unwrap();
        assert_eq!(again.len(), 1);
        assert_eq!(hits(), before + 1, "the second question reads the cache");

        db.with_request_branch_scope("r", || db.execute("DROP TABLE child"))
            .unwrap();
        assert!(cached(&db).is_none(), "a branch DDL moves the schema generation");
        let answer = db
            .with_request_branch_scope("r", || db.storage.catalog().get_referencing_fks("parent"))
            .unwrap();
        assert!(answer.is_empty());
        assert_eq!(hits(), before + 1, "and the question after it is a miss again");
    }
}
