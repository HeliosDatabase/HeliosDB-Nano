//! Join operators
//!
//! This module provides nested loop join and hash join implementations.

#![allow(elided_lifetimes_in_paths)]

use crate::{Result, Error, Tuple, Schema};
use super::{PhysicalOperator, TimeoutContext, Executor};
use std::sync::Arc;

/// Nested loop join operator
///
/// Implements joins using nested loop algorithm.
pub struct NestedLoopJoinOperator {
    left: Box<dyn PhysicalOperator>,
    join_type: crate::sql::JoinType,
    on_condition: Option<crate::sql::LogicalExpr>,
    output_schema: Arc<Schema>,
    evaluator: crate::sql::Evaluator,
    // State for nested loop
    left_tuple: Option<Tuple>,
    right_tuples: Vec<Tuple>,
    right_index: usize,
    timeout_ctx: Option<TimeoutContext>,
}

impl NestedLoopJoinOperator {
    pub fn new(
        left: Box<dyn PhysicalOperator>,
        mut right: Box<dyn PhysicalOperator>,
        join_type: crate::sql::JoinType,
        on_condition: Option<crate::sql::LogicalExpr>,
        timeout_ctx: Option<TimeoutContext>,
    ) -> Result<Self> {
        // Build output schema by combining left and right schemas
        let left_schema = left.schema();
        let right_schema = right.schema();

        let mut columns = left_schema.columns.clone();
        columns.extend(right_schema.columns.clone());
        let output_schema = Arc::new(Schema { columns });

        // Create evaluator with output schema for evaluating join conditions
        let evaluator = crate::sql::Evaluator::new(output_schema.clone());

        // Materialize all right tuples upfront (with timeout checking)
        let mut right_tuples = Vec::new();
        while let Some(tuple) = right.next()? {
            // Check timeout during right side materialization (blocking operation)
            if let Some(ref ctx) = timeout_ctx {
                ctx.check_timeout()?;
            }
            right_tuples.push(tuple);
        }

        Ok(Self {
            left,
            join_type,
            on_condition,
            output_schema,
            evaluator,
            left_tuple: None,
            right_tuples,
            right_index: 0,
            timeout_ctx,
        })
    }

    /// Set timeout context (no-op since timeout is set during construction)
    pub fn with_timeout(self, _timeout_ctx: Option<TimeoutContext>) -> Self {
        // Timeout already set during construction, ignore this call
        self
    }
}

impl PhysicalOperator for NestedLoopJoinOperator {
    fn next(&mut self) -> Result<Option<Tuple>> {
        // For now, only support INNER JOIN
        if !matches!(self.join_type, crate::sql::JoinType::Inner) {
            return Err(crate::Error::query_execution(
                "Only INNER JOIN is currently supported"
            ));
        }

        loop {
            // Check timeout during nested loop iteration
            if let Some(ref ctx) = self.timeout_ctx {
                ctx.check_timeout()?;
            }

            // If we don't have a left tuple, get the next one
            if self.left_tuple.is_none() {
                self.left_tuple = self.left.next()?;

                // If no more left tuples, we're done
                if self.left_tuple.is_none() {
                    return Ok(None);
                }

                // Reset right index for new left tuple
                self.right_index = 0;
            }

            // Try to find a matching right tuple
            while self.right_index < self.right_tuples.len() {
                let right_tuple = &self.right_tuples[self.right_index];
                self.right_index += 1;

                // Combine left and right tuples
                let left_tuple = self.left_tuple.as_ref()
                    .ok_or_else(|| Error::query_execution("Left tuple unexpectedly None"))?;
                let mut combined_values = left_tuple.values.clone();
                combined_values.extend(right_tuple.values.clone());
                let combined_tuple = Tuple::new(combined_values);

                // Check join condition
                let matches = if let Some(condition) = &self.on_condition {
                    let result = self.evaluator.evaluate(condition, &combined_tuple)?;
                    match result {
                        crate::Value::Boolean(b) => b,
                        _ => false,
                    }
                } else {
                    // No condition means cross join - all combinations match
                    true
                };

                if matches {
                    return Ok(Some(combined_tuple));
                }
            }

            // No more right tuples for this left tuple, get next left tuple
            self.left_tuple = None;
        }
    }

