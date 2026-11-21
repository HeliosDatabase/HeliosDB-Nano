//! The privilege gate: may the current role run this plan?
//!
//! One function, [`check_plan`], called from every place a plan is about to
//! run. It resolves the principal first, so an unrestricted caller pays one
//! thread-local read and nothing else.
//!
//! Check order for a role: superuser, then ownership, then direct grants, then
//! grants inherited through role membership, then `PUBLIC`.
//!
//! For a query, the gate collects `(relation, privilege, columns)` by walking
//! the plan and every expression in it, subqueries included. A column that
//! cannot be attributed to a relation widens the requirement to the whole
//! relation: only a table-level grant satisfies it. There is no arm that means
//! "nothing required".

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::sql::logical_plan::{OnConflictAction, ReturningItem, TableConstraint};
use crate::sql::{LogicalExpr, LogicalPlan};
use crate::storage::StorageEngine;
use crate::{Error, Result};

use super::access::{current_principal, nothing_to_check, Principal};
use super::catalog::{self, Snapshot, DATABASE, FUNCTION, SCHEMA, SEQUENCE, TABLE};

/// Every refusal for a missing privilege starts with this. Both wires map a
/// message that contains it to SQLSTATE 42501.
pub const PERMISSION_DENIED: &str = "permission denied";
/// Every refusal for a missing ownership starts with this (42501 as well).
pub const MUST_BE_OWNER: &str = "must be owner of";
/// Appended to a refusal on a network session that has no role.
pub const NO_ROLE_ON_CONNECTION: &str = "no authenticated role on this connection";

/// The name of the database every connection sees.
pub const DATABASE_NAME: &str = "heliosdb";

/// True if the error is one of this gate's refusals.
pub fn is_permission_error(error: &Error) -> bool {
    let message = error.to_string();
    message.contains(PERMISSION_DENIED) || message.contains(MUST_BE_OWNER)
}

/// `permission denied for <kind> <object>`.
pub fn denied(kind_word: &str, object: &str) -> Error {
    Error::query_execution(format!(
        "{PERMISSION_DENIED} for {kind_word} {}",
        display_name(object)
    ))
}

/// `must be owner of <kind> <object>`.
pub fn not_owner(kind_word: &str, object: &str) -> Error {
    Error::query_execution(format!("{MUST_BE_OWNER} {kind_word} {}", display_name(object)))
}

/// `permission denied to <action>`.
pub fn denied_to(action: &str) -> Error {
    Error::query_execution(format!("{PERMISSION_DENIED} to {action}"))
}

/// The name PostgreSQL prints for a relation: without its schema.
fn display_name(object: &str) -> &str {
    object.rsplit_once('.').map_or(object, |(_, bare)| bare)
}

/// True for a relation in a session's temporary namespace: private to the
/// session that created it, so nothing is checked.
fn is_temp_relation(name: &str) -> bool {
    name.starts_with("pg_temp")
}

/// The schema part of a relation key; `public` for a bare key.
fn schema_of(object: &str) -> &str {
    object.split_once('.').map_or(catalog::PUBLIC, |(schema, _)| schema)
}

/// The columns of one relation that a statement needs a privilege on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequiredColumns {
    /// The relation is named but no column of it is read (`SELECT count(*)`).
    /// A grant on any one column is enough, as in PostgreSQL.
    AnyColumn,
    /// Exactly these columns, lower-cased. Never empty.
    Named(BTreeSet<String>),
    /// Every column of the relation (`INSERT` with no column list, `RETURNING *`).
    AllColumns,
    /// The fail-closed value: the columns could not be determined, or the
    /// privilege has no column form. Only a table-level grant satisfies it.
    TableWide,
}

impl RequiredColumns {
    fn one(column: &str) -> Self {
        let mut set = BTreeSet::new();
        set.insert(normalize_column(column));
        Self::Named(set)
    }

    fn named<I: IntoIterator<Item = String>>(columns: I) -> Self {
        let set: BTreeSet<String> = columns.into_iter().map(|c| normalize_column(&c)).collect();
        if set.is_empty() {
            Self::AllColumns
        } else {
            Self::Named(set)
        }
    }

    fn rank(&self) -> u8 {
        match self {
            Self::AnyColumn => 0,
            Self::Named(_) => 1,
            Self::AllColumns => 2,
            Self::TableWide => 3,
        }
    }

    fn merge_with(&mut self, other: Self) {
        match (&mut *self, other) {
            (Self::Named(mine), Self::Named(theirs)) => mine.extend(theirs),
            (slot, other) => {
                if other.rank() > slot.rank() {
                    *slot = other;
                }
            }
        }
    }
}

fn normalize_column(column: &str) -> String {
    column.trim_matches('"').to_ascii_lowercase()
}

/// One `(relation, privilege, columns)` requirement of a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanPrivilege {
    /// Relation key, as the planner resolved it.
    pub table: String,
    /// `SELECT` / `INSERT` / `UPDATE` / `DELETE`.
    pub privilege: &'static str,
    /// The columns touched under `privilege`.
    pub columns: RequiredColumns,
}

/// The decision context for one statement.
pub(crate) struct Gate<'a> {
    storage: &'a StorageEngine,
    snapshot: Arc<Snapshot>,
    principal: Principal,
    /// The role and every role it inherits from. Empty for an anonymous caller.
    identities: Vec<String>,
    superuser: bool,
    /// Names of the user-defined routines, read on first use.
    routines: std::cell::OnceCell<std::collections::HashSet<String>>,
}

impl<'a> Gate<'a> {
    /// The gate for the statement running on this thread, or `None` when
    /// nothing is checked (an unrestricted caller).
    pub(crate) fn current(storage: &'a StorageEngine) -> Result<Option<Self>> {
        if nothing_to_check() {
            return Ok(None);
        }
        let principal = current_principal(storage);
        if principal == Principal::Unrestricted {
            return Ok(None);
        }
        Self::for_principal(storage, principal).map(Some)
    }

    /// The gate for a given principal.
    pub(crate) fn for_principal(storage: &'a StorageEngine, principal: Principal) -> Result<Self> {
        let snapshot = catalog::snapshot(storage)?;
        let superuser = snapshot.is_superuser(&principal);
        let identities = match &principal {
            Principal::Role { name, .. } => snapshot.identities(name),
            _ => Vec::new(),
        };
        Ok(Self {
            storage,
            snapshot,
            principal,
            identities,
            superuser,
            routines: std::cell::OnceCell::new(),
        })
    }

