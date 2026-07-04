//! PostgreSQL protocol handler
//!
//! This module implements the PostgreSQL wire protocol handler that processes
//! client messages and generates appropriate responses.

use crate::{EmbeddedDatabase, Error, Result, Schema, Tuple, Value};

/// Case-insensitive prefix check without allocating a new String.
#[inline]
pub(super) fn starts_with_icase(s: &str, prefix: &str) -> bool {
    // Safety: length is checked on the left side of &&
    #[allow(clippy::indexing_slicing)]
    {
        s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
    }
}

/// True if the statement is a CTE (`WITH …` / `WITH RECURSIVE …`). These are
/// almost always row-returning (`WITH … SELECT`) but do not start with
/// `SELECT`, so the SELECT-prefix routing in both query paths used to misroute
/// them to the command path and silently return zero rows over the wire
/// (Token Dashboard #3). The trailing-char guard rejects `WITHIN` / `WITHOUT`.
#[inline]
pub(super) fn starts_with_cte(s: &str) -> bool {
    let t = s.trim_start();
    starts_with_icase(t, "WITH")
        && t.as_bytes()
            .get(4)
            .map_or(true, |c| !c.is_ascii_alphanumeric() && *c != b'_')
}
use super::auth::{AuthManager, AuthMethod, ScramAuthState};
use super::catalog::PgCatalog;
use super::messages::{AuthenticationMessage, BackendMessage, FieldDescription, FrontendMessage, TransactionStatus};
use super::prepared::PreparedStatementManager;
use super::ssl::SecureConnection;
use bytes::{BufMut, BytesMut};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;

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
    pub(super) suppress_ready_for_query: bool,
    /// Extended-query protocol requires ErrorResponse, then discarding input
    /// until Sync, then ReadyForQuery. Sending ReadyForQuery early can make
    /// drivers close or wedge the connection after an Execute-time error.
    awaiting_sync_after_error: bool,
    /// Per-connection database session (R0.1). Transactions opened by this
    /// connection live in the session, not in the process-global slot, so
    /// concurrent connections get isolated transactions.
    pub(super) session_id: crate::session::SessionId,
}

impl<S> Drop for PgConnectionHandler<S> {
    fn drop(&mut self) {
        // Roll back any open transaction and release the session when the
        // connection ends, however it ends.
        let _ = self.database.destroy_session(self.session_id);
    }
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

        let session_id = database
            .create_wire_session("pg_wire")
            .expect("wire session creation is infallible");
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
            awaiting_sync_after_error: false,
            session_id,
        }
    }
}

#[cfg(unix)]
impl PgConnectionHandler<BufWriter<UnixStream>> {
    /// Create a new connection handler bound to a Unix domain socket stream.
    pub fn new_unix(stream: UnixStream, database: Arc<EmbeddedDatabase>, auth_manager: Arc<AuthManager>) -> Self {
        let session_id = database
            .create_wire_session("pg_wire")
            .expect("wire session creation is infallible");
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
            awaiting_sync_after_error: false,
            session_id,
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

        let session_id = database
            .create_wire_session("pg_wire")
            .expect("wire session creation is infallible");
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
            awaiting_sync_after_error: false,
            session_id,
        }
    }
}

