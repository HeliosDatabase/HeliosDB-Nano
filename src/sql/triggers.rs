//! Trigger registry and management
//!
//! This module provides trigger storage, persistence, and retrieval for HeliosDB-Lite.
//! Triggers are stored in-memory and persisted to the catalog for durability across sessions.

use super::evaluator::Evaluator;
use super::logical_plan::{
    LogicalExpr, LogicalPlan, TransitionTable, TriggerCharacteristics, TriggerEvent, TriggerFor, TriggerTiming,
    TriggerType,
};
use crate::{Error, Result, Schema, Tuple, Value};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, RwLock};

/// Maximum cascading depth for trigger execution (PostgreSQL compatible)
pub const MAX_TRIGGER_DEPTH: usize = 16;

/// Trigger metadata and definition
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriggerDefinition {
    /// Trigger name
    pub name: String,
    /// Table name this trigger is attached to
    pub table_name: String,
    /// Trigger timing (BEFORE, AFTER, INSTEAD OF)
    pub timing: TriggerTiming,
    /// Trigger events (INSERT, UPDATE, DELETE)
    pub events: Vec<TriggerEvent>,
    /// For each row or statement
    pub for_each: TriggerFor,
    /// Optional WHEN clause condition
    pub when_condition: Option<Box<LogicalExpr>>,
    /// Trigger body statements
    pub body: Vec<LogicalPlan>,
    /// Whether trigger is enabled
    pub enabled: bool,
    /// Creation timestamp (Unix epoch milliseconds)
    pub created_at: u64,
    /// REFERENCING clause: transition table aliases for statement-level triggers
    pub referencing: Vec<TransitionTable>,
    /// DEFERRABLE characteristics (deferrable, initially deferred)
    pub characteristics: TriggerCharacteristics,
    /// Trigger type (Regular or Constraint)
    pub trigger_type: TriggerType,
    /// Referenced constraint name (for CONSTRAINT triggers with FROM clause)
    pub from_constraint: Option<String>,
}

/// Transition tables for statement-level trigger execution
/// Holds all affected rows for OLD TABLE and NEW TABLE references
#[derive(Debug, Clone, Default)]
pub struct TransitionTables {
    /// OLD TABLE rows (for UPDATE/DELETE triggers)
    pub old_rows: Vec<Tuple>,
    /// NEW TABLE rows (for INSERT/UPDATE triggers)
    pub new_rows: Vec<Tuple>,
    /// Alias for OLD TABLE (from REFERENCING clause)
    pub old_table_alias: Option<String>,
    /// Alias for NEW TABLE (from REFERENCING clause)
    pub new_table_alias: Option<String>,
}

impl TransitionTables {
    /// Create new empty transition tables
    pub fn new() -> Self {
        Self::default()
    }

    /// Create transition tables from row contexts (collects all rows)
    pub fn from_row_contexts(contexts: &[TriggerRowContext]) -> Self {
        let mut old_rows = Vec::new();
        let mut new_rows = Vec::new();

        for ctx in contexts {
            if let Some(old) = &ctx.old_tuple {
                old_rows.push(old.clone());
            }
            if let Some(new) = &ctx.new_tuple {
                new_rows.push(new.clone());
            }
        }

        Self {
            old_rows,
            new_rows,
            old_table_alias: None,
            new_table_alias: None,
        }
    }

    /// Set the OLD TABLE alias from REFERENCING clause
    pub fn with_old_table_alias(mut self, alias: String) -> Self {
        self.old_table_alias = Some(alias);
        self
    }

    /// Set the NEW TABLE alias from REFERENCING clause
    pub fn with_new_table_alias(mut self, alias: String) -> Self {
        self.new_table_alias = Some(alias);
        self
    }

    /// Check if this has an OLD TABLE
    pub fn has_old_table(&self) -> bool {
        self.old_table_alias.is_some() && !self.old_rows.is_empty()
    }

    /// Check if this has a NEW TABLE
    pub fn has_new_table(&self) -> bool {
        self.new_table_alias.is_some() && !self.new_rows.is_empty()
    }

    /// Get rows for a given alias (returns None if alias doesn't match)
    pub fn get_rows_by_alias(&self, alias: &str) -> Option<&Vec<Tuple>> {
        if self.old_table_alias.as_deref() == Some(alias) {
            Some(&self.old_rows)
        } else if self.new_table_alias.as_deref() == Some(alias) {
            Some(&self.new_rows)
        } else {
            None
        }
    }
}

impl TriggerDefinition {
    /// Create a new trigger definition with default (non-deferrable) characteristics
    pub fn new(
        name: String,
        table_name: String,
        timing: TriggerTiming,
        events: Vec<TriggerEvent>,
        for_each: TriggerFor,
        when_condition: Option<Box<LogicalExpr>>,
        body: Vec<LogicalPlan>,
        referencing: Vec<TransitionTable>,
    ) -> Self {
        Self {
            name,
            table_name,
            timing,
            events,
            for_each,
            when_condition,
            body,
            enabled: true,
            created_at: Self::current_timestamp(),
            referencing,
            characteristics: TriggerCharacteristics::default(),
            trigger_type: TriggerType::default(),
            from_constraint: None,
        }
    }

    /// Create a new trigger definition with custom characteristics
    pub fn new_with_characteristics(
        name: String,
        table_name: String,
        timing: TriggerTiming,
        events: Vec<TriggerEvent>,
        for_each: TriggerFor,
        when_condition: Option<Box<LogicalExpr>>,
        body: Vec<LogicalPlan>,
        referencing: Vec<TransitionTable>,
        characteristics: TriggerCharacteristics,
    ) -> Self {
        Self {
            name,
            table_name,
            timing,
            events,
            for_each,
            when_condition,
            body,
            enabled: true,
            created_at: Self::current_timestamp(),
            referencing,
            characteristics,
            trigger_type: TriggerType::default(),
            from_constraint: None,
        }
    }

    /// Create a constraint trigger with deferred execution
    pub fn new_constraint_trigger(
        name: String,
        table_name: String,
        timing: TriggerTiming,
        events: Vec<TriggerEvent>,
        for_each: TriggerFor,
        when_condition: Option<Box<LogicalExpr>>,
        body: Vec<LogicalPlan>,
        referencing: Vec<TransitionTable>,
        characteristics: TriggerCharacteristics,
        from_constraint: Option<String>,
    ) -> Self {
        Self {
            name,
            table_name,
            timing,
            events,
            for_each,
            when_condition,
            body,
            enabled: true,
            created_at: Self::current_timestamp(),
            referencing,
            characteristics,
            trigger_type: TriggerType::Constraint,
            from_constraint,
        }
    }

    /// Check if this trigger is deferrable
    pub fn is_deferrable(&self) -> bool {
        // Constraint triggers are always deferrable
        self.trigger_type == TriggerType::Constraint || self.characteristics.deferrable
    }

    /// Check if this trigger is initially deferred
    pub fn is_initially_deferred(&self) -> bool {
        // Constraint triggers are always initially deferred unless explicitly set otherwise
        if self.trigger_type == TriggerType::Constraint {
            // If not explicitly set to immediate, default to deferred
            !self.characteristics.initially_deferred || self.characteristics.deferrable
        } else {
            self.characteristics.initially_deferred
        }
    }

    /// Check if this is a constraint trigger
    pub fn is_constraint_trigger(&self) -> bool {
        self.trigger_type == TriggerType::Constraint
    }

    /// Get current timestamp in milliseconds
    fn current_timestamp() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Check if trigger matches a specific event type
    pub fn matches_event(&self, event: &TriggerEvent) -> bool {
        self.events.iter().any(|e| {
            match (e, event) {
                (TriggerEvent::Insert, TriggerEvent::Insert) => true,
                (TriggerEvent::Delete, TriggerEvent::Delete) => true,
                (TriggerEvent::Truncate, TriggerEvent::Truncate) => true,
                (TriggerEvent::Update(None), TriggerEvent::Update(_)) => true,
                (TriggerEvent::Update(Some(cols1)), TriggerEvent::Update(Some(cols2))) => {
                    // Check if any of the updated columns match
                    cols2.iter().any(|c| cols1.contains(c))
                }
                (TriggerEvent::Update(Some(_)), TriggerEvent::Update(None)) => true,
                _ => false,
            }
        })
    }

    /// Check if trigger matches timing
    pub fn matches_timing(&self, timing: &TriggerTiming) -> bool {
        &self.timing == timing
    }
}

/// Trigger execution context for tracking cascading depth
#[derive(Debug, Clone)]
pub struct TriggerContext {
    /// Current execution depth
    pub depth: usize,
    /// Stack of trigger names being executed
    pub trigger_stack: Vec<String>,
}

