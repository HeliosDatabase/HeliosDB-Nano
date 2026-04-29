//! PostgreSQL protocol handler
//!
//! This module implements the PostgreSQL wire protocol handler that processes
//! client messages and generates appropriate responses.

use crate::{Result, Error, EmbeddedDatabase, Tuple, Value, Schema};

/// Case-insensitive prefix check without allocating a new String.
#[inline]
pub(super) fn starts_with_icase(s: &str, prefix: &str) -> bool {
    // Safety: length is checked on the left side of &&
    #[allow(clippy::indexing_slicing)]
    {
        s.len() >= prefix.len()
            && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
    }
}
use super::messages::{
    FrontendMessage, BackendMessage, AuthenticationMessage,
    TransactionStatus, FieldDescription,
};
use super::auth::{AuthManager, AuthMethod, ScramAuthState};
use super::catalog::PgCatalog;
use super::prepared::PreparedStatementManager;
use super::ssl::SecureConnection;
use bytes::{BytesMut, BufMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;
use std::sync::Arc;

/// PostgreSQL connection handler
///
/// Uses `BufWriter` to batch all write_all() calls into a single TCP packet,
/// flushed once at the end of each response cycle (ReadyForQuery). Combined
/// with TCP_NODELAY on the socket, this minimizes both syscalls and latency.
pub struct PgConnectionHandler<S = BufWriter<TcpStream>> {
    stream: S,
    pub(super) database: Arc<EmbeddedDatabase>,
    auth_manager: Arc<AuthManager>,
    pub(super) catalog: PgCatalog,
    pub(super) prepared_statements: PreparedStatementManager,
    authenticated: bool,
    transaction_status: TransactionStatus,
    buffer: BytesMut,
    username: Option<String>,
    scram_state: Option<ScramAuthState>,
    write_buf: BytesMut,
    /// When `true`, `send_ready_for_query()` is a no-op. Used by
    /// multi-statement simple query dispatch to emit a single trailing
    /// ReadyForQuery after the whole `;`-separated batch.
    suppress_ready_for_query: bool,
}

impl PgConnectionHandler<BufWriter<TcpStream>> {
    /// Create a new connection handler with TcpStream (wrapped in BufWriter)
    pub fn new(
        stream: TcpStream,
        database: Arc<EmbeddedDatabase>,
        auth_manager: Arc<AuthManager>,
        initial_data: Option<&[u8]>,
    ) -> Self {
        let mut buffer = BytesMut::with_capacity(8192);
        if let Some(data) = initial_data {
            buffer.extend_from_slice(data);
        }

        Self {
            stream: BufWriter::new(stream),
            database: database.clone(),
            auth_manager,
            catalog: PgCatalog::with_database(database),
            prepared_statements: PreparedStatementManager::new(),
            authenticated: false,
            transaction_status: TransactionStatus::Idle,
            buffer,
            username: None,
            scram_state: None,
            write_buf: BytesMut::with_capacity(4096),
            suppress_ready_for_query: false,
        }
    }
}

#[cfg(unix)]
impl PgConnectionHandler<BufWriter<UnixStream>> {
    /// Create a new connection handler bound to a Unix domain socket stream.
    pub fn new_unix(
        stream: UnixStream,
        database: Arc<EmbeddedDatabase>,
        auth_manager: Arc<AuthManager>,
    ) -> Self {
        Self {
            stream: BufWriter::new(stream),
            database: database.clone(),
            auth_manager,
            catalog: PgCatalog::with_database(database),
            prepared_statements: PreparedStatementManager::new(),
            authenticated: false,
            transaction_status: TransactionStatus::Idle,
            buffer: BytesMut::with_capacity(8192),
            username: None,
            scram_state: None,
            write_buf: BytesMut::with_capacity(4096),
            suppress_ready_for_query: false,
        }
    }
}

/// Accept a PostgreSQL client connection over a Unix domain socket.
///
/// Used for local / embedded deployments where clients expect libpq's
/// default `/tmp/.s.PGSQL.<port>` socket path.
#[cfg(unix)]
pub async fn handle_connection_unix(
    database: Arc<EmbeddedDatabase>,
    stream: UnixStream,
    _connection_id: u32,
) -> Result<()> {
    // Trust auth over a local socket — the kernel already enforces access
    // via filesystem permissions on the socket file.
    let auth_manager = Arc::new(AuthManager::new(AuthMethod::Trust));
    let mut handler = PgConnectionHandler::new_unix(stream, database, auth_manager);
    handler.handle().await
}

impl PgConnectionHandler<BufWriter<SecureConnection<TcpStream>>> {
    /// Create a new connection handler with SecureConnection (wrapped in BufWriter)
    pub fn new_with_stream(
        stream: SecureConnection<TcpStream>,
        database: Arc<EmbeddedDatabase>,
        auth_manager: Arc<AuthManager>,
        initial_data: Option<&[u8]>,
    ) -> Self {
        let mut buffer = BytesMut::with_capacity(8192);
        if let Some(data) = initial_data {
            buffer.extend_from_slice(data);
        }

        Self {
            stream: BufWriter::new(stream),
            database: database.clone(),
            auth_manager,
            catalog: PgCatalog::with_database(database),
            prepared_statements: PreparedStatementManager::new(),
            authenticated: false,
            transaction_status: TransactionStatus::Idle,
            buffer,
            username: None,
            scram_state: None,
            write_buf: BytesMut::with_capacity(4096),
            suppress_ready_for_query: false,
        }
    }
}

impl<S> PgConnectionHandler<S>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    /// Handle connection lifecycle
    pub async fn handle(&mut self) -> Result<()> {
        tracing::info!("New PostgreSQL connection");

        // Handle startup and authentication
        if let Err(e) = self.handle_startup().await {
            tracing::error!("Startup failed: {}", e);
            let _ = self.send_error("FATAL", "08P01", &e.to_string(), None, None).await;
            return Err(e);
        }

        // Main message loop
        tracing::debug!("Entering main message loop");
        loop {
            tracing::trace!("Waiting for next message from client");
            match self.read_message().await {
                Ok(Some(msg)) => {
                    tracing::debug!("Received message: {:?}", msg);
                    if let Err(e) = self.handle_message(msg).await {
                        tracing::error!("Error handling message: {}", e);
                        self.send_error("ERROR", "XX000", &e.to_string(), None, None).await?;
                    }
                }
                Ok(None) => {
                    // Connection closed gracefully
                    tracing::info!("Client disconnected");
                    break;
                }
                Err(e) => {
                    tracing::error!("Error reading message: {}", e);
                    break;
                }
            }
        }

        Ok(())
    }

    /// Handle startup sequence
    // SAFETY: Buffer indices [0..3] are guarded by self.buffer.len() >= 4 check.
    #[allow(clippy::indexing_slicing)]
    async fn handle_startup(&mut self) -> Result<()> {
        // Check if we have initial data in the buffer (passed from server after reading 8 bytes)
        // This happens when client sends StartupMessage directly without SSLRequest
        let len_buf: [u8; 4];
        if self.buffer.len() >= 4 {
            // Length was already read by server and passed to us
            len_buf = [self.buffer[0], self.buffer[1], self.buffer[2], self.buffer[3]];
        } else {
            // Read startup message length from stream
            let mut buf = [0u8; 4];
            self.stream.read_exact(&mut buf).await
                .map_err(|e| Error::network(format!("Failed to read startup length: {}", e)))?;
            len_buf = buf;
            self.buffer.extend_from_slice(&len_buf);
        }

        let len = i32::from_be_bytes(len_buf) as usize;

        // Calculate how many bytes we still need to read
        let bytes_in_buffer = self.buffer.len();
        let bytes_needed = len.saturating_sub(bytes_in_buffer);

        if bytes_needed > 0 {
            let mut remaining_buf = vec![0u8; bytes_needed];
            self.stream.read_exact(&mut remaining_buf).await
                .map_err(|e| Error::network(format!("Failed to read startup message: {}", e)))?;
            self.buffer.extend_from_slice(&remaining_buf);
        }

        // Parse startup message
        let msg = FrontendMessage::parse_startup(&mut self.buffer)?
            .ok_or_else(|| Error::protocol("Invalid startup message"))?;

        if let FrontendMessage::Startup { protocol_version, params } = msg {
            tracing::info!("Protocol version: {}, params: {:?}", protocol_version, params);

            self.username = params.get("user").cloned();

            // Send authentication request
            match self.auth_manager.method() {
                AuthMethod::Trust => {
                    self.authenticated = true;
                    self.send_auth_ok().await?;
                }
                AuthMethod::CleartextPassword => {
                    self.send_message(BackendMessage::Authentication(
                        AuthenticationMessage::CleartextPassword
                    )).await?;
                    self.flush().await?; // Client must read challenge before responding

                    // Wait for password message
                    if let Some(FrontendMessage::PasswordMessage { password }) = self.read_message().await? {
                        let username = self.username.as_ref()
                            .ok_or_else(|| Error::authentication("No username provided"))?;

                        if self.auth_manager.verify_cleartext(username, &password)? {
                            self.authenticated = true;
                            self.send_auth_ok().await?;
                        } else {
                            return Err(Error::authentication("Invalid password"));
                        }
                    } else {
                        return Err(Error::protocol("Expected password message"));
                    }
                }
                AuthMethod::ScramSha256 => {
                    // Initiate SCRAM-SHA-256 authentication
                    self.handle_scram_authentication().await?;
                }
                _ => {
                    // Other auth methods not yet implemented
                    self.authenticated = true;
                    self.send_auth_ok().await?;
                }
            }

            // Send parameter status messages
            self.send_parameter_status(
                "server_version",
                &format!("16.0 (HeliosDB Nano {})", env!("CARGO_PKG_VERSION")),
            ).await?;
            self.send_parameter_status("server_encoding", "UTF8").await?;
            self.send_parameter_status("client_encoding", "UTF8").await?;
            self.send_parameter_status("DateStyle", "ISO, MDY").await?;
            self.send_parameter_status("TimeZone", "UTC").await?;
            self.send_parameter_status("integer_datetimes", "on").await?;

            // Send backend key data
            self.send_message(BackendMessage::BackendKeyData {
                process_id: std::process::id() as i32,
                secret_key: rand::random(),
            }).await?;

            // Send ready for query
            self.send_ready_for_query().await?;

            Ok(())
        } else {
            Err(Error::protocol("Expected startup message"))
        }
    }

    /// Read a message from the client
    // SAFETY: temp_buf[..n] slice is bounded by n from stream.read() which is <= temp_buf.len().
    #[allow(clippy::indexing_slicing)]
    async fn read_message(&mut self) -> Result<Option<FrontendMessage>> {
        // Try to parse existing buffer first
        tracing::trace!("read_message: Checking buffer, len={}", self.buffer.len());
        if let Some(msg) = FrontendMessage::parse(&mut self.buffer)? {
            tracing::trace!("read_message: Parsed message from existing buffer");
            return Ok(Some(msg));
        }

        // Read more data
        let mut temp_buf = vec![0u8; 4096];
        loop {
            tracing::trace!("read_message: Attempting to read from stream");
            match self.stream.read(&mut temp_buf).await {
                Ok(0) => {
                    tracing::debug!("read_message: EOF received (0 bytes)");
                    return Ok(None); // EOF
                }
                Ok(n) => {
                    tracing::trace!("read_message: Read {} bytes", n);
                    self.buffer.extend_from_slice(&temp_buf[..n]);
                    if let Some(msg) = FrontendMessage::parse(&mut self.buffer)? {
                        tracing::trace!("read_message: Successfully parsed message after read");
                        return Ok(Some(msg));
                    }
                    tracing::trace!("read_message: Insufficient data for complete message, continuing");
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    tracing::trace!("read_message: WouldBlock, sleeping 10ms");
                    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                }
                Err(e) => {
                    tracing::error!("read_message: Read error: {}", e);
                    return Err(Error::network(format!("Read error: {}", e)));
                }
            }
        }
    }

    /// Handle a frontend message
    async fn handle_message(&mut self, msg: FrontendMessage) -> Result<()> {
        if !self.authenticated && !matches!(msg, FrontendMessage::PasswordMessage { .. }) {
            return Err(Error::authentication("Not authenticated"));
        }

        match msg {
            FrontendMessage::Query { query } => {
                self.handle_query(&query).await?;
            }
            FrontendMessage::Parse { statement_name, query, param_types } => {
                self.handle_parse_extended(statement_name, query, param_types).await?;
            }
            FrontendMessage::Bind { portal_name, statement_name, param_formats, params, result_formats } => {
                self.handle_bind_extended(portal_name, statement_name, param_formats, params, result_formats).await?;
            }
            FrontendMessage::Execute { portal_name, max_rows } => {
                self.handle_execute_extended(portal_name, max_rows).await?;
            }
            FrontendMessage::Describe { target, name } => {
                self.handle_describe_extended(target, name).await?;
            }
            FrontendMessage::Close { target, name } => {
                self.handle_close(target, name).await?;
            }
            FrontendMessage::Sync => {
                self.send_ready_for_query().await?;
            }
            FrontendMessage::Flush => {
                // Flush tells the server to push any buffered response
                // bytes to the network now — without ending the
                // implicit extended-protocol transaction (that's what
                // Sync does) and without sending ReadyForQuery. Every
                // pipelined driver (postgres-js, pg, npgsql, etc.)
                // emits Parse+Bind+[Describe+]Execute+Flush and waits
                // for the ParseComplete / BindComplete / DataRows /
                // CommandComplete to arrive before sending Sync.
                self.flush().await?;
            }
            FrontendMessage::Terminate => {
                return Ok(());
            }
            _ => {
                tracing::warn!("Unhandled message type: {:?}", msg);
            }
        }

        Ok(())
    }

    /// Handle simple query protocol — dispatches one or more `;`-separated
    /// statements within a single `Q` message, per the PostgreSQL wire
    /// spec. Each statement produces its own response messages
    /// (CommandComplete / RowDescription+DataRow / etc.); a **single**
    /// ReadyForQuery terminates the whole batch.
    async fn handle_query(&mut self, query: &str) -> Result<()> {
        let statements = pg_split_sql_respecting_quotes(query);
        if statements.len() <= 1 {
            return self.handle_single_query(query).await;
        }

        // Multi-statement simple query. Each inner statement calls the
        // per-statement handler, which sends its own ReadyForQuery. We
        // swallow all but the last RFQ via the per-connection flag so the
        // client sees the PostgreSQL-correct single trailing RFQ.
        self.suppress_ready_for_query = true;
        let last_idx = statements.len() - 1;
        for (i, stmt) in statements.iter().enumerate() {
            if i == last_idx {
                self.suppress_ready_for_query = false;
            }
            self.handle_single_query(stmt).await?;
        }
        self.suppress_ready_for_query = false;
        Ok(())
    }

    /// Handle exactly one statement. All pre-existing `handle_query`
    /// logic lives here so the per-statement response sequence stays
    /// unchanged from v3.12's behaviour.
    // SAFETY: results[0] is guarded by !results.is_empty() check.
    #[allow(clippy::indexing_slicing)]
    async fn handle_single_query(&mut self, query: &str) -> Result<()> {
        tracing::debug!("Executing query: {}", query);

        // `DO $$ … $$` / `DO LANGUAGE … $$ … $$` blocks. sqlparser
        // doesn't support DO, so we unwrap the body and recurse over
        // the plain-SQL statements inside. No PL/pgSQL control flow
        // is expanded — this covers the common Drizzle / Prisma
        // migration backfill pattern only.
        if pg_looks_like_do_block(query.trim()) {
            return self.handle_do_block(query).await;
        }

        // Check for empty query
        if query.trim().is_empty() {
            self.send_message(BackendMessage::EmptyQueryResponse).await?;
            self.send_ready_for_query().await?;
            return Ok(());
        }

        // Handle transaction commands (case-insensitive without allocation)
        let trimmed = query.trim();
        if trimmed.eq_ignore_ascii_case("BEGIN") || starts_with_icase(trimmed, "BEGIN ") || trimmed.eq_ignore_ascii_case("START TRANSACTION") || starts_with_icase(trimmed, "START TRANSACTION ") {
            // Parse isolation level if specified
            // Supported: BEGIN [TRANSACTION] [ISOLATION LEVEL {READ UNCOMMITTED | READ COMMITTED | REPEATABLE READ | SERIALIZABLE}]
            let isolation_level = Self::parse_isolation_level(trimmed);

            // Check if already in a transaction - PostgreSQL behavior is to warn but continue
            if self.transaction_status == TransactionStatus::InTransaction {
                // Send warning like PostgreSQL does
                self.send_message(BackendMessage::NoticeResponse {
                    severity: "WARNING".to_string(),
                    code: "25001".to_string(),
                    message: "there is already a transaction in progress".to_string(),
                }).await?;
            } else {
                // Begin transaction (isolation level would be applied if storage supported it)
                // For now we just begin - isolation level is informational
                self.database.begin()?;
                self.transaction_status = TransactionStatus::InTransaction;

                // Log the isolation level for debugging
                if let Some(level) = isolation_level {
                    tracing::debug!("Transaction started with isolation level: {}", level);
                }
            }
            self.send_command_complete("BEGIN").await?;
            self.send_ready_for_query().await?;
            return Ok(());
        } else if starts_with_icase(trimmed, "SET TRANSACTION ISOLATION LEVEL ") || starts_with_icase(trimmed, "SET SESSION CHARACTERISTICS") {
            // SET TRANSACTION ISOLATION LEVEL is valid before any queries in a transaction
            let level = Self::parse_isolation_level(trimmed);
            if level.is_some() {
                self.send_command_complete("SET").await?;
            } else {
                self.send_error("ERROR", "22023", "Invalid isolation level", None, None).await?;
                return Ok(());
            }
            self.send_ready_for_query().await?;
            return Ok(());
        } else if starts_with_icase(trimmed, "SET ") && !starts_with_icase(trimmed, "SET TRANSACTION") && !starts_with_icase(trimmed, "SET SESSION") {
            // Handle generic SET commands for client compatibility (e.g., SET client_encoding = 'UTF8')
            // Accept silently - these configure client-side parameters
            self.send_command_complete("SET").await?;
            self.send_ready_for_query().await?;
            return Ok(());
        } else if starts_with_icase(trimmed, "SHOW ") {
            // Handle SHOW commands for client compatibility
            let param = trimmed[5..].trim().trim_end_matches(';').trim();
            let (col_name, value) = Self::resolve_show_parameter(param);
            let schema = Schema::new(vec![
                crate::Column::new(&col_name, crate::DataType::Text),
            ]);
            let row = Tuple::new(vec![Value::String(value)]);
            self.send_query_result(schema, vec![row]).await?;
            self.send_ready_for_query().await?;
            return Ok(());
        } else if starts_with_icase(trimmed, "PRAGMA ") || trimmed.eq_ignore_ascii_case("PRAGMA") {
            // SQLite drop-in: accept and stub PRAGMA. `table_info(t)` returns
            // SQLite-shaped column rows; everything else returns an empty
            // ack so sqlite3-driven apps don't blow up on schema-introspection
            // / connection-tuning calls that have no PG analogue.
            if let Some((name, arg)) = crate::sql::sqlite_compat::parse_pragma(trimmed) {
                match name.to_lowercase().as_str() {
                    "table_info" => {
                        let table = arg.unwrap_or_default();
                        // strip surrounding quotes / brackets
                        let table = table.trim().trim_matches(|c| c == '\'' || c == '"' || c == '`').to_string();
                        let rows = self.pragma_table_info(&table)?;
                        let schema = Schema::new(vec![
                            crate::Column::new("cid", crate::DataType::Int4),
                            crate::Column::new("name", crate::DataType::Text),
                            crate::Column::new("type", crate::DataType::Text),
                            crate::Column::new("notnull", crate::DataType::Int4),
                            crate::Column::new("dflt_value", crate::DataType::Text),
                            crate::Column::new("pk", crate::DataType::Int4),
                        ]);
                        self.send_query_result(schema, rows).await?;
                        self.send_ready_for_query().await?;
                        return Ok(());
                    }
                    _ => {
                        // Acknowledge unknown / connection-tuning PRAGMAs as no-ops.
                        // Includes: foreign_keys, journal_mode, synchronous, busy_timeout,
                        // cache_size, temp_store, locking_mode, etc.
                        tracing::debug!("PRAGMA stubbed (no-op): {} = {:?}", name, arg);
                        self.send_command_complete("PRAGMA").await?;
                        self.send_ready_for_query().await?;
                        return Ok(());
                    }
                }
            } else {
                self.send_command_complete("PRAGMA").await?;
                self.send_ready_for_query().await?;
                return Ok(());
            }
        } else if trimmed.eq_ignore_ascii_case("COMMIT") {
            // Handle commit even if no transaction active (PostgreSQL warns but succeeds)
            if self.transaction_status == TransactionStatus::InTransaction {
                self.database.commit()?;
            } else {
                self.send_message(BackendMessage::NoticeResponse {
                    severity: "WARNING".to_string(),
                    code: "25P01".to_string(),
                    message: "there is no transaction in progress".to_string(),
                }).await?;
            }
            self.transaction_status = TransactionStatus::Idle;
            self.send_command_complete("COMMIT").await?;
            self.send_ready_for_query().await?;
            return Ok(());
        } else if trimmed.eq_ignore_ascii_case("ROLLBACK") {
            // Handle rollback even if no transaction active (PostgreSQL warns but succeeds)
            if self.transaction_status == TransactionStatus::InTransaction {
                self.database.rollback()?;
            } else {
                self.send_message(BackendMessage::NoticeResponse {
                    severity: "WARNING".to_string(),
                    code: "25P01".to_string(),
                    message: "there is no transaction in progress".to_string(),
                }).await?;
            }
            self.transaction_status = TransactionStatus::Idle;
            self.send_command_complete("ROLLBACK").await?;
            self.send_ready_for_query().await?;
            return Ok(());
        }

        // Transparent write forwarding for standbys (HeliosProxy feature)
        // DML operations are forwarded to primary, DQL executes locally
        #[cfg(feature = "ha-tier1")]
        {
            use crate::replication::ha_state::{ha_state, SyncMode};
            use crate::replication::query_forwarder::{query_forwarder, ForwardedResult};

            if ha_state().is_read_only() {
                let is_write = starts_with_icase(trimmed, "INSERT")
                    || starts_with_icase(trimmed, "UPDATE")
                    || starts_with_icase(trimmed, "DELETE")
                    || starts_with_icase(trimmed, "CREATE")
                    || starts_with_icase(trimmed, "DROP")
                    || starts_with_icase(trimmed, "ALTER")
                    || starts_with_icase(trimmed, "TRUNCATE");

                if is_write {
                    // Check sync mode - only forward in Sync or SemiSync mode
                    let config = ha_state().get_config();
                    let sync_mode = config.as_ref().map(|c| c.sync_mode).unwrap_or(SyncMode::Async);

                    if matches!(sync_mode, SyncMode::Sync | SyncMode::SemiSync) {
                        // Forward write to primary transparently
                        if let Some(forwarder) = query_forwarder() {
                            match forwarder.forward_query(query) {
                                Ok(ForwardedResult::Command { tag, .. }) => {
                                    self.send_command_complete(&tag).await?;
                                    self.send_ready_for_query().await?;
                                    return Ok(());
                                }
                                Ok(ForwardedResult::Rows { columns, rows }) => {
                                    // Send forwarded row results (for RETURNING clauses)
                                    self.send_forwarded_rows(&columns, &rows).await?;
                                    self.send_ready_for_query().await?;
                                    return Ok(());
                                }
                                Ok(ForwardedResult::Error { severity, code, message, detail, hint }) => {
                                    self.send_error(&severity, &code, &message, detail, hint).await?;
                                    self.send_ready_for_query().await?;
                                    return Ok(());
                                }
                                Err(e) => {
                                    self.send_error(
                                        "ERROR",
                                        "08006",
                                        &format!("Failed to forward query to primary: {}", e),
                                        None,
                                        Some("Check primary connectivity".to_string()),
                                    ).await?;
                                    self.send_ready_for_query().await?;
                                    return Ok(());
                                }
                            }
                        } else {
                            // Forwarder not initialized - reject write
                            self.send_error(
                                "ERROR",
                                "25006",
                                "cannot execute write operations: primary connection not established",
                                None,
                                Some("Standby is still connecting to primary".to_string()),
                            ).await?;
                            self.send_ready_for_query().await?;
                            return Ok(());
                        }
                    } else {
                        // Async mode - reject writes (traditional read-only standby)
                        self.send_error(
                            "ERROR",
                            "25006",
                            "cannot execute write operations in read-only mode (async standby)",
                            None,
                            Some("Connect to the primary for write operations, or configure sync mode for transparent routing.".to_string()),
                        ).await?;
                        self.send_ready_for_query().await?;
                        return Ok(());
                    }
                }
            }
        }

        // Check for pg_catalog queries
        if let Some(result) = self.catalog.handle_query(query)? {
            self.send_query_result(result.0, result.1).await?;
            self.send_ready_for_query().await?;
            return Ok(());
        }

        // Execute query through database
        let is_select = starts_with_icase(trimmed, "SELECT");
        let is_dml_returning = !is_select && {
            let upper = trimmed.to_uppercase();
            (starts_with_icase(trimmed, "INSERT")
                || starts_with_icase(trimmed, "UPDATE")
                || starts_with_icase(trimmed, "DELETE"))
                && upper.contains("RETURNING")
        };

        if is_select {
            let (results, columns) = self.database.query_with_columns(query)?;
            let schema = if !columns.is_empty() {
                Schema::new(columns.iter().enumerate().map(|(i, name)| {
                    let data_type = results.first()
                        .and_then(|r| r.values.get(i))
                        .map(Value::data_type)
                        .unwrap_or(crate::DataType::Text);
                    crate::Column {
                        name: name.clone(),
                        data_type,
                        nullable: true,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: crate::ColumnStorageMode::Default,
                    }
                }).collect())
            } else if !results.is_empty() {
                results[0].schema()
            } else {
                Schema::new(vec![])
            };
            self.send_query_result(schema, results).await?;
        } else if is_dml_returning {
            // DML with RETURNING clause - returns rows like a query
            let (affected, tuples) = self.database.execute_returning(query)?;
            if tuples.is_empty() {
                // No rows returned - send command complete with count
                let tag = self.get_command_tag(query, affected);
                self.send_command_complete(&tag).await?;
            } else {
                // Derive schema from plan for proper column names
                let schema = self.derive_returning_schema(query)
                    .unwrap_or_else(|_| {
                        if let Some(first) = tuples.first() {
                            first.schema()
                        } else {
                            Schema::new(vec![])
                        }
                    });
                self.send_query_result(schema, tuples).await?;
            }
        } else {
            let affected = self.database.execute(query)?;
            let tag = self.get_command_tag(query, affected);
            self.send_command_complete(&tag).await?;
        }

        self.send_ready_for_query().await?;
        Ok(())
    }

    // Extended protocol methods are in handler_extended.rs module

    /// Handle SCRAM-SHA-256 authentication flow
    // SAFETY: parts[1] and parts[2] are guarded by parts.len() >= 3 check.
    #[allow(clippy::indexing_slicing)]
    async fn handle_scram_authentication(&mut self) -> Result<()> {
        // Send AuthenticationSASL with SCRAM-SHA-256 mechanism
        self.send_message(BackendMessage::Authentication(
            AuthenticationMessage::ScramSha256
        )).await?;
        self.flush().await?; // Client must read challenge before responding

        // Wait for client-first-message (SASL initial response)
        let client_first = match self.read_message().await? {
            Some(FrontendMessage::PasswordMessage { password }) => password,
            _ => return Err(Error::protocol("Expected SASL initial response")),
        };

        tracing::debug!("Received client-first-message: {}", client_first);

        // Parse client-first-message
        // Format: [gs2-header,]client-first-message-bare
        // client-first-message-bare: n=user,r=nonce
        let parts: Vec<&str> = client_first.split(',').collect();
        if parts.len() < 3 {
            return Err(Error::protocol("Invalid SCRAM client-first-message"));
        }

        // Parse username and client nonce
        let username = parts[1].strip_prefix("n=")
            .ok_or_else(|| Error::protocol("Invalid username in SCRAM message"))?;
        let client_nonce = parts[2].strip_prefix("r=")
            .ok_or_else(|| Error::protocol("Invalid nonce in SCRAM message"))?;

        tracing::info!("SCRAM authentication for user: {}", username);

        // Get user credentials from password store
        let password_store = self.auth_manager.password_store()
            .ok_or_else(|| Error::authentication("SCRAM password store not configured"))?;

        let credentials = password_store.get_credentials(username)
            .ok_or_else(|| Error::authentication("User not found"))?;

        // Create SCRAM state
        let mut scram_state = ScramAuthState::new(username.to_string());
        scram_state.set_client_nonce(client_nonce.to_string());

        // Build client-first-message-bare for auth message
        let client_first_bare = format!("n={},r={}", username, client_nonce);
        scram_state.set_client_first_message_bare(client_first_bare);

        // Generate server-first-message
        let server_first = scram_state.build_server_first_message()?;

        tracing::debug!("Sending server-first-message: {}", server_first);

        // Send AuthenticationSASLContinue with server-first-message
        self.send_message(BackendMessage::Authentication(
            AuthenticationMessage::ScramSha256Continue {
                data: server_first.as_bytes().to_vec(),
            }
        )).await?;
        self.flush().await?; // Client must read continue before responding

        // Wait for client-final-message
        let client_final = match self.read_message().await? {
            Some(FrontendMessage::PasswordMessage { password }) => password,
            _ => return Err(Error::protocol("Expected SASL response")),
        };

        tracing::debug!("Received client-final-message: {}", client_final);

        // Parse client-final-message
        // Format: c=channel-binding,r=nonce,p=proof
        let final_parts: Vec<&str> = client_final.split(',').collect();
        if final_parts.len() < 3 {
            return Err(Error::protocol("Invalid SCRAM client-final-message"));
        }

        // Extract proof
        let proof_part = final_parts.iter()
            .find(|p| p.starts_with("p="))
            .ok_or_else(|| Error::protocol("Missing proof in client-final-message"))?;
        let client_proof_b64 = proof_part.strip_prefix("p=")
            .ok_or_else(|| Error::protocol("Invalid proof format"))?;

        // Build client-final-message-without-proof
        let client_final_without_proof: Vec<&str> = final_parts.iter()
            .filter(|p| !p.starts_with("p="))
            .copied()
            .collect();
        let client_final_without_proof = client_final_without_proof.join(",");

        // Verify client proof and get server signature
        let server_signature = scram_state.verify_client_proof(
            client_proof_b64,
            &client_final_without_proof,
            &credentials.stored_key,
            &credentials.server_key,
        )?;

        tracing::info!("SCRAM authentication successful for user: {}", username);

        // Build server-final-message
        let server_final = scram_state.build_server_final_message(&server_signature)?;

        tracing::debug!("Sending server-final-message: {}", server_final);

        // Send AuthenticationSASLFinal with server-final-message
        self.send_message(BackendMessage::Authentication(
            AuthenticationMessage::ScramSha256Final {
                data: server_final.as_bytes().to_vec(),
            }
        )).await?;

        // Authentication successful
        self.authenticated = true;
        self.username = Some(username.to_string());

        Ok(())
    }

    /// Send query results
    async fn send_query_result(&mut self, schema: Schema, rows: Vec<Tuple>) -> Result<()> {
        // Send RowDescription
        let fields = schema_to_field_descriptions(&schema);
        self.send_message(BackendMessage::RowDescription { fields }).await?;

        // Send DataRows (direct encoding avoids intermediate Vec allocations)
        for row in &rows {
            self.send_data_row_direct(row).await?;
        }

        // Send CommandComplete
        let tag = format!("SELECT {}", rows.len());
        self.send_command_complete(&tag).await?;

        Ok(())
    }

    /// Send forwarded query results from primary (for transparent routing)
    #[cfg(feature = "ha-tier1")]
    async fn send_forwarded_rows(
        &mut self,
        columns: &[crate::replication::query_forwarder::ColumnInfo],
        rows: &[Vec<Option<String>>],
    ) -> Result<()> {
        use crate::protocol::postgres::messages::FieldDescription;

        // Send RowDescription
        let fields: Vec<FieldDescription> = columns
            .iter()
            .map(|col| FieldDescription {
                name: col.name.clone(),
                table_oid: 0,
                column_attr_num: 0,
                data_type_oid: col.type_oid,
                data_type_size: -1,
                type_modifier: -1,
                format_code: 0, // Text format
            })
            .collect();
        self.send_message(BackendMessage::RowDescription { fields }).await?;

        // Send DataRows
        for row in rows {
            let values: Vec<Option<Vec<u8>>> = row
                .iter()
                .map(|v| v.as_ref().map(|s| s.as_bytes().to_vec()))
                .collect();
            self.send_message(BackendMessage::DataRow { values }).await?;
        }

        // Send CommandComplete
        let tag = format!("SELECT {}", rows.len());
        self.send_command_complete(&tag).await?;

        Ok(())
    }

    /// Send a backend message (write only, no flush).
    /// Caller is responsible for flushing at the end of a response cycle.
    pub(super) async fn send_message(&mut self, msg: BackendMessage) -> Result<()> {
        self.write_buf.clear();
        msg.encode(&mut self.write_buf);
        self.stream.write_all(&self.write_buf).await
            .map_err(|e| Error::network(format!("Failed to send message: {}", e)))?;
        Ok(())
    }

    /// Encode and send a DataRow directly from a Tuple, avoiding intermediate Vec allocations.
    /// Uses length-prefix backpatching: writes placeholder, encodes values, then patches the length.
    #[allow(clippy::indexing_slicing)] // length_pos and count_pos are set by us, always valid
    async fn send_data_row_direct(&mut self, tuple: &Tuple) -> Result<()> {
        self.write_buf.clear();
        self.write_buf.put_u8(b'D');

        // Reserve space for message length (4 bytes) — will be backpatched
        let length_pos = self.write_buf.len();
        self.write_buf.put_i32(0);

        // Column count
        self.write_buf.put_i16(tuple.values.len() as i16);

        // Encode each value directly into the buffer
        let mut itoa_buf = itoa::Buffer::new();
        let mut ryu_buf = ryu::Buffer::new();
        for val in &tuple.values {
            match val {
                Value::Null => {
                    self.write_buf.put_i32(-1);
                }
                Value::Boolean(b) => {
                    self.write_buf.put_i32(1);
                    self.write_buf.put_u8(if *b { b't' } else { b'f' });
                }
                Value::Int2(i) => {
                    let s = itoa_buf.format(*i);
                    self.write_buf.put_i32(s.len() as i32);
                    self.write_buf.put_slice(s.as_bytes());
                }
                Value::Int4(i) => {
                    let s = itoa_buf.format(*i);
                    self.write_buf.put_i32(s.len() as i32);
                    self.write_buf.put_slice(s.as_bytes());
                }
                Value::Int8(i) => {
                    let s = itoa_buf.format(*i);
                    self.write_buf.put_i32(s.len() as i32);
                    self.write_buf.put_slice(s.as_bytes());
                }
                Value::Float4(f) => {
                    let s = ryu_buf.format(*f);
                    self.write_buf.put_i32(s.len() as i32);
                    self.write_buf.put_slice(s.as_bytes());
                }
                Value::Float8(f) => {
                    let s = ryu_buf.format(*f);
                    self.write_buf.put_i32(s.len() as i32);
                    self.write_buf.put_slice(s.as_bytes());
                }
                Value::String(s) => {
                    self.write_buf.put_i32(s.len() as i32);
                    self.write_buf.put_slice(s.as_bytes());
                }
                Value::Bytes(b) => {
                    self.write_buf.put_i32(b.len() as i32);
                    self.write_buf.put_slice(b);
                }
                Value::Json(j) => {
                    let s = j.to_string();
                    self.write_buf.put_i32(s.len() as i32);
                    self.write_buf.put_slice(s.as_bytes());
                }
                Value::Numeric(n) => {
                    self.write_buf.put_i32(n.len() as i32);
                    self.write_buf.put_slice(n.as_bytes());
                }
                Value::Uuid(u) => {
                    let s = u.to_string();
                    self.write_buf.put_i32(s.len() as i32);
                    self.write_buf.put_slice(s.as_bytes());
                }
                Value::Timestamp(ts) => {
                    // PG wire format: space separator, microsecond
                    // precision, no trailing zone. rfc3339 nanosecond
                    // output (9 fractional digits) crashes psycopg
                    // ("timestamp too large (after year 10K)") and
                    // makes drizzle-orm's timestamp parser return null.
                    let s = ts.naive_utc().format("%Y-%m-%d %H:%M:%S%.6f").to_string();
                    self.write_buf.put_i32(s.len() as i32);
                    self.write_buf.put_slice(s.as_bytes());
                }
                Value::Date(d) => {
                    let s = d.format("%Y-%m-%d").to_string();
                    self.write_buf.put_i32(s.len() as i32);
                    self.write_buf.put_slice(s.as_bytes());
                }
                Value::Time(t) => {
                    // Microsecond precision — same reason as Timestamp.
                    let s = t.format("%H:%M:%S%.6f").to_string();
                    self.write_buf.put_i32(s.len() as i32);
                    self.write_buf.put_slice(s.as_bytes());
                }
                Value::Interval(micros) => {
                    let total_secs = micros / 1_000_000;
                    let days = total_secs / 86400;
                    let hours = (total_secs % 86400) / 3600;
                    let mins = (total_secs % 3600) / 60;
                    let secs = total_secs % 60;
                    let s = if days > 0 {
                        format!("{} days {:02}:{:02}:{:02}", days, hours, mins, secs)
                    } else {
                        format!("{:02}:{:02}:{:02}", hours, mins, secs)
                    };
                    self.write_buf.put_i32(s.len() as i32);
                    self.write_buf.put_slice(s.as_bytes());
                }
                Value::Array(arr) => {
                    let val_length_pos = self.write_buf.len();
                    self.write_buf.put_i32(0);
                    self.write_buf.put_u8(b'{');
                    for (i, v) in arr.iter().enumerate() {
                        if i > 0 { self.write_buf.put_u8(b','); }
                        match v {
                            Value::String(s) => {
                                self.write_buf.put_u8(b'"');
                                self.write_buf.put_slice(s.as_bytes());
                                self.write_buf.put_u8(b'"');
                            }
                            Value::Null => self.write_buf.put_slice(b"NULL"),
                            other => {
                                let s = other.to_string();
                                self.write_buf.put_slice(s.as_bytes());
                            }
                        }
                    }
                    self.write_buf.put_u8(b'}');
                    let val_len = (self.write_buf.len() - val_length_pos - 4) as i32;
                    self.write_buf[val_length_pos..val_length_pos + 4].copy_from_slice(&val_len.to_be_bytes());
                }
                Value::Vector(v) => {
                    // Format as PostgreSQL array: {1.0,2.0,3.0}
                    // Reserve length slot, write content, backpatch
                    let val_length_pos = self.write_buf.len();
                    self.write_buf.put_i32(0);
                    self.write_buf.put_u8(b'{');
                    for (i, x) in v.iter().enumerate() {
                        if i > 0 { self.write_buf.put_u8(b','); }
                        let s = ryu_buf.format(*x);
                        self.write_buf.put_slice(s.as_bytes());
                    }
                    self.write_buf.put_u8(b'}');
                    let val_len = (self.write_buf.len() - val_length_pos - 4) as i32;
                    self.write_buf[val_length_pos..val_length_pos + 4].copy_from_slice(&val_len.to_be_bytes());
                }
                Value::DictRef { dict_id } => {
                    let s = itoa_buf.format(*dict_id);
                    self.write_buf.put_i32(s.len() as i32);
                    self.write_buf.put_slice(s.as_bytes());
                }
                Value::CasRef { hash } => {
                    let s = hex::encode(hash);
                    self.write_buf.put_i32(s.len() as i32);
                    self.write_buf.put_slice(s.as_bytes());
                }
                Value::ColumnarRef => {
                    self.write_buf.put_i32(10);
                    self.write_buf.put_slice(b"<columnar>");
                }
            }
        }

        // Backpatch the message length (excludes the type byte, includes itself)
        let msg_len = (self.write_buf.len() - length_pos) as i32;
        self.write_buf[length_pos..length_pos + 4].copy_from_slice(&msg_len.to_be_bytes());

        self.stream.write_all(&self.write_buf).await
            .map_err(|e| Error::network(format!("Failed to send message: {}", e)))?;
        Ok(())
    }

    /// Flush all buffered writes to the client.
    async fn flush(&mut self) -> Result<()> {
        self.stream.flush().await
            .map_err(|e| Error::network(format!("Failed to flush stream: {}", e)))
    }

    /// Send authentication OK
    async fn send_auth_ok(&mut self) -> Result<()> {
        self.send_message(BackendMessage::Authentication(AuthenticationMessage::Ok)).await
    }

    /// Send parameter status
    async fn send_parameter_status(&mut self, name: &str, value: &str) -> Result<()> {
        self.send_message(BackendMessage::ParameterStatus {
            name: name.to_string(),
            value: value.to_string(),
        }).await
    }

    /// Send ready for query (flushes all buffered writes)
    /// Handle `DO $$ … END $$ ;` blocks by executing the inner
    /// plain-SQL body statement-by-statement. We do **not** implement
    /// PL/pgSQL control flow (IF / LOOP / RAISE / variables) — only
    /// sequences of plain SQL statements are supported. This covers
    /// the common Drizzle / Prisma migration backfill pattern, not
    /// procedural logic.
    async fn handle_do_block(&mut self, query: &str) -> Result<()> {
        // Extract the body between the first and last `$tag$` delimiters.
        let body = pg_extract_do_block_body(query).unwrap_or("");
        // Strip optional `BEGIN`/`END` wrapper tokens — their semantics
        // (open/close anonymous PL/pgSQL block) are moot once we're
        // just running plain statements sequentially.
        let trimmed = pg_strip_begin_end(body.trim());

        // If the body uses real PL/pgSQL control flow (DECLARE, IF,
        // LOOP, FOR … IN SELECT … LOOP, RAISE, variables with `:=`,
        // etc.), we can't execute it — only plain-SQL bodies are
        // supported in this version. Surface a clear error so the
        // caller knows to either rewrite the migration as plain SQL
        // or to skip this hunk and run it manually — DO NOT silently
        // succeed, which would corrupt migrations.
        if let Some(kw) = pg_detect_plpgsql(trimmed) {
            return Err(Error::query_execution(format!(
                "PL/pgSQL control flow (`{kw}`) inside DO blocks is not yet \
                 supported in HeliosDB Nano. Rewrite the block as plain SQL, \
                 or execute each statement separately. \
                 See: docs/compatibility/plpgsql.md"
            )));
        }

        let statements = pg_split_sql_respecting_quotes(trimmed);

        if statements.is_empty() {
            self.send_command_complete("DO").await?;
            self.send_ready_for_query().await?;
            return Ok(());
        }
        // Execute each inner statement but suppress its ReadyForQuery;
        // we emit a single `DO` CommandComplete + RFQ at the end. We
        // call `execute()` directly rather than recursing into
        // `handle_single_query` — avoids async recursion and skips the
        // per-statement protocol response chatter, which is what
        // callers of a DO block actually expect.
        let prev = self.suppress_ready_for_query;
        self.suppress_ready_for_query = true;
        for stmt in &statements {
            if let Err(e) = self.database.execute(stmt) {
                self.suppress_ready_for_query = prev;
                return Err(e);
            }
        }
        self.suppress_ready_for_query = prev;
        self.send_command_complete("DO").await?;
        self.send_ready_for_query().await?;
        Ok(())
    }

    async fn send_ready_for_query(&mut self) -> Result<()> {
        // Multi-statement simple query mode: suppress intra-batch
        // ReadyForQuery so the client sees exactly one at the end.
        if self.suppress_ready_for_query {
            return Ok(());
        }
        self.send_message(BackendMessage::ReadyForQuery {
            status: self.transaction_status,
        }).await?;
        self.flush().await
    }

    /// Send command complete
    pub(super) async fn send_command_complete(&mut self, tag: &str) -> Result<()> {
        self.send_message(BackendMessage::CommandComplete {
            tag: tag.to_string(),
        }).await
    }

    /// Send error response
    async fn send_error(&mut self, severity: &str, code: &str, message: &str, detail: Option<String>, hint: Option<String>) -> Result<()> {
        self.send_message(BackendMessage::ErrorResponse {
            severity: severity.to_string(),
            code: code.to_string(),
            message: message.to_string(),
            detail,
            hint,
            position: None,
        }).await?;
        self.send_ready_for_query().await
    }

    /// SQLite-shaped `PRAGMA table_info(t)` rows.
    ///
    /// One row per column with `(cid, name, type, notnull, dflt_value, pk)` —
    /// the shape sqlite3-driven Python apps (notably token-dashboard's schema
    /// migrators) consume to detect "does this column already exist?".
    fn pragma_table_info(&self, table: &str) -> Result<Vec<Tuple>> {
        let catalog = self.database.storage.catalog();
        let schema = catalog.get_table_schema(table)?;
        let mut rows = Vec::with_capacity(schema.columns.len());
        for (idx, col) in schema.columns.iter().enumerate() {
            rows.push(Tuple::new(vec![
                Value::Int4(idx as i32),
                Value::String(col.name.clone()),
                Value::String(format!("{:?}", col.data_type).to_uppercase()),
                Value::Int4(if col.nullable { 0 } else { 1 }),
                col.default_expr
                    .as_ref()
                    .map(|d| Value::String(d.clone()))
                    .unwrap_or(Value::Null),
                Value::Int4(if col.primary_key { 1 } else { 0 }),
            ]));
        }
        Ok(rows)
    }

    /// Derive schema for a DML statement with RETURNING clause
    fn derive_returning_schema(&self, sql: &str) -> Result<Schema> {
        let catalog = self.database.storage.catalog();
        let planner = crate::sql::planner::Planner::with_catalog(&catalog)
            .with_sql(sql.to_string());
        let (statement, _) = self.database.parse_cached(sql)?;
        let plan = planner.statement_to_plan(statement)?;

        // Extract table name and returning items from the plan
        let (table_name, returning_items) = match &plan {
            crate::sql::LogicalPlan::Insert { table_name, returning, .. }
            | crate::sql::LogicalPlan::InsertSelect { table_name, returning, .. } => {
                (table_name.as_str(), returning.as_ref())
            }
            crate::sql::LogicalPlan::Update { table_name, returning, .. } => {
                (table_name.as_str(), returning.as_ref())
            }
            crate::sql::LogicalPlan::Delete { table_name, returning, .. } => {
                (table_name.as_str(), returning.as_ref())
            }
            _ => return Err(crate::Error::query_execution("Not a DML statement")),
        };

        if let Some(items) = returning_items {
            let table_schema = catalog.get_table_schema(table_name)?;
            Ok(crate::EmbeddedDatabase::returning_schema(&table_schema, items))
        } else {
            Ok(Schema::new(vec![]))
        }
    }

    /// Get command tag for a query
    pub(super) fn get_command_tag(&self, query: &str, affected: u64) -> String {
        let trimmed = query.trim();
        if starts_with_icase(trimmed, "INSERT") {
            format!("INSERT 0 {}", affected)
        } else if starts_with_icase(trimmed, "UPDATE") {
            format!("UPDATE {}", affected)
        } else if starts_with_icase(trimmed, "DELETE") {
            format!("DELETE {}", affected)
        } else if starts_with_icase(trimmed, "CREATE TABLE") {
            "CREATE TABLE".to_string()
        } else if starts_with_icase(trimmed, "DROP TABLE") {
            "DROP TABLE".to_string()
        } else if starts_with_icase(trimmed, "CREATE INDEX") {
            "CREATE INDEX".to_string()
        } else {
            format!("OK {}", affected)
        }
    }

    /// Parse isolation level from transaction command
    ///
    /// Supports:
    /// - BEGIN [TRANSACTION] [ISOLATION LEVEL {READ UNCOMMITTED | READ COMMITTED | REPEATABLE READ | SERIALIZABLE}]
    /// - START TRANSACTION [ISOLATION LEVEL ...]
    /// - SET TRANSACTION ISOLATION LEVEL ...
    // SAFETY: pos is found by .find() so pos+15 is within the string
    // ("ISOLATION LEVEL" is exactly 15 chars).
    #[allow(clippy::indexing_slicing)]
    /// Resolve a SHOW parameter name to (column_name, value).
    /// Returns PostgreSQL-compatible values for common parameters.
    fn resolve_show_parameter(param: &str) -> (String, String) {
        let param_lower = param.to_lowercase();
        let col = param_lower.clone();
        let val = match param_lower.as_str() {
            "server_version" => format!("16.0 (HeliosDB Nano {})", env!("CARGO_PKG_VERSION")),
            "server_encoding" => "UTF8".to_string(),
            "client_encoding" => "UTF8".to_string(),
            "standard_conforming_strings" => "on".to_string(),
            "transaction_isolation" | "transaction isolation level" =>
                "read committed".to_string(),
            "datestyle" => "ISO, MDY".to_string(),
            "timezone" | "time zone" => "UTC".to_string(),
            "integer_datetimes" => "on".to_string(),
            "max_connections" => "100".to_string(),
            "lc_collate" => "en_US.UTF-8".to_string(),
            "lc_ctype" => "en_US.UTF-8".to_string(),
            "search_path" => "\"$user\", public".to_string(),
            "default_transaction_isolation" => "read committed".to_string(),
            "is_superuser" => "on".to_string(),
            _ => String::new(),
        };
        (col, val)
    }

    fn parse_isolation_level(query: &str) -> Option<String> {
        // Find "ISOLATION LEVEL" case-insensitively without allocating
        let query_bytes = query.as_bytes();
        let needle = b"ISOLATION LEVEL";
        let pos = query_bytes.windows(needle.len())
            .position(|w| w.eq_ignore_ascii_case(needle))?;
        let rest = query[pos + needle.len()..].trim();
        if starts_with_icase(rest, "READ UNCOMMITTED") {
            Some("READ UNCOMMITTED".to_string())
        } else if starts_with_icase(rest, "READ COMMITTED") {
            Some("READ COMMITTED".to_string())
        } else if starts_with_icase(rest, "REPEATABLE READ") {
            Some("REPEATABLE READ".to_string())
        } else if starts_with_icase(rest, "SERIALIZABLE") {
            Some("SERIALIZABLE".to_string())
        } else {
            None
        }
    }
}

