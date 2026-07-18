//! PostgreSQL wire protocol message types
//!
//! This module implements the PostgreSQL wire protocol message format
//! for both frontend (client) and backend (server) messages.
//!
//! Reference: <https://www.postgresql.org/docs/current/protocol-message-formats.html>

#![allow(unused_variables)]

use crate::{Error, Result};
use bytes::{Buf, BufMut, BytesMut};
use std::collections::HashMap;

/// PostgreSQL frontend message type identifier (client to server)
/// Note: In the actual protocol, password-related messages (PasswordMessage,
/// SaslInitialResponse, SaslResponse) all use byte 'p' and are distinguished
/// by authentication context. We assign unique discriminants here for the enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrontendMessageType {
    Query = b'Q',
    Parse = b'P',
    Bind = b'B',
    Execute = b'E',
    Describe = b'D',
    Close = b'C',
    Sync = b'S',
    Terminate = b'X',
    PasswordMessage = b'p',
    // These would be b'p' in protocol but need unique discriminants for enum
    SaslInitialResponse = 200,
    SaslResponse = 201,
}

/// PostgreSQL backend message type identifier (server to client)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BackendMessageType {
    Authentication = b'R',
    BackendKeyData = b'K',
    BindComplete = b'2',
    CloseComplete = b'3',
    CommandComplete = b'C',
    DataRow = b'D',
    EmptyQueryResponse = b'I',
    ErrorResponse = b'E',
    NoData = b'n',
    NoticeResponse = b'N',
    ParameterDescription = b't',
    ParameterStatus = b'S',
    ParseComplete = b'1',
    ReadyForQuery = b'Z',
    RowDescription = b'T',
    CopyInResponse = b'G',
    CopyOutResponse = b'H',
    CopyData = b'd',
    CopyDone = b'c',
}

/// Frontend message (client to server)
#[derive(Debug, Clone)]
pub enum FrontendMessage {
    /// Startup message (no message type byte)
    Startup {
        protocol_version: i32,
        params: HashMap<String, String>,
    },

    /// Simple query protocol
    Query { query: String },

    /// Extended protocol - Parse
    Parse {
        statement_name: String,
        query: String,
        param_types: Vec<i32>,
    },

    /// Extended protocol - Bind
    Bind {
        portal_name: String,
        statement_name: String,
        param_formats: Vec<i16>,
        params: Vec<Option<Vec<u8>>>,
        result_formats: Vec<i16>,
    },

    /// Extended protocol - Execute
    Execute { portal_name: String, max_rows: i32 },

    /// Extended protocol - Describe
    Describe { target: DescribeTarget, name: String },

    /// Extended protocol - Close
    Close { target: DescribeTarget, name: String },

    /// Sync (complete extended protocol sequence)
    Sync,

    /// Flush — force the server to push any buffered response
    /// immediately, without ending the extended-protocol transaction
    /// (that's Sync's job). `postgres-js`, `pg` and every other
    /// pipelined driver emit `Parse, Bind, [Describe,] Execute, Flush`
    /// to get the results back without committing the implicit txn.
    Flush,

    /// Terminate connection
    Terminate,

    /// Password message
    PasswordMessage { password: String },

    /// SASL initial response
    SaslInitialResponse { mechanism: String, data: Vec<u8> },

    /// SASL response (continue)
    SaslResponse { data: Vec<u8> },

    /// COPY ... FROM STDIN data frame (one or more rows of raw payload bytes).
    CopyData(Vec<u8>),
    /// COPY ... FROM STDIN completed normally by the client.
    CopyDone,
    /// COPY ... FROM STDIN aborted by the client (carries the error message).
    CopyFail(String),
}

/// Describe target (statement or portal)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescribeTarget {
    Statement,
    Portal,
}

/// Backend message (server to client)
#[derive(Debug, Clone)]
pub enum BackendMessage {
    /// Authentication request
    Authentication(AuthenticationMessage),

    /// Backend key data (for cancel requests)
    BackendKeyData { process_id: i32, secret_key: i32 },

    /// Bind complete
    BindComplete,

    /// Close complete
    CloseComplete,

    /// Command completion
    CommandComplete { tag: String },

    /// Data row
    DataRow { values: Vec<Option<Vec<u8>>> },

    /// Empty query response
    EmptyQueryResponse,

    /// Error response
    ErrorResponse {
        severity: String,
        code: String,
        message: String,
        detail: Option<String>,
        hint: Option<String>,
        position: Option<i32>,
    },

    /// No data (describe returned no columns)
    NoData,

    /// Notice response
    NoticeResponse {
        severity: String,
        code: String,
        message: String,
    },

    /// Parameter description
    ParameterDescription { param_types: Vec<i32> },

