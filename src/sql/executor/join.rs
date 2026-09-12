//! Join operators
//!
//! This module provides nested loop join and hash join implementations.

#![allow(elided_lifetimes_in_paths)]

use super::{Executor, PhysicalOperator, TimeoutContext};
use crate::{Error, Result, Schema, Tuple};
use std::hash::Hash;
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
    left_matched: bool,             // Did current left tuple match any right tuple?
    right_matched: Vec<bool>,       // Which right tuples have been matched?
    emitting_unmatched_right: bool, // Are we emitting unmatched right tuples?
    unmatched_right_index: usize,   // Index into right_tuples for unmatched emission
    // R3.5 item 4: when true, the ON condition can be evaluated against a
    // borrowed (left, right) pair view without materializing the combined
    // tuple — output is allocated only for matching pairs.
    condition_pair_evaluable: bool,
}

/// Borrowed (left, right) tuple pair, indexed as if the two value vectors
/// were concatenated — the shape `NestedLoopJoinOperator` previously
/// materialized (with two full `Vec<Value>` clones) for EVERY probed pair
/// just to evaluate the ON condition (R3.5 item 4).
struct PairView<'a> {
    left: &'a Tuple,
    right: &'a Tuple,
}

impl PairView<'_> {
    fn get(&self, index: usize) -> Option<&crate::Value> {
        let split = self.left.values.len();
        if index < split {
            self.left.get(index)
        } else {
            self.right.get(index - split)
        }
    }
}

/// Can `expr` be evaluated by [`eval_condition_on_pair`]? Decided ONCE at
/// operator construction on the *bound* condition. Anything outside the
/// supported set (subqueries, CASE, functions, parameters, unresolved
/// columns, row constructors…) keeps the materialize-then-evaluate path,
/// byte-identical to the previous behavior.
fn pair_evaluable(expr: &crate::sql::LogicalExpr) -> bool {
    use crate::sql::LogicalExpr;
    match expr {
        LogicalExpr::Literal(_) | LogicalExpr::BoundColumn { .. } => true,
        LogicalExpr::BinaryExpr { left, op: _, right } => {
            // Row-constructor comparisons `(a,b) < (c,d)` are intercepted
            // structurally by the full evaluator — leave them on the
            // fallback path.
            !matches!(left.as_ref(), LogicalExpr::Tuple { .. })
                && !matches!(right.as_ref(), LogicalExpr::Tuple { .. })
                && pair_evaluable(left)
                && pair_evaluable(right)
        }
        LogicalExpr::UnaryExpr { expr, .. } | LogicalExpr::IsNull { expr, .. } => pair_evaluable(expr),
        _ => false,
    }
}

/// Evaluate a pair-evaluable ON condition against a borrowed pair view,
/// mirroring `Evaluator::evaluate` semantics exactly (including SQL
/// three-valued AND/OR short-circuit and its error messages) while only
/// cloning the individual column values the condition actually touches.
fn eval_condition_on_pair(
    evaluator: &crate::sql::Evaluator,
    expr: &crate::sql::LogicalExpr,
    pair: &PairView<'_>,
) -> Result<crate::Value> {
    use crate::sql::{BinaryOperator, LogicalExpr};
    use crate::Value;
    match expr {
        LogicalExpr::Literal(value) => Ok(value.clone()),
        LogicalExpr::BoundColumn { index, .. } => pair
            .get(*index)
            .cloned()
            .ok_or_else(|| Error::query_execution(format!("Column index {} out of bounds in tuple", index))),
        LogicalExpr::BinaryExpr { left, op, right } => match op {
            BinaryOperator::And => {
                let left_val = eval_condition_on_pair(evaluator, left, pair)?;
                match &left_val {
                    Value::Boolean(false) => Ok(Value::Boolean(false)),
                    Value::Boolean(true) => {
                        let right_val = eval_condition_on_pair(evaluator, right, pair)?;
                        match &right_val {
                            Value::Boolean(b) => Ok(Value::Boolean(*b)),
                            Value::Null => Ok(Value::Null),
                            _ => Err(Error::query_execution(format!(
                                "Cannot convert {:?} to boolean",
                                right_val
                            ))),
                        }
                    }
                    Value::Null => {
                        let right_val = eval_condition_on_pair(evaluator, right, pair)?;
                        match &right_val {
                            Value::Boolean(false) => Ok(Value::Boolean(false)),
                            Value::Boolean(true) | Value::Null => Ok(Value::Null),
                            _ => Err(Error::query_execution(format!(
                                "Cannot convert {:?} to boolean",
                                right_val
                            ))),
                        }
                    }
                    _ => Err(Error::query_execution(format!(
                        "Cannot convert {:?} to boolean",
                        left_val
                    ))),
                }
            }
            BinaryOperator::Or => {
                let left_val = eval_condition_on_pair(evaluator, left, pair)?;
                match &left_val {
                    Value::Boolean(true) => Ok(Value::Boolean(true)),
                    Value::Boolean(false) => {
                        let right_val = eval_condition_on_pair(evaluator, right, pair)?;
                        match &right_val {
                            Value::Boolean(b) => Ok(Value::Boolean(*b)),
                            Value::Null => Ok(Value::Null),
                            _ => Err(Error::query_execution(format!(
                                "Cannot convert {:?} to boolean",
                                right_val
                            ))),
                        }
                    }
                    Value::Null => {
                        let right_val = eval_condition_on_pair(evaluator, right, pair)?;
                        match &right_val {
                            Value::Boolean(true) => Ok(Value::Boolean(true)),
                            Value::Boolean(false) | Value::Null => Ok(Value::Null),
                            _ => Err(Error::query_execution(format!(
                                "Cannot convert {:?} to boolean",
                                right_val
                            ))),
                        }
                    }
                    _ => Err(Error::query_execution(format!(
                        "Cannot convert {:?} to boolean",
                        left_val
                    ))),
                }
            }
            _ => {
                let left_val = eval_condition_on_pair(evaluator, left, pair)?;
                let right_val = eval_condition_on_pair(evaluator, right, pair)?;
                evaluator.evaluate_binary_op(&left_val, op, &right_val)
            }
        },
        LogicalExpr::UnaryExpr { op, expr } => {
            let val = eval_condition_on_pair(evaluator, expr, pair)?;
            evaluator.evaluate_unary_op(op, &val)
        }
        LogicalExpr::IsNull { expr, is_null } => {
            let val = eval_condition_on_pair(evaluator, expr, pair)?;
            Ok(Value::Boolean(matches!(val, Value::Null) == *is_null))
        }
        // Unreachable: gated by pair_evaluable at construction.
        other => Err(Error::query_execution(format!(
            "Internal error: expression not pair-evaluable: {:?}",
            other
        ))),
    }
}

impl NestedLoopJoinOperator {
    /// `parameters` are the statement's bind values (GH#29 c7, M1). The ON
    /// condition reaches this operator WHOLE — including a residual term such
    /// as `AND b.k = $1`, which the key binder declines precisely because a
    /// parameter is not a column — so its evaluator must carry them. Built
    /// with `Evaluator::new` (an EMPTY parameter vector) through candidate 6,
    /// which turned every parameterized residual into
    /// `Parameter $1 not provided`.
    pub fn new(
        left: Box<dyn PhysicalOperator>,
        right: Box<dyn PhysicalOperator>,
        join_type: crate::sql::JoinType,
        on_condition: Option<crate::sql::LogicalExpr>,
        parameters: Vec<crate::Value>,
        timeout_ctx: Option<TimeoutContext>,
    ) -> Result<Self> {
        Self::with_memory_limit(
            left,
            right,
            join_type,
            on_condition,
            parameters,
            join_memory_limit(),
            timeout_ctx,
        )
    }

    /// [`Self::new`] with an explicit materialization cap, mirroring
    /// [`HashJoinOperator::with_memory_limit`] — the seam the cap is pinned
    /// through without touching the process-global configuration.
    fn with_memory_limit(
        left: Box<dyn PhysicalOperator>,
        mut right: Box<dyn PhysicalOperator>,
        join_type: crate::sql::JoinType,
        on_condition: Option<crate::sql::LogicalExpr>,
        parameters: Vec<crate::Value>,
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

        // Create evaluator with output schema for evaluating join conditions
        let evaluator = crate::sql::Evaluator::with_parameters(output_schema.clone(), parameters);

        // R3.5 items 1+4: bind the ON condition once against the combined
        // schema (safe here — unlike HashJoin, NLJ evaluates the condition
        // against the combined shape only), then decide once whether it can
        // be evaluated against a borrowed pair view without materializing.
        let on_condition = on_condition.map(|condition| evaluator.bind(condition));
        let condition_pair_evaluable = on_condition.as_ref().map(pair_evaluable).unwrap_or(false);

        // Materialize all right tuples upfront (with timeout checking).
        //
        // GH#29 (c7, M3): under the SAME cap as the hash join's build side,
        // and with the same error. Candidate 6 routes every RIGHT / FULL join
        // carrying a residual ON term here, and this materialization was
        // unbounded — the one shape the hash join it replaced was bounded on.
        let mut memory_used: usize = 0;
        let mut right_tuples = Vec::new();
        while let Some(tuple) = right.next()? {
            // Check timeout during right side materialization (blocking operation)
            if let Some(ref ctx) = timeout_ctx {
                ctx.check_timeout()?;
            }
            memory_used = memory_used
                .saturating_add(HashJoinOperator::estimate_tuple_size(&tuple))
                .saturating_add(std::mem::size_of::<bool>());
            if memory_used > memory_limit {
                return Err(join_memory_limit_exceeded(memory_limit));
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
            condition_pair_evaluable,
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
                let right_tuple = self
                    .right_tuples
                    .get(right_idx)
                    .ok_or_else(|| Error::query_execution("Right tuple index out of bounds"))?;
                self.right_index += 1;

                let left_tuple = self
                    .left_tuple
                    .as_ref()
                    .ok_or_else(|| Error::query_execution("Left tuple unexpectedly None"))?;

                // Check the join condition. R3.5 item 4: for pair-evaluable
                // conditions, evaluate against a borrowed (left, right) view
                // — the combined tuple is allocated ONLY for matching pairs.
                // `materialized` carries the combined tuple when the fallback
                // path had to build one, so a match doesn't re-combine.
                let mut materialized: Option<Tuple> = None;
                let matches = if let Some(condition) = &self.on_condition {
                    let result = if self.condition_pair_evaluable {
                        let pair = PairView {
                            left: left_tuple,
                            right: right_tuple,
                        };
                        eval_condition_on_pair(&self.evaluator, condition, &pair)?
                    } else {
                        let mut combined_values = left_tuple.values.clone();
                        combined_values.extend(right_tuple.values.iter().cloned());
                        let combined_tuple = Tuple::new(combined_values);
                        let result = self.evaluator.evaluate(condition, &combined_tuple)?;
                        materialized = Some(combined_tuple);
                        result
                    };
                    match result {
                        crate::Value::Boolean(b) => b,
                        _ => false,
                    }
                } else {
                    // No condition means cross join - all combinations match
                    true
                };

                if matches {
                    let combined_tuple = match materialized {
                        Some(tuple) => tuple,
                        None => {
                            let mut combined_values =
                                Vec::with_capacity(left_tuple.values.len() + right_tuple.values.len());
                            combined_values.extend_from_slice(&left_tuple.values);
                            combined_values.extend_from_slice(&right_tuple.values);
                            Tuple::new(combined_values)
                        }
                    };
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
                let left_tuple = self
                    .left_tuple
                    .as_ref()
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
                let right_tuple = self
                    .right_tuples
                    .get(idx)
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
    // Probe input operator
    left: Box<dyn PhysicalOperator>,

    // Join specification
    join_type: crate::sql::JoinType,
    on_condition: Option<crate::sql::LogicalExpr>,
    /// GH#29 (c7, n5): the part of the ON condition the hash keys do NOT
    /// express, checked on each candidate pair before the pair counts as a
    /// match. `None` for a join whose keys are the whole condition (and for
    /// the legacy path, which re-evaluates `on_condition` itself when its
    /// keys turn out not to cover it).
    ///
    /// BOUND against the combined schema at construction (GH#29 c8, m7), so
    /// the per-pair check can read the two tuples through a borrowed
    /// [`PairView`] instead of cloning every value of both into a combined
    /// tuple, once per candidate pair.
    pair_residual: Option<crate::sql::LogicalExpr>,
    /// Can [`pair_residual`](Self::pair_residual) be evaluated against a
    /// borrowed pair view? Decided ONCE, exactly as the nested loop decides
    /// it for its whole condition; anything else keeps the
    /// materialize-then-evaluate path.
    pair_residual_on_pair: bool,

    // Hash table (key: join columns, value: matching tuple bucket)
    hash_table: std::collections::HashMap<JoinKey, JoinBucket>,

    // Output schema
    output_schema: Arc<Schema>,

    // Expression evaluator for combined tuples (used for condition evaluation after join)
    evaluator: crate::sql::Evaluator,

    // Separate evaluators for left and right sides (used during key extraction)
    left_evaluator: crate::sql::Evaluator,
    right_evaluator: crate::sql::Evaluator,
    direct_key_indices: Option<DirectJoinKeyIndices>,
    /// GH#29 (c4, F1): the key expressions of every `=` term the direct
    /// index path could not take, each operand assigned to its side ONCE at
    /// construction and bound against that side's schema. Empty when the
    /// direct path applies or when no term could be assigned.
    bound_key_pairs: Vec<BoundJoinKeyPair>,
    build_side: HashJoinBuildSide,

    // State machine
    state: JoinState,

    // Probe phase state
    current_left_tuple: Option<Tuple>,
    current_match_key: Option<JoinKey>,
    match_index: usize,
    /// Has the probe row currently being streamed matched at least one build
    /// tuple? Decides the LEFT / FULL NULL extension once the bucket is
    /// exhausted (GH#29 c7, n5: candidate 6 answered that question by
    /// deep-cloning every surviving build tuple into a `Vec` first).
    current_match_found: bool,
    pure_equi_join: bool,
    output_projection: Option<Vec<usize>>,

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

#[derive(Debug, Clone)]
struct DirectJoinKeyIndices {
    left: Vec<usize>,
    right: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HashJoinBuildSide {
    Left,
    Right,
}

enum JoinBucket {
    One(Tuple),
    Many(Vec<Tuple>),
}

impl JoinBucket {
    fn push(&mut self, tuple: Tuple) {
        match self {
            Self::One(existing) => {
                let first = std::mem::replace(existing, Tuple::new(Vec::new()));
                *self = Self::Many(vec![first, tuple]);
            }
            Self::Many(values) => values.push(tuple),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::One(_) => 1,
            Self::Many(values) => values.len(),
        }
    }

    fn get(&self, index: usize) -> Option<&Tuple> {
        match self {
            Self::One(tuple) => (index == 0).then_some(tuple),
            Self::Many(values) => values.get(index),
        }
    }

    fn iter(&self) -> JoinBucketIter<'_> {
        match self {
            Self::One(tuple) => JoinBucketIter::One(Some(tuple)),
            Self::Many(values) => JoinBucketIter::Many(values.iter()),
        }
    }

    fn clone_tuples(&self) -> Vec<Tuple> {
        match self {
            Self::One(tuple) => vec![tuple.clone()],
            Self::Many(values) => values.clone(),
        }
    }
}

enum JoinBucketIter<'a> {
    One(Option<&'a Tuple>),
    Many(std::slice::Iter<'a, Tuple>),
}

impl<'a> Iterator for JoinBucketIter<'a> {
    type Item = &'a Tuple;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::One(value) => value.take(),
            Self::Many(iter) => iter.next(),
        }
    }
}

/// Join key for hash table lookups.
///
/// Implements custom PartialEq/Hash so that Int2(1), Int4(1), Int8(1) all
/// match each other in the hash table. This is critical for JOINs where one
/// side has SERIAL (Int4) and the other BIGSERIAL (Int8).
#[derive(Debug, Clone)]
enum JoinKey {
    Int(i64),
    Single(crate::Value),
    Composite(Vec<crate::Value>),
}

impl JoinKey {
    fn from_values(mut values: Vec<crate::Value>) -> Self {
        if values.len() == 1 {
            Self::from_single_value(values.pop().expect("single value exists"))
        } else {
            Self::Composite(values)
        }
    }

    fn from_single_value(value: crate::Value) -> Self {
        match value {
            crate::Value::Int2(value) => Self::Int(i64::from(value)),
            crate::Value::Int4(value) => Self::Int(i64::from(value)),
            crate::Value::Int8(value) => Self::Int(value),
            value => Self::Single(value),
        }
    }

    fn from_single_value_ref(value: &crate::Value) -> Self {
        match value {
            crate::Value::Int2(value) => Self::Int(i64::from(*value)),
            crate::Value::Int4(value) => Self::Int(i64::from(*value)),
            crate::Value::Int8(value) => Self::Int(*value),
            value => Self::Single(value.clone()),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Int(_) => 1,
            Self::Single(_) => 1,
            Self::Composite(values) => values.len(),
        }
    }
}

impl PartialEq for JoinKey {
    fn eq(&self, other: &Self) -> bool {
        if self.len() != other.len() {
            return false;
        }

        match (self, other) {
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::Int(a), Self::Single(b)) | (Self::Single(b), Self::Int(a)) => int_value_equal_for_join(*a, b),
            (Self::Int(a), Self::Composite(values)) | (Self::Composite(values), Self::Int(a)) => {
                values.first().is_some_and(|b| int_value_equal_for_join(*a, b))
            }
            (Self::Single(a), Self::Single(b)) => values_equal_for_join(a, b),
            (Self::Single(a), Self::Composite(values)) | (Self::Composite(values), Self::Single(a)) => {
                values.first().is_some_and(|b| values_equal_for_join(a, b))
            }
            (Self::Composite(left), Self::Composite(right)) => {
                left.iter().zip(right).all(|(a, b)| values_equal_for_join(a, b))
            }
        }
    }
}
impl Eq for JoinKey {}

impl std::hash::Hash for JoinKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.len().hash(state);
        match self {
            Self::Int(value) => {
                2u8.hash(state);
                value.hash(state);
            }
            Self::Single(value) => hash_value_for_join(value, state),
            Self::Composite(values) => {
                for value in values {
                    hash_value_for_join(value, state);
                }
            }
        }
    }
}

fn int_value_equal_for_join(left: i64, right: &crate::Value) -> bool {
    use crate::Value;
    match right {
        Value::Int2(value) => left == i64::from(*value),
        Value::Int4(value) => left == i64::from(*value),
        Value::Int8(value) => left == *value,
        Value::String(value) => value.parse::<i64>().map_or(false, |parsed| left == parsed),
        _ => false,
    }
}

fn hash_value_for_join<H: std::hash::Hasher>(value: &crate::Value, state: &mut H) {
    use crate::Value;
    match value {
        Value::String(s) => {
            if let Ok(n) = s.parse::<i64>() {
                2u8.hash(state);
                n.hash(state);
            } else if let Ok(uuid) = uuid::Uuid::parse_str(s) {
                10u8.hash(state);
                uuid.hash(state);
            } else {
                value.hash(state);
            }
        }
        Value::Uuid(uuid) => {
            10u8.hash(state);
            uuid.hash(state);
        }
        _ => value.hash(state),
    }
}

fn string_equals_uuid(s: &str, uuid: &uuid::Uuid) -> bool {
    uuid::Uuid::parse_str(s).is_ok_and(|parsed| parsed == *uuid)
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
        // UUID/string coercion
        (Value::Uuid(uuid), Value::String(s)) | (Value::String(s), Value::Uuid(uuid)) => string_equals_uuid(s, uuid),
        // String comparison
        (Value::String(x), Value::String(y)) => x == y,
        // Cross-type string/int (MySQL does this freely)
        (Value::String(s), Value::Int4(n)) | (Value::Int4(n), Value::String(s)) => {
            s.parse::<i32>().map_or(false, |parsed| parsed == *n)
        }
        (Value::String(s), Value::Int8(n)) | (Value::Int8(n), Value::String(s)) => {
            s.parse::<i64>().map_or(false, |parsed| parsed == *n)
        }
        (Value::String(s), Value::Int2(n)) | (Value::Int2(n), Value::String(s)) => {
            s.parse::<i16>().map_or(false, |parsed| parsed == *n)
        }
        // Default: derived PartialEq
        _ => a == b,
    }
}

