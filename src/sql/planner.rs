//! Query planner
//!
//! Converts sqlparser AST to logical plans.

#![allow(unused_variables)]

use super::constraints::ConstraintEnforcement;
use super::explain_options::{ExplainFormatOption, ExplainOptions};
use super::logical_plan::*;
use crate::storage::Catalog;
use crate::{Column, DataType, Error, Result, Schema, Value};
use sqlparser::ast::{
    AnalyzeFormat, BinaryOperator as SqlBinaryOp, ColumnDef as SqlColumnDef, ColumnOption, ConstraintCharacteristics,
    DataType as SqlDataType, Expr, JoinConstraint, JoinOperator, ObjectName, Query,
    ReferentialAction as SqlReferentialAction, Select, SelectItem, SetExpr, Statement, TableFactor, TableWithJoins,
    TriggerEvent as SqlTriggerEvent, TriggerExecBody, TriggerObject, TriggerPeriod, TriggerReferencing,
    UnaryOperator as SqlUnaryOp, UtilityOption,
};

/// Convert sqlparser ReferentialAction to our internal representation
fn convert_referential_action(action: &SqlReferentialAction) -> ReferentialAction {
    match action {
        SqlReferentialAction::NoAction => ReferentialAction::NoAction,
        SqlReferentialAction::Restrict => ReferentialAction::Restrict,
        SqlReferentialAction::Cascade => ReferentialAction::Cascade,
        SqlReferentialAction::SetNull => ReferentialAction::SetNull,
        SqlReferentialAction::SetDefault => ReferentialAction::SetDefault,
    }
}

fn convert_constraint_enforcement(characteristics: Option<&ConstraintCharacteristics>) -> ConstraintEnforcement {
    match characteristics.and_then(|c| c.enforced) {
        Some(false) => ConstraintEnforcement::NotEnforced,
        _ => ConstraintEnforcement::Immediate,
    }
}
use super::phase3::materialized_views::MaterializedViewParser;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

const ANY_ARRAY_MARKER_FUNCTION: &str = "__hdb_any_array";

/// Information about an aggregate plan needed to rewrite ORDER BY expressions.
/// The aggregate and GROUP BY expressions of a grouped plan, used to rewrite
/// ORDER BY keys to the Aggregate operator's output columns (group_N / agg_N)
/// via the same `rewrite_expr_replace_aggregates` the select list uses.
/// (The former per-Project alias fields were removed: slicing Project aliases
/// positionally as [group cols…, agg cols…] is wrong whenever the select list
/// is not exactly the group list — see the ORDER BY rewrite site.)
struct AggregateInfo {
    /// The aggregate expressions from the Aggregate plan (e.g., SUM(val), COUNT(*))
    aggr_exprs: Vec<LogicalExpr>,
    /// The GROUP BY expressions from the Aggregate plan
    group_by_exprs: Vec<LogicalExpr>,
}

/// Parsed `CREATE SEQUENCE` option clauses (each `Option` = "clause present?").
/// For MIN/MAXVALUE the inner `Option<i64>` distinguishes an explicit value
/// (`Some(Some(n))`) from `NO MINVALUE`/`NO MAXVALUE` (`Some(None)`); a literal
/// that failed to parse degrades to `Some(None)` (treated as the type default).
#[derive(Debug, Default, Clone)]
struct ParsedSeqOpts {
    start: Option<i64>,
    increment: Option<i64>,
    min: Option<Option<i64>>,
    max: Option<Option<i64>>,
    cache: Option<i64>,
    cycle: bool,
}

/// Query planner
pub struct Planner<'a> {
    catalog: Option<&'a Catalog<'a>>,
    /// Original SQL for time-travel AS OF parsing
    original_sql: Option<String>,
    /// CTE schemas in scope (name -> schema) - uses RefCell for interior mutability
    cte_schemas: RefCell<HashMap<String, Arc<Schema>>>,
    /// Named window definitions in scope (name -> WindowSpec) from WINDOW clause
    named_windows: RefCell<HashMap<String, sqlparser::ast::WindowSpec>>,
}

impl<'a> Planner<'a> {
    /// Create a new planner without catalog (for testing)
    pub fn new() -> Self {
        Self {
            catalog: None,
            original_sql: None,
            cte_schemas: RefCell::new(HashMap::new()),
            named_windows: RefCell::new(HashMap::new()),
        }
    }

    /// Create a new planner with catalog access
    pub fn with_catalog(catalog: &'a Catalog<'_>) -> Self {
        Self {
            catalog: Some(catalog),
            original_sql: None,
            cte_schemas: RefCell::new(HashMap::new()),
            named_windows: RefCell::new(HashMap::new()),
        }
    }

    /// Set the original SQL for time-travel AS OF parsing
    pub fn with_sql(mut self, sql: String) -> Self {
        self.original_sql = Some(sql);
        self
    }

    /// Check if a name is a CTE in scope
    fn get_cte_schema(&self, name: &str) -> Option<Arc<Schema>> {
        self.cte_schemas.borrow().get(name).cloned()
    }

    /// Add a CTE to scope
    fn add_cte(&self, name: String, schema: Arc<Schema>) {
        self.cte_schemas.borrow_mut().insert(name, schema);
    }

    /// Clear all CTEs from scope
    fn clear_ctes(&self) {
        self.cte_schemas.borrow_mut().clear();
    }

    /// Look up a named window definition by name
    fn get_named_window(&self, name: &str) -> Option<sqlparser::ast::WindowSpec> {
        self.named_windows.borrow().get(name).cloned()
    }

    /// Populate the named window definitions from a SELECT's WINDOW clause.
    fn populate_named_windows(&self, named_window_defs: &[sqlparser::ast::NamedWindowDefinition]) -> Result<()> {
        use sqlparser::ast::NamedWindowExpr;
        self.named_windows.borrow_mut().clear();
        for def in named_window_defs {
            let name = def.0.value.clone();
            let spec = match &def.1 {
                NamedWindowExpr::WindowSpec(spec) => {
                    if let Some(ref parent_name) = spec.window_name {
                        let parent_spec = self.get_named_window(&parent_name.value).ok_or_else(|| {
                            Error::query_execution(format!(
                                "Window \"{}\" references undefined window \"{}\"",
                                name, parent_name.value
                            ))
                        })?;
                        Self::merge_window_specs(&parent_spec, spec)?
                    } else {
                        spec.clone()
                    }
                }
                NamedWindowExpr::NamedWindow(ref_ident) => {
                    self.get_named_window(&ref_ident.value).ok_or_else(|| {
                        Error::query_execution(format!(
                            "Window \"{}\" references undefined window \"{}\"",
                            name, ref_ident.value
                        ))
                    })?
                }
            };
            self.named_windows.borrow_mut().insert(name, spec);
        }
        Ok(())
    }

    /// Merge a parent window spec with a child spec (window inheritance).
    fn merge_window_specs(
        parent: &sqlparser::ast::WindowSpec,
        child: &sqlparser::ast::WindowSpec,
    ) -> Result<sqlparser::ast::WindowSpec> {
        if !parent.partition_by.is_empty() && !child.partition_by.is_empty() {
            return Err(Error::query_execution(
                "Cannot override PARTITION BY of referenced window",
            ));
        }
        if !parent.order_by.is_empty() && !child.order_by.is_empty() {
            return Err(Error::query_execution("Cannot override ORDER BY of referenced window"));
        }
        if parent.window_frame.is_some() && child.window_frame.is_some() {
            return Err(Error::query_execution(
                "Cannot override window frame of referenced window",
            ));
        }
        Ok(sqlparser::ast::WindowSpec {
            window_name: None,
            partition_by: if child.partition_by.is_empty() {
                parent.partition_by.clone()
            } else {
                child.partition_by.clone()
            },
            order_by: if child.order_by.is_empty() {
                parent.order_by.clone()
            } else {
                child.order_by.clone()
            },
            window_frame: child.window_frame.clone().or_else(|| parent.window_frame.clone()),
        })
    }

    /// Clear named window definitions from scope
    fn clear_named_windows(&self) {
        self.named_windows.borrow_mut().clear();
    }

    /// Parse a data type string into a DataType
    ///
    /// Handles common types: INT, INTEGER, TEXT, VARCHAR, DECIMAL, BOOLEAN, etc.
    pub fn parse_data_type_string(type_str: &str) -> Result<DataType> {
        let upper = type_str.trim().to_uppercase();

        if let Some(base) = upper.strip_suffix("[]") {
            let inner = Self::parse_data_type_string(base)?;
            return Ok(DataType::Array(Box::new(inner)));
        }

        // Handle parameterized types first
        if upper.starts_with("VARCHAR") || upper.starts_with("CHARACTER VARYING") {
            // Extract length if present: VARCHAR(255)
            if let Some(start) = upper.find('(') {
                if let Some(end) = upper.find(')') {
                    if let Some(len_str) = upper.get(start + 1..end) {
                        if let Ok(len) = len_str.parse::<usize>() {
                            return Ok(DataType::Varchar(Some(len)));
                        }
                    }
                }
            }
            return Ok(DataType::Varchar(None));
        }

        if upper.starts_with("DECIMAL") || upper.starts_with("NUMERIC") {
            return Ok(DataType::Numeric);
        }

        if upper.starts_with("VECTOR") || upper.starts_with("HALFVEC") {
            if let Some(start) = upper.find('(') {
                if let Some(end) = upper.find(')') {
                    if let Some(dim_str) = upper.get(start + 1..end) {
                        if let Ok(dim) = dim_str.parse::<usize>() {
                            return Ok(DataType::Vector(dim));
                        }
                    }
                }
            }
            return Err(Error::query_execution("VECTOR type requires dimension: VECTOR(n)"));
        }

        // Handle simple types
        match upper.as_str() {
            "INT" | "INTEGER" | "INT4" => Ok(DataType::Int4),
            "SMALLINT" | "INT2" => Ok(DataType::Int2),
            "BIGINT" | "INT8" => Ok(DataType::Int8),
            "REAL" | "FLOAT4" => Ok(DataType::Float4),
            "FLOAT" | "FLOAT8" | "DOUBLE" | "DOUBLE PRECISION" => Ok(DataType::Float8),
            "TEXT" => Ok(DataType::Text),
            "BOOLEAN" | "BOOL" => Ok(DataType::Boolean),
            "DATE" => Ok(DataType::Date),
            "TIME" => Ok(DataType::Time),
            "TIMESTAMP" | "TIMESTAMPTZ" => Ok(DataType::Timestamp),
            "INTERVAL" => Ok(DataType::Interval),
            "UUID" => Ok(DataType::Uuid),
            "JSON" => Ok(DataType::Json),
            "JSONB" => Ok(DataType::Jsonb),
            "BYTEA" => Ok(DataType::Bytea),
            "SERIAL" => Ok(DataType::Int4),
            "BIGSERIAL" => Ok(DataType::Int8),
            "SMALLSERIAL" => Ok(DataType::Int2),
            _ => Err(Error::query_execution(format!("Unknown data type: {}", type_str))),
        }
    }

    /// Normalise a single identifier per the PostgreSQL rule:
    ///   - unquoted identifier → lower-cased (`Foo` → `foo`)
    ///   - quoted identifier   → preserved as written (`"Foo"` → `Foo`)
    ///
    /// sqlparser exposes the quoting state via `Ident::quote_style`;
    /// `None` means unquoted. This helper is the single source of truth
    /// for identifier case handling.
    pub(crate) fn normalize_ident(ident: &sqlparser::ast::Ident) -> String {
        if ident.quote_style.is_some() {
            ident.value.clone()
        } else {
            ident.value.to_lowercase()
        }
    }

    /// Normalise a (possibly qualified) `ObjectName` into a dotted
    /// string, applying `normalize_ident` to each component. Callers
    /// use this instead of `ObjectName::to_string()` so that
    /// `CREATE TABLE Users` and `SELECT FROM users` resolve to the
    /// same name.
    /// Pull every recognised clause out of the parsed `CREATE SEQUENCE`
    /// options. Each `Option` distinguishes "clause omitted" from "clause
    /// present". For MIN/MAXVALUE the inner `Option<i64>` distinguishes
    /// `MINVALUE n` (`Some(Some(n))`) from `NO MINVALUE` (`Some(None)`).
    /// Non-literal expressions are skipped (fall back to defaults).
    fn extract_sequence_options(opts: &[sqlparser::ast::SequenceOptions]) -> ParsedSeqOpts {
        use sqlparser::ast::SequenceOptions;
        let mut out = ParsedSeqOpts::default();
        for opt in opts {
            match opt {
                SequenceOptions::StartWith(expr, _) => out.start = Self::literal_i64(expr),
                SequenceOptions::IncrementBy(expr, _) => out.increment = Self::literal_i64(expr),
                // `MINVALUE n` → Some(Some(n)); `NO MINVALUE` → Some(None).
                SequenceOptions::MinValue(Some(expr)) => out.min = Some(Self::literal_i64(expr)),
                SequenceOptions::MinValue(None) => out.min = Some(None),
                SequenceOptions::MaxValue(Some(expr)) => out.max = Some(Self::literal_i64(expr)),
                SequenceOptions::MaxValue(None) => out.max = Some(None),
                SequenceOptions::Cache(expr) => out.cache = Self::literal_i64(expr),
                // sqlparser 0.53 has an INVERTED CYCLE flag: it parses `CYCLE`
                // as `Cycle(false)` and `NO CYCLE` as `Cycle(true)` (see
                // sqlparser parser/mod.rs `parse_create_sequence_options`). We
                // invert it back so `CYCLE` => cycle=true.
                SequenceOptions::Cycle(b) => out.cycle = !*b,
            }
        }
        out
    }

    /// Map a parsed `AS <type>` clause to the canonical sequence type string
    /// (`"smallint"` | `"integer"` | `"bigint"`). Unknown / unsupported types
    /// fall back to `bigint` (PG's default sequence type).
    fn sequence_type_name(dt: &sqlparser::ast::DataType) -> String {
        use sqlparser::ast::DataType as Dt;
        match dt {
            Dt::SmallInt(_) | Dt::Int2(_) => "smallint".to_string(),
            Dt::Int(_) | Dt::Integer(_) | Dt::Int4(_) => "integer".to_string(),
            _ => "bigint".to_string(),
        }
    }

    /// Map a parsed `OWNED BY <ref>` `ObjectName` to `Some((table, col))`.
    /// `OWNED BY NONE` parses as a single ident "NONE" (case-insensitively),
    /// which maps to `None`. A multi-part name yields its last two idents as
    /// (table, col); anything shorter than two parts is treated as unowned.
    fn sequence_owned_by(name: &sqlparser::ast::ObjectName) -> Option<(String, String)> {
        let parts = &name.0;
        if parts.len() == 1 && parts[0].value.eq_ignore_ascii_case("NONE") {
            return None;
        }
        if parts.len() >= 2 {
            let table = Self::normalize_ident(&parts[parts.len() - 2]);
            let col = Self::normalize_ident(&parts[parts.len() - 1]);
            Some((table, col))
        } else {
            None
        }
    }

    /// Best-effort evaluation of a literal integer expression, tolerating a
    /// leading unary `+`/`-` (sequence bounds may be negative).
    fn literal_i64(expr: &sqlparser::ast::Expr) -> Option<i64> {
        use sqlparser::ast::{Expr, UnaryOperator, Value};
        match expr {
            Expr::Value(Value::Number(s, _)) => s.parse::<i64>().ok(),
            Expr::UnaryOp {
                op: UnaryOperator::Minus,
                expr,
            } => Self::literal_i64(expr).map(|n| -n),
            Expr::UnaryOp {
                op: UnaryOperator::Plus,
                expr,
            } => Self::literal_i64(expr),
            _ => None,
        }
    }

    pub(crate) fn normalize_object_name(name: &sqlparser::ast::ObjectName) -> String {
        let joined = name.0.iter().map(Self::normalize_ident).collect::<Vec<_>>().join(".");
        // Schema-namespacing alias: `_hdb_code.<table>` → `_hdb_code_<table>`,
        // `_hdb_graph.<table>` → `_hdb_graph_<table>`.  Lets users
        // address the code-graph / graph-rag tables in dotted form
        // without forcing a catalog refactor.  The flat-prefix names
        // remain the canonical storage keys.
        if let Some(dealiased) = Self::dealias_schema(&joined) {
            return dealiased;
        }
        // Nano stores every table in the implicit `public` schema, so ANY
        // schema qualifier collapses to the bare table name (the last
        // dot-separated component): `public.`/`pg_catalog.` (most ORM-emitted
        // DDL prefixes `"public"."tbl"` — KanttBan #13), AND any user schema.
        // `CREATE SCHEMA` / `SET search_path` do not create a real separate
        // namespace, so a table created unqualified (or under a non-default
        // search_path that Nano resolves to `public`) must resolve identically
        // whether later referenced bare OR schema-qualified — otherwise
        // `CREATE TABLE t; DROP TABLE myschema.t` fails with "table does not
        // exist" (reported as a search_path scoping error). A single quoted
        // identifier that itself contains a dot (e.g. `"my.table"`) parses as
        // ONE part and is correctly left intact. The one exception is
        // `information_schema.*`, whose system views are registered/resolved
        // under their qualified name — those keep the prefix.
        if name.0.len() >= 2 && Self::normalize_ident(&name.0[0]) != "information_schema" {
            return Self::normalize_ident(&name.0[name.0.len() - 1]);
        }
        joined
    }

    /// Normalise a raw (possibly quoted / schema-qualified) dotted name string
    /// the SAME way [`Self::normalize_object_name`] normalises a parsed
    /// `ObjectName`, so names that arrive via a custom string pre-parse path
    /// (e.g. ALTER SEQUENCE) resolve identically to ones parsed by sqlparser.
    /// Splits on `.` outside double quotes; each unquoted part is lowercased,
    /// each quoted part keeps its case; then the `_hdb_*` dealias and
    /// `public.`/`pg_catalog.` collapse are applied.
    pub(crate) fn normalize_dotted_name(raw: &str) -> String {
        // Split on unquoted dots.
        let mut parts: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut in_quote = false;
        let mut quoted_part = false;
        for ch in raw.trim().chars() {
            match ch {
                '"' => {
                    in_quote = !in_quote;
                    quoted_part = true;
                }
                '.' if !in_quote => {
                    parts.push(Self::normalize_name_part(&cur, quoted_part));
                    cur.clear();
                    quoted_part = false;
                }
                _ => cur.push(ch),
            }
        }
        parts.push(Self::normalize_name_part(&cur, quoted_part));
        let joined = parts.join(".");

        if let Some(dealiased) = Self::dealias_schema(&joined) {
            return dealiased;
        }
        // NB: only `public.`/`pg_catalog.` collapse here — NOT every schema
        // qualifier. Unlike `normalize_object_name` (table/object names), this
        // string pre-parse path is ALSO used for `OWNED BY table.col`
        // references, where the dotted `table.col` structure is meaningful and
        // the caller splits it back into (table, col). Collapsing to the last
        // component would corrupt that (regression: alter_sequence
        // owned_by_table_col_and_none). The reported DROP/CREATE schema-scoping
        // bug only flows through `normalize_object_name`, which is fixed there.
        if let Some(rest) = joined.strip_prefix("public.") {
            return rest.to_string();
        }
        if let Some(rest) = joined.strip_prefix("pg_catalog.") {
            return rest.to_string();
        }
        joined
    }

    /// One component of a dotted name: quoted parts keep case (quotes already
    /// stripped by the splitter), unquoted parts are lowercased — matching
    /// [`Self::normalize_ident`].
    fn normalize_name_part(part: &str, was_quoted: bool) -> String {
        if was_quoted {
            part.to_string()
        } else {
            part.to_lowercase()
        }
    }

    /// Map dotted names of the form `_hdb_code.<table>` or
    /// `_hdb_graph.<table>` onto their canonical flat-prefix
    /// counterparts.  Returns `None` for everything else, so plain
    /// `public` table names go through unchanged.
    pub(crate) fn dealias_schema(name: &str) -> Option<String> {
        if let Some(rest) = name.strip_prefix("_hdb_code.") {
            return Some(format!("_hdb_code_{rest}"));
        }
        if let Some(rest) = name.strip_prefix("_hdb_graph.") {
            return Some(format!("_hdb_graph_{rest}"));
        }
        // KanttBan #22 (v3.31.0): treat `pg_catalog.<view>` and bare
        // `<view>` as equivalent for system-view lookup. The Phase 3
        // SystemViewRegistry keys on bare names; psql / drizzle-kit /
        // most ORMs schema-qualify catalog reads. Stripping the prefix
        // here funnels both forms to the same registry entry.
        if let Some(rest) = name.strip_prefix("pg_catalog.") {
            return Some(rest.to_string());
        }
        None
    }