    /// Parameter status
    ParameterStatus { name: String, value: String },

    /// Parse complete
    ParseComplete,

    /// Ready for query
    ReadyForQuery { status: TransactionStatus },

    /// Row description (result set metadata)
    RowDescription { fields: Vec<FieldDescription> },

    /// COPY FROM STDIN: server is ready to receive CopyData frames.
    /// `overall_format` 0=text, 1=binary; `column_formats` per-column codes.
    CopyInResponse {
        overall_format: u8,
        column_formats: Vec<i16>,
    },
    /// COPY TO STDOUT: server is about to stream CopyData rows.
    CopyOutResponse {
        overall_format: u8,
        column_formats: Vec<i16>,
    },
    /// A COPY data frame (raw payload bytes).
    CopyData(Vec<u8>),
    /// COPY stream complete (server -> client).
    CopyDone,
}

/// Authentication message types
#[derive(Debug, Clone)]
pub enum AuthenticationMessage {
    /// Authentication successful
    Ok,

    /// Clear-text password required
    CleartextPassword,

    /// MD5 password required
    Md5Password { salt: [u8; 4] },

    /// SCRAM-SHA-256 authentication
    ScramSha256,

    /// SCRAM-SHA-256 continue
    ScramSha256Continue { data: Vec<u8> },

    /// SCRAM-SHA-256 final
    ScramSha256Final { data: Vec<u8> },
}

/// Transaction status indicator
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TransactionStatus {
    /// Idle (not in a transaction block)
    Idle = b'I',
    /// In transaction block
    InTransaction = b'T',
    /// In failed transaction block
    Failed = b'E',
}

/// Field description for RowDescription message
#[derive(Debug, Clone)]
pub struct FieldDescription {
    pub name: String,
    pub table_oid: i32,
    pub column_attr_num: i16,
    pub data_type_oid: i32,
    pub data_type_size: i16,
    pub type_modifier: i32,
    pub format_code: i16,
}

/// Maximum accepted length (bytes, including the 4-byte length field itself) of
/// a single frontend wire message. A peer claiming more is rejected with a
/// protocol error instead of buffered — otherwise an (even unauthenticated)
/// client could make the server buffer up to 2 GiB per connection, and a
/// negative length would stall the read loop forever.
pub(crate) const MAX_MESSAGE_LEN: usize = 64 * 1024 * 1024;

/// Startup packets only carry the protocol version and a small parameter map;
/// cap them much tighter than regular messages. This bounds the pre-auth
/// allocation in the startup path.
pub(crate) const MAX_STARTUP_MESSAGE_LEN: usize = 1024 * 1024;

impl FrontendMessage {
    /// Parse a frontend message from bytes
    // SAFETY: Buffer offsets are validated (buf.len() >= 5) before indexing buf[0..4].
    #[allow(clippy::indexing_slicing)]
    pub fn parse(buf: &mut BytesMut) -> Result<Option<Self>> {
        // Check if we have enough bytes for message type and length
        if buf.len() < 5 {
            return Ok(None);
        }

        let msg_type = buf[0];
        let len = i32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;

        // Reject impossible or oversized claims before waiting for more bytes:
        // the protocol length includes its own 4 bytes, so anything below 4 is
        // malformed (a `len < 4` CopyData would otherwise underflow below), and
        // an i32 read as usize can also be a reinterpreted negative value.
        if !(4..=MAX_MESSAGE_LEN).contains(&len) {
            return Err(Error::protocol(format!(
                "Invalid frontend message length {} for type 0x{:02x} (max {})",
                len as i64, msg_type, MAX_MESSAGE_LEN
            )));
        }

        // Check if we have the full message
        if buf.len() < 1 + len {
            return Ok(None);
        }

        // Consume message type byte
        buf.advance(1);

        // Parse based on message type
        let message = match msg_type {
            b'Q' => Self::parse_query(buf, len)?,
            b'P' => Self::parse_parse(buf, len)?,
            b'B' => Self::parse_bind(buf, len)?,
            b'E' => Self::parse_execute(buf, len)?,
            b'D' => Self::parse_describe(buf, len)?,
            b'C' => Self::parse_close(buf, len)?,
            b'S' => {
                buf.advance(len);
                FrontendMessage::Sync
            }
            b'H' => {
                // Flush has no payload beyond the length header.
                buf.advance(len);
                FrontendMessage::Flush
            }
            b'X' => {
                buf.advance(len);
                FrontendMessage::Terminate
            }
            b'p' => Self::parse_password(buf, len)?,
            b'd' => {
                // CopyData: the payload is (len - 4) raw bytes.
                buf.advance(4);
                let data = buf.copy_to_bytes(len - 4).to_vec();
                FrontendMessage::CopyData(data)
            }
            b'c' => {
                // CopyDone: no payload beyond the length header.
                buf.advance(len);
                FrontendMessage::CopyDone
            }
            b'f' => {
                // CopyFail: a null-terminated error message.
                buf.advance(4);
                let msg = read_cstring(buf)?;
                FrontendMessage::CopyFail(msg)
            }
            _ => {
                return Err(Error::protocol(format!(
                    "Unknown message type: {} (0x{:02x})",
                    msg_type as char, msg_type
                )));
            }
        };

        Ok(Some(message))
    }

