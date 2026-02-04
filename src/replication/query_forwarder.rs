//! Query Router for Transparent Write Routing (TWR)
//!
//! This module implements Transparent Write Routing (TWR) from standbys to primary.
//! When a standby receives a write operation (DML/DDL), it routes the query to the
//! primary and returns the result to the client transparently.
//!
//! This enables:
//! - Applications to connect to any node (primary or standby)
//! - Automatic write routing to primary (TWR)
//! - Local read execution on standbys for load distribution
//!
//! # Behavior by Sync Mode
//!
//! - **Sync/Semi-Sync**: Writes routed to primary, results returned synchronously
//! - **Async**: Writes rejected (standby is read-only) - configurable

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::RwLock;
use std::time::Duration;

/// Result from a forwarded query
#[derive(Debug, Clone)]
pub enum ForwardedResult {
    /// Query executed successfully with row data
    Rows {
        columns: Vec<ColumnInfo>,
        rows: Vec<Vec<Option<String>>>,
    },
    /// Command completed (INSERT, UPDATE, DELETE, etc.)
    Command {
        tag: String,
        rows_affected: u64,
    },
    /// Error from primary
    Error {
        severity: String,
        code: String,
        message: String,
        detail: Option<String>,
        hint: Option<String>,
    },
}

/// Column information from query result
#[derive(Debug, Clone)]
pub struct ColumnInfo {
    pub name: String,
    pub type_oid: i32,
}

/// Query forwarder that routes writes to primary
pub struct QueryForwarder {
    primary_host: String,
    primary_port: u16,
    connection_timeout: Duration,
    query_timeout: Duration,
    /// Connection pool (simple implementation)
    connections: RwLock<Vec<TcpStream>>,
    max_connections: usize,
}

impl QueryForwarder {
    /// Create a new query forwarder
    pub fn new(primary_host: String, primary_port: u16) -> Self {
        Self {
            primary_host,
            primary_port,
            connection_timeout: Duration::from_secs(5),
            query_timeout: Duration::from_secs(30),
            connections: RwLock::new(Vec::new()),
            max_connections: 10,
        }
    }

    /// Set connection timeout
    pub fn with_connection_timeout(mut self, timeout: Duration) -> Self {
        self.connection_timeout = timeout;
        self
    }

    /// Set query timeout
    pub fn with_query_timeout(mut self, timeout: Duration) -> Self {
        self.query_timeout = timeout;
        self
    }

    /// Forward a query to the primary and return the result
    pub fn forward_query(&self, query: &str) -> Result<ForwardedResult, ForwarderError> {
        // Get or create a connection
        let mut conn = self.get_connection()?;

        // Send the query using Simple Query protocol
        let result = self.execute_query(&mut conn, query);

        // Return connection to pool if still valid
        if result.is_ok() {
            self.return_connection(conn);
        }

        result
    }

    /// Get a connection from pool or create new one
    fn get_connection(&self) -> Result<TcpStream, ForwarderError> {
        // Try to get from pool
        if let Ok(mut pool) = self.connections.write() {
            if let Some(conn) = pool.pop() {
                // Verify connection is still alive
                if Self::is_connection_alive(&conn) {
                    return Ok(conn);
                }
            }
        }

        // Create new connection
        self.create_connection()
    }

    /// Return connection to pool
    fn return_connection(&self, conn: TcpStream) {
        if let Ok(mut pool) = self.connections.write() {
            if pool.len() < self.max_connections {
                pool.push(conn);
            }
            // Otherwise drop the connection
        }
    }

    /// Check if connection is still alive
    fn is_connection_alive(conn: &TcpStream) -> bool {
        // Try to set non-blocking and peek
        if conn.set_nonblocking(true).is_err() {
            return false;
        }
        let mut buf = [0u8; 1];
        let result = match conn.peek(&mut buf) {
            Ok(0) => false, // Connection closed
            Ok(_) => true,  // Data available
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => true, // No data, but alive
            Err(_) => false, // Error
        };
        let _ = conn.set_nonblocking(false);
        result
    }

    /// Create a new connection to the primary
    fn create_connection(&self) -> Result<TcpStream, ForwarderError> {
        use std::net::ToSocketAddrs;

        let addr_str = format!("{}:{}", self.primary_host, self.primary_port);

        // Resolve hostname to socket address (supports Docker DNS)
        let addr = addr_str
            .to_socket_addrs()
            .map_err(|e| ForwarderError::Connection(format!("Cannot resolve '{}': {}", addr_str, e)))?
            .next()
            .ok_or_else(|| ForwarderError::Connection(format!("No addresses found for '{}'", addr_str)))?;

        let conn = TcpStream::connect_timeout(&addr, self.connection_timeout)
            .map_err(|e| ForwarderError::Connection(format!("Failed to connect to primary at {}: {}", addr_str, e)))?;

        conn.set_read_timeout(Some(self.query_timeout))
            .map_err(|e| ForwarderError::Connection(format!("Failed to set read timeout: {}", e)))?;
        conn.set_write_timeout(Some(self.query_timeout))
            .map_err(|e| ForwarderError::Connection(format!("Failed to set write timeout: {}", e)))?;

        // Perform startup handshake
        let mut conn = conn;
        self.perform_startup(&mut conn)?;

        Ok(conn)
    }

