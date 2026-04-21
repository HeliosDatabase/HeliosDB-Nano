//! Type inference for SQL expressions
//!
//! This module provides type inference capabilities for logical expressions,
//! enabling the query executor to determine correct column types at compile time
//! rather than defaulting everything to Text.

use crate::{DataType, Schema, Column, Error, Result};
use super::{LogicalExpr, BinaryOperator, UnaryOperator, AggregateFunction};

/// Type inference trait for logical expressions
pub trait TypeInference {
    /// Infer the data type of this expression given a schema context
    ///
    /// # Arguments
    ///
    /// * `schema` - The input schema providing column type information
    ///
    /// # Returns
    ///
    /// The inferred data type or an error if type cannot be determined
    fn infer_type(&self, schema: &Schema) -> Result<DataType>;

    /// Infer nullability of this expression
    ///
    /// # Arguments
    ///
    /// * `schema` - The input schema providing column nullability information
    ///
    /// # Returns
    ///
    /// True if the expression can produce NULL values, false otherwise
    fn infer_nullable(&self, schema: &Schema) -> bool;

    /// Create a Column definition from this expression
    ///
    /// Infers both type and nullability to create a complete column definition.
    ///
    /// # Arguments
    ///
    /// * `name` - The column name (or alias)
    /// * `schema` - The input schema providing type information
    ///
    /// # Returns
    ///
    /// A Column definition with inferred type and nullability
    fn to_column(&self, name: String, schema: &Schema) -> Column;
}

