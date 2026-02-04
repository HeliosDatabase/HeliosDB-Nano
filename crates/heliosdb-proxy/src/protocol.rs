//! Protocol Handling
//!
//! Wire protocol parsing and serialization for HeliosDB proxy.

use crate::{ProxyError, Result};
use bytes::{Buf, BufMut, BytesMut};
use std::collections::HashMap;

/// Protocol message types
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageType {
    /// Startup message from client
    Startup,
    /// SSL request
    SSLRequest,
    /// Authentication request
    AuthRequest,
    /// Password message
    Password,
    /// Query message
    Query,
    /// Parse message (prepared statement)
    Parse,
    /// Bind message
    Bind,
    /// Describe message
    Describe,
    /// Execute message
    Execute,
    /// Sync message
    Sync,
    /// Flush message
    Flush,
    /// Close message
    Close,
    /// Terminate message
    Terminate,
    /// Copy data
    CopyData,
    /// Copy done
    CopyDone,
    /// Copy fail
    CopyFail,
    /// Function call (deprecated)
    FunctionCall,
    /// Backend key data
    BackendKeyData,
    /// Parameter status
    ParameterStatus,
    /// Ready for query
    ReadyForQuery,
    /// Row description
    RowDescription,
    /// Data row
    DataRow,
    /// Command complete
    CommandComplete,
    /// Empty query response
    EmptyQueryResponse,
    /// Error response
    ErrorResponse,
    /// Notice response
    NoticeResponse,
    /// Notification response
    NotificationResponse,
    /// Parse complete
    ParseComplete,
    /// Bind complete
    BindComplete,
    /// Close complete
    CloseComplete,
    /// Portal suspended
    PortalSuspended,
    /// No data
    NoData,
    /// Parameter description
    ParameterDescription,
    /// Unknown message
    Unknown(u8),
}

impl MessageType {
    /// Get message type from tag byte
    pub fn from_tag(tag: u8) -> Self {
        match tag {
            b'Q' => MessageType::Query,
            b'P' => MessageType::Parse,
            b'B' => MessageType::Bind,
            b'D' => MessageType::Describe,
            b'E' => MessageType::Execute,
            b'S' => MessageType::Sync,
            b'H' => MessageType::Flush,
            b'C' => MessageType::Close,
            b'X' => MessageType::Terminate,
            b'd' => MessageType::CopyData,
            b'c' => MessageType::CopyDone,
            b'f' => MessageType::CopyFail,
            b'F' => MessageType::FunctionCall,
            b'p' => MessageType::Password,
            b'K' => MessageType::BackendKeyData,
            b'S' => MessageType::ParameterStatus,
            b'Z' => MessageType::ReadyForQuery,
            b'T' => MessageType::RowDescription,
            b'D' => MessageType::DataRow,
            b'C' => MessageType::CommandComplete,
            b'I' => MessageType::EmptyQueryResponse,
            b'E' => MessageType::ErrorResponse,
            b'N' => MessageType::NoticeResponse,
            b'A' => MessageType::NotificationResponse,
            b'1' => MessageType::ParseComplete,
            b'2' => MessageType::BindComplete,
            b'3' => MessageType::CloseComplete,
            b's' => MessageType::PortalSuspended,
            b'n' => MessageType::NoData,
            b't' => MessageType::ParameterDescription,
            _ => MessageType::Unknown(tag),
        }
    }

    /// Get tag byte for message type
    pub fn to_tag(&self) -> Option<u8> {
        match self {
            MessageType::Query => Some(b'Q'),
            MessageType::Parse => Some(b'P'),
            MessageType::Bind => Some(b'B'),
            MessageType::Describe => Some(b'D'),
            MessageType::Execute => Some(b'E'),
            MessageType::Sync => Some(b'S'),
            MessageType::Flush => Some(b'H'),
            MessageType::Close => Some(b'C'),
            MessageType::Terminate => Some(b'X'),
            MessageType::CopyData => Some(b'd'),
            MessageType::CopyDone => Some(b'c'),
            MessageType::CopyFail => Some(b'f'),
            MessageType::FunctionCall => Some(b'F'),
            MessageType::Password => Some(b'p'),
            MessageType::BackendKeyData => Some(b'K'),
            MessageType::ParameterStatus => Some(b'S'),
            MessageType::ReadyForQuery => Some(b'Z'),
            MessageType::RowDescription => Some(b'T'),
            MessageType::DataRow => Some(b'D'),
            MessageType::CommandComplete => Some(b'C'),
            MessageType::EmptyQueryResponse => Some(b'I'),
            MessageType::ErrorResponse => Some(b'E'),
            MessageType::NoticeResponse => Some(b'N'),
            MessageType::NotificationResponse => Some(b'A'),
            MessageType::ParseComplete => Some(b'1'),
            MessageType::BindComplete => Some(b'2'),
            MessageType::CloseComplete => Some(b'3'),
            MessageType::PortalSuspended => Some(b's'),
            MessageType::NoData => Some(b'n'),
            MessageType::ParameterDescription => Some(b't'),
            _ => None,
        }
    }
}

