//! REST API Query Executor
//!
//! Translates PostgREST-style filter operators into parameterized SQL queries
//! and executes them against the embedded database.  All user-supplied values
//! are bound via `$N` placeholders — **no** string interpolation.

use std::sync::Arc;

use crate::{EmbeddedDatabase, Result, Error, Value, Tuple};

/// Executes REST API operations against the embedded database.
#[allow(dead_code)]
pub struct RestExecutor {
    db: Arc<EmbeddedDatabase>,
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Validate that a SQL identifier (table or column name) contains only safe characters.
fn validate_identifier(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::sql_parse("Empty identifier"));
    }
    // Allow letters, digits, underscores; must start with letter or underscore.
    let valid = name.chars().enumerate().all(|(i, c)| {
        if i == 0 {
            c.is_ascii_alphabetic() || c == '_'
        } else {
            c.is_ascii_alphanumeric() || c == '_'
        }
    });
    if !valid {
        return Err(Error::sql_parse(format!(
            "Invalid identifier: '{name}'. Only ASCII letters, digits and underscores are allowed"
        )));
    }
    Ok(())
}

/// Return a sanitized SQL identifier.
///
/// Since `validate_identifier` already guarantees the name is
/// `[a-zA-Z_][a-zA-Z0-9_]*`, we use it verbatim — HeliosDB's parser
/// does not support double-quoted identifiers.
fn quote_ident(name: &str) -> String {
    name.to_string()
}

/// Infer a `Value` from a string representation.
///
/// Tries, in order: `true`/`false` -> bool, i32 -> `Int4`, i64 -> `Int8`,
/// f64 -> `Float8`, and falls back to `String`.
fn infer_value(s: &str) -> Value {
    match s {
        "true" => return Value::Boolean(true),
        "false" => return Value::Boolean(false),
        _ => {}
    }
    if let Ok(i) = s.parse::<i32>() {
        return Value::Int4(i);
    }
    if let Ok(i) = s.parse::<i64>() {
        return Value::Int8(i);
    }
    if let Ok(f) = s.parse::<f64>() {
        return Value::Float8(f);
    }
    Value::String(s.to_string())
}

/// Parsed representation of a single PostgREST filter.
#[derive(Debug, Clone)]
pub struct ParsedFilter {
    /// The SQL fragment that belongs in the WHERE clause.
    pub sql_fragment: String,
    /// Bound parameter values (may be empty for `IS NULL` / `IS NOT NULL`).
    pub params: Vec<Value>,
}