    fn schema(&self) -> Arc<Schema> {
        self.output_schema.clone()
    }
}

/// Hash join operator
///
/// Implements hash-based join using classic two-phase algorithm:
/// 1. Build phase: Hash all tuples from right (build) side
/// 2. Probe phase: Stream left (probe) side, lookup matches
///
/// This provides O(N + M) time complexity vs O(N * M) for nested loop join.
/// Algorithm from Silberschatz "Database System Concepts" Ch. 12.5.3.
pub struct HashJoinOperator {
    // Input operators
    left: Box<dyn PhysicalOperator>,

    // Join specification
    join_type: crate::sql::JoinType,
    on_condition: Option<crate::sql::LogicalExpr>,

    // Hash table (key: join columns, value: matching tuples)
    hash_table: std::collections::HashMap<JoinKey, Vec<Tuple>>,

    // Output schema
    output_schema: Arc<Schema>,

    // Expression evaluator for combined tuples (used for condition evaluation after join)
    evaluator: crate::sql::Evaluator,

    // Separate evaluators for left and right sides (used during key extraction)
    left_evaluator: crate::sql::Evaluator,
    right_evaluator: crate::sql::Evaluator,

    // State machine
    state: JoinState,

    // Probe phase state
    current_left_tuple: Option<Tuple>,
    current_matches: Vec<Tuple>,
    match_index: usize,

    // LEFT/RIGHT/FULL join state
    matched_right_keys: std::collections::HashSet<JoinKey>,
    unmatched_right_iter: Option<std::vec::IntoIter<(JoinKey, Vec<Tuple>)>>,
    unmatched_right_current: Option<std::vec::IntoIter<Tuple>>,

    // Memory management
    memory_limit: usize,
    memory_used: usize,

    // Right side schema for NULL padding
    right_column_count: usize,
    left_column_count: usize,

    // Query timeout
    timeout_ctx: Option<TimeoutContext>,
}

/// Join key for hash table lookups
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct JoinKey(Vec<crate::Value>);

/// State machine for join execution
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JoinState {
    /// Initial state - build phase not started
    Initial,
    /// Probing hash table with left side
    Probing,
    /// Emitting unmatched tuples (for outer joins)
    EmittingUnmatched,
    /// Exhausted - no more tuples
    Exhausted,
}

impl HashJoinOperator {
    /// Default memory limit: 100 MB
    const DEFAULT_MEMORY_LIMIT: usize = 100 * 1024 * 1024;

    /// Create a new hash join operator with default memory limit
    pub fn new(
        left: Box<dyn PhysicalOperator>,
        right: Box<dyn PhysicalOperator>,
        join_type: crate::sql::JoinType,
        on_condition: Option<crate::sql::LogicalExpr>,
        timeout_ctx: Option<TimeoutContext>,
    ) -> Result<Self> {
        Self::with_memory_limit(left, right, join_type, on_condition, Self::DEFAULT_MEMORY_LIMIT, timeout_ctx)
    }

