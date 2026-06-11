//! Filter operator for WHERE clause execution
//!
//! This module provides the FilterOperator that evaluates predicates
//! on tuples from input operators.

use super::{PhysicalOperator, TimeoutContext};
use crate::{Error, Result, Schema, Tuple, Value};
use std::cmp::Ordering;
use std::sync::Arc;

/// Filter operator
///
/// Evaluates a predicate on each tuple from the input operator.
pub struct FilterOperator {
    input: Box<dyn PhysicalOperator>,
    predicate: crate::sql::LogicalExpr,
    evaluator: crate::sql::Evaluator,
    direct_filter: Option<DirectFilterPredicate>,
    timeout_ctx: Option<TimeoutContext>,
}

impl FilterOperator {
    pub fn new(
        input: Box<dyn PhysicalOperator>,
        predicate: crate::sql::LogicalExpr,
        parameters: Vec<crate::Value>,
    ) -> Self {
        let schema = input.schema();
        let direct_filter = DirectFilterPredicate::try_from_expr(&schema, &predicate, &parameters);
        let evaluator = crate::sql::Evaluator::with_parameters(schema, parameters);
        // R3.5 item 1: resolve column refs to positional indices once, instead
        // of a per-row linear schema scan inside the evaluator.
        let predicate = evaluator.bind(predicate);
        Self {
            input,
            predicate,
            evaluator,
            direct_filter,
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
                    let passes = if let Some(direct_filter) = &self.direct_filter {
                        match direct_filter.matches(&tuple) {
                            Some(passes) => passes,
                            None => self.evaluate_predicate(&tuple)?,
                        }
                    } else {
                        self.evaluate_predicate(&tuple)?
                    };

                    if passes {
                        return Ok(Some(tuple));
                    }
                }
            }
        }
    }

    fn schema(&self) -> Arc<Schema> {
        self.input.schema()
    }
}

impl FilterOperator {
    fn evaluate_predicate(&self, tuple: &Tuple) -> Result<bool> {
        match self.evaluator.evaluate(&self.predicate, tuple)? {
            Value::Boolean(true) => Ok(true),
            Value::Boolean(false) | Value::Null => Ok(false),
            result => Err(Error::query_execution(format!(
                "Filter predicate must evaluate to boolean, got: {:?}",
                result
            ))),
        }
    }
}

#[derive(Clone, Debug)]
enum DirectFilterPredicate {
    Compare {
        column_idx: usize,
        op: DirectCompareOp,
        literal: Value,
    },
    IsNull {
        column_idx: usize,
        is_null: bool,
    },
    And(Vec<DirectFilterPredicate>),
}

#[derive(Clone, Copy, Debug)]
enum DirectCompareOp {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
}

impl DirectFilterPredicate {
    fn try_from_expr(
        schema: &Schema,
        expr: &crate::sql::LogicalExpr,
        parameters: &[Value],
    ) -> Option<DirectFilterPredicate> {
        use crate::sql::{BinaryOperator, LogicalExpr};

        match expr {
            LogicalExpr::BinaryExpr { left, op, right } if *op == BinaryOperator::And => {
                let mut predicates = Vec::new();
                Self::collect_and_predicates(schema, left, parameters, &mut predicates)?;
                Self::collect_and_predicates(schema, right, parameters, &mut predicates)?;
                Some(DirectFilterPredicate::And(predicates))
            }
            LogicalExpr::BinaryExpr { left, op, right } => {
                let op = DirectCompareOp::try_from_binary_op(*op)?;
                if let Some((idx, literal)) = column_literal_comparison(schema, left, right, parameters) {
                    Some(DirectFilterPredicate::Compare {
                        column_idx: idx,
                        op,
                        literal,
                    })
                } else if let Some((idx, literal)) = column_literal_comparison(schema, right, left, parameters) {
                    Some(DirectFilterPredicate::Compare {
                        column_idx: idx,
                        op: op.reversed(),
                        literal,
                    })
                } else {
                    None
                }
            }
            LogicalExpr::IsNull { expr, is_null } => match expr.as_ref() {
                LogicalExpr::Column { table, name } => {
                    let column_idx = schema.get_qualified_column_index(table.as_deref(), name)?;
                    Some(DirectFilterPredicate::IsNull {
                        column_idx,
                        is_null: *is_null,
                    })
                }
                _ => None,
            },
            _ => None,
        }
    }

