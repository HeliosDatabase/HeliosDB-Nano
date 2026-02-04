//! Constraint Types and Validation
//!
//! This module provides support for table constraints including:
//! - Foreign Key constraints with IMMEDIATE/DEFERRED/LOCK-FREE modes
//! - Constraint validation during INSERT/UPDATE/DELETE operations

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use crate::{Error, Result, Value, Schema};

/// Referential action for ON DELETE / ON UPDATE clauses
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReferentialAction {
    /// No action - fail if referenced rows exist
    NoAction,
    /// Restrict - same as NoAction but checked immediately
    Restrict,
    /// Cascade - delete/update referencing rows
    Cascade,
    /// Set Null - set foreign key columns to NULL
    SetNull,
    /// Set Default - set foreign key columns to their default values
    SetDefault,
}

impl Default for ReferentialAction {
    fn default() -> Self {
        ReferentialAction::NoAction
    }
}

impl std::fmt::Display for ReferentialAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReferentialAction::NoAction => write!(f, "NO ACTION"),
            ReferentialAction::Restrict => write!(f, "RESTRICT"),
            ReferentialAction::Cascade => write!(f, "CASCADE"),
            ReferentialAction::SetNull => write!(f, "SET NULL"),
            ReferentialAction::SetDefault => write!(f, "SET DEFAULT"),
        }
    }
}

/// Constraint enforcement mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ConstraintEnforcement {
    /// Check constraint on each statement (default)
    #[default]
    Immediate,
    /// Check constraint at transaction COMMIT
    Deferred,
    /// Async validation for bulk operations (eventual consistency)
    LockFree,
}

impl std::fmt::Display for ConstraintEnforcement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConstraintEnforcement::Immediate => write!(f, "IMMEDIATE"),
            ConstraintEnforcement::Deferred => write!(f, "DEFERRED"),
            ConstraintEnforcement::LockFree => write!(f, "LOCK-FREE"),
        }
    }
}

/// Foreign Key constraint definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForeignKeyConstraint {
    /// Constraint name (optional, auto-generated if not provided)
    pub name: String,
    /// Table containing the foreign key
    pub table_name: String,
    /// Foreign key columns in this table
    pub columns: Vec<String>,
    /// Referenced table name
    pub references_table: String,
    /// Referenced columns (must be PRIMARY KEY or UNIQUE)
    pub references_columns: Vec<String>,
    /// Action on DELETE of referenced row
    pub on_delete: ReferentialAction,
    /// Action on UPDATE of referenced row
    pub on_update: ReferentialAction,
    /// Whether constraint is deferrable
    pub deferrable: bool,
    /// If deferrable, whether initially deferred
    pub initially_deferred: bool,
    /// Enforcement mode (runtime configurable)
    pub enforcement: ConstraintEnforcement,
}

impl ForeignKeyConstraint {
    /// Create a new foreign key constraint with default options
    pub fn new(
        name: String,
        table_name: String,
        columns: Vec<String>,
        references_table: String,
        references_columns: Vec<String>,
    ) -> Self {
        Self {
            name,
            table_name,
            columns,
            references_table,
            references_columns,
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
            deferrable: false,
            initially_deferred: false,
            enforcement: ConstraintEnforcement::Immediate,
        }
    }

    /// Set ON DELETE action
    pub fn on_delete(mut self, action: ReferentialAction) -> Self {
        self.on_delete = action;
        self
    }

    /// Set ON UPDATE action
    pub fn on_update(mut self, action: ReferentialAction) -> Self {
        self.on_update = action;
        self
    }

    /// Make constraint deferrable
    pub fn deferrable(mut self, initially_deferred: bool) -> Self {
        self.deferrable = true;
        self.initially_deferred = initially_deferred;
        self
    }

    /// Set enforcement mode
    pub fn with_enforcement(mut self, enforcement: ConstraintEnforcement) -> Self {
        self.enforcement = enforcement;
        self
    }

    /// Generate automatic constraint name
    pub fn generate_name(table: &str, columns: &[String], references_table: &str) -> String {
        let cols = columns.join("_");
        format!("fk_{}_{}__{}", table, cols, references_table)
    }
}

