//! Projection and limit operators
//!
//! This module provides operators for SELECT column projection and LIMIT/OFFSET.

use super::{PhysicalOperator, TimeoutContext};
use crate::{Result, Schema, Tuple};
use std::sync::Arc;

/// Project operator
///
/// Evaluates expressions to produce output columns.
pub struct ProjectOperator {
    input: Box<dyn PhysicalOperator>,
    exprs: Vec<crate::sql::LogicalExpr>,
    aliases: Vec<String>,
    output_schema: Arc<Schema>,
    evaluator: crate::sql::Evaluator,
    direct_column_indices: Option<Vec<usize>>,
    direct_move_max_index: Option<usize>,
    distinct: bool,
    seen: std::collections::HashSet<Vec<u8>>,
    timeout_ctx: Option<TimeoutContext>,
    /// DISTINCT ON expressions (PostgreSQL extension)
    distinct_on_exprs: Option<Vec<crate::sql::LogicalExpr>>,
}

impl ProjectOperator {
    pub fn new(
        input: Box<dyn PhysicalOperator>,
        exprs: Vec<crate::sql::LogicalExpr>,
        aliases: Vec<String>,
        distinct: bool,
        parameters: Vec<crate::Value>,
    ) -> Self {
        Self::new_with_distinct_on(input, exprs, aliases, distinct, None, parameters)
    }

    pub fn new_with_distinct_on(
        input: Box<dyn PhysicalOperator>,
        exprs: Vec<crate::sql::LogicalExpr>,
        aliases: Vec<String>,
        distinct: bool,
        distinct_on: Option<Vec<crate::sql::LogicalExpr>>,
        parameters: Vec<crate::Value>,
    ) -> Self {
        // Get input schema for type inference
        let input_schema = input.schema();

        // Build output schema with type inference
        use crate::sql::TypeInference;
        let columns = aliases
            .iter()
            .zip(exprs.iter())
            .map(|(alias, expr)| {
                // Infer type from expression, fallback to Text if inference fails
                let data_type = expr.infer_type(&input_schema).unwrap_or(crate::DataType::Text);

                crate::Column {
                    name: alias.clone(),
                    data_type,
                    nullable: true,
                    primary_key: false,
                    source_table: None,
                    source_table_name: None,
                    default_expr: None,
                    unique: false,
                    storage_mode: crate::ColumnStorageMode::Default,
                }
            })
            .collect();
        let output_schema = Arc::new(Schema { columns });

        let direct_column_indices = direct_project_column_indices(&input_schema, &exprs);
        let direct_move_max_index = direct_column_indices
            .as_deref()
            .and_then(|indices| projection_move_max_index(indices, input_schema.columns.len()));

        // Create evaluator with input schema and parameters
        let evaluator = crate::sql::Evaluator::with_parameters(input_schema, parameters);

        Self {
            input,
            exprs,
            aliases,
            output_schema,
            evaluator,
            direct_column_indices,
            direct_move_max_index,
            distinct,
            seen: std::collections::HashSet::new(),
            timeout_ctx: None,
            distinct_on_exprs: distinct_on,
        }
    }

    /// Set timeout context for query execution
    pub fn with_timeout(mut self, timeout_ctx: Option<TimeoutContext>) -> Self {
        self.timeout_ctx = timeout_ctx;
        self
    }
}

impl PhysicalOperator for ProjectOperator {
    fn next(&mut self) -> Result<Option<Tuple>> {
        loop {
            match self.input.next()? {
                None => return Ok(None),
                Some(mut tuple) => {
                    // Evaluate each expression to produce output values
                    let output_values: Result<Vec<crate::Value>> = if let Some(indices) = &self.direct_column_indices {
                        if self.distinct_on_exprs.is_none()
                            && self
                                .direct_move_max_index
                                .is_some_and(|max_idx| max_idx < tuple.values.len())
                        {
                            let mut values = Vec::with_capacity(indices.len());
                            for &idx in indices {
                                values.push(std::mem::replace(&mut tuple.values[idx], crate::Value::Null));
                            }
                            Ok(values)
                        } else {
                            indices
                                .iter()
                                .map(|&idx| {
                                    tuple.get(idx).cloned().ok_or_else(|| {
                                        crate::Error::query_execution(format!(
                                            "Column index {} out of bounds in tuple",
                                            idx
                                        ))
                                    })
                                })
                                .collect()
                        }
                    } else {
                        self.exprs
                            .iter()
                            .map(|expr| self.evaluator.evaluate(expr, &tuple))
                            .collect()
                    };

                    let mut output_tuple = Tuple::new(output_values?);
                    // Preserve row_id through projection for DML operations
                    output_tuple.row_id = tuple.row_id;

                    // Handle DISTINCT ON (PostgreSQL extension)
                    // Only return the first row for each unique combination of DISTINCT ON expressions
                    if let Some(ref distinct_on_exprs) = self.distinct_on_exprs {
                        let key_values: Result<Vec<crate::Value>> = distinct_on_exprs
                            .iter()
                            .map(|expr| self.evaluator.evaluate(expr, &tuple))
                            .collect();
                        let key = bincode::serialize(&key_values?).map_err(|e| {
                            crate::Error::query_execution(format!("Failed to serialize DISTINCT ON key: {}", e))
                        })?;

                        if self.seen.contains(&key) {
                            // Skip rows with duplicate DISTINCT ON values
                            continue;
                        }
                        self.seen.insert(key);
                    } else if self.distinct {
                        // Regular DISTINCT - check for duplicates using full tuple values
                        // Serialize only the values for deduplication (not row_id or branch_id)
                        let serialized = bincode::serialize(&output_tuple.values).map_err(|e| {
                            crate::Error::query_execution(format!("Failed to serialize tuple for DISTINCT: {}", e))
                        })?;

                        // Check if we've seen this tuple before
                        if self.seen.contains(&serialized) {
                            // Skip duplicate, continue to next tuple
                            continue;
                        }

                        // Add to seen set
                        self.seen.insert(serialized);
                    }

                    return Ok(Some(output_tuple));
                }
            }
        }
    }