    /// True if `name` (a routine key) is a user-defined function or procedure.
    /// A catalog that cannot be read answers true, so the routine is checked.
    pub(crate) fn is_routine(&self, name: &str) -> bool {
        if self.routines.get().is_none() {
            match catalog::routine_names(self.storage) {
                Ok(names) => {
                    let _ = self.routines.set(names);
                }
                Err(_) => return true,
            }
        }
        self.routines.get().is_some_and(|names| names.contains(name))
    }

    pub(crate) fn is_superuser(&self) -> bool {
        self.superuser
    }

    pub(crate) fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    pub(crate) fn principal(&self) -> &Principal {
        &self.principal
    }

    pub(crate) fn identities(&self) -> &[String] {
        &self.identities
    }

    fn role_name(&self) -> &str {
        self.principal.name().unwrap_or("")
    }

    fn is_anonymous(&self) -> bool {
        self.principal == Principal::Anonymous
    }

    /// The refusal for an object, with the reason when the session has no role.
    fn deny(&self, kind_word: &str, object: &str) -> Error {
        if self.is_anonymous() {
            return Error::query_execution(format!(
                "{PERMISSION_DENIED} for {kind_word} {}: {NO_ROLE_ON_CONNECTION}",
                display_name(object)
            ));
        }
        denied(kind_word, object)
    }

