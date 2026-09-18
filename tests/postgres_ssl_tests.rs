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
        let temp_dir = tempfile::TempDir::new().map_err(|e| heliosdb_nano::Error::io(format!("temp dir: {}", e)))?;
        let cert_path = temp_dir.path().join("server.crt");
        let key_path = temp_dir.path().join("server.key");
        // `generate_test_cert()`'s hardcoded fallback PEM template doesn't
        // actually parse (`generate_self_signed` shells out to the real
        // `openssl` binary, present on this dev host, and is what
        // `setup_test_certs()` uses too) — use it directly.
        CertificateManager::generate_self_signed(&cert_path, &key_path, "localhost")?;

        let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
        let ssl_config = SslConfig::new(SslMode::Require, &cert_path, &key_path).with_post_quantum(server_post_quantum);
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

    // ------------------------------------------------------------------
    // Item 1: `SHOW ssl_key_exchange` must agree with what the client
    // actually negotiated.
    // ------------------------------------------------------------------

    /// Build a minimal PostgreSQL `StartupMessage` (protocol 3.0, `user`
    /// param only — trust auth expects nothing further. With no explicit
    /// `database` param the engine defaults it to the `user` name and
    /// validates THAT against the catalog (`docs/guides/authentication.md`),
    /// so `user` must be an existing database — `"postgres"` always is.
    fn build_startup_message(user: &str) -> Vec<u8> {
        let mut params = Vec::new();
        params.extend_from_slice(b"user\0");
        params.extend_from_slice(user.as_bytes());
        params.push(0);
        params.push(0); // terminator

        let mut msg = Vec::new();
        msg.extend_from_slice(&(196_608i32).to_be_bytes()); // protocol 3.0
        msg.extend_from_slice(&params);

        let mut framed = Vec::with_capacity(4 + msg.len());
        framed.extend_from_slice(&((msg.len() + 4) as i32).to_be_bytes());
        framed.extend_from_slice(&msg);
        framed
    }

    /// Read one tagged backend message: `(tag, payload)`.
    async fn read_message<S: tokio::io::AsyncRead + Unpin>(stream: &mut S) -> Result<(u8, Vec<u8>)> {
        use tokio::io::AsyncReadExt as _;
        let mut tag = [0u8; 1];
        stream
            .read_exact(&mut tag)
            .await
            .map_err(|e| heliosdb_nano::Error::network(format!("tag read failed: {}", e)))?;
        let mut len_buf = [0u8; 4];
        stream
            .read_exact(&mut len_buf)
            .await
            .map_err(|e| heliosdb_nano::Error::network(format!("length read failed: {}", e)))?;
        let len = i32::from_be_bytes(len_buf) as usize;
        let mut payload = vec![0u8; len - 4];
        stream
            .read_exact(&mut payload)
            .await
            .map_err(|e| heliosdb_nano::Error::network(format!("payload read failed: {}", e)))?;
        Ok((tag[0], payload))
    }

    /// Complete a PQ (or classical) TLS handshake, run a trust-auth startup,
    /// issue `SHOW ssl_key_exchange`, and return `(negotiated group, SQL
    /// value)`.
    async fn handshake_and_query_kx_group(
        server_post_quantum: bool,
        client_cfg: StdArc<ClientConfig>,
    ) -> Result<(rustls::NamedGroup, String)> {
        let _ = tracing_subscriber::fmt().with_env_filter("debug").try_init();
        let temp_dir = tempfile::TempDir::new().map_err(|e| heliosdb_nano::Error::io(format!("temp dir: {}", e)))?;
        let cert_path = temp_dir.path().join("server.crt");
        let key_path = temp_dir.path().join("server.key");
        CertificateManager::generate_self_signed(&cert_path, &key_path, "localhost")?;

        let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
        let ssl_config = SslConfig::new(SslMode::Require, &cert_path, &key_path).with_post_quantum(server_post_quantum);
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

            // StartupMessage -> trust auth -> ReadyForQuery.
            tls_stream
                .write_all(&build_startup_message("postgres"))
                .await
                .map_err(|e| heliosdb_nano::Error::network(format!("startup write failed: {}", e)))?;
            tls_stream
                .flush()
                .await
                .map_err(|e| heliosdb_nano::Error::network(format!("startup flush failed: {}", e)))?;
            loop {
                let (tag, _payload) = read_message(&mut tls_stream).await?;
                if tag == b'Z' {
                    break; // ReadyForQuery
                }
            }

            // Simple Query: SHOW ssl_key_exchange.
            let sql = "SHOW ssl_key_exchange";
            let mut q = Vec::new();
            q.push(b'Q');
            let body_len = sql.len() + 1 + 4;
            q.extend_from_slice(&(body_len as i32).to_be_bytes());
            q.extend_from_slice(sql.as_bytes());
            q.push(0);
            tls_stream
                .write_all(&q)
                .await
                .map_err(|e| heliosdb_nano::Error::network(format!("query write failed: {}", e)))?;
            tls_stream
                .flush()
                .await
                .map_err(|e| heliosdb_nano::Error::network(format!("query flush failed: {}", e)))?;

            let mut value = String::new();
            loop {
                let (tag, payload) = read_message(&mut tls_stream).await?;
                match tag {
                    b'D' => {
                        // DataRow: int16 field count, then per field int32
                        // length + bytes. One column, non-NULL.
                        let field_len = i32::from_be_bytes([payload[2], payload[3], payload[4], payload[5]]);
                        if field_len >= 0 {
                            let start = 6;
                            let end = start + field_len as usize;
                            value = String::from_utf8_lossy(&payload[start..end]).into_owned();
                        }
                    }
                    b'Z' => break, // ReadyForQuery: query round-trip done
                    _ => {}
                }
            }

            Ok((group, value))
        })
        .await
        .expect("TLS handshake + query timed out after 10s")
    }

    #[tokio::test]
    async fn pq_client_ssl_key_exchange_matches_negotiated_group() -> Result<()> {
        let (group, reported) = handshake_and_query_kx_group(true, client_config(true)).await?;
        assert_eq!(group, rustls::NamedGroup::X25519MLKEM768);
        assert_eq!(
            reported, "X25519MLKEM768",
            "SHOW ssl_key_exchange must report the group the client actually negotiated"
        );
        Ok(())
    }

    #[tokio::test]
    async fn classical_client_ssl_key_exchange_matches_negotiated_group() -> Result<()> {
        let (group, reported) = handshake_and_query_kx_group(true, client_config(false)).await?;
        assert_ne!(group, rustls::NamedGroup::X25519MLKEM768);
        assert_eq!(
            reported,
            format!("{:?}", group),
            "SHOW ssl_key_exchange must report the group the client actually negotiated"
        );
        Ok(())
    }
}

