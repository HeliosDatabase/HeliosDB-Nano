//! TLS on the replication port (spec P2.2,
//! `docs/plans/specs/P2_2_tls_remote_resumable.md`).
//!
//! # Why
//!
//! Through P2.1 the one replication port (now `NPHYS004`) was plaintext and
//! therefore loopback-only: its bearer token would otherwise cross the network
//! in the clear. Every real deployment needs a standby on another host. The
//! framing is already generic over `AsyncRead + AsyncWrite`
//! (`physical_wire::serve_session`, `PhysicalClient`), so TLS is a stream
//! wrapper here, not a protocol change.
//!
//! # Rules
//!
//! * **Transport is not authentication.** The bearer token is checked in
//!   every mode; with mutual TLS (a primary `tls_ca_path`) the standby's
//!   certificate is verified first and the token is still required.
//! * **A non-loopback peer needs TLS, or the explicit plaintext opt-in**
//!   ([`remote_transport_allowed`]): a primary binds a non-loopback address
//!   only with `tls_enabled` or `allow_insecure_remote`, and a standby
//!   connects to one only with `primary_tls` or `allow_insecure_remote`.
//! * **A TLS failure is a transport failure.** A wrong CA, an untrusted
//!   certificate or a name mismatch says nothing about the data: the standby
//!   reconnects with backoff (posture `reconnecting`, reason `tls: …`) and
//!   never enters `needs_recovery` for it. A primary logs and counts every
//!   failed handshake ([`handshake_failures`]); no `Refuse` frame can be sent
//!   before a handshake.
//! * The certificate files of a standby are read again at every connection
//!   attempt, so a rotated CA or client certificate is picked up without a
//!   restart. A primary loads its own once, at start.
//!
//! Everything is built on [`crate::protocol::tls_provider`]'s explicit
//! `aws-lc-rs` provider (post-quantum `X25519MLKEM768` on by default), as the
//! PostgreSQL and MySQL listeners are; no process-wide default provider is
//! installed or relied on.

use crate::protocol::tls_provider::{load_ca_certs, load_cert_and_key, pq_capable_provider};
use crate::{Error, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, ServerConfig, SignatureScheme};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::{TlsAcceptor, TlsConnector};

/// The ALPN protocol both sides of a replication session offer.
pub const ALPN_PROTOCOL: &[u8] = b"nphys002";
/// Error-message label of the replication port's TLS files.
const LABEL: &str = "replication TLS";

/// Where a system trust store usually is, when a standby names no CA
/// (`SSL_CERT_FILE` first, then the distributions' bundles).
const SYSTEM_BUNDLES: [&str; 7] = [
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
    "/etc/ssl/ca-bundle.pem",
    "/etc/ssl/cert.pem",
    "/usr/local/etc/openssl/cert.pem",
    "/usr/local/share/certs/ca-root-nss.crt",
];

static HANDSHAKE_FAILURES: AtomicU64 = AtomicU64::new(0);

/// Count one failed TLS handshake on the replication port; returns the count.
pub fn note_handshake_failure() -> u64 {
    HANDSHAKE_FAILURES.fetch_add(1, Ordering::Relaxed).saturating_add(1)
}

/// How many TLS handshakes this process's replication port refused.
pub fn handshake_failures() -> u64 {
    HANDSHAKE_FAILURES.load(Ordering::Relaxed)
}

/// Spec P2.2: may a replication connection to or from `address` run over the
/// given transport? Loopback always may; any other address (a wildcard bind
/// included) needs TLS, or the explicit plaintext opt-in.
pub fn remote_transport_allowed(address: &SocketAddr, tls: bool, allow_insecure_remote: bool) -> bool {
    address.ip().is_loopback() || tls || allow_insecure_remote
}

/// The host part of a `host:port` / `[IPv6]:port` endpoint: the default name
/// a standby verifies its primary's certificate against.
pub fn endpoint_host(endpoint: &str) -> &str {
    if let Some(rest) = endpoint.strip_prefix('[') {
        if let Some((host, _)) = rest.split_once(']') {
            return host;
        }
    }
    endpoint.rsplit_once(':').map_or(endpoint, |(host, _)| host)
}

fn tls_error(what: &str, error: impl std::fmt::Display) -> Error {
    Error::config(format!("{LABEL}: {what}: {error}"))
}

fn root_store(ca_path: &Path) -> Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    for certificate in load_ca_certs(ca_path, LABEL)? {
        roots
            .add(certificate)
            .map_err(|error| tls_error(&format!("CA certificate {}", ca_path.display()), error))?;
    }
    Ok(roots)
}