impl TriggerContext {
    /// Create a new trigger context
    pub fn new() -> Self {
        Self {
            depth: 0,
            trigger_stack: Vec::new(),
        }
    }

    /// Enter a trigger execution (increment depth)
    pub fn enter(&mut self, trigger_name: &str) -> Result<()> {
        if self.depth >= MAX_TRIGGER_DEPTH {
            return Err(Error::query_execution(format!(
                "Maximum trigger cascading depth ({}) exceeded. Trigger chain: {}",
                MAX_TRIGGER_DEPTH,
                self.trigger_stack.join(" -> ")
            )));
        }
        self.depth += 1;
        self.trigger_stack.push(trigger_name.to_string());
        Ok(())
    }

    /// Exit a trigger execution (decrement depth)
    pub fn exit(&mut self) {
        if self.depth > 0 {
            self.depth -= 1;
            self.trigger_stack.pop();
        }
    }

    /// Get current depth
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// Check if we're at maximum depth
    pub fn at_max_depth(&self) -> bool {
        self.depth >= MAX_TRIGGER_DEPTH
    }
}

impl Default for TriggerContext {
    fn default() -> Self {
        Self::new()
    }
}

/// A BEFORE-row trigger-function effect for the common
/// `NEW.<col> = <expr>; … RETURN NEW|NULL;` pattern. Resolved from the
/// invoked function's body at CREATE TRIGGER time and applied to the NEW
/// tuple before the row is written.
///
/// PERSISTED, but deliberately NOT as part of `TriggerDefinition`. The
/// definition struct is bincode-POSITIONAL, lives in the `trigger:{table}:{name}`
/// catalog keys AND in `WalOperation::CreateTrigger`, and is replicated to
/// standbys — adding a field to it would break replay of every record written
/// by an older build. This recipe therefore gets its own additive sidecar key
/// namespace, `trigger_rowmut:{table}:{name}` (see
/// `Catalog::save_trigger_row_mutation`).
///
/// It has to be persisted rather than rebuilt at open, because the recipe is
/// derived from the trigger FUNCTION's body and the function registry is
/// rebuilt from `meta:function:` records that a plain `CREATE FUNCTION …
/// RETURNS TRIGGER` may not have (and, historically, never had) — the compiled
/// recipe is the only thing guaranteed to survive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerRowMutation {
    /// Events the owning trigger fires on (INSERT / UPDATE).
    pub events: Vec<TriggerEvent>,
    /// `NEW.<col> = <expr>` assignments, applied in order.
    pub assignments: Vec<(String, LogicalExpr)>,
    /// Body contains `RETURN NULL` ⇒ skip the row entirely.
    pub returns_null: bool,
}

/// Trigger registry for managing trigger definitions
pub struct TriggerRegistry {
    /// In-memory trigger storage keyed by (table_name, trigger_name)
    triggers: Arc<RwLock<HashMap<(String, String), TriggerDefinition>>>,
    /// BEFORE-row NEW-mutation recipes keyed by (table_name, trigger_name).
    /// Loaded from the catalog at open and updated by CREATE/DROP TRIGGER.
    row_mutations: Arc<RwLock<HashMap<(String, String), TriggerRowMutation>>>,
    /// `row_mutations.len()`, mirrored into an atomic.
    ///
    /// PERF (write path): `has_row_mutations` / `apply_before_row_mutations`
    /// sit on the DML INSERT path of BOTH executor families. When no trigger
    /// anywhere defines a row-rewrite recipe — the overwhelmingly common case —
    /// this lets them answer with a single relaxed-ish atomic load and never
    /// touch the `RwLock` at all, so a trigger-free database pays no lock
    /// traffic for the feature. Always written under the `row_mutations` write
    /// guard, so it can never disagree with the map.
    row_mutation_count: Arc<AtomicUsize>,
    /// Storage key prefix for triggers
    storage_prefix: &'static str,
}

impl TriggerRegistry {
    /// Create a new trigger registry
    pub fn new() -> Self {
        Self {
            triggers: Arc::new(RwLock::new(HashMap::new())),
            row_mutations: Arc::new(RwLock::new(HashMap::new())),
            row_mutation_count: Arc::new(AtomicUsize::new(0)),
            storage_prefix: "trigger:",
        }
    }

    /// Register a BEFORE-row NEW-mutation recipe for a trigger.
    pub fn register_row_mutation(&self, table_name: &str, trigger_name: &str, mutation: TriggerRowMutation) {
        if let Ok(mut guard) = self.row_mutations.write() {
            guard.insert((table_name.to_string(), trigger_name.to_string()), mutation);
            self.row_mutation_count.store(guard.len(), AtomicOrdering::Release);
        }
    }

    /// Forget a trigger's NEW-mutation recipe (on DROP TRIGGER).
    pub fn drop_row_mutation(&self, table_name: &str, trigger_name: &str) {
        if let Ok(mut guard) = self.row_mutations.write() {
            guard.remove(&(table_name.to_string(), trigger_name.to_string()));
            self.row_mutation_count.store(guard.len(), AtomicOrdering::Release);
        }
    }

    /// Whether any BEFORE-row mutation recipe is registered for `table_name`.
    ///
    /// Hoist this OUT of per-row loops: it is a per-statement gate, not a
    /// per-row one.
    pub fn has_row_mutations(&self, table_name: &str) -> bool {
        if self.row_mutation_count.load(AtomicOrdering::Acquire) == 0 {
            return false;
        }
        self.row_mutations
            .read()
            .map(|g| g.keys().any(|(tbl, _)| tbl == table_name))
            .unwrap_or(false)
    }

    /// Evaluate `expr` against the pending NEW row.
    ///
    /// ONE evaluator construction shared by the `WHEN` predicate and by every
    /// `NEW.<col> = <rhs>` right-hand side, so the two can never resolve
    /// `NEW.` differently.
    fn eval_against_new_row(expr: &LogicalExpr, new_tuple: &Tuple, schema: &Arc<Schema>) -> Result<Value> {
        let ctx = TriggerRowContext::for_insert(new_tuple.clone());
        let evaluator = Evaluator::with_trigger_row_context(schema.clone(), Vec::new(), ctx, schema.clone());
        let empty = Tuple::new(Vec::new());
        evaluator.evaluate(expr, &empty)
    }

    /// Apply BEFORE-row trigger-function mutations (`NEW.col = expr`) to
    /// `new_tuple` in place for `event`. Returns `Ok(false)` if a trigger body
    /// did `RETURN NULL` (the row should be skipped). Unknown target columns
    /// and non-evaluable expressions are skipped rather than erroring, so an
    /// unsupported body never breaks the DML.
    ///
    /// Gating (this is the whole of what "the trigger fired" means today —
    /// real body execution is NOT implemented):
    /// * a recipe whose owning `TriggerDefinition` is `enabled == false` is
    ///   skipped;
    /// * a recipe whose owning definition carries a `WHEN` predicate is skipped
    ///   unless that predicate evaluates to `TRUE` against the NEW row. FALSE
    ///   and NULL mean "not fired" (PostgreSQL semantics). An UNEVALUABLE
    ///   predicate also means "not fired" and is logged at debug rather than
    ///   raised: this mechanism is a best-effort approximation of a trigger
    ///   body, and an INSERT that succeeds today must not start failing because
    ///   the approximation cannot understand a predicate.
    /// * recipes fire in trigger-NAME order, as PostgreSQL does for triggers of
    ///   the same timing on the same table.
    pub fn apply_before_row_mutations(
        &self,
        table_name: &str,
        event: &TriggerEvent,
        new_tuple: &mut Tuple,
        schema: Arc<Schema>,
    ) -> Result<bool> {
        // Free when no recipe exists anywhere: one atomic load, no lock.
        if self.row_mutation_count.load(AtomicOrdering::Acquire) == 0 {
            return Ok(true);
        }

        let mut applicable: Vec<(String, TriggerRowMutation)> = match self.row_mutations.read() {
            Ok(guard) => guard
                .iter()
                // Match by event kind, ignoring `Update`'s column-list payload
                // (`Update(None)` at CREATE time vs `Update(Some(cols))` at fire time).
                .filter(|((tbl, _), m)| {
                    tbl == table_name
                        && m.events
                            .iter()
                            .any(|e| std::mem::discriminant(e) == std::mem::discriminant(event))
                })
                .map(|((_, trigger), m)| (trigger.clone(), m.clone()))
                .collect(),
            Err(_) => return Ok(true),
        };
        if applicable.is_empty() {
            return Ok(true);
        }
        applicable.sort_by(|a, b| a.0.cmp(&b.0));

        // `enabled` and `WHEN` live on the DEFINITION, not on the recipe —
        // resolved here so the two can never disagree.
        let gates: HashMap<String, (bool, Option<Box<LogicalExpr>>)> = match self.triggers.read() {
            Ok(guard) => guard
                .iter()
                .filter(|((tbl, _), _)| tbl == table_name)
                .map(|((_, trigger), def)| (trigger.clone(), (def.enabled, def.when_condition.clone())))
                .collect(),
            Err(_) => return Ok(true),
        };

        for (trigger_name, m) in &applicable {
            // A recipe whose definition is gone does not fire. (The two are
            // dropped together; this is defence in depth, not an expected path.)
            let Some((enabled, when)) = gates.get(trigger_name) else {
                continue;
            };
            if !*enabled {
                continue;
            }
            if let Some(cond) = when {
                match Self::eval_against_new_row(cond, new_tuple, &schema) {
                    Ok(Value::Boolean(true)) => {}
                    Ok(_) => continue,
                    Err(e) => {
                        tracing::debug!(
                            "trigger '{}' on '{}': WHEN clause could not be evaluated ({}) — treated as not fired",
                            trigger_name,
                            table_name,
                            e
                        );
                        continue;
                    }
                }
            }
            if m.returns_null {
                return Ok(false);
            }
            for (col, expr) in &m.assignments {
                let Some(idx) = schema.columns.iter().position(|c| c.name.eq_ignore_ascii_case(col)) else {
                    continue; // unknown column — ignore
                };
                // Evaluate the RHS against the current NEW row (so later
                // assignments can see earlier ones).
                match Self::eval_against_new_row(expr, new_tuple, &schema) {
                    Ok(value) => {
                        if idx < new_tuple.values.len() {
                            new_tuple.values[idx] = value;
                        }
                    }
                    Err(_) => continue, // non-evaluable RHS — leave column unchanged
                }
            }
        }
        Ok(true)
    }