/// Convert Schema to FieldDescriptions
pub(super) fn schema_to_field_descriptions(schema: &Schema) -> Vec<FieldDescription> {
    schema.columns.iter().map(|col| {
        FieldDescription {
            name: col.name.clone(),
            table_oid: 0,
            column_attr_num: 0,
            data_type_oid: datatype_to_oid(&col.data_type),
            data_type_size: datatype_to_size(&col.data_type),
            type_modifier: -1,
            format_code: 0, // text format
        }
    }).collect()
}

/// Convert DataType to PostgreSQL OID
pub(super) fn datatype_to_oid(dt: &crate::DataType) -> i32 {
    match dt {
        crate::DataType::Boolean => 16,
        crate::DataType::Int2 => 21,
        crate::DataType::Int4 => 23,
        crate::DataType::Int8 => 20,
        crate::DataType::Float4 => 700,
        crate::DataType::Float8 => 701,
        crate::DataType::Text => 25,
        crate::DataType::Varchar(_) => 1043,
        crate::DataType::Json => 114,
        crate::DataType::Jsonb => 3802,
        crate::DataType::Timestamp => 1114,
        crate::DataType::Date => 1082,
        crate::DataType::Time => 1083,
        crate::DataType::Uuid => 2950,
        crate::DataType::Vector(_) => 1000, // Custom type
        _ => 705, // Unknown
    }
}

