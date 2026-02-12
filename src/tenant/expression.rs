//! RLS expression parsing and evaluation
//!
//! This module provides functionality to parse and evaluate Row-Level Security (RLS)
//! policy expressions for multi-tenant data isolation.

use crate::{Result, Error, Value, Tuple, Schema};
use crate::sql::{LogicalExpr, BinaryOperator};
use crate::tenant::TenantContext;
use sqlparser::ast::{Expr, BinaryOperator as SqlBinaryOp, UnaryOperator as SqlUnaryOp};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser as SqlParser;
use std::sync::Arc;

/// RLS expression evaluator
///
/// Evaluates RLS policy expressions against tuples in a tenant context.
pub struct RLSExpressionEvaluator {
    /// The schema of the table being evaluated
    schema: Arc<Schema>,
    /// The current tenant context
    tenant_context: Option<TenantContext>,
}

impl RLSExpressionEvaluator {
    /// Create a new RLS expression evaluator
    pub fn new(schema: Arc<Schema>, tenant_context: Option<TenantContext>) -> Self {
        Self {
            schema,
            tenant_context,
        }
    }

    /// Parse an RLS expression string into a LogicalExpr
    ///
    /// # Arguments
    ///
    /// * `expr_str` - The RLS expression string (e.g., "tenant_id = current_tenant()")
    ///
    /// # Returns
    ///
    /// A LogicalExpr that can be evaluated against tuples
    pub fn parse(&self, expr_str: &str) -> Result<LogicalExpr> {
        // Parse the expression using sqlparser
        let dialect = PostgreSqlDialect {};

        // Wrap the expression in a SELECT to make it parseable
        let sql = format!("SELECT * FROM dummy WHERE {}", expr_str);

        let mut statements = SqlParser::parse_sql(&dialect, &sql)
            .map_err(|e| Error::query_execution(format!("Failed to parse RLS expression '{}': {}", expr_str, e)))?;

        if statements.len() != 1 {
            return Err(Error::query_execution("Invalid RLS expression: expected single statement"));
        }

        // Extract the WHERE clause from the SELECT statement
        let statement = statements.remove(0);

        if let sqlparser::ast::Statement::Query(query) = statement {
            if let sqlparser::ast::SetExpr::Select(select) = *query.body {
                if let Some(selection) = select.selection {
                    return self.sql_expr_to_logical(&selection);
                }
            }
        }

        Err(Error::query_execution(format!("Failed to extract WHERE clause from RLS expression: {}", expr_str)))
    }

    /// Convert a SQL expression to a LogicalExpr
    fn sql_expr_to_logical(&self, expr: &Expr) -> Result<LogicalExpr> {
        match expr {
            Expr::Identifier(ident) => Ok(LogicalExpr::Column {
                table: None,
                name: ident.value.clone(),
            }),

            Expr::CompoundIdentifier(idents) => {
                // Handle table.column references - preserve the table qualifier for JOIN disambiguation
                if idents.len() >= 2 {
                    let table_alias = idents.get(idents.len() - 2)
                        .ok_or_else(|| Error::query_execution("Invalid compound identifier"))?
                        .value.clone();
                    let column_name = idents.last()
                        .ok_or_else(|| Error::query_execution("Empty compound identifier"))?
                        .value.clone();
                    Ok(LogicalExpr::Column {
                        table: Some(table_alias),
                        name: column_name,
                    })
                } else {
                    let column_name = idents.last()
                        .ok_or_else(|| Error::query_execution("Empty compound identifier"))?
                        .value.clone();
                    Ok(LogicalExpr::Column {
                        table: None,
                        name: column_name,
                    })
                }
            }

            Expr::Value(value) => {
                Ok(LogicalExpr::Literal(self.sql_value_to_value(value)?))
            }

            Expr::BinaryOp { left, op, right } => {
                let left_expr = self.sql_expr_to_logical(left)?;
                let right_expr = self.sql_expr_to_logical(right)?;
                let logical_op = self.sql_binary_op_to_logical(op)?;

                Ok(LogicalExpr::BinaryExpr {
                    left: Box::new(left_expr),
                    op: logical_op,
                    right: Box::new(right_expr),
                })
            }

            Expr::UnaryOp { op, expr } => {
                let logical_expr = self.sql_expr_to_logical(expr)?;
                let logical_op = self.sql_unary_op_to_logical(op)?;

                Ok(LogicalExpr::UnaryExpr {
                    op: logical_op,
                    expr: Box::new(logical_expr),
                })
            }

            Expr::Function(func) => {
                let func_name = func.name.to_string().to_lowercase();

                // Extract arguments based on FunctionArguments type
                let arg_list = match &func.args {
                    sqlparser::ast::FunctionArguments::None => vec![],
                    sqlparser::ast::FunctionArguments::Subquery(_) => {
                        return Err(Error::query_execution(
                            "Subquery arguments not supported in RLS functions".to_string()
                        ));
                    }
                    sqlparser::ast::FunctionArguments::List(list) => list.args.clone(),
                };

                let args: Result<Vec<LogicalExpr>> = arg_list
                    .iter()
                    .filter_map(|arg| {
                        if let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr)) = arg {
                            Some(self.sql_expr_to_logical(&expr))
                        } else {
                            None
                        }
                    })
                    .collect();

                Ok(LogicalExpr::ScalarFunction {
                    fun: func_name,
                    args: args?,
                })
            }

