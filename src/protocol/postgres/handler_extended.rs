//! Extended query protocol handler additions
//!
//! This module extends the PgConnectionHandler with full extended query protocol support.

use crate::{Result, Error, Value};
use super::handler::PgConnectionHandler;
use super::prepared::{PreparedStatement, Portal, PortalState, decode_parameter, substitute_parameters};
use super::messages::{BackendMessage, FieldDescription};
use tokio::io::{AsyncRead, AsyncWrite};

impl<S: AsyncRead + AsyncWrite + Unpin> PgConnectionHandler<S> {
    #[allow(clippy::similar_names)]
    /// Handle Parse message (extended protocol) with full implementation
    pub async fn handle_parse_extended(
        &mut self,
        statement_name: String,
        query: String,
        param_types: Vec<i32>,
    ) -> Result<()> {
        tracing::debug!("Parse statement '{}': {}", statement_name, query);

        // Parse and validate query syntax
        let parser = crate::sql::Parser::new();
        let statement = parser.parse_one(&query)?;

        // If param_types is empty, infer parameter count from query placeholders ($1, $2, etc)
        // Use OID 0 (unknown) which tells the client to use its preferred type for each parameter
        let inferred_param_types = if param_types.is_empty() {
            let param_count = Self::count_parameters(&query);
            if param_count > 0 {
                tracing::debug!("Inferred {} parameters, using unknown type (OID 0)", param_count);
                vec![0i32; param_count] // OID 0 = unknown/unspecified
            } else {
                vec![]
            }
        } else {
            param_types.clone()
        };

        // Derive result schema from query plan (gracefully handle failures).
        // If planning fails (e.g., table doesn't exist, unknown function), we
        // still create the prepared statement. For `SELECT` specifically,
        // fall back to a best-effort schema derived straight from the
        // projection list so Describe never sends `NoData` for a query that
        // actually returns rows — SQLAlchemy's psycopg dialect calls
        // `_row_as_tuple_getter` on that metadata and raises
        // NotImplementedError if it's missing.
        // Catalog-first derivation: if this is a SELECT against
        // pg_catalog / information_schema, get the schema from our
        // catalog emulator so Describe returns the right
        // RowDescription. Otherwise fall through to plan-based
        // derivation + the AST-based fallback.
        let catalog_schema = self.catalog
            .handle_query(&query)
            .ok()
            .flatten()
            .map(|(schema, _)| schema);
        let result_schema = if let Some(s) = catalog_schema {
            Some(s)
        } else {
            match self.derive_result_schema(&statement) {
                Ok(schema) => schema,
                Err(e) => {
                    tracing::debug!("Schema derivation failed (falling back to AST): {}", e);
                    Self::synthesise_schema_from_ast(&statement)
                }
            }
        };

        tracing::debug!(
            "Derived schema for '{}': {} columns, {} parameters",
            statement_name,
            result_schema.as_ref().map_or(0, |s| s.columns.len()),
            inferred_param_types.len()
        );

        // Create prepared statement with cached plan for faster execution
        let prepared = PreparedStatement {
            name: statement_name.clone(),
            query: query.clone(),
            param_types: inferred_param_types,
            result_schema,
            cached_plan: None, // Plan will be parsed and cached on first execute
        };

        // Store in prepared statement manager
        self.prepared_statements.store_statement(prepared)?;

        tracing::debug!("Stored prepared statement '{}'", statement_name);

        // Send ParseComplete
        self.send_message(BackendMessage::ParseComplete).await?;
        Ok(())
    }