/// Convert DataType to PostgreSQL size
pub(super) fn datatype_to_size(dt: &crate::DataType) -> i16 {
    match dt {
        crate::DataType::Boolean => 1,
        crate::DataType::Int2 => 2,
        crate::DataType::Int4 => 4,
        crate::DataType::Int8 => 8,
        crate::DataType::Float4 => 4,
        crate::DataType::Float8 => 8,
        crate::DataType::Text => -1, // variable
        crate::DataType::Varchar(_) => -1,
        crate::DataType::Uuid => 16,
        _ => -1,
    }
}

/// Convert Tuple to PostgreSQL wire format values.
/// Uses itoa/ryu for fast numeric formatting (avoids String allocation per value).
pub(super) fn tuple_to_pg_values(tuple: &Tuple) -> Vec<Option<Vec<u8>>> {
    tuple.values.iter().map(|val| {
        match val {
            Value::Null => None,
            Value::Boolean(b) => Some(if *b { b"t" } else { b"f" }.to_vec()),
            Value::Int2(i) => Some(itoa::Buffer::new().format(*i).as_bytes().to_vec()),
            Value::Int4(i) => Some(itoa::Buffer::new().format(*i).as_bytes().to_vec()),
            Value::Int8(i) => Some(itoa::Buffer::new().format(*i).as_bytes().to_vec()),
            Value::Float4(f) => Some(ryu::Buffer::new().format(*f).as_bytes().to_vec()),
            Value::Float8(f) => Some(ryu::Buffer::new().format(*f).as_bytes().to_vec()),
            Value::String(s) => Some(s.as_bytes().to_vec()),
            Value::Bytes(b) => Some(b.clone()),
            Value::Json(j) => Some(j.to_string().into_bytes()),
            Value::Numeric(n) => Some(n.as_bytes().to_vec()),
            Value::Uuid(u) => Some(u.to_string().into_bytes()),
            // PostgreSQL timestamp wire format: microsecond precision,
            // space separator, no trailing timezone on `timestamp without
            // time zone` columns. psycopg's native parser rejects
            // `rfc3339` nanosecond output ("timestamp too large (after
            // year 10K)") — PG itself truncates to 6 fractional digits.
            Value::Timestamp(ts) => Some(
                ts.naive_utc()
                    .format("%Y-%m-%d %H:%M:%S%.6f")
                    .to_string()
                    .into_bytes()
            ),
            Value::Date(d) => Some(d.format("%Y-%m-%d").to_string().into_bytes()),
            // Same story for TIME — truncate to microseconds.
            Value::Time(t) => Some(t.format("%H:%M:%S%.6f").to_string().into_bytes()),
            Value::Interval(micros) => {
                let total_secs = micros / 1_000_000;
                let days = total_secs / 86400;
                let hours = (total_secs % 86400) / 3600;
                let mins = (total_secs % 3600) / 60;
                let secs = total_secs % 60;
                let s = if days > 0 {
                    format!("{} days {:02}:{:02}:{:02}", days, hours, mins, secs)
                } else {
                    format!("{:02}:{:02}:{:02}", hours, mins, secs)
                };
                Some(s.into_bytes())
            }
            Value::Array(arr) => {
                let mut buf = String::with_capacity(arr.len() * 8 + 2);
                buf.push('{');
                for (i, v) in arr.iter().enumerate() {
                    if i > 0 { buf.push(','); }
                    match v {
                        Value::String(s) => { buf.push('"'); buf.push_str(s); buf.push('"'); }
                        Value::Null => buf.push_str("NULL"),
                        other => buf.push_str(&other.to_string()),
                    }
                }
                buf.push('}');
                Some(buf.into_bytes())
            }
            Value::Vector(v) => {
                // Format as PostgreSQL array: {1.0,2.0,3.0}
                let mut buf = String::with_capacity(v.len() * 8 + 2);
                buf.push('{');
                let mut ryu_buf = ryu::Buffer::new();
                for (i, x) in v.iter().enumerate() {
                    if i > 0 { buf.push(','); }
                    buf.push_str(ryu_buf.format(*x));
                }
                buf.push('}');
                Some(buf.into_bytes())
            }
            Value::DictRef { dict_id } => Some(itoa::Buffer::new().format(*dict_id).as_bytes().to_vec()),
            Value::CasRef { hash } => Some(hex::encode(hash).into_bytes()),
            Value::ColumnarRef => Some(b"<columnar>".to_vec()),
        }
    }).collect()
}