    /// Parse startup message (special case - no message type byte)
    // SAFETY: Buffer length checked (>= 4) before indexing buf[0..3].
    #[allow(clippy::indexing_slicing)]
    pub fn parse_startup(buf: &mut BytesMut) -> Result<Option<Self>> {
        if buf.len() < 4 {
            return Ok(None);
        }

        let len = i32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;

        // Startup length includes the 4 length bytes + 4 protocol-version
        // bytes; cap it tightly — this is reachable before authentication.
        if !(8..=MAX_STARTUP_MESSAGE_LEN).contains(&len) {
            return Err(Error::protocol(format!(
                "Invalid startup message length {} (max {})",
                len as i64, MAX_STARTUP_MESSAGE_LEN
            )));
        }

        if buf.len() < len {
            return Ok(None);
        }

        buf.advance(4); // Skip length
        let protocol_version = buf.get_i32();

        let mut params = HashMap::new();
        while buf.len() > 1 {
            let key = read_cstring(buf)?;
            if key.is_empty() {
                break;
            }
            let value = read_cstring(buf)?;
            params.insert(key, value);
        }

        // Consume final null terminator if present
        if buf.len() > 0 && buf[0] == 0 {
            buf.advance(1);
        }

        Ok(Some(FrontendMessage::Startup {
            protocol_version,
            params,
        }))
    }

    fn parse_query(buf: &mut BytesMut, len: usize) -> Result<Self> {
        buf.advance(4); // Skip length
        let query = read_cstring(buf)?;
        Ok(FrontendMessage::Query { query })
    }

    fn parse_parse(buf: &mut BytesMut, len: usize) -> Result<Self> {
        buf.advance(4); // Skip length
        let statement_name = read_cstring(buf)?;
        let query = read_cstring(buf)?;
        let num_params = get_count_checked(buf, 4, "Parse parameter-type")?;
        let mut param_types = Vec::with_capacity(num_params);
        for _ in 0..num_params {
            param_types.push(get_i32_checked(buf, "Parse parameter type")?);
        }
        Ok(FrontendMessage::Parse {
            statement_name,
            query,
            param_types,
        })
    }

    fn parse_bind(buf: &mut BytesMut, len: usize) -> Result<Self> {
        buf.advance(4); // Skip length
        let portal_name = read_cstring(buf)?;
        let statement_name = read_cstring(buf)?;

        // Parameter formats
        let num_formats = get_count_checked(buf, 2, "Bind parameter-format")?;
        let mut param_formats = Vec::with_capacity(num_formats);
        for _ in 0..num_formats {
            param_formats.push(get_i16_checked(buf, "Bind parameter format")?);
        }

        // Parameters
        let num_params = get_count_checked(buf, 4, "Bind parameter")?;
        let mut params = Vec::with_capacity(num_params);
        for _ in 0..num_params {
            let param_len = get_i32_checked(buf, "Bind parameter length")?;
            if param_len == -1 {
                params.push(None);
            } else {
                if param_len < 0 {
                    return Err(Error::protocol(format!("Invalid Bind parameter length: {}", param_len)));
                }
                let param_len = param_len as usize;
                if param_len > buf.remaining() {
                    return Err(Error::protocol(format!(
                        "Bind parameter length {} exceeds message body ({} bytes remaining)",
                        param_len,
                        buf.remaining()
                    )));
                }
                let mut param_data = vec![0u8; param_len];
                buf.copy_to_slice(&mut param_data);
                params.push(Some(param_data));
            }
        }

        // Result formats
        let num_result_formats = get_count_checked(buf, 2, "Bind result-format")?;
        let mut result_formats = Vec::with_capacity(num_result_formats);
        for _ in 0..num_result_formats {
            result_formats.push(get_i16_checked(buf, "Bind result format")?);
        }

        Ok(FrontendMessage::Bind {
            portal_name,
            statement_name,
            param_formats,
            params,
            result_formats,
        })
    }

    fn parse_execute(buf: &mut BytesMut, len: usize) -> Result<Self> {
        buf.advance(4); // Skip length
        let portal_name = read_cstring(buf)?;
        let max_rows = get_i32_checked(buf, "Execute max-rows")?;
        Ok(FrontendMessage::Execute { portal_name, max_rows })
    }