#[derive(Debug, Clone, Copy)]
enum DirectJoinKeySide {
    Left(usize),
    Right(usize),
}

fn build_direct_join_key_indices(
    condition: &crate::sql::LogicalExpr,
    left_schema: &Schema,
    right_schema: &Schema,
) -> Option<DirectJoinKeyIndices> {
    let mut pairs = Vec::new();
    collect_direct_equi_pairs(condition, &mut pairs)?;
    if pairs.is_empty() {
        return None;
    }

    let mut left = Vec::with_capacity(pairs.len());
    let mut right = Vec::with_capacity(pairs.len());
    for (lhs, rhs) in pairs {
        let lhs_side = direct_join_key_side(lhs, left_schema, right_schema)?;
        let rhs_side = direct_join_key_side(rhs, left_schema, right_schema)?;
        match (lhs_side, rhs_side) {
            (DirectJoinKeySide::Left(left_idx), DirectJoinKeySide::Right(right_idx))
            | (DirectJoinKeySide::Right(right_idx), DirectJoinKeySide::Left(left_idx)) => {
                left.push(left_idx);
                right.push(right_idx);
            }
            _ => return None,
        }
    }

    Some(DirectJoinKeyIndices { left, right })
}

fn collect_direct_equi_pairs<'a>(
    expr: &'a crate::sql::LogicalExpr,
    pairs: &mut Vec<(&'a crate::sql::LogicalExpr, &'a crate::sql::LogicalExpr)>,
) -> Option<()> {
    use crate::sql::{BinaryOperator, LogicalExpr};
    match expr {
        LogicalExpr::BinaryExpr {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            collect_direct_equi_pairs(left, pairs)?;
            collect_direct_equi_pairs(right, pairs)
        }
        LogicalExpr::BinaryExpr {
            left,
            op: BinaryOperator::Eq,
            right,
        } => {
            pairs.push((left, right));
            Some(())
        }
        _ => None,
    }
}

/// Which of a join input's columns a qualifier is matched against
/// (GH#29 c3, M1): the alias a `FROM` entry is known by (`source_table`) or
/// the real table name behind an aliased base table (`source_table_name`).
/// Alias matches are tried on BOTH sides before any real-name match is
/// considered, so `FROM t AS a JOIN (SELECT id FROM u) t ON t.id = a.id`
/// keys `t.id` on the derived table (alias `t`), never on `a`'s column
/// whose real table happens to be `t`.
#[derive(Clone, Copy)]
enum DirectJoinQualifierTier {
    Alias,
    RealName,
}

/// Outcome of resolving one join-key column against one join input.
enum DirectJoinMatch {
    None,
    Unique(usize),
    Ambiguous,
}

/// How a key column's qualifier and name are compared against a join
/// input's columns (GH#29 c5, MAJOR). The per-tuple evaluator this resolver
/// stands in for (`Schema::get_qualified_column_index`, exact `==`) tells
/// `FROM t AS "A" JOIN t AS "a"` apart — two legal, case-distinct quoted
/// aliases — so the EXACT pass runs first on both sides, and the ASCII
/// case-folded pass (the leniency for unquoted / lower-cased spellings)
/// is consulted only when the exact pass matched on NEITHER side. Through
/// candidate 4 the folded comparison was the only one: `"a".id` matched
/// alias `"A"` too, both operands of `ON "a".id = "A".id + 1` fitted both
/// sides, and the natural-order guess keyed the join backwards.
#[derive(Clone, Copy)]
enum DirectJoinNameCase {
    Exact,
    IgnoreAsciiCase,
}

fn direct_join_key_side(
    expr: &crate::sql::LogicalExpr,
    left_schema: &Schema,
    right_schema: &Schema,
) -> Option<DirectJoinKeySide> {
    let crate::sql::LogicalExpr::Column { table, name } = expr else {
        return None;
    };
    let pick = |left: DirectJoinMatch, right: DirectJoinMatch| match (left, right) {
        (DirectJoinMatch::Unique(idx), DirectJoinMatch::None) => Some(DirectJoinKeySide::Left(idx)),
        (DirectJoinMatch::None, DirectJoinMatch::Unique(idx)) => Some(DirectJoinKeySide::Right(idx)),
        _ => None,
    };
    let Some(qualifier) = table.as_deref() else {
        let (left, right) =
            resolve_join_column_both_sides(left_schema, right_schema, None, name, DirectJoinQualifierTier::Alias);
        return pick(left, right);
    };
    // Tier 1: the qualifier names an alias on either side. Any alias match
    // (unique or not) settles the question at this tier; the real-name tier
    // is consulted only when no column on either side carries the alias.
    let (left_alias, right_alias) = resolve_join_column_both_sides(
        left_schema,
        right_schema,
        Some(qualifier),
        name,
        DirectJoinQualifierTier::Alias,
    );
    if !matches!(left_alias, DirectJoinMatch::None) || !matches!(right_alias, DirectJoinMatch::None) {
        return pick(left_alias, right_alias);
    }
    // Tier 2: the qualifier is the real name of an aliased base table.
    let (left_real, right_real) = resolve_join_column_both_sides(
        left_schema,
        right_schema,
        Some(qualifier),
        name,
        DirectJoinQualifierTier::RealName,
    );
    pick(left_real, right_real)
}

/// Resolve one key column on BOTH inputs at one qualifier tier: the
/// exact-case pass first; the ASCII-case-folded pass only when the exact
/// pass matched on neither side (see [`DirectJoinNameCase`]).
fn resolve_join_column_both_sides(
    left_schema: &Schema,
    right_schema: &Schema,
    qualifier: Option<&str>,
    name: &str,
    tier: DirectJoinQualifierTier,
) -> (DirectJoinMatch, DirectJoinMatch) {
    let exact_left = resolve_direct_join_column(left_schema, qualifier, name, tier, DirectJoinNameCase::Exact);
    let exact_right = resolve_direct_join_column(right_schema, qualifier, name, tier, DirectJoinNameCase::Exact);
    if !matches!(exact_left, DirectJoinMatch::None) || !matches!(exact_right, DirectJoinMatch::None) {
        return (exact_left, exact_right);
    }
    (
        resolve_direct_join_column(left_schema, qualifier, name, tier, DirectJoinNameCase::IgnoreAsciiCase),
        resolve_direct_join_column(right_schema, qualifier, name, tier, DirectJoinNameCase::IgnoreAsciiCase),
    )
}

fn resolve_direct_join_column(
    schema: &Schema,
    qualifier: Option<&str>,
    name: &str,
    tier: DirectJoinQualifierTier,
    case: DirectJoinNameCase,
) -> DirectJoinMatch {
    let same = |value: Option<&str>, expected: &str| match case {
        DirectJoinNameCase::Exact => value == Some(expected),
        DirectJoinNameCase::IgnoreAsciiCase => option_eq_ignore_ascii_case(value, expected),
    };
    let mut matches = schema.columns.iter().enumerate().filter(|(_, column)| {
        if !same(Some(column.name.as_str()), name) {
            return false;
        }
        qualifier.map_or(true, |q| match tier {
            DirectJoinQualifierTier::Alias => same(column.source_table.as_deref(), q),
            DirectJoinQualifierTier::RealName => same(column.source_table_name.as_deref(), q),
        })
    });
    let Some((idx, _)) = matches.next() else {
        return DirectJoinMatch::None;
    };
    if matches.next().is_some() {
        DirectJoinMatch::Ambiguous
    } else {
        DirectJoinMatch::Unique(idx)
    }
}

fn option_eq_ignore_ascii_case(value: Option<&str>, expected: &str) -> bool {
    value.is_some_and(|value| value.eq_ignore_ascii_case(expected))
}

/// One `=` term of a hash-join ON condition with each operand assigned to
/// the join input it evaluates on and BOUND (`BoundColumn`) against that
/// input's schema (GH#29 c4, F1). Decided once at construction, never per
/// tuple: with `FROM u AS a JOIN (SELECT id FROM t) u ON u.id + 10 = a.id`
/// the left input (alias `a`, real name `u`) could evaluate `u.id + 10`
/// through the real-name fallback, so a per-tuple "natural operand, other
/// operand on `Err`" rule keyed the left side on `u.id + 10` (a's own id)
/// and the right side on `u.id + 10` too — the keys never met.
#[derive(Debug, Clone)]
struct BoundJoinKeyPair {
    left: crate::sql::LogicalExpr,
    right: crate::sql::LogicalExpr,
}

/// Which join inputs one key operand can be evaluated on: the operand bound
/// against the left schema when every column it references resolves there,
/// likewise for the right. An operand without column references (a
/// literal) fits both — and is never keyed (GH#29 c5, m3): the hash key
/// compares raw values with no int / float / numeric coercion, so
/// `5 = b.price` on a NUMERIC column hashed `Int(5)` against `Numeric(5)`
/// and matched nothing; declined, the term is re-evaluated by the
/// coercing evaluator per candidate pair.
struct JoinOperandFit {
    left: Option<crate::sql::LogicalExpr>,
    right: Option<crate::sql::LogicalExpr>,
    has_column_refs: bool,
    /// Did any column reference of this operand carry a QUALIFIER
    /// (`t.c`)? GH#29 (c6, BLOCKER): the both-fit-both decline below is
    /// about a qualifier that names a relation on both sides; an operand
    /// spelled with bare names only fits both sides by definition, and
    /// declining THAT is what turned every `NATURAL JOIN` / `JOIN … USING`
    /// into a cartesian product.
    has_qualified_refs: bool,
}

/// Assign both operands of every `=` term of `condition` to a side (see
/// [`BoundJoinKeyPair`]). Returns the bound pairs and whether they cover
/// the WHOLE condition: a term whose operands cannot be assigned
/// unambiguously — or any non-equality node — is left out of the key, and
/// the caller must then re-evaluate the full condition per candidate pair.
fn bind_hash_join_key_pairs(
    condition: &crate::sql::LogicalExpr,
    left_schema: &Schema,
    right_schema: &Schema,
) -> (Vec<BoundJoinKeyPair>, bool) {
    let mut pairs = Vec::new();
    let mut covered = true;
    collect_bound_key_pairs(condition, left_schema, right_schema, &mut pairs, &mut covered);
    (pairs, covered)
}

fn collect_bound_key_pairs(
    expr: &crate::sql::LogicalExpr,
    left_schema: &Schema,
    right_schema: &Schema,
    pairs: &mut Vec<BoundJoinKeyPair>,
    covered: &mut bool,
) {
    use crate::sql::{BinaryOperator, LogicalExpr};
    match expr {
        LogicalExpr::BinaryExpr {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            collect_bound_key_pairs(left, left_schema, right_schema, pairs, covered);
            collect_bound_key_pairs(right, left_schema, right_schema, pairs, covered);
        }
        LogicalExpr::BinaryExpr {
            left,
            op: BinaryOperator::Eq,
            right,
        } => match bind_join_key_term(left, right, left_schema, right_schema) {
            Some(pair) => pairs.push(pair),
            // The reason was traced by `bind_join_key_term`.
            None => *covered = false,
        },
        _ => {
            tracing::debug!(
                target: "helios::join",
                node = ?expr,
                "hash-join ON node is not an equality; the full condition is re-evaluated per candidate pair"
            );
            *covered = false;
        }
    }
}

/// GH#29 (c5, m4): every declined key term is traced, so a hash join that
/// degrades to per-pair re-evaluation of its ON condition is diagnosable
/// (`RUST_LOG=helios::join=debug`).
fn trace_declined_join_key_term(lhs: &crate::sql::LogicalExpr, rhs: &crate::sql::LogicalExpr, reason: &str) {
    tracing::debug!(
        target: "helios::join",
        lhs = ?lhs,
        rhs = ?rhs,
        reason,
        "hash-join key term declined; the full ON condition is re-evaluated per candidate pair"
    );
}

/// Assign the two operands of one `lhs = rhs` term. The natural order
/// (`lhs` on the left input, `rhs` on the right) is tried first — an
/// unqualified name both inputs carry keeps that first-side behaviour, and
/// it is the `NATURAL JOIN` / `JOIN … USING` lowering's own shape — then
/// the reversed order; a term that fits neither way is declined.
///
/// Declined too: a term with a literal operand (GH#29 c5, m3 — the hash
/// key does not coerce; the re-evaluation does) and a term whose operands
/// BOTH fit BOTH sides *through a qualifier* (GH#29 c5 MAJOR, narrowed in
/// c6 — after the exact-case pass the only legal SQL that reaches that
/// branch is a case-folded alias collision, and guessing the natural order
/// keyed `ON "a".id = "A".id + 1` backwards; the combined evaluator
/// resolves it exactly).
///
/// A declined term is NOT dropped: `plan_join_condition` puts it in the
/// RESIDUAL bucket, and the residual is either checked inside the join or
/// filtered after it (GH#29 c6, m4).
fn bind_join_key_term(
    lhs: &crate::sql::LogicalExpr,
    rhs: &crate::sql::LogicalExpr,
    left_schema: &Schema,
    right_schema: &Schema,
) -> Option<BoundJoinKeyPair> {
    let Some(lhs_fit) = join_operand_fit(lhs, left_schema, right_schema) else {
        trace_declined_join_key_term(lhs, rhs, "left operand carries a node the key binder does not walk");
        return None;
    };
    let Some(rhs_fit) = join_operand_fit(rhs, left_schema, right_schema) else {
        trace_declined_join_key_term(lhs, rhs, "right operand carries a node the key binder does not walk");
        return None;
    };
    if !lhs_fit.has_column_refs || !rhs_fit.has_column_refs {
        trace_declined_join_key_term(
            lhs,
            rhs,
            "an operand references no column (the hash key would not coerce a literal)",
        );
        return None;
    }
    let lhs_fits_both = lhs_fit.left.is_some() && lhs_fit.right.is_some();
    let rhs_fits_both = rhs_fit.left.is_some() && rhs_fit.right.is_some();
    // GH#29 (c6, BLOCKER): only a QUALIFIED double fit is an alias
    // collision. A term whose operands carry bare names only fits both
    // sides by construction — and that is exactly the shape the planner
    // lowers EVERY `NATURAL JOIN` / `JOIN … USING (c)` to, unconditionally:
    // one `=` per shared column, both operands bare. For it the natural
    // order (lhs -> left input, rhs -> right input) is the meaning.
    // Candidate 5 declined it, the key came out empty, and the nested-loop
    // join then bound BOTH bare operands to the left input's slot:
    // `left.c = left.c`, i.e. a cartesian product.
    if lhs_fits_both && rhs_fits_both && (lhs_fit.has_qualified_refs || rhs_fit.has_qualified_refs) {
        trace_declined_join_key_term(lhs, rhs, "both operands resolve on both inputs (alias collision)");
        return None;
    }
    let JoinOperandFit {
        left: lhs_on_left,
        right: lhs_on_right,
        ..
    } = lhs_fit;
    let JoinOperandFit {
        left: rhs_on_left,
        right: rhs_on_right,
        ..
    } = rhs_fit;
    if let (Some(left), Some(right)) = (lhs_on_left, rhs_on_right) {
        return Some(BoundJoinKeyPair { left, right });
    }
    if let (Some(left), Some(right)) = (rhs_on_left, lhs_on_right) {
        return Some(BoundJoinKeyPair { left, right });
    }
    trace_declined_join_key_term(lhs, rhs, "operands fit no (left, right) assignment");
    None
}

