//! Spec R5.2: is a column `DEFAULT` stable, and its one evaluation rule.
//!
//! `ALTER TABLE … ADD COLUMN … DEFAULT <expr>` evaluates a STABLE expression
//! once and stores the answer as the column's missing value; a VOLATILE one is
//! evaluated per row, which takes a rewrite of the table. The codebase has no
//! per-function volatility catalog (`EmbeddedDatabase::query_is_non_deterministic`
//! matches SQL text, for the result cache), so the rule is here, fail-safe:
//! ONLY what [`default_is_stable`] recognizes is stable, and anything else — a
//! built-in not listed, a function nothing knows, a column, a subquery — is
//! volatile. The cost of a wrong "volatile" is a rewrite; the cost of a wrong
//! "stable" is every row holding one value that should have been many.
//!
//! The list follows PostgreSQL: a function of its arguments alone, and the
//! `STABLE` clock and session functions (`now()`, `current_date`,
//! `current_user`), which PostgreSQL also evaluates once for `ADD COLUMN`.
//! `random`, `gen_random_uuid`, `uuid_generate_v4`, the sequence functions, the
//! advisory locks and the model calls (`predict`, `infer`, `generate`,
//! `code_embed`) are absent on purpose, and so is what reads per-session state a
//! later session would see differently (`current_tenant`, `current_setting`).
//! A user-defined function is stable only when it was declared `IMMUTABLE` or
//! `STABLE`.

use super::logical_plan::LogicalExpr;
use super::Evaluator;
use crate::{DataType, Result, Schema, Tuple};
use std::sync::Arc;

/// Whether `expr` may be evaluated once and stand for every existing row.
pub fn default_is_stable(expr: &LogicalExpr) -> bool {
    match expr {
        LogicalExpr::Literal(_) => true,
        LogicalExpr::BinaryExpr { left, right, .. } => default_is_stable(left) && default_is_stable(right),
        LogicalExpr::UnaryExpr { expr, .. } | LogicalExpr::Cast { expr, .. } | LogicalExpr::IsNull { expr, .. } => {
            default_is_stable(expr)
        }
        LogicalExpr::ScalarFunction { fun, args } => function_is_stable(fun) && args.iter().all(default_is_stable),
        LogicalExpr::Case {
            expr,
            when_then,
            else_result,
        } => {
            expr.as_deref().is_none_or(default_is_stable)
                && when_then
                    .iter()
                    .all(|(when, then)| default_is_stable(when) && default_is_stable(then))
                && else_result.as_deref().is_none_or(default_is_stable)
        }
        LogicalExpr::Between { expr, low, high, .. } => {
            default_is_stable(expr) && default_is_stable(low) && default_is_stable(high)
        }
        LogicalExpr::InList { expr, list, .. } => default_is_stable(expr) && list.iter().all(default_is_stable),
        LogicalExpr::ArraySubscript { array, index } => default_is_stable(array) && default_is_stable(index),
        LogicalExpr::Tuple { items } => items.iter().all(default_is_stable),
        // Columns, parameters, subqueries, aggregates, window functions, trigger
        // context: nothing a default can be evaluated once for.
        _ => false,
    }
}

/// `fun` as the evaluator dispatches it: lowercase, no `pg_catalog.` or
/// `public.` qualifier.
fn function_is_stable(fun: &str) -> bool {
    let lowered = fun.to_ascii_lowercase();
    let name = lowered.strip_prefix("pg_catalog.").unwrap_or(&lowered);
    if is_stable_builtin(name) {
        return true;
    }
    let base = name.strip_prefix("public.").unwrap_or(name);
    if base.contains('.') {
        return false;
    }
    super::udf_bridge::resolve(base)
        .and_then(|bridge| bridge.registry.get_function(base))
        .and_then(|function| function.volatility)
        .is_some_and(|volatility| {
            volatility.eq_ignore_ascii_case("IMMUTABLE") || volatility.eq_ignore_ascii_case("STABLE")
        })
}