    /// Register a new trigger
    ///
    /// # Arguments
    ///
    /// * `definition` - Trigger definition to register
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - A trigger with the same name already exists on the table
    /// - Failed to acquire write lock
    pub fn register_trigger(&self, definition: TriggerDefinition) -> Result<()> {
        let key = (definition.table_name.clone(), definition.name.clone());

        let mut triggers = self
            .triggers
            .write()
            .map_err(|e| Error::Generic(format!("Failed to acquire write lock: {}", e)))?;

        if triggers.contains_key(&key) {
            return Err(Error::query_execution(format!(
                "Trigger '{}' already exists on table '{}'",
                definition.name, definition.table_name
            )));
        }

        triggers.insert(key, definition);
        Ok(())
    }

    /// Drop a trigger
    ///
    /// # Arguments
    ///
    /// * `table_name` - Table name
    /// * `trigger_name` - Trigger name
    ///
    /// # Returns
    ///
    /// Returns `Ok(true)` if trigger was found and removed, `Ok(false)` if not found
    pub fn drop_trigger(&self, table_name: &str, trigger_name: &str) -> Result<bool> {
        let key = (table_name.to_string(), trigger_name.to_string());

        let mut triggers = self
            .triggers
            .write()
            .map_err(|e| Error::Generic(format!("Failed to acquire write lock: {}", e)))?;

        Ok(triggers.remove(&key).is_some())
    }

    /// Get a specific trigger
    ///
    /// # Arguments
    ///
    /// * `table_name` - Table name
    /// * `trigger_name` - Trigger name
    ///
    /// # Returns
    ///
    /// Returns the trigger definition if found, None otherwise
    pub fn get_trigger(&self, table_name: &str, trigger_name: &str) -> Result<Option<TriggerDefinition>> {
        let key = (table_name.to_string(), trigger_name.to_string());

        let triggers = self
            .triggers
            .read()
            .map_err(|e| Error::Generic(format!("Failed to acquire read lock: {}", e)))?;

        Ok(triggers.get(&key).cloned())
    }

    /// Get all triggers for a table
    ///
    /// # Arguments
    ///
    /// * `table_name` - Table name
    ///
    /// # Returns
    ///
    /// Check if any triggers exist for a table (cheap check, no cloning)
    pub fn has_triggers_for_table(&self, table_name: &str) -> bool {
        if let Ok(triggers) = self.triggers.read() {
            triggers.values().any(|t| t.table_name == table_name)
        } else {
            false
        }
    }

    /// Vector of all trigger definitions for the table
    pub fn get_triggers_for_table(&self, table_name: &str) -> Result<Vec<TriggerDefinition>> {
        let triggers = self
            .triggers
            .read()
            .map_err(|e| Error::Generic(format!("Failed to acquire read lock: {}", e)))?;

        Ok(triggers
            .values()
            .filter(|t| t.table_name == table_name)
            .cloned()
            .collect())
    }

    /// Get triggers for a table filtered by event and timing
    ///
    /// # Arguments
    ///
    /// * `table_name` - Table name
    /// * `event` - Trigger event (INSERT, UPDATE, DELETE)
    /// * `timing` - Trigger timing (BEFORE, AFTER, INSTEAD OF)
    ///
    /// # Returns
    ///
    /// Vector of matching trigger definitions, sorted by creation time
    pub fn get_triggers_for_event(
        &self,
        table_name: &str,
        event: &TriggerEvent,
        timing: &TriggerTiming,
    ) -> Result<Vec<TriggerDefinition>> {
        let triggers = self
            .triggers
            .read()
            .map_err(|e| Error::Generic(format!("Failed to acquire read lock: {}", e)))?;

        let mut matching: Vec<_> = triggers
            .values()
            .filter(|t| t.table_name == table_name && t.enabled && t.matches_event(event) && t.matches_timing(timing))
            .cloned()
            .collect();

        // Sort by creation time for deterministic execution order
        matching.sort_by_key(|t| t.created_at);

        Ok(matching)
    }