    fn collect_and_predicates(
        schema: &Schema,
        expr: &crate::sql::LogicalExpr,
        parameters: &[Value],
        predicates: &mut Vec<DirectFilterPredicate>,
    ) -> Option<()> {
        use crate::sql::{BinaryOperator, LogicalExpr};

        if let LogicalExpr::BinaryExpr { left, op, right } = expr {
            if *op == BinaryOperator::And {
                Self::collect_and_predicates(schema, left, parameters, predicates)?;
                Self::collect_and_predicates(schema, right, parameters, predicates)?;
                return Some(());
            }
        }

        predicates.push(Self::try_from_expr(schema, expr, parameters)?);
        Some(())
    }

    fn matches(&self, tuple: &Tuple) -> Option<bool> {
        match self {
            DirectFilterPredicate::Compare {
                column_idx,
                op,
                literal,
            } => {
                let value = tuple.get(*column_idx)?;
                op.matches(value, literal)
            }
            DirectFilterPredicate::IsNull { column_idx, is_null } => {
                let value = tuple.get(*column_idx)?;
                Some(matches!(value, Value::Null) == *is_null)
            }
            DirectFilterPredicate::And(predicates) => {
                for predicate in predicates {
                    if !predicate.matches(tuple)? {
                        return Some(false);
                    }
                }
                Some(true)
            }
        }
    }
}

impl DirectCompareOp {
    fn try_from_binary_op(op: crate::sql::BinaryOperator) -> Option<Self> {
        use crate::sql::BinaryOperator;
        match op {
            BinaryOperator::Eq => Some(Self::Eq),
            BinaryOperator::NotEq => Some(Self::NotEq),
            BinaryOperator::Lt => Some(Self::Lt),
            BinaryOperator::LtEq => Some(Self::LtEq),
            BinaryOperator::Gt => Some(Self::Gt),
            BinaryOperator::GtEq => Some(Self::GtEq),
            _ => None,
        }
    }

    fn reversed(self) -> Self {
        match self {
            Self::Eq => Self::Eq,
            Self::NotEq => Self::NotEq,
            Self::Lt => Self::Gt,
            Self::LtEq => Self::GtEq,
            Self::Gt => Self::Lt,
            Self::GtEq => Self::LtEq,
        }
    }

    fn matches(self, left: &Value, right: &Value) -> Option<bool> {
        let ord = compare_direct_values(left, right)?;
        Some(match self {
            Self::Eq => ord.is_eq(),
            Self::NotEq => ord.is_ne(),
            Self::Lt => ord.is_lt(),
            Self::LtEq => ord.is_le(),
            Self::Gt => ord.is_gt(),
            Self::GtEq => ord.is_ge(),
        })
    }
}

fn column_literal_comparison(
    schema: &Schema,
    column_expr: &crate::sql::LogicalExpr,
    literal_expr: &crate::sql::LogicalExpr,
    parameters: &[Value],
) -> Option<(usize, Value)> {
    let crate::sql::LogicalExpr::Column { table, name } = column_expr else {
        return None;
    };
    let column_idx = schema.get_qualified_column_index(table.as_deref(), name)?;
    let literal = direct_literal_value(literal_expr, parameters)?;
    if matches!(literal, Value::Null) {
        return None;
    }
    Some((column_idx, literal))
}

fn direct_literal_value(expr: &crate::sql::LogicalExpr, parameters: &[Value]) -> Option<Value> {
    match expr {
        crate::sql::LogicalExpr::Literal(value) => Some(value.clone()),
        crate::sql::LogicalExpr::Parameter { index } => parameters.get(index.checked_sub(1)?).cloned(),
        _ => None,
    }
}