    /// `table`, `view` or `materialized view`, for messages.
    pub(crate) fn relation_word(&self, relation: &str) -> &'static str {
        relation_word(self.storage, relation)
    }

    /// True if the role (or a role it inherits from) owns the object.
    pub(crate) fn owns(&self, kind: &str, object: &str) -> bool {
        if self.superuser {
            return true;
        }
        let owner = self.snapshot.owner_of(kind, object);
        self.identities.iter().any(|identity| identity == owner)
    }

    /// Ownership of an object, or the refusal.
    pub(crate) fn require_owner(&self, kind: &str, object: &str, kind_word: &str) -> Result<()> {
        if self.owns(kind, object) {
            Ok(())
        } else if self.is_anonymous() {
            Err(self.deny(kind_word, object))
        } else {
            Err(not_owner(kind_word, object))
        }
    }

    /// Ownership of a relation that exists; a missing relation is left for the
    /// statement itself to report.
    fn require_relation_owner(&self, relation: &str) -> Result<()> {
        if self.superuser || is_temp_relation(relation) || !relation_exists(self.storage, relation) {
            return Ok(());
        }
        self.require_schema_usage(relation)?;
        self.require_owner(TABLE, relation, self.relation_word(relation))
    }

    /// A privilege on a non-relation object (sequence, schema, function,
    /// database): ownership, a grant, or PUBLIC's default.
    pub(crate) fn has_object_privilege(&self, kind: &str, object: &str, privilege: &str) -> bool {
        if self.superuser {
            return true;
        }
        if self.is_anonymous() {
            return false;
        }
        self.owns(kind, object)
            || self
                .snapshot
                .object_grant(kind, object, privilege, &self.identities)
                .is_some()
    }

    pub(crate) fn require_object_privilege(
        &self,
        kind: &str,
        object: &str,
        privilege: &str,
        kind_word: &str,
    ) -> Result<()> {
        if self.has_object_privilege(kind, object, privilege) {
            Ok(())
        } else {
            Err(self.deny(kind_word, object))
        }
    }

    /// `USAGE` on the schema a relation key lives in.
    fn require_schema_usage(&self, object: &str) -> Result<()> {
        let schema = schema_of(object);
        if is_temp_relation(schema) || self.has_object_privilege(SCHEMA, schema, "USAGE") {
            Ok(())
        } else {
            Err(self.deny("schema", schema))
        }
    }

    /// `CREATE` on the schema a new object's key lives in.
    fn require_schema_create(&self, object: &str) -> Result<()> {
        let schema = schema_of(object);
        if is_temp_relation(schema) || self.has_object_privilege(SCHEMA, schema, "CREATE") {
            Ok(())
        } else {
            Err(self.deny("schema", schema))
        }
    }

    /// Does `identities` hold `privilege` on `columns` of `relation`?
    fn relation_allowed(
        &self,
        identities: &[String],
        relation: &str,
        privilege: &str,
        columns: &RequiredColumns,
    ) -> bool {
        let owner = self.snapshot.owner_of(TABLE, relation);
        if identities.iter().any(|identity| identity == owner) {
            return true;
        }
        if self
            .snapshot
            .object_grant(TABLE, relation, privilege, identities)
            .is_some()
        {
            return true;
        }
        let granted = self.snapshot.granted_columns(relation, privilege, identities, false);
        if granted.is_empty() {
            return false;
        }
        match columns {
            RequiredColumns::AnyColumn => true,
            RequiredColumns::Named(needed) => needed.iter().all(|column| granted.contains(column)),
            RequiredColumns::AllColumns => match relation_columns(self.storage, relation) {
                Some(all) if !all.is_empty() => all.iter().all(|column| granted.contains(column)),
                _ => false,
            },
            RequiredColumns::TableWide => false,
        }
    }

    /// The caller's privilege on a relation, or the refusal.
    pub(crate) fn require_relation(&self, relation: &str, privilege: &str, columns: &RequiredColumns) -> Result<()> {
        if self.superuser || is_temp_relation(relation) {
            return Ok(());
        }
        if self.is_anonymous() {
            return Err(self.deny(self.relation_word(relation), relation));
        }
        self.require_schema_usage(relation)?;
        if self.relation_allowed(&self.identities, relation, privilege, columns) {
            Ok(())
        } else {
            Err(self.deny(self.relation_word(relation), relation))
        }
    }

    /// True if the caller holds `privilege` on the relation (for the
    /// `has_table_privilege` family).
    pub(crate) fn relation_privilege(&self, relation: &str, privilege: &str, columns: &RequiredColumns) -> bool {
        self.require_relation(relation, privilege, columns).is_ok()
    }

    /// What a view's owner must hold on a relation its body reads: `SELECT` on
    /// the whole relation (or ownership).
    fn require_for_definer(&self, view: &str, relation: &str) -> Result<()> {
        let owner = self.snapshot.owner_of(TABLE, view).to_string();
        if self.snapshot.role_is_superuser(&owner) || is_temp_relation(relation) {
            return Ok(());
        }
        let identities = self.snapshot.identities(&owner);
        if self.relation_allowed(&identities, relation, "SELECT", &RequiredColumns::TableWide) {
            Ok(())
        } else {
            Err(denied(self.relation_word(relation), relation))
        }
    }

    fn role_record(&self) -> Option<&crate::storage::RoleRecord> {
        self.snapshot.role(self.role_name())
    }

    fn has_createrole(&self) -> bool {
        self.superuser || self.role_record().is_some_and(|role| role.rolcreaterole)
    }

    fn has_createdb(&self) -> bool {
        self.superuser || self.role_record().is_some_and(|role| role.rolcreatedb)
    }

    fn require_superuser(&self, action: &str) -> Result<()> {
        if self.superuser {
            Ok(())
        } else {
            Err(denied_to(&format!("{action}: must be superuser")))
        }
    }

    /// Check one statement.
    pub(crate) fn statement(&self, plan: &LogicalPlan) -> Result<()> {
        if self.superuser {
            return Ok(());
        }
        use LogicalPlan as P;
        match plan {
            // -- Queries and DML: collected by the walker ---------------------
            P::Scan { .. }
            | P::FilteredScan { .. }
            | P::Filter { .. }
            | P::Project { .. }
            | P::Aggregate { .. }
            | P::Join { .. }
            | P::Sort { .. }
            | P::Limit { .. }
            | P::Union { .. }
            | P::Intersect { .. }
            | P::Except { .. }
            | P::With { .. }
            | P::SystemView { .. }
            | P::TableFunction { .. }
            | P::DualScan
            | P::Insert { .. }
            | P::InsertSelect { .. }
            | P::Update { .. }
            | P::Delete { .. }
            | P::Call { .. } => self.walk(plan),
            P::Explain { input, .. } => self.statement(input),
            P::Prepare { statement, .. } => self.statement(statement),

            // -- Nothing to check ---------------------------------------------
            P::StartTransaction
            | P::Commit
            | P::Rollback
            | P::Savepoint { .. }
            | P::ReleaseSavepoint { .. }
            | P::RollbackToSavepoint { .. }
            | P::Execute { .. }
            | P::Deallocate { .. }
            | P::SetConstraints { .. }
            | P::Noop
            | P::Notification(_)
            | P::UseBranch { .. }
            | P::ShowBranches => Ok(()),
            // Grants, role membership, SET ROLE and owner changes are decided
            // where they run, with the grantor's rights on each object.
            P::GrantPrivileges { .. } | P::RevokePrivileges { .. } | P::AccessControl(_) => Ok(()),
            // Server control: decided by the surface that runs it.
            #[cfg(feature = "ha-tier1")]
            P::Switchover { .. }
            | P::SwitchoverCheck { .. }
            | P::ClusterStatus
            | P::SetNodeAlias { .. }
            | P::ShowTopology => Ok(()),

            // -- Relations ----------------------------------------------------
            P::CreateTable {
                name,
                columns,
                constraints,
                ..
            } => {
                self.require_schema_create(name)?;
                for constraint in constraints {
                    if let TableConstraint::ForeignKey {
                        references_table,
                        references_columns,
                        ..
                    } = constraint
                    {
                        self.require_references(references_table, references_columns)?;
                    }
                }
                for column in columns {
                    if let Some(default) = &column.default {
                        self.walk_expr_only(default)?;
                    }
                }
                Ok(())
            }
            P::CreateTableAs { name, query, .. } => {
                self.require_schema_create(name)?;
                self.walk(query)
            }
            P::DropTable { name, .. } | P::DropTableCascade { name, .. } => self.require_relation_owner(name),
            P::DropMulti { drops } => drops.iter().try_for_each(|drop| self.statement(drop)),
            P::Truncate { table_name } => {
                if is_temp_relation(table_name) || !relation_exists(self.storage, table_name) {
                    return Ok(());
                }
                self.require_relation(table_name, "TRUNCATE", &RequiredColumns::TableWide)
            }
            P::CreateIndex { table_name, .. }
            | P::AlterColumnStorage { table_name, .. }
            | P::AlterTableAddColumn { table_name, .. }
            | P::AlterTableDropColumn { table_name, .. }
            | P::AlterTableRenameColumn { table_name, .. }
            | P::AlterTableAlterColumnNullability { table_name, .. }
            | P::AlterTableRename { table_name, .. }
            | P::AlterTableAlterConstraintEnforcement { table_name, .. }
            | P::AlterTableDropConstraint { table_name, .. }
            | P::AlterTableAddUnique { table_name, .. }
            | P::AlterTableAddPrimaryKey { table_name, .. }
            | P::AlterTableAddCheck { table_name, .. } => self.require_relation_owner(table_name),
            P::AlterTableSetSchema {
                table_name, new_schema, ..
            } => {
                self.require_relation_owner(table_name)?;
                self.require_object_privilege(SCHEMA, new_schema, "CREATE", "schema")
            }
            P::AlterTableAddForeignKey {
                table_name,
                references_table,
                references_columns,
                ..
            } => {
                self.require_relation_owner(table_name)?;
                self.require_references(references_table, references_columns)
            }
            P::AlterTableMulti { operations } => operations.iter().try_for_each(|op| self.statement(op)),
            P::DropIndex { name, .. } => {
                match self.storage.catalog().get_index_definition(name) {
                    Ok(Some(definition)) => self.require_relation_owner(&definition.table_name),
                    // An index the catalog does not know: the statement reports it.
                    Ok(None) => Ok(()),
                    Err(error) => Err(error),
                }
            }
            P::CreateTrigger { table_name, .. } => {
                if is_temp_relation(table_name) || !relation_exists(self.storage, table_name) {
                    return Ok(());
                }
                self.require_relation(table_name, "TRIGGER", &RequiredColumns::TableWide)
            }
            P::DropTrigger { name, table_name, .. } => match table_name {
                Some(table) => self.require_relation_owner(table),
                None => {
                    let triggers = self.storage.catalog().load_all_triggers()?;
                    for trigger in triggers.iter().filter(|trigger| trigger.name.eq_ignore_ascii_case(name)) {
                        self.require_relation_owner(&trigger.table_name)?;
                    }
                    Ok(())
                }
            },

            // -- Views --------------------------------------------------------
            P::CreateView { name, or_replace, .. } => {
                if *or_replace && relation_exists(self.storage, name) {
                    return self.require_relation_owner(name);
                }
                self.require_schema_create(name)
            }
            P::DropView { name, .. } => self.require_relation_owner(name),
            P::CreateMaterializedView { name, query, .. } => {
                self.require_schema_create(name)?;
                self.walk(query)
            }
            P::RefreshMaterializedView { name, .. }
            | P::DropMaterializedView { name, .. }
            | P::AlterMaterializedView { name, .. } => self.require_relation_owner(name),

            // -- Sequences ----------------------------------------------------
            P::CreateSequence { name, .. } => self.require_schema_create(name),
            P::AlterSequence(action) => self.require_existing_owner(SEQUENCE, &action.name, "sequence"),
            P::DropSequence { name, .. } => self.require_existing_owner(SEQUENCE, name, "sequence"),

            // -- Types, schemas, databases, extensions --------------------------
            P::CreateEnumType { name, .. } => self.require_schema_create(name),
            P::DropEnumType { name, .. } => Err(not_owner("type", name)),
            P::CreateSchema { name, .. } => {
                // IF NOT EXISTS on a schema the role already owns is a no-op.
                let already_own = self.storage.catalog().schema_exists(name).unwrap_or(false) && self.owns(SCHEMA, name);
                if already_own || self.has_object_privilege(DATABASE, DATABASE_NAME, "CREATE") {
                    Ok(())
                } else {
                    Err(self.deny("database", DATABASE_NAME))
                }
            }
            P::DropSchema { names, .. } => names.iter().try_for_each(|name| {
                if self.storage.catalog().schema_exists(name).unwrap_or(true) {
                    self.require_owner(SCHEMA, name, "schema")
                } else {
                    Ok(())
                }
            }),
            P::CreateDatabase { .. } => {
                if self.has_createdb() {
                    Ok(())
                } else {
                    Err(denied_to("create database"))
                }
            }
            P::DropDatabase { name, .. } => self.require_owner(DATABASE, name, "database"),
            P::CreateExtension { .. } => self.require_superuser("create extension"),
            P::DropExtension { .. } => self.require_superuser("drop extension"),

            // -- Routines -------------------------------------------------------
            P::CreateFunction { name, or_replace, .. } | P::CreateProcedure { name, or_replace, .. } => {
                let key = routine_key(name);
                if *or_replace && self.is_routine(&key) {
                    return self.require_owner(FUNCTION, &key, routine_word(plan));
                }
                self.require_schema_create(catalog::PUBLIC)
            }
            P::DropFunction { name, .. } | P::DropProcedure { name, .. } => {
                self.require_existing_owner(FUNCTION, &routine_key(name), routine_word(plan))
            }

            // -- Branches -------------------------------------------------------
            P::CreateBranch { .. } => self.require_superuser("create a branch"),
            P::DropBranch { .. } => self.require_superuser("drop a branch"),
            P::MergeBranch { .. } => self.require_superuser("merge a branch"),

            // -- Roles ----------------------------------------------------------
            P::CreateRole {
                superuser,
                replication,
                bypassrls,
                ..
            } => {
                if !self.has_createrole() {
                    return Err(denied_to("create role"));
                }
                if *superuser || *replication || *bypassrls {
                    return Err(denied_to(
                        "create role: only a superuser may create a role with SUPERUSER, REPLICATION or BYPASSRLS",
                    ));
                }
                Ok(())
            }
            P::AlterRole {
                name,
                set_login,
                set_superuser,
                set_createdb,
                set_createrole,
                set_inherit,
                set_replication,
                set_bypassrls,
                set_conn_limit,
                set_valid_until,
                set_password: _,
            } => {
                if set_superuser.is_some() || set_replication.is_some() || set_bypassrls.is_some() {
                    return Err(denied_to(
                        "alter role: only a superuser may change SUPERUSER, REPLICATION or BYPASSRLS",
                    ));
                }
                if self.snapshot.role_is_superuser(name) {
                    return Err(denied_to("alter role: only a superuser may alter a superuser"));
                }
                if self.has_createrole() {
                    return Ok(());
                }
                // A role may change its own password, nothing else.
                let only_password = set_login.is_none()
                    && set_createdb.is_none()
                    && set_createrole.is_none()
                    && set_inherit.is_none()
                    && set_conn_limit.is_none()
                    && set_valid_until.is_none();
                if only_password && name == self.role_name() {
                    Ok(())
                } else {
                    Err(denied_to("alter role"))
                }
            }
            P::DropRole { name, .. } => {
                if !self.has_createrole() {
                    return Err(denied_to("drop role"));
                }
                if self.snapshot.role_is_superuser(name) {
                    return Err(denied_to("drop role: only a superuser may drop a superuser"));
                }
                Ok(())
            }
        }
    }

    /// Ownership of a non-relation object, when it exists.
    fn require_existing_owner(&self, kind: &str, object: &str, kind_word: &str) -> Result<()> {
        let exists = match kind {
            SEQUENCE => self.storage.catalog().sequence_exists(object).unwrap_or(true),
            FUNCTION => self.is_routine(object),
            _ => true,
        };
        if !exists {
            return Ok(());
        }
        self.require_owner(kind, object, kind_word)
    }

    /// `REFERENCES` on the table a foreign key points at: on the whole table,
    /// or on every referenced column (its primary key when none is written).
    fn require_references(&self, parent: &str, written: &[String]) -> Result<()> {
        if is_temp_relation(parent) || !relation_exists(self.storage, parent) {
            return Ok(());
        }
        let columns = if written.is_empty() {
            match primary_key_columns(self.storage, parent) {
                Some(columns) if !columns.is_empty() => RequiredColumns::named(columns),
                _ => RequiredColumns::TableWide,
            }
        } else {
            RequiredColumns::named(written.iter().cloned())
        };
        self.require_relation(parent, "REFERENCES", &columns)
    }

    fn walk(&self, plan: &LogicalPlan) -> Result<()> {
        let mut walk = Walk::new(self);
        let scope = walk.plan(plan);
        walk.expose(&scope);
        walk.finish()
    }

    /// An expression outside any relation (a column default): only the
    /// subqueries and routines inside it matter.
    fn walk_expr_only(&self, expr: &LogicalExpr) -> Result<()> {
        let mut walk = Walk::new(self);
        walk.expr(expr, &Scope::default());
        walk.finish()
    }
}

