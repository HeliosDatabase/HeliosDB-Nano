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
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
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
    /// Optional path to a CA certificate (PEM) used to verify client
    /// certificates (mutual TLS). `None` (the default) builds the acceptor
    /// with `.with_no_client_auth()`, same as before this field existed.
    pub ca_cert_path: Option<PathBuf>,
    /// Require every TLS client to present a certificate signed by
    /// `ca_cert_path` (mutual TLS). Only meaningful when `ca_cert_path` is
    /// set — `validate()` rejects `require_client_cert = true` with no CA
    /// configured. Default `false`.
    pub require_client_cert: bool,
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
            ca_cert_path: None,
            require_client_cert: false,
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

    /// Require client-certificate verification (mutual TLS) against `ca_path`.
    /// Every TLS client must present a certificate signed by this CA, or the
    /// handshake is refused by rustls itself.
    pub fn with_client_cert_verification<P: AsRef<Path>>(mut self, ca_path: P) -> Self {
        self.ca_cert_path = Some(ca_path.as_ref().to_path_buf());
        self.require_client_cert = true;
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
        if let Some(ref ca_path) = self.ca_cert_path {
            if !ca_path.exists() {
                return Err(Error::io(format!(
                    "MySQL TLS CA certificate not found: {}",
                    ca_path.display()
                )));
            }
        } else if self.require_client_cert {
            return Err(Error::io(
                "MySQL TLS require_client_cert is set but no ca_cert_path was configured",
            ));
        }
        Ok(())
    }
}

/// Loads certs/keys and builds the [`TlsAcceptor`] used to upgrade a MySQL
/// connection once `CLIENT_SSL` is observed in the client's handshake
/// response.
pub struct MysqlSslNegotiator {
    config: MysqlSslConfig,
    /// `None` iff `config.enabled` is `false` — a disabled negotiator never
    /// touches the filesystem or holds a live TLS acceptor, regardless of
    /// caller discipline. `MysqlServer` only keeps a negotiator around
    /// (`Option<Arc<MysqlSslNegotiator>>`) when TLS is actually offered, so
    /// `acceptor()` is only ever called on an enabled one.
    acceptor: Option<TlsAcceptor>,
}

impl MysqlSslNegotiator {
    /// Create a new MySQL TLS negotiator.
    ///
    /// When `config.enabled` is `false` this returns immediately without
    /// loading certificates/keys or building a TLS acceptor.
    pub fn new(config: MysqlSslConfig) -> Result<Self> {
        config.validate()?;
        if !config.enabled {
            return Ok(Self { config, acceptor: None });
        }
        let acceptor = Self::load_tls_config(&config)?;
        Ok(Self {
            config,
            acceptor: Some(acceptor),
        })
    }

    fn load_tls_config(config: &MysqlSslConfig) -> Result<TlsAcceptor> {
        let (certs, private_key) =
            crate::protocol::tls_provider::load_cert_and_key(&config.cert_path, &config.key_path, "MySQL TLS")?;

        // Explicit PQ-aware CryptoProvider — same shared helper as Postgres.
        let provider = crate::protocol::tls_provider::pq_capable_provider(config.post_quantum);
        let builder = ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .map_err(|e| Error::io(format!("Failed to select TLS protocol versions: {}", e)))?;

        let tls_config = if let Some(ca_path) = &config.ca_cert_path {
            // Mutual TLS: verify the client's certificate against this CA.
            // `WebPkiClientVerifier` refuses the handshake outright for a
            // missing or wrong-CA client certificate when `require_client_cert`
            // is set — `.allow_unauthenticated()` would accept a connection
            // with no client certificate at all, which we never want here.
            // `builder_with_provider` (not `builder`) — the plain `builder()`
            // reaches for the process-wide default `CryptoProvider`, which
            // this crate deliberately never installs (see this module's
            // doc comment on why aws-lc-rs is selected explicitly instead).
            let mut roots = RootCertStore::empty();
            let ca_certs = crate::protocol::tls_provider::load_ca_certs(ca_path, "MySQL TLS")?;
            for cert in ca_certs {
                roots
                    .add(cert)
                    .map_err(|e| Error::io(format!("Failed to add MySQL TLS CA certificate: {}", e)))?;
            }
            let roots = Arc::new(roots);
            let verifier = WebPkiClientVerifier::builder_with_provider(roots, provider)
                .build()
                .map_err(|e| Error::io(format!("Failed to build MySQL TLS client verifier: {}", e)))?;
            builder
                .with_client_cert_verifier(verifier)
                .with_single_cert(certs, private_key)
                .map_err(|e| Error::io(format!("Failed to build MySQL TLS config: {}", e)))?
        } else {
            builder
                .with_no_client_auth()
                .with_single_cert(certs, private_key)
                .map_err(|e| Error::io(format!("Failed to build MySQL TLS config: {}", e)))?
        };

        // No ALPN needed for the MySQL wire protocol.
        Ok(TlsAcceptor::from(Arc::new(tls_config)))
    }

    /// The TLS acceptor used to upgrade a connection. Panics if `config.enabled`
    /// was `false` at construction — callers must only hold onto a
    /// negotiator (e.g. as `Option<Arc<MysqlSslNegotiator>>`) when TLS is
    /// actually offered.
    pub fn acceptor(&self) -> &TlsAcceptor {
        self.acceptor
            .as_ref()
            .expect("MysqlSslNegotiator::acceptor() called on a disabled negotiator")
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