    /// Perform PostgreSQL startup handshake
    fn perform_startup(&self, conn: &mut TcpStream) -> Result<(), ForwarderError> {

        // Build startup message
        // Protocol version 3.0 (196608 = 3 << 16)
        let mut params: Vec<u8> = Vec::new();

        // user parameter
        params.extend_from_slice(b"user\0");
        params.extend_from_slice(b"heliosdb\0");

        // database parameter
        params.extend_from_slice(b"database\0");
        params.extend_from_slice(b"heliosdb\0");

        // Null terminator for params
        params.push(0);

        let msg_len = 4 + 4 + params.len(); // length + protocol + params
        let mut msg = Vec::with_capacity(msg_len);
        msg.extend_from_slice(&(msg_len as i32).to_be_bytes());
        msg.extend_from_slice(&196608i32.to_be_bytes()); // Protocol 3.0
        msg.extend_from_slice(&params);

        conn.write_all(&msg)
            .map_err(|e| ForwarderError::Protocol(format!("Failed to send startup: {}", e)))?;
        conn.flush()
            .map_err(|e| ForwarderError::Protocol(format!("Failed to flush startup: {}", e)))?;

        // Read response - expect AuthenticationOk or ReadyForQuery
        loop {
            let msg_type = self.read_byte(conn)?;
            let msg_len = self.read_i32(conn)? as usize - 4;

            match msg_type {
                b'R' => {
                    // AuthenticationXxx
                    let auth_type = self.read_i32(conn)?;
                    if auth_type == 0 {
                        // AuthenticationOk
                        continue;
                    } else {
                        // Skip other auth bytes
                        let remaining = msg_len - 4;
                        if remaining > 0 {
                            let mut buf = vec![0u8; remaining];
                            conn.read_exact(&mut buf)
                                .map_err(|e| ForwarderError::Protocol(format!("Failed to read auth data: {}", e)))?;
                        }
                        // For now, we only support trust auth
                        // In production, would need to handle password, etc.
                    }
                }
                b'S' => {
                    // ParameterStatus - skip
                    let mut buf = vec![0u8; msg_len];
                    conn.read_exact(&mut buf)
                        .map_err(|e| ForwarderError::Protocol(format!("Failed to read param status: {}", e)))?;
                }
                b'K' => {
                    // BackendKeyData - skip
                    let mut buf = vec![0u8; msg_len];
                    conn.read_exact(&mut buf)
                        .map_err(|e| ForwarderError::Protocol(format!("Failed to read backend key: {}", e)))?;
                }
                b'Z' => {
                    // ReadyForQuery - we're done
                    let mut buf = vec![0u8; msg_len];
                    conn.read_exact(&mut buf)
                        .map_err(|e| ForwarderError::Protocol(format!("Failed to read ready: {}", e)))?;
                    return Ok(());
                }
                b'E' => {
                    // ErrorResponse
                    let error = self.parse_error_response(conn, msg_len)?;
                    return Err(ForwarderError::Primary(error));
                }
                _ => {
                    // Skip unknown message
                    let mut buf = vec![0u8; msg_len];
                    conn.read_exact(&mut buf)
                        .map_err(|e| ForwarderError::Protocol(format!("Failed to skip message: {}", e)))?;
                }
            }
        }
    }

