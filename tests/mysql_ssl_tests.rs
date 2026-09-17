//! MySQL SSL/TLS integration tests.
//!
//! Speaks the MySQL wire protocol at the raw-packet level (no `mysql` crate
//! dependency) rather than through a client library, mirroring how
//! `tests/postgres_ssl_tests.rs` drives its server with raw `TcpStream`
//! reads/writes. Nano's MySQL listener is trust-authenticated, so a
//! zero-length auth response is accepted for any username — these tests
//! exercise the TLS negotiation and PQ hybrid key-exchange path, not
//! credential verification.

use heliosdb_nano::protocol::mysql::{MysqlServer, MysqlServerConfig, MysqlSslConfig};
use heliosdb_nano::protocol::tls_provider::pq_capable_provider;
use heliosdb_nano::{EmbeddedDatabase, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
const CLIENT_SSL: u32 = 0x0000_0800;

/// Probe-derive a free loopback port (same rationale as
/// `postgres_ssl_tests::free_loopback_port`: hardcoded ports collide with
/// long-running containers on shared hosts).
fn free_loopback_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral probe port")
        .local_addr()
        .expect("probe local_addr")
        .port()
}

async fn read_packet<S: AsyncRead + Unpin>(stream: &mut S) -> std::io::Result<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 4];
    stream.read_exact(&mut hdr).await?;
    let len = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], 0]) as usize;
    let seq = hdr[3];
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    Ok((seq, payload))
}

async fn write_packet<S: AsyncWrite + Unpin>(stream: &mut S, seq: u8, payload: &[u8]) -> std::io::Result<()> {
    let len = payload.len() as u32;
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.push((len & 0xFF) as u8);
    buf.push(((len >> 8) & 0xFF) as u8);
    buf.push(((len >> 16) & 0xFF) as u8);
    buf.push(seq);
    buf.extend_from_slice(payload);
    stream.write_all(&buf).await?;
    stream.flush().await?;
    Ok(())
}

/// Build a truncated `SSLRequest` payload — capability flags (with
/// `CLIENT_SSL` set) + max packet size + charset + 23 reserved bytes, no
/// username/auth yet, matching what a real MySQL client sends before
/// upgrading to TLS.
fn build_ssl_request(capabilities: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32);
    buf.extend_from_slice(&capabilities.to_le_bytes());
    buf.extend_from_slice(&16_777_216u32.to_le_bytes());
    buf.push(45); // utf8mb4_general_ci
    buf.extend_from_slice(&[0u8; 23]);
    buf
}

/// Build a full `HandshakeResponse41` payload for trust auth: capability
/// flags, max packet size, charset, 23 reserved bytes, null-terminated
/// username, then a single zero byte (CLIENT_SECURE_CONNECTION empty
/// auth-response). No database, no plugin name, no connect attrs — this
/// client doesn't set those capability bits.
fn build_handshake_response(capabilities: u32, username: &str) -> Vec<u8> {
    let mut buf = build_ssl_request(capabilities);
    buf.extend_from_slice(username.as_bytes());
    buf.push(0); // null terminator
    buf.push(0); // auth-response length = 0
    buf
}

/// Accepts any server certificate — this suite tests TLS negotiation and
/// key-exchange group selection, not certificate-chain validation, against
/// the self-signed test cert `CertificateManager::setup_test_certs()` writes.
#[derive(Debug)]
struct AcceptAnyCert;

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
        ]
    }
}

fn client_config(post_quantum: bool) -> Arc<ClientConfig> {
    let provider = pq_capable_provider(post_quantum);
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3 only (PQ hybrid key exchange is TLS 1.3-only)")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_no_client_auth();
    Arc::new(config)
}

/// Generate a self-signed test cert/key pair into a per-test temp
/// directory — NOT the shared `certs/server.{crt,key}` path
/// `CertificateManager::setup_test_certs()` writes: several of these tests
/// build a real `rustls::ServerConfig` and run in parallel within the same
/// test binary, and two of them regenerating the SAME shared file pair
/// raced and intermittently produced a mismatched cert/key
/// ("keys may not be consistent: KeyMismatch") — see the identical fix in
/// `tests/postgres_ssl_tests.rs`.
fn setup_test_certs() -> Result<(tempfile::TempDir, std::path::PathBuf, std::path::PathBuf)> {
    use heliosdb_nano::protocol::postgres::CertificateManager;
    let temp_dir = tempfile::TempDir::new().map_err(|e| heliosdb_nano::Error::io(format!("temp dir: {}", e)))?;
    let cert_path = temp_dir.path().join("server.crt");
    let key_path = temp_dir.path().join("server.key");
    // `generate_test_cert()`'s hardcoded fallback PEM template doesn't
    // actually parse — use the real `openssl`-backed generator instead
    // (same one `setup_test_certs()` on `CertificateManager` uses).
    CertificateManager::generate_self_signed(&cert_path, &key_path, "localhost")?;
    Ok((temp_dir, cert_path, key_path))
}