    /// Enable a trigger
    ///
    /// # Arguments
    ///
    /// * `table_name` - Table name
    /// * `trigger_name` - Trigger name
    ///
    /// # Returns
    ///
    /// Returns `Ok(true)` if trigger was found and enabled, `Ok(false)` if not found
    pub fn enable_trigger(&self, table_name: &str, trigger_name: &str) -> Result<bool> {
        let key = (table_name.to_string(), trigger_name.to_string());

        let mut triggers = self
            .triggers
            .write()
            .map_err(|e| Error::Generic(format!("Failed to acquire write lock: {}", e)))?;

        if let Some(trigger) = triggers.get_mut(&key) {
            trigger.enabled = true;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Disable a trigger
    ///
    /// # Arguments
    ///
    /// * `table_name` - Table name
    /// * `trigger_name` - Trigger name
    ///
    /// # Returns
    ///
    /// Returns `Ok(true)` if trigger was found and disabled, `Ok(false)` if not found
    pub fn disable_trigger(&self, table_name: &str, trigger_name: &str) -> Result<bool> {
        let key = (table_name.to_string(), trigger_name.to_string());

        let mut triggers = self
            .triggers
            .write()
            .map_err(|e| Error::Generic(format!("Failed to acquire write lock: {}", e)))?;

        if let Some(trigger) = triggers.get_mut(&key) {
            trigger.enabled = false;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// List all triggers in the database
    ///
    /// # Returns
    ///
    /// Vector of all trigger definitions
    pub fn list_all_triggers(&self) -> Result<Vec<TriggerDefinition>> {
        let triggers = self
            .triggers
            .read()
            .map_err(|e| Error::Generic(format!("Failed to acquire read lock: {}", e)))?;

        Ok(triggers.values().cloned().collect())
    }

    /// Check if a trigger exists
    ///
    /// # Arguments
    ///
    /// * `table_name` - Table name
    /// * `trigger_name` - Trigger name
    ///
    /// # Returns
    ///
    /// Returns true if the trigger exists
    pub fn trigger_exists(&self, table_name: &str, trigger_name: &str) -> Result<bool> {
        let key = (table_name.to_string(), trigger_name.to_string());

        let triggers = self
            .triggers
            .read()
            .map_err(|e| Error::Generic(format!("Failed to acquire read lock: {}", e)))?;

        Ok(triggers.contains_key(&key))
    }

    /// Drop all triggers for a table
    ///
    /// This is typically called when a table is dropped.
    ///
    /// # Arguments
    ///
    /// * `table_name` - Table name
    ///
    /// # Returns
    ///
    /// Number of triggers dropped
    pub fn drop_table_triggers(&self, table_name: &str) -> Result<usize> {
        let mut triggers = self
            .triggers
            .write()
            .map_err(|e| Error::Generic(format!("Failed to acquire write lock: {}", e)))?;

        let keys_to_remove: Vec<_> = triggers
            .keys()
            .filter(|(table, _)| table == table_name)
            .cloned()
            .collect();

        let count = keys_to_remove.len();
        for key in keys_to_remove {
            triggers.remove(&key);
        }
        drop(triggers);

        // The BEFORE-row rewrite recipes are keyed identically and MUST go with
        // the definitions. Leaving them behind would keep rewriting rows of a
        // table whose triggers no longer exist — and `apply_before_row_mutations`
        // now refuses to fire a recipe whose definition is gone, so an orphan
        // would be permanently inert dead weight in the hot-path scan.
        if let Ok(mut mutations) = self.row_mutations.write() {
            mutations.retain(|key, _| key.0 != table_name);
            self.row_mutation_count.store(mutations.len(), AtomicOrdering::Release);
        }

        Ok(count)
    }

    /// Get trigger storage key
    ///
    /// Format: trigger:{table_name}:{trigger_name}
    pub fn trigger_storage_key(&self, table_name: &str, trigger_name: &str) -> Vec<u8> {
        format!("{}{}:{}", self.storage_prefix, table_name, trigger_name).into_bytes()
    }

    /// Clear all triggers (for testing)
    #[cfg(test)]
    pub fn clear(&self) -> Result<()> {
        let mut triggers = self
            .triggers
            .write()
            .map_err(|e| Error::Generic(format!("Failed to acquire write lock: {}", e)))?;
        triggers.clear();
        Ok(())
    }

    /// Get trigger count (for testing)
    #[cfg(test)]
    pub fn count(&self) -> Result<usize> {
        let triggers = self
            .triggers
            .read()
            .map_err(|e| Error::Generic(format!("Failed to acquire read lock: {}", e)))?;
        Ok(triggers.len())
    }
}

impl Default for TriggerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Row context for trigger execution
///
/// Provides access to OLD and NEW tuple values during trigger execution
#[derive(Debug, Clone)]
pub struct TriggerRowContext {
    /// OLD tuple (for UPDATE and DELETE)
    pub old_tuple: Option<crate::Tuple>,
    /// NEW tuple (for INSERT and UPDATE)
    pub new_tuple: Option<crate::Tuple>,
    /// Transition tables for STATEMENT-level triggers (REFERENCING clause)
    pub transition_tables: Option<TransitionTables>,
}

impl TriggerRowContext {
    /// Create context for INSERT operation
    pub fn for_insert(new_tuple: crate::Tuple) -> Self {
        Self {
            old_tuple: None,
            new_tuple: Some(new_tuple),
            transition_tables: None,
        }
    }

    /// Create context for UPDATE operation
    pub fn for_update(old_tuple: crate::Tuple, new_tuple: crate::Tuple) -> Self {
        Self {
            old_tuple: Some(old_tuple),
            new_tuple: Some(new_tuple),
            transition_tables: None,
        }
    }

    /// Create context for DELETE operation
    pub fn for_delete(old_tuple: crate::Tuple) -> Self {
        Self {
            old_tuple: Some(old_tuple),
            new_tuple: None,
            transition_tables: None,
        }
    }

    /// Create context for STATEMENT-level triggers with transition tables
    pub fn for_statement(transition_tables: TransitionTables) -> Self {
        Self {
            old_tuple: None,
            new_tuple: None,
            transition_tables: Some(transition_tables),
        }
    }

    /// Attach transition tables to this context (for statement-level triggers)
    pub fn with_transition_tables(mut self, tables: TransitionTables) -> Self {
        self.transition_tables = Some(tables);
        self
    }

    /// Evaluate a WHEN condition expression against this row context
    ///
    /// # Arguments
    ///
    /// * `when_expr` - The WHEN condition expression to evaluate
    /// * `table_schema` - The schema of the table (for column resolution)
    ///
    /// # Returns
    ///
    /// Returns `true` if the condition is satisfied, `false` otherwise.
    /// Returns an error if evaluation fails.
    pub fn evaluate_when_condition(&self, when_expr: &LogicalExpr, table_schema: Arc<Schema>) -> Result<bool> {
        // Create an evaluator with trigger row context
        let evaluator = Evaluator::with_trigger_row_context(
            table_schema.clone(),
            Vec::new(), // No parameters for WHEN conditions
            self.clone(),
            table_schema,
        );

        // Use an empty tuple since we're accessing NEW/OLD through the context
        let empty_tuple = Tuple::new(Vec::new());

        // Evaluate the expression
        let result = evaluator.evaluate(when_expr, &empty_tuple)?;

        // Convert result to boolean
        match result {
            Value::Boolean(b) => Ok(b),
            Value::Null => Ok(false), // NULL treated as false in WHEN conditions
            _ => Err(Error::query_execution(format!(
                "WHEN condition must evaluate to boolean, got {:?}",
                result
            ))),
        }
    }
}

/// Trigger execution result
#[derive(Debug, Clone)]
pub enum TriggerAction {
    /// Continue with the DML operation
    Continue,
    /// Skip the DML operation (INSTEAD OF trigger)
    Skip,
    /// Abort the DML operation (trigger raised error)
    Abort(String),
}

impl TriggerRegistry {
    /// Execute triggers for a DML operation
    ///
    /// # Arguments
    ///
    /// * `table_name` - Table name
    /// * `event` - Trigger event (INSERT, UPDATE, DELETE)
    /// * `timing` - Trigger timing (BEFORE, AFTER, INSTEAD OF)
    /// * `row_context` - Row context with OLD and NEW tuples
    /// * `trigger_context` - Execution context for cascading tracking
    /// * `table_schema` - Optional table schema for WHEN condition evaluation
    /// * `executor_fn` - Function to execute trigger body statements
    ///
    /// # Returns
    ///
    /// TriggerAction indicating what to do next
    pub fn execute_triggers<F>(
        &self,
        table_name: &str,
        event: &TriggerEvent,
        timing: &TriggerTiming,
        row_context: &TriggerRowContext,
        trigger_context: &mut TriggerContext,
        table_schema: Option<Arc<Schema>>,
        executor_fn: &mut F,
    ) -> Result<TriggerAction>
    where
        F: FnMut(&LogicalPlan, &TriggerRowContext) -> Result<()>,
    {
        // Get matching triggers
        let triggers = self.get_triggers_for_event(table_name, event, timing)?;

        // If no triggers, continue
        if triggers.is_empty() {
            return Ok(TriggerAction::Continue);
        }

        // Execute each trigger
        for trigger_def in triggers {
            // Enter trigger context (check depth)
            trigger_context.enter(&trigger_def.name)?;

            // Evaluate WHEN condition if present
            let should_execute = if let Some(when_expr) = &trigger_def.when_condition {
                // Evaluate WHEN condition with row_context and table schema
                if let Some(schema) = &table_schema {
                    match row_context.evaluate_when_condition(when_expr, schema.clone()) {
                        Ok(result) => result,
                        Err(e) => {
                            // WHEN condition evaluation failed - log and skip this trigger
                            tracing::warn!(
                                "WHEN condition evaluation failed for trigger '{}': {}",
                                trigger_def.name,
                                e
                            );
                            false
                        }
                    }
                } else {
                    // No schema provided - fall back to always executing
                    // This maintains backward compatibility
                    true
                }
            } else {
                true
            };

            if should_execute {
                // Execute trigger body statements
                for stmt in &trigger_def.body {
                    if let Err(e) = executor_fn(stmt, row_context) {
                        // Trigger failed - abort operation
                        trigger_context.exit();
                        return Ok(TriggerAction::Abort(format!(
                            "Trigger '{}' failed: {}",
                            trigger_def.name, e
                        )));
                    }
                }

                // Handle INSTEAD OF triggers
                if trigger_def.timing == TriggerTiming::InsteadOf {
                    trigger_context.exit();
                    return Ok(TriggerAction::Skip);
                }
            }

            // Exit trigger context
            trigger_context.exit();
        }

        Ok(TriggerAction::Continue)
    }

    /// Execute triggers for multiple rows (FOR EACH ROW)
    ///
    /// # Arguments
    ///
    /// * `table_name` - Table name
    /// * `event` - Trigger event
    /// * `timing` - Trigger timing
    /// * `row_contexts` - Vector of row contexts
    /// * `trigger_context` - Execution context
    /// * `table_schema` - Optional table schema for WHEN condition evaluation
    /// * `executor_fn` - Function to execute trigger body
    ///
    /// # Returns
    ///
    /// Result with number of rows processed or error
    pub fn execute_row_triggers<F>(
        &self,
        table_name: &str,
        event: &TriggerEvent,
        timing: &TriggerTiming,
        row_contexts: &[TriggerRowContext],
        trigger_context: &mut TriggerContext,
        table_schema: Option<Arc<Schema>>,
        executor_fn: &mut F,
    ) -> Result<TriggerAction>
    where
        F: FnMut(&LogicalPlan, &TriggerRowContext) -> Result<()>,
    {
        // Get matching triggers
        let triggers = self.get_triggers_for_event(table_name, event, timing)?;

        if triggers.is_empty() {
            return Ok(TriggerAction::Continue);
        }

        // Separate ROW and STATEMENT triggers
        let row_triggers: Vec<_> = triggers.iter().filter(|t| t.for_each == TriggerFor::Row).collect();

        let statement_triggers: Vec<_> = triggers
            .iter()
            .filter(|t| t.for_each == TriggerFor::Statement)
            .collect();

        // Execute FOR EACH ROW triggers
        for row_context in row_contexts {
            for trigger_def in &row_triggers {
                trigger_context.enter(&trigger_def.name)?;

                // Evaluate WHEN condition if present
                let should_execute = if let Some(when_expr) = &trigger_def.when_condition {
                    // Evaluate WHEN condition with row_context and table schema
                    if let Some(schema) = &table_schema {
                        match row_context.evaluate_when_condition(when_expr, schema.clone()) {
                            Ok(result) => result,
                            Err(e) => {
                                // WHEN condition evaluation failed - log and skip this trigger
                                tracing::warn!(
                                    "WHEN condition evaluation failed for trigger '{}': {}",
                                    trigger_def.name,
                                    e
                                );
                                false
                            }
                        }
                    } else {
                        // No schema provided - fall back to always executing
                        true
                    }
                } else {
                    true
                };

                if should_execute {
                    for stmt in &trigger_def.body {
                        if let Err(e) = executor_fn(stmt, row_context) {
                            trigger_context.exit();
                            return Ok(TriggerAction::Abort(format!(
                                "Trigger '{}' failed: {}",
                                trigger_def.name, e
                            )));
                        }
                    }

                    if trigger_def.timing == TriggerTiming::InsteadOf {
                        trigger_context.exit();
                        return Ok(TriggerAction::Skip);
                    }
                }

                trigger_context.exit();
            }
        }

        // Execute FOR EACH STATEMENT triggers (once per statement)
        // STATEMENT triggers run even with empty row_contexts (e.g., TRUNCATE)
        for trigger_def in &statement_triggers {
            trigger_context.enter(&trigger_def.name)?;

            // Build transition tables from row contexts and trigger's REFERENCING clause
            let mut transition_tables = TransitionTables::from_row_contexts(row_contexts);

            // Set aliases from the trigger's REFERENCING clause
            for ref_clause in &trigger_def.referencing {
                match ref_clause {
                    TransitionTable::OldTable { alias } => {
                        transition_tables = transition_tables.with_old_table_alias(alias.clone());
                    }
                    TransitionTable::NewTable { alias } => {
                        transition_tables = transition_tables.with_new_table_alias(alias.clone());
                    }
                }
            }

            // Create statement-level context with transition tables
            let stmt_context = TriggerRowContext::for_statement(transition_tables);

            // WHEN conditions are not typically used with STATEMENT-level triggers,
            // but we evaluate them for completeness
            let should_execute = if let Some(when_expr) = &trigger_def.when_condition {
                if let Some(schema) = &table_schema {
                    match stmt_context.evaluate_when_condition(when_expr, schema.clone()) {
                        Ok(result) => result,
                        Err(e) => {
                            tracing::warn!(
                                "WHEN condition evaluation failed for trigger '{}': {}",
                                trigger_def.name,
                                e
                            );
                            false
                        }
                    }
                } else {
                    true
                }
            } else {
                true
            };

            if should_execute {
                for stmt in &trigger_def.body {
                    if let Err(e) = executor_fn(stmt, &stmt_context) {
                        trigger_context.exit();
                        return Ok(TriggerAction::Abort(format!(
                            "Trigger '{}' failed: {}",
                            trigger_def.name, e
                        )));
                    }
                }
            }

            trigger_context.exit();
        }

        Ok(TriggerAction::Continue)
    }
}

/// Persistent trigger storage operations
///
/// These methods integrate the trigger registry with the storage engine
/// for persistence across database sessions.
pub trait TriggerPersistence {
    /// Save a trigger to persistent storage
    fn save_trigger(&self, definition: &TriggerDefinition) -> Result<()>;

    /// Load a trigger from persistent storage
    fn load_trigger(&self, table_name: &str, trigger_name: &str) -> Result<Option<TriggerDefinition>>;

    /// Delete a trigger from persistent storage
    fn delete_trigger(&self, table_name: &str, trigger_name: &str) -> Result<()>;

    /// Load all triggers from persistent storage
    fn load_all_triggers(&self) -> Result<Vec<TriggerDefinition>>;
}

/// Pending trigger execution for deferred triggers
#[derive(Debug, Clone)]
pub struct PendingTriggerExecution {
    /// Trigger definition
    pub trigger: TriggerDefinition,
    /// Table name
    pub table_name: String,
    /// Event that fired the trigger
    pub event: TriggerEvent,
    /// Row contexts for the trigger execution
    pub row_contexts: Vec<TriggerRowContext>,
    /// Table schema for WHEN condition evaluation
    pub table_schema: Option<Arc<Schema>>,
}

/// Deferral mode for constraints/triggers
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeferralMode {
    /// Check immediately (default)
    #[default]
    Immediate,
    /// Defer until transaction commit
    Deferred,
}

/// Deferred trigger execution tracker
///
/// Tracks triggers that have been deferred and need to be executed at commit time.
/// Used for DEFERRABLE triggers and CONSTRAINT triggers.
#[derive(Debug)]
pub struct DeferredTriggerTracker {
    /// Pending trigger executions keyed by transaction ID
    pending: HashMap<u64, Vec<PendingTriggerExecution>>,
    /// Current deferral mode for triggers (can be changed with SET CONSTRAINTS)
    /// Key is (table_name, trigger_name), value is the current mode
    trigger_modes: HashMap<(String, String), DeferralMode>,
    /// Global deferral mode (SET CONSTRAINTS ALL)
    global_mode: Option<DeferralMode>,
}

impl DeferredTriggerTracker {
    /// Create a new deferred trigger tracker
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            trigger_modes: HashMap::new(),
            global_mode: None,
        }
    }

    /// Check if a trigger should be deferred based on current settings
    pub fn should_defer(&self, trigger: &TriggerDefinition) -> bool {
        // Non-deferrable triggers always run immediately
        if !trigger.is_deferrable() {
            return false;
        }

        // Check for specific trigger mode override
        let key = (trigger.table_name.clone(), trigger.name.clone());
        if let Some(mode) = self.trigger_modes.get(&key) {
            return *mode == DeferralMode::Deferred;
        }

        // Check global mode
        if let Some(mode) = self.global_mode {
            return mode == DeferralMode::Deferred;
        }

        // Use trigger's default (INITIALLY DEFERRED or INITIALLY IMMEDIATE)
        trigger.is_initially_deferred()
    }

    /// Add a pending trigger execution for deferred execution
    pub fn add_pending(&mut self, txn_id: u64, execution: PendingTriggerExecution) {
        self.pending.entry(txn_id).or_insert_with(Vec::new).push(execution);
    }

    /// Get all pending executions for a transaction
    pub fn get_pending(&self, txn_id: u64) -> Option<&Vec<PendingTriggerExecution>> {
        self.pending.get(&txn_id)
    }

    /// Take all pending executions for a transaction (removes them from tracker)
    pub fn take_pending(&mut self, txn_id: u64) -> Vec<PendingTriggerExecution> {
        self.pending.remove(&txn_id).unwrap_or_default()
    }

    /// Clear all pending executions for a transaction (e.g., on rollback)
    pub fn clear(&mut self, txn_id: u64) {
        self.pending.remove(&txn_id);
    }

    /// Set deferral mode for a specific trigger
    pub fn set_trigger_mode(&mut self, table_name: &str, trigger_name: &str, mode: DeferralMode) {
        let key = (table_name.to_string(), trigger_name.to_string());
        self.trigger_modes.insert(key, mode);
    }

    /// Set global deferral mode (SET CONSTRAINTS ALL)
    pub fn set_global_mode(&mut self, mode: DeferralMode) {
        self.global_mode = Some(mode);
    }

    /// Clear global deferral mode
    pub fn clear_global_mode(&mut self) {
        self.global_mode = None;
    }

    /// Clear all per-trigger mode overrides
    pub fn clear_trigger_modes(&mut self) {
        self.trigger_modes.clear();
    }

    /// Execute all pending triggers for a transaction
    ///
    /// Returns TriggerAction indicating if any trigger aborted
    pub fn execute_pending<F>(
        &mut self,
        txn_id: u64,
        trigger_context: &mut TriggerContext,
        executor_fn: &mut F,
    ) -> Result<TriggerAction>
    where
        F: FnMut(&LogicalPlan, &TriggerRowContext) -> Result<()>,
    {
        let pending = self.take_pending(txn_id);

        for execution in pending {
            trigger_context.enter(&execution.trigger.name)?;

            // Execute for each row context
            for row_context in &execution.row_contexts {
                // Evaluate WHEN condition if present
                let should_execute = if let Some(when_expr) = &execution.trigger.when_condition {
                    if let Some(schema) = &execution.table_schema {
                        match row_context.evaluate_when_condition(when_expr, schema.clone()) {
                            Ok(result) => result,
                            Err(e) => {
                                tracing::warn!(
                                    "Deferred WHEN condition evaluation failed for trigger '{}': {}",
                                    execution.trigger.name,
                                    e
                                );
                                false
                            }
                        }
                    } else {
                        true
                    }
                } else {
                    true
                };

                if should_execute {
                    for stmt in &execution.trigger.body {
                        if let Err(e) = executor_fn(stmt, row_context) {
                            trigger_context.exit();
                            return Ok(TriggerAction::Abort(format!(
                                "Deferred trigger '{}' failed at commit: {}",
                                execution.trigger.name, e
                            )));
                        }
                    }
                }
            }

            trigger_context.exit();
        }

        Ok(TriggerAction::Continue)
    }

    /// Check if there are any pending triggers for a transaction
    pub fn has_pending(&self, txn_id: u64) -> bool {
        self.pending.get(&txn_id).map(|v| !v.is_empty()).unwrap_or(false)
    }

    /// Get count of pending triggers for a transaction
    pub fn pending_count(&self, txn_id: u64) -> usize {
        self.pending.get(&txn_id).map(|v| v.len()).unwrap_or(0)
    }
}

impl Default for DeferredTriggerTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::sql::BinaryOperator;

    #[test]
    fn test_trigger_registry_basic() {
        let registry = TriggerRegistry::new();

        let trigger = TriggerDefinition::new(
            "audit_trigger".to_string(),
            "users".to_string(),
            TriggerTiming::After,
            vec![TriggerEvent::Insert, TriggerEvent::Update(None)],
            TriggerFor::Row,
            None,
            vec![],
            vec![], // referencing
        );

        // Register trigger
        assert!(registry.register_trigger(trigger.clone()).is_ok());

        // Get trigger
        let retrieved = registry.get_trigger("users", "audit_trigger").unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().name, "audit_trigger");

        // Check exists
        assert!(registry.trigger_exists("users", "audit_trigger").unwrap());

        // Drop trigger
        assert!(registry.drop_trigger("users", "audit_trigger").unwrap());
        assert!(!registry.trigger_exists("users", "audit_trigger").unwrap());
    }

