//! A small MySQL client for the wire tests: the handshake per plugin and
//! transport, `COM_QUERY` with every answer of a multi-statement batch, and
//! the commands that change the session's identity.
//!
//! It speaks the public protocol only. Packets are read and written one at a
//! time so a test can assert each step of an exchange.

#![allow(dead_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::many_single_char_names)]

use heliosdb_nano::protocol::mysql::{MySqlHandler, MysqlAuth};
use heliosdb_nano::protocol::postgres::timeouts::ConnectionTimeouts;
use heliosdb_nano::protocol::postgres::{AuthManager, AuthMethod, InMemoryPasswordStore, SharedPasswordStore};
use heliosdb_nano::EmbeddedDatabase;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};

pub const COM_QUERY: u8 = 0x03;
pub const COM_PING: u8 = 0x0e;
pub const COM_CHANGE_USER: u8 = 0x11;
pub const COM_RESET_CONNECTION: u8 = 0x1f;

pub const CLIENT_CONNECT_WITH_DB: u32 = 0x0000_0008;
pub const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
pub const CLIENT_SSL: u32 = 0x0000_0800;
pub const CLIENT_TRANSACTIONS: u32 = 0x0000_2000;
pub const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
pub const CLIENT_MULTI_STATEMENTS: u32 = 0x0001_0000;
pub const CLIENT_MULTI_RESULTS: u32 = 0x0002_0000;
pub const CLIENT_PLUGIN_AUTH: u32 = 0x0008_0000;

/// What a current client asks for, without multi-statements.
pub const CAPS: u32 =
    CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_TRANSACTIONS | CLIENT_PLUGIN_AUTH | CLIENT_MULTI_RESULTS;
/// [`CAPS`] with `CLIENT_MULTI_STATEMENTS`.
pub const CAPS_MULTI: u32 = CAPS | CLIENT_MULTI_STATEMENTS;

pub const SERVER_STATUS_IN_TRANS: u16 = 0x0001;
pub const SERVER_STATUS_AUTOCOMMIT: u16 = 0x0002;
pub const SERVER_MORE_RESULTS_EXISTS: u16 = 0x0008;

pub const CACHING_SHA2: &str = "caching_sha2_password";
pub const NATIVE: &str = "mysql_native_password";
pub const CLEAR: &str = "mysql_clear_password";

pub fn read_lenenc_u64(buf: &[u8], pos: &mut usize) -> u64 {
    let first = buf[*pos];
    *pos += 1;
    match first {
        0xFC => {
            let v = u64::from(u16::from_le_bytes([buf[*pos], buf[*pos + 1]]));
            *pos += 2;
            v
        }
        0xFD => {
            let v = u64::from(buf[*pos]) | (u64::from(buf[*pos + 1]) << 8) | (u64::from(buf[*pos + 2]) << 16);
            *pos += 3;
            v
        }
        0xFE => {
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[*pos..*pos + 8]);
            *pos += 8;
            u64::from_le_bytes(b)
        }
        v => u64::from(v),
    }
}

pub fn read_lenenc_str(buf: &[u8], pos: &mut usize) -> String {
    let len = read_lenenc_u64(buf, pos) as usize;
    let s = String::from_utf8_lossy(&buf[*pos..*pos + len]).into_owned();
    *pos += len;
    s
}

fn read_nul_str(buf: &[u8], pos: &mut usize) -> String {
    let end = buf[*pos..].iter().position(|b| *b == 0).map_or(buf.len(), |i| *pos + i);
    let s = String::from_utf8_lossy(&buf[*pos..end]).into_owned();
    *pos = (end + 1).min(buf.len());
    s
}

/// The server's first packet.
#[derive(Debug, Clone)]
pub struct Greeting {
    pub raw: Vec<u8>,
    pub server_version: String,
    pub connection_id: u32,
    pub nonce: Vec<u8>,
    pub capabilities: u32,
    pub status: u16,
    pub plugin: String,
}

