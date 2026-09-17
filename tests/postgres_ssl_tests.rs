//! PostgreSQL SSL/TLS integration tests
//!
//! Tests SSL/TLS encryption for PostgreSQL wire protocol connections.

use heliosdb_nano::protocol::postgres::{AuthMethod, CertificateManager, PgServerBuilder, SslConfig, SslMode};
use heliosdb_nano::{EmbeddedDatabase, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// SSL request message code
const SSL_REQUEST_CODE: i32 = 80877103;

/// Probe-derive a free loopback port. Hardcoded ports (15432/15433/…) collide
/// with long-running containers on shared hosts (observed: heliosdb-lite-ca
/// maps 0.0.0.0:15432-15433) — the test server's bind then fails inside its
/// spawned task while connect() happily reaches the FOREIGN listener, which
/// never answers the SSLRequest, wedging the test forever at 0 CPU. A small
/// bind→drop→rebind race remains, but combined with the negotiation timeout
/// below it can only produce a clean failure, never a hang.
fn free_loopback_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral probe port")
        .local_addr()
        .expect("probe local_addr")
        .port()
}

/// Create test server with SSL
async fn create_ssl_server(ssl_mode: SslMode, port: u16) -> Result<(Arc<EmbeddedDatabase>, SocketAddr)> {
    // Setup test certificates
    CertificateManager::setup_test_certs()?;

    // Create database
    let db = Arc::new(EmbeddedDatabase::new_in_memory()?);

    // Configure SSL
    let ssl_config = SslConfig::new(ssl_mode, "certs/server.crt", "certs/server.key");

    // Build server
    let addr: SocketAddr = format!("127.0.0.1:{}", port)
        .parse()
        .map_err(|e| heliosdb_nano::Error::config(format!("Invalid address: {}", e)))?;

    let server = PgServerBuilder::new()
        .address(addr)
        .auth_method(AuthMethod::Trust)
        .ssl_config(ssl_config)
        .build(db.clone())?;

    // Start server in background
    let server_addr = server.config().address;
    tokio::spawn(async move {
        if let Err(e) = server.serve().await {
            eprintln!("Server error: {}", e);
        }
    });

    // Wait for server to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    Ok((db, server_addr))
}

/// Send SSL request and check response
async fn send_ssl_request(stream: &mut TcpStream) -> Result<bool> {
    // Send SSL request message
    // Length (8 bytes total)
    stream
        .write_i32(8)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("Write failed: {}", e)))?;

    // SSL request code
    stream
        .write_i32(SSL_REQUEST_CODE)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("Write failed: {}", e)))?;

    stream
        .flush()
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("Flush failed: {}", e)))?;

    // Read response (should be 'S' or 'N')
    let mut response = [0u8; 1];
    stream
        .read_exact(&mut response)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("Read failed: {}", e)))?;

    Ok(response[0] == b'S')
}

#[tokio::test]
#[ignore = "Requires Rustls CryptoProvider configuration"]
async fn test_ssl_mode_allow_accepts_ssl_request() -> Result<()> {
    let (_db, addr) = create_ssl_server(SslMode::Allow, free_loopback_port()).await?;

    // Connect to server
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("Connection failed: {}", e)))?;

    // Send SSL request
    let ssl_accepted = send_ssl_request(&mut stream).await?;

    // Server should accept SSL request
    assert!(ssl_accepted, "Server should accept SSL request in Allow mode");

    Ok(())
}

