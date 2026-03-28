#![allow(dead_code)]

/// MySQL Extended Protocol Features
///
/// Implements advanced MySQL protocol features:
/// - Multi-statement queries
/// - Binary protocol for prepared statements
/// - LOCAL INFILE support
/// - Compression protocol
/// - Enhanced prepared statement lifecycle
use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info};

// These types will be provided by the handler module (another agent).
// For now, define minimal stubs so this file compiles standalone.
// Once handler.rs is in place, switch to: use super::handler::{ColumnType, PacketHeader, StatusFlags};

/// MySQL Status Flags (stub until handler module is wired)
pub struct StatusFlags;

impl StatusFlags {
    pub const SERVER_MORE_RESULTS_EXISTS: u16 = 0x0008;
}

/// MySQL Packet Header (stub until handler module is wired)
#[derive(Debug, Clone)]
pub struct PacketHeader {
    pub payload_length: u32,
    pub sequence_id: u8,
}

impl PacketHeader {
    pub fn new(payload_length: u32, sequence_id: u8) -> Self {
        Self {
            payload_length,
            sequence_id,
        }
    }

    pub fn encode(&self, buf: &mut BytesMut) {
        // 3 bytes for length (little-endian)
        buf.put_u8((self.payload_length & 0xFF) as u8);
        buf.put_u8(((self.payload_length >> 8) & 0xFF) as u8);
        buf.put_u8(((self.payload_length >> 16) & 0xFF) as u8);
        // 1 byte for sequence
        buf.put_u8(self.sequence_id);
    }
}

/// MySQL Column Types
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u8)]
pub enum ColumnType {
    Decimal = 0x00,
    Tiny = 0x01,
    Short = 0x02,
    Long = 0x03,
    Float = 0x04,
    Double = 0x05,
    Null = 0x06,
    Timestamp = 0x07,
    LongLong = 0x08,
    Int24 = 0x09,
    Date = 0x0a,
    Time = 0x0b,
    DateTime = 0x0c,
    Year = 0x0d,
    NewDate = 0x0e,
    VarChar = 0x0f,
    Bit = 0x10,
    Timestamp2 = 0x11,
    DateTime2 = 0x12,
    Time2 = 0x13,
    Json = 0xf5,
    NewDecimal = 0xf6,
    Enum = 0xf7,
    Set = 0xf8,
    TinyBlob = 0xf9,
    MediumBlob = 0xfa,
    LongBlob = 0xfb,
    Blob = 0xfc,
    VarString = 0xfd,
    String = 0xfe,
    Geometry = 0xff,
}

/// Multi-statement query handler
#[derive(Debug)]
pub struct MultiStatementHandler {
    statements: Vec<String>,
    current_index: usize,
    has_more_results: bool,
}

impl MultiStatementHandler {
    /// Parse multi-statement query (separated by semicolons)
    pub fn parse(query: &str) -> Self {
        let statements: Vec<String> = query
            .split(';')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        Self {
            current_index: 0,
            has_more_results: statements.len() > 1,
            statements,
        }
    }

    /// Get next statement to execute
    pub fn next_statement(&mut self) -> Option<&str> {
        if self.current_index < self.statements.len() {
            let stmt = &self.statements[self.current_index];
            self.current_index += 1;
            self.has_more_results = self.current_index < self.statements.len();
            Some(stmt)
        } else {
            None
        }
    }

    /// Check if there are more results to process
    pub fn has_more(&self) -> bool {
        self.has_more_results
    }

    /// Get status flags for current statement
    pub fn status_flags(&self, base_flags: u16) -> u16 {
        if self.has_more_results {
            base_flags | StatusFlags::SERVER_MORE_RESULTS_EXISTS
        } else {
            base_flags & !StatusFlags::SERVER_MORE_RESULTS_EXISTS
        }
    }
}

/// Binary protocol prepared statement
#[derive(Debug, Clone)]
pub struct BinaryPreparedStatement {
    pub statement_id: u32,
    pub sql: String,
    pub num_params: u16,
    pub num_columns: u16,
    pub param_types: Vec<BinaryFieldType>,
    pub column_types: Vec<BinaryFieldType>,
}