/// Split a SQL string on `;` while respecting single-quoted strings
/// and dollar-quoted strings. Mirrors the MySQL handler's splitter
/// (`protocol::mysql::handler::split_sql_respecting_quotes`) with
/// added support for `$$…$$` / `$tag$…$tag$` blocks, which PG uses
/// for procedure bodies and dollar-quoted literals.
fn pg_split_sql_respecting_quotes(sql: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut in_single_quote = false;
    let mut in_dollar: Option<String> = None; // tag between `$` delimiters
    let bytes = sql.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];
        // Inside a `$tag$ … $tag$` block, we scan for the closing tag.
        if let Some(tag) = &in_dollar {
            current.push(b as char);
            if b == b'$' {
                // Try to match `$tag$`
                let close = format!("${tag}$");
                if sql.get(i..i + close.len()) == Some(close.as_str()) {
                    for c in close.chars().skip(1) {
                        current.push(c);
                    }
                    i += close.len();
                    in_dollar = None;
                    continue;
                }
            }
            i += 1;
            continue;
        }
        // Inside a single-quoted string: handle escaped quotes and
        // backslash escapes identically to the MySQL splitter.
        if in_single_quote {
            current.push(b as char);
            if b == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    current.push('\'');
                    i += 2;
                    continue;
                }
                in_single_quote = false;
            } else if b == b'\\' {
                if let Some(&next) = bytes.get(i + 1) {
                    current.push(next as char);
                    i += 2;
                    continue;
                }
            }
            i += 1;
            continue;
        }
        // Start of a dollar-quoted string? `$tag$` where tag is empty or [A-Za-z0-9_]+.
        if b == b'$' {
            let rest = &sql[i + 1..];
            if let Some(end) = rest.find('$') {
                let tag = &rest[..end];
                if tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    current.push('$');
                    for c in tag.chars() { current.push(c); }
                    current.push('$');
                    i += 1 + end + 1;
                    in_dollar = Some(tag.to_string());
                    continue;
                }
            }
        }
        match b {
            b'\'' => { in_single_quote = true; current.push('\''); }
            b';' => {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    statements.push(trimmed);
                }
                current.clear();
            }
            _ => current.push(b as char),
        }
        i += 1;
    }
    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        statements.push(trimmed);
    }
    statements
}