/// A protocol message
#[derive(Debug, Clone)]
pub struct Message {
    /// Message type
    pub msg_type: MessageType,
    /// Message payload
    pub payload: BytesMut,
}

impl Message {
    /// Create a new message
    pub fn new(msg_type: MessageType, payload: BytesMut) -> Self {
        Self { msg_type, payload }
    }

    /// Create an empty message
    pub fn empty(msg_type: MessageType) -> Self {
        Self {
            msg_type,
            payload: BytesMut::new(),
        }
    }

    /// Encode message to bytes
    pub fn encode(&self) -> BytesMut {
        let mut buf = BytesMut::new();

        if let Some(tag) = self.msg_type.to_tag() {
            buf.put_u8(tag);
        }

        // Length includes itself (4 bytes)
        let len = self.payload.len() as u32 + 4;
        buf.put_u32(len);
        buf.extend_from_slice(&self.payload);

        buf
    }
}

/// Protocol codec for framing messages
pub struct ProtocolCodec {
    /// Maximum message size
    max_message_size: usize,
}

impl Default for ProtocolCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl ProtocolCodec {
    /// Create a new codec
    pub fn new() -> Self {
        Self {
            max_message_size: 100 * 1024 * 1024, // 100MB max
        }
    }

    /// Create codec with custom max message size
    pub fn with_max_size(max_message_size: usize) -> Self {
        Self { max_message_size }
    }

    /// Decode a startup message (no tag byte)
    pub fn decode_startup(&self, src: &mut BytesMut) -> Result<Option<StartupMessage>> {
        if src.len() < 4 {
            return Ok(None);
        }

        let len = u32::from_be_bytes([src[0], src[1], src[2], src[3]]) as usize;

        if len > self.max_message_size {
            return Err(ProxyError::Protocol(format!(
                "Message too large: {} bytes",
                len
            )));
        }

        if src.len() < len {
            return Ok(None);
        }

        src.advance(4);
        let protocol_version = src.get_u32();

        // Check for SSL request
        if protocol_version == 80877103 {
            return Ok(Some(StartupMessage::SSLRequest));
        }

        // Check for cancel request
        if protocol_version == 80877102 {
            let pid = src.get_u32();
            let key = src.get_u32();
            return Ok(Some(StartupMessage::CancelRequest { pid, key }));
        }

        // Parse parameters
        let mut params = HashMap::new();
        let remaining = len - 8; // Already read length and version
        let mut param_bytes = src.split_to(remaining);

        while param_bytes.has_remaining() {
            let key = read_cstring(&mut param_bytes)?;
            if key.is_empty() {
                break;
            }
            let value = read_cstring(&mut param_bytes)?;
            params.insert(key, value);
        }

        Ok(Some(StartupMessage::Startup {
            protocol_version,
            params,
        }))
    }

    /// Decode a regular message (with tag byte)
    pub fn decode_message(&self, src: &mut BytesMut) -> Result<Option<Message>> {
        if src.len() < 5 {
            return Ok(None);
        }

        let tag = src[0];
        let len = u32::from_be_bytes([src[1], src[2], src[3], src[4]]) as usize;

        if len > self.max_message_size {
            return Err(ProxyError::Protocol(format!(
                "Message too large: {} bytes",
                len
            )));
        }

        // Length includes itself, so total message is 1 (tag) + len
        let total_len = 1 + len;
        if src.len() < total_len {
            return Ok(None);
        }

        src.advance(5); // Skip tag and length
        let payload = src.split_to(len - 4); // Length includes the 4-byte length field

        let msg_type = MessageType::from_tag(tag);
        Ok(Some(Message::new(msg_type, payload)))
    }