    /// Convert a SQL statement to a logical plan
    pub fn statement_to_plan(&self, statement: Statement) -> Result<LogicalPlan> {
        match statement {
            Statement::Query(query) => self.query_to_plan(*query),
            Statement::Insert(insert) => {
                // Extract fields from Insert struct for v0.53 API
                let table_name = Self::normalize_object_name(&insert.table_name);
                let columns = insert.columns;
                // `INSERT INTO t DEFAULT VALUES` — sqlparser leaves
                // `source = None`. Emit an Insert with an empty
                // VALUES row; the executor's default-fill pass
                // provides every column's default (or NULL, or
                // NOT NULL error). No columns are provided, so every
                // slot goes through the "omitted" path.
                let source_opt = insert.source;
                // Extract RETURNING clause if present
                let returning = insert
                    .returning
                    .as_ref()
                    .map(|ret_items| self.convert_returning(ret_items))
                    .transpose()?;
                // Extract ON CONFLICT clause if present
                let on_conflict = self.convert_on_conflict(&insert.on)?;
                match source_opt {
                    Some(source) => self.insert_to_plan(table_name, columns, source, returning, on_conflict),
                    None => Ok(LogicalPlan::Insert {
                        table_name,
                        columns: if columns.is_empty() {
                            None
                        } else {
                            Some(columns.iter().map(Self::normalize_ident).collect())
                        },
                        // One row with zero user-provided values — the
                        // INSERT executor's default-fill covers the rest.
                        values: vec![vec![]],
                        returning,
                        on_conflict,
                    }),
                }
            }
            Statement::CreateTable(create_table) => {
                // Extract fields from CreateTable struct for v0.53 API
                let name = Self::normalize_object_name(&create_table.name);
                let columns = create_table.columns;
                let if_not_exists = create_table.if_not_exists;
                let constraints = create_table.constraints;
                let with_options = create_table.with_options;
                self.create_table_to_plan(name, columns, if_not_exists, constraints, with_options)
            }
            Statement::Drop {
                names,
                if_exists,
                object_type,
                ..
            } => {
                if names.is_empty() {
                    return Err(Error::query_execution("DROP requires a name"));
                }
                // Build one drop plan per object. `DROP TABLE a, b` (PostgreSQL's
                // comma list) yields a DropMulti executed sequentially. DROP TYPE
                // notes: removes the enum registration from the catalog; tables
                // that referenced the type keep their synthesized CHECK constraint
                // (we don't track back-pointers), which mirrors PG closely enough
                // for an embedded DB.
                let to_plan = |name: &sqlparser::ast::ObjectName| -> LogicalPlan {
                    let name = Self::normalize_object_name(name);
                    match object_type {
                        sqlparser::ast::ObjectType::View => LogicalPlan::DropView { name, if_exists },
                        sqlparser::ast::ObjectType::Table => LogicalPlan::DropTable { name, if_exists },
                        sqlparser::ast::ObjectType::Database => LogicalPlan::DropDatabase { name, if_exists },
                        sqlparser::ast::ObjectType::Type => LogicalPlan::DropEnumType { name, if_exists },
                        sqlparser::ast::ObjectType::Sequence => LogicalPlan::DropSequence { name, if_exists },
                        // Default to DROP TABLE for backwards compatibility.
                        _ => LogicalPlan::DropTable { name, if_exists },
                    }
                };

                if names.len() == 1 {
                    Ok(to_plan(&names[0]))
                } else {
                    Ok(LogicalPlan::DropMulti {
                        drops: names.iter().map(&to_plan).collect(),
                    })
                }
            }
            Statement::Truncate { table_names, .. } => {
                if table_names.is_empty() {
                    return Err(Error::query_execution("TRUNCATE requires a table name"));
                }
                if table_names.len() > 1 {
                    return Err(Error::query_execution("Multiple table TRUNCATE not supported"));
                }
                let first_table = table_names
                    .first()
                    .ok_or_else(|| Error::query_execution("TRUNCATE requires a table name"))?;
                Ok(LogicalPlan::Truncate {
                    table_name: first_table.to_string(),
                })
            }
            Statement::Update {
                table,
                assignments,
                selection,
                returning,
                ..
            } => {
                // Extract RETURNING clause if present
                let returning_items = returning
                    .as_ref()
                    .map(|ret_items| self.convert_returning(ret_items))
                    .transpose()?;
                self.update_to_plan(table, assignments, selection, returning_items)
            }
            Statement::Delete(delete_stmt) => {
                // Extract table from FromTable enum
                let table = match &delete_stmt.from {
                    sqlparser::ast::FromTable::WithFromKeyword(tables) => {
                        if tables.len() != 1 {
                            return Err(Error::query_execution("Multi-table DELETE not supported"));
                        }
                        tables
                            .first()
                            .ok_or_else(|| Error::query_execution("DELETE requires a table"))?
                            .clone()
                    }
                    sqlparser::ast::FromTable::WithoutKeyword(tables) => {
                        if tables.len() != 1 {
                            return Err(Error::query_execution("Multi-table DELETE not supported"));
                        }
                        tables
                            .first()
                            .ok_or_else(|| Error::query_execution("DELETE requires a table"))?
                            .clone()
                    }
                };
                // Extract RETURNING clause if present
                let returning = delete_stmt
                    .returning
                    .as_ref()
                    .map(|ret_items| self.convert_returning(ret_items))
                    .transpose()?;
                self.delete_to_plan(table, delete_stmt.selection.clone(), returning)
            }
            Statement::CreateIndex(create_index) => {
                // Extract index name
                let index_name = Self::normalize_object_name(
                    create_index
                        .name
                        .as_ref()
                        .ok_or_else(|| Error::query_execution("Index name is required"))?,
                );

                // Extract table name
                let table = Self::normalize_object_name(&create_index.table_name);

                // Extract column name. We support single-column B-tree / HNSW
                // indexes natively; multi-column B-tree indexes are accepted
                // (sqlite3 apps rely on them for query planning) but we only
                // index the leading column today — the others are recorded for
                // catalog visibility but not used by the executor. Vector
                // indexes (HNSW, IVF, …) are rejected when multi-column since
                // the algorithm can't span columns.
                if create_index.columns.is_empty() {
                    return Err(Error::query_execution("At least one column required for index"));
                }

                // Extract index type (USING clause) up-front so we can decide
                // whether multi-column is acceptable.
                let index_type = create_index.using.as_ref().map(|ident| ident.value.to_lowercase());
                let is_vector_index = matches!(
                    index_type.as_deref(),
                    Some("hnsw") | Some("ivfflat") | Some("ivf") | Some("vector")
                );

                if create_index.columns.len() > 1 && is_vector_index {
                    return Err(Error::query_execution(
                        "Multi-column vector indexes are not supported (HNSW/IVF require a single VECTOR column)",
                    ));
                }
                if create_index.columns.len() > 1 {
                    tracing::debug!(
                        "Composite index {:?} on {:?}({} cols): only the leading column is indexed",
                        index_name,
                        table,
                        create_index.columns.len()
                    );
                }

                let first_col = create_index
                    .columns
                    .first()
                    .ok_or_else(|| Error::query_execution("At least one column required for index"))?;
                let column = match &first_col.expr {
                    Expr::Identifier(ident) => Self::normalize_ident(ident),
                    _ => return Err(Error::query_execution("Column name expected in CREATE INDEX")),
                };

                // Parse WITH options from SQL
                let options = self.parse_index_options(&create_index.with)?;

                Ok(LogicalPlan::CreateIndex {
                    name: index_name,
                    table_name: table,
                    column_name: column,
                    index_type,
                    options,
                    if_not_exists: create_index.if_not_exists,
                })
            }
            Statement::AlterTable { name, operations, .. } => {
                self.alter_table_to_plan(Self::normalize_object_name(&name), operations)
            }
            Statement::CreateTrigger {
                or_replace,
                is_constraint,
                name,
                period,
                events,
                table_name,
                referenced_table_name,
                referencing,
                trigger_object,
                include_each,
                condition,
                exec_body,
                characteristics,
            } => self.create_trigger_to_plan(
                or_replace,
                is_constraint,
                name,
                period,
                events,
                table_name,
                referenced_table_name,
                referencing,
                trigger_object,
                include_each,
                condition,
                exec_body,
                characteristics,
            ),
            Statement::DropTrigger {
                if_exists,
                trigger_name,
                table_name,
                option,
            } => self.drop_trigger_to_plan(if_exists, trigger_name, table_name, option),
            Statement::CreateView {
                name,
                query,
                materialized,
                or_replace,
                if_not_exists,
                options,
                ..
            } => {
                // Check if this is a materialized view
                if materialized {
                    // Convert the query to a logical plan
                    let query_plan = self.query_to_plan(*query.clone())?;

                    // Extract WITH options as string for parsing
                    let options_str = match options {
                        sqlparser::ast::CreateTableOptions::None => None,
                        sqlparser::ast::CreateTableOptions::With(opts)
                        | sqlparser::ast::CreateTableOptions::Options(opts) => {
                            if opts.is_empty() {
                                None
                            } else {
                                Some(
                                    opts.iter()
                                        .filter_map(|opt| {
                                            // SqlOption is an enum - extract key=value from KeyValue variant
                                            match opt {
                                                sqlparser::ast::SqlOption::KeyValue { key, value } => {
                                                    Some(format!("{}={}", key, value))
                                                }
                                                _ => None, // Skip non-key-value options
                                            }
                                        })
                                        .collect::<Vec<_>>()
                                        .join(", "),
                                )
                            }
                        }
                    };

                    MaterializedViewParser::parse_create_mv(
                        name.to_string(),
                        query_plan,
                        options_str.as_deref(),
                        if_not_exists,
                    )
                } else {
                    // Regular (non-materialized) view
                    // Store the query SQL for expansion at query time
                    let query_sql = query.to_string();

                    Ok(LogicalPlan::CreateView {
                        name: name.to_string(),
                        query_sql,
                        if_not_exists,
                        or_replace,
                    })
                }
            }
            Statement::Explain {
                analyze,
                verbose,
                statement,
                format,
                options: utility_options,
                ..
            } => {
                // Convert the inner statement to a logical plan
                let inner_plan = self.statement_to_plan(*statement)?;

                // Parse EXPLAIN options into unified ExplainOptions struct
                let options = self.parse_explain_options(analyze, verbose, format, utility_options)?;

                Ok(LogicalPlan::Explain {
                    input: Box::new(inner_plan),
                    options,
                })
            }
            // Transaction control statements
            Statement::StartTransaction { .. } => Ok(LogicalPlan::StartTransaction),
            Statement::Commit { .. } => Ok(LogicalPlan::Commit),
            Statement::Rollback { savepoint, .. } => {
                if let Some(sp) = savepoint {
                    Ok(LogicalPlan::RollbackToSavepoint { name: sp.value.clone() })
                } else {
                    Ok(LogicalPlan::Rollback)
                }
            }
            Statement::Savepoint { name } => Ok(LogicalPlan::Savepoint {
                name: name.value.clone(),
            }),
            Statement::ReleaseSavepoint { name } => Ok(LogicalPlan::ReleaseSavepoint {
                name: name.value.clone(),
            }),
            // Prepared statements
            Statement::Prepare {
                name,
                data_types,
                statement,
                ..
            } => {
                let param_types: Vec<DataType> = data_types
                    .iter()
                    .filter_map(|dt| self.sql_data_type_to_data_type(dt).ok())
                    .collect();
                let inner_plan = self.statement_to_plan(*statement.clone())?;
                Ok(LogicalPlan::Prepare {
                    name: name.to_string(),
                    param_types,
                    statement: Box::new(inner_plan),
                })
            }
            Statement::Execute { name, parameters, .. } => {
                let params: Result<Vec<LogicalExpr>> = parameters.iter().map(|e| self.expr_to_logical(e)).collect();
                Ok(LogicalPlan::Execute {
                    name: name.to_string(),
                    parameters: params?,
                })
            }
            Statement::Deallocate { name, .. } => {
                let name_str = name.to_string();
                let stmt_name = if name_str.to_uppercase() == "ALL" {
                    None
                } else {
                    Some(name_str)
                };
                Ok(LogicalPlan::Deallocate { name: stmt_name })
            }
            // CREATE EXTENSION <name> — phase 2 of the code-graph
            // track. Dispatches to extension-specific installers at
            // execution time; unknown names error cleanly unless
            // IF NOT EXISTS is set.
            Statement::CreateExtension {
                name, if_not_exists, ..
            } => Ok(LogicalPlan::CreateExtension {
                name: name.value.to_ascii_lowercase(),
                if_not_exists,
            }),
            // Procedural statements
            Statement::CreateFunction(cf) => self.create_function_to_plan(cf),
            Statement::CreateProcedure {
                or_alter,
                name,
                params,
                body,
            } => {
                // ProcedureParam has fields: name: Ident, data_type: DataType (no mode field)
                let param_list = params
                    .unwrap_or_default()
                    .into_iter()
                    .map(|p| {
                        FunctionParam {
                            name: p.name.value.clone(),
                            data_type: self.sql_data_type_to_data_type(&p.data_type).unwrap_or(DataType::Text),
                            mode: ParamMode::In, // ProcedureParam doesn't have mode field
                            default: None,
                        }
                    })
                    .collect();
                let body_str = body.iter().map(|s| format!("{}", s)).collect::<Vec<_>>().join("\n");
                Ok(LogicalPlan::CreateProcedure {
                    name: name.to_string(),
                    or_replace: or_alter, // Map or_alter to or_replace
                    params: param_list,
                    body: body_str,
                    language: "sql".to_string(), // Default to SQL
                })
            }
            Statement::DropFunction {
                if_exists, func_desc, ..
            } => {
                if let Some(fd) = func_desc.first() {
                    Ok(LogicalPlan::DropFunction {
                        name: fd.name.to_string(),
                        if_exists,
                    })
                } else {
                    Err(Error::query_execution("DROP FUNCTION requires a name"))
                }
            }
            Statement::DropProcedure {
                if_exists, proc_desc, ..
            } => {
                if let Some(pd) = proc_desc.first() {
                    Ok(LogicalPlan::DropProcedure {
                        name: pd.name.to_string(),
                        if_exists,
                    })
                } else {
                    Err(Error::query_execution("DROP PROCEDURE requires a name"))
                }
            }
            Statement::Call(call) => {
                // FunctionArguments is an enum: None, Subquery, List(FunctionArgumentList)
                let args: Result<Vec<_>> = match call.args {
                    sqlparser::ast::FunctionArguments::None => Ok(vec![]),
                    sqlparser::ast::FunctionArguments::Subquery(_) => {
                        Err(Error::query_execution("CALL with subquery not supported"))
                    }
                    sqlparser::ast::FunctionArguments::List(arg_list) => arg_list
                        .args
                        .into_iter()
                        .map(|arg| match arg {
                            sqlparser::ast::FunctionArg::Unnamed(fe) => match fe {
                                sqlparser::ast::FunctionArgExpr::Expr(e) => self.expr_to_logical(&e),
                                _ => Err(Error::query_execution("Unsupported CALL argument")),
                            },
                            _ => Err(Error::query_execution("Named CALL arguments not supported")),
                        })
                        .collect(),
                };
                Ok(LogicalPlan::Call {
                    name: call.name.to_string(),
                    args: args?,
                })
            }
            // `CREATE SEQUENCE name [IF NOT EXISTS]` — minimal
            // implementation that registers a named counter in the
            // process-wide in-memory sequence store. `nextval`,
            // `currval`, `setval` read/write the same store. No ORM
            // ownership relationship to columns yet — this is scoped to
            // unblock Prisma / Drizzle migrations that emit sequence DDL.
            Statement::CreateSequence {
                name,
                if_not_exists,
                data_type,
                sequence_options,
                owned_by,
                temporary: _,
            } => {
                let seq_name = Self::normalize_object_name(&name);
                let opts = Self::extract_sequence_options(&sequence_options);
                let data_type = data_type.as_ref().map(Self::sequence_type_name);
                // MINVALUE: Some(Some(n)) = explicit n; Some(None) = NO MINVALUE.
                let (min_value, no_minvalue) = match opts.min {
                    Some(Some(n)) => (Some(n), false),
                    Some(None) => (None, true),
                    None => (None, false),
                };
                let (max_value, no_maxvalue) = match opts.max {
                    Some(Some(n)) => (Some(n), false),
                    Some(None) => (None, true),
                    None => (None, false),
                };
                let owned_by = owned_by.as_ref().and_then(Self::sequence_owned_by);
                Ok(LogicalPlan::CreateSequence {
                    name: seq_name,
                    if_not_exists,
                    data_type,
                    start_value: opts.start,
                    increment_by: opts.increment,
                    min_value,
                    no_minvalue,
                    max_value,
                    no_maxvalue,
                    cache: opts.cache,
                    cycle: opts.cycle,
                    owned_by,
                })
            }
            // `CREATE DATABASE name [IF NOT EXISTS]` — Bug 1 from the
            // dashboard-migration triage. Wraps the existing
            // `TenantManager::register_tenant_with_plan` API as a
            // metadata-only DDL. The reserved-name and duplicate
            // semantics live in the executor.
            Statement::CreateDatabase {
                db_name, if_not_exists, ..
            } => {
                let name = Self::normalize_object_name(&db_name);
                Ok(LogicalPlan::CreateDatabase { name, if_not_exists })
            }
            // CREATE SCHEMA [IF NOT EXISTS] <name>. HeliosDB uses a single flat
            // namespace (schema.table resolves as a composite table name), so
            // this is accepted as a namespace no-op rather than erroring. See
            // the Token Dashboard outstanding item #5.
            Statement::CreateSchema {
                schema_name,
                if_not_exists,
            } => {
                use sqlparser::ast::SchemaName;
                let name = match schema_name {
                    SchemaName::Simple(object_name) => Self::normalize_object_name(&object_name),
                    SchemaName::NamedAuthorization(object_name, _) => Self::normalize_object_name(&object_name),
                    SchemaName::UnnamedAuthorization(ident) => Self::normalize_ident(&ident),
                };
                Ok(LogicalPlan::CreateSchema { name, if_not_exists })
            }
            // `SHOW BRANCHES` parses as a generic SHOW variable. Map it to the
            // branch-listing plan here so it works through every query entry
            // point — the textual pre-detect only covers some of them, so
            // `db.query("SHOW BRANCHES")` previously errored "Statement not
            // yet supported". Token Dashboard #4.
            Statement::ShowVariable { variable } => {
                let name = variable
                    .iter()
                    .map(|ident| ident.value.to_uppercase())
                    .collect::<Vec<_>>()
                    .join(" ");
                if name == "BRANCHES" || name == "DATABASE BRANCHES" {
                    Ok(LogicalPlan::ShowBranches)
                } else {
                    Err(Error::query_execution(format!("SHOW {name} is not supported")))
                }
            }
            // KanttBan #20 (v3.31.0): `CREATE TYPE <name> AS ENUM (…)`.
            // drizzle wraps this in an idempotent DO block — `DO $$
            // BEGIN CREATE TYPE foo AS ENUM ('a','b'); EXCEPTION WHEN
            // duplicate_object THEN null; END $$;` — so we don't need
            // IF NOT EXISTS at the syntax level. Composite / range /
            // domain representations error here; only enums are
            // implemented in this slice.
            Statement::CreateType { name, representation } => {
                let normalised = Self::normalize_object_name(&name);
                use sqlparser::ast::UserDefinedTypeRepresentation;
                match representation {
                    UserDefinedTypeRepresentation::Enum { labels } => {
                        let labels: Vec<String> = labels.into_iter().map(|ident| ident.value).collect();
                        Ok(LogicalPlan::CreateEnumType {
                            name: normalised,
                            labels,
                        })
                    }
                    other => Err(Error::query_execution(format!(
                        "CREATE TYPE {normalised} — only AS ENUM is supported \
                         (got representation: {other:?}). Composite, range, \
                         and domain types are not yet implemented."
                    ))),
                }
            }
            // Priority #4 of the pgrust-corpus diagnosis:
            // `GRANT privileges ON objects TO grantees` /
            // `REVOKE privileges ON objects FROM grantees`. HeliosDB has no
            // SQL-level roles/permissions/ACL module to hang real
            // enforcement off of (checked src/**/role*, permission*, rbac*,
            // auth* — only HTTP/OAuth/replication-role/MCP auth exist,
            // nothing privilege-shaped at the SQL layer), so — following
            // the exact CREATE SCHEMA "single flat namespace" precedent
            // above — these are accepted as a parse-and-accept no-op rather
            // than erroring "Statement not yet supported". This is
            // deliberately NOT real privilege enforcement; see
            // `LogicalPlan::Noop`'s doc comment.
            Statement::Grant { .. } | Statement::Revoke { .. } => Ok(LogicalPlan::Noop),
            _ => Err(Error::query_execution(format!(
                "Statement not yet supported: {:?}",
                statement
            ))),
        }
    }

    /// Convert CREATE FUNCTION to plan
    fn create_function_to_plan(&self, cf: sqlparser::ast::CreateFunction) -> Result<LogicalPlan> {
        // OperateFunctionArg has: mode: Option<ArgMode>, name: Option<Ident>, data_type: DataType, default_expr: Option<Expr>
        let params = cf
            .args
            .unwrap_or_default()
            .into_iter()
            .map(|arg| {
                let data_type = self
                    .sql_data_type_to_data_type(&arg.data_type)
                    .unwrap_or(DataType::Text);
                FunctionParam {
                    name: arg.name.map(|n| n.value).unwrap_or_default(),
                    data_type,
                    mode: match arg.mode {
                        Some(sqlparser::ast::ArgMode::In) => ParamMode::In,
                        Some(sqlparser::ast::ArgMode::Out) => ParamMode::Out,
                        Some(sqlparser::ast::ArgMode::InOut) => ParamMode::InOut,
                        None => ParamMode::In,
                    },
                    default: None,
                }
            })
            .collect();

        let return_type = match cf.return_type.as_ref() {
            // `RETURNS TRIGGER` is a pseudo-type for trigger functions — it is
            // not a real value type, so record None rather than failing type
            // mapping. The trigger machinery interprets the body separately.
            Some(sqlparser::ast::DataType::Trigger) => None,
            Some(rt) => Some(self.sql_data_type_to_data_type(rt)?),
            None => None,
        };

        let body = match cf.function_body {
            Some(sqlparser::ast::CreateFunctionBody::AsBeforeOptions(expr)) => match expr {
                sqlparser::ast::Expr::Value(sqlparser::ast::Value::DollarQuotedString(dqs)) => {
                    Self::repair_sqlparser_string(&dqs.value)
                }
                sqlparser::ast::Expr::Value(sqlparser::ast::Value::SingleQuotedString(s)) => {
                    Self::repair_sqlparser_string(&s)
                }
                other => format!("{}", other),
            },
            Some(sqlparser::ast::CreateFunctionBody::AsAfterOptions(expr)) => match expr {
                sqlparser::ast::Expr::Value(sqlparser::ast::Value::DollarQuotedString(dqs)) => {
                    Self::repair_sqlparser_string(&dqs.value)
                }
                sqlparser::ast::Expr::Value(sqlparser::ast::Value::SingleQuotedString(s)) => {
                    Self::repair_sqlparser_string(&s)
                }
                other => format!("{}", other),
            },
            Some(sqlparser::ast::CreateFunctionBody::Return(expr)) => format!("RETURN {}", expr),
            None => String::new(),
        };

        let language = cf.language.map(|l| l.value).unwrap_or_else(|| "sql".to_string());

        Ok(LogicalPlan::CreateFunction {
            name: cf.name.to_string(),
            or_replace: cf.or_replace,
            params,
            return_type,
            body,
            language,
            volatility: None,
        })
    }

    /// Detect whether a CTE body references its own name as a table — the
    /// signal that an Oracle-style WITH (no RECURSIVE keyword) is in fact
    /// recursive. Walks table references only (not arbitrary identifiers) to
    /// avoid false positives from columns or string literals.
    fn cte_body_references(query: &Query, target: &str) -> bool {
        Self::set_expr_references_table(query.body.as_ref(), target)
    }

    fn set_expr_references_table(set_expr: &SetExpr, target: &str) -> bool {
        match set_expr {
            SetExpr::SetOperation { left, right, .. } => {
                Self::set_expr_references_table(left, target)
                    || Self::set_expr_references_table(right, target)
            }
            SetExpr::Query(q) => Self::set_expr_references_table(q.body.as_ref(), target),
            SetExpr::Select(select) => select
                .from
                .iter()
                .any(|twj| Self::table_with_joins_references(twj, target)),
            _ => false,
        }
    }

    fn table_with_joins_references(twj: &TableWithJoins, target: &str) -> bool {
        if Self::table_factor_references(&twj.relation, target) {
            return true;
        }
        twj.joins
            .iter()
            .any(|j| Self::table_factor_references(&j.relation, target))
    }

    fn table_factor_references(tf: &TableFactor, target: &str) -> bool {
        match tf {
            TableFactor::Table { name, .. } => {
                Self::normalize_object_name(name).eq_ignore_ascii_case(target)
            }
            TableFactor::Derived { subquery, .. } => {
                Self::set_expr_references_table(subquery.body.as_ref(), target)
            }
            _ => false,
        }
    }

    fn infer_recursive_cte_anchor_schema(&self, query: &Query) -> Result<Option<Arc<Schema>>> {
        let SetExpr::SetOperation { left, .. } = query.body.as_ref() else {
            return Ok(None);
        };
        let anchor_plan = self.set_expr_to_plan((**left).clone())?;
        Ok(Some(anchor_plan.schema()))
    }

    /// Convert a Query to a logical plan
    fn query_to_plan(&self, query: Query) -> Result<LogicalPlan> {
        // Handle WITH clause (CTEs)
        let (cte_plans, is_recursive) = if let Some(with_clause) = query.with {
            // Oracle infers recursion and omits the RECURSIVE keyword; Postgres/
            // sqlparser require it. If any CTE references its own name as a table
            // in its body, treat the whole WITH as recursive so the CTE name is
            // pre-registered into scope before its body is planned and the
            // executor uses the fixpoint loop — making a non-RECURSIVE
            // self-referencing CTE behave like WITH RECURSIVE (migration-friendly).
            //
            // C12: but ONLY when the self-reference cannot resolve to a real
            // table. A non-RECURSIVE `WITH x AS (SELECT … FROM x)` where a table
            // `x` also exists is a table-SHADOW under PostgreSQL semantics — the
            // inner `x` is the table, not the CTE — and must NOT be hijacked into
            // an (empty-then-fixpoint) recursion, which returned 0 rows. A genuine
            // recursive CTE has no same-named table (or is written WITH RECURSIVE,
            // which sets `recursive` directly).
            let is_recursive = with_clause.recursive
                || with_clause.cte_tables.iter().any(|cte| {
                    let name = cte.alias.name.to_string();
                    Self::cte_body_references(&cte.query, &name)
                        && !self
                            .catalog
                            .map(|c| c.table_exists(&name).unwrap_or(false))
                            .unwrap_or(false)
                });
            let mut ctes = Vec::new();
            for cte in with_clause.cte_tables {
                let cte_name = cte.alias.name.to_string();

                // For recursive CTEs, pre-register a placeholder schema
                // so that the recursive reference can resolve
                let column_aliases: Vec<String> = cte.alias.columns.iter().map(|col| col.name.value.clone()).collect();

                if is_recursive && !column_aliases.is_empty() {
                    // Use the explicit column aliases with Int8 as placeholder type
                    // (works for numeric recursion like n+1)
                    let schema = Arc::new(Schema::new(
                        column_aliases
                            .iter()
                            .map(|name| Column::new(name, DataType::Int8))
                            .collect(),
                    ));
                    self.add_cte(cte_name.clone(), schema);
                } else if is_recursive {
                    if let Some(schema) = self.infer_recursive_cte_anchor_schema(&cte.query)? {
                        self.add_cte(cte_name.clone(), schema);
                    }
                }

                // Convert CTE query to logical plan
                let cte_plan = self.query_to_plan(*cte.query)?;

                // Apply column aliases if specified (rename CTE columns)
                let cte_schema = if !column_aliases.is_empty() {
                    let original_schema = cte_plan.schema();
                    if column_aliases.len() == original_schema.columns.len() {
                        // Rename columns using the aliases
                        Arc::new(Schema::new(
                            original_schema
                                .columns
                                .iter()
                                .zip(column_aliases.iter())
                                .map(|(col, alias)| {
                                    let mut new_col = col.clone();
                                    new_col.name = alias.clone();
                                    new_col
                                })
                                .collect(),
                        ))
                    } else {
                        // Column count mismatch - use original schema
                        original_schema
                    }
                } else {
                    cte_plan.schema()
                };

                // Pass column aliases to executor for renaming
                let aliases = if !column_aliases.is_empty() {
                    Some(column_aliases)
                } else {
                    None
                };

                self.add_cte(cte_name.clone(), cte_schema);
                ctes.push((cte_name, Box::new(cte_plan), aliases));
            }
            (ctes, is_recursive)
        } else {
            (Vec::new(), false)
        };

        // Convert the body (CTEs are now in scope for table name resolution)
        let mut plan = self.set_expr_to_plan(*query.body)?;

        // Handle ORDER BY
        if let Some(order_by) = &query.order_by {
            // Get the output schema so we can resolve ordinal positions (ORDER BY 1, 2, etc.)
            let output_schema = plan.schema();
            let num_output_cols = output_schema.columns.len();

            // Extract aggregate info from the plan if it's a Project over an Aggregate.
            // This is needed to rewrite ORDER BY aggregate expressions (e.g., ORDER BY SUM(val))
            // to column references that the Sort operator can evaluate.
            let aggregate_info = Self::extract_aggregate_info(&plan);

            // The Sort is planned ABOVE the Project, so it only sees the
            // projected output columns. An ORDER BY *expression* that
            // references base columns the projection dropped (e.g.
            // `ORDER BY embedding <=> $1` when the select list carries
            // `embedding <=> $1 AS d` but not `embedding`) would otherwise
            // fail to evaluate and silently leave the rows unsorted — the
            // pgvector kNN idiom, NANO-DEFICIENCIES A17. Capture the
            // projected (expr, alias) pairs so a matching ORDER BY
            // expression can be redirected to the already-computed column.
            let projection_outputs: Vec<(LogicalExpr, String)> = match &plan {
                LogicalPlan::Project { exprs, aliases, .. } => {
                    exprs.iter().cloned().zip(aliases.iter().cloned()).collect()
                }
                _ => Vec::new(),
            };

            let exprs: Result<Vec<_>> = order_by
                .exprs
                .iter()
                .map(|order_by_expr| {
                    // Check if this is an ordinal position (literal integer)
                    if let Expr::Value(sqlparser::ast::Value::Number(n, _)) = &order_by_expr.expr {
                        if let Ok(ordinal) = n.parse::<usize>() {
                            if ordinal >= 1 && ordinal <= num_output_cols {
                                // Replace with column reference to the Nth output column (1-indexed)
                                // Safety: ordinal validated in range 1..=num_output_cols above
                                #[allow(clippy::indexing_slicing)]
                                let col = &output_schema.columns[ordinal - 1];
                                return Ok(LogicalExpr::Column {
                                    table: None,
                                    name: col.name.clone(),
                                });
                            } else if ordinal >= 1 {
                                return Err(Error::query_execution(format!(
                                    "ORDER BY position {} is not in select list (select list has {} columns)",
                                    ordinal, num_output_cols
                                )));
                            }
                            // ordinal == 0: fall through to treat as literal
                        }
                    }
                    let logical_expr = self.expr_to_logical(&order_by_expr.expr)?;

                    // Grouped plans: rewrite the sort key with the SAME helper the
                    // select list uses (`rewrite_expr_replace_aggregates`), so group
                    // keys become `group_N` and aggregates `agg_N` — the Aggregate
                    // operator's actual output names. `place_order_by` then resolves
                    // the Sort BELOW the post-aggregate Project, where every group
                    // key exists. This fixes two bugs in the old alias-position
                    // rewrite (which sliced the Project aliases as [group cols…,
                    // agg cols…] — only true when the select list IS the group
                    // list): (1) `SELECT a FROM t GROUP BY a, b ORDER BY a, b`
                    // errored "Column 'b' not found" (unprojected group key had no
                    // alias, T8); (2) `SELECT b, a FROM t GROUP BY a, b ORDER BY a`
                    // silently sorted by the WRONG column (positional alias slice).
                    // Bare alias references (`ORDER BY total`) don't match any
                    // group/aggregate expression, stay unrewritten, fail below-
                    // resolution, and keep sorting above the Project as before.
                    let resolved = if let Some(ref info) = aggregate_info {
                        Self::rewrite_expr_replace_aggregates(&logical_expr, &info.aggr_exprs, &info.group_by_exprs)
                    } else {
                        logical_expr
                    };
                    // Redirect an expression that matches a projected select-list
                    // expression to its output column (A17, see above).
                    Ok(Self::rewrite_order_by_to_projection(resolved, &projection_outputs))
                })
                .collect();
            let asc: Vec<_> = order_by
                .exprs
                .iter()
                .map(|order_by_expr| order_by_expr.asc.unwrap_or(true))
                .collect();

            plan = Self::place_order_by(plan, exprs?, asc);
        }

        // Handle LIMIT and OFFSET
        if query.limit.is_some() || query.offset.is_some() {
            let (limit, limit_param) = match query.limit {
                Some(expr) => self.expr_to_limit_bound(&expr)?,
                None => (usize::MAX, None),
            };
            let (offset, offset_param) = match &query.offset {
                Some(offset) => self.expr_to_limit_bound(&offset.value)?,
                None => (0, None),
            };

            plan = LogicalPlan::Limit {
                input: Box::new(plan),
                limit,
                offset,
                limit_param,
                offset_param,
            };
        }

        // If there are CTEs, wrap the plan in a With logical plan
        if !cte_plans.is_empty() {
            plan = LogicalPlan::With {
                ctes: cte_plans,
                recursive: is_recursive,
                query: Box::new(plan),
            };
        }

        Ok(plan)
    }

    /// Convert a SetExpr to a logical plan
    fn set_expr_to_plan(&self, set_expr: SetExpr) -> Result<LogicalPlan> {
        use sqlparser::ast::{SetOperator, SetQuantifier};

        match set_expr {
            SetExpr::Select(select) => self.select_to_plan(*select),
            SetExpr::SetOperation {
                op,
                set_quantifier,
                left,
                right,
            } => {
                let left_plan = self.set_expr_to_plan(*left)?;
                let right_plan = self.set_expr_to_plan(*right)?;

                // ALL keyword means keep duplicates
                let all = matches!(set_quantifier, SetQuantifier::All | SetQuantifier::AllByName);

                match op {
                    SetOperator::Union => Ok(LogicalPlan::Union {
                        left: Box::new(left_plan),
                        right: Box::new(right_plan),
                        all,
                    }),
                    SetOperator::Intersect => Ok(LogicalPlan::Intersect {
                        left: Box::new(left_plan),
                        right: Box::new(right_plan),
                        all,
                    }),
                    SetOperator::Except => Ok(LogicalPlan::Except {
                        left: Box::new(left_plan),
                        right: Box::new(right_plan),
                        all,
                    }),
                }
            }
            SetExpr::Query(query) => self.query_to_plan(*query),
            SetExpr::Values(values) => self.values_to_plan(&values),
            _ => Err(Error::query_execution("Unsupported set expression")),
        }
    }