    fn parse_describe(buf: &mut BytesMut, len: usize) -> Result<Self> {
        buf.advance(4); // Skip length
        let target_byte = get_u8_checked(buf, "Describe target")?;
        let target = match target_byte {
            b'S' => DescribeTarget::Statement,
            b'P' => DescribeTarget::Portal,
            _ => return Err(Error::protocol(format!("Invalid describe target: {}", target_byte))),
        };
        let name = read_cstring(buf)?;
        Ok(FrontendMessage::Describe { target, name })
    }

    fn parse_password(buf: &mut BytesMut, len: usize) -> Result<Self> {
        // BUG-003 (Perf-73): the 'p' message tag covers THREE wire shapes:
        //   - PasswordMessage:    `<password>\0`
        //   - SaslInitialResponse: `<mechanism>\0<i32 data_len><data>`
        //   - SaslResponse:       `<data>` (raw, length = msg body)
        // Differentiate by the first body byte. SaslResponse's data is a
        // SCRAM client-final like `c=biws,r=…,p=…` whose first byte is an
        // ASCII letter (no leading null). PasswordMessage / SaslInitial both
        // start with a printable name followed by `\0`. The unambiguous tell
        // for SaslResponse is "no null byte in the body" — peek to decide.
        buf.advance(4); // Skip length field
        let body_len = len.saturating_sub(4);
        if body_len == 0 {
            return Ok(FrontendMessage::PasswordMessage {
                password: String::new(),
            });
        }
        // Peek the body to see if it contains a null byte.
        let has_null = buf.chunk().iter().take(body_len).any(|b| *b == 0);
        if !has_null {
            // SaslResponse: raw bytes only.
            let mut data = vec![0u8; body_len];
            buf.copy_to_slice(&mut data);
            return Ok(FrontendMessage::SaslResponse { data });
        }
        // Has at least one null: parse first cstring.
        let cstring_start_remaining = buf.remaining();
        let first = read_cstring(buf)?;
        let consumed_by_cstring = cstring_start_remaining - buf.remaining();
        let body_left = body_len.saturating_sub(consumed_by_cstring);
        if body_left == 0 {
            return Ok(FrontendMessage::PasswordMessage { password: first });
        }
        // SaslInitialResponse: cstring + i32 data_len + data.
        if body_left < 4 || buf.remaining() < 4 {
            return Err(Error::protocol("SASL InitialResponse: truncated data length"));
        }
        let data_len_raw = buf.get_i32();
        let data_len = if data_len_raw < 0 { 0 } else { data_len_raw as usize };
        if buf.remaining() < data_len {
            return Err(Error::protocol("SASL InitialResponse: truncated response data"));
        }
        let mut data = vec![0u8; data_len];
        buf.copy_to_slice(&mut data);
        Ok(FrontendMessage::SaslInitialResponse { mechanism: first, data })
    }

    fn parse_close(buf: &mut BytesMut, len: usize) -> Result<Self> {
        buf.advance(4); // Skip length
        let target_byte = get_u8_checked(buf, "Close target")?;
        let target = match target_byte {
            b'S' => DescribeTarget::Statement,
            b'P' => DescribeTarget::Portal,
            _ => return Err(Error::protocol(format!("Invalid close target: {}", target_byte))),
        };
        let name = read_cstring(buf)?;
        Ok(FrontendMessage::Close { target, name })
    }
}