impl<S> PgConnectionHandler<S>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    /// Test-only constructor over an arbitrary stream, pre-authenticated.
    /// Used by the in-crate wire tests (`wire_tests.rs`) — fields are
    /// private, so tests outside this module need a builder.
    #[cfg(test)]
    pub(super) fn new_for_tests(database: Arc<EmbeddedDatabase>, stream: S) -> Self {
        Self {
            stream,
            session_id: database
                .create_wire_session("pg_wire_test")
                .expect("wire session creation is infallible"),
            database: Arc::clone(&database),
            auth_manager: Arc::new(AuthManager::new(AuthMethod::Trust)),
            catalog: PgCatalog::with_database(database),
            prepared_statements: PreparedStatementManager::new(),
            authenticated: true,
            transaction_status: TransactionStatus::Idle,
            buffer: BytesMut::with_capacity(8192),
            username: None,
            scram_state: None,
            write_buf: BytesMut::with_capacity(4096),
            suppress_ready_for_query: false,
            awaiting_sync_after_error: false,
        }
    }

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
                    if self.awaiting_sync_after_error {
                        self.handle_message_while_awaiting_sync(msg).await?;
                        continue;
                    }

                    let wait_for_sync = Self::message_requires_sync_after_error(&msg);
                    if let Err(e) = self.handle_message(msg).await {
                        tracing::error!("Error handling message: {}", e);
                        self.send_error_for_query(&e, wait_for_sync).await?;
                        if wait_for_sync {
                            self.awaiting_sync_after_error = true;
                        }
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

    async fn handle_message_while_awaiting_sync(&mut self, msg: FrontendMessage) -> Result<()> {
        match msg {
            FrontendMessage::Sync => {
                self.awaiting_sync_after_error = false;
                self.send_ready_for_query().await
            }
            FrontendMessage::Terminate => Ok(()),
            _ => {
                tracing::debug!("Discarding frontend message until Sync after extended-query error");
                Ok(())
            }
        }
    }

    fn message_requires_sync_after_error(msg: &FrontendMessage) -> bool {
        matches!(
            msg,
            FrontendMessage::Parse { .. }
                | FrontendMessage::Bind { .. }
                | FrontendMessage::Execute { .. }
                | FrontendMessage::Describe { .. }
                | FrontendMessage::Close { .. }
        )
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
            self.stream
                .read_exact(&mut buf)
                .await
                .map_err(|e| Error::network(format!("Failed to read startup length: {}", e)))?;
            len_buf = buf;
            self.buffer.extend_from_slice(&len_buf);
        }

        let len = i32::from_be_bytes(len_buf) as usize;

        // Validate before allocating: this runs pre-authentication, so an
        // unchecked length here is a remote 2 GiB allocation (or, negative
        // reinterpreted, effectively unbounded). Mirrors parse_startup().
        if !(8..=super::messages::MAX_STARTUP_MESSAGE_LEN).contains(&len) {
            return Err(Error::protocol(format!(
                "Invalid startup message length {} (max {})",
                len as i64,
                super::messages::MAX_STARTUP_MESSAGE_LEN
            )));
        }

        // Calculate how many bytes we still need to read
        let bytes_in_buffer = self.buffer.len();
        let bytes_needed = len.saturating_sub(bytes_in_buffer);

        if bytes_needed > 0 {
            let mut remaining_buf = vec![0u8; bytes_needed];
            self.stream
                .read_exact(&mut remaining_buf)
                .await
                .map_err(|e| Error::network(format!("Failed to read startup message: {}", e)))?;
            self.buffer.extend_from_slice(&remaining_buf);
        }

        // Parse startup message
        let msg = FrontendMessage::parse_startup(&mut self.buffer)?
            .ok_or_else(|| Error::protocol("Invalid startup message"))?;

        if let FrontendMessage::Startup {
            protocol_version,
            params,
        } = msg
        {
            tracing::info!("Protocol version: {}, params: {:?}", protocol_version, params);

            self.username = params.get("user").cloned();

            // Bug 5 — validate the requested database name. Reject
            // unknown names rather than silently routing every
            // connection to the default `heliosdb` keyspace. Reserved
            // names (`heliosdb`, `postgres`) and any registered tenant
            // are accepted. An empty / missing `database` parameter
            // falls back to the username, matching libpq behaviour;
            // we still validate that fallback.
            if let Some(requested) = params.get("database").cloned().or_else(|| params.get("user").cloned()) {
                if !self.database.database_name_is_valid(&requested) {
                    return Err(Error::authentication(format!(
                        "database \"{requested}\" does not exist"
                    )));
                }
            }

            // Send authentication request
            match self.auth_manager.method() {
                AuthMethod::Trust => {
                    self.authenticated = true;
                    self.send_auth_ok().await?;
                }
                AuthMethod::CleartextPassword => {
                    self.send_message(BackendMessage::Authentication(AuthenticationMessage::CleartextPassword))
                        .await?;
                    self.flush().await?; // Client must read challenge before responding

                    // Wait for password message
                    if let Some(FrontendMessage::PasswordMessage { password }) = self.read_message().await? {
                        let username = self
                            .username
                            .as_ref()
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
            )
            .await?;
            self.send_parameter_status("server_encoding", "UTF8").await?;
            self.send_parameter_status("client_encoding", "UTF8").await?;
            self.send_parameter_status("DateStyle", "ISO, MDY").await?;
            self.send_parameter_status("TimeZone", "UTC").await?;
            self.send_parameter_status("integer_datetimes", "on").await?;
            // Advertise standard_conforming_strings=on (the PostgreSQL default
            // since 9.1, and what Nano's lexer actually implements: backslashes
            // in regular '...' literals are ordinary characters). Drivers such
            // as psycopg2 read this at connect time; when it is ABSENT they
            // assume the legacy off behaviour and DOUBLE every backslash in
            // bytea/text literals (`'\x00..'::bytea` -> `'\\x00..'::bytea`),
            // which the (correctly conforming) lexer then decodes via bytea
            // escape format into the wrong bytes — silently corrupting BLOB/RAW
            // round-trips. Sending it makes those drivers emit single-backslash
            // literals that Nano decodes correctly.
            self.send_parameter_status("standard_conforming_strings", "on")
                .await?;

            // Item 10: advertise the helios.* opt-in capabilities so a
            // capability-probing client (HeliosProxy) auto-enables only what
            // this server actually supports. libpq tolerates unknown
            // ParameterStatus names, and non-helios clients ignore them, so
            // this is safe to always send (paid once per connection, off the
            // per-query path pg35 measures).
            self.send_parameter_status("helios.copy", "on").await?;
            self.send_parameter_status("helios.pipeline", "on").await?;
            self.send_parameter_status("helios.plan_cache", "on").await?;
            self.send_parameter_status("helios.binary_results", "on").await?;
            self.send_parameter_status("helios.reset_session", "on").await?;
            self.send_parameter_status("helios.fast_autocommit", "off").await?;

            // Send backend key data
            self.send_message(BackendMessage::BackendKeyData {
                process_id: std::process::id() as i32,
                secret_key: rand::random(),
            })
            .await?;

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

    /// Handle a frontend message.
    ///
    /// `pub(super)` so the wire conformance tests can drive an exact
    /// Parse/Bind/Execute/Sync pipeline through the same dispatch the main
    /// loop uses (the loop is just `read_message` → `handle_message`).
    pub(super) async fn handle_message(&mut self, msg: FrontendMessage) -> Result<()> {
        if !self.authenticated && !matches!(msg, FrontendMessage::PasswordMessage { .. }) {
            return Err(Error::authentication("Not authenticated"));
        }

        match msg {
            FrontendMessage::Query { query } => {
                self.handle_query(&query).await?;
            }
            FrontendMessage::Parse {
                statement_name,
                query,
                param_types,
            } => {
                self.handle_parse_extended(statement_name, query, param_types).await?;
            }
            FrontendMessage::Bind {
                portal_name,
                statement_name,
                param_formats,
                params,
                result_formats,
            } => {
                self.handle_bind_extended(portal_name, statement_name, param_formats, params, result_formats)
                    .await?;
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
    pub(super) async fn handle_single_query(&mut self, query: &str) -> Result<()> {
        tracing::debug!("Executing query: {}", query);

        // `DO $$ … $$` / `DO LANGUAGE … $$ … $$` blocks. sqlparser
        // doesn't support DO, so we unwrap the body and recurse over
        // the plain-SQL statements inside. No PL/pgSQL control flow
        // is expanded — this covers the common Drizzle / Prisma
        // migration backfill pattern only.
        if pg_looks_like_do_block(query.trim()) {
            return self.handle_do_block(query).await;
        }

        // KanttBan #23 (v3.31.1 phase 2.2): rewrite PG's
        // `SELECT FROM …` shorthand (empty projection, valid in
        // PG inside EXISTS subqueries) to `SELECT 1 FROM …`.
        // sqlparser-rs doesn't accept the empty-projection form,
        // and drizzle-kit's getColumnsInfoQuery uses it heavily in
        // an EXISTS subquery for SERIAL detection. Hoisting the
        // rewrite into a single helper so every wire-protocol path
        // (simple-query here, extended-query in handler_extended.rs)
        // benefits.
        let rewritten = pg_rewrite_empty_projection(query);
        let query = rewritten.as_str();

        // Check for empty query
        if query.trim().is_empty() {
            self.send_message(BackendMessage::EmptyQueryResponse).await?;
            self.send_ready_for_query().await?;
            return Ok(());
        }

        // COPY ... FROM STDIN / TO STDOUT is a wire sub-protocol — handle it
        // here, before the normal parse/plan path (item 2c). parse_copy returns
        // None for any non-STDIN/STDOUT SQL, so everything else falls through.
        if let Some(copy_stmt) = super::copy::parse_copy(query) {
            return self.handle_copy(copy_stmt).await;
        }

        // Handle transaction commands (case-insensitive without allocation)
        let trimmed = query.trim();
        if trimmed.eq_ignore_ascii_case("BEGIN")
            || starts_with_icase(trimmed, "BEGIN ")
            || trimmed.eq_ignore_ascii_case("START TRANSACTION")
            || starts_with_icase(trimmed, "START TRANSACTION ")
        {
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
                })
                .await?;
            } else {
                self.rollback_failed_transaction_for_recovery()?;
                // Map the requested isolation level onto the session before
                // BEGIN (READ UNCOMMITTED runs as READ COMMITTED, like
                // PostgreSQL itself).
                if let Some(level) = isolation_level.as_deref() {
                    let mapped = match level {
                        "SERIALIZABLE" => crate::session::IsolationLevel::Serializable,
                        "REPEATABLE READ" => crate::session::IsolationLevel::RepeatableRead,
                        _ => crate::session::IsolationLevel::ReadCommitted,
                    };
                    let _ = self.database.set_session_isolation(self.session_id, mapped);
                    tracing::debug!("Transaction starting with isolation level: {}", level);
                }
                self.database.begin_transaction_for_session(self.session_id)?;
                self.transaction_status = TransactionStatus::InTransaction;
            }
            self.send_command_complete("BEGIN").await?;
            self.send_ready_for_query().await?;
            return Ok(());
        } else if starts_with_icase(trimmed, "SET TRANSACTION ISOLATION LEVEL ")
            || starts_with_icase(trimmed, "SET SESSION CHARACTERISTICS")
        {
            // SET TRANSACTION ISOLATION LEVEL is valid before any queries in a transaction
            let level = Self::parse_isolation_level(trimmed);
            if level.is_some() {
                self.send_command_complete("SET").await?;
            } else {
                self.send_error("ERROR", "22023", "Invalid isolation level", None, None)
                    .await?;
                return Ok(());
            }
            self.send_ready_for_query().await?;
            return Ok(());
        } else if starts_with_icase(trimmed, "SET ")
            && !starts_with_icase(trimmed, "SET TRANSACTION")
            && !starts_with_icase(trimmed, "SET SESSION CHARACTERISTICS")
        {
            // R1.3-p2: session-scoped synchronous_commit (PG-compatible).
            match EmbeddedDatabase::parse_synchronous_commit_statement(trimmed) {
                Ok(Some(value)) => {
                    if let Err(e) = self.database.set_session_synchronous_commit(self.session_id, value) {
                        self.send_error("ERROR", "22023", &e.to_string(), None, None).await?;
                        return Ok(());
                    }
                    self.send_command_complete("SET").await?;
                    self.send_ready_for_query().await?;
                    return Ok(());
                }
                Err(e) => {
                    self.send_error("ERROR", "22023", &e.to_string(), None, None).await?;
                    return Ok(());
                }
                Ok(None) => {}
            }
            // Item 9: SET helios.fast_autocommit = on|off — intercept before the
            // generic ack below (which would silently drop an unknown SET).
            match EmbeddedDatabase::parse_helios_fast_autocommit_statement(trimmed) {
                Ok(Some(value)) => {
                    if let Err(e) = self.database.set_session_fast_autocommit(self.session_id, value) {
                        self.send_error("ERROR", "22023", &e.to_string(), None, None).await?;
                        return Ok(());
                    }
                    // Item 10: GUC_REPORT — echo the new value so a capability-
                    // probing pool observes the change.
                    self.send_parameter_status("helios.fast_autocommit", if value { "on" } else { "off" })
                        .await?;
                    self.send_command_complete("SET").await?;
                    self.send_ready_for_query().await?;
                    return Ok(());
                }
                Err(e) => {
                    self.send_error("ERROR", "22023", &e.to_string(), None, None).await?;
                    return Ok(());
                }
                Ok(None) => {}
            }
            if EmbeddedDatabase::is_fk_setting_statement(trimmed) {
                if let Err(e) = self.database.execute(trimmed) {
                    self.send_error("ERROR", "22023", &e.to_string(), None, None).await?;
                    return Ok(());
                }
            }
            // Handle generic SET commands for client compatibility (e.g., SET client_encoding = 'UTF8').
            self.send_command_complete("SET").await?;
            self.send_ready_for_query().await?;
            return Ok(());
        } else if starts_with_icase(trimmed, "RESET ")
            && trimmed[6..]
                .trim()
                .trim_end_matches(';')
                .trim()
                .eq_ignore_ascii_case("synchronous_commit")
        {
            // R1.3-p2: back to the storage.durable_commit default.
            if let Err(e) = self.database.set_session_synchronous_commit(self.session_id, None) {
                self.send_error("ERROR", "22023", &e.to_string(), None, None).await?;
                return Ok(());
            }
            self.send_command_complete("RESET").await?;
            self.send_ready_for_query().await?;
            return Ok(());
        } else if starts_with_icase(trimmed, "RESET ")
            && trimmed[6..]
                .trim()
                .trim_end_matches(';')
                .trim()
                .eq_ignore_ascii_case("helios.fast_autocommit")
        {
            // Item 9: RESET helios.fast_autocommit -> default (off).
            let _ = self.database.set_session_fast_autocommit(self.session_id, false);
            self.send_parameter_status("helios.fast_autocommit", "off").await?; // item 10 GUC_REPORT
            self.send_command_complete("RESET").await?;
            self.send_ready_for_query().await?;
            return Ok(());
        } else if starts_with_icase(trimmed, "SHOW ") && !crate::sql::Parser::is_show_branches(trimmed) {
            // Handle SHOW commands for client compatibility
            let param = trimmed[5..].trim().trim_end_matches(';').trim();
            let (col_name, value) = if param.eq_ignore_ascii_case("synchronous_commit") {
                // R1.3-p2: effective per-session value.
                let effective = self
                    .database
                    .session_synchronous_commit_effective(self.session_id)
                    .unwrap_or(false);
                (
                    "synchronous_commit".to_string(),
                    if effective { "on".to_string() } else { "off".to_string() },
                )
            } else if param.eq_ignore_ascii_case("helios.fast_autocommit") {
                // Item 9: SHOW helios.fast_autocommit.
                let on = self
                    .database
                    .session_fast_autocommit(self.session_id)
                    .unwrap_or(false);
                (
                    "helios.fast_autocommit".to_string(),
                    if on { "on".to_string() } else { "off".to_string() },
                )
            } else {
                Self::resolve_show_parameter(param)
            };
            let schema = Schema::new(vec![crate::Column::new(&col_name, crate::DataType::Text)]);
            let row = Tuple::new(vec![Value::String(value)]);
            let rows = vec![row];
            self.send_query_result(schema, &rows).await?;
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
                        let table = table
                            .trim()
                            .trim_matches(|c| c == '\'' || c == '"' || c == '`')
                            .to_string();
                        let rows = self.pragma_table_info(&table)?;
                        let schema = Schema::new(vec![
                            crate::Column::new("cid", crate::DataType::Int4),
                            crate::Column::new("name", crate::DataType::Text),
                            crate::Column::new("type", crate::DataType::Text),
                            crate::Column::new("notnull", crate::DataType::Int4),
                            crate::Column::new("dflt_value", crate::DataType::Text),
                            crate::Column::new("pk", crate::DataType::Int4),
                        ]);
                        self.send_query_result(schema, &rows).await?;
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
                self.database.commit_transaction_for_session(self.session_id)?;
            } else if self.transaction_status == TransactionStatus::Failed {
                self.rollback_failed_transaction_for_recovery()?;
                self.send_command_complete("ROLLBACK").await?;
                self.send_ready_for_query().await?;
                return Ok(());
            } else {
                self.send_message(BackendMessage::NoticeResponse {
                    severity: "WARNING".to_string(),
                    code: "25P01".to_string(),
                    message: "there is no transaction in progress".to_string(),
                })
                .await?;
            }
            self.transaction_status = TransactionStatus::Idle;
            self.send_command_complete("COMMIT").await?;
            self.send_ready_for_query().await?;
            return Ok(());
        } else if trimmed.eq_ignore_ascii_case("ROLLBACK") {
            // Handle rollback even if no transaction active (PostgreSQL warns but succeeds)
            if matches!(
                self.transaction_status,
                TransactionStatus::InTransaction | TransactionStatus::Failed
            ) {
                if self.database.session_in_transaction(self.session_id) {
                    self.database.rollback_transaction_for_session(self.session_id)?;
                }
            } else {
                self.send_message(BackendMessage::NoticeResponse {
                    severity: "WARNING".to_string(),
                    code: "25P01".to_string(),
                    message: "there is no transaction in progress".to_string(),
                })
                .await?;
            }
            self.transaction_status = TransactionStatus::Idle;
            self.send_command_complete("ROLLBACK").await?;
            self.send_ready_for_query().await?;
            return Ok(());
        } else if let Some(target) = Self::parse_discard_target(trimmed) {
            // Item 5: DISCARD resets session state without a reconnect, so a
            // connection pool can hand this physical connection to a different
            // client. DISCARD ALL is the pool's reset; the narrower forms are
            // accepted for compatibility.
            match target {
                "ALL" => {
                    if matches!(
                        self.transaction_status,
                        TransactionStatus::InTransaction | TransactionStatus::Failed
                    ) && self.database.session_in_transaction(self.session_id)
                    {
                        self.database.rollback_transaction_for_session(self.session_id)?;
                    }
                    self.transaction_status = TransactionStatus::Idle;
                    self.prepared_statements.clear_all()?;
                    // Reset session GUCs to their defaults.
                    let _ = self.database.set_session_synchronous_commit(self.session_id, None);
                    let _ = self.database.set_session_fast_autocommit(self.session_id, false);
                    let _ = self.database.set_session_isolation(
                        self.session_id,
                        crate::session::IsolationLevel::ReadCommitted,
                    );
                    // Item 10: GUC_REPORT the reset helios.* GUC.
                    self.send_parameter_status("helios.fast_autocommit", "off").await?;
                }
                "PLANS" => {
                    self.prepared_statements.clear_all()?;
                }
                // Nano has no session-scoped temp tables or sequence caches to
                // drop; accept and ack for client compatibility.
                _ => {}
            }
            self.send_command_complete(&format!("DISCARD {target}")).await?;
            self.send_ready_for_query().await?;
            return Ok(());
        }

        if self.transaction_status == TransactionStatus::Failed {
            self.send_error(
                "ERROR",
                "25P02",
                "current transaction is aborted, commands ignored until end of transaction block",
                None,
                Some("Use ROLLBACK to clear the failed transaction state".to_string()),
            )
            .await?;
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
                                Ok(ForwardedResult::Error {
                                    severity,
                                    code,
                                    message,
                                    detail,
                                    hint,
                                }) => {
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
                                    )
                                    .await?;
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
                            )
                            .await?;
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
            let (schema, rows) = result;
            self.send_query_result(schema, &rows).await?;
            self.send_ready_for_query().await?;
            return Ok(());
        }

        // Execute query through database
        let is_select = starts_with_icase(trimmed, "SELECT");
        let is_show_branches = crate::sql::Parser::is_show_branches(trimmed);
        // A CTE (`WITH … SELECT …`) is row-returning but doesn't start with
        // SELECT; route it through query_with_columns so it returns rows
        // rather than a command tag (Token Dashboard #3). Skip the read cache
        // so a data-modifying CTE still executes on every call.
        let is_cte = !is_select && starts_with_cte(trimmed);
        let is_dml_returning = !is_select && !is_cte && {
            let upper = trimmed.to_uppercase();
            (starts_with_icase(trimmed, "INSERT")
                || starts_with_icase(trimmed, "UPDATE")
                || starts_with_icase(trimmed, "DELETE"))
                && upper.contains("RETURNING")
        };

        if is_select || is_show_branches {
            let cached_query = if is_show_branches || self.database.session_in_transaction(self.session_id) {
                None
            } else {
                self.database.try_cached_query_with_columns(query)
            };
            if let Some((cached_results, columns)) = cached_query {
                let schema = Self::schema_from_query_columns(&columns, cached_results.as_slice());
                self.send_query_result(schema, cached_results.as_slice()).await?;
            } else {
                let (results, columns) = self.database.query_with_columns_for_session(self.session_id, query)?;
                let schema = Self::schema_from_query_columns(&columns, &results);
                self.send_query_result(schema, &results).await?;
            }
        } else if is_cte {
            let (results, columns) = self.database.query_with_columns_for_session(self.session_id, query)?;
            let schema = Self::schema_from_query_columns(&columns, &results);
            self.send_query_result(schema, &results).await?;
        } else if is_dml_returning {
            // DML with RETURNING clause - returns rows like a query
            let (affected, tuples) = self.database.execute_returning_for_session(self.session_id, query)?;
            if tuples.is_empty() {
                // No rows returned - send command complete with count
                let tag = self.get_command_tag(query, affected);
                self.send_command_complete(&tag).await?;
            } else {
                // Derive schema from plan for proper column names
                let schema = self.derive_returning_schema(query).unwrap_or_else(|_| {
                    if let Some(first) = tuples.first() {
                        first.schema()
                    } else {
                        Schema::new(vec![])
                    }
                });
                self.send_query_result(schema, &tuples).await?;
            }
        } else {
            let affected = self.database.execute_for_session(self.session_id, query)?;
            let tag = self.get_command_tag(query, affected);
            self.send_command_complete(&tag).await?;
        }

        self.send_ready_for_query().await?;
        Ok(())
    }

    fn schema_from_query_columns(columns: &[String], rows: &[Tuple]) -> Schema {
        if !columns.is_empty() {
            Schema::new(
                columns
                    .iter()
                    .enumerate()
                    .map(|(i, name)| {
                        let data_type = rows
                            .first()
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
                    })
                    .collect(),
            )
        } else if !rows.is_empty() {
            rows[0].schema()
        } else {
            Schema::new(vec![])
        }
    }

    // Extended protocol methods are in handler_extended.rs module

    /// Handle SCRAM-SHA-256 authentication flow
    // SAFETY: parts[1] and parts[2] are guarded by parts.len() >= 3 check.
    #[allow(clippy::indexing_slicing)]
    async fn handle_scram_authentication(&mut self) -> Result<()> {
        // Send AuthenticationSASL with SCRAM-SHA-256 mechanism
        self.send_message(BackendMessage::Authentication(AuthenticationMessage::ScramSha256))
            .await?;
        self.flush().await?; // Client must read challenge before responding

        // Wait for client-first-message (SASL initial response)
        // BUG-003 (restored iter-72): accept the parser's SaslInitialResponse
        // variant. Older handler only matched PasswordMessage, which read the
        // SCRAM body as if it were null-terminated and lost everything after
        // "SCRAM-SHA-256" — every libpq client then failed
        // `parse_scram_client_first` with "missing GS2 authzid slot".
        let client_first = match self.read_message().await? {
            Some(FrontendMessage::SaslInitialResponse { mechanism: _, data }) => String::from_utf8(data)
                .map_err(|e| Error::protocol(format!("SCRAM client-first-message not UTF-8: {}", e)))?,
            Some(FrontendMessage::PasswordMessage { password }) => password,
            _ => return Err(Error::protocol("Expected SASL initial response")),
        };

        tracing::debug!("Received client-first-message: {}", client_first);

        // Parse client-first-message per RFC 5802. Bug 2: the previous
        // parser ignored the GS2 header and misaligned every offset for
        // real clients (libpq, asyncpg, node-postgres, JDBC). Now
        // delegated to `auth::parse_scram_client_first`.
        // BUG-003 (Perf-73): per RFC 5802 + the Postgres SCRAM profile, the
        // SCRAM `n=` field is empty. Source the real username from the
        // StartupMessage that was stored on `self.username` during the
        // startup phase.
        let (_scram_user_unused, client_nonce_owned) = super::auth::parse_scram_client_first(&client_first)?;
        let username_owned = self
            .username
            .clone()
            .ok_or_else(|| Error::authentication("SCRAM authentication started without a startup-time user name"))?;
        let username: &str = &username_owned;
        let client_nonce: &str = &client_nonce_owned;

        tracing::info!("SCRAM authentication for user: {}", username);

        // Get user credentials from password store
        let password_store = self
            .auth_manager
            .password_store()
            .ok_or_else(|| Error::authentication("SCRAM password store not configured"))?;

        let credentials = password_store
            .get_credentials(username)
            .ok_or_else(|| Error::authentication("User not found"))?;

        // Create SCRAM state
        let mut scram_state = ScramAuthState::new(username.to_string());
        scram_state.set_client_nonce(client_nonce.to_string());

        // Build client-first-message-bare for auth message.
        // BUG-003 (Perf-73): MUST match exactly what the client sent (empty
        // `n=` per the Postgres SCRAM profile + RFC 5802) — the SCRAM proof
        // verification hashes this string. Reconstructing with the startup
        // user breaks the proof and yields "Invalid password".
        let client_first_bare = format!("n=,r={}", client_nonce);
        scram_state.set_client_first_message_bare(client_first_bare);

        // Generate server-first-message
        let server_first = scram_state.build_server_first_message()?;

        tracing::debug!("Sending server-first-message: {}", server_first);

        // Send AuthenticationSASLContinue with server-first-message
        self.send_message(BackendMessage::Authentication(
            AuthenticationMessage::ScramSha256Continue {
                data: server_first.as_bytes().to_vec(),
            },
        ))
        .await?;
        self.flush().await?; // Client must read continue before responding

        // Wait for client-final-message. BUG-003 (Perf-73): SCRAM client-final
        // arrives as a SASLResponse (no mechanism prefix), not a PasswordMessage.
        let client_final = match self.read_message().await? {
            Some(FrontendMessage::SaslResponse { data }) => String::from_utf8(data)
                .map_err(|e| Error::protocol(format!("SCRAM client-final-message not UTF-8: {}", e)))?,
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
        let proof_part = final_parts
            .iter()
            .find(|p| p.starts_with("p="))
            .ok_or_else(|| Error::protocol("Missing proof in client-final-message"))?;
        let client_proof_b64 = proof_part
            .strip_prefix("p=")
            .ok_or_else(|| Error::protocol("Invalid proof format"))?;

        // Build client-final-message-without-proof
        let client_final_without_proof: Vec<&str> =
            final_parts.iter().filter(|p| !p.starts_with("p=")).copied().collect();
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
            },
        ))
        .await?;

        // Authentication successful
        self.authenticated = true;
        self.username = Some(username.to_string());

        Ok(())
    }

    /// Send query results
    async fn send_query_result(&mut self, schema: Schema, rows: &[Tuple]) -> Result<()> {
        // Send RowDescription
        let fields = schema_to_field_descriptions(&schema);
        self.send_message(BackendMessage::RowDescription { fields }).await?;

        self.send_data_rows_direct(rows).await?;

        // Send CommandComplete
        let tag = format!("SELECT {}", rows.len());
        self.send_command_complete(&tag).await?;

        Ok(())
    }

    /// Extended-protocol DataRow emission honouring the portal's result
    /// formats (R5.W1): all-text format requests stream through the direct
    /// encoder; any binary format code falls back to the legacy per-row
    /// conversion path, where the existing binary value encoders
    /// (`value_to_pg_binary`) live. Wire output is identical either way for
    /// text formats — both encoders share the same per-type text forms.
    pub(super) async fn send_data_rows_with_formats(&mut self, rows: &[Tuple], result_formats: &[i16]) -> Result<()> {
        if formats_request_text_only(result_formats) {
            return self.send_data_rows_direct(rows).await;
        }
        for row in rows {
            let values = tuple_to_pg_values_with_formats(row, result_formats);
            self.send_message(BackendMessage::DataRow { values }).await?;
        }
        Ok(())
    }

    /// Stream DataRows through the direct Tuple encoder (text format).
    ///
    /// Encodes rows into chunks before writing — keeps the zero-clone
    /// per-value encoding of [`Self::encode_data_row_direct`] while avoiding
    /// one async write per result row. Used by the simple-query path and,
    /// since R5.W1, by extended-protocol Execute whenever every requested
    /// result format is text (format code 0) — which is what mainstream
    /// drivers request.
    pub(super) async fn send_data_rows_direct(&mut self, rows: &[Tuple]) -> Result<()> {
        const DATA_ROW_FLUSH_AT: usize = 64 * 1024;
        self.write_buf.clear();
        for row in rows {
            self.encode_data_row_direct(row);
            if self.write_buf.len() >= DATA_ROW_FLUSH_AT {
                self.stream
                    .write_all(&self.write_buf)
                    .await
                    .map_err(|e| Error::network(format!("Failed to send query rows: {}", e)))?;
                self.write_buf.clear();
            }
        }
        if !self.write_buf.is_empty() {
            self.stream
                .write_all(&self.write_buf)
                .await
                .map_err(|e| Error::network(format!("Failed to send query rows: {}", e)))?;
            self.write_buf.clear();
        }
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
            let values: Vec<Option<Vec<u8>>> = row.iter().map(|v| v.as_ref().map(|s| s.as_bytes().to_vec())).collect();
            self.send_message(BackendMessage::DataRow { values }).await?;
        }

        // Send CommandComplete
        let tag = format!("SELECT {}", rows.len());
        self.send_command_complete(&tag).await?;

        Ok(())
    }

    /// Send a backend message (write only, no flush).
    /// Caller is responsible for flushing at the end of a response cycle.
    /// COPY ... FROM STDIN wire sub-protocol (v3.58 item 2c). Sends
    /// CopyInResponse, drains CopyData frames until CopyDone/CopyFail, parses
    /// the text rows, and bulk-inserts them in batches. TO STDOUT and non-text
    /// formats return a clear error for now (added in a 2c follow-up). Row
    /// decoding + injection-safe INSERT construction live in the unit-tested
    /// `copy` module.
    async fn handle_copy(&mut self, copy: super::copy::CopyStatement) -> Result<()> {
        use super::copy::CopyFormat;
        if copy.format == CopyFormat::Binary {
            return self
                .send_error("ERROR", "0A000", "COPY binary format is not yet supported", None, None)
                .await;
        }
        if copy.to_stdout {
            return self.handle_copy_to_stdout(&copy).await;
        }

        // Ready to receive: text format (overall_format = 0).
        let ncols = copy.columns.len();
        self.send_message(BackendMessage::CopyInResponse {
            overall_format: 0,
            column_formats: vec![0i16; ncols],
        })
        .await?;
        // The client must SEE CopyInResponse before it will send any CopyData,
        // but send_message only writes into the BufWriter. Flush now — exactly
        // like the auth-challenge path ("Client must read challenge before
        // responding") — or both sides deadlock: client waits for 'G', server
        // waits for CopyData. (Found by Proxy live validation of 2c.)
        self.flush().await?;

        // Drain CopyData frames until CopyDone / CopyFail.
        let mut data: Vec<u8> = Vec::new();
        let mut client_fail: Option<String> = None;
        loop {
            match self.read_message().await? {
                Some(FrontendMessage::CopyData(chunk)) => data.extend_from_slice(&chunk),
                Some(FrontendMessage::CopyDone) => break,
                Some(FrontendMessage::CopyFail(msg)) => {
                    client_fail = Some(msg);
                    break;
                }
                // Connection ending mid-copy: abort without inserting.
                None | Some(FrontendMessage::Terminate) => return Ok(()),
                Some(_) => {
                    return self
                        .send_error(
                            "ERROR",
                            "08P01",
                            "unexpected message during COPY FROM STDIN",
                            None,
                            None,
                        )
                        .await;
                }
            }
        }
        if let Some(msg) = client_fail {
            return self
                .send_error("ERROR", "57014", &format!("COPY from stdin failed: {msg}"), None, None)
                .await;
        }

        // Parse rows (text or CSV) and bulk-insert in batches.
        let rows = if copy.format == CopyFormat::Csv {
            super::copy::parse_csv_rows(&data)
        } else {
            super::copy::parse_text_rows(&data)
        };
        let total = rows.len();
        const BATCH: usize = 500;
        for chunk in rows.chunks(BATCH) {
            if let Some(sql) = super::copy::build_insert_sql(&copy.table, &copy.columns, chunk) {
                if let Err(e) = self.database.execute(&sql) {
                    return self
                        .send_error("ERROR", "XX000", &format!("COPY insert failed: {e}"), None, None)
                        .await;
                }
            }
        }
        self.send_command_complete(&format!("COPY {total}")).await?;
        self.send_ready_for_query().await
    }

    /// COPY ... TO STDOUT (text format) — SELECT the rows and stream each as a
    /// CopyData frame. The server only sends here (never waits on the client
    /// mid-stream), so no intermediate flush is needed; the trailing
    /// ReadyForQuery flushes the whole sequence.
    async fn handle_copy_to_stdout(&mut self, copy: &super::copy::CopyStatement) -> Result<()> {
        let cols_sql = if copy.columns.is_empty() {
            "*".to_string()
        } else {
            copy.columns
                .iter()
                .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let sql = format!("SELECT {} FROM \"{}\"", cols_sql, copy.table.replace('"', "\"\""));
        let (rows, columns) = match self
            .database
            .query_with_columns_for_session(self.session_id, &sql)
        {
            Ok(r) => r,
            Err(e) => {
                return self
                    .send_error("ERROR", "XX000", &format!("COPY TO STDOUT failed: {e}"), None, None)
                    .await;
            }
        };
        let ncols = columns.len();
        self.send_message(BackendMessage::CopyOutResponse {
            overall_format: 0,
            column_formats: vec![0i16; ncols],
        })
        .await?;
        let total = rows.len();
        for row in &rows {
            let fields = tuple_to_pg_values(row);
            let line = if copy.format == super::copy::CopyFormat::Csv {
                super::copy::encode_csv_row(&fields)
            } else {
                super::copy::encode_text_row(&fields)
            };
            self.send_message(BackendMessage::CopyData(line)).await?;
        }
        self.send_message(BackendMessage::CopyDone).await?;
        self.send_command_complete(&format!("COPY {total}")).await?;
        self.send_ready_for_query().await
    }

    pub(super) async fn send_message(&mut self, msg: BackendMessage) -> Result<()> {
        self.write_buf.clear();
        msg.encode(&mut self.write_buf);
        self.stream
            .write_all(&self.write_buf)
            .await
            .map_err(|e| Error::network(format!("Failed to send message: {}", e)))?;
        Ok(())
    }

    /// Encode a DataRow directly from a Tuple, avoiding intermediate Vec allocations.
    /// Uses length-prefix backpatching: writes placeholder, encodes values, then patches the length.
    #[allow(clippy::indexing_slicing)] // length_pos and count_pos are set by us, always valid
    fn encode_data_row_direct(&mut self, tuple: &Tuple) {
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
                    // bytea text format is `\x<hex>` (PostgreSQL bytea_output=hex).
                    // Raw bytes here make libpq/psycopg2 un-escape the field and
                    // drop any 0x5C (backslash) byte — see tuple_to_pg_values.
                    let hex = hex::encode(b);
                    self.write_buf.put_i32((2 + hex.len()) as i32);
                    self.write_buf.put_slice(b"\\x");
                    self.write_buf.put_slice(hex.as_bytes());
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
                        if i > 0 {
                            self.write_buf.put_u8(b',');
                        }
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
                        if i > 0 {
                            self.write_buf.put_u8(b',');
                        }
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
    }

    /// Flush all buffered writes to the client.
    async fn flush(&mut self) -> Result<()> {
        self.stream
            .flush()
            .await
            .map_err(|e| Error::network(format!("Failed to flush stream: {}", e)))
    }

    /// Send authentication OK
    async fn send_auth_ok(&mut self) -> Result<()> {
        self.send_message(BackendMessage::Authentication(AuthenticationMessage::Ok))
            .await
    }

    /// Send parameter status
    async fn send_parameter_status(&mut self, name: &str, value: &str) -> Result<()> {
        self.send_message(BackendMessage::ParameterStatus {
            name: name.to_string(),
            value: value.to_string(),
        })
        .await
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
        // Strip optional `BEGIN`/`END` wrapper tokens.
        let stripped = pg_strip_begin_end(body.trim());

        // KanttBan #14 (v3.29.0): idempotent EXCEPTION handler. Drizzle
        // wraps every ALTER in
        //   DO $$ BEGIN <stmt>; EXCEPTION WHEN duplicate_object THEN null; END $$;
        // Parse the body into (main, exception_codes); swallow only
        // errors whose message matches the named exception conditions.
        // This is intentionally narrow — full PL/pgSQL control flow
        // still errors via `pg_detect_plpgsql` below.
        let (main_body, exception_codes) = pg_split_exception(stripped);

        if let Some(kw) = pg_detect_plpgsql(main_body) {
            return Err(Error::query_execution(format!(
                "PL/pgSQL control flow (`{kw}`) inside DO blocks is not yet \
                 supported in HeliosDB Nano. Rewrite the block as plain SQL, \
                 or execute each statement separately. \
                 See: docs/compatibility/plpgsql.md"
            )));
        }

        let statements = pg_split_sql_respecting_quotes(main_body);

        if statements.is_empty() {
            self.send_command_complete("DO").await?;
            self.send_ready_for_query().await?;
            return Ok(());
        }
        let prev = self.suppress_ready_for_query;
        self.suppress_ready_for_query = true;
        for stmt in &statements {
            if let Err(e) = self.database.execute_for_session(self.session_id, stmt) {
                if pg_exception_matches(&exception_codes, &e.to_string()) {
                    tracing::debug!("DO block: caught {:?} via EXCEPTION clause; continuing", e.to_string());
                    continue;
                }
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
        })
        .await?;
        self.flush().await
    }

    /// Send command complete
    pub(super) async fn send_command_complete(&mut self, tag: &str) -> Result<()> {
        self.send_message(BackendMessage::CommandComplete { tag: tag.to_string() })
            .await
    }

    /// Send error response (ErrorResponse only, no trailing ReadyForQuery)
    async fn send_error_message(
        &mut self,
        severity: &str,
        code: &str,
        message: &str,
        detail: Option<String>,
        hint: Option<String>,
    ) -> Result<()> {
        self.send_message(BackendMessage::ErrorResponse {
            severity: severity.to_string(),
            code: code.to_string(),
            message: message.to_string(),
            detail,
            hint,
            position: None,
        })
        .await
    }

    /// Send error response followed by ReadyForQuery (simple-query path)
    async fn send_error(
        &mut self,
        severity: &str,
        code: &str,
        message: &str,
        detail: Option<String>,
        hint: Option<String>,
    ) -> Result<()> {
        self.mark_transaction_failed_after_error();
        self.send_error_message(severity, code, message, detail, hint).await?;
        self.send_ready_for_query().await
    }

    async fn send_error_for_query(&mut self, error: &Error, wait_for_sync: bool) -> Result<()> {
        self.mark_transaction_failed_after_error();
        self.suppress_ready_for_query = false;
        let code = sqlstate_for_error(error);
        let (detail, hint) = detail_hint_for_error(code, error);
        if wait_for_sync {
            // Extended-query protocol: emit ErrorResponse now and defer
            // ReadyForQuery until the client's Sync, discarding messages in
            // between. Sending ReadyForQuery early closes/wedges drivers.
            self.send_error_message("ERROR", code, &error.to_string(), detail, hint)
                .await?;
            self.flush().await
        } else {
            self.send_error("ERROR", code, &error.to_string(), detail, hint).await
        }
    }

    fn mark_transaction_failed_after_error(&mut self) {
        if self.transaction_status == TransactionStatus::InTransaction {
            self.transaction_status = TransactionStatus::Failed;
        }
    }

    fn rollback_failed_transaction_for_recovery(&mut self) -> Result<()> {
        if self.transaction_status == TransactionStatus::Failed {
            if self.database.session_in_transaction(self.session_id) {
                self.database.rollback_transaction_for_session(self.session_id)?;
            }
            self.transaction_status = TransactionStatus::Idle;
        }
        Ok(())
    }

    pub(super) fn transaction_failed(&self) -> bool {
        self.transaction_status == TransactionStatus::Failed
    }

    pub(super) async fn send_extended_failed_transaction_error(&mut self) -> Result<()> {
        self.suppress_ready_for_query = false;
        self.awaiting_sync_after_error = true;
        self.send_error_message(
            "ERROR",
            "25P02",
            "current transaction is aborted, commands ignored until end of transaction block",
            None,
            Some("Use ROLLBACK to clear the failed transaction state".to_string()),
        )
        .await?;
        self.flush().await
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
        let planner = crate::sql::planner::Planner::with_catalog(&catalog).with_sql(sql.to_string());
        let (statement, _) = self.database.parse_cached(sql)?;
        let plan = planner.statement_to_plan(statement)?;

        // Extract table name and returning items from the plan
        let (table_name, returning_items) = match &plan {
            crate::sql::LogicalPlan::Insert {
                table_name, returning, ..
            }
            | crate::sql::LogicalPlan::InsertSelect {
                table_name, returning, ..
            } => (table_name.as_str(), returning.as_ref()),
            crate::sql::LogicalPlan::Update {
                table_name, returning, ..
            } => (table_name.as_str(), returning.as_ref()),
            crate::sql::LogicalPlan::Delete {
                table_name, returning, ..
            } => (table_name.as_str(), returning.as_ref()),
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
            "transaction_isolation" | "transaction isolation level" => "read committed".to_string(),
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

    /// Item 5: classify a `DISCARD …` statement, returning the canonical
    /// target (`"ALL" | "PLANS" | "SEQUENCES" | "TEMP"`) or `None` when this is
    /// not a recognized DISCARD form (so it falls through to normal parsing).
    fn parse_discard_target(trimmed: &str) -> Option<&'static str> {
        let rest = trimmed.trim_end_matches(';').trim();
        if !starts_with_icase(rest, "DISCARD ") {
            return None;
        }
        let arg = rest[8..].trim(); // "DISCARD ".len() == 8 (ASCII)
        if arg.eq_ignore_ascii_case("ALL") {
            Some("ALL")
        } else if arg.eq_ignore_ascii_case("PLANS") {
            Some("PLANS")
        } else if arg.eq_ignore_ascii_case("SEQUENCES") {
            Some("SEQUENCES")
        } else if arg.eq_ignore_ascii_case("TEMP") || arg.eq_ignore_ascii_case("TEMPORARY") {
            Some("TEMP")
        } else {
            None
        }
    }

    fn parse_isolation_level(query: &str) -> Option<String> {
        // Find "ISOLATION LEVEL" case-insensitively without allocating
        let query_bytes = query.as_bytes();
        let needle = b"ISOLATION LEVEL";
        let pos = query_bytes
            .windows(needle.len())
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
    schema
        .columns
        .iter()
        .map(|col| {
            FieldDescription {
                name: col.name.clone(),
                table_oid: 0,
                column_attr_num: 0,
                data_type_oid: datatype_to_oid(&col.data_type),
                data_type_size: datatype_to_size(&col.data_type),
                type_modifier: -1,
                format_code: 0, // text format
            }
        })
        .collect()
}

/// Convert Schema to FieldDescriptions, honoring requested result formats
/// where this wire encoder has a matching binary representation.
pub(super) fn schema_to_field_descriptions_with_formats(
    schema: &Schema,
    result_formats: &[i16],
) -> Vec<FieldDescription> {
    schema
        .columns
        .iter()
        .enumerate()
        .map(|(index, col)| FieldDescription {
            name: col.name.clone(),
            table_oid: 0,
            column_attr_num: 0,
            data_type_oid: datatype_to_oid(&col.data_type),
            data_type_size: datatype_to_size(&col.data_type),
            type_modifier: -1,
            format_code: effective_result_format(&col.data_type, result_formats, index),
        })
        .collect()
}

/// True when the portal's result-format codes request text for every column
/// (empty list, all zeros, or the single-`0` shorthand) — the precondition
/// for routing extended-protocol rows through the direct encoder (R5.W1).
pub(super) fn formats_request_text_only(result_formats: &[i16]) -> bool {
    result_formats.iter().all(|f| *f == 0)
}

pub(super) fn requested_result_format(result_formats: &[i16], column_index: usize) -> i16 {
    if result_formats.is_empty() {
        0
    } else if result_formats.len() == 1 {
        result_formats[0]
    } else {
        result_formats.get(column_index).copied().unwrap_or(0)
    }
}

fn effective_result_format(data_type: &crate::DataType, result_formats: &[i16], column_index: usize) -> i16 {
    let requested = requested_result_format(result_formats, column_index);
    if requested == 1 && datatype_has_binary_result(data_type) {
        1
    } else {
        0
    }
}

fn datatype_has_binary_result(data_type: &crate::DataType) -> bool {
    matches!(
        data_type,
        crate::DataType::Boolean
            | crate::DataType::Int2
            | crate::DataType::Int4
            | crate::DataType::Int8
            | crate::DataType::Float4
            | crate::DataType::Float8
            | crate::DataType::Bytea
            | crate::DataType::Text
            | crate::DataType::Varchar(_)
            | crate::DataType::Uuid
    )
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
        // Arbitrary-precision numeric. Must advertise the real `numeric` OID
        // (1700), not 705/unknown — otherwise clients (psycopg, JDBC, …)
        // receive numeric columns as untyped strings and skip decimal parsing,
        // breaking value fidelity for migration/validation tools.
        crate::DataType::Numeric => 1700,
        crate::DataType::Text => 25,
        crate::DataType::Varchar(_) => 1043,
        crate::DataType::Char(_) => 1042, // bpchar
        crate::DataType::Bytea => 17,
        crate::DataType::Json => 114,
        crate::DataType::Jsonb => 3802,
        crate::DataType::Timestamp => 1114,
        crate::DataType::Timestamptz => 1184,
        crate::DataType::Interval => 1186,
        crate::DataType::Date => 1082,
        crate::DataType::Time => 1083,
        crate::DataType::Uuid => 2950,
        crate::DataType::Vector(_) => 1000, // Custom type
        _ => 705,                           // Unknown
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
    tuple
        .values
        .iter()
        .map(|val| {
            match val {
                Value::Null => None,
                Value::Boolean(b) => Some(if *b { b"t" } else { b"f" }.to_vec()),
                Value::Int2(i) => Some(itoa::Buffer::new().format(*i).as_bytes().to_vec()),
                Value::Int4(i) => Some(itoa::Buffer::new().format(*i).as_bytes().to_vec()),
                Value::Int8(i) => Some(itoa::Buffer::new().format(*i).as_bytes().to_vec()),
                Value::Float4(f) => Some(ryu::Buffer::new().format(*f).as_bytes().to_vec()),
                Value::Float8(f) => Some(ryu::Buffer::new().format(*f).as_bytes().to_vec()),
                Value::String(s) => Some(s.as_bytes().to_vec()),
                // bytea text format is `\x<hex>` (PostgreSQL bytea_output=hex).
                // Emitting the raw bytes makes libpq/psycopg2 apply
                // escape-format un-escaping on the field, which silently drops
                // any 0x5C (backslash) byte — corrupting BLOB/RAW round-trips.
                Value::Bytes(b) => {
                    let mut out = Vec::with_capacity(2 + b.len() * 2);
                    out.extend_from_slice(b"\\x");
                    out.extend_from_slice(hex::encode(b).as_bytes());
                    Some(out)
                }
                Value::Json(j) => Some(j.to_string().into_bytes()),
                Value::Numeric(n) => Some(n.as_bytes().to_vec()),
                Value::Uuid(u) => Some(u.to_string().into_bytes()),
                // PostgreSQL timestamp wire format: microsecond precision,
                // space separator, no trailing timezone on `timestamp without
                // time zone` columns. psycopg's native parser rejects
                // `rfc3339` nanosecond output ("timestamp too large (after
                // year 10K)") — PG itself truncates to 6 fractional digits.
                Value::Timestamp(ts) => Some(ts.naive_utc().format("%Y-%m-%d %H:%M:%S%.6f").to_string().into_bytes()),
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
                        if i > 0 {
                            buf.push(',');
                        }
                        match v {
                            Value::String(s) => {
                                buf.push('"');
                                buf.push_str(s);
                                buf.push('"');
                            }
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
                        if i > 0 {
                            buf.push(',');
                        }
                        buf.push_str(ryu_buf.format(*x));
                    }
                    buf.push('}');
                    Some(buf.into_bytes())
                }
                Value::DictRef { dict_id } => Some(itoa::Buffer::new().format(*dict_id).as_bytes().to_vec()),
                Value::CasRef { hash } => Some(hex::encode(hash).into_bytes()),
                Value::ColumnarRef => Some(b"<columnar>".to_vec()),
            }
        })
        .collect()
}

/// Convert Tuple to PostgreSQL wire-format values using portal result formats.
pub(super) fn tuple_to_pg_values_with_formats(tuple: &Tuple, result_formats: &[i16]) -> Vec<Option<Vec<u8>>> {
    tuple
        .values
        .iter()
        .enumerate()
        .map(|(index, val)| match val {
            Value::Null => None,
            _ if requested_result_format(result_formats, index) == 1 => {
                value_to_pg_binary(val).or_else(|| single_value_to_pg_text(val))
            }
            _ => single_value_to_pg_text(val),
        })
        .collect()
}

fn single_value_to_pg_text(value: &Value) -> Option<Vec<u8>> {
    let tuple = Tuple::new(vec![value.clone()]);
    tuple_to_pg_values(&tuple).into_iter().next().flatten()
}

fn value_to_pg_binary(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::Boolean(value) => Some(vec![u8::from(*value)]),
        Value::Int2(value) => Some(value.to_be_bytes().to_vec()),
        Value::Int4(value) => Some(value.to_be_bytes().to_vec()),
        Value::Int8(value) => Some(value.to_be_bytes().to_vec()),
        Value::Float4(value) => Some(value.to_be_bytes().to_vec()),
        Value::Float8(value) => Some(value.to_be_bytes().to_vec()),
        Value::String(value) => Some(value.as_bytes().to_vec()),
        Value::Bytes(value) => Some(value.clone()),
        Value::Uuid(value) => Some(value.as_bytes().to_vec()),
        _ => None,
    }
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
                    for c in tag.chars() {
                        current.push(c);
                    }
                    current.push('$');
                    i += 1 + end + 1;
                    in_dollar = Some(tag.to_string());
                    continue;
                }
            }
        }
        match b {
            b'\'' => {
                in_single_quote = true;
                current.push('\'');
            }
            b';' => {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() && !pg_stmt_is_only_comment(&trimmed) {
                    statements.push(trimmed);
                }
                current.clear();
            }
            _ => current.push(b as char),
        }
        i += 1;
    }
    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() && !pg_stmt_is_only_comment(&trimmed) {
        statements.push(trimmed);
    }
    statements
}