async fn start_mysql_server(ssl: Option<MysqlSslConfig>) -> Result<SocketAddr> {
    let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
    let addr: SocketAddr = format!("127.0.0.1:{}", free_loopback_port())
        .parse()
        .map_err(|e| heliosdb_nano::Error::config(format!("Invalid address: {}", e)))?;
    let mut config = MysqlServerConfig::with_address(addr);
    if let Some(ssl) = ssl {
        config = config.with_ssl(ssl);
    }
    let server = MysqlServer::new(config, db)?;
    let server_addr = server.config().address;
    tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(server_addr)
}

/// Non-TLS client: read the greeting, send a full `HandshakeResponse41`
/// directly (no `CLIENT_SSL`), and expect an OK packet back.
async fn plaintext_login(addr: SocketAddr) -> Result<()> {
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("connect failed: {}", e)))?;
    let (_seq, _greeting) = read_packet(&mut stream)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("greeting read failed: {}", e)))?;

    let response = build_handshake_response(CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION, "test_user");
    write_packet(&mut stream, 1, &response)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("handshake response write failed: {}", e)))?;

    let (_seq, ok_payload) = read_packet(&mut stream)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("OK read failed: {}", e)))?;
    assert_eq!(ok_payload.first().copied(), Some(0x00), "expected an OK packet (0x00), got {:?}", ok_payload);
    Ok(())
}

/// TLS client: read the greeting, send a truncated SSLRequest, complete a
/// real rustls handshake, then send the full `HandshakeResponse41` over the
/// encrypted stream and expect an OK packet.
async fn tls_login(addr: SocketAddr, cfg: Arc<ClientConfig>) -> Result<rustls::NamedGroup> {
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("connect failed: {}", e)))?;
    let (_seq, _greeting) = read_packet(&mut stream)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("greeting read failed: {}", e)))?;

    let ssl_request = build_ssl_request(CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_SSL);
    write_packet(&mut stream, 1, &ssl_request)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("SSLRequest write failed: {}", e)))?;

    let connector = TlsConnector::from(cfg);
    let server_name = ServerName::try_from("localhost")
        .map_err(|e| heliosdb_nano::Error::network(format!("invalid server name: {}", e)))?
        .to_owned();
    let mut tls_stream = connector
        .connect(server_name, stream)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("TLS handshake failed: {}", e)))?;

    let group = tls_stream
        .get_ref()
        .1
        .negotiated_key_exchange_group()
        .ok_or_else(|| heliosdb_nano::Error::network("no key-exchange group negotiated".to_string()))?
        .name();

    let response = build_handshake_response(
        CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_SSL,
        "test_user",
    );
    write_packet(&mut tls_stream, 2, &response)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("handshake response write failed: {}", e)))?;

    let (_seq, ok_payload) = read_packet(&mut tls_stream)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("OK read failed: {}", e)))?;
    assert_eq!(ok_payload.first().copied(), Some(0x00), "expected an OK packet (0x00), got {:?}", ok_payload);

    Ok(group)
}

#[tokio::test]
async fn plain_client_connects_when_tls_not_configured() -> Result<()> {
    let addr = start_mysql_server(None).await?;
    tokio::time::timeout(Duration::from_secs(10), plaintext_login(addr))
        .await
        .expect("login timed out")
}

#[tokio::test]
async fn plain_client_still_connects_when_tls_enabled_but_not_required() -> Result<()> {
    let (_certs_dir, cert_path, key_path) = setup_test_certs()?;
    let ssl = MysqlSslConfig::new(&cert_path, &key_path);
    let addr = start_mysql_server(Some(ssl)).await?;
    tokio::time::timeout(Duration::from_secs(10), plaintext_login(addr))
        .await
        .expect("login timed out")
}

#[tokio::test]
async fn pq_only_client_negotiates_hybrid_group_against_pq_server() -> Result<()> {
    let (_certs_dir, cert_path, key_path) = setup_test_certs()?;
    let ssl = MysqlSslConfig::new(&cert_path, &key_path).with_post_quantum(true);
    let addr = start_mysql_server(Some(ssl)).await?;
    let group = tokio::time::timeout(Duration::from_secs(10), tls_login(addr, client_config(true)))
        .await
        .expect("TLS login timed out")?;
    assert_eq!(
        group,
        rustls::NamedGroup::X25519MLKEM768,
        "a PQ-capable client against a PQ-enabled MySQL server must negotiate the hybrid group"
    );
    Ok(())
}

