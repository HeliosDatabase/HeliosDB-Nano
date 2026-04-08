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
/// Supports INNER, LEFT, RIGHT, FULL, and CROSS joins.
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
    // Outer join state
    left_column_count: usize,
    right_column_count: usize,
    left_matched: bool,  // Did current left tuple match any right tuple?
    right_matched: Vec<bool>,  // Which right tuples have been matched?
    emitting_unmatched_right: bool,  // Are we emitting unmatched right tuples?
    unmatched_right_index: usize,  // Index into right_tuples for unmatched emission
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

        let left_column_count = left_schema.columns.len();
        let right_column_count = right_schema.columns.len();

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

        let right_count = right_tuples.len();

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
            left_column_count,
            right_column_count,
            left_matched: false,
            right_matched: vec![false; right_count],
            emitting_unmatched_right: false,
            unmatched_right_index: 0,
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
        use crate::sql::JoinType;

        // Handle unmatched right tuples phase (for RIGHT/FULL joins)
        if self.emitting_unmatched_right {
            return self.emit_unmatched_right();
        }

        loop {
            // Check timeout during nested loop iteration
            if let Some(ref ctx) = self.timeout_ctx {
                ctx.check_timeout()?;
            }

            // If we don't have a left tuple, get the next one
            if self.left_tuple.is_none() {
                self.left_tuple = self.left.next()?;

                // If no more left tuples, handle outer join completion
                if self.left_tuple.is_none() {
                    // For RIGHT/FULL joins, emit unmatched right tuples
                    if matches!(self.join_type, JoinType::Right | JoinType::Full) {
                        self.emitting_unmatched_right = true;
                        return self.emit_unmatched_right();
                    }
                    return Ok(None);
                }

                // Reset right index for new left tuple
                self.right_index = 0;
                self.left_matched = false;
            }

            // Try to find a matching right tuple
            while self.right_index < self.right_tuples.len() {
                let right_idx = self.right_index;
                let right_tuple = self.right_tuples.get(right_idx)
                    .ok_or_else(|| Error::query_execution("Right tuple index out of bounds"))?;
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
                    self.left_matched = true;
                    // Mark right tuple as matched (for RIGHT/FULL joins)
                    if matches!(self.join_type, JoinType::Right | JoinType::Full) {
                        if let Some(matched) = self.right_matched.get_mut(right_idx) {
                            *matched = true;
                        }
                    }
                    return Ok(Some(combined_tuple));
                }
            }

            // No more right tuples for this left tuple
            // For LEFT/FULL joins, emit unmatched left tuple with NULLs
            if !self.left_matched && matches!(self.join_type, JoinType::Left | JoinType::Full) {
                let left_tuple = self.left_tuple.as_ref()
                    .ok_or_else(|| Error::query_execution("Left tuple unexpectedly None"))?;
                let result = self.join_with_nulls_right(left_tuple);
                self.left_tuple = None;
                return Ok(Some(result));
            }

            // Get next left tuple
            self.left_tuple = None;
        }
    }

    fn schema(&self) -> Arc<Schema> {
        self.output_schema.clone()
    }
}

impl NestedLoopJoinOperator {
    /// Emit unmatched right tuples with NULL left columns (for RIGHT/FULL joins)
    fn emit_unmatched_right(&mut self) -> Result<Option<Tuple>> {
        while self.unmatched_right_index < self.right_tuples.len() {
            let idx = self.unmatched_right_index;
            self.unmatched_right_index += 1;

            if !self.right_matched.get(idx).copied().unwrap_or(false) {
                let right_tuple = self.right_tuples.get(idx)
                    .ok_or_else(|| Error::query_execution("Right tuple index out of bounds"))?;
                return Ok(Some(self.join_with_nulls_left(right_tuple)));
            }
        }
        Ok(None)
    }

    /// Join left tuple with NULLs for right columns
    fn join_with_nulls_right(&self, left: &Tuple) -> Tuple {
        let mut values = left.values.clone();
        values.extend(vec![crate::Value::Null; self.right_column_count]);
        Tuple::new(values)
    }

