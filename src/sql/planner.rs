//! Query planner
//!
//! Converts sqlparser AST to logical plans.

#![allow(unused_variables)]

use crate::{Result, Error, Schema, DataType, Value, Column};
use crate::storage::Catalog;
use super::logical_plan::*;
use sqlparser::ast::{
    Statement, Query, Select, SelectItem, Expr, TableFactor, TableWithJoins,
    JoinOperator, JoinConstraint, SetExpr, BinaryOperator as SqlBinaryOp,
    UnaryOperator as SqlUnaryOp, DataType as SqlDataType, ColumnDef as SqlColumnDef,
    ColumnOption, ObjectName, TriggerPeriod, TriggerEvent as SqlTriggerEvent,
    TriggerObject, TriggerReferencing, TriggerExecBody, ConstraintCharacteristics,
    ReferentialAction as SqlReferentialAction, AnalyzeFormat, UtilityOption,
};
use super::explain_options::{ExplainOptions, ExplainFormatOption};

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
use std::sync::Arc;
use std::cell::RefCell;
use std::collections::HashMap;
use super::phase3::materialized_views::MaterializedViewParser;

/// Information about an aggregate plan needed to rewrite ORDER BY expressions.
/// Contains the aggregate expressions and the corresponding output aliases from the
/// wrapping Project layer, plus GROUP BY expressions and their aliases.
struct AggregateInfo {
    /// The aggregate expressions from the Aggregate plan (e.g., SUM(val), COUNT(*))
    aggr_exprs: Vec<LogicalExpr>,
    /// The output aliases for each aggregate (from the Project layer)
    aggr_aliases: Vec<String>,
    /// The GROUP BY expressions from the Aggregate plan
    group_by_exprs: Vec<LogicalExpr>,
    /// The output aliases for each GROUP BY column (from the Project layer)
    group_by_aliases: Vec<String>,
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
    fn populate_named_windows(
        &self,
        named_window_defs: &[sqlparser::ast::NamedWindowDefinition],
    ) -> Result<()> {
        use sqlparser::ast::NamedWindowExpr;
        self.named_windows.borrow_mut().clear();
        for def in named_window_defs {
            let name = def.0.value.clone();
            let spec = match &def.1 {
                NamedWindowExpr::WindowSpec(spec) => {
                    if let Some(ref parent_name) = spec.window_name {
                        let parent_spec =
                            self.get_named_window(&parent_name.value).ok_or_else(|| {
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
            return Err(Error::query_execution(
                "Cannot override ORDER BY of referenced window",
            ));
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
            window_frame: child
                .window_frame
                .clone()
                .or_else(|| parent.window_frame.clone()),
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

        if upper.starts_with("VECTOR") {
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
            "UUID" => Ok(DataType::Uuid),
            "JSON" => Ok(DataType::Json),
            "JSONB" => Ok(DataType::Jsonb),
            "BYTEA" => Ok(DataType::Bytea),
            "SERIAL" => Ok(DataType::Int4),
            "BIGSERIAL" => Ok(DataType::Int8),
            "SMALLSERIAL" => Ok(DataType::Int2),
            _ => Err(Error::query_execution(format!(
                "Unknown data type: {}",
                type_str
            ))),
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
    pub(crate) fn normalize_object_name(name: &sqlparser::ast::ObjectName) -> String {
        name.0
            .iter()
            .map(Self::normalize_ident)
            .collect::<Vec<_>>()
            .join(".")
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
                let returning = insert.returning.as_ref()
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
            Statement::Drop { names, if_exists, object_type, .. } => {
                if names.len() != 1 {
                    return Err(Error::query_execution("Multiple drops not supported"));
                }
                // SAFETY: We've verified names.len() == 1 above
                let name = Self::normalize_object_name(
                    names.first().ok_or_else(|| Error::query_execution("DROP requires a name"))?
                );

                match object_type {
                    sqlparser::ast::ObjectType::View => {
                        // DROP VIEW
                        Ok(LogicalPlan::DropView {
                            name,
                            if_exists,
                        })
                    }
                    sqlparser::ast::ObjectType::Table => {
                        // DROP TABLE
                        Ok(LogicalPlan::DropTable {
                            name,
                            if_exists,
                        })
                    }
                    _ => {
                        // Default to DROP TABLE for backwards compatibility
                        Ok(LogicalPlan::DropTable {
                            name,
                            if_exists,
                        })
                    }
                }
            }
            Statement::Truncate { table_names, .. } => {
                if table_names.is_empty() {
                    return Err(Error::query_execution("TRUNCATE requires a table name"));
                }
                if table_names.len() > 1 {
                    return Err(Error::query_execution("Multiple table TRUNCATE not supported"));
                }
                let first_table = table_names.first()
                    .ok_or_else(|| Error::query_execution("TRUNCATE requires a table name"))?;
                Ok(LogicalPlan::Truncate {
                    table_name: first_table.to_string(),
                })
            }
            Statement::Update { table, assignments, selection, returning, .. } => {
                // Extract RETURNING clause if present
                let returning_items = returning.as_ref()
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
                        tables.first()
                            .ok_or_else(|| Error::query_execution("DELETE requires a table"))?
                            .clone()
                    }
                    sqlparser::ast::FromTable::WithoutKeyword(tables) => {
                        if tables.len() != 1 {
                            return Err(Error::query_execution("Multi-table DELETE not supported"));
                        }
                        tables.first()
                            .ok_or_else(|| Error::query_execution("DELETE requires a table"))?
                            .clone()
                    }
                };
                // Extract RETURNING clause if present
                let returning = delete_stmt.returning.as_ref()
                    .map(|ret_items| self.convert_returning(ret_items))
                    .transpose()?;
                self.delete_to_plan(table, delete_stmt.selection.clone(), returning)
            }
            Statement::CreateIndex(create_index) => {
                // Extract index name
                let index_name = Self::normalize_object_name(
                    create_index.name.as_ref()
                        .ok_or_else(|| Error::query_execution("Index name is required"))?
                );

                // Extract table name
                let table = Self::normalize_object_name(&create_index.table_name);

                // Extract column name (we only support single-column indexes for now)
                if create_index.columns.is_empty() {
                    return Err(Error::query_execution("At least one column required for index"));
                }
                if create_index.columns.len() > 1 {
                    return Err(Error::query_execution("Multi-column vector indexes not yet supported"));
                }

                let first_col = create_index.columns.first()
                    .ok_or_else(|| Error::query_execution("At least one column required for index"))?;
                let column = match &first_col.expr {
                    Expr::Identifier(ident) => Self::normalize_ident(ident),
                    _ => return Err(Error::query_execution("Column name expected in CREATE INDEX")),
                };

                // Extract index type (USING clause)
                let index_type = create_index.using.as_ref().map(|ident| ident.value.to_lowercase());

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
            } => {
                self.create_trigger_to_plan(
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
                )
            }
            Statement::DropTrigger {
                if_exists,
                trigger_name,
                table_name,
                option,
            } => {
                self.drop_trigger_to_plan(if_exists, trigger_name, table_name, option)
            }
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
                        sqlparser::ast::CreateTableOptions::With(opts) |
                        sqlparser::ast::CreateTableOptions::Options(opts) => {
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
                                                _ => None // Skip non-key-value options
                                            }
                                        })
                                        .collect::<Vec<_>>()
                                        .join(", ")
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
            Statement::Savepoint { name } => {
                Ok(LogicalPlan::Savepoint { name: name.value.clone() })
            }
            Statement::ReleaseSavepoint { name } => {
                Ok(LogicalPlan::ReleaseSavepoint { name: name.value.clone() })
            }
            // Prepared statements
            Statement::Prepare { name, data_types, statement, .. } => {
                let param_types: Vec<DataType> = data_types.iter()
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
                let params: Result<Vec<LogicalExpr>> = parameters.iter()
                    .map(|e| self.expr_to_logical(e))
                    .collect();
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
            // Procedural statements
            Statement::CreateFunction(cf) => self.create_function_to_plan(cf),
            Statement::CreateProcedure { or_alter, name, params, body } => {
                // ProcedureParam has fields: name: Ident, data_type: DataType (no mode field)
                let param_list = params.unwrap_or_default().into_iter().map(|p| {
                    FunctionParam {
                        name: p.name.value.clone(),
                        data_type: self.sql_data_type_to_data_type(&p.data_type).unwrap_or(DataType::Text),
                        mode: ParamMode::In,  // ProcedureParam doesn't have mode field
                        default: None,
                    }
                }).collect();
                let body_str = body.iter().map(|s| format!("{}", s)).collect::<Vec<_>>().join("\n");
                Ok(LogicalPlan::CreateProcedure {
                    name: name.to_string(),
                    or_replace: or_alter,  // Map or_alter to or_replace
                    params: param_list,
                    body: body_str,
                    language: "sql".to_string(),  // Default to SQL
                })
            }
            Statement::DropFunction { if_exists, func_desc, .. } => {
                if let Some(fd) = func_desc.first() {
                    Ok(LogicalPlan::DropFunction {
                        name: fd.name.to_string(),
                        if_exists,
                    })
                } else {
                    Err(Error::query_execution("DROP FUNCTION requires a name"))
                }
            }
            Statement::DropProcedure { if_exists, proc_desc, .. } => {
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
                    sqlparser::ast::FunctionArguments::List(arg_list) => {
                        arg_list.args.into_iter()
                            .map(|arg| match arg {
                                sqlparser::ast::FunctionArg::Unnamed(fe) => match fe {
                                    sqlparser::ast::FunctionArgExpr::Expr(e) => self.expr_to_logical(&e),
                                    _ => Err(Error::query_execution("Unsupported CALL argument")),
                                },
                                _ => Err(Error::query_execution("Named CALL arguments not supported")),
                            })
                            .collect()
                    }
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
            Statement::CreateSequence { name, if_not_exists, .. } => {
                let seq_name = Self::normalize_object_name(&name);
                Ok(LogicalPlan::CreateSequence { name: seq_name, if_not_exists })
            }
            _ => Err(Error::query_execution(format!(
                "Statement not yet supported: {:?}",
                statement
            ))),
        }
    }

    /// Convert CREATE FUNCTION to plan
    fn create_function_to_plan(&self, cf: sqlparser::ast::CreateFunction) -> Result<LogicalPlan> {
        // OperateFunctionArg has: mode: Option<ArgMode>, name: Option<Ident>, data_type: DataType, default_expr: Option<Expr>
        let params = cf.args.unwrap_or_default().into_iter().map(|arg| {
            let data_type = self.sql_data_type_to_data_type(&arg.data_type).unwrap_or(DataType::Text);
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
        }).collect();

        let return_type = cf.return_type.as_ref()
            .map(|rt| self.sql_data_type_to_data_type(rt))
            .transpose()?;

        let body = match cf.function_body {
            Some(sqlparser::ast::CreateFunctionBody::AsBeforeOptions(expr)) => {
                match expr {
                    sqlparser::ast::Expr::Value(sqlparser::ast::Value::DollarQuotedString(dqs)) => dqs.value,
                    sqlparser::ast::Expr::Value(sqlparser::ast::Value::SingleQuotedString(s)) => s,
                    other => format!("{}", other),
                }
            }
            Some(sqlparser::ast::CreateFunctionBody::AsAfterOptions(expr)) => {
                match expr {
                    sqlparser::ast::Expr::Value(sqlparser::ast::Value::DollarQuotedString(dqs)) => dqs.value,
                    sqlparser::ast::Expr::Value(sqlparser::ast::Value::SingleQuotedString(s)) => s,
                    other => format!("{}", other),
                }
            }
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

    /// Convert a Query to a logical plan
    fn query_to_plan(&self, query: Query) -> Result<LogicalPlan> {
        // Handle WITH clause (CTEs)
        let (cte_plans, is_recursive) = if let Some(with_clause) = query.with {
            let is_recursive = with_clause.recursive;
            let mut ctes = Vec::new();
            for cte in with_clause.cte_tables {
                let cte_name = cte.alias.name.to_string();

                // For recursive CTEs, pre-register a placeholder schema
                // so that the recursive reference can resolve
                let column_aliases: Vec<String> = cte.alias.columns
                    .iter()
                    .map(|col| col.name.value.clone())
                    .collect();

                if is_recursive && !column_aliases.is_empty() {
                    // Use the explicit column aliases with Int8 as placeholder type
                    // (works for numeric recursion like n+1)
                    let schema = Arc::new(Schema::new(
                        column_aliases.iter().map(|name| {
                            Column::new(name, DataType::Int8)
                        }).collect()
                    ));
                    self.add_cte(cte_name.clone(), schema);
                }

                // Convert CTE query to logical plan
                let cte_plan = self.query_to_plan(*cte.query)?;

                // Apply column aliases if specified (rename CTE columns)
                let cte_schema = if !column_aliases.is_empty() {
                    let original_schema = cte_plan.schema();
                    if column_aliases.len() == original_schema.columns.len() {
                        // Rename columns using the aliases
                        Arc::new(Schema::new(
                            original_schema.columns.iter()
                                .zip(column_aliases.iter())
                                .map(|(col, alias)| {
                                    let mut new_col = col.clone();
                                    new_col.name = alias.clone();
                                    new_col
                                })
                                .collect()
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

            let exprs: Result<Vec<_>> = order_by.exprs.iter()
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

                    // If the ORDER BY expression contains aggregate functions and
                    // we have an aggregate plan, rewrite them to column references
                    if let Some(ref info) = aggregate_info {
                        Ok(Self::rewrite_order_by_aggregates(&logical_expr, info))
                    } else {
                        Ok(logical_expr)
                    }
                })
                .collect();
            let asc: Vec<_> = order_by.exprs.iter()
                .map(|order_by_expr| order_by_expr.asc.unwrap_or(true))
                .collect();

            plan = LogicalPlan::Sort {
                input: Box::new(plan),
                exprs: exprs?,
                asc,
            };
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
            SetExpr::SetOperation { op, set_quantifier, left, right } => {
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
            _ => Err(Error::query_execution("Unsupported set expression")),
        }
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
                select.from.first()
                    .ok_or_else(|| Error::query_execution("FROM clause is empty"))?
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

        // Handle GROUP BY or implicit aggregation (when aggregates are present without GROUP BY)
        if has_aggregates {
            let group_by = if let sqlparser::ast::GroupByExpr::Expressions(group_by_exprs, _) = &select.group_by {
                if !group_by_exprs.is_empty() {
                    let num_select_items = select.projection.len();
                    let group_by: Result<Vec<_>> = group_by_exprs.iter()
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
                                            _ => return Err(Error::query_execution(format!(
                                                "GROUP BY position {} refers to a wildcard or unsupported select item",
                                                ordinal
                                            ))),
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
                        let rewritten = Self::rewrite_expr_replace_aggregates(
                            &logical, &aggr_exprs, &group_by,
                        );
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
                    let on_parsed: Result<Vec<LogicalExpr>> = on_exprs
                        .iter()
                        .map(|e| self.expr_to_logical(e))
                        .collect();
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
            let is_lateral = matches!(
                &join.relation,
                TableFactor::Derived { lateral: true, .. }
            );

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

                let common_columns: Vec<String> = left_schema.columns.iter()
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
                        "NATURAL JOIN requires at least one common column between tables"
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
                    | JoinOperator::FullOuter(JoinConstraint::On(expr)) => {
                        Some(self.expr_to_logical(expr)?)
                    }
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
                sqlparser::ast::FunctionArg::Unnamed(arg_expr) => {
                    match arg_expr {
                        sqlparser::ast::FunctionArgExpr::Expr(e) => {
                            logical_args.push(self.expr_to_logical(e)?);
                        }
                        _ => {
                            return Err(Error::query_execution(
                                "Unsupported function argument type in table function"
                            ));
                        }
                    }
                }
                _ => {
                    return Err(Error::query_execution(
                        "Named arguments are not supported in table functions"
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
                        return Ok(LogicalPlan::TableFunction {
                            function_name: lower_name,
                            args: logical_args,
                            alias: table_alias,
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

                // Check if this is a system view (Phase 3 features)
                use crate::sql::phase3::SystemViewRegistry;
                let registry = SystemViewRegistry::new();

                if registry.is_system_view(&table_name) {
                    // This is a system view, not a regular table
                    return Ok(LogicalPlan::SystemView {
                        name: table_name,
                        args: vec![], // System views don't use arguments from table name
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
                        columns: vec![
                            Column {
                                name: "id".to_string(),
                                data_type: DataType::Int4,
                                nullable: false,
                                primary_key: false,
                                source_table: None,
                                source_table_name: None,
                                default_expr: None,
                                unique: false,
                                storage_mode: crate::ColumnStorageMode::Default,
                            },
                        ],
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
            TableFactor::Derived { subquery, alias, lateral } => {
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
                                                    "Unsupported function argument in TABLE() expression"
                                                ));
                                            }
                                        }
                                        _ => {
                                            return Err(Error::query_execution(
                                                "Named arguments not supported in TABLE() expression"
                                            ));
                                        }
                                    }
                                }
                            }
                            let table_alias = alias.as_ref().map(|a| a.name.value.clone());
                            Ok(LogicalPlan::TableFunction {
                                function_name: func_name,
                                args: logical_args,
                                alias: table_alias,
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
                    )))
                }
            }
            TableFactor::UNNEST { alias, array_exprs, .. } => {
                // Handle UNNEST(ARRAY[...]) syntax
                let mut logical_args = Vec::new();
                for expr in array_exprs {
                    logical_args.push(self.expr_to_logical(expr)?);
                }
                let table_alias = alias.as_ref().map(|a| a.name.value.clone());
                Ok(LogicalPlan::TableFunction {
                    function_name: "unnest".to_string(),
                    args: logical_args,
                    alias: table_alias,
                })
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
    fn select_items_to_exprs(&self, items: &[SelectItem], input: &LogicalPlan) -> Result<(Vec<LogicalExpr>, Vec<String>)> {
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
                        exprs.push(LogicalExpr::Column { table: None, name: column.name.clone() });
                        aliases.push(column.name.clone());
                    }
                }
                SelectItem::QualifiedWildcard(object_name, _) => {
                    // Expand alias.* or table.* to all columns from that table
                    let qualifier = object_name.0.iter().map(|i| i.value.clone()).collect::<Vec<_>>().join(".");
                    let schema = input.schema();
                    let mut matched = false;
                    for column in &schema.columns {
                        // Match by source_table_name (alias or real table name)
                        let col_table = column.source_table_name.as_deref().unwrap_or("");
                        if col_table.eq_ignore_ascii_case(&qualifier) || column.name.starts_with(&format!("{}.", qualifier)) {
                            exprs.push(LogicalExpr::Column { table: Some(qualifier.clone()), name: column.name.clone() });
                            aliases.push(column.name.clone());
                            matched = true;
                        }
                    }
                    // If no columns matched by source_table, expand ALL columns
                    // (fallback for when source_table isn't set)
                    if !matched {
                        for column in &schema.columns {
                            exprs.push(LogicalExpr::Column { table: None, name: column.name.clone() });
                            aliases.push(column.name.clone());
                        }
                    }
                }
                _ => return Err(Error::query_execution("SELECT item not supported")),
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
            Expr::CompoundIdentifier(idents) => {
                idents.last().map(|i| i.value.clone()).unwrap_or_else(|| format!("col_{}", index))
            }
            // Aggregate functions: use function name + column
            Expr::Function(func) => {
                let func_name = func.name.to_string().to_lowercase();
                match func.args {
                    sqlparser::ast::FunctionArguments::List(ref list) if !list.args.is_empty() => {
                        if let Some(sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(inner))) = list.args.first() {
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
                sqlparser::ast::Value::SingleQuotedString(s) => format!("'{}'", s),
                sqlparser::ast::Value::DoubleQuotedString(s) => format!("\"{}\"", s),
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
        if let LogicalPlan::Project { input, aliases, .. } = plan {
            if let LogicalPlan::Aggregate { group_by, aggr_exprs, .. } = input.as_ref() {
                let num_groups = group_by.len();
                let num_aggs = aggr_exprs.len();

                // The Project aliases are ordered: group columns first, then aggregate columns.
                // aliases[0..num_groups] -> GROUP BY aliases
                // aliases[num_groups..num_groups+num_aggs] -> aggregate aliases
                let group_by_aliases: Vec<String> = aliases.iter()
                    .take(num_groups)
                    .cloned()
                    .collect();
                let aggr_aliases: Vec<String> = aliases.iter()
                    .skip(num_groups)
                    .take(num_aggs)
                    .cloned()
                    .collect();

                return Some(AggregateInfo {
                    aggr_exprs: aggr_exprs.clone(),
                    aggr_aliases,
                    group_by_exprs: group_by.clone(),
                    group_by_aliases,
                });
            }
        }
        None
    }

    /// Rewrite an ORDER BY expression by replacing aggregate function references
    /// with column references to the corresponding output alias.
    /// For example, `SUM(val)` becomes `Column { name: "total" }` if that aggregate
    /// has alias "total" in the output schema.
    /// Also handles GROUP BY column references that need remapping.
    fn rewrite_order_by_aggregates(expr: &LogicalExpr, info: &AggregateInfo) -> LogicalExpr {
        match expr {
            LogicalExpr::AggregateFunction { fun, args, distinct } => {
                // Find this aggregate in the plan's aggregate expressions
                for (i, aggr_expr) in info.aggr_exprs.iter().enumerate() {
                    if let LogicalExpr::AggregateFunction {
                        fun: aggr_fun, args: aggr_args, distinct: aggr_distinct
                    } = aggr_expr {
                        if fun == aggr_fun && args == aggr_args && distinct == aggr_distinct {
                            // Found a match - replace with column reference to the output alias
                            if let Some(alias) = info.aggr_aliases.get(i) {
                                return LogicalExpr::Column {
                                    table: None,
                                    name: alias.clone(),
                                };
                            }
                        }
                    }
                }
                // No match found - keep as-is (may fail at evaluation, but that's expected
                // for aggregates not in SELECT list)
                expr.clone()
            }
            LogicalExpr::BinaryExpr { left, op, right } => {
                LogicalExpr::BinaryExpr {
                    left: Box::new(Self::rewrite_order_by_aggregates(left, info)),
                    op: *op,
                    right: Box::new(Self::rewrite_order_by_aggregates(right, info)),
                }
            }
            LogicalExpr::UnaryExpr { op, expr: inner } => {
                LogicalExpr::UnaryExpr {
                    op: *op,
                    expr: Box::new(Self::rewrite_order_by_aggregates(inner, info)),
                }
            }
            LogicalExpr::Cast { expr: inner, data_type } => {
                LogicalExpr::Cast {
                    expr: Box::new(Self::rewrite_order_by_aggregates(inner, info)),
                    data_type: data_type.clone(),
                }
            }
            LogicalExpr::ScalarFunction { fun, args } => {
                let rewritten_args: Vec<_> = args.iter()
                    .map(|a| Self::rewrite_order_by_aggregates(a, info))
                    .collect();
                LogicalExpr::ScalarFunction {
                    fun: fun.clone(),
                    args: rewritten_args,
                }
            }
            // For column references in an aggregate context, check if they match
            // a GROUP BY expression and remap to the output alias
            LogicalExpr::Column { table, name } => {
                for (i, group_expr) in info.group_by_exprs.iter().enumerate() {
                    if group_expr == expr {
                        if let Some(alias) = info.group_by_aliases.get(i) {
                            return LogicalExpr::Column {
                                table: None,
                                name: alias.clone(),
                            };
                        }
                    }
                }
                expr.clone()
            }
            // All other expression types: return as-is
            _ => expr.clone(),
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
            LogicalExpr::Case { expr: base, when_then, else_result } => {
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
            LogicalExpr::Between { expr: inner, low, high, .. } => {
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
            (
                LogicalExpr::Column { table: t1, name: n1 },
                LogicalExpr::Column { table: t2, name: n2 },
            ) => {
                if n1 != n2 {
                    return false;
                }
                match (t1, t2) {
                    (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                    _ => true,
                }
            }
            (
                LogicalExpr::ScalarFunction { fun: f1, args: a1 },
                LogicalExpr::ScalarFunction { fun: f2, args: a2 },
            ) => {
                f1.eq_ignore_ascii_case(f2)
                    && a1.len() == a2.len()
                    && a1.iter().zip(a2.iter()).all(|(x, y)| Self::exprs_equivalent(x, y))
            }
            (
                LogicalExpr::AggregateFunction { fun: f1, args: a1, distinct: d1 },
                LogicalExpr::AggregateFunction { fun: f2, args: a2, distinct: d2 },
            ) => {
                f1 == f2
                    && d1 == d2
                    && a1.len() == a2.len()
                    && a1.iter().zip(a2.iter()).all(|(x, y)| Self::exprs_equivalent(x, y))
            }
            (
                LogicalExpr::BinaryExpr { left: l1, op: op1, right: r1 },
                LogicalExpr::BinaryExpr { left: l2, op: op2, right: r2 },
            ) => {
                op1 == op2
                    && Self::exprs_equivalent(l1, l2)
                    && Self::exprs_equivalent(r1, r2)
            }
            (
                LogicalExpr::UnaryExpr { op: op1, expr: e1 },
                LogicalExpr::UnaryExpr { op: op2, expr: e2 },
            ) => op1 == op2 && Self::exprs_equivalent(e1, e2),
            (
                LogicalExpr::Cast { expr: e1, data_type: d1 },
                LogicalExpr::Cast { expr: e2, data_type: d2 },
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
            LogicalExpr::BinaryExpr { left, op, right } => {
                LogicalExpr::BinaryExpr {
                    left: Box::new(Self::rewrite_expr_replace_aggregates(left, aggr_exprs, group_by)),
                    op: *op,
                    right: Box::new(Self::rewrite_expr_replace_aggregates(right, aggr_exprs, group_by)),
                }
            }
            LogicalExpr::UnaryExpr { op, expr: inner } => {
                LogicalExpr::UnaryExpr {
                    op: *op,
                    expr: Box::new(Self::rewrite_expr_replace_aggregates(inner, aggr_exprs, group_by)),
                }
            }
            LogicalExpr::Cast { expr: inner, data_type } => {
                LogicalExpr::Cast {
                    expr: Box::new(Self::rewrite_expr_replace_aggregates(inner, aggr_exprs, group_by)),
                    data_type: data_type.clone(),
                }
            }
            LogicalExpr::Case { expr: base, when_then, else_result } => {
                LogicalExpr::Case {
                    expr: base.as_ref().map(|e| Box::new(Self::rewrite_expr_replace_aggregates(e, aggr_exprs, group_by))),
                    when_then: when_then.iter().map(|(w, t)| {
                        (
                            Self::rewrite_expr_replace_aggregates(w, aggr_exprs, group_by),
                            Self::rewrite_expr_replace_aggregates(t, aggr_exprs, group_by),
                        )
                    }).collect(),
                    else_result: else_result.as_ref().map(|e| Box::new(Self::rewrite_expr_replace_aggregates(e, aggr_exprs, group_by))),
                }
            }
            LogicalExpr::IsNull { expr: inner, is_null } => {
                LogicalExpr::IsNull {
                    expr: Box::new(Self::rewrite_expr_replace_aggregates(inner, aggr_exprs, group_by)),
                    is_null: *is_null,
                }
            }
            LogicalExpr::Between { expr: inner, low, high, negated } => {
                LogicalExpr::Between {
                    expr: Box::new(Self::rewrite_expr_replace_aggregates(inner, aggr_exprs, group_by)),
                    low: Box::new(Self::rewrite_expr_replace_aggregates(low, aggr_exprs, group_by)),
                    high: Box::new(Self::rewrite_expr_replace_aggregates(high, aggr_exprs, group_by)),
                    negated: *negated,
                }
            }
            LogicalExpr::InList { expr: inner, list, negated } => {
                LogicalExpr::InList {
                    expr: Box::new(Self::rewrite_expr_replace_aggregates(inner, aggr_exprs, group_by)),
                    list: list.iter().map(|e| Self::rewrite_expr_replace_aggregates(e, aggr_exprs, group_by)).collect(),
                    negated: *negated,
                }
            }
            LogicalExpr::ScalarFunction { fun, args } => {
                LogicalExpr::ScalarFunction {
                    fun: fun.clone(),
                    args: args.iter().map(|a| Self::rewrite_expr_replace_aggregates(a, aggr_exprs, group_by)).collect(),
                }
            }
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
                            let args: Result<Vec<_>> = arg_list.args.iter()
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
                                                _ => Err(Error::query_execution("Complex function args not supported"))
                                            }
                                        }
                                        _ => Err(Error::query_execution("Named function args not supported"))
                                    }
                                })
                                .collect();
                            let distinct = matches!(arg_list.duplicate_treatment,
                                Some(sqlparser::ast::DuplicateTreatment::Distinct));
                            (args?, distinct)
                        }
                        _ => (vec![], false),
                    };

                    Ok(Some(LogicalExpr::AggregateFunction {
                        fun,
                        args,
                        distinct,
                    }))
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
                let parsed_args: Result<Vec<_>> = arg_list.args.iter()
                    .map(|arg| {
                        match arg {
                            sqlparser::ast::FunctionArg::Unnamed(func_arg_expr) => {
                                match func_arg_expr {
                                    sqlparser::ast::FunctionArgExpr::Expr(expr) => {
                                        self.expr_to_logical(&expr)
                                    }
                                    _ => Err(Error::query_execution("STRING_AGG requires value expressions"))
                                }
                            }
                            _ => Err(Error::query_execution("Named function args not supported"))
                        }
                    })
                    .collect();
                parsed_args?
            }
            _ => return Err(Error::query_execution("STRING_AGG requires arguments")),
        };

        if args.len() != 2 {
            return Err(Error::query_execution("STRING_AGG requires exactly 2 arguments: value and delimiter"));
        }

        // Extract delimiter from second argument (must be a literal string)
        let delimiter_arg = args.get(1)
            .ok_or_else(|| Error::query_execution("STRING_AGG requires a delimiter argument"))?;
        let delimiter = match delimiter_arg {
            LogicalExpr::Literal(crate::Value::String(s)) => s.clone(),
            _ => return Err(Error::query_execution("STRING_AGG delimiter must be a string literal")),
        };

        let distinct = match &func.args {
            sqlparser::ast::FunctionArguments::List(arg_list) => {
                matches!(arg_list.duplicate_treatment, Some(sqlparser::ast::DuplicateTreatment::Distinct))
            }
            _ => false,
        };

        let value_expr = args.first()
            .ok_or_else(|| Error::query_execution("STRING_AGG requires a value argument"))?;
        Ok(Some(LogicalExpr::AggregateFunction {
            fun: AggregateFunction::StringAgg { delimiter },
            args: vec![value_expr.clone()], // Only the value expression
            distinct,
        }))
    }

    /// Parse a window function expression
    fn parse_window_function(&self, func: &sqlparser::ast::Function) -> Result<LogicalExpr> {
        use super::logical_plan::{WindowFunctionType, WindowFrame, WindowFrameType};

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
            _ => return Err(Error::query_execution(format!(
                "Unknown window function: {}", func_name
            ))),
        };

        // Parse function arguments
        let args = match &func.args {
            sqlparser::ast::FunctionArguments::List(arg_list) => {
                arg_list.args.iter()
                    .filter_map(|arg| {
                        match arg {
                            sqlparser::ast::FunctionArg::Unnamed(func_arg_expr) => {
                                match func_arg_expr {
                                    sqlparser::ast::FunctionArgExpr::Expr(expr) => {
                                        self.expr_to_logical(expr).ok()
                                    }
                                    _ => None
                                }
                            }
                            _ => None
                        }
                    })
                    .collect()
            }
            _ => vec![],
        };

        // Parse OVER clause
        let over = func.over.as_ref().ok_or_else(|| {
            Error::query_execution("Window function requires OVER clause")
        })?;

        // Resolve the window specification: either inline or from a named window reference
        let resolved_spec: Option<sqlparser::ast::WindowSpec>;
        let spec_ref = match over {
            sqlparser::ast::WindowType::WindowSpec(spec) => spec,
            sqlparser::ast::WindowType::NamedWindow(name) => {
                resolved_spec = Some(
                    self.get_named_window(&name.value).ok_or_else(|| {
                        Error::query_execution(format!(
                            "Window \"{}\" is not defined",
                            name.value
                        ))
                    })?,
                );
                resolved_spec.as_ref().ok_or_else(|| {
                    Error::query_execution("Internal error resolving named window")
                })?
            }
        };

        // Parse window specification fields from the resolved spec
        let partition_by: Vec<LogicalExpr> = spec_ref.partition_by.iter()
            .filter_map(|expr| self.expr_to_logical(expr).ok())
            .collect();

        let order_by: Vec<(LogicalExpr, bool)> = spec_ref.order_by.iter()
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

            WindowFrame {
                frame_type,
                start,
                end,
            }
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
    pub fn convert_expr_to_logical(&self, expr: &sqlparser::ast::Expr, _schema: Option<&crate::Schema>) -> Result<LogicalExpr> {
        self.expr_to_logical(expr)
    }

    /// Convert sqlparser Expr to LogicalExpr
    fn expr_to_logical(&self, expr: &Expr) -> Result<LogicalExpr> {
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
                        idents.get(idents.len() - 2)
                            .ok_or_else(|| Error::query_execution("Invalid compound identifier"))?
                    );
                    let column_name = Self::normalize_ident(
                        idents.last()
                            .ok_or_else(|| Error::query_execution("Empty compound identifier"))?
                    );
                    Ok(LogicalExpr::Column {
                        table: Some(table_alias),
                        name: column_name,
                    })
                } else {
                    let column_name = Self::normalize_ident(
                        idents.last()
                            .ok_or_else(|| Error::query_execution("Empty compound identifier"))?
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
                            let index = index_str.parse::<usize>()
                                .map_err(|_| Error::query_execution(format!(
                                    "Invalid parameter placeholder: {}. Expected format: $1, $2, etc.",
                                    placeholder
                                )))?;

                            if index == 0 {
                                return Err(Error::query_execution(
                                    "Parameter indices must be 1-based (e.g., $1, $2)"
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

            Expr::Like { negated, expr, pattern, .. } => {
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

            Expr::ILike { negated, expr, pattern, .. } => {
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

            Expr::SimilarTo { negated, expr, pattern, .. } => {
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

            Expr::RLike { negated, expr, pattern, .. } => {
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
                        let args: Result<Vec<_>> = arg_list.args.iter()
                            .map(|arg| {
                                match arg {
                                    sqlparser::ast::FunctionArg::Unnamed(func_arg_expr) => {
                                        match func_arg_expr {
                                            sqlparser::ast::FunctionArgExpr::Expr(expr) => {
                                                self.expr_to_logical(&expr)
                                            }
                                            _ => Err(Error::query_execution("Complex function args not supported"))
                                        }
                                    }
                                    _ => Err(Error::query_execution("Named function args not supported"))
                                }
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

            // Array literals: ARRAY[1, 2, 3] or '[1.0, 2.0, 3.0]' (for vectors)
            Expr::Array(sqlparser::ast::Array { elem, .. }) => {
                // Check if all elements are numeric - could be vector or array
                let all_numeric = elem.iter().all(|e| matches!(e, Expr::Value(sqlparser::ast::Value::Number(_, _))));
                let has_floats = elem.iter().any(|e| {
                    if let Expr::Value(sqlparser::ast::Value::Number(n, _)) = e {
                        n.contains('.')
                    } else {
                        false
                    }
                });

                // If all floats, treat as vector for vector search compatibility
                if all_numeric && has_floats {
                    let elements: Result<Vec<f32>> = elem.iter()
                        .map(|e| {
                            match e {
                                Expr::Value(sqlparser::ast::Value::Number(n, _)) => {
                                    n.parse::<f32>()
                                        .map_err(|e| Error::query_execution(format!("Invalid vector element: {}", e)))
                                }
                                _ => Err(Error::query_execution("Vector elements must be numbers"))
                            }
                        })
                        .collect();
                    Ok(LogicalExpr::Literal(Value::Vector(elements?)))
                } else {
                    // General array - convert each element to a Value
                    let elements: Result<Vec<Value>> = elem.iter()
                        .map(|e| {
                            match e {
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
                                    Ok(Value::String(s.clone()))
                                }
                                Expr::Value(sqlparser::ast::Value::Boolean(b)) => {
                                    Ok(Value::Boolean(*b))
                                }
                                Expr::Value(sqlparser::ast::Value::Null) => {
                                    Ok(Value::Null)
                                }
                                _ => Err(Error::query_execution("Unsupported array element type"))
                            }
                        })
                        .collect();
                    Ok(LogicalExpr::Literal(Value::Array(elements?)))
                }
            }

            // CAST expressions: CAST(expr AS type) or expr::type
            Expr::Cast { expr, data_type, .. } => {
                let logical_expr = self.expr_to_logical(expr)?;
                let target_type = self.sql_data_type_to_data_type(data_type)?;
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

            // Scalar subquery: `(SELECT col FROM t WHERE ...)` used as
            // an expression. We don't materialise here because the
            // caller (e.g. UPDATE SET) needs to supply the outer-row
            // context for correlation. At simple-evaluation sites the
            // evaluator materialises uncorrelated cases via
            // `materialize_subqueries`.
            Expr::Subquery(subquery) => {
                let plan = self.query_to_plan((**subquery).clone())?;
                Ok(LogicalExpr::ScalarSubquery { subquery: Box::new(plan) })
            }

            // IN subquery: expr IN (SELECT ...)
            Expr::InSubquery { expr, subquery, negated } => {
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
            Expr::Between { expr, negated, low, high } => {
                Ok(LogicalExpr::Between {
                    expr: Box::new(self.expr_to_logical(expr)?),
                    low: Box::new(self.expr_to_logical(low)?),
                    high: Box::new(self.expr_to_logical(high)?),
                    negated: *negated,
                })
            }

            // CASE expression
            Expr::Case { operand, conditions, results, else_result } => {
                // Convert operand (if present)
                let expr = if let Some(op) = operand {
                    Some(Box::new(self.expr_to_logical(op)?))
                } else {
                    None
                };

                // Convert WHEN conditions and THEN results
                let when_then: Vec<(LogicalExpr, LogicalExpr)> = conditions.iter()
                    .zip(results.iter())
                    .map(|(cond, res)| {
                        Ok((self.expr_to_logical(cond)?, self.expr_to_logical(res)?))
                    })
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

            _ => Err(Error::query_execution(format!(
                "Expression not yet supported: {:?}",
                expr
            ))),
        }
    }

    /// Convert SQL value to internal Value
    fn sql_value_to_value(&self, value: &sqlparser::ast::Value) -> Result<Value> {
        match value {
            sqlparser::ast::Value::Number(n, _) => {
                // Try integer types first (more common and exact)
                if let Ok(i) = n.parse::<i32>() {
                    Ok(Value::Int4(i))
                } else if let Ok(i) = n.parse::<i64>() {
                    Ok(Value::Int8(i))
                } else if let Ok(f) = n.parse::<f64>() {
                    // If it has a decimal point or is too large for i64, treat as float
                    Ok(Value::Float8(f))
                } else if let Ok(f) = n.parse::<f32>() {
                    Ok(Value::Float4(f))
                } else {
                    Err(Error::query_execution(format!("Invalid number: {}", n)))
                }
            }
            sqlparser::ast::Value::SingleQuotedString(s) => Ok(Value::String(s.clone())),
            // Dollar-quoted strings: `$$hello$$` or `$tag$hello$tag$`. The
            // content is already safely delimited by the parser, so we
            // just lift it to a plain String value — the tag is
            // discarded.
            sqlparser::ast::Value::DollarQuotedString(dqs) => Ok(Value::String(dqs.value.clone())),
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
            SqlBinaryOp::Custom(op_str) => {
                match op_str.as_str() {
                    "<->" => Ok(BinaryOperator::VectorL2Distance),
                    "<=>" => Ok(BinaryOperator::VectorCosineDistance),
                    "<#>" => Ok(BinaryOperator::VectorInnerProduct),
                    _ => Err(Error::query_execution(format!(
                        "Custom operator not supported: {}",
                        op_str
                    ))),
                }
            }
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

            let rows: Result<Vec<Vec<LogicalExpr>>> = values.rows.iter()
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
                        cols.len(), source_col_count
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
        // Extract storage modes from original SQL if available
        let storage_modes = if let Some(ref sql) = self.original_sql {
            crate::sql::Parser::extract_column_storage_modes(sql)
        } else {
            std::collections::HashMap::new()
        };

        let mut column_defs: Vec<_> = columns.iter()
            .map(|col| self.sql_column_def_to_column_def(col))
            .collect::<Result<Vec<_>>>()?;

        // Apply storage modes from original SQL
        for col_def in &mut column_defs {
            if let Some(mode) = storage_modes.get(&col_def.name) {
                col_def.storage_mode = *mode;
            }
        }

        // Convert sqlparser constraints to our TableConstraint type
        let mut constraints: Vec<_> = sql_constraints.iter()
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
                        ..
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

        // Propagate table-level PRIMARY KEY constraint to column defs.
        // WordPress uses `PRIMARY KEY (col)` at the table level, not inline.
        // Without this, col.primary_key stays false and SERIAL auto-fill never fires.
        for constraint in &constraints {
            if let TableConstraint::PrimaryKey { columns: pk_cols, .. } = constraint {
                for pk_col_name in pk_cols {
                    if let Some(col_def) = column_defs.iter_mut().find(|c| c.name.eq_ignore_ascii_case(pk_col_name)) {
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
                        if let Some(col_def) = column_defs.iter_mut().find(|c| c.name.eq_ignore_ascii_case(uq_col_name)) {
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

    /// Convert sqlparser TableConstraint to our internal representation
    fn convert_table_constraint(
        &self,
        constraint: &sqlparser::ast::TableConstraint,
        table_name: &str,
    ) -> Option<TableConstraint> {
        use sqlparser::ast::TableConstraint as SqlTC;

        match constraint {
            SqlTC::PrimaryKey { name, columns, .. } => {
                Some(TableConstraint::PrimaryKey {
                    name: name.as_ref().map(|n| n.to_string()),
                    columns: columns.iter().map(|c| c.to_string()).collect(),
                })
            }
            SqlTC::Unique { name, columns, .. } => {
                Some(TableConstraint::Unique {
                    name: name.as_ref().map(|n| n.to_string()),
                    columns: columns.iter().map(|c| c.to_string()).collect(),
                })
            }
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
                    .map(|c| (c.deferrable.unwrap_or(false), c.initially.map(|i| matches!(i, sqlparser::ast::DeferrableInitial::Deferred)).unwrap_or(false)))
                    .unwrap_or((false, false));

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
            return Err(Error::query_execution(
                "ALTER TABLE requires an operation"
            ));
        }

        // Single operation: return directly (no wrapper needed)
        if operations.len() == 1 {
            let operation = operations.into_iter().next()
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
            AlterTableOperation::AddColumn { column_def, if_not_exists, .. } => {
                let col_def = self.sql_column_def_to_column_def(&column_def)?;
                Ok(LogicalPlan::AlterTableAddColumn {
                    table_name,
                    column_def: col_def,
                    if_not_exists,
                })
            }
            AlterTableOperation::DropColumn { column_name, if_exists, cascade } => {
                Ok(LogicalPlan::AlterTableDropColumn {
                    table_name,
                    column_name: column_name.value,
                    if_exists,
                    cascade,
                })
            }
            AlterTableOperation::RenameColumn { old_column_name, new_column_name } => {
                Ok(LogicalPlan::AlterTableRenameColumn {
                    table_name,
                    old_column_name: old_column_name.value,
                    new_column_name: new_column_name.value,
                })
            }
            AlterTableOperation::RenameTable { table_name: new_name } => {
                Ok(LogicalPlan::AlterTableRename {
                    table_name,
                    new_table_name: new_name.to_string(),
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
                    generated_as: sqlparser::ast::GeneratedAs::Always
                        | sqlparser::ast::GeneratedAs::ByDefault,
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
        })
    }

    /// Convert SQL data type to internal DataType
    fn sql_data_type_to_data_type(&self, data_type: &SqlDataType) -> Result<DataType> {
        match data_type {
            SqlDataType::Boolean => Ok(DataType::Boolean),
            SqlDataType::SmallInt(_) | SqlDataType::Int2(_) => Ok(DataType::Int2),
            SqlDataType::Int(_) | SqlDataType::Integer(_) | SqlDataType::Int4(_) => Ok(DataType::Int4),
            SqlDataType::BigInt(_) | SqlDataType::Int8(_) => Ok(DataType::Int8),
            SqlDataType::Real => Ok(DataType::Float4),  // REAL is Float4 in PostgreSQL
            SqlDataType::Float(_) => Ok(DataType::Float8),  // FLOAT(n) defaults to Float8
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
            SqlDataType::Text => Ok(DataType::Text),
            SqlDataType::Bytea => Ok(DataType::Bytea),
            SqlDataType::Date => Ok(DataType::Date),
            SqlDataType::Time(_, _) => Ok(DataType::Time),
            SqlDataType::Timestamp(_, _) => Ok(DataType::Timestamp),
            SqlDataType::Uuid => Ok(DataType::Uuid),
            SqlDataType::JSON => Ok(DataType::Json),
            SqlDataType::JSONB => Ok(DataType::Jsonb),
            // Handle NUMERIC/DECIMAL types (PostgreSQL arbitrary precision decimal)
            SqlDataType::Numeric(_) | SqlDataType::Decimal(_) => Ok(DataType::Numeric),
            // Handle PostgreSQL SERIAL types
            SqlDataType::Custom(object_name, type_modifiers) => {
                let type_name = object_name.to_string().to_uppercase();
                match type_name.as_str() {
                    "SERIAL" => Ok(DataType::Int4),      // SERIAL is Int4 with auto-increment
                    "BIGSERIAL" => Ok(DataType::Int8),   // BIGSERIAL is Int8 with auto-increment
                    "SMALLSERIAL" => Ok(DataType::Int2), // SMALLSERIAL is Int2 with auto-increment
                    "VECTOR" => {
                        // Parse dimension from type modifiers: VECTOR(1536)
                        if type_modifiers.len() == 1 {
                            let modifier = type_modifiers.first()
                                .ok_or_else(|| Error::query_execution("VECTOR type requires dimension"))?;
                            let dimension = modifier.parse::<usize>()
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
                    _ => Err(Error::query_execution(format!(
                        "Custom data type not yet supported: {}",
                        type_name
                    ))),
                }
            }
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
                let v = n.parse::<usize>()
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
                    Error::query_execution(format!(
                        "LIMIT/OFFSET must be a number (got quoted value '{s}')"
                    ))
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
    /// - sharding_strategy = 'hash'
    /// - shard_count = 16
    fn parse_index_options(&self, with_exprs: &[Expr]) -> Result<Vec<super::logical_plan::IndexOption>> {
        use super::logical_plan::{IndexOption, QuantizationType};

        let mut options = Vec::new();

        for expr in with_exprs {
            // Each expression should be a binary operation like "m = 16"
            let (name, value) = match expr {
                Expr::BinaryOp { left, op: SqlBinaryOp::Eq, right } => {
                    let name = match left.as_ref() {
                        Expr::Identifier(ident) => ident.value.to_lowercase(),
                        _ => return Err(Error::query_execution("Expected identifier on left side of WITH option")),
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
                            _ => return Err(Error::query_execution(format!(
                                "Invalid quantization type: {}. Expected 'none', 'scalar', 'product', or 'auto'",
                                s
                            ))),
                        };
                        options.push(IndexOption::Quantization(quant_type));
                    } else {
                        return Err(Error::query_execution("quantization must be a string"));
                    }
                }
                "pq_subquantizers" => {
                    if let Expr::Value(sqlparser::ast::Value::Number(n, _)) = value {
                        let count = n.parse::<usize>()
                            .map_err(|_| Error::query_execution("Invalid pq_subquantizers value"))?;
                        options.push(IndexOption::PqSubquantizers(count));
                    } else {
                        return Err(Error::query_execution("pq_subquantizers must be a number"));
                    }
                }
                "pq_centroids" => {
                    if let Expr::Value(sqlparser::ast::Value::Number(n, _)) = value {
                        let count = n.parse::<usize>()
                            .map_err(|_| Error::query_execution("Invalid pq_centroids value"))?;
                        options.push(IndexOption::PqCentroids(count));
                    } else {
                        return Err(Error::query_execution("pq_centroids must be a number"));
                    }
                }
                "m" => {
                    if let Expr::Value(sqlparser::ast::Value::Number(n, _)) = value {
                        let m_param = n.parse::<usize>()
                            .map_err(|_| Error::query_execution("Invalid m value"))?;
                        options.push(IndexOption::HnswM(m_param));
                    } else {
                        return Err(Error::query_execution("m must be a number"));
                    }
                }
                "ef_construction" => {
                    if let Expr::Value(sqlparser::ast::Value::Number(n, _)) = value {
                        let ef = n.parse::<usize>()
                            .map_err(|_| Error::query_execution("Invalid ef_construction value"))?;
                        options.push(IndexOption::EfConstruction(ef));
                    } else {
                        return Err(Error::query_execution("ef_construction must be a number"));
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
                        let count = n.parse::<usize>()
                            .map_err(|_| Error::query_execution("Invalid shard_count value"))?;
                        options.push(IndexOption::ShardCount(count));
                    } else {
                        return Err(Error::query_execution("shard_count must be a number"));
                    }
                }
                _ => {
                    return Err(Error::query_execution(format!(
                        "Unknown CREATE INDEX option: {}",
                        name
                    )));
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
            _ => return Err(Error::query_execution("Complex table expressions in UPDATE not supported")),
        };

        // Convert assignments to (column_name, value_expr) pairs
        let assignments: Result<Vec<_>> = assignments.iter()
            .map(|assignment| {
                // Extract column name from AssignmentTarget
                let column_name = match &assignment.target {
                    sqlparser::ast::AssignmentTarget::ColumnName(object_name) => {
                        object_name.0.iter()
                            .map(|ident| ident.value.clone())
                            .collect::<Vec<_>>()
                            .join(".")
                    }
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
        items.iter()
            .map(|item| {
                match item {
                    sqlparser::ast::SelectItem::Wildcard(_) => {
                        Ok(ReturningItem::Wildcard)
                    }
                    sqlparser::ast::SelectItem::UnnamedExpr(sqlparser::ast::Expr::Identifier(ident)) => {
                        Ok(ReturningItem::Column(Self::normalize_ident(ident)))
                    }
                    sqlparser::ast::SelectItem::UnnamedExpr(expr) => {
                        // Expression without alias - generate alias from expression text
                        let logical_expr = self.expr_to_logical(expr)?;
                        let alias = format!("{expr}");
                        Ok(ReturningItem::Expression { expr: logical_expr, alias })
                    }
                    sqlparser::ast::SelectItem::ExprWithAlias { expr, alias } => {
                        let logical_expr = self.expr_to_logical(expr)?;
                        Ok(ReturningItem::Expression { expr: logical_expr, alias: alias.value.clone() })
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
    fn convert_on_conflict(
        &self,
        on_insert: &Option<sqlparser::ast::OnInsert>,
    ) -> Result<Option<OnConflictAction>> {
        let on = match on_insert {
            Some(on) => on,
            None => return Ok(None),
        };
        match on {
            sqlparser::ast::OnInsert::OnConflict(conflict) => {
                match &conflict.action {
                    sqlparser::ast::OnConflictAction::DoNothing => {
                        Ok(Some(OnConflictAction::DoNothing))
                    }
                    sqlparser::ast::OnConflictAction::DoUpdate(do_update) => {
                        let assignments = do_update.assignments.iter()
                            .map(|a| {
                                let col_name = a.target.to_string();
                                let expr = self.expr_to_logical(&a.value)?;
                                Ok((col_name, expr))
                            })
                            .collect::<Result<Vec<_>>>()?;
                        Ok(Some(OnConflictAction::DoUpdate { assignments }))
                    }
                }
            }
            sqlparser::ast::OnInsert::DuplicateKeyUpdate(assignments) => {
                // MySQL ON DUPLICATE KEY UPDATE — convert to DoUpdate
                let assign_pairs = assignments.iter()
                    .map(|a| {
                        let col_name = a.target.to_string();
                        let expr = self.expr_to_logical(&a.value)?;
                        Ok((col_name, expr))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(Some(OnConflictAction::DoUpdate { assignments: assign_pairs }))
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
            _ => return Err(Error::query_execution("Complex table expressions in DELETE not supported")),
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
        let trigger_events: Result<Vec<TriggerEvent>> = events.iter()
            .map(|event| {
                match event {
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
                }
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
            if events.iter().any(|e| matches!(e, TriggerEvent::Truncate))
                && for_each == TriggerFor::Row {
                return Err(Error::query_execution(
                    "TRUNCATE triggers do not support FOR EACH ROW - use FOR EACH STATEMENT"
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
        let transition_tables: Vec<TransitionTable> = referencing.iter()
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
                    sqlparser::ast::TriggerReferencingType::OldTable => {
                        TransitionTable::OldTable { alias }
                    }
                    sqlparser::ast::TriggerReferencingType::NewTable => {
                        TransitionTable::NewTable { alias }
                    }
                }
            })
            .collect();

        // Validate: REFERENCING only allowed for FOR EACH STATEMENT triggers
        if !transition_tables.is_empty() && for_each == TriggerFor::Row {
            return Err(Error::query_execution(
                "REFERENCING clause only valid for FOR EACH STATEMENT triggers"
            ));
        }

        // Validate OLD TABLE only for UPDATE/DELETE, NEW TABLE only for INSERT/UPDATE
        let trigger_events_val = trigger_events.as_ref();
        if let Ok(events) = trigger_events_val {
            for tt in &transition_tables {
                match tt {
                    TransitionTable::OldTable { .. } => {
                        if !events.iter().any(|e| matches!(e, TriggerEvent::Update(_) | TriggerEvent::Delete)) {
                            return Err(Error::query_execution(
                                "OLD TABLE in REFERENCING clause requires UPDATE or DELETE trigger event"
                            ));
                        }
                    }
                    TransitionTable::NewTable { .. } => {
                        if !events.iter().any(|e| matches!(e, TriggerEvent::Insert | TriggerEvent::Update(_))) {
                            return Err(Error::query_execution(
                                "NEW TABLE in REFERENCING clause requires INSERT or UPDATE trigger event"
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
                let initially_deferred = c.initially
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
                        "Only CASCADE and RESTRICT are supported for DROP TRIGGER"
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
            costs: true, // PostgreSQL default
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
                let value = option.arg.as_ref()
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::sql::Parser;

    #[test]
    fn test_select_to_plan() {
        let parser = Parser::new();
        let statement = parser.parse_one("SELECT id, name FROM users WHERE id = 1")
            .expect("Failed to parse SQL statement");

        let planner = Planner::new();
        let plan = planner.statement_to_plan(statement);
        assert!(plan.is_ok());
    }

    #[test]
    fn test_insert_to_plan() {
        let parser = Parser::new();
        let statement = parser.parse_one(
            "INSERT INTO users (name, email) VALUES ('Alice', 'alice@example.com')"
        ).expect("Failed to parse SQL statement");

        let planner = Planner::new();
        let plan = planner.statement_to_plan(statement);
        assert!(plan.is_ok());
    }

    #[test]
    fn test_create_table_to_plan() {
        let parser = Parser::new();
        let statement = parser.parse_one(
            "CREATE TABLE users (id SERIAL PRIMARY KEY, name TEXT NOT NULL)"
        ).expect("Failed to parse SQL statement");

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
        let statement = parser.parse_one(
            "CREATE TRIGGER audit_insert AFTER INSERT ON users FOR EACH ROW EXECUTE FUNCTION audit_log()"
        ).expect("Failed to parse CREATE TRIGGER statement");

        let planner = Planner::new();
        let plan = planner.statement_to_plan(statement);
        assert!(plan.is_ok(), "Failed to convert CREATE TRIGGER to plan: {:?}", plan.err());

        if let Ok(LogicalPlan::CreateTrigger { name, table_name, timing, events, for_each, .. }) = plan {
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
        assert!(plan.is_ok(), "Failed to convert CREATE TRIGGER to plan: {:?}", plan.err());

        if let Ok(LogicalPlan::CreateTrigger { name, table_name, timing, events, .. }) = plan {
            assert_eq!(name, "update_timestamp");
            assert_eq!(table_name, "products");
            assert_eq!(timing, TriggerTiming::Before);
            assert_eq!(events.len(), 1);
            match &events[0] {
                TriggerEvent::Update(None) => {},
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
        assert!(plan.is_ok(), "Failed to convert CREATE TRIGGER to plan: {:?}", plan.err());

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
        assert!(plan.is_ok(), "Failed to convert CREATE TRIGGER to plan: {:?}", plan.err());

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
        assert!(plan.is_ok(), "Failed to convert CREATE TRIGGER to plan: {:?}", plan.err());

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
        assert!(plan.is_ok(), "Failed to convert CREATE OR REPLACE TRIGGER to plan: {:?}", plan.err());

        if let Ok(LogicalPlan::CreateTrigger { name, if_not_exists, .. }) = plan {
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
        assert!(plan.is_ok(), "Failed to convert CREATE TRIGGER with multiple events to plan: {:?}", plan.err());

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
        let statement = parser.parse_one(
            "DROP TRIGGER audit_insert ON users"
        ).expect("Failed to parse DROP TRIGGER statement");

        let planner = Planner::new();
        let plan = planner.statement_to_plan(statement);
        assert!(plan.is_ok(), "Failed to convert DROP TRIGGER to plan: {:?}", plan.err());

        if let Ok(LogicalPlan::DropTrigger { name, table_name, if_exists }) = plan {
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
        let statement = parser.parse_one(
            "DROP TRIGGER IF EXISTS old_trigger ON products"
        ).expect("Failed to parse DROP TRIGGER IF EXISTS statement");

        let planner = Planner::new();
        let plan = planner.statement_to_plan(statement);
        assert!(plan.is_ok(), "Failed to convert DROP TRIGGER IF EXISTS to plan: {:?}", plan.err());

        if let Ok(LogicalPlan::DropTrigger { name, table_name, if_exists }) = plan {
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
        let statement = parser.parse_one(
            "DROP TRIGGER legacy_trigger ON orders CASCADE"
        ).expect("Failed to parse DROP TRIGGER CASCADE statement");

        let planner = Planner::new();
        let plan = planner.statement_to_plan(statement);
        assert!(plan.is_ok(), "Failed to convert DROP TRIGGER CASCADE to plan: {:?}", plan.err());

        if let Ok(LogicalPlan::DropTrigger { name, .. }) = plan {
            assert_eq!(name, "legacy_trigger");
        } else {
            panic!("Expected DropTrigger plan");
        }
    }
}
