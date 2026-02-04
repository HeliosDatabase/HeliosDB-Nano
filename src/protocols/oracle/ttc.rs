//! TTC (Two-Task Common) Protocol Implementation
//!
//! TTC is Oracle's protocol layer on top of TNS for SQL command execution.
//! This module implements TTC message types and data structures.
//!
//! Reference: Oracle Database Net Services Reference

use bytes::{Buf, BufMut, BytesMut};
use std::io::{self, Cursor};

/// TTC function codes (message types)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TtcFunction {
    /// Protocol negotiation
    ProtoNeg = 1,
    /// Data type negotiation
    DataTypeNeg = 2,
    /// Open cursor
    OpenCursor = 3,
    /// Parse statement
    Parse = 4,
    /// Execute statement
    Execute = 5,
    /// Fetch rows
    Fetch = 6,
    /// Close cursor
    CloseCursor = 7,
    /// Commit transaction
    Commit = 8,
    /// Rollback transaction
    Rollback = 9,
    /// Describe statement
    Describe = 10,
    /// Define output columns
    Define = 11,
    /// Bind variables
    Bind = 12,
    /// Get server version
    Version = 13,
    /// Logon to database
    Logon = 14,
    /// Logoff from database
    Logoff = 15,
    /// OALL (Oracle All) - generic call
    Oall = 17,
    /// Ping
    Ping = 147,
}

impl TtcFunction {
    /// Convert u8 to TtcFunction
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::ProtoNeg),
            2 => Some(Self::DataTypeNeg),
            3 => Some(Self::OpenCursor),
            4 => Some(Self::Parse),
            5 => Some(Self::Execute),
            6 => Some(Self::Fetch),
            7 => Some(Self::CloseCursor),
            8 => Some(Self::Commit),
            9 => Some(Self::Rollback),
            10 => Some(Self::Describe),
            11 => Some(Self::Define),
            12 => Some(Self::Bind),
            13 => Some(Self::Version),
            14 => Some(Self::Logon),
            15 => Some(Self::Logoff),
            17 => Some(Self::Oall),
            147 => Some(Self::Ping),
            _ => None,
        }
    }
}

/// TTC message header
#[derive(Debug, Clone)]
pub struct TtcHeader {
    /// Function code
    pub function: TtcFunction,
    /// Sequence number
    pub seq_num: u8,
    /// Data flags
    pub flags: u8,
}

impl TtcHeader {
    /// TTC header size
    pub const SIZE: usize = 3;

    /// Parse TTC header from bytes
    pub fn parse(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Insufficient data for TTC header",
            ));
        }

        let mut cursor = Cursor::new(data);
        let function_code = cursor.get_u8();
        let seq_num = cursor.get_u8();
        let flags = cursor.get_u8();

        let function = TtcFunction::from_u8(function_code)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Invalid TTC function code: {}", function_code),
                )
            })?;

        Ok(Self {
            function,
            seq_num,
            flags,
        })
    }

    /// Encode TTC header to bytes
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = BytesMut::with_capacity(Self::SIZE);
        buf.put_u8(self.function as u8);
        buf.put_u8(self.seq_num);
        buf.put_u8(self.flags);
        buf.to_vec()
    }
}

/// TTC message
#[derive(Debug, Clone)]
pub struct TtcMessage {
    /// TTC header
    pub header: TtcHeader,
    /// Message payload
    pub payload: Vec<u8>,
}

impl TtcMessage {
    /// Create a new TTC message
    pub fn new(function: TtcFunction, payload: Vec<u8>) -> Self {
        let header = TtcHeader {
            function,
            seq_num: 0,
            flags: 0,
        };

        Self { header, payload }
    }

    /// Parse TTC message from bytes
    pub fn parse(data: &[u8]) -> io::Result<Self> {
        let header = TtcHeader::parse(data)?;
        let payload = data[TtcHeader::SIZE..].to_vec();

        Ok(Self { header, payload })
    }