/// KanttBan #23 phase 2.7: return true if `stmt` (already trimmed)
/// contains nothing but SQL line comments (`-- …`) and whitespace.
/// drizzle-kit's getColumnsInfoQuery ends with
/// `ORDER BY a.attnum;  -- Order by column number` — splitting on
/// `;` left the trailing `-- …` as a "statement" which sqlparser
/// rejected with "No SQL statement found". Skipping comment-only
/// segments at the splitter level mirrors PG's tolerance for
/// trailing comments after a terminating semicolon.
fn pg_stmt_is_only_comment(stmt: &str) -> bool {
    for line in stmt.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !trimmed.starts_with("--") {
            return false;
        }
    }
    true
}

/// Detect `DO $$ … $$` / `DO [LANGUAGE plpgsql] $tag$ … $tag$;` blocks
/// at statement level so we can strip the wrapper and execute the
/// plain-SQL body statement-by-statement.
fn pg_looks_like_do_block(trimmed: &str) -> bool {
    let upper = trimmed.trim_start().to_ascii_uppercase();
    upper.starts_with("DO $") || upper.starts_with("DO LANGUAGE ")
}

/// Split a DO-block body into its main BEGIN body and the list of
/// exception names declared in the trailing `EXCEPTION` clause.
///
/// drizzle-kit emits this exact shape and nothing else for the
/// EXCEPTION case — we don't try to parse the THEN action since
/// it's always `null` (a no-op).
/// KanttBan #23 (v3.31.1 phase 2.2): rewrite PG's `SELECT FROM …`
/// empty-projection shorthand to `SELECT 1 FROM …` so sqlparser-rs
/// can parse it. drizzle-kit's getColumnsInfoQuery uses the empty
/// form inside `EXISTS (SELECT FROM pg_attrdef ad WHERE …)` — valid
/// in PG, rejected by our parser.
///
/// Scope: only matches `SELECT` followed by whitespace then `FROM`.
/// `SELECT *` / `SELECT col` / `SELECT 1` are untouched. Comments and
/// string literals containing `SELECT FROM` would be falsely rewritten
/// — that's a known sharp edge but no real driver query has that
/// shape inside a string. The substitution is idempotent and case-
/// preserving on the matched tokens.
pub(crate) fn pg_rewrite_empty_projection(query: &str) -> String {
    let needle_lower = "select from ";
    let mut out = String::with_capacity(query.len() + 16);
    let mut remaining = query;
    loop {
        let lower = remaining.to_ascii_lowercase();
        match lower.find(needle_lower) {
            None => {
                out.push_str(remaining);
                break;
            }
            Some(idx) => {
                let after = idx + needle_lower.len();
                // Preserve the original case of "SELECT" / "select" / "FROM"
                // by slicing remaining (the actual text) rather than `lower`.
                out.push_str(&remaining[..idx]);
                out.push_str(&remaining[idx..idx + 6]); // "SELECT" / "select"
                out.push_str(" 1 ");
                out.push_str(&remaining[idx + 7..after]); // "FROM " / "from "
                remaining = &remaining[after..];
            }
        }
    }
    out
}