fn routine_word(plan: &LogicalPlan) -> &'static str {
    match plan {
        LogicalPlan::CreateProcedure { .. } | LogicalPlan::DropProcedure { .. } => "procedure",
        _ => "function",
    }
}

/// The key a routine's grants and owner are recorded under.
pub(crate) fn routine_key(name: &str) -> String {
    let bare = name.rsplit_once('.').map_or(name, |(_, bare)| bare);
    bare.trim_matches('"').to_ascii_lowercase()
}

/// `table`, `view` or `materialized view`.
pub(crate) fn relation_word(storage: &StorageEngine, relation: &str) -> &'static str {
    if storage.view_catalog().view_exists(relation).unwrap_or(false) {
        "view"
    } else if storage.mv_catalog().view_exists(relation).unwrap_or(false) {
        "materialized view"
    } else {
        "table"
    }
}

/// True if a table, view or materialized view has this key.
pub(crate) fn relation_exists(storage: &StorageEngine, relation: &str) -> bool {
    storage.catalog().table_exists(relation).unwrap_or(false)
        || storage.view_catalog().view_exists(relation).unwrap_or(false)
        || storage.mv_catalog().view_exists(relation).unwrap_or(false)
}

/// Lower-cased column names of a relation, when the catalog knows them.
pub(crate) fn relation_columns(storage: &StorageEngine, relation: &str) -> Option<Vec<String>> {
    if let Ok(schema) = storage.catalog().get_table_schema(relation) {
        return Some(schema.columns.iter().map(|c| c.name.to_ascii_lowercase()).collect());
    }
    storage
        .view_catalog()
        .get_view(relation)
        .ok()
        .map(|view| view.schema.columns.iter().map(|c| c.name.to_ascii_lowercase()).collect())
}