    /// Join right tuple with NULLs for left columns
    fn join_with_nulls_left(&self, right: &Tuple) -> Tuple {
        let mut values = vec![crate::Value::Null; self.left_column_count];
        values.extend(right.values.clone());
        Tuple::new(values)
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

/// Join key for hash table lookups.
///
/// Implements custom PartialEq/Hash so that Int2(1), Int4(1), Int8(1) all
/// match each other in the hash table. This is critical for JOINs where one
/// side has SERIAL (Int4) and the other BIGSERIAL (Int8).
#[derive(Debug, Clone)]
struct JoinKey(Vec<crate::Value>);

impl PartialEq for JoinKey {
    fn eq(&self, other: &Self) -> bool {
        if self.0.len() != other.0.len() {
            return false;
        }
        self.0.iter().zip(other.0.iter()).all(|(a, b)| values_equal_for_join(a, b))
    }
}
impl Eq for JoinKey {}

impl std::hash::Hash for JoinKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.len().hash(state);
        for v in &self.0 {
            v.hash(state);
        }
    }
}

/// Compare two values for join equality, with cross-type numeric coercion.
fn values_equal_for_join(a: &crate::Value, b: &crate::Value) -> bool {
    use crate::Value;
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => false,
        // Same type — direct compare
        (Value::Int2(x), Value::Int2(y)) => x == y,
        (Value::Int4(x), Value::Int4(y)) => x == y,
        (Value::Int8(x), Value::Int8(y)) => x == y,
        // Cross-type integer comparison
        (Value::Int2(x), Value::Int4(y)) | (Value::Int4(y), Value::Int2(x)) => i64::from(*x) == i64::from(*y),
        (Value::Int2(x), Value::Int8(y)) | (Value::Int8(y), Value::Int2(x)) => i64::from(*x) == *y,
        (Value::Int4(x), Value::Int8(y)) | (Value::Int8(y), Value::Int4(x)) => i64::from(*x) == *y,
        // String comparison
        (Value::String(x), Value::String(y)) => x == y,
        // Cross-type string/int (MySQL does this freely)
        (Value::String(s), Value::Int4(n)) | (Value::Int4(n), Value::String(s)) => {
            s.parse::<i32>().map_or(false, |parsed| parsed == *n)
        }
        (Value::String(s), Value::Int8(n)) | (Value::Int8(n), Value::String(s)) => {
            s.parse::<i64>().map_or(false, |parsed| parsed == *n)
        }
        // Default: derived PartialEq
        _ => a == b,
    }
}

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
                        // For ON a.x = b.x, the user may write the columns in either order:
                        //   ON left_table.col = right_table.col
                        //   ON right_table.col = left_table.col
                        // First try the "natural" side (left expr for left eval, right for right),
                        // and if that fails fall back to the other side.
                        let (primary, fallback) = if is_right_side { (right, left) } else { (left, right) };
                        match evaluator.evaluate(primary, tuple) {
                            Ok(value) => Ok(vec![value]),
                            Err(_) => {
                                let value = evaluator.evaluate(fallback, tuple)?;
                                Ok(vec![value])
                            }
                        }
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
                let right_tuple = self.current_matches.get(self.match_index)
                    .ok_or_else(|| Error::query_execution("Match index out of bounds"))?;
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
                        // For pure equi-joins, hash lookup already confirmed match —
                        // skip per-tuple filter and clone directly
                        let is_equi = is_pure_equi_join(&self.on_condition);
                        let filtered_matches: Vec<Tuple> = if is_equi {
                            matches.clone()
                        } else {
                            matches.iter()
                                .filter(|right_tuple| {
                                    self.evaluate_join_condition(&left_tuple, right_tuple)
                                        .unwrap_or(false)
                                })
                                .cloned()
                                .collect()
                        };

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
            // The hash join now only receives equi-join conditions (equality predicates).
            // The hash lookup already confirmed the keys match, so skip re-evaluation
            // which can fail due to duplicate column names in the combined schema.
            if is_pure_equi_join(&self.on_condition) {
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
        let mut values = Vec::with_capacity(left.values.len() + right.values.len());
        values.extend_from_slice(&left.values);
        values.extend_from_slice(&right.values);
        Tuple::new(values)
    }

    /// Join left tuple with NULLs (for unmatched left tuple in LEFT/FULL join)
    fn join_with_nulls_right(&self, left: &Tuple) -> Tuple {
        let mut values = Vec::with_capacity(left.values.len() + self.right_column_count);
        values.extend_from_slice(&left.values);
        values.resize(values.len() + self.right_column_count, crate::Value::Null);
        Tuple::new(values)
    }

    /// Join right tuple with NULLs (for unmatched right tuple in RIGHT/FULL join)
    fn join_with_nulls_left(&self, right: &Tuple) -> Tuple {
        let mut values = Vec::with_capacity(self.left_column_count + right.values.len());
        values.resize(self.left_column_count, crate::Value::Null);
        values.extend_from_slice(&right.values);
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
            Value::Interval(_) => 16, // Interval contains months, days, microseconds
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
    executor: &mut Executor,
    left: &crate::sql::LogicalPlan,
    right: &crate::sql::LogicalPlan,
    join_type: &crate::sql::JoinType,
    on: &Option<crate::sql::LogicalExpr>,
    lateral: bool,
) -> Result<Box<dyn PhysicalOperator>> {
    // LATERAL joins require nested loop join (right side depends on left row)
    if lateral {
        let left_op = executor.plan_to_operator(left)?;
        let right_op = executor.plan_to_operator(right)?;
        let timeout_ctx = executor.timeout_ctx();
        return Ok(Box::new(NestedLoopJoinOperator::new(
            left_op,
            right_op,
            join_type.clone(),
            on.clone(),
            timeout_ctx,
        )?));
    }

    // Note: Index-Nested-Loop Join is available via try_index_nested_loop_join()
    // but is currently disabled as hash join + predicate pushdown is faster for
    // small-to-medium tables (individual RocksDB lookups are slower than batch scans).
    // Enable with cardinality-based cost estimation in the future.

    let left_op = executor.plan_to_operator(left)?;
    let right_op = executor.plan_to_operator(right)?;
    let timeout_ctx = executor.timeout_ctx();

    // Split compound ON conditions into equi-join keys and residual filters.
    // This allows hash join even when the condition mixes equality and non-equality predicates.
    match on {
        None => {
            // Cross join — use hash join with empty key
            Ok(Box::new(HashJoinOperator::new(
                left_op,
                right_op,
                join_type.clone(),
                None,
                timeout_ctx,
            )?))
        }
        Some(condition) => {
            let (equi_part, residual_part) = split_join_condition(condition);

            if equi_part.is_some() {
                // Use hash join on equi-join keys
                let mut join_op: Box<dyn PhysicalOperator> = Box::new(HashJoinOperator::new(
                    left_op,
                    right_op,
                    join_type.clone(),
                    equi_part,
                    timeout_ctx,
                )?);

                // Apply residual filter on top if present
                if let Some(residual) = residual_part {
                    join_op = Box::new(super::filter::FilterOperator::new(
                        join_op,
                        residual,
                        vec![],
                    ));
                }

                Ok(join_op)
            } else {
                // No equi-join keys — fall back to nested loop
                Ok(Box::new(NestedLoopJoinOperator::new(
                    left_op,
                    right_op,
                    join_type.clone(),
                    on.clone(),
                    timeout_ctx,
                )?))
            }
        }
    }
}

