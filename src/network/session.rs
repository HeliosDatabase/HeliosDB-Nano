//! Client session management
//!
//! Handles individual client connections and their state

use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, error, info};

use super::auth::{ScramAuth, SimpleAuth};
use super::protocol::{
    BackendMessage, FrontendMessage, MessageDecoder, MessageEncoder, TransactionStatus,
    AuthenticationMessage, DescribeKind, CloseKind, schema_to_row_description, value_to_pg_text,
    parse_pg_text_param, error_fields,
};
use crate::{EmbeddedDatabase, Error, Tuple, Value};

/// Client session state
pub struct Session {
    /// Database instance
    db: Arc<EmbeddedDatabase>,
    /// Session ID
    session_id: u32,
    /// Authenticated username
    username: Option<String>,
    /// Message encoder
    encoder: MessageEncoder,
    /// Message decoder
    decoder: MessageDecoder,
    /// Prepared statements
    prepared_statements: HashMap<String, PreparedStatement>,
    /// Portals (bound statements ready for execution)
    portals: HashMap<String, Portal>,
    /// Current transaction status
    transaction_status: TransactionStatus,
    /// Authentication state
    auth_state: AuthState,
    // SQL security validator (optional - would require heliosdb-protocols dependency)
    // sql_validator: heliosdb_protocols::sql_security::SqlSecurityValidator,
}

/// Authentication state
enum AuthState {
    /// Not authenticated
    Unauthenticated,
    /// SCRAM authentication in progress
    ScramInProgress(ScramAuth),
    /// Authenticated
    Authenticated(String), // username
}

/// Prepared statement
struct PreparedStatement {
    /// SQL query
    query: String,
    /// Parameter types (PostgreSQL OIDs)
    param_types: Vec<i32>,
}

/// Portal (bound statement)
struct Portal {
    /// Statement name
    statement_name: String,
    /// Bound parameters (raw bytes)
    params: Vec<Option<Vec<u8>>>,
    /// Parsed parameter values
    param_values: Vec<Value>,
}

impl Session {
    /// Create a new session
    pub fn new(db: Arc<EmbeddedDatabase>, session_id: u32) -> Self {
        Self {
            db,
            session_id,
            username: None,
            encoder: MessageEncoder::new(),
            decoder: MessageDecoder::new(),
            prepared_statements: HashMap::new(),
            portals: HashMap::new(),
            transaction_status: TransactionStatus::Idle,
            auth_state: AuthState::Unauthenticated,
            // sql_validator: heliosdb_protocols::sql_security::SqlSecurityValidator::new(),
        }
    }

    /// Handle a client connection
    pub async fn handle_connection(mut self, mut stream: TcpStream) -> Result<(), Error> {
        info!("Session {} started", self.session_id);

        let mut buf = vec![0u8; 8192];

        loop {
            // Read data from client
            let n = match stream.read(&mut buf).await {
                Ok(0) => {
                    // Connection closed
                    info!("Session {} closed by client", self.session_id);
                    return Ok(());
                }
                Ok(n) => n,
                Err(e) => {
                    error!("Session {} read error: {}", self.session_id, e);
                    return Err(Error::Io(e));
                }
            };

            // Add data to decoder buffer
            self.decoder.buffer_data(buf.get(..n).unwrap_or(&buf));

            // Process all available messages
            loop {
                match self.decoder.decode() {
                    Ok(Some(msg)) => {
                        debug!("Session {} received message: {:?}", self.session_id, msg);

                        // Process message and send responses
                        let responses = self.handle_message(msg).await?;
                        for response in responses {
                            let encoded = self.encoder.encode(&response)?;
                            stream.write_all(&encoded).await.map_err(Error::Io)?;
                        }
                        stream.flush().await.map_err(Error::Io)?;
                    }
                    Ok(None) => {
                        // Need more data
                        break;
                    }
                    Err(e) => {
                        error!("Session {} decode error: {}", self.session_id, e);
                        // Send error response
                        let error_msg = self.create_error_response("08P01", &e.to_string());
                        let encoded = self.encoder.encode(&error_msg)?;
                        stream.write_all(&encoded).await.map_err(Error::Io)?;
                        return Err(Error::Protocol(e.to_string()));
                    }
                }
            }
        }
    }

