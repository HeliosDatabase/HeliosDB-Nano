//! MySQL wire protocol handler for HeliosDB Nano.
//!
//! Implements the MySQL protocol v10 (HandshakeV10, COM_QUERY, COM_STMT_*,
//! etc.) on top of Nano's [`EmbeddedDatabase`] API.  All SQL execution is
//! delegated to `EmbeddedDatabase::execute()` / `query_with_columns()`, making
//! this handler dramatically simpler than Full's storage-level implementation.
//!
//! Protocol reference:
//! <https://dev.mysql.com/doc/dev/mysql-server/latest/page_protocol.html>

#![allow(dead_code, unused_variables)]

use bytes::{Buf, BufMut, Bytes, BytesMut};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::ErrorKind;
use std::sync::{Arc, OnceLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, error, info, warn};

use regex::Regex;

use crate::{EmbeddedDatabase, Tuple, Value};

// ============================================================================
// Constants
// ============================================================================

const PROTOCOL_VERSION: u8 = 10;
const SERVER_VERSION: &str = "8.0.35-HeliosDB-Nano";

/// Default character set: utf8mb4_general_ci.
const UTF8MB4_GENERAL_CI: u8 = 45;

// ============================================================================
// Capability Flags
// ============================================================================

/// MySQL capability flags (bitmask).
///
/// Reference: <https://dev.mysql.com/doc/dev/mysql-server/latest/group__group__cs__capabilities__flags.html>
#[derive(Debug, Clone, Copy)]
pub struct CapabilityFlags(u32);

impl CapabilityFlags {
    pub const CLIENT_LONG_PASSWORD: u32                  = 0x0000_0001;
    pub const CLIENT_FOUND_ROWS: u32                     = 0x0000_0002;
    pub const CLIENT_LONG_FLAG: u32                      = 0x0000_0004;
    pub const CLIENT_CONNECT_WITH_DB: u32                = 0x0000_0008;
    pub const CLIENT_NO_SCHEMA: u32                      = 0x0000_0010;
    pub const CLIENT_COMPRESS: u32                       = 0x0000_0020;
    pub const CLIENT_ODBC: u32                           = 0x0000_0040;
    pub const CLIENT_LOCAL_FILES: u32                    = 0x0000_0080;
    pub const CLIENT_IGNORE_SPACE: u32                   = 0x0000_0100;
    pub const CLIENT_PROTOCOL_41: u32                    = 0x0000_0200;
    pub const CLIENT_INTERACTIVE: u32                    = 0x0000_0400;
    pub const CLIENT_SSL: u32                            = 0x0000_0800;
    pub const CLIENT_IGNORE_SIGPIPE: u32                 = 0x0000_1000;
    pub const CLIENT_TRANSACTIONS: u32                   = 0x0000_2000;
    pub const CLIENT_RESERVED: u32                       = 0x0000_4000;
    pub const CLIENT_SECURE_CONNECTION: u32              = 0x0000_8000;
    pub const CLIENT_MULTI_STATEMENTS: u32               = 0x0001_0000;
    pub const CLIENT_MULTI_RESULTS: u32                  = 0x0002_0000;
    pub const CLIENT_PS_MULTI_RESULTS: u32               = 0x0004_0000;
    pub const CLIENT_PLUGIN_AUTH: u32                    = 0x0008_0000;
    pub const CLIENT_CONNECT_ATTRS: u32                  = 0x0010_0000;
    pub const CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA: u32 = 0x0020_0000;
    pub const CLIENT_CAN_HANDLE_EXPIRED_PASSWORDS: u32   = 0x0040_0000;
    pub const CLIENT_SESSION_TRACK: u32                  = 0x0080_0000;
    pub const CLIENT_DEPRECATE_EOF: u32                  = 0x0100_0000;

    pub fn new(flags: u32) -> Self {
        Self(flags)
    }

    pub fn has(&self, flag: u32) -> bool {
        (self.0 & flag) != 0
    }

    pub fn set(&mut self, flag: u32) {
        self.0 |= flag;
    }

    pub fn as_u32(&self) -> u32 {
        self.0
    }

    /// Sensible server-side defaults.
    pub fn server_default() -> Self {
        Self(
            Self::CLIENT_LONG_PASSWORD
                | Self::CLIENT_FOUND_ROWS
                | Self::CLIENT_LONG_FLAG
                | Self::CLIENT_CONNECT_WITH_DB
                | Self::CLIENT_NO_SCHEMA
                | Self::CLIENT_ODBC
                | Self::CLIENT_LOCAL_FILES
                | Self::CLIENT_IGNORE_SPACE
                | Self::CLIENT_PROTOCOL_41
                | Self::CLIENT_INTERACTIVE
                | Self::CLIENT_IGNORE_SIGPIPE
                | Self::CLIENT_TRANSACTIONS
                | Self::CLIENT_SECURE_CONNECTION
                | Self::CLIENT_MULTI_STATEMENTS
                | Self::CLIENT_MULTI_RESULTS
                | Self::CLIENT_PS_MULTI_RESULTS
                | Self::CLIENT_PLUGIN_AUTH
                | Self::CLIENT_CONNECT_ATTRS
                | Self::CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA
                | Self::CLIENT_SESSION_TRACK
                | Self::CLIENT_DEPRECATE_EOF,
        )
    }
}

// ============================================================================
// Status Flags
// ============================================================================

#[derive(Debug, Clone, Copy)]
pub struct StatusFlags(u16);

impl StatusFlags {
    pub const SERVER_STATUS_IN_TRANS: u16     = 0x0001;
    pub const SERVER_STATUS_AUTOCOMMIT: u16  = 0x0002;
    pub const SERVER_MORE_RESULTS_EXISTS: u16 = 0x0008;

    pub fn new(flags: u16) -> Self {
        Self(flags)
    }

    pub fn has(&self, flag: u16) -> bool {
        (self.0 & flag) != 0
    }

    pub fn set(&mut self, flag: u16) {
        self.0 |= flag;
    }

    pub fn clear(&mut self, flag: u16) {
        self.0 &= !flag;
    }

    pub fn as_u16(&self) -> u16 {
        self.0
    }

    pub fn default_flags() -> Self {
        Self(Self::SERVER_STATUS_AUTOCOMMIT)
    }
}

// ============================================================================
// MySQL Column Types
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u8)]
pub enum ColumnType {
    Decimal    = 0x00,
    Tiny       = 0x01,
    Short      = 0x02,
    Long       = 0x03,
    Float      = 0x04,
    Double     = 0x05,
    Null       = 0x06,
    Timestamp  = 0x07,
    LongLong   = 0x08,
    Int24      = 0x09,
    Date       = 0x0a,
    Time       = 0x0b,
    DateTime   = 0x0c,
    Year       = 0x0d,
    VarChar    = 0x0f,
    Bit        = 0x10,
    Json       = 0xf5,
    NewDecimal = 0xf6,
    Blob       = 0xfc,
    VarString  = 0xfd,
    String     = 0xfe,
}

impl ColumnType {
    /// Map a Nano `Value` to the closest MySQL column type.
    fn from_value(v: &Value) -> Self {
        match v {
            Value::Null       => ColumnType::Null,
            Value::Boolean(_) => ColumnType::Tiny,
            Value::Int2(_)    => ColumnType::Short,
            Value::Int4(_)    => ColumnType::Long,
            Value::Int8(_)    => ColumnType::LongLong,
            Value::Float4(_)  => ColumnType::Float,
            Value::Float8(_)  => ColumnType::Double,
            Value::Numeric(_) => ColumnType::NewDecimal,
            Value::String(_)  => ColumnType::VarString,
            Value::Bytes(_)   => ColumnType::Blob,
            Value::Uuid(_)    => ColumnType::VarString,
            Value::Timestamp(_) => ColumnType::Timestamp,
            Value::Date(_)    => ColumnType::Date,
            Value::Time(_)    => ColumnType::Time,
            Value::Interval(_) => ColumnType::VarString,
            Value::Json(_)    => ColumnType::Json,
            Value::Array(_)   => ColumnType::Json,
            Value::Vector(_)  => ColumnType::Json,
            Value::DictRef { .. } => ColumnType::LongLong,
            Value::CasRef { .. }  => ColumnType::VarString,
            Value::ColumnarRef    => ColumnType::VarString,
        }
    }
}

// ============================================================================
// Command byte constants
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u8)]
pub enum Command {
    ComQuit            = 0x01,
    ComInitDb          = 0x02,
    ComQuery           = 0x03,
    ComFieldList       = 0x04,
    ComStatistics      = 0x09,
    ComPing            = 0x0e,
    ComStmtPrepare     = 0x16,
    ComStmtExecute     = 0x17,
    ComStmtClose       = 0x19,
    ComStmtReset       = 0x1a,
    ComSetOption       = 0x1b,
    ComResetConnection = 0x1f,
}

impl Command {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0x01 => Some(Self::ComQuit),
            0x02 => Some(Self::ComInitDb),
            0x03 => Some(Self::ComQuery),
            0x04 => Some(Self::ComFieldList),
            0x09 => Some(Self::ComStatistics),
            0x0e => Some(Self::ComPing),
            0x16 => Some(Self::ComStmtPrepare),
            0x17 => Some(Self::ComStmtExecute),
            0x19 => Some(Self::ComStmtClose),
            0x1a => Some(Self::ComStmtReset),
            0x1b => Some(Self::ComSetOption),
            0x1f => Some(Self::ComResetConnection),
            _    => None,
        }
    }
}

// ============================================================================
// Error type
// ============================================================================

#[derive(Debug)]
pub enum MySqlError {
    Io(std::io::Error),
    Protocol(String),
    ConnectionClosed,
    Unsupported(u8),
    StatementNotFound(u32),
    Db(crate::Error),
}

impl From<std::io::Error> for MySqlError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<crate::Error> for MySqlError {
    fn from(e: crate::Error) -> Self {
        Self::Db(e)
    }
}

impl std::fmt::Display for MySqlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e)            => write!(f, "IO: {}", e),
            Self::Protocol(msg)    => write!(f, "Protocol: {}", msg),
            Self::ConnectionClosed => write!(f, "Connection closed"),
            Self::Unsupported(c)   => write!(f, "Unsupported command: 0x{:02x}", c),
            Self::StatementNotFound(id) => write!(f, "Statement {} not found", id),
            Self::Db(e)            => write!(f, "DB: {}", e),
        }
    }
}

pub type Result<T> = std::result::Result<T, MySqlError>;

// ============================================================================
// Low-level packet I/O
// ============================================================================

/// Read one MySQL packet (3-byte length + 1-byte seq + payload).
async fn read_packet(stream: &mut TcpStream) -> Result<(u8, Bytes)> {
    let mut hdr = [0u8; 4];
    stream.read_exact(&mut hdr).await.map_err(|e| {
        if e.kind() == ErrorKind::UnexpectedEof {
            MySqlError::ConnectionClosed
        } else {
            MySqlError::Io(e)
        }
    })?;
    let len = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], 0]) as usize;
    let seq = hdr[3];
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    Ok((seq, Bytes::from(payload)))
}