/// Try to use Index-Nested-Loop Join when the right table has an ART index on the join column.
///
/// Returns `Some(operator)` if INLJ is applicable, `None` otherwise (falls back to hash join).
fn try_index_nested_loop_join(
    executor: &mut Executor,
    left: &crate::sql::LogicalPlan,
    right: &crate::sql::LogicalPlan,
    join_type: &crate::sql::JoinType,
    condition: &crate::sql::LogicalExpr,
) -> Result<Option<Box<dyn PhysicalOperator>>> {
    use crate::storage::art_manager::ArtIndexManager;

    // Phase 1: Check eligibility (immutable borrow of executor/storage)
    let (right_table, right_schema, index_name, left_join_col) = {
        let storage = match executor.storage() {
            Some(s) => s,
            None => return Ok(None),
        };

        // Extract the right table name and schema from a Scan or Filter(Scan) node
        let (right_table, right_alias, right_schema) = match extract_scan_info(right) {
            Some(info) => info,
            None => return Ok(None),
        };

        // Extract equi-join column pair from condition (simple case: single equality)
        let (left_col, right_col) = match extract_equi_columns(condition) {
            Some(pair) => pair,
            None => return Ok(None),
        };

        // Determine which column belongs to the right table
        let right_join_col = if column_matches_table(&right_col, &right_table, right_alias.as_deref()) {
            right_col.1.clone()
        } else if column_matches_table(&left_col, &right_table, right_alias.as_deref()) {
            left_col.1.clone()
        } else {
            return Ok(None);
        };

        // Check if there's an ART index on the right table's join column
        let index_name = match storage.art_indexes().find_column_index(&right_table, &right_join_col) {
            Some(name) => name,
            None => return Ok(None),
        };

        // Determine which column is the left key
        let left_join_col = if column_matches_table(&right_col, &right_table, right_alias.as_deref()) {
            left_col.clone()
        } else {
            right_col.clone()
        };

        (right_table, right_schema, index_name, left_join_col)
    }; // Immutable borrow of executor dropped here

    // Phase 2: Build left operator (mutable borrow of executor)
    let mut left_op = executor.plan_to_operator(left)?;
    let left_schema = left_op.schema();

    // Find the column index of the join key in left tuples
    let left_key_idx = match find_column_index(&left_schema, left_join_col.0.as_deref(), &left_join_col.1) {
        Some(idx) => idx,
        None => return Ok(None),
    };

    // Build output schema (left + right)
    let mut output_columns = left_schema.columns.clone();
    output_columns.extend(right_schema.columns.clone());
    let output_schema = Arc::new(Schema { columns: output_columns });

    let right_col_count = right_schema.columns.len();
    let is_left_join = matches!(join_type, crate::sql::JoinType::Left);

    // Phase 3: Execute INLJ (immutable borrow of executor/storage again)
    let storage = executor.storage()
        .ok_or_else(|| Error::query_execution("Storage unavailable for INLJ"))?;

    let mut result_tuples = Vec::new();

    while let Some(left_tuple) = left_op.next()? {
        // Extract join key value from left tuple
        let key_value = match left_tuple.values.get(left_key_idx) {
            Some(v) if !matches!(v, crate::Value::Null) => v.clone(),
            _ => {
                if is_left_join {
                    let mut combined_values = left_tuple.values.clone();
                    combined_values.resize(combined_values.len() + right_col_count, crate::Value::Null);
                    result_tuples.push(Tuple { values: combined_values, row_id: None, branch_id: None });
                }
                continue;
            }
        };

        // Encode the key for ART lookup
        let encoded_key = ArtIndexManager::encode_key(&[key_value]);

        // Look up all matching row_ids from the ART index
        let matching_row_ids = storage.art_indexes().index_get_all(&index_name, &encoded_key);

        if matching_row_ids.is_empty() {
            if is_left_join {
                let mut combined_values = left_tuple.values.clone();
                combined_values.resize(combined_values.len() + right_col_count, crate::Value::Null);
                result_tuples.push(Tuple { values: combined_values, row_id: None, branch_id: None });
            }
            continue;
        }

        // Fetch each matching right row and combine
        for row_id in matching_row_ids {
            if let Some(right_tuple) = storage.get_row_by_id(&right_table, row_id, &right_schema)? {
                let mut combined_values = Vec::with_capacity(left_tuple.values.len() + right_tuple.values.len());
                combined_values.extend_from_slice(&left_tuple.values);
                combined_values.extend_from_slice(&right_tuple.values);
                result_tuples.push(Tuple { values: combined_values, row_id: None, branch_id: None });
            }
        }
    }

    Ok(Some(Box::new(super::MaterializedOperator::new(result_tuples, output_schema))))
}