fn compare_direct_values(left: &Value, right: &Value) -> Option<Ordering> {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return None;
    }

    match (left, right) {
        (Value::Int2(a), Value::Int2(b)) => Some(a.cmp(b)),
        (Value::Int4(a), Value::Int4(b)) => Some(a.cmp(b)),
        (Value::Int8(a), Value::Int8(b)) => Some(a.cmp(b)),
        (Value::Int2(a), Value::Int4(b)) => Some((*a as i64).cmp(&(*b as i64))),
        (Value::Int4(a), Value::Int2(b)) => Some((*a as i64).cmp(&(*b as i64))),
        (Value::Int2(a), Value::Int8(b)) => Some((*a as i64).cmp(b)),
        (Value::Int8(a), Value::Int2(b)) => Some(a.cmp(&(*b as i64))),
        (Value::Int4(a), Value::Int8(b)) => Some((*a as i64).cmp(b)),
        (Value::Int8(a), Value::Int4(b)) => Some(a.cmp(&(*b as i64))),
        (Value::Float4(a), Value::Float4(b)) => a.partial_cmp(b),
        (Value::Float8(a), Value::Float8(b)) => a.partial_cmp(b),
        (Value::Float4(a), Value::Float8(b)) => (*a as f64).partial_cmp(b),
        (Value::Float8(a), Value::Float4(b)) => a.partial_cmp(&(*b as f64)),
        (Value::Int2(a), Value::Float4(b)) => (*a as f32).partial_cmp(b),
        (Value::Float4(a), Value::Int2(b)) => a.partial_cmp(&(*b as f32)),
        (Value::Int4(a), Value::Float4(b)) => (*a as f32).partial_cmp(b),
        (Value::Float4(a), Value::Int4(b)) => a.partial_cmp(&(*b as f32)),
        (Value::Int8(a), Value::Float4(b)) => (*a as f32).partial_cmp(b),
        (Value::Float4(a), Value::Int8(b)) => a.partial_cmp(&(*b as f32)),
        (Value::Int2(a), Value::Float8(b)) => (*a as f64).partial_cmp(b),
        (Value::Float8(a), Value::Int2(b)) => a.partial_cmp(&(*b as f64)),
        (Value::Int4(a), Value::Float8(b)) => (*a as f64).partial_cmp(b),
        (Value::Float8(a), Value::Int4(b)) => a.partial_cmp(&(*b as f64)),
        (Value::Int8(a), Value::Float8(b)) => (*a as f64).partial_cmp(b),
        (Value::Float8(a), Value::Int8(b)) => a.partial_cmp(&(*b as f64)),
        (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
        (Value::Boolean(a), Value::Boolean(b)) => Some(a.cmp(b)),
        (Value::Uuid(a), Value::Uuid(b)) => Some(a.cmp(b)),
        (Value::Uuid(a), Value::String(b)) => uuid::Uuid::parse_str(b).ok().map(|uuid| a.cmp(&uuid)),
        (Value::String(a), Value::Uuid(b)) => uuid::Uuid::parse_str(a).ok().map(|uuid| uuid.cmp(b)),
        (Value::Timestamp(a), Value::Timestamp(b)) => Some(a.cmp(b)),
        (Value::Date(a), Value::Date(b)) => Some(a.cmp(b)),
        (Value::Time(a), Value::Time(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::executor::ScanOperator;
    use crate::sql::{BinaryOperator, LogicalExpr};
    use crate::{Column, DataType};

    fn test_column(name: &str) -> Column {
        Column {
            name: name.to_string(),
            data_type: DataType::Int4,
            nullable: true,
            primary_key: false,
            source_table: Some("t".to_string()),
            source_table_name: Some("t".to_string()),
            default_expr: None,
            unique: false,
            storage_mode: crate::ColumnStorageMode::Default,
        }
    }

    fn test_scan() -> ScanOperator {
        let schema = Arc::new(Schema {
            columns: vec![test_column("id"), test_column("age")],
        });
        ScanOperator::new(
            "t".to_string(),
            schema,
            None,
            vec![
                Tuple::new(vec![Value::Int4(1), Value::Int4(25)]),
                Tuple::new(vec![Value::Int4(2), Value::Int4(42)]),
                Tuple::new(vec![Value::Int4(3), Value::Null]),
            ],
            Vec::new(),
        )
    }

    #[test]
    fn filter_operator_uses_direct_comparisons_for_and_predicate() {
        let predicate = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::BinaryExpr {
                left: Box::new(LogicalExpr::Column {
                    table: Some("t".to_string()),
                    name: "id".to_string(),
                }),
                op: BinaryOperator::Gt,
                right: Box::new(LogicalExpr::Literal(Value::Int4(1))),
            }),
            op: BinaryOperator::And,
            right: Box::new(LogicalExpr::BinaryExpr {
                left: Box::new(LogicalExpr::Column {
                    table: Some("t".to_string()),
                    name: "age".to_string(),
                }),
                op: BinaryOperator::Eq,
                right: Box::new(LogicalExpr::Parameter { index: 1 }),
            }),
        };
        let mut filter = FilterOperator::new(Box::new(test_scan()), predicate, vec![Value::Int4(42)]);

        assert!(filter.direct_filter.is_some());
        let row = filter.next().unwrap().unwrap();
        assert_eq!(row.values, vec![Value::Int4(2), Value::Int4(42)]);
        assert!(filter.next().unwrap().is_none());
    }

    #[test]
    fn filter_operator_uses_direct_is_null() {
        let predicate = LogicalExpr::IsNull {
            expr: Box::new(LogicalExpr::Column {
                table: Some("t".to_string()),
                name: "age".to_string(),
            }),
            is_null: true,
        };
        let mut filter = FilterOperator::new(Box::new(test_scan()), predicate, Vec::new());

        assert!(filter.direct_filter.is_some());
        let row = filter.next().unwrap().unwrap();
        assert_eq!(row.values, vec![Value::Int4(3), Value::Null]);
        assert!(filter.next().unwrap().is_none());
    }
}
