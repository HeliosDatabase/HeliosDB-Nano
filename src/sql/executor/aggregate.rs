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

        // Collect all input tuples (with timeout checking)
        let mut tuples = Vec::new();
        while let Some(tuple) = input.next()? {
            // Check timeout during materialization (blocking operation)
            if let Some(ref ctx) = timeout_ctx {
                ctx.check_timeout()?;
            }
            tuples.push(tuple);
        }

        // Group tuples by GROUP BY expressions
        use std::collections::BTreeMap;
        use crate::Value;

        let mut groups: BTreeMap<GroupKey, Vec<Tuple>> = BTreeMap::new();

        if group_by.is_empty() {
            // No GROUP BY - single group containing all tuples
            let key = GroupKey(vec![]); // Empty key for single group
            groups.insert(key, tuples);
        } else {
            // Group tuples by GROUP BY expressions
            for tuple in tuples {
                // Evaluate GROUP BY expressions to create group key
                let key: Result<Vec<Value>> = group_by.iter()
                    .map(|expr| evaluator.evaluate(expr, &tuple))
                    .collect();
                let key = GroupKey(key?);

                groups.entry(key).or_insert_with(Vec::new).push(tuple);
            }
        }

        // Compute aggregates for each group (with timeout checking)
        let mut output_tuples = Vec::new();
        for (group_key, group_tuples) in groups {
            // Check timeout during aggregate computation (can be expensive)
            if let Some(ref ctx) = timeout_ctx {
                ctx.check_timeout()?;
            }

            // Evaluate aggregate expressions
            let aggr_values: Result<Vec<Value>> = aggr_exprs.iter()
                .map(|expr| Self::evaluate_aggregate(expr, &group_tuples, &evaluator))
                .collect();
            let mut aggr_values = aggr_values?;

            // Combine group key + aggregate values
            let mut output_values = group_key.0; // Unwrap GroupKey
            output_values.append(&mut aggr_values);

            output_tuples.push(Tuple::new(output_values));
        }

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
            let rewritten_having = Self::rewrite_having_expr(&having_expr, &group_by, &aggr_exprs);

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

                let arg_expr = &args[0];

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
                        values.into_iter()
                            .min_by(|a, b| compare_values(a, b))
                            .ok_or_else(|| Error::query_execution("MIN on empty set"))
                    }
                    AggregateFunction::Max => {
                        values.into_iter()
                            .max_by(|a, b| compare_values(a, b))
                            .ok_or_else(|| Error::query_execution("MAX on empty set"))
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
        group_by: &[crate::sql::LogicalExpr],
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
                    left: Box::new(Self::rewrite_having_expr(left, group_by, aggr_exprs)),
                    op: *op,
                    right: Box::new(Self::rewrite_having_expr(right, group_by, aggr_exprs)),
                }
            }
            LogicalExpr::UnaryExpr { op, expr: inner_expr } => {
                LogicalExpr::UnaryExpr {
                    op: *op,
                    expr: Box::new(Self::rewrite_having_expr(inner_expr, group_by, aggr_exprs)),
                }
            }
            // For other expression types, just clone them
            _ => expr.clone(),
        }
    }
}

impl PhysicalOperator for AggregateOperator {
    fn next(&mut self) -> Result<Option<Tuple>> {
        if self.current_index >= self.output_tuples.len() {
            return Ok(None);
        }

        let tuple = self.output_tuples[self.current_index].clone();
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

        let tuple = self.sorted_tuples[self.current_index].clone();
        self.current_index += 1;
        Ok(Some(tuple))
    }

    fn schema(&self) -> Arc<Schema> {
        self.schema.clone()
    }
}
