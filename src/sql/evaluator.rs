//! Expression evaluation
//!
//! Evaluates logical expressions against tuples to produce values.

use crate::{Result, Error, Value, Tuple, Schema, DataType};
use crate::tenant::{get_current_tenant_id, get_current_user_id};
use super::LogicalExpr;
use chrono::{Utc, Local, Datelike, Timelike};
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
                // AND/OR require short-circuit evaluation with SQL three-valued logic.
                // We must not evaluate the right side if the left side already
                // determines the result (e.g., `val IS NOT NULL AND val != 10`
                // must skip `val != 10` when `val IS NOT NULL` is false).
                match op {
                    super::BinaryOperator::And => {
                        return self.evaluate_and_short_circuit(left, right, tuple);
                    }
                    super::BinaryOperator::Or => {
                        return self.evaluate_or_short_circuit(left, right, tuple);
                    }
                    _ => {}
                }
                // Row-constructor comparison: `(a, b) <op> (c, d)` evaluates
                // each side element-wise and compares lexicographically.
                // Used for keyset pagination `WHERE (col, id) < ($1, $2)`.
                if let (LogicalExpr::Tuple { items: l_items }, LogicalExpr::Tuple { items: r_items }) =
                    (left.as_ref(), right.as_ref())
                {
                    return self.evaluate_tuple_compare(l_items, op, r_items, tuple);
                }
                let left_val = self.evaluate(left, tuple)?;
                let right_val = self.evaluate(right, tuple)?;
                self.evaluate_binary_op(&left_val, op, &right_val)
            }

            LogicalExpr::Tuple { .. } => {
                // Bare tuples only make sense inside row-constructor
                // comparisons, which we intercept in `BinaryExpr` above.
                Err(Error::query_execution(
                    "Row constructor used outside a comparison — expected (a, b) <op> (c, d)",
                ))
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

            LogicalExpr::InSet { expr, values, negated } => {
                let value = self.evaluate(expr, tuple)?;
                if matches!(value, Value::Null) {
                    return Ok(Value::Null);
                }
                // Use values_equal for cross-type coercion (String("9") matches Int8(9))
                let found = values.iter().any(|v| self.values_equal(&value, v));
                Ok(Value::Boolean(if *negated { !found } else { found }))
            }

            LogicalExpr::InSubquery { .. } => {
                // Subquery evaluation requires executor context
                // This should be handled at the executor level, not evaluator
                Err(Error::query_execution(
                    "IN subquery evaluation requires executor context. Use executor for subquery evaluation."
                ))
            }

            LogicalExpr::DefaultValue => {
                // `DEFAULT` keyword appearing outside an INSERT VALUES
                // list — there's no target column to resolve against.
                Err(Error::query_execution(
                    "DEFAULT keyword is only valid inside INSERT … VALUES (…)"
                ))
            }

            LogicalExpr::ScalarSubquery { .. } => {
                // Scalar subquery must be materialised before evaluation.
                // Uncorrelated ones are materialised by
                // `Executor::materialize_subqueries`; correlated ones
                // by the caller (see the UPDATE path).
                Err(Error::query_execution(
                    "Scalar subquery reached the evaluator without materialisation. \
                     Correlated subqueries are only supported in UPDATE SET at the moment; \
                     rewrite other uses as UPDATE … FROM joins or plain SELECT expressions."
                ))
            }

            LogicalExpr::Exists { .. } => {
                // EXISTS evaluation requires executor context
                // This should be handled at the executor level, not evaluator
                Err(Error::query_execution(
                    "EXISTS subquery evaluation requires executor context. Use executor for subquery evaluation."
                ))
            }

            LogicalExpr::Between { expr, low, high, negated } => {
                let value = self.evaluate(expr, tuple)?;
                let low_value = self.evaluate(low, tuple)?;
                let high_value = self.evaluate(high, tuple)?;

                // NULL handling: if any value is NULL, result is NULL
                if matches!(value, Value::Null) || matches!(low_value, Value::Null) || matches!(high_value, Value::Null) {
                    return Ok(Value::Null);
                }

                // value BETWEEN low AND high is equivalent to: value >= low AND value <= high
                let gte_low = self.compare_values(&value, &low_value, |ord| ord != std::cmp::Ordering::Less)?;
                let lte_high = self.compare_values(&value, &high_value, |ord| ord != std::cmp::Ordering::Greater)?;

                // Both comparisons must be true for value to be in range
                let in_range = matches!(gte_low, Value::Boolean(true)) && matches!(lte_high, Value::Boolean(true));
                let result = if *negated { !in_range } else { in_range };

                Ok(Value::Boolean(result))
            }

            LogicalExpr::Case { expr: operand, when_then, else_result } => {
                // If there's an operand, we're doing: CASE operand WHEN val THEN result...
                // Otherwise, we're doing: CASE WHEN condition THEN result...
                if let Some(op) = operand {
                    let op_value = self.evaluate(op, tuple)?;

                    for (when_expr, then_expr) in when_then {
                        let when_value = self.evaluate(when_expr, tuple)?;
                        if self.values_equal(&op_value, &when_value) {
                            return self.evaluate(then_expr, tuple);
                        }
                    }
                } else {
                    // Searched CASE: CASE WHEN condition THEN result...
                    for (when_expr, then_expr) in when_then {
                        let condition = self.evaluate(when_expr, tuple)?;
                        if matches!(condition, Value::Boolean(true)) {
                            return self.evaluate(then_expr, tuple);
                        }
                    }
                }

                // No condition matched, return ELSE result or NULL
                if let Some(else_expr) = else_result {
                    self.evaluate(else_expr, tuple)
                } else {
                    Ok(Value::Null)
                }
            }

            LogicalExpr::WindowFunction { .. } => {
                // Window functions cannot be evaluated row-by-row
                // They need access to all rows in a partition and are handled
                // by the WindowOperator in the executor
                Err(Error::query_execution(
                    "Window functions must be evaluated by WindowOperator, not row-by-row"
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
            "jsonb_path_query_array" => {
                self.jsonb_path_query_array(&arg_values)
            }
            "jsonb_path_query_first" => {
                self.jsonb_path_query_first(&arg_values)
            }
            "jsonb_path_exists" => {
                self.jsonb_path_exists(&arg_values)
            }
            "jsonb_path_match" => {
                self.jsonb_path_match(&arg_values)
            }

            // JSONB formatting functions
            "jsonb_pretty" => {
                self.jsonb_pretty(&arg_values)
            }
            "jsonb_strip_nulls" => {
                self.jsonb_strip_nulls(&arg_values)
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

            // NULL handling functions
            "coalesce" => self.func_coalesce(&arg_values),
            "nullif" => self.func_nullif(&arg_values),
            "ifnull" | "nvl" => self.func_coalesce(&arg_values), // Aliases for COALESCE(a, b)

            // Array functions (PostgreSQL compatible)
            "array_length" => self.array_length(&arg_values),
            "array_upper" => self.array_upper(&arg_values),
            "array_lower" => self.array_lower(&arg_values),
            "array_append" => self.array_append(&arg_values),
            "array_prepend" => self.array_prepend(&arg_values),
            "array_cat" => self.array_cat(&arg_values),
            "array_remove" => self.array_remove(&arg_values),
            "array_position" => self.array_position(&arg_values),
            "cardinality" => self.array_cardinality(&arg_values),

            // String functions
            "upper" => self.func_upper(&arg_values),
            "lower" => self.func_lower(&arg_values),
            "length" | "char_length" | "character_length" => self.func_length(&arg_values),
            "substr" | "substring" => self.func_substr(&arg_values),
            "trim" => self.func_trim(&arg_values, None),
            "ltrim" => self.func_trim(&arg_values, Some("left")),
            "rtrim" => self.func_trim(&arg_values, Some("right")),
            "btrim" => self.func_trim(&arg_values, Some("both")),
            "concat" => self.func_concat(&arg_values),
            "concat_ws" => self.func_concat_ws(&arg_values),
            "left" => self.func_left(&arg_values),
            "right" => self.func_right(&arg_values),
            "repeat" => self.func_repeat(&arg_values),
            "replace" => self.func_replace(&arg_values),
            "reverse" => self.func_reverse(&arg_values),
            "position" | "strpos" => self.func_position(&arg_values),
            "split_part" => self.func_split_part(&arg_values),
            "initcap" => self.func_initcap(&arg_values),
            "lpad" => self.func_lpad(&arg_values),
            "rpad" => self.func_rpad(&arg_values),

            // Math functions
            "abs" => self.func_abs(&arg_values),
            "round" => self.func_round(&arg_values),
            "ceil" | "ceiling" => self.func_ceil(&arg_values),
            "floor" => self.func_floor(&arg_values),
            "trunc" | "truncate" => self.func_trunc(&arg_values),
            "sqrt" => self.func_sqrt(&arg_values),
            "power" | "pow" => self.func_power(&arg_values),
            "mod" => self.func_mod(&arg_values),
            "sign" => self.func_sign(&arg_values),
            "greatest" => self.func_greatest(&arg_values),
            "least" => self.func_least(&arg_values),
            "random" => self.func_random(&arg_values),
            "pi" => Ok(Value::Float8(std::f64::consts::PI)),
            "exp" => self.func_exp(&arg_values),
            "ln" | "log" => self.func_ln(&arg_values),
            "log10" => self.func_log10(&arg_values),
            "sin" => self.func_sin(&arg_values),
            "cos" => self.func_cos(&arg_values),
            "tan" => self.func_tan(&arg_values),
            "asin" => self.func_asin(&arg_values),
            "acos" => self.func_acos(&arg_values),
            "atan" => self.func_atan(&arg_values),
            "atan2" => self.func_atan2(&arg_values),
            "degrees" => self.func_degrees(&arg_values),
            "radians" => self.func_radians(&arg_values),

            // MySQL date/time functions (WordPress compatibility)
            "date_format" => self.func_date_format(&arg_values),
            "date" => self.func_date_extract(&arg_values),
            "year" => self.func_year(&arg_values),
            "month" => self.func_month(&arg_values),
            "day" | "dayofmonth" => self.func_day(&arg_values),
            "date_add" | "adddate" => self.func_date_add(&arg_values),
            "date_sub" | "subdate" => self.func_date_sub(&arg_values),
            "datediff" => self.func_datediff(&arg_values),
            "timestampdiff" => self.func_timestampdiff(&arg_values),
            "unix_timestamp" => self.func_unix_timestamp(&arg_values),
            "from_unixtime" => self.func_from_unixtime(&arg_values),

            // PostgreSQL date/time surface — drop-in for any client expecting
            // PG date semantics (psycopg, Drizzle, Prisma, sqlx, …) and the
            // canonical home for date formatting now that we no longer carry
            // SQLite-specific names like STRFTIME / JULIANDAY.
            "to_char"        => self.func_to_char(&arg_values),
            "to_date"        => self.func_to_date(&arg_values),
            "to_timestamp"   => self.func_to_timestamp(&arg_values),
            "date_trunc"     => self.func_date_trunc(&arg_values),
            "make_date"      => self.func_make_date(&arg_values),
            "make_timestamp" => self.func_make_timestamp(&arg_values),
            "age"            => self.func_age(&arg_values),
            "date_part"      => {
                // PostgreSQL alias: date_part('field', expr) ≡ EXTRACT(field FROM expr).
                let [field_arg, val_arg] = arg_values.as_slice() else {
                    return Err(Error::query_execution(
                        "DATE_PART requires exactly 2 arguments"
                    ));
                };
                let field = match field_arg {
                    Value::String(s) => s.to_lowercase(),
                    Value::Null => return Ok(Value::Null),
                    other => return Err(Error::query_execution(format!(
                        "DATE_PART field must be a string, got {:?}", other
                    ))),
                };
                Self::extract_field(&field, std::slice::from_ref(val_arg))
            }

            // MySQL string functions (WordPress plugin compatibility)
            "locate" => self.func_locate(&arg_values),
            "instr" => self.func_instr(&arg_values),

            // PostgreSQL compatibility functions (SQLAlchemy, psql, pgAdmin, DBeaver)
            "version" | "pg_catalog.version" => {
                Ok(Value::String(format!(
                    "PostgreSQL 16.0 (HeliosDB Nano {})",
                    env!("CARGO_PKG_VERSION")
                )))
            }
            "current_schema" => {
                Ok(Value::String("public".to_string()))
            }
            "current_database" => {
                Ok(Value::String("heliosdb".to_string()))
            }
            "current_user" | "session_user" => {
                Ok(Value::String("heliosdb".to_string()))
            }
            // Random UUID v4 — the default for Postgres 13+ PK columns.
            "gen_random_uuid" | "pg_catalog.gen_random_uuid" | "uuid_generate_v4" => {
                Ok(Value::Uuid(uuid::Uuid::new_v4()))
            }

            // `nextval('seq')` / `currval('seq')` / `setval('seq', n)`.
            // Backed by the process-scoped sequence store in
            // `crate::sql::sequences`. Returns Int8 to match Postgres.
            "nextval" | "pg_catalog.nextval" => {
                let name = match arg_values.first() {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Null) => return Ok(Value::Null),
                    _ => return Err(Error::query_execution(
                        "nextval() requires a text argument (sequence name)",
                    )),
                };
                Ok(Value::Int8(crate::sql::sequences::nextval(&name)))
            }
            "currval" | "pg_catalog.currval" => {
                let name = match arg_values.first() {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Null) => return Ok(Value::Null),
                    _ => return Err(Error::query_execution(
                        "currval() requires a text argument (sequence name)",
                    )),
                };
                Ok(Value::Int8(crate::sql::sequences::currval(&name)))
            }
            "setval" | "pg_catalog.setval" => {
                let name = match arg_values.first() {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Null) => return Ok(Value::Null),
                    _ => return Err(Error::query_execution(
                        "setval() requires (text, integer) arguments",
                    )),
                };
                let value = match arg_values.get(1) {
                    Some(Value::Int8(n)) => *n,
                    Some(Value::Int4(n)) => *n as i64,
                    Some(Value::Int2(n)) => *n as i64,
                    _ => return Err(Error::query_execution(
                        "setval() second argument must be an integer",
                    )),
                };
                Ok(Value::Int8(crate::sql::sequences::setval(&name, value)))
            }
            // Self-introspection: summarise what HeliosDB Nano supports
            // vs. stock PostgreSQL. Useful for drivers / migration tools
            // to probe at connect-time without guessing.
            "heliosdb_capability_report" => {
                Ok(Value::String(concat!(
                    "HeliosDB Nano ", env!("CARGO_PKG_VERSION"), "\n",
                    "  SERIAL / BIGSERIAL / GENERATED AS IDENTITY  : yes\n",
                    "  ON CONFLICT DO NOTHING / DO UPDATE         : yes\n",
                    "  RETURNING *                                : yes\n",
                    "  EXTRACT(EPOCH|YEAR|MONTH|... FROM ...)     : yes\n",
                    "  gen_random_uuid() / uuid_generate_v4()     : yes\n",
                    "  Full-text search (tsvector/@@/ts_rank_cd)  : yes (unstemmed, no phrase)\n",
                    "  pg_catalog.pg_type / pg_tables / pg_indexes: yes\n",
                    "  Keyset pagination (row constructor <,<=,=) : yes\n",
                    "  Dollar-quoted strings $$text$$             : yes\n",
                    "  DO $$ plain-SQL body $$                    : yes (no PL/pgSQL control flow)\n",
                    "  Multi-statement simple query (Q message)   : yes\n",
                    "  Case-folding of unquoted identifiers       : yes (lowercase, PG-compatible)\n",
                    "  CREATE SEQUENCE / nextval / currval / setval: yes\n",
                    "  GIN / GiST indexes                         : DDL accepted, no backing store yet\n",
                    "  PL/pgSQL control flow (IF/LOOP/RAISE)      : no — use procedures\n",
                    "  Language-specific FTS stemmers             : no — tokenize + lowercase only\n",
                ).to_string()))
            }

            // EXTRACT(<field> FROM <expr>) — the planner lowers these
            // to `__extract_<field>(expr)` so we don't need a separate
            // match arm for Expr::Extract.
            f if f.starts_with("__extract_") => {
                let field = &f["__extract_".len()..];
                Self::extract_field(field, &arg_values)
            }

            // ---- Postgres full-text search (FTS) ----
            // `to_tsvector(text)` and `to_tsvector(config, text)`:
            // tokenise with the shared search::tokenizer and return a
            // JSON-encoded array of normalised tokens. The optional
            // `config` argument (e.g. 'english') is accepted for
            // compatibility but ignored — we use a single Unicode-word
            // tokenizer regardless.
            "to_tsvector" | "pg_catalog.to_tsvector" => {
                Self::fts_build_from_text(&arg_values, "to_tsvector")
            }
            // `to_tsquery(text)` / `plainto_tsquery(text)`: same encoding
            // as tsvector. `to_tsquery` normally understands `&`, `|`,
            // `!`, `<->`; we treat those as term separators (no boolean
            // logic, no phrase queries — documented limitation).
            "to_tsquery" | "plainto_tsquery" | "phraseto_tsquery"
            | "pg_catalog.to_tsquery" | "pg_catalog.plainto_tsquery" => {
                Self::fts_build_from_text(&arg_values, fun)
            }
            // `ts_rank_cd(doc, query)` / `ts_rank(doc, query)`: run BM25
            // against a 1-document index built on the fly. Returns
            // Float8 in the 0..~10 range, higher = more relevant.
            // Optional `weights` and `normalization` args are accepted
            // for signature compatibility but ignored.
            "ts_rank" | "ts_rank_cd" => {
                Self::fts_score(&arg_values)
            }

            _ => Err(Error::query_execution(format!(
                "Unknown scalar function: {}",
                fun
            ))),
        }
    }

    // ---- Postgres FTS helpers ----
    //
    // Internal representation of `tsvector` / `tsquery`:
    //   `Value::Json(serde_json::to_string(&["tok1","tok2",...]))`.
    // This lets the value flow through the whole pipeline (wire
    // protocol, storage, bincode) unchanged while still being
    // introspectable by psql and clients as a JSON array. Full Postgres
    // fidelity (positions, weights, phrase queries) is intentionally
    // out of scope — see docs/compatibility/fts.md.

    fn fts_build_from_text(args: &[Value], fn_name: &str) -> Result<Value> {
        // Accept (text) or (config, text); first arg is treated as
        // config iff there are exactly two args.
        let text_val = match args.len() {
            1 => &args[0],
            2 => &args[1],
            n => return Err(Error::query_execution(format!(
                "{fn_name} expects 1 or 2 arguments, got {n}"
            ))),
        };
        let text = match text_val {
            Value::Null => return Ok(Value::Null),
            Value::String(s) => s.as_str(),
            Value::Json(s) => s.as_str(),
            _ => return Err(Error::query_execution(format!(
                "{fn_name} expects text argument"
            ))),
        };
        let tokens = crate::search::tokenizer::tokenize(text);
        let json = serde_json::to_string(&tokens)
            .map_err(|e| Error::query_execution(format!("tsvector encode: {e}")))?;
        Ok(Value::Json(json))
    }

    /// Decode a tsvector/tsquery value back into a `Vec<String>`.
    /// Accepts:
    ///   • `Value::Json("[\"a\",\"b\"]")` — our canonical encoding.
    ///   • `Value::String("'a' 'b' 'c'")` — a Postgres-style tsvector
    ///     printout (tokens quoted, separated by spaces). We fall back
    ///     to plain tokenisation if parsing the quoted form fails.
    ///   • `Value::Array([String, ...])` — if someone hand-builds one.
    fn fts_decode_tokens(v: &Value) -> Vec<String> {
        match v {
            Value::Json(s) => serde_json::from_str::<Vec<String>>(s).unwrap_or_else(|_| {
                // Fall back: treat as raw text if JSON parse fails.
                crate::search::tokenizer::tokenize(s)
            }),
            Value::String(s) => crate::search::tokenizer::tokenize(s),
            Value::Array(items) => items.iter().filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            }).collect(),
            _ => Vec::new(),
        }
    }

    fn fts_score(args: &[Value]) -> Result<Value> {
        // Signature: ts_rank[_cd]([weights,] doc, query [, normalization])
        // We only use `doc` and `query` — the rest are accepted for
        // compatibility and ignored.
        let (doc, query) = match args.len() {
            2 => (&args[0], &args[1]),
            3 => (&args[0], &args[1]),      // weights, doc, query  — ignore weights[0]? no, skip
            4 => (&args[1], &args[2]),      // weights, doc, query, norm
            n => return Err(Error::query_execution(format!(
                "ts_rank expects 2..4 arguments, got {n}"
            ))),
        };
        // Handle the 3-arg form: PG's 3-arg signature is
        // `ts_rank(doc, query, normalization)`, so check if args[0] is
        // a tsvector-like value (Json/String with tokens).
        let (doc, query) = if args.len() == 3 {
            // Heuristic: if the first arg is a numeric array / weights,
            // skip it; otherwise args[0]=doc, args[1]=query, args[2]=norm.
            (doc, query)
        } else {
            (doc, query)
        };

        if matches!(doc, Value::Null) || matches!(query, Value::Null) {
            return Ok(Value::Null);
        }
        let doc_tokens = Self::fts_decode_tokens(doc);
        let q_tokens = Self::fts_decode_tokens(query);
        if doc_tokens.is_empty() || q_tokens.is_empty() {
            return Ok(Value::Float8(0.0));
        }
        // Build an ephemeral 1-doc BM25 index and score.
        let idx = crate::search::Bm25Index::new();
        idx.add_document(1, &doc_tokens.join(" "));
        let query_str = q_tokens.join(" ");
        let hits = idx.score(&query_str, Some(1));
        let score = hits.first().map(|h| h.score).unwrap_or(0.0);
        Ok(Value::Float8(score))
    }

    /// tsvector @@ tsquery: true iff any query term appears in the document.
    pub(crate) fn evaluate_ts_match(left: &Value, right: &Value) -> Result<Value> {
        if matches!(left, Value::Null) || matches!(right, Value::Null) {
            return Ok(Value::Null);
        }
        let doc_tokens = Self::fts_decode_tokens(left);
        let q_tokens = Self::fts_decode_tokens(right);
        if q_tokens.is_empty() {
            return Ok(Value::Boolean(false));
        }
        let doc_set: std::collections::HashSet<&str> =
            doc_tokens.iter().map(String::as_str).collect();
        let matched = q_tokens.iter().any(|t| doc_set.contains(t.as_str()));
        Ok(Value::Boolean(matched))
    }

    // ---- MySQL date/time helper functions (WordPress compatibility) ----

    /// Convert a Value to a `chrono::NaiveDateTime`, accepting Timestamp, Date, or String.
    fn value_to_naive_datetime(val: &Value) -> Result<chrono::NaiveDateTime> {
        match val {
            Value::Timestamp(ts) => Ok(ts.naive_utc()),
            Value::Date(d) => d.and_hms_opt(0, 0, 0)
                .ok_or_else(|| Error::query_execution("Invalid date for datetime conversion")),
            Value::String(s) => {
                chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                    .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f"))
                    .or_else(|e| {
                        match chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                            Ok(d) => match d.and_hms_opt(0, 0, 0) {
                                Some(ndt) => Ok(ndt),
                                None => Err(e),
                            },
                            Err(e2) => Err(e2),
                        }
                    })
                    .map_err(|e| Error::query_execution(format!("Cannot parse '{}' as datetime: {}", s, e)))
            }
            Value::Int8(epoch) => {
                chrono::DateTime::from_timestamp(*epoch, 0)
                    .map(|dt| dt.naive_utc())
                    .ok_or_else(|| Error::query_execution(format!("Invalid unix timestamp: {}", epoch)))
            }
            Value::Int4(epoch) => {
                chrono::DateTime::from_timestamp(i64::from(*epoch), 0)
                    .map(|dt| dt.naive_utc())
                    .ok_or_else(|| Error::query_execution(format!("Invalid unix timestamp: {}", epoch)))
            }
            Value::Null => Err(Error::query_execution("NULL datetime")),
            _ => Err(Error::query_execution(format!(
                "Cannot convert {:?} to datetime", val
            ))),
        }
    }

    /// Convert a Value to `chrono::NaiveDate`, accepting Date, Timestamp, or String.
    fn value_to_naive_date(val: &Value) -> Result<chrono::NaiveDate> {
        match val {
            Value::Date(d) => Ok(*d),
            Value::Timestamp(ts) => Ok(ts.date_naive()),
            Value::String(s) => {
                chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                    .or_else(|_| {
                        // Also accept datetime strings — just take the date part
                        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                            .map(|ndt| ndt.date())
                    })
                    .map_err(|e| Error::query_execution(format!("Cannot parse '{}' as date: {}", s, e)))
            }
            Value::Null => Err(Error::query_execution("NULL date")),
            _ => Err(Error::query_execution(format!(
                "Cannot convert {:?} to date", val
            ))),
        }
    }

    /// Convert MySQL format specifiers to chrono format specifiers.
    ///
    /// MySQL and chrono share some specifiers but differ on others.
    /// Replacement order matters to avoid conflicts (e.g., MySQL `%i` = minutes,
    /// chrono `%M` = minutes, but MySQL `%M` = month name).
    fn mysql_format_to_chrono(format: &str) -> String {
        // We process character by character to avoid replacement conflicts.
        let mut result = String::with_capacity(format.len());
        let mut chars = format.chars();
        while let Some(ch) = chars.next() {
            if ch == '%' {
                match chars.next() {
                    Some('Y') => result.push_str("%Y"),  // 4-digit year
                    Some('y') => result.push_str("%y"),  // 2-digit year
                    Some('m') => result.push_str("%m"),  // month 01-12
                    Some('c') => result.push_str("%-m"), // month 1-12 (no leading zero)
                    Some('d') => result.push_str("%d"),  // day 01-31
                    Some('e') => result.push_str("%-d"), // day 1-31 (no leading zero)
                    Some('H') => result.push_str("%H"),  // hour 00-23
                    Some('h') | Some('I') => result.push_str("%I"), // hour 01-12
                    Some('i') => result.push_str("%M"),  // minutes 00-59 (MySQL %i → chrono %M)
                    Some('s') | Some('S') => result.push_str("%S"), // seconds 00-59
                    Some('p') => result.push_str("%p"),  // AM/PM
                    Some('W') => result.push_str("%A"),  // full weekday name
                    Some('a') => result.push_str("%a"),  // abbreviated weekday name
                    Some('M') => result.push_str("%B"),  // full month name (MySQL %M → chrono %B)
                    Some('b') => result.push_str("%b"),  // abbreviated month name
                    Some('j') => result.push_str("%j"),  // day of year 001-366
                    Some('w') => result.push_str("%w"),  // day of week 0=Sunday
                    Some('T') => result.push_str("%H:%M:%S"), // time 24-hour
                    Some('r') => result.push_str("%I:%M:%S %p"), // time 12-hour
                    Some('%') => result.push('%'),        // literal %
                    Some(other) => {
                        // Unknown specifier — pass through as-is
                        result.push('%');
                        result.push(other);
                    }
                    None => result.push('%'), // trailing %
                }
            } else {
                result.push(ch);
            }
        }
        result
    }

    /// DATE_FORMAT(date, format) — format a timestamp using MySQL format specifiers.
    fn func_date_format(&self, args: &[Value]) -> Result<Value> {
        let [date_arg, fmt_arg] = args else {
            return Err(Error::query_execution("DATE_FORMAT requires exactly 2 arguments"));
        };
        if matches!(date_arg, Value::Null) || matches!(fmt_arg, Value::Null) {
            return Ok(Value::Null);
        }
        let ndt = Self::value_to_naive_datetime(date_arg)?;
        let format_str = match fmt_arg {
            Value::String(s) => s,
            _ => return Err(Error::query_execution("DATE_FORMAT second argument must be a format string")),
        };
        let chrono_fmt = Self::mysql_format_to_chrono(format_str);
        Ok(Value::String(ndt.format(&chrono_fmt).to_string()))
    }

    /// Translate a PostgreSQL `TO_CHAR` template string into a chrono format
    /// string. Covers the codes that show up in the wild: YYYY/YY, MM/MON/Mon,
    /// DD/DDD/DY/Day, HH24/HH12/HH, MI, SS, MS/US, AM/PM, am/pm, Q, IW/IYYY,
    /// W, D.
    ///
    /// Case-sensitive PG codes (MON vs Mon vs mon, DAY vs Day vs day) and
    /// codes with no chrono equivalent (Q, W) are emitted as `\u{1}TAG\u{2}`
    /// markers so the post-processing pass can substitute the correctly-
    /// cased value while leaving everything chrono produces alone.
    /// Unknown codes pass through verbatim — same fail-soft policy as
    /// `mysql_format_to_chrono`.
    fn pg_format_to_chrono(format: &str) -> String {
        const PATTERNS: &[(&str, &str)] = &[
            // 5+ char codes.
            ("MONTH", "\u{1}MONTH\u{2}"),
            ("Month", "\u{1}Month\u{2}"),
            ("month", "\u{1}month\u{2}"),
            ("YYYY",  "%Y"),
            ("HH24",  "%H"),
            ("HH12",  "%I"),
            ("IYYY",  "%G"),
            // 3-char codes.
            ("DDD",  "%j"),
            ("DAY",  "\u{1}DAY\u{2}"),
            ("Day",  "\u{1}Day\u{2}"),
            ("day",  "\u{1}day\u{2}"),
            ("MON",  "\u{1}MON\u{2}"),
            ("Mon",  "\u{1}Mon\u{2}"),
            ("mon",  "\u{1}mon\u{2}"),
            ("am" ,  "%P"),
            ("pm" ,  "%P"),
            ("AM" ,  "%p"),
            ("PM" ,  "%p"),
            // 2-char codes.
            ("YY", "%y"),
            ("MM", "%m"),
            ("DD", "%d"),
            ("DY", "%a"),
            ("HH", "%I"),
            ("MI", "%M"),
            ("SS", "%S"),
            ("MS", "%3f"),
            ("US", "%6f"),
            ("IW", "%V"),
            // 1-char codes (matched last so they don't shadow longer codes).
            ("Q",  "\u{1}Q\u{2}"),
            ("W",  "\u{1}W\u{2}"),
            ("D",  "%u"),
        ];
        let mut out = String::with_capacity(format.len());
        let bytes = format.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            let mut matched = false;
            for (pat, repl) in PATTERNS {
                let plen = pat.len();
                if i + plen <= bytes.len() && &bytes[i..i + plen] == pat.as_bytes() {
                    out.push_str(repl);
                    i += plen;
                    matched = true;
                    break;
                }
            }
            if !matched {
                #[allow(clippy::indexing_slicing)]
                out.push(bytes[i] as char);
                i += 1;
            }
        }
        out
    }

    /// Substitute the `\u{1}TAG\u{2}` markers left by `pg_format_to_chrono`
    /// with the correctly-cased / computed values for `ndt`.
    fn pg_format_post_substitute(input: String, ndt: chrono::NaiveDateTime) -> String {
        use chrono::Datelike;
        if !input.contains('\u{1}') {
            return input;
        }
        let month_full = ndt.format("%B").to_string();
        let month_abbr = ndt.format("%b").to_string();
        let day_full   = ndt.format("%A").to_string();
        let q = ((ndt.month() - 1) / 3 + 1).to_string();
        let w = ((ndt.day() - 1) / 7 + 1).to_string();
        let mut out = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\u{1}' {
                out.push(c);
                continue;
            }
            // Read tag until \u{2}.
            let mut tag = String::new();
            for next in chars.by_ref() {
                if next == '\u{2}' {
                    break;
                }
                tag.push(next);
            }
            match tag.as_str() {
                "MONTH" => out.push_str(&month_full.to_uppercase()),
                "Month" => out.push_str(&month_full),
                "month" => out.push_str(&month_full.to_lowercase()),
                "MON"   => out.push_str(&month_abbr.to_uppercase()),
                "Mon"   => out.push_str(&month_abbr),
                "mon"   => out.push_str(&month_abbr.to_lowercase()),
                "DAY"   => out.push_str(&day_full.to_uppercase()),
                "Day"   => out.push_str(&day_full),
                "day"   => out.push_str(&day_full.to_lowercase()),
                "Q"     => out.push_str(&q),
                "W"     => out.push_str(&w),
                other   => { out.push('\u{1}'); out.push_str(other); out.push('\u{2}'); }
            }
        }
        out
    }

    /// `TO_CHAR(value, format)` — Postgres-style date/number formatting.
    /// Date/timestamp values use `pg_format_to_chrono` + post-substitution
    /// to render case-significant tokens (Mon vs MON vs mon, etc.).
    /// Numeric `TO_CHAR(123.45, '9,999.00')` formatting is not implemented
    /// yet — token-dashboard and similar callers don't need it.
    fn func_to_char(&self, args: &[Value]) -> Result<Value> {
        let [val, fmt] = args else {
            return Err(Error::query_execution("TO_CHAR requires exactly 2 arguments"));
        };
        if matches!(val, Value::Null) || matches!(fmt, Value::Null) {
            return Ok(Value::Null);
        }
        let format_str = match fmt {
            Value::String(s) => s.clone(),
            other => return Err(Error::query_execution(format!(
                "TO_CHAR format must be a string, got {:?}", other
            ))),
        };
        match val {
            Value::Date(_) | Value::Timestamp(_) | Value::String(_) => {
                let ndt = Self::value_to_naive_datetime(val)?;
                let chrono_fmt = Self::pg_format_to_chrono(&format_str);
                let formatted = ndt.format(&chrono_fmt).to_string();
                Ok(Value::String(Self::pg_format_post_substitute(formatted, ndt)))
            }
            other => Err(Error::query_execution(format!(
                "TO_CHAR({:?}) is not yet supported (date/timestamp only)", other
            ))),
        }
    }

    /// `TO_DATE(text, format)` — parse text into a Date using a Postgres
    /// template translated to chrono.
    fn func_to_date(&self, args: &[Value]) -> Result<Value> {
        let [text, fmt] = args else {
            return Err(Error::query_execution("TO_DATE requires exactly 2 arguments"));
        };
        if matches!(text, Value::Null) || matches!(fmt, Value::Null) {
            return Ok(Value::Null);
        }
        let s = match text {
            Value::String(s) => s.clone(),
            other => return Err(Error::query_execution(format!(
                "TO_DATE input must be a string, got {:?}", other
            ))),
        };
        let f = match fmt {
            Value::String(s) => s.clone(),
            _ => unreachable!(),
        };
        let chrono_fmt = Self::pg_format_to_chrono(&f);
        let parsed = chrono::NaiveDate::parse_from_str(&s, &chrono_fmt)
            .map_err(|e| Error::query_execution(format!(
                "TO_DATE('{}','{}') failed: {}", s, f, e
            )))?;
        Ok(Value::Date(parsed))
    }

    /// `TO_TIMESTAMP(text, format)` — parse text into a Timestamp.
    /// Single-arg form `TO_TIMESTAMP(epoch_seconds)` converts a numeric
    /// unix epoch (matches Postgres / MySQL `from_unixtime`).
    fn func_to_timestamp(&self, args: &[Value]) -> Result<Value> {
        match args.len() {
            1 => {
                let arg = args.first()
                    .ok_or_else(|| Error::query_execution("TO_TIMESTAMP requires an argument"))?;
                if matches!(arg, Value::Null) {
                    return Ok(Value::Null);
                }
                let secs: f64 = match arg {
                    Value::Int4(n) => *n as f64,
                    Value::Int8(n) => *n as f64,
                    Value::Float4(f) => *f as f64,
                    Value::Float8(f) => *f,
                    other => return Err(Error::query_execution(format!(
                        "TO_TIMESTAMP({:?}) requires a numeric epoch", other
                    ))),
                };
                let s = secs as i64;
                let nanos = ((secs - s as f64) * 1e9) as u32;
                let ts = chrono::DateTime::from_timestamp(s, nanos)
                    .ok_or_else(|| Error::query_execution(format!(
                        "TO_TIMESTAMP: invalid epoch {}", secs
                    )))?;
                Ok(Value::Timestamp(ts))
            }
            2 => {
                let text = args.first().expect("len 2");
                let fmt = args.get(1).expect("len 2");
                if matches!(text, Value::Null) || matches!(fmt, Value::Null) {
                    return Ok(Value::Null);
                }
                let s = match text {
                    Value::String(s) => s.clone(),
                    other => return Err(Error::query_execution(format!(
                        "TO_TIMESTAMP input must be a string, got {:?}", other
                    ))),
                };
                let f = match fmt {
                    Value::String(s) => s.clone(),
                    _ => unreachable!(),
                };
                let chrono_fmt = Self::pg_format_to_chrono(&f);
                let parsed = chrono::NaiveDateTime::parse_from_str(&s, &chrono_fmt)
                    .map_err(|e| Error::query_execution(format!(
                        "TO_TIMESTAMP('{}','{}') failed: {}", s, f, e
                    )))?;
                Ok(Value::Timestamp(parsed.and_utc()))
            }
            _ => Err(Error::query_execution("TO_TIMESTAMP requires 1 or 2 arguments")),
        }
    }

    /// `DATE_TRUNC(field, value)` — round a timestamp/date down to the
    /// boundary of the specified field. PostgreSQL standard: returns a
    /// Timestamp (or Date if input was Date).
    fn func_date_trunc(&self, args: &[Value]) -> Result<Value> {
        use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
        let [field_arg, val_arg] = args else {
            return Err(Error::query_execution("DATE_TRUNC requires exactly 2 arguments"));
        };
        if matches!(val_arg, Value::Null) || matches!(field_arg, Value::Null) {
            return Ok(Value::Null);
        }
        let field = match field_arg {
            Value::String(s) => s.to_lowercase(),
            other => return Err(Error::query_execution(format!(
                "DATE_TRUNC field must be a string, got {:?}", other
            ))),
        };
        let was_date = matches!(val_arg, Value::Date(_));
        let ndt = Self::value_to_naive_datetime(val_arg)?;
        let truncated: NaiveDateTime = match field.as_str() {
            "microseconds" => ndt,
            "milliseconds" => {
                let ns = ndt.nanosecond();
                let trimmed = (ns / 1_000_000) * 1_000_000;
                ndt.with_nanosecond(trimmed).unwrap_or(ndt)
            }
            "second" | "seconds" => ndt.with_nanosecond(0).unwrap_or(ndt),
            "minute" | "minutes" => ndt
                .with_second(0).unwrap_or(ndt)
                .with_nanosecond(0).unwrap_or(ndt),
            "hour" | "hours" => ndt
                .with_minute(0).unwrap_or(ndt)
                .with_second(0).unwrap_or(ndt)
                .with_nanosecond(0).unwrap_or(ndt),
            "day" | "days" => ndt.date()
                .and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap_or_default()),
            "week" | "weeks" => {
                // ISO week: truncate to Monday.
                let date = ndt.date();
                let weekday = date.weekday().num_days_from_monday();
                let monday = date - chrono::Duration::days(weekday as i64);
                monday.and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap_or_default())
            }
            "month" | "months" => NaiveDate::from_ymd_opt(ndt.year(), ndt.month(), 1)
                .ok_or_else(|| Error::query_execution("DATE_TRUNC(month): invalid date"))?
                .and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap_or_default()),
            "quarter" | "quarters" => {
                let q_start_month = ((ndt.month() - 1) / 3) * 3 + 1;
                NaiveDate::from_ymd_opt(ndt.year(), q_start_month, 1)
                    .ok_or_else(|| Error::query_execution("DATE_TRUNC(quarter): invalid date"))?
                    .and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap_or_default())
            }
            "year" | "years" => NaiveDate::from_ymd_opt(ndt.year(), 1, 1)
                .ok_or_else(|| Error::query_execution("DATE_TRUNC(year): invalid date"))?
                .and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap_or_default()),
            "decade" | "decades" => NaiveDate::from_ymd_opt(ndt.year() / 10 * 10, 1, 1)
                .ok_or_else(|| Error::query_execution("DATE_TRUNC(decade): invalid date"))?
                .and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap_or_default()),
            "century" | "centuries" => {
                let century_year = ((ndt.year() - 1) / 100) * 100 + 1;
                NaiveDate::from_ymd_opt(century_year, 1, 1)
                    .ok_or_else(|| Error::query_execution("DATE_TRUNC(century): invalid date"))?
                    .and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap_or_default())
            }
            "millennium" | "millennia" => {
                let millennium_year = ((ndt.year() - 1) / 1000) * 1000 + 1;
                NaiveDate::from_ymd_opt(millennium_year, 1, 1)
                    .ok_or_else(|| Error::query_execution("DATE_TRUNC(millennium): invalid date"))?
                    .and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap_or_default())
            }
            other => return Err(Error::query_execution(format!(
                "DATE_TRUNC: unsupported field '{}'", other
            ))),
        };
        if was_date {
            Ok(Value::Date(truncated.date()))
        } else {
            Ok(Value::Timestamp(truncated.and_utc()))
        }
    }

    /// `MAKE_DATE(year, month, day)` — construct a Date from integer
    /// components. Returns an error for invalid combinations (matches
    /// PostgreSQL).
    fn func_make_date(&self, args: &[Value]) -> Result<Value> {
        let [y, m, d] = args else {
            return Err(Error::query_execution("MAKE_DATE requires exactly 3 arguments"));
        };
        let to_i32 = |v: &Value, name: &str| -> Result<i32> {
            match v {
                Value::Int2(n) => Ok(*n as i32),
                Value::Int4(n) => Ok(*n),
                Value::Int8(n) => Ok(*n as i32),
                Value::Null => Err(Error::query_execution(format!(
                    "MAKE_DATE: NULL not allowed for {}", name
                ))),
                other => Err(Error::query_execution(format!(
                    "MAKE_DATE: {} must be integer, got {:?}", name, other
                ))),
            }
        };
        let year = to_i32(y, "year")?;
        let month = to_i32(m, "month")? as u32;
        let day = to_i32(d, "day")? as u32;
        let date = chrono::NaiveDate::from_ymd_opt(year, month, day)
            .ok_or_else(|| Error::query_execution(format!(
                "MAKE_DATE: invalid date ({}, {}, {})", year, month, day
            )))?;
        Ok(Value::Date(date))
    }

    /// `MAKE_TIMESTAMP(year, month, day, hour, minute, sec)` — construct a
    /// timestamp from integer components plus a numeric seconds value
    /// (sec may be a float for sub-second precision). Returns Timestamp.
    fn func_make_timestamp(&self, args: &[Value]) -> Result<Value> {
        let [y, mo, d, h, mi, s] = args else {
            return Err(Error::query_execution(
                "MAKE_TIMESTAMP requires exactly 6 arguments (year, month, day, hour, min, sec)"
            ));
        };
        let to_i32 = |v: &Value, name: &str| -> Result<i32> {
            match v {
                Value::Int2(n) => Ok(*n as i32),
                Value::Int4(n) => Ok(*n),
                Value::Int8(n) => Ok(*n as i32),
                Value::Null => Err(Error::query_execution(format!(
                    "MAKE_TIMESTAMP: NULL not allowed for {}", name
                ))),
                other => Err(Error::query_execution(format!(
                    "MAKE_TIMESTAMP: {} must be integer, got {:?}", name, other
                ))),
            }
        };
        let year = to_i32(y, "year")?;
        let month = to_i32(mo, "month")? as u32;
        let day = to_i32(d, "day")? as u32;
        let hour = to_i32(h, "hour")? as u32;
        let minute = to_i32(mi, "minute")? as u32;
        let secs: f64 = match s {
            Value::Int2(n) => *n as f64,
            Value::Int4(n) => *n as f64,
            Value::Int8(n) => *n as f64,
            Value::Float4(f) => *f as f64,
            Value::Float8(f) => *f,
            Value::Null => return Err(Error::query_execution("MAKE_TIMESTAMP: NULL second")),
            other => return Err(Error::query_execution(format!(
                "MAKE_TIMESTAMP: sec must be numeric, got {:?}", other
            ))),
        };
        let whole = secs.trunc() as u32;
        let nanos = ((secs - secs.trunc()) * 1e9) as u32;
        let date = chrono::NaiveDate::from_ymd_opt(year, month, day)
            .ok_or_else(|| Error::query_execution(format!(
                "MAKE_TIMESTAMP: invalid date ({}, {}, {})", year, month, day
            )))?;
        let time = chrono::NaiveTime::from_hms_nano_opt(hour, minute, whole, nanos)
            .ok_or_else(|| Error::query_execution(format!(
                "MAKE_TIMESTAMP: invalid time ({}:{}:{})", hour, minute, secs
            )))?;
        Ok(Value::Timestamp(date.and_time(time).and_utc()))
    }

    /// `AGE(timestamp1, timestamp2)` — interval between two timestamps
    /// (`t1 - t2`). Single-argument form uses the current timestamp as
    /// `t1`. Result is a microsecond-precision `Interval`.
    fn func_age(&self, args: &[Value]) -> Result<Value> {
        let (a, b) = match args.len() {
            1 => {
                let arg = args.first().expect("len 1");
                if matches!(arg, Value::Null) {
                    return Ok(Value::Null);
                }
                (Value::Timestamp(chrono::Utc::now()), arg.clone())
            }
            2 => {
                let lhs = args.first().expect("len 2");
                let rhs = args.get(1).expect("len 2");
                if matches!(lhs, Value::Null) || matches!(rhs, Value::Null) {
                    return Ok(Value::Null);
                }
                (lhs.clone(), rhs.clone())
            }
            _ => return Err(Error::query_execution("AGE requires 1 or 2 arguments")),
        };
        let lhs_ndt = Self::value_to_naive_datetime(&a)?;
        let rhs_ndt = Self::value_to_naive_datetime(&b)?;
        let delta = lhs_ndt.and_utc().timestamp_micros() - rhs_ndt.and_utc().timestamp_micros();
        Ok(Value::Interval(delta))
    }

    /// DATE(timestamp) — extract the date part from a timestamp.
    fn func_date_extract(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("DATE() requires exactly 1 argument"));
        };
        if matches!(arg, Value::Null) {
            return Ok(Value::Null);
        }
        let d = Self::value_to_naive_date(arg)?;
        Ok(Value::Date(d))
    }

    /// `EXTRACT(<field> FROM <timestamp|date|interval>)` — the planner
    /// lowers this into a call to `__extract_<field>(expr)` so we can
    /// dispatch here with all the normal function-evaluation
    /// machinery (null propagation, type inference).
    ///
    /// Returns `Float8` for `Epoch` / sub-second fields (matching
    /// Postgres' `double precision` output) and `Int4` for calendar
    /// fields (year, month, day, hour, minute, dow, doy, week, quarter).
    fn extract_field(field: &str, args: &[Value]) -> Result<Value> {
        use chrono::{Datelike, Timelike};
        let [arg] = args else {
            return Err(Error::query_execution(format!(
                "EXTRACT({field}) requires exactly one argument"
            )));
        };
        if matches!(arg, Value::Null) {
            return Ok(Value::Null);
        }

        // EPOCH is the one field that works on any temporal type
        // (timestamp, date, interval) and returns Float8 (seconds).
        if field == "epoch" {
            let secs = match arg {
                Value::Timestamp(ts) => {
                    ts.timestamp() as f64 + ts.timestamp_subsec_nanos() as f64 / 1e9
                }
                Value::Date(d) => d
                    .and_hms_opt(0, 0, 0)
                    .map(|dt| dt.and_utc().timestamp() as f64)
                    .unwrap_or(0.0),
                Value::Time(t) => {
                    (t.num_seconds_from_midnight() as f64)
                        + (t.nanosecond() as f64) / 1e9
                }
                Value::Interval(us) => *us as f64 / 1e6,
                Value::Int8(n) => *n as f64, // treat as unix seconds
                Value::Int4(n) => *n as f64,
                Value::String(s) => Self::value_to_naive_datetime(&Value::String(s.clone()))?
                    .and_utc()
                    .timestamp() as f64,
                _ => return Err(Error::query_execution(format!(
                    "EXTRACT(EPOCH) does not accept {:?}",
                    arg
                ))),
            };
            return Ok(Value::Float8(secs));
        }

        // Interval extraction: components measured as ints.
        if let Value::Interval(us) = arg {
            let total_secs = (*us as i64) / 1_000_000;
            return Ok(Value::Int4(match field {
                "year" | "years" => (total_secs / (365 * 24 * 3600)) as i32,
                "month" | "months" => ((total_secs / (30 * 24 * 3600)) % 12) as i32,
                "day" | "days" => (total_secs / (24 * 3600)) as i32,
                "hour" | "hours" => ((total_secs / 3600) % 24) as i32,
                "minute" | "minutes" => ((total_secs / 60) % 60) as i32,
                "second" | "seconds" => (total_secs % 60) as i32,
                _ => return Err(Error::query_execution(format!(
                    "EXTRACT({field}) from interval not supported"
                ))),
            }));
        }

        // Date / timestamp / string: dispatch on field.
        let ndt = Self::value_to_naive_datetime(arg)?;
        let out = match field {
            "year" | "years" => ndt.year(),
            "month" | "months" => ndt.month() as i32,
            "day" | "days" => ndt.day() as i32,
            "hour" | "hours" => ndt.hour() as i32,
            "minute" | "minutes" => ndt.minute() as i32,
            "second" | "seconds" => ndt.second() as i32,
            "dow" => ndt.weekday().num_days_from_sunday() as i32,
            "isodow" => ndt.weekday().number_from_monday() as i32,
            "doy" => ndt.ordinal() as i32,
            "week" => ndt.iso_week().week() as i32,
            "quarter" => ((ndt.month() - 1) / 3 + 1) as i32,
            "decade" => ndt.year() / 10,
            "century" => (ndt.year() - 1) / 100 + 1,
            "millennium" => (ndt.year() - 1) / 1000 + 1,
            "millisecond" | "milliseconds" => {
                return Ok(Value::Float8(
                    ndt.second() as f64 + ndt.nanosecond() as f64 / 1e6,
                ))
            }
            "microsecond" | "microseconds" => {
                return Ok(Value::Float8(
                    ndt.second() as f64 * 1e6 + ndt.nanosecond() as f64 / 1e3,
                ))
            }
            _ => return Err(Error::query_execution(format!(
                "EXTRACT({field}) is not supported"
            ))),
        };
        Ok(Value::Int4(out))
    }

    /// YEAR(date) — extract the year from a date/timestamp. Returns Int4.
    fn func_year(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("YEAR() requires exactly 1 argument"));
        };
        if matches!(arg, Value::Null) {
            return Ok(Value::Null);
        }
        let d = Self::value_to_naive_date(arg)?;
        Ok(Value::Int4(d.year()))
    }

    /// MONTH(date) — extract the month (1-12) from a date/timestamp. Returns Int4.
    fn func_month(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("MONTH() requires exactly 1 argument"));
        };
        if matches!(arg, Value::Null) {
            return Ok(Value::Null);
        }
        let d = Self::value_to_naive_date(arg)?;
        Ok(Value::Int4(d.month() as i32))
    }

    /// DAY(date) / DAYOFMONTH(date) — extract the day (1-31). Returns Int4.
    fn func_day(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("DAY() requires exactly 1 argument"));
        };
        if matches!(arg, Value::Null) {
            return Ok(Value::Null);
        }
        let d = Self::value_to_naive_date(arg)?;
        Ok(Value::Int4(d.day() as i32))
    }

    /// Parse an interval from the second argument of DATE_ADD/DATE_SUB.
    ///
    /// Accepts either:
    ///  - `Value::Interval(micros)` (already parsed by planner)
    ///  - `Value::String("INTERVAL N UNIT")` or just `Value::String("N")`
    ///    paired with an optional third argument for the unit
    ///  - A numeric value (N) with a third argument for the unit string
    fn parse_date_add_interval(args: &[Value]) -> Result<(i64, String)> {
        let interval_arg = args.get(1).ok_or_else(|| Error::query_execution(
            "DATE_ADD/DATE_SUB requires at least 2 arguments"
        ))?;
        let unit_arg = args.get(2);

        // Case 1: Second arg is an Interval value (microseconds)
        if let Value::Interval(micros) = interval_arg {
            return Ok((*micros, String::new()));
        }

        // Case 2: Second arg is a string like "INTERVAL 1 DAY" or just the amount
        if let Value::String(s) = interval_arg {
            let trimmed = s.trim();
            // Try parsing "INTERVAL N UNIT" format
            let stripped = trimmed.strip_prefix("INTERVAL ").or_else(|| trimmed.strip_prefix("interval "));
            if let Some(rest) = stripped {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if let (Some(amt_str), Some(unit_str)) = (parts.first(), parts.get(1)) {
                    let amount: i64 = amt_str.parse()
                        .map_err(|_| Error::query_execution(format!("Invalid interval amount: {}", amt_str)))?;
                    return Ok((amount, unit_str.to_uppercase()));
                }
            }
            // Try parsing a plain number with a third argument for the unit
            if let Ok(amount) = trimmed.parse::<i64>() {
                let unit = match unit_arg {
                    Some(Value::String(u)) => u.to_uppercase(),
                    _ => "DAY".to_string(),
                };
                return Ok((amount, unit));
            }
        }

        // Case 3: Second arg is numeric (the interval amount), third is the unit
        let amount = match interval_arg {
            Value::Int2(n) => i64::from(*n),
            Value::Int4(n) => i64::from(*n),
            Value::Int8(n) => *n,
            Value::Float8(f) => *f as i64,
            _ => return Err(Error::query_execution(
                "DATE_ADD/DATE_SUB second argument must be an interval, number, or string"
            )),
        };
        let unit = match unit_arg {
            Some(Value::String(u)) => u.to_uppercase(),
            _ => "DAY".to_string(),
        };
        Ok((amount, unit))
    }

    /// Apply an interval (amount, unit) to a `NaiveDateTime`, returning a new datetime.
    /// `sign` is 1 for DATE_ADD and -1 for DATE_SUB.
    fn apply_interval(ndt: chrono::NaiveDateTime, amount: i64, unit: &str, sign: i64) -> Result<chrono::NaiveDateTime> {
        let signed = amount * sign;
        match unit {
            "SECOND" => Ok(ndt + chrono::Duration::seconds(signed)),
            "MINUTE" => Ok(ndt + chrono::Duration::minutes(signed)),
            "HOUR"   => Ok(ndt + chrono::Duration::hours(signed)),
            "DAY"    => Ok(ndt + chrono::Duration::days(signed)),
            "WEEK"   => Ok(ndt + chrono::Duration::weeks(signed)),
            "MONTH" => {
                // Month arithmetic: shift the month, clamping the day if needed
                let total_months = i64::from(ndt.year()) * 12 + i64::from(ndt.month0() as i32) + signed;
                let new_year = (total_months / 12) as i32;
                let new_month0 = total_months.rem_euclid(12) as u32;
                let new_month = new_month0 + 1;
                let max_day = Self::days_in_month(new_year, new_month);
                let new_day = ndt.day().min(max_day);
                chrono::NaiveDate::from_ymd_opt(new_year, new_month, new_day)
                    .and_then(|d| d.and_hms_opt(ndt.hour(), ndt.minute(), ndt.second()))
                    .ok_or_else(|| Error::query_execution("Date overflow in MONTH interval"))
            }
            "YEAR" => {
                let new_year = ndt.year() + signed as i32;
                let max_day = Self::days_in_month(new_year, ndt.month());
                let new_day = ndt.day().min(max_day);
                chrono::NaiveDate::from_ymd_opt(new_year, ndt.month(), new_day)
                    .and_then(|d| d.and_hms_opt(ndt.hour(), ndt.minute(), ndt.second()))
                    .ok_or_else(|| Error::query_execution("Date overflow in YEAR interval"))
            }
            "" => {
                // Interval was already in microseconds; apply directly
                Ok(ndt + chrono::Duration::microseconds(amount * sign))
            }
            _ => Err(Error::query_execution(format!("Unsupported interval unit: {}", unit))),
        }
    }

    /// Return the number of days in a given month.
    fn days_in_month(year: i32, month: u32) -> u32 {
        match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 => {
                if (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0) {
                    29
                } else {
                    28
                }
            }
            _ => 30,
        }
    }

    /// DATE_ADD(date, INTERVAL n unit) — add an interval to a date/timestamp.
    fn func_date_add(&self, args: &[Value]) -> Result<Value> {
        if args.len() < 2 || args.len() > 3 {
            return Err(Error::query_execution(
                "DATE_ADD requires 2 or 3 arguments: DATE_ADD(date, interval [, unit])"
            ));
        }
        let date_arg = args.first().ok_or_else(|| Error::query_execution("DATE_ADD requires a date argument"))?;
        let interval_arg = args.get(1).ok_or_else(|| Error::query_execution("DATE_ADD requires an interval argument"))?;
        if matches!(date_arg, Value::Null) || matches!(interval_arg, Value::Null) {
            return Ok(Value::Null);
        }
        let ndt = Self::value_to_naive_datetime(date_arg)?;
        let (amount, unit) = Self::parse_date_add_interval(args)?;
        let result = Self::apply_interval(ndt, amount, &unit, 1)?;
        Ok(Value::Timestamp(chrono::DateTime::from_naive_utc_and_offset(result, Utc)))
    }

    /// DATE_SUB(date, INTERVAL n unit) — subtract an interval from a date/timestamp.
    fn func_date_sub(&self, args: &[Value]) -> Result<Value> {
        if args.len() < 2 || args.len() > 3 {
            return Err(Error::query_execution(
                "DATE_SUB requires 2 or 3 arguments: DATE_SUB(date, interval [, unit])"
            ));
        }
        let date_arg = args.first().ok_or_else(|| Error::query_execution("DATE_SUB requires a date argument"))?;
        let interval_arg = args.get(1).ok_or_else(|| Error::query_execution("DATE_SUB requires an interval argument"))?;
        if matches!(date_arg, Value::Null) || matches!(interval_arg, Value::Null) {
            return Ok(Value::Null);
        }
        let ndt = Self::value_to_naive_datetime(date_arg)?;
        let (amount, unit) = Self::parse_date_add_interval(args)?;
        let result = Self::apply_interval(ndt, amount, &unit, -1)?;
        Ok(Value::Timestamp(chrono::DateTime::from_naive_utc_and_offset(result, Utc)))
    }

    /// DATEDIFF(date1, date2) — returns integer number of days between two dates.
    fn func_datediff(&self, args: &[Value]) -> Result<Value> {
        let [a, b] = args else {
            return Err(Error::query_execution("DATEDIFF requires exactly 2 arguments"));
        };
        if matches!(a, Value::Null) || matches!(b, Value::Null) {
            return Ok(Value::Null);
        }
        let d1 = Self::value_to_naive_date(a)?;
        let d2 = Self::value_to_naive_date(b)?;
        let diff = (d1 - d2).num_days();
        #[allow(clippy::cast_possible_truncation)]
        Ok(Value::Int4(diff as i32))
    }

    /// TIMESTAMPDIFF(unit, start, end) — returns integer difference in specified unit.
    fn func_timestampdiff(&self, args: &[Value]) -> Result<Value> {
        let [unit_arg, start_arg, end_arg] = args else {
            return Err(Error::query_execution(
                "TIMESTAMPDIFF requires exactly 3 arguments: TIMESTAMPDIFF(unit, start, end)"
            ));
        };
        let unit = match unit_arg {
            Value::String(s) => s.to_uppercase(),
            Value::Null => return Ok(Value::Null),
            _ => return Err(Error::query_execution(
                "TIMESTAMPDIFF first argument must be a unit string (SECOND, MINUTE, HOUR, DAY, MONTH, YEAR)"
            )),
        };
        if matches!(start_arg, Value::Null) || matches!(end_arg, Value::Null) {
            return Ok(Value::Null);
        }
        let start = Self::value_to_naive_datetime(start_arg)?;
        let end = Self::value_to_naive_datetime(end_arg)?;
        let diff = match unit.as_str() {
            "SECOND" => (end - start).num_seconds(),
            "MINUTE" => (end - start).num_minutes(),
            "HOUR"   => (end - start).num_hours(),
            "DAY"    => (end - start).num_days(),
            "WEEK"   => (end - start).num_weeks(),
            "MONTH" => {
                let months_end = i64::from(end.year()) * 12 + i64::from(end.month0() as i32);
                let months_start = i64::from(start.year()) * 12 + i64::from(start.month0() as i32);
                months_end - months_start
            }
            "YEAR" => i64::from(end.year() - start.year()),
            _ => return Err(Error::query_execution(format!(
                "TIMESTAMPDIFF unsupported unit: {}. Use SECOND, MINUTE, HOUR, DAY, MONTH, or YEAR", unit
            ))),
        };
        #[allow(clippy::cast_possible_truncation)]
        Ok(Value::Int8(diff))
    }

    /// UNIX_TIMESTAMP() — returns seconds since epoch (no args = now).
    /// UNIX_TIMESTAMP(date) — converts a date/timestamp to seconds since epoch.
    fn func_unix_timestamp(&self, args: &[Value]) -> Result<Value> {
        if args.is_empty() {
            return Ok(Value::Int8(Utc::now().timestamp()));
        }
        let [arg] = args else {
            return Err(Error::query_execution("UNIX_TIMESTAMP requires 0 or 1 arguments"));
        };
        if matches!(arg, Value::Null) {
            return Ok(Value::Null);
        }
        let ndt = Self::value_to_naive_datetime(arg)?;
        let ts: chrono::DateTime<Utc> = chrono::DateTime::from_naive_utc_and_offset(ndt, Utc);
        Ok(Value::Int8(ts.timestamp()))
    }

    /// FROM_UNIXTIME(timestamp) — converts Unix timestamp (seconds) to datetime string.
    fn func_from_unixtime(&self, args: &[Value]) -> Result<Value> {
        if args.is_empty() || args.len() > 2 {
            return Err(Error::query_execution(
                "FROM_UNIXTIME requires 1 or 2 arguments: FROM_UNIXTIME(unix_ts [, format])"
            ));
        }
        let ts_arg = args.first().ok_or_else(|| Error::query_execution("FROM_UNIXTIME requires an argument"))?;
        if matches!(ts_arg, Value::Null) {
            return Ok(Value::Null);
        }
        let epoch = match ts_arg {
            Value::Int2(n) => i64::from(*n),
            Value::Int4(n) => i64::from(*n),
            Value::Int8(n) => *n,
            Value::Float8(f) => *f as i64,
            Value::String(s) => s.parse::<i64>()
                .map_err(|_| Error::query_execution(format!("Invalid unix timestamp: {}", s)))?,
            _ => return Err(Error::query_execution("FROM_UNIXTIME argument must be numeric")),
        };
        let dt = chrono::DateTime::from_timestamp(epoch, 0)
            .ok_or_else(|| Error::query_execution(format!("Invalid unix timestamp: {}", epoch)))?;
        if let Some(fmt_arg) = args.get(1) {
            // Optional format string (MySQL syntax)
            if matches!(fmt_arg, Value::Null) {
                return Ok(Value::Null);
            }
            let format_str = match fmt_arg {
                Value::String(s) => s,
                _ => return Err(Error::query_execution("FROM_UNIXTIME format must be a string")),
            };
            let chrono_fmt = Self::mysql_format_to_chrono(format_str);
            Ok(Value::String(dt.naive_utc().format(&chrono_fmt).to_string()))
        } else {
            Ok(Value::Timestamp(dt))
        }
    }

    // ---- MySQL string functions ----

    /// LOCATE(substr, str [, pos]) — find position of substring (1-indexed, 0 if not found).
    fn func_locate(&self, args: &[Value]) -> Result<Value> {
        if args.len() < 2 || args.len() > 3 {
            return Err(Error::query_execution(
                "LOCATE requires 2 or 3 arguments: LOCATE(substr, str [, pos])"
            ));
        }
        let needle_arg = args.first().ok_or_else(|| Error::query_execution("LOCATE requires arguments"))?;
        let haystack_arg = args.get(1).ok_or_else(|| Error::query_execution("LOCATE requires 2 arguments"))?;
        if matches!(needle_arg, Value::Null) || matches!(haystack_arg, Value::Null) {
            return Ok(Value::Null);
        }
        let needle = match needle_arg {
            Value::String(s) => s.as_str(),
            _ => return Err(Error::query_execution("LOCATE first argument must be a string")),
        };
        let haystack = match haystack_arg {
            Value::String(s) => s.as_str(),
            _ => return Err(Error::query_execution("LOCATE second argument must be a string")),
        };
        // Optional starting position (1-indexed)
        let start_pos = if let Some(pos_arg) = args.get(2) {
            match pos_arg {
                Value::Null => return Ok(Value::Null),
                Value::Int2(n) => (*n as usize).saturating_sub(1),
                Value::Int4(n) => (*n as usize).saturating_sub(1),
                Value::Int8(n) => (*n as usize).saturating_sub(1),
                _ => return Err(Error::query_execution("LOCATE third argument must be an integer")),
            }
        } else {
            0
        };
        if start_pos >= haystack.len() {
            return Ok(Value::Int4(0));
        }
        // Use .get() to safely slice the haystack
        let search_region = haystack.get(start_pos..).unwrap_or("");
        match search_region.find(needle) {
            Some(pos) => Ok(Value::Int4((pos + start_pos + 1) as i32)), // 1-indexed
            None => Ok(Value::Int4(0)),
        }
    }

    /// INSTR(str, substr) — same as LOCATE but with reversed argument order.
    fn func_instr(&self, args: &[Value]) -> Result<Value> {
        let [str_arg, substr_arg] = args else {
            return Err(Error::query_execution("INSTR requires exactly 2 arguments"));
        };
        // INSTR(str, substr) is LOCATE(substr, str), so swap args
        let swapped = [substr_arg.clone(), str_arg.clone()];
        self.func_locate(&swapped)
    }

    /// jsonb_extract_path(json, path_elements...)
    /// Extract JSON sub-object at the specified path
    fn jsonb_extract_path(&self, args: &[Value]) -> Result<Value> {
        let (first, rest) = args.split_first().ok_or_else(|| Error::query_execution(
            "jsonb_extract_path requires at least one argument"
        ))?;

        let json_str = match first {
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
        for path_elem in rest {
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
        let [arg] = args else {
            return Err(Error::query_execution(
                "jsonb_array_elements requires exactly one argument"
            ));
        };

        let json_str = match arg {
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
        let [arg] = args else {
            return Err(Error::query_execution(
                "jsonb_object_keys requires exactly one argument"
            ));
        };

        let json_str = match arg {
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
        let [arg] = args else {
            return Err(Error::query_execution(
                "jsonb_array_length requires exactly one argument"
            ));
        };

        let json_str = match arg {
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
        let [arg] = args else {
            return Err(Error::query_execution(
                "jsonb_typeof requires exactly one argument"
            ));
        };

        let json_str = match arg {
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
        let [first, second] = args else {
            return Err(Error::query_execution(
                "jsonb_path_query requires exactly two arguments"
            ));
        };

        let json_str = match first {
            Value::Json(j) => j,
            Value::Null => return Ok(Value::Null),
            _ => return Err(Error::query_execution(
                "First argument must be JSON"
            )),
        };

        let path = match second {
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

    /// jsonb_path_query_array(target, path)
    /// Query JSON using path and return results as an array
    fn jsonb_path_query_array(&self, args: &[Value]) -> Result<Value> {
        let result = self.jsonb_path_query(args)?;
        match result {
            Value::Array(_) => Ok(result),
            Value::Null => Ok(Value::Null),
            other => Ok(Value::Array(vec![other])),
        }
    }

    /// jsonb_path_query_first(target, path)
    /// Query JSON using path and return first result
    fn jsonb_path_query_first(&self, args: &[Value]) -> Result<Value> {
        let result = self.jsonb_path_query(args)?;
        match result {
            Value::Array(arr) => Ok(arr.into_iter().next().unwrap_or(Value::Null)),
            other => Ok(other),
        }
    }

    /// jsonb_path_exists(target, path)
    /// Check if path exists in JSON
    fn jsonb_path_exists(&self, args: &[Value]) -> Result<Value> {
        let result = self.jsonb_path_query(args)?;
        let exists = !matches!(result, Value::Null);
        Ok(Value::Boolean(exists))
    }

    /// jsonb_path_match(target, path)
    /// Check if path returns true
    fn jsonb_path_match(&self, args: &[Value]) -> Result<Value> {
        let result = self.jsonb_path_query(args)?;
        match result {
            Value::Boolean(b) => Ok(Value::Boolean(b)),
            Value::Json(s) => {
                if s == "true" {
                    Ok(Value::Boolean(true))
                } else if s == "false" {
                    Ok(Value::Boolean(false))
                } else {
                    Ok(Value::Null)
                }
            }
            _ => Ok(Value::Null),
        }
    }

    /// jsonb_pretty(json)
    /// Pretty print JSON
    fn jsonb_pretty(&self, args: &[Value]) -> Result<Value> {
        let first = args.first().ok_or_else(|| Error::query_execution("jsonb_pretty requires an argument"))?;

        let json_str = match first {
            Value::Json(j) => j,
            Value::Null => return Ok(Value::Null),
            _ => return Err(Error::query_execution("Argument must be JSON")),
        };

        let json: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| Error::query_execution(format!("Invalid JSON: {}", e)))?;

        let pretty = serde_json::to_string_pretty(&json)
            .map_err(|e| Error::query_execution(format!("Failed to format JSON: {}", e)))?;

        Ok(Value::String(pretty))
    }

    /// jsonb_strip_nulls(json)
    /// Remove null values from JSON
    fn jsonb_strip_nulls(&self, args: &[Value]) -> Result<Value> {
        let first = args.first().ok_or_else(|| Error::query_execution("jsonb_strip_nulls requires an argument"))?;

        let json_str = match first {
            Value::Json(j) => j,
            Value::Null => return Ok(Value::Null),
            _ => return Err(Error::query_execution("Argument must be JSON")),
        };

        let json: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| Error::query_execution(format!("Invalid JSON: {}", e)))?;

        fn strip_nulls(val: serde_json::Value) -> serde_json::Value {
            match val {
                serde_json::Value::Object(map) => {
                    let new_map: serde_json::Map<String, serde_json::Value> = map
                        .into_iter()
                        .filter(|(_, v)| !v.is_null())
                        .map(|(k, v)| (k, strip_nulls(v)))
                        .collect();
                    serde_json::Value::Object(new_map)
                }
                serde_json::Value::Array(arr) => {
                    serde_json::Value::Array(arr.into_iter().map(strip_nulls).collect())
                }
                other => other,
            }
        }

        let stripped = strip_nulls(json);
        Ok(Value::Json(stripped.to_string()))
    }

    /// jsonb_build_object(key1, val1, key2, val2, ...)
    /// Constructs a JSONB object from alternating key-value pairs
    fn jsonb_build_object(&self, args: &[Value]) -> Result<Value> {
        if args.len() % 2 != 0 {
            return Err(Error::query_execution(
                "jsonb_build_object requires an even number of arguments (key-value pairs)"
            ));
        }

        let mut obj = serde_json::Map::new();

        for pair in args.chunks(2) {
            let key_val = pair.first().ok_or_else(|| Error::query_execution("Missing key in jsonb_build_object"))?;
            let value = pair.get(1).ok_or_else(|| Error::query_execution("Missing value in jsonb_build_object"))?;

            // Convert key to string
            let key = match key_val {
                Value::String(s) => s.clone(),
                Value::Null => continue, // Skip null keys
                other => other.to_string().trim_matches('\'').to_string(),
            };

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
                    let hex: String = b.iter().fold(String::new(), |mut acc, byte| {
                        use std::fmt::Write;
                        let _ = write!(acc, "{:02x}", byte);
                        acc
                    });
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
                Value::Interval(iv) => serde_json::json!(format!("{} microseconds", iv)),
            };

            obj.insert(key, json_val);
        }

        Ok(Value::Json(serde_json::Value::Object(obj).to_string()))
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
                    let hex: String = b.iter().fold(String::new(), |mut acc, byte| {
                        use std::fmt::Write;
                        let _ = write!(acc, "{:02x}", byte);
                        acc
                    });
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
                Value::Interval(iv) => serde_json::json!(format!("{} microseconds", iv)),
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

        let arg0 = args.get(0).ok_or_else(|| Error::query_execution("jsonb_set: missing target"))?;
        let arg1 = args.get(1).ok_or_else(|| Error::query_execution("jsonb_set: missing path"))?;
        let arg2 = args.get(2).ok_or_else(|| Error::query_execution("jsonb_set: missing new_value"))?;

        let json_str = match arg0 {
            Value::Json(j) => j.clone(),
            Value::Null => return Ok(Value::Null),
            _ => return Err(Error::query_execution("First argument must be JSON")),
        };

        let path_arr = match arg1 {
            Value::Array(arr) => arr,
            _ => return Err(Error::query_execution("Second argument must be an array (path)")),
        };

        let create_missing = if let Some(arg3) = args.get(3) {
            match arg3 {
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
        let new_val = match arg2 {
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
            other => serde_json::json!(other.to_string()),
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
        jsonb_set_recursive_impl(current, path, index, value, create_missing)
    }

    /// jsonb_concat(jsonb1, jsonb2)
    /// Merges two JSONB objects
    fn jsonb_concat(&self, args: &[Value]) -> Result<Value> {
        let [first, second] = args else {
            return Err(Error::query_execution("jsonb_concat requires exactly 2 arguments"));
        };

        let json1_str = match first {
            Value::Json(j) => j.clone(),
            Value::Null => return Ok(Value::Null),
            _ => return Err(Error::query_execution("First argument must be JSON")),
        };

        let json2_str = match second {
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
        let [first, second] = args else {
            return Err(Error::query_execution("jsonb_delete requires exactly 2 arguments"));
        };

        let json_str = match first {
            Value::Json(j) => j.clone(),
            Value::Null => return Ok(Value::Null),
            _ => return Err(Error::query_execution("First argument must be JSON")),
        };

        let path_arr = match second {
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
        jsonb_delete_recursive_impl(current, path, index)
    }

    /// jsonb_each(jsonb_object)
    /// Returns object key-value pairs (returns array of keys for MVP)
    fn jsonb_each(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("jsonb_each requires exactly 1 argument"));
        };

        let json_str = match arg {
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
        let [arg] = args else {
            return Err(Error::query_execution("jsonb_each_text requires exactly 1 argument"));
        };

        let json_str = match arg {
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
            // Comparison operators with type coercion
            // Use compare_values for Eq/NotEq to handle cross-type comparisons (e.g., 1 = 1.0)
            BinaryOperator::Eq => self.compare_values(left, right, |cmp| cmp.is_eq()),
            BinaryOperator::NotEq => self.compare_values(left, right, |cmp| cmp.is_ne()),
            BinaryOperator::Lt => self.compare_values(left, right, |cmp| cmp.is_lt()),
            BinaryOperator::LtEq => self.compare_values(left, right, |cmp| cmp.is_le()),
            BinaryOperator::Gt => self.compare_values(left, right, |cmp| cmp.is_gt()),
            BinaryOperator::GtEq => self.compare_values(left, right, |cmp| cmp.is_ge()),

            // Logical operators - SQL three-valued logic for NULL
            BinaryOperator::And => Self::three_valued_and(left, right),
            BinaryOperator::Or => Self::three_valued_or(left, right),

            // Arithmetic operators
            BinaryOperator::Plus => self.arithmetic_add(left, right),
            BinaryOperator::Minus => self.arithmetic_subtract(left, right),
            BinaryOperator::Multiply => self.arithmetic_multiply(left, right),
            BinaryOperator::Divide => {
                // SQL standard: NULL / x or x / NULL = NULL
                if matches!(left, Value::Null) || matches!(right, Value::Null) {
                    return Ok(Value::Null);
                }
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

            // String concatenation: ||
            BinaryOperator::StringConcat => self.string_concat_op(left, right),

            // Full-text search match: tsvector @@ tsquery
            BinaryOperator::TsMatch => Self::evaluate_ts_match(left, right),

            // String pattern matching (LIKE)
            BinaryOperator::Like => self.like_op(left, right, false),
            BinaryOperator::NotLike => self.like_op(left, right, true),

            // Case-insensitive LIKE (ILIKE)
            BinaryOperator::ILike => self.ilike_op(left, right, false),
            BinaryOperator::NotILike => self.ilike_op(left, right, true),

            // Regular expression matching
            BinaryOperator::RegexMatch => self.regex_op(left, right, false, false),
            BinaryOperator::RegexIMatch => self.regex_op(left, right, false, true),
            BinaryOperator::NotRegexMatch => self.regex_op(left, right, true, false),
            BinaryOperator::NotRegexIMatch => self.regex_op(left, right, true, true),

            // SQL SIMILAR TO (uses SQL regex syntax)
            BinaryOperator::SimilarTo => self.similar_to_op(left, right, false),
            BinaryOperator::NotSimilarTo => self.similar_to_op(left, right, true),

            // Modulo operator
            BinaryOperator::Modulo => self.arithmetic_modulo(left, right),
        }
    }

    /// Evaluate a unary operation
    fn evaluate_unary_op(&self, op: &super::UnaryOperator, value: &Value) -> Result<Value> {
        use super::UnaryOperator;

        match op {
            UnaryOperator::Not => {
                // SQL standard: NOT NULL = NULL (three-valued logic)
                if matches!(value, Value::Null) {
                    return Ok(Value::Null);
                }
                let bool_val = self.to_boolean(value)?;
                Ok(Value::Boolean(!bool_val))
            }
            UnaryOperator::Minus => match value {
                Value::Int2(i) => i.checked_neg()
                    .map(Value::Int2)
                    .ok_or_else(|| Error::query_execution("integer overflow: SMALLINT negation")),
                Value::Int4(i) => i.checked_neg()
                    .map(Value::Int4)
                    .ok_or_else(|| Error::query_execution("integer overflow: INT negation")),
                Value::Int8(i) => i.checked_neg()
                    .map(Value::Int8)
                    .ok_or_else(|| Error::query_execution("integer overflow: BIGINT negation")),
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

    /// Lexicographic row-constructor comparison:
    /// `(a1, a2, …) <op> (b1, b2, …)` compares element-by-element with
    /// three-valued logic. Supports Eq/NotEq/Lt/LtEq/Gt/GtEq.
    fn evaluate_tuple_compare(
        &self,
        left: &[LogicalExpr],
        op: &super::BinaryOperator,
        right: &[LogicalExpr],
        tuple: &Tuple,
    ) -> Result<Value> {
        use super::BinaryOperator as Op;
        if left.len() != right.len() {
            return Err(Error::query_execution(format!(
                "Row constructor size mismatch: {} vs {}",
                left.len(),
                right.len()
            )));
        }

        // Evaluate pairs from left to right; for Eq/NotEq we can
        // short-circuit on the first inequality, for ordering ops we
        // walk until we find the first non-equal pair.
        let mut saw_null = false;
        for (l_expr, r_expr) in left.iter().zip(right.iter()) {
            let l_val = self.evaluate(l_expr, tuple)?;
            let r_val = self.evaluate(r_expr, tuple)?;

            if matches!(l_val, Value::Null) || matches!(r_val, Value::Null) {
                // NULL makes the pair comparison unknown — propagate.
                saw_null = true;
                continue;
            }

            let eq = self.compare_values(&l_val, &r_val, |c| c.is_eq())?;
            let is_eq = matches!(eq, Value::Boolean(true));
            if is_eq {
                continue;
            }

            // First unequal pair decides the comparison.
            return match op {
                Op::Eq => Ok(Value::Boolean(false)),
                Op::NotEq => Ok(Value::Boolean(true)),
                Op::Lt => self.compare_values(&l_val, &r_val, |c| c.is_lt()),
                Op::LtEq => self.compare_values(&l_val, &r_val, |c| c.is_lt()),
                Op::Gt => self.compare_values(&l_val, &r_val, |c| c.is_gt()),
                Op::GtEq => self.compare_values(&l_val, &r_val, |c| c.is_gt()),
                _ => Err(Error::query_execution(format!(
                    "Operator {:?} not supported on row constructors",
                    op
                ))),
            };
        }

        // All pairs equal (or NULL).
        if saw_null {
            return Ok(Value::Null);
        }
        match op {
            Op::Eq | Op::LtEq | Op::GtEq => Ok(Value::Boolean(true)),
            Op::NotEq | Op::Lt | Op::Gt => Ok(Value::Boolean(false)),
            _ => Err(Error::query_execution(format!(
                "Operator {:?} not supported on row constructors",
                op
            ))),
        }
    }

    /// Parse an ISO-8601 / PG-timestamp string into a UTC DateTime.
    /// Accepts the same formats the TIMESTAMP cast path accepts —
    /// RFC 3339 (`2026-04-23T00:00:00Z`, `…+00:00`), space-separated
    /// (`2026-04-23 00:00:00[.ffffff]`), and date-only
    /// (`2026-04-23`, treated as midnight UTC).
    fn parse_timestamp_string(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
        use chrono::Utc;
        if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(s) {
            return Some(ts.with_timezone(&Utc));
        }
        if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f") {
            return Some(chrono::DateTime::from_naive_utc_and_offset(ndt, Utc));
        }
        if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
            return Some(chrono::DateTime::from_naive_utc_and_offset(ndt, Utc));
        }
        if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
            if let Some(ndt) = d.and_hms_opt(0, 0, 0) {
                return Some(chrono::DateTime::from_naive_utc_and_offset(ndt, Utc));
            }
        }
        None
    }

    /// Parse a date string into a NaiveDate. Accepts `YYYY-MM-DD` and
    /// the leading date portion of an ISO 8601 timestamp.
    fn parse_date_string(s: &str) -> Option<chrono::NaiveDate> {
        if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
            return Some(d);
        }
        // Fall back to parsing a timestamp and extracting the date.
        Self::parse_timestamp_string(s).map(|ts| ts.date_naive())
    }

    /// Compare two values using a comparison function
    fn compare_values<F>(&self, left: &Value, right: &Value, cmp: F) -> Result<Value>
    where
        F: FnOnce(std::cmp::Ordering) -> bool,
    {
        use std::cmp::Ordering;

        // SQL standard: any comparison with NULL yields NULL (three-valued logic)
        if matches!(left, Value::Null) || matches!(right, Value::Null) {
            return Ok(Value::Null);
        }

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
                    _ => return Err(Error::query_execution(format!(
                        "Cannot compare invalid NUMERIC values '{}' and '{}'", a, b
                    ))),
                }
            }

            // Numeric to Int comparisons
            (Value::Numeric(a), Value::Int2(b)) => {
                match a.parse::<Decimal>() {
                    Ok(a_dec) => a_dec.cmp(&Decimal::from(*b)),
                    Err(_) => return Err(Error::query_execution(format!(
                        "Invalid NUMERIC value '{}' in comparison", a
                    ))),
                }
            }
            (Value::Int2(a), Value::Numeric(b)) => {
                match b.parse::<Decimal>() {
                    Ok(b_dec) => Decimal::from(*a).cmp(&b_dec),
                    Err(_) => return Err(Error::query_execution(format!(
                        "Invalid NUMERIC value '{}' in comparison", b
                    ))),
                }
            }
            (Value::Numeric(a), Value::Int4(b)) => {
                match a.parse::<Decimal>() {
                    Ok(a_dec) => a_dec.cmp(&Decimal::from(*b)),
                    Err(_) => return Err(Error::query_execution(format!(
                        "Invalid NUMERIC value '{}' in comparison", a
                    ))),
                }
            }
            (Value::Int4(a), Value::Numeric(b)) => {
                match b.parse::<Decimal>() {
                    Ok(b_dec) => Decimal::from(*a).cmp(&b_dec),
                    Err(_) => return Err(Error::query_execution(format!(
                        "Invalid NUMERIC value '{}' in comparison", b
                    ))),
                }
            }
            (Value::Numeric(a), Value::Int8(b)) => {
                match a.parse::<Decimal>() {
                    Ok(a_dec) => a_dec.cmp(&Decimal::from(*b)),
                    Err(_) => return Err(Error::query_execution(format!(
                        "Invalid NUMERIC value '{}' in comparison", a
                    ))),
                }
            }
            (Value::Int8(a), Value::Numeric(b)) => {
                match b.parse::<Decimal>() {
                    Ok(b_dec) => Decimal::from(*a).cmp(&b_dec),
                    Err(_) => return Err(Error::query_execution(format!(
                        "Invalid NUMERIC value '{}' in comparison", b
                    ))),
                }
            }

            // Numeric to Float comparisons (convert to f64 for comparison)
            (Value::Numeric(a), Value::Float4(b)) => {
                match a.parse::<f64>() {
                    Ok(a_f) => a_f.partial_cmp(&(*b as f64)).unwrap_or(Ordering::Equal),
                    Err(_) => return Err(Error::query_execution(format!(
                        "Invalid NUMERIC value '{}' in comparison", a
                    ))),
                }
            }
            (Value::Float4(a), Value::Numeric(b)) => {
                match b.parse::<f64>() {
                    Ok(b_f) => (*a as f64).partial_cmp(&b_f).unwrap_or(Ordering::Equal),
                    Err(_) => return Err(Error::query_execution(format!(
                        "Invalid NUMERIC value '{}' in comparison", b
                    ))),
                }
            }
            (Value::Numeric(a), Value::Float8(b)) => {
                match a.parse::<f64>() {
                    Ok(a_f) => a_f.partial_cmp(b).unwrap_or(Ordering::Equal),
                    Err(_) => return Err(Error::query_execution(format!(
                        "Invalid NUMERIC value '{}' in comparison", a
                    ))),
                }
            }
            (Value::Float8(a), Value::Numeric(b)) => {
                match b.parse::<f64>() {
                    Ok(b_f) => a.partial_cmp(&b_f).unwrap_or(Ordering::Equal),
                    Err(_) => return Err(Error::query_execution(format!(
                        "Invalid NUMERIC value '{}' in comparison", b
                    ))),
                }
            }

            // UUID comparisons
            (Value::Uuid(a), Value::Uuid(b)) => a.cmp(b),
            // String-to-UUID coercion (WHERE uuid_col = 'string-literal')
            (Value::Uuid(a), Value::String(b)) => {
                match uuid::Uuid::parse_str(b) {
                    Ok(b_uuid) => a.cmp(&b_uuid),
                    Err(_) => return Err(Error::query_execution(format!(
                        "Cannot compare UUID with invalid UUID string '{}'", b
                    ))),
                }
            }
            (Value::String(a), Value::Uuid(b)) => {
                match uuid::Uuid::parse_str(a) {
                    Ok(a_uuid) => a_uuid.cmp(b),
                    Err(_) => return Err(Error::query_execution(format!(
                        "Cannot compare invalid UUID string '{}' with UUID", a
                    ))),
                }
            }

            // Boolean comparisons (false < true)
            (Value::Boolean(a), Value::Boolean(b)) => a.cmp(b),

            // Timestamp comparisons
            (Value::Timestamp(a), Value::Timestamp(b)) => a.cmp(b),
            (Value::Date(a), Value::Date(b)) => a.cmp(b),
            // Timestamp to Date comparisons (compare dates only)
            (Value::Timestamp(a), Value::Date(b)) => a.date_naive().cmp(b),
            (Value::Date(a), Value::Timestamp(b)) => a.cmp(&b.date_naive()),

            // Timestamp ↔ String: implicit coercion mirrors stock
            // PostgreSQL, which accepts `timestamp = 'YYYY-MM-DD…'`
            // by casting the literal to TIMESTAMP. Drizzle binds
            // JavaScript `Date`s as ISO 8601 strings into gte/lte
            // helpers over OID 1114 columns — without this the
            // analytics endpoints fail with "Cannot compare" (B32).
            (Value::Timestamp(a), Value::String(b)) => {
                match Self::parse_timestamp_string(b) {
                    Some(b_ts) => a.cmp(&b_ts),
                    None => a.to_rfc3339().as_str().cmp(b.as_str()),
                }
            }
            (Value::String(a), Value::Timestamp(b)) => {
                match Self::parse_timestamp_string(a) {
                    Some(a_ts) => a_ts.cmp(b),
                    None => a.as_str().cmp(b.to_rfc3339().as_str()),
                }
            }
            // Date ↔ String: same rationale for `date` columns.
            (Value::Date(a), Value::String(b)) => {
                match Self::parse_date_string(b) {
                    Some(b_d) => a.cmp(&b_d),
                    None => a.to_string().as_str().cmp(b.as_str()),
                }
            }
            (Value::String(a), Value::Date(b)) => {
                match Self::parse_date_string(a) {
                    Some(a_d) => a_d.cmp(b),
                    None => a.as_str().cmp(b.to_string().as_str()),
                }
            }

            // String-to-Integer coercion (MySQL compatibility: WHERE int_col = '0')
            (Value::String(a), Value::Int2(b)) => {
                if let Ok(a_i) = a.parse::<i16>() {
                    a_i.cmp(b)
                } else {
                    a.as_str().cmp(&b.to_string().as_str())
                }
            }
            (Value::Int2(a), Value::String(b)) => {
                if let Ok(b_i) = b.parse::<i16>() {
                    a.cmp(&b_i)
                } else {
                    a.to_string().as_str().cmp(b.as_str())
                }
            }
            (Value::String(a), Value::Int4(b)) => {
                if let Ok(a_i) = a.parse::<i32>() {
                    a_i.cmp(b)
                } else {
                    a.as_str().cmp(&b.to_string().as_str())
                }
            }
            (Value::Int4(a), Value::String(b)) => {
                if let Ok(b_i) = b.parse::<i32>() {
                    a.cmp(&b_i)
                } else {
                    a.to_string().as_str().cmp(b.as_str())
                }
            }
            (Value::String(a), Value::Int8(b)) => {
                if let Ok(a_i) = a.parse::<i64>() {
                    a_i.cmp(b)
                } else {
                    a.as_str().cmp(&b.to_string().as_str())
                }
            }
            (Value::Int8(a), Value::String(b)) => {
                if let Ok(b_i) = b.parse::<i64>() {
                    a.cmp(&b_i)
                } else {
                    a.to_string().as_str().cmp(b.as_str())
                }
            }
            // String-to-Float coercion
            (Value::String(a), Value::Float4(b)) => {
                if let Ok(a_f) = a.parse::<f32>() {
                    a_f.partial_cmp(b).unwrap_or(Ordering::Equal)
                } else {
                    a.as_str().cmp(&b.to_string().as_str())
                }
            }
            (Value::Float4(a), Value::String(b)) => {
                if let Ok(b_f) = b.parse::<f32>() {
                    a.partial_cmp(&b_f).unwrap_or(Ordering::Equal)
                } else {
                    a.to_string().as_str().cmp(b.as_str())
                }
            }
            (Value::String(a), Value::Float8(b)) => {
                if let Ok(a_f) = a.parse::<f64>() {
                    a_f.partial_cmp(b).unwrap_or(Ordering::Equal)
                } else {
                    a.as_str().cmp(&b.to_string().as_str())
                }
            }
            (Value::Float8(a), Value::String(b)) => {
                if let Ok(b_f) = b.parse::<f64>() {
                    a.partial_cmp(&b_f).unwrap_or(Ordering::Equal)
                } else {
                    a.to_string().as_str().cmp(b.as_str())
                }
            }
            // Boolean-to-String coercion
            (Value::Boolean(a), Value::String(b)) => {
                let b_bool = matches!(b.as_str(), "1" | "true" | "TRUE" | "t" | "yes");
                a.cmp(&b_bool)
            }
            (Value::String(a), Value::Boolean(b)) => {
                let a_bool = matches!(a.as_str(), "1" | "true" | "TRUE" | "t" | "yes");
                a_bool.cmp(b)
            }
            // Boolean-to-Integer coercion
            (Value::Boolean(a), Value::Int4(b)) => {
                let a_i = i32::from(*a);
                a_i.cmp(b)
            }
            (Value::Int4(a), Value::Boolean(b)) => {
                let b_i = i32::from(*b);
                a.cmp(&b_i)
            }
            (Value::Boolean(a), Value::Int8(b)) => {
                let a_i = i64::from(*a);
                a_i.cmp(b)
            }
            (Value::Int8(a), Value::Boolean(b)) => {
                let b_i = i64::from(*b);
                a.cmp(&b_i)
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
        // SQL standard: NULL op anything = NULL
        if matches!(left, Value::Null) || matches!(right, Value::Null) {
            return Ok(Value::Null);
        }
        match (left, right) {
            (Value::Int4(a), Value::Int4(b)) => {
                let result = op(*a as i64, *b as i64);
                Ok(i32::try_from(result).map_or(Value::Int8(result), Value::Int4))
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
        // SQL standard: NULL + anything = NULL
        if matches!(left, Value::Null) || matches!(right, Value::Null) {
            return Ok(Value::Null);
        }
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
                Ok(i32::try_from(result).map_or(Value::Int8(result), Value::Int4))
            }
            (Value::Int8(a), Value::Int8(b)) => {
                a.checked_add(*b)
                    .map(Value::Int8)
                    .ok_or_else(|| Error::query_execution("integer overflow: BIGINT addition"))
            }
            (Value::Float4(a), Value::Float4(b)) => Ok(Value::Float4(a + b)),
            (Value::Float8(a), Value::Float8(b)) => Ok(Value::Float8(a + b)),
            (Value::Int4(a), Value::Int8(b)) => {
                (*a as i64).checked_add(*b)
                    .map(Value::Int8)
                    .ok_or_else(|| Error::query_execution("integer overflow: BIGINT addition"))
            }
            (Value::Int8(a), Value::Int4(b)) => {
                a.checked_add(*b as i64)
                    .map(Value::Int8)
                    .ok_or_else(|| Error::query_execution("integer overflow: BIGINT addition"))
            }
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
            (Value::Int2(a), Value::Int4(b)) => {
                let result = (*a as i64) + (*b as i64);
                Ok(i32::try_from(result).map_or(Value::Int8(result), Value::Int4))
            }
            (Value::Int4(a), Value::Int2(b)) => {
                let result = (*a as i64) + (*b as i64);
                Ok(i32::try_from(result).map_or(Value::Int8(result), Value::Int4))
            }
            (Value::Int2(a), Value::Int8(b)) => {
                (*a as i64).checked_add(*b)
                    .map(Value::Int8)
                    .ok_or_else(|| Error::query_execution("integer overflow: BIGINT addition"))
            }
            (Value::Int8(a), Value::Int2(b)) => {
                a.checked_add(*b as i64)
                    .map(Value::Int8)
                    .ok_or_else(|| Error::query_execution("integer overflow: BIGINT addition"))
            }
            (Value::Int2(a), Value::Int2(b)) => {
                let result = (*a as i32) + (*b as i32);
                Ok(i16::try_from(result).map_or(Value::Int4(result), Value::Int2))
            }
            (Value::Int2(a), Value::Float4(b)) => Ok(Value::Float4((*a as f32) + b)),
            (Value::Float4(a), Value::Int2(b)) => Ok(Value::Float4(a + (*b as f32))),
            (Value::Int2(a), Value::Float8(b)) => Ok(Value::Float8((*a as f64) + b)),
            (Value::Float8(a), Value::Int2(b)) => Ok(Value::Float8(a + (*b as f64))),
            // Timestamp + Interval arithmetic
            (Value::Timestamp(ts), Value::Interval(micros)) => {
                let duration = chrono::Duration::microseconds(*micros);
                let new_ts = *ts + duration;
                Ok(Value::Timestamp(new_ts))
            }
            (Value::Interval(micros), Value::Timestamp(ts)) => {
                let duration = chrono::Duration::microseconds(*micros);
                let new_ts = *ts + duration;
                Ok(Value::Timestamp(new_ts))
            }
            // Date + Interval arithmetic (add days)
            (Value::Date(d), Value::Interval(micros)) => {
                let days = (*micros / 86_400_000_000) as i64;
                let new_date = *d + chrono::Duration::days(days);
                Ok(Value::Date(new_date))
            }
            (Value::Interval(micros), Value::Date(d)) => {
                let days = (*micros / 86_400_000_000) as i64;
                let new_date = *d + chrono::Duration::days(days);
                Ok(Value::Date(new_date))
            }
            // Interval + Interval
            (Value::Interval(a), Value::Interval(b)) => {
                Ok(Value::Interval(a + b))
            }
            _ => Err(Error::query_execution(format!(
                "Cannot add {:?} and {:?}",
                left, right
            ))),
        }
    }

    /// Subtraction operator with support for Numeric precision
    fn arithmetic_subtract(&self, left: &Value, right: &Value) -> Result<Value> {
        // SQL standard: NULL - anything = NULL
        if matches!(left, Value::Null) || matches!(right, Value::Null) {
            return Ok(Value::Null);
        }
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
                Ok(i32::try_from(result).map_or(Value::Int8(result), Value::Int4))
            }
            (Value::Int8(a), Value::Int8(b)) => {
                a.checked_sub(*b)
                    .map(Value::Int8)
                    .ok_or_else(|| Error::query_execution("integer overflow: BIGINT subtraction"))
            }
            (Value::Float4(a), Value::Float4(b)) => Ok(Value::Float4(a - b)),
            (Value::Float8(a), Value::Float8(b)) => Ok(Value::Float8(a - b)),
            (Value::Int4(a), Value::Int8(b)) => {
                (*a as i64).checked_sub(*b)
                    .map(Value::Int8)
                    .ok_or_else(|| Error::query_execution("integer overflow: BIGINT subtraction"))
            }
            (Value::Int8(a), Value::Int4(b)) => {
                a.checked_sub(*b as i64)
                    .map(Value::Int8)
                    .ok_or_else(|| Error::query_execution("integer overflow: BIGINT subtraction"))
            }
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
            (Value::Int2(a), Value::Int4(b)) => {
                let result = (*a as i64) - (*b as i64);
                Ok(i32::try_from(result).map_or(Value::Int8(result), Value::Int4))
            }
            (Value::Int4(a), Value::Int2(b)) => {
                let result = (*a as i64) - (*b as i64);
                Ok(i32::try_from(result).map_or(Value::Int8(result), Value::Int4))
            }
            (Value::Int2(a), Value::Int8(b)) => {
                (*a as i64).checked_sub(*b)
                    .map(Value::Int8)
                    .ok_or_else(|| Error::query_execution("integer overflow: BIGINT subtraction"))
            }
            (Value::Int8(a), Value::Int2(b)) => {
                a.checked_sub(*b as i64)
                    .map(Value::Int8)
                    .ok_or_else(|| Error::query_execution("integer overflow: BIGINT subtraction"))
            }
            (Value::Int2(a), Value::Int2(b)) => {
                let result = (*a as i32) - (*b as i32);
                Ok(i16::try_from(result).map_or(Value::Int4(result), Value::Int2))
            }
            (Value::Int2(a), Value::Float4(b)) => Ok(Value::Float4((*a as f32) - b)),
            (Value::Float4(a), Value::Int2(b)) => Ok(Value::Float4(a - (*b as f32))),
            (Value::Int2(a), Value::Float8(b)) => Ok(Value::Float8((*a as f64) - b)),
            (Value::Float8(a), Value::Int2(b)) => Ok(Value::Float8(a - (*b as f64))),
            // Timestamp - Interval arithmetic
            (Value::Timestamp(ts), Value::Interval(micros)) => {
                let duration = chrono::Duration::microseconds(*micros);
                let new_ts = *ts - duration;
                Ok(Value::Timestamp(new_ts))
            }
            // Date - Interval arithmetic (subtract days)
            (Value::Date(d), Value::Interval(micros)) => {
                let days = (*micros / 86_400_000_000) as i64;
                let new_date = *d - chrono::Duration::days(days);
                Ok(Value::Date(new_date))
            }
            // Timestamp - Timestamp = Interval
            (Value::Timestamp(a), Value::Timestamp(b)) => {
                let diff = *a - *b;
                let micros = diff.num_microseconds().unwrap_or(0);
                Ok(Value::Interval(micros))
            }
            // Date - Date = Interval (in days)
            (Value::Date(a), Value::Date(b)) => {
                let diff = *a - *b;
                let micros = diff.num_days() * 86_400_000_000;
                Ok(Value::Interval(micros))
            }
            // Interval - Interval
            (Value::Interval(a), Value::Interval(b)) => {
                Ok(Value::Interval(a - b))
            }
            _ => Err(Error::query_execution(format!(
                "Cannot subtract {:?} and {:?}",
                left, right
            ))),
        }
    }

    /// Multiplication operator with support for Numeric precision
    fn arithmetic_multiply(&self, left: &Value, right: &Value) -> Result<Value> {
        // SQL standard: NULL * anything = NULL
        if matches!(left, Value::Null) || matches!(right, Value::Null) {
            return Ok(Value::Null);
        }
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
                Ok(i32::try_from(result).map_or(Value::Int8(result), Value::Int4))
            }
            (Value::Int8(a), Value::Int8(b)) => {
                a.checked_mul(*b)
                    .map(Value::Int8)
                    .ok_or_else(|| Error::query_execution("integer overflow: BIGINT multiplication"))
            }
            (Value::Float4(a), Value::Float4(b)) => Ok(Value::Float4(a * b)),
            (Value::Float8(a), Value::Float8(b)) => Ok(Value::Float8(a * b)),
            (Value::Int4(a), Value::Int8(b)) => {
                (*a as i64).checked_mul(*b)
                    .map(Value::Int8)
                    .ok_or_else(|| Error::query_execution("integer overflow: BIGINT multiplication"))
            }
            (Value::Int8(a), Value::Int4(b)) => {
                a.checked_mul(*b as i64)
                    .map(Value::Int8)
                    .ok_or_else(|| Error::query_execution("integer overflow: BIGINT multiplication"))
            }
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
            (Value::Int2(a), Value::Int4(b)) => {
                let result = (*a as i64) * (*b as i64);
                Ok(i32::try_from(result).map_or(Value::Int8(result), Value::Int4))
            }
            (Value::Int4(a), Value::Int2(b)) => {
                let result = (*a as i64) * (*b as i64);
                Ok(i32::try_from(result).map_or(Value::Int8(result), Value::Int4))
            }
            (Value::Int2(a), Value::Int8(b)) => {
                (*a as i64).checked_mul(*b)
                    .map(Value::Int8)
                    .ok_or_else(|| Error::query_execution("integer overflow: BIGINT multiplication"))
            }
            (Value::Int8(a), Value::Int2(b)) => {
                a.checked_mul(*b as i64)
                    .map(Value::Int8)
                    .ok_or_else(|| Error::query_execution("integer overflow: BIGINT multiplication"))
            }
            (Value::Int2(a), Value::Int2(b)) => {
                let result = (*a as i32) * (*b as i32);
                Ok(i16::try_from(result).map_or(Value::Int4(result), Value::Int2))
            }
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
        // SQL standard: NULL / anything = NULL
        if matches!(left, Value::Null) || matches!(right, Value::Null) {
            return Ok(Value::Null);
        }
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
                Ok(i32::try_from(result).map_or(Value::Int8(result), Value::Int4))
            }
            (Value::Int8(a), Value::Int8(b)) => {
                a.checked_div(*b)
                    .map(Value::Int8)
                    .ok_or_else(|| Error::query_execution("integer overflow: BIGINT division"))
            }
            (Value::Float4(a), Value::Float4(b)) => Ok(Value::Float4(a / b)),
            (Value::Float8(a), Value::Float8(b)) => Ok(Value::Float8(a / b)),
            (Value::Int4(a), Value::Int8(b)) => {
                (*a as i64).checked_div(*b)
                    .map(Value::Int8)
                    .ok_or_else(|| Error::query_execution("integer overflow: BIGINT division"))
            }
            (Value::Int8(a), Value::Int4(b)) => {
                a.checked_div(*b as i64)
                    .map(Value::Int8)
                    .ok_or_else(|| Error::query_execution("integer overflow: BIGINT division"))
            }
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
            (Value::Int2(a), Value::Int4(b)) => {
                let result = (*a as i64) / (*b as i64);
                Ok(i32::try_from(result).map_or(Value::Int8(result), Value::Int4))
            }
            (Value::Int4(a), Value::Int2(b)) => {
                let result = (*a as i64) / (*b as i64);
                Ok(i32::try_from(result).map_or(Value::Int8(result), Value::Int4))
            }
            (Value::Int2(a), Value::Int8(b)) => {
                (*a as i64).checked_div(*b)
                    .map(Value::Int8)
                    .ok_or_else(|| Error::query_execution("integer overflow: BIGINT division"))
            }
            (Value::Int8(a), Value::Int2(b)) => {
                a.checked_div(*b as i64)
                    .map(Value::Int8)
                    .ok_or_else(|| Error::query_execution("integer overflow: BIGINT division"))
            }
            (Value::Int2(a), Value::Int2(b)) => {
                let result = (*a as i32) / (*b as i32);
                Ok(i16::try_from(result).map_or(Value::Int4(result), Value::Int2))
            }
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

    /// Convert a value to an `Option<bool>` for SQL three-valued logic.
    /// Returns `None` for NULL, `Some(true/false)` for booleans.
    fn to_tri_bool(value: &Value) -> Result<Option<bool>> {
        match value {
            Value::Boolean(b) => Ok(Some(*b)),
            Value::Null => Ok(None),
            _ => Err(Error::query_execution(format!(
                "Cannot convert {:?} to boolean",
                value
            ))),
        }
    }

    /// SQL three-valued AND on two already-evaluated values.
    ///
    /// Truth table:
    /// - false AND anything = false
    /// - true  AND true     = true
    /// - true  AND false    = false
    /// - true  AND NULL     = NULL
    /// - NULL  AND false    = false
    /// - NULL  AND true     = NULL
    /// - NULL  AND NULL     = NULL
    fn three_valued_and(left: &Value, right: &Value) -> Result<Value> {
        let l = Self::to_tri_bool(left)?;
        let r = Self::to_tri_bool(right)?;
        match (l, r) {
            (Some(false), _) | (_, Some(false)) => Ok(Value::Boolean(false)),
            (Some(true), Some(true)) => Ok(Value::Boolean(true)),
            _ => Ok(Value::Null), // At least one NULL and neither is false
        }
    }

    /// SQL three-valued OR on two already-evaluated values.
    ///
    /// Truth table:
    /// - true  OR anything = true
    /// - false OR false    = false
    /// - false OR true     = true
    /// - false OR NULL     = NULL
    /// - NULL  OR true     = true
    /// - NULL  OR false    = NULL
    /// - NULL  OR NULL     = NULL
    fn three_valued_or(left: &Value, right: &Value) -> Result<Value> {
        let l = Self::to_tri_bool(left)?;
        let r = Self::to_tri_bool(right)?;
        match (l, r) {
            (Some(true), _) | (_, Some(true)) => Ok(Value::Boolean(true)),
            (Some(false), Some(false)) => Ok(Value::Boolean(false)),
            _ => Ok(Value::Null), // At least one NULL and neither is true
        }
    }

    /// Short-circuit AND evaluation with SQL three-valued NULL logic.
    ///
    /// Evaluates the left side first. If left is definitively false, returns
    /// false without evaluating the right side (preventing errors like
    /// comparing NULL values on the right side).
    fn evaluate_and_short_circuit(
        &self,
        left: &LogicalExpr,
        right: &LogicalExpr,
        tuple: &Tuple,
    ) -> Result<Value> {
        let left_val = self.evaluate(left, tuple)?;
        match &left_val {
            // false AND anything = false (short-circuit)
            Value::Boolean(false) => Ok(Value::Boolean(false)),
            // true AND right = right (must evaluate right)
            Value::Boolean(true) => {
                let right_val = self.evaluate(right, tuple)?;
                match &right_val {
                    Value::Boolean(b) => Ok(Value::Boolean(*b)),
                    Value::Null => Ok(Value::Null),
                    _ => Err(Error::query_execution(format!(
                        "Cannot convert {:?} to boolean", right_val
                    ))),
                }
            }
            // NULL AND right: must evaluate right to check for false
            // NULL AND false = false, NULL AND true = NULL, NULL AND NULL = NULL
            Value::Null => {
                let right_val = self.evaluate(right, tuple)?;
                match &right_val {
                    Value::Boolean(false) => Ok(Value::Boolean(false)),
                    Value::Boolean(true) | Value::Null => Ok(Value::Null),
                    _ => Err(Error::query_execution(format!(
                        "Cannot convert {:?} to boolean", right_val
                    ))),
                }
            }
            _ => Err(Error::query_execution(format!(
                "Cannot convert {:?} to boolean", left_val
            ))),
        }
    }

    /// Short-circuit OR evaluation with SQL three-valued NULL logic.
    ///
    /// Evaluates the left side first. If left is definitively true, returns
    /// true without evaluating the right side.
    fn evaluate_or_short_circuit(
        &self,
        left: &LogicalExpr,
        right: &LogicalExpr,
        tuple: &Tuple,
    ) -> Result<Value> {
        let left_val = self.evaluate(left, tuple)?;
        match &left_val {
            // true OR anything = true (short-circuit)
            Value::Boolean(true) => Ok(Value::Boolean(true)),
            // false OR right = right (must evaluate right)
            Value::Boolean(false) => {
                let right_val = self.evaluate(right, tuple)?;
                match &right_val {
                    Value::Boolean(b) => Ok(Value::Boolean(*b)),
                    Value::Null => Ok(Value::Null),
                    _ => Err(Error::query_execution(format!(
                        "Cannot convert {:?} to boolean", right_val
                    ))),
                }
            }
            // NULL OR right: must evaluate right to check for true
            // NULL OR true = true, NULL OR false = NULL, NULL OR NULL = NULL
            Value::Null => {
                let right_val = self.evaluate(right, tuple)?;
                match &right_val {
                    Value::Boolean(true) => Ok(Value::Boolean(true)),
                    Value::Boolean(false) | Value::Null => Ok(Value::Null),
                    _ => Err(Error::query_execution(format!(
                        "Cannot convert {:?} to boolean", right_val
                    ))),
                }
            }
            _ => Err(Error::query_execution(format!(
                "Cannot convert {:?} to boolean", left_val
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
        let [a, b] = args else {
            return Err(Error::query_execution(
                "COSINE_SIMILARITY requires exactly 2 vector arguments".to_string()
            ));
        };
        let distance = self.vector_distance_op(a, b, crate::vector::cosine_distance)?;
        match distance {
            Value::Float4(d) => Ok(Value::Float4(1.0 - d)),
            _ => Err(Error::query_execution("Unexpected result type".to_string())),
        }
    }

    /// COSINE_DISTANCE(v1, v2) - returns cosine distance
    fn vector_cosine_distance(&self, args: &[Value]) -> Result<Value> {
        let [a, b] = args else {
            return Err(Error::query_execution(
                "COSINE_DISTANCE requires exactly 2 vector arguments".to_string()
            ));
        };
        self.vector_distance_op(a, b, crate::vector::cosine_distance)
    }

    /// L2_DISTANCE(v1, v2) - returns Euclidean distance
    fn vector_l2_distance(&self, args: &[Value]) -> Result<Value> {
        let [a, b] = args else {
            return Err(Error::query_execution(
                "L2_DISTANCE requires exactly 2 vector arguments".to_string()
            ));
        };
        self.vector_distance_op(a, b, crate::vector::l2_distance)
    }

    /// INNER_PRODUCT(v1, v2) - returns inner product distance
    fn vector_inner_product(&self, args: &[Value]) -> Result<Value> {
        let [a, b] = args else {
            return Err(Error::query_execution(
                "INNER_PRODUCT requires exactly 2 vector arguments".to_string()
            ));
        };
        self.vector_distance_op(a, b, crate::vector::inner_product_distance)
    }

    /// COALESCE(val1, val2, ...) - return first non-null value
    fn func_coalesce(&self, args: &[Value]) -> Result<Value> {
        for arg in args {
            if !matches!(arg, Value::Null) {
                return Ok(arg.clone());
            }
        }
        // All values are NULL, return NULL
        Ok(Value::Null)
    }

    /// NULLIF(val1, val2) - return NULL if val1 = val2, else val1
    fn func_nullif(&self, args: &[Value]) -> Result<Value> {
        let [val1, val2] = args else {
            return Err(Error::query_execution("NULLIF requires exactly 2 arguments"));
        };

        // If val1 equals val2, return NULL
        if self.values_equal(val1, val2) {
            Ok(Value::Null)
        } else {
            Ok(val1.clone())
        }
    }

    /// array_length(arr, dimension) - returns length of array dimension (1-based)
    /// PostgreSQL compatible: dimension is typically 1 for one-dimensional arrays
    fn array_length(&self, args: &[Value]) -> Result<Value> {
        let [a, b] = args else {
            return Err(Error::query_execution(
                "array_length requires exactly two arguments"
            ));
        };

        match (a, b) {
            (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
            (Value::Array(arr), Value::Int2(dim)) if *dim == 1 => {
                Ok(Value::Int4(arr.len() as i32))
            }
            (Value::Array(arr), Value::Int4(dim)) if *dim == 1 => {
                Ok(Value::Int4(arr.len() as i32))
            }
            (Value::Array(arr), Value::Int8(dim)) if *dim == 1 => {
                Ok(Value::Int4(arr.len() as i32))
            }
            // Also support Vector type
            (Value::Vector(vec), Value::Int2(dim)) if *dim == 1 => {
                Ok(Value::Int4(vec.len() as i32))
            }
            (Value::Vector(vec), Value::Int4(dim)) if *dim == 1 => {
                Ok(Value::Int4(vec.len() as i32))
            }
            (Value::Vector(vec), Value::Int8(dim)) if *dim == 1 => {
                Ok(Value::Int4(vec.len() as i32))
            }
            (Value::Array(_), _) | (Value::Vector(_), _) => {
                // Dimension other than 1 for one-dimensional array returns NULL
                Ok(Value::Null)
            }
            _ => Err(Error::query_execution(
                "array_length requires an array and an integer dimension"
            )),
        }
    }

    /// array_upper(arr, dimension) - returns upper bound of array dimension (1-based)
    fn array_upper(&self, args: &[Value]) -> Result<Value> {
        let [a, b] = args else {
            return Err(Error::query_execution(
                "array_upper requires exactly two arguments"
            ));
        };

        match (a, b) {
            (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
            (Value::Array(arr), Value::Int2(dim)) if *dim == 1 => {
                Ok(Value::Int4(arr.len() as i32))
            }
            (Value::Array(arr), Value::Int4(dim)) if *dim == 1 => {
                Ok(Value::Int4(arr.len() as i32))
            }
            (Value::Array(arr), Value::Int8(dim)) if *dim == 1 => {
                Ok(Value::Int4(arr.len() as i32))
            }
            (Value::Array(_), _) => Ok(Value::Null),
            _ => Err(Error::query_execution(
                "array_upper requires an array and an integer dimension"
            )),
        }
    }

    /// array_lower(arr, dimension) - returns lower bound of array dimension (always 1 in PostgreSQL)
    fn array_lower(&self, args: &[Value]) -> Result<Value> {
        let [a, b] = args else {
            return Err(Error::query_execution(
                "array_lower requires exactly two arguments"
            ));
        };

        match (a, b) {
            (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
            (Value::Array(arr), _) if arr.is_empty() => Ok(Value::Null),
            (Value::Array(_), Value::Int2(dim)) if *dim == 1 => {
                Ok(Value::Int4(1))
            }
            (Value::Array(_), Value::Int4(dim)) if *dim == 1 => {
                Ok(Value::Int4(1))
            }
            (Value::Array(_), Value::Int8(dim)) if *dim == 1 => {
                Ok(Value::Int4(1))
            }
            (Value::Array(_), _) => Ok(Value::Null),
            _ => Err(Error::query_execution(
                "array_lower requires an array and an integer dimension"
            )),
        }
    }

    /// array_append(arr, element) - appends element to array
    fn array_append(&self, args: &[Value]) -> Result<Value> {
        let [a, b] = args else {
            return Err(Error::query_execution(
                "array_append requires exactly two arguments"
            ));
        };

        match (a, b) {
            (Value::Array(arr), elem) => {
                let mut result = arr.clone();
                result.push(elem.clone());
                Ok(Value::Array(result))
            }
            (Value::Null, elem) => {
                Ok(Value::Array(vec![elem.clone()]))
            }
            _ => Err(Error::query_execution(
                "array_append requires an array as first argument"
            )),
        }
    }

    /// array_prepend(element, arr) - prepends element to array
    fn array_prepend(&self, args: &[Value]) -> Result<Value> {
        let [a, b] = args else {
            return Err(Error::query_execution(
                "array_prepend requires exactly two arguments"
            ));
        };

        match (a, b) {
            (elem, Value::Array(arr)) => {
                let mut result = vec![elem.clone()];
                result.extend(arr.clone());
                Ok(Value::Array(result))
            }
            (elem, Value::Null) => {
                Ok(Value::Array(vec![elem.clone()]))
            }
            _ => Err(Error::query_execution(
                "array_prepend requires an array as second argument"
            )),
        }
    }

    /// array_cat(arr1, arr2) - concatenates two arrays
    fn array_cat(&self, args: &[Value]) -> Result<Value> {
        let [a, b] = args else {
            return Err(Error::query_execution(
                "array_cat requires exactly two arguments"
            ));
        };

        self.array_concat_op(a, b)
    }

    /// array_remove(arr, element) - removes all occurrences of element from array
    fn array_remove(&self, args: &[Value]) -> Result<Value> {
        let [a, b] = args else {
            return Err(Error::query_execution(
                "array_remove requires exactly two arguments"
            ));
        };

        match (a, b) {
            (Value::Array(arr), elem) => {
                let result: Vec<Value> = arr.iter()
                    .filter(|v| *v != elem)
                    .cloned()
                    .collect();
                Ok(Value::Array(result))
            }
            (Value::Null, _) => Ok(Value::Null),
            _ => Err(Error::query_execution(
                "array_remove requires an array as first argument"
            )),
        }
    }

    /// array_position(arr, element) - returns 1-based position of first occurrence
    fn array_position(&self, args: &[Value]) -> Result<Value> {
        let [a, b] = args else {
            return Err(Error::query_execution(
                "array_position requires exactly two arguments"
            ));
        };

        match (a, b) {
            (Value::Array(arr), elem) => {
                for (i, v) in arr.iter().enumerate() {
                    if v == elem {
                        return Ok(Value::Int4((i + 1) as i32)); // 1-based index
                    }
                }
                Ok(Value::Null)
            }
            (Value::Null, _) => Ok(Value::Null),
            _ => Err(Error::query_execution(
                "array_position requires an array as first argument"
            )),
        }
    }

    /// cardinality(arr) - returns total number of elements in array
    fn array_cardinality(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution(
                "cardinality requires exactly one argument"
            ));
        };

        match arg {
            Value::Array(arr) => Ok(Value::Int8(arr.len() as i64)),
            Value::Vector(vec) => Ok(Value::Int8(vec.len() as i64)),
            Value::Null => Ok(Value::Null),
            _ => Err(Error::query_execution(
                "cardinality requires an array argument"
            )),
        }
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
                Value::Int4(i) => i16::try_from(i).map(Value::Int2).map_err(|_| Error::query_execution(format!("value out of range for SMALLINT: {}", i))),
                Value::Int8(i) => i16::try_from(i).map(Value::Int2).map_err(|_| Error::query_execution(format!("value out of range for SMALLINT: {}", i))),
                Value::Float4(f) => { let i = f as i64; i16::try_from(i).map(Value::Int2).map_err(|_| Error::query_execution(format!("value out of range for SMALLINT: {}", f))) },
                Value::Float8(f) => { let i = f as i64; i16::try_from(i).map(Value::Int2).map_err(|_| Error::query_execution(format!("value out of range for SMALLINT: {}", f))) },
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
                        .and_then(|dec| {
                            dec.to_f32()
                                .map(Value::Float4)
                                .ok_or_else(|| Error::query_execution(format!("Cannot cast '{}' to FLOAT4: value out of range", n)))
                        })
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
                        .and_then(|dec| {
                            dec.to_f64()
                                .map(Value::Float8)
                                .ok_or_else(|| Error::query_execution(format!("Cannot cast '{}' to FLOAT8: value out of range", n)))
                        })
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
                        .or_else(|e| {
                            // Date-only format: treat as midnight UTC
                            if let Ok(date) = chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d") {
                                if let Some(ndt) = date.and_hms_opt(0, 0, 0) {
                                    return Ok(Value::Timestamp(chrono::DateTime::from_naive_utc_and_offset(ndt, Utc)));
                                }
                            }
                            Err(e)
                        })
                        .map_err(|e| Error::query_execution(format!("Cannot cast '{}' to TIMESTAMP: {}", s, e)))
                }
                _ => Err(Error::query_execution(format!("Cannot cast {:?} to TIMESTAMP", value))),
            },

            DataType::Uuid => match value {
                Value::Uuid(u) => Ok(Value::Uuid(u)),
                Value::String(s) => uuid::Uuid::parse_str(&s)
                    .map(Value::Uuid)
                    .map_err(|e| Error::query_execution(format!("Cannot cast '{}' to UUID: {}", s, e))),
                _ => Err(Error::query_execution(format!("Cannot cast {:?} to UUID", value))),
            },

            DataType::Bytea => match value {
                Value::Bytes(b) => Ok(Value::Bytes(b)),
                Value::String(s) => {
                    // Support hex format: \x... or 0x...
                    let trimmed = s.trim();
                    if let Some(hex_str) = trimmed.strip_prefix("\\x").or_else(|| trimmed.strip_prefix("0x")) {
                        hex::decode(hex_str)
                            .map(Value::Bytes)
                            .map_err(|e| Error::query_execution(format!("Cannot cast '{}' to BYTEA: {}", s, e)))
                    } else {
                        // Raw string as bytes
                        Ok(Value::Bytes(s.into_bytes()))
                    }
                }
                _ => Err(Error::query_execution(format!("Cannot cast {:?} to BYTEA", value))),
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
    #[allow(clippy::float_cmp)]
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
            // String-to-UUID coercion for IN lists and equality
            (Value::Uuid(a), Value::String(b)) => uuid::Uuid::parse_str(b).is_ok_and(|b_uuid| *a == b_uuid),
            (Value::String(a), Value::Uuid(b)) => uuid::Uuid::parse_str(a).is_ok_and(|a_uuid| a_uuid == *b),
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

            // Numeric (DECIMAL) cross-type comparisons — Numeric stores decimal as String
            (Value::Numeric(a), Value::Float8(b)) => {
                a.parse::<f64>().is_ok_and(|a| a == *b)
            }
            (Value::Float8(a), Value::Numeric(b)) => {
                b.parse::<f64>().is_ok_and(|b| *a == b)
            }
            (Value::Numeric(a), Value::Float4(b)) => {
                a.parse::<f64>().is_ok_and(|a| a == f64::from(*b))
            }
            (Value::Float4(a), Value::Numeric(b)) => {
                b.parse::<f64>().is_ok_and(|b| f64::from(*a) == b)
            }
            (Value::Numeric(a), Value::Int2(b)) => {
                a.parse::<Decimal>().is_ok_and(|a| a == Decimal::from(*b))
            }
            (Value::Int2(a), Value::Numeric(b)) => {
                b.parse::<Decimal>().is_ok_and(|b| Decimal::from(*a) == b)
            }
            (Value::Numeric(a), Value::Int4(b)) => {
                a.parse::<Decimal>().is_ok_and(|a| a == Decimal::from(*b))
            }
            (Value::Int4(a), Value::Numeric(b)) => {
                b.parse::<Decimal>().is_ok_and(|b| Decimal::from(*a) == b)
            }
            (Value::Numeric(a), Value::Int8(b)) => {
                a.parse::<Decimal>().is_ok_and(|a| a == Decimal::from(*b))
            }
            (Value::Int8(a), Value::Numeric(b)) => {
                b.parse::<Decimal>().is_ok_and(|b| Decimal::from(*a) == b)
            }

            // String↔Int coercion (MySQL sends WHERE id IN ('9') via $wpdb->prepare)
            (Value::String(s), Value::Int8(n)) | (Value::Int8(n), Value::String(s)) => {
                s.parse::<i64>().is_ok_and(|parsed| parsed == *n)
            }
            (Value::String(s), Value::Int4(n)) | (Value::Int4(n), Value::String(s)) => {
                s.parse::<i32>().is_ok_and(|parsed| parsed == *n)
            }
            (Value::String(s), Value::Int2(n)) | (Value::Int2(n), Value::String(s)) => {
                s.parse::<i16>().is_ok_and(|parsed| parsed == *n)
            }
            // String↔Float coercion
            (Value::String(s), Value::Float8(n)) | (Value::Float8(n), Value::String(s)) => {
                s.parse::<f64>().is_ok_and(|parsed| (parsed - *n).abs() < f64::EPSILON)
            }
            (Value::String(s), Value::Float4(n)) | (Value::Float4(n), Value::String(s)) => {
                s.parse::<f32>().is_ok_and(|parsed| (parsed - *n).abs() < f32::EPSILON)
            }
            // String↔Bool coercion
            (Value::String(s), Value::Boolean(b)) | (Value::Boolean(b), Value::String(s)) => {
                matches!((s.as_str(), b), ("1" | "true" | "TRUE" | "t", true) | ("0" | "false" | "FALSE" | "f", false))
            }

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

    /// String concatenation operator: ||
    /// Per SQL standard: if either operand is NULL, the result is NULL.
    /// Non-string types are cast to their string representation (PostgreSQL behavior).
    /// If either operand is an array, delegates to array concatenation instead.
    fn string_concat_op(&self, left: &Value, right: &Value) -> Result<Value> {
        // If either operand is an array, delegate to array concatenation
        if matches!(left, Value::Array(_)) || matches!(right, Value::Array(_)) {
            return self.array_concat_op(left, right);
        }
        // SQL standard: NULL || anything = NULL
        if matches!(left, Value::Null) || matches!(right, Value::Null) {
            return Ok(Value::Null);
        }
        let left_str = Self::value_to_concat_string(left);
        let right_str = Self::value_to_concat_string(right);
        Ok(Value::String(format!("{}{}", left_str, right_str)))
    }

    /// Convert a Value to its string representation for concatenation.
    /// Unlike Display, this returns the raw value without quoting.
    fn value_to_concat_string(value: &Value) -> String {
        match value {
            Value::Null => String::new(),
            Value::Boolean(b) => b.to_string(),
            Value::Int2(i) => i.to_string(),
            Value::Int4(i) => i.to_string(),
            Value::Int8(i) => i.to_string(),
            Value::Float4(f) => f.to_string(),
            Value::Float8(f) => f.to_string(),
            Value::Numeric(n) => n.clone(),
            Value::String(s) => s.clone(),
            Value::Bytes(b) => format!("\\x{}", hex::encode(b)),
            Value::Uuid(u) => u.to_string(),
            Value::Timestamp(ts) => ts.to_rfc3339(),
            Value::Date(d) => d.format("%Y-%m-%d").to_string(),
            Value::Time(t) => t.format("%H:%M:%S%.f").to_string(),
            Value::Interval(micros) => {
                let total_secs = micros / 1_000_000;
                let days = total_secs / 86400;
                let hours = (total_secs % 86400) / 3600;
                let mins = (total_secs % 3600) / 60;
                let secs = total_secs % 60;
                if days > 0 {
                    format!("{} days {:02}:{:02}:{:02}", days, hours, mins, secs)
                } else {
                    format!("{:02}:{:02}:{:02}", hours, mins, secs)
                }
            }
            Value::Json(j) => j.clone(),
            Value::Array(arr) => {
                let elems: Vec<String> = arr.iter().map(|v| Self::value_to_concat_string(v)).collect();
                format!("{{{}}}", elems.join(","))
            }
            Value::Vector(vec) => format!("[{}]", vec.iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(",")),
            Value::DictRef { dict_id } => format!("<dict:{}>", dict_id),
            Value::CasRef { hash } => format!("<cas:{}>", hex::encode(&hash[..8])),
            Value::ColumnarRef => "<columnar>".to_string(),
        }
    }

    /// LIKE pattern matching operator
    /// Supports SQL LIKE patterns: % (any sequence), _ (single char)
    fn like_op(&self, left: &Value, right: &Value, negated: bool) -> Result<Value> {
        // Handle NULL values
        if matches!(left, Value::Null) || matches!(right, Value::Null) {
            return Ok(Value::Null);
        }

        // Get string values
        let text = match left {
            Value::String(s) => s.as_str(),
            _ => return Err(Error::query_execution(format!(
                "LIKE requires string operand, got {:?}", left
            ))),
        };

        let pattern = match right {
            Value::String(s) => s.as_str(),
            _ => return Err(Error::query_execution(format!(
                "LIKE pattern must be a string, got {:?}", right
            ))),
        };

        // Convert SQL LIKE pattern to regex
        let regex_pattern = self.like_pattern_to_regex(pattern);

        let result = match regex::Regex::new(&regex_pattern) {
            Ok(re) => re.is_match(text),
            Err(e) => return Err(Error::query_execution(format!(
                "Invalid LIKE pattern '{}': {}", pattern, e
            ))),
        };

        Ok(Value::Boolean(if negated { !result } else { result }))
    }

    /// Convert SQL LIKE pattern to regex pattern
    /// % -> .* (any sequence)
    /// _ -> . (single char)
    /// Escape regex special chars
    fn like_pattern_to_regex(&self, pattern: &str) -> String {
        let mut regex = String::with_capacity(pattern.len() * 2 + 2);
        regex.push('^'); // Anchor at start

        let mut chars = pattern.chars();
        while let Some(c) = chars.next() {
            match c {
                // Escape character - next char is literal
                '\\' => {
                    if let Some(next) = chars.next() {
                        // Escape regex special chars
                        if "^$.*+?{}[]|()\\".contains(next) {
                            regex.push('\\');
                        }
                        regex.push(next);
                    }
                }
                // SQL wildcards
                '%' => regex.push_str(".*"),
                '_' => regex.push('.'),
                // Escape regex special characters
                '^' | '$' | '.' | '*' | '+' | '?' | '{' | '}' | '[' | ']' | '|' | '(' | ')' => {
                    regex.push('\\');
                    regex.push(c);
                }
                // Regular character
                _ => regex.push(c),
            }
        }

        regex.push('$'); // Anchor at end
        regex
    }

    /// ILIKE pattern matching operator (case-insensitive LIKE)
    fn ilike_op(&self, left: &Value, right: &Value, negated: bool) -> Result<Value> {
        // Handle NULL values
        if matches!(left, Value::Null) || matches!(right, Value::Null) {
            return Ok(Value::Null);
        }

        // Get string values
        let text = match left {
            Value::String(s) => s.to_lowercase(),
            _ => return Err(Error::query_execution(format!(
                "ILIKE requires string operand, got {:?}", left
            ))),
        };

        let pattern = match right {
            Value::String(s) => s.to_lowercase(),
            _ => return Err(Error::query_execution(format!(
                "ILIKE pattern must be a string, got {:?}", right
            ))),
        };

        // Convert SQL LIKE pattern to regex
        let regex_pattern = self.like_pattern_to_regex(&pattern);

        let result = match regex::Regex::new(&regex_pattern) {
            Ok(re) => re.is_match(&text),
            Err(e) => return Err(Error::query_execution(format!(
                "Invalid ILIKE pattern '{}': {}", pattern, e
            ))),
        };

        Ok(Value::Boolean(if negated { !result } else { result }))
    }

    /// Regular expression matching operator (POSIX ~, ~*, !~, !~*)
    fn regex_op(&self, left: &Value, right: &Value, negated: bool, case_insensitive: bool) -> Result<Value> {
        // Handle NULL values
        if matches!(left, Value::Null) || matches!(right, Value::Null) {
            return Ok(Value::Null);
        }

        // Get string values
        let text = match left {
            Value::String(s) => s.as_str(),
            _ => return Err(Error::query_execution(format!(
                "Regex match requires string operand, got {:?}", left
            ))),
        };

        let pattern = match right {
            Value::String(s) => s.as_str(),
            _ => return Err(Error::query_execution(format!(
                "Regex pattern must be a string, got {:?}", right
            ))),
        };

        // Build regex with optional case-insensitivity
        let regex_pattern = if case_insensitive {
            format!("(?i){}", pattern)
        } else {
            pattern.to_string()
        };

        let result = match regex::Regex::new(&regex_pattern) {
            Ok(re) => re.is_match(text),
            Err(e) => return Err(Error::query_execution(format!(
                "Invalid regex pattern '{}': {}", pattern, e
            ))),
        };

        Ok(Value::Boolean(if negated { !result } else { result }))
    }

    /// SQL SIMILAR TO pattern matching
    /// SIMILAR TO uses SQL regex syntax: % -> .*, _ -> ., | for alternation
    fn similar_to_op(&self, left: &Value, right: &Value, negated: bool) -> Result<Value> {
        // Handle NULL values
        if matches!(left, Value::Null) || matches!(right, Value::Null) {
            return Ok(Value::Null);
        }

        // Get string values
        let text = match left {
            Value::String(s) => s.as_str(),
            _ => return Err(Error::query_execution(format!(
                "SIMILAR TO requires string operand, got {:?}", left
            ))),
        };

        let pattern = match right {
            Value::String(s) => s.as_str(),
            _ => return Err(Error::query_execution(format!(
                "SIMILAR TO pattern must be a string, got {:?}", right
            ))),
        };

        // Convert SQL SIMILAR TO pattern to regex
        let regex_pattern = self.similar_to_pattern_to_regex(pattern);

        let result = match regex::Regex::new(&regex_pattern) {
            Ok(re) => re.is_match(text),
            Err(e) => return Err(Error::query_execution(format!(
                "Invalid SIMILAR TO pattern '{}': {}", pattern, e
            ))),
        };

        Ok(Value::Boolean(if negated { !result } else { result }))
    }

    /// Convert SQL SIMILAR TO pattern to regex
    /// % -> .*, _ -> ., | is kept, other regex chars need escaping
    fn similar_to_pattern_to_regex(&self, pattern: &str) -> String {
        let mut regex = String::with_capacity(pattern.len() * 2 + 2);
        regex.push('^'); // Anchor at start

        let mut chars = pattern.chars();
        while let Some(c) = chars.next() {
            match c {
                // Escape character
                '\\' => {
                    if let Some(next) = chars.next() {
                        if "^$.*+?{}[]|()\\".contains(next) {
                            regex.push('\\');
                        }
                        regex.push(next);
                    }
                }
                // SQL wildcards
                '%' => regex.push_str(".*"),
                '_' => regex.push('.'),
                // SIMILAR TO allows | for alternation and () for grouping
                '|' | '(' | ')' => regex.push(c),
                // Character class
                '[' => {
                    regex.push('[');
                    // Copy until closing ]
                    for inner in chars.by_ref() {
                        regex.push(inner);
                        if inner == ']' {
                            break;
                        }
                    }
                }
                // Escape other regex special characters
                '^' | '$' | '.' | '*' | '+' | '?' | '{' | '}' => {
                    regex.push('\\');
                    regex.push(c);
                }
                // Regular character
                _ => regex.push(c),
            }
        }

        regex.push('$'); // Anchor at end
        regex
    }

    /// Modulo operator
    fn arithmetic_modulo(&self, left: &Value, right: &Value) -> Result<Value> {
        // SQL standard: NULL % anything = NULL
        if matches!(left, Value::Null) || matches!(right, Value::Null) {
            return Ok(Value::Null);
        }
        match (left, right) {
            (Value::Int2(a), Value::Int2(b)) => {
                if *b == 0 { return Err(Error::query_execution("Division by zero")); }
                a.checked_rem(*b)
                    .map(Value::Int2)
                    .ok_or_else(|| Error::query_execution("integer overflow: SMALLINT modulo"))
            }
            (Value::Int4(a), Value::Int4(b)) => {
                if *b == 0 { return Err(Error::query_execution("Division by zero")); }
                a.checked_rem(*b)
                    .map(Value::Int4)
                    .ok_or_else(|| Error::query_execution("integer overflow: INT modulo"))
            }
            (Value::Int8(a), Value::Int8(b)) => {
                if *b == 0 { return Err(Error::query_execution("Division by zero")); }
                a.checked_rem(*b)
                    .map(Value::Int8)
                    .ok_or_else(|| Error::query_execution("integer overflow: BIGINT modulo"))
            }
            // Cross-type integer modulo
            (Value::Int4(a), Value::Int8(b)) => {
                if *b == 0 { return Err(Error::query_execution("Division by zero")); }
                (*a as i64).checked_rem(*b)
                    .map(Value::Int8)
                    .ok_or_else(|| Error::query_execution("integer overflow: BIGINT modulo"))
            }
            (Value::Int8(a), Value::Int4(b)) => {
                if *b == 0 { return Err(Error::query_execution("Division by zero")); }
                a.checked_rem(*b as i64)
                    .map(Value::Int8)
                    .ok_or_else(|| Error::query_execution("integer overflow: BIGINT modulo"))
            }
            _ => Err(Error::query_execution(format!(
                "Modulo requires integer operands, got {:?} % {:?}", left, right
            ))),
        }
    }

    // ========== String Functions ==========

    /// UPPER(string) - convert to uppercase
    fn func_upper(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("UPPER requires exactly one argument"));
        };
        match arg {
            Value::Null => Ok(Value::Null),
            Value::String(s) => Ok(Value::String(s.to_uppercase())),
            _ => Err(Error::query_execution("UPPER requires a string argument")),
        }
    }

    /// LOWER(string) - convert to lowercase
    fn func_lower(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("LOWER requires exactly one argument"));
        };
        match arg {
            Value::Null => Ok(Value::Null),
            Value::String(s) => Ok(Value::String(s.to_lowercase())),
            _ => Err(Error::query_execution("LOWER requires a string argument")),
        }
    }

    /// LENGTH(string) - get string length in characters
    fn func_length(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("LENGTH requires exactly one argument"));
        };
        match arg {
            Value::Null => Ok(Value::Null),
            Value::String(s) => Ok(Value::Int4(s.chars().count() as i32)),
            Value::Bytes(b) => Ok(Value::Int4(b.len() as i32)),
            _ => Err(Error::query_execution("LENGTH requires a string argument")),
        }
    }

    /// SUBSTR(string, start [, length]) - extract substring (1-based indexing)
    fn func_substr(&self, args: &[Value]) -> Result<Value> {
        if args.len() < 2 || args.len() > 3 {
            return Err(Error::query_execution("SUBSTR requires 2 or 3 arguments"));
        }

        let arg0 = args.get(0).ok_or_else(|| Error::query_execution("SUBSTR: missing string"))?;
        let arg1 = args.get(1).ok_or_else(|| Error::query_execution("SUBSTR: missing start"))?;

        let s = match arg0 {
            Value::Null => return Ok(Value::Null),
            Value::String(s) => s,
            _ => return Err(Error::query_execution("SUBSTR first argument must be a string")),
        };

        let start = match arg1 {
            Value::Int2(n) => *n as i64,
            Value::Int4(n) => *n as i64,
            Value::Int8(n) => *n,
            _ => return Err(Error::query_execution("SUBSTR start must be an integer")),
        };

        // SQL uses 1-based indexing
        let start_idx = if start < 1 { 0 } else { (start - 1) as usize };
        let chars: Vec<char> = s.chars().collect();

        let result = if let Some(arg2) = args.get(2) {
            let length = match arg2 {
                Value::Int2(n) => *n as usize,
                Value::Int4(n) => *n as usize,
                Value::Int8(n) => *n as usize,
                _ => return Err(Error::query_execution("SUBSTR length must be an integer")),
            };
            chars.iter().skip(start_idx).take(length).collect::<String>()
        } else {
            chars.iter().skip(start_idx).collect::<String>()
        };

        Ok(Value::String(result))
    }

    /// TRIM([LEADING|TRAILING|BOTH] [characters] FROM string) or TRIM(string [, characters])
    fn func_trim(&self, args: &[Value], mode: Option<&str>) -> Result<Value> {
        if args.is_empty() || args.len() > 2 {
            return Err(Error::query_execution("TRIM requires 1 or 2 arguments"));
        }

        let first = args.first().ok_or_else(|| Error::query_execution("TRIM requires at least 1 argument"))?;

        let s = match first {
            Value::Null => return Ok(Value::Null),
            Value::String(s) => s.as_str(),
            _ => return Err(Error::query_execution("TRIM argument must be a string")),
        };

        let chars_to_trim: &[char] = if let Some(second) = args.get(1) {
            match second {
                Value::String(chars) => &chars.chars().collect::<Vec<_>>(),
                _ => &[' '],
            }
        } else {
            &[' ']
        };

        let result = match mode {
            Some("left") => s.trim_start_matches(chars_to_trim),
            Some("right") => s.trim_end_matches(chars_to_trim),
            _ => s.trim_matches(chars_to_trim),
        };

        Ok(Value::String(result.to_string()))
    }

    /// CONCAT(str1, str2, ...) - concatenate strings
    fn func_concat(&self, args: &[Value]) -> Result<Value> {
        let mut result = String::new();
        for arg in args {
            match arg {
                Value::Null => {} // NULL is treated as empty string in CONCAT
                Value::String(s) => result.push_str(s),
                other => result.push_str(&other.to_string()),
            }
        }
        Ok(Value::String(result))
    }

    /// CONCAT_WS(separator, str1, str2, ...) - concatenate with separator
    fn func_concat_ws(&self, args: &[Value]) -> Result<Value> {
        let (first, rest) = args.split_first().ok_or_else(|| {
            Error::query_execution("CONCAT_WS requires at least one argument")
        })?;

        let sep = match first {
            Value::Null => return Ok(Value::Null),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };

        let parts: Vec<String> = rest.iter()
            .filter_map(|arg| match arg {
                Value::Null => None,
                Value::String(s) => Some(s.clone()),
                other => Some(other.to_string()),
            })
            .collect();

        Ok(Value::String(parts.join(&sep)))
    }

    /// LEFT(string, n) - get first n characters
    fn func_left(&self, args: &[Value]) -> Result<Value> {
        let [a, b] = args else {
            return Err(Error::query_execution("LEFT requires exactly 2 arguments"));
        };
        let s = match a {
            Value::Null => return Ok(Value::Null),
            Value::String(s) => s,
            _ => return Err(Error::query_execution("LEFT first argument must be a string")),
        };
        let n = match b {
            Value::Int2(n) => *n as usize,
            Value::Int4(n) => *n as usize,
            Value::Int8(n) => *n as usize,
            _ => return Err(Error::query_execution("LEFT second argument must be an integer")),
        };
        Ok(Value::String(s.chars().take(n).collect()))
    }

    /// RIGHT(string, n) - get last n characters
    fn func_right(&self, args: &[Value]) -> Result<Value> {
        let [a, b] = args else {
            return Err(Error::query_execution("RIGHT requires exactly 2 arguments"));
        };
        let s = match a {
            Value::Null => return Ok(Value::Null),
            Value::String(s) => s,
            _ => return Err(Error::query_execution("RIGHT first argument must be a string")),
        };
        let n = match b {
            Value::Int2(n) => *n as usize,
            Value::Int4(n) => *n as usize,
            Value::Int8(n) => *n as usize,
            _ => return Err(Error::query_execution("RIGHT second argument must be an integer")),
        };
        let chars: Vec<char> = s.chars().collect();
        let skip = chars.len().saturating_sub(n);
        Ok(Value::String(chars.into_iter().skip(skip).collect()))
    }

    /// REPEAT(string, n) - repeat string n times
    fn func_repeat(&self, args: &[Value]) -> Result<Value> {
        let [a, b] = args else {
            return Err(Error::query_execution("REPEAT requires exactly 2 arguments"));
        };
        let s = match a {
            Value::Null => return Ok(Value::Null),
            Value::String(s) => s,
            _ => return Err(Error::query_execution("REPEAT first argument must be a string")),
        };
        let n = match b {
            Value::Int2(n) => *n as usize,
            Value::Int4(n) => *n as usize,
            Value::Int8(n) => *n as usize,
            _ => return Err(Error::query_execution("REPEAT second argument must be an integer")),
        };
        Ok(Value::String(s.repeat(n)))
    }

    /// REPLACE(string, from, to) - replace all occurrences
    fn func_replace(&self, args: &[Value]) -> Result<Value> {
        let [a, b, c] = args else {
            return Err(Error::query_execution("REPLACE requires exactly 3 arguments"));
        };
        let s = match a {
            Value::Null => return Ok(Value::Null),
            Value::String(s) => s,
            _ => return Err(Error::query_execution("REPLACE first argument must be a string")),
        };
        let from = match b {
            Value::String(s) => s,
            _ => return Err(Error::query_execution("REPLACE second argument must be a string")),
        };
        let to = match c {
            Value::String(s) => s,
            _ => return Err(Error::query_execution("REPLACE third argument must be a string")),
        };
        Ok(Value::String(s.replace(from.as_str(), to.as_str())))
    }

    /// REVERSE(string) - reverse string
    fn func_reverse(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("REVERSE requires exactly one argument"));
        };
        match arg {
            Value::Null => Ok(Value::Null),
            Value::String(s) => Ok(Value::String(s.chars().rev().collect())),
            _ => Err(Error::query_execution("REVERSE requires a string argument")),
        }
    }

    /// POSITION(substring IN string) or STRPOS(string, substring) - find position (1-based)
    fn func_position(&self, args: &[Value]) -> Result<Value> {
        let [a, b] = args else {
            return Err(Error::query_execution("POSITION/STRPOS requires exactly 2 arguments"));
        };
        let (haystack, needle) = match (a, b) {
            (Value::Null, _) | (_, Value::Null) => return Ok(Value::Null),
            (Value::String(h), Value::String(n)) => (h, n),
            _ => return Err(Error::query_execution("POSITION/STRPOS requires string arguments")),
        };
        match haystack.find(needle.as_str()) {
            Some(pos) => Ok(Value::Int4((pos + 1) as i32)), // 1-based
            None => Ok(Value::Int4(0)),
        }
    }

    /// SPLIT_PART(string, delimiter, field) - split string and get nth part (1-based)
    fn func_split_part(&self, args: &[Value]) -> Result<Value> {
        let [str_arg, delim_arg, field_arg] = args else {
            return Err(Error::query_execution("SPLIT_PART requires exactly 3 arguments"));
        };
        let s = match str_arg {
            Value::Null => return Ok(Value::Null),
            Value::String(s) => s,
            _ => return Err(Error::query_execution("SPLIT_PART first argument must be a string")),
        };
        let delim = match delim_arg {
            Value::String(s) => s,
            _ => return Err(Error::query_execution("SPLIT_PART second argument must be a string")),
        };
        let field = match field_arg {
            Value::Int2(n) => *n as usize,
            Value::Int4(n) => *n as usize,
            Value::Int8(n) => *n as usize,
            _ => return Err(Error::query_execution("SPLIT_PART third argument must be an integer")),
        };
        if field == 0 {
            return Err(Error::query_execution("SPLIT_PART field number must be >= 1"));
        }
        let parts: Vec<&str> = s.split(delim.as_str()).collect();
        match parts.get(field - 1) {
            Some(part) => Ok(Value::String(part.to_string())),
            None => Ok(Value::String(String::new())),
        }
    }

    /// INITCAP(string) - capitalize first letter of each word
    fn func_initcap(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("INITCAP requires exactly one argument"));
        };
        match arg {
            Value::Null => Ok(Value::Null),
            Value::String(s) => {
                let mut result = String::with_capacity(s.len());
                let mut capitalize_next = true;
                for c in s.chars() {
                    if c.is_whitespace() || !c.is_alphanumeric() {
                        result.push(c);
                        capitalize_next = true;
                    } else if capitalize_next {
                        result.extend(c.to_uppercase());
                        capitalize_next = false;
                    } else {
                        result.extend(c.to_lowercase());
                    }
                }
                Ok(Value::String(result))
            }
            _ => Err(Error::query_execution("INITCAP requires a string argument")),
        }
    }

    /// LPAD(string, length [, fill]) - left-pad string to length
    fn func_lpad(&self, args: &[Value]) -> Result<Value> {
        if args.len() < 2 || args.len() > 3 {
            return Err(Error::query_execution("LPAD requires 2 or 3 arguments"));
        }
        let arg0 = args.get(0).ok_or_else(|| Error::query_execution("LPAD: missing string"))?;
        let arg1 = args.get(1).ok_or_else(|| Error::query_execution("LPAD: missing length"))?;

        let s = match arg0 {
            Value::Null => return Ok(Value::Null),
            Value::String(s) => s,
            _ => return Err(Error::query_execution("LPAD first argument must be a string")),
        };
        let length = match arg1 {
            Value::Int2(n) => *n as usize,
            Value::Int4(n) => *n as usize,
            Value::Int8(n) => *n as usize,
            _ => return Err(Error::query_execution("LPAD second argument must be an integer")),
        };
        let fill = if let Some(arg2) = args.get(2) {
            match arg2 {
                Value::String(f) => f.clone(),
                _ => " ".to_string(),
            }
        } else {
            " ".to_string()
        };

        let char_count = s.chars().count();
        if char_count >= length {
            return Ok(Value::String(s.chars().take(length).collect()));
        }

        let pad_len = length - char_count;
        let fill_chars: Vec<char> = fill.chars().collect();
        let mut result = String::with_capacity(length);
        if !fill_chars.is_empty() {
            result.extend(fill_chars.iter().cycle().take(pad_len));
        }
        result.push_str(s);
        Ok(Value::String(result))
    }

    /// RPAD(string, length [, fill]) - right-pad string to length
    fn func_rpad(&self, args: &[Value]) -> Result<Value> {
        if args.len() < 2 || args.len() > 3 {
            return Err(Error::query_execution("RPAD requires 2 or 3 arguments"));
        }
        let arg0 = args.get(0).ok_or_else(|| Error::query_execution("RPAD: missing string"))?;
        let arg1 = args.get(1).ok_or_else(|| Error::query_execution("RPAD: missing length"))?;

        let s = match arg0 {
            Value::Null => return Ok(Value::Null),
            Value::String(s) => s,
            _ => return Err(Error::query_execution("RPAD first argument must be a string")),
        };
        let length = match arg1 {
            Value::Int2(n) => *n as usize,
            Value::Int4(n) => *n as usize,
            Value::Int8(n) => *n as usize,
            _ => return Err(Error::query_execution("RPAD second argument must be an integer")),
        };
        let fill = if let Some(arg2) = args.get(2) {
            match arg2 {
                Value::String(f) => f.clone(),
                _ => " ".to_string(),
            }
        } else {
            " ".to_string()
        };

        let char_count = s.chars().count();
        if char_count >= length {
            return Ok(Value::String(s.chars().take(length).collect()));
        }

        let pad_len = length - char_count;
        let fill_chars: Vec<char> = fill.chars().collect();
        let mut result = s.clone();
        if !fill_chars.is_empty() {
            result.extend(fill_chars.iter().cycle().take(pad_len));
        }
        Ok(Value::String(result))
    }

    // ========== Math Functions ==========

    /// Helper to extract a float from a Value
    fn value_to_f64(&self, val: &Value) -> Result<f64> {
        match val {
            Value::Int2(n) => Ok(*n as f64),
            Value::Int4(n) => Ok(*n as f64),
            Value::Int8(n) => Ok(*n as f64),
            Value::Float4(f) => Ok(*f as f64),
            Value::Float8(f) => Ok(*f),
            Value::Numeric(s) => s.parse::<f64>()
                .map_err(|_| Error::query_execution("Invalid numeric value")),
            _ => Err(Error::query_execution("Expected numeric value")),
        }
    }

    /// ABS(x) - absolute value
    fn func_abs(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("ABS requires exactly one argument"));
        };
        match arg {
            Value::Null => Ok(Value::Null),
            Value::Int2(n) => Ok(Value::Int2(n.abs())),
            Value::Int4(n) => Ok(Value::Int4(n.abs())),
            Value::Int8(n) => Ok(Value::Int8(n.abs())),
            Value::Float4(f) => Ok(Value::Float4(f.abs())),
            Value::Float8(f) => Ok(Value::Float8(f.abs())),
            Value::Numeric(s) => {
                if let Ok(d) = s.parse::<Decimal>() {
                    Ok(Value::Numeric(d.abs().to_string()))
                } else {
                    Err(Error::query_execution("Invalid numeric value"))
                }
            }
            _ => Err(Error::query_execution("ABS requires a numeric argument")),
        }
    }

    /// ROUND(x [, precision]) - round to precision decimal places
    fn func_round(&self, args: &[Value]) -> Result<Value> {
        if args.is_empty() || args.len() > 2 {
            return Err(Error::query_execution("ROUND requires 1 or 2 arguments"));
        }
        let first = args.first().ok_or_else(|| Error::query_execution("ROUND requires at least 1 argument"))?;
        if matches!(first, Value::Null) {
            return Ok(Value::Null);
        }

        let precision = if let Some(second) = args.get(1) {
            match second {
                Value::Int2(n) => *n as i32,
                Value::Int4(n) => *n,
                Value::Int8(n) => *n as i32,
                _ => 0,
            }
        } else {
            0
        };

        match first {
            Value::Int2(n) => Ok(Value::Int2(*n)),
            Value::Int4(n) => Ok(Value::Int4(*n)),
            Value::Int8(n) => Ok(Value::Int8(*n)),
            Value::Float4(f) => {
                let factor = 10_f32.powi(precision);
                Ok(Value::Float4((f * factor).round() / factor))
            }
            Value::Float8(f) => {
                let factor = 10_f64.powi(precision);
                Ok(Value::Float8((f * factor).round() / factor))
            }
            Value::Numeric(s) => {
                if let Ok(d) = s.parse::<Decimal>() {
                    Ok(Value::Numeric(d.round_dp(precision as u32).to_string()))
                } else {
                    Err(Error::query_execution("Invalid numeric value"))
                }
            }
            _ => Err(Error::query_execution("ROUND requires a numeric argument")),
        }
    }

    /// CEIL(x) - smallest integer >= x
    fn func_ceil(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("CEIL requires exactly one argument"));
        };
        match arg {
            Value::Null => Ok(Value::Null),
            Value::Int2(n) => Ok(Value::Int2(*n)),
            Value::Int4(n) => Ok(Value::Int4(*n)),
            Value::Int8(n) => Ok(Value::Int8(*n)),
            Value::Float4(f) => Ok(Value::Float8((*f as f64).ceil())),
            Value::Float8(f) => Ok(Value::Float8(f.ceil())),
            Value::Numeric(s) => {
                if let Ok(d) = s.parse::<Decimal>() {
                    Ok(Value::Numeric(d.ceil().to_string()))
                } else {
                    Err(Error::query_execution("Invalid numeric value"))
                }
            }
            _ => Err(Error::query_execution("CEIL requires a numeric argument")),
        }
    }

    /// FLOOR(x) - largest integer <= x
    fn func_floor(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("FLOOR requires exactly one argument"));
        };
        match arg {
            Value::Null => Ok(Value::Null),
            Value::Int2(n) => Ok(Value::Int2(*n)),
            Value::Int4(n) => Ok(Value::Int4(*n)),
            Value::Int8(n) => Ok(Value::Int8(*n)),
            Value::Float4(f) => Ok(Value::Float8((*f as f64).floor())),
            Value::Float8(f) => Ok(Value::Float8(f.floor())),
            Value::Numeric(s) => {
                if let Ok(d) = s.parse::<Decimal>() {
                    Ok(Value::Numeric(d.floor().to_string()))
                } else {
                    Err(Error::query_execution("Invalid numeric value"))
                }
            }
            _ => Err(Error::query_execution("FLOOR requires a numeric argument")),
        }
    }

    /// TRUNC(x [, precision]) - truncate toward zero
    fn func_trunc(&self, args: &[Value]) -> Result<Value> {
        if args.is_empty() || args.len() > 2 {
            return Err(Error::query_execution("TRUNC requires 1 or 2 arguments"));
        }
        let first = args.first().ok_or_else(|| Error::query_execution("TRUNC requires at least 1 argument"))?;
        if matches!(first, Value::Null) {
            return Ok(Value::Null);
        }

        let precision = if let Some(second) = args.get(1) {
            match second {
                Value::Int2(n) => *n as i32,
                Value::Int4(n) => *n,
                Value::Int8(n) => *n as i32,
                _ => 0,
            }
        } else {
            0
        };

        match first {
            Value::Int2(n) => Ok(Value::Int2(*n)),
            Value::Int4(n) => Ok(Value::Int4(*n)),
            Value::Int8(n) => Ok(Value::Int8(*n)),
            Value::Float4(f) => {
                let factor = 10_f32.powi(precision);
                Ok(Value::Float4((f * factor).trunc() / factor))
            }
            Value::Float8(f) => {
                let factor = 10_f64.powi(precision);
                Ok(Value::Float8((f * factor).trunc() / factor))
            }
            Value::Numeric(s) => {
                if let Ok(d) = s.parse::<Decimal>() {
                    Ok(Value::Numeric(d.trunc_with_scale(precision as u32).to_string()))
                } else {
                    Err(Error::query_execution("Invalid numeric value"))
                }
            }
            _ => Err(Error::query_execution("TRUNC requires a numeric argument")),
        }
    }

    /// SQRT(x) - square root
    fn func_sqrt(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("SQRT requires exactly one argument"));
        };
        if matches!(arg, Value::Null) {
            return Ok(Value::Null);
        }
        let x = self.value_to_f64(arg)?;
        if x < 0.0 {
            return Err(Error::query_execution("SQRT of negative number"));
        }
        Ok(Value::Float8(x.sqrt()))
    }

    /// POWER(x, y) - x raised to power y
    fn func_power(&self, args: &[Value]) -> Result<Value> {
        let [a, b] = args else {
            return Err(Error::query_execution("POWER requires exactly 2 arguments"));
        };
        if matches!(a, Value::Null) || matches!(b, Value::Null) {
            return Ok(Value::Null);
        }
        let x = self.value_to_f64(a)?;
        let y = self.value_to_f64(b)?;
        Ok(Value::Float8(x.powf(y)))
    }

    /// MOD(x, y) - modulo (same as x % y)
    fn func_mod(&self, args: &[Value]) -> Result<Value> {
        let [a, b] = args else {
            return Err(Error::query_execution("MOD requires exactly 2 arguments"));
        };
        self.arithmetic_modulo(a, b)
    }

    /// SIGN(x) - returns -1, 0, or 1
    fn func_sign(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("SIGN requires exactly one argument"));
        };
        match arg {
            Value::Null => Ok(Value::Null),
            Value::Int2(n) => Ok(Value::Int4(n.signum() as i32)),
            Value::Int4(n) => Ok(Value::Int4(n.signum())),
            Value::Int8(n) => Ok(Value::Int4(n.signum() as i32)),
            Value::Float4(f) => {
                if f.is_nan() { Ok(Value::Float8(f64::NAN)) }
                else if *f > 0.0 { Ok(Value::Int4(1)) }
                else if *f < 0.0 { Ok(Value::Int4(-1)) }
                else { Ok(Value::Int4(0)) }
            }
            Value::Float8(f) => {
                if f.is_nan() { Ok(Value::Float8(f64::NAN)) }
                else if *f > 0.0 { Ok(Value::Int4(1)) }
                else if *f < 0.0 { Ok(Value::Int4(-1)) }
                else { Ok(Value::Int4(0)) }
            }
            _ => Err(Error::query_execution("SIGN requires a numeric argument")),
        }
    }

    /// GREATEST(val1, val2, ...) - returns the largest value
    fn func_greatest(&self, args: &[Value]) -> Result<Value> {
        let (first, rest) = args.split_first().ok_or_else(|| {
            Error::query_execution("GREATEST requires at least one argument")
        })?;
        let mut result = first;
        for arg in rest {
            if matches!(arg, Value::Null) {
                continue;
            }
            if matches!(result, Value::Null) {
                result = arg;
                continue;
            }
            if self.compare_values_internal(arg, result)?.is_gt() {
                result = arg;
            }
        }
        Ok(result.clone())
    }

    /// LEAST(val1, val2, ...) - returns the smallest value
    fn func_least(&self, args: &[Value]) -> Result<Value> {
        let (first, rest) = args.split_first().ok_or_else(|| {
            Error::query_execution("LEAST requires at least one argument")
        })?;
        let mut result = first;
        for arg in rest {
            if matches!(arg, Value::Null) {
                continue;
            }
            if matches!(result, Value::Null) {
                result = arg;
                continue;
            }
            if self.compare_values_internal(arg, result)?.is_lt() {
                result = arg;
            }
        }
        Ok(result.clone())
    }

    /// Helper for comparing values (returns Ordering)
    fn compare_values_internal(&self, left: &Value, right: &Value) -> Result<std::cmp::Ordering> {
        use std::cmp::Ordering;
        match (left, right) {
            (Value::Int2(a), Value::Int2(b)) => Ok(a.cmp(b)),
            (Value::Int4(a), Value::Int4(b)) => Ok(a.cmp(b)),
            (Value::Int8(a), Value::Int8(b)) => Ok(a.cmp(b)),
            (Value::Float4(a), Value::Float4(b)) => Ok(a.partial_cmp(b).unwrap_or(Ordering::Equal)),
            (Value::Float8(a), Value::Float8(b)) => Ok(a.partial_cmp(b).unwrap_or(Ordering::Equal)),
            (Value::String(a), Value::String(b)) => Ok(a.cmp(b)),
            (Value::Numeric(a), Value::Numeric(b)) => {
                match (a.parse::<Decimal>(), b.parse::<Decimal>()) {
                    (Ok(a_dec), Ok(b_dec)) => Ok(a_dec.cmp(&b_dec)),
                    _ => Err(Error::query_execution(format!(
                        "Cannot compare invalid NUMERIC values '{}' and '{}'", a, b
                    ))),
                }
            }

            // Cross-type integer comparisons
            (Value::Int2(a), Value::Int4(b)) => Ok((*a as i32).cmp(b)),
            (Value::Int4(a), Value::Int2(b)) => Ok(a.cmp(&(*b as i32))),
            (Value::Int2(a), Value::Int8(b)) => Ok((*a as i64).cmp(b)),
            (Value::Int8(a), Value::Int2(b)) => Ok(a.cmp(&(*b as i64))),
            (Value::Int4(a), Value::Int8(b)) => Ok((*a as i64).cmp(b)),
            (Value::Int8(a), Value::Int4(b)) => Ok(a.cmp(&(*b as i64))),

            // Int to Float comparisons
            (Value::Int4(a), Value::Float8(b)) => Ok((*a as f64).partial_cmp(b).unwrap_or(Ordering::Equal)),
            (Value::Float8(a), Value::Int4(b)) => Ok(a.partial_cmp(&(*b as f64)).unwrap_or(Ordering::Equal)),
            (Value::Int8(a), Value::Float8(b)) => Ok((*a as f64).partial_cmp(b).unwrap_or(Ordering::Equal)),
            (Value::Float8(a), Value::Int8(b)) => Ok(a.partial_cmp(&(*b as f64)).unwrap_or(Ordering::Equal)),
            (Value::Float4(a), Value::Float8(b)) => Ok((*a as f64).partial_cmp(b).unwrap_or(Ordering::Equal)),
            (Value::Float8(a), Value::Float4(b)) => Ok(a.partial_cmp(&(*b as f64)).unwrap_or(Ordering::Equal)),

            // Numeric (DECIMAL) cross-type comparisons — Numeric stores decimal as String
            (Value::Numeric(a), Value::Float8(b)) => {
                let af = a.parse::<f64>().map_err(|_| Error::query_execution(format!(
                    "Invalid NUMERIC value '{}' in comparison", a
                )))?;
                Ok(af.partial_cmp(b).unwrap_or(Ordering::Equal))
            }
            (Value::Float8(a), Value::Numeric(b)) => {
                let bf = b.parse::<f64>().map_err(|_| Error::query_execution(format!(
                    "Invalid NUMERIC value '{}' in comparison", b
                )))?;
                Ok(a.partial_cmp(&bf).unwrap_or(Ordering::Equal))
            }
            (Value::Numeric(a), Value::Float4(b)) => {
                let af = a.parse::<f64>().map_err(|_| Error::query_execution(format!(
                    "Invalid NUMERIC value '{}' in comparison", a
                )))?;
                Ok(af.partial_cmp(&f64::from(*b)).unwrap_or(Ordering::Equal))
            }
            (Value::Float4(a), Value::Numeric(b)) => {
                let bf = b.parse::<f64>().map_err(|_| Error::query_execution(format!(
                    "Invalid NUMERIC value '{}' in comparison", b
                )))?;
                Ok(f64::from(*a).partial_cmp(&bf).unwrap_or(Ordering::Equal))
            }
            (Value::Numeric(a), Value::Int4(b)) => {
                let ad = a.parse::<Decimal>().map_err(|_| Error::query_execution(format!(
                    "Invalid NUMERIC value '{}' in comparison", a
                )))?;
                Ok(ad.cmp(&Decimal::from(*b)))
            }
            (Value::Int4(a), Value::Numeric(b)) => {
                let bd = b.parse::<Decimal>().map_err(|_| Error::query_execution(format!(
                    "Invalid NUMERIC value '{}' in comparison", b
                )))?;
                Ok(Decimal::from(*a).cmp(&bd))
            }
            (Value::Numeric(a), Value::Int8(b)) => {
                let ad = a.parse::<Decimal>().map_err(|_| Error::query_execution(format!(
                    "Invalid NUMERIC value '{}' in comparison", a
                )))?;
                Ok(ad.cmp(&Decimal::from(*b)))
            }
            (Value::Int8(a), Value::Numeric(b)) => {
                let bd = b.parse::<Decimal>().map_err(|_| Error::query_execution(format!(
                    "Invalid NUMERIC value '{}' in comparison", b
                )))?;
                Ok(Decimal::from(*a).cmp(&bd))
            }
            (Value::Numeric(a), Value::Int2(b)) => {
                let ad = a.parse::<Decimal>().map_err(|_| Error::query_execution(format!(
                    "Invalid NUMERIC value '{}' in comparison", a
                )))?;
                Ok(ad.cmp(&Decimal::from(*b)))
            }
            (Value::Int2(a), Value::Numeric(b)) => {
                let bd = b.parse::<Decimal>().map_err(|_| Error::query_execution(format!(
                    "Invalid NUMERIC value '{}' in comparison", b
                )))?;
                Ok(Decimal::from(*a).cmp(&bd))
            }

            _ => Ok(Ordering::Equal),
        }
    }

    /// RANDOM() - returns random value between 0 and 1
    fn func_random(&self, args: &[Value]) -> Result<Value> {
        if !args.is_empty() {
            return Err(Error::query_execution("RANDOM takes no arguments"));
        }
        use std::time::{SystemTime, UNIX_EPOCH};
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        // Simple LCG random number generator
        let random = ((seed.wrapping_mul(1103515245).wrapping_add(12345)) as f64) / (u64::MAX as f64);
        Ok(Value::Float8(random.abs()))
    }

    /// EXP(x) - e raised to power x
    fn func_exp(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("EXP requires exactly one argument"));
        };
        if matches!(arg, Value::Null) {
            return Ok(Value::Null);
        }
        let x = self.value_to_f64(arg)?;
        Ok(Value::Float8(x.exp()))
    }

    /// LN(x) or LOG(x) - natural logarithm
    fn func_ln(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("LN requires exactly one argument"));
        };
        if matches!(arg, Value::Null) {
            return Ok(Value::Null);
        }
        let x = self.value_to_f64(arg)?;
        if x <= 0.0 {
            return Err(Error::query_execution("LN of non-positive number"));
        }
        Ok(Value::Float8(x.ln()))
    }

    /// LOG10(x) - base-10 logarithm
    fn func_log10(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("LOG10 requires exactly one argument"));
        };
        if matches!(arg, Value::Null) {
            return Ok(Value::Null);
        }
        let x = self.value_to_f64(arg)?;
        if x <= 0.0 {
            return Err(Error::query_execution("LOG10 of non-positive number"));
        }
        Ok(Value::Float8(x.log10()))
    }

    /// SIN(x) - sine (x in radians)
    fn func_sin(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("SIN requires exactly one argument"));
        };
        if matches!(arg, Value::Null) {
            return Ok(Value::Null);
        }
        let x = self.value_to_f64(arg)?;
        Ok(Value::Float8(x.sin()))
    }

    /// COS(x) - cosine (x in radians)
    fn func_cos(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("COS requires exactly one argument"));
        };
        if matches!(arg, Value::Null) {
            return Ok(Value::Null);
        }
        let x = self.value_to_f64(arg)?;
        Ok(Value::Float8(x.cos()))
    }

    /// TAN(x) - tangent (x in radians)
    fn func_tan(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("TAN requires exactly one argument"));
        };
        if matches!(arg, Value::Null) {
            return Ok(Value::Null);
        }
        let x = self.value_to_f64(arg)?;
        Ok(Value::Float8(x.tan()))
    }

    /// ASIN(x) - arcsine (result in radians)
    fn func_asin(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("ASIN requires exactly one argument"));
        };
        if matches!(arg, Value::Null) {
            return Ok(Value::Null);
        }
        let x = self.value_to_f64(arg)?;
        if x < -1.0 || x > 1.0 {
            return Err(Error::query_execution("ASIN argument out of range [-1, 1]"));
        }
        Ok(Value::Float8(x.asin()))
    }

    /// ACOS(x) - arccosine (result in radians)
    fn func_acos(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("ACOS requires exactly one argument"));
        };
        if matches!(arg, Value::Null) {
            return Ok(Value::Null);
        }
        let x = self.value_to_f64(arg)?;
        if x < -1.0 || x > 1.0 {
            return Err(Error::query_execution("ACOS argument out of range [-1, 1]"));
        }
        Ok(Value::Float8(x.acos()))
    }

    /// ATAN(x) - arctangent (result in radians)
    fn func_atan(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("ATAN requires exactly one argument"));
        };
        if matches!(arg, Value::Null) {
            return Ok(Value::Null);
        }
        let x = self.value_to_f64(arg)?;
        Ok(Value::Float8(x.atan()))
    }

    /// ATAN2(y, x) - arctangent of y/x
    fn func_atan2(&self, args: &[Value]) -> Result<Value> {
        let [a, b] = args else {
            return Err(Error::query_execution("ATAN2 requires exactly 2 arguments"));
        };
        if matches!(a, Value::Null) || matches!(b, Value::Null) {
            return Ok(Value::Null);
        }
        let y = self.value_to_f64(a)?;
        let x = self.value_to_f64(b)?;
        Ok(Value::Float8(y.atan2(x)))
    }

    /// DEGREES(x) - convert radians to degrees
    fn func_degrees(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("DEGREES requires exactly one argument"));
        };
        if matches!(arg, Value::Null) {
            return Ok(Value::Null);
        }
        let x = self.value_to_f64(arg)?;
        Ok(Value::Float8(x.to_degrees()))
    }

    /// RADIANS(x) - convert degrees to radians
    fn func_radians(&self, args: &[Value]) -> Result<Value> {
        let [arg] = args else {
            return Err(Error::query_execution("RADIANS requires exactly one argument"));
        };
        if matches!(arg, Value::Null) {
            return Ok(Value::Null);
        }
        let x = self.value_to_f64(arg)?;
        Ok(Value::Float8(x.to_radians()))
    }
}