            Expr::IsNull(expr) => Ok(LogicalExpr::IsNull {
                expr: Box::new(self.sql_expr_to_logical(expr)?),
                is_null: true,
            }),

            Expr::IsNotNull(expr) => Ok(LogicalExpr::IsNull {
                expr: Box::new(self.sql_expr_to_logical(expr)?),
                is_null: false,
            }),

            _ => Err(Error::query_execution(format!(
                "Unsupported expression in RLS policy: {:?}",
                expr
            ))),
        }
    }

    /// Convert SQL binary operator to logical operator
    fn sql_binary_op_to_logical(&self, op: &SqlBinaryOp) -> Result<BinaryOperator> {
        match op {
            SqlBinaryOp::Eq => Ok(BinaryOperator::Eq),
            SqlBinaryOp::NotEq => Ok(BinaryOperator::NotEq),
            SqlBinaryOp::Lt => Ok(BinaryOperator::Lt),
            SqlBinaryOp::LtEq => Ok(BinaryOperator::LtEq),
            SqlBinaryOp::Gt => Ok(BinaryOperator::Gt),
            SqlBinaryOp::GtEq => Ok(BinaryOperator::GtEq),
            SqlBinaryOp::And => Ok(BinaryOperator::And),
            SqlBinaryOp::Or => Ok(BinaryOperator::Or),
            _ => Err(Error::query_execution(format!(
                "Unsupported binary operator in RLS: {:?}",
                op
            ))),
        }
    }

    /// Convert SQL unary operator to logical operator
    fn sql_unary_op_to_logical(&self, op: &SqlUnaryOp) -> Result<crate::sql::UnaryOperator> {
        match op {
            SqlUnaryOp::Not => Ok(crate::sql::UnaryOperator::Not),
            SqlUnaryOp::Minus => Ok(crate::sql::UnaryOperator::Minus),
            SqlUnaryOp::Plus => Ok(crate::sql::UnaryOperator::Plus),
            _ => Err(Error::query_execution(format!(
                "Unsupported unary operator in RLS: {:?}",
                op
            ))),
        }
    }

    /// Convert SQL value to internal Value
    fn sql_value_to_value(&self, value: &sqlparser::ast::Value) -> Result<Value> {
        match value {
            sqlparser::ast::Value::Number(n, _) => {
                // Try to parse as integer first, then as float
                if let Ok(i) = n.parse::<i64>() {
                    Ok(Value::Int8(i))
                } else if let Ok(f) = n.parse::<f64>() {
                    Ok(Value::Float8(f))
                } else {
                    Err(Error::query_execution(format!("Invalid number: {}", n)))
                }
            }
            sqlparser::ast::Value::SingleQuotedString(s) |
            sqlparser::ast::Value::DoubleQuotedString(s) => {
                Ok(Value::String(s.clone()))
            }
            sqlparser::ast::Value::Boolean(b) => Ok(Value::Boolean(*b)),
            sqlparser::ast::Value::Null => Ok(Value::Null),
            _ => Err(Error::query_execution(format!(
                "Unsupported value type in RLS: {:?}",
                value
            ))),
        }
    }

    /// Evaluate an RLS expression against a tuple
    ///
    /// # Arguments
    ///
    /// * `expr` - The logical expression to evaluate
    /// * `tuple` - The tuple to evaluate against
    ///
    /// # Returns
    ///
    /// `true` if the tuple satisfies the RLS policy, `false` otherwise
    pub fn evaluate(&self, expr: &LogicalExpr, tuple: &Tuple) -> Result<bool> {
        let value = self.evaluate_expr(expr, tuple)?;

        match value {
            Value::Boolean(b) => Ok(b),
            _ => Err(Error::query_execution(format!(
                "RLS expression must evaluate to boolean, got: {:?}",
                value
            ))),
        }
    }

    /// Evaluate an expression to a value
    fn evaluate_expr(&self, expr: &LogicalExpr, tuple: &Tuple) -> Result<Value> {
        match expr {
            LogicalExpr::Literal(value) => Ok(value.clone()),

            LogicalExpr::Column { name, .. } => {
                // Find column index in schema
                let index = self.schema.get_column_index(name)
                    .ok_or_else(|| Error::query_execution(format!(
                        "Column '{}' not found in schema",
                        name
                    )))?;

                // Get value from tuple
                tuple.get(index)
                    .cloned()
                    .ok_or_else(|| Error::query_execution(format!(
                        "Column index {} out of bounds in tuple",
                        index
                    )))
            }

            LogicalExpr::BinaryExpr { left, op, right } => {
                let left_val = self.evaluate_expr(left, tuple)?;
                let right_val = self.evaluate_expr(right, tuple)?;
                self.evaluate_binary_op(&left_val, op, &right_val)
            }

            LogicalExpr::UnaryExpr { op, expr } => {
                let val = self.evaluate_expr(expr, tuple)?;
                self.evaluate_unary_op(op, &val)
            }

            LogicalExpr::IsNull { expr, is_null } => {
                let val = self.evaluate_expr(expr, tuple)?;
                let is_actually_null = matches!(val, Value::Null);
                Ok(Value::Boolean(is_actually_null == *is_null))
            }

            LogicalExpr::ScalarFunction { fun, args } => {
                self.evaluate_scalar_function(fun, args, tuple)
            }

            _ => Err(Error::query_execution(format!(
                "Expression not supported in RLS: {:?}",
                expr
            ))),
        }
    }

    /// Evaluate a binary operation
    fn evaluate_binary_op(&self, left: &Value, op: &BinaryOperator, right: &Value) -> Result<Value> {
        match op {
            BinaryOperator::Eq => Ok(Value::Boolean(left == right)),
            BinaryOperator::NotEq => Ok(Value::Boolean(left != right)),

            BinaryOperator::Lt => match (left, right) {
                (Value::Int8(l), Value::Int8(r)) => Ok(Value::Boolean(l < r)),
                (Value::Int4(l), Value::Int4(r)) => Ok(Value::Boolean(l < r)),
                (Value::Int2(l), Value::Int2(r)) => Ok(Value::Boolean(l < r)),
                (Value::Float8(l), Value::Float8(r)) => Ok(Value::Boolean(l < r)),
                (Value::Float4(l), Value::Float4(r)) => Ok(Value::Boolean(l < r)),
                (Value::String(l), Value::String(r)) => Ok(Value::Boolean(l < r)),
                _ => Err(Error::query_execution(format!("Cannot compare {:?} < {:?}", left, right))),
            },

            BinaryOperator::LtEq => match (left, right) {
                (Value::Int8(l), Value::Int8(r)) => Ok(Value::Boolean(l <= r)),
                (Value::Int4(l), Value::Int4(r)) => Ok(Value::Boolean(l <= r)),
                (Value::Int2(l), Value::Int2(r)) => Ok(Value::Boolean(l <= r)),
                (Value::Float8(l), Value::Float8(r)) => Ok(Value::Boolean(l <= r)),
                (Value::Float4(l), Value::Float4(r)) => Ok(Value::Boolean(l <= r)),
                (Value::String(l), Value::String(r)) => Ok(Value::Boolean(l <= r)),
                _ => Err(Error::query_execution(format!("Cannot compare {:?} <= {:?}", left, right))),
            },

            BinaryOperator::Gt => match (left, right) {
                (Value::Int8(l), Value::Int8(r)) => Ok(Value::Boolean(l > r)),
                (Value::Int4(l), Value::Int4(r)) => Ok(Value::Boolean(l > r)),
                (Value::Int2(l), Value::Int2(r)) => Ok(Value::Boolean(l > r)),
                (Value::Float8(l), Value::Float8(r)) => Ok(Value::Boolean(l > r)),
                (Value::Float4(l), Value::Float4(r)) => Ok(Value::Boolean(l > r)),
                (Value::String(l), Value::String(r)) => Ok(Value::Boolean(l > r)),
                _ => Err(Error::query_execution(format!("Cannot compare {:?} > {:?}", left, right))),
            },

            BinaryOperator::GtEq => match (left, right) {
                (Value::Int8(l), Value::Int8(r)) => Ok(Value::Boolean(l >= r)),
                (Value::Int4(l), Value::Int4(r)) => Ok(Value::Boolean(l >= r)),
                (Value::Int2(l), Value::Int2(r)) => Ok(Value::Boolean(l >= r)),
                (Value::Float8(l), Value::Float8(r)) => Ok(Value::Boolean(l >= r)),
                (Value::Float4(l), Value::Float4(r)) => Ok(Value::Boolean(l >= r)),
                (Value::String(l), Value::String(r)) => Ok(Value::Boolean(l >= r)),
                _ => Err(Error::query_execution(format!("Cannot compare {:?} >= {:?}", left, right))),
            },

            BinaryOperator::And => {
                let l = self.value_to_bool(left)?;
                let r = self.value_to_bool(right)?;
                Ok(Value::Boolean(l && r))
            }

            BinaryOperator::Or => {
                let l = self.value_to_bool(left)?;
                let r = self.value_to_bool(right)?;
                Ok(Value::Boolean(l || r))
            }

            _ => Err(Error::query_execution(format!(
                "Operator {:?} not supported in RLS",
                op
            ))),
        }
    }

    /// Evaluate a unary operation
    fn evaluate_unary_op(&self, op: &crate::sql::UnaryOperator, val: &Value) -> Result<Value> {
        match op {
            crate::sql::UnaryOperator::Not => {
                let b = self.value_to_bool(val)?;
                Ok(Value::Boolean(!b))
            }
            crate::sql::UnaryOperator::Minus => match val {
                Value::Int8(i) => Ok(Value::Int8(-i)),
                Value::Int4(i) => Ok(Value::Int4(-i)),
                Value::Int2(i) => Ok(Value::Int2(-i)),
                Value::Float8(f) => Ok(Value::Float8(-f)),
                Value::Float4(f) => Ok(Value::Float4(-f)),
                _ => Err(Error::query_execution(format!("Cannot negate {:?}", val))),
            },
            crate::sql::UnaryOperator::Plus => Ok(val.clone()),
        }
    }

    /// Convert a value to boolean
    fn value_to_bool(&self, val: &Value) -> Result<bool> {
        match val {
            Value::Boolean(b) => Ok(*b),
            _ => Err(Error::query_execution(format!(
                "Expected boolean value, got {:?}",
                val
            ))),
        }
    }

    /// Evaluate a scalar function
    fn evaluate_scalar_function(&self, fun: &str, args: &[LogicalExpr], tuple: &Tuple) -> Result<Value> {
        match fun.to_lowercase().as_str() {
            "current_tenant" => {
                // Return the current tenant ID from context
                if let Some(ref context) = self.tenant_context {
                    Ok(Value::String(context.tenant_id.to_string()))
                } else {
                    Err(Error::query_execution("current_tenant() called without tenant context"))
                }
            }

            "current_setting" => {
                // current_setting('var_name')
                if args.len() != 1 {
                    return Err(Error::query_execution("current_setting() requires exactly 1 argument"));
                }

                // Evaluate the argument to get the setting name
                let setting_name_val = self.evaluate_expr(args.first().ok_or_else(|| Error::query_execution("current_setting() requires exactly 1 argument"))?, tuple)?;
                let setting_name = match setting_name_val {
                    Value::String(s) => s,
                    _ => return Err(Error::query_execution("current_setting() argument must be a string")),
                };

                // For now, we only support tenant-specific settings
                match setting_name.as_str() {
                    "app.current_tenant" => {
                        if let Some(ref context) = self.tenant_context {
                            Ok(Value::String(context.tenant_id.to_string()))
                        } else {
                            Ok(Value::Null)
                        }
                    }
                    "app.current_user" => {
                        if let Some(ref context) = self.tenant_context {
                            Ok(Value::String(context.user_id.clone()))
                        } else {
                            Ok(Value::Null)
                        }
                    }
                    _ => {
                        // Unknown setting
                        Ok(Value::Null)
                    }
                }
            }

            _ => Err(Error::query_execution(format!(
                "Function '{}' not supported in RLS expressions",
                fun
            ))),
        }
    }
}

