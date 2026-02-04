//! Expression evaluation
//!
//! Evaluates logical expressions against tuples to produce values.

use crate::{Result, Error, Value, Tuple, Schema, DataType};
use crate::tenant::{get_current_tenant_id, get_current_user_id};
use super::LogicalExpr;
use chrono::{Utc, Local};
use std::sync::Arc;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

/// Expression evaluator
///
/// Evaluates expressions in the context of a tuple and schema.
pub struct Evaluator {
    schema: Arc<Schema>,
    /// Parameter values for parameterized queries ($1, $2, etc.)
    parameters: Vec<Value>,
    /// Trigger row context for NEW and OLD row access (only set during trigger execution)
    /// Uses the TriggerRowContext from triggers module
    trigger_row_context: Option<(super::triggers::TriggerRowContext, Arc<Schema>)>,
}

impl Evaluator {
    /// Create a new evaluator with the given schema
    pub fn new(schema: Arc<Schema>) -> Self {
        Self {
            schema,
            parameters: Vec::new(),
            trigger_row_context: None,
        }
    }

    /// Create a new evaluator with schema and parameters
    pub fn with_parameters(schema: Arc<Schema>, parameters: Vec<Value>) -> Self {
        Self {
            schema,
            parameters,
            trigger_row_context: None,
        }
    }

    /// Create a new evaluator with trigger row context
    ///
    /// # Arguments
    ///
    /// * `schema` - The schema context for evaluating expressions
    /// * `parameters` - Query parameters
    /// * `trigger_row_context` - The trigger row context with NEW/OLD tuples
    /// * `row_schema` - The schema of the NEW/OLD rows (usually the table schema)
    pub fn with_trigger_row_context(
        schema: Arc<Schema>,
        parameters: Vec<Value>,
        trigger_row_context: super::triggers::TriggerRowContext,
        row_schema: Arc<Schema>,
    ) -> Self {
        Self {
            schema,
            parameters,
            trigger_row_context: Some((trigger_row_context, row_schema)),
        }
    }

    /// Get the schema
    pub fn schema(&self) -> &Arc<Schema> {
        &self.schema
    }

    /// Evaluate an expression against a tuple
    pub fn evaluate(&self, expr: &LogicalExpr, tuple: &Tuple) -> Result<Value> {
        match expr {
            LogicalExpr::Literal(value) => Ok(value.clone()),

            LogicalExpr::Parameter { index } => {
                // PostgreSQL uses 1-based parameter indices
                if *index == 0 {
                    return Err(Error::query_execution(
                        "Parameter indices must be 1-based (e.g., $1, $2)"
                    ));
                }

                // Convert to 0-based index for Vec
                let zero_based_index = index - 1;

                self.parameters.get(zero_based_index)
                    .cloned()
                    .ok_or_else(|| Error::query_execution(format!(
                        "Parameter ${} not provided. Expected {} parameters, got {}",
                        index,
                        index,
                        self.parameters.len()
                    )))
            }

            LogicalExpr::Column { table, name } => {
                // Find column index in schema, using table qualifier for disambiguation if provided
                let index = self.schema.get_qualified_column_index(table.as_deref(), name)
                    .ok_or_else(|| Error::query_execution(format!(
                        "Column '{}' not found in schema",
                        if let Some(t) = table { format!("{}.{}", t, name) } else { name.clone() }
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
                let left_val = self.evaluate(left, tuple)?;
                let right_val = self.evaluate(right, tuple)?;
                self.evaluate_binary_op(&left_val, op, &right_val)
            }

            LogicalExpr::UnaryExpr { op, expr } => {
                let val = self.evaluate(expr, tuple)?;
                self.evaluate_unary_op(op, &val)
            }

            LogicalExpr::IsNull { expr, is_null } => {
                let val = self.evaluate(expr, tuple)?;
                let is_actually_null = matches!(val, Value::Null);
                // is_null is true for IS NULL, false for IS NOT NULL
                Ok(Value::Boolean(is_actually_null == *is_null))
            }

            LogicalExpr::ScalarFunction { fun, args } => {
                self.evaluate_scalar_function(fun, args, tuple)
            }

            LogicalExpr::Cast { expr, data_type } => {
                let value = self.evaluate(expr, tuple)?;
                self.cast_value(value, data_type)
            }

            LogicalExpr::Wildcard => {
                // Wildcards should be expanded during planning, not evaluation
                Err(Error::query_execution(
                    "Wildcard expressions should be expanded before evaluation"
                ))
            }

            LogicalExpr::NewRow { column } => {
                // Access NEW row from trigger row context
                let (ctx, row_schema) = self.trigger_row_context.as_ref()
                    .ok_or_else(|| Error::query_execution(
                        "NEW is only valid in trigger context"
                    ))?;

                let new_tuple = ctx.new_tuple.as_ref()
                    .ok_or_else(|| Error::query_execution(
                        "NEW is not available in this trigger (DELETE triggers only have OLD)"
                    ))?;

                // Find column index in trigger row schema
                let index = row_schema.get_column_index(column)
                    .ok_or_else(|| Error::query_execution(format!(
                        "Column '{}' not found in NEW row",
                        column
                    )))?;

                // Get value from NEW tuple
                new_tuple.get(index)
                    .cloned()
                    .ok_or_else(|| Error::query_execution(format!(
                        "Column index {} out of bounds in NEW row",
                        index
                    )))
            }

            LogicalExpr::OldRow { column } => {
                // Access OLD row from trigger row context
                let (ctx, row_schema) = self.trigger_row_context.as_ref()
                    .ok_or_else(|| Error::query_execution(
                        "OLD is only valid in trigger context"
                    ))?;

                let old_tuple = ctx.old_tuple.as_ref()
                    .ok_or_else(|| Error::query_execution(
                        "OLD is not available in this trigger (INSERT triggers only have NEW)"
                    ))?;

                // Find column index in trigger row schema
                let index = row_schema.get_column_index(column)
                    .ok_or_else(|| Error::query_execution(format!(
                        "Column '{}' not found in OLD row",
                        column
                    )))?;

                // Get value from OLD tuple
                old_tuple.get(index)
                    .cloned()
                    .ok_or_else(|| Error::query_execution(format!(
                        "Column index {} out of bounds in OLD row",
                        index
                    )))
            }

            LogicalExpr::ArraySubscript { array, index } => {
                let array_val = self.evaluate(array, tuple)?;
                let index_val = self.evaluate(index, tuple)?;
                self.evaluate_array_subscript(&array_val, &index_val)
            }

            LogicalExpr::InList { expr, list, negated } => {
                let value = self.evaluate(expr, tuple)?;

                // Check if value is NULL - SQL semantics: NULL IN (...) = NULL
                if matches!(value, Value::Null) {
                    return Ok(Value::Null);
                }

                // Evaluate all list items and check for membership
                let mut found = false;
                let mut has_null = false;

                for item in list {
                    let item_value = self.evaluate(item, tuple)?;
                    if matches!(item_value, Value::Null) {
                        has_null = true;
                        continue;
                    }
                    if self.values_equal(&value, &item_value) {
                        found = true;
                        break;
                    }
                }

                // SQL semantics for IN with NULLs:
                // - If found, result is true (or false for NOT IN)
                // - If not found and list has NULL, result is NULL
                // - If not found and no NULL, result is false (or true for NOT IN)
                let result = if found {
                    !*negated
                } else if has_null {
                    return Ok(Value::Null);
                } else {
                    *negated
                };

                Ok(Value::Boolean(result))
            }

            LogicalExpr::InSubquery { .. } => {
                // Subquery evaluation requires executor context
                // This should be handled at the executor level, not evaluator
                Err(Error::query_execution(
                    "IN subquery evaluation requires executor context. Use executor for subquery evaluation."
                ))
            }

            LogicalExpr::Exists { .. } => {
                // EXISTS evaluation requires executor context
                // This should be handled at the executor level, not evaluator
                Err(Error::query_execution(
                    "EXISTS subquery evaluation requires executor context. Use executor for subquery evaluation."
                ))
            }

            _ => Err(Error::query_execution(format!(
                "Expression not yet implemented: {:?}",
                expr
            ))),
        }
    }

    /// Evaluate a scalar function
    fn evaluate_scalar_function(
        &self,
        fun: &str,
        args: &[LogicalExpr],
        tuple: &Tuple,
    ) -> Result<Value> {
        // Evaluate all arguments
        let arg_values: Result<Vec<Value>> = args.iter()
            .map(|arg| self.evaluate(arg, tuple))
            .collect();
        let arg_values = arg_values?;

        match fun.to_lowercase().as_str() {
            // JSONB extraction functions
            "jsonb_extract_path" | "json_extract_path" => {
                self.jsonb_extract_path(&arg_values)
            }
            "jsonb_extract_path_text" | "json_extract_path_text" => {
                self.jsonb_extract_path_text(&arg_values)
            }

            // JSONB array functions
            "jsonb_array_elements" => {
                self.jsonb_array_elements(&arg_values)
            }
            "jsonb_array_elements_text" => {
                self.jsonb_array_elements_text(&arg_values)
            }

            // JSONB object functions
            "jsonb_object_keys" => {
                self.jsonb_object_keys(&arg_values)
            }

            // JSONB aggregation
            "jsonb_array_length" => {
                self.jsonb_array_length(&arg_values)
            }

            // JSONB type check
            "jsonb_typeof" => {
                self.jsonb_typeof(&arg_values)
            }

            // JSONB path query (basic support)
            "jsonb_path_query" => {
                self.jsonb_path_query(&arg_values)
            }

            // JSONB construction functions (Phase 1)
            "jsonb_build_object" | "json_build_object" => {
                self.jsonb_build_object(&arg_values)
            }
            "jsonb_build_array" | "json_build_array" => {
                self.jsonb_build_array(&arg_values)
            }
            "jsonb_set" | "json_set" => {
                self.jsonb_set(&arg_values)
            }
            "jsonb_concat" => {
                self.jsonb_concat(&arg_values)
            }
            "jsonb_delete" => {
                self.jsonb_delete(&arg_values)
            }
            "jsonb_each" => {
                self.jsonb_each(&arg_values)
            }
            "jsonb_each_text" => {
                self.jsonb_each_text(&arg_values)
            }

            // Vector distance functions
            "cosine_similarity" => {
                self.vector_cosine_similarity(&arg_values)
            }
            "cosine_distance" => {
                self.vector_cosine_distance(&arg_values)
            }
            "l2_distance" | "euclidean_distance" => {
                self.vector_l2_distance(&arg_values)
            }
            "inner_product" => {
                self.vector_inner_product(&arg_values)
            }

            // Date/Time functions - PostgreSQL, Oracle, SQL Server, MySQL compatible aliases
            "current_timestamp" | "now" | "sysdate" | "getdate" | "systimestamp" | "sysdatetime"
            | "getutcdate" | "utc_timestamp" => {
                // Return current timestamp in UTC
                Ok(Value::Timestamp(Utc::now()))
            }
            "current_date" | "curdate" => {
                // Return current date (without time)
                Ok(Value::Date(Utc::now().date_naive()))
            }
            "current_time" | "curtime" => {
                // Return current time (without date)
                Ok(Value::Time(Utc::now().time()))
            }
            "localtimestamp" | "localtime" => {
                // Return local timestamp (using local timezone, stored as UTC equivalent)
                Ok(Value::Timestamp(Local::now().with_timezone(&Utc)))
            }

            // Multi-tenant context functions
            "current_tenant" | "current_tenant_id" => {
                // Return the current tenant ID from thread-local storage
                if let Some(tenant_id) = get_current_tenant_id() {
                    Ok(Value::String(tenant_id.to_string()))
                } else {
                    // No tenant context set - return NULL (allows queries to run without tenant)
                    Ok(Value::Null)
                }
            }

            "current_user_id" => {
                // Return the current user ID from thread-local storage
                if let Some(user_id) = get_current_user_id() {
                    Ok(Value::String(user_id))
                } else {
                    Ok(Value::Null)
                }
            }

            _ => Err(Error::query_execution(format!(
                "Unknown scalar function: {}",
                fun
            ))),
        }
    }

    /// jsonb_extract_path(json, path_elements...)
    /// Extract JSON sub-object at the specified path
    fn jsonb_extract_path(&self, args: &[Value]) -> Result<Value> {
        if args.is_empty() {
            return Err(Error::query_execution(
                "jsonb_extract_path requires at least one argument"
            ));
        }

        let json_str = match &args[0] {
            Value::Json(j) => j,
            Value::Null => return Ok(Value::Null),
            _ => return Err(Error::query_execution(
                "First argument must be JSON"
            )),
        };

        // Parse the JSON string
        let mut current: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| Error::query_execution(format!("Invalid JSON: {}", e)))?;

        // Navigate through the path
        for path_elem in &args[1..] {
            match path_elem {
                Value::String(key) => {
                    current = match current.get(key) {
                        Some(v) => v.clone(),
                        None => return Ok(Value::Null),
                    };
                }
                Value::Int4(idx) => {
                    if let Some(arr) = current.as_array() {
                        let index = if *idx < 0 {
                            (arr.len() as i32 + idx) as usize
                        } else {
                            *idx as usize
                        };
                        current = match arr.get(index) {
                            Some(v) => v.clone(),
                            None => return Ok(Value::Null),
                        };
                    } else {
                        return Ok(Value::Null);
                    }
                }
                _ => return Err(Error::query_execution(
                    "Path elements must be strings or integers"
                )),
            }
        }

        Ok(Value::Json(current.to_string()))
    }

    /// jsonb_extract_path_text(json, path_elements...)
    /// Extract JSON sub-object at the specified path as text
    fn jsonb_extract_path_text(&self, args: &[Value]) -> Result<Value> {
        let result = self.jsonb_extract_path(args)?;
        match result {
            Value::Json(j) => {
                // Parse the JSON string to check if it's a string value
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&j) {
                    match parsed {
                        serde_json::Value::String(s) => Ok(Value::String(s)),
                        _ => Ok(Value::String(j)),
                    }
                } else {
                    Ok(Value::String(j))
                }
            },
            Value::Null => Ok(Value::Null),
            _ => Ok(Value::String(result.to_string())),
        }
    }

    /// jsonb_array_elements(json)
    /// Expands JSON array to set of JSON values (returns first element for now)
    fn jsonb_array_elements(&self, args: &[Value]) -> Result<Value> {
        if args.len() != 1 {
            return Err(Error::query_execution(
                "jsonb_array_elements requires exactly one argument"
            ));
        }

        let json_str = match &args[0] {
            Value::Json(j) => j,
            Value::Null => return Ok(Value::Null),
            _ => return Err(Error::query_execution(
                "Argument must be JSON"
            )),
        };

        // Parse JSON string to serde_json::Value
        let json_val: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| Error::query_execution(format!("Invalid JSON: {}", e)))?;

        if let Some(arr) = json_val.as_array() {
            if let Some(first) = arr.first() {
                Ok(Value::Json(first.to_string()))
            } else {
                Ok(Value::Null)
            }
        } else {
            Err(Error::query_execution(
                "Argument must be a JSON array"
            ))
        }
    }