impl TypeInference for LogicalExpr {
    // SAFETY: All args[0] accesses are guarded by args.is_empty() checks.
    #[allow(clippy::indexing_slicing)]
    fn infer_type(&self, schema: &Schema) -> Result<DataType> {
        match self {
            // Column reference: look up in schema
            LogicalExpr::Column { name, .. } => {
                schema.get_column(name)
                    .map(|col| col.data_type.clone())
                    .ok_or_else(|| Error::type_conversion(format!("Column '{}' not found in schema", name)))
            }

            // Literal: get type from value
            LogicalExpr::Literal(value) => {
                Ok(value.data_type())
            }

            // Binary expression: apply type coercion rules
            LogicalExpr::BinaryExpr { left, op, right } => {
                let left_type = left.infer_type(schema)?;
                let right_type = right.infer_type(schema)?;
                coerce_binary_types(left_type, right_type, op)
            }

            // Unary expression: preserve operand type (with exceptions)
            LogicalExpr::UnaryExpr { op, expr } => {
                let expr_type = expr.infer_type(schema)?;
                match op {
                    UnaryOperator::Not => Ok(DataType::Boolean),
                    UnaryOperator::Minus | UnaryOperator::Plus => Ok(expr_type),
                }
            }

            // Aggregate function: determine result type based on function
            LogicalExpr::AggregateFunction { fun, args, .. } => {
                match fun {
                    AggregateFunction::Count => Ok(DataType::Int8),
                    AggregateFunction::Sum => {
                        if args.is_empty() {
                            return Err(Error::type_conversion("SUM requires at least one argument"));
                        }
                        // Preserve input type for SUM
                        args[0].infer_type(schema)
                    }
                    AggregateFunction::Avg => {
                        // AVG always returns Float8
                        Ok(DataType::Float8)
                    }
                    AggregateFunction::Min | AggregateFunction::Max => {
                        if args.is_empty() {
                            return Err(Error::type_conversion(format!("{:?} requires at least one argument", fun)));
                        }
                        // Preserve input type for MIN/MAX
                        args[0].infer_type(schema)
                    }
                    AggregateFunction::JsonAgg => {
                        // JSON_AGG always returns Jsonb
                        Ok(DataType::Jsonb)
                    }
                    AggregateFunction::ArrayAgg => {
                        // ARRAY_AGG returns an array of the input type
                        if args.is_empty() {
                            return Ok(DataType::Array(Box::new(DataType::Text)));
                        }
                        let elem_type = args[0].infer_type(schema)?;
                        Ok(DataType::Array(Box::new(elem_type)))
                    }
                    AggregateFunction::StringAgg { .. } => {
                        // STRING_AGG always returns Text
                        Ok(DataType::Text)
                    }
                }
            }

            // Scalar function: need function registry for proper typing
            // For now, use conservative Text default
            LogicalExpr::ScalarFunction { fun, args } => {
                match fun.to_lowercase().as_str() {
                    "length" | "char_length" | "character_length" => Ok(DataType::Int8),
                    "upper" | "lower" | "trim" | "ltrim" | "rtrim" => Ok(DataType::Text),
                    "substring" | "substr" => Ok(DataType::Text),
                    "concat" => Ok(DataType::Text),
                    "abs" => {
                        if args.is_empty() {
                            Ok(DataType::Float8)
                        } else {
                            args[0].infer_type(schema)
                        }
                    }
                    "round" | "floor" | "ceil" | "ceiling" => Ok(DataType::Float8),
                    "now" | "current_timestamp" => Ok(DataType::Timestamp),
                    "current_date" => Ok(DataType::Date),
                    "current_time" => Ok(DataType::Time),

                    // Phase 1: New JSON/JSONB functions
                    "jsonb_build_object" | "json_build_object" => Ok(DataType::Jsonb),
                    "jsonb_build_array" | "json_build_array" => Ok(DataType::Jsonb),
                    "jsonb_set" | "json_set" => Ok(DataType::Jsonb),
                    "jsonb_concat" => Ok(DataType::Jsonb),
                    "jsonb_delete" => Ok(DataType::Jsonb),
                    "jsonb_extract_path" | "json_extract_path" => Ok(DataType::Jsonb),
                    "jsonb_extract_path_text" | "json_extract_path_text" => Ok(DataType::Text),
                    "jsonb_array_elements" => Ok(DataType::Jsonb),
                    "jsonb_array_elements_text" => Ok(DataType::Array(Box::new(DataType::Text))),
                    "jsonb_each" => Ok(DataType::Array(Box::new(DataType::Text))),
                    "jsonb_each_text" => Ok(DataType::Array(Box::new(DataType::Text))),
                    "jsonb_object_keys" => Ok(DataType::Array(Box::new(DataType::Text))),
                    "jsonb_array_length" => Ok(DataType::Int4),
                    "jsonb_typeof" => Ok(DataType::Text),
                    "jsonb_path_query" => Ok(DataType::Jsonb),

                    _ => Ok(DataType::Text), // Conservative fallback
                }
            }

            // CASE expression: infer from THEN/ELSE branches
            LogicalExpr::Case { when_then, else_result, .. } => {
                // Try to infer from first THEN branch
                if let Some((_, then_expr)) = when_then.first() {
                    return then_expr.infer_type(schema);
                }
                // Otherwise try ELSE branch
                if let Some(else_expr) = else_result {
                    return else_expr.infer_type(schema);
                }
                // Conservative fallback
                Ok(DataType::Text)
            }

            // CAST expression: use target type
            LogicalExpr::Cast { data_type, .. } => {
                Ok(data_type.clone())
            }

            // IS NULL / IS NOT NULL: always boolean
            LogicalExpr::IsNull { .. } => {
                Ok(DataType::Boolean)
            }

            // BETWEEN: always boolean
            LogicalExpr::Between { .. } => {
                Ok(DataType::Boolean)
            }

            // IN list: always boolean
            LogicalExpr::InList { .. } => {
                Ok(DataType::Boolean)
            }

            // IN set (HashSet-based): always boolean
            LogicalExpr::InSet { .. } => {
                Ok(DataType::Boolean)
            }

            // IN subquery: always returns boolean
            LogicalExpr::InSubquery { .. } => {
                Ok(DataType::Boolean)
            }

            // Scalar subquery: type is the first column of the subplan
            LogicalExpr::ScalarSubquery { subquery } => {
                subquery.schema().columns.first()
                    .map(|c| c.data_type.clone())
                    .ok_or_else(|| Error::type_conversion(
                        "Scalar subquery returned no columns".to_string()
                    ))
            }

            // EXISTS subquery: always returns boolean
            LogicalExpr::Exists { .. } => {
                Ok(DataType::Boolean)
            }

            // Wildcard: cannot infer type
            LogicalExpr::Wildcard => {
                Err(Error::type_conversion("Cannot infer type for wildcard expression"))
            }

            // Parameter: cannot infer without parameter values
            LogicalExpr::Parameter { index } => {
                Err(Error::type_conversion(format!("Cannot infer type for parameter ${}", index)))
            }

            // NEW row: look up in schema (same as column reference)
            LogicalExpr::NewRow { column } => {
                schema.get_column(column)
                    .map(|col| col.data_type.clone())
                    .ok_or_else(|| Error::type_conversion(format!("Column '{}' not found in NEW row", column)))
            }

            // OLD row: look up in schema (same as column reference)
            LogicalExpr::OldRow { column } => {
                schema.get_column(column)
                    .map(|col| col.data_type.clone())
                    .ok_or_else(|| Error::type_conversion(format!("Column '{}' not found in OLD row", column)))
            }

            // Array subscript: arr[n] returns element type
            LogicalExpr::ArraySubscript { array, .. } => {
                let array_type = array.infer_type(schema)?;
                match array_type {
                    DataType::Array(elem_type) => Ok(*elem_type),
                    _ => Err(Error::type_conversion(
                        "Array subscript requires an array type".to_string()
                    )),
                }
            }

            // Row constructor: only appears inside comparisons, where the
            // overall expression type is Boolean. If someone asks for the
            // type of a bare tuple, fall back to Boolean (caller will error
            // at evaluation time anyway).
            LogicalExpr::Tuple { .. } => Ok(DataType::Boolean),

            // Window function: infer based on function type
            LogicalExpr::WindowFunction { fun, args, .. } => {
                use super::logical_plan::WindowFunctionType;
                match fun {
                    WindowFunctionType::RowNumber |
                    WindowFunctionType::Rank |
                    WindowFunctionType::DenseRank |
                    WindowFunctionType::Ntile => Ok(DataType::Int8),
                    WindowFunctionType::PercentRank |
                    WindowFunctionType::CumeDist => Ok(DataType::Float8),
                    WindowFunctionType::Lag |
                    WindowFunctionType::Lead |
                    WindowFunctionType::FirstValue |
                    WindowFunctionType::LastValue |
                    WindowFunctionType::NthValue => {
                        // Return type matches the argument type
                        if args.is_empty() {
                            Ok(DataType::Text) // Fallback
                        } else {
                            args[0].infer_type(schema)
                        }
                    }
                    WindowFunctionType::Aggregate(aggr) => {
                        // Delegate to aggregate type inference
                        match aggr {
                            crate::sql::AggregateFunction::Count => Ok(DataType::Int8),
                            crate::sql::AggregateFunction::Sum => {
                                if args.is_empty() {
                                    Ok(DataType::Float8)
                                } else {
                                    args[0].infer_type(schema)
                                }
                            }
                            crate::sql::AggregateFunction::Avg => Ok(DataType::Float8),
                            crate::sql::AggregateFunction::Min |
                            crate::sql::AggregateFunction::Max => {
                                if args.is_empty() {
                                    Ok(DataType::Float8)
                                } else {
                                    args[0].infer_type(schema)
                                }
                            }
                            crate::sql::AggregateFunction::JsonAgg => Ok(DataType::Jsonb),
                            crate::sql::AggregateFunction::ArrayAgg => {
                                if args.is_empty() {
                                    Ok(DataType::Array(Box::new(DataType::Text)))
                                } else {
                                    let elem_type = args[0].infer_type(schema)?;
                                    Ok(DataType::Array(Box::new(elem_type)))
                                }
                            }
                            crate::sql::AggregateFunction::StringAgg { .. } => Ok(DataType::Text),
                        }
                    }
                }
            }
        }
    }

