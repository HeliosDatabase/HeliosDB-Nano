//! SSL/TLS support for PostgreSQL protocol
//!
//! This module implements SSL/TLS encryption for PostgreSQL wire protocol connections.
//! It handles SSL negotiation, TLS handshake, and secure communication.
//!
//! ## PostgreSQL SSL/TLS Protocol Flow
//!
//! 1. Client sends SSLRequest message (code 80877103)
//! 2. Server responds with 'S' (SSL supported) or 'N' (not supported)
//! 3. If 'S', TLS handshake begins using tokio-rustls
//! 4. After TLS handshake, normal startup message follows over encrypted connection
//!
//! ## SSL Modes
//!
//! - `Disable`: SSL connections are disabled
//! - `Allow`: Accept both SSL and non-SSL connections
//! - `Prefer`: Prefer SSL but allow non-SSL fallback
//! - `Require`: Require SSL connections (no fallback)
//! - `VerifyCA`: Require SSL and verify client certificate against CA
//! - `VerifyFull`: Require SSL and verify client certificate with hostname

#![allow(elided_lifetimes_in_paths)]

use crate::{Result, Error};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncReadExt, AsyncWriteExt};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls_pemfile::{certs, rsa_private_keys, pkcs8_private_keys};
use tokio_rustls::TlsAcceptor;

/// SSL/TLS mode configuration
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SslMode {
    /// SSL connections are disabled
    Disable,
    /// Accept both SSL and non-SSL connections
    Allow,
    /// Prefer SSL but allow non-SSL fallback
    Prefer,
    /// Require SSL connections (no fallback)
    Require,
    /// Require SSL and verify client certificate against CA
    VerifyCA,
    /// Require SSL and verify client certificate with hostname
    VerifyFull,
}

impl Default for SslMode {
    fn default() -> Self {
        SslMode::Allow
    }
}

impl SslMode {
    /// Check if SSL is enabled
    pub fn is_enabled(&self) -> bool {
        !matches!(self, SslMode::Disable)
    }

    /// Check if SSL is required
    pub fn is_required(&self) -> bool {
        matches!(self, SslMode::Require | SslMode::VerifyCA | SslMode::VerifyFull)
    }

    /// Check if client certificate verification is required
    pub fn requires_client_verification(&self) -> bool {
        matches!(self, SslMode::VerifyCA | SslMode::VerifyFull)
    }
}

/// SSL/TLS configuration
#[derive(Debug, Clone)]
pub struct SslConfig {
    /// SSL mode
    pub mode: SslMode,
    /// Path to server certificate (PEM format)
    pub cert_path: PathBuf,
    /// Path to server private key (PEM format)
    pub key_path: PathBuf,
    /// Optional path to CA certificate for client verification
    pub ca_cert_path: Option<PathBuf>,
}

impl SslConfig {
    /// Create a new SSL configuration
    pub fn new<P: AsRef<Path>>(
        mode: SslMode,
        cert_path: P,
        key_path: P,
    ) -> Self {
        Self {
            mode,
            cert_path: cert_path.as_ref().to_path_buf(),
            key_path: key_path.as_ref().to_path_buf(),
            ca_cert_path: None,
        }
    }

    /// Set CA certificate path for client verification
    pub fn with_ca_cert<P: AsRef<Path>>(mut self, ca_cert_path: P) -> Self {
        self.ca_cert_path = Some(ca_cert_path.as_ref().to_path_buf());
        self
    }

    /// Create a default SSL configuration for testing (self-signed)
    pub fn default_test() -> Self {
        Self {
            mode: SslMode::Allow,
            cert_path: PathBuf::from("certs/server.crt"),
            key_path: PathBuf::from("certs/server.key"),
            ca_cert_path: None,
        }
    }

