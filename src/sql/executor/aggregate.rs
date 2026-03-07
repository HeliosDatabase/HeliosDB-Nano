//! Aggregation and sorting operators
//!
//! This module provides operators for GROUP BY, aggregates (COUNT, SUM, AVG, etc.),
//! and ORDER BY operations.

use crate::{Result, Error, Tuple, Schema};
use super::{PhysicalOperator, TimeoutContext, compare_values};
use std::sync::Arc;

/// Group key for aggregate grouping
#[derive(Debug, Clone, PartialEq)]
struct GroupKey(Vec<crate::Value>);

impl Eq for GroupKey {}

impl PartialOrd for GroupKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for GroupKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        for (a, b) in self.0.iter().zip(&other.0) {
            let cmp = compare_values(a, b);
            if cmp != std::cmp::Ordering::Equal {
                return cmp;
            }
        }
        self.0.len().cmp(&other.0.len())
    }
}

/// Aggregate operator
///
/// Groups tuples and computes aggregate functions.
/// Note: This is a blocking operator that materializes all input.
pub struct AggregateOperator {
    output_tuples: Vec<Tuple>,
    current_index: usize,
    schema: Arc<Schema>,
    timeout_ctx: Option<TimeoutContext>,
}

impl AggregateOperator {
    pub fn new(
        mut input: Box<dyn PhysicalOperator>,
        group_by: Vec<crate::sql::LogicalExpr>,
        aggr_exprs: Vec<crate::sql::LogicalExpr>,
        having: Option<crate::sql::LogicalExpr>,
        parameters: Vec<crate::Value>,
        timeout_ctx: Option<TimeoutContext>,
    ) -> Result<Self> {
        let input_schema = input.schema();
        let evaluator = crate::sql::Evaluator::with_parameters(input_schema.clone(), parameters);

        // Streaming fast path: no GROUP BY and all aggregates are streamable
        let mut output_tuples = if group_by.is_empty() && Self::all_streamable(&aggr_exprs) {
            let result = Self::streaming_aggregate(&mut input, &aggr_exprs, &evaluator, &timeout_ctx)?;
            vec![Tuple::new(result)]
        } else {
            // Standard materialization path
            Self::materialized_aggregate(&mut input, &group_by, &aggr_exprs, &evaluator, &timeout_ctx)?
        };

        // Build output schema: GROUP BY columns + aggregate columns
        use crate::DataType;
        use crate::sql::TypeInference;
        let mut columns = Vec::new();

        // Add GROUP BY columns with type inference
        for (i, expr) in group_by.iter().enumerate() {
            // Infer type from GROUP BY expression, fallback to Text if inference fails
            let data_type = expr.infer_type(&input_schema)
                .unwrap_or(DataType::Text);

            columns.push(crate::Column {
                name: format!("group_{}", i),
                data_type,
                nullable: true,
                primary_key: false,
                source_table: None,
                    source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: crate::ColumnStorageMode::Default,
            });
        }

        // Add aggregate columns with type inference
        for (i, expr) in aggr_exprs.iter().enumerate() {
            // Infer type from aggregate expression, fallback to Int8 if inference fails
            let data_type = expr.infer_type(&input_schema)
                .unwrap_or(DataType::Int8);

            columns.push(crate::Column {
                name: format!("agg_{}", i),
                data_type,
                nullable: true,
                primary_key: false,
                source_table: None,
                    source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: crate::ColumnStorageMode::Default,
            });
        }

        let schema = Arc::new(Schema { columns });

        // Filter output tuples based on HAVING clause if present
        if let Some(having_expr) = having {
            // Rewrite HAVING expression to replace aggregate functions with column references
            let rewritten_having = Self::rewrite_having_expr(&having_expr, &aggr_exprs);

            let having_evaluator = crate::sql::Evaluator::new(schema.clone());
            output_tuples = output_tuples.into_iter()
                .filter(|tuple| {
                    match having_evaluator.evaluate(&rewritten_having, tuple) {
                        Ok(crate::Value::Boolean(true)) => true,
                        _ => false,
                    }
                })
                .collect();
        }

        Ok(Self {
            output_tuples,
            current_index: 0,
            schema,
            timeout_ctx,
        })
    }

