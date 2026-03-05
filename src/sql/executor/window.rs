//! Window function operator
//!
//! Implements window functions like ROW_NUMBER, RANK, DENSE_RANK, LAG, LEAD, etc.
//! Window functions operate over a set of rows (a "window" or "partition") and
//! return a value for each row based on its position within that partition.

use crate::{Result, Tuple, Schema, Value};
use crate::sql::logical_plan::{LogicalExpr, WindowFunctionType, WindowFrame, WindowFrameBound};
use crate::sql::Evaluator;
use super::PhysicalOperator;
use std::sync::Arc;
use std::collections::HashMap;

/// Window function operator
///
/// This operator:
/// 1. Collects all input rows
/// 2. Groups them by PARTITION BY columns
/// 3. Sorts each partition by ORDER BY columns
/// 4. Computes window function values for each row
/// 5. Returns rows with window function values appended
pub struct WindowOperator {
    /// Input operator
    input: Box<dyn PhysicalOperator>,
    /// Window function expressions to compute
    window_exprs: Vec<WindowExprInfo>,
    /// Output schema (input schema + window columns)
    schema: Arc<Schema>,
    /// Evaluator for expressions
    evaluator: Evaluator,
    /// Materialized results
    results: Vec<Tuple>,
    /// Current position
    current_index: usize,
    /// Whether we've processed input
    processed: bool,
}

/// Information about a window expression
#[derive(Clone)]
struct WindowExprInfo {
    /// Window function type
    fun: WindowFunctionType,
    /// Arguments to the function
    args: Vec<LogicalExpr>,
    /// PARTITION BY columns
    partition_by: Vec<LogicalExpr>,
    /// ORDER BY expressions and directions
    order_by: Vec<(LogicalExpr, bool)>,
    /// Window frame
    frame: Option<WindowFrame>,
    /// Output column name
    output_name: String,
}

impl WindowOperator {
    /// Create a new window operator
    pub fn new(
        input: Box<dyn PhysicalOperator>,
        window_exprs: Vec<(LogicalExpr, String)>,
        schema: Arc<Schema>,
    ) -> Self {
        let input_schema = input.schema();
        let evaluator = Evaluator::new(input_schema);

        let window_infos: Vec<WindowExprInfo> = window_exprs
            .into_iter()
            .map(|(expr, name)| {
                if let LogicalExpr::WindowFunction { fun, args, partition_by, order_by, frame } = expr {
                    WindowExprInfo {
                        fun,
                        args,
                        partition_by,
                        order_by,
                        frame,
                        output_name: name,
                    }
                } else {
                    // Non-window expression - shouldn't happen
                    WindowExprInfo {
                        fun: WindowFunctionType::RowNumber,
                        args: vec![],
                        partition_by: vec![],
                        order_by: vec![],
                        frame: None,
                        output_name: name,
                    }
                }
            })
            .collect();

        Self {
            input,
            window_exprs: window_infos,
            schema,
            evaluator,
            results: Vec::new(),
            current_index: 0,
            processed: false,
        }
    }

    /// Process all input and compute window functions
    #[allow(clippy::indexing_slicing)]
    // SAFETY: All indexing uses original_idx/expr_idx from enumeration, bounded by vec lengths
    fn process_input(&mut self) -> Result<()> {
        // Collect all input tuples
        let mut all_tuples: Vec<(usize, Tuple)> = Vec::new();
        let mut idx = 0;
        while let Some(tuple) = self.input.next()? {
            all_tuples.push((idx, tuple));
            idx += 1;
        }

        if all_tuples.is_empty() {
            return Ok(());
        }

        // For each window expression, compute values
        let mut window_values: Vec<Vec<Value>> = vec![vec![Value::Null; all_tuples.len()]; self.window_exprs.len()];

        for (expr_idx, window_expr) in self.window_exprs.iter().enumerate() {
            // Partition the rows
            let partitions = self.partition_rows(&all_tuples, &window_expr.partition_by)?;

            // Process each partition
            for partition in partitions {
                // Sort within partition
                let sorted_partition = self.sort_partition(partition, &window_expr.order_by)?;

                // Compute window function values
                let values = self.compute_window_function(
                    &sorted_partition,
                    &window_expr.fun,
                    &window_expr.args,
                    &window_expr.order_by,
                    &window_expr.frame,
                    !window_expr.order_by.is_empty(),
                )?;

                // Map values back to original positions
                for (i, (original_idx, _)) in sorted_partition.iter().enumerate() {
                    if i < values.len() {
                        window_values[expr_idx][*original_idx] = values[i].clone();
                    }
                }
            }
        }

        // Build result tuples (original values + window function values)
        for (i, (_, tuple)) in all_tuples.into_iter().enumerate() {
            let mut values = tuple.values;
            for expr_values in &window_values {
                values.push(expr_values[i].clone());
            }
            self.results.push(Tuple::new(values));
        }

        Ok(())
    }