impl BackendMessage {
    /// Encode backend message to bytes
    pub fn encode(&self, buf: &mut BytesMut) {
        match self {
            BackendMessage::Authentication(auth) => {
                Self::encode_authentication(auth, buf);
            }
            BackendMessage::BackendKeyData { process_id, secret_key } => {
                buf.put_u8(BackendMessageType::BackendKeyData as u8);
                buf.put_i32(12); // length (excluding type byte)
                buf.put_i32(*process_id);
                buf.put_i32(*secret_key);
            }
            BackendMessage::ParameterStatus { name, value } => {
                buf.put_u8(BackendMessageType::ParameterStatus as u8);
                let len = 4 + name.len() + 1 + value.len() + 1;
                buf.put_i32(len as i32);
                write_cstring(buf, name);
                write_cstring(buf, value);
            }
            BackendMessage::ReadyForQuery { status } => {
                buf.put_u8(BackendMessageType::ReadyForQuery as u8);
                buf.put_i32(5);
                buf.put_u8(*status as u8);
            }
            BackendMessage::RowDescription { fields } => {
                buf.put_u8(BackendMessageType::RowDescription as u8);

                // Calculate total length
                let mut len = 4 + 2; // length field + field count
                for field in fields {
                    len += field.name.len() + 1 + 4 + 2 + 4 + 2 + 4 + 2;
                }
                buf.put_i32(len as i32);
                buf.put_i16(fields.len() as i16);

                for field in fields {
                    write_cstring(buf, &field.name);
                    buf.put_i32(field.table_oid);
                    buf.put_i16(field.column_attr_num);
                    buf.put_i32(field.data_type_oid);
                    buf.put_i16(field.data_type_size);
                    buf.put_i32(field.type_modifier);
                    buf.put_i16(field.format_code);
                }
            }
            BackendMessage::DataRow { values } => {
                buf.put_u8(b'D');

                // Calculate total length
                let mut len = 4 + 2; // length field + column count
                for value in values {
                    len += 4; // length field
                    if let Some(v) = value {
                        len += v.len();
                    }
                }
                buf.put_i32(len as i32);
                buf.put_i16(values.len() as i16);

                for value in values {
                    match value {
                        Some(v) => {
                            buf.put_i32(v.len() as i32);
                            buf.put_slice(v);
                        }
                        None => {
                            buf.put_i32(-1); // NULL
                        }
                    }
                }
            }
            BackendMessage::CommandComplete { tag } => {
                buf.put_u8(BackendMessageType::CommandComplete as u8);
                let len = 4 + tag.len() + 1;
                buf.put_i32(len as i32);
                write_cstring(buf, tag);
            }
            BackendMessage::ParseComplete => {
                buf.put_u8(BackendMessageType::ParseComplete as u8);
                buf.put_i32(4);
            }
            BackendMessage::BindComplete => {
                buf.put_u8(BackendMessageType::BindComplete as u8);
                buf.put_i32(4);
            }
            BackendMessage::EmptyQueryResponse => {
                buf.put_u8(BackendMessageType::EmptyQueryResponse as u8);
                buf.put_i32(4);
            }
            BackendMessage::NoData => {
                buf.put_u8(BackendMessageType::NoData as u8);
                buf.put_i32(4);
            }
            BackendMessage::ParameterDescription { param_types } => {
                buf.put_u8(BackendMessageType::ParameterDescription as u8);
                // Length = 4 (for length field itself) + 2 (param count) + 4 * num_params
                let len = 4 + 2 + (param_types.len() * 4);
                buf.put_i32(len as i32);
                buf.put_i16(param_types.len() as i16);
                for oid in param_types {
                    buf.put_i32(*oid);
                }
            }
            BackendMessage::CloseComplete => {
                buf.put_u8(BackendMessageType::CloseComplete as u8);
                buf.put_i32(4);
            }
            BackendMessage::ErrorResponse {
                severity,
                code,
                message,
                detail,
                hint,
                position,
            } => {
                buf.put_u8(BackendMessageType::ErrorResponse as u8);

                let mut len = 4 + 1; // length + terminator
                len += 1 + severity.len() + 1; // S field
                len += 1 + code.len() + 1; // C field
                len += 1 + message.len() + 1; // M field
                if let Some(d) = detail {
                    len += 1 + d.len() + 1; // D field
                }
                if let Some(h) = hint {
                    len += 1 + h.len() + 1; // H field
                }
                if let Some(_p) = position {
                    len += 1 + 10 + 1; // P field (approximate)
                }

                buf.put_i32(len as i32);
                buf.put_u8(b'S');
                write_cstring(buf, severity);
                buf.put_u8(b'C');
                write_cstring(buf, code);
                buf.put_u8(b'M');
                write_cstring(buf, message);

                if let Some(d) = detail {
                    buf.put_u8(b'D');
                    write_cstring(buf, d);
                }
                if let Some(h) = hint {
                    buf.put_u8(b'H');
                    write_cstring(buf, h);
                }
                if let Some(p) = position {
                    buf.put_u8(b'P');
                    write_cstring(buf, &p.to_string());
                }

                buf.put_u8(0); // Terminator
            }
            BackendMessage::CopyInResponse {
                overall_format,
                column_formats,
            } => {
                buf.put_u8(BackendMessageType::CopyInResponse as u8);
                let len = 4 + 1 + 2 + column_formats.len() * 2;
                buf.put_i32(len as i32);
                buf.put_u8(*overall_format);
                buf.put_i16(column_formats.len() as i16);
                for f in column_formats {
                    buf.put_i16(*f);
                }
            }
            BackendMessage::CopyOutResponse {
                overall_format,
                column_formats,
            } => {
                buf.put_u8(BackendMessageType::CopyOutResponse as u8);
                let len = 4 + 1 + 2 + column_formats.len() * 2;
                buf.put_i32(len as i32);
                buf.put_u8(*overall_format);
                buf.put_i16(column_formats.len() as i16);
                for f in column_formats {
                    buf.put_i16(*f);
                }
            }
            BackendMessage::CopyData(data) => {
                buf.put_u8(BackendMessageType::CopyData as u8);
                buf.put_i32((4 + data.len()) as i32);
                buf.put_slice(data);
            }
            BackendMessage::CopyDone => {
                buf.put_u8(BackendMessageType::CopyDone as u8);
                buf.put_i32(4);
            }
            _ => {
                // Other message types not yet implemented
            }
        }
    }