    fn infer_nullable(&self, schema: &Schema) -> bool {
        match self {
            // Column: check schema for nullability
            LogicalExpr::Column { name, .. } => {
                schema.get_column(name)
                    .map(|col| col.nullable)
                    .unwrap_or(true) // Default to nullable if not found
            }

            // Literal: only NULL is nullable
            LogicalExpr::Literal(value) => {
                matches!(value, crate::Value::Null)
            }

            // Binary expressions: nullable if either operand is nullable
            LogicalExpr::BinaryExpr { left, right, .. } => {
                left.infer_nullable(schema) || right.infer_nullable(schema)
            }

            // Unary expressions: preserve operand nullability
            LogicalExpr::UnaryExpr { expr, .. } => {
                expr.infer_nullable(schema)
            }

            // Aggregate functions: generally nullable except COUNT(*)
            LogicalExpr::AggregateFunction { fun, args, .. } => {
                match fun {
                    AggregateFunction::Count => {
                        // COUNT(*) is never null, COUNT(col) can be
                        if args.is_empty() {
                            false // COUNT(*)
                        } else {
                            true // COUNT(col) - returns NULL if no rows
                        }
                    }
                    // Other aggregates can return NULL if no rows match
                    AggregateFunction::Sum | AggregateFunction::Avg |
                    AggregateFunction::Min | AggregateFunction::Max |
                    AggregateFunction::JsonAgg |
                    AggregateFunction::ArrayAgg |
                    AggregateFunction::StringAgg { .. } => true,
                }
            }

            // Scalar functions: generally nullable if any arg is nullable
            LogicalExpr::ScalarFunction { args, .. } => {
                args.iter().any(|arg| arg.infer_nullable(schema))
            }

            // CASE: nullable if any branch is nullable
            LogicalExpr::Case { when_then, else_result, .. } => {
                // Check all THEN branches
                let any_then_nullable = when_then.iter()
                    .any(|(_, then_expr)| then_expr.infer_nullable(schema));

                // Check ELSE branch
                let else_nullable = else_result.as_ref()
                    .map(|expr| expr.infer_nullable(schema))
                    .unwrap_or(true); // No ELSE means implicit NULL

                any_then_nullable || else_nullable
            }

            // CAST: preserve source nullability
            LogicalExpr::Cast { expr, .. } => {
                expr.infer_nullable(schema)
            }

            // IS NULL/IS NOT NULL: never nullable (always returns boolean)
            LogicalExpr::IsNull { .. } => false,

            // BETWEEN: never nullable (always returns boolean)
            LogicalExpr::Between { .. } => false,

            // IN list: never nullable (always returns boolean)
            LogicalExpr::InList { .. } => false,

            // IN set (HashSet-based): never nullable (always returns boolean)
            LogicalExpr::InSet { .. } => false,

            // IN subquery: never nullable (always returns boolean)
            LogicalExpr::InSubquery { .. } => false,

            // Scalar subquery: may return zero rows → NULL.
            LogicalExpr::ScalarSubquery { .. } => true,

            // EXISTS subquery: never nullable (always returns boolean)
            LogicalExpr::Exists { .. } => false,

            // Wildcard: treated as nullable
            LogicalExpr::Wildcard => true,

            // Parameter: treated as nullable (unknown)
            LogicalExpr::Parameter { .. } => true,

            // NEW row: check schema for nullability (same as column)
            LogicalExpr::NewRow { column } => {
                schema.get_column(column)
                    .map(|col| col.nullable)
                    .unwrap_or(true) // Default to nullable if not found
            }

            // OLD row: check schema for nullability (same as column)
            LogicalExpr::OldRow { column } => {
                schema.get_column(column)
                    .map(|col| col.nullable)
                    .unwrap_or(true) // Default to nullable if not found
            }

            // Array subscript: arr[n] is nullable (could be out of bounds or null input)
            LogicalExpr::ArraySubscript { .. } => true,

            // Row constructor: boolean comparison, nullable only if any
            // inner element is nullable.
            LogicalExpr::Tuple { items } => items.iter().any(|e| e.infer_nullable(schema)),

            // Window function: most are nullable
            LogicalExpr::WindowFunction { fun, .. } => {
                use super::logical_plan::WindowFunctionType;
                match fun {
                    // Ranking functions are never null
                    WindowFunctionType::RowNumber |
                    WindowFunctionType::Rank |
                    WindowFunctionType::DenseRank |
                    WindowFunctionType::Ntile => false,
                    // Statistical functions are never null
                    WindowFunctionType::PercentRank |
                    WindowFunctionType::CumeDist => false,
                    // Offset and value functions can return null
                    _ => true,
                }
            }
        }
    }

