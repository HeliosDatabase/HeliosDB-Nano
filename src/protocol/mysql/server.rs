//! MySQL TCP server
//!
//! Mirrors [`crate::protocol::postgres::server`]: a `MysqlServerConfig` /
//! `MysqlServer` pair that owns the listener, the connection-limiting
//! semaphore and (new) TLS negotiation for the MySQL wire protocol.
//!
//! ## TLS negotiation shape
//!
//! Unlike PostgreSQL (where the client asks for SSL BEFORE the server has
//! said anything), MySQL's server speaks first: it sends the HandshakeV10
//! greeting, and only then does the client's `HandshakeResponse41` reveal —
//! via the `CLIENT_SSL` capability bit — whether it wants to upgrade. If it
//! does, that first response is a TRUNCATED `SSLRequest` (capability flags +
//! max packet size + charset + 23 reserved bytes, no username/auth yet); the
//! real `HandshakeResponse41` follows over the now-encrypted stream.
//!
//! So the greeting is always sent once, in plaintext, before the TLS
//! decision is even knowable — [`MysqlServer::negotiate`] therefore builds
//! and sends that greeting itself (via the same
//! [`super::handler::build_handshake_v10`] bytes `MySqlHandler` would send),
//! reads the client's first response, and only constructs the
//! `MySqlHandler` once the final stream (plain or TLS-upgraded) is known —
//! via [`super::handler::MySqlHandler::new_pre_negotiated`] /
//! `finish_handshake`, passing through the SAME seed/capabilities/charset so
//! the handler's own state matches what the client already saw.

use super::handler::{
    build_handshake_v10, read_packet, write_packet, CapabilityFlags, HandshakeResponse, MySqlHandler, StatusFlags,
    UTF8MB4_GENERAL_CI,
};
use bytes::{BufMut, BytesMut};
use super::ssl::{MysqlSslConfig, MysqlSslNegotiator};
use crate::protocol::postgres::timeouts::ConnectionTimeouts;
use crate::protocol::tls_stream::SecureConnection;
use crate::{EmbeddedDatabase, Error, Result};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

/// Default MySQL listen address (127.0.0.1:3306 — localhost-only, matching
/// the existing `--mysql-listen` CLI default).
const DEFAULT_MYSQL_ADDRESS: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3306);

/// MySQL server configuration.
#[derive(Clone)]
pub struct MysqlServerConfig {
    /// Listen address
    pub address: SocketAddr,
    /// Maximum concurrent connections
    pub max_connections: usize,
    /// TLS configuration (optional — TLS is off by default, matching the
    /// pre-existing MySQL listener which had none at all)
    pub ssl_config: Option<MysqlSslConfig>,
    /// GH#28-style connection-lifetime policy, shared with the PostgreSQL
    /// listener's `ConnectionTimeouts`.
    pub timeouts: ConnectionTimeouts,
}

impl Default for MysqlServerConfig {
    fn default() -> Self {
        Self {
            address: DEFAULT_MYSQL_ADDRESS,
            max_connections: 100,
            ssl_config: None,
            timeouts: ConnectionTimeouts::disabled(),
        }
    }
}

impl MysqlServerConfig {
    /// Create with a custom address.
    pub fn with_address(address: SocketAddr) -> Self {
        Self {
            address,
            ..Default::default()
        }
    }

    /// Set maximum connections.
    pub fn with_max_connections(mut self, max: usize) -> Self {
        self.max_connections = max;
        self
    }

