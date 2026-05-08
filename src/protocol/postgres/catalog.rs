//! PostgreSQL system catalog emulation
//!
//! This module provides minimal emulation of PostgreSQL system catalogs
//! (pg_catalog) and information_schema for client compatibility.
//! Many PostgreSQL clients query these system tables during connection
//! and for introspection.

use crate::{Result, Schema, Tuple, Value, Column, DataType, EmbeddedDatabase};
use std::sync::Arc;

/// PostgreSQL catalog emulator
pub struct PgCatalog {
    /// Reference to the database for real catalog queries
    database: Option<Arc<EmbeddedDatabase>>,
}

impl PgCatalog {
    /// Create a new catalog emulator (without database access - static responses only)
    pub fn new() -> Self {
        Self { database: None }
    }

    /// Create a new catalog emulator with database access for real table/column metadata
    pub fn with_database(database: Arc<EmbeddedDatabase>) -> Self {
        Self { database: Some(database) }
    }

    /// Handle catalog queries
    ///
    /// Returns Some((schema, rows)) if this is a catalog query,
    /// None if it should be handled by the normal query engine
    pub fn handle_query(&self, query: &str) -> Result<Option<(Schema, Vec<Tuple>)>> {
        let query_lower = query.trim().to_lowercase();

        // --- psql meta-command query detection ---------------------------
        // psql sends complex JOINs across pg_class / pg_namespace /
        // pg_attribute that our simple substring matcher can't resolve, so
        // recognise them by signature and synthesise a shaped response.
        if let Some(result) = self.try_psql_metacommand(&query_lower)? {
            return Ok(Some(result));
        }

        // Handle SELECT version() - required by SQLAlchemy, psql, pgAdmin, DBeaver
        if query_lower.contains("version()") {
            return Ok(Some(self.query_version()?));
        }

        // Handle SELECT current_schema() - required by SQLAlchemy connection init
        if query_lower.contains("current_schema()") {
            return Ok(Some(Self::query_current_schema()?));
        }

        // Handle SELECT current_database() - required by SQLAlchemy / pgAdmin
        if query_lower.contains("current_database()") {
            return Ok(Some(Self::query_current_database()?));
        }

        // Handle SELECT current_user - required by various PG clients
        if query_lower.contains("current_user")
            && !query_lower.contains("current_user_id")
            && (query_lower.starts_with("select") || query_lower.starts_with("show"))
        {
            return Ok(Some(Self::query_current_user()?));
        }

        // Check for information_schema queries (table / column listing).
        // We must match the TABLE reference (`information_schema.<name>`
        // or `from information_schema.`) and NOT the string literal
        // `'information_schema'` that Drizzle / postgres-js / Prisma
        // pass in WHERE clauses like
        //   SELECT … FROM pg_tables WHERE schemaname NOT IN
        //   ('pg_catalog','information_schema');
        let has_information_schema_ref =
            query_lower.contains("information_schema.")
            || query_lower.contains(" information_schema ");
        let result = if has_information_schema_ref {
            if query_lower.contains("information_schema.columns") {
                Some(self.query_information_schema_columns(&query_lower)?)
            } else if query_lower.contains("information_schema.tables") {
                Some(self.query_information_schema_tables(&query_lower)?)
            } else if query_lower.contains("information_schema.key_column_usage") {
                Some(self.query_information_schema_key_column_usage()?)
            } else if query_lower.contains("information_schema.table_constraints") {
                Some(self.query_information_schema_table_constraints()?)
            } else if query_lower.contains("information_schema.referential_constraints") {
                Some(self.query_information_schema_referential_constraints()?)
            } else if query_lower.contains("information_schema.routines") {
                Some(Self::query_information_schema_routines())
            } else if query_lower.contains("information_schema.check_constraints") {
                Some(Self::query_information_schema_check_constraints())
            } else if query_lower.contains("information_schema.views") {
                Some(Self::query_information_schema_views())
            } else if query_lower.contains("information_schema.schemata") {
                Some(self.query_information_schema_schemata()?)
            } else if let Some(name) = Self::information_schema_view_name(&query_lower) {
                if let Some(empty) = Self::known_empty_information_schema_view(&name) {
                    Some(empty)
                } else {
                    return Err(crate::Error::QueryExecution(format!(
                        "information_schema.{name} is not a recognised view; \
                         HeliosDB Nano implements the SQL-standard subset \
                         (tables, columns, schemata, key_column_usage, \
                         table_constraints, referential_constraints, routines, \
                         check_constraints, views) and a whitelist of empty \
                         placeholder views (triggers, parameters, sequences, \
                         domains, character_sets, collations, *_privileges, \
                         role_*). Please file an issue if this view is needed."
                    )));
                }
            } else {
                // Bare `from information_schema` reference without a view name
                // (rare; psql `\dn`-style probes). Pass through.
                Some((Schema::new(vec![]), vec![]))
            }
        } else if !Self::is_catalog_query(&query_lower) {
            return Ok(None);
        } else if query_lower.contains("pg_type") {
            Some(self.query_pg_type()?)
        } else if query_lower.contains("pg_sequences") {
            // KanttBan / Drizzle-kit pull queries `pg_sequences` during
            // introspection. Nano doesn't expose user-facing sequences
            // (BIGSERIAL is a synthetic counter, not a sequence object),
            // so return the standard 7-column shape with no rows.
            Some((Schema::new(vec![
                Column::new("schemaname", DataType::Text),
                Column::new("sequencename", DataType::Text),
                Column::new("sequenceowner", DataType::Text),
                Column::new("data_type", DataType::Text),
                Column::new("start_value", DataType::Int8),
                Column::new("min_value", DataType::Int8),
                Column::new("max_value", DataType::Int8),
                Column::new("increment_by", DataType::Int8),
                Column::new("cycle", DataType::Boolean),
                Column::new("cache_size", DataType::Int8),
                Column::new("last_value", DataType::Int8),
            ]), vec![]))
        } else if query_lower.contains("pg_policies") {
            // RLS policies — drizzle-kit `pull` introspects this after
            // pg_sequences (KanttBan #12 against v3.28.0). Nano's RLS
            // is exposed via the per-tenant manager rather than
            // pg_policies, so we return the standard 8-column shape
            // with zero rows.
            Some((Schema::new(vec![
                Column::new("schemaname", DataType::Text),
                Column::new("tablename", DataType::Text),
                Column::new("policyname", DataType::Text),
                Column::new("permissive", DataType::Text),
                Column::new("roles", DataType::Text),
                Column::new("cmd", DataType::Text),
                Column::new("qual", DataType::Text),
                Column::new("with_check", DataType::Text),
            ]), vec![]))
        } else if query_lower.contains("pg_statistic_ext") {
            // Extended-statistics catalog. psql's `\d <table>` sends a
            // 9-column query joining pg_attribute in a subquery. Nano
            // has no extended stats — return the empty 9-column shape
            // so libpq's column-count check succeeds (KanttBan #7
            // follow-up, v3.30.1 smoke).
            Some((Schema::new(vec![
                Column::new("oid", DataType::Int4),
                Column::new("stxrelid", DataType::Text),
                Column::new("nsp", DataType::Text),
                Column::new("stxname", DataType::Text),
                Column::new("columns", DataType::Text),
                Column::new("ndist_enabled", DataType::Boolean),
                Column::new("deps_enabled", DataType::Boolean),
                Column::new("mcv_enabled", DataType::Boolean),
                Column::new("stxstattarget", DataType::Int4),
            ]), vec![]))
        } else if query_lower.contains("pg_catalog.pg_policy ")
            || query_lower.contains("from pg_policy ")
            || query_lower.contains("pg_catalog.pg_policy\n")
        {
            // pg_policy (singular catalog table; pg_policies is the
            // schemaname-prefixed view above). psql `\d <table>` sends
            // a 6-column query against this. Empty shape.
            Some((Schema::new(vec![
                Column::new("polname", DataType::Text),
                Column::new("polpermissive", DataType::Boolean),
                Column::new("roles", DataType::Text),
                Column::new("qual", DataType::Text),
                Column::new("with_check", DataType::Text),
                Column::new("cmd", DataType::Text),
            ]), vec![]))
        } else if query_lower.contains("pg_matviews") {
            // Materialized views — drizzle-kit also introspects this.
            // Standard 6-column shape; we have first-class support
            // for matviews via `pg_mv_staleness()` but the pg_matviews
            // PG-shaped view is a separate surface most ORMs default to.
            Some((Schema::new(vec![
                Column::new("schemaname", DataType::Text),
                Column::new("matviewname", DataType::Text),
                Column::new("matviewowner", DataType::Text),
                Column::new("tablespace", DataType::Text),
                Column::new("hasindexes", DataType::Boolean),
                Column::new("ispopulated", DataType::Boolean),
                Column::new("definition", DataType::Text),
            ]), vec![]))
        } else if query_lower.contains("pg_inherits") {
            // Inheritance is not a Nano feature. psql `\d` sends two
            // queries against pg_inherits (parent + child); without an
            // explicit empty shape they fall through to pg_class and
            // psql renders bogus "Inherits / Number of child tables"
            // sections. Return empty 3-col shape that absorbs both
            // forms (parent: 1 col, child: 3 cols — psql is tolerant
            // when zero rows come back).
            Some((Schema::new(vec![
                Column::new("oid", DataType::Text),
                Column::new("relkind", DataType::Char(1)),
                Column::new("partbound", DataType::Text),
            ]), vec![]))
        } else if query_lower.contains("pg_publication") {
            // Logical replication publications — not implemented.
            // psql `\d` sends a UNION over pg_publication +
            // pg_publication_rel selecting one column (`pubname`).
            Some((Schema::new(vec![
                Column::new("pubname", DataType::Text),
            ]), vec![]))
        } else if query_lower.contains("pg_index") && !query_lower.contains("pg_indexes") {
            Some(self.query_pg_index()?)
        } else if query_lower.contains("pg_indexes") {
            Some(self.query_pg_indexes()?)
        } else if query_lower.contains("pg_tables") {
            Some(self.query_pg_tables()?)
        } else if query_lower.contains("pg_views") {
            Some(self.query_pg_views()?)
        } else if query_lower.contains("pg_constraint") {
            Some(self.query_pg_constraint()?)
        } else if query_lower.contains("pg_description") {
            // No table/column comments stored.
            Some((Schema::new(vec![
                Column::new("objoid", DataType::Int4),
                Column::new("classoid", DataType::Int4),
                Column::new("objsubid", DataType::Int4),
                Column::new("description", DataType::Text),
            ]), vec![]))
        } else if query_lower.contains("pg_roles") || query_lower.contains("pg_user") {
            Some(self.query_pg_roles()?)
        } else if query_lower.contains("pg_proc") {
            // Procedures — empty set is fine (we don't expose pg_catalog-registered functions).
            Some((Schema::new(vec![
                Column::new("oid", DataType::Int4),
                Column::new("proname", DataType::Text),
                Column::new("pronamespace", DataType::Int4),
            ]), vec![]))
        } else if query_lower.contains("pg_class") {
            Some(self.query_pg_class()?)
        } else if query_lower.contains("pg_namespace") {
            Some(self.query_pg_namespace()?)
        } else if query_lower.contains("pg_database") {
            Some(self.query_pg_database()?)
        } else if query_lower.contains("pg_settings") {
            Some(self.query_pg_settings()?)
        } else if query_lower.contains("pg_attribute") {
            Some(self.query_pg_attribute()?)
        } else {
            // Return empty result for unknown catalog queries
            Some((Schema::new(vec![]), vec![]))
        };

        // Apply WHERE filter + column projection based on the user's
        // SELECT clause. Catalog queries come in from every direction
        // (Drizzle / postgres-js / psycopg introspection), so without
        // these filters we'd send the full table regardless of the
        // predicate — B20 from the TimeTracker report.
        //
        // KanttBan #21A (v3.30.1): if the SELECT contains an aggregate
        // (`count(*)` / `count(col)`) we collapse rows AFTER filtering
        // and BEFORE projection — projection looks for column names in
        // the schema and can't see synthetic aggregate output columns.
        // drizzle-kit's introspection asks for things like
        //   SELECT count(*) FROM pg_namespace WHERE nspname IS NULL;
        //   SELECT table_schema, count(*) FROM information_schema.tables GROUP BY table_schema;
        // Without this stage both queries return the underlying tuples
        // and break tooling that expects scalar shapes.
        match result {
            Some((schema, rows)) => {
                let filtered = Self::apply_where_filter(&query_lower, &schema, rows);
                if let Some(agg) = Self::apply_aggregate(&query_lower, &schema, &filtered) {
                    return Ok(Some(agg));
                }
                let projected = Self::project_columns(&query_lower, schema, filtered);
                Ok(Some(projected))
            }
            None => Ok(None),
        }
    }