    /// Encode a message
    pub fn encode_message(&self, msg: &Message) -> BytesMut {
        msg.encode()
    }
}

/// Startup message variants
#[derive(Debug, Clone)]
pub enum StartupMessage {
    /// Regular startup
    Startup {
        protocol_version: u32,
        params: HashMap<String, String>,
    },
    /// SSL request
    SSLRequest,
    /// Cancel request
    CancelRequest { pid: u32, key: u32 },
}

/// Read a null-terminated string from buffer
fn read_cstring(buf: &mut BytesMut) -> Result<String> {
    let mut bytes = Vec::new();

    while buf.has_remaining() {
        let b = buf.get_u8();
        if b == 0 {
            break;
        }
        bytes.push(b);
    }

    String::from_utf8(bytes)
        .map_err(|e| ProxyError::Protocol(format!("Invalid UTF-8 in string: {}", e)))
}

/// Write a null-terminated string to buffer
fn write_cstring(buf: &mut BytesMut, s: &str) {
    buf.extend_from_slice(s.as_bytes());
    buf.put_u8(0);
}

/// Query message payload
#[derive(Debug, Clone)]
pub struct QueryMessage {
    pub query: String,
}

impl QueryMessage {
    /// Parse from message payload
    pub fn parse(mut payload: BytesMut) -> Result<Self> {
        let query = read_cstring(&mut payload)?;
        Ok(Self { query })
    }

    /// Encode to message
    pub fn encode(&self) -> Message {
        let mut payload = BytesMut::new();
        write_cstring(&mut payload, &self.query);
        Message::new(MessageType::Query, payload)
    }
}

/// Parse message payload (prepared statement)
#[derive(Debug, Clone)]
pub struct ParseMessage {
    pub name: String,
    pub query: String,
    pub param_types: Vec<u32>,
}

impl ParseMessage {
    /// Parse from message payload
    pub fn parse(mut payload: BytesMut) -> Result<Self> {
        let name = read_cstring(&mut payload)?;
        let query = read_cstring(&mut payload)?;

        let num_params = payload.get_u16() as usize;
        let mut param_types = Vec::with_capacity(num_params);

        for _ in 0..num_params {
            param_types.push(payload.get_u32());
        }

        Ok(Self {
            name,
            query,
            param_types,
        })
    }

    /// Encode to message
    pub fn encode(&self) -> Message {
        let mut payload = BytesMut::new();
        write_cstring(&mut payload, &self.name);
        write_cstring(&mut payload, &self.query);
        payload.put_u16(self.param_types.len() as u16);
        for &t in &self.param_types {
            payload.put_u32(t);
        }
        Message::new(MessageType::Parse, payload)
    }
}

/// Bind message payload
#[derive(Debug, Clone)]
pub struct BindMessage {
    pub portal: String,
    pub statement: String,
    pub param_formats: Vec<i16>,
    pub param_values: Vec<Option<Vec<u8>>>,
    pub result_formats: Vec<i16>,
}

impl BindMessage {
    /// Parse from message payload
    pub fn parse(mut payload: BytesMut) -> Result<Self> {
        let portal = read_cstring(&mut payload)?;
        let statement = read_cstring(&mut payload)?;

        // Parameter formats
        let num_formats = payload.get_u16() as usize;
        let mut param_formats = Vec::with_capacity(num_formats);
        for _ in 0..num_formats {
            param_formats.push(payload.get_i16());
        }

        // Parameter values
        let num_values = payload.get_u16() as usize;
        let mut param_values = Vec::with_capacity(num_values);
        for _ in 0..num_values {
            let len = payload.get_i32();
            if len == -1 {
                param_values.push(None);
            } else {
                let value = payload.split_to(len as usize).to_vec();
                param_values.push(Some(value));
            }
        }

        // Result formats
        let num_result_formats = payload.get_u16() as usize;
        let mut result_formats = Vec::with_capacity(num_result_formats);
        for _ in 0..num_result_formats {
            result_formats.push(payload.get_i16());
        }

        Ok(Self {
            portal,
            statement,
            param_formats,
            param_values,
            result_formats,
        })
    }
}