/// Binary field type descriptor
#[derive(Debug, Clone, Copy)]
pub struct BinaryFieldType {
    pub type_code: ColumnType,
    pub flags: u16,
    pub decimals: u8,
}

impl BinaryFieldType {
    pub fn new(type_code: ColumnType) -> Self {
        Self {
            type_code,
            flags: 0,
            decimals: 0,
        }
    }

    pub fn with_flags(mut self, flags: u16) -> Self {
        self.flags = flags;
        self
    }

    pub fn with_decimals(mut self, decimals: u8) -> Self {
        self.decimals = decimals;
        self
    }
}

/// Binary protocol encoder/decoder
pub struct BinaryProtocol;

impl BinaryProtocol {
    /// Encode value in binary format
    pub fn encode_value(field_type: &BinaryFieldType, value: &str) -> io::Result<Bytes> {
        let mut buf = BytesMut::new();

        match field_type.type_code {
            ColumnType::Tiny => {
                let val: u8 = value
                    .parse()
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                buf.put_u8(val);
            }
            ColumnType::Short => {
                let val: i16 = value
                    .parse()
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                buf.put_i16_le(val);
            }
            ColumnType::Long => {
                let val: i32 = value
                    .parse()
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                buf.put_i32_le(val);
            }
            ColumnType::LongLong => {
                let val: i64 = value
                    .parse()
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                buf.put_i64_le(val);
            }
            ColumnType::Float => {
                let val: f32 = value
                    .parse()
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                buf.put_f32_le(val);
            }
            ColumnType::Double => {
                let val: f64 = value
                    .parse()
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                buf.put_f64_le(val);
            }
            _ => {
                // For strings and other types, use length-encoded string
                Self::write_length_encoded_string(&mut buf, value);
            }
        }

        Ok(buf.freeze())
    }