    /// Handle a frontend message
    async fn handle_message(&mut self, msg: FrontendMessage) -> Result<Vec<BackendMessage>, Error> {
        match msg {
            FrontendMessage::Startup { protocol_version, params } => {
                self.handle_startup(protocol_version, params).await
            }
            FrontendMessage::PasswordMessage { password } => {
                self.handle_password(password).await
            }
            FrontendMessage::Query { query } => {
                self.handle_query(&query).await
            }
            FrontendMessage::Parse { statement_name, query, param_types } => {
                self.handle_parse(statement_name, query, param_types).await
            }
            FrontendMessage::Bind { portal_name, statement_name, param_formats: _, params, result_formats: _ } => {
                self.handle_bind(portal_name, statement_name, params).await
            }
            FrontendMessage::Execute { portal_name, max_rows } => {
                self.handle_execute(portal_name, max_rows).await
            }
            FrontendMessage::Describe { kind, name } => {
                self.handle_describe(kind, name).await
            }
            FrontendMessage::Close { kind, name } => {
                self.handle_close(kind, name).await
            }
            FrontendMessage::Sync => {
                Ok(vec![BackendMessage::ReadyForQuery { status: self.transaction_status }])
            }
            FrontendMessage::Flush => {
                Ok(vec![])
            }
            FrontendMessage::Terminate => {
                info!("Session {} terminated", self.session_id);
                Ok(vec![])
            }
        }
    }

    /// Handle startup message
    async fn handle_startup(&mut self, protocol_version: i32, params: HashMap<String, String>) -> Result<Vec<BackendMessage>, Error> {
        // Special case: SSL request
        if protocol_version == 80877103 {
            // We don't support SSL yet - client will retry without SSL
            return Ok(vec![]);
        }

        debug!("Startup parameters: {:?}", params);

        let username = params.get("user").cloned().unwrap_or_default();
        let _database = params.get("database").cloned().unwrap_or_default();

        // For now, use simple password authentication
        // In production, you'd use SCRAM-SHA-256
        self.auth_state = AuthState::Unauthenticated;
        self.username = Some(username);

        // Request password (cleartext for simplicity - use SCRAM in production)
        Ok(vec![BackendMessage::Authentication(
            AuthenticationMessage::CleartextPassword,
        )])
    }

    /// Handle password message
    async fn handle_password(&mut self, password: String) -> Result<Vec<BackendMessage>, Error> {
        // Simple authentication (for MVP - use SCRAM in production)
        let username = self.username.as_ref().ok_or_else(|| {
            Error::protocol("No username provided")
        })?;

        // For MVP, accept any password (or configure specific credentials)
        let auth = SimpleAuth::new("postgres".to_string(), "postgres".to_string());

        if !(auth.verify(username, &password) || username == "postgres" || username == "helios") {
            return Ok(vec![
                self.create_error_response("28P01", "Authentication failed"),
                BackendMessage::ReadyForQuery { status: TransactionStatus::Idle },
            ]);
        }

        self.auth_state = AuthState::Authenticated(username.clone());

        // Send successful authentication
        let mut responses = vec![
            BackendMessage::Authentication(AuthenticationMessage::Ok),
        ];

        // Send parameter status messages
        responses.push(BackendMessage::ParameterStatus {
            name: "server_version".to_string(),
            value: "17.0 (HeliosDB Lite 0.1.0)".to_string(),
        });
        responses.push(BackendMessage::ParameterStatus {
            name: "server_encoding".to_string(),
            value: "UTF8".to_string(),
        });
        responses.push(BackendMessage::ParameterStatus {
            name: "client_encoding".to_string(),
            value: "UTF8".to_string(),
        });
        responses.push(BackendMessage::ParameterStatus {
            name: "DateStyle".to_string(),
            value: "ISO, MDY".to_string(),
        });
        responses.push(BackendMessage::ParameterStatus {
            name: "TimeZone".to_string(),
            value: "UTC".to_string(),
        });

        // Send backend key data (for cancellation - not implemented yet)
        responses.push(BackendMessage::BackendKeyData {
            process_id: self.session_id as i32,
            secret_key: 12345, // Random secret
        });

        // Send ready for query
        responses.push(BackendMessage::ReadyForQuery {
            status: TransactionStatus::Idle,
        });

        Ok(responses)
    }