    /// Set timeout context (no-op since timeout is set during construction)
    ///
    /// Note: This method exists for API consistency but does nothing since
    /// AggregateOperator materializes data during `new()`, so timeout must
    /// be passed to the constructor.
    pub fn with_timeout(self, _timeout_ctx: Option<TimeoutContext>) -> Self {
        // Timeout already set during construction, ignore this call
        self
    }

    /// Evaluate an aggregate expression over a group of tuples
    fn evaluate_aggregate(
        expr: &crate::sql::LogicalExpr,
        tuples: &[Tuple],
        evaluator: &crate::sql::Evaluator,
    ) -> Result<crate::Value> {
        use crate::sql::{LogicalExpr, AggregateFunction};
        use crate::Value;

        match expr {
            LogicalExpr::AggregateFunction { fun, args, distinct } => {
                // For now, only support single-argument aggregates
                if args.len() != 1 {
                    return Err(Error::query_execution(
                        "Aggregate functions must have exactly one argument"
                    ));
                }

                let arg_expr = args.first().ok_or_else(|| Error::query_execution(
                    "Aggregate function has no arguments"
                ))?;

                // Special case: COUNT(*)
                if matches!(fun, AggregateFunction::Count) && matches!(arg_expr, LogicalExpr::Wildcard) {
                    return Ok(Value::Int8(tuples.len() as i64));
                }

                // Evaluate argument for each tuple
                let values: Result<Vec<Value>> = tuples.iter()
                    .map(|tuple| evaluator.evaluate(arg_expr, tuple))
                    .collect();
                let mut values = values?;

                // Handle DISTINCT
                if *distinct {
                    values.sort_by(|a, b| compare_values(a, b));
                    values.dedup();
                }

                // Filter out NULLs (except for COUNT)
                if !matches!(fun, AggregateFunction::Count) {
                    values.retain(|v| !matches!(v, Value::Null));
                }

                // Compute aggregate
                match fun {
                    AggregateFunction::Count => {
                        // COUNT excludes NULLs
                        let count = values.iter().filter(|v| !matches!(v, Value::Null)).count();
                        Ok(Value::Int8(count as i64))
                    }
                    AggregateFunction::Sum => {
                        // Check if any value is Numeric or floating-point
                        let has_decimal = values.iter().any(|v| matches!(v, Value::Numeric(_) | Value::Float4(_) | Value::Float8(_)));

                        if has_decimal {
                            // Sum as Numeric for precision
                            use rust_decimal::Decimal;
                            let mut sum = Decimal::from(0);
                            for val in values {
                                match val {
                                    Value::Int2(i) => sum += Decimal::from(i),
                                    Value::Int4(i) => sum += Decimal::from(i),
                                    Value::Int8(i) => sum += Decimal::from(i),
                                    Value::Float4(f) => {
                                        if let Ok(dec) = Decimal::try_from(f as f64) {
                                            sum += dec;
                                        }
                                    }
                                    Value::Float8(f) => {
                                        if let Ok(dec) = Decimal::try_from(f) {
                                            sum += dec;
                                        }
                                    }
                                    Value::Numeric(n) => {
                                        if let Ok(dec) = n.parse::<Decimal>() {
                                            sum += dec;
                                        }
                                    }
                                    _ => return Err(Error::query_execution("SUM requires numeric values")),
                                }
                            }
                            Ok(Value::Numeric(format!("{}", sum)))
                        } else {
                            // Sum as Int64 for integer values
                            let mut sum = 0i64;
                            for val in values {
                                match val {
                                    Value::Int2(i) => sum += i as i64,
                                    Value::Int4(i) => sum += i as i64,
                                    Value::Int8(i) => sum += i,
                                    _ => return Err(Error::query_execution("SUM requires numeric values")),
                                }
                            }
                            Ok(Value::Int8(sum))
                        }
                    }
                    AggregateFunction::Avg => {
                        if values.is_empty() {
                            return Ok(Value::Null);
                        }
                        let mut sum = 0.0f64;
                        for val in &values {
                            match val {
                                Value::Int2(i) => sum += *i as f64,
                                Value::Int4(i) => sum += *i as f64,
                                Value::Int8(i) => sum += *i as f64,
                                Value::Float4(f) => sum += *f as f64,
                                Value::Float8(f) => sum += *f,
                                Value::Numeric(n) => {
                                    if let Ok(f) = n.parse::<f64>() {
                                        sum += f;
                                    }
                                }
                                _ => return Err(Error::query_execution("AVG requires numeric values")),
                            }
                        }
                        let avg = sum / values.len() as f64;
                        Ok(Value::Float8(avg))
                    }
                    AggregateFunction::Min => {
                        // SQL standard: MIN on empty set returns NULL
                        Ok(values.into_iter()
                            .min_by(|a, b| compare_values(a, b))
                            .unwrap_or(Value::Null))
                    }
                    AggregateFunction::Max => {
                        // SQL standard: MAX on empty set returns NULL
                        Ok(values.into_iter()
                            .max_by(|a, b| compare_values(a, b))
                            .unwrap_or(Value::Null))
                    }
                    AggregateFunction::JsonAgg => {
                        // JSON_AGG collects all values into a JSON array
                        use serde_json::json;
                        let mut json_values = Vec::new();

                        for val in values {
                            let json_val = match val {
                                Value::Null => json!(null),
                                Value::Boolean(b) => json!(b),
                                Value::Int2(n) => json!(n),
                                Value::Int4(n) => json!(n),
                                Value::Int8(n) => json!(n),
                                Value::Float4(f) => json!(f as f64),
                                Value::Float8(f) => json!(f),
                                Value::String(s) => json!(s),
                                Value::Bytes(b) => json!(hex::encode(&b)),
                                Value::Uuid(u) => json!(u.to_string()),
                                Value::Timestamp(ts) => json!(ts.to_rfc3339()),
                                Value::Json(j) => {
                                    serde_json::from_str(j.as_str()).unwrap_or_else(|_| json!(j))
                                }
                                Value::Array(arr) => {
                                    // Recursively convert array elements
                                    let json_arr: Vec<serde_json::Value> = arr.iter().map(|v| {
                                        match v {
                                            Value::Null => json!(null),
                                            Value::Boolean(b) => json!(b),
                                            Value::Int2(n) => json!(n),
                                            Value::Int4(n) => json!(n),
                                            Value::Int8(n) => json!(n),
                                            Value::Float4(f) => json!(*f as f64),
                                            Value::Float8(f) => json!(f),
                                            Value::String(s) => json!(s),
                                            Value::Bytes(b) => json!(hex::encode(b)),
                                            Value::Uuid(u) => json!(u.to_string()),
                                            Value::Timestamp(ts) => json!(ts.to_rfc3339()),
                                            Value::Json(j) => {
                                                serde_json::from_str(j).unwrap_or_else(|_| json!(j))
                                            }
                                            _ => json!(null),
                                        }
                                    }).collect();
                                    serde_json::Value::Array(json_arr)
                                }
                                _ => json!(null),
                            };
                            json_values.push(json_val);
                        }

                        Ok(Value::Json(json!(json_values).to_string()))
                    }
                    AggregateFunction::ArrayAgg => {
                        // ARRAY_AGG collects all values into an array
                        Ok(Value::Array(values))
                    }
                    AggregateFunction::StringAgg { delimiter } => {
                        // STRING_AGG concatenates string values with delimiter
                        let strings: Vec<String> = values
                            .into_iter()
                            .filter_map(|v| match v {
                                Value::Null => None,
                                Value::String(s) => Some(s),
                                other => Some(other.to_string()),
                            })
                            .collect();
                        Ok(Value::String(strings.join(delimiter)))
                    }
                }
            }
            _ => Err(Error::query_execution("Expected aggregate function expression")),
        }
    }