    /// Plan a `VALUES (…), (…)` relation — top-level (`VALUES (1),(2)`) or as
    /// a CTE body (`WITH t(a,b) AS (VALUES …)`). Desugars into a UNION ALL of
    /// single-row projections over DualScan, reusing the existing projection /
    /// union operators. Columns are named `column1`, `column2`, … (PostgreSQL's
    /// convention for a bare VALUES); a CTE column-alias list renames them
    /// downstream via the normal CTE aliasing path.
    fn values_to_plan(&self, values: &sqlparser::ast::Values) -> Result<LogicalPlan> {
        let first = values
            .rows
            .first()
            .ok_or_else(|| Error::query_execution("VALUES requires at least one row"))?;
        let ncols = first.len();
        let aliases: Vec<String> = (1..=ncols).map(|i| format!("column{i}")).collect();

        let mut combined: Option<LogicalPlan> = None;
        for row in &values.rows {
            if row.len() != ncols {
                return Err(Error::query_execution(
                    "VALUES lists must all be the same length",
                ));
            }
            let exprs = row
                .iter()
                .map(|e| self.expr_to_logical(e))
                .collect::<Result<Vec<_>>>()?;
            let project = LogicalPlan::Project {
                input: Box::new(LogicalPlan::DualScan),
                exprs,
                aliases: aliases.clone(),
                distinct: false,
                distinct_on: None,
            };
            combined = Some(match combined {
                None => project,
                Some(prev) => LogicalPlan::Union {
                    left: Box::new(prev),
                    right: Box::new(project),
                    all: true,
                },
            });
        }
        combined.ok_or_else(|| Error::query_execution("VALUES requires at least one row"))
    }

    /// Convert a SELECT to a logical plan
    fn select_to_plan(&self, select: Select) -> Result<LogicalPlan> {
        // Start with FROM clause
        let mut plan = if select.from.is_empty() {
            // SELECT without FROM (like SELECT 1+1)
            // Use DualScan as the input - it produces a single row with no columns
            LogicalPlan::DualScan
        } else if select.from.len() == 1 {
            self.table_with_joins_to_plan(
                select
                    .from
                    .first()
                    .ok_or_else(|| Error::query_execution("FROM clause is empty"))?,
            )?
        } else {
            // Multiple FROM tables: implicit cross-join (comma-join).
            // FROM t1, t2 WHERE t1.id = t2.id  ≡  FROM t1 CROSS JOIN t2 WHERE ...
            // WordPress uses this for _update_post_term_count.
            let mut cross = self.table_with_joins_to_plan(&select.from[0])?;
            #[allow(clippy::indexing_slicing)]
            for from_item in &select.from[1..] {
                let right = self.table_with_joins_to_plan(from_item)?;
                cross = LogicalPlan::Join {
                    left: Box::new(cross),
                    right: Box::new(right),
                    join_type: crate::sql::JoinType::Cross,
                    on: None,
                    lateral: false,
                };
            }
            cross
        };

        // Add WHERE clause as Filter
        if let Some(predicate) = select.selection {
            let filter_expr = self.expr_to_logical(&predicate)?;
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                predicate: filter_expr,
            };
        }

        // Populate named window definitions from the WINDOW clause before processing
        // projections, so that OVER w references can be resolved during expression planning.
        if !select.named_window.is_empty() {
            self.populate_named_windows(&select.named_window)?;
        }

        // Check if we have aggregate functions (even without GROUP BY)
        // Collect from both SELECT and HAVING so all referenced aggregates are computed.
        let mut aggr_exprs = self.extract_aggregate_exprs(&select.projection)?;
        if let Some(having_expr) = &select.having {
            let having_logical = self.expr_to_logical(having_expr)?;
            Self::collect_aggregates_from_logical(&having_logical, &mut aggr_exprs);
        }
        let has_aggregates = !aggr_exprs.is_empty();
        // A GROUP BY without aggregate functions must still collapse each group to
        // a single row (e.g. `SELECT a, b FROM t GROUP BY a, b` is equivalent to
        // `SELECT DISTINCT a, b`). Route it through the aggregate path with empty
        // aggr_exprs so the grouped operator dedupes by the group key; otherwise
        // the GROUP BY is silently dropped and all base rows are returned
        // (checklist T8). Note this is NOT a blanket DISTINCT on the projection:
        // `SELECT a FROM t GROUP BY a, b` must emit one row per (a, b) group.
        let has_group_by = matches!(
            &select.group_by,
            sqlparser::ast::GroupByExpr::Expressions(exprs, _) if !exprs.is_empty()
        );

        // Handle GROUP BY or implicit aggregation (when aggregates are present without GROUP BY)
        if has_aggregates || has_group_by {
            let group_by = if let sqlparser::ast::GroupByExpr::Expressions(group_by_exprs, _) = &select.group_by {
                if !group_by_exprs.is_empty() {
                    let num_select_items = select.projection.len();
                    let group_by: Result<Vec<_>> = group_by_exprs
                        .iter()
                        .map(|expr| {
                            // Check if this is an ordinal position (literal integer)
                            if let Expr::Value(sqlparser::ast::Value::Number(n, _)) = expr {
                                if let Ok(ordinal) = n.parse::<usize>() {
                                    if ordinal >= 1 && ordinal <= num_select_items {
                                        // Resolve to the Nth SELECT list expression (1-indexed)
                                        // Safety: ordinal validated in range 1..=num_select_items above
                                        #[allow(clippy::indexing_slicing)]
                                        let select_item = &select.projection[ordinal - 1];
                                        let resolved_expr = match select_item {
                                            SelectItem::UnnamedExpr(e) => e,
                                            SelectItem::ExprWithAlias { expr: e, .. } => e,
                                            _ => {
                                                return Err(Error::query_execution(format!(
                                                "GROUP BY position {} refers to a wildcard or unsupported select item",
                                                ordinal
                                            )))
                                            }
                                        };
                                        return self.expr_to_logical(resolved_expr);
                                    } else if ordinal >= 1 {
                                        return Err(Error::query_execution(format!(
                                            "GROUP BY position {} is not in select list (select list has {} columns)",
                                            ordinal, num_select_items
                                        )));
                                    }
                                }
                            }
                            self.expr_to_logical(expr)
                        })
                        .collect();
                    group_by?
                } else {
                    vec![]
                }
            } else {
                vec![]
            };

            // Extract HAVING clause if present
            let having = if let Some(having_expr) = &select.having {
                Some(self.expr_to_logical(having_expr)?)
            } else {
                None
            };

            plan = LogicalPlan::Aggregate {
                input: Box::new(plan),
                group_by: group_by.clone(),
                aggr_exprs: aggr_exprs.clone(),
                having,
            };

            // Add a Project layer to evaluate post-aggregate expressions and apply aliases.
            // The Aggregate operator outputs: group_0, group_1, ..., agg_0, agg_1, ...
            // For each SELECT item, we rewrite the full expression tree so that:
            //   - AggregateFunction nodes become Column refs to agg_N
            //   - Column refs matching GROUP BY become Column refs to group_N
            // This allows expressions like SUM(a) + SUM(b), CAST(AVG(x) AS INT), etc.
            let (_proj_exprs, aliases) = self.select_items_to_exprs(&select.projection, &plan)?;
            let distinct = select.distinct.is_some();

            // Build rewritten projection expressions for each SELECT item
            let mut output_exprs = Vec::new();
            for item in &select.projection {
                match item {
                    SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                        let logical = self.expr_to_logical(expr)?;
                        let rewritten = Self::rewrite_expr_replace_aggregates(&logical, &aggr_exprs, &group_by);
                        output_exprs.push(rewritten);
                    }
                    SelectItem::Wildcard(_) => {
                        // Expand wildcard: group columns + aggregate columns
                        for (i, _) in group_by.iter().enumerate() {
                            output_exprs.push(LogicalExpr::Column {
                                table: None,
                                name: format!("group_{}", i),
                            });
                        }
                        for (i, _) in aggr_exprs.iter().enumerate() {
                            output_exprs.push(LogicalExpr::Column {
                                table: None,
                                name: format!("agg_{}", i),
                            });
                        }
                    }
                    _ => {
                        // Unsupported select item in aggregate context — pass through
                        output_exprs.push(LogicalExpr::Literal(Value::Null));
                    }
                }
            }