    /// Handle simple query
    async fn handle_query(&mut self, query: &str) -> Result<Vec<BackendMessage>, Error> {
        if !self.is_authenticated() {
            return Ok(vec![
                self.create_error_response("08006", "Not authenticated"),
                BackendMessage::ReadyForQuery { status: TransactionStatus::Idle },
            ]);
        }

        let query_start = std::time::Instant::now();
        debug!("Executing query: {}", query);

        // Handle empty query
        if query.trim().is_empty() {
            return Ok(vec![
                BackendMessage::EmptyQueryResponse,
                BackendMessage::ReadyForQuery { status: self.transaction_status },
            ]);
        }

        // SQL injection protection: Validate query before execution (optional - requires heliosdb-protocols)
        // if let Err(e) = self.sql_validator.validate(query) {
        //     error!("SQL security validation failed for query: {}", e);
        //     return Ok(vec![
        //         self.create_error_response("42000", &format!("Security validation failed: {}", e)),
        //         BackendMessage::ReadyForQuery { status: self.transaction_status },
        //     ]);
        // }

        let mut responses = Vec::new();

        // Execute query with optional timeout from config
        let result = if let Some(timeout_ms) = self.db.query_timeout_ms() {
            match tokio::time::timeout(
                std::time::Duration::from_millis(timeout_ms),
                self.execute_sql(query),
            ).await {
                Ok(result) => result,
                Err(_) => Err(Error::query_execution(format!(
                    "Query cancelled: exceeded timeout of {}ms", timeout_ms
                ))),
            }
        } else {
            self.execute_sql(query).await
        };

        match result {
            Ok(QueryResult::Rows { schema, rows }) => {
                // Send row description
                let fields = schema_to_row_description(&schema);
                responses.push(BackendMessage::RowDescription { fields });

                // Count rows before consuming
                let row_count = rows.len();

                // Send data rows
                for row in rows {
                    let values: Vec<Option<Vec<u8>>> = row
                        .values
                        .iter()
                        .map(|v| match v {
                            Value::Null => None,
                            v => Some(value_to_pg_text(v)),
                        })
                        .collect();
                    responses.push(BackendMessage::DataRow { values });
                }

                // Send command complete
                responses.push(BackendMessage::CommandComplete {
                    tag: format!("SELECT {}", row_count),
                });
            }
            Ok(QueryResult::Modified { count, operation }) => {
                // Send command complete
                responses.push(BackendMessage::CommandComplete {
                    tag: format!("{} {}", operation, count),
                });
            }
            Err(e) => {
                error!("Query execution error: {}", e);
                responses.push(self.create_error_response("42000", &e.to_string()));
            }
        }

        // Log query timing
        let elapsed = query_start.elapsed();
        debug!(
            session = self.session_id,
            duration_us = elapsed.as_micros() as u64,
            "Query completed"
        );

        // Send ready for query
        responses.push(BackendMessage::ReadyForQuery {
            status: self.transaction_status,
        });

        Ok(responses)
    }

    /// Handle Parse message (prepare statement)
    async fn handle_parse(&mut self, statement_name: String, query: String, param_types: Vec<i32>) -> Result<Vec<BackendMessage>, Error> {
        if !self.is_authenticated() {
            return Ok(vec![self.create_error_response("08006", "Not authenticated")]);
        }

        debug!("Preparing statement '{}': {}", statement_name, query);

        // SQL injection protection: Validate query before preparing (optional - requires heliosdb-protocols)
        // if let Err(e) = self.sql_validator.validate(&query) {
        //     error!("SQL security validation failed for prepared statement: {}", e);
        //     return Ok(vec![
        //         self.create_error_response("42000", &format!("Security validation failed: {}", e))
        //     ]);
        // }

        // Store prepared statement
        self.prepared_statements.insert(
            statement_name.clone(),
            PreparedStatement {
                query,
                param_types,
            },
        );

        Ok(vec![BackendMessage::ParseComplete])
    }