    /// jsonb_array_elements_text(json)
    /// Expands JSON array to set of text values
    fn jsonb_array_elements_text(&self, args: &[Value]) -> Result<Value> {
        let result = self.jsonb_array_elements(args)?;
        match result {
            Value::Json(json_str) => {
                // Parse the JSON string to extract text value
                let json_val: serde_json::Value = serde_json::from_str(&json_str)
                    .map_err(|e| Error::query_execution(format!("Invalid JSON: {}", e)))?;

                match json_val {
                    serde_json::Value::String(s) => Ok(Value::String(s)),
                    _ => Ok(Value::String(json_val.to_string())),
                }
            },
            other => Ok(other),
        }
    }

    /// jsonb_object_keys(json)
    /// Returns set of keys in the JSON object (returns array for now)
    fn jsonb_object_keys(&self, args: &[Value]) -> Result<Value> {
        if args.len() != 1 {
            return Err(Error::query_execution(
                "jsonb_object_keys requires exactly one argument"
            ));
        }

        let json_str = match &args[0] {
            Value::Json(j) => j,
            Value::Null => return Ok(Value::Null),
            _ => return Err(Error::query_execution(
                "Argument must be JSON"
            )),
        };

        // Parse JSON string to serde_json::Value
        let json_val: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| Error::query_execution(format!("Invalid JSON: {}", e)))?;