/// Execute message payload
#[derive(Debug, Clone)]
pub struct ExecuteMessage {
    pub portal: String,
    pub max_rows: i32,
}

impl ExecuteMessage {
    /// Parse from message payload
    pub fn parse(mut payload: BytesMut) -> Result<Self> {
        let portal = read_cstring(&mut payload)?;
        let max_rows = payload.get_i32();
        Ok(Self { portal, max_rows })
    }

    /// Encode to message
    pub fn encode(&self) -> Message {
        let mut payload = BytesMut::new();
        write_cstring(&mut payload, &self.portal);
        payload.put_i32(self.max_rows);
        Message::new(MessageType::Execute, payload)
    }
}

/// Error response message
#[derive(Debug, Clone)]
pub struct ErrorResponse {
    pub fields: HashMap<char, String>,
}

impl ErrorResponse {
    /// Parse from message payload
    pub fn parse(mut payload: BytesMut) -> Result<Self> {
        let mut fields = HashMap::new();

        while payload.has_remaining() {
            let code = payload.get_u8();
            if code == 0 {
                break;
            }
            let value = read_cstring(&mut payload)?;
            fields.insert(code as char, value);
        }

        Ok(Self { fields })
    }

    /// Get severity
    pub fn severity(&self) -> Option<&str> {
        self.fields.get(&'S').map(|s| s.as_str())
    }

    /// Get error code
    pub fn code(&self) -> Option<&str> {
        self.fields.get(&'C').map(|s| s.as_str())
    }

    /// Get message
    pub fn message(&self) -> Option<&str> {
        self.fields.get(&'M').map(|s| s.as_str())
    }

    /// Encode to message
    pub fn encode(&self) -> Message {
        let mut payload = BytesMut::new();
        for (&code, value) in &self.fields {
            payload.put_u8(code as u8);
            write_cstring(&mut payload, value);
        }
        payload.put_u8(0);
        Message::new(MessageType::ErrorResponse, payload)
    }
}

/// Ready for query message
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionStatus {
    /// Idle (not in transaction)
    Idle,
    /// In transaction block
    InTransaction,
    /// In failed transaction block
    Failed,
}

impl TransactionStatus {
    /// Parse from byte
    pub fn from_byte(b: u8) -> Self {
        match b {
            b'I' => TransactionStatus::Idle,
            b'T' => TransactionStatus::InTransaction,
            b'E' => TransactionStatus::Failed,
            _ => TransactionStatus::Idle,
        }
    }

    /// Convert to byte
    pub fn to_byte(&self) -> u8 {
        match self {
            TransactionStatus::Idle => b'I',
            TransactionStatus::InTransaction => b'T',
            TransactionStatus::Failed => b'E',
        }
    }
}

/// Command complete message
#[derive(Debug, Clone)]
pub struct CommandComplete {
    pub tag: String,
}

impl CommandComplete {
    /// Parse from message payload
    pub fn parse(mut payload: BytesMut) -> Result<Self> {
        let tag = read_cstring(&mut payload)?;
        Ok(Self { tag })
    }

    /// Encode to message
    pub fn encode(&self) -> Message {
        let mut payload = BytesMut::new();
        write_cstring(&mut payload, &self.tag);
        Message::new(MessageType::CommandComplete, payload)
    }

    /// Get rows affected for INSERT/UPDATE/DELETE
    pub fn rows_affected(&self) -> Option<u64> {
        let parts: Vec<&str> = self.tag.split_whitespace().collect();
        if parts.len() >= 2 {
            parts.last()?.parse().ok()
        } else {
            None
        }
    }
}

/// Authentication request types
#[derive(Debug, Clone)]
pub enum AuthRequest {
    /// Authentication OK
    Ok,
    /// Cleartext password
    CleartextPassword,
    /// MD5 password
    Md5Password { salt: [u8; 4] },
    /// SASL
    SASL { mechanisms: Vec<String> },
    /// SASL continue
    SASLContinue { data: Vec<u8> },
    /// SASL final
    SASLFinal { data: Vec<u8> },
    /// Unknown
    Unknown(i32),
}