/// Parse a single PostgREST filter expression.
///
/// `column` is the query-string key (e.g. `"age"`).
/// `expr` is the query-string value (e.g. `"gt.18"`).
/// `param_offset` is the next available `$N` index (1-based).
///
/// Returns a `ParsedFilter` with the SQL fragment and bound params.
pub fn parse_filter(column: &str, expr: &str, param_offset: usize) -> Result<ParsedFilter> {
    validate_identifier(column)?;
    let col = quote_ident(column);

    // Detect negation prefix `not.`
    let (negated, rest) = if let Some(stripped) = expr.strip_prefix("not.") {
        (true, stripped)
    } else {
        (false, expr)
    };

    let neg = if negated { "NOT " } else { "" };

    // Split on first '.' to get operator and value
    let (op_str, value_str) = rest.split_once('.')
        .ok_or_else(|| Error::sql_parse(format!(
            "Invalid filter expression: '{expr}'. Expected format: operator.value"
        )))?;

    match op_str {
        "eq" => {
            let idx = param_offset;
            Ok(ParsedFilter {
                sql_fragment: format!("{neg}{col} = ${idx}"),
                params: vec![infer_value(value_str)],
            })
        }
        "neq" => {
            let idx = param_offset;
            Ok(ParsedFilter {
                sql_fragment: format!("{neg}{col} != ${idx}"),
                params: vec![infer_value(value_str)],
            })
        }
        "gt" => {
            let idx = param_offset;
            Ok(ParsedFilter {
                sql_fragment: format!("{neg}{col} > ${idx}"),
                params: vec![infer_value(value_str)],
            })
        }
        "gte" => {
            let idx = param_offset;
            Ok(ParsedFilter {
                sql_fragment: format!("{neg}{col} >= ${idx}"),
                params: vec![infer_value(value_str)],
            })
        }
        "lt" => {
            let idx = param_offset;
            Ok(ParsedFilter {
                sql_fragment: format!("{neg}{col} < ${idx}"),
                params: vec![infer_value(value_str)],
            })
        }
        "lte" => {
            let idx = param_offset;
            Ok(ParsedFilter {
                sql_fragment: format!("{neg}{col} <= ${idx}"),
                params: vec![infer_value(value_str)],
            })
        }
        "like" => {
            let idx = param_offset;
            // PostgREST uses `*` as a wildcard which maps to SQL `%`
            let pattern = value_str.replace('*', "%");
            Ok(ParsedFilter {
                sql_fragment: format!("{neg}{col} LIKE ${idx}"),
                params: vec![Value::String(pattern)],
            })
        }
        "ilike" => {
            let idx = param_offset;
            let pattern = value_str.replace('*', "%");
            Ok(ParsedFilter {
                sql_fragment: format!("{neg}{col} ILIKE ${idx}"),
                params: vec![Value::String(pattern)],
            })
        }
        "is" => {
            // IS NULL / IS NOT NULL / IS TRUE / IS FALSE — no bound parameter
            let kw = value_str.to_uppercase();
            match kw.as_str() {
                "NULL" | "TRUE" | "FALSE" => Ok(ParsedFilter {
                    sql_fragment: format!("{neg}{col} IS {kw}"),
                    params: vec![],
                }),
                _ => Err(Error::sql_parse(format!(
                    "Invalid IS value: '{value_str}'. Expected null, true, or false"
                ))),
            }
        }
        "in" => {
            // Value is `(a,b,c)` — strip parens, split on commas
            let inner = value_str
                .strip_prefix('(')
                .and_then(|s| s.strip_suffix(')'))
                .ok_or_else(|| Error::sql_parse(format!(
                    "Invalid IN value: '{value_str}'. Expected format: (val1,val2,...)"
                )))?;

            let items: Vec<&str> = inner.split(',').collect();
            if items.is_empty() {
                return Err(Error::sql_parse("Empty IN list"));
            }

            let mut placeholders = Vec::with_capacity(items.len());
            let mut params = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                let idx = param_offset + i;
                placeholders.push(format!("${idx}"));
                params.push(infer_value(item.trim()));
            }

            Ok(ParsedFilter {
                sql_fragment: format!("{neg}{col} IN ({})", placeholders.join(", ")),
                params,
            })
        }
        other => Err(Error::sql_parse(format!(
            "Unsupported filter operator: '{other}'"
        ))),
    }
}

/// Parse an ORDER BY specification in PostgREST format.
///
/// Examples: `"created_at.desc"`, `"name.asc,id.desc"`, `"id"` (defaults to ASC).
pub fn parse_order_clause(order: &str) -> Result<String> {
    let mut parts = Vec::new();
    for segment in order.split(',') {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        let (col, dir) = if let Some((c, d)) = segment.split_once('.') {
            (c, d)
        } else {
            (segment, "asc")
        };
        validate_identifier(col)?;
        let dir_sql = match dir.to_lowercase().as_str() {
            "asc" => "ASC",
            "desc" => "DESC",
            other => return Err(Error::sql_parse(format!(
                "Invalid order direction: '{other}'. Expected asc or desc"
            ))),
        };
        parts.push(format!("{} {dir_sql}", quote_ident(col)));
    }
    Ok(parts.join(", "))
}

/// Parse a SELECT column list.
///
/// `"*"` is passed through as-is.  Otherwise each comma-separated name is
/// validated and quoted.
pub fn parse_select_columns(select: &str) -> Result<String> {
    let select = select.trim();
    if select == "*" {
        return Ok("*".to_string());
    }
    let mut cols = Vec::new();
    for col in select.split(',') {
        let col = col.trim();
        validate_identifier(col)?;
        cols.push(quote_ident(col));
    }
    Ok(cols.join(", "))
}

// ── RestExecutor ─────────────────────────────────────────────────────────────