    /// Execute a query on the connection
    fn execute_query(&self, conn: &mut TcpStream, query: &str) -> Result<ForwardedResult, ForwarderError> {
        // Send Simple Query message
        let query_bytes = query.as_bytes();
        let msg_len = 4 + query_bytes.len() + 1; // length + query + null

        let mut msg = Vec::with_capacity(1 + msg_len);
        msg.push(b'Q');
        msg.extend_from_slice(&(msg_len as i32).to_be_bytes());
        msg.extend_from_slice(query_bytes);
        msg.push(0);

        conn.write_all(&msg)
            .map_err(|e| ForwarderError::Protocol(format!("Failed to send query: {}", e)))?;
        conn.flush()
            .map_err(|e| ForwarderError::Protocol(format!("Failed to flush query: {}", e)))?;

        // Read response
        let mut columns: Vec<ColumnInfo> = Vec::new();
        let mut rows: Vec<Vec<Option<String>>> = Vec::new();
        let mut command_tag: Option<String> = None;

        loop {
            let msg_type = self.read_byte(conn)?;
            let msg_len = self.read_i32(conn)? as usize - 4;

            match msg_type {
                b'T' => {
                    // RowDescription
                    columns = self.parse_row_description(conn, msg_len)?;
                }
                b'D' => {
                    // DataRow
                    let row = self.parse_data_row(conn, msg_len, columns.len())?;
                    rows.push(row);
                }
                b'C' => {
                    // CommandComplete
                    let mut buf = vec![0u8; msg_len];
                    conn.read_exact(&mut buf)
                        .map_err(|e| ForwarderError::Protocol(format!("Failed to read command complete: {}", e)))?;
                    // Remove null terminator
                    if let Some(0) = buf.last() {
                        buf.pop();
                    }
                    command_tag = Some(String::from_utf8_lossy(&buf).to_string());
                }
                b'Z' => {
                    // ReadyForQuery - done
                    let mut buf = vec![0u8; msg_len];
                    conn.read_exact(&mut buf)
                        .map_err(|e| ForwarderError::Protocol(format!("Failed to read ready: {}", e)))?;

                    // Return appropriate result
                    if !columns.is_empty() || !rows.is_empty() {
                        return Ok(ForwardedResult::Rows { columns, rows });
                    } else if let Some(tag) = command_tag {
                        let rows_affected = Self::parse_rows_affected(&tag);
                        return Ok(ForwardedResult::Command { tag, rows_affected });
                    } else {
                        return Ok(ForwardedResult::Command {
                            tag: "OK".to_string(),
                            rows_affected: 0,
                        });
                    }
                }
                b'E' => {
                    // ErrorResponse
                    let error = self.parse_error_response(conn, msg_len)?;

                    // Still need to read until ReadyForQuery
                    loop {
                        let mt = self.read_byte(conn)?;
                        let ml = self.read_i32(conn)? as usize - 4;
                        let mut buf = vec![0u8; ml];
                        conn.read_exact(&mut buf).ok();
                        if mt == b'Z' {
                            break;
                        }
                    }

                    return Ok(error);
                }
                b'N' => {
                    // NoticeResponse - skip
                    let mut buf = vec![0u8; msg_len];
                    conn.read_exact(&mut buf)
                        .map_err(|e| ForwarderError::Protocol(format!("Failed to read notice: {}", e)))?;
                }
                b'I' => {
                    // EmptyQueryResponse
                    // msg_len should be 0, nothing to read
                }
                _ => {
                    // Skip unknown message
                    let mut buf = vec![0u8; msg_len];
                    conn.read_exact(&mut buf)
                        .map_err(|e| ForwarderError::Protocol(format!("Failed to skip message type {}: {}", msg_type as char, e)))?;
                }
            }
        }
    }

    /// Parse RowDescription message
    fn parse_row_description(&self, conn: &mut TcpStream, _msg_len: usize) -> Result<Vec<ColumnInfo>, ForwarderError> {
        let num_fields = self.read_i16(conn)? as usize;
        let mut columns = Vec::with_capacity(num_fields);

        for _ in 0..num_fields {
            // Read null-terminated column name
            let name = self.read_string(conn)?;

            // Skip: table OID (4), column attr (2), type OID (4), type size (2), type mod (4), format (2)
            let _table_oid = self.read_i32(conn)?;
            let _column_attr = self.read_i16(conn)?;
            let type_oid = self.read_i32(conn)?;
            let _type_size = self.read_i16(conn)?;
            let _type_mod = self.read_i32(conn)?;
            let _format = self.read_i16(conn)?;

            columns.push(ColumnInfo { name, type_oid });
        }

        Ok(columns)
    }

    /// Parse DataRow message
    fn parse_data_row(&self, conn: &mut TcpStream, _msg_len: usize, num_columns: usize) -> Result<Vec<Option<String>>, ForwarderError> {
        let num_values = self.read_i16(conn)? as usize;
        let mut row = Vec::with_capacity(num_columns.max(num_values));

        for _ in 0..num_values {
            let len = self.read_i32(conn)?;
            if len == -1 {
                row.push(None); // NULL
            } else {
                let mut buf = vec![0u8; len as usize];
                conn.read_exact(&mut buf)
                    .map_err(|e| ForwarderError::Protocol(format!("Failed to read data: {}", e)))?;
                row.push(Some(String::from_utf8_lossy(&buf).to_string()));
            }
        }

        Ok(row)
    }