impl AuthRequest {
    /// Parse from message payload
    pub fn parse(mut payload: BytesMut) -> Result<Self> {
        let auth_type = payload.get_i32();

        Ok(match auth_type {
            0 => AuthRequest::Ok,
            3 => AuthRequest::CleartextPassword,
            5 => {
                let mut salt = [0u8; 4];
                payload.copy_to_slice(&mut salt);
                AuthRequest::Md5Password { salt }
            }
            10 => {
                let mut mechanisms = Vec::new();
                loop {
                    let mech = read_cstring(&mut payload)?;
                    if mech.is_empty() {
                        break;
                    }
                    mechanisms.push(mech);
                }
                AuthRequest::SASL { mechanisms }
            }
            11 => {
                let data = payload.to_vec();
                AuthRequest::SASLContinue { data }
            }
            12 => {
                let data = payload.to_vec();
                AuthRequest::SASLFinal { data }
            }
            _ => AuthRequest::Unknown(auth_type),
        })
    }

    /// Encode to message
    pub fn encode(&self) -> Message {
        let mut payload = BytesMut::new();

        match self {
            AuthRequest::Ok => {
                payload.put_i32(0);
            }
            AuthRequest::CleartextPassword => {
                payload.put_i32(3);
            }
            AuthRequest::Md5Password { salt } => {
                payload.put_i32(5);
                payload.extend_from_slice(salt);
            }
            AuthRequest::SASL { mechanisms } => {
                payload.put_i32(10);
                for mech in mechanisms {
                    write_cstring(&mut payload, mech);
                }
                payload.put_u8(0);
            }
            AuthRequest::SASLContinue { data } => {
                payload.put_i32(11);
                payload.extend_from_slice(data);
            }
            AuthRequest::SASLFinal { data } => {
                payload.put_i32(12);
                payload.extend_from_slice(data);
            }
            AuthRequest::Unknown(t) => {
                payload.put_i32(*t);
            }
        }

        Message::new(MessageType::AuthRequest, payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_type_round_trip() {
        let types = vec![
            MessageType::Query,
            MessageType::Parse,
            MessageType::Bind,
            MessageType::Execute,
            MessageType::Sync,
        ];

        for msg_type in types {
            if let Some(tag) = msg_type.to_tag() {
                let decoded = MessageType::from_tag(tag);
                assert_eq!(decoded, msg_type);
            }
        }
    }

    #[test]
    fn test_query_message() {
        let query = QueryMessage {
            query: "SELECT 1".to_string(),
        };
        let msg = query.encode();
        assert_eq!(msg.msg_type, MessageType::Query);

        let decoded = QueryMessage::parse(msg.payload).unwrap();
        assert_eq!(decoded.query, "SELECT 1");
    }

    #[test]
    fn test_error_response() {
        let mut fields = HashMap::new();
        fields.insert('S', "ERROR".to_string());
        fields.insert('C', "42P01".to_string());
        fields.insert('M', "relation does not exist".to_string());

        let err = ErrorResponse { fields };
        assert_eq!(err.severity(), Some("ERROR"));
        assert_eq!(err.code(), Some("42P01"));
        assert_eq!(err.message(), Some("relation does not exist"));
    }

    #[test]
    fn test_command_complete() {
        let cmd = CommandComplete {
            tag: "INSERT 0 5".to_string(),
        };
        assert_eq!(cmd.rows_affected(), Some(5));

        let cmd2 = CommandComplete {
            tag: "SELECT 100".to_string(),
        };
        assert_eq!(cmd2.rows_affected(), Some(100));
    }

    #[test]
    fn test_transaction_status() {
        assert_eq!(TransactionStatus::from_byte(b'I'), TransactionStatus::Idle);
        assert_eq!(
            TransactionStatus::from_byte(b'T'),
            TransactionStatus::InTransaction
        );
        assert_eq!(TransactionStatus::from_byte(b'E'), TransactionStatus::Failed);

        assert_eq!(TransactionStatus::Idle.to_byte(), b'I');
        assert_eq!(TransactionStatus::InTransaction.to_byte(), b'T');
        assert_eq!(TransactionStatus::Failed.to_byte(), b'E');
    }

    #[test]
    fn test_protocol_codec() {
        let codec = ProtocolCodec::new();
        let query = QueryMessage {
            query: "SELECT 1".to_string(),
        };
        let msg = query.encode();
        let encoded = codec.encode_message(&msg);

        assert!(encoded.len() > 5);
        assert_eq!(encoded[0], b'Q');
    }
}