/// Resolve every column reference of one key operand against both inputs
/// and bind the operand for each side it fits. `None` when the operand
/// carries a node whose references the binder does not walk (a subquery,
/// an aggregate or window call, a wildcard, a row marker): such a term is
/// declined rather than keyed on a guess.
fn join_operand_fit(
    expr: &crate::sql::LogicalExpr,
    left_schema: &Schema,
    right_schema: &Schema,
) -> Option<JoinOperandFit> {
    use crate::sql::evaluator::map_column_refs;
    use crate::sql::LogicalExpr;
    if contains_operator_managed_node(expr) {
        return None;
    }
    let mut left_indices: Vec<Option<usize>> = Vec::new();
    let mut right_indices: Vec<Option<usize>> = Vec::new();
    let mut has_qualified_refs = false;
    let _ = map_column_refs(expr.clone(), &mut |table, name| {
        let (left, right) = resolve_join_key_ref(table.as_deref(), &name, left_schema, right_schema);
        has_qualified_refs |= table.is_some();
        left_indices.push(left);
        right_indices.push(right);
        LogicalExpr::Column { table, name }
    });
    let has_column_refs = !left_indices.is_empty();
    let bind = |indices: &[Option<usize>]| -> Option<LogicalExpr> {
        if indices.iter().any(Option::is_none) {
            return None;
        }
        // Same walker, same expression, same visiting order as above.
        let mut next = indices.iter();
        Some(map_column_refs(
            expr.clone(),
            &mut |table, name| match next.next().copied().flatten() {
                Some(index) => LogicalExpr::BoundColumn { index, table, name },
                None => LogicalExpr::Column { table, name },
            },
        ))
    };
    Some(JoinOperandFit {
        left: bind(&left_indices),
        right: bind(&right_indices),
        has_column_refs,
        has_qualified_refs,
    })
}

/// Does `expr` contain a node whose column references `map_column_refs`
/// does not descend into (or that cannot be a hash key at all)?
fn contains_operator_managed_node(expr: &crate::sql::LogicalExpr) -> bool {
    use crate::sql::LogicalExpr;
    match expr {
        LogicalExpr::Column { .. }
        | LogicalExpr::BoundColumn { .. }
        | LogicalExpr::Literal(_)
        | LogicalExpr::Parameter { .. } => false,
        LogicalExpr::BinaryExpr { left, right, .. } => {
            contains_operator_managed_node(left) || contains_operator_managed_node(right)
        }
        LogicalExpr::UnaryExpr { expr, .. }
        | LogicalExpr::Cast { expr, .. }
        | LogicalExpr::IsNull { expr, .. }
        | LogicalExpr::InSet { expr, .. } => contains_operator_managed_node(expr),
        LogicalExpr::Between { expr, low, high, .. } => {
            contains_operator_managed_node(expr)
                || contains_operator_managed_node(low)
                || contains_operator_managed_node(high)
        }
        LogicalExpr::InList { expr, list, .. } => {
            contains_operator_managed_node(expr) || list.iter().any(contains_operator_managed_node)
        }
        LogicalExpr::Case {
            expr,
            when_then,
            else_result,
        } => {
            expr.as_deref().is_some_and(contains_operator_managed_node)
                || when_then
                    .iter()
                    .any(|(when, then)| contains_operator_managed_node(when) || contains_operator_managed_node(then))
                || else_result.as_deref().is_some_and(contains_operator_managed_node)
        }
        LogicalExpr::ScalarFunction { args, .. } => args.iter().any(contains_operator_managed_node),
        LogicalExpr::Tuple { items } => items.iter().any(contains_operator_managed_node),
        LogicalExpr::ArraySubscript { array, index } => {
            contains_operator_managed_node(array) || contains_operator_managed_node(index)
        }
        _ => true,
    }
}

/// Resolve one column reference of a key operand on both inputs, with the
/// same two tiers as [`direct_join_key_side`]: a qualifier that names an
/// ALIAS on either side settles the question at that tier (the real-name
/// tier is consulted only when no column on either side carries the
/// alias); a duplicate match on a side is no match. Within a tier the
/// exact-case pass runs first, the case-folded one only when it matched
/// on neither side ([`DirectJoinNameCase`]). An unqualified name resolves
/// to the first column of that name on each side, as before.
fn resolve_join_key_ref(
    qualifier: Option<&str>,
    name: &str,
    left_schema: &Schema,
    right_schema: &Schema,
) -> (Option<usize>, Option<usize>) {
    let Some(qualifier) = qualifier else {
        return (left_schema.get_column_index(name), right_schema.get_column_index(name));
    };
    let unique = |found: DirectJoinMatch| match found {
        DirectJoinMatch::Unique(idx) => Some(idx),
        DirectJoinMatch::None | DirectJoinMatch::Ambiguous => None,
    };
    let (left_alias, right_alias) = resolve_join_column_both_sides(
        left_schema,
        right_schema,
        Some(qualifier),
        name,
        DirectJoinQualifierTier::Alias,
    );
    if !matches!(left_alias, DirectJoinMatch::None) || !matches!(right_alias, DirectJoinMatch::None) {
        return (unique(left_alias), unique(right_alias));
    }
    let (left_real, right_real) = resolve_join_column_both_sides(
        left_schema,
        right_schema,
        Some(qualifier),
        name,
        DirectJoinQualifierTier::RealName,
    );
    (unique(left_real), unique(right_real))
}

/// How one join's ON condition is executed, decided ONCE — before either
/// operator is built, because the hash join's constructor consumes its
/// build input and there is no falling back afterwards.
///
/// GH#29 (c6, m4): terms are bucketed by BINDABILITY, not by syntax.
/// `collect_and_terms` sent every `BinaryExpr{op: Eq}` to the key bucket,
/// so `sa.x = 20`, `5 = pa.pn` and `sa.x = (SELECT …)` — none of which any
/// key binder can bind — landed there, were declined, and then appeared in
/// NEITHER bucket: silently dropped, with `is_pure_equi_join` telling the
/// operator the keys expressed the whole condition. A term the binder
/// declines now goes to the residual, where it is evaluated.
struct JoinConditionPlan {
    /// The `=` terms whose operands the binder assigned to a side. `None`
    /// when not one term bound: nothing to hash on.
    equi: Option<crate::sql::LogicalExpr>,
    /// Everything else — a non-equality term, and every `=` term the binder
    /// declined. Never dropped.
    residual: Option<crate::sql::LogicalExpr>,
    /// The plain-column fast path over `equi`, when it applies.
    direct_key_indices: Option<DirectJoinKeyIndices>,
    /// The bound operand pairs of `equi`, when the direct path does
    /// not apply. Bound HERE and handed to the operator (GH#29 c6, m3):
    /// candidate 5 bound every pair twice (pre-check + constructor) and
    /// traced every declined term twice.
    bound_key_pairs: Vec<BoundJoinKeyPair>,
}

/// The key decision [`plan_join_condition`] made, handed to
/// `HashJoinOperator` so it does not repeat it (GH#29 c6, m3).
struct PreboundJoinKeys {
    direct_key_indices: Option<DirectJoinKeyIndices>,
    bound_key_pairs: Vec<BoundJoinKeyPair>,
    /// Do the keys express the WHOLE `on_condition` the operator is given?
    /// False when the operator is handed the residual too, to check on the
    /// candidate pair before it calls the pair matched (GH#29 c6, m5).
    keys_cover_condition: bool,
}

fn plan_join_condition(
    condition: &crate::sql::LogicalExpr,
    left_schema: &Schema,
    right_schema: &Schema,
) -> JoinConditionPlan {
    // Fast path, and the one every PK/FK join takes: the WHOLE condition is
    // plain `Column = Column` equalities with one operand on each side, which
    // `build_direct_join_key_indices` covers by slot index. No term can be
    // declined, so no term has to be bound to find that out — the per-term
    // binder below is skipped exactly as it was before the bucketing.
    if let Some(direct_key_indices) = build_direct_join_key_indices(condition, left_schema, right_schema) {
        return JoinConditionPlan {
            equi: Some(condition.clone()),
            residual: None,
            direct_key_indices: Some(direct_key_indices),
            bound_key_pairs: Vec::new(),
        };
    }

    let mut equi_parts = Vec::new();
    let mut residual_parts = Vec::new();
    let mut bound_key_pairs = Vec::new();
    collect_bindable_terms(
        condition,
        left_schema,
        right_schema,
        &mut equi_parts,
        &mut residual_parts,
        &mut bound_key_pairs,
    );
    let equi = combine_with_and(equi_parts);
    let residual = combine_with_and(residual_parts);
    // The plain-column index path is preferred whenever it covers the equi
    // part; the bound pairs are then unused, exactly as in the constructor.
    let direct_key_indices = equi
        .as_ref()
        .and_then(|equi| build_direct_join_key_indices(equi, left_schema, right_schema));
    if direct_key_indices.is_some() {
        bound_key_pairs.clear();
    }
    JoinConditionPlan {
        equi,
        residual,
        direct_key_indices,
        bound_key_pairs,
    }
}

/// Walk the AND chain, sending each term to the bucket that can actually
/// execute it (see [`JoinConditionPlan`]).
fn collect_bindable_terms(
    expr: &crate::sql::LogicalExpr,
    left_schema: &Schema,
    right_schema: &Schema,
    equi: &mut Vec<crate::sql::LogicalExpr>,
    residual: &mut Vec<crate::sql::LogicalExpr>,
    pairs: &mut Vec<BoundJoinKeyPair>,
) {
    use crate::sql::{BinaryOperator, LogicalExpr};
    match expr {
        LogicalExpr::BinaryExpr {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            collect_bindable_terms(left, left_schema, right_schema, equi, residual, pairs);
            collect_bindable_terms(right, left_schema, right_schema, equi, residual, pairs);
        }
        LogicalExpr::BinaryExpr {
            left,
            op: BinaryOperator::Eq,
            right,
        } => match bind_join_key_term(left, right, left_schema, right_schema) {
            Some(pair) => {
                pairs.push(pair);
                equi.push(expr.clone());
            }
            // The reason was traced by `bind_join_key_term`.
            None => residual.push(expr.clone()),
        },
        _ => residual.push(expr.clone()),
    }
}

/// Must this condition take `NestedLoopJoinOperator`?
///
/// * Nothing bound (`equi` is `None`): every build row would land in ONE
///   bucket and the probe would be O(n·m) anyway, with the hash join's
///   per-bucket bookkeeping on top.
/// * A residual under RIGHT / FULL: the hash join tracks unmatched build
///   rows per key BUCKET, so once one probe row passes the residual for a
///   bucket, the build rows of that bucket that FAILED it were dropped
///   instead of NULL-extended (`a RIGHT JOIN b ON a.id = b.id AND b.x =
///   b.y` lost b's `x <> y` row). The nested-loop join keeps a per-tuple
///   matched bitmap. INNER and LEFT keep the hash join: INNER filters the
///   residual after the join, LEFT checks it on the candidate pair.
fn join_condition_needs_nested_loop(plan: &JoinConditionPlan, join_type: &crate::sql::JoinType) -> bool {
    if plan.equi.is_none() {
        tracing::debug!(
            target: "helios::join",
            residual = ?plan.residual,
            "no hash-join key term could be bound; using a nested-loop join"
        );
        return true;
    }
    if plan.residual.is_some() && matches!(join_type, crate::sql::JoinType::Right | crate::sql::JoinType::Full) {
        tracing::debug!(
            target: "helios::join",
            residual = ?plan.residual,
            join_type = ?join_type,
            "a residual ON term under an outer join that preserves the build side; using a nested-loop join"
        );
        return true;
    }
    false
}

/// [`join_condition_needs_nested_loop`] over a raw condition — the shape the
/// unit tests pin, and the reason the two are separate functions is that the
/// executor plans the condition once and reuses the plan.
#[cfg(test)]
fn hash_join_keys_need_nested_loop(
    condition: &crate::sql::LogicalExpr,
    join_type: &crate::sql::JoinType,
    left_schema: &Schema,
    right_schema: &Schema,
) -> bool {
    join_condition_needs_nested_loop(&plan_join_condition(condition, left_schema, right_schema), join_type)
}

fn join_input_leaf_info(plan: &crate::sql::LogicalPlan) -> Option<(&str, Option<&String>, &Schema, bool)> {
    match plan {
        crate::sql::LogicalPlan::Scan {
            table_name,
            alias,
            schema,
            projection,
            ..
        }
        | crate::sql::LogicalPlan::FilteredScan {
            table_name,
            alias,
            schema,
            projection,
            ..
        } => Some((
            table_name.as_str(),
            alias.as_ref(),
            schema.as_ref(),
            projection.is_some(),
        )),
        crate::sql::LogicalPlan::Filter { input, .. } => join_input_leaf_info(input),
        _ => None,
    }
}

fn qualifier_matches_join_input(table_name: &str, alias: Option<&String>, qualifier: &str) -> bool {
    qualifier.eq_ignore_ascii_case(table_name) || alias.is_some_and(|alias| qualifier.eq_ignore_ascii_case(alias))
}

fn collect_join_input_expr_columns(
    input: &crate::sql::LogicalPlan,
    expr: &crate::sql::LogicalExpr,
    required: &mut std::collections::BTreeSet<usize>,
) -> Option<()> {
    let (table_name, alias, schema, _) = join_input_leaf_info(input)?;
    collect_join_input_expr_columns_inner(table_name, alias, schema, expr, required)
}

fn collect_join_input_expr_columns_inner(
    table_name: &str,
    alias: Option<&String>,
    schema: &Schema,
    expr: &crate::sql::LogicalExpr,
    required: &mut std::collections::BTreeSet<usize>,
) -> Option<()> {
    use crate::sql::LogicalExpr;
    match expr {
        LogicalExpr::Column {
            table: Some(table),
            name,
        } => {
            if qualifier_matches_join_input(table_name, alias, table) {
                let idx = schema
                    .columns
                    .iter()
                    .position(|column| column.name.eq_ignore_ascii_case(name))?;
                required.insert(idx);
            }
            Some(())
        }
        // Leave unqualified join/projection/filter expressions on the old path.
        // The compact path must know which side owns every referenced column.
        LogicalExpr::Column { table: None, .. } | LogicalExpr::Wildcard => None,
        LogicalExpr::BinaryExpr { left, right, .. } => {
            collect_join_input_expr_columns_inner(table_name, alias, schema, left, required)?;
            collect_join_input_expr_columns_inner(table_name, alias, schema, right, required)
        }
        LogicalExpr::UnaryExpr { expr, .. } | LogicalExpr::Cast { expr, .. } | LogicalExpr::IsNull { expr, .. } => {
            collect_join_input_expr_columns_inner(table_name, alias, schema, expr, required)
        }
        LogicalExpr::Between { expr, low, high, .. } => {
            collect_join_input_expr_columns_inner(table_name, alias, schema, expr, required)?;
            collect_join_input_expr_columns_inner(table_name, alias, schema, low, required)?;
            collect_join_input_expr_columns_inner(table_name, alias, schema, high, required)
        }
        LogicalExpr::InList { expr, list, .. } => {
            collect_join_input_expr_columns_inner(table_name, alias, schema, expr, required)?;
            for item in list {
                collect_join_input_expr_columns_inner(table_name, alias, schema, item, required)?;
            }
            Some(())
        }
        LogicalExpr::InSet { expr, .. } => {
            collect_join_input_expr_columns_inner(table_name, alias, schema, expr, required)
        }
        LogicalExpr::ArraySubscript { array, index } => {
            collect_join_input_expr_columns_inner(table_name, alias, schema, array, required)?;
            collect_join_input_expr_columns_inner(table_name, alias, schema, index, required)
        }
        LogicalExpr::Case {
            expr,
            when_then,
            else_result,
        } => {
            if let Some(expr) = expr {
                collect_join_input_expr_columns_inner(table_name, alias, schema, expr, required)?;
            }
            for (when, then) in when_then {
                collect_join_input_expr_columns_inner(table_name, alias, schema, when, required)?;
                collect_join_input_expr_columns_inner(table_name, alias, schema, then, required)?;
            }
            if let Some(else_result) = else_result {
                collect_join_input_expr_columns_inner(table_name, alias, schema, else_result, required)?;
            }
            Some(())
        }
        LogicalExpr::ScalarFunction { args, .. } | LogicalExpr::AggregateFunction { args, .. } => {
            for arg in args {
                collect_join_input_expr_columns_inner(table_name, alias, schema, arg, required)?;
            }
            Some(())
        }
        LogicalExpr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for arg in args {
                collect_join_input_expr_columns_inner(table_name, alias, schema, arg, required)?;
            }
            for expr in partition_by {
                collect_join_input_expr_columns_inner(table_name, alias, schema, expr, required)?;
            }
            for (expr, _) in order_by {
                collect_join_input_expr_columns_inner(table_name, alias, schema, expr, required)?;
            }
            Some(())
        }
        LogicalExpr::Tuple { items } => {
            for item in items {
                collect_join_input_expr_columns_inner(table_name, alias, schema, item, required)?;
            }
            Some(())
        }
        LogicalExpr::Literal(_) | LogicalExpr::Parameter { .. } | LogicalExpr::DefaultValue => Some(()),
        LogicalExpr::ScalarSubquery { .. }
        | LogicalExpr::InSubquery { .. }
        | LogicalExpr::Exists { .. }
        | LogicalExpr::NewRow { .. }
        | LogicalExpr::OldRow { .. } => None,
        // Physical-only node (R3.5): never present in plan expressions, which
        // is what this collector walks. Fall back to the old path if seen.
        LogicalExpr::BoundColumn { .. } => None,
    }
}