fn pg_split_exception(body: &str) -> (&str, Vec<String>) {
    let upper = body.to_ascii_uppercase();
    // KanttBan #14 (v3.30.1 smoke): the previous needle `" EXCEPTION "`
    // only matched space-bounded EXCEPTION. drizzle-kit emits a
    // multi-line shape with a newline before `EXCEPTION`, which
    // slipped past the split — `pg_detect_plpgsql` then ran on the
    // un-stripped body and (after we taught it to ignore IF NOT EXISTS)
    // the unstripped EXCEPTION clause leaked through to the SQL
    // splitter, which fed `EXCEPTION WHEN duplicate_object …` to
    // sqlparser as a top-level statement and parse-errored.
    //
    // Find the standalone `EXCEPTION` keyword by checking for any
    // ASCII whitespace immediately before and after it.
    let offset = upper
        .match_indices("EXCEPTION")
        .find(|(idx, _)| {
            let before_ok = *idx == 0
                || upper
                    .as_bytes()
                    .get(*idx - 1)
                    .copied()
                    .is_some_and(|b| b.is_ascii_whitespace());
            let after_pos = *idx + "EXCEPTION".len();
            let after_ok = after_pos == upper.len()
                || upper
                    .as_bytes()
                    .get(after_pos)
                    .copied()
                    .is_some_and(|b| b.is_ascii_whitespace());
            before_ok && after_ok
        })
        .map(|(idx, _)| idx);
    let Some(offset) = offset else {
        return (body, Vec::new());
    };
    let main = &body[..offset];
    let exception_block = &body[offset + "EXCEPTION".len()..];

    // Collect every identifier following a `WHEN` keyword. Multiple
    // exceptions can be OR'd: `WHEN a OR b THEN …`. We don't try to
    // parse `THEN <stmt>` — the action is implied no-op for our use.
    let mut codes = Vec::new();
    let upper_eb = exception_block.to_ascii_uppercase();
    let mut search_from = 0;
    while let Some(rel) = upper_eb[search_from..].find("WHEN") {
        let start = search_from + rel + "WHEN".len();
        // Read identifiers until THEN.
        let after = &upper_eb[start..];
        let then_pos = after.find("THEN").unwrap_or(after.len());
        let conditions = &after[..then_pos];
        for name in conditions.split(|c: char| c == ',' || c == '|' || c == 'O' && false) {
            // Split conservatively on commas, tabs, OR (matched as an identifier).
            for token in name.split_whitespace() {
                let cleaned: String = token
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if cleaned.is_empty() || cleaned.eq_ignore_ascii_case("OR") {
                    continue;
                }
                codes.push(cleaned.to_lowercase());
            }
        }
        search_from = start + then_pos.min(after.len());
        if search_from >= upper_eb.len() {
            break;
        }
    }
    (main, codes)
}