    /// Create a new hash join operator with custom memory limit
    fn with_memory_limit(
        left: Box<dyn PhysicalOperator>,
        mut right: Box<dyn PhysicalOperator>,  // Must be mutable to consume in build_phase
        join_type: crate::sql::JoinType,
        on_condition: Option<crate::sql::LogicalExpr>,
        memory_limit: usize,
        timeout_ctx: Option<TimeoutContext>,
    ) -> Result<Self> {
        // Build output schema by combining left and right schemas
        let left_schema = left.schema();
        let right_schema = right.schema();

        let left_column_count = left_schema.columns.len();
        let right_column_count = right_schema.columns.len();

        let mut columns = left_schema.columns.clone();
        columns.extend(right_schema.columns.clone());
        let output_schema = Arc::new(Schema { columns });

        // Create evaluator with output schema for evaluating join conditions on combined tuples
        let evaluator = crate::sql::Evaluator::new(output_schema.clone());

        // Create separate evaluators for key extraction (left and right schemas)
        let left_evaluator = crate::sql::Evaluator::new(left_schema);
        let right_evaluator = crate::sql::Evaluator::new(right_schema);

        // Create the operator instance
        let mut operator = Self {
            left,
            join_type,
            on_condition,
            hash_table: std::collections::HashMap::new(),
            output_schema,
            evaluator,
            left_evaluator,
            right_evaluator,
            state: JoinState::Initial,
            current_left_tuple: None,
            current_matches: Vec::new(),
            match_index: 0,
            matched_right_keys: std::collections::HashSet::new(),
            unmatched_right_iter: None,
            unmatched_right_current: None,
            memory_limit,
            memory_used: 0,
            right_column_count,
            left_column_count,
            timeout_ctx: timeout_ctx.clone(),
        };

        // Execute build phase during construction
        operator.build_phase(&mut right)?;

        Ok(operator)
    }

    /// Set timeout context (no-op since timeout is set during construction)
    pub fn with_timeout(self, _timeout_ctx: Option<TimeoutContext>) -> Self {
        // Timeout already set during construction, ignore this call
        self
    }

    /// Execute build phase: construct hash table from right side
    ///
    /// This phase materializes ALL tuples from the right (build) input
    /// into an in-memory hash table indexed by join keys.
    fn build_phase(&mut self, right: &mut Box<dyn PhysicalOperator>) -> Result<()> {
        tracing::debug!(
            "HashJoin build_phase: right_schema columns = {:?}",
            right.schema().columns.iter().map(|c| &c.name).collect::<Vec<_>>()
        );

        // Read all tuples from right (build) side (with timeout checking)
        while let Some(tuple) = right.next()? {
            tracing::debug!("HashJoin build: tuple = {:?}", tuple.values);
            // Check timeout during hash table build (blocking operation)
            if let Some(ref ctx) = self.timeout_ctx {
                ctx.check_timeout()?;
            }

            // Extract join key from tuple
            let key_opt = self.extract_join_key(&tuple, true)?;
            tracing::debug!("HashJoin build: extracted key = {:?}", key_opt);

            // Skip tuples with NULL join keys (they will never match per SQL standard)
            let key = match key_opt {
                Some(k) => k,
                None => continue, // Skip this tuple
            };

            // Estimate memory for this tuple
            let tuple_size = Self::estimate_tuple_size(&tuple);
            let key_size = Self::estimate_key_size(&key);
            let entry_overhead = 24; // HashMap entry overhead
            let additional_memory = tuple_size + key_size + entry_overhead;

            // Check memory limit
            if self.memory_used + additional_memory > self.memory_limit {
                return Err(Error::query_execution(
                    format!(
                        "Hash join exceeds memory limit ({} bytes). Consider using nested loop join or increasing limit.",
                        self.memory_limit
                    )
                ));
            }

            // Insert into hash table (with overflow chaining)
            self.hash_table
                .entry(key)
                .or_insert_with(Vec::new)
                .push(tuple);

            self.memory_used += additional_memory;
        }

        // Transition to probe phase
        self.state = JoinState::Probing;
        Ok(())
    }

    /// Extract join key from a tuple
    ///
    /// For ON clause like: a.id = b.id AND a.type = b.type
    /// Extract values of [a.id, a.type] or [b.id, b.type] depending on side
    ///
    /// Returns None if any join key value is NULL (per SQL standard, NULLs never match in joins)
    fn extract_join_key(&self, tuple: &Tuple, is_right_side: bool) -> Result<Option<JoinKey>> {
        if let Some(condition) = &self.on_condition {
            let key_values = self.extract_join_columns(condition, tuple, is_right_side)?;

            // Check if any key value is NULL - if so, this tuple will never match
            if key_values.iter().any(|v| matches!(v, crate::Value::Null)) {
                return Ok(None);
            }

            Ok(Some(JoinKey(key_values)))
        } else {
            // Cross join - use empty key (all tuples match)
            Ok(Some(JoinKey(vec![])))
        }
    }