#[tokio::test]
async fn test_ssl_mode_disable_rejects_ssl_request() -> Result<()> {
    // Setup test certificates
    CertificateManager::setup_test_certs()?;

    let db = Arc::new(EmbeddedDatabase::new_in_memory()?);

    // Configure SSL as disabled
    let ssl_config = SslConfig::new(SslMode::Disable, "certs/server.crt", "certs/server.key");

    // Ephemeral port: 15433 collided with the heliosdb-lite-ca container on
    // shared hosts (see free_loopback_port), turning this test into an
    // indefinite hang against a foreign listener.
    let addr: SocketAddr = format!("127.0.0.1:{}", free_loopback_port())
        .parse()
        .map_err(|e| heliosdb_nano::Error::config(format!("Invalid address: {}", e)))?;

    let server = PgServerBuilder::new()
        .address(addr)
        .auth_method(AuthMethod::Trust)
        .ssl_config(ssl_config)
        .build(db)?;

    // Start server in background
    let server_addr = server.config().address;
    tokio::spawn(async move {
        if let Err(e) = server.serve().await {
            eprintln!("Server error: {}", e);
        }
    });

    // Wait for server to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect + negotiate under a hard timeout: the test harness has no
    // per-test timeout, so an unanswered SSLRequest read must fail loudly
    // instead of wedging the whole suite (it wedged a gate run for 66 minutes
    // on 2026-07-16).
    let ssl_accepted = tokio::time::timeout(Duration::from_secs(10), async {
        let mut stream = TcpStream::connect(server_addr)
            .await
            .map_err(|e| heliosdb_nano::Error::network(format!("Connection failed: {}", e)))?;
        send_ssl_request(&mut stream).await
    })
    .await
    .expect("SSL negotiation timed out after 10s — server not answering (bind failure or foreign listener?)")?;

    // Server should reject SSL request
    assert!(!ssl_accepted, "Server should reject SSL request in Disable mode");

    Ok(())
}

#[tokio::test]
#[ignore = "Requires Rustls CryptoProvider configuration"]
async fn test_ssl_mode_require() -> Result<()> {
    let (_db, addr) = create_ssl_server(SslMode::Require, free_loopback_port()).await?;

    // Connect to server
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("Connection failed: {}", e)))?;

    // Send SSL request
    let ssl_accepted = send_ssl_request(&mut stream).await?;

    // Server should accept SSL request
    assert!(ssl_accepted, "Server should accept SSL request in Require mode");

    // Note: Full TLS handshake testing would require proper TLS client implementation
    // This test verifies the SSL negotiation phase only

    Ok(())
}

#[tokio::test]
async fn test_ssl_config_validation() {
    // Valid configuration
    let valid_config = SslConfig::new(SslMode::Disable, "certs/server.crt", "certs/server.key");
    // Validation should pass for disabled mode even if files don't exist
    assert!(valid_config.validate().is_ok());

    // Invalid certificate path (for enabled modes)
    let invalid_config = SslConfig::new(SslMode::Require, "nonexistent/cert.pem", "certs/server.key");
    assert!(invalid_config.validate().is_err());
}

#[tokio::test]
async fn test_certificate_generation() -> Result<()> {
    use tempfile::TempDir;

    let temp_dir = TempDir::new().map_err(|e| heliosdb_nano::Error::io(format!("Temp dir creation failed: {}", e)))?;

    let cert_path = temp_dir.path().join("test.crt");
    let key_path = temp_dir.path().join("test.key");

    // Generate test certificate in memory
    let (cert_pem, key_pem) = CertificateManager::generate_test_cert()?;

    // Save to files
    CertificateManager::save_cert_files(&cert_pem, &key_pem, &cert_path, &key_path)?;

    // Verify files exist
    assert!(cert_path.exists(), "Certificate file should exist");
    assert!(key_path.exists(), "Key file should exist");

    // Verify certificate files
    CertificateManager::verify_cert_files(&cert_path, &key_path)?;

    Ok(())
}

#[tokio::test]
async fn test_ssl_mode_properties() {
    assert!(!SslMode::Disable.is_enabled());
    assert!(SslMode::Allow.is_enabled());
    assert!(SslMode::Prefer.is_enabled());
    assert!(SslMode::Require.is_enabled());

    assert!(!SslMode::Disable.is_required());
    assert!(!SslMode::Allow.is_required());
    assert!(SslMode::Require.is_required());
    assert!(SslMode::VerifyCA.is_required());

    assert!(!SslMode::Require.requires_client_verification());
    assert!(SslMode::VerifyCA.requires_client_verification());
    assert!(SslMode::VerifyFull.requires_client_verification());
}