    #[test]
    fn test_duplicate_trigger() {
        let registry = TriggerRegistry::new();

        let trigger = TriggerDefinition::new(
            "test_trigger".to_string(),
            "users".to_string(),
            TriggerTiming::Before,
            vec![TriggerEvent::Insert],
            TriggerFor::Row,
            None,
            vec![],
            vec![], // referencing
        );

        // First registration should succeed
        assert!(registry.register_trigger(trigger.clone()).is_ok());

        // Second registration should fail
        assert!(registry.register_trigger(trigger).is_err());
    }

    #[test]
    fn test_get_triggers_for_table() {
        let registry = TriggerRegistry::new();

        let trigger1 = TriggerDefinition::new(
            "trigger1".to_string(),
            "users".to_string(),
            TriggerTiming::Before,
            vec![TriggerEvent::Insert],
            TriggerFor::Row,
            None,
            vec![],
            vec![], // referencing
        );

        let trigger2 = TriggerDefinition::new(
            "trigger2".to_string(),
            "users".to_string(),
            TriggerTiming::After,
            vec![TriggerEvent::Delete],
            TriggerFor::Row,
            None,
            vec![],
            vec![], // referencing
        );

        let trigger3 = TriggerDefinition::new(
            "trigger3".to_string(),
            "products".to_string(),
            TriggerTiming::Before,
            vec![TriggerEvent::Update(None)],
            TriggerFor::Row,
            None,
            vec![],
            vec![], // referencing
        );

        registry.register_trigger(trigger1).unwrap();
        registry.register_trigger(trigger2).unwrap();
        registry.register_trigger(trigger3).unwrap();

        let users_triggers = registry.get_triggers_for_table("users").unwrap();
        assert_eq!(users_triggers.len(), 2);

        let products_triggers = registry.get_triggers_for_table("products").unwrap();
        assert_eq!(products_triggers.len(), 1);
    }

