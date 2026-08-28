//! PostgreSQL system catalog emulation
//!
//! This module provides minimal emulation of PostgreSQL system catalogs
//! (pg_catalog) and information_schema for client compatibility.
//! Many PostgreSQL clients query these system tables during connection
//! and for introspection.

use crate::{Column, DataType, EmbeddedDatabase, Result, Schema, Tuple, Value};
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
        Self {
            database: Some(database),
        }
    }

    /// Handle catalog queries
    ///
    /// Returns Some((schema, rows)) if this is a catalog query,
    /// None if it should be handled by the normal query engine
    pub fn handle_query(&self, query: &str) -> Result<Option<(Schema, Vec<Tuple>)>> {
        let query_lower = query.trim().to_lowercase();

        // --- F1: statement-kind gate (task #38) --------------------------
        // This handler runs on the RAW, UNPARSED statement text and can only
        // *substring-match*. Every legitimate interception it performs — psql
        // meta-command signatures and client introspection probes — is a
        // read: a `SELECT`, a CTE `WITH`, or a parenthesised `( SELECT … )`.
        // A DML/DDL statement (UPDATE/INSERT/DELETE/CREATE/…) that merely
        // *mentions* a catalog name (in a string literal, a column value, a
        // comment) must NEVER be intercepted here — doing so silently discards
        // the write and hands the client a fake SELECT-shaped result. Gate on
        // the first keyword up front so no downstream substring check can
        // hijack a write. This alone kills the live-verified silent-write-loss
        // class: `UPDATE t SET note='see pg_tables'`,
        // `CREATE TABLE pg_type_registry (…)`,
        // `INSERT … VALUES ('… information_schema.sql_features …')`, and the
        // full-psql-\dt-signature-inside-a-string-literal INSERT.
        let first_word: String = query_lower.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
        let is_select_like = query_lower.starts_with('(') || first_word == "select" || first_word == "with";
        if !is_select_like {
            return Ok(None);
        }

        // --- F2: literal/comment-stripped view of the statement (task #38) -
        // A raw `contains()` also fires on catalog names that appear INSIDE a
        // single-quoted string literal or a SQL comment (e.g.
        // `SELECT * FROM my_notes WHERE body = 'see pg_type docs'`, or
        // `SELECT * FROM t -- see pg_tables`). Those are ordinary reads of a
        // USER table, not catalog probes. `matchable` blanks out the CONTENTS
        // of literals and comments (see `strip_literals_and_comments`) so the
        // catalog-detection predicates only ever see real SQL. IMPORTANT:
        // `matchable` is routed ONLY to the detection predicates below
        // (`has_information_schema_ref`, `is_catalog_query`, and the pg_*
        // dispatch). `try_psql_metacommand` and every result post-processing
        // helper (`apply_where_filter` / `apply_aggregate` / `project_columns`
        // / `extract_*`) keep receiving the ORIGINAL `query_lower` — they
        // legitimately parse string literals (psql's `'r'` relkind fragment,
        // WHERE filter values).
        let matchable = Self::strip_literals_and_comments(&query_lower);

        // --- psql meta-command query detection ---------------------------
        // psql sends complex JOINs across pg_class / pg_namespace /
        // pg_attribute that our simple substring matcher can't resolve, so
        // recognise them by signature and synthesise a shaped response.
        if let Some(result) = self.try_psql_metacommand(&query_lower)? {
            return Ok(Some(result));
        }

        // `version()` / `current_database()` / `current_user` / `session_user` /
        // `current_schema()` are deliberately NOT intercepted here. This handler
        // runs on the RAW, UNPARSED query text before the real parser/planner
        // even sees the statement — a `contains()` check can't tell "this
        // substring IS the whole statement" from "this substring occurs
        // somewhere inside a larger expression" (e.g. `current_database() ~ 'x'`,
        // `length(version())`, or a WHERE clause on an UPDATE/DELETE that happens
        // to mention one of these names), so a hardcoded canned row here would
        // silently discard the rest of the statement — including write
        // statements, which would then return a fake SELECT-shaped result
        // instead of executing. Falling through to `Ok(None)` lets the real
        // parser/planner/evaluator answer these correctly and uniformly
        // (session-aware where relevant) for both the wire and embedded paths —
        // see `Evaluator`'s `"version"` / `"current_database"` / `"current_user"`
        // / `"session_user"` / `"current_schema"` scalar-function arms.

        // Check for information_schema queries (table / column listing).
        // Match the TABLE reference (`information_schema.<name>`) over the
        // literal/comment-stripped `matchable` text. Historically this check
        // was hand-rolled to avoid matching the `'information_schema'` string
        // literal that Drizzle / postgres-js / Prisma pass in WHERE clauses
        // like `… WHERE schemaname NOT IN ('pg_catalog','information_schema')`;
        // F2 stripping now blanks that literal's contents generically, so the
        // special-case dodge is no longer needed. The old bare
        // space-delimited ` information_schema ` disjunct was dropped together
        // with F4 (its only consumer was a degenerate empty-result branch that
        // now falls through to the planner).
        let has_information_schema_ref = matchable.contains("information_schema.");
        // HC3 (catalog unification): every information_schema view listed below
        // is served by the planner-backed SystemViewRegistry
        // (src/sql/phase3/system_views.rs). `return Ok(None)` defers to the
        // planner on ALL three routes that reach this function — the PG simple
        // query path, the PG extended/Parse path (handler_extended.rs derives
        // RowDescription from the planner instead), and the MySQL wire (which
        // calls `execute_query` on Ok(None)) — so ONE implementation now answers
        // every interface instead of a wire-only fixed-shape copy plus a
        // divergent registry copy.
        //
        // This deletes a whole class of bug rather than instances of it: the
        // substring router could not filter, project or JOIN, so
        // `… FROM information_schema.columns WHERE table_schema = 'public'` —
        // the most common ORM introspection query in existence — tested a column
        // the wire shape did not have, compared it against NULL and dropped
        // EVERY row; written without spaces around `=` it instead silently
        // dropped `table_schema` from the projection and returned a narrower row
        // than RowDescription had promised. The planner does real filtering,
        // projection, JOINs and aggregates, so all of that simply goes away.
        //
        // NEVER add an interception branch back here. This handler runs on RAW,
        // UNPARSED text and has already caused two silent-write-loss incidents
        // (commits 0c27a30, 4ec06fa / tasks #34, #38); REMOVING interception is
        // the only safe direction. If a wire test fails, fix the registry.
        let result = if has_information_schema_ref {
            if query_lower.contains("information_schema.columns")
                || query_lower.contains("information_schema.tables")
                || query_lower.contains("information_schema.key_column_usage")
                || query_lower.contains("information_schema.table_constraints")
                || query_lower.contains("information_schema.referential_constraints")
                || query_lower.contains("information_schema.constraint_column_usage")
                || query_lower.contains("information_schema.sequences")
                || query_lower.contains("information_schema.schemata")
                || query_lower.contains("information_schema.catalog_name")
                || query_lower.contains("information_schema.check_constraints")
                || query_lower.contains("information_schema.views")
                // HC4 privilege/role views. `table_privileges` and
                // `role_table_grants` are now POPULATED from the persisted ACL
                // catalog, and the other eight are registered shape-correct and
                // empty — all ten in the phase-3 registry, so the embedded /
                // REPL / Python routes stop reporting them as unknown
                // relations. Deferring here means the planner (which can
                // filter, project and JOIN) answers them on the wire too.
                //
                // A ROW IN THESE VIEWS MEANS "SOMEBODY RAN GRANT". It does not
                // mean access is restricted: this build enforces no privilege.
                || query_lower.contains("information_schema.table_privileges")
                || query_lower.contains("information_schema.role_table_grants")
                || query_lower.contains("information_schema.column_privileges")
                || query_lower.contains("information_schema.role_column_grants")
                || query_lower.contains("information_schema.usage_privileges")
                || query_lower.contains("information_schema.role_usage_grants")
                || query_lower.contains("information_schema.role_routine_grants")
                || query_lower.contains("information_schema.applicable_roles")
                || query_lower.contains("information_schema.enabled_roles")
                || query_lower.contains("information_schema.administrable_role_authorizations")
            {
                return Ok(None);
            } else if query_lower.contains("information_schema.routines") {
                Some(Self::query_information_schema_routines())
            } else if let Some(name) = Self::information_schema_view_name(&query_lower) {
                if let Some(empty) = Self::known_empty_information_schema_view(&name) {
                    Some(empty)
                } else {
                    // Keep this list HONEST: "populated" means the view returns rows
                    // reflecting real schema state, measured over the wire. Several
                    // views resolve and report the correct column list but return zero
                    // rows by construction — listing those as implemented is what sent
                    // users looking for their own mistake. See
                    // docs/compatibility/information_schema.md.
                    return Err(crate::Error::QueryExecution(format!(
                        "information_schema.{name} is not a recognised view; \
                         HeliosDB Nano populates catalog_name, tables (base tables AND \
                         views), columns, schemata, views, key_column_usage, \
                         table_constraints, constraint_column_usage, \
                         referential_constraints, check_constraints, sequences, \
                         table_privileges and role_table_grants — the last two report \
                         STORED grants; HeliosDB does NOT enforce SQL privileges. \
                         These resolve but are ALWAYS EMPTY: view_table_usage, \
                         view_column_usage, routines, parameters, triggers, domains, \
                         character_sets, collations, column_privileges, \
                         usage_privileges, role_column_grants, role_usage_grants, \
                         role_routine_grants, applicable_roles, enabled_roles, \
                         administrable_role_authorizations. \
                         Please file an issue if this view is needed."
                    )));
                }
            } else {
                // F4 (task #38): `information_schema.` is present but no view
                // name is extractable (a degenerate trailing dot). The old
                // behaviour returned a zero-column empty result, silently
                // masking the real outcome. Fall through to the planner so a
                // genuine "relation does not exist" surfaces instead of a fake
                // empty rowset.
                return Ok(None);
            }
        } else if !Self::is_catalog_query(&matchable) {
            return Ok(None);
        } else if Self::contains_word(&matchable, "pg_type") {
            Some(self.query_pg_type()?)
        } else if matchable.contains("pg_inherits") {
            // KanttBan #22 slice 5 regression carve-out: pg_inherits
            // is registered in the SystemViewRegistry but psql's `\d`
            // sub-queries against it use `c.oid::pg_catalog.regclass`
            // which the planner doesn't yet parse. Short-circuit with
            // an empty 3-col shape so libpq doesn't error and psql's
            // describe panel doesn't render bogus "Inherits" sections.
            // Direct ORM queries against pg_inherits still get the
            // empty rowset via this route — same behaviour as the
            // registry would have produced.
            Some((
                Schema::new(vec![
                    Column::new("oid", DataType::Text),
                    Column::new("relkind", DataType::Char(1)),
                    Column::new("partbound", DataType::Text),
                ]),
                vec![],
            ))
        } else if matchable.contains("pg_publication") {
            // Same carve-out as pg_inherits: psql `\d` joins this with
            // `pg_relation_is_publishable(<oid>)`, which the planner
            // doesn't implement. Empty 1-col `pubname` response.
            Some((Schema::new(vec![Column::new("pubname", DataType::Text)]), vec![]))
        } else if matchable.contains("pg_statistic_ext") {
            // Same carve-out: psql's `\d` query against pg_statistic_ext
            // projects `stxrelid::pg_catalog.regclass` and
            // `stxnamespace::pg_catalog.regnamespace`, both regclass-family
            // type casts the planner doesn't handle. Empty 9-col shape
            // matches the slice 5 registry registration.
            Some((
                Schema::new(vec![
                    Column::new("oid", DataType::Int4),
                    Column::new("stxrelid", DataType::Text),
                    Column::new("nsp", DataType::Text),
                    Column::new("stxname", DataType::Text),
                    Column::new("columns", DataType::Text),
                    Column::new("ndist_enabled", DataType::Boolean),
                    Column::new("deps_enabled", DataType::Boolean),
                    Column::new("mcv_enabled", DataType::Boolean),
                    Column::new("stxstattarget", DataType::Int4),
                ]),
                vec![],
            ))
        } else if Self::contains_word(&matchable, "pg_tables") {
            // Leave until migrated to registry. `contains_word` still matches
            // inside `pg_catalog.pg_tables` (the `.` is a boundary).
            Some(self.query_pg_tables()?)
        } else if Self::contains_word(&matchable, "pg_settings") {
            Some(self.query_pg_settings()?)
        } else {
            // KanttBan #22 (v3.31.0): pg_namespace / pg_class / pg_attribute /
            // pg_index / pg_constraint / pg_user / pg_roles previously had
            // fixed-shape branches here; HC3 added pg_views and pg_indexes to
            // that list (pg_indexes was the LIVE implementation and is ported
            // verbatim into the registry, which also un-errors it on the
            // embedded / REPL / Python routes). They now flow through the
            // regular planner via the SystemViewRegistry (see src/sql/planner.rs
            // dealias_schema + table_factor_to_plan; src/sql/executor/scan.rs
            // handle_scan). Returning None signals the caller to fall through
            // to the planner; the planner handles SELECT projection, column
            // aliases, JOINs, complex WHERE, aggregates — all the things
            // this substring router didn't.
            return Ok(None);
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
                    if p < end {
                        end = p;
                    }
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
            let col_idx = schema.columns.iter().position(|c| c.name.to_lowercase() == group_col)?;

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
            let out_schema = Schema::new(vec![group_col_meta, Column::new("count", DataType::Int8)]);
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
        let start = match q.find(where_kw) {
            Some(p) => p + where_kw.len(),
            None => return rows,
        };
        let terminators = [" order by ", " group by ", " limit ", " offset ", ";"];
        let mut end = q.len();
        for t in &terminators {
            if let Some(p) = q[start..].find(t) {
                let cand = start + p;
                if cand < end {
                    end = cand;
                }
            }
        }
        let predicate = q[start..end].trim();
        if predicate.is_empty() {
            return rows;
        }

        // Split on " and " at the top level (we don't handle parens).
        let preds: Vec<&str> = predicate.split(" and ").map(str::trim).collect();
        rows.into_iter()
            .filter(|row| preds.iter().all(|p| Self::eval_simple_pred(p, schema, row)))
            .collect()
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
        s.trim()
            .trim_matches(|c: char| c == '(' || c == ')')
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
        if lit.eq_ignore_ascii_case("true") {
            return matches!(val, Value::Boolean(true));
        }
        if lit.eq_ignore_ascii_case("false") {
            return matches!(val, Value::Boolean(false));
        }
        false
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
            .filter_map(|requested| schema.columns.iter().position(|c| c.name == *requested))
            .collect();

        // If no columns matched, return all (safety fallback)
        if col_indices.is_empty() {
            return (schema, rows);
        }

        // Build projected schema
        let projected_schema = Schema::new(
            // Safety: col_indices validated against schema.columns.len() above
            #[allow(clippy::indexing_slicing)]
            col_indices.iter().map(|&i| schema.columns[i].clone()).collect(),
        );

        // Build projected rows
        let projected_rows = rows
            .into_iter()
            .map(|row| {
                let values: Vec<Value> = col_indices
                    .iter()
                    .map(|&i| row.values.get(i).cloned().unwrap_or(Value::Null))
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
                Value::Int4(16),
                Value::String("bool".to_string()),
                Value::Int4(11),
                Value::Int2(1),
                Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(20),
                Value::String("int8".to_string()),
                Value::Int4(11),
                Value::Int2(8),
                Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(21),
                Value::String("int2".to_string()),
                Value::Int4(11),
                Value::Int2(2),
                Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(23),
                Value::String("int4".to_string()),
                Value::Int4(11),
                Value::Int2(4),
                Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(25),
                Value::String("text".to_string()),
                Value::Int4(11),
                Value::Int2(-1),
                Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(700),
                Value::String("float4".to_string()),
                Value::Int4(11),
                Value::Int2(4),
                Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(701),
                Value::String("float8".to_string()),
                Value::Int4(11),
                Value::Int2(8),
                Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(1043),
                Value::String("varchar".to_string()),
                Value::Int4(11),
                Value::Int2(-1),
                Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(1114),
                Value::String("timestamp".to_string()),
                Value::Int4(11),
                Value::Int2(8),
                Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(2950),
                Value::String("uuid".to_string()),
                Value::Int4(11),
                Value::Int2(16),
                Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(114),
                Value::String("json".to_string()),
                Value::Int4(11),
                Value::Int2(-1),
                Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(3802),
                Value::String("jsonb".to_string()),
                Value::Int4(11),
                Value::Int2(-1),
                Value::String("b".to_string()),
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
                Value::Int4(2200),              // public namespace
                Value::String("r".to_string()), // regular table
                Value::Int4(10),                // owner
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
        let mut rows = vec![Tuple::new(vec![
            Value::Int4(1),
            Value::String("heliosdb".to_string()),
            Value::Int4(10),
            Value::Int4(6), // UTF8
        ])];
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
                    if name != p {
                        continue;
                    }
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
        // psql sends a multi-column SELECT joining pg_database to
        // pg_authid + pg_tablespace + pg_shdescription. v3.31.0 slice 4
        // wrinkle: the previous signature (`pg_database` + `d.datname`)
        // false-fired on drizzle-kit-style queries like
        // `SELECT d.datname AS db_name FROM pg_database d WHERE …`.
        // Tightened to require the multi-column shape psql actually
        // sends — `pg_get_userbyid(d.datdba)` (the owner column) is a
        // good discriminator since no ORM emits it.
        if q.contains("pg_database")
            && q.contains("pg_catalog.pg_database")
            && q.contains("d.datname")
            && q.contains("pg_get_userbyid(d.datdba)")
        {
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
        //
        // HC4: these rows used to be two hardcoded all-privilege superusers, so
        // `\du` reported a privilege posture nobody had configured. Shape and
        // rows now come from `sql::acl_views` — the SAME builders the phase-3
        // registry uses for `pg_roles` / `pg_user` / `pg_authid` — so `\du` and
        // `SELECT * FROM pg_roles` can never disagree. The two virtual built-ins
        // are still listed (compatibility), followed by every persisted role
        // with its REAL attribute bits. Those bits are RECORDED, NOT ENFORCED.
        if q.contains("pg_catalog.pg_roles") && q.contains("rolname") && q.contains("rolsuper") {
            let schema = Schema::new(crate::sql::acl_views::psql_du_columns());
            let rows = crate::sql::acl_views::psql_du_rows(&catalog)?;
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
                    if !Self::sql_like_match(&name, pat) {
                        continue;
                    }
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
                    if t != table_oid {
                        continue;
                    }
                }
                let has_index = catalog
                    .get_table_schema(name)
                    .map(|s| s.columns.iter().any(|c| c.primary_key || c.unique))
                    .unwrap_or(false);
                rows.push(Tuple::new(vec![
                    Value::Int2(0),               // relchecks
                    Value::String("r".into()),    // relkind = ordinary table
                    Value::Boolean(has_index),    // relhasindex
                    Value::Boolean(false),        // relhasrules
                    Value::Boolean(false),        // relhastriggers
                    Value::Boolean(false),        // relrowsecurity
                    Value::Boolean(false),        // relforcerowsecurity
                    Value::Boolean(false),        // relhasoids
                    Value::Boolean(false),        // relispartition
                    Value::String(String::new()), // (literal '' from psql query)
                    Value::Int4(0),               // reltablespace = pg_default
                    Value::String(String::new()), // CASE reloftype → ''
                    Value::String("p".into()),    // relpersistence = permanent
                    Value::String("d".into()),    // relreplident = default
                    Value::String("heap".into()), // am.amname
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
                            col.default_expr
                                .as_ref()
                                .map(|d| Value::String(d.clone()))
                                .unwrap_or(Value::Null),
                            Value::Boolean(!col.nullable),
                            Value::Null, // collation
                            Value::String(if col.primary_key {
                                "d".to_string()
                            } else {
                                "".to_string()
                            }),
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
        if q.contains("pg_get_indexdef") && q.contains("pg_get_constraintdef") && q.contains("c2.relname") {
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
                    if t != table_oid {
                        continue;
                    }
                }
                if let Ok(ts) = catalog.get_table_schema(name) {
                    let pk_cols: Vec<&str> = ts
                        .columns
                        .iter()
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
            "pg_catalog",
            "pg_type",
            "pg_class",
            "pg_namespace",
            "pg_attribute",
            "pg_database",
            "pg_index",
            "pg_indexes",
            "pg_sequences",
            "pg_tables",
            "pg_views",
            "pg_constraint",
            "pg_description",
            "pg_roles",
            "pg_user",
            "pg_proc",
            "pg_settings",
            "pg_policies",
            "pg_matviews",
        ];
        // Word-boundary match (task #38 F3): a marker must be a whole
        // identifier token, not a substring of a larger name. Without this a
        // user table like `app_pg_settings` or `my_pg_tables_backup` would be
        // permanently shadowed by the canned catalog response. `contains_word`
        // still matches qualified references (`pg_catalog.pg_class`) because
        // `.` is a boundary character. Caller passes the literal/comment
        // stripped `matchable` text so markers inside string literals /
        // comments don't count either.
        MARKERS.iter().any(|m| Self::contains_word(q, m))
    }

    /// Replace the CONTENTS of single-quoted string literals, line comments
    /// (`-- … EOL`) and block comments (`/* … */`, non-nested) with spaces,
    /// preserving every other byte verbatim (task #38 F2). This yields a
    /// "matchable" view of the statement in which catalog-name substring
    /// checks can't be fooled by a marker that only appears inside a literal
    /// or a comment. Doubled `''` inside a literal is an escaped quote and
    /// keeps us INSIDE the literal. Delimiter bytes (`'`, `-`, `/`, `*`,
    /// newline) are all ASCII (<0x80) and so never collide with a UTF-8
    /// continuation byte, making the byte scan safe for multibyte input.
    fn strip_literals_and_comments(q: &str) -> String {
        let bytes = q.as_bytes();
        let n = bytes.len();
        let mut out: Vec<u8> = Vec::with_capacity(n);
        let mut i = 0;
        while i < n {
            let c = bytes[i];
            // Line comment: `--` to end of line.
            if c == b'-' && i + 1 < n && bytes[i + 1] == b'-' {
                out.push(b' ');
                out.push(b' ');
                i += 2;
                while i < n && bytes[i] != b'\n' {
                    out.push(b' ');
                    i += 1;
                }
                continue;
            }
            // Block comment: `/* … */` (non-nested).
            if c == b'/' && i + 1 < n && bytes[i + 1] == b'*' {
                out.push(b' ');
                out.push(b' ');
                i += 2;
                while i < n {
                    if bytes[i] == b'*' && i + 1 < n && bytes[i + 1] == b'/' {
                        out.push(b' ');
                        out.push(b' ');
                        i += 2;
                        break;
                    }
                    out.push(if bytes[i] == b'\n' { b'\n' } else { b' ' });
                    i += 1;
                }
                continue;
            }
            // Single-quoted string literal (with `''` escape).
            if c == b'\'' {
                out.push(b'\''); // preserve the opening quote position
                i += 1;
                while i < n {
                    if bytes[i] == b'\'' {
                        if i + 1 < n && bytes[i + 1] == b'\'' {
                            // Escaped quote: stay inside the literal.
                            out.push(b' ');
                            out.push(b' ');
                            i += 2;
                            continue;
                        }
                        out.push(b'\''); // closing quote
                        i += 1;
                        break;
                    }
                    out.push(if bytes[i] == b'\n' { b'\n' } else { b' ' });
                    i += 1;
                }
                continue;
            }
            out.push(c);
            i += 1;
        }
        // Every emitted byte is either a verbatim source byte or an ASCII
        // space/newline; no multibyte sequence is ever split, so the result is
        // valid UTF-8. Fall back to the original on the impossible error path.
        String::from_utf8(out).unwrap_or_else(|_| q.to_string())
    }

    /// True iff `needle` occurs in `haystack` at an identifier-token boundary:
    /// the character immediately before and after the match (if any) must NOT
    /// be an identifier byte (`[a-z0-9_]`) (task #38 F3). This is what stops a
    /// marker like `pg_settings` from matching inside `app_pg_settings`, while
    /// still matching inside `pg_catalog.pg_settings` (the `.` is a boundary).
    /// Operates on the already-lowercased text.
    fn contains_word(haystack: &str, needle: &str) -> bool {
        if needle.is_empty() {
            return false;
        }
        let hb = haystack.as_bytes();
        let nlen = needle.len();
        let mut start = 0;
        while let Some(rel) = haystack[start..].find(needle) {
            let abs = start + rel;
            let before_ok = abs == 0 || !Self::is_ident_byte(hb[abs - 1]);
            let after_idx = abs + nlen;
            let after_ok = after_idx >= hb.len() || !Self::is_ident_byte(hb[after_idx]);
            if before_ok && after_ok {
                return true;
            }
            start = abs + 1;
        }
        false
    }

    /// Identifier byte for `contains_word`: `[a-z0-9_]` (lowercased input).
    fn is_ident_byte(b: u8) -> bool {
        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'
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
                let pk_cols: Vec<String> = tschema
                    .columns
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.primary_key)
                    .map(|(i, _)| (i + 1).to_string())
                    .collect();
                if !pk_cols.is_empty() {
                    rows.push(Tuple::new(vec![
                        Value::Int4(table_oid + 100_000), // synthetic index oid
                        Value::Int4(table_oid),
                        Value::Boolean(true), // indisunique
                        Value::Boolean(true), // indisprimary
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
        let rows = tables
            .into_iter()
            .map(|t| {
                Tuple::new(vec![
                    Value::String("public".into()),
                    Value::String(t),
                    Value::String("heliosdb".into()),
                    Value::Null,
                    Value::Boolean(true),
                ])
            })
            .collect();
        Ok((schema, rows))
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
                let pk_cols: Vec<String> = tschema
                    .columns
                    .iter()
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

    // HC4: `query_pg_roles` (two hardcoded all-privilege superusers) is gone.
    // `pg_roles` / `pg_user` / `pg_authid` have no branch in this substring
    // router at all — they fall through to the planner and are answered by the
    // phase-3 registry from `sql::acl_views`, which reads the persisted role
    // catalog. The only role rows still built on this file's side are psql's
    // `\du` / `\dg` meta-command response, and those come from the same
    // `acl_views` builders (see `try_psql_metacommand`).

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
        if name.is_empty() {
            None
        } else {
            Some(name)
        }
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
            // HC4: table_privileges / column_privileges / usage_privileges /
            // role_*_grants / applicable_roles / enabled_roles /
            // administrable_role_authorizations are NOT listed here any more.
            // All ten are registered in the phase-3 registry (two populated
            // from the stored ACL catalog, eight shape-correct empty) and the
            // caller defers them to the planner, so one implementation answers
            // every route. Do not re-add a wire-side copy.
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

    /// HC3: every registry-backed catalog view DEFERS to the planner
    /// (`Ok(None)`), including a plain single-view SELECT. The fixed-shape wire
    /// copies are deleted: they could not filter, project or JOIN, which is why
    /// `WHERE table_schema = 'public'` — the query every ORM opens with — used
    /// to return zero rows on `columns`. Pin the deferral so nobody
    /// "helpfully" re-adds an interception branch when a wire test fails.
    #[test]
    fn hc3_registry_backed_catalog_views_defer_to_planner() {
        let catalog = PgCatalog::new();
        for q in &[
            "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public'",
            "SELECT column_name FROM information_schema.columns WHERE table_name = 'my_notes'",
            "SELECT table_schema, table_name, column_name, data_type, is_nullable, column_default \
             FROM information_schema.columns WHERE table_schema = 'public'",
            "SELECT * FROM information_schema.schemata",
            "SELECT * FROM information_schema.catalog_name",
            "SELECT * FROM information_schema.views",
            "SELECT * FROM information_schema.check_constraints",
            "SELECT * FROM information_schema.key_column_usage",
            "SELECT * FROM information_schema.table_constraints",
            "SELECT * FROM information_schema.referential_constraints",
            "SELECT * FROM information_schema.constraint_column_usage",
            "SELECT * FROM information_schema.sequences",
            "SELECT * FROM pg_views",
            "SELECT * FROM pg_indexes",
        ] {
            let result = catalog.handle_query(q).unwrap();
            assert!(
                result.is_none(),
                "`{q}` must DEFER to the planner-backed SystemViewRegistry (Ok(None)); got {result:?}"
            );
        }
    }

    // -------------------------------------------------------------------
    // Task #38 — wire-protocol substring-hijack closure.
    //
    // `handle_query` runs on the RAW, lowercased statement text for EVERY
    // statement on the PG wire path. Before this fix, a `contains()` marker
    // check would intercept ANY statement mentioning a catalog name — even
    // inside a string literal, a comment, or as a substring of a user
    // identifier — silently discarding writes and shadowing user tables.
    // F1 (statement-kind gate), F2 (literal/comment stripping) and F3
    // (word-boundary matching) close that surface. These tests pin the
    // exact live-verified hijacks from the audit.
    // -------------------------------------------------------------------

    /// F1: a write whose literal mentions `pg_tables` must NOT be intercepted
    /// (it would silently never execute). Falls through to the real engine.
    #[test]
    fn task38_update_with_pg_tables_literal_falls_through() {
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query("UPDATE inventory SET note='see pg_tables' WHERE id=1")
            .unwrap();
        assert!(result.is_none(), "UPDATE must fall through, got {result:?}");
    }

    /// F1: a write whose literal mentions `pg_settings` must fall through.
    #[test]
    fn task38_update_with_pg_settings_literal_falls_through() {
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query("UPDATE inventory SET note='pg_settings changed' WHERE id=1")
            .unwrap();
        assert!(result.is_none(), "UPDATE must fall through, got {result:?}");
    }

    /// F1: `CREATE TABLE pg_type_registry` (marker as an identifier substring)
    /// must fall through so the table is actually created.
    #[test]
    fn task38_create_table_pg_type_substring_falls_through() {
        let catalog = PgCatalog::new();
        let result = catalog.handle_query("CREATE TABLE pg_type_registry (id int)").unwrap();
        assert!(result.is_none(), "CREATE TABLE must fall through, got {result:?}");
    }

    /// F1: `CREATE TABLE pg_views_cache` must fall through.
    #[test]
    fn task38_create_table_pg_views_substring_falls_through() {
        let catalog = PgCatalog::new();
        let result = catalog.handle_query("CREATE TABLE pg_views_cache (id int)").unwrap();
        assert!(result.is_none(), "CREATE TABLE must fall through, got {result:?}");
    }

    /// F2: a SELECT of a USER table whose literal mentions `pg_type` must NOT
    /// be intercepted by the pg_type dispatch — the marker is inside a string.
    #[test]
    fn task38_select_user_table_with_pg_type_literal_falls_through() {
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query("SELECT * FROM my_notes WHERE body = 'see pg_type docs'")
            .unwrap();
        assert!(
            result.is_none(),
            "SELECT of user table must fall through, got {result:?}"
        );
    }

    /// F3: a user table named `app_pg_settings` must NOT be shadowed by the
    /// pg_settings canned response (word boundary: `_` before the marker).
    #[test]
    fn task38_select_word_boundary_app_pg_settings_falls_through() {
        let catalog = PgCatalog::new();
        let result = catalog.handle_query("SELECT * FROM app_pg_settings").unwrap();
        assert!(result.is_none(), "app_pg_settings must not be shadowed, got {result:?}");
    }

    /// F1/F2: a write whose literal mentions `information_schema.columns` must
    /// fall through (the write must execute).
    #[test]
    fn task38_update_with_information_schema_literal_falls_through() {
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query("UPDATE inventory SET note='check information_schema.columns' WHERE id=1")
            .unwrap();
        assert!(result.is_none(), "UPDATE must fall through, got {result:?}");
    }

    /// F1: an INSERT mentioning an unknown information_schema view in a literal
    /// must fall through as Ok(None) — NOT raise the spurious unknown-view
    /// ERROR the old bare-branch produced.
    #[test]
    fn task38_insert_with_unknown_information_schema_literal_is_none_not_err() {
        let catalog = PgCatalog::new();
        let result =
            catalog.handle_query("INSERT INTO my_notes VALUES (9, 'read information_schema.sql_features spec')");
        assert!(
            matches!(result, Ok(None)),
            "INSERT with information_schema literal must be Ok(None), got {result:?}"
        );
    }

    /// F2/F4: a SELECT of a user table whose literal contains the bare word
    /// `information_schema` must fall through (no degenerate empty result).
    #[test]
    fn task38_select_user_table_with_bare_information_schema_literal_falls_through() {
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query("SELECT * FROM my_notes WHERE body = 'the information_schema is useful'")
            .unwrap();
        assert!(
            result.is_none(),
            "bare information_schema literal must fall through, got {result:?}"
        );
    }

    /// F1: an INSERT whose literal contains the verbatim psql `\dt` catalog
    /// query must fall through — the psql signature must not intercept a write.
    #[test]
    fn task38_insert_with_psql_dt_signature_in_literal_falls_through() {
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query(
                "INSERT INTO query_log VALUES (1, 'SELECT n.nspname, c.relname FROM pg_catalog.pg_class c \
                 LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relkind IN (''r'') AND pg_catalog.pg_get_userbyid(c.relowner) = x')",
            )
            .unwrap();
        assert!(
            result.is_none(),
            "INSERT with psql signature literal must fall through, got {result:?}"
        );
    }

    /// F2: a trailing line comment mentioning `pg_tables` must not hijack a
    /// plain user-table SELECT.
    #[test]
    fn task38_select_with_trailing_comment_marker_falls_through() {
        let catalog = PgCatalog::new();
        let result = catalog.handle_query("SELECT * FROM t -- see pg_tables").unwrap();
        assert!(result.is_none(), "comment marker must not hijack, got {result:?}");
    }

    // ---- The introspection contract these branches exist for still holds ---

    /// A real `pg_tables` reference is still intercepted.
    #[test]
    fn task38_real_pg_tables_still_intercepted() {
        let catalog = PgCatalog::new();
        let result = catalog.handle_query("SELECT tablename FROM pg_tables").unwrap();
        assert!(result.is_some(), "real pg_tables SELECT must still be served");
    }

    /// The drizzle shape: markers inside ITS OWN literals get stripped, but the
    /// real `FROM pg_tables` reference remains and must still be served.
    #[test]
    fn task38_drizzle_pg_tables_shape_still_intercepted() {
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query(
                "SELECT schemaname, tablename FROM pg_tables \
                 WHERE schemaname NOT IN ('pg_catalog','information_schema')",
            )
            .unwrap();
        assert!(result.is_some(), "drizzle pg_tables shape must still be served");
    }

    /// A schema-qualified `pg_catalog.pg_type` reference must survive
    /// `contains_word` (the `.` is a token boundary).
    #[test]
    fn task38_qualified_pg_type_still_intercepted() {
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query("SELECT oid, typname FROM pg_catalog.pg_type")
            .unwrap();
        assert!(result.is_some(), "qualified pg_catalog.pg_type must still be served");
    }

    /// A real `information_schema.columns` SELECT is answered — by the planner
    /// after HC3, not by this router. The task-#38 contract that matters here is
    /// that it neither ERRORS nor gets hijacked: `Ok(None)` is the "the real
    /// engine handles this" signal, and the engine has the view registered.
    /// Row-level behaviour is asserted in tests/catalog_introspection_tests.rs
    /// and the wire tests.
    #[test]
    fn task38_real_information_schema_columns_reaches_the_engine() {
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query("SELECT column_name FROM information_schema.columns WHERE table_name = 'my_notes'")
            .expect("a real information_schema.columns SELECT must never error here");
        assert!(
            result.is_none(),
            "real information_schema.columns SELECT must reach the planner, got {result:?}"
        );
    }

    /// The verbatim psql `\dt` query still returns the 4-column
    /// Schema/Name/Type/Owner shape (needs a live database handle).
    #[test]
    fn task38_psql_dt_still_returns_four_column_shape() {
        use std::sync::Arc;
        let db = crate::EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE widgets (id INT PRIMARY KEY)").unwrap();
        let catalog = PgCatalog::with_database(Arc::new(db));
        // The query psql sends for `\dt` (modern form, `!~` not OPERATOR()).
        let dt = "SELECT n.nspname as \"Schema\", c.relname as \"Name\", \
                  CASE c.relkind WHEN 'r' THEN 'table' WHEN 'v' THEN 'view' \
                  WHEN 'm' THEN 'materialized view' WHEN 'S' THEN 'sequence' \
                  WHEN 'f' THEN 'foreign table' WHEN 'p' THEN 'partitioned table' END as \"Type\", \
                  pg_catalog.pg_get_userbyid(c.relowner) as \"Owner\" \
                  FROM pg_catalog.pg_class c \
                  LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                  WHERE c.relkind IN ('r','p','') AND n.nspname <> 'pg_catalog' \
                  AND n.nspname !~ '^pg_toast' AND n.nspname <> 'information_schema' \
                  AND pg_catalog.pg_table_is_visible(c.oid) ORDER BY 1,2";
        let (schema, _rows) = catalog.handle_query(dt).unwrap().expect("psql \\dt must be served");
        let names: Vec<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Schema", "Name", "Type", "Owner"],
            "psql \\dt must return the 4-column Schema/Name/Type/Owner shape"
        );
    }

    /// A real `pg_settings` reference is still intercepted.
    #[test]
    fn task38_real_pg_settings_still_intercepted() {
        let catalog = PgCatalog::new();
        let result = catalog.handle_query("SELECT name, setting FROM pg_settings").unwrap();
        assert!(result.is_some(), "real pg_settings SELECT must still be served");
    }

    // ---- Direct unit coverage of the F2/F3 helpers ---------------------

    #[test]
    fn task38_strip_literals_and_comments_blanks_contents() {
        // Literal contents blanked, quote positions preserved, `''` escape kept
        // inside the literal, structure outside literals intact.
        let out = PgCatalog::strip_literals_and_comments("select * from t where c='pg_tables' and d=1");
        assert!(!out.contains("pg_tables"), "literal contents must be blanked: {out}");
        assert!(
            out.contains("select * from t where c="),
            "outside-literal text intact: {out}"
        );
        assert!(out.contains("and d=1"), "trailing predicate intact: {out}");

        // Line comment blanked.
        let out = PgCatalog::strip_literals_and_comments("select * from t -- see pg_tables");
        assert!(!out.contains("pg_tables"), "line comment must be blanked: {out}");

        // Block comment blanked.
        let out = PgCatalog::strip_literals_and_comments("select /* pg_settings */ 1");
        assert!(!out.contains("pg_settings"), "block comment must be blanked: {out}");

        // Doubled '' escape keeps us inside the literal (no marker leaks).
        let out = PgCatalog::strip_literals_and_comments("x 'a''pg_type''b' y");
        assert!(
            !out.contains("pg_type"),
            "escaped-quote literal must stay blanked: {out}"
        );
        assert!(out.contains('x') && out.contains('y'), "surrounding text intact: {out}");
    }

    #[test]
    fn task38_contains_word_respects_boundaries() {
        assert!(PgCatalog::contains_word("select * from pg_tables", "pg_tables"));
        // Qualified reference: `.` is a boundary.
        assert!(PgCatalog::contains_word("from pg_catalog.pg_tables x", "pg_tables"));
        // Substring of a longer identifier must NOT match.
        assert!(!PgCatalog::contains_word(
            "select * from app_pg_settings",
            "pg_settings"
        ));
        assert!(!PgCatalog::contains_word("select * from pg_tables_backup", "pg_tables"));
        // Trailing/leading boundary at string ends.
        assert!(PgCatalog::contains_word("pg_type", "pg_type"));
        assert!(!PgCatalog::contains_word("pg_typeof(x)", "pg_type"));
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
    fn test_information_schema_columns_filter_distinguishes_tables() {
        // Regression for the a2h v3.60.3 report. With multiple tables each having
        // a `nextval` default, `information_schema.columns` read back the WRONG
        // table's default: the `table_name='t'`/`column_name='c'` filter (no
        // spaces around `=`, as psycopg emits) was dropped, the handler returned
        // every table's columns, and a client `fetchone()` got the first table's
        // first defaulted column. The stored defaults were always correct.
        // HC3: the hand-rolled `extract_eq_filter` this used to exercise is gone
        // along with the whole wire-side copy of the view; the planner now
        // evaluates the predicate. The USER-VISIBLE contract is unchanged and is
        // what this test pins — asserted through the engine, which is exactly
        // where the wire now routes it.
        let db = crate::EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE SEQUENCE actor_actor_id_seq").unwrap();
        db.execute("CREATE TABLE actor (actor_id INT DEFAULT nextval('actor_actor_id_seq'), first_name TEXT)")
            .unwrap();
        db.execute("CREATE SEQUENCE harden_seq").unwrap();
        db.execute("CREATE TABLE harden_t (id INT DEFAULT nextval('harden_seq'), v TEXT)")
            .unwrap();

        let default_of = |sql: &str| -> String {
            let (rows, _cols) = db.query_with_columns(sql).unwrap();
            assert_eq!(
                rows.len(),
                1,
                "expected exactly one row for `{sql}`, got {}",
                rows.len()
            );
            match rows[0].values.first() {
                Some(Value::String(s)) => s.clone(),
                other => panic!("expected a string column_default, got {other:?}"),
            }
        };

        // a2h's exact no-space query must return each table's OWN sequence default.
        let h = default_of(
            "select column_default from information_schema.columns where table_name='harden_t' and column_name='id'",
        );
        assert!(
            h.contains("harden_seq"),
            "harden_t.id default should be harden_seq, got {h}"
        );
        assert!(
            !h.contains("actor"),
            "harden_t.id default must NOT leak actor's sequence, got {h}"
        );

        let a = default_of(
            "select column_default from information_schema.columns where table_name='actor' and column_name='actor_id'",
        );
        assert!(
            a.contains("actor_actor_id_seq"),
            "actor.actor_id default should be actor_actor_id_seq, got {a}"
        );
    }

    // -------------------------------------------------------------------
    // KanttBan #21A (v3.30.1) — aggregates / WHERE IS NULL on pg_catalog.
    //
    // v3.30.1 implemented these in a custom `apply_aggregate` post-filter
    // stage inside the catalog handler. v3.31.0 (KanttBan #22) moved the
    // catalog reads through the regular planner — these queries now
    // return `Ok(None)` from `handle_query` and the planner's aggregate
    // operator takes over. The contract these tests assert flipped:
    //     v3.30.1: Some((schema=[count], rows=[Int8(n)]))
    //     v3.31.0: None  (fall through to planner)
    // End-to-end behaviour for the user is identical (smoked via psql);
    // tested here at the handler boundary.
    // -------------------------------------------------------------------

    #[test]
    fn count_star_pg_namespace_falls_through_to_planner() {
        let catalog = PgCatalog::new();
        let result = catalog.handle_query("select count(*) from pg_namespace").unwrap();
        assert!(
            result.is_none(),
            "pg_namespace should fall through to planner; got {result:?}"
        );
    }

    #[test]
    fn count_star_with_is_null_filter_falls_through_to_planner() {
        // Original KanttBan #21A shape:
        // SELECT count(*) FROM pg_namespace WHERE nspname IS NULL;
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query("select count(*) from pg_namespace where nspname is null")
            .unwrap();
        assert!(
            result.is_none(),
            "pg_namespace WHERE IS NULL should fall through; got {result:?}"
        );
    }

    #[test]
    fn count_star_with_is_not_null_filter_falls_through_to_planner() {
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query("select count(*) from pg_namespace where nspname is not null")
            .unwrap();
        assert!(
            result.is_none(),
            "pg_namespace WHERE IS NOT NULL should fall through; got {result:?}"
        );
    }

    #[test]
    fn group_by_information_schema_tables_falls_through_to_planner() {
        // v3.31.0 slice 4: information_schema.tables migrated to the
        // SystemViewRegistry, so this query now falls through to the
        // planner exactly like the pg_namespace variants above.
        // End-to-end behaviour is preserved (smoked via psql); the
        // aggregate is now applied by the planner's aggregate
        // operator, not the catalog handler's apply_aggregate.
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query("select table_schema, count(*) from information_schema.tables group by table_schema")
            .unwrap();
        assert!(
            result.is_none(),
            "information_schema.tables should fall through; got {result:?}"
        );
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