    fn schema(&self) -> Arc<Schema> {
        self.output_schema.clone()
    }
}

fn projection_move_max_index(indices: &[usize], schema_len: usize) -> Option<usize> {
    let mut max_idx: Option<usize> = None;
    for (pos, &idx) in indices.iter().enumerate() {
        if idx >= schema_len || indices[..pos].contains(&idx) {
            return None;
        }
        max_idx = Some(max_idx.map_or(idx, |max| max.max(idx)));
    }
    max_idx
}

fn direct_project_column_indices(schema: &Schema, exprs: &[crate::sql::LogicalExpr]) -> Option<Vec<usize>> {
    let mut indices = Vec::with_capacity(exprs.len());
    for expr in exprs {
        let crate::sql::LogicalExpr::Column { table, name } = expr else {
            return None;
        };
        let idx = schema.get_qualified_column_index(table.as_deref(), name)?;
        indices.push(idx);
    }
    Some(indices)
}

/// Limit operator
///
/// Limits the number of tuples and skips an offset.
pub struct LimitOperator {
    input: Box<dyn PhysicalOperator>,
    limit: usize,
    offset: usize,
    skipped: usize,
    returned: usize,
    timeout_ctx: Option<TimeoutContext>,
}

impl LimitOperator {
    pub fn new(input: Box<dyn PhysicalOperator>, limit: usize, offset: usize) -> Self {
        Self {
            input,
            limit,
            offset,
            skipped: 0,
            returned: 0,
            timeout_ctx: None,
        }
    }

    /// Set timeout context for query execution
    pub fn with_timeout(mut self, timeout_ctx: Option<TimeoutContext>) -> Self {
        self.timeout_ctx = timeout_ctx;
        self
    }
}

impl PhysicalOperator for LimitOperator {
    fn next(&mut self) -> Result<Option<Tuple>> {
        // Skip offset tuples
        while self.skipped < self.offset {
            match self.input.next()? {
                None => return Ok(None),
                Some(_) => {
                    self.skipped += 1;
                }
            }
        }

        // Return up to limit tuples
        if self.returned >= self.limit {
            return Ok(None);
        }

        match self.input.next()? {
            None => Ok(None),
            Some(tuple) => {
                self.returned += 1;
                Ok(Some(tuple))
            }
        }
    }

    fn schema(&self) -> Arc<Schema> {
        self.input.schema()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::sql::executor::ScanOperator;
    use crate::Column;
    use crate::DataType;
    use crate::Value;

    fn test_column(name: &str, source: Option<&str>) -> Column {
        Column {
            name: name.to_string(),
            data_type: DataType::Int4,
            nullable: false,
            primary_key: false,
            source_table: source.map(str::to_string),
            source_table_name: source.map(str::to_string),
            default_expr: None,
            unique: false,
            storage_mode: crate::ColumnStorageMode::Default,
        }
    }

    #[test]
    fn project_operator_uses_direct_indices_for_plain_columns() {
        let schema = Arc::new(Schema {
            columns: vec![
                test_column("id", Some("u")),
                test_column("age", Some("u")),
                test_column("amount", Some("o")),
            ],
        });
        let scan = ScanOperator::new(
            "joined".to_string(),
            schema,
            None,
            vec![Tuple::new(vec![Value::Int4(1), Value::Int4(42), Value::Int4(99)])],
            Vec::new(),
        );
        let mut project = ProjectOperator::new(
            Box::new(scan),
            vec![
                crate::sql::LogicalExpr::Column {
                    table: Some("u".to_string()),
                    name: "age".to_string(),
                },
                crate::sql::LogicalExpr::Column {
                    table: Some("o".to_string()),
                    name: "amount".to_string(),
                },
            ],
            vec!["age".to_string(), "amount".to_string()],
            false,
            Vec::new(),
        );

        assert_eq!(project.direct_column_indices, Some(vec![1, 2]));
        let row = project.next().unwrap().unwrap();
        assert_eq!(row.values, vec![Value::Int4(42), Value::Int4(99)]);
    }

    #[test]
    fn test_limit_operator() {
        // Test with empty scan
        let schema = Arc::new(Schema {
            columns: vec![Column {
                name: "id".to_string(),
                data_type: DataType::Int4,
                nullable: false,
                primary_key: true,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
            }],
        });

        let scan = ScanOperator::new("test".to_string(), schema.clone(), None, Vec::new(), Vec::new());
        let mut limit = LimitOperator::new(Box::new(scan), 10, 0);
        assert!(limit.next().expect("Failed to execute limit").is_none());
    }
}