/// Detect `DO $$ … $$` / `DO [LANGUAGE plpgsql] $tag$ … $tag$;` blocks
/// at statement level so we can strip the wrapper and execute the
/// plain-SQL body statement-by-statement.
fn pg_looks_like_do_block(trimmed: &str) -> bool {
    let upper = trimmed.trim_start().to_ascii_uppercase();
    upper.starts_with("DO $") || upper.starts_with("DO LANGUAGE ")
}

/// Scan a DO-block body for PL/pgSQL control-flow tokens we can't
/// interpret. Returns `Some(keyword)` when one is found, so the
/// caller can put the offending token into the error message.
///
/// Kept simple on purpose — whole-word, case-insensitive substring
/// check. Will miss the occasional corner case (`IF` inside a string
/// literal), but those are false positives that lead to a clear
/// error rather than silent data issues.
fn pg_detect_plpgsql(body: &str) -> Option<&'static str> {
    let upper = body.to_ascii_uppercase();
    // Whole-word matches — bracket each keyword with a non-alnum
    // lookalike by padding the haystack.
    let padded = format!(" {upper} ");
    const KEYWORDS: &[&str] = &[
        " DECLARE ", " IF ", " LOOP ", " FOR ",
        " WHILE ", " RAISE ", " RETURN ", " PERFORM ",
        " EXCEPTION ", " EXIT ", " CONTINUE ",
    ];
    for kw in KEYWORDS {
        if padded.contains(kw) {
            // Strip the leading/trailing spaces for the error message.
            let trimmed: &str = kw.trim();
            return Some(match trimmed {
                "DECLARE" => "DECLARE",
                "IF" => "IF",
                "LOOP" => "LOOP",
                "FOR" => "FOR",
                "WHILE" => "WHILE",
                "RAISE" => "RAISE",
                "RETURN" => "RETURN",
                "PERFORM" => "PERFORM",
                "EXCEPTION" => "EXCEPTION",
                "EXIT" => "EXIT",
                "CONTINUE" => "CONTINUE",
                _ => "plpgsql",
            });
        }
    }
    // Also catch the `:=` assignment operator.
    if body.contains(":=") {
        return Some(":=");
    }
    None
}