// ============================================================================
// Mutual TLS: `ssl_mode = verify-ca` / `verify-full` must actually verify the
// CLIENT certificate (sprinter 0fd0449e8466).
// ============================================================================
//
// `SslNegotiator::load_tls_config` used to call `.with_no_client_auth()` for
// EVERY `SslMode`, so an operator who configured `verify-ca` (and a
// `ca_cert_path`) got a listener that accepted any client certificate or none
// at all — mutual TLS silently not enforced, no error, no log line. The wiring
// is ported from the working reference on this tree,
// `src/protocol/mysql/ssl.rs`, and these tests mirror
// `tests/mysql_ssl_tests.rs`'s `mtls_*` cases (including its
// `generate_ca_and_signed_cert` helper) against the PostgreSQL listener.

mod mtls {
    use super::*;
    use heliosdb_nano::protocol::tls_provider::pq_capable_provider;
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
    use std::sync::Arc as StdArc;
    use tokio_rustls::TlsConnector;

    /// Accepts any SERVER certificate — these tests are about the CLIENT
    /// certificate the server does or does not demand, not about chain
    /// validation of the self-signed test server cert.
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

    /// Generate a self-signed CA and a leaf certificate signed by it, via the
    /// `openssl` CLI — the same tool `CertificateManager::generate_self_signed`
    /// shells out to. Copied from `tests/mysql_ssl_tests.rs`
    /// (`generate_ca_and_signed_cert`), including the X.509v3 `-extfile`
    /// workaround: `openssl x509 -req` without it emits a v1 certificate on
    /// some OpenSSL builds, which rustls's webpki verifier refuses with
    /// `UnsupportedCertVersion`.
    ///
    /// Returns `(ca_cert, ca_key, leaf_cert, leaf_key)`, all inside `dir` and
    /// namespaced by `leaf_cn` so two calls into the same directory do not
    /// overwrite each other's CA.
    fn generate_ca_and_signed_cert(
        dir: &std::path::Path,
        leaf_cn: &str,
    ) -> Result<(
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
    )> {
        let run = |args: &[&str]| -> Result<()> {
            let output = std::process::Command::new("openssl")
                .args(args)
                .output()
                .map_err(|e| heliosdb_nano::Error::io(format!("failed to execute openssl: {}", e)))?;
            if !output.status.success() {
                return Err(heliosdb_nano::Error::io(format!(
                    "openssl {:?} failed: {}",
                    args,
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            Ok(())
        };

        let ca_key = dir.join(format!("{leaf_cn}-ca.key"));
        let ca_cert = dir.join(format!("{leaf_cn}-ca.crt"));
        let leaf_key = dir.join(format!("{leaf_cn}.key"));
        let leaf_csr = dir.join(format!("{leaf_cn}.csr"));
        let leaf_cert = dir.join(format!("{leaf_cn}.crt"));

        run(&[
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            ca_key.to_str().expect("path"),
            "-out",
            ca_cert.to_str().expect("path"),
            "-days",
            "365",
            "-subj",
            "/CN=Test CA",
        ])?;

        run(&[
            "req",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            leaf_key.to_str().expect("path"),
            "-out",
            leaf_csr.to_str().expect("path"),
            "-subj",
            &format!("/CN={leaf_cn}"),
        ])?;

        let extfile = dir.join(format!("{leaf_cn}.ext"));
        std::fs::write(
            &extfile,
            "basicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=clientAuth\n",
        )
        .map_err(|e| heliosdb_nano::Error::io(format!("failed to write {extfile:?}: {e}")))?;
        run(&[
            "x509",
            "-req",
            "-in",
            leaf_csr.to_str().expect("path"),
            "-CA",
            ca_cert.to_str().expect("path"),
            "-CAkey",
            ca_key.to_str().expect("path"),
            "-CAcreateserial",
            "-out",
            leaf_cert.to_str().expect("path"),
            "-days",
            "365",
            "-extfile",
            extfile.to_str().expect("path"),
        ])?;

        Ok((ca_cert, ca_key, leaf_cert, leaf_key))
    }

    /// A client that verifies nothing about the server and presents NO
    /// certificate of its own.
    fn client_without_certificate() -> StdArc<ClientConfig> {
        let provider = pq_capable_provider(true);
        StdArc::new(
            ClientConfig::builder_with_provider(provider)
                .with_protocol_versions(&[&rustls::version::TLS13])
                .expect("TLS 1.3")
                .dangerous()
                .with_custom_certificate_verifier(StdArc::new(AcceptAnyCert))
                .with_no_client_auth(),
        )
    }

    /// A client that presents `cert`/`key` for mutual TLS.
    fn client_with_certificate(cert: &std::path::Path, key: &std::path::Path) -> Result<StdArc<ClientConfig>> {
        use rustls_pemfile::{certs, pkcs8_private_keys};
        use std::fs::File;
        use std::io::BufReader;

        let chain: Vec<_> =
            certs(&mut BufReader::new(File::open(cert).map_err(|e| {
                heliosdb_nano::Error::io(format!("open client cert: {}", e))
            })?))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| heliosdb_nano::Error::io(format!("parse client cert: {}", e)))?;
        let mut keys: Vec<_> = pkcs8_private_keys(&mut BufReader::new(
            File::open(key).map_err(|e| heliosdb_nano::Error::io(format!("open client key: {}", e)))?,
        ))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| heliosdb_nano::Error::io(format!("parse client key: {}", e)))?;
        let private_key = rustls::pki_types::PrivateKeyDer::Pkcs8(keys.remove(0));

        let provider = pq_capable_provider(true);
        let config = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("TLS 1.3")
            .dangerous()
            .with_custom_certificate_verifier(StdArc::new(AcceptAnyCert))
            .with_client_auth_cert(chain, private_key)
            .map_err(|e| heliosdb_nano::Error::network(format!("client auth cert config: {}", e)))?;
        Ok(StdArc::new(config))
    }

    /// Minimal protocol-3.0 `StartupMessage` with only a `user` parameter —
    /// the database name defaults to the user, so `postgres` is used (it always
    /// exists). Same shape as `pq_hybrid::build_startup_message`.
    fn build_startup_message(user: &str) -> Vec<u8> {
        let mut params = Vec::new();
        params.extend_from_slice(b"user\0");
        params.extend_from_slice(user.as_bytes());
        params.push(0);
        params.push(0);

        let mut msg = Vec::new();
        msg.extend_from_slice(&(196_608i32).to_be_bytes());
        msg.extend_from_slice(&params);

        let mut framed = Vec::with_capacity(4 + msg.len());
        framed.extend_from_slice(&((msg.len() + 4) as i32).to_be_bytes());
        framed.extend_from_slice(&msg);
        framed
    }

    /// Start a PostgreSQL listener in `verify-ca` mode trusting `ca_cert`.
    /// Returns its bound address; the server's own certificate is a fresh
    /// self-signed pair in `dir` (per-test, never the shared `certs/server.*`
    /// pair — two tests regenerating that in parallel race, see
    /// `pq_hybrid::handshake_and_get_group`).
    async fn start_verify_ca_server(dir: &std::path::Path, ca_cert: &std::path::Path) -> Result<SocketAddr> {
        let cert_path = dir.join("server.crt");
        let key_path = dir.join("server.key");
        CertificateManager::generate_self_signed(&cert_path, &key_path, "localhost")?;

        let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
        let ssl_config = SslConfig::new(SslMode::VerifyCA, &cert_path, &key_path).with_ca_cert(ca_cert);
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
        Ok(server_addr)
    }

    /// SSLRequest → TLS handshake → trust-auth startup → ReadyForQuery.
    ///
    /// The startup round-trip is load-bearing for the NEGATIVE case: under
    /// TLS 1.3 the server sends its `Finished` before it has seen the client's
    /// `Certificate`, so `connect()` itself can return `Ok` even when the
    /// server is about to refuse. The rejection surfaces on the first read
    /// after that.
    async fn login_over_tls(addr: SocketAddr, client_cfg: StdArc<ClientConfig>) -> Result<()> {
        let mut stream = TcpStream::connect(addr)
            .await
            .map_err(|e| heliosdb_nano::Error::network(format!("Connection failed: {}", e)))?;
        let ssl_accepted = send_ssl_request(&mut stream).await?;
        assert!(
            ssl_accepted,
            "a verify-ca server must still answer the SSLRequest with 'S'"
        );

        let connector = TlsConnector::from(client_cfg);
        let server_name = ServerName::try_from("localhost")
            .map_err(|e| heliosdb_nano::Error::network(format!("Invalid server name: {}", e)))?
            .to_owned();
        let mut tls_stream = connector
            .connect(server_name, stream)
            .await
            .map_err(|e| heliosdb_nano::Error::network(format!("TLS handshake failed: {}", e)))?;

        tls_stream
            .write_all(&build_startup_message("postgres"))
            .await
            .map_err(|e| heliosdb_nano::Error::network(format!("startup write failed: {}", e)))?;
        tls_stream
            .flush()
            .await
            .map_err(|e| heliosdb_nano::Error::network(format!("startup flush failed: {}", e)))?;

        loop {
            let mut tag = [0u8; 1];
            tls_stream
                .read_exact(&mut tag)
                .await
                .map_err(|e| heliosdb_nano::Error::network(format!("tag read failed: {}", e)))?;
            let mut len_buf = [0u8; 4];
            tls_stream
                .read_exact(&mut len_buf)
                .await
                .map_err(|e| heliosdb_nano::Error::network(format!("length read failed: {}", e)))?;
            let len = i32::from_be_bytes(len_buf) as usize;
            let mut payload = vec![0u8; len.saturating_sub(4)];
            tls_stream
                .read_exact(&mut payload)
                .await
                .map_err(|e| heliosdb_nano::Error::network(format!("payload read failed: {}", e)))?;
            if tag[0] == b'Z' {
                return Ok(()); // ReadyForQuery
            }
            if tag[0] == b'E' {
                return Err(heliosdb_nano::Error::network(
                    "server returned ErrorResponse".to_string(),
                ));
            }
        }
    }

    #[tokio::test]
    async fn postgres_verify_ca_accepts_a_client_with_a_ca_signed_certificate() -> Result<()> {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let (ca_cert, _ca_key, client_cert, client_key) = generate_ca_and_signed_cert(temp_dir.path(), "pg-client")?;
        let addr = start_verify_ca_server(temp_dir.path(), &ca_cert).await?;

        let client = client_with_certificate(&client_cert, &client_key)?;
        tokio::time::timeout(Duration::from_secs(10), login_over_tls(addr, client))
            .await
            .expect("mTLS login timed out")
    }

    #[tokio::test]
    async fn postgres_verify_ca_rejects_a_client_with_no_certificate() -> Result<()> {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let (ca_cert, _ca_key, _client_cert, _client_key) = generate_ca_and_signed_cert(temp_dir.path(), "unused")?;
        let addr = start_verify_ca_server(temp_dir.path(), &ca_cert).await?;

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            login_over_tls(addr, client_without_certificate()),
        )
        .await;
        match result {
            Ok(Ok(())) => panic!(
                "verify-ca accepted a client that presented NO certificate — \
                 mutual TLS is not being enforced (sprinter 0fd0449e8466)"
            ),
            Ok(Err(_)) | Err(_) => Ok(()), // handshake/IO failure or timeout: both are a rejection
        }
    }

    #[tokio::test]
    async fn postgres_verify_ca_rejects_a_certificate_from_another_ca() -> Result<()> {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let (ca_cert, _ca_key, _client_cert, _client_key) =
            generate_ca_and_signed_cert(temp_dir.path(), "pg-server-ca")?;
        // A second, unrelated CA signs this client's certificate.
        let (_other_ca, _other_key, wrong_cert, wrong_key) =
            generate_ca_and_signed_cert(temp_dir.path(), "pg-wrong-ca-client")?;
        let addr = start_verify_ca_server(temp_dir.path(), &ca_cert).await?;

        let client = client_with_certificate(&wrong_cert, &wrong_key)?;
        let result = tokio::time::timeout(Duration::from_secs(10), login_over_tls(addr, client)).await;
        match result {
            Ok(Ok(())) => panic!("a certificate signed by an untrusted CA must not be accepted"),
            Ok(Err(_)) | Err(_) => Ok(()),
        }
    }

    /// Fail closed: `verify-ca` with no `ca_cert_path` must be REFUSED at
    /// configuration time rather than silently degrading to "no client
    /// authentication" — which is precisely the bug.
    #[tokio::test]
    async fn verify_ca_without_a_configured_ca_is_refused() -> Result<()> {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let cert_path = temp_dir.path().join("server.crt");
        let key_path = temp_dir.path().join("server.key");
        CertificateManager::generate_self_signed(&cert_path, &key_path, "localhost")?;

        let err = SslConfig::new(SslMode::VerifyCA, &cert_path, &key_path)
            .validate()
            .expect_err("verify-ca with no CA must not validate");
        assert!(err.to_string().contains("ca_cert_path"), "got: {err}");
        Ok(())
    }
}