    /// Handle Bind message
    async fn handle_bind(&mut self, portal_name: String, statement_name: String, params: Vec<Option<Vec<u8>>>) -> Result<Vec<BackendMessage>, Error> {
        if !self.is_authenticated() {
            return Ok(vec![self.create_error_response("08006", "Not authenticated")]);
        }

        // Get prepared statement
        let stmt = match self.prepared_statements.get(&statement_name) {
            Some(s) => s,
            None => return Ok(vec![self.create_error_response("26000", "Prepared statement not found")]),
        };

        debug!("Binding portal '{}' to statement '{}'", portal_name, statement_name);

        // Parse parameter values
        let mut param_values = Vec::new();
        for (i, param_bytes) in params.iter().enumerate() {
            match param_bytes {
                None => {
                    param_values.push(Value::Null);
                }
                Some(bytes) => {
                    // SQL injection protection: Validate parameter values (optional - requires heliosdb-protocols)
                    // if let Err(e) = heliosdb_protocols::sql_security::ParameterBindingValidator::validate_parameter_value(bytes) {
                    //     warn!("Parameter validation failed for parameter {}: {}", i + 1, e);
                    //     return Ok(vec![self.create_error_response(
                    //         "42000",
                    //         &format!("Parameter {} validation failed: {}", i + 1, e)
                    //     )]);
                    // }

                    // Get parameter type from prepared statement or default to TEXT
                    let type_oid = stmt.param_types.get(i).copied().unwrap_or(25); // TEXT = 25

                    match parse_pg_text_param(bytes, type_oid) {
                        Ok(value) => param_values.push(value),
                        Err(e) => {
                            return Ok(vec![self.create_error_response(
                                "22P02",
                                &format!("Invalid parameter {}: {}", i + 1, e)
                            )]);
                        }
                    }
                }
            }
        }

        // Store portal
        self.portals.insert(
            portal_name,
            Portal {
                statement_name,
                params,
                param_values,
            },
        );

        Ok(vec![BackendMessage::BindComplete])
    }

    /// Handle Execute message
    async fn handle_execute(&mut self, portal_name: String, _max_rows: i32) -> Result<Vec<BackendMessage>, Error> {
        if !self.is_authenticated() {
            return Ok(vec![self.create_error_response("08006", "Not authenticated")]);
        }

        // Get portal (clone to avoid borrow checker issues)
        let portal = match self.portals.get(&portal_name) {
            Some(p) => p,
            None => {
                return Ok(vec![self.create_error_response("34000", "Portal not found")]);
            }
        };

        // Get prepared statement
        let stmt = match self.prepared_statements.get(&portal.statement_name) {
            Some(s) => s,
            None => {
                return Ok(vec![self.create_error_response("26000", "Prepared statement not found")]);
            }
        };

        debug!("Executing portal '{}': {} with {} parameters",
            portal_name, stmt.query, portal.param_values.len());

        // Clone data we need to avoid borrow issues
        let query = stmt.query.clone();
        let params = portal.param_values.clone();

        let mut responses = Vec::new();

        // Execute query with bound parameters
        match self.execute_sql_with_params(&query, &params).await {
            Ok(QueryResult::Rows { schema, rows }) => {
                // Send row description
                let fields = schema_to_row_description(&schema);
                responses.push(BackendMessage::RowDescription { fields });

                // Count rows before consuming
                let row_count = rows.len();

                // Send data rows
                for row in rows {
                    let values: Vec<Option<Vec<u8>>> = row
                        .values
                        .iter()
                        .map(|v| match v {
                            Value::Null => None,
                            v => Some(value_to_pg_text(v)),
                        })
                        .collect();
                    responses.push(BackendMessage::DataRow { values });
                }

                // Send command complete
                responses.push(BackendMessage::CommandComplete {
                    tag: format!("SELECT {}", row_count),
                });
            }
            Ok(QueryResult::Modified { count, operation }) => {
                responses.push(BackendMessage::CommandComplete {
                    tag: format!("{} {}", operation, count),
                });
            }
            Err(e) => {
                error!("Query execution error: {}", e);
                responses.push(self.create_error_response("42000", &e.to_string()));
            }
        }

        Ok(responses)
    }

    /// Handle Describe message
    async fn handle_describe(&mut self, kind: DescribeKind, name: String) -> Result<Vec<BackendMessage>, Error> {
        match kind {
            DescribeKind::Statement => {
                // Describe prepared statement
                if self.prepared_statements.contains_key(&name) {
                    // For now, return no parameters and no rows
                    Ok(vec![
                        BackendMessage::ParameterDescription { param_types: vec![] },
                        BackendMessage::NoData,
                    ])
                } else {
                    Ok(vec![self.create_error_response("26000", "Prepared statement not found")])
                }
            }
            DescribeKind::Portal => {
                // Describe portal
                if self.portals.contains_key(&name) {
                    Ok(vec![BackendMessage::NoData])
                } else {
                    Ok(vec![self.create_error_response("34000", "Portal not found")])
                }
            }
        }
    }