/// The system trust store, from `SSL_CERT_FILE` or the first readable
/// distribution bundle. Certificates the verifier cannot parse are skipped.
fn system_roots() -> Result<RootCertStore> {
    let mut candidates: Vec<PathBuf> = std::env::var_os("SSL_CERT_FILE")
        .map(PathBuf::from)
        .into_iter()
        .collect();
    candidates.extend(SYSTEM_BUNDLES.iter().map(PathBuf::from));
    for candidate in candidates {
        if !candidate.is_file() {
            continue;
        }
        let Ok(certificates) = load_ca_certs(&candidate, LABEL) else {
            continue;
        };
        let mut roots = RootCertStore::empty();
        let (added, _ignored) = roots.add_parsable_certificates(certificates);
        if added > 0 {
            return Ok(roots);
        }
    }
    Err(Error::config(
        "replication TLS: no system trust store was found (SSL_CERT_FILE or a distribution CA bundle); name the \
         primary's CA with --primary-ca ([replication] primary_tls_ca_path)",
    ))
}

/// A primary's replication-port TLS: its certificate, and the CA standbys'
/// client certificates must chain to (mutual TLS) when one is set.
#[derive(Clone, Debug)]
pub struct ReplicationServerTls {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    /// When set, every standby must present a certificate issued by it.
    pub ca_path: Option<PathBuf>,
    pub post_quantum: bool,
}

impl ReplicationServerTls {
    /// Whether standbys must present a client certificate.
    pub fn mutual(&self) -> bool {
        self.ca_path.is_some()
    }

    /// Load the files and build the acceptor. Fails closed: an unreadable
    /// certificate, key or CA is an error, never a plaintext fallback, and a
    /// mutual-TLS port refuses a standby without a certificate.
    pub fn acceptor(&self) -> Result<TlsAcceptor> {
        let (certificates, key) = load_cert_and_key(&self.cert_path, &self.key_path, LABEL)?;
        let provider = pq_capable_provider(self.post_quantum);
        let builder = ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .map_err(|error| tls_error("protocol versions", error))?;
        let mut config = match &self.ca_path {
            Some(ca_path) => {
                // `builder_with_provider`: this crate never installs a
                // process-wide provider (see `tls_provider`). No
                // `allow_unauthenticated`: a standby without a certificate is
                // refused by rustls itself.
                let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(root_store(ca_path)?), provider)
                    .build()
                    .map_err(|error| tls_error("client certificate verifier", error))?;
                builder
                    .with_client_cert_verifier(verifier)
                    .with_single_cert(certificates, key)
                    .map_err(|error| tls_error("certificate", error))?
            }
            None => builder
                .with_no_client_auth()
                .with_single_cert(certificates, key)
                .map_err(|error| tls_error("certificate", error))?,
        };
        config.alpn_protocols = vec![ALPN_PROTOCOL.to_vec()];
        Ok(TlsAcceptor::from(Arc::new(config)))
    }
}

/// A standby's TLS to its primary: what the primary's certificate is checked
/// against, and the client certificate a mutual-TLS primary asks for.
#[derive(Clone, Debug)]
pub struct ReplicationClientTls {
    /// The primary's CA; `None`: the system trust store.
    ca_path: Option<PathBuf>,
    server_name: ServerName<'static>,
    client_identity: Option<(PathBuf, PathBuf)>,
    insecure_skip_verify: bool,
    post_quantum: bool,
}

impl ReplicationClientTls {
    /// `server_name` is a DNS name or an IP literal (verified as an IP
    /// subject alternative name). `insecure_skip_verify` accepts any
    /// certificate; the caller honours it only with the plaintext opt-in.
    pub fn new(
        server_name: &str,
        ca_path: Option<PathBuf>,
        client_identity: Option<(PathBuf, PathBuf)>,
        insecure_skip_verify: bool,
        post_quantum: bool,
    ) -> Result<Self> {
        let server_name = ServerName::try_from(server_name.trim().to_string()).map_err(|_| {
            Error::config(format!(
                "replication TLS: '{server_name}' is neither a DNS name nor an IP address; set \
                 --primary-server-name"
            ))
        })?;
        Ok(Self {
            ca_path,
            server_name,
            client_identity,
            insecure_skip_verify,
            post_quantum,
        })
    }

    /// The name the primary's certificate must carry.
    pub fn server_name(&self) -> ServerName<'static> {
        self.server_name.clone()
    }

    pub fn insecure_skip_verify(&self) -> bool {
        self.insecure_skip_verify
    }

    pub fn presents_client_certificate(&self) -> bool {
        self.client_identity.is_some()
    }

    /// Read the files (again: a rotated CA or client certificate takes effect
    /// at the next connection) and build the connector.
    pub fn connector(&self) -> Result<TlsConnector> {
        let provider = pq_capable_provider(self.post_quantum);
        let builder = ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .map_err(|error| tls_error("protocol versions", error))?;
        let builder = if self.insecure_skip_verify {
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AnyServerCertificate { provider }))
        } else {
            let roots = match &self.ca_path {
                Some(ca_path) => root_store(ca_path)?,
                None => system_roots()?,
            };
            builder.with_root_certificates(roots)
        };
        let mut config = match &self.client_identity {
            Some((cert_path, key_path)) => {
                let (certificates, key) = load_cert_and_key(cert_path, key_path, LABEL)?;
                builder
                    .with_client_auth_cert(certificates, key)
                    .map_err(|error| tls_error("client certificate", error))?
            }
            None => builder.with_no_client_auth(),
        };
        config.alpn_protocols = vec![ALPN_PROTOCOL.to_vec()];
        Ok(TlsConnector::from(Arc::new(config)))
    }
}