    /// Set the connection-lifetime policy.
    pub fn with_timeouts(mut self, timeouts: ConnectionTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// Set TLS configuration.
    pub fn with_ssl(mut self, ssl_config: MysqlSslConfig) -> Self {
        self.ssl_config = Some(ssl_config);
        self
    }
}

/// MySQL server.
pub struct MysqlServer {
    config: MysqlServerConfig,
    database: Arc<EmbeddedDatabase>,
    ssl_negotiator: Option<Arc<MysqlSslNegotiator>>,
    connection_limiter: Arc<Semaphore>,
    conn_counter: AtomicU32,
    utilisation_warned: AtomicBool,
}

impl MysqlServer {
    /// Create a new MySQL server.
    pub fn new(config: MysqlServerConfig, database: Arc<EmbeddedDatabase>) -> Result<Self> {
        let ssl_negotiator = if let Some(ref ssl_config) = config.ssl_config {
            if ssl_config.enabled {
                Some(Arc::new(MysqlSslNegotiator::new(ssl_config.clone())?))
            } else {
                None
            }
        } else {
            None
        };

        let connection_limiter = Arc::new(Semaphore::new(config.max_connections));

        Ok(Self {
            config,
            database,
            ssl_negotiator,
            connection_limiter,
            conn_counter: AtomicU32::new(1),
            utilisation_warned: AtomicBool::new(false),
        })
    }

    /// Get server configuration.
    pub fn config(&self) -> &MysqlServerConfig {
        &self.config
    }

    fn maybe_warn_utilisation(&self) {
        self.config.timeouts.maybe_warn_utilisation(
            "MySQL",
            &self.utilisation_warned,
            &self.connection_limiter,
            self.config.max_connections,
        );
    }

    /// Start the server and listen for connections. Does not return unless
    /// an error occurs — run it under `tokio::spawn`.
    pub async fn serve(&self) -> Result<()> {
        let listener = TcpListener::bind(self.config.address)
            .await
            .map_err(|e| Error::network(format!("Failed to bind to {}: {}", self.config.address, e)))?;

        tracing::info!(
            "MySQL server listening on {} (tls: {})",
            self.config.address,
            if self.ssl_negotiator.is_some() { "enabled" } else { "disabled" }
        );

        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    if let Err(e) =
                        crate::protocol::postgres::timeouts::apply_socket_options(&stream, &self.config.timeouts)
                    {
                        tracing::warn!("Failed to apply socket options for MySQL {}: {}", addr, e);
                    }

                    let permit = match Arc::clone(&self.connection_limiter).try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => {
                            tracing::warn!(
                                "MySQL connection limit reached ({}), rejecting {}",
                                self.config.max_connections,
                                addr
                            );
                            drop(stream);
                            continue;
                        }
                    };
                    self.maybe_warn_utilisation();

                    let database = Arc::clone(&self.database);
                    let ssl_negotiator = self.ssl_negotiator.clone();
                    let timeouts = self.config.timeouts.clone();
                    let connection_id = self.conn_counter.fetch_add(1, Ordering::Relaxed);