/// Return true if `error_message` matches any of the named PG exception
/// conditions in `codes`. Only the codes drizzle-kit actually emits
/// are mapped — others fall through (safer to surface unknown errors
/// than to silently swallow them).
fn pg_exception_matches(codes: &[String], error_message: &str) -> bool {
    if codes.is_empty() {
        return false;
    }
    let lower = error_message.to_ascii_lowercase();
    for code in codes {
        let matches = match code.as_str() {
            // "already exists" family (drizzle's idempotent ADD)
            "duplicate_object" | "duplicate_table" | "duplicate_column" | "duplicate_database" | "duplicate_schema"
            | "duplicate_function" | "duplicate_alias" => lower.contains("already exists"),
            "unique_violation" => lower.contains("unique") || lower.contains("duplicate key"),
            "undefined_table" | "undefined_object" | "undefined_column" | "undefined_function" => {
                lower.contains("does not exist") || lower.contains("not found")
            }
            // OTHERS catches everything — drizzle uses this rarely.
            "others" => true,
            _ => false,
        };
        if matches {
            return true;
        }
    }
    false
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
    // KanttBan #14 (v3.30.1): strip the SQL DDL idioms `IF NOT EXISTS`
    // / `IF EXISTS` before scanning. drizzle-kit emits
    //   DO $$ BEGIN CREATE TABLE IF NOT EXISTS … END $$;
    // and the literal `IF` inside that DDL would otherwise trip the
    // PL/pgSQL `IF` detector below — even though the body is plain SQL.
    let scrubbed = upper.replace(" IF NOT EXISTS ", " ").replace(" IF EXISTS ", " ");
    // Whole-word matches — bracket each keyword with a non-alnum
    // lookalike by padding the haystack.
    let padded = format!(" {scrubbed} ");
    // Note: `EXCEPTION` intentionally absent — handled via
    // `pg_split_exception` upstream. drizzle-kit emits idempotent
    // ALTERs as `DO $$ BEGIN <stmt>; EXCEPTION WHEN duplicate_object
    // THEN null; END $$;` which we now run.
    const KEYWORDS: &[&str] = &[
        " DECLARE ",
        " IF ",
        " LOOP ",
        " FOR ",
        " WHILE ",
        " RAISE ",
        " RETURN ",
        " PERFORM ",
        " EXIT ",
        " CONTINUE ",
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

#[cfg(test)]
mod plpgsql_detection_tests {
    use super::pg_detect_plpgsql;

    #[test]
    fn create_table_if_not_exists_is_plain_sql() {
        // KanttBan #14 (v3.30.1): drizzle-kit wraps DDL in
        //   DO $$ BEGIN CREATE TABLE IF NOT EXISTS … END $$;
        // The IF in IF NOT EXISTS must NOT be flagged as PL/pgSQL.
        assert_eq!(pg_detect_plpgsql("CREATE TABLE IF NOT EXISTS foo (id integer);"), None,);
        assert_eq!(pg_detect_plpgsql("DROP TABLE IF EXISTS foo;"), None,);
        assert_eq!(pg_detect_plpgsql("create index if not exists ix_foo on foo(id);"), None,);
    }

    #[test]
    fn real_plpgsql_if_still_detected() {
        // A genuine PL/pgSQL IF block must still be flagged.
        assert_eq!(pg_detect_plpgsql("IF x > 0 THEN PERFORM 1; END IF;"), Some("IF"),);
    }

    #[test]
    fn other_plpgsql_keywords_still_detected() {
        assert_eq!(pg_detect_plpgsql("LOOP exit; END LOOP;"), Some("LOOP"));
        assert_eq!(pg_detect_plpgsql("RAISE NOTICE 'hi';"), Some("RAISE"));
        assert_eq!(pg_detect_plpgsql("DECLARE v integer;"), Some("DECLARE"));
    }
}

#[cfg(test)]
mod datatype_oid_tests {
    use super::datatype_to_oid;
    use crate::DataType;

    /// Numeric/decimal columns MUST advertise the real `numeric` OID (1700),
    /// not 705/unknown. Regression for the Any2HeliosDB Oracle→Nano data-fidelity
    /// gap: with OID 705, psycopg returns numeric columns as untyped strings, so
    /// `NUMBER(10,2)` values like 98000.0 fail checksum comparison against the
    /// source (which normalizes the decimal). See a2h test-data EMPLOYEES.
    #[test]
    fn numeric_advertises_numeric_oid_not_unknown() {
        assert_eq!(datatype_to_oid(&DataType::Numeric), 1700);
        assert_ne!(datatype_to_oid(&DataType::Numeric), 705);
    }

    /// Other scalar types that previously fell through to 705/unknown.
    #[test]
    fn scalar_types_map_to_canonical_oids() {
        assert_eq!(datatype_to_oid(&DataType::Char(10)), 1042); // bpchar
        assert_eq!(datatype_to_oid(&DataType::Timestamptz), 1184);
        assert_eq!(datatype_to_oid(&DataType::Interval), 1186);
        // Spot-check the already-correct ones didn't regress.
        assert_eq!(datatype_to_oid(&DataType::Int4), 23);
        assert_eq!(datatype_to_oid(&DataType::Int8), 20);
        assert_eq!(datatype_to_oid(&DataType::Float8), 701);
        assert_eq!(datatype_to_oid(&DataType::Timestamp), 1114);
    }
}

// ============================================================================
// SQLSTATE mapping (D4 phase 1)
// ============================================================================

/// Map an [`Error`] to the SQLSTATE the wire reports (D4 phase 1).
///
/// Pattern-matches the engine's existing message shapes (same approach as
/// the constraint sniffing that predates it); codes come from the shared
/// constants in [`crate::network::protocol::sqlstate`]. Phase 2 will carry
/// structured codes from the raise sites instead of sniffing text.
pub(crate) fn sqlstate_for_error(error: &Error) -> &'static str {
    use crate::network::protocol::sqlstate;

    match error {
        Error::ConstraintViolation(message) => {
            let lower = message.to_ascii_lowercase();
            if lower.contains("duplicate") || lower.contains("unique") {
                sqlstate::UNIQUE_VIOLATION // 23505
            } else if lower.contains("foreign key") {
                sqlstate::FOREIGN_KEY_VIOLATION // 23503
            } else if lower.contains("check") {
                sqlstate::CHECK_VIOLATION // 23514
            } else {
                sqlstate::INTEGRITY_CONSTRAINT_VIOLATION // 23000
            }
        }
        Error::SqlParse(_) => sqlstate::SYNTAX_ERROR,            // 42601
        Error::TypeConversion(_) => sqlstate::DATATYPE_MISMATCH, // 42804
        Error::Transaction(message) => {
            let lower = message.to_ascii_lowercase();
            if lower.contains("serialization failure") {
                // First-committer-wins write-write conflict (R0.2):
                // drivers retry on serialization_failure.
                sqlstate::SERIALIZATION_FAILURE // 40001
            } else if lower.contains("deadlock") {
                // LockManager aborts the victim with "Deadlock detected for
                // transaction N" (storage/lock_manager.rs).
                sqlstate::DEADLOCK_DETECTED // 40P01
            } else {
                sqlstate::INVALID_TRANSACTION_STATE // 25000
            }
        }
        Error::Protocol(_) => sqlstate::PROTOCOL_VIOLATION, // 08P01
        Error::QueryTimeout(_) | Error::QueryCancelled(_) => sqlstate::QUERY_CANCELED, // 57014
        // Catalog/executor errors surface as QueryExecution with stable
        // message shapes ("Table 'x' does not exist", "Column 'x' not
        // found", "Table 'x' already exists", "Unknown scalar function").
        Error::QueryExecution(message) => sqlstate_for_query_execution_message(message),
        _ => sqlstate::INTERNAL_ERROR, // XX000
    }
}

/// Classify a `QueryExecution` message into a SQLSTATE.
///
/// Order matters: function and column shapes are checked before table
/// shapes because messages like `Column 'c' not found in table 't'`
/// mention both.
fn sqlstate_for_query_execution_message(message: &str) -> &'static str {
    use crate::network::protocol::sqlstate;

    let lower = message.to_ascii_lowercase();
    let not_found = lower.contains("not found") || lower.contains("does not exist") || lower.contains("doesn't exist");

    if lower.contains("function") && (not_found || lower.contains("unknown")) {
        sqlstate::UNDEFINED_FUNCTION // 42883
    } else if lower.contains("column") && (not_found || lower.contains("unknown")) {
        sqlstate::UNDEFINED_COLUMN // 42703
    } else if (lower.contains("table") || lower.contains("relation")) && lower.contains("already exists") {
        sqlstate::DUPLICATE_TABLE // 42P07
    } else if (lower.contains("table") || lower.contains("relation")) && not_found {
        sqlstate::UNDEFINED_TABLE // 42P01
    } else {
        sqlstate::INTERNAL_ERROR // XX000
    }
}

