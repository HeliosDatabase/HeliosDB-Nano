//! Keeping owners and grants in step with the objects they describe.
//!
//! The gate runs before a statement; this runs after it, and only when the
//! statement succeeded:
//!
//! * a created object gets its creator as owner, and starts with no grants
//!   (whatever a dropped object of the same name once had is forgotten);
//! * a dropped object's grants and owner are forgotten;
//! * a renamed table or column takes its grants along.
//!
//! What the statement will change is worked out from its plan when it is
//! admitted ([`admit`]); [`run_admitted`] wraps the execution and settles the
//! change afterwards. Statements nest (a routine body runs statements of its
//! own), so the pending change is kept in a stack of frames.

use std::cell::RefCell;

use crate::sql::LogicalPlan;
use crate::storage::StorageEngine;
use crate::Result;

use super::access::{current_principal, Principal};
use super::catalog::{self, FUNCTION, SCHEMA, SEQUENCE, TABLE};
use super::gate;
use super::statement::object_exists;

/// What a statement will change in the privilege catalog if it succeeds.
#[derive(Debug, Default)]
struct Pending {
    /// The role recorded as owner of what is created (`None`: the bootstrap
    /// superuser, which needs no record).
    owner: Option<String>,
    /// `(kind, object)` that did not exist when the statement was admitted.
    creates: Vec<(&'static str, String)>,
    /// `(kind, object)` the statement drops.
    drops: Vec<(&'static str, String)>,
    schema_drops: Vec<String>,
    /// `(old key, new key candidates)` of a renamed or moved table.
    renames: Vec<(String, Vec<String>)>,
    column_drops: Vec<(String, String)>,
    column_renames: Vec<(String, String, String)>,
}

impl Pending {
    fn is_empty(&self) -> bool {
        self.creates.is_empty()
            && self.drops.is_empty()
            && self.schema_drops.is_empty()
            && self.renames.is_empty()
            && self.column_drops.is_empty()
            && self.column_renames.is_empty()
    }
}

thread_local! {
    static FRAMES: RefCell<Vec<Option<Pending>>> = const { RefCell::new(Vec::new()) };
}

/// Note an object the statement is about to create. One that exists already
/// (`IF NOT EXISTS`, `OR REPLACE`) keeps its owner and its grants.
fn note_create(storage: &StorageEngine, pending: &mut Pending, kind: &'static str, name: &str) {
    if !object_exists(storage, kind, name).unwrap_or(true) {
        pending.creates.push((kind, name.to_string()));
    }
}

fn collect(storage: &StorageEngine, plan: &LogicalPlan, pending: &mut Pending) {
    use LogicalPlan as P;
    match plan {
        P::CreateTable { name, .. }
        | P::CreateTableAs { name, .. }
        | P::CreateView { name, .. }
        | P::CreateMaterializedView { name, .. } => note_create(storage, pending, TABLE, name),
        P::CreateSequence { name, .. } => note_create(storage, pending, SEQUENCE, name),
        P::CreateSchema { name, .. } => note_create(storage, pending, SCHEMA, name),
        P::CreateFunction { name, .. } | P::CreateProcedure { name, .. } => {
            note_create(storage, pending, FUNCTION, &gate::routine_key(name));
        }
        P::DropTable { name, .. }
        | P::DropTableCascade { name, .. }
        | P::DropView { name, .. }
        | P::DropMaterializedView { name, .. } => pending.drops.push((TABLE, name.clone())),
        P::DropSequence { name, .. } => pending.drops.push((SEQUENCE, name.clone())),
        P::DropFunction { name, .. } | P::DropProcedure { name, .. } => {
            pending.drops.push((FUNCTION, gate::routine_key(name)));
        }
        P::DropSchema { names, .. } => pending.schema_drops.extend(names.iter().cloned()),
        P::DropMulti { drops } => {
            for drop in drops {
                collect(storage, drop, pending);
            }
        }
        P::AlterTableMulti { operations } => {
            for operation in operations {
                collect(storage, operation, pending);
            }
        }
        P::AlterTableRename {
            table_name,
            new_table_name,
        } => {
            let mut candidates = Vec::new();
            if let Some((schema, _)) = table_name.split_once('.') {
                if !new_table_name.contains('.') {
                    candidates.push(format!("{schema}.{new_table_name}"));
                }
            }
            candidates.push(new_table_name.clone());
            pending.renames.push((table_name.clone(), candidates));
        }
        P::AlterTableSetSchema {
            table_name, new_schema, ..
        } => {
            let bare = table_name.rsplit_once('.').map_or(table_name.as_str(), |(_, bare)| bare);
            let moved = if new_schema == catalog::PUBLIC {
                bare.to_string()
            } else {
                format!("{new_schema}.{bare}")
            };
            pending.renames.push((table_name.clone(), vec![moved]));
        }
        P::AlterTableDropColumn {
            table_name, column_name, ..
        } => pending
            .column_drops
            .push((table_name.clone(), column_name.to_ascii_lowercase())),
        P::AlterTableRenameColumn {
            table_name,
            old_column_name,
            new_column_name,
        } => pending.column_renames.push((
            table_name.clone(),
            old_column_name.to_ascii_lowercase(),
            new_column_name.to_ascii_lowercase(),
        )),
        _ => {}
    }
}

fn pending_for(storage: &StorageEngine, plan: &LogicalPlan) -> Option<Pending> {
    let mut pending = Pending::default();
    collect(storage, plan, &mut pending);
    if pending.is_empty() {
        return None;
    }
    if !pending.creates.is_empty() {
        pending.owner = match current_principal(storage) {
            Principal::Role { name, operator: false } if !crate::storage::is_reserved_role_name(&name) => {
                Some(name.to_string())
            }
            _ => None,
        };
    }
    Some(pending)
}

fn settle(storage: &StorageEngine, pending: &Pending) -> Result<()> {
    let catalog = storage.catalog();
    for (kind, object) in &pending.creates {
        if !object_exists(storage, kind, object)? {
            continue;
        }
        catalog::forget_object(&catalog, kind, object)?;
        if let Some(owner) = &pending.owner {
            catalog::set_owner(&catalog, kind, object, owner)?;
        }
    }
    for (kind, object) in &pending.drops {
        if !object_exists(storage, kind, object)? {
            catalog::forget_object(&catalog, kind, object)?;
        }
    }
    for schema in &pending.schema_drops {
        if !catalog.schema_exists(schema)? {
            catalog::forget_schema(&catalog, schema)?;
        }
    }
    for (old, candidates) in &pending.renames {
        if gate::relation_exists(storage, old) {
            continue;
        }
        if let Some(new) = candidates.iter().find(|new| gate::relation_exists(storage, new)) {
            catalog::rename_object(&catalog, TABLE, old, new)?;
        }
    }
    for (table, column) in &pending.column_drops {
        let still_there = gate::relation_columns(storage, table).is_some_and(|columns| columns.contains(column));
        if !still_there {
            catalog::forget_column(&catalog, table, column)?;
        }
    }
    for (table, old, new) in &pending.column_renames {
        let renamed = gate::relation_columns(storage, table)
            .is_some_and(|columns| columns.contains(new) && !columns.contains(old));
        if renamed {
            catalog::rename_column(&catalog, table, old, new)?;
        }
    }
    Ok(())
}

/// Check `plan` against the current role and note what it will change. Call
/// inside [`run_admitted`], once the statement's plan is known.
pub fn admit(storage: &StorageEngine, plan: &LogicalPlan) -> Result<()> {
    gate::check_plan(storage, plan)?;
    let pending = pending_for(storage, plan);
    FRAMES.with(|frames| {
        if let Some(top) = frames.borrow_mut().last_mut() {
            *top = pending;
        }
    });
    Ok(())
}

/// Run one statement's execution. With `plan`, the statement is admitted
/// first; without, the body admits it itself once it has planned. When the
/// body succeeds, the privilege catalog is brought in step with what it did.
pub fn run_admitted<T>(
    storage: &StorageEngine,
    plan: Option<&LogicalPlan>,
    body: impl FnOnce() -> Result<T>,
) -> Result<T> {
    FRAMES.with(|frames| frames.borrow_mut().push(None));
    let admitted = match plan {
        Some(plan) => admit(storage, plan),
        None => Ok(()),
    };
    let result = match admitted {
        Ok(()) => body(),
        Err(error) => Err(error),
    };
    let pending = FRAMES.with(|frames| frames.borrow_mut().pop()).flatten();
    match (result, pending) {
        (Ok(value), Some(pending)) => {
            settle(storage, &pending)?;
            Ok(value)
        }
        (result, _) => result,
    }
}
