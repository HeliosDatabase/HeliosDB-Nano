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
                let right_tuple = self
                    .right_tuples
                    .get(right_idx)
                    .ok_or_else(|| Error::query_execution("Right tuple index out of bounds"))?;
                self.right_index += 1;

                // Combine left and right tuples
                let left_tuple = self
                    .left_tuple
                    .as_ref()
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
    build_side: HashJoinBuildSide,

    // State machine
    state: JoinState,

    // Probe phase state
    current_left_tuple: Option<Tuple>,
    current_match_key: Option<JoinKey>,
    current_matches: Vec<Tuple>,
    match_index: usize,
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
            } else {
                value.hash(state);
            }
        }
        _ => value.hash(state),
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

fn direct_join_key_side(
    expr: &crate::sql::LogicalExpr,
    left_schema: &Schema,
    right_schema: &Schema,
) -> Option<DirectJoinKeySide> {
    let crate::sql::LogicalExpr::Column { table, name } = expr else {
        return None;
    };
    let left = resolve_direct_join_column(left_schema, table.as_deref(), name);
    let right = resolve_direct_join_column(right_schema, table.as_deref(), name);
    match (left, right) {
        (Some(idx), None) => Some(DirectJoinKeySide::Left(idx)),
        (None, Some(idx)) => Some(DirectJoinKeySide::Right(idx)),
        _ => None,
    }
}

fn resolve_direct_join_column(schema: &Schema, qualifier: Option<&str>, name: &str) -> Option<usize> {
    let mut matches = schema.columns.iter().enumerate().filter(|(_, column)| {
        if !column.name.eq_ignore_ascii_case(name) {
            return false;
        }
        qualifier.map_or(true, |q| {
            option_eq_ignore_ascii_case(column.source_table.as_deref(), q)
                || option_eq_ignore_ascii_case(column.source_table_name.as_deref(), q)
        })
    });
    let (idx, _) = matches.next()?;
    matches.next().is_none().then_some(idx)
}