    /// Rewrite HAVING expression to replace aggregate functions with column references
    /// This allows the HAVING clause to reference already-computed aggregate values
    fn rewrite_having_expr(
        expr: &crate::sql::LogicalExpr,
        aggr_exprs: &[crate::sql::LogicalExpr],
    ) -> crate::sql::LogicalExpr {
        use crate::sql::LogicalExpr;

        match expr {
            LogicalExpr::AggregateFunction { fun, args, distinct } => {
                // Find this aggregate function in aggr_exprs
                for (i, aggr_expr) in aggr_exprs.iter().enumerate() {
                    if let LogicalExpr::AggregateFunction { fun: aggr_fun, args: aggr_args, distinct: aggr_distinct } = aggr_expr {
                        if fun == aggr_fun && args == aggr_args && distinct == aggr_distinct {
                            // Replace with column reference to agg_{i}
                            return LogicalExpr::Column {
                                table: None,
                                name: format!("agg_{}", i),
                            };
                        }
                    }
                }
                // If not found, keep as is (will likely fail evaluation, but that's okay)
                expr.clone()
            }
            LogicalExpr::BinaryExpr { left, op, right } => {
                LogicalExpr::BinaryExpr {
                    left: Box::new(Self::rewrite_having_expr(left, aggr_exprs)),
                    op: *op,
                    right: Box::new(Self::rewrite_having_expr(right, aggr_exprs)),
                }
            }
            LogicalExpr::UnaryExpr { op, expr: inner_expr } => {
                LogicalExpr::UnaryExpr {
                    op: *op,
                    expr: Box::new(Self::rewrite_having_expr(inner_expr, aggr_exprs)),
                }
            }
            // For other expression types, just clone them
            _ => expr.clone(),
        }
    }