    /// Decode binary value to string
    pub fn decode_value(field_type: &BinaryFieldType, mut data: Bytes) -> io::Result<String> {
        match field_type.type_code {
            ColumnType::Tiny => {
                if data.len() != 1 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid tiny length"));
                }
                Ok(data.get_u8().to_string())
            }
            ColumnType::Short => {
                if data.len() != 2 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid short length"));
                }
                Ok(data.get_i16_le().to_string())
            }
            ColumnType::Long => {
                if data.len() != 4 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid long length"));
                }
                Ok(data.get_i32_le().to_string())
            }
            ColumnType::LongLong => {
                if data.len() != 8 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid longlong length"));
                }
                Ok(data.get_i64_le().to_string())
            }
            ColumnType::Float => {
                if data.len() != 4 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid float length"));
                }
                Ok(data.get_f32_le().to_string())
            }
            ColumnType::Double => {
                if data.len() != 8 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid double length"));
                }
                Ok(data.get_f64_le().to_string())
            }
            _ => {
                // Assume string
                String::from_utf8(data.to_vec()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
            }
        }
    }

    /// Parse binary result row
    pub fn parse_binary_row(data: &[u8], column_types: &[BinaryFieldType]) -> io::Result<Vec<Option<String>>> {
        let mut buf = Bytes::copy_from_slice(data);

        // Skip packet header (0x00)
        if buf.is_empty() || buf.get_u8() != 0x00 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid binary row header"));
        }

        // Parse NULL bitmap
        let null_bitmap_len = (column_types.len() + 7 + 2) / 8;
        if buf.remaining() < null_bitmap_len {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid NULL bitmap"));
        }

        let mut null_bitmap = vec![0u8; null_bitmap_len];
        for byte in &mut null_bitmap {
            *byte = buf.get_u8();
        }

        // Parse column values
        let mut values = Vec::new();
        for (i, field_type) in column_types.iter().enumerate() {
            let byte_pos = (i + 2) / 8;
            let bit_pos = (i + 2) % 8;

            if (null_bitmap[byte_pos] & (1 << bit_pos)) != 0 {
                // NULL value
                values.push(None);
            } else {
                // Non-NULL value
                let value = match field_type.type_code {
                    ColumnType::Tiny => {
                        if buf.remaining() < 1 {
                            return Err(io::Error::new(io::ErrorKind::InvalidData, "Insufficient data"));
                        }
                        buf.get_u8().to_string()
                    }
                    ColumnType::Short => {
                        if buf.remaining() < 2 {
                            return Err(io::Error::new(io::ErrorKind::InvalidData, "Insufficient data"));
                        }
                        buf.get_i16_le().to_string()
                    }
                    ColumnType::Long => {
                        if buf.remaining() < 4 {
                            return Err(io::Error::new(io::ErrorKind::InvalidData, "Insufficient data"));
                        }
                        buf.get_i32_le().to_string()
                    }
                    ColumnType::LongLong => {
                        if buf.remaining() < 8 {
                            return Err(io::Error::new(io::ErrorKind::InvalidData, "Insufficient data"));
                        }
                        buf.get_i64_le().to_string()
                    }
                    ColumnType::Float => {
                        if buf.remaining() < 4 {
                            return Err(io::Error::new(io::ErrorKind::InvalidData, "Insufficient data"));
                        }
                        buf.get_f32_le().to_string()
                    }
                    ColumnType::Double => {
                        if buf.remaining() < 8 {
                            return Err(io::Error::new(io::ErrorKind::InvalidData, "Insufficient data"));
                        }
                        buf.get_f64_le().to_string()
                    }
                    _ => {
                        // Length-encoded string
                        let len = Self::read_length_encoded_integer(&mut buf)?;
                        if buf.remaining() < len as usize {
                            return Err(io::Error::new(io::ErrorKind::InvalidData, "Insufficient data"));
                        }
                        let bytes = buf.copy_to_bytes(len as usize);
                        String::from_utf8(bytes.to_vec()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
                    }
                };
                values.push(Some(value));
            }
        }

        Ok(values)
    }

    fn write_length_encoded_string(buf: &mut BytesMut, value: &str) {
        Self::write_length_encoded_integer(buf, value.len() as u64);
        buf.put_slice(value.as_bytes());
    }

    fn write_length_encoded_integer(buf: &mut BytesMut, value: u64) {
        if value < 251 {
            buf.put_u8(value as u8);
        } else if value < 65536 {
            buf.put_u8(0xFC);
            buf.put_u16_le(value as u16);
        } else if value < 16_777_216 {
            buf.put_u8(0xFD);
            buf.put_u8((value & 0xFF) as u8);
            buf.put_u8(((value >> 8) & 0xFF) as u8);
            buf.put_u8(((value >> 16) & 0xFF) as u8);
        } else {
            buf.put_u8(0xFE);
            buf.put_u64_le(value);
        }
    }

    fn read_length_encoded_integer(buf: &mut Bytes) -> io::Result<u64> {
        if buf.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Empty buffer"));
        }

        let first_byte = buf.get_u8();

        match first_byte {
            0xFB => Ok(0), // NULL
            0xFC => {
                if buf.remaining() < 2 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "Insufficient bytes"));
                }
                Ok(u64::from(buf.get_u16_le()))
            }
            0xFD => {
                if buf.remaining() < 3 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "Insufficient bytes"));
                }
                let b1 = u64::from(buf.get_u8());
                let b2 = u64::from(buf.get_u8());
                let b3 = u64::from(buf.get_u8());
                Ok(b1 | (b2 << 8) | (b3 << 16))
            }
            0xFE => {
                if buf.remaining() < 8 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "Insufficient bytes"));
                }
                Ok(buf.get_u64_le())
            }
            _ => Ok(u64::from(first_byte)),
        }
    }
}

/// LOCAL INFILE support
pub struct LocalInfileHandler {
    filename: String,
    state: InfileState,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum InfileState {
    Ready,
    Sending,
    Complete,
}

impl LocalInfileHandler {
    pub fn new(filename: String) -> Self {
        Self {
            filename,
            state: InfileState::Ready,
        }
    }

    /// Send LOCAL INFILE request to client
    pub async fn send_infile_request(&mut self, stream: &mut TcpStream, sequence_id: u8) -> io::Result<()> {
        let mut payload = BytesMut::new();
        payload.put_u8(0xFB); // LOCAL INFILE request
        payload.put_slice(self.filename.as_bytes());

        let header = PacketHeader::new(payload.len() as u32, sequence_id);
        let mut buf = BytesMut::new();
        header.encode(&mut buf);
        buf.put_slice(&payload);

        stream.write_all(&buf).await?;
        stream.flush().await?;

        self.state = InfileState::Sending;
        debug!("Sent LOCAL INFILE request for: {}", self.filename);
        Ok(())
    }