    /// Partition rows by PARTITION BY columns
    fn partition_rows(
        &self,
        rows: &[(usize, Tuple)],
        partition_by: &[LogicalExpr],
    ) -> Result<Vec<Vec<(usize, Tuple)>>> {
        if partition_by.is_empty() {
            // No partitioning - all rows in one partition
            return Ok(vec![rows.to_vec()]);
        }

        let mut partitions: HashMap<Vec<Value>, Vec<(usize, Tuple)>> = HashMap::new();

        for (idx, tuple) in rows {
            let key: Vec<Value> = partition_by
                .iter()
                .map(|expr| self.evaluator.evaluate(expr, tuple).unwrap_or(Value::Null))
                .collect();

            partitions
                .entry(key)
                .or_insert_with(Vec::new)
                .push((*idx, tuple.clone()));
        }

        Ok(partitions.into_values().collect())
    }

    /// Sort a partition by ORDER BY columns
    fn sort_partition(
        &self,
        mut partition: Vec<(usize, Tuple)>,
        order_by: &[(LogicalExpr, bool)],
    ) -> Result<Vec<(usize, Tuple)>> {
        if order_by.is_empty() {
            return Ok(partition);
        }

        partition.sort_by(|(_, a), (_, b)| {
            for (expr, ascending) in order_by {
                let a_val = self.evaluator.evaluate(expr, a).unwrap_or(Value::Null);
                let b_val = self.evaluator.evaluate(expr, b).unwrap_or(Value::Null);

                let cmp = compare_values(&a_val, &b_val);
                if cmp != std::cmp::Ordering::Equal {
                    return if *ascending { cmp } else { cmp.reverse() };
                }
            }
            std::cmp::Ordering::Equal
        });

        Ok(partition)
    }