pub fn parse_greeting(raw: &[u8]) -> Greeting {
    assert_eq!(raw[0], 10, "protocol version 10");
    let mut pos = 1usize;
    let server_version = read_nul_str(raw, &mut pos);
    let connection_id = u32::from_le_bytes([raw[pos], raw[pos + 1], raw[pos + 2], raw[pos + 3]]);
    pos += 4;
    let mut nonce = raw[pos..pos + 8].to_vec();
    pos += 9; // 8 bytes and the filler
    let low = u32::from(u16::from_le_bytes([raw[pos], raw[pos + 1]]));
    pos += 3; // capabilities (low) and the character set
    let status = u16::from_le_bytes([raw[pos], raw[pos + 1]]);
    pos += 2;
    let high = u32::from(u16::from_le_bytes([raw[pos], raw[pos + 1]]));
    pos += 2;
    let auth_len = raw[pos] as usize;
    pos += 11; // the length and 10 reserved bytes
    let part2 = auth_len.saturating_sub(8).max(13);
    // The scramble is the first 20 bytes; the 13th byte of part 2 is its terminator.
    nonce.extend_from_slice(&raw[pos..pos + part2 - 1]);
    pos += part2;
    let plugin = read_nul_str(raw, &mut pos);
    Greeting {
        raw: raw.to_vec(),
        server_version,
        connection_id,
        nonce,
        capabilities: low | (high << 16),
        status,
        plugin,
    }
}

/// The `caching_sha2_password` scramble a client answers a nonce with:
/// `SHA256(password) XOR SHA256(SHA256(SHA256(password)) || nonce)`.
pub fn sha2_scramble(password: &str, nonce: &[u8]) -> Vec<u8> {
    if password.is_empty() {
        return Vec::new();
    }
    let stage1 = Sha256::digest(password.as_bytes());
    let stage2 = Sha256::digest(stage1);
    let mut h = Sha256::new();
    h.update(stage2);
    h.update(nonce);
    let stage3 = h.finalize();
    stage1.iter().zip(stage3.iter()).map(|(a, b)| a ^ b).collect()
}

/// One answer to a statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Ok {
        affected: u64,
        last_insert_id: u64,
        status: u16,
    },
    Err {
        code: u16,
        state: String,
        message: String,
    },
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Option<String>>>,
        status: u16,
    },
}

impl Answer {
    pub fn status(&self) -> u16 {
        match self {
            Self::Ok { status, .. } | Self::Rows { status, .. } => *status,
            Self::Err { .. } => 0,
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok { .. })
    }

    pub fn err_code(&self) -> Option<u16> {
        match self {
            Self::Err { code, .. } => Some(*code),
            _ => None,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Err { message, .. } => message.clone(),
            other => panic!("expected an error, got {other:?}"),
        }
    }

    /// The rows of a result set, NULL as `"NULL"`.
    pub fn rows(&self) -> Vec<Vec<String>> {
        match self {
            Self::Rows { rows, .. } => rows
                .iter()
                .map(|row| row.iter().map(|c| c.clone().unwrap_or_else(|| "NULL".to_string())).collect())
                .collect(),
            other => panic!("expected a result set, got {other:?}"),
        }
    }

    pub fn columns(&self) -> Vec<String> {
        match self {
            Self::Rows { columns, .. } => columns.clone(),
            other => panic!("expected a result set, got {other:?}"),
        }
    }
}

pub fn decode_err(pkt: &[u8]) -> Answer {
    assert_eq!(pkt[0], 0xFF);
    let code = u16::from_le_bytes([pkt[1], pkt[2]]);
    let (state, message) = if pkt.get(3) == Some(&b'#') {
        (
            String::from_utf8_lossy(&pkt[4..9]).into_owned(),
            String::from_utf8_lossy(&pkt[9..]).into_owned(),
        )
    } else {
        (String::new(), String::from_utf8_lossy(&pkt[3..]).into_owned())
    };
    Answer::Err { code, state, message }
}