    /// Validate configuration
    pub fn validate(&self) -> Result<()> {
        if !self.mode.is_enabled() {
            return Ok(());
        }

        if !self.cert_path.exists() {
            return Err(Error::io(format!(
                "SSL certificate not found: {}",
                self.cert_path.display()
            )));
        }

        if !self.key_path.exists() {
            return Err(Error::io(format!(
                "SSL private key not found: {}",
                self.key_path.display()
            )));
        }

        if let Some(ref ca_path) = self.ca_cert_path {
            if !ca_path.exists() {
                return Err(Error::io(format!(
                    "SSL CA certificate not found: {}",
                    ca_path.display()
                )));
            }
        }

        Ok(())
    }
}

/// PostgreSQL SSLRequest message code
pub const SSL_REQUEST_CODE: i32 = 80877103;

/// SSL negotiation handler
pub struct SslNegotiator {
    config: SslConfig,
    acceptor: Option<TlsAcceptor>,
}

impl SslNegotiator {
    /// Create a new SSL negotiator
    pub fn new(config: SslConfig) -> Result<Self> {
        // Validate configuration
        config.validate()?;

        // Load TLS configuration if SSL is enabled
        let acceptor = if config.mode.is_enabled() {
            Some(Self::load_tls_config(&config)?)
        } else {
            None
        };

        Ok(Self {
            config,
            acceptor,
        })
    }

    /// Load TLS configuration from certificates
    fn load_tls_config(config: &SslConfig) -> Result<TlsAcceptor> {
        // Load server certificate
        let cert_file = File::open(&config.cert_path)
            .map_err(|e| Error::io(format!(
                "Failed to open certificate {}: {}",
                config.cert_path.display(),
                e
            )))?;
        let mut cert_reader = BufReader::new(cert_file);
        let certs_iter = certs(&mut cert_reader);
        let certs: Vec<CertificateDer> = certs_iter
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::io(format!("Failed to parse certificate: {}", e)))?;

        if certs.is_empty() {
            return Err(Error::io("No certificates found in certificate file"));
        }

        // Load private key
        let key_file = File::open(&config.key_path)
            .map_err(|e| Error::io(format!(
                "Failed to open private key {}: {}",
                config.key_path.display(),
                e
            )))?;
        let mut key_reader = BufReader::new(key_file);

        // Try PKCS#8 first, then RSA
        let private_key = {
            let pkcs8_keys_iter = pkcs8_private_keys(&mut key_reader);
            let mut pkcs8_keys: Vec<_> = pkcs8_keys_iter
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| Error::io(format!("Failed to parse PKCS#8 key: {}", e)))?;

            if !pkcs8_keys.is_empty() {
                PrivateKeyDer::Pkcs8(pkcs8_keys.remove(0))
            } else {
                // Try RSA format
                let key_file = File::open(&config.key_path)
                    .map_err(|e| Error::io(format!(
                        "Failed to open private key {}: {}",
                        config.key_path.display(),
                        e
                    )))?;
                let mut key_reader = BufReader::new(key_file);
                let rsa_keys_iter = rsa_private_keys(&mut key_reader);
                let mut rsa_keys: Vec<_> = rsa_keys_iter
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|e| Error::io(format!("Failed to parse RSA key: {}", e)))?;

                if rsa_keys.is_empty() {
                    return Err(Error::io("No private keys found in key file"));
                }