    #[test]
    fn test_get_triggers_for_event() {
        let registry = TriggerRegistry::new();

        let trigger1 = TriggerDefinition::new(
            "before_insert".to_string(),
            "users".to_string(),
            TriggerTiming::Before,
            vec![TriggerEvent::Insert],
            TriggerFor::Row,
            None,
            vec![],
            vec![], // referencing
        );

        let trigger2 = TriggerDefinition::new(
            "after_insert".to_string(),
            "users".to_string(),
            TriggerTiming::After,
            vec![TriggerEvent::Insert],
            TriggerFor::Row,
            None,
            vec![],
            vec![], // referencing
        );

        let trigger3 = TriggerDefinition::new(
            "after_update".to_string(),
            "users".to_string(),
            TriggerTiming::After,
            vec![TriggerEvent::Update(None)],
            TriggerFor::Row,
            None,
            vec![],
            vec![], // referencing
        );

        registry.register_trigger(trigger1).unwrap();
        registry.register_trigger(trigger2).unwrap();
        registry.register_trigger(trigger3).unwrap();

        // Get BEFORE INSERT triggers
        let before_insert = registry
            .get_triggers_for_event("users", &TriggerEvent::Insert, &TriggerTiming::Before)
            .unwrap();
        assert_eq!(before_insert.len(), 1);
        assert_eq!(before_insert[0].name, "before_insert");

        // Get AFTER INSERT triggers
        let after_insert = registry
            .get_triggers_for_event("users", &TriggerEvent::Insert, &TriggerTiming::After)
            .unwrap();
        assert_eq!(after_insert.len(), 1);
        assert_eq!(after_insert[0].name, "after_insert");

        // Get AFTER UPDATE triggers
        let after_update = registry
            .get_triggers_for_event("users", &TriggerEvent::Update(None), &TriggerTiming::After)
            .unwrap();
        assert_eq!(after_update.len(), 1);
        assert_eq!(after_update[0].name, "after_update");
    }

    #[test]
    fn test_enable_disable_trigger() {
        let registry = TriggerRegistry::new();

        let trigger = TriggerDefinition::new(
            "test_trigger".to_string(),
            "users".to_string(),
            TriggerTiming::Before,
            vec![TriggerEvent::Insert],
            TriggerFor::Row,
            None,
            vec![],
            vec![], // referencing
        );

        registry.register_trigger(trigger).unwrap();

        // Disable trigger
        assert!(registry.disable_trigger("users", "test_trigger").unwrap());

        // Check it's disabled (won't show up in event query)
        let triggers = registry
            .get_triggers_for_event("users", &TriggerEvent::Insert, &TriggerTiming::Before)
            .unwrap();
        assert_eq!(triggers.len(), 0);

        // Enable trigger
        assert!(registry.enable_trigger("users", "test_trigger").unwrap());

        // Check it's enabled again
        let triggers = registry
            .get_triggers_for_event("users", &TriggerEvent::Insert, &TriggerTiming::Before)
            .unwrap();
        assert_eq!(triggers.len(), 1);
    }

    #[test]
    fn test_trigger_context_depth() {
        let mut context = TriggerContext::new();

        assert_eq!(context.depth(), 0);
        assert!(!context.at_max_depth());

        // Enter triggers up to max depth
        for i in 0..MAX_TRIGGER_DEPTH {
            assert!(context.enter(&format!("trigger_{}", i)).is_ok());
        }

        assert_eq!(context.depth(), MAX_TRIGGER_DEPTH);
        assert!(context.at_max_depth());

        // Attempting to exceed max depth should fail
        assert!(context.enter("trigger_overflow").is_err());

        // Exit all triggers
        for _ in 0..MAX_TRIGGER_DEPTH {
            context.exit();
        }

        assert_eq!(context.depth(), 0);
    }

