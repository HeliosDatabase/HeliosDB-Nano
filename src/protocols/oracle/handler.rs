//! Oracle Protocol Handler
//!
//! Handles TNS/TTC protocol messages and executes SQL queries
//! through the HeliosDB-Lite query executor.

use super::tns::{TnsPacket, TnsPacketType, TnsConnect, TnsData};
use super::ttc::{TtcMessage, TtcFunction, TtcParse, TtcExecute, TtcFetch, TtcLogon, TtcResponseBuilder};
use super::translator::OracleTranslator;
use super::ORACLE_PROTOCOL_VERSION;
use crate::{Result, Error, storage::StorageEngine, Tuple, Value};
use crate::sql::{Parser, Planner, Executor};
use std::collections::HashMap;
use std::sync::Arc;

/// Oracle protocol connection state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Initial state - awaiting TNS Connect
    Initial,
    /// TNS connection established, awaiting authentication
    Connected,
    /// Authenticated and ready for queries
    Authenticated,
    /// Connection closed
    Closed,
}

/// Cursor state for multi-step query execution
#[derive(Debug, Clone)]
pub struct CursorState {
    /// Parsed SQL statement
    pub sql: String,
    /// Translated PostgreSQL SQL
    pub translated_sql: String,
    /// Query results (cached for fetching)
    pub results: Vec<Tuple>,
    /// Current fetch position
    pub fetch_position: usize,
    /// Is query executed
    pub executed: bool,
}

/// Oracle protocol handler
pub struct OracleProtocolHandler {
    /// Connection state
    state: ConnectionState,
    /// SQL translator
    translator: OracleTranslator,
    /// Storage engine reference
    storage: Arc<StorageEngine>,
    /// Active cursors (cursor_id -> cursor state)
    cursors: HashMap<u16, CursorState>,
    /// Next cursor ID
    next_cursor_id: u16,
    /// Current session user
    username: Option<String>,
    /// TNS connection parameters
    sdu_size: u16,
    tdu_size: u16,
}

impl OracleProtocolHandler {
    /// Create a new Oracle protocol handler
    pub fn new(storage: Arc<StorageEngine>) -> Self {
        Self {
            state: ConnectionState::Initial,
            translator: OracleTranslator::new(),
            storage,
            cursors: HashMap::new(),
            next_cursor_id: 1,
            username: None,
            sdu_size: 8192,
            tdu_size: 65535,
        }
    }

    /// Handle incoming TNS packet
    pub fn handle_packet(&mut self, packet: TnsPacket) -> Result<Vec<TnsPacket>> {
        match packet.header.packet_type {
            TnsPacketType::Connect => self.handle_connect(packet),
            TnsPacketType::Data => self.handle_data(packet),
            TnsPacketType::Ack => Ok(vec![]), // ACK doesn't need response
            TnsPacketType::Marker => Ok(vec![]), // Marker doesn't need response
            TnsPacketType::Attention => Ok(vec![]), // Attention handled internally
            _ => {
                Err(Error::protocol(format!(
                    "Unsupported TNS packet type: {:?}",
                    packet.header.packet_type
                )))
            }
        }
    }

    /// Handle TNS Connect packet
    fn handle_connect(&mut self, packet: TnsPacket) -> Result<Vec<TnsPacket>> {
        let connect = TnsConnect::parse(&packet.payload)?;

        // Extract service name for logging
        let service_name = connect.service_name()
            .unwrap_or_else(|| "unknown".to_string());

        tracing::info!(
            "TNS Connect received: version={}, service={}",
            connect.version,
            service_name
        );

        // Update connection parameters
        self.sdu_size = connect.sdu_size.min(8192); // Cap at 8KB
        self.tdu_size = connect.tdu_size;
        self.state = ConnectionState::Connected;

        // Send TNS Accept packet
        let accept = TnsPacket::accept(ORACLE_PROTOCOL_VERSION, self.sdu_size, self.tdu_size);

        Ok(vec![accept])
    }