    /// Handle Bind message (extended protocol) with full implementation
    pub async fn handle_bind_extended(
        &mut self,
        portal_name: String,
        statement_name: String,
        param_formats: Vec<i16>,
        params: Vec<Option<Vec<u8>>>,
        result_formats: Vec<i16>,
    ) -> Result<()> {
        tracing::debug!("Bind portal '{}' to statement '{}'", portal_name, statement_name);

        // Get prepared statement
        let statement = self.prepared_statements.get_statement(&statement_name)?
            .ok_or_else(|| Error::query_execution(format!(
                "Prepared statement '{}' not found", statement_name
            )))?;

        // Validate parameter count
        if !params.is_empty() && !statement.param_types.is_empty()
            && params.len() != statement.param_types.len() {
            return Err(Error::query_execution(format!(
                "Parameter count mismatch: expected {}, got {}",
                statement.param_types.len(),
                params.len()
            )));
        }

        // Create portal
        let portal = Portal {
            name: portal_name.clone(),
            statement_name: statement_name.clone(),
            params,
            param_formats,
            result_formats,
            state: PortalState::Ready,
        };

        // Store portal
        self.prepared_statements.store_portal(portal)?;

        tracing::debug!("Created portal '{}' bound to '{}'", portal_name, statement_name);

        // Send BindComplete
        self.send_message(BackendMessage::BindComplete).await?;
        Ok(())
    }

    /// Handle Execute message (extended protocol) with full implementation
    // SAFETY: param_formats[i] and param_types[i] are guarded by i < len checks.
    #[allow(clippy::indexing_slicing)]
    pub async fn handle_execute_extended(
        &mut self,
        portal_name: String,
        max_rows: i32,
    ) -> Result<()> {
        tracing::debug!("Execute portal '{}' (max_rows: {})", portal_name, max_rows);

        // Get portal
        let portal = self.prepared_statements.get_portal(&portal_name)?
            .ok_or_else(|| Error::query_execution(format!(
                "Portal '{}' not found", portal_name
            )))?;

        // Check portal state
        if portal.state == PortalState::Complete {
            return Err(Error::query_execution(format!(
                "Portal '{}' already complete", portal_name
            )));
        }

        // Get statement
        let statement = self.prepared_statements.get_statement(&portal.statement_name)?
            .ok_or_else(|| Error::query_execution(format!(
                "Statement '{}' not found", portal.statement_name
            )))?;

        // Convert parameters from wire format to Value
        let mut param_values = Vec::new();
        for (i, param_data) in portal.params.iter().enumerate() {
            if let Some(data) = param_data {
                let format = if i < portal.param_formats.len() {
                    portal.param_formats[i]
                } else {
                    0 // Default to text format
                };

                let type_oid = if i < statement.param_types.len() {
                    statement.param_types[i]
                } else {
                    25 // Default to TEXT
                };

                let value = decode_parameter(data, format, type_oid)?;
                param_values.push(value);
            } else {
                param_values.push(Value::Null);
            }
        }

        // Substitute parameters into query
        let executed_query = if param_values.is_empty() {
            statement.query.clone()
        } else {
            substitute_parameters(&statement.query, &param_values)?
        };

        tracing::debug!("Executing query: {}", executed_query);

        // Execute query
        let is_select = {
            let t = executed_query.trim();
            t.len() >= 6 && t.as_bytes()[..6].eq_ignore_ascii_case(b"SELECT")
        };

        if is_select {
            // Catalog fast path — `pg_catalog.pg_type` and friends must
            // resolve on the extended query protocol too, not just on
            // simple-Q. `postgres-js`, `pg`, `psycopg` all do their
            // connect-time type introspection through Parse / Bind /
            // Execute; without this route, every driver gets a
            // spurious `Table 'pg_catalog.pg_type' does not exist`.
            if let Some(catalog_result) = self.catalog.handle_query(&executed_query)? {
                // Mark the portal complete and emit DataRows + CommandComplete
                // directly against the catalog-emulated result.
                self.prepared_statements.update_portal_state(
                    &portal_name,
                    PortalState::Complete,
                )?;
                for row in &catalog_result.1 {
                    let values = super::handler::tuple_to_pg_values(row);
                    self.send_message(BackendMessage::DataRow { values }).await?;
                }
                let tag = format!("SELECT {}", catalog_result.1.len());
                self.send_command_complete(&tag).await?;
                return Ok(());
            }

            // SELECT query - return result set
            let results = self.database.query(&executed_query, &[])?;

            // Handle max_rows limit
            let results_to_send = if max_rows > 0 {
                let max = max_rows as usize;
                if results.len() > max {
                    // Portal suspended - cache remaining results
                    let (to_send, remaining) = results.split_at(max);
                    self.prepared_statements.update_portal_state(
                        &portal_name,
                        PortalState::Suspended {
                            rows_returned: max,
                            cached_results: Some(remaining.to_vec()),
                        },
                    )?;
                    to_send.to_vec()
                } else {
                    self.prepared_statements.update_portal_state(
                        &portal_name,
                        PortalState::Complete,
                    )?;
                    results
                }
            } else {
                self.prepared_statements.update_portal_state(
                    &portal_name,
                    PortalState::Complete,
                )?;
                results
            };

            // In extended protocol, RowDescription was already sent during Describe.
            // Execute only sends DataRows and CommandComplete.

            // Send DataRows
            for row in &results_to_send {
                let values = super::handler::tuple_to_pg_values(row);
                self.send_message(BackendMessage::DataRow { values }).await?;
            }

            // Send CommandComplete
            let tag = format!("SELECT {}", results_to_send.len());
            self.send_command_complete(&tag).await?;
        } else {
            // INSERT / UPDATE / DELETE with RETURNING must route through
            // `execute_returning` — otherwise the tuples are dropped and
            // Drizzle's `.returning()` / psycopg's fetchone() see no rows
            // even though the write succeeded. Detection mirrors
            // `handle_query`'s `is_dml_returning` check.
            let upper = executed_query.to_uppercase();
            let is_dml_returning = upper.contains("RETURNING")
                && (super::handler::starts_with_icase(executed_query.trim(), "INSERT")
                    || super::handler::starts_with_icase(executed_query.trim(), "UPDATE")
                    || super::handler::starts_with_icase(executed_query.trim(), "DELETE"));
            if is_dml_returning {
                let (affected, tuples) = self.database.execute_returning(&executed_query)?;
                self.prepared_statements.update_portal_state(&portal_name, PortalState::Complete)?;
                // RowDescription was already sent during Describe; Execute
                // only emits DataRows + CommandComplete.
                for row in &tuples {
                    let values = super::handler::tuple_to_pg_values(row);
                    self.send_message(BackendMessage::DataRow { values }).await?;
                }
                let tag = if super::handler::starts_with_icase(executed_query.trim(), "INSERT") {
                    format!("INSERT 0 {}", affected)
                } else if super::handler::starts_with_icase(executed_query.trim(), "UPDATE") {
                    format!("UPDATE {}", affected)
                } else {
                    format!("DELETE {}", affected)
                };
                self.send_command_complete(&tag).await?;
                return Ok(());
            }

            // Non-SELECT query
            let affected = self.database.execute(&executed_query)?;
            let tag = self.get_command_tag(&executed_query, affected);
            self.send_command_complete(&tag).await?;

            // Mark portal complete
            self.prepared_statements.update_portal_state(
                &portal_name,
                PortalState::Complete,
            )?;
        }

        Ok(())
    }