        if let Some(obj) = json_val.as_object() {
            let keys: Vec<Value> = obj.keys()
                .map(|k| Value::String(k.clone()))
                .collect();
            Ok(Value::Array(keys))
        } else {
            Err(Error::query_execution(
                "Argument must be a JSON object"
            ))
        }
    }

    /// jsonb_array_length(json)
    /// Returns the number of elements in the JSON array
    fn jsonb_array_length(&self, args: &[Value]) -> Result<Value> {
        if args.len() != 1 {
            return Err(Error::query_execution(
                "jsonb_array_length requires exactly one argument"
            ));
        }

        let json_str = match &args[0] {
            Value::Json(j) => j,
            Value::Null => return Ok(Value::Null),
            _ => return Err(Error::query_execution(
                "Argument must be JSON"
            )),
        };

        // Parse JSON string to serde_json::Value
        let json_val: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| Error::query_execution(format!("Invalid JSON: {}", e)))?;

        if let Some(arr) = json_val.as_array() {
            Ok(Value::Int4(arr.len() as i32))
        } else {
            Err(Error::query_execution(
                "Argument must be a JSON array"
            ))
        }
    }

    /// jsonb_typeof(json)
    /// Returns the type of the JSON value as text
    fn jsonb_typeof(&self, args: &[Value]) -> Result<Value> {
        if args.len() != 1 {
            return Err(Error::query_execution(
                "jsonb_typeof requires exactly one argument"
            ));
        }

        let json_str = match &args[0] {
            Value::Json(j) => j,
            Value::Null => return Ok(Value::String("null".to_string())),
            _ => return Err(Error::query_execution(
                "Argument must be JSON"
            )),
        };

        // Parse JSON string to serde_json::Value
        let json_val: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| Error::query_execution(format!("Invalid JSON: {}", e)))?;

        let type_name = match json_val {
            serde_json::Value::Null => "null",
            serde_json::Value::Bool(_) => "boolean",
            serde_json::Value::Number(_) => "number",
            serde_json::Value::String(_) => "string",
            serde_json::Value::Array(_) => "array",
            serde_json::Value::Object(_) => "object",
        };

        Ok(Value::String(type_name.to_string()))
    }

    /// jsonb_path_query(json, path)
    /// Basic JSON path query support (simplified)
    fn jsonb_path_query(&self, args: &[Value]) -> Result<Value> {
        if args.len() != 2 {
            return Err(Error::query_execution(
                "jsonb_path_query requires exactly two arguments"
            ));
        }

        let json_str = match &args[0] {
            Value::Json(j) => j,
            Value::Null => return Ok(Value::Null),
            _ => return Err(Error::query_execution(
                "First argument must be JSON"
            )),
        };

        let path = match &args[1] {
            Value::String(s) => s,
            _ => return Err(Error::query_execution(
                "Second argument must be string (JSON path)"
            )),
        };

        // Parse JSON string to serde_json::Value
        let json_val: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| Error::query_execution(format!("Invalid JSON: {}", e)))?;

        // Simple path parsing: split by '.' and navigate
        let mut current = &json_val;
        for key in path.split('.') {
            let key = key.trim();
            if key.is_empty() {
                continue;
            }

            // Handle array index notation [n]
            if key.starts_with('[') && key.ends_with(']') {
                if let Ok(idx) = key[1..key.len()-1].parse::<usize>() {
                    if let Some(arr) = current.as_array() {
                        current = match arr.get(idx) {
                            Some(v) => v,
                            None => return Ok(Value::Null),
                        };
                        continue;
                    } else {
                        return Ok(Value::Null);
                    }
                }
            }

            // Object key access
            current = match current.get(key) {
                Some(v) => v,
                None => return Ok(Value::Null),
            };
        }

        Ok(Value::Json(current.to_string()))
    }

    /// jsonb_build_object(key1, val1, key2, val2, ...)
    /// Constructs a JSONB object from alternating key-value pairs
    fn jsonb_build_object(&self, args: &[Value]) -> Result<Value> {
        if args.len() % 2 != 0 {
            return Err(Error::query_execution(
                "jsonb_build_object requires an even number of arguments (key-value pairs)"
            ));
        }

        let mut obj = serde_json::json!({});

        for i in (0..args.len()).step_by(2) {
            // Convert key to string
            let key = match &args[i] {
                Value::String(s) => s.clone(),
                Value::Null => continue, // Skip null keys
                other => other.to_string().trim_matches('\'').to_string(),
            };

            let value = &args[i + 1];

            // Convert value to serde_json::Value
            let json_val = match value {
                Value::Null => serde_json::json!(null),
                Value::Boolean(b) => serde_json::json!(b),
                Value::Int2(i) => serde_json::json!(i),
                Value::Int4(i) => serde_json::json!(i),
                Value::Int8(i) => serde_json::json!(i),
                Value::Float4(f) => serde_json::json!(f),
                Value::Float8(f) => serde_json::json!(f),
                Value::Numeric(n) => {
                    // Try to parse as number, fallback to string
                    if let Ok(num) = n.parse::<f64>() {
                        serde_json::json!(num)
                    } else {
                        serde_json::json!(n.as_str())
                    }
                }
                Value::String(s) => serde_json::json!(s),
                Value::Bytes(b) => {
                    // Encode bytes as hex string
                    let hex = b.iter().map(|byte| format!("{:02x}", byte)).collect::<String>();
                    serde_json::json!(hex)
                }
                Value::Uuid(u) => serde_json::json!(u.to_string()),
                Value::Timestamp(ts) => serde_json::json!(ts.to_rfc3339()),
                Value::Date(d) => serde_json::json!(d.format("%Y-%m-%d").to_string()),
                Value::Time(t) => serde_json::json!(t.format("%H:%M:%S%.f").to_string()),
                Value::Json(j) => {
                    // Parse JSON string
                    serde_json::from_str(j).unwrap_or_else(|_| serde_json::json!(j.as_str()))
                }
                Value::Array(arr) => {
                    // Convert array to JSON array
                    let json_arr: Vec<serde_json::Value> = arr.iter().map(|v| {
                        match v {
                            Value::Null => serde_json::json!(null),
                            Value::Boolean(b) => serde_json::json!(b),
                            Value::Int2(i) => serde_json::json!(i),
                            Value::Int4(i) => serde_json::json!(i),
                            Value::Int8(i) => serde_json::json!(i),
                            Value::Float4(f) => serde_json::json!(f),
                            Value::Float8(f) => serde_json::json!(f),
                            Value::String(s) => serde_json::json!(s),
                            _ => serde_json::json!(v.to_string()),
                        }
                    }).collect();
                    serde_json::json!(json_arr)
                }
                Value::Vector(_) => {
                    // Convert vector to JSON array
                    return Err(Error::query_execution("Vector type not supported in jsonb_build_object"));
                }
                // Storage references (should be resolved before reaching here)
                Value::DictRef { dict_id } => serde_json::json!(format!("dict:{}", dict_id)),
                Value::CasRef { hash } => serde_json::json!(format!("cas:{}", hex::encode(hash))),
                Value::ColumnarRef => serde_json::json!("columnar_ref"),
            };

            obj[key] = json_val;
        }

        Ok(Value::Json(obj.to_string()))
    }

    /// jsonb_build_array(val1, val2, ...)
    /// Constructs a JSONB array from individual values
    fn jsonb_build_array(&self, args: &[Value]) -> Result<Value> {
        let mut arr = Vec::new();

        for value in args {
            let json_val = match value {
                Value::Null => serde_json::json!(null),
                Value::Boolean(b) => serde_json::json!(b),
                Value::Int2(i) => serde_json::json!(i),
                Value::Int4(i) => serde_json::json!(i),
                Value::Int8(i) => serde_json::json!(i),
                Value::Float4(f) => serde_json::json!(f),
                Value::Float8(f) => serde_json::json!(f),
                Value::Numeric(n) => {
                    if let Ok(num) = n.parse::<f64>() {
                        serde_json::json!(num)
                    } else {
                        serde_json::json!(n.as_str())
                    }
                }
                Value::String(s) => serde_json::json!(s),
                Value::Bytes(b) => {
                    let hex = b.iter().map(|byte| format!("{:02x}", byte)).collect::<String>();
                    serde_json::json!(hex)
                }
                Value::Uuid(u) => serde_json::json!(u.to_string()),
                Value::Timestamp(ts) => serde_json::json!(ts.to_rfc3339()),
                Value::Date(d) => serde_json::json!(d.format("%Y-%m-%d").to_string()),
                Value::Time(t) => serde_json::json!(t.format("%H:%M:%S%.f").to_string()),
                Value::Json(j) => {
                    serde_json::from_str(j).unwrap_or_else(|_| serde_json::json!(j.as_str()))
                }
                Value::Array(inner) => {
                    let json_arr: Vec<serde_json::Value> = inner.iter().map(|v| {
                        match v {
                            Value::Null => serde_json::json!(null),
                            Value::Boolean(b) => serde_json::json!(b),
                            Value::Int2(i) => serde_json::json!(i),
                            Value::Int4(i) => serde_json::json!(i),
                            Value::Int8(i) => serde_json::json!(i),
                            Value::Float4(f) => serde_json::json!(f),
                            Value::Float8(f) => serde_json::json!(f),
                            Value::String(s) => serde_json::json!(s),
                            _ => serde_json::json!(v.to_string()),
                        }
                    }).collect();
                    serde_json::json!(json_arr)
                }
                Value::Vector(_) => {
                    return Err(Error::query_execution("Vector type not supported in jsonb_build_array"));
                }
                // Storage references (should be resolved before reaching here)
                Value::DictRef { dict_id } => serde_json::json!(format!("dict:{}", dict_id)),
                Value::CasRef { hash } => serde_json::json!(format!("cas:{}", hex::encode(hash))),
                Value::ColumnarRef => serde_json::json!("columnar_ref"),
            };

            arr.push(json_val);
        }

        Ok(Value::Json(serde_json::json!(arr).to_string()))
    }

    /// jsonb_set(jsonb, path_array, new_value, [create_missing])
    /// Sets the value at the specified path in a JSONB object
    fn jsonb_set(&self, args: &[Value]) -> Result<Value> {
        if args.len() < 3 || args.len() > 4 {
            return Err(Error::query_execution(
                "jsonb_set requires 3 or 4 arguments: jsonb_set(target, path_array, new_value, [create_missing])"
            ));
        }

        let json_str = match &args[0] {
            Value::Json(j) => j.clone(),
            Value::Null => return Ok(Value::Null),
            _ => return Err(Error::query_execution("First argument must be JSON")),
        };

        let path_arr = match &args[1] {
            Value::Array(arr) => arr,
            _ => return Err(Error::query_execution("Second argument must be an array (path)")),
        };

        let create_missing = if args.len() == 4 {
            match &args[3] {
                Value::Boolean(b) => *b,
                _ => true,
            }
        } else {
            true
        };

        // Parse path elements
        let mut path = Vec::new();
        for elem in path_arr {
            match elem {
                Value::String(s) => path.push(s.clone()),
                Value::Int4(i) => path.push(i.to_string()),
                Value::Int8(i) => path.push(i.to_string()),
                _ => {
                    return Err(Error::query_execution(
                        "Path array elements must be strings or integers"
                    ))
                }
            }
        }

        let mut current: serde_json::Value = serde_json::from_str(&json_str)
            .map_err(|e| Error::query_execution(format!("Invalid JSON: {}", e)))?;

        // Convert new_value to JSON
        let new_val = match &args[2] {
            Value::Null => serde_json::json!(null),
            Value::Boolean(b) => serde_json::json!(b),
            Value::Int2(i) => serde_json::json!(i),
            Value::Int4(i) => serde_json::json!(i),
            Value::Int8(i) => serde_json::json!(i),
            Value::Float4(f) => serde_json::json!(f),
            Value::Float8(f) => serde_json::json!(f),
            Value::String(s) => serde_json::json!(s),
            Value::Uuid(u) => serde_json::json!(u.to_string()),
            Value::Json(j) => serde_json::from_str(j).unwrap_or_else(|_| serde_json::json!(j.as_str())),
            _ => serde_json::json!(args[2].to_string()),
        };

        // Navigate and set the value
        if !path.is_empty() {
            self.jsonb_set_recursive(&mut current, &path, 0, &new_val, create_missing)?;
        }

        Ok(Value::Json(current.to_string()))
    }

    /// Helper function for recursive JSON path setting
    fn jsonb_set_recursive(
        &self,
        current: &mut serde_json::Value,
        path: &[String],
        index: usize,
        value: &serde_json::Value,
        create_missing: bool,
    ) -> Result<()> {
        if index >= path.len() {
            return Ok(());
        }

        let key = &path[index];
        let is_last = index == path.len() - 1;

        // Check if key is a number (array index)
        if let Ok(arr_idx) = key.parse::<usize>() {
            // Handle array index
            if !current.is_array() && create_missing {
                *current = serde_json::json!([]);
            }

            if let Some(arr) = current.as_array_mut() {
                // Extend array if necessary
                while arr.len() <= arr_idx {
                    arr.push(serde_json::json!(null));
                }

                if is_last {
                    arr[arr_idx] = value.clone();
                } else {
                    if arr[arr_idx].is_null() && create_missing {
                        arr[arr_idx] = serde_json::json!({});
                    }
                    self.jsonb_set_recursive(&mut arr[arr_idx], path, index + 1, value, create_missing)?;
                }
            }
        } else {
            // Handle object key
            if !current.is_object() && create_missing {
                *current = serde_json::json!({});
            }

            if let Some(obj) = current.as_object_mut() {
                if is_last {
                    obj.insert(key.clone(), value.clone());
                } else {
                    if !obj.contains_key(key) && create_missing {
                        obj.insert(key.clone(), serde_json::json!({}));
                    }
                    if let Some(next) = obj.get_mut(key) {
                        self.jsonb_set_recursive(next, path, index + 1, value, create_missing)?;
                    }
                }
            }
        }

        Ok(())
    }

    /// jsonb_concat(jsonb1, jsonb2)
    /// Merges two JSONB objects
    fn jsonb_concat(&self, args: &[Value]) -> Result<Value> {
        if args.len() != 2 {
            return Err(Error::query_execution("jsonb_concat requires exactly 2 arguments"));
        }

        let json1_str = match &args[0] {
            Value::Json(j) => j.clone(),
            Value::Null => return Ok(Value::Null),
            _ => return Err(Error::query_execution("First argument must be JSON")),
        };

        let json2_str = match &args[1] {
            Value::Json(j) => j.clone(),
            Value::Null => return Ok(Value::Json(json1_str)),
            _ => return Err(Error::query_execution("Second argument must be JSON")),
        };

        let mut json1: serde_json::Value = serde_json::from_str(&json1_str)
            .map_err(|e| Error::query_execution(format!("Invalid JSON in first argument: {}", e)))?;
        let json2: serde_json::Value = serde_json::from_str(&json2_str)
            .map_err(|e| Error::query_execution(format!("Invalid JSON in second argument: {}", e)))?;

        match (&mut json1, &json2) {
            // Object merge: right overwrites left
            (serde_json::Value::Object(obj1), serde_json::Value::Object(obj2)) => {
                for (key, value) in obj2.iter() {
                    obj1.insert(key.clone(), value.clone());
                }
            }
            // Array concatenation
            (serde_json::Value::Array(arr1), serde_json::Value::Array(arr2)) => {
                arr1.extend(arr2.iter().cloned());
            }
            // If types differ, second replaces first
            _ => {
                json1 = json2;
            }
        }

        Ok(Value::Json(json1.to_string()))
    }

    /// jsonb_delete(jsonb, path_array)
    /// Deletes the element at the specified path
    fn jsonb_delete(&self, args: &[Value]) -> Result<Value> {
        if args.len() != 2 {
            return Err(Error::query_execution("jsonb_delete requires exactly 2 arguments"));
        }

        let json_str = match &args[0] {
            Value::Json(j) => j.clone(),
            Value::Null => return Ok(Value::Null),
            _ => return Err(Error::query_execution("First argument must be JSON")),
        };

        let path_arr = match &args[1] {
            Value::Array(arr) => arr,
            _ => return Err(Error::query_execution("Second argument must be an array (path)")),
        };

        let mut path = Vec::new();
        for elem in path_arr {
            match elem {
                Value::String(s) => path.push(s.clone()),
                Value::Int4(i) => path.push(i.to_string()),
                Value::Int8(i) => path.push(i.to_string()),
                _ => {
                    return Err(Error::query_execution(
                        "Path array elements must be strings or integers"
                    ))
                }
            }
        }

        let mut current: serde_json::Value = serde_json::from_str(&json_str)
            .map_err(|e| Error::query_execution(format!("Invalid JSON: {}", e)))?;

        self.jsonb_delete_recursive(&mut current, &path, 0)?;

        Ok(Value::Json(current.to_string()))
    }

    /// Helper function for recursive JSON path deletion
    fn jsonb_delete_recursive(
        &self,
        current: &mut serde_json::Value,
        path: &[String],
        index: usize,
    ) -> Result<()> {
        if index >= path.len() {
            return Ok(());
        }

        let key = &path[index];
        let is_last = index == path.len() - 1;

        if let Ok(arr_idx) = key.parse::<usize>() {
            // Array index
            if let Some(arr) = current.as_array_mut() {
                if is_last {
                    if arr_idx < arr.len() {
                        arr.remove(arr_idx);
                    }
                } else if arr_idx < arr.len() {
                    self.jsonb_delete_recursive(&mut arr[arr_idx], path, index + 1)?;
                }
            }
        } else {
            // Object key
            if let Some(obj) = current.as_object_mut() {
                if is_last {
                    obj.remove(key);
                } else if let Some(next) = obj.get_mut(key) {
                    self.jsonb_delete_recursive(next, path, index + 1)?;
                }
            }
        }

        Ok(())
    }

    /// jsonb_each(jsonb_object)
    /// Returns object key-value pairs (returns array of keys for MVP)
    fn jsonb_each(&self, args: &[Value]) -> Result<Value> {
        if args.len() != 1 {
            return Err(Error::query_execution("jsonb_each requires exactly 1 argument"));
        }

        let json_str = match &args[0] {
            Value::Json(j) => j,
            Value::Null => return Ok(Value::Null),
            _ => return Err(Error::query_execution("Argument must be JSON")),
        };

        let json_val: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| Error::query_execution(format!("Invalid JSON: {}", e)))?;

        // For MVP, return array of key-value pairs flattened
        let mut result = Vec::new();
        if let Some(obj) = json_val.as_object() {
            for (key, value) in obj.iter() {
                result.push(Value::String(key.clone()));
                result.push(Value::Json(value.to_string()));
            }
        }

        Ok(Value::Array(result))
    }

    /// jsonb_each_text(jsonb_object)
    /// Returns object key-value pairs as text (returns array for MVP)
    fn jsonb_each_text(&self, args: &[Value]) -> Result<Value> {
        if args.len() != 1 {
            return Err(Error::query_execution("jsonb_each_text requires exactly 1 argument"));
        }

        let json_str = match &args[0] {
            Value::Json(j) => j,
            Value::Null => return Ok(Value::Null),
            _ => return Err(Error::query_execution("Argument must be JSON")),
        };

        let json_val: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| Error::query_execution(format!("Invalid JSON: {}", e)))?;

        // For MVP, return array of key-value pairs as text
        let mut result = Vec::new();
        if let Some(obj) = json_val.as_object() {
            for (key, value) in obj.iter() {
                result.push(Value::String(key.clone()));
                // Convert value to string without JSON quotes
                let val_str = match value {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Null => "".to_string(),
                    _ => value.to_string(),
                };
                result.push(Value::String(val_str));
            }
        }

        Ok(Value::Array(result))
    }

    /// Evaluate a binary operation
    fn evaluate_binary_op(
        &self,
        left: &Value,
        op: &super::BinaryOperator,
        right: &Value,
    ) -> Result<Value> {
        use super::BinaryOperator;

        match op {
            // Comparison operators
            BinaryOperator::Eq => Ok(Value::Boolean(left == right)),
            BinaryOperator::NotEq => Ok(Value::Boolean(left != right)),
            BinaryOperator::Lt => self.compare_values(left, right, |cmp| cmp.is_lt()),
            BinaryOperator::LtEq => self.compare_values(left, right, |cmp| cmp.is_le()),
            BinaryOperator::Gt => self.compare_values(left, right, |cmp| cmp.is_gt()),
            BinaryOperator::GtEq => self.compare_values(left, right, |cmp| cmp.is_ge()),

            // Logical operators
            BinaryOperator::And => {
                let left_bool = self.to_boolean(left)?;
                let right_bool = self.to_boolean(right)?;
                Ok(Value::Boolean(left_bool && right_bool))
            }
            BinaryOperator::Or => {
                let left_bool = self.to_boolean(left)?;
                let right_bool = self.to_boolean(right)?;
                Ok(Value::Boolean(left_bool || right_bool))
            }

            // Arithmetic operators
            BinaryOperator::Plus => self.arithmetic_add(left, right),
            BinaryOperator::Minus => self.arithmetic_subtract(left, right),
            BinaryOperator::Multiply => self.arithmetic_multiply(left, right),
            BinaryOperator::Divide => {
                // Check for division by zero
                if self.is_zero(right) {
                    return Err(Error::query_execution("Division by zero"));
                }
                self.arithmetic_divide(left, right)
            }

            // Vector similarity operators
            BinaryOperator::VectorL2Distance => {
                self.vector_distance_op(left, right, crate::vector::l2_distance)
            }
            BinaryOperator::VectorCosineDistance => {
                self.vector_distance_op(left, right, crate::vector::cosine_distance)
            }
            BinaryOperator::VectorInnerProduct => {
                self.vector_distance_op(left, right, crate::vector::inner_product_distance)
            }

            // JSONB operators
            BinaryOperator::JsonGet => self.json_get_op(left, right, false),
            BinaryOperator::JsonGetText => self.json_get_op(left, right, true),
            BinaryOperator::JsonContains => self.json_contains_op(left, right),
            BinaryOperator::JsonContainedBy => self.json_contains_op(right, left),
            BinaryOperator::JsonExists => self.json_exists_op(left, right, false),
            BinaryOperator::JsonExistsAny => self.json_exists_op(left, right, true),
            BinaryOperator::JsonExistsAll => self.json_exists_all_op(left, right),

            // Array operators
            BinaryOperator::ArrayConcat => self.array_concat_op(left, right),

            _ => Err(Error::query_execution(format!(
                "Binary operator not yet implemented: {:?}",
                op
            ))),
        }
    }

    /// Evaluate a unary operation
    fn evaluate_unary_op(&self, op: &super::UnaryOperator, value: &Value) -> Result<Value> {
        use super::UnaryOperator;

        match op {
            UnaryOperator::Not => {
                let bool_val = self.to_boolean(value)?;
                Ok(Value::Boolean(!bool_val))
            }
            UnaryOperator::Minus => match value {
                Value::Int2(i) => Ok(Value::Int2(-i)),
                Value::Int4(i) => Ok(Value::Int4(-i)),
                Value::Int8(i) => Ok(Value::Int8(-i)),
                Value::Float4(f) => Ok(Value::Float4(-f)),
                Value::Float8(f) => Ok(Value::Float8(-f)),
                Value::Numeric(n) => {
                    // Negate a numeric value by parsing and inverting sign
                    let negated = if n.starts_with('-') {
                        n[1..].to_string()
                    } else {
                        format!("-{}", n)
                    };
                    Ok(Value::Numeric(negated))
                }
                _ => Err(Error::query_execution(format!(
                    "Cannot apply unary minus to {:?}",
                    value
                ))),
            },
            _ => Err(Error::query_execution(format!(
                "Unary operator not yet implemented: {:?}",
                op
            ))),
        }
    }

    /// Compare two values using a comparison function
    fn compare_values<F>(&self, left: &Value, right: &Value, cmp: F) -> Result<Value>
    where
        F: FnOnce(std::cmp::Ordering) -> bool,
    {
        use std::cmp::Ordering;

        let ordering = match (left, right) {
            // Same type comparisons
            (Value::Int2(a), Value::Int2(b)) => a.cmp(b),
            (Value::Int4(a), Value::Int4(b)) => a.cmp(b),
            (Value::Int8(a), Value::Int8(b)) => a.cmp(b),
            (Value::Float4(a), Value::Float4(b)) => {
                a.partial_cmp(b).unwrap_or(Ordering::Equal)
            }
            (Value::Float8(a), Value::Float8(b)) => {
                a.partial_cmp(b).unwrap_or(Ordering::Equal)
            }
            (Value::String(a), Value::String(b)) => a.cmp(b),

            // Cross-type integer comparisons (promote to i64)
            (Value::Int2(a), Value::Int4(b)) => (*a as i64).cmp(&(*b as i64)),
            (Value::Int4(a), Value::Int2(b)) => (*a as i64).cmp(&(*b as i64)),
            (Value::Int2(a), Value::Int8(b)) => (*a as i64).cmp(b),
            (Value::Int8(a), Value::Int2(b)) => a.cmp(&(*b as i64)),
            (Value::Int4(a), Value::Int8(b)) => (*a as i64).cmp(b),
            (Value::Int8(a), Value::Int4(b)) => a.cmp(&(*b as i64)),

            // Integer to float comparisons (promote to f64)
            (Value::Int2(a), Value::Float4(b)) => (*a as f64).partial_cmp(&(*b as f64)).unwrap_or(Ordering::Equal),
            (Value::Float4(a), Value::Int2(b)) => (*a as f64).partial_cmp(&(*b as f64)).unwrap_or(Ordering::Equal),
            (Value::Int2(a), Value::Float8(b)) => (*a as f64).partial_cmp(b).unwrap_or(Ordering::Equal),
            (Value::Float8(a), Value::Int2(b)) => a.partial_cmp(&(*b as f64)).unwrap_or(Ordering::Equal),
            (Value::Int4(a), Value::Float4(b)) => (*a as f64).partial_cmp(&(*b as f64)).unwrap_or(Ordering::Equal),
            (Value::Float4(a), Value::Int4(b)) => (*a as f64).partial_cmp(&(*b as f64)).unwrap_or(Ordering::Equal),
            (Value::Int4(a), Value::Float8(b)) => (*a as f64).partial_cmp(b).unwrap_or(Ordering::Equal),
            (Value::Float8(a), Value::Int4(b)) => a.partial_cmp(&(*b as f64)).unwrap_or(Ordering::Equal),
            (Value::Int8(a), Value::Float4(b)) => (*a as f64).partial_cmp(&(*b as f64)).unwrap_or(Ordering::Equal),
            (Value::Float4(a), Value::Int8(b)) => (*a as f64).partial_cmp(&(*b as f64)).unwrap_or(Ordering::Equal),
            (Value::Int8(a), Value::Float8(b)) => (*a as f64).partial_cmp(b).unwrap_or(Ordering::Equal),
            (Value::Float8(a), Value::Int8(b)) => a.partial_cmp(&(*b as f64)).unwrap_or(Ordering::Equal),

            // Float4 to Float8 comparisons
            (Value::Float4(a), Value::Float8(b)) => (*a as f64).partial_cmp(b).unwrap_or(Ordering::Equal),
            (Value::Float8(a), Value::Float4(b)) => a.partial_cmp(&(*b as f64)).unwrap_or(Ordering::Equal),

            // Numeric to Numeric comparisons (same type)
            (Value::Numeric(a), Value::Numeric(b)) => {
                match (a.parse::<Decimal>(), b.parse::<Decimal>()) {
                    (Ok(a_dec), Ok(b_dec)) => a_dec.cmp(&b_dec),
                    _ => Ordering::Equal, // If parsing fails, treat as equal
                }
            }

            // Numeric to Int comparisons
            (Value::Numeric(a), Value::Int2(b)) => {
                match a.parse::<Decimal>() {
                    Ok(a_dec) => a_dec.cmp(&Decimal::from(*b)),
                    Err(_) => Ordering::Equal,
                }
            }
            (Value::Int2(a), Value::Numeric(b)) => {
                match b.parse::<Decimal>() {
                    Ok(b_dec) => Decimal::from(*a).cmp(&b_dec),
                    Err(_) => Ordering::Equal,
                }
            }
            (Value::Numeric(a), Value::Int4(b)) => {
                match a.parse::<Decimal>() {
                    Ok(a_dec) => a_dec.cmp(&Decimal::from(*b)),
                    Err(_) => Ordering::Equal,
                }
            }
            (Value::Int4(a), Value::Numeric(b)) => {
                match b.parse::<Decimal>() {
                    Ok(b_dec) => Decimal::from(*a).cmp(&b_dec),
                    Err(_) => Ordering::Equal,
                }
            }
            (Value::Numeric(a), Value::Int8(b)) => {
                match a.parse::<Decimal>() {
                    Ok(a_dec) => a_dec.cmp(&Decimal::from(*b)),
                    Err(_) => Ordering::Equal,
                }
            }
            (Value::Int8(a), Value::Numeric(b)) => {
                match b.parse::<Decimal>() {
                    Ok(b_dec) => Decimal::from(*a).cmp(&b_dec),
                    Err(_) => Ordering::Equal,
                }
            }

            // Numeric to Float comparisons (convert to f64 for comparison)
            (Value::Numeric(a), Value::Float4(b)) => {
                match a.parse::<f64>() {
                    Ok(a_f) => a_f.partial_cmp(&(*b as f64)).unwrap_or(Ordering::Equal),
                    Err(_) => Ordering::Equal,
                }
            }
            (Value::Float4(a), Value::Numeric(b)) => {
                match b.parse::<f64>() {
                    Ok(b_f) => (*a as f64).partial_cmp(&b_f).unwrap_or(Ordering::Equal),
                    Err(_) => Ordering::Equal,
                }
            }
            (Value::Numeric(a), Value::Float8(b)) => {
                match a.parse::<f64>() {
                    Ok(a_f) => a_f.partial_cmp(b).unwrap_or(Ordering::Equal),
                    Err(_) => Ordering::Equal,
                }
            }
            (Value::Float8(a), Value::Numeric(b)) => {
                match b.parse::<f64>() {
                    Ok(b_f) => a.partial_cmp(&b_f).unwrap_or(Ordering::Equal),
                    Err(_) => Ordering::Equal,
                }
            }

            _ => {
                return Err(Error::query_execution(format!(
                    "Cannot compare {:?} and {:?}",
                    left, right
                )))
            }
        };

        Ok(Value::Boolean(cmp(ordering)))
    }

    /// Perform arithmetic operation on two values
    fn arithmetic_op<F>(&self, left: &Value, right: &Value, op: F) -> Result<Value>
    where
        F: Fn(i64, i64) -> i64,
    {
        match (left, right) {
            (Value::Int4(a), Value::Int4(b)) => {
                let result = op(*a as i64, *b as i64);
                Ok(Value::Int4(result as i32))
            }
            (Value::Int8(a), Value::Int8(b)) => Ok(Value::Int8(op(*a, *b))),
            _ => Err(Error::query_execution(format!(
                "Cannot perform arithmetic on {:?} and {:?}",
                left, right
            ))),
        }
    }

    /// Addition operator with support for Numeric precision
    fn arithmetic_add(&self, left: &Value, right: &Value) -> Result<Value> {
        match (left, right) {
            // Numeric + Numeric: preserve precision
            (Value::Numeric(a), Value::Numeric(b)) => {
                // Parse both numeric strings and add
                let a_dec = a.parse::<Decimal>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", a, e)))?;
                let b_dec = b.parse::<Decimal>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", b, e)))?;
                Ok(Value::Numeric(format!("{}", a_dec + b_dec)))
            }
            // Numeric + Int: convert int to numeric
            (Value::Numeric(a), Value::Int4(b)) => {
                let a_dec = a.parse::<Decimal>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", a, e)))?;
                let b_dec = Decimal::from(*b);
                Ok(Value::Numeric(format!("{}", a_dec + b_dec)))
            }
            (Value::Int4(a), Value::Numeric(b)) => {
                let a_dec = Decimal::from(*a);
                let b_dec = b.parse::<Decimal>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", b, e)))?;
                Ok(Value::Numeric(format!("{}", a_dec + b_dec)))
            }
            (Value::Numeric(a), Value::Int8(b)) => {
                let a_dec = a.parse::<Decimal>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", a, e)))?;
                let b_dec = Decimal::from(*b);
                Ok(Value::Numeric(format!("{}", a_dec + b_dec)))
            }
            (Value::Int8(a), Value::Numeric(b)) => {
                let a_dec = Decimal::from(*a);
                let b_dec = b.parse::<Decimal>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", b, e)))?;
                Ok(Value::Numeric(format!("{}", a_dec + b_dec)))
            }
            // Numeric + Float: convert to float
            (Value::Numeric(a), Value::Float8(b)) => {
                let a_f = a.parse::<f64>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", a, e)))?;
                Ok(Value::Float8(a_f + b))
            }
            (Value::Float8(a), Value::Numeric(b)) => {
                let b_f = b.parse::<f64>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", b, e)))?;
                Ok(Value::Float8(a + b_f))
            }
            (Value::Numeric(a), Value::Float4(b)) => {
                let a_f = a.parse::<f32>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", a, e)))?;
                Ok(Value::Float4(a_f + b))
            }
            (Value::Float4(a), Value::Numeric(b)) => {
                let b_f = b.parse::<f32>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", b, e)))?;
                Ok(Value::Float4(a + b_f))
            }
            // Existing Int/Float operations
            (Value::Int4(a), Value::Int4(b)) => {
                let result = (*a as i64) + (*b as i64);
                Ok(Value::Int4(result as i32))
            }
            (Value::Int8(a), Value::Int8(b)) => Ok(Value::Int8(a + b)),
            (Value::Float4(a), Value::Float4(b)) => Ok(Value::Float4(a + b)),
            (Value::Float8(a), Value::Float8(b)) => Ok(Value::Float8(a + b)),
            (Value::Int4(a), Value::Int8(b)) => Ok(Value::Int8((*a as i64) + b)),
            (Value::Int8(a), Value::Int4(b)) => Ok(Value::Int8(a + (*b as i64))),
            // Cross-type Float/Int coercion
            (Value::Float4(a), Value::Int4(b)) => Ok(Value::Float4(a + (*b as f32))),
            (Value::Int4(a), Value::Float4(b)) => Ok(Value::Float4((*a as f32) + b)),
            (Value::Float8(a), Value::Int4(b)) => Ok(Value::Float8(a + (*b as f64))),
            (Value::Int4(a), Value::Float8(b)) => Ok(Value::Float8((*a as f64) + b)),
            (Value::Float4(a), Value::Int8(b)) => Ok(Value::Float8((*a as f64) + (*b as f64))),
            (Value::Int8(a), Value::Float4(b)) => Ok(Value::Float8((*a as f64) + (*b as f64))),
            (Value::Float8(a), Value::Int8(b)) => Ok(Value::Float8(a + (*b as f64))),
            (Value::Int8(a), Value::Float8(b)) => Ok(Value::Float8((*a as f64) + b)),
            (Value::Float4(a), Value::Float8(b)) => Ok(Value::Float8((*a as f64) + b)),
            (Value::Float8(a), Value::Float4(b)) => Ok(Value::Float8(a + (*b as f64))),
            // Int2 coercion
            (Value::Int2(a), Value::Int4(b)) => Ok(Value::Int4((*a as i32) + b)),
            (Value::Int4(a), Value::Int2(b)) => Ok(Value::Int4(a + (*b as i32))),
            (Value::Int2(a), Value::Int8(b)) => Ok(Value::Int8((*a as i64) + b)),
            (Value::Int8(a), Value::Int2(b)) => Ok(Value::Int8(a + (*b as i64))),
            (Value::Int2(a), Value::Int2(b)) => Ok(Value::Int2(a + b)),
            (Value::Int2(a), Value::Float4(b)) => Ok(Value::Float4((*a as f32) + b)),
            (Value::Float4(a), Value::Int2(b)) => Ok(Value::Float4(a + (*b as f32))),
            (Value::Int2(a), Value::Float8(b)) => Ok(Value::Float8((*a as f64) + b)),
            (Value::Float8(a), Value::Int2(b)) => Ok(Value::Float8(a + (*b as f64))),
            _ => Err(Error::query_execution(format!(
                "Cannot add {:?} and {:?}",
                left, right
            ))),
        }
    }

    /// Subtraction operator with support for Numeric precision
    fn arithmetic_subtract(&self, left: &Value, right: &Value) -> Result<Value> {
        match (left, right) {
            // Numeric - Numeric: preserve precision
            (Value::Numeric(a), Value::Numeric(b)) => {
                let a_dec = a.parse::<Decimal>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", a, e)))?;
                let b_dec = b.parse::<Decimal>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", b, e)))?;
                Ok(Value::Numeric(format!("{}", a_dec - b_dec)))
            }
            // Numeric - Int: convert int to numeric
            (Value::Numeric(a), Value::Int4(b)) => {
                let a_dec = a.parse::<Decimal>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", a, e)))?;
                let b_dec = Decimal::from(*b);
                Ok(Value::Numeric(format!("{}", a_dec - b_dec)))
            }
            (Value::Int4(a), Value::Numeric(b)) => {
                let a_dec = Decimal::from(*a);
                let b_dec = b.parse::<Decimal>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", b, e)))?;
                Ok(Value::Numeric(format!("{}", a_dec - b_dec)))
            }
            (Value::Numeric(a), Value::Int8(b)) => {
                let a_dec = a.parse::<Decimal>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", a, e)))?;
                let b_dec = Decimal::from(*b);
                Ok(Value::Numeric(format!("{}", a_dec - b_dec)))
            }
            (Value::Int8(a), Value::Numeric(b)) => {
                let a_dec = Decimal::from(*a);
                let b_dec = b.parse::<Decimal>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", b, e)))?;
                Ok(Value::Numeric(format!("{}", a_dec - b_dec)))
            }
            // Numeric - Float: convert to float
            (Value::Numeric(a), Value::Float8(b)) => {
                let a_f = a.parse::<f64>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", a, e)))?;
                Ok(Value::Float8(a_f - b))
            }
            (Value::Float8(a), Value::Numeric(b)) => {
                let b_f = b.parse::<f64>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", b, e)))?;
                Ok(Value::Float8(a - b_f))
            }
            (Value::Numeric(a), Value::Float4(b)) => {
                let a_f = a.parse::<f32>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", a, e)))?;
                Ok(Value::Float4(a_f - b))
            }
            (Value::Float4(a), Value::Numeric(b)) => {
                let b_f = b.parse::<f32>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", b, e)))?;
                Ok(Value::Float4(a - b_f))
            }
            // Existing Int/Float operations
            (Value::Int4(a), Value::Int4(b)) => {
                let result = (*a as i64) - (*b as i64);
                Ok(Value::Int4(result as i32))
            }
            (Value::Int8(a), Value::Int8(b)) => Ok(Value::Int8(a - b)),
            (Value::Float4(a), Value::Float4(b)) => Ok(Value::Float4(a - b)),
            (Value::Float8(a), Value::Float8(b)) => Ok(Value::Float8(a - b)),
            (Value::Int4(a), Value::Int8(b)) => Ok(Value::Int8((*a as i64) - b)),
            (Value::Int8(a), Value::Int4(b)) => Ok(Value::Int8(a - (*b as i64))),
            // Cross-type Float/Int coercion
            (Value::Float4(a), Value::Int4(b)) => Ok(Value::Float4(a - (*b as f32))),
            (Value::Int4(a), Value::Float4(b)) => Ok(Value::Float4((*a as f32) - b)),
            (Value::Float8(a), Value::Int4(b)) => Ok(Value::Float8(a - (*b as f64))),
            (Value::Int4(a), Value::Float8(b)) => Ok(Value::Float8((*a as f64) - b)),
            (Value::Float4(a), Value::Int8(b)) => Ok(Value::Float8((*a as f64) - (*b as f64))),
            (Value::Int8(a), Value::Float4(b)) => Ok(Value::Float8((*a as f64) - (*b as f64))),
            (Value::Float8(a), Value::Int8(b)) => Ok(Value::Float8(a - (*b as f64))),
            (Value::Int8(a), Value::Float8(b)) => Ok(Value::Float8((*a as f64) - b)),
            (Value::Float4(a), Value::Float8(b)) => Ok(Value::Float8((*a as f64) - b)),
            (Value::Float8(a), Value::Float4(b)) => Ok(Value::Float8(a - (*b as f64))),
            // Int2 coercion
            (Value::Int2(a), Value::Int4(b)) => Ok(Value::Int4((*a as i32) - b)),
            (Value::Int4(a), Value::Int2(b)) => Ok(Value::Int4(a - (*b as i32))),
            (Value::Int2(a), Value::Int8(b)) => Ok(Value::Int8((*a as i64) - b)),
            (Value::Int8(a), Value::Int2(b)) => Ok(Value::Int8(a - (*b as i64))),
            (Value::Int2(a), Value::Int2(b)) => Ok(Value::Int2(a - b)),
            (Value::Int2(a), Value::Float4(b)) => Ok(Value::Float4((*a as f32) - b)),
            (Value::Float4(a), Value::Int2(b)) => Ok(Value::Float4(a - (*b as f32))),
            (Value::Int2(a), Value::Float8(b)) => Ok(Value::Float8((*a as f64) - b)),
            (Value::Float8(a), Value::Int2(b)) => Ok(Value::Float8(a - (*b as f64))),
            _ => Err(Error::query_execution(format!(
                "Cannot subtract {:?} and {:?}",
                left, right
            ))),
        }
    }

    /// Multiplication operator with support for Numeric precision
    fn arithmetic_multiply(&self, left: &Value, right: &Value) -> Result<Value> {
        match (left, right) {
            // Numeric * Numeric: preserve precision
            (Value::Numeric(a), Value::Numeric(b)) => {
                let a_dec = a.parse::<Decimal>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", a, e)))?;
                let b_dec = b.parse::<Decimal>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", b, e)))?;
                Ok(Value::Numeric(format!("{}", a_dec * b_dec)))
            }
            // Numeric * Int: convert int to numeric
            (Value::Numeric(a), Value::Int4(b)) => {
                let a_dec = a.parse::<Decimal>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", a, e)))?;
                let b_dec = Decimal::from(*b);
                Ok(Value::Numeric(format!("{}", a_dec * b_dec)))
            }
            (Value::Int4(a), Value::Numeric(b)) => {
                let a_dec = Decimal::from(*a);
                let b_dec = b.parse::<Decimal>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", b, e)))?;
                Ok(Value::Numeric(format!("{}", a_dec * b_dec)))
            }
            (Value::Numeric(a), Value::Int8(b)) => {
                let a_dec = a.parse::<Decimal>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", a, e)))?;
                let b_dec = Decimal::from(*b);
                Ok(Value::Numeric(format!("{}", a_dec * b_dec)))
            }
            (Value::Int8(a), Value::Numeric(b)) => {
                let a_dec = Decimal::from(*a);
                let b_dec = b.parse::<Decimal>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", b, e)))?;
                Ok(Value::Numeric(format!("{}", a_dec * b_dec)))
            }
            // Numeric * Float: convert to float
            (Value::Numeric(a), Value::Float8(b)) => {
                let a_f = a.parse::<f64>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", a, e)))?;
                Ok(Value::Float8(a_f * b))
            }
            (Value::Float8(a), Value::Numeric(b)) => {
                let b_f = b.parse::<f64>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", b, e)))?;
                Ok(Value::Float8(a * b_f))
            }
            (Value::Numeric(a), Value::Float4(b)) => {
                let a_f = a.parse::<f32>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", a, e)))?;
                Ok(Value::Float4(a_f * b))
            }
            (Value::Float4(a), Value::Numeric(b)) => {
                let b_f = b.parse::<f32>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", b, e)))?;
                Ok(Value::Float4(a * b_f))
            }
            // Existing Int/Float operations
            (Value::Int4(a), Value::Int4(b)) => {
                let result = (*a as i64) * (*b as i64);
                Ok(Value::Int4(result as i32))
            }
            (Value::Int8(a), Value::Int8(b)) => Ok(Value::Int8(a * b)),
            (Value::Float4(a), Value::Float4(b)) => Ok(Value::Float4(a * b)),
            (Value::Float8(a), Value::Float8(b)) => Ok(Value::Float8(a * b)),
            (Value::Int4(a), Value::Int8(b)) => Ok(Value::Int8((*a as i64) * b)),
            (Value::Int8(a), Value::Int4(b)) => Ok(Value::Int8(a * (*b as i64))),
            // Cross-type Float/Int coercion
            (Value::Float4(a), Value::Int4(b)) => Ok(Value::Float4(a * (*b as f32))),
            (Value::Int4(a), Value::Float4(b)) => Ok(Value::Float4((*a as f32) * b)),
            (Value::Float8(a), Value::Int4(b)) => Ok(Value::Float8(a * (*b as f64))),
            (Value::Int4(a), Value::Float8(b)) => Ok(Value::Float8((*a as f64) * b)),
            (Value::Float4(a), Value::Int8(b)) => Ok(Value::Float8((*a as f64) * (*b as f64))),
            (Value::Int8(a), Value::Float4(b)) => Ok(Value::Float8((*a as f64) * (*b as f64))),
            (Value::Float8(a), Value::Int8(b)) => Ok(Value::Float8(a * (*b as f64))),
            (Value::Int8(a), Value::Float8(b)) => Ok(Value::Float8((*a as f64) * b)),
            (Value::Float4(a), Value::Float8(b)) => Ok(Value::Float8((*a as f64) * b)),
            (Value::Float8(a), Value::Float4(b)) => Ok(Value::Float8(a * (*b as f64))),
            // Int2 coercion
            (Value::Int2(a), Value::Int4(b)) => Ok(Value::Int4((*a as i32) * b)),
            (Value::Int4(a), Value::Int2(b)) => Ok(Value::Int4(a * (*b as i32))),
            (Value::Int2(a), Value::Int8(b)) => Ok(Value::Int8((*a as i64) * b)),
            (Value::Int8(a), Value::Int2(b)) => Ok(Value::Int8(a * (*b as i64))),
            (Value::Int2(a), Value::Int2(b)) => Ok(Value::Int2(a * b)),
            (Value::Int2(a), Value::Float4(b)) => Ok(Value::Float4((*a as f32) * b)),
            (Value::Float4(a), Value::Int2(b)) => Ok(Value::Float4(a * (*b as f32))),
            (Value::Int2(a), Value::Float8(b)) => Ok(Value::Float8((*a as f64) * b)),
            (Value::Float8(a), Value::Int2(b)) => Ok(Value::Float8(a * (*b as f64))),
            _ => Err(Error::query_execution(format!(
                "Cannot multiply {:?} and {:?}",
                left, right
            ))),
        }
    }

    /// Division operator with support for Numeric precision
    fn arithmetic_divide(&self, left: &Value, right: &Value) -> Result<Value> {
        match (left, right) {
            // Numeric / Numeric: preserve precision
            (Value::Numeric(a), Value::Numeric(b)) => {
                let a_dec = a.parse::<Decimal>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", a, e)))?;
                let b_dec = b.parse::<Decimal>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", b, e)))?;
                Ok(Value::Numeric(format!("{}", a_dec / b_dec)))
            }
            // Numeric / Int: convert int to numeric
            (Value::Numeric(a), Value::Int4(b)) => {
                let a_dec = a.parse::<Decimal>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", a, e)))?;
                let b_dec = Decimal::from(*b);
                Ok(Value::Numeric(format!("{}", a_dec / b_dec)))
            }
            (Value::Int4(a), Value::Numeric(b)) => {
                let a_dec = Decimal::from(*a);
                let b_dec = b.parse::<Decimal>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", b, e)))?;
                Ok(Value::Numeric(format!("{}", a_dec / b_dec)))
            }
            (Value::Numeric(a), Value::Int8(b)) => {
                let a_dec = a.parse::<Decimal>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", a, e)))?;
                let b_dec = Decimal::from(*b);
                Ok(Value::Numeric(format!("{}", a_dec / b_dec)))
            }
            (Value::Int8(a), Value::Numeric(b)) => {
                let a_dec = Decimal::from(*a);
                let b_dec = b.parse::<Decimal>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", b, e)))?;
                Ok(Value::Numeric(format!("{}", a_dec / b_dec)))
            }
            // Numeric / Float: convert to float
            (Value::Numeric(a), Value::Float8(b)) => {
                let a_f = a.parse::<f64>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", a, e)))?;
                Ok(Value::Float8(a_f / b))
            }
            (Value::Float8(a), Value::Numeric(b)) => {
                let b_f = b.parse::<f64>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", b, e)))?;
                Ok(Value::Float8(a / b_f))
            }
            (Value::Numeric(a), Value::Float4(b)) => {
                let a_f = a.parse::<f32>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", a, e)))?;
                Ok(Value::Float4(a_f / b))
            }
            (Value::Float4(a), Value::Numeric(b)) => {
                let b_f = b.parse::<f32>()
                    .map_err(|e| Error::query_execution(format!("Invalid numeric value '{}': {}", b, e)))?;
                Ok(Value::Float4(a / b_f))
            }
            // Existing Int/Float operations
            (Value::Int4(a), Value::Int4(b)) => {
                let result = (*a as i64) / (*b as i64);
                Ok(Value::Int4(result as i32))
            }
            (Value::Int8(a), Value::Int8(b)) => Ok(Value::Int8(a / b)),
            (Value::Float4(a), Value::Float4(b)) => Ok(Value::Float4(a / b)),
            (Value::Float8(a), Value::Float8(b)) => Ok(Value::Float8(a / b)),
            (Value::Int4(a), Value::Int8(b)) => Ok(Value::Int8((*a as i64) / b)),
            (Value::Int8(a), Value::Int4(b)) => Ok(Value::Int8(a / (*b as i64))),
            // Cross-type Float/Int coercion
            (Value::Float4(a), Value::Int4(b)) => Ok(Value::Float4(a / (*b as f32))),
            (Value::Int4(a), Value::Float4(b)) => Ok(Value::Float4((*a as f32) / b)),
            (Value::Float8(a), Value::Int4(b)) => Ok(Value::Float8(a / (*b as f64))),
            (Value::Int4(a), Value::Float8(b)) => Ok(Value::Float8((*a as f64) / b)),
            (Value::Float4(a), Value::Int8(b)) => Ok(Value::Float8((*a as f64) / (*b as f64))),
            (Value::Int8(a), Value::Float4(b)) => Ok(Value::Float8((*a as f64) / (*b as f64))),
            (Value::Float8(a), Value::Int8(b)) => Ok(Value::Float8(a / (*b as f64))),
            (Value::Int8(a), Value::Float8(b)) => Ok(Value::Float8((*a as f64) / b)),
            (Value::Float4(a), Value::Float8(b)) => Ok(Value::Float8((*a as f64) / b)),
            (Value::Float8(a), Value::Float4(b)) => Ok(Value::Float8(a / (*b as f64))),
            // Int2 coercion
            (Value::Int2(a), Value::Int4(b)) => Ok(Value::Int4((*a as i32) / b)),
            (Value::Int4(a), Value::Int2(b)) => Ok(Value::Int4(a / (*b as i32))),
            (Value::Int2(a), Value::Int8(b)) => Ok(Value::Int8((*a as i64) / b)),
            (Value::Int8(a), Value::Int2(b)) => Ok(Value::Int8(a / (*b as i64))),
            (Value::Int2(a), Value::Int2(b)) => Ok(Value::Int2(a / b)),
            (Value::Int2(a), Value::Float4(b)) => Ok(Value::Float4((*a as f32) / b)),
            (Value::Float4(a), Value::Int2(b)) => Ok(Value::Float4(a / (*b as f32))),
            (Value::Int2(a), Value::Float8(b)) => Ok(Value::Float8((*a as f64) / b)),
            (Value::Float8(a), Value::Int2(b)) => Ok(Value::Float8(a / (*b as f64))),
            _ => Err(Error::query_execution(format!(
                "Cannot divide {:?} and {:?}",
                left, right
            ))),
        }
    }

    /// Convert a value to boolean
    fn to_boolean(&self, value: &Value) -> Result<bool> {
        match value {
            Value::Boolean(b) => Ok(*b),
            Value::Null => Ok(false),
            _ => Err(Error::query_execution(format!(
                "Cannot convert {:?} to boolean",
                value
            ))),
        }
    }

    /// Check if a value is zero
    fn is_zero(&self, value: &Value) -> bool {
        match value {
            Value::Int2(0) | Value::Int4(0) | Value::Int8(0) | Value::Float4(0.0) | Value::Float8(0.0) => true,
            Value::Numeric(n) => {
                // Check if numeric string represents zero
                match n.parse::<Decimal>() {
                    Ok(dec) => dec == Decimal::from(0),
                    Err(_) => false,
                }
            }
            _ => false,
        }
    }

    /// Compute vector distance between two vectors
    fn vector_distance_op<F>(
        &self,
        left: &Value,
        right: &Value,
        distance_fn: F,
    ) -> Result<Value>
    where
        F: Fn(&[f32], &[f32]) -> f32,
    {
        // Auto-cast strings to vectors if needed
        let left_vec = match left {
            Value::Vector(v) => v.clone(),
            Value::String(s) if s.trim().starts_with('[') && s.trim().ends_with(']') => {
                // Parse string as vector
                let trimmed = s.trim();
                let without_brackets = trimmed.trim_start_matches('[').trim_end_matches(']');
                let elements: Result<Vec<f32>> = without_brackets
                    .split(',')
                    .map(|elem| {
                        elem.trim()
                            .parse::<f32>()
                            .map_err(|e| Error::query_execution(format!("Invalid vector element '{}': {}", elem, e)))
                    })
                    .collect();
                elements?
            }
            _ => return Err(Error::query_execution(format!(
                "Vector distance operators require vector operands, got {:?} and {:?}",
                left, right
            ))),
        };

        let right_vec = match right {
            Value::Vector(v) => v.clone(),
            Value::String(s) if s.trim().starts_with('[') && s.trim().ends_with(']') => {
                // Parse string as vector
                let trimmed = s.trim();
                let without_brackets = trimmed.trim_start_matches('[').trim_end_matches(']');
                let elements: Result<Vec<f32>> = without_brackets
                    .split(',')
                    .map(|elem| {
                        elem.trim()
                            .parse::<f32>()
                            .map_err(|e| Error::query_execution(format!("Invalid vector element '{}': {}", elem, e)))
                    })
                    .collect();
                elements?
            }
            _ => return Err(Error::query_execution(format!(
                "Vector distance operators require vector operands, got {:?} and {:?}",
                left, right
            ))),
        };

        if left_vec.len() != right_vec.len() {
            return Err(Error::query_execution(format!(
                "Vector dimension mismatch: {} vs {}",
                left_vec.len(),
                right_vec.len()
            )));
        }

        let distance = distance_fn(&left_vec, &right_vec);
        Ok(Value::Float4(distance))
    }

    /// COSINE_SIMILARITY(v1, v2) - returns similarity (1 - cosine_distance)
    fn vector_cosine_similarity(&self, args: &[Value]) -> Result<Value> {
        if args.len() != 2 {
            return Err(Error::query_execution(
                "COSINE_SIMILARITY requires exactly 2 vector arguments".to_string()
            ));
        }
        let distance = self.vector_distance_op(&args[0], &args[1], crate::vector::cosine_distance)?;
        match distance {
            Value::Float4(d) => Ok(Value::Float4(1.0 - d)),
            _ => Err(Error::query_execution("Unexpected result type".to_string())),
        }
    }

    /// COSINE_DISTANCE(v1, v2) - returns cosine distance
    fn vector_cosine_distance(&self, args: &[Value]) -> Result<Value> {
        if args.len() != 2 {
            return Err(Error::query_execution(
                "COSINE_DISTANCE requires exactly 2 vector arguments".to_string()
            ));
        }
        self.vector_distance_op(&args[0], &args[1], crate::vector::cosine_distance)
    }

    /// L2_DISTANCE(v1, v2) - returns Euclidean distance
    fn vector_l2_distance(&self, args: &[Value]) -> Result<Value> {
        if args.len() != 2 {
            return Err(Error::query_execution(
                "L2_DISTANCE requires exactly 2 vector arguments".to_string()
            ));
        }
        self.vector_distance_op(&args[0], &args[1], crate::vector::l2_distance)
    }

    /// INNER_PRODUCT(v1, v2) - returns inner product distance
    fn vector_inner_product(&self, args: &[Value]) -> Result<Value> {
        if args.len() != 2 {
            return Err(Error::query_execution(
                "INNER_PRODUCT requires exactly 2 vector arguments".to_string()
            ));
        }
        self.vector_distance_op(&args[0], &args[1], crate::vector::inner_product_distance)
    }

    /// JSON get operator: -> or ->>
    /// Extracts field from JSON object
    /// If as_text is true, returns text value (->>), otherwise returns JSON (->)
    fn json_get_op(&self, json_val: &Value, key_val: &Value, as_text: bool) -> Result<Value> {
        let json_str = match json_val {
            Value::Json(j) => j,
            Value::Null => return Ok(Value::Null),
            _ => return Err(Error::query_execution(format!(
                "Left operand of -> must be JSON, got {:?}",
                json_val
            ))),
        };

        // Parse JSON string to serde_json::Value
        let json: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| Error::query_execution(format!("Invalid JSON: {}", e)))?;

        let key = match key_val {
            Value::String(s) => s.as_str(),
            Value::Int4(i) => {
                // Array index access
                if let Some(arr) = json.as_array() {
                    let idx = if *i < 0 {
                        // Negative index: count from end
                        (arr.len() as i32 + i) as usize
                    } else {
                        *i as usize
                    };

                    return if let Some(elem) = arr.get(idx) {
                        if as_text {
                            // Return as text
                            match elem {
                                serde_json::Value::String(s) => Ok(Value::String(s.clone())),
                                _ => Ok(Value::String(elem.to_string())),
                            }
                        } else {
                            // Return as JSON
                            Ok(Value::Json(elem.to_string()))
                        }
                    } else {
                        Ok(Value::Null)
                    };
                }
                return Err(Error::query_execution(
                    "Integer index can only be used with JSON arrays"
                ));
            }
            _ => return Err(Error::query_execution(format!(
                "Right operand of -> must be string or integer, got {:?}",
                key_val
            ))),
        };

        // Object field access
        if let Some(obj) = json.as_object() {
            if let Some(field) = obj.get(key) {
                if as_text {
                    // Return as text
                    match field {
                        serde_json::Value::String(s) => Ok(Value::String(s.clone())),
                        _ => Ok(Value::String(field.to_string())),
                    }
                } else {
                    // Return as JSON
                    Ok(Value::Json(field.to_string()))
                }
            } else {
                Ok(Value::Null)
            }
        } else {
            Err(Error::query_execution(
                "String key can only be used with JSON objects"
            ))
        }
    }

    /// JSON contains operator: @>
    /// Checks if left JSON contains right JSON
    fn json_contains_op(&self, left: &Value, right: &Value) -> Result<Value> {
        let left_json_str = match left {
            Value::Json(j) => j,
            Value::Null => return Ok(Value::Boolean(false)),
            _ => return Err(Error::query_execution(format!(
                "JSON contains operator requires JSON operands, got {:?}",
                left
            ))),
        };

        let right_json_str = match right {
            Value::Json(j) => j,
            Value::Null => return Ok(Value::Boolean(true)), // NULL is contained in any JSON
            _ => return Err(Error::query_execution(format!(
                "JSON contains operator requires JSON operands, got {:?}",
                right
            ))),
        };

        // Parse JSON strings to serde_json::Value
        let left_json: serde_json::Value = serde_json::from_str(left_json_str)
            .map_err(|e| Error::query_execution(format!("Invalid JSON: {}", e)))?;
        let right_json: serde_json::Value = serde_json::from_str(right_json_str)
            .map_err(|e| Error::query_execution(format!("Invalid JSON: {}", e)))?;

        Ok(Value::Boolean(json_contains(&left_json, &right_json)))
    }

    /// JSON exists operator: ? or ?|
    /// Checks if key(s) exist in JSON object
    fn json_exists_op(&self, json_val: &Value, key_val: &Value, any: bool) -> Result<Value> {
        let json_str = match json_val {
            Value::Json(j) => j,
            Value::Null => return Ok(Value::Boolean(false)),
            _ => return Err(Error::query_execution(format!(
                "JSON exists operator requires JSON operand, got {:?}",
                json_val
            ))),
        };

        // Parse JSON string to serde_json::Value
        let json: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| Error::query_execution(format!("Invalid JSON: {}", e)))?;

        let obj = match json.as_object() {
            Some(o) => o,
            None => return Ok(Value::Boolean(false)),
        };

        match key_val {
            Value::String(key) => {
                Ok(Value::Boolean(obj.contains_key(key.as_str())))
            }
            Value::Array(keys) => {
                // For ?| (any), return true if any key exists
                for key in keys {
                    if let Value::String(k) = key {
                        if obj.contains_key(k.as_str()) {
                            if any {
                                return Ok(Value::Boolean(true));
                            }
                        } else if !any {
                            // For ?&, if any key is missing, return false
                            return Ok(Value::Boolean(false));
                        }
                    }
                }
                // If any==true and we get here, no keys matched
                // If any==false and we get here, all keys matched
                Ok(Value::Boolean(!any))
            }
            _ => Err(Error::query_execution(format!(
                "JSON exists operator requires string or array, got {:?}",
                key_val
            ))),
        }
    }

    /// JSON exists all operator: ?&
    /// Checks if all keys exist in JSON object
    fn json_exists_all_op(&self, json_val: &Value, keys_val: &Value) -> Result<Value> {
        let json_str = match json_val {
            Value::Json(j) => j,
            Value::Null => return Ok(Value::Boolean(false)),
            _ => return Err(Error::query_execution(format!(
                "JSON exists operator requires JSON operand, got {:?}",
                json_val
            ))),
        };

        // Parse JSON string to serde_json::Value
        let json: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| Error::query_execution(format!("Invalid JSON: {}", e)))?;

        let obj = match json.as_object() {
            Some(o) => o,
            None => return Ok(Value::Boolean(false)),
        };

        let keys = match keys_val {
            Value::Array(k) => k,
            _ => return Err(Error::query_execution(format!(
                "?& operator requires array operand, got {:?}",
                keys_val
            ))),
        };

        // Check if all keys exist
        for key in keys {
            if let Value::String(k) = key {
                if !obj.contains_key(k.as_str()) {
                    return Ok(Value::Boolean(false));
                }
            }
        }

        Ok(Value::Boolean(true))
    }

    /// Cast a value to a target data type
    pub fn cast_value(&self, value: Value, target_type: &DataType) -> Result<Value> {
        use crate::DataType;

        // NULL casts to NULL for any type
        if matches!(value, Value::Null) {
            return Ok(Value::Null);
        }

        match target_type {
            DataType::Boolean => match value {
                Value::Boolean(b) => Ok(Value::Boolean(b)),
                Value::Int4(i) => Ok(Value::Boolean(i != 0)),
                Value::Int8(i) => Ok(Value::Boolean(i != 0)),
                Value::String(s) => {
                    let s_lower = s.to_lowercase();
                    match s_lower.as_str() {
                        "true" | "t" | "yes" | "y" | "1" => Ok(Value::Boolean(true)),
                        "false" | "f" | "no" | "n" | "0" => Ok(Value::Boolean(false)),
                        _ => Err(Error::query_execution(format!("Cannot cast '{}' to BOOLEAN", s))),
                    }
                }
                _ => Err(Error::query_execution(format!("Cannot cast {:?} to BOOLEAN", value))),
            },

            DataType::Int2 => match value {
                Value::Int2(i) => Ok(Value::Int2(i)),
                Value::Int4(i) => Ok(Value::Int2(i as i16)),
                Value::Int8(i) => Ok(Value::Int2(i as i16)),
                Value::Float4(f) => Ok(Value::Int2(f as i16)),
                Value::Float8(f) => Ok(Value::Int2(f as i16)),
                Value::Numeric(n) => {
                    // Parse as decimal, truncate to integer, then to i16
                    n.parse::<Decimal>()
                        .map_err(|e| Error::query_execution(format!("Cannot cast '{}' to INT2: {}", n, e)))
                        .and_then(|dec| {
                            // Truncate decimal to integer
                            let int_val = dec.trunc().to_i128().unwrap_or(0);
                            if int_val >= i16::MIN as i128 && int_val <= i16::MAX as i128 {
                                Ok(Value::Int2(int_val as i16))
                            } else {
                                Err(Error::query_execution(format!("Numeric value {} out of range for INT2", n)))
                            }
                        })
                }
                Value::String(s) => s.parse::<i16>()
                    .map(Value::Int2)
                    .map_err(|e| Error::query_execution(format!("Cannot cast '{}' to INT2: {}", s, e))),
                _ => Err(Error::query_execution(format!("Cannot cast {:?} to INT2", value))),
            },

            DataType::Int4 => match value {
                Value::Int2(i) => Ok(Value::Int4(i as i32)),
                Value::Int4(i) => Ok(Value::Int4(i)),
                Value::Int8(i) => Ok(Value::Int4(i as i32)),
                Value::Float4(f) => Ok(Value::Int4(f as i32)),
                Value::Float8(f) => Ok(Value::Int4(f as i32)),
                Value::Numeric(n) => {
                    // Parse as decimal, truncate to integer, then to i32
                    n.parse::<Decimal>()
                        .map_err(|e| Error::query_execution(format!("Cannot cast '{}' to INT4: {}", n, e)))
                        .and_then(|dec| {
                            // Truncate decimal to integer
                            let int_val = dec.trunc().to_i128().unwrap_or(0);
                            if int_val >= i32::MIN as i128 && int_val <= i32::MAX as i128 {
                                Ok(Value::Int4(int_val as i32))
                            } else {
                                Err(Error::query_execution(format!("Numeric value {} out of range for INT4", n)))
                            }
                        })
                }
                Value::String(s) => s.parse::<i32>()
                    .map(Value::Int4)
                    .map_err(|e| Error::query_execution(format!("Cannot cast '{}' to INT4: {}", s, e))),
                _ => Err(Error::query_execution(format!("Cannot cast {:?} to INT4", value))),
            },

            DataType::Int8 => match value {
                Value::Int2(i) => Ok(Value::Int8(i as i64)),
                Value::Int4(i) => Ok(Value::Int8(i as i64)),
                Value::Int8(i) => Ok(Value::Int8(i)),
                Value::Float4(f) => Ok(Value::Int8(f as i64)),
                Value::Float8(f) => Ok(Value::Int8(f as i64)),
                Value::Numeric(n) => {
                    // Parse as decimal, truncate to integer, then to i64
                    n.parse::<Decimal>()
                        .map_err(|e| Error::query_execution(format!("Cannot cast '{}' to INT8: {}", n, e)))
                        .and_then(|dec| {
                            // Truncate decimal to integer
                            let int_val = dec.trunc().to_i128().unwrap_or(0);
                            if int_val >= i64::MIN as i128 && int_val <= i64::MAX as i128 {
                                Ok(Value::Int8(int_val as i64))
                            } else {
                                Err(Error::query_execution(format!("Numeric value {} out of range for INT8", n)))
                            }
                        })
                }
                Value::String(s) => s.parse::<i64>()
                    .map(Value::Int8)
                    .map_err(|e| Error::query_execution(format!("Cannot cast '{}' to INT8: {}", s, e))),
                _ => Err(Error::query_execution(format!("Cannot cast {:?} to INT8", value))),
            },

            DataType::Float4 => match value {
                Value::Int2(i) => Ok(Value::Float4(i as f32)),
                Value::Int4(i) => Ok(Value::Float4(i as f32)),
                Value::Int8(i) => Ok(Value::Float4(i as f32)),
                Value::Float4(f) => Ok(Value::Float4(f)),
                Value::Float8(f) => Ok(Value::Float4(f as f32)),
                Value::Numeric(n) => {
                    // Parse as decimal and convert to f32
                    n.parse::<Decimal>()
                        .map_err(|e| Error::query_execution(format!("Cannot cast '{}' to FLOAT4: {}", n, e)))
                        .map(|dec| Value::Float4(dec.to_f32().unwrap_or(0.0)))
                }
                Value::String(s) => s.parse::<f32>()
                    .map(Value::Float4)
                    .map_err(|e| Error::query_execution(format!("Cannot cast '{}' to FLOAT4: {}", s, e))),
                _ => Err(Error::query_execution(format!("Cannot cast {:?} to FLOAT4", value))),
            },

            DataType::Float8 => match value {
                Value::Int2(i) => Ok(Value::Float8(i as f64)),
                Value::Int4(i) => Ok(Value::Float8(i as f64)),
                Value::Int8(i) => Ok(Value::Float8(i as f64)),
                Value::Float4(f) => Ok(Value::Float8(f as f64)),
                Value::Float8(f) => Ok(Value::Float8(f)),
                Value::Numeric(n) => {
                    // Parse as decimal and convert to f64
                    n.parse::<Decimal>()
                        .map_err(|e| Error::query_execution(format!("Cannot cast '{}' to FLOAT8: {}", n, e)))
                        .map(|dec| Value::Float8(dec.to_f64().unwrap_or(0.0)))
                }
                Value::String(s) => s.parse::<f64>()
                    .map(Value::Float8)
                    .map_err(|e| Error::query_execution(format!("Cannot cast '{}' to FLOAT8: {}", s, e))),
                _ => Err(Error::query_execution(format!("Cannot cast {:?} to FLOAT8", value))),
            },

            DataType::Text | DataType::Varchar(_) => {
                // Most types can be converted to text
                Ok(Value::String(value.to_string()))
            },

            DataType::Vector(dimension) => match value {
                Value::Vector(v) => {
                    if v.len() == *dimension {
                        Ok(Value::Vector(v))
                    } else {
                        Err(Error::query_execution(format!(
                            "Vector dimension mismatch: got {}, expected {}",
                            v.len(), dimension
                        )))
                    }
                }
                Value::String(s) => {
                    // Parse string as vector: "[1.0, 2.0, 3.0]" or "1.0, 2.0, 3.0"
                    let trimmed = s.trim();
                    let without_brackets = trimmed.trim_start_matches('[').trim_end_matches(']');

                    let elements: Result<Vec<f32>> = without_brackets
                        .split(',')
                        .map(|elem| {
                            elem.trim()
                                .parse::<f32>()
                                .map_err(|e| Error::query_execution(format!("Invalid vector element '{}': {}", elem, e)))
                        })
                        .collect();

                    let vec = elements?;
                    if vec.len() != *dimension {
                        return Err(Error::query_execution(format!(
                            "Vector dimension mismatch: got {}, expected {}",
                            vec.len(), dimension
                        )));
                    }
                    Ok(Value::Vector(vec))
                }
                _ => Err(Error::query_execution(format!("Cannot cast {:?} to VECTOR({})", value, dimension))),
            },

            DataType::Json => match value {
                Value::Json(j) => Ok(Value::Json(j)),
                Value::String(s) => {
                    // Validate JSON string by parsing, then store original string
                    serde_json::from_str::<serde_json::Value>(&s)
                        .map(|_| Value::Json(s))
                        .map_err(|e| Error::query_execution(format!("Invalid JSON string: {}", e)))
                }
                _ => Err(Error::query_execution(format!("Cannot cast {:?} to JSON", value))),
            },

            DataType::Jsonb => match value {
                Value::Json(j) => Ok(Value::Json(j)), // JSONB and JSON share same in-memory representation
                Value::String(s) => {
                    // Validate JSONB string by parsing, then store original string
                    serde_json::from_str::<serde_json::Value>(&s)
                        .map(|_| Value::Json(s))
                        .map_err(|e| Error::query_execution(format!("Invalid JSONB string: {}", e)))
                }
                _ => Err(Error::query_execution(format!("Cannot cast {:?} to JSONB", value))),
            },

            DataType::Numeric => match value {
                // Numeric to Numeric: validate and preserve
                Value::Numeric(n) => Ok(Value::Numeric(n)),
                // Integer to Numeric
                Value::Int2(i) => Ok(Value::Numeric(format!("{}", i))),
                Value::Int4(i) => Ok(Value::Numeric(format!("{}", i))),
                Value::Int8(i) => Ok(Value::Numeric(format!("{}", i))),
                // Float to Numeric: convert with precision loss warning (converted as string for precision)
                Value::Float4(f) => Ok(Value::Numeric(format!("{}", f))),
                Value::Float8(f) => Ok(Value::Numeric(format!("{}", f))),
                // String to Numeric: parse and validate
                Value::String(s) => {
                    // Validate that the string is a valid numeric value
                    s.parse::<Decimal>()
                        .map(|dec| Value::Numeric(format!("{}", dec)))
                        .map_err(|e| Error::query_execution(format!("Cannot cast '{}' to NUMERIC: {}", s, e)))
                }
                _ => Err(Error::query_execution(format!("Cannot cast {:?} to NUMERIC", value))),
            },

            DataType::Date => match value {
                Value::Date(d) => Ok(Value::Date(d)),
                Value::Timestamp(ts) => Ok(Value::Date(ts.date_naive())),
                Value::String(s) => {
                    chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d")
                        .map(Value::Date)
                        .map_err(|e| Error::query_execution(format!("Cannot cast '{}' to DATE: {}", s, e)))
                }
                _ => Err(Error::query_execution(format!("Cannot cast {:?} to DATE", value))),
            },

            DataType::Time => match value {
                Value::Time(t) => Ok(Value::Time(t)),
                Value::Timestamp(ts) => Ok(Value::Time(ts.time())),
                Value::String(s) => {
                    chrono::NaiveTime::parse_from_str(&s, "%H:%M:%S")
                        .or_else(|_| chrono::NaiveTime::parse_from_str(&s, "%H:%M:%S%.f"))
                        .map(Value::Time)
                        .map_err(|e| Error::query_execution(format!("Cannot cast '{}' to TIME: {}", s, e)))
                }
                _ => Err(Error::query_execution(format!("Cannot cast {:?} to TIME", value))),
            },

            DataType::Timestamp | DataType::Timestamptz => match value {
                Value::Timestamp(ts) => Ok(Value::Timestamp(ts)),
                Value::Date(d) => {
                    // Convert date to timestamp at midnight UTC
                    let datetime = d.and_hms_opt(0, 0, 0)
                        .ok_or_else(|| Error::query_execution("Invalid date for timestamp conversion"))?;
                    Ok(Value::Timestamp(chrono::DateTime::from_naive_utc_and_offset(datetime, Utc)))
                }
                Value::String(s) => {
                    // Try RFC3339 format first, then common formats
                    chrono::DateTime::parse_from_rfc3339(&s)
                        .map(|ts| Value::Timestamp(ts.with_timezone(&Utc)))
                        .or_else(|_| {
                            chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S")
                                .map(|ndt| Value::Timestamp(chrono::DateTime::from_naive_utc_and_offset(ndt, Utc)))
                        })
                        .or_else(|_| {
                            chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S%.f")
                                .map(|ndt| Value::Timestamp(chrono::DateTime::from_naive_utc_and_offset(ndt, Utc)))
                        })
                        .map_err(|e| Error::query_execution(format!("Cannot cast '{}' to TIMESTAMP: {}", s, e)))
                }
                _ => Err(Error::query_execution(format!("Cannot cast {:?} to TIMESTAMP", value))),
            },

            _ => Err(Error::query_execution(format!(
                "CAST to {:?} not yet implemented",
                target_type
            ))),
        }
    }

    /// Array subscript operator: arr[n]
    /// Returns the nth element of an array (1-based indexing like PostgreSQL)
    fn evaluate_array_subscript(&self, array: &Value, index: &Value) -> Result<Value> {
        match (array, index) {
            (Value::Array(arr), Value::Int2(idx)) => {
                self.get_array_element(arr, *idx as i64)
            }
            (Value::Array(arr), Value::Int4(idx)) => {
                self.get_array_element(arr, *idx as i64)
            }
            (Value::Array(arr), Value::Int8(idx)) => {
                self.get_array_element(arr, *idx)
            }
            (Value::Null, _) => Ok(Value::Null),
            (_, Value::Null) => Ok(Value::Null),
            _ => Err(Error::query_execution(format!(
                "Array subscript requires array and integer index, got {:?}[{:?}]",
                array, index
            ))),
        }
    }

    /// Get element from array using 1-based index (PostgreSQL style)
    fn get_array_element(&self, arr: &[Value], idx: i64) -> Result<Value> {
        // PostgreSQL uses 1-based indexing
        if idx < 1 {
            // Out of bounds, return NULL
            Ok(Value::Null)
        } else {
            let zero_based_idx = (idx - 1) as usize;
            Ok(arr.get(zero_based_idx).cloned().unwrap_or(Value::Null))
        }
    }

    /// Compare two values for equality (used by IN list evaluation)
    /// Handles type coercion for common numeric comparisons
    fn values_equal(&self, left: &Value, right: &Value) -> bool {
        match (left, right) {
            // Exact matches
            (Value::Int2(a), Value::Int2(b)) => a == b,
            (Value::Int4(a), Value::Int4(b)) => a == b,
            (Value::Int8(a), Value::Int8(b)) => a == b,
            (Value::Float4(a), Value::Float4(b)) => a == b,
            (Value::Float8(a), Value::Float8(b)) => a == b,
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Boolean(a), Value::Boolean(b)) => a == b,
            (Value::Numeric(a), Value::Numeric(b)) => a == b,
            (Value::Uuid(a), Value::Uuid(b)) => a == b,
            (Value::Date(a), Value::Date(b)) => a == b,
            (Value::Time(a), Value::Time(b)) => a == b,
            (Value::Timestamp(a), Value::Timestamp(b)) => a == b,

            // Cross-type numeric comparisons (coerce to f64)
            (Value::Int2(a), Value::Int4(b)) => (*a as i32) == *b,
            (Value::Int4(a), Value::Int2(b)) => *a == (*b as i32),
            (Value::Int2(a), Value::Int8(b)) => (*a as i64) == *b,
            (Value::Int8(a), Value::Int2(b)) => *a == (*b as i64),
            (Value::Int4(a), Value::Int8(b)) => (*a as i64) == *b,
            (Value::Int8(a), Value::Int4(b)) => *a == (*b as i64),

            // Int to Float comparisons
            (Value::Int2(a), Value::Float4(b)) => (*a as f32) == *b,
            (Value::Float4(a), Value::Int2(b)) => *a == (*b as f32),
            (Value::Int4(a), Value::Float4(b)) => (*a as f32) == *b,
            (Value::Float4(a), Value::Int4(b)) => *a == (*b as f32),
            (Value::Int2(a), Value::Float8(b)) => (*a as f64) == *b,
            (Value::Float8(a), Value::Int2(b)) => *a == (*b as f64),
            (Value::Int4(a), Value::Float8(b)) => (*a as f64) == *b,
            (Value::Float8(a), Value::Int4(b)) => *a == (*b as f64),
            (Value::Int8(a), Value::Float4(b)) => (*a as f32) == *b,
            (Value::Float4(a), Value::Int8(b)) => *a == (*b as f32),
            (Value::Int8(a), Value::Float8(b)) => (*a as f64) == *b,
            (Value::Float8(a), Value::Int8(b)) => *a == (*b as f64),

            // Float to Float
            (Value::Float4(a), Value::Float8(b)) => (*a as f64) == *b,
            (Value::Float8(a), Value::Float4(b)) => *a == (*b as f64),

            // Null comparisons (SQL: NULL = anything is false, not NULL)
            (Value::Null, _) | (_, Value::Null) => false,

            // Default: not equal
            _ => false,
        }
    }

    /// Array concatenation operator: arr1 || arr2
    /// Concatenates two arrays into a single array
    fn array_concat_op(&self, left: &Value, right: &Value) -> Result<Value> {
        match (left, right) {
            (Value::Array(left_arr), Value::Array(right_arr)) => {
                // Concatenate arrays
                let mut result = left_arr.clone();
                result.extend(right_arr.clone());
                Ok(Value::Array(result))
            }
            (Value::Array(left_arr), right_val) => {
                // Single value concatenation: arr || value
                let mut result = left_arr.clone();
                result.push(right_val.clone());
                Ok(Value::Array(result))
            }
            (left_val, Value::Array(right_arr)) => {
                // Single value concatenation: value || arr
                let mut result = vec![left_val.clone()];
                result.extend(right_arr.clone());
                Ok(Value::Array(result))
            }
            (Value::Null, right) => Ok(right.clone()),
            (left, Value::Null) => Ok(left.clone()),
            _ => Err(Error::query_execution(format!(
                "Array concatenation requires arrays or array-compatible types, got {:?} || {:?}",
                left, right
            ))),
        }
    }
}