/// Check constraint definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckConstraint {
    /// Constraint name
    pub name: String,
    /// Table name
    pub table_name: String,
    /// SQL expression that must evaluate to true
    pub expression: String,
    /// Parsed expression (cached for efficiency)
    #[serde(skip)]
    pub parsed_expression: Option<crate::sql::LogicalExpr>,
}

impl CheckConstraint {
    pub fn new(name: String, table_name: String, expression: String) -> Self {
        Self {
            name,
            table_name,
            expression,
            parsed_expression: None,
        }
    }
}

/// Unique constraint definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UniqueConstraint {
    /// Constraint name
    pub name: String,
    /// Table name
    pub table_name: String,
    /// Columns that must be unique together
    pub columns: Vec<String>,
    /// Whether this is a PRIMARY KEY constraint
    pub is_primary_key: bool,
}

impl UniqueConstraint {
    pub fn new(name: String, table_name: String, columns: Vec<String>, is_primary_key: bool) -> Self {
        Self {
            name,
            table_name,
            columns,
            is_primary_key,
        }
    }
}

/// All constraints for a table
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TableConstraints {
    /// Foreign key constraints
    pub foreign_keys: Vec<ForeignKeyConstraint>,
    /// Check constraints
    pub check_constraints: Vec<CheckConstraint>,
    /// Unique constraints (including PRIMARY KEY)
    pub unique_constraints: Vec<UniqueConstraint>,
}

impl TableConstraints {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_foreign_key(&mut self, fk: ForeignKeyConstraint) {
        self.foreign_keys.push(fk);
    }

    pub fn add_check(&mut self, check: CheckConstraint) {
        self.check_constraints.push(check);
    }

    pub fn add_unique(&mut self, unique: UniqueConstraint) {
        self.unique_constraints.push(unique);
    }

    /// Find a constraint by name
    pub fn find_by_name(&self, name: &str) -> Option<ConstraintRef<'_>> {
        for fk in &self.foreign_keys {
            if fk.name == name {
                return Some(ConstraintRef::ForeignKey(fk));
            }
        }
        for check in &self.check_constraints {
            if check.name == name {
                return Some(ConstraintRef::Check(check));
            }
        }
        for unique in &self.unique_constraints {
            if unique.name == name {
                return Some(ConstraintRef::Unique(unique));
            }
        }
        None
    }
}