pub fn decode_ok(pkt: &[u8]) -> Answer {
    let mut pos = 1usize;
    let affected = read_lenenc_u64(pkt, &mut pos);
    let last_insert_id = read_lenenc_u64(pkt, &mut pos);
    let status = u16::from_le_bytes([pkt[pos], pkt[pos + 1]]);
    Answer::Ok {
        affected,
        last_insert_id,
        status,
    }
}

/// How the password travels in a `caching_sha2_password` full authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullAuth {
    /// As it is: what a client does on TLS and on the Unix socket.
    Cleartext,
    /// Encrypted with the server's RSA public key, which the client asks for.
    PublicKey,
}

/// How a login ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Login {
    /// Accepted; `fast` says the server took the scramble (no password was sent).
    Accepted { fast: bool },
    Refused { code: u16, state: String, message: String },
}

impl Login {
    pub fn accepted(&self) -> bool {
        matches!(self, Self::Accepted { .. })
    }
}

async fn read_raw<S: AsyncRead + Unpin>(stream: &mut S) -> Option<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 4];
    stream.read_exact(&mut hdr).await.ok()?;
    let len = (hdr[0] as usize) | ((hdr[1] as usize) << 8) | ((hdr[2] as usize) << 16);
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await.ok()?;
    Some((hdr[3], payload))
}