fn option_eq_ignore_ascii_case(value: Option<&str>, expected: &str) -> bool {
    value.is_some_and(|value| value.eq_ignore_ascii_case(expected))
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
        Self::with_memory_limit(
            left,
            right,
            join_type,
            on_condition,
            HashJoinBuildSide::Right,
            Self::DEFAULT_MEMORY_LIMIT,
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
        timeout_ctx: Option<TimeoutContext>,
    ) -> Result<Self> {
        Self::with_memory_limit(
            left,
            right,
            join_type,
            on_condition,
            HashJoinBuildSide::Left,
            Self::DEFAULT_MEMORY_LIMIT,
            timeout_ctx,
        )
    }

    /// Create a projected inner hash join. The projection indexes refer to the
    /// normal combined left+right join schema, but emitted tuples contain only
    /// those projected values. This avoids building a full combined tuple only
    /// for a parent ProjectOperator to clone a small subset of columns.
    fn new_projected_inner(
        left: Box<dyn PhysicalOperator>,
        right: Box<dyn PhysicalOperator>,
        on_condition: Option<crate::sql::LogicalExpr>,
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
            build_side,
            Self::DEFAULT_MEMORY_LIMIT,
            timeout_ctx,
            Some(projection),
            Some(output_schema),
        )
    }

    /// Create a new hash join operator with custom memory limit
    fn with_memory_limit(
        left: Box<dyn PhysicalOperator>,
        right: Box<dyn PhysicalOperator>,
        join_type: crate::sql::JoinType,
        on_condition: Option<crate::sql::LogicalExpr>,
        build_side: HashJoinBuildSide,
        memory_limit: usize,
        timeout_ctx: Option<TimeoutContext>,
    ) -> Result<Self> {
        Self::with_memory_limit_projected(
            left,
            right,
            join_type,
            on_condition,
            build_side,
            memory_limit,
            timeout_ctx,
            None,
            None,
        )
    }

    fn with_memory_limit_projected(
        left: Box<dyn PhysicalOperator>,
        right: Box<dyn PhysicalOperator>,
        join_type: crate::sql::JoinType,
        on_condition: Option<crate::sql::LogicalExpr>,
        build_side: HashJoinBuildSide,
        memory_limit: usize,
        timeout_ctx: Option<TimeoutContext>,
        output_projection: Option<Vec<usize>>,
        projected_output_schema: Option<Arc<Schema>>,
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

        let direct_key_indices = on_condition
            .as_ref()
            .and_then(|condition| build_direct_join_key_indices(condition, &left_schema, &right_schema));
        let pure_equi_join = is_pure_equi_join(&on_condition);

        // Create evaluator with output schema for evaluating join conditions on combined tuples
        let evaluator = crate::sql::Evaluator::new(combined_schema);

        // Create separate evaluators for key extraction (left and right schemas)
        let left_evaluator = crate::sql::Evaluator::new(left_schema);
        let right_evaluator = crate::sql::Evaluator::new(right_schema);
        let (probe_input, mut build_input) = match build_side {
            HashJoinBuildSide::Right => (left, right),
            HashJoinBuildSide::Left => (right, left),
        };

        // Create the operator instance
        let mut operator = Self {
            left: probe_input,
            join_type,
            on_condition,
            hash_table: std::collections::HashMap::new(),
            output_schema,
            evaluator,
            left_evaluator,
            right_evaluator,
            direct_key_indices,
            build_side,
            state: JoinState::Initial,
            current_left_tuple: None,
            current_match_key: None,
            current_matches: Vec::new(),
            match_index: 0,
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
                return Err(Error::query_execution(format!(
                    "Hash join exceeds memory limit ({} bytes). Consider using nested loop join or increasing limit.",
                    self.memory_limit
                )));
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

        if let Some(condition) = &self.on_condition {
            let key_values = self.extract_join_columns(condition, tuple, is_right_side)?;

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

    /// Extract join column values from ON condition
    fn extract_join_columns(
        &self,
        condition: &crate::sql::LogicalExpr,
        tuple: &Tuple,
        is_right_side: bool,
    ) -> Result<Vec<crate::Value>> {
        use crate::sql::{BinaryOperator, LogicalExpr};

        // Use the appropriate evaluator based on which side's tuple we're evaluating
        let evaluator = if is_right_side {
            &self.right_evaluator
        } else {
            &self.left_evaluator
        };

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

            // Pure equi-joins can stream directly from the hash bucket. Avoid
            // cloning the whole match vector for every probe row.
            if let Some(key) = self.current_match_key.as_ref() {
                if let Some(matches) = self.hash_table.get(key) {
                    if self.match_index < matches.len() {
                        let right_tuple = matches
                            .get(self.match_index)
                            .ok_or_else(|| Error::query_execution("Match index out of bounds"))?;
                        let left_tuple = self
                            .current_left_tuple
                            .as_ref()
                            .ok_or_else(|| Error::query_execution("Missing left tuple"))?;
                        self.match_index += 1;
                        return Ok(Some(self.join_probe_with_build(left_tuple, right_tuple)));
                    }
                }

                self.current_match_key = None;
                self.current_left_tuple = None;
                self.match_index = 0;
            }

            // If we have pending matches for current left tuple, emit them
            if self.match_index < self.current_matches.len() {
                let right_tuple = self
                    .current_matches
                    .get(self.match_index)
                    .ok_or_else(|| Error::query_execution("Match index out of bounds"))?;
                self.match_index += 1;

                let left_tuple = self
                    .current_left_tuple
                    .as_ref()
                    .ok_or_else(|| Error::query_execution("Missing left tuple"))?;

                return Ok(Some(self.join_probe_with_build(left_tuple, right_tuple)));
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
                        if self.pure_equi_join {
                            // Mark key as matched (for RIGHT/FULL joins)
                            if matches!(self.join_type, crate::sql::JoinType::Right | crate::sql::JoinType::Full) {
                                self.matched_right_keys.insert(key.clone());
                            }

                            if matches.len() == 1 {
                                let right_tuple = matches
                                    .get(0)
                                    .ok_or_else(|| Error::query_execution("Match index out of bounds"))?;
                                return Ok(Some(self.join_probe_with_build(&left_tuple, right_tuple)));
                            }

                            self.current_left_tuple = Some(left_tuple);
                            self.current_match_key = Some(key);
                            self.match_index = 0;

                            // Continue loop to emit first match
                            continue;
                        }

                        let filtered_matches: Vec<Tuple> = matches
                            .iter()
                            .filter(|right_tuple| {
                                self.evaluate_probe_build_condition(&left_tuple, right_tuple)
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

    let left_rows = estimate_hash_join_rows(executor, left);
    let right_rows = estimate_hash_join_rows(executor, right);
    // Diagnostic kill switch for A/B perf runs; default is to build the
    // smaller known side for inner equi-joins.
    let build_left_for_inner = matches!(join_type, crate::sql::JoinType::Inner)
        && std::env::var("HELIOS_HASHJOIN_BUILD_RIGHT").is_err()
        && matches!((left_rows, right_rows), (Some(l), Some(r)) if l < r);

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
                let mut join_op: Box<dyn PhysicalOperator> = if build_left_for_inner {
                    Box::new(HashJoinOperator::new_build_left(
                        left_op,
                        right_op,
                        join_type.clone(),
                        equi_part,
                        timeout_ctx,
                    )?)
                } else {
                    Box::new(HashJoinOperator::new(
                        left_op,
                        right_op,
                        join_type.clone(),
                        equi_part,
                        timeout_ctx,
                    )?)
                };

                // Apply residual filter on top if present
                if let Some(residual) = residual_part {
                    join_op = Box::new(super::filter::FilterOperator::new(join_op, residual, vec![]));
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

    for expr in exprs {
        if !matches!(expr, crate::sql::LogicalExpr::Column { .. }) {
            return Ok(None);
        }
    }

    let left_rows = estimate_hash_join_rows(executor, left);
    let right_rows = estimate_hash_join_rows(executor, right);
    let build_left_for_inner = std::env::var("HELIOS_HASHJOIN_BUILD_RIGHT").is_err()
        && matches!((left_rows, right_rows), (Some(l), Some(r)) if l < r);

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

    let timeout_ctx = executor.timeout_ctx();
    let build_side = if build_left_for_inner {
        HashJoinBuildSide::Left
    } else {
        HashJoinBuildSide::Right
    };

    if let Some(predicate) = post_join_predicate {
        let mut join_op: Box<dyn PhysicalOperator> = if build_left_for_inner {
            Box::new(HashJoinOperator::new_build_left(
                left_op,
                right_op,
                crate::sql::JoinType::Inner,
                equi_part,
                timeout_ctx.clone(),
            )?)
        } else {
            Box::new(HashJoinOperator::new(
                left_op,
                right_op,
                crate::sql::JoinType::Inner,
                equi_part,
                timeout_ctx.clone(),
            )?)
        };
        let materialized_predicate = executor.materialize_subqueries(predicate)?;
        join_op = Box::new(
            super::filter::FilterOperator::new(join_op, materialized_predicate, executor.parameters().to_vec())
                .with_timeout(timeout_ctx.clone()),
        );
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

    let op = HashJoinOperator::new_projected_inner(
        left_op,
        right_op,
        equi_part,
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

        // Fetch each matching right row and combine
        for row_id in matching_row_ids {
            if let Some(right_tuple) = storage.get_row_by_id(&right_table, row_id, &right_schema)? {
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
        crate::sql::LogicalPlan::Project { input, .. } => extract_scan_info(input),
        _ => None,
    }
}

/// Extract column pair from a simple equi-join condition: col1 = col2
/// Returns (table_option, column_name) for each side
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
        // For compound AND conditions, try the first equality
        LogicalExpr::BinaryExpr {
            left,
            op: BinaryOperator::And,
            ..
        } => extract_equi_columns(left),
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