#[tokio::test]
#[ignore = "Requires Rustls CryptoProvider configuration"]
async fn test_ssl_negotiation_protocol() -> Result<()> {
    let (_db, addr) = create_ssl_server(SslMode::Allow, free_loopback_port()).await?;

    // Connect to server
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("Connection failed: {}", e)))?;

    // Manually construct SSL request message
    let mut request = Vec::new();
    request.extend_from_slice(&8i32.to_be_bytes()); // Message length
    request.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes()); // SSL request code

    // Send request
    stream
        .write_all(&request)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("Write failed: {}", e)))?;
    stream
        .flush()
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("Flush failed: {}", e)))?;

    // Read response
    let mut response = [0u8; 1];
    stream
        .read_exact(&mut response)
        .await
        .map_err(|e| heliosdb_nano::Error::network(format!("Read failed: {}", e)))?;

    // Verify response is 'S' (SSL accepted)
    assert_eq!(response[0], b'S', "Expected SSL acceptance response");

    Ok(())
}

#[tokio::test]
#[ignore = "Requires Rustls CryptoProvider configuration"]
async fn test_server_builder_with_ssl() -> Result<()> {
    CertificateManager::setup_test_certs()?;

    let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
    let addr: SocketAddr = "127.0.0.1:15436"
        .parse()
        .map_err(|e| heliosdb_nano::Error::config(format!("Invalid address: {}", e)))?;

    let ssl_config = SslConfig::new(SslMode::Prefer, "certs/server.crt", "certs/server.key");

    let server = PgServerBuilder::new().address(addr).ssl_config(ssl_config).build(db)?;

    assert!(server.config().ssl_config.is_some());
    assert_eq!(server.config().ssl_config.as_ref().unwrap().mode, SslMode::Prefer);

    Ok(())
}

#[tokio::test]
#[ignore = "Requires Rustls CryptoProvider configuration"]
async fn test_ssl_test_builder_method() -> Result<()> {
    CertificateManager::setup_test_certs()?;

    let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
    let addr: SocketAddr = "127.0.0.1:15437"
        .parse()
        .map_err(|e| heliosdb_nano::Error::config(format!("Invalid address: {}", e)))?;

    let server = PgServerBuilder::new().address(addr).ssl_test().build(db)?;

    assert!(server.config().ssl_config.is_some());
    assert_eq!(server.config().ssl_config.as_ref().unwrap().mode, SslMode::Allow);

    Ok(())
}

#[test]
fn test_ssl_request_code_constant() {
    // Verify the SSL request code matches PostgreSQL specification
    assert_eq!(SSL_REQUEST_CODE, 80877103);
}

// ============================================================================
// PQ hybrid TLS — full rustls client handshakes
//
// These build the server's `ServerConfig` from an EXPLICIT `CryptoProvider`
// (`heliosdb_nano::protocol::tls_provider::pq_capable_provider`) instead of
// the implicit process-wide default, so — unlike the negotiation-only tests
// above (several of which are `#[ignore]`d because `ServerConfig::builder()`
// needed a process-wide `CryptoProvider::install_default()` that conflicts
// across the test binary) — a real client TLS handshake can run here without
// installing anything process-wide.
// ============================================================================

mod pq_hybrid {
    use super::*;
    use heliosdb_nano::protocol::tls_provider::pq_capable_provider;
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
    use std::sync::Arc as StdArc;
    use tokio_rustls::TlsConnector;

    /// Accepts any server certificate — fine for a test harness talking to
    /// the self-signed cert `CertificateManager::setup_test_certs()` writes;
    /// this suite is testing key-exchange group negotiation, not chain
    /// validation.
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
            // Every scheme aws-lc-rs's default provider can verify — the
            // test cert is RSA or ECDSA depending on `CertificateManager`,
            // and this verifier doesn't care which.
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

    /// Build a rustls `ClientConfig` from an explicit provider (PQ-capable
    /// or classical-only, matching the server-side helper under test) that
    /// accepts any server certificate.
    fn client_config(post_quantum: bool) -> StdArc<ClientConfig> {
        let provider = pq_capable_provider(post_quantum);
        let config = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("TLS 1.3 only (PQ hybrid key exchange is TLS 1.3-only)")
            .dangerous()
            .with_custom_certificate_verifier(StdArc::new(AcceptAnyCert))
            .with_no_client_auth();
        StdArc::new(config)
    }

