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

        // Derive result schema from query plan (gracefully handle failures)
        // If schema derivation fails (e.g., table doesn't exist), we still
        // create the prepared statement. The actual error will surface during Execute.
        let result_schema = match self.derive_result_schema(&statement) {
            Ok(schema) => schema,
            Err(e) => {
                tracing::debug!("Schema derivation failed (will fail at execute): {}", e);
                None
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
        let query_upper = executed_query.trim().to_uppercase();
        let is_select = query_upper.starts_with("SELECT");

        if is_select {
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
            // INSERT, UPDATE, DELETE with RETURNING clause would go here
            // For now, we don't support RETURNING clause schema derivation
            Statement::Insert(_) |
            Statement::Update { .. } |
            Statement::Delete(_) => {
                // Non-SELECT statements don't return result sets
                // (unless they have RETURNING clause - not implemented yet)
                Ok(None)
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