fn primary_key_columns(storage: &StorageEngine, relation: &str) -> Option<Vec<String>> {
    let schema = storage.catalog().get_table_schema(relation).ok()?;
    Some(
        schema
            .columns
            .iter()
            .filter(|column| column.primary_key)
            .map(|column| column.name.to_ascii_lowercase())
            .collect(),
    )
}

/// Check a plan against the role running the current statement.
pub fn check_plan(storage: &StorageEngine, plan: &LogicalPlan) -> Result<()> {
    match Gate::current(storage)? {
        Some(gate) => gate.statement(plan),
        None => Ok(()),
    }
}

/// Check one privilege on one relation (bulk load paths that never plan).
pub fn check_relation(storage: &StorageEngine, relation: &str, privilege: &str, columns: &[String]) -> Result<()> {
    match Gate::current(storage)? {
        Some(gate) => {
            let columns = if columns.is_empty() {
                RequiredColumns::AllColumns
            } else {
                RequiredColumns::named(columns.iter().cloned())
            };
            gate.require_relation(relation, privilege, &columns)
        }
        None => Ok(()),
    }
}

/// Check a sequence function call (`nextval` needs USAGE or UPDATE, `currval`
/// USAGE or SELECT, `setval` UPDATE) for the statement running on this thread.
pub fn check_sequence_call(function: &str, sequence: &str) -> Result<()> {
    if nothing_to_check() {
        return Ok(());
    }
    let Some(storage) = crate::sql::sequences::current_engine() else {
        return Ok(());
    };
    let Some(gate) = Gate::current(&storage)? else {
        return Ok(());
    };
    if gate.is_superuser() {
        return Ok(());
    }
    let key = sequence.trim_matches('"');
    let accepted: &[&str] = match function {
        "nextval" => &["USAGE", "UPDATE"],
        "currval" | "lastval" => &["USAGE", "SELECT"],
        _ => &["UPDATE"],
    };
    if accepted
        .iter()
        .any(|privilege| gate.has_object_privilege(SEQUENCE, key, privilege))
    {
        Ok(())
    } else {
        Err(gate.deny("sequence", key))
    }
}

// ---------------------------------------------------------------------------
// The plan walk.
// ---------------------------------------------------------------------------

/// One relation visible for column resolution at a query level.
#[derive(Debug, Clone)]
struct ScopeRel {
    /// The relation charged for a column read through this entry. `None` for
    /// a CTE reference, a derived table, a system view, a table function or a
    /// scan planned inside a view body: nothing there is charged to the caller.
    table: Option<String>,
    /// The name a qualified reference uses, lower-cased.
    qualifier: String,
    /// Output column names, lower-cased. Empty means "not known".
    columns: Vec<String>,
}

/// One query level's name-resolution environment.
#[derive(Debug, Default, Clone)]
struct Scope {
    rels: Vec<ScopeRel>,
    /// Names that resolve here without charging a relation: projection
    /// aliases. The columns behind them were charged where the expression
    /// itself was collected.
    free_names: Vec<String>,
    /// The level sits above an aggregate: a name that matches no relation is
    /// one of the aggregate's outputs, whose inputs were charged below.
    synthetic: bool,
    /// Relations whose whole rows still flow upward: nothing has projected
    /// them yet. If they reach the top of a query unprojected, the query
    /// returns every column of them.
    raw: Vec<String>,
}

impl Scope {
    fn absorb(&mut self, other: Scope) {
        self.rels.extend(other.rels);
        self.free_names.extend(other.free_names);
        self.synthetic |= other.synthetic;
        self.raw.extend(other.raw);
    }
}

struct Walk<'g, 'a> {
    gate: &'g Gate<'a>,
    out: Vec<PlanPrivilege>,
    /// The first refusal met on something that is not a caller's relation
    /// privilege (a view owner's rights, a routine, a schema).
    refusal: Option<Error>,
    /// CTE names in scope, with their output columns.
    ctes: Vec<(String, Vec<String>)>,
    /// Enclosing query levels, outermost first.
    outer: Vec<Scope>,
}

impl<'g, 'a> Walk<'g, 'a> {
    fn new(gate: &'g Gate<'a>) -> Self {
        Self {
            gate,
            out: Vec::new(),
            refusal: None,
            ctes: Vec::new(),
            outer: Vec::new(),
        }
    }

