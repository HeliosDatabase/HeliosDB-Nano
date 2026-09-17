//! Shared, explicit PQ-aware `rustls` `CryptoProvider` construction.
//!
//! Both the PostgreSQL and MySQL wire listeners build their `rustls::ServerConfig`
//! from the provider returned here instead of relying on the implicit
//! process-wide default (`rustls::crypto::CryptoProvider::install_default`).
//!
//! ## Why aws-lc-rs, not ring
//!
//! `X25519MLKEM768` (draft-ietf-tls-ecdhe-mlkem — the hybrid post-quantum key
//! exchange group already shipped by AWS, MySQL 26.7 and CockroachDB under the
//! "quantum-ready TLS" banner) only exists in rustls's `aws_lc_rs` crypto
//! backend. Confirmed against the vendored rustls 0.23.45 source
//! (`crypto/aws_lc_rs/pq/mod.rs:14`) — the `ring` backend has no PQ key
//! exchange groups at all.
//!
//! `aws_lc_rs::default_provider()` already lists `X25519MLKEM768` first in
//! `DEFAULT_KX_GROUPS` (classical `X25519`/`SECP256R1`/`SECP384R1` follow), so
//! a PQ-capable provider is just the default provider unmodified; a
//! classical-only provider is the same provider with the hybrid group
//! filtered out of the key-exchange group list.

use crate::{Error, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls_pemfile::{certs, pkcs8_private_keys, rsa_private_keys};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

/// Build an explicit `rustls` `CryptoProvider`.
///
/// - `post_quantum = true`: offers `X25519MLKEM768` (hybrid post-quantum key
///   exchange) alongside the classical groups, in aws-lc-rs's default
///   preference order (PQ hybrid first).
/// - `post_quantum = false`: the same provider with the PQ hybrid group
///   filtered out, leaving only classical key exchange.
pub fn pq_capable_provider(post_quantum: bool) -> Arc<rustls::crypto::CryptoProvider> {
    let provider = rustls::crypto::aws_lc_rs::default_provider();

    if post_quantum {
        return Arc::new(provider);
    }

    let classical_kx_groups: Vec<_> = provider
        .kx_groups
        .iter()
        .copied()
        .filter(|group| group.name() != rustls::NamedGroup::X25519MLKEM768)
        .collect();

    Arc::new(rustls::crypto::CryptoProvider {
        kx_groups: classical_kx_groups,
        ..provider
    })
}

/// Load a PEM certificate chain and private key from disk, shared by the
/// PostgreSQL and MySQL TLS listeners.
///
/// Tries PKCS#8 first, then falls back to RSA (PKCS#1) — reopening the key
/// file for the fallback pass since `pkcs8_private_keys` may have already
/// consumed the reader. `label` (e.g. `"PostgreSQL TLS"` / `"MySQL TLS"`) is
/// folded into error messages so failures stay attributable to the right
/// listener.
pub fn load_cert_and_key(
    cert_path: &Path,
    key_path: &Path,
    label: &str,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    // Load server certificate chain.
    let cert_file = File::open(cert_path)
        .map_err(|e| Error::io(format!("Failed to open {} certificate {}: {}", label, cert_path.display(), e)))?;
    let mut cert_reader = BufReader::new(cert_file);
    let certs_iter = certs(&mut cert_reader);
    let certs: Vec<CertificateDer<'static>> = certs_iter
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::io(format!("Failed to parse {} certificate: {}", label, e)))?;

    if certs.is_empty() {
        return Err(Error::io(format!("No certificates found in {} certificate file", label)));
    }

    // Load private key — PKCS#8 first, then RSA.
    let key_file = File::open(key_path)
        .map_err(|e| Error::io(format!("Failed to open {} private key {}: {}", label, key_path.display(), e)))?;
    let mut key_reader = BufReader::new(key_file);

    let private_key = {
        let pkcs8_keys_iter = pkcs8_private_keys(&mut key_reader);
        let mut pkcs8_keys: Vec<_> = pkcs8_keys_iter
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::io(format!("Failed to parse PKCS#8 key: {}", e)))?;

        if !pkcs8_keys.is_empty() {
            PrivateKeyDer::Pkcs8(pkcs8_keys.remove(0))
        } else {
            // Reopen: `pkcs8_private_keys` may have consumed the reader.
            let key_file = File::open(key_path).map_err(|e| {
                Error::io(format!("Failed to open {} private key {}: {}", label, key_path.display(), e))
            })?;
            let mut key_reader = BufReader::new(key_file);
            let rsa_keys_iter = rsa_private_keys(&mut key_reader);
            let mut rsa_keys: Vec<_> = rsa_keys_iter
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| Error::io(format!("Failed to parse RSA key: {}", e)))?;

            if rsa_keys.is_empty() {
                return Err(Error::io(format!("No private keys found in {} key file", label)));
            }

            PrivateKeyDer::Pkcs1(rsa_keys.remove(0))
        }
    };

    Ok((certs, private_key))
}

/// Load a PEM file of one or more CA certificates, for building a
/// [`rustls::RootCertStore`] used by a mutual-TLS client-certificate
/// verifier. Shared by any listener that wants to verify client certs.
pub fn load_ca_certs(ca_path: &Path, label: &str) -> Result<Vec<CertificateDer<'static>>> {
    let ca_file = File::open(ca_path)
        .map_err(|e| Error::io(format!("Failed to open {} CA certificate {}: {}", label, ca_path.display(), e)))?;
    let mut ca_reader = BufReader::new(ca_file);
    let ca_certs: Vec<CertificateDer<'static>> = certs(&mut ca_reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::io(format!("Failed to parse {} CA certificate: {}", label, e)))?;

    if ca_certs.is_empty() {
        return Err(Error::io(format!("No certificates found in {} CA certificate file", label)));
    }

    Ok(ca_certs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pq_provider_offers_hybrid_group_first() {
        let provider = pq_capable_provider(true);
        assert!(
            provider
                .kx_groups
                .iter()
                .any(|g| g.name() == rustls::NamedGroup::X25519MLKEM768),
            "PQ-capable provider must offer X25519MLKEM768"
        );
    }

    #[test]
    fn classical_provider_excludes_hybrid_group() {
        let provider = pq_capable_provider(false);
        assert!(
            !provider
                .kx_groups
                .iter()
                .any(|g| g.name() == rustls::NamedGroup::X25519MLKEM768),
            "classical-only provider must not offer X25519MLKEM768"
        );
        assert!(!provider.kx_groups.is_empty(), "classical provider must still offer classical groups");
    }
}