    /// Handle TNS Data packet (contains TTC messages)
    fn handle_data(&mut self, packet: TnsPacket) -> Result<Vec<TnsPacket>> {
        let tns_data = TnsData::parse(&packet.payload)?;

        // Parse TTC message from data payload
        let ttc_msg = TtcMessage::parse(&tns_data.data)?;

        // Handle TTC message based on function code
        let response_data = match ttc_msg.header.function {
            TtcFunction::ProtoNeg => self.handle_proto_neg()?,
            TtcFunction::DataTypeNeg => self.handle_datatype_neg()?,
            TtcFunction::Logon => self.handle_logon(&ttc_msg.payload)?,
            TtcFunction::Parse => self.handle_parse(&ttc_msg.payload)?,
            TtcFunction::Execute => self.handle_execute(&ttc_msg.payload)?,
            TtcFunction::Fetch => self.handle_fetch(&ttc_msg.payload)?,
            TtcFunction::CloseCursor => self.handle_close_cursor(&ttc_msg.payload)?,
            TtcFunction::Commit => self.handle_commit()?,
            TtcFunction::Rollback => self.handle_rollback()?,
            TtcFunction::Logoff => self.handle_logoff()?,
            TtcFunction::Ping => self.handle_ping()?,
            _ => {
                return Err(Error::protocol(format!(
                    "Unsupported TTC function: {:?}",
                    ttc_msg.header.function
                )));
            }
        };

        // Wrap response in TTC message and TNS Data packet
        let ttc_response = TtcMessage::new(ttc_msg.header.function, response_data);
        let tns_data_response = TnsData::new(ttc_response.encode());
        let tns_packet = TnsPacket::data(tns_data_response.encode());

        Ok(vec![tns_packet])
    }

    /// Handle protocol negotiation
    fn handle_proto_neg(&mut self) -> Result<Vec<u8>> {
        // Send back acceptance of protocol version
        let mut builder = TtcResponseBuilder::new();
        builder.write_header(TtcFunction::ProtoNeg);
        Ok(builder.build())
    }

    /// Handle data type negotiation
    fn handle_datatype_neg(&mut self) -> Result<Vec<u8>> {
        // Accept data type negotiation
        let mut builder = TtcResponseBuilder::new();
        builder.write_header(TtcFunction::DataTypeNeg);
        Ok(builder.build())
    }

    /// Handle logon authentication
    fn handle_logon(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        let logon = TtcLogon::parse(payload)?;

        tracing::info!(
            "Oracle logon attempt: user={}, database={}",
            logon.username,
            logon.database
        );

        // For now, accept any authentication
        // In production, this should verify credentials
        self.username = Some(logon.username.clone());
        self.state = ConnectionState::Authenticated;

        // Send successful logon response
        let mut builder = TtcResponseBuilder::new();
        builder.write_header(TtcFunction::Logon);
        Ok(builder.build())
    }

    /// Handle SQL parse request
    fn handle_parse(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        if self.state != ConnectionState::Authenticated {
            return self.error_response("ORA-01017", "Not authenticated");
        }

        let parse_msg = TtcParse::parse(payload)?;

        tracing::debug!("Parsing Oracle SQL: {}", parse_msg.sql);

        // Translate Oracle SQL to PostgreSQL
        let translated_sql = self.translator.translate(&parse_msg.sql)?;

        tracing::debug!("Translated SQL: {}", translated_sql);

        // Create cursor state
        let cursor_id = self.next_cursor_id;
        self.next_cursor_id = self.next_cursor_id.wrapping_add(1);

        let cursor = CursorState {
            sql: parse_msg.sql.clone(),
            translated_sql,
            results: Vec::new(),
            fetch_position: 0,
            executed: false,
        };

        self.cursors.insert(cursor_id, cursor);

        // Send parse complete response
        let mut builder = TtcResponseBuilder::new();
        builder.write_header(TtcFunction::Parse);
        Ok(builder.build())
    }

    /// Handle SQL execute request
    fn handle_execute(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        if self.state != ConnectionState::Authenticated {
            return self.error_response("ORA-01017", "Not authenticated");
        }

        let execute_msg = TtcExecute::parse(payload)?;

        // Get cursor
        let cursor = self.cursors.get_mut(&execute_msg.cursor_id)
            .ok_or_else(|| Error::query_execution("Invalid cursor ID"))?;

        tracing::debug!("Executing SQL: {}", cursor.translated_sql);

        // Parse and execute SQL
        let parser = Parser::new();
        let statement = parser.parse_one(&cursor.translated_sql)?;

        let catalog = self.storage.catalog();
        let planner = Planner::with_catalog(&catalog);
        let plan = planner.statement_to_plan(statement)?;

        let mut executor = Executor::with_storage(&self.storage);
        let results = executor.execute(&plan)?;

        // Store results in cursor
        let rows_affected = results.len() as u64;
        cursor.results = results;
        cursor.executed = true;
        cursor.fetch_position = 0;

        // Send execute response
        let mut builder = TtcResponseBuilder::new();
        builder.write_header(TtcFunction::Execute);
        builder.write_command_complete(rows_affected);

        Ok(builder.build())
    }