    /// Handle Describe message (extended protocol) with full implementation
    pub async fn handle_describe_extended(
        &mut self,
        target: super::messages::DescribeTarget,
        name: String,
    ) -> Result<()> {
        tracing::debug!("Describe {:?} '{}'", target, name);

        use super::messages::DescribeTarget;

        match target {
            DescribeTarget::Statement => {
                // Describe a prepared statement
                let statement = self.prepared_statements.get_statement(&name)?
                    .ok_or_else(|| Error::query_execution(format!(
                        "Statement '{}' not found", name
                    )))?;

                // Send ParameterDescription
                if !statement.param_types.is_empty() {
                    self.send_message(BackendMessage::ParameterDescription {
                        param_types: statement.param_types.clone(),
                    }).await?;
                } else {
                    self.send_message(BackendMessage::ParameterDescription {
                        param_types: vec![],
                    }).await?;
                }

                // Send RowDescription or NoData based on derived schema
                if let Some(schema) = &statement.result_schema {
                    let fields: Vec<FieldDescription> = schema.columns.iter().map(|col| {
                        FieldDescription {
                            name: col.name.clone(),
                            table_oid: 0,
                            column_attr_num: 0,
                            data_type_oid: super::handler::datatype_to_oid(&col.data_type),
                            data_type_size: super::handler::datatype_to_size(&col.data_type),
                            type_modifier: -1,
                            format_code: 0,
                        }
                    }).collect();
                    self.send_message(BackendMessage::RowDescription { fields }).await?;
                } else {
                    // Statement doesn't return results (INSERT, UPDATE, DELETE, DDL)
                    self.send_message(BackendMessage::NoData).await?;
                }
            }
            DescribeTarget::Portal => {
                // Describe a portal
                let portal = self.prepared_statements.get_portal(&name)?
                    .ok_or_else(|| Error::query_execution(format!(
                        "Portal '{}' not found", name
                    )))?;

                let statement = self.prepared_statements.get_statement(&portal.statement_name)?
                    .ok_or_else(|| Error::query_execution(format!(
                        "Statement '{}' not found", portal.statement_name
                    )))?;

                // Send RowDescription or NoData
                if let Some(schema) = &statement.result_schema {
                    let fields: Vec<FieldDescription> = schema.columns.iter().map(|col| {
                        FieldDescription {
                            name: col.name.clone(),
                            table_oid: 0,
                            column_attr_num: 0,
                            data_type_oid: super::handler::datatype_to_oid(&col.data_type),
                            data_type_size: super::handler::datatype_to_size(&col.data_type),
                            type_modifier: -1,
                            format_code: 0,
                        }
                    }).collect();
                    self.send_message(BackendMessage::RowDescription { fields }).await?;
                } else {
                    self.send_message(BackendMessage::NoData).await?;
                }
            }
        }

        Ok(())
    }

