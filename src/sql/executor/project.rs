//! Projection and limit operators
//!
//! This module provides operators for SELECT column projection and LIMIT/OFFSET.

use crate::{Result, Tuple, Schema};
use super::{PhysicalOperator, TimeoutContext};
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
        let columns = aliases.iter()
            .zip(exprs.iter())
            .map(|(alias, expr)| {
                // Infer type from expression, fallback to Text if inference fails
                let data_type = expr.infer_type(&input_schema)
                    .unwrap_or(crate::DataType::Text);

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

        // Create evaluator with input schema and parameters
        let evaluator = crate::sql::Evaluator::with_parameters(input_schema, parameters);

        Self {
            input,
            exprs,
            aliases,
            output_schema,
            evaluator,
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
                Some(tuple) => {
                    // Evaluate each expression to produce output values
                    let output_values: Result<Vec<crate::Value>> = self.exprs.iter()
                        .map(|expr| self.evaluator.evaluate(expr, &tuple))
                        .collect();

                    let mut output_tuple = Tuple::new(output_values?);
                    // Preserve row_id through projection for DML operations
                    output_tuple.row_id = tuple.row_id;

                    // Handle DISTINCT ON (PostgreSQL extension)
                    // Only return the first row for each unique combination of DISTINCT ON expressions
                    if let Some(ref distinct_on_exprs) = self.distinct_on_exprs {
                        let key_values: Result<Vec<crate::Value>> = distinct_on_exprs.iter()
                            .map(|expr| self.evaluator.evaluate(expr, &tuple))
                            .collect();
                        let key = bincode::serialize(&key_values?)
                            .map_err(|e| crate::Error::query_execution(
                                format!("Failed to serialize DISTINCT ON key: {}", e)
                            ))?;

                        if self.seen.contains(&key) {
                            // Skip rows with duplicate DISTINCT ON values
                            continue;
                        }
                        self.seen.insert(key);
                    } else if self.distinct {
                        // Regular DISTINCT - check for duplicates using full tuple values
                        // Serialize only the values for deduplication (not row_id or branch_id)
                        let serialized = bincode::serialize(&output_tuple.values)
                            .map_err(|e| crate::Error::query_execution(
                                format!("Failed to serialize tuple for DISTINCT: {}", e)
                            ))?;

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
    use crate::Column;
    use crate::DataType;
    use crate::sql::executor::ScanOperator;

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
            }],
        });

        let scan = ScanOperator::new("test".to_string(), schema.clone(), None, Vec::new(), Vec::new());
        let mut limit = LimitOperator::new(Box::new(scan), 10, 0);
        assert!(limit.next().expect("Failed to execute limit").is_none());
    }
}