    fn encode_authentication(auth: &AuthenticationMessage, buf: &mut BytesMut) {
        buf.put_u8(BackendMessageType::Authentication as u8);

        match auth {
            AuthenticationMessage::Ok => {
                buf.put_i32(8); // length
                buf.put_i32(0); // AuthenticationOk
            }
            AuthenticationMessage::CleartextPassword => {
                buf.put_i32(8);
                buf.put_i32(3); // AuthenticationCleartextPassword
            }
            AuthenticationMessage::Md5Password { salt } => {
                buf.put_i32(12);
                buf.put_i32(5); // AuthenticationMD5Password
                buf.put_slice(salt);
            }
            AuthenticationMessage::ScramSha256 => {
                // AuthenticationSASL message with mechanism list
                // Format: int32(length) + int32(10) + mechanism_name\0 + \0
                let mechanism = b"SCRAM-SHA-256\0";
                buf.put_i32(8 + mechanism.len() as i32 + 1); // +1 for final null terminator
                buf.put_i32(10); // AuthenticationSASL
                buf.put_slice(mechanism); // SCRAM-SHA-256\0
                buf.put_u8(0); // Final null terminator (end of mechanism list)
            }
            AuthenticationMessage::ScramSha256Continue { data } => {
                buf.put_i32(8 + data.len() as i32);
                buf.put_i32(11); // AuthenticationSASLContinue
                buf.put_slice(data);
            }
            AuthenticationMessage::ScramSha256Final { data } => {
                buf.put_i32(8 + data.len() as i32);
                buf.put_i32(12); // AuthenticationSASLFinal
                buf.put_slice(data);
            }
        }
    }
}

/// Read a null-terminated C string from buffer
/// Bounds-checked primitive reads for frontend-message bodies. The outer
/// `parse()` guarantees the *declared* message is fully buffered, but a
/// malformed body can still declare counts/lengths that walk past its own
/// end (into pipelined bytes or off the buffer entirely) — `bytes` panics on
/// overread, so every body-field read must go through these.
fn get_u8_checked(buf: &mut BytesMut, what: &str) -> Result<u8> {
    if buf.remaining() < 1 {
        return Err(Error::protocol(format!("Truncated message: missing {}", what)));
    }
    Ok(buf.get_u8())
}

fn get_i16_checked(buf: &mut BytesMut, what: &str) -> Result<i16> {
    if buf.remaining() < 2 {
        return Err(Error::protocol(format!("Truncated message: missing {}", what)));
    }
    Ok(buf.get_i16())
}

fn get_i32_checked(buf: &mut BytesMut, what: &str) -> Result<i32> {
    if buf.remaining() < 4 {
        return Err(Error::protocol(format!("Truncated message: missing {}", what)));
    }
    Ok(buf.get_i32())
}

/// Read an i16 element count and validate it against the bytes actually
/// available (`min_elem_size` = smallest possible wire size per element), so a
/// hostile count can neither go negative nor force a huge upfront reservation.
fn get_count_checked(buf: &mut BytesMut, min_elem_size: usize, what: &str) -> Result<usize> {
    let n = get_i16_checked(buf, what)?;
    if n < 0 {
        return Err(Error::protocol(format!("Negative {} count: {}", what, n)));
    }
    let n = n as usize;
    if n * min_elem_size > buf.remaining() {
        return Err(Error::protocol(format!(
            "{} count {} exceeds message body ({} bytes remaining)",
            what,
            n,
            buf.remaining()
        )));
    }
    Ok(n)
}

fn read_cstring(buf: &mut BytesMut) -> Result<String> {
    let mut bytes = Vec::new();
    loop {
        if buf.is_empty() {
            return Err(Error::protocol("Unexpected end of buffer while reading C string"));
        }
        let byte = buf.get_u8();
        if byte == 0 {
            break;
        }
        bytes.push(byte);
    }
    String::from_utf8(bytes).map_err(|e| Error::protocol(format!("Invalid UTF-8 in C string: {}", e)))
}