    /// Handle Close message (close statement or portal)
    pub async fn handle_close(
        &mut self,
        target: super::messages::DescribeTarget,
        name: String,
    ) -> Result<()> {
        tracing::debug!("Close {:?} '{}'", target, name);

        use super::messages::DescribeTarget;

        match target {
            DescribeTarget::Statement => {
                self.prepared_statements.remove_statement(&name)?;
            }
            DescribeTarget::Portal => {
                self.prepared_statements.remove_portal(&name)?;
            }
        }

        self.send_message(BackendMessage::CloseComplete).await?;
        Ok(())
    }

    /// Derive result schema from SQL statement for prepared statements
    ///
    /// This method analyzes the SQL statement and extracts the result schema
    /// that will be returned when the statement is executed. It uses the query
    /// planner to build a logical plan and extract schema information.
    ///
    /// # Returns
    ///
    /// - `Some(Schema)` for queries that return results (SELECT, RETURNING)
    /// - `None` for queries that don't return results (INSERT, UPDATE, DELETE, DDL)
    fn derive_result_schema(&self, statement: &sqlparser::ast::Statement) -> Result<Option<crate::Schema>> {
        use sqlparser::ast::Statement;

        // Only derive schema for queries that return results
        match statement {
            Statement::Query(_) => {
                // SELECT and other queries - derive schema from logical plan
                let catalog = self.database.storage.catalog();
                let planner = crate::sql::planner::Planner::with_catalog(&catalog);

                // Convert statement to logical plan
                let logical_plan = planner.statement_to_plan(statement.clone())?;

                // Extract schema from the logical plan
                let schema_arc = logical_plan.schema();
                let schema = (*schema_arc).clone();

                // Only return schema if it has columns
                if schema.columns.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(schema))
                }
            }
            // INSERT, UPDATE, DELETE - check for RETURNING clause
            Statement::Insert(ref insert) => {
                if insert.returning.is_some() {
                    // Has RETURNING clause - derive schema
                    let catalog = self.database.storage.catalog();
                    let planner = crate::sql::planner::Planner::with_catalog(&catalog);
                    let plan = planner.statement_to_plan(statement.clone())?;
                    if let crate::sql::LogicalPlan::Insert { table_name, returning: Some(ref items), .. }
                        | crate::sql::LogicalPlan::InsertSelect { table_name, returning: Some(ref items), .. } = plan {
                        let table_schema = catalog.get_table_schema(&table_name)?;
                        Ok(Some(crate::EmbeddedDatabase::returning_schema(&table_schema, items)))
                    } else {
                        Ok(None)
                    }
                } else {
                    Ok(None)
                }
            }
            Statement::Update { ref returning, .. } => {
                if returning.is_some() {
                    let catalog = self.database.storage.catalog();
                    let planner = crate::sql::planner::Planner::with_catalog(&catalog);
                    let plan = planner.statement_to_plan(statement.clone())?;
                    if let crate::sql::LogicalPlan::Update { table_name, returning: Some(ref items), .. } = plan {
                        let table_schema = catalog.get_table_schema(&table_name)?;
                        Ok(Some(crate::EmbeddedDatabase::returning_schema(&table_schema, items)))
                    } else {
                        Ok(None)
                    }
                } else {
                    Ok(None)
                }
            }
            Statement::Delete(ref del) => {
                if del.returning.is_some() {
                    let catalog = self.database.storage.catalog();
                    let planner = crate::sql::planner::Planner::with_catalog(&catalog);
                    let plan = planner.statement_to_plan(statement.clone())?;
                    if let crate::sql::LogicalPlan::Delete { table_name, returning: Some(ref items), .. } = plan {
                        let table_schema = catalog.get_table_schema(&table_name)?;
                        Ok(Some(crate::EmbeddedDatabase::returning_schema(&table_schema, items)))
                    } else {
                        Ok(None)
                    }
                } else {
                    Ok(None)
                }
            }
            // DDL statements (CREATE, DROP, ALTER) don't return results
            Statement::CreateTable(_) |
            Statement::Drop { .. } |
            Statement::CreateIndex(_) |
            Statement::AlterTable { .. } |
            Statement::Truncate { .. } => {
                Ok(None)
            }
            // For any other statement types, assume no result schema
            _ => Ok(None),
        }
    }

    /// Best-effort schema extraction straight from the `Statement::Query`
    /// projection list, used as a fallback when full planner-based schema
    /// derivation fails. Returns `None` for non-queries or when no
    /// projection names can be extracted.
    ///
    /// All columns are typed as `Text` because we don't know the real
    /// types without planning; psycopg accepts that and will coerce on
    /// the client side. The goal is purely to keep `Describe` from
    /// degrading to `NoData` for a SELECT, which breaks SQLAlchemy's
    /// `_row_as_tuple_getter`.
    fn synthesise_schema_from_ast(statement: &sqlparser::ast::Statement) -> Option<crate::Schema> {
        use sqlparser::ast::{Statement, SetExpr, SelectItem};

        let query = if let Statement::Query(q) = statement { q } else { return None; };
        let select = if let SetExpr::Select(s) = &*query.body { s } else { return None; };

        let columns: Vec<crate::Column> = select.projection.iter().enumerate().map(|(i, item)| {
            let name = match item {
                SelectItem::UnnamedExpr(expr) => Self::expr_column_label(expr).unwrap_or_else(|| format!("column{}", i + 1)),
                SelectItem::ExprWithAlias { alias, .. } => alias.value.clone(),
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => format!("column{}", i + 1),
            };
            crate::Column::new(name, crate::DataType::Text)
        }).collect();

        if columns.is_empty() { None } else { Some(crate::Schema::new(columns)) }
    }

    /// Pick a reasonable label for an unaliased SELECT expression — column
    /// name for `Expr::Identifier`, rightmost part of a compound
    /// identifier, function name for a call, or None otherwise.
    fn expr_column_label(expr: &sqlparser::ast::Expr) -> Option<String> {
        use sqlparser::ast::Expr;
        match expr {
            Expr::Identifier(ident) => Some(ident.value.clone()),
            Expr::CompoundIdentifier(parts) => parts.last().map(|p| p.value.clone()),
            Expr::Function(f) => f.name.to_string().split('.').last().map(|s| s.to_string()),
            _ => None,
        }
    }

    /// Count the number of $N placeholders in a SQL query
    /// Returns the highest parameter number found (e.g., "$7" means 7 parameters)
    fn count_parameters(query: &str) -> usize {
        let mut max_param = 0;
        let mut chars = query.chars().peekable();

        while let Some(c) = chars.next() {
            if c == '$' {
                // Collect digits following the $
                let mut num_str = String::new();
                while let Some(&digit) = chars.peek() {
                    if digit.is_ascii_digit() {
                        num_str.push(digit);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if !num_str.is_empty() {
                    if let Ok(num) = num_str.parse::<usize>() {
                        max_param = max_param.max(num);
                    }
                }
            }
        }

        max_param
    }
}