    /// Extract join column values from ON condition
    fn extract_join_columns(
        &self,
        condition: &crate::sql::LogicalExpr,
        tuple: &Tuple,
        is_right_side: bool,
    ) -> Result<Vec<crate::Value>> {
        use crate::sql::{LogicalExpr, BinaryOperator};

        // Use the appropriate evaluator based on which side's tuple we're evaluating
        let evaluator = if is_right_side { &self.right_evaluator } else { &self.left_evaluator };

        match condition {
            LogicalExpr::BinaryExpr { left, op, right } => {
                match op {
                    BinaryOperator::Eq => {
                        // Single equality: extract the appropriate side
                        let expr = if is_right_side { right } else { left };
                        let value = evaluator.evaluate(expr, tuple)?;
                        Ok(vec![value])
                    }
                    BinaryOperator::And => {
                        // Composite key: a.x = b.x AND a.y = b.y
                        let mut values = self.extract_join_columns(left, tuple, is_right_side)?;
                        values.extend(self.extract_join_columns(right, tuple, is_right_side)?);
                        Ok(values)
                    }
                    _ => {
                        // For non-equality joins, use empty key (fallback to full scan)
                        Ok(vec![])
                    }
                }
            }
            _ => {
                // For complex conditions, use empty key
                Ok(vec![])
            }
        }
    }

    /// Probe phase: stream left side, lookup matches in hash table
    fn probe_phase(&mut self) -> Result<Option<Tuple>> {
        loop {
            // Check timeout during probe loop
            if let Some(ref ctx) = self.timeout_ctx {
                ctx.check_timeout()?;
            }

            // If we have pending matches for current left tuple, emit them
            if self.match_index < self.current_matches.len() {
                let right_tuple = &self.current_matches[self.match_index];
                self.match_index += 1;

                let left_tuple = self.current_left_tuple.as_ref()
                    .ok_or_else(|| Error::query_execution("Missing left tuple"))?;

                return Ok(Some(Self::join_tuples(left_tuple, right_tuple)));
            }

            // Get next left tuple
            match self.left.next()? {
                None => {
                    // No more left tuples
                    // For outer joins, emit unmatched tuples
                    if matches!(self.join_type, crate::sql::JoinType::Right | crate::sql::JoinType::Full) {
                        self.state = JoinState::EmittingUnmatched;
                        return self.emit_unmatched();
                    }

                    self.state = JoinState::Exhausted;
                    return Ok(None);
                }
                Some(left_tuple) => {
                    // Extract join key and probe hash table
                    let key_opt = self.extract_join_key(&left_tuple, false)?;
                    tracing::debug!(
                        "HashJoin probe: left_tuple = {:?}, extracted key = {:?}",
                        left_tuple.values, key_opt
                    );

                    // If join key contains NULL, this tuple will never match
                    let key = match key_opt {
                        Some(k) => k,
                        None => {
                            // For LEFT/FULL join, emit with NULLs
                            if matches!(self.join_type, crate::sql::JoinType::Left | crate::sql::JoinType::Full) {
                                return Ok(Some(self.join_with_nulls_right(&left_tuple)));
                            }
                            // For INNER join, skip
                            continue;
                        }
                    };

                    // Lookup in hash table
                    if let Some(matches) = self.hash_table.get(&key) {
                        // Found matches - filter by full join condition
                        let filtered_matches: Vec<Tuple> = matches.iter()
                            .filter(|right_tuple| {
                                self.evaluate_join_condition(&left_tuple, right_tuple)
                                    .unwrap_or(false)
                            })
                            .cloned()
                            .collect();

                        if !filtered_matches.is_empty() {
                            // Mark key as matched (for RIGHT/FULL joins)
                            if matches!(self.join_type, crate::sql::JoinType::Right | crate::sql::JoinType::Full) {
                                self.matched_right_keys.insert(key);
                            }

                            // Store matches and emit first one
                            self.current_left_tuple = Some(left_tuple);
                            self.current_matches = filtered_matches;
                            self.match_index = 0;

                            // Continue loop to emit first match
                            continue;
                        }
                    }

                    // No matches found for this left tuple
                    // For LEFT/FULL join, emit with NULLs
                    if matches!(self.join_type, crate::sql::JoinType::Left | crate::sql::JoinType::Full) {
                        return Ok(Some(self.join_with_nulls_right(&left_tuple)));
                    }

                    // For INNER join, skip this tuple
                    continue;
                }
            }
        }
    }

