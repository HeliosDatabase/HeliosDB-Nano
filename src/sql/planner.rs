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
use super::phase3::materialized_views::MaterializedViewParser;

/// Query planner
pub struct Planner<'a> {
    catalog: Option<&'a Catalog<'a>>,
    /// Original SQL for time-travel AS OF parsing
    original_sql: Option<String>,
}

impl<'a> Planner<'a> {
    /// Create a new planner without catalog (for testing)
    pub fn new() -> Self {
        Self {
            catalog: None,
            original_sql: None,
        }
    }

    /// Create a new planner with catalog access
    pub fn with_catalog(catalog: &'a Catalog<'_>) -> Self {
        Self {
            catalog: Some(catalog),
            original_sql: None,
        }
    }

    /// Set the original SQL for time-travel AS OF parsing
    pub fn with_sql(mut self, sql: String) -> Self {
        self.original_sql = Some(sql);
        self
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
                    let len_str = &upper[start + 1..end];
                    if let Ok(len) = len_str.parse::<usize>() {
                        return Ok(DataType::Varchar(Some(len)));
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
                    let dim_str = &upper[start + 1..end];
                    if let Ok(dim) = dim_str.parse::<usize>() {
                        return Ok(DataType::Vector(dim));
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

    /// Convert a SQL statement to a logical plan
    pub fn statement_to_plan(&self, statement: Statement) -> Result<LogicalPlan> {
        match statement {
            Statement::Query(query) => self.query_to_plan(*query),
            Statement::Insert(insert) => {
                // Extract fields from Insert struct for v0.53 API
                let table_name = insert.table_name.to_string();
                let columns = insert.columns;
                let source = insert.source.ok_or_else(||
                    Error::query_execution("INSERT statement missing source query")
                )?;
                // Extract RETURNING clause if present
                let returning = insert.returning.as_ref().map(|ret_items| {
                    ret_items.iter()
                        .filter_map(|item| {
                            // Extract column name from SelectItem
                            match item {
                                sqlparser::ast::SelectItem::UnnamedExpr(sqlparser::ast::Expr::Identifier(ident)) => {
                                    Some(ident.value.clone())
                                }
                                sqlparser::ast::SelectItem::ExprWithAlias { expr, alias } => {
                                    // For aliased expressions, use the alias
                                    Some(alias.value.clone())
                                }
                                _ => None,
                            }
                        })
                        .collect::<Vec<_>>()
                });
                self.insert_to_plan(table_name, columns, source, returning)
            }
            Statement::CreateTable(create_table) => {
                // Extract fields from CreateTable struct for v0.53 API
                let name = create_table.name.to_string();
                let columns = create_table.columns;
                let if_not_exists = create_table.if_not_exists;
                let constraints = create_table.constraints;
                let with_options = create_table.with_options;
                self.create_table_to_plan(name, columns, if_not_exists, constraints, with_options)
            }
            Statement::Drop { names, if_exists, .. } => {
                if names.len() != 1 {
                    return Err(Error::query_execution("Multiple table drops not supported"));
                }
                Ok(LogicalPlan::DropTable {
                    name: names[0].to_string(),
                    if_exists,
                })
            }
            Statement::Truncate { table_names, .. } => {
                if table_names.is_empty() {
                    return Err(Error::query_execution("TRUNCATE requires a table name"));
                }
                if table_names.len() > 1 {
                    return Err(Error::query_execution("Multiple table TRUNCATE not supported"));
                }
                Ok(LogicalPlan::Truncate {
                    table_name: table_names[0].to_string(),
                })
            }
            Statement::Update { table, assignments, selection, returning, .. } => {
                // Extract RETURNING clause if present
                let returning_cols = returning.as_ref().map(|ret_items| {
                    ret_items.iter()
                        .filter_map(|item| {
                            // Extract column name from SelectItem
                            match item {
                                sqlparser::ast::SelectItem::UnnamedExpr(sqlparser::ast::Expr::Identifier(ident)) => {
                                    Some(ident.value.clone())
                                }
                                sqlparser::ast::SelectItem::ExprWithAlias { expr, alias } => {
                                    // For aliased expressions, use the alias
                                    Some(alias.value.clone())
                                }
                                _ => None,
                            }
                        })
                        .collect::<Vec<_>>()
                });
                self.update_to_plan(table, assignments, selection, returning_cols)
            }
            Statement::Delete(delete_stmt) => {
                // Extract table from FromTable enum
                let table = match &delete_stmt.from {
                    sqlparser::ast::FromTable::WithFromKeyword(tables) => {
                        if tables.len() != 1 {
                            return Err(Error::query_execution("Multi-table DELETE not supported"));
                        }
                        tables[0].clone()
                    }
                    sqlparser::ast::FromTable::WithoutKeyword(tables) => {
                        if tables.len() != 1 {
                            return Err(Error::query_execution("Multi-table DELETE not supported"));
                        }
                        tables[0].clone()
                    }
                };
                // Extract RETURNING clause if present
                let returning = delete_stmt.returning.as_ref().map(|ret_items| {
                    ret_items.iter()
                        .filter_map(|item| {
                            // Extract column name from SelectItem
                            match item {
                                sqlparser::ast::SelectItem::UnnamedExpr(sqlparser::ast::Expr::Identifier(ident)) => {
                                    Some(ident.value.clone())
                                }
                                sqlparser::ast::SelectItem::ExprWithAlias { expr, alias } => {
                                    // For aliased expressions, use the alias
                                    Some(alias.value.clone())
                                }
                                _ => None,
                            }
                        })
                        .collect::<Vec<_>>()
                });
                self.delete_to_plan(table, delete_stmt.selection.clone(), returning)
            }
            Statement::CreateIndex(create_index) => {
                // Extract index name
                let index_name = create_index.name.as_ref()
                    .ok_or_else(|| Error::query_execution("Index name is required"))?
                    .to_string();

                // Extract table name
                let table = create_index.table_name.to_string();

                // Extract column name (we only support single-column indexes for now)
                if create_index.columns.is_empty() {
                    return Err(Error::query_execution("At least one column required for index"));
                }
                if create_index.columns.len() > 1 {
                    return Err(Error::query_execution("Multi-column vector indexes not yet supported"));
                }

                let column = match &create_index.columns[0].expr {
                    Expr::Identifier(ident) => ident.value.clone(),
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
                self.alter_table_to_plan(name.to_string(), operations)
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
                or_replace: _,
                if_not_exists,
                options,
                ..
            } => {
                // Check if this is a materialized view
                if materialized {
                    // Convert the query to a logical plan
                    let query_plan = self.query_to_plan(*query)?;

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
                    // Regular views not yet supported
                    Err(Error::query_execution(
                        "Non-materialized views are not yet supported. Use CREATE MATERIALIZED VIEW instead."
                    ))
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
            Statement::Rollback { .. } => Ok(LogicalPlan::Rollback),
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
        let cte_plans = if let Some(with_clause) = query.with {
            let mut ctes = Vec::new();
            for cte in with_clause.cte_tables {
                // Each CTE is a CteDefinition - parse the subquery
                // Convert CTE query to logical plan
                let cte_plan = self.query_to_plan(*cte.query)?;
                ctes.push((cte.alias.name.to_string(), Box::new(cte_plan)));
            }
            ctes
        } else {
            Vec::new()
        };

        // Convert the body
        let mut plan = self.set_expr_to_plan(*query.body)?;

        // Handle ORDER BY
        if let Some(order_by) = &query.order_by {
            let exprs: Result<Vec<_>> = order_by.exprs.iter()
                .map(|order_by_expr| self.expr_to_logical(&order_by_expr.expr))
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
            let limit = match query.limit {
                Some(expr) => self.expr_to_usize(&expr)?,
                None => usize::MAX,
            };
            let offset = match &query.offset {
                Some(offset) => self.expr_to_usize(&offset.value)?,
                None => 0,
            };

            plan = LogicalPlan::Limit {
                input: Box::new(plan),
                limit,
                offset,
            };
        }

        // If there are CTEs, wrap the plan in a With logical plan
        if !cte_plans.is_empty() {
            plan = LogicalPlan::With {
                ctes: cte_plans,
                query: Box::new(plan),
            };
        }

        Ok(plan)
    }

    /// Convert a SetExpr to a logical plan
    fn set_expr_to_plan(&self, set_expr: SetExpr) -> Result<LogicalPlan> {
        match set_expr {
            SetExpr::Select(select) => self.select_to_plan(*select),
            _ => Err(Error::query_execution("UNION/INTERSECT/EXCEPT not yet supported")),
        }
    }

    /// Convert a SELECT to a logical plan
    fn select_to_plan(&self, select: Select) -> Result<LogicalPlan> {
        // Start with FROM clause
        let mut plan = if select.from.is_empty() {
            // SELECT without FROM (like SELECT 1+1)
            // Use DualScan as the input - it produces a single row with no columns
            LogicalPlan::DualScan
        } else if select.from.len() > 1 {
            return Err(Error::query_execution("Multiple FROM tables require explicit JOIN"));
        } else {
            self.table_with_joins_to_plan(&select.from[0])?
        };

        // Add WHERE clause as Filter
        if let Some(predicate) = select.selection {
            let filter_expr = self.expr_to_logical(&predicate)?;
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                predicate: filter_expr,
            };
        }

        // Check if we have aggregate functions (even without GROUP BY)
        let aggr_exprs = self.extract_aggregate_exprs(&select.projection)?;
        let has_aggregates = !aggr_exprs.is_empty();

        // Handle GROUP BY or implicit aggregation (when aggregates are present without GROUP BY)
        if has_aggregates {
            let group_by = if let sqlparser::ast::GroupByExpr::Expressions(group_by_exprs, _) = &select.group_by {
                if !group_by_exprs.is_empty() {
                    let group_by: Result<Vec<_>> = group_by_exprs.iter()
                        .map(|expr| self.expr_to_logical(expr))
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
                group_by,
                aggr_exprs,
                having,
            };

            // The Aggregate operator outputs GROUP BY columns followed by aggregate results
            // This matches the SELECT clause, so we don't need a Project on top
        } else {
            // No aggregates - just add projection (SELECT columns)
            let (exprs, aliases) = self.select_items_to_exprs(&select.projection, &plan)?;
            let distinct = select.distinct.is_some();
            plan = LogicalPlan::Project {
                input: Box::new(plan),
                exprs,
                aliases,
                distinct,
            };
        }

        Ok(plan)
    }

    /// Convert TableWithJoins to a plan
    fn table_with_joins_to_plan(&self, table_with_joins: &TableWithJoins) -> Result<LogicalPlan> {
        // Start with the main table
        let mut plan = self.table_factor_to_plan(&table_with_joins.relation)?;

        // Process joins
        for join in &table_with_joins.joins {
            let right = self.table_factor_to_plan(&join.relation)?;

            let join_type = match &join.join_operator {
                JoinOperator::Inner(_) => JoinType::Inner,
                JoinOperator::LeftOuter(_) => JoinType::Left,
                JoinOperator::RightOuter(_) => JoinType::Right,
                JoinOperator::FullOuter(_) => JoinType::Full,
                JoinOperator::CrossJoin => JoinType::Cross,
                _ => return Err(Error::query_execution("Join type not supported")),
            };

            let on = match &join.join_operator {
                JoinOperator::Inner(JoinConstraint::On(expr))
                | JoinOperator::LeftOuter(JoinConstraint::On(expr))
                | JoinOperator::RightOuter(JoinConstraint::On(expr))
                | JoinOperator::FullOuter(JoinConstraint::On(expr)) => {
                    Some(self.expr_to_logical(expr)?)
                }
                _ => None,
            };

            plan = LogicalPlan::Join {
                left: Box::new(plan),
                right: Box::new(right),
                join_type,
                on,
            };
        }

        Ok(plan)
    }

    /// Convert a TableFactor to a plan
    fn table_factor_to_plan(&self, table_factor: &TableFactor) -> Result<LogicalPlan> {
        match table_factor {
            TableFactor::Table { name, alias, .. } => {
                let table_name = name.to_string();

                // Check if this is a system view first (Phase 3 features)
                use crate::sql::phase3::SystemViewRegistry;
                let registry = SystemViewRegistry::new();

                if registry.is_system_view(&table_name) {
                    // This is a system view, not a regular table
                    return Ok(LogicalPlan::SystemView {
                        name: table_name,
                        args: vec![], // System views don't use arguments from table name
                    });
                }

                // Not a system view, treat as regular table
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
            _ => Err(Error::query_execution("Complex table expressions not yet supported")),
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
            let remainder = &sql[start..].trim_start();

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

            let clause = remainder[..end].trim();
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
                _ => return Err(Error::query_execution("SELECT item not supported")),
            }
        }

        Ok((exprs, aliases))
    }

    /// Extract a meaningful alias from an expression
    /// Falls back to col_{index} if no meaningful name can be extracted
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
                        if let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(inner)) = &list.args[0] {
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

    /// Extract aggregate expressions from SELECT items
    fn extract_aggregate_exprs(&self, items: &[SelectItem]) -> Result<Vec<LogicalExpr>> {
        let mut aggr_exprs = Vec::new();

        for item in items {
            match item {
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                    if let Some(aggr) = self.extract_aggregate_from_expr(expr)? {
                        aggr_exprs.push(aggr);
                    }
                }
                _ => {}
            }
        }

        Ok(aggr_exprs)
    }

    /// Extract aggregate function from an expression
    fn extract_aggregate_from_expr(&self, expr: &Expr) -> Result<Option<LogicalExpr>> {
        match expr {
            Expr::Function(func) => {
                let func_name = func.name.to_string().to_uppercase();
                let aggr_fun = match func_name.as_str() {
                    "COUNT" => Some(AggregateFunction::Count),
                    "SUM" => Some(AggregateFunction::Sum),
                    "AVG" => Some(AggregateFunction::Avg),
                    "MIN" => Some(AggregateFunction::Min),
                    "MAX" => Some(AggregateFunction::Max),
                    "JSON_AGG" => Some(AggregateFunction::JsonAgg),
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
                name: ident.value.clone(),
            }),

            Expr::CompoundIdentifier(idents) => {
                // Handle table.column references - preserve the table qualifier for JOIN disambiguation
                if idents.len() >= 2 {
                    let table_alias = idents[idents.len() - 2].value.clone();
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
                // Check if this is a parameter placeholder ($1, $2, etc.)
                match value {
                    sqlparser::ast::Value::Placeholder(placeholder) => {
                        // PostgreSQL-style parameter: $1, $2, etc.
                        if placeholder.starts_with('$') {
                            let index_str = &placeholder[1..];
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

            Expr::Function(func) => {
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

            // Array literals for vectors: '[1.0, 2.0, 3.0]'
            Expr::Array(sqlparser::ast::Array { elem, .. }) => {
                // Parse array elements into vector
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
        returning: Option<Vec<String>>,
    ) -> Result<LogicalPlan> {
        // Extract VALUES from query
        if let SetExpr::Values(values) = *source.body {
            let column_names = if columns.is_empty() {
                None
            } else {
                Some(columns.iter().map(|c| c.value.clone()).collect())
            };

            let rows: Result<Vec<Vec<LogicalExpr>>> = values.rows.iter()
                .map(|row| {
                    row.iter()
                        .map(|expr| self.expr_to_logical(expr))
                        .collect()
                })
                .collect();

            Ok(LogicalPlan::Insert {
                table_name,
                columns: column_names,
                values: rows?,
                returning,
            })
        } else {
            Err(Error::query_execution("INSERT ... SELECT not yet supported"))
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
                        let fk_constraint = TableConstraint::ForeignKey {
                            name: None,
                            columns: vec![col.name.value.clone()],
                            references_table: foreign_table.to_string(),
                            references_columns: referred_columns.iter().map(|i| i.value.clone()).collect(),
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
                    columns: columns.iter().map(|c| c.to_string()).collect(),
                    references_table: foreign_table.to_string(),
                    references_columns: referred_columns.iter().map(|c| c.to_string()).collect(),
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
        _table_name: String,
        operations: Vec<sqlparser::ast::AlterTableOperation>,
    ) -> Result<LogicalPlan> {
        // Check if operations are empty
        if operations.is_empty() {
            return Err(Error::query_execution(
                "ALTER TABLE requires an operation"
            ));
        }

        // For now, return error for unsupported operations
        Err(Error::query_execution(
            "ALTER TABLE operation not yet supported"
        ))
    }

    /// Convert SQL column definition to internal ColumnDef
    fn sql_column_def_to_column_def(&self, col: &SqlColumnDef) -> Result<ColumnDef> {
        let data_type = self.sql_data_type_to_data_type(&col.data_type)?;

        let mut not_null = false;
        let mut primary_key = false;
        let mut unique = false;
        let mut default = None;

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
                _ => {}
            }
        }

        Ok(ColumnDef {
            name: col.name.value.clone(),
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
                        if !type_modifiers.is_empty() && type_modifiers.len() == 1 {
                            let dimension = type_modifiers[0].parse::<usize>()
                                .map_err(|e| Error::query_execution(format!("Invalid vector dimension: {}", e)))?;
                            return Ok(DataType::Vector(dimension));
                        }
                        Err(Error::query_execution("VECTOR type requires dimension: VECTOR(n)"))
                    }
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

    /// Convert expression to usize (for LIMIT/OFFSET)
    fn expr_to_usize(&self, expr: &Expr) -> Result<usize> {
        match expr {
            Expr::Value(sqlparser::ast::Value::Number(n, _)) => {
                n.parse::<usize>()
                    .map_err(|e| Error::query_execution(format!("Invalid number: {}", e)))
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
        returning: Option<Vec<String>>,
    ) -> Result<LogicalPlan> {
        // Get table name
        let table_name = match &table.relation {
            sqlparser::ast::TableFactor::Table { name, .. } => name.to_string(),
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

    /// Convert DELETE statement to logical plan
    fn delete_to_plan(
        &self,
        table: sqlparser::ast::TableWithJoins,
        selection: Option<Expr>,
        returning: Option<Vec<String>>,
    ) -> Result<LogicalPlan> {
        // Get table name
        let table_name = match &table.relation {
            sqlparser::ast::TableFactor::Table { name, .. } => name.to_string(),
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
            .filter_map(|r| {
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
                        Some(TransitionTable::OldTable { alias })
                    }
                    sqlparser::ast::TriggerReferencingType::NewTable => {
                        Some(TransitionTable::NewTable { alias })
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

impl<'a> Default for Planner<'a> {
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