    #[test]
    fn test_drop_table_triggers() {
        let registry = TriggerRegistry::new();

        // Add multiple triggers to users table
        for i in 0..3 {
            let trigger = TriggerDefinition::new(
                format!("trigger_{}", i),
                "users".to_string(),
                TriggerTiming::Before,
                vec![TriggerEvent::Insert],
                TriggerFor::Row,
                None,
                vec![],
                vec![], // referencing
            );
            registry.register_trigger(trigger).unwrap();
        }

        // Add trigger to products table
        let trigger = TriggerDefinition::new(
            "product_trigger".to_string(),
            "products".to_string(),
            TriggerTiming::Before,
            vec![TriggerEvent::Insert],
            TriggerFor::Row,
            None,
            vec![],
            vec![], // referencing
        );
        registry.register_trigger(trigger).unwrap();

        // Drop all users triggers
        let count = registry.drop_table_triggers("users").unwrap();
        assert_eq!(count, 3);

        // Verify users triggers are gone
        let users_triggers = registry.get_triggers_for_table("users").unwrap();
        assert_eq!(users_triggers.len(), 0);

        // Verify products trigger still exists
        let products_triggers = registry.get_triggers_for_table("products").unwrap();
        assert_eq!(products_triggers.len(), 1);
    }

    #[test]
    fn test_trigger_event_matching() {
        let trigger = TriggerDefinition::new(
            "test".to_string(),
            "users".to_string(),
            TriggerTiming::Before,
            vec![TriggerEvent::Update(Some(vec![
                "email".to_string(),
                "name".to_string(),
            ]))],
            TriggerFor::Row,
            None,
            vec![],
            vec![], // referencing
        );

        // Should match if email is updated
        assert!(trigger.matches_event(&TriggerEvent::Update(Some(vec!["email".to_string()]))));

        // Should match if name is updated
        assert!(trigger.matches_event(&TriggerEvent::Update(Some(vec!["name".to_string()]))));

        // Should match if both are updated
        assert!(trigger.matches_event(&TriggerEvent::Update(Some(vec![
            "email".to_string(),
            "name".to_string()
        ]))));

        // Should not match if different column is updated
        assert!(!trigger.matches_event(&TriggerEvent::Update(Some(vec!["age".to_string()]))));

        // Should match UPDATE(None) - any column update
        assert!(trigger.matches_event(&TriggerEvent::Update(None)));

        // Should not match INSERT
        assert!(!trigger.matches_event(&TriggerEvent::Insert));
    }