/// Check if left JSON contains right JSON (recursive containment check)
fn json_contains(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    use serde_json::Value as JV;

    match (left, right) {
        // Exact match
        (l, r) if l == r => true,

        // Object containment: all key-value pairs in right must be in left
        (JV::Object(left_obj), JV::Object(right_obj)) => {
            right_obj.iter().all(|(key, right_val)| {
                left_obj.get(key).map_or(false, |left_val| json_contains(left_val, right_val))
            })
        }

        // Array containment: all elements in right must be in left
        (JV::Array(left_arr), JV::Array(right_arr)) => {
            right_arr.iter().all(|right_elem| {
                left_arr.iter().any(|left_elem| json_contains(left_elem, right_elem))
            })
        }

        // Otherwise, no containment
        _ => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{Column, DataType};
    use crate::sql::BinaryOperator;

    fn test_schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Column {
                name: "id".to_string(),
                data_type: DataType::Int4,
                nullable: false,
                primary_key: true,
                source_table: None,
                source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: crate::ColumnStorageMode::Default,
            },
            Column {
                name: "age".to_string(),
                data_type: DataType::Int4,
                nullable: true,
                primary_key: false,
                source_table: None,
                source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: crate::ColumnStorageMode::Default,
            },
            Column {
                name: "name".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                source_table: None,
                source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: crate::ColumnStorageMode::Default,
            },
        ]))
    }

    #[test]
    fn test_literal_evaluation() {
        let schema = test_schema();
        let evaluator = Evaluator::new(schema);
        let tuple = Tuple::new(vec![Value::Int4(1), Value::Int4(30), Value::String("Alice".to_string())]);

        let expr = LogicalExpr::Literal(Value::Int4(42));
        let result = evaluator.evaluate(&expr, &tuple)
            .expect("Failed to evaluate literal expression");
        assert_eq!(result, Value::Int4(42));
    }

    #[test]
    fn test_column_evaluation() {
        let schema = test_schema();
        let evaluator = Evaluator::new(schema);
        let tuple = Tuple::new(vec![Value::Int4(1), Value::Int4(30), Value::String("Alice".to_string())]);

        let expr = LogicalExpr::Column { table: None, name: "age".to_string()  };
        let result = evaluator.evaluate(&expr, &tuple)
            .expect("Failed to evaluate column expression");
        assert_eq!(result, Value::Int4(30));
    }

    #[test]
    fn test_comparison_operators() {
        let schema = test_schema();
        let evaluator = Evaluator::new(schema);
        let tuple = Tuple::new(vec![Value::Int4(1), Value::Int4(30), Value::String("Alice".to_string())]);

        // age = 30
        let expr = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column { table: None, name: "age".to_string()  }),
            op: BinaryOperator::Eq,
            right: Box::new(LogicalExpr::Literal(Value::Int4(30))),
        };
        let result = evaluator.evaluate(&expr, &tuple)
            .expect("Failed to evaluate comparison expression");
        assert_eq!(result, Value::Boolean(true));

        // age > 25
        let expr = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column { table: None, name: "age".to_string()  }),
            op: BinaryOperator::Gt,
            right: Box::new(LogicalExpr::Literal(Value::Int4(25))),
        };
        let result = evaluator.evaluate(&expr, &tuple)
            .expect("Failed to evaluate comparison expression");
        assert_eq!(result, Value::Boolean(true));
    }

    #[test]
    fn test_arithmetic_operators() {
        let schema = test_schema();
        let evaluator = Evaluator::new(schema);
        let tuple = Tuple::new(vec![Value::Int4(1), Value::Int4(30), Value::String("Alice".to_string())]);

        // age + 10
        let expr = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column { table: None, name: "age".to_string()  }),
            op: BinaryOperator::Plus,
            right: Box::new(LogicalExpr::Literal(Value::Int4(10))),
        };
        let result = evaluator.evaluate(&expr, &tuple)
            .expect("Failed to evaluate arithmetic expression");
        assert_eq!(result, Value::Int4(40));
    }
}