    /// Check if all aggregate expressions can be computed in a single streaming pass
    fn all_streamable(aggr_exprs: &[crate::sql::LogicalExpr]) -> bool {
        use crate::sql::{LogicalExpr, AggregateFunction};
        aggr_exprs.iter().all(|expr| {
            matches!(expr,
                LogicalExpr::AggregateFunction { fun, distinct: false, .. }
                if matches!(fun,
                    AggregateFunction::Count | AggregateFunction::Sum |
                    AggregateFunction::Avg | AggregateFunction::Min | AggregateFunction::Max
                )
            )
        })
    }

    /// Compute aggregates in a single streaming pass (no GROUP BY, no DISTINCT).
    /// Avoids materializing all input tuples — O(1) memory instead of O(N).
    fn streaming_aggregate(
        input: &mut Box<dyn PhysicalOperator>,
        aggr_exprs: &[crate::sql::LogicalExpr],
        evaluator: &crate::sql::Evaluator,
        timeout_ctx: &Option<TimeoutContext>,
    ) -> Result<Vec<crate::Value>> {
        use crate::sql::{LogicalExpr, AggregateFunction};
        use crate::Value;

        // Initialize accumulators for each aggregate expression
        let mut accumulators: Vec<StreamingAccumulator> = aggr_exprs.iter().map(|expr| {
            if let LogicalExpr::AggregateFunction { fun, .. } = expr {
                match fun {
                    AggregateFunction::Count => StreamingAccumulator::Count(0),
                    AggregateFunction::Sum => StreamingAccumulator::Sum(SumState::Empty),
                    AggregateFunction::Avg => StreamingAccumulator::Avg { sum: 0.0, count: 0 },
                    AggregateFunction::Min => StreamingAccumulator::Min(None),
                    AggregateFunction::Max => StreamingAccumulator::Max(None),
                    _ => StreamingAccumulator::Count(0), // unreachable due to all_streamable check
                }
            } else {
                StreamingAccumulator::Count(0) // unreachable
            }
        }).collect();

        // Process input tuples one at a time
        while let Some(tuple) = input.next()? {
            if let Some(ref ctx) = timeout_ctx {
                ctx.check_timeout()?;
            }

            // Update each accumulator
            for (i, expr) in aggr_exprs.iter().enumerate() {
                if let LogicalExpr::AggregateFunction { args, .. } = expr {
                    let arg_expr = args.first().ok_or_else(|| Error::query_execution(
                        "Aggregate function has no arguments"
                    ))?;

                    // COUNT(*) doesn't need to evaluate the arg
                    let val = if matches!(arg_expr, LogicalExpr::Wildcard) {
                        Value::Null // sentinel — COUNT(*) counts all rows
                    } else {
                        evaluator.evaluate(arg_expr, &tuple)?
                    };

                    if let Some(acc) = accumulators.get_mut(i) {
                        acc.update(&val, matches!(arg_expr, LogicalExpr::Wildcard))?;
                    }
                }
            }
        }

        // Finalize accumulators into result values
        accumulators.into_iter().map(|acc| acc.finalize()).collect()
    }