    /// Start a PQ-capable (or classical-only) test server and complete a
    /// real rustls TLS handshake against it as the given client config.
    /// Returns the negotiated key-exchange group name.
    async fn handshake_and_get_group(
        server_post_quantum: bool,
        client_cfg: StdArc<ClientConfig>,
    ) -> Result<rustls::NamedGroup> {
        // A per-test temp dir, NOT the shared `certs/server.{crt,key}` path
        // the negotiation-only tests above use: those never build a real
        // `rustls::ServerConfig` (SSL is either off or the test only checks
        // the 'S'/'N' byte), but these tests do a full TLS handshake, so two
        // of them regenerating the SAME shared cert+key pair in parallel
        // (`cargo test` runs tests in one binary concurrently) raced and
        // intermittently produced a mismatched cert/key pair
        // ("keys may not be consistent: KeyMismatch").
        let temp_dir =
            tempfile::TempDir::new().map_err(|e| heliosdb_nano::Error::io(format!("temp dir: {}", e)))?;
        let cert_path = temp_dir.path().join("server.crt");
        let key_path = temp_dir.path().join("server.key");
        // `generate_test_cert()`'s hardcoded fallback PEM template doesn't
        // actually parse (`generate_self_signed` shells out to the real
        // `openssl` binary, present on this dev host, and is what
        // `setup_test_certs()` uses too) — use it directly.
        CertificateManager::generate_self_signed(&cert_path, &key_path, "localhost")?;

        let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
        let ssl_config =
            SslConfig::new(SslMode::Require, &cert_path, &key_path).with_post_quantum(server_post_quantum);
        let addr: SocketAddr = format!("127.0.0.1:{}", free_loopback_port())
            .parse()
            .map_err(|e| heliosdb_nano::Error::config(format!("Invalid address: {}", e)))?;
        let server = PgServerBuilder::new()
            .address(addr)
            .auth_method(AuthMethod::Trust)
            .ssl_config(ssl_config)
            .build(db)?;
        let server_addr = server.config().address;
        tokio::spawn(async move {
            let _ = server.serve().await;
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        tokio::time::timeout(Duration::from_secs(10), async move {
            let mut stream = TcpStream::connect(server_addr)
                .await
                .map_err(|e| heliosdb_nano::Error::network(format!("Connection failed: {}", e)))?;
            let ssl_accepted = send_ssl_request(&mut stream).await?;
            assert!(ssl_accepted, "server must accept the SSLRequest");

            let connector = TlsConnector::from(client_cfg);
            let server_name = ServerName::try_from("localhost")
                .map_err(|e| heliosdb_nano::Error::network(format!("Invalid server name: {}", e)))?
                .to_owned();
            let tls_stream = connector
                .connect(server_name, stream)
                .await
                .map_err(|e| heliosdb_nano::Error::network(format!("TLS handshake failed: {}", e)))?;

            let (_, conn) = tls_stream.get_ref();
            let group = conn
                .negotiated_key_exchange_group()
                .ok_or_else(|| heliosdb_nano::Error::network("no key-exchange group negotiated".to_string()))?
                .name();
            Ok(group)
        })
        .await
        .expect("TLS handshake timed out after 10s")
    }

    #[tokio::test]
    async fn pq_only_client_negotiates_hybrid_group_against_pq_server() -> Result<()> {
        let group = handshake_and_get_group(true, client_config(true)).await?;
        assert_eq!(
            group,
            rustls::NamedGroup::X25519MLKEM768,
            "a PQ-capable client against a PQ-enabled server must negotiate the hybrid group"
        );
        Ok(())
    }

    #[tokio::test]
    async fn classical_only_client_still_succeeds_against_pq_server() -> Result<()> {
        // No regression: a client that only offers classical groups must
        // still complete a handshake against a PQ-enabled server.
        let group = handshake_and_get_group(true, client_config(false)).await?;
        assert_ne!(
            group,
            rustls::NamedGroup::X25519MLKEM768,
            "a classical-only client cannot have negotiated the PQ hybrid group"
        );
        Ok(())
    }

    #[tokio::test]
    async fn pq_disabled_server_never_offers_hybrid_group() -> Result<()> {
        // Even a PQ-capable client cannot negotiate the hybrid group when
        // the server has tls_post_quantum = false.
        let group = handshake_and_get_group(false, client_config(true)).await?;
        assert_ne!(
            group,
            rustls::NamedGroup::X25519MLKEM768,
            "tls_post_quantum = false must not offer the PQ hybrid group"
        );
        Ok(())
    }
}