    /// Parse ErrorResponse message
    fn parse_error_response(&self, conn: &mut TcpStream, msg_len: usize) -> Result<ForwardedResult, ForwarderError> {
        let mut buf = vec![0u8; msg_len];
        conn.read_exact(&mut buf)
            .map_err(|e| ForwarderError::Protocol(format!("Failed to read error: {}", e)))?;

        let mut severity = String::from("ERROR");
        let mut code = String::from("XX000");
        let mut message = String::from("Unknown error");
        let mut detail = None;
        let mut hint = None;

        let mut i = 0;
        while i < buf.len() {
            let field_type = buf[i];
            i += 1;
            if field_type == 0 {
                break;
            }

            // Read null-terminated string
            let start = i;
            while i < buf.len() && buf[i] != 0 {
                i += 1;
            }
            let value = String::from_utf8_lossy(&buf[start..i]).to_string();
            i += 1; // Skip null terminator

            match field_type {
                b'S' => severity = value,
                b'C' => code = value,
                b'M' => message = value,
                b'D' => detail = Some(value),
                b'H' => hint = Some(value),
                _ => {} // Ignore other fields
            }
        }

        Ok(ForwardedResult::Error {
            severity,
            code,
            message,
            detail,
            hint,
        })
    }

    /// Parse rows affected from command tag
    fn parse_rows_affected(tag: &str) -> u64 {
        // Tags like "INSERT 0 1", "UPDATE 5", "DELETE 3"
        let parts: Vec<&str> = tag.split_whitespace().collect();
        if let Some(last) = parts.last() {
            last.parse().unwrap_or(0)
        } else {
            0
        }
    }

    // Helper functions for reading postgres protocol data

    fn read_byte(&self, conn: &mut TcpStream) -> Result<u8, ForwarderError> {
        let mut buf = [0u8; 1];
        conn.read_exact(&mut buf)
            .map_err(|e| ForwarderError::Protocol(format!("Failed to read byte: {}", e)))?;
        Ok(buf[0])
    }

    fn read_i16(&self, conn: &mut TcpStream) -> Result<i16, ForwarderError> {
        let mut buf = [0u8; 2];
        conn.read_exact(&mut buf)
            .map_err(|e| ForwarderError::Protocol(format!("Failed to read i16: {}", e)))?;
        Ok(i16::from_be_bytes(buf))
    }

    fn read_i32(&self, conn: &mut TcpStream) -> Result<i32, ForwarderError> {
        let mut buf = [0u8; 4];
        conn.read_exact(&mut buf)
            .map_err(|e| ForwarderError::Protocol(format!("Failed to read i32: {}", e)))?;
        Ok(i32::from_be_bytes(buf))
    }

    fn read_string(&self, conn: &mut TcpStream) -> Result<String, ForwarderError> {
        let mut bytes = Vec::new();
        loop {
            let b = self.read_byte(conn)?;
            if b == 0 {
                break;
            }
            bytes.push(b);
        }
        Ok(String::from_utf8_lossy(&bytes).to_string())
    }
}

/// Errors from query forwarding
#[derive(Debug)]
pub enum ForwarderError {
    /// Connection error
    Connection(String),
    /// Protocol error
    Protocol(String),
    /// Error from primary
    Primary(ForwardedResult),
    /// Not configured
    NotConfigured,
}

impl std::fmt::Display for ForwarderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ForwarderError::Connection(msg) => write!(f, "Connection error: {}", msg),
            ForwarderError::Protocol(msg) => write!(f, "Protocol error: {}", msg),
            ForwarderError::Primary(result) => {
                if let ForwardedResult::Error { message, .. } = result {
                    write!(f, "Primary error: {}", message)
                } else {
                    write!(f, "Primary error")
                }
            }
            ForwarderError::NotConfigured => write!(f, "Query forwarder not configured"),
        }
    }
}

impl std::error::Error for ForwarderError {}

/// Global query forwarder instance (initialized when standby connects to primary)
static QUERY_FORWARDER: once_cell::sync::OnceCell<QueryForwarder> = once_cell::sync::OnceCell::new();

/// Initialize the global query forwarder
pub fn init_query_forwarder(primary_host: String, primary_port: u16) {
    let _ = QUERY_FORWARDER.set(QueryForwarder::new(primary_host, primary_port));
}

/// Get the global query forwarder
pub fn query_forwarder() -> Option<&'static QueryForwarder> {
    QUERY_FORWARDER.get()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rows_affected() {
        assert_eq!(QueryForwarder::parse_rows_affected("INSERT 0 1"), 1);
        assert_eq!(QueryForwarder::parse_rows_affected("UPDATE 5"), 5);
        assert_eq!(QueryForwarder::parse_rows_affected("DELETE 10"), 10);
        assert_eq!(QueryForwarder::parse_rows_affected("SELECT 100"), 100);
        assert_eq!(QueryForwarder::parse_rows_affected("CREATE TABLE"), 0);
    }
}