    /// Detect `count(*)` (with optional `GROUP BY <col>`) in the SELECT
    /// clause of a catalog query and collapse the rows accordingly.
    /// Returns `None` when the query is not an aggregate, leaving the
    /// caller to fall through to ordinary projection.
    ///
    /// Only handles the shapes drivers actually emit against catalog
    /// tables — bare `count(*)` and single-column `GROUP BY`. Anything
    /// more complex (multiple GROUP BY columns, HAVING, custom
    /// aggregates) falls through and the caller returns the
    /// underlying rows; that's the same "graceful degradation" path
    /// `apply_where_filter` and `project_columns` use.
    fn apply_aggregate(q: &str, schema: &Schema, rows: &[Tuple]) -> Option<(Schema, Vec<Tuple>)> {
        if !q.contains("count(") {
            return None;
        }

        let select_pos = q.find("select")? + "select".len();
        let from_pos = q.find(" from ")?;
        if select_pos >= from_pos {
            return None;
        }

        // Pull the GROUP BY column (if any). Stop at the next clause
        // keyword so trailing ORDER BY / LIMIT don't bleed in.
        let group_by_col = q.find(" group by ").map(|g| {
            let after = &q[g + " group by ".len()..];
            let mut end = after.len();
            for t in [" order by ", " having ", " limit ", " offset ", ";"] {
                if let Some(p) = after.find(t) {
                    if p < end { end = p; }
                }
            }
            after[..end].trim().to_string()
        });

        if let Some(group_col_raw) = group_by_col {
            // Strip alias prefix (`t.col` → `col`) and quotes.
            let group_col = group_col_raw
                .rsplit('.')
                .next()
                .unwrap_or(&group_col_raw)
                .trim()
                .trim_matches('"')
                .to_lowercase();
            let col_idx = schema
                .columns
                .iter()
                .position(|c| c.name.to_lowercase() == group_col)?;

            let mut buckets: Vec<(Value, i64)> = Vec::new();
            for row in rows {
                let v = row.values.get(col_idx).cloned().unwrap_or(Value::Null);
                if let Some(b) = buckets.iter_mut().find(|(bv, _)| bv == &v) {
                    b.1 += 1;
                } else {
                    buckets.push((v, 1));
                }
            }

            // Safety: col_idx came from `position` above.
            #[allow(clippy::indexing_slicing)]
            let group_col_meta = schema.columns[col_idx].clone();
            let out_schema = Schema::new(vec![
                group_col_meta,
                Column::new("count", DataType::Int8),
            ]);
            let out_rows: Vec<Tuple> = buckets
                .into_iter()
                .map(|(v, c)| Tuple::new(vec![v, Value::Int8(c)]))
                .collect();
            Some((out_schema, out_rows))
        } else {
            // Bare `count(*)` — collapse to a single scalar row.
            let n = rows.len() as i64;
            let out_schema = Schema::new(vec![Column::new("count", DataType::Int8)]);
            let out_rows = vec![Tuple::new(vec![Value::Int8(n)])];
            Some((out_schema, out_rows))
        }
    }

    /// Apply a small subset of WHERE predicates directly to catalog
    /// rows before we send them back. Supports the common driver
    /// introspection shapes:
    ///   * `col = 'literal'`
    ///   * `col = N`
    ///   * `col IN ('a','b',...)` / `col NOT IN (...)`
    ///   * `col <> 'literal'` / `col != 'literal'`
    ///   * conjunctions (`AND`) — evaluated left-to-right
    ///
    /// Anything more complex (OR, function calls, subqueries) falls
    /// through unchanged; the caller will get all rows, which is
    /// still correct-if-noisy for every driver I've tested.
    fn apply_where_filter(q: &str, schema: &Schema, rows: Vec<Tuple>) -> Vec<Tuple> {
        // Find `where ` and collect the text up to the next clause
        // keyword (`order by`, `group by`, `limit`, `;`, end).
        let where_kw = " where ";
        let start = match q.find(where_kw) { Some(p) => p + where_kw.len(), None => return rows };
        let terminators = [" order by ", " group by ", " limit ", " offset ", ";"];
        let mut end = q.len();
        for t in &terminators {
            if let Some(p) = q[start..].find(t) {
                let cand = start + p;
                if cand < end { end = cand; }
            }
        }
        let predicate = q[start..end].trim();
        if predicate.is_empty() { return rows; }

        // Split on " and " at the top level (we don't handle parens).
        let preds: Vec<&str> = predicate.split(" and ").map(str::trim).collect();
        rows.into_iter().filter(|row| preds.iter().all(|p| Self::eval_simple_pred(p, schema, row))).collect()
    }

    /// Evaluate one of the predicate shapes supported by
    /// `apply_where_filter`. Returns `true` when the predicate can't
    /// be parsed — matches our "when in doubt, keep the row"
    /// behaviour and avoids silently dropping data for complex
    /// WHEREs we don't yet interpret.
    fn eval_simple_pred(pred: &str, schema: &Schema, row: &Tuple) -> bool {
        let p = pred.trim();

        // `col is null` / `col is not null` (KanttBan #21A, v3.30.1).
        // Must be tested BEFORE the `=` / `<>` family because these
        // predicates also contain spaces around the column name.
        if let Some(idx) = p.find(" is not null") {
            let col_name = p[..idx].trim();
            let val = Self::row_value(schema, row, col_name);
            return !matches!(val, Value::Null);
        }
        if let Some(idx) = p.find(" is null") {
            let col_name = p[..idx].trim();
            let val = Self::row_value(schema, row, col_name);
            return matches!(val, Value::Null);
        }

        // `col NOT IN (a, b, c)` — must be tested BEFORE plain `IN`.
        if let Some(idx) = p.find(" not in (") {
            let col_name = p[..idx].trim();
            let rest = p[idx + " not in (".len()..].trim_end_matches(')');
            let items = Self::parse_in_list(rest);
            let val = Self::row_value(schema, row, col_name);
            return !items.iter().any(|v| Self::lit_eq_value(v, &val));
        }
        if let Some(idx) = p.find(" in (") {
            let col_name = p[..idx].trim();
            let rest = p[idx + " in (".len()..].trim_end_matches(')');
            let items = Self::parse_in_list(rest);
            let val = Self::row_value(schema, row, col_name);
            return items.iter().any(|v| Self::lit_eq_value(v, &val));
        }

        // `col = 'lit'`, `col = N`, `col <> 'lit'`, `col != 'lit'`
        for (op, eq) in [(" = ", true), (" <> ", false), (" != ", false)] {
            if let Some(idx) = p.find(op) {
                let col_name = p[..idx].trim();
                let rhs = p[idx + op.len()..].trim();
                let val = Self::row_value(schema, row, col_name);
                let matches = Self::lit_eq_value(rhs, &val);
                return if eq { matches } else { !matches };
            }
        }

        // Unknown predicate shape — keep the row.
        true
    }