    /// Evaluate full join condition on combined tuple
    ///
    /// For pure equi-joins (only equality predicates), this always returns true
    /// because the hash lookup already verified the join keys match. This avoids
    /// issues with duplicate column names in the combined schema where the evaluator
    /// might find the wrong column (e.g., finding employees.id instead of departments.id).
    fn evaluate_join_condition(&self, left: &Tuple, right: &Tuple) -> Result<bool> {
        if let Some(condition) = &self.on_condition {
            // For pure equi-joins, the hash lookup already confirmed the keys match.
            // We skip re-evaluation because the combined schema may have duplicate
            // column names, causing the evaluator to match the wrong column.
            if is_equi_join(&self.on_condition) {
                return Ok(true);
            }

            // For non-equi-joins (with additional predicates beyond equality),
            // we need to evaluate the full condition on the combined tuple.
            // Note: This path currently has a limitation with duplicate column names.
            let combined = Self::join_tuples(left, right);

            // Evaluate condition
            let result = self.evaluator.evaluate(condition, &combined)?;

            match result {
                crate::Value::Boolean(b) => Ok(b),
                crate::Value::Null => Ok(false), // NULL is treated as false in join conditions
                _ => Ok(false),
            }
        } else {
            // No condition = cross join = always match
            Ok(true)
        }
    }

    /// Emit unmatched tuples from right side (for RIGHT/FULL joins)
    fn emit_unmatched(&mut self) -> Result<Option<Tuple>> {
        // Initialize iterator if not already done
        if self.unmatched_right_iter.is_none() {
            // Collect unmatched right tuples
            let unmatched: Vec<_> = self.hash_table
                .iter()
                .filter(|(key, _)| !self.matched_right_keys.contains(key))
                .map(|(key, tuples)| (key.clone(), tuples.clone()))
                .collect();

            self.unmatched_right_iter = Some(unmatched.into_iter());
        }

        // Emit tuples from current bucket
        if let Some(ref mut current_iter) = self.unmatched_right_current {
            if let Some(right_tuple) = current_iter.next() {
                return Ok(Some(self.join_with_nulls_left(&right_tuple)));
            }
        }

        // Move to next bucket
        if let Some(ref mut iter) = self.unmatched_right_iter {
            if let Some((_, tuples)) = iter.next() {
                self.unmatched_right_current = Some(tuples.into_iter());
                return self.emit_unmatched();
            }
        }

        // All done
        self.state = JoinState::Exhausted;
        Ok(None)
    }

    /// Join two tuples (concatenate values)
    fn join_tuples(left: &Tuple, right: &Tuple) -> Tuple {
        let mut values = left.values.clone();
        values.extend(right.values.clone());
        Tuple::new(values)
    }

    /// Join left tuple with NULLs (for unmatched left tuple in LEFT/FULL join)
    fn join_with_nulls_right(&self, left: &Tuple) -> Tuple {
        let mut values = left.values.clone();
        values.extend(vec![crate::Value::Null; self.right_column_count]);
        Tuple::new(values)
    }

    /// Join right tuple with NULLs (for unmatched right tuple in RIGHT/FULL join)
    fn join_with_nulls_left(&self, right: &Tuple) -> Tuple {
        let mut values = vec![crate::Value::Null; self.left_column_count];
        values.extend(right.values.clone());
        Tuple::new(values)
    }

    /// Estimate memory size of a tuple
    fn estimate_tuple_size(tuple: &Tuple) -> usize {
        let base = 24; // Vec overhead
        let values_size: usize = tuple.values.iter()
            .map(|v| Self::estimate_value_size(v))
            .sum();
        base + values_size
    }