fn collect_join_input_local_filter_columns(
    input: &crate::sql::LogicalPlan,
    required: &mut std::collections::BTreeSet<usize>,
) -> Option<()> {
    match input {
        crate::sql::LogicalPlan::Filter { predicate, .. } => {
            collect_join_input_expr_columns(input, predicate, required)?;
            if let crate::sql::LogicalPlan::Filter { input, .. } = input {
                collect_join_input_local_filter_columns(input, required)?;
            }
            Some(())
        }
        // FilteredScan evaluates its local predicate before rows cross the scan
        // boundary. The scan decode hint still includes predicate columns for
        // storage/evaluator fallback, so predicate-only columns do not need to
        // be kept in the compact tuple handed to the join.
        crate::sql::LogicalPlan::FilteredScan { .. } => Some(()),
        crate::sql::LogicalPlan::Scan { .. } => Some(()),
        _ => None,
    }
}

fn apply_join_input_projection(
    input: &crate::sql::LogicalPlan,
    required: &std::collections::BTreeSet<usize>,
) -> Option<(crate::sql::LogicalPlan, bool)> {
    if required.is_empty() {
        return None;
    }
    let (_, _, schema, already_projected) = join_input_leaf_info(input)?;
    if already_projected {
        return None;
    }
    let indices: Vec<usize> = required.iter().copied().collect();
    if indices.iter().any(|&idx| idx >= schema.columns.len()) {
        return None;
    }
    if indices.len() >= schema.columns.len() {
        return Some((input.clone(), false));
    }

    match input {
        crate::sql::LogicalPlan::Scan {
            table_name,
            alias,
            schema,
            as_of,
            ..
        } => Some((
            crate::sql::LogicalPlan::Scan {
                table_name: table_name.clone(),
                alias: alias.clone(),
                schema: schema.clone(),
                projection: Some(indices),
                as_of: as_of.clone(),
            },
            true,
        )),
        crate::sql::LogicalPlan::FilteredScan {
            table_name,
            alias,
            schema,
            predicate,
            as_of,
            ..
        } => Some((
            crate::sql::LogicalPlan::FilteredScan {
                table_name: table_name.clone(),
                alias: alias.clone(),
                schema: schema.clone(),
                projection: Some(indices),
                predicate: predicate.clone(),
                as_of: as_of.clone(),
            },
            true,
        )),
        crate::sql::LogicalPlan::Filter { input, predicate } => {
            let (projected_input, changed) = apply_join_input_projection(input, required)?;
            Some((
                crate::sql::LogicalPlan::Filter {
                    input: Box::new(projected_input),
                    predicate: predicate.clone(),
                },
                changed,
            ))
        }
        _ => None,
    }
}