    /// Handle fetch request
    fn handle_fetch(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        if self.state != ConnectionState::Authenticated {
            return self.error_response("ORA-01017", "Not authenticated");
        }

        let fetch_msg = TtcFetch::parse(payload)?;

        // Get cursor
        let cursor = self.cursors.get_mut(&fetch_msg.cursor_id)
            .ok_or_else(|| Error::query_execution("Invalid cursor ID"))?;

        if !cursor.executed {
            return self.error_response("ORA-24338", "Statement not executed");
        }

        let mut builder = TtcResponseBuilder::new();
        builder.write_header(TtcFunction::Fetch);

        // Fetch rows
        let mut rows_fetched = 0;
        let max_rows = fetch_msg.num_rows as usize;

        while rows_fetched < max_rows {
            let tuple = match cursor.results.get(cursor.fetch_position) {
                Some(t) => t,
                None => break,
            };
            let num_columns = tuple.values.len() as u16;

            builder.write_row_header(num_columns);

            for value in &tuple.values {
                match value {
                    Value::Null => builder.write_null_column(),
                    Value::Boolean(b) => builder.write_column(&b.to_string()),
                    Value::Int4(i) => builder.write_column(&i.to_string()),
                    Value::Int8(i) => builder.write_column(&i.to_string()),
                    Value::Float4(f) => builder.write_column(&f.to_string()),
                    Value::Float8(f) => builder.write_column(&f.to_string()),
                    Value::String(s) => builder.write_column(s),
                    Value::Timestamp(ts) => builder.write_column(&ts.to_rfc3339()),
                    Value::Json(j) => builder.write_column(j),
                    _ => builder.write_column(&value.to_string()),
                }
            }

            cursor.fetch_position += 1;
            rows_fetched += 1;
        }

        // Mark end of fetch if all rows consumed
        if cursor.fetch_position >= cursor.results.len() {
            builder.write_end_of_fetch();
        }

        Ok(builder.build())
    }

    /// Handle close cursor request
    fn handle_close_cursor(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        // Extract cursor ID from payload (simplified)
        let cursor_id = if let (Some(&b0), Some(&b1)) = (payload.first(), payload.get(1)) {
            u16::from_be_bytes([b0, b1])
        } else {
            return self.error_response("ORA-01001", "Invalid cursor");
        };

        // Remove cursor
        self.cursors.remove(&cursor_id);

        let mut builder = TtcResponseBuilder::new();
        builder.write_header(TtcFunction::CloseCursor);
        Ok(builder.build())
    }

    /// Handle commit request
    fn handle_commit(&mut self) -> Result<Vec<u8>> {
        // In embedded mode, commits are auto-committed
        // This is a no-op acknowledgment
        let mut builder = TtcResponseBuilder::new();
        builder.write_header(TtcFunction::Commit);
        Ok(builder.build())
    }

    /// Handle rollback request
    fn handle_rollback(&mut self) -> Result<Vec<u8>> {
        // In embedded mode, transactions are auto-committed
        // This is a no-op acknowledgment
        let mut builder = TtcResponseBuilder::new();
        builder.write_header(TtcFunction::Rollback);
        Ok(builder.build())
    }

    /// Handle logoff request
    fn handle_logoff(&mut self) -> Result<Vec<u8>> {
        self.state = ConnectionState::Closed;
        self.cursors.clear();

        let mut builder = TtcResponseBuilder::new();
        builder.write_header(TtcFunction::Logoff);
        Ok(builder.build())
    }

    /// Handle ping request
    fn handle_ping(&mut self) -> Result<Vec<u8>> {
        let mut builder = TtcResponseBuilder::new();
        builder.write_header(TtcFunction::Ping);
        Ok(builder.build())
    }

    /// Create an error response
    fn error_response(&self, code: &str, message: &str) -> Result<Vec<u8>> {
        let mut builder = TtcResponseBuilder::new();
        builder.write_error(code, message);
        Ok(builder.build())
    }

    /// Get connection state
    pub fn state(&self) -> ConnectionState {
        self.state
    }

    /// Check if connection is closed
    pub fn is_closed(&self) -> bool {
        self.state == ConnectionState::Closed
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn test_handler_creation() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).unwrap();
        let handler = OracleProtocolHandler::new(Arc::new(storage));

        assert_eq!(handler.state(), ConnectionState::Initial);
        assert!(!handler.is_closed());
    }

    #[test]
    fn test_connection_state_transitions() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).unwrap();
        let mut handler = OracleProtocolHandler::new(Arc::new(storage));

        assert_eq!(handler.state, ConnectionState::Initial);

        // Simulate authentication
        handler.state = ConnectionState::Authenticated;
        assert_eq!(handler.state, ConnectionState::Authenticated);

        // Simulate logoff
        handler.state = ConnectionState::Closed;
        assert!(handler.is_closed());
    }
}