    fn parse_in_list(s: &str) -> Vec<String> {
        s.trim().trim_matches(|c: char| c == '(' || c == ')')
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    fn row_value(schema: &Schema, row: &Tuple, col_name: &str) -> Value {
        let col_lower = col_name.trim().trim_matches('"').to_lowercase();
        if let Some(idx) = schema.columns.iter().position(|c| c.name.to_lowercase() == col_lower) {
            row.values.get(idx).cloned().unwrap_or(Value::Null)
        } else {
            Value::Null
        }
    }

    /// Compare a literal (as written in SQL: `'abc'` or `42`) with a
    /// `Value`. Strips single quotes, parses numerics.
    fn lit_eq_value(lit: &str, val: &Value) -> bool {
        let lit = lit.trim();
        // String literal
        if (lit.starts_with('\'') && lit.ends_with('\'')) && lit.len() >= 2 {
            let s = &lit[1..lit.len() - 1];
            return match val {
                Value::String(v) => v == s,
                Value::Null => false,
                other => other.to_string() == s,
            };
        }
        // NULL literal
        if lit.eq_ignore_ascii_case("null") {
            return matches!(val, Value::Null);
        }
        // Numeric literal
        if let Ok(n) = lit.parse::<i64>() {
            return match val {
                Value::Int2(v) => (*v as i64) == n,
                Value::Int4(v) => (*v as i64) == n,
                Value::Int8(v) => *v == n,
                _ => false,
            };
        }
        if let Ok(f) = lit.parse::<f64>() {
            return match val {
                Value::Float4(v) => (*v as f64 - f).abs() < 1e-9,
                Value::Float8(v) => (v - f).abs() < 1e-9,
                _ => false,
            };
        }
        // Bool
        if lit.eq_ignore_ascii_case("true") { return matches!(val, Value::Boolean(true)); }
        if lit.eq_ignore_ascii_case("false") { return matches!(val, Value::Boolean(false)); }
        false
    }

    /// Query information_schema.tables - returns real table metadata from the catalog
    fn query_information_schema_tables(&self, query_lower: &str) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("table_catalog", DataType::Text),
            Column::new("table_schema", DataType::Text),
            Column::new("table_name", DataType::Text),
            Column::new("table_type", DataType::Text),
        ]);

        let db = match &self.database {
            Some(db) => db,
            None => return Ok((schema, vec![])),
        };

        // Get real table list from storage catalog
        let catalog = db.storage.catalog();
        let table_names = catalog.list_tables()?;

        // Extract LIKE filter if present (e.g., "table_name LIKE 'tenant_xyz__%'")
        let like_filter = Self::extract_like_filter(query_lower, "table_name");

        let mut rows = Vec::new();
        for name in &table_names {
            // Apply LIKE filter if present
            if let Some(ref pattern) = like_filter {
                if !Self::sql_like_match(name, pattern) {
                    continue;
                }
            }

            rows.push(Tuple::new(vec![
                Value::String("heliosdb".to_string()),
                Value::String("public".to_string()),
                Value::String(name.clone()),
                Value::String("BASE TABLE".to_string()),
            ]));
        }

        Ok((schema, rows))
    }

    /// Query information_schema.columns - returns real column metadata from the catalog
    fn query_information_schema_columns(&self, query_lower: &str) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("table_name", DataType::Text),
            Column::new("column_name", DataType::Text),
            Column::new("data_type", DataType::Text),
            Column::new("is_nullable", DataType::Text),
            Column::new("ordinal_position", DataType::Int4),
            Column::new("is_pk", DataType::Boolean),
        ]);

        let db = match &self.database {
            Some(db) => db,
            None => return Ok((schema, vec![])),
        };

        // Extract table_name filter (e.g., "WHERE table_name = 'my_table'")
        let table_filter = Self::extract_eq_filter(query_lower, "table_name");

        let catalog = db.storage.catalog();

        let tables_to_query: Vec<String> = if let Some(ref filter_name) = table_filter {
            // Query specific table
            if catalog.table_exists(filter_name)? {
                vec![filter_name.clone()]
            } else {
                vec![]
            }
        } else {
            // Query all tables
            catalog.list_tables()?
        };

        let mut rows = Vec::new();
        for table_name in &tables_to_query {
            if let Ok(table_schema) = catalog.get_table_schema(table_name) {
                for (i, col) in table_schema.columns.iter().enumerate() {
                    rows.push(Tuple::new(vec![
                        Value::String(table_name.clone()),
                        Value::String(col.name.clone()),
                        Value::String(col.data_type.to_string()),
                        Value::String(if col.nullable { "YES".to_string() } else { "NO".to_string() }),
                        Value::Int4((i + 1) as i32),
                        Value::Boolean(col.primary_key),
                    ]));
                }
            }
        }

        Ok((schema, rows))
    }

    /// Extract a LIKE filter value from a query
    /// E.g., "table_name LIKE 'tenant_xyz__%'" -> Some("tenant_xyz__%")
    fn extract_like_filter(query: &str, column: &str) -> Option<String> {
        let pattern = format!("{} like '", column);
        if let Some(start) = query.find(&pattern) {
            let after = &query[start + pattern.len()..];
            if let Some(end) = after.find('\'') {
                return Some(after[..end].to_string());
            }
        }
        None
    }

    /// Extract an equality filter value from a query
    /// E.g., "table_name = 'my_table'" -> Some("my_table")
    fn extract_eq_filter(query: &str, column: &str) -> Option<String> {
        let pattern = format!("{} = '", column);
        if let Some(start) = query.find(&pattern) {
            let after = &query[start + pattern.len()..];
            if let Some(end) = after.find('\'') {
                return Some(after[..end].to_string());
            }
        }
        None
    }

    /// Apply column projection based on the SELECT clause
    /// Parses "SELECT col1, col2 FROM ..." and returns only the requested columns
    /// Returns all columns for "SELECT *" or if parsing fails
    fn project_columns(query_lower: &str, schema: Schema, rows: Vec<Tuple>) -> (Schema, Vec<Tuple>) {
        // Extract SELECT column list
        let select_cols = Self::parse_select_columns(query_lower);

        // If no specific columns requested (SELECT * or parse failure), return all
        if select_cols.is_empty() {
            return (schema, rows);
        }

        // Build index map: for each requested column, find its position in the full schema
        let col_indices: Vec<usize> = select_cols
            .iter()
            .filter_map(|requested| {
                schema.columns.iter().position(|c| c.name == *requested)
            })
            .collect();

        // If no columns matched, return all (safety fallback)
        if col_indices.is_empty() {
            return (schema, rows);
        }

        // Build projected schema
        let projected_schema = Schema::new(
            // Safety: col_indices validated against schema.columns.len() above
            #[allow(clippy::indexing_slicing)]
            col_indices.iter().map(|&i| schema.columns[i].clone()).collect()
        );

        // Build projected rows
        let projected_rows = rows
            .into_iter()
            .map(|row| {
                let values: Vec<Value> = col_indices
                    .iter()
                    .map(|&i| {
                        row.values.get(i).cloned().unwrap_or(Value::Null)
                    })
                    .collect();
                Tuple::new(values)
            })
            .collect();

        (projected_schema, projected_rows)
    }

    /// Parse SELECT column list from a query string
    /// Returns empty vec for "SELECT *" or if parsing fails
    fn parse_select_columns(query_lower: &str) -> Vec<String> {
        // Find "select" and "from" positions
        let select_pos = match query_lower.find("select") {
            Some(pos) => pos + 6, // skip "select"
            None => return vec![],
        };
        let from_pos = match query_lower.find(" from ") {
            Some(pos) => pos,
            None => return vec![],
        };

        if select_pos >= from_pos {
            return vec![];
        }

        let col_list = query_lower[select_pos..from_pos].trim();

        // SELECT * returns all columns
        if col_list == "*" {
            return vec![];
        }

        // Split by comma, trim, and collect column names
        col_list
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Simple SQL LIKE pattern matching (supports % and _ wildcards)
    fn sql_like_match(text: &str, pattern: &str) -> bool {
        let t_chars: Vec<char> = text.chars().collect();
        let p_chars: Vec<char> = pattern.chars().collect();

        Self::like_match_recursive(&t_chars, &p_chars, 0, 0)
    }

    #[allow(clippy::indexing_slicing)] // Safety: pi/ti bounds checked at function entry and before use
    fn like_match_recursive(text: &[char], pattern: &[char], ti: usize, pi: usize) -> bool {
        if pi == pattern.len() {
            return ti == text.len();
        }

        match pattern[pi] {
            '%' => {
                // % matches zero or more characters
                for i in ti..=text.len() {
                    if Self::like_match_recursive(text, pattern, i, pi + 1) {
                        return true;
                    }
                }
                false
            }
            '_' => {
                // _ matches exactly one character
                if ti < text.len() {
                    Self::like_match_recursive(text, pattern, ti + 1, pi + 1)
                } else {
                    false
                }
            }
            c => {
                if ti < text.len() && text[ti] == c {
                    Self::like_match_recursive(text, pattern, ti + 1, pi + 1)
                } else {
                    false
                }
            }
        }
    }

    /// Return PostgreSQL-compatible version string
    fn query_version(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("version", DataType::Text),
        ]);
        let row = Tuple::new(vec![Value::String(format!(
            "PostgreSQL 16.0 (HeliosDB Nano {})",
            env!("CARGO_PKG_VERSION")
        ))]);
        Ok((schema, vec![row]))
    }

    /// Return current schema (always "public")
    fn query_current_schema() -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("current_schema", DataType::Text),
        ]);
        let row = Tuple::new(vec![Value::String("public".to_string())]);
        Ok((schema, vec![row]))
    }

    /// Return current database name
    fn query_current_database() -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("current_database", DataType::Text),
        ]);
        let row = Tuple::new(vec![Value::String("heliosdb".to_string())]);
        Ok((schema, vec![row]))
    }

    /// Return current user
    fn query_current_user() -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("current_user", DataType::Text),
        ]);
        let row = Tuple::new(vec![Value::String("heliosdb".to_string())]);
        Ok((schema, vec![row]))
    }

    /// Query pg_type (type information)
    fn query_pg_type(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("oid", DataType::Int4),
            Column::new("typname", DataType::Text),
            Column::new("typnamespace", DataType::Int4),
            Column::new("typlen", DataType::Int2),
            Column::new("typtype", DataType::Text),
        ]);

        let rows = vec![
            // Common types
            Tuple::new(vec![
                Value::Int4(16), Value::String("bool".to_string()), Value::Int4(11),
                Value::Int2(1), Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(20), Value::String("int8".to_string()), Value::Int4(11),
                Value::Int2(8), Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(21), Value::String("int2".to_string()), Value::Int4(11),
                Value::Int2(2), Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(23), Value::String("int4".to_string()), Value::Int4(11),
                Value::Int2(4), Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(25), Value::String("text".to_string()), Value::Int4(11),
                Value::Int2(-1), Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(700), Value::String("float4".to_string()), Value::Int4(11),
                Value::Int2(4), Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(701), Value::String("float8".to_string()), Value::Int4(11),
                Value::Int2(8), Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(1043), Value::String("varchar".to_string()), Value::Int4(11),
                Value::Int2(-1), Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(1114), Value::String("timestamp".to_string()), Value::Int4(11),
                Value::Int2(8), Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(2950), Value::String("uuid".to_string()), Value::Int4(11),
                Value::Int2(16), Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(114), Value::String("json".to_string()), Value::Int4(11),
                Value::Int2(-1), Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(3802), Value::String("jsonb".to_string()), Value::Int4(11),
                Value::Int2(-1), Value::String("b".to_string()),
            ]),
        ];

        Ok((schema, rows))
    }

    /// Query pg_class (relation/table information) - returns real tables from catalog
    fn query_pg_class(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("oid", DataType::Int4),
            Column::new("relname", DataType::Text),
            Column::new("relnamespace", DataType::Int4),
            Column::new("relkind", DataType::Text),
            Column::new("relowner", DataType::Int4),
        ]);

        let db = match &self.database {
            Some(db) => db,
            None => return Ok((schema, vec![])),
        };

        let catalog = db.storage.catalog();
        let table_names = catalog.list_tables()?;

        let mut rows = Vec::new();
        for (i, name) in table_names.iter().enumerate() {
            rows.push(Tuple::new(vec![
                Value::Int4((16384 + i) as i32), // Start OIDs at 16384 (user tables)
                Value::String(name.clone()),
                Value::Int4(2200), // public namespace
                Value::String("r".to_string()), // regular table
                Value::Int4(10), // owner
            ]));
        }

        Ok((schema, rows))
    }

    /// Query pg_namespace (schema information)
    fn query_pg_namespace(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("oid", DataType::Int4),
            Column::new("nspname", DataType::Text),
            Column::new("nspowner", DataType::Int4),
        ]);

        let rows = vec![
            Tuple::new(vec![
                Value::Int4(11),
                Value::String("pg_catalog".to_string()),
                Value::Int4(10),
            ]),
            Tuple::new(vec![
                Value::Int4(2200),
                Value::String("public".to_string()),
                Value::Int4(10),
            ]),
        ];

        Ok((schema, rows))
    }

    /// Query pg_database (database information)
    fn query_pg_database(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("oid", DataType::Int4),
            Column::new("datname", DataType::Text),
            Column::new("datdba", DataType::Int4),
            Column::new("encoding", DataType::Int4),
        ]);

        // Always include the implicit `heliosdb` system database. Then
        // append every tenant registered via `CREATE DATABASE` (the
        // v3.25 wrap of the multi-tenant API). Without this, `\l` and
        // every ORM that calls `pg_database` see only the default DB
        // even after `CREATE DATABASE foo` succeeded — KanttBan #16
        // partial fix against v3.28.0.
        let mut rows = vec![
            Tuple::new(vec![
                Value::Int4(1),
                Value::String("heliosdb".to_string()),
                Value::Int4(10),
                Value::Int4(6), // UTF8
            ]),
        ];
        if let Some(db) = self.database.as_ref() {
            for (i, t) in db.tenant_manager.list_tenants().iter().enumerate() {
                // Skip the implicit system database — already in the list.
                if t.name.eq_ignore_ascii_case("heliosdb") || t.name.eq_ignore_ascii_case("postgres") {
                    continue;
                }
                rows.push(Tuple::new(vec![
                    Value::Int4((100 + i) as i32),
                    Value::String(t.name.clone()),
                    Value::Int4(10),
                    Value::Int4(6),
                ]));
            }
        }

        Ok((schema, rows))
    }

    /// Query pg_settings (configuration parameters)
    fn query_pg_settings(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("name", DataType::Text),
            Column::new("setting", DataType::Text),
            Column::new("unit", DataType::Text),
            Column::new("category", DataType::Text),
        ]);

        let rows = vec![
            Tuple::new(vec![
                Value::String("server_version".to_string()),
                Value::String("17.0".to_string()),
                Value::Null,
                Value::String("Preset Options".to_string()),
            ]),
            Tuple::new(vec![
                Value::String("server_encoding".to_string()),
                Value::String("UTF8".to_string()),
                Value::Null,
                Value::String("Preset Options".to_string()),
            ]),
            Tuple::new(vec![
                Value::String("client_encoding".to_string()),
                Value::String("UTF8".to_string()),
                Value::Null,
                Value::String("Client Connection Defaults".to_string()),
            ]),
            Tuple::new(vec![
                Value::String("max_connections".to_string()),
                Value::String("100".to_string()),
                Value::Null,
                Value::String("Connections and Authentication".to_string()),
            ]),
        ];

        Ok((schema, rows))
    }

    /// Query pg_attribute (column information) - returns real column data from catalog
    fn query_pg_attribute(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("attrelid", DataType::Int4),
            Column::new("attname", DataType::Text),
            Column::new("atttypid", DataType::Int4),
            Column::new("attnum", DataType::Int2),
            Column::new("attlen", DataType::Int2),
        ]);

        let db = match &self.database {
            Some(db) => db,
            None => return Ok((schema, vec![])),
        };

        let storage_catalog = db.storage.catalog();
        let table_names = storage_catalog.list_tables()?;

        let mut rows = Vec::new();
        for (ti, table_name) in table_names.iter().enumerate() {
            let oid = (16384 + ti) as i32;
            if let Ok(table_schema) = storage_catalog.get_table_schema(table_name) {
                for (ci, col) in table_schema.columns.iter().enumerate() {
                    let type_oid = Self::datatype_to_oid(&col.data_type);
                    let type_len = Self::datatype_to_len(&col.data_type);
                    rows.push(Tuple::new(vec![
                        Value::Int4(oid),
                        Value::String(col.name.clone()),
                        Value::Int4(type_oid),
                        Value::Int2((ci + 1) as i16),
                        Value::Int2(type_len),
                    ]));
                }
            }
        }

        Ok((schema, rows))
    }

    /// Map DataType to PostgreSQL type OID
    fn datatype_to_oid(dt: &DataType) -> i32 {
        match dt {
            DataType::Boolean => 16,
            DataType::Int2 => 21,
            DataType::Int4 => 23,
            DataType::Int8 => 20,
            DataType::Float4 => 700,
            DataType::Float8 => 701,
            DataType::Numeric => 1700,
            DataType::Varchar(_) => 1043,
            DataType::Text => 25,
            DataType::Char(_) => 1042,
            DataType::Bytea => 17,
            DataType::Date => 1082,
            DataType::Time => 1083,
            DataType::Timestamp => 1114,
            DataType::Timestamptz => 1184,
            DataType::Interval => 1186,
            DataType::Uuid => 2950,
            DataType::Json => 114,
            DataType::Jsonb => 3802,
            DataType::Array(_) => 2277,
            DataType::Vector(_) => 25, // stored as text
        }
    }

    /// Detect the canonical queries that `psql` sends for its meta-commands
    /// (`\dt`, `\d table`, `\di`, `\dn`, `\du`, `\l`) and synthesise a shaped
    /// response. Returns `Ok(None)` if the query doesn't match any known
    /// psql signature — the caller should then fall through to the generic
    /// catalog handler.
    fn try_psql_metacommand(&self, q: &str) -> Result<Option<(Schema, Vec<Tuple>)>> {
        let db = match &self.database {
            Some(db) => db,
            None => return Ok(None),
        };
        let catalog = db.storage.catalog();

        // ---- \d <name> first sub-query: relation OID lookup ---------------------
        // psql resolves the target with a regex match:
        //
        //   SELECT c.oid, n.nspname, c.relname
        //   FROM pg_catalog.pg_class c
        //     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
        //   WHERE c.relname OPERATOR(pg_catalog.~) '^(<name>)$' COLLATE pg_catalog.default
        //     AND pg_catalog.pg_table_is_visible(c.oid)
        //   ORDER BY 2, 3;
        //
        // The 5-col query_pg_class fallback returned every table, so
        // psql then iterated `\d` over each one in turn. Filter to
        // exactly the matching relation here (KanttBan #7 follow-up,
        // v3.30.1 smoke).
        if q.contains("operator(pg_catalog.~)")
            && q.contains("c.oid")
            && q.contains("c.relname")
            && q.contains("pg_table_is_visible")
        {
            let schema = Schema::new(vec![
                Column::new("oid", DataType::Int4),
                Column::new("nspname", DataType::Text),
                Column::new("relname", DataType::Text),
            ]);
            let pat = Self::extract_psql_regex_relname(q);
            let mut rows = Vec::new();
            for (ti, name) in catalog.list_tables()?.iter().enumerate() {
                if let Some(ref p) = pat {
                    if name != p { continue; }
                }
                rows.push(Tuple::new(vec![
                    Value::Int4((16384 + ti) as i32),
                    Value::String("public".into()),
                    Value::String(name.clone()),
                ]));
            }
            return Ok(Some((schema, rows)));
        }

        // ---- \l (list databases) ------------------------------------------------
        // psql sends SELECT d.datname ... FROM pg_catalog.pg_database d LEFT JOIN ...
        if q.contains("pg_database") && q.contains("pg_catalog.pg_database") && q.contains("d.datname") {
            let schema = Schema::new(vec![
                Column::new("Name", DataType::Text),
                Column::new("Owner", DataType::Text),
                Column::new("Encoding", DataType::Text),
                Column::new("Collate", DataType::Text),
                Column::new("Ctype", DataType::Text),
                Column::new("Access privileges", DataType::Text),
            ]);
            let rows = vec![Tuple::new(vec![
                Value::String("heliosdb".into()),
                Value::String("heliosdb".into()),
                Value::String("UTF8".into()),
                Value::String("C.UTF-8".into()),
                Value::String("C.UTF-8".into()),
                Value::Null,
            ])];
            return Ok(Some((schema, rows)));
        }

        // ---- \du / \dg (list roles) --------------------------------------------
        // psql sends a SELECT of 11 columns from pg_catalog.pg_roles.
        // Mirror its exact shape so psql's client-side formatter accepts it.
        if q.contains("pg_catalog.pg_roles") && q.contains("rolname")
            && q.contains("rolsuper")
        {
            let schema = Schema::new(vec![
                Column::new("rolname", DataType::Text),
                Column::new("rolsuper", DataType::Boolean),
                Column::new("rolinherit", DataType::Boolean),
                Column::new("rolcreaterole", DataType::Boolean),
                Column::new("rolcreatedb", DataType::Boolean),
                Column::new("rolcanlogin", DataType::Boolean),
                Column::new("rolconnlimit", DataType::Int4),
                Column::new("rolvaliduntil", DataType::Text),
                Column::new("memberof", DataType::Text),
                Column::new("rolreplication", DataType::Boolean),
                Column::new("rolbypassrls", DataType::Boolean),
            ]);
            let role = |name: &str| Tuple::new(vec![
                Value::String(name.into()),
                Value::Boolean(true),  // rolsuper
                Value::Boolean(true),  // rolinherit
                Value::Boolean(true),  // rolcreaterole
                Value::Boolean(true),  // rolcreatedb
                Value::Boolean(true),  // rolcanlogin
                Value::Int4(-1),       // rolconnlimit (unlimited)
                Value::Null,           // rolvaliduntil
                Value::String("{}".into()), // memberof
                Value::Boolean(true),  // rolreplication
                Value::Boolean(true),  // rolbypassrls
            ]);
            let rows = vec![role("postgres"), role("helios")];
            return Ok(Some((schema, rows)));
        }

        // ---- \dn (list schemas) -------------------------------------------------
        // Must NOT match \dt / \di / \d — those also JOIN pg_namespace.
        if q.contains("pg_catalog.pg_namespace")
            && q.contains("nspname")
            && q.contains("pg_get_userbyid")
            && !q.contains("pg_catalog.pg_class")
            && !q.contains("pg_class c")
        {
            let schema = Schema::new(vec![
                Column::new("Name", DataType::Text),
                Column::new("Owner", DataType::Text),
            ]);
            let rows = vec![Tuple::new(vec![
                Value::String("public".into()),
                Value::String("heliosdb".into()),
            ])];
            return Ok(Some((schema, rows)));
        }

        // ---- \dt / \d (list tables) --------------------------------------------
        // Signature: SELECT n.nspname, c.relname, ..., pg_get_userbyid(c.relowner)
        // FROM pg_catalog.pg_class c LEFT JOIN pg_catalog.pg_namespace n ...
        // WHERE c.relkind IN ('r', ...)
        let is_dt = q.contains("pg_catalog.pg_class")
            && q.contains("pg_catalog.pg_namespace")
            && q.contains("pg_get_userbyid")
            && (q.contains("'r'") || q.contains("relkind in ('r"))
            && !q.contains("pg_index ");
        if is_dt {
            let schema = Schema::new(vec![
                Column::new("Schema", DataType::Text),
                Column::new("Name", DataType::Text),
                Column::new("Type", DataType::Text),
                Column::new("Owner", DataType::Text),
            ]);
            let mut rows = Vec::new();
            let name_filter = Self::extract_psql_relname_filter(q);
            for name in catalog.list_tables()? {
                if let Some(ref pat) = name_filter {
                    if !Self::sql_like_match(&name, pat) { continue; }
                }
                rows.push(Tuple::new(vec![
                    Value::String("public".into()),
                    Value::String(name),
                    Value::String("table".into()),
                    Value::String("heliosdb".into()),
                ]));
            }
            return Ok(Some((schema, rows)));
        }

        // ---- \d table_name (KanttBan #7, v3.30.1 follow-up) ------------
        // The first query psql sends for `\d <name>` after resolving
        // the relation OID is a 15-column pg_class header pull:
        //
        //   SELECT c.relchecks, c.relkind, c.relhasindex, c.relhasrules,
        //          c.relhastriggers, c.relrowsecurity, c.relforcerowsecurity,
        //          false AS relhasoids, c.relispartition, '',
        //          c.reltablespace,
        //          CASE WHEN c.reloftype = 0 THEN '' ELSE … END,
        //          c.relpersistence, c.relreplident, am.amname
        //   FROM pg_catalog.pg_class c
        //     LEFT JOIN pg_catalog.pg_class tc ON (c.reltoastrelid = tc.oid)
        //     LEFT JOIN pg_catalog.pg_am am ON (c.relam = am.oid)
        //   WHERE c.oid = '<oid>';
        //
        // The generic `pg_class` matcher returns only 5 columns, so
        // psql's libpq errors with "column number 5 is out of range
        // 0..4" — the exact message KanttBan reported in the v3.30
        // re-test. We special-case the shape and emit the 15 columns
        // psql's client formatter expects.
        if q.contains("pg_catalog.pg_class")
            && q.contains("relchecks")
            && q.contains("relhasindex")
            && q.contains("c.oid = '")
        {
            let schema = Schema::new(vec![
                Column::new("relchecks", DataType::Int2),
                Column::new("relkind", DataType::Char(1)),
                Column::new("relhasindex", DataType::Boolean),
                Column::new("relhasrules", DataType::Boolean),
                Column::new("relhastriggers", DataType::Boolean),
                Column::new("relrowsecurity", DataType::Boolean),
                Column::new("relforcerowsecurity", DataType::Boolean),
                Column::new("relhasoids", DataType::Boolean),
                Column::new("relispartition", DataType::Boolean),
                Column::new("reltoasttable", DataType::Text),
                Column::new("reltablespace", DataType::Int4),
                Column::new("reloftype", DataType::Text),
                Column::new("relpersistence", DataType::Char(1)),
                Column::new("relreplident", DataType::Char(1)),
                Column::new("amname", DataType::Text),
            ]);
            let target_oid = Self::extract_relchecks_oid(q);
            let table_names = catalog.list_tables()?;
            let mut rows = Vec::new();
            for (ti, name) in table_names.iter().enumerate() {
                let table_oid = (16384 + ti) as i32;
                if let Some(t) = target_oid {
                    if t != table_oid { continue; }
                }
                let has_index = catalog
                    .get_table_schema(name)
                    .map(|s| s.columns.iter().any(|c| c.primary_key || c.unique))
                    .unwrap_or(false);
                rows.push(Tuple::new(vec![
                    Value::Int2(0),                   // relchecks
                    Value::String("r".into()),        // relkind = ordinary table
                    Value::Boolean(has_index),        // relhasindex
                    Value::Boolean(false),            // relhasrules
                    Value::Boolean(false),            // relhastriggers
                    Value::Boolean(false),            // relrowsecurity
                    Value::Boolean(false),            // relforcerowsecurity
                    Value::Boolean(false),            // relhasoids
                    Value::Boolean(false),            // relispartition
                    Value::String(String::new()),     // (literal '' from psql query)
                    Value::Int4(0),                   // reltablespace = pg_default
                    Value::String(String::new()),     // CASE reloftype → ''
                    Value::String("p".into()),        // relpersistence = permanent
                    Value::String("d".into()),        // relreplident = default
                    Value::String("heap".into()),     // am.amname
                ]));
            }
            return Ok(Some((schema, rows)));
        }

        // ---- \d table_name (KanttBan #7, deferred from v3.28) ----------
        // psql's `\d <name>` sends several catalog queries; the one that
        // libpq error-rejects with "column number 5 is out of range 0..4"
        // is the per-column descriptor:
        //
        //   SELECT a.attname,
        //          pg_catalog.format_type(a.atttypid, a.atttypmod),
        //          (default-expr subquery),
        //          a.attnotnull,
        //          (collation subquery),
        //          a.attidentity,
        //          a.attgenerated
        //   FROM pg_catalog.pg_attribute a
        //   WHERE a.attrelid = '<oid>' AND a.attnum > 0 AND NOT a.attisdropped
        //   ORDER BY a.attnum;
        //
        // Match on the telltale `attnum > 0` + `attisdropped` combination
        // and emit the 7-column shape filled from our internal schema —
        // identity / generated / collation default to empty since Nano
        // doesn't expose them.
        //
        // KanttBan #7 follow-up (v3.30.1 smoke): the previous matcher
        // false-fired on `pg_statistic_ext` queries which JOIN
        // `pg_catalog.pg_attribute` in a subquery. Tightened to require
        // the OUTER `FROM pg_catalog.pg_attribute a` plus the
        // `a.attrelid = '<oid>'` WHERE predicate that only the
        // descriptor query emits.
        if q.contains("from pg_catalog.pg_attribute a")
            && q.contains("a.attrelid = '")
            && q.contains("a.attnum > 0")
            && q.contains("attisdropped")
        {
            let schema = Schema::new(vec![
                Column::new("attname", DataType::Text),
                Column::new("format_type", DataType::Text),
                Column::new("default_expr", DataType::Text),
                Column::new("attnotnull", DataType::Boolean),
                Column::new("collation", DataType::Text),
                Column::new("attidentity", DataType::Char(1)),
                Column::new("attgenerated", DataType::Char(1)),
            ]);
            // Extract the OID literal so we can find the matching table.
            // psql formats it as `a.attrelid = '<oid>'`. Any single OID
            // literal in the query is the target.
            let oid_literal = Self::extract_attrelid(q);
            let table_names = catalog.list_tables()?;
            let mut rows = Vec::new();
            for (ti, table_name) in table_names.iter().enumerate() {
                let table_oid = (16384 + ti) as i32;
                if let Some(target_oid) = oid_literal {
                    if target_oid != table_oid {
                        continue;
                    }
                }
                if let Ok(table_schema) = catalog.get_table_schema(table_name) {
                    for col in &table_schema.columns {
                        rows.push(Tuple::new(vec![
                            Value::String(col.name.clone()),
                            Value::String(Self::pg_format_type(&col.data_type)),
                            col.default_expr.as_ref()
                                .map(|d| Value::String(d.clone()))
                                .unwrap_or(Value::Null),
                            Value::Boolean(!col.nullable),
                            Value::Null, // collation
                            Value::String(if col.primary_key { "d".to_string() } else { "".to_string() }),
                            Value::String(String::new()), // attgenerated — Nano has no GENERATED columns
                        ]));
                    }
                }
            }
            return Ok(Some((schema, rows)));
        }

        // ---- \d <name> index list (12 columns) -----------------------------
        // psql sends:
        //
        //   SELECT c2.relname, i.indisprimary, i.indisunique, i.indisclustered,
        //          i.indisvalid, pg_catalog.pg_get_indexdef(...),
        //          pg_catalog.pg_get_constraintdef(con.oid, true), contype,
        //          condeferrable, condeferred, i.indisreplident, c2.reltablespace
        //   FROM pg_catalog.pg_class c, pg_catalog.pg_class c2,
        //        pg_catalog.pg_index i
        //     LEFT JOIN pg_catalog.pg_constraint con ON …
        //   WHERE c.oid = '<oid>' AND c.oid = i.indrelid AND i.indexrelid = c2.oid
        //
        // The generic pg_index handler returns 5 cols; psql expected 12,
        // hence "column number 7 is out of range 0..4" on the v3.30.1
        // smoke (KanttBan #7 follow-up). Emit one row per PRIMARY KEY
        // and per UNIQUE column on the target relation.
        if q.contains("pg_get_indexdef")
            && q.contains("pg_get_constraintdef")
            && q.contains("c2.relname")
        {
            let schema = Schema::new(vec![
                Column::new("relname", DataType::Text),
                Column::new("indisprimary", DataType::Boolean),
                Column::new("indisunique", DataType::Boolean),
                Column::new("indisclustered", DataType::Boolean),
                Column::new("indisvalid", DataType::Boolean),
                Column::new("indexdef", DataType::Text),
                Column::new("constraintdef", DataType::Text),
                Column::new("contype", DataType::Char(1)),
                Column::new("condeferrable", DataType::Boolean),
                Column::new("condeferred", DataType::Boolean),
                Column::new("indisreplident", DataType::Boolean),
                Column::new("reltablespace", DataType::Int4),
            ]);
            let target_oid = Self::extract_relchecks_oid(q);
            let mut rows = Vec::new();
            for (ti, name) in catalog.list_tables()?.iter().enumerate() {
                let table_oid = (16384 + ti) as i32;
                if let Some(t) = target_oid {
                    if t != table_oid { continue; }
                }
                if let Ok(ts) = catalog.get_table_schema(name) {
                    let pk_cols: Vec<&str> = ts.columns.iter()
                        .filter(|c| c.primary_key)
                        .map(|c| c.name.as_str())
                        .collect();
                    if !pk_cols.is_empty() {
                        let cols = pk_cols.join(", ");
                        rows.push(Tuple::new(vec![
                            Value::String(format!("{}_pkey", name)),
                            Value::Boolean(true),  // indisprimary
                            Value::Boolean(true),  // indisunique
                            Value::Boolean(false), // indisclustered
                            Value::Boolean(true),  // indisvalid
                            Value::String(format!(
                                "CREATE UNIQUE INDEX {}_pkey ON public.{} USING btree ({})",
                                name, name, cols,
                            )),
                            Value::String(format!("PRIMARY KEY ({})", cols)),
                            Value::String("p".into()),
                            Value::Boolean(false),
                            Value::Boolean(false),
                            Value::Boolean(false),
                            Value::Int4(0),
                        ]));
                    }
                    for col in &ts.columns {
                        if col.unique && !col.primary_key {
                            rows.push(Tuple::new(vec![
                                Value::String(format!("{}_{}_key", name, col.name)),
                                Value::Boolean(false),
                                Value::Boolean(true),
                                Value::Boolean(false),
                                Value::Boolean(true),
                                Value::String(format!(
                                    "CREATE UNIQUE INDEX {0}_{1}_key ON public.{0} USING btree ({1})",
                                    name, col.name,
                                )),
                                Value::String(format!("UNIQUE ({})", col.name)),
                                Value::String("u".into()),
                                Value::Boolean(false),
                                Value::Boolean(false),
                                Value::Boolean(false),
                                Value::Int4(0),
                            ]));
                        }
                    }
                }
            }
            return Ok(Some((schema, rows)));
        }

        // ---- \di (list indexes) ------------------------------------------------
        let is_di = q.contains("pg_catalog.pg_class")
            && q.contains("pg_catalog.pg_namespace")
            && q.contains("pg_get_userbyid")
            && (q.contains("'i'") || q.contains("relkind in ('i"));
        if is_di {
            let schema = Schema::new(vec![
                Column::new("Schema", DataType::Text),
                Column::new("Name", DataType::Text),
                Column::new("Type", DataType::Text),
                Column::new("Owner", DataType::Text),
                Column::new("Table", DataType::Text),
            ]);
            let mut rows = Vec::new();
            for name in catalog.list_tables()? {
                if let Ok(ts) = catalog.get_table_schema(&name) {
                    if ts.columns.iter().any(|c| c.primary_key) {
                        rows.push(Tuple::new(vec![
                            Value::String("public".into()),
                            Value::String(format!("{}_pkey", name)),
                            Value::String("index".into()),
                            Value::String("heliosdb".into()),
                            Value::String(name.clone()),
                        ]));
                    }
                    for col in &ts.columns {
                        if col.unique && !col.primary_key {
                            rows.push(Tuple::new(vec![
                                Value::String("public".into()),
                                Value::String(format!("{}_{}_key", name, col.name)),
                                Value::String("index".into()),
                                Value::String("heliosdb".into()),
                                Value::String(name.clone()),
                            ]));
                        }
                    }
                }
            }
            return Ok(Some((schema, rows)));
        }

        Ok(None)
    }

    /// Extract the table OID literal from psql's
    /// `WHERE a.attrelid = '<oid>'` shape used by `\d <table>`.
    fn extract_attrelid(q: &str) -> Option<i32> {
        let marker = "attrelid = '";
        let start = q.find(marker)?;
        let after = q.get(start + marker.len()..)?;
        let end = after.find('\'')?;
        after.get(..end)?.parse::<i32>().ok()
    }

    /// Extract the table OID literal from psql's
    /// `WHERE c.oid = '<oid>'` shape used by `\d <table>`'s
    /// 15-column pg_class header pull.
    fn extract_relchecks_oid(q: &str) -> Option<i32> {
        let marker = "c.oid = '";
        let start = q.find(marker)?;
        let after = q.get(start + marker.len()..)?;
        let end = after.find('\'')?;
        after.get(..end)?.parse::<i32>().ok()
    }

    /// Extract the relation name from psql's `\d <name>` regex-match
    /// shape `c.relname OPERATOR(pg_catalog.~) '^(<name>)$' COLLATE …`.
    /// Returns None when the regex isn't a plain anchored name (e.g.
    /// the user passed a pattern with metacharacters), in which case
    /// the caller falls back to "return all tables".
    fn extract_psql_regex_relname(q: &str) -> Option<String> {
        let marker = "operator(pg_catalog.~) '^(";
        let start = q.find(marker)?;
        let after = q.get(start + marker.len()..)?;
        let end = after.find(")$'")?;
        let name = after.get(..end)?;
        if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            Some(name.to_string())
        } else {
            None
        }
    }

    /// Render a `DataType` in the long form `pg_catalog.format_type`
    /// produces for psql `\d`. Lossy but human-readable enough for the
    /// describe panel; `integer` / `text` / `timestamp without time zone`
    /// match how stock PG renders the corresponding columns.
    fn pg_format_type(dt: &DataType) -> String {
        match dt {
            DataType::Boolean => "boolean".into(),
            DataType::Int2 => "smallint".into(),
            DataType::Int4 => "integer".into(),
            DataType::Int8 => "bigint".into(),
            DataType::Float4 => "real".into(),
            DataType::Float8 => "double precision".into(),
            DataType::Numeric => "numeric".into(),
            DataType::Varchar(n) => match n {
                Some(len) => format!("character varying({len})"),
                None => "character varying".into(),
            },
            DataType::Char(n) => format!("character({n})"),
            DataType::Text => "text".into(),
            DataType::Bytea => "bytea".into(),
            DataType::Date => "date".into(),
            DataType::Time => "time without time zone".into(),
            DataType::Timestamp => "timestamp without time zone".into(),
            DataType::Timestamptz => "timestamp with time zone".into(),
            DataType::Interval => "interval".into(),
            DataType::Uuid => "uuid".into(),
            DataType::Json => "json".into(),
            DataType::Jsonb => "jsonb".into(),
            DataType::Array(inner) => format!("{}[]", Self::pg_format_type(inner)),
            DataType::Vector(n) => format!("vector({n})"),
        }
    }

    /// Extract a `relname ~ '^(pattern)$'` filter from a psql \d query.
    fn extract_psql_relname_filter(q: &str) -> Option<String> {
        let marker = "relname ~ '^(";
        if let Some(start) = q.find(marker) {
            let after = q.get(start + marker.len()..)?;
            if let Some(end) = after.find(")$") {
                let pat = after.get(..end)?;
                // Convert regex anchor to LIKE-style pattern (approx): leave as-is for exact match.
                return Some(pat.to_string());
            }
        }
        None
    }

    /// Check whether a query touches any pg_catalog table we emulate.
    fn is_catalog_query(q: &str) -> bool {
        const MARKERS: &[&str] = &[
            "pg_catalog", "pg_type", "pg_class", "pg_namespace", "pg_attribute",
            "pg_database", "pg_index", "pg_indexes", "pg_sequences", "pg_tables",
            "pg_views", "pg_constraint", "pg_description", "pg_roles", "pg_user",
            "pg_proc", "pg_settings", "pg_policies", "pg_matviews",
        ];
        MARKERS.iter().any(|m| q.contains(m))
    }

    /// Query pg_index — per-table primary key and unique indexes.
    /// Columns: indexrelid, indrelid, indisunique, indisprimary, indkey.
    fn query_pg_index(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("indexrelid", DataType::Int4),
            Column::new("indrelid", DataType::Int4),
            Column::new("indisunique", DataType::Boolean),
            Column::new("indisprimary", DataType::Boolean),
            Column::new("indkey", DataType::Text),
        ]);
        let db = match &self.database {
            Some(db) => db,
            None => return Ok((schema, vec![])),
        };
        let catalog = db.storage.catalog();
        let tables = catalog.list_tables()?;
        let mut rows = Vec::new();
        for (ti, name) in tables.iter().enumerate() {
            let table_oid = (16384 + ti) as i32;
            if let Ok(tschema) = catalog.get_table_schema(name) {
                // Primary key: any column flagged primary_key
                let pk_cols: Vec<String> = tschema.columns.iter()
                    .enumerate()
                    .filter(|(_, c)| c.primary_key)
                    .map(|(i, _)| (i + 1).to_string())
                    .collect();
                if !pk_cols.is_empty() {
                    rows.push(Tuple::new(vec![
                        Value::Int4(table_oid + 100_000), // synthetic index oid
                        Value::Int4(table_oid),
                        Value::Boolean(true),  // indisunique
                        Value::Boolean(true),  // indisprimary
                        Value::String(pk_cols.join(" ")),
                    ]));
                }
                // Unique indexes: any column flagged unique (non-PK)
                for (ci, col) in tschema.columns.iter().enumerate() {
                    if col.unique && !col.primary_key {
                        rows.push(Tuple::new(vec![
                            Value::Int4(table_oid + 100_000 + ci as i32 + 1),
                            Value::Int4(table_oid),
                            Value::Boolean(true),
                            Value::Boolean(false),
                            Value::String((ci + 1).to_string()),
                        ]));
                    }
                }
            }
        }
        Ok((schema, rows))
    }

    /// Query pg_indexes (view) — 5 columns (schemaname, tablename, indexname, tablespace, indexdef).
    fn query_pg_indexes(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("schemaname", DataType::Text),
            Column::new("tablename", DataType::Text),
            Column::new("indexname", DataType::Text),
            Column::new("tablespace", DataType::Text),
            Column::new("indexdef", DataType::Text),
        ]);
        let db = match &self.database {
            Some(db) => db,
            None => return Ok((schema, vec![])),
        };
        let catalog = db.storage.catalog();
        let tables = catalog.list_tables()?;
        let mut rows = Vec::new();
        for name in &tables {
            if let Ok(tschema) = catalog.get_table_schema(name) {
                let pk_cols: Vec<String> = tschema.columns.iter()
                    .filter(|c| c.primary_key)
                    .map(|c| c.name.clone())
                    .collect();
                if !pk_cols.is_empty() {
                    let idx_name = format!("{}_pkey", name);
                    let def = format!(
                        "CREATE UNIQUE INDEX {} ON public.{} USING btree ({})",
                        idx_name, name, pk_cols.join(", ")
                    );
                    rows.push(Tuple::new(vec![
                        Value::String("public".into()),
                        Value::String(name.clone()),
                        Value::String(idx_name),
                        Value::Null,
                        Value::String(def),
                    ]));
                }
                for col in &tschema.columns {
                    if col.unique && !col.primary_key {
                        let idx_name = format!("{}_{}_key", name, col.name);
                        let def = format!(
                            "CREATE UNIQUE INDEX {} ON public.{} USING btree ({})",
                            idx_name, name, col.name
                        );
                        rows.push(Tuple::new(vec![
                            Value::String("public".into()),
                            Value::String(name.clone()),
                            Value::String(idx_name),
                            Value::Null,
                            Value::String(def),
                        ]));
                    }
                }
            }
        }
        Ok((schema, rows))
    }

    /// Query pg_tables (view) — 5 cols (schemaname, tablename, tableowner, tablespace, hasindexes).
    fn query_pg_tables(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("schemaname", DataType::Text),
            Column::new("tablename", DataType::Text),
            Column::new("tableowner", DataType::Text),
            Column::new("tablespace", DataType::Text),
            Column::new("hasindexes", DataType::Boolean),
        ]);
        let db = match &self.database {
            Some(db) => db,
            None => return Ok((schema, vec![])),
        };
        let tables = db.storage.catalog().list_tables()?;
        let rows = tables.into_iter().map(|t| {
            Tuple::new(vec![
                Value::String("public".into()),
                Value::String(t),
                Value::String("heliosdb".into()),
                Value::Null,
                Value::Boolean(true),
            ])
        }).collect();
        Ok((schema, rows))
    }

    /// Query pg_views (view) — always empty; Nano does not persist view definitions.
    fn query_pg_views(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("schemaname", DataType::Text),
            Column::new("viewname", DataType::Text),
            Column::new("viewowner", DataType::Text),
            Column::new("definition", DataType::Text),
        ]);
        Ok((schema, vec![]))
    }

    /// Query pg_constraint — primary key + unique constraints per table.
    fn query_pg_constraint(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("oid", DataType::Int4),
            Column::new("conname", DataType::Text),
            Column::new("contype", DataType::Text), // 'p' PK, 'u' unique
            Column::new("conrelid", DataType::Int4),
            Column::new("conkey", DataType::Text),
        ]);
        let db = match &self.database {
            Some(db) => db,
            None => return Ok((schema, vec![])),
        };
        let catalog = db.storage.catalog();
        let tables = catalog.list_tables()?;
        let mut rows = Vec::new();
        for (ti, name) in tables.iter().enumerate() {
            let table_oid = (16384 + ti) as i32;
            if let Ok(tschema) = catalog.get_table_schema(name) {
                let pk_cols: Vec<String> = tschema.columns.iter()
                    .enumerate()
                    .filter(|(_, c)| c.primary_key)
                    .map(|(i, _)| (i + 1).to_string())
                    .collect();
                if !pk_cols.is_empty() {
                    rows.push(Tuple::new(vec![
                        Value::Int4(table_oid + 200_000),
                        Value::String(format!("{}_pkey", name)),
                        Value::String("p".into()),
                        Value::Int4(table_oid),
                        Value::String(format!("{{{}}}", pk_cols.join(","))),
                    ]));
                }
                for (ci, col) in tschema.columns.iter().enumerate() {
                    if col.unique && !col.primary_key {
                        rows.push(Tuple::new(vec![
                            Value::Int4(table_oid + 200_000 + ci as i32 + 1),
                            Value::String(format!("{}_{}_key", name, col.name)),
                            Value::String("u".into()),
                            Value::Int4(table_oid),
                            Value::String(format!("{{{}}}", ci + 1)),
                        ]));
                    }
                }
            }
        }
        Ok((schema, rows))
    }

    /// Query pg_roles / pg_user — single admin role.
    fn query_pg_roles(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("oid", DataType::Int4),
            Column::new("rolname", DataType::Text),
            Column::new("rolsuper", DataType::Boolean),
            Column::new("rolcanlogin", DataType::Boolean),
        ]);
        let rows = vec![
            Tuple::new(vec![
                Value::Int4(10),
                Value::String("postgres".into()),
                Value::Boolean(true),
                Value::Boolean(true),
            ]),
            Tuple::new(vec![
                Value::Int4(11),
                Value::String("helios".into()),
                Value::Boolean(true),
                Value::Boolean(true),
            ]),
        ];
        Ok((schema, rows))
    }

    /// information_schema.schemata
    fn query_information_schema_schemata(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("catalog_name", DataType::Text),
            Column::new("schema_name", DataType::Text),
            Column::new("schema_owner", DataType::Text),
        ]);
        let rows = vec![
            Tuple::new(vec![
                Value::String("heliosdb".into()),
                Value::String("public".into()),
                Value::String("heliosdb".into()),
            ]),
            Tuple::new(vec![
                Value::String("heliosdb".into()),
                Value::String("information_schema".into()),
                Value::String("heliosdb".into()),
            ]),
            Tuple::new(vec![
                Value::String("heliosdb".into()),
                Value::String("pg_catalog".into()),
                Value::String("heliosdb".into()),
            ]),
        ];
        Ok((schema, rows))
    }

    /// information_schema.key_column_usage — PK / unique columns.
    fn query_information_schema_key_column_usage(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("constraint_catalog", DataType::Text),
            Column::new("constraint_schema", DataType::Text),
            Column::new("constraint_name", DataType::Text),
            Column::new("table_name", DataType::Text),
            Column::new("column_name", DataType::Text),
            Column::new("ordinal_position", DataType::Int4),
        ]);
        let db = match &self.database {
            Some(db) => db,
            None => return Ok((schema, vec![])),
        };
        let catalog = db.storage.catalog();
        let mut rows = Vec::new();
        for name in catalog.list_tables()? {
            if let Ok(tschema) = catalog.get_table_schema(&name) {
                let mut pos = 1;
                for col in &tschema.columns {
                    if col.primary_key {
                        rows.push(Tuple::new(vec![
                            Value::String("heliosdb".into()),
                            Value::String("public".into()),
                            Value::String(format!("{}_pkey", name)),
                            Value::String(name.clone()),
                            Value::String(col.name.clone()),
                            Value::Int4(pos),
                        ]));
                        pos += 1;
                    } else if col.unique {
                        rows.push(Tuple::new(vec![
                            Value::String("heliosdb".into()),
                            Value::String("public".into()),
                            Value::String(format!("{}_{}_key", name, col.name)),
                            Value::String(name.clone()),
                            Value::String(col.name.clone()),
                            Value::Int4(1),
                        ]));
                    }
                }
            }
        }
        Ok((schema, rows))
    }

    /// information_schema.table_constraints — PK and UNIQUE per table.
    fn query_information_schema_table_constraints(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("constraint_catalog", DataType::Text),
            Column::new("constraint_schema", DataType::Text),
            Column::new("constraint_name", DataType::Text),
            Column::new("table_name", DataType::Text),
            Column::new("constraint_type", DataType::Text),
        ]);
        let db = match &self.database {
            Some(db) => db,
            None => return Ok((schema, vec![])),
        };
        let catalog = db.storage.catalog();
        let mut rows = Vec::new();
        for name in catalog.list_tables()? {
            if let Ok(tschema) = catalog.get_table_schema(&name) {
                if tschema.columns.iter().any(|c| c.primary_key) {
                    rows.push(Tuple::new(vec![
                        Value::String("heliosdb".into()),
                        Value::String("public".into()),
                        Value::String(format!("{}_pkey", name)),
                        Value::String(name.clone()),
                        Value::String("PRIMARY KEY".into()),
                    ]));
                }
                for col in &tschema.columns {
                    if col.unique && !col.primary_key {
                        rows.push(Tuple::new(vec![
                            Value::String("heliosdb".into()),
                            Value::String("public".into()),
                            Value::String(format!("{}_{}_key", name, col.name)),
                            Value::String(name.clone()),
                            Value::String("UNIQUE".into()),
                        ]));
                    }
                }
            }
        }
        Ok((schema, rows))
    }

    /// Extract the view name from an `information_schema.<view>` reference.
    /// Returns the lowercase name on the first match, or `None` if the
    /// query references `information_schema` without naming a view.
    fn information_schema_view_name(q: &str) -> Option<String> {
        let marker = "information_schema.";
        let idx = q.find(marker)?;
        let tail = q.get(idx + marker.len()..)?;
        // Stop at the first non-identifier character.
        let end = tail
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(tail.len());
        let name = tail.get(..end)?.to_string();
        if name.is_empty() { None } else { Some(name) }
    }

    /// Whitelist of SQL-standard `information_schema` view names that Nano
    /// recognises but legitimately doesn't populate. Returns a stable
    /// schema-only response (zero rows) so ORM probes get a well-formed
    /// reply rather than an error.
    fn known_empty_information_schema_view(name: &str) -> Option<(Schema, Vec<Tuple>)> {
        let cols: &[(&str, DataType)] = match name {
            "triggers" => &[
                ("trigger_catalog", DataType::Text),
                ("trigger_schema", DataType::Text),
                ("trigger_name", DataType::Text),
                ("event_manipulation", DataType::Text),
                ("event_object_catalog", DataType::Text),
                ("event_object_schema", DataType::Text),
                ("event_object_table", DataType::Text),
                ("action_statement", DataType::Text),
                ("action_orientation", DataType::Text),
                ("action_timing", DataType::Text),
            ],
            "parameters" => &[
                ("specific_catalog", DataType::Text),
                ("specific_schema", DataType::Text),
                ("specific_name", DataType::Text),
                ("ordinal_position", DataType::Int4),
                ("parameter_mode", DataType::Text),
                ("parameter_name", DataType::Text),
                ("data_type", DataType::Text),
            ],
            "sequences" => &[
                ("sequence_catalog", DataType::Text),
                ("sequence_schema", DataType::Text),
                ("sequence_name", DataType::Text),
                ("data_type", DataType::Text),
                ("start_value", DataType::Text),
                ("minimum_value", DataType::Text),
                ("maximum_value", DataType::Text),
                ("increment", DataType::Text),
            ],
            "domains" => &[
                ("domain_catalog", DataType::Text),
                ("domain_schema", DataType::Text),
                ("domain_name", DataType::Text),
                ("data_type", DataType::Text),
            ],
            "character_sets" => &[
                ("character_set_catalog", DataType::Text),
                ("character_set_schema", DataType::Text),
                ("character_set_name", DataType::Text),
                ("default_collate_name", DataType::Text),
            ],
            "collations" => &[
                ("collation_catalog", DataType::Text),
                ("collation_schema", DataType::Text),
                ("collation_name", DataType::Text),
            ],
            "table_privileges" | "column_privileges" | "usage_privileges" => &[
                ("grantor", DataType::Text),
                ("grantee", DataType::Text),
                ("table_catalog", DataType::Text),
                ("table_schema", DataType::Text),
                ("table_name", DataType::Text),
                ("privilege_type", DataType::Text),
                ("is_grantable", DataType::Text),
            ],
            "role_table_grants" | "role_column_grants" | "role_usage_grants" | "role_routine_grants" => &[
                ("grantor", DataType::Text),
                ("grantee", DataType::Text),
                ("table_catalog", DataType::Text),
                ("table_schema", DataType::Text),
                ("table_name", DataType::Text),
                ("privilege_type", DataType::Text),
                ("is_grantable", DataType::Text),
            ],
            "constraint_column_usage" | "constraint_table_usage" => &[
                ("table_catalog", DataType::Text),
                ("table_schema", DataType::Text),
                ("table_name", DataType::Text),
                ("column_name", DataType::Text),
                ("constraint_catalog", DataType::Text),
                ("constraint_schema", DataType::Text),
                ("constraint_name", DataType::Text),
            ],
            "view_column_usage" | "view_table_usage" => &[
                ("view_catalog", DataType::Text),
                ("view_schema", DataType::Text),
                ("view_name", DataType::Text),
                ("table_catalog", DataType::Text),
                ("table_schema", DataType::Text),
                ("table_name", DataType::Text),
            ],
            "applicable_roles" | "enabled_roles" | "administrable_role_authorizations" => &[
                ("grantee", DataType::Text),
                ("role_name", DataType::Text),
                ("is_grantable", DataType::Text),
            ],
            "element_types" => &[
                ("object_catalog", DataType::Text),
                ("object_schema", DataType::Text),
                ("object_name", DataType::Text),
                ("data_type", DataType::Text),
            ],
            _ => return None,
        };
        let columns = cols.iter().map(|(n, dt)| Column::new(*n, dt.clone())).collect();
        Some((Schema::new(columns), vec![]))
    }

    /// information_schema.routines — SQL-standard schema, zero rows.
    /// Nano supports CREATE FUNCTION but does not currently expose its
    /// runtime function catalog through this view; ORM probes that look
    /// up routine names will see an empty set, which is correct (it
    /// signals "no user-defined routines visible").
    fn query_information_schema_routines() -> (Schema, Vec<Tuple>) {
        let schema = Schema::new(vec![
            Column::new("specific_catalog", DataType::Text),
            Column::new("specific_schema", DataType::Text),
            Column::new("specific_name", DataType::Text),
            Column::new("routine_catalog", DataType::Text),
            Column::new("routine_schema", DataType::Text),
            Column::new("routine_name", DataType::Text),
            Column::new("routine_type", DataType::Text),
            Column::new("data_type", DataType::Text),
            Column::new("type_udt_catalog", DataType::Text),
            Column::new("type_udt_schema", DataType::Text),
            Column::new("type_udt_name", DataType::Text),
            Column::new("routine_body", DataType::Text),
            Column::new("routine_definition", DataType::Text),
            Column::new("external_language", DataType::Text),
            Column::new("is_deterministic", DataType::Text),
            Column::new("security_type", DataType::Text),
        ]);
        (schema, vec![])
    }

    /// information_schema.check_constraints — SQL-standard schema, zero
    /// rows. Nano stores CHECK constraints internally but does not yet
    /// surface them through this view.
    fn query_information_schema_check_constraints() -> (Schema, Vec<Tuple>) {
        let schema = Schema::new(vec![
            Column::new("constraint_catalog", DataType::Text),
            Column::new("constraint_schema", DataType::Text),
            Column::new("constraint_name", DataType::Text),
            Column::new("check_clause", DataType::Text),
        ]);
        (schema, vec![])
    }

    /// information_schema.views — SQL-standard schema, zero rows. Nano
    /// does not persist VIEW definitions, mirroring `pg_views`.
    fn query_information_schema_views() -> (Schema, Vec<Tuple>) {
        let schema = Schema::new(vec![
            Column::new("table_catalog", DataType::Text),
            Column::new("table_schema", DataType::Text),
            Column::new("table_name", DataType::Text),
            Column::new("view_definition", DataType::Text),
            Column::new("check_option", DataType::Text),
            Column::new("is_updatable", DataType::Text),
            Column::new("is_insertable_into", DataType::Text),
        ]);
        (schema, vec![])
    }

    /// information_schema.referential_constraints — one row per FK
    /// constraint. Reads from the per-table `TableConstraints` blob via
    /// the storage catalog, so cross-schema and self-referential FKs
    /// surface correctly.
    fn query_information_schema_referential_constraints(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("constraint_catalog", DataType::Text),
            Column::new("constraint_schema", DataType::Text),
            Column::new("constraint_name", DataType::Text),
            Column::new("unique_constraint_catalog", DataType::Text),
            Column::new("unique_constraint_schema", DataType::Text),
            Column::new("unique_constraint_name", DataType::Text),
            Column::new("match_option", DataType::Text),
            Column::new("update_rule", DataType::Text),
            Column::new("delete_rule", DataType::Text),
        ]);
        let db = match &self.database {
            Some(db) => db,
            None => return Ok((schema, vec![])),
        };
        let catalog = db.storage.catalog();
        let mut rows = Vec::new();
        for table in catalog.list_tables()? {
            let constraints = match catalog.load_table_constraints(&table) {
                Ok(c) => c,
                Err(_) => continue,
            };
            for fk in &constraints.foreign_keys {
                rows.push(Tuple::new(vec![
                    Value::String("heliosdb".into()),
                    Value::String("public".into()),
                    Value::String(fk.name.clone()),
                    Value::String("heliosdb".into()),
                    Value::String("public".into()),
                    Value::String(format!("{}_pkey", fk.references_table)),
                    Value::String("NONE".into()),
                    Value::String(fk.on_update.to_string()),
                    Value::String(fk.on_delete.to_string()),
                ]));
            }
        }
        Ok((schema, rows))
    }

    /// Bug 5 — validate a StartupMessage `database` parameter. Thin
    /// associated-function wrapper around `EmbeddedDatabase::database_name_is_valid`
    /// so the PG-wire handler doesn't need to peek at internals.
    pub fn is_valid_database_name(db: &EmbeddedDatabase, name: &str) -> bool {
        db.database_name_is_valid(name)
    }

    /// Map DataType to PostgreSQL type length
    fn datatype_to_len(dt: &DataType) -> i16 {
        match dt {
            DataType::Boolean => 1,
            DataType::Int2 => 2,
            DataType::Int4 => 4,
            DataType::Int8 => 8,
            DataType::Float4 => 4,
            DataType::Float8 => 8,
            DataType::Timestamp | DataType::Timestamptz => 8,
            DataType::Uuid => 16,
            _ => -1, // variable length
        }
    }
}