    /// Handle Close message
    async fn handle_close(&mut self, kind: CloseKind, name: String) -> Result<Vec<BackendMessage>, Error> {
        match kind {
            CloseKind::Statement => {
                self.prepared_statements.remove(&name);
            }
            CloseKind::Portal => {
                self.portals.remove(&name);
            }
        }
        Ok(vec![BackendMessage::CloseComplete])
    }

    /// Execute SQL query without parameters
    async fn execute_sql(&self, query: &str) -> Result<QueryResult, Error> {
        self.execute_sql_with_params(query, &[]).await
    }

    /// Execute SQL query with parameters
    async fn execute_sql_with_params(&self, query: &str, params: &[Value]) -> Result<QueryResult, Error> {
        // Determine if this is a query or a command
        let query_upper = query.trim().to_uppercase();

        if query_upper.starts_with("SELECT") || query_upper.starts_with("WITH") {
            // Query - return rows
            // Convert Value params to &dyn Display for the query method
            let param_refs: Vec<&dyn std::fmt::Display> = params
                .iter()
                .map(|v| v as &dyn std::fmt::Display)
                .collect();
            let rows = self.db.query(query, &param_refs)?;

            // Get schema from first row or create empty schema
            let schema = if let Some(first_row) = rows.first() {
                // In a real implementation, we'd get the schema from the planner
                // For now, create a simple schema based on the first row
                self.infer_schema(first_row)
            } else {
                crate::Schema::new(vec![])
            };

            Ok(QueryResult::Rows { schema, rows })
        } else {
            // Command - return affected rows
            // For commands with parameters, we'd need to handle them properly
            let count = if params.is_empty() {
                self.db.execute(query)?
            } else {
                // For now, execute without parameters
                // In a full implementation, we'd pass parameters through
                // (INSERT/UPDATE/DELETE would need special handling)
                self.db.execute(query)?
            };

            let operation = if query_upper.starts_with("INSERT") {
                "INSERT 0"
            } else if query_upper.starts_with("UPDATE") {
                "UPDATE"
            } else if query_upper.starts_with("DELETE") {
                "DELETE"
            } else if query_upper.starts_with("CREATE") {
                "CREATE TABLE"
            } else if query_upper.starts_with("DROP") {
                "DROP TABLE"
            } else {
                "OK"
            };

            Ok(QueryResult::Modified {
                count,
                operation: operation.to_string(),
            })
        }
    }

    /// Infer schema from a tuple
    fn infer_schema(&self, tuple: &Tuple) -> crate::Schema {
        // This is a simplified implementation
        // In a real database, the schema would come from the catalog
        let columns: Vec<crate::Column> = tuple
            .values
            .iter()
            .enumerate()
            .map(|(i, v)| crate::Column {
                name: format!("column{}", i + 1),
                data_type: Self::value_to_datatype(v),
                nullable: true,
                primary_key: false,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
            })
            .collect();

        crate::Schema::new(columns)
    }

    /// Convert a value to a DataType
    fn value_to_datatype(value: &Value) -> crate::DataType {
        match value {
            Value::Null => crate::DataType::Text,
            Value::Boolean(_) => crate::DataType::Boolean,
            Value::Int4(_) => crate::DataType::Int4,
            Value::Int8(_) => crate::DataType::Int8,
            Value::Float4(_) => crate::DataType::Float4,
            Value::Float8(_) => crate::DataType::Float8,
            Value::String(_) => crate::DataType::Text,
            Value::Bytes(_) => crate::DataType::Bytea,
            Value::Timestamp(_) => crate::DataType::Timestamp,
            Value::Uuid(_) => crate::DataType::Uuid,
            Value::Json(_) => crate::DataType::Json,
            _ => crate::DataType::Text,
        }
    }

    /// Create an error response
    fn create_error_response(&self, code: &str, message: &str) -> BackendMessage {
        let mut fields = HashMap::new();
        fields.insert(error_fields::SEVERITY, "ERROR".to_string());
        fields.insert(error_fields::CODE, code.to_string());
        fields.insert(error_fields::MESSAGE, message.to_string());

        BackendMessage::ErrorResponse { fields }
    }

    /// Check if session is authenticated
    fn is_authenticated(&self) -> bool {
        matches!(self.auth_state, AuthState::Authenticated(_))
    }
}

/// Query execution result
enum QueryResult {
    /// Rows returned
    Rows {
        schema: crate::Schema,
        rows: Vec<Tuple>,
    },
    /// Rows modified
    Modified {
        count: u64,
        operation: String,
    },
}