/// Detail/hint fields for the ErrorResponse, keyed on the mapped SQLSTATE.
///
/// Populated where they help a client act: retry guidance for
/// serialization failures/deadlocks, and the offending name for undefined
/// table/column (extracted from the engine's quoted message).
pub(crate) fn detail_hint_for_error(code: &str, error: &Error) -> (Option<String>, Option<String>) {
    use crate::network::protocol::sqlstate;

    let message = error.to_string();
    match code {
        sqlstate::SERIALIZATION_FAILURE => (
            Some("Another transaction committed a conflicting write first (first-committer-wins).".to_string()),
            Some("Retry the transaction.".to_string()),
        ),
        sqlstate::DEADLOCK_DETECTED => (
            Some("This transaction was chosen as the deadlock victim and rolled back.".to_string()),
            Some("Retry the transaction.".to_string()),
        ),
        sqlstate::UNDEFINED_TABLE => (
            first_single_quoted(&message).map(|name| format!("Table '{name}' does not exist in the catalog.")),
            Some("Check the table name, or create the table first.".to_string()),
        ),
        sqlstate::UNDEFINED_COLUMN => (
            first_single_quoted(&message).map(|name| format!("Column '{name}' does not exist.")),
            Some("Check the column name against the table definition.".to_string()),
        ),
        _ => (None, None),
    }
}