/// Extract the text between the opening and closing `$tag$` markers
/// of a `DO` block. Returns `None` if the delimiters can't be matched.
fn pg_extract_do_block_body(sql: &str) -> Option<&str> {
    // Skip past `DO` and optional `LANGUAGE plpgsql` preamble.
    let trimmed = sql.trim();
    let after_do = trimmed.get(2..)?.trim_start();
    let after_lang = if after_do.to_ascii_uppercase().starts_with("LANGUAGE") {
        let after = after_do.get("LANGUAGE".len()..)?.trim_start();
        // Eat the language identifier
        let ident_end = after.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))?;
        after.get(ident_end..)?.trim_start()
    } else {
        after_do
    };

    // Expect `$tag$` opener — tag may be empty.
    if !after_lang.starts_with('$') {
        return None;
    }
    let rest = after_lang.get(1..)?;
    let tag_end = rest.find('$')?;
    let tag = rest.get(..tag_end)?;
    let closer = format!("${tag}$");
    let body_start_abs = sql.len() - rest.len() + tag_end + 1;
    let body_search = sql.get(body_start_abs..)?;
    let close_rel = body_search.find(&closer)?;
    sql.get(body_start_abs..body_start_abs + close_rel)
}

/// Strip optional `BEGIN` / `END` tokens that wrap a PL/pgSQL-style
/// DO body. Keeps the inner statements intact so the splitter can
/// run each one independently.
fn pg_strip_begin_end(body: &str) -> &str {
    let mut s = body.trim();
    if s.to_ascii_uppercase().starts_with("BEGIN") {
        s = s.get(5..).map(str::trim_start).unwrap_or(s);
    }
    // Strip a trailing `END;` or `END`.
    let u = s.to_ascii_uppercase();
    for suffix in ["END;", "END"] {
        if u.ends_with(suffix) {
            s = s.get(..s.len() - suffix.len()).map(str::trim_end).unwrap_or(s);
            break;
        }
    }
    s
}