pub struct Client<S> {
    pub stream: S,
    pub greeting: Greeting,
    pub caps: u32,
    /// The sequence number of the last packet read.
    pub last_seq: u8,
    /// The scramble in force (the greeting's, or an AuthSwitchRequest's).
    pub nonce: Vec<u8>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Client<S> {
    /// Read the greeting from a freshly accepted connection.
    pub async fn greet(mut stream: S) -> Self {
        let (seq, raw) = read_raw(&mut stream).await.expect("a greeting");
        let greeting = parse_greeting(&raw);
        Self {
            stream,
            nonce: greeting.nonce.clone(),
            greeting,
            caps: CAPS,
            last_seq: seq,
        }
    }

    pub async fn write_packet(&mut self, seq: u8, payload: &[u8]) {
        let len = payload.len();
        let hdr = [
            (len & 0xFF) as u8,
            ((len >> 8) & 0xFF) as u8,
            ((len >> 16) & 0xFF) as u8,
            seq,
        ];
        self.stream.write_all(&hdr).await.unwrap();
        self.stream.write_all(payload).await.unwrap();
        self.stream.flush().await.unwrap();
    }

    /// Write the next packet of an exchange (the handshake's numbering continues).
    pub async fn reply(&mut self, payload: &[u8]) {
        let seq = self.last_seq.wrapping_add(1);
        self.write_packet(seq, payload).await;
    }

    pub async fn try_read_packet(&mut self) -> Option<Vec<u8>> {
        let (seq, payload) = read_raw(&mut self.stream).await?;
        self.last_seq = seq;
        Some(payload)
    }

    pub async fn read_packet(&mut self) -> Vec<u8> {
        self.try_read_packet().await.expect("the server closed the connection")
    }

    /// Send the handshake response: `auth` is the first answer of `plugin`.
    pub async fn send_handshake_response(
        &mut self,
        caps: u32,
        user: &str,
        auth: &[u8],
        database: Option<&str>,
        plugin: Option<&str>,
    ) {
        self.caps = caps;
        let mut p = Vec::new();
        p.extend_from_slice(&caps.to_le_bytes());
        p.extend_from_slice(&(1u32 << 24).to_le_bytes());
        p.push(45); // utf8mb4_general_ci
        p.extend_from_slice(&[0u8; 23]);
        p.extend_from_slice(user.as_bytes());
        p.push(0);
        p.push(auth.len() as u8);
        p.extend_from_slice(auth);
        if caps & CLIENT_CONNECT_WITH_DB != 0 {
            p.extend_from_slice(database.unwrap_or("").as_bytes());
            p.push(0);
        }
        if caps & CLIENT_PLUGIN_AUTH != 0 {
            p.extend_from_slice(plugin.unwrap_or("").as_bytes());
            p.push(0);
        }
        self.reply(&p).await;
    }

    /// The rest of a `caching_sha2_password` exchange, after the scramble was
    /// sent: the fast-path acknowledgement, or full authentication by `full`.
    pub async fn finish_sha2(&mut self, password: &str, full: FullAuth) -> Login {
        loop {
            let pkt = self.read_packet().await;
            match pkt[0] {
                0x00 => return Login::Accepted { fast: false },
                0xFF => {
                    let Answer::Err { code, state, message } = decode_err(&pkt) else {
                        unreachable!()
                    };
                    return Login::Refused { code, state, message };
                }
                0x01 if pkt.get(1) == Some(&0x03) => {
                    let ok = self.read_packet().await;
                    assert_eq!(ok[0], 0x00, "OK after fast authentication, got {ok:?}");
                    return Login::Accepted { fast: true };
                }
                0x01 if pkt.get(1) == Some(&0x04) => match full {
                    FullAuth::Cleartext => {
                        let mut p = password.as_bytes().to_vec();
                        p.push(0);
                        self.reply(&p).await;
                    }
                    FullAuth::PublicKey => {
                        self.reply(&[0x02]).await;
                        let key = self.read_packet().await;
                        if key[0] == 0xFF {
                            let Answer::Err { code, state, message } = decode_err(&key) else {
                                unreachable!()
                            };
                            return Login::Refused { code, state, message };
                        }
                        assert_eq!(key[0], 0x01, "AuthMoreData carrying the public key");
                        let pem = String::from_utf8(key[1..].to_vec()).unwrap();
                        assert!(pem.starts_with("-----BEGIN PUBLIC KEY-----"), "{pem}");
                        let sealed =
                            heliosdb_nano::protocol::mysql::auth::rsa_encrypt_password(&pem, password, &self.nonce)
                                .expect("encrypt under the server's key");
                        self.reply(&sealed).await;
                    }
                },
                other => panic!("unexpected packet 0x{other:02x} in the login exchange: {pkt:?}"),
            }
        }
    }

    /// Log in the way a MySQL 8 client does: `caching_sha2_password`, the
    /// scramble first.
    pub async fn login_sha2(&mut self, caps: u32, user: &str, password: &str, full: FullAuth) -> Login {
        let scramble = sha2_scramble(password, &self.nonce);
        self.send_handshake_response(caps, user, &scramble, None, Some(CACHING_SHA2))
            .await;
        self.finish_sha2(password, full).await
    }

    /// Read an `AuthSwitchRequest`; returns the plugin it names and adopts its scramble.
    pub async fn read_auth_switch(&mut self) -> String {
        let pkt = self.read_packet().await;
        assert_eq!(pkt[0], 0xFE, "AuthSwitchRequest, got {pkt:?}");
        let mut pos = 1usize;
        let plugin = read_nul_str(&pkt, &mut pos);
        let mut nonce = pkt[pos..].to_vec();
        if nonce.last() == Some(&0) {
            nonce.pop();
        }
        self.nonce = nonce;
        plugin
    }

    /// `COM_CHANGE_USER`, then the exchange the server asks for.
    pub async fn change_user(&mut self, user: &str, password: &str, full: FullAuth) -> Login {
        let mut p = vec![COM_CHANGE_USER];
        p.extend_from_slice(user.as_bytes());
        p.push(0);
        let scramble = sha2_scramble(password, &self.nonce);
        p.push(scramble.len() as u8);
        p.extend_from_slice(&scramble);
        p.push(0); // no database
        p.extend_from_slice(&45u16.to_le_bytes());
        p.extend_from_slice(CACHING_SHA2.as_bytes());
        p.push(0);
        self.write_packet(0, &p).await;
        let first = self.read_packet().await;
        match first[0] {
            0x00 => Login::Accepted { fast: false },
            0xFF => {
                let Answer::Err { code, state, message } = decode_err(&first) else {
                    unreachable!()
                };
                Login::Refused { code, state, message }
            }
            0xFE => {
                let mut pos = 1usize;
                let plugin = read_nul_str(&first, &mut pos);
                assert_eq!(plugin, CACHING_SHA2);
                let mut nonce = first[pos..].to_vec();
                if nonce.last() == Some(&0) {
                    nonce.pop();
                }
                self.nonce = nonce;
                let scramble = sha2_scramble(password, &self.nonce);
                self.reply(&scramble).await;
                self.finish_sha2(password, full).await
            }
            other => panic!("unexpected answer 0x{other:02x} to COM_CHANGE_USER"),
        }
    }

    /// `COM_RESET_CONNECTION`.
    pub async fn reset_connection(&mut self) -> Answer {
        self.write_packet(0, &[COM_RESET_CONNECTION]).await;
        let pkt = self.read_packet().await;
        if pkt[0] == 0xFF {
            decode_err(&pkt)
        } else {
            decode_ok(&pkt)
        }
    }

    /// Send a `COM_QUERY` without waiting for its answer ([`Self::read_answer`]).
    pub async fn start_query(&mut self, sql: &str) {
        let mut p = vec![COM_QUERY];
        p.extend_from_slice(sql.as_bytes());
        self.write_packet(0, &p).await;
    }

    pub async fn read_answer(&mut self) -> Answer {
        let first = self.read_packet().await;
        match first[0] {
            0x00 => decode_ok(&first),
            0xFF => decode_err(&first),
            _ => {
                let mut pos = 0usize;
                let ncols = read_lenenc_u64(&first, &mut pos) as usize;
                let mut columns = Vec::with_capacity(ncols);
                for _ in 0..ncols {
                    let def = self.read_packet().await;
                    let mut dp = 0usize;
                    for _ in 0..4 {
                        let _ = read_lenenc_str(&def, &mut dp);
                    }
                    columns.push(read_lenenc_str(&def, &mut dp));
                }
                let eof = self.read_packet().await;
                assert_eq!(eof[0], 0xFE, "EOF after the column definitions");
                let mut rows = Vec::new();
                loop {
                    let pkt = self.read_packet().await;
                    if pkt[0] == 0xFF {
                        return decode_err(&pkt);
                    }
                    if pkt[0] == 0xFE && pkt.len() < 9 {
                        let status = u16::from_le_bytes([pkt[3], pkt[4]]);
                        return Answer::Rows { columns, rows, status };
                    }
                    let mut rp = 0usize;
                    let mut row = Vec::with_capacity(ncols);
                    for _ in 0..ncols {
                        if pkt[rp] == 0xFB {
                            rp += 1;
                            row.push(None);
                        } else {
                            row.push(Some(read_lenenc_str(&pkt, &mut rp)));
                        }
                    }
                    rows.push(row);
                }
            }
        }
    }

    /// `COM_QUERY`: every answer the server gives, following
    /// `SERVER_MORE_RESULTS_EXISTS`.
    pub async fn query_all(&mut self, sql: &str) -> Vec<Answer> {
        let mut p = vec![COM_QUERY];
        p.extend_from_slice(sql.as_bytes());
        self.write_packet(0, &p).await;
        let mut answers = Vec::new();
        loop {
            let answer = self.read_answer().await;
            let more = answer.status() & SERVER_MORE_RESULTS_EXISTS != 0;
            answers.push(answer);
            if !more {
                return answers;
            }
        }
    }

    /// `COM_QUERY` with one answer.
    pub async fn query(&mut self, sql: &str) -> Answer {
        let mut answers = self.query_all(sql).await;
        assert_eq!(answers.len(), 1, "one answer for `{sql}`, got {answers:?}");
        answers.remove(0)
    }

    /// A statement that must answer OK; returns the status flags.
    pub async fn ok(&mut self, sql: &str) -> u16 {
        match self.query(sql).await {
            Answer::Ok { status, .. } => status,
            other => panic!("expected OK for `{sql}`, got {other:?}"),
        }
    }

    /// A statement that must answer OK; returns the affected rows.
    pub async fn affected(&mut self, sql: &str) -> u64 {
        match self.query(sql).await {
            Answer::Ok { affected, .. } => affected,
            other => panic!("expected OK for `{sql}`, got {other:?}"),
        }
    }

    /// A statement that must be refused; returns the error number and message.
    pub async fn err(&mut self, sql: &str) -> (u16, String) {
        match self.query(sql).await {
            Answer::Err { code, message, .. } => (code, message),
            other => panic!("expected an error for `{sql}`, got {other:?}"),
        }
    }

    /// The rows of a query, NULL as `"NULL"`.
    pub async fn rows(&mut self, sql: &str) -> Vec<Vec<String>> {
        let answer = self.query(sql).await;
        assert!(matches!(answer, Answer::Rows { .. }), "a result set for `{sql}`, got {answer:?}");
        answer.rows()
    }

    /// The one value of a one-row, one-column result.
    pub async fn scalar(&mut self, sql: &str) -> String {
        let rows = self.rows(sql).await;
        assert_eq!(rows.len(), 1, "one row from `{sql}`, got {rows:?}");
        rows[0][0].clone()
    }

    /// The value `SHOW VARIABLES LIKE 'name'` lists for exactly `name`.
    pub async fn variable(&mut self, name: &str) -> String {
        let rows = self.rows(&format!("SHOW VARIABLES LIKE '{name}'")).await;
        assert_eq!(rows.len(), 1, "SHOW VARIABLES LIKE '{name}' listed {rows:?}");
        assert_eq!(rows[0][0], name);
        rows[0][1].clone()
    }
}

pub fn test_db() -> Arc<EmbeddedDatabase> {
    Arc::new(EmbeddedDatabase::new_in_memory().expect("in-memory db"))
}

/// An authenticator over a SCRAM store holding the given accounts: what a
/// server started with `--auth scram-sha-256` hands its MySQL listeners.
pub fn scram_auth(accounts: &[(&str, &str)]) -> Arc<MysqlAuth> {
    let store = SharedPasswordStore::new(InMemoryPasswordStore::new());
    for (user, password) in accounts {
        store.add_user(user, password).expect("add account");
    }
    let manager = Arc::new(AuthManager::with_password_store(AuthMethod::ScramSha256, store));
    MysqlAuth::for_manager(manager).expect("an authenticator for a method that asks for a password")
}

/// Open a connection to a listener over an in-memory stream and read its
/// greeting. `auth` is the listener's authenticator (`None` is trust);
/// `secure` says whether the transport counts as TLS or the Unix socket.
pub async fn open(db: &Arc<EmbeddedDatabase>, auth: Option<Arc<MysqlAuth>>, secure: bool) -> Client<DuplexStream> {
    open_with_timeouts(db, auth, secure, ConnectionTimeouts::disabled()).await
}

pub async fn open_with_timeouts(
    db: &Arc<EmbeddedDatabase>,
    auth: Option<Arc<MysqlAuth>>,
    secure: bool,
    timeouts: ConnectionTimeouts,
) -> Client<DuplexStream> {
    let (client, srv) = tokio::io::duplex(1 << 20);
    let db = Arc::clone(db);
    tokio::spawn(async move {
        let _ = MySqlHandler::handle_connection_with_auth(db, srv, 1, timeouts, auth, secure).await;
    });
    Client::greet(client).await
}

/// A logged-in connection to a trust listener, with the given capabilities.
pub async fn trust_session(db: &Arc<EmbeddedDatabase>, caps: u32) -> Client<DuplexStream> {
    trust_session_as(db, caps, "root").await
}

pub async fn trust_session_as(db: &Arc<EmbeddedDatabase>, caps: u32, user: &str) -> Client<DuplexStream> {
    let mut client = open(db, None, false).await;
    client.send_handshake_response(caps, user, &[], None, Some(NATIVE)).await;
    let ok = client.read_packet().await;
    assert_eq!(ok[0], 0x00, "login on a trust listener, got {ok:?}");
    client
}