impl Default for PgCatalog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_pg_type_query() {
        let catalog = PgCatalog::new();
        let result = catalog.query_pg_type();
        assert!(result.is_ok());

        let (schema, rows) = result.unwrap();
        assert_eq!(schema.columns.len(), 5);
        assert!(rows.len() > 0);
    }

    #[test]
    fn test_pg_namespace_query() {
        let catalog = PgCatalog::new();
        let result = catalog.query_pg_namespace();
        assert!(result.is_ok());

        let (schema, rows) = result.unwrap();
        assert_eq!(schema.columns.len(), 3);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn test_handle_query_non_catalog() {
        let catalog = PgCatalog::new();
        let result = catalog.handle_query("SELECT * FROM users");
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn test_handle_query_catalog() {
        let catalog = PgCatalog::new();
        let result = catalog.handle_query("SELECT * FROM pg_type");
        assert!(result.is_ok());
        assert!(result.unwrap().is_some());
    }

    #[test]
    fn test_handle_query_information_schema_tables() {
        let catalog = PgCatalog::new();
        let result = catalog.handle_query("SELECT table_name FROM information_schema.tables WHERE table_schema = 'public'");
        assert!(result.is_ok());
        // Without database, returns empty but doesn't error
        // project_columns reduces to only the requested column (table_name)
        let (schema, rows) = result.unwrap().unwrap();
        assert_eq!(schema.columns.len(), 1);
        assert_eq!(rows.len(), 0);
    }

    #[test]
    fn test_handle_query_information_schema_columns() {
        let catalog = PgCatalog::new();
        let result = catalog.handle_query("SELECT column_name, data_type FROM information_schema.columns WHERE table_name = 'test'");
        assert!(result.is_ok());
        // project_columns reduces to only the requested columns (column_name, data_type)
        let (schema, rows) = result.unwrap().unwrap();
        assert_eq!(schema.columns.len(), 2);
        assert_eq!(rows.len(), 0);
    }

    #[test]
    fn test_like_match() {
        assert!(PgCatalog::sql_like_match("tenant_abc__users", "tenant_abc__%"));
        assert!(PgCatalog::sql_like_match("tenant_abc__orders", "tenant_abc__%"));
        assert!(!PgCatalog::sql_like_match("other_table", "tenant_abc__%"));
        assert!(PgCatalog::sql_like_match("hello", "hel%"));
        assert!(PgCatalog::sql_like_match("hello", "h_llo"));
        assert!(!PgCatalog::sql_like_match("hello", "h_lo"));
    }

    #[test]
    fn test_extract_like_filter() {
        let query = "select table_name from information_schema.tables where table_name like 'tenant_abc__%'";
        assert_eq!(PgCatalog::extract_like_filter(query, "table_name"), Some("tenant_abc__%".to_string()));

        let query = "select table_name from information_schema.tables where table_schema = 'public'";
        assert_eq!(PgCatalog::extract_like_filter(query, "table_name"), None);
    }

    #[test]
    fn test_extract_eq_filter() {
        let query = "select column_name from information_schema.columns c where table_name = 'my_table'";
        assert_eq!(PgCatalog::extract_eq_filter(query, "table_name"), Some("my_table".to_string()));
    }

    // -------------------------------------------------------------------
    // KanttBan #21A (v3.30.1) — aggregates / WHERE IS NULL on catalog
    // tables. drizzle-kit's introspection emits these shapes; without
    // the apply_aggregate stage they short-circuit catalog tuples back
    // to the client and break `drizzle-kit push`.
    // -------------------------------------------------------------------

    #[test]
    fn count_star_pg_namespace_returns_scalar() {
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query("select count(*) from pg_namespace")
            .unwrap()
            .unwrap();
        let (schema, rows) = result;
        assert_eq!(schema.columns.len(), 1);
        assert_eq!(schema.columns[0].name, "count");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values[0], Value::Int8(2));
    }

    #[test]
    fn count_star_with_is_null_filter_returns_zero() {
        // Reproduces the exact KanttBan #21A repro:
        // SELECT count(*) FROM pg_namespace WHERE nspname IS NULL;
        let catalog = PgCatalog::new();
        let (schema, rows) = catalog
            .handle_query("select count(*) from pg_namespace where nspname is null")
            .unwrap()
            .unwrap();
        assert_eq!(schema.columns.len(), 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values[0], Value::Int8(0));
    }

    #[test]
    fn count_star_with_is_not_null_filter_returns_all() {
        let catalog = PgCatalog::new();
        let (_schema, rows) = catalog
            .handle_query("select count(*) from pg_namespace where nspname is not null")
            .unwrap()
            .unwrap();
        assert_eq!(rows[0].values[0], Value::Int8(2));
    }

    #[test]
    fn group_by_information_schema_tables_buckets_correctly() {
        // Reproduces the `drizzle-kit push` blocker shape:
        // SELECT table_schema, count(*) FROM information_schema.tables
        // GROUP BY table_schema;
        // Without a database attached the rows list is empty, but we
        // still want the response shape to be (table_schema, count).
        let catalog = PgCatalog::new();
        let (schema, rows) = catalog
            .handle_query(
                "select table_schema, count(*) from information_schema.tables group by table_schema",
            )
            .unwrap()
            .unwrap();
        assert_eq!(schema.columns.len(), 2);
        assert_eq!(schema.columns[1].name, "count");
        assert_eq!(rows.len(), 0);
    }

    #[test]
    fn is_null_eval_simple_pred_drops_non_null_row() {
        let schema = Schema::new(vec![Column::new("c", DataType::Text)]);
        let row_text = Tuple::new(vec![Value::String("x".into())]);
        let row_null = Tuple::new(vec![Value::Null]);
        assert!(!PgCatalog::eval_simple_pred("c is null", &schema, &row_text));
        assert!(PgCatalog::eval_simple_pred("c is null", &schema, &row_null));
        assert!(PgCatalog::eval_simple_pred("c is not null", &schema, &row_text));
        assert!(!PgCatalog::eval_simple_pred("c is not null", &schema, &row_null));
    }

    #[test]
    fn extract_relchecks_oid_parses_psql_d_query() {
        // KanttBan #7 (v3.30.1): the literal 15-column header that
        // psql `\d <name>` sends after resolving the relation OID.
        let q = "select c.relchecks, c.relkind, c.relhasindex, c.relhasrules, \
                 c.relhastriggers, c.relrowsecurity, c.relforcerowsecurity, \
                 false as relhasoids, c.relispartition, '', c.reltablespace, \
                 case when c.reloftype = 0 then '' else \
                 c.reloftype::pg_catalog.regtype::pg_catalog.text end, \
                 c.relpersistence, c.relreplident, am.amname \
                 from pg_catalog.pg_class c \
                 left join pg_catalog.pg_class tc on (c.reltoastrelid = tc.oid) \
                 left join pg_catalog.pg_am am on (c.relam = am.oid) \
                 where c.oid = '16384';";
        assert_eq!(PgCatalog::extract_relchecks_oid(q), Some(16384));
    }
}