    #[test]
    fn test_when_condition_evaluation() {
        use crate::{Column, DataType};

        // Create a test schema for users table
        let schema = Arc::new(Schema::new(vec![
            Column {
                name: "id".to_string(),
                data_type: DataType::Int4,
                nullable: false,
                primary_key: true,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
            },
            Column {
                name: "age".to_string(),
                data_type: DataType::Int4,
                nullable: false,
                primary_key: false,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
            },
            Column {
                name: "status".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
            },
        ]));

        // Create a row context for INSERT with NEW.age = 25
        let new_tuple = Tuple::new(vec![
            Value::Int4(1),
            Value::Int4(25),
            Value::String("active".to_string()),
        ]);
        let row_context = TriggerRowContext::for_insert(new_tuple);

        // Test WHEN condition: NEW.age > 20 (should be true)
        let when_expr = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::NewRow {
                column: "age".to_string(),
            }),
            op: BinaryOperator::Gt,
            right: Box::new(LogicalExpr::Literal(Value::Int4(20))),
        };

        let result = row_context.evaluate_when_condition(&when_expr, schema.clone());
        assert!(result.is_ok());
        assert!(result.unwrap()); // NEW.age (25) > 20 is true

        // Test WHEN condition: NEW.age > 30 (should be false)
        let when_expr_false = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::NewRow {
                column: "age".to_string(),
            }),
            op: BinaryOperator::Gt,
            right: Box::new(LogicalExpr::Literal(Value::Int4(30))),
        };

        let result_false = row_context.evaluate_when_condition(&when_expr_false, schema.clone());
        assert!(result_false.is_ok());
        assert!(!result_false.unwrap()); // NEW.age (25) > 30 is false

        // Test WHEN condition with string comparison: NEW.status = 'active'
        let when_expr_str = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::NewRow {
                column: "status".to_string(),
            }),
            op: BinaryOperator::Eq,
            right: Box::new(LogicalExpr::Literal(Value::String("active".to_string()))),
        };

        let result_str = row_context.evaluate_when_condition(&when_expr_str, schema.clone());
        assert!(result_str.is_ok());
        assert!(result_str.unwrap()); // NEW.status = 'active' is true
    }

    #[test]
    fn test_when_condition_with_old_and_new() {
        use crate::{Column, DataType};

        // Create a test schema
        let schema = Arc::new(Schema::new(vec![
            Column {
                name: "id".to_string(),
                data_type: DataType::Int4,
                nullable: false,
                primary_key: true,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
            },
            Column {
                name: "price".to_string(),
                data_type: DataType::Int4,
                nullable: false,
                primary_key: false,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
            },
        ]));

        // Create a row context for UPDATE: price changed from 100 to 150
        let old_tuple = Tuple::new(vec![Value::Int4(1), Value::Int4(100)]);
        let new_tuple = Tuple::new(vec![Value::Int4(1), Value::Int4(150)]);
        let row_context = TriggerRowContext::for_update(old_tuple, new_tuple);

        // Test WHEN condition: NEW.price > OLD.price (price increased)
        let when_expr = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::NewRow {
                column: "price".to_string(),
            }),
            op: BinaryOperator::Gt,
            right: Box::new(LogicalExpr::OldRow {
                column: "price".to_string(),
            }),
        };

        let result = row_context.evaluate_when_condition(&when_expr, schema.clone());
        assert!(result.is_ok());
        assert!(result.unwrap()); // NEW.price (150) > OLD.price (100) is true

        // Test WHEN condition: NEW.price < OLD.price (price decreased - false)
        let when_expr_dec = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::NewRow {
                column: "price".to_string(),
            }),
            op: BinaryOperator::Lt,
            right: Box::new(LogicalExpr::OldRow {
                column: "price".to_string(),
            }),
        };

        let result_dec = row_context.evaluate_when_condition(&when_expr_dec, schema);
        assert!(result_dec.is_ok());
        assert!(!result_dec.unwrap()); // NEW.price (150) < OLD.price (100) is false
    }

    // =======================================================================
    // apply_before_row_mutations — the ONE trigger mechanism that has an
    // observable effect today. Trigger BODIES are still never executed; these
    // tests pin the gating (`enabled`, `WHEN`) and the firing ORDER of the
    // `NEW.<col> = <expr>` / `RETURN NULL` rewrite, all of which the pre-4.20
    // implementation ignored outright.
    // =======================================================================

    /// `mut_t(id INT, tag TEXT)`.
    fn mutation_test_schema() -> Arc<Schema> {
        use crate::{Column, DataType};
        Arc::new(Schema::new(vec![
            Column {
                name: "id".to_string(),
                data_type: DataType::Int4,
                nullable: false,
                primary_key: true,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
            },
            Column {
                name: "tag".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
            },
        ]))
    }

    /// Register a BEFORE INSERT FOR EACH ROW trigger named `name` on `mut_t`
    /// whose recipe sets `tag = <literal>`.
    fn register_tag_setter(
        registry: &TriggerRegistry,
        name: &str,
        literal: &str,
        when: Option<Box<LogicalExpr>>,
        enabled: bool,
    ) {
        let mut def = TriggerDefinition::new(
            name.to_string(),
            "mut_t".to_string(),
            TriggerTiming::Before,
            vec![TriggerEvent::Insert],
            TriggerFor::Row,
            when,
            vec![],
            vec![],
        );
        def.enabled = enabled;
        registry.register_trigger(def).expect("register");
        registry.register_row_mutation(
            "mut_t",
            name,
            TriggerRowMutation {
                events: vec![TriggerEvent::Insert],
                assignments: vec![(
                    "tag".to_string(),
                    LogicalExpr::Literal(Value::String(literal.to_string())),
                )],
                returns_null: false,
            },
        );
    }

    fn tag_of(tuple: &Tuple) -> String {
        match tuple.values.get(1) {
            Some(Value::String(s)) => s.clone(),
            other => panic!("expected a text tag, got {other:?}"),
        }
    }

    #[test]
    fn test_row_mutation_is_skipped_when_trigger_is_disabled() {
        let registry = TriggerRegistry::new();
        let schema = mutation_test_schema();
        register_tag_setter(&registry, "t_disabled", "rewritten", None, false);

        let mut tuple = Tuple::new(vec![Value::Int4(1), Value::String("original".to_string())]);
        let kept = registry
            .apply_before_row_mutations("mut_t", &TriggerEvent::Insert, &mut tuple, schema)
            .expect("apply");

        assert!(kept, "a disabled trigger must not suppress the row");
        assert_eq!(
            tag_of(&tuple),
            "original",
            "a disabled trigger's recipe must not rewrite the row"
        );
    }

    #[test]
    fn test_row_mutations_fire_in_trigger_name_order() {
        let registry = TriggerRegistry::new();
        let schema = mutation_test_schema();
        // Registered out of order on purpose; PostgreSQL fires alphabetically,
        // so `z_last` must win regardless of insertion order or hash order.
        register_tag_setter(&registry, "z_last", "from-z", None, true);
        register_tag_setter(&registry, "a_first", "from-a", None, true);

        let mut tuple = Tuple::new(vec![Value::Int4(1), Value::String("original".to_string())]);
        let kept = registry
            .apply_before_row_mutations("mut_t", &TriggerEvent::Insert, &mut tuple, schema)
            .expect("apply");

        assert!(kept);
        assert_eq!(tag_of(&tuple), "from-z", "the alphabetically last trigger writes last");
    }

    #[test]
    fn test_row_mutation_when_condition_gates_firing() {
        let schema = mutation_test_schema();
        // WHEN (NEW.id > 10)
        let when = || {
            Some(Box::new(LogicalExpr::BinaryExpr {
                left: Box::new(LogicalExpr::NewRow {
                    column: "id".to_string(),
                }),
                op: BinaryOperator::Gt,
                right: Box::new(LogicalExpr::Literal(Value::Int4(10))),
            }))
        };

        // Row satisfying WHEN is rewritten.
        let registry = TriggerRegistry::new();
        register_tag_setter(&registry, "t_when", "rewritten", when(), true);
        let mut hit = Tuple::new(vec![Value::Int4(42), Value::String("original".to_string())]);
        assert!(registry
            .apply_before_row_mutations("mut_t", &TriggerEvent::Insert, &mut hit, schema.clone())
            .expect("apply"));
        assert_eq!(tag_of(&hit), "rewritten", "WHEN true must fire");

        // Row failing WHEN is untouched.
        let mut miss = Tuple::new(vec![Value::Int4(1), Value::String("original".to_string())]);
        assert!(registry
            .apply_before_row_mutations("mut_t", &TriggerEvent::Insert, &mut miss, schema.clone())
            .expect("apply"));
        assert_eq!(tag_of(&miss), "original", "WHEN false must not fire");

        // WHEN over a NULL column is NULL, which is not TRUE — not fired.
        let mut null_row = Tuple::new(vec![Value::Null, Value::String("original".to_string())]);
        assert!(registry
            .apply_before_row_mutations("mut_t", &TriggerEvent::Insert, &mut null_row, schema)
            .expect("apply"));
        assert_eq!(tag_of(&null_row), "original", "WHEN NULL must not fire");
    }

    #[test]
    fn test_when_condition_gates_return_null_suppression() {
        let registry = TriggerRegistry::new();
        let schema = mutation_test_schema();
        let mut def = TriggerDefinition::new(
            "t_skip".to_string(),
            "mut_t".to_string(),
            TriggerTiming::Before,
            vec![TriggerEvent::Insert],
            TriggerFor::Row,
            Some(Box::new(LogicalExpr::BinaryExpr {
                left: Box::new(LogicalExpr::NewRow {
                    column: "id".to_string(),
                }),
                op: BinaryOperator::Gt,
                right: Box::new(LogicalExpr::Literal(Value::Int4(10))),
            })),
            vec![],
            vec![],
        );
        def.enabled = true;
        registry.register_trigger(def).expect("register");
        registry.register_row_mutation(
            "mut_t",
            "t_skip",
            TriggerRowMutation {
                events: vec![TriggerEvent::Insert],
                assignments: vec![],
                returns_null: true,
            },
        );

        let mut suppressed = Tuple::new(vec![Value::Int4(42), Value::String("a".to_string())]);
        assert!(
            !registry
                .apply_before_row_mutations("mut_t", &TriggerEvent::Insert, &mut suppressed, schema.clone())
                .expect("apply"),
            "RETURN NULL under a satisfied WHEN suppresses the row"
        );

        let mut kept = Tuple::new(vec![Value::Int4(1), Value::String("a".to_string())]);
        assert!(
            registry
                .apply_before_row_mutations("mut_t", &TriggerEvent::Insert, &mut kept, schema)
                .expect("apply"),
            "RETURN NULL under an unsatisfied WHEN must NOT suppress the row"
        );
    }

    #[test]
    fn test_row_mutation_unknown_column_and_bad_rhs_are_skipped_without_error() {
        let registry = TriggerRegistry::new();
        let schema = mutation_test_schema();
        let def = TriggerDefinition::new(
            "t_bad".to_string(),
            "mut_t".to_string(),
            TriggerTiming::Before,
            vec![TriggerEvent::Insert],
            TriggerFor::Row,
            None,
            vec![],
            vec![],
        );
        registry.register_trigger(def).expect("register");
        registry.register_row_mutation(
            "mut_t",
            "t_bad",
            TriggerRowMutation {
                events: vec![TriggerEvent::Insert],
                assignments: vec![
                    // Column that does not exist on the table.
                    ("nope".to_string(), LogicalExpr::Literal(Value::String("x".to_string()))),
                    // RHS referencing a column that does not exist.
                    (
                        "tag".to_string(),
                        LogicalExpr::NewRow {
                            column: "also_nope".to_string(),
                        },
                    ),
                ],
                returns_null: false,
            },
        );

        let mut tuple = Tuple::new(vec![Value::Int4(1), Value::String("original".to_string())]);
        let kept = registry
            .apply_before_row_mutations("mut_t", &TriggerEvent::Insert, &mut tuple, schema)
            .expect("an unsupported recipe must never fail the DML");
        assert!(kept);
        assert_eq!(tag_of(&tuple), "original", "nothing applied, nothing corrupted");
    }

    #[test]
    fn test_drop_table_triggers_purges_row_mutations() {
        let registry = TriggerRegistry::new();
        let schema = mutation_test_schema();
        register_tag_setter(&registry, "t_gone", "rewritten", None, true);
        assert!(registry.has_row_mutations("mut_t"));

        registry.drop_table_triggers("mut_t").expect("drop");

        assert!(
            !registry.has_row_mutations("mut_t"),
            "DROP TABLE must take the rewrite recipe with the definition"
        );
        let mut tuple = Tuple::new(vec![Value::Int4(1), Value::String("original".to_string())]);
        assert!(registry
            .apply_before_row_mutations("mut_t", &TriggerEvent::Insert, &mut tuple, schema)
            .expect("apply"));
        assert_eq!(tag_of(&tuple), "original");
    }

    #[test]
    fn test_has_row_mutations_is_false_on_an_empty_registry() {
        let registry = TriggerRegistry::new();
        assert!(!registry.has_row_mutations("anything"));
        let schema = mutation_test_schema();
        let mut tuple = Tuple::new(vec![Value::Int4(1), Value::String("original".to_string())]);
        assert!(registry
            .apply_before_row_mutations("mut_t", &TriggerEvent::Insert, &mut tuple, schema)
            .expect("apply"));
        assert_eq!(tag_of(&tuple), "original");
    }

    #[test]
    fn test_trigger_row_mutation_round_trips_through_bincode() {
        // The recipe is persisted under `trigger_rowmut:{table}:{name}` so a
        // trigger keeps rewriting rows after a restart. If this stops
        // round-tripping, restart durability is silently gone.
        let mutation = TriggerRowMutation {
            events: vec![
                TriggerEvent::Insert,
                TriggerEvent::Update(Some(vec!["tag".to_string()])),
            ],
            assignments: vec![(
                "tag".to_string(),
                LogicalExpr::Literal(Value::String("set-by-trigger".to_string())),
            )],
            returns_null: false,
        };
        let bytes = bincode::serialize(&mutation).expect("serialize");
        let back: TriggerRowMutation = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(back.events.len(), 2);
        assert_eq!(back.assignments.len(), 1);
        assert_eq!(back.assignments[0].0, "tag");
        assert!(!back.returns_null);
    }
}