/// Extract table name, alias, and schema from a Scan or Filter(Scan) plan node
fn extract_scan_info(plan: &crate::sql::LogicalPlan) -> Option<(String, Option<String>, Arc<Schema>)> {
    match plan {
        crate::sql::LogicalPlan::Scan { table_name, alias, schema, .. } => {
            Some((table_name.clone(), alias.clone(), schema.clone()))
        }
        crate::sql::LogicalPlan::Filter { input, .. } => extract_scan_info(input),
        crate::sql::LogicalPlan::Project { input, .. } => extract_scan_info(input),
        _ => None,
    }
}

/// Extract column pair from a simple equi-join condition: col1 = col2
/// Returns (table_option, column_name) for each side
fn extract_equi_columns(condition: &crate::sql::LogicalExpr) -> Option<((Option<String>, String), (Option<String>, String))> {
    use crate::sql::{LogicalExpr, BinaryOperator};

    match condition {
        LogicalExpr::BinaryExpr { left, op: BinaryOperator::Eq, right } => {
            match (left.as_ref(), right.as_ref()) {
                (LogicalExpr::Column { table: lt, name: ln }, LogicalExpr::Column { table: rt, name: rn }) => {
                    Some(((lt.clone(), ln.clone()), (rt.clone(), rn.clone())))
                }
                _ => None,
            }
        }
        // For compound AND conditions, try the first equality
        LogicalExpr::BinaryExpr { left, op: BinaryOperator::And, .. } => {
            extract_equi_columns(left)
        }
        _ => None,
    }
}