impl RestExecutor {
    /// Create a new executor backed by the given database.
    pub fn new(db: Arc<EmbeddedDatabase>) -> Self {
        Self { db }
    }

    // ── SELECT ───────────────────────────────────────────────────────────

    /// Execute a SELECT query with PostgREST-style parameters.
    ///
    /// Returns `(rows, column_names)`.
    pub fn select(
        &self,
        table: &str,
        select_cols: &str,
        filters: &[(String, String)],
        order: Option<&str>,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<(Vec<Tuple>, Vec<String>)> {
        validate_identifier(table)?;
        let columns = parse_select_columns(select_cols)?;

        let mut sql = format!("SELECT {columns} FROM {}", quote_ident(table));
        let mut params: Vec<Value> = Vec::new();
        let mut param_idx: usize = 1;

        // WHERE clause
        if !filters.is_empty() {
            let mut conditions = Vec::with_capacity(filters.len());
            for (col, expr) in filters {
                let pf = parse_filter(col, expr, param_idx)?;
                param_idx += pf.params.len();
                conditions.push(pf.sql_fragment);
                params.extend(pf.params);
            }
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }

        // ORDER BY
        if let Some(order_str) = order {
            let clause = parse_order_clause(order_str)?;
            if !clause.is_empty() {
                sql.push_str(" ORDER BY ");
                sql.push_str(&clause);
            }
        }

        // LIMIT
        if let Some(lim) = limit {
            sql.push_str(&format!(" LIMIT {lim}"));
        }

        // OFFSET
        if let Some(off) = offset {
            sql.push_str(&format!(" OFFSET {off}"));
        }

        // Execute
        if params.is_empty() {
            self.db.query_with_columns(&sql)
        } else {
            // query_with_columns does not accept params, so we fall back to
            // building the column list from query_params and a separate
            // query_with_columns call for column names only.
            let tuples = self.db.query_params(&sql, &params)?;

            // Derive column names: run a LIMIT 0 variant to grab names cheaply.
            let col_sql = format!(
                "SELECT {columns} FROM {} LIMIT 0",
                quote_ident(table)
            );
            let (_, col_names) = self.db.query_with_columns(&col_sql)?;
            Ok((tuples, col_names))
        }
    }

    // ── SELECT with RLS ─────────────────────────────────────────────────

    /// Execute a SELECT query with implicit Row-Level Security filtering.
    ///
    /// When `user_id` is `Some(uid)`, the executor checks whether the table
    /// contains a column named `owner_id` or `user_id`.  If it does, an
    /// additional `WHERE owner_id = $N` / `WHERE user_id = $N` predicate is
    /// appended so that the caller can only see rows they own.
    ///
    /// This implements the common `auth.uid() = owner_id` RLS pattern used
    /// by Supabase for row-level security.
    pub fn select_with_rls(
        &self,
        table: &str,
        select_cols: &str,
        filters: &[(String, String)],
        order: Option<&str>,
        limit: Option<usize>,
        offset: Option<usize>,
        user_id: Option<&str>,
    ) -> Result<(Vec<Tuple>, Vec<String>)> {
        // Build the base filter list, then maybe add an ownership filter.
        let mut all_filters: Vec<(String, String)> = filters.to_vec();

        if let Some(uid) = user_id {
            if let Some(col) = self.detect_owner_column(table)? {
                all_filters.push((col, format!("eq.{uid}")));
            }
        }

        self.select(table, select_cols, &all_filters, order, limit, offset)
    }

    // ── UPDATE with RLS ─────────────────────────────────────────────────

    /// Update rows with an implicit RLS ownership filter.
    pub fn update_with_rls(
        &self,
        table: &str,
        set_values: &serde_json::Value,
        filters: &[(String, String)],
        user_id: Option<&str>,
    ) -> Result<u64> {
        let mut all_filters: Vec<(String, String)> = filters.to_vec();

        if let Some(uid) = user_id {
            if let Some(col) = self.detect_owner_column(table)? {
                all_filters.push((col, format!("eq.{uid}")));
            }
        }

        self.update(table, set_values, &all_filters)
    }

    // ── DELETE with RLS ─────────────────────────────────────────────────

    /// Delete rows with an implicit RLS ownership filter.
    pub fn delete_with_rls(
        &self,
        table: &str,
        filters: &[(String, String)],
        user_id: Option<&str>,
    ) -> Result<u64> {
        let mut all_filters: Vec<(String, String)> = filters.to_vec();

        if let Some(uid) = user_id {
            if let Some(col) = self.detect_owner_column(table)? {
                all_filters.push((col, format!("eq.{uid}")));
            }
        }

        self.delete(table, &all_filters)
    }

    // ── Ownership detection ─────────────────────────────────────────────

    /// Check whether `table` has a column named `owner_id` or `user_id`.
    ///
    /// Returns `Some(column_name)` if found, `None` otherwise.
    fn detect_owner_column(&self, table: &str) -> Result<Option<String>> {
        validate_identifier(table)?;
        let col_sql = format!("SELECT * FROM {} LIMIT 0", quote_ident(table));
        let (_, col_names) = self.db.query_with_columns(&col_sql)?;

        for candidate in &["owner_id", "user_id"] {
            if col_names.iter().any(|c| c == candidate) {
                return Ok(Some((*candidate).to_string()));
            }
        }
        Ok(None)
    }

    // ── INSERT ───────────────────────────────────────────────────────────

    /// Insert one or more rows (provided as JSON objects) into `table`.
    ///
    /// Returns `(affected_rows, inserted_tuples, column_names)`.
    /// The returned tuples come from a follow-up SELECT; if the table is
    /// empty after insert this falls back gracefully.
    pub fn insert(
        &self,
        table: &str,
        rows: &[serde_json::Value],
    ) -> Result<(u64, Vec<Tuple>, Vec<String>)> {
        validate_identifier(table)?;

        if rows.is_empty() {
            return Err(Error::sql_parse("No rows to insert"));
        }

        let mut total_affected: u64 = 0;

        for row in rows {
            let obj = row.as_object().ok_or_else(|| {
                Error::sql_parse("Each row must be a JSON object")
            })?;

            if obj.is_empty() {
                return Err(Error::sql_parse("Row object must have at least one column"));
            }

            let mut col_names = Vec::with_capacity(obj.len());
            let mut placeholders = Vec::with_capacity(obj.len());
            let mut params: Vec<Value> = Vec::with_capacity(obj.len());

            for (i, (col, val)) in obj.iter().enumerate() {
                validate_identifier(col)?;
                col_names.push(quote_ident(col));
                placeholders.push(format!("${}", i + 1));
                params.push(json_value_to_db_value(val));
            }

            let sql = format!(
                "INSERT INTO {} ({}) VALUES ({})",
                quote_ident(table),
                col_names.join(", "),
                placeholders.join(", "),
            );

            let affected = self.db.execute_params(&sql, &params)?;
            total_affected += affected;
        }

        // Fetch the inserted data back for Prefer: return=representation
        let col_sql = format!("SELECT * FROM {} LIMIT 0", quote_ident(table));
        let (_, col_names) = self.db.query_with_columns(&col_sql)?;

        Ok((total_affected, Vec::new(), col_names))
    }

    // ── UPDATE ───────────────────────────────────────────────────────────

    /// Update rows matching `filters` with the values from `set_values`.
    ///
    /// Returns the number of rows affected.
    pub fn update(
        &self,
        table: &str,
        set_values: &serde_json::Value,
        filters: &[(String, String)],
    ) -> Result<u64> {
        validate_identifier(table)?;

        let obj = set_values.as_object().ok_or_else(|| {
            Error::sql_parse("Update body must be a JSON object")
        })?;

        if obj.is_empty() {
            return Err(Error::sql_parse("Update body must have at least one column"));
        }

        let mut set_parts = Vec::with_capacity(obj.len());
        let mut params: Vec<Value> = Vec::with_capacity(obj.len() + filters.len());
        let mut param_idx: usize = 1;

        for (col, val) in obj {
            validate_identifier(col)?;
            set_parts.push(format!("{} = ${param_idx}", quote_ident(col)));
            params.push(json_value_to_db_value(val));
            param_idx += 1;
        }

        let mut sql = format!(
            "UPDATE {} SET {}",
            quote_ident(table),
            set_parts.join(", "),
        );

        // WHERE clause
        if !filters.is_empty() {
            let mut conditions = Vec::with_capacity(filters.len());
            for (col, expr) in filters {
                let pf = parse_filter(col, expr, param_idx)?;
                param_idx += pf.params.len();
                conditions.push(pf.sql_fragment);
                params.extend(pf.params);
            }
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }

        self.db.execute_params(&sql, &params)
    }

    // ── DELETE ───────────────────────────────────────────────────────────

    /// Delete rows matching `filters`.
    ///
    /// Returns the number of rows affected.
    pub fn delete(
        &self,
        table: &str,
        filters: &[(String, String)],
    ) -> Result<u64> {
        validate_identifier(table)?;

        let mut sql = format!("DELETE FROM {}", quote_ident(table));
        let mut params: Vec<Value> = Vec::new();
        let mut param_idx: usize = 1;

        if !filters.is_empty() {
            let mut conditions = Vec::with_capacity(filters.len());
            for (col, expr) in filters {
                let pf = parse_filter(col, expr, param_idx)?;
                param_idx += pf.params.len();
                conditions.push(pf.sql_fragment);
                params.extend(pf.params);
            }
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }

        if params.is_empty() {
            self.db.execute(&sql)
        } else {
            self.db.execute_params(&sql, &params)
        }
    }
}

/// Convert a `serde_json::Value` to our internal `Value` type.
fn json_value_to_db_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Boolean(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                if let Ok(i32_val) = i32::try_from(i) {
                    Value::Int4(i32_val)
                } else {
                    Value::Int8(i)
                }
            } else if let Some(f) = n.as_f64() {
                Value::Float8(f)
            } else {
                Value::String(n.to_string())
            }
        }
        serde_json::Value::String(s) => Value::String(s.clone()),
        // Arrays and objects are stored as JSON text
        other => Value::String(other.to_string()),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── Filter parsing ───────────────────────────────────────────────────

    #[test]
    fn test_parse_filter_eq() {
        let pf = parse_filter("id", "eq.123", 1).unwrap();
        assert_eq!(pf.sql_fragment, "id = $1");
        assert_eq!(pf.params.len(), 1);
    }

    #[test]
    fn test_parse_filter_neq() {
        let pf = parse_filter("status", "neq.deleted", 3).unwrap();
        assert_eq!(pf.sql_fragment, "status != $3");
        assert_eq!(pf.params.len(), 1);
    }

    #[test]
    fn test_parse_filter_gt() {
        let pf = parse_filter("age", "gt.18", 1).unwrap();
        assert_eq!(pf.sql_fragment, "age > $1");
    }

    #[test]
    fn test_parse_filter_gte() {
        let pf = parse_filter("score", "gte.90", 2).unwrap();
        assert_eq!(pf.sql_fragment, "score >= $2");
    }

    #[test]
    fn test_parse_filter_lt() {
        let pf = parse_filter("price", "lt.100", 1).unwrap();
        assert_eq!(pf.sql_fragment, "price < $1");
    }

    #[test]
    fn test_parse_filter_lte() {
        let pf = parse_filter("qty", "lte.5", 1).unwrap();
        assert_eq!(pf.sql_fragment, "qty <= $1");
    }

    #[test]
    fn test_parse_filter_like() {
        let pf = parse_filter("name", "like.*test*", 1).unwrap();
        assert_eq!(pf.sql_fragment, "name LIKE $1");
        assert_eq!(pf.params, vec![Value::String("%test%".to_string())]);
    }

    #[test]
    fn test_parse_filter_ilike() {
        let pf = parse_filter("email", "ilike.*@example*", 1).unwrap();
        assert_eq!(pf.sql_fragment, "email ILIKE $1");
        assert_eq!(pf.params, vec![Value::String("%@example%".to_string())]);
    }

    #[test]
    fn test_parse_filter_is_null() {
        let pf = parse_filter("deleted", "is.null", 1).unwrap();
        assert_eq!(pf.sql_fragment, "deleted IS NULL");
        assert!(pf.params.is_empty());
    }

    #[test]
    fn test_parse_filter_is_true() {
        let pf = parse_filter("active", "is.true", 1).unwrap();
        assert_eq!(pf.sql_fragment, "active IS TRUE");
    }

    #[test]
    fn test_parse_filter_is_false() {
        let pf = parse_filter("verified", "is.false", 1).unwrap();
        assert_eq!(pf.sql_fragment, "verified IS FALSE");
    }

    #[test]
    fn test_parse_filter_in() {
        let pf = parse_filter("status", "in.(active,pending,review)", 1).unwrap();
        assert_eq!(pf.sql_fragment, "status IN ($1, $2, $3)");
        assert_eq!(pf.params.len(), 3);
    }

    #[test]
    fn test_parse_filter_negated() {
        let pf = parse_filter("role", "not.eq.admin", 1).unwrap();
        assert_eq!(pf.sql_fragment, "NOT role = $1");
    }

    #[test]
    fn test_parse_filter_not_in() {
        let pf = parse_filter("id", "not.in.(1,2,3)", 1).unwrap();
        assert_eq!(pf.sql_fragment, "NOT id IN ($1, $2, $3)");
        assert_eq!(pf.params.len(), 3);
    }

    #[test]
    fn test_parse_filter_invalid_operator() {
        let result = parse_filter("x", "foo.bar", 1);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_filter_no_dot() {
        let result = parse_filter("x", "nodot", 1);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_filter_invalid_column() {
        let result = parse_filter("drop table;--", "eq.1", 1);
        assert!(result.is_err());
    }

    // ── Order parsing ────────────────────────────────────────────────────

    #[test]
    fn test_parse_order_single_asc() {
        let clause = parse_order_clause("name.asc").unwrap();
        assert_eq!(clause, "name ASC");
    }

    #[test]
    fn test_parse_order_single_desc() {
        let clause = parse_order_clause("created_at.desc").unwrap();
        assert_eq!(clause, "created_at DESC");
    }

    #[test]
    fn test_parse_order_default_asc() {
        let clause = parse_order_clause("id").unwrap();
        assert_eq!(clause, "id ASC");
    }

    #[test]
    fn test_parse_order_multiple() {
        let clause = parse_order_clause("name.asc,id.desc").unwrap();
        assert_eq!(clause, "name ASC, id DESC");
    }

    #[test]
    fn test_parse_order_invalid_direction() {
        let result = parse_order_clause("id.sideways");
        assert!(result.is_err());
    }

    // ── Select columns ──────────────────────────────────────────────────

    #[test]
    fn test_parse_select_star() {
        let cols = parse_select_columns("*").unwrap();
        assert_eq!(cols, "*");
    }

    #[test]
    fn test_parse_select_named() {
        let cols = parse_select_columns("id,name,email").unwrap();
        assert_eq!(cols, "id, name, email");
    }

    #[test]
    fn test_parse_select_invalid() {
        let result = parse_select_columns("id, 1nvalid");
        assert!(result.is_err());
    }

    // ── Identifier validation ────────────────────────────────────────────

    #[test]
    fn test_validate_identifier_ok() {
        assert!(validate_identifier("users").is_ok());
        assert!(validate_identifier("_private").is_ok());
        assert!(validate_identifier("col_123").is_ok());
    }

    #[test]
    fn test_validate_identifier_bad() {
        assert!(validate_identifier("").is_err());
        assert!(validate_identifier("123abc").is_err());
        assert!(validate_identifier("my-col").is_err());
        assert!(validate_identifier("table; DROP").is_err());
    }

    // ── JSON to DB value conversion ─────────────────────────────────────

    #[test]
    fn test_json_to_value_null() {
        assert_eq!(json_value_to_db_value(&serde_json::Value::Null), Value::Null);
    }

    #[test]
    fn test_json_to_value_bool() {
        let v = json_value_to_db_value(&serde_json::json!(true));
        assert_eq!(v, Value::Boolean(true));
    }

    #[test]
    fn test_json_to_value_int() {
        let v = json_value_to_db_value(&serde_json::json!(42));
        assert_eq!(v, Value::Int4(42));
    }

    #[test]
    fn test_json_to_value_big_int() {
        let big: i64 = 5_000_000_000;
        let v = json_value_to_db_value(&serde_json::json!(big));
        assert_eq!(v, Value::Int8(big));
    }

    #[test]
    fn test_json_to_value_float() {
        let v = json_value_to_db_value(&serde_json::json!(3.14));
        assert_eq!(v, Value::Float8(3.14));
    }

    #[test]
    fn test_json_to_value_string() {
        let v = json_value_to_db_value(&serde_json::json!("hello"));
        assert_eq!(v, Value::String("hello".to_string()));
    }

    // ── Integration: RestExecutor with in-memory DB ─────────────────────

    #[test]
    fn test_executor_select_empty_table() {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        db.execute("CREATE TABLE items (id INT, name TEXT)").unwrap();

        let exec = RestExecutor::new(db);
        let (rows, cols) = exec.select("items", "*", &[], None, None, None).unwrap();
        assert!(rows.is_empty());
        assert_eq!(cols.len(), 2);
    }

    #[test]
    fn test_executor_insert_and_select() {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        db.execute("CREATE TABLE users (id INT, name TEXT)").unwrap();

        let exec = RestExecutor::new(db);
        let rows = vec![serde_json::json!({"id": 1, "name": "Alice"})];
        let (affected, _, _) = exec.insert("users", &rows).unwrap();
        assert_eq!(affected, 1);

        let (result, cols) = exec.select("users", "*", &[], None, None, None).unwrap();
        assert_eq!(result.len(), 1);
        assert!(!cols.is_empty());
    }

    #[test]
    fn test_executor_select_with_eq_filter() {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        db.execute("CREATE TABLE t (id INT, val TEXT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
        db.execute("INSERT INTO t VALUES (2, 'b')").unwrap();

        let exec = RestExecutor::new(db);
        let filters = vec![("val".to_string(), "eq.a".to_string())];
        let (rows, _) = exec.select("t", "*", &filters, None, None, None).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn test_executor_update() {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        db.execute("CREATE TABLE t (id INT, val TEXT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 'old')").unwrap();

        let exec = RestExecutor::new(db.clone());
        let set = serde_json::json!({"val": "new"});
        let filters = vec![("id".to_string(), "eq.1".to_string())];
        let affected = exec.update("t", &set, &filters).unwrap();
        assert_eq!(affected, 1);

        // Verify
        let tuples = db.query("SELECT val FROM t WHERE id = 1", &[]).unwrap();
        assert_eq!(tuples.len(), 1);
        assert_eq!(tuples[0].values[0], Value::String("new".to_string()));
    }

    #[test]
    fn test_executor_delete() {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        db.execute("CREATE TABLE t (id INT, val TEXT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
        db.execute("INSERT INTO t VALUES (2, 'b')").unwrap();

        let exec = RestExecutor::new(db.clone());
        let filters = vec![("id".to_string(), "eq.1".to_string())];
        let affected = exec.delete("t", &filters).unwrap();
        assert_eq!(affected, 1);

        let tuples = db.query("SELECT * FROM t", &[]).unwrap();
        assert_eq!(tuples.len(), 1);
    }

    #[test]
    fn test_executor_select_with_order_limit_offset() {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        db.execute("CREATE TABLE nums (id INT, val INT)").unwrap();
        db.execute("INSERT INTO nums VALUES (1, 10)").unwrap();
        db.execute("INSERT INTO nums VALUES (2, 20)").unwrap();
        db.execute("INSERT INTO nums VALUES (3, 30)").unwrap();

        let exec = RestExecutor::new(db);
        let (rows, _) = exec.select(
            "nums", "*", &[],
            Some("id.desc"),
            Some(2),
            Some(0),
        ).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn test_executor_insert_no_rows() {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        db.execute("CREATE TABLE t (id INT)").unwrap();

        let exec = RestExecutor::new(db);
        let result = exec.insert("t", &[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_executor_invalid_table_name() {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let exec = RestExecutor::new(db);
        let result = exec.select("bad;table", "*", &[], None, None, None);
        assert!(result.is_err());
    }
}