    /// Standard materialization path for GROUP BY queries
    fn materialized_aggregate(
        input: &mut Box<dyn PhysicalOperator>,
        group_by: &[crate::sql::LogicalExpr],
        aggr_exprs: &[crate::sql::LogicalExpr],
        evaluator: &crate::sql::Evaluator,
        timeout_ctx: &Option<TimeoutContext>,
    ) -> Result<Vec<Tuple>> {
        use std::collections::BTreeMap;
        use crate::Value;

        // Collect all input tuples
        let mut tuples = Vec::new();
        while let Some(tuple) = input.next()? {
            if let Some(ref ctx) = timeout_ctx {
                ctx.check_timeout()?;
            }
            tuples.push(tuple);
        }

        // Group tuples
        let mut groups: BTreeMap<GroupKey, Vec<Tuple>> = BTreeMap::new();
        if group_by.is_empty() {
            let key = GroupKey(vec![]);
            groups.insert(key, tuples);
        } else {
            for tuple in tuples {
                let key: Result<Vec<Value>> = group_by.iter()
                    .map(|expr| evaluator.evaluate(expr, &tuple))
                    .collect();
                let key = GroupKey(key?);
                groups.entry(key).or_insert_with(Vec::new).push(tuple);
            }
        }

        // Compute aggregates for each group
        let mut output_tuples = Vec::new();
        for (group_key, group_tuples) in groups {
            if let Some(ref ctx) = timeout_ctx {
                ctx.check_timeout()?;
            }
            let aggr_values: Result<Vec<Value>> = aggr_exprs.iter()
                .map(|expr| Self::evaluate_aggregate(expr, &group_tuples, evaluator))
                .collect();
            let mut aggr_values = aggr_values?;
            let mut output_values = group_key.0;
            output_values.append(&mut aggr_values);
            output_tuples.push(Tuple::new(output_values));
        }

        Ok(output_tuples)
    }
}

/// Running sum state that preserves type (integer vs decimal)
enum SumState {
    Empty,
    Int(i64),
    Decimal(rust_decimal::Decimal),
}

/// Streaming accumulator for single-pass aggregation
enum StreamingAccumulator {
    Count(i64),
    Sum(SumState),
    Avg { sum: f64, count: u64 },
    Min(Option<crate::Value>),
    Max(Option<crate::Value>),
}