    fn to_column(&self, name: String, schema: &Schema) -> Column {
        let data_type = self.infer_type(schema)
            .unwrap_or(DataType::Text); // Fallback to Text if inference fails
        let nullable = self.infer_nullable(schema);

        Column {
            name,
            data_type,
            nullable,
            primary_key: false,
            source_table: None,
            source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: crate::ColumnStorageMode::Default,
        }
    }
}

/// Type coercion rules for binary operations
///
/// Implements SQL-standard type promotion and coercion rules.
///
/// # Arguments
///
/// * `left` - Left operand type
/// * `right` - Right operand type
/// * `op` - Binary operator
///
/// # Returns
///
/// The result type after type coercion or an error if types are incompatible
fn coerce_binary_types(left: DataType, right: DataType, op: &BinaryOperator) -> Result<DataType> {
    match op {
        // Comparison operators always return boolean
        BinaryOperator::Eq | BinaryOperator::NotEq |
        BinaryOperator::Lt | BinaryOperator::LtEq |
        BinaryOperator::Gt | BinaryOperator::GtEq => {
            Ok(DataType::Boolean)
        }

        // Logical operators require boolean operands and return boolean
        BinaryOperator::And | BinaryOperator::Or => {
            Ok(DataType::Boolean)
        }

        // String pattern matching operators
        BinaryOperator::Like | BinaryOperator::NotLike |
        BinaryOperator::ILike | BinaryOperator::NotILike |
        BinaryOperator::RegexMatch | BinaryOperator::RegexIMatch |
        BinaryOperator::NotRegexMatch | BinaryOperator::NotRegexIMatch |
        BinaryOperator::SimilarTo | BinaryOperator::NotSimilarTo |
        // FTS match
        BinaryOperator::TsMatch => {
            Ok(DataType::Boolean)
        }

        // Arithmetic operators: numeric type promotion
        BinaryOperator::Plus | BinaryOperator::Minus |
        BinaryOperator::Multiply | BinaryOperator::Divide |
        BinaryOperator::Modulo => {
            coerce_numeric_types(left, right)
        }

        // Vector similarity operators return float distance
        BinaryOperator::VectorL2Distance |
        BinaryOperator::VectorCosineDistance |
        BinaryOperator::VectorInnerProduct => {
            // Verify both operands are vectors
            match (&left, &right) {
                (DataType::Vector(_), DataType::Vector(_)) => Ok(DataType::Float8),
                _ => Err(Error::type_conversion(
                    format!("Vector operators require vector operands, got {:?} and {:?}", left, right)
                ))
            }
        }

        // JSONB operators
        BinaryOperator::JsonGet => {
            // -> returns JSON
            Ok(DataType::Jsonb)
        }
        BinaryOperator::JsonGetText => {
            // ->> returns text
            Ok(DataType::Text)
        }
        BinaryOperator::JsonContains |
        BinaryOperator::JsonContainedBy |
        BinaryOperator::JsonExists |
        BinaryOperator::JsonExistsAny |
        BinaryOperator::JsonExistsAll => {
            // All return boolean
            Ok(DataType::Boolean)
        }

        // String concatenation: || operator
        // If either operand is an array, treat as array concatenation
        // Otherwise returns text
        BinaryOperator::StringConcat => {
            match (&left, &right) {
                (DataType::Array(elem), DataType::Array(_)) => {
                    Ok(DataType::Array(elem.clone()))
                }
                (DataType::Array(elem), _) | (_, DataType::Array(elem)) => {
                    Ok(DataType::Array(elem.clone()))
                }
                _ => Ok(DataType::Text),
            }
        }

        // Array operators
        BinaryOperator::ArrayConcat => {
            // Both operands must be arrays of compatible types
            match (&left, &right) {
                (DataType::Array(left_elem), DataType::Array(right_elem)) => {
                    // Both are arrays - return array of left element type
                    // PostgreSQL behavior: array || array -> array
                    Ok(DataType::Array(left_elem.clone()))
                }
                (DataType::Array(elem), other) | (other, DataType::Array(elem)) => {
                    // One is array, other is scalar - wrap scalar in array
                    // PostgreSQL behavior: array || element -> array
                    Ok(DataType::Array(elem.clone()))
                }
                _ => Err(Error::type_conversion(
                    format!("Array concatenation requires at least one array operand, got {:?} and {:?}", left, right)
                ))
            }
        }
    }
}