/// Standalone recursive helper for JSON path setting (avoids clippy::only_used_in_recursion)
fn jsonb_set_recursive_impl(
    current: &mut serde_json::Value,
    path: &[String],
    index: usize,
    value: &serde_json::Value,
    create_missing: bool,
) -> Result<()> {
    let key = match path.get(index) {
        Some(k) => k,
        None => return Ok(()),
    };
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
                if let Some(elem) = arr.get_mut(arr_idx) {
                    *elem = value.clone();
                }
            } else {
                if arr.get(arr_idx).is_some_and(|v| v.is_null()) && create_missing {
                    if let Some(elem) = arr.get_mut(arr_idx) {
                        *elem = serde_json::json!({});
                    }
                }
                if let Some(elem) = arr.get_mut(arr_idx) {
                    jsonb_set_recursive_impl(elem, path, index + 1, value, create_missing)?;
                }
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
                    jsonb_set_recursive_impl(next, path, index + 1, value, create_missing)?;
                }
            }
        }
    }

    Ok(())
}

/// Standalone recursive helper for JSON path deletion (avoids clippy::only_used_in_recursion)
fn jsonb_delete_recursive_impl(
    current: &mut serde_json::Value,
    path: &[String],
    index: usize,
) -> Result<()> {
    let key = match path.get(index) {
        Some(k) => k,
        None => return Ok(()),
    };
    let is_last = index == path.len() - 1;

    if let Ok(arr_idx) = key.parse::<usize>() {
        // Array index
        if let Some(arr) = current.as_array_mut() {
            if is_last {
                if arr_idx < arr.len() {
                    arr.remove(arr_idx);
                }
            } else if let Some(elem) = arr.get_mut(arr_idx) {
                jsonb_delete_recursive_impl(elem, path, index + 1)?;
            }
        }
    } else {
        // Object key
        if let Some(obj) = current.as_object_mut() {
            if is_last {
                obj.remove(key);
            } else if let Some(next) = obj.get_mut(key) {
                jsonb_delete_recursive_impl(next, path, index + 1)?;
            }
        }
    }

    Ok(())
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
                left_obj.get(key).is_some_and(|left_val| json_contains(left_val, right_val))
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

    #[test]
    fn test_uuid_comparison_with_string() {
        let uuid_val = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();

        // UUID schema with uuid PK column
        let schema = Arc::new(Schema::new(vec![
            Column {
                name: "id".to_string(),
                data_type: DataType::Uuid,
                nullable: false,
                primary_key: true,
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
        ]));

        let evaluator = Evaluator::new(schema);
        let tuple = Tuple::new(vec![
            Value::Uuid(uuid_val),
            Value::String("Alice".to_string()),
        ]);

        // UUID column = UUID string literal (the common case: WHERE id = '550e...')
        let expr = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column { table: None, name: "id".to_string() }),
            op: BinaryOperator::Eq,
            right: Box::new(LogicalExpr::Literal(Value::String(
                "550e8400-e29b-41d4-a716-446655440000".to_string(),
            ))),
        };
        let result = evaluator.evaluate(&expr, &tuple)
            .expect("UUID = String comparison should work");
        assert_eq!(result, Value::Boolean(true));

        // Non-matching UUID
        let expr_neq = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column { table: None, name: "id".to_string() }),
            op: BinaryOperator::Eq,
            right: Box::new(LogicalExpr::Literal(Value::String(
                "00000000-0000-0000-0000-000000000000".to_string(),
            ))),
        };
        let result_neq = evaluator.evaluate(&expr_neq, &tuple)
            .expect("UUID = String comparison should work");
        assert_eq!(result_neq, Value::Boolean(false));

        // UUID = UUID direct comparison
        let expr_uuid = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column { table: None, name: "id".to_string() }),
            op: BinaryOperator::Eq,
            right: Box::new(LogicalExpr::Literal(Value::Uuid(uuid_val))),
        };
        let result_uuid = evaluator.evaluate(&expr_uuid, &tuple)
            .expect("UUID = UUID comparison should work");
        assert_eq!(result_uuid, Value::Boolean(true));
    }

    #[test]
    fn test_uuid_cast() {
        let schema = test_schema();
        let evaluator = Evaluator::new(schema);
        let uuid_str = "550e8400-e29b-41d4-a716-446655440000";

        // String to UUID cast
        let result = evaluator.cast_value(
            Value::String(uuid_str.to_string()),
            &DataType::Uuid,
        ).expect("String to UUID cast should work");
        assert!(matches!(result, Value::Uuid(_)));

        // UUID to UUID cast (identity)
        let uuid_val = uuid::Uuid::parse_str(uuid_str).unwrap();
        let result2 = evaluator.cast_value(
            Value::Uuid(uuid_val),
            &DataType::Uuid,
        ).expect("UUID to UUID cast should work");
        assert_eq!(result2, Value::Uuid(uuid_val));

        // Invalid UUID string
        let result3 = evaluator.cast_value(
            Value::String("not-a-uuid".to_string()),
            &DataType::Uuid,
        );
        assert!(result3.is_err());
    }
}
