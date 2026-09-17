//! TLS support for the MySQL wire protocol.
//!
//! Mirrors [`crate::protocol::postgres::ssl`]: certificate/key loading, an
//! explicit PQ-aware `rustls::ServerConfig` (via
//! [`crate::protocol::tls_provider::pq_capable_provider`]), and a
//! [`tokio_rustls::TlsAcceptor`] wrapped for use by
//! [`super::server::MysqlServer`].
//!
//! Unlike PostgreSQL's SSLRequest/'S'/'N' pre-startup handshake, MySQL signals
//! TLS via a capability-flags bit (`CLIENT_SSL`) inside the client's
//! `HandshakeResponse41` — negotiation is driven from `super::server`, not
//! from this module. No ALPN is needed for the MySQL wire protocol.

use crate::{Error, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use rustls_pemfile::{certs, pkcs8_private_keys, rsa_private_keys};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;

/// TLS configuration for the MySQL listener.
#[derive(Debug, Clone)]
pub struct MysqlSslConfig {
    /// Whether TLS is offered to clients at all.
    pub enabled: bool,
    /// Path to server certificate (PEM format)
    pub cert_path: PathBuf,
    /// Path to server private key (PEM format)
    pub key_path: PathBuf,
    /// Offer the `X25519MLKEM768` hybrid post-quantum key-exchange group
    /// alongside the classical groups. Default `true`.
    pub post_quantum: bool,
    /// Reject non-TLS connections outright (a future "always encrypt" mode).
    /// Default `false` — MySQL clients that don't set `CLIENT_SSL` connect
    /// in plaintext, same as a real MySQL server with `require_secure_transport`
    /// off.
    pub require_tls: bool,
}

impl MysqlSslConfig {
    /// Create a new MySQL TLS configuration (offered, not required).
    pub fn new<P: AsRef<Path>>(cert_path: P, key_path: P) -> Self {
        Self {
            enabled: true,
            cert_path: cert_path.as_ref().to_path_buf(),
            key_path: key_path.as_ref().to_path_buf(),
            post_quantum: true,
            require_tls: false,
        }
    }

    /// Enable or disable the PQ hybrid key-exchange group.
    pub fn with_post_quantum(mut self, post_quantum: bool) -> Self {
        self.post_quantum = post_quantum;
        self
    }

    /// Require TLS for every connection (reject plaintext clients).
    pub fn with_require_tls(mut self, require_tls: bool) -> Self {
        self.require_tls = require_tls;
        self
    }

    /// Validate configuration (certificate/key files exist).
    pub fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if !self.cert_path.exists() {
            return Err(Error::io(format!(
                "MySQL TLS certificate not found: {}",
                self.cert_path.display()
            )));
        }
        if !self.key_path.exists() {
            return Err(Error::io(format!(
                "MySQL TLS private key not found: {}",
                self.key_path.display()
            )));
        }
        Ok(())
    }
}

/// Loads certs/keys and builds the [`TlsAcceptor`] used to upgrade a MySQL
/// connection once `CLIENT_SSL` is observed in the client's handshake
/// response.
pub struct MysqlSslNegotiator {
    config: MysqlSslConfig,
    acceptor: TlsAcceptor,
}

impl MysqlSslNegotiator {
    /// Create a new MySQL TLS negotiator. Fails if `config.enabled` is false
    /// — callers should only construct this when TLS is actually configured.
    pub fn new(config: MysqlSslConfig) -> Result<Self> {
        config.validate()?;
        let acceptor = Self::load_tls_config(&config)?;
        Ok(Self { config, acceptor })
    }

    fn load_tls_config(config: &MysqlSslConfig) -> Result<TlsAcceptor> {
        // Load server certificate
        let cert_file = File::open(&config.cert_path).map_err(|e| {
            Error::io(format!(
                "Failed to open MySQL TLS certificate {}: {}",
                config.cert_path.display(),
                e
            ))
        })?;
        let mut cert_reader = BufReader::new(cert_file);
        let certs_iter = certs(&mut cert_reader);
        let certs: Vec<CertificateDer<'_>> = certs_iter
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::io(format!("Failed to parse MySQL TLS certificate: {}", e)))?;

        if certs.is_empty() {
            return Err(Error::io("No certificates found in MySQL TLS certificate file"));
        }

        // Load private key — PKCS#8 first, then RSA (same fallback order as
        // the PostgreSQL loader).
        let key_file = File::open(&config.key_path).map_err(|e| {
            Error::io(format!(
                "Failed to open MySQL TLS private key {}: {}",
                config.key_path.display(),
                e
            ))
        })?;
        let mut key_reader = BufReader::new(key_file);

        let private_key = {
            let pkcs8_keys_iter = pkcs8_private_keys(&mut key_reader);
            let mut pkcs8_keys: Vec<_> = pkcs8_keys_iter
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| Error::io(format!("Failed to parse PKCS#8 key: {}", e)))?;

            if !pkcs8_keys.is_empty() {
                PrivateKeyDer::Pkcs8(pkcs8_keys.remove(0))
            } else {
                let key_file = File::open(&config.key_path).map_err(|e| {
                    Error::io(format!(
                        "Failed to open MySQL TLS private key {}: {}",
                        config.key_path.display(),
                        e
                    ))
                })?;
                let mut key_reader = BufReader::new(key_file);
                let rsa_keys_iter = rsa_private_keys(&mut key_reader);
                let mut rsa_keys: Vec<_> = rsa_keys_iter
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|e| Error::io(format!("Failed to parse RSA key: {}", e)))?;

                if rsa_keys.is_empty() {
                    return Err(Error::io("No private keys found in MySQL TLS key file"));
                }

                PrivateKeyDer::Pkcs1(rsa_keys.remove(0))
            }
        };

        // Explicit PQ-aware CryptoProvider — same shared helper as Postgres.
        let provider = crate::protocol::tls_provider::pq_capable_provider(config.post_quantum);
        let tls_config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| Error::io(format!("Failed to select TLS protocol versions: {}", e)))?
            .with_no_client_auth()
            .with_single_cert(certs, private_key)
            .map_err(|e| Error::io(format!("Failed to build MySQL TLS config: {}", e)))?;

        // No ALPN needed for the MySQL wire protocol.
        Ok(TlsAcceptor::from(Arc::new(tls_config)))
    }

    /// The TLS acceptor used to upgrade a connection.
    pub fn acceptor(&self) -> &TlsAcceptor {
        &self.acceptor
    }

    /// The configuration this negotiator was built from.
    pub fn config(&self) -> &MysqlSslConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mysql_ssl_config_defaults() {
        let config = MysqlSslConfig::new("cert.pem", "key.pem");
        assert!(config.enabled);
        assert!(config.post_quantum);
        assert!(!config.require_tls);
    }

    #[test]
    fn test_mysql_ssl_config_builders() {
        let config = MysqlSslConfig::new("cert.pem", "key.pem")
            .with_post_quantum(false)
            .with_require_tls(true);
        assert!(!config.post_quantum);
        assert!(config.require_tls);
    }
}