    /// Estimate memory size of a value
    fn estimate_value_size(value: &crate::Value) -> usize {
        use crate::Value;
        match value {
            Value::Null => 1,
            Value::Boolean(_) => 1,
            Value::Int2(_) => 2,
            Value::Int4(_) => 4,
            Value::Int8(_) => 8,
            Value::Float4(_) => 4,
            Value::Float8(_) => 8,
            Value::Numeric(n) => 24 + n.len(),
            Value::String(s) => 24 + s.len(),
            Value::Bytes(b) => 24 + b.len(),
            Value::Vector(v) => 24 + v.len() * 4,
            Value::Array(arr) => 24 + arr.iter().map(Self::estimate_value_size).sum::<usize>(),
            Value::Json(_) => 256, // Rough estimate
            Value::Uuid(_) => 16,
            Value::Timestamp(_) => 16,
            Value::Date(_) => 4,
            Value::Time(_) => 8,
            // Storage references
            Value::DictRef { .. } => 4,
            Value::CasRef { .. } => 32,
            Value::ColumnarRef => 1,
        }
    }

    /// Estimate memory size of a join key
    fn estimate_key_size(key: &JoinKey) -> usize {
        24 + key.0.iter().map(Self::estimate_value_size).sum::<usize>()
    }
}

impl PhysicalOperator for HashJoinOperator {
    fn next(&mut self) -> Result<Option<Tuple>> {
        // Build phase is executed during construction, so we start in Probing state
        match self.state {
            JoinState::Probing => self.probe_phase(),
            JoinState::EmittingUnmatched => self.emit_unmatched(),
            JoinState::Exhausted => Ok(None),
            JoinState::Initial => {
                // Should never happen as build phase runs during construction
                Err(Error::query_execution("HashJoinOperator in invalid initial state"))
            }
        }
    }

    fn schema(&self) -> Arc<Schema> {
        self.output_schema.clone()
    }
}

/// Handle Join logical plan node
pub(super) fn handle_join(
    executor: &Executor,
    left: &crate::sql::LogicalPlan,
    right: &crate::sql::LogicalPlan,
    join_type: &crate::sql::JoinType,
    on: &Option<crate::sql::LogicalExpr>,
) -> Result<Box<dyn PhysicalOperator>> {
    let left_op = executor.plan_to_operator(left)?;
    let right_op = executor.plan_to_operator(right)?;
    let timeout_ctx = executor.timeout_ctx();

    // Use HashJoin for equi-joins (with equality conditions)
    // Fall back to NestedLoopJoin for non-equi joins or cross joins
    if is_equi_join(on) {
        Ok(Box::new(HashJoinOperator::new(
            left_op,
            right_op,
            join_type.clone(),
            on.clone(),
            timeout_ctx,
        )?))
    } else {
        Ok(Box::new(NestedLoopJoinOperator::new(
            left_op,
            right_op,
            join_type.clone(),
            on.clone(),
            timeout_ctx,
        )?))
    }
}

/// Check if a join condition is an equi-join (uses only equality predicates)
///
/// Returns true if the condition contains only equality comparisons (=)
/// combined with AND, which allows efficient hash join implementation.
fn is_equi_join(condition: &Option<crate::sql::LogicalExpr>) -> bool {
    match condition {
        None => true, // Cross join - can use hash join with empty key
        Some(expr) => is_equi_join_expr(expr),
    }
}

/// Recursively check if an expression is suitable for equi-join
fn is_equi_join_expr(expr: &crate::sql::LogicalExpr) -> bool {
    use crate::sql::{LogicalExpr, BinaryOperator};

    match expr {
        LogicalExpr::BinaryExpr { left, op, right } => {
            match op {
                BinaryOperator::Eq => true, // Equality is perfect for hash join
                BinaryOperator::And => {
                    // Both sides must be equi-joins
                    is_equi_join_expr(left) && is_equi_join_expr(right)
                }
                _ => false, // Other operators require nested loop
            }
        }
        _ => false, // Complex expressions require nested loop
    }
}