/// First single-quoted token in an engine error message
/// (`Table 'users' does not exist` → `users`).
fn first_single_quoted(message: &str) -> Option<&str> {
    let start = message.find('\'')? + 1;
    let rest = message.get(start..)?;
    let end = rest.find('\'')?;
    rest.get(..end)
}

#[cfg(test)]
mod do_block_split_tests {
    use super::pg_split_exception;

    #[test]
    fn split_handles_newline_before_exception() {
        // KanttBan #14 (v3.30.1 smoke): drizzle-kit emits the
        // EXCEPTION clause on its own line — `\n` before EXCEPTION,
        // which the previous space-bounded needle missed.
        let body = "CREATE TABLE IF NOT EXISTS hdb_test_do (id integer);\n\
                    EXCEPTION WHEN duplicate_object THEN null;";
        let (main, codes) = pg_split_exception(body);
        assert!(main.starts_with("CREATE TABLE"));
        assert!(!main.to_ascii_uppercase().contains("EXCEPTION"));
        assert_eq!(codes, vec!["duplicate_object".to_string()]);
    }

    #[test]
    fn split_still_handles_inline_exception() {
        let body = "ALTER TABLE x ADD COLUMN y INT; EXCEPTION WHEN duplicate_column THEN null;";
        let (main, codes) = pg_split_exception(body);
        assert!(main.starts_with("ALTER TABLE"));
        assert!(!main.to_ascii_uppercase().contains("EXCEPTION"));
        assert_eq!(codes, vec!["duplicate_column".to_string()]);
    }

    #[test]
    fn split_handles_no_exception() {
        let body = "ALTER TABLE x ADD COLUMN y INT;";
        let (main, codes) = pg_split_exception(body);
        assert_eq!(main, body);
        assert!(codes.is_empty());
    }

    #[test]
    fn split_does_not_match_exception_inside_identifier() {
        // `MY_EXCEPTION_TABLE` should not be confused with the EXCEPTION
        // keyword. Standalone EXCEPTION later in the body still wins.
        let body = "SELECT * FROM my_exception_table;\nEXCEPTION WHEN others THEN null;";
        let (main, codes) = pg_split_exception(body);
        assert!(main.starts_with("SELECT"));
        assert!(main.to_ascii_uppercase().contains("MY_EXCEPTION_TABLE"));
        assert!(!main.to_ascii_uppercase().ends_with("EXCEPTION"));
        assert_eq!(codes, vec!["others".to_string()]);
    }
}

#[cfg(test)]
mod failed_transaction_state_tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::DuplexStream;

    fn test_handler(db: Arc<EmbeddedDatabase>) -> (PgConnectionHandler<DuplexStream>, DuplexStream) {
        let (stream, client) = tokio::io::duplex(4096);
        (
            PgConnectionHandler {
                stream,
                session_id: db
                    .create_wire_session("pg_wire_test")
                    .expect("wire session creation is infallible"),
                database: db.clone(),
                auth_manager: Arc::new(AuthManager::new(AuthMethod::Trust)),
                catalog: PgCatalog::with_database(db),
                prepared_statements: PreparedStatementManager::new(),
                authenticated: true,
                transaction_status: TransactionStatus::Idle,
                buffer: BytesMut::with_capacity(8192),
                username: None,
                scram_state: None,
                write_buf: BytesMut::with_capacity(4096),
                suppress_ready_for_query: false,
                awaiting_sync_after_error: false,
            },
            client,
        )
    }

    #[tokio::test]
    async fn query_error_marks_open_transaction_failed_and_reenables_ready() {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
        db.begin().expect("begin");
        let (mut handler, _client) = test_handler(Arc::clone(&db));
        handler.transaction_status = TransactionStatus::InTransaction;
        handler.suppress_ready_for_query = true;

        handler
            .send_error_for_query(&Error::constraint_violation("duplicate key"), false)
            .await
            .expect("send error");

        assert_eq!(handler.transaction_status, TransactionStatus::Failed);
        assert!(!handler.suppress_ready_for_query);
        assert!(db.in_transaction());
        handler
            .handle_single_query("BEGIN")
            .await
            .expect("BEGIN recovers failed transaction");
        assert_eq!(handler.transaction_status, TransactionStatus::InTransaction);
        db.rollback().expect("rollback cleanup");
    }

    #[tokio::test]
    async fn direct_error_response_marks_open_transaction_failed() {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
        db.begin().expect("begin");
        let (mut handler, _client) = test_handler(Arc::clone(&db));
        handler.transaction_status = TransactionStatus::InTransaction;

        handler
            .send_error("ERROR", "22023", "bad setting", None, None)
            .await
            .expect("send error");

        assert_eq!(handler.transaction_status, TransactionStatus::Failed);
        assert!(db.in_transaction());
        db.rollback().expect("rollback cleanup");
    }

    #[tokio::test]
    async fn rollback_clears_failed_transaction_state() {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
        db.execute("BEGIN").expect("begin");
        let (mut handler, _client) = test_handler(db);
        handler.transaction_status = TransactionStatus::Failed;

        handler
            .handle_single_query("ROLLBACK")
            .await
            .expect("rollback after failed transaction");

        assert_eq!(handler.transaction_status, TransactionStatus::Idle);
    }
}