    fn finish(self) -> Result<()> {
        if let Some(refusal) = self.refusal {
            return Err(refusal);
        }
        for need in &self.out {
            self.gate.require_relation(&need.table, need.privilege, &need.columns)?;
        }
        Ok(())
    }

    fn refuse(&mut self, result: Result<()>) {
        if let Err(error) = result {
            if self.refusal.is_none() {
                self.refusal = Some(error);
            }
        }
    }

    fn require(&mut self, table: &str, privilege: &'static str, columns: RequiredColumns) {
        if let Some(slot) = self
            .out
            .iter_mut()
            .find(|need| need.privilege == privilege && need.table == table)
        {
            slot.columns.merge_with(columns);
            return;
        }
        self.out.push(PlanPrivilege {
            table: table.to_string(),
            privilege,
            columns,
        });
    }

    fn free_rel(qualifier: String, columns: Vec<String>) -> Scope {
        Scope {
            rels: vec![ScopeRel {
                table: None,
                qualifier,
                columns,
            }],
            free_names: Vec::new(),
            synthetic: false,
            raw: Vec::new(),
        }
    }

    fn leaf_relation(&mut self, table_name: &str, alias: Option<&str>, schema: &Arc<crate::Schema>) -> Scope {
        let columns: Vec<String> = schema.columns.iter().map(|c| c.name.to_ascii_lowercase()).collect();
        let qualifier = normalize_column(alias.unwrap_or_else(|| display_name(table_name)));
        if self.ctes.iter().any(|(name, _)| name.eq_ignore_ascii_case(table_name)) {
            return Self::free_rel(qualifier, columns);
        }
        if crate::sql::phase3::SystemViewRegistry::shared().is_system_view(table_name) {
            return Self::free_rel(qualifier, columns);
        }
        if let Some(chain) = super::views::chain_of(schema) {
            // Planned inside a view body: the innermost view's owner answers
            // for the base relation, each outer view's owner for the view it
            // reads, and the caller for the outermost view only.
            let mut inner = table_name.to_string();
            for view in &chain {
                let result = self.gate.require_for_definer(view, &inner);
                self.refuse(result);
                inner = view.clone();
            }
            self.require(&inner, "SELECT", RequiredColumns::TableWide);
            return Self::free_rel(qualifier, columns);
        }
        self.require(table_name, "SELECT", RequiredColumns::AnyColumn);
        Scope {
            rels: vec![ScopeRel {
                table: Some(table_name.to_string()),
                qualifier,
                columns,
            }],
            free_names: Vec::new(),
            synthetic: false,
            raw: vec![table_name.to_string()],
        }
    }

    /// The rows of this level leave the query (or a subquery) as they are:
    /// every column of a relation nothing projected is read.
    fn expose(&mut self, scope: &Scope) {
        for table in &scope.raw {
            self.require(table, "SELECT", RequiredColumns::AllColumns);
        }
    }

    /// `*` in a select list: every column of every relation of the level.
    fn expose_all(&mut self, scope: &Scope) {
        for rel in &scope.rels {
            if let Some(table) = &rel.table {
                self.require(table, "SELECT", RequiredColumns::AllColumns);
            }
        }
    }

    /// The scope of a DML target, whose plan node carries no schema.
    fn target_scope(&self, target: &str) -> Scope {
        Scope {
            rels: vec![ScopeRel {
                table: Some(target.to_string()),
                qualifier: normalize_column(display_name(target)),
                columns: relation_columns(self.gate.storage, target).unwrap_or_default(),
            }],
            free_names: Vec::new(),
            synthetic: false,
            raw: Vec::new(),
        }
    }

    /// `RETURNING` reads the affected rows: its columns need SELECT.
    fn returning(&mut self, returning: &Option<Vec<ReturningItem>>, target: &str, scope: &Scope) {
        let Some(items) = returning else { return };
        if items.is_empty() {
            self.require(target, "SELECT", RequiredColumns::AllColumns);
            return;
        }
        for item in items {
            match item {
                ReturningItem::Wildcard => self.require(target, "SELECT", RequiredColumns::AllColumns),
                ReturningItem::Column(name) => {
                    let bare = name.rsplit_once('.').map_or(name.as_str(), |(_, bare)| bare);
                    self.require(target, "SELECT", RequiredColumns::one(bare));
                }
                ReturningItem::Expression { expr, .. } => self.expr(expr, scope),
            }
        }
    }

    fn routine(&mut self, name: &str, kind_word: &str) {
        let key = routine_key(name);
        if self.gate.is_routine(&key) {
            let result = self
                .gate
                .require_object_privilege(FUNCTION, &key, "EXECUTE", kind_word);
            self.refuse(result);
        }
    }