    /// Compute window function values for a partition
    #[allow(clippy::indexing_slicing)]
    // SAFETY: All partition indexing is bounded by `len`, `offset`, and frame calculations
    fn compute_window_function(
        &self,
        partition: &[(usize, Tuple)],
        fun: &WindowFunctionType,
        args: &[LogicalExpr],
        order_by: &[(LogicalExpr, bool)],
        frame: &Option<WindowFrame>,
        has_order_by: bool,
    ) -> Result<Vec<Value>> {
        let len = partition.len();

        match fun {
            WindowFunctionType::RowNumber => {
                // ROW_NUMBER: sequential row number within partition
                Ok((1..=len).map(|i| Value::Int8(i as i64)).collect())
            }

            WindowFunctionType::Rank => {
                // RANK: rank with gaps for ties
                self.compute_rank(partition, order_by, true)
            }

            WindowFunctionType::DenseRank => {
                // DENSE_RANK: rank without gaps for ties
                self.compute_rank(partition, order_by, false)
            }

            WindowFunctionType::PercentRank => {
                // PERCENT_RANK: (rank - 1) / (total_rows - 1)
                let ranks = self.compute_rank(partition, order_by, true)?;
                if len <= 1 {
                    return Ok(vec![Value::Float8(0.0); len]);
                }
                Ok(ranks
                    .iter()
                    .map(|r| {
                        if let Value::Int8(rank) = r {
                            Value::Float8((*rank - 1) as f64 / (len - 1) as f64)
                        } else {
                            Value::Float8(0.0)
                        }
                    })
                    .collect())
            }

            WindowFunctionType::CumeDist => {
                // CUME_DIST: count of rows <= current row / total rows
                let mut result = Vec::with_capacity(len);
                for i in 0..len {
                    // Count rows with same or lower order key values
                    let current_keys: Vec<Value> = partition
                        .get(i)
                        .map(|(_, t)| t)
                        .map(|t| {
                            args.iter()
                                .map(|e| self.evaluator.evaluate(e, t).unwrap_or(Value::Null))
                                .collect()
                        })
                        .unwrap_or_default();

                    let count = partition
                        .iter()
                        .filter(|(_, t)| {
                            let keys: Vec<Value> = args
                                .iter()
                                .map(|e| self.evaluator.evaluate(e, t).unwrap_or(Value::Null))
                                .collect();
                            // Lexicographic comparison of keys
                            compare_value_vecs(&keys, &current_keys) != std::cmp::Ordering::Greater
                        })
                        .count();

                    result.push(Value::Float8(count as f64 / len as f64));
                }
                Ok(result)
            }

            WindowFunctionType::Ntile => {
                // NTILE(n): divide into n buckets
                let n = args
                    .first()
                    .and_then(|e| {
                        if let LogicalExpr::Literal(Value::Int4(v)) = e {
                            Some(*v as usize)
                        } else if let LogicalExpr::Literal(Value::Int8(v)) = e {
                            Some(*v as usize)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(1)
                    .max(1);

                let bucket_size = (len + n - 1) / n; // Ceiling division
                Ok((0..len)
                    .map(|i| Value::Int8((i / bucket_size + 1) as i64))
                    .collect())
            }

            WindowFunctionType::Lag => {
                // LAG(expr, offset, default): value from previous row
                let offset = args
                    .get(1)
                    .and_then(|e| {
                        if let LogicalExpr::Literal(Value::Int4(v)) = e {
                            Some(*v as usize)
                        } else if let LogicalExpr::Literal(Value::Int8(v)) = e {
                            Some(*v as usize)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(1);

                let default = args
                    .get(2)
                    .and_then(|e| {
                        if let LogicalExpr::Literal(v) = e {
                            Some(v.clone())
                        } else {
                            None
                        }
                    })
                    .unwrap_or(Value::Null);

                let expr = args.first();
                Ok((0..len)
                    .map(|i| {
                        if i >= offset {
                            expr.map(|e| {
                                self.evaluator
                                    .evaluate(e, &partition[i - offset].1)
                                    .unwrap_or(default.clone())
                            })
                            .unwrap_or(default.clone())
                        } else {
                            default.clone()
                        }
                    })
                    .collect())
            }

            WindowFunctionType::Lead => {
                // LEAD(expr, offset, default): value from next row
                let offset = args
                    .get(1)
                    .and_then(|e| {
                        if let LogicalExpr::Literal(Value::Int4(v)) = e {
                            Some(*v as usize)
                        } else if let LogicalExpr::Literal(Value::Int8(v)) = e {
                            Some(*v as usize)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(1);

                let default = args
                    .get(2)
                    .and_then(|e| {
                        if let LogicalExpr::Literal(v) = e {
                            Some(v.clone())
                        } else {
                            None
                        }
                    })
                    .unwrap_or(Value::Null);

                let expr = args.first();
                Ok((0..len)
                    .map(|i| {
                        if i + offset < len {
                            expr.map(|e| {
                                self.evaluator
                                    .evaluate(e, &partition[i + offset].1)
                                    .unwrap_or(default.clone())
                            })
                            .unwrap_or(default.clone())
                        } else {
                            default.clone()
                        }
                    })
                    .collect())
            }

            WindowFunctionType::FirstValue => {
                // FIRST_VALUE: first value in window frame
                let expr = args.first();
                let first_val = expr
                    .map(|e| self.evaluator.evaluate(e, &partition[0].1).unwrap_or(Value::Null))
                    .unwrap_or(Value::Null);
                Ok(vec![first_val; len])
            }

            WindowFunctionType::LastValue => {
                // LAST_VALUE: last value in window frame
                // With default frame (RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW),
                // last value is the current row's value
                let expr = args.first();
                Ok((0..len)
                    .map(|i| {
                        let frame_end = self.get_frame_end(i, len, frame, has_order_by);
                        expr.map(|e| {
                            self.evaluator
                                .evaluate(e, &partition[frame_end].1)
                                .unwrap_or(Value::Null)
                        })
                        .unwrap_or(Value::Null)
                    })
                    .collect())
            }

            WindowFunctionType::NthValue => {
                // NTH_VALUE(expr, n): nth value in window frame
                let n = args
                    .get(1)
                    .and_then(|e| {
                        if let LogicalExpr::Literal(Value::Int4(v)) = e {
                            Some(*v as usize)
                        } else if let LogicalExpr::Literal(Value::Int8(v)) = e {
                            Some(*v as usize)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(1);

                let expr = args.first();
                Ok((0..len)
                    .map(|i| {
                        let frame_start = self.get_frame_start(i, len, frame);
                        let frame_end = self.get_frame_end(i, len, frame, has_order_by);
                        let target_idx = frame_start + n - 1;
                        if target_idx <= frame_end && target_idx < len {
                            expr.map(|e| {
                                self.evaluator
                                    .evaluate(e, &partition[target_idx].1)
                                    .unwrap_or(Value::Null)
                            })
                            .unwrap_or(Value::Null)
                        } else {
                            Value::Null
                        }
                    })
                    .collect())
            }

            WindowFunctionType::Aggregate(aggr) => {
                // Aggregate function used as window function
                self.compute_window_aggregate(partition, aggr, args, frame, has_order_by)
            }
        }
    }

    /// Compute RANK or DENSE_RANK
    #[allow(clippy::indexing_slicing)]
    // SAFETY: Loop index `i` ranges from 0..len; `i-1` only accessed when `i > 0`
    fn compute_rank(&self, partition: &[(usize, Tuple)], order_by: &[(LogicalExpr, bool)], with_gaps: bool) -> Result<Vec<Value>> {
        let len = partition.len();
        if len == 0 {
            return Ok(vec![]);
        }

        let mut ranks = Vec::with_capacity(len);
        let mut current_rank = 1i64;
        let mut same_rank_count = 0usize;

        for i in 0..len {
            if i > 0 {
                // Compare only ORDER BY expression values, not full tuple
                let prev_vals: Vec<Value> = order_by.iter()
                    .map(|(expr, _)| self.evaluator.evaluate(expr, &partition[i - 1].1).unwrap_or(Value::Null))
                    .collect();
                let curr_vals: Vec<Value> = order_by.iter()
                    .map(|(expr, _)| self.evaluator.evaluate(expr, &partition[i].1).unwrap_or(Value::Null))
                    .collect();

                if prev_vals != curr_vals {
                    if with_gaps {
                        current_rank += same_rank_count as i64;
                    } else {
                        current_rank += 1;
                    }
                    same_rank_count = 0;
                }
            }

            ranks.push(Value::Int8(current_rank));
            same_rank_count += 1;
        }

        Ok(ranks)
    }

    /// Compute window aggregate function
    #[allow(clippy::indexing_slicing)]
    // SAFETY: Frame indices are bounded by `j < len` filter and frame start/end calculations
    fn compute_window_aggregate(
        &self,
        partition: &[(usize, Tuple)],
        aggr: &crate::sql::AggregateFunction,
        args: &[LogicalExpr],
        frame: &Option<WindowFrame>,
        has_order_by: bool,
    ) -> Result<Vec<Value>> {
        let len = partition.len();
        let expr = args.first();

        Ok((0..len)
            .map(|i| {
                let frame_start = self.get_frame_start(i, len, frame);
                let frame_end = self.get_frame_end(i, len, frame, has_order_by);

                // Collect values in frame
                // For COUNT(*), expr is None — count all rows in the frame
                let values: Vec<Value> = if let Some(e) = expr {
                    (frame_start..=frame_end)
                        .filter(|&j| j < len)
                        .map(|j| self.evaluator.evaluate(e, &partition[j].1).unwrap_or(Value::Null))
                        .collect()
                } else {
                    // No expression (COUNT(*)) — placeholder per row in frame
                    (frame_start..=frame_end)
                        .filter(|&j| j < len)
                        .map(|_| Value::Int8(1))
                        .collect()
                };

                match aggr {
                    crate::sql::AggregateFunction::Count => {
                        // COUNT(col) excludes NULLs; COUNT(*) placeholders are never NULL
                        let non_null = values.iter().filter(|v| !matches!(v, Value::Null)).count();
                        Value::Int8(non_null as i64)
                    }
                    crate::sql::AggregateFunction::Sum => {
                        let sum: f64 = values
                            .iter()
                            .filter_map(|v| value_to_f64(v))
                            .sum();
                        Value::Float8(sum)
                    }
                    crate::sql::AggregateFunction::Avg => {
                        let nums: Vec<f64> = values.iter().filter_map(|v| value_to_f64(v)).collect();
                        if nums.is_empty() {
                            Value::Null
                        } else {
                            Value::Float8(nums.iter().sum::<f64>() / nums.len() as f64)
                        }
                    }
                    crate::sql::AggregateFunction::Min => {
                        values
                            .into_iter()
                            .filter(|v| !matches!(v, Value::Null))
                            .min_by(|a, b| compare_values(a, b))
                            .unwrap_or(Value::Null)
                    }
                    crate::sql::AggregateFunction::Max => {
                        values
                            .into_iter()
                            .filter(|v| !matches!(v, Value::Null))
                            .max_by(|a, b| compare_values(a, b))
                            .unwrap_or(Value::Null)
                    }
                    crate::sql::AggregateFunction::JsonAgg => {
                        // JSON aggregation - return array of values
                        Value::Array(values.clone())
                    }
                    crate::sql::AggregateFunction::ArrayAgg => {
                        // ARRAY_AGG - return array of values
                        Value::Array(values)
                    }
                    crate::sql::AggregateFunction::StringAgg { delimiter } => {
                        // STRING_AGG - concatenate strings
                        let strings: Vec<String> = values
                            .into_iter()
                            .filter_map(|v| match v {
                                Value::Null => None,
                                Value::String(s) => Some(s),
                                other => Some(other.to_string()),
                            })
                            .collect();
                        Value::String(strings.join(delimiter))
                    }
                }
            })
            .collect())
    }

    /// Get frame start position
    fn get_frame_start(&self, current: usize, partition_size: usize, frame: &Option<WindowFrame>) -> usize {
        let frame = match frame {
            Some(f) => f,
            None => return 0, // Default: UNBOUNDED PRECEDING
        };

        match &frame.start {
            WindowFrameBound::UnboundedPreceding => 0,
            WindowFrameBound::Preceding(n) => current.saturating_sub(*n as usize),
            WindowFrameBound::CurrentRow => current,
            WindowFrameBound::Following(n) => (current + *n as usize).min(partition_size - 1),
            WindowFrameBound::UnboundedFollowing => partition_size - 1,
        }
    }

    /// Get frame end position
    fn get_frame_end(&self, current: usize, partition_size: usize, frame: &Option<WindowFrame>, has_order_by: bool) -> usize {
        let frame = match frame {
            Some(f) => f,
            // SQL standard defaults:
            // - With ORDER BY and no frame: RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
            // - Without ORDER BY and no frame: RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING
            None => {
                if has_order_by {
                    return current; // CURRENT ROW
                } else {
                    return partition_size.saturating_sub(1); // UNBOUNDED FOLLOWING
                }
            }
        };

        match frame.end.as_ref().unwrap_or(&WindowFrameBound::CurrentRow) {
            WindowFrameBound::UnboundedPreceding => 0,
            WindowFrameBound::Preceding(n) => current.saturating_sub(*n as usize),
            WindowFrameBound::CurrentRow => current,
            WindowFrameBound::Following(n) => (current + *n as usize).min(partition_size - 1),
            WindowFrameBound::UnboundedFollowing => partition_size - 1,
        }
    }
}

impl PhysicalOperator for WindowOperator {
    fn next(&mut self) -> Result<Option<Tuple>> {
        // Process input on first call
        if !self.processed {
            self.process_input()?;
            self.processed = true;
        }

        // Return next result
        if let Some(tuple) = self.results.get(self.current_index) {
            let tuple = tuple.clone();
            self.current_index += 1;
            Ok(Some(tuple))
        } else {
            Ok(None)
        }
    }

    fn schema(&self) -> Arc<Schema> {
        self.schema.clone()
    }
}

/// Compare two vectors of values lexicographically
fn compare_value_vecs(a: &[Value], b: &[Value]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for (av, bv) in a.iter().zip(b.iter()) {
        let cmp = compare_values(av, bv);
        if cmp != Ordering::Equal {
            return cmp;
        }
    }
    a.len().cmp(&b.len())
}

/// Compare two values for ordering
fn compare_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::Int2(a), Value::Int2(b)) => a.cmp(b),
        (Value::Int4(a), Value::Int4(b)) => a.cmp(b),
        (Value::Int8(a), Value::Int8(b)) => a.cmp(b),
        (Value::Float4(a), Value::Float4(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
        (Value::Float8(a), Value::Float8(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::Boolean(a), Value::Boolean(b)) => a.cmp(b),
        // Cross-type numeric comparison
        _ => {
            if let (Some(a), Some(b)) = (value_to_f64(a), value_to_f64(b)) {
                a.partial_cmp(&b).unwrap_or(Ordering::Equal)
            } else {
                Ordering::Equal
            }
        }
    }
}

/// Convert a value to f64 for numeric operations
fn value_to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int2(n) => Some(*n as f64),
        Value::Int4(n) => Some(*n as f64),
        Value::Int8(n) => Some(*n as f64),
        Value::Float4(n) => Some(*n as f64),
        Value::Float8(n) => Some(*n),
        Value::Numeric(d) => d.parse::<f64>().ok(),
        _ => None,
    }
}