fn compact_projected_join_inputs(
    left: &crate::sql::LogicalPlan,
    right: &crate::sql::LogicalPlan,
    join_condition: &crate::sql::LogicalExpr,
    post_join_predicate: Option<&crate::sql::LogicalExpr>,
    project_exprs: &[crate::sql::LogicalExpr],
) -> Option<(crate::sql::LogicalPlan, crate::sql::LogicalPlan)> {
    let mut left_required = std::collections::BTreeSet::new();
    let mut right_required = std::collections::BTreeSet::new();

    collect_join_input_expr_columns(left, join_condition, &mut left_required)?;
    collect_join_input_expr_columns(right, join_condition, &mut right_required)?;
    if let Some(predicate) = post_join_predicate {
        collect_join_input_expr_columns(left, predicate, &mut left_required)?;
        collect_join_input_expr_columns(right, predicate, &mut right_required)?;
    }
    for expr in project_exprs {
        collect_join_input_expr_columns(left, expr, &mut left_required)?;
        collect_join_input_expr_columns(right, expr, &mut right_required)?;
    }
    collect_join_input_local_filter_columns(left, &mut left_required)?;
    collect_join_input_local_filter_columns(right, &mut right_required)?;

    let (projected_left, left_changed) = apply_join_input_projection(left, &left_required)?;
    let (projected_right, right_changed) = apply_join_input_projection(right, &right_required)?;
    if left_changed || right_changed {
        tracing::debug!(
            left_cols = ?left_required,
            right_cols = ?right_required,
            "projected inner join inputs to compact scan columns"
        );
        Some((projected_left, projected_right))
    } else {
        None
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

/// Fallback join materialization limit, in megabytes, used when neither the
/// configuration nor the environment sets one.
///
/// A hard 100 MB cap previously aborted large analytic joins outright with no
/// recourse (NANO-DEFICIENCIES A2 — e.g. a 116K ⋈ 614K join). The default is
/// 1 GB.
const DEFAULT_JOIN_MEMORY_LIMIT_MB: usize = 1024;

/// `[performance] join_memory_limit_mb` / `--join-memory-limit-mb`, applied by
/// `EmbeddedDatabase` at startup. `0` = not configured (use the default).
/// Process-global, last config wins — the same shape as the other runtime
/// toggles applied there (`lock_census`, `write_volume`, `copy_phase_stats`).
static JOIN_MEMORY_LIMIT_MB: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Apply the configured join materialization limit (MB). `0` clears it back
/// to the built-in default. See [`join_memory_limit`].
pub(crate) fn set_join_memory_limit_mb(mb: usize) {
    JOIN_MEMORY_LIMIT_MB.store(mb, std::sync::atomic::Ordering::Relaxed);
}

/// The cap, in BYTES, on how much a join may materialize before it is
/// refused: the hash join's build side and — since GH#29 (c7, M3) — the
/// nested loop's right input, which candidate 6 made reachable for every
/// RIGHT / FULL join carrying a residual ON term and which had no cap at all.
///
/// Resolution order: the `HELIOSDB_HASH_JOIN_MEM_MB` environment variable
/// (the documented runtime override, kept for compatibility), then
/// `[performance] join_memory_limit_mb` / `--join-memory-limit-mb`, then
/// [`DEFAULT_JOIN_MEMORY_LIMIT_MB`]. One knob, one error, both operators.
fn join_memory_limit() -> usize {
    resolve_join_memory_limit(
        std::env::var("HELIOSDB_HASH_JOIN_MEM_MB").ok().as_deref(),
        JOIN_MEMORY_LIMIT_MB.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// [`join_memory_limit`] without the two global reads, so the precedence is
/// pinned without mutating process state a concurrently running test could
/// observe.
fn resolve_join_memory_limit(env_mb: Option<&str>, configured_mb: usize) -> usize {
    env_mb
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|mb| *mb > 0)
        .or(if configured_mb > 0 { Some(configured_mb) } else { None })
        .unwrap_or(DEFAULT_JOIN_MEMORY_LIMIT_MB)
        .saturating_mul(1024 * 1024)
}

/// The one refusal both join materializations raise when they hit
/// [`join_memory_limit`] — same text, same remedies, whichever operator the
/// planner picked (GH#29 c7, M3).
fn join_memory_limit_exceeded(memory_limit: usize) -> Error {
    Error::query_execution(format!(
        "Join exceeds memory limit ({} MB). Raise it with the [performance] join_memory_limit_mb \
         configuration key, the --join-memory-limit-mb flag or the HELIOSDB_HASH_JOIN_MEM_MB \
         environment variable, or rewrite the query (e.g. add a more selective filter or join key).",
        memory_limit / (1024 * 1024)
    ))
}

impl HashJoinOperator {
    /// Resolve the default materialization limit (bytes); see
    /// [`join_memory_limit`].
    fn default_memory_limit() -> usize {
        join_memory_limit()
    }

    /// Create a new hash join operator with default memory limit.
    ///
    /// `parameters` are the statement's bind values: the combined evaluator
    /// needs them for any part of the ON condition it re-evaluates per
    /// candidate pair (GH#29 c7, M1).
    pub fn new(
        left: Box<dyn PhysicalOperator>,
        right: Box<dyn PhysicalOperator>,
        join_type: crate::sql::JoinType,
        on_condition: Option<crate::sql::LogicalExpr>,
        parameters: Vec<crate::Value>,
        timeout_ctx: Option<TimeoutContext>,
    ) -> Result<Self> {
        Self::with_memory_limit(
            left,
            right,
            join_type,
            on_condition,
            parameters,
            HashJoinBuildSide::Right,
            Self::default_memory_limit(),
            timeout_ctx,
        )
    }

    /// Create a hash join that builds the left input and probes the right input.
    /// Restricted to INNER joins by the caller so output column order can be
    /// preserved without changing outer-join NULL-extension semantics.
    pub fn new_build_left(
        left: Box<dyn PhysicalOperator>,
        right: Box<dyn PhysicalOperator>,
        join_type: crate::sql::JoinType,
        on_condition: Option<crate::sql::LogicalExpr>,
        parameters: Vec<crate::Value>,
        timeout_ctx: Option<TimeoutContext>,
    ) -> Result<Self> {
        Self::with_memory_limit(
            left,
            right,
            join_type,
            on_condition,
            parameters,
            HashJoinBuildSide::Left,
            Self::default_memory_limit(),
            timeout_ctx,
        )
    }

    /// Create a new hash join operator with custom memory limit
    fn with_memory_limit(
        left: Box<dyn PhysicalOperator>,
        right: Box<dyn PhysicalOperator>,
        join_type: crate::sql::JoinType,
        on_condition: Option<crate::sql::LogicalExpr>,
        parameters: Vec<crate::Value>,
        build_side: HashJoinBuildSide,
        memory_limit: usize,
        timeout_ctx: Option<TimeoutContext>,
    ) -> Result<Self> {
        Self::with_memory_limit_projected(
            left,
            right,
            join_type,
            on_condition,
            parameters,
            build_side,
            memory_limit,
            timeout_ctx,
            None,
            None,
            None,
            None,
        )
    }

    /// Create a hash join whose keys were already decided by
    /// [`plan_join_condition`] (GH#29 c6, m3 + m5).
    ///
    /// `on_condition` is the KEYED part. `pair_residual`, when present, is the
    /// rest of the ON condition, checked on every candidate pair BEFORE the
    /// pair counts as a match — which is how a residual ON term under a LEFT
    /// join is honoured without dropping the NULL-extended rows a post-join
    /// filter would have eaten. Candidate 6 handed the operator the WHOLE
    /// condition instead, so every candidate pair re-evaluated the equality
    /// terms the hash lookup had already proved (GH#29 c7, n5).
    fn new_with_keys(
        left: Box<dyn PhysicalOperator>,
        right: Box<dyn PhysicalOperator>,
        join_type: crate::sql::JoinType,
        on_condition: Option<crate::sql::LogicalExpr>,
        pair_residual: Option<crate::sql::LogicalExpr>,
        parameters: Vec<crate::Value>,
        keys: PreboundJoinKeys,
        build_side: HashJoinBuildSide,
        timeout_ctx: Option<TimeoutContext>,
    ) -> Result<Self> {
        Self::with_memory_limit_projected(
            left,
            right,
            join_type,
            on_condition,
            parameters,
            build_side,
            Self::default_memory_limit(),
            timeout_ctx,
            None,
            None,
            Some(keys),
            pair_residual,
        )
    }

    /// [`Self::new_with_keys`] for the projected inner join built by
    /// `handle_projected_join`. The projection indexes refer to the normal
    /// combined left+right join schema, but emitted tuples contain only those
    /// projected values — which avoids building a full combined tuple only
    /// for a parent `ProjectOperator` to clone a small subset of columns, and
    /// is why this shape can serve neither a residual nor a post-join
    /// predicate.
    fn new_projected_inner_with_keys(
        left: Box<dyn PhysicalOperator>,
        right: Box<dyn PhysicalOperator>,
        on_condition: Option<crate::sql::LogicalExpr>,
        parameters: Vec<crate::Value>,
        keys: PreboundJoinKeys,
        projection: Vec<usize>,
        output_schema: Arc<Schema>,
        build_side: HashJoinBuildSide,
        timeout_ctx: Option<TimeoutContext>,
    ) -> Result<Self> {
        Self::with_memory_limit_projected(
            left,
            right,
            crate::sql::JoinType::Inner,
            on_condition,
            parameters,
            build_side,
            Self::default_memory_limit(),
            timeout_ctx,
            Some(projection),
            Some(output_schema),
            Some(keys),
            None,
        )
    }

    fn with_memory_limit_projected(
        left: Box<dyn PhysicalOperator>,
        right: Box<dyn PhysicalOperator>,
        join_type: crate::sql::JoinType,
        on_condition: Option<crate::sql::LogicalExpr>,
        parameters: Vec<crate::Value>,
        build_side: HashJoinBuildSide,
        memory_limit: usize,
        timeout_ctx: Option<TimeoutContext>,
        output_projection: Option<Vec<usize>>,
        projected_output_schema: Option<Arc<Schema>>,
        prebound_keys: Option<PreboundJoinKeys>,
        pair_residual: Option<crate::sql::LogicalExpr>,
    ) -> Result<Self> {
        // Build output schema by combining left and right schemas
        let left_schema = left.schema();
        let right_schema = right.schema();

        let left_column_count = left_schema.columns.len();
        let right_column_count = right_schema.columns.len();

        let mut columns = left_schema.columns.clone();
        columns.extend(right_schema.columns.clone());
        let combined_schema = Arc::new(Schema { columns });
        let output_schema = projected_output_schema.unwrap_or_else(|| Arc::clone(&combined_schema));

        // GH#29 (c4, F1): operand sides are decided here, never per tuple.
        // A term that cannot be assigned unambiguously leaves the key, and
        // the operator then re-evaluates the WHOLE condition on the combined
        // tuple (`pure_equi_join == false`) — the alias-first evaluator path
        // — instead of guessing. GH#29 (c6, m3): when the caller already
        // planned the condition, the pairs come in with it and are not bound
        // (nor traced) a second time.
        let (direct_key_indices, bound_key_pairs, keys_cover_condition) = match prebound_keys {
            Some(keys) => (keys.direct_key_indices, keys.bound_key_pairs, keys.keys_cover_condition),
            None => {
                let direct_key_indices = on_condition
                    .as_ref()
                    .and_then(|condition| build_direct_join_key_indices(condition, &left_schema, &right_schema));
                let (pairs, covered) = match (&direct_key_indices, &on_condition) {
                    (None, Some(condition)) => bind_hash_join_key_pairs(condition, &left_schema, &right_schema),
                    _ => (Vec::new(), true),
                };
                (direct_key_indices, pairs, covered)
            }
        };
        // GH#29 (c7, n5): a pair residual is, by construction, a part of the
        // condition the keys do NOT cover.
        let pure_equi_join = pair_residual.is_none() && keys_cover_condition && is_pure_equi_join(&on_condition);

        // Create evaluator with output schema for evaluating join conditions
        // on combined tuples. GH#29 (c7, M1): WITH the statement's bind
        // values — `Evaluator::new` gave it an empty parameter vector, so
        // `ON a.id = b.id AND b.k = $1` (whose `$1` term the key binder
        // declines, exactly as intended) failed with `Parameter $1 not
        // provided` instead of evaluating.
        let evaluator = crate::sql::Evaluator::with_parameters(combined_schema, parameters.clone());

        // Create separate evaluators for key extraction (left and right
        // schemas). These carry the bind values too: a key OPERAND may
        // legally contain one — `ON a.id + $1 = b.id` binds (it references a
        // column, and a parameter is not a node the binder refuses), and
        // `extract_join_columns` evaluates that operand per tuple. Two clones
        // of the bind vector per join CONSTRUCTION, never per tuple.
        let left_evaluator = crate::sql::Evaluator::with_parameters(left_schema, parameters.clone());
        let right_evaluator = crate::sql::Evaluator::with_parameters(right_schema, parameters);
        // GH#29 (c8, m7): bind the pair residual against the combined schema
        // once, then decide once whether it can be evaluated on a BORROWED
        // pair — the same seam `NestedLoopJoinOperator` has used since R3.5.
        // A residual that does not bind (an ambiguous bare name) stays a
        // `Column` node, is not pair-evaluable, and keeps the previous
        // materialize-then-evaluate path byte for byte.
        let pair_residual = pair_residual.map(|residual| evaluator.bind(residual));
        let pair_residual_on_pair = pair_residual.as_ref().map(pair_evaluable).unwrap_or(false);
        let (probe_input, mut build_input) = match build_side {
            HashJoinBuildSide::Right => (left, right),
            HashJoinBuildSide::Left => (right, left),
        };

        // Create the operator instance
        let mut operator = Self {
            left: probe_input,
            join_type,
            on_condition,
            pair_residual,
            pair_residual_on_pair,
            hash_table: std::collections::HashMap::new(),
            output_schema,
            evaluator,
            left_evaluator,
            right_evaluator,
            direct_key_indices,
            bound_key_pairs,
            build_side,
            state: JoinState::Initial,
            current_left_tuple: None,
            current_match_key: None,
            match_index: 0,
            current_match_found: false,
            pure_equi_join,
            output_projection,
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
        operator.build_phase(&mut build_input)?;

        Ok(operator)
    }

    /// Set timeout context (no-op since timeout is set during construction)
    pub fn with_timeout(self, _timeout_ctx: Option<TimeoutContext>) -> Self {
        // Timeout already set during construction, ignore this call
        self
    }

    /// Execute build phase: construct hash table from the selected build side.
    ///
    /// This phase materializes ALL tuples from the build input
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
            let key_opt = self.extract_join_key(&tuple, self.build_side == HashJoinBuildSide::Right)?;
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
                return Err(join_memory_limit_exceeded(self.memory_limit));
            }

            // Insert into hash table (with overflow chaining). Keep the common
            // unique-key bucket as a single tuple to avoid one Vec allocation
            // per build row.
            match self.hash_table.entry(key) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    entry.get_mut().push(tuple);
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(JoinBucket::One(tuple));
                }
            }

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
        if let Some(indices) = &self.direct_key_indices {
            let key_indices = if is_right_side { &indices.right } else { &indices.left };
            if key_indices.len() == 1 {
                let value = tuple
                    .values
                    .get(key_indices[0])
                    .ok_or_else(|| Error::query_execution("Join key index out of bounds"))?;
                if matches!(value, crate::Value::Null) {
                    return Ok(None);
                }
                return Ok(Some(JoinKey::from_single_value_ref(value)));
            }

            let mut key_values = Vec::with_capacity(key_indices.len());
            for &idx in key_indices {
                let value = tuple
                    .values
                    .get(idx)
                    .ok_or_else(|| Error::query_execution("Join key index out of bounds"))?
                    .clone();
                if matches!(value, crate::Value::Null) {
                    return Ok(None);
                }
                key_values.push(value);
            }
            return Ok(Some(JoinKey::from_values(key_values)));
        }

        if self.on_condition.is_some() {
            let key_values = self.extract_join_columns(tuple, is_right_side)?;

            // Check if any key value is NULL - if so, this tuple will never match
            if key_values.iter().any(|v| matches!(v, crate::Value::Null)) {
                return Ok(None);
            }

            Ok(Some(JoinKey::from_values(key_values)))
        } else {
            // Cross join - use empty key (all tuples match)
            Ok(Some(JoinKey::Composite(Vec::new())))
        }
    }

    /// Extract the join key values of `tuple` for its side: one value per
    /// bound key pair (see [`BoundJoinKeyPair`]), evaluated with the side's
    /// own evaluator against the expression bound for that side. Through
    /// candidate 3 this evaluated the "natural" operand with the side's
    /// evaluator and fell back to the other operand on `Err`, per tuple —
    /// which is exactly how `u.id + 10` evaluated on `u AS a` through the
    /// real-name fallback instead of erroring (GH#29 c4, F1).
    fn extract_join_columns(&self, tuple: &Tuple, is_right_side: bool) -> Result<Vec<crate::Value>> {
        let evaluator = if is_right_side {
            &self.right_evaluator
        } else {
            &self.left_evaluator
        };
        let mut values = Vec::with_capacity(self.bound_key_pairs.len());
        for pair in &self.bound_key_pairs {
            let expr = if is_right_side { &pair.right } else { &pair.left };
            values.push(evaluator.evaluate(expr, tuple)?);
        }
        Ok(values)
    }

    /// Probe phase: stream left side, lookup matches in hash table
    fn probe_phase(&mut self) -> Result<Option<Tuple>> {
        loop {
            // Check timeout during probe loop
            if let Some(ref ctx) = self.timeout_ctx {
                ctx.check_timeout()?;
            }

            // Stream the current bucket in place — no bucket is ever cloned,
            // for a pure equi-join or for one carrying a pair residual
            // (GH#29 c7, n5). A residual is checked HERE, on the candidate
            // pair, which is what keeps a LEFT join's NULL-extended rows: the
            // probe row is NULL-extended only once the whole bucket has been
            // walked without a pair passing.
            if let Some(key) = self.current_match_key.as_ref() {
                let mut emitted: Option<Tuple> = None;
                if let Some(matches) = self.hash_table.get(key) {
                    while self.match_index < matches.len() {
                        let right_tuple = matches
                            .get(self.match_index)
                            .ok_or_else(|| Error::query_execution("Match index out of bounds"))?;
                        let left_tuple = self
                            .current_left_tuple
                            .as_ref()
                            .ok_or_else(|| Error::query_execution("Missing left tuple"))?;
                        self.match_index += 1;
                        // GH#29 (c5, m2): an evaluation error is the
                        // statement's error, never "no match" — through
                        // candidate 4 `unwrap_or(false)` turned a declined
                        // term the combined evaluator could not evaluate into
                        // silently missing rows.
                        if !self.pure_equi_join && !self.evaluate_probe_build_condition(left_tuple, right_tuple)? {
                            continue;
                        }
                        emitted = Some(self.join_probe_with_build(left_tuple, right_tuple));
                        break;
                    }
                }

                if let Some(tuple) = emitted {
                    if !self.current_match_found {
                        self.current_match_found = true;
                        // Mark key as matched (for RIGHT/FULL joins) — once
                        // per probe row, and only once a pair really passed.
                        if matches!(self.join_type, crate::sql::JoinType::Right | crate::sql::JoinType::Full) {
                            let key = key.clone();
                            self.matched_right_keys.insert(key);
                        }
                    }
                    return Ok(Some(tuple));
                }

                // Bucket exhausted.
                let matched = self.current_match_found;
                let left_tuple = self.current_left_tuple.take();
                self.current_match_key = None;
                self.match_index = 0;
                self.current_match_found = false;
                if !matched {
                    if let Some(left_tuple) = left_tuple {
                        // Every candidate pair failed the residual: for
                        // LEFT/FULL this probe row is NULL-extended, exactly
                        // as if the bucket had been empty.
                        if matches!(self.join_type, crate::sql::JoinType::Left | crate::sql::JoinType::Full) {
                            return Ok(Some(self.join_with_nulls_right(&left_tuple)));
                        }
                    }
                }
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
                    let key_opt = self.extract_join_key(&left_tuple, self.build_side == HashJoinBuildSide::Left)?;
                    tracing::debug!(
                        "HashJoin probe: left_tuple = {:?}, extracted key = {:?}",
                        left_tuple.values,
                        key_opt
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
                        let mut found = false;
                        if self.pure_equi_join {
                            // Every tuple of a non-empty bucket matches, so
                            // the probe row is matched already: mark the key
                            // (for RIGHT/FULL joins) once, here.
                            if matches!(self.join_type, crate::sql::JoinType::Right | crate::sql::JoinType::Full) {
                                self.matched_right_keys.insert(key.clone());
                            }

                            if matches.len() == 1 {
                                let right_tuple = matches
                                    .get(0)
                                    .ok_or_else(|| Error::query_execution("Match index out of bounds"))?;
                                return Ok(Some(self.join_probe_with_build(&left_tuple, right_tuple)));
                            }
                            found = true;
                        }

                        // Hand the bucket to the streaming block above, which
                        // emits it in place — checking the pair residual per
                        // candidate pair when there is one, and NULL-extending
                        // the probe row if none passes.
                        self.current_left_tuple = Some(left_tuple);
                        self.current_match_key = Some(key);
                        self.match_index = 0;
                        self.current_match_found = found;
                        continue;
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
        // GH#29 (c7, n5): when the caller split the ON condition, only the
        // RESIDUAL is evaluated here — the equality terms were proved by the
        // hash lookup, and re-proving them per candidate pair was pure waste
        // (and, for the `pure_equi_join` shape above, has never been done).
        if let Some(residual) = &self.pair_residual {
            // GH#29 (c8, m7): read the pair through a borrowed view whenever
            // the residual allows it. `join_tuples` deep-clones every value
            // of both tuples, and this runs once per CANDIDATE PAIR — a
            // 1000-row bucket did 1000 full combined-tuple allocations per
            // probe row. The nested loop has avoided exactly that since R3.5.
            let value = if self.pair_residual_on_pair {
                eval_condition_on_pair(&self.evaluator, residual, &PairView { left, right })?
            } else {
                let combined = Self::join_tuples(left, right);
                self.evaluator.evaluate(residual, &combined)?
            };
            return match value {
                crate::Value::Boolean(b) => Ok(b),
                crate::Value::Null => Ok(false), // NULL is treated as false in join conditions
                _ => Ok(false),
            };
        }
        if let Some(condition) = &self.on_condition {
            // The hash join now only receives equi-join conditions (equality predicates).
            // The hash lookup already confirmed the keys match, so skip re-evaluation
            // which can fail due to duplicate column names in the combined schema.
            // GH#29 (c4, F1): the FIELD, not the shape — a pure equi-join whose
            // key pairs could not all be assigned a side is re-evaluated here.
            if self.pure_equi_join {
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

    fn evaluate_probe_build_condition(&self, probe: &Tuple, build: &Tuple) -> Result<bool> {
        match self.build_side {
            HashJoinBuildSide::Right => self.evaluate_join_condition(probe, build),
            HashJoinBuildSide::Left => self.evaluate_join_condition(build, probe),
        }
    }

    /// Emit unmatched tuples from right side (for RIGHT/FULL joins)
    fn emit_unmatched(&mut self) -> Result<Option<Tuple>> {
        // Initialize iterator if not already done
        if self.unmatched_right_iter.is_none() {
            // Collect unmatched right tuples
            let unmatched: Vec<_> = self
                .hash_table
                .iter()
                .filter(|(key, _)| !self.matched_right_keys.contains(key))
                .map(|(key, tuples)| (key.clone(), tuples.clone_tuples()))
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

    fn join_tuples_projected(&self, left: &Tuple, right: &Tuple) -> Tuple {
        let Some(indices) = &self.output_projection else {
            return Self::join_tuples(left, right);
        };

        let mut values = Vec::with_capacity(indices.len());
        for &idx in indices {
            let value = if idx < self.left_column_count {
                left.values.get(idx)
            } else {
                right.values.get(idx - self.left_column_count)
            }
            .cloned()
            .unwrap_or(crate::Value::Null);
            values.push(value);
        }
        Tuple::new(values)
    }

    fn join_probe_with_build(&self, probe: &Tuple, build: &Tuple) -> Tuple {
        match self.build_side {
            HashJoinBuildSide::Right => self.join_tuples_projected(probe, build),
            HashJoinBuildSide::Left => self.join_tuples_projected(build, probe),
        }
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
        let values_size: usize = tuple.values.iter().map(|v| Self::estimate_value_size(v)).sum();
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
        match key {
            JoinKey::Int(_) => std::mem::size_of::<i64>(),
            JoinKey::Single(value) => Self::estimate_value_size(value),
            JoinKey::Composite(values) => 24 + values.iter().map(Self::estimate_value_size).sum::<usize>(),
        }
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
        let parameters = executor.parameters().to_vec();
        return Ok(Box::new(NestedLoopJoinOperator::new(
            left_op,
            right_op,
            join_type.clone(),
            on.clone(),
            parameters,
            timeout_ctx,
        )?));
    }

    let left_rows = estimate_hash_join_rows(executor, left);
    let right_rows = estimate_hash_join_rows(executor, right);
    // Diagnostic kill switch for A/B perf runs. Build left only when it is
    // much smaller; otherwise keep the left side as the probe stream, which is
    // faster for common filtered one-to-many joins.
    let build_left_for_inner = matches!(join_type, crate::sql::JoinType::Inner)
        && std::env::var("HELIOS_HASHJOIN_BUILD_RIGHT").is_err()
        && should_build_left_for_inner(left_rows, right_rows);

    if let Some(condition) = on {
        let inlj_left_rows = if matches!(join_type, crate::sql::JoinType::Inner) {
            estimate_index_nested_loop_probe_rows(executor, left).or(left_rows)
        } else {
            left_rows
        };
        // GH#29 (c7, n3): gate on what the index nested loop can actually
        // EXECUTE, not on the syntax split. It probes ONE indexed column
        // equality and has nowhere to apply a second term, so the whole ON
        // condition must be that one plain `Column = Column` equality —
        // `extract_equi_columns` is precisely that test, and it is the test
        // `try_index_nested_loop_join` applies internally. Candidate 6 gated
        // on `residual_part.is_none()`, i.e. "every term is an `=`", and so
        // dispatched every compound all-equality ON into a call that could
        // only decline it.
        if extract_equi_columns(condition).is_some()
            && matches!(join_type, crate::sql::JoinType::Inner | crate::sql::JoinType::Left)
            && should_try_index_nested_loop_join(inlj_left_rows, right_rows)
            && is_plain_scan_like(right)
        {
            if let Some(join_op) = try_index_nested_loop_join(executor, left, right, join_type, condition)? {
                return Ok(join_op);
            }
        }
    }

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
                executor.parameters().to_vec(),
                timeout_ctx,
            )?))
        }
        Some(condition) => {
            // GH#29 (c5, m2): uncorrelated subqueries in the ON condition
            // are materialized before either operator is built — exactly as
            // the post-join predicate path does — so a declined `=` term
            // such as `a.x = (SELECT max(x) FROM c)` is evaluable by the
            // hash join's combined evaluator (which has no storage) instead
            // of erroring per candidate pair. GH#29 (c6, m1): a CORRELATED
            // one is refused here, never silently read as NULL.
            let condition = executor.materialize_join_subqueries(condition)?;
            let left_schema = left_op.schema();
            let right_schema = right_op.schema();
            let condition_plan = plan_join_condition(&condition, &left_schema, &right_schema);

            if join_condition_needs_nested_loop(&condition_plan, join_type) {
                // Nothing bound, or a residual under RIGHT / FULL — the whole
                // (materialized) condition on a nested-loop join, which keeps
                // a per-TUPLE matched bitmap.
                return Ok(Box::new(NestedLoopJoinOperator::new(
                    left_op,
                    right_op,
                    join_type.clone(),
                    Some(condition),
                    executor.parameters().to_vec(),
                    timeout_ctx,
                )?));
            }

            let JoinConditionPlan {
                equi,
                residual,
                direct_key_indices,
                bound_key_pairs,
            } = condition_plan;
            let equi = equi.ok_or_else(|| Error::query_execution("hash join planned with no key"))?;

            // GH#29 (c6, m5): under LEFT the residual must be checked INSIDE
            // the join — a `FilterOperator` on top of the join sees the
            // NULL-extended rows and drops them, which is how
            // `na LEFT JOIN nb ON na.id = nb.id AND nb.b > 1000` returned
            // ZERO rows instead of every `na` row NULL-extended. Under INNER
            // (and CROSS) a post-join filter is still exactly equivalent, and
            // is kept: it filters the residual alone instead of re-evaluating
            // the equality terms the hash lookup already proved.
            //
            // GH#29 (c7, n5): the operator is handed the KEYED part plus the
            // residual SEPARATELY. Candidate 6 handed it `condition` whole,
            // so every candidate pair re-evaluated the equality terms the
            // hash lookup had already proved.
            let residual_inside_join = residual.is_some() && matches!(join_type, crate::sql::JoinType::Left);
            let (pair_residual, post_join_residual) = if residual_inside_join {
                (residual, None)
            } else {
                (None, residual)
            };
            let keys = PreboundJoinKeys {
                direct_key_indices,
                bound_key_pairs,
                keys_cover_condition: true,
            };

            let mut join_op: Box<dyn PhysicalOperator> = Box::new(HashJoinOperator::new_with_keys(
                left_op,
                right_op,
                join_type.clone(),
                Some(equi),
                pair_residual,
                executor.parameters().to_vec(),
                keys,
                if build_left_for_inner {
                    HashJoinBuildSide::Left
                } else {
                    HashJoinBuildSide::Right
                },
                timeout_ctx,
            )?);

            // Apply residual filter on top if present (INNER / CROSS only).
            if let Some(residual) = post_join_residual {
                join_op = Box::new(super::filter::FilterOperator::new(
                    join_op,
                    residual,
                    executor.parameters().to_vec(),
                ));
            }

            Ok(join_op)
        }
    }
}

pub(super) fn handle_projected_join(
    executor: &mut Executor,
    input: &crate::sql::LogicalPlan,
    exprs: &[crate::sql::LogicalExpr],
    aliases: &[String],
) -> Result<Option<Box<dyn PhysicalOperator>>> {
    let (join_input, post_join_predicate) = match input {
        crate::sql::LogicalPlan::Join { .. } => (input, None),
        crate::sql::LogicalPlan::Filter { input, predicate }
            if matches!(input.as_ref(), crate::sql::LogicalPlan::Join { .. }) =>
        {
            (input.as_ref(), Some(predicate))
        }
        _ => return Ok(None),
    };

    let crate::sql::LogicalPlan::Join {
        left,
        right,
        join_type,
        on,
        lateral,
    } = join_input
    else {
        return Ok(None);
    };
    if *lateral || !matches!(join_type, crate::sql::JoinType::Inner) {
        return Ok(None);
    }

    let Some(condition) = on else {
        return Ok(None);
    };
    let (equi_part, residual_part) = split_join_condition(condition);
    if equi_part.is_none() || residual_part.is_some() {
        return Ok(None);
    }
    // GH#29 (c7, n3): what the index nested loop below can EXECUTE — one
    // plain `Column = Column` equality, the whole condition — decided here
    // rather than by the syntax split, which let every compound all-equality
    // ON into a call that could only decline it.
    let inlj_condition = extract_equi_columns(condition).is_some();

    for expr in exprs {
        if !matches!(expr, crate::sql::LogicalExpr::Column { .. }) {
            return Ok(None);
        }
    }

    let left_rows = estimate_hash_join_rows(executor, left);
    let right_rows = estimate_hash_join_rows(executor, right);
    let build_left_for_inner =
        std::env::var("HELIOS_HASHJOIN_BUILD_RIGHT").is_err() && should_build_left_for_inner(left_rows, right_rows);

    let inlj_left_rows = estimate_index_nested_loop_probe_rows(executor, left).or(left_rows);
    if inlj_condition
        && post_join_predicate.is_none()
        && should_try_index_nested_loop_join(inlj_left_rows, right_rows)
        && is_plain_scan_like(right)
    {
        if let Some(join_op) = try_index_nested_loop_join(executor, left, right, join_type, condition)? {
            let timeout_ctx = executor.timeout_ctx();
            let project_op = super::project::ProjectOperator::new(
                join_op,
                exprs.to_vec(),
                aliases.to_vec(),
                false,
                executor.parameters().to_vec(),
            )
            .with_timeout(timeout_ctx);
            return Ok(Some(Box::new(project_op)));
        }
    }

    let compact_plans = equi_part
        .as_ref()
        .and_then(|condition| compact_projected_join_inputs(left, right, condition, post_join_predicate, exprs));
    if post_join_predicate.is_some() && compact_plans.is_none() {
        return Ok(None);
    }
    let (left_plan, right_plan) =
        compact_plans.unwrap_or_else(|| ((*left).as_ref().clone(), (*right).as_ref().clone()));

    let left_op = executor.plan_to_operator(&left_plan)?;
    let right_op = executor.plan_to_operator(&right_plan)?;
    let left_schema = left_op.schema();
    let right_schema = right_op.schema();
    let mut combined_columns = left_schema.columns.clone();
    combined_columns.extend(right_schema.columns.clone());
    let combined_schema = Arc::new(Schema {
        columns: combined_columns,
    });

    // GH#29 (c5, m2 + m4 / c6, m1 + m3 + m4): same as `handle_join` — the ON
    // condition is materialized before either operator is built (a
    // CORRELATED subquery is refused, not read as NULL), and its terms are
    // bucketed by BINDABILITY, so an `=` term no key binder can bind lands
    // in the residual instead of being silently dropped.
    //
    // This branch is INNER-only, so a residual is a post-join filter, which
    // is exactly equivalent for INNER. The PROJECTED hash join emits only
    // the projected columns, so it can serve neither a residual nor a
    // post-join predicate; those take the generic shape below, projected the
    // same way the index-nested-loop branch above is.
    let timeout_ctx = executor.timeout_ctx();
    let materialized = executor.materialize_join_subqueries(condition)?;
    let JoinConditionPlan {
        equi,
        residual,
        direct_key_indices,
        bound_key_pairs,
    } = plan_join_condition(&materialized, &left_schema, &right_schema);
    let build_side = if build_left_for_inner {
        HashJoinBuildSide::Left
    } else {
        HashJoinBuildSide::Right
    };

    if equi.is_none() || residual.is_some() || post_join_predicate.is_some() {
        // GH#29 (c7, M4): when nothing bound, the nested loop is handed the
        // WHOLE condition, so nothing may be stacked on top of it — see
        // [`post_join_residual_filter`].
        let residual_to_filter = post_join_residual_filter(equi.is_none(), residual);
        let mut join_op: Box<dyn PhysicalOperator> = match equi {
            None => Box::new(NestedLoopJoinOperator::new(
                left_op,
                right_op,
                crate::sql::JoinType::Inner,
                Some(materialized),
                executor.parameters().to_vec(),
                timeout_ctx.clone(),
            )?),
            Some(equi) => Box::new(HashJoinOperator::new_with_keys(
                left_op,
                right_op,
                crate::sql::JoinType::Inner,
                Some(equi),
                None,
                executor.parameters().to_vec(),
                PreboundJoinKeys {
                    direct_key_indices,
                    bound_key_pairs,
                    keys_cover_condition: true,
                },
                build_side,
                timeout_ctx.clone(),
            )?),
        };
        if let Some(residual) = residual_to_filter {
            join_op = Box::new(
                super::filter::FilterOperator::new(join_op, residual, executor.parameters().to_vec())
                    .with_timeout(timeout_ctx.clone()),
            );
        }
        if let Some(predicate) = post_join_predicate {
            let materialized_predicate = executor.materialize_subqueries(predicate)?;
            join_op = Box::new(
                super::filter::FilterOperator::new(join_op, materialized_predicate, executor.parameters().to_vec())
                    .with_timeout(timeout_ctx.clone()),
            );
        }
        let project_op = super::project::ProjectOperator::new(
            join_op,
            exprs.to_vec(),
            aliases.to_vec(),
            false,
            executor.parameters().to_vec(),
        )
        .with_timeout(timeout_ctx);
        return Ok(Some(Box::new(project_op)));
    }

    let equi = equi.ok_or_else(|| Error::query_execution("projected hash join planned with no key"))?;

    let mut projection = Vec::with_capacity(exprs.len());
    for expr in exprs {
        let crate::sql::LogicalExpr::Column { table, name } = expr else {
            return Ok(None);
        };
        let Some(idx) = combined_schema.get_qualified_column_index(table.as_deref(), name) else {
            return Ok(None);
        };
        projection.push(idx);
    }

    use crate::sql::TypeInference;
    let output_schema = Arc::new(Schema {
        columns: aliases
            .iter()
            .zip(exprs.iter())
            .map(|(alias, expr)| expr.to_column(alias.clone(), &combined_schema))
            .collect(),
    });

    let op = HashJoinOperator::new_projected_inner_with_keys(
        left_op,
        right_op,
        Some(equi),
        executor.parameters().to_vec(),
        PreboundJoinKeys {
            direct_key_indices,
            bound_key_pairs,
            keys_cover_condition: true,
        },
        projection,
        output_schema,
        build_side,
        timeout_ctx,
    )?;
    Ok(Some(Box::new(op)))
}

fn estimate_hash_join_rows(executor: &Executor<'_>, plan: &crate::sql::LogicalPlan) -> Option<usize> {
    match plan {
        crate::sql::LogicalPlan::Scan { table_name, .. } => executor.storage()?.count_table_rows(table_name).ok(),
        crate::sql::LogicalPlan::FilteredScan {
            table_name, predicate, ..
        } => {
            let rows = executor.storage()?.count_table_rows(table_name).ok()?;
            if predicate.is_some() {
                Some(estimate_filtered_rows(rows))
            } else {
                Some(rows)
            }
        }
        crate::sql::LogicalPlan::Filter { input, .. } => {
            estimate_hash_join_rows(executor, input).map(estimate_filtered_rows)
        }
        crate::sql::LogicalPlan::Project { input, .. } | crate::sql::LogicalPlan::Sort { input, .. } => {
            estimate_hash_join_rows(executor, input)
        }
        crate::sql::LogicalPlan::Limit { input, limit, .. } => {
            estimate_hash_join_rows(executor, input).map(|rows| rows.min(*limit))
        }
        _ => None,
    }
}

fn estimate_index_nested_loop_probe_rows(executor: &Executor<'_>, plan: &crate::sql::LogicalPlan) -> Option<usize> {
    match plan {
        crate::sql::LogicalPlan::FilteredScan {
            table_name,
            alias,
            schema,
            predicate: Some(predicate),
            ..
        } => indexed_equality_predicate_is_selective(executor, table_name, alias.as_deref(), schema, predicate)
            .then_some(1),
        crate::sql::LogicalPlan::Filter { input, .. } => {
            estimate_index_nested_loop_probe_rows(executor, input).map(estimate_filtered_rows)
        }
        crate::sql::LogicalPlan::Project { input, .. } | crate::sql::LogicalPlan::Sort { input, .. } => {
            estimate_index_nested_loop_probe_rows(executor, input)
        }
        crate::sql::LogicalPlan::Limit { input, limit, .. } => {
            estimate_index_nested_loop_probe_rows(executor, input).map(|rows| rows.min(*limit))
        }
        crate::sql::LogicalPlan::Join {
            left,
            right,
            join_type,
            on: Some(condition),
            lateral: false,
        } if matches!(join_type, crate::sql::JoinType::Inner) => {
            let left_rows = estimate_index_nested_loop_probe_rows(executor, left)?;
            if left_rows > 128 || !is_plain_scan_like(right) || !right_join_key_has_index(executor, right, condition) {
                return None;
            }
            Some(left_rows.saturating_mul(8).min(128))
        }
        _ => None,
    }
}

fn indexed_equality_predicate_is_selective(
    executor: &Executor<'_>,
    table_name: &str,
    alias: Option<&str>,
    schema: &Schema,
    predicate: &crate::sql::LogicalExpr,
) -> bool {
    use crate::sql::{BinaryOperator, LogicalExpr};
    let LogicalExpr::BinaryExpr {
        left,
        op: BinaryOperator::Eq,
        right,
    } = predicate
    else {
        return false;
    };
    let column = match (left.as_ref(), right.as_ref()) {
        (LogicalExpr::Column { table, name }, LogicalExpr::Literal(_) | LogicalExpr::Parameter { .. })
        | (LogicalExpr::Literal(_) | LogicalExpr::Parameter { .. }, LogicalExpr::Column { table, name }) => {
            if table.as_deref().is_some_and(|qualifier| {
                !qualifier.eq_ignore_ascii_case(table_name)
                    && !alias.is_some_and(|alias| qualifier.eq_ignore_ascii_case(alias))
            }) {
                return false;
            }
            name
        }
        _ => return false,
    };
    if !schema.columns.iter().any(|col| col.name.eq_ignore_ascii_case(column)) {
        return false;
    }
    executor
        .storage()
        .and_then(|storage| storage.art_indexes().find_column_index(table_name, column))
        .is_some()
}

/// Is ANY `=` term of `condition` an indexable equality on `right`'s join
/// column? A CARDINALITY heuristic, not a plan: it asks whether an index
/// nested loop could be worth trying further up a left-deep chain.
///
/// GH#29 (c7, n2): it walks the `AND` chain. Candidate 6 removed the `And`
/// arm from [`extract_equi_columns`] — correctly, because the INLJ can only
/// EXECUTE one equality and silently dropped every other term — but this
/// heuristic and [`estimate_index_nested_loop_probe_rows`] share that helper,
/// so a left-deep chain whose inner node carried a compound ON lost INLJ
/// eligibility for the OUTER join too, even when the outer ON was a single
/// equality. "Is there an indexable equality on this side" is a legitimate
/// question for a compound ON; "which single equality IS the whole condition"
/// is not, and stays strict.
fn right_join_key_has_index(
    executor: &Executor<'_>,
    right: &crate::sql::LogicalPlan,
    condition: &crate::sql::LogicalExpr,
) -> bool {
    let Some(storage) = executor.storage() else {
        return false;
    };
    let Some((right_table, right_alias, _right_schema)) = extract_scan_info(right) else {
        return false;
    };
    let mut pairs = Vec::new();
    collect_equi_column_pairs(condition, &mut pairs);
    pairs.into_iter().any(|(left_col, right_col)| {
        let right_join_col = if column_matches_table(&right_col, &right_table, right_alias.as_deref()) {
            right_col.1
        } else if column_matches_table(&left_col, &right_table, right_alias.as_deref()) {
            left_col.1
        } else {
            return false;
        };
        storage
            .art_indexes()
            .find_column_index(&right_table, &right_join_col)
            .is_some()
    })
}

/// Every plain `Column = Column` term of an `AND` chain, in order. Used by
/// the cardinality heuristics only (GH#29 c7, n2) — the key PLAN is built by
/// [`plan_join_condition`], which binds each term against the real schemas.
fn collect_equi_column_pairs(
    condition: &crate::sql::LogicalExpr,
    out: &mut Vec<((Option<String>, String), (Option<String>, String))>,
) {
    use crate::sql::{BinaryOperator, LogicalExpr};
    match condition {
        LogicalExpr::BinaryExpr {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            collect_equi_column_pairs(left, out);
            collect_equi_column_pairs(right, out);
        }
        other => {
            if let Some(pair) = extract_equi_columns(other) {
                out.push(pair);
            }
        }
    }
}

fn should_build_left_for_inner(left_rows: Option<usize>, right_rows: Option<usize>) -> bool {
    match (left_rows, right_rows) {
        (Some(l), Some(r)) => l.saturating_mul(4) < r,
        _ => false,
    }
}

fn should_try_index_nested_loop_join(left_rows: Option<usize>, right_rows: Option<usize>) -> bool {
    if std::env::var("HELIOS_INLJ_OFF").is_ok() {
        return false;
    }
    match (left_rows, right_rows) {
        (Some(l), Some(r)) => l <= 128 || l.saturating_mul(8) <= r,
        (Some(l), None) => l <= 128,
        _ => false,
    }
}

fn is_plain_scan_like(plan: &crate::sql::LogicalPlan) -> bool {
    match plan {
        crate::sql::LogicalPlan::Scan { .. } => true,
        // GH#29 (c2): a projection stamped with a derived-table alias is a
        // range entry of its own, not a plain scan — its output must keep the
        // projection AND the alias tag, which the index-nested-loop path
        // (which reads the base scan directly) would drop.
        crate::sql::LogicalPlan::Project {
            input,
            source_alias: None,
            ..
        } => is_plain_scan_like(input),
        _ => false,
    }
}

fn estimate_filtered_rows(rows: usize) -> usize {
    rows.saturating_mul(33).saturating_add(99).saturating_div(100).max(1)
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
    let (right_table, right_schema, right_output_schema, index_name, left_join_col, right_join_col_type) = {
        let storage = match executor.storage() {
            Some(s) => s,
            None => return Ok(None),
        };

        // Visibility guard (correctness): INLJ probes committed ART/storage
        // directly and bypasses branch routing. When a branch is active, fall
        // back to the branch-correct hash/NLJ path rather than returning
        // main-branch rows.
        if storage.is_branch_active() {
            return Ok(None);
        }

        // Extract the right table name and schema from a plain right scan.
        // Callers exclude filtered right inputs because this helper probes
        // ART row IDs directly and would otherwise bypass the right predicate.
        let (right_table, right_alias, right_schema) = match extract_scan_info(right) {
            Some(info) => info,
            None => return Ok(None),
        };
        let right_source_name = right_alias.as_deref().unwrap_or(right_table.as_str());
        let right_output_schema = Arc::new(super::scan::schema_with_source(
            right_schema.as_ref(),
            right_source_name,
            &right_table,
        ));

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

        // Capture the right join column's declared type so Phase 2 can require
        // the left key to encode to the SAME byte layout (encode_key is
        // type-width sensitive). None here -> Phase 2 bails conservatively.
        let right_join_col_type = right_schema
            .columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(&right_join_col))
            .map(|c| c.data_type.clone());

        // Determine which column is the left key
        let left_join_col = if column_matches_table(&right_col, &right_table, right_alias.as_deref()) {
            left_col.clone()
        } else {
            right_col.clone()
        };

        (
            right_table,
            right_schema,
            right_output_schema,
            index_name,
            left_join_col,
            right_join_col_type,
        )
    }; // Immutable borrow of executor dropped here

    // Visibility guard (correctness): bail when the active transaction has a
    // stale snapshot or staged writes for the right table. INLJ bypasses the
    // write-set overlay, so without this it would return stale committed-only
    // rows / miss read-your-own-writes — diverging from the hash/NLJ path.
    if executor.txn_forces_slow_reads_for_table(&right_table) {
        return Ok(None);
    }

    // Phase 2: Build left operator (mutable borrow of executor)
    let mut left_op = executor.plan_to_operator(left)?;
    let left_schema = left_op.schema();

    // Find the column index of the join key in left tuples
    let left_key_idx = match find_column_index(&left_schema, left_join_col.0.as_deref(), &left_join_col.1) {
        Some(idx) => idx,
        None => return Ok(None),
    };

    // Type-equivalence guard (correctness): INLJ encodes the LEFT key value and
    // probes the RIGHT column's index. encode_key is type-width sensitive
    // (Int2=2B, Int4=4B, Int8=8B, etc.), so a cross-type equi-join such as
    // `int4_fk = int8_pk` or `text = int` would build a key with a different
    // byte layout than the index entries and silently drop every match. Only
    // proceed when the left key column and right index column share a type;
    // otherwise fall back to the type-coercing hash/NLJ path.
    match (
        left_schema.columns.get(left_key_idx).map(|c| &c.data_type),
        right_join_col_type.as_ref(),
    ) {
        (Some(lt), Some(rt)) if lt == rt => {}
        _ => return Ok(None),
    }

    // Build output schema (left + right)
    let mut output_columns = left_schema.columns.clone();
    output_columns.extend(right_output_schema.columns.clone());
    let output_schema = Arc::new(Schema {
        columns: output_columns,
    });

    let right_col_count = right_schema.columns.len();
    let is_left_join = matches!(join_type, crate::sql::JoinType::Left);

    // Phase 3: Execute INLJ (immutable borrow of executor/storage again)
    let storage = executor
        .storage()
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
                    result_tuples.push(Tuple {
                        values: combined_values,
                        row_id: None,
                        branch_id: None,
                    });
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
                result_tuples.push(Tuple {
                    values: combined_values,
                    row_id: None,
                    branch_id: None,
                });
            }
            continue;
        }

        // Fetch each matching right row and combine. Use the Arc variant: we
        // only borrow the right row here to copy its values into the combined
        // tuple, so a cache hit/fill avoids deep-copying the whole row.
        for row_id in matching_row_ids {
            if let Some(right_tuple) = storage.get_row_by_id_arc(&right_table, row_id, &right_schema)? {
                let mut combined_values = Vec::with_capacity(left_tuple.values.len() + right_tuple.values.len());
                combined_values.extend_from_slice(&left_tuple.values);
                combined_values.extend_from_slice(&right_tuple.values);
                result_tuples.push(Tuple {
                    values: combined_values,
                    row_id: None,
                    branch_id: None,
                });
            }
        }
    }

    Ok(Some(Box::new(super::MaterializedOperator::new(
        result_tuples,
        output_schema,
    ))))
}