#[tokio::test]
async fn classical_only_client_still_succeeds_against_pq_mysql_server() -> Result<()> {
    let (_certs_dir, cert_path, key_path) = setup_test_certs()?;
    let ssl = MysqlSslConfig::new(&cert_path, &key_path).with_post_quantum(true);
    let addr = start_mysql_server(Some(ssl)).await?;
    let group = tokio::time::timeout(Duration::from_secs(10), tls_login(addr, client_config(false)))
        .await
        .expect("TLS login timed out")?;
    assert_ne!(
        group,
        rustls::NamedGroup::X25519MLKEM768,
        "a classical-only client cannot have negotiated the PQ hybrid group"
    );
    Ok(())
}

#[tokio::test]
async fn pq_disabled_mysql_server_never_offers_hybrid_group() -> Result<()> {
    let (_certs_dir, cert_path, key_path) = setup_test_certs()?;
    let ssl = MysqlSslConfig::new(&cert_path, &key_path).with_post_quantum(false);
    let addr = start_mysql_server(Some(ssl)).await?;
    let group = tokio::time::timeout(Duration::from_secs(10), tls_login(addr, client_config(true)))
        .await
        .expect("TLS login timed out")?;
    assert_ne!(
        group,
        rustls::NamedGroup::X25519MLKEM768,
        "mysql_tls_post_quantum = false must not offer the PQ hybrid group"
    );
    Ok(())
}

// ---------------------------------------------------------------------
// require_tls
// ---------------------------------------------------------------------
//
// `MysqlSslConfig::with_require_tls(true)` must actually reject a
// plaintext client rather than being a no-op (review finding #2).

/// A plaintext handshake against a `require_tls = true` server must be
/// rejected: the connection either closes without an OK packet, or
/// delivers an ERR packet — never the OK a successful login would produce.
async fn expect_plaintext_rejected(addr: SocketAddr) -> Result<()> {
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("connect failed: {}", e)))?;
    let (_seq, _greeting) = read_packet(&mut stream)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("greeting read failed: {}", e)))?;

    let response = build_handshake_response(CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION, "test_user");
    write_packet(&mut stream, 1, &response)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("handshake response write failed: {}", e)))?;

    match read_packet(&mut stream).await {
        Ok((_seq, payload)) => {
            assert_ne!(
                payload.first().copied(),
                Some(0x00),
                "require_tls=true must not let a plaintext client through with an OK packet"
            );
            // Non-OK (typically an ERR 0xFF packet) is the rejection.
        }
        Err(_) => {
            // The server closed the connection outright — also an
            // acceptable rejection.
        }
    }
    Ok(())
}

#[tokio::test]
async fn require_tls_rejects_plaintext_client() -> Result<()> {
    let (_certs_dir, cert_path, key_path) = setup_test_certs()?;
    let ssl = MysqlSslConfig::new(&cert_path, &key_path).with_require_tls(true);
    let addr = start_mysql_server(Some(ssl)).await?;
    tokio::time::timeout(Duration::from_secs(10), expect_plaintext_rejected(addr))
        .await
        .expect("rejection check timed out")
}

#[tokio::test]
async fn require_tls_still_accepts_tls_client() -> Result<()> {
    let (_certs_dir, cert_path, key_path) = setup_test_certs()?;
    let ssl = MysqlSslConfig::new(&cert_path, &key_path).with_require_tls(true);
    let addr = start_mysql_server(Some(ssl)).await?;
    tokio::time::timeout(Duration::from_secs(10), tls_login(addr, client_config(true)))
        .await
        .expect("TLS login timed out")?;
    Ok(())
}

#[tokio::test]
async fn require_tls_false_plaintext_client_still_succeeds() -> Result<()> {
    // No-regression check: `require_tls = false` (the default) must keep
    // allowing plaintext clients even when TLS is enabled/offered.
    let (_certs_dir, cert_path, key_path) = setup_test_certs()?;
    let ssl = MysqlSslConfig::new(&cert_path, &key_path).with_require_tls(false);
    let addr = start_mysql_server(Some(ssl)).await?;
    tokio::time::timeout(Duration::from_secs(10), plaintext_login(addr))
        .await
        .expect("login timed out")
}
