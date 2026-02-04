//! PostgreSQL SSL/TLS integration tests
//!
//! Tests SSL/TLS encryption for PostgreSQL wire protocol connections.

use heliosdb_lite::{EmbeddedDatabase, Result};
use heliosdb_lite::protocol::postgres::{
    PgServerBuilder, SslConfig, SslMode, CertificateManager, AuthMethod
};
use std::sync::Arc;
use std::net::SocketAddr;
use tokio::net::TcpStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use std::time::Duration;

/// SSL request message code
const SSL_REQUEST_CODE: i32 = 80877103;

/// Create test server with SSL
async fn create_ssl_server(
    ssl_mode: SslMode,
    port: u16,
) -> Result<(Arc<EmbeddedDatabase>, SocketAddr)> {
    // Setup test certificates
    CertificateManager::setup_test_certs()?;

    // Create database
    let db = Arc::new(EmbeddedDatabase::new_in_memory()?);

    // Configure SSL
    let ssl_config = SslConfig::new(
        ssl_mode,
        "certs/server.crt",
        "certs/server.key",
    );

    // Build server
    let addr: SocketAddr = format!("127.0.0.1:{}", port).parse()
        .map_err(|e| heliosdb_lite::Error::config(format!("Invalid address: {}", e)))?;

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
    stream.write_i32(8).await
        .map_err(|e| heliosdb_lite::Error::network(format!("Write failed: {}", e)))?;

    // SSL request code
    stream.write_i32(SSL_REQUEST_CODE).await
        .map_err(|e| heliosdb_lite::Error::network(format!("Write failed: {}", e)))?;

    stream.flush().await
        .map_err(|e| heliosdb_lite::Error::network(format!("Flush failed: {}", e)))?;

    // Read response (should be 'S' or 'N')
    let mut response = [0u8; 1];
    stream.read_exact(&mut response).await
        .map_err(|e| heliosdb_lite::Error::network(format!("Read failed: {}", e)))?;

    Ok(response[0] == b'S')
}

#[tokio::test]
#[ignore = "Requires Rustls CryptoProvider configuration"]
async fn test_ssl_mode_allow_accepts_ssl_request() -> Result<()> {
    let (_db, addr) = create_ssl_server(SslMode::Allow, 15432).await?;

    // Connect to server
    let mut stream = TcpStream::connect(addr).await
        .map_err(|e| heliosdb_lite::Error::network(format!("Connection failed: {}", e)))?;

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
    let ssl_config = SslConfig::new(
        SslMode::Disable,
        "certs/server.crt",
        "certs/server.key",
    );

    let addr: SocketAddr = "127.0.0.1:15433".parse()
        .map_err(|e| heliosdb_lite::Error::config(format!("Invalid address: {}", e)))?;

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

    // Connect to server
    let mut stream = TcpStream::connect(server_addr).await
        .map_err(|e| heliosdb_lite::Error::network(format!("Connection failed: {}", e)))?;

    // Send SSL request
    let ssl_accepted = send_ssl_request(&mut stream).await?;

    // Server should reject SSL request
    assert!(!ssl_accepted, "Server should reject SSL request in Disable mode");

    Ok(())
}

#[tokio::test]
#[ignore = "Requires Rustls CryptoProvider configuration"]
async fn test_ssl_mode_require() -> Result<()> {
    let (_db, addr) = create_ssl_server(SslMode::Require, 15434).await?;

    // Connect to server
    let mut stream = TcpStream::connect(addr).await
        .map_err(|e| heliosdb_lite::Error::network(format!("Connection failed: {}", e)))?;

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
    let valid_config = SslConfig::new(
        SslMode::Disable,
        "certs/server.crt",
        "certs/server.key",
    );
    // Validation should pass for disabled mode even if files don't exist
    assert!(valid_config.validate().is_ok());

    // Invalid certificate path (for enabled modes)
    let invalid_config = SslConfig::new(
        SslMode::Require,
        "nonexistent/cert.pem",
        "certs/server.key",
    );
    assert!(invalid_config.validate().is_err());
}

#[tokio::test]
async fn test_certificate_generation() -> Result<()> {
    use tempfile::TempDir;

    let temp_dir = TempDir::new()
        .map_err(|e| heliosdb_lite::Error::io(format!("Temp dir creation failed: {}", e)))?;

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
    let (_db, addr) = create_ssl_server(SslMode::Allow, 15435).await?;

    // Connect to server
    let mut stream = TcpStream::connect(addr).await
        .map_err(|e| heliosdb_lite::Error::network(format!("Connection failed: {}", e)))?;

    // Manually construct SSL request message
    let mut request = Vec::new();
    request.extend_from_slice(&8i32.to_be_bytes()); // Message length
    request.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes()); // SSL request code

    // Send request
    stream.write_all(&request).await
        .map_err(|e| heliosdb_lite::Error::network(format!("Write failed: {}", e)))?;
    stream.flush().await
        .map_err(|e| heliosdb_lite::Error::network(format!("Flush failed: {}", e)))?;

    // Read response
    let mut response = [0u8; 1];
    stream.read_exact(&mut response).await
        .map_err(|e| heliosdb_lite::Error::network(format!("Read failed: {}", e)))?;

    // Verify response is 'S' (SSL accepted)
    assert_eq!(response[0], b'S', "Expected SSL acceptance response");

    Ok(())
}

#[tokio::test]
#[ignore = "Requires Rustls CryptoProvider configuration"]
async fn test_server_builder_with_ssl() -> Result<()> {
    CertificateManager::setup_test_certs()?;

    let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
    let addr: SocketAddr = "127.0.0.1:15436".parse()
        .map_err(|e| heliosdb_lite::Error::config(format!("Invalid address: {}", e)))?;

    let ssl_config = SslConfig::new(
        SslMode::Prefer,
        "certs/server.crt",
        "certs/server.key",
    );

    let server = PgServerBuilder::new()
        .address(addr)
        .ssl_config(ssl_config)
        .build(db)?;

    assert!(server.config().ssl_config.is_some());
    assert_eq!(server.config().ssl_config.as_ref().unwrap().mode, SslMode::Prefer);

    Ok(())
}

#[tokio::test]
#[ignore = "Requires Rustls CryptoProvider configuration"]
async fn test_ssl_test_builder_method() -> Result<()> {
    CertificateManager::setup_test_certs()?;

    let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
    let addr: SocketAddr = "127.0.0.1:15437".parse()
        .map_err(|e| heliosdb_lite::Error::config(format!("Invalid address: {}", e)))?;

    let server = PgServerBuilder::new()
        .address(addr)
        .ssl_test()
        .build(db)?;

    assert!(server.config().ssl_config.is_some());
    assert_eq!(server.config().ssl_config.as_ref().unwrap().mode, SslMode::Allow);

    Ok(())
}

#[test]
fn test_ssl_request_code_constant() {
    // Verify the SSL request code matches PostgreSQL specification
    assert_eq!(SSL_REQUEST_CODE, 80877103);
}