/// Extract table name, alias, and schema from a Scan or Filter(Scan) plan node
fn extract_scan_info(plan: &crate::sql::LogicalPlan) -> Option<(String, Option<String>, Arc<Schema>)> {
    match plan {
        crate::sql::LogicalPlan::Scan {
            table_name,
            alias,
            schema,
            ..
        } => Some((table_name.clone(), alias.clone(), schema.clone())),
        crate::sql::LogicalPlan::Filter { input, .. } => extract_scan_info(input),
        // GH#29 (c2): see `is_plain_scan_like` — never see through a stamped
        // derived-table projection.
        crate::sql::LogicalPlan::Project {
            input,
            source_alias: None,
            ..
        } => extract_scan_info(input),
        _ => None,
    }
}

/// Extract the column pair of a join condition that is ONE equality:
/// `col1 = col2`. Returns (table_option, column_name) for each side.
///
/// GH#29 (c6, m4): a compound `AND` condition is refused. The index nested
/// loop probes the right table's ART index on this one pair and emits every
/// row it finds — it has nowhere to apply a SECOND term — so descending into
/// the left operand of an `AND` silently DROPPED every other term of the ON
/// clause. `sa JOIN sb ON sa.id = sb.id AND sa.x = 20` came back with both
/// rows; `sa LEFT JOIN sb ON sa.id = sb.id AND sa.x = (SELECT max(x) FROM sc)`
/// came back with the wrong pairs. (On the text family the optimizer pushes
/// most such terms into a scan, which is why only the shapes it may NOT push
/// — anything kept on an outer join — and the whole parameterized family,
/// which runs no optimizer passes, showed it.) A compound condition now falls
/// through to the hash join, which keys what it can and evaluates the rest.
fn extract_equi_columns(
    condition: &crate::sql::LogicalExpr,
) -> Option<((Option<String>, String), (Option<String>, String))> {
    use crate::sql::{BinaryOperator, LogicalExpr};

    match condition {
        LogicalExpr::BinaryExpr {
            left,
            op: BinaryOperator::Eq,
            right,
        } => match (left.as_ref(), right.as_ref()) {
            (LogicalExpr::Column { table: lt, name: ln }, LogicalExpr::Column { table: rt, name: rn }) => {
                Some(((lt.clone(), ln.clone()), (rt.clone(), rn.clone())))
            }
            _ => None,
        },
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

/// Find the index of the INLJ left join-key column in the left input's
/// schema by optional table qualifier and column name.
///
/// GH#29 c3 (M1): with a qualifier, the alias (`source_table`) match wins
/// over the real-name (`source_table_name`) match — the same order
/// `Schema::get_qualified_column_index` uses — and a qualifier that matches
/// NOTHING returns `None`, so the caller declines the index nested loop
/// (hash/NLJ resolve the key themselves) instead of guessing the first
/// column of that name. Through candidate 2 the qualifier was matched
/// against `source_table_name` only (which a stamped derived table never
/// carries) and a miss fell back to the first name match — the wrong slot
/// whenever a left-side join carried the key name twice.
fn find_column_index(schema: &Schema, table: Option<&str>, name: &str) -> Option<usize> {
    match table {
        Some(qualifier) => schema.get_qualified_column_index(Some(qualifier), name),
        None => schema.columns.iter().position(|c| c.name == name),
    }
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

/// The residual to stack as a post-join `FilterOperator` above a projected
/// INNER join — `None` when the join operator is already carrying the WHOLE
/// ON condition (GH#29 c7, M4).
///
/// `handle_projected_join` builds a `NestedLoopJoinOperator` over the whole
/// materialized condition whenever no key term bound. `residual` is then that
/// same condition (equi is empty, so the residual bucket holds every term),
/// and candidate 6 stacked it AGAIN as a filter: every surviving row had the
/// full ON condition evaluated TWICE, on the slowest operator we have.
/// Idempotent for a deterministic predicate — but not for a non-deterministic
/// one, and never free.
fn post_join_residual_filter(
    operator_carries_whole_condition: bool,
    residual: Option<crate::sql::LogicalExpr>,
) -> Option<crate::sql::LogicalExpr> {
    if operator_carries_whole_condition {
        return None;
    }
    residual
}

/// Check if a join condition is purely equi-join (only equality + AND).
/// Used internally by HashJoinOperator to skip redundant condition re-evaluation.
fn is_pure_equi_join(condition: &Option<crate::sql::LogicalExpr>) -> bool {
    use crate::sql::{BinaryOperator, LogicalExpr};

    fn check(expr: &LogicalExpr) -> bool {
        match expr {
            LogicalExpr::BinaryExpr {
                op: BinaryOperator::Eq, ..
            } => true,
            LogicalExpr::BinaryExpr {
                left,
                op: BinaryOperator::And,
                right,
            } => check(left) && check(right),
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
    use crate::sql::{BinaryOperator, LogicalExpr};

    match expr {
        LogicalExpr::BinaryExpr {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            collect_and_terms(left, equi, residual);
            collect_and_terms(right, equi, residual);
        }
        LogicalExpr::BinaryExpr {
            op: BinaryOperator::Eq, ..
        } => {
            equi.push(expr.clone());
        }
        _ => {
            residual.push(expr.clone());
        }
    }
}

/// Combine a list of predicates with AND
fn combine_with_and(parts: Vec<crate::sql::LogicalExpr>) -> Option<crate::sql::LogicalExpr> {
    use crate::sql::{BinaryOperator, LogicalExpr};

    parts.into_iter().reduce(|left, right| LogicalExpr::BinaryExpr {
        left: Box::new(left),
        op: BinaryOperator::And,
        right: Box::new(right),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod gh29_c5_key_binding_tests {
    //! GH#29 (c5): the construction-time key binder, pinned structurally —
    //! an integration test can only see the rows, not whether they came out
    //! of a hash bucket. Each input is a one-column schema whose `id` is
    //! stamped with the alias (`source_table`) and the real name
    //! (`source_table_name`) exactly as `handle_scan` stamps a base-table
    //! scan.
    use super::*;
    use crate::sql::{BinaryOperator, JoinType, LogicalExpr};
    use crate::{Column, DataType, Value};

    fn input(alias: &str, real: &str) -> Schema {
        let mut id = Column::new("id", DataType::Int4);
        id.source_table = Some(alias.to_string());
        id.source_table_name = Some(real.to_string());
        Schema { columns: vec![id] }
    }

    fn col(table: Option<&str>, name: &str) -> LogicalExpr {
        LogicalExpr::Column {
            table: table.map(str::to_string),
            name: name.to_string(),
        }
    }

    fn binary(left: LogicalExpr, op: BinaryOperator, right: LogicalExpr) -> LogicalExpr {
        LogicalExpr::BinaryExpr {
            left: Box::new(left),
            op,
            right: Box::new(right),
        }
    }

    fn plus_one(expr: LogicalExpr) -> LogicalExpr {
        binary(expr, BinaryOperator::Plus, LogicalExpr::Literal(Value::Int4(1)))
    }

    fn eq(left: LogicalExpr, right: LogicalExpr) -> LogicalExpr {
        binary(left, BinaryOperator::Eq, right)
    }

    fn is_bound_slot_zero(expr: &LogicalExpr) -> bool {
        matches!(expr, LogicalExpr::BoundColumn { index: 0, .. })
    }

    #[test]
    fn case_distinct_quoted_aliases_bind_to_their_exact_side() {
        // FROM t AS "A" JOIN t AS "a" ON "a".id = "A".id + 1
        let left = input("A", "t");
        let right = input("a", "t");
        let (pairs, covered) =
            bind_hash_join_key_pairs(&eq(col(Some("a"), "id"), plus_one(col(Some("A"), "id"))), &left, &right);
        assert!(covered);
        assert_eq!(pairs.len(), 1);
        // `"a".id` is the RIGHT input's column; `"A".id + 1` the LEFT's.
        assert!(is_bound_slot_zero(&pairs[0].right), "{:?}", pairs[0].right);
        assert!(
            matches!(&pairs[0].left, LogicalExpr::BinaryExpr { left, .. } if is_bound_slot_zero(left)),
            "{:?}",
            pairs[0].left
        );
        // Reversed spelling, same assignment.
        let (pairs, covered) =
            bind_hash_join_key_pairs(&eq(plus_one(col(Some("A"), "id")), col(Some("a"), "id")), &left, &right);
        assert!(covered);
        assert_eq!(pairs.len(), 1);
        assert!(is_bound_slot_zero(&pairs[0].right), "{:?}", pairs[0].right);
    }

    #[test]
    fn the_unquoted_twin_still_hashes() {
        // FROM t AS a JOIN t AS b ON b.id = a.id + 1
        let left = input("a", "t");
        let right = input("b", "t");
        let condition = eq(col(Some("b"), "id"), plus_one(col(Some("a"), "id")));
        let (pairs, covered) = bind_hash_join_key_pairs(&condition, &left, &right);
        assert!(covered);
        assert_eq!(pairs.len(), 1);
        assert!(is_bound_slot_zero(&pairs[0].right), "{:?}", pairs[0].right);
        assert!(!hash_join_keys_need_nested_loop(
            &condition,
            &JoinType::Inner,
            &left,
            &right
        ));
    }

    #[test]
    fn the_case_folded_pass_still_serves_an_unquoted_reference_to_a_mixed_case_alias() {
        // Alias `"Acc"` referenced as `acc`: the exact pass matches on
        // neither side, the folded fallback does — leniency kept, second.
        let left = input("Acc", "t");
        let right = input("b", "t");
        let (pairs, covered) = bind_hash_join_key_pairs(
            &eq(plus_one(col(Some("acc"), "id")), col(Some("b"), "id")),
            &left,
            &right,
        );
        assert!(covered);
        assert_eq!(pairs.len(), 1);
        assert!(is_bound_slot_zero(&pairs[0].right), "{:?}", pairs[0].right);
    }

    #[test]
    fn a_term_whose_qualified_operands_both_fit_both_sides_is_declined() {
        // Aliases `"Acc"` and `"ACC"` are two legal, case-distinct range
        // entries; `acc.id` matches NEITHER exactly, so the case-folded pass
        // resolves it on BOTH — a real alias collision. Guessing an order
        // here is what keyed `ON "a".id = "A".id + 1` backwards.
        let left = input("Acc", "t");
        let right = input("ACC", "t");
        let condition = eq(col(Some("acc"), "id"), plus_one(col(Some("acc"), "id")));
        let (pairs, covered) = bind_hash_join_key_pairs(&condition, &left, &right);
        assert!(!covered);
        assert!(pairs.is_empty());
        // Nothing to hash on: a nested-loop join, whatever the join type.
        assert!(hash_join_keys_need_nested_loop(
            &condition,
            &JoinType::Inner,
            &left,
            &right
        ));
    }

    #[test]
    fn a_bare_name_both_inputs_carry_keys_left_to_right() {
        // GH#29 (c6, BLOCKER). This is the shape the planner lowers EVERY
        // `NATURAL JOIN` / `JOIN … USING (id)` to — both operands bare, with
        // no qualifier on either side, whatever the inputs are. Candidate 5
        // declined it as an "alias collision", the
        // key came out empty, the join fell to the nested loop, and the
        // nested loop's combined-schema binder resolved BOTH bare operands to
        // the LEFT input's slot: `left.id = left.id`, true for every pair —
        // a cartesian product. It must bind ONE pair, left slot to right slot.
        let left = input("a", "t");
        let right = input("b", "t");
        let condition = eq(col(None, "id"), col(None, "id"));
        let (pairs, covered) = bind_hash_join_key_pairs(&condition, &left, &right);
        assert!(covered);
        assert_eq!(pairs.len(), 1);
        assert!(is_bound_slot_zero(&pairs[0].left), "{:?}", pairs[0].left);
        assert!(is_bound_slot_zero(&pairs[0].right), "{:?}", pairs[0].right);
        assert!(
            !hash_join_keys_need_nested_loop(&condition, &JoinType::Inner, &left, &right),
            "a NATURAL/USING key must never be routed to the nested loop"
        );
        for join_type in [JoinType::Left, JoinType::Right, JoinType::Full] {
            assert!(
                !hash_join_keys_need_nested_loop(&condition, &join_type, &left, &right),
                "{join_type:?}"
            );
        }
        // …and the expression spelling still binds one pair, natural order.
        let condition = eq(col(None, "id"), plus_one(col(None, "id")));
        let (pairs, covered) = bind_hash_join_key_pairs(&condition, &left, &right);
        assert!(covered);
        assert_eq!(pairs.len(), 1);
        assert!(is_bound_slot_zero(&pairs[0].left), "{:?}", pairs[0].left);
        assert!(
            matches!(&pairs[0].right, LogicalExpr::BinaryExpr { left, .. } if is_bound_slot_zero(left)),
            "{:?}",
            pairs[0].right
        );
        assert!(!hash_join_keys_need_nested_loop(
            &condition,
            &JoinType::Inner,
            &left,
            &right
        ));
    }

    #[test]
    fn a_declined_equality_term_lands_in_the_residual_never_nowhere() {
        // GH#29 (c6, m4): `collect_and_terms` bucketed by SYNTAX, so an `=`
        // term no binder can bind (a literal operand, a materialized
        // subquery) went to the key bucket, was declined there, and then
        // appeared in NEITHER bucket — dropped, with `is_pure_equi_join`
        // telling the operator the keys were the whole condition.
        let left = input("a", "t");
        let right = input("b", "t");
        let condition = binary(
            eq(col(Some("a"), "id"), col(Some("b"), "id")),
            BinaryOperator::And,
            eq(LogicalExpr::Literal(Value::Int4(5)), col(Some("b"), "id")),
        );
        let plan = plan_join_condition(&condition, &left, &right);
        assert!(plan.equi.is_some(), "the column pair still keys the join");
        let residual = plan.residual.expect("the literal term must be in the residual");
        assert!(
            matches!(&residual, LogicalExpr::BinaryExpr { left, op: BinaryOperator::Eq, .. }
                if matches!(left.as_ref(), LogicalExpr::Literal(Value::Int4(5)))),
            "{residual:?}"
        );
        // A non-equality term keeps going to the residual, as before.
        let condition = binary(
            eq(col(Some("a"), "id"), col(Some("b"), "id")),
            BinaryOperator::And,
            binary(
                col(Some("b"), "id"),
                BinaryOperator::Gt,
                LogicalExpr::Literal(Value::Int4(5)),
            ),
        );
        let plan = plan_join_condition(&condition, &left, &right);
        assert!(plan.equi.is_some());
        assert!(plan.residual.is_some());
        // …and under RIGHT / FULL a residual takes the nested loop, which
        // tracks matched build rows per TUPLE, not per key bucket.
        assert!(join_condition_needs_nested_loop(&plan, &JoinType::Right));
        assert!(join_condition_needs_nested_loop(&plan, &JoinType::Full));
        assert!(!join_condition_needs_nested_loop(&plan, &JoinType::Inner));
        assert!(!join_condition_needs_nested_loop(&plan, &JoinType::Left));
    }

    #[test]
    fn the_index_nested_loop_never_takes_a_compound_on_condition() {
        // GH#29 (c6, m4): `extract_equi_columns` descended into the LEFT
        // operand of an `AND` and returned the first equality; the index
        // nested loop then probed on that pair alone and emitted every row it
        // found, DROPPING every other term of the ON clause.
        let one = eq(col(Some("a"), "id"), col(Some("b"), "id"));
        assert!(extract_equi_columns(&one).is_some());
        let compound = binary(
            one,
            BinaryOperator::And,
            eq(col(Some("a"), "x"), LogicalExpr::Literal(Value::Int4(20))),
        );
        assert!(
            extract_equi_columns(&compound).is_none(),
            "a compound ON must fall through to the hash join"
        );
    }

    #[test]
    fn a_literal_operand_is_never_keyed_and_right_or_full_declines_to_nested_loop() {
        // ON a.id = b.id AND 5 = b.id
        let left = input("a", "t");
        let right = input("b", "t");
        let condition = binary(
            eq(col(Some("a"), "id"), col(Some("b"), "id")),
            BinaryOperator::And,
            eq(LogicalExpr::Literal(Value::Int4(5)), col(Some("b"), "id")),
        );
        let (pairs, covered) = bind_hash_join_key_pairs(&condition, &left, &right);
        assert!(!covered, "the literal term leaves the key");
        assert_eq!(pairs.len(), 1, "the column pair still hashes");
        assert!(!hash_join_keys_need_nested_loop(
            &condition,
            &JoinType::Inner,
            &left,
            &right
        ));
        assert!(!hash_join_keys_need_nested_loop(
            &condition,
            &JoinType::Left,
            &left,
            &right
        ));
        assert!(hash_join_keys_need_nested_loop(
            &condition,
            &JoinType::Right,
            &left,
            &right
        ));
        assert!(hash_join_keys_need_nested_loop(
            &condition,
            &JoinType::Full,
            &left,
            &right
        ));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod gh29_c7_operator_tests {
    //! GH#29 (candidate 7): the pieces an integration test cannot see —
    //! WHICH operator carries the condition, whether a filter is stacked on
    //! top of one that already does, and whether the nested loop's
    //! materialization is capped like the hash join's.
    use super::*;
    use crate::sql::{BinaryOperator, JoinType, LogicalExpr};
    use crate::{Column, DataType, Value};

    fn schema(name: &str, alias: &str) -> Arc<Schema> {
        let mut column = Column::new(name, DataType::Int4);
        column.source_table = Some(alias.to_string());
        column.source_table_name = Some(alias.to_string());
        Arc::new(Schema { columns: vec![column] })
    }

    fn rows(schema: &Arc<Schema>, values: &[i32]) -> Box<dyn PhysicalOperator> {
        let tuples = values
            .iter()
            .map(|v| Tuple::new(vec![Value::Int4(*v)]))
            .collect::<Vec<_>>();
        Box::new(super::super::scan::MaterializedOperator::new(
            tuples,
            Arc::clone(schema),
        ))
    }

    fn eq(left: LogicalExpr, right: LogicalExpr) -> LogicalExpr {
        LogicalExpr::BinaryExpr {
            left: Box::new(left),
            op: BinaryOperator::Eq,
            right: Box::new(right),
        }
    }

    fn col(table: &str, name: &str) -> LogicalExpr {
        LogicalExpr::Column {
            table: Some(table.to_string()),
            name: name.to_string(),
        }
    }

    /// M3. The nested loop's right-input materialization is capped, with the
    /// SAME error the hash join raises — candidate 6 routes every RIGHT /
    /// FULL join carrying a residual ON term to this operator, and it had no
    /// cap at all on a host this repo has already OOM-killed once.
    #[test]
    fn the_nested_loop_materialization_is_capped_like_the_hash_join_and_says_so_the_same_way() {
        let left = schema("id", "a");
        let right = schema("id", "b");
        let condition = eq(col("a", "id"), col("b", "id"));

        // `expect_err` would need the operator itself to be `Debug` (it owns
        // boxed child operators), so match instead.
        let nested_loop = match NestedLoopJoinOperator::with_memory_limit(
            rows(&left, &[1, 2, 3, 4, 5]),
            rows(&right, &[1, 2, 3, 4, 5]),
            JoinType::Full,
            Some(condition.clone()),
            Vec::new(),
            1, // one byte: the first build tuple already exceeds it
            None,
        ) {
            Ok(_) => panic!("the nested loop must refuse to materialize past the cap"),
            Err(err) => err.to_string(),
        };
        assert!(
            nested_loop.contains("Join exceeds memory limit"),
            "nested loop: {nested_loop}"
        );
        assert!(
            nested_loop.contains("join_memory_limit_mb") && nested_loop.contains("HELIOSDB_HASH_JOIN_MEM_MB"),
            "the message must name the config key AND the env override: {nested_loop}"
        );

        let hash = match HashJoinOperator::with_memory_limit(
            rows(&left, &[1, 2, 3, 4, 5]),
            rows(&right, &[1, 2, 3, 4, 5]),
            JoinType::Full,
            Some(condition),
            Vec::new(),
            HashJoinBuildSide::Right,
            1,
            None,
        ) {
            Ok(_) => panic!("the hash join must refuse the same way"),
            Err(err) => err.to_string(),
        };
        assert_eq!(nested_loop, hash, "one cap, one error, both operators");

        // Under the cap, the same join runs.
        let left_schema = schema("id", "a");
        let right_schema = schema("id", "b");
        let mut op = NestedLoopJoinOperator::with_memory_limit(
            rows(&left_schema, &[1, 2]),
            rows(&right_schema, &[2, 3]),
            JoinType::Inner,
            Some(eq(col("a", "id"), col("b", "id"))),
            Vec::new(),
            1024 * 1024,
            None,
        )
        .expect("under the cap");
        let mut emitted = 0;
        while op.next().expect("next").is_some() {
            emitted += 1;
        }
        assert_eq!(emitted, 1, "only id = 2 joins");
    }

    /// M3. The cap comes from the ONE configuration key
    /// (`[performance] join_memory_limit_mb`, set by `--join-memory-limit-mb`
    /// or the file), with the documented environment variable still winning
    /// and no second threshold anywhere.
    #[test]
    fn the_join_memory_cap_comes_from_the_configuration_key() {
        const MB: usize = 1024 * 1024;
        assert_eq!(
            resolve_join_memory_limit(None, 0),
            DEFAULT_JOIN_MEMORY_LIMIT_MB * MB,
            "nothing set: the built-in default"
        );
        assert_eq!(
            resolve_join_memory_limit(None, 64),
            64 * MB,
            "[performance] join_memory_limit_mb / --join-memory-limit-mb"
        );
        assert_eq!(
            resolve_join_memory_limit(Some("32"), 64),
            32 * MB,
            "HELIOSDB_HASH_JOIN_MEM_MB overrides the configured value"
        );
        assert_eq!(
            resolve_join_memory_limit(Some("nonsense"), 64),
            64 * MB,
            "an unparseable override falls back to the configured value"
        );
        assert_eq!(
            resolve_join_memory_limit(Some("0"), 0),
            DEFAULT_JOIN_MEMORY_LIMIT_MB * MB,
            "0 = the built-in default"
        );
        assert_eq!(
            resolve_join_memory_limit(Some(""), 64),
            64 * MB,
            "an empty override is not a value"
        );
        // GH#29 (c9, m4): this test asserts the PURE resolver and nothing
        // else. Candidate 8 parked `JOIN_MEMORY_LIMIT_MB` — a process-global
        // (`set_join_memory_limit_mb`) read at every join construction — at
        // 48 MB for the duration of the test, while `cargo test --lib` runs
        // the whole binary's tests on parallel threads and every
        // `EmbeddedDatabase` open in it stores 1024 over that global
        // (`src/lib.rs`, `set_join_memory_limit_mb(config.performance
        // .join_memory_limit_mb)`). That can redden this test AND, for the
        // window it is held, any join elsewhere in the binary that
        // materializes more than 48 MB. A concurrent unit test may not mutate
        // a process-global other tests read — the precedent this repo already
        // states for its own flags (`src/copy_phase_stats.rs`,
        // `src/write_volume.rs`). The setter/reader wiring is covered where it
        // is owned: the operators below take an EXPLICIT limit, and the
        // configuration key reaches the global through `EmbeddedDatabase`'s
        // own open path.
    }

    /// M3 scope (GH#29 c8, m3a). The cap is charged on EVERY nested-loop
    /// materialization — not only the RIGHT/FULL-with-residual shape that
    /// made candidate 6 route joins here. A LATERAL join (whose call site
    /// hands this operator the ON condition as written, `None` for
    /// `FROM a, LATERAL (…)`) and a THETA join (`ON a.x > b.y`, which binds
    /// no key at all) both materialize their right input, both were
    /// previously unbounded, and both now fail with the one error.
    #[test]
    fn the_cap_covers_every_nested_loop_shape_not_only_right_full_with_a_residual() {
        let left = schema("id", "a");
        let right = schema("id", "b");
        let expected = join_memory_limit_exceeded(1).to_string();

        // The LATERAL / cross shape: no ON condition at all.
        let lateral = match NestedLoopJoinOperator::with_memory_limit(
            rows(&left, &[1, 2]),
            rows(&right, &[1, 2, 3, 4, 5]),
            JoinType::Inner,
            None,
            Vec::new(),
            1,
            None,
        ) {
            Ok(_) => panic!("an uncapped LATERAL materialization is what OOM-killed this host"),
            Err(err) => err.to_string(),
        };
        assert_eq!(lateral, expected, "one cap, one error");

        // A theta join: an INNER join whose ON binds no key.
        let theta = match NestedLoopJoinOperator::with_memory_limit(
            rows(&left, &[1, 2]),
            rows(&right, &[1, 2, 3, 4, 5]),
            JoinType::Inner,
            Some(LogicalExpr::BinaryExpr {
                left: Box::new(col("a", "id")),
                op: BinaryOperator::Gt,
                right: Box::new(col("b", "id")),
            }),
            Vec::new(),
            1,
            None,
        ) {
            Ok(_) => panic!("a theta join materializes its right input too"),
            Err(err) => err.to_string(),
        };
        assert_eq!(theta, expected, "one cap, one error");
    }

    /// M4. The whole ON condition is applied ONCE. When no key term bound,
    /// `handle_projected_join` hands the nested loop the WHOLE condition —
    /// and candidate 6 then stacked that same condition again as a
    /// `FilterOperator`.
    #[test]
    fn a_projected_join_never_stacks_a_filter_over_an_operator_that_already_has_the_condition() {
        let residual = eq(col("a", "id"), col("b", "id"));
        assert!(
            post_join_residual_filter(true, Some(residual.clone())).is_none(),
            "the nested loop already carries the whole condition: nothing may be stacked on it"
        );
        // The hash-join branch keys part of the condition, so its residual IS
        // stacked — that arm must not regress into dropping the term.
        assert_eq!(
            post_join_residual_filter(false, Some(residual.clone())),
            Some(residual),
            "a residual the operator does NOT carry must still be filtered"
        );
        assert!(post_join_residual_filter(false, None).is_none());
    }

    /// M1. Every evaluator a join operator builds for a CONDITION carries the
    /// statement's bind values. `ON a.id = b.id AND b.id = $1` keys the first
    /// term and leaves the second to the evaluator; with an empty parameter
    /// vector that evaluator raised `Parameter $1 not provided`.
    #[test]
    fn a_parameter_in_a_join_condition_is_evaluated_not_refused() {
        let left = schema("id", "a");
        let right = schema("id", "b");
        let condition = LogicalExpr::BinaryExpr {
            left: Box::new(eq(col("a", "id"), col("b", "id"))),
            op: BinaryOperator::And,
            right: Box::new(eq(col("b", "id"), LogicalExpr::Parameter { index: 1 })),
        };

        // Nested loop: the whole condition, parameters threaded.
        let mut op = NestedLoopJoinOperator::new(
            rows(&left, &[1, 2, 3]),
            rows(&right, &[1, 2, 3]),
            JoinType::Inner,
            Some(condition.clone()),
            vec![Value::Int4(2)],
            None,
        )
        .expect("nested loop builds");
        let mut emitted = Vec::new();
        while let Some(tuple) = op.next().expect("the `$1` term must evaluate, not error") {
            emitted.push(tuple.values[0].clone());
        }
        assert_eq!(emitted, vec![Value::Int4(2)], "only id = $1 = 2 survives");

        // Hash join: the keyed term hashes, the `$1` term is the pair
        // residual — the binder declines it because a parameter is not a
        // column, which is exactly why the evaluator has to have the values.
        let plan = plan_join_condition(&condition, left.as_ref(), right.as_ref());
        let mut op = HashJoinOperator::new_with_keys(
            rows(&left, &[1, 2, 3]),
            rows(&right, &[1, 2, 3]),
            JoinType::Left,
            Some(eq(col("a", "id"), col("b", "id"))),
            Some(eq(col("b", "id"), LogicalExpr::Parameter { index: 1 })),
            vec![Value::Int4(2)],
            PreboundJoinKeys {
                direct_key_indices: None,
                bound_key_pairs: bind_hash_join_key_pairs(
                    &eq(col("a", "id"), col("b", "id")),
                    left.as_ref(),
                    right.as_ref(),
                )
                .0,
                keys_cover_condition: true,
            },
            HashJoinBuildSide::Right,
            None,
        )
        .expect("hash join builds");
        let mut emitted = Vec::new();
        while let Some(tuple) = op.next().expect("the `$1` residual must evaluate, not error") {
            emitted.push((tuple.values[0].clone(), tuple.values[1].clone()));
        }
        emitted.sort_by_key(|(l, _)| format!("{l:?}"));
        assert_eq!(
            emitted,
            vec![
                (Value::Int4(1), Value::Null),
                (Value::Int4(2), Value::Int4(2)),
                (Value::Int4(3), Value::Null),
            ],
            "LEFT: the probe rows whose only candidate fails the residual are NULL-extended, not dropped"
        );
        assert!(plan.equi.is_some(), "the `a.id = b.id` term keys the join");
        assert!(
            plan.residual.is_some(),
            "the `$1` term is bucketed as the residual, never dropped"
        );
    }

    /// n2. The cardinality heuristic walks the `AND` chain again — candidate
    /// 6's strict `extract_equi_columns` blinded it to a compound ON, which
    /// cost the OUTER join of a left-deep chain its INLJ eligibility.
    #[test]
    fn the_cardinality_heuristic_sees_an_equality_inside_a_compound_on() {
        let compound = LogicalExpr::BinaryExpr {
            left: Box::new(eq(col("a", "id"), col("b", "id"))),
            op: BinaryOperator::And,
            right: Box::new(eq(col("a", "x"), LogicalExpr::Literal(Value::Int4(20)))),
        };
        let mut pairs = Vec::new();
        collect_equi_column_pairs(&compound, &mut pairs);
        assert_eq!(pairs.len(), 1, "the one plain column equality of the chain");
        assert_eq!(pairs[0].0, (Some("a".to_string()), "id".to_string()));
        assert_eq!(pairs[0].1, (Some("b".to_string()), "id".to_string()));
        // The strict helper the PLAN uses still refuses the compound shape:
        // the index nested loop can execute exactly one equality.
        assert!(extract_equi_columns(&compound).is_none());
    }

    /// m7 (GH#29 c8). The pair residual is checked through a BORROWED pair
    /// view — the zero-copy seam the nested loop has used since R3.5 — not by
    /// allocating a fresh combined tuple (a deep clone of every value of both
    /// tuples) once per candidate pair. The rows are what they were, and a
    /// residual the pair evaluator does not cover keeps the old path.
    #[test]
    fn a_pair_residual_is_checked_on_a_borrowed_pair_not_on_a_fresh_combined_tuple() {
        let left = schema("id", "a");
        let right = schema("id", "b");
        let keyed = eq(col("a", "id"), col("b", "id"));
        let residual = LogicalExpr::BinaryExpr {
            left: Box::new(col("b", "id")),
            op: BinaryOperator::Gt,
            right: Box::new(LogicalExpr::Literal(Value::Int4(1))),
        };
        let keys = || PreboundJoinKeys {
            direct_key_indices: None,
            bound_key_pairs: bind_hash_join_key_pairs(&keyed, left.as_ref(), right.as_ref()).0,
            keys_cover_condition: true,
        };

        let mut op = HashJoinOperator::new_with_keys(
            rows(&left, &[1, 2, 3]),
            rows(&right, &[1, 2, 3]),
            JoinType::Left,
            Some(keyed.clone()),
            Some(residual),
            Vec::new(),
            keys(),
            HashJoinBuildSide::Right,
            None,
        )
        .expect("hash join builds");
        assert!(
            op.pair_residual_on_pair,
            "a plain comparison residual is evaluated on the borrowed pair"
        );
        let mut emitted = Vec::new();
        while let Some(tuple) = op.next().expect("next") {
            emitted.push((tuple.values[0].clone(), tuple.values[1].clone()));
        }
        emitted.sort_by_key(|(l, _)| format!("{l:?}"));
        assert_eq!(
            emitted,
            vec![
                (Value::Int4(1), Value::Null),
                (Value::Int4(2), Value::Int4(2)),
                (Value::Int4(3), Value::Int4(3)),
            ],
            "LEFT: the row whose only candidate fails the residual is NULL-extended"
        );

        // A `$n` is not in the pair evaluator's supported set, so that
        // residual keeps the materialize-then-evaluate path — and still
        // evaluates (M1).
        let op = HashJoinOperator::new_with_keys(
            rows(&left, &[1, 2, 3]),
            rows(&right, &[1, 2, 3]),
            JoinType::Left,
            Some(keyed.clone()),
            Some(eq(col("b", "id"), LogicalExpr::Parameter { index: 1 })),
            vec![Value::Int4(2)],
            keys(),
            HashJoinBuildSide::Right,
            None,
        )
        .expect("hash join builds");
        assert!(
            !op.pair_residual_on_pair,
            "a parameterized residual falls back to the combined tuple"
        );
    }
}