/// How a receiver reaches its source: TLS (or not), and whether plaintext may
/// cross a non-loopback network ([`remote_transport_allowed`]).
#[derive(Clone, Debug, Default)]
pub struct ReplicationTransport {
    pub tls: Option<ReplicationClientTls>,
    pub allow_insecure_remote: bool,
}

impl ReplicationTransport {
    /// Whether this transport may reach `address`.
    pub fn allows(&self, address: &SocketAddr) -> bool {
        remote_transport_allowed(address, self.tls.is_some(), self.allow_insecure_remote)
    }
}

/// `--primary-tls-insecure-skip-verify`: no chain or name check. The
/// handshake signature is still verified against the presented certificate,
/// so the session is encrypted to whoever holds that certificate's key.
#[derive(Debug)]
struct AnyServerCertificate {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for AnyServerCertificate {
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
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// One replication connection: plaintext, or TLS on either side.
pub enum ReplicationStream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::TlsStream<TcpStream>>),
}

impl ReplicationStream {
    pub fn is_tls(&self) -> bool {
        matches!(self, Self::Tls(_))
    }
}

impl std::fmt::Debug for ReplicationStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.is_tls() {
            "ReplicationStream::Tls"
        } else {
            "ReplicationStream::Plain"
        })
    }
}

impl AsyncRead for ReplicationStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for ReplicationStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Spec P2.2: the rule on a non-loopback address. Loopback needs nothing;
    /// anything else, the unspecified address included, needs TLS or the
    /// plaintext opt-in.
    #[test]
    fn a_non_loopback_peer_needs_tls_or_the_plaintext_opt_in() {
        let remote: SocketAddr = "192.0.2.10:5433".parse().unwrap();
        let wildcard: SocketAddr = "0.0.0.0:5433".parse().unwrap();
        let remote_v6: SocketAddr = "[2001:db8::7]:5433".parse().unwrap();
        for address in [remote, wildcard, remote_v6] {
            assert!(!remote_transport_allowed(&address, false, false), "{address}");
            assert!(remote_transport_allowed(&address, true, false), "{address}");
            assert!(remote_transport_allowed(&address, false, true), "{address}");
        }
        for loopback in ["127.0.0.1:5433", "127.0.0.2:5433", "[::1]:5433"] {
            let address: SocketAddr = loopback.parse().unwrap();
            assert!(remote_transport_allowed(&address, false, false), "{address}");
        }
        let plaintext = ReplicationTransport::default();
        assert!(!plaintext.allows(&remote) && plaintext.allows(&"127.0.0.1:1".parse().unwrap()));
        let opted_in = ReplicationTransport {
            tls: None,
            allow_insecure_remote: true,
        };
        assert!(opted_in.allows(&remote));
    }

    #[test]
    fn the_default_server_name_is_the_host_of_the_endpoint() {
        assert_eq!(endpoint_host("db.example:5433"), "db.example");
        assert_eq!(endpoint_host("127.0.0.2:27741"), "127.0.0.2");
        assert_eq!(endpoint_host("[::1]:5433"), "::1");
        assert_eq!(endpoint_host("[2001:db8::7]:5433"), "2001:db8::7");
        // An IP literal is verified as an IP SAN, a name as a DNS name.
        let ip = ReplicationClientTls::new("127.0.0.2", None, None, false, true).unwrap();
        assert!(matches!(ip.server_name(), ServerName::IpAddress(_)));
        let name = ReplicationClientTls::new("db.example", None, None, false, true).unwrap();
        assert!(matches!(name.server_name(), ServerName::DnsName(_)));
        assert!(ReplicationClientTls::new("not a name", None, None, false, true).is_err());
    }

    #[test]
    fn unreadable_files_are_errors_never_a_plaintext_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.pem");
        let server = ReplicationServerTls {
            cert_path: missing.clone(),
            key_path: missing.clone(),
            ca_path: None,
            post_quantum: true,
        };
        assert!(server.acceptor().is_err());
        assert!(!server.mutual());
        let client = ReplicationClientTls::new("db.example", Some(missing), None, false, true).unwrap();
        assert!(client.connector().is_err());
    }

    #[test]
    fn handshake_failures_are_counted() {
        let before = handshake_failures();
        assert!(note_handshake_failure() > before);
        assert!(handshake_failures() > before);
    }
}