/// Numeric type coercion following SQL standard rules
///
/// Type promotion hierarchy (lowest to highest):
/// Int2 -> Int4 -> Int8 -> Float4 -> Float8 -> Numeric
///
/// # Arguments
///
/// * `left` - Left operand type
/// * `right` - Right operand type
///
/// # Returns
///
/// The promoted numeric type or an error if not numeric
fn coerce_numeric_types(left: DataType, right: DataType) -> Result<DataType> {
    use DataType::*;

    match (left, right) {
        // If either is Numeric, result is Numeric
        (Numeric, _) | (_, Numeric) => Ok(Numeric),

        // If either is Float8, result is Float8
        (Float8, _) | (_, Float8) => Ok(Float8),

        // If either is Float4, result is Float4 (unless other is Int8)
        (Float4, Int8) | (Int8, Float4) => Ok(Float8),
        (Float4, _) | (_, Float4) => Ok(Float4),

        // Integer promotions
        (Int8, Int8) => Ok(Int8),
        (Int8, Int4) | (Int4, Int8) => Ok(Int8),
        (Int8, Int2) | (Int2, Int8) => Ok(Int8),
        (Int4, Int4) => Ok(Int4),
        (Int4, Int2) | (Int2, Int4) => Ok(Int4),
        (Int2, Int2) => Ok(Int2),

        // Text can be coerced for arithmetic (PostgreSQL behavior)
        (Text, n) | (n, Text) if is_numeric(&n) => Ok(n),

        // Non-numeric types in arithmetic
        (l, r) => Err(Error::type_conversion(
            format!("Cannot perform arithmetic on types {:?} and {:?}", l, r)
        ))
    }
}