    /// Receive file data from client
    pub async fn receive_file_data(&mut self, stream: &mut TcpStream) -> io::Result<Vec<Vec<u8>>> {
        let mut chunks = Vec::new();

        loop {
            // Read packet header
            let mut header_buf = [0u8; 4];
            stream.read_exact(&mut header_buf).await?;

            let payload_length = u32::from_le_bytes([header_buf[0], header_buf[1], header_buf[2], 0]);
            let _sequence_id = header_buf[3];

            if payload_length == 0 {
                // Empty packet signals end of data
                break;
            }

            // Read payload
            let mut payload = vec![0u8; payload_length as usize];
            stream.read_exact(&mut payload).await?;

            chunks.push(payload);
        }

        self.state = InfileState::Complete;
        info!("Received {} chunks from LOCAL INFILE", chunks.len());
        Ok(chunks)
    }
}

/// Compression protocol support
pub struct CompressionHandler {
    enabled: bool,
    threshold: usize,
}

impl CompressionHandler {
    pub fn new(threshold: usize) -> Self {
        Self {
            enabled: false,
            threshold,
        }
    }

    pub fn enable(&mut self) {
        self.enabled = true;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Compress packet payload if it exceeds threshold
    #[cfg(feature = "compression")]
    pub fn compress(&self, data: &[u8]) -> io::Result<Bytes> {
        if !self.enabled || data.len() < self.threshold {
            return Ok(Bytes::copy_from_slice(data));
        }

        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write;

        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data)?;
        let compressed = encoder.finish()?;

        Ok(Bytes::from(compressed))
    }

    /// Decompress packet payload
    #[cfg(feature = "compression")]
    pub fn decompress(&self, data: &[u8], _uncompressed_length: u32) -> io::Result<Bytes> {
        if !self.enabled {
            return Ok(Bytes::copy_from_slice(data));
        }

        use flate2::read::ZlibDecoder;
        use std::io::Read;

        let mut decoder = ZlibDecoder::new(data);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed)?;

        Ok(Bytes::from(decompressed))
    }

    #[cfg(not(feature = "compression"))]
    pub fn compress(&self, data: &[u8]) -> io::Result<Bytes> {
        Ok(Bytes::copy_from_slice(data))
    }

    #[cfg(not(feature = "compression"))]
    pub fn decompress(&self, data: &[u8], _uncompressed_length: u32) -> io::Result<Bytes> {
        Ok(Bytes::copy_from_slice(data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_multi_statement_parse() {
        let query = "SELECT 1; SELECT 2; SELECT 3";
        let mut handler = MultiStatementHandler::parse(query);

        assert_eq!(handler.next_statement(), Some("SELECT 1"));
        assert!(handler.has_more());

        assert_eq!(handler.next_statement(), Some("SELECT 2"));
        assert!(handler.has_more());

        assert_eq!(handler.next_statement(), Some("SELECT 3"));
        assert!(!handler.has_more());

        assert_eq!(handler.next_statement(), None);
    }

    #[test]
    fn test_binary_protocol_encode_decode() {
        let field_type = BinaryFieldType::new(ColumnType::Long);

        let encoded = BinaryProtocol::encode_value(&field_type, "12345").expect("encode failed");
        let decoded = BinaryProtocol::decode_value(&field_type, encoded).expect("decode failed");

        assert_eq!(decoded, "12345");
    }

    #[test]
    fn test_binary_field_type() {
        let field = BinaryFieldType::new(ColumnType::NewDecimal)
            .with_flags(128)
            .with_decimals(2);

        assert_eq!(field.flags, 128);
        assert_eq!(field.decimals, 2);
    }

    #[test]
    fn test_local_infile_handler() {
        let handler = LocalInfileHandler::new("data.csv".to_string());
        assert_eq!(handler.filename, "data.csv");
        assert_eq!(handler.state, InfileState::Ready);
    }
}