#[cfg(test)]
mod show_branches_wire_tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::io::DuplexStream;

    fn test_handler(db: Arc<EmbeddedDatabase>) -> (PgConnectionHandler<DuplexStream>, DuplexStream) {
        let (stream, client) = tokio::io::duplex(4096);
        (
            PgConnectionHandler {
                stream,
                session_id: db
                    .create_wire_session("pg_wire_test")
                    .expect("wire session creation is infallible"),
                database: db.clone(),
                auth_manager: Arc::new(AuthManager::new(AuthMethod::Trust)),
                catalog: PgCatalog::with_database(db),
                prepared_statements: PreparedStatementManager::new(),
                authenticated: true,
                transaction_status: TransactionStatus::Idle,
                buffer: BytesMut::with_capacity(8192),
                username: None,
                scram_state: None,
                write_buf: BytesMut::with_capacity(4096),
                suppress_ready_for_query: false,
                awaiting_sync_after_error: false,
            },
            client,
        )
    }

    fn has_complete_ready_for_query(buf: &[u8]) -> bool {
        let mut pos = 0;
        while pos + 5 <= buf.len() {
            let tag = buf[pos];
            let len = i32::from_be_bytes([buf[pos + 1], buf[pos + 2], buf[pos + 3], buf[pos + 4]]) as usize;
            let end = pos + 1 + len;
            if end > buf.len() {
                return false;
            }
            if tag == b'Z' {
                return true;
            }
            pos = end;
        }
        false
    }

    async fn read_until_ready(mut client: DuplexStream) -> Vec<u8> {
        let mut out = Vec::new();
        let mut chunk = [0_u8; 2048];
        loop {
            let read = tokio::time::timeout(Duration::from_secs(1), client.read(&mut chunk))
                .await
                .expect("timed out waiting for PostgreSQL response")
                .expect("read PostgreSQL response");
            assert!(read > 0, "connection closed before ReadyForQuery; bytes={out:?}");
            out.extend_from_slice(&chunk[..read]);
            if has_complete_ready_for_query(&out) {
                return out;
            }
        }
    }

    fn first_column_text_values(buf: &[u8]) -> Vec<Option<String>> {
        let mut pos = 0;
        let mut values = Vec::new();
        while pos + 5 <= buf.len() {
            let tag = buf[pos];
            let len = i32::from_be_bytes([buf[pos + 1], buf[pos + 2], buf[pos + 3], buf[pos + 4]]) as usize;
            let end = pos + 1 + len;
            if end > buf.len() {
                break;
            }

            if tag == b'D' {
                let mut cur = pos + 5;
                let columns = i16::from_be_bytes([buf[cur], buf[cur + 1]]) as usize;
                cur += 2;
                for idx in 0..columns {
                    let value_len = i32::from_be_bytes([buf[cur], buf[cur + 1], buf[cur + 2], buf[cur + 3]]);
                    cur += 4;
                    if value_len < 0 {
                        if idx == 0 {
                            values.push(None);
                        }
                        continue;
                    }
                    let value_end = cur + value_len as usize;
                    if idx == 0 {
                        values.push(Some(
                            std::str::from_utf8(&buf[cur..value_end])
                                .expect("first column should be UTF-8 text")
                                .to_string(),
                        ));
                    }
                    cur = value_end;
                }
            }

            pos = end;
        }
        values
    }

    #[tokio::test]
    async fn show_branches_simple_query_returns_branch_registry_rows() {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
        db.execute("CREATE TABLE t(x INT)").expect("create table");
        db.execute("INSERT INTO t VALUES(1),(2),(3)").expect("insert rows");
        db.execute("CREATE BRANCH 'alpha' AS OF NOW").expect("create alpha");
        db.execute("CREATE BRANCH 'beta' AS OF NOW").expect("create beta");

        let (mut handler, client) = test_handler(db);
        handler
            .handle_single_query("SHOW BRANCHES")
            .await
            .expect("SHOW BRANCHES over PostgreSQL wire");

        let response = read_until_ready(client).await;
        let names = first_column_text_values(&response);
        let rendered: Vec<String> = names
            .iter()
            .map(|name| name.clone().unwrap_or_else(|| "<NULL>".to_string()))
            .collect();

        assert!(
            rendered.iter().any(|name| name == "main"),
            "missing main; names={rendered:?}"
        );
        assert!(
            rendered.iter().any(|name| name == "alpha"),
            "missing alpha; names={rendered:?}"
        );
        assert!(
            rendered.iter().any(|name| name == "beta"),
            "missing beta; names={rendered:?}"
        );
        assert!(
            rendered.iter().all(|name| !name.is_empty()),
            "SHOW BRANCHES must not return a blank branch name; names={rendered:?}"
        );
    }
}

#[cfg(test)]
mod sqlstate_mapping_unit_tests {
    //! D4 phase 1: the SQLSTATE mapping, exercised with error values
    //! produced by real SQL against an EmbeddedDatabase wherever the
    //! embedded API can produce the shape (undefined table/column,
    //! duplicate table, unknown function, unique violation,
    //! serialization failure). Deadlock uses the constructor the
    //! LockManager itself uses (a second OS-level session race is not
    //! reproducible deterministically in a unit test).

    use super::{detail_hint_for_error, first_single_quoted, sqlstate_for_error};
    use crate::session::IsolationLevel;
    use crate::{EmbeddedDatabase, Error};

    fn sql_error(db: &EmbeddedDatabase, sql: &str) -> Error {
        db.execute(sql).expect_err("statement must fail")
    }

    /// SELECTs must go through the query path: `execute` does not
    /// evaluate result expressions, which is what surfaces undefined
    /// column errors.
    fn query_error(db: &EmbeddedDatabase, sql: &str) -> Error {
        db.query(sql, &[]).expect_err("query must fail")
    }

    #[test]
    fn undefined_table_maps_to_42p01_with_detail() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let err = query_error(&db, "SELECT * FROM no_such_table");
        let code = sqlstate_for_error(&err);
        assert_eq!(code, "42P01", "got error: {err}");
        let (detail, hint) = detail_hint_for_error(code, &err);
        assert!(
            detail.as_deref().unwrap_or_default().contains("no_such_table"),
            "detail must carry the table name; got {detail:?} for {err}"
        );
        assert!(hint.is_some());
    }

    #[test]
    fn undefined_column_maps_to_42703_with_detail() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t42703 (id INTEGER PRIMARY KEY)").unwrap();
        // Need a row: projection expressions are evaluated per-row, so an
        // empty table would return Ok([]) instead of the column error.
        db.execute("INSERT INTO t42703 VALUES (1)").unwrap();
        let err = query_error(&db, "SELECT no_such_col FROM t42703");
        let code = sqlstate_for_error(&err);
        assert_eq!(code, "42703", "got error: {err}");
        let (detail, hint) = detail_hint_for_error(code, &err);
        assert!(
            detail.as_deref().unwrap_or_default().contains("no_such_col"),
            "detail must carry the column name; got {detail:?} for {err}"
        );
        assert!(hint.is_some());
    }

    #[test]
    fn duplicate_table_maps_to_42p07() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t42p07 (id INTEGER PRIMARY KEY)").unwrap();
        let err = sql_error(&db, "CREATE TABLE t42p07 (id INTEGER PRIMARY KEY)");
        assert_eq!(sqlstate_for_error(&err), "42P07", "got error: {err}");
    }

    #[test]
    fn undefined_function_maps_to_42883() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t42883 (id INTEGER PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO t42883 VALUES (1)").unwrap();
        let err = query_error(&db, "SELECT definitely_not_a_function(id) FROM t42883");
        assert_eq!(sqlstate_for_error(&err), "42883", "got error: {err}");
    }

    #[test]
    fn unique_violation_still_maps_to_23505() {
        // Regression guard: the pre-D4 constraint sniffing must not change.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t23505 (id INTEGER PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO t23505 VALUES (1)").unwrap();
        let err = sql_error(&db, "INSERT INTO t23505 VALUES (1)");
        assert_eq!(sqlstate_for_error(&err), "23505", "got error: {err}");
    }

    #[test]
    fn serialization_failure_maps_to_40001_with_retry_hint() {
        // Real first-committer-wins conflict via two REPEATABLE READ
        // sessions (R0.2).
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t40001 (id INTEGER PRIMARY KEY, v INTEGER)")
            .unwrap();
        db.execute("INSERT INTO t40001 VALUES (1, 100)").unwrap();

        let a = db.create_session("a", IsolationLevel::RepeatableRead).unwrap();
        let b = db.create_session("b", IsolationLevel::RepeatableRead).unwrap();
        db.begin_transaction_for_session(a).unwrap();
        db.begin_transaction_for_session(b).unwrap();
        db.execute_in_session(a, "UPDATE t40001 SET v = 101 WHERE id = 1")
            .unwrap();
        db.commit_transaction_for_session(a).unwrap();
        db.execute_in_session(b, "UPDATE t40001 SET v = 150 WHERE id = 1")
            .unwrap();
        let err = db
            .commit_transaction_for_session(b)
            .expect_err("conflicting commit must fail");

        let code = sqlstate_for_error(&err);
        assert_eq!(code, "40001", "got error: {err}");
        let (detail, hint) = detail_hint_for_error(code, &err);
        assert!(detail.is_some());
        assert!(
            hint.as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .contains("retry"),
            "hint must suggest retrying; got {hint:?}"
        );

        db.destroy_session(a).unwrap();
        db.destroy_session(b).unwrap();
    }

    #[test]
    fn deadlock_maps_to_40p01_with_retry_hint() {
        // Same constructor the LockManager victim path uses
        // (storage/lock_manager.rs: Error::deadlock("Deadlock detected
        // for transaction N")).
        let err = Error::deadlock("Deadlock detected for transaction 7");
        let code = sqlstate_for_error(&err);
        assert_eq!(code, "40P01");
        let (_, hint) = detail_hint_for_error(code, &err);
        assert!(hint
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .contains("retry"));
    }

    #[test]
    fn non_serialization_transaction_error_keeps_25000() {
        let err = Error::transaction("no transaction in progress");
        assert_eq!(sqlstate_for_error(&err), "25000");
    }

    #[test]
    fn unknown_query_execution_error_stays_internal() {
        let err = Error::query_execution("something exotic went wrong");
        assert_eq!(sqlstate_for_error(&err), "XX000");
    }

    #[test]
    fn quoted_name_extraction() {
        assert_eq!(first_single_quoted("Table 'users' does not exist"), Some("users"));
        assert_eq!(first_single_quoted("no quotes here"), None);
        assert_eq!(first_single_quoted("dangling 'quote"), None);
    }
}