/// Write one MySQL packet.
async fn write_packet(stream: &mut TcpStream, seq: u8, payload: &[u8]) -> Result<()> {
    let len = payload.len() as u32;
    let mut buf = BytesMut::with_capacity(4 + payload.len());
    buf.put_u8((len & 0xFF) as u8);
    buf.put_u8(((len >> 8) & 0xFF) as u8);
    buf.put_u8(((len >> 16) & 0xFF) as u8);
    buf.put_u8(seq);
    buf.put_slice(payload);
    stream.write_all(&buf).await?;
    stream.flush().await?;
    Ok(())
}

// ============================================================================
// Length-encoded integer/string helpers
// ============================================================================

fn write_lenenc_int(buf: &mut BytesMut, value: u64) {
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

fn write_lenenc_str(buf: &mut BytesMut, s: &str) {
    write_lenenc_int(buf, s.len() as u64);
    buf.put_slice(s.as_bytes());
}

fn read_lenenc_int(buf: &mut Bytes) -> Result<u64> {
    if buf.is_empty() {
        return Err(MySqlError::Protocol("empty buffer in lenenc_int".into()));
    }
    let first = buf.get_u8();
    match first {
        0xFB => Ok(0),
        0xFC => {
            if buf.remaining() < 2 {
                return Err(MySqlError::Protocol("short lenenc_int (2)".into()));
            }
            Ok(u64::from(buf.get_u16_le()))
        }
        0xFD => {
            if buf.remaining() < 3 {
                return Err(MySqlError::Protocol("short lenenc_int (3)".into()));
            }
            let b1 = u64::from(buf.get_u8());
            let b2 = u64::from(buf.get_u8());
            let b3 = u64::from(buf.get_u8());
            Ok(b1 | (b2 << 8) | (b3 << 16))
        }
        0xFE => {
            if buf.remaining() < 8 {
                return Err(MySqlError::Protocol("short lenenc_int (8)".into()));
            }
            Ok(buf.get_u64_le())
        }
        _ => Ok(u64::from(first)),
    }
}

fn read_lenenc_str(buf: &mut Bytes) -> Result<String> {
    let len = read_lenenc_int(buf)? as usize;
    if buf.remaining() < len {
        return Err(MySqlError::Protocol("short lenenc_str".into()));
    }
    let bytes = buf.copy_to_bytes(len);
    String::from_utf8(bytes.to_vec())
        .map_err(|e| MySqlError::Protocol(format!("invalid utf-8: {}", e)))
}

fn read_lenenc_bytes(buf: &mut Bytes) -> Result<Vec<u8>> {
    let len = read_lenenc_int(buf)? as usize;
    if buf.remaining() < len {
        return Err(MySqlError::Protocol("short lenenc_bytes".into()));
    }
    Ok(buf.copy_to_bytes(len).to_vec())
}

fn read_null_terminated(buf: &mut Bytes) -> Result<String> {
    let mut out = Vec::new();
    loop {
        if buf.is_empty() {
            return Err(MySqlError::Protocol("unterminated null string".into()));
        }
        let b = buf.get_u8();
        if b == 0 {
            break;
        }
        out.push(b);
    }
    String::from_utf8(out)
        .map_err(|e| MySqlError::Protocol(format!("invalid utf-8: {}", e)))
}

fn read_null_terminated_bytes(buf: &mut Bytes) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        if buf.is_empty() {
            return Err(MySqlError::Protocol("unterminated null bytes".into()));
        }
        let b = buf.get_u8();
        if b == 0 {
            break;
        }
        out.push(b);
    }
    Ok(out)
}

// ============================================================================
// Handshake response decoder
// ============================================================================

#[derive(Debug)]
struct HandshakeResponse {
    capability_flags: CapabilityFlags,
    max_packet_size: u32,
    character_set: u8,
    username: String,
    auth_response: Vec<u8>,
    database: Option<String>,
    auth_plugin_name: Option<String>,
    connect_attrs: HashMap<String, String>,
}

impl HandshakeResponse {
    fn decode(mut payload: Bytes, server_caps: &CapabilityFlags) -> Result<Self> {
        if payload.remaining() < 4 {
            return Err(MySqlError::Protocol("handshake response too short".into()));
        }
        let client_flags = CapabilityFlags::new(payload.get_u32_le());
        let max_packet_size = payload.get_u32_le();
        let character_set = payload.get_u8();

        // 23 reserved bytes
        if payload.remaining() < 23 {
            return Err(MySqlError::Protocol("handshake response too short (reserved)".into()));
        }
        payload.advance(23);

        let username = read_null_terminated(&mut payload)?;

        let auth_response =
            if client_flags.has(CapabilityFlags::CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA) {
                read_lenenc_bytes(&mut payload)?
            } else if client_flags.has(CapabilityFlags::CLIENT_SECURE_CONNECTION) {
                let len = payload.get_u8() as usize;
                if payload.remaining() < len {
                    return Err(MySqlError::Protocol("auth response truncated".into()));
                }
                payload.copy_to_bytes(len).to_vec()
            } else {
                read_null_terminated_bytes(&mut payload)?
            };

        let database =
            if client_flags.has(CapabilityFlags::CLIENT_CONNECT_WITH_DB) && payload.has_remaining() {
                Some(read_null_terminated(&mut payload)?)
            } else {
                None
            };

        let auth_plugin_name =
            if client_flags.has(CapabilityFlags::CLIENT_PLUGIN_AUTH) && payload.has_remaining() {
                Some(read_null_terminated(&mut payload)?)
            } else {
                None
            };

        let mut connect_attrs = HashMap::new();
        if client_flags.has(CapabilityFlags::CLIENT_CONNECT_ATTRS) && payload.has_remaining() {
            let attrs_len = read_lenenc_int(&mut payload)? as usize;
            let mut attrs = payload.copy_to_bytes(attrs_len.min(payload.remaining()));
            while attrs.has_remaining() {
                let key = read_lenenc_str(&mut attrs)?;
                let val = read_lenenc_str(&mut attrs)?;
                connect_attrs.insert(key, val);
            }
        }

        Ok(Self {
            capability_flags: client_flags,
            max_packet_size,
            character_set,
            username,
            auth_response,
            database,
            auth_plugin_name,
            connect_attrs,
        })
    }
}

// ============================================================================
// Prepared statement cache
// ============================================================================

#[derive(Debug, Clone)]
struct PreparedStatement {
    id: u32,
    sql: String,
    num_params: u16,
}

// ============================================================================
// Case-insensitive prefix check (same helper as PG handler)
// ============================================================================

#[inline]
fn starts_with_icase(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len()
        && s.as_bytes()
            .get(..prefix.len())
            .map_or(false, |b| b.eq_ignore_ascii_case(prefix.as_bytes()))
}

// ============================================================================
// MySqlHandler — the main connection handler
// ============================================================================

/// Per-connection MySQL protocol handler.
///
/// All SQL is delegated to `EmbeddedDatabase`, mirroring how the PG handler
/// works.  The handler owns the TCP stream and sequence counter.
pub struct MySqlHandler {
    database: Arc<EmbeddedDatabase>,
    stream: TcpStream,
    seq: u8,
    connection_id: u32,
    capabilities: CapabilityFlags,
    status_flags: StatusFlags,
    character_set: u8,
    auth_seed: [u8; 20],
    auth_plugin: String,
    username: Option<String>,
    current_database: Option<String>,
    in_transaction: bool,
    prepared_statements: HashMap<u32, PreparedStatement>,
    next_stmt_id: u32,
    last_row_count: u64,
    /// Last auto-generated ID from INSERT (for `SELECT LAST_INSERT_ID()`)
    last_insert_id: u64,
}

impl MySqlHandler {
    // ------------------------------------------------------------------
    // Construction
    // ------------------------------------------------------------------

    fn new(database: Arc<EmbeddedDatabase>, stream: TcpStream, connection_id: u32) -> Self {
        let mut auth_seed = [0u8; 20];
        use rand::Rng;
        rand::thread_rng().fill(&mut auth_seed);

        Self {
            database,
            stream,
            seq: 0,
            connection_id,
            capabilities: CapabilityFlags::server_default(),
            status_flags: StatusFlags::default_flags(),
            character_set: UTF8MB4_GENERAL_CI,
            auth_seed,
            auth_plugin: "mysql_native_password".into(),
            username: None,
            current_database: None,
            in_transaction: false,
            prepared_statements: HashMap::new(),
            next_stmt_id: 1,
            last_row_count: 0,
            last_insert_id: 0,
        }
    }

    // ------------------------------------------------------------------
    // Sequence helpers
    // ------------------------------------------------------------------

    fn next_seq(&mut self) -> u8 {
        let s = self.seq;
        self.seq = self.seq.wrapping_add(1);
        s
    }

    fn reset_seq(&mut self) {
        self.seq = 0;
    }

    /// Write a MySQL packet, automatically consuming the next sequence id.
    ///
    /// This helper avoids borrow-checker issues that arise when calling
    /// `write_packet(&mut self.stream, self.next_seq(), ...)` — the two
    /// `&mut self` borrows would overlap.
    async fn write_pkt(&mut self, payload: &[u8]) -> Result<()> {
        let seq = self.next_seq();
        write_packet(&mut self.stream, seq, payload).await
    }

    // ------------------------------------------------------------------
    // Public entry point
    // ------------------------------------------------------------------