/// Reference to a constraint (for lookups)
pub enum ConstraintRef<'a> {
    ForeignKey(&'a ForeignKeyConstraint),
    Check(&'a CheckConstraint),
    Unique(&'a UniqueConstraint),
}

/// Pending constraint check for deferred validation
#[derive(Debug, Clone)]
pub struct PendingConstraintCheck {
    /// Constraint name
    pub constraint_name: String,
    /// Table name
    pub table_name: String,
    /// Operation type (INSERT, UPDATE, DELETE)
    pub operation: ConstraintOperation,
    /// Row data to validate
    pub row_key: Vec<Value>,
}

/// Operation that triggered the constraint check
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintOperation {
    Insert,
    Update,
    Delete,
}

/// Deferred constraint tracker for transactions
#[derive(Debug, Default)]
pub struct DeferredConstraintTracker {
    /// Pending constraint checks keyed by transaction ID
    pending: HashMap<u64, Vec<PendingConstraintCheck>>,
}

impl DeferredConstraintTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a pending constraint check for a transaction
    pub fn add_pending(&mut self, txn_id: u64, check: PendingConstraintCheck) {
        self.pending.entry(txn_id).or_default().push(check);
    }

    /// Get all pending checks for a transaction
    pub fn get_pending(&self, txn_id: u64) -> &[PendingConstraintCheck] {
        self.pending.get(&txn_id).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Clear pending checks for a transaction (on COMMIT success or ROLLBACK)
    pub fn clear(&mut self, txn_id: u64) {
        self.pending.remove(&txn_id);
    }

    /// Check if there are pending checks for a transaction
    pub fn has_pending(&self, txn_id: u64) -> bool {
        self.pending.get(&txn_id).map(|v| !v.is_empty()).unwrap_or(false)
    }
}

/// Foreign Key validator
pub struct ForeignKeyValidator {
    /// Track tables we're currently validating to detect cycles
    validation_stack: HashSet<String>,
}

impl ForeignKeyValidator {
    pub fn new() -> Self {
        Self {
            validation_stack: HashSet::new(),
        }
    }

    /// Validate foreign key constraint for an INSERT operation
    /// Returns Ok(()) if the referenced row exists, Error otherwise
    pub fn validate_insert(
        &mut self,
        fk: &ForeignKeyConstraint,
        fk_values: &[Value],
        referenced_schema: &Schema,
        check_reference_exists: impl FnOnce(&str, &[String], &[Value]) -> Result<bool>,
    ) -> Result<()> {
        // NULL values are allowed (unless NOT NULL is specified on the column)
        if fk_values.iter().any(|v| matches!(v, Value::Null)) {
            return Ok(());
        }

        // Check for circular reference
        if self.validation_stack.contains(&fk.references_table) {
            return Err(Error::constraint_violation(format!(
                "Circular reference detected in foreign key constraint '{}'",
                fk.name
            )));
        }

        self.validation_stack.insert(fk.table_name.clone());

        // Check if the referenced row exists
        let exists = check_reference_exists(
            &fk.references_table,
            &fk.references_columns,
            fk_values,
        )?;

        self.validation_stack.remove(&fk.table_name);

        if !exists {
            return Err(Error::constraint_violation(format!(
                "Foreign key constraint '{}' violated: referenced row in table '{}' does not exist",
                fk.name, fk.references_table
            )));
        }

        Ok(())
    }

    /// Validate foreign key constraint for a DELETE operation on the referenced table
    /// Returns the referential action to take, or Error if violation
    pub fn validate_delete(
        &self,
        fk: &ForeignKeyConstraint,
        deleted_values: &[Value],
        check_referencing_exists: impl FnOnce(&str, &[String], &[Value]) -> Result<bool>,
    ) -> Result<ReferentialAction> {
        // Check if any rows in the FK table reference this row
        let has_references = check_referencing_exists(
            &fk.table_name,
            &fk.columns,
            deleted_values,
        )?;

        if !has_references {
            return Ok(ReferentialAction::NoAction);
        }

        // There are referencing rows, apply the ON DELETE action
        match fk.on_delete {
            ReferentialAction::NoAction | ReferentialAction::Restrict => {
                Err(Error::constraint_violation(format!(
                    "Foreign key constraint '{}' violated: cannot delete row from '{}' - referenced by '{}'",
                    fk.name, fk.references_table, fk.table_name
                )))
            }
            action => Ok(action), // CASCADE, SET NULL, SET DEFAULT - caller handles these
        }
    }

    /// Validate foreign key constraint for an UPDATE operation on the referenced table
    /// Returns the referential action to take, or Error if violation
    pub fn validate_update(
        &self,
        fk: &ForeignKeyConstraint,
        old_values: &[Value],
        new_values: &[Value],
        check_referencing_exists: impl FnOnce(&str, &[String], &[Value]) -> Result<bool>,
    ) -> Result<ReferentialAction> {
        // If referenced columns didn't change, no action needed
        if old_values == new_values {
            return Ok(ReferentialAction::NoAction);
        }

        // Check if any rows in the FK table reference the old values
        let has_references = check_referencing_exists(
            &fk.table_name,
            &fk.columns,
            old_values,
        )?;

        if !has_references {
            return Ok(ReferentialAction::NoAction);
        }

        // There are referencing rows, apply the ON UPDATE action
        match fk.on_update {
            ReferentialAction::NoAction | ReferentialAction::Restrict => {
                Err(Error::constraint_violation(format!(
                    "Foreign key constraint '{}' violated: cannot update row in '{}' - referenced by '{}'",
                    fk.name, fk.references_table, fk.table_name
                )))
            }
            action => Ok(action), // CASCADE, SET NULL, SET DEFAULT - caller handles these
        }
    }
}

impl Default for ForeignKeyValidator {
    fn default() -> Self {
        Self::new()
    }
}

/// Lock-free constraint validation queue for bulk operations
#[derive(Debug)]
pub struct LockFreeValidationQueue {
    /// Queue of pending validations
    queue: Vec<LockFreeValidation>,
    /// Maximum queue size before forcing synchronous validation
    max_queue_size: usize,
}

#[derive(Debug, Clone)]
pub struct LockFreeValidation {
    /// Foreign key constraint
    pub constraint_name: String,
    /// Table name
    pub table_name: String,
    /// Rows to validate
    pub row_keys: Vec<Vec<Value>>,
    /// Timestamp when queued
    pub queued_at: std::time::Instant,
}

impl LockFreeValidationQueue {
    pub fn new(max_queue_size: usize) -> Self {
        Self {
            queue: Vec::new(),
            max_queue_size,
        }
    }

    /// Queue a validation for async processing
    pub fn enqueue(&mut self, validation: LockFreeValidation) -> bool {
        if self.queue.len() >= self.max_queue_size {
            return false; // Queue full, must validate synchronously
        }
        self.queue.push(validation);
        true
    }

    /// Drain the queue and return all pending validations
    pub fn drain(&mut self) -> Vec<LockFreeValidation> {
        std::mem::take(&mut self.queue)
    }

    /// Get queue length
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Check if queue is empty
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

impl Default for LockFreeValidationQueue {
    fn default() -> Self {
        Self::new(1000)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_foreign_key_constraint_creation() {
        let fk = ForeignKeyConstraint::new(
            "fk_orders_customer".to_string(),
            "orders".to_string(),
            vec!["customer_id".to_string()],
            "customers".to_string(),
            vec!["id".to_string()],
        )
        .on_delete(ReferentialAction::Cascade)
        .on_update(ReferentialAction::Restrict)
        .deferrable(true);

        assert_eq!(fk.name, "fk_orders_customer");
        assert_eq!(fk.on_delete, ReferentialAction::Cascade);
        assert_eq!(fk.on_update, ReferentialAction::Restrict);
        assert!(fk.deferrable);
        assert!(fk.initially_deferred);
    }

    #[test]
    fn test_generate_constraint_name() {
        let name = ForeignKeyConstraint::generate_name(
            "orders",
            &["customer_id".to_string()],
            "customers",
        );
        assert_eq!(name, "fk_orders_customer_id__customers");
    }

    #[test]
    fn test_table_constraints() {
        let mut constraints = TableConstraints::new();

        constraints.add_foreign_key(ForeignKeyConstraint::new(
            "fk_test".to_string(),
            "orders".to_string(),
            vec!["customer_id".to_string()],
            "customers".to_string(),
            vec!["id".to_string()],
        ));

        assert!(constraints.find_by_name("fk_test").is_some());
        assert!(constraints.find_by_name("nonexistent").is_none());
    }

    #[test]
    fn test_deferred_constraint_tracker() {
        let mut tracker = DeferredConstraintTracker::new();

        let check = PendingConstraintCheck {
            constraint_name: "fk_test".to_string(),
            table_name: "orders".to_string(),
            operation: ConstraintOperation::Insert,
            row_key: vec![Value::Int4(1)],
        };

        tracker.add_pending(1, check);
        assert!(tracker.has_pending(1));
        assert!(!tracker.has_pending(2));

        tracker.clear(1);
        assert!(!tracker.has_pending(1));
    }

    #[test]
    fn test_lock_free_validation_queue() {
        let mut queue = LockFreeValidationQueue::new(2);

        let validation = LockFreeValidation {
            constraint_name: "fk_test".to_string(),
            table_name: "orders".to_string(),
            row_keys: vec![vec![Value::Int4(1)]],
            queued_at: std::time::Instant::now(),
        };

        assert!(queue.enqueue(validation.clone()));
        assert!(queue.enqueue(validation.clone()));
        assert!(!queue.enqueue(validation)); // Queue full

        let drained = queue.drain();
        assert_eq!(drained.len(), 2);
        assert!(queue.is_empty());
    }

    #[test]
    fn test_referential_action_display() {
        assert_eq!(ReferentialAction::NoAction.to_string(), "NO ACTION");
        assert_eq!(ReferentialAction::Cascade.to_string(), "CASCADE");
        assert_eq!(ReferentialAction::SetNull.to_string(), "SET NULL");
    }
}
