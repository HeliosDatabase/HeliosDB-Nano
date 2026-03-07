//! Filter operator for WHERE clause execution
//!
//! This module provides the FilterOperator that evaluates predicates
//! on tuples from input operators.

use crate::{Result, Error, Tuple, Schema};
use super::{PhysicalOperator, TimeoutContext};
use std::sync::Arc;

/// Filter operator
///
/// Evaluates a predicate on each tuple from the input operator.
pub struct FilterOperator {
    input: Box<dyn PhysicalOperator>,
    predicate: crate::sql::LogicalExpr,
    evaluator: crate::sql::Evaluator,
    timeout_ctx: Option<TimeoutContext>,
}

impl FilterOperator {
    pub fn new(
        input: Box<dyn PhysicalOperator>,
        predicate: crate::sql::LogicalExpr,
        parameters: Vec<crate::Value>,
    ) -> Self {
        let schema = input.schema();
        let evaluator = crate::sql::Evaluator::with_parameters(schema, parameters);
        Self {
            input,
            predicate,
            evaluator,
            timeout_ctx: None,
        }
    }

    pub fn with_timeout(mut self, timeout_ctx: Option<TimeoutContext>) -> Self {
        self.timeout_ctx = timeout_ctx;
        self
    }
}

impl PhysicalOperator for FilterOperator {
    fn next(&mut self) -> Result<Option<Tuple>> {
        loop {
            // Check timeout before processing
            if let Some(ref ctx) = self.timeout_ctx {
                ctx.check_timeout()?;
            }

            match self.input.next()? {
                None => return Ok(None),
                Some(tuple) => {
                    // Evaluate predicate
                    let result = self.evaluator.evaluate(&self.predicate, &tuple)?;

                    // Check if result is true
                    match result {
                        crate::Value::Boolean(true) => return Ok(Some(tuple)),
                        crate::Value::Boolean(false) => continue, // Skip this tuple
                        // SQL standard: NULL predicates are falsy (three-valued logic)
                        crate::Value::Null => continue,
                        _ => {
                            return Err(Error::query_execution(format!(
                                "Filter predicate must evaluate to boolean, got: {:?}",
                                result
                            )))
                        }
                    }
                }
            }
        }
    }

    fn schema(&self) -> Arc<Schema> {
        self.input.schema()
    }
}