    /// Accept a MySQL client, perform handshake + auth, then enter the
    /// command loop.
    pub async fn handle_connection(
        database: Arc<EmbeddedDatabase>,
        stream: TcpStream,
        connection_id: u32,
    ) -> Result<()> {
        let mut handler = Self::new(database, stream, connection_id);
        info!("New MySQL connection: id={}", connection_id);

        // Handshake
        handler.send_handshake().await?;
        let hs = handler.receive_handshake_response().await?;

        // Authenticate (trust-based for Nano — accept any non-empty creds)
        handler.authenticate(&hs)?;
        handler.send_ok(0, 0).await?;

        // Command loop
        loop {
            handler.reset_seq();
            match handler.receive_command().await {
                Ok((cmd, payload)) => {
                    if let Err(e) = handler.dispatch_command(cmd, payload).await {
                        match e {
                            MySqlError::ConnectionClosed => {
                                info!("MySQL connection {} closed", connection_id);
                                break;
                            }
                            _ => {
                                error!("Command error: {}", e);
                                let msg = e.to_string();
                                let (code, state) = map_error_code(&msg);
                                let _ = handler
                                    .send_error(code, state, &msg)
                                    .await;
                            }
                        }
                    }
                }
                Err(MySqlError::ConnectionClosed) => {
                    info!("MySQL connection {} disconnected", connection_id);
                    break;
                }
                Err(e) => {
                    error!("Receive error: {}", e);
                    break;
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Handshake
    // ------------------------------------------------------------------

    async fn send_handshake(&mut self) -> Result<()> {
        let mut p = BytesMut::new();

        // Protocol version
        p.put_u8(PROTOCOL_VERSION);

        // Server version (null-terminated)
        p.put_slice(SERVER_VERSION.as_bytes());
        p.put_u8(0);

        // Connection ID
        p.put_u32_le(self.connection_id);

        // Auth-plugin-data part 1 (8 bytes)
        #[allow(clippy::indexing_slicing)]
        p.put_slice(&self.auth_seed[0..8]);

        // Filler
        p.put_u8(0);

        // Capability flags lower 2 bytes
        p.put_u16_le((self.capabilities.as_u32() & 0xFFFF) as u16);

        // Character set
        p.put_u8(self.character_set);

        // Status flags
        p.put_u16_le(self.status_flags.as_u16());

        // Capability flags upper 2 bytes
        p.put_u16_le(((self.capabilities.as_u32() >> 16) & 0xFFFF) as u16);

        // Auth-plugin data length (1 byte) — total seed len + 1
        p.put_u8(21);

        // Reserved (10 zero bytes)
        p.put_bytes(0, 10);

        // Auth-plugin-data part 2 (12 bytes + null)
        #[allow(clippy::indexing_slicing)]
        p.put_slice(&self.auth_seed[8..20]);
        p.put_u8(0);

        // Auth-plugin name (null-terminated)
        if self.capabilities.has(CapabilityFlags::CLIENT_PLUGIN_AUTH) {
            p.put_slice(self.auth_plugin.as_bytes());
            p.put_u8(0);
        }

        self.write_pkt(&p).await?;
        debug!("Sent HandshakeV10");
        Ok(())
    }

    async fn receive_handshake_response(&mut self) -> Result<HandshakeResponse> {
        let (seq, payload) = read_packet(&mut self.stream).await?;
        self.seq = seq.wrapping_add(1);
        HandshakeResponse::decode(payload, &self.capabilities)
    }

    /// Trust-based authentication: accept any user that provides a username.
    fn authenticate(&mut self, hs: &HandshakeResponse) -> Result<()> {
        self.username = Some(hs.username.clone());
        self.current_database = hs.database.clone();

        // Intersect capabilities
        self.capabilities = CapabilityFlags::new(
            self.capabilities.as_u32() & hs.capability_flags.as_u32(),
        );

        let plugin = hs
            .auth_plugin_name
            .as_deref()
            .unwrap_or("mysql_native_password");

        debug!(
            "Auth user='{}' plugin='{}' db={:?}",
            hs.username, plugin, hs.database
        );

        // Nano uses trust authentication — accept any connection.
        // A production deployment would verify credentials here.
        info!("User '{}' authenticated (trust)", hs.username);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Command receive / dispatch
    // ------------------------------------------------------------------

    async fn receive_command(&mut self) -> Result<(Command, Bytes)> {
        let (seq, mut payload) = read_packet(&mut self.stream).await?;
        self.seq = seq.wrapping_add(1);

        if payload.is_empty() {
            return Err(MySqlError::Protocol("empty command packet".into()));
        }

        let cmd_byte = payload.get_u8();
        let command = Command::from_u8(cmd_byte)
            .ok_or(MySqlError::Unsupported(cmd_byte))?;

        debug!("Received {:?}", command);
        Ok((command, payload))
    }

    async fn dispatch_command(&mut self, cmd: Command, payload: Bytes) -> Result<()> {
        match cmd {
            Command::ComQuit => {
                return Err(MySqlError::ConnectionClosed);
            }
            Command::ComPing => {
                self.send_ok(0, 0).await?;
            }
            Command::ComInitDb => {
                self.handle_init_db(payload).await?;
            }
            Command::ComQuery => {
                self.handle_com_query(payload).await?;
            }
            Command::ComStmtPrepare => {
                self.handle_stmt_prepare(payload).await?;
            }
            Command::ComStmtExecute => {
                self.handle_stmt_execute(payload).await?;
            }
            Command::ComStmtClose => {
                self.handle_stmt_close(payload);
            }
            Command::ComStmtReset => {
                self.send_ok(0, 0).await?;
            }
            Command::ComResetConnection => {
                self.status_flags = StatusFlags::default_flags();
                self.in_transaction = false;
                self.send_ok(0, 0).await?;
            }
            Command::ComStatistics => {
                // Return a simple statistics string (no packet framing
                // beyond the normal packet header).
                let stats = format!(
                    "Uptime: 0  Threads: 1  Questions: 0  Slow queries: 0  \
                     Opens: 0  Flush tables: 0  Open tables: 0  \
                     Queries per second avg: 0.000"
                );
                self.write_pkt(stats.as_bytes()).await?;
            }
            _ => {
                warn!("Unsupported MySQL command: {:?}", cmd);
                self.send_error(
                    1047,
                    "08S01",
                    &format!("Unsupported command: {:?}", cmd),
                )
                .await?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // COM_INIT_DB
    // ------------------------------------------------------------------

    async fn handle_init_db(&mut self, payload: Bytes) -> Result<()> {
        let db_name = String::from_utf8_lossy(&payload).to_string();
        debug!("COM_INIT_DB: {}", db_name);
        self.current_database = Some(db_name);
        self.send_ok(0, 0).await
    }

    // ------------------------------------------------------------------
    // COM_QUERY  —  the core handler
    // ------------------------------------------------------------------

    async fn handle_com_query(&mut self, payload: Bytes) -> Result<()> {
        let raw_sql = String::from_utf8_lossy(&payload).to_string();
        debug!("COM_QUERY: {}", raw_sql);

        // Apply MySQL-to-PostgreSQL SQL translation
        let translated = super::translator::translate(&raw_sql);
        let sql = translated.as_str();
        let trimmed = sql.trim();
        if trimmed.is_empty() {
            return self.send_ok(0, 0).await;
        }

        // ---- SET (session variables) — acknowledge silently ----
        if starts_with_icase(trimmed, "SET ") {
            return self.send_ok(0, 0).await;
        }

        // ---- SHOW commands ----
        if starts_with_icase(trimmed, "SHOW ") {
            return self.handle_show(trimmed).await;
        }

        // ---- DESCRIBE / DESC ----
        if starts_with_icase(trimmed, "DESCRIBE ") || starts_with_icase(trimmed, "DESC ") {
            return self.handle_describe(trimmed).await;
        }

        // ---- Transaction control ----
        if starts_with_icase(trimmed, "BEGIN")
            || starts_with_icase(trimmed, "START TRANSACTION")
        {
            return self.handle_begin().await;
        }
        if trimmed.eq_ignore_ascii_case("COMMIT") {
            return self.handle_commit().await;
        }
        if trimmed.eq_ignore_ascii_case("ROLLBACK") {
            return self.handle_rollback().await;
        }

        // ---- SELECT FOUND_ROWS() ----
        {
            let upper = trimmed.to_uppercase();
            if upper.contains("FOUND_ROWS()") {
                let cols = vec!["FOUND_ROWS()".to_string()];
                let rows = vec![Tuple::new(vec![Value::Int8(self.last_row_count as i64)])];
                return self.send_result_set(&cols, &rows).await;
            }
        }

        // ---- SELECT LAST_INSERT_ID() ----
        {
            let upper = trimmed.to_uppercase();
            if upper.contains("LAST_INSERT_ID()") {
                let cols = vec!["LAST_INSERT_ID()".to_string()];
                let rows = vec![Tuple::new(vec![Value::Int8(self.last_insert_id as i64)])];
                return self.send_result_set(&cols, &rows).await;
            }
        }

        // ---- SELECT VERSION() — return MySQL-compatible version ----
        {
            let upper = trimmed.to_uppercase();
            if upper.contains("VERSION()") && !upper.contains("@@") {
                let cols = vec!["VERSION()".to_string()];
                let rows = vec![Tuple::new(vec![Value::String(SERVER_VERSION.to_string())])];
                return self.send_result_set(&cols, &rows).await;
            }
        }

        // ---- SELECT @@variable — MySQL session/global variables ----
        if starts_with_icase(trimmed, "SELECT") && trimmed.contains("@@") {
            return self.handle_select_variable(trimmed).await;
        }

        // ---- USE database — SQL-level database switch ----
        if starts_with_icase(trimmed, "USE ") {
            return self.send_ok(0, 0).await;
        }

        // ---- INFORMATION_SCHEMA queries — intercept before SQL parser ----
        {
            let lower = trimmed.to_lowercase();
            if lower.contains("information_schema") {
                return self.handle_information_schema(trimmed).await;
            }
        }

        // ---- SELECT / DML / DDL — delegate to EmbeddedDatabase ----
        let is_select = starts_with_icase(trimmed, "SELECT")
            || starts_with_icase(trimmed, "WITH")
            || starts_with_icase(trimmed, "VALUES")
            || starts_with_icase(trimmed, "TABLE ");

        if is_select {
            self.execute_query(trimmed).await
        } else if raw_sql.to_uppercase().contains("ON DUPLICATE KEY UPDATE") {
            self.handle_upsert_dml(trimmed, &raw_sql).await
        } else {
            self.execute_dml(trimmed).await
        }
    }

    // ------------------------------------------------------------------
    // Transaction helpers
    // ------------------------------------------------------------------

    async fn handle_begin(&mut self) -> Result<()> {
        if !self.in_transaction {
            self.database.begin()?;
            self.in_transaction = true;
            self.status_flags.set(StatusFlags::SERVER_STATUS_IN_TRANS);
        }
        self.send_ok(0, 0).await
    }

    async fn handle_commit(&mut self) -> Result<()> {
        if self.in_transaction {
            self.database.commit()?;
            self.in_transaction = false;
            self.status_flags.clear(StatusFlags::SERVER_STATUS_IN_TRANS);
        }
        self.send_ok(0, 0).await
    }

    async fn handle_rollback(&mut self) -> Result<()> {
        if self.in_transaction {
            self.database.rollback()?;
            self.in_transaction = false;
            self.status_flags.clear(StatusFlags::SERVER_STATUS_IN_TRANS);
        }
        self.send_ok(0, 0).await
    }

    // ------------------------------------------------------------------
    // SELECT execution
    // ------------------------------------------------------------------

    async fn execute_query(&mut self, sql: &str) -> Result<()> {
        match self.database.query_with_columns(sql) {
            Ok((rows, columns)) => {
                self.last_row_count = rows.len() as u64;
                self.send_result_set(&columns, &rows).await
            }
            Err(e) => {
                let msg = e.to_string();
                let (code, state) = map_error_code(&msg);
                self.send_error(code, state, &msg).await
            }
        }
    }

    // ------------------------------------------------------------------
    // DML / DDL execution
    // ------------------------------------------------------------------

    async fn execute_dml(&mut self, sql: &str) -> Result<()> {
        // Track whether this is an INSERT to capture last_insert_id
        let is_insert = starts_with_icase(sql.trim(), "INSERT");
        let table_name = if is_insert {
            Self::extract_insert_table(sql)
        } else {
            None
        };

        match self.database.execute(sql) {
            Ok(affected) => {
                // After INSERT, try to capture the auto-generated ID
                let insert_id = if is_insert && affected > 0 {
                    if let Some(ref tbl) = table_name {
                        self.query_last_serial_id(tbl)
                    } else {
                        0
                    }
                } else {
                    0
                };
                if insert_id > 0 {
                    self.last_insert_id = insert_id;
                }
                self.send_ok(affected, insert_id).await
            }
            Err(e) => {
                let msg = e.to_string();
                let (code, state) = map_error_code(&msg);
                self.send_error(code, state, &msg).await
            }
        }
    }

    /// Handle INSERT ... ON DUPLICATE KEY UPDATE (MySQL upsert).
    ///
    /// The translator has already stripped the ON DUPLICATE KEY UPDATE clause,
    /// so `translated_sql` is a plain INSERT.  We try the INSERT first; if it
    /// fails with a duplicate-key error we build an UPDATE from the original
    /// MySQL SQL and execute that instead.
    async fn handle_upsert_dml(&mut self, translated_sql: &str, raw_sql: &str) -> Result<()> {
        // Try the plain INSERT first
        match self.database.execute(translated_sql) {
            Ok(affected) => {
                let table_name = Self::extract_insert_table(translated_sql);
                let insert_id = if affected > 0 {
                    if let Some(ref tbl) = table_name {
                        self.query_last_serial_id(tbl)
                    } else {
                        0
                    }
                } else {
                    0
                };
                if insert_id > 0 {
                    self.last_insert_id = insert_id;
                }
                self.send_ok(affected, insert_id).await
            }
            Err(e) => {
                let msg = e.to_string();
                // Check if this is a duplicate key error
                if msg.contains("duplicate key")
                    || msg.contains("UNIQUE constraint")
                    || msg.contains("PRIMARY KEY constraint")
                {
                    // Build an UPDATE from the ON DUPLICATE KEY UPDATE clause
                    if let Some(update_sql) = Self::build_upsert_update(raw_sql) {
                        let translated_update = super::translator::translate(&update_sql);
                        match self.database.execute(&translated_update) {
                            Ok(affected) => self.send_ok(affected, 0).await,
                            Err(ue) => {
                                let umsg = ue.to_string();
                                let (code, state) = map_error_code(&umsg);
                                self.send_error(code, state, &umsg).await
                            }
                        }
                    } else {
                        // Could not build UPDATE — report the original duplicate error
                        let (code, state) = map_error_code(&msg);
                        self.send_error(code, state, &msg).await
                    }
                } else {
                    let (code, state) = map_error_code(&msg);
                    self.send_error(code, state, &msg).await
                }
            }
        }
    }

    /// Build an UPDATE statement from a MySQL INSERT ... ON DUPLICATE KEY UPDATE.
    ///
    /// Given: `INSERT INTO t (a, b, c) VALUES (1, 'x', 3) ON DUPLICATE KEY UPDATE b = VALUES(b), c = VALUES(c)`
    /// Produce: `UPDATE t SET b = 'x', c = 3 WHERE a = 1`
    /// (assuming `a` is the primary key)
    fn build_upsert_update(raw_sql: &str) -> Option<String> {
        let upper = raw_sql.to_uppercase();
        let odk_pos = upper.find("ON DUPLICATE KEY UPDATE")?;

        // Extract the SET clause from ON DUPLICATE KEY UPDATE
        let set_part = raw_sql.get(odk_pos + 23..)?.trim();

        // Extract table name
        let table_name = Self::extract_insert_table(raw_sql)?;

        // Extract column list and values from the INSERT part
        let insert_part = &raw_sql[..odk_pos];
        let (columns, values) = Self::extract_insert_columns_values(insert_part)?;

        // Build a column -> value map for VALUES() references
        let mut col_val_map = std::collections::HashMap::new();
        for (i, col) in columns.iter().enumerate() {
            if let Some(val) = values.get(i) {
                col_val_map.insert(col.to_uppercase(), val.clone());
            }
        }

        // Parse and resolve the SET assignments
        let mut set_clauses = Vec::new();
        for assignment in set_part.split(',') {
            let parts: Vec<&str> = assignment.trim().splitn(2, '=').collect();
            if parts.len() != 2 {
                continue;
            }
            let col = parts[0].trim().trim_matches('`');
            let expr = parts[1].trim();
            let expr_upper = expr.to_uppercase();

            // Resolve VALUES(col_name) references
            if expr_upper.starts_with("VALUES(") || expr_upper.starts_with("VALUES (") {
                let inner = expr.trim_end_matches(')');
                let inner = inner.find('(').map(|p| &inner[p + 1..])?;
                let ref_col = inner.trim().trim_matches('`').to_uppercase();
                if let Some(val) = col_val_map.get(&ref_col) {
                    set_clauses.push(format!("{} = {}", col, val));
                }
            } else {
                set_clauses.push(format!("{} = {}", col, expr));
            }
        }

        if set_clauses.is_empty() {
            return None;
        }

        // Build WHERE clause from the first column (assumed to be PK)
        // This is a simplification — the first column in the INSERT is typically the PK
        // or UNIQUE key that caused the conflict
        let where_clause = if let (Some(pk_col), Some(pk_val)) = (columns.first(), values.first()) {
            format!("{} = {}", pk_col, pk_val)
        } else {
            return None;
        };

        Some(format!(
            "UPDATE {} SET {} WHERE {}",
            table_name,
            set_clauses.join(", "),
            where_clause
        ))
    }

    /// Extract column names and value literals from an INSERT statement.
    fn extract_insert_columns_values(insert_sql: &str) -> Option<(Vec<String>, Vec<String>)> {
        // Find column list
        let first_paren = insert_sql.find('(')?;
        let first_close = insert_sql.find(')')?;
        let col_str = insert_sql.get(first_paren + 1..first_close)?;
        let columns: Vec<String> = col_str
            .split(',')
            .map(|c| c.trim().trim_matches('`').to_string())
            .collect();

        // Find VALUES
        let upper = insert_sql.to_uppercase();
        let values_pos = upper.find("VALUES")?;
        let rest = insert_sql.get(values_pos + 6..)?.trim();
        let val_open = rest.find('(')?;
        // Find matching close paren (handle quoted strings)
        let inner = rest.get(val_open + 1..)?;
        let close_idx = Self::find_matching_close_paren(inner)?;
        let val_str = inner.get(..close_idx)?;

        // Split values respecting quoted strings
        let values = Self::split_sql_values(val_str);

        Some((columns, values))
    }

    /// Find matching close paren, respecting single-quoted strings.
    fn find_matching_close_paren(s: &str) -> Option<usize> {
        let mut depth = 0u32;
        let mut in_quote = false;
        for (i, ch) in s.char_indices() {
            if in_quote {
                if ch == '\'' {
                    in_quote = false;
                }
                continue;
            }
            match ch {
                '\'' => in_quote = true,
                '(' => depth += 1,
                ')' => {
                    if depth == 0 {
                        return Some(i);
                    }
                    depth -= 1;
                }
                _ => {}
            }
        }
        None
    }

    /// Split comma-separated SQL values, respecting single-quoted strings.
    fn split_sql_values(s: &str) -> Vec<String> {
        let mut result = Vec::new();
        let mut current = String::new();
        let mut in_quote = false;
        let mut depth = 0u32;

        for ch in s.chars() {
            if in_quote {
                current.push(ch);
                if ch == '\'' {
                    in_quote = false;
                }
                continue;
            }
            match ch {
                '\'' => {
                    in_quote = true;
                    current.push(ch);
                }
                '(' => {
                    depth += 1;
                    current.push(ch);
                }
                ')' => {
                    depth = depth.saturating_sub(1);
                    current.push(ch);
                }
                ',' if depth == 0 => {
                    result.push(current.trim().to_string());
                    current.clear();
                }
                _ => current.push(ch),
            }
        }
        if !current.trim().is_empty() {
            result.push(current.trim().to_string());
        }
        result
    }

    /// Extract the table name from an INSERT statement.
    fn extract_insert_table(sql: &str) -> Option<String> {
        static INSERT_TABLE_RE: OnceLock<Regex> = OnceLock::new();
        let re = INSERT_TABLE_RE.get_or_init(|| {
            Regex::new(r#"(?i)\bINSERT\s+INTO\s+[`"]*(\w+)[`"]*"#)
                .unwrap_or_else(|_| Regex::new("^$").expect("static regex"))
        });
        re.captures(sql).and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
    }

    /// Query the current max primary-key (SERIAL) value for a table.
    ///
    /// After an INSERT into a table with a SERIAL/BIGSERIAL column, the
    /// auto-generated sequence value is the maximum PK value. This matches
    /// MySQL's `LAST_INSERT_ID()` semantics for single-row inserts.
    fn query_last_serial_id(&self, table_name: &str) -> u64 {
        // Find the PK column name from the catalog
        let pk_col = match self.database.storage.catalog().get_table_schema(table_name) {
            Ok(schema) => {
                schema.columns.iter()
                    .find(|c| c.primary_key)
                    .map(|c| c.name.clone())
            }
            Err(_) => None,
        };

        let pk_col = match pk_col {
            Some(c) => c,
            None => return 0,
        };

        // Query MAX(pk_col) — no double-quotes (they cause case-sensitive mismatch)
        let query = format!("SELECT MAX({}) FROM {}", pk_col, table_name);
        match self.database.query_with_columns(&query) {
            Ok((rows, _)) => {
                let result = rows.first()
                    .and_then(|r| r.values.first())
                    .and_then(|v| match v {
                        Value::Int4(n) => Some(*n as u64),
                        Value::Int8(n) => Some(*n as u64),
                        Value::Int2(n) => Some(*n as u64),
                        _ => None,
                    })
                    .unwrap_or(0);
                tracing::debug!("query_last_serial_id({}): pk_col={}, result={}", table_name, pk_col, result);
                result
            }
            Err(e) => {
                tracing::debug!("query_last_serial_id({}) error: {}", table_name, e);
                0
            }
        }
    }

    // ------------------------------------------------------------------
    // SHOW command handling
    // ------------------------------------------------------------------

    async fn handle_show(&mut self, trimmed: &str) -> Result<()> {
        let upper = trimmed.to_uppercase();

        if upper.contains("DATABASES") || upper.contains("SCHEMAS") {
            return self.show_single_column("Database", &["heliosdb"]).await;
        }

        // TABLE STATUS must be checked before TABLES to avoid false match
        if upper.contains("TABLE STATUS") {
            return self.handle_show_table_status(trimmed).await;
        }

        if upper.contains("TABLES") {
            // Query actual tables from the catalog.
            let mut tables = self
                .database
                .storage
                .catalog()
                .list_tables()
                .unwrap_or_default();

            // SHOW TABLES LIKE 'pattern' — apply LIKE filter
            if let Some(like_pattern) = extract_like_pattern(trimmed) {
                tables.retain(|t| sql_like_match(t, &like_pattern));
            }

            let refs: Vec<&str> = tables.iter().map(String::as_str).collect();
            return self.show_single_column("Tables_in_heliosdb", &refs).await;
        }

        if upper.contains("INDEX") || upper.contains("INDEXES") || upper.contains("KEYS") {
            return self.handle_show_index(trimmed).await;
        }

        if upper.contains("COLUMNS") || upper.contains("FIELDS") {
            return self.handle_show_columns(trimmed).await;
        }

        if upper.contains("CREATE TABLE") {
            return self.handle_show_create_table(trimmed).await;
        }

        if upper.contains("VARIABLES") || upper.contains("SESSION STATUS")
            || upper.contains("GLOBAL STATUS")
        {
            return self.handle_show_variables(&upper).await;
        }

        if upper.contains("WARNINGS") {
            // Return empty set — no warnings queued.
            return self
                .show_three_columns("Level", "Code", "Message", &[])
                .await;
        }

        if upper.contains("COLLATION") {
            return self
                .show_single_column("Collation", &["utf8mb4_general_ci"])
                .await;
        }

        if upper.contains("ENGINES") {
            return self
                .show_single_column("Engine", &["HeliosDB"])
                .await;
        }

        // Fallback: empty OK
        self.send_ok(0, 0).await
    }

    async fn handle_show_columns(&mut self, sql: &str) -> Result<()> {
        let upper = sql.to_uppercase();
        let is_full = upper.contains("FULL");

        // Extract table name from "SHOW [FULL] COLUMNS FROM <table>"
        let table_name = upper
            .find("FROM ")
            .and_then(|pos| {
                let rest = sql.get(pos + 5..)?;
                let name = rest.trim().trim_end_matches(';').trim();
                let name = name.trim_matches('`').trim_matches('"');
                Some(name.to_string())
            });

        let table_name = match table_name {
            Some(t) => t,
            None => return self.send_ok(0, 0).await,
        };

        // Read schema from catalog directly for complete metadata
        let schema = match self.database.storage.catalog().get_table_schema(&table_name) {
            Ok(s) => s,
            Err(_) => {
                return self.send_error(1146, "42S02",
                    &format!("Table '{}' doesn't exist", table_name)).await;
            }
        };

        if is_full {
            // SHOW FULL COLUMNS: Field, Type, Collation, Null, Key, Default, Extra, Privileges, Comment
            let cols = vec![
                "Field".to_string(), "Type".to_string(), "Collation".to_string(),
                "Null".to_string(), "Key".to_string(), "Default".to_string(),
                "Extra".to_string(), "Privileges".to_string(), "Comment".to_string(),
            ];
            let rows: Vec<Tuple> = schema.columns.iter().map(|c| {
                let type_str = datatype_to_mysql(&c.data_type);
                let null_str = if c.nullable { "YES" } else { "NO" };
                let key_str = if c.primary_key { "PRI" } else if c.unique { "UNI" } else { "" };
                let default_str = c.default_expr.as_deref().unwrap_or("NULL");
                let extra = if c.primary_key && matches!(c.data_type, crate::DataType::Int4 | crate::DataType::Int8) {
                    "auto_increment"
                } else { "" };
                Tuple::new(vec![
                    Value::String(c.name.clone()),
                    Value::String(type_str),
                    Value::String("utf8mb4_unicode_ci".to_string()),
                    Value::String(null_str.to_string()),
                    Value::String(key_str.to_string()),
                    Value::String(default_str.to_string()),
                    Value::String(extra.to_string()),
                    Value::String("select,insert,update,references".to_string()),
                    Value::String(String::new()),
                ])
            }).collect();
            self.send_result_set(&cols, &rows).await
        } else {
            // SHOW COLUMNS: Field, Type, Null, Key, Default, Extra
            let cols = vec![
                "Field".to_string(), "Type".to_string(), "Null".to_string(),
                "Key".to_string(), "Default".to_string(), "Extra".to_string(),
            ];
            let rows: Vec<Tuple> = schema.columns.iter().map(|c| {
                let type_str = datatype_to_mysql(&c.data_type);
                let null_str = if c.nullable { "YES" } else { "NO" };
                let key_str = if c.primary_key { "PRI" } else if c.unique { "UNI" } else { "" };
                let default_str = c.default_expr.as_deref().unwrap_or("NULL");
                let extra = if c.primary_key && matches!(c.data_type, crate::DataType::Int4 | crate::DataType::Int8) {
                    "auto_increment"
                } else { "" };
                Tuple::new(vec![
                    Value::String(c.name.clone()),
                    Value::String(type_str),
                    Value::String(null_str.to_string()),
                    Value::String(key_str.to_string()),
                    Value::String(default_str.to_string()),
                    Value::String(extra.to_string()),
                ])
            }).collect();
            self.send_result_set(&cols, &rows).await
        }
    }

    async fn handle_show_create_table(&mut self, sql: &str) -> Result<()> {
        let table_name = sql
            .to_uppercase()
            .find("TABLE ")
            .and_then(|pos| {
                let after_kw = sql.get(pos + 6..)?;
                let name = after_kw.trim().trim_end_matches(';').trim();
                let name = name.trim_matches('`');
                Some(name.to_string())
            });

        let table_name = match table_name {
            Some(t) => t,
            None => return self.send_ok(0, 0).await,
        };

        let ddl = self.generate_create_table_ddl(&table_name);
        let cols = vec!["Table".to_string(), "Create Table".to_string()];
        let row = Tuple::new(vec![
            Value::String(table_name),
            Value::String(ddl),
        ]);
        self.send_result_set(&cols, &[row]).await
    }

    /// Generate MySQL-compatible CREATE TABLE DDL from the catalog schema.
    fn generate_create_table_ddl(&self, table_name: &str) -> String {
        let schema = match self.database.storage.catalog().get_table_schema(table_name) {
            Ok(s) => s,
            Err(_) => {
                return format!("CREATE TABLE `{}` (\n  /* schema not available */\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4", table_name);
            }
        };

        let mut col_defs = Vec::new();
        let mut pk_cols = Vec::new();

        for col in &schema.columns {
            let mysql_type = datatype_to_mysql(&col.data_type);
            let nullable = if col.nullable { "" } else { " NOT NULL" };
            let default = col.default_expr.as_ref().map_or(String::new(), |d| format!(" DEFAULT {}", d));
            col_defs.push(format!("  `{}` {}{}{}", col.name, mysql_type, nullable, default));
            if col.primary_key {
                pk_cols.push(format!("`{}`", col.name));
            }
        }

        if !pk_cols.is_empty() {
            col_defs.push(format!("  PRIMARY KEY ({})", pk_cols.join(",")));
        }

        // Add UNIQUE constraints
        for col in &schema.columns {
            if col.unique && !col.primary_key {
                col_defs.push(format!("  UNIQUE KEY `idx_{}_unique` (`{}`)", col.name, col.name));
            }
        }

        format!(
            "CREATE TABLE `{}` (\n{}\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
            table_name,
            col_defs.join(",\n")
        )
    }

    async fn handle_show_variables(&mut self, upper: &str) -> Result<()> {
        // Return a small set of common MySQL variables.
        let vars: Vec<(&str, &str)> = vec![
            ("character_set_client", "utf8mb4"),
            ("character_set_connection", "utf8mb4"),
            ("character_set_results", "utf8mb4"),
            ("character_set_server", "utf8mb4"),
            ("collation_connection", "utf8mb4_general_ci"),
            ("collation_server", "utf8mb4_general_ci"),
            ("version", SERVER_VERSION),
            ("version_comment", "HeliosDB Nano"),
            ("max_allowed_packet", "67108864"),
            ("system_time_zone", "UTC"),
            ("time_zone", "SYSTEM"),
            ("lower_case_table_names", "0"),
            ("sql_mode", "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION"),
            ("autocommit", "ON"),
            ("tx_isolation", "REPEATABLE-READ"),
            ("transaction_isolation", "REPEATABLE-READ"),
        ];

        // If there is a LIKE filter, apply it.
        let filter = if let Some(pos) = upper.find("LIKE ") {
            let rest = upper.get(pos + 5..).unwrap_or("").trim();
            let pattern = rest.trim_matches('\'').trim_matches('%');
            if pattern.is_empty() {
                None
            } else {
                Some(pattern.to_lowercase())
            }
        } else {
            None
        };

        let cols = vec!["Variable_name".to_string(), "Value".to_string()];
        let rows: Vec<Tuple> = vars
            .iter()
            .filter(|(name, _)| {
                if let Some(ref pat) = filter {
                    name.to_lowercase().contains(pat.as_str())
                } else {
                    true
                }
            })
            .map(|(name, val)| {
                Tuple::new(vec![
                    Value::String((*name).to_string()),
                    Value::String((*val).to_string()),
                ])
            })
            .collect();

        self.send_result_set(&cols, &rows).await
    }

    /// Handle SHOW INDEX FROM / SHOW INDEXES FROM / SHOW KEYS FROM.
    ///
    /// Returns MySQL-compatible index metadata columns. At minimum, returns the
    /// primary key if one exists.
    async fn handle_show_index(&mut self, sql: &str) -> Result<()> {
        let table_name = sql
            .to_uppercase()
            .find("FROM ")
            .and_then(|pos| {
                let rest = sql.get(pos + 5..)?;
                // Take only the first token (table name), stripping backticks,
                // semicolons, and ignoring any trailing FROM db / WHERE clause.
                let name = rest.trim();
                let name = name.split_once(|c: char| c.is_whitespace() || c == ';')
                    .map_or(name, |(first, _)| first);
                let name = name.trim_matches('`').trim_matches('"');
                if name.is_empty() { return None; }
                // Strip database qualifier (db.table -> table)
                let name = name.rsplit('.').next().unwrap_or(name);
                Some(name.to_string())
            });

        let table_name = match table_name {
            Some(t) => t,
            None => return self.send_ok(0, 0).await,
        };
        tracing::debug!("handle_show_index: resolved table_name = '{}'", table_name);

        let cols = vec![
            "Table".to_string(),
            "Non_unique".to_string(),
            "Key_name".to_string(),
            "Seq_in_index".to_string(),
            "Column_name".to_string(),
            "Collation".to_string(),
            "Cardinality".to_string(),
            "Sub_part".to_string(),
            "Packed".to_string(),
            "Null".to_string(),
            "Index_type".to_string(),
            "Comment".to_string(),
            "Index_comment".to_string(),
        ];

        let mut rows: Vec<Tuple> = Vec::new();

        // Try to read primary key info from catalog
        if let Ok(schema) = self.database.storage.catalog().get_table_schema(&table_name) {
            let mut seq = 1i64;
            for col in &schema.columns {
                if col.primary_key {
                    rows.push(Tuple::new(vec![
                        Value::String(table_name.clone()),       // Table
                        Value::String("0".to_string()),          // Non_unique (0 = unique)
                        Value::String("PRIMARY".to_string()),    // Key_name
                        Value::String(seq.to_string()),          // Seq_in_index
                        Value::String(col.name.clone()),         // Column_name
                        Value::String("A".to_string()),          // Collation
                        Value::String("0".to_string()),          // Cardinality
                        Value::Null,                             // Sub_part
                        Value::Null,                             // Packed
                        Value::String(String::new()),            // Null
                        Value::String("BTREE".to_string()),      // Index_type
                        Value::String(String::new()),            // Comment
                        Value::String(String::new()),            // Index_comment
                    ]));
                    seq += 1;
                }
            }

            // Also add UNIQUE indexes
            let mut unique_seq = 1i64;
            for col in &schema.columns {
                if col.unique && !col.primary_key {
                    rows.push(Tuple::new(vec![
                        Value::String(table_name.clone()),
                        Value::String("0".to_string()),
                        Value::String(format!("idx_{}_unique", col.name)),
                        Value::String(unique_seq.to_string()),
                        Value::String(col.name.clone()),
                        Value::String("A".to_string()),
                        Value::String("0".to_string()),
                        Value::Null,
                        Value::Null,
                        if col.nullable { Value::String("YES".to_string()) } else { Value::String(String::new()) },
                        Value::String("BTREE".to_string()),
                        Value::String(String::new()),
                        Value::String(String::new()),
                    ]));
                    unique_seq += 1;
                }
            }
        }

        self.send_result_set(&cols, &rows).await
    }

    // ------------------------------------------------------------------
    // SELECT @@variable — MySQL session/global variable queries
    // ------------------------------------------------------------------

    /// Handle `SELECT @@variable` queries from MySQL clients.
    ///
    /// MySQL clients (including PHP's `mysqli`) send these to probe server
    /// capabilities and configuration.  We return sensible defaults without
    /// hitting the SQL parser, which does not understand `@@` syntax.
    async fn handle_select_variable(&mut self, sql: &str) -> Result<()> {
        // Extract @@variable names from the query using a simple regex.
        // Handles both @@session.var and @@global.var and @@var forms.
        static VAR_RE: OnceLock<Regex> = OnceLock::new();
        let re = VAR_RE.get_or_init(|| {
            Regex::new(r"@@(?:session\.|global\.)?(\w+)")
                .unwrap_or_else(|_| Regex::new("^$").expect("static regex"))
        });

        let mut col_names: Vec<String> = Vec::new();
        let mut values: Vec<Value> = Vec::new();

        for cap in re.captures_iter(sql) {
            let full_match = cap.get(0).map_or("", |m| m.as_str());
            let var_name = cap.get(1).map_or("", |m| m.as_str()).to_lowercase();

            let val = match var_name.as_str() {
                "version" => Value::String(SERVER_VERSION.to_string()),
                "version_comment" => Value::String("HeliosDB Nano".to_string()),
                "max_allowed_packet" => Value::Int8(67_108_864),
                "character_set_client" | "character_set_connection"
                | "character_set_results" | "character_set_server"
                | "character_set_database" => Value::String("utf8mb4".to_string()),
                "collation_connection" | "collation_server"
                | "collation_database" => Value::String("utf8mb4_general_ci".to_string()),
                "auto_increment_increment" | "auto_increment_offset" => Value::Int8(1),
                "interactive_timeout" | "wait_timeout" => Value::Int8(28800),
                "net_write_timeout" | "net_read_timeout" => Value::Int8(30),
                "sql_mode" => Value::String(
                    "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION".to_string()
                ),
                "time_zone" | "system_time_zone" => Value::String("SYSTEM".to_string()),
                "tx_isolation" | "transaction_isolation" => Value::String("REPEATABLE-READ".to_string()),
                "autocommit" => Value::Int8(1),
                "have_ssl" | "have_openssl" => Value::String("YES".to_string()),
                "lower_case_table_names" => Value::Int8(0),
                "sql_auto_is_null" => Value::Int8(0),
                "last_insert_id" => Value::Int8(self.last_insert_id as i64),
                _ => Value::String(String::new()),
            };
            col_names.push(full_match.to_string());
            values.push(val);
        }

        if col_names.is_empty() {
            // Fallback: return empty result
            return self.send_ok(0, 0).await;
        }

        let row = Tuple::new(values);
        self.send_result_set(&col_names, &[row]).await
    }

    // ------------------------------------------------------------------
    // INFORMATION_SCHEMA queries
    // ------------------------------------------------------------------

    /// Handle queries against `information_schema.tables` and
    /// `information_schema.columns`.
    ///
    /// These are routed through the PG catalog handler which already
    /// supports both views.  If the catalog handler doesn't recognize
    /// the query, we fall back to the normal SQL path.
    async fn handle_information_schema(&mut self, sql: &str) -> Result<()> {
        use crate::protocol::postgres::catalog::PgCatalog;

        let catalog = PgCatalog::with_database(Arc::clone(&self.database));
        match catalog.handle_query(sql) {
            Ok(Some((schema, rows))) => {
                let col_names: Vec<String> = schema.columns.iter()
                    .map(|c| c.name.clone())
                    .collect();
                self.send_result_set(&col_names, &rows).await
            }
            Ok(None) => {
                // Catalog handler didn't recognize it — try normal SQL
                self.execute_query(sql).await
            }
            Err(e) => {
                // Catalog handler errored — try normal SQL as fallback
                debug!("information_schema catalog handler error: {}, falling back to SQL", e);
                self.execute_query(sql).await
            }
        }
    }

    // ------------------------------------------------------------------
    // SHOW TABLE STATUS
    // ------------------------------------------------------------------

    /// Handle `SHOW TABLE STATUS` — returns table metadata in MySQL format.
    async fn handle_show_table_status(&mut self, sql: &str) -> Result<()> {
        let tables = self.database.storage.catalog().list_tables().unwrap_or_default();

        let like_pattern = extract_like_pattern(sql);

        let cols = vec![
            "Name".to_string(),
            "Engine".to_string(),
            "Version".to_string(),
            "Row_format".to_string(),
            "Rows".to_string(),
            "Avg_row_length".to_string(),
            "Data_length".to_string(),
            "Max_data_length".to_string(),
            "Index_length".to_string(),
            "Data_free".to_string(),
            "Auto_increment".to_string(),
            "Create_time".to_string(),
            "Update_time".to_string(),
            "Check_time".to_string(),
            "Collation".to_string(),
            "Checksum".to_string(),
            "Create_options".to_string(),
            "Comment".to_string(),
        ];

        let mut rows: Vec<Tuple> = Vec::new();
        for table in &tables {
            // Apply LIKE filter if present
            if let Some(ref pat) = like_pattern {
                if !sql_like_match(table, pat) {
                    continue;
                }
            }

            rows.push(Tuple::new(vec![
                Value::String(table.clone()),                      // Name
                Value::String("InnoDB".to_string()),               // Engine
                Value::String("10".to_string()),                   // Version
                Value::String("Dynamic".to_string()),              // Row_format
                Value::Int8(0),                                    // Rows (estimate)
                Value::Int8(0),                                    // Avg_row_length
                Value::Int8(0),                                    // Data_length
                Value::Int8(0),                                    // Max_data_length
                Value::Int8(0),                                    // Index_length
                Value::Int8(0),                                    // Data_free
                Value::Null,                                       // Auto_increment
                Value::Null,                                       // Create_time
                Value::Null,                                       // Update_time
                Value::Null,                                       // Check_time
                Value::String("utf8mb4_general_ci".to_string()),   // Collation
                Value::Null,                                       // Checksum
                Value::String(String::new()),                      // Create_options
                Value::String(String::new()),                      // Comment
            ]));
        }

        self.send_result_set(&cols, &rows).await
    }

    /// Handle DESCRIBE / DESC table_name — equivalent to SHOW COLUMNS FROM.
    async fn handle_describe(&mut self, sql: &str) -> Result<()> {
        // Extract table name after DESCRIBE or DESC keyword
        let table_name = if starts_with_icase(sql, "DESCRIBE ") {
            sql.get(9..)
        } else {
            // DESC
            sql.get(5..)
        };

        let table_name = match table_name {
            Some(rest) => {
                let name = rest.trim().trim_end_matches(';').trim().trim_matches('`');
                if name.is_empty() {
                    return self.send_ok(0, 0).await;
                }
                name.to_string()
            }
            None => return self.send_ok(0, 0).await,
        };

        let cols = vec![
            "Field".to_string(),
            "Type".to_string(),
            "Null".to_string(),
            "Key".to_string(),
            "Default".to_string(),
            "Extra".to_string(),
        ];

        let mut rows: Vec<Tuple> = Vec::new();

        if let Ok(schema) = self.database.storage.catalog().get_table_schema(&table_name) {
            for col in &schema.columns {
                let mysql_type = datatype_to_mysql(&col.data_type);
                let null_str = if col.nullable { "YES" } else { "NO" };
                let key_str = if col.primary_key {
                    "PRI"
                } else if col.unique {
                    "UNI"
                } else {
                    ""
                };
                let default_val = col.default_expr.clone().unwrap_or_default();

                rows.push(Tuple::new(vec![
                    Value::String(col.name.clone()),
                    Value::String(mysql_type),
                    Value::String(null_str.to_string()),
                    Value::String(key_str.to_string()),
                    if default_val.is_empty() { Value::Null } else { Value::String(default_val) },
                    Value::String(String::new()),
                ]));
            }
        } else {
            let msg = format!("Table '{}' doesn't exist", table_name);
            return self.send_error(1146, "42S02", &msg).await;
        }

        self.send_result_set(&cols, &rows).await
    }

    /// Convenience: single-column result set.
    async fn show_single_column(&mut self, col_name: &str, values: &[&str]) -> Result<()> {
        let cols = vec![col_name.to_string()];
        let rows: Vec<Tuple> = values
            .iter()
            .map(|v| Tuple::new(vec![Value::String((*v).to_string())]))
            .collect();
        self.send_result_set(&cols, &rows).await
    }

    /// Convenience: three-column result set (e.g. SHOW WARNINGS).
    async fn show_three_columns(
        &mut self,
        c1: &str,
        c2: &str,
        c3: &str,
        rows: &[(String, String, String)],
    ) -> Result<()> {
        let cols = vec![c1.to_string(), c2.to_string(), c3.to_string()];
        let tuples: Vec<Tuple> = rows
            .iter()
            .map(|(a, b, c)| {
                Tuple::new(vec![
                    Value::String(a.clone()),
                    Value::String(b.clone()),
                    Value::String(c.clone()),
                ])
            })
            .collect();
        self.send_result_set(&cols, &tuples).await
    }

    // ------------------------------------------------------------------
    // COM_STMT_PREPARE / EXECUTE / CLOSE
    // ------------------------------------------------------------------

    async fn handle_stmt_prepare(&mut self, payload: Bytes) -> Result<()> {
        let raw_sql = String::from_utf8_lossy(&payload).to_string();
        debug!("COM_STMT_PREPARE: {}", raw_sql);
        let sql = super::translator::translate(&raw_sql);

        let stmt_id = self.next_stmt_id;
        self.next_stmt_id += 1;

        let num_params = sql.matches('?').count() as u16;

        self.prepared_statements.insert(
            stmt_id,
            PreparedStatement {
                id: stmt_id,
                sql,
                num_params,
            },
        );

        // COM_STMT_PREPARE_OK response
        let mut resp = BytesMut::new();
        resp.put_u8(0x00); // status OK
        resp.put_u32_le(stmt_id);
        resp.put_u16_le(0); // num_columns (determined at execute time)
        resp.put_u16_le(num_params);
        resp.put_u8(0x00); // filler
        resp.put_u16_le(0); // warning count
        self.write_pkt(&resp).await?;

        // Send param column definitions (all as VARCHAR for now)
        for i in 0..num_params {
            self.send_column_def(&format!("?{}", i), ColumnType::VarString)
                .await?;
        }

        if num_params > 0 && !self.capabilities.has(CapabilityFlags::CLIENT_DEPRECATE_EOF) {
            self.send_eof().await?;
        }

        Ok(())
    }

    async fn handle_stmt_execute(&mut self, mut payload: Bytes) -> Result<()> {
        if payload.remaining() < 9 {
            return Err(MySqlError::Protocol("COM_STMT_EXECUTE too short".into()));
        }

        let stmt_id = payload.get_u32_le();
        let _flags = payload.get_u8();
        let _iteration_count = payload.get_u32_le();

        let stmt = self
            .prepared_statements
            .get(&stmt_id)
            .ok_or(MySqlError::StatementNotFound(stmt_id))?
            .clone();

        debug!("COM_STMT_EXECUTE: id={} sql={}", stmt_id, stmt.sql);

        // For a full implementation we would parse the null-bitmap and
        // parameter values here.  For now, route through COM_QUERY logic.
        let sql_bytes = Bytes::from(stmt.sql.clone());
        self.handle_com_query(sql_bytes).await
    }

    fn handle_stmt_close(&mut self, mut payload: Bytes) {
        if payload.remaining() >= 4 {
            let stmt_id = payload.get_u32_le();
            self.prepared_statements.remove(&stmt_id);
            debug!("COM_STMT_CLOSE: id={}", stmt_id);
        }
        // No response for COM_STMT_CLOSE
    }

    // ------------------------------------------------------------------
    // Result set encoding
    // ------------------------------------------------------------------

    /// Encode and send a full result set (column defs + rows + EOF).
    async fn send_result_set(
        &mut self,
        columns: &[String],
        rows: &[Tuple],
    ) -> Result<()> {
        let ncols = columns.len();

        // 1. Column count
        {
            let mut buf = BytesMut::new();
            write_lenenc_int(&mut buf, ncols as u64);
            self.write_pkt(&buf).await?;
        }

        // 2. Column definitions
        // Try to infer types from first row; default to VarString.
        for (i, col_name) in columns.iter().enumerate() {
            let col_type = rows
                .first()
                .and_then(|r| r.values.get(i))
                .map(ColumnType::from_value)
                .unwrap_or(ColumnType::VarString);
            self.send_column_def(col_name, col_type).await?;
        }

        // 3. EOF after column defs (unless CLIENT_DEPRECATE_EOF)
        if !self.capabilities.has(CapabilityFlags::CLIENT_DEPRECATE_EOF) {
            self.send_eof().await?;
        }

        // 4. Row data (text protocol — length-encoded strings)
        for row in rows {
            self.send_text_result_row(row).await?;
        }

        // 5. Closing EOF / OK
        if self.capabilities.has(CapabilityFlags::CLIENT_DEPRECATE_EOF) {
            self.send_ok(0, 0).await
        } else {
            self.send_eof().await
        }
    }

    /// Send a single column definition packet.
    async fn send_column_def(&mut self, name: &str, col_type: ColumnType) -> Result<()> {
        let mut p = BytesMut::new();

        write_lenenc_str(&mut p, "def");       // catalog
        write_lenenc_str(&mut p, "");           // schema
        write_lenenc_str(&mut p, "");           // virtual table
        write_lenenc_str(&mut p, "");           // physical table
        write_lenenc_str(&mut p, name);         // virtual column name
        write_lenenc_str(&mut p, name);         // physical column name

        // Fixed-length fields (0x0c = 12 bytes follow)
        write_lenenc_int(&mut p, 0x0c);
        p.put_u16_le(u16::from(UTF8MB4_GENERAL_CI)); // charset
        p.put_u32_le(255);                            // column length
        p.put_u8(col_type as u8);                     // type
        p.put_u16_le(0);                              // flags
        p.put_u8(0);                                  // decimals
        p.put_u16_le(0);                              // filler

        self.write_pkt(&p).await
    }

    /// Encode one row of text-protocol data.
    async fn send_text_result_row(&mut self, row: &Tuple) -> Result<()> {
        let mut p = BytesMut::new();
        for val in &row.values {
            match val {
                Value::Null => {
                    p.put_u8(0xFB); // NULL marker
                }
                _ => {
                    let s = value_to_mysql_string(val);
                    write_lenenc_str(&mut p, &s);
                }
            }
        }
        self.write_pkt(&p).await
    }

    // ------------------------------------------------------------------
    // OK / ERR / EOF packets
    // ------------------------------------------------------------------

    async fn send_ok(&mut self, affected_rows: u64, last_insert_id: u64) -> Result<()> {
        let mut p = BytesMut::new();
        p.put_u8(0x00); // OK header
        write_lenenc_int(&mut p, affected_rows);
        write_lenenc_int(&mut p, last_insert_id);

        if self.capabilities.has(CapabilityFlags::CLIENT_PROTOCOL_41) {
            p.put_u16_le(self.status_flags.as_u16());
            p.put_u16_le(0); // warnings
        }

        self.write_pkt(&p).await
    }

    async fn send_error(&mut self, code: u16, state: &str, msg: &str) -> Result<()> {
        let mut p = BytesMut::new();
        p.put_u8(0xFF); // ERR header
        p.put_u16_le(code);

        if self.capabilities.has(CapabilityFlags::CLIENT_PROTOCOL_41) {
            p.put_u8(b'#');
            // SQL state is always 5 bytes — pad or truncate.
            let state_bytes = state.as_bytes();
            #[allow(clippy::indexing_slicing)]
            for i in 0..5 {
                p.put_u8(if i < state_bytes.len() {
                    state_bytes[i]
                } else {
                    b' '
                });
            }
        }

        p.put_slice(msg.as_bytes());
        self.write_pkt(&p).await
    }

    async fn send_eof(&mut self) -> Result<()> {
        let mut p = BytesMut::new();
        p.put_u8(0xFE); // EOF header

        if self.capabilities.has(CapabilityFlags::CLIENT_PROTOCOL_41) {
            p.put_u16_le(0); // warnings
            p.put_u16_le(self.status_flags.as_u16());
        }

        self.write_pkt(&p).await
    }
}

// ============================================================================
// Value → MySQL text-protocol string
// ============================================================================

/// Convert a Nano `Value` to its MySQL text representation.
///
/// MySQL text protocol sends everything as length-encoded strings (except
/// NULL which uses the 0xFB sentinel).  This is analogous to the PG
/// handler's `send_data_row_direct`.
fn value_to_mysql_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(), // Should not be called for NULL (handled above)
        Value::Boolean(b) => if *b { "1" } else { "0" }.to_string(),
        Value::Int2(i) => i.to_string(),
        Value::Int4(i) => i.to_string(),
        Value::Int8(i) => i.to_string(),
        Value::Float4(f) => f.to_string(),
        Value::Float8(f) => f.to_string(),
        Value::Numeric(n) => n.clone(),
        Value::String(s) => s.clone(),
        Value::Bytes(b) => format!("0x{}", hex::encode(b)),
        Value::Uuid(u) => u.to_string(),
        Value::Timestamp(ts) => ts.format("%Y-%m-%d %H:%M:%S").to_string(),
        Value::Date(d) => d.format("%Y-%m-%d").to_string(),
        Value::Time(t) => t.format("%H:%M:%S").to_string(),
        Value::Interval(micros) => {
            let total_secs = micros / 1_000_000;
            let days = total_secs / 86400;
            let hours = (total_secs % 86400) / 3600;
            let mins = (total_secs % 3600) / 60;
            let secs = total_secs % 60;
            if days > 0 {
                format!("{} days {:02}:{:02}:{:02}", days, hours, mins, secs)
            } else {
                format!("{:02}:{:02}:{:02}", hours, mins, secs)
            }
        }
        Value::Json(j) => j.clone(),
        Value::Array(arr) => {
            let inner: Vec<String> = arr.iter().map(value_to_mysql_string).collect();
            format!("[{}]", inner.join(","))
        }
        Value::Vector(vec) => {
            let inner: Vec<String> = vec.iter().map(|f| f.to_string()).collect();
            format!("[{}]", inner.join(","))
        }
        Value::DictRef { dict_id } => dict_id.to_string(),
        Value::CasRef { hash } => hex::encode(hash),
        Value::ColumnarRef => "<columnar>".to_string(),
    }
}

// ============================================================================
// MySQL error code mapping
// ============================================================================

/// Map an error message to the appropriate MySQL error code and SQL state.
fn map_error_code(err_msg: &str) -> (u16, &'static str) {
    let lower = err_msg.to_lowercase();
    if lower.contains("duplicate") || lower.contains("unique") || lower.contains("already exists") {
        (1062, "23000") // ER_DUP_ENTRY
    } else if lower.contains("does not exist") || lower.contains("not found") || lower.contains("doesn't exist") {
        (1146, "42S02") // ER_NO_SUCH_TABLE
    } else if lower.contains("unknown column") || (lower.contains("column") && lower.contains("not found")) {
        (1054, "42S22") // ER_BAD_FIELD_ERROR
    } else if lower.contains("syntax") || lower.contains("parse") {
        (1064, "42000") // ER_PARSE_ERROR
    } else if lower.contains("access denied") {
        (1045, "28000") // ER_ACCESS_DENIED
    } else if lower.contains("foreign key") || lower.contains("constraint") {
        (1452, "23000") // ER_NO_REFERENCED_ROW_2
    } else if lower.contains("null") && lower.contains("not null") {
        (1048, "23000") // ER_BAD_NULL_ERROR
    } else {
        (1105, "HY000") // ER_UNKNOWN_ERROR
    }
}

// ============================================================================
// SQL LIKE pattern matching
// ============================================================================

/// Match a value against a SQL LIKE pattern.
///
/// Supports `%` (any sequence of characters) and `_` (any single character).
fn sql_like_match(value: &str, pattern: &str) -> bool {
    // Build regex by processing the pattern character-by-character:
    // - `%` becomes `.*` (match any sequence)
    // - `_` becomes `.`  (match any single char)
    // - all other characters are regex-escaped
    let mut regex_str = String::from("^");
    for ch in pattern.chars() {
        match ch {
            '%' => regex_str.push_str(".*"),
            '_' => regex_str.push('.'),
            _ => {
                // Escape regex-special characters
                let escaped = regex::escape(&ch.to_string());
                regex_str.push_str(&escaped);
            }
        }
    }
    regex_str.push('$');
    Regex::new(&regex_str)
        .map(|re| re.is_match(value))
        .unwrap_or(false)
}

/// Extract the LIKE pattern from a SQL statement (e.g. `SHOW TABLES LIKE 'wp_%'`).
fn extract_like_pattern(sql: &str) -> Option<String> {
    let upper = sql.to_uppercase();
    let pos = upper.find("LIKE ")?;
    let rest = sql.get(pos + 5..)?.trim();
    // Pattern is typically quoted with single quotes
    if rest.starts_with('\'') {
        let end = rest.get(1..)?.find('\'')?;
        rest.get(1..end + 1).map(String::from)
    } else {
        // Unquoted — take until whitespace or semicolon
        let end = rest.find(|c: char| c.is_whitespace() || c == ';').unwrap_or(rest.len());
        rest.get(..end).map(String::from)
    }
}

// ============================================================================
// DataType → MySQL type string
// ============================================================================

/// Convert a Nano `DataType` to MySQL-compatible type string for DDL output.
fn datatype_to_mysql(dt: &crate::DataType) -> String {
    match dt {
        crate::DataType::Boolean => "tinyint(1)".to_string(),
        crate::DataType::Int2 => "smallint".to_string(),
        crate::DataType::Int4 => "int".to_string(),
        crate::DataType::Int8 => "bigint".to_string(),
        crate::DataType::Float4 => "float".to_string(),
        crate::DataType::Float8 => "double".to_string(),
        crate::DataType::Numeric => "decimal(65,30)".to_string(),
        crate::DataType::Varchar(Some(n)) => format!("varchar({})", n),
        crate::DataType::Varchar(None) => "varchar(255)".to_string(),
        crate::DataType::Text => "longtext".to_string(),
        crate::DataType::Char(n) => format!("char({})", n),
        crate::DataType::Bytea => "longblob".to_string(),
        crate::DataType::Date => "date".to_string(),
        crate::DataType::Time => "time".to_string(),
        crate::DataType::Timestamp | crate::DataType::Timestamptz => "datetime".to_string(),
        crate::DataType::Interval => "varchar(64)".to_string(),
        crate::DataType::Uuid => "char(36)".to_string(),
        crate::DataType::Json | crate::DataType::Jsonb => "json".to_string(),
        crate::DataType::Array(_) => "json".to_string(),
        _ => "varchar(255)".to_string(),
    }
}

// ============================================================================
// Authentication helpers (kept for future use / client-side testing)
// ============================================================================

/// Compute `caching_sha2_password` auth response.
///
/// `SHA256(password) XOR SHA256(SHA256(SHA256(password)) + nonce)`
pub fn compute_caching_sha2_auth(password: &str, nonce: &[u8]) -> Vec<u8> {
    let stage1 = Sha256::digest(password.as_bytes());
    let stage2 = Sha256::digest(stage1);
    let mut h = Sha256::new();
    h.update(stage2);
    h.update(nonce);
    let stage3 = h.finalize();
    stage1
        .iter()
        .zip(stage3.iter())
        .map(|(a, b)| a ^ b)
        .collect()
}

/// Compute `mysql_native_password` auth response (SHA256 approximation).
///
/// Note: the real `mysql_native_password` uses SHA1.  We use SHA256 here
/// because Nano does not depend on the `sha1` crate.  This is sufficient for
/// trust-mode authentication; a full credential-checking implementation
/// should add `sha1` to `Cargo.toml`.
pub fn compute_native_auth(password: &str, nonce: &[u8]) -> Vec<u8> {
    // SHA256-based stand-in (same XOR structure, different hash)
    let stage1 = Sha256::digest(password.as_bytes());
    let stage2 = Sha256::digest(stage1);
    let mut h = Sha256::new();
    h.update(stage2);
    h.update(nonce);
    let stage3 = h.finalize();
    stage1
        .iter()
        .zip(stage3.iter())
        .map(|(a, b)| a ^ b)
        .collect()
}

// ============================================================================
// Public convenience API
// ============================================================================

/// Accept and fully handle one MySQL client connection.
///
/// This is the top-level entry point — spawn one task per accepted socket.
///
/// ```rust,no_run
/// use heliosdb_lite::protocol::mysql::handler;
/// use heliosdb_lite::EmbeddedDatabase;
/// use std::sync::Arc;
/// use tokio::net::TcpListener;
///
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
/// let listener = TcpListener::bind("127.0.0.1:3306").await?;
/// let mut conn_id = 0u32;
/// loop {
///     let (stream, _) = listener.accept().await?;
///     let db = db.clone();
///     conn_id += 1;
///     let id = conn_id;
///     tokio::spawn(async move {
///         if let Err(e) = handler::handle_mysql_connection(db, stream, id).await {
///             eprintln!("MySQL error: {}", e);
///         }
///     });
/// }
/// # }
/// ```
pub async fn handle_mysql_connection(
    database: Arc<EmbeddedDatabase>,
    stream: TcpStream,
    connection_id: u32,
) -> Result<()> {
    MySqlHandler::handle_connection(database, stream, connection_id).await
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capability_flags_default() {
        let caps = CapabilityFlags::server_default();
        assert!(caps.has(CapabilityFlags::CLIENT_PROTOCOL_41));
        assert!(caps.has(CapabilityFlags::CLIENT_SECURE_CONNECTION));
        assert!(!caps.has(CapabilityFlags::CLIENT_SSL));
    }

    #[test]
    fn test_capability_flags_set() {
        let mut caps = CapabilityFlags::server_default();
        caps.set(CapabilityFlags::CLIENT_SSL);
        assert!(caps.has(CapabilityFlags::CLIENT_SSL));
    }

    #[test]
    fn test_lenenc_int_roundtrip_small() {
        let mut buf = BytesMut::new();
        write_lenenc_int(&mut buf, 42);
        let mut read = buf.freeze();
        assert_eq!(read_lenenc_int(&mut read).expect("read"), 42);
    }

    #[test]
    fn test_lenenc_int_roundtrip_medium() {
        let mut buf = BytesMut::new();
        write_lenenc_int(&mut buf, 1000);
        let mut read = buf.freeze();
        assert_eq!(read_lenenc_int(&mut read).expect("read"), 1000);
    }

    #[test]
    fn test_lenenc_int_roundtrip_large() {
        let mut buf = BytesMut::new();
        write_lenenc_int(&mut buf, 100_000);
        let mut read = buf.freeze();
        assert_eq!(read_lenenc_int(&mut read).expect("read"), 100_000);
    }

    #[test]
    fn test_lenenc_int_roundtrip_u64() {
        let mut buf = BytesMut::new();
        write_lenenc_int(&mut buf, u64::MAX);
        let mut read = buf.freeze();
        assert_eq!(read_lenenc_int(&mut read).expect("read"), u64::MAX);
    }

    #[test]
    fn test_lenenc_string_roundtrip() {
        let mut buf = BytesMut::new();
        write_lenenc_str(&mut buf, "hello");
        let mut read = buf.freeze();
        assert_eq!(read_lenenc_str(&mut read).expect("read"), "hello");
    }

    #[test]
    fn test_value_to_mysql_string() {
        assert_eq!(value_to_mysql_string(&Value::Boolean(true)), "1");
        assert_eq!(value_to_mysql_string(&Value::Boolean(false)), "0");
        assert_eq!(value_to_mysql_string(&Value::Int4(42)), "42");
        assert_eq!(
            value_to_mysql_string(&Value::String("abc".into())),
            "abc"
        );
    }

    #[test]
    fn test_status_flags_clear() {
        let mut sf = StatusFlags::default_flags();
        sf.set(StatusFlags::SERVER_STATUS_IN_TRANS);
        assert!(sf.has(StatusFlags::SERVER_STATUS_IN_TRANS));
        sf.clear(StatusFlags::SERVER_STATUS_IN_TRANS);
        assert!(!sf.has(StatusFlags::SERVER_STATUS_IN_TRANS));
    }

    #[test]
    fn test_command_from_u8() {
        assert_eq!(Command::from_u8(0x03), Some(Command::ComQuery));
        assert_eq!(Command::from_u8(0x01), Some(Command::ComQuit));
        assert_eq!(Command::from_u8(0xFF), None);
    }

    #[test]
    fn test_starts_with_icase() {
        assert!(starts_with_icase("SELECT * FROM t", "SELECT"));
        assert!(starts_with_icase("select * FROM t", "SELECT"));
        assert!(!starts_with_icase("INS", "INSERT"));
    }

    #[test]
    fn test_map_error_code_duplicate() {
        let (code, state) = map_error_code("duplicate key value violates unique constraint");
        assert_eq!(code, 1062);
        assert_eq!(state, "23000");
    }

    #[test]
    fn test_map_error_code_not_found() {
        let (code, state) = map_error_code("Table 'users' does not exist");
        assert_eq!(code, 1146);
        assert_eq!(state, "42S02");
    }

    #[test]
    fn test_map_error_code_bad_field() {
        let (code, state) = map_error_code("unknown column 'foo'");
        assert_eq!(code, 1054);
        assert_eq!(state, "42S22");
    }

    #[test]
    fn test_map_error_code_syntax() {
        let (code, state) = map_error_code("syntax error at or near 'WHERE'");
        assert_eq!(code, 1064);
        assert_eq!(state, "42000");
    }

    #[test]
    fn test_map_error_code_unknown() {
        let (code, state) = map_error_code("something went wrong");
        assert_eq!(code, 1105);
        assert_eq!(state, "HY000");
    }

    #[test]
    fn test_sql_like_match_percent_wildcard() {
        assert!(sql_like_match("wp_users", "wp_%"));
        assert!(sql_like_match("wp_posts", "wp_%"));
        assert!(!sql_like_match("users", "wp_%"));
    }

    #[test]
    fn test_sql_like_match_underscore_wildcard() {
        assert!(sql_like_match("ab", "a_"));
        assert!(!sql_like_match("abc", "a_"));
    }

    #[test]
    fn test_sql_like_match_exact() {
        assert!(sql_like_match("users", "users"));
        assert!(!sql_like_match("users", "posts"));
    }

    #[test]
    fn test_sql_like_match_both_wildcards() {
        assert!(sql_like_match("wp_options", "%options"));
        assert!(sql_like_match("my_options", "%options"));
        assert!(!sql_like_match("my_posts", "%options"));
    }

    #[test]
    fn test_extract_like_pattern_quoted() {
        let pat = extract_like_pattern("SHOW TABLES LIKE 'wp_%'");
        assert_eq!(pat, Some("wp_%".to_string()));
    }

    #[test]
    fn test_extract_like_pattern_none() {
        let pat = extract_like_pattern("SHOW TABLES");
        assert_eq!(pat, None);
    }

    #[test]
    fn test_extract_like_pattern_unquoted() {
        let pat = extract_like_pattern("SHOW TABLES LIKE wp_%");
        assert_eq!(pat, Some("wp_%".to_string()));
    }

    #[test]
    fn test_datatype_to_mysql_coverage() {
        assert_eq!(datatype_to_mysql(&crate::DataType::Boolean), "tinyint(1)");
        assert_eq!(datatype_to_mysql(&crate::DataType::Int4), "int");
        assert_eq!(datatype_to_mysql(&crate::DataType::Int8), "bigint");
        assert_eq!(datatype_to_mysql(&crate::DataType::Text), "longtext");
        assert_eq!(datatype_to_mysql(&crate::DataType::Varchar(Some(100))), "varchar(100)");
        assert_eq!(datatype_to_mysql(&crate::DataType::Varchar(None)), "varchar(255)");
        assert_eq!(datatype_to_mysql(&crate::DataType::Json), "json");
        assert_eq!(datatype_to_mysql(&crate::DataType::Uuid), "char(36)");
        assert_eq!(datatype_to_mysql(&crate::DataType::Bytea), "longblob");
        assert_eq!(datatype_to_mysql(&crate::DataType::Timestamp), "datetime");
    }
}