/// Check if a type is numeric
fn is_numeric(data_type: &DataType) -> bool {
    matches!(data_type,
        DataType::Int2 | DataType::Int4 | DataType::Int8 |
        DataType::Float4 | DataType::Float8 | DataType::Numeric
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{Column, Value};

    fn test_schema() -> Schema {
        Schema::new(vec![
            Column::new("id", DataType::Int4),
            Column::new("name", DataType::Text),
            Column::new("price", DataType::Float8),
            Column::new("quantity", DataType::Int8),
            Column::new("active", DataType::Boolean),
            Column::new("embedding", DataType::Vector(128)),
        ])
    }

    #[test]
    fn test_column_type_inference() {
        let schema = test_schema();
        let expr = LogicalExpr::Column { table: None, name: "price".to_string()  };
        assert_eq!(expr.infer_type(&schema).unwrap(), DataType::Float8);
    }

    #[test]
    fn test_literal_type_inference() {
        let schema = test_schema();
        let expr = LogicalExpr::Literal(Value::Int4(42));
        assert_eq!(expr.infer_type(&schema).unwrap(), DataType::Int4);
    }

    #[test]
    fn test_aggregate_count() {
        let schema = test_schema();
        let expr = LogicalExpr::AggregateFunction {
            fun: AggregateFunction::Count,
            args: vec![LogicalExpr::Column { table: None, name: "id".to_string()  }],
            distinct: false,
        };
        assert_eq!(expr.infer_type(&schema).unwrap(), DataType::Int8);
    }

    #[test]
    fn test_aggregate_sum_preserves_type() {
        let schema = test_schema();
        let expr = LogicalExpr::AggregateFunction {
            fun: AggregateFunction::Sum,
            args: vec![LogicalExpr::Column { table: None, name: "price".to_string()  }],
            distinct: false,
        };
        assert_eq!(expr.infer_type(&schema).unwrap(), DataType::Float8);
    }

    #[test]
    fn test_aggregate_avg_returns_float() {
        let schema = test_schema();
        let expr = LogicalExpr::AggregateFunction {
            fun: AggregateFunction::Avg,
            args: vec![LogicalExpr::Column { table: None, name: "quantity".to_string()  }],
            distinct: false,
        };
        assert_eq!(expr.infer_type(&schema).unwrap(), DataType::Float8);
    }

    #[test]
    fn test_arithmetic_type_coercion() {
        let schema = test_schema();
        // Int4 + Float8 = Float8
        let expr = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column { table: None, name: "id".to_string()  }),
            op: BinaryOperator::Plus,
            right: Box::new(LogicalExpr::Column { table: None, name: "price".to_string()  }),
        };
        assert_eq!(expr.infer_type(&schema).unwrap(), DataType::Float8);
    }

    #[test]
    fn test_comparison_returns_boolean() {
        let schema = test_schema();
        let expr = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column { table: None, name: "price".to_string()  }),
            op: BinaryOperator::Gt,
            right: Box::new(LogicalExpr::Literal(Value::Float8(100.0))),
        };
        assert_eq!(expr.infer_type(&schema).unwrap(), DataType::Boolean);
    }

    #[test]
    fn test_cast_expression() {
        let schema = test_schema();
        let expr = LogicalExpr::Cast {
            expr: Box::new(LogicalExpr::Column { table: None, name: "id".to_string()  }),
            data_type: DataType::Text,
        };
        assert_eq!(expr.infer_type(&schema).unwrap(), DataType::Text);
    }

    #[test]
    fn test_is_null_returns_boolean() {
        let schema = test_schema();
        let expr = LogicalExpr::IsNull {
            expr: Box::new(LogicalExpr::Column { table: None, name: "name".to_string()  }),
            is_null: true,
        };
        assert_eq!(expr.infer_type(&schema).unwrap(), DataType::Boolean);
    }

    #[test]
    fn test_vector_distance_returns_float() {
        let schema = test_schema();
        let expr = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column { table: None, name: "embedding".to_string()  }),
            op: BinaryOperator::VectorL2Distance,
            right: Box::new(LogicalExpr::Column { table: None, name: "embedding".to_string()  }),
        };
        assert_eq!(expr.infer_type(&schema).unwrap(), DataType::Float8);
    }

    #[test]
    fn test_unknown_column_error() {
        let schema = test_schema();
        let expr = LogicalExpr::Column { table: None, name: "nonexistent".to_string()  };
        assert!(expr.infer_type(&schema).is_err());
    }

    // ===== Nullability Inference Tests =====

    #[test]
    fn test_nullable_column() {
        let schema = test_schema();
        let expr = LogicalExpr::Column { table: None, name: "name".to_string()  };
        assert!(expr.infer_nullable(&schema)); // name is nullable
    }

    #[test]
    fn test_non_nullable_literal() {
        let schema = test_schema();
        let expr = LogicalExpr::Literal(Value::Int4(42));
        assert!(!expr.infer_nullable(&schema)); // Literal is not nullable
    }

    #[test]
    fn test_nullable_literal() {
        let schema = test_schema();
        let expr = LogicalExpr::Literal(Value::Null);
        assert!(expr.infer_nullable(&schema)); // NULL is nullable
    }

    #[test]
    fn test_binary_expr_nullability() {
        let schema = test_schema();
        // nullable + non-nullable = nullable
        let expr = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column { table: None, name: "name".to_string()  }),
            op: BinaryOperator::Plus,
            right: Box::new(LogicalExpr::Literal(Value::Int4(1))),
        };
        assert!(expr.infer_nullable(&schema));
    }

    #[test]
    fn test_count_star_not_nullable() {
        let schema = test_schema();
        // COUNT(*) is never nullable
        let expr = LogicalExpr::AggregateFunction {
            fun: AggregateFunction::Count,
            args: vec![], // COUNT(*)
            distinct: false,
        };
        assert!(!expr.infer_nullable(&schema));
    }

    #[test]
    fn test_count_column_nullable() {
        let schema = test_schema();
        // COUNT(col) can return NULL if no rows
        let expr = LogicalExpr::AggregateFunction {
            fun: AggregateFunction::Count,
            args: vec![LogicalExpr::Column { table: None, name: "id".to_string()  }],
            distinct: false,
        };
        assert!(expr.infer_nullable(&schema));
    }

    #[test]
    fn test_sum_nullable() {
        let schema = test_schema();
        let expr = LogicalExpr::AggregateFunction {
            fun: AggregateFunction::Sum,
            args: vec![LogicalExpr::Column { table: None, name: "price".to_string()  }],
            distinct: false,
        };
        assert!(expr.infer_nullable(&schema)); // SUM returns NULL for empty set
    }

    #[test]
    fn test_is_null_not_nullable() {
        let schema = test_schema();
        let expr = LogicalExpr::IsNull {
            expr: Box::new(LogicalExpr::Column { table: None, name: "name".to_string()  }),
            is_null: true,
        };
        assert!(!expr.infer_nullable(&schema)); // IS NULL always returns boolean
    }

    #[test]
    fn test_case_without_else_nullable() {
        let schema = test_schema();
        let expr = LogicalExpr::Case {
            expr: None,
            when_then: vec![(
                LogicalExpr::Literal(Value::Boolean(true)),
                LogicalExpr::Literal(Value::Int4(1)),
            )],
            else_result: None, // No ELSE = implicit NULL
        };
        assert!(expr.infer_nullable(&schema));
    }

    #[test]
    fn test_case_with_nullable_branch() {
        let schema = test_schema();
        let expr = LogicalExpr::Case {
            expr: None,
            when_then: vec![(
                LogicalExpr::Literal(Value::Boolean(true)),
                LogicalExpr::Literal(Value::Null), // Nullable branch
            )],
            else_result: Some(Box::new(LogicalExpr::Literal(Value::Int4(0)))),
        };
        assert!(expr.infer_nullable(&schema));
    }

    #[test]
    fn test_cast_preserves_nullability() {
        let schema = test_schema();
        let expr = LogicalExpr::Cast {
            expr: Box::new(LogicalExpr::Column { table: None, name: "name".to_string()  }),
            data_type: DataType::Int4,
        };
        assert!(expr.infer_nullable(&schema)); // Cast preserves nullability
    }

    // ===== to_column() Tests =====

    #[test]
    fn test_to_column_from_literal() {
        let schema = test_schema();
        let expr = LogicalExpr::Literal(Value::Int4(42));
        let col = expr.to_column("test_col".to_string(), &schema);

        assert_eq!(col.name, "test_col");
        assert_eq!(col.data_type, DataType::Int4);
        assert!(!col.nullable);
        assert!(!col.primary_key);
    }

    #[test]
    fn test_to_column_from_nullable_column() {
        let schema = test_schema();
        let expr = LogicalExpr::Column { table: None, name: "name".to_string()  };
        let col = expr.to_column("result".to_string(), &schema);

        assert_eq!(col.name, "result");
        assert_eq!(col.data_type, DataType::Text);
        assert!(col.nullable);
    }

    #[test]
    fn test_to_column_from_aggregate() {
        let schema = test_schema();
        let expr = LogicalExpr::AggregateFunction {
            fun: AggregateFunction::Avg,
            args: vec![LogicalExpr::Column { table: None, name: "price".to_string()  }],
            distinct: false,
        };
        let col = expr.to_column("avg_price".to_string(), &schema);

        assert_eq!(col.name, "avg_price");
        assert_eq!(col.data_type, DataType::Float8);
        assert!(col.nullable);
    }

    #[test]
    fn test_to_column_from_arithmetic() {
        let schema = test_schema();
        // price * quantity (Float8 * Int8 = Float8)
        let expr = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column { table: None, name: "price".to_string()  }),
            op: BinaryOperator::Multiply,
            right: Box::new(LogicalExpr::Column { table: None, name: "quantity".to_string()  }),
        };
        let col = expr.to_column("total".to_string(), &schema);

        assert_eq!(col.name, "total");
        assert_eq!(col.data_type, DataType::Float8);
        assert!(col.nullable); // Either operand could be nullable
    }

    // ===== Complex Expression Tests =====

    #[test]
    fn test_nested_arithmetic_coercion() {
        let schema = test_schema();
        // id + (price * 2.0) -> Int4 + Float8 = Float8
        let expr = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column { table: None, name: "id".to_string()  }),
            op: BinaryOperator::Plus,
            right: Box::new(LogicalExpr::BinaryExpr {
                left: Box::new(LogicalExpr::Column { table: None, name: "price".to_string()  }),
                op: BinaryOperator::Multiply,
                right: Box::new(LogicalExpr::Literal(Value::Float8(2.0))),
            }),
        };
        assert_eq!(expr.infer_type(&schema).unwrap(), DataType::Float8);
    }

    #[test]
    fn test_scalar_function_string_length() {
        let schema = test_schema();
        let expr = LogicalExpr::ScalarFunction {
            fun: "length".to_string(),
            args: vec![LogicalExpr::Column { table: None, name: "name".to_string()  }],
        };
        assert_eq!(expr.infer_type(&schema).unwrap(), DataType::Int8);
        assert!(expr.infer_nullable(&schema)); // Function arg is nullable
    }

    #[test]
    fn test_scalar_function_concat() {
        let schema = test_schema();
        let expr = LogicalExpr::ScalarFunction {
            fun: "concat".to_string(),
            args: vec![
                LogicalExpr::Column { table: None, name: "name".to_string()  },
                LogicalExpr::Literal(Value::String(" suffix".to_string())),
            ],
        };
        assert_eq!(expr.infer_type(&schema).unwrap(), DataType::Text);
    }

    #[test]
    fn test_multiple_aggregates_distinct_types() {
        let schema = test_schema();

        // COUNT(*)
        let count_expr = LogicalExpr::AggregateFunction {
            fun: AggregateFunction::Count,
            args: vec![],
            distinct: false,
        };
        assert_eq!(count_expr.infer_type(&schema).unwrap(), DataType::Int8);
        assert!(!count_expr.infer_nullable(&schema));

        // SUM(quantity)
        let sum_expr = LogicalExpr::AggregateFunction {
            fun: AggregateFunction::Sum,
            args: vec![LogicalExpr::Column { table: None, name: "quantity".to_string()  }],
            distinct: false,
        };
        assert_eq!(sum_expr.infer_type(&schema).unwrap(), DataType::Int8);
        assert!(sum_expr.infer_nullable(&schema));

        // AVG(price)
        let avg_expr = LogicalExpr::AggregateFunction {
            fun: AggregateFunction::Avg,
            args: vec![LogicalExpr::Column { table: None, name: "price".to_string()  }],
            distinct: false,
        };
        assert_eq!(avg_expr.infer_type(&schema).unwrap(), DataType::Float8);
        assert!(avg_expr.infer_nullable(&schema));
    }

    #[test]
    fn test_json_operators() {
        let mut schema_cols = test_schema().columns;
        schema_cols.push(Column::new("metadata", DataType::Jsonb));
        let schema = Schema::new(schema_cols);

        // -> returns JSONB
        let json_get = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column { table: None, name: "metadata".to_string()  }),
            op: BinaryOperator::JsonGet,
            right: Box::new(LogicalExpr::Literal(Value::String("key".to_string()))),
        };
        assert_eq!(json_get.infer_type(&schema).unwrap(), DataType::Jsonb);

        // ->> returns TEXT
        let json_get_text = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column { table: None, name: "metadata".to_string()  }),
            op: BinaryOperator::JsonGetText,
            right: Box::new(LogicalExpr::Literal(Value::String("key".to_string()))),
        };
        assert_eq!(json_get_text.infer_type(&schema).unwrap(), DataType::Text);

        // @> returns BOOLEAN
        let json_contains = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column { table: None, name: "metadata".to_string()  }),
            op: BinaryOperator::JsonContains,
            right: Box::new(LogicalExpr::Literal(Value::Json("{}".to_string()))),
        };
        assert_eq!(json_contains.infer_type(&schema).unwrap(), DataType::Boolean);
    }
}