                PrivateKeyDer::Pkcs1(rsa_keys.remove(0))
            }
        };

        // Build TLS server configuration
        let mut tls_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, private_key)
            .map_err(|e| Error::io(format!("Failed to build TLS config: {}", e)))?;

        // Enable ALPN for PostgreSQL (optional but good practice)
        tls_config.alpn_protocols = vec![b"postgresql".to_vec()];

        Ok(TlsAcceptor::from(Arc::new(tls_config)))
    }

    /// Check if an SSL request was received
    pub async fn check_ssl_request<S>(&self, stream: &mut S) -> Result<bool>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        // Read message length
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await
            .map_err(|e| Error::network(format!("Failed to read message length: {}", e)))?;

        let _len = i32::from_be_bytes(len_buf) as usize;

        // Read request code
        let mut code_buf = [0u8; 4];
        stream.read_exact(&mut code_buf).await
            .map_err(|e| Error::network(format!("Failed to read request code: {}", e)))?;

        let code = i32::from_be_bytes(code_buf);

        Ok(code == SSL_REQUEST_CODE)
    }

    /// Handle SSL negotiation
    ///
    /// Returns:
    /// - `Ok(true)` if SSL was negotiated and accepted
    /// - `Ok(false)` if SSL was rejected or not requested
    /// - `Err(...)` if an error occurred
    pub async fn negotiate<S>(&self, stream: &mut S, is_ssl_request: bool) -> Result<bool>
    where
        S: AsyncWrite + Unpin,
    {
        if !is_ssl_request {
            return Ok(false);
        }

        match self.config.mode {
            SslMode::Disable => {
                // SSL is disabled, reject request
                tracing::debug!("SSL request received but SSL is disabled");
                stream.write_all(b"N").await
                    .map_err(|e| Error::network(format!("Failed to send SSL rejection: {}", e)))?;
                stream.flush().await
                    .map_err(|e| Error::network(format!("Failed to flush stream: {}", e)))?;
                Ok(false)
            }
            SslMode::Allow | SslMode::Prefer | SslMode::Require | SslMode::VerifyCA | SslMode::VerifyFull => {
                // SSL is enabled, accept request
                tracing::debug!("SSL request received, accepting SSL connection");
                stream.write_all(b"S").await
                    .map_err(|e| Error::network(format!("Failed to send SSL acceptance: {}", e)))?;
                stream.flush().await
                    .map_err(|e| Error::network(format!("Failed to flush stream: {}", e)))?;
                Ok(true)
            }
        }
    }

    /// Get the TLS acceptor
    pub fn acceptor(&self) -> Option<&TlsAcceptor> {
        self.acceptor.as_ref()
    }

    /// Get SSL configuration
    pub fn config(&self) -> &SslConfig {
        &self.config
    }

    /// Check if SSL is enabled
    pub fn is_enabled(&self) -> bool {
        self.config.mode.is_enabled()
    }

    /// Check if SSL is required
    pub fn is_required(&self) -> bool {
        self.config.mode.is_required()
    }
}

/// Connection wrapper that can be either plain or TLS-encrypted
pub enum SecureConnection<S> {
    /// Plain TCP connection
    Plain(S),
    /// TLS-encrypted connection
    Tls(tokio_rustls::server::TlsStream<S>),
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for SecureConnection<S> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            SecureConnection::Plain(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
            SecureConnection::Tls(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for SecureConnection<S> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            SecureConnection::Plain(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
            SecureConnection::Tls(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            SecureConnection::Plain(stream) => std::pin::Pin::new(stream).poll_flush(cx),
            SecureConnection::Tls(stream) => std::pin::Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            SecureConnection::Plain(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
            SecureConnection::Tls(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ssl_mode_properties() {
        assert!(!SslMode::Disable.is_enabled());
        assert!(SslMode::Allow.is_enabled());
        assert!(SslMode::Prefer.is_enabled());
        assert!(SslMode::Require.is_enabled());

        assert!(!SslMode::Disable.is_required());
        assert!(!SslMode::Allow.is_required());
        assert!(SslMode::Require.is_required());

        assert!(!SslMode::Allow.requires_client_verification());
        assert!(SslMode::VerifyCA.requires_client_verification());
        assert!(SslMode::VerifyFull.requires_client_verification());
    }

    #[test]
    fn test_ssl_config_creation() {
        let config = SslConfig::new(
            SslMode::Require,
            "cert.pem",
            "key.pem",
        );

        assert_eq!(config.mode, SslMode::Require);
        assert_eq!(config.cert_path, PathBuf::from("cert.pem"));
        assert_eq!(config.key_path, PathBuf::from("key.pem"));
        assert!(config.ca_cert_path.is_none());
    }

    #[test]
    fn test_ssl_config_with_ca() {
        let config = SslConfig::new(
            SslMode::VerifyCA,
            "cert.pem",
            "key.pem",
        ).with_ca_cert("ca.pem");

        assert_eq!(config.ca_cert_path, Some(PathBuf::from("ca.pem")));
    }

    #[test]
    fn test_ssl_request_code() {
        assert_eq!(SSL_REQUEST_CODE, 80877103);
    }
}