/// Write a null-terminated C string to buffer
fn write_cstring(buf: &mut BytesMut, s: &str) {
    buf.put_slice(s.as_bytes());
    buf.put_u8(0);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn test_ready_for_query_encode() {
        let mut buf = BytesMut::new();
        let msg = BackendMessage::ReadyForQuery {
            status: TransactionStatus::Idle,
        };
        msg.encode(&mut buf);

        assert_eq!(buf[0], b'Z');
        assert_eq!(i32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]), 5);
        assert_eq!(buf[5], b'I');
    }

    #[test]
    fn test_command_complete_encode() {
        let mut buf = BytesMut::new();
        let msg = BackendMessage::CommandComplete {
            tag: "SELECT 1".to_string(),
        };
        msg.encode(&mut buf);

        assert_eq!(buf[0], b'C');
        let len = i32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]);
        assert!(len > 4);
    }

    #[test]
    fn test_copy_in_response_encode() {
        let mut buf = BytesMut::new();
        BackendMessage::CopyInResponse {
            overall_format: 0,
            column_formats: vec![0, 0, 0],
        }
        .encode(&mut buf);
        assert_eq!(buf[0], b'G');
        // len = 4 + 1(format) + 2(count) + 3*2(per-col) = 13
        assert_eq!(i32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]), 13);
        assert_eq!(buf[5], 0); // overall format = text
        assert_eq!(i16::from_be_bytes([buf[6], buf[7]]), 3); // column count
    }

    #[test]
    fn test_copy_data_done_encode() {
        let mut buf = BytesMut::new();
        BackendMessage::CopyData(vec![1, 2, 3, 4]).encode(&mut buf);
        assert_eq!(buf[0], b'd');
        assert_eq!(i32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]), 8); // 4 + 4
        assert_eq!(&buf[5..9], &[1, 2, 3, 4]);

        let mut buf2 = BytesMut::new();
        BackendMessage::CopyDone.encode(&mut buf2);
        assert_eq!(buf2[0], b'c');
        assert_eq!(i32::from_be_bytes([buf2[1], buf2[2], buf2[3], buf2[4]]), 4);
    }

    #[test]
    fn test_copy_frontend_parse_roundtrip() {
        // CopyData: 'd' | len | raw payload
        let payload = b"1\thello\n";
        let mut buf = BytesMut::new();
        buf.put_u8(b'd');
        buf.put_i32((4 + payload.len()) as i32);
        buf.put_slice(payload);
        match FrontendMessage::parse(&mut buf).unwrap().unwrap() {
            FrontendMessage::CopyData(d) => assert_eq!(d, payload),
            other => panic!("expected CopyData, got {:?}", other),
        }

        // CopyDone: 'c' | len(4)
        let mut buf = BytesMut::new();
        buf.put_u8(b'c');
        buf.put_i32(4);
        assert!(matches!(
            FrontendMessage::parse(&mut buf).unwrap().unwrap(),
            FrontendMessage::CopyDone
        ));

        // CopyFail: 'f' | len | cstring
        let mut buf = BytesMut::new();
        let m = b"oops\0";
        buf.put_u8(b'f');
        buf.put_i32((4 + m.len()) as i32);
        buf.put_slice(m);
        match FrontendMessage::parse(&mut buf).unwrap().unwrap() {
            FrontendMessage::CopyFail(s) => assert_eq!(s, "oops"),
            other => panic!("expected CopyFail, got {:?}", other),
        }
    }

    // ---- malformed-frame hardening (S1/S7): every case below must return a
    // clean protocol Err — never panic, never allocate from the wire claim,
    // and never wait for more bytes that will not come. ----

    fn frame(msg_type: u8, len: i32, body: &[u8]) -> BytesMut {
        let mut buf = BytesMut::new();
        buf.put_u8(msg_type);
        buf.put_i32(len);
        buf.put_slice(body);
        buf
    }

    #[test]
    fn copydata_len_below_4_is_error_not_panic() {
        // 5-byte packet `64 00 00 00 02`: len=2 used to underflow
        // `copy_to_bytes(len - 4)` to usize::MAX and panic — pre-auth.
        let mut buf = frame(b'd', 2, &[]);
        assert!(FrontendMessage::parse(&mut buf).is_err());
    }

    #[test]
    fn oversized_length_claim_is_error_not_buffered() {
        // Rejected immediately from the 5 header bytes — must NOT return
        // Ok(None) (which would keep the connection buffering toward 2 GiB).
        let mut buf = frame(b'Q', i32::MAX, &[]);
        assert!(FrontendMessage::parse(&mut buf).is_err());
    }

    #[test]
    fn negative_length_claim_is_error() {
        let mut buf = frame(b'Q', -1, &[]);
        assert!(FrontendMessage::parse(&mut buf).is_err());
    }

    #[test]
    fn bind_negative_format_count_is_error() {
        // 'B' | len=8 | portal "" | stmt "" | num_formats = -1
        let mut body = Vec::new();
        body.extend_from_slice(&[0, 0]); // two empty cstrings
        body.extend_from_slice(&(-1i16).to_be_bytes());
        let mut buf = frame(b'B', (4 + body.len()) as i32, &body);
        assert!(FrontendMessage::parse(&mut buf).is_err());
    }

    #[test]
    fn bind_param_count_beyond_body_is_error_not_panic() {
        // Declares 1000 parameters with an empty remainder: used to run
        // `get_i32()` off the end of the buffer and panic.
        let mut body = Vec::new();
        body.extend_from_slice(&[0, 0]); // portal "", stmt ""
        body.extend_from_slice(&0i16.to_be_bytes()); // num_formats = 0
        body.extend_from_slice(&1000i16.to_be_bytes()); // num_params = 1000
        let mut buf = frame(b'B', (4 + body.len()) as i32, &body);
        assert!(FrontendMessage::parse(&mut buf).is_err());
    }

    #[test]
    fn bind_param_len_beyond_body_is_error_not_giant_alloc() {
        // One parameter claiming ~2 GiB: used to do `vec![0u8; param_len]`
        // before noticing the body is empty.
        let mut body = Vec::new();
        body.extend_from_slice(&[0, 0]);
        body.extend_from_slice(&0i16.to_be_bytes()); // num_formats
        body.extend_from_slice(&1i16.to_be_bytes()); // num_params = 1
        body.extend_from_slice(&0x7FFF_0000i32.to_be_bytes()); // param_len
        let mut buf = frame(b'B', (4 + body.len()) as i32, &body);
        assert!(FrontendMessage::parse(&mut buf).is_err());
    }

    #[test]
    fn parse_param_type_count_beyond_body_is_error_not_panic() {
        // 'P' | stmt "" | query "SELECT 1" | num_params=500 | (no type bytes)
        let mut body = Vec::new();
        body.push(0); // statement_name ""
        body.extend_from_slice(b"SELECT 1\0");
        body.extend_from_slice(&500i16.to_be_bytes());
        let mut buf = frame(b'P', (4 + body.len()) as i32, &body);
        assert!(FrontendMessage::parse(&mut buf).is_err());
    }

    #[test]
    fn execute_truncated_max_rows_is_error_not_panic() {
        // 'E' | portal "" | (no max_rows i32)
        let mut buf = frame(b'E', 5, &[0]);
        assert!(FrontendMessage::parse(&mut buf).is_err());
    }

    #[test]
    fn describe_empty_body_is_error_not_panic() {
        let mut buf = frame(b'D', 4, &[]);
        assert!(FrontendMessage::parse(&mut buf).is_err());
    }

    #[test]
    fn startup_length_bounds_are_enforced() {
        // Undersized (< 8: length + protocol version).
        let mut buf = BytesMut::new();
        buf.put_i32(4);
        assert!(FrontendMessage::parse_startup(&mut buf).is_err());

        // Oversized claim must error immediately, not buffer toward it —
        // this is the pre-auth 2 GiB allocation vector.
        let mut buf = BytesMut::new();
        buf.put_i32(i32::MAX);
        assert!(FrontendMessage::parse_startup(&mut buf).is_err());
    }

    #[test]
    fn valid_bind_still_parses_after_hardening() {
        // 'B' | portal "" | stmt "s" | 1 format (binary) | 1 param (4 bytes) |
        // 1 result format (text) — the happy path must be unaffected.
        let mut body = Vec::new();
        body.push(0); // portal ""
        body.extend_from_slice(b"s\0");
        body.extend_from_slice(&1i16.to_be_bytes());
        body.extend_from_slice(&1i16.to_be_bytes()); // format = binary
        body.extend_from_slice(&1i16.to_be_bytes()); // num_params
        body.extend_from_slice(&4i32.to_be_bytes());
        body.extend_from_slice(&42i32.to_be_bytes());
        body.extend_from_slice(&1i16.to_be_bytes()); // num_result_formats
        body.extend_from_slice(&0i16.to_be_bytes());
        let mut buf = frame(b'B', (4 + body.len()) as i32, &body);
        match FrontendMessage::parse(&mut buf).unwrap().unwrap() {
            FrontendMessage::Bind {
                statement_name,
                param_formats,
                params,
                result_formats,
                ..
            } => {
                assert_eq!(statement_name, "s");
                assert_eq!(param_formats, vec![1]);
                assert_eq!(params, vec![Some(42i32.to_be_bytes().to_vec())]);
                assert_eq!(result_formats, vec![0]);
            }
            other => panic!("expected Bind, got {:?}", other),
        }
    }
}