/// Parse and evaluate an RLS expression in one call
///
/// # Arguments
///
/// * `expr_str` - The RLS expression string
/// * `tuple` - The tuple to evaluate against
/// * `schema` - The schema of the table
/// * `tenant_context` - The current tenant context
///
/// # Returns
///
/// `true` if the tuple satisfies the RLS policy, `false` otherwise
pub fn evaluate_rls_expression(
    expr_str: &str,
    tuple: &Tuple,
    schema: Arc<Schema>,
    tenant_context: Option<TenantContext>,
) -> Result<bool> {
    let evaluator = RLSExpressionEvaluator::new(schema, tenant_context);
    let expr = evaluator.parse(expr_str)?;
    evaluator.evaluate(&expr, tuple)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Column, DataType};
    use crate::tenant::IsolationMode;
    use uuid::Uuid;

    fn create_test_schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Column::new("id".to_string(), DataType::Int4),
            Column::new("tenant_id".to_string(), DataType::Text),
            Column::new("name".to_string(), DataType::Text),
        ]))
    }

    fn create_test_context() -> TenantContext {
        TenantContext {
            tenant_id: Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap(),
            user_id: "user1".to_string(),
            roles: vec!["user".to_string()],
            isolation_mode: IsolationMode::SharedSchema,
        }
    }

    #[test]
    fn test_parse_simple_equality() {
        let schema = create_test_schema();
        let context = create_test_context();
        let evaluator = RLSExpressionEvaluator::new(schema, Some(context));

        let expr = evaluator.parse("tenant_id = '550e8400-e29b-41d4-a716-446655440000'").unwrap();

        match expr {
            LogicalExpr::BinaryExpr { op: BinaryOperator::Eq, .. } => {},
            _ => panic!("Expected BinaryExpr with Eq operator"),
        }
    }

    #[test]
    fn test_evaluate_tenant_match() {
        let schema = create_test_schema();
        let context = create_test_context();
        let evaluator = RLSExpressionEvaluator::new(schema, Some(context));

        let tuple = Tuple::new(vec![
            Value::Int8(1),
            Value::String("550e8400-e29b-41d4-a716-446655440000".to_string()),
            Value::String("Test".to_string()),
        ]);

        let expr = evaluator.parse("tenant_id = '550e8400-e29b-41d4-a716-446655440000'").unwrap();
        let result = evaluator.evaluate(&expr, &tuple).unwrap();

        assert!(result);
    }

    #[test]
    fn test_evaluate_tenant_mismatch() {
        let schema = create_test_schema();
        let context = create_test_context();
        let evaluator = RLSExpressionEvaluator::new(schema, Some(context));

        let tuple = Tuple::new(vec![
            Value::Int8(1),
            Value::String("different-tenant-id".to_string()),
            Value::String("Test".to_string()),
        ]);

        let expr = evaluator.parse("tenant_id = '550e8400-e29b-41d4-a716-446655440000'").unwrap();
        let result = evaluator.evaluate(&expr, &tuple).unwrap();

        assert!(!result);
    }

    #[test]
    fn test_current_tenant_function() {
        let schema = create_test_schema();
        let context = create_test_context();
        let evaluator = RLSExpressionEvaluator::new(schema, Some(context.clone()));

        let tuple = Tuple::new(vec![
            Value::Int8(1),
            Value::String(context.tenant_id.to_string()),
            Value::String("Test".to_string()),
        ]);

        let expr = evaluator.parse("tenant_id = current_tenant()").unwrap();
        let result = evaluator.evaluate(&expr, &tuple).unwrap();

        assert!(result);
    }

    #[test]
    fn test_complex_expression() {
        let schema = create_test_schema();
        let context = create_test_context();
        let evaluator = RLSExpressionEvaluator::new(schema, Some(context.clone()));

        let tuple = Tuple::new(vec![
            Value::Int8(5),
            Value::String(context.tenant_id.to_string()),
            Value::String("Test".to_string()),
        ]);

        let expr = evaluator.parse("tenant_id = current_tenant() AND id > 3").unwrap();
        let result = evaluator.evaluate(&expr, &tuple).unwrap();

        assert!(result);
    }
}