fn is_stable_builtin(name: &str) -> bool {
    matches!(
        name,
        // The clock, as PostgreSQL's STABLE functions: the statement's time.
        "current_timestamp" | "now" | "sysdate" | "getdate" | "systimestamp" | "sysdatetime" | "getutcdate"
            | "utc_timestamp" | "current_date" | "curdate" | "current_time" | "curtime" | "localtimestamp"
            | "localtime" | "unix_timestamp"
            // Session identity, as PostgreSQL's STABLE functions.
            | "current_user" | "session_user" | "current_role" | "user" | "current_schema" | "current_database"
            | "version"
            // NULL handling, comparison.
            | "coalesce" | "nullif" | "ifnull" | "greatest" | "least"
            // Text.
            | "upper" | "lower" | "length" | "char_length" | "character_length" | "substr" | "substring" | "trim"
            | "ltrim" | "rtrim" | "btrim" | "concat" | "concat_ws" | "left" | "right" | "repeat" | "replace"
            | "reverse" | "position" | "split_part" | "initcap" | "lpad" | "rpad" | "locate" | "instr"
            // Numbers.
            | "abs" | "round" | "ceil" | "ceiling" | "floor" | "trunc" | "sqrt" | "power" | "pow" | "mod" | "sign"
            | "pi" | "exp" | "ln" | "log" | "log10" | "sin" | "cos" | "tan" | "asin" | "acos" | "atan" | "atan2"
            | "degrees" | "radians"
            // Dates, of their arguments.
            | "date_format" | "date" | "year" | "month" | "day" | "date_add" | "date_sub" | "datediff"
            | "timestampdiff" | "from_unixtime" | "to_char" | "to_date" | "to_timestamp" | "date_trunc" | "make_date"
            | "make_timestamp" | "age" | "date_part"
            // Arrays and JSON, of their arguments.
            | "array_length" | "array_upper" | "array_lower" | "array_append" | "array_prepend" | "array_cat"
            | "array_remove" | "array_position" | "cardinality"
            | "jsonb_build_object" | "json_build_object" | "jsonb_build_array" | "json_build_array" | "jsonb_set"
            | "json_set" | "jsonb_concat" | "jsonb_strip_nulls" | "jsonb_pretty" | "jsonb_typeof"
            | "jsonb_extract_path" | "json_extract_path" | "jsonb_extract_path_text" | "json_extract_path_text"
            | "to_tsvector" | "to_tsquery"
    )
}

/// One evaluation of a column's default, and the cast to the column's type: the
/// INSERT gate's rule (`EmbeddedDatabase::build_insert_select_row`), which is
/// this function, so an `ADD COLUMN` and a later INSERT cannot disagree about
/// what a default yields.
pub fn evaluate_column_default(expr: &LogicalExpr, data_type: &DataType) -> Result<crate::Value> {
    let evaluator = Evaluator::new(Arc::new(Schema { columns: vec![] }));
    evaluate_column_default_with(&evaluator, &Tuple::new(vec![]), expr, data_type)
}

/// [`evaluate_column_default`] with the evaluator a caller already holds.
pub fn evaluate_column_default_with(
    evaluator: &Evaluator,
    empty_tuple: &Tuple,
    expr: &LogicalExpr,
    data_type: &DataType,
) -> Result<crate::Value> {
    let mut value = evaluator.evaluate(expr, empty_tuple)?;
    if value.data_type() != *data_type {
        value = evaluator.cast_value(value, data_type)?;
    }
    Ok(value)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::Value;

    fn function(name: &str, args: Vec<LogicalExpr>) -> LogicalExpr {
        LogicalExpr::ScalarFunction {
            fun: name.to_string(),
            args,
        }
    }

    #[test]
    fn literals_and_stable_functions_are_stable() {
        assert!(default_is_stable(&LogicalExpr::Literal(Value::Int4(5))));
        assert!(default_is_stable(&function("now", vec![])));
        assert!(default_is_stable(&function("NOW", vec![])));
        assert!(default_is_stable(&function(
            "pg_catalog.upper",
            vec![LogicalExpr::Literal(Value::String("x".to_string()))]
        )));
    }

    #[test]
    fn volatile_and_unknown_functions_are_volatile() {
        for name in [
            "random",
            "gen_random_uuid",
            "uuid_generate_v4",
            "nextval",
            "setval",
            "clock_timestamp",
            "timeofday",
            "predict",
            "current_setting",
            "a_function_nobody_has_heard_of",
        ] {
            assert!(!default_is_stable(&function(name, vec![])), "{name}");
        }
    }

    #[test]
    fn one_volatile_argument_makes_the_expression_volatile() {
        assert!(!default_is_stable(&function("abs", vec![function("random", vec![])])));
        assert!(!default_is_stable(&LogicalExpr::Column {
            table: None,
            name: "a".to_string(),
        }));
    }

    #[test]
    fn the_evaluation_casts_to_the_column_type() {
        let value = evaluate_column_default(&LogicalExpr::Literal(Value::Int4(5)), &DataType::Int8).unwrap();
        assert_eq!(value, Value::Int8(5));
        let null = evaluate_column_default(&LogicalExpr::Literal(Value::Null), &DataType::Int8).unwrap();
        assert_eq!(null, Value::Null);
    }
}