    /// Walk one plan node and return the scope it exposes to the node above.
    fn plan(&mut self, plan: &LogicalPlan) -> Scope {
        use LogicalPlan as P;
        match plan {
            P::Scan {
                table_name,
                alias,
                schema,
                ..
            } => self.leaf_relation(table_name, alias.as_deref(), schema),
            P::FilteredScan {
                table_name,
                alias,
                schema,
                predicate,
                ..
            } => {
                let scope = self.leaf_relation(table_name, alias.as_deref(), schema);
                if let Some(predicate) = predicate {
                    self.expr(predicate, &scope);
                }
                scope
            }
            P::DualScan => Scope::default(),
            P::SystemView { name, args } => {
                let enclosing = Scope::default();
                for arg in args {
                    self.expr(arg, &enclosing);
                }
                Self::free_rel(normalize_column(name), Vec::new())
            }
            P::TableFunction {
                function_name,
                args,
                alias,
                ..
            } => {
                let enclosing = Scope::default();
                for arg in args {
                    self.expr(arg, &enclosing);
                }
                self.routine(function_name, "function");
                Self::free_rel(
                    normalize_column(alias.as_deref().unwrap_or(function_name.as_str())),
                    Vec::new(),
                )
            }
            P::Filter { input, predicate } => {
                let scope = self.plan(input);
                self.expr(predicate, &scope);
                scope
            }
            P::Project {
                input,
                exprs,
                aliases,
                distinct_on,
                source_alias,
                ..
            } => {
                let mut scope = self.plan(input);
                for expr in exprs {
                    if matches!(expr, LogicalExpr::Wildcard) {
                        self.expose_all(&scope);
                    }
                    self.expr(expr, &scope);
                }
                // From here up only the projected expressions flow.
                scope.raw.clear();
                if let Some(keys) = distinct_on {
                    let mut with_aliases = scope.clone();
                    with_aliases
                        .free_names
                        .extend(aliases.iter().map(|alias| normalize_column(alias)));
                    for key in keys {
                        self.expr(key, &with_aliases);
                    }
                }
                let outputs: Vec<String> = aliases.iter().map(|alias| normalize_column(alias)).collect();
                match source_alias {
                    // A derived table: the level above sees only its outputs.
                    Some(alias) => Self::free_rel(normalize_column(alias), outputs),
                    None => {
                        scope.free_names.extend(outputs);
                        scope
                    }
                }
            }
            P::Aggregate {
                input,
                group_by,
                aggr_exprs,
                having,
            } => {
                let mut scope = self.plan(input);
                for expr in group_by.iter().chain(aggr_exprs.iter()) {
                    self.expr(expr, &scope);
                }
                scope.synthetic = true;
                scope.raw.clear();
                if let Some(having) = having {
                    self.expr(having, &scope);
                }
                scope
            }
            P::Join {
                left,
                right,
                on,
                lateral,
                ..
            } => {
                let mut scope = self.plan(left);
                let right_scope = if *lateral {
                    self.outer.push(scope.clone());
                    let right_scope = self.plan(right);
                    self.outer.pop();
                    right_scope
                } else {
                    self.plan(right)
                };
                scope.absorb(right_scope);
                if let Some(on) = on {
                    self.expr(on, &scope);
                }
                scope
            }
            P::Sort { input, exprs, .. } => {
                let scope = self.plan(input);
                for expr in exprs {
                    self.expr(expr, &scope);
                }
                scope
            }
            P::Limit { input, .. } => self.plan(input),
            P::Union { left, right, .. } | P::Intersect { left, right, .. } | P::Except { left, right, .. } => {
                let mut scope = self.plan(left);
                let right_scope = self.plan(right);
                scope.absorb(right_scope);
                scope
            }
            P::With { ctes, query, recursive } => {
                let before = self.ctes.len();
                for (name, cte, column_names) in ctes {
                    let columns = |plan: &LogicalPlan| -> Vec<String> {
                        match column_names {
                            Some(names) => names.iter().map(|name| normalize_column(name)).collect(),
                            None => plan
                                .schema()
                                .columns
                                .iter()
                                .map(|c| c.name.to_ascii_lowercase())
                                .collect(),
                        }
                    };
                    if *recursive {
                        // The body may name itself.
                        self.ctes.push((name.clone(), columns(cte)));
                        let body = self.plan(cte);
                        self.expose(&body);
                    } else {
                        let body = self.plan(cte);
                        self.expose(&body);
                        self.ctes.push((name.clone(), columns(cte)));
                    }
                }
                let scope = self.plan(query);
                self.ctes.truncate(before);
                scope
            }

            // -- DML ----------------------------------------------------------
            P::Insert {
                table_name,
                columns,
                values,
                returning,
                on_conflict,
            } => {
                self.insert_target(table_name, columns.as_deref());
                let empty = Scope::default();
                for row in values {
                    for value in row {
                        self.expr(value, &empty);
                    }
                }
                let scope = self.target_scope(table_name);
                self.on_conflict(on_conflict.as_ref(), table_name, &scope);
                self.returning(returning, table_name, &scope);
                Scope::default()
            }
            P::InsertSelect {
                table_name,
                columns,
                source,
                returning,
                on_conflict,
            } => {
                self.insert_target(table_name, columns.as_deref());
                let source_scope = self.plan(source);
                self.expose(&source_scope);
                let scope = self.target_scope(table_name);
                self.on_conflict(on_conflict.as_ref(), table_name, &scope);
                self.returning(returning, table_name, &scope);
                Scope::default()
            }
            P::Update {
                table_name,
                assignments,
                selection,
                returning,
            } => {
                let scope = self.target_scope(table_name);
                self.require(
                    table_name,
                    "UPDATE",
                    RequiredColumns::named(assignments.iter().map(|(column, _)| column.clone())),
                );
                for (_, value) in assignments {
                    self.expr(value, &scope);
                }
                if let Some(selection) = selection {
                    self.expr(selection, &scope);
                }
                self.returning(returning, table_name, &scope);
                Scope::default()
            }
            P::Delete {
                table_name,
                selection,
                returning,
            } => {
                let scope = self.target_scope(table_name);
                self.require(table_name, "DELETE", RequiredColumns::TableWide);
                if let Some(selection) = selection {
                    self.expr(selection, &scope);
                }
                self.returning(returning, table_name, &scope);
                Scope::default()
            }
            P::Call { name, args } => {
                let empty = Scope::default();
                for arg in args {
                    self.expr(arg, &empty);
                }
                self.routine(name, "procedure");
                Scope::default()
            }
            P::Explain { input, .. } => self.plan(input),
            // A statement that is not a query, met inside one (never produced
            // by the planner): checked as a statement of its own.
            other => {
                let result = self.gate.statement(other);
                self.refuse(result);
                Scope::default()
            }
        }
    }

    fn insert_target(&mut self, table: &str, columns: Option<&[String]>) {
        let required = match columns {
            Some(columns) if !columns.is_empty() => RequiredColumns::named(columns.iter().cloned()),
            _ => RequiredColumns::AllColumns,
        };
        self.require(table, "INSERT", required);
    }

    fn on_conflict(&mut self, on_conflict: Option<&OnConflictAction>, table: &str, scope: &Scope) {
        let Some(OnConflictAction::DoUpdate {
            assignments,
            selection,
            conflict_target,
        }) = on_conflict
        else {
            return;
        };
        self.require(
            table,
            "UPDATE",
            RequiredColumns::named(assignments.iter().map(|(column, _)| column.clone())),
        );
        if let Some(target) = conflict_target {
            if !target.is_empty() {
                self.require(table, "SELECT", RequiredColumns::named(target.iter().cloned()));
            }
        }
        // `EXCLUDED.x` names the proposed row, not a stored one.
        let mut with_excluded = scope.clone();
        with_excluded.rels.push(ScopeRel {
            table: None,
            qualifier: "excluded".to_string(),
            columns: Vec::new(),
        });
        for (_, value) in assignments {
            self.expr(value, &with_excluded);
        }
        if let Some(selection) = selection {
            self.expr(selection, &with_excluded);
        }
    }