                    tokio::spawn(async move {
                        let _permit = permit;
                        if let Err(e) =
                            Self::handle_connection(stream, database, ssl_negotiator, timeouts, connection_id).await
                        {
                            tracing::debug!("MySQL connection {} error from {}: {}", connection_id, addr, e);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("MySQL accept error: {}", e);
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        }
    }

    /// GH#28-style bound: the whole handshake — greeting, first-response
    /// read, TLS upgrade (`acceptor.accept(stream).await` included) and the
    /// post-upgrade re-read — runs under ONE absolute
    /// `authentication_timeout` deadline computed here at connection-accept
    /// time, mirroring `PgServer::handle_connection` /
    /// `MySqlHandler::handle_connection_with_timeouts`. A client that never
    /// completes its handshake (or stalls mid-TLS-handshake) is dropped
    /// instead of holding a `max_connections` permit forever. On expiry
    /// nothing further is written to the unauthenticated peer — the
    /// half-built stream is simply dropped.
    async fn handle_connection(
        stream: TcpStream,
        database: Arc<EmbeddedDatabase>,
        ssl_negotiator: Option<Arc<MysqlSslNegotiator>>,
        timeouts: ConnectionTimeouts,
        connection_id: u32,
    ) -> Result<()> {
        let auth_deadline = timeouts
            .read_deadline(crate::protocol::postgres::timeouts::SessionActivity::Authenticating)
            .map(|d| tokio::time::Instant::now() + d);

        let negotiate = Self::negotiate(stream, database, ssl_negotiator, connection_id);
        let mut handler = match auth_deadline {
            Some(at) => match tokio::time::timeout_at(at, negotiate).await {
                Ok(negotiated) => negotiated.map_err(|e| Error::network(format!("MySQL TLS negotiation failed: {}", e)))?,
                Err(_elapsed) => {
                    tracing::debug!(
                        "MySQL connection {}: authentication_timeout expired during handshake; closing",
                        connection_id
                    );
                    return Ok(());
                }
            },
            None => negotiate
                .await
                .map_err(|e| Error::network(format!("MySQL TLS negotiation failed: {}", e)))?,
        };
        handler.set_timeouts(timeouts);
        handler
            .run_command_loop()
            .await
            .map_err(|e| Error::network(e.to_string()))
    }

    /// Send the HandshakeV10 greeting, read the client's first response,
    /// upgrade to TLS if the client set `CLIENT_SSL` and TLS is configured,
    /// and return a fully authenticated [`MySqlHandler`] ready for
    /// [`MySqlHandler::run_command_loop`].
    ///
    /// If `ssl_negotiator.config().require_tls` is set and the client did
    /// NOT request TLS, the connection is rejected (an access-denied-style
    /// ERR packet is sent, then the connection is closed) rather than
    /// allowed to proceed in cleartext.
    async fn negotiate(
        stream: TcpStream,
        database: Arc<EmbeddedDatabase>,
        ssl_negotiator: Option<Arc<MysqlSslNegotiator>>,
        connection_id: u32,
    ) -> super::handler::Result<MySqlHandler<SecureConnection<TcpStream>>> {
        let mut auth_seed = [0u8; 20];
        {
            use rand::Rng;
            rand::thread_rng().fill(&mut auth_seed);
        }
        let has_tls = ssl_negotiator.is_some();
        let capabilities = CapabilityFlags::server_default(has_tls);
        let status_flags = StatusFlags::default_flags();
        let character_set = UTF8MB4_GENERAL_CI;
        let auth_plugin = "mysql_native_password".to_string();

        let greeting = build_handshake_v10(connection_id, &auth_seed, &capabilities, character_set, &status_flags, &auth_plugin);

        let mut stream = stream;
        write_packet(&mut stream, 0, &greeting).await?;
        let (first_seq, first_payload) = read_packet(&mut stream).await?;

        // Peek the capability-flags u32 (first 4 bytes of EVERY
        // HandshakeResponse41 variant, truncated SSLRequest included)
        // without running the full `decode()` — an SSLRequest has none of
        // the fields decode() expects beyond it.
        let client_wants_ssl = first_payload.len() >= 4
            && (u32::from_le_bytes([first_payload[0], first_payload[1], first_payload[2], first_payload[3]])
                & CapabilityFlags::CLIENT_SSL)
                != 0;

        let (secure_stream, final_seq, hs_payload, kx_group) = if client_wants_ssl {
            match ssl_negotiator.as_ref() {
                Some(negotiator) => {
                    tracing::debug!("MySQL connection {}: upgrading to TLS", connection_id);
                    let tls_stream = negotiator.acceptor().accept(stream).await.map_err(|e| {
                        super::handler::MySqlError::Protocol(format!("TLS handshake failed: {}", e))
                    })?;
                    // Capture the negotiated key-exchange group (PQ hybrid
                    // `X25519MLKEM768` vs classical `X25519`/etc.) before the
                    // stream is wrapped — `.get_ref().1` is the
                    // `&ServerConnection`, which derefs to `CommonState`.
                    let kx_group = tls_stream
                        .get_ref()
                        .1
                        .negotiated_key_exchange_group()
                        .map(|g| format!("{:?}", g.name()));
                    if let Some(group) = &kx_group {
                        tracing::debug!(
                            "MySQL connection {}: TLS negotiated key-exchange group: {}",
                            connection_id,
                            group
                        );
                    }
                    let mut secure = SecureConnection::Tls(tls_stream);
                    // The truncated SSLRequest carried no username/auth —
                    // the REAL HandshakeResponse41 arrives now, over the
                    // encrypted stream.
                    let (seq2, payload2) = read_packet(&mut secure).await?;
                    (secure, seq2, payload2, kx_group)
                }
                None => {
                    // Client asked for SSL but the server didn't advertise
                    // CLIENT_SSL (unreachable in practice: `capabilities`
                    // only sets the bit when `ssl_negotiator` is `Some`) —
                    // fail closed rather than silently downgrading.
                    return Err(super::handler::MySqlError::Protocol(
                        "client requested TLS but the server has none configured".into(),
                    ));
                }
            }
        } else {
            // Client didn't set CLIENT_SSL. If this listener requires TLS
            // for every connection, reject rather than silently accepting
            // cleartext — `require_tls` must not be a no-op.
            if ssl_negotiator
                .as_ref()
                .map(|n| n.config().require_tls)
                .unwrap_or(false)
            {
                let mut stream = stream;
                let err = build_access_denied_packet(&capabilities, "TLS is required by this server");
                // Best-effort: the client is being rejected either way, and
                // a write failure here (e.g. peer already gone) must not
                // mask the rejection as a different kind of error.
                let _ = write_packet(&mut stream, first_seq.wrapping_add(1), &err).await;
                return Err(super::handler::MySqlError::Protocol(
                    "rejected plaintext connection: TLS is required (require_tls)".into(),
                ));
            }
            (SecureConnection::Plain(stream), first_seq, first_payload, None)
        };

        let hs = HandshakeResponse::decode(hs_payload, &capabilities)?;

        let mut handler = MySqlHandler::new_pre_negotiated(
            database,
            secure_stream,
            connection_id,
            final_seq.wrapping_add(1),
            auth_seed,
            capabilities,
            character_set,
            status_flags,
            auth_plugin,
            has_tls,
            kx_group,
        );
        handler.finish_handshake(hs).await?;
        Ok(handler)
    }
}

/// Build a MySQL ERR packet payload (access-denied-style, code 1045,
/// SQL state 28000) used to reject a plaintext client when `require_tls` is
/// set. Standalone (not a method on `MySqlHandler`) because rejection
/// happens in [`MysqlServer::negotiate`] before any handler exists.
fn build_access_denied_packet(capabilities: &CapabilityFlags, msg: &str) -> BytesMut {
    let mut p = BytesMut::new();
    p.put_u8(0xFF); // ERR header
    p.put_u16_le(1045); // ER_ACCESS_DENIED_ERROR

    if capabilities.has(CapabilityFlags::CLIENT_PROTOCOL_41) {
        p.put_u8(b'#');
        let state = b"28000"; // invalid authorization spec
        p.put_slice(state);
    }

    p.put_slice(msg.as_bytes());
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = MysqlServerConfig::default();
        assert_eq!(config.address.port(), 3306);
        assert_eq!(config.max_connections, 100);
        assert!(config.ssl_config.is_none());
    }

    #[test]
    fn test_config_builder() {
        let addr: SocketAddr = "127.0.0.1:13306".parse().unwrap();
        let config = MysqlServerConfig::with_address(addr).with_max_connections(50);
        assert_eq!(config.address, addr);
        assert_eq!(config.max_connections, 50);
    }

    #[test]
    fn test_config_with_ssl() {
        let ssl_config = MysqlSslConfig::new("cert.pem", "key.pem");
        let config = MysqlServerConfig::default().with_ssl(ssl_config);
        assert!(config.ssl_config.is_some());
    }
}