/// Check if a (table, column) pair refers to the given table name or alias
fn column_matches_table(col: &(Option<String>, String), table_name: &str, alias: Option<&str>) -> bool {
    match &col.0 {
        Some(qualifier) => qualifier == table_name || alias.is_some_and(|a| a == qualifier),
        None => false,
    }
}

/// Find the index of a column in a schema by optional table qualifier and column name
fn find_column_index(schema: &Schema, table: Option<&str>, name: &str) -> Option<usize> {
    // Try exact match with table qualifier first
    if let Some(tbl) = table {
        for (i, col) in schema.columns.iter().enumerate() {
            if col.name == name {
                if let Some(ref src) = col.source_table_name {
                    if src == tbl {
                        return Some(i);
                    }
                }
            }
        }
    }
    // Fall back to name-only match
    schema.columns.iter().position(|c| c.name == name)
}

/// Split a join condition into equi-join predicates and residual filters.
///
/// Walks the AND chain and classifies each predicate:
/// - Equality (`=`) predicates → equi-join part (used for hash join keys)
/// - Everything else → residual part (applied as post-join filter)
///
/// Returns `(equi_part, residual_part)` where each is `Option<LogicalExpr>`.
fn split_join_condition(
    condition: &crate::sql::LogicalExpr,
) -> (Option<crate::sql::LogicalExpr>, Option<crate::sql::LogicalExpr>) {
    let mut equi_parts = Vec::new();
    let mut residual_parts = Vec::new();

    collect_and_terms(condition, &mut equi_parts, &mut residual_parts);

    let equi = combine_with_and(equi_parts);
    let residual = combine_with_and(residual_parts);

    (equi, residual)
}

/// Check if a join condition is purely equi-join (only equality + AND).
/// Used internally by HashJoinOperator to skip redundant condition re-evaluation.
fn is_pure_equi_join(condition: &Option<crate::sql::LogicalExpr>) -> bool {
    use crate::sql::{LogicalExpr, BinaryOperator};

    fn check(expr: &LogicalExpr) -> bool {
        match expr {
            LogicalExpr::BinaryExpr { op: BinaryOperator::Eq, .. } => true,
            LogicalExpr::BinaryExpr { left, op: BinaryOperator::And, right } => {
                check(left) && check(right)
            }
            _ => false,
        }
    }

    match condition {
        None => true,
        Some(expr) => check(expr),
    }
}

/// Recursively collect AND-connected terms into equi-join and residual buckets
fn collect_and_terms(
    expr: &crate::sql::LogicalExpr,
    equi: &mut Vec<crate::sql::LogicalExpr>,
    residual: &mut Vec<crate::sql::LogicalExpr>,
) {
    use crate::sql::{LogicalExpr, BinaryOperator};

    match expr {
        LogicalExpr::BinaryExpr { left, op: BinaryOperator::And, right } => {
            collect_and_terms(left, equi, residual);
            collect_and_terms(right, equi, residual);
        }
        LogicalExpr::BinaryExpr { op: BinaryOperator::Eq, .. } => {
            equi.push(expr.clone());
        }
        _ => {
            residual.push(expr.clone());
        }
    }
}

/// Combine a list of predicates with AND
fn combine_with_and(parts: Vec<crate::sql::LogicalExpr>) -> Option<crate::sql::LogicalExpr> {
    use crate::sql::{LogicalExpr, BinaryOperator};

    parts.into_iter().reduce(|left, right| {
        LogicalExpr::BinaryExpr {
            left: Box::new(left),
            op: BinaryOperator::And,
            right: Box::new(right),
        }
    })
}