    /// Encode TTC message to bytes
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&self.header.encode());
        buf.extend_from_slice(&self.payload);
        buf.to_vec()
    }
}

/// TTC Parse message data
#[derive(Debug, Clone)]
pub struct TtcParse {
    /// Cursor number
    pub cursor_id: u16,
    /// SQL statement text
    pub sql: String,
    /// Parse options
    pub options: u32,
}

impl TtcParse {
    /// Parse TTC Parse message
    pub fn parse(data: &[u8]) -> io::Result<Self> {
        let mut cursor = Cursor::new(data);

        // Simple parsing - in reality TTC uses a complex wire format
        // This is a simplified version for basic functionality
        let cursor_id = if cursor.remaining() >= 2 {
            cursor.get_u16()
        } else {
            1
        };

        let options = if cursor.remaining() >= 4 {
            cursor.get_u32()
        } else {
            0
        };

        // Remaining bytes are SQL text
        let sql_bytes: Vec<u8> = data[cursor.position() as usize..].to_vec();
        let sql = String::from_utf8_lossy(&sql_bytes).to_string();

        Ok(Self {
            cursor_id,
            sql,
            options,
        })
    }
}

/// TTC Execute message data
#[derive(Debug, Clone)]
pub struct TtcExecute {
    /// Cursor number
    pub cursor_id: u16,
    /// Number of iterations (for batch operations)
    pub iterations: u32,
    /// Execute options
    pub options: u32,
}

impl TtcExecute {
    /// Parse TTC Execute message
    pub fn parse(data: &[u8]) -> io::Result<Self> {
        let mut cursor = Cursor::new(data);

        let cursor_id = if cursor.remaining() >= 2 {
            cursor.get_u16()
        } else {
            1
        };

        let iterations = if cursor.remaining() >= 4 {
            cursor.get_u32()
        } else {
            1
        };

        let options = if cursor.remaining() >= 4 {
            cursor.get_u32()
        } else {
            0
        };

        Ok(Self {
            cursor_id,
            iterations,
            options,
        })
    }
}

/// TTC Fetch message data
#[derive(Debug, Clone)]
pub struct TtcFetch {
    /// Cursor number
    pub cursor_id: u16,
    /// Number of rows to fetch
    pub num_rows: u32,
}

impl TtcFetch {
    /// Parse TTC Fetch message
    pub fn parse(data: &[u8]) -> io::Result<Self> {
        let mut cursor = Cursor::new(data);

        let cursor_id = if cursor.remaining() >= 2 {
            cursor.get_u16()
        } else {
            1
        };

        let num_rows = if cursor.remaining() >= 4 {
            cursor.get_u32()
        } else {
            1
        };

        Ok(Self {
            cursor_id,
            num_rows,
        })
    }
}

/// TTC Logon message data
#[derive(Debug, Clone)]
pub struct TtcLogon {
    /// Username
    pub username: String,
    /// Password (encrypted or plain)
    pub password: String,
    /// Database name/SID
    pub database: String,
}

impl TtcLogon {
    /// Parse TTC Logon message (simplified)
    pub fn parse(data: &[u8]) -> io::Result<Self> {
        // In reality, TTC logon uses complex encoding with O5LOGON, O7LOGON, etc.
        // This is a simplified version that extracts basic auth info

        let text = String::from_utf8_lossy(data).to_string();

        // Try to extract username/password from connection string
        let username = Self::extract_field(&text, "USER=")
            .or_else(|| Self::extract_field(&text, "UID="))
            .unwrap_or_else(|| "helios".to_string());

        let password = Self::extract_field(&text, "PASSWORD=")
            .or_else(|| Self::extract_field(&text, "PWD="))
            .unwrap_or_else(|| "".to_string());

        let database = Self::extract_field(&text, "SERVICE_NAME=")
            .or_else(|| Self::extract_field(&text, "SID="))
            .unwrap_or_else(|| "heliosdb".to_string());

        Ok(Self {
            username,
            password,
            database,
        })
    }