            plan = LogicalPlan::Project {
                input: Box::new(plan),
                exprs: output_exprs,
                aliases,
                distinct,
                distinct_on: None,
            };
        } else {
            // No aggregates - just add projection (SELECT columns)
            let (exprs, aliases) = self.select_items_to_exprs(&select.projection, &plan)?;

            // Handle DISTINCT and DISTINCT ON
            let (distinct, distinct_on) = match &select.distinct {
                None => (false, None),
                Some(sqlparser::ast::Distinct::Distinct) => (true, None),
                Some(sqlparser::ast::Distinct::On(on_exprs)) => {
                    // DISTINCT ON (expr1, expr2, ...)
                    let on_parsed: Result<Vec<LogicalExpr>> =
                        on_exprs.iter().map(|e| self.expr_to_logical(e)).collect();
                    (true, Some(on_parsed?))
                }
            };

            plan = LogicalPlan::Project {
                input: Box::new(plan),
                exprs,
                aliases,
                distinct,
                distinct_on,
            };
        }

        // Clean up named window definitions after processing the SELECT
        self.clear_named_windows();

        Ok(plan)
    }

    /// Convert TableWithJoins to a plan
    fn table_with_joins_to_plan(&self, table_with_joins: &TableWithJoins) -> Result<LogicalPlan> {
        // Start with the main table
        let mut plan = self.table_factor_to_plan(&table_with_joins.relation)?;

        // Process joins
        for join in &table_with_joins.joins {
            let right = self.table_factor_to_plan(&join.relation)?;

            // Check if this is a LATERAL join (right side is a LATERAL subquery)
            let is_lateral = matches!(&join.relation, TableFactor::Derived { lateral: true, .. });

            let join_type = match &join.join_operator {
                JoinOperator::Inner(_) => JoinType::Inner,
                JoinOperator::LeftOuter(_) => JoinType::Left,
                JoinOperator::RightOuter(_) => JoinType::Right,
                JoinOperator::FullOuter(_) => JoinType::Full,
                JoinOperator::CrossJoin => JoinType::Cross,
                _ => return Err(Error::query_execution("Join type not supported")),
            };

            // Check for NATURAL join - auto-generate ON clause from common columns
            let is_natural = matches!(
                &join.join_operator,
                JoinOperator::Inner(JoinConstraint::Natural)
                    | JoinOperator::LeftOuter(JoinConstraint::Natural)
                    | JoinOperator::RightOuter(JoinConstraint::Natural)
                    | JoinOperator::FullOuter(JoinConstraint::Natural)
            );

            let on = if is_natural {
                // Find common columns between left and right schemas
                let left_schema = plan.schema();
                let right_schema = right.schema();

                let common_columns: Vec<String> = left_schema
                    .columns
                    .iter()
                    .filter_map(|lc| {
                        if right_schema.columns.iter().any(|rc| rc.name == lc.name) {
                            Some(lc.name.clone())
                        } else {
                            None
                        }
                    })
                    .collect();

                if common_columns.is_empty() {
                    return Err(Error::query_execution(
                        "NATURAL JOIN requires at least one common column between tables",
                    ));
                }

                // Build AND of all column equalities: l.col1 = r.col1 AND l.col2 = r.col2 ...
                let mut condition: Option<LogicalExpr> = None;
                for col_name in common_columns {
                    let eq_expr = LogicalExpr::BinaryExpr {
                        left: Box::new(LogicalExpr::Column {
                            table: None,
                            name: col_name.clone(),
                        }),
                        op: super::BinaryOperator::Eq,
                        right: Box::new(LogicalExpr::Column {
                            table: None,
                            name: col_name,
                        }),
                    };

                    condition = Some(match condition {
                        Some(cond) => LogicalExpr::BinaryExpr {
                            left: Box::new(cond),
                            op: super::BinaryOperator::And,
                            right: Box::new(eq_expr),
                        },
                        None => eq_expr,
                    });
                }

                condition
            } else {
                match &join.join_operator {
                    JoinOperator::Inner(JoinConstraint::On(expr))
                    | JoinOperator::LeftOuter(JoinConstraint::On(expr))
                    | JoinOperator::RightOuter(JoinConstraint::On(expr))
                    | JoinOperator::FullOuter(JoinConstraint::On(expr)) => Some(self.expr_to_logical(expr)?),
                    _ => None,
                }
            };

            plan = LogicalPlan::Join {
                left: Box::new(plan),
                right: Box::new(right),
                join_type,
                on,
                lateral: is_lateral,
            };
        }

        Ok(plan)
    }

    /// Extract function arguments from `TableFunctionArgs` to `Vec<LogicalExpr>`
    fn extract_table_function_args(&self, tf_args: &sqlparser::ast::TableFunctionArgs) -> Result<Vec<LogicalExpr>> {
        let mut logical_args = Vec::new();
        for arg in &tf_args.args {
            match arg {
                sqlparser::ast::FunctionArg::Unnamed(arg_expr) => match arg_expr {
                    sqlparser::ast::FunctionArgExpr::Expr(e) => {
                        logical_args.push(self.expr_to_logical(e)?);
                    }
                    _ => {
                        return Err(Error::query_execution(
                            "Unsupported function argument type in table function",
                        ));
                    }
                },
                _ => {
                    return Err(Error::query_execution(
                        "Named arguments are not supported in table functions",
                    ));
                }
            }
        }
        Ok(logical_args)
    }

    /// Check if a table name is a known table-valued function
    fn is_table_function(name: &str) -> bool {
        matches!(name.to_lowercase().as_str(), "generate_series" | "unnest")
    }

    /// Convert a TableFactor to a plan
    fn table_factor_to_plan(&self, table_factor: &TableFactor) -> Result<LogicalPlan> {
        match table_factor {
            TableFactor::Table { name, alias, args, .. } => {
                let table_name = Self::normalize_object_name(name);

                // Check if this is a table-valued function call (e.g., generate_series(1, 10))
                // In sqlparser, FROM generate_series(1, 10) is parsed as Table with args
                if let Some(tf_args) = args {
                    let lower_name = table_name.to_lowercase();
                    if Self::is_table_function(&lower_name) {
                        let logical_args = self.extract_table_function_args(tf_args)?;
                        let table_alias = alias.as_ref().map(|a| a.name.value.clone());
                        // Priority #6: `FROM generate_series(1, 10) g(i)`
                        // parses as this Table-with-args shape (not
                        // TableFactor::TableFunction), so the column alias
                        // `(i)` lives on `alias.columns` here too.
                        let column_alias = alias
                            .as_ref()
                            .and_then(|a| a.columns.first())
                            .map(|c| c.name.value.clone());
                        return Ok(LogicalPlan::TableFunction {
                            function_name: lower_name,
                            args: logical_args,
                            alias: table_alias,
                            column_alias,
                        });
                    }
                }

                // Check if this is a CTE reference first
                if let Some(cte_schema) = self.get_cte_schema(&table_name) {
                    // This is a CTE reference - create a Scan with the CTE schema
                    // The executor will handle looking up the CTE data
                    let table_alias = alias.as_ref().map(|a| a.name.value.clone());
                    return Ok(LogicalPlan::Scan {
                        table_name,
                        alias: table_alias,
                        schema: cte_schema,
                        projection: None,
                        as_of: None, // CTEs don't support time-travel
                    });
                }

                // Check if this is a system view (Phase 3 features).
                //
                // KanttBan #22 (v3.31.0): previously this returned a
                // `LogicalPlan::SystemView` terminal that didn't
                // compose with Project / Filter / Join — so
                // `SELECT n.nspname AS x FROM pg_namespace n` had no
                // way to reach the projection / alias logic. Now we
                // emit `LogicalPlan::Scan` with the registry's schema;
                // `scan::handle_scan` recognises the table name and
                // materialises rows from the registry. This makes
                // catalog reads first-class in the operator pipeline
                // — WHERE / JOIN / projection / aliases all "just
                // work" because the same operators handle them as for
                // user tables.
                use crate::sql::phase3::SystemViewRegistry;
                let registry = SystemViewRegistry::shared();

                if registry.is_system_view(&table_name) {
                    let schema = Arc::new(
                        registry
                            .get_schema(&table_name)
                            .cloned()
                            .unwrap_or_else(|| Schema { columns: vec![] }),
                    );
                    let table_alias = alias.as_ref().map(|a| a.name.value.clone());
                    return Ok(LogicalPlan::Scan {
                        table_name,
                        alias: table_alias,
                        schema,
                        projection: None,
                        as_of: None,
                    });
                }

                // Check if this is a regular view (non-materialized)
                if let Some(catalog) = self.catalog {
                    let storage = catalog.storage();
                    let view_catalog = storage.view_catalog();
                    if view_catalog.view_exists(&table_name)? {
                        // This is a regular view - expand it by parsing and planning its query
                        let view_metadata = view_catalog.get_view(&table_name)?;

                        // Parse the view's query SQL
                        let parser = super::Parser::new();
                        let stmt = parser.parse_one(&view_metadata.query_sql)?;

                        // Create a new planner for the view query (to avoid self-borrow issues)
                        let view_planner = Planner::with_catalog(catalog);
                        let view_plan = view_planner.statement_to_plan(stmt)?;

                        // If there's an alias, wrap in a subquery with alias
                        // For now, just return the expanded plan
                        return Ok(view_plan);
                    }
                }

                // Not a CTE, system view, or regular view - treat as regular table
                // Fetch schema from catalog if available
                let schema = if let Some(catalog) = self.catalog {
                    // Get actual schema from catalog
                    Arc::new(catalog.get_table_schema(&table_name)?)
                } else {
                    // Fallback to placeholder for tests without storage
                    Arc::new(Schema {
                        columns: vec![Column {
                            name: "id".to_string(),
                            data_type: DataType::Int4,
                            nullable: false,
                            primary_key: false,
                            source_table: None,
                            source_table_name: None,
                            default_expr: None,
                            unique: false,
                            storage_mode: crate::ColumnStorageMode::Default,
                        }],
                    })
                };

                // Parse AS OF clause from original SQL if available
                let as_of = self.parse_as_of_for_table(&table_name)?;

                // Extract alias if present
                let table_alias = alias.as_ref().map(|a| a.name.value.clone());

                Ok(LogicalPlan::Scan {
                    table_name,
                    alias: table_alias,
                    schema,
                    projection: None,
                    as_of,
                })
            }
            TableFactor::Derived {
                subquery,
                alias,
                lateral,
            } => {
                // Handle subqueries in FROM clause: SELECT * FROM (SELECT ...) AS sub
                // Also handles LATERAL: SELECT * FROM t, LATERAL (SELECT ... WHERE t.id = ...)
                let subquery_plan = self.query_to_plan(*subquery.clone())?;

                // If there's an alias, we could wrap this but for now just return the plan
                // The LATERAL flag is handled at the join level
                let _ = alias; // Alias is used for column qualification but schema already has names
                let _ = lateral; // LATERAL is tracked at the join level

                Ok(subquery_plan)
            }
            TableFactor::TableFunction { expr, alias } => {
                // Handle TABLE(expr) syntax
                match expr {
                    Expr::Function(func) => {
                        let func_name = func.name.to_string().to_lowercase();
                        if Self::is_table_function(&func_name) {
                            let mut logical_args = Vec::new();
                            if let sqlparser::ast::FunctionArguments::List(ref arg_list) = func.args {
                                for arg in &arg_list.args {
                                    match arg {
                                        sqlparser::ast::FunctionArg::Unnamed(arg_expr) => {
                                            if let sqlparser::ast::FunctionArgExpr::Expr(e) = arg_expr {
                                                logical_args.push(self.expr_to_logical(e)?);
                                            } else {
                                                return Err(Error::query_execution(
                                                    "Unsupported function argument in TABLE() expression",
                                                ));
                                            }
                                        }
                                        _ => {
                                            return Err(Error::query_execution(
                                                "Named arguments not supported in TABLE() expression",
                                            ));
                                        }
                                    }
                                }
                            }
                            let table_alias = alias.as_ref().map(|a| a.name.value.clone());
                            let column_alias = alias
                                .as_ref()
                                .and_then(|a| a.columns.first())
                                .map(|c| c.name.value.clone());
                            Ok(LogicalPlan::TableFunction {
                                function_name: func_name,
                                args: logical_args,
                                alias: table_alias,
                                column_alias,
                            })
                        } else {
                            Err(Error::query_execution(format!(
                                "Table function '{}' not supported",
                                func_name
                            )))
                        }
                    }
                    _ => Err(Error::query_execution(format!(
                        "Table function expression '{}' not supported",
                        expr
                    ))),
                }
            }
            TableFactor::UNNEST { alias, array_exprs, .. } => {
                // Handle UNNEST(ARRAY[...]) syntax
                let mut logical_args = Vec::new();
                for expr in array_exprs {
                    logical_args.push(self.expr_to_logical(expr)?);
                }
                let table_alias = alias.as_ref().map(|a| a.name.value.clone());
                let column_alias = alias
                    .as_ref()
                    .and_then(|a| a.columns.first())
                    .map(|c| c.name.value.clone());
                Ok(LogicalPlan::TableFunction {
                    function_name: "unnest".to_string(),
                    args: logical_args,
                    alias: table_alias,
                    column_alias,
                })
            }
            TableFactor::NestedJoin {
                table_with_joins,
                alias,
            } => {
                // Parenthesized join group, e.g. `(a JOIN b ON …) JOIN c ON …`.
                // a2h's view-body translation emits left-deep nested joins like
                // this for multi-table views (Pagila / sakila `*_list`). Recurse
                // to build the inner join sub-plan; the inner table aliases are
                // preserved so the ON clauses resolve. An explicit alias on the
                // whole group would re-qualify its columns, which we don't
                // rewrite yet — reject it rather than silently mis-resolve.
                if alias.is_some() {
                    return Err(Error::query_execution(
                        "Aliased parenthesized join `(… JOIN …) AS alias` is not yet supported",
                    ));
                }
                self.table_with_joins_to_plan(table_with_joins)
            }
            other => Err(Error::query_execution(format!(
                "Unsupported table expression: {:?}",
                other
            ))),
        }
    }

    /// Parse AS OF clause for a specific table from the original SQL
    ///
    /// This method extracts time-travel AS OF clauses from the SQL string.
    /// Since sqlparser doesn't natively support AS OF syntax, we parse it manually.
    ///
    /// Supports:
    /// - SELECT * FROM table AS OF TIMESTAMP '2025-11-15 06:00:00'
    /// - SELECT * FROM table AS OF TRANSACTION 987654
    /// - SELECT * FROM table AS OF SCN 123456789
    /// - SELECT * FROM table AS OF NOW
    /// - SELECT * FROM table VERSIONS BETWEEN TIMESTAMP '...' AND TIMESTAMP '...'
    fn parse_as_of_for_table(&self, table_name: &str) -> Result<Option<super::logical_plan::AsOfClause>> {
        use crate::sql::TimeTravelParser;

        // Return None if no original SQL is available
        let sql = match &self.original_sql {
            Some(s) => s,
            None => return Ok(None),
        };

        // Check if SQL contains time-travel syntax
        if !TimeTravelParser::contains_time_travel_syntax(sql) {
            return Ok(None);
        }

        let upper_sql = sql.to_uppercase();
        let table_pattern = format!("FROM {}", table_name).to_uppercase();

        // Check if this query is for the current table
        if !upper_sql.contains(&table_pattern) {
            return Ok(None);
        }

        // Check for VERSIONS BETWEEN first (more specific)
        if upper_sql.contains("VERSIONS BETWEEN") {
            if let Some(versions_clause) = Self::extract_versions_between_from_sql(sql) {
                let (start, end) = TimeTravelParser::parse_versions_between(&versions_clause)?;
                return Ok(Some(super::logical_plan::AsOfClause::VersionsBetween {
                    start: Box::new(start),
                    end: Box::new(end),
                }));
            }
        }

        // Extract AS OF clause from SQL
        if let Some(as_of_str) = TimeTravelParser::extract_as_of_from_sql(sql) {
            // Parse the AS OF clause
            let as_of_clause = TimeTravelParser::parse_as_of_clause(&as_of_str)?;
            return Ok(Some(as_of_clause));
        }

        Ok(None)
    }

    /// Extract VERSIONS BETWEEN clause from SQL
    fn extract_versions_between_from_sql(sql: &str) -> Option<String> {
        let upper = sql.to_uppercase();

        if let Some(pos) = upper.find("VERSIONS BETWEEN") {
            // Skip "VERSIONS BETWEEN " to get the clause content
            let start = pos + "VERSIONS BETWEEN".len();
            let remainder = sql.get(start..)?.trim_start();

            // Find the end of the clause - look for SQL keywords or end
            let keywords = ["WHERE", "GROUP BY", "ORDER BY", "LIMIT", "JOIN", ";"];
            let mut end = remainder.len();

            for keyword in &keywords {
                if let Some(kw_pos) = remainder.to_uppercase().find(keyword) {
                    if kw_pos < end {
                        end = kw_pos;
                    }
                }
            }

            let clause = remainder.get(..end).unwrap_or_default().trim();
            if !clause.is_empty() {
                return Some(clause.to_string());
            }
        }

        None
    }

    /// Convert SELECT items to expressions and aliases
    fn select_items_to_exprs(
        &self,
        items: &[SelectItem],
        input: &LogicalPlan,
    ) -> Result<(Vec<LogicalExpr>, Vec<String>)> {
        let mut exprs = Vec::new();
        let mut aliases = Vec::new();

        for item in items {
            match item {
                SelectItem::UnnamedExpr(expr) => {
                    let logical_expr = self.expr_to_logical(expr)?;
                    // Try to extract a meaningful alias from the expression
                    let alias = self.extract_expr_alias(expr, exprs.len());
                    exprs.push(logical_expr);
                    aliases.push(alias);
                }
                SelectItem::ExprWithAlias { expr, alias } => {
                    let logical_expr = self.expr_to_logical(expr)?;
                    exprs.push(logical_expr);
                    aliases.push(alias.value.clone());
                }
                SelectItem::Wildcard(_) => {
                    // Expand wildcard to all columns from input schema
                    let schema = input.schema();
                    for column in &schema.columns {
                        exprs.push(LogicalExpr::Column {
                            table: None,
                            name: column.name.clone(),
                        });
                        aliases.push(column.name.clone());
                    }
                }
                SelectItem::QualifiedWildcard(object_name, _) => {
                    // Expand alias.* or table.* to all columns from that table
                    let qualifier = object_name
                        .0
                        .iter()
                        .map(|i| i.value.clone())
                        .collect::<Vec<_>>()
                        .join(".");
                    let schema = input.schema();
                    let mut matched = false;
                    for column in &schema.columns {
                        // Match by source_table_name (alias or real table name)
                        let col_table = column.source_table_name.as_deref().unwrap_or("");
                        if col_table.eq_ignore_ascii_case(&qualifier)
                            || column.name.starts_with(&format!("{}.", qualifier))
                        {
                            exprs.push(LogicalExpr::Column {
                                table: Some(qualifier.clone()),
                                name: column.name.clone(),
                            });
                            aliases.push(column.name.clone());
                            matched = true;
                        }
                    }
                    // If no columns matched by source_table, expand ALL columns
                    // (fallback for when source_table isn't set)
                    if !matched {
                        for column in &schema.columns {
                            exprs.push(LogicalExpr::Column {
                                table: None,
                                name: column.name.clone(),
                            });
                            aliases.push(column.name.clone());
                        }
                    }
                } // SelectItem is exhaustive across the four variants
                  // above — the explicit fallback was for a pre-0.53
                  // sqlparser shape and the compiler now flags it as
                  // unreachable.  Keep the match exhaustive without
                  // a wildcard.
            }
        }

        Ok((exprs, aliases))
    }

    /// Extract a meaningful alias from an expression
    /// Falls back to col_{index} if no meaningful name can be extracted
    #[allow(clippy::self_only_used_in_recursion)]
    fn extract_expr_alias(&self, expr: &Expr, index: usize) -> String {
        match expr {
            // Simple column reference: use column name
            Expr::Identifier(ident) => ident.value.clone(),
            // Qualified column: table.column - use column name
            Expr::CompoundIdentifier(idents) => idents
                .last()
                .map(|i| i.value.clone())
                .unwrap_or_else(|| format!("col_{}", index)),
            // Aggregate functions: use function name + column
            Expr::Function(func) => {
                let func_name = func.name.to_string().to_lowercase();
                match func.args {
                    sqlparser::ast::FunctionArguments::List(ref list) if !list.args.is_empty() => {
                        if let Some(sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                            inner,
                        ))) = list.args.first()
                        {
                            if let Expr::Identifier(ident) = inner {
                                return format!("{}({})", func_name, ident.value);
                            }
                        }
                        format!("{}(...)", func_name)
                    }
                    _ => func_name,
                }
            }
            // Binary expressions: left op right
            Expr::BinaryOp { left, op, right } => {
                let left_name = self.extract_expr_alias(left, 0);
                let right_name = self.extract_expr_alias(right, 0);
                format!("{} {} {}", left_name, op, right_name)
            }
            // Cast: use inner expression name
            Expr::Cast { expr: inner, .. } => self.extract_expr_alias(inner, index),
            // Unary expressions
            Expr::UnaryOp { expr: inner, op } => {
                let inner_name = self.extract_expr_alias(inner, index);
                format!("{}{}", op, inner_name)
            }
            // Nested expressions in parentheses
            Expr::Nested(inner) => self.extract_expr_alias(inner, index),
            // Literal values: use the value representation
            Expr::Value(val) => match val {
                sqlparser::ast::Value::Number(n, _) => n.clone(),
                sqlparser::ast::Value::SingleQuotedString(s) => {
                    format!("'{}'", Self::repair_sqlparser_string(s))
                }
                sqlparser::ast::Value::DoubleQuotedString(s) => {
                    format!("\"{}\"", Self::repair_sqlparser_string(s))
                }
                sqlparser::ast::Value::Boolean(b) => b.to_string(),
                sqlparser::ast::Value::Null => "NULL".to_string(),
                _ => format!("col_{}", index),
            },
            // Default fallback
            _ => format!("col_{}", index),
        }
    }

    /// Extract aggregate info from the plan if it's a Project over an Aggregate.
    /// Returns None if the plan is not an aggregate query.
    fn extract_aggregate_info(plan: &LogicalPlan) -> Option<AggregateInfo> {
        if let LogicalPlan::Project { input, .. } = plan {
            if let LogicalPlan::Aggregate {
                group_by, aggr_exprs, ..
            } = input.as_ref()
            {
                return Some(AggregateInfo {
                    aggr_exprs: aggr_exprs.clone(),
                    group_by_exprs: group_by.clone(),
                });
            }
        }
        None
    }

    /// Redirect an ORDER BY expression to a projected output column when it
    /// structurally matches a select-list expression. The Sort runs above the
    /// Project and can only see projected columns, so an ORDER BY expression
    /// over a base column the projection dropped (the pgvector
    /// `ORDER BY embedding <=> $1` idiom, with `embedding <=> $1 AS d` in the
    /// select list) must be redirected to the already-computed column.
    /// Unqualified column / literal sort keys are left untouched — they already
    /// resolve, and redirecting a bare column could shadow a base column that
    /// was projected under a different alias. Qualified columns that match a
    /// projected expression are redirected, because the Project output drops
    /// table qualifiers (`SELECT a.id ... ORDER BY a.id`). See ada-core B6.
    fn rewrite_order_by_to_projection(expr: LogicalExpr, projection_outputs: &[(LogicalExpr, String)]) -> LogicalExpr {
        if let LogicalExpr::Column {
            table: Some(_), name, ..
        } = &expr
        {
            for (proj_expr, alias) in projection_outputs {
                if proj_expr == &expr {
                    return LogicalExpr::Column {
                        table: None,
                        name: if alias.is_empty() { name.clone() } else { alias.clone() },
                    };
                }
            }
            return expr;
        }
        if matches!(expr, LogicalExpr::Column { .. } | LogicalExpr::Literal(_)) {
            return expr;
        }
        for (proj_expr, alias) in projection_outputs {
            if !alias.is_empty() && proj_expr == &expr {
                return LogicalExpr::Column {
                    table: None,
                    name: alias.clone(),
                };
            }
        }
        expr
    }

    /// Plan-time approximation of the schema the EXECUTOR will expose for
    /// `plan`'s output, for ORDER BY placement decisions (R3.5 item 2).
    ///
    /// `handle_scan` stamps each scanned column's `source_table` (alias) and
    /// `source_table_name` at runtime, but plan-time Scan schemas come
    /// straight from the catalog with no stamping. The placement check in
    /// `place_order_by` therefore concluded that qualified sort keys
    /// (`ORDER BY e.id` on a self-join) could not resolve below the Project
    /// and hoisted the Sort above it — where the qualifier really is
    /// unresolvable. The old comparator then silently skipped the per-row
    /// evaluation error and emitted *unsorted* output; with sort-key errors
    /// surfaced, those queries would fail instead. Stamping here lets the
    /// Sort be placed below the Project, where the key resolves and the sort
    /// is actually performed.
    fn runtime_stamped_schema(plan: &LogicalPlan) -> Arc<Schema> {
        match plan {
            LogicalPlan::Scan { table_name, alias, .. } | LogicalPlan::FilteredScan { table_name, alias, .. } => {
                // `plan.schema()` already applies any scan projection.
                let mut schema = (*plan.schema()).clone();
                let source_name = alias.as_ref().unwrap_or(table_name);
                for col in &mut schema.columns {
                    // Mirror handle_scan: unconditional stamp.
                    col.source_table = Some(source_name.clone());
                    col.source_table_name = Some(table_name.clone());
                }
                Arc::new(schema)
            }
            LogicalPlan::Filter { input, .. } => Self::runtime_stamped_schema(input),
            LogicalPlan::Join { left, right, .. } => {
                let mut columns = Self::runtime_stamped_schema(left).columns.clone();
                columns.extend(Self::runtime_stamped_schema(right).columns.clone());
                Arc::new(Schema { columns })
            }
            other => other.schema(),
        }
    }

    fn place_order_by(plan: LogicalPlan, exprs: Vec<LogicalExpr>, asc: Vec<bool>) -> LogicalPlan {
        match plan {
            LogicalPlan::Project {
                input,
                exprs: project_exprs,
                aliases,
                distinct,
                distinct_on,
            } if !distinct && distinct_on.is_none() => {
                let input_schema = Self::runtime_stamped_schema(&input);
                if Self::order_by_resolves_against_schema(&exprs, &input_schema) {
                    return LogicalPlan::Project {
                        input: Box::new(LogicalPlan::Sort { input, exprs, asc }),
                        exprs: project_exprs,
                        aliases,
                        distinct,
                        distinct_on,
                    };
                }

                LogicalPlan::Sort {
                    input: Box::new(LogicalPlan::Project {
                        input,
                        exprs: project_exprs,
                        aliases,
                        distinct,
                        distinct_on,
                    }),
                    exprs,
                    asc,
                }
            }
            other => LogicalPlan::Sort {
                input: Box::new(other),
                exprs,
                asc,
            },
        }
    }

    fn order_by_resolves_against_schema(exprs: &[LogicalExpr], schema: &Schema) -> bool {
        exprs
            .iter()
            .all(|expr| Self::expr_resolves_against_schema(expr, schema))
    }

    fn expr_resolves_against_schema(expr: &LogicalExpr, schema: &Schema) -> bool {
        match expr {
            LogicalExpr::Column { table, name } => schema.get_qualified_column_index(table.as_deref(), name).is_some(),
            LogicalExpr::Literal(_) | LogicalExpr::Parameter { .. } => true,
            LogicalExpr::BinaryExpr { left, right, .. } => {
                Self::expr_resolves_against_schema(left, schema) && Self::expr_resolves_against_schema(right, schema)
            }
            LogicalExpr::UnaryExpr { expr, .. } | LogicalExpr::Cast { expr, .. } | LogicalExpr::IsNull { expr, .. } => {
                Self::expr_resolves_against_schema(expr, schema)
            }
            LogicalExpr::ScalarFunction { args, .. } => {
                args.iter().all(|arg| Self::expr_resolves_against_schema(arg, schema))
            }
            LogicalExpr::Case {
                expr,
                when_then,
                else_result,
            } => {
                expr.as_ref()
                    .is_none_or(|inner| Self::expr_resolves_against_schema(inner, schema))
                    && when_then.iter().all(|(when, then)| {
                        Self::expr_resolves_against_schema(when, schema)
                            && Self::expr_resolves_against_schema(then, schema)
                    })
                    && else_result
                        .as_ref()
                        .is_none_or(|inner| Self::expr_resolves_against_schema(inner, schema))
            }
            LogicalExpr::Between { expr, low, high, .. } => {
                Self::expr_resolves_against_schema(expr, schema)
                    && Self::expr_resolves_against_schema(low, schema)
                    && Self::expr_resolves_against_schema(high, schema)
            }
            LogicalExpr::InList { expr, list, .. } => {
                Self::expr_resolves_against_schema(expr, schema)
                    && list.iter().all(|item| Self::expr_resolves_against_schema(item, schema))
            }
            LogicalExpr::InSet { expr, .. } => Self::expr_resolves_against_schema(expr, schema),
            LogicalExpr::ArraySubscript { array, index } => {
                Self::expr_resolves_against_schema(array, schema) && Self::expr_resolves_against_schema(index, schema)
            }
            LogicalExpr::Tuple { items } => items
                .iter()
                .all(|item| Self::expr_resolves_against_schema(item, schema)),
            _ => false,
        }
    }

    /// Extract aggregate expressions from SELECT items (deep walk).
    ///
    /// Walks each SELECT expression tree to find ALL aggregate function nodes,
    /// even when nested inside arithmetic, CASE, CAST, etc.
    /// Returns a deduplicated list of aggregate expressions.
    fn extract_aggregate_exprs(&self, items: &[SelectItem]) -> Result<Vec<LogicalExpr>> {
        let mut aggr_exprs: Vec<LogicalExpr> = Vec::new();

        for item in items {
            match item {
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                    let logical = self.expr_to_logical(expr)?;
                    Self::collect_aggregates_from_logical(&logical, &mut aggr_exprs);
                }
                _ => {}
            }
        }

        Ok(aggr_exprs)
    }

    /// Recursively walk a LogicalExpr tree and collect all AggregateFunction nodes.
    /// Deduplicates by PartialEq comparison.
    fn collect_aggregates_from_logical(expr: &LogicalExpr, out: &mut Vec<LogicalExpr>) {
        match expr {
            LogicalExpr::AggregateFunction { .. } => {
                if !out.iter().any(|existing| existing == expr) {
                    out.push(expr.clone());
                }
            }
            LogicalExpr::BinaryExpr { left, right, .. } => {
                Self::collect_aggregates_from_logical(left, out);
                Self::collect_aggregates_from_logical(right, out);
            }
            LogicalExpr::UnaryExpr { expr: inner, .. } => {
                Self::collect_aggregates_from_logical(inner, out);
            }
            LogicalExpr::Cast { expr: inner, .. } => {
                Self::collect_aggregates_from_logical(inner, out);
            }
            LogicalExpr::Case {
                expr: base,
                when_then,
                else_result,
            } => {
                if let Some(base_expr) = base {
                    Self::collect_aggregates_from_logical(base_expr, out);
                }
                for (when_expr, then_expr) in when_then {
                    Self::collect_aggregates_from_logical(when_expr, out);
                    Self::collect_aggregates_from_logical(then_expr, out);
                }
                if let Some(else_expr) = else_result {
                    Self::collect_aggregates_from_logical(else_expr, out);
                }
            }
            LogicalExpr::IsNull { expr: inner, .. } => {
                Self::collect_aggregates_from_logical(inner, out);
            }
            LogicalExpr::Between {
                expr: inner, low, high, ..
            } => {
                Self::collect_aggregates_from_logical(inner, out);
                Self::collect_aggregates_from_logical(low, out);
                Self::collect_aggregates_from_logical(high, out);
            }
            LogicalExpr::InList { expr: inner, list, .. } => {
                Self::collect_aggregates_from_logical(inner, out);
                for item in list {
                    Self::collect_aggregates_from_logical(item, out);
                }
            }
            LogicalExpr::ScalarFunction { args, .. } => {
                for arg in args {
                    Self::collect_aggregates_from_logical(arg, out);
                }
            }
            _ => {}
        }
    }

    /// Rewrite a LogicalExpr tree, replacing AggregateFunction nodes with
    /// Column references to the pre-computed aggregate output columns (agg_0, agg_1, ...).
    /// Also replaces column references that match GROUP BY expressions with group_N references.
    /// Qualifier-insensitive structural equivalence between two
    /// `LogicalExpr`s. Two `Column { table, name }` references are
    /// equivalent when their names match and either side has no
    /// qualifier (or both carry the same qualifier). Everything else
    /// recurses into the matching constructor.
    ///
    /// Needed because a single statement may freely mix qualified
    /// (`"t"."col"`) and unqualified (`"col"`) references across
    /// SELECT / WHERE / GROUP BY — stock PostgreSQL treats them as
    /// the same column when unambiguous (B35).
    fn exprs_equivalent(a: &LogicalExpr, b: &LogicalExpr) -> bool {
        match (a, b) {
            (LogicalExpr::Column { table: t1, name: n1 }, LogicalExpr::Column { table: t2, name: n2 }) => {
                if n1 != n2 {
                    return false;
                }
                match (t1, t2) {
                    (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                    _ => true,
                }
            }
            (LogicalExpr::ScalarFunction { fun: f1, args: a1 }, LogicalExpr::ScalarFunction { fun: f2, args: a2 }) => {
                f1.eq_ignore_ascii_case(f2)
                    && a1.len() == a2.len()
                    && a1.iter().zip(a2.iter()).all(|(x, y)| Self::exprs_equivalent(x, y))
            }
            (
                LogicalExpr::AggregateFunction {
                    fun: f1,
                    args: a1,
                    distinct: d1,
                },
                LogicalExpr::AggregateFunction {
                    fun: f2,
                    args: a2,
                    distinct: d2,
                },
            ) => {
                f1 == f2
                    && d1 == d2
                    && a1.len() == a2.len()
                    && a1.iter().zip(a2.iter()).all(|(x, y)| Self::exprs_equivalent(x, y))
            }
            (
                LogicalExpr::BinaryExpr {
                    left: l1,
                    op: op1,
                    right: r1,
                },
                LogicalExpr::BinaryExpr {
                    left: l2,
                    op: op2,
                    right: r2,
                },
            ) => op1 == op2 && Self::exprs_equivalent(l1, l2) && Self::exprs_equivalent(r1, r2),
            (LogicalExpr::UnaryExpr { op: op1, expr: e1 }, LogicalExpr::UnaryExpr { op: op2, expr: e2 }) => {
                op1 == op2 && Self::exprs_equivalent(e1, e2)
            }
            (
                LogicalExpr::Cast {
                    expr: e1,
                    data_type: d1,
                },
                LogicalExpr::Cast {
                    expr: e2,
                    data_type: d2,
                },
            ) => d1 == d2 && Self::exprs_equivalent(e1, e2),
            // For everything else fall back on strict PartialEq — this
            // covers Literal, Parameter, IN, BETWEEN, Like, etc.
            _ => a == b,
        }
    }

    fn rewrite_expr_replace_aggregates(
        expr: &LogicalExpr,
        aggr_exprs: &[LogicalExpr],
        group_by: &[LogicalExpr],
    ) -> LogicalExpr {
        // First check if the entire expression matches a GROUP BY expression.
        // This handles cases like `id % 2` in both SELECT and GROUP BY, where
        // the whole expression should map to `group_N` rather than recursing into parts.
        // Qualifier-insensitive equivalence (B35): `date("check_in")` and
        // `date("t"."check_in")` are the same expression when unambiguous.
        for (i, gb_expr) in group_by.iter().enumerate() {
            if Self::exprs_equivalent(gb_expr, expr) {
                return LogicalExpr::Column {
                    table: None,
                    name: format!("group_{}", i),
                };
            }
        }

        match expr {
            LogicalExpr::AggregateFunction { .. } => {
                for (i, aggr) in aggr_exprs.iter().enumerate() {
                    if Self::exprs_equivalent(aggr, expr) {
                        return LogicalExpr::Column {
                            table: None,
                            name: format!("agg_{}", i),
                        };
                    }
                }
                expr.clone()
            }
            LogicalExpr::Column { name, .. } => {
                for (i, gb_expr) in group_by.iter().enumerate() {
                    if Self::exprs_equivalent(gb_expr, expr) {
                        return LogicalExpr::Column {
                            table: None,
                            name: format!("group_{}", i),
                        };
                    }
                    if let LogicalExpr::Column { name: gb_name, .. } = gb_expr {
                        if gb_name == name {
                            return LogicalExpr::Column {
                                table: None,
                                name: format!("group_{}", i),
                            };
                        }
                    }
                }
                expr.clone()
            }
            LogicalExpr::BinaryExpr { left, op, right } => LogicalExpr::BinaryExpr {
                left: Box::new(Self::rewrite_expr_replace_aggregates(left, aggr_exprs, group_by)),
                op: *op,
                right: Box::new(Self::rewrite_expr_replace_aggregates(right, aggr_exprs, group_by)),
            },
            LogicalExpr::UnaryExpr { op, expr: inner } => LogicalExpr::UnaryExpr {
                op: *op,
                expr: Box::new(Self::rewrite_expr_replace_aggregates(inner, aggr_exprs, group_by)),
            },
            LogicalExpr::Cast { expr: inner, data_type } => LogicalExpr::Cast {
                expr: Box::new(Self::rewrite_expr_replace_aggregates(inner, aggr_exprs, group_by)),
                data_type: data_type.clone(),
            },
            LogicalExpr::Case {
                expr: base,
                when_then,
                else_result,
            } => LogicalExpr::Case {
                expr: base
                    .as_ref()
                    .map(|e| Box::new(Self::rewrite_expr_replace_aggregates(e, aggr_exprs, group_by))),
                when_then: when_then
                    .iter()
                    .map(|(w, t)| {
                        (
                            Self::rewrite_expr_replace_aggregates(w, aggr_exprs, group_by),
                            Self::rewrite_expr_replace_aggregates(t, aggr_exprs, group_by),
                        )
                    })
                    .collect(),
                else_result: else_result
                    .as_ref()
                    .map(|e| Box::new(Self::rewrite_expr_replace_aggregates(e, aggr_exprs, group_by))),
            },
            LogicalExpr::IsNull { expr: inner, is_null } => LogicalExpr::IsNull {
                expr: Box::new(Self::rewrite_expr_replace_aggregates(inner, aggr_exprs, group_by)),
                is_null: *is_null,
            },
            LogicalExpr::Between {
                expr: inner,
                low,
                high,
                negated,
            } => LogicalExpr::Between {
                expr: Box::new(Self::rewrite_expr_replace_aggregates(inner, aggr_exprs, group_by)),
                low: Box::new(Self::rewrite_expr_replace_aggregates(low, aggr_exprs, group_by)),
                high: Box::new(Self::rewrite_expr_replace_aggregates(high, aggr_exprs, group_by)),
                negated: *negated,
            },
            LogicalExpr::InList {
                expr: inner,
                list,
                negated,
            } => LogicalExpr::InList {
                expr: Box::new(Self::rewrite_expr_replace_aggregates(inner, aggr_exprs, group_by)),
                list: list
                    .iter()
                    .map(|e| Self::rewrite_expr_replace_aggregates(e, aggr_exprs, group_by))
                    .collect(),
                negated: *negated,
            },
            LogicalExpr::ScalarFunction { fun, args } => LogicalExpr::ScalarFunction {
                fun: fun.clone(),
                args: args
                    .iter()
                    .map(|a| Self::rewrite_expr_replace_aggregates(a, aggr_exprs, group_by))
                    .collect(),
            },
            _ => expr.clone(),
        }
    }

    /// Extract aggregate function from an expression (used by expr_to_logical for Function nodes)
    fn extract_aggregate_from_expr(&self, expr: &Expr) -> Result<Option<LogicalExpr>> {
        match expr {
            Expr::Function(func) => {
                // If the function has an OVER clause, it's a window function, not a regular aggregate
                if func.over.is_some() {
                    return Ok(None);
                }

                let func_name = func.name.to_string().to_uppercase();

                // Handle STRING_AGG / GROUP_CONCAT specially as they have a delimiter argument
                // GROUP_CONCAT is the MySQL alias for STRING_AGG (WordPress compatibility)
                if func_name == "STRING_AGG" || func_name == "GROUP_CONCAT" {
                    return self.parse_string_agg(func);
                }

                let aggr_fun = match func_name.as_str() {
                    "COUNT" => Some(AggregateFunction::Count),
                    "SUM" => Some(AggregateFunction::Sum),
                    "AVG" => Some(AggregateFunction::Avg),
                    "MIN" => Some(AggregateFunction::Min),
                    "MAX" => Some(AggregateFunction::Max),
                    "JSON_AGG" => Some(AggregateFunction::JsonAgg),
                    "ARRAY_AGG" => Some(AggregateFunction::ArrayAgg),
                    _ => None,
                };

                if let Some(fun) = aggr_fun {
                    // Extract args and distinct from FunctionArguments enum
                    let (args, distinct) = match &func.args {
                        sqlparser::ast::FunctionArguments::List(arg_list) => {
                            let args: Result<Vec<_>> = arg_list
                                .args
                                .iter()
                                .map(|arg| {
                                    match arg {
                                        sqlparser::ast::FunctionArg::Unnamed(func_arg_expr) => {
                                            match func_arg_expr {
                                                sqlparser::ast::FunctionArgExpr::Expr(expr) => {
                                                    self.expr_to_logical(&expr)
                                                }
                                                sqlparser::ast::FunctionArgExpr::Wildcard => {
                                                    // COUNT(*) uses Wildcard
                                                    Ok(LogicalExpr::Wildcard)
                                                }
                                                _ => Err(Error::query_execution("Complex function args not supported")),
                                            }
                                        }
                                        _ => Err(Error::query_execution("Named function args not supported")),
                                    }
                                })
                                .collect();
                            let distinct = matches!(
                                arg_list.duplicate_treatment,
                                Some(sqlparser::ast::DuplicateTreatment::Distinct)
                            );
                            (args?, distinct)
                        }
                        _ => (vec![], false),
                    };

                    Ok(Some(LogicalExpr::AggregateFunction { fun, args, distinct }))
                } else {
                    Ok(None)
                }
            }
            _ => Ok(None),
        }
    }

    /// Parse STRING_AGG(value, delimiter) aggregate function
    fn parse_string_agg(&self, func: &sqlparser::ast::Function) -> Result<Option<LogicalExpr>> {
        // Extract args from function
        let args = match &func.args {
            sqlparser::ast::FunctionArguments::List(arg_list) => {
                let parsed_args: Result<Vec<_>> = arg_list
                    .args
                    .iter()
                    .map(|arg| match arg {
                        sqlparser::ast::FunctionArg::Unnamed(func_arg_expr) => match func_arg_expr {
                            sqlparser::ast::FunctionArgExpr::Expr(expr) => self.expr_to_logical(&expr),
                            _ => Err(Error::query_execution("STRING_AGG requires value expressions")),
                        },
                        _ => Err(Error::query_execution("Named function args not supported")),
                    })
                    .collect();
                parsed_args?
            }
            _ => return Err(Error::query_execution("STRING_AGG requires arguments")),
        };

        // GROUP_CONCAT(x) — the MySQL form, and Pagila's custom aggregate of the
        // same name — defaults the separator to ',' when called with a single
        // argument. STRING_AGG always requires an explicit delimiter, matching
        // PostgreSQL. A two-argument call uses the given literal delimiter.
        let is_group_concat = func.name.to_string().eq_ignore_ascii_case("group_concat");
        let delimiter = match args.len() {
            2 => match args.get(1) {
                Some(LogicalExpr::Literal(crate::Value::String(s))) => s.clone(),
                _ => {
                    return Err(Error::query_execution(
                        "STRING_AGG delimiter must be a string literal",
                    ))
                }
            },
            1 if is_group_concat => ",".to_string(),
            _ => {
                return Err(Error::query_execution(
                    "STRING_AGG requires (value, delimiter); GROUP_CONCAT requires (value) or (value, delimiter)",
                ))
            }
        };

        let distinct = match &func.args {
            sqlparser::ast::FunctionArguments::List(arg_list) => {
                matches!(
                    arg_list.duplicate_treatment,
                    Some(sqlparser::ast::DuplicateTreatment::Distinct)
                )
            }
            _ => false,
        };

        let value_expr = args
            .first()
            .ok_or_else(|| Error::query_execution("STRING_AGG requires a value argument"))?;
        Ok(Some(LogicalExpr::AggregateFunction {
            fun: AggregateFunction::StringAgg { delimiter },
            args: vec![value_expr.clone()], // Only the value expression
            distinct,
        }))
    }

    /// Parse a window function expression
    fn parse_window_function(&self, func: &sqlparser::ast::Function) -> Result<LogicalExpr> {
        use super::logical_plan::{WindowFrame, WindowFrameType, WindowFunctionType};

        let func_name = func.name.to_string().to_uppercase();

        // Determine window function type
        let window_fun = match func_name.as_str() {
            "ROW_NUMBER" => WindowFunctionType::RowNumber,
            "RANK" => WindowFunctionType::Rank,
            "DENSE_RANK" => WindowFunctionType::DenseRank,
            "PERCENT_RANK" => WindowFunctionType::PercentRank,
            "CUME_DIST" => WindowFunctionType::CumeDist,
            "NTILE" => WindowFunctionType::Ntile,
            "LAG" => WindowFunctionType::Lag,
            "LEAD" => WindowFunctionType::Lead,
            "FIRST_VALUE" => WindowFunctionType::FirstValue,
            "LAST_VALUE" => WindowFunctionType::LastValue,
            "NTH_VALUE" => WindowFunctionType::NthValue,
            // Aggregate functions used as window functions
            "COUNT" => WindowFunctionType::Aggregate(AggregateFunction::Count),
            "SUM" => WindowFunctionType::Aggregate(AggregateFunction::Sum),
            "AVG" => WindowFunctionType::Aggregate(AggregateFunction::Avg),
            "MIN" => WindowFunctionType::Aggregate(AggregateFunction::Min),
            "MAX" => WindowFunctionType::Aggregate(AggregateFunction::Max),
            _ => {
                return Err(Error::query_execution(format!(
                    "Unknown window function: {}",
                    func_name
                )))
            }
        };

        // Parse function arguments
        let args = match &func.args {
            sqlparser::ast::FunctionArguments::List(arg_list) => arg_list
                .args
                .iter()
                .filter_map(|arg| match arg {
                    sqlparser::ast::FunctionArg::Unnamed(func_arg_expr) => match func_arg_expr {
                        sqlparser::ast::FunctionArgExpr::Expr(expr) => self.expr_to_logical(expr).ok(),
                        _ => None,
                    },
                    _ => None,
                })
                .collect(),
            _ => vec![],
        };

        // Parse OVER clause
        let over = func
            .over
            .as_ref()
            .ok_or_else(|| Error::query_execution("Window function requires OVER clause"))?;

        // Resolve the window specification: either inline or from a named window reference
        let resolved_spec: Option<sqlparser::ast::WindowSpec>;
        let spec_ref = match over {
            sqlparser::ast::WindowType::WindowSpec(spec) => spec,
            sqlparser::ast::WindowType::NamedWindow(name) => {
                resolved_spec = Some(
                    self.get_named_window(&name.value)
                        .ok_or_else(|| Error::query_execution(format!("Window \"{}\" is not defined", name.value)))?,
                );
                resolved_spec
                    .as_ref()
                    .ok_or_else(|| Error::query_execution("Internal error resolving named window"))?
            }
        };

        // Parse window specification fields from the resolved spec
        let partition_by: Vec<LogicalExpr> = spec_ref
            .partition_by
            .iter()
            .filter_map(|expr| self.expr_to_logical(expr).ok())
            .collect();

        let order_by: Vec<(LogicalExpr, bool)> = spec_ref
            .order_by
            .iter()
            .filter_map(|order_expr| {
                self.expr_to_logical(&order_expr.expr).ok().map(|expr| {
                    let ascending = order_expr.asc.unwrap_or(true);
                    (expr, ascending)
                })
            })
            .collect();

        let frame = spec_ref.window_frame.as_ref().map(|wf| {
            let frame_type = match wf.units {
                sqlparser::ast::WindowFrameUnits::Rows => WindowFrameType::Rows,
                sqlparser::ast::WindowFrameUnits::Range => WindowFrameType::Range,
                sqlparser::ast::WindowFrameUnits::Groups => WindowFrameType::Groups,
            };

            let start = Self::parse_frame_bound(&wf.start_bound);
            let end = wf.end_bound.as_ref().map(Self::parse_frame_bound);

            WindowFrame { frame_type, start, end }
        });

        Ok(LogicalExpr::WindowFunction {
            fun: window_fun,
            args,
            partition_by,
            order_by,
            frame,
        })
    }

    /// Parse a window frame bound
    fn parse_frame_bound(bound: &sqlparser::ast::WindowFrameBound) -> WindowFrameBound {
        use super::logical_plan::WindowFrameBound;

        match bound {
            sqlparser::ast::WindowFrameBound::CurrentRow => WindowFrameBound::CurrentRow,
            sqlparser::ast::WindowFrameBound::Preceding(None) => WindowFrameBound::UnboundedPreceding,
            sqlparser::ast::WindowFrameBound::Preceding(Some(expr)) => {
                if let sqlparser::ast::Expr::Value(sqlparser::ast::Value::Number(n, _)) = expr.as_ref() {
                    WindowFrameBound::Preceding(n.parse().unwrap_or(1))
                } else {
                    WindowFrameBound::Preceding(1)
                }
            }
            sqlparser::ast::WindowFrameBound::Following(None) => WindowFrameBound::UnboundedFollowing,
            sqlparser::ast::WindowFrameBound::Following(Some(expr)) => {
                if let sqlparser::ast::Expr::Value(sqlparser::ast::Value::Number(n, _)) = expr.as_ref() {
                    WindowFrameBound::Following(n.parse().unwrap_or(1))
                } else {
                    WindowFrameBound::Following(1)
                }
            }
        }
    }

    /// Convert sqlparser Expr to LogicalExpr (public wrapper for CHECK constraint evaluation)
    ///
    /// This method converts a sqlparser AST expression to a LogicalExpr that can be
    /// evaluated by the evaluator. Used by CHECK constraint validation.
    pub fn convert_expr_to_logical(
        &self,
        expr: &sqlparser::ast::Expr,
        _schema: Option<&crate::Schema>,
    ) -> Result<LogicalExpr> {
        self.expr_to_logical(expr)
    }

    /// Convert sqlparser Expr to LogicalExpr
    /// KanttBan #23 (v3.31.1 phase 2.3): unpack a PG literal-array
    /// expression like `'{int,int8,int2}'::regtype[]` into a list of
    /// LogicalExpr items suitable for the existing `InList` operator.
    /// Errors when the input isn't a recognised literal-array shape;
    /// callers (currently AnyOp) fall through to the original error
    /// path in that case.
    fn literal_array_to_list(&self, expr: &Expr) -> Result<Vec<LogicalExpr>> {
        use sqlparser::ast::{Expr as SqlExpr, Value as SqlValue};
        let (inner_str, elem_type) = match expr {
            SqlExpr::Cast {
                expr: inner, data_type, ..
            } => {
                let s = match inner.as_ref() {
                    SqlExpr::Value(SqlValue::SingleQuotedString(s)) => Self::repair_sqlparser_string(s),
                    _ => {
                        return Err(Error::query_execution(format!(
                            "ANY(array): only literal-array cast supported, got {:?}",
                            inner
                        )))
                    }
                };
                (s, data_type.clone())
            }
            _ => {
                return Err(Error::query_execution(format!(
                    "ANY(array): only literal-array cast supported, got {:?}",
                    expr
                )))
            }
        };
        // PG array literal syntax: `{a,b,c}` (curly-brace separated).
        let stripped = inner_str.trim_matches(|c: char| c == '{' || c == '}');
        let labels: Vec<&str> = stripped.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        // If the element type is a regtype (or its array form), map
        // each label to the PG OID. Otherwise keep as text — InList's
        // string-equality semantics will at least cover identical
        // textual comparisons.
        let is_regtype = format!("{:?}", elem_type).to_uppercase().contains("REGTYPE");
        let items: Vec<LogicalExpr> = labels
            .into_iter()
            .map(|s| {
                if is_regtype {
                    let oid = regtype_label_to_oid(s);
                    LogicalExpr::Literal(Value::Int4(oid))
                } else {
                    LogicalExpr::Literal(Value::String(s.to_string()))
                }
            })
            .collect();
        Ok(items)
    }

    fn array_expr_to_list(&self, expr: &Expr) -> Result<Vec<LogicalExpr>> {
        match expr {
            Expr::Array(sqlparser::ast::Array { elem, .. }) => elem
                .iter()
                .map(|item| self.expr_to_logical(item))
                .collect::<Result<Vec<_>>>(),
            Expr::Cast { expr: inner, .. } => self.array_expr_to_list(inner),
            _ => Err(Error::query_execution(format!(
                "ANY(array): unsupported array expression: {:?}",
                expr
            ))),
        }
    }

    fn runtime_any_array_expr(&self, expr: &Expr) -> Result<LogicalExpr> {
        use sqlparser::ast::{Expr as SqlExpr, Value as SqlValue};

        match expr {
            SqlExpr::Value(SqlValue::Placeholder(_)) => self.expr_to_logical(expr),
            SqlExpr::Cast { expr: inner, .. } if matches!(inner.as_ref(), SqlExpr::Value(SqlValue::Placeholder(_))) => {
                self.expr_to_logical(inner)
            }
            _ => Err(Error::query_execution(format!(
                "ANY(array): unsupported runtime array expression: {:?}",
                expr
            ))),
        }
    }

    pub(crate) fn expr_to_logical(&self, expr: &Expr) -> Result<LogicalExpr> {
        match expr {
            Expr::Identifier(ident) => Ok(LogicalExpr::Column {
                table: None,
                name: Self::normalize_ident(ident),
            }),

            Expr::CompoundIdentifier(idents) => {
                // Handle table.column references - preserve the table qualifier for JOIN disambiguation
                if idents.len() >= 2 {
                    // SAFETY: len() >= 2 guarantees len()-2 is valid
                    let table_alias = Self::normalize_ident(
                        idents
                            .get(idents.len() - 2)
                            .ok_or_else(|| Error::query_execution("Invalid compound identifier"))?,
                    );
                    let column_name = Self::normalize_ident(
                        idents
                            .last()
                            .ok_or_else(|| Error::query_execution("Empty compound identifier"))?,
                    );
                    Ok(LogicalExpr::Column {
                        table: Some(table_alias),
                        name: column_name,
                    })
                } else {
                    let column_name = Self::normalize_ident(
                        idents
                            .last()
                            .ok_or_else(|| Error::query_execution("Empty compound identifier"))?,
                    );
                    Ok(LogicalExpr::Column {
                        table: None,
                        name: column_name,
                    })
                }
            }

            Expr::Value(value) => {
                // Check if this is a parameter placeholder ($1, $2, etc.)
                match value {
                    sqlparser::ast::Value::Placeholder(placeholder) => {
                        // PostgreSQL-style parameter: $1, $2, etc.
                        if placeholder.starts_with('$') {
                            let index_str = placeholder.get(1..).unwrap_or_default();
                            let index = index_str.parse::<usize>().map_err(|_| {
                                Error::query_execution(format!(
                                    "Invalid parameter placeholder: {}. Expected format: $1, $2, etc.",
                                    placeholder
                                ))
                            })?;

                            if index == 0 {
                                return Err(Error::query_execution(
                                    "Parameter indices must be 1-based (e.g., $1, $2)",
                                ));
                            }

                            Ok(LogicalExpr::Parameter { index })
                        } else {
                            Err(Error::query_execution(format!(
                                "Unsupported placeholder format: {}. Use PostgreSQL-style $N placeholders",
                                placeholder
                            )))
                        }
                    }
                    _ => Ok(LogicalExpr::Literal(self.sql_value_to_value(value)?)),
                }
            }

            // Row / tuple constructor: (a, b, c) — used in keyset-style
            // comparisons like `WHERE (created_at, id) < ($1, $2)`.
            // sqlparser represents this as `Expr::Tuple(Vec<Expr>)` or,
            // for a single-element parenthesised expression, as
            // `Expr::Nested(Box<Expr>)`, which we don't treat as a tuple.
            Expr::Tuple(items) => {
                let logical: Vec<LogicalExpr> = items
                    .iter()
                    .map(|e| self.expr_to_logical(e))
                    .collect::<Result<Vec<_>>>()?;
                Ok(LogicalExpr::Tuple { items: logical })
            }

            Expr::BinaryOp { left, op, right } => {
                let left_expr = self.expr_to_logical(left)?;
                let right_expr = self.expr_to_logical(right)?;
                let logical_op = self.sql_binary_op_to_logical(op)?;

                Ok(LogicalExpr::BinaryExpr {
                    left: Box::new(left_expr),
                    op: logical_op,
                    right: Box::new(right_expr),
                })
            }

            Expr::UnaryOp { op, expr } => {
                let logical_expr = self.expr_to_logical(expr)?;
                let logical_op = self.sql_unary_op_to_logical(op)?;

                Ok(LogicalExpr::UnaryExpr {
                    op: logical_op,
                    expr: Box::new(logical_expr),
                })
            }

            Expr::IsNull(expr) => Ok(LogicalExpr::IsNull {
                expr: Box::new(self.expr_to_logical(expr)?),
                is_null: true,
            }),

            Expr::IsNotNull(expr) => Ok(LogicalExpr::IsNull {
                expr: Box::new(self.expr_to_logical(expr)?),
                is_null: false,
            }),

            Expr::IsDistinctFrom(left, right) => Ok(LogicalExpr::BinaryExpr {
                left: Box::new(self.expr_to_logical(left)?),
                op: BinaryOperator::IsDistinctFrom,
                right: Box::new(self.expr_to_logical(right)?),
            }),

            Expr::IsNotDistinctFrom(left, right) => Ok(LogicalExpr::BinaryExpr {
                left: Box::new(self.expr_to_logical(left)?),
                op: BinaryOperator::IsNotDistinctFrom,
                right: Box::new(self.expr_to_logical(right)?),
            }),

            Expr::Like {
                negated, expr, pattern, ..
            } => {
                let left_expr = self.expr_to_logical(expr)?;
                let right_expr = self.expr_to_logical(pattern)?;
                let op = if *negated {
                    BinaryOperator::NotLike
                } else {
                    BinaryOperator::Like
                };

                Ok(LogicalExpr::BinaryExpr {
                    left: Box::new(left_expr),
                    op,
                    right: Box::new(right_expr),
                })
            }

            Expr::ILike {
                negated, expr, pattern, ..
            } => {
                let left_expr = self.expr_to_logical(expr)?;
                let right_expr = self.expr_to_logical(pattern)?;
                let op = if *negated {
                    BinaryOperator::NotILike
                } else {
                    BinaryOperator::ILike
                };

                Ok(LogicalExpr::BinaryExpr {
                    left: Box::new(left_expr),
                    op,
                    right: Box::new(right_expr),
                })
            }

            Expr::SimilarTo {
                negated, expr, pattern, ..
            } => {
                let left_expr = self.expr_to_logical(expr)?;
                let right_expr = self.expr_to_logical(pattern)?;
                let op = if *negated {
                    BinaryOperator::NotSimilarTo
                } else {
                    BinaryOperator::SimilarTo
                };

                Ok(LogicalExpr::BinaryExpr {
                    left: Box::new(left_expr),
                    op,
                    right: Box::new(right_expr),
                })
            }

            Expr::RLike {
                negated, expr, pattern, ..
            } => {
                // RLike is MySQL's regex syntax
                let left_expr = self.expr_to_logical(expr)?;
                let right_expr = self.expr_to_logical(pattern)?;
                let op = if *negated {
                    BinaryOperator::NotRegexMatch
                } else {
                    BinaryOperator::RegexMatch
                };

                Ok(LogicalExpr::BinaryExpr {
                    left: Box::new(left_expr),
                    op,
                    right: Box::new(right_expr),
                })
            }

            Expr::Function(func) => {
                // Check if this is a window function (has OVER clause)
                if func.over.is_some() {
                    return self.parse_window_function(func);
                }

                // Check if it's an aggregate function
                if let Some(aggr) = self.extract_aggregate_from_expr(expr)? {
                    return Ok(aggr);
                }

                // Otherwise treat as scalar function
                let args = match &func.args {
                    sqlparser::ast::FunctionArguments::List(arg_list) => {
                        let args: Result<Vec<_>> = arg_list
                            .args
                            .iter()
                            .map(|arg| match arg {
                                sqlparser::ast::FunctionArg::Unnamed(func_arg_expr) => match func_arg_expr {
                                    sqlparser::ast::FunctionArgExpr::Expr(expr) => self.expr_to_logical(&expr),
                                    _ => Err(Error::query_execution("Complex function args not supported")),
                                },
                                _ => Err(Error::query_execution("Named function args not supported")),
                            })
                            .collect();
                        args?
                    }
                    _ => vec![],
                };

                Ok(LogicalExpr::ScalarFunction {
                    fun: func.name.to_string(),
                    args,
                })
            }

            Expr::Interval(interval) => Ok(LogicalExpr::Literal(Value::Interval(Self::interval_to_micros(
                interval,
            )?))),

            // Array literals: ARRAY[1, 2, 3] or '[1.0, 2.0, 3.0]' (for vectors)
            Expr::Array(sqlparser::ast::Array { elem, .. }) => {
                // Check if all elements are numeric - could be vector or array
                let all_numeric = elem
                    .iter()
                    .all(|e| matches!(e, Expr::Value(sqlparser::ast::Value::Number(_, _))));
                let has_floats = elem.iter().any(|e| {
                    if let Expr::Value(sqlparser::ast::Value::Number(n, _)) = e {
                        n.contains('.')
                    } else {
                        false
                    }
                });

                // If all floats, treat as vector for vector search compatibility
                if all_numeric && has_floats {
                    let elements: Result<Vec<f32>> = elem
                        .iter()
                        .map(|e| match e {
                            Expr::Value(sqlparser::ast::Value::Number(n, _)) => n
                                .parse::<f32>()
                                .map_err(|e| Error::query_execution(format!("Invalid vector element: {}", e))),
                            _ => Err(Error::query_execution("Vector elements must be numbers")),
                        })
                        .collect();
                    Ok(LogicalExpr::Literal(Value::Vector(elements?)))
                } else {
                    // General array - convert each element to a Value
                    let elements: Result<Vec<Value>> = elem
                        .iter()
                        .map(|e| match e {
                            Expr::Value(sqlparser::ast::Value::Number(n, _)) => {
                                if let Ok(i) = n.parse::<i32>() {
                                    Ok(Value::Int4(i))
                                } else if let Ok(f) = n.parse::<f64>() {
                                    Ok(Value::Float8(f))
                                } else {
                                    Err(Error::query_execution(format!("Invalid array element: {}", n)))
                                }
                            }
                            Expr::Value(sqlparser::ast::Value::SingleQuotedString(s)) => {
                                Ok(Value::String(Self::repair_sqlparser_string(s)))
                            }
                            Expr::Value(sqlparser::ast::Value::Boolean(b)) => Ok(Value::Boolean(*b)),
                            Expr::Value(sqlparser::ast::Value::Null) => Ok(Value::Null),
                            _ => Err(Error::query_execution("Unsupported array element type")),
                        })
                        .collect();
                    Ok(LogicalExpr::Literal(Value::Array(elements?)))
                }
            }

            // CAST expressions: CAST(expr AS type) or expr::type
            Expr::Cast { expr, data_type, .. } => {
                let logical_expr = self.expr_to_logical(expr)?;
                // ada-core compat: pgvector accepts the bare `'[1,2,3]'::vector`
                // form (no dimension) and infers the dimension from the literal.
                // Match that behavior at the CAST site so every pgvector tutorial
                // / drizzle / asyncpg snippet works out of the box. DDL still
                // requires explicit `VECTOR(n)` (sql_data_type_to_data_type at
                // planner.rs:3591 enforces it for column types).
                let target_type = match (data_type, expr.as_ref()) {
                    (sqlparser::ast::DataType::Custom(name, modifiers), inner)
                        if modifiers.is_empty() && name.to_string().eq_ignore_ascii_case("vector") =>
                    {
                        if let sqlparser::ast::Expr::Value(sqlparser::ast::Value::SingleQuotedString(s)) = inner {
                            // Infer dimension by counting elements of the literal.
                            let trimmed = s.trim();
                            let inner_str = trimmed.trim_start_matches('[').trim_end_matches(']');
                            let count = inner_str.split(',').map(str::trim).filter(|t| !t.is_empty()).count();
                            if count > 0 {
                                DataType::Vector(count)
                            } else {
                                return Err(Error::query_execution(
                                    "bare ::vector cast requires a non-empty literal to infer dimension; \
                                     use ::vector(N) for parameter or column-typed sources"
                                        .to_string(),
                                ));
                            }
                        } else {
                            return Err(Error::query_execution(
                                "bare ::vector cast can only infer dimension from a literal string; \
                                 use ::vector(N) explicitly otherwise"
                                    .to_string(),
                            ));
                        }
                    }
                    _ => self.sql_data_type_to_data_type(data_type)?,
                };
                Ok(LogicalExpr::Cast {
                    expr: Box::new(logical_expr),
                    data_type: target_type,
                })
            }

            // IN list: expr IN (val1, val2, ...)
            Expr::InList { expr, list, negated } => {
                let logical_expr = self.expr_to_logical(expr)?;
                let logical_list: Vec<LogicalExpr> = list
                    .iter()
                    .map(|e| self.expr_to_logical(e))
                    .collect::<Result<Vec<_>>>()?;
                Ok(LogicalExpr::InList {
                    expr: Box::new(logical_expr),
                    list: logical_list,
                    negated: *negated,
                })
            }

            // KanttBan #23 (v3.31.1 phase 2.3): `x = ANY(arr)` and
            // `x <> ANY(arr)` — equivalent to `x IN (a, b, …)`
            // for literal arrays, with runtime expansion for bound
            // parameter arrays. drizzle-kit's getColumnsInfoQuery uses
            // `a.atttypid = ANY ('{int,int8,int2}'::regtype[])` for
            // SERIAL detection. Convert at plan time so the existing
            // InList operator handles evaluation.
            //
            // Column-array and subquery-array ANY still need true
            // executor support and fall back in the AnyOp arm below.
            Expr::AnyOp {
                left,
                compare_op,
                right,
                is_some: _,
            } => {
                use sqlparser::ast::BinaryOperator;
                let negated = matches!(compare_op, BinaryOperator::NotEq);
                // Literal arrays become a normal InList. Parameter arrays use
                // an internal marker that the InList evaluator expands at
                // runtime after parameter binding. Column/subquery arrays
                // still fall back to constant false; full array-column ANY
                // support is a bigger executor feature.
                let logical_expr = self.expr_to_logical(left)?;
                match self
                    .literal_array_to_list(right)
                    .or_else(|_| self.array_expr_to_list(right))
                {
                    Ok(list) => Ok(LogicalExpr::InList {
                        expr: Box::new(logical_expr),
                        list,
                        negated,
                    }),
                    Err(_) => match self.runtime_any_array_expr(right) {
                        Ok(array_expr) => Ok(LogicalExpr::InList {
                            expr: Box::new(logical_expr),
                            list: vec![LogicalExpr::ScalarFunction {
                                fun: ANY_ARRAY_MARKER_FUNCTION.to_string(),
                                args: vec![array_expr],
                            }],
                            negated,
                        }),
                        Err(_) => {
                            // The left expression may have side-effects
                            // worth preserving in the plan; eagerly
                            // evaluating to a constant boolean drops it.
                            // For drizzle's usage the left is also a
                            // simple column reference, so dropping is
                            // benign.
                            Ok(LogicalExpr::Literal(Value::Boolean(negated)))
                        }
                    },
                }
            }

            // Scalar subquery: `(SELECT col FROM t WHERE ...)` used as
            // an expression. We don't materialise here because the
            // caller (e.g. UPDATE SET) needs to supply the outer-row
            // context for correlation. At simple-evaluation sites the
            // evaluator materialises uncorrelated cases via
            // `materialize_subqueries`.
            Expr::Subquery(subquery) => {
                let plan = self.query_to_plan((**subquery).clone())?;
                Ok(LogicalExpr::ScalarSubquery {
                    subquery: Box::new(plan),
                })
            }

            // IN subquery: expr IN (SELECT ...)
            Expr::InSubquery {
                expr,
                subquery,
                negated,
            } => {
                // Convert subquery to a logical plan (dereference Box and clone Query)
                let subquery_plan = self.query_to_plan((**subquery).clone())?;
                let logical_expr = self.expr_to_logical(expr)?;
                Ok(LogicalExpr::InSubquery {
                    expr: Box::new(logical_expr),
                    subquery: Box::new(subquery_plan),
                    negated: *negated,
                })
            }

            // EXISTS subquery: EXISTS (SELECT ...)
            Expr::Exists { subquery, negated } => {
                let subquery_plan = self.query_to_plan((**subquery).clone())?;
                Ok(LogicalExpr::Exists {
                    subquery: Box::new(subquery_plan),
                    negated: *negated,
                })
            }

            // BETWEEN: expr BETWEEN low AND high
            Expr::Between {
                expr,
                negated,
                low,
                high,
            } => Ok(LogicalExpr::Between {
                expr: Box::new(self.expr_to_logical(expr)?),
                low: Box::new(self.expr_to_logical(low)?),
                high: Box::new(self.expr_to_logical(high)?),
                negated: *negated,
            }),

            // CASE expression
            Expr::Case {
                operand,
                conditions,
                results,
                else_result,
            } => {
                // Convert operand (if present)
                let expr = if let Some(op) = operand {
                    Some(Box::new(self.expr_to_logical(op)?))
                } else {
                    None
                };

                // Convert WHEN conditions and THEN results
                let when_then: Vec<(LogicalExpr, LogicalExpr)> = conditions
                    .iter()
                    .zip(results.iter())
                    .map(|(cond, res)| Ok((self.expr_to_logical(cond)?, self.expr_to_logical(res)?)))
                    .collect::<Result<Vec<_>>>()?;

                // Convert ELSE result (if present)
                let else_result = if let Some(e) = else_result {
                    Some(Box::new(self.expr_to_logical(e)?))
                } else {
                    None
                };

                Ok(LogicalExpr::Case {
                    expr,
                    when_then,
                    else_result,
                })
            }

            // Parenthesized expressions: (expr) → unwrap and recurse
            Expr::Nested(inner) => self.expr_to_logical(inner),

            // EXTRACT(<field> FROM <expr>) — lower to a scalar function
            // call `__extract_<field>(expr)`. The evaluator has dedicated
            // handlers for each field and returns the same type as
            // stock Postgres (double precision for EPOCH, int8 for
            // everything else).
            Expr::Extract { field, expr: inner, .. } => {
                let arg = self.expr_to_logical(inner)?;
                let fun_name = format!("__extract_{}", format!("{field:?}").to_lowercase());
                Ok(LogicalExpr::ScalarFunction {
                    fun: fun_name,
                    args: vec![arg],
                })
            }

            // TYPE 'literal' forms — `TIMESTAMP '2026-01-01'`,
            // `DATE '2026-01-01'`, `TIME '12:00:00'`, `BOOL 'true'`.
            // Lower to a CAST so the evaluator's existing coercion
            // machinery handles the parse.
            Expr::TypedString { data_type, value } => {
                let dt = self.sql_data_type_to_data_type(data_type)?;
                Ok(LogicalExpr::Cast {
                    expr: Box::new(LogicalExpr::Literal(Value::String(value.clone()))),
                    data_type: dt,
                })
            }

            // Quirk D from the dashboard cutover: SQL-standard
            // `SUBSTRING (s FROM x [FOR y])` and the special bracket form
            // are first-class AST nodes in sqlparser, distinct from the
            // function form `SUBSTR(s, x, y)`. Desugar them into the same
            // `substr` ScalarFunction the executor already handles.
            //
            // - `SUBSTRING(s FROM x FOR y)` → `substr(s, x, y)`
            // - `SUBSTRING(s FROM x)`       → `substr(s, x)` (open-ended)
            // - `SUBSTRING(s FOR y)`        → `substr(s, 1, y)` (start at 1)
            Expr::Substring {
                expr: inner,
                substring_from,
                substring_for,
                ..
            } => {
                let s = self.expr_to_logical(inner)?;
                let mut args = vec![s];
                match (substring_from, substring_for) {
                    (Some(from), Some(for_)) => {
                        args.push(self.expr_to_logical(from)?);
                        args.push(self.expr_to_logical(for_)?);
                    }
                    (Some(from), None) => {
                        args.push(self.expr_to_logical(from)?);
                    }
                    (None, Some(for_)) => {
                        args.push(LogicalExpr::Literal(Value::Int8(1)));
                        args.push(self.expr_to_logical(for_)?);
                    }
                    (None, None) => {} // SUBSTRING(s) with no slice — pass-through
                }
                Ok(LogicalExpr::ScalarFunction {
                    fun: "substr".to_string(),
                    args,
                })
            }

            _ => Err(Error::query_execution(format!(
                "Expression not yet supported: {:?}",
                expr
            ))),
        }
    }

    /// sqlparser currently returns non-ASCII string literals as UTF-8 bytes
    /// widened through Latin-1 in some paths (for example `a—b` becomes
    /// `aâ\u{80}\u{94}b`). Collapse that representation back to UTF-8 when
    /// it is present, while leaving already-correct Unicode and plain ASCII
    /// untouched.
    fn repair_sqlparser_string(s: &str) -> String {
        let mut bytes = Vec::with_capacity(s.len());
        for ch in s.chars() {
            let code = ch as u32;
            if code > u8::MAX as u32 {
                return s.to_string();
            }
            bytes.push(code as u8);
        }
        match String::from_utf8(bytes) {
            Ok(decoded) if decoded != s => decoded,
            _ => s.to_string(),
        }
    }

    fn interval_to_micros(interval: &sqlparser::ast::Interval) -> Result<i64> {
        let value = match interval.value.as_ref() {
            Expr::Value(sqlparser::ast::Value::SingleQuotedString(s)) => Self::repair_sqlparser_string(s),
            Expr::Value(sqlparser::ast::Value::Number(n, _)) => n.clone(),
            other => {
                return Err(Error::query_execution(format!(
                    "Unsupported interval literal value: {other:?}"
                )))
            }
        };

        if let Some(field) = interval.leading_field.as_ref() {
            let amount = value
                .trim()
                .parse::<f64>()
                .map_err(|e| Error::query_execution(format!("Invalid interval value '{value}': {e}")))?;
            return Self::interval_unit_to_micros(amount, field);
        }

        Self::parse_interval_text(&value)
    }

    fn parse_interval_text(value: &str) -> Result<i64> {
        let tokens: Vec<&str> = value.split_whitespace().collect();
        if tokens.is_empty() || tokens.len() % 2 != 0 {
            return Err(Error::query_execution(format!(
                "Unsupported interval literal '{value}'. Expected '<number> <unit>' pairs"
            )));
        }

        let mut micros = 0f64;
        for pair in tokens.chunks_exact(2) {
            let amount = pair[0]
                .parse::<f64>()
                .map_err(|e| Error::query_execution(format!("Invalid interval amount '{}': {e}", pair[0])))?;
            micros += Self::interval_unit_name_to_micros(amount, pair[1])? as f64;
        }

        Ok(micros.round() as i64)
    }

    fn interval_unit_to_micros(amount: f64, field: &sqlparser::ast::DateTimeField) -> Result<i64> {
        use sqlparser::ast::DateTimeField;
        let micros = match field {
            // Calendar fields have no fixed microsecond width. Nano stores
            // intervals as a single i64 microsecond count (no months/years
            // component), so we APPROXIMATE: YEAR -> 365 days, MONTH -> 30 days.
            // Imprecise across leap years / variable months (e.g.
            // `DATE '2020-01-01' + INTERVAL '2' YEAR` lands on 2021-12-31), but
            // it unblocks Oracle/PostgreSQL migrations using `<date> + INTERVAL
            // 'N' YEAR/MONTH`. A precise calendar interval would require a months
            // field threaded through Value::Interval.
            DateTimeField::Year => amount * 365.0 * 86_400_000_000.0,
            DateTimeField::Month => amount * 30.0 * 86_400_000_000.0,
            DateTimeField::Week(_) => amount * 7.0 * 86_400_000_000.0,
            DateTimeField::Day => amount * 86_400_000_000.0,
            DateTimeField::Hour => amount * 3_600_000_000.0,
            DateTimeField::Minute => amount * 60_000_000.0,
            DateTimeField::Second => amount * 1_000_000.0,
            DateTimeField::Millisecond | DateTimeField::Milliseconds => amount * 1_000.0,
            DateTimeField::Microsecond | DateTimeField::Microseconds => amount,
            DateTimeField::Custom(ident) => return Self::interval_unit_name_to_micros(amount, &ident.value),
            other => {
                return Err(Error::query_execution(format!(
                    "Unsupported interval field {other}. Nano intervals are stored as microseconds"
                )))
            }
        };
        Ok(micros.round() as i64)
    }

    fn interval_unit_name_to_micros(amount: f64, unit: &str) -> Result<i64> {
        let lower = unit.trim().to_ascii_lowercase();
        let normalized = match lower.as_str() {
            "ms" | "us" => lower.as_str(),
            _ => lower.trim_end_matches('s'),
        };
        let micros = match normalized {
            "us" | "usec" | "microsecond" => amount,
            "ms" | "msec" | "millisecond" => amount * 1_000.0,
            "s" | "sec" | "second" => amount * 1_000_000.0,
            "m" | "min" | "minute" => amount * 60_000_000.0,
            "h" | "hr" | "hour" => amount * 3_600_000_000.0,
            "d" | "day" => amount * 86_400_000_000.0,
            "w" | "week" => amount * 7.0 * 86_400_000_000.0,
            // Calendar approximations, matching interval_unit_to_micros:
            // MONTH -> 30 days, YEAR -> 365 days (imprecise; Nano has no months
            // component in Value::Interval).
            "month" | "mon" => amount * 30.0 * 86_400_000_000.0,
            "year" | "yr" => amount * 365.0 * 86_400_000_000.0,
            _ => {
                return Err(Error::query_execution(format!(
                    "Unsupported interval unit '{unit}'. Nano intervals are stored as microseconds"
                )))
            }
        };
        Ok(micros.round() as i64)
    }

    /// Convert SQL value to internal Value
    /// Type a numeric literal exactly as `sql_value_to_value` does: try i32,
    /// then i64, then f64, then f32. Shared with the query-normalizer (A2) so a
    /// normalized `$n` parameter and the inline literal it replaced can never
    /// diverge in type.
    pub(crate) fn number_literal_to_value(text: &str) -> Result<Value> {
        if let Ok(i) = text.parse::<i32>() {
            Ok(Value::Int4(i))
        } else if let Ok(i) = text.parse::<i64>() {
            Ok(Value::Int8(i))
        } else if let Ok(f) = text.parse::<f64>() {
            Ok(Value::Float8(f))
        } else if let Ok(f) = text.parse::<f32>() {
            Ok(Value::Float4(f))
        } else {
            Err(Error::query_execution(format!("Invalid number: {}", text)))
        }
    }

    /// Wrap already-unquoted single-quoted-string content into a `Value`,
    /// exactly as `sql_value_to_value` does. Shared with the query-normalizer.
    pub(crate) fn single_quoted_content_to_value(content: &str) -> Value {
        Value::String(Self::repair_sqlparser_string(content))
    }

    fn sql_value_to_value(&self, value: &sqlparser::ast::Value) -> Result<Value> {
        match value {
            sqlparser::ast::Value::Number(n, _) => Self::number_literal_to_value(n),
            sqlparser::ast::Value::SingleQuotedString(s) => Ok(Self::single_quoted_content_to_value(s)),
            // C-style escaped string `E'...'` and unicode string `U&'...'`.
            // sqlparser has already processed their escape sequences, so the
            // stored content is the final text — lift it straight to a String
            // (no `repair_sqlparser_string`, which is for doubled `''` quotes).
            // Needed because psycopg renders bytea parameters as escaped
            // literals (e.g. `E'\\x89504e470001'`), which the CDC replicat's
            // INSERT ... ON CONFLICT upsert emits — previously rejected with
            // "Value type not yet supported: EscapedStringLiteral".
            sqlparser::ast::Value::EscapedStringLiteral(s)
            | sqlparser::ast::Value::UnicodeStringLiteral(s) => Ok(Value::String(s.clone())),
            // Dollar-quoted strings: `$$hello$$` or `$tag$hello$tag$`. The
            // content is already safely delimited by the parser, so we
            // just lift it to a plain String value — the tag is
            // discarded.
            sqlparser::ast::Value::DollarQuotedString(dqs) => {
                Ok(Value::String(Self::repair_sqlparser_string(&dqs.value)))
            }
            sqlparser::ast::Value::Boolean(b) => Ok(Value::Boolean(*b)),
            sqlparser::ast::Value::Null => Ok(Value::Null),
            _ => Err(Error::query_execution(format!(
                "Value type not yet supported: {:?}",
                value
            ))),
        }
    }

    /// Convert SQL binary operator to logical operator
    fn sql_binary_op_to_logical(&self, op: &SqlBinaryOp) -> Result<BinaryOperator> {
        match op {
            SqlBinaryOp::Plus => Ok(BinaryOperator::Plus),
            SqlBinaryOp::Minus => Ok(BinaryOperator::Minus),
            SqlBinaryOp::Multiply => Ok(BinaryOperator::Multiply),
            SqlBinaryOp::Divide => Ok(BinaryOperator::Divide),
            SqlBinaryOp::Modulo => Ok(BinaryOperator::Modulo),
            SqlBinaryOp::Eq => Ok(BinaryOperator::Eq),
            SqlBinaryOp::NotEq => Ok(BinaryOperator::NotEq),
            SqlBinaryOp::Lt => Ok(BinaryOperator::Lt),
            SqlBinaryOp::LtEq => Ok(BinaryOperator::LtEq),
            SqlBinaryOp::Gt => Ok(BinaryOperator::Gt),
            SqlBinaryOp::GtEq => Ok(BinaryOperator::GtEq),
            SqlBinaryOp::And => Ok(BinaryOperator::And),
            SqlBinaryOp::Or => Ok(BinaryOperator::Or),
            // PostgreSQL JSONB operators
            SqlBinaryOp::Arrow => Ok(BinaryOperator::JsonGet),
            SqlBinaryOp::LongArrow => Ok(BinaryOperator::JsonGetText),
            SqlBinaryOp::AtArrow => Ok(BinaryOperator::JsonContains),
            SqlBinaryOp::ArrowAt => Ok(BinaryOperator::JsonContainedBy),
            SqlBinaryOp::Question => Ok(BinaryOperator::JsonExists),
            SqlBinaryOp::QuestionPipe => Ok(BinaryOperator::JsonExistsAny),
            SqlBinaryOp::QuestionAnd => Ok(BinaryOperator::JsonExistsAll),
            // PostgreSQL vector operators
            SqlBinaryOp::Spaceship => Ok(BinaryOperator::VectorCosineDistance),
            SqlBinaryOp::HashArrow => Ok(BinaryOperator::VectorInnerProduct),
            // String concatenation operator: ||
            SqlBinaryOp::StringConcat => Ok(BinaryOperator::StringConcat),
            // Postgres FTS match operator: tsvector @@ tsquery
            SqlBinaryOp::AtAt => Ok(BinaryOperator::TsMatch),
            // Vector similarity operators (pgvector compatible)
            SqlBinaryOp::Custom(op_str) => match op_str.as_str() {
                "<->" => Ok(BinaryOperator::VectorL2Distance),
                "<=>" => Ok(BinaryOperator::VectorCosineDistance),
                "<#>" => Ok(BinaryOperator::VectorInnerProduct),
                _ => Err(Error::query_execution(format!(
                    "Custom operator not supported: {}",
                    op_str
                ))),
            },
            // LIKE is not a binary operator in sqlparser v0.53+, handle separately
            _ => Err(Error::query_execution(format!(
                "Binary operator not yet supported: {:?}",
                op
            ))),
        }
    }

    /// Convert SQL unary operator to logical operator
    fn sql_unary_op_to_logical(&self, op: &SqlUnaryOp) -> Result<UnaryOperator> {
        match op {
            SqlUnaryOp::Not => Ok(UnaryOperator::Not),
            SqlUnaryOp::Minus => Ok(UnaryOperator::Minus),
            SqlUnaryOp::Plus => Ok(UnaryOperator::Plus),
            _ => Err(Error::query_execution(format!(
                "Unary operator not yet supported: {:?}",
                op
            ))),
        }
    }

    /// Convert INSERT statement to plan
    fn insert_to_plan(
        &self,
        table_name: String,
        columns: Vec<sqlparser::ast::Ident>,
        source: Box<Query>,
        returning: Option<Vec<ReturningItem>>,
        on_conflict: Option<OnConflictAction>,
    ) -> Result<LogicalPlan> {
        // Extract VALUES from query
        if let SetExpr::Values(values) = *source.body {
            let column_names = if columns.is_empty() {
                None
            } else {
                Some(columns.iter().map(Self::normalize_ident).collect())
            };

            let rows: Result<Vec<Vec<LogicalExpr>>> = values
                .rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|expr| {
                            // SQL `DEFAULT` keyword in a VALUES list:
                            // sqlparser classifies it as
                            // `Expr::Identifier` because `DEFAULT` isn't
                            // a keyword in the expression grammar.
                            // Emit a dedicated `DefaultValue` marker so
                            // the INSERT executor can fall through to
                            // the column's declared DEFAULT expression
                            // (or NULL if none) — matching stock
                            // PostgreSQL semantics. Drizzle emits
                            // `VALUES (default, ...)` for every INSERT.
                            if let Expr::Identifier(ident) = expr {
                                if ident.value.eq_ignore_ascii_case("DEFAULT") {
                                    return Ok(LogicalExpr::DefaultValue);
                                }
                            }
                            self.expr_to_logical(expr)
                        })
                        .collect()
                })
                .collect();

            Ok(LogicalPlan::Insert {
                table_name,
                columns: column_names,
                values: rows?,
                returning,
                on_conflict,
            })
        } else {
            // INSERT ... SELECT: plan the source query
            let column_names = if columns.is_empty() {
                None
            } else {
                Some(columns.iter().map(Self::normalize_ident).collect::<Vec<String>>())
            };

            let source_plan = self.query_to_plan(*source)?;

            // Validate column count: SELECT output columns must match target columns
            let source_col_count = source_plan.schema().columns.len();
            if let Some(ref cols) = column_names {
                // Explicit column list specified
                if source_col_count != cols.len() {
                    return Err(Error::query_execution(format!(
                        "INSERT ... SELECT column count mismatch: {} target columns but SELECT returns {} columns",
                        cols.len(),
                        source_col_count
                    )));
                }
            } else if let Some(catalog) = self.catalog {
                // No explicit column list - validate against table schema
                let table_schema = catalog.get_table_schema(&table_name)?;
                if source_col_count != table_schema.columns.len() {
                    return Err(Error::query_execution(format!(
                        "INSERT ... SELECT column count mismatch: table '{}' has {} columns but SELECT returns {} columns",
                        table_name, table_schema.columns.len(), source_col_count
                    )));
                }
            }

            Ok(LogicalPlan::InsertSelect {
                table_name,
                columns: column_names,
                source: Box::new(source_plan),
                returning,
            })
        }
    }

    /// Convert CREATE TABLE to plan
    fn create_table_to_plan(
        &self,
        name: String,
        columns: Vec<SqlColumnDef>,
        if_not_exists: bool,
        sql_constraints: Vec<sqlparser::ast::TableConstraint>,
        with_options: Vec<sqlparser::ast::SqlOption>,
    ) -> Result<LogicalPlan> {
        // Stage-0 partitioning: a child `CREATE TABLE child PARTITION OF parent
        // …` was rewritten to an empty-column `CREATE TABLE child ()` by
        // `Parser::preprocess_partition_of` so sqlparser 0.53 accepts it. This
        // is the first layer with catalog access, so here we clone the parent's
        // columns and materialize `child` as an independent, self-consistent
        // standalone table (Stage-0 flatten: no tuple routing; direct child
        // DML/SELECT/DROP are correct immediately). Triggered strictly when the
        // ORIGINAL SQL carries `PARTITION OF` for THIS child name — a plain
        // `CREATE TABLE t ()` (including `… () INHERITS (…)`) has no such spec
        // and falls through to the ordinary empty-table path unchanged.
        if columns.is_empty() {
            if let (Some(catalog), Some(orig)) = (self.catalog, self.original_sql.as_deref()) {
                if let Some(spec) = crate::sql::Parser::extract_partition_of(orig) {
                    if Self::normalize_partition_name(&spec.child) == name {
                        return self.partition_of_to_plan(name, if_not_exists, &spec, catalog);
                    }
                }
            }
        }

        // Extract storage modes from original SQL if available
        let storage_modes = if let Some(ref sql) = self.original_sql {
            crate::sql::Parser::extract_column_storage_modes(sql)
        } else {
            std::collections::HashMap::new()
        };

        let mut column_defs: Vec<_> = columns
            .iter()
            .map(|col| self.sql_column_def_to_column_def(col))
            .collect::<Result<Vec<_>>>()?;

        // Apply storage modes from original SQL
        for col_def in &mut column_defs {
            if let Some(mode) = storage_modes.get(&col_def.name) {
                col_def.storage_mode = *mode;
            }
        }

        // Convert sqlparser constraints to our TableConstraint type
        let mut constraints: Vec<_> = sql_constraints
            .iter()
            .filter_map(|c| self.convert_table_constraint(c, &name))
            .collect();

        // Also extract inline REFERENCES and CHECK from column definitions
        for col in &columns {
            for option in &col.options {
                match &option.option {
                    ColumnOption::ForeignKey {
                        foreign_table,
                        referred_columns,
                        on_delete,
                        on_update,
                        characteristics,
                    } => {
                        // Normalize the referenced table / columns (B36).
                        // See the matching comment on the table-level FK
                        // branch in `convert_table_constraint`.
                        let fk_constraint = TableConstraint::ForeignKey {
                            name: None,
                            columns: vec![Self::normalize_ident(&col.name)],
                            references_table: Self::normalize_object_name(foreign_table),
                            references_columns: referred_columns.iter().map(Self::normalize_ident).collect(),
                            on_delete: on_delete.as_ref().map(|a| convert_referential_action(a)),
                            on_update: on_update.as_ref().map(|a| convert_referential_action(a)),
                            deferrable: false,
                            initially_deferred: false,
                            enforcement: convert_constraint_enforcement(characteristics.as_ref()),
                        };
                        constraints.push(fk_constraint);
                    }
                    ColumnOption::Check(expr) => {
                        // Extract column-level CHECK constraint
                        if let Ok(logical_expr) = self.expr_to_logical(expr) {
                            let check_constraint = TableConstraint::Check {
                                name: None,
                                expression: logical_expr,
                            };
                            constraints.push(check_constraint);
                        }
                    }
                    _ => {}
                }
            }
        }

        // KanttBan #20 (v3.31.0): generate the implicit
        // `CHECK (col IN ('a','b',…))` constraint for columns whose
        // declared type was a registered enum. The column's resolved
        // DataType is already TEXT (rewritten in
        // sql_data_type_to_data_type); the labels list lives in the
        // catalog. We scan the original parsed columns to identify
        // which ones were enum-typed, then append one Check
        // TableConstraint per enum column.
        if let Some(catalog) = self.catalog {
            for col in &columns {
                if let SqlDataType::Custom(object_name, _) = &col.data_type {
                    let raw_name = object_name.to_string();
                    let lookup_name = Self::dealias_schema(&raw_name).unwrap_or(raw_name);
                    if let Some(labels) = catalog.get_enum_labels(&lookup_name)? {
                        let col_name = Self::normalize_ident(&col.name);
                        let expression = LogicalExpr::InList {
                            expr: Box::new(LogicalExpr::Column {
                                table: None,
                                name: col_name,
                            }),
                            list: labels
                                .into_iter()
                                .map(|l| LogicalExpr::Literal(Value::String(l)))
                                .collect(),
                            negated: false,
                        };
                        constraints.push(TableConstraint::Check { name: None, expression });
                    }
                }
            }
        }

        // Propagate table-level PRIMARY KEY constraint to column defs.
        // WordPress uses `PRIMARY KEY (col)` at the table level, not inline.
        // Without this, col.primary_key stays false and SERIAL auto-fill never fires.
        for constraint in &constraints {
            if let TableConstraint::PrimaryKey { columns: pk_cols, .. } = constraint {
                for pk_col_name in pk_cols {
                    if let Some(col_def) = column_defs
                        .iter_mut()
                        .find(|c| c.name.eq_ignore_ascii_case(pk_col_name))
                    {
                        col_def.primary_key = true;
                        // Don't override not_null if already set to false by SERIAL detection
                        // (SERIAL columns must stay nullable for auto-fill: INSERT NULL → row_id)
                    }
                }
            }
        }

        // Propagate table-level UNIQUE constraints to column defs (single-column only).
        // WordPress uses `UNIQUE KEY name (col)` which the translator converts to
        // `UNIQUE(col)`.  Setting col.unique = true lets the catalog schema reflect
        // uniqueness, which SHOW INDEX and ON DUPLICATE KEY UPDATE rely on.
        for constraint in &constraints {
            if let TableConstraint::Unique { columns: uq_cols, .. } = constraint {
                if uq_cols.len() == 1 {
                    if let Some(uq_col_name) = uq_cols.first() {
                        if let Some(col_def) = column_defs
                            .iter_mut()
                            .find(|c| c.name.eq_ignore_ascii_case(uq_col_name))
                        {
                            col_def.unique = true;
                        }
                    }
                }
            }
        }

        Ok(LogicalPlan::CreateTable {
            name,
            columns: column_defs,
            if_not_exists,
            constraints,
        })
    }

    /// Stage-0 partitioning: build the `CreateTable` plan for a
    /// `PARTITION OF` child by cloning the parent's column shape.
    ///
    /// The parent must already exist (the corpus creates parents before
    /// children in seq order); a missing parent surfaces the catalog's own
    /// "does not exist" error class — never a panic. Only the column shape is
    /// copied: name, data type, and NOT NULL. Parent PRIMARY KEY / UNIQUE / FK /
    /// DEFAULT / IDENTITY are intentionally NOT propagated at Stage 0 — the
    /// child is a plain standalone clone, and copying keys would fabricate
    /// constraints the flatten model does not own (real parent/child linkage +
    /// routing is Stage 1).
    fn partition_of_to_plan(
        &self,
        name: String,
        if_not_exists: bool,
        spec: &crate::sql::parser::PartitionOfSpec,
        catalog: &Catalog<'_>,
    ) -> Result<LogicalPlan> {
        let parent = Self::normalize_partition_name(&spec.parent);
        let schema = catalog.get_table_schema(&parent)?;
        let columns: Vec<ColumnDef> = schema
            .columns
            .iter()
            .map(|col| ColumnDef {
                name: col.name.clone(),
                data_type: col.data_type.clone(),
                not_null: !col.nullable,
                primary_key: false,
                unique: false,
                default: None,
                storage_mode: col.storage_mode,
                is_identity: false,
            })
            .collect();
        // Record the partition intent for Stage 1. The bound text is stored
        // verbatim and never interpreted at Stage 0, so no bound evaluation
        // happens here. A durable partition catalog (parent↔child links + typed
        // bounds, persisted through the catalog + WAL) is Stage-1 scope; adding
        // it now would exceed a parse-accept pass, and per-statement planning
        // must stay side-effect-free (no process-global mutable state). We
        // therefore surface the captured (parent, bound) via a structured log
        // and leave the persistence hook as a marker.
        // TODO(stage-1): persist (child `name` → parent, verbatim bound) in a
        // durable partition side-record to drive INSERT routing / parent-scan
        // union / pruning.
        tracing::debug!(
            child = %name,
            parent = %parent,
            bound = %spec.bound,
            "Stage-0 PARTITION OF flattened to a standalone child (bound recorded, not routed)"
        );
        Ok(LogicalPlan::CreateTable {
            name,
            columns,
            if_not_exists,
            constraints: Vec::new(),
        })
    }

    /// Normalize a raw (possibly schema-qualified / double-quoted) partition
    /// table-name string the same way [`Planner::normalize_object_name`]
    /// collapses a parsed `ObjectName`, so the parent lookup and the child-name
    /// key match how tables are stored in the catalog. Reuses the real
    /// normalizer by reconstructing an `ObjectName` from the raw parts.
    ///
    /// `pub(crate)` so the DROP-cascade registration in the executor path can
    /// normalize a `PARTITION OF` parent name IDENTICALLY to how the child was
    /// planned — registration and drop-lookup must agree byte-for-byte.
    pub(crate) fn normalize_partition_name(raw: &str) -> String {
        let idents: Vec<sqlparser::ast::Ident> = Self::split_dotted_ident(raw)
            .into_iter()
            .map(|(value, quoted)| {
                if quoted {
                    sqlparser::ast::Ident::with_quote('"', value)
                } else {
                    sqlparser::ast::Ident::new(value)
                }
            })
            .collect();
        if idents.is_empty() {
            return raw.trim().to_lowercase();
        }
        Self::normalize_object_name(&sqlparser::ast::ObjectName(idents))
    }

    /// Split a raw dotted identifier into `(value, was_quoted)` parts, honoring
    /// double-quoted spans (a `""` inside a quoted part is an embedded quote).
    /// Quotes are stripped from the returned value.
    fn split_dotted_ident(raw: &str) -> Vec<(String, bool)> {
        let mut parts: Vec<(String, bool)> = Vec::new();
        let mut chars = raw.trim().chars().peekable();
        while chars.peek().is_some() {
            // Consume any separator dots between parts.
            while chars.peek() == Some(&'.') {
                chars.next();
            }
            match chars.peek() {
                None => break,
                Some('"') => {
                    chars.next(); // opening quote
                    let mut value = String::new();
                    while let Some(c) = chars.next() {
                        if c == '"' {
                            if chars.peek() == Some(&'"') {
                                value.push('"');
                                chars.next();
                                continue;
                            }
                            break;
                        }
                        value.push(c);
                    }
                    parts.push((value, true));
                }
                Some(_) => {
                    let mut value = String::new();
                    while let Some(&c) = chars.peek() {
                        if c == '.' || c == '"' {
                            break;
                        }
                        value.push(c);
                        chars.next();
                    }
                    let trimmed = value.trim().to_string();
                    if !trimmed.is_empty() {
                        parts.push((trimmed, false));
                    }
                }
            }
        }
        parts
    }

    /// Convert sqlparser TableConstraint to our internal representation
    fn convert_table_constraint(
        &self,
        constraint: &sqlparser::ast::TableConstraint,
        table_name: &str,
    ) -> Option<TableConstraint> {
        use sqlparser::ast::TableConstraint as SqlTC;

        match constraint {
            SqlTC::PrimaryKey { name, columns, .. } => Some(TableConstraint::PrimaryKey {
                name: name.as_ref().map(|n| n.to_string()),
                columns: columns.iter().map(|c| c.to_string()).collect(),
            }),
            SqlTC::Unique { name, columns, .. } => Some(TableConstraint::Unique {
                name: name.as_ref().map(|n| n.to_string()),
                columns: columns.iter().map(|c| c.to_string()).collect(),
            }),
            SqlTC::ForeignKey {
                name,
                columns,
                foreign_table,
                referred_columns,
                on_delete,
                on_update,
                characteristics,
            } => {
                // Convert characteristics to deferrable/initially_deferred
                let (deferrable, initially_deferred) = characteristics
                    .as_ref()
                    .map(|c| {
                        (
                            c.deferrable.unwrap_or(false),
                            c.initially
                                .map(|i| matches!(i, sqlparser::ast::DeferrableInitial::Deferred))
                                .unwrap_or(false),
                        )
                    })
                    .unwrap_or((false, false));
                let enforcement = convert_constraint_enforcement(characteristics.as_ref());

                Some(TableConstraint::ForeignKey {
                    name: name.as_ref().map(|n| n.to_string()),
                    // Normalize identifiers so FK metadata matches the
                    // form tables/columns are stored under in the
                    // catalog. Previously `ObjectName::to_string()`
                    // preserved the original quote characters, so
                    // `REFERENCES "users"(id)` produced a
                    // `references_table = "\"users\""` that later
                    // FK-check lookups could not resolve (B36).
                    columns: columns.iter().map(Self::normalize_ident).collect(),
                    references_table: Self::normalize_object_name(foreign_table),
                    references_columns: referred_columns.iter().map(Self::normalize_ident).collect(),
                    on_delete: on_delete.as_ref().map(|a| convert_referential_action(a)),
                    on_update: on_update.as_ref().map(|a| convert_referential_action(a)),
                    deferrable,
                    initially_deferred,
                    enforcement,
                })
            }
            SqlTC::Check { name, expr } => {
                // Convert sqlparser Expr to our LogicalExpr
                match self.expr_to_logical(expr) {
                    Ok(expr) => Some(TableConstraint::Check {
                        name: name.as_ref().map(|n| n.to_string()),
                        expression: expr,
                    }),
                    Err(_) => None, // Skip constraints we can't parse
                }
            }
            _ => None, // Skip Index, FulltextOrSpatial, etc.
        }
    }

    /// Convert ALTER TABLE statement to logical plan
    fn alter_table_to_plan(
        &self,
        table_name: String,
        operations: Vec<sqlparser::ast::AlterTableOperation>,
    ) -> Result<LogicalPlan> {
        // Check if operations are empty
        if operations.is_empty() {
            return Err(Error::query_execution("ALTER TABLE requires an operation"));
        }

        // Single operation: return directly (no wrapper needed)
        if operations.len() == 1 {
            let operation = operations
                .into_iter()
                .next()
                .ok_or_else(|| Error::query_execution("ALTER TABLE requires an operation"))?;
            return self.alter_table_single_op_to_plan(table_name, operation);
        }

        // Multiple operations: plan each individually, wrap in AlterTableMulti
        let mut plans = Vec::with_capacity(operations.len());
        for operation in operations {
            plans.push(self.alter_table_single_op_to_plan(table_name.clone(), operation)?);
        }
        Ok(LogicalPlan::AlterTableMulti { operations: plans })
    }

    /// Convert a single ALTER TABLE operation to a logical plan node
    fn alter_table_single_op_to_plan(
        &self,
        table_name: String,
        operation: sqlparser::ast::AlterTableOperation,
    ) -> Result<LogicalPlan> {
        use sqlparser::ast::AlterTableOperation;

        match operation {
            AlterTableOperation::AddColumn {
                column_def,
                if_not_exists,
                ..
            } => {
                let col_def = self.sql_column_def_to_column_def(&column_def)?;
                Ok(LogicalPlan::AlterTableAddColumn {
                    table_name,
                    column_def: col_def,
                    if_not_exists,
                })
            }
            AlterTableOperation::DropColumn {
                column_name,
                if_exists,
                cascade,
            } => Ok(LogicalPlan::AlterTableDropColumn {
                table_name,
                column_name: column_name.value,
                if_exists,
                cascade,
            }),
            AlterTableOperation::RenameColumn {
                old_column_name,
                new_column_name,
            } => Ok(LogicalPlan::AlterTableRenameColumn {
                table_name,
                old_column_name: old_column_name.value,
                new_column_name: new_column_name.value,
            }),
            AlterTableOperation::RenameTable { table_name: new_name } => Ok(LogicalPlan::AlterTableRename {
                table_name,
                new_table_name: new_name.to_string(),
            }),
            AlterTableOperation::AlterColumn {
                column_name,
                op: sqlparser::ast::AlterColumnOperation::DropNotNull,
            } => Ok(LogicalPlan::AlterTableAlterColumnNullability {
                table_name,
                column_name: column_name.value,
                nullable: true,
            }),
            // ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY (KanttBan #5
            // against v3.27.0). drizzle-kit / Prisma / Flyway / Liquibase
            // all emit FKs as a separate ALTER TABLE step at the end of
            // their migrations so they can topologically order tables
            // without worrying about cycle resolution. Wires the
            // sqlparser ForeignKey shape through to the same
            // `catalog.add_foreign_key` API the inline-`REFERENCES`
            // path in CREATE TABLE already uses.
            AlterTableOperation::AddConstraint(sqlparser::ast::TableConstraint::ForeignKey {
                name,
                columns,
                foreign_table,
                referred_columns,
                on_delete,
                on_update,
                characteristics,
            }) => {
                let (deferrable, initially_deferred) = characteristics
                    .as_ref()
                    .map(|c| {
                        (
                            c.deferrable.unwrap_or(false),
                            c.initially
                                .map(|i| matches!(i, sqlparser::ast::DeferrableInitial::Deferred))
                                .unwrap_or(false),
                        )
                    })
                    .unwrap_or((false, false));
                let enforcement = convert_constraint_enforcement(characteristics.as_ref());
                Ok(LogicalPlan::AlterTableAddForeignKey {
                    table_name,
                    constraint_name: name.as_ref().map(|n| n.to_string()),
                    columns: columns.iter().map(Self::normalize_ident).collect(),
                    references_table: Self::normalize_object_name(&foreign_table),
                    references_columns: referred_columns.iter().map(Self::normalize_ident).collect(),
                    on_delete: on_delete.as_ref().map(convert_referential_action),
                    on_update: on_update.as_ref().map(convert_referential_action),
                    deferrable,
                    initially_deferred,
                    enforcement,
                })
            }
            // ALTER TABLE … DROP CONSTRAINT [IF EXISTS] name [CASCADE].
            // Migration tools (a2h's chunked loader) drop FKs before a
            // range-delete + reload pass and re-add them after. CASCADE is
            // accepted but not propagated (Nano drops the named constraint
            // only; dependent objects are handled by their own DDL).
            AlterTableOperation::DropConstraint { name, if_exists, .. } => {
                Ok(LogicalPlan::AlterTableDropConstraint {
                    table_name,
                    constraint_name: name.value,
                    if_exists,
                })
            }
            _ => Err(Error::query_execution(format!(
                "Unsupported ALTER TABLE operation: {operation:?}",
            ))),
        }
    }

    /// Convert SQL column definition to internal ColumnDef
    fn sql_column_def_to_column_def(&self, col: &SqlColumnDef) -> Result<ColumnDef> {
        let data_type = self.sql_data_type_to_data_type(&col.data_type)?;

        // Detect SERIAL/BIGSERIAL/SMALLSERIAL types (parsed as Custom)
        let is_serial = matches!(&col.data_type, SqlDataType::Custom(name, _)
        if {
            let n = name.to_string().to_uppercase();
            n == "SERIAL" || n == "BIGSERIAL" || n == "SMALLSERIAL"
        });

        let mut not_null = false;
        let mut primary_key = false;
        let mut unique = false;
        let mut default = None;
        // `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY` — SQL-standard
        // equivalent of SERIAL. Treat it the same: the column is
        // auto-generated if the user omits the value at INSERT time.
        let mut is_identity = false;

        for option in &col.options {
            match &option.option {
                ColumnOption::NotNull => not_null = true,
                ColumnOption::Unique { is_primary, .. } => {
                    if *is_primary {
                        primary_key = true;
                        not_null = true;
                    } else {
                        unique = true;
                    }
                }
                ColumnOption::Default(expr) => {
                    default = Some(self.expr_to_logical(expr)?);
                }
                ColumnOption::Generated {
                    generated_as: sqlparser::ast::GeneratedAs::Always | sqlparser::ast::GeneratedAs::ByDefault,
                    ..
                } => {
                    is_identity = true;
                }
                _ => {}
            }
        }

        // SERIAL / IDENTITY columns auto-generate values: make them
        // nullable internally so omitted columns get NULL, then the
        // INSERT path fills via `next_row_id`.
        if (is_serial || is_identity) && default.is_none() {
            not_null = false;
        }

        Ok(ColumnDef {
            name: Self::normalize_ident(&col.name),
            data_type,
            not_null,
            primary_key,
            unique,
            default,
            storage_mode: crate::ColumnStorageMode::Default, // Set via ALTER TABLE ALTER COLUMN SET STORAGE
            is_identity: is_identity || is_serial,
        })
    }

    /// Convert SQL data type to internal DataType
    fn sql_data_type_to_data_type(&self, data_type: &SqlDataType) -> Result<DataType> {
        match data_type {
            SqlDataType::Boolean => Ok(DataType::Boolean),
            SqlDataType::SmallInt(_) | SqlDataType::Int2(_) => Ok(DataType::Int2),
            SqlDataType::Int(_) | SqlDataType::Integer(_) | SqlDataType::Int4(_) => Ok(DataType::Int4),
            SqlDataType::BigInt(_) | SqlDataType::Int8(_) => Ok(DataType::Int8),
            SqlDataType::Real => Ok(DataType::Float4), // REAL is Float4 in PostgreSQL
            SqlDataType::Float(_) => Ok(DataType::Float8), // FLOAT(n) defaults to Float8
            SqlDataType::Float4 => Ok(DataType::Float4),
            SqlDataType::Float8 | SqlDataType::DoublePrecision => Ok(DataType::Float8),
            SqlDataType::Varchar(char_len) => {
                // CharacterLength can be complex, extract simple usize if possible
                let len = char_len.as_ref().and_then(|cl| {
                    if let sqlparser::ast::CharacterLength::IntegerLength { length, .. } = cl {
                        Some(*length as usize)
                    } else {
                        None
                    }
                });
                Ok(DataType::Varchar(len))
            }
            // Fixed-length CHAR / CHARACTER. Per the SQL standard an
            // unspecified length means CHAR(1). Needed for e.g. Pagila's
            // `language.name CHARACTER(20)` — previously rejected at CREATE
            // TABLE with "Data type not yet supported: Char(..)".
            SqlDataType::Char(char_len) | SqlDataType::Character(char_len) => {
                let len = char_len
                    .as_ref()
                    .and_then(|cl| match cl {
                        sqlparser::ast::CharacterLength::IntegerLength { length, .. } => {
                            Some(*length as usize)
                        }
                        _ => None,
                    })
                    .unwrap_or(1);
                Ok(DataType::Char(len))
            }
            // Variable-length national/var character types behave as VARCHAR(n).
            SqlDataType::CharacterVarying(char_len)
            | SqlDataType::CharVarying(char_len)
            | SqlDataType::Nvarchar(char_len) => {
                let len = char_len.as_ref().and_then(|cl| {
                    if let sqlparser::ast::CharacterLength::IntegerLength { length, .. } = cl {
                        Some(*length as usize)
                    } else {
                        None
                    }
                });
                Ok(DataType::Varchar(len))
            }
            // Character large objects map to unbounded TEXT.
            SqlDataType::Clob(_)
            | SqlDataType::CharacterLargeObject(_)
            | SqlDataType::CharLargeObject(_) => Ok(DataType::Text),
            SqlDataType::Text => Ok(DataType::Text),
            SqlDataType::Bytea => Ok(DataType::Bytea),
            SqlDataType::Date => Ok(DataType::Date),
            SqlDataType::Time(_, _) => Ok(DataType::Time),
            SqlDataType::Timestamp(_, _) => Ok(DataType::Timestamp),
            SqlDataType::Interval => Ok(DataType::Interval),
            SqlDataType::Uuid => Ok(DataType::Uuid),
            SqlDataType::JSON => Ok(DataType::Json),
            SqlDataType::JSONB => Ok(DataType::Jsonb),
            SqlDataType::Array(array_def) => {
                let inner = match array_def {
                    sqlparser::ast::ArrayElemTypeDef::SquareBracket(inner, _)
                    | sqlparser::ast::ArrayElemTypeDef::AngleBracket(inner)
                    | sqlparser::ast::ArrayElemTypeDef::Parenthesis(inner) => self.sql_data_type_to_data_type(inner)?,
                    sqlparser::ast::ArrayElemTypeDef::None => DataType::Text,
                };
                Ok(DataType::Array(Box::new(inner)))
            }
            // Handle NUMERIC/DECIMAL types (PostgreSQL arbitrary precision decimal)
            SqlDataType::Numeric(_) | SqlDataType::Decimal(_) => Ok(DataType::Numeric),
            // Bit / bit-varying. sqlparser exposes these as dedicated
            // variants (not Custom). Stored as TEXT carrying the
            // canonical '0'/'1' string form -- same simplification
            // pattern used throughout this function (e.g. Clob -> Text).
            SqlDataType::Bit(_) | SqlDataType::BitVarying(_) => Ok(DataType::Text),
            // Handle PostgreSQL SERIAL types
            SqlDataType::Custom(object_name, type_modifiers) => {
                let type_name = object_name.to_string().to_uppercase();
                match type_name.as_str() {
                    "SERIAL" => Ok(DataType::Int4),      // SERIAL is Int4 with auto-increment
                    "BIGSERIAL" => Ok(DataType::Int8),   // BIGSERIAL is Int8 with auto-increment
                    "SMALLSERIAL" => Ok(DataType::Int2), // SMALLSERIAL is Int2 with auto-increment
                    // PostgreSQL fixed-length internal identifier type.
                    // sqlparser has no dedicated `NAME` keyword dispatch
                    // (confirmed: no `Keyword::NAME`-to-DataType arm
                    // anywhere in its parser), so it falls in here rather
                    // than as a top-level DataType variant. Real Postgres
                    // stores this as a 64-byte blank-padded string used for
                    // catalog columns (pg_class.relname etc) and is
                    // exercised directly by fixture tables like onek/tenk1
                    // (`stringu1/stringu2/string4 name`). TEXT is the same
                    // simplification already applied to CLOB elsewhere in
                    // this function: no 63-byte truncation enforcement in
                    // this first pass.
                    "NAME" => Ok(DataType::Text),
                    // Geometric POINT type (and, by the same Stage-1
                    // simplification, PATH used by fixtures like `road`).
                    // Stage 1 (this fix): round-trip through Nano's
                    // canonical `(x,y)` textual form as TEXT so CREATE
                    // TABLE POINT_TBL/person/road succeed and unblock
                    // the large set of downstream fixture-dependent
                    // files. Stage 2 (fast-follow, not in this pass):
                    // a real DataType::Point with (f64,f64) storage and
                    // operator support (`<->` etc), mirroring how
                    // DataType::Vector(usize) was added.
                    "POINT" | "PATH" => Ok(DataType::Text),
                    // `VARBIT` — the SQL-standard alias for BIT VARYING.
                    // sqlparser 0.53 dispatches `BIT VARYING` to the dedicated
                    // `SqlDataType::BitVarying` variant (mapped to Text above),
                    // but the single-word `VARBIT` spelling has no keyword and
                    // arrives here as `Custom("VARBIT")`. Map it to the same
                    // Text form so `col VARBIT` / `col VARBIT(n)` match the
                    // BitVarying arm — the Stage-1 simplification of c488ce2.
                    "VARBIT" => Ok(DataType::Text),
                    "VECTOR" | "VECTOR_F16" | "VECTOR_I8" | "VECTOR_I16" | "HALFVEC" => {
                        // Parse dimension from type modifiers: VECTOR(1536).
                        // Multi-precision aliases are accepted as schema-level
                        // compatibility; current row values remain f32 vectors.
                        if type_modifiers.len() == 1 {
                            let modifier = type_modifiers
                                .first()
                                .ok_or_else(|| Error::query_execution("VECTOR type requires dimension"))?;
                            let dimension = modifier
                                .parse::<usize>()
                                .map_err(|e| Error::query_execution(format!("Invalid vector dimension: {}", e)))?;
                            return Ok(DataType::Vector(dimension));
                        }
                        Err(Error::query_execution("VECTOR type requires dimension: VECTOR(n)"))
                    }
                    // PostgreSQL FTS column types. We store tsvector /
                    // tsquery values as JSON arrays of normalised tokens
                    // (see `Evaluator::fts_*`), so treat the declared
                    // type as JSON. Full Postgres fidelity (positions,
                    // weights, phrase queries) is intentionally out of
                    // scope — see docs/compatibility/fts.md.
                    "TSVECTOR" | "TSQUERY" => Ok(DataType::Json),
                    // KanttBan #23 (v3.31.1 phase 1): regtype / regrole /
                    // regnamespace etc. — OID-alias types. drizzle-kit's
                    // getColumnsInfoQuery uses `'int'::regtype` to
                    // resolve type-name → OID. Treat as TEXT for now;
                    // a fuller implementation would do the name→OID
                    // lookup at cast time so `int = ANY('{int}'::regtype[])`
                    // works against the int OID. The current behaviour
                    // makes the CASE branch in drizzle's query fall
                    // through to format_type, which is what we want.
                    "REGTYPE" | "REGCLASS" | "REGROLE" | "REGNAMESPACE" | "REGOPER" | "REGOPERATOR" | "REGPROC"
                    | "REGPROCEDURE" | "REGCONFIG" | "REGDICTIONARY" => Ok(DataType::Text),
                    _ => {
                        // KanttBan #20 (v3.31.0): unknown custom type
                        // might be a user-defined enum. Check the
                        // catalog; on hit, store as TEXT and rely on
                        // the CHECK constraint appended by
                        // `create_table_to_plan` to enforce labels.
                        // `object_name.to_string()` here preserves
                        // dotted-prefix form (e.g. `public.foo`); the
                        // catalog lookup normalises before keying.
                        if let Some(catalog) = self.catalog {
                            let raw_name = object_name.to_string();
                            let lookup_name = Self::dealias_schema(&raw_name).unwrap_or(raw_name);
                            if catalog.enum_type_exists(&lookup_name)? {
                                return Ok(DataType::Text);
                            }
                        }
                        Err(Error::query_execution(format!(
                            "Custom data type not yet supported: {}",
                            type_name
                        )))
                    }
                }
            }
            // KanttBan #23 (v3.31.1 phase 1): sqlparser exposes
            // these PG OID-alias types as first-class variants (not
            // Custom). drizzle-kit's getColumnsInfoQuery uses
            // `a.attrelid::regclass::text AS table_name`. Treat as
            // TEXT — the cast is essentially a no-op for our purposes
            // since downstream code reads `cls.relname` for the real
            // name, and the `attrelid::regclass::text` output is just
            // shown in the result (where any string suffices for
            // drizzle's diff path).
            SqlDataType::Regclass => Ok(DataType::Text),
            _ => Err(Error::query_execution(format!(
                "Data type not yet supported: {:?}",
                data_type
            ))),
        }
    }

    /// Convert expression to usize (for LIMIT/OFFSET).
    ///
    /// Accepts placeholders (`$1`, `?`) so that schema derivation during
    /// psycopg's extended-query Parse step can succeed even when the actual
    /// LIMIT/OFFSET values are only known at Bind/Execute time. The real
    /// values get substituted by `substitute_parameters()` before the
    /// Execute-time planner runs, so `usize::MAX` here is a safe
    /// schema-preserving placeholder.
    fn expr_to_usize(&self, expr: &Expr) -> Result<usize> {
        Ok(self.expr_to_limit_bound(expr)?.0)
    }

    /// Resolve a LIMIT / OFFSET expression to `(literal_value,
    /// parameter_index)`.
    ///
    /// - `Number` → `(n, None)`.
    /// - `Placeholder("$N")` → `(usize::MAX, Some(N))`; the real value
    ///   is bound at execute time by the executor using its parameter
    ///   list. `usize::MAX` is a schema-preserving sentinel so Parse /
    ///   Describe still see a sensible plan and the optimiser doesn't
    ///   accidentally short-circuit.
    /// - `SingleQuotedString(n)` where `n` parses as `usize` →
    ///   `(n, None)`. This is what substituted wire-level queries look
    ///   like when the bind parameter is typed TEXT (OID 25) or UNKNOWN
    ///   (OID 0): `substitute_parameters` renders string values with
    ///   surrounding single quotes. Matches stock PG's implicit
    ///   `text → integer` cast for LIMIT / OFFSET (B33).
    fn expr_to_limit_bound(&self, expr: &Expr) -> Result<(usize, Option<usize>)> {
        match expr {
            Expr::Value(sqlparser::ast::Value::Number(n, _)) => {
                let v = n
                    .parse::<usize>()
                    .map_err(|e| Error::query_execution(format!("Invalid number: {}", e)))?;
                Ok((v, None))
            }
            Expr::Value(sqlparser::ast::Value::Placeholder(placeholder)) => {
                if let Some(idx_str) = placeholder.strip_prefix('$') {
                    let idx = idx_str.parse::<usize>().map_err(|_| {
                        Error::query_execution(format!(
                            "Invalid parameter placeholder: {placeholder}. Expected $1, $2, ..."
                        ))
                    })?;
                    if idx == 0 {
                        return Err(Error::query_execution(
                            "Parameter indices must be 1-based (e.g. $1, $2)",
                        ));
                    }
                    Ok((usize::MAX, Some(idx)))
                } else {
                    Ok((usize::MAX, None))
                }
            }
            Expr::Value(sqlparser::ast::Value::SingleQuotedString(s)) => {
                let v = s.parse::<usize>().map_err(|_| {
                    Error::query_execution(format!("LIMIT/OFFSET must be a number (got quoted value '{s}')"))
                })?;
                Ok((v, None))
            }
            _ => Err(Error::query_execution("LIMIT/OFFSET must be a number")),
        }
    }

    /// Parse CREATE INDEX WITH options
    ///
    /// Supports options like:
    /// - quantization = 'product'
    /// - pq_subquantizers = 8
    /// - pq_centroids = 256
    /// - m = 16 (HNSW M parameter)
    /// - ef_construction = 200
    /// - metric = 'l2' | 'cosine' | 'inner_product'
    /// - sharding_strategy = 'hash'
    /// - shard_count = 16
    fn parse_index_options(&self, with_exprs: &[Expr]) -> Result<Vec<super::logical_plan::IndexOption>> {
        use super::logical_plan::{IndexOption, QuantizationType};

        let mut options = Vec::new();

        for expr in with_exprs {
            // Each expression should be a binary operation like "m = 16"
            let (name, value) = match expr {
                Expr::BinaryOp {
                    left,
                    op: SqlBinaryOp::Eq,
                    right,
                } => {
                    let name = match left.as_ref() {
                        Expr::Identifier(ident) => ident.value.to_lowercase(),
                        _ => {
                            return Err(Error::query_execution(
                                "Expected identifier on left side of WITH option",
                            ))
                        }
                    };
                    (name, right.as_ref())
                }
                _ => return Err(Error::query_execution("Expected key=value format in WITH clause")),
            };

            match name.as_str() {
                "quantization" => {
                    if let Expr::Value(sqlparser::ast::Value::SingleQuotedString(s)) = value {
                        let quant_type = match s.to_lowercase().as_str() {
                            "none" => QuantizationType::None,
                            "scalar" => QuantizationType::Scalar,
                            "product" => QuantizationType::Product,
                            "auto" => QuantizationType::Auto,
                            _ => {
                                return Err(Error::query_execution(format!(
                                    "Invalid quantization type: {}. Expected 'none', 'scalar', 'product', or 'auto'",
                                    s
                                )))
                            }
                        };
                        options.push(IndexOption::Quantization(quant_type));
                    } else {
                        return Err(Error::query_execution("quantization must be a string"));
                    }
                }
                "pq_subquantizers" => {
                    if let Expr::Value(sqlparser::ast::Value::Number(n, _)) = value {
                        let count = n
                            .parse::<usize>()
                            .map_err(|_| Error::query_execution("Invalid pq_subquantizers value"))?;
                        options.push(IndexOption::PqSubquantizers(count));
                    } else {
                        return Err(Error::query_execution("pq_subquantizers must be a number"));
                    }
                }
                "pq_centroids" => {
                    if let Expr::Value(sqlparser::ast::Value::Number(n, _)) = value {
                        let count = n
                            .parse::<usize>()
                            .map_err(|_| Error::query_execution("Invalid pq_centroids value"))?;
                        options.push(IndexOption::PqCentroids(count));
                    } else {
                        return Err(Error::query_execution("pq_centroids must be a number"));
                    }
                }
                "m" => {
                    if let Expr::Value(sqlparser::ast::Value::Number(n, _)) = value {
                        let m_param = n
                            .parse::<usize>()
                            .map_err(|_| Error::query_execution("Invalid m value"))?;
                        options.push(IndexOption::HnswM(m_param));
                    } else {
                        return Err(Error::query_execution("m must be a number"));
                    }
                }
                "ef_construction" => {
                    if let Expr::Value(sqlparser::ast::Value::Number(n, _)) = value {
                        let ef = n
                            .parse::<usize>()
                            .map_err(|_| Error::query_execution("Invalid ef_construction value"))?;
                        options.push(IndexOption::EfConstruction(ef));
                    } else {
                        return Err(Error::query_execution("ef_construction must be a number"));
                    }
                }
                "metric" | "distance_metric" => {
                    if let Expr::Value(sqlparser::ast::Value::SingleQuotedString(s)) = value {
                        let metric = s.to_ascii_lowercase();
                        if !matches!(metric.as_str(), "l2" | "euclidean" | "cosine" | "ip" | "inner_product") {
                            return Err(Error::query_execution(
                                "metric must be 'l2', 'euclidean', 'cosine', 'ip', or 'inner_product'",
                            ));
                        }
                        options.push(IndexOption::DistanceMetric(metric));
                    } else {
                        return Err(Error::query_execution("metric must be a string"));
                    }
                }
                "sharding_strategy" => {
                    if let Expr::Value(sqlparser::ast::Value::SingleQuotedString(s)) = value {
                        options.push(IndexOption::ShardingStrategy(s.clone()));
                    } else {
                        return Err(Error::query_execution("sharding_strategy must be a string"));
                    }
                }
                "shard_count" => {
                    if let Expr::Value(sqlparser::ast::Value::Number(n, _)) = value {
                        let count = n
                            .parse::<usize>()
                            .map_err(|_| Error::query_execution("Invalid shard_count value"))?;
                        options.push(IndexOption::ShardCount(count));
                    } else {
                        return Err(Error::query_execution("shard_count must be a number"));
                    }
                }
                "persistent" => {
                    let enabled = match value {
                        Expr::Value(sqlparser::ast::Value::Boolean(b)) => *b,
                        Expr::Value(sqlparser::ast::Value::SingleQuotedString(s)) => {
                            matches!(s.to_ascii_lowercase().as_str(), "true" | "on" | "1" | "yes")
                        }
                        Expr::Value(sqlparser::ast::Value::Number(n, _)) => n == "1",
                        _ => return Err(Error::query_execution("persistent must be a boolean")),
                    };
                    options.push(IndexOption::Persistent(enabled));
                }
                "rerank_precision" => {
                    if let Expr::Value(sqlparser::ast::Value::SingleQuotedString(s)) = value {
                        let lower = s.to_ascii_lowercase();
                        if !matches!(lower.as_str(), "f32" | "f16" | "i8") {
                            return Err(Error::query_execution("rerank_precision must be 'f32', 'f16', or 'i8'"));
                        }
                        options.push(IndexOption::RerankPrecision(lower));
                    } else {
                        return Err(Error::query_execution("rerank_precision must be a string"));
                    }
                }
                _ => {
                    return Err(Error::query_execution(format!("Unknown CREATE INDEX option: {}", name)));
                }
            }
        }

        Ok(options)
    }

    /// Convert UPDATE statement to logical plan
    fn update_to_plan(
        &self,
        table: sqlparser::ast::TableWithJoins,
        assignments: Vec<sqlparser::ast::Assignment>,
        selection: Option<Expr>,
        returning: Option<Vec<ReturningItem>>,
    ) -> Result<LogicalPlan> {
        // Get table name
        let table_name = match &table.relation {
            sqlparser::ast::TableFactor::Table { name, .. } => Self::normalize_object_name(name),
            _ => {
                return Err(Error::query_execution(
                    "Complex table expressions in UPDATE not supported",
                ))
            }
        };

        // Convert assignments to (column_name, value_expr) pairs
        let assignments: Result<Vec<_>> = assignments
            .iter()
            .map(|assignment| {
                // Extract column name from AssignmentTarget
                let column_name = match &assignment.target {
                    sqlparser::ast::AssignmentTarget::ColumnName(object_name) => object_name
                        .0
                        .iter()
                        .map(|ident| ident.value.clone())
                        .collect::<Vec<_>>()
                        .join("."),
                    _ => return Err(Error::query_execution("Complex assignment targets not supported")),
                };
                let value_expr = self.expr_to_logical(&assignment.value)?;
                Ok((column_name, value_expr))
            })
            .collect();

        // Convert WHERE clause if present
        let selection = if let Some(expr) = selection {
            Some(self.expr_to_logical(&expr)?)
        } else {
            None
        };

        Ok(LogicalPlan::Update {
            table_name,
            assignments: assignments?,
            selection,
            returning,
        })
    }

    /// Convert RETURNING clause SelectItems to ReturningItems
    fn convert_returning(&self, items: &[sqlparser::ast::SelectItem]) -> Result<Vec<ReturningItem>> {
        items
            .iter()
            .map(|item| {
                match item {
                    sqlparser::ast::SelectItem::Wildcard(_) => Ok(ReturningItem::Wildcard),
                    sqlparser::ast::SelectItem::UnnamedExpr(sqlparser::ast::Expr::Identifier(ident)) => {
                        Ok(ReturningItem::Column(Self::normalize_ident(ident)))
                    }
                    sqlparser::ast::SelectItem::UnnamedExpr(expr) => {
                        // Expression without alias - generate alias from expression text
                        let logical_expr = self.expr_to_logical(expr)?;
                        let alias = format!("{expr}");
                        Ok(ReturningItem::Expression {
                            expr: logical_expr,
                            alias,
                        })
                    }
                    sqlparser::ast::SelectItem::ExprWithAlias { expr, alias } => {
                        let logical_expr = self.expr_to_logical(expr)?;
                        Ok(ReturningItem::Expression {
                            expr: logical_expr,
                            alias: alias.value.clone(),
                        })
                    }
                    sqlparser::ast::SelectItem::QualifiedWildcard(name, _) => {
                        // table.* - treat as wildcard (single-table DML context)
                        let _ = name;
                        Ok(ReturningItem::Wildcard)
                    }
                }
            })
            .collect()
    }

    /// Convert ON CONFLICT clause from sqlparser AST to our internal representation
    /// Extract the column name from an `ON CONFLICT … DO UPDATE SET <target>`
    /// (or MySQL `ON DUPLICATE KEY UPDATE`) assignment target. Uses each
    /// ident's unquoted `value` exactly like the regular UPDATE path —
    /// `target.to_string()` would re-emit the quote characters (`"col"`),
    /// which then fail the executor's schema lookup (the EXCLUDED quoted-
    /// identifier upsert bug).
    fn assignment_target_name(target: &sqlparser::ast::AssignmentTarget) -> Result<String> {
        match target {
            sqlparser::ast::AssignmentTarget::ColumnName(object_name) => Ok(object_name
                .0
                .iter()
                .map(|ident| ident.value.clone())
                .collect::<Vec<_>>()
                .join(".")),
            _ => Err(Error::query_execution(
                "Complex assignment targets not supported in ON CONFLICT DO UPDATE",
            )),
        }
    }

    fn convert_on_conflict(&self, on_insert: &Option<sqlparser::ast::OnInsert>) -> Result<Option<OnConflictAction>> {
        let on = match on_insert {
            Some(on) => on,
            None => return Ok(None),
        };
        match on {
            sqlparser::ast::OnInsert::OnConflict(conflict) => match &conflict.action {
                sqlparser::ast::OnConflictAction::DoNothing => Ok(Some(OnConflictAction::DoNothing)),
                sqlparser::ast::OnConflictAction::DoUpdate(do_update) => {
                    let assignments = do_update
                        .assignments
                        .iter()
                        .map(|a| {
                            let col_name = Self::assignment_target_name(&a.target)?;
                            let expr = self.expr_to_logical(&a.value)?;
                            Ok((col_name, expr))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    let selection = do_update
                        .selection
                        .as_ref()
                        .map(|expr| self.expr_to_logical(expr))
                        .transpose()?;
                    Ok(Some(OnConflictAction::DoUpdate { assignments, selection }))
                }
            },
            sqlparser::ast::OnInsert::DuplicateKeyUpdate(assignments) => {
                // MySQL ON DUPLICATE KEY UPDATE — convert to DoUpdate
                let assign_pairs = assignments
                    .iter()
                    .map(|a| {
                        let col_name = Self::assignment_target_name(&a.target)?;
                        let expr = self.expr_to_logical(&a.value)?;
                        Ok((col_name, expr))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(Some(OnConflictAction::DoUpdate {
                    assignments: assign_pairs,
                    selection: None,
                }))
            }
            _ => {
                // Future OnInsert variants — unsupported for now
                Err(Error::query_execution("Unsupported ON INSERT clause"))
            }
        }
    }

    /// Convert DELETE statement to logical plan
    fn delete_to_plan(
        &self,
        table: sqlparser::ast::TableWithJoins,
        selection: Option<Expr>,
        returning: Option<Vec<ReturningItem>>,
    ) -> Result<LogicalPlan> {
        // Get table name
        let table_name = match &table.relation {
            sqlparser::ast::TableFactor::Table { name, .. } => Self::normalize_object_name(name),
            _ => {
                return Err(Error::query_execution(
                    "Complex table expressions in DELETE not supported",
                ))
            }
        };

        // Convert WHERE clause if present
        let selection = if let Some(expr) = selection {
            Some(self.expr_to_logical(&expr)?)
        } else {
            None
        };

        Ok(LogicalPlan::Delete {
            table_name,
            selection,
            returning,
        })
    }

    /// Convert CREATE TRIGGER statement to logical plan
    #[allow(clippy::too_many_arguments)]
    fn create_trigger_to_plan(
        &self,
        or_replace: bool,
        is_constraint: bool,
        name: ObjectName,
        period: TriggerPeriod,
        events: Vec<SqlTriggerEvent>,
        table_name: ObjectName,
        referenced_table_name: Option<ObjectName>,
        referencing: Vec<TriggerReferencing>,
        trigger_object: TriggerObject,
        include_each: bool,
        condition: Option<Expr>,
        exec_body: TriggerExecBody,
        characteristics: Option<ConstraintCharacteristics>,
    ) -> Result<LogicalPlan> {
        // Convert trigger name
        let trigger_name = name.to_string();

        // Convert table name
        let table_name_str = table_name.to_string();

        // Convert timing
        let timing = match period {
            TriggerPeriod::Before => TriggerTiming::Before,
            TriggerPeriod::After => TriggerTiming::After,
            TriggerPeriod::InsteadOf => TriggerTiming::InsteadOf,
        };

        // Convert events
        let trigger_events: Result<Vec<TriggerEvent>> = events
            .iter()
            .map(|event| match event {
                SqlTriggerEvent::Insert => Ok(TriggerEvent::Insert),
                SqlTriggerEvent::Update(cols) => {
                    let column_names = if cols.is_empty() {
                        None
                    } else {
                        Some(cols.iter().map(|c| c.value.clone()).collect())
                    };
                    Ok(TriggerEvent::Update(column_names))
                }
                SqlTriggerEvent::Delete => Ok(TriggerEvent::Delete),
                SqlTriggerEvent::Truncate => Ok(TriggerEvent::Truncate),
            })
            .collect();

        // Convert FOR EACH clause
        let for_each = match trigger_object {
            TriggerObject::Row => TriggerFor::Row,
            TriggerObject::Statement => TriggerFor::Statement,
        };

        // PostgreSQL compatibility: TRUNCATE triggers must be FOR EACH STATEMENT
        let trigger_events_ref = trigger_events.as_ref();
        if let Ok(events) = trigger_events_ref {
            if events.iter().any(|e| matches!(e, TriggerEvent::Truncate)) && for_each == TriggerFor::Row {
                return Err(Error::query_execution(
                    "TRUNCATE triggers do not support FOR EACH ROW - use FOR EACH STATEMENT",
                ));
            }
        }

        // Convert WHEN condition
        let when_condition = if let Some(cond_expr) = condition {
            Some(Box::new(self.expr_to_logical(&cond_expr)?))
        } else {
            None
        };

        // Determine trigger type and constraint reference
        let trigger_type = if is_constraint {
            TriggerType::Constraint
        } else {
            TriggerType::Regular
        };

        // FROM clause reference (for CONSTRAINT triggers)
        let from_constraint = referenced_table_name.map(|n| n.to_string());

        // Parse REFERENCING clause for transition tables
        let transition_tables: Vec<TransitionTable> = referencing
            .iter()
            .map(|r| {
                // TriggerReferencing has refer_type (OLD/NEW), is_table flag, and transition_relation_name
                // transition_relation_name is an ObjectName - use it directly or provide default
                let alias = {
                    let name_str = r.transition_relation_name.to_string();
                    if name_str.is_empty() {
                        if r.refer_type == sqlparser::ast::TriggerReferencingType::OldTable {
                            "OLD".to_string()
                        } else {
                            "NEW".to_string()
                        }
                    } else {
                        name_str
                    }
                };

                match r.refer_type {
                    sqlparser::ast::TriggerReferencingType::OldTable => TransitionTable::OldTable { alias },
                    sqlparser::ast::TriggerReferencingType::NewTable => TransitionTable::NewTable { alias },
                }
            })
            .collect();

        // Validate: REFERENCING only allowed for FOR EACH STATEMENT triggers
        if !transition_tables.is_empty() && for_each == TriggerFor::Row {
            return Err(Error::query_execution(
                "REFERENCING clause only valid for FOR EACH STATEMENT triggers",
            ));
        }

        // Validate OLD TABLE only for UPDATE/DELETE, NEW TABLE only for INSERT/UPDATE
        let trigger_events_val = trigger_events.as_ref();
        if let Ok(events) = trigger_events_val {
            for tt in &transition_tables {
                match tt {
                    TransitionTable::OldTable { .. } => {
                        if !events
                            .iter()
                            .any(|e| matches!(e, TriggerEvent::Update(_) | TriggerEvent::Delete))
                        {
                            return Err(Error::query_execution(
                                "OLD TABLE in REFERENCING clause requires UPDATE or DELETE trigger event",
                            ));
                        }
                    }
                    TransitionTable::NewTable { .. } => {
                        if !events
                            .iter()
                            .any(|e| matches!(e, TriggerEvent::Insert | TriggerEvent::Update(_)))
                        {
                            return Err(Error::query_execution(
                                "NEW TABLE in REFERENCING clause requires INSERT or UPDATE trigger event",
                            ));
                        }
                    }
                }
            }
        }

        // Parse DEFERRABLE characteristics
        let trigger_characteristics = characteristics
            .as_ref()
            .map(|c| {
                let deferrable = c.deferrable.unwrap_or(false);
                let initially_deferred = c
                    .initially
                    .map(|i| matches!(i, sqlparser::ast::DeferrableInitial::Deferred))
                    .unwrap_or(false);
                TriggerCharacteristics {
                    deferrable,
                    initially_deferred,
                }
            })
            .unwrap_or_default();

        // Parse trigger body
        // For now, we'll store the exec_body as a placeholder
        // In a real implementation, we would parse the function/procedure body
        // or store a reference to the function name
        let body = vec![]; // Empty body for now - will be populated in Phase 2

        // Note: sqlparser's exec_body contains a FunctionDesc which has the function name
        // and arguments. We would typically store this as a reference to a function
        // that will be called when the trigger fires. For now, we're just parsing
        // the structure, not implementing execution.

        // Capture the invoked function name (`EXECUTE FUNCTION f()`) so the
        // executor can resolve its body for BEFORE-row NEW mutation.
        let function_name = Some(Self::normalize_object_name(&exec_body.func_desc.name));

        Ok(LogicalPlan::CreateTrigger {
            name: trigger_name,
            table_name: table_name_str,
            timing,
            events: trigger_events?,
            for_each,
            when_condition,
            body,
            if_not_exists: or_replace, // OR REPLACE treated similarly to IF NOT EXISTS
            referencing: transition_tables,
            characteristics: trigger_characteristics,
            trigger_type,
            from_constraint,
            function_name,
        })
    }

    /// Convert DROP TRIGGER statement to logical plan
    fn drop_trigger_to_plan(
        &self,
        if_exists: bool,
        trigger_name: ObjectName,
        table_name: ObjectName,
        option: Option<SqlReferentialAction>,
    ) -> Result<LogicalPlan> {
        // Handle CASCADE/RESTRICT option
        if let Some(action) = option {
            match action {
                SqlReferentialAction::Cascade => {
                    // CASCADE behavior: drop dependent objects
                    // For now, just log a warning that this is not implemented
                }
                SqlReferentialAction::Restrict => {
                    // RESTRICT behavior: fail if there are dependent objects
                    // This is the default behavior
                }
                _ => {
                    return Err(Error::query_execution(
                        "Only CASCADE and RESTRICT are supported for DROP TRIGGER",
                    ));
                }
            }
        }

        Ok(LogicalPlan::DropTrigger {
            name: trigger_name.to_string(),
            table_name: Some(table_name.to_string()),
            if_exists,
        })
    }

    /// Parse sqlparser EXPLAIN options into unified ExplainOptions
    ///
    /// Supports PostgreSQL-compatible options:
    /// - ANALYZE: Execute query and show actual statistics
    /// - VERBOSE: Show additional detail
    /// - FORMAT { TEXT | JSON | YAML | TREE }: Output format
    /// - COSTS: Show cost estimates (default: true)
    /// - BUFFERS: Show buffer usage
    /// - TIMING: Show timing info (default: true with ANALYZE)
    /// - SUMMARY: Show summary at end
    ///
    /// HeliosDB extensions:
    /// - STORAGE: Show storage layer details
    /// - AI: Enable AI explanations
    /// - WHY_NOT: Enable Why-Not analysis
    /// - INDEXES: Show index analysis
    /// - STATISTICS: Show table/column statistics
    fn parse_explain_options(
        &self,
        analyze: bool,
        verbose: bool,
        format: Option<AnalyzeFormat>,
        utility_options: Option<Vec<UtilityOption>>,
    ) -> Result<ExplainOptions> {
        let mut opts = ExplainOptions {
            analyze,
            verbose,
            costs: true,     // PostgreSQL default
            timing: analyze, // Default on when ANALYZE is used
            ..ExplainOptions::default()
        };

        // Parse format if specified directly
        if let Some(fmt) = format {
            opts.format = match fmt {
                AnalyzeFormat::TEXT => ExplainFormatOption::Text,
                AnalyzeFormat::JSON => ExplainFormatOption::Json,
                AnalyzeFormat::GRAPHVIZ => ExplainFormatOption::Tree,
            };
        }

        // Parse PostgreSQL-style utility options: EXPLAIN (opt1, opt2 value, ...)
        if let Some(options) = utility_options {
            for option in options {
                let name = option.name.value.to_uppercase();
                let value = option
                    .arg
                    .as_ref()
                    .map(|v| v.to_string().to_uppercase())
                    .unwrap_or_else(|| "TRUE".to_string());
                let is_true = value == "TRUE" || value == "ON" || value == "1";
                let is_false = value == "FALSE" || value == "OFF" || value == "0";

                match name.as_str() {
                    // PostgreSQL-compatible options
                    "ANALYZE" => opts.analyze = !is_false,
                    "VERBOSE" => opts.verbose = !is_false,
                    "COSTS" => opts.costs = !is_false,
                    "BUFFERS" => opts.buffers = is_true,
                    "TIMING" => opts.timing = !is_false,
                    "SUMMARY" => opts.summary = is_true,
                    "FORMAT" => {
                        opts.format = ExplainFormatOption::from_str(&value);
                    }

                    // HeliosDB extensions
                    "STORAGE" => opts.storage = !is_false,
                    "AI" => opts.ai = !is_false,
                    "WHY_NOT" | "WHYNOT" => opts.why_not = !is_false,
                    "INDEXES" => opts.indexes = !is_false,
                    "STATISTICS" | "STATS" => opts.statistics = !is_false,

                    // Unknown options are silently ignored for compatibility
                    _ => {}
                }
            }
        }

        // If ANALYZE is set and timing wasn't explicitly disabled, enable it
        if opts.analyze && !opts.timing {
            opts.timing = true;
        }

        Ok(opts)
    }
}

impl Default for Planner<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// KanttBan #23 (v3.31.1 phase 2.3): map a PG regtype text label
/// to the corresponding type OID. Mirrors `PgCatalog::datatype_to_oid`
/// from the opposite direction. Unknown labels yield OID 0 (invalid),
/// which makes the InList comparison harmlessly false against any
/// real atttypid — preferable to erroring at plan time on names we
/// don't recognise yet.
fn regtype_label_to_oid(label: &str) -> i32 {
    match label.to_lowercase().as_str() {
        "bool" | "boolean" => 16,
        "bytea" => 17,
        "int2" | "smallint" => 21,
        "int" | "int4" | "integer" => 23,
        "int8" | "bigint" => 20,
        "text" => 25,
        "json" => 114,
        "float4" | "real" => 700,
        "float8" | "double precision" => 701,
        "bpchar" | "character" => 1042,
        "varchar" | "character varying" => 1043,
        "date" => 1082,
        "time" | "time without time zone" => 1083,
        "timestamp" | "timestamp without time zone" => 1114,
        "timestamptz" | "timestamp with time zone" => 1184,
        "interval" => 1186,
        "numeric" | "decimal" => 1700,
        "uuid" => 2950,
        "jsonb" => 3802,
        _ => 0,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::sql::Parser;

    #[test]
    fn schema_qualified_names_collapse_to_bare_table() {
        use sqlparser::ast::{Ident, ObjectName};
        let on = |parts: &[&str]| ObjectName(parts.iter().map(|p| Ident::new(*p)).collect());
        // Nano is public-only: ANY schema qualifier -> the bare table name, so a
        // table created bare resolves whether later referenced bare or
        // schema-qualified. (Regression: `CREATE TABLE t; DROP TABLE sch.t`
        // previously failed with "table does not exist".)
        assert_eq!(Planner::normalize_object_name(&on(&["public", "t"])), "t");
        assert_eq!(Planner::normalize_object_name(&on(&["pg_catalog", "pg_type"])), "pg_type");
        assert_eq!(Planner::normalize_object_name(&on(&["smoke", "____smoketest"])), "____smoketest");
        assert_eq!(Planner::normalize_object_name(&on(&["myschema", "qualtest"])), "qualtest");
        // bare name unchanged
        assert_eq!(Planner::normalize_object_name(&on(&["____smoketest"])), "____smoketest");
        // a single (quoted) identifier that contains a dot is ONE part -> intact
        assert_eq!(Planner::normalize_object_name(&on(&["my.table"])), "my.table");
        // information_schema.* system views KEEP their qualifier (registered/
        // resolved under the qualified name — must not collapse to bare).
        assert_eq!(
            Planner::normalize_object_name(&on(&["information_schema", "tables"])),
            "information_schema.tables"
        );
        // The string pre-parse path (ALTER SEQUENCE / OWNED BY) collapses ONLY
        // public./pg_catalog. — it must PRESERVE `table.col` (and other custom
        // dotted names) so OWNED BY can split them back into (table, col).
        assert_eq!(Planner::normalize_dotted_name("public.t"), "t");
        assert_eq!(Planner::normalize_dotted_name("mytable.mycol"), "mytable.mycol");
        assert_eq!(Planner::normalize_dotted_name("\"my.table\""), "my.table");
        assert_eq!(
            Planner::normalize_dotted_name("information_schema.columns"),
            "information_schema.columns"
        );
    }

    #[test]
    fn test_select_to_plan() {
        let parser = Parser::new();
        let statement = parser
            .parse_one("SELECT id, name FROM users WHERE id = 1")
            .expect("Failed to parse SQL statement");

        let planner = Planner::new();
        let plan = planner.statement_to_plan(statement);
        assert!(plan.is_ok());
    }

    #[test]
    fn test_insert_to_plan() {
        let parser = Parser::new();
        let statement = parser
            .parse_one("INSERT INTO users (name, email) VALUES ('Alice', 'alice@example.com')")
            .expect("Failed to parse SQL statement");

        let planner = Planner::new();
        let plan = planner.statement_to_plan(statement);
        assert!(plan.is_ok());
    }

    #[test]
    fn test_create_table_to_plan() {
        let parser = Parser::new();
        let statement = parser
            .parse_one("CREATE TABLE users (id SERIAL PRIMARY KEY, name TEXT NOT NULL)")
            .expect("Failed to parse SQL statement");

        let planner = Planner::new();
        let plan = planner.statement_to_plan(statement);
        if let Err(e) = &plan {
            eprintln!("Error in test_create_table_to_plan: {}", e);
        }
        assert!(plan.is_ok(), "Plan failed: {:?}", plan.err());
    }

    #[test]
    fn test_create_trigger_after_insert() {
        let parser = Parser::new();
        let statement = parser
            .parse_one("CREATE TRIGGER audit_insert AFTER INSERT ON users FOR EACH ROW EXECUTE FUNCTION audit_log()")
            .expect("Failed to parse CREATE TRIGGER statement");

        let planner = Planner::new();
        let plan = planner.statement_to_plan(statement);
        assert!(
            plan.is_ok(),
            "Failed to convert CREATE TRIGGER to plan: {:?}",
            plan.err()
        );

        if let Ok(LogicalPlan::CreateTrigger {
            name,
            table_name,
            timing,
            events,
            for_each,
            ..
        }) = plan
        {
            assert_eq!(name, "audit_insert");
            assert_eq!(table_name, "users");
            assert_eq!(timing, TriggerTiming::After);
            assert_eq!(events.len(), 1);
            assert_eq!(events[0], TriggerEvent::Insert);
            assert_eq!(for_each, TriggerFor::Row);
        } else {
            panic!("Expected CreateTrigger plan");
        }
    }

    #[test]
    fn test_create_trigger_before_update() {
        let parser = Parser::new();
        let statement = parser.parse_one(
            "CREATE TRIGGER update_timestamp BEFORE UPDATE ON products FOR EACH ROW EXECUTE FUNCTION update_modified_at()"
        ).expect("Failed to parse CREATE TRIGGER statement");

        let planner = Planner::new();
        let plan = planner.statement_to_plan(statement);
        assert!(
            plan.is_ok(),
            "Failed to convert CREATE TRIGGER to plan: {:?}",
            plan.err()
        );

        if let Ok(LogicalPlan::CreateTrigger {
            name,
            table_name,
            timing,
            events,
            ..
        }) = plan
        {
            assert_eq!(name, "update_timestamp");
            assert_eq!(table_name, "products");
            assert_eq!(timing, TriggerTiming::Before);
            assert_eq!(events.len(), 1);
            match &events[0] {
                TriggerEvent::Update(None) => {}
                _ => panic!("Expected UPDATE event without column list"),
            }
        } else {
            panic!("Expected CreateTrigger plan");
        }
    }

    #[test]
    fn test_create_trigger_update_of_columns() {
        let parser = Parser::new();
        let statement = parser.parse_one(
            "CREATE TRIGGER track_price_change AFTER UPDATE OF price, discount ON products FOR EACH ROW EXECUTE FUNCTION log_price_change()"
        ).expect("Failed to parse CREATE TRIGGER statement");

        let planner = Planner::new();
        let plan = planner.statement_to_plan(statement);
        assert!(
            plan.is_ok(),
            "Failed to convert CREATE TRIGGER to plan: {:?}",
            plan.err()
        );

        if let Ok(LogicalPlan::CreateTrigger { name, events, .. }) = plan {
            assert_eq!(name, "track_price_change");
            assert_eq!(events.len(), 1);
            match &events[0] {
                TriggerEvent::Update(Some(cols)) => {
                    assert_eq!(cols.len(), 2);
                    assert!(cols.contains(&"price".to_string()));
                    assert!(cols.contains(&"discount".to_string()));
                }
                _ => panic!("Expected UPDATE OF with column list"),
            }
        } else {
            panic!("Expected CreateTrigger plan");
        }
    }

    #[test]
    fn test_create_trigger_instead_of() {
        let parser = Parser::new();
        let statement = parser.parse_one(
            "CREATE TRIGGER prevent_delete INSTEAD OF DELETE ON users FOR EACH ROW EXECUTE FUNCTION log_delete_attempt()"
        ).expect("Failed to parse CREATE TRIGGER statement");

        let planner = Planner::new();
        let plan = planner.statement_to_plan(statement);
        assert!(
            plan.is_ok(),
            "Failed to convert CREATE TRIGGER to plan: {:?}",
            plan.err()
        );

        if let Ok(LogicalPlan::CreateTrigger { timing, events, .. }) = plan {
            assert_eq!(timing, TriggerTiming::InsteadOf);
            assert_eq!(events[0], TriggerEvent::Delete);
        } else {
            panic!("Expected CreateTrigger plan");
        }
    }

    #[test]
    fn test_create_trigger_for_each_statement() {
        let parser = Parser::new();
        let statement = parser.parse_one(
            "CREATE TRIGGER bulk_audit AFTER INSERT ON orders FOR EACH STATEMENT EXECUTE FUNCTION audit_bulk_insert()"
        ).expect("Failed to parse CREATE TRIGGER statement");

        let planner = Planner::new();
        let plan = planner.statement_to_plan(statement);
        assert!(
            plan.is_ok(),
            "Failed to convert CREATE TRIGGER to plan: {:?}",
            plan.err()
        );

        if let Ok(LogicalPlan::CreateTrigger { for_each, .. }) = plan {
            assert_eq!(for_each, TriggerFor::Statement);
        } else {
            panic!("Expected CreateTrigger plan");
        }
    }

    #[test]
    fn test_create_trigger_or_replace() {
        let parser = Parser::new();
        let statement = parser.parse_one(
            "CREATE OR REPLACE TRIGGER replace_audit AFTER INSERT ON logs FOR EACH ROW EXECUTE FUNCTION audit_logs()"
        ).expect("Failed to parse CREATE OR REPLACE TRIGGER statement");

        let planner = Planner::new();
        let plan = planner.statement_to_plan(statement);
        assert!(
            plan.is_ok(),
            "Failed to convert CREATE OR REPLACE TRIGGER to plan: {:?}",
            plan.err()
        );

        if let Ok(LogicalPlan::CreateTrigger {
            name, if_not_exists, ..
        }) = plan
        {
            assert_eq!(name, "replace_audit");
            assert!(if_not_exists); // OR REPLACE treated as if_not_exists
        } else {
            panic!("Expected CreateTrigger plan");
        }
    }

    #[test]
    fn test_create_trigger_multiple_events() {
        let parser = Parser::new();
        let statement = parser.parse_one(
            "CREATE TRIGGER multi_event AFTER INSERT OR UPDATE OR DELETE ON items FOR EACH ROW EXECUTE FUNCTION track_changes()"
        ).expect("Failed to parse CREATE TRIGGER statement with multiple events");

        let planner = Planner::new();
        let plan = planner.statement_to_plan(statement);
        assert!(
            plan.is_ok(),
            "Failed to convert CREATE TRIGGER with multiple events to plan: {:?}",
            plan.err()
        );

        if let Ok(LogicalPlan::CreateTrigger { events, .. }) = plan {
            assert_eq!(events.len(), 3);
            assert!(events.contains(&TriggerEvent::Insert));
            assert!(events.iter().any(|e| matches!(e, TriggerEvent::Update(None))));
            assert!(events.contains(&TriggerEvent::Delete));
        } else {
            panic!("Expected CreateTrigger plan");
        }
    }

    #[test]
    fn test_drop_trigger() {
        let parser = Parser::new();
        let statement = parser
            .parse_one("DROP TRIGGER audit_insert ON users")
            .expect("Failed to parse DROP TRIGGER statement");

        let planner = Planner::new();
        let plan = planner.statement_to_plan(statement);
        assert!(plan.is_ok(), "Failed to convert DROP TRIGGER to plan: {:?}", plan.err());

        if let Ok(LogicalPlan::DropTrigger {
            name,
            table_name,
            if_exists,
        }) = plan
        {
            assert_eq!(name, "audit_insert");
            assert_eq!(table_name, Some("users".to_string()));
            assert!(!if_exists);
        } else {
            panic!("Expected DropTrigger plan");
        }
    }

    #[test]
    fn test_drop_trigger_if_exists() {
        let parser = Parser::new();
        let statement = parser
            .parse_one("DROP TRIGGER IF EXISTS old_trigger ON products")
            .expect("Failed to parse DROP TRIGGER IF EXISTS statement");

        let planner = Planner::new();
        let plan = planner.statement_to_plan(statement);
        assert!(
            plan.is_ok(),
            "Failed to convert DROP TRIGGER IF EXISTS to plan: {:?}",
            plan.err()
        );

        if let Ok(LogicalPlan::DropTrigger {
            name,
            table_name,
            if_exists,
        }) = plan
        {
            assert_eq!(name, "old_trigger");
            assert_eq!(table_name, Some("products".to_string()));
            assert!(if_exists);
        } else {
            panic!("Expected DropTrigger plan");
        }
    }

    #[test]
    fn test_drop_trigger_cascade() {
        let parser = Parser::new();
        let statement = parser
            .parse_one("DROP TRIGGER legacy_trigger ON orders CASCADE")
            .expect("Failed to parse DROP TRIGGER CASCADE statement");

        let planner = Planner::new();
        let plan = planner.statement_to_plan(statement);
        assert!(
            plan.is_ok(),
            "Failed to convert DROP TRIGGER CASCADE to plan: {:?}",
            plan.err()
        );

        if let Ok(LogicalPlan::DropTrigger { name, .. }) = plan {
            assert_eq!(name, "legacy_trigger");
        } else {
            panic!("Expected DropTrigger plan");
        }
    }
}