    fn subquery(&mut self, plan: &LogicalPlan, enclosing: &Scope) {
        self.outer.push(enclosing.clone());
        let scope = self.plan(plan);
        self.expose(&scope);
        self.outer.pop();
    }

    /// Collect the columns, subqueries and routines of one expression. No `_`
    /// arm: a new expression kind has to be classified here to compile.
    fn expr(&mut self, expr: &LogicalExpr, scope: &Scope) {
        use LogicalExpr as E;
        match expr {
            E::Column { table, name } | E::BoundColumn { table, name, .. } => {
                self.resolve_column(table.as_deref(), name, scope);
            }
            E::Literal(_)
            | E::Parameter { .. }
            | E::DefaultValue
            | E::Wildcard
            | E::NewRow { .. }
            | E::OldRow { .. } => {}
            E::BinaryExpr { left, right, .. } => {
                self.expr(left, scope);
                self.expr(right, scope);
            }
            E::UnaryExpr { expr: inner, .. } | E::Cast { expr: inner, .. } | E::IsNull { expr: inner, .. } => {
                self.expr(inner, scope);
            }
            E::AggregateFunction { args, .. } => {
                for arg in args {
                    self.expr(arg, scope);
                }
            }
            E::ScalarFunction { fun, args } => {
                self.routine(fun, "function");
                for arg in args {
                    self.expr(arg, scope);
                }
            }
            E::WindowFunction {
                args,
                partition_by,
                order_by,
                ..
            } => {
                for arg in args.iter().chain(partition_by.iter()) {
                    self.expr(arg, scope);
                }
                for (key, _) in order_by {
                    self.expr(key, scope);
                }
            }
            E::Case {
                expr: base,
                when_then,
                else_result,
            } => {
                if let Some(base) = base {
                    self.expr(base, scope);
                }
                for (when, then) in when_then {
                    self.expr(when, scope);
                    self.expr(then, scope);
                }
                if let Some(else_result) = else_result {
                    self.expr(else_result, scope);
                }
            }
            E::Between {
                expr: inner, low, high, ..
            } => {
                self.expr(inner, scope);
                self.expr(low, scope);
                self.expr(high, scope);
            }
            E::InList { expr: inner, list, .. } => {
                self.expr(inner, scope);
                for item in list {
                    self.expr(item, scope);
                }
            }
            E::InSet { expr: inner, .. } => self.expr(inner, scope),
            E::Tuple { items } => {
                for item in items {
                    self.expr(item, scope);
                }
            }
            E::ArraySubscript { array, index } => {
                self.expr(array, scope);
                self.expr(index, scope);
            }
            E::InSubquery {
                expr: inner, subquery, ..
            } => {
                self.expr(inner, scope);
                self.subquery(subquery, scope);
            }
            E::ScalarSubquery { subquery } | E::Exists { subquery, .. } => self.subquery(subquery, scope),
        }
    }

    /// Attribute one column reference to the relation it reads and charge
    /// SELECT on it.
    fn resolve_column(&mut self, qualifier: Option<&str>, name: &str, scope: &Scope) {
        let wanted = normalize_column(name);
        if wanted == "*" {
            // `t.*` (or a bare `*` that reached here as a column): every
            // column of the relations it names.
            let qualifier = qualifier.map(|qualifier| normalize_column(display_name(qualifier)));
            let tables: Vec<String> = scope
                .rels
                .iter()
                .filter(|rel| qualifier.as_ref().is_none_or(|qualifier| &rel.qualifier == qualifier))
                .filter_map(|rel| rel.table.clone())
                .collect();
            for table in tables {
                self.require(&table, "SELECT", RequiredColumns::AllColumns);
            }
            return;
        }
        let levels: Vec<Scope> = std::iter::once(scope.clone())
            .chain(self.outer.iter().rev().cloned())
            .collect();

        // 1. A written qualifier: the alias, else the relation's own name. The
        //    innermost level wins.
        if let Some(qualifier) = qualifier {
            let qualifier = normalize_column(display_name(qualifier));
            for level in &levels {
                let matches: Vec<&ScopeRel> = level.rels.iter().filter(|rel| rel.qualifier == qualifier).collect();
                if matches.is_empty() {
                    continue;
                }
                for rel in matches {
                    if let Some(table) = &rel.table {
                        self.require(table, "SELECT", RequiredColumns::one(&wanted));
                    }
                }
                return;
            }
        }

        // 2. By column name. More than one match: charge all of them.
        for level in &levels {
            let matches: Vec<&ScopeRel> = level.rels.iter().filter(|rel| rel.columns.contains(&wanted)).collect();
            if matches.is_empty() {
                continue;
            }
            for rel in matches {
                if let Some(table) = &rel.table {
                    self.require(table, "SELECT", RequiredColumns::one(&wanted));
                }
            }
            return;
        }

        // 3. A projection alias: nothing is read through the name itself.
        if levels.iter().any(|level| level.free_names.contains(&wanted)) {
            return;
        }

        // 4. The name matches nothing known. A real relation whose columns the
        //    catalog could not list may own it: only a table-level grant will do.
        let unknown: Vec<String> = levels
            .iter()
            .flat_map(|level| level.rels.iter())
            .filter(|rel| rel.columns.is_empty())
            .filter_map(|rel| rel.table.clone())
            .collect();
        if !unknown.is_empty() {
            for table in unknown {
                self.require(&table, "SELECT", RequiredColumns::TableWide);
            }
            return;
        }

        // 5. Every real relation in scope has a known column list and none has
        //    this name, so the name reads no stored column if it belongs to an
        //    aggregate's outputs or to a relation that is never charged.
        let absorbed = levels.iter().any(|level| {
            level.synthetic
                || level
                    .rels
                    .iter()
                    .any(|rel| rel.table.is_none() && rel.columns.is_empty())
        });
        if absorbed {
            return;
        }

        // 6. Unattributable: fail closed on every relation in sight.
        let all: Vec<String> = levels
            .iter()
            .flat_map(|level| level.rels.iter())
            .filter_map(|rel| rel.table.clone())
            .collect();
        for table in all {
            self.require(&table, "SELECT", RequiredColumns::TableWide);
        }
    }
}