    fn extract_field(text: &str, field_name: &str) -> Option<String> {
        let text_upper = text.to_uppercase();
        if let Some(start) = text_upper.find(field_name) {
            let start = start + field_name.len();
            let end = text_upper[start..]
                .find(|c: char| c == ')' || c == ' ' || c == ';')
                .map(|pos| start + pos)
                .unwrap_or(text.len());
            return Some(text[start..end].to_string());
        }
        None
    }
}

/// TTC Response builder
pub struct TtcResponseBuilder {
    buffer: BytesMut,
}

impl TtcResponseBuilder {
    /// Create a new response builder
    pub fn new() -> Self {
        Self {
            buffer: BytesMut::new(),
        }
    }

    /// Write a response header
    pub fn write_header(&mut self, function: TtcFunction) {
        self.buffer.put_u8(function as u8);
        self.buffer.put_u8(0); // seq_num
        self.buffer.put_u8(0); // flags
    }

    /// Write a row data marker
    pub fn write_row_header(&mut self, num_columns: u16) {
        self.buffer.put_u8(0x15); // Row data marker
        self.buffer.put_u16(num_columns);
    }

    /// Write a column value (text format)
    pub fn write_column(&mut self, value: &str) {
        let bytes = value.as_bytes();
        self.buffer.put_u16(bytes.len() as u16);
        self.buffer.extend_from_slice(bytes);
    }

    /// Write a NULL column
    pub fn write_null_column(&mut self) {
        self.buffer.put_u16(0xFFFF); // NULL marker
    }

    /// Write an error response
    pub fn write_error(&mut self, code: &str, message: &str) {
        self.buffer.put_u8(0x04); // Error marker
        self.buffer.extend_from_slice(code.as_bytes());
        self.buffer.put_u8(0x00);
        self.buffer.extend_from_slice(message.as_bytes());
        self.buffer.put_u8(0x00);
    }

    /// Write end-of-fetch marker
    pub fn write_end_of_fetch(&mut self) {
        self.buffer.put_u8(0x08); // End of fetch marker
    }

    /// Write command complete marker
    pub fn write_command_complete(&mut self, rows_affected: u64) {
        self.buffer.put_u8(0x06); // Command complete marker
        self.buffer.put_u64(rows_affected);
    }

    /// Build the final response
    pub fn build(self) -> Vec<u8> {
        self.buffer.to_vec()
    }
}

impl Default for TtcResponseBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Oracle data type codes (simplified)
#[allow(dead_code)]
pub mod oracle_types {
    pub const VARCHAR2: u8 = 1;
    pub const NUMBER: u8 = 2;
    pub const LONG: u8 = 8;
    pub const DATE: u8 = 12;
    pub const RAW: u8 = 23;
    pub const LONG_RAW: u8 = 24;
    pub const CHAR: u8 = 96;
    pub const BINARY_FLOAT: u8 = 100;
    pub const BINARY_DOUBLE: u8 = 101;
    pub const CLOB: u8 = 112;
    pub const BLOB: u8 = 113;
    pub const TIMESTAMP: u8 = 180;
    pub const TIMESTAMP_TZ: u8 = 181;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_ttc_header_parse() {
        let data = vec![
            0x04, // Parse function
            0x00, // seq_num
            0x00, // flags
        ];

        let header = TtcHeader::parse(&data).unwrap();
        assert_eq!(header.function, TtcFunction::Parse);
    }

    #[test]
    fn test_ttc_message_encode_decode() {
        let payload = vec![1, 2, 3, 4];
        let msg = TtcMessage::new(TtcFunction::Execute, payload.clone());

        let encoded = msg.encode();
        let decoded = TtcMessage::parse(&encoded).unwrap();

        assert_eq!(decoded.header.function, TtcFunction::Execute);
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn test_response_builder() {
        let mut builder = TtcResponseBuilder::new();
        builder.write_header(TtcFunction::Execute);
        builder.write_command_complete(5);

        let response = builder.build();
        assert!(!response.is_empty());
    }
}