impl StreamingAccumulator {
    fn update(&mut self, val: &crate::Value, is_wildcard: bool) -> Result<()> {
        use crate::Value;
        match self {
            Self::Count(ref mut c) => {
                // COUNT(*) counts all rows; COUNT(expr) skips NULLs
                if is_wildcard || !matches!(val, Value::Null) {
                    *c += 1;
                }
            }
            Self::Sum(ref mut state) => {
                if matches!(val, Value::Null) { return Ok(()); }
                match val {
                    Value::Int2(i) => match state {
                        SumState::Empty => *state = SumState::Int(*i as i64),
                        SumState::Int(s) => *s += *i as i64,
                        SumState::Decimal(s) => *s += rust_decimal::Decimal::from(*i),
                    },
                    Value::Int4(i) => match state {
                        SumState::Empty => *state = SumState::Int(*i as i64),
                        SumState::Int(s) => *s += *i as i64,
                        SumState::Decimal(s) => *s += rust_decimal::Decimal::from(*i),
                    },
                    Value::Int8(i) => match state {
                        SumState::Empty => *state = SumState::Int(*i),
                        SumState::Int(s) => *s += *i,
                        SumState::Decimal(s) => *s += rust_decimal::Decimal::from(*i),
                    },
                    Value::Float4(f) => {
                        let dec = rust_decimal::Decimal::try_from(*f as f64).unwrap_or_default();
                        match state {
                            SumState::Empty => *state = SumState::Decimal(dec),
                            SumState::Int(s) => *state = SumState::Decimal(rust_decimal::Decimal::from(*s) + dec),
                            SumState::Decimal(s) => *s += dec,
                        }
                    }
                    Value::Float8(f) => {
                        let dec = rust_decimal::Decimal::try_from(*f).unwrap_or_default();
                        match state {
                            SumState::Empty => *state = SumState::Decimal(dec),
                            SumState::Int(s) => *state = SumState::Decimal(rust_decimal::Decimal::from(*s) + dec),
                            SumState::Decimal(s) => *s += dec,
                        }
                    }
                    Value::Numeric(n) => {
                        let dec = n.parse::<rust_decimal::Decimal>().unwrap_or_default();
                        match state {
                            SumState::Empty => *state = SumState::Decimal(dec),
                            SumState::Int(s) => *state = SumState::Decimal(rust_decimal::Decimal::from(*s) + dec),
                            SumState::Decimal(s) => *s += dec,
                        }
                    }
                    _ => return Err(Error::query_execution("SUM requires numeric values")),
                }
            }
            Self::Avg { ref mut sum, ref mut count } => {
                if matches!(val, Value::Null) { return Ok(()); }
                match val {
                    Value::Int2(i) => { *sum += *i as f64; *count += 1; }
                    Value::Int4(i) => { *sum += *i as f64; *count += 1; }
                    Value::Int8(i) => { *sum += *i as f64; *count += 1; }
                    Value::Float4(f) => { *sum += *f as f64; *count += 1; }
                    Value::Float8(f) => { *sum += *f; *count += 1; }
                    Value::Numeric(n) => {
                        if let Ok(f) = n.parse::<f64>() { *sum += f; *count += 1; }
                    }
                    _ => return Err(Error::query_execution("AVG requires numeric values")),
                }
            }
            Self::Min(ref mut current) => {
                if matches!(val, Value::Null) { return Ok(()); }
                match current {
                    None => *current = Some(val.clone()),
                    Some(c) => {
                        if compare_values(val, c) == std::cmp::Ordering::Less {
                            *current = Some(val.clone());
                        }
                    }
                }
            }
            Self::Max(ref mut current) => {
                if matches!(val, Value::Null) { return Ok(()); }
                match current {
                    None => *current = Some(val.clone()),
                    Some(c) => {
                        if compare_values(val, c) == std::cmp::Ordering::Greater {
                            *current = Some(val.clone());
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn finalize(self) -> Result<crate::Value> {
        use crate::Value;
        match self {
            Self::Count(c) => Ok(Value::Int8(c)),
            Self::Sum(state) => match state {
                SumState::Empty => Ok(Value::Null),
                SumState::Int(s) => Ok(Value::Int8(s)),
                SumState::Decimal(s) => Ok(Value::Numeric(format!("{s}"))),
            },
            Self::Avg { sum, count } => {
                if count == 0 { Ok(Value::Null) } else { Ok(Value::Float8(sum / count as f64)) }
            }
            // SQL standard: MIN/MAX on empty set returns NULL
            Self::Min(v) => Ok(v.unwrap_or(Value::Null)),
            Self::Max(v) => Ok(v.unwrap_or(Value::Null)),
        }
    }
}

impl PhysicalOperator for AggregateOperator {
    fn next(&mut self) -> Result<Option<Tuple>> {
        if self.current_index >= self.output_tuples.len() {
            return Ok(None);
        }

        let tuple = self.output_tuples.get(self.current_index).cloned()
            .ok_or_else(|| Error::query_execution("Aggregate index out of bounds"))?;
        self.current_index += 1;
        Ok(Some(tuple))
    }

    fn schema(&self) -> Arc<Schema> {
        self.schema.clone()
    }
}

/// Sort operator
///
/// Sorts tuples based on sort expressions.
/// Note: This is a blocking operator that materializes all input.
pub struct SortOperator {
    sorted_tuples: Vec<Tuple>,
    current_index: usize,
    schema: Arc<Schema>,
    timeout_ctx: Option<TimeoutContext>,
}

impl SortOperator {
    pub fn new(
        mut input: Box<dyn PhysicalOperator>,
        exprs: Vec<crate::sql::LogicalExpr>,
        asc: Vec<bool>,
        timeout_ctx: Option<TimeoutContext>,
    ) -> Result<Self> {
        let schema = input.schema();
        let evaluator = crate::sql::Evaluator::new(schema.clone());

        // Collect all tuples from input (with timeout checking)
        let mut tuples = Vec::new();
        while let Some(tuple) = input.next()? {
            // Check timeout during materialization (blocking operation)
            if let Some(ref ctx) = timeout_ctx {
                ctx.check_timeout()?;
            }
            tuples.push(tuple);
        }

        // Sort tuples
        tuples.sort_by(|a, b| {
            for (i, expr) in exprs.iter().enumerate() {
                // Evaluate expression for both tuples
                let val_a = evaluator.evaluate(expr, a);
                let val_b = evaluator.evaluate(expr, b);

                // Handle evaluation errors
                let (val_a, val_b) = match (val_a, val_b) {
                    (Ok(a), Ok(b)) => (a, b),
                    _ => continue, // Skip comparison on error
                };

                // Compare values
                use std::cmp::Ordering;
                let cmp = compare_values(&val_a, &val_b);

                // Apply ascending/descending
                let cmp = if asc.get(i).copied().unwrap_or(true) {
                    cmp
                } else {
                    cmp.reverse()
                };

                if cmp != Ordering::Equal {
                    return cmp;
                }
            }
            std::cmp::Ordering::Equal
        });

        Ok(Self {
            sorted_tuples: tuples,
            current_index: 0,
            schema,
            timeout_ctx,
        })
    }

    /// Set timeout context (no-op since timeout is set during construction)
    ///
    /// Note: This method exists for API consistency but does nothing since
    /// SortOperator materializes data during `new()`, so timeout must
    /// be passed to the constructor.
    pub fn with_timeout(self, _timeout_ctx: Option<TimeoutContext>) -> Self {
        // Timeout already set during construction, ignore this call
        self
    }
}

impl PhysicalOperator for SortOperator {
    fn next(&mut self) -> Result<Option<Tuple>> {
        if self.current_index >= self.sorted_tuples.len() {
            return Ok(None);
        }

        let tuple = self.sorted_tuples.get(self.current_index).cloned()
            .ok_or_else(|| Error::query_execution("Sort index out of bounds"))?;
        self.current_index += 1;
        Ok(Some(tuple))
    }

    fn schema(&self) -> Arc<Schema> {
        self.schema.clone()
    }
}
